//! [`EzspCoordinator`] and the `ezsp-worker` thread that owns the NCP link
//! (module docs of [`super`]): the loop, events and the session here,
//! requests in [`requests`], admin jobs in [`jobs`].

mod backup;
mod jobs;
mod requests;

use std::collections::{BTreeMap, VecDeque};
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use hub_coord::{
    ApsDst, ApsRequest, BackendConfig, Beacon, ChannelChange, Child, Code, CoordError, CoordEvent,
    CoordStatus, Coordinator, DiagCounter, Diagnostics, EndpointConfig, EnergyChannel, EventQueue,
    FormParams, Inventory, InventoryDevice, Kind, NcpInfo, NcpStatus, Neighbor, NetChange,
    NetState, NetworkIdentity, NetworkOwnership, NetworkParams, Progress, Reply, Request,
    RequestId, Route, SessionEvent, SessionState, TablesRead, Terminal,
};
use log::{debug, info, warn};

use super::admin::{Answer, Job, Op, Slot};
use super::convert::{
    CAPS_CONFIG, COUNTERS, capabilities, child_capability, energy_of_rssi, ezsp_error, host_error,
    indication, is_link_error, sent_terminal,
};
use super::{EzspOptions, Fitted, LinkReport, SessionTimes};
use crate::ezsp::command::{
    BroadcastExtra, Callback, Command, Endpoint, MulticastExtra, Response, id,
};
use crate::ezsp::types::*;
use crate::ezsp::version::EzspVersion;
use crate::host::{
    Clock, EventReceiver, EzspHost, HostError, HostEvent, NcpInfo as HostInfo, Transport,
};

/// Requests queued for the worker at most; more are refused with
/// [`CoordError::Busy`] (design §3.5, as the ZRHP backend).
pub const COMMAND_CAP: usize = 2048;

/// Events the worker holds for the application before it stops reading
/// callbacks (the application is behind).
const OUT_LIMIT: usize = 16;
/// IEEE → short address entries kept for IEEE-addressed requests.
const NWK_CACHE: usize = 256;
/// A request that timed out keeps its message tag this long for its late
/// `messageSentHandler`.
const EXPIRED_GRACE_MS: u64 = 300_000;
/// Slots the queue keeps once drained.
const QUEUE_KEEP: usize = 64;

/// The well-known link key "ZigBeeAlliance09" (Zigbee 3.0 centralized
/// security): the transient key of joining devices while the network is
/// open.
const ZIGBEE_ALLIANCE_09: KeyData = *b"ZigBeeAlliance09";
/// Wildcard EUI64 of a transient key for every joining device.
const ANY_EUI64: Eui64 = Eui64([0xFF; 8]);

/// ZDO clusters (Zigbee spec 2.4.3).
const ZDO_DEVICE_ANNCE: u16 = 0x0013;
const ZDO_MGMT_PERMIT_JOINING_REQ: u16 = 0x0036;

/// A request as the worker runs it.
pub(crate) enum Cmd {
    Aps { req: ApsRequest, timeout: Duration },
    Group { endpoint: u8, group: u16, add: bool },
    Remove { ieee: u64, nwk: Option<u16> },
    PermitJoin { secs: u8, target: Option<u16> },
    Inventory,
}

/// The worker's input, in order.
pub(crate) enum Msg {
    Cmd(RequestId, Box<Cmd>),
    Admin(Job),
    Stop,
}

/// The worker's input queue: a queue under a mutex, closed when the
/// worker stops; it gives back the buffer of a burst once drained.
#[derive(Default)]
pub(crate) struct Inbox {
    q: Mutex<(VecDeque<Msg>, bool)>,
}

impl Inbox {
    /// `Err(m)` once the worker has stopped.
    pub(crate) fn send(&self, m: Msg) -> Result<(), Msg> {
        let mut q = self.q.lock().unwrap();
        if q.1 {
            return Err(m);
        }
        q.0.push_back(m);
        Ok(())
    }

    fn take(&self) -> VecDeque<Msg> {
        let mut q = self.q.lock().unwrap();
        std::mem::take(&mut q.0)
    }

    fn close(&self) -> VecDeque<Msg> {
        let mut q = self.q.lock().unwrap();
        q.1 = true;
        std::mem::take(&mut q.0)
    }
}

/// Requests cancelled before the worker took them.
#[derive(Default)]
struct Cancels {
    /// Last request the worker took.
    taken: u32,
    ids: Vec<u32>,
}

/// State shared by the coordinator, the admin handle and the worker.
pub(crate) struct Shared {
    pub(crate) inbox: Inbox,
    queued: AtomicUsize,
    high_water: AtomicUsize,
    cancels: Mutex<Cancels>,
    pub(crate) status: Mutex<CoordStatus>,
    pub(crate) tx_power: Mutex<Option<i8>>,
    pub(crate) stopped: AtomicBool,
}

fn initial_status() -> CoordStatus {
    CoordStatus {
        session: SessionState::Down,
        epoch: None,
        ncp: None,
        caps: None,
        network_state: NetState::None,
        network: None,
        ownership: NetworkOwnership::Unknown,
        permit_join_s: 0,
    }
}

/// The EZSP coordinator backend. Dropping it stops the worker.
pub struct EzspCoordinator {
    shared: Arc<Shared>,
    events: Arc<EventQueue>,
    next_id: u32,
    confirm_timeout: Duration,
    /// Control requests wait for the next session too
    /// ([`EzspOptions::traffic_hold`]).
    hold_controls: bool,
    worker: Option<JoinHandle<()>>,
}

impl EzspCoordinator {
    /// Starts the worker on `transport`; its events go to `events`. The
    /// worker resets the NCP and starts a session at once. Fails only for
    /// an invalid host configuration.
    pub fn start<T, C>(
        transport: T,
        clock: C,
        opts: EzspOptions,
        cfg: BackendConfig,
        events: Arc<EventQueue>,
    ) -> Result<(EzspCoordinator, super::EzspAdmin), HostError>
    where
        T: Transport + 'static,
        C: Clock + 'static,
    {
        let (host, rx) = EzspHost::new(transport, clock, opts.host.clone())?;
        let shared = Arc::new(Shared {
            inbox: Inbox::default(),
            queued: AtomicUsize::new(0),
            high_water: AtomicUsize::new(0),
            cancels: Mutex::default(),
            status: Mutex::new(initial_status()),
            tx_power: Mutex::new(opts.tx_power),
            stopped: AtomicBool::new(false),
        });
        let stack = opts.worker_stack;
        let confirm_timeout = opts.confirm_timeout;
        let hold_controls = opts.traffic.is_some() && !opts.traffic_hold.is_zero();
        // On the heap: moved by value, the worker (~1.8 KB) would sit in
        // the spawned thread's stack several times over (the closure, its
        // calls, `run`'s `self`).
        let worker = Box::new(Worker::new(
            host,
            rx,
            opts,
            cfg.endpoints,
            shared.clone(),
            events.clone(),
        ));
        let handle = thread::Builder::new()
            .name("ezsp-worker".into())
            .stack_size(stack)
            .spawn(move || {
                let mut worker = worker;
                worker.run()
            })
            .map_err(HostError::Io)?;
        let admin = super::EzspAdmin {
            shared: shared.clone(),
        };
        Ok((
            EzspCoordinator {
                shared,
                events,
                next_id: 0,
                confirm_timeout,
                hold_controls,
                worker: Some(handle),
            },
            admin,
        ))
    }

