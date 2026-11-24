//! Simulated EmberZNet NCP (feature `sim`): ASH (NCP role of [`Ash`]) and
//! an EZSP command model over an in-memory UART with fault injection, on
//! a virtual clock so that tests are deterministic and fast.
//!
//! The model follows the documents only; it has not been compared with a
//! real NCP. It covers the core stage: reset/RSTACK, version negotiation
//! (legacy first frame), configuration and values, endpoints,
//! `networkInit` with a formed or empty network, `networkState`,
//! `getNetworkParameters`, `permitJoining`, `sendUnicast`/`Broadcast`/
//! `Multicast` with a configurable `messageSentHandler` outcome, the
//! multicast table, injected callbacks, ERROR and spontaneous resets.
//!
//! For the coordinator backend it also plays an EmberZNet trust center:
//! policies, the initial security state, `formNetwork`/`leaveNetwork`,
//! energy and active scans, radio power, neighbor/route/child tables,
//! counters, security manager key export and transient keys, the
//! concentrator, channel change (`energyScanRequest` 0xFE), random
//! numbers, manufacturing tokens and node ID lookup. Faults beyond the
//! UART: `messageSentHandler`s held back ([`SimNcp::hold_message_sent`]),
//! an APS unicast limit (`APS_UNICAST_MESSAGE_COUNT` held messages, then
//! `MAX_MESSAGE_LIMIT_REACHED`), forced buffer exhaustion
//! ([`SimNcp::busy_sends`]), a command left unanswered
//! ([`SimNcp::stall`]) and raw XON/XOFF bytes ([`Sim::to_host_raw`]).
//! [`Sim::set_pace`] ties the virtual clock to real time for hosts that
//! run on their own thread.

pub mod pipe;

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::ash::codec::{XOFF, XON};

use crate::ash::{Ash, AshConfig, AshEvent, ResetCode};
use crate::ezsp::command::{Callback, Command, Endpoint, Response, id};
use crate::ezsp::frame::{self, ResponseFlags};
use crate::ezsp::types::*;
use crate::ezsp::version::{EzspVersion, STACK_TYPE_MESH, StackVersion};
use crate::host::{Clock, Transport};

pub use pipe::{Fault, Faults, Pipe, PipeStats, Rng};

/// A link fault the NCP raises right after its next response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkFault {
    /// Reboot and send RSTACK unprompted.
    Reset(ResetCode),
    /// Enter the FAILED state and send ERROR.
    Error(ResetCode),
}

/// Outcome reported by `messageSentHandler` for unicasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Delivered,
    Failed,
}

/// Simulated NCP parameters.
#[derive(Debug, Clone)]
pub struct SimConfig {
    /// EZSP protocol version of the NCP image (may be < 13 to test refusal).
    pub ezsp_version: u8,
    pub stack_version: StackVersion,
    /// Boot time after RST or a hardware reset. The NCP ignores the UART
    /// while booting and then sends RSTACK (software reset code after RST).
    pub boot_ms: u64,
    /// A network is stored: `networkInit` succeeds.
    pub network_formed: bool,
    pub eui64: Eui64,
    pub node_id: NodeId,
    pub node_type: NodeType,
    pub network: NetworkParameters,
    pub unicast_delivery: Delivery,
    pub max_payload: u8,
    pub multicast_table_size: usize,
    /// NCP-side ASH (UG101 §7 TX_K = 5).
    pub ash: AshConfig,
    /// The network key of the stored network.
    pub network_key: KeyData,
    pub network_key_sequence: u8,
    /// `exportKey` of the network key succeeds (no Secure Vault lock).
    pub key_exportable: bool,
    /// `VERSION_INFO` value: build, major, minor, patch, special, type.
    pub version_info: [u8; 7],
    /// Manufacturing token `BOARD_NAME` (16 bytes).
    pub board_name: [u8; 16],
    /// Manufacturing token `STRING` (16 bytes).
    pub mfg_string: [u8; 16],
    /// Neighbor table (routers).
    pub neighbors: Vec<NeighborTableEntry>,
    /// Route table.
    pub routes: Vec<RouteTableEntry>,
    /// Child table.
    pub children: Vec<ChildData>,
    /// Maximum RSSI per channel reported by an energy scan (others: -90).
    pub energy: Vec<(u8, i8)>,
    /// Beacons an active scan finds.
    pub beacons: Vec<(ZigbeeNetwork, u8, i8)>,
    /// `readCounters` values.
    pub counters: Vec<u16>,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            ezsp_version: 18,
            stack_version: StackVersion(0x8230),
            boot_ms: 0,
            network_formed: true,
            eui64: Eui64::from_u64(0x00_0B_57_FF_FE_12_34_56),
            node_id: 0x0000,
            node_type: NodeType::COORDINATOR,
            network: NetworkParameters {
                extended_pan_id: [0xDD; 8],
                pan_id: 0x1A62,
                radio_tx_power: 8,
                radio_channel: 15,
                join_method: JoinMethod::USE_MAC_ASSOCIATION,
                nwk_manager_id: 0x0000,
                nwk_update_id: 0,
                channels: 1 << 15,
            },
            unicast_delivery: Delivery::Delivered,
            max_payload: 82,
            multicast_table_size: 8,
            ash: AshConfig {
                tx_window: 5,
                tx_queue_frames: 32,
                rx_queue_frames: 4,
                ..AshConfig::default()
            },
            network_key: [0x4B; 16],
            network_key_sequence: 0,
            key_exportable: true,
            version_info: [0x8D, 0x01, 8, 2, 3, 0, 0xAA],
            board_name: *b"SIM-MG24\0\0\0\0\0\0\0\0",
            mfg_string: *b"hub-ezsp sim\xFF\xFF\xFF\xFF",
            neighbors: Vec::new(),
            routes: Vec::new(),
            children: Vec::new(),
            energy: Vec::new(),
            beacons: Vec::new(),
            counters: (0..41).collect(),
        }
    }
}

