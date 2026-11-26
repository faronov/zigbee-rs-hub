//! The catalog service end to end on the simulated NCP: a signed release in
//! a directory (manifest, bank, shards), `hubdata` in RAM; update, on-demand
//! fetch, restart, chunked upload with anti-rollback, a corrupted bank.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use hub_devices::device::DeviceKind;
use hub_devices::fingerprint::{EndpointFp, Fingerprint};
use hub_devices::persist::DeviceSnapshot;
use hub_ncp::store::{MemFlash, Store};
use serde_json::{Value as J, json};
use zdefc::bank::{BankFile, BankParams, Item, SigningKey};

use super::service::UPLOAD_CHUNK_MAX;

use super::data::{DataFlash, MemData, SharedFlash};
use crate::tests::{T, call, cfg, rig, session};
use crate::{App, CatalogSetup};

const SML: &str = "zb:0017880103334444";

fn key() -> SigningKey {
    SigningKey::from_seed([42; 32])
}

fn items() -> Vec<Item> {
    let dir = format!("{}/../../definitions/examples", env!("CARGO_MANIFEST_DIR"));
    let mut paths: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    paths.sort();
    let mut out = Vec::new();
    for p in paths {
        let c =
            zdefc::compile_str(&std::fs::read_to_string(&p).unwrap(), zdefc::Format::Yaml, &zdefc::Options::default())
                .unwrap();
        if !c.def.is_fragment() {
            out.push(Item::new(&p.display().to_string(), c.cbor).unwrap());
        }
    }
    out
}

