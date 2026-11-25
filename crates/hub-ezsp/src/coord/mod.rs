//! The EZSP coordinator backend: `hub-coord`'s [`Coordinator`] and
//! [`NetworkAdmin`] over the ASH + EZSP host of this crate, for an
//! EmberZNet NCP that keeps the network in its own NVM
//! ([`OwnershipModel::NcpNv`]). Design: `docs/coordinator-boundary.md`
//! §2, §3 and §5; choices and gaps: `docs/ezsp-backend.md`.
//!
//! **Status: host-tested against the simulated NCP only.**
//!
//! # Threading (design §3)
//!
//! One worker thread (`ezsp-worker`) owns the [`EzspHost`] and therefore
//! the UART. [`EzspCoordinator`]'s `submit` checks the session and the
//! request, queues it and returns its [`RequestId`]; it never waits.
//! Events go to the application's bounded [`EventQueue`] with
//! [`EventQueue::try_push`]: while the application is behind, the worker
//! keeps acknowledging ASH frames but stops reading callbacks, so the
//! host's queue fills and ASH not-ready holds the NCP's callbacks (UG101
//! §5) instead of the NCP declaring the link failed. [`EzspAdmin`] (job
//! threads) hands its work to the worker and waits for the result.
//!
//! # Session
//!
//! Each session starts with an ASH reset and the version exchange
//! ([`EzspHost::start`]), then the configuration values
//! ([`EzspOptions::config`], which the NCP accepts only before the
//! network is initialized), policies, the hub endpoints (`addEndpoint`
//! only before `networkInit`), `networkInit` (the NCP resumes the network
//! in its NVM, or answers `NOT_JOINED`) and, with a network up, the
//! concentrator, the TX power the operator set and the multicast table
//! (RAM on the NCP) are applied again. Then [`CoordEvent::Ownership`]
//! (the NCP's [`NetworkIdentity`], with the key ID when the network key
//! can be exported), [`SessionEvent::Up`] with a new epoch (a counter:
//! every session follows an NCP reset) and, with a network up,
//! [`NetChange::Up`]. Callbacks the NCP sent meanwhile follow.
//!
//! An unprompted RSTACK, an ASH ERROR or a link failure ends the session:
//! [`SessionEvent::Down`], then every request on the NCP (sent, without
//! its `messageSentHandler`) ends `Unknown(DeliveryUnknown)` (design §2.2:
//! EZSP loses them across a reset). Queued requests stay queued and go out
//! after the next `Up`. The worker starts a new session at once and then
//! with a growing delay ([`EzspOptions::retry_min`] to
//! [`EzspOptions::retry_max`]).
//!
//! # Requests (design §5)
//!
//! | Request | EZSP |
//! |---|---|
//! | APS to a short address | `sendUnicast` (DIRECT; APS option `RETRY` when acknowledged) |
//! | APS to an IEEE address | the same, short address from joins, device announcements or `lookupNodeIdByEui64` |
//! | APS broadcast (0xFFFC/D/F) | `sendBroadcast` |
//! | APS group | `sendMulticast` |
//! | group membership | `setMulticastTableEntry`, replayed in every session |
//! | permit join | transient ZigBeeAlliance09 key, trust center policy, `permitJoining`, ZDO Mgmt_Permit_Joining_req |
//! | device removal | the address is forgotten; the engine sends Mgmt_Leave_req itself (EZSP `removeDevice` as a force removal: follow-up) |
//! | device inventory | child table and neighbor table |
//! | Green Power | refused ([`CoordError::Unsupported`]) until the GP follow-up |
//!
//! The message tag of each send maps to its [`RequestId`]. The response
//! (`sendUnicast` status) is [`Progress::Accepted`] or the terminal
//! `Refused` (`MAX_MESSAGE_LIMIT_REACHED`, `NO_BUFFERS`, `NETWORK_BUSY`:
//! `Refused(Busy)`); `messageSentHandler` is the terminal: `Delivered`
//! only for a unicast that asked for an APS ACK, `Sent` otherwise,
//! `NotDelivered` on failure. A request whose `messageSentHandler` does not
//! come in time ([`EzspOptions::confirm_timeout`]) ends `Unknown(Timeout)`;
//! its tag stays reserved for the late callback. Acknowledged unicasts in
//! flight are limited to the NCP's `APS_UNICAST_MESSAGE_COUNT`
//! ([`Capabilities::max_in_flight`]); more wait in the queue, in order.
//!
//! Every request except APS data is a barrier: requests submitted later
//! reach the NCP only after it completed (the worker runs the queue in
//! order, one control request at a time).
//!
//! # Room for the follow-ups
//!
//! - Green Power: `gpepIncomingMessageHandler` and `dGpSentHandler`
//!   arrive as [`Callback::Unknown`] and go to the worker's GP path
//!   (counted, not translated yet); [`Request::Gp`] is refused at submit;
//!   the GP sink and proxy table sizes are configuration values
//!   (`GP_SINK_TABLE_SIZE`, `GP_PROXY_TABLE_SIZE`).
//! - NCP firmware update (Gecko bootloader): an admin job of the same
//!   shape as backup and restore.
//!
//! # Backup and restore
//!
//! [`EzspAdmin::backup_export`](hub_coord::NetworkAdmin::backup_export)
//! reads the network, its keys and frame counters through the security
//! manager ([`SecManContext`]: `exportKey`, `getNetworkKeyInfo`, the
//! hashed TC link key root, `exportLinkKeyByIndex`), `getValue`, the child
//! table; the restore ([`BlobEncoder`], `restore_check`, `backup_restore`)
//! writes the coordinator IEEE (a rewritable stack token, or the one-time
//! custom EUI-64 only when confirmed), the frame counters plus margins,
//! the initial security state with `NO_FRAME_COUNTER_RESET`, the key and
//! child tables and forms the network, with NCP resets the worker runs
//! from its loop (`docs/ezsp-backend.md` "Backup and restore").
//!
//! [`Callback::Unknown`]: crate::ezsp::Callback::Unknown
//! [`SecManContext`]: crate::ezsp::SecManContext
//! [`OwnershipModel::NcpNv`]: hub_coord::OwnershipModel::NcpNv
//! [`Coordinator`]: hub_coord::Coordinator
//! [`NetworkAdmin`]: hub_coord::NetworkAdmin
//! [`EventQueue`]: hub_coord::EventQueue
//! [`EventQueue::try_push`]: hub_coord::EventQueue::try_push
//! [`RequestId`]: hub_coord::RequestId
//! [`CoordEvent::Ownership`]: hub_coord::CoordEvent::Ownership
//! [`NetworkIdentity`]: hub_coord::NetworkIdentity
//! [`SessionEvent::Up`]: hub_coord::SessionEvent::Up
//! [`SessionEvent::Down`]: hub_coord::SessionEvent::Down
//! [`NetChange::Up`]: hub_coord::NetChange::Up
//! [`Progress::Accepted`]: hub_coord::Progress::Accepted
//! [`Request::Gp`]: hub_coord::Request::Gp
//! [`CoordError::Unsupported`]: hub_coord::CoordError::Unsupported
//! [`Capabilities::max_in_flight`]: hub_coord::Capabilities::max_in_flight
//! [`EzspHost`]: crate::host::EzspHost
//! [`EzspHost::start`]: crate::host::EzspHost::start

