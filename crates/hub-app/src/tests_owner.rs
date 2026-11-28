//! The ownership tag of the hub's records end to end (design §7 row I):
//! the first start of a tagging build on PR A's untagged journal, a start
//! on records that are not this hub's, the operator's adopt and clear, and
//! records that turn foreign between their loads at the start.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::Receiver;

use hub_coord::{BackendKind, CoordStatus, OwnerTag, RegistryState};
use hub_ncp::coord::owner;
use hub_store::kv::{Guarded, Kv, scope};
use hub_store::{Flash, Gate, Journal, JournalStats, KvStore, OWNER_KEY, Scope, Store};
use serde_json::{Value as J, json};

use crate::backend::Report;
use crate::{App, Backend, Records};
use crate::baseline;
use crate::tests::{T, call, cfg, form, ok, rig, rig_with, session};
use crate::tests_baseline::SharedMem;

const KV_PUT: u8 = 0x20;
const KV_DEL: u8 = 0x21;

/// PR A's journal at the end of its scenario, as builds before the tag
/// wrote it: a network, two devices, their last values, the last backup.
fn untagged() -> SharedMem {
    let text = std::fs::read_to_string(baseline::path("untagged_store_end.hex")).unwrap();
    let mem = SharedMem::new(32, 4096);
    mem.flash.lock().unwrap().write(0, &baseline::unhexdump(&text)).unwrap();
    mem
}

/// [`untagged`] with a time zone, tagged as an EZSP image's records (that
/// image ran on this `hubdata`).
fn foreign() -> SharedMem {
    let mem = untagged();
    let mut store = Store::open(Box::new(mem.clone())).unwrap();
    store.kv_put("cfg/tz", b"UTC0").unwrap();
    let tag = OwnerTag::new(BackendKind::Ezsp, owner::identity(store.state()));
    store.kv_put(OWNER_KEY, tag.encode().as_bytes()).unwrap();
    mem.take_log();
    mem
}

fn kv(mem: &SharedMem) -> BTreeMap<String, Vec<u8>> {
    Journal::open(mem.flash.lock().unwrap().clone()).unwrap().state().kv.clone()
}

fn key_records(log: &[(u8, Vec<u8>)]) -> Vec<String> {
    log.iter()
        .filter(|(t, _)| *t == KV_PUT || *t == KV_DEL)
        .map(|(t, p)| format!("{t:02x} {}", String::from_utf8_lossy(&p[1..1 + usize::from(p[0])])))
        .collect()
}

fn network_keys(kv: &BTreeMap<String, Vec<u8>>) -> Vec<&str> {
    kv.keys().map(String::as_str).filter(|k| scope(k) == Scope::Network).collect()
}

fn devices(app: &App, sid: u64, rx: &Receiver<String>) -> Vec<String> {
    let list = call(app, sid, rx, 50, json!({"type": "devices/list"}));
    ok(&list).as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect()
}

fn registry(app: &App, sid: u64, rx: &Receiver<String>) -> J {
    ok(&call(app, sid, rx, 51, json!({"type": "network/info"})))["registry"].clone()
}

#[test]
fn untagged_journal_gains_only_the_tag() {
    let mem = untagged();
    let old = mem.bytes();
    let mut store = Store::open(Box::new(mem.clone())).unwrap();
    let before = store.state().clone();
    assert!(before.network.is_some() && !before.kv.contains_key(OWNER_KEY));
    assert_eq!(owner::open(&mut store, true), RegistryState::Owned);

    // One record, the tag of the stored network.
    assert_eq!(key_records(&mem.take_log()), [format!("20 {OWNER_KEY}")]);
    let tag = OwnerTag::decode(store.kv_get(OWNER_KEY).unwrap()).unwrap();
    assert_eq!(tag, OwnerTag::new(BackendKind::Zrhp, owner::identity(&before)));
    assert!(tag.identity.unwrap().key_id.is_some());

    // Programmed into erased flash: every other byte is as it was.
    let new = mem.bytes();
    let changed: Vec<usize> = (0..old.len()).filter(|i| old[*i] != new[*i]).collect();
    let (first, last) = (changed[0], *changed.last().unwrap());
    assert!(old[first..=last].iter().all(|b| *b == 0xFF), "{first}..={last}");
    let mut want = before;
    want.kv.insert(OWNER_KEY.into(), tag.encode().into_bytes());
    assert_eq!(Journal::open(mem.flash.lock().unwrap().clone()).unwrap().state(), &want);

    // Started again: nothing more.
    drop(store);
    let mut store = Store::open(Box::new(mem.clone())).unwrap();
    assert_eq!(owner::open(&mut store, true), RegistryState::Owned);
    assert_eq!(mem.bytes(), new);
}

