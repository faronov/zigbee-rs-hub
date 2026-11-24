//! Blocking EZSP host on top of [`Ash`] and [`Session`].
//!
//! One thread owns an [`EzspHost`]: every call writes the command, then
//! pumps the transport (feeding ASH, sending ACKs and retransmissions,
//! routing callbacks) until the matching response arrives or the timeout
//! expires. Callbacks go to a bounded [`EventReceiver`] that another
//! thread may drain; between calls, [`EzspHost::pump`] keeps the link
//! serviced. The owner must pump continuously: an NCP frame that is not
//! acknowledged is retransmitted, and after ACK_TIMEOUTS (4) consecutive
//! timeouts the NCP enters the FAILED state (UG101 §5, §6, §7).
//!
//! Errors: a command that fails for any reason (timeout, link failure,
//! I/O error) is abandoned, so its late response is ignored and the next
//! call can proceed. A link failure that arrives behind a response is
//! reported by the next call or [`EzspHost::pump`]; the session is reset
//! and [`HostEvent::LinkLost`] is queued. `ErrorKind::Interrupted` from
//! the transport is retried.
//!
//! Flow control: when the event queue holds
//! [`HostConfig::event_not_ready_at`] entries, ASH advertises nRdy so the
//! NCP pauses callback frames while responses keep flowing (UG101 §5
//! "DATA Frame Flow Control"). If the queue is still full when a callback
//! arrives, the callback is dropped and counted, and
//! [`HostEvent::Dropped`] is reported once space returns.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::ash::{
    Ash, AshConfig, AshEvent, ConfigError, FailReason, LinkState, ResetCode, SendError,
};
use crate::ezsp::command::{Callback, Command, EncodeError, Endpoint, Response, id};
use crate::ezsp::session::{Inbound, Session, SessionError};
use crate::ezsp::types::*;
use crate::ezsp::version::{
    EzspVersion, MAX_VERIFIED_VERSION, STACK_TYPE_MESH, StackVersion, VersionError, VersionPolicy,
};
use crate::ezsp::wire::DecodeError;

/// Byte stream to the NCP (UART, or the simulator).
pub trait Transport: Send {
    /// Writes every byte.
    fn write_all(&mut self, data: &[u8]) -> io::Result<()>;
    /// Waits up to `timeout` for at least one byte; `Ok(0)` on timeout.
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize>;

