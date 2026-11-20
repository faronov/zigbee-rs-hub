//! Host core against the simulated NCP.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use zigbee_zrhp::codec::Rest;
use zigbee_zrhp::msg::data::{ApsDataRequest, ApsDst};
use zigbee_zrhp::msg::ids;
use zigbee_zrhp::msg::system::SysEcho;
use zigbee_zrhp::types::Status;

use crate::driver::{CallOpts, Error};
use crate::fw::{FwImage, SliceSource, sha256, slot_state};
use crate::lifecycle::{FormRequest, FwState, Hub, HubConfig, Ownership, Rng};
use crate::sim::{STRIDE, SimNcp};
use crate::store::{EndpointRecord, Flash, MemFlash, Store};

use crate::transport::{Line, pipe};

const T: Duration = Duration::from_secs(10);

fn rng() -> Rng {
    let n = Arc::new(AtomicU32::new(0x1234_5678));
    Arc::new(move |buf: &mut [u8]| {
        for b in buf {
            let x = n.fetch_add(0x9E37_79B9, Ordering::Relaxed);
            *b = (x >> 13) as u8 ^ (x >> 24) as u8;
        }
    })
}

fn start(store: Store) -> (Hub, SimNcp) {
    start_with(store, HubConfig::default())
}

fn start_with(store: Store, mut cfg: HubConfig) -> (Hub, SimNcp) {
    let (host, ncp) = pipe::pair();
    let sim = SimNcp::start(ncp);
    let (rx, tx) = host.split();
    cfg.driver.lines = vec![Line::BR_DEFAULT];
    (Hub::start(cfg, rx, tx, None, store, rng()), sim)
}

/// Reboot the NCP and wait until the host has noticed (requests issued
/// afterwards are held for the resume instead of racing the reboot).
fn reboot(hub: &Hub, sim: &SimNcp) {
    let boots = sim.state().boots;
    sim.reboot();
    while sim.state().boots == boots {
        std::thread::sleep(Duration::from_millis(1));
    }
    let cookie = sim.state().cookie;
    let t = std::time::Instant::now();
    while t.elapsed() < T {
        match hub.client().session() {
            Some(s) if s.link.peer_cookie != cookie => std::thread::sleep(Duration::from_millis(1)),
            _ => break,
        }
    }
}

fn echo(hub: &Hub, data: &[u8]) -> Result<Vec<u8>, Error> {
    let rsp = hub.client().call(&SysEcho { data: Rest(data) }, CallOpts::default())?;
    Ok(rsp.payload[1..].to_vec())
}

#[test]
fn session_negotiates_and_reports_the_ncp() {
    let (hub, sim) = start(Store::volatile());
    assert!(hub.wait_ready(T));
    let session = hub.client().session().unwrap();
    assert_eq!(session.negotiated.fw, "sim-ncp 0.1.0");
    assert_eq!(session.info.as_ref().unwrap().flash_size, 4 << 20);
    assert_eq!(echo(&hub, b"hello").unwrap(), b"hello");
    assert!(hub.wait_status(T, |s| s.ownership == Ownership::NoNetwork));
    hub.shutdown();
    sim.stop();
}

#[test]
fn forms_then_resumes_seamlessly_after_an_ncp_reboot() {
    let (hub, sim) = start(Store::volatile());
    assert!(hub.wait_ready(T));
    let net = hub.form(FormRequest { channel_mask: 1 << 17, ..FormRequest::default() }).unwrap();
    assert_eq!(net.channel, 17);
    assert_eq!(sim.state().pan_id, net.pan_id);
    assert_eq!(sim.state().key, net.network_key);
    hub.add_endpoint(EndpointRecord {
        ep: 1,
        profile: 0x0104,
        device_id: 0x0005,
        version: 0,
        in_clusters: vec![0x0000],
        out_clusters: vec![0x0006],
    })
    .unwrap();
    // The checkpoint the NCP emitted after forming lands in the store.
    assert!(hub.wait_status(T, |_| hub.store_state().fc[0].is_some()));
    let logged = hub.store_state().fc[0].unwrap().limit;

    let resumes = hub.status().resumes;
    reboot(&hub, &sim);
    // A request issued while the NCP reboots is held and then served.
    assert_eq!(echo(&hub, b"during reboot").unwrap(), b"during reboot");
    assert!(hub.wait_status(T, |s| s.resumes > resumes));
    let st = sim.state();
    assert_eq!(st.network_state, 2, "NWK_INIT resumed the network");
    assert_eq!(st.floors.last().copied(), Some((logged + 9 * STRIDE, 0)));
    assert_eq!(st.endpoints, vec![1], "endpoint replayed");
    let status = hub.status();
    assert_eq!(status.ownership, Ownership::Owned);
    assert_eq!(status.ncp_restarts, 1);
    hub.shutdown();
    sim.stop();
}