#[test]
fn a_foreign_start_writes_and_deletes_nothing() {
    let mem = foreign();
    let before = kv(&mem);
    assert_eq!(network_keys(&before).len(), 5, "{before:?}");
    let r = rig(Store::open(Box::new(mem.clone())).unwrap());
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.endpoint_registered));
    let (sid, rx) = session(&app);
    let reg = registry(&app, sid, &rx);
    assert_eq!((reg["state"].as_str(), reg["backend"].as_str()), (Some("foreign"), Some("ezsp")), "{reg}");
    assert_eq!(reg["why"], "another coordinator backend");
    assert_eq!(devices(&app, sid, &rx), Vec::<String>::new(), "not loaded");
    assert_eq!(app.status().restored, 0);
    // The hub's own records are read and written as usual.
    let tz = call(&app, sid, &rx, 2, json!({"type": "hub/time"}));
    assert_eq!(ok(&tz)["tz"], "UTC0", "{tz}");
    // The registry is written at the stop (a full sweep).
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
    assert_eq!(key_records(&mem.take_log()), Vec::<String>::new(), "no key/value record written or deleted");
    assert_eq!(kv(&mem), before);
}

#[test]
fn adopt_and_clear_need_confirm_and_do_what_they_say() {
    let mem = foreign();
    let r = rig(Store::open(Box::new(mem.clone())).unwrap());
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.endpoint_registered));
    let (sid, rx) = session(&app);
    for (i, body) in [
        json!({"type": "network/registry/adopt"}),
        json!({"type": "network/registry/adopt", "confirm": false}),
        json!({"type": "network/registry/clear", "confirm": "yes"}),
        json!({"type": "network/registry/clear"}),
    ]
    .into_iter()
    .enumerate()
    {
        let f = call(&app, sid, &rx, 10 + i as u64, body);
        assert_eq!((&f["success"], &f["error"]["code"]), (&J::Bool(false), &J::from("invalid_value")), "{f}");
    }
    assert_eq!(registry(&app, sid, &rx)["state"], "foreign");
    assert_eq!(key_records(&mem.take_log()), Vec::<String>::new());

    // Adopt: the records become this hub's (its stored network) and load.
    let a = call(&app, sid, &rx, 20, json!({"type": "network/registry/adopt", "confirm": true}));
    assert_eq!(ok(&a)["restored"], 2, "{a}");
    assert_eq!(ok(&a)["registry"], json!({"state": "owned"}));
    assert_eq!(devices(&app, sid, &rx), ["gp:00f40002", "zb:5a52530000000001"]);
    let tag = OwnerTag::decode(&kv(&mem)[OWNER_KEY]).unwrap();
    assert_eq!(tag, OwnerTag::new(BackendKind::Zrhp, owner::identity(&r.ncp.store_state())));
    assert_eq!(key_records(&mem.take_log()), [format!("20 {OWNER_KEY}")]);

    // Clear: the network's records go, in the store and the engine; the
    // hub's own stay.
    let c = call(&app, sid, &rx, 21, json!({"type": "network/registry/clear", "confirm": true}));
    assert_eq!((&ok(&c)["deleted"], &ok(&c)["forgotten"]), (&J::from(5), &J::from(2)), "{c}");
    assert_eq!(ok(&c)["registry"]["state"], "owned");
    assert_eq!(devices(&app, sid, &rx), Vec::<String>::new());
    // Written as before a reboot (the test has no platform to reboot).
    call(&app, sid, &rx, 22, json!({"type": "hub/reboot"}));
    let after = kv(&mem);
    assert_eq!(network_keys(&after), Vec::<&str>::new(), "{after:?}");
    assert_eq!(after["cfg/tz"], b"UTC0");
    assert_eq!(OwnerTag::decode(&after[OWNER_KEY]).unwrap(), tag);
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

#[test]
fn a_network_formed_over_foreign_records_takes_them() {
    let mem = foreign();
    let r = rig(Store::open(Box::new(mem.clone())).unwrap());
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.endpoint_registered));
    let (sid, rx) = session(&app);
    assert_eq!(devices(&app, sid, &rx), Vec::<String>::new());
    // `network/reset` is explicit: the records become the new network's
    // (devices must join again) and are loaded.
    form(&app, sid, &rx, 20);
    assert_eq!(registry(&app, sid, &rx)["state"], "owned");
    assert!(app.wait(T, |s| s.restored == 2), "{:?}", app.status());
    assert_eq!(devices(&app, sid, &rx), ["gp:00f40002", "zb:5a52530000000001"]);
    let tag = OwnerTag::decode(&kv(&mem)[OWNER_KEY]).unwrap();
    assert_eq!(tag.identity.unwrap().channel, 20);
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
    assert_eq!(network_keys(&kv(&mem)).len(), 5, "kept");
}

