//! Reporting configuration (spec 5.2, 5.3): the editable global defaults
//! per attribute class, per-entity user overrides, their validation, and
//! the readback of what a device applied (ZCL Read Reporting
//! Configuration, 0x08).
//!
//! Precedence for one value: the entity's override (layer 5) > the
//! definition's `report` (a definition entity, or `generic.patch`) > the
//! edited global default of the attribute's class > the built-in table of
//! 5.2. The global defaults apply where the built-in table does: to the
//! generic tier's values. Both kinds of user setting end up in the
//! Configure Reporting op keys, so a change changes the configure hash
//! and the device is reconfigured (mains at once, sleepy at the next
//! contact). `off` set by the user sends Configure Reporting with the
//! maximum interval 0xFFFF (ZCL: stop reporting) and the hub never polls
//! that attribute; `off` in a definition sends nothing (unchanged).

use std::collections::BTreeMap;

use serde_json::{Map, Value as J, json};

use crate::device::{Device, Entity, InterviewStep, Millis, ts};
use crate::hub::{ApiError, Hub, HubEvent, Purpose, device_of_uid};
use crate::model::{Binding, EntityDef, LayerSource, PowerKind, ReportSpec, ValueDef};
use crate::transport::ZigbeeTransport;
use crate::zcl::frame::{self as zf, ReportingConfig, global};
use crate::zcl::{FrameType, Status, ZclFrame, ZclType, cluster as cl};

/// Host store key of the edited global defaults: JSON `{class: setting}`
/// with the edited classes only (absent while nothing is edited).
pub const DEFAULTS_KEY: &str = "cfg/report";

/// Largest maximum interval a user may set; 0xFFFF tells the device to
/// stop reporting (ZCL r8 2.5.7.1.6), which is what `off` sends.
pub const MAX_INTERVAL: u16 = 0xFFFE;

/// A user's reporting setting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReportSetting {
    /// `[min, max, change]`: intervals in s, change in raw attribute units
    /// (analog types only; absent: 0).
    Config {
        /// Minimum interval.
        min: u16,
        /// Maximum interval.
        max: u16,
        /// Reportable change.
        change: Option<f64>,
    },
    /// `off`: the device is told to stop reporting; never polled.
    Off,
}

const fn cfg(min: u16, max: u16, change: Option<f64>) -> ReportSetting {
    ReportSetting::Config { min, max, change }
}

fn bad(m: impl Into<String>) -> ApiError {
    ApiError::new("invalid_value", m)
}

/// The largest reportable change a type can carry (`None`: floats).
fn change_limit(t: ZclType) -> Option<f64> {
    if let Some(w) = t.uint_width() {
        return Some(2f64.powi(8 * w as i32) - 1.0);
    }
    if let Some(w) = t.int_width() {
        return Some(2f64.powi(8 * w as i32 - 1) - 1.0);
    }
    (!t.is_float()).then_some(f64::from(u32::MAX))
}

impl ReportSetting {
    /// `[min, max]`, `[min, max, change]` or `"off"`; `"default"` and null
    /// give `None` (back to the inherited setting).
    pub fn parse(v: &J) -> Result<Option<Self>, String> {
        let int = |x: &J, n: &str| {
            x.as_u64()
                .and_then(|v| u16::try_from(v).ok())
                .ok_or_else(|| format!("{n}: an integer 0–{MAX_INTERVAL}"))
        };
        match v {
            J::Null => Ok(None),
            J::String(s) if s == "default" => Ok(None),
            J::String(s) if s == "off" => Ok(Some(Self::Off)),
            J::Array(a) if (2..=3).contains(&a.len()) => Ok(Some(Self::Config {
                min: int(&a[0], "min")?,
                max: int(&a[1], "max")?,
                change: match a.get(2) {
                    None | Some(J::Null) => None,
                    Some(c) => Some(c.as_f64().ok_or("change: a number")?),
                },
            })),
            _ => Err("report: [min, max, change], \"off\" or \"default\"".into()),
        }
    }

