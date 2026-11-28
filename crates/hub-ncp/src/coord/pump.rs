//! [`ZrhpCoordinator`]: the host core behind [`Coordinator`]
//! (`docs/coordinator-boundary.md` §3, §4).
//!
//! [`Coordinator::submit`] checks the session, encodes the request and
//! queues it for the `zrhp-pump` thread; it never waits. The pump takes,
//! from one queue and in order, the application's commands and the NCP's
//! events (a [`Hub::subscribe_into`] subscriber):
//!
//! - **APS data** goes out with [`Client::start`](crate::Client::start)
//!   and the same call options as before (the driver holds requests across
//!   an NCP resume), at most `max_aps_inflight` (the NCP's capacity, of
//!   the last session that reported it) not yet answered at once: the NCP
//!   frees its slot when it hands a request to its stack, not at the
//!   confirm. The others wait in the pump, in order; one the NCP refuses
//!   with `BUSY` waits there and goes again ([`BUSY_RETRY`]). Its outcome
//!   is emitted at the position of its `EVT_APS_DATA_CONFIRM` among the
//!   events (the driver completes the request before it passes the event
//!   on), so it never precedes an indication the NCP sent before it.
//!   Outcomes a response alone decides (refusals, driver errors, timeouts)
//!   are emitted before the next event.
//! - **Control requests** (GP, group membership, removal, permit join, the
//!   device inventory) are today's blocking [`Hub`] calls, made inline (an
//!   APS request waiting for a slot does not hold them): later commands
//!   and events wait in the queue until the call returns.
//!   That makes them barriers, as when the application thread made them.
//! - **Hub endpoints** ([`BackendConfig`]): added once per process unless
//!   the store has them (the lifecycle replays them on every resume), then
//!   [`SessionEvent::Up`] follows; a failure is retried a second later.
//!
//! Events go to the application's bounded [`EventQueue`]; the pump waits
//! while it is full.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hub_coord::{
    BackendConfig, CoordError, CoordEvent, CoordStatus, Coordinator, EndpointConfig, EventQueue,
    Inventory, InventoryDevice, Progress, RadioControl, Reply, Request, RequestId, SessionEvent,
    Terminal,
};
use log::{debug, info, warn};
use zigbee_zrhp::msg::ids;
use zigbee_zrhp::msg::system::capacity;
use zigbee_zrhp::types::{Ieee, Status};

use super::convert;
use super::error::{
    control_outcome, coord_error, engine_error, legacy_zb_status, zrhp_not_delivered,
};
use super::status::{self, Facts};
use crate::Hub;
use crate::driver::{CallOpts, Confirm, Error, Message, Outcome, SessionState, Ticket};
use crate::store::EndpointRecord;

/// Stack of the `zrhp-pump` thread. It makes the control calls the
/// application thread made before (store commits, decoding, logging).
pub const PUMP_STACK: usize = 8 * 1024;

/// Requests queued for the pump at most (those waiting for an APS slot
/// included); more are refused with [`CoordError::Busy`] (design §3.5).
pub const COMMAND_CAP: usize = 2048;

/// How long an accepted APS request waits for its confirm. The NCP holds a
/// sleepy child's frame until its next expected poll; past this the
/// outcome is unknown (the engine's response timeout decides).
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);

/// The pump wakes at least this often to collect timed-out requests.
const POLL: Duration = Duration::from_millis(100);

/// A completed request waits this long for its `EVT_APS_DATA_CONFIRM` to
/// come through the lifecycle before its outcome is emitted anyway (once
/// the pump has read every message the lifecycle passed on).
const CONFIRM_SLACK: Duration = Duration::from_secs(2);

/// Delay before an endpoint registration that failed is tried again.
const ENDPOINT_RETRY: Duration = Duration::from_secs(1);

/// An APS request the NCP refused with `BUSY` (its queue full) goes again
/// after this, at most [`BUSY_TRIES`] times; then its outcome is the
/// refusal.
const BUSY_RETRY: Duration = Duration::from_millis(100);
const BUSY_TRIES: u8 = 20;

enum Cmd {
    Aps {
        payload: Vec<u8>,
        timeout: Duration,
        /// Refused with `BUSY` this many times.
        busy: u8,
    },
    Gp {
        msg_id: u16,
        payload: Vec<u8>,
        remote: bool,
        opts: CallOpts,
    },
    Group {
        op: u8,
        group: u16,
        endpoint: u8,
    },
    Remove {
        nwk: u16,
        ieee: [u8; 8],
    },
    PermitJoin {
        secs: u8,
        target: Option<u16>,
    },
    Inventory,
}

enum PumpMsg {
    Cmd(RequestId, Cmd),
    Ncp(Message),
    Stop,
}

/// Requests cancelled before the pump took them.
#[derive(Default)]
struct Cancels {
    /// Last request the pump took.
    taken: u32,
    ids: Vec<u32>,
}

/// Slots the pump's queue keeps once drained.
const INBOX_KEEP: usize = 64;

/// The pump's messages, in order: a queue under a mutex rather than a
/// channel (a channel type costs ~5 KB of firmware). Closed, as a
/// channel's receiver is dropped, when the pump stops.
///
/// A burst (the pump blocked in a control call or on a full event queue:
/// up to [`COMMAND_CAP`] commands plus the NCP's events) must not hold
/// RAM for good, which boards without PSRAM cannot spare: messages are
/// boxed, so the queue's own buffer is a pointer per message (no large
/// contiguous block, nor a large copy under the lock when it grows), and
/// it shrinks back to [`INBOX_KEEP`] slots once drained.
#[derive(Default)]
struct Inbox {
    q: Mutex<(VecDeque<Box<PumpMsg>>, bool)>,
    ready: Condvar,
}

impl Inbox {
    /// `false` once the pump has stopped.
    fn send(&self, m: PumpMsg) -> bool {
        let mut q = self.q.lock().unwrap();
        if q.1 {
            return false;
        }
        q.0.push_back(Box::new(m));
        self.ready.notify_one();
        true
    }