#[test]
fn a_backup_while_foreign_is_returned_but_not_kept() {
    let mem = foreign();
    let store = Store::open(Box::new(mem.clone())).unwrap();
    let n = store.state().network.unwrap();
    let before = kv(&mem);
    // The NCP runs the stored network: the backend owns it, the records
    // stay another backend's.
    let r = rig_with(store, |sim| {
        sim.with(|s| (s.network_state, s.pan_id, s.ext_pan_id, s.channel, s.key) = (2, n.pan_id, n.ext_pan_id, n.channel, n.network_key))
    });
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.endpoint_registered));
    let (sid, rx) = session(&app);
    let info = ok(&call(&app, sid, &rx, 1, json!({"type": "network/info"}))).clone();
    assert_eq!((&info["ownership"], &info["registry"]["state"]), (&J::from("owned"), &J::from("foreign")), "{info}");
    let b = call(&app, sid, &rx, 2, json!({"type": "network/backup"}));
    assert_eq!(ok(&b)["metadata"]["format"], "zigpy/open-coordinator-backup", "{b}");
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
    assert_eq!(kv(&mem), before, "the other network's last backup stays");
}

/// Frames until the result of request `id`, which is sent after `body`.
fn frames_until(app: &App, sid: u64, rx: &Receiver<String>, id: u64, body: J) -> Vec<J> {
    let mut body = body;
    body["id"] = id.into();
    app.text(sid, body.to_string());
    let mut out = Vec::new();
    loop {
        let f: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
        let done = f["type"] == "result" && f["id"] == id;
        out.push(f);
        if done {
            return out;
        }
    }
}

#[test]
fn adopt_keeps_the_devices_imported_meanwhile_and_their_last_values() {
    let mem = foreign();
    let store = Store::open(Box::new(mem.clone())).unwrap();
    let n = store.state().network.unwrap();
    let plug = "zb:5a52530000000001";
    let st = format!("st/{plug}");
    let stored_st = kv(&mem)[&st].clone();
    // The NCP runs the stored network and knows the plug: the app imports
    // it while the records are foreign.
    let r = rig_with(store, |sim| {
        sim.with(|s| {
            (s.network_state, s.pan_id, s.ext_pan_id, s.channel, s.key) = (2, n.pan_id, n.ext_pan_id, n.channel, n.network_key);
            s.table_devices = vec![(0x5a52530000000001u64.to_le_bytes(), 0x5A01, None)];
        })
    });
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.imported == 1), "{:?}", app.status());
    let (sid, rx) = session(&app);
    assert_eq!(devices(&app, sid, &rx), [plug]);
    ok(&call(&app, sid, &rx, 1, json!({"type": "subscribe", "topics": ["devices"]})));

    let frames = frames_until(&app, sid, &rx, 3, json!({"type": "network/registry/adopt", "confirm": true}));
    let frames = [frames, frames_until(&app, sid, &rx, 4, json!({"type": "devices/list"}))].concat();
    let topics: Vec<&str> = frames.iter().filter_map(|f| f["event"]["topic"].as_str()).collect();
    assert!(!topics.contains(&"device_removed"), "{topics:?}");
    assert_eq!(topics.iter().filter(|t| **t == "device_added").count(), 1, "only the GP switch is new: {topics:?}");
    assert_eq!(devices(&app, sid, &rx), ["gp:00f40002", plug]);
    // Written as before a reboot: the plug's last values stay.
    call(&app, sid, &rx, 5, json!({"type": "hub/reboot"}));
    let after = kv(&mem);
    assert!(after.contains_key(&st), "{after:?}");
    let values = |b: &[u8]| serde_json::from_slice::<J>(b).unwrap()["e"].clone();
    assert_eq!(values(&after[&st]), values(&stored_st));
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

#[test]
fn rules_cannot_change_while_the_records_are_foreign() {
    let mem = foreign();
    let r = rig(Store::open(Box::new(mem.clone())).unwrap());
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.endpoint_registered));
    let (sid, rx) = session(&app);
    let rule = json!({"id": "night", "triggers": [{"type": "time", "cron": "0 22 * * *"}],
        "actions": [{"type": "request", "request": {"type": "network/permit_join", "duration_s": 0}}]});
    let create = json!({"type": "automations/create", "rule": rule});
    let f = call(&app, sid, &rx, 1, create.clone());
    assert_eq!(f["error"]["code"], "registry_read_only", "{f}");
    assert!(f["error"]["message"].as_str().unwrap().contains("foreign"), "{f}");
    let f = call(&app, sid, &rx, 2, json!({"type": "automations/enable", "automation": "night"}));
    assert_eq!(f["error"]["code"], "registry_read_only", "{f}");
    // Reading and validating still work.
    ok(&call(&app, sid, &rx, 3, json!({"type": "automations/list"})));
    assert_eq!(ok(&call(&app, sid, &rx, 4, json!({"type": "automations/validate", "rule": rule})))["ok"], true);
    assert_eq!(key_records(&mem.take_log()), Vec::<String>::new());
    // Once adopted, rules are stored.
    ok(&call(&app, sid, &rx, 5, json!({"type": "network/registry/adopt", "confirm": true})));
    ok(&call(&app, sid, &rx, 6, create));
    call(&app, sid, &rx, 7, json!({"type": "hub/reboot"}));
    assert!(kv(&mem).contains_key("auto/night"));
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

