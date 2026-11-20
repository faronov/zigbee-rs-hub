//! Entity commands (spec 4.5, 14.3): HA service names and parameters →
//! ZCL commands, attribute writes, Tuya DP writes or hub options.

use std::collections::BTreeMap;

use serde_json::{Value as J, json};

use crate::device::{Device, Millis};
use crate::expr::{ExprCtx, Value};
use crate::hub::{ApiError, DevEnv, Hub, HubEvent, Purpose, device_of_uid, set_value};
use crate::model::*;
use crate::transport::ZigbeeTransport;
use crate::tuya::{self, DpRecord};
use crate::values;
use crate::zcl::frame::{self as zf, global};
use crate::zcl::{FrameType, TypedValue, ZclFrame, ZclValue, cluster as cl};

/// Immediate answer to `entities/command`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CommandAck {
    /// Ticket; the final result follows as [`HubEvent::CommandResult`].
    pub ticket: u64,
    /// `pending`, `queued` (sleepy device) or `ok` (nothing to wait for).
    pub status: &'static str,
}

#[derive(Debug, Clone)]
enum Action {
    Write { ep: u8, cluster: u16, mfr: Option<u16>, attr: u16, value: TypedValue },
    Cmd { ep: u8, cluster: u16, mfr: Option<u16>, id: u8, payload: Vec<u8> },
    Tuya(DpRecord),
    Hub { key: String, value: Value },
}

struct Plan {
    actions: Vec<Action>,
    optimistic: Vec<(Option<String>, Value)>,
    reset_after_s: Option<u64>,
}

fn bad(m: impl Into<String>) -> ApiError {
    ApiError::new("invalid_value", m)
}

fn param<'a>(p: &'a J, k: &str) -> Option<&'a J> {
    p.get(k).filter(|v| !v.is_null())
}

fn num_param(p: &J, k: &str) -> Result<Option<f64>, ApiError> {
    match param(p, k) {
        None => Ok(None),
        Some(v) => v.as_f64().map(Some).ok_or_else(|| bad(format!("{k} must be a number"))),
    }
}

fn transition(p: &J) -> Result<u16, ApiError> {
    Ok(num_param(p, "transition")?.map_or(0, |t| (t * 10.0).round().clamp(0.0, 65534.0) as u16))
}

fn part<'a>(e: &'a EntityDef, n: &str) -> Option<&'a ValueDef> {
    e.parts.iter().find(|(k, _)| k == n).map(|(_, v)| v)
}

fn zcl_of(v: &ValueDef) -> Option<&ZclBinding> {
    match &v.binding {
        Binding::Zcl(z) => Some(z),
        _ => None,
    }
}

impl<T: ZigbeeTransport> Hub<T> {
    /// Value write through a binding (attribute, DP, hub option).
    fn write_action(&self, d: &Device, e: &EntityDef, ep: u8, vd: &ValueDef, is_bool: bool, v: &Value, now: Millis) -> Result<Action, ApiError> {
        if !e.access_of(vd).write {
            return Err(ApiError::new("unsupported", format!("{} is read-only", e.key)));
        }
        let env = DevEnv { dev: d, now, tz: self.cfg.tz_offset_s };
        let raw = values::encode(vd, is_bool, v, &env).map_err(|x| bad(x.0))?;
        match &vd.binding {
            Binding::Zcl(z) if z.write != ZclWrite::ReadOnly => {
                let zv = values::to_zcl(z.ty, &raw).map_err(bad)?;
                Ok(Action::Write { ep, cluster: z.cluster, mfr: z.mfr, attr: z.attr, value: TypedValue::new(z.ty, zv) })
            }
            Binding::Tuya { dp, ty } => {
                let data = tuya::dp_encode(*ty, &raw).map_err(bad)?;
                Ok(Action::Tuya(DpRecord { dp: *dp, ty: *ty as u8, data }))
            }
            Binding::Hub { .. } => Ok(Action::Hub { key: e.key.clone(), value: v.clone() }),
            _ => Err(ApiError::new("unsupported", format!("{} cannot be written", e.key))),
        }
    }

