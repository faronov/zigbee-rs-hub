//! Simulated ZRHP NCP (feature `sim`): the shared link on the NCP side over
//! an in-memory pipe, with enough of the command set for the host core's
//! tests and for `hubctl --sim`. Not a Zigbee stack: formation, joins and
//! APS confirms are scripted.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use zigbee_zrhp::codec::{Bytes8, List};
use zigbee_zrhp::frame::{ResetReason, TX_BUFFER_LEN};
use zigbee_zrhp::link::{Link, LinkConfig, LinkEvent};
use zigbee_zrhp::msg::data::{ApsDataRequest, EvtApsDataConfirm};
use zigbee_zrhp::msg::firmware::{FwBegin, FwBeginRsp, FwFinishRsp, FwGetSlotsRsp, FwSlot, FwWrite, FwWriteRsp};
use zigbee_zrhp::msg::network::{
    EvtNetworkState, EvtPermitJoinState, NwkForm, NwkGetInfoRsp, NwkInit, NwkInitRsp, NwkPermitJoin,
};
use zigbee_zrhp::msg::security::{EvtFcCheckpoint, SecGetFrameCountersRsp, SecGetNetworkKeyRsp};
use zigbee_zrhp::msg::system::{Capacity, EvtNcpReady, SysGetInfoRsp, SysNegotiateRsp, caps};
use zigbee_zrhp::msg::tables::TblSyncRsp;
use zigbee_zrhp::msg::{Batch, Kind, RawMessage, ids};
use zigbee_zrhp::queue::EndpointQueues;
use zigbee_zrhp::types::{Endpoint, Ieee, Status};

use crate::fw::{sha256, slot_state};
use crate::transport::{PortRx, pipe};

/// Stride of the simulated frame-counter reservations.
pub const STRIDE: u32 = 0x400;

/// Simulated NCP state, inspectable by tests.
#[derive(Debug, Clone)]
pub struct SimState {
    /// Boot cookie.
    pub cookie: u32,
    /// Boots so far.
    pub boots: u32,
    /// 0 none, 1 formed-down, 2 up.
    pub network_state: u8,
    /// PAN ID.
    pub pan_id: u16,
    /// EPID.
    pub ext_pan_id: [u8; 8],
    /// Channel.
    pub channel: u8,
    /// Network key.
    pub key: [u8; 16],
    /// NWK frame-counter durable limit.
    pub nwk_limit: u32,
    /// APS frame-counter durable limit.
    pub aps_limit: u32,
    /// `NWK_INIT` floors received.
    pub floors: Vec<(u32, u32)>,
    /// Request IDs received (with the `RETRY` flag).
    pub requests: Vec<(u16, bool)>,
    /// Host endpoints registered this boot.
    pub endpoints: Vec<u8>,
    /// Slot images `(version, sha256)`; index = slot.
    pub slots: [(u32, [u8; 32]); 2],
    /// Running slot.
    pub running: u8,
    /// Running image is on trial.
    pub trial: bool,
    /// Staging buffer.
    pub staging: Option<(u32, [u8; 32], u32, Vec<u8>)>,
    /// Verified staged image `(version, sha)` waiting for `FW_ACTIVATE`.
    pub staged: Option<(u32, [u8; 32])>,
    /// Slot to boot on trial at the next reboot.
    pub activate: Option<u8>,
    /// Slot to roll back to.
    pub prev: u8,
    /// The trial image crashes at once (the bootloader rolls back).
    pub crash_trial: bool,
    /// Swallow requests with this ID (no response) while set.
    pub stall: Option<u16>,
    /// Retry-cache hits served.
    pub retries_served: u32,
    /// Payloads of every `APS_DATA_REQUEST`.
    pub aps: Vec<Vec<u8>>,
    /// Other requests worth checking: `(msg_id, payload)`.
    pub log: Vec<(u16, Vec<u8>)>,
    /// Next ZCL TSN returned by remote GP requests.
    pub gp_tsn: u8,
}

/// Handle to a running simulated NCP.
#[derive(Clone)]
pub struct SimNcp {
    state: Arc<Mutex<SimState>>,
    reboot: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    inject: Arc<Mutex<Vec<(u16, Vec<u8>)>>>,
}