    /// Cancellations the worker keeps for requests not taken yet.
    #[cfg(test)]
    pub(crate) fn cancels_kept(&self) -> usize {
        self.shared.cancels.lock().unwrap().ids.len()
    }

    /// Requests queued for the worker now, and the most at once.
    pub fn queued(&self) -> (usize, usize) {
        (
            self.shared.queued.load(Ordering::Relaxed),
            self.shared.high_water.load(Ordering::Relaxed),
        )
    }

    /// Stops the worker and waits for it (up to one command timeout).
    pub fn shutdown(mut self) {
        self.stop();
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }

    fn stop(&mut self) {
        let _ = self.shared.inbox.send(Msg::Stop);
        self.events.close();
    }

    fn command(&self, req: Request, deadline: Option<Duration>) -> Result<Cmd, CoordError> {
        let (state, caps) = {
            let st = self.shared.status.lock().unwrap();
            (st.session, st.caps.clone())
        };
        if let Request::Aps(req) = req {
            // Queued until the session is back.
            if matches!(state, SessionState::Stopped | SessionState::Incompatible) {
                return Err(CoordError::LinkDown);
            }
            let max = caps.map_or(255, |c| usize::from(c.max_asdu).max(1));
            if req.payload.len() > max {
                return Err(CoordError::described(
                    CoordError::Backend {
                        code: Code {
                            backend: hub_coord::BackendKind::Ezsp,
                            value: u16::from(EmberStatus::MESSAGE_TOO_LONG.0),
                            text: None,
                        },
                        kind: Kind::TooLarge,
                    },
                    format!(
                        "APS payload of {} bytes exceeds the NCP's {max}",
                        req.payload.len()
                    ),
                ));
            }
            let timeout = deadline.unwrap_or(self.confirm_timeout);
            return Ok(Cmd::Aps { req, timeout });
        }
        if let Request::Gp(_) = req {
            return Err(CoordError::described(
                CoordError::Unsupported,
                "Green Power is not supported by the EZSP backend yet",
            ));
        }
        // Without a session: queued while the application waits for the
        // next one (bounded by the worker's hold), refused otherwise.
        let starting = matches!(
            state,
            SessionState::Down | SessionState::Negotiating | SessionState::Resuming
        );
        if state != SessionState::Ready && !(self.hold_controls && starting) {
            return Err(CoordError::LinkDown);
        }
        Ok(match req {
            Request::Group {
                endpoint,
                group,
                add,
            } => Cmd::Group {
                endpoint,
                group,
                add,
            },
            Request::RemoveDevice { ieee, nwk } => Cmd::Remove { ieee, nwk },
            Request::PermitJoin { duration_s, target } => Cmd::PermitJoin {
                secs: duration_s,
                target,
            },
            Request::DeviceInventory => Cmd::Inventory,
            Request::Aps(_) | Request::Gp(_) => unreachable!("handled above"),
        })
    }
}

impl Coordinator for EzspCoordinator {
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
        // Only `submit` raises the mark (no `fetch_max`: see the ZRHP pump).
        if n > self.shared.high_water.load(Ordering::Relaxed) {
            self.shared.high_water.store(n, Ordering::Relaxed);
        }
        if self.shared.inbox.send(Msg::Cmd(id, Box::new(cmd))).is_err() {
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
        self.shared.status.lock().unwrap().clone()
    }

    fn permit_join_s(&self) -> u32 {
        self.shared.status.lock().unwrap().permit_join_s
    }
}

impl Drop for EzspCoordinator {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Who waits for a message tag's `messageSentHandler`.
#[derive(Debug)]
enum Owner {
    /// A request of the application.
    Request(RequestId),
    /// The worker itself; `Some` once the outcome came.
    Wait(Option<Result<(), CoordError>>),
    /// A ZDO request of an admin job ([`ZdoWait`]).
    Zdo,
    /// Nobody (a broadcast the worker sent).
    Internal,
    /// A request that timed out, since then (ms).
    Expired(u64),
}

/// A message on the NCP.
#[derive(Debug)]
struct Inflight {
    /// 0: none (a request that ends with the session).
    tag: u16,
    owner: Owner,
    deadline: u64,
    unicast: bool,
    /// An APS ACK was requested (takes an APS unicast slot).
    acked: bool,
}

/// An admin ZDO request waiting for its delivery and response.
struct ZdoWait {
    /// The message tag while the NCP holds the request: 0 once its
    /// `messageSentHandler` came and the tag is free for other sends.
    tag: u16,
    nwk: u16,
    cluster: u16,
    tsn: u8,
    t_ms: u64,
    deadline: u64,
    /// The `messageSentHandler` came and said delivered.
    delivered: bool,
    slot: Arc<Slot>,
}

/// A scan in progress.
struct Scan {
    energy: Vec<EnergyChannel>,
    beacons: Vec<Beacon>,
    total: u8,
    done: Option<Result<(), CoordError>>,
}

#[derive(Debug, Default)]
struct Stats {
    gp_frames: u32,
    devices_left: u32,
    route_records: u32,
    ncp_overflows: u32,
    host_dropped: u32,
    sessions: u32,
}

/// Clamps a permit-join duration: 0xFF ("for ever") is no longer allowed
/// (Zigbee R21+ Mgmt_Permit_Joining_req), 254 s is the longest.
const MAX_PERMIT_S: u8 = 254;

struct Worker<T: Transport, C: Clock> {
    host: EzspHost<T, C>,
    rx: EventReceiver,
    opts: EzspOptions,
    shared: Arc<Shared>,
    events: Arc<EventQueue>,
    endpoints: Vec<EndpointConfig>,
    // Session.
    up: bool,
    lost: bool,
    state: SessionState,
    epoch: u32,
    retry_at: u64,
    /// Until then requests queued while the traffic flag is closed wait for
    /// the application's decision ([`EzspOptions::traffic_hold`]).
    hold_until: u64,
    backoff_ms: u64,
    version: Option<EzspVersion>,
    info: Option<Arc<NcpInfo>>,
    caps: Option<Arc<hub_coord::Capabilities>>,
    config: BTreeMap<u8, u16>,
    max_payload: Option<u8>,
    eui64: Eui64,
    net: NetState,
    network: Option<NetworkParams>,
    ownership: NetworkOwnership,
    key_exportable: Option<bool>,
    /// Callbacks of a session not announced yet.
    held: VecDeque<HostEvent>,
    // Requests.
    fifo: VecDeque<(RequestId, Box<Cmd>)>,
    jobs: VecDeque<Job>,
    inflight: Vec<Inflight>,
    next_tag: u16,
    groups: Vec<Option<MulticastTableEntry>>,
    permit_until: Option<u64>,
    permit_published: u32,
    zdo: Vec<ZdoWait>,
    nwk_cache: VecDeque<(u64, u16)>,
    zdo_tsn: u8,
    out: VecDeque<CoordEvent>,
    tx_power: Option<i8>,
    scan: Option<Scan>,
    channel_changed: bool,
    refresh_pending: bool,
    /// A restore waiting for the session after its NCP reset.
    restoring: Option<Box<backup::Restoring>>,
    stats: Stats,
    // Link report.
    /// The current start's phases; the last announced one's.
    times: SessionTimes,
    start: Option<SessionTimes>,
    failed_starts: u32,
    /// Capacities fitted to the NCP's memory, kept for the boot's later
    /// sessions (by configuration ID).
    fits: BTreeMap<u8, Fit>,
    report_at: u64,
}

/// How a capacity ([`is_capacity`]) fitted the NCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fit {
    asked: u16,
    /// What the NCP took.
    took: u16,
    /// The NCP's own value (`None`: not readable).
    own: Option<u16>,
}

