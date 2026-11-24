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

#[test]
fn inventory_merges_ncp_table_children_and_neighbors() {
    use crate::inventory::source;
    let (hub, sim) = start(Store::volatile());
    assert!(hub.wait_ready(T));
    let (a, b, c, d, e) = ([0xA1; 8], [0xB2; 8], [0xC3; 8], [0xD4; 8], [0xE5; 8]);
    sim.with(|s| s.table_devices = vec![(a, 0x1111, Some(0x80)), (b, 0x2222, None), (c, 0x3333, None)]);
    // The master NCP image answers DIAG_GET_CHILDREN/NEIGHBORS UNSUPPORTED.
    let inv = hub.ncp_devices().unwrap();
    assert_eq!(inv.unsupported, vec![ids::DIAG_GET_CHILDREN, ids::DIAG_GET_NEIGHBORS]);
    assert_eq!(inv.devices.iter().map(|x| (x.ieee, x.nwk)).collect::<Vec<_>>(), vec![(a, 0x1111), (b, 0x2222), (c, 0x3333)]);

    let own = hub.client().session().unwrap().info.as_ref().unwrap().ieee_active;
    sim.with(|s| {
        s.children = Some(vec![(a, 0x1112, 0x8C), (d, 0x4444, 0x80)]);
        s.neighbors = Some(vec![(own, 0x0000, 0, 3), (d, 0x4444, 2, 1), (e, 0x5555, 1, 4)]);
    });
    let inv = hub.ncp_devices().unwrap();
    assert!(inv.unsupported.is_empty());
    let got: Vec<_> = inv.devices.iter().map(|x| (x.ieee, x.nwk, x.capability, x.sources)).collect();
    assert_eq!(
        got,
        vec![
            // A child's live address and capability win over the table's.
            (a, 0x1112, Some(0x8C), source::TABLE | source::CHILD),
            (b, 0x2222, None, source::TABLE),
            (c, 0x3333, None, source::TABLE),
            (d, 0x4444, Some(0x80), source::CHILD | source::NEIGHBOR),
        ],
        "the coordinator and a previous child are left out"
    );
    hub.shutdown();
    sim.stop();
}

fn formed(channel: u8) -> (Hub, SimNcp) {
    let (hub, sim) = start(Store::volatile());
    assert!(hub.wait_ready(T));
    hub.form(FormRequest { channel_mask: 1 << channel, ..FormRequest::default() }).unwrap();
    (hub, sim)
}

#[test]
fn energy_scan_reports_every_channel_of_the_mask() {
    let (hub, sim) = formed(15);
    let scan = hub.energy_scan(0x07FF_F800, 3, 1).unwrap();
    assert_eq!(scan.iter().map(|c| c.channel).collect::<Vec<_>>(), (11..=26).collect::<Vec<_>>());
    assert_eq!(scan[5].energy, sim.state().energy[5]);
    let two = hub.energy_scan((1 << 15) | (1 << 25), 2, 1).unwrap();
    assert_eq!(two.iter().map(|c| c.channel).collect::<Vec<_>>(), vec![15, 25]);
    // An NCP image without it answers UNSUPPORTED_COMMAND.
    sim.with(|s| s.unsupported.push(ids::NWK_ENERGY_SCAN));
    assert_eq!(hub.energy_scan(0x07FF_F800, 3, 1), Err(Error::Status(Status::UnsupportedCommand)));
    hub.shutdown();
    sim.stop();
}

#[test]
fn channel_change_moves_the_network_and_the_store() {
    let (hub, sim) = formed(15);
    let done = hub.change_channel(20).unwrap();
    assert_eq!((done.channel, done.nwk_update_id), (20, 1));
    assert_eq!(sim.state().channel, 20);
    let stored = hub.store_state().network.unwrap();
    assert_eq!((stored.channel, stored.nwk_update_id), (20, 1));
    let st = hub.status().network.unwrap();
    assert_eq!((st.channel, st.nwk_update_id), (20, 1));
    assert_eq!(hub.change_channel(27), Err(Error::Status(Status::InvalidParameter)));
    // The current channel completes without a new update ID.
    assert_eq!(hub.change_channel(20).unwrap().nwk_update_id, 1);
    // A resume after an NCP reboot keeps it.
    let resumes = hub.status().resumes;
    reboot(&hub, &sim);
    assert!(hub.wait_status(T, |s| s.resumes > resumes));
    assert_eq!(hub.status().ownership, Ownership::Owned);
    assert_eq!(hub.store_state().network.unwrap().channel, 20);
    hub.shutdown();
    sim.stop();
}

