//! The hub's network-scoped records and the operator's decisions about
//! them (design §2.7): `network/registry/adopt` and
//! `network/registry/clear`, and loading the records once they became
//! readable after the start (adopted, or a network formed or restored over
//! records that were not this hub's).

use hub_devices::device::Millis;
use hub_devices::hub::ApiError;
use hub_coord::RegistryState;
use hub_store::kv;
use serde_json::{Value as J, json};

use super::Runner;
use crate::admin::{self, Head};
use crate::automations;
use crate::transport::registry_json;

impl Runner {
    /// `network/registry/adopt` (the records become this hub's, of the
    /// running network, and are loaded) and `network/registry/clear` (the
    /// network-scoped records and the engine's devices, groups, bindings
    /// and rules are dropped; the NCP's devices are imported again). Both
    /// need `"confirm": true`.
    pub(super) fn registry_request(&mut self, head: &Head, now: Millis) -> String {
        let id = head.id;
        if head.v.get("confirm") != Some(&J::Bool(true)) {
            return admin::frame(id, Err(ApiError::new("invalid_value", "confirm: true is required")));
        }
        let failed = |e: std::io::Error| ApiError::new("failed", e.to_string());
        let adopt = head.ty == "network/registry/adopt";
        let r = if adopt {
            self.ncp.registry_adopt().map_err(failed).map(|_| {
                let n = if self.registry.loaded { 0 } else { self.load_network(now) };
                format!("\"restored\":{n}")
            })
        } else {
            self.forget_network(now).and_then(|forgotten| {
                let deleted = self.ncp.registry_clear().map_err(failed)?;
                self.registry.loaded = true;
                self.imported_in = None;
                self.import_retry = None;
                self.import_gen = self.import_gen.wrapping_add(1);
                Ok(format!("\"deleted\":{deleted},\"forgotten\":{forgotten}"))
            })
        };
        match r {
            Ok(fields) => {
                let reason = if adopt { "registry_adopted" } else { "registry_cleared" };
                self.engine.network_event(json!({"topic": "network_updated", "reason": reason}));
                let registry = registry_json(&self.ncp.registry());
                format!(r#"{{"id":{id},"type":"result","success":true,"result":{{{fields},"registry":{registry}}}}}"#)
            }
            Err(e) => admin::frame(id, Err(e)),
        }
    }

    /// Rules are network records: while those are not writable, requests
    /// that change rules are refused, naming the registry state.
    pub(super) fn automations_refused(&self, kind: &str) -> Option<ApiError> {
        let changes = ["create", "update", "delete", "enable", "disable"];
        if !kind.strip_prefix("automations/").is_some_and(|k| changes.contains(&k)) {
            return None;
        }
        let why = match self.ncp.registry() {
            RegistryState::Untagged | RegistryState::Owned => return None,
            RegistryState::Unverified => "the device registry is unverified",
            RegistryState::Foreign { .. } => "the device registry is foreign: adopt or clear it first",
        };
        Some(ApiError::new("registry_read_only", why))
    }

    /// Records that became readable after the start are loaded (every
    /// tick, before the registry is written).
    pub(super) fn check_registry(&mut self, now: Millis) {
        if !self.registry.loaded && !matches!(self.ncp.registry(), RegistryState::Foreign { .. }) {
            self.load_network(now);
        }
    }

    /// Devices (the stored ones replace those the engine has made since
    /// the start), groups, bindings, last values and rules from the store.
    fn load_network(&mut self, now: Millis) -> usize {
        let n = self.registry.load_network(&self.ncp, &mut self.engine, now);
        self.last.load(&self.ncp, &mut self.engine, now);
        let rules = kv::with(&self.ncp, |s| s.prefix(automations::PREFIX).map(|(k, v)| (k.to_string(), v.to_vec())).collect());
        self.automations.load(rules);
        self.with_automations(|a, w, _| a.prime(w));
        let known: Vec<(u64, u16)> = self.engine.devices().filter_map(|d| Some((d.ieee()?, d.nwk()?))).collect();
        self.engine.transport.nwk.extend(known);
        n
    }

    /// The engine's network state dropped without network traffic: devices,
    /// groups and bindings, the rules. How many devices.
    fn forget_network(&mut self, now: Millis) -> Result<usize, ApiError> {
        let rules: Vec<String> = self.automations.rules().map(|r| r.id.clone()).collect();
        for id in rules {
            if let Some(Err(e)) = self.with_automations(|a, w, clk| a.request("automations/delete", &json!({"automation": id}), w, clk)) {
                return Err(e);
            }
        }
        self.engine.zg_replace_groups(&[], now);
        self.engine.zg.bindings.clear();
        let devices: Vec<String> = self.engine.devices().map(|d| d.id.clone()).collect();
        for id in &devices {
            self.engine.forget_device(id);
        }
        self.engine.transport.nwk.clear();
        Ok(devices.len())
    }
}