    /// Discards input that arrived before a reset (stale frames, e.g. the
    /// unprompted RSTACK after a hardware reset). The default reads until
    /// the line has been quiet for [`DRAIN_QUIET`], at most
    /// [`DRAIN_MAX_READS`] reads; a driver with an input flush should
    /// override it. The SDK reference host flushes its serial input before
    /// starting ASH (`ash-host.c`).
    fn discard_input(&mut self) -> io::Result<()> {
        let mut buf = [0u8; 64];
        for _ in 0..DRAIN_MAX_READS {
            match self.read(&mut buf, DRAIN_QUIET) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Quiet time that ends the default [`Transport::discard_input`].
pub const DRAIN_QUIET: Duration = Duration::from_millis(20);
/// Read bound of the default [`Transport::discard_input`], so that a
/// noisy line cannot stall the start.
pub const DRAIN_MAX_READS: usize = 256;
/// Retries of a write interrupted by a signal (`ErrorKind::Interrupted`).
const WRITE_RETRIES: usize = 8;

/// Monotonic millisecond clock.
pub trait Clock: Send {
    fn now_ms(&self) -> u64;
}

/// [`Clock`] over [`Instant`].
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    start: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        SystemClock {
            start: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

/// Host parameters.
#[derive(Debug, Clone)]
pub struct HostConfig {
    pub ash: AshConfig,
    pub policy: VersionPolicy,
    /// Version requested in the first (legacy) `version` command.
    pub desired_version: u8,
    /// Time allowed for one command's response.
    pub command_timeout: Duration,
    /// Bound of the event queue.
    pub event_queue: usize,
    /// Event-queue depth at which the host advertises nRdy.
    pub event_not_ready_at: usize,
}

impl Default for HostConfig {
    fn default() -> Self {
        HostConfig {
            ash: AshConfig::default(),
            policy: VersionPolicy::default(),
            desired_version: MAX_VERIFIED_VERSION,
            command_timeout: Duration::from_secs(10),
            event_queue: 64,
            event_not_ready_at: 48,
        }
    }
}

/// What the host learned while starting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NcpInfo {
    pub ash_version: u8,
    /// Always the software reset code: the answer to our RST.
    pub reset_code: ResetCode,
    pub ezsp_version: EzspVersion,
    pub stack_type: u8,
    pub stack_version: StackVersion,
    /// 1 if the first (legacy) exchange agreed, 2 if a second was needed.
    pub version_exchanges: u8,
}

/// Items of the event queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    Callback(Callback),
    /// The NCP reported the overflow flag (UG100 successor §2 "NCP Status").
    NcpOverflow,
    /// This many callbacks were dropped because the queue was full.
    Dropped(u32),
    /// The link failed or the NCP reset; [`EzspHost::start`] is required.
    LinkLost(FailReason),
}

/// Host failures.
#[derive(Debug)]
pub enum HostError {
    Io(io::Error),
    Config(ConfigError),
    /// The ASH link failed.
    Link(FailReason),
    /// No RSTACK within the reset budget.
    ResetTimeout,
    Send(SendError),
    Session(SessionError),
    Encode(EncodeError),
    /// The NCP's version was refused.
    Version(VersionError),
    /// [`EzspHost::start`] has not succeeded since the last reset.
    NotStarted,
    /// No response within [`HostConfig::command_timeout`].
    Timeout {
        frame_id: u16,
    },
    /// The NCP answered `invalidCommand`.
    InvalidCommand {
        frame_id: u16,
        reason: EzspStatus,
    },
    /// The NCP truncated the response.
    Truncated {
        frame_id: u16,
    },
    /// The response did not decode.
    Decode {
        frame_id: u16,
        error: DecodeError,
    },
    /// A typed helper got a non-success status.
    Status {
        frame_id: u16,
        status: Status,
    },
    /// A typed helper got a response of another type.
    UnexpectedResponse(Response),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Io(e) => write!(f, "transport: {e}"),
            HostError::Config(e) => e.fmt(f),
            HostError::Link(r) => write!(f, "ASH link failed: {r:?}"),
            HostError::ResetTimeout => write!(f, "no RSTACK from the NCP"),
            HostError::Send(e) => e.fmt(f),
            HostError::Session(e) => e.fmt(f),
            HostError::Encode(e) => e.fmt(f),
            HostError::Version(e) => e.fmt(f),
            HostError::NotStarted => write!(f, "EZSP host not started"),
            HostError::Timeout { frame_id } => write!(f, "no response to frame 0x{frame_id:04X}"),
            HostError::InvalidCommand { frame_id, reason } => {
                write!(f, "NCP rejected frame 0x{frame_id:04X}: {reason:?}")
            }
            HostError::Truncated { frame_id } => {
                write!(f, "NCP truncated the response to 0x{frame_id:04X}")
            }
            HostError::Decode { frame_id, error } => {
                write!(f, "response to 0x{frame_id:04X}: {error}")
            }
            HostError::Status { frame_id, status } => {
                write!(f, "frame 0x{frame_id:04X} returned {status:?}")
            }
            HostError::UnexpectedResponse(r) => write!(f, "unexpected response {r:?}"),
        }
    }
}

impl std::error::Error for HostError {}

impl From<io::Error> for HostError {
    fn from(e: io::Error) -> Self {
        HostError::Io(e)
    }
}

impl From<VersionError> for HostError {
    fn from(e: VersionError) -> Self {
        HostError::Version(e)
    }
}

#[derive(Debug)]
struct Queue {
    items: Mutex<VecDeque<HostEvent>>,
    ready: Condvar,
    capacity: usize,
}

impl Queue {
    fn try_push(&self, ev: HostEvent) -> Result<(), HostEvent> {
        let mut q = self.items.lock().unwrap_or_else(|p| p.into_inner());
        if q.len() >= self.capacity {
            return Err(ev);
        }
        q.push_back(ev);
        self.ready.notify_one();
        Ok(())
    }