    /// The next message, waiting up to `t` for one.
    fn recv_timeout(&self, t: Duration) -> Option<PumpMsg> {
        let q = self.q.lock().unwrap();
        let (mut q, _) = self
            .ready
            .wait_timeout_while(q, t, |q| q.0.is_empty())
            .unwrap();
        let m = q.0.pop_front();
        if q.0.is_empty() && q.0.capacity() > INBOX_KEEP {
            q.0.shrink_to(INBOX_KEEP);
        }
        m.map(|m| *m)
    }

    /// The pump stopped: what is queued is dropped (the buffer too), later
    /// sends fail.
    fn close(&self) {
        let mut q = self.q.lock().unwrap();
        q.1 = true;
        q.0 = VecDeque::new();
    }
}

#[derive(Default)]
struct Shared {
    queued: AtomicUsize,
    high_water: AtomicUsize,
    cancels: Mutex<Cancels>,
    facts: Facts,
    inbox: Inbox,
}

/// The ZRHP coordinator backend. Dropping it stops the pump.
pub struct ZrhpCoordinator {
    hub: Hub,
    events: Arc<EventQueue>,
    shared: Arc<Shared>,
    next_id: u32,
}

fn endpoint_record(e: EndpointConfig) -> EndpointRecord {
    EndpointRecord {
        ep: e.ep,
        profile: e.profile,
        device_id: e.device_id,
        version: e.version,
        in_clusters: e.in_clusters,
        out_clusters: e.out_clusters,
    }
}

impl ZrhpCoordinator {
    /// Starts the pump on `hub`; its events go to `events`.
    pub fn start(hub: Hub, cfg: BackendConfig, events: Arc<EventQueue>) -> Self {
        let shared = Arc::new(Shared::default());
        let sub = shared.clone();
        hub.subscribe_into(Box::new(move |m| sub.inbox.send(PumpMsg::Ncp(m.clone()))));
        let pump = Pump {
            hub: hub.clone(),
            events: events.clone(),
            shared: shared.clone(),
            endpoints: cfg.endpoints.into_iter().map(endpoint_record).collect(),
            endpoint_retry: None,
            up: None,
            inflight: Vec::new(),
            held: VecDeque::new(),
            aps_slots: None,
            busy_until: None,
        };
        thread::Builder::new()
            .name("zrhp-pump".into())
            .stack_size(PUMP_STACK)
            .spawn(move || pump.run())
            .expect("zrhp pump thread");
        Self {
            hub,
            events,
            shared,
            next_id: 0,
        }
    }

    /// Requests queued for the pump now, and the most at once.
    pub fn queued(&self) -> (usize, usize) {
        (
            self.shared.queued.load(Ordering::Relaxed),
            self.shared.high_water.load(Ordering::Relaxed),
        )
    }

    fn command(&self, req: Request, deadline: Option<Duration>) -> Result<Cmd, CoordError> {
        let state = self.hub.client().state();
        if let Request::Aps(r) = &req {
            // Held by the driver until the session is back.
            if matches!(state, SessionState::Stopped | SessionState::Incompatible) {
                return Err(CoordError::LinkDown);
            }
            let payload = convert::aps_request(r).map_err(engine_error)?;
            return Ok(Cmd::Aps {
                payload,
                timeout: deadline.unwrap_or(CONFIRM_TIMEOUT),
                busy: 0,
            });
        }
        if state != SessionState::Ready {
            return Err(CoordError::LinkDown);
        }
        Ok(match req {
            Request::Gp(r) => {
                let (msg_id, payload) = convert::gp_request(&r).map_err(engine_error)?;
                let opts = deadline.map_or_else(CallOpts::default, CallOpts::timeout);
                Cmd::Gp {
                    msg_id,
                    payload,
                    remote: r.is_remote(),
                    opts,
                }
            }
            Request::Group {
                endpoint,
                group,
                add,
            } => Cmd::Group {
                op: if add { 1 } else { 2 },
                group,
                endpoint,
            },
            Request::RemoveDevice { ieee, nwk } => Cmd::Remove {
                nwk: nwk.unwrap_or(0xFFFF),
                ieee: Ieee::from_u64(ieee).0,
            },
            Request::PermitJoin { duration_s, target } => Cmd::PermitJoin {
                secs: duration_s,
                target,
            },
            Request::DeviceInventory => Cmd::Inventory,
            Request::Aps(_) => unreachable!(),
        })
    }
}

impl Coordinator for ZrhpCoordinator {
    fn submit(
        &mut self,
        req: Request,
        deadline: Option<Duration>,
    ) -> Result<RequestId, CoordError> {
        if self.shared.queued.load(Ordering::Relaxed) >= COMMAND_CAP {
            return Err(CoordError::Busy);
        }
        let cmd = self.command(req, deadline)?;
        self.next_id = self.next_id.wrapping_add(1);
        let id = RequestId(self.next_id);
        let n = self.shared.queued.fetch_add(1, Ordering::Relaxed) + 1;
        // Only `submit` raises the mark. Not `fetch_max`: the Xtensa LLVM
        // backend of Rust 1.90 miscompiles it ("Undefined temporary symbol").
        if n > self.shared.high_water.load(Ordering::Relaxed) {
            self.shared.high_water.store(n, Ordering::Relaxed);
        }
        if !self.shared.inbox.send(PumpMsg::Cmd(id, cmd)) {
            self.shared.queued.fetch_sub(1, Ordering::Relaxed);
            return Err(CoordError::LinkDown);
        }
        Ok(id)
    }

    fn cancel(&mut self, id: RequestId) {
        let mut c = self.shared.cancels.lock().unwrap();
        if id.0 > c.taken && !c.ids.contains(&id.0) {
            c.ids.push(id.0);
        }
    }

    fn status(&self) -> CoordStatus {
        status::status(&self.hub, &self.shared.facts)
    }

    fn permit_join_s(&self) -> u32 {
        self.hub.permit_join_remaining()
    }
}

impl Drop for ZrhpCoordinator {
    fn drop(&mut self) {
        self.shared.inbox.send(PumpMsg::Stop);
        // A pump waiting for room in the queue returns.
        self.events.close();
    }
}

