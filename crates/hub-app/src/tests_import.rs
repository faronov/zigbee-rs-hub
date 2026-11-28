//! Migration from Zigbee2MQTT or ZHA (`network/import/preview`,
//! `network/import`, `docs/migration.md`) on both backends: the network
//! restored from the source's backup, the devices from its records without
//! an interview, verified when they are heard from, configured again only
//! when the coordinator IEEE changed; refusals change nothing, a second
//! run over the same network changes nothing more.

use std::collections::BTreeMap;

use hub_devices::hub::ApiError;

use super::*;

/// The imported network (Zigbee2MQTT's form, generated values): one router
/// on the coordinator's child table, with a unique link key.
fn network(coordinator: &str) -> J {
    json!({
        "metadata": {"format": "zigpy/open-coordinator-backup", "version": 1, "source": "zigbee-herdsman@3.2.0",
            "internal": {"date": "2026-11-20T10:00:00.000Z", "znpVersion": 2}},
        "stack_specific": {"zstack": {"tclk_seed": "77777777777777777777777777777777"}},
        "coordinator_ieee": coordinator, "pan_id": "1b2c", "extended_pan_id": "a1a1a1a1a1a1a1a1",
        "nwk_update_id": 0, "security_level": 5, "channel": 15, "channel_mask": [15],
        "network_key": {"key": "8899aabbccddeeff0011223344556677", "sequence_number": 0, "frame_counter": 5000},
        "devices": [{"nwk_address": "5a01", "ieee_address": "5a52530000000001", "is_child": true,
            "link_key": {"key": "0123456789abcdef0123456789abcdef", "tx_counter": 3, "rx_counter": 4}}]
    })
}

fn plug_entry() -> J {
    json!({"ieee": "0x5a52530000000001", "nwk": PLUG_NWK, "name": "Desk plug", "type": "router",
        "manufacturer": "zigbee-rs", "model": "SIM-PLUG", "manufacturer_code": 0x1234, "power_source": 1,
        "endpoints": [{"id": 1, "profile": 260, "device_id": 81, "in": [0, 3, 6], "out": []}]})
}

fn document(coordinator: &str) -> J {
    json!({
        "format": "zigbee-rs-hub/import", "version": 1,
        "source": {"kind": "z2m", "name": "Zigbee2MQTT 2.1.3"},
        "network": network(coordinator),
        "devices": [plug_entry(), {"ieee": "0x00000000017880ff", "nwk": 1, "type": "green_power", "name": "Remote"}],
        "groups": [{"id": 4097, "name": "Desk", "members": [{"ieee": "5a52530000000001", "ep": 1}]}]
    })
}

/// The coordinator IEEE of each simulated NCP (big-endian hex).
fn sim_ieee(kind: Kind) -> &'static str {
    match kind {
        Kind::Zrhp => "020100000000515a",
        Kind::Ezsp => "000b57fffe123456",
    }
}

const PLUG_ID: &str = "zb:5a52530000000001";

fn device(app: &App, sid: u64, rx: &Receiver<String>) -> J {
    call(
        app,
        sid,
        rx,
        70,
        json!({"type": "devices/get", "device": PLUG_ID}),
    )
}

fn zdo(answered: &Mutex<Vec<ApsRequest>>, cluster: u16) -> bool {
    answered
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.profile == 0 && r.cluster == cluster)
}

/// The same coordinator IEEE (the same radio, or an IEEE the new one
/// took): preview, refusals, the import, verification by the first
/// answer, and a second run that changes nothing.
/// The rig of `kind`; the EZSP NCP with room for link keys.
fn rig(kind: Kind) -> Rig {
    match kind {
        Kind::Zrhp => Rig::new(kind, true),
        Kind::Ezsp => Rig::ezsp(
            hub_ezsp::sim::SimConfig {
                key_table_size: 4,
                ..ezsp_sim(true, 0xDD)
            },
            Store::volatile(),
        ),
    }
}

