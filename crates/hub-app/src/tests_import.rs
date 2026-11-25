//! Migration from Zigbee2MQTT or ZHA (`network/import/preview`,
//! `network/import`, `docs/migration.md`) on both backends: the network
//! restored from the source's backup, the devices from its records without
//! an interview, verified when they are heard from, configured again only
//! when the coordinator IEEE changed; refusals change nothing, a second
//! run over the same network changes nothing more.

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
    assert_eq!(r["error"]["code"], "invalid_value", "{r}");
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
