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

pub mod pipe;

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::ash::{Ash, AshConfig, AshEvent, ResetCode};
use crate::ezsp::command::{Callback, Command, Endpoint, Response, id};
use crate::ezsp::frame::{self, ResponseFlags};
use crate::ezsp::types::*;
use crate::ezsp::version::{EzspVersion, STACK_TYPE_MESH, StackVersion};
use crate::host::{Clock, Transport};

pub use pipe::{Fault, Faults, Pipe, PipeStats, Rng};

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
    /// Reported by RSTACK.
    pub reset_code: ResetCode,
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
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            ezsp_version: 18,
            stack_version: StackVersion(0x8230),
            reset_code: ResetCode::SOFTWARE,
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
    ]
    .into_iter()
    .map(|(k, v)| (k.0, v))
    .collect()
}

impl SimNcp {
    pub fn new(cfg: SimConfig) -> SimNcp {
        let mut link = Ash::new_ncp(cfg.ash.clone()).expect("valid simulator ASH config");
        link.set_rst_reply_code(cfg.reset_code);
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

    fn reset_ezsp(&mut self) {
        self.version = None;
        self.config = default_config();
        self.values.clear();
        self.endpoints.clear();
        self.network_up = false;
        self.multicast
            .iter_mut()
            .for_each(|e| *e = MulticastTableEntry::default());
        self.overflow_pending = false;
    }

    fn handle_rx(&mut self, bytes: &[u8], now: u64) {
        self.link.handle_rx(bytes, now);
        while let Some(ev) = self.link.poll_event() {
            match ev {
                AshEvent::PeerReset => self.reset_ezsp(),
                AshEvent::Data(frame) => self.on_ezsp(&frame),
                AshEvent::Connected { .. } | AshEvent::Failed(_) => {}
            }
        }
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
        self.respond(h.sequence, resp);
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
        };
        (resp, callbacks)
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
        if !self.network_up {
            let resp = match resp {
                Response::SendUnicast { .. } => Response::SendUnicast {
                    status: down,
                    sequence: 0,
                },
                Response::SendBroadcast { .. } => Response::SendBroadcast {
                    status: down,
                    sequence: 0,
                },
                _ => Response::SendMulticast {
                    status: down,
                    sequence: 0,
                },
            };
            return (resp, vec![]);
        }
        if len > usize::from(self.cfg.max_payload) {
            let too_long = Self::st(v, Ember, K::MessageTooLong);
            let resp = match resp {
                Response::SendUnicast { .. } => Response::SendUnicast {
                    status: too_long,
                    sequence: 0,
                },
                Response::SendBroadcast { .. } => Response::SendBroadcast {
                    status: too_long,
                    sequence: 0,
                },
                _ => Response::SendMulticast {
                    status: too_long,
                    sequence: 0,
                },
            };
            return (resp, vec![]);
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
        self.ncp.link.handle_timeout(now);
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
            self.ncp.link.next_timeout(),
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
            w.now.store(next, Ordering::SeqCst);
        }
    }
}