/// Configuration values that size a table in the NCP's RAM: the hub asks
/// for at least so many, keeps a larger value of the NCP's own, and takes
/// the most that fits when the NCP's memory does not hold what it asks for
/// (`ERROR_OUT_OF_MEMORY`).
fn is_capacity(id: ConfigId) -> bool {
    matches!(
        id,
        ConfigId::PACKET_BUFFER_COUNT
            | ConfigId::NEIGHBOR_TABLE_SIZE
            | ConfigId::APS_UNICAST_MESSAGE_COUNT
            | ConfigId::BINDING_TABLE_SIZE
            | ConfigId::ADDRESS_TABLE_SIZE
            | ConfigId::MULTICAST_TABLE_SIZE
            | ConfigId::ROUTE_TABLE_SIZE
            | ConfigId::DISCOVERY_TABLE_SIZE
            | ConfigId::MAX_END_DEVICE_CHILDREN
            | ConfigId::TRUST_CENTER_ADDRESS_CACHE_SIZE
            | ConfigId::SOURCE_ROUTE_TABLE_SIZE
            | ConfigId::KEY_TABLE_SIZE
            | ConfigId::BROADCAST_TABLE_SIZE
            | ConfigId::GP_PROXY_TABLE_SIZE
            | ConfigId::GP_SINK_TABLE_SIZE
    )
}

/// The values tried after `asked` was refused, largest first, down to just
/// above the NCP's own `own`: at most [`FIT_STEPS`]. Descending, so a
/// refused value costs nothing and only the first accepted one takes the
/// NCP's memory.
fn fit_candidates(asked: u16, own: u16) -> Vec<u16> {
    if asked <= own.saturating_add(1) {
        return Vec::new();
    }
    let span = u32::from(asked - own - 1);
    let n = span.min(FIT_STEPS);
    (1..=n)
        .map(|i| (u32::from(asked) - (span * i).div_ceil(n)) as u16)
        .filter(|v| *v > own)
        .collect()
}

/// Values tried below a refused capacity at most.
const FIT_STEPS: u32 = 8;

impl<T: Transport, C: Clock> Worker<T, C> {
    fn new(
        host: EzspHost<T, C>,
        rx: EventReceiver,
        opts: EzspOptions,
        endpoints: Vec<EndpointConfig>,
        shared: Arc<Shared>,
        events: Arc<EventQueue>,
    ) -> Self {
        let backoff_ms = opts.retry_min.as_millis() as u64;
        let tx_power = opts.tx_power;
        Self {
            host,
            rx,
            opts,
            shared,
            events,
            endpoints,
            up: false,
            lost: false,
            state: SessionState::Down,
            epoch: 0,
            retry_at: 0,
            hold_until: 0,
            backoff_ms,
            version: None,
            info: None,
            caps: None,
            config: BTreeMap::new(),
            max_payload: None,
            eui64: Eui64::default(),
            net: NetState::None,
            network: None,
            ownership: NetworkOwnership::Unknown,
            key_exportable: None,
            held: VecDeque::new(),
            fifo: VecDeque::new(),
            jobs: VecDeque::new(),
            inflight: Vec::new(),
            next_tag: 0,
            groups: Vec::new(),
            permit_until: None,
            permit_published: 0,
            zdo: Vec::new(),
            nwk_cache: VecDeque::new(),
            zdo_tsn: 0,
            out: VecDeque::new(),
            tx_power,
            scan: None,
            channel_changed: false,
            refresh_pending: false,
            restoring: None,
            stats: Stats::default(),
            times: SessionTimes::default(),
            start: None,
            failed_starts: 0,
            fits: BTreeMap::new(),
            report_at: 0,
        }
    }

    fn now(&self) -> u64 {
        self.host.now_ms()
    }

    // ------------------------------------------------------------ loop

    fn run(&mut self) {
        self.hold_requests();
        loop {
            if !self.take_inbox() || self.events.is_closed() {
                break;
            }
            if self.lost {
                self.lose_session();
                continue;
            }
            if !self.up {
                self.while_down();
                continue;
            }
            // The application is behind: keep the link, read no callbacks.
            if !self.flush() {
                self.keep_link();
                continue;
            }
            // Callbacks that came before a deadline decide before it.
            self.dispatch(false);
            if self.rx.is_empty() {
                self.expire();
            }
            if self.refresh_pending {
                self.refresh_network();
                continue;
            }
            if let Some(r) = self.restoring.take() {
                self.restore_resume(r);
                continue;
            }
            if let Some(job) = self.jobs.pop_front() {
                self.admin(job);
                continue;
            }
            if self.step_queue() {
                continue;
            }
            self.service();
        }
        self.stopped();
    }

    /// Moves the inbox into the queues; `false` on `Stop`.
    fn take_inbox(&mut self) -> bool {
        for m in self.shared.inbox.take() {
            match m {
                Msg::Cmd(id, cmd) => self.fifo.push_back((id, cmd)),
                Msg::Admin(job) => self.jobs.push_back(job),
                Msg::Stop => return false,
            }
        }
        if self.fifo.is_empty() && self.fifo.capacity() > QUEUE_KEEP {
            self.fifo.shrink_to(QUEUE_KEEP);
        }
        true
    }

    fn stopped(&mut self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        for m in self.shared.inbox.close() {
            if let Msg::Admin(job) = m {
                job.fail(CoordError::LinkDown);
            }
        }
        for job in self.jobs.drain(..) {
            job.fail(CoordError::LinkDown);
        }
        for w in self.zdo.drain(..) {
            w.slot.answer(Answer::Zdo(Err(CoordError::LinkDown)));
        }
        self.restore_lost();
        self.state = SessionState::Stopped;
        self.publish();
        debug!("ezsp: worker stopped");
    }

