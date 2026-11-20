//! Tier a: generic ZCL rules (spec 5). No data: rules match the clusters
//! of the fingerprint and what the hub learned about attribute support,
//! and produce entities with fixed keys, reporting defaults (5.2) and the
//! events of client clusters (5.4).

use crate::expr::{NativeExpr, Value};
use crate::fingerprint::Fingerprint;
use crate::model::{
    Access, AttrScale, Binding, Category, DeviceField, EntityDef, EventSel, Platform, PowerKind,
    Press, PressPayload, ReportSpec, StateClass, Trigger, TriggerSource, ValueDef, ZclBinding,
    ZclWrite,
};
use crate::zcl::{PROFILE_GP, ZclType, ZclValue, cluster as cl};

/// What the hub knows about one attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// Never read or reported.
    Unknown,
    /// A read or report succeeded.
    Supported,
    /// UNSUPPORTED_ATTRIBUTE.
    Unsupported,
}

/// Read-only view of the device's attribute knowledge.
pub trait AttrView {
    /// Support state.
    fn support(&self, ep: u8, cluster: u16, attr: u16) -> Support;
    /// Last known value.
    fn value(&self, ep: u8, cluster: u16, attr: u16) -> Option<&ZclValue>;
}

/// Inputs of the generic tier besides the fingerprint.
pub struct GenericInput<'a> {
    /// Attribute knowledge.
    pub attrs: &'a dyn AttrView,
    /// Effective power source (fingerprint overlaid by definitions).
    pub power: PowerKind,
}

/// A reporting default.
pub fn rep(min: u16, max: u16, change: Option<f64>) -> Option<ReportSpec> {
    Some(ReportSpec::Config { min, max, change })
}

/// A ZCL value definition.
pub fn zcl(
    cluster: u16,
    attr: u16,
    ty: ZclType,
    write: ZclWrite,
    report: Option<ReportSpec>,
) -> ValueDef {
    ValueDef {
        binding: Binding::Zcl(ZclBinding {
            cluster,
            attr,
            ty,
            mfr: None,
            write,
        }),
        report,
        ..Default::default()
    }
}

fn ro() -> Option<Access> {
    Some(Access {
        read: true,
        write: false,
        report: true,
    })
}

fn native_from(name: &'static str, f: fn(f64) -> Value) -> Option<crate::expr::Expr> {
    Some(NativeExpr::new(name, move |c| {
        c.x.as_f64().map_or(Value::Null, f)
    }))
}

/// The measurement rule of one `(cluster, attribute)`, shared by the
/// Zigbee generic tier and the generic GP tier (13.4).
pub fn measurement(cluster: u16, attr: u16) -> Option<EntityDef> {
    let sensor = |key: &str, dc: Option<&str>, unit: Option<&str>| {
        let mut e = EntityDef::new(key, Platform::Sensor);
        e.device_class = dc.map(str::to_string);
        e.unit = unit.map(str::to_string);
        e.state_class = Some(StateClass::Measurement);
        e.translation_key = Some(key.to_string());
        e
    };
    let mut e;
    match (cluster, attr) {
        (cl::TEMPERATURE, 0x0000) => {
            e = sensor("temperature", Some("temperature"), Some("°C"));
            e.value = zcl(
                cluster,
                attr,
                ZclType::I16,
                ZclWrite::ReadOnly,
                rep(30, 900, Some(50.0)),
            );
            e.value.transforms.invalid = Some(-32768.0);
            e.value.transforms.div = Some(100.0);
        }
        (cl::HUMIDITY, 0x0000) => {
            e = sensor("humidity", Some("humidity"), Some("%"));
            e.value = zcl(
                cluster,
                attr,
                ZclType::U16,
                ZclWrite::ReadOnly,
                rep(30, 900, Some(100.0)),
            );
            e.value.transforms.invalid = Some(65535.0);
            e.value.transforms.div = Some(100.0);
        }
        (cl::PRESSURE, 0x0000) => {
            e = sensor("pressure", Some("atmospheric_pressure"), Some("hPa"));
            e.value = zcl(
                cluster,
                attr,
                ZclType::I16,
                ZclWrite::ReadOnly,
                rep(30, 900, Some(1.0)),
            );
            e.value.transforms.invalid = Some(-32768.0);
        }
        (cl::ILLUMINANCE, 0x0000) => {
            e = sensor("illuminance", Some("illuminance"), Some("lx"));
            e.value = zcl(
                cluster,
                attr,
                ZclType::U16,
                ZclWrite::ReadOnly,
                rep(30, 900, Some(500.0)),
            );
            e.value.transforms.invalid = Some(65535.0);
            e.value.transforms.from = native_from("illuminance_log", |x| {
                if x == 0.0 {
                    Value::Int(0)
                } else {
                    Value::Int(10f64.powf((x - 1.0) / 10000.0).round() as i64)
                }
            });
        }
        (cl::OCCUPANCY, 0x0000) => {
            e = EntityDef::new("occupancy", Platform::BinarySensor);
            e.device_class = Some("occupancy".into());
            e.value = zcl(
                cluster,
                attr,
                ZclType::MAP8,
                ZclWrite::ReadOnly,
                rep(0, 900, None),
            );
            e.value.transforms.from = Some(NativeExpr::new("bit0", |c| {
                c.x.as_i64()
                    .map_or(Value::Null, |v| Value::Bool(v & 1 == 1))
            }));
        }
        (cl::SOIL_MOISTURE, 0x0000) => {
            e = sensor("soil_moisture", Some("moisture"), Some("%"));
            e.value = zcl(
                cluster,
                attr,
                ZclType::U16,
                ZclWrite::ReadOnly,
                rep(30, 900, Some(100.0)),
            );
            e.value.transforms.invalid = Some(65535.0);
            e.value.transforms.div = Some(100.0);
        }
        (cl::POWER_CONFIG, 0x0021) => {
            e = sensor("battery", Some("battery"), Some("%"));
            e.category = Some(Category::Diagnostic);
            e.value = zcl(
                cluster,
                attr,
                ZclType::U8,
                ZclWrite::ReadOnly,
                rep(3600, 21600, Some(2.0)),
            );
            e.value.transforms.invalid = Some(255.0);
            e.value.transforms.div = Some(2.0);
        }
        (cl::POWER_CONFIG, 0x0020) => {
            e = sensor("battery_voltage", Some("voltage"), Some("V"));
            e.category = Some(Category::Diagnostic);
            e.enabled = false;
            e.value = zcl(
                cluster,
                attr,
                ZclType::U8,
                ZclWrite::ReadOnly,
                rep(3600, 21600, Some(1.0)),
            );
            e.value.transforms.invalid = Some(255.0);
            e.value.transforms.div = Some(10.0);
        }
        (cl::DEVICE_TEMPERATURE, 0x0000) => {
            e = sensor("device_temperature", Some("temperature"), Some("°C"));
            e.category = Some(Category::Diagnostic);
            e.enabled = false;
            e.value = zcl(cluster, attr, ZclType::I16, ZclWrite::ReadOnly, None);
            e.value.transforms.invalid = Some(-32768.0);
        }
        (c, 0x0000) if (0x040C..=0x042F).contains(&c) => {
            let (key, dc, unit, ppm) = concentration(c)?;
            e = sensor(key, dc, Some(unit));
            e.value = zcl(
                c,
                0,
                ZclType::SINGLE,
                ZclWrite::ReadOnly,
                rep(30, 900, Some(if ppm { 0.000_025 } else { 1.0 })),
            );
            if ppm {
                e.value.transforms.mul = Some(1_000_000.0);
            }
            // A single-precision fraction scaled to ppm carries float noise.
            e.value.transforms.precision = Some(if ppm { 0 } else { 1 });
        }
        _ => return None,
    }
    e.value.access = e.value.access.or_else(ro);
    Some(e)
}

