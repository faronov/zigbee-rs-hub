//! The hub's network-scoped records and the operator's decisions about
//! them (design §2.7): `network/registry/adopt` and
//! `network/registry/clear`, and loading the records once they became
//! readable after the start (adopted, or a network formed or restored over
//! records that were not this hub's). Each kind (devices with groups and
//! bindings, last values, rules, OTA device states) keeps whether it was
//! loaded: the gate may close between their loads at the start.

use hub_devices::device::Millis;
use hub_devices::hub::ApiError;
use hub_coord::RegistryState;
use hub_store::{Gate, kv};
use log::info;
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
        // The NCP's network as the backend reports it now.
        let st = self.engine.transport.status();
        let report = st.epoch.filter(|_| st.session == hub_coord::SessionState::Ready).map(|e| (&st.ownership, e));
        let r = if adopt {
            self.records.adopt(report).map_err(failed).map(|_| {
                let n = self.load_network(now);
                format!("\"restored\":{n}")
            })
        } else {
            self.forget_network(now).and_then(|forgotten| {
                let deleted = self.records.clear(report).map_err(failed)?;
                self.registry.loaded = true;
                #[cfg(feature = "ota")]
                self.ota.forget_devices();
                self.imported_in = None;
                self.import_retry = None;
                self.import_gen = self.import_gen.wrapping_add(1);
                // What was hidden from the start (last values, rules) is
                // gone too: read now, empty.
                self.load_network(now);
                Ok(format!("\"deleted\":{deleted},\"forgotten\":{forgotten}"))
            })
        };
        match r {
            Ok(fields) => {
                self.automations.read_only = false;
                #[cfg(feature = "ota")]
                self.ota.set_writable(true);
                let reason = if adopt { "registry_adopted" } else { "registry_cleared" };
                self.engine.network_event(json!({"topic": "network_updated", "reason": reason}));
                let registry = registry_json(&self.records.registry());
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
        self.registry_read_only()
    }

    /// Why the network records cannot be written now (`None`: they can).
    pub(super) fn registry_read_only(&self) -> Option<ApiError> {
        let why = match self.records.registry() {
            RegistryState::Untagged | RegistryState::Owned => return None,
            RegistryState::Unverified => "the device registry is unverified",
            RegistryState::Foreign { .. } => "the device registry is foreign: adopt or clear it first",
        };
        Some(ApiError::new("registry_read_only", why))
    }

    /// Every tick, before the registry is written: records that became
    /// readable after the start are loaded, and the rules learn whether
    /// they can be stored (their enable and disable actions).
    pub(super) fn check_registry(&mut self, now: Millis) {
        let state = self.records.registry();
        self.automations.read_only = !matches!(state, RegistryState::Untagged | RegistryState::Owned);
        #[cfg(feature = "ota")]
        self.ota.set_writable(!self.automations.read_only);
        // Not while they are hidden (foreign, or of unknown origin).
        if !self.network_loaded()
            && !matches!(state, RegistryState::Foreign { .. })
            && kv::with(&*self.records, |s| s.gate()) != Gate::Closed
        {
            self.load_network(now);
        }
    }

    /// Whether every kind of network record was loaded.
    fn network_loaded(&self) -> bool {
        #[cfg(feature = "ota")]
        if !self.ota.loaded() {
            return false;
        }
        self.registry.loaded && self.last.loaded && self.automations.loaded
    }

    /// The network records not loaded yet, unless the gate hides them (at
    /// the start, and once they are adopted, cleared or readable): devices
    /// (the stored ones replace those the engine has made since the start),
    /// groups and bindings; then, once the devices are in, their last
    /// values; then the rules and the OTA device states. Loaded ones are
    /// not read again. How many devices.
    pub(super) fn load_network(&mut self, now: Millis) -> usize {
        let n = if self.registry.loaded { 0 } else { self.registry.load_network(&*self.records, &mut self.engine, now) };
        // A value of a device not restored yet would be dropped.
        if self.registry.loaded && !self.last.loaded {
            self.last.load(&*self.records, &mut self.engine, now);
        }
        // State triggers compare with the devices' values: restored first
        // (one primed with a value not known yet would fire on the first
        // report as a change from unknown).
        if self.registry.loaded && self.last.loaded && !self.automations.loaded {
            let rules = self.load_rules();
            if rules > 0 {
                info!("automations: {rules} rules restored");
            }
        }
        // And the state of a device not restored yet, deleted.
        #[cfg(feature = "ota")]
        if self.registry.loaded && !self.ota.loaded() {
            self.ota.load_devices(&*self.records, &self.engine, now);
        }
        self.with_automations(|a, w, _| a.prime(w));
        let known: Vec<(u64, u16)> = self.engine.devices().filter_map(|d| Some((d.ieee()?, d.nwk()?))).collect();
        self.engine.transport.nwk.extend(known);
        n
    }

    /// The stored rules, unless the gate hides them; how many rules.
    fn load_rules(&mut self) -> usize {
        let (gate, rules) = kv::with(&*self.records, |s| {
            (s.gate(), s.prefix(automations::PREFIX).map(|(k, v)| (k.to_string(), v.to_vec())).collect())
        });
        self.automations.loaded = gate != Gate::Closed;
        self.automations.load(rules)
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