/// A message the NCP was asked to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMessage {
    pub command: Command,
    pub aps_sequence: u8,
}

/// The NCP model.
#[derive(Debug)]
pub struct SimNcp {
    cfg: SimConfig,
    link: Ash,
    version: Option<EzspVersion>,
    last_sequence: u8,
    config: BTreeMap<u8, u16>,
    values: BTreeMap<u8, Vec<u8>>,
    endpoints: Vec<Endpoint>,
    network_up: bool,
    multicast: Vec<MulticastTableEntry>,
    aps_sequence: u8,
    overflow_pending: bool,
    commands: Vec<u16>,
    sent: Vec<SentMessage>,
    dropped_callbacks: u32,
    booting: Option<(u64, ResetCode)>,
    fault_after_response: Option<LinkFault>,
    pending_fault: Option<LinkFault>,
    policies: BTreeMap<u8, u8>,
    security: Option<InitialSecurityState>,
    transient_keys: Vec<(Eui64, KeyData)>,
    concentrator: Option<Command>,
    /// `messageSentHandler`s held back, and whether new ones are.
    held: Vec<Callback>,
    holding: bool,
    busy_sends: u32,
    stall: Option<u16>,
    random: u16,
    /// Callbacks sent just before the next response.
    early: Vec<Callback>,
}

fn default_config() -> BTreeMap<u8, u16> {
    // Defaults of the UG100 successor §2 "Configuration Values" table.
    [
        (ConfigId::NEIGHBOR_TABLE_SIZE, 16),
        (ConfigId::APS_UNICAST_MESSAGE_COUNT, 10),
        (ConfigId::BINDING_TABLE_SIZE, 2),
        (ConfigId::ADDRESS_TABLE_SIZE, 12),
        (ConfigId::MULTICAST_TABLE_SIZE, 8),
        (ConfigId::ROUTE_TABLE_SIZE, 0),
        (ConfigId::DISCOVERY_TABLE_SIZE, 8),
        (ConfigId::STACK_PROFILE, 2),
        (ConfigId::SECURITY_LEVEL, 5),
        (ConfigId::MAX_HOPS, 30),
        (ConfigId::MAX_END_DEVICE_CHILDREN, 6),
        (ConfigId::INDIRECT_TRANSMISSION_TIMEOUT, 7680),
        (ConfigId::END_DEVICE_POLL_TIMEOUT, 8),
        (ConfigId::TRUST_CENTER_ADDRESS_CACHE_SIZE, 2),
        (ConfigId::SOURCE_ROUTE_TABLE_SIZE, 0),
        (ConfigId::KEY_TABLE_SIZE, 0),
        (ConfigId::APS_ACK_TIMEOUT, 1600),
        (ConfigId::APPLICATION_ZDO_FLAGS, 0),
        (ConfigId::BROADCAST_TABLE_SIZE, 15),
        (ConfigId::SUPPORTED_NETWORKS, 2),
        (ConfigId::TRANSIENT_KEY_TIMEOUT_S, 300),
    ]
    .into_iter()
    .map(|(k, v)| (k.0, v))
    .collect()
}

impl SimNcp {
    pub fn new(cfg: SimConfig) -> SimNcp {
        let link = Ash::new_ncp(cfg.ash.clone()).expect("valid simulator ASH config");
        let multicast = vec![MulticastTableEntry::default(); cfg.multicast_table_size];
        SimNcp {
            cfg,
            link,
            version: None,
            last_sequence: 0,
            config: default_config(),
            values: BTreeMap::new(),
            endpoints: Vec::new(),
            network_up: false,
            multicast,
            aps_sequence: 0,
            overflow_pending: false,
            commands: Vec::new(),
            sent: Vec::new(),
            dropped_callbacks: 0,
            booting: None,
            fault_after_response: None,
            pending_fault: None,
            policies: BTreeMap::new(),
            security: None,
            transient_keys: Vec::new(),
            concentrator: None,
            held: Vec::new(),
            holding: false,
            busy_sends: 0,
            stall: None,
            random: 0x1234,
            early: Vec::new(),
        }
    }

    pub fn config(&self) -> &SimConfig {
        &self.cfg
    }

    pub fn config_mut(&mut self) -> &mut SimConfig {
        &mut self.cfg
    }

    pub fn link(&self) -> &Ash {
        &self.link
    }

    /// Negotiated version, once the host sent `version(own)`.
    pub fn version(&self) -> Option<EzspVersion> {
        self.version
    }

    /// Frame IDs of every command received, in order (duplicates removed
    /// by ASH never appear).
    pub fn commands(&self) -> &[u16] {
        &self.commands
    }

    pub fn sent(&self) -> &[SentMessage] {
        &self.sent
    }

    pub fn endpoints(&self) -> &[Endpoint] {
        &self.endpoints
    }

    pub fn network_up(&self) -> bool {
        self.network_up
    }

    pub fn configuration(&self, id: ConfigId) -> Option<u16> {
        self.config.get(&id.0).copied()
    }

    /// Callbacks lost because the NCP's ASH queue was full.
    pub fn dropped_callbacks(&self) -> u32 {
        self.dropped_callbacks
    }

    /// Decision set for `policy` since the last reset.
    pub fn policy(&self, policy: PolicyId) -> Option<u8> {
        self.policies.get(&policy.0).copied()
    }

    /// The last `setInitialSecurityState`.
    pub fn security_state(&self) -> Option<InitialSecurityState> {
        self.security
    }

    /// Transient link keys imported and not cleared.
    pub fn transient_keys(&self) -> &[(Eui64, KeyData)] {
        &self.transient_keys
    }

    /// The last `setConcentrator`.
    pub fn concentrator(&self) -> Option<&Command> {
        self.concentrator.as_ref()
    }

    /// The multicast table.
    pub fn multicast_table(&self) -> &[MulticastTableEntry] {
        &self.multicast
    }