impl SimNcp {
    /// Start the NCP on `end` of a pipe; the other end is the host's.
    pub fn start(end: pipe::End) -> Self {
        let state = Arc::new(Mutex::new(SimState {
            cookie: 0x5100_0001,
            boots: 1,
            network_state: 0,
            pan_id: 0,
            ext_pan_id: [0; 8],
            channel: 0,
            key: [0; 16],
            nwk_limit: STRIDE,
            aps_limit: STRIDE,
            floors: Vec::new(),
            requests: Vec::new(),
            endpoints: Vec::new(),
            slots: [(0x0001_0000, [0x11; 32]), (0, [0; 32])],
            running: 0,
            trial: false,
            staging: None,
            staged: None,
            activate: None,
            prev: 0,
            crash_trial: false,
            stall: None,
            retries_served: 0,
            aps: Vec::new(),
            log: Vec::new(),
            gp_tsn: 0x40,
        }));
        let inject = Arc::new(Mutex::new(Vec::new()));
        let sim = Self { state, reboot: Arc::new(AtomicBool::new(false)), stop: Arc::new(AtomicBool::new(false)), inject };
        let worker = sim.clone();
        thread::Builder::new().name("sim-ncp".into()).spawn(move || worker.run(end)).expect("sim thread");
        sim
    }

    /// Inspect the state.
    pub fn state(&self) -> SimState {
        self.state.lock().unwrap().clone()
    }

    /// Modify the state.
    pub fn with<R>(&self, f: impl FnOnce(&mut SimState) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    /// Reboot (new cookie; the network survives as formed-down; an
    /// unconfirmed trial image rolls back).
    pub fn reboot(&self) {
        self.reboot.store(true, Ordering::SeqCst);
    }

    /// Send an event (ZIGBEE endpoint) with a raw payload, as if the
    /// network produced it (indications, joins, GP frames).
    pub fn inject_event(&self, msg_id: u16, payload: Vec<u8>) {
        self.inject.lock().unwrap().push((msg_id, payload));
    }

    /// Stop the thread.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    fn boot(&self, s: &mut SimState) {
        s.boots += 1;
        s.cookie = s.cookie.wrapping_mul(0x9E37_79B1).wrapping_add(s.boots) | 1;
        if s.network_state == 2 {
            s.network_state = 1;
        }
        if let Some(slot) = s.activate.take().filter(|_| !s.crash_trial) {
            s.prev = s.running;
            s.running = slot;
            s.trial = true;
        } else if s.trial {
            // Not confirmed: the bootloader returns to the previous slot.
            s.running = s.prev;
            s.trial = false;
        }
        s.endpoints.clear();
        s.staging = None;
        s.nwk_limit += STRIDE;
        s.aps_limit += STRIDE;
    }

