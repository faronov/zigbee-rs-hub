//! The coordinator: request submission, events and status (design §2.2),
//! backend configuration and radio control.

use std::sync::Arc;
use std::time::Duration;

use crate::{
    ApsIndication, Capabilities, CoordError, GpEvent, NetworkOwnership, Progress, Request,
    RequestId, Terminal,
};

/// Something the backend reports. Large variants are boxed: the event
/// queue has bounded, preallocated slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordEvent {
    /// NCP session change.
    Session(Box<SessionEvent>),
    /// Network ownership change.
    Ownership(Box<NetworkOwnership>),
    /// The NCP's network changed state.
    NetworkState(NetChange),
    /// Joining opened or closed.
    PermitJoin {
        /// Seconds left (0 closed).
        remaining_s: u32,
    },
    /// APS data received.
    Indication(ApsIndication),
    /// A device joined or rejoined.
    DeviceJoined {
        /// IEEE.
        ieee: u64,
        /// Short address.
        nwk: u16,
        /// How.
        kind: JoinKind,
    },
    /// The trust center refused a device's join or rejoin.
    JoinDenied {
        /// IEEE.
        ieee: u64,
        /// Short address it was given or asked for.
        nwk: u16,
        /// How it tried.
        kind: JoinKind,
    },
    /// A device left the network (the trust center or the NCP reported
    /// it).
    DeviceLeft {
        /// IEEE.
        ieee: u64,
        /// Short address it had (0xFFFF when unknown).
        nwk: u16,
        /// Whether it announced a rejoin; `None` when the backend does not
        /// know (EZSP's trust center reports a leave without it).
        rejoin: Option<bool>,
        /// The NCP only dropped it from its child table after a timeout
        /// (ZRHP reason 4: an end device missed its End Device Timeout, a
        /// router child stopped sending Link Status): it need not have
        /// left the network.
        aged_out: bool,
    },
    /// A child of the NCP polled it (ZRHP `EVT_CHILD_POLL`): it is awake
    /// now. A hint only: the NCP rate-limits the reports and drops some.
    ChildPoll {
        /// IEEE, when the NCP knows it.
        ieee: Option<u64>,
        /// Short address.
        nwk: u16,
        /// The ACK to the poll carried Frame Pending: the NCP is delivering
        /// a frame it holds for the child.
        frame_pending: bool,
    },
    /// Green Power event.
    Gp(Box<GpEvent>),
    /// [`Progress`] of a request.
    Progress {
        /// The request.
        id: RequestId,
        /// What happened.
        p: Progress,
    },
    /// [`Terminal`] outcome of a request.
    Done {
        /// The request.
        id: RequestId,
        /// The outcome.
        t: Terminal,
    },
    /// Energy scan progress.
    ScanProgress {
        /// Scan pass.
        scan: u8,
        /// Channels done.
        done: u8,
        /// Channels in the scan.
        total: u8,
    },
    /// Events lost since the last delivered one (full queue).
    Dropped {
        /// How many.
        events: u32,
    },
}

/// How a device joined (ZRHP `EVT_DEVICE_JOINED.join_type`, EZSP's trust
/// center join status).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// MAC association (a new join).
    Association,
    /// Secured rejoin (it had the network key).
    SecuredRejoin,
    /// Unsecured rejoin, through the trust center.
    TcRejoin,
    /// Direct join (the NCP added it).
    Direct,
}

/// NCP session change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// A session is ready for requests, and the backend has registered
    /// the hub's endpoints ([`BackendConfig`]).
    Up {
        /// Session epoch: changes with every NCP restart (ZRHP: the peer
        /// cookie).
        epoch: u32,
        /// The NCP.
        info: Arc<NcpInfo>,
        /// What it can do.
        caps: Arc<Capabilities>,
    },
    /// The session ended. Requests in flight still get one terminal each:
    /// backends that lose them across an NCP reset end them with
    /// `Terminal::Unknown(CoordError::DeliveryUnknown)`; ZRHP holds them
    /// and reports their real terminal after the next `Up`.
    Down,
    /// The NCP speaks no supported protocol version.
    Incompatible,
}

