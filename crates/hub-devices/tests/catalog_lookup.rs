//! The on-demand catalog path for Tuya TS011F plugs (zigbee-rs-hub 0.3.0
//! kept them generic): the shard index a hub looks a device up in
//! (`fingerprint_keys` → shard → `shard_matches`) must find the definition
//! zigbee-herdsman-converters picks, for manufacturer names zhc lists only
//! as white-label fingerprints and for names it does not know (zhc falls
//! back to the `zigbeeModel` definition), and the selection over what it
//! finds must pick that definition and white label. Unlike the parity
//! test this runs on shards and does not stop at definitions that need
//! features this hub lacks: they are decoded with every feature, so the
//! index and the selection are checked for them as well.

use std::collections::BTreeMap;
use std::sync::Arc;

use hub_devices::bankcat::{Fetched, FetchedCatalog, fingerprint_keys};
use hub_devices::fingerprint::{EndpointFp, Fingerprint};
use hub_devices::model::{Definition, LayerSource};
use hub_devices::resolve::{
    Catalog, HUB_FEATURES, MemoryCatalog, Resolution, ResolveInput, resolve,
};
use hub_devices::zdo::LogicalType;

/// zhc 26.130.0 `findByDevice` for a TS011F plug: (manufacturer,
/// applicationVersion) → (definition model, white-label model).
const ZHC: &[(&str, u32, &str, &str)] = &[
    ("_TZ3000_okaz9tjs", 77, "TS011F_plug_1", "LSPA9"),
    ("_TZ3000_okaz9tjs", 69, "TS011F_plug_3", "JR-ZPM03"),
    ("_TZ3000_gjnozsaz", 77, "TS011F_plug_1", "NAS-WR01B"),
    ("_TZ3000_gjnozsaz", 69, "TS011F_plug_3", "TS011F_plug_3"),
    ("_TZ3210_w0qqde0g", 77, "TS011F_plug_1", "HT-SP-ZB-01"),
    ("_TZ3008_tary5dvv", 69, "TS011F_plug_1", "EIGHTREE Socket"),
    ("_TZ3000_hyfvrar3", 69, "TS011F_plug_2", "TS011F_plug_2"),
    // Unknown to zhc: its zigbeeModel definition, or a version fingerprint.
    ("_TZ3000_zzzzzzzz", 77, "TS011F_plug_1", "TS011F_plug_1"),
    ("_TZ3000_zzzzzzzz", 69, "TS011F_plug_3", "TS011F_plug_3"),
    ("_TZ3000_zzzzzzzz ", 77, "TS011F_plug_1", "TS011F_plug_1"),
];

fn plug(manufacturer: &str, app: u32, model: &str) -> Fingerprint {
    Fingerprint {
        manufacturer_name: Some(manufacturer.into()),
        model_id: Some(model.into()),
        app_version: Some(app),
        stack_version: Some(0),
        hw_version: Some(1),
        zcl_version: Some(3),
        power_source: Some(1),
        logical_type: Some(LogicalType::Router),
        mfr_code: Some(0x1002),
        endpoints: vec![EndpointFp {
            ep: 1,
            profile: 0x0104,
            device_id: 0x0051,
            in_clusters: vec![
                0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0702, 0x0B04, 0xE000, 0xE001,
            ],
            out_clusters: vec![0x000A, 0x0019],
        }],
        ..Default::default()
    }
}

/// The shards of the TS011F fixtures, as `zdefc catalog build` writes them.
fn shards() -> BTreeMap<u8, Vec<u8>> {
    let dir = format!("{}/tests/fixtures/ts011f", env!("CARGO_MANIFEST_DIR"));
    let mut items = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let src = std::fs::read_to_string(&p).unwrap();
        let c = zdefc::compile_str(&src, zdefc::Format::Yaml, &zdefc::Options::default()).unwrap();
        items.push(zdefc::bank::Item::new(&p.display().to_string(), c.cbor).unwrap());
    }
    zdefc::bank::build_shards(&items.iter().collect::<Vec<_>>())
        .unwrap()
        .0
}

/// What the shards hold under a device's keys, decoded whatever features
/// it needs.
#[derive(Default)]
struct Found(Vec<Arc<Definition>>);

impl Catalog for Found {
    fn candidates(&self, fp: &Fingerprint) -> Vec<Arc<Definition>> {
        self.0
            .iter()
            .filter(|d| hub_devices::resolve::score(d, fp).is_some())
            .cloned()
            .collect()
    }
    fn get(&self, _: &str) -> Option<Arc<Definition>> {
        None
    }
    fn all(&self) -> Vec<Arc<Definition>> {
        self.0.clone()
    }
}

fn lookup(shards: &BTreeMap<u8, Vec<u8>>, fp: &Fingerprint) -> Found {
    let keys = fingerprint_keys(fp);
    let mut nums: Vec<u8> = keys.iter().map(|k| zdef::bank::shard_of(*k)).collect();
    nums.sort();
    nums.dedup();
    let mut found = Found::default();
    for n in nums {
        let Some(s) = shards.get(&n) else { continue };
        for (_, _, cbor) in zdef::bank::shard_matches(s, &keys).unwrap() {
            let zd = zdef::load(cbor, &zdef::HubFeatures(u128::MAX)).unwrap();
            let d = hub_devices::zdef_adapter::adapt(zd, LayerSource::Fetched, None).unwrap();
            if !found.0.iter().any(|x| x.id == d.id) {
                found.0.push(Arc::new(d));
            }
        }
    }
    found
}

