//! The link driver: one thread that owns the ZRHP [`Link`], the transmit
//! half of the transport and every outstanding request.
//!
//! ```text
//!  reader thread ── Rx bytes ──┐
//!  Client::call ── Call ───────┤ mpsc ──> driver thread ──> Link ──> PortTx
//!  Client::respond, resume_done┘            │
//!                                           └──> DriverEvent (sessions, NCP events,
//!                                                NCP requests) to the lifecycle
//! ```
//!
//! Session handling (spec 5.5, 5.11, 6.5, 12.1):
//!
//! - after every `ESTABLISHED` the driver waits for `EVT_NCP_READY`, sends
//!   `SYS_NEGOTIATE` and `SYS_GET_INFO` itself, then reports
//!   [`DriverEvent::SessionUp`];
//! - requests are **gated**: until the lifecycle calls
//!   [`Client::resume_done`] only requests marked
//!   [`CallOpts::during_resume`] go out; everything else waits (bounded by
//!   the resume budget), which hides NCP reboots from callers;
//! - when a session drops, requests already transmitted are "in doubt":
//!   if the new session has the same NCP cookie (`PEER_RESYNC`) they are
//!   re-sent with the same TID and the `RETRY` flag (the NCP's retry cache
//!   answers them without executing twice); if the NCP restarted they fail
//!   with [`Error::LinkReset`] (delivery unknown);
//! - while no session comes up, the driver cycles through
//!   [`DriverConfig::lines`] (921 600 with RTS/CTS, then the 460 800
//!   fallback).

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use log::{debug, info, trace, warn};
use zigbee_zrhp::codec::{Bytes8, Reader, Wire};
use zigbee_zrhp::frame::{ResetReason, TX_BUFFER_LEN};
use zigbee_zrhp::link::{
    Link, LinkConfig, LinkCounters, LinkEvent, LinkState, PeerChange, SessionInfo, TxFailure,
};
use zigbee_zrhp::msg::system::{EvtNcpReady, SysGetInfoRsp, SysNegotiateReq, SysNegotiateRsp};
use zigbee_zrhp::msg::{self, Batch, BatchWriter, Kind, MessageDef, TidAllocator, ids};
use zigbee_zrhp::queue::TxSource;
use zigbee_zrhp::types::{Endpoint, Status};

use crate::transport::{Line, PortRx, PortTx};

/// Embedded-host link: 8 TX and 8 RX slots (spec 3.3/3.4, ≈ 11.9 KB).
pub type HostLink = Link<8, 8>;

/// LOG endpoint credit (frames) the hub grants the NCP once a session is
/// up ([`DriverConfig::log_credits`]). The embedded profile grants none at
/// the handshake (spec 3.3), and an NCP keeps its log, boot records
/// included, until a host grants some; then they arrive and go into the
/// hub's log (`lifecycle`, NCP log). Returned as each record is taken
/// (`auto_credit`).
pub const LOG_CREDITS: u8 = 2;

/// Compile-time image of a fresh embedded-profile link (cookie 1; the
/// driver gives it its real cookie with `reopen`).
static TEMPLATE: HostLink = HostLink::new(LinkConfig::embedded_host(), 1);

/// A heap-allocated link. `Link::new` builds ~12 KB by value and needs
/// 24–32 KiB of stack in unoptimised builds, so the default profile is
/// copied from a static straight into the allocation instead. Any other
/// profile is built out of line ([`built_link`]): inlined here, its frame
/// (16 240 B on the classic ESP32) was reserved by every caller, the
/// driver thread's entry included, whichever branch ran.
pub fn boxed_link(cfg: LinkConfig, cookie: u32) -> Box<HostLink> {
    if cfg != LinkConfig::embedded_host() {
        return built_link(cfg, cookie);
    }
    let mut slot = Box::<HostLink>::new_uninit();
    // SAFETY: `Link` is plain data (no heap, no `Drop`: the crate is
    // `no_std` without `alloc`), so a bitwise copy of the never-used
    // template is a valid fresh value, and the copy initialises the slot.
    unsafe {
        core::ptr::copy_nonoverlapping(&TEMPLATE, slot.as_mut_ptr(), 1);
        slot.assume_init()
    }
}

/// A link of another profile than the embedded host's (desktop tools):
/// `Link::new` by value, with its ~12 KB frame, kept out of the callers.
#[cold]
#[inline(never)]
fn built_link(cfg: LinkConfig, cookie: u32) -> Box<HostLink> {
    Box::new(HostLink::new(cfg, cookie))
}

/// Stack of the driver thread ([`DriverConfig::stack_size`]), as the S3
/// hub has always run it. Measured on the classic ESP32 (`-Z
/// emit-stack-sizes`, zrh-donglem): the thread's entry 720 B, `run`
/// 1 328 B, `drain_link` 848 B (the link's handling inlined), besides the
/// channel and logging frames; before [`built_link`] was out of line the
/// entry alone took 16 240 B. Callers keep this size (the NCP probe
/// overflowed a 16 KiB stack).
pub const DRIVER_STACK: usize = 32 * 1024;

/// Driver settings.
#[derive(Debug, Clone)]
pub struct DriverConfig {
    /// Link parameters (default: [`LinkConfig::embedded_host`]).
    pub link: LinkConfig,
    /// Host name sent in `SYS_NEGOTIATE`.
    pub host_name: String,
    /// `host_caps` of `SYS_NEGOTIATE` (spec 8.8).
    pub host_caps: u64,
    /// Host line settings tried in order while no session comes up.
    pub lines: Vec<Line>,
    /// Time on one line without a session before trying the next.
    pub hunt_after: Duration,
    /// Wait this long for `EVT_NCP_READY` after `ESTABLISHED`, then
    /// negotiate anyway.
    pub ready_timeout: Duration,
    /// Requests waiting for a session/resume fail after this long.
    pub resume_budget: Duration,
    /// Open the request gate right after negotiation (no lifecycle).
    pub auto_ready: bool,
    /// Driver thread stack (the link itself is on the heap).
    pub stack_size: usize,
    /// LOG endpoint credit granted in every session (default
    /// [`LOG_CREDITS`]; 0: the NCP keeps its log, as for a probe that
    /// must leave the boot records to the hub).
    pub log_credits: u8,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            link: LinkConfig::embedded_host(),
            host_name: "zigbee-rs-hub".into(),
            host_caps: msg::system::host_caps::FC_CHECKPOINTS | msg::system::host_caps::RETRY_CACHE,
            lines: vec![Line::BR_DEFAULT, Line::BR_FALLBACK],
            hunt_after: Duration::from_secs(4),
            ready_timeout: Duration::from_secs(2),
            resume_budget: Duration::from_secs(20),
            auto_ready: false,
            stack_size: DRIVER_STACK,
            log_credits: LOG_CREDITS,
        }
    }
}