    fn part_write(&self, d: &Device, e: &EntityDef, ep: u8, name: &str, v: Value, now: Millis, plan: &mut Plan) -> Result<(), ApiError> {
        let vd = part(e, name).ok_or_else(|| ApiError::new("unsupported", format!("{} has no {name}", e.key)))?;
        let a = self.write_action(d, e, ep, vd, name == "on_off", &v, now)?;
        plan.actions.push(a);
        plan.optimistic.push((Some(name.to_string()), v));
        Ok(())
    }

    fn encode_part(&self, d: &Device, e: &EntityDef, name: &str, v: &Value, now: Millis) -> Result<i64, ApiError> {
        let vd = part(e, name).ok_or_else(|| ApiError::new("unsupported", format!("{} has no {name}", e.key)))?;
        let env = DevEnv { dev: d, now, tz: self.cfg.tz_offset_s };
        let raw = values::encode(vd, false, v, &env).map_err(|x| bad(x.0))?;
        raw.as_i64().ok_or_else(|| bad(format!("{name}: integer expected")))
    }

    fn plan(&self, d: &Device, ent: &crate::device::Entity, command: &str, p: &J, now: Millis) -> Result<Plan, ApiError> {
        let e = &ent.r.def;
        let ep = ent.r.ep;
        let mut plan = Plan { actions: Vec::new(), optimistic: Vec::new(), reset_after_s: None };
        let unsupported = || ApiError::new("unsupported", format!("{command} is not a {} command", e.platform.as_str()));
        let cmd = |cluster: u16, id: u8, payload: Vec<u8>| Action::Cmd { ep, cluster, mfr: None, id, payload };
        match e.platform {
            Platform::Switch => {
                let on = match command {
                    "turn_on" => Some(true),
                    "turn_off" => Some(false),
                    "toggle" => None,
                    _ => return Err(unsupported()),
                };
                match zcl_of(&e.value).map(|z| z.write) {
                    Some(ZclWrite::OnOffCommands) => {
                        plan.actions.push(cmd(cl::ON_OFF, on.map_or(0x02, u8::from), Vec::new()));
                        plan.optimistic.push((None, on.map_or(Value::Null, Value::Bool)));
                    }
                    _ => {
                        let on = on.unwrap_or_else(|| ent.state.value.truthy() != Some(true));
                        plan.actions.push(self.write_action(d, e, ep, &e.value, true, &Value::Bool(on), now)?);
                        plan.optimistic.push((None, Value::Bool(on)));
                    }
                }
            }
            Platform::Number | Platform::Text | Platform::Select => {
                let (want, key) = match e.platform {
                    Platform::Select => ("select_option", "option"),
                    _ => ("set_value", "value"),
                };
                if command != want {
                    return Err(unsupported());
                }
                let v = Value::from_json(param(p, key).ok_or_else(|| bad(format!("missing {key}")))?);
                plan.actions.push(self.write_action(d, e, ep, &e.value, false, &v, now)?);
                plan.optimistic.push((None, v));
            }
            Platform::Button => {
                if command != "press" {
                    return Err(unsupported());
                }
                let env = DevEnv { dev: d, now, tz: self.cfg.tz_offset_s };
                match e.press.as_ref().ok_or_else(unsupported)? {
                    Press::Cmd { cluster, id, mfr, ep: pep, payload } => {
                        let payload = match payload {
                            None => Vec::new(),
                            Some(PressPayload::Bytes(b)) => b.clone(),
                            Some(PressPayload::Expr(x)) => match x.eval(&ExprCtx { x: Value::Null, payload: None, last: None, env: &env }) {
                                Value::Bytes(b) => b,
                                _ => return Err(bad("press payload expression failed")),
                            },
                        };
                        plan.actions.push(Action::Cmd { ep: pep.unwrap_or(ep), cluster: *cluster, mfr: *mfr, id: *id, payload });
                    }
                    Press::Write { cluster, attr, ty, value, mfr } => {
                        let v = values::to_zcl(*ty, value).map_err(bad)?;
                        plan.actions.push(Action::Write { ep, cluster: *cluster, mfr: *mfr, attr: *attr, value: TypedValue::new(*ty, v) });
                    }
                    Press::Tuya { dp, ty, value } => {
                        let data = tuya::dp_encode(*ty, value).map_err(bad)?;
                        plan.actions.push(Action::Tuya(DpRecord { dp: *dp, ty: *ty as u8, data }));
                    }
                }
            }
            Platform::Light => {
                let on_off = part(e, "on_off");
                let generic = on_off.and_then(zcl_of).is_some_and(|z| z.write == ZclWrite::OnOffCommands);
                let tr = transition(p)?;
                match command {
                    "turn_off" if generic => {
                        if tr > 0 && part(e, "brightness").is_some() {
                            let mut pl = vec![0];
                            pl.extend_from_slice(&tr.to_le_bytes());
                            plan.actions.push(cmd(cl::LEVEL, 0x04, pl));
                        } else {
                            plan.actions.push(cmd(cl::ON_OFF, 0x00, Vec::new()));
                        }
                        plan.optimistic.push((Some("on_off".into()), Value::Bool(false)));
                    }
                    "toggle" if generic => plan.actions.push(cmd(cl::ON_OFF, 0x02, Vec::new())),
                    "turn_on" if generic => {
                        let mut bri = num_param(p, "brightness")?;
                        if let Some(pct) = num_param(p, "brightness_pct")? {
                            bri = Some(pct * 255.0 / 100.0);
                        }
                        let has_color = ["color_temp_kelvin", "xy_color", "hs_color"].iter().any(|k| param(p, k).is_some());
                        if let Some(b) = bri {
                            if part(e, "brightness").is_none() {
                                return Err(ApiError::new("unsupported", "light has no brightness"));
                            }
                            let level = b.round().clamp(1.0, 254.0) as u8;
                            let mut pl = vec![level];
                            pl.extend_from_slice(&tr.to_le_bytes());
                            plan.actions.push(cmd(cl::LEVEL, 0x04, pl));
                            plan.optimistic.push((Some("brightness".into()), Value::Int(i64::from(level))));
                        } else if !has_color || ent.state.parts.get("on_off").and_then(Value::truthy) != Some(true) {
                            plan.actions.push(cmd(cl::ON_OFF, 0x01, Vec::new()));
                        }
                        plan.optimistic.push((Some("on_off".into()), Value::Bool(true)));
                        if let Some(k) = num_param(p, "color_temp_kelvin")? {
                            let m = self.encode_part(d, e, "color_temp", &Value::Float(k), now)?;
                            let mut pl = (m.clamp(1, 0xFEFF) as u16).to_le_bytes().to_vec();
                            pl.extend_from_slice(&tr.to_le_bytes());
                            plan.actions.push(cmd(cl::COLOR, 0x0A, pl));
                            plan.optimistic.push((Some("color_temp".into()), Value::Int(k.round() as i64)));
                        }
                        if let Some(xy) = param(p, "xy_color").and_then(J::as_array) {
                            let (x, y) = (xy.first().and_then(J::as_f64), xy.get(1).and_then(J::as_f64));
                            let (Some(x), Some(y)) = (x, y) else { return Err(bad("xy_color must be [x, y]")) };
                            let rx = self.encode_part(d, e, "color_x", &Value::Float(x), now)?;
                            let ry = self.encode_part(d, e, "color_y", &Value::Float(y), now)?;
                            let mut pl = (rx.clamp(0, 0xFEFF) as u16).to_le_bytes().to_vec();
                            pl.extend_from_slice(&(ry.clamp(0, 0xFEFF) as u16).to_le_bytes());
                            pl.extend_from_slice(&tr.to_le_bytes());
                            plan.actions.push(cmd(cl::COLOR, 0x07, pl));
                        }
                        if let Some(hs) = param(p, "hs_color").and_then(J::as_array) {
                            let (h, s) = (hs.first().and_then(J::as_f64), hs.get(1).and_then(J::as_f64));
                            let (Some(h), Some(s)) = (h, s) else { return Err(bad("hs_color must be [h, s]")) };
                            let rh = self.encode_part(d, e, "hue", &Value::Float(h), now)?;
                            let rs = self.encode_part(d, e, "saturation", &Value::Float(s), now)?;
                            let mut pl = vec![rh.clamp(0, 254) as u8, rs.clamp(0, 254) as u8];
                            pl.extend_from_slice(&tr.to_le_bytes());
                            plan.actions.push(cmd(cl::COLOR, 0x06, pl));
                        }
                    }
                    "turn_on" | "turn_off" | "toggle" => {
                        let on = match command {
                            "turn_on" => true,
                            "turn_off" => false,
                            _ => ent.state.parts.get("on_off").and_then(Value::truthy) != Some(true),
                        };
                        self.part_write(d, e, ep, "on_off", Value::Bool(on), now, &mut plan)?;
                        if let (true, Some(b)) = (on, num_param(p, "brightness")?) {
                            self.part_write(d, e, ep, "brightness", Value::Float(b), now, &mut plan)?;
                        }
                    }
                    _ => return Err(unsupported()),
                }
            }
            Platform::Cover => {
                let generic = part(e, "position").and_then(zcl_of).is_some_and(|z| z.write == ZclWrite::CoverLift);
                let pct = |k: &str| -> Result<f64, ApiError> { num_param(p, k)?.ok_or_else(|| bad(format!("missing {k}"))) };
                match (command, generic) {
                    ("open_cover", true) => plan.actions.push(cmd(cl::WINDOW_COVERING, 0x00, Vec::new())),
                    ("close_cover", true) => plan.actions.push(cmd(cl::WINDOW_COVERING, 0x01, Vec::new())),
                    ("stop_cover", true) => plan.actions.push(cmd(cl::WINDOW_COVERING, 0x02, Vec::new())),
                    ("set_cover_position", true) | ("set_cover_tilt_position", true) => {
                        let (k, n, id) = if command == "set_cover_position" { ("position", "position", 0x05) } else { ("tilt_position", "tilt", 0x08) };
                        let v = pct(k)?;
                        if !(0.0..=100.0).contains(&v) {
                            return Err(bad(format!("{k} must be 0..100")));
                        }
                        let raw = if e.invert { v.round() as i64 } else { self.encode_part(d, e, n, &Value::Float(v), now)? };
                        plan.actions.push(cmd(cl::WINDOW_COVERING, id, vec![raw.clamp(0, 100) as u8]));
                    }
                    ("open_cover" | "close_cover" | "stop_cover", false) => {
                        let label = &command[..command.len() - 6];
                        self.part_write(d, e, ep, "state", Value::Str(label.into()), now, &mut plan)?;
                        plan.optimistic.clear();
                    }
                    ("set_cover_position", false) => {
                        self.part_write(d, e, ep, "position", Value::Float(pct("position")?), now, &mut plan)?;
                        plan.optimistic.clear();
                    }
                    ("set_cover_tilt_position", false) => {
                        self.part_write(d, e, ep, "tilt", Value::Float(pct("tilt_position")?), now, &mut plan)?;
                        plan.optimistic.clear();
                    }
                    _ => return Err(unsupported()),
                }
            }
            Platform::Climate => match command {
                "set_hvac_mode" => {
                    let m = param(p, "hvac_mode").and_then(J::as_str).ok_or_else(|| bad("missing hvac_mode"))?;
                    self.part_write(d, e, ep, "hvac_mode", Value::Str(m.into()), now, &mut plan)?;
                }
                "set_temperature" => {
                    let t = num_param(p, "temperature")?.ok_or_else(|| bad("missing temperature"))?;
                    if let Some(m) = param(p, "hvac_mode").and_then(J::as_str) {
                        self.part_write(d, e, ep, "hvac_mode", Value::Str(m.into()), now, &mut plan)?;
                    }
                    self.part_write(d, e, ep, "target_temperature", Value::Float(t), now, &mut plan)?;
                }
                "set_preset_mode" => {
                    let m = param(p, "preset_mode").and_then(J::as_str).ok_or_else(|| bad("missing preset_mode"))?;
                    self.part_write(d, e, ep, "preset", Value::Str(m.into()), now, &mut plan)?;
                }
                "set_fan_mode" => {
                    let m = param(p, "fan_mode").and_then(J::as_str).ok_or_else(|| bad("missing fan_mode"))?;
                    self.part_write(d, e, ep, "fan_mode", Value::Str(m.into()), now, &mut plan)?;
                }
                "turn_off" => self.part_write(d, e, ep, "hvac_mode", Value::Str("off".into()), now, &mut plan)?,
                "turn_on" => {
                    let modes: Vec<String> = part(e, "hvac_mode").map(|v| v.transforms.map.iter().map(|(_, l)| l.clone()).collect()).unwrap_or_default();
                    let m = ["heat", "auto", "cool"].into_iter().find(|m| modes.iter().any(|x| x == m)).ok_or_else(unsupported)?;
                    self.part_write(d, e, ep, "hvac_mode", Value::Str(m.into()), now, &mut plan)?;
                }
                _ => return Err(unsupported()),
            },
            Platform::Lock => {
                let lock = match command {
                    "lock" => true,
                    "unlock" | "open" => false,
                    _ => return Err(unsupported()),
                };
                let st = part(e, "state").ok_or_else(unsupported)?;
                if zcl_of(st).is_some_and(|z| z.write == ZclWrite::LockCommands) {
                    plan.actions.push(cmd(cl::DOOR_LOCK, if lock { 0x00 } else { 0x01 }, vec![0x00]));
                } else {
                    self.part_write(d, e, ep, "state", Value::Str(if lock { "locked" } else { "unlocked" }.into()), now, &mut plan)?;
                    plan.optimistic.clear();
                }
            }
            Platform::Fan => {
                let label = match command {
                    "turn_off" => "off".to_string(),
                    "turn_on" | "set_percentage" => match (num_param(p, "percentage")?, param(p, "preset_mode").and_then(J::as_str)) {
                        (_, Some(pm)) => pm.to_string(),
                        (Some(x), _) if x <= 0.0 => "off".into(),
                        (Some(x), _) if x <= 34.0 => "low".into(),
                        (Some(x), _) if x <= 67.0 => "medium".into(),
                        (Some(_), _) => "high".into(),
                        (None, _) if command == "turn_on" => "on".into(),
                        (None, _) => return Err(bad("missing percentage")),
                    },
                    "set_preset_mode" => param(p, "preset_mode").and_then(J::as_str).ok_or_else(|| bad("missing preset_mode"))?.to_string(),
                    _ => return Err(unsupported()),
                };
                self.part_write(d, e, ep, "fan_mode", Value::Str(label), now, &mut plan)?;
            }
            Platform::Siren => {
                if !matches!(e.value.binding, Binding::IasWarning) {
                    let on = command == "turn_on";
                    self.part_write(d, e, ep, "on_off", Value::Bool(on), now, &mut plan)?;
                } else {
                    let (mode, dur) = match command {
                        "turn_on" => {
                            let tones = ["burglar", "fire", "emergency", "police_panic", "fire_panic", "emergency_panic"];
                            let mode = match param(p, "tone") {
                                None => 1,
                                Some(J::String(t)) => tones.iter().position(|x| x == t).ok_or_else(|| bad(format!("unknown tone {t}")))? as u8 + 1,
                                Some(J::Number(n)) => n.as_u64().filter(|n| (1..=6).contains(n)).ok_or_else(|| bad("tone 1..6"))? as u8,
                                Some(_) => return Err(bad("tone")),
                            };
                            (mode, num_param(p, "duration")?.unwrap_or(10.0).clamp(0.0, 65535.0) as u16)
                        }
                        "turn_off" => (0, 0),
                        _ => return Err(unsupported()),
                    };
                    let level = num_param(p, "volume_level")?.map_or(2, |v| (v * 3.0).round().clamp(0.0, 3.0) as u8);
                    let info = (mode << 4) | (u8::from(mode != 0) << 2) | level;
                    let mut pl = vec![info];
                    pl.extend_from_slice(&dur.to_le_bytes());
                    pl.extend_from_slice(&[0x00, 0x00]);
                    plan.actions.push(cmd(cl::IAS_WD, 0x00, pl));
                    plan.optimistic.push((None, Value::Bool(mode != 0)));
                    plan.reset_after_s = (mode != 0).then_some(u64::from(dur));
                }
            }
            Platform::Valve => match command {
                "open_valve" | "close_valve" => {
                    let open = command == "open_valve";
                    if part(e, "state").is_some() {
                        self.part_write(d, e, ep, "state", Value::Str(if open { "open" } else { "closed" }.into()), now, &mut plan)?;
                    } else {
                        self.part_write(d, e, ep, "on_off", Value::Bool(open), now, &mut plan)?;
                    }
                }
                "set_valve_position" => {
                    let v = num_param(p, "position")?.ok_or_else(|| bad("missing position"))?;
                    self.part_write(d, e, ep, "position", Value::Float(v), now, &mut plan)?;
                }
                _ => return Err(unsupported()),
            },
            _ => return Err(unsupported()),
        }
        Ok(plan)
    }