mod admin;
mod backup;
#[cfg(test)]
pub(crate) mod contract;
mod convert;
#[cfg(test)]
mod tests;
mod worker;

use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::ezsp::types::*;
use crate::host::{HostConfig, LinkCounters};

pub use admin::EzspAdmin;
pub use backup::BlobEncoder;
pub use convert::{COUNTERS, capabilities, energy_of_rssi, ezsp_error, host_error};
pub use worker::{COMMAND_CAP, EzspCoordinator};

/// Configuration values set at the start of every session, before
/// `networkInit`, in this order. The tables (capacities: the NCP's RAM)
/// come last, by priority: what the NCP's memory does not hold is fitted
/// (the most that fits, at least the NCP's own value; the worker logs it
/// once and keeps it for later sessions), so the source route table, the
/// largest and needed only by bigger networks, takes what is left. The
/// worker reads the values back and reports what the NCP took in the
/// [`Capabilities`](hub_coord::Capabilities). `PACKET_BUFFER_COUNT` is
/// not set: EmberZNet 7.4 sizes packet buffers at build time ("The NCP no
/// longer supports configuration of packet buffer count at runtime", Gecko
/// SDK 4.4.6 `ezsp-enum.h`).
pub const DEFAULT_CONFIG: &[(ConfigId, u16)] = &[
    // One network: the RAM goes to the tables below.
    (ConfigId::SUPPORTED_NETWORKS, 1),
    // ZigBee PRO, network security level 5 (the UG100 defaults, explicit).
    (ConfigId::STACK_PROFILE, 2),
    (ConfigId::SECURITY_LEVEL, 5),
    // Supported ZDO requests reach the host too (device announcements);
    // the NCP still answers them.
    (
        ConfigId::APPLICATION_ZDO_FLAGS,
        zdo_flags::APP_RECEIVES_SUPPORTED_ZDO_REQUESTS,
    ),
    // Spec defaults for sleepy children: 7.68 s indirect hold, 2^8 min
    // poll timeout for end devices that do not negotiate one.
    (ConfigId::INDIRECT_TRANSMISSION_TIMEOUT, 7680),
    (ConfigId::END_DEVICE_POLL_TIMEOUT, 8),
    (ConfigId::MAX_HOPS, 30),
    (ConfigId::TRANSIENT_KEY_TIMEOUT_S, 300),
    // Tables, by priority. The hub endpoint's group memberships.
    (ConfigId::MULTICAST_TABLE_SIZE, 16),
    (ConfigId::ADDRESS_TABLE_SIZE, 16),
    // Near-simultaneous joins the trust center can process (default 2).
    (ConfigId::TRUST_CENTER_ADDRESS_CACHE_SIZE, 4),
    (ConfigId::MAX_END_DEVICE_CHILDREN, 32),
    // Acknowledged unicasts in flight (default 10).
    (ConfigId::APS_UNICAST_MESSAGE_COUNT, 32),
    (ConfigId::NEIGHBOR_TABLE_SIZE, 16),
    (ConfigId::BROADCAST_TABLE_SIZE, 15),
    // Source routes are kept by the NCP (high-RAM concentrator from route
    // records); the host keeps none.
    (ConfigId::SOURCE_ROUTE_TABLE_SIZE, 200),
];