    /// Without a session: admin jobs fail at once (except behind a
    /// restore's NCP reset), a new session is started when due, time
    /// passes on the host's clock otherwise.
    fn while_down(&mut self) {
        self.report(false);
        if self.restoring.is_none() {
            for job in self.jobs.drain(..) {
                job.fail(CoordError::LinkDown);
            }
        }
        if !self.holding() {
            self.drop_held_requests();
        }
        if self.now() >= self.retry_at {
            self.start_session();
            if !self.up {
                self.restore_lost();
            }
        } else {
            if self.host.pump(self.opts.poll).is_err() {
                // A transport that fails at once (adapter unplugged) must
                // not spin the thread until the retry.
                thread::sleep(self.opts.poll);
            }
            while self.rx.try_recv().is_some() {}
        }
    }

    /// One slice of link service, then the callbacks that came.
    fn service(&mut self) {
        self.report(false);
        if let Err(e) = self.host.pump(self.opts.poll) {
            self.link_error(&e);
        }
        self.dispatch(false);
    }

    /// Link service only: ACKs and retransmissions, callbacks stay in the
    /// host's queue (and its not-ready flow control holds the NCP).
    fn keep_link(&mut self) {
        self.report(false);
        if let Err(e) = self.host.pump(self.opts.poll) {
            self.link_error(&e);
        }
    }

    fn link_error(&mut self, e: &HostError) {
        if is_link_error(e) {
            if !self.lost {
                warn!("ezsp: link lost: {e}");
            }
            self.lost = true;
        }
    }

    /// Hands the host's events to the handlers (to the held buffer before
    /// the session is announced). `all`: even beyond [`OUT_LIMIT`], for
    /// callbacks that came before a response whose outcome follows.
    fn dispatch(&mut self, all: bool) {
        while all || self.out.len() < OUT_LIMIT {
            let Some(ev) = self.rx.try_recv() else { break };
            if self.up {
                self.on_event(ev);
            } else {
                self.hold(ev);
            }
        }
        self.flush();
    }

    // ------------------------------------------------------------ events

    fn emit(&mut self, ev: CoordEvent) {
        self.out.push_back(ev);
        self.flush();
    }

    /// Passes held events to the application; `false` while it is behind.
    fn flush(&mut self) -> bool {
        while let Some(ev) = self.out.pop_front() {
            if let Err(ev) = self.events.try_push(ev) {
                self.out.push_front(ev);
                return false;
            }
        }
        true
    }

    fn done(&mut self, id: RequestId, t: Terminal) {
        if let Terminal::NotDelivered(e) | Terminal::Refused(e) | Terminal::Unknown(e) = &t {
            debug!("ezsp: request {}: {e}", id.0);
        }
        self.emit(CoordEvent::Done { id, t });
    }

    /// [`EzspOptions::link`]: at once with `force`, else about once a
    /// second.
    #[inline(never)]
    fn report(&mut self, force: bool) {
        let Some(link) = &self.opts.link else { return };
        let now = self.host.now_ms();
        if !force && now < self.report_at {
            return;
        }
        self.report_at = now + 1000;
        let mut fitted: Vec<Fitted> = self
            .fits
            .iter()
            .filter(|(_, f)| f.took < f.asked)
            .map(|(id, f)| Fitted {
                id: ConfigId(*id),
                asked: f.asked,
                took: f.took,
            })
            .collect();
        fitted.sort_by_key(|f| f.id);
        let r = LinkReport {
            sessions: self.stats.sessions,
            failed_starts: self.failed_starts,
            up: self.up,
            start: self.start,
            fitted,
            counters: self.host.counters(),
        };
        *link.lock().unwrap_or_else(|p| p.into_inner()) = r;
    }

    fn publish(&self) {
        let now = self.now();
        let permit_join_s = self
            .permit_until
            .map_or(0, |t| t.saturating_sub(now).div_ceil(1000) as u32);
        let up = self.state == SessionState::Ready;
        *self.shared.status.lock().unwrap() = CoordStatus {
            session: self.state,
            epoch: up.then_some(self.epoch),
            ncp: self.info.clone().filter(|_| up),
            caps: self.caps.clone().filter(|_| up),
            network_state: if up { self.net } else { NetState::None },
            network: self.network.filter(|_| up),
            ownership: if up {
                self.ownership.clone()
            } else {
                NetworkOwnership::Unknown
            },
            permit_join_s,
        };
    }

    fn on_event(&mut self, ev: HostEvent) {
        match ev {
            HostEvent::Callback(cb) => self.on_callback(cb),
            HostEvent::NcpOverflow => {
                self.stats.ncp_overflows += 1;
                warn!("ezsp: the NCP ran out of memory (callbacks may be lost)");
            }
            HostEvent::Dropped(n) => {
                self.stats.host_dropped += n;
                self.emit(CoordEvent::Dropped { events: n });
            }
            HostEvent::LinkLost(r) => {
                warn!("ezsp: link lost: {r:?}");
                self.lost = true;
            }
        }
    }

    /// Before the session is announced: stack statuses are noted (the
    /// announcement reports the network), everything else waits.
    fn hold(&mut self, ev: HostEvent) {
        match ev {
            HostEvent::Callback(Callback::StackStatus { status }) => match status.kind() {
                StatusKind::NetworkUp => self.net = NetState::Up,
                StatusKind::NetworkDown => self.net = NetState::FormedDown,
                _ => {}
            },
            HostEvent::LinkLost(_) => self.lost = true,
            ev => self.held.push_back(ev),
        }
    }