    /// While `on`, `messageSentHandler`s are held back until
    /// [`release_message_sent`](Self::release_message_sent).
    pub fn hold_message_sent(&mut self, on: bool) {
        self.holding = on;
    }

    /// `messageSentHandler`s held back now.
    pub fn held_message_sent(&self) -> usize {
        self.held.len()
    }

    /// Sends up to `n` held `messageSentHandler`s, oldest first.
    pub fn release_message_sent(&mut self, n: usize) {
        let n = n.min(self.held.len());
        for cb in self.held.drain(..n).collect::<Vec<_>>() {
            self.callback(cb);
        }
    }

    /// The next `n` send commands fail with `MAX_MESSAGE_LIMIT_REACHED`
    /// (packet buffers exhausted).
    pub fn busy_sends(&mut self, n: u32) {
        self.busy_sends = n;
    }

    /// Sends `cb` right before the response to the next command (the NCP
    /// queues a callback while it executes it).
    pub fn callback_before_next_response(&mut self, cb: Callback) {
        self.early.push(cb);
    }

    /// Commands with this frame ID get no response (and have no effect)
    /// until cleared.
    pub fn stall(&mut self, frame_id: Option<u16>) {
        self.stall = frame_id;
    }

    fn reset_ezsp(&mut self) {
        self.version = None;
        self.config = default_config();
        self.values.clear();
        self.endpoints.clear();
        self.network_up = false;
        self.multicast = vec![MulticastTableEntry::default(); self.cfg.multicast_table_size];
        self.overflow_pending = false;
        self.policies.clear();
        self.transient_keys.clear();
        self.concentrator = None;
        self.held.clear();
    }

    fn handle_rx(&mut self, bytes: &[u8], now: u64) {
        if self.booting.is_some() {
            return;
        }
        self.link.handle_rx(bytes, now);
        while let Some(ev) = self.link.poll_event() {
            match ev {
                AshEvent::PeerReset => {
                    self.reset_ezsp();
                    if self.cfg.boot_ms > 0 {
                        self.reboot(now, ResetCode::SOFTWARE);
                        return;
                    }
                }
                AshEvent::Data(frame) => self.on_ezsp(&frame),
                AshEvent::Connected { .. } | AshEvent::Failed(_) => {}
            }
        }
    }

    fn reboot(&mut self, now: u64, code: ResetCode) {
        self.link.ncp_reboot();
        self.booting = Some((now + self.cfg.boot_ms, code));
    }

    fn handle_timeout(&mut self, now: u64) {
        if let Some((t, code)) = self.booting
            && now >= t
        {
            self.booting = None;
            self.link.ncp_spontaneous_reset(code);
        }
        self.link.handle_timeout(now);
    }

    fn next_timeout(&self) -> Option<u64> {
        [self.link.next_timeout(), self.booting.map(|(t, _)| t)]
            .into_iter()
            .flatten()
            .min()
    }

    /// Applies a fault armed with [`fail_after_next_response`](Self::fail_after_next_response)
    /// once the response is on the wire.
    fn after_transmit(&mut self) -> bool {
        match self.pending_fault.take() {
            Some(LinkFault::Reset(code)) => self.spontaneous_reset(code),
            Some(LinkFault::Error(code)) => self.fail(code),
            None => return false,
        }
        true
    }

    /// Raises `fault` right after the response to the next command, so
    /// that both reach the host back to back.
    pub fn fail_after_next_response(&mut self, fault: LinkFault) {
        self.fault_after_response = Some(fault);
    }

    /// Hardware reset (nRESET): the NCP reboots for `boot_ms` and then
    /// sends an unprompted RSTACK with `code` (UG101 §5, AN706 successor §5).
    pub fn hardware_reset(&mut self, now: u64, code: ResetCode) {
        self.reset_ezsp();
        self.reboot(now, code);
    }

    fn on_ezsp(&mut self, bytes: &[u8]) {
        let Ok((h, params)) = frame::parse(bytes) else {
            return;
        };
        if h.control.is_response() {
            return;
        }
        self.last_sequence = h.sequence;
        self.commands.push(h.frame_id);
        if self.stall == Some(h.frame_id) {
            return;
        }
        if h.frame_id == id::VERSION {
            let Some(&desired) = params.first() else {
                return;
            };
            let own = self.cfg.ezsp_version;
            if desired == own {
                self.version = Some(EzspVersion::new(own));
            }
            let resp = Response::Version {
                protocol_version: own,
                stack_type: STACK_TYPE_MESH,
                stack_version: self.cfg.stack_version,
            };
            let params = resp
                .encode_params(EzspVersion::new(own))
                .expect("version response encodes");
            let flags = self.take_flags();
            let out = if h.is_legacy() {
                frame::legacy_version_response(h.sequence, flags, &params)
            } else {
                frame::response(h.sequence, flags, id::VERSION, &params)
            };
            let _ = self.link.send_ncp(&out, false);
            return;
        }
        let Some(v) = self.version else {
            self.respond(
                h.sequence,
                Response::InvalidCommand {
                    reason: EzspStatus::ERROR_VERSION_NOT_SET,
                },
            );
            return;
        };
        let (resp, callbacks) = match Command::decode(h.frame_id, v, params) {
            Ok(Some(cmd)) => self.execute(v, cmd),
            Ok(None) => (
                Response::InvalidCommand {
                    reason: EzspStatus::ERROR_INVALID_FRAME_ID,
                },
                vec![],
            ),
            // UNVERIFIED: the reason a real NCP gives for malformed parameters.
            Err(_) => (
                Response::InvalidCommand {
                    reason: EzspStatus::ERROR_INVALID_VALUE,
                },
                vec![],
            ),
        };
        for cb in std::mem::take(&mut self.early) {
            self.callback(cb);
        }
        self.respond(h.sequence, resp);
        if let Some(fault) = self.fault_after_response.take() {
            self.pending_fault = Some(fault);
            return;
        }
        for cb in callbacks {
            self.callback(cb);
        }
    }