    /// Checks a setting for attributes of types `types`; `cap` is the
    /// battery cap of `max` (5.2).
    pub fn check(self, types: &[ZclType], cap: Option<u16>) -> Result<(), String> {
        let Self::Config { min, max, change } = self else {
            return Ok(());
        };
        if max > MAX_INTERVAL {
            return Err(format!(
                "max {max} above {MAX_INTERVAL} (0xFFFF stops reporting: use \"off\")"
            ));
        }
        if min > max {
            return Err(format!("min {min} above max {max}"));
        }
        if let Some(c) = cap.filter(|c| max > *c) {
            return Err(format!(
                "max {max} above {c} s, the cap of a battery device"
            ));
        }
        if let Some(c) = change {
            if !c.is_finite() || c < 0.0 {
                return Err("change: a non-negative number".into());
            }
            for &t in types {
                if !t.is_analog() {
                    return Err(format!(
                        "change: type {t} is discrete (no reportable change)"
                    ));
                }
                if change_limit(t).is_some_and(|l| c > l) {
                    return Err(format!("change {c} does not fit type {t}"));
                }
            }
        }
        Ok(())
    }

    /// API and store form (an integral change as an integer).
    pub fn to_json(self) -> J {
        match self {
            Self::Config {
                min,
                max,
                change: Some(c),
            } if c.fract() == 0.0 && c.abs() < 9e15 => json!([min, max, c as i64]),
            Self::Config {
                min,
                max,
                change: Some(c),
            } => json!([min, max, c]),
            Self::Config { min, max, .. } => json!([min, max]),
            Self::Off => "off".into(),
        }
    }

    /// The resolved form.
    pub fn spec(self) -> ReportSpec {
        match self {
            Self::Config { min, max, change } => ReportSpec::Config { min, max, change },
            Self::Off => ReportSpec::Disable,
        }
    }
}

impl serde::Serialize for ReportSetting {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(s)
    }
}

impl<'de> serde::Deserialize<'de> for ReportSetting {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(&J::deserialize(d)?)
            .map_err(serde::de::Error::custom)?
            .ok_or_else(|| serde::de::Error::custom("report: a setting"))
    }
}

/// `max` as sent: battery devices cap it at `cap` (but not below `min`).
pub fn capped(min: u16, max: u16, cap: Option<u16>) -> u16 {
    cap.map_or(max, |c| max.min(c.max(min)))
}

/// The battery cap of the maximum interval (5.2): the check-in interval or
/// 6 h, whichever is shorter; `None` for mains devices.
pub fn max_cap(d: &Device) -> Option<u16> {
    (d.power() != PowerKind::Mains || d.sleepy()).then(|| {
        d.resolution
            .device
            .checkin_s
            .map_or(21600, |c| c.min(21600)) as u16
    })
}

/// A resolved reporting as the API shows it: `[min, max, change]` (capped
/// as sent), `"off"` (the user's), or null (no Configure Reporting).
pub fn spec_json(s: Option<ReportSpec>, cap: Option<u16>) -> J {
    match s {
        Some(ReportSpec::Config { min, max, change }) => {
            cfg(min, capped(min, max, cap), change).to_json()
        }
        Some(ReportSpec::Disable) => "off".into(),
        _ => J::Null,
    }
}

/// One row of the global defaults: a class of attributes (5.2).
#[derive(Debug)]
pub struct ReportClass {
    /// Key in the API and the store.
    pub key: &'static str,
    /// `(cluster, attribute, type)` of the generic tier.
    pub attrs: &'static [(u16, u16, ZclType)],
    /// Built-in default.
    pub builtin: ReportSetting,
}

const fn class(
    key: &'static str,
    attrs: &'static [(u16, u16, ZclType)],
    builtin: ReportSetting,
) -> ReportClass {
    ReportClass {
        key,
        attrs,
        builtin,
    }
}

