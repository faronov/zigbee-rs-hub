//! Home Assistant MQTT discovery as Zigbee2MQTT publishes it: one retained
//! config per entity under `<prefix>/<component>/<ieee>/<object_id>/config`,
//! `unique_id` = `<ieee>_<object_id>_<base topic>`, device identifiers
//! `zigbee2mqtt_<ieee>` and availability from `bridge/state` plus the
//! device's `availability` topic. Opt-in: the hub's own HA integration
//! creates the same devices, so a setup uses one or the other.

use serde_json::{Map, Value as J};

use crate::obj::{Obj, put};

use crate::model::Platform;
use crate::z2m::{self, Feat, Kind, Layout};

/// What every config needs.
pub struct Ctx<'a> {
    /// Base topic.
    pub base: &'a str,
    /// Discovery prefix.
    pub prefix: &'a str,
    /// The bridge's `0x…` coordinator IEEE (its device identifier).
    pub coordinator: &'a str,
    /// Hub software version.
    pub version: &'a str,
}

impl Ctx<'_> {
    fn bridge_id(&self) -> String {
        format!("zigbee2mqtt_bridge_{}", self.coordinator)
    }

    fn origin(&self) -> J {
        Obj::new()
            .with("name", "zigbee-rs-hub")
            .with("sw", self.version)
            .with("url", "https://github.com/faronov/zigbee-rs-hub")
            .j()
    }

    fn availability(&self, friendly: Option<&str>) -> J {
        let mut a = vec![
            Obj::new()
                .with("topic", format!("{}/bridge/state", self.base))
                .with("value_template", "{{ value_json.state }}")
                .j(),
        ];
        if let Some(f) = friendly {
            a.push(
                Obj::new()
                    .with("topic", format!("{}/{f}/availability", self.base))
                    .with("value_template", "{{ value_json.state }}")
                    .j(),
            );
        }
        J::Array(a)
    }
}

/// The config topic of an entity.
pub fn topic(ctx: &Ctx, component: &str, node: &str, object_id: &str) -> String {
    format!("{}/{component}/{node}/{object_id}/config", ctx.prefix)
}

/// `{{ value_json.temperature }}` (bracket syntax for names Jinja cannot
/// take as attributes).
fn tpl(prop: &str) -> String {
    let plain = prop
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && prop.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        format!("{{{{ value_json.{prop} }}}}")
    } else {
        format!("{{{{ value_json['{prop}'] }}}}")
    }
}

/// Component and object ID of an entity's config (`None`: nothing to
/// discover).
fn component(f: &Feat) -> Option<(&'static str, String)> {
    let main = |name: &str| match &f.suffix {
        Some(s) => format!("{name}_{s}"),
        None => name.to_string(),
    };
    Some(match f.kind {
        Kind::Simple => (
            match f.e.platform {
                Platform::BinarySensor => "binary_sensor",
                Platform::Number => "number",
                Platform::Select => "select",
                Platform::Text => "text",
                _ => "sensor",
            },
            f.p(&f.prop),
        ),
        Kind::Switch if f.prop == "state" => ("switch", main("switch")),
        Kind::Switch => ("switch", f.p(&f.prop)),
        Kind::Button => ("button", f.p(&f.prop)),
        Kind::Siren => ("siren", f.p(&f.prop)),
        Kind::Light => ("light", main("light")),
        Kind::Cover if f.e.platform == Platform::Valve => ("valve", main("valve")),
        Kind::Cover => ("cover", main("cover")),
        Kind::Climate => ("climate", main("climate")),
        Kind::Lock => ("lock", main("lock")),
        Kind::Fan => ("fan", main("fan")),
        Kind::Action => return None,
    })
}