/// Policies set at the start of every session (`setPolicy`). The trust
/// center policy is the closed one: joins are allowed only while joining
/// is permitted ([`tc_policy::ALLOW_JOINS`] is added then, as Silabs'
/// `network-creator-security` plugin does when it opens the network,
/// Gecko SDK 4.4.6).
pub const DEFAULT_POLICIES: &[(PolicyId, u8)] = &[
    // Devices that missed a network key update may rejoin through the
    // trust center; new joins need an open network.
    (
        PolicyId::TRUST_CENTER_POLICY,
        tc_policy::ALLOW_UNSECURED_REJOINS,
    ),
    // Zigbee 3.0 devices request a trust center link key after joining;
    // with hashed link keys the "current key" is unique per device (the
    // plugin sets this on NETWORK_UP of a centralized network).
    (
        PolicyId::TC_KEY_REQUEST_POLICY,
        DecisionId::ALLOW_TC_KEY_REQUESTS_AND_SEND_CURRENT_KEY.0,
    ),
    // The hub brokers no application link keys between devices.
    (
        PolicyId::APP_KEY_REQUEST_POLICY,
        DecisionId::DENY_APP_KEY_REQUESTS.0,
    ),
    // messageSentHandler carries the tag only: less UART traffic.
    (
        PolicyId::MESSAGE_CONTENTS_IN_CALLBACK_POLICY,
        DecisionId::MESSAGE_TAG_ONLY_IN_CALLBACK.0,
    ),
];

/// How long a session's start took, phase by phase (ms).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionTimes {
    /// Input discarded, RST, RSTACK.
    pub reset_ms: u32,
    /// The `version` exchange(s).
    pub version_ms: u32,
    /// `VERSION_INFO`, the configuration values set and read back.
    pub config_ms: u32,
    /// Policies and endpoints.
    pub setup_ms: u32,
    /// `networkInit` and the wait for the network.
    pub network_ms: u32,
    /// EUI-64, manufacturing tokens, groups, network parameters,
    /// concentrator, TX power.
    pub rest_ms: u32,
    /// From the reset to the session's announcement.
    pub total_ms: u32,
}

impl std::fmt::Display for SessionTimes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ms (reset {}, version {}, config {}, policies+endpoints {}, network {}, rest {})",
            self.total_ms,
            self.reset_ms,
            self.version_ms,
            self.config_ms,
            self.setup_ms,
            self.network_ms,
            self.rest_ms
        )
    }
}

/// A configuration value the NCP took less of than asked
/// ([`EzspOptions::config`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fitted {
    pub id: ConfigId,
    /// Asked.
    pub asked: u16,
    /// What the NCP took (its own value when nothing more fitted).
    pub took: u16,
}