#[test]
fn ts011f_shard_lookup_finds_and_picks_what_zhc_picks() {
    let shards = shards();
    let every: Vec<&str> = (0..128u64)
        .filter_map(zdef::enums::Feature::from_code)
        .map(|f| f.as_str())
        .collect();
    let generic = |_| Vec::new();
    for &(mfr, app, want, want_label) in ZHC {
        // Model IDs as some firmwares send them find the same records (index
        // keys are normalized names, 6.2); zhc's fingerprints compare the
        // raw model ID, so the selection is checked for the plain one.
        for model in ["TS011F\0\0", " TS011F ", "TS011F"] {
            let fp = plug(mfr, app, model);
            let found = lookup(&shards, &fp);
            let ids: Vec<&str> = found.0.iter().map(|d| d.model.as_str()).collect();
            assert!(
                ids.contains(&want),
                "{mfr:?}/{model:?}: the index finds {ids:?}, not {want}"
            );
            if model != "TS011F" {
                continue;
            }
            let res = resolve(&ResolveInput {
                fp: &fp,
                generic: &generic,
                catalog: &found,
                user: &MemoryCatalog::default(),
                self_description: None,
                pinned: None,
                features: &every,
            });
            assert_eq!(
                (res.def_model.as_deref(), res.model.as_deref()),
                (Some(want), Some(want_label)),
                "{mfr:?} app {app} {model:?}"
            );
        }
    }
}

fn resolve_with(cat: &dyn Catalog, fp: &Fingerprint, features: &[&str]) -> Resolution {
    resolve(&ResolveInput {
        fp,
        generic: &|_| Vec::new(),
        catalog: cat,
        user: &MemoryCatalog::default(),
        self_description: None,
        pinned: None,
        features,
    })
}

fn picked(r: &Resolution) -> (Option<&str>, Option<&str>) {
    (r.def_model.as_deref(), r.model.as_deref())
}

/// The plug of the hub 0.3.0 report, `_TZ3000_gjnozsaz` / `TS011F`, against
/// the published catalog-2026.11.24 (the shards it asks for, unchanged):
/// the in-memory matcher (every record of those shards, the parity test's
/// view) and the hub's index path (keys → shard numbers → `shard_matches`)
/// find the same and pick zhc's definition and white label once the hub
/// has the features, and agree with this firmware's features (hub 0.3.0
/// without the template: generic only).
#[test]
fn published_catalog_gjnozsaz_ts011f() {
    let dir = format!(
        "{}/tests/fixtures/catalog-2026.11.24",
        env!("CARGO_MANIFEST_DIR")
    );
    let every: Vec<&str> = (0..128u64)
        .filter_map(zdef::enums::Feature::from_code)
        .map(|f| f.as_str())
        .collect();
    for (app, want) in [
        (77, ("TS011F_plug_1", "NAS-WR01B")),
        (69, ("TS011F_plug_3", "TS011F_plug_3")),
    ] {
        let fp = plug("_TZ3000_gjnozsaz", app, "TS011F");
        let mut nums: Vec<u8> = fingerprint_keys(&fp)
            .iter()
            .map(|k| zdef::bank::shard_of(*k))
            .collect();
        nums.sort();
        nums.dedup();
        assert_eq!(
            nums,
            [0x08, 0x7d, 0xb0, 0xc6, 0xd9],
            "the shards catalog/status names"
        );
        let shards: BTreeMap<u8, Vec<u8>> = nums
            .iter()
            .map(|n| {
                (
                    *n,
                    std::fs::read(format!("{dir}/shard-{n:02x}.cbor")).unwrap(),
                )
            })
            .collect();
        // The index path, decoded whatever features they need.
        let found = lookup(&shards, &fp);
        let mut ids: Vec<&str> = found.0.iter().map(|d| d.id.as_str()).collect();
        ids.sort();
        assert_eq!(
            ids,
            ["tuya/ts011f_plug_1", "tuya/ts011f_plug_3"],
            "app {app}"
        );
        // The in-memory matcher over every record of those shards.
        let mut mem = FetchedCatalog::default();
        for s in shards.values() {
            for r in zdef::bank::decode_shard(s).unwrap() {
                mem.defs.push(Fetched {
                    keys: r.key_hashes,
                    cbor: Box::leak(r.definition.into_boxed_slice()),
                });
            }
        }
        // With every feature both pick zhc's choice.
        assert_eq!(
            picked(&resolve_with(&found, &fp, &every)),
            (Some(want.0), Some(want.1)),
            "app {app}"
        );
        // With this firmware's features both agree as well: TS011F_plug_1
        // (template tuya_metering_zero_debounce); TS011F_plug_3 needs
        // template tuya and is skipped as if it did not match (3.3).
        let on_hub = resolve_with(&mem, &fp, HUB_FEATURES);
        assert_eq!(
            picked(&resolve_with(&found, &fp, HUB_FEATURES)),
            picked(&on_hub),
            "app {app}"
        );
        assert_eq!(
            picked(&on_hub),
            (Some("TS011F_plug_1"), Some("NAS-WR01B")),
            "app {app}"
        );
    }
}