/// Request failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The NCP answered with a non-success status.
    Status(Status),
    /// The session reset before the response arrived and the NCP restarted:
    /// the outcome is unknown (spec 6.5).
    LinkReset,
    /// The carrying frame was REJECTed (raw reason).
    Rejected(u8),
    /// No response (or confirmation) in time.
    Timeout,
    /// The payload could not be encoded or the response not decoded.
    Codec,
    /// The driver stopped.
    Closed,
    /// Anything else.
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status(s) => write!(f, "NCP status {s:?}"),
            Self::LinkReset => f.write_str("link reset, outcome unknown"),
            Self::Rejected(r) => write!(f, "frame rejected (reason {r})"),
            Self::Timeout => f.write_str("timeout"),
            Self::Codec => f.write_str("encode/decode error"),
            Self::Closed => f.write_str("driver stopped"),
            Self::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for Error {}

/// An owned message (response, event or NCP request).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Kind.
    pub kind: Kind,
    /// Message ID.
    pub msg_id: u16,
    /// TID (event: event sequence).
    pub tid: u16,
    /// Payload (responses: starting with the status byte).
    pub payload: Vec<u8>,
}

impl Message {
    /// Decode a request or event body.
    pub fn decode<'a, T: Wire<'a>>(&'a self) -> Result<T, Error> {
        T::read(&mut Reader::new(&self.payload)).map_err(|_| Error::Codec)
    }

    /// Response status.
    pub fn status(&self) -> Status {
        Status::from_u8(self.payload.first().copied().unwrap_or(0xFF))
    }

    /// Decode the fields after a response's status.
    pub fn body<'a, T: Wire<'a>>(&'a self) -> Result<T, Error> {
        T::read(&mut Reader::new(self.payload.get(1..).unwrap_or(&[]))).map_err(|_| Error::Codec)
    }
}

/// Response, plus the event that completed a two-phase command.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The response.
    pub reply: Message,
    /// The completion event ([`CallOpts::confirm`]).
    pub confirm: Option<Message>,
}

/// Completion event of a two-phase command (spec 6.2): `event_id`
/// carrying the request TID as a little-endian `u16` at `tid_offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Confirm {
    /// Event ID.
    pub event_id: u16,
    /// Offset of `req_tid` in the event payload.
    pub tid_offset: usize,
    /// Time allowed after the response.
    pub timeout: Duration,
}

impl Confirm {
    /// `EVT_APS_DATA_CONFIRM` (`req_tid` first).
    pub const fn aps(timeout: Duration) -> Self {
        Self {
            event_id: ids::EVT_APS_DATA_CONFIRM,
            tid_offset: 0,
            timeout,
        }
    }
    /// `EVT_NETWORK_STATE` (`req_tid` at offset 3).
    pub const fn network_state(timeout: Duration) -> Self {
        Self {
            event_id: ids::EVT_NETWORK_STATE,
            tid_offset: 3,
            timeout,
        }
    }
}

/// Per-call options.
#[derive(Debug, Clone, Copy)]
pub struct CallOpts {
    /// Response timeout after transmission.
    pub timeout: Duration,
    /// Allowed while the lifecycle resumes the NCP (lifecycle only).
    pub during_resume: bool,
    /// Wait for a completion event after a `SUCCESS` response.
    pub confirm: Option<Confirm>,
}

impl Default for CallOpts {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            during_resume: false,
            confirm: None,
        }
    }
}

impl CallOpts {
    /// Default options with another timeout.
    pub fn timeout(timeout: Duration) -> Self {
        Self {
            timeout,
            ..Self::default()
        }
    }
}

/// `SYS_NEGOTIATE` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    /// Major version.
    pub major: u8,
    /// Minor version.
    pub minor: u8,
    /// NCP capability bits ([`msg::system::caps`]).
    pub ncp_caps: u64,
    /// Capacities `(id, value)` ([`msg::system::capacity`]).
    pub capacities: Vec<(u8, u32)>,
    /// NCP firmware version string.
    pub fw: String,
    /// Build ID.
    pub build_id: [u8; 8],
    /// Chip ID (1 = ESP32-H2).
    pub chip: u8,
}

impl Negotiated {
    /// Whether every bit of `caps` is set.
    pub fn has(&self, caps: u64) -> bool {
        self.ncp_caps & caps == caps
    }
    /// One capacity.
    pub fn capacity(&self, id: u8) -> Option<u32> {
        self.capacities
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, v)| *v)
    }
}

/// `SYS_GET_INFO` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NcpInfo {
    /// Factory IEEE.
    pub ieee_factory: [u8; 8],
    /// Active IEEE.
    pub ieee_active: [u8; 8],
    /// 0 factory, 1 override.
    pub ieee_source: u8,
    /// Chip revision.
    pub chip_rev: u16,
    /// NCP flash size in bytes (decision N4).
    pub flash_size: u32,
    /// 1 USJ, 2 UART.
    pub transport: u8,
    /// UART rate (0 for USJ).
    pub uart_baud: u32,
    /// 0 none, 1 RTS/CTS.
    pub uart_flow: u8,
    /// Board name.
    pub board: String,
    /// Product name.
    pub product: String,
}

/// One established, negotiated session.
#[derive(Debug, Clone)]
pub struct NcpSession {
    /// Link parameters (epoch, cookies, windows).
    pub link: SessionInfo,
    /// `EVT_NCP_READY` (absent if the NCP never sent it).
    pub ready: Option<EvtNcpReady>,
    /// Negotiation result.
    pub negotiated: Negotiated,
    /// `SYS_GET_INFO` (absent if it failed).
    pub info: Option<NcpInfo>,
    /// Host line in use.
    pub line: Line,
    /// Same NCP cookie as the previous session (`PEER_RESYNC`).
    pub resync: bool,
}