/// The editable classes: the rows of the 5.2 table.
pub const CLASSES: &[ReportClass] = &[
    class(
        "on_off",
        &[(cl::ON_OFF, 0, ZclType::BOOL)],
        cfg(0, 900, None),
    ),
    class(
        "level",
        &[(cl::LEVEL, 0, ZclType::U8)],
        cfg(1, 900, Some(1.0)),
    ),
    class(
        "color",
        &[
            (cl::COLOR, 0x0007, ZclType::U16),
            (cl::COLOR, 0x0003, ZclType::U16),
            (cl::COLOR, 0x0004, ZclType::U16),
            (cl::COLOR, 0x0000, ZclType::U8),
            (cl::COLOR, 0x0001, ZclType::U8),
        ],
        cfg(1, 900, Some(1.0)),
    ),
    class(
        "temperature",
        &[(cl::TEMPERATURE, 0, ZclType::I16)],
        cfg(30, 900, Some(50.0)),
    ),
    class(
        "humidity",
        &[(cl::HUMIDITY, 0, ZclType::U16)],
        cfg(30, 900, Some(100.0)),
    ),
    class(
        "pressure",
        &[(cl::PRESSURE, 0, ZclType::I16)],
        cfg(30, 900, Some(1.0)),
    ),
    class(
        "illuminance",
        &[(cl::ILLUMINANCE, 0, ZclType::U16)],
        cfg(30, 900, Some(500.0)),
    ),
    class(
        "occupancy",
        &[(cl::OCCUPANCY, 0, ZclType::MAP8)],
        cfg(0, 900, None),
    ),
    class(
        "soil_moisture",
        &[(cl::SOIL_MOISTURE, 0, ZclType::U16)],
        cfg(30, 900, Some(100.0)),
    ),
    class(
        "co2",
        &[(cl::CO2, 0, ZclType::SINGLE)],
        cfg(30, 900, Some(0.000_025)),
    ),
    class(
        "pm25",
        &[(cl::PM25, 0, ZclType::SINGLE)],
        cfg(30, 900, Some(1.0)),
    ),
    class(
        "pm1",
        &[(cl::PM1, 0, ZclType::SINGLE)],
        cfg(30, 900, Some(1.0)),
    ),
    class(
        "pm10",
        &[(cl::PM10, 0, ZclType::SINGLE)],
        cfg(30, 900, Some(1.0)),
    ),
    class(
        "voc",
        &[(cl::TVOC, 0, ZclType::SINGLE)],
        cfg(30, 900, Some(0.000_000_1)),
    ),
    class(
        "formaldehyde",
        &[(cl::FORMALDEHYDE, 0, ZclType::SINGLE)],
        cfg(30, 900, Some(0.000_000_01)),
    ),
    class(
        "battery_pct",
        &[(cl::POWER_CONFIG, 0x0021, ZclType::U8)],
        cfg(3600, 21600, Some(2.0)),
    ),
    class(
        "battery_voltage",
        &[(cl::POWER_CONFIG, 0x0020, ZclType::U8)],
        cfg(3600, 21600, Some(1.0)),
    ),
    class(
        "power",
        &[(cl::ELECTRICAL, 0x050B, ZclType::I16)],
        cfg(5, 900, Some(10.0)),
    ),
    class(
        "voltage",
        &[(cl::ELECTRICAL, 0x0505, ZclType::U16)],
        cfg(5, 900, Some(10.0)),
    ),
    class(
        "current",
        &[(cl::ELECTRICAL, 0x0508, ZclType::U16)],
        cfg(5, 900, Some(10.0)),
    ),
    class(
        "energy",
        &[(cl::METERING, 0x0000, ZclType::U48)],
        cfg(10, 900, Some(1.0)),
    ),
    class(
        "demand",
        &[(cl::METERING, 0x0400, ZclType::I24)],
        cfg(5, 900, Some(1.0)),
    ),
    class(
        "local_temperature",
        &[(cl::THERMOSTAT, 0x0000, ZclType::I16)],
        cfg(30, 900, Some(10.0)),
    ),
    class(
        "setpoint",
        &[
            (cl::THERMOSTAT, 0x0011, ZclType::I16),
            (cl::THERMOSTAT, 0x0012, ZclType::I16),
        ],
        cfg(0, 900, Some(1.0)),
    ),
    class(
        "thermostat_state",
        &[
            (cl::THERMOSTAT, 0x001C, ZclType::ENUM8),
            (cl::THERMOSTAT, 0x0029, ZclType::MAP16),
        ],
        cfg(0, 900, None),
    ),
    class(
        "pi_heating_demand",
        &[(cl::THERMOSTAT, 0x0008, ZclType::U8)],
        cfg(30, 900, Some(5.0)),
    ),
    class(
        "cover",
        &[
            (cl::WINDOW_COVERING, 0x0008, ZclType::U8),
            (cl::WINDOW_COVERING, 0x0009, ZclType::U8),
        ],
        cfg(1, 900, Some(1.0)),
    ),
    class(
        "lock",
        &[(cl::DOOR_LOCK, 0, ZclType::ENUM8)],
        cfg(0, 900, None),
    ),
    class(
        "fan",
        &[(cl::FAN_CONTROL, 0, ZclType::ENUM8)],
        cfg(0, 900, None),
    ),
];