    /// `entities/command`.
    pub fn entity_command(&mut self, uid: &str, command: &str, params: &J, now: Millis) -> Result<CommandAck, ApiError> {
        let did = device_of_uid(uid).to_string();
        let d = self.devices.get(&did).ok_or_else(|| ApiError::new("not_found", format!("no entity {uid}")))?;
        let i = d.entity_index(uid).ok_or_else(|| ApiError::new("not_found", format!("no entity {uid}")))?;
        let ent = &d.entities[i];
        if ent.orphaned {
            return Err(ApiError::new("not_found", format!("{uid} is orphaned")));
        }
        if d.gp.is_some() {
            return Err(ApiError::new("unsupported", "GPD commands are not supported (unidirectional)"));
        }
        let plan = self.plan(d, ent, command, params, now)?;
        let queued = d.sleepy() && now > d.awake_until;
        let tuya_optimistic = d.resolution.tuya.as_ref().is_some_and(|t| t.optimistic);
        let ticket = self.new_ticket();
        // Group attribute writes per (ep, cluster, mfr); Tuya DPs into one frame.
        let mut writes: BTreeMap<(u8, u16, Option<u16>), Vec<(u16, TypedValue)>> = BTreeMap::new();
        let mut dps: Vec<DpRecord> = Vec::new();
        let mut cmds = Vec::new();
        let mut hub_vals = Vec::new();
        for a in plan.actions {
            match a {
                Action::Write { ep, cluster, mfr, attr, value } => writes.entry((ep, cluster, mfr)).or_default().push((attr, value)),
                Action::Tuya(r) => dps.push(r),
                Action::Cmd { .. } => cmds.push(a),
                Action::Hub { key, value } => hub_vals.push((key, value)),
            }
        }
        let zcl_optimistic: Vec<(String, Option<String>, Value)> =
            if dps.is_empty() || tuya_optimistic { plan.optimistic.iter().map(|(p, v)| (uid.to_string(), p.clone(), v.clone())).collect() } else { Vec::new() };
        let frames = (writes.len() + cmds.len()) as u32;
        self.ticket_add(ticket, frames, zcl_optimistic);
        for ((ep, cluster, mfr), recs) in writes {
            let tsn = self.next_zcl_tsn();
            let payload = zf::write_attributes(&recs).map_err(|e| bad(e.to_string()))?;
            let f = ZclFrame::command(FrameType::Global, tsn, global::WRITE_ATTRIBUTES, mfr, payload);
            self.send_zcl(&did, ep, cluster, f, Purpose::Command(ticket), now);
        }
        for a in cmds {
            if let Action::Cmd { ep, cluster, mfr, id, payload } = a {
                let tsn = self.next_zcl_tsn();
                let f = ZclFrame::command(FrameType::Cluster, tsn, id, mfr, payload);
                self.send_zcl(&did, ep, cluster, f, Purpose::Command(ticket), now);
            }
        }
        if !dps.is_empty() {
            self.tuya_send(&did, &dps, now);
            if !tuya_optimistic {
                let dp_ids: Vec<u8> = dps.iter().map(|r| r.dp).collect();
                self.ticket_wait_tuya(ticket, &did, &dp_ids, now + 15_000);
            }
        }
        if !hub_vals.is_empty() {
            let d = self.devices.get_mut(&did).unwrap();
            for (k, v) in hub_vals {
                d.options.insert(k, v.clone());
                set_value(d, i, None, v, now);
            }
            self.publish_state(uid);
        }
        if let Some(s) = plan.reset_after_s {
            if let Some(d) = self.devices.get_mut(&did) {
                d.entities[i].state.reset_at = Some(now + s * 1000);
            }
        }
        if queued {
            if let Some(d) = self.devices.get_mut(&did) {
                d.entities[i].state.pending = Some(json!({"command": command, "params": params}));
            }
            self.publish_state(uid);
        }
        let waiting = frames > 0 || (!dps.is_empty() && !tuya_optimistic);
        if !waiting {
            // Nothing to confirm (hub options, optimistic Tuya).
            self.ticket_part_done_immediate(ticket, now);
            return Ok(CommandAck { ticket, status: "ok" });
        }
        Ok(CommandAck { ticket, status: if queued { "queued" } else { "pending" } })
    }

