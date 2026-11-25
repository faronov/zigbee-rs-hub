//! Sans-IO ASH link state machine (UG101 §4–§7).
//!
//! The caller feeds received bytes ([`Ash::handle_rx`]), drives timers with
//! a monotonic millisecond clock ([`Ash::handle_timeout`],
//! [`Ash::next_timeout`]), drains bytes to transmit one frame at a time
//! ([`Ash::poll_transmit`]) and takes delivered EZSP frames and link
//! events ([`Ash::poll_event`]). Nothing here blocks, allocates without a
//! bound, or reads a clock.

use std::collections::VecDeque;

use super::codec::{
    ASH_VERSION, CANCEL, Deframed, Deframer, Frame, FrameError, MAX_DATA_FIELD, MIN_DATA_FIELD,
};

/// Reset and error codes of RSTACK / ERROR frames (UG101 §6 table).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResetCode(pub u8);

impl ResetCode {
    pub const UNKNOWN: ResetCode = ResetCode(0x00);
    pub const EXTERNAL: ResetCode = ResetCode(0x01);
    pub const POWER_ON: ResetCode = ResetCode(0x02);
    pub const WATCHDOG: ResetCode = ResetCode(0x03);
    pub const ASSERT: ResetCode = ResetCode(0x06);
    pub const BOOTLOADER: ResetCode = ResetCode(0x09);
    pub const SOFTWARE: ResetCode = ResetCode(0x0B);
    pub const EXCEEDED_MAX_ACK_TIMEOUTS: ResetCode = ResetCode(0x51);
    /// Codes ≥ 0x80 are chip-specific: the HAL reset code + 0x80.
    pub const CHIP_SPECIFIC: ResetCode = ResetCode(0x80);

    /// Name from the UG101 table, if the code is listed there.
    pub fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            0x00 => "unknown",
            0x01 => "external",
            0x02 => "power-on",
            0x03 => "watchdog",
            0x06 => "assert",
            0x09 => "bootloader",
            0x0B => "software",
            0x51 => "exceeded maximum ACK timeout count",
            0x80.. => "chip-specific",
            _ => return None,
        })
    }
}

impl std::fmt::Debug for ResetCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.name() {
            Some(n) => write!(f, "ResetCode(0x{:02X} {n})", self.0),
            None => write!(f, "ResetCode(0x{:02X})", self.0),
        }
    }
}

/// Link parameters. Defaults follow UG101 §7 where the document gives a
/// value; the others are marked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AshConfig {
    /// Maximum unacknowledged DATA frames in flight, 1..=7 ("must be
    /// smaller than the 8 frames allowed by the 3-bit frame number",
    /// UG101 §5). UG101 §7 gives TX_K = 5 for the NCP only.
    // UNVERIFIED: no host TX_K is documented; 3 matches the Gecko SDK
    // reference host default. EZSP keeps one command in flight anyway.
    pub tx_window: u8,
    /// Randomize DATA fields (UG101 §7 RANDOMIZE: enabled).
    pub randomize: bool,
    /// T_RX_ACK_INIT, initial ACK timeout (UG101 §7: 1.6 s).
    pub ack_timeout_init_ms: u32,
    /// T_RX_ACK_MIN (UG101 §7: 0.4 s).
    pub ack_timeout_min_ms: u32,
    /// T_RX_ACK_MAX (UG101 §7: 3.2 s).
    pub ack_timeout_max_ms: u32,
    /// Consecutive ACK timeouts tolerated before the link is declared
    /// failed (UG101 §7 ACK_TIMEOUTS: 4, defined for the NCP and applied
    /// to the host the same way). 0 disables the limit.
    // UNVERIFIED: UG101 defines ACK_TIMEOUTS for the NCP only.
    pub max_ack_timeouts: u8,
    /// T_RSTACK_MAX: wait for RSTACK after RST.
    // UNVERIFIED: UG101 §5 names T_RSTACK_MAX but the current revision
    // gives no value; 3.2 s (= T_RX_ACK_MAX) is our choice.
    pub rstack_timeout_ms: u32,
    /// RST retries after the first attempt (UG101 §5: "up to five times").
    pub reset_retries: u8,
    /// T_LOCAL_NOTRDY: period at which a not-ready host repeats nRdy.
    // UNVERIFIED: UG101 §5 names T_LOCAL_NOTRDY without a value; it must
    // stay below T_REMOTE_NOTRDY (1.0 s, UG101 §7).
    pub not_ready_refresh_ms: u32,
    /// Bound of delivered-but-unread DATA frames. A valid in-sequence frame
    /// arriving when full is discarded and NAKed ("discarded due to lack of
    /// memory", UG101 §5).
    pub rx_queue_frames: usize,
    /// At this many unread frames the host advertises nRdy (UG101 §5
    /// "DATA Frame Flow Control"). Must be ≤ `rx_queue_frames`.
    pub rx_not_ready_at: usize,
    /// Bound of frames queued by [`Ash::send`] that do not fit the window.
    pub tx_queue_frames: usize,
    /// Longest pause of the host's transmitter after an XOFF without the
    /// matching XON (software flow control, UG101 §4). A lost XON must not
    /// stall the link for good; transmission resumes after this.
    // UNVERIFIED: UG101 gives no value; 1 s stays below the ACK timeouts
    // the NCP counts before it gives up on the link (UG101 §7).
    pub xoff_timeout_ms: u32,
}