/// Concentration clusters 0x040C–0x042F: key, HA device class, unit,
/// "fraction → ppm".
fn concentration(c: u16) -> Option<(&'static str, Option<&'static str>, &'static str, bool)> {
    Some(match c {
        0x040C => ("carbon_monoxide", Some("carbon_monoxide"), "ppm", true),
        0x040D => ("co2", Some("carbon_dioxide"), "ppm", true),
        0x040E => ("ethylene", None, "ppm", true),
        0x040F => ("ethylene_oxide", None, "ppm", true),
        0x0410 => ("hydrogen", None, "ppm", true),
        0x0411 => ("hydrogen_sulfide", None, "ppm", true),
        0x0412 => ("nitric_oxide", None, "ppm", true),
        0x0413 => ("nitrogen_dioxide", None, "ppm", true),
        0x0414 => ("oxygen", None, "ppm", true),
        0x0415 => ("ozone", None, "ppm", true),
        0x0416 => ("sulfur_dioxide", None, "ppm", true),
        0x042A => ("pm25", Some("pm25"), "µg/m³", false),
        0x042B => ("formaldehyde", None, "ppm", true),
        0x042C => ("pm1", Some("pm1"), "µg/m³", false),
        0x042D => ("pm10", Some("pm10"), "µg/m³", false),
        0x042E => ("voc", Some("volatile_organic_compounds_parts"), "ppm", true),
        0x042F => ("radon", None, "Bq/m³", false),
        _ => return None,
    })
}

fn is_light(profile: u16, device_id: u16, has_color: bool) -> bool {
    has_color
        || (profile == crate::zcl::PROFILE_HA
            && matches!(device_id, 0x0100 | 0x0101 | 0x0102 | 0x010C | 0x010D))
        || (profile == crate::zcl::PROFILE_ZLL
            && matches!(device_id, 0x0000 | 0x0100 | 0x0200 | 0x0210 | 0x0220))
}

fn is_outlet(profile: u16, device_id: u16) -> bool {
    (profile == crate::zcl::PROFILE_HA && matches!(device_id, 0x0009 | 0x0051 | 0x010A))
        || (profile == crate::zcl::PROFILE_ZLL && device_id == 0x0010)
}

/// Reads the generic tier wants at configure to decide on optional
/// entities and capabilities (beyond the exposed attributes).
pub fn probe_reads(fp: &Fingerprint) -> Vec<(u8, u16, Vec<u16>)> {
    let mut out = Vec::new();
    for e in fp.endpoints.iter().filter(|e| e.profile != PROFILE_GP) {
        for &c in &e.in_clusters {
            let attrs: &[u16] = match c {
                cl::ON_OFF => &[0x4003],
                cl::LEVEL => &[0x0010, 0x0011],
                cl::COLOR => &[0x400A, 0x400B, 0x400C, 0x0008],
                cl::OCCUPANCY => &[0x0010],
                cl::POWER_CONFIG => &[0x0020, 0x0021],
                cl::DEVICE_TEMPERATURE => &[0x0000],
                cl::IAS_ZONE => &[0x0000, 0x0001, 0x0002],
                cl::THERMOSTAT => &[0x001B, 0x0015, 0x0016, 0x0010, 0x0008],
                cl::FAN_CONTROL => &[0x0001],
                cl::WINDOW_COVERING => &[0x0009],
                cl::METERING => &[0x0300, 0x0301, 0x0302, 0x0400],
                cl::ELECTRICAL => &[0x0600, 0x0601, 0x0602, 0x0603, 0x0604],
                cl::BINARY_INPUT | cl::ANALOG_INPUT | cl::MULTISTATE_INPUT => &[0x001C],
                cl::DIAGNOSTICS => &[0x011C, 0x011D],
                _ => &[],
            };
            if !attrs.is_empty() {
                out.push((e.ep, c, attrs.to_vec()));
            }
            if c == cl::ELECTRICAL {
                out.push((e.ep, c, vec![0x0605, 0x0400, 0x0401, 0x0510]));
            }
        }
    }
    out
}