#[test]
fn tx_power_is_applied_persisted_and_replayed() {
    let (hub, sim) = formed(15);
    assert_eq!(hub.stored_tx_power(), None);
    assert_eq!(hub.set_tx_power(30).unwrap(), 20, "the NCP clamps to its limit");
    assert_eq!(hub.set_tx_power(-4).unwrap(), -4);
    assert_eq!(hub.stored_tx_power(), Some(-4));
    assert_eq!(hub.status().network.unwrap().tx_power, -4);
    sim.with(|s| s.tx_power = 8);
    let resumes = hub.status().resumes;
    reboot(&hub, &sim);
    assert!(hub.wait_status(T, |s| s.resumes > resumes));
    assert_eq!(sim.state().tx_power, -4, "replayed in the resume");
    hub.shutdown();
    sim.stop();
}

#[test]
fn zrhp_admin_zdo_requests_and_coordinator_tables() {
    use hub_coord::{CoordError, NetworkAdmin};

    use crate::coord::ZrhpAdmin;
    use crate::sim::SimRemote;

    let (hub, sim) = formed(15);
    let admin = ZrhpAdmin::new(hub.clone());
    sim.with(|s| {
        s.remotes = vec![SimRemote { nwk: 0x1111, ieee: [0x11; 8], neighbors: vec![([0x22; 8], 0x2222, 1, 2, 150)], routes: vec![] }];
        s.undeliverable = vec![(0x3333, 0xA7)];
    });
    let t = Duration::from_millis(500);
    // Mgmt_Lqi_req with TSN 0xA1: the response after its TSN.
    let rsp = admin.zdo_request(0x1111, 0x0031, &[0xA1, 0], t).unwrap();
    assert_eq!(&rsp[..4], &[0, 1, 0, 1], "status, total, start, count");
    assert_eq!(sim.state().aps.last().map(|p| p.ends_with(&[0xA1, 0])), Some(true), "the caller's TSN");
    assert_eq!(admin.zdo_request(0x2222, 0x0031, &[0xA2, 0], t), Err(CoordError::Timeout), "no response");
    match admin.zdo_request(0x3333, 0x0031, &[0xA3, 0], t) {
        Err(CoordError::NoAck(c)) => assert_eq!(c.value, 0xA7),
        r => panic!("{r:?}"),
    }
    let tables = admin.coordinator_tables(false);
    assert!(tables.routes.is_none());
    let e = tables.children.unwrap_err();
    assert_eq!((e.meaning(), e.to_string().as_str()), (&CoordError::Unsupported, "NCP status UnsupportedCommand"));
    hub.shutdown();
    sim.stop();
}

#[test]
fn diagnostics_counters_status_and_tables() {
    let (hub, sim) = formed(15);
    let link = hub.diag_counters(0x01).unwrap();
    assert!(link.iter().all(|(id, _)| id >> 8 == 1) && link.contains(&(0x0101, 120)));
    let all = hub.diag_counters(0xFF).unwrap();
    assert!(all.iter().any(|(id, _)| *id == 0x0701));
    hub.diag_reset_counters(0x01).unwrap();
    assert!(hub.diag_counters(0x01).unwrap().iter().all(|(_, v)| *v == 0));
    let st = hub.ncp_status().unwrap();
    assert_eq!((st.network_state, st.uptime_s), (2, 3600));
    let fc = hub.frame_counters().unwrap();
    assert_eq!(fc.stride, STRIDE);
    let (a, b, c, d, e) = ([0xA1; 8], [0xB2; 8], [0xC3; 8], [0xD4; 8], [0xE5; 8]);
    sim.with(|s| {
        s.neighbors = Some(vec![(a, 0x1111, 1, 2), (b, 0x2222, 1, 2), (c, 0x3333, 2, 1), (d, 0x4444, 2, 1), (e, 0x5555, 1, 4)]);
        s.routes = vec![(0x1111, 0x1111, 0), (0x6666, 0x2222, 0), (0x7777, 0x1111, 1)];
        s.children = Some(vec![(c, 0x3333, 0x80), (d, 0x4444, 0x80)]);
    });
    let n = hub.coordinator_neighbors().unwrap();
    assert_eq!(n.iter().map(|x| x.nwk).collect::<Vec<_>>(), vec![0x1111, 0x2222, 0x3333, 0x4444, 0x5555], "all pages");
    let r = hub.coordinator_routes().unwrap();
    assert_eq!(r.iter().map(|x| (x.dst, x.next_hop)).collect::<Vec<_>>(), vec![(0x1111, 0x1111), (0x6666, 0x2222), (0x7777, 0x1111)]);
    assert_eq!(hub.coordinator_children().unwrap().len(), 2);
    assert!(hub.maintenance_caps().contains(&"energy_scan"));
    hub.shutdown();
    sim.stop();
}