/// The class of a standard attribute.
pub fn class_of(cluster: u16, attr: u16) -> Option<&'static ReportClass> {
    CLASSES
        .iter()
        .find(|c| c.attrs.iter().any(|a| a.0 == cluster && a.1 == attr))
}

fn values_mut(e: &mut EntityDef) -> impl Iterator<Item = &mut ValueDef> {
    std::iter::once(&mut e.value).chain(e.parts.iter_mut().map(|(_, v)| v))
}

/// The global defaults: edits of the built-in table by class.
#[derive(Debug, Clone, Default)]
pub struct ReportDefaults {
    edits: BTreeMap<&'static str, ReportSetting>,
    /// Bumped on every change (the store writes when it moved).
    pub generation: u64,
}

impl ReportDefaults {
    /// Nothing edited.
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    /// The edited setting of a class.
    pub fn edit(&self, key: &str) -> Option<ReportSetting> {
        self.edits.get(key).copied()
    }

    /// Applies the edits to the generic tier's entities (values with a
    /// built-in reporting of an edited class).
    pub fn apply(&self, entities: &mut [EntityDef]) {
        if self.edits.is_empty() {
            return;
        }
        for v in entities.iter_mut().flat_map(values_mut) {
            let Binding::Zcl(z) = &v.binding else {
                continue;
            };
            if v.report.is_none() || z.mfr.is_some() {
                continue;
            }
            if let Some(s) = class_of(z.cluster, z.attr).and_then(|c| self.edits.get(c.key)) {
                v.report = Some(s.spec());
            }
        }
    }

    /// Store form (`DEFAULTS_KEY`).
    pub fn to_bytes(&self) -> Vec<u8> {
        let m: Map<String, J> = self
            .edits
            .iter()
            .map(|(k, s)| ((*k).to_string(), s.to_json()))
            .collect();
        serde_json::to_vec(&m).unwrap_or_default()
    }

    /// Reads the store form; classes this build does not know and invalid
    /// settings are skipped.
    pub fn from_bytes(b: &[u8]) -> Result<Self, String> {
        let m: Map<String, J> = serde_json::from_slice(b).map_err(|e| e.to_string())?;
        let mut d = Self::default();
        for (k, v) in &m {
            let Some(c) = CLASSES.iter().find(|c| c.key == k) else {
                continue;
            };
            if let Ok(Some(s)) = ReportSetting::parse(v) {
                if check_class(c, s).is_ok() {
                    d.edits.insert(c.key, s);
                }
            }
        }
        Ok(d)
    }

    /// `reporting/defaults/get`.
    pub fn record(&self) -> J {
        let classes: Vec<J> = CLASSES
            .iter()
            .map(|c| {
                let attrs: Vec<J> = c.attrs.iter().map(|a| json!([a.0, a.1])).collect();
                let mut o = Map::new();
                o.insert("key".into(), c.key.into());
                o.insert("attributes".into(), J::Array(attrs));
                o.insert("builtin".into(), c.builtin.to_json());
                o.insert(
                    "report".into(),
                    self.edits
                        .get(c.key)
                        .copied()
                        .unwrap_or(c.builtin)
                        .to_json(),
                );
                o.insert("edited".into(), self.edits.contains_key(c.key).into());
                J::Object(o)
            })
            .collect();
        json!({ "classes": classes })
    }