impl Default for AshConfig {
    fn default() -> Self {
        AshConfig {
            tx_window: 3,
            randomize: true,
            ack_timeout_init_ms: 1600,
            ack_timeout_min_ms: 400,
            ack_timeout_max_ms: 3200,
            max_ack_timeouts: 4,
            rstack_timeout_ms: 3200,
            reset_retries: 5,
            not_ready_refresh_ms: 500,
            rx_queue_frames: 8,
            rx_not_ready_at: 4,
            tx_queue_frames: 4,
            xoff_timeout_ms: 1000,
        }
    }
}

/// Invalid [`AshConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub &'static str);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid ASH configuration: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

impl AshConfig {
    /// Checks the ranges documented on the fields.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=7).contains(&self.tx_window) {
            return Err(ConfigError("tx_window must be 1..=7"));
        }
        if self.ack_timeout_min_ms == 0
            || self.ack_timeout_min_ms > self.ack_timeout_max_ms
            || !(self.ack_timeout_min_ms..=self.ack_timeout_max_ms)
                .contains(&self.ack_timeout_init_ms)
        {
            return Err(ConfigError(
                "ACK timeouts must satisfy 0 < min <= init <= max",
            ));
        }
        if self.rstack_timeout_ms == 0
            || self.not_ready_refresh_ms == 0
            || self.xoff_timeout_ms == 0
        {
            return Err(ConfigError("timeouts must be non-zero"));
        }
        if self.rx_queue_frames == 0 || self.tx_queue_frames == 0 {
            return Err(ConfigError("queues must hold at least one frame"));
        }
        if self.rx_not_ready_at == 0 || self.rx_not_ready_at > self.rx_queue_frames {
            return Err(ConfigError("rx_not_ready_at must be 1..=rx_queue_frames"));
        }
        Ok(())
    }
}

/// Why the link left the CONNECTED state or never reached it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    /// No RSTACK after the first RST and all retries (UG101 §5).
    ResetTimeout,
    /// RSTACK carried an ASH version other than 2 (UG101 §3, §5).
    AshVersion(u8),
    /// The NCP sent an ERROR frame and is in the FAILED state (UG101 §6).
    NcpError(ResetCode),
    /// An RSTACK arrived while connected: the NCP reset by itself.
    NcpReset(ResetCode),
    /// More than `max_ack_timeouts` consecutive ACK timeouts (UG101 §5, §7).
    AckTimeouts,
}

/// Link state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Before the first reset.
    Disconnected,
    /// RST sent, waiting for RSTACK.
    Resetting,
    /// RSTACK received: DATA may flow (UG101 §5).
    Connected,
    /// Failed; a new [`Ash::reset`] is required.
    Failed(FailReason),
}

/// Output of the link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AshEvent {
    /// RSTACK received after our RST; frame numbers restart at 0.
    Connected { version: u8, reset_code: ResetCode },
    /// The EZSP frame of a valid, in-sequence DATA frame.
    Data(Vec<u8>),
    /// The link failed; queued and unacknowledged frames were dropped.
    /// DATA delivered before the failure stays readable before this event.
    Failed(FailReason),
    /// (NCP role) RST received; the link restarted.
    PeerReset,
}

/// [`Ash::send`] rejected a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// Not in the CONNECTED state.
    NotConnected,
    /// EZSP frame length outside 3..=220 bytes.
    InvalidLength(usize),
    /// The transmit queue is full ([`AshConfig::tx_queue_frames`]).
    QueueFull,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::NotConnected => write!(f, "ASH link not connected"),
            SendError::InvalidLength(n) => write!(
                f,
                "DATA field of {n} bytes outside {MIN_DATA_FIELD}..={MAX_DATA_FIELD}"
            ),
            SendError::QueueFull => write!(f, "ASH transmit queue full"),
        }
    }
}

impl std::error::Error for SendError {}