    fn run(self, end: pipe::End) {
        let (mut rx, tx) = end.split();
        let clock = Instant::now();
        let now = || clock.elapsed().as_micros() as u64;
        let cookie = self.state().cookie;
        let mut link = Box::new(Link::<16, 16>::new(LinkConfig::ncp(), cookie));
        let mut q: Box<EndpointQueues<4096>> = Box::new(EndpointQueues::new());
        let mut out = [0u8; TX_BUFFER_LEN];
        let mut buf = [0u8; 1024];
        let mut seq = 0u16;
        let mut cache: VecDeque<(u16, u16, Vec<u8>)> = VecDeque::new();
        let mut after: Vec<(Instant, Action)> = Vec::new();
        link.open(now(), ResetReason::PowerOn);
        while !self.stop.load(Ordering::SeqCst) {
            if self.reboot.swap(false, Ordering::SeqCst) {
                let c = self.with(|s| {
                    self.boot(s);
                    s.cookie
                });
                q.clear();
                after.clear();
                cache.clear();
                link.reopen(now(), c, ResetReason::PeerRequested);
            }
            let due: Vec<Action> = {
                let t = Instant::now();
                let (ready, rest): (Vec<_>, Vec<_>) = after.drain(..).partition(|(at, _)| *at <= t);
                after = rest;
                ready.into_iter().map(|(_, a)| a).collect()
            };
            let injected: Vec<Action> =
                self.inject.lock().unwrap().drain(..).map(|(id, p)| Action::Event(id, p)).collect();
            for a in due.into_iter().chain(injected) {
                match a {
                    Action::Reboot => self.reboot(),
                    Action::Event(id, payload) => {
                        seq = seq.wrapping_add(1);
                        let _ = q.queue(Endpoint::Zigbee).raw(Kind::Event, id, seq, &payload);
                    }
                }
            }
            while let Some(n) = link.poll_transmit(now(), &mut *q, &mut out) {
                if tx.send(&out[..n]).is_err() {
                    return;
                }
            }
            let n = match rx.read(&mut buf, Duration::from_millis(2)) {
                Ok(n) => n,
                Err(_) => return,
            };
            let mut off = 0;
            loop {
                off += link.push_rx(&buf[off..n], now());
                let mut frames = Vec::new();
                while let Some(ev) = link.poll_event() {
                    match ev {
                        LinkEvent::Established(_) => frames.push(None),
                        LinkEvent::Data { payload, .. } => frames.push(Some(payload.to_vec())),
                        _ => {}
                    }
                }
                for f in frames {
                    match f {
                        None => {
                            let s = self.state();
                            let ready = EvtNcpReady {
                                ncp_cookie: s.cookie,
                                reset_reason: if s.boots == 1 { 1 } else { 2 },
                                network_state: s.network_state,
                                table_epoch: 0xE0E0_0001,
                                nwk_fc_limit: s.nwk_limit,
                                aps_fc_limit: s.aps_limit,
                                dirty_writebacks: 0,
                                dropped_indications: 0,
                            };
                            seq = 1;
                            let _ = q.event(seq, &ready);
                        }
                        Some(payload) => {
                            let Ok(batch) = Batch::parse(&payload) else { continue };
                            for m in batch.iter() {
                                if m.header.kind != Kind::Request {
                                    continue;
                                }
                                let (id, tid) = (m.header.msg_id, m.header.tid);
                                if m.header.retry
                                    && let Some((_, _, bytes)) =
                                        cache.iter().find(|(t, i, _)| *t == tid && *i == id).cloned()
                                    {
                                        self.with(|s| {
                                            s.requests.push((id, true));
                                            s.retries_served += 1;
                                        });
                                        let ep = zigbee_zrhp::msg::home_endpoint(id).unwrap_or(Endpoint::System);
                                        let _ = q.queue(ep).raw(Kind::Response, id, tid, &bytes);
                                        continue;
                                    }
                                let stalled = self.with(|s| {
                                    s.requests.push((id, m.header.retry));
                                    s.stall == Some(id)
                                });
                                if stalled {
                                    continue;
                                }
                                let (rsp, actions) = self.handle(&m);
                                let ep = zigbee_zrhp::msg::home_endpoint(id).unwrap_or(Endpoint::System);
                                let _ = q.queue(ep).raw(Kind::Response, id, tid, &rsp);
                                cache.push_back((tid, id, rsp));
                                if cache.len() > 64 {
                                    cache.pop_front();
                                }
                                for (delay, a) in actions {
                                    after.push((Instant::now() + delay, a));
                                }
                            }
                        }
                    }
                }
                if off >= n {
                    break;
                }
            }
        }
    }