/// Runs the server-cluster rules (5.1), the client-cluster events (5.4)
/// and the device-level entities.
pub fn generic_entities(fp: &Fingerprint, input: &GenericInput<'_>) -> Vec<EntityDef> {
    let mut out = Vec::new();
    let a = input.attrs;
    let present = |ep: u8, c: u16, at: u16| a.support(ep, c, at) != Support::Unsupported;
    let supported = |ep: u8, c: u16, at: u16| a.support(ep, c, at) == Support::Supported;
    let val = |ep: u8, c: u16, at: u16| a.value(ep, c, at).and_then(ZclValue::as_i64);

    for epd in fp.endpoints.iter().filter(|e| e.profile != PROFILE_GP) {
        let ep = epd.ep;
        let has = |c: u16| epd.in_clusters.contains(&c);
        let mut push = |mut e: EntityDef| {
            e.ep = Some(ep);
            out.push(e);
        };

        // On/Off, Level, Color → light or switch.
        if has(cl::ON_OFF) && present(ep, cl::ON_OFF, 0) {
            if is_light(epd.profile, epd.device_id, has(cl::COLOR)) {
                push(light(ep, epd, a));
            } else {
                let mut e = EntityDef::new("switch", Platform::Switch);
                if is_outlet(epd.profile, epd.device_id) {
                    e.device_class = Some("outlet".into());
                }
                e.value = zcl(
                    cl::ON_OFF,
                    0,
                    ZclType::BOOL,
                    ZclWrite::OnOffCommands,
                    rep(0, 900, None),
                );
                push(e);
            }
            if supported(ep, cl::ON_OFF, 0x4003) {
                let mut e = EntityDef::new("power_on_behavior", Platform::Select);
                e.category = Some(Category::Config);
                e.value = zcl(
                    cl::ON_OFF,
                    0x4003,
                    ZclType::ENUM8,
                    ZclWrite::Attribute,
                    None,
                );
                e.value.transforms.map = vec![
                    (0, "off".into()),
                    (1, "on".into()),
                    (2, "toggle".into()),
                    (255, "previous".into()),
                ];
                push(e);
            }
        }
        if has(cl::LEVEL) {
            if supported(ep, cl::LEVEL, 0x0011) {
                let mut e = EntityDef::new("on_level", Platform::Number);
                e.category = Some(Category::Config);
                e.enabled = false;
                e.value = zcl(cl::LEVEL, 0x0011, ZclType::U8, ZclWrite::Attribute, None);
                e.value.limits.min = Some(1.0);
                e.value.limits.max = Some(255.0);
                push(e);
            }
            if supported(ep, cl::LEVEL, 0x0010) {
                let mut e = EntityDef::new("transition", Platform::Number);
                e.category = Some(Category::Config);
                e.enabled = false;
                e.unit = Some("s".into());
                e.value = zcl(cl::LEVEL, 0x0010, ZclType::U16, ZclWrite::Attribute, None);
                e.value.transforms.div = Some(10.0);
                e.value.limits.min = Some(0.0);
                e.value.limits.max = Some(6553.5);
                e.value.limits.step = Some(0.1);
                push(e);
            }
        }

        // Measurements and power configuration.
        for &c in &epd.in_clusters {
            let attrs: &[u16] = match c {
                cl::POWER_CONFIG if input.power == PowerKind::Battery => &[0x0021, 0x0020],
                cl::POWER_CONFIG => &[],
                cl::DEVICE_TEMPERATURE => &[0x0000],
                _ => &[0x0000],
            };
            for &at in attrs {
                let Some(e) = measurement(c, at) else {
                    continue;
                };
                let optional = matches!(c, cl::POWER_CONFIG | cl::DEVICE_TEMPERATURE);
                if (optional && supported(ep, c, at)) || (!optional && present(ep, c, at)) {
                    push(e);
                }
            }
        }
        if has(cl::OCCUPANCY) && supported(ep, cl::OCCUPANCY, 0x0010) {
            let mut e = EntityDef::new("occupancy_timeout", Platform::Number);
            e.category = Some(Category::Config);
            e.unit = Some("s".into());
            e.value = zcl(
                cl::OCCUPANCY,
                0x0010,
                ZclType::U16,
                ZclWrite::Attribute,
                None,
            );
            e.value.limits.min = Some(0.0);
            e.value.limits.max = Some(3600.0);
            push(e);
        }

        // IAS Zone: entities by ZoneType.
        if has(cl::IAS_ZONE) {
            let zt = match a.support(ep, cl::IAS_ZONE, 0x0001) {
                Support::Supported => val(ep, cl::IAS_ZONE, 0x0001),
                Support::Unsupported => Some(0xFFFF),
                Support::Unknown => None,
            };
            if let Some(zt) = zt {
                let (key, dc) = match zt {
                    0x000D => ("motion", "motion"),
                    0x0015 => ("contact", "door"),
                    0x0028 => ("smoke", "smoke"),
                    0x002A => ("water_leak", "moisture"),
                    0x002B => ("carbon_monoxide", "carbon_monoxide"),
                    0x002D => ("vibration", "vibration"),
                    _ => ("alarm", "safety"),
                };
                for (key, dc, bit, diag) in [
                    (key, dc, 0u8, false),
                    ("tamper", "tamper", 2, true),
                    ("battery_low", "battery", 3, true),
                ] {
                    let mut e = EntityDef::new(key, Platform::BinarySensor);
                    e.device_class = Some(dc.into());
                    if diag {
                        e.category = Some(Category::Diagnostic);
                    }
                    e.value = zcl(
                        cl::IAS_ZONE,
                        0x0002,
                        ZclType::MAP16,
                        ZclWrite::ReadOnly,
                        None,
                    );
                    e.value.access = ro();
                    e.value.transforms.from = Some(NativeExpr::new("zone_bit", move |c| {
                        c.x.as_i64()
                            .map_or(Value::Null, |v| Value::Bool((v >> bit) & 1 == 1))
                    }));
                    push(e);
                }
            }
        }
        if has(cl::IAS_WD) {
            let mut e = EntityDef::new("siren", Platform::Siren);
            e.value.binding = Binding::IasWarning;
            e.capabilities.insert(
                "available_tones".into(),
                serde_json::json!([
                    "burglar",
                    "fire",
                    "emergency",
                    "police_panic",
                    "fire_panic",
                    "emergency_panic"
                ]),
            );
            e.capabilities.insert(
                "supported_features".into(),
                serde_json::json!(["turn_on", "turn_off", "tones", "duration"]),
            );
            push(e);
        }

        if has(cl::THERMOSTAT) {
            push(thermostat(ep, a));
            if supported(ep, cl::THERMOSTAT, 0x0010) {
                let mut e = EntityDef::new("local_temperature_calibration", Platform::Number);
                e.category = Some(Category::Config);
                e.device_class = Some("temperature".into());
                e.unit = Some("°C".into());
                e.value = zcl(
                    cl::THERMOSTAT,
                    0x0010,
                    ZclType::I8,
                    ZclWrite::Attribute,
                    None,
                );
                e.value.transforms.div = Some(10.0);
                e.value.limits = crate::model::Limits {
                    min: Some(-2.5),
                    max: Some(2.5),
                    step: Some(0.1),
                    mode: None,
                };
                push(e);
            }
            if supported(ep, cl::THERMOSTAT, 0x0008) {
                let mut e = EntityDef::new("pi_heating_demand", Platform::Sensor);
                e.unit = Some("%".into());
                e.state_class = Some(StateClass::Measurement);
                e.value = zcl(
                    cl::THERMOSTAT,
                    0x0008,
                    ZclType::U8,
                    ZclWrite::ReadOnly,
                    rep(30, 900, Some(5.0)),
                );
                e.value.access = ro();
                push(e);
            }
        }
        if has(cl::THERMOSTAT_UI) {
            let mut e = EntityDef::new("keypad_lockout", Platform::Select);
            e.category = Some(Category::Config);
            e.value = zcl(
                cl::THERMOSTAT_UI,
                0x0001,
                ZclType::ENUM8,
                ZclWrite::Attribute,
                None,
            );
            e.value.transforms.map = (0..=5)
                .map(|i| {
                    (
                        i,
                        if i == 0 {
                            "unlocked".to_string()
                        } else {
                            format!("lock_{i}")
                        },
                    )
                })
                .collect();
            push(e);
            let mut e = EntityDef::new("temperature_display_mode", Platform::Select);
            e.category = Some(Category::Config);
            e.value = zcl(
                cl::THERMOSTAT_UI,
                0x0000,
                ZclType::ENUM8,
                ZclWrite::Attribute,
                None,
            );
            e.value.transforms.map = vec![(0, "celsius".into()), (1, "fahrenheit".into())];
            push(e);
        }
        if has(cl::FAN_CONTROL) {
            push(fan(ep, a));
        }
        if has(cl::WINDOW_COVERING) {
            let mut e = EntityDef::new("cover", Platform::Cover);
            let mut pos = zcl(
                cl::WINDOW_COVERING,
                0x0008,
                ZclType::U8,
                ZclWrite::CoverLift,
                rep(1, 900, Some(1.0)),
            );
            pos.transforms.from = Some(NativeExpr::new("100-x", |c| {
                c.x.as_i64().map_or(Value::Null, |v| Value::Int(100 - v))
            }));
            pos.transforms.to = pos.transforms.from.clone();
            e.parts.push(("position".into(), pos.clone()));
            let mut feats = vec!["open", "close", "stop", "set_position"];
            if supported(ep, cl::WINDOW_COVERING, 0x0009) {
                let mut tilt = pos;
                tilt.binding = Binding::Zcl(ZclBinding {
                    cluster: cl::WINDOW_COVERING,
                    attr: 0x0009,
                    ty: ZclType::U8,
                    mfr: None,
                    write: ZclWrite::CoverTilt,
                });
                e.parts.push(("tilt".into(), tilt));
                feats.extend(["set_tilt_position"]);
            }
            e.capabilities
                .insert("supported_features".into(), serde_json::json!(feats));
            push(e);
        }
        if has(cl::DOOR_LOCK) {
            let mut e = EntityDef::new("lock", Platform::Lock);
            let mut st = zcl(
                cl::DOOR_LOCK,
                0x0000,
                ZclType::ENUM8,
                ZclWrite::LockCommands,
                rep(0, 900, None),
            );
            st.transforms.map = vec![
                (0, "jammed".into()),
                (1, "locked".into()),
                (2, "unlocked".into()),
            ];
            e.parts.push(("state".into(), st));
            push(e);
            push(lock_event());
        }
        if has(cl::METERING) {
            let unit_of_measure = val(ep, cl::METERING, 0x0300);
            let mut e = EntityDef::new("energy", Platform::Sensor);
            e.state_class = Some(StateClass::TotalIncreasing);
            if unit_of_measure == Some(1) {
                e.device_class = Some("gas".into());
                e.unit = Some("m³".into());
            } else {
                e.device_class = Some("energy".into());
                e.unit = Some("kWh".into());
            }
            e.value = zcl(
                cl::METERING,
                0x0000,
                ZclType::U48,
                ZclWrite::ReadOnly,
                rep(10, 900, Some(1.0)),
            );
            e.value.access = ro();
            e.value.transforms.attr_scale = Some(AttrScale {
                cluster: cl::METERING,
                mul_attr: Some(0x0301),
                div_attr: Some(0x0302),
                factor: 1.0,
            });
            push(e);
            if !has(cl::ELECTRICAL) && supported(ep, cl::METERING, 0x0400) {
                let mut e = EntityDef::new("power", Platform::Sensor);
                e.device_class = Some("power".into());
                e.unit = Some("W".into());
                e.state_class = Some(StateClass::Measurement);
                e.value = zcl(
                    cl::METERING,
                    0x0400,
                    ZclType::I24,
                    ZclWrite::ReadOnly,
                    rep(5, 900, Some(1.0)),
                );
                e.value.access = ro();
                e.value.transforms.attr_scale = Some(AttrScale {
                    cluster: cl::METERING,
                    mul_attr: Some(0x0301),
                    div_attr: Some(0x0302),
                    factor: 1000.0,
                });
                push(e);
            }
        }
        if has(cl::ELECTRICAL) {
            let em = |key: &str,
                      dc: &str,
                      unit: &str,
                      attr: u16,
                      ty: ZclType,
                      scale: Option<(u16, u16)>,
                      report| {
                let mut e = EntityDef::new(key, Platform::Sensor);
                e.device_class = Some(dc.into());
                e.unit = Some(unit.into());
                e.state_class = Some(StateClass::Measurement);
                e.value = zcl(cl::ELECTRICAL, attr, ty, ZclWrite::ReadOnly, report);
                e.value.access = ro();
                e.value.transforms.attr_scale = scale.map(|(m, d)| AttrScale {
                    cluster: cl::ELECTRICAL,
                    mul_attr: Some(m),
                    div_attr: Some(d),
                    factor: 1.0,
                });
                e
            };
            if present(ep, cl::ELECTRICAL, 0x050B) {
                push(em(
                    "power",
                    "power",
                    "W",
                    0x050B,
                    ZclType::I16,
                    Some((0x0604, 0x0605)),
                    rep(5, 900, Some(10.0)),
                ));
            }
            if present(ep, cl::ELECTRICAL, 0x0505) {
                push(em(
                    "voltage",
                    "voltage",
                    "V",
                    0x0505,
                    ZclType::U16,
                    Some((0x0600, 0x0601)),
                    rep(5, 900, Some(10.0)),
                ));
            }
            if present(ep, cl::ELECTRICAL, 0x0508) {
                push(em(
                    "current",
                    "current",
                    "A",
                    0x0508,
                    ZclType::U16,
                    Some((0x0602, 0x0603)),
                    rep(5, 900, Some(10.0)),
                ));
            }
            if supported(ep, cl::ELECTRICAL, 0x0510) {
                push(em(
                    "power_factor",
                    "power_factor",
                    "%",
                    0x0510,
                    ZclType::I8,
                    None,
                    None,
                ));
            }
            if supported(ep, cl::ELECTRICAL, 0x0300) {
                push(em(
                    "frequency",
                    "frequency",
                    "Hz",
                    0x0300,
                    ZclType::U16,
                    Some((0x0400, 0x0401)),
                    None,
                ));
            }
        }
        if has(cl::IDENTIFY) {
            let mut e = EntityDef::new("identify", Platform::Button);
            e.device_class = Some("identify".into());
            e.category = Some(Category::Config);
            e.press = Some(Press::Cmd {
                cluster: cl::IDENTIFY,
                id: 0x00,
                mfr: None,
                ep: Some(ep),
                payload: Some(PressPayload::Bytes(vec![5, 0])),
            });
            push(e);
        }
        for (c, key, platform, ty) in [
            (
                cl::BINARY_INPUT,
                "binary_input",
                Platform::BinarySensor,
                ZclType::BOOL,
            ),
            (
                cl::ANALOG_INPUT,
                "analog_input",
                Platform::Sensor,
                ZclType::SINGLE,
            ),
            (
                cl::MULTISTATE_INPUT,
                "multistate_input",
                Platform::Sensor,
                ZclType::U16,
            ),
        ] {
            if has(c) && present(ep, c, 0x0055) {
                let mut e = EntityDef::new(key, platform);
                e.enabled = false;
                if let Some(ZclValue::String(d)) = a.value(ep, c, 0x001C) {
                    if !d.is_empty() {
                        e.name = Some(d.clone());
                    }
                }
                e.value = zcl(c, 0x0055, ty, ZclWrite::ReadOnly, None);
                e.value.access = ro();
                push(e);
            }
        }
        if has(cl::DIAGNOSTICS) {
            for (attr, key, ty) in [
                (0x011C, "last_message_lqi", ZclType::U8),
                (0x011D, "last_message_rssi", ZclType::I8),
            ] {
                if supported(ep, cl::DIAGNOSTICS, attr) {
                    let mut e = EntityDef::new(key, Platform::Sensor);
                    e.category = Some(Category::Diagnostic);
                    e.enabled = false;
                    e.state_class = Some(StateClass::Measurement);
                    e.value = zcl(cl::DIAGNOSTICS, attr, ty, ZclWrite::ReadOnly, None);
                    e.value.access = ro();
                    push(e);
                }
            }
        }
    }

    if let Some(e) = remote_action(fp) {
        out.push(e);
    }
    out.extend(device_level(0));
    out
}