#[test]
fn resync_resends_in_doubt_requests_with_retry() {
    let (hub, sim) = start(Store::volatile());
    assert!(hub.wait_ready(T));
    sim.with(|s| s.stall = Some(ids::SYS_ECHO));
    let ticket = hub.client().start(ids::SYS_ECHO, b"once".to_vec(), CallOpts::default()).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    sim.with(|s| s.stall = None);
    hub.client().reopen();
    let outcome = ticket.wait().unwrap();
    assert_eq!(&outcome.reply.payload[1..], b"once");
    let echoes: Vec<bool> =
        sim.state().requests.iter().filter(|(id, _)| *id == ids::SYS_ECHO).map(|(_, retry)| *retry).collect();
    assert_eq!(echoes, vec![false, true]);
    hub.shutdown();
    sim.stop();
}

#[test]
fn restart_fails_in_doubt_requests_with_link_reset() {
    let (hub, sim) = start(Store::volatile());
    assert!(hub.wait_ready(T));
    sim.with(|s| s.stall = Some(ids::SYS_ECHO));
    let ticket = hub.client().start(ids::SYS_ECHO, b"lost".to_vec(), CallOpts::default()).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    sim.with(|s| s.stall = None);
    sim.reboot();
    assert_eq!(ticket.wait().unwrap_err(), Error::LinkReset);
    assert_eq!(echo(&hub, b"after").unwrap(), b"after");
    hub.shutdown();
    sim.stop();
}

#[test]
fn aps_data_waits_for_the_confirm() {
    let (hub, sim) = start(Store::volatile());
    assert!(hub.wait_ready(T));
    hub.form(FormRequest::default()).unwrap();
    let req = ApsDataRequest {
        dst: ApsDst::Nwk(0x1234),
        dst_ep: 1,
        src_ep: 1,
        profile: 0x0104,
        cluster: 0x0006,
        tx_options: 0x01,
        radius: 0,
        non_member_radius: 0,
        priority: 1,
        relays: None,
        alias: None,
        asdu: &[0x01, 0x01, 0x02],
    };
    let confirm = hub.send_aps(&req, T).unwrap();
    assert_eq!((confirm.status, confirm.dst_nwk), (0, 0x1234));
    hub.shutdown();
    sim.stop();
}

fn image(version: u32, len: usize) -> (FwImage, Vec<u8>) {
    let data: Vec<u8> = (0..len).map(|i| (i * 7 + version as usize) as u8).collect();
    (FwImage { size: len as u32, sha256: sha256(&data), version, format: 1, flags: 0 }, data)
}

#[test]
fn firmware_update_is_staged_activated_and_confirmed() {
    let (hub, sim) = start(Store::volatile());
    assert!(hub.wait_ready(T));
    hub.form(FormRequest::default()).unwrap();
    let (img, data) = image(0x0001_0100, 5000);
    let slot = hub.fw_update(&img, &mut SliceSource(data)).unwrap();
    assert_eq!(slot, 1);
    assert!(hub.wait_status(T, |s| s.fw == FwState::Confirmed { version: 0x0001_0100 }), "{:?}", hub.status().fw);
    let st = sim.state();
    assert_eq!((st.running, st.trial), (1, false));
    assert!(hub.store_state().fw_pending.is_none());
    let slots = hub.fw_slots().unwrap();
    assert_eq!(slots[1].state, slot_state::RUNNING);
    // Confirmed images survive a reboot.
    reboot(&hub, &sim);
    assert!(hub.wait_ready(T));
    assert_eq!(echo(&hub, b"x").unwrap(), b"x");
    assert_eq!(sim.state().running, 1);
    hub.shutdown();
    sim.stop();
}