    fn ticket_part_done_immediate(&mut self, ticket: u64, now: Millis) {
        self.ticket_add(ticket, 1, Vec::new());
        self.ticket_part_done(ticket, None, now);
    }

    /// `entities/refresh`: reads the bound attributes now (mains devices).
    pub fn refresh(&mut self, uid: &str, now: Millis) -> Result<(), ApiError> {
        let did = device_of_uid(uid).to_string();
        let d = self.devices.get(&did).ok_or_else(|| ApiError::new("not_found", uid.to_string()))?;
        let e = d.entities.iter().find(|e| e.uid == uid).ok_or_else(|| ApiError::new("not_found", uid.to_string()))?;
        if d.sleepy() || d.gp.is_some() {
            return Err(ApiError::new("unsupported", "sleepy and GP devices cannot be polled"));
        }
        let mut reads: BTreeMap<(u8, u16, Option<u16>), Vec<u16>> = BTreeMap::new();
        let mut tuya = false;
        for (_, v) in e.r.def.values() {
            match &v.binding {
                Binding::Zcl(z) => reads.entry((e.r.ep, z.cluster, z.mfr)).or_default().push(z.attr),
                Binding::Struct { cluster, attr, mfr, .. } => reads.entry((e.r.ep, *cluster, *mfr)).or_default().push(*attr),
                Binding::Tuya { .. } => tuya = true,
                _ => {}
            }
        }
        if reads.is_empty() && !tuya {
            return Err(ApiError::new("unsupported", "nothing to read"));
        }
        for ((ep, c, m), attrs) in reads {
            for chunk in attrs.chunks(5) {
                self.read_attrs(&did, ep, c, m, chunk, Purpose::Refresh, now);
            }
        }
        if tuya {
            self.tuya_query(&did, now);
        }
        Ok(())
    }