    fn len(&self) -> usize {
        self.items.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

/// Consumer side of the bounded event queue.
#[derive(Debug, Clone)]
pub struct EventReceiver {
    queue: Arc<Queue>,
}

impl EventReceiver {
    /// Next event without waiting.
    pub fn try_recv(&self) -> Option<HostEvent> {
        self.queue
            .items
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
    }

    /// Next event, waiting up to `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<HostEvent> {
        let q = self.queue.items.lock().unwrap_or_else(|p| p.into_inner());
        let (mut q, _) = self
            .queue
            .ready
            .wait_timeout_while(q, timeout, |q| q.is_empty())
            .unwrap_or_else(|p| p.into_inner());
        q.pop_front()
    }

    /// Every queued event.
    pub fn drain(&self) -> Vec<HostEvent> {
        self.queue
            .items
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bound of the queue.
    pub fn capacity(&self) -> usize {
        self.queue.capacity
    }
}

/// Host counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostStats {
    pub commands: u32,
    pub callbacks: u32,
    pub dropped_callbacks: u32,
    pub ncp_overflows: u32,
    pub unexpected_responses: u32,
    pub bad_frames: u32,
}

enum Step {
    Connected { version: u8, reset_code: ResetCode },
    Done(Result<Response, HostError>),
    Failed(FailReason),
}

const RX_CHUNK: usize = 256;

/// Blocking EZSP host.
pub struct EzspHost<T: Transport, C: Clock = SystemClock> {
    cfg: HostConfig,
    transport: T,
    clock: C,
    ash: Ash,
    session: Session,
    queue: Arc<Queue>,
    pending_dropped: u32,
    info: Option<NcpInfo>,
    stats: HostStats,
    tx: Vec<u8>,
    rx: [u8; RX_CHUNK],
}

impl<T: Transport, C: Clock> EzspHost<T, C> {
    /// Creates the host and the receiver of its event queue.
    pub fn new(
        transport: T,
        clock: C,
        cfg: HostConfig,
    ) -> Result<(Self, EventReceiver), HostError> {
        let ash = Ash::new(cfg.ash.clone()).map_err(HostError::Config)?;
        if cfg.event_queue < 2
            || cfg.event_not_ready_at == 0
            || cfg.event_not_ready_at > cfg.event_queue
        {
            return Err(HostError::Config(ConfigError(
                "event_not_ready_at must be 1..=event_queue, event_queue >= 2",
            )));
        }
        let queue = Arc::new(Queue {
            items: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            capacity: cfg.event_queue,
        });
        let rx = EventReceiver {
            queue: queue.clone(),
        };
        let host = EzspHost {
            cfg,
            transport,
            clock,
            ash,
            session: Session::new(),
            queue,
            pending_dropped: 0,
            info: None,
            stats: HostStats::default(),
            tx: Vec::new(),
            rx: [0; RX_CHUNK],
        };
        Ok((host, rx))
    }

    /// Result of the last successful [`start`](Self::start).
    pub fn info(&self) -> Option<NcpInfo> {
        self.info
    }

    pub fn stats(&self) -> &HostStats {
        &self.stats
    }

