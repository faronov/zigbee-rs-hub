//! Green Power (spec 13): identity, commissioning, the generic GP tier,
//! GP definitions and GP bindings (D23). The NCP runs the sink; the hub
//! turns `EVT_GP_*` into devices, entity updates, events and bindings.

pub mod bindings;
pub mod generic;

/// Owned mirrors of the ZRHP 7.11 GP records, events and requests; they
/// live in the coordinator boundary crate ([`hub_coord::gp`]).
pub mod types {
    pub use hub_coord::gp::*;
}

use std::collections::{BTreeMap, VecDeque};

use serde_json::{Value as J, json};

use crate::device::{Device, DeviceKind, GpState, Millis, ts};
use crate::expr::Value;
use crate::fingerprint::{EndpointFp, GpFingerprint};
use crate::hub::control::{AnswerTo, GpRemoved, Then};
use crate::hub::{ApiError, DKey, DevEnv, Hub, HubEvent, eval_trigger, set_value};
use crate::model::{Binding, DeviceField, TriggerSource};
use crate::transport::{Request, ZigbeeTransport};
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

/// GPDs in bidirectional commissioning kept for `gp/commissioning/status`.
pub const MAX_PENDING: usize = 8;
/// Recent commissioning failures kept for `gp/commissioning/status`.
pub const MAX_FAILED: usize = 8;
/// A pending GPD without a commissioning frame for this long is dropped.
pub const PENDING_TTL_MS: Millis = 60_000;

/// A GPD in bidirectional commissioning (`gp/pending`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpPending {
    /// GPD.
    pub gpd: GpdId,
    /// Last commissioning command (0xE0 / 0xE3).
    pub command_id: u8,
    /// Last `EVT_GP_FRAME` result.
    pub result: u8,
    /// First commissioning frame.
    pub since: Millis,
    /// Last commissioning frame.
    pub last: Millis,
}

/// A commissioning failure (`gp/failed`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpFailure {
    /// GPD.
    pub gpd: GpdId,
    /// `SinkDrop` reason code.
    pub reason: u8,
    /// When.
    pub at: Millis,
}

/// The sink's commissioning window as the hub knows it (its own requests
/// and `EVT_GP_COMMISSIONING`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpWindow {
    /// Open.
    pub open: bool,
    /// Closes at (none: until closed).
    pub deadline: Option<Millis>,
    /// Proxies involved (last request).
    pub involve_proxies: bool,
    /// Exit on the first pairing (last request).
    pub exit_on_first_pairing: bool,
    /// Last exit reason: 0 none, 1 explicit, 2 first pairing, 3 timeout.
    pub exit_reason: u8,
}

impl Default for GpWindow {
    fn default() -> Self {
        Self {
            open: false,
            deadline: None,
            involve_proxies: true,
            exit_on_first_pairing: false,
            exit_reason: 0,
        }
    }
}

/// Sink configuration last written with `GP_SINK_CONFIG` (`gp/config`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpSinkSettings {
    /// `gpsSecurityLevel`.
    pub security_level: u8,
    /// Communication mode.
    pub communication_mode: u8,
    /// Written through the API since the hub started (else the default).
    pub from_api: bool,
}

impl Default for GpSinkSettings {
    fn default() -> Self {
        Self {
            security_level: 0,
            communication_mode: 2,
            from_api: false,
        }
    }
}

fn exit_reason_name(r: u8) -> &'static str {
    match r {
        1 => "explicit",
        2 => "first_pairing",
        3 => "timeout",
        _ => "none",
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
    /// Remote answers that came before their request was accepted.
    pub early: VecDeque<GpEvent>,
    /// Next binding ID.
    pub next_id: u32,
    /// Commissioning window.
    pub window: GpWindow,
    /// GPDs in bidirectional commissioning (≤ [`MAX_PENDING`]).
    pub pending: VecDeque<GpPending>,
    /// Recent failures, oldest first (≤ [`MAX_FAILED`]).
    pub failed: VecDeque<GpFailure>,
    /// Sink configuration.
    pub sink: GpSinkSettings,
}

