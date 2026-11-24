//! Requests: the queue in order, APS sends and their outcomes, control
//! requests (barriers), deadlines, and the ZDO frames the worker sends.

use super::*;

impl<T: Transport, C: Clock> Worker<T, C> {
    /// `false` when request `id` was cancelled.
    pub(super) fn take(&self, id: RequestId) -> bool {
        let mut c = self.shared.cancels.lock().unwrap();
        c.taken = id.0;
        let before = c.ids.len();
        c.ids.retain(|x| *x != id.0);
        c.ids.len() == before
    }

    /// Acknowledged unicasts on the NCP (its APS unicast slots).
    pub(super) fn slots_used(&self) -> usize {
        self.inflight
            .iter()
            .filter(|f| f.acked && !matches!(f.owner, Owner::Expired(_)))
            .count()
    }

    pub(super) fn max_in_flight(&self) -> usize {
        self.caps
            .as_ref()
            .map_or(1, |c| usize::from(c.max_in_flight))
    }

    /// Runs the next queued request, if it may go now.
    pub(super) fn step_queue(&mut self) -> bool {
        let Some((_, cmd)) = self.fifo.front() else {
            return false;
        };
        if let Cmd::Aps { req, .. } = &**cmd
            && needs_slot(req)
            && self.slots_used() >= self.max_in_flight()
        {
            return false;
        }
        let (id, cmd) = self.fifo.pop_front().expect("front exists");
        self.shared.queued.fetch_sub(1, Ordering::Relaxed);
        if !self.take(id) {
            self.done(id, Terminal::Cancelled);
            return true;
        }
        // Callbacks that came before go first.
        self.dispatch(true);
        match *cmd {
            Cmd::Aps { req, timeout } => self.send_aps(id, req, timeout),
            cmd => {
                let t = self.control(cmd);
                self.dispatch(true);
                if self.lost {
                    // Ended with the session, after `Down`.
                    self.park(id);
                } else {
                    self.done(id, t);
                }
            }
        }
        true
    }

    /// A request whose outcome the end of the session decides.
    pub(super) fn park(&mut self, id: RequestId) {
        self.inflight.push(Inflight {
            tag: 0,
            owner: Owner::Request(id),
            deadline: u64::MAX,
            unicast: false,
            acked: false,
        });
    }

    /// A free message tag (8 bits in EZSP 13, 16 bits from 14); 0 is never
    /// used.
    pub(super) fn alloc_tag(&mut self) -> u16 {
        let wide = self.version.is_some_and(|v| v.wide_message_tag());
        let max = if wide { u16::MAX } else { 0xFF };
        loop {
            self.next_tag = if self.next_tag >= max {
                1
            } else {
                self.next_tag + 1
            };
            let tag = self.next_tag;
            if !self.inflight.iter().any(|f| f.tag == tag) {
                return tag;
            }
            if self.inflight.len() >= usize::from(max) {
                // Every tag taken: the oldest expired one is given up.
                if let Some(i) = self
                    .inflight
                    .iter()
                    .position(|f| matches!(f.owner, Owner::Expired(_)))
                {
                    let tag = self.inflight.remove(i).tag;
                    return tag;
                }
            }
        }
    }

    /// The short address of `ieee`: learned from joins and announcements,
    /// else asked from the NCP's address tables.
    pub(super) fn resolve(&mut self, ieee: u64) -> Result<Option<u16>, HostError> {
        if let Some((_, nwk)) = self.nwk_cache.iter().find(|(i, _)| *i == ieee) {
            return Ok(Some(*nwk));
        }
        let r = self.host.call(&Command::LookupNodeIdByEui64 {
            eui64: Eui64::from_u64(ieee),
        })?;
        Ok(match r {
            Response::LookupNodeIdByEui64 { status, node_id }
                if node_id < 0xFFF8 && status.is_none_or(|s| s.is_success()) =>
            {
                self.learn(ieee, node_id);
                Some(node_id)
            }
            _ => None,
        })
    }

