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
use zigbee_zrhp::msg::backup::{
    BkpExportBegin, BkpExportBeginRsp, BkpExportRead, BkpRestoreBegin, BkpRestoreBeginRsp, BkpRestoreCommitRsp, BkpRestorePutRsp, RestoreNote,
};
use zigbee_zrhp::msg::data::{ApsDataRequest, EvtApsDataConfirm};
use zigbee_zrhp::msg::diag::{
    Child, Counter, DiagGetChildren, DiagGetChildrenRsp, DiagGetCounters, DiagGetCountersRsp, DiagGetNeighbors, DiagGetNeighborsRsp, DiagGetRoutes,
    DiagGetRoutesRsp, DiagResetCounters, Neighbor, Route,
};
use zigbee_zrhp::msg::firmware::{FwBegin, FwBeginRsp, FwFinishRsp, FwGetSlotsRsp, FwSlot, FwWrite, FwWriteRsp};
use zigbee_zrhp::msg::network::{
    EnergyResult, EvtChannelChanged, EvtEnergyScanResult, EvtNetworkState, EvtPermitJoinState, NwkActiveScan, NwkActiveScanRsp, NwkChangeChannel,
    NwkEnergyScan, NwkForm, NwkGetInfoRsp, NwkInit, NwkInitRsp, NwkPermitJoin, NwkSetTxPower, NwkSetTxPowerRsp, ScanResult,
};
use zigbee_zrhp::msg::security::{EvtFcCheckpoint, SecGetFrameCountersRsp, SecGetNetworkKeyRsp};
use zigbee_zrhp::msg::system::{Capacity, EvtNcpReady, SysGetInfoRsp, SysGetStatusRsp, SysNegotiateRsp, caps};
use zigbee_zrhp::msg::tables::{TblRead, TblReadEntry, TblReadRsp, TblSyncRsp};
use zigbee_zrhp::msg::{Batch, Kind, RawMessage, ids};
use zigbee_zrhp::queue::EndpointQueues;
use zigbee_zrhp::records::{ChildInfo, DeviceRecord, LinkBlock, Record, device_flags};
use zigbee_zrhp::records::{NetworkKeyRecord, NetworkRecord, NodeRecord, RawRecords, RsParamsRecord, StreamCrc, TcLinkKeyRecord, verify_stream};
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
    /// `Some`: every request from now on, in arrival order, as
    /// `(msg_id, payload)`: first transmissions only (retransmissions are
    /// in `requests`), stalled ones included. Off by default; the hub's
    /// wire baseline test turns it on.
    pub trace: Option<Vec<(u16, Vec<u8>)>>,
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
    /// `DEVICE` table for `TBL_READ`: `(ieee, nwk, capability of a child)`.
    pub table_devices: Vec<([u8; 8], u16, Option<u8>)>,
    /// `DIAG_GET_CHILDREN`: `(ieee, nwk, capability)`; `None` answers
    /// `UNSUPPORTED_COMMAND` (as the master NCP image does).
    pub children: Option<Vec<([u8; 8], u16, u8)>>,
    /// `DIAG_GET_NEIGHBORS`: `(ieee, nwk, device type, relationship)`.
    pub neighbors: Option<Vec<SimNeighbor>>,
    /// No `EVT_PERMIT_JOIN_STATE` (as the zigbee-rs NCP image).
    pub silent_permit_join: bool,
    /// `(dst_nwk, zb_status)`: `APS_DATA_REQUEST`s to these addresses are
    /// confirmed as not delivered.
    pub undeliverable: Vec<(u16, u8)>,
    /// Requests answered `UNSUPPORTED_COMMAND` (to mimic an NCP image that
    /// lacks them, e.g. `NWK_ENERGY_SCAN`).
    pub unsupported: Vec<u16>,
    /// NWK update ID.
    pub nwk_update_id: u8,
    /// TX power (dBm).
    pub tx_power: i8,
    /// Energy per channel 11–26 for `NWK_ENERGY_SCAN`.
    pub energy: [u8; 16],
    /// Delay of `EVT_CHANNEL_CHANGED` after `NWK_CHANGE_CHANNEL`.
    pub channel_delay: Duration,
    /// `DIAG_GET_ROUTES`: `(dst, next_hop, status)`.
    pub routes: Vec<(u16, u16, u8)>,
    /// `DIAG_GET_COUNTERS`: `(id, value)`; the group is the ID's high byte.
    pub counters: Vec<(u16, u32)>,
    /// Remote nodes answering ZDO Mgmt_Lqi_req / Mgmt_Rtg_req sent as APS
    /// data (the over-the-air topology scan).
    pub remotes: Vec<SimRemote>,
    /// Device link keys exported with `DEVICE` records: `(ieee, key)`.
    pub link_keys: Vec<([u8; 8], [u8; 16])>,
    /// Running export `(txn, stream, offset)`.
    pub export: Option<(u16, Vec<u8>, usize)>,
    /// Running restore `(txn, nwk_margin, aps_margin, stream)`.
    pub restore: Option<(u16, u32, u32, Vec<u8>)>,
    /// The last committed restore stream.
    pub restored: Option<Vec<u8>>,
    /// Beacons `NWK_ACTIVE_SCAN` finds: `(channel, PAN, EPID, src, LQI,
    /// RSSI, permit join)`.
    pub beacons: Vec<SimBeacon>,
    /// How long an energy or active scan takes.
    pub scan_time: Duration,
    /// Send the proposed `EVT_SCAN_PROGRESS` (0x018B) after each channel.
    pub scan_progress: bool,
}