/// The OTA server's device states (`ota/d/`) are network records: not
/// written while foreign (requests that would are refused), loaded on
/// adopt, dropped by clear.
#[cfg(feature = "ota")]
#[test]
fn ota_device_states_follow_the_registry() {
    let mem = foreign();
    let mut store = Store::open(Box::new(mem.clone())).unwrap();
    let n = store.state().network.unwrap();
    let plug = "zb:5a52530000000001";
    let key = format!("ota/d/{plug}");
    let st = hub_ota::server::DevState { policy: Some(hub_ota::Policy::Off), ..Default::default() };
    store.kv_put(&key, &st.to_bytes()).unwrap();
    mem.take_log();
    let r = rig_with(store, |sim| {
        sim.with(|s| {
            (s.network_state, s.pan_id, s.ext_pan_id, s.channel, s.key) = (2, n.pan_id, n.ext_pan_id, n.channel, n.network_key);
            s.table_devices = vec![(0x5a52530000000001u64.to_le_bytes(), 0x5A01, None)];
        })
    });
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.imported == 1), "{:?}", app.status());
    let (sid, rx) = session(&app);
    let ota_list = |id| ok(&call(&app, sid, &rx, id, json!({"type": "ota/devices/list"}))).clone();
    assert_eq!(ota_list(1), json!([]), "not loaded while foreign");
    for (i, ty) in ["ota/devices/policy", "ota/start"].into_iter().enumerate() {
        let f = call(&app, sid, &rx, 2 + i as u64, json!({"type": ty, "device": plug, "policy": "auto"}));
        assert_eq!(f["error"]["code"], "registry_read_only", "{f}");
    }
    call(&app, sid, &rx, 4, json!({"type": "hub/reboot"}));
    assert_eq!(key_records(&mem.take_log()), Vec::<String>::new());

    // Adopted: the stored state loads.
    ok(&call(&app, sid, &rx, 5, json!({"type": "network/registry/adopt", "confirm": true})));
    let l = ota_list(6);
    assert_eq!((l[0]["device"].as_str(), l[0]["policy"].as_str()), (Some(plug), Some("off")), "{l}");

    // Cleared: gone from the store and the server; a new policy is written.
    ok(&call(&app, sid, &rx, 7, json!({"type": "network/registry/clear", "confirm": true})));
    assert!(app.wait(T, |s| s.imported == 2), "{:?}", app.status());
    assert_eq!(ota_list(8), json!([]));
    assert!(!kv(&mem).contains_key(&key));
    ok(&call(&app, sid, &rx, 9, json!({"type": "ota/devices/policy", "device": plug, "policy": "manual"})));
    call(&app, sid, &rx, 10, json!({"type": "hub/reboot"}));
    let stored = hub_ota::server::DevState::from_bytes(&kv(&mem)[&key], 0).unwrap();
    assert_eq!(stored.policy, Some(hub_ota::Policy::Manual));
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

