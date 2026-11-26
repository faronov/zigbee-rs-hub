//! Selection parity with zigbee-herdsman-converters (spec 3.3).
//!
//! For every fingerprint zhc knows (its fingerprints, zigbeeModels and
//! white-label fingerprints and their variants), the definition the hub
//! resolves from the catalog's shards must be the one zhc's `findByDevice`
//! picks, or none where zhc finds none. Needs the
//! vectors of the definitions repository and a built catalog:
//!
//! ```sh
//! ZHC_VECTORS=vectors.json ZDEF_SHARDS=out/ \
//!   cargo test -p hub-devices --test zhc_parity -- --ignored --nocapture
//! ```
//!
//! `ZHC_PARITY_MAX` (default 0) is the number of mismatches tolerated;
//! `ZHC_PARITY_SAME` (default 0) the number of vectors that must resolve
//! exactly like zhc (set it to the last result, so that a regression shows).
//! A vector whose definition needs features this hub lacks is tolerated
//! only while the hub resolves it generic only and a hub with the features
//! would pick zhc's definition; picking another definition meanwhile is a
//! mismatch.

use std::collections::BTreeMap;

use std::sync::Arc;

use hub_devices::bankcat::{Fetched, FetchedCatalog, fingerprint_keys};
use hub_devices::fingerprint::{EndpointFp, Fingerprint};
use hub_devices::model::{Definition, LayerSource};
use hub_devices::resolve::{
    Catalog, HUB_FEATURES, MemoryCatalog, ResolveInput, missing_features, resolve,
};
use hub_devices::zdo::LogicalType;
use serde_json::Value as J;

