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
