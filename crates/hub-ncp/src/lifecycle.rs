//! NCP lifecycle: what the hub does with every session, and the hub-level
//! operations on top of the [`Client`].
//!
//! On every [`DriverEvent::SessionUp`] the lifecycle thread runs the
//! **seamless resume** (spec 12.1/12.4) while normal requests are held:
//!
//! 1. UART check: an NCP without RTS/CTS turns the host's flow control off
//!    (spec 11.5, mismatch handling);
//! 2. store identity (`ncp_ieee`), NCP cookie and `table_epoch`;
//! 3. network: adopt the NCP's network into an empty store, refuse to touch
//!    a network that does not match the stored one, `NWK_INIT` with the
//!    frame-counter floors of the checkpoint log (`last + 9 × fc_stride`);
//! 4. replay: host endpoints, the open permit-join remainder;
//! 5. `TBL_SYNC` with the store's epochs;
//! 6. NCP firmware: confirm the hub's own trial image (`FW_CONFIRM` after
//!    the resume), report a rollback;
//! 7. open the gate ([`Client::resume_done`]).
//!
//! NCP events update the store (checkpoints, device registry, table epoch)
//! and are forwarded to subscribers.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use log::{error, info, warn};
use zigbee_zrhp::codec::List;
use zigbee_zrhp::msg::data::{ApsDataRequest, EvtApsDataConfirm};
use zigbee_zrhp::msg::ids;
use zigbee_zrhp::msg::log::EvtLogRecord;
use zigbee_zrhp::msg::network::{
    EvtAddressChanged, EvtDeviceAuthorized, EvtDeviceJoined, EvtDeviceLeft, EvtNetworkState, EvtPermitJoinState,
    NwkAddEndpoint, NwkForm, NwkGetInfoRsp, NwkInit, NwkInitRsp, NwkPermitJoin,
};
use zigbee_zrhp::msg::security::{EvtFcCheckpoint, SecGetFrameCountersRsp, SecGetNetworkKey, SecGetNetworkKeyRsp};
use zigbee_zrhp::msg::system::{EvtFatal, EvtIndicationsDropped, SysReset, capacity, caps};
use zigbee_zrhp::msg::tables::{TblSync, TblSyncRsp};
use zigbee_zrhp::msg::{Kind, MessageDef};
use zigbee_zrhp::types::Status;

use crate::driver::{self, CallOpts, Client, Confirm, DriverConfig, DriverEvent, Error, Message, NcpSession, SessionState};
use crate::fw::{self, FwImage, FwSource, Slot, slot_state};
use crate::store::{Change, DeviceRecord, EndpointRecord, FcRecord, FwPending, JournalStats, Meta, NetworkRecord, Store};
use crate::store::state::device_flags;
use crate::transport::{Line, NcpControl, PortRx, PortTx};

/// Fallback `fc_stride` (spec 8.4: `0x400`).
pub const DEFAULT_FC_STRIDE: u32 = 0x400;

/// Resume behaviour.
#[derive(Debug, Clone)]
pub struct ResumePolicy {
    /// Send `NWK_INIT` (with floors) when the NCP holds a network. Off for
    /// read-only observation of a shared NCP.
    pub init_network: bool,
    /// Record the NCP's network in an empty store.
    pub adopt: bool,
    /// Replay host endpoints and the permit-join remainder.
    pub replay: bool,
    /// Also confirm trial images the hub did not activate.
    pub confirm_foreign_trial: bool,
    /// `LOG_CONFIG` level to request in every session (`None`: leave it).
    pub log_level: Option<u8>,
}

impl Default for ResumePolicy {
    fn default() -> Self {
        Self { init_network: true, adopt: true, replay: true, confirm_foreign_trial: false, log_level: None }
    }
}

impl ResumePolicy {
    /// Observe only: no `NWK_INIT`, no replay, nothing written to the NCP
    /// except the session handshake, `TBL_SYNC` and read-only queries.
    pub fn read_only() -> Self {
        Self { init_network: false, adopt: false, replay: false, confirm_foreign_trial: false, log_level: None }
    }
}

/// Hub settings.
#[derive(Debug, Clone, Default)]
pub struct HubConfig {
    /// Link driver.
    pub driver: DriverConfig,
    /// Resume.
    pub resume: ResumePolicy,
}

/// Network as the NCP reports it (`NWK_GET_INFO`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkInfo {
    /// 0 none, 1 formed-down, 2 up.
    pub state: u8,
    /// PAN ID.
    pub pan_id: u16,
    /// Extended PAN ID.
    pub ext_pan_id: [u8; 8],
    /// Channel.
    pub channel: u8,
    /// Short address.
    pub short: u16,
    /// NWK update ID.
    pub nwk_update_id: u8,
    /// Key sequence.
    pub key_seq: u8,
    /// TX power.
    pub tx_power: i8,
}

/// NCP firmware update state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FwState {
    /// Nothing in progress.
    Idle,
    /// Streaming into the inactive slot.
    Staging {
        /// Bytes acknowledged.
        written: u32,
        /// Image size.
        total: u32,
    },
    /// `FW_ACTIVATE` sent; waiting for the trial image's session.
    Activated {
        /// Expected version.
        version: u32,
    },
    /// The trial image resumed and was confirmed.
    Confirmed {
        /// Running version.
        version: u32,
    },
    /// The NCP came back with another image (H2 bootloader rollback).
    RolledBack {
        /// Version that was activated.
        expected: u32,
        /// Version running now.
        running: u32,
    },
    /// The update failed.
    Failed(String),
}

