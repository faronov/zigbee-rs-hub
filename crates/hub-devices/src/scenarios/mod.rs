//! Golden scenarios: captured/synthesized frames through the full engine
//! (interview → resolution → entities → configure → states → commands).

mod gp;
mod plain;
mod props;
mod tuya_trv;
mod vendors;

use serde_json::Value as J;

use crate::hub::{Hub, HubConfig, HubEvent};
use crate::resolve::MemoryCatalog;
use crate::testutil::yaml::example;
use crate::transport::MockTransport;

/// Start of all scenario clocks (2026-11-19 12:00 UTC).
pub const T0: u64 = 1_795_089_600_000;

/// A hub with the given example definitions in its catalog.
pub fn hub_with(examples: &[&str]) -> Hub<MockTransport> {
    let mut cat = MemoryCatalog::default();
    for e in examples {
        cat.insert(example(e));
    }
    Hub::new(MockTransport::default(), HubConfig::default(), Box::new(cat))
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
            HubEvent::EntityEvent { uid: u, event_type, attributes, .. } if u == uid => Some((event_type.clone(), attributes.clone())),
            _ => None,
        })
        .collect()
}

/// Live entity keys of a device as `ep:key`.
pub fn keys(hub: &Hub<MockTransport>, id: &str) -> Vec<String> {
    hub.device(id).unwrap().entities.iter().filter(|e| !e.orphaned).map(|e| format!("{}:{}", e.r.ep, e.r.def.key)).collect()
}

/// Current state value of a uid.
pub fn state(hub: &Hub<MockTransport>, uid: &str) -> J {
    hub.state_of(uid).unwrap_or_else(|| panic!("no entity {uid}"))["state"].clone()
}

/// The command result of a ticket.
pub fn result_of(events: &[HubEvent], ticket: u64) -> Option<Result<String, crate::hub::ApiError>> {
    events.iter().find_map(|e| match e {
        HubEvent::CommandResult { ticket: t, result } if *t == ticket => Some(result.clone()),
        _ => None,
    })
}