/// [`RadioControl`] of the host core: PTA coexistence, re-applied by the
/// lifecycle in every resume. Thread-safe; the firmware calls it from its
/// Wi-Fi callbacks.
#[derive(Clone)]
pub struct ZrhpRadio(pub Hub);

impl RadioControl for ZrhpRadio {
    fn set_coex(&self, on: bool) -> Result<(), CoordError> {
        self.0.set_coex(on).map_err(coord_error)
    }
}

/// The outcome of an `APS_DATA_REQUEST` (design §4): confirm status 0 →
/// `Delivered`; another confirm status → `NotDelivered` with the
/// `zb_status` (`legacy`: the numbering of NCP images up to 0.1.1); a
/// success without a confirm → `Sent`; an NCP status → `Refused`; no
/// confirm in time → `Unknown(Timeout)`; anything else → `Unknown`.
pub fn aps_terminal(r: Result<Outcome, Error>, legacy: bool) -> Terminal {
    match r {
        Ok(o) => match (
            o.reply.status(),
            o.confirm.as_ref().and_then(|c| c.payload.get(2..4)),
        ) {
            (Status::Success, Some(&[0, _])) => Terminal::Delivered,
            (Status::Success, Some(&[_, zb])) => zrhp_not_delivered(zb, legacy),
            (Status::Success, _) => Terminal::Sent,
            (s, _) => Terminal::Refused(engine_error(Error::Status(s))),
        },
        // Accepted but no confirm in time (a long-polling child).
        Err(Error::Timeout) => Terminal::Unknown(CoordError::Timeout),
        Err(e) => Terminal::Unknown(coord_error(e)),
    }
}

fn inventory(inv: crate::inventory::Inventory) -> Inventory {
    Inventory {
        devices: inv
            .devices
            .into_iter()
            .map(|d| InventoryDevice {
                ieee: u64::from_le_bytes(d.ieee),
                nwk: (d.nwk != 0xFFFF).then_some(d.nwk),
                capability: d.capability,
                device_type: d.device_type,
            })
            .collect(),
        unsupported: inv.unsupported,
    }
}

/// An APS request on the wire.
struct Inflight {
    id: RequestId,
    ticket: Ticket,
    /// Completed with a confirm: its event has not come through yet.
    done: Option<(Result<Outcome, Error>, Instant)>,
    /// Until the NCP took it: the request, to send again after a `BUSY`.
    again: Option<Cmd>,
}

struct Pump {
    hub: Hub,
    events: Arc<EventQueue>,
    shared: Arc<Shared>,
    /// Hub endpoints still to register.
    endpoints: Vec<EndpointRecord>,
    endpoint_retry: Option<Instant>,
    /// Epoch of the session announced up.
    up: Option<u32>,
    inflight: Vec<Inflight>,
    /// APS requests waiting, in order, for a slot of the NCP.
    held: VecDeque<(RequestId, Cmd)>,
    /// `max_aps_inflight` of the last session that reported it.
    aps_slots: Option<usize>,
    /// After a `BUSY`: no APS request goes out before this.
    busy_until: Option<Instant>,
}

/// Like a channel receiver going: queued messages are dropped and later
/// sends fail (the NCP subscriber unsubscribes, `submit` says `LinkDown`).
impl Drop for Pump {
    fn drop(&mut self) {
        self.shared.inbox.close();
    }
}

impl Pump {
    fn run(mut self) {
        // A session already up is announced at once.
        if self.session() {
            while self.step() {}
        }
        debug!("zrhp: pump stopped");
    }

    /// One message (or a poll); `false` stops the pump.
    fn step(&mut self) -> bool {
        let msg = self.shared.inbox.recv_timeout(POLL);
        // The session as it is now: `Up` goes before whatever the NCP sent
        // in the new session.
        if !self.session() {
            return false;
        }
        // `ZrhpCoordinator` sends `Stop` when it goes: no sender is ever
        // gone without one.
        let Some(msg) = msg else {
            return self.replies(true) && self.release();
        };
        // Nothing more reaches the NCP once the coordinator is gone.
        if self.events.is_closed() {
            return false;
        }
        // Outcomes the driver had before this message came.
        if !self.replies(false) {
            return false;
        }
        match msg {
            PumpMsg::Stop => false,
            PumpMsg::Ncp(m) => self.event(&m) && self.release(),
            PumpMsg::Cmd(id, cmd) => {
                if !self.take(id) {
                    self.shared.queued.fetch_sub(1, Ordering::Relaxed);
                    return self.done(id, Terminal::Cancelled);
                }
                if matches!(cmd, Cmd::Aps { .. }) {
                    self.held.push_back((id, cmd));
                    return self.release();
                }
                // Control requests wait for no APS slot.
                self.shared.queued.fetch_sub(1, Ordering::Relaxed);
                self.command(id, cmd)
            }
        }
    }

    /// Sends the held APS requests, in order, while the NCP has a slot.
    fn release(&mut self) -> bool {
        if self.busy_until.is_some_and(|t| Instant::now() < t) {
            return true;
        }
        while self.aps_slot()
            && let Some((id, cmd)) = self.held.pop_front()
        {
            self.shared.queued.fetch_sub(1, Ordering::Relaxed);
            if !self.command(id, cmd) {
                return false;
            }
        }
        if self.held.is_empty() && self.held.capacity() > INBOX_KEEP {
            self.held.shrink_to(INBOX_KEEP);
        }
        true
    }

    /// Whether the NCP takes one more APS request: fewer not yet answered
    /// than its `max_aps_inflight` (none known: no limit).
    fn aps_slot(&self) -> bool {
        self.aps_slots.is_none_or(|n| {
            self.inflight.iter().filter(|f| f.done.is_none() && !f.ticket.accepted()).count() < n
        })
    }

    fn emit(&self, ev: CoordEvent) -> bool {
        self.events.push(ev)
    }

    fn done(&self, id: RequestId, t: Terminal) -> bool {
        self.emit(CoordEvent::Done { id, t })
    }