#[test]
fn an_import_restores_the_network_and_the_devices_without_pairing() {
    for kind in BOTH {
        let mut rig = rig(kind);
        let answered = Arc::new(Mutex::new(Vec::new()));
        rig.devices(vec![plug()], answered.clone());
        let (sid, rx) = session(&rig.app);
        wait("owned", || info(&rig.app, sid, &rx)["ownership"] == "owned");
        let before = rig.tag().map(|t| t.ext_pan_id);
        let doc = document(sim_ieee(kind));

        let p = call(
            &rig.app,
            sid,
            &rx,
            1,
            json!({"type": "network/import/preview", "document": doc}),
        );
        let p = ok(&p).clone();
        let n = &p["network"];
        assert_eq!(
            (n["action"].as_str(), n["coordinator"].as_str()),
            (Some("restore"), Some("same")),
            "{kind:?}: {p}"
        );
        assert_eq!(
            (
                n["ext_pan_id"].as_str(),
                n["link_keys"].as_u64(),
                n["nwk_margin"].as_u64()
            ),
            (Some("a1a1a1a1a1a1a1a1"), Some(1), Some(1 << 20))
        );
        assert!(
            n["needs"].as_array().unwrap().contains(&json!("force")),
            "{kind:?}: {n}"
        );
        assert_eq!(p["restore_check"], "ok", "{kind:?}: {p}");
        assert_eq!(
            p["summary"],
            json!({"new": 1, "replace": 0, "keep": 0, "skip": 1}),
            "{kind:?}"
        );
        let rows = p["devices"].as_array().unwrap();
        assert_eq!(
            (rows[0]["status"].as_str(), rows[0]["name"].as_str()),
            (Some("new"), Some("Desk plug"))
        );
        assert!(
            rows[0]["entities"].as_u64().unwrap() >= 1,
            "{kind:?}: {rows:?}"
        );
        assert_eq!(
            (rows[1]["status"].as_str(), rows[1]["reason"].as_str()),
            (Some("skip"), Some("green_power"))
        );
        assert_eq!(p["groups"], 1);
        assert_eq!(
            rig.tag().map(|t| t.ext_pan_id),
            before,
            "{kind:?}: a preview changes nothing"
        );
        assert_eq!(device(&rig.app, sid, &rx)["error"]["code"], "not_found");

        let r = call(
            &rig.app,
            sid,
            &rx,
            2,
            json!({"type": "network/import", "document": doc}),
        );
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .contains("set force"),
            "{kind:?}: {r}"
        );
        let r = call(
            &rig.app,
            sid,
            &rx,
            3,
            json!({"type": "network/import", "document": doc, "force": true, "confirm": true}),
        );
        let res = ok(&r).clone();
        assert_eq!(res["network"]["action"], "restored", "{kind:?}: {res}");
        assert_eq!(
            res["devices"],
            json!({"new": 1, "replace": 0, "keep": 0, "skip": 1}),
            "{kind:?}"
        );
        assert_eq!(
            (
                res["coordinator_changed"].as_bool(),
                res["reconfigure"].as_bool()
            ),
            (Some(false), Some(false)),
            "{kind:?}: {res}"
        );
        assert_eq!(res["groups"], 1);
        assert_eq!(
            res["stored"], true,
            "{kind:?}: the devices went in once the registry was writable"
        );
        assert_eq!(res["previous_network_backed_up"], true);
        assert_eq!(
            res["backup"]["extended_pan_id"].as_str().map(str::len),
            Some(16),
            "the replaced network's backup"
        );
        assert!(
            res["network"]["nwk_fc_start"].as_u64().unwrap() >= 5000 + (1 << 20),
            "{kind:?}: {res}"
        );
        if kind == Kind::Ezsp {
            assert_eq!(
                res["network"]["link_keys_on_ncp"], 1,
                "the unique link key in the NCP's key table"
            );
        }
        wait("the imported network owned", || {
            let i = info(&rig.app, sid, &rx);
            i["ext_pan_id"] == "a1a1a1a1a1a1a1a1" && i["registry"]["state"] == "owned"
        });
        assert_eq!(rig.tag().map(|t| t.ext_pan_id), Some([0xA1; 8]));
        // At once, with its name and entities; verified by its answer to the probe.
        let d = device(&rig.app, sid, &rx);
        assert_eq!(ok(&d)["name"], "Desk plug", "{kind:?}: {d}");
        wait(&format!("{kind:?}: verified"), || {
            ok(&device(&rig.app, sid, &rx))["interview"] == "complete"
        });
        wait(&format!("{kind:?}: its state read"), || {
            state(&rig.app, sid, &rx, SWITCH) == json!(false)
        });
        assert!(
            !zdo(&answered, 0x0002),
            "{kind:?}: no interview (Node_Desc_req)"
        );
        assert!(
            !zdo(&answered, 0x0021),
            "{kind:?}: the same coordinator IEEE: no binds"
        );
        wait("stored", || {
            kv::with(&*rig.kv, |s| s.get(&format!("dev/{PLUG_ID}")).is_some())
        });
        let kept = call(
            &rig.app,
            sid,
            &rx,
            6,
            json!({"type": "network/backup/replaced"}),
        );
        assert_eq!(
            ok(&kept)["backup"]["extended_pan_id"],
            res["backup"]["extended_pan_id"],
            "{kind:?}: kept before the leave"
        );
        let g = call(&rig.app, sid, &rx, 4, json!({"type": "groups/list"}));
        assert_eq!(ok(&g)[0]["name"], "Desk", "{kind:?}: {g}");
        assert_eq!(ok(&g)[0]["members"][0]["device"], PLUG_ID);

        // Again: the network runs already, the device is the hub's now.
        let r = call(
            &rig.app,
            sid,
            &rx,
            5,
            json!({"type": "network/import", "document": doc, "force": true, "confirm": true}),
        );
        let res = ok(&r).clone();
        assert_eq!(res["network"]["action"], "same", "{kind:?}: {res}");
        assert_eq!(
            res["devices"],
            json!({"new": 0, "replace": 0, "keep": 1, "skip": 1})
        );
        assert_eq!(
            (
                res["previous_network_backed_up"].as_bool(),
                res["groups"].as_u64()
            ),
            (Some(false), Some(0))
        );
        rig.finish();
    }
}

