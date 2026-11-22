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
        &[BankFile { name: "bank-4m.zdb".into(), bytes: &bank, definitions: bank_items.len() as u32, generation }],
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
    std::fs::write(dir.join("zdefc-web.zwb"), &tools).unwrap();
    std::fs::write(dir.join("manifest.cose"), key().cose_sign1(&m.encode(), false)).unwrap();
    std::fs::write(dir.join("bank-4m.zdb"), &bank).unwrap();
    for (n, s) in &shards {
        std::fs::write(dir.join(format!("shard-{n:02x}.cbor")), s).unwrap();
    }
    bank
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