    /// `reporting/defaults/set`: `{class: setting | "default" | null}`,
    /// all checked before any is applied; `true` when something changed.
    pub fn update(&mut self, changes: &J) -> Result<bool, String> {
        let m = changes
            .as_object()
            .ok_or("defaults: an object {class: setting}")?;
        let mut next = self.edits.clone();
        for (k, v) in m {
            let c = CLASSES
                .iter()
                .find(|c| c.key == k)
                .ok_or_else(|| format!("unknown class {k}"))?;
            match ReportSetting::parse(v).map_err(|e| format!("{k}: {e}"))? {
                Some(s) if s != c.builtin => {
                    check_class(c, s).map_err(|e| format!("{k}: {e}"))?;
                    next.insert(c.key, s);
                }
                _ => {
                    next.remove(c.key);
                }
            }
        }
        let changed = next != self.edits;
        self.edits = next;
        Ok(changed)
    }
}

fn check_class(c: &ReportClass, s: ReportSetting) -> Result<(), String> {
    let types: Vec<ZclType> = c.attrs.iter().map(|a| a.2).collect();
    s.check(&types, None)
}

impl Entity {
    /// The reporting of one of its values: the user's override, else the
    /// resolved one.
    pub fn report_of(&self, part: Option<&str>, v: &ValueDef) -> Option<ReportSpec> {
        match self.user_report.get(part.unwrap_or("")) {
            Some(s) => Some(s.spec()),
            None => v.report,
        }
    }

    /// The overrides as the entity record shows them (`""`: the own value).
    pub fn report_json(&self) -> J {
        J::Object(
            self.user_report
                .iter()
                .map(|(k, s)| (k.clone(), s.to_json()))
                .collect(),
        )
    }
}

/// A ZCL value that can carry a reporting configuration: (part, endpoint,
/// cluster, attribute, type, manufacturer, value).
type Reportable<'a> = (
    Option<&'a str>,
    u8,
    u16,
    u16,
    ZclType,
    Option<u16>,
    &'a ValueDef,
);

fn reportable(e: &Entity) -> Vec<Reportable<'_>> {
    e.r.def
        .values()
        .filter_map(|(p, v)| match &v.binding {
            Binding::Zcl(z) => Some((p, e.r.ep, z.cluster, z.attr, z.ty, z.mfr, v)),
            Binding::Struct {
                cluster, attr, mfr, ..
            } => Some((p, e.r.ep, *cluster, *attr, ZclType::OCTET_STR, *mfr, v)),
            _ => None,
        })
        .collect()
}

/// What a device answered to Read Reporting Configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// Record status (`SUCCESS`, `UNSUPPORTED_ATTRIBUTE`,
    /// `UNREPORTABLE_ATTRIBUTE`, `NOT_FOUND`: nothing configured) or the
    /// Default Response status.
    pub status: Status,
    /// The configuration for `SUCCESS`.
    pub config: Option<ReportingConfig>,
    /// When, ms.
    pub at: Millis,
}

impl Applied {
    fn to_json(&self) -> J {
        let mut o = Map::new();
        o.insert("status".into(), self.status.name().into());
        if let Some(c) = &self.config {
            let r = if c.max == 0xFFFF {
                "off".into()
            } else {
                cfg(c.min, c.max, c.change.filter(|_| c.ty.is_analog())).to_json()
            };
            o.insert("report".into(), r);
        }
        o.insert("time".into(), ts(self.at).into());
        J::Object(o)
    }
}

/// A readback in progress (`entities/reporting` with `read`).
#[derive(Debug, Clone)]
pub(crate) struct ReportingRead {
    /// The API answer ticket; `None` for a sleeping device's read, which
    /// was answered `queued` and ends in an `entity_reporting` event.
    answer: Option<u64>,
    device: String,
    uid: Option<String>,
    remaining: u32,
    answered: bool,
    error: Option<ApiError>,
}