/// Device-level diagnostic entities on `ep` (0 for Zigbee, 242 for GP).
pub fn device_level(ep: u8) -> Vec<EntityDef> {
    let mut v = Vec::new();
    for (key, dc, unit, f) in [
        ("linkquality", None, None, DeviceField::Linkquality),
        (
            "rssi",
            Some("signal_strength"),
            Some("dBm"),
            DeviceField::Rssi,
        ),
        ("last_seen", Some("timestamp"), None, DeviceField::LastSeen),
    ] {
        let mut e = EntityDef::new(key, Platform::Sensor);
        e.device_class = dc.map(str::to_string);
        e.unit = unit.map(str::to_string);
        if f != DeviceField::LastSeen {
            e.state_class = Some(StateClass::Measurement);
        }
        e.category = Some(Category::Diagnostic);
        e.enabled = false;
        e.ep = Some(ep);
        e.value.binding = Binding::DeviceField(f);
        e.value.access = ro();
        v.push(e);
    }
    v
}

fn light(ep: u8, epd: &crate::fingerprint::EndpointFp, a: &dyn AttrView) -> EntityDef {
    let mut e = EntityDef::new("light", Platform::Light);
    e.parts.push((
        "on_off".into(),
        zcl(
            cl::ON_OFF,
            0,
            ZclType::BOOL,
            ZclWrite::OnOffCommands,
            rep(0, 900, None),
        ),
    ));
    let mut modes: Vec<&str> = Vec::new();
    if epd.in_clusters.contains(&cl::LEVEL) {
        e.parts.push((
            "brightness".into(),
            zcl(
                cl::LEVEL,
                0,
                ZclType::U8,
                ZclWrite::MoveToLevel,
                rep(1, 900, Some(1.0)),
            ),
        ));
    }
    if epd.in_clusters.contains(&cl::COLOR) {
        let caps = match a.support(ep, cl::COLOR, 0x400A) {
            Support::Supported => a
                .value(ep, cl::COLOR, 0x400A)
                .and_then(ZclValue::as_i64)
                .unwrap_or(0x18),
            _ => 0x18,
        };
        if caps & 0x10 != 0 {
            let mut ct = zcl(
                cl::COLOR,
                0x0007,
                ZclType::U16,
                ZclWrite::MoveToColorTemp,
                rep(1, 900, Some(1.0)),
            );
            let inv = NativeExpr::new("mireds<->kelvin", |c| match c.x.as_f64() {
                Some(v) if v > 0.0 => Value::Int((1_000_000.0 / v).round() as i64),
                _ => Value::Null,
            });
            ct.transforms.from = Some(inv.clone());
            ct.transforms.to = Some(inv);
            e.parts.push(("color_temp".into(), ct));
            modes.push("color_temp");
            let mired =
                |attr, dflt: f64| match a.value(ep, cl::COLOR, attr).and_then(ZclValue::as_i64) {
                    Some(v) if v > 0 && v < 0xFFFF => v as f64,
                    _ => dflt,
                };
            let (min_m, max_m) = (mired(0x400B, 153.0), mired(0x400C, 500.0));
            e.capabilities.insert(
                "min_color_temp_kelvin".into(),
                ((1_000_000.0 / max_m).round() as i64).into(),
            );
            e.capabilities.insert(
                "max_color_temp_kelvin".into(),
                ((1_000_000.0 / min_m).round() as i64).into(),
            );
        }
        if caps & 0x08 != 0 {
            for (n, attr, w) in [
                ("color_x", 0x0003, ZclWrite::ColorX),
                ("color_y", 0x0004, ZclWrite::ColorY),
            ] {
                let mut v = zcl(cl::COLOR, attr, ZclType::U16, w, rep(1, 900, Some(1.0)));
                v.transforms.div = Some(65535.0);
                v.transforms.precision = Some(4);
                e.parts.push((n.into(), v));
            }
            modes.push("xy");
        }
        if caps & 0x01 != 0 {
            let mut h = zcl(
                cl::COLOR,
                0x0000,
                ZclType::U8,
                ZclWrite::Hue,
                rep(1, 900, Some(1.0)),
            );
            h.transforms.mul = Some(360.0);
            h.transforms.div = Some(254.0);
            h.transforms.precision = Some(1);
            let mut s = zcl(
                cl::COLOR,
                0x0001,
                ZclType::U8,
                ZclWrite::Saturation,
                rep(1, 900, Some(1.0)),
            );
            s.transforms.mul = Some(100.0);
            s.transforms.div = Some(254.0);
            s.transforms.precision = Some(1);
            e.parts.push(("hue".into(), h));
            e.parts.push(("saturation".into(), s));
            modes.push("hs");
        }
        let mut cm = zcl(cl::COLOR, 0x0008, ZclType::ENUM8, ZclWrite::ReadOnly, None);
        cm.transforms.map = vec![(0, "hs".into()), (1, "xy".into()), (2, "color_temp".into())];
        cm.access = ro();
        e.parts.push(("color_mode".into(), cm));
    }
    if modes.is_empty() {
        modes.push(if epd.in_clusters.contains(&cl::LEVEL) {
            "brightness"
        } else {
            "onoff"
        });
    }
    e.capabilities
        .insert("supported_color_modes".into(), serde_json::json!(modes));
    e.capabilities.insert(
        "supported_features".into(),
        serde_json::json!(["transition"]),
    );
    e
}