/// Another coordinator IEEE (a new radio that keeps its own): the devices
/// are configured again (binds to the new IEEE) once they answer.
#[test]
fn a_new_coordinator_ieee_configures_the_devices_again() {
    for kind in BOTH {
        let mut rig = Rig::new(kind, true);
        let answered = Arc::new(Mutex::new(Vec::new()));
        rig.devices(vec![plug()], answered.clone());
        let (sid, rx) = session(&rig.app);
        wait("owned", || info(&rig.app, sid, &rx)["ownership"] == "owned");
        let doc = document("00124b0001020304");
        let p = ok(&call(
            &rig.app,
            sid,
            &rx,
            1,
            json!({"type": "network/import/preview", "document": doc, "allow_ieee_change": false}),
        ))
        .clone();
        assert_eq!(p["network"]["coordinator"], "differs", "{kind:?}: {p}");
        let req = json!({"type": "network/import", "document": doc, "force": true, "confirm": true, "allow_ieee_change": false});
        let res = ok(&call(&rig.app, sid, &rx, 2, req)).clone();
        assert_eq!(
            (
                res["coordinator_changed"].as_bool(),
                res["reconfigure"].as_bool()
            ),
            (Some(true), Some(true)),
            "{kind:?}: {res}"
        );
        wait(&format!("{kind:?}: bound to the new coordinator"), || {
            zdo(&answered, 0x0021)
        });
        wait(&format!("{kind:?}: verified"), || {
            ok(&device(&rig.app, sid, &rx))["interview"] == "complete"
        });
        assert!(
            !zdo(&answered, 0x0002),
            "{kind:?}: configured, not interviewed"
        );
        rig.finish();
    }
}

/// A large document goes in batches; the session that started it only;
/// refusals name what is missing and change nothing.
#[test]
fn imports_are_staged_in_batches_and_refused_clearly() {
    let rig = Rig::new(Kind::Zrhp, true);
    let (sid, rx) = session(&rig.app);
    let mut head = document(sim_ieee(Kind::Zrhp));
    head["devices"] = json!([]);
    head["network"]["devices"] = json!([]);
    let r = call(
        &rig.app,
        sid,
        &rx,
        1,
        json!({"type": "network/import/preview", "document": head, "more": true}),
    );
    assert_eq!(
        (
            ok(&r)["staged_devices"].as_u64(),
            ok(&r)["network"]["devices"].as_u64()
        ),
        (Some(0), Some(0)),
        "{r}"
    );
    // Another session cannot continue it.
    let (sid2, rx2) = session(&rig.app);
    let r = call(
        &rig.app,
        sid2,
        &rx2,
        2,
        json!({"type": "network/import/preview", "devices": [plug_entry()]}),
    );
    assert_eq!(r["error"]["code"], "busy", "{r}");
    let nd = network(sim_ieee(Kind::Zrhp))["devices"].clone();
    let r = call(
        &rig.app,
        sid,
        &rx,
        3,
        json!({"type": "network/import/preview", "network_devices": nd, "more": true}),
    );
    assert_eq!(ok(&r)["staged_devices"], 0);
    // A device without its short address takes the network part's.
    let mut e = plug_entry();
    e.as_object_mut().unwrap().remove("nwk");
    let r = call(
        &rig.app,
        sid,
        &rx,
        4,
        json!({"type": "network/import/preview", "devices": [e, plug_entry()]}),
    );
    let p = ok(&r);
    assert_eq!(
        p["summary"],
        json!({"new": 1, "replace": 0, "keep": 0, "skip": 1}),
        "the second one is a duplicate: {p}"
    );
    assert_eq!(p["network"]["devices"], 1);
    assert_eq!(p["devices"][1]["reason"], "duplicate");
    assert_eq!(p["devices"][0]["definition"], J::Null, "SIM-PLUG: generic");
    // The staging ended with that request.
    let r = call(
        &rig.app,
        sid,
        &rx,
        5,
        json!({"type": "network/import/preview", "devices": []}),
    );
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("send the document first"),
        "{r}"
    );

    let mut no_net = document(sim_ieee(Kind::Zrhp));
    no_net.as_object_mut().unwrap().remove("network");
    let r = call(
        &rig.app,
        sid,
        &rx,
        6,
        json!({"type": "network/import", "document": no_net}),
    );
    assert_eq!(r["error"]["code"], "network_required", "{r}");
    let r = call(
        &rig.app,
        sid,
        &rx,
        7,
        json!({"type": "network/import", "document": {"format": "zigbee2mqtt", "version": 1}}),
    );
    assert_eq!(r["error"]["code"], "invalid_value", "{r}");
    let mut bad_key = document(sim_ieee(Kind::Zrhp));
    bad_key["network"]["network_key"]["key"] = json!("00000000000000000000000000000000");
    let r = call(
        &rig.app,
        sid,
        &rx,
        8,
        json!({"type": "network/import", "document": bad_key, "force": true}),
    );
    assert_eq!(r["error"]["code"], "invalid_value", "{r}");
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("network key"),
        "{r}"
    );
    let r = call(
        &rig.app,
        sid,
        &rx,
        9,
        json!({"type": "network/import", "document": document(sim_ieee(Kind::Zrhp)), "force": true, "reconfigure": "maybe"}),
    );
    assert_eq!(r["error"]["code"], "invalid_value", "{r}");
    assert_ne!(
        info(&rig.app, sid, &rx)["ext_pan_id"],
        "a1a1a1a1a1a1a1a1",
        "nothing changed"
    );
    rig.finish();
}