    fn on_callback(&mut self, cb: Callback) {
        match cb {
            Callback::StackStatus { status } => self.on_stack_status(status),
            Callback::MessageSent { status, tag, .. } => self.on_sent(tag, status),
            Callback::IncomingMessage {
                kind,
                aps,
                info,
                message,
            } => self.on_incoming(kind, &aps, &info, message),
            Callback::TrustCenterJoin {
                new_node_id,
                new_node_eui64,
                status,
                policy_decision,
                parent_of_new_node_id,
            } => {
                let ieee = new_node_eui64.to_u64();
                if status == DeviceUpdate::DEVICE_LEFT {
                    self.stats.devices_left += 1;
                    self.nwk_cache.retain(|(i, _)| *i != ieee);
                    info!("ezsp: {ieee:016X} left (0x{new_node_id:04X})");
                    // The trust center's update does not say whether the
                    // device rejoins.
                    self.emit(CoordEvent::DeviceLeft {
                        ieee,
                        nwk: new_node_id,
                        rejoin: None,
                    });
                } else if policy_decision == JoinDecision::DENY_JOIN {
                    info!("ezsp: join of {ieee:016X} denied");
                } else {
                    debug!(
                        "ezsp: {ieee:016X} joined as 0x{new_node_id:04X} via 0x{parent_of_new_node_id:04X} ({status:?})"
                    );
                    self.learn(ieee, new_node_id);
                    self.emit(CoordEvent::DeviceJoined {
                        ieee,
                        nwk: new_node_id,
                    });
                }
            }
            Callback::ChildJoin {
                joining,
                child_id,
                child_eui64,
                ..
            } => debug!(
                "ezsp: child {child_eui64} 0x{child_id:04X} {}",
                if joining { "joined" } else { "left" }
            ),
            Callback::EnergyScanResult { channel, max_rssi } => {
                if let Some(scan) = &mut self.scan {
                    scan.energy.push(EnergyChannel {
                        channel,
                        max_rssi_dbm: max_rssi,
                        energy: energy_of_rssi(max_rssi),
                    });
                    let (done, total) = (scan.energy.len() as u8, scan.total);
                    self.emit(CoordEvent::ScanProgress {
                        scan: 0,
                        done,
                        total,
                    });
                }
            }
            Callback::NetworkFound {
                network,
                last_hop_lqi,
                last_hop_rssi,
            } => {
                if let Some(scan) = &mut self.scan {
                    scan.beacons.push(Beacon {
                        channel: network.channel,
                        pan_id: network.pan_id,
                        ext_pan_id: network.extended_pan_id,
                        // networkFoundHandler does not carry the sender.
                        src: 0xFFFF,
                        lqi: last_hop_lqi,
                        rssi: last_hop_rssi,
                        permit_join: network.allowing_join,
                        stack_profile: network.stack_profile,
                        update_id: network.nwk_update_id,
                    });
                }
            }
            Callback::ScanComplete { channel, status } => {
                if let Some(scan) = &mut self.scan {
                    scan.done = Some(if status.is_success() {
                        Ok(())
                    } else {
                        Err(CoordError::described(
                            ezsp_error(id::SCAN_COMPLETE_HANDLER, status),
                            format!("scan failed on channel {channel}: {status:?}"),
                        ))
                    });
                }
            }
            Callback::IncomingRouteRecord { source, relays, .. } => {
                // The NCP keeps the source route itself
                // (SOURCE_ROUTE_TABLE_SIZE); nothing to store here.
                self.stats.route_records += 1;
                debug!("ezsp: route record from 0x{source:04X} via {relays:04X?}");
            }
            Callback::Unknown { frame_id, params } => match frame_id {
                id::GPEP_INCOMING_MESSAGE_HANDLER | id::D_GP_SENT_HANDLER => {
                    self.gp_callback(frame_id, &params)
                }
                id::ID_CONFLICT_HANDLER => warn!("ezsp: short address conflict {params:02X?}"),
                _ => debug!("ezsp: callback 0x{frame_id:04X} ignored"),
            },
        }
    }

    /// The Green Power path (GP follow-up): `gpepIncomingMessageHandler`
    /// will become [`CoordEvent::Gp`] frames and `dGpSentHandler` the
    /// outcome of GP requests. Counted only for now.
    fn gp_callback(&mut self, frame_id: u16, _params: &[u8]) {
        if self.stats.gp_frames == 0 {
            info!("ezsp: Green Power callback 0x{frame_id:04X} ignored (not supported yet)");
        }
        self.stats.gp_frames += 1;
    }

    fn on_stack_status(&mut self, status: Status) {
        match status.kind() {
            StatusKind::NetworkUp => {
                self.net = NetState::Up;
                self.refresh_pending = true;
                self.emit(CoordEvent::NetworkState(NetChange::Up));
            }
            StatusKind::NetworkDown => {
                if self.net == NetState::Up {
                    self.net = NetState::FormedDown;
                }
                self.emit(CoordEvent::NetworkState(NetChange::Down));
            }
            StatusKind::ChannelChanged => {
                self.channel_changed = true;
                self.refresh_pending = true;
            }
            _ => debug!("ezsp: stack status {status:?}"),
        }
        self.publish();
    }

    fn on_sent(&mut self, tag: u16, status: Status) {
        let Some(i) = self.inflight.iter().position(|f| f.tag == tag && tag != 0) else {
            debug!("ezsp: messageSentHandler for unknown tag {tag}");
            return;
        };
        let f = self.inflight.remove(i);
        match f.owner {
            Owner::Wait(_) => {
                let r = if status.is_success() {
                    Ok(())
                } else {
                    Err(match sent_terminal(status, true, true) {
                        Terminal::NotDelivered(e) => e,
                        _ => ezsp_error(id::MESSAGE_SENT_HANDLER, status),
                    })
                };
                self.inflight.insert(
                    i,
                    Inflight {
                        owner: Owner::Wait(Some(r)),
                        ..f
                    },
                );
            }
            Owner::Zdo => {
                let Some(w) = self.zdo.iter().position(|w| w.tag == tag) else {
                    return;
                };
                if status.is_success() {
                    let now = self.now();
                    let w = &mut self.zdo[w];
                    w.delivered = true;
                    // The NCP is done with the tag: another send may take
                    // it while the response is awaited.
                    w.tag = 0;
                    w.deadline = now + w.t_ms;
                } else {
                    let w = self.zdo.remove(w);
                    let e = match sent_terminal(status, true, true) {
                        Terminal::NotDelivered(e) => e,
                        _ => ezsp_error(id::MESSAGE_SENT_HANDLER, status),
                    };
                    w.slot.answer(Answer::Zdo(Err(e)));
                }
            }
            Owner::Request(id) => {
                let t = sent_terminal(status, f.unicast, f.acked);
                self.done(id, t);
            }
            Owner::Internal | Owner::Expired(_) => {}
        }
    }

    fn on_incoming(
        &mut self,
        kind: IncomingMessageType,
        aps: &ApsFrame,
        info: &RxPacketInfo,
        message: Vec<u8>,
    ) {
        if aps.profile_id == 0 {
            if aps.cluster_id == ZDO_DEVICE_ANNCE && message.len() >= 11 {
                let nwk = u16::from_le_bytes([message[1], message[2]]);
                let ieee = u64::from_le_bytes(message[3..11].try_into().unwrap());
                self.learn(ieee, nwk);
            }
            if let Some(i) = self.zdo.iter().position(|w| {
                w.nwk == info.sender
                    && aps.cluster_id == w.cluster | 0x8000
                    && message.first() == Some(&w.tsn)
            }) {
                let w = self.zdo.remove(i);
                let now = self.now();
                self.release_zdo_tag(w.tag, now);
                w.slot.answer(Answer::Zdo(Ok(message[1..].to_vec())));
            }
        }
        if let Some(ind) = indication(kind, aps, info, &message) {
            if let Some(ieee) = ind.src_ieee {
                self.learn(ieee, ind.src_nwk);
            }
            self.emit(CoordEvent::Indication(ind));
        }
    }

    /// A ZDO wait ended before the `messageSentHandler` of its request: the
    /// tag stays reserved for that late callback (as a timed-out request's).
    /// Only the wait's own entry is touched; tag 0 means it was released.
    fn release_zdo_tag(&mut self, tag: u16, now: u64) {
        if tag == 0 {
            return;
        }
        if let Some(f) = self
            .inflight
            .iter_mut()
            .find(|f| f.tag == tag && matches!(f.owner, Owner::Zdo))
        {
            f.owner = Owner::Expired(now);
        }
    }