    fn take_flags(&mut self) -> ResponseFlags {
        ResponseFlags {
            overflow: std::mem::take(&mut self.overflow_pending),
            ..ResponseFlags::default()
        }
    }

    fn respond(&mut self, sequence: u8, resp: Response) {
        let v = self
            .version
            .unwrap_or(EzspVersion::new(self.cfg.ezsp_version));
        let params = resp
            .encode_params(v)
            .expect("simulator builds version-correct responses");
        let flags = self.take_flags();
        let out = frame::response(sequence, flags, resp.frame_id(), &params);
        if self.link.send_ncp(&out, false).is_err() {
            self.overflow_pending = true;
        }
    }

    /// Queues an asynchronous callback (UART default mode). If the ASH
    /// queue is full it is lost and the next response carries the
    /// overflow flag, as the UG100 successor §2 "NCP Status" describes.
    pub fn callback(&mut self, cb: Callback) {
        let Some(v) = self.version else { return };
        let params = cb
            .encode_params(v)
            .expect("simulator builds version-correct callbacks");
        let flags = ResponseFlags {
            async_callback: true,
            ..self.take_flags()
        };
        let out = frame::response(self.last_sequence, flags, cb.frame_id(), &params);
        if self.link.send_ncp(&out, true).is_err() {
            self.dropped_callbacks += 1;
            self.overflow_pending = true;
        }
    }

    fn st(v: EzspVersion, family: StatusFamily, kind: StatusKind) -> Status {
        Status::for_version(v, family, kind).expect("status exists for every simulated outcome")
    }

    fn index_error(v: EzspVersion) -> Status {
        if v.uses_sl_status() {
            Status::Sl(SlStatus::INVALID_INDEX)
        } else {
            Status::Ember(EmberStatus::INDEX_OUT_OF_RANGE)
        }
    }

    fn execute(&mut self, v: EzspVersion, cmd: Command) -> (Response, Vec<Callback>) {
        use StatusFamily::{Ember, Ezsp};
        use StatusKind as K;
        let ok_ember = Self::st(v, Ember, K::Success);
        let mut callbacks = Vec::new();
        let resp = match cmd {
            Command::Version { .. } => unreachable!("handled before decoding"),
            Command::GetConfigurationValue { id } => match self.config.get(&id.0) {
                Some(&value) => Response::GetConfigurationValue {
                    status: Self::st(v, Ezsp, K::Success),
                    value,
                },
                None => Response::GetConfigurationValue {
                    status: Self::st(v, Ezsp, K::InvalidId),
                    value: 0,
                },
            },
            Command::SetConfigurationValue { id, value } => {
                let status = if self.network_up {
                    Self::st(v, Ezsp, K::InvalidCall)
                } else if let Some(slot) = self.config.get_mut(&id.0) {
                    *slot = value;
                    if id == ConfigId::MULTICAST_TABLE_SIZE {
                        self.multicast
                            .resize(usize::from(value), MulticastTableEntry::default());
                    }
                    Self::st(v, Ezsp, K::Success)
                } else {
                    Self::st(v, Ezsp, K::InvalidId)
                };
                Response::SetConfigurationValue { status }
            }
            Command::GetValue { id } => match self.values.get(&id.0) {
                Some(value) => Response::GetValue {
                    status: Self::st(v, Ezsp, K::Success),
                    value: value.clone(),
                },
                None if id == ValueId::UART_SYNCH_CALLBACKS => Response::GetValue {
                    status: Self::st(v, Ezsp, K::Success),
                    value: vec![0],
                },
                None if id == ValueId::VERSION_INFO => Response::GetValue {
                    status: Self::st(v, Ezsp, K::Success),
                    value: self.cfg.version_info.to_vec(),
                },
                None if id == ValueId::EXTENDED_SECURITY_BITMASK => Response::GetValue {
                    status: Self::st(v, Ezsp, K::Success),
                    value: vec![0, 0],
                },
                None => Response::GetValue {
                    status: Self::st(v, Ezsp, K::InvalidId),
                    value: vec![],
                },
            },
            Command::SetValue { id, value } => {
                self.values.insert(id.0, value);
                Response::SetValue {
                    status: Self::st(v, Ezsp, K::Success),
                }
            }
            Command::AddEndpoint(e) => {
                let status = if self.network_up {
                    Self::st(v, Ezsp, K::InvalidCall)
                } else if self.endpoints.iter().any(|x| x.endpoint == e.endpoint) {
                    Self::st(v, Ezsp, K::InvalidValue)
                } else {
                    self.endpoints.push(e);
                    Self::st(v, Ezsp, K::Success)
                };
                Response::AddEndpoint { status }
            }
            Command::NetworkInit { .. } => {
                if self.cfg.network_formed {
                    self.network_up = true;
                    callbacks.push(Callback::StackStatus {
                        status: Self::st(v, Ember, K::NetworkUp),
                    });
                    Response::NetworkInit { status: ok_ember }
                } else {
                    Response::NetworkInit {
                        status: Self::st(v, Ember, K::NotJoined),
                    }
                }
            }
            Command::NetworkState => Response::NetworkState {
                state: if self.network_up {
                    NetworkStatus::JOINED_NETWORK
                } else {
                    NetworkStatus::NO_NETWORK
                },
            },
            Command::GetNetworkParameters => {
                if self.network_up {
                    Response::GetNetworkParameters {
                        status: ok_ember,
                        node_type: self.cfg.node_type,
                        parameters: self.cfg.network,
                    }
                } else {
                    // UNVERIFIED: the status a real NCP returns without a network.
                    Response::GetNetworkParameters {
                        status: Self::st(v, Ember, K::NotJoined),
                        node_type: NodeType::UNKNOWN_DEVICE,
                        parameters: NetworkParameters::default(),
                    }
                }
            }
            Command::GetEui64 => Response::GetEui64 {
                eui64: self.cfg.eui64,
            },
            Command::GetNodeId => Response::GetNodeId {
                node_id: self.cfg.node_id,
            },
            Command::PermitJoining { duration } => {
                if self.network_up {
                    let kind = if duration == 0 {
                        K::NetworkClosed
                    } else {
                        K::NetworkOpened
                    };
                    // UNVERIFIED: whether a real NCP reports NETWORK_OPENED/CLOSED here.
                    callbacks.push(Callback::StackStatus {
                        status: Self::st(v, Ember, kind),
                    });
                    Response::PermitJoining { status: ok_ember }
                } else {
                    Response::PermitJoining {
                        status: Self::st(v, Ember, K::NetworkDown),
                    }
                }
            }
            cmd @ (Command::SendUnicast { .. }
            | Command::SendBroadcast { .. }
            | Command::SendMulticast { .. }) => {
                return self.send(v, cmd);
            }
            Command::SetMulticastTableEntry { index, entry } => {
                match self.multicast.get_mut(usize::from(index)) {
                    Some(slot) => {
                        *slot = entry;
                        Response::SetMulticastTableEntry { status: ok_ember }
                    }
                    None => Response::SetMulticastTableEntry {
                        status: Self::index_error(v),
                    },
                }
            }
            Command::GetMulticastTableEntry { index } => {
                match self.multicast.get(usize::from(index)) {
                    Some(entry) => Response::GetMulticastTableEntry {
                        status: ok_ember,
                        entry: *entry,
                    },
                    None => Response::GetMulticastTableEntry {
                        status: Self::index_error(v),
                        entry: MulticastTableEntry::default(),
                    },
                }
            }
            Command::MaximumPayloadLength => Response::MaximumPayloadLength {
                aps_length: self.cfg.max_payload,
            },
            Command::Nop => Response::Nop,
            Command::Raw { .. } => unreachable!("decode never yields Raw"),
            cmd => return self.execute_coordinator(v, cmd),
        };
        (resp, callbacks)
    }

