//! Green Power (spec 13): identity, commissioning, the generic GP tier,
//! GP definitions and GP bindings (D23). The NCP runs the sink; the hub
//! turns `EVT_GP_*` into devices, entity updates, events and bindings.

pub mod bindings;
pub mod generic;
pub mod types;

use std::collections::BTreeMap;

use serde_json::json;

use crate::device::{Device, DeviceKind, GpState, Millis};
use crate::expr::Value;
use crate::fingerprint::{EndpointFp, GpFingerprint};
use crate::hub::{ApiError, DKey, DevEnv, Hub, HubEvent, eval_trigger, set_value};
use crate::model::{Binding, DeviceField, TriggerSource};
use crate::transport::ZigbeeTransport;
use crate::zcl::PROFILE_GP;
use types::{GpEvent, GpFrame, GpRequest, GpSinkRecord, GpdId, frame_result};

/// The Green Power endpoint.
pub const GP_ENDPOINT: u8 = 242;

/// `SinkDrop` reasons of `EVT_GP_COMMISSIONING_FAILED` (ZRHP 7.11).
pub fn sink_drop_reason(r: u8) -> &'static str {
    match r {
        1 => "Invalid",
        2 => "SecurityPolicy",
        3 => "Stale",
        4 => "TableFull",
        5 => "NotCommissioning",
        6 => "UnknownGpd",
        7 => "KeyMismatch",
        8 => "NoTempMaster",
        9 => "Unsupported",
        10 => "Duplicate",
        11 => "AuthFailed",
        12 => "ChannelMismatch",
        13 => "QueueFull",
        _ => "Unknown",
    }
}

/// GP state of the hub.
#[derive(Debug, Default)]
pub struct GpHubState {
    /// Last commissioning payload per GPD (kept 30 s).
    pub payloads: BTreeMap<GpdId, (Millis, Vec<u8>)>,
    /// Bindings.
    pub bindings: Vec<bindings::GpBindingRecord>,
    /// Probed target capabilities by device ID.
    pub caps: BTreeMap<String, bindings::TargetCaps>,
    /// Outstanding remote requests by ZCL TSN.
    pub remote: BTreeMap<u8, bindings::RemoteWait>,
    /// Next binding ID.
    pub next_id: u32,
    /// Commissioning window open.
    pub commissioning: bool,
}

fn fp_from_payload(gpd_id: GpdId, device_id: u8, payload: Option<&[u8]>, level: u8) -> GpFingerprint {
    let mut g = GpFingerprint {
        gpd_id,
        device_id,
        manufacturer_id: None,
        model_id: None,
        commands: Vec::new(),
        server_clusters: Vec::new(),
        client_clusters: Vec::new(),
        switch_info: None,
        security_level: level,
        rx_on_capability: false,
    };
    if let Some(cp) = payload.and_then(|p| zigbee_gp::commissioning::CommissioningPayload::decode(p).ok()) {
        g.device_id = cp.device_id;
        g.rx_on_capability = cp.rx_on_capability;
        if let Some(ai) = cp.app_info {
            g.manufacturer_id = ai.manufacturer_id;
            g.model_id = ai.model_id;
            if let Some(c) = ai.gpd_commands {
                g.commands = c.iter().copied().collect();
            }
            if let Some((s, c)) = ai.clusters {
                g.server_clusters = s.iter().copied().collect();
                g.client_clusters = c.iter().copied().collect();
            }
            if let Some(si) = ai.switch_info {
                g.switch_info = Some((si.num_contacts, si.switch_type));
            }
        }
    }
    g
}

impl<T: ZigbeeTransport> Hub<T> {
    /// GP events from the NCP.
    pub fn on_gp_event(&mut self, ev: GpEvent, now: Millis) {
        match ev {
            GpEvent::Frame(f) => self.gp_frame(f, now),
            GpEvent::Commissioning { state, window_s, exit_reason } => {
                self.gp.commissioning = state == 1;
                let reason = match exit_reason {
                    1 => "explicit",
                    2 => "first_pairing",
                    3 => "timeout",
                    _ => "none",
                };
                self.emit(HubEvent::Gp {
                    kind: "gp/commissioning".into(),
                    body: json!({"state": if state == 1 { "open" } else { "closed" }, "remaining_s": window_s, "exit_reason": reason}),
                });
            }
            GpEvent::Commissioned { gpd_id, device_id, flags, record } => self.gp_commissioned(gpd_id, device_id, flags, record, now),
            GpEvent::CommissioningFailed { gpd_id, reason } => {
                self.emit(HubEvent::Gp { kind: "gp/failed".into(), body: json!({"gpd": gpd_id.device_id(), "reason": sink_drop_reason(reason)}) });
            }
            GpEvent::Decommissioned { gpd_id } => {
                let id = gpd_id.device_id();
                if self.devices.contains_key(&id) {
                    let _ = self.remove_gp_device_local(&id);
                }
            }
            GpEvent::RemoteStatus { src_nwk, zcl_tsn, command_id, zcl_status } => self.gp_remote_status(src_nwk, zcl_tsn, command_id, zcl_status, now),
            GpEvent::RemoteSinkTable { src_nwk, zcl_tsn, status, records } => self.gp_remote_sink_table(src_nwk, zcl_tsn, status, &records, now),
            GpEvent::RemoteTranslationTable { src_nwk, zcl_tsn, status, records } => {
                self.gp_remote_translation_table(src_nwk, zcl_tsn, status, &records, now)
            }
        }
    }