#[test]
fn backup_exports_and_restores_onto_a_fresh_ncp() {
    use zigbee_zrhp::records::{RawRecords, Record, verify_stream};
    let (hub, sim) = formed(17);
    let (a, b) = ([0xA1; 8], [0xB2; 8]);
    sim.with(|s| {
        s.table_devices = vec![(a, 0x1111, Some(0x80)), (b, 0x2222, None)];
        s.link_keys = vec![(a, [0x42; 16])];
    });
    let net = hub.store_state().network.unwrap();
    let stream = hub.backup_export(true).unwrap();
    assert_eq!(verify_stream(&stream).unwrap(), 7, "node, network, key, TCLK, 2 devices, RS_PARAMS");
    let has_key = RawRecords::new(&stream)
        .flatten()
        .any(|(_, tlv)| matches!(zigbee_zrhp::codec::Reader::new(tlv).read::<Record>(), Ok(Record::NetworkKey(k)) if k.key == net.network_key));
    assert!(has_key);
    let without = hub.backup_export(false).unwrap();
    assert_eq!(verify_stream(&without).unwrap(), 5, "no key records");
    let limit = sim.state().nwk_limit;
    hub.shutdown();
    sim.stop();

    // A fresh NCP and an empty store.
    let (hub2, sim2) = start(Store::volatile());
    assert!(hub2.wait_ready(T));
    assert!(hub2.wait_status(T, |s| s.ownership == Ownership::NoNetwork));
    let report = hub2.backup_restore(&stream, crate::lifecycle::RestoreOptions::default()).unwrap();
    let st = sim2.state();
    assert_eq!((st.network_state, st.pan_id, st.channel, st.key), (2, net.pan_id, 17, net.network_key));
    assert_eq!(st.table_devices.len(), 2);
    assert!(report.nwk_fc_start >= limit + 10_000, "start above the exported limit plus the margin");
    assert_eq!((report.children, report.link_keys_on_ncp), (1, 1));
    let stored = hub2.store_state().network.unwrap();
    assert_eq!((stored.pan_id, stored.ext_pan_id, stored.network_key), (net.pan_id, net.ext_pan_id, net.network_key));
    assert!(hub2.store_state().fc[0].unwrap().limit >= report.nwk_fc_start);
    assert_eq!(hub2.status().ownership, Ownership::Owned);
    // Survives an NCP reboot as the owned network.
    let resumes = hub2.status().resumes;
    reboot(&hub2, &sim2);
    assert!(hub2.wait_status(T, |s| s.resumes > resumes));
    assert_eq!(hub2.status().ownership, Ownership::Owned);
    hub2.shutdown();
    sim2.stop();
}

#[test]
fn leave_reset_then_form_a_new_network() {
    let (hub, sim) = formed(15);
    let old = hub.store_state().network.unwrap();
    assert_eq!(hub.form(FormRequest::default()).unwrap_err(), Error::Status(Status::AlreadyExists));
    hub.leave_reset().unwrap();
    assert!(hub.store_state().network.is_none());
    assert_eq!(hub.status().ownership, Ownership::NoNetwork);
    assert_eq!(sim.state().network_state, 0);
    let flags = sim.state().log.iter().find(|(id, _)| *id == ids::NWK_LEAVE_RESET).map(|(_, p)| p.clone());
    assert_eq!(flags, Some(vec![0x03]), "keep the IEEE override and the host endpoints");
    let new = hub.form(FormRequest { channel_mask: 1 << 25, pan_id: Some(0x1A2B), ..FormRequest::default() }).unwrap();
    assert_eq!((new.pan_id, new.channel), (0x1A2B, 25));
    assert_ne!(new.network_key, old.network_key);
    assert_eq!(hub.status().ownership, Ownership::Owned);
    hub.shutdown();
    sim.stop();
}