    fn ember(v: EzspVersion, s: EmberStatus) -> Status {
        if v.uses_sl_status() {
            Status::Sl(match s {
                EmberStatus::SUCCESS => SlStatus::OK,
                EmberStatus::INVALID_CALL => SlStatus::INVALID_STATE,
                EmberStatus::NOT_JOINED => SlStatus::NOT_JOINED,
                EmberStatus::INDEX_OUT_OF_RANGE => SlStatus::INVALID_INDEX,
                EmberStatus::NETWORK_DOWN => SlStatus::NETWORK_DOWN,
                _ => SlStatus::FAIL,
            })
        } else {
            Status::Ember(s)
        }
    }

    fn sl(s: SlStatus) -> Status {
        Status::Sl(s)
    }

    /// The coordinator frames (see the module docs).
    // UNVERIFIED: the error statuses a real NCP returns for an unused
    // table index, a missing network or a key it will not export.
    fn execute_coordinator(&mut self, v: EzspVersion, cmd: Command) -> (Response, Vec<Callback>) {
        use EmberStatus as E;
        let ok = Self::ember(v, E::SUCCESS);
        let mut callbacks = Vec::new();
        let resp = match cmd {
            Command::SetPolicy { policy, decision } => {
                self.policies.insert(policy.0, decision);
                Response::SetPolicy {
                    status: Self::st(v, StatusFamily::Ezsp, StatusKind::Success),
                }
            }
            Command::SetInitialSecurityState(state) => {
                self.security = Some(state);
                Response::SetInitialSecurityState { status: ok }
            }
            Command::FormNetwork(p) => {
                if self.network_up {
                    Response::FormNetwork {
                        status: Self::ember(v, E::INVALID_CALL),
                    }
                } else {
                    self.cfg.network = p;
                    self.cfg.network.nwk_manager_id = 0;
                    self.cfg.network_formed = true;
                    self.network_up = true;
                    if let Some(sec) = self.security
                        && sec.bitmask & security_bitmask::HAVE_NETWORK_KEY != 0
                    {
                        self.cfg.network_key = sec.network_key;
                        self.cfg.network_key_sequence = sec.network_key_sequence_number;
                    }
                    callbacks.push(Callback::StackStatus {
                        status: Self::st(v, StatusFamily::Ember, StatusKind::NetworkUp),
                    });
                    Response::FormNetwork { status: ok }
                }
            }
            Command::LeaveNetwork { .. } => {
                if self.network_up {
                    self.network_up = false;
                    self.cfg.network_formed = false;
                    callbacks.push(Callback::StackStatus {
                        status: Self::st(v, StatusFamily::Ember, StatusKind::NetworkDown),
                    });
                    Response::LeaveNetwork { status: ok }
                } else {
                    Response::LeaveNetwork {
                        status: Self::ember(v, E::INVALID_CALL),
                    }
                }
            }
            Command::StartScan {
                scan_type,
                channel_mask,
                ..
            } => {
                let channels = (11..=26u8).filter(|c| channel_mask & (1 << c) != 0);
                if scan_type == ScanType::ENERGY_SCAN {
                    for channel in channels {
                        let max_rssi = self
                            .cfg
                            .energy
                            .iter()
                            .find(|(c, _)| *c == channel)
                            .map_or(-90, |(_, r)| *r);
                        callbacks.push(Callback::EnergyScanResult { channel, max_rssi });
                    }
                } else {
                    for (network, lqi, rssi) in &self.cfg.beacons {
                        if channel_mask & (1 << network.channel) != 0 {
                            callbacks.push(Callback::NetworkFound {
                                network: *network,
                                last_hop_lqi: *lqi,
                                last_hop_rssi: *rssi,
                            });
                        }
                    }
                }
                callbacks.push(Callback::ScanComplete {
                    channel: 0,
                    status: ok,
                });
                Response::StartScan {
                    status: Self::sl(SlStatus::OK),
                }
            }
            Command::StopScan => Response::StopScan { status: ok },
            Command::SetRadioPower { power } => {
                self.cfg.network.radio_tx_power = power;
                Response::SetRadioPower { status: ok }
            }
            Command::GetNeighbor { index } => match self.cfg.neighbors.get(usize::from(index)) {
                Some(entry) => Response::GetNeighbor {
                    status: ok,
                    entry: *entry,
                },
                None => Response::GetNeighbor {
                    status: Self::ember(v, E::INDEX_OUT_OF_RANGE),
                    entry: NeighborTableEntry::default(),
                },
            },
            Command::NeighborCount => Response::NeighborCount {
                count: self.cfg.neighbors.len() as u8,
            },
            Command::GetRouteTableEntry { index } => {
                match self.cfg.routes.get(usize::from(index)) {
                    Some(entry) => Response::GetRouteTableEntry {
                        status: ok,
                        entry: *entry,
                    },
                    None => Response::GetRouteTableEntry {
                        status: Self::ember(v, E::INDEX_OUT_OF_RANGE),
                        entry: RouteTableEntry::default(),
                    },
                }
            }
            Command::GetChildData { index } => match self.cfg.children.get(usize::from(index)) {
                Some(data) => Response::GetChildData {
                    status: ok,
                    data: *data,
                },
                None => Response::GetChildData {
                    status: Self::ember(v, E::NOT_JOINED),
                    data: ChildData::default(),
                },
            },
            Command::ReadCounters => Response::ReadCounters {
                values: self.counters(v),
            },
            Command::ReadAndClearCounters => {
                let values = self.counters(v);
                self.cfg.counters.iter_mut().for_each(|c| *c = 0);
                Response::ReadAndClearCounters { values }
            }
            Command::ExportKey(context) => {
                let found = context.core_key_type == key_type::NETWORK
                    && self.cfg.network_formed
                    && self.cfg.key_exportable;
                let (status, key) = if found {
                    (SlStatus::OK, self.cfg.network_key)
                } else if self.cfg.network_formed && !self.cfg.key_exportable {
                    (SlStatus::NOT_SUPPORTED, [0; 16])
                } else {
                    (SlStatus::NOT_FOUND, [0; 16])
                };
                Response::ExportKey {
                    status: Self::sl(status),
                    key,
                    context: v.simplicity_layout().then_some(context),
                }
            }
            Command::GetNetworkKeyInfo => Response::GetNetworkKeyInfo {
                status: Self::sl(if self.cfg.network_formed {
                    SlStatus::OK
                } else {
                    SlStatus::NOT_FOUND
                }),
                info: NetworkKeyInfo {
                    network_key_set: self.cfg.network_formed,
                    alternate_network_key_set: false,
                    network_key_sequence_number: self.cfg.network_key_sequence,
                    alt_network_key_sequence_number: 0,
                    network_key_frame_counter: 0x1000,
                },
            },
            Command::ImportTransientKey { eui64, key, .. } => {
                self.transient_keys.retain(|(e, _)| *e != eui64);
                self.transient_keys.push((eui64, key));
                Response::ImportTransientKey {
                    status: Self::sl(SlStatus::OK),
                }
            }
            Command::ClearTransientLinkKeys => {
                self.transient_keys.clear();
                Response::ClearTransientLinkKeys
            }
            cmd @ Command::SetConcentrator { .. } => {
                self.concentrator = Some(cmd);
                Response::SetConcentrator { status: ok }
            }
            Command::EnergyScanRequest {
                channels, duration, ..
            } => {
                if !self.network_up {
                    Response::EnergyScanRequest {
                        status: Self::ember(v, E::NETWORK_DOWN),
                    }
                } else {
                    if duration == 0xFE
                        && let Some(ch) = (11..=26u8).find(|c| channels & (1 << c) != 0)
                    {
                        self.cfg.network.radio_channel = ch;
                        self.cfg.network.channels = 1 << ch;
                        self.cfg.network.nwk_update_id =
                            self.cfg.network.nwk_update_id.wrapping_add(1);
                        callbacks.push(Callback::StackStatus {
                            status: Self::st(v, StatusFamily::Ember, StatusKind::ChannelChanged),
                        });
                    }
                    Response::EnergyScanRequest { status: ok }
                }
            }
            Command::GetRandomNumber => {
                self.random = self.random.wrapping_mul(25173).wrapping_add(13849);
                Response::GetRandomNumber {
                    status: ok,
                    value: self.random,
                }
            }
            Command::GetMfgToken { token } => Response::GetMfgToken {
                data: match token {
                    MfgTokenId::BOARD_NAME => self.cfg.board_name.to_vec(),
                    MfgTokenId::STRING => self.cfg.mfg_string.to_vec(),
                    _ => vec![0xFF, 0xFF],
                },
            },
            Command::LookupNodeIdByEui64 { eui64 } => {
                let id = self
                    .cfg
                    .children
                    .iter()
                    .find(|c| c.eui64 == eui64)
                    .map(|c| c.id)
                    .or_else(|| {
                        self.cfg
                            .neighbors
                            .iter()
                            .find(|n| n.long_id == eui64)
                            .map(|n| n.short_id)
                    });
                Response::LookupNodeIdByEui64 {
                    status: v.simplicity_layout().then(|| {
                        Self::sl(if id.is_some() {
                            SlStatus::OK
                        } else {
                            SlStatus::NOT_FOUND
                        })
                    }),
                    node_id: id.unwrap_or(0xFFFE),
                }
            }
            _ => unreachable!("core frames are executed by `execute`"),
        };
        (resp, callbacks)
    }

