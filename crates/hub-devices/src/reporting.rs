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
use crate::hub::OpKind;
use crate::hub::{ApiError, Hub, HubEvent, Purpose, device_of_uid};
use crate::model::{Binding, ConfigureOp, EntityDef, LayerSource, PowerKind, ReportSpec, ValueDef};
use crate::transport::ZigbeeTransport;
use crate::vecmap::VecMap;
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
    /// Left by a removed override when nothing else configures the
    /// attribute: the device is told to go back to its own default
    /// configuration (ZCL r8 2.5.7.1.6: minimum 0xFFFF, maximum 0). Not a
    /// user setting (the API shows no override); stored as `"revert"`.
    Revert,
}

/// Device attribute: `(endpoint, cluster, attribute, manufacturer code)`.
/// Configure Reporting is per attribute, so overrides are too: entities
/// bound to the same attribute share one.
pub type AttrKey = (u8, u16, u16, Option<u16>);

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
            Self::Config { min, max, change } => {
                let mut a = vec![J::from(min), J::from(max)];
                if let Some(c) = change {
                    a.push(if c.fract() == 0.0 && c.abs() < 9e15 {
                        J::from(c as i64)
                    } else {
                        J::from(c)
                    });
                }
                J::Array(a)
            }
            Self::Off => "off".into(),
            Self::Revert => "revert".into(),
        }
    }

    /// The resolved form (`Revert` resolves to nothing: `build_ops` sends
    /// its record).
    pub fn spec(self) -> ReportSpec {
        match self {
            Self::Config { min, max, change } => ReportSpec::Config { min, max, change },
            Self::Off => ReportSpec::Disable,
            Self::Revert => ReportSpec::Off,
        }
    }

    /// A user's override (not a pending revert).
    pub fn is_override(self) -> bool {
        self != Self::Revert
    }
}

impl serde::Serialize for ReportSetting {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(s)
    }
}