    fn handle(&self, m: &RawMessage<'_>) -> (Vec<u8>, Vec<(Duration, Action)>) {
        let tid = m.header.tid;
        let ok = |body: Vec<u8>| {
            let mut v = vec![0u8];
            v.extend(body);
            v
        };
        let enc = |body: &dyn Fn(&mut [u8]) -> usize| {
            let mut b = vec![0u8; 640];
            let n = body(&mut b);
            b.truncate(n);
            b
        };
        macro_rules! encode {
            ($v:expr) => {{
                let v = $v;
                enc(&|b: &mut [u8]| zigbee_zrhp::codec::encode_into(&v, b).unwrap())
            }};
        }
        let status = |s: Status| vec![s.to_u8()];
        let mut actions = Vec::new();
        let mut s = self.state.lock().unwrap();
        let rsp = match m.header.msg_id {
            ids::SYS_NEGOTIATE => {
                let capacities = [Capacity { id: 1, value: 4 }, Capacity { id: 7, value: STRIDE }];
                ok(encode!(SysNegotiateRsp {
                    major: 1,
                    minor: 0,
                    ncp_caps: caps::NCP_FW_UPDATE | caps::KEY_EXPORT | caps::UART_FLOW_CONTROL,
                    capacities: List::new(&capacities),
                    fw: Bytes8(b"sim-ncp 0.1.0"),
                    build_id: [0x51; 8],
                    chip: 1,
                }))
            }
            ids::SYS_GET_INFO => ok(encode!(SysGetInfoRsp {
                ieee_factory: Ieee([0x5A, 0x51, 0, 0, 0, 0, 0x01, 0x02]),
                ieee_active: Ieee([0x5A, 0x51, 0, 0, 0, 0, 0x01, 0x02]),
                ieee_source: 0,
                chip_rev: 1,
                flash_size: 4 << 20,
                transport: 2,
                uart_baud: 921_600,
                uart_flow: 1,
                board: Bytes8(b"sim"),
                product: Bytes8(b"sim-ncp"),
            })),
            ids::SYS_ECHO => ok(m.payload.to_vec()),
            ids::SYS_RESET => {
                actions.push((Duration::from_millis(20), Action::Reboot));
                status(Status::Success)
            }
            ids::NWK_INIT => {
                if s.network_state == 0 {
                    status(Status::NotFormed)
                } else {
                    let init: NwkInit = m.decode().unwrap();
                    s.floors.push((init.nwk_fc_floor, init.aps_fc_floor));
                    s.nwk_limit = s.nwk_limit.max(init.nwk_fc_floor);
                    s.aps_limit = s.aps_limit.max(init.aps_fc_floor);
                    let was = s.network_state;
                    s.network_state = 2;
                    if was == 1 {
                        actions.push((Duration::from_millis(5), event(&EvtNetworkState { state: 2, reason: 0, zb_status: 0, req_tid: tid })));
                    }
                    ok(encode!(NwkInitRsp { network_state: was }))
                }
            }
            ids::NWK_FORM => {
                if s.network_state != 0 {
                    status(Status::InvalidState)
                } else {
                    let form: NwkForm = m.decode().unwrap();
                    s.pan_id = form.pan_id;
                    s.ext_pan_id = form.ext_pan_id;
                    s.key = form.network_key;
                    s.channel = (11..=26).find(|c| form.channel_mask & (1 << c) != 0).unwrap_or(11);
                    s.network_state = 2;
                    actions.push((Duration::from_millis(30), event(&EvtNetworkState { state: 2, reason: 0, zb_status: 0, req_tid: tid })));
                    actions.push((
                        Duration::from_millis(40),
                        event(&EvtFcCheckpoint { domain: 1, key_seq: 0, durable_limit: s.nwk_limit, generation: 1 }),
                    ));
                    status(Status::Success)
                }
            }
            ids::NWK_GET_INFO => ok(encode!(NwkGetInfoRsp {
                state: s.network_state,
                pan_id: s.pan_id,
                ext_pan_id: s.ext_pan_id,
                channel: s.channel,
                channel_mask: 1 << s.channel,
                short: 0,
                ieee: Ieee([0x5A, 0x51, 0, 0, 0, 0, 0x01, 0x02]),
                nwk_update_id: 0,
                nwk_manager: 0,
                key_seq: 0,
                security_level: 5,
                tx_power: 0,
                stack_profile: 2,
            })),
            ids::NWK_PERMIT_JOIN => {
                let p: NwkPermitJoin = m.decode().unwrap();
                actions.push((Duration::from_millis(5), event(&EvtPermitJoinState { remaining_s: p.duration_s })));
                status(Status::Success)
            }
            ids::NWK_ADD_ENDPOINT => {
                s.endpoints.push(m.payload.first().copied().unwrap_or(0));
                status(Status::Success)
            }
            ids::APS_DATA_REQUEST => {
                s.aps.push(m.payload.to_vec());
                let req: ApsDataRequest = m.decode().unwrap();
                let dst = match req.dst {
                    zigbee_zrhp::msg::data::ApsDst::Nwk(n) => n,
                    _ => 0xFFFF,
                };
                actions.push((
                    Duration::from_millis(10),
                    event(&EvtApsDataConfirm { req_tid: tid, status: 0, zb_status: 0, aps_counter: 7, dst_nwk: dst, flags: 1, elapsed_ms: 10 }),
                ));
                status(Status::Success)
            }
            ids::SEC_GET_NETWORK_KEY => ok(encode!(SecGetNetworkKeyRsp { key: s.key, seq: 0, nwk_fc_limit: s.nwk_limit })),
            ids::SEC_GET_FRAME_COUNTERS => ok(encode!(SecGetFrameCountersRsp {
                nwk_current: s.nwk_limit - STRIDE,
                nwk_limit: s.nwk_limit,
                aps_current: s.aps_limit - STRIDE,
                aps_limit: s.aps_limit,
                stride: STRIDE,
            })),
            ids::TBL_SYNC => ok(encode!(TblSyncRsp {
                ncp_table_epoch: 0xE0E0_0001,
                host_acked_rev: 0,
                dirty: 0,
                cached_devices: 0,
                cached_routes: 0,
                paging_active: 0,
            })),
            ids::LOG_CONFIG => status(Status::Success),
            ids::NWK_GROUP_MEMBERSHIP | ids::NWK_REMOVE_DEVICE | ids::GP_SINK_CONFIG | ids::GP_SINK_COMMISSIONING | ids::GP_SINK_REMOVE => {
                s.log.push((m.header.msg_id, m.payload.to_vec()));
                status(Status::Success)
            }
            ids::GP_PAIRING_CONFIGURATION..=ids::GP_REMOTE_PROXY_TABLE_REQUEST => {
                s.log.push((m.header.msg_id, m.payload.to_vec()));
                s.gp_tsn = s.gp_tsn.wrapping_add(1);
                vec![0, s.gp_tsn]
            }
            ids::FW_GET_SLOTS => {
                let slots: [FwSlot; 2] = core::array::from_fn(|i| {
                    let (version, sha) = s.slots[i];
                    let state = if i as u8 == s.running {
                        if s.trial { slot_state::RUNNING_TRIAL } else { slot_state::RUNNING }
                    } else if s.staged.is_some() {
                        slot_state::STAGED
                    } else if version != 0 {
                        slot_state::INACTIVE
                    } else {
                        slot_state::EMPTY
                    };
                    FwSlot { slot: i as u8, state, version, sha256: sha }
                });
                ok(encode!(FwGetSlotsRsp { slots: List::new(&slots) }))
            }
            ids::FW_BEGIN => {
                let b: FwBegin = m.decode().unwrap();
                if b.image_size > 0x1E_0000 {
                    status(Status::TooLarge)
                } else {
                    s.staging = Some((b.version, b.sha256, b.image_size, Vec::new()));
                    s.staged = None;
                    ok(encode!(FwBeginRsp { max_chunk: 512, slot: s.running ^ 1 }))
                }
            }
            ids::FW_WRITE => {
                let w: FwWrite = m.decode().unwrap();
                match s.staging.as_mut() {
                    None => status(Status::InvalidState),
                    Some((_, _, size, data)) => {
                        let off = w.offset as usize;
                        let chunk = w.data.0;
                        if off < data.len() {
                            if data.get(off..off + chunk.len()) == Some(chunk) {
                                ok(encode!(FwWriteRsp { next_offset: data.len() as u32 }))
                            } else {
                                status(Status::Conflict)
                            }
                        } else if off > data.len() || data.len() + chunk.len() > *size as usize {
                            status(Status::InvalidParameter)
                        } else {
                            data.extend_from_slice(chunk);
                            ok(encode!(FwWriteRsp { next_offset: data.len() as u32 }))
                        }
                    }
                }
            }
            ids::FW_FINISH => match s.staging.take() {
                Some((version, sha, size, data)) if data.len() == size as usize && sha256(&data) == sha => {
                    let slot = (s.running ^ 1) as usize;
                    s.slots[slot] = (version, sha);
                    s.staged = Some((version, sha));
                    ok(encode!(FwFinishRsp { verify_result: 0 }))
                }
                Some(_) => status(Status::SecurityError),
                None => status(Status::InvalidState),
            },
            ids::FW_ACTIVATE => {
                if s.staged.take().is_some() {
                    s.activate = Some(s.running ^ 1);
                    actions.push((Duration::from_millis(30), Action::Reboot));
                    status(Status::Success)
                } else {
                    status(Status::InvalidState)
                }
            }
            ids::FW_CONFIRM => {
                if s.network_state == 1 {
                    status(Status::InvalidState)
                } else {
                    s.trial = false;
                    status(Status::Success)
                }
            }
            ids::FW_ABORT => {
                s.staging = None;
                status(Status::Success)
            }
            _ => status(Status::UnsupportedCommand),
        };
        (rsp, actions)
    }
}

enum Action {
    Reboot,
    Event(u16, Vec<u8>),
}

fn event<'a, T: zigbee_zrhp::codec::Wire<'a> + zigbee_zrhp::msg::MessageDef>(body: &T) -> Action {
    let mut b = vec![0u8; 640];
    let n = zigbee_zrhp::codec::encode_into(body, &mut b).unwrap();
    b.truncate(n);
    Action::Event(T::ID, b)
}