/// Link counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AshStats {
    pub tx_data: u32,
    pub tx_retransmit: u32,
    pub tx_ack: u32,
    pub tx_nak: u32,
    pub tx_rst: u32,
    pub rx_data: u32,
    pub rx_ack: u32,
    pub rx_nak: u32,
    pub rx_duplicate: u32,
    pub rx_out_of_sequence: u32,
    pub rx_no_space: u32,
    pub rx_bad_crc: u32,
    pub rx_bad_control: u32,
    pub rx_bad_length: u32,
    pub rx_bad_ack_num: u32,
    pub rx_too_short: u32,
    pub rx_too_long: u32,
    pub rx_cancelled: u32,
    pub rx_substituted: u32,
    pub rx_discarded: u32,
    /// RSTACKs ignored while resetting: not the answer to our RST.
    pub rx_stale_rstack: u32,
    /// XOFF bytes received (host role).
    pub rx_xoff: u32,
    /// XON bytes received (host role).
    pub rx_xon: u32,
    /// XOFF pauses that ended without an XON ([`AshConfig::xoff_timeout_ms`]).
    pub xoff_timeouts: u32,
    pub ack_timeouts: u32,
    pub resets: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Host,
    Ncp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Disconnected,
    Resetting {
        attempt: u8,
        rst_pending: bool,
        deadline: Option<u64>,
    },
    Connected,
    Failed(FailReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctrl {
    Ack,
    Nak,
}

#[derive(Debug)]
struct Queued {
    payload: Vec<u8>,
    callback: bool,
}

#[derive(Debug)]
struct InFlight {
    frm_num: u8,
    payload: Vec<u8>,
    first_sent: u64,
    last_sent: u64,
    retransmitted: bool,
}

/// Control events (not DATA) kept for [`Ash::poll_event`]; the oldest is
/// dropped beyond this. They are produced at most a few times per reset.
const MAX_CONTROL_EVENTS: usize = 8;
/// T_REMOTE_NOTRDY (UG101 §7: 1.0 s), honoured by the NCP role.
const REMOTE_NOT_READY_MS: u64 = 1000;

fn inc(n: u8) -> u8 {
    (n + 1) & 7
}

/// `lo <= n <= hi` in modulo-8 frame-number space.
fn within(lo: u8, n: u8, hi: u8) -> bool {
    n.wrapping_sub(lo) & 7 <= hi.wrapping_sub(lo) & 7
}

/// ASH endpoint. [`Ash::new`] is the host side; the NCP side exists for
/// the simulator.
#[derive(Debug)]
pub struct Ash {
    role: Role,
    cfg: AshConfig,
    state: State,
    deframer: Deframer,
    // Transmit direction.
    frm_tx_next: u8,
    ack_rx: u8,
    window: VecDeque<InFlight>,
    queue: VecDeque<Queued>,
    retx: Option<usize>,
    t_rx_ack: u64,
    timeouts: u8,
    // Receive direction.
    frm_rx_expected: u8,
    reject: bool,
    ctrl: Option<Ctrl>,
    // Flow control.
    user_not_ready: bool,
    advertised_not_ready: bool,
    last_not_ready_tx: u64,
    remote_not_ready_until: Option<u64>,
    /// XOFF received: nothing is transmitted before XON or this time.
    xoff_until: Option<u64>,
    // NCP role.
    rstack_pending: Option<u8>,
    error_pending: bool,
    // Output.
    events: VecDeque<AshEvent>,
    data_events: usize,
    stats: AshStats,
}

impl Ash {
    /// Host-side link in [`LinkState::Disconnected`]; call [`reset`](Self::reset).
    pub fn new(cfg: AshConfig) -> Result<Ash, ConfigError> {
        Self::with_role(Role::Host, cfg)
    }

    /// NCP-side link (simulator): waits for RST, answers RSTACK.
    #[cfg(any(test, feature = "sim"))]
    pub(crate) fn new_ncp(cfg: AshConfig) -> Result<Ash, ConfigError> {
        Self::with_role(Role::Ncp, cfg)
    }

    fn with_role(role: Role, cfg: AshConfig) -> Result<Ash, ConfigError> {
        cfg.validate()?;
        Ok(Ash {
            role,
            t_rx_ack: u64::from(cfg.ack_timeout_init_ms),
            cfg,
            state: State::Disconnected,
            deframer: Deframer::new(),
            frm_tx_next: 0,
            ack_rx: 0,
            window: VecDeque::new(),
            queue: VecDeque::new(),
            retx: None,
            timeouts: 0,
            frm_rx_expected: 0,
            reject: false,
            ctrl: None,
            user_not_ready: false,
            advertised_not_ready: false,
            last_not_ready_tx: 0,
            remote_not_ready_until: None,
            xoff_until: None,
            rstack_pending: None,
            error_pending: false,
            events: VecDeque::new(),
            data_events: 0,
            stats: AshStats::default(),
        })
    }

    /// Current state.
    pub fn state(&self) -> LinkState {
        match self.state {
            State::Disconnected => LinkState::Disconnected,
            State::Resetting { .. } => LinkState::Resetting,
            State::Connected => LinkState::Connected,
            State::Failed(r) => LinkState::Failed(r),
        }
    }

    /// Configuration in use.
    pub fn config(&self) -> &AshConfig {
        &self.cfg
    }

    /// Counters.
    pub fn stats(&self) -> &AshStats {
        &self.stats
    }

    /// Current adaptive ACK timeout `t_rx_ack` in milliseconds.
    pub fn ack_timeout_ms(&self) -> u64 {
        self.t_rx_ack
    }

    /// Frames accepted by [`send`](Self::send) and not yet acknowledged.
    pub fn tx_pending(&self) -> usize {
        self.window.len() + self.queue.len()
    }

    /// Host-initiated reset (UG101 §5): drops every queued frame and event,
    /// sends Cancel + RST and waits for an RSTACK with the software reset
    /// code, retrying [`AshConfig::reset_retries`] times. Other RSTACKs are
    /// stale and discarded ([`AshStats::rx_stale_rstack`]).
    pub fn reset(&mut self, _now: u64) {
        if self.role != Role::Host {
            return;
        }
        self.restart_link();
        self.events.clear();
        self.data_events = 0;
        self.stats.resets += 1;
        self.state = State::Resetting {
            attempt: 0,
            rst_pending: true,
            deadline: None,
        };
    }

    fn restart_link(&mut self) {
        self.deframer.clear();
        self.frm_tx_next = 0;
        self.ack_rx = 0;
        self.window.clear();
        self.queue.clear();
        self.retx = None;
        self.t_rx_ack = u64::from(self.cfg.ack_timeout_init_ms);
        self.timeouts = 0;
        self.frm_rx_expected = 0;
        self.reject = false;
        self.ctrl = None;
        self.advertised_not_ready = false;
        self.remote_not_ready_until = None;
        self.xoff_until = None;
        self.error_pending = false;
    }

    /// Queues one EZSP frame. It is sent when the window allows.
    pub fn send(&mut self, ezsp: &[u8]) -> Result<(), SendError> {
        self.enqueue(ezsp, false)
    }

    /// NCP role: queues a frame, marking callbacks for nRdy flow control.
    #[cfg(any(test, feature = "sim"))]
    pub(crate) fn send_ncp(&mut self, ezsp: &[u8], callback: bool) -> Result<(), SendError> {
        self.enqueue(ezsp, callback)
    }

    fn enqueue(&mut self, ezsp: &[u8], callback: bool) -> Result<(), SendError> {
        if self.state != State::Connected {
            return Err(SendError::NotConnected);
        }
        if !(MIN_DATA_FIELD..=MAX_DATA_FIELD).contains(&ezsp.len()) {
            return Err(SendError::InvalidLength(ezsp.len()));
        }
        if self.queue.len() >= self.cfg.tx_queue_frames {
            return Err(SendError::QueueFull);
        }
        self.queue.push_back(Queued {
            payload: ezsp.to_vec(),
            callback,
        });
        Ok(())
    }

    /// Asks the NCP to pause callback DATA frames (nRdy, UG101 §5). The
    /// link also sets nRdy by itself when [`AshConfig::rx_not_ready_at`]
    /// frames are unread.
    pub fn set_not_ready(&mut self, not_ready: bool) {
        self.user_not_ready = not_ready;
    }

    fn local_not_ready(&self) -> bool {
        self.user_not_ready || self.data_events >= self.cfg.rx_not_ready_at
    }

    /// NCP role: enter the FAILED state and report it with ERROR frames.
    #[cfg(any(test, feature = "sim"))]
    pub(crate) fn fail_ncp(&mut self, code: ResetCode) {
        self.restart_link();
        self.state = State::Failed(FailReason::NcpError(code));
        self.error_pending = true;
    }

    /// NCP role: reboot silently (simulated boot time); frames are
    /// discarded until [`ncp_spontaneous_reset`](Self::ncp_spontaneous_reset).
    #[cfg(any(test, feature = "sim"))]
    pub(crate) fn ncp_reboot(&mut self) {
        self.restart_link();
        self.events.clear();
        self.data_events = 0;
        self.rstack_pending = None;
        self.state = State::Disconnected;
    }

    /// NCP role: reboot without an RST and announce it with RSTACK.
    #[cfg(any(test, feature = "sim"))]
    pub(crate) fn ncp_spontaneous_reset(&mut self, code: ResetCode) {
        self.restart_link();
        self.events.clear();
        self.data_events = 0;
        self.state = State::Connected;
        self.rstack_pending = Some(code.0);
    }

    /// Next link event: DATA in order of arrival, interleaved with state
    /// changes.
    pub fn poll_event(&mut self) -> Option<AshEvent> {
        let ev = self.events.pop_front()?;
        if matches!(ev, AshEvent::Data(_)) {
            self.data_events -= 1;
        }
        Some(ev)
    }

    fn push_control_event(&mut self, ev: AshEvent) {
        let controls = self.events.len() - self.data_events;
        if controls >= MAX_CONTROL_EVENTS
            && let Some(i) = self
                .events
                .iter()
                .position(|e| !matches!(e, AshEvent::Data(_)))
        {
            self.events.remove(i);
        }
        self.events.push_back(ev);
    }

    fn fail(&mut self, reason: FailReason) {
        self.restart_link();
        self.state = State::Failed(reason);
        self.push_control_event(AshEvent::Failed(reason));
    }

    /// Feeds received bytes.
    pub fn handle_rx(&mut self, bytes: &[u8], now: u64) {
        for &b in bytes {
            match self.deframer.push(b) {
                None => {}
                Some(Deframed::Frame) => {
                    match Frame::decode(self.deframer.frame(), self.cfg.randomize) {
                        Ok(frame) => self.on_frame(frame, now),
                        Err(e) => self.on_bad_frame(e),
                    }
                }
                Some(Deframed::Cancelled) => self.stats.rx_cancelled += 1,
                Some(Deframed::Substituted) => {
                    self.stats.rx_substituted += 1;
                    self.reject_if_connected();
                }
                Some(Deframed::TooLong) => {
                    self.stats.rx_too_long += 1;
                    self.reject_if_connected();
                }
                // UG101 §4: XON/XOFF are always ignored by the NCP.
                Some(Deframed::Xoff) if self.role == Role::Host => {
                    self.stats.rx_xoff += 1;
                    self.xoff_until = Some(now + u64::from(self.cfg.xoff_timeout_ms));
                }
                Some(Deframed::Xon) if self.role == Role::Host => {
                    self.stats.rx_xon += 1;
                    self.xoff_until = None;
                }
                Some(Deframed::Xoff | Deframed::Xon) => {}
            }
        }
    }

    /// Whether an XOFF holds our transmitter at `now`.
    pub fn tx_paused(&self, now: u64) -> bool {
        self.xoff_until.is_some_and(|t| now < t)
    }

    /// Ends an XOFF pause whose XON did not come in time.
    fn expire_xoff(&mut self, now: u64) {
        if self.xoff_until.is_some_and(|t| now >= t) {
            self.xoff_until = None;
            self.stats.xoff_timeouts += 1;
        }
    }

    fn on_bad_frame(&mut self, e: FrameError) {
        match e {
            FrameError::TooShort => self.stats.rx_too_short += 1,
            FrameError::BadCrc => self.stats.rx_bad_crc += 1,
            FrameError::BadControl(_) => self.stats.rx_bad_control += 1,
            FrameError::BadLength { .. } => self.stats.rx_bad_length += 1,
        }
        self.reject_if_connected();
    }

    fn reject_if_connected(&mut self) {
        match self.state {
            State::Connected => self.set_reject(),
            State::Failed(_) if self.role == Role::Ncp => self.error_pending = true,
            _ => {}
        }
    }

    /// Sets the Reject Condition; a NAK is sent only when it was clear
    /// (UG101 §5 "Reject Condition and NAK Frames").
    fn set_reject(&mut self) {
        if !self.reject {
            self.reject = true;
            self.ctrl = Some(Ctrl::Nak);
        }
    }

    fn on_frame(&mut self, frame: Frame, now: u64) {
        let valid_type = matches!(
            (&frame, self.role),
            (Frame::Rst, Role::Ncp)
                | (Frame::RstAck { .. } | Frame::Error { .. }, Role::Host)
                | (
                    Frame::Data { .. } | Frame::Ack { .. } | Frame::Nak { .. },
                    _
                )
        );
        if !valid_type {
            self.on_bad_frame(FrameError::BadControl(frame.control().encode()));
            return;
        }
        match self.role {
            Role::Host => self.on_frame_host(frame, now),
            Role::Ncp => self.on_frame_ncp(frame, now),
        }
    }

    fn on_frame_host(&mut self, frame: Frame, now: u64) {
        match self.state {
            State::Resetting { rst_pending, .. } => {
                // UG101 §5: discard everything until a valid RSTACK. Only an
                // RSTACK with the software reset code answers our RST; any
                // other (hardware reset, earlier boot) is stale. UG101 does
                // not say this; the SDK reference host `ash-host.c` filters
                // the same way for its RST reset method.
                if let Frame::RstAck {
                    version,
                    reset_code,
                } = frame
                {
                    if rst_pending || reset_code != ResetCode::SOFTWARE.0 {
                        self.stats.rx_discarded += 1;
                        self.stats.rx_stale_rstack += 1;
                        // UNVERIFIED (EmberZNet 7.4.5 on the Dongle Max:
                        // an RST in its first ~0.2 s after nRESET is lost):
                        // the NCP announced its own boot after our RST went
                        // out, so it was still starting and did not see the
                        // RST. Send it again now (one attempt) instead of
                        // waiting out the RSTACK timeout (3.2 s).
                        if let State::Resetting {
                            attempt,
                            rst_pending: false,
                            ..
                        } = self.state
                            && reset_code != ResetCode::SOFTWARE.0
                            && attempt < self.cfg.reset_retries
                        {
                            self.state = State::Resetting {
                                attempt: attempt + 1,
                                rst_pending: true,
                                deadline: None,
                            };
                        }
                    } else if version != ASH_VERSION {
                        self.fail(FailReason::AshVersion(version));
                    } else {
                        self.restart_link();
                        self.state = State::Connected;
                        self.push_control_event(AshEvent::Connected {
                            version,
                            reset_code: ResetCode(reset_code),
                        });
                    }
                } else {
                    self.stats.rx_discarded += 1;
                }
            }
            State::Connected => match frame {
                // UNVERIFIED: UG101 does not describe an unsolicited RSTACK;
                // it means the NCP rebooted and lost the EZSP state.
                Frame::RstAck { reset_code, .. } => {
                    self.fail(FailReason::NcpReset(ResetCode(reset_code)))
                }
                Frame::Error { code, .. } => self.fail(FailReason::NcpError(ResetCode(code))),
                other => self.on_connected_frame(other, now),
            },
            State::Disconnected | State::Failed(_) => self.stats.rx_discarded += 1,
        }
    }

    fn on_frame_ncp(&mut self, frame: Frame, now: u64) {
        if frame == Frame::Rst {
            // RST is valid in every state, including FAILED (UG101 §3, §6).
            self.restart_link();
            self.events.clear();
            self.data_events = 0;
            self.state = State::Connected;
            self.rstack_pending = Some(ResetCode::SOFTWARE.0);
            self.push_control_event(AshEvent::PeerReset);
            return;
        }
        match self.state {
            State::Connected => self.on_connected_frame(frame, now),
            // FAILED: reply ERROR to every frame except RST (UG101 §6).
            State::Failed(_) => self.error_pending = true,
            _ => self.stats.rx_discarded += 1,
        }
    }

    fn on_connected_frame(&mut self, frame: Frame, now: u64) {
        match frame {
            Frame::Data {
                frm_num,
                re_tx,
                ack_num,
                payload,
            } => {
                if !self.ack_num_valid(ack_num) {
                    self.stats.rx_bad_ack_num += 1;
                    self.set_reject();
                    return;
                }
                // ackNum is processed even when the data is not accepted
                // (UG101 §5 "Receiving DATA Frames").
                self.process_ack(ack_num, now);
                if frm_num == self.frm_rx_expected {
                    if self.data_events < self.cfg.rx_queue_frames {
                        self.frm_rx_expected = inc(frm_num);
                        self.reject = false;
                        self.stats.rx_data += 1;
                        self.events.push_back(AshEvent::Data(payload));
                        self.data_events += 1;
                        self.ctrl = Some(Ctrl::Ack);
                    } else {
                        self.stats.rx_no_space += 1;
                        self.set_reject();
                    }
                } else if re_tx {
                    // Already received: discard, ACK at once, no Reject
                    // Condition (UG101 §5 "Retransmitting").
                    self.stats.rx_duplicate += 1;
                    self.ctrl.get_or_insert(Ctrl::Ack);
                } else {
                    self.stats.rx_out_of_sequence += 1;
                    self.set_reject();
                }
            }
            Frame::Ack { ack_num, not_ready } | Frame::Nak { ack_num, not_ready } => {
                let nak = matches!(frame, Frame::Nak { .. });
                if !self.ack_num_valid(ack_num) {
                    self.stats.rx_bad_ack_num += 1;
                    self.set_reject();
                    return;
                }
                if nak {
                    self.stats.rx_nak += 1;
                } else {
                    self.stats.rx_ack += 1;
                }
                self.process_ack(ack_num, now);
                if self.role == Role::Ncp {
                    self.remote_not_ready_until = not_ready.then_some(now + REMOTE_NOT_READY_MS);
                }
                if nak && !self.window.is_empty() && self.retx.is_none() {
                    // UG101 §5: begin retransmitting from the oldest frame.
                    self.retx = Some(0);
                }
            }
            _ => {}
        }
    }

    /// UG101 §5: a valid ackNum lies between the last received ackNum and
    /// the last transmitted frmNum plus one, both inclusive.
    fn ack_num_valid(&self, ack_num: u8) -> bool {
        within(self.ack_rx, ack_num, self.frm_tx_next)
    }

    fn process_ack(&mut self, ack_num: u8, now: u64) {
        let mut acked = 0;
        while self.ack_rx != ack_num {
            let Some(f) = self.window.pop_front() else {
                break;
            };
            debug_assert_eq!(f.frm_num, self.ack_rx);
            if !f.retransmitted {
                self.update_ack_timeout(now.saturating_sub(f.first_sent));
            }
            self.ack_rx = inc(self.ack_rx);
            acked += 1;
        }
        if acked > 0 {
            self.timeouts = 0;
            self.retx = match self.retx {
                Some(p) if !self.window.is_empty() => Some(p.saturating_sub(acked)),
                _ => None,
            };
        }
    }

    /// UG101 §5 "DATA frame Acknowledgement timing": t = 7/8 t + 1/2
    /// measured, clamped to [T_RX_ACK_MIN, T_RX_ACK_MAX]. Only frames sent
    /// once are measured.
    // UNVERIFIED: UG101 does not say which samples to use; skipping
    // retransmitted frames avoids ambiguous measurements.
    fn update_ack_timeout(&mut self, measured: u64) {
        let t = self.t_rx_ack * 7 / 8 + measured / 2;
        self.t_rx_ack = t.clamp(
            u64::from(self.cfg.ack_timeout_min_ms),
            u64::from(self.cfg.ack_timeout_max_ms),
        );
    }

    /// Runs expired timers: RSTACK wait / RST retry, ACK timeout with
    /// retransmission and timer doubling (UG101 §5).
    pub fn handle_timeout(&mut self, now: u64) {
        self.expire_xoff(now);
        // The ACK timer does not run while XOFF holds our retransmissions.
        let paused = self.tx_paused(now);
        match self.state {
            State::Resetting {
                attempt,
                rst_pending: false,
                deadline: Some(d),
            } if now >= d => {
                if attempt < self.cfg.reset_retries {
                    self.state = State::Resetting {
                        attempt: attempt + 1,
                        rst_pending: true,
                        deadline: None,
                    };
                } else {
                    self.fail(FailReason::ResetTimeout);
                }
            }
            State::Connected => {
                if let Some(front) = self.window.front()
                    && !paused
                    && self.retx != Some(0)
                    && now >= front.last_sent + self.t_rx_ack
                {
                    self.stats.ack_timeouts += 1;
                    self.timeouts = self.timeouts.saturating_add(1);
                    self.t_rx_ack = (self.t_rx_ack * 2).min(u64::from(self.cfg.ack_timeout_max_ms));
                    if self.cfg.max_ack_timeouts != 0 && self.timeouts > self.cfg.max_ack_timeouts {
                        self.fail(FailReason::AckTimeouts);
                        return;
                    }
                    self.retx = Some(0);
                }
                if let Some(t) = self.remote_not_ready_until
                    && now >= t
                {
                    self.remote_not_ready_until = None;
                }
            }
            _ => {}
        }
    }

    /// Earliest time at which [`handle_timeout`](Self::handle_timeout) or
    /// [`poll_transmit`](Self::poll_transmit) has work to do.
    pub fn next_timeout(&self) -> Option<u64> {
        if let Some(t) = self.xoff_until {
            return Some(t);
        }
        match self.state {
            State::Resetting { deadline, .. } => deadline,
            State::Connected => {
                let ack = self.window.front().map(|f| f.last_sent + self.t_rx_ack);
                let nrdy = (self.role == Role::Host
                    && self.local_not_ready()
                    && self.advertised_not_ready)
                    .then(|| self.last_not_ready_tx + u64::from(self.cfg.not_ready_refresh_ms));
                let remote = self.remote_not_ready_until;
                [ack, nrdy, remote].into_iter().flatten().min()
            }
            _ => None,
        }
    }

    /// Appends at most one encoded frame to `out`; returns whether it did.
    /// Call until it returns `false`. Priority: RST/RSTACK/ERROR, ACK/NAK,
    /// retransmissions, new DATA (UG101 §5 "Sending DATA Frames").
    ///
    /// Nothing is sent while an XOFF from the NCP holds the transmitter
    /// (UG101 §4 "XOFF: Stop transmission"), until XON or
    /// [`AshConfig::xoff_timeout_ms`]. A frame already written to the
    /// transport is not recalled: the pause applies between frames.
    pub fn poll_transmit(&mut self, now: u64, out: &mut Vec<u8>) -> bool {
        self.expire_xoff(now);
        if self.tx_paused(now) {
            return false;
        }
        match self.state {
            State::Resetting {
                attempt,
                rst_pending: true,
                ..
            } => {
                // UG101 §4: RST is preceded by a Cancel Byte.
                out.push(CANCEL);
                Frame::Rst.encode(false, out);
                self.stats.tx_rst += 1;
                let deadline = now + u64::from(self.cfg.rstack_timeout_ms);
                self.state = State::Resetting {
                    attempt,
                    rst_pending: false,
                    deadline: Some(deadline),
                };
                true
            }
            State::Connected => self.transmit_connected(now, out),
            State::Failed(FailReason::NcpError(code))
                if self.role == Role::Ncp && self.error_pending =>
            {
                self.error_pending = false;
                Frame::Error {
                    version: ASH_VERSION,
                    code: code.0,
                }
                .encode(false, out);
                true
            }
            _ => false,
        }
    }

    fn transmit_connected(&mut self, now: u64, out: &mut Vec<u8>) -> bool {
        if let Some(code) = self.rstack_pending.take() {
            out.push(CANCEL);
            Frame::RstAck {
                version: ASH_VERSION,
                reset_code: code,
            }
            .encode(false, out);
            return true;
        }
        let next_new = self.next_new_frame(now);
        if let Some(ctrl) = self.ctrl {
            // The NCP may piggyback an ACK on a DATA frame; the host may
            // not (UG101 §5 "Piggybacked Acknowledgements").
            let piggyback = self.role == Role::Ncp
                && ctrl == Ctrl::Ack
                && (self.retx.is_some() || next_new.is_some());
            if !piggyback {
                self.ctrl = None;
                self.send_ctrl(ctrl, now, out);
                return true;
            }
            self.ctrl = None;
        }
        if self.role == Role::Host {
            let not_ready = self.local_not_ready();
            let refresh_due =
                now >= self.last_not_ready_tx + u64::from(self.cfg.not_ready_refresh_ms);
            if (not_ready && (!self.advertised_not_ready || refresh_due))
                || (!not_ready && self.advertised_not_ready)
            {
                self.send_ctrl(Ctrl::Ack, now, out);
                return true;
            }
        }
        if let Some(pos) = self.retx {
            let ack_num = self.frm_rx_expected;
            let f = &mut self.window[pos];
            f.last_sent = now;
            f.retransmitted = true;
            Frame::Data {
                frm_num: f.frm_num,
                re_tx: true,
                ack_num,
                payload: f.payload.clone(),
            }
            .encode(self.cfg.randomize, out);
            self.retx = (pos + 1 < self.window.len()).then_some(pos + 1);
            self.stats.tx_retransmit += 1;
            return true;
        }
        if let Some(i) = next_new {
            let q = self.queue.remove(i).expect("index from next_new_frame");
            let frm_num = self.frm_tx_next;
            self.frm_tx_next = inc(frm_num);
            Frame::Data {
                frm_num,
                re_tx: false,
                ack_num: self.frm_rx_expected,
                payload: q.payload.clone(),
            }
            .encode(self.cfg.randomize, out);
            self.window.push_back(InFlight {
                frm_num,
                payload: q.payload,
                first_sent: now,
                last_sent: now,
                retransmitted: false,
            });
            self.stats.tx_data += 1;
            return true;
        }
        false
    }

    /// Index in the queue of the next new DATA frame allowed by the window
    /// and, for the NCP role, by the host's nRdy (callbacks only).
    fn next_new_frame(&self, now: u64) -> Option<usize> {
        if self.window.len() >= usize::from(self.cfg.tx_window) {
            return None;
        }
        let paused = self.remote_not_ready_until.is_some_and(|t| now < t);
        self.queue.iter().position(|q| !(paused && q.callback))
    }

    fn send_ctrl(&mut self, ctrl: Ctrl, now: u64, out: &mut Vec<u8>) {
        let not_ready = self.role == Role::Host && self.local_not_ready();
        let ack_num = self.frm_rx_expected;
        let frame = match ctrl {
            Ctrl::Ack => {
                self.stats.tx_ack += 1;
                Frame::Ack { ack_num, not_ready }
            }
            Ctrl::Nak => {
                self.stats.tx_nak += 1;
                Frame::Nak { ack_num, not_ready }
            }
        };
        frame.encode(false, out);
        if self.role == Role::Host {
            self.advertised_not_ready = not_ready;
            if not_ready {
                self.last_not_ready_tx = now;
            }
        }
    }
}
