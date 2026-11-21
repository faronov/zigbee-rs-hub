//! The engine on a catalog bank read in place (spec 9.1): resolution by
//! index keys, pinning and `definitions/get` by `id` (block scan), paged
//! listings, swapping the bank, fetched definitions.

use std::sync::Arc;

use serde_json::{Value as J, json};

use super::*;
use crate::api::ApiServer;
use crate::bankcat::{BankCatalog, Fetched, FetchedCatalog, StoreCatalog, fingerprint_keys};
use crate::model::LayerSource;
use crate::resolve::Catalog;
use crate::testutil::sim::{Sim, SimDevice};
use crate::zcl::{TypedValue, ZclType};
use crate::zdo::LogicalType;

/// A format 2 bank of every example definition, leaked like a mapping.
fn example_bank() -> &'static [u8] {
    let dir = format!("{}/../../definitions/examples", env!("CARGO_MANIFEST_DIR"));
    let mut items = Vec::new();
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    paths.sort();
    for p in paths {
        let c = zdefc::compile_str(
            &std::fs::read_to_string(&p).unwrap(),
            zdefc::Format::Yaml,
            &zdefc::Options::default(),
        )
        .unwrap();
        if !c.def.is_fragment() {
            items.push(zdefc::bank::Item::new(&p.display().to_string(), c.cbor).unwrap());
        }
    }
    let refs: Vec<&zdefc::bank::Item> = items.iter().collect();
    let b = zdefc::bank::build_bank(&refs, &zdefc::bank::BankParams::default()).unwrap();
    assert_eq!(&b.bytes[4..6], &zdef::bank::FORMAT_V2.to_le_bytes());
    Box::leak(b.bytes.into_boxed_slice())
}

fn bank_catalog() -> BankCatalog {
    BankCatalog::open(example_bank(), || Box::new(zdef::bank::MinizInflate::new())).unwrap()
}

fn sml001() -> SimDevice {
    SimDevice::new(
        0x0017880103334444,
        0x6003,
        LogicalType::EndDevice,
        false,
        0x100B,
    )
    .endpoint(
        1,
        0xC05E,
        0x0850,
        &[0x0000],
        &[0x0000, 0x0003, 0x0004, 0x0006, 0x0008, 0x0300],
    )
    .endpoint(
        2,
        0x0104,
        0x0107,
        &[0x0000, 0x0001, 0x0003, 0x0406, 0x0400, 0x0402],
        &[0x0019],
    )
    .basic("Signify Netherlands B.V.", "SML001", 0x03)
    .attr(2, 0x0406, 0x0000, TypedValue::uint(ZclType::MAP8, 1))
}

const ID: &str = "zb:0017880103334444";

fn hub_on(cat: StoreCatalog) -> (Hub<MockTransport>, Sim) {
    let mut hub = Hub::new(
        MockTransport::default(),
        HubConfig::default(),
        Box::new(cat),
    );
    let dev = sml001();
    let ieee = dev.ieee;
    let mut sim = Sim::default();
    sim.devices.push(dev);
    sim.join(&mut hub, ieee, T0);
    (hub, sim)
}

fn call(api: &mut ApiServer, hub: &mut Hub<MockTransport>, s: u64, id: u64, mut req: J) -> J {
    req["id"] = id.into();
    let out = api.handle_text(hub, s, &req.to_string(), T0 + 10_000);
    let v: J = serde_json::from_str(out.last().unwrap()).unwrap();
    assert_eq!(v["success"], true, "{v}");
    v["result"].clone()
}

#[test]
fn bank_lookup_resolves_by_index_keys() {
    let b = bank_catalog();
    let fp = crate::fingerprint::Fingerprint {
        manufacturer_name: Some("Signify Netherlands B.V.".into()),
        model_id: Some("SML001".into()),
        ..Default::default()
    };
    let ids: Vec<String> = b.candidates(&fp).iter().map(|d| d.id.clone()).collect();
    assert_eq!(ids, ["philips/sml001"], "only what the index keys reach");
    assert!(
        b.candidates(&crate::fingerprint::Fingerprint::default())
            .is_empty()
    );
    let d = b
        .get("tuya/ts0601-thermostat-3")
        .expect("located by a block scan");
    assert_eq!(
        (d.source, d.vendor.as_str()),
        (LayerSource::Catalog, "Tuya")
    );
    assert!(d.content_hash.is_some());
    assert!(b.get("nope/none").is_none());
    let (total, page) = b.list(0, 3, None);
    assert_eq!(
        (total, page.len()),
        (b.bank().header.definitions as usize, 3)
    );
    let (n, hits) = b.list(0, 50, Some("PHILIPS"));
    assert_eq!(n, 2, "{hits:?}");
    assert_eq!(b.status()["format"], 2);
    assert_eq!(b.ram_bytes(), 0, "nothing held between lookups");
}