/// Every discovery config of a device: `(topic, payload)`.
pub fn device_configs(ctx: &Ctx, l: &Layout, friendly: &str) -> Vec<(String, J)> {
    let d = l.dev;
    let node = d.ieee_address();
    let state_topic = format!("{}/{friendly}", ctx.base);
    let set = |prop: &str| format!("{}/{friendly}/set/{prop}", ctx.base);
    let mut device = Obj::new()
        .with("identifiers", J::from(vec![format!("zigbee2mqtt_{node}")]))
        .with("name", friendly)
        .with("via_device", ctx.bridge_id())
        .j();
    for (k, v) in [
        ("manufacturer", d.manufacturer.clone()),
        (
            "model",
            d.definition_model.clone().or_else(|| d.model.clone()),
        ),
        ("model_id", d.model_id.clone()),
        ("sw_version", d.sw_version.clone()),
    ] {
        if let Some(v) = v {
            device[k] = v.into();
        }
    }
    let common = |object_id: &str, name: J| {
        let mut c = Map::new();
        put(&mut c, "availability", ctx.availability(Some(friendly)));
        put(&mut c, "availability_mode", "all".into());
        put(&mut c, "device", device.clone());
        put(&mut c, "name", name);
        put(
            &mut c,
            "object_id",
            format!("{}_{object_id}", friendly.replace('/', "_")).into(),
        );
        put(&mut c, "origin", ctx.origin());
        put(
            &mut c,
            "unique_id",
            format!("{node}_{object_id}_{}", ctx.base).into(),
        );
        put(&mut c, "state_topic", state_topic.clone().into());
        c
    };
    let mut out = Vec::new();
    for f in &l.feats {
        let Some((comp, oid)) = component(f) else {
            continue;
        };
        let e = f.e;
        let p = |b: &str| f.p(b);
        // Composite and main switch entities take the device name (or the
        // endpoint's); others their own.
        let main_name = match &f.suffix {
            Some(s) => J::from(z2m::label(s)),
            None => J::Null,
        };
        let name = match f.kind {
            Kind::Light | Kind::Cover | Kind::Climate | Kind::Lock | Kind::Fan => main_name,
            Kind::Switch if f.prop == "state" => main_name,
            _ => J::from(e.name.clone().unwrap_or_else(|| z2m::label(&p(&f.prop)))),
        };
        let mut c = common(&oid, name);
        if !e.enabled_default {
            put(&mut c, "enabled_by_default", false.into());
        }
        if let Some(cat @ ("config" | "diagnostic")) = e.category.as_deref() {
            put(&mut c, "entity_category", cat.into());
        }
        let prop = p(&f.prop);
        match f.kind {
            Kind::Simple => {
                put(&mut c, "value_template", tpl(&prop).into());
                match e.platform {
                    Platform::BinarySensor => {
                        put(&mut c, "payload_on", true.into());
                        put(&mut c, "payload_off", false.into());
                        if let Some(dc) = &e.device_class {
                            put(&mut c, "device_class", dc.clone().into());
                        }
                    }
                    Platform::Number => {
                        put(&mut c, "command_topic", set(&prop).into());
                        for (k, cap) in [
                            ("min", "native_min_value"),
                            ("max", "native_max_value"),
                            ("step", "native_step"),
                        ] {
                            if let Some(v) = e.cap_f64(cap) {
                                c.insert(k.into(), J::from(v));
                            }
                        }
                        if let Some(m) = e.cap("mode").filter(J::is_string) {
                            put(&mut c, "mode", m);
                        }
                        if let Some(u) = &e.unit {
                            put(&mut c, "unit_of_measurement", u.clone().into());
                        }
                    }
                    Platform::Select => {
                        put(&mut c, "command_topic", set(&prop).into());
                        put(&mut c, "options", e.cap_list("options").into());
                    }
                    Platform::Text => {
                        put(&mut c, "command_topic", set(&prop).into());
                        if let Some(p) = e.cap("pattern") {
                            put(&mut c, "pattern", p);
                        }
                    }
                    _ => {
                        let voltage_mv = f.prop == "voltage" && e.key == "battery_voltage";
                        if let Some(u) = &e.unit {
                            put(
                                &mut c,
                                "unit_of_measurement",
                                if voltage_mv {
                                    "mV".into()
                                } else {
                                    u.clone().into()
                                },
                            );
                        }
                        match e.device_class.as_deref() {
                            // Epoch seconds are not HA timestamps.
                            Some("timestamp" | "date") | None => {}
                            Some("enum") => {
                                put(&mut c, "device_class", "enum".into());
                                put(&mut c, "options", e.cap_list("options").into());
                            }
                            Some(dc) => {
                                put(&mut c, "device_class", dc.into());
                            }
                        }
                        if let Some(sc) = &e.state_class {
                            put(&mut c, "state_class", sc.clone().into());
                        }
                        if let Some(p) = e.cap("suggested_display_precision") {
                            put(&mut c, "suggested_display_precision", p);
                        }
                    }
                }
            }
            Kind::Switch | Kind::Siren => {
                put(&mut c, "command_topic", set(&prop).into());
                put(&mut c, "payload_on", "ON".into());
                put(&mut c, "payload_off", "OFF".into());
                put(&mut c, "value_template", tpl(&prop).into());
                if f.kind == Kind::Siren {
                    put(&mut c, "state_value_template", tpl(&prop).into());
                    c.remove("value_template");
                    put(&mut c, "state_on", "ON".into());
                    put(&mut c, "state_off", "OFF".into());
                } else if let Some(dc @ ("outlet" | "switch")) = e.device_class.as_deref() {
                    put(&mut c, "device_class", dc.into());
                }
            }
            Kind::Button => {
                put(&mut c, "command_topic", set(&prop).into());
                put(&mut c, "payload_press", f.prop.clone().into());
                c.remove("state_topic");
                if let Some(dc @ ("identify" | "restart" | "update")) = e.device_class.as_deref() {
                    put(&mut c, "device_class", dc.into());
                } else if e.key == "identify" {
                    put(&mut c, "device_class", "identify".into());
                }
            }
            Kind::Light => {
                // HA's JSON light schema reads `state`, `brightness`,
                // `color_temp`, `color` unprefixed: an endpoint light uses
                // its own topics `<name>/<endpoint>` and `<name>/<endpoint>/set`.
                let (st, cmd) = match &f.suffix {
                    Some(s) => (
                        format!("{state_topic}/{s}"),
                        format!("{state_topic}/{s}/set"),
                    ),
                    None => (state_topic.clone(), format!("{state_topic}/set")),
                };
                put(&mut c, "schema", "json".into());
                put(&mut c, "state_topic", st.into());
                put(&mut c, "command_topic", cmd.into());
                let modes = e.cap_list("supported_color_modes");
                let ha: Vec<&str> = modes
                    .iter()
                    .map(String::as_str)
                    .filter(|m| matches!(*m, "onoff" | "brightness" | "color_temp" | "xy" | "hs"))
                    .collect();
                let ha = if ha.is_empty() { vec!["onoff"] } else { ha };
                if ha.iter().any(|m| *m != "onoff") {
                    put(&mut c, "brightness", true.into());
                    put(&mut c, "brightness_scale", 254.into());
                }
                if ha.contains(&"color_temp") {
                    let mired = |k: f64| (1_000_000.0 / k).round() as i64;
                    put(
                        &mut c,
                        "min_mireds",
                        e.cap_f64("max_color_temp_kelvin").map_or(150, mired).into(),
                    );
                    put(
                        &mut c,
                        "max_mireds",
                        e.cap_f64("min_color_temp_kelvin").map_or(500, mired).into(),
                    );
                }
                put(&mut c, "supported_color_modes", J::from(ha));
                if e.cap_list("supported_features")
                    .iter()
                    .any(|x| x == "transition")
                {
                    put(&mut c, "transition", true.into());
                }
            }
            Kind::Cover => {
                let s = p("state");
                put(&mut c, "command_topic", set(&s).into());
                put(&mut c, "payload_open", "OPEN".into());
                put(&mut c, "payload_close", "CLOSE".into());
                put(&mut c, "payload_stop", "STOP".into());
                put(&mut c, "value_template", tpl(&s).into());
                put(&mut c, "state_open", "OPEN".into());
                put(&mut c, "state_closed", "CLOSE".into());
                let sf = e.cap_list("supported_features");
                if sf.iter().any(|x| x == "set_position") {
                    if comp == "valve" {
                        put(&mut c, "reports_position", true.into());
                        put(&mut c, "value_template", tpl(&p("position")).into());
                        put(&mut c, "command_topic", set(&p("position")).into());
                        c.remove("payload_open");
                        c.remove("payload_close");
                    } else {
                        put(&mut c, "position_topic", state_topic.clone().into());
                        put(&mut c, "position_template", tpl(&p("position")).into());
                        put(&mut c, "set_position_topic", set(&p("position")).into());
                    }
                }
                if comp == "valve" {
                    c.remove("payload_stop");
                }
                if sf.iter().any(|x| x == "set_tilt_position") {
                    put(&mut c, "tilt_command_topic", set(&p("tilt")).into());
                    put(&mut c, "tilt_status_topic", state_topic.clone().into());
                    put(&mut c, "tilt_status_template", tpl(&p("tilt")).into());
                }
            }
            Kind::Climate => {
                c.remove("state_topic");
                let st = state_topic.clone();
                let mut both = |prefix: &str, prop: String, cmd: bool| {
                    put(&mut c, &format!("{prefix}_state_topic"), st.clone().into());
                    put(
                        &mut c,
                        &format!("{prefix}_state_template"),
                        tpl(&prop).into(),
                    );
                    if cmd {
                        put(
                            &mut c,
                            &format!("{prefix}_command_topic"),
                            set(&prop).into(),
                        );
                    }
                };
                both("temperature", p(l.setpoint), true);
                let modes = e.cap_list("hvac_modes");
                if !modes.is_empty() {
                    both("mode", p("system_mode"), true);
                    put(&mut c, "modes", modes.into());
                }
                let presets: Vec<String> = e
                    .cap_list("preset_modes")
                    .into_iter()
                    .filter(|x| x != "none")
                    .collect();
                if !presets.is_empty() {
                    put(
                        &mut c,
                        "preset_mode_state_topic",
                        state_topic.clone().into(),
                    );
                    put(
                        &mut c,
                        "preset_mode_value_template",
                        tpl(&p("preset")).into(),
                    );
                    put(
                        &mut c,
                        "preset_mode_command_topic",
                        set(&p("preset")).into(),
                    );
                    put(&mut c, "preset_modes", presets.into());
                }
                let fans = e.cap_list("fan_modes");
                if !fans.is_empty() {
                    put(&mut c, "fan_mode_state_topic", state_topic.clone().into());
                    put(
                        &mut c,
                        "fan_mode_state_template",
                        tpl(&p("fan_mode")).into(),
                    );
                    put(&mut c, "fan_mode_command_topic", set(&p("fan_mode")).into());
                    put(&mut c, "fan_modes", fans.into());
                }
                put(
                    &mut c,
                    "current_temperature_topic",
                    state_topic.clone().into(),
                );
                put(
                    &mut c,
                    "current_temperature_template",
                    tpl(&p("local_temperature")).into(),
                );
                put(&mut c, "action_topic", state_topic.clone().into());
                c.insert(
                    "action_template".into(),
                    format!(
                        "{{% set values = {{None:None,'idle':'idle','heat':'heating','cool':'cooling','fan_only':'fan'}} %}}{{{{ values[value_json.{}] }}}}",
                        p("running_state")
                    )
                    .into(),
                );
                for (k, cap, def) in [
                    ("min_temp", "min_temp", 7.0),
                    ("max_temp", "max_temp", 35.0),
                    ("temp_step", "target_temperature_step", 0.5),
                ] {
                    c.insert(k.into(), J::from(e.cap_f64(cap).unwrap_or(def)));
                }
                put(&mut c, "temperature_unit", "C".into());
            }
            Kind::Lock => {
                let s = p("state");
                put(&mut c, "command_topic", set(&s).into());
                put(&mut c, "payload_lock", "LOCK".into());
                put(&mut c, "payload_unlock", "UNLOCK".into());
                put(&mut c, "state_locked", "LOCK".into());
                put(&mut c, "state_unlocked", "UNLOCK".into());
                put(&mut c, "value_template", tpl(&s).into());
            }
            Kind::Fan => {
                let s = p("fan_state");
                put(&mut c, "command_topic", set(&s).into());
                put(&mut c, "state_value_template", tpl(&s).into());
                put(&mut c, "payload_on", "ON".into());
                put(&mut c, "payload_off", "OFF".into());
                put(
                    &mut c,
                    "preset_mode_state_topic",
                    state_topic.clone().into(),
                );
                put(
                    &mut c,
                    "preset_mode_value_template",
                    tpl(&p("fan_mode")).into(),
                );
                put(
                    &mut c,
                    "preset_mode_command_topic",
                    set(&p("fan_mode")).into(),
                );
                put(
                    &mut c,
                    "preset_modes",
                    J::from(vec!["off", "low", "medium", "high", "on", "auto", "smart"]),
                );
            }
            Kind::Action => {}
        }
        out.push((topic(ctx, comp, &node, &oid), J::Object(c)));
    }
    // Events: one HA event entity fed by `<name>/action`.
    let actions = z2m::action_values(l);
    if l.feats.iter().any(|f| f.kind == Kind::Action) {
        let mut c = common("action", "Action".into());
        put(
            &mut c,
            "state_topic",
            format!("{state_topic}/action").into(),
        );
        put(
            &mut c,
            "value_template",
            "{ \"event_type\": \"{{ value }}\" }".into(),
        );
        put(&mut c, "event_types", actions.into());
        out.push((topic(ctx, "event", &node, "action"), J::Object(c)));
    }
    if !d.is_gp() {
        let mut c = common("linkquality", "Linkquality".into());
        put(&mut c, "value_template", tpl("linkquality").into());
        put(&mut c, "unit_of_measurement", "lqi".into());
        put(&mut c, "state_class", "measurement".into());
        put(&mut c, "entity_category", "diagnostic".into());
        put(&mut c, "enabled_by_default", false.into());
        put(&mut c, "icon", "mdi:signal".into());
        out.push((topic(ctx, "sensor", &node, "linkquality"), J::Object(c)));
    }
    out
}