/// While the records are foreign the devices' own policies are unknown (a
/// stored `off` among them): the hub-wide `auto` starts nothing; adopted,
/// the stored policy applies.
#[cfg(feature = "ota")]
#[test]
fn while_foreign_the_hub_wide_auto_starts_no_download() {
    use std::sync::{Arc, Mutex};

    use base64::Engine as _;
    use hub_ncp::sim::{SimAps, indication};
    use hub_ota::sim::{SimClient, frame};
    use zigbee_zrhp::msg::ids;

    let mem = foreign();
    let mut store = Store::open(Box::new(mem.clone())).unwrap();
    let n = store.state().network.unwrap();
    let (ieee, nwk) = (0x5a52_5300_0000_0001u64, 0x5A01u16);
    let plug = format!("zb:{ieee:016x}");
    let off = hub_ota::server::DevState { policy: Some(hub_ota::Policy::Off), ..Default::default() };
    store.kv_put(&format!("ota/d/{plug}"), &off.to_bytes()).unwrap();
    let r = rig_with(store, |sim| {
        sim.with(|s| {
            (s.network_state, s.pan_id, s.ext_pan_id, s.channel, s.key) = (2, n.pan_id, n.ext_pan_id, n.channel, n.network_key);
            s.table_devices = vec![(ieee.to_le_bytes(), nwk, None)];
        })
    });
    let client = Arc::new(Mutex::new(SimClient::new(0x6D6B, 1, 0x0001_0000)));
    let c = client.clone();
    r.sim.set_aps_hook(Box::new(move |a: &SimAps<'_>| {
        if a.dst != nwk || a.cluster != 0x0019 {
            return Vec::new();
        }
        let frames = c.lock().unwrap().on_zcl(a.asdu, 0);
        frames.into_iter().map(|f| indication(nwk, ieee, a.dst_ep, a.src_ep, 0x0104, 0x0019, &f)).collect()
    }));
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.imported == 1), "{:?}", app.status());
    let (sid, rx) = session(&app);
    ok(&call(&app, sid, &rx, 1, json!({"type": "ota/settings/set", "policy": "auto"})));
    let file = hub_ota::file::build(0x6D6B, 1, 0x0001_0100, "", &[7; 3000], None);
    let header = base64::engine::general_purpose::STANDARD.encode(&file[..256]);
    ok(&call(&app, sid, &rx, 2, json!({"type": "ota/stream/offer", "size": file.len(), "header": header, "name": "p.ota"})));

    // The plug asks: recorded, not offered.
    let q = frame(client.lock().unwrap().query());
    r.sim.inject_event(ids::EVT_APS_DATA_INDICATION, indication(nwk, ieee, 1, 1, 0x0104, 0x0019, &q));
    let t = std::time::Instant::now();
    let row = loop {
        let l = ok(&call(&app, sid, &rx, 3, json!({"type": "ota/devices/list"}))).clone();
        if l[0]["current_version"] == 0x0001_0000 {
            break l[0].clone();
        }
        assert!(t.elapsed() < T, "{l}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(row["available"].is_object(), "{row}");
    assert_eq!((&row["effective_policy"], &row["job"]), (&J::from("manual"), &J::Null), "{row}");
    assert_eq!(client.lock().unwrap().upgrades, 0);

    // Adopted: the stored policy.
    ok(&call(&app, sid, &rx, 4, json!({"type": "network/registry/adopt", "confirm": true})));
    let l = ok(&call(&app, sid, &rx, 5, json!({"type": "ota/devices/list"}))).clone();
    assert_eq!((&l[0]["policy"], &l[0]["effective_policy"]), (&J::from("off"), &J::from("off")), "{l}");
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

/// Records of the host core that turn foreign during the start, right
/// after the app read the `after` records `reads` times (0: never; the gate closes
/// between two kinds of network records), until adopted, cleared or
/// [`settle`]d; or, with `start_foreign`, that are foreign until then.
///
/// [`settle`]: Racy::settle
struct Racy {
    hub: hub_ncp::Hub,
    after: &'static str,
    reads: u8,
    start_foreign: bool,
    /// 0: before, 1: foreign, 2: as the host core says.
    phase: AtomicU8,
    hits: AtomicU8,
}

impl Racy {
    fn foreign(&self) -> bool {
        match self.phase.load(Ordering::SeqCst) {
            0 => self.start_foreign,
            1 => true,
            _ => false,
        }
    }

    /// The records are the host core's again, without the operator.
    fn settle(&self) {
        self.phase.store(2, Ordering::SeqCst);
    }
}

/// Notes a read of the `after` records.
struct Spy<'a> {
    kv: &'a mut dyn Kv,
    after: &'a str,
    hit: &'a Cell<bool>,
}

impl Kv for Spy<'_> {
    fn get(&self, key: &str) -> Option<&[u8]> {
        self.kv.get(key)
    }
    fn prefix<'a>(&'a self, prefix: &'a str) -> Box<dyn Iterator<Item = (&'a str, &'a [u8])> + 'a> {
        self.hit.set(self.hit.get() || prefix == self.after);
        self.kv.prefix(prefix)
    }
    fn put(&mut self, key: &str, value: &[u8]) -> io::Result<()> {
        self.kv.put(key, value)
    }
    fn delete(&mut self, key: &str) -> io::Result<()> {
        self.kv.delete(key)
    }
    fn gate(&self) -> Gate {
        self.kv.gate()
    }
}

impl KvStore for Racy {
    fn with_kv(&self, f: &mut dyn FnMut(&mut dyn Kv)) {
        let (foreign, hit) = (self.foreign(), Cell::new(false));
        self.hub.with_kv(&mut |kv| {
            let mut closed;
            let kv: &mut dyn Kv = if foreign {
                closed = Guarded::new(kv, Gate::Closed);
                &mut closed
            } else {
                kv
            };
            f(&mut Spy { kv, after: self.after, hit: &hit });
        });
        if hit.get() && self.hits.fetch_add(1, Ordering::SeqCst) + 1 == self.reads {
            let next = if self.start_foreign { 2 } else { 1 };
            let _ = self.phase.compare_exchange(0, next, Ordering::SeqCst, Ordering::SeqCst);
        }
    }
    fn stats(&self) -> JournalStats {
        self.hub.stats()
    }
}