/// The preview resolves the definition an interview would have found
/// (the built-in examples: Philips SML001) from the imported fingerprint.
#[test]
fn the_preview_resolves_catalog_definitions() {
    let rig = Rig::new(Kind::Zrhp, true);
    let (sid, rx) = session(&rig.app);
    let mut doc = document(sim_ieee(Kind::Zrhp));
    doc["devices"] = json!([{"ieee": "0017880103334444", "nwk": 0x6003, "type": "end_device", "power_source": "Battery",
        "manufacturer": "Signify Netherlands B.V.", "model": "SML001", "manufacturer_code": 0x100B,
        "endpoints": [{"id": 1, "profile": 0xC05E, "device_id": 0x0850, "in": [0], "out": [6]},
                      {"id": 2, "profile": 260, "device_id": 0x0107, "in": [0, 1, 0x0406, 0x0400, 0x0402], "out": [25]}]}]);
    let p = ok(&call(
        &rig.app,
        sid,
        &rx,
        1,
        json!({"type": "network/import/preview", "document": doc}),
    ))
    .clone();
    let row = &p["devices"][0];
    assert_eq!(
        row["definition"],
        json!({"source": "catalog", "id": "philips/sml001"}),
        "{p}"
    );
    assert!(row["entities"].as_u64().unwrap() >= 3, "{row}");
    assert_eq!(
        (row["model"].as_str(), row["interview"].as_str()),
        (Some("9290012607"), Some("imported")),
        "the definition's model"
    );
    // Records of an interview the source did not finish: interviewed when
    // heard from (Zigbee2MQTT's FAILED interview: endpoints, no model;
    // `interviewed: false`).
    let mut no_model = plug_entry();
    no_model.as_object_mut().unwrap().remove("model");
    let mut unfinished = plug_entry();
    unfinished["ieee"] = json!("5a52530000000002");
    unfinished["interviewed"] = json!(false);
    doc["devices"] = json!([no_model, unfinished]);
    let p = ok(&call(
        &rig.app,
        sid,
        &rx,
        2,
        json!({"type": "network/import/preview", "document": doc}),
    ))
    .clone();
    for row in p["devices"].as_array().unwrap() {
        assert_eq!(row["interview"], "on_first_contact", "{row}");
    }
    rig.finish();
}