    /// Remembers the short address of `ieee` (joins, announcements).
    fn learn(&mut self, ieee: u64, nwk: u16) {
        self.nwk_cache.retain(|(i, _)| *i != ieee);
        if self.nwk_cache.len() >= NWK_CACHE {
            self.nwk_cache.pop_front();
        }
        self.nwk_cache.push_back((ieee, nwk));
    }

    // ------------------------------------------------------------ session

    fn start_session(&mut self) {
        // An incompatible NCP stays reported as such while it is retried.
        if self.state != SessionState::Incompatible {
            self.state = SessionState::Negotiating;
            self.publish();
        }
        self.held.clear();
        while self.rx.try_recv().is_some() {}
        self.lost = false;
        self.net = NetState::None;
        self.times = SessionTimes::default();
        let t0 = self.now();
        let info = match self.host.start() {
            Ok(info) => info,
            Err(HostError::Version(e)) => {
                warn!("ezsp: {e}");
                self.incompatible();
                return;
            }
            Err(e) => {
                warn!("ezsp: no session: {e}");
                self.retry_later();
                return;
            }
        };
        let st = self.host.start_times();
        self.times.reset_ms = st.reset_ms;
        self.times.version_ms = st.version_ms;
        match self.configure(info) {
            Ok(()) if !self.lost => {
                self.times.total_ms = self.now().saturating_sub(t0) as u32;
                self.start = Some(self.times);
                self.announce();
            }
            Ok(()) => self.retry_later(),
            Err(e) => {
                warn!("ezsp: session setup failed: {e}");
                self.retry_later();
            }
        }
    }

    fn retry_later(&mut self) {
        self.failed_starts += 1;
        if self.state != SessionState::Incompatible {
            self.state = SessionState::Down;
        }
        self.lost = false;
        self.retry_at = self.now() + self.backoff_ms;
        let max = self.opts.retry_max.as_millis() as u64;
        self.backoff_ms = (self.backoff_ms * 2).clamp(1, max.max(1));
        self.publish();
        self.report(true);
    }

    /// The NCP speaks no supported EZSP version: queued requests can never
    /// go out. Tried again slowly (the NCP may be updated).
    fn incompatible(&mut self) {
        if self.state != SessionState::Incompatible {
            self.emit(CoordEvent::Session(Box::new(SessionEvent::Incompatible)));
        }
        self.state = SessionState::Incompatible;
        for (id, _) in std::mem::take(&mut self.fifo) {
            let e =
                CoordError::described(CoordError::LinkDown, "NCP speaks no supported EZSP version");
            self.end_queued(id, Terminal::Unknown(e));
        }
        self.retry_at = self.now() + self.opts.retry_max.as_millis() as u64;
        self.publish();
    }