    fn gp_commissioned(&mut self, gpd_id: GpdId, device_id: u8, flags: u8, record: GpSinkRecord, now: Millis) {
        let payload = self.gp.payloads.remove(&gpd_id).filter(|(t, _)| now.saturating_sub(*t) <= 30_000).map(|(_, p)| p);
        let id = gpd_id.device_id();
        let fresh = !self.devices.contains_key(&id);
        let level = record.security_level;
        let mut g = fp_from_payload(gpd_id, device_id, payload.as_deref(), level);
        if payload.is_none() {
            if let Some(old) = self.devices.get(&id).and_then(|d| d.fp.gp.clone()) {
                // Re-commissioning without a payload keeps the known identity.
                g = GpFingerprint { security_level: level, ..old };
            }
        }
        let d = self.devices.entry(id.clone()).or_insert_with(|| Device::new(id.clone(), DeviceKind::Gp { gpd_id }, now));
        d.fp.gp = Some(g);
        d.fp.endpoints = vec![EndpointFp {
            ep: GP_ENDPOINT,
            profile: PROFILE_GP,
            device_id: u16::from(device_id),
            in_clusters: Vec::new(),
            out_clusters: Vec::new(),
        }];
        d.interview = crate::device::InterviewStep::Done;
        d.last_seen = Some(now);
        d.gp = Some(GpState { frame_counter: record.frame_counter, security_level: level, sink: Some(record), contacts: 0, recent: Default::default() });
        if fresh {
            self.emit(HubEvent::DeviceAdded(id.clone()));
        }
        self.resolve_device(&id, now);
        self.gp_update_warnings(&id);
        self.gp_device_fields(&id, now);
        let rec = self.devices[&id].record();
        let layers = serde_json::to_value(&self.devices[&id].resolution.layers).unwrap_or_default();
        self.emit(HubEvent::Gp { kind: "gp/commissioned".into(), body: json!({"device": rec, "bidirectional": flags & 1 != 0, "layers": layers}) });
    }

    fn gp_update_warnings(&mut self, id: &str) {
        let Some(d) = self.devices.get_mut(id) else { return };
        let level = d.gp.as_ref().map_or(0, |g| g.security_level);
        let min = d.resolution.gp.as_ref().and_then(|g| g.security_min).unwrap_or(0);
        d.warnings.retain(|w| w != "gp_unsecured");
        if level == 0 || level < min {
            d.warnings.push("gp_unsecured".into());
        }
    }

    fn gp_device_fields(&mut self, id: &str, now: Millis) {
        let Some(d) = self.devices.get_mut(id) else { return };
        let Some(g) = d.gp.clone() else { return };
        let mut touched = Vec::new();
        for i in 0..d.entities.len() {
            let v = match d.entities[i].r.def.value.binding {
                Binding::DeviceField(DeviceField::FrameCounter) => Value::Int(i64::from(g.frame_counter)),
                Binding::DeviceField(DeviceField::SecurityLevel) => Value::Int(i64::from(g.security_level)),
                _ => continue,
            };
            if d.entities[i].state.value != v {
                set_value(d, i, None, v, now);
                touched.push(d.entities[i].uid.clone());
            }
        }
        for u in touched {
            self.publish_state(&u);
        }
    }

