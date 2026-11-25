//! Admin jobs ([`super::super::EzspAdmin`]): forming, leaving, scans,
//! channel change, TX power, tables, ZDO requests, diagnostics; backup and
//! restore in [`super::backup`].

use super::*;

impl<T: Transport, C: Clock> Worker<T, C> {
    pub(super) fn admin(&mut self, job: Job) {
        let Job { op, slot } = job;
        let answer = match op {
            Op::Form(p) => Answer::Form(self.form(p)),
            Op::LeaveReset => Answer::Unit(self.leave_reset()),
            Op::EnergyScan { mask, exp } => Answer::Energy(
                self.scan(ScanType::ENERGY_SCAN, mask, exp)
                    .map(|s| s.energy),
            ),
            Op::ActiveScan { mask, exp } => Answer::Beacons(
                self.scan(ScanType::ACTIVE_SCAN, mask, exp)
                    .map(|s| s.beacons),
            ),
            Op::ChangeChannel(ch) => Answer::Channel(self.change_channel(ch)),
            Op::SetTxPower(dbm) => Answer::TxPower(self.set_tx_power(dbm)),
            Op::Tables { routes } => {
                let neighbors = self.neighbors().map_err(|e| self.admin_error(e));
                let children = self.children().map_err(|e| self.admin_error(e));
                let routes = routes.then(|| self.routes().map_err(|e| self.admin_error(e)));
                Answer::Tables(TablesRead {
                    neighbors,
                    children,
                    routes,
                })
            }
            Op::Zdo {
                nwk,
                cluster,
                zdp,
                t,
            } => {
                let tsn = zdp[0];
                match self.send_zdo_unicast(nwk, cluster, zdp, Owner::Zdo) {
                    Ok(tag) => {
                        let t_ms = t.as_millis() as u64;
                        self.zdo.push(ZdoWait {
                            tag,
                            nwk,
                            cluster,
                            tsn,
                            t_ms,
                            deadline: self.now() + t_ms,
                            delivered: false,
                            slot,
                        });
                        return;
                    }
                    Err(e) => {
                        self.link_error(&e);
                        Answer::Zdo(Err(host_error(&e)))
                    }
                }
            }
            Op::Diagnostics { reset } => Answer::Diagnostics(self.diagnostics(reset)),
            Op::Backup { keys } => Answer::Backup(self.backup_items(keys)),
            Op::RestoreCheck(plan, o) => Answer::Unit(self.restore_check(&plan, o).map(|_| ())),
            Op::Restore(plan, o) => return self.restore_start(plan, o, slot),
        };
        slot.answer(answer);
    }

    /// A host failure of an admin job: the session ends on a link error.
    pub(super) fn admin_error(&mut self, e: HostError) -> CoordError {
        self.link_error(&e);
        host_error(&e)
    }

    pub(super) fn not_up() -> CoordError {
        CoordError::described(
            CoordError::Backend {
                code: Code {
                    backend: hub_coord::BackendKind::Ezsp,
                    value: u16::from(EmberStatus::NETWORK_DOWN.0),
                    text: None,
                },
                kind: Kind::InvalidState,
            },
            "the NCP has no network up",
        )
    }