/// The bridge's own device: connection state and permit join.
pub fn bridge_configs(ctx: &Ctx) -> Vec<(String, J)> {
    let node = format!("bridge_{}", ctx.coordinator);
    let device = Obj::new()
        .with("identifiers", J::from(vec![ctx.bridge_id()]))
        .with("name", "zigbee-rs-hub bridge")
        .with("manufacturer", "zigbee-rs")
        .with("model", "Bridge")
        .with("sw_version", ctx.version)
        .j();
    let common = |oid: &str, name: &str| {
        let mut c = Map::new();
        put(&mut c, "device", device.clone());
        put(&mut c, "name", name.into());
        put(
            &mut c,
            "object_id",
            format!("zigbee2mqtt_bridge_{oid}").into(),
        );
        put(&mut c, "origin", ctx.origin());
        put(
            &mut c,
            "unique_id",
            format!("{node}_{oid}_{}", ctx.base).into(),
        );
        c
    };
    let mut conn = common("connection_state", "Connection state");
    put(
        &mut conn,
        "state_topic",
        format!("{}/bridge/state", ctx.base).into(),
    );
    put(&mut conn, "value_template", "{{ value_json.state }}".into());
    put(&mut conn, "payload_on", "online".into());
    put(&mut conn, "payload_off", "offline".into());
    put(&mut conn, "device_class", "connectivity".into());
    put(&mut conn, "entity_category", "diagnostic".into());
    let mut pj = common("permit_join", "Permit join");
    put(&mut pj, "availability", ctx.availability(None));
    put(
        &mut pj,
        "state_topic",
        format!("{}/bridge/info", ctx.base).into(),
    );
    put(
        &mut pj,
        "value_template",
        "{{ value_json.permit_join | lower }}".into(),
    );
    put(
        &mut pj,
        "command_topic",
        format!("{}/bridge/request/permit_join", ctx.base).into(),
    );
    put(&mut pj, "payload_on", "{\"time\": 254}".into());
    put(&mut pj, "payload_off", "{\"time\": 0}".into());
    put(&mut pj, "state_on", "true".into());
    put(&mut pj, "state_off", "false".into());
    put(&mut pj, "icon", "mdi:human-greeting-proximity".into());
    vec![
        (
            topic(ctx, "binary_sensor", &node, "connection_state"),
            J::Object(conn),
        ),
        (topic(ctx, "switch", &node, "permit_join"), J::Object(pj)),
    ]
}