/// Network ownership as the lifecycle sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// No session yet.
    Unknown,
    /// The NCP has no network and the store none either (form one).
    NoNetwork,
    /// The NCP's network is the stored one.
    Owned,
    /// The store has a network but the NCP has none (NV wiped): restore
    /// from a backup is needed (phase 2 / operator).
    Lost,
    /// The NCP runs a different network, or the store belongs to another
    /// NCP; the hub does not touch it.
    Mismatch(String),
    /// Observed only (read-only policy, store empty).
    Observed,
}

/// Snapshot of the hub state.
#[derive(Debug, Clone)]
pub struct HubStatus {
    /// Driver session state.
    pub session: SessionState,
    /// Current session.
    pub ncp: Option<Arc<NcpSession>>,
    /// NCP network state (0 none, 1 formed-down, 2 up).
    pub network_state: u8,
    /// Last `NWK_GET_INFO`.
    pub network: Option<NetworkInfo>,
    /// Ownership.
    pub ownership: Ownership,
    /// Completed resumes.
    pub resumes: u32,
    /// NCP restarts seen (new cookie).
    pub ncp_restarts: u32,
    /// Duration of the last resume.
    pub last_resume: Duration,
    /// Last NCP slots.
    pub slots: Vec<Slot>,
    /// NCP firmware update.
    pub fw: FwState,
    /// Permit-join seconds left (`EVT_PERMIT_JOIN_STATE`).
    pub permit_join_s: u8,
    /// Devices in the registry.
    pub devices: usize,
    /// Last NCP frame counters (`SEC_GET_FRAME_COUNTERS`).
    pub counters: Option<SecGetFrameCountersRsp>,
    /// Last error or warning worth showing.
    pub last_error: Option<String>,
    /// The store does not survive a reset.
    pub store_volatile: bool,
    /// Journal occupancy.
    pub journal: JournalStats,
}

/// Network formation parameters; `None` fields are generated by the hub
/// (random PAN ID, EPID and key) and persisted before `NWK_FORM`.
#[derive(Debug, Clone, Copy)]
pub struct FormRequest {
    /// Channel mask (the NCP picks the quietest channel).
    pub channel_mask: u32,
    /// PAN ID.
    pub pan_id: Option<u16>,
    /// Extended PAN ID.
    pub ext_pan_id: Option<[u8; 8]>,
    /// Network key.
    pub network_key: Option<[u8; 16]>,
    /// TX power (dBm).
    pub tx_power: i8,
}

impl Default for FormRequest {
    fn default() -> Self {
        // Zigbee Light Link primary channels first is a hub-level choice;
        // the default is the whole 2.4 GHz band.
        Self { channel_mask: 0x07FF_F800, pan_id: None, ext_pan_id: None, network_key: None, tx_power: 0 }
    }
}

/// Application slot of the H2 NCP layouts (spec 8.9) for a detected flash
/// size (`SYS_GET_INFO.flash_size`, decision N4).
pub fn ncp_slot_size(flash_size: u32) -> Option<u32> {
    match flash_size {
        s if s >= 4 << 20 => Some(0x1E_0000),
        s if s >= 2 << 20 => Some(0xE_0000),
        _ => None,
    }
}

/// Random bytes from the platform (`esp_fill_random`, `/dev/urandom`).
pub type Rng = Arc<dyn Fn(&mut [u8]) + Send + Sync>;

struct Inner {
    client: Client,
    store: Mutex<Store>,
    status: Mutex<HubStatus>,
    cv: Condvar,
    policy: ResumePolicy,
    rng: Rng,
    control: Mutex<Option<Box<dyn NcpControl>>>,
    started: Instant,
    permit_until: Mutex<Option<Instant>>,
    subscribers: Mutex<Vec<Sender<Message>>>,
    resumed_cookie: Mutex<Option<u32>>,
    pending_fc: Mutex<[u32; 2]>,
    coex: Mutex<Option<bool>>,
}

/// The running hub core. Cheap to clone.
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

impl Hub {
    /// Start the driver and the lifecycle thread.
    pub fn start<R, T>(cfg: HubConfig, rx: R, tx: T, control: Option<Box<dyn NcpControl>>, store: Store, rng: Rng) -> Hub
    where
        R: PortRx + 'static,
        T: PortTx + 'static,
    {
        let mut cookie = [0u8; 4];
        while u32::from_le_bytes(cookie) == 0 {
            rng(&mut cookie);
        }
        let (client, events) = driver::start(cfg.driver, rx, tx, u32::from_le_bytes(cookie));
        let status = HubStatus {
            session: SessionState::Down,
            ncp: None,
            network_state: 0,
            network: None,
            ownership: Ownership::Unknown,
            resumes: 0,
            ncp_restarts: 0,
            last_resume: Duration::ZERO,
            slots: Vec::new(),
            fw: match store.state().fw_pending {
                Some(p) if p.activated => FwState::Activated { version: p.version },
                _ => FwState::Idle,
            },
            permit_join_s: 0,
            devices: store.state().devices.len(),
            counters: None,
            last_error: None,
            store_volatile: store.is_volatile(),
            journal: store.stats(),
        };
        let inner = Arc::new(Inner {
            client,
            store: Mutex::new(store),
            status: Mutex::new(status),
            cv: Condvar::new(),
            policy: cfg.resume,
            rng,
            control: Mutex::new(control),
            started: Instant::now(),
            permit_until: Mutex::new(None),
            subscribers: Mutex::new(Vec::new()),
            resumed_cookie: Mutex::new(None),
            pending_fc: Mutex::new([0; 2]),
            coex: Mutex::new(None),
        });
        let hub = Hub { inner };
        let worker = hub.clone();
        thread::Builder::new()
            .name("hub-lifecycle".into())
            .stack_size(12 * 1024)
            .spawn(move || worker.run(events))
            .expect("hub lifecycle thread");
        hub
    }