    /// 41 counters (`EMBER_COUNTER_TYPE_COUNT`) up to EZSP 16, 42 from 17.
    fn counters(&self, v: EzspVersion) -> Vec<u16> {
        let n = if v.get() >= 17 { 42 } else { 41 };
        let mut c = self.cfg.counters.clone();
        c.resize(n, 0);
        c
    }

    fn send(&mut self, v: EzspVersion, cmd: Command) -> (Response, Vec<Callback>) {
        use StatusFamily::Ember;
        use StatusKind as K;
        let down = Self::st(v, Ember, K::NetworkDown);
        let ok = Self::st(v, Ember, K::Success);
        let sequence = self.aps_sequence;
        let (resp, kind, dest, mut aps, tag, len, unicast) = match &cmd {
            Command::SendUnicast {
                kind,
                index_or_destination,
                aps,
                tag,
                message,
            } => (
                Response::SendUnicast {
                    status: ok,
                    sequence,
                },
                *kind,
                *index_or_destination,
                *aps,
                *tag,
                message.len(),
                true,
            ),
            Command::SendBroadcast {
                destination,
                aps,
                tag,
                message,
                ..
            } => (
                Response::SendBroadcast {
                    status: ok,
                    sequence,
                },
                OutgoingMessageType::BROADCAST,
                *destination,
                *aps,
                *tag,
                message.len(),
                false,
            ),
            Command::SendMulticast {
                aps, tag, message, ..
            } => (
                Response::SendMulticast {
                    status: ok,
                    sequence,
                },
                OutgoingMessageType::MULTICAST,
                aps.group_id,
                *aps,
                *tag,
                message.len(),
                false,
            ),
            _ => unreachable!("only send commands"),
        };
        let refuse = |resp: Response, status: Status| -> Response {
            match resp {
                Response::SendUnicast { .. } => Response::SendUnicast {
                    status,
                    sequence: 0,
                },
                Response::SendBroadcast { .. } => Response::SendBroadcast {
                    status,
                    sequence: 0,
                },
                _ => Response::SendMulticast {
                    status,
                    sequence: 0,
                },
            }
        };
        if !self.network_up {
            return (refuse(resp, down), vec![]);
        }
        if len > usize::from(self.cfg.max_payload) {
            let too_long = Self::st(v, Ember, K::MessageTooLong);
            return (refuse(resp, too_long), vec![]);
        }
        // Packet buffers exhausted, or every APS unicast slot waits for
        // its messageSentHandler (`APS_UNICAST_MESSAGE_COUNT`).
        let slots = self
            .configuration(ConfigId::APS_UNICAST_MESSAGE_COUNT)
            .unwrap_or(10);
        let waiting = self
            .held
            .iter()
            .filter(|c| {
                matches!(c, Callback::MessageSent { kind, aps, .. }
                    if *kind == OutgoingMessageType::DIRECT && aps.options.contains(ApsOptions::RETRY))
            })
            .count();
        let retried = matches!(&cmd, Command::SendUnicast { aps, .. } if aps.options.contains(ApsOptions::RETRY));
        if self.busy_sends > 0 || (retried && waiting >= usize::from(slots)) {
            self.busy_sends = self.busy_sends.saturating_sub(1);
            let busy = Self::st(v, Ember, K::MaxMessageLimitReached);
            return (refuse(resp, busy), vec![]);
        }
        self.aps_sequence = self.aps_sequence.wrapping_add(1);
        aps.sequence = sequence;
        let delivered = !unicast || self.cfg.unicast_delivery == Delivery::Delivered;
        let status = Self::st(
            v,
            Ember,
            if delivered {
                K::Success
            } else {
                K::DeliveryFailed
            },
        );
        self.sent.push(SentMessage {
            command: cmd,
            aps_sequence: sequence,
        });
        let cb = Callback::MessageSent {
            status,
            kind,
            index_or_destination: dest,
            aps,
            tag,
            message: vec![],
        };
        if self.holding {
            self.held.push(cb);
            return (resp, vec![]);
        }
        (resp, vec![cb])
    }