#[test]
fn engine_on_a_bank_store_catalog() {
    let (mut hub, _sim) = hub_on(StoreCatalog {
        bank: Some(bank_catalog()),
        ..Default::default()
    });
    let layers = &hub.device(ID).unwrap().resolution.layers;
    assert_eq!(
        layers.last().map(|l| (l.source, l.id.as_str())),
        Some((LayerSource::Catalog, "philips/sml001"))
    );
    let mut api = ApiServer::new("hub", "t");
    let (s, _) = api.connect();
    api.handle_text(&mut hub, s, r#"{"type":"auth","token":"t"}"#, T0);
    // Device-filtered list goes through the index.
    let l = call(
        &mut api,
        &mut hub,
        s,
        1,
        json!({"type": "definitions/list", "device": ID}),
    );
    assert_eq!(
        l.as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].clone())
            .collect::<Vec<_>>(),
        [json!("philips/sml001")]
    );
    // Paged listing over the bank.
    let p = call(
        &mut api,
        &mut hub,
        s,
        2,
        json!({"type": "definitions/list", "query": "tuya", "limit": 1}),
    );
    assert_eq!(
        (p["total"].as_u64(), p["items"].as_array().map(Vec::len)),
        (Some(2), Some(1))
    );
    let p2 = call(
        &mut api,
        &mut hub,
        s,
        3,
        json!({"type": "definitions/list", "query": "tuya", "offset": 1, "limit": 5}),
    );
    assert_ne!(p["items"][0]["id"], p2["items"][0]["id"]);
    // get by id (scan) with CBOR, and status with the bank facts.
    let g = call(
        &mut api,
        &mut hub,
        s,
        4,
        json!({"type": "definitions/get", "definition": "innr/sp-120"}),
    );
    assert!(g["cbor"].as_str().is_some_and(|c| !c.is_empty()));
    let st = call(&mut api, &mut hub, s, 5, json!({"type": "catalog/status"}));
    assert!(st["definitions"].is_number(), "{st}");
    assert_eq!(hub.catalog().status()["bank"]["format"], 2);
    // Pin another catalog definition (located by scan), then unpin.
    call(
        &mut api,
        &mut hub,
        s,
        6,
        json!({"type": "devices/pin_definition", "device": ID, "definition": "philips/rwl022"}),
    );
    assert_eq!(
        hub.device(ID).unwrap().resolution.layers.last().unwrap().id,
        "philips/rwl022"
    );
    call(
        &mut api,
        &mut hub,
        s,
        7,
        json!({"type": "devices/pin_definition", "device": ID, "definition": null}),
    );
    assert_eq!(
        hub.device(ID).unwrap().resolution.layers.last().unwrap().id,
        "philips/sml001"
    );

    // Swapping in an empty catalog re-resolves to generic and reports it.
    let report = hub.set_catalog(Box::new(StoreCatalog::default()), T0 + 20_000);
    assert_eq!(report.len(), 1, "{report:?}");
    assert_eq!(report[0]["device"], ID);
    assert_eq!(report[0]["from"], json!(["philips/sml001"]));
    assert_eq!(report[0]["to"], json!([]));
    assert!(!report[0]["orphaned"].as_array().unwrap().is_empty());
}

#[test]
fn fetched_definitions_resolve_and_pin() {
    let cbor: &'static [u8] = Box::leak(golden_cbor("philips-sml001-motion").into_boxed_slice());
    let fp = sml001_fp();
    let fetched = FetchedCatalog {
        defs: vec![Fetched {
            keys: fingerprint_keys(&fp)[..1].to_vec(),
            cbor,
        }],
    };
    let (hub, _sim) = hub_on(StoreCatalog {
        fetched,
        ..Default::default()
    });
    let l = hub.device(ID).unwrap().resolution.layers.last().unwrap();
    assert_eq!(
        (l.source, l.id.as_str()),
        (LayerSource::Fetched, "philips/sml001")
    );
    // A fetched record whose keys do not cover the device is not offered.
    let other = FetchedCatalog {
        defs: vec![Fetched {
            keys: vec![1],
            cbor,
        }],
    };
    assert!(other.candidates(&fp).is_empty());
    assert!(other.get("philips/sml001").is_some());
    let _ = Arc::new(());
}

fn sml001_fp() -> crate::fingerprint::Fingerprint {
    crate::fingerprint::Fingerprint {
        manufacturer_name: Some("Signify Netherlands B.V.".into()),
        model_id: Some("SML001".into()),
        ..Default::default()
    }
}