fn fingerprint(v: &J) -> Fingerprint {
    let s = |k: &str| v[k].as_str().map(str::to_string);
    let n = |k: &str| v[k].as_u64().map(|x| x as u32);
    Fingerprint {
        model_id: s("modelID"),
        manufacturer_name: s("manufacturerName"),
        app_version: n("applicationVersion"),
        stack_version: n("stackVersion"),
        hw_version: n("hardwareVersion"),
        zcl_version: n("zclVersion"),
        date_code: s("dateCode"),
        sw_build_id: s("softwareBuildID"),
        mfr_code: v["manufacturerID"].as_u64().map(|x| x as u16),
        power_source: v["powerSource"].as_str().map(|p| match p {
            "Mains (single phase)" => 1,
            "Mains (3 phase)" => 2,
            "Battery" => 3,
            "DC Source" => 4,
            "Emergency mains constantly powered" => 5,
            "Emergency mains and transfer switch" => 6,
            _ => 0,
        }),
        logical_type: v["type"].as_str().and_then(|t| match t {
            "Router" => Some(LogicalType::Router),
            "EndDevice" => Some(LogicalType::EndDevice),
            "Coordinator" => Some(LogicalType::Coordinator),
            _ => None,
        }),
        endpoints: v["endpoints"]
            .as_array()
            .map(|eps| {
                eps.iter()
                    .map(|e| {
                        let list = |k: &str| {
                            e[k].as_array()
                                .map(|c| c.iter().filter_map(J::as_u64).map(|x| x as u16).collect())
                                .unwrap_or_default()
                        };
                        EndpointFp {
                            ep: e["ID"].as_u64().unwrap_or(1) as u8,
                            profile: e["profileID"].as_u64().unwrap_or(0x0104) as u16,
                            device_id: e["deviceID"].as_u64().unwrap_or(0) as u16,
                            in_clusters: list("inputClusters"),
                            out_clusters: list("outputClusters"),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default(),
        ..Default::default()
    }
}

/// Every shard record as a fetched definition, and per model the features
/// this engine lacks to load it (none: it loads).
fn catalog(dir: &str) -> (FetchedCatalog, BTreeMap<String, Vec<String>>) {
    let mut cat = FetchedCatalog::default();
    let mut models: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for i in 0..=255u8 {
        let path = format!("{dir}/shard-{i:02x}.cbor");
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        for r in zdef::bank::decode_shard(&bytes).expect(&path) {
            let cbor: &'static [u8] = Box::leak(r.definition.into_boxed_slice());
            if let Some(d) = load_every(cbor) {
                let lacks = missing_features(&d, HUB_FEATURES);
                match models.get_mut(&d.model) {
                    Some(m) if lacks.len() < m.len() => *m = lacks,
                    Some(_) => {}
                    None => {
                        models.insert(d.model.clone(), lacks);
                    }
                }
            }
            cat.defs.push(Fetched {
                keys: r.key_hashes,
                cbor,
            });
        }
    }
    (cat, models)
}

/// A definition decoded whatever features it needs.
fn load_every(cbor: &[u8]) -> Option<Definition> {
    let zd = zdef::load(cbor, &zdef::HubFeatures(u128::MAX)).ok()?;
    hub_devices::zdef_adapter::adapt(zd, LayerSource::Fetched, None).ok()
}

/// The catalog as a hub with every feature would see it: the selection
/// for definitions that need features this hub lacks (the index and the
/// scoring are the same; only the feature gate is left out).
struct Every(Vec<(Vec<u64>, Arc<Definition>)>);

impl Every {
    fn new(cat: &FetchedCatalog) -> Self {
        Self(
            cat.defs
                .iter()
                .filter_map(|f| Some((f.keys.clone(), Arc::new(load_every(f.cbor)?))))
                .collect(),
        )
    }
}

impl Catalog for Every {
    fn candidates(&self, fp: &Fingerprint) -> Vec<Arc<Definition>> {
        let keys = fingerprint_keys(fp);
        self.0
            .iter()
            .filter(|(k, _)| k.iter().any(|h| keys.contains(h)))
            .map(|(_, d)| d.clone())
            .collect()
    }
    fn get(&self, _: &str) -> Option<Arc<Definition>> {
        None
    }
    fn all(&self) -> Vec<Arc<Definition>> {
        Vec::new()
    }
}

/// Vectors whose definition needs features this hub lacks (vendor
/// templates, ...), that this hub resolves generic only and a hub with the
/// features like zhc: a feature gap, not a parity difference (the catalog
/// service names the definition, `catalog_refused`).
const NEEDS_FEATURES: &str = "needs features the hub lacks: generic only (zhc's choice with them)";

#[test]
#[ignore = "needs ZHC_VECTORS and ZDEF_SHARDS"]
fn hub_picks_the_definition_zhc_picks() {
    let (Ok(vectors), Ok(shards)) = (std::env::var("ZHC_VECTORS"), std::env::var("ZDEF_SHARDS"))
    else {
        eprintln!("ZHC_VECTORS/ZDEF_SHARDS not set");
        return;
    };
    let vectors: Vec<J> =
        serde_json::from_str(&std::fs::read_to_string(&vectors).unwrap()).unwrap();
    let (cat, models) = catalog(&shards);
    let every = Every::new(&cat);
    let all_features: Vec<&str> = (0..128u64)
        .filter_map(zdef::enums::Feature::from_code)
        .map(|f| f.as_str())
        .collect();
    let mut lacking: BTreeMap<String, usize> = BTreeMap::new();
    eprintln!(
        "{} vectors, {} catalog definitions",
        vectors.len(),
        cat.defs.len()
    );
    let user = MemoryCatalog::default();
    let generic = |_| Vec::new();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut examples: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for v in &vectors {
        // `expect: null`: zhc finds no definition, nor may the hub.
        let expect = v["expect"].as_object();
        let want = expect.and_then(|e| e["model"].as_str()).unwrap_or("");
        let want_base = expect.and_then(|e| e["base"].as_str()).unwrap_or("");
        let mut fp = fingerprint(&v["fp"]);
        // Only what the interview stores (zhc lists every endpoint).
        let ids: Vec<u8> = fp.endpoints.iter().map(|e| e.ep).collect();
        let stored = hub_devices::hub::interview_endpoints(&ids);
        fp.endpoints.retain(|e| stored.contains(&e.ep));
        let res = resolve(&ResolveInput {
            fp: &fp,
            generic: &generic,
            catalog: &cat,
            user: &user,
            self_description: None,
            pinned: None,
            features: HUB_FEATURES,
        });
        let got_base = res.def_model.as_deref().unwrap_or("");
        let got = res.model.as_deref().unwrap_or("");
        let class = if got_base == want_base && got == want {
            "same"
        } else if expect.is_none() {
            "zhc finds no definition"
        } else if !models.contains_key(want_base) {
            "expected definition not in the catalog"
        } else if !models[want_base].is_empty() {
            // What a hub with those features would pick: the gap is a
            // feature, not the index or the selection.
            let with = resolve(&ResolveInput {
                fp: &fp,
                generic: &generic,
                catalog: &every,
                user: &user,
                self_description: None,
                pinned: None,
                features: &all_features,
            });
            if with.def_model.as_deref() != Some(want_base) || with.model.as_deref() != Some(want) {
                "needs features, and picks another definition with them"
            } else if !got_base.is_empty() {
                // Meanwhile the hub runs another definition (e.g. TS011F
                // with application version 69: TS011F_plug_1 while zhc's
                // TS011F_plug_3 needs template.tuya).
                "needs features, another definition meanwhile"
            } else {
                for f in &models[want_base] {
                    *lacking.entry(f.clone()).or_default() += 1;
                }
                NEEDS_FEATURES
            }
        } else if got_base.is_empty() {
            "generic only"
        } else if got_base != want_base {
            "another definition"
        } else {
            "another white label"
        };
        *counts.entry(class).or_default() += 1;
        let ex = examples.entry(class).or_default();
        if class != "same" && class != NEEDS_FEATURES && ex.len() < 15 {
            ex.push(format!(
                "{} -> hub {got_base}/{got}, zhc {want_base}/{want}",
                v["fp"]
            ));
        }
    }
    eprintln!("{counts:#?}");
    eprintln!("\n{NEEDS_FEATURES}, by feature: {lacking:#?}");
    for (class, ex) in &examples {
        if !ex.is_empty() {
            eprintln!("\n{class}:");
            for e in ex {
                eprintln!("  {e}");
            }
        }
    }
    let max: usize = std::env::var("ZHC_PARITY_MAX")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let bad: usize = counts
        .iter()
        .filter(|(k, _)| **k != "same" && **k != NEEDS_FEATURES)
        .map(|(_, n)| n)
        .sum();
    assert!(
        bad <= max,
        "{bad} vectors differ from zhc (tolerated {max})"
    );
    let same = counts.get("same").copied().unwrap_or(0);
    let min: usize = std::env::var("ZHC_PARITY_SAME")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert!(
        same >= min,
        "{same} vectors resolve like zhc, {min} expected (ZHC_PARITY_SAME)"
    );
}