    /// An `incomingMessageHandler` in the layout of the negotiated version.
    pub fn incoming_message(
        &self,
        kind: IncomingMessageType,
        aps: ApsFrame,
        sender: NodeId,
        message: &[u8],
    ) -> Callback {
        let wide = self.version.is_some_and(|v| v.alias_fields());
        Callback::IncomingMessage {
            kind,
            aps,
            info: RxPacketInfo {
                sender,
                sender_eui64: wide.then_some(Eui64::default()),
                binding_index: 0xFF,
                address_index: 0xFF,
                last_hop_lqi: 255,
                last_hop_rssi: -40,
                last_hop_timestamp: wide.then_some(0),
            },
            message: message.to_vec(),
        }
    }

    /// The NCP enters the FAILED state and sends ERROR frames (UG101 §6).
    pub fn fail(&mut self, code: ResetCode) {
        self.link.fail_ncp(code);
        self.reset_ezsp();
    }

    /// The NCP reboots by itself and sends RSTACK unprompted.
    pub fn spontaneous_reset(&mut self, code: ResetCode) {
        self.link.ncp_spontaneous_reset(code);
        self.reset_ezsp();
    }
}

#[derive(Debug)]
struct World {
    now: Arc<AtomicU64>,
    /// Virtual milliseconds per real millisecond while the host waits.
    pace: Option<u64>,
    ncp: SimNcp,
    to_ncp: Pipe,
    to_host: Pipe,
    out: Vec<u8>,
    rx: Vec<u8>,
}

impl World {
    fn now(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }

    fn step(&mut self) {
        let now = self.now();
        loop {
            self.rx.resize(512, 0);
            let n = self.to_ncp.read(now, &mut self.rx);
            if n == 0 {
                break;
            }
            let chunk = self.rx[..n].to_vec();
            self.ncp.handle_rx(&chunk, now);
        }
        self.ncp.handle_timeout(now);
        loop {
            self.transmit(now);
            if !self.ncp.after_transmit() {
                break;
            }
        }
    }

    fn transmit(&mut self, now: u64) {
        loop {
            self.out.clear();
            if !self.ncp.link.poll_transmit(now, &mut self.out) {
                break;
            }
            let out = std::mem::take(&mut self.out);
            self.to_host.push(&out, now);
            self.out = out;
        }
    }