fn thermostat(ep: u8, a: &dyn AttrView) -> EntityDef {
    let mut e = EntityDef::new("thermostat", Platform::Climate);
    let t = |attr, report| {
        let mut v = zcl(
            cl::THERMOSTAT,
            attr,
            ZclType::I16,
            ZclWrite::Attribute,
            report,
        );
        v.transforms.div = Some(100.0);
        v.transforms.invalid = Some(-32768.0);
        v
    };
    let seq = a
        .value(ep, cl::THERMOSTAT, 0x001B)
        .and_then(ZclValue::as_i64);
    let cooling_only = matches!(seq, Some(0x00 | 0x01));
    let mut cur = t(0x0000, rep(30, 900, Some(10.0)));
    cur.access = ro();
    e.parts.push(("current_temperature".into(), cur));
    let mut target = t(
        if cooling_only { 0x0011 } else { 0x0012 },
        rep(0, 900, Some(1.0)),
    );
    let lim = |attr, d: f64| {
        a.value(ep, cl::THERMOSTAT, attr)
            .and_then(ZclValue::as_i64)
            .map_or(d, |v| v as f64 / 100.0)
    };
    target.limits.min = Some(lim(0x0015, 7.0));
    target.limits.max = Some(lim(0x0016, 30.0));
    target.limits.step = Some(0.5);
    e.parts.push(("target_temperature".into(), target));
    let mut mode = zcl(
        cl::THERMOSTAT,
        0x001C,
        ZclType::ENUM8,
        ZclWrite::Attribute,
        rep(0, 900, None),
    );
    mode.transforms.map = vec![
        (0, "off".into()),
        (1, "auto".into()),
        (3, "cool".into()),
        (4, "heat".into()),
        (7, "fan_only".into()),
        (8, "dry".into()),
    ];
    e.parts.push(("hvac_mode".into(), mode));
    let mut action = zcl(
        cl::THERMOSTAT,
        0x0029,
        ZclType::MAP16,
        ZclWrite::ReadOnly,
        rep(0, 900, None),
    );
    action.access = ro();
    action.transforms.from = Some(NativeExpr::new("running_state", |c| match c.x.as_i64() {
        Some(v) if v & 0x09 != 0 => Value::Str("heating".into()),
        Some(v) if v & 0x12 != 0 => Value::Str("cooling".into()),
        Some(v) if v & 0x64 != 0 => Value::Str("fan".into()),
        Some(_) => Value::Str("idle".into()),
        None => Value::Null,
    }));
    e.parts.push(("hvac_action".into(), action));
    let modes: Vec<&str> = match seq {
        Some(0x00 | 0x01) => vec!["off", "cool"],
        Some(0x04 | 0x05) => vec!["off", "heat", "cool", "auto"],
        _ => vec!["off", "heat"],
    };
    e.capabilities
        .insert("hvac_modes".into(), serde_json::json!(modes));
    e
}