    /// `false` when request `id` was cancelled.
    fn take(&self, id: RequestId) -> bool {
        let mut c = self.shared.cancels.lock().unwrap();
        c.taken = id.0;
        let before = c.ids.len();
        c.ids.retain(|x| *x != id.0);
        c.ids.len() == before
    }

    /// Session changes: the hub endpoints, then `Up`; `Down` when the
    /// session is gone.
    fn session(&mut self) -> bool {
        let c = self.hub.client();
        let current = c.session();
        if let Some(s) = &current {
            self.aps_slots = s
                .negotiated
                .capacity(capacity::MAX_APS_INFLIGHT)
                .map(|n| n.max(1) as usize);
        }
        let session = current.filter(|_| c.state() == SessionState::Ready);
        let epoch = session.as_ref().map(|s| s.link.peer_cookie);
        if self.up.is_some() && self.up != epoch {
            self.up = None;
            if !self.emit(CoordEvent::Session(Box::new(SessionEvent::Down))) {
                return false;
            }
        }
        let (Some(s), Some(epoch)) = (session, epoch) else {
            return true;
        };
        if self.up.is_some() || !self.register() {
            return true;
        }
        self.up = Some(epoch);
        let (info, caps) = self.shared.facts.of(&s);
        self.emit(CoordEvent::Session(Box::new(SessionEvent::Up {
            epoch,
            info,
            caps,
        })))
    }

    /// The hub endpoints the store lacks are added (once per process).
    fn register(&mut self) -> bool {
        if self.endpoints.is_empty() {
            return true;
        }
        if self.endpoint_retry.is_some_and(|t| Instant::now() < t) {
            return false;
        }
        let known = self.hub.store_state().endpoints;
        for ep in std::mem::take(&mut self.endpoints) {
            let stored = known.get(&ep.ep);
            let r = if stored == Some(&ep) {
                Ok(())
            } else {
                // Another image's descriptor (e.g. with OTA Upgrade): the
                // NCP replaces an endpoint with the same number.
                if stored.is_some() {
                    info!("zrhp: hub endpoint {} differs from the stored one: registered again", ep.ep);
                }
                self.hub.add_endpoint(ep.clone())
            };
            match r {
                Ok(()) => info!("zrhp: hub endpoint {} registered", ep.ep),
                Err(e) => {
                    warn!("zrhp: NWK_ADD_ENDPOINT {}: {e}", ep.ep);
                    self.endpoints.push(ep);
                }
            }
        }
        self.endpoint_retry = (!self.endpoints.is_empty()).then(|| Instant::now() + ENDPOINT_RETRY);
        self.endpoints.is_empty()
    }

    /// Emits the outcomes of APS requests a response (or the driver)
    /// decided; parks those a confirm decided until their event comes.
    /// `idle`: the queue is empty, so a confirm parked for long is not
    /// coming (the time the pump spent blocked does not count otherwise).
    fn replies(&mut self, idle: bool) -> bool {
        let mut i = 0;
        while i < self.inflight.len() {
            let f = &mut self.inflight[i];
            if f.again.is_some() && f.ticket.accepted() {
                f.again = None;
            }
            if f.done.is_none()
                && let Some(r) = f.ticket.try_wait()
            {
                if matches!(&r, Ok(o) if o.confirm.is_some()) {
                    f.done = Some((r, Instant::now()));
                } else {
                    let f = self.inflight.remove(i);
                    // The NCP's queue was full: again shortly, first.
                    if let (Ok(o), Some(Cmd::Aps { payload, timeout, busy })) = (&r, f.again)
                        && o.reply.status() == Status::Busy
                        && busy < BUSY_TRIES
                    {
                        self.shared.queued.fetch_add(1, Ordering::Relaxed);
                        self.held.push_front((f.id, Cmd::Aps { payload, timeout, busy: busy + 1 }));
                        self.busy_until = Some(Instant::now() + BUSY_RETRY);
                        continue;
                    }
                    if !self.finish(f.id, f.ticket.tid, r) {
                        return false;
                    }
                    continue;
                }
            }
            // The event should have come by now.
            if idle
                && f.done
                    .as_ref()
                    .is_some_and(|(_, at)| at.elapsed() >= CONFIRM_SLACK)
            {
                let f = self.inflight.remove(i);
                if let Some((r, _)) = f.done
                    && !self.finish(f.id, f.ticket.tid, r)
                {
                    return false;
                }
                continue;
            }
            i += 1;
        }
        true
    }

    fn finish(&self, id: RequestId, tid: u16, r: Result<Outcome, Error>) -> bool {
        // zigbee-rs NCP images up to 0.1.1 number APS statuses off by one.
        let legacy = self
            .hub
            .client()
            .session()
            .is_some_and(|s| legacy_zb_status(&s.negotiated.fw));
        let t = aps_terminal(r, legacy);
        if let Terminal::NotDelivered(e) | Terminal::Refused(e) | Terminal::Unknown(e) = &t {
            debug!("APS_DATA_REQUEST tid {tid}: {e}");
        }
        self.done(id, t)
    }

    fn event(&mut self, m: &Message) -> bool {
        if m.msg_id == ids::EVT_APS_DATA_CONFIRM {
            let i = self
                .inflight
                .iter()
                .position(|f| matches!(&f.done, Some((Ok(o), _)) if o.confirm.as_ref() == Some(m)));
            return match i.map(|i| self.inflight.remove(i)) {
                Some(Inflight {
                    id,
                    ticket,
                    done: Some((r, _)),
                    ..
                }) => self.finish(id, ticket.tid, r),
                _ => true,
            };
        }
        match convert::coord_event(m) {
            Some(ev) => self.emit(ev),
            None => true,
        }
    }

