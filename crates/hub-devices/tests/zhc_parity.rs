//! Selection parity with zigbee-herdsman-converters (spec 3.3).
//!
//! For every fingerprint zhc knows (its fingerprints, zigbeeModels and
//! white-label fingerprints), the definition the hub resolves from the
//! catalog's shards must be the one zhc's `findByDevice` picks. Needs the
//! vectors of the definitions repository and a built catalog:
//!
//! ```sh
//! ZHC_VECTORS=vectors.json ZDEF_SHARDS=out/ \
//!   cargo test -p hub-devices --test zhc_parity -- --ignored --nocapture
//! ```
//!
//! `ZHC_PARITY_MAX` (default 0) is the number of mismatches tolerated.

use std::collections::BTreeMap;

use hub_devices::bankcat::{Fetched, FetchedCatalog};
use hub_devices::fingerprint::{EndpointFp, Fingerprint};
use hub_devices::resolve::{HUB_FEATURES, MemoryCatalog, ResolveInput, resolve};
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

/// Every shard record as a fetched definition, and the models present
/// (whether this engine can load them: it has every feature they need).
fn catalog(dir: &str) -> (FetchedCatalog, BTreeMap<String, bool>) {
    let mut cat = FetchedCatalog::default();
    let mut models = BTreeMap::new();
    for i in 0..=255u8 {
        let path = format!("{dir}/shard-{i:02x}.cbor");
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        for r in zdef::bank::decode_shard(&bytes).expect(&path) {
            let cbor: &'static [u8] = Box::leak(r.definition.into_boxed_slice());
            if let Ok(s) = zdef::codec::peek_summary(cbor, &zdef::codec::DecodeOptions::default())
                && let Some(model) = s.model
            {
                let ok = hub_devices::zdef_adapter::load_cbor(
                    cbor,
                    hub_devices::model::LayerSource::Fetched,
                )
                .is_ok();
                *models.entry(model).or_insert(false) |= ok;
            }
            cat.defs.push(Fetched {
                keys: r.key_hashes,
                cbor,
            });
        }
    }
    (cat, models)
}

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
        let Some(expect) = v["expect"].as_object() else {
            continue;
        };
        let want = expect["model"].as_str().unwrap_or("");
        let want_base = expect["base"].as_str().unwrap_or("");
        let fp = fingerprint(&v["fp"]);
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
        } else if !models.contains_key(want_base) {
            "expected definition not in the catalog"
        } else if !models[want_base] {
            "expected definition needs features the hub lacks"
        } else if got_base.is_empty() {
            "generic only"
        } else if got_base != want_base {
            "another definition"
        } else {
            "another white label"
        };
        *counts.entry(class).or_default() += 1;
        let ex = examples.entry(class).or_default();
        if class != "same" && ex.len() < 15 {
            ex.push(format!(
                "{} -> hub {got_base}/{got}, zhc {want_base}/{want}",
                v["fp"]
            ));
        }
    }
    eprintln!("{counts:#?}");
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
        .filter(|(k, _)| **k != "same")
        .map(|(_, n)| n)
        .sum();
    assert!(
        bad <= max,
        "{bad} vectors differ from zhc (tolerated {max})"
    );
}