/// Public session state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// No link session.
    Down,
    /// Link up, negotiating.
    Negotiating,
    /// Negotiated; the lifecycle resumes the NCP.
    Resuming,
    /// Requests flow.
    Ready,
    /// No common protocol version (or negotiation refused).
    Incompatible,
    /// Driver stopped.
    Stopped,
}

/// What the driver reports.
#[derive(Debug, Clone)]
pub enum DriverEvent {
    /// The link established a session (before negotiation).
    LinkUp(SessionInfo),
    /// The peer reset the link (`PEER_RESTARTED` / `PEER_RESYNC`).
    PeerReset {
        /// Peer cookie.
        cookie: u32,
        /// Relation to the previous cookie.
        change: PeerChange,
    },
    /// No valid frame for `link_timeout_ms`, or ARQ gave up.
    LinkLost {
        /// `true` for `LINK_FAILED` (retries exhausted).
        failed: bool,
    },
    /// Negotiated session; run the resume, then [`Client::resume_done`].
    SessionUp(Arc<NcpSession>),
    /// Negotiation failed.
    Incompatible(String),
    /// An NCP event.
    Event(Message),
    /// An NCP → host request (`TBL_LOOKUP`, `TBL_WRITEBACK`); answer it with
    /// [`Client::respond`].
    Request(Message),
    /// The driver stopped.
    Stopped,
}

enum Input {
    Rx(Vec<u8>),
    RxError(String),
    Call(Call),
    Respond {
        msg_id: u16,
        tid: u16,
        payload: Vec<u8>,
    },
    ResumeDone,
    SetLine(Line),
    Reopen,
    Shutdown,
}

struct Call {
    msg_id: u16,
    payload: Vec<u8>,
    opts: CallOpts,
    reply: SyncSender<Result<Outcome, Error>>,
    tid: SyncSender<u16>,
}

struct Shared {
    session: Mutex<Option<Arc<NcpSession>>>,
    counters: Mutex<LinkCounters>,
    latency: Mutex<CallLatency>,
    state: Mutex<SessionState>,
    cv: Condvar,
    stop: AtomicBool,
    /// The reader thread has returned (it no longer reads the port).
    reader_done: AtomicBool,
}

/// Handle to a running driver. Cheap to clone; every method is thread-safe.
#[derive(Clone)]
pub struct Client {
    input: Sender<Input>,
    shared: Arc<Shared>,
}

/// An outstanding request started with [`Client::start`].
pub struct Ticket {
    /// TID allocated for the request.
    pub tid: u16,
    rx: Receiver<Result<Outcome, Error>>,
}

impl Ticket {
    /// Block until the response (and confirmation, if requested).
    pub fn wait(self) -> Result<Outcome, Error> {
        self.rx.recv().unwrap_or(Err(Error::Closed))
    }

    /// The outcome if it is there already.
    pub fn try_wait(&self) -> Option<Result<Outcome, Error>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(Error::Closed)),
        }
    }
}

impl Client {
    /// Start a request; returns at once. `payload` is the encoded body.
    pub fn start(&self, msg_id: u16, payload: Vec<u8>, opts: CallOpts) -> Result<Ticket, Error> {
        let (reply, rx) = mpsc::sync_channel(1);
        let (tid_tx, tid_rx) = mpsc::sync_channel(1);
        self.input
            .send(Input::Call(Call {
                msg_id,
                payload,
                opts,
                reply,
                tid: tid_tx,
            }))
            .map_err(|_| Error::Closed)?;
        let tid = tid_rx.recv().map_err(|_| Error::Closed)?;
        Ok(Ticket { tid, rx })
    }

    /// Request with a raw payload; non-success statuses are errors.
    pub fn call_raw(&self, msg_id: u16, payload: &[u8], opts: CallOpts) -> Result<Outcome, Error> {
        let outcome = self.start(msg_id, payload.to_vec(), opts)?.wait()?;
        match outcome.reply.status() {
            Status::Success => Ok(outcome),
            status => Err(Error::Status(status)),
        }
    }

    /// Typed request; returns the response message (status `SUCCESS`).
    pub fn call<'a, T: Wire<'a> + MessageDef>(
        &self,
        body: &T,
        opts: CallOpts,
    ) -> Result<Message, Error> {
        let payload = encode_body(body)?;
        Ok(self.call_raw(T::ID, &payload, opts)?.reply)
    }

    /// Request without a payload.
    pub fn call_empty(&self, msg_id: u16, opts: CallOpts) -> Result<Message, Error> {
        Ok(self.call_raw(msg_id, &[], opts)?.reply)
    }

    /// Answer an NCP → host request. `payload` starts with the status.
    pub fn respond(&self, msg_id: u16, tid: u16, payload: Vec<u8>) {
        let _ = self.input.send(Input::Respond {
            msg_id,
            tid,
            payload,
        });
    }

    /// The lifecycle finished resuming: open the request gate.
    pub fn resume_done(&self) {
        let _ = self.input.send(Input::ResumeDone);
    }

    /// Switch the host side of the line.
    pub fn set_line(&self, line: Line) {
        let _ = self.input.send(Input::SetLine(line));
    }

    /// Start a fresh handshake (same cookie).
    pub fn reopen(&self) {
        let _ = self.input.send(Input::Reopen);
    }