    pub fn ash(&self) -> &Ash {
        &self.ash
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Resets the NCP over ASH (Cancel + RST, RSTACK, retries; UG101 §5,
    /// AN706 successor §5 "the Host should always perform a reset"), then
    /// negotiates the EZSP version (UG100 successor §2, §3): a legacy-format
    /// `version(desired)`; if the NCP answers with another version that
    /// the policy accepts, an extended-format `version(that)` confirms it.
    ///
    /// Input pending before the reset is discarded first
    /// ([`Transport::discard_input`]); while resetting, only an RSTACK with
    /// the software reset code is accepted (see [`Ash::reset`]).
    pub fn start(&mut self) -> Result<NcpInfo, HostError> {
        self.info = None;
        self.session.reset();
        self.transport.discard_input()?;
        let now = self.clock.now_ms();
        self.ash.reset(now);
        let a = self.ash.config();
        let budget = u64::from(a.rstack_timeout_ms) * (u64::from(a.reset_retries) + 1) + 1000;
        let deadline = now + budget;
        let (ash_version, reset_code) = loop {
            match self.pump_until(deadline)? {
                Some(Step::Connected {
                    version,
                    reset_code,
                }) => break (version, reset_code),
                Some(Step::Failed(FailReason::ResetTimeout)) | None => {
                    return Err(HostError::ResetTimeout);
                }
                Some(Step::Failed(r)) => return Err(HostError::Link(r)),
                Some(Step::Done(_)) => {}
            }
        };

        let desired = self.cfg.desired_version;
        let (ncp, stack_type, stack_version) = self.version_exchange(desired)?;
        if stack_type != STACK_TYPE_MESH {
            return Err(VersionError::StackType { stack_type }.into());
        }
        let v = self.cfg.policy.check(ncp)?;
        let mut exchanges = 1;
        // UNVERIFIED: UG100 successor §4 only says `desiredProtocolVersion`
        // must equal the NCP's version; re-sending `version` with the NCP's
        // number (extended format, as the SDK host frames every command
        // after the first) is how we satisfy that. The NCP may answer the
        // legacy command in either format; both are accepted.
        if ncp != desired {
            let (confirmed, _, _) = self.version_exchange(ncp)?;
            exchanges = 2;
            if confirmed != ncp {
                return Err(VersionError::NotConfirmed {
                    requested: ncp,
                    ncp: confirmed,
                }
                .into());
            }
        }
        self.session.set_version(v);
        let info = NcpInfo {
            ash_version,
            reset_code,
            ezsp_version: v,
            stack_type,
            stack_version,
            version_exchanges: exchanges,
        };
        self.info = Some(info);
        Ok(info)
    }

    fn version_exchange(&mut self, desired: u8) -> Result<(u8, u8, StackVersion), HostError> {
        match self.call_inner(&Command::Version { desired })? {
            Response::Version {
                protocol_version,
                stack_type,
                stack_version,
            } => Ok((protocol_version, stack_type, stack_version)),
            other => Err(HostError::UnexpectedResponse(other)),
        }
    }

    /// Sends one command and waits for its response. `invalidCommand`
    /// becomes [`HostError::InvalidCommand`]; other statuses are returned
    /// in the [`Response`].
    pub fn call(&mut self, cmd: &Command) -> Result<Response, HostError> {
        if self.session.version().is_none() {
            return Err(HostError::NotStarted);
        }
        self.call_inner(cmd)
    }

    fn call_inner(&mut self, cmd: &Command) -> Result<Response, HostError> {
        self.check_link()?;
        let frame_id = cmd.frame_id();
        let (sequence, bytes) = self.session.encode(cmd).map_err(|e| match e {
            SessionError::Encode(e) => HostError::Encode(e),
            e => HostError::Session(e),
        })?;
        if let Err(e) = self.ash.send(&bytes) {
            self.session.abandon();
            return Err(HostError::Send(e));
        }
        self.stats.commands += 1;
        let result = self.await_response(frame_id);
        // Whatever ended the wait (I/O error included), the command is no
        // longer awaited; a late response becomes `Unexpected`.
        if result.is_err() && self.session.outstanding() == Some(sequence) {
            self.session.abandon();
        }
        result
    }

    /// Processes link events left from earlier reads (a failure can arrive
    /// in the same chunk as the previous response) and refuses to send on
    /// a link that is not connected.
    fn check_link(&mut self) -> Result<(), HostError> {
        while let Some(step) = self.drain_link_events() {
            if let Step::Failed(r) = step {
                return Err(HostError::Link(r));
            }
        }
        match self.ash.state() {
            LinkState::Connected => Ok(()),
            LinkState::Failed(r) => {
                if self.session.version().is_some() {
                    self.session.reset();
                    self.info = None;
                    self.deliver(HostEvent::LinkLost(r));
                }
                Err(HostError::Link(r))
            }
            LinkState::Disconnected | LinkState::Resetting => Err(HostError::NotStarted),
        }
    }

    fn await_response(&mut self, frame_id: u16) -> Result<Response, HostError> {
        let timeout = self.cfg.command_timeout.as_millis() as u64;
        let deadline = self.clock.now_ms() + timeout;
        loop {
            match self.pump_until(deadline)? {
                Some(Step::Done(r)) => {
                    return match r {
                        Ok(Response::InvalidCommand { reason }) => {
                            Err(HostError::InvalidCommand { frame_id, reason })
                        }
                        other => other,
                    };
                }
                Some(Step::Failed(r)) => return Err(HostError::Link(r)),
                Some(Step::Connected { .. }) => {}
                None => return Err(HostError::Timeout { frame_id }),
            }
        }
    }

    /// Services the link for `duration`: ACKs, retransmissions, callbacks.
    pub fn pump(&mut self, duration: Duration) -> Result<(), HostError> {
        let deadline = self.clock.now_ms() + duration.as_millis() as u64;
        loop {
            match self.pump_until(deadline)? {
                None => return Ok(()),
                Some(Step::Failed(r)) => return Err(HostError::Link(r)),
                Some(_) => {}
            }
        }
    }

    fn pump_until(&mut self, deadline: u64) -> Result<Option<Step>, HostError> {
        loop {
            let now = self.clock.now_ms();
            self.ash.handle_timeout(now);
            self.flush_queue_state();
            self.ash
                .set_not_ready(self.queue.len() >= self.cfg.event_not_ready_at);
            self.tx.clear();
            while self.ash.poll_transmit(now, &mut self.tx) {}
            if !self.tx.is_empty() {
                self.write_tx()?;
            }
            if let Some(step) = self.drain_link_events() {
                return Ok(Some(step));
            }
            if now >= deadline {
                return Ok(None);
            }
            let wake = self
                .ash
                .next_timeout()
                .map_or(deadline, |t| t.clamp(now, deadline));
            let n = match self
                .transport
                .read(&mut self.rx, Duration::from_millis(wake - now))
            {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => 0,
                other => other?,
            };
            if n > 0 {
                let now = self.clock.now_ms();
                self.ash.handle_rx(&self.rx[..n], now);
            }
        }
    }

    /// Writes the pending frames; a signal interruption is retried (ASH
    /// recovers from a partially written frame by CRC failure and NAK).
    fn write_tx(&mut self) -> io::Result<()> {
        let mut tries = 0;
        loop {
            match self.transport.write_all(&self.tx) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted && tries < WRITE_RETRIES => {
                    tries += 1
                }
                other => return other,
            }
        }
    }

    fn drain_link_events(&mut self) -> Option<Step> {
        while let Some(ev) = self.ash.poll_event() {
            match ev {
                AshEvent::Connected {
                    version,
                    reset_code,
                } => {
                    return Some(Step::Connected {
                        version,
                        reset_code,
                    });
                }
                AshEvent::Failed(r) => {
                    self.session.reset();
                    self.info = None;
                    self.deliver(HostEvent::LinkLost(r));
                    return Some(Step::Failed(r));
                }
                AshEvent::PeerReset => {}
                AshEvent::Data(bytes) => {
                    if let Some(step) = self.on_ezsp(&bytes) {
                        return Some(step);
                    }
                }
            }
        }
        None
    }

    fn on_ezsp(&mut self, bytes: &[u8]) -> Option<Step> {
        let inbound = match self.session.on_frame(bytes) {
            Ok(i) => i,
            Err(_) => {
                self.stats.bad_frames += 1;
                return None;
            }
        };
        if inbound.flags().overflow {
            self.stats.ncp_overflows += 1;
            self.deliver(HostEvent::NcpOverflow);
        }
        match inbound {
            Inbound::Response { response, .. } => Some(Step::Done(Ok(response))),
            Inbound::Truncated { frame_id, .. } => {
                Some(Step::Done(Err(HostError::Truncated { frame_id })))
            }
            Inbound::BadResponse {
                frame_id, error, ..
            } => Some(Step::Done(Err(HostError::Decode { frame_id, error }))),
            Inbound::Callback { callback, .. } => {
                self.stats.callbacks += 1;
                self.deliver(HostEvent::Callback(callback));
                None
            }
            Inbound::BadCallback { .. } => {
                self.stats.bad_frames += 1;
                None
            }
            Inbound::Unexpected { .. } => {
                self.stats.unexpected_responses += 1;
                None
            }
        }
    }

    fn flush_queue_state(&mut self) {
        if self.pending_dropped > 0
            && self
                .queue
                .try_push(HostEvent::Dropped(self.pending_dropped))
                .is_ok()
        {
            self.pending_dropped = 0;
        }
    }

    fn deliver(&mut self, ev: HostEvent) {
        self.flush_queue_state();
        if self.pending_dropped > 0 || self.queue.try_push(ev).is_err() {
            self.pending_dropped += 1;
            self.stats.dropped_callbacks += 1;
        }
    }

    fn expect_ok(frame_id: u16, status: Status) -> Result<(), HostError> {
        if status.is_success() {
            Ok(())
        } else {
            Err(HostError::Status { frame_id, status })
        }
    }

    /// `getConfigurationValue`.
    pub fn get_configuration_value(&mut self, id: ConfigId) -> Result<u16, HostError> {
        match self.call(&Command::GetConfigurationValue { id })? {
            Response::GetConfigurationValue { status, value } => {
                Self::expect_ok(id::GET_CONFIGURATION_VALUE, status)?;
                Ok(value)
            }
            other => Err(HostError::UnexpectedResponse(other)),
        }
    }

    /// `setConfigurationValue`.
    pub fn set_configuration_value(&mut self, id: ConfigId, value: u16) -> Result<(), HostError> {
        match self.call(&Command::SetConfigurationValue { id, value })? {
            Response::SetConfigurationValue { status } => {
                Self::expect_ok(id::SET_CONFIGURATION_VALUE, status)
            }
            other => Err(HostError::UnexpectedResponse(other)),
        }
    }

    /// `addEndpoint` (before `networkInit`: UG100 successor §4).
    pub fn add_endpoint(&mut self, endpoint: Endpoint) -> Result<(), HostError> {
        match self.call(&Command::AddEndpoint(endpoint))? {
            Response::AddEndpoint { status } => Self::expect_ok(id::ADD_ENDPOINT, status),
            other => Err(HostError::UnexpectedResponse(other)),
        }
    }

    /// `networkInit`; the raw status, because `NOT_JOINED` is a normal
    /// outcome for an NCP without a stored network.
    pub fn network_init(&mut self, bitmask: NetworkInitBitmask) -> Result<Status, HostError> {
        match self.call(&Command::NetworkInit { bitmask })? {
            Response::NetworkInit { status } => Ok(status),
            other => Err(HostError::UnexpectedResponse(other)),
        }
    }

    /// `networkState`.
    pub fn network_state(&mut self) -> Result<NetworkStatus, HostError> {
        match self.call(&Command::NetworkState)? {
            Response::NetworkState { state } => Ok(state),
            other => Err(HostError::UnexpectedResponse(other)),
        }
    }

    /// `getNetworkParameters`.
    pub fn get_network_parameters(&mut self) -> Result<(NodeType, NetworkParameters), HostError> {
        match self.call(&Command::GetNetworkParameters)? {
            Response::GetNetworkParameters {
                status,
                node_type,
                parameters,
            } => {
                Self::expect_ok(id::GET_NETWORK_PARAMETERS, status)?;
                Ok((node_type, parameters))
            }
            other => Err(HostError::UnexpectedResponse(other)),
        }
    }

    /// `getEui64`.
    pub fn get_eui64(&mut self) -> Result<Eui64, HostError> {
        match self.call(&Command::GetEui64)? {
            Response::GetEui64 { eui64 } => Ok(eui64),
            other => Err(HostError::UnexpectedResponse(other)),
        }
    }

    /// `getNodeId`.
    pub fn get_node_id(&mut self) -> Result<NodeId, HostError> {
        match self.call(&Command::GetNodeId)? {
            Response::GetNodeId { node_id } => Ok(node_id),
            other => Err(HostError::UnexpectedResponse(other)),
        }
    }

    /// `permitJoining`.
    pub fn permit_joining(&mut self, duration: u8) -> Result<(), HostError> {
        match self.call(&Command::PermitJoining { duration })? {
            Response::PermitJoining { status } => Self::expect_ok(id::PERMIT_JOINING, status),
            other => Err(HostError::UnexpectedResponse(other)),
        }
    }

    /// `sendUnicast` (direct); returns the APS sequence number. The
    /// outcome arrives as [`Callback::MessageSent`] with the same `tag`.
    pub fn send_unicast(
        &mut self,
        destination: NodeId,
        aps: ApsFrame,
        tag: u16,
        message: &[u8],
    ) -> Result<u8, HostError> {
        let cmd = Command::SendUnicast {
            kind: OutgoingMessageType::DIRECT,
            index_or_destination: destination,
            aps,
            tag,
            message: message.to_vec(),
        };
        match self.call(&cmd)? {
            Response::SendUnicast { status, sequence } => {
                Self::expect_ok(id::SEND_UNICAST, status)?;
                Ok(sequence)
            }
            other => Err(HostError::UnexpectedResponse(other)),
        }
    }

    /// `maximumPayloadLength`.
    pub fn maximum_payload_length(&mut self) -> Result<u8, HostError> {
        match self.call(&Command::MaximumPayloadLength)? {
            Response::MaximumPayloadLength { aps_length } => Ok(aps_length),
            other => Err(HostError::UnexpectedResponse(other)),
        }
    }
}