/// Facts about the NCP of one session (`network/info.ncp`, backups).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NcpInfo {
    /// Firmware version string.
    pub fw: String,
    /// Protocol version `(major, minor)`.
    pub protocol: (u8, u8),
    /// Firmware build ID.
    pub build_id: [u8; 8],
    /// Backend chip ID (ZRHP: 1 = ESP32-H2).
    pub chip: u8,
    /// IEEE address in use (wire order), if the NCP reported it.
    pub ieee: Option<[u8; 8]>,
    /// NCP flash size in bytes (0 unknown).
    pub flash_size: u32,
    /// Board name.
    pub board: String,
    /// Product name.
    pub product: String,
    /// Host line bit rate (0 for USB).
    pub baud: u32,
    /// RTS/CTS flow control on the host line.
    pub flow_control: bool,
}

/// A change of the NCP's network, as the NCP reports it (the API's
/// `network_state` event). [`NetState`] is the state in a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetChange {
    /// The network went down.
    Down,
    /// Forming or joining.
    Forming,
    /// The network is up.
    Up,
    /// Leaving the network.
    Leaving,
}

/// The NCP's network state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetState {
    /// No network.
    None,
    /// Formed, not up.
    FormedDown,
    /// Up.
    Up,
}

/// Link session state. Variant names mirror `hub_ncp::SessionState`, whose
/// lower-case names are the `network/info` `session` strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// No link session.
    Down,
    /// Link up, negotiating.
    Negotiating,
    /// Negotiated; the backend resumes the NCP.
    Resuming,
    /// Requests flow.
    Ready,
    /// No common protocol version.
    Incompatible,
    /// Backend stopped.
    Stopped,
}

/// Parameters of the running network (`NWK_GET_INFO`, the result of
/// forming one). Mirrors `hub_ncp::lifecycle::NetworkInfo`; the ZRHP
/// adapter maps it (PR E/G).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkParams {
    /// PAN ID.
    pub pan_id: u16,
    /// Extended PAN ID (wire order).
    pub ext_pan_id: [u8; 8],
    /// Channel.
    pub channel: u8,
    /// NWK update ID.
    pub nwk_update_id: u8,
    /// Network key sequence number.
    pub key_seq: u8,
    /// TX power (dBm), if known.
    pub tx_power: Option<i8>,
}

/// Snapshot of the coordinator, taken without I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordStatus {
    /// Link session.
    pub session: SessionState,
    /// Epoch of the current session.
    pub epoch: Option<u32>,
    /// The NCP of the current session.
    pub ncp: Option<Arc<NcpInfo>>,
    /// Its capabilities.
    pub caps: Option<Arc<Capabilities>>,
    /// The NCP's network state.
    pub network_state: NetState,
    /// The running network.
    pub network: Option<NetworkParams>,
    /// Ownership.
    pub ownership: NetworkOwnership,
    /// Permit-join seconds left.
    pub permit_join_s: u32,
}

/// The coordinator as the application thread uses it.
pub trait Coordinator: Send {
    /// Queues a request. Never waits on I/O or a worker; refusals are
    /// immediate. `deadline: None` is the backend's default for the request.
    fn submit(&mut self, req: Request, deadline: Option<Duration>)
    -> Result<RequestId, CoordError>;

    /// Cancels a request that has not reached the NCP yet; it ends with
    /// [`Terminal::Cancelled`].
    fn cancel(&mut self, id: RequestId);

    /// Snapshot behind a short lock, no I/O.
    fn status(&self) -> CoordStatus;

    /// Permit-join seconds left ([`CoordStatus::permit_join_s`]) without
    /// the whole snapshot: the application looks after every input.
    fn permit_join_s(&self) -> u32 {
        self.status().permit_join_s
    }
}

/// Radio control owned by the firmware, called from Wi-Fi callbacks rather
/// than the application thread.
pub trait RadioControl: Send + Sync {
    /// Enables or disables Wi-Fi coexistence (PTA) on the NCP.
    fn set_coex(&self, on: bool) -> Result<(), CoordError>;
}

/// Configuration given to a backend when it is built.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackendConfig {
    /// The hub's endpoints; the backend registers them as its NCP
    /// requires (ZRHP: once, then replayed on every resume).
    pub endpoints: Vec<EndpointConfig>,
}

/// One hub endpoint. Mirrors `hub_ncp::store::EndpointRecord`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointConfig {
    /// Endpoint (1..=240).
    pub ep: u8,
    /// Profile.
    pub profile: u16,
    /// Device ID.
    pub device_id: u16,
    /// Device version.
    pub version: u8,
    /// Input clusters.
    pub in_clusters: Vec<u16>,
    /// Output clusters.
    pub out_clusters: Vec<u16>,
}