    /// Stop the driver and the reader thread.
    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        let _ = self.input.send(Input::Shutdown);
    }

    /// [`shutdown`](Self::shutdown), then wait until neither thread uses
    /// the port any more: the driver stopped (no more writes) and the
    /// reader returned (no more reads; it checks between reads of at most
    /// 100 ms). Whether both ended within `timeout`; for a caller that
    /// hands the port to someone else next (a probe before the backend).
    pub fn shutdown_and_wait(&self, timeout: Duration) -> bool {
        self.shutdown();
        let deadline = Instant::now() + timeout;
        let stopped = self.wait_state(timeout, |s| s == SessionState::Stopped).is_some();
        while !self.shared.reader_done.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
        stopped
    }

    /// Current negotiated session.
    pub fn session(&self) -> Option<Arc<NcpSession>> {
        self.shared.session.lock().unwrap().clone()
    }

    /// Current state.
    pub fn state(&self) -> SessionState {
        *self.shared.state.lock().unwrap()
    }

    /// Wait until the state satisfies `pred`.
    pub fn wait_state(
        &self,
        timeout: Duration,
        pred: impl Fn(SessionState) -> bool,
    ) -> Option<SessionState> {
        let deadline = Instant::now() + timeout;
        let mut g = self.shared.state.lock().unwrap();
        loop {
            if pred(*g) {
                return Some(*g);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            g = self.shared.cv.wait_timeout(g, deadline - now).unwrap().0;
        }
    }

    /// Host link counters (spec 5.12).
    pub fn counters(&self) -> LinkCounters {
        *self.shared.counters.lock().unwrap()
    }

    /// Where answered requests spent their time (since the start or the
    /// last [`Client::take_latency`]).
    pub fn take_latency(&self) -> CallLatency {
        std::mem::take(&mut *self.shared.latency.lock().unwrap())
    }
}

/// Time of answered requests, split by stage (µs sums; divide by `calls`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallLatency {
    pub calls: u32,
    /// Submitted → written to the port (credits, window, driver wake-up).
    pub queued_us: u64,
    /// Written → response decoded (port, NCP, port back, reader thread).
    pub wire_us: u64,
    /// Longest `wire_us` of one request.
    pub wire_max_us: u64,
    /// Driver loop passes and the time they took (µs): handling inputs,
    /// timers, and building and writing frames.
    pub passes: u32,
    pub handle_us: u64,
    pub timers_us: u64,
    pub transmit_us: u64,
    pub pass_max_us: u64,
}

impl std::fmt::Display for CallLatency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = u64::from(self.calls.max(1));
        write!(
            f,
            "{} answered: queued {} us, on the wire {} us (max {} us) on average",
            self.calls,
            self.queued_us / n,
            self.wire_us / n,
            self.wire_max_us
        )?;
        let p = u64::from(self.passes.max(1));
        write!(
            f,
            "; driver {} passes: handle {} us, timers {} us, transmit {} us (longest pass {} us) on average",
            self.passes,
            self.handle_us / p,
            self.timers_us / p,
            self.transmit_us / p,
            self.pass_max_us
        )
    }
}

/// Encode a typed body.
pub fn encode_body<'a, T: Wire<'a>>(body: &T) -> Result<Vec<u8>, Error> {
    let mut buf = vec![0u8; msg::MAX_MESSAGE_PAYLOAD];
    let n = zigbee_zrhp::codec::encode_into(body, &mut buf).map_err(|_| Error::Codec)?;
    buf.truncate(n);
    Ok(buf)
}