    fn command(&mut self, id: RequestId, cmd: Cmd) -> bool {
        let hub = &self.hub;
        let t = match cmd {
            Cmd::Aps {
                payload,
                timeout,
                busy,
            } => {
                let opts = CallOpts {
                    confirm: Some(Confirm::aps(timeout)),
                    ..CallOpts::default()
                };
                return match hub
                    .client()
                    .start(ids::APS_DATA_REQUEST, payload.clone(), opts)
                {
                    Ok(ticket) => {
                        self.inflight.push(Inflight {
                            id,
                            ticket,
                            done: None,
                            again: Some(Cmd::Aps {
                                payload,
                                timeout,
                                busy,
                            }),
                        });
                        true
                    }
                    Err(e) => self.done(id, Terminal::Unknown(coord_error(e))),
                };
            }
            Cmd::Gp {
                msg_id,
                payload,
                remote,
                opts,
            } => match hub.client().call_raw(msg_id, &payload, opts) {
                Ok(out) => {
                    // The ZCL TSN the NCP used, which its EVT_GP_REMOTE_* carries.
                    let p = Progress::Accepted {
                        zcl_tsn: out.reply.payload.get(1).copied(),
                    };
                    if remote && !self.emit(CoordEvent::Progress { id, p }) {
                        return false;
                    }
                    Terminal::Done(Reply::None)
                }
                Err(e) => control_outcome(Err(e)),
            },
            Cmd::Group {
                op,
                group,
                endpoint,
            } => control_outcome(hub.group_membership(op, group, endpoint)),
            Cmd::Remove { nwk, ieee } => control_outcome(hub.purge_device(nwk, ieee)),
            Cmd::PermitJoin { secs, target } => control_outcome(hub.permit_join_on(secs, target)),
            Cmd::Inventory => match hub.ncp_devices() {
                Ok(inv) => Terminal::Done(Reply::Inventory(Box::new(inventory(inv)))),
                Err(e @ Error::Status(_)) => Terminal::Refused(coord_error(e)),
                Err(e) => Terminal::Unknown(coord_error(e)),
            },
        };
        self.done(id, t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::FormRequest;
    use crate::store::Store;
    use crate::tests::{T, start};
    use hub_coord::{ApsDst, ApsRequest, GpRequest, TxOptions};
    use zigbee_zrhp::codec::Bytes16;
    use zigbee_zrhp::msg::data::EvtApsDataIndication;

    fn rig() -> (Hub, crate::sim::SimNcp, ZrhpCoordinator, Arc<EventQueue>) {
        let (hub, sim) = start(Store::volatile());
        assert!(hub.wait_ready(T));
        hub.form(FormRequest::default()).unwrap();
        sim.with(|s| s.trace = Some(Vec::new()));
        let q = Arc::new(EventQueue::new(hub_coord::EVENT_SLOTS, Box::new(|| {})));
        let coord = ZrhpCoordinator::start(hub.clone(), BackendConfig::default(), q.clone());
        (hub, sim, coord, q)
    }

    /// Events until `stop` holds for one of them.
    fn events_until(q: &EventQueue, stop: impl Fn(&CoordEvent) -> bool) -> Vec<CoordEvent> {
        let mut got = Vec::new();
        let t = Instant::now();
        while !got.iter().any(&stop) {
            assert!(t.elapsed() < T, "{got:?}");
            q.drain(usize::MAX, |e| got.push(e));
            thread::sleep(Duration::from_millis(5));
        }
        got
    }

    fn sent(sim: &crate::sim::SimNcp, id: u16) -> usize {
        sim.with(|s| {
            s.trace
                .as_ref()
                .unwrap()
                .iter()
                .filter(|(m, _)| *m == id)
                .count()
        })
    }

    fn aps() -> Request {
        Request::Aps(ApsRequest {
            dst: ApsDst::Nwk(0x1234),
            dst_ep: 1,
            src_ep: 1,
            profile: 0x0104,
            cluster: 6,
            payload: vec![0x01, 0x07, 0x01],
            options: TxOptions {
                ack: true,
                extended_timeout: false,
            },
        })
    }

    fn gp_config() -> Request {
        Request::Gp(GpRequest::SinkConfig {
            mode: 1,
            communication_mode: 2,
            group: 0x0B84,
            exit_mode: 2,
            window_s: 180,
            security_level: 0,
            groupcast_radius: 0,
        })
    }

    fn done(ev: &CoordEvent, want: RequestId) -> bool {
        matches!(ev, CoordEvent::Done { id, .. } if *id == want)
    }

    #[test]
    fn the_inbox_keeps_order_and_closes_like_a_dropped_receiver() {
        let inbox = Inbox::default();
        let short = Duration::from_millis(1);
        assert!(inbox.recv_timeout(short).is_none());
        assert!(inbox.send(PumpMsg::Stop));
        assert!(inbox.send(PumpMsg::Cmd(RequestId(1), Cmd::Inventory)));
        assert!(matches!(inbox.recv_timeout(short), Some(PumpMsg::Stop)));
        // Closing drops what is queued; later sends fail.
        inbox.close();
        assert!(inbox.recv_timeout(short).is_none());
        assert!(!inbox.send(PumpMsg::Stop));
    }

    #[test]
    fn a_drained_inbox_gives_back_the_buffer_of_a_burst() {
        let inbox = Inbox::default();
        let short = Duration::from_millis(1);
        for i in 0..COMMAND_CAP as u32 {
            assert!(inbox.send(PumpMsg::Cmd(RequestId(i), Cmd::Inventory)));
        }
        assert!(inbox.q.lock().unwrap().0.capacity() >= COMMAND_CAP);
        for i in 0..COMMAND_CAP as u32 {
            let m = inbox.recv_timeout(short);
            assert!(matches!(m, Some(PumpMsg::Cmd(RequestId(id), _)) if id == i));
        }
        assert!(inbox.recv_timeout(short).is_none());
        assert!(inbox.q.lock().unwrap().0.capacity() <= INBOX_KEEP);
        // Still usable.
        assert!(inbox.send(PumpMsg::Stop));
        assert!(matches!(inbox.recv_timeout(short), Some(PumpMsg::Stop)));
    }

    /// A removal purges the device from the NCP's `DEVICE` table
    /// (`TBL_INVALIDATE`) and the host store's records even when the NCP
    /// does not implement `NWK_REMOVE_DEVICE` (the zigbee-rs NCP): the
    /// inventory no longer lists it; a child entry the NCP keeps does.
    #[test]
    fn a_removal_purges_the_ncps_table_and_the_store_record() {
        use zigbee_zrhp::msg::network::EvtDeviceJoined;
        let (hub, sim, mut coord, q) = rig();
        events_until(
            &q,
            |e| matches!(e, CoordEvent::Session(s) if matches!(**s, SessionEvent::Up { .. })),
        );
        let (gone, child) = (0x00158d0001a2b3c4u64, 0x00158d0001a2b3c5u64);
        for (ieee, nwk) in [(gone, 0x1234), (child, 0x2345)] {
            let ev = EvtDeviceJoined {
                nwk,
                ieee: Ieee::from_u64(ieee),
                parent_nwk: 0,
                join_type: 0,
                capability: 0x80,
                decision: 0,
            };
            sim.inject_event(
                ids::EVT_DEVICE_JOINED,
                crate::driver::encode_body(&ev).unwrap(),
            );
        }
        let t = Instant::now();
        while hub.store_state().devices.len() < 2 {
            assert!(t.elapsed() < T);
            thread::sleep(Duration::from_millis(5));
        }
        sim.with(|s| {
            s.table_devices = vec![
                (gone.to_le_bytes(), 0x1234, None),
                (child.to_le_bytes(), 0x2345, Some(0x80)),
            ];
            s.children = Some(vec![(child.to_le_bytes(), 0x2345, 0x80)]);
            s.unsupported.push(ids::NWK_REMOVE_DEVICE);
        });
        let ieees = |coord: &mut ZrhpCoordinator, q: &EventQueue| {
            let id = coord.submit(Request::DeviceInventory, None).unwrap();
            let evs = events_until(q, |e| done(e, id));
            match evs.into_iter().find(|e| done(e, id)) {
                Some(CoordEvent::Done {
                    t: Terminal::Done(Reply::Inventory(inv)),
                    ..
                }) => inv.devices.iter().map(|d| d.ieee).collect::<Vec<_>>(),
                e => panic!("{e:?}"),
            }
        };
        assert_eq!(ieees(&mut coord, &q), [gone, child]);
        for (ieee, nwk) in [(gone, 0x1234), (child, 0x2345)] {
            let id = coord
                .submit(
                    Request::RemoveDevice {
                        ieee,
                        nwk: Some(nwk),
                    },
                    None,
                )
                .unwrap();
            let evs = events_until(&q, |e| done(e, id));
            assert!(
                evs.iter().any(|e| matches!(e, CoordEvent::Done { id: i, t: Terminal::Done(Reply::None) } if *i == id)),
                "{evs:?}"
            );
        }
        assert_eq!(sent(&sim, ids::NWK_REMOVE_DEVICE), 2);
        assert_eq!(sent(&sim, ids::TBL_INVALIDATE), 2);
        assert!(hub.store_state().devices.is_empty());
        assert_eq!(
            sim.state().table_devices,
            [(child.to_le_bytes(), 0x2345, Some(0x80))],
            "the child entry stays"
        );
        assert_eq!(ieees(&mut coord, &q), [child]);
        drop(coord);
        hub.shutdown();
        sim.stop();
    }

    /// A child the NCP dropped after a timeout (`EVT_DEVICE_LEFT` reason
    /// 4, flags 0) keeps its host store record and reaches the engine as
    /// `aged_out`; a leave without the rejoin flag drops the record.
    #[test]
    fn an_aged_out_child_keeps_its_store_record() {
        use zigbee_zrhp::msg::network::{EvtDeviceJoined, EvtDeviceLeft};
        let (hub, sim, coord, q) = rig();
        events_until(
            &q,
            |e| matches!(e, CoordEvent::Session(s) if matches!(**s, SessionEvent::Up { .. })),
        );
        let (router, gone) = (0x00158d0001a2b3c6u64, 0x00158d0001a2b3c7u64);
        for (ieee, nwk) in [(router, 0x3456), (gone, 0x4567)] {
            let ev = EvtDeviceJoined {
                nwk,
                ieee: Ieee::from_u64(ieee),
                parent_nwk: 0,
                join_type: 0,
                capability: 0x8E,
                decision: 0,
            };
            sim.inject_event(
                ids::EVT_DEVICE_JOINED,
                crate::driver::encode_body(&ev).unwrap(),
            );
        }
        let t = Instant::now();
        while hub.store_state().devices.len() < 2 {
            assert!(t.elapsed() < T);
            thread::sleep(Duration::from_millis(5));
        }
        for (ieee, nwk, reason) in [(router, 0x3456, 4), (gone, 0x4567, 1)] {
            let ev = EvtDeviceLeft {
                nwk,
                ieee: Ieee::from_u64(ieee),
                flags: 0,
                reason,
            };
            sim.inject_event(
                ids::EVT_DEVICE_LEFT,
                crate::driver::encode_body(&ev).unwrap(),
            );
        }
        let evs = events_until(
            &q,
            |e| matches!(e, CoordEvent::DeviceLeft { ieee, .. } if *ieee == gone),
        );
        let left: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                CoordEvent::DeviceLeft { ieee, aged_out, .. } => Some((*ieee, *aged_out)),
                _ => None,
            })
            .collect();
        assert_eq!(left, [(router, true), (gone, false)]);
        let t = Instant::now();
        while hub.store_state().devices.len() > 1 {
            assert!(t.elapsed() < T);
            thread::sleep(Duration::from_millis(5));
        }
        let kept: Vec<_> = hub.store_state().devices.keys().copied().collect();
        assert_eq!(kept, [router.to_le_bytes()]);
        drop(coord);
        hub.shutdown();
        sim.stop();
    }