    fn gp_frame(&mut self, f: GpFrame, now: Millis) {
        if f.command_id == 0xE0 || f.command_id == 0xE3 {
            if f.command_id == 0xE0 && matches!(f.result, frame_result::COMMISSIONED | frame_result::COMMISSIONING_REPLY_SENT) {
                self.gp.payloads.retain(|_, (t, _)| now.saturating_sub(*t) <= 30_000);
                self.gp.payloads.insert(f.gpd_id, (now, f.payload.clone()));
            }
            if matches!(f.result, frame_result::COMMISSIONING_REPLY_SENT | frame_result::COMMISSIONING_STEP) {
                self.emit(HubEvent::Gp {
                    kind: "gp/pending".into(),
                    body: json!({"gpd": f.gpd_id.device_id(), "command_id": f.command_id, "result": f.result}),
                });
            }
            return;
        }
        if !matches!(f.result, frame_result::TRANSLATED | frame_result::PASSED_THROUGH) {
            return;
        }
        let id = f.gpd_id.device_id();
        if !self.devices.contains_key(&id) {
            // In the NCP's Sink Table but unknown to the hub (imported):
            // resolve on DeviceID and SrcID only (13.2).
            let record = GpSinkRecord {
                gpd_id: f.gpd_id,
                device_id: f.device_id,
                communication_mode: 2,
                group: self.cfg.gp_group,
                assigned_alias: 0xFFFF,
                flags: 0,
                security_level: f.security_level,
                key_type: f.key_type,
                key: None,
                frame_counter: f.frame_counter,
                groupcast_radius: 0,
            };
            self.gp_commissioned(f.gpd_id, f.device_id, 0, record, now);
        }
        // Hub-side de-duplication (the NCP already filters proxies' copies).
        {
            let d = self.devices.get_mut(&id).unwrap();
            let g = d.gp.get_or_insert_with(|| GpState { sink: None, frame_counter: 0, security_level: 0, contacts: 0, recent: Default::default() });
            g.recent.retain(|(t, _, _)| now.saturating_sub(*t) < 2_000);
            if g.recent.iter().any(|(_, c, cmd)| *c == f.frame_counter && *cmd == f.command_id) {
                d.diag.count("gp_duplicate");
                return;
            }
            g.recent.push_back((now, f.frame_counter, f.command_id));
            g.frame_counter = g.frame_counter.max(f.frame_counter);
            g.security_level = f.security_level;
            if d.resolution.gp.as_ref().is_some_and(|x| x.ignore_commands.contains(&f.command_id)) {
                return;
            }
        }
        self.seen(&id, Some(f.lqi), Some(f.rssi), now);
        self.gp_device_fields(&id, now);
        match f.command_id {
            0xA0..=0xA3 => self.gp_report(&id, &f, now),
            0xA8 => {
                if let Some(d) = self.devices.get_mut(&id) {
                    d.diag.gp_data.insert(f.command_id, crate::expr::hex(&f.payload));
                }
            }
            _ => self.gp_command(&id, &f, now),
        }
    }

    fn gp_report(&mut self, id: &str, f: &GpFrame, now: Millis) {
        let Ok(recs) = generic::decode_report(f.command_id, &f.payload) else {
            if let Some(d) = self.devices.get_mut(id) {
                d.diag.count("gp_malformed_report");
                d.diag.gp_data.insert(f.command_id, crate::expr::hex(&f.payload));
            }
            return;
        };
        let mut new_attr = false;
        if let Some(d) = self.devices.get_mut(id) {
            for (c, a, m, v) in &recs {
                let known = d.attrs.map.contains_key(&(GP_ENDPOINT, *c, *a, *m));
                d.attrs.set(GP_ENDPOINT, *c, *a, *m, Some(v.value.clone()));
                new_attr |= !known && m.is_none();
            }
        }
        if new_attr {
            self.resolve_device(id, now);
        }
        let Some(table) = self.dispatch.get(id) else { return };
        let mut work = Vec::new();
        let mut unbound = Vec::new();
        for (c, a, m, v) in &recs {
            match table.get(&DKey::GpAttr(*c, *a, *m)) {
                Some(list) => work.extend(list.iter().map(|&(i, p)| (i, p, crate::values::from_zcl(&v.value)))),
                None => unbound.push(format!("0x{c:04x}/0x{a:04x}{}={:?}", m.map_or(String::new(), |m| format!("@{m:04x}")), v.value)),
            }
        }
        if let Some(d) = self.devices.get_mut(id) {
            if !unbound.is_empty() {
                d.diag.gp_data.insert(f.command_id, unbound.join(" "));
            }
        }
        // Every accepted report refreshes the states (D16): apply_raw
        // publishes each touched entity even when the value is unchanged.
        self.apply_raw(id, work, now);
    }