    /// Configuration, policies, endpoints, `networkInit` and what the
    /// session needs again after an NCP reset.
    fn configure(&mut self, info: HostInfo) -> Result<(), HostError> {
        let v = info.ezsp_version;
        self.version = Some(v);
        // Nothing of the previous session's NCP carries over.
        self.max_payload = None;
        self.key_exportable = None;
        self.info = None;
        self.caps = None;
        let mut t = self.now();
        let mut lap = |w: &Self| {
            let now = w.now();
            let d = now.saturating_sub(t) as u32;
            t = now;
            d
        };
        let version_info = match self.try_call(&Command::GetValue {
            id: ValueId::VERSION_INFO,
        })? {
            Some(Response::GetValue { status, value })
                if status.is_success() && value.len() == 7 =>
            {
                Some(value)
            }
            _ => None,
        };
        self.set_config()?;
        self.config.clear();
        for id in CAPS_CONFIG {
            match self.host.get_configuration_value(*id) {
                Ok(value) => {
                    self.config.insert(id.0, value);
                }
                Err(HostError::Status { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        self.times.config_ms = lap(self);
        for (policy, decision) in self.opts.policies.clone() {
            match self.set_policy(policy, decision) {
                Ok(()) | Err(HostError::Status { .. }) => {}
                Err(e) if is_link_error(&e) => return Err(e),
                Err(e) => warn!("ezsp: policy {policy:?}: {e}"),
            }
        }
        for e in self.endpoints.clone() {
            let ep = Endpoint {
                endpoint: e.ep,
                profile_id: e.profile,
                device_id: e.device_id,
                app_flags: e.version & 0x0F,
                input_clusters: e.in_clusters,
                output_clusters: e.out_clusters,
            };
            match self.host.add_endpoint(ep) {
                Ok(()) => {}
                Err(HostError::Status { status, .. }) => {
                    warn!("ezsp: addEndpoint {} refused: {status:?}", e.ep)
                }
                Err(e) => return Err(e),
            }
        }
        self.times.setup_ms = lap(self);
        let init = self.host.network_init(NetworkInitBitmask::NO_OPTIONS)?;
        self.network = None;
        // Only NOT_JOINED says the NCP stores no network: forming is
        // allowed then alone. Any other failure may hide a stored network
        // (NcpNv): unknown, and not overwritten.
        if init.kind() == StatusKind::NotJoined {
            self.net = NetState::None;
            self.ownership = NetworkOwnership::NoNetwork;
        } else {
            self.net = NetState::FormedDown;
            self.ownership = NetworkOwnership::Unknown;
            if init.is_success() {
                let deadline = self.now() + self.opts.network_up_timeout.as_millis() as u64;
                self.wait(deadline, |w| w.net == NetState::Up);
            } else {
                warn!("ezsp: networkInit: {init:?}; the NCP's network is unknown");
            }
        }
        self.times.network_ms = lap(self);
        self.eui64 = self.host.get_eui64()?;
        let board = self.mfg_string(MfgTokenId::BOARD_NAME)?;
        let product = self.mfg_string(MfgTokenId::STRING)?;
        self.groups.resize(
            usize::from(self.cfg(ConfigId::MULTICAST_TABLE_SIZE).unwrap_or(8)),
            None,
        );
        self.replay_groups()?;
        if self.net == NetState::Up {
            self.after_network_up()?;
        }
        self.info = Some(Arc::new(NcpInfo {
            fw: firmware_name(version_info.as_deref(), &info),
            protocol: (v.get(), 0),
            build_id: [0; 8],
            chip: 0,
            ieee: Some(self.eui64.0),
            flash_size: 0,
            board,
            product,
            baud: self.opts.baud,
            flow_control: self.opts.flow_control,
        }));
        self.caps = Some(Arc::new(capabilities(&self.config, self.max_payload)));
        self.times.rest_ms = lap(self);
        Ok(())
    }

    /// [`EzspOptions::config`] in order: capacities fitted to the NCP's
    /// memory ([`Self::fit`]), the other values as they are.
    #[inline(never)]
    fn set_config(&mut self) -> Result<(), HostError> {
        for (id, value) in self.opts.config.clone() {
            if is_capacity(id) {
                self.fit(id, value)?;
                continue;
            }
            match self.host.set_configuration_value(id, value) {
                Ok(()) => {}
                Err(HostError::Status { status, .. }) => {
                    warn!("ezsp: configuration {id:?} = {value} refused: {status:?}")
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// `setConfigurationValue` that the NCP may refuse: `false` then.
    fn try_set(&mut self, id: ConfigId, value: u16) -> Result<bool, HostError> {
        match self.host.set_configuration_value(id, value) {
            Ok(()) => Ok(true),
            Err(HostError::Status { status, .. }) => {
                debug!("ezsp: configuration {id:?} = {value}: {status:?}");
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// A capacity: at least the NCP's own value (never lowered: SONOFF's
    /// image has 26 neighbors, the hub asks for 16), `asked` if the NCP
    /// takes it, else the largest of [`fit_candidates`] it accepts. The
    /// outcome is kept: after an NCP reset the next session sets it with
    /// one command (and fits again if that is refused). Logged once at info
    /// when less than `asked` fitted.
    fn fit(&mut self, id: ConfigId, asked: u16) -> Result<(), HostError> {
        if let Some(f) = self.fits.get(&id.0).copied().filter(|f| f.asked == asked) {
            // Nothing was set when the NCP's own value stayed.
            if f.own == Some(f.took) || self.try_set(id, f.took)? {
                return Ok(());
            }
            self.fits.remove(&id.0);
        }
        let own = match self.host.get_configuration_value(id) {
            Ok(v) => Some(v),
            Err(HostError::Status { .. }) => None,
            Err(e) => return Err(e),
        };
        let took = match own {
            Some(o) if o >= asked => o,
            _ if self.try_set(id, asked)? => asked,
            _ => {
                let mut took = own.unwrap_or(0);
                for v in fit_candidates(asked, took) {
                    if self.try_set(id, v)? {
                        took = v;
                        break;
                    }
                }
                match own {
                    Some(o) if took == o => info!(
                        "ezsp: {id:?}: {asked} does not fit the NCP's memory; its own {o} kept"
                    ),
                    _ => info!("ezsp: {id:?}: {asked} does not fit the NCP's memory; {took} set"),
                }
                took
            }
        };
        self.fits.insert(id.0, Fit { asked, took, own });
        Ok(())
    }

    /// A call whose failure the session survives: `None` (logged) unless
    /// the link is gone.
    fn try_call(&mut self, cmd: &Command) -> Result<Option<Response>, HostError> {
        match self.host.call(cmd) {
            Ok(r) => Ok(Some(r)),
            Err(e) if is_link_error(&e) => Err(e),
            Err(e) => {
                debug!("ezsp: frame 0x{:04X}: {e}", cmd.frame_id());
                Ok(None)
            }
        }
    }

    fn cfg(&self, id: ConfigId) -> Option<u16> {
        self.config.get(&id.0).copied()
    }

    fn set_policy(&mut self, policy: PolicyId, decision: u8) -> Result<(), HostError> {
        match self.host.call(&Command::SetPolicy { policy, decision })? {
            Response::SetPolicy { status } if status.is_success() => Ok(()),
            Response::SetPolicy { status } => {
                warn!("ezsp: policy {policy:?} = 0x{decision:02X} refused: {status:?}");
                Err(HostError::Status {
                    frame_id: id::SET_POLICY,
                    status,
                })
            }
            other => Err(HostError::UnexpectedResponse(other)),
        }
    }

    /// A manufacturing string token, without its padding.
    fn mfg_string(&mut self, token: MfgTokenId) -> Result<String, HostError> {
        Ok(match self.try_call(&Command::GetMfgToken { token })? {
            Some(Response::GetMfgToken { data }) => {
                let end = data
                    .iter()
                    .position(|b| *b == 0 || *b == 0xFF)
                    .unwrap_or(data.len());
                String::from_utf8_lossy(&data[..end]).trim().to_string()
            }
            _ => String::new(),
        })
    }

    /// The multicast table is RAM on the NCP: written again in every
    /// session.
    fn replay_groups(&mut self) -> Result<(), HostError> {
        for (index, entry) in self.groups.clone().into_iter().enumerate() {
            let Some(entry) = entry else { continue };
            let r = self.host.call(&Command::SetMulticastTableEntry {
                index: index as u8,
                entry,
            })?;
            if r.status().is_some_and(|s| !s.is_success()) {
                warn!("ezsp: multicast entry {index} not restored: {r:?}");
            }
        }
        Ok(())
    }

    /// The network as the NCP runs it, the payload limit, the trust
    /// center's hardening, the concentrator and the operator's TX power.
    fn after_network_up(&mut self) -> Result<(), HostError> {
        self.read_network()?;
        self.max_payload = Some(self.host.maximum_payload_length()?);
        // Not kept across NCP resets (`network-creator-security` sets it
        // on every NETWORK_UP): the trust center ignores NWK leave
        // requests.
        if let Some(Response::GetValue { status, value }) = self.try_call(&Command::GetValue {
            id: ValueId::EXTENDED_SECURITY_BITMASK,
        })? && status.is_success()
            && value.len() == 2
        {
            let ext = u16::from_le_bytes([value[0], value[1]])
                | extended_security_bitmask::NWK_LEAVE_REQUEST_NOT_ALLOWED;
            let r = self.try_call(&Command::SetValue {
                id: ValueId::EXTENDED_SECURITY_BITMASK,
                value: ext.to_le_bytes().to_vec(),
            })?;
            if r.as_ref()
                .and_then(Response::status)
                .is_none_or(|s| !s.is_success())
            {
                warn!("ezsp: extended security bitmask not set: {r:?}");
            }
        }
        if self.opts.concentrator {
            let r = self.try_call(&Command::SetConcentrator {
                on: true,
                concentrator_type: concentrator::HIGH_RAM,
                min_time: 10,
                max_time: 60,
                route_error_threshold: 3,
                delivery_failure_threshold: 1,
                max_hops: 0,
            })?;
            if r.as_ref()
                .and_then(Response::status)
                .is_none_or(|s| !s.is_success())
            {
                warn!("ezsp: setConcentrator: {r:?}");
            }
        }
        if let Some(dbm) = self.tx_power {
            let r = self.try_call(&Command::SetRadioPower { power: dbm })?;
            if r.as_ref()
                .and_then(Response::status)
                .is_some_and(|s| s.is_success())
                && let Some(n) = &mut self.network
            {
                n.tx_power = Some(dbm);
            }
        }
        Ok(())
    }

    /// Network parameters, key sequence and key ID: the NCP's
    /// [`NetworkIdentity`]. The key ID needs the network key, which the
    /// security manager may refuse to export (then `None`: EPID-only
    /// matching, design §8.5).
    fn read_network(&mut self) -> Result<(), HostError> {
        let (_, p) = self.host.get_network_parameters()?;
        let key_seq = match self.try_call(&Command::GetNetworkKeyInfo)? {
            Some(Response::GetNetworkKeyInfo { status, info }) if status.is_success() => {
                info.network_key_sequence_number
            }
            _ => 0,
        };
        let key_id = match self.try_call(&Command::ExportKey(SecManContext::network_key()))? {
            Some(Response::ExportKey {
                status, mut key, ..
            }) if status.is_success() => {
                let id = NetworkIdentity::key_id_for(&key);
                key.fill(0);
                Some(id)
            }
            r => {
                debug!(
                    "ezsp: network key not exportable: {:?}",
                    r.as_ref().and_then(Response::status)
                );
                None
            }
        };
        self.key_exportable = Some(key_id.is_some());
        let tx_power = self.tx_power.or(Some(p.radio_tx_power));
        self.network = Some(NetworkParams {
            pan_id: p.pan_id,
            ext_pan_id: p.extended_pan_id,
            channel: p.radio_channel,
            nwk_update_id: p.nwk_update_id,
            key_seq,
            tx_power,
        });
        // NcpNv: the backend reports the network it found; the hub decides
        // whether it is its own (design §2.5, §2.7).
        self.ownership = NetworkOwnership::Observed(NetworkIdentity {
            ext_pan_id: p.extended_pan_id,
            key_id,
            pan_id: p.pan_id,
            channel: p.radio_channel,
            key_seq,
            coordinator_ieee: self.eui64.0,
        });
        Ok(())
    }

    /// A NETWORK_UP or channel change in the session: the network is read
    /// again and the ownership reported.
    fn refresh_network(&mut self) {
        self.refresh_pending = false;
        if self.net != NetState::Up {
            return;
        }
        match self.after_network_up() {
            Ok(()) => {
                self.caps = Some(Arc::new(capabilities(&self.config, self.max_payload)));
                self.publish();
                let o = self.ownership.clone();
                self.emit(CoordEvent::Ownership(Box::new(o)));
            }
            Err(e) => {
                warn!("ezsp: reading the network: {e}");
                self.link_error(&e);
            }
        }
    }

    /// The session is ready: ownership, `Up`, the network state, then the
    /// callbacks that came meanwhile.
    fn announce(&mut self) {
        self.epoch = self.epoch.wrapping_add(1).max(1);
        // The application decides about this session's network first.
        if let Some(t) = &self.opts.traffic {
            t.store(super::traffic_closed(self.epoch), Ordering::SeqCst);
        }
        self.up = true;
        self.state = SessionState::Ready;
        self.stats.sessions += 1;
        self.backoff_ms = self.opts.retry_min.as_millis() as u64;
        self.publish();
        self.report(true);
        let (Some(info), Some(caps)) = (self.info.clone(), self.caps.clone()) else {
            unreachable!("configure sets both");
        };
        info!(
            "ezsp: session {} up ({}, EZSP {}, network {:?}) in {}",
            self.epoch, info.fw, info.protocol.0, self.net, self.times
        );
        let o = self.ownership.clone();
        self.emit(CoordEvent::Ownership(Box::new(o)));
        self.emit(CoordEvent::Session(Box::new(SessionEvent::Up {
            epoch: self.epoch,
            info,
            caps,
        })));
        if self.net == NetState::Up {
            self.emit(CoordEvent::NetworkState(NetChange::Up));
        }
        for ev in std::mem::take(&mut self.held) {
            self.on_event(ev);
        }
    }

    /// The session ended (NCP reset, ERROR, link failure).
    fn lose_session(&mut self) {
        self.lost = false;
        self.close_traffic();
        let was_up = self.up;
        if was_up {
            // What the NCP sent before the failure goes first.
            while let Some(ev) = self.rx.try_recv() {
                if !matches!(ev, HostEvent::LinkLost(_)) {
                    self.on_event(ev);
                }
            }
        }
        self.up = false;
        self.state = SessionState::Down;
        self.lost = false;
        self.net = NetState::None;
        if was_up {
            self.emit(CoordEvent::Session(Box::new(SessionEvent::Down)));
        }
        let lost = || CoordError::described(CoordError::DeliveryUnknown, "NCP reset or link lost");
        for f in std::mem::take(&mut self.inflight) {
            if let Owner::Request(id) = f.owner {
                self.done(id, Terminal::Unknown(lost()));
            }
        }
        for w in std::mem::take(&mut self.zdo) {
            w.slot.answer(Answer::Zdo(Err(lost())));
        }
        if self.permit_until.take().is_some() && was_up {
            self.emit(CoordEvent::PermitJoin { remaining_s: 0 });
        }
        self.scan = None;
        self.refresh_pending = false;
        self.retry_at = self.now();
        // Queued for this session's network: never sent in the next one.
        self.drop_held_requests();
        self.hold_requests();
        self.publish();
        self.report(true);
    }

    /// Requests queued from now on wait for the decision about the next
    /// session for at most [`EzspOptions::traffic_hold`].
    fn hold_requests(&mut self) {
        self.hold_until = self
            .now()
            .saturating_add(self.opts.traffic_hold.as_millis() as u64);
    }

    /// Requests queued while the traffic flag is closed still wait.
    pub(super) fn holding(&self) -> bool {
        self.now() < self.hold_until
    }

    /// [`EzspOptions::traffic`] at the end of a session: no epoch, closed.
    fn close_traffic(&self) {
        if let Some(t) = &self.opts.traffic {
            t.store(0, Ordering::SeqCst);
        }
    }

    /// Requests may go out ([`EzspOptions::traffic`]).
    pub(super) fn traffic_open(&self) -> bool {
        self.opts.traffic.as_ref().is_none_or(|t| {
            self.up && t.load(Ordering::SeqCst) == super::traffic_closed(self.epoch) | 1
        })
    }

    /// While [`EzspOptions::traffic`] is closed, queued requests end
    /// `Refused(LinkDown)`: they were meant for a network that may not be
    /// the one of the next session, or the application decided against
    /// this session's network (or did not decide within the hold).
    pub(super) fn drop_held_requests(&mut self) {
        if self.traffic_open() {
            return;
        }
        for (id, _) in std::mem::take(&mut self.fifo) {
            let e = CoordError::described(
                CoordError::LinkDown,
                "not sent: the NCP's network is not verified as the application's",
            );
            self.end_queued(id, Terminal::Refused(e));
        }
    }

    /// Runs the link until `cond` holds, the deadline passes or the link
    /// is lost; callbacks are handled meanwhile.
    fn wait(&mut self, deadline: u64, cond: impl Fn(&Self) -> bool) -> bool {
        loop {
            if cond(self) {
                return true;
            }
            if self.lost || self.now() >= deadline {
                return false;
            }
            if self.flush() {
                self.service();
            } else {
                self.keep_link();
            }
            if self.up && self.rx.is_empty() {
                self.expire();
            }
        }
    }
}

/// "EmberZNet 7.4.5.0 build 397" from the `VERSION_INFO` value (build,
/// major, minor, patch, special, type: Gecko SDK 4.4.6 `ezsp.c`
/// `ezspGetVersionStruct`), else the stack version of the version
/// response.
fn firmware_name(version_info: Option<&[u8]>, info: &HostInfo) -> String {
    match version_info {
        Some(&[b0, b1, major, minor, patch, special, _]) => format!(
            "EmberZNet {major}.{minor}.{patch}.{special} build {}",
            u16::from_le_bytes([b0, b1])
        ),
        _ => format!("EmberZNet {}", info.stack_version),
    }
}