    pub(super) fn send_aps(&mut self, id: RequestId, req: ApsRequest, timeout: Duration) {
        let v = self.version.expect("a session");
        let is_unicast = |dst: u16| dst < 0xFFF8;
        let options = |ack: bool| {
            let base = ApsOptions::ENABLE_ROUTE_DISCOVERY;
            if ack { base | ApsOptions::RETRY } else { base }
        };
        let mut aps = ApsFrame {
            profile_id: req.profile,
            cluster_id: req.cluster,
            source_endpoint: req.src_ep,
            destination_endpoint: req.dst_ep,
            options: ApsOptions::NONE,
            group_id: 0,
            sequence: 0,
        };
        let resolved = match req.dst {
            ApsDst::Ieee(ieee) => {
                let r = self.resolve(ieee);
                // Callbacks that came before the lookup's response go
                // first, whatever its outcome.
                self.dispatch(true);
                Some((ieee, r))
            }
            _ => None,
        };
        let dst = match (req.dst, resolved) {
            (ApsDst::Nwk(n), _) => Some(n),
            (ApsDst::Group(_), _) => None,
            (ApsDst::Ieee(_), None) => unreachable!("resolved above"),
            (ApsDst::Ieee(_), Some((ieee, r))) => match r {
                Ok(Some(n)) => Some(n),
                Ok(None) => {
                    let e = CoordError::described(
                        CoordError::Backend {
                            code: Code {
                                backend: hub_coord::BackendKind::Ezsp,
                                value: u16::from(EmberStatus::NOT_FOUND.0),
                                text: None,
                            },
                            kind: Kind::NoRoute,
                        },
                        format!("no short address for {ieee:016X}"),
                    );
                    self.done(id, Terminal::Refused(e));
                    return;
                }
                Err(e) => {
                    self.link_error(&e);
                    self.send_failed(id, 0, &e);
                    return;
                }
            },
        };
        let tag = self.alloc_tag();
        let (cmd, unicast, acked) = match (req.dst, dst) {
            (ApsDst::Group(g), _) => {
                aps.group_id = g;
                aps.destination_endpoint = 0xFF;
                let cmd = Command::SendMulticast {
                    aps,
                    hops: 0,
                    // Infinite non-member radius (EZSP 13).
                    extra: MulticastExtra::none(v, 7),
                    tag,
                    message: req.payload,
                };
                (cmd, false, false)
            }
            (_, Some(n)) if is_unicast(n) => {
                let ack = req.options.ack;
                aps.options = options(ack);
                let cmd = Command::SendUnicast {
                    kind: OutgoingMessageType::DIRECT,
                    index_or_destination: n,
                    aps,
                    tag,
                    message: req.payload,
                };
                (cmd, true, ack)
            }
            (_, Some(n)) => {
                let cmd = Command::SendBroadcast {
                    destination: n,
                    extra: BroadcastExtra::none(v),
                    aps,
                    radius: 0,
                    tag,
                    message: req.payload,
                };
                (cmd, false, false)
            }
            (_, None) => unreachable!("only groups have no short address"),
        };
        let frame_id = cmd.frame_id();
        let r = self.host.call(&cmd);
        // Callbacks that came before the response go first.
        self.dispatch(true);
        match r {
            Ok(resp) => match resp.status() {
                Some(s) if s.is_success() => {
                    let deadline = self
                        .now()
                        .saturating_add(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX));
                    self.inflight.push(Inflight {
                        tag,
                        owner: Owner::Request(id),
                        deadline,
                        unicast,
                        acked,
                    });
                    self.emit(CoordEvent::Progress {
                        id,
                        p: Progress::Accepted { zcl_tsn: None },
                    });
                }
                Some(s) => self.done(id, Terminal::Refused(ezsp_error(frame_id, s))),
                None => self.done(
                    id,
                    Terminal::Unknown(CoordError::Other(format!("unexpected {resp:?}").into())),
                ),
            },
            Err(e) => self.send_failed(id, tag, &e),
        }
    }

    /// A send without a usable response.
    pub(super) fn send_failed(&mut self, id: RequestId, tag: u16, e: &HostError) {
        self.link_error(e);
        if self.lost {
            self.park(id);
            return;
        }
        match e {
            HostError::InvalidCommand { .. } | HostError::Encode(_) => {
                self.done(id, Terminal::Refused(host_error(e)))
            }
            HostError::Timeout { .. } => {
                // The NCP may still send it: the tag waits for a late
                // messageSentHandler.
                if tag != 0 {
                    let now = self.now();
                    self.inflight.push(Inflight {
                        tag,
                        owner: Owner::Expired(now),
                        deadline: u64::MAX,
                        unicast: false,
                        acked: false,
                    });
                }
                let e = CoordError::described(CoordError::DeliveryUnknown, e.to_string());
                self.done(id, Terminal::Unknown(e));
            }
            e => self.done(id, Terminal::Unknown(host_error(e))),
        }
    }

    /// Deadlines: requests without `messageSentHandler`, ZDO waits, the
    /// permit-join window, tags kept for late callbacks.
    pub(super) fn expire(&mut self) {
        let now = self.now();
        let mut timed_out = Vec::new();
        for f in &mut self.inflight {
            if let Owner::Request(id) = f.owner
                && f.tag != 0
                && now >= f.deadline
            {
                timed_out.push(id);
                f.owner = Owner::Expired(now);
            }
        }
        self.inflight.retain(|f| match f.owner {
            Owner::Expired(t) => now < t + EXPIRED_GRACE_MS,
            // A broadcast whose messageSentHandler was lost.
            Owner::Internal => now < f.deadline,
            _ => true,
        });
        for id in timed_out {
            self.done(id, Terminal::Unknown(CoordError::Timeout));
        }
        let mut i = 0;
        while i < self.zdo.len() {
            if now >= self.zdo[i].deadline {
                let w = self.zdo.remove(i);
                let e = if w.delivered {
                    CoordError::Timeout
                } else {
                    CoordError::described(
                        CoordError::DeliveryUnknown,
                        "no messageSentHandler in time",
                    )
                };
                if let Some(f) = self.inflight.iter_mut().find(|f| f.tag == w.tag) {
                    f.owner = Owner::Expired(now);
                }
                w.slot.answer(Answer::Zdo(Err(e)));
            } else {
                i += 1;
            }
        }
        if let Some(until) = self.permit_until {
            if now >= until {
                self.permit_until = None;
                if let Err(e) = self.close_joining() {
                    warn!("ezsp: closing the network: {e}");
                    self.link_error(&e);
                }
                self.emit(CoordEvent::PermitJoin { remaining_s: 0 });
                self.publish();
            } else {
                let s = until.saturating_sub(now).div_ceil(1000) as u32;
                if s != self.permit_published {
                    self.permit_published = s;
                    self.publish();
                }
            }
        }
    }

    /// A control request, run to completion (a barrier).
    pub(super) fn control(&mut self, cmd: Cmd) -> Terminal {
        let r = match cmd {
            Cmd::Group {
                endpoint,
                group,
                add,
            } => self.group(endpoint, group, add),
            Cmd::Remove { ieee, nwk } => {
                // The engine sends Mgmt_Leave_req itself. With hashed link
                // keys the trust center keeps no key for the device, and
                // the stack drops a child that leaves; the address is
                // forgotten here.
                self.nwk_cache.retain(|(i, _)| *i != ieee);
                debug!(
                    "ezsp: {ieee:016X} (0x{:04X}) removed",
                    nwk.unwrap_or(0xFFFF)
                );
                Ok(Terminal::Done(Reply::None))
            }
            Cmd::PermitJoin { secs, target } => self.permit_join(secs, target),
            Cmd::Inventory => self
                .inventory()
                .map(|inv| Terminal::Done(Reply::Inventory(Box::new(inv)))),
            Cmd::Aps { .. } => unreachable!("APS requests are sent by send_aps"),
        };
        match r {
            Ok(t) => t,
            Err(e) => {
                self.link_error(&e);
                match e {
                    HostError::Status { frame_id, status } => {
                        Terminal::Refused(ezsp_error(frame_id, status))
                    }
                    e if is_link_error(&e) => Terminal::Unknown(CoordError::described(
                        CoordError::DeliveryUnknown,
                        e.to_string(),
                    )),
                    e => Terminal::Unknown(host_error(&e)),
                }
            }
        }
    }

    pub(super) fn group(
        &mut self,
        endpoint: u8,
        group: u16,
        add: bool,
    ) -> Result<Terminal, HostError> {
        let at = self
            .groups
            .iter()
            .position(|e| e.is_some_and(|e| e.multicast_id == group && e.endpoint == endpoint));
        let (index, entry) = match (add, at) {
            (true, Some(_)) | (false, None) => return Ok(Terminal::Done(Reply::None)),
            (true, None) => {
                let Some(free) = self.groups.iter().position(Option::is_none) else {
                    let e = CoordError::described(
                        CoordError::Backend {
                            code: Code {
                                backend: hub_coord::BackendKind::Ezsp,
                                value: u16::from(EmberStatus::TABLE_FULL.0),
                                text: None,
                            },
                            kind: Kind::TableFull,
                        },
                        "multicast table full",
                    );
                    return Ok(Terminal::Refused(e));
                };
                let entry = MulticastTableEntry {
                    multicast_id: group,
                    endpoint,
                };
                (free, entry)
            }
            // An entry with endpoint 0 is unused.
            (false, Some(i)) => (i, MulticastTableEntry::default()),
        };
        let r = self.host.call(&Command::SetMulticastTableEntry {
            index: index as u8,
            entry,
        })?;
        match r.status() {
            Some(s) if s.is_success() => {
                self.groups[index] = add.then_some(entry);
                Ok(Terminal::Done(Reply::None))
            }
            Some(s) => Err(HostError::Status {
                frame_id: id::SET_MULTICAST_TABLE_ENTRY,
                status: s,
            }),
            None => Err(HostError::UnexpectedResponse(r)),
        }
    }

    pub(super) fn next_zdo_tsn(&mut self) -> u8 {
        self.zdo_tsn = self.zdo_tsn.wrapping_add(1);
        self.zdo_tsn
    }

    /// Opens joining (design §5): the well-known key as a transient key
    /// for every joining device and joins in the trust center policy (as
    /// Silabs' `network-creator-security` opens a network), then the
    /// coordinator's own `permitJoining` and a Mgmt_Permit_Joining_req
    /// broadcast to the routers, or the request to one router. 0 closes.
    pub(super) fn permit_join(
        &mut self,
        secs: u8,
        target: Option<u16>,
    ) -> Result<Terminal, HostError> {
        let secs = secs.min(MAX_PERMIT_S);
        // This request decides the window: the previous one does not close
        // it while the request waits for the NCP.
        let was_open = self.permit_until.take().is_some();
        let r = self.open_joining(secs, target);
        let ok = matches!(r, Ok(Terminal::Done(_)));
        if secs == 0 || !ok {
            // Closed, or not opened everywhere: nothing stays open that
            // the hub does not report (best effort when the link is gone).
            if !ok && secs > 0 && matches!(target, None | Some(0x0000)) {
                let _ = self.host.permit_joining(0);
            }
            if let Err(e) = self.close_joining() {
                self.link_error(&e);
            }
        } else {
            self.permit_until = Some(self.now() + u64::from(secs) * 1000);
        }
        let remaining = if ok { u32::from(secs) } else { 0 };
        self.permit_published = remaining;
        self.publish();
        if ok || was_open {
            self.emit(CoordEvent::PermitJoin {
                remaining_s: remaining,
            });
        }
        r
    }

    /// The NCP's part of [`Self::permit_join`]; `Done` when every step
    /// succeeded.
    fn open_joining(&mut self, secs: u8, target: Option<u16>) -> Result<Terminal, HostError> {
        if secs > 0 {
            match self.host.call(&Command::ImportTransientKey {
                eui64: ANY_EUI64,
                key: ZIGBEE_ALLIANCE_09,
                flags: 0,
            })? {
                Response::ImportTransientKey { status } if status.is_success() => {}
                Response::ImportTransientKey { status } => {
                    return Err(HostError::Status {
                        frame_id: id::IMPORT_TRANSIENT_KEY,
                        status,
                    });
                }
                other => return Err(HostError::UnexpectedResponse(other)),
            }
            self.set_policy(
                PolicyId::TRUST_CENTER_POLICY,
                tc_policy::ALLOW_JOINS | tc_policy::ALLOW_UNSECURED_REJOINS,
            )?;
        }
        let tsn = self.next_zdo_tsn();
        let zdp = vec![tsn, secs, 1];
        match target {
            None | Some(0x0000) => {
                self.host.permit_joining(secs)?;
                if target.is_none() {
                    self.send_zdo_broadcast(0xFFFC, ZDO_MGMT_PERMIT_JOINING_REQ, zdp)?;
                }
            }
            Some(router) => {
                if let Err(e) = self.send_zdo_and_wait(router, ZDO_MGMT_PERMIT_JOINING_REQ, zdp)? {
                    return Ok(Terminal::NotDelivered(e));
                }
            }
        }
        Ok(Terminal::Done(Reply::None))
    }

    /// Back to the closed trust center policy; the transient keys go.
    pub(super) fn close_joining(&mut self) -> Result<(), HostError> {
        let closed = self
            .opts
            .policies
            .iter()
            .find(|(p, _)| *p == PolicyId::TRUST_CENTER_POLICY)
            .map_or(tc_policy::ALLOW_UNSECURED_REJOINS, |(_, d)| *d);
        self.set_policy(PolicyId::TRUST_CENTER_POLICY, closed)?;
        self.host.call(&Command::ClearTransientLinkKeys)?;
        Ok(())
    }

    pub(super) fn zdo_aps(cluster: u16, options: ApsOptions) -> ApsFrame {
        ApsFrame {
            profile_id: 0,
            cluster_id: cluster,
            source_endpoint: 0,
            destination_endpoint: 0,
            options,
            group_id: 0,
            sequence: 0,
        }
    }

    pub(super) fn send_zdo_broadcast(
        &mut self,
        dst: u16,
        cluster: u16,
        zdp: Vec<u8>,
    ) -> Result<(), HostError> {
        let v = self.version.expect("a session");
        let tag = self.alloc_tag();
        let r = self.host.call(&Command::SendBroadcast {
            destination: dst,
            extra: BroadcastExtra::none(v),
            aps: Self::zdo_aps(cluster, ApsOptions::NONE),
            radius: 0,
            tag,
            message: zdp,
        })?;
        match r.status() {
            Some(s) if s.is_success() => {
                let deadline = self.now() + self.opts.confirm_timeout.as_millis() as u64;
                self.inflight.push(Inflight {
                    tag,
                    owner: Owner::Internal,
                    deadline,
                    unicast: false,
                    acked: false,
                });
                Ok(())
            }
            Some(s) => Err(HostError::Status {
                frame_id: id::SEND_BROADCAST,
                status: s,
            }),
            None => Err(HostError::UnexpectedResponse(r)),
        }
    }

    /// An acknowledged ZDO unicast of the worker: sent, then its
    /// `messageSentHandler` awaited (`Ok(Err)`: not delivered).
    pub(super) fn send_zdo_and_wait(
        &mut self,
        dst: u16,
        cluster: u16,
        zdp: Vec<u8>,
    ) -> Result<Result<(), CoordError>, HostError> {
        let tag = self.send_zdo_unicast(dst, cluster, zdp, Owner::Wait(None))?;
        let deadline = self.now() + self.opts.confirm_timeout.as_millis() as u64;
        let sent = |w: &Self| {
            w.inflight
                .iter()
                .any(|f| f.tag == tag && matches!(f.owner, Owner::Wait(Some(_))))
        };
        self.wait(deadline, sent);
        let i = self.inflight.iter().position(|f| f.tag == tag);
        match i.map(|i| self.inflight.remove(i).owner) {
            Some(Owner::Wait(Some(r))) => Ok(r),
            _ if self.lost => Ok(Err(CoordError::described(
                CoordError::DeliveryUnknown,
                "NCP reset or link lost",
            ))),
            _ => Ok(Err(CoordError::Timeout)),
        }
    }

    pub(super) fn send_zdo_unicast(
        &mut self,
        dst: u16,
        cluster: u16,
        zdp: Vec<u8>,
        owner: Owner,
    ) -> Result<u16, HostError> {
        let tag = self.alloc_tag();
        let r = self.host.call(&Command::SendUnicast {
            kind: OutgoingMessageType::DIRECT,
            index_or_destination: dst,
            aps: Self::zdo_aps(
                cluster,
                ApsOptions::RETRY | ApsOptions::ENABLE_ROUTE_DISCOVERY,
            ),
            tag,
            message: zdp,
        })?;
        match r.status() {
            Some(s) if s.is_success() => {
                self.inflight.push(Inflight {
                    tag,
                    owner,
                    deadline: u64::MAX,
                    unicast: true,
                    acked: true,
                });
                Ok(tag)
            }
            Some(s) => Err(HostError::Status {
                frame_id: id::SEND_UNICAST,
                status: s,
            }),
            None => Err(HostError::UnexpectedResponse(r)),
        }
    }

    /// The coordinator's children and router neighbors.
    pub(super) fn inventory(&mut self) -> Result<Inventory, HostError> {
        let mut inv = Inventory::default();
        for c in self.children()? {
            inv.devices.push(InventoryDevice {
                ieee: u64::from_le_bytes(c.ieee),
                nwk: Some(c.nwk),
                capability: Some(c.capability),
                device_type: Some(2),
            });
        }
        for n in self.neighbors()? {
            let ieee = u64::from_le_bytes(n.ieee);
            if !inv.devices.iter().any(|d| d.ieee == ieee) {
                inv.devices.push(InventoryDevice {
                    ieee,
                    nwk: Some(n.nwk),
                    capability: None,
                    device_type: Some(1),
                });
            }
        }
        Ok(inv)
    }

    pub(super) fn children(&mut self) -> Result<Vec<Child>, HostError> {
        let max = self
            .cfg(ConfigId::MAX_END_DEVICE_CHILDREN)
            .unwrap_or(32)
            .min(64) as u8;
        let mut list = Vec::new();
        for index in 0..max {
            if let Response::GetChildData { status, data } =
                self.host.call(&Command::GetChildData { index })?
                && status.is_success()
            {
                list.push(Child {
                    ieee: data.eui64.0,
                    nwk: data.id,
                    capability: child_capability(data.node_type),
                    timeout_exp: data.timeout,
                    // EmberChildData has no time since the last poll.
                    last_poll_ms: 0,
                });
            }
        }
        Ok(list)
    }

    pub(super) fn neighbors(&mut self) -> Result<Vec<Neighbor>, HostError> {
        let count = match self.host.call(&Command::NeighborCount)? {
            Response::NeighborCount { count } => count,
            other => return Err(HostError::UnexpectedResponse(other)),
        };
        let mut list = Vec::new();
        for index in 0..count {
            if let Response::GetNeighbor { status, entry } =
                self.host.call(&Command::GetNeighbor { index })?
                && status.is_success()
            {
                list.push(Neighbor {
                    ieee: entry.long_id.0,
                    nwk: entry.short_id,
                    // The neighbor table holds routers only.
                    device_type: 1,
                    rx_on: true,
                    relationship: 2,
                    lqi: entry.average_lqi,
                    outgoing_cost: entry.out_cost,
                    age: entry.age,
                });
            }
        }
        Ok(list)
    }

    pub(super) fn routes(&mut self) -> Result<Vec<Route>, HostError> {
        // The table size the NCP reports (UG100 lists 0 as the default),
        // else up to the first index it refuses.
        let size = match self.cfg(ConfigId::ROUTE_TABLE_SIZE) {
            Some(n) if n > 0 => n.min(255) as u8,
            _ => 255,
        };
        let mut list = Vec::new();
        for index in 0..size {
            match self.host.call(&Command::GetRouteTableEntry { index })? {
                Response::GetRouteTableEntry { status, entry } if status.is_success() => {
                    // Status 3: unused entry.
                    if entry.status != 3 && entry.destination < 0xFFF8 {
                        list.push(Route {
                            dst: entry.destination,
                            next_hop: entry.next_hop,
                            status: entry.status,
                            flags: 0,
                        });
                    }
                }
                _ => break,
            }
        }
        Ok(list)
    }
}

/// An APS request that takes one of the NCP's APS unicast slots: an
/// acknowledged unicast.
fn needs_slot(req: &ApsRequest) -> bool {
    req.options.ack
        && !matches!(req.dst, ApsDst::Nwk(n) if n >= 0xFFF8)
        && !matches!(req.dst, ApsDst::Group(_))
}