type Groups = BTreeMap<(u8, u16, Option<u16>), Vec<u16>>;

impl<T: ZigbeeTransport> Hub<T> {
    /// The global defaults.
    pub fn reporting_defaults(&self) -> &ReportDefaults {
        &self.report_defaults
    }

    /// Restores the global defaults from the store (before the devices,
    /// so that their resolution uses them).
    pub fn load_reporting_defaults(&mut self, b: &[u8]) -> Result<(), String> {
        let generation = self.report_defaults.generation;
        self.report_defaults = ReportDefaults::from_bytes(b)?;
        self.report_defaults.generation = generation;
        Ok(())
    }

    /// `reporting/defaults/set`: devices re-resolve and reconfigure what
    /// changed.
    pub fn set_reporting_defaults(&mut self, changes: &J, now: Millis) -> Result<J, ApiError> {
        if self.report_defaults.update(changes).map_err(bad)? {
            self.report_defaults.generation += 1;
            // Devices still interviewing resolve with them when done.
            let ids: Vec<String> = self
                .devices
                .values()
                .filter(|d| {
                    d.gp.is_none()
                        && !matches!(
                            d.interview,
                            InterviewStep::NodeDesc
                                | InterviewStep::ActiveEp
                                | InterviewStep::SimpleDesc(_)
                                | InterviewStep::Basic(_)
                        )
                })
                .map(|d| d.id.clone())
                .collect();
            for id in ids {
                self.resolve_device(&id, now);
                self.reporting_changed(&id, now);
            }
        }
        Ok(self.report_defaults.record())
    }

    fn reporting_changed(&mut self, id: &str, now: Millis) {
        if let Some(d) = self.devices.get_mut(id) {
            d.configure.reports_changed = true;
        }
        self.redo_configure(id, now);
    }

    /// `entities/update` `report` (and `part`): sets or clears the
    /// override of one value of an entity.
    pub fn set_entity_report(
        &mut self,
        uid: &str,
        part: Option<&str>,
        report: &J,
        now: Millis,
    ) -> Result<(), ApiError> {
        let setting = ReportSetting::parse(report).map_err(bad)?;
        let did = device_of_uid(uid).to_string();
        let d = self
            .devices
            .get(&did)
            .ok_or_else(|| ApiError::new("not_found", uid.to_string()))?;
        if d.gp.is_some() {
            return Err(ApiError::new(
                "unsupported",
                "Green Power devices take no reporting configuration",
            ));
        }
        let i = d
            .entity_index(uid)
            .filter(|i| !d.entities[*i].orphaned)
            .ok_or_else(|| ApiError::new("not_found", uid.to_string()))?;
        let vals = reportable(&d.entities[i]);
        let names: Vec<&str> = vals.iter().map(|v| v.0.unwrap_or("")).collect();
        let k = match part {
            Some(p) => names.iter().position(|n| *n == p),
            None if vals.len() == 1 => Some(0),
            None => None,
        }
        .ok_or_else(|| {
            bad(if vals.is_empty() {
                format!("{uid} has no ZCL attribute to report")
            } else {
                format!("part: one of {names:?}")
            })
        })?;
        if let Some(s) = setting {
            s.check(&[vals[k].4], max_cap(d)).map_err(bad)?;
        }
        let key = names[k].to_string();
        let d = self.devices.get_mut(&did).unwrap();
        let e = &mut d.entities[i];
        let changed = match setting {
            Some(s) => e.user_report.insert(key, s) != Some(s),
            None => e.user_report.remove(&key).is_some(),
        };
        if changed {
            self.reporting_changed(&did, now);
        }
        Ok(())
    }