    /// `entities/update`: name, enabled, value of a hub binding.
    pub fn update_entity(&mut self, uid: &str, name: Option<Option<String>>, enabled: Option<bool>, value: Option<&J>, now: Millis) -> Result<J, ApiError> {
        let did = device_of_uid(uid).to_string();
        let d = self.devices.get_mut(&did).ok_or_else(|| ApiError::new("not_found", uid.to_string()))?;
        let i = d.entity_index(uid).ok_or_else(|| ApiError::new("not_found", uid.to_string()))?;
        if let Some(n) = name {
            d.entities[i].user_name = n;
        }
        if let Some(en) = enabled {
            d.entities[i].user_enabled = Some(en);
        }
        if let Some(v) = value {
            if !matches!(d.entities[i].r.def.value.binding, Binding::Hub { .. }) {
                return Err(ApiError::new("invalid_value", "value is only accepted for hub bindings"));
            }
            let v = Value::from_json(v);
            let key = d.entities[i].r.def.key.clone();
            d.options.insert(key, v.clone());
            set_value(d, i, None, v, now);
        }
        let rec = d.entities[i].record(&did);
        self.emit(HubEvent::EntityUpdated(uid.to_string()));
        self.publish_state(uid);
        Ok(rec)
    }
}

/// `true` when the value is a ZCL bool.
pub fn is_bool_value(v: &ZclValue) -> bool {
    matches!(v, ZclValue::Bool(_))
}