impl Records for Racy {
    fn registry(&self) -> RegistryState {
        if self.foreign() { RegistryState::Foreign { backend: "zrhp", why: "another network" } } else { self.hub.registry() }
    }
    fn adopt(&self, now: Report<'_>) -> io::Result<RegistryState> {
        self.settle();
        Records::adopt(&self.hub, now)
    }
    fn clear(&self, now: Report<'_>) -> io::Result<usize> {
        self.settle();
        Records::clear(&self.hub, now)
    }
    fn system(&self, st: &CoordStatus) -> (J, J) {
        Records::system(&self.hub, st)
    }
}

const RACE_PLUG: &str = "zb:5a52530000000001";

/// The app on [`untagged`] records with one record of each kind network
/// records load as (the plug's last values are there already): the group
/// `Kitchen`, the rule `night`, the plug's OTA state with policy `auto`;
/// behind [`Racy`] records.
fn racy_start(after: &'static str, reads: u8, start_foreign: bool) -> (SharedMem, crate::tests::Rig, Arc<Racy>, App) {
    racy_start_with(after, reads, start_foreign, &[])
}

/// [`racy_start`] with `extra` records in the store.
fn racy_start_with(
    after: &'static str,
    reads: u8,
    start_foreign: bool,
    extra: &[(&str, J)],
) -> (SharedMem, crate::tests::Rig, Arc<Racy>, App) {
    let mem = untagged();
    let mut store = Store::open(Box::new(mem.clone())).unwrap();
    for (k, v) in extra {
        store.kv_put(k, v.to_string().as_bytes()).unwrap();
    }
    store.kv_put("grp/0001", br#"{"id":1,"name":"Kitchen","hub_member":false,"members":[],"scenes":[]}"#).unwrap();
    store.kv_put("auto/night", rule("night").to_string().as_bytes()).unwrap();
    #[cfg(feature = "ota")]
    {
        let st = hub_ota::server::DevState { policy: Some(hub_ota::Policy::Auto), ..Default::default() };
        store.kv_put(&format!("ota/d/{RACE_PLUG}"), &st.to_bytes()).unwrap();
    }
    let r = rig(store);
    let racy = Arc::new(Racy { hub: r.ncp.clone(), after, reads, start_foreign, phase: AtomicU8::new(0), hits: AtomicU8::new(0) });
    let mut backend = Backend::zrhp(r.ncp.clone());
    backend.kv = racy.clone();
    let app = App::start_backend(backend, cfg(), None);
    assert!(app.wait(T, |s| s.endpoint_registered));
    (mem, r, racy, app)
}

fn rule(id: &str) -> J {
    json!({"id": id, "triggers": [{"type": "time", "cron": "0 22 * * *"}],
        "actions": [{"type": "request", "request": {"type": "network/permit_join", "duration_s": 0}}]})
}

/// What of the network records the app has: `[groups and bindings, last
/// values, rules, OTA device states]` (see [`racy_start`]).
fn loaded(app: &App, sid: u64, rx: &Receiver<String>) -> [bool; 4] {
    let groups = ok(&call(app, sid, rx, 60, json!({"type": "groups/list"})))[0]["name"] == "Kitchen";
    let last = app.status().last_values.0 > 0;
    let rules = ok(&call(app, sid, rx, 61, json!({"type": "automations/list"})))["rules"][0]["id"] == "night";
    #[cfg(feature = "ota")]
    let ota = {
        let l = ok(&call(app, sid, rx, 62, json!({"type": "ota/devices/list"}))).clone();
        let row = l.as_array().unwrap().iter().find(|r| r["device"] == RACE_PLUG).cloned();
        // Loaded, and the hold lifted: `auto` acts as `auto` again.
        row.is_some_and(|r| (&r["policy"], &r["effective_policy"]) == (&J::from("auto"), &J::from("auto")))
    };
    #[cfg(not(feature = "ota"))]
    let ota = true;
    [groups, last, rules, ota]
}

/// Until every kind is loaded (a tick loads what became readable).
fn all_loaded(app: &App, sid: u64, rx: &Receiver<String>) -> [bool; 4] {
    let t = std::time::Instant::now();
    loop {
        let l = loaded(app, sid, rx);
        if l == [true; 4] || t.elapsed() > T {
            return l;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Written once loaded: a new rule is stored, the other records kept.
fn written_and_kept(app: &App, sid: u64, rx: &Receiver<String>, mem: &SharedMem) {
    ok(&call(app, sid, rx, 70, json!({"type": "automations/create", "rule": rule("late")})));
    call(app, sid, rx, 71, json!({"type": "hub/reboot"}));
    let kv = kv(mem);
    let mut want = vec!["auto/late".to_string(), "auto/night".into(), "grp/0001".into(), format!("st/{RACE_PLUG}")];
    if cfg!(feature = "ota") {
        want.push(format!("ota/d/{RACE_PLUG}"));
    }
    for k in want {
        assert!(kv.contains_key(&k), "{k}: {kv:?}");
    }
}

/// The records turn foreign right after the start read `after`; then they
/// are adopted (`adopt`) or become readable again by themselves: every
/// kind the start did not load is loaded then (`at_start`: what it did).
fn gate_closes_during_the_start(after: &'static str, mut at_start: [bool; 4], adopt: bool) {
    // Without Zigbee OTA there are no OTA states.
    at_start[3] |= cfg!(not(feature = "ota"));
    let (mem, r, racy, app) = racy_start(after, 1, false);
    let (sid, rx) = session(&app);
    assert!(racy.foreign(), "the gate closed after {after}");
    assert_eq!(registry(&app, sid, &rx)["state"], "foreign");
    assert_eq!(loaded(&app, sid, &rx), at_start, "after {after}: loaded at the start");
    if adopt {
        ok(&call(&app, sid, &rx, 1, json!({"type": "network/registry/adopt", "confirm": true})));
    } else {
        racy.settle();
    }
    assert_eq!(all_loaded(&app, sid, &rx), [true; 4], "after {after}, adopt {adopt}");
    written_and_kept(&app, sid, &rx, &mem);
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

#[test]
fn records_hidden_after_the_devices_load_on_adopt() {
    // Groups and bindings are read with the devices; the rest after.
    gate_closes_during_the_start("dev/", [true, false, false, false], true);
}

#[test]
fn records_hidden_after_the_last_values_load_on_adopt() {
    gate_closes_during_the_start("st/", [true, true, false, false], true);
}

#[cfg(feature = "ota")]
#[test]
fn ota_states_hidden_after_the_rules_load_on_adopt() {
    gate_closes_during_the_start("auto/", [true, true, true, false], true);
}

#[test]
fn records_hidden_during_the_start_load_once_readable() {
    gate_closes_during_the_start("dev/", [true, false, false, false], false);
}

/// The other way round: hidden when the devices were read (by the start's
/// registry and again with the rest), readable right after. The devices'
/// last values and OTA states wait for them (a value of a device not
/// restored yet would be lost, its OTA state deleted).
#[test]
fn records_readable_after_the_devices_wait_for_them() {
    let (mem, r, racy, app) = racy_start("dev/", 2, true);
    let (sid, rx) = session(&app);
    assert!(!racy.foreign());
    assert_eq!(all_loaded(&app, sid, &rx), [true; 4]);
    written_and_kept(&app, sid, &rx, &mem);
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

/// Last values are network records too: while the kept ones are not
/// loaded (hidden right after the devices loaded) none is written and no
/// write counts as failed; what changed meanwhile stays noted and is
/// written once they are loaded with the gate open.
#[test]
fn last_values_changed_before_they_load_wait_for_them() {
    use hub_ncp::sim::indication;
    use zigbee_zrhp::msg::ids;

    let (mem, r, racy, app) = racy_start("dev/", 1, false);
    let (sid, rx) = session(&app);
    assert!(racy.foreign());
    let key = format!("st/{RACE_PLUG}");
    let stored = kv(&mem)[&key].clone();
    let switch = |b: &[u8]| serde_json::from_slice::<J>(b).unwrap()["e"]["1:switch"]["v"].clone();
    assert_eq!(switch(&stored), J::Bool(false), "{}", String::from_utf8_lossy(&stored));
    let uid = format!("{RACE_PLUG}:1:switch");
    let state = |id| ok(&call(&app, sid, &rx, id, json!({"type": "entities/states", "uids": [uid]})))[0]["state"].clone();

    // The plug reports On/Off: on.
    let on = [0x18, 0x01, 0x0A, 0x00, 0x00, 0x10, 0x01];
    r.sim.inject_event(ids::EVT_APS_DATA_INDICATION, indication(0x5A01, 0x5a52_5300_0000_0001, 1, 1, 0x0104, 0x0006, &on));
    let t = std::time::Instant::now();
    while state(1) != J::Bool(true) {
        assert!(t.elapsed() < T, "{}", state(1));
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // Written as before a reboot: nothing yet.
    call(&app, sid, &rx, 2, json!({"type": "hub/reboot"}));
    assert_eq!(kv(&mem)[&key], stored);

    // Adopted: loaded (the live value stays), written with the next flush.
    ok(&call(&app, sid, &rx, 3, json!({"type": "network/registry/adopt", "confirm": true})));
    assert!(app.wait(T, |s| s.last_values.0 > 0), "{:?}", app.status().last_values);
    call(&app, sid, &rx, 4, json!({"type": "hub/reboot"}));
    assert_eq!(switch(&kv(&mem)[&key]), J::Bool(true));
    assert!(app.wait(T, |s| s.last_values.1 > 0), "{:?}", app.status().last_values);
    assert_eq!(app.status().last_values.3, 0, "no write failed: none was attempted while foreign");
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

/// State triggers compare with the devices' restored last values, not
/// with an unknown one: rules readable before their devices (the order of
/// [`records_readable_after_the_devices_wait_for_them`]) wait for them and
/// their last values, and the restored devices' first states (never
/// reported, unknown) do not replace the values the rules read; also when
/// the records load on adopt. The plug was off (its stored last values): a
/// first report of off fires no `equals false` (a change from unknown
/// would), one of on fires `changed` (from unknown it would not). A
/// reported unknown (an invalid value) is the previous value as before:
/// off after it fires `equals false` again, and no `changed`. `reported`
/// off fires at every report of off (twice in a row too), never at the
/// restored value or a read response (a refresh).
#[test]
fn state_triggers_compare_with_the_restored_values() {
    use hub_ncp::sim::indication;
    use zigbee_zrhp::msg::ids;

    let uid = format!("{RACE_PLUG}:1:switch");
    let state_rule = |id: &str, op: &str| {
        let mut t = json!({"type": "state", "uid": uid, "op": op});
        if op != "changed" {
            t["value"] = false.into();
        }
        let mut r = rule(id);
        r["triggers"] = json!([t]);
        (format!("auto/{id}"), r)
    };
    let (off, chg, rep) = (state_rule("off", "equals"), state_rule("chg", "changed"), state_rule("rep", "reported"));
    for (adopt, first) in [(false, false), (false, true), (true, false), (true, true)] {
        let extra = [(off.0.as_str(), off.1.clone()), (chg.0.as_str(), chg.1.clone()), (rep.0.as_str(), rep.1.clone())];
        // Readable right after the devices were read, or foreign until adopted.
        let (_mem, r, racy, app) = racy_start_with("dev/", if adopt { 0 } else { 2 }, true, &extra);
        let (sid, rx) = session(&app);
        if adopt {
            assert!(racy.foreign());
            ok(&call(&app, sid, &rx, 3, json!({"type": "network/registry/adopt", "confirm": true})));
        }
        assert!(!racy.foreign());
        assert!(app.wait(T, |s| s.last_values.0 > 0), "{:?}", app.status().last_values);
        ok(&call(&app, sid, &rx, 1, json!({"type": "subscribe", "topics": ["entities"]})));
        // On/Off from the plug (0xFF: invalid), seen by the rules before
        // their subscribers.
        let frame = |zcl: &[u8]| {
            r.sim.inject_event(ids::EVT_APS_DATA_INDICATION, indication(0x5A01, 0x5a52_5300_0000_0001, 1, 1, 0x0104, 0x0006, zcl));
            loop {
                let f: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
                if f["event"]["topic"] == "entity_state" && f["event"]["uid"] == uid.as_str() {
                    assert!(!f["event"]["last_reported"].is_null(), "{f}");
                    assert!(f["event"].get("pushed").is_none(), "internal: {f}");
                    return f["event"]["state"].clone();
                }
            }
        };
        let report = |v: u8, tsn: u8| frame(&[0x18, tsn, 0x0A, 0x00, 0x00, 0x10, v]);
        let runs = |id: u64| {
            let list = ok(&call(&app, sid, &rx, id, json!({"type": "automations/list"}))).clone();
            let n = |r: &str| list["rules"].as_array().unwrap().iter().find(|x| x["id"] == r).map(|x| x["runs"].clone());
            (n("off"), n("chg"), n("rep"))
        };
        let want = |off: u64, chg: u64, rep: u64| (Some(J::from(off)), Some(J::from(chg)), Some(J::from(rep)));
        // `reported` off: at every report of off, not at the restored value.
        let rep = u64::from(!first);
        assert_eq!(report(u8::from(first), 1), J::Bool(first));
        assert_eq!(runs(2), want(0, u64::from(first), rep), "adopt {adopt}, first report {first}");
        assert_eq!(report(0xFF, 2), J::Null);
        assert_eq!(report(0, 3), J::Bool(false));
        assert_eq!(runs(4), want(1, u64::from(first), rep + 1), "adopt {adopt}, first {first}: unknown, then off");
        assert_eq!(report(0, 4), J::Bool(false));
        assert_eq!(runs(5), want(1, u64::from(first), rep + 2), "off again: a report, not a change");
        assert_eq!(frame(&[0x18, 5, 0x01, 0x00, 0x00, 0x00, 0x10, 0]), J::Bool(false));
        assert_eq!(runs(6), want(1, u64::from(first), rep + 2), "a read response is not the device's own report");
        app.shutdown();
        r.ncp.shutdown();
        r.sim.stop();
    }
}