#[test]
fn rolled_back_update_is_reported() {
    let (hub, sim) = start(Store::volatile());
    assert!(hub.wait_ready(T));
    hub.form(FormRequest::default()).unwrap();
    sim.with(|s| s.crash_trial = true);
    let (img, data) = image(0x0002_0000, 1500);
    hub.fw_update(&img, &mut SliceSource(data)).unwrap();
    assert!(
        hub.wait_status(T, |s| s.fw == FwState::RolledBack { expected: 0x0002_0000, running: 0x0001_0000 }),
        "{:?}",
        hub.status().fw
    );
    assert_eq!(sim.state().running, 0);
    assert!(hub.store_state().fw_pending.is_none());
    // Larger than the 4 MB layout's slot: refused before FW_BEGIN.
    let big = FwImage { size: 0x1E_0001, ..img };
    let begins = sim.state().requests.iter().filter(|(id, _)| *id == ids::FW_BEGIN).count();
    assert_eq!(hub.fw_update(&big, &mut SliceSource(Vec::new())), Err(Error::Status(Status::TooLarge)));
    assert_eq!(sim.state().requests.iter().filter(|(id, _)| *id == ids::FW_BEGIN).count(), begins);
    hub.shutdown();
    sim.stop();
}

#[test]
fn held_trial_rolls_back_on_reset() {
    let mut cfg = HubConfig::default();
    cfg.resume.hold_trial = true;
    let (hub, sim) = start_with(Store::volatile(), cfg);
    assert!(hub.wait_ready(T));
    hub.form(FormRequest::default()).unwrap();
    // Same version as the running image: only the slot tells them apart.
    let (img, data) = image(0x0001_0000, 1500);
    assert_eq!(hub.fw_update(&img, &mut SliceSource(data)).unwrap(), 1);
    assert!(hub.wait_status(T, |s| s.slots.iter().any(|x| x.slot == 1 && x.state == slot_state::RUNNING_TRIAL)));
    assert!(sim.state().trial, "held, not confirmed");
    reboot(&hub, &sim);
    assert!(
        hub.wait_status(T, |s| s.fw == FwState::RolledBack { expected: 0x0001_0000, running: 0x0001_0000 }),
        "{:?}",
        hub.status().fw
    );
    assert_eq!(sim.state().running, 0);
    assert!(hub.store_state().fw_pending.is_none());
    hub.shutdown();
    sim.stop();
}

#[test]
fn store_survives_a_hub_restart() {
    let flash = Arc::new(std::sync::Mutex::new(MemFlash::new(32, 4096)));
    struct Shared(Arc<std::sync::Mutex<MemFlash>>);
    impl Flash for Shared {
        fn sector_size(&self) -> u32 {
            self.0.lock().unwrap().sector_size()
        }
        fn sectors(&self) -> u32 {
            self.0.lock().unwrap().sectors()
        }
        fn read(&mut self, o: u32, b: &mut [u8]) -> std::io::Result<()> {
            self.0.lock().unwrap().read(o, b)
        }
        fn write(&mut self, o: u32, d: &[u8]) -> std::io::Result<()> {
            self.0.lock().unwrap().write(o, d)
        }
        fn erase(&mut self, s: u32) -> std::io::Result<()> {
            self.0.lock().unwrap().erase(s)
        }
    }
    let (host, ncp) = pipe::pair();
    let sim = SimNcp::start(ncp);
    let (rx, tx) = host.split();
    let mut cfg = HubConfig::default();
    cfg.driver.lines = vec![Line::BR_DEFAULT];
    let hub = Hub::start(cfg.clone(), rx, tx, None, Store::open(Box::new(Shared(flash.clone()))).unwrap(), rng());
    assert!(hub.wait_ready(T));
    let net = hub.form(FormRequest::default()).unwrap();
    hub.shutdown();
    sim.stop();

    // New hub process, same NCP state (formed-down after its reboot).
    let (host, ncp) = pipe::pair();
    let sim2 = SimNcp::start(ncp);
    let st = sim.state();
    sim2.with(|s| {
        s.network_state = 1;
        s.pan_id = st.pan_id;
        s.ext_pan_id = st.ext_pan_id;
        s.channel = st.channel;
        s.key = st.key;
    });
    let (rx, tx) = host.split();
    let store = Store::open(Box::new(Shared(flash))).unwrap();
    assert_eq!(store.state().network.map(|n| n.pan_id), Some(net.pan_id));
    let hub = Hub::start(cfg, rx, tx, None, store, rng());
    assert!(hub.wait_status(T, |s| s.resumes > 0));
    assert_eq!(hub.status().ownership, Ownership::Owned);
    assert_eq!(sim2.state().network_state, 2);
    hub.shutdown();
    sim2.stop();
}