    fn next_event(&self) -> Option<u64> {
        [
            self.to_ncp.next_due(),
            self.to_host.next_due(),
            self.ncp.next_timeout(),
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

/// Handle on a simulation: configure faults, drive the NCP model, read
/// the virtual clock.
#[derive(Debug, Clone)]
pub struct Sim {
    world: Arc<Mutex<World>>,
}

/// Host side of the simulated UART.
#[derive(Debug)]
pub struct SimTransport {
    world: Arc<Mutex<World>>,
}

/// Virtual clock shared by the host and the simulator.
#[derive(Debug, Clone)]
pub struct SimClock(Arc<AtomicU64>);

impl Clock for SimClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn lock(world: &Mutex<World>) -> MutexGuard<'_, World> {
    world.lock().unwrap_or_else(|p| p.into_inner())
}

impl Sim {
    /// A simulated NCP with `faults` applied in both directions (each
    /// direction gets its own seed derived from `faults.seed`).
    pub fn new(cfg: SimConfig, faults: Faults) -> (Sim, SimTransport, SimClock) {
        let now = Arc::new(AtomicU64::new(0));
        let world = World {
            now: now.clone(),
            pace: None,
            ncp: SimNcp::new(cfg),
            to_ncp: Pipe::new(faults),
            to_host: Pipe::new(Faults {
                seed: faults.seed ^ 0x5EED_5EED_5EED_5EED,
                ..faults
            }),
            out: Vec::new(),
            rx: Vec::new(),
        };
        let world = Arc::new(Mutex::new(world));
        (
            Sim {
                world: world.clone(),
            },
            SimTransport { world },
            SimClock(now),
        )
    }

    /// Runs `f` on the NCP model.
    pub fn ncp<R>(&self, f: impl FnOnce(&mut SimNcp) -> R) -> R {
        f(&mut lock(&self.world).ncp)
    }

    /// Replaces the faults of both directions.
    pub fn set_faults(&self, faults: Faults) {
        let mut w = lock(&self.world);
        w.to_ncp.set_faults(faults);
        w.to_host.set_faults(Faults {
            seed: faults.seed ^ 0x5EED_5EED_5EED_5EED,
            ..faults
        });
    }

    /// Ties the virtual clock to real time while the host waits for input:
    /// at most `virtual_ms_per_real_ms` virtual milliseconds pass per real
    /// millisecond (`None`: the clock jumps to the next event, the
    /// default). For hosts that run on a thread of their own, so that
    /// tests can act between their reads.
    pub fn set_pace(&self, virtual_ms_per_real_ms: Option<u64>) {
        lock(&self.world).pace = virtual_ms_per_real_ms.map(|p| p.max(1));
    }

    /// Sends raw bytes to the host now, in order with the NCP's frames
    /// and without faults.
    pub fn to_host_raw(&self, bytes: &[u8]) {
        let mut w = lock(&self.world);
        let now = w.now();
        w.to_host.push_raw(bytes, now);
    }

    /// The NCP's UART sends XOFF (software flow control, UG101 §4).
    pub fn xoff(&self) {
        self.to_host_raw(&[XOFF]);
    }

    /// The NCP's UART sends XON.
    pub fn xon(&self) {
        self.to_host_raw(&[XON]);
    }

    /// One-shot fault on the next host → NCP frame.
    pub fn inject_to_ncp(&self, fault: Fault) {
        lock(&self.world).to_ncp.inject(fault);
    }

    /// One-shot fault on the next NCP → host frame.
    pub fn inject_to_host(&self, fault: Fault) {
        lock(&self.world).to_host.inject(fault);
    }

    /// (host → NCP, NCP → host) pipe counters.
    pub fn pipe_stats(&self) -> (PipeStats, PipeStats) {
        let w = lock(&self.world);
        (w.to_ncp.stats(), w.to_host.stats())
    }

    pub fn now_ms(&self) -> u64 {
        lock(&self.world).now()
    }

    /// Hardware-resets the NCP now (see [`SimNcp::hardware_reset`]).
    pub fn hardware_reset(&self, code: ResetCode) {
        let mut w = lock(&self.world);
        let now = w.now();
        w.ncp.hardware_reset(now, code);
    }

    /// Runs the NCP side for `ms` of virtual time without the host reading:
    /// whatever the NCP sends waits in the host's input.
    pub fn advance(&self, ms: u64) {
        let mut w = lock(&self.world);
        let end = w.now() + ms;
        loop {
            w.step();
            let now = w.now();
            if now >= end {
                return;
            }
            let next = [w.to_ncp.next_due(), w.ncp.next_timeout()]
                .into_iter()
                .flatten()
                .filter(|&t| t > now)
                .min()
                .unwrap_or(end)
                .min(end);
            w.now.store(next, Ordering::SeqCst);
        }
    }
}

impl Transport for SimTransport {
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        let mut w = lock(&self.world);
        let now = w.now();
        w.to_ncp.push(data, now);
        Ok(())
    }

    /// Runs the simulation until bytes reach the host or `timeout` of
    /// virtual time has passed.
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        let mut w = lock(&self.world);
        let deadline = w.now() + timeout.as_millis() as u64;
        loop {
            w.step();
            let now = w.now();
            let n = w.to_host.read(now, buf);
            if n > 0 {
                return Ok(n);
            }
            if now >= deadline {
                return Ok(0);
            }
            let next = w.next_event().unwrap_or(deadline).clamp(now + 1, deadline);
            match w.pace {
                None => w.now.store(next, Ordering::SeqCst),
                Some(pace) => {
                    let next = next.min(now + pace);
                    drop(w);
                    std::thread::sleep(Duration::from_millis(1));
                    w = lock(&self.world);
                    // Never backwards: a test may have advanced it meanwhile.
                    w.now.fetch_max(next, Ordering::SeqCst);
                }
            }
        }
    }
}