    #[test]
    fn submit_never_waits_and_control_requests_are_barriers() {
        let (hub, sim, mut coord, q) = rig();
        events_until(
            &q,
            |e| matches!(e, CoordEvent::Session(s) if matches!(**s, SessionEvent::Up { .. })),
        );
        sim.with(|s| s.stall = Some(ids::GP_SINK_CONFIG));
        let t = Instant::now();
        let gp = coord
            .submit(gp_config(), Some(Duration::from_millis(600)))
            .unwrap();
        let first = coord.submit(aps(), None).unwrap();
        let cancelled = coord.submit(aps(), None).unwrap();
        coord.cancel(cancelled);
        assert!(t.elapsed() < Duration::from_millis(50), "{:?}", t.elapsed());
        // Nothing passes the unanswered GP request.
        thread::sleep(Duration::from_millis(300));
        assert_eq!(sent(&sim, ids::GP_SINK_CONFIG), 1);
        assert_eq!(sent(&sim, ids::APS_DATA_REQUEST), 0);
        let evs = events_until(&q, |e| done(e, first));
        let outcomes: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                CoordEvent::Done { id, t } => Some((*id, t.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            outcomes,
            [
                (gp, Terminal::Unknown(engine_error(Error::Timeout))),
                (cancelled, Terminal::Cancelled),
                (first, Terminal::Delivered),
            ]
        );
        assert_eq!(sent(&sim, ids::APS_DATA_REQUEST), 1);
        assert_eq!(coord.queued().0, 0);
        drop(coord);
        hub.shutdown();
        sim.stop();
    }

    #[test]
    fn aps_outcome_follows_the_indications_the_ncp_sent_before_its_confirm() {
        let (hub, sim) = start(Store::volatile());
        assert!(hub.wait_ready(T));
        hub.form(FormRequest::default()).unwrap();
        // The lifecycle passes the indication on late, after the driver
        // has completed the request with the confirm that followed it.
        hub.subscribe_into(Box::new(|m| {
            if m.msg_id == ids::EVT_APS_DATA_INDICATION {
                thread::sleep(Duration::from_millis(300));
            }
            true
        }));
        let q = Arc::new(EventQueue::new(hub_coord::EVENT_SLOTS, Box::new(|| {})));
        let mut coord = ZrhpCoordinator::start(hub.clone(), BackendConfig::default(), q.clone());
        let ind = EvtApsDataIndication {
            dst_mode: 2,
            dst: 0,
            dst_ep: 1,
            src_nwk: 0x1234,
            src_ieee: Ieee::from_u64(0),
            src_ep: 1,
            profile: 0x0104,
            cluster: 6,
            flags: 0,
            lqi: 200,
            rssi: -50,
            last_hop_nwk: 0x1234,
            aps_counter: 1,
            key_source: 0,
            rx_time_ms: 0,
            asdu: Bytes16(&[0x18, 0x01, 0x0B, 0x01, 0x00]),
        };
        sim.inject_event(
            ids::EVT_APS_DATA_INDICATION,
            crate::driver::encode_body(&ind).unwrap(),
        );
        let id = coord.submit(aps(), None).unwrap();
        let evs = events_until(&q, |e| done(e, id));
        let at = |f: &dyn Fn(&CoordEvent) -> bool| evs.iter().position(f).unwrap();
        assert!(
            at(&|e| matches!(e, CoordEvent::Indication(_))) < at(&|e| done(e, id)),
            "{evs:?}"
        );
        drop(coord);
        hub.shutdown();
        sim.stop();
    }

    /// Events until `stop` holds for all of them.
    fn collect(q: &EventQueue, got: &mut Vec<CoordEvent>, stop: impl Fn(&[CoordEvent]) -> bool) {
        let t = Instant::now();
        while !stop(got) {
            assert!(t.elapsed() < T, "{got:?}");
            q.drain(usize::MAX, |e| got.push(e));
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn up(ev: &CoordEvent) -> Option<u32> {
        match ev {
            CoordEvent::Session(s) => match **s {
                SessionEvent::Up { epoch, .. } => Some(epoch),
                _ => None,
            },
            _ => None,
        }
    }

    fn down(ev: &CoordEvent) -> bool {
        matches!(ev, CoordEvent::Session(s) if **s == SessionEvent::Down)
    }

    fn outcome(evs: &[CoordEvent], want: RequestId) -> (usize, &Terminal) {
        evs.iter()
            .enumerate()
            .find_map(|(i, e)| match e {
                CoordEvent::Done { id, t } if *id == want => Some((i, t)),
                _ => None,
            })
            .unwrap()
    }

    /// A request on the wire (`a`, unanswered) and one submitted while the
    /// session is down (`b`, held by the driver) across a session loss;
    /// `lose` ends the session and keeps the next one from negotiating
    /// until the pump has reported it down.
    fn across_a_session_loss(
        lose: impl FnOnce(&Hub, &crate::sim::SimNcp),
    ) -> (
        Vec<CoordEvent>,
        RequestId,
        RequestId,
        u32,
        crate::sim::SimNcp,
    ) {
        let (hub, sim, mut coord, q) = rig();
        let mut evs = Vec::new();
        collect(&q, &mut evs, |e| e.iter().any(|e| up(e).is_some()));
        let epoch = evs.iter().find_map(up).unwrap();
        sim.with(|s| s.stall = Some(ids::APS_DATA_REQUEST));
        let a = coord.submit(aps(), None).unwrap();
        while sent(&sim, ids::APS_DATA_REQUEST) == 0 {
            thread::sleep(Duration::from_millis(5));
        }
        sim.with(|s| s.stall = Some(ids::SYS_NEGOTIATE));
        lose(&hub, &sim);
        let mut evs = Vec::new();
        collect(&q, &mut evs, |e| e.iter().any(down));
        let b = coord.submit(aps(), None).unwrap();
        sim.with(|s| s.stall = None);
        collect(&q, &mut evs, |e| {
            e.iter().filter(|e| done(e, a) || done(e, b)).count() == 2
        });
        drop(coord);
        hub.shutdown();
        sim.stop();
        (evs, a, b, epoch, sim)
    }

    /// ZRHP holds requests across a resync (same NCP cookie): `Down` ends
    /// none of them; after `Up` the one on the wire is sent again (`RETRY`)
    /// and both report their real outcome.
    #[test]
    fn a_resync_holds_requests_and_their_outcomes_follow_up() {
        let (evs, a, b, epoch, sim) = across_a_session_loss(|hub, _| hub.client().reopen());
        let at_up = evs
            .iter()
            .position(|e| up(e) == Some(epoch))
            .expect("up again, same epoch");
        assert!(evs.iter().position(down).unwrap() < at_up, "{evs:?}");
        for id in [a, b] {
            let (i, t) = outcome(&evs, id);
            assert_eq!(t, &Terminal::Delivered, "{evs:?}");
            assert!(i > at_up, "{evs:?}");
        }
        assert!(
            sim.state()
                .requests
                .contains(&(ids::APS_DATA_REQUEST, true)),
            "sent again with RETRY"
        );
    }

    /// After an NCP restart the request on the wire ends unknown (its
    /// outcome is lost with the NCP); the held one is sent after `Up`.
    #[test]
    fn an_ncp_restart_ends_requests_on_the_wire_and_sends_the_others_after_up() {
        let (evs, a, b, epoch, _) = across_a_session_loss(|_, sim| sim.reboot());
        let at_up = evs
            .iter()
            .position(|e| up(e).is_some_and(|e| e != epoch))
            .expect("a new epoch");
        assert!(evs.iter().position(down).unwrap() < at_up, "{evs:?}");
        let (_, t) = outcome(&evs, a);
        assert!(
            matches!(t, Terminal::Unknown(e) if *e.meaning() == CoordError::DeliveryUnknown),
            "{t:?}"
        );
        let (i, t) = outcome(&evs, b);
        assert_eq!((t, i > at_up), (&Terminal::Delivered, true), "{evs:?}");
    }

    fn permit_join() -> Request {
        Request::PermitJoin {
            duration_s: 60,
            target: None,
        }
    }

    fn outcomes(evs: &[CoordEvent]) -> Vec<(RequestId, Terminal)> {
        evs.iter()
            .filter_map(|e| match e {
                CoordEvent::Done { id, t } => Some((*id, t.clone())),
                _ => None,
            })
            .collect()
    }

    fn until_sent(sim: &crate::sim::SimNcp, n: usize) {
        let t = Instant::now();
        while sent(sim, ids::APS_DATA_REQUEST) < n {
            assert!(t.elapsed() < T);
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// At most `max_aps_inflight` APS requests the NCP has not answered:
    /// the others wait in the pump. A control request waits for none.
    #[test]
    fn unanswered_aps_requests_hold_the_others_but_no_control_request() {
        use crate::sim::SIM_MAX_APS_INFLIGHT;
        let (hub, sim, mut coord, q) = rig();
        events_until(&q, |e| up(e).is_some());
        sim.with(|s| s.stall = Some(ids::APS_DATA_REQUEST));
        for _ in 0..SIM_MAX_APS_INFLIGHT + 2 {
            coord.submit(aps(), None).unwrap();
        }
        until_sent(&sim, SIM_MAX_APS_INFLIGHT);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(sent(&sim, ids::APS_DATA_REQUEST), SIM_MAX_APS_INFLIGHT);
        assert_eq!(coord.queued().0, 2);
        let pj = coord.submit(permit_join(), None).unwrap();
        let evs = events_until(&q, |e| done(e, pj));
        assert_eq!(outcomes(&evs), [(pj, Terminal::Done(Reply::None))]);
        drop(coord);
        hub.shutdown();
        sim.stop();
    }

    /// The NCP frees an APS slot when it takes a request, not at its
    /// confirm: 16 requests waiting for their confirms (sleepy or
    /// unreachable devices) hold neither a later APS request nor a
    /// control request.
    #[test]
    fn unconfirmed_aps_requests_hold_nothing() {
        let (hub, sim, mut coord, q) = rig();
        events_until(&q, |e| up(e).is_some());
        sim.with(|s| s.aps_confirm_delay = Duration::from_secs(2));
        let t = Instant::now();
        let mut reqs: Vec<_> = (0..16).map(|_| coord.submit(aps(), None).unwrap()).collect();
        until_sent(&sim, 16);
        let pj = coord.submit(permit_join(), None).unwrap();
        reqs.push(coord.submit(aps(), None).unwrap());
        let evs = events_until(&q, |e| done(e, pj));
        until_sent(&sim, 17);
        assert!(t.elapsed() < Duration::from_secs(2), "before the first confirm");
        assert_eq!(outcomes(&evs), [(pj, Terminal::Done(Reply::None))]);
        let evs = events_until(&q, |e| done(e, *reqs.last().unwrap()));
        let delivered: Vec<_> = reqs.iter().map(|id| (*id, Terminal::Delivered)).collect();
        assert_eq!(outcomes(&evs), delivered);
        drop(coord);
        hub.shutdown();
        sim.stop();
    }

    /// An APS request the NCP refuses with `BUSY` (its queue full) goes
    /// again shortly instead of failing.
    #[test]
    fn a_busy_ncp_gets_the_request_again() {
        let (hub, sim, mut coord, q) = rig();
        events_until(&q, |e| up(e).is_some());
        sim.with(|s| s.busy = 2);
        let id = coord.submit(aps(), None).unwrap();
        let evs = events_until(&q, |e| done(e, id));
        assert_eq!(outcomes(&evs), [(id, Terminal::Delivered)]);
        assert_eq!(sent(&sim, ids::APS_DATA_REQUEST), 3);
        assert_eq!(coord.queued().0, 0);
        drop(coord);
        hub.shutdown();
        sim.stop();
    }

    #[test]
    fn requests_beyond_the_command_cap_are_refused() {
        let (hub, sim, mut coord, q) = rig();
        events_until(&q, |e| matches!(e, CoordEvent::Session(_)));
        sim.with(|s| s.stall = Some(ids::GP_SINK_CONFIG));
        coord
            .submit(gp_config(), Some(Duration::from_secs(2)))
            .unwrap();
        // The pump has taken the GP request and waits for its answer.
        while sent(&sim, ids::GP_SINK_CONFIG) == 0 {
            thread::sleep(Duration::from_millis(5));
        }
        for _ in 0..COMMAND_CAP {
            coord.submit(aps(), None).unwrap();
        }
        assert_eq!(coord.submit(aps(), None), Err(CoordError::Busy));
        assert_eq!(coord.queued(), (COMMAND_CAP, COMMAND_CAP));
        drop(coord);
        hub.shutdown();
        sim.stop();
    }
}