/// The link as the worker last reported it ([`EzspOptions::link`]):
/// refreshed at every session start and end and about once a second.
#[derive(Debug, Clone, Default)]
pub struct LinkReport {
    /// Sessions announced.
    pub sessions: u32,
    /// Session starts that failed (no RSTACK, link failure, refused setup).
    pub failed_starts: u32,
    /// The session is up.
    pub up: bool,
    /// The last announced session's start.
    pub start: Option<SessionTimes>,
    /// Configuration values that did not fit the NCP's memory.
    pub fitted: Vec<Fitted>,
    /// ASH, host and UART counters (since the worker started).
    pub counters: LinkCounters,
}

/// The [`EzspOptions::traffic`] value of session `epoch`, closed; `| 1`
/// opens it.
pub fn traffic_closed(epoch: u32) -> u32 {
    epoch.wrapping_shl(1)
}

/// Parameters of the EZSP backend.
#[derive(Debug, Clone)]
pub struct EzspOptions {
    /// ASH and EZSP host parameters (versions, timeouts, queues).
    pub host: HostConfig,
    /// UART bit rate, for [`NcpInfo::baud`](hub_coord::NcpInfo::baud).
    pub baud: u32,
    /// RTS/CTS on the UART (`false`: none, or XON/XOFF).
    pub flow_control: bool,
    /// Configuration values ([`DEFAULT_CONFIG`]).
    pub config: Vec<(ConfigId, u16)>,
    /// Policies ([`DEFAULT_POLICIES`]).
    pub policies: Vec<(PolicyId, u8)>,
    /// Make the coordinator a high-RAM concentrator (many-to-one route
    /// requests every 10–60 s, the timings of Silabs' `concentrator`
    /// plugin, Gecko SDK 4.4.6).
    pub concentrator: bool,
    /// Longest wait for one slice of UART input: bounds how late a
    /// submitted request is noticed.
    pub poll: Duration,
    /// How long a request on the NCP waits for its `messageSentHandler`
    /// (`deadline: None` of `Coordinator::submit`).
    pub confirm_timeout: Duration,
    /// How long `networkInit`, `formNetwork` and `leaveNetwork` may take to
    /// report the network up or down.
    pub network_up_timeout: Duration,
    /// First delay before a new session after a failed start.
    pub retry_min: Duration,
    /// Longest delay between session attempts.
    pub retry_max: Duration,
    /// Stack of the worker thread.
    pub worker_stack: usize,
    /// The TX power the operator set in an earlier run (the hub keeps it),
    /// applied in every session until it is changed.
    pub tx_power: Option<i8>,
    /// For an application that decides itself whether the NCP's network is
    /// its own (`NcpNv`): `(epoch << 1) | open`. The worker stores
    /// `epoch << 1` (closed) when a session starts, before it reports the
    /// session's ownership, and 0 when the session ends; requests go out
    /// only while it holds this session's epoch with the open bit. The
    /// application opens it with `compare_exchange(epoch << 1, epoch << 1 |
    /// 1)` for the epoch whose network it checked ([`traffic_closed`]), so a
    /// late decision about an earlier session cannot open a later one.
    /// Requests queued while it is closed end `Refused(LinkDown)` instead of
    /// going out in the next session, which may run another network (those
    /// queued when a session ends at once, others after
    /// [`traffic_hold`](Self::traffic_hold)). `None`: queued requests wait
    /// for the next session.
    pub traffic: Option<Arc<AtomicU32>>,
    /// With [`traffic`](Self::traffic): how long after the worker starts or
    /// a session ends the requests queued meanwhile wait for the
    /// application's decision about the next session's network, instead of
    /// ending `Refused(LinkDown)` at once. They go out only once it opens
    /// the flag for that session. Zero: no wait.
    pub traffic_hold: Duration,
    /// Where the worker reports the link ([`LinkReport`]), for a console
    /// or a status page of the firmware.
    pub link: Option<Arc<Mutex<LinkReport>>>,
}

impl Default for EzspOptions {
    fn default() -> Self {
        Self {
            host: HostConfig::default(),
            baud: 115_200,
            flow_control: false,
            config: DEFAULT_CONFIG.to_vec(),
            policies: DEFAULT_POLICIES.to_vec(),
            concentrator: true,
            poll: Duration::from_millis(10),
            confirm_timeout: Duration::from_secs(60),
            network_up_timeout: Duration::from_secs(10),
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(30),
            worker_stack: 16 * 1024,
            tx_power: None,
            traffic: None,
            traffic_hold: Duration::ZERO,
            link: None,
        }
    }
}
