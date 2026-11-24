//! The ownership tag of the hub's records end to end (design §7 row I):
//! the first start of a tagging build on PR A's untagged journal, a start
//! on records that are not this hub's, and the operator's adopt and clear.

use std::collections::BTreeMap;
use std::sync::mpsc::Receiver;

use hub_coord::{BackendKind, OwnerTag, RegistryState};
use hub_ncp::coord::owner;
use hub_store::kv::scope;
use hub_store::{Flash, Journal, OWNER_KEY, Scope, Store};
use serde_json::{Value as J, json};

use crate::App;
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