fn fan(ep: u8, a: &dyn AttrView) -> EntityDef {
    let mut e = EntityDef::new("fan", Platform::Fan);
    let mut m = zcl(
        cl::FAN_CONTROL,
        0x0000,
        ZclType::ENUM8,
        ZclWrite::Attribute,
        rep(0, 900, None),
    );
    m.transforms.map = vec![
        (0, "off".into()),
        (1, "low".into()),
        (2, "medium".into()),
        (3, "high".into()),
        (4, "on".into()),
        (5, "auto".into()),
        (6, "smart".into()),
    ];
    e.parts.push(("fan_mode".into(), m));
    let seq = a
        .value(ep, cl::FAN_CONTROL, 0x0001)
        .and_then(ZclValue::as_i64);
    let (speeds, presets): (u8, Vec<&str>) = match seq {
        Some(1) => (2, vec![]),
        Some(2) => (3, vec!["auto"]),
        Some(3) => (2, vec!["auto"]),
        Some(4) => (1, vec!["auto"]),
        _ => (3, vec![]),
    };
    e.capabilities.insert("speed_count".into(), speeds.into());
    e.capabilities
        .insert("preset_modes".into(), serde_json::json!(presets));
    e
}

fn lock_event() -> EntityDef {
    let mut e = EntityDef::new("lock_event", Platform::Event);
    const CODES: [&str; 15] = [
        "unknown",
        "lock",
        "unlock",
        "lock_failure_invalid_pin_or_id",
        "lock_failure_invalid_schedule",
        "unlock_failure_invalid_pin_or_id",
        "unlock_failure_invalid_schedule",
        "one_touch_lock",
        "key_lock",
        "key_unlock",
        "auto_lock",
        "schedule_lock",
        "schedule_unlock",
        "manual_lock",
        "manual_unlock",
    ];
    e.event_types = CODES.iter().map(|s| s.to_string()).collect();
    e.triggers.push(Trigger {
        source: TriggerSource::Cmd {
            ep: None,
            cluster: cl::DOOR_LOCK,
            id: 0x20,
            mfr: None,
            global: false,
        },
        when: None,
        event: EventSel::Expr(NativeExpr::new("lock_event", |c| {
            c.payload
                .and_then(|p| p.get(1))
                .and_then(|&code| CODES.get(usize::from(code)))
                .map_or(Value::Null, |s| Value::Str((*s).into()))
        })),
        attrs: vec![
            ("source".into(), payload_u8(0)),
            (
                "user_id".into(),
                NativeExpr::new("u16le(2)", |c| u16_at(c.payload, 2)),
            ),
        ],
    });
    e
}