    /// The request client.
    pub fn client(&self) -> &Client {
        &self.inner.client
    }

    /// Status snapshot.
    pub fn status(&self) -> HubStatus {
        let mut s = self.inner.status.lock().unwrap().clone();
        s.session = self.inner.client.state();
        s.ncp = self.inner.client.session();
        s
    }

    /// A copy of the store state.
    pub fn store_state(&self) -> crate::store::HostState {
        self.inner.store.lock().unwrap().state().clone()
    }

    /// Wait until requests flow (session negotiated and resumed).
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        self.inner.client.wait_state(timeout, |s| matches!(s, SessionState::Ready | SessionState::Incompatible))
            == Some(SessionState::Ready)
    }

    /// Wait until the status satisfies `pred`.
    pub fn wait_status(&self, timeout: Duration, pred: impl Fn(&HubStatus) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut g = self.inner.status.lock().unwrap();
        loop {
            if pred(&g) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            g = self.inner.cv.wait_timeout(g, (deadline - now).min(Duration::from_millis(100))).unwrap().0;
        }
    }

    /// Every NCP event from now on.
    pub fn subscribe(&self) -> Receiver<Message> {
        let (tx, rx) = mpsc::channel();
        self.inner.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// Stop the driver.
    pub fn shutdown(&self) {
        self.inner.client.shutdown();
    }

    fn update(&self, f: impl FnOnce(&mut HubStatus)) {
        let mut g = self.inner.status.lock().unwrap();
        f(&mut g);
        self.inner.cv.notify_all();
    }

    fn note_error(&self, what: String) {
        warn!("hub: {what}");
        self.update(|s| s.last_error = Some(what));
    }

    fn commit(&self, change: Change) {
        let mut store = self.inner.store.lock().unwrap();
        if let Err(e) = store.commit_if_changed(change) {
            error!("hub: store commit failed: {e}");
        }
        let (devices, journal) = (store.state().devices.len(), store.stats());
        drop(store);
        self.update(|s| {
            s.devices = devices;
            s.journal = journal;
        });
    }

    fn now_s(&self) -> u32 {
        self.inner.started.elapsed().as_secs() as u32
    }

    fn run(self, events: Receiver<DriverEvent>) {
        for ev in events {
            match ev {
                DriverEvent::SessionUp(session) => {
                    let t0 = Instant::now();
                    match self.resume(&session) {
                        Ok(()) => {
                            self.inner.client.resume_done();
                            *self.inner.resumed_cookie.lock().unwrap() = session.ready.as_ref().map(|r| r.ncp_cookie);
                            self.update(|s| {
                                s.resumes += 1;
                                s.last_resume = t0.elapsed();
                            });
                            info!("hub: resumed in {:?}", t0.elapsed());
                        }
                        Err(Error::LinkReset | Error::Closed) => {
                            warn!("hub: session lost during resume; waiting for the next one");
                        }
                        Err(e) => {
                            self.note_error(format!("resume: {e}"));
                            self.inner.client.resume_done();
                        }
                    }
                }
                DriverEvent::Event(m) => {
                    self.on_event(&m);
                    self.inner.subscribers.lock().unwrap().retain(|s| s.send(m.clone()).is_ok());
                }
                DriverEvent::Request(m) => self.on_ncp_request(&m),
                DriverEvent::Incompatible(why) => self.note_error(format!("NCP incompatible: {why}")),
                DriverEvent::PeerReset { .. } | DriverEvent::LinkLost { .. } | DriverEvent::LinkUp(_) => {
                    self.update(|_| {});
                }
                DriverEvent::Stopped => break,
            }
        }
    }

    fn opts(&self, timeout: Duration) -> CallOpts {
        CallOpts { timeout, during_resume: true, confirm: None }
    }

    fn resume(&self, session: &NcpSession) -> Result<(), Error> {
        let policy = &self.inner.policy;
        let c = &self.inner.client;
        let ready = session.ready.clone();
        let info = session.info.clone();
        let negotiated = &session.negotiated;
        info!(
            "hub: NCP fw {:?} chip {} caps 0x{:X} flash {} B, network_state {}",
            negotiated.fw,
            negotiated.chip,
            negotiated.ncp_caps,
            info.as_ref().map_or(0, |i| i.flash_size),
            ready.as_ref().map_or(0, |r| r.network_state)
        );

        // 1. UART flow-control mismatch (spec 11.5).
        if let Some(i) = &info
            && session.line.flow && i.transport == 2 && i.uart_flow == 0 {
                warn!("hub: NCP reports no RTS/CTS; turning host flow control off");
                c.set_line(Line { baud: session.line.baud, flow: false });
            }

        // Quick path: same NCP instance, already resumed in this process.
        let cookie = ready.as_ref().map(|r| r.ncp_cookie);
        if session.resync && cookie.is_some() && *self.inner.resumed_cookie.lock().unwrap() == cookie {
            info!("hub: resync with the same NCP instance, nothing to replay");
            return Ok(());
        }

        // 2. Store identity, cookie, table epoch.
        let meta = self.inner.store.lock().unwrap().state().meta;
        let ieee = info.as_ref().map(|i| i.ieee_active).unwrap_or_default();
        let foreign_store = meta.ncp_ieee != [0; 8] && ieee != [0; 8] && meta.ncp_ieee != ieee;
        let mut new_meta = meta;
        if meta.ncp_ieee == [0; 8] && (policy.adopt || policy.init_network) {
            new_meta.ncp_ieee = ieee;
        }
        if let Some(r) = &ready {
            if meta.last_ncp_cookie != 0 && meta.last_ncp_cookie != r.ncp_cookie {
                self.update(|s| s.ncp_restarts += 1);
            }
            if meta.table_epoch != 0 && meta.table_epoch != r.table_epoch {
                warn!("hub: NCP table epoch 0x{:08X} -> 0x{:08X}", meta.table_epoch, r.table_epoch);
            }
            new_meta.last_ncp_cookie = r.ncp_cookie;
            new_meta.table_epoch = r.table_epoch;
        }
        if foreign_store {
            let why = format!("store belongs to NCP {:02X?}, connected NCP is {:02X?}", meta.ncp_ieee, ieee);
            self.update(|s| s.ownership = Ownership::Mismatch(why.clone()));
            self.note_error(why);
            return Ok(());
        }

        // 3. Network.
        let network_state = ready.as_ref().map_or(0, |r| r.network_state);
        self.update(|s| s.network_state = network_state);
        let stored = self.inner.store.lock().unwrap().state().network;
        let mut ownership = Ownership::NoNetwork;
        if network_state == 0 {
            if stored.is_some() {
                ownership = Ownership::Lost;
                self.note_error("the NCP has no network but the store has one: restore needed".into());
            }
        } else {
            let nwk = self.get_network_info(self.opts(Duration::from_secs(5)))?;
            match stored {
                None if policy.adopt => {
                    let key = self.read_network_key(negotiated.has(caps::KEY_EXPORT));
                    self.commit(Change::Network(NetworkRecord {
                        pan_id: nwk.pan_id,
                        ext_pan_id: nwk.ext_pan_id,
                        channel: nwk.channel,
                        channel_mask: 1 << nwk.channel,
                        network_key: key.unwrap_or([0; 16]),
                        key_seq: nwk.key_seq,
                        nwk_update_id: nwk.nwk_update_id,
                        tx_power: nwk.tx_power,
                        nwk_manager: 0,
                    }));
                    info!("hub: adopted network PAN 0x{:04X} channel {}", nwk.pan_id, nwk.channel);
                    ownership = Ownership::Owned;
                }
                None => ownership = Ownership::Observed,
                Some(n) if n.pan_id != nwk.pan_id || n.ext_pan_id != nwk.ext_pan_id => {
                    let why = format!(
                        "NCP runs PAN 0x{:04X}, the store owns PAN 0x{:04X}; not touching it",
                        nwk.pan_id, n.pan_id
                    );
                    self.note_error(why.clone());
                    ownership = Ownership::Mismatch(why);
                }
                Some(mut n) => {
                    // A channel change or key rotation completed during the outage.
                    if nwk.nwk_update_id != n.nwk_update_id || nwk.key_seq != n.key_seq || nwk.channel != n.channel {
                        n.channel = nwk.channel;
                        n.nwk_update_id = nwk.nwk_update_id;
                        n.key_seq = nwk.key_seq;
                        self.commit(Change::Network(n));
                    }
                    ownership = Ownership::Owned;
                }
            }
            if ownership == Ownership::Owned && policy.init_network {
                self.nwk_init(negotiated.capacity(capacity::FC_STRIDE), network_state)?;
                self.get_network_info(self.opts(Duration::from_secs(5)))?;
            }
        }
        self.update(|s| s.ownership = ownership.clone());

        // 4. Replay.
        if policy.replay && ownership == Ownership::Owned {
            let endpoints: Vec<EndpointRecord> =
                self.inner.store.lock().unwrap().state().endpoints.values().cloned().collect();
            for ep in &endpoints {
                self.send_endpoint(ep, true)?;
            }
            let remaining = self.inner.permit_until.lock().unwrap().and_then(|t| t.checked_duration_since(Instant::now()));
            if let Some(left) = remaining {
                let secs = left.as_secs().clamp(1, 254) as u8;
                c.call(&NwkPermitJoin { duration_s: secs, target: 0xFFFC, flags: 1 }, self.opts(Duration::from_secs(5)))?;
            }
        }
        if policy.replay {
            let coex = *self.inner.coex.lock().unwrap();
            if let Some(on) = coex {
                self.apply_coex(on, true)?;
            }
        }
        if let Some(level) = policy.log_level {
            let body = zigbee_zrhp::msg::log::LogConfig { level, module_mask: u32::MAX };
            if let Err(e) = c.call(&body, self.opts(Duration::from_secs(5))) {
                warn!("hub: LOG_CONFIG: {e}");
            }
        }

        // 5. TBL_SYNC (not in read-only observation).
        if policy.adopt || policy.init_network {
            let sync = TblSync {
                host_db_epoch: meta.host_db_epoch,
                last_ncp_table_epoch: meta.table_epoch,
                host_max_rev: meta.host_max_rev,
                flags: 0,
            };
            match c.call(&sync, self.opts(Duration::from_secs(5))) {
                Ok(rsp) => {
                    if let Ok(r) = rsp.body::<TblSyncRsp>() {
                        new_meta.table_epoch = r.ncp_table_epoch;
                    }
                }
                Err(Error::Status(s)) => warn!("hub: TBL_SYNC: {s:?}"),
                Err(e) => return Err(e),
            }
        }
        if policy.adopt || policy.init_network || meta.ncp_ieee != [0; 8] {
            self.commit(Change::Meta(new_meta));
        }

        // 6. NCP firmware.
        if negotiated.has(caps::NCP_FW_UPDATE) {
            self.check_firmware(ownership == Ownership::Owned || network_state == 0)?;
        }
        if let Ok(rsp) = c.call_empty(ids::SEC_GET_FRAME_COUNTERS, self.opts(Duration::from_secs(5))) {
            let fc = rsp.body::<SecGetFrameCountersRsp>().ok();
            self.update(|s| s.counters = fc);
        }
        Ok(())
    }

    fn get_network_info(&self, opts: CallOpts) -> Result<NetworkInfo, Error> {
        let rsp = self.inner.client.call_empty(ids::NWK_GET_INFO, opts)?;
        let r: NwkGetInfoRsp = rsp.body()?;
        let nwk = NetworkInfo {
            state: r.state,
            pan_id: r.pan_id,
            ext_pan_id: r.ext_pan_id,
            channel: r.channel,
            short: r.short,
            nwk_update_id: r.nwk_update_id,
            key_seq: r.key_seq,
            tx_power: r.tx_power,
        };
        self.update(|s| {
            s.network = Some(nwk);
            s.network_state = nwk.state;
        });
        Ok(nwk)
    }

    fn read_network_key(&self, exportable: bool) -> Option<[u8; 16]> {
        if !exportable {
            return None;
        }
        let rsp = self.inner.client.call(&SecGetNetworkKey { which: 0 }, self.opts(Duration::from_secs(5))).ok()?;
        rsp.body::<SecGetNetworkKeyRsp>().ok().map(|k| k.key)
    }

    /// Checkpoint floors for `NWK_INIT` (spec 8.4): `last logged + 9 × stride`.
    pub fn floors(&self, stride: Option<u32>) -> (u32, u32) {
        let stride = stride.unwrap_or(DEFAULT_FC_STRIDE);
        let fc = self.inner.store.lock().unwrap().state().fc;
        let floor = |f: Option<FcRecord>| f.map_or(0, |f| f.limit.saturating_add(stride.saturating_mul(9)));
        (floor(fc[0]), floor(fc[1]))
    }

    fn nwk_init(&self, stride: Option<u32>, network_state: u8) -> Result<(), Error> {
        let (nwk_fc_floor, aps_fc_floor) = self.floors(stride);
        let mut opts = self.opts(Duration::from_secs(10));
        if network_state == 1 {
            opts.confirm = Some(Confirm::network_state(Duration::from_secs(30)));
        }
        let outcome = self.inner.client.call_raw(
            NwkInit::ID,
            &driver::encode_body(&NwkInit { nwk_fc_floor, aps_fc_floor })?,
            opts,
        )?;
        let state = outcome.reply.body::<NwkInitRsp>().map(|r| r.network_state).unwrap_or(0);
        if let Some(ev) = outcome.confirm.as_ref().and_then(|m| m.decode::<EvtNetworkState>().ok())
            && ev.state != 2 {
                return Err(Error::Other(format!("NWK_INIT: network state {} zb_status 0x{:02X}", ev.state, ev.zb_status)));
            }
        info!("hub: NWK_INIT (floors {nwk_fc_floor}/{aps_fc_floor}) -> state {state}");
        Ok(())
    }

    fn send_endpoint(&self, ep: &EndpointRecord, during_resume: bool) -> Result<(), Error> {
        let body = NwkAddEndpoint {
            ep: ep.ep,
            profile: ep.profile,
            device_id: ep.device_id,
            version: ep.version,
            in_clusters: List::new(&ep.in_clusters),
            out_clusters: List::new(&ep.out_clusters),
        };
        let opts = CallOpts { during_resume, ..CallOpts::timeout(Duration::from_secs(5)) };
        self.inner.client.call(&body, opts).map(|_| ())
    }

    fn check_firmware(&self, may_confirm: bool) -> Result<(), Error> {
        let c = &self.inner.client;
        let slots = match fw::slots(c, self.opts(Duration::from_secs(5))) {
            Ok(s) => s,
            Err(Error::Status(s)) => {
                warn!("hub: FW_GET_SLOTS: {s:?}");
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        self.update(|s| s.slots = slots.clone());
        let Some(run) = fw::running(&slots) else {
            return Ok(());
        };
        let pending = self.inner.store.lock().unwrap().state().fw_pending;
        match pending {
            Some(p) if p.activated => {
                if run.version != p.version || run.sha256 != p.sha256 {
                    self.note_error(format!(
                        "NCP update 0x{:08X} rolled back: running 0x{:08X} in slot {}",
                        p.version, run.version, run.slot
                    ));
                    self.update(|s| s.fw = FwState::RolledBack { expected: p.version, running: run.version });
                    self.commit(Change::FwClear);
                } else if run.state == slot_state::RUNNING_TRIAL {
                    if !may_confirm {
                        self.note_error("trial image running but the network did not resume; not confirming".into());
                        return Ok(());
                    }
                    c.call_empty(ids::FW_CONFIRM, self.opts(Duration::from_secs(10)))?;
                    info!("hub: FW_CONFIRM 0x{:08X} (slot {})", run.version, run.slot);
                    self.update(|s| s.fw = FwState::Confirmed { version: run.version });
                    self.commit(Change::FwClear);
                    if let Ok(slots) = fw::slots(c, self.opts(Duration::from_secs(5))) {
                        self.update(|s| s.slots = slots);
                    }
                } else {
                    self.update(|s| s.fw = FwState::Confirmed { version: run.version });
                    self.commit(Change::FwClear);
                }
            }
            _ if run.state == slot_state::RUNNING_TRIAL => {
                if self.inner.policy.confirm_foreign_trial && may_confirm {
                    c.call_empty(ids::FW_CONFIRM, self.opts(Duration::from_secs(10)))?;
                    info!("hub: FW_CONFIRM of a trial image the hub did not start (policy)");
                } else {
                    self.note_error(format!(
                        "NCP runs trial image 0x{:08X} not started by this hub; not confirming",
                        run.version
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn on_event(&self, m: &Message) {
        match m.msg_id {
            ids::EVT_NETWORK_STATE => {
                if let Ok(e) = m.decode::<EvtNetworkState>() {
                    info!("hub: network state {} (reason {}, zb 0x{:02X})", e.state, e.reason, e.zb_status);
                    let state = match e.state {
                        2 => 2,
                        0 => 1,
                        _ => return,
                    };
                    self.update(|s| s.network_state = state);
                }
            }
            ids::EVT_FC_CHECKPOINT => {
                if let Ok(e) = m.decode::<EvtFcCheckpoint>() {
                    self.on_checkpoint(e);
                }
            }
            ids::EVT_DEVICE_JOINED => {
                if let Ok(e) = m.decode::<EvtDeviceJoined>() {
                    info!("hub: device {:02X?} 0x{:04X} joined (decision {})", e.ieee.0, e.nwk, e.decision);
                    if e.decision == 0 {
                        self.commit(Change::Device(DeviceRecord {
                            ieee: e.ieee.0,
                            nwk: e.nwk,
                            capability: e.capability,
                            flags: 0,
                            last_seen: self.now_s(),
                        }));
                    }
                }
            }
            ids::EVT_DEVICE_AUTHORIZED => {
                if let Ok(e) = m.decode::<EvtDeviceAuthorized>() {
                    let mut store = self.inner.store.lock().unwrap();
                    let known = store.state().devices.get(&e.ieee.0).copied();
                    drop(store.commit_if_changed(Change::Device(DeviceRecord {
                        ieee: e.ieee.0,
                        nwk: e.nwk,
                        capability: known.map_or(0, |d| d.capability),
                        flags: known.map_or(0, |d| d.flags)
                            | if e.result < 2 { device_flags::AUTHORIZED } else { 0 },
                        last_seen: self.now_s(),
                    })));
                }
            }
            ids::EVT_DEVICE_LEFT => {
                if let Ok(e) = m.decode::<EvtDeviceLeft>() {
                    info!("hub: device {:02X?} 0x{:04X} left (flags 0x{:02X})", e.ieee.0, e.nwk, e.flags);
                    if e.flags & 0x01 == 0 && e.ieee.0 != [0; 8] {
                        self.commit(Change::DeviceDel(e.ieee.0));
                    }
                }
            }
            ids::EVT_ADDRESS_CHANGED => {
                if let Ok(e) = m.decode::<EvtAddressChanged>() {
                    let known = self.inner.store.lock().unwrap().state().devices.get(&e.ieee.0).copied();
                    if let Some(mut d) = known {
                        d.nwk = e.new_nwk;
                        d.last_seen = self.now_s();
                        self.commit(Change::Device(d));
                    }
                }
            }
            ids::EVT_PERMIT_JOIN_STATE => {
                if let Ok(e) = m.decode::<EvtPermitJoinState>() {
                    self.update(|s| s.permit_join_s = e.remaining_s);
                    if e.remaining_s == 0 {
                        *self.inner.permit_until.lock().unwrap() = None;
                    }
                }
            }
            ids::EVT_TBL_EPOCH_CHANGED => {
                if let Some(epoch) = m.payload.get(..4) {
                    let epoch = u32::from_le_bytes(epoch.try_into().unwrap());
                    let mut meta: Meta = self.inner.store.lock().unwrap().state().meta;
                    meta.table_epoch = epoch;
                    self.commit(Change::Meta(meta));
                }
            }
            ids::EVT_FATAL => {
                if let Ok(e) = m.decode::<EvtFatal>() {
                    self.note_error(format!(
                        "NCP fatal kind {} code 0x{:08X}: {}",
                        e.kind,
                        e.code,
                        String::from_utf8_lossy(e.text.0)
                    ));
                }
            }
            ids::EVT_INDICATIONS_DROPPED => {
                if let Ok(e) = m.decode::<EvtIndicationsDropped>() {
                    warn!("hub: NCP dropped {} indications on endpoint {} (reason {})", e.count, e.endpoint, e.reason);
                }
            }
            ids::EVT_LOG_RECORD => {
                if let Ok(e) = m.decode::<EvtLogRecord>() {
                    info!("ncp [{} ms L{}] {}", e.time_ms, e.level, String::from_utf8_lossy(e.text.0));
                }
            }
            _ => {}
        }
    }

    /// Persist every 4th checkpoint per domain (spec 8.4 requires ≤ 8);
    /// the floors add 9 strides on top of the logged limit.
    fn on_checkpoint(&self, e: EvtFcCheckpoint) {
        let Some(slot) = usize::from(e.domain).checked_sub(1).filter(|i| *i < 2) else {
            return;
        };
        let logged = self.inner.store.lock().unwrap().state().fc[slot];
        let mut skipped = self.inner.pending_fc.lock().unwrap();
        let due = logged.is_none_or(|f| f.key_seq != e.key_seq || e.durable_limit < f.limit) || skipped[slot] >= 3;
        if due {
            skipped[slot] = 0;
            drop(skipped);
            self.commit(Change::Fc(e.domain, FcRecord { key_seq: e.key_seq, limit: e.durable_limit, generation: e.generation }));
        } else {
            skipped[slot] += 1;
        }
    }

    fn on_ncp_request(&self, m: &Message) {
        // Phase 1 does not set HOST_TABLES, so the NCP should not page; a
        // lookup is answered NOT_FOUND (the NCP applies its policy), a
        // writeback is refused (the NCP keeps the entry dirty).
        let status = match m.msg_id {
            ids::TBL_LOOKUP => Status::NotFound,
            ids::TBL_WRITEBACK => Status::StorageError,
            _ => Status::UnsupportedCommand,
        };
        if m.kind == Kind::Request {
            self.inner.client.respond(m.msg_id, m.tid, vec![status.to_u8()]);
        }
    }

    // ── Operations ──────────────────────────────────────────────────

    /// Form a new network with host-chosen parameters (spec 7.2
    /// `NWK_FORM`). The parameters are persisted before the request.
    pub fn form(&self, req: FormRequest) -> Result<NetworkRecord, Error> {
        if self.status().network_state != 0 {
            return Err(Error::Status(Status::AlreadyExists));
        }
        let rng = &self.inner.rng;
        let pan_id = req.pan_id.unwrap_or_else(|| loop {
            let mut b = [0u8; 2];
            rng(&mut b);
            let p = u16::from_le_bytes(b);
            if (0x0001..0xFFF7).contains(&p) {
                break p;
            }
        });
        let ext_pan_id = req.ext_pan_id.unwrap_or_else(|| loop {
            let mut b = [0u8; 8];
            rng(&mut b);
            if b != [0; 8] && b != [0xFF; 8] {
                break b;
            }
        });
        let network_key = req.network_key.unwrap_or_else(|| {
            let mut k = [0u8; 16];
            rng(&mut k);
            k
        });
        let mut record = NetworkRecord {
            pan_id,
            ext_pan_id,
            channel: 0,
            channel_mask: req.channel_mask,
            network_key,
            key_seq: 0,
            nwk_update_id: 0,
            tx_power: req.tx_power,
            nwk_manager: 0,
        };
        self.commit(Change::Network(record));
        let body = NwkForm {
            channel_mask: req.channel_mask,
            pan_id,
            ext_pan_id,
            network_key,
            key_seq: 0,
            nwk_update_id: 0,
            tx_power: req.tx_power,
            nwk_manager: 0,
        };
        let opts = CallOpts { confirm: Some(Confirm::network_state(Duration::from_secs(60))), ..CallOpts::default() };
        let outcome = self.inner.client.call_raw(NwkForm::ID, &driver::encode_body(&body)?, opts)?;
        let ev: EvtNetworkState = outcome.confirm.as_ref().ok_or(Error::Codec)?.decode()?;
        if ev.state != 2 {
            return Err(Error::Other(format!("formation failed: state {} zb_status 0x{:02X}", ev.state, ev.zb_status)));
        }
        let nwk = self.get_network_info(CallOpts::default())?;
        record.channel = nwk.channel;
        self.commit(Change::Network(record));
        let mut meta = self.inner.store.lock().unwrap().state().meta;
        if let Some(i) = self.inner.client.session().and_then(|s| s.info.clone()) {
            meta.ncp_ieee = i.ieee_active;
        }
        self.commit(Change::Meta(meta));
        self.update(|s| s.ownership = Ownership::Owned);
        info!("hub: formed PAN 0x{:04X} on channel {}", pan_id, nwk.channel);
        Ok(record)
    }

    /// Open joining on the whole network (TC significance) for `secs`; the
    /// remainder is replayed after an NCP restart.
    pub fn permit_join(&self, secs: u8) -> Result<(), Error> {
        self.inner.client.call(&NwkPermitJoin { duration_s: secs, target: 0xFFFC, flags: 1 }, CallOpts::default())?;
        *self.inner.permit_until.lock().unwrap() =
            (secs > 0).then(|| Instant::now() + Duration::from_secs(secs.into()));
        Ok(())
    }

    /// Run `f` on the host store (also used for the application's
    /// key/value records).
    pub fn with_store<R>(&self, f: impl FnOnce(&mut Store) -> R) -> R {
        let mut store = self.inner.store.lock().unwrap();
        let r = f(&mut store);
        let (devices, journal) = (store.state().devices.len(), store.stats());
        drop(store);
        self.update(|s| {
            s.devices = devices;
            s.journal = journal;
        });
        r
    }

    /// `NWK_PERMIT_JOIN` on one router (`Some(nwk)`, not replayed) or the
    /// whole network (`None`, as [`permit_join`](Self::permit_join)).
    pub fn permit_join_on(&self, secs: u8, target: Option<u16>) -> Result<(), Error> {
        match target {
            None => self.permit_join(secs),
            Some(t) => self
                .inner
                .client
                .call(&NwkPermitJoin { duration_s: secs, target: t, flags: 1 }, CallOpts::default())
                .map(|_| ()),
        }
    }

    /// `NWK_GROUP_MEMBERSHIP` (`op` 1 add, 2 remove, 3 clear) of a host
    /// endpoint.
    pub fn group_membership(&self, op: u8, group: u16, ep: u8) -> Result<(), Error> {
        let body = zigbee_zrhp::msg::network::NwkGroupMembership { op, group, ep };
        self.inner.client.call(&body, CallOpts::default()).map(|_| ())
    }

    /// `NWK_REMOVE_DEVICE` (`flags` bit0 rejoin, bit1 remove children,
    /// bit2 TC remove via parent).
    pub fn remove_device(&self, nwk: u16, ieee: [u8; 8], flags: u8) -> Result<(), Error> {
        let body = zigbee_zrhp::msg::network::NwkRemoveDevice { nwk, ieee: zigbee_zrhp::types::Ieee(ieee), flags };
        self.inner.client.call(&body, CallOpts::default()).map(|_| ())
    }

    /// PTA coexistence (`coex_enable`, spec 11.5 / decision N7): the hub
    /// turns it on while its Wi-Fi is up. Volatile on the NCP, so it is
    /// re-applied in every resume; a no-op without `PTA_COEX`.
    pub fn set_coex(&self, on: bool) -> Result<(), Error> {
        *self.inner.coex.lock().unwrap() = Some(on);
        if self.inner.client.state() != SessionState::Ready {
            return Ok(());
        }
        self.apply_coex(on, false)
    }

    fn apply_coex(&self, on: bool, during_resume: bool) -> Result<(), Error> {
        let Some(session) = self.inner.client.session() else { return Ok(()) };
        if !session.negotiated.has(caps::PTA_COEX) {
            return Ok(());
        }
        let value = [u8::from(on)];
        let item = [zigbee_zrhp::msg::system::ConfigValue {
            id: zigbee_zrhp::msg::system::config::COEX_ENABLE,
            value: zigbee_zrhp::codec::Bytes8(&value),
        }];
        let body = zigbee_zrhp::msg::system::SysSetConfig { values: List::new(&item) };
        let opts = CallOpts { during_resume, ..CallOpts::timeout(Duration::from_secs(5)) };
        self.inner.client.call(&body, opts).map(|_| ())
    }

    /// Register a host endpoint now and on every resume.
    pub fn add_endpoint(&self, ep: EndpointRecord) -> Result<(), Error> {
        self.send_endpoint(&ep, false)?;
        self.commit(Change::Endpoint(ep));
        Ok(())
    }

    /// `APS_DATA_REQUEST` and its `EVT_APS_DATA_CONFIRM`.
    pub fn send_aps(&self, req: &ApsDataRequest<'_>, timeout: Duration) -> Result<EvtApsDataConfirm, Error> {
        let opts = CallOpts { confirm: Some(Confirm::aps(timeout)), ..CallOpts::default() };
        let outcome = self.inner.client.call_raw(ApsDataRequest::ID, &driver::encode_body(req)?, opts)?;
        outcome.confirm.as_ref().ok_or(Error::Codec)?.decode()
    }

    /// `FW_GET_SLOTS`.
    pub fn fw_slots(&self) -> Result<Vec<Slot>, Error> {
        let slots = fw::slots(&self.inner.client, CallOpts::default())?;
        self.update(|s| s.slots = slots.clone());
        Ok(slots)
    }

    /// Stream `image` into the NCP's inactive slot, verify it and activate
    /// it as a trial boot. The next session confirms it (or reports the
    /// rollback).
    pub fn fw_update(&self, image: &FwImage, src: &mut dyn FwSource) -> Result<u8, Error> {
        let session = self.inner.client.session().ok_or(Error::Status(Status::InvalidState))?;
        if !session.negotiated.has(caps::NCP_FW_UPDATE) {
            return Err(Error::Status(Status::UnsupportedCommand));
        }
        let slot_size = session.info.as_ref().and_then(|i| ncp_slot_size(i.flash_size));
        if slot_size.is_some_and(|max| image.size > max) {
            return Err(Error::Status(Status::TooLarge));
        }
        self.update(|s| s.fw = FwState::Staging { written: 0, total: image.size });
        let this = self.clone();
        let mut progress = move |written: u32, total: u32| this.update(|s| s.fw = FwState::Staging { written, total });
        let slot = match fw::stage(&self.inner.client, image, src, 2, &mut progress) {
            Ok(slot) => slot,
            Err(e) => {
                self.update(|s| s.fw = FwState::Failed(e.to_string()));
                return Err(e);
            }
        };
        self.commit(Change::FwPending(FwPending { version: image.version, sha256: image.sha256, slot, activated: true }));
        let body = zigbee_zrhp::msg::firmware::FwActivate { delay_ms: 200 };
        if let Err(e) = self.inner.client.call(&body, CallOpts::default()) {
            self.commit(Change::FwClear);
            self.update(|s| s.fw = FwState::Failed(format!("FW_ACTIVATE: {e}")));
            return Err(e);
        }
        self.update(|s| s.fw = FwState::Activated { version: image.version });
        info!("hub: NCP image 0x{:08X} staged in slot {slot}, trial boot", image.version);
        Ok(slot)
    }

    /// `FW_CONFIRM` (manual; the resume confirms the hub's own updates).
    pub fn fw_confirm(&self) -> Result<(), Error> {
        self.inner.client.call_empty(ids::FW_CONFIRM, CallOpts::default())?;
        self.commit(Change::FwClear);
        Ok(())
    }

    /// Reset the NCP: `SYS_RESET` (soft) or the EN line (hard).
    pub fn reset_ncp(&self, hard: bool) -> Result<(), Error> {
        if hard {
            let mut control = self.inner.control.lock().unwrap();
            let control = control.as_mut().ok_or(Error::Status(Status::UnsupportedCommand))?;
            return control.reset().map_err(|e| Error::Other(e.to_string()));
        }
        self.inner.client.call(&SysReset { mode: 0, confirm: 0 }, CallOpts::default()).map(|_| ())
    }

    /// Manual recovery path (spec 11.5): stop ZRHP and reset the NCP into
    /// its ROM serial loader. The byte stream then belongs to the
    /// operator's flasher (the phase-0 bridge on the Thread BR board); the
    /// hub keeps no NCP image and never does this on its own.
    pub fn enter_rom_loader(&self) -> Result<(), Error> {
        let mut control = self.inner.control.lock().unwrap();
        let control = control.as_mut().ok_or(Error::Status(Status::UnsupportedCommand))?;
        self.inner.client.shutdown();
        control.enter_rom_loader().map_err(|e| Error::Other(e.to_string()))
    }
}