    /// The reporting rows of a device's entities (all, or `uid`'s) and the
    /// attributes to read back, by `(ep, cluster, mfr)`.
    fn reporting_rows(&self, did: &str, uid: Option<&str>) -> Result<(Vec<J>, Groups), ApiError> {
        let d = self
            .devices
            .get(did)
            .ok_or_else(|| ApiError::new("not_found", uid.unwrap_or(did).to_string()))?;
        if d.gp.is_some() {
            return Err(ApiError::new(
                "unsupported",
                "Green Power devices take no reporting configuration",
            ));
        }
        if uid.is_some_and(|u| d.entity_index(u).is_none()) {
            return Err(ApiError::new(
                "not_found",
                uid.unwrap_or_default().to_string(),
            ));
        }
        let cap = max_cap(d);
        let mut rows = Vec::new();
        let mut groups = Groups::new();
        for e in d
            .entities
            .iter()
            .filter(|e| !e.orphaned && uid.is_none_or(|u| e.uid == u))
        {
            for (part, ep, cluster, attr, ty, mfr, v) in reportable(e) {
                let class = class_of(cluster, attr).filter(|_| mfr.is_none());
                let over = e.user_report.get(part.unwrap_or(""));
                let source = if over.is_some() {
                    "override"
                } else if e.r.source != LayerSource::Generic {
                    "definition"
                } else if class.is_some_and(|c| {
                    v.report != Some(self.report_defaults.edit(c.key).unwrap_or(c.builtin).spec())
                }) {
                    // A `generic.patch` of the definition.
                    "definition"
                } else {
                    "default"
                };
                let mut o = Map::new();
                o.insert("uid".into(), e.uid.clone().into());
                o.insert("part".into(), part.into());
                o.insert("endpoint".into(), ep.into());
                o.insert("cluster".into(), cluster.into());
                o.insert("attribute".into(), attr.into());
                o.insert("type".into(), ty.0.into());
                if let Some(m) = mfr {
                    o.insert("manufacturer".into(), m.into());
                }
                o.insert("report".into(), spec_json(e.report_of(part, v), cap));
                o.insert("source".into(), source.into());
                if over.is_some() {
                    o.insert("default".into(), spec_json(v.report, cap));
                }
                if let Some(c) = class {
                    o.insert("class".into(), c.key.into());
                }
                if let Some(a) = d.reporting.get(&(ep, cluster, attr)) {
                    o.insert("applied".into(), a.to_json());
                }
                rows.push(J::Object(o));
                let l = groups.entry((ep, cluster, mfr)).or_default();
                if !l.contains(&attr) {
                    l.push(attr);
                }
            }
        }
        Ok((rows, groups))
    }

    /// `entities/reporting`: what the hub configures for each reportable
    /// value of an entity (`uid`) or of all of a device's entities
    /// (`device`), and what the device applied when last read back. With
    /// `read` the device is asked (Read Reporting Configuration, ≤ 3
    /// attributes per frame): the answer follows when it replies; a
    /// sleeping device is answered `queued` at once, asked when it is next
    /// heard from, and its answer is the event `entity_reporting` (topic
    /// `entities`) with the same body plus `device` and `uid`.
    pub fn entity_reporting(
        &mut self,
        uid: Option<&str>,
        device: Option<&str>,
        read: bool,
        now: Millis,
    ) -> Result<J, ApiError> {
        let did = match (uid, device) {
            (Some(u), _) => device_of_uid(u).to_string(),
            (None, Some(d)) => d.to_string(),
            (None, None) => return Err(bad("uid or device required")),
        };
        let (rows, groups) = self.reporting_rows(&did, uid)?;
        if !read || groups.is_empty() {
            let status = if read { "ok" } else { "cached" };
            return Ok(json!({"status": status, "records": rows}));
        }
        let d = &self.devices[&did];
        let queued = d.sleepy() && now > d.awake_until;
        let frames: u32 = groups.values().map(|a| a.len().div_ceil(3) as u32).sum();
        let ticket = (!queued).then(|| self.answer_ticket().ticket());
        self.reporting_seq += 1;
        let read_id = self.reporting_seq;
        self.reporting_reads.insert(
            read_id,
            ReportingRead {
                answer: ticket,
                device: did.clone(),
                uid: uid.map(str::to_string),
                remaining: frames,
                answered: false,
                error: None,
            },
        );
        for ((ep, cluster, mfr), attrs) in groups {
            for chunk in attrs.chunks(3) {
                let tsn = self.next_zcl_tsn();
                let f = ZclFrame::command(
                    FrameType::Global,
                    tsn,
                    global::READ_REPORTING_CONFIG,
                    mfr,
                    zf::read_reporting_config(chunk),
                );
                let purpose = Purpose::Reporting {
                    read: read_id,
                    ep,
                    cluster,
                    attrs: chunk.to_vec(),
                };
                self.send_zcl(&did, ep, cluster, f, purpose, now);
            }
        }
        let Some(t) = ticket else {
            return Ok(json!({"status": "queued", "records": rows}));
        };
        // Every frame may have failed already (transport refused).
        let done = self
            .events
            .iter()
            .position(|e| matches!(e, HubEvent::Answer { ticket, .. } if *ticket == t));
        if let Some(HubEvent::Answer { result, .. }) = done.map(|i| self.events.remove(i)) {
            return result;
        }
        self.deferred = Some(t);
        Ok(J::Null)
    }