/// A network the simulated NCP hears in an active scan.
pub type SimBeacon = (u8, u16, [u8; 8], u16, u8, i8, bool);

/// A router (or any node) of the simulated network answering Mgmt_Lqi_req
/// and Mgmt_Rtg_req over the air.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimRemote {
    /// Its short address.
    pub nwk: u16,
    /// Its IEEE.
    pub ieee: [u8; 8],
    /// Neighbor table: `(ieee, nwk, device type, relationship, lqi)`.
    pub neighbors: Vec<([u8; 8], u16, u8, u8, u8)>,
    /// Routing table: `(dst, next_hop, status)`.
    pub routes: Vec<(u16, u16, u8)>,
}

/// `(ieee, nwk, device type, relationship)` of a simulated neighbor.
pub type SimNeighbor = ([u8; 8], u16, u8, u8);

type Injected = Arc<Mutex<Vec<(u16, Vec<u8>)>>>;

/// Handle to a running simulated NCP.
#[derive(Clone)]
pub struct SimNcp {
    state: Arc<Mutex<SimState>>,
    reboot: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    inject: Injected,
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
            trace: None,
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
            table_devices: Vec::new(),
            children: None,
            neighbors: None,
            silent_permit_join: false,
            undeliverable: Vec::new(),
            unsupported: Vec::new(),
            nwk_update_id: 0,
            tx_power: 8,
            energy: [30, 25, 60, 120, 180, 200, 170, 90, 35, 20, 28, 40, 30, 22, 18, 26],
            channel_delay: Duration::from_millis(50),
            routes: Vec::new(),
            counters: vec![(0x0101, 120), (0x0103, 2), (0x0111, 118), (0x0113, 1), (0x0701, 3), (0x0702, 0), (0x0703, 0)],
            remotes: Vec::new(),
            link_keys: Vec::new(),
            export: None,
            restore: None,
            restored: None,
            beacons: Vec::new(),
            scan_time: Duration::from_millis(20),
            scan_progress: true,
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
                            let Ok(batch) = Batch::parse(&payload) else {
                                continue;
                            };
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
                                    if let Some(t) = s.trace.as_mut().filter(|_| !m.header.retry) {
                                        t.push((id, m.payload.to_vec()));
                                    }
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
        if s.unsupported.contains(&m.header.msg_id) {
            return (status(Status::UnsupportedCommand), actions);
        }
        let rsp = match m.header.msg_id {
            ids::SYS_NEGOTIATE => {
                let capacities = [Capacity { id: 1, value: 4 }, Capacity { id: 7, value: STRIDE }];
                ok(encode!(SysNegotiateRsp {
                    major: 1,
                    minor: 0,
                    ncp_caps: caps::NCP_FW_UPDATE
                        | caps::KEY_EXPORT
                        | caps::UART_FLOW_CONTROL
                        | caps::BACKUP_RESTORE
                        | if s.unsupported.contains(&ids::NWK_ENERGY_SCAN) { 0 } else { caps::ENERGY_SCAN }
                        | if s.unsupported.contains(&ids::NWK_SET_TX_POWER) { 0 } else { caps::TX_POWER },
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
                    actions.push((Duration::from_millis(40), event(&EvtFcCheckpoint { domain: 1, key_seq: 0, durable_limit: s.nwk_limit, generation: 1 })));
                    status(Status::Success)
                }
            }
            // Like the real NCP: the NIB is loaded by NWK_INIT, so a
            // formed-down network reports no PAN, EPID or channel.
            ids::NWK_GET_INFO => ok(encode!(NwkGetInfoRsp {
                state: s.network_state,
                pan_id: if s.network_state == 2 { s.pan_id } else { 0xFFFF },
                ext_pan_id: if s.network_state == 2 { s.ext_pan_id } else { [0; 8] },
                channel: if s.network_state == 2 { s.channel } else { 0 },
                channel_mask: 1 << s.channel,
                short: 0,
                ieee: Ieee([0x5A, 0x51, 0, 0, 0, 0, 0x01, 0x02]),
                nwk_update_id: s.nwk_update_id,
                nwk_manager: 0,
                key_seq: 0,
                security_level: 5,
                tx_power: s.tx_power,
                stack_profile: 2,
            })),
            ids::NWK_ENERGY_SCAN => {
                let r: NwkEnergyScan = m.decode().unwrap();
                let results: Vec<EnergyResult> = (11u8..=26)
                    .filter(|c| r.channel_mask & (1 << c) != 0)
                    .map(|c| {
                        let energy = s.energy[usize::from(c - 11)];
                        EnergyResult { channel: c, max_rssi_dbm: (-100 + i16::from(energy) * 60 / 255) as i8, energy }
                    })
                    .collect();
                if s.scan_progress {
                    let n = results.len() as u32;
                    for (i, c) in results.iter().enumerate() {
                        let p = scan_progress(tid, 0, c.channel, i + 1, n, 0);
                        actions.push((s.scan_time * (i as u32 + 1) / (n + 1), p));
                    }
                }
                actions.push((s.scan_time, event(&EvtEnergyScanResult { req_tid: tid, status: 0, results: List::new(&results) })));
                status(Status::Success)
            }
            ids::NWK_CHANGE_CHANNEL => {
                let r: NwkChangeChannel = m.decode().unwrap();
                if s.network_state != 2 {
                    status(Status::InvalidState)
                } else if !(11..=26).contains(&r.channel) {
                    status(Status::InvalidParameter)
                } else {
                    if r.channel != s.channel {
                        s.nwk_update_id = if r.nwk_update_id == 0 { s.nwk_update_id.wrapping_add(1) } else { r.nwk_update_id };
                        s.channel = r.channel;
                    }
                    s.log.push((m.header.msg_id, m.payload.to_vec()));
                    let ev = EvtChannelChanged { channel: s.channel, nwk_update_id: s.nwk_update_id, req_tid: tid };
                    actions.push((s.channel_delay, event(&ev)));
                    status(Status::Success)
                }
            }
            ids::NWK_ACTIVE_SCAN => {
                let r: NwkActiveScan = m.decode().unwrap();
                // The real response comes after the scan; this thread only
                // holds the sim, not the link.
                let (wait, progress) = (s.scan_time, s.scan_progress);
                let chans: Vec<u8> = (11u8..=26).filter(|c| r.channel_mask & (1 << c) != 0).collect();
                let n = chans.len().max(1) as u32;
                drop(s);
                for (i, &c) in chans.iter().enumerate() {
                    thread::sleep(wait / n);
                    if progress && let Action::Event(id, p) = scan_progress(tid, 1, c, i + 1, n, 0) {
                        self.inject.lock().unwrap().push((id, p));
                    }
                }
                s = self.state.lock().unwrap();
                let found: Vec<ScanResult> = s
                    .beacons
                    .iter()
                    .filter(|b| r.channel_mask & (1 << b.0) != 0)
                    .map(|&(channel, pan, epid, src, lqi, rssi, permit)| ScanResult {
                        channel,
                        pan,
                        epid,
                        src,
                        lqi,
                        rssi,
                        permit: u8::from(permit),
                        stack_profile: 2,
                        update_id: 0,
                    })
                    .collect();
                ok(encode!(NwkActiveScanRsp { results: List::new(&found) }))
            }
            ids::NWK_SET_TX_POWER => {
                let r: NwkSetTxPower = m.decode().unwrap();
                s.tx_power = r.dbm.clamp(-24, 20);
                s.log.push((m.header.msg_id, m.payload.to_vec()));
                ok(encode!(NwkSetTxPowerRsp { applied_dbm: s.tx_power }))
            }
            ids::NWK_LEAVE_RESET => {
                s.network_state = 0;
                s.pan_id = 0;
                s.ext_pan_id = [0; 8];
                s.key = [0; 16];
                s.channel = 0;
                s.log.push((m.header.msg_id, m.payload.to_vec()));
                status(Status::Success)
            }
            ids::SYS_GET_STATUS => ok(encode!(SysGetStatusRsp {
                network_state: s.network_state,
                uptime_s: 3600,
                free_aps_slots: 4,
                host_absent_queue: 0,
                dirty_writebacks: 0,
                restore_in_progress: u8::from(s.restore.is_some()),
                flags: 0,
            })),
            ids::DIAG_GET_COUNTERS => {
                let r: DiagGetCounters = m.decode().unwrap();
                let list: Vec<Counter> = s
                    .counters
                    .iter()
                    .filter(|(id, _)| (id >> 8) >= 1 && (id >> 8) <= 8 && r.group_mask & (1 << ((id >> 8) - 1)) != 0)
                    .map(|&(id, value)| Counter { id, value })
                    .collect();
                ok(encode!(DiagGetCountersRsp { counters: List::new(&list) }))
            }
            ids::DIAG_RESET_COUNTERS => {
                let r: DiagResetCounters = m.decode().unwrap();
                for (id, v) in s.counters.iter_mut() {
                    if (*id >> 8) >= 1 && (*id >> 8) <= 8 && r.group_mask & (1 << ((*id >> 8) - 1)) != 0 {
                        *v = 0;
                    }
                }
                status(Status::Success)
            }
            ids::DIAG_GET_ROUTES => {
                let r: DiagGetRoutes = m.decode().unwrap();
                let entries: Vec<Route> = s
                    .routes
                    .iter()
                    .skip(r.start as usize)
                    .take(2)
                    .map(|&(dst, next_hop, status)| Route { dst, next_hop, status, flags: 0 })
                    .collect();
                ok(encode!(DiagGetRoutesRsp { total: s.routes.len() as u8, entries: List::new(&entries) }))
            }
            ids::BKP_EXPORT_BEGIN => {
                let r: BkpExportBegin = m.decode().unwrap();
                if s.network_state != 2 {
                    status(Status::InvalidState)
                } else {
                    let stream = export_stream(&s, r.flags & 1 != 0);
                    let count = RawRecords::new(&stream).count() as u32;
                    s.export = Some((tid, stream, 0));
                    ok(encode!(BkpExportBeginRsp { txn: tid, record_count: count }))
                }
            }
            ids::BKP_EXPORT_READ => {
                let r: BkpExportRead = m.decode().unwrap();
                match s.export.as_mut().filter(|e| e.0 == r.txn) {
                    None => status(Status::NotFound),
                    Some((_, stream, off)) => {
                        let mut end = *off;
                        for (_, tlv) in RawRecords::new(&stream[*off..]).flatten() {
                            if end - *off + tlv.len() > usize::from(r.max_bytes) {
                                break;
                            }
                            end += tlv.len();
                        }
                        let mut v = vec![0, u8::from(end < stream.len())];
                        v.extend_from_slice(&stream[*off..end]);
                        *off = end;
                        v
                    }
                }
            }
            ids::BKP_EXPORT_END => {
                s.export = None;
                status(Status::Success)
            }
            ids::BKP_RESTORE_BEGIN => {
                let r: BkpRestoreBegin = m.decode().unwrap();
                if s.restore.is_some() {
                    status(Status::InProgress)
                } else {
                    s.restore = Some((tid, r.nwk_margin, r.aps_margin, Vec::new()));
                    ok(encode!(BkpRestoreBeginRsp { txn: tid, max_records_per_put: 12 }))
                }
            }
            ids::BKP_RESTORE_PUT => {
                let txn = u16::from_le_bytes([m.payload[0], m.payload[1]]);
                match s.restore.as_mut().filter(|r| r.0 == txn) {
                    None => status(Status::NotFound),
                    Some(r) => {
                        let before = RawRecords::new(&r.3).count();
                        r.3.extend_from_slice(&m.payload[2..]);
                        let notes: Vec<RestoreNote> = RawRecords::new(&r.3)
                            .flatten()
                            .enumerate()
                            .skip(before)
                            .filter(|(_, (t, _))| *t == zigbee_zrhp::records::record_type::GP_PROXY)
                            .map(|(i, _)| RestoreNote { index: i as u16, note: 3 })
                            .collect();
                        let accepted = RawRecords::new(&r.3).count() as u16;
                        ok(encode!(BkpRestorePutRsp { accepted, notes: List::new(&notes) }))
                    }
                }
            }
            ids::BKP_RESTORE_COMMIT => match s.restore.take() {
                None => status(Status::NotFound),
                Some((_, nwk_margin, aps_margin, stream)) => match verify_stream(&stream) {
                    Err(_) => status(Status::InvalidParameter),
                    Ok(_) => {
                        let mut nwk_fc = 0u32;
                        let mut aps_fc = 0u32;
                        let mut children = 0u16;
                        let mut keys = 0u16;
                        s.table_devices.clear();
                        for (_, tlv) in RawRecords::new(&stream).flatten() {
                            match zigbee_zrhp::codec::Reader::new(tlv).read::<Record>() {
                                Ok(Record::Network(n)) => {
                                    s.pan_id = n.pan_id;
                                    s.ext_pan_id = n.ext_pan_id;
                                    s.channel = n.channel;
                                    s.nwk_update_id = n.nwk_update_id;
                                }
                                Ok(Record::NetworkKey(k)) => {
                                    s.key = k.key;
                                    nwk_fc = nwk_fc.max(k.tx_counter);
                                }
                                Ok(Record::TcLinkKey(k)) => aps_fc = aps_fc.max(k.tx_counter),
                                Ok(Record::RsParams(p)) => {
                                    nwk_fc = nwk_fc.max(p.nwk_fc_limit);
                                    aps_fc = aps_fc.max(p.aps_fc_limit);
                                }
                                Ok(Record::Device(d)) => {
                                    children += u16::from(d.child.is_some());
                                    if let Some(l) = d.link {
                                        keys += 1;
                                        aps_fc = aps_fc.max(l.tx_counter);
                                    }
                                    s.table_devices.push((d.ieee.0, d.nwk, d.child.map(|c| c.capability)));
                                }
                                _ => {}
                            }
                        }
                        s.nwk_limit = s.nwk_limit.max(nwk_fc.saturating_add(nwk_margin));
                        s.aps_limit = s.aps_limit.max(aps_fc.saturating_add(aps_margin));
                        s.network_state = 2;
                        s.restored = Some(stream);
                        ok(encode!(BkpRestoreCommitRsp {
                            nwk_fc_start: s.nwk_limit,
                            aps_fc_start: s.aps_limit,
                            link_keys_on_ncp: keys,
                            link_keys_host_backed: 0,
                            children,
                        }))
                    }
                },
            },
            ids::BKP_RESTORE_ABORT => {
                s.restore = None;
                status(Status::Success)
            }
            ids::NWK_PERMIT_JOIN => {
                let p: NwkPermitJoin = m.decode().unwrap();
                if !s.silent_permit_join {
                    actions.push((Duration::from_millis(5), event(&EvtPermitJoinState { remaining_s: p.duration_s })));
                }
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
                let zb_status = s.undeliverable.iter().find(|u| u.0 == dst).map_or(0, |u| u.1);
                let (st, flags) = if zb_status == 0 { (0, 1) } else { (1, 0) };
                actions.push((
                    Duration::from_millis(10),
                    event(&EvtApsDataConfirm { req_tid: tid, status: st, zb_status, aps_counter: 7, dst_nwk: dst, flags, elapsed_ms: 10 }),
                ));
                if req.profile == 0
                    && let Some(rsp) = s.remotes.iter().find(|r| r.nwk == dst).and_then(|r| remote_zdo(r, req.cluster, req.asdu))
                {
                    actions.push((Duration::from_millis(15), Action::Event(ids::EVT_APS_DATA_INDICATION, rsp)));
                }
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
            ids::TBL_SYNC => {
                ok(encode!(TblSyncRsp { ncp_table_epoch: 0xE0E0_0001, host_acked_rev: 0, dirty: 0, cached_devices: 0, cached_routes: 0, paging_active: 0 }))
            }
            ids::LOG_CONFIG => status(Status::Success),
            // Pages of two, to exercise the cursors.
            ids::TBL_READ => {
                let r: TblRead = m.decode().unwrap();
                let start = r.cursor as usize;
                let entries: Vec<TblReadEntry> = s
                    .table_devices
                    .iter()
                    .skip(start)
                    .take(2)
                    .map(|&(ieee, nwk, cap)| TblReadEntry {
                        rev: 1,
                        state: 0,
                        record: Record::Device(DeviceRecord {
                            ieee: Ieee(ieee),
                            nwk,
                            flags: device_flags::NWK_VALID | if cap.is_some() { device_flags::IS_CHILD } else { 0 },
                            child: cap.map(|capability| ChildInfo { capability, timeout_exp: 8 }),
                            link: None,
                        }),
                    })
                    .collect();
                let end = start + entries.len();
                let next_cursor = if r.table != 5 || end >= s.table_devices.len() { 0xFFFF } else { end as u16 };
                let entries = if r.table == 5 { entries } else { Vec::new() };
                ok(encode!(TblReadRsp { next_cursor, entries: List::new(&entries) }))
            }
            ids::DIAG_GET_CHILDREN => match &s.children {
                None => status(Status::UnsupportedCommand),
                Some(list) => {
                    let r: DiagGetChildren = m.decode().unwrap();
                    let entries: Vec<Child> = list
                        .iter()
                        .skip(r.start as usize)
                        .take(2)
                        .map(|&(ieee, nwk, capability)| Child { ieee: Ieee(ieee), nwk, capability, timeout_exp: 8, last_poll_ms: 100 })
                        .collect();
                    ok(encode!(DiagGetChildrenRsp { total: list.len() as u8, entries: List::new(&entries) }))
                }
            },
            ids::DIAG_GET_NEIGHBORS => match &s.neighbors {
                None => status(Status::UnsupportedCommand),
                Some(list) => {
                    let r: DiagGetNeighbors = m.decode().unwrap();
                    let entries: Vec<Neighbor> = list
                        .iter()
                        .skip(r.start as usize)
                        .take(2)
                        .map(|&(ieee, nwk, device_type, relationship)| Neighbor {
                            ieee: Ieee(ieee),
                            nwk,
                            device_type,
                            rx_on: 1,
                            relationship,
                            lqi: 200,
                            outgoing_cost: 1,
                            age: 0,
                        })
                        .collect();
                    ok(encode!(DiagGetNeighborsRsp { total: list.len() as u8, entries: List::new(&entries) }))
                }
            },
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

/// The export stream of the simulated network (spec 9.5 order).
fn export_stream(s: &SimState, keys: bool) -> Vec<u8> {
    let ieee = Ieee([0x5A, 0x51, 0, 0, 0, 0, 0x01, 0x02]);
    let mut records = vec![
        Record::Node(NodeRecord { ieee, nwk: 0, logical_type: 0, flags: 0 }),
        Record::Network(NetworkRecord {
            pan_id: s.pan_id,
            ext_pan_id: s.ext_pan_id,
            channel: s.channel,
            channel_mask: 1 << s.channel,
            nwk_update_id: s.nwk_update_id,
            nwk_manager: 0,
            security_level: 5,
            tx_power: s.tx_power,
        }),
    ];
    if keys {
        records.push(Record::NetworkKey(NetworkKeyRecord { key: s.key, seq: 0, tx_counter: s.nwk_limit, flags: 1 }));
        records.push(Record::TcLinkKey(TcLinkKeyRecord { key: *b"ZigBeeAlliance09", tc_ieee: ieee, tx_counter: s.aps_limit, flags: 0 }));
    }
    for &(ieee, nwk, cap) in &s.table_devices {
        let link = s.link_keys.iter().find(|(i, _)| *i == ieee).filter(|_| keys).map(|(_, key)| LinkBlock {
            key: *key,
            key_seq: 0,
            attrs: 2,
            origin: 2,
            tx_counter: s.aps_limit,
            rx_floor: 0,
        });
        records.push(Record::Device(DeviceRecord {
            ieee: Ieee(ieee),
            nwk,
            flags: device_flags::NWK_VALID
                | if cap.is_some() { device_flags::IS_CHILD | device_flags::CHILD_INFO } else { 0 }
                | if link.is_some() { device_flags::LINK } else { 0 },
            child: cap.map(|capability| ChildInfo { capability, timeout_exp: 8 }),
            link,
        }));
    }
    records.push(Record::RsParams(RsParamsRecord {
        fc_stride: STRIDE,
        table_epoch: 0xE0E0_0001,
        nwk_fc_limit: s.nwk_limit,
        aps_fc_limit: s.aps_limit,
        flags: 0,
    }));
    let mut buf = vec![0u8; 64 + 64 * records.len()];
    let mut w = zigbee_zrhp::codec::Writer::new(&mut buf);
    let mut crc = StreamCrc::new();
    for r in &records {
        crc.write_record(r, &mut w).unwrap();
    }
    zigbee_zrhp::codec::Wire::write(&Record::End(crc.end()), &mut w).unwrap();
    let n = w.len();
    buf.truncate(n);
    buf
}

/// A remote node's answer to a ZDO request (as `EVT_APS_DATA_INDICATION`
/// payload): Mgmt_Lqi_rsp and Mgmt_Rtg_rsp, three entries per page.
fn remote_zdo(r: &SimRemote, cluster: u16, asdu: &[u8]) -> Option<Vec<u8>> {
    let (&tsn, &start) = (asdu.first()?, asdu.get(1)?);
    let mut zdp = vec![tsn, 0];
    let rsp_cluster: u16 = match cluster {
        0x0031 => {
            zdp.extend([r.neighbors.len() as u8, start]);
            let page: Vec<_> = r.neighbors.iter().skip(usize::from(start)).take(3).collect();
            zdp.push(page.len() as u8);
            for &&(ieee, nwk, dtype, rel, lqi) in &page {
                zdp.extend([0xDD; 8]);
                zdp.extend(ieee);
                zdp.extend(nwk.to_le_bytes());
                let rx_on = if dtype == 2 { 0 } else { 1 };
                zdp.push(dtype & 3 | rx_on << 2 | (rel & 7) << 4);
                zdp.push(2);
                zdp.push(1);
                zdp.push(lqi);
            }
            0x8031
        }
        0x0032 => {
            zdp.extend([r.routes.len() as u8, start]);
            let page: Vec<_> = r.routes.iter().skip(usize::from(start)).take(3).collect();
            zdp.push(page.len() as u8);
            for &&(dst, next_hop, status) in &page {
                zdp.extend(dst.to_le_bytes());
                zdp.push(status & 7);
                zdp.extend(next_hop.to_le_bytes());
            }
            0x8032
        }
        _ => return None,
    };
    let mut p = vec![0x02, 0, 0, 0];
    p.extend(r.nwk.to_le_bytes());
    p.extend(r.ieee);
    p.push(0);
    p.extend(0u16.to_le_bytes());
    p.extend(rsp_cluster.to_le_bytes());
    p.push(0x06);
    p.push(180);
    p.push(-60i8 as u8);
    p.extend(r.nwk.to_le_bytes());
    p.push(0);
    p.push(0);
    p.extend(0u32.to_le_bytes());
    p.extend((zdp.len() as u16).to_le_bytes());
    p.extend(zdp);
    Some(p)
}

/// The proposed `EVT_SCAN_PROGRESS`: `u16 req_tid, u8 scan, u8 channel,
/// u8 done, u8 total, u8 networks_found`.
fn scan_progress(tid: u16, scan: u8, channel: u8, done: usize, total: u32, found: u8) -> Action {
    let mut p = tid.to_le_bytes().to_vec();
    p.extend([scan, channel, done as u8, total as u8, found]);
    Action::Event(0x018B, p)
}

fn event<'a, T: zigbee_zrhp::codec::Wire<'a> + zigbee_zrhp::msg::MessageDef>(body: &T) -> Action {
    let mut b = vec![0u8; 640];
    let n = zigbee_zrhp::codec::encode_into(body, &mut b).unwrap();
    b.truncate(n);
    Action::Event(T::ID, b)
}