    fn gp_command(&mut self, id: &str, f: &GpFrame, now: Millis) {
        // Remember the command (the generic tier grows its event types).
        let new_cmd = self.devices.get_mut(id).is_some_and(|d| {
            let k = (GP_ENDPOINT, generic::SEEN_COMMANDS, u16::from(f.command_id), None);
            let known_list = d.fp.gp.as_ref().is_some_and(|g| g.commands.contains(&f.command_id));
            let new = !d.attrs.map.contains_key(&k) && !known_list;
            d.attrs.map.insert(k, crate::device::AttrEntry { support: crate::generic::Support::Supported, value: None });
            new && generic::command_event(f.command_id).is_some()
        });
        if new_cmd {
            self.resolve_device(id, now);
        }
        let d = &self.devices[id];
        let work: Vec<_> = self
            .dispatch
            .get(id)
            .and_then(|t| t.get(&DKey::GpCmd(f.command_id)))
            .map(|l| l.iter().map(|&(i, p)| (i, p, Value::Bytes(f.payload.clone()))).collect())
            .unwrap_or_default();
        let mut fired = Vec::new();
        let contacts_before = d.gp.as_ref().map_or(0, |g| g.contacts);
        {
            let env = DevEnv { dev: d, now, tz: self.cfg.tz_offset_s };
            for (i, e) in d.entities.iter().enumerate() {
                if e.orphaned {
                    continue;
                }
                // Generic per-contact entities (0x69/0x6A with switch information).
                if let Some(bit) = e.r.def.capabilities.get("contact_bit").and_then(serde_json::Value::as_u64) {
                    if matches!(f.command_id, 0x69 | 0x6A) {
                        let now_bits = f.payload.first().copied().unwrap_or(0);
                        let (was, is) = ((contacts_before >> bit) & 1 == 1, (now_bits >> bit) & 1 == 1);
                        if is && !was {
                            fired.push((i, "press".to_string(), serde_json::Map::new()));
                        } else if !is && was {
                            fired.push((i, "release".to_string(), serde_json::Map::new()));
                        }
                    }
                    continue;
                }
                for t in &e.r.def.triggers {
                    if t.source != TriggerSource::GpCmd(f.command_id) {
                        continue;
                    }
                    if let Some(ev) = eval_trigger(t, &f.payload, e.state.last_event.as_deref(), &env) {
                        fired.push((i, ev.0, ev.1));
                        break;
                    }
                }
            }
        }
        if matches!(f.command_id, 0x69 | 0x6A) {
            if let Some(g) = self.devices.get_mut(id).and_then(|d| d.gp.as_mut()) {
                g.contacts = f.payload.first().copied().unwrap_or(0);
            }
        }
        if work.is_empty() && fired.is_empty() && matches!(f.command_id, 0xB0..=0xBF) {
            if let Some(d) = self.devices.get_mut(id) {
                d.diag.gp_data.insert(f.command_id, crate::expr::hex(&f.payload));
            }
        }
        self.apply_raw(id, work, now);
        // Hub-side virtual bindings (13.8 b) run on passed-through frames.
        // Events fire either way (a binding never swallows an event).
        if f.result == frame_result::PASSED_THROUGH {
            self.gp_execute_bindings(f, now);
        }
        self.gp_last_frame = Some((f.gpd_id, f.command_id, f.payload.clone()));
        for (i, et, attrs) in fired {
            self.fire_event(id, i, et, attrs, now);
        }
        self.gp_last_frame = None;
    }

    fn remove_gp_device_local(&mut self, id: &str) -> Result<(), ApiError> {
        let d = self.devices.remove(id).ok_or_else(|| ApiError::new("not_found", id.to_string()))?;
        for e in &d.entities {
            self.emit(HubEvent::EntityRemoved(e.uid.clone()));
        }
        self.dispatch.remove(id);
        self.emit(HubEvent::DeviceRemoved(id.to_string()));
        Ok(())
    }

    /// Removes a GPD: its on-device bindings first, then the Sink Table
    /// entry (`gp/devices/remove`).
    pub(crate) fn gp_remove(&mut self, gpd_id: GpdId, send_remove: bool, now: Millis) -> Result<(), ApiError> {
        let ids: Vec<u32> = self.gp.bindings.iter().filter(|b| b.gpd == gpd_id).map(|b| b.id).collect();
        for b in ids {
            let _ = self.delete_binding(b, now);
        }
        self.transport
            .gp_request(GpRequest::SinkRemove { gpd_id, flags: u8::from(send_remove) })
            .map_err(|e| ApiError::new("busy", e.to_string()))?;
        Ok(())
    }

    /// `gp/commissioning/open`.
    pub fn gp_commissioning_open(&mut self, duration_s: u16, involve_proxies: bool, exit_on_first_pairing: bool) -> Result<(), ApiError> {
        if duration_s > 600 {
            return Err(ApiError::new("invalid_value", "duration_s must be ≤ 600"));
        }
        let flags = u8::from(involve_proxies) | (u8::from(exit_on_first_pairing) << 1);
        self.transport
            .gp_request(GpRequest::SinkCommissioning { action: 1, flags, window_s: duration_s })
            .map_err(|e| ApiError::new("busy", e.to_string()))?;
        Ok(())
    }

    /// `gp/commissioning/close`.
    pub fn gp_commissioning_close(&mut self) -> Result<(), ApiError> {
        self.transport
            .gp_request(GpRequest::SinkCommissioning { action: 0, flags: 0, window_s: 0 })
            .map_err(|e| ApiError::new("busy", e.to_string()))?;
        Ok(())
    }
}