/// A hub without an API token never takes network keys.
#[test]
fn an_import_needs_an_api_token() {
    let r = crate::tests::rig(Store::volatile());
    let app = App::start(
        r.ncp.clone(),
        crate::AppConfig {
            api_token: String::new(),
            ..cfg()
        },
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let sid = app.connect(Box::new(tx)).unwrap();
    let _ = rx.recv_timeout(T).unwrap();
    app.text(sid, json!({"type": "auth", "token": ""}).to_string());
    let _ = rx.recv_timeout(T).unwrap();
    for ty in ["network/import/preview", "network/import"] {
        let r = call(
            &app,
            sid,
            &rx,
            1,
            json!({"type": ty, "document": document(sim_ieee(Kind::Zrhp))}),
        );
        assert_eq!(r["error"]["code"], "unauthorized", "{ty}: {r}");
    }
    app.shutdown();
    r.ncp.shutdown();
}

/// Resuming: the network was restored before (a run that ended before its
/// devices, or `network/restore`), and the hub interviews the NCP's devices
/// meanwhile; the import over the running network takes their records
/// instead, and `remove_missing` drops the replaced network's devices.
#[test]
fn an_import_over_the_restored_network_resumes_and_removes_what_it_does_not_list() {
    let mut rig = Rig::new(Kind::Ezsp, true);
    let answered = Arc::new(Mutex::new(Vec::new()));
    let other = SimDevice::new(
        0x5A52_5300_0000_0099,
        0x5A99,
        LogicalType::Router,
        true,
        0x1234,
    )
    .endpoint(1, 0x0104, 0x0051, &[0x0000, 0x0006], &[])
    .basic("zigbee-rs", "SIM-PLUG", 1);
    rig.devices(vec![plug(), other], answered.clone());
    let (sid, rx) = session(&rig.app);
    wait("owned", || info(&rig.app, sid, &rx)["ownership"] == "owned");
    // A device of the network the import replaces.
    rig.link.join(0x5A52_5300_0000_0099, 0x5A99);
    wait("the old network's device", || {
        call(
            &rig.app,
            sid,
            &rx,
            1,
            json!({"type": "devices/get", "device": "zb:5a52530000000099"}),
        )["result"]["interview"]
            == "complete"
    });
    let doc = document(sim_ieee(Kind::Ezsp));
    ok(&call(
        &rig.app,
        sid,
        &rx,
        2,
        json!({"type": "network/restore", "backup": doc["network"], "force": true, "confirm": true}),
    ));
    wait("restored", || {
        info(&rig.app, sid, &rx)["registry"]["state"] == "owned"
    });
    // Its child table makes the plug known to the NCP: the hub interviews it.
    wait("the NCP's device interviewed", || {
        device(&rig.app, sid, &rx)["success"] == true
    });
    let p = ok(&call(
        &rig.app,
        sid,
        &rx,
        3,
        json!({"type": "network/import/preview", "document": doc}),
    ))
    .clone();
    assert_eq!(
        (p["network"]["action"].as_str(), p["restore_check"].as_str()),
        (Some("same"), Some("not_needed")),
        "{p}"
    );
    assert_eq!(p["not_imported"]["count"], 1, "{p}");
    assert_eq!(p["not_imported"]["devices"][0]["id"], "zb:5a52530000000099");
    let res = ok(&call(
        &rig.app,
        sid,
        &rx,
        4,
        json!({"type": "network/import", "document": doc, "remove_missing": true}),
    ))
    .clone();
    assert_eq!(res["network"]["action"], "same", "{res}");
    let d = &res["devices"];
    assert_eq!(
        d["new"].as_u64().unwrap() + d["replace"].as_u64().unwrap() + d["keep"].as_u64().unwrap(),
        1,
        "{res}"
    );
    assert_eq!(res["removed"], 1);
    let gone = call(
        &rig.app,
        sid,
        &rx,
        5,
        json!({"type": "devices/get", "device": "zb:5a52530000000099"}),
    );
    assert_eq!(gone["error"]["code"], "not_found");
    assert_eq!(ok(&device(&rig.app, sid, &rx))["name"], "Desk plug");
    rig.finish();
}

/// Forty devices in batches of ten: the answers carry each batch's rows,
/// the apply takes them all.
#[test]
fn many_devices_go_in_batches() {
    let rig = Rig::new(Kind::Zrhp, true);
    let (sid, rx) = session(&rig.app);
    let entry = |i: u64| {
        let mut e = plug_entry();
        e["ieee"] = json!(format!("{:016x}", 0x5A52_5300_0000_1000 + i));
        e["nwk"] = json!(0x6000 + i);
        e["name"] = json!(format!("Plug {i}"));
        e
    };
    let mut doc = document(sim_ieee(Kind::Zrhp));
    doc["devices"] = json!([]);
    let r = call(
        &rig.app,
        sid,
        &rx,
        1,
        json!({"type": "network/import", "document": doc, "force": true, "more": true}),
    );
    assert_eq!(ok(&r)["network"]["action"], "restore", "{r}");
    for b in 0..4u64 {
        let batch: Vec<J> = (0..10).map(|i| entry(b * 10 + i)).collect();
        let r = call(
            &rig.app,
            sid,
            &rx,
            2 + b,
            json!({"type": "network/import", "devices": batch, "more": b < 3}),
        );
        if b < 3 {
            assert_eq!(ok(&r)["staged_devices"], (b + 1) * 10, "{r}");
            assert_eq!(ok(&r)["devices"].as_array().map(Vec::len), Some(10));
        } else {
            assert_eq!(ok(&r)["devices"]["new"], 40, "{r}");
        }
    }
    let list = call(&rig.app, sid, &rx, 9, json!({"type": "devices/list"}));
    let list = ok(&list).as_array().unwrap().clone();
    assert_eq!(list.len(), 40);
    assert!(
        list.iter()
            .all(|d| d["interview"] == "imported" || d["interview"] == "complete"),
        "{list:?}"
    );
    rig.finish();
}

/// The operator's two confirmations are asked for one at a time: `force`
/// (the NCP runs another network) and `confirm` (the records are another
/// network's); either alone changes nothing.
#[test]
fn force_and_confirm_are_each_required() {
    let rig = Rig::new(Kind::Ezsp, true);
    let (sid, rx) = session(&rig.app);
    wait("owned", || {
        info(&rig.app, sid, &rx)["registry"]["state"] == "owned"
    });
    let doc = document(sim_ieee(Kind::Ezsp));
    let p = ok(&call(
        &rig.app,
        sid,
        &rx,
        1,
        json!({"type": "network/import/preview", "document": doc}),
    ))
    .clone();
    assert_eq!(p["network"]["needs"], json!(["force", "confirm"]), "{p}");
    let r = call(
        &rig.app,
        sid,
        &rx,
        2,
        json!({"type": "network/import", "document": doc, "force": true}),
    );
    assert_eq!(r["error"]["code"], "registry_read_only", "{r}");
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("set confirm"),
        "{r}"
    );
    let r = call(
        &rig.app,
        sid,
        &rx,
        3,
        json!({"type": "network/import", "document": doc, "confirm": true}),
    );
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("set force"),
        "{r}"
    );
    assert_eq!(rig.tag().map(|t| t.ext_pan_id), Some([0xDD; 8]));
    assert_eq!(device(&rig.app, sid, &rx)["error"]["code"], "not_found");
    rig.finish();
}

/// The NCP runs the imported network, but the records are another
/// backend's: the import refuses (its devices could not be stored) until
/// the operator adopts or clears them.
#[test]
fn foreign_records_on_the_imported_network_are_adopted_first() {
    let flash = SharedFlash::new();
    let rig = Rig::ezsp(ezsp_sim(true, 0xDD), flash.store());
    let (sid, rx) = session(&rig.app);
    wait("owned", || {
        info(&rig.app, sid, &rx)["registry"]["state"] == "owned"
    });
    let doc = document(sim_ieee(Kind::Ezsp));
    ok(&call(
        &rig.app,
        sid,
        &rx,
        1,
        json!({"type": "network/restore", "backup": doc["network"], "force": true, "confirm": true}),
    ));
    wait("restored", || {
        let i = info(&rig.app, sid, &rx);
        i["ext_pan_id"] == "a1a1a1a1a1a1a1a1" && i["registry"]["state"] == "owned"
    });
    let ncp = rig.ezsp.as_ref().unwrap().ncp(|n| n.config().clone());
    rig.finish();
    let mut s = flash.store();
    s.kv_put(
        OWNER_KEY,
        OwnerTag::new(BackendKind::Zrhp, None).encode().as_bytes(),
    )
    .unwrap();
    drop(s);

    let rig = Rig::ezsp(ncp, flash.store());
    let (sid, rx) = session(&rig.app);
    wait("foreign", || {
        info(&rig.app, sid, &rx)["registry"]["state"] == "foreign"
    });
    let p = ok(&call(
        &rig.app,
        sid,
        &rx,
        2,
        json!({"type": "network/import/preview", "document": doc}),
    ))
    .clone();
    assert_eq!(p["network"]["action"], "same", "{p}");
    let r = call(
        &rig.app,
        sid,
        &rx,
        3,
        json!({"type": "network/import", "document": doc, "force": true, "confirm": true}),
    );
    assert_eq!(r["error"]["code"], "registry_read_only", "{r}");
    assert!(
        r["error"]["message"].as_str().unwrap().contains("adopt"),
        "{r}"
    );
    assert_eq!(device(&rig.app, sid, &rx)["error"]["code"], "not_found");
    assert_eq!(
        kv::with(&*rig.kv, |s| s.get(OWNER_KEY).and_then(OwnerTag::decode))
            .unwrap()
            .backend,
        "zrhp",
        "the tag kept"
    );
    let a = call(
        &rig.app,
        sid,
        &rx,
        4,
        json!({"type": "network/registry/adopt", "confirm": true}),
    );
    assert_eq!(ok(&a)["registry"]["state"], "owned", "{a}");
    let res = ok(&call(
        &rig.app,
        sid,
        &rx,
        5,
        json!({"type": "network/import", "document": doc}),
    ))
    .clone();
    assert_eq!(
        (res["network"]["action"].as_str(), res["stored"].as_bool()),
        (Some("same"), Some(true)),
        "{res}"
    );
    wait("stored", || {
        kv::with(&*rig.kv, |s| s.get(&format!("dev/{PLUG_ID}")).is_some())
    });
    rig.finish();
}