fn payload_u8(i: usize) -> crate::expr::Expr {
    NativeExpr::new("payload[i]", move |c| {
        c.payload
            .and_then(|p| p.get(i))
            .map_or(Value::Null, |&b| Value::Int(i64::from(b)))
    })
}

fn u16_at(p: Option<&[u8]>, i: usize) -> Value {
    p.and_then(|p| p.get(i..i + 2)).map_or(Value::Null, |b| {
        Value::Int(i64::from(u16::from_le_bytes([b[0], b[1]])))
    })
}

fn trig(
    cluster: u16,
    id: u8,
    ev: crate::expr::Expr,
    attrs: Vec<(&str, crate::expr::Expr)>,
) -> Trigger {
    Trigger {
        source: TriggerSource::Cmd {
            ep: None,
            cluster,
            id,
            mfr: None,
            global: false,
        },
        when: None,
        event: EventSel::Expr(ev),
        attrs: attrs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
    }
}

fn fixed(cluster: u16, id: u8, ev: &str) -> Trigger {
    Trigger {
        source: TriggerSource::Cmd {
            ep: None,
            cluster,
            id,
            mfr: None,
            global: false,
        },
        when: None,
        event: EventSel::Fixed(ev.to_string()),
        attrs: Vec::new(),
    }
}

fn updown(up: &'static str, down: &'static str, down_mode: u8) -> crate::expr::Expr {
    // Level Control: mode 0 up, 1 down; Color Control: 1 up, 3 down.
    let up_mode = if down_mode == 1 { 0 } else { 1 };
    NativeExpr::new("mode_updown", move |c| {
        match c.payload.and_then(|p| p.first()).copied() {
            Some(m) if m == up_mode => Value::Str(up.into()),
            Some(m) if m == down_mode => Value::Str(down.into()),
            _ => Value::Null,
        }
    })
}