fn fp_from_payload(
    gpd_id: GpdId,
    device_id: u8,
    payload: Option<&[u8]>,
    level: u8,
) -> GpFingerprint {
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
    if let Some(cp) =
        payload.and_then(|p| zigbee_gp::commissioning::CommissioningPayload::decode(p).ok())
    {
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
        let Some(ev) = self.gp_early(ev) else {
            return;
        };
        match ev {
            GpEvent::Frame(f) => self.gp_frame(f, now),
            GpEvent::Commissioning {
                state,
                window_s,
                exit_reason,
            } => {
                let w = &mut self.gp.window;
                w.open = state == 1;
                if w.open {
                    w.deadline = (window_s != 0 && window_s != 0xFFFF)
                        .then(|| now + Millis::from(window_s) * 1000);
                    w.exit_reason = 0;
                } else {
                    w.deadline = None;
                    w.exit_reason = exit_reason;
                }
                self.gp_window_event(now);
            }
            GpEvent::Commissioned {
                gpd_id,
                device_id,
                flags,
                record,
            } => {
                self.gp.pending.retain(|p| p.gpd != gpd_id);
                self.gp_commissioned(gpd_id, device_id, flags, record, now);
                if self.gp.window.open && self.gp.window.exit_on_first_pairing {
                    // The sink leaves on the first pairing (an NCP need not
                    // report it).
                    self.gp_window_closed(2, now);
                }
            }
            GpEvent::CommissioningFailed { gpd_id, reason } => {
                self.gp.pending.retain(|p| p.gpd != gpd_id);
                if self.gp.failed.len() >= MAX_FAILED {
                    self.gp.failed.pop_front();
                }
                self.gp.failed.push_back(GpFailure {
                    gpd: gpd_id,
                    reason,
                    at: now,
                });
                self.emit(HubEvent::Gp {
                    kind: "gp/failed".into(),
                    body: json!({"gpd": gpd_id.device_id(), "reason": sink_drop_reason(reason), "reason_code": reason, "time": ts(now)}),
                });
            }
            GpEvent::Decommissioned { gpd_id } => {
                let id = gpd_id.device_id();
                if self.devices.contains_key(&id) {
                    let _ = self.remove_gp_device_local(&id);
                }
            }
            GpEvent::RemoteStatus {
                src_nwk,
                zcl_tsn,
                command_id,
                zcl_status,
            } => self.gp_remote_status(src_nwk, zcl_tsn, command_id, zcl_status, now),
            GpEvent::RemoteSinkTable {
                src_nwk,
                zcl_tsn,
                status,
                records,
            } => self.gp_remote_sink_table(src_nwk, zcl_tsn, status, &records, now),
            GpEvent::RemoteTranslationTable {
                src_nwk,
                zcl_tsn,
                status,
                records,
            } => self.gp_remote_translation_table(src_nwk, zcl_tsn, status, &records, now),
        }
    }

    fn gp_commissioned(
        &mut self,
        gpd_id: GpdId,
        device_id: u8,
        flags: u8,
        record: GpSinkRecord,
        now: Millis,
    ) {
        let payload = self
            .gp
            .payloads
            .remove(&gpd_id)
            .filter(|(t, _)| now.saturating_sub(*t) <= 30_000)
            .map(|(_, p)| p);
        let id = gpd_id.device_id();
        let fresh = !self.devices.contains_key(&id);
        let level = record.security_level;
        let mut g = fp_from_payload(gpd_id, device_id, payload.as_deref(), level);
        if payload.is_none() {
            if let Some(old) = self.devices.get(&id).and_then(|d| d.fp.gp.clone()) {
                // Re-commissioning without a payload keeps the known identity.
                g = GpFingerprint {
                    security_level: level,
                    ..old
                };
            }
        }
        let d = self
            .devices
            .entry(id.clone())
            .or_insert_with(|| Device::new(id.clone(), DeviceKind::Gp { gpd_id }, now));
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
        d.gp = Some(GpState {
            frame_counter: record.frame_counter,
            security_level: level,
            sink: Some(record),
            contacts: 0,
            recent: Default::default(),
        });
        if fresh {
            self.emit(HubEvent::DeviceAdded(id.clone()));
        }
        self.resolve_device(&id, now);
        self.gp_update_warnings(&id);
        self.gp_device_fields(&id, now);
        let rec = self.devices[&id].record();
        let layers = serde_json::to_value(&self.devices[&id].resolution.layers).unwrap_or_default();
        self.emit(HubEvent::Gp {
            kind: "gp/commissioned".into(),
            body: json!({"device": rec, "bidirectional": flags & 1 != 0, "layers": layers}),
        });
    }

    fn gp_update_warnings(&mut self, id: &str) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        let level = d.gp.as_ref().map_or(0, |g| g.security_level);
        let min = d
            .resolution
            .gp
            .as_ref()
            .and_then(|g| g.security_min)
            .unwrap_or(0);
        d.warnings.retain(|w| w != "gp_unsecured");
        if level == 0 || level < min {
            d.warnings.push("gp_unsecured".into());
        }
    }

    fn gp_device_fields(&mut self, id: &str, now: Millis) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        let Some(g) = d.gp.clone() else { return };
        let mut touched = Vec::new();
        for i in 0..d.entities.len() {
            let v = match d.entities[i].r.def.value.binding {
                Binding::DeviceField(DeviceField::FrameCounter) => {
                    Value::Int(i64::from(g.frame_counter))
                }
                Binding::DeviceField(DeviceField::SecurityLevel) => {
                    Value::Int(i64::from(g.security_level))
                }
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
            if f.command_id == 0xE0
                && matches!(
                    f.result,
                    frame_result::COMMISSIONED | frame_result::COMMISSIONING_REPLY_SENT
                )
            {
                self.gp
                    .payloads
                    .retain(|_, (t, _)| now.saturating_sub(*t) <= 30_000);
                self.gp.payloads.insert(f.gpd_id, (now, f.payload.clone()));
            }
            if matches!(
                f.result,
                frame_result::COMMISSIONING_REPLY_SENT | frame_result::COMMISSIONING_STEP
            ) {
                self.gp_pending(&f, now);
                self.emit(HubEvent::Gp {
                    kind: "gp/pending".into(),
                    body: json!({"gpd": f.gpd_id.device_id(), "command_id": f.command_id, "result": f.result, "time": ts(now)}),
                });
            }
            return;
        }
        if !matches!(
            f.result,
            frame_result::TRANSLATED | frame_result::PASSED_THROUGH
        ) {
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
            let g = d.gp.get_or_insert_with(|| GpState {
                sink: None,
                frame_counter: 0,
                security_level: 0,
                contacts: 0,
                recent: Default::default(),
            });
            g.recent.retain(|(t, _, _)| now.saturating_sub(*t) < 2_000);
            if g.recent
                .iter()
                .any(|(_, c, cmd)| *c == f.frame_counter && *cmd == f.command_id)
            {
                d.diag.count("gp_duplicate");
                return;
            }
            g.recent.push_back((now, f.frame_counter, f.command_id));
            g.frame_counter = g.frame_counter.max(f.frame_counter);
            g.security_level = f.security_level;
            if d.resolution
                .gp
                .as_ref()
                .is_some_and(|x| x.ignore_commands.contains(&f.command_id))
            {
                return;
            }
        }
        self.seen(&id, Some(f.lqi), Some(f.rssi), now);
        self.gp_device_fields(&id, now);
        match f.command_id {
            0xA0..=0xA3 => self.gp_report(&id, &f, now),
            0xA8 => {
                if let Some(d) = self.devices.get_mut(&id) {
                    d.diag
                        .gp_data
                        .insert(f.command_id, crate::expr::hex(&f.payload));
                }
            }
            _ => self.gp_command(&id, &f, now),
        }
    }

    fn gp_report(&mut self, id: &str, f: &GpFrame, now: Millis) {
        let Ok(recs) = generic::decode_report(f.command_id, &f.payload) else {
            if let Some(d) = self.devices.get_mut(id) {
                d.diag.count("gp_malformed_report");
                d.diag
                    .gp_data
                    .insert(f.command_id, crate::expr::hex(&f.payload));
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
        let Some(table) = self.dispatch.get(id) else {
            return;
        };
        let mut work = Vec::new();
        let mut unbound = Vec::new();
        for (c, a, m, v) in &recs {
            match table.get(&DKey::GpAttr(*c, *a, *m)) {
                Some(list) => work.extend(
                    list.iter()
                        .map(|&(i, p)| (i, p, crate::values::from_zcl(&v.value))),
                ),
                None => unbound.push(format!(
                    "0x{c:04x}/0x{a:04x}{}={:?}",
                    m.map_or(String::new(), |m| format!("@{m:04x}")),
                    v.value
                )),
            }
        }
        if let Some(d) = self.devices.get_mut(id) {
            if !unbound.is_empty() {
                d.diag.gp_data.insert(f.command_id, unbound.join(" "));
            }
        }
        // Every accepted report refreshes the states (D16): apply_raw
        // publishes each touched entity even when the value is unchanged.
        self.apply_raw(id, work, true, now);
    }

    fn gp_command(&mut self, id: &str, f: &GpFrame, now: Millis) {
        // Remember the command (the generic tier grows its event types).
        let new_cmd = self.devices.get_mut(id).is_some_and(|d| {
            let k = (
                GP_ENDPOINT,
                generic::SEEN_COMMANDS,
                u16::from(f.command_id),
                None,
            );
            let known_list =
                d.fp.gp
                    .as_ref()
                    .is_some_and(|g| g.commands.contains(&f.command_id));
            let new = !d.attrs.map.contains_key(&k) && !known_list;
            d.attrs.map.insert(
                k,
                crate::device::AttrEntry {
                    support: crate::generic::Support::Supported,
                    value: None,
                },
            );
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
            .map(|l| {
                l.iter()
                    .map(|&(i, p)| (i, p, Value::Bytes(f.payload.clone())))
                    .collect()
            })
            .unwrap_or_default();
        let mut fired = Vec::new();
        let contacts_before = d.gp.as_ref().map_or(0, |g| g.contacts);
        {
            let env = DevEnv {
                dev: d,
                now,
                tz: self.cfg.tz_offset_s,
            };
            for (i, e) in d.entities.iter().enumerate() {
                if e.orphaned {
                    continue;
                }
                // Generic per-contact entities (0x69/0x6A with switch information).
                if let Some(bit) =
                    e.r.def
                        .capabilities
                        .get("contact_bit")
                        .and_then(serde_json::Value::as_u64)
                {
                    if matches!(f.command_id, 0x69 | 0x6A) {
                        let now_bits = f.payload.first().copied().unwrap_or(0);
                        let (was, is) = (
                            (contacts_before >> bit) & 1 == 1,
                            (now_bits >> bit) & 1 == 1,
                        );
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
                    if let Some(ev) =
                        eval_trigger(t, &f.payload, e.state.last_event.as_deref(), &env)
                    {
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
                d.diag
                    .gp_data
                    .insert(f.command_id, crate::expr::hex(&f.payload));
            }
        }
        self.apply_raw(id, work, true, now);
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
        let d = self
            .devices
            .remove(id)
            .ok_or_else(|| ApiError::new("not_found", id.to_string()))?;
        for e in &d.entities {
            self.emit(HubEvent::EntityRemoved(e.uid.clone()));
        }
        self.dispatch.remove(id);
        self.emit(HubEvent::DeviceRemoved(id.to_string()));
        Ok(())
    }

    /// Removes a GPD: its on-device bindings first, then the Sink Table
    /// entry, then (once the NCP took that) its record as `removed` says.
    pub(crate) fn gp_remove(
        &mut self,
        gpd_id: GpdId,
        send_remove: bool,
        removed: GpRemoved,
        now: Millis,
    ) -> Result<J, ApiError> {
        let ticket = self.answer_ticket();
        let ids: Vec<u32> = self
            .gp
            .bindings
            .iter()
            .filter(|b| b.gpd == gpd_id)
            .map(|b| b.id)
            .collect();
        for b in ids {
            let _ = self.delete_binding(b, now);
        }
        let req = GpRequest::SinkRemove {
            gpd_id,
            flags: u8::from(send_remove),
        };
        self.control(Request::Gp(req), Then::GpRemove(removed), Some(ticket), now);
        self.answer(ticket)
    }

    /// `gp/commissioning/open`.
    pub fn gp_commissioning_open(
        &mut self,
        duration_s: u16,
        involve_proxies: bool,
        exit_on_first_pairing: bool,
        now: Millis,
    ) -> Result<(), ApiError> {
        if duration_s > 600 {
            return Err(ApiError::new("invalid_value", "duration_s must be ≤ 600"));
        }
        let ticket = self.answer_ticket();
        self.gp_open(
            duration_s,
            involve_proxies,
            exit_on_first_pairing,
            Some(ticket),
            now,
        );
        self.answer(ticket).map(drop)
    }

    /// `GP_SINK_COMMISSIONING` open; `answer` follows from it.
    pub(crate) fn gp_open(
        &mut self,
        duration_s: u16,
        involve_proxies: bool,
        exit_on_first_pairing: bool,
        answer: Option<AnswerTo>,
        now: Millis,
    ) {
        let flags = u8::from(involve_proxies) | (u8::from(exit_on_first_pairing) << 1);
        let req = GpRequest::SinkCommissioning {
            action: 1,
            flags,
            window_s: duration_s,
        };
        let then = Then::GpOpen {
            duration_s,
            involve_proxies,
            exit_on_first_pairing,
            at: now,
        };
        self.control(Request::Gp(req), then, answer, now);
    }

    /// The NCP opened the commissioning window.
    pub(crate) fn gp_window_opened(
        &mut self,
        duration_s: u16,
        involve_proxies: bool,
        exit_on_first_pairing: bool,
        now: Millis,
    ) {
        // The hub keeps its own deadline: an NCP image need not report
        // `EVT_GP_COMMISSIONING` (as for permit join, 14.2).
        self.gp.window = GpWindow {
            open: true,
            deadline: (duration_s > 0).then(|| now + Millis::from(duration_s) * 1000),
            involve_proxies,
            exit_on_first_pairing,
            exit_reason: 0,
        };
        self.gp_window_event(now);
    }

    /// `gp/commissioning/close`.
    pub fn gp_commissioning_close(&mut self, now: Millis) -> Result<(), ApiError> {
        let ticket = self.answer_ticket();
        let req = GpRequest::SinkCommissioning {
            action: 0,
            flags: 0,
            window_s: 0,
        };
        self.control(Request::Gp(req), Then::GpClose, Some(ticket), now);
        self.answer(ticket).map(drop)
    }

    pub(crate) fn gp_window_closed(&mut self, reason: u8, now: Millis) {
        let w = &mut self.gp.window;
        w.open = false;
        w.deadline = None;
        w.exit_reason = reason;
        self.gp_window_event(now);
    }

    fn gp_window_json(&self, now: Millis) -> J {
        let w = &self.gp.window;
        let remaining = match (w.open, w.deadline) {
            (false, _) => json!(0),
            (true, Some(d)) => json!(d.saturating_sub(now).div_ceil(1000)),
            (true, None) => J::Null,
        };
        json!({
            "open": w.open,
            "state": if w.open { "open" } else { "closed" },
            "remaining_s": remaining,
            "involve_proxies": w.involve_proxies,
            "exit_on_first_pairing": w.exit_on_first_pairing,
            "exit_reason": exit_reason_name(w.exit_reason),
        })
    }

    fn gp_window_event(&mut self, now: Millis) {
        let body = self.gp_window_json(now);
        self.emit(HubEvent::Gp {
            kind: "gp/commissioning".into(),
            body,
        });
    }

    fn gp_pending(&mut self, f: &GpFrame, now: Millis) {
        let p = &mut self.gp.pending;
        p.retain(|x| now.saturating_sub(x.last) <= PENDING_TTL_MS);
        if let Some(x) = p.iter_mut().find(|x| x.gpd == f.gpd_id) {
            x.command_id = f.command_id;
            x.result = f.result;
            x.last = now;
            return;
        }
        if p.len() >= MAX_PENDING {
            p.pop_front();
        }
        p.push_back(GpPending {
            gpd: f.gpd_id,
            command_id: f.command_id,
            result: f.result,
            since: now,
            last: now,
        });
    }

    /// Window expiry and pending clean-up (called from `tick`).
    pub(crate) fn gp_window_tick(&mut self, now: Millis) {
        if self.gp.window.open && self.gp.window.deadline.is_some_and(|d| now >= d) {
            self.gp_window_closed(3, now);
        }
        self.gp
            .pending
            .retain(|x| now.saturating_sub(x.last) <= PENDING_TTL_MS);
    }

    /// `gp/commissioning/status`: the window, the GPDs in bidirectional
    /// commissioning and the recent failures (newest first).
    pub fn gp_commissioning_status(&self, now: Millis) -> J {
        let mut r = self.gp_window_json(now);
        r["pending"] = self
            .gp
            .pending
            .iter()
            .filter(|x| now.saturating_sub(x.last) <= PENDING_TTL_MS)
            .map(|x| {
                json!({"gpd": x.gpd.device_id(), "command_id": x.command_id, "result": x.result, "since": ts(x.since), "time": ts(x.last)})
            })
            .collect();
        r["failed"] = self
            .gp
            .failed
            .iter()
            .rev()
            .map(|x| {
                json!({"gpd": x.gpd.device_id(), "reason": sink_drop_reason(x.reason), "reason_code": x.reason, "time": ts(x.at)})
            })
            .collect();
        r
    }

    /// `gp/config/get` (and the result of `gp/config`).
    pub fn gp_config(&self) -> J {
        let s = &self.gp.sink;
        json!({
            "security_level": s.security_level,
            "communication_mode": s.communication_mode,
            "group": self.cfg.gp_group,
            "source": if s.from_api { "api" } else { "default" },
            "warning": (s.security_level > 0).then_some("GPDs below this level (e.g. FlowerX GP) are rejected"),
        })
    }

    /// `gp/config`: writes `GP_SINK_CONFIG`; an absent field keeps its
    /// current value.
    pub fn gp_set_config(
        &mut self,
        security_level: Option<u8>,
        communication_mode: Option<u8>,
        now: Millis,
    ) -> Result<J, ApiError> {
        let level = security_level.unwrap_or(self.gp.sink.security_level);
        let mode = communication_mode.unwrap_or(self.gp.sink.communication_mode);
        if level > 3 {
            return Err(ApiError::new("invalid_value", "security_level must be 0–3"));
        }
        if mode > 3 {
            return Err(ApiError::new(
                "invalid_value",
                "communication_mode must be 0–3",
            ));
        }
        let req = GpRequest::SinkConfig {
            mode: 1,
            communication_mode: mode,
            group: self.cfg.gp_group,
            exit_mode: 0x02,
            window_s: 180,
            security_level: level,
            groupcast_radius: 0,
        };
        let s = GpSinkSettings {
            security_level: level,
            communication_mode: mode,
            from_api: true,
        };
        let ticket = self.answer_ticket();
        self.control(Request::Gp(req), Then::GpConfig(s), Some(ticket), now);
        self.answer(ticket)
    }

    /// The sink settings written: kept, announced; the `gp/config` result.
    pub(crate) fn gp_sink_set(&mut self, s: GpSinkSettings) -> J {
        self.gp.sink = s;
        let body = self.gp_config();
        self.emit(HubEvent::Gp {
            kind: "gp/config".into(),
            body: body.clone(),
        });
        body
    }
}