/// Records closed at the start (another backend's): an import of another
/// network with `force` and `confirm` takes them; its devices go in after
/// the records the restore opened were loaded, and are stored at once
/// with the imported data.
#[test]
fn an_import_over_records_closed_at_the_start_stores_its_devices() {
    let flash = SharedFlash::new();
    let mut s = flash.store();
    s.kv_put(
        OWNER_KEY,
        OwnerTag::new(BackendKind::Zrhp, None).encode().as_bytes(),
    )
    .unwrap();
    drop(s);
    let rig = Rig::ezsp(ezsp_sim(true, 0xDD), flash.store());
    let (sid, rx) = session(&rig.app);
    wait("foreign", || {
        info(&rig.app, sid, &rx)["registry"]["state"] == "foreign"
    });
    let doc = document(sim_ieee(Kind::Ezsp));
    let res = ok(&call(
        &rig.app,
        sid,
        &rx,
        1,
        json!({"type": "network/import", "document": doc, "force": true, "confirm": true}),
    ))
    .clone();
    assert_eq!(
        (res["network"]["action"].as_str(), res["stored"].as_bool()),
        (Some("restored"), Some(true)),
        "{res}"
    );
    let rec = kv::with(&*rig.kv, |s| {
        s.get(&format!("dev/{PLUG_ID}")).map(<[u8]>::to_vec)
    })
    .expect("stored with the answer");
    assert!(
        String::from_utf8_lossy(&rec).contains("Desk plug"),
        "the imported record"
    );
    assert_eq!(ok(&device(&rig.app, sid, &rx))["name"], "Desk plug");
    rig.finish();
}

/// Holds the app thread (`test/stall`): what it should take waits.
struct Stall;

impl crate::RequestHook for Stall {
    fn prefix(&self) -> &'static str {
        "test/"
    }

    fn request(&mut self, _: &str, _: &J) -> Result<J, ApiError> {
        thread::sleep(Duration::from_secs(5));
        Ok(json!(true))
    }
}

/// ZRHP: a second apply while one runs is refused without ending the
/// first one's hold; an app thread that does not take the devices in time
/// ends the apply with `timeout` after the restore, and the import run
/// again over the restored network finishes it.
#[test]
fn an_apply_that_timed_out_after_the_restore_resumes() {
    let mut rig = rig(Kind::Zrhp);
    let answered = Arc::new(Mutex::new(Vec::new()));
    rig.devices(vec![plug()], answered);
    let (sid, rx) = session(&rig.app);
    wait("owned", || info(&rig.app, sid, &rx)["ownership"] == "owned");
    rig.app.add_hook(Box::new(Stall));
    let doc = document(sim_ieee(Kind::Zrhp));
    let apply = |id: u64| {
        json!({"type": "network/import", "id": id, "document": doc, "force": true, "confirm": true})
            .to_string()
    };
    rig.app.text(sid, apply(1));
    rig.app.text(sid, apply(2));
    rig.app
        .text(sid, json!({"type": "test/stall", "id": 3}).to_string());
    let mut answers = BTreeMap::new();
    while answers.len() < 3 {
        let f: J = serde_json::from_str(&rx.recv_timeout(T * 2).unwrap()).unwrap();
        if f["type"] == "result" {
            answers.insert(f["id"].as_u64().unwrap(), f);
        }
    }
    assert_eq!(answers[&2]["error"]["code"], "busy", "{}", answers[&2]);
    assert!(
        answers[&2]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("an import is being applied"),
        "{}",
        answers[&2]
    );
    assert_eq!(answers[&1]["error"]["code"], "timeout", "{}", answers[&1]);
    assert_eq!(
        answers[&1]["backup"]["extended_pan_id"]
            .as_str()
            .map(str::len),
        Some(16),
        "the replaced network's backup comes with the error"
    );
    wait("the restored network owned", || {
        let i = info(&rig.app, sid, &rx);
        i["ext_pan_id"] == "a1a1a1a1a1a1a1a1" && i["registry"]["state"] == "owned"
    });
    let res = ok(&call(
        &rig.app,
        sid,
        &rx,
        4,
        json!({"type": "network/import", "document": doc}),
    ))
    .clone();
    assert_eq!(
        (res["network"]["action"].as_str(), res["stored"].as_bool()),
        (Some("same"), Some(true)),
        "{res}"
    );
    let d = &res["devices"];
    assert_eq!(
        d["new"].as_u64().unwrap() + d["replace"].as_u64().unwrap() + d["keep"].as_u64().unwrap(),
        1,
        "{res}"
    );
    assert_eq!(ok(&device(&rig.app, sid, &rx))["name"], "Desk plug");
    rig.finish();
}