/// The `action` event entity of a remote (5.4).
pub fn remote_action(fp: &Fingerprint) -> Option<EntityDef> {
    const EVENT_CLUSTERS: [u16; 6] = [
        cl::ON_OFF,
        cl::LEVEL,
        cl::SCENES,
        cl::COLOR,
        cl::WINDOW_COVERING,
        cl::IAS_ACE,
    ];
    let eps: Vec<_> = fp
        .endpoints
        .iter()
        .filter(|e| {
            e.profile != PROFILE_GP && e.out_clusters.iter().any(|c| EVENT_CLUSTERS.contains(c))
        })
        .collect();
    let first = eps.iter().map(|e| e.ep).min()?;
    let clusters: Vec<u16> = EVENT_CLUSTERS
        .into_iter()
        .filter(|c| eps.iter().any(|e| e.out_clusters.contains(c)))
        .collect();
    let mut e = EntityDef::new("action", Platform::Event);
    e.device_class = Some("button".into());
    e.ep = Some(first);
    let mut types: Vec<&str> = Vec::new();
    for c in clusters {
        match c {
            cl::ON_OFF => {
                types.extend(["off", "on", "toggle"]);
                for (id, ev) in [
                    (0x00, "off"),
                    (0x01, "on"),
                    (0x02, "toggle"),
                    (0x40, "off"),
                    (0x42, "on"),
                ] {
                    e.triggers.push(fixed(c, id, ev));
                }
            }
            cl::LEVEL => {
                types.extend([
                    "brightness_move_up",
                    "brightness_move_down",
                    "brightness_step_up",
                    "brightness_step_down",
                    "brightness_stop",
                    "brightness_move_to_level",
                ]);
                for id in [0x01, 0x05] {
                    e.triggers.push(trig(
                        c,
                        id,
                        updown("brightness_move_up", "brightness_move_down", 1),
                        vec![("rate", payload_u8(1))],
                    ));
                }
                for id in [0x02, 0x06] {
                    e.triggers.push(trig(
                        c,
                        id,
                        updown("brightness_step_up", "brightness_step_down", 1),
                        vec![
                            ("step_size", payload_u8(1)),
                            (
                                "transition_time",
                                NativeExpr::new("u16le(2)", |c| u16_at(c.payload, 2)),
                            ),
                        ],
                    ));
                }
                for id in [0x03, 0x07] {
                    e.triggers.push(fixed(c, id, "brightness_stop"));
                }
                for id in [0x00, 0x04] {
                    e.triggers.push(trig(
                        c,
                        id,
                        NativeExpr::new("move_to_level", |_| {
                            Value::Str("brightness_move_to_level".into())
                        }),
                        vec![
                            ("level", payload_u8(0)),
                            (
                                "transition_time",
                                NativeExpr::new("u16le(1)", |c| u16_at(c.payload, 1)),
                            ),
                        ],
                    ));
                }
            }
            cl::SCENES => {
                types.push("recall_scene");
                e.triggers.push(trig(
                    c,
                    0x05,
                    NativeExpr::new("recall_scene", |_| Value::Str("recall_scene".into())),
                    vec![
                        (
                            "group",
                            NativeExpr::new("u16le(0)", |c| u16_at(c.payload, 0)),
                        ),
                        ("scene", payload_u8(2)),
                    ],
                ));
            }
            cl::COLOR => {
                types.extend([
                    "color_temperature_move_up",
                    "color_temperature_move_down",
                    "color_temperature_move_stop",
                    "color_temperature_step_up",
                    "color_temperature_step_down",
                    "color_temperature_move",
                ]);
                e.triggers.push(trig(
                    c,
                    0x4B,
                    NativeExpr::new("ct_move", |c| match c.payload.and_then(|p| p.first()) {
                        Some(0) => Value::Str("color_temperature_move_stop".into()),
                        Some(1) => Value::Str("color_temperature_move_up".into()),
                        Some(3) => Value::Str("color_temperature_move_down".into()),
                        _ => Value::Null,
                    }),
                    vec![(
                        "rate",
                        NativeExpr::new("u16le(1)", |c| u16_at(c.payload, 1)),
                    )],
                ));
                e.triggers.push(trig(
                    c,
                    0x4C,
                    updown(
                        "color_temperature_step_up",
                        "color_temperature_step_down",
                        3,
                    ),
                    vec![(
                        "step_size",
                        NativeExpr::new("u16le(1)", |c| u16_at(c.payload, 1)),
                    )],
                ));
                e.triggers.push(trig(
                    c,
                    0x0A,
                    NativeExpr::new("ct", |_| Value::Str("color_temperature_move".into())),
                    vec![(
                        "color_temp",
                        NativeExpr::new("u16le(0)", |c| u16_at(c.payload, 0)),
                    )],
                ));
            }
            cl::WINDOW_COVERING => {
                types.extend(["open", "close", "stop"]);
                for (id, ev) in [(0x00, "open"), (0x01, "close"), (0x02, "stop")] {
                    e.triggers.push(fixed(c, id, ev));
                }
            }
            cl::IAS_ACE => {
                types.extend([
                    "arm_disarm",
                    "arm_day",
                    "arm_night",
                    "arm_all",
                    "emergency",
                    "fire",
                    "panic",
                ]);
                e.triggers.push(trig(
                    c,
                    0x00,
                    NativeExpr::new("arm", |c| match c.payload.and_then(|p| p.first()) {
                        Some(0) => Value::Str("arm_disarm".into()),
                        Some(1) => Value::Str("arm_day".into()),
                        Some(2) => Value::Str("arm_night".into()),
                        Some(3) => Value::Str("arm_all".into()),
                        _ => Value::Null,
                    }),
                    vec![],
                ));
                for (id, ev) in [(0x02, "emergency"), (0x03, "fire"), (0x04, "panic")] {
                    e.triggers.push(fixed(c, id, ev));
                }
            }
            _ => {}
        }
    }
    e.event_types = types.into_iter().map(str::to_string).collect();
    Some(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::EndpointFp;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Attrs(BTreeMap<(u8, u16, u16), Option<ZclValue>>);
    impl AttrView for Attrs {
        fn support(&self, ep: u8, c: u16, a: u16) -> Support {
            match self.0.get(&(ep, c, a)) {
                None => Support::Unknown,
                Some(None) => Support::Unsupported,
                Some(Some(_)) => Support::Supported,
            }
        }
        fn value(&self, ep: u8, c: u16, a: u16) -> Option<&ZclValue> {
            self.0.get(&(ep, c, a)).and_then(Option::as_ref)
        }
    }

    fn fp(in_clusters: &[u16], out_clusters: &[u16], device_id: u16) -> Fingerprint {
        Fingerprint {
            endpoints: vec![EndpointFp {
                ep: 1,
                profile: 0x0104,
                device_id,
                in_clusters: in_clusters.to_vec(),
                out_clusters: out_clusters.to_vec(),
            }],
            ..Default::default()
        }
    }

    fn keys(v: &[EntityDef]) -> Vec<&str> {
        v.iter().map(|e| e.key.as_str()).collect()
    }

    #[test]
    fn sensor_and_light_rules() {
        let a = Attrs::default();
        let inp = GenericInput {
            attrs: &a,
            power: PowerKind::Battery,
        };
        let v = generic_entities(
            &fp(&[0x0000, 0x0001, 0x0402, 0x0405], &[0x0019], 0x0302),
            &inp,
        );
        // Battery % is optional: only after a successful read.
        assert_eq!(
            keys(&v),
            [
                "temperature",
                "humidity",
                "linkquality",
                "rssi",
                "last_seen"
            ]
        );
        let mut a = Attrs::default();
        a.0.insert((1, 1, 0x0021), Some(ZclValue::Uint(200)));
        let inp = GenericInput {
            attrs: &a,
            power: PowerKind::Battery,
        };
        let v = generic_entities(&fp(&[0x0001, 0x0402], &[], 0x0302), &inp);
        assert!(keys(&v).contains(&"battery"));

        let a = Attrs::default();
        let inp = GenericInput {
            attrs: &a,
            power: PowerKind::Mains,
        };
        let v = generic_entities(&fp(&[0x0003, 0x0006, 0x0008, 0x0300], &[], 0x010D), &inp);
        let l = v.iter().find(|e| e.key == "light").unwrap();
        let parts: Vec<_> = l.parts.iter().map(|p| p.0.as_str()).collect();
        assert_eq!(
            parts,
            [
                "on_off",
                "brightness",
                "color_temp",
                "color_x",
                "color_y",
                "color_mode"
            ]
        );
        assert_eq!(
            l.capabilities["supported_color_modes"],
            serde_json::json!(["color_temp", "xy"])
        );
        assert!(keys(&v).contains(&"identify"));
        let v = generic_entities(&fp(&[0x0006], &[], 0x0051), &inp);
        assert_eq!(v[0].device_class.as_deref(), Some("outlet"));
    }

    #[test]
    fn remote_events() {
        let a = Attrs::default();
        let inp = GenericInput {
            attrs: &a,
            power: PowerKind::Battery,
        };
        let v = generic_entities(
            &fp(&[0x0000, 0x0001], &[0x0006, 0x0008, 0x0005], 0x0820),
            &inp,
        );
        let act = v.iter().find(|e| e.key == "action").unwrap();
        assert!(act.event_types.contains(&"brightness_step_up".to_string()));
        assert!(act.event_types.contains(&"recall_scene".to_string()));
    }
}