/// Start the reader and driver threads. `cookie` must be random, non-zero
/// and new for every start (spec 5.5).
pub fn start<R, T>(
    cfg: DriverConfig,
    mut rx: R,
    tx: T,
    cookie: u32,
) -> (Client, Receiver<DriverEvent>)
where
    R: PortRx + 'static,
    T: PortTx + 'static,
{
    let (input, inputs) = mpsc::channel();
    let (events_tx, events) = mpsc::channel();
    let shared = Arc::new(Shared {
        session: Mutex::new(None),
        counters: Mutex::new(LinkCounters::default()),
        latency: Mutex::default(),
        state: Mutex::new(SessionState::Down),
        cv: Condvar::new(),
        stop: AtomicBool::new(false),
        reader_done: AtomicBool::new(false),
    });
    let client = Client {
        input: input.clone(),
        shared: shared.clone(),
    };

    let reader_shared = shared.clone();
    thread::Builder::new()
        .name("zrhp-rx".into())
        .stack_size(4096)
        .spawn(move || {
            let _ = rx.clear_input();
            let mut buf = vec![0u8; 1024];
            while !reader_shared.stop.load(Ordering::SeqCst) {
                match rx.read(&mut buf, Duration::from_millis(100)) {
                    Ok(0) => {}
                    Ok(n) => {
                        if input.send(Input::Rx(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        if input.send(Input::RxError(e.to_string())).is_err() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(200));
                    }
                }
            }
            reader_shared.reader_done.store(true, Ordering::SeqCst);
        })
        .expect("zrhp reader thread");

    let stack = cfg.stack_size;
    thread::Builder::new()
        .name("zrhp-link".into())
        .stack_size(stack)
        .spawn(move || Driver::new(cfg, tx, cookie, events_tx, shared).run(inputs))
        .expect("zrhp driver thread");
    (client, events)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Internal,
    Resume,
    Normal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Internal {
    Negotiate,
    GetInfo,
}

#[derive(Debug)]
enum PState {
    Held,
    Queued(Instant),
    Sent(Instant),
    AwaitConfirm(Instant),
}

struct Pending {
    seq: u64,
    msg_id: u16,
    endpoint: Endpoint,
    payload: Vec<u8>,
    gate: Gate,
    internal: Option<Internal>,
    state: PState,
    created: Instant,
    opts: CallOpts,
    in_doubt: bool,
    retry: bool,
    reply_msg: Option<Message>,
    reply: Option<SyncSender<Result<Outcome, Error>>>,
}

struct Outgoing {
    tid: Option<u16>,
    bytes: Vec<u8>,
}

/// Per-endpoint FIFOs that record which request TIDs went into a frame.
#[derive(Default)]
struct OutQueues {
    q: [VecDeque<Outgoing>; 5],
    sent: Vec<u16>,
}

impl OutQueues {
    fn push(&mut self, ep: Endpoint, o: Outgoing) {
        self.q[(ep as usize).saturating_sub(1).min(4)].push_back(o);
    }
    fn clear(&mut self) {
        self.q.iter_mut().for_each(VecDeque::clear);
        self.sent.clear();
    }
}

impl TxSource for OutQueues {
    fn ready(&self, ep: Endpoint) -> bool {
        ep != Endpoint::Link && !self.q[ep as usize - 1].is_empty()
    }
    fn fill(&mut self, ep: Endpoint, buf: &mut [u8]) -> usize {
        if ep == Endpoint::Link {
            return 0;
        }
        let q = &mut self.q[ep as usize - 1];
        let mut n = 0;
        while let Some(front) = q.front() {
            if n + front.bytes.len() > buf.len() {
                break;
            }
            let o = q.pop_front().unwrap();
            buf[n..n + o.bytes.len()].copy_from_slice(&o.bytes);
            n += o.bytes.len();
            if let Some(tid) = o.tid {
                self.sent.push(tid);
            }
        }
        n
    }
}

fn encode_message(
    kind: Kind,
    retry: bool,
    msg_id: u16,
    tid: u16,
    payload: &[u8],
) -> Result<Vec<u8>, Error> {
    let mut buf = vec![0u8; msg::HEADER_LEN + payload.len()];
    let mut w = BatchWriter::new(&mut buf);
    w.push_raw(kind, retry, msg_id, tid, payload)
        .map_err(|_| Error::Codec)?;
    let n = w.len();
    buf.truncate(n);
    Ok(buf)
}

enum Owned {
    PeerReset(u32, PeerChange),
    Down,
    Failed,
    Established(SessionInfo),
    Data(Vec<u8>),
    TxFailed(Vec<u8>, TxFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Down,
    AwaitReady,
    Negotiating,
    Info,
    Resuming,
    Ready,
    Incompatible,
}

struct Driver<T: PortTx> {
    cfg: DriverConfig,
    link: Box<HostLink>,
    tx: T,
    out: OutQueues,
    buf: Box<[u8; TX_BUFFER_LEN]>,
    pending: BTreeMap<u16, Pending>,
    held: Vec<u16>,
    tids: TidAllocator,
    seq: u64,
    phase: Phase,
    clock: Instant,
    events: Sender<DriverEvent>,
    shared: Arc<Shared>,
    session: Option<SessionInfo>,
    cookie_before: Option<u32>,
    resync: bool,
    ready: Option<EvtNcpReady>,
    negotiated: Option<Negotiated>,
    established_at: Instant,
    line_idx: usize,
    line_since: Instant,
    cookie: u32,
}

impl<T: PortTx> Driver<T> {
    fn new(
        cfg: DriverConfig,
        tx: T,
        cookie: u32,
        events: Sender<DriverEvent>,
        shared: Arc<Shared>,
    ) -> Self {
        let now = Instant::now();
        let link = boxed_link(cfg.link, cookie);
        Self {
            cfg,
            link,
            tx,
            out: OutQueues::default(),
            buf: Box::new([0; TX_BUFFER_LEN]),
            pending: BTreeMap::new(),
            held: Vec::new(),
            tids: TidAllocator::new(),
            seq: 0,
            phase: Phase::Down,
            clock: now,
            events,
            shared,
            session: None,
            cookie_before: None,
            resync: false,
            ready: None,
            negotiated: None,
            established_at: now,
            line_idx: 0,
            line_since: now,
            cookie,
        }
    }

    fn now_us(&self) -> u64 {
        self.clock.elapsed().as_micros() as u64
    }

    fn line(&self) -> Line {
        self.cfg
            .lines
            .get(self.line_idx)
            .copied()
            .unwrap_or(Line::BR_DEFAULT)
    }

    fn emit(&self, ev: DriverEvent) {
        let _ = self.events.send(ev);
    }

    fn set_phase(&mut self, phase: Phase) {
        self.phase = phase;
        let public = match phase {
            Phase::Down => SessionState::Down,
            Phase::AwaitReady | Phase::Negotiating | Phase::Info => SessionState::Negotiating,
            Phase::Resuming => SessionState::Resuming,
            Phase::Ready => SessionState::Ready,
            Phase::Incompatible => SessionState::Incompatible,
        };
        *self.shared.state.lock().unwrap() = public;
        self.shared.cv.notify_all();
    }

    fn run(mut self, inputs: Receiver<Input>) {
        if let Err(e) = self.tx.set_line(self.line()) {
            warn!("zrhp: set_line {:?}: {e}", self.line());
        }
        let now = self.now_us();
        self.link.reopen(now, self.cookie, ResetReason::HostOpen);
        let mut transmit_us = 0;
        'outer: loop {
            let t = Instant::now();
            self.transmit();
            transmit_us += t.elapsed().as_micros() as u64;
            let wait = self.next_wait();
            let mut next = match inputs.recv_timeout(wait) {
                Ok(i) => Some(i),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            let woke = Instant::now();
            let mut handled = 0;
            while let Some(input) = next.take() {
                if !self.handle(input) {
                    break 'outer;
                }
                handled += 1;
                if handled < 64 {
                    next = inputs.try_recv().ok();
                }
            }
            let handled_at = Instant::now();
            self.timers();
            *self.shared.counters.lock().unwrap() = *self.link.counters();
            let mut l = self.shared.latency.lock().unwrap();
            let timers = handled_at.elapsed().as_micros() as u64;
            let handle = handled_at.duration_since(woke).as_micros() as u64;
            l.passes += 1;
            l.handle_us += handle;
            l.timers_us += timers;
            l.transmit_us += std::mem::take(&mut transmit_us);
            l.pass_max_us = l.pass_max_us.max(handle + timers);
        }
        let ids: Vec<u16> = self.pending.keys().copied().collect();
        for tid in ids {
            self.fail(tid, Error::Closed);
        }
        self.shared.stop.store(true, Ordering::SeqCst);
        *self.shared.session.lock().unwrap() = None;
        *self.shared.state.lock().unwrap() = SessionState::Stopped;
        self.shared.cv.notify_all();
        self.emit(DriverEvent::Stopped);
    }

    fn handle(&mut self, input: Input) -> bool {
        match input {
            Input::Rx(bytes) => self.feed(&bytes),
            Input::RxError(e) => warn!("zrhp: read error: {e}"),
            Input::Call(call) => self.submit(call),
            Input::Respond {
                msg_id,
                tid,
                payload,
            } => {
                if let (Some(ep), Ok(bytes)) = (
                    msg::home_endpoint(msg_id),
                    encode_message(Kind::Response, false, msg_id, tid, &payload),
                ) {
                    self.out.push(ep, Outgoing { tid: None, bytes });
                }
            }
            Input::ResumeDone => {
                if self.phase == Phase::Resuming {
                    self.set_phase(Phase::Ready);
                    self.release_held();
                }
            }
            Input::SetLine(line) => {
                if let Err(e) = self.tx.set_line(line) {
                    warn!("zrhp: set_line {line:?}: {e}");
                }
                if let Some(i) = self.cfg.lines.iter().position(|l| *l == line) {
                    self.line_idx = i;
                } else {
                    self.cfg.lines.insert(0, line);
                    self.line_idx = 0;
                }
            }
            Input::Reopen => {
                self.session_lost();
                let now = self.now_us();
                self.link.open(now, ResetReason::HostOpen);
            }
            Input::Shutdown => return false,
        }
        true
    }

    fn transmit(&mut self) {
        let now = self.now_us();
        while let Some(n) = self.link.poll_transmit(now, &mut self.out, &mut self.buf) {
            trace!("zrhp: tx {n} B");
            if let Err(e) = self.tx.write_all(&self.buf[..n]) {
                warn!("zrhp: write error: {e}");
            }
        }
        let at = Instant::now();
        for tid in std::mem::take(&mut self.out.sent) {
            if let Some(p) = self.pending.get_mut(&tid) {
                p.state = PState::Sent(at);
            }
        }
        self.drain_link();
    }

    fn feed(&mut self, bytes: &[u8]) {
        trace!("zrhp: rx {} B", bytes.len());
        let now = self.now_us();
        let mut off = 0;
        while off < bytes.len() {
            off += self.link.push_rx(&bytes[off..], now);
            self.drain_link();
        }
    }

    fn drain_link(&mut self) {
        let mut evs = Vec::new();
        while let Some(ev) = self.link.poll_event() {
            evs.push(match ev {
                LinkEvent::PeerReset { cookie, change } => Owned::PeerReset(cookie, change),
                LinkEvent::LinkDown => Owned::Down,
                LinkEvent::LinkFailed => Owned::Failed,
                LinkEvent::Established(info) => Owned::Established(info),
                LinkEvent::Data { payload, .. } => Owned::Data(payload.to_vec()),
                LinkEvent::TxFailed {
                    payload, reason, ..
                } => Owned::TxFailed(payload.to_vec(), reason),
            });
        }
        for ev in evs {
            self.on_link(ev);
        }
    }

    fn on_link(&mut self, ev: Owned) {
        match ev {
            Owned::PeerReset(cookie, change) => {
                info!("zrhp: peer reset, cookie 0x{cookie:08X} ({change:?})");
                self.session_lost();
                self.emit(DriverEvent::PeerReset { cookie, change });
            }
            Owned::Down | Owned::Failed => {
                let failed = matches!(ev, Owned::Failed);
                warn!("zrhp: link {}", if failed { "failed" } else { "down" });
                self.session_lost();
                self.emit(DriverEvent::LinkLost { failed });
            }
            Owned::Established(info) => self.on_established(info),
            Owned::Data(payload) => match Batch::parse(&payload) {
                Ok(batch) => {
                    let msgs: Vec<Message> = batch
                        .iter()
                        .map(|m| Message {
                            kind: m.header.kind,
                            msg_id: m.header.msg_id,
                            tid: m.header.tid,
                            payload: m.payload.to_vec(),
                        })
                        .collect();
                    for m in msgs {
                        self.on_message(m);
                    }
                }
                Err(e) => warn!("zrhp: bad batch {e:?}"),
            },
            Owned::TxFailed(payload, TxFailure::Rejected(reason)) => {
                if let Ok(batch) = Batch::parse(&payload) {
                    let tids: Vec<u16> = batch
                        .iter()
                        .filter(|m| m.header.kind == Kind::Request)
                        .map(|m| m.header.tid)
                        .collect();
                    for tid in tids {
                        self.fail(tid, Error::Rejected(reason));
                    }
                }
            }
            Owned::TxFailed(_, TxFailure::LinkReset) => {}
        }
    }

    /// The session ended (peer reset, link down or our reopen): transmitted
    /// requests become "in doubt" until the next session shows whether the
    /// NCP restarted.
    fn session_lost(&mut self) {
        if let Some(s) = self.session.take() {
            self.cookie_before = Some(s.peer_cookie);
        }
        self.out.clear();
        self.ready = None;
        *self.shared.session.lock().unwrap() = None;
        let mut drop_internal = Vec::new();
        for (tid, p) in self.pending.iter_mut() {
            if p.internal.is_some() {
                drop_internal.push(*tid);
                continue;
            }
            match p.state {
                PState::Sent(_) => {
                    p.in_doubt = true;
                    p.state = PState::Held;
                }
                PState::Queued(_) => p.state = PState::Held,
                PState::AwaitConfirm(_) => p.in_doubt = true,
                PState::Held => {}
            }
        }
        for tid in drop_internal {
            self.pending.remove(&tid);
        }
        self.rebuild_held();
        if self.phase != Phase::Down {
            self.set_phase(Phase::Down);
        }
    }

    fn rebuild_held(&mut self) {
        let mut held: Vec<(u64, u16)> = self
            .pending
            .iter()
            .filter(|(_, p)| matches!(p.state, PState::Held))
            .map(|(t, p)| (p.seq, *t))
            .collect();
        held.sort_unstable();
        self.held = held.into_iter().map(|(_, t)| t).collect();
    }

    fn on_established(&mut self, info: SessionInfo) {
        let same = self.cookie_before == Some(info.peer_cookie);
        info!(
            "zrhp: session up, epoch {} NCP cookie 0x{:08X}{} at {:?}",
            info.epoch,
            info.peer_cookie,
            if same { " (resync)" } else { "" },
            self.line()
        );
        self.session = Some(info);
        self.resync = same;
        self.established_at = Instant::now();
        // Every session starts with the handshake's credit (none on LOG).
        if self.cfg.log_credits > 0 {
            self.link.release_credit(Endpoint::Log, self.cfg.log_credits);
        }
        let doubtful: Vec<u16> = self
            .pending
            .iter()
            .filter(|(_, p)| p.in_doubt)
            .map(|(t, _)| *t)
            .collect();
        for tid in doubtful {
            if same {
                let p = self.pending.get_mut(&tid).unwrap();
                p.in_doubt = false;
                if matches!(p.state, PState::Held) {
                    p.retry = true;
                }
            } else {
                self.fail(tid, Error::LinkReset);
            }
        }
        self.set_phase(Phase::AwaitReady);
        self.emit(DriverEvent::LinkUp(info));
    }

    fn on_message(&mut self, m: Message) {
        match m.kind {
            Kind::Response => self.on_response(m),
            Kind::Event => {
                if m.msg_id == ids::EVT_NCP_READY {
                    match m.decode::<EvtNcpReady>() {
                        Ok(ready) => {
                            debug!("zrhp: EVT_NCP_READY {ready:?}");
                            self.ready = Some(ready);
                            if self.phase == Phase::AwaitReady {
                                self.negotiate();
                            }
                        }
                        Err(_) => warn!("zrhp: malformed EVT_NCP_READY"),
                    }
                }
                self.match_confirm(&m);
                self.emit(DriverEvent::Event(m));
            }
            Kind::Request => self.emit(DriverEvent::Request(m)),
        }
    }

    fn on_response(&mut self, m: Message) {
        let Some(p) = self.pending.get(&m.tid) else {
            debug!(
                "zrhp: response 0x{:04X} tid {} without request",
                m.msg_id, m.tid
            );
            return;
        };
        if p.msg_id != m.msg_id || matches!(p.state, PState::AwaitConfirm(_)) {
            warn!(
                "zrhp: response 0x{:04X} tid {} does not match 0x{:04X}",
                m.msg_id, m.tid, p.msg_id
            );
            return;
        }
        if let Some(kind) = p.internal {
            self.pending.remove(&m.tid);
            self.on_internal(kind, m);
            return;
        }
        if p.opts.confirm.is_some() && m.status().is_success() {
            let p = self.pending.get_mut(&m.tid).unwrap();
            p.state = PState::AwaitConfirm(Instant::now());
            p.reply_msg = Some(m);
            return;
        }
        let p = self.pending.remove(&m.tid).unwrap();
        if let PState::Sent(at) = p.state {
            let mut l = self.shared.latency.lock().unwrap();
            let wire = at.elapsed().as_micros() as u64;
            l.calls += 1;
            l.queued_us += at.saturating_duration_since(p.created).as_micros() as u64;
            l.wire_us += wire;
            l.wire_max_us = l.wire_max_us.max(wire);
        }
        if let Some(reply) = p.reply {
            let _ = reply.send(Ok(Outcome {
                reply: m,
                confirm: None,
            }));
        }
    }

    fn match_confirm(&mut self, m: &Message) {
        let hit = self.pending.iter().find_map(|(tid, p)| {
            let c = p.opts.confirm?;
            let ok = matches!(p.state, PState::AwaitConfirm(_))
                && c.event_id == m.msg_id
                && m.payload.get(c.tid_offset..c.tid_offset + 2) == Some(&tid.to_le_bytes()[..]);
            ok.then_some(*tid)
        });
        if let Some(tid) = hit {
            let p = self.pending.remove(&tid).unwrap();
            if let (Some(reply), Some(msg)) = (p.reply, p.reply_msg) {
                let _ = reply.send(Ok(Outcome {
                    reply: msg,
                    confirm: Some(m.clone()),
                }));
            }
        }
    }

    fn negotiate(&mut self) {
        let name = self.cfg.host_name.clone();
        let req = SysNegotiateReq {
            major_min: zigbee_zrhp::PROTOCOL_MAJOR,
            major_max: zigbee_zrhp::PROTOCOL_MAJOR,
            host_caps: self.cfg.host_caps,
            name: Bytes8(&name.as_bytes()[..name.len().min(32)]),
        };
        match encode_body(&req) {
            Ok(payload) => {
                self.set_phase(Phase::Negotiating);
                self.internal(Internal::Negotiate, ids::SYS_NEGOTIATE, payload);
            }
            Err(e) => warn!("zrhp: negotiate encode: {e}"),
        }
    }

    fn internal(&mut self, kind: Internal, msg_id: u16, payload: Vec<u8>) {
        let tid = self.alloc_tid();
        let Ok(bytes) = encode_message(Kind::Request, false, msg_id, tid, &payload) else {
            return;
        };
        let now = Instant::now();
        self.seq += 1;
        self.pending.insert(
            tid,
            Pending {
                seq: self.seq,
                msg_id,
                endpoint: Endpoint::System,
                payload,
                gate: Gate::Internal,
                internal: Some(kind),
                state: PState::Queued(now),
                created: now,
                opts: CallOpts::timeout(Duration::from_secs(3)),
                in_doubt: false,
                retry: false,
                reply_msg: None,
                reply: None,
            },
        );
        self.out.push(
            Endpoint::System,
            Outgoing {
                tid: Some(tid),
                bytes,
            },
        );
    }

    fn on_internal(&mut self, kind: Internal, m: Message) {
        match kind {
            Internal::Negotiate => {
                if !m.status().is_success() {
                    let why = format!("SYS_NEGOTIATE refused: {:?}", m.status());
                    warn!("zrhp: {why}");
                    self.set_phase(Phase::Incompatible);
                    self.emit(DriverEvent::Incompatible(why));
                    return;
                }
                let Ok(rsp) = m.body::<SysNegotiateRsp>() else {
                    self.set_phase(Phase::Incompatible);
                    self.emit(DriverEvent::Incompatible(
                        "malformed SYS_NEGOTIATE response".into(),
                    ));
                    return;
                };
                let negotiated = Negotiated {
                    major: rsp.major,
                    minor: rsp.minor,
                    ncp_caps: rsp.ncp_caps,
                    capacities: rsp.capacities.iter().map(|c| (c.id, c.value)).collect(),
                    fw: String::from_utf8_lossy(rsp.fw.0).into_owned(),
                    build_id: rsp.build_id,
                    chip: rsp.chip,
                };
                if negotiated.major != zigbee_zrhp::PROTOCOL_MAJOR {
                    self.set_phase(Phase::Incompatible);
                    self.emit(DriverEvent::Incompatible(format!(
                        "NCP major {}",
                        negotiated.major
                    )));
                    return;
                }
                self.negotiated = Some(negotiated);
                self.set_phase(Phase::Info);
                self.internal(Internal::GetInfo, ids::SYS_GET_INFO, Vec::new());
            }
            Internal::GetInfo => {
                let info = if m.status().is_success() {
                    m.body::<SysGetInfoRsp>().ok().map(|r| NcpInfo {
                        ieee_factory: r.ieee_factory.0,
                        ieee_active: r.ieee_active.0,
                        ieee_source: r.ieee_source,
                        chip_rev: r.chip_rev,
                        flash_size: r.flash_size,
                        transport: r.transport,
                        uart_baud: r.uart_baud,
                        uart_flow: r.uart_flow,
                        board: String::from_utf8_lossy(r.board.0).into_owned(),
                        product: String::from_utf8_lossy(r.product.0).into_owned(),
                    })
                } else {
                    warn!("zrhp: SYS_GET_INFO: {:?}", m.status());
                    None
                };
                let (Some(link), Some(negotiated)) = (self.session, self.negotiated.clone()) else {
                    return;
                };
                let session = Arc::new(NcpSession {
                    link,
                    ready: self.ready.clone(),
                    negotiated,
                    info,
                    line: self.line(),
                    resync: self.resync,
                });
                *self.shared.session.lock().unwrap() = Some(session.clone());
                self.set_phase(if self.cfg.auto_ready {
                    Phase::Ready
                } else {
                    Phase::Resuming
                });
                self.release_held();
                self.emit(DriverEvent::SessionUp(session));
            }
        }
    }

    fn alloc_tid(&mut self) -> u16 {
        loop {
            let t = self.tids.next_tid();
            if !self.pending.contains_key(&t) {
                return t;
            }
        }
    }

    fn submit(&mut self, call: Call) {
        let Some(endpoint) = msg::home_endpoint(call.msg_id) else {
            let _ = call.tid.send(0);
            let _ = call
                .reply
                .send(Err(Error::Status(Status::UnsupportedCommand)));
            return;
        };
        if call.payload.len() > msg::MAX_MESSAGE_PAYLOAD {
            let _ = call.tid.send(0);
            let _ = call.reply.send(Err(Error::Status(Status::TooLarge)));
            return;
        }
        let tid = self.alloc_tid();
        let _ = call.tid.send(tid);
        self.seq += 1;
        self.pending.insert(
            tid,
            Pending {
                seq: self.seq,
                msg_id: call.msg_id,
                endpoint,
                payload: call.payload,
                gate: if call.opts.during_resume {
                    Gate::Resume
                } else {
                    Gate::Normal
                },
                internal: None,
                state: PState::Held,
                created: Instant::now(),
                opts: call.opts,
                in_doubt: false,
                retry: false,
                reply_msg: None,
                reply: Some(call.reply),
            },
        );
        self.held.push(tid);
        self.release_held();
    }

    fn gate_open(&self, gate: Gate) -> bool {
        match gate {
            Gate::Internal => self.phase != Phase::Down,
            Gate::Resume => matches!(self.phase, Phase::Resuming | Phase::Ready),
            Gate::Normal => self.phase == Phase::Ready,
        }
    }

    fn release_held(&mut self) {
        if self.link.state() != LinkState::Established {
            return;
        }
        let now = Instant::now();
        let held = std::mem::take(&mut self.held);
        for tid in held {
            let Some(p) = self.pending.get(&tid) else {
                continue;
            };
            if !self.gate_open(p.gate) {
                self.held.push(tid);
                continue;
            }
            match encode_message(Kind::Request, p.retry, p.msg_id, tid, &p.payload) {
                Ok(bytes) => {
                    let ep = p.endpoint;
                    self.out.push(
                        ep,
                        Outgoing {
                            tid: Some(tid),
                            bytes,
                        },
                    );
                    self.pending.get_mut(&tid).unwrap().state = PState::Queued(now);
                }
                Err(e) => self.fail(tid, e),
            }
        }
    }

    fn fail(&mut self, tid: u16, error: Error) {
        if let Some(p) = self.pending.remove(&tid)
            && let Some(reply) = p.reply
        {
            let _ = reply.send(Err(error));
        }
        self.held.retain(|t| *t != tid);
    }

    fn deadline(&self, p: &Pending) -> Instant {
        match p.state {
            PState::Held => p.created + self.cfg.resume_budget.max(p.opts.timeout),
            PState::Queued(at) | PState::Sent(at) => at + p.opts.timeout,
            PState::AwaitConfirm(at) => at + p.opts.confirm.map_or(p.opts.timeout, |c| c.timeout),
        }
    }

    fn next_wait(&self) -> Duration {
        let now = Instant::now();
        let mut next = now + Duration::from_millis(200);
        if let Some(d) = self.link.next_deadline() {
            let at = self.clock + Duration::from_micros(d);
            next = next.min(at);
        }
        for p in self.pending.values() {
            next = next.min(self.deadline(p));
        }
        if self.phase == Phase::Down && self.cfg.lines.len() > 1 {
            next = next.min(self.line_since + self.cfg.hunt_after);
        }
        if self.phase == Phase::AwaitReady {
            next = next.min(self.established_at + self.cfg.ready_timeout);
        }
        next.saturating_duration_since(now)
    }

    fn timers(&mut self) {
        let now = Instant::now();
        let expired: Vec<(u16, bool)> = self
            .pending
            .iter()
            .filter(|(_, p)| now >= self.deadline(p))
            .map(|(t, p)| (*t, p.internal.is_some()))
            .collect();
        for (tid, internal) in expired {
            if internal {
                warn!("zrhp: no answer to the session handshake; reopening");
                self.pending.remove(&tid);
                self.session_lost();
                let at = self.now_us();
                self.link.open(at, ResetReason::HostOpen);
            } else {
                self.fail(tid, Error::Timeout);
            }
        }
        if self.phase == Phase::AwaitReady && now >= self.established_at + self.cfg.ready_timeout {
            warn!("zrhp: no EVT_NCP_READY; negotiating anyway");
            self.negotiate();
        }
        if self.phase == Phase::Down {
            if self.link.state() == LinkState::Established {
                self.line_since = now;
            } else if self.cfg.lines.len() > 1 && now >= self.line_since + self.cfg.hunt_after {
                self.line_idx = (self.line_idx + 1) % self.cfg.lines.len();
                self.line_since = now;
                let line = self.line();
                info!("zrhp: no session, trying {line:?}");
                if let Err(e) = self.tx.set_line(line) {
                    warn!("zrhp: set_line {line:?}: {e}");
                }
                let at = self.now_us();
                self.link.open(at, ResetReason::HostOpen);
            }
        } else {
            self.line_since = now;
        }
    }
}