    /// 16 random bytes: the host's (SipHash keyed by the OS, std's
    /// `RandomState`) XOR the NCP's `getRandomNumber` (its TRNG), so that
    /// either source alone suffices.
    pub(super) fn random16(&mut self) -> Result<[u8; 16], HostError> {
        let mut out = [0u8; 16];
        for (i, chunk) in out.chunks_mut(8).enumerate() {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_usize(i);
            h.write_u64(self.now());
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos()),
            );
            chunk.copy_from_slice(&h.finish().to_le_bytes());
        }
        for pair in out.chunks_mut(2) {
            if let Response::GetRandomNumber { status, value } =
                self.host.call(&Command::GetRandomNumber)?
                && status.is_success()
            {
                pair[0] ^= value as u8;
                pair[1] ^= (value >> 8) as u8;
            }
        }
        Ok(out)
    }

    /// Forms a network (design §2.6 `form`): channel (the quietest of the
    /// mask by an energy scan), PAN ID, EPID and network key from the
    /// parameters or random, Zigbee 3.0 centralized security with hashed
    /// trust center link keys (Silabs `network-creator-security`, Gecko
    /// SDK 4.4.6: global TC link key, preconfigured and network key,
    /// no frame counter reset, encrypted key required, a random root key
    /// hashed per device).
    pub(super) fn form(&mut self, p: FormParams) -> Result<NetworkParams, CoordError> {
        // Only over a definite "no network" (networkInit NOT_JOINED, or
        // after leave_reset): the network is in the NCP's NVM, a form
        // would overwrite one that is merely down or unreadable.
        if self.ownership != NetworkOwnership::NoNetwork {
            let why = if self.net == NetState::Up {
                "the NCP already runs a network"
            } else {
                "the NCP may still store a network (leave_reset first)"
            };
            return Err(CoordError::described(
                CoordError::Backend {
                    code: Code {
                        backend: hub_coord::BackendKind::Ezsp,
                        value: u16::from(EmberStatus::INVALID_CALL.0),
                        text: None,
                    },
                    kind: Kind::InvalidState,
                },
                why,
            ));
        }
        let mask = p.channel_mask & 0x07FF_F800;
        let channel = if mask.count_ones() == 1 {
            mask.trailing_zeros() as u8
        } else {
            let scan = self.scan(ScanType::ENERGY_SCAN, mask, 3)?;
            scan.energy
                .iter()
                .min_by_key(|c| (c.max_rssi_dbm, c.channel))
                .map(|c| c.channel)
                .ok_or_else(|| CoordError::Other("no channel in the mask".into()))?
        };
        let r = self.random16().map_err(|e| self.admin_error(e))?;
        let pan_id = p.pan_id.unwrap_or(match u16::from_le_bytes([r[0], r[1]]) {
            0 => 1,
            x if x >= 0xFFF8 => x & 0x7FFF,
            x => x,
        });
        let ext_pan_id = p.ext_pan_id.unwrap_or_else(|| {
            let mut e = [0u8; 8];
            e.copy_from_slice(&r[2..10]);
            if e == [0; 8] || e == [0xFF; 8] {
                e[0] = 0x5A;
            }
            e
        });
        let network_key = match p.network_key {
            Some(k) => k,
            None => self.random16().map_err(|e| self.admin_error(e))?,
        };
        let root_key = self.random16().map_err(|e| self.admin_error(e))?;
        use security_bitmask as sb;
        let state = InitialSecurityState {
            bitmask: sb::TRUST_CENTER_GLOBAL_LINK_KEY
                | sb::HAVE_PRECONFIGURED_KEY
                | sb::HAVE_NETWORK_KEY
                | sb::NO_FRAME_COUNTER_RESET
                | sb::REQUIRE_ENCRYPTED_KEY
                | sb::TRUST_CENTER_USES_HASHED_LINK_KEY,
            preconfigured_key: root_key,
            network_key,
            network_key_sequence_number: 0,
            preconfigured_trust_center_eui64: Eui64::default(),
        };
        let params = NetworkParameters {
            extended_pan_id: ext_pan_id,
            pan_id,
            radio_tx_power: p.tx_power,
            radio_channel: channel,
            join_method: JoinMethod::USE_MAC_ASSOCIATION,
            nwk_manager_id: 0,
            nwk_update_id: 0,
            channels: 1 << channel,
        };
        let r = (|| -> Result<Status, HostError> {
            let s = match self.host.call(&Command::SetInitialSecurityState(state))? {
                Response::SetInitialSecurityState { status } => status,
                other => return Err(HostError::UnexpectedResponse(other)),
            };
            if !s.is_success() {
                return Err(HostError::Status {
                    frame_id: id::SET_INITIAL_SECURITY_STATE,
                    status: s,
                });
            }
            let ext = extended_security_bitmask::JOINER_GLOBAL_LINK_KEY
                | extended_security_bitmask::NWK_LEAVE_REQUEST_NOT_ALLOWED;
            self.host.call(&Command::SetValue {
                id: ValueId::EXTENDED_SECURITY_BITMASK,
                value: ext.to_le_bytes().to_vec(),
            })?;
            match self.host.call(&Command::FormNetwork(params))? {
                Response::FormNetwork { status } => Ok(status),
                other => Err(HostError::UnexpectedResponse(other)),
            }
        })();
        let status = r.map_err(|e| self.admin_error(e))?;
        if !status.is_success() {
            return Err(ezsp_error(id::FORM_NETWORK, status));
        }
        self.emit(CoordEvent::NetworkState(NetChange::Forming));
        let deadline = self.now() + self.opts.network_up_timeout.as_millis() as u64;
        if !self.wait(deadline, |w| w.net == NetState::Up) {
            return Err(if self.lost {
                CoordError::LinkDown
            } else {
                CoordError::described(CoordError::Timeout, "formNetwork: no NETWORK_UP")
            });
        }
        self.refresh_network();
        self.network.ok_or_else(|| {
            CoordError::described(
                CoordError::DeliveryUnknown,
                "network not readable after forming",
            )
        })
    }

    /// Leaves the network; the NCP clears it from its NVM. The multicast
    /// table (network-scoped) is cleared too.
    pub(super) fn leave_reset(&mut self) -> Result<(), CoordError> {
        self.emit(CoordEvent::NetworkState(NetChange::Leaving));
        let status = match self.host.call(&Command::LeaveNetwork { options: 0 }) {
            Ok(Response::LeaveNetwork { status }) => status,
            Ok(other) => return Err(host_error(&HostError::UnexpectedResponse(other))),
            Err(e) => return Err(self.admin_error(e)),
        };
        if status.is_success() {
            let deadline = self.now() + self.opts.network_up_timeout.as_millis() as u64;
            let left = self.wait(deadline, |w| w.net != NetState::Up);
            if !left {
                return Err(if self.lost {
                    CoordError::LinkDown
                } else {
                    CoordError::described(CoordError::Timeout, "leaveNetwork: no NETWORK_DOWN")
                });
            }
        } else if !matches!(
            status.kind(),
            StatusKind::InvalidCall | StatusKind::NotJoined
        ) {
            // INVALID_CALL / NOT_JOINED: there was no network.
            return Err(ezsp_error(id::LEAVE_NETWORK, status));
        }
        for index in 0..self.groups.len() {
            if self.groups[index].take().is_some() {
                let _ = self.host.call(&Command::SetMulticastTableEntry {
                    index: index as u8,
                    entry: MulticastTableEntry::default(),
                });
            }
        }
        if self.permit_until.take().is_some() {
            self.emit(CoordEvent::PermitJoin { remaining_s: 0 });
        }
        self.net = NetState::None;
        self.network = None;
        self.ownership = NetworkOwnership::NoNetwork;
        self.publish();
        self.emit(CoordEvent::Ownership(Box::new(NetworkOwnership::NoNetwork)));
        Ok(())
    }

    /// An energy or active scan over `mask` (channels 11–26) with
    /// `exp`: 15.36 ms × (2ⁿ + 1) per channel (UG100 §6 startScan).
    pub(super) fn scan(&mut self, kind: ScanType, mask: u32, exp: u8) -> Result<Scan, CoordError> {
        let mask = mask & 0x07FF_F800;
        if mask == 0 || exp > 14 {
            return Err(CoordError::described(
                CoordError::Backend {
                    code: Code {
                        backend: hub_coord::BackendKind::Ezsp,
                        value: u16::from(EmberStatus::BAD_ARGUMENT.0),
                        text: None,
                    },
                    kind: Kind::InvalidParameter,
                },
                "scan: channel mask without 11-26, or duration above 14",
            ));
        }
        let total = mask.count_ones() as u8;
        self.scan = Some(Scan {
            energy: Vec::new(),
            beacons: Vec::new(),
            total,
            done: None,
        });
        let status = match self.host.call(&Command::StartScan {
            scan_type: kind,
            channel_mask: mask,
            duration: exp,
        }) {
            Ok(Response::StartScan { status }) => status,
            Ok(other) => {
                self.scan = None;
                return Err(host_error(&HostError::UnexpectedResponse(other)));
            }
            Err(e) => {
                self.scan = None;
                return Err(self.admin_error(e));
            }
        };
        if !status.is_success() {
            self.scan = None;
            return Err(ezsp_error(id::START_SCAN, status));
        }
        let per_channel = (15.36 * f64::from((1u32 << exp) + 1)) as u64 + 1;
        let deadline = self.now() + per_channel * u64::from(total) + 5000;
        let done = self.wait(deadline, |w| {
            w.scan.as_ref().is_none_or(|s| s.done.is_some())
        });
        let scan = self.scan.take();
        match (done, scan) {
            (true, Some(s)) => match s.done {
                Some(Ok(())) => Ok(s),
                Some(Err(e)) => Err(e),
                None => unreachable!("done"),
            },
            _ if self.lost => Err(CoordError::LinkDown),
            _ => {
                let _ = self.host.call(&Command::StopScan);
                Err(CoordError::described(
                    CoordError::Timeout,
                    "scan did not complete",
                ))
            }
        }
    }

    /// Moves the network to channel `ch` the way the stack's
    /// `emberChannelChangeRequest` does (Gecko SDK 4.4.6
    /// `zigbee-device-stack.h`): a Mgmt_NWK_Update_req with duration 0xFE
    /// to the sleepy broadcast address; the coordinator follows after the
    /// broadcast delivery time and reports CHANNEL_CHANGED.
    pub(super) fn change_channel(&mut self, ch: u8) -> Result<ChannelChange, CoordError> {
        if !(11..=26).contains(&ch) {
            return Err(CoordError::described(
                CoordError::Backend {
                    code: Code {
                        backend: hub_coord::BackendKind::Ezsp,
                        value: u16::from(EmberStatus::BAD_ARGUMENT.0),
                        text: None,
                    },
                    kind: Kind::InvalidParameter,
                },
                format!("channel {ch} outside 11-26"),
            ));
        }
        if self.net != NetState::Up {
            return Err(Self::not_up());
        }
        if let Some(n) = self.network
            && n.channel == ch
        {
            return Ok(ChannelChange {
                channel: ch,
                nwk_update_id: n.nwk_update_id,
            });
        }
        self.channel_changed = false;
        let r = self.host.call(&Command::EnergyScanRequest {
            target: 0xFFFF,
            channels: 1 << ch,
            duration: 0xFE,
            count: 0,
        });
        match r {
            Ok(Response::EnergyScanRequest { status }) if status.is_success() => {}
            Ok(Response::EnergyScanRequest { status }) => {
                return Err(ezsp_error(id::ENERGY_SCAN_REQUEST, status));
            }
            Ok(other) => return Err(host_error(&HostError::UnexpectedResponse(other))),
            Err(e) => return Err(self.admin_error(e)),
        }
        // nwkBroadcastDeliveryTime (9 s) plus margin.
        let deadline = self.now() + 15_000;
        if !self.wait(deadline, |w| w.channel_changed) {
            return Err(if self.lost {
                CoordError::LinkDown
            } else {
                CoordError::described(CoordError::Timeout, "no CHANNEL_CHANGED")
            });
        }
        self.refresh_network();
        let n = self.network.ok_or(CoordError::DeliveryUnknown)?;
        Ok(ChannelChange {
            channel: n.channel,
            nwk_update_id: n.nwk_update_id,
        })
    }

    /// Sets the radio power; it is applied again in every session.
    pub(super) fn set_tx_power(&mut self, dbm: i8) -> Result<i8, CoordError> {
        match self.host.call(&Command::SetRadioPower { power: dbm }) {
            Ok(Response::SetRadioPower { status }) if status.is_success() => {
                self.tx_power = Some(dbm);
                *self.shared.tx_power.lock().unwrap() = Some(dbm);
                if let Some(n) = &mut self.network {
                    n.tx_power = Some(dbm);
                }
                self.publish();
                Ok(dbm)
            }
            Ok(Response::SetRadioPower { status }) => Err(ezsp_error(id::SET_RADIO_POWER, status)),
            Ok(other) => Err(host_error(&HostError::UnexpectedResponse(other))),
            Err(e) => Err(self.admin_error(e)),
        }
    }

    /// EmberZNet counters (`readCounters`, or `readAndClearCounters` with
    /// `reset`), the NWK frame counter, and the host's ASH and EZSP
    /// counters.
    pub(super) fn diagnostics(&mut self, reset: bool) -> Diagnostics {
        let mut d = Diagnostics {
            tx_power_set: self.tx_power,
            ..Diagnostics::default()
        };
        let cmd = if reset {
            Command::ReadAndClearCounters
        } else {
            Command::ReadCounters
        };
        match self.host.call(&cmd) {
            Ok(Response::ReadCounters { values } | Response::ReadAndClearCounters { values }) => {
                d.counters = values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let (name, group) = COUNTERS
                            .get(i)
                            .map_or((None, "other"), |(n, g)| (Some(*n), *g));
                        DiagCounter {
                            id: i as u16,
                            group,
                            name,
                            value: u32::from(*v),
                        }
                    })
                    .collect();
            }
            Ok(other) => d.errors.push(format!("readCounters: {other:?}")),
            Err(e) => {
                d.errors.push(format!("readCounters: {e}"));
                self.link_error(&e);
            }
        }
        if let Ok(Response::GetNetworkKeyInfo { status, info }) =
            self.host.call(&Command::GetNetworkKeyInfo)
            && status.is_success()
        {
            d.counters.push(DiagCounter {
                id: 0x0300,
                group: "security",
                name: Some("nwk_frame_counter"),
                value: info.network_key_frame_counter,
            });
        }
        let used = self.slots_used();
        d.status = Some(NcpStatus {
            network_state: match self.net {
                NetState::None => 0,
                NetState::FormedDown => 1,
                NetState::Up => 2,
            },
            uptime_s: 0,
            free_aps_slots: self.max_in_flight().saturating_sub(used) as u16,
            host_absent_queue: 0,
            dirty_writebacks: 0,
            restore_in_progress: false,
            key_export_locked: self.key_exportable == Some(false),
            paging_active: false,
            pta_coex: false,
            gp_commissioning: false,
            permit_join: self.permit_until.is_some(),
        });
        let c = self.host.counters();
        let (ash, host) = (&c.ash, &c.host);
        let t = self.start.unwrap_or_default();
        let link = [
            (0x0101, "tx_data", ash.tx_data),
            (0x0102, "tx_retransmissions", ash.tx_retransmit),
            (0x0103, "tx_ack", ash.tx_ack),
            (0x0104, "tx_nak", ash.tx_nak),
            (0x0105, "rx_data", ash.rx_data),
            (0x0106, "rx_nak", ash.rx_nak),
            (0x0107, "rx_bad_crc", ash.rx_bad_crc),
            (0x0108, "rx_duplicate", ash.rx_duplicate),
            (0x0109, "rx_out_of_sequence", ash.rx_out_of_sequence),
            (0x010A, "ack_timeouts", ash.ack_timeouts),
            (0x010B, "resets", ash.resets),
            (0x010C, "rx_xoff", ash.rx_xoff),
            (0x010D, "xoff_timeouts", ash.xoff_timeouts),
            (0x010E, "rx_ack", ash.rx_ack),
            (
                0x010F,
                "rx_bad_frames",
                ash.rx_bad_control
                    + ash.rx_bad_length
                    + ash.rx_bad_ack_num
                    + ash.rx_too_short
                    + ash.rx_too_long,
            ),
            (
                0x0110,
                "rx_discarded",
                ash.rx_discarded + ash.rx_cancelled + ash.rx_substituted,
            ),
            (0x0111, "rx_stale_rstack", ash.rx_stale_rstack),
            (0x0201, "commands", host.commands),
            (0x0202, "callbacks", host.callbacks),
            (0x0203, "dropped_callbacks", host.dropped_callbacks),
            (0x0204, "ncp_overflows", self.stats.ncp_overflows),
            (0x0205, "sessions", self.stats.sessions),
            (0x0206, "devices_left", self.stats.devices_left),
            (0x0207, "route_records", self.stats.route_records),
            (0x0208, "gp_frames_ignored", self.stats.gp_frames),
            (0x0209, "failed_starts", self.failed_starts),
            (0x020A, "bad_frames", host.bad_frames),
            (0x020B, "unexpected_responses", host.unexpected_responses),
            (0x020C, "callbacks_trailing", host.callbacks_trailing),
            (0x0301, "uart_overruns", c.line.overruns),
            (0x0302, "uart_line_errors", c.line.errors),
            (0x0401, "start_ms", t.total_ms),
            (0x0402, "start_reset_ms", t.reset_ms),
            (0x0403, "start_version_ms", t.version_ms),
            (0x0404, "start_config_ms", t.config_ms),
            (0x0405, "start_setup_ms", t.setup_ms),
            (0x0406, "start_network_ms", t.network_ms),
            (0x0407, "start_rest_ms", t.rest_ms),
        ];
        d.host_link = link
            .into_iter()
            .map(|(id, name, value)| DiagCounter {
                id,
                group: match id >> 8 {
                    1 => "link",
                    2 => "host",
                    3 => "uart",
                    _ => "start",
                },
                name: Some(name),
                value,
            })
            .collect();
        d
    }
}
