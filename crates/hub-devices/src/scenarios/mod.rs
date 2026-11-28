//! Golden scenarios: captured/synthesized frames through the full engine
//! (interview → resolution → entities → configure → states → commands).

mod app_frames;
mod bank;
#[cfg(feature = "advanced")]
mod console;
mod gp;
mod groups;
mod import;
mod lumi;
mod merge;
mod plain;
mod props;
mod remotes;
mod reporting;
mod templates;
mod tuya_plug;
mod tuya_trv;
mod vendors;

use serde_json::Value as J;

use crate::hub::{Hub, HubConfig, HubEvent};
use crate::resolve::MemoryCatalog;
use crate::testutil::yaml::example;
use crate::transport::MockTransport;

/// Start of all scenario clocks (2026-11-19 12:00 UTC).
pub const T0: u64 = 1_795_089_600_000;

/// The CBOR of an example as compiled by `zdefc` (its golden files).
pub fn golden_cbor(name: &str) -> Vec<u8> {
    let p = format!(
        "{}/../zdefc/tests/golden/{name}.cbor",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}"))
}

/// An example as the hub loads it: zdefc CBOR → `zdef::load` → adapter.
pub fn catalog_def(name: &str) -> crate::model::Definition {
    crate::zdef_adapter::load_cbor(&golden_cbor(name), crate::model::LayerSource::Catalog)
        .unwrap_or_else(|e| panic!("{name}: {e:?}"))
}

/// A hub with the given example definitions in its catalog (CBOR path).
pub fn hub_with(examples: &[&str]) -> Hub<MockTransport> {
    let mut cat = MemoryCatalog::default();
    for e in examples {
        cat.insert(catalog_def(e));
    }
    Hub::new(
        MockTransport::default(),
        HubConfig::default(),
        Box::new(cat),
    )
}

/// The same hub with the definitions from the YAML sources through the
/// test loader and the reference interpreter (cross-check).
pub fn hub_with_yaml(examples: &[&str]) -> Hub<MockTransport> {
    let mut cat = MemoryCatalog::default();
    for e in examples {
        cat.insert(example(e));
    }
    Hub::new(
        MockTransport::default(),
        HubConfig::default(),
        Box::new(cat),
    )
}

/// States published for a uid.
pub fn states(events: &[HubEvent], uid: &str) -> Vec<J> {
    events
        .iter()
        .filter_map(|e| match e {
            HubEvent::EntityState(s) if s["uid"] == uid => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// Events fired for a uid: `(event_type, attributes)`.
pub fn fired(events: &[HubEvent], uid: &str) -> Vec<(String, serde_json::Map<String, J>)> {
    events
        .iter()
        .filter_map(|e| match e {
            HubEvent::EntityEvent {
                uid: u,
                event_type,
                attributes,
                ..
            } if u == uid => Some((event_type.clone(), attributes.clone())),
            _ => None,
        })
        .collect()
}

/// Live entity keys of a device as `ep:key`.
pub fn keys(hub: &Hub<MockTransport>, id: &str) -> Vec<String> {
    hub.device(id)
        .unwrap()
        .entities
        .iter()
        .filter(|e| !e.orphaned)
        .map(|e| format!("{}:{}", e.r.ep, e.r.def.key))
        .collect()
}

/// Differential check of the merge (README "zdef::resolve"): the entities
/// a catalog definition contributes, as `ep:key`, are the same in the
/// engine's resolution and in `zdef::resolve` given only that definition.
/// The engine keeps its own merge for the generic tier; this pins the
/// definition-layer semantics (default endpoints, `present_if`, `parts`)
/// to the reference implementation.
pub fn assert_zdef_resolve_agrees(hub: &Hub<MockTransport>, id: &str, example: &str) {
    let d = hub.device(id).unwrap();
    let mut engine: Vec<String> = d
        .entities
        .iter()
        .filter(|e| !e.orphaned && e.r.source == crate::model::LayerSource::Catalog)
        .map(|e| format!("{}:{}", e.r.ep, e.r.def.key))
        .collect();
    engine.sort();
    let zd = zdef::load(&golden_cbor(example), &crate::zdef_adapter::hub_features()).unwrap();
    let fp = crate::zdef_adapter::zdef_fingerprint(&d.fp);
    let layers = zdef::resolve::Layers {
        generic: &[],
        generic_consts: &[],
        catalog: Some((zdef::matching::Source::Catalog, &zd)),
        self_description: None,
        user: None,
        overrides: None,
    };
    let r = zdef::resolve::resolve(&layers, zdef::resolve::Target::Zigbee(&fp));
    let mut reference: Vec<String> = r
        .entities
        .iter()
        .map(|e| format!("{}:{}", e.ep, e.key()))
        .collect();
    reference.sort();
    assert_eq!(engine, reference, "{example}: engine vs zdef::resolve");
}

/// Current state value of a uid.
pub fn state(hub: &Hub<MockTransport>, uid: &str) -> J {
    hub.state_of(uid)
        .unwrap_or_else(|| panic!("no entity {uid}"))["state"]
        .clone()
}

/// The command result of a ticket.
pub fn result_of(events: &[HubEvent], ticket: u64) -> Option<Result<String, crate::hub::ApiError>> {
    events.iter().find_map(|e| match e {
        HubEvent::CommandResult { ticket: t, result } if *t == ticket => Some(result.clone()),
        _ => None,
    })
}