/// A staging ends when its session closes, or after its TTL (3 s in
/// tests) without a request; meanwhile another session is told it runs.
#[test]
fn a_staging_ends_with_its_session_or_its_ttl() {
    let rig = Rig::new(Kind::Zrhp, true);
    let (a, rxa) = session(&rig.app);
    let (b, rxb) = session(&rig.app);
    let mut head = document(sim_ieee(Kind::Zrhp));
    head["devices"] = json!([]);
    let stage = |sid, rx: &Receiver<String>| {
        ok(&call(
            &rig.app,
            sid,
            rx,
            1,
            json!({"type": "network/import/preview", "document": head, "more": true}),
        ));
    };
    let probe = |sid, rx: &Receiver<String>| {
        call(
            &rig.app,
            sid,
            rx,
            2,
            json!({"type": "network/import/preview", "devices": [], "more": true}),
        )["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    stage(a, &rxa);
    let staged = Instant::now();
    assert_eq!(probe(b, &rxb), "busy");
    let r = call(
        &rig.app,
        b,
        &rxb,
        3,
        json!({"type": "network/import/preview", "document": head, "more": true}),
    );
    assert_eq!(
        r["error"]["code"], "busy",
        "not over another session's: {r}"
    );
    rig.app.disconnect(a);
    // Well before the TTL.
    while probe(b, &rxb) != "invalid_value" {
        assert!(
            staged.elapsed() < Duration::from_millis(1500),
            "dropped with its session"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let (c, rxc) = session(&rig.app);
    stage(c, &rxc);
    assert_eq!(probe(b, &rxb), "busy");
    wait("expired", || probe(b, &rxb) == "invalid_value");
    rig.finish();
}

/// The groups count in the staged bytes; at most 256 groups.
#[test]
fn imports_beyond_the_limits_are_refused() {
    let rig = Rig::new(Kind::Zrhp, true);
    let (sid, rx) = session(&rig.app);
    let mut head = document(sim_ieee(Kind::Zrhp));
    head["devices"] = json!([]);
    head["groups"] = json!([]);
    let group = |id: u64, members: u64| {
        json!({"id": id, "name": "Room", "members": (0..members)
            .map(|m| json!({"ieee": format!("{:016x}", 0x5A52_5300_0001_0000 + m), "ep": 1}))
            .collect::<Vec<_>>()})
    };
    let mut many = head.clone();
    many["groups"] = (1..=257).map(|i| group(i, 0)).collect();
    let r = call(
        &rig.app,
        sid,
        &rx,
        1,
        json!({"type": "network/import/preview", "document": many}),
    );
    assert_eq!(r["error"]["code"], "too_large", "{r}");
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("256 groups"),
        "{r}"
    );
    ok(&call(
        &rig.app,
        sid,
        &rx,
        2,
        json!({"type": "network/import", "document": head, "force": true, "more": true}),
    ));
    let mut refused = None;
    for b in 0..5u64 {
        let groups: Vec<J> = (0..50).map(|i| group(1 + b * 50 + i, 40)).collect();
        let r = call(
            &rig.app,
            sid,
            &rx,
            3 + b,
            json!({"type": "network/import", "groups": groups, "more": true}),
        );
        if r["success"] == false {
            refused = Some((b, r));
            break;
        }
    }
    let (b, r) = refused.expect("refused");
    assert_eq!(r["error"]["code"], "too_large", "{r}");
    assert!(
        r["error"]["message"].as_str().unwrap().contains("KiB"),
        "{r}"
    );
    assert!(b >= 2, "about 140 groups of 40 members fit: {b}");
    assert_ne!(
        info(&rig.app, sid, &rx)["ext_pan_id"],
        "a1a1a1a1a1a1a1a1",
        "nothing changed"
    );
    rig.finish();
}

/// The NCP's child entry for `ieee`: its capability (EZSP: 0x8C rx-on,
/// 0x80 sleepy), or `None` without one.
fn ncp_child(rig: &Rig, ieee: u64) -> Option<u8> {
    if let Some(z) = &rig.zsim {
        return z.with(|s| {
            s.table_devices
                .iter()
                .find(|d| d.0 == ieee.to_le_bytes())
                .and_then(|d| d.2)
        });
    }
    rig.ezsp.as_ref().unwrap().ncp(|n| {
        n.config()
            .children
            .iter()
            .find(|c| c.eui64 == Eui64::from_u64(ieee))
            .map(|c| {
                if c.node_type.0 == hub_ezsp::ezsp::NodeType::END_DEVICE.0 {
                    0x8C
                } else {
                    0x80
                }
            })
    })
}

/// A router that joined through the coordinator, written with
/// `is_child: true` (zigbee-rs-hub 0.4.0 and earlier, Z-Stack files), is
/// restored without a child entry when it is known to be a router: by the
/// import document's device type, and on `network/restore` by the
/// registry. Restored as a sleepy child, its frames would wait for polls it
/// never sends. End devices stay children.
#[test]
fn a_router_marked_as_a_child_is_not_restored_as_one() {
    const SENSOR: u64 = 0x5A52_5300_0000_0002;
    for kind in BOTH {
        let mut rig = rig(kind);
        rig.devices(vec![plug()], Arc::new(Mutex::new(Vec::new())));
        let (sid, rx) = session(&rig.app);
        wait("owned", || info(&rig.app, sid, &rx)["ownership"] == "owned");
        let mut doc = document(sim_ieee(kind));
        doc["network"]["devices"]
            .as_array_mut()
            .unwrap()
            .push(json!({"nwk_address": "5a02", "ieee_address": "5a52530000000002", "is_child": true}));
        let net = doc["network"].clone();

        // The import: the plug's `type: "router"` wins over `is_child`.
        let r = call(
            &rig.app,
            sid,
            &rx,
            1,
            json!({"type": "network/import", "document": doc, "force": true, "confirm": true}),
        );
        assert_eq!(ok(&r)["network"]["children"], 1, "{kind:?}: {r}");
        assert_eq!(ncp_child(&rig, PLUG), None, "{kind:?}: the router");
        assert_eq!(ncp_child(&rig, SENSOR), Some(0x80), "{kind:?}: the end device");
        wait("the imported network owned", || {
            info(&rig.app, sid, &rx)["registry"]["state"] == "owned"
        });

        // A 0.4.0-style backup of the same network, restored: the registry
        // knows the plug as a router.
        let r = call(
            &rig.app,
            sid,
            &rx,
            2,
            json!({"type": "network/restore", "backup": net, "force": true}),
        );
        assert_eq!(ok(&r)["children"], 1, "{kind:?}: {r}");
        assert_eq!(ncp_child(&rig, PLUG), None, "{kind:?}: the router");
        assert_eq!(ncp_child(&rig, SENSOR), Some(0x80), "{kind:?}: the end device");
        rig.finish();
    }
}

/// An IKEA remote on group-only firmware that Zigbee2MQTT bound to its bind
/// group 901, imported into the running network (`same`: no new NCP
/// session) with the same coordinator IEEE (its configure taken as done):
/// the hub endpoint joins 0x0000 and 901 at once, so the remote's first
/// groupcast is heard.
#[test]
fn an_imported_old_ikea_remote_is_heard_through_its_bind_group() {
    const REMOTE: u64 = 0x5A52_5300_0000_0012;
    let remote = SimDevice::new(REMOTE, 0x5A12, LogicalType::EndDevice, false, 0x117C)
        .endpoint(1, 0x0104, 0x0820, &[0x0000, 0x0001, 0x0003, 0x1000], &[0x0003, 0x0004, 0x0005, 0x0006, 0x0008, 0x0019])
        .basic("IKEA of Sweden", "TRADFRI remote control", 3)
        .attr(1, 0x0000, 0x4000, TypedValue::new(ZclType::CHAR_STR, ZclValue::String("2.3.014".into())));
    let entry = json!({"ieee": "0x5a52530000000012", "nwk": 0x5A12, "name": "Remote", "type": "end_device",
        "manufacturer": "IKEA of Sweden", "model": "TRADFRI remote control", "manufacturer_code": 0x117C,
        "power_source": 3, "sw_build_id": "2.3.014",
        "endpoints": [{"id": 1, "profile": 260, "device_id": 0x0820, "in": [0, 1, 3, 4096], "out": [3, 4, 5, 6, 8, 25]}]});
    for kind in BOTH {
        let mut rig = rig(kind);
        rig.devices(vec![plug(), remote.clone()], Arc::new(Mutex::new(Vec::new())));
        let (sid, rx) = session(&rig.app);
        let (esid, erx) = session(&rig.app);
        ok(&call(&rig.app, esid, &erx, 1, json!({"type": "subscribe", "topics": ["events"]})));
        wait("owned", || info(&rig.app, sid, &rx)["ownership"] == "owned");
        let mut doc = document(sim_ieee(kind));
        let r = call(&rig.app, sid, &rx, 2, json!({"type": "network/import", "document": doc, "force": true, "confirm": true}));
        assert_eq!(ok(&r)["network"]["action"], "restored", "{kind:?}: {r}");
        wait("the imported network owned", || info(&rig.app, sid, &rx)["registry"]["state"] == "owned");
        thread::sleep(Duration::from_millis(300));
        assert!(!rig.link.groups().contains(&901), "{kind:?}");

        doc["devices"].as_array_mut().unwrap().push(entry.clone());
        let r = call(&rig.app, sid, &rx, 3, json!({"type": "network/import", "document": doc, "force": true, "confirm": true}));
        let res = ok(&r).clone();
        assert_eq!((res["network"]["action"].as_str(), res["reconfigure"].as_bool()), (Some("same"), Some(false)), "{kind:?}: {res}");
        wait(&format!("{kind:?}: the hub endpoint in 0x0000 and 901"), || {
            let g = rig.link.groups();
            g.contains(&0) && g.contains(&901)
        });
        rig.link.indicate(&press(&remote, Some(901)));
        assert_eq!(next_event(&erx, "zb:5a52530000000012:1:action"), "toggle", "{kind:?}");
        rig.finish();
    }
}