    /// Read Reporting Configuration Response from `id`.
    pub(crate) fn reporting_rsp(
        &mut self,
        id: &str,
        ep: u8,
        cluster: u16,
        tsn: u8,
        payload: &[u8],
        now: Millis,
    ) {
        let Some(p) = self.take_pending(id, tsn) else {
            return;
        };
        let Purpose::Reporting { read, .. } = p.purpose else {
            return;
        };
        match zf::parse_read_reporting_config_rsp(payload) {
            Ok(recs) => {
                if let Some(d) = self.devices.get_mut(id) {
                    for r in recs {
                        d.reporting.insert(
                            (ep, cluster, r.attr),
                            Applied {
                                status: r.status,
                                config: r.config,
                                at: now,
                            },
                        );
                    }
                }
                self.reporting_part_done(read, None);
            }
            Err(e) => self.reporting_part_done(
                read,
                Some(ApiError::new("failed", format!("malformed answer: {e}"))),
            ),
        }
    }

    /// A readback frame answered with a Default Response (`Ok`: the
    /// device does not support the command, recorded like an answer), or
    /// failed (timeout, not delivered).
    pub(crate) fn reporting_failed(
        &mut self,
        id: &str,
        purpose: &Purpose,
        r: Result<Status, ApiError>,
        now: Millis,
    ) {
        let Purpose::Reporting {
            read,
            ep,
            cluster,
            attrs,
        } = purpose
        else {
            return;
        };
        match r {
            Ok(st) => {
                if let Some(d) = self.devices.get_mut(id) {
                    for a in attrs {
                        d.reporting.insert(
                            (*ep, *cluster, *a),
                            Applied {
                                status: st,
                                config: None,
                                at: now,
                            },
                        );
                    }
                }
                self.reporting_part_done(*read, None);
            }
            Err(e) => self.reporting_part_done(*read, Some(e)),
        }
    }

    /// One frame of readback `read` done; the last one answers the
    /// request, or (a sleeping device's read) emits `entity_reporting`.
    fn reporting_part_done(&mut self, read: u64, err: Option<ApiError>) {
        let Some(r) = self.reporting_reads.get_mut(&read) else {
            return;
        };
        r.remaining = r.remaining.saturating_sub(1);
        match err {
            Some(e) => {
                r.error.get_or_insert(e);
            }
            None => r.answered = true,
        }
        if r.remaining > 0 {
            return;
        }
        let Some(r) = self.reporting_reads.remove(&read) else {
            return;
        };
        let result = match r.error.filter(|_| !r.answered) {
            Some(e) => Err(e),
            None => self
                .reporting_rows(&r.device, r.uid.as_deref())
                .map(|(rows, _)| json!({"status": "ok", "records": rows})),
        };
        match r.answer {
            Some(ticket) => self.emit(HubEvent::Answer { ticket, result }),
            None => {
                let mut body = match result {
                    Ok(b) => b,
                    Err(e) => json!({"status": "failed", "error": e}),
                };
                body["device"] = r.device.into();
                body["uid"] = r.uid.into();
                self.emit(HubEvent::EntityReporting(body));
            }
        }
    }
}