/// A signed release: the bank leaves out the Philips motion sensor (only
/// in the shards, for the on-demand fetch).
fn release(dir: &Path, generation: u32) -> Vec<u8> {
    std::fs::create_dir_all(dir).unwrap();
    let files = release_files(generation);
    for (name, bytes) in &files {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
    files["bank-4m.zdb"].clone()
}

/// The files of [`release`] by name.
fn release_files(generation: u32) -> BTreeMap<String, Vec<u8>> {
    let all = items();
    let refs: Vec<&Item> = all.iter().collect();
    let bank_items: Vec<&Item> = all.iter().filter(|i| !i.name.contains("sml001")).collect();
    let params = BankParams {
        generation,
        catalog_date: 20261121,
        catalog_serial: generation as u16,
        max_size: 256 * 1024,
        ..Default::default()
    };
    let bank = zdefc::bank::build_bank(&bank_items, &params).unwrap();
    let bank = zdefc::bank::sign_bank(&bank.bytes, &key()).unwrap();
    let (shards, digest) = zdefc::bank::build_shards(&refs).unwrap();
    let mut m = zdefc::bank::manifest(
        &params,
        &[BankFile {
            name: "bank-4m.zdb".into(),
            bytes: &bank,
            definitions: bank_items.len() as u32,
            generation,
        }],
        &shards,
        vec![],
        Some(digest),
        vec![],
    );
    let tools = web_bundle();
    m.tools.push(zdef::sign::ManifestAsset {
        name: "zdefc-web.zwb".into(),
        sha256: sha256(&tools),
        size: tools.len() as u64,
        definitions: 0,
        generation: 0,
    });
    let mut files = BTreeMap::new();
    files.insert("zdefc-web.zwb".to_string(), tools);
    files.insert(
        "manifest.cose".to_string(),
        key().cose_sign1(&m.encode(), false),
    );
    files.insert("bank-4m.zdb".to_string(), bank);
    for (n, s) in shards {
        files.insert(format!("shard-{n:02x}.cbor"), s);
    }
    files
}

fn sml001_snapshot() -> DeviceSnapshot {
    let fp = Fingerprint {
        manufacturer_name: Some("Signify Netherlands B.V.".into()),
        model_id: Some("SML001".into()),
        mfr_code: Some(0x100B),
        endpoints: vec![
            EndpointFp {
                ep: 1,
                profile: 0xC05E,
                device_id: 0x0850,
                in_clusters: vec![0x0000],
                out_clusters: vec![0x0006],
            },
            EndpointFp {
                ep: 2,
                profile: 0x0104,
                device_id: 0x0107,
                in_clusters: vec![0x0000, 0x0001, 0x0406, 0x0400, 0x0402],
                out_clusters: vec![0x0019],
            },
        ],
        ..Default::default()
    };
    DeviceSnapshot {
        v: hub_devices::persist::SNAPSHOT_VERSION,
        id: SML.into(),
        kind: DeviceKind::Zigbee { ieee: 0x0017880103334444, nwk: 0x6003 },
        fp,
        node_desc: None,
        active_eps: vec![1, 2],
        interviewed: true,
        interview_partial: false,
        attrs: vec![],
        user_name: None,
        disabled: false,
        pinned: None,
        options: BTreeMap::new(),
        configure_hash: None,
        configure_done: BTreeMap::new(),
        unreportable: vec![],
        warnings: vec![],
        gp: None,
        schema: None,
        entities: vec![],
        reporting: vec![],
        imported: 0,
    }
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hub-catalog-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn web_bundle() -> Vec<u8> {
    let files = vec![
        ("zdefc_wasm.js".to_string(), "text/javascript".to_string(), zdefc::web::gzip(b"export default 1;")),
        ("zdefc_wasm_bg.wasm".to_string(), "application/wasm".to_string(), zdefc::web::gzip(&[0, 0x61, 0x73, 0x6d])),
    ];
    zdef::webtools::build(&files).unwrap()
}

fn start(r: &crate::tests::Rig, flash: &SharedFlash) -> App {
    let setup = CatalogSetup { flash: flash.clone(), extra_keys: vec![key().public()] };
    let app = App::start_with(r.ncp.clone(), crate::AppConfig { builtin_catalog: false, ..cfg() }, Some(setup));
    assert!(app.wait(T, |s| s.endpoint_registered));
    app
}

type Rx = std::sync::mpsc::Receiver<String>;

/// Uploads `bytes` to `target` in 8 KiB chunks from `from`, then commits.
fn upload(app: &App, sid: u64, rx: &Rx, id: &mut u64, target: &str, bytes: &[u8], from: usize, commit: J) -> J {
    for (i, c) in bytes[from..].chunks(UPLOAD_CHUNK_MAX).enumerate() {
        let off = from + i * UPLOAD_CHUNK_MAX;
        let mut req = json!({"type": "catalog/upload_chunk", "target": target, "offset": off,
            "data": base64::engine::general_purpose::STANDARD.encode(c)});
        if off == 0 {
            req["size"] = bytes.len().into();
            req["sha256"] = hex(&sha256(bytes)).into();
        }
        *id += 1;
        let w = call(app, sid, rx, *id, req);
        assert_eq!(w["success"], true, "{target} chunk at {off}: {w}");
    }
    let mut c = commit;
    c["type"] = "catalog/commit".into();
    c["target"] = target.into();
    *id += 1;
    call(app, sid, rx, *id, c)
}

fn layers(app: &App, sid: u64, rx: &Rx, id: u64) -> J {
    call(app, sid, rx, id, json!({"type": "devices/get", "device": SML}))["result"]["resolution"]["layers"].clone()
}

#[test]
fn uploads_install_fetch_restart_resume_and_corruption() {
    let dir = tmp("rel");
    release(&dir, 3);
    let file = |n: &str| std::fs::read(dir.join(n)).unwrap();
    let store = Store::open(Box::new(MemFlash::new(32, 4096)) as Box<dyn hub_ncp::store::Flash>).unwrap();
    let r = rig(store);
    r.ncp.with_store(|s| s.kv_put(&format!("dev/{SML}"), &sml001_snapshot().to_bytes())).unwrap();
    let flash: SharedFlash = Arc::new(Mutex::new(Box::new(MemData::new(1 << 20)) as Box<dyn DataFlash>));
    let mut id = 0;

    let app = start(&r, &flash);
    let (sid, rx) = session(&app);
    let st = call(&app, sid, &rx, 1, json!({"type": "catalog/status"}));
    assert_eq!((st["result"]["installed"].clone(), st["result"]["wants"].clone()), (J::Null, J::Null), "{st}");
    assert!(st["result"]["config"]["base_url"].as_str().unwrap().contains("github.io"));
    // A web bundle needs a manifest that lists it.
    let no =
        call(&app, sid, &rx, 2, json!({"type": "catalog/upload_chunk", "target": "web", "offset": 0, "data": "AA=="}));
    assert_eq!(no["success"], false, "{no}");
    // Chunks above 4 KiB are refused (a hub without PSRAM holds a request
    // three times while handling it).
    let big = base64::engine::general_purpose::STANDARD.encode(vec![0u8; UPLOAD_CHUNK_MAX + 1]);
    let no = call(
        &app,
        sid,
        &rx,
        3,
        json!({"type": "catalog/upload_chunk", "target": "manifest", "offset": 0, "data": big}),
    );
    assert_eq!(no["error"]["code"], "invalid_value", "{no}");

    // 1. The manifest: what the hub wants.
    let m = upload(&app, sid, &rx, &mut id, "manifest", &file("manifest.cose"), 0, json!({}));
    assert_eq!(m["result"]["wants"]["bank"]["name"], "bank-4m.zdb", "{m}");
    assert_eq!(m["result"]["wants"]["web"]["name"], "zdefc-web.zwb", "{m}");
    // 2. The bank, interrupted after one chunk and resumed from the status.
    let bank = file("bank-4m.zdb");
    upload_part(&app, sid, &rx, &mut id, "bank", &bank, 1024);
    let st = call(&app, sid, &rx, 900, json!({"type": "catalog/status"}));
    let at = st["result"]["upload"]["written"].as_u64().unwrap() as usize;
    assert_eq!((st["result"]["upload"]["target"].as_str(), at), (Some("bank"), 1024), "{st}");
    // Another target meanwhile (a second client, the page's own
    // definition_needed fetch): refused, the bank upload goes on.
    let other = call(
        &app,
        sid,
        &rx,
        899,
        json!({"type": "catalog/upload_chunk", "target": "shard", "offset": 0, "data": "AA=="}),
    );
    assert_eq!(other["error"]["code"], "busy", "{other}");
    let b = upload(&app, sid, &rx, &mut id, "bank", &bank, at, json!({}));
    assert_eq!(b["result"]["generation"], 3, "{b}");
    // 3. The web tools: served at /zdefc/.
    let w = upload(&app, sid, &rx, &mut id, "web", &file("zdefc-web.zwb"), 0, json!({}));
    assert_eq!(w["success"], true, "{w}");
    assert!(app.web().serve("zdefc_wasm_bg.wasm", |f| f.is_some_and(|f| f.content_type == "application/wasm")));
    let st = call(&app, sid, &rx, 901, json!({"type": "catalog/status"}));
    assert_eq!(st["result"]["wants"], json!({"bank": null, "web": null}), "{st}");
    // The same manifest again (a client's periodic check): nothing in
    // hubdata changes (the staging area gets the same bytes).
    let before = flash.lock().unwrap().map(0, 1 << 20).to_vec();
    let again = upload(&app, sid, &rx, &mut id, "manifest", &file("manifest.cose"), 0, json!({}));
    assert_eq!(again["success"], true, "{again}");
    assert!(flash.lock().unwrap().map(0, 1 << 20) == &before[..], "manifest or record rewritten");
    // 4. The motion sensor is not in the bank: the hub names its shards,
    // the client uploads them.
    assert!(layers(&app, sid, &rx, 902).as_array().unwrap().iter().all(|x| x["source"] == "generic"));
    let miss = st["result"]["missing"].as_array().unwrap().clone();
    assert_eq!(miss.len(), 1, "{st}");
    let f = call(&app, sid, &rx, 903, json!({"type": "definitions/fetch", "device": SML}));
    assert_eq!(f["result"]["shards"], miss[0]["shards"], "{f}");
    // A shard that is not the published one is refused.
    let n0 = miss[0]["shards"][0].as_u64().unwrap();
    let wrong = upload(&app, sid, &rx, &mut id, "shard", b"\x80", 0, json!({"shard": n0}));
    assert_eq!(wrong["error"]["code"], "verification_failed", "{wrong}");
    let mut got = Vec::new();
    for n in miss[0]["shards"].as_array().unwrap() {
        let n = n.as_u64().unwrap();
        let s = upload(&app, sid, &rx, &mut id, "shard", &file(&format!("shard-{n:02x}.cbor")), 0, json!({"shard": n}));
        assert_eq!(s["success"], true, "{s}");
        got.extend(s["result"]["definitions"].as_array().unwrap().clone());
    }
    assert_eq!(got, [json!("philips/sml001")]);
    let l = layers(&app, sid, &rx, 904);
    assert_eq!(l.as_array().unwrap().last().unwrap()["source"], "fetched", "{l}");
    app.shutdown();

    // A restart keeps the bank, the fetched definition and the web tools.
    let app = start(&r, &flash);
    let (sid, rx) = session(&app);
    let st = call(&app, sid, &rx, 1, json!({"type": "catalog/status"}));
    assert_eq!(st["result"]["installed"]["generation"], 3, "{st}");
    assert_eq!(st["result"]["local"]["definitions"], 1, "{st}");
    assert_eq!(st["result"]["web_tools"]["installed"], true, "{st}");
    assert_eq!(layers(&app, sid, &rx, 2).as_array().unwrap().last().unwrap()["id"], "philips/sml001");

    // A newer bank (not in the installed manifest: signature only) goes
    // into the other slot; an older one is refused (anti-rollback).
    let newer = release(&tmp("rel5"), 5);
    let b = upload(&app, sid, &rx, &mut id, "bank", &newer, 0, json!({}));
    assert_eq!(b["result"]["generation"], 5, "{b}");
    let no = upload(&app, sid, &rx, &mut id, "bank", &bank, 0, json!({}));
    assert!(no["error"]["message"].as_str().unwrap().contains("anti-rollback"), "{no}");
    let st = call(&app, sid, &rx, 3, json!({"type": "catalog/status"}));
    assert_eq!(
        (st["result"]["active_slot"].clone(), st["result"]["installed"]["generation"].clone()),
        (json!("b"), json!(5))
    );
    app.shutdown();

    // A corrupted active bank is not used at the next start.
    let map = hub_core::layout::HubdataMap::for_size(1 << 20).unwrap();
    flash.lock().unwrap().write(map.bank_b.offset + 2000, &[0x00; 16]).unwrap();
    let app = start(&r, &flash);
    let (sid, rx) = session(&app);
    let st = call(&app, sid, &rx, 1, json!({"type": "catalog/status"}));
    assert_eq!(st["result"]["installed"], J::Null, "{st}");
    assert!(st["result"]["warnings"][0].as_str().unwrap().contains("slot 1"), "{st}");
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

const PLUG: &str = "zb:a4c1380000000001";

/// A Tuya TS011F plug as the interview stores it.
fn ts011f_snapshot(manufacturer: &str, app: u32) -> DeviceSnapshot {
    let fp = Fingerprint {
        manufacturer_name: Some(manufacturer.into()),
        model_id: Some("TS011F".into()),
        app_version: Some(app),
        power_source: Some(1),
        mfr_code: Some(0x1002),
        endpoints: vec![EndpointFp {
            ep: 1,
            profile: 0x0104,
            device_id: 0x0051,
            in_clusters: vec![0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0702, 0x0B04, 0xE000, 0xE001],
            out_clusters: vec![0x000A, 0x0019],
        }],
        ..Default::default()
    };
    DeviceSnapshot {
        id: PLUG.into(),
        kind: DeviceKind::Zigbee { ieee: 0xa4c1380000000001, nwk: 0x1234 },
        fp,
        active_eps: vec![1],
        ..sml001_snapshot()
    }
}

/// A release of the TS011F fixtures whose file name contains one of
/// `names`: signed manifest and shards (no bank: shard-only definitions).
fn ts011f_release(names: &[&str], serial: u16) -> BTreeMap<String, Vec<u8>> {
    ts011f_release_edited(names, serial, |s| s.to_string())
}

/// [`ts011f_release`] with each source passed through `edit`.
fn ts011f_release_edited(names: &[&str], serial: u16, edit: fn(&str) -> String) -> BTreeMap<String, Vec<u8>> {
    let dir = format!("{}/../hub-devices/tests/fixtures/ts011f", env!("CARGO_MANIFEST_DIR"));
    let mut items = Vec::new();
    for n in names {
        let p = format!("{dir}/{n}.yaml");
        let src = edit(&std::fs::read_to_string(&p).unwrap());
        let c = zdefc::compile_str(&src, zdefc::Format::Yaml, &Default::default()).unwrap();
        items.push(Item::new(&p, c.cbor).unwrap());
    }
    let (shards, digest) = zdefc::bank::build_shards(&items.iter().collect::<Vec<_>>()).unwrap();
    let params = BankParams { catalog_date: 20261124, catalog_serial: serial, ..Default::default() };
    let m = zdefc::bank::manifest(&params, &[], &shards, vec![], Some(digest), vec![]);
    let mut files: BTreeMap<String, Vec<u8>> =
        shards.into_iter().map(|(n, s)| (format!("shard-{n:02x}.cbor"), s)).collect();
    files.insert("manifest.cose".into(), key().cose_sign1(&m.encode(), false));
    files
}

/// zigbee-rs-hub 0.3.0 kept a Tuya TS011F plug (`_TZ3000_gjnozsaz`)
/// generic and its Find definition said "the catalog has no definition":
/// the published catalog's shards hold its definitions, but they need
/// vendor templates this firmware lacks and were dropped without a word
/// (`skipped: ["invalid: requires feature 23 ..."]`). Replayed against the
/// published catalog-2026.11.24 (signed manifest and the shards the plug
/// asks for, unchanged): the hub now names them, in the shard's commit, in
/// `catalog/status` `missing`, `definitions/fetch` and the device's
/// diagnostics, until a new manifest.
#[test]
fn published_catalog_names_what_a_ts011f_plug_needs() {
    let dir = format!("{}/../hub-devices/tests/fixtures/catalog-2026.11.24", env!("CARGO_MANIFEST_DIR"));
    let file = |n: &str| std::fs::read(format!("{dir}/{n}")).unwrap();
    let plug_1 = "tuya/ts011f_plug_1 (NEO NAS-WR01B) requires template.tuya_metering_zero_debounce";
    let plug_3 = "tuya/ts011f_plug_3 (Tuya TS011F_plug_3) requires template.tuya";
    // zhc: TS011F_plug_1 (white label NAS-WR01B); with application
    // version 69 TS011F_plug_3 (a version fingerprint).
    for (app, want) in [(77, json!([plug_1])), (69, json!([plug_3, plug_1]))] {
        let store = Store::open(Box::new(MemFlash::new(32, 4096)) as Box<dyn hub_ncp::store::Flash>).unwrap();
        let r = rig(store);
        let snap = ts011f_snapshot("_TZ3000_gjnozsaz", app);
        r.ncp.with_store(|s| s.kv_put(&format!("dev/{PLUG}"), &snap.to_bytes())).unwrap();
        let flash: SharedFlash = Arc::new(Mutex::new(Box::new(MemData::new(1 << 20)) as Box<dyn DataFlash>));
        let app_ = start(&r, &flash);
        let (sid, rx) = session(&app_);
        let mut id = 0;
        // Verified with the firmware's built-in key.
        let m = upload(&app_, sid, &rx, &mut id, "manifest", &file("manifest.cose"), 0, json!({}));
        assert_eq!(m["result"]["catalog_date"], 20261124, "{m}");
        let st = call(&app_, sid, &rx, 900, json!({"type": "catalog/status"}));
        let miss = st["result"]["missing"].clone();
        assert_eq!(miss, json!([{"device": PLUG, "shards": [0x08, 0x7d, 0xb0, 0xc6, 0xd9]}]), "{st}");
        let mut said = J::Null;
        for n in miss[0]["shards"].as_array().unwrap() {
            let n = n.as_u64().unwrap();
            let c = upload(&app_, sid, &rx, &mut id, "shard", &file(&format!("shard-{n:02x}.cbor")), 0, json!({"shard": n}));
            assert_eq!(c["result"]["definitions"], json!([]), "{c}");
            if let Some(w) = c["result"]["refused"].get(PLUG) {
                said = w.clone();
            }
        }
        assert_eq!(said, want, "app {app}");
        let st = call(&app_, sid, &rx, 901, json!({"type": "catalog/status"}));
        assert_eq!(st["result"]["missing"][0]["refused"], want, "{st}");
        let f = call(&app_, sid, &rx, 902, json!({"type": "definitions/fetch", "device": PLUG}));
        assert_eq!(f["result"]["refused"], want, "{f}");
        let d = call(&app_, sid, &rx, 903, json!({"type": "devices/diagnostics", "device": PLUG}));
        assert_eq!(d["result"]["catalog_refused"], want, "{d}");
        let l = call(&app_, sid, &rx, 904, json!({"type": "devices/get", "device": PLUG}))["result"]["resolution"]
            ["layers"]
            .clone();
        assert!(l.as_array().unwrap().iter().all(|x| x["source"] == "generic"), "{l}");
        // A newer release: its shards decide again.
        let newer = ts011f_release(&["ts011f_plug_2", "ts011f_plug_3"], 2);
        let m = upload(&app_, sid, &rx, &mut id, "manifest", &newer["manifest.cose"], 0, json!({}));
        assert_eq!(m["result"]["catalog_serial"], 2, "{m}");
        let st = call(&app_, sid, &rx, 905, json!({"type": "catalog/status"}));
        assert_eq!(st["result"]["missing"][0].get("refused"), None, "{st}");
        app_.shutdown();
        r.ncp.shutdown();
        r.sim.stop();
    }
}

/// A device's keys span several shards (TS011F `_TZ3000_gjnozsaz`: its
/// white-label match in shard b0, the model and version match in d9), and
/// clients upload them in turn. A definition from an earlier shard must
/// not keep a better one of a later shard out: with application version
/// 69 zhc picks TS011F_plug_3 (d9) over TS011F_plug_1 (b0). Both runnable
/// here (the fixtures without their vendor templates).
#[test]
fn a_later_shard_can_still_improve_a_fetched_definition() {
    // The `templates:` entry (and its indented lines) left out.
    let files = ts011f_release_edited(&["ts011f_plug_1", "ts011f_plug_3"], 1, |s| {
        let mut skip = false;
        let keep = |l: &&str| {
            skip = l.starts_with("templates:") || (skip && l.starts_with(' '));
            !skip
        };
        s.lines().filter(keep).collect::<Vec<_>>().join("\n")
    });
    let store = Store::open(Box::new(MemFlash::new(32, 4096)) as Box<dyn hub_ncp::store::Flash>).unwrap();
    let r = rig(store);
    r.ncp.with_store(|s| s.kv_put(&format!("dev/{PLUG}"), &ts011f_snapshot("_TZ3000_gjnozsaz", 69).to_bytes())).unwrap();
    let flash: SharedFlash = Arc::new(Mutex::new(Box::new(MemData::new(1 << 20)) as Box<dyn DataFlash>));
    let app = start(&r, &flash);
    let (sid, rx) = session(&app);
    let mut id = 0;
    let m = upload(&app, sid, &rx, &mut id, "manifest", &files["manifest.cose"], 0, json!({}));
    assert_eq!(m["success"], true, "{m}");
    let st = call(&app, sid, &rx, 900, json!({"type": "catalog/status"}));
    let shards: Vec<u64> =
        st["result"]["missing"][0]["shards"].as_array().unwrap().iter().map(|n| n.as_u64().unwrap()).collect();
    assert_eq!(shards, [0xb0, 0xd9], "{st}");
    let mut stored = Vec::new();
    for n in shards {
        let c = upload(&app, sid, &rx, &mut id, "shard", &files[&format!("shard-{n:02x}.cbor")], 0, json!({"shard": n}));
        stored.extend(c["result"]["definitions"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()));
    }
    assert_eq!(stored, ["tuya/ts011f_plug_1", "tuya/ts011f_plug_3"]);
    let l = call(&app, sid, &rx, 901, json!({"type": "devices/get", "device": PLUG}))["result"]["resolution"]["layers"]
        .clone();
    assert_eq!(l.as_array().unwrap().last().unwrap()["id"], "tuya/ts011f_plug_3", "{l}");
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

/// The first `n` bytes of an upload, no commit (a dropped connection).
fn upload_part(app: &App, sid: u64, rx: &Rx, id: &mut u64, target: &str, bytes: &[u8], n: usize) {
    *id += 1;
    let req = json!({"type": "catalog/upload_chunk", "target": target, "offset": 0, "size": bytes.len(),
        "data": base64::engine::general_purpose::STANDARD.encode(&bytes[..n])});
    assert_eq!(call(app, sid, rx, *id, req)["success"], true);
}

fn sha256(b: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(b).into()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Coordinator-boundary baseline (`docs/coordinator-boundary.md` §7 PR A):
/// the catalog areas of `hubdata` after installing a release (manifest,
/// bank, web tools) and the on-demand fetch of a definition from its
/// shards, byte for byte. The release is frozen in
/// `tests/golden/baseline/catalog/` (built from `definitions/examples` when
/// that directory is absent), so that the golden pins the catalog store,
/// not the compiler or the examples.
#[test]
fn catalog_areas_golden() {
    let dir = crate::baseline::path("catalog");
    let generated = (!dir.join("manifest.cose").exists()).then(|| release_files(3));
    let file = |n: &str| {
        if let Some(files) = &generated {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(n), &files[n]).unwrap();
        }
        std::fs::read(dir.join(n)).unwrap_or_else(|e| {
            panic!("{n}: {e} (remove {} to rebuild the release)", dir.display())
        })
    };
    let store =
        Store::open(Box::new(MemFlash::new(32, 4096)) as Box<dyn hub_ncp::store::Flash>).unwrap();
    let r = rig(store);
    r.ncp
        .with_store(|s| s.kv_put(&format!("dev/{SML}"), &sml001_snapshot().to_bytes()))
        .unwrap();
    let flash: SharedFlash = Arc::new(Mutex::new(
        Box::new(MemData::new(1 << 20)) as Box<dyn DataFlash>
    ));
    let app = start(&r, &flash);
    let (sid, rx) = session(&app);
    let mut id = 0;
    for (target, name) in [
        ("manifest", "manifest.cose"),
        ("bank", "bank-4m.zdb"),
        ("web", "zdefc-web.zwb"),
    ] {
        let c = upload(&app, sid, &rx, &mut id, target, &file(name), 0, json!({}));
        assert_eq!(c["success"], true, "{target}: {c}");
    }
    let f = call(
        &app,
        sid,
        &rx,
        900,
        json!({"type": "definitions/fetch", "device": SML}),
    );
    let shards = f["result"]["shards"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(!shards.is_empty(), "{f}");
    for n in shards {
        let n = n.as_u64().unwrap();
        let s = upload(
            &app,
            sid,
            &rx,
            &mut id,
            "shard",
            &file(&format!("shard-{n:02x}.cbor")),
            0,
            json!({"shard": n}),
        );
        assert_eq!(s["success"], true, "shard {n}: {s}");
    }
    assert_eq!(
        layers(&app, sid, &rx, 901)
            .as_array()
            .unwrap()
            .last()
            .unwrap()["source"],
        "fetched"
    );
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();

    let map = hub_core::layout::HubdataMap::for_size(1 << 20).unwrap();
    let mut text = String::from(
        "# hubdata catalog areas after the install and fetch of catalog_areas_golden (erased runs collapsed).\n",
    );
    for (name, a) in [
        ("catalog_state", map.catalog_state),
        ("manifest", map.manifest),
        ("bank_a", map.bank_a),
        ("bank_b", map.bank_b),
        ("local", map.local),
        ("scratch", map.scratch),
        ("web", map.web),
    ] {
        text += &format!("@ {name} offset 0x{:06x} size 0x{:06x}\n", a.offset, a.size);
        text += &crate::baseline::hexdump(flash.lock().unwrap().map(a.offset, a.size));
    }
    crate::baseline::assert_all(vec![crate::baseline::check("catalog_areas.hex", &text)]);
}