impl<'de> serde::Deserialize<'de> for ReportSetting {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::from_store(&J::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

impl ReportSetting {
    /// The store form: a setting or `"revert"`.
    pub fn from_store(v: &J) -> Result<Self, String> {
        if v == "revert" {
            return Ok(Self::Revert);
        }
        Self::parse(v)?.ok_or_else(|| "report: a setting".into())
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
    edits: VecMap<&'static str, ReportSetting>,
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
        // Inserted, not collected: no sort instantiation (firmware size).
        let mut m = Map::new();
        for (k, s) in &self.edits {
            m.insert((*k).to_string(), s.to_json());
        }
        serde_json::to_vec(&m).unwrap_or_default()
    }

    /// Reads the store form; classes this build does not know and invalid
    /// settings are skipped.
    pub fn from_bytes(b: &[u8]) -> Result<Self, String> {
        // Parsed as a value (its parser is in the firmware anyway).
        let v: J = serde_json::from_slice(b).map_err(|e| e.to_string())?;
        let m = v.as_object().ok_or("reporting defaults: an object")?;
        let mut d = Self::default();
        for (k, v) in m {
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
                let attrs: Vec<J> = c
                    .attrs
                    .iter()
                    .map(|a| J::Array(vec![a.0.into(), a.1.into()]))
                    .collect();
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
        let mut o = Map::new();
        o.insert("classes".into(), J::Array(classes));
        J::Object(o)
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
            let edit = ReportSetting::parse(v)
                .and_then(|s| match s {
                    Some(s) if s != c.builtin => check_class(c, s).map(|()| Some(s)),
                    _ => Ok(None),
                })
                .map_err(|e| format!("{k}: {e}"))?;
            match edit {
                Some(s) => next.insert(c.key, s),
                None => next.remove(c.key),
            };
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

/// The attribute a value is bound to: `(cluster, attribute, type, mfr)`.
pub fn zcl_attr(v: &ValueDef) -> Option<(u16, u16, ZclType, Option<u16>)> {
    match &v.binding {
        Binding::Zcl(z) => Some((z.cluster, z.attr, z.ty, z.mfr)),
        Binding::Struct {
            cluster, attr, mfr, ..
        } => Some((*cluster, *attr, ZclType::OCTET_STR, *mfr)),
        _ => None,
    }
}

/// The reporting of an entity value on endpoint `ep`: the user's override
/// of its attribute, else the resolved one (definition `configure: report`
/// ops aside).
pub fn value_report(d: &Device, ep: u8, v: &ValueDef) -> Option<ReportSpec> {
    let user = zcl_attr(v).and_then(|(c, a, _, m)| d.report_overrides.get(&(ep, c, a, m)));
    match user {
        Some(s) if s.is_override() => Some(s.spec()),
        _ => v.report,
    }
}

/// The reporting of the last definition `configure: report` op for the
/// attribute that sends a record (`report: off` ops send nothing), if any.
pub fn op_report(d: &Device, key: AttrKey) -> Option<ReportSpec> {
    d.resolution.configure.iter().rev().find_map(|op| match op {
        ConfigureOp::Report {
            ep,
            cluster,
            attr,
            mfr,
            report,
            ..
        } if (*cluster, *attr, *mfr) == (key.1, key.2, key.3)
            && matches!(report, ReportSpec::Config { .. })
            && ep.unwrap_or_else(|| crate::configure::first_ep_with(d, *cluster)) == key.0 =>
        {
            Some(*report)
        }
        _ => None,
    })
}

fn has_record(s: Option<ReportSpec>) -> bool {
    matches!(s, Some(ReportSpec::Config { .. } | ReportSpec::Disable))
}

/// The resolved reporting an attribute gets without a user override:
/// that of a definition op, else of the first entity value with a record
/// (the one `build_ops` sends), with that entity and value.
fn inherited(d: &Device, key: AttrKey) -> (Option<ReportSpec>, Option<(&Entity, &ValueDef)>) {
    let owner = d.entities.iter().filter(|e| !e.orphaned).find_map(|e| {
        reportable(e)
            .into_iter()
            .find(|r| (r.1, r.2, r.3, r.5) == key && has_record(r.6.report))
            .map(|r| (e, r.6))
    });
    let op = op_report(d, key);
    (op.or(owner.and_then(|o| o.1.report)), owner)
}

/// Whether the attribute gets a Configure Reporting record without a user
/// override.
pub fn inherited_record(d: &Device, key: AttrKey) -> bool {
    has_record(inherited(d, key).0)
}

/// A record as the API shows it: `[min, max, change]`, `"off"` (maximum
/// 0xFFFF), or null for a revert to the device's own default.
fn record_json(c: &ReportingConfig) -> J {
    if c.max == 0xFFFF {
        "off".into()
    } else if c.min == 0xFFFF {
        J::Null
    } else {
        cfg(c.min, c.max, c.change.filter(|_| c.ty.is_analog())).to_json()
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

/// The values of a device's live entities bound to attribute `key`, with
/// their entity uids.
fn shared(
    d: &Device,
    key: AttrKey,
) -> Vec<(
    Option<&str>,
    u8,
    u16,
    u16,
    ZclType,
    Option<u16>,
    &ValueDef,
    &str,
)> {
    d.entities
        .iter()
        .filter(|e| !e.orphaned)
        .flat_map(|e| {
            reportable(e)
                .into_iter()
                .filter(|r| (r.1, r.2, r.3, r.5) == key)
                .map(|r| (r.0, r.1, r.2, r.3, r.4, r.5, r.6, e.uid.as_str()))
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
            o.insert("report".into(), record_json(c));
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

/// An `entities/reporting` answer: `{status, records}`.
fn answer(status: &str, rows: Vec<J>) -> J {
    let mut o = Map::new();
    o.insert("status".into(), status.into());
    o.insert("records".into(), J::Array(rows));
    J::Object(o)
}

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
    /// override of the attribute one value of an entity is bound to (all
    /// entities bound to that attribute share it). Clearing it when
    /// nothing else configures the attribute leaves a revert to the
    /// device's own default ([`ReportSetting::Revert`]).
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
        let key = (vals[k].1, vals[k].2, vals[k].3, vals[k].5);
        if let Some(s) = setting {
            let types: Vec<ZclType> = shared(d, key).into_iter().map(|r| r.4).collect();
            s.check(&types, max_cap(d)).map_err(bad)?;
        }
        let cur = d.report_overrides.get(&key).copied();
        let next = match (setting, cur) {
            (Some(s), _) => Some(s),
            (None, Some(c)) if c.is_override() && !inherited_record(d, key) => {
                Some(ReportSetting::Revert)
            }
            (None, Some(ReportSetting::Revert)) => cur,
            (None, _) => None,
        };
        if next == cur {
            return Ok(());
        }
        let d = self.devices.get_mut(&did).unwrap();
        match next {
            Some(s) => d.report_overrides.insert(key, s),
            None => d.report_overrides.remove(&key),
        };
        self.reporting_changed(&did, now);
        Ok(())
    }

    /// The reporting rows of a device's entities (all, or `uid`'s) and the
    /// attributes to read back, by `(ep, cluster, mfr)`. `report` is the
    /// record the configure sends for the attribute (the last one, after
    /// overrides, definition ops and the battery cap).
    fn reporting_rows(
        &self,
        did: &str,
        uid: Option<&str>,
        now: Millis,
    ) -> Result<(Vec<J>, Groups), ApiError> {
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
        let env = crate::hub::DevEnv {
            dev: d,
            now,
            tz: self.cfg.tz_offset_s,
        };
        let ops =
            crate::configure::build_ops(d, self.cfg.coordinator_ieee, self.cfg.hub_endpoint, &env);
        let sent = |key: AttrKey| {
            ops.iter().rev().find_map(|o| match &o.kind {
                OpKind::Report {
                    ep,
                    cluster,
                    mfr,
                    records,
                } if (*ep, *cluster, *mfr) == (key.0, key.1, key.3) => {
                    records.iter().find(|r| r.attr == key.2)
                }
                _ => None,
            })
        };
        // Generic values keep the default unless a definition patched it.
        let label = |e: &Entity, v: &ValueDef| {
            let class = zcl_attr(v)
                .filter(|a| a.3.is_none())
                .and_then(|a| class_of(a.0, a.1));
            let patched = class.is_some_and(|c| {
                v.report != Some(self.report_defaults.edit(c.key).unwrap_or(c.builtin).spec())
            });
            if e.r.source != LayerSource::Generic || patched {
                "definition"
            } else {
                "default"
            }
        };
        let cap = max_cap(d);
        let mut rows = Vec::new();
        let mut groups = Groups::new();
        for e in d
            .entities
            .iter()
            .filter(|e| !e.orphaned && uid.is_none_or(|u| e.uid == u))
        {
            for (part, ep, cluster, attr, ty, mfr, v) in reportable(e) {
                let key = (ep, cluster, attr, mfr);
                let over = d.report_overrides.get(&key).filter(|s| s.is_override());
                let (inh, owner) = inherited(d, key);
                let source = if over.is_some() {
                    "override"
                } else if op_report(d, key).is_some() {
                    "definition"
                } else {
                    let (o, ov) = owner.unwrap_or((e, v));
                    label(o, ov)
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
                o.insert("report".into(), sent(key).map_or(J::Null, record_json));
                o.insert("source".into(), source.into());
                if over.is_some() {
                    o.insert("default".into(), spec_json(inh, cap));
                }
                let others: Vec<J> = shared(d, key)
                    .into_iter()
                    .filter(|r| r.7 != e.uid)
                    .map(|r| J::from(r.7))
                    .collect();
                if !others.is_empty() {
                    o.insert("shared".into(), J::Array(others));
                }
                if let Some(c) = class_of(cluster, attr).filter(|_| mfr.is_none()) {
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
        let (rows, groups) = self.reporting_rows(&did, uid, now)?;
        if !read || groups.is_empty() {
            let status = if read { "ok" } else { "cached" };
            return Ok(answer(status, rows));
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
            return Ok(answer("queued", rows));
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
                self.reporting_part_done(read, None, now);
            }
            Err(e) => self.reporting_part_done(
                read,
                Some(ApiError::new("failed", format!("malformed answer: {e}"))),
                now,
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
                self.reporting_part_done(*read, None, now);
            }
            Err(e) => self.reporting_part_done(*read, Some(e), now),
        }
    }

    /// One frame of readback `read` done; the last one answers the
    /// request, or (a sleeping device's read) emits `entity_reporting`.
    fn reporting_part_done(&mut self, read: u64, err: Option<ApiError>, now: Millis) {
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
                .reporting_rows(&r.device, r.uid.as_deref(), now)
                .map(|(rows, _)| answer("ok", rows)),
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
