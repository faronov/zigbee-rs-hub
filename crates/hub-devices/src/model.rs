//! The engine's definition model: what a resolved layer contributes
//! (spec 6). Every tier produces these records — generic rules (5),
//! catalog and user definitions (6), self-descriptions (12) and GP
//! definitions (13.5) — so there is one entity runtime (goal "one entity
//! runtime, three sources").
//!
//! `zdef` decodes CBOR into its own definition types; an adapter converts
//! them into these (documented in the crate README as the zdef boundary).

use std::collections::BTreeMap;

use crate::expr::{Expr, Value};
use crate::zcl::ZclType;

/// HA entity platform (spec 4.3, CBOR enum 8.3).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Platform {
    Sensor,
    BinarySensor,
    Number,
    Select,
    Switch,
    Button,
    Text,
    Light,
    Cover,
    Climate,
    Lock,
    Fan,
    Event,
    Siren,
    Valve,
    Update,
    /// Group scenes (14.9); not in definitions (no CBOR value).
    Scene,
}

impl Platform {
    /// All platforms in CBOR order.
    pub const ALL: [Self; 16] = [
        Self::Sensor,
        Self::BinarySensor,
        Self::Number,
        Self::Select,
        Self::Switch,
        Self::Button,
        Self::Text,
        Self::Light,
        Self::Cover,
        Self::Climate,
        Self::Lock,
        Self::Fan,
        Self::Event,
        Self::Siren,
        Self::Valve,
        Self::Update,
    ];

    /// HA platform name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sensor => "sensor",
            Self::BinarySensor => "binary_sensor",
            Self::Number => "number",
            Self::Select => "select",
            Self::Switch => "switch",
            Self::Button => "button",
            Self::Text => "text",
            Self::Light => "light",
            Self::Cover => "cover",
            Self::Climate => "climate",
            Self::Lock => "lock",
            Self::Fan => "fan",
            Self::Event => "event",
            Self::Siren => "siren",
            Self::Valve => "valve",
            Self::Update => "update",
            Self::Scene => "scene",
        }
    }

    /// Parses an HA platform name.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }

    /// Default access letters (spec 6.3).
    pub fn default_access(self) -> Access {
        match self {
            Self::Sensor | Self::BinarySensor | Self::Event => Access {
                read: true,
                write: false,
                report: true,
            },
            Self::Button => Access {
                read: false,
                write: true,
                report: false,
            },
            _ => Access {
                read: true,
                write: true,
                report: true,
            },
        }
    }

    /// Composite platforms are built from parts (4.3).
    pub fn is_composite(self) -> bool {
        matches!(
            self,
            Self::Light
                | Self::Cover
                | Self::Climate
                | Self::Lock
                | Self::Fan
                | Self::Siren
                | Self::Valve
        )
    }

    /// Platforms whose state is a bool.
    pub fn is_bool(self) -> bool {
        matches!(self, Self::BinarySensor | Self::Switch)
    }
}

/// HA sensor state class (8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum StateClass {
    Measurement,
    Total,
    TotalIncreasing,
    MeasurementAngle,
}

impl StateClass {
    /// Parses an HA name.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "measurement" => Self::Measurement,
            "total" => Self::Total,
            "total_increasing" => Self::TotalIncreasing,
            "measurement_angle" => Self::MeasurementAngle,
            _ => return None,
        })
    }
}

/// HA entity category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Category {
    Config,
    Diagnostic,
}

impl Category {
    /// Parses an HA name.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "config" => Self::Config,
            "diagnostic" => Self::Diagnostic,
            _ => return None,
        })
    }
}

/// Access letters `r`, `w`, `p` (6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Access {
    /// Read.
    pub read: bool,
    /// Write.
    pub write: bool,
    /// The device reports/pushes it.
    pub report: bool,
}

impl Access {
    /// Parses letters.
    pub fn parse(s: &str) -> Option<Self> {
        let mut a = Self::default();
        for c in s.chars() {
            match c {
                'r' => a.read = true,
                'w' => a.write = true,
                'p' => a.report = true,
                _ => return None,
            }
        }
        Some(a)
    }
}

/// `report:` (6.3, 5.2).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ReportSpec {
    /// No Configure Reporting.
    Off,
    /// `[min, max, change]`; change in raw attribute units.
    Config {
        /// Minimum interval, s.
        min: u16,
        /// Maximum interval, s.
        max: u16,
        /// Reportable change.
        change: Option<f64>,
    },
}

/// How a generic ZCL binding is written (the generic tier uses cluster
/// commands where ZCL has them; definitions write attributes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ZclWrite {
    /// Write Attributes (0x02).
    #[default]
    Attribute,
    /// On/Off commands 0x00 / 0x01.
    OnOffCommands,
    /// Level: Move to Level with On/Off (0x04).
    MoveToLevel,
    /// Color: Move to Color Temperature (0x0A).
    MoveToColorTemp,
    /// Color: Move to Color (0x07), X part.
    ColorX,
    /// Color: Move to Color (0x07), Y part.
    ColorY,
    /// Color: Move to Hue and Saturation (0x06), hue part.
    Hue,
    /// Color: Move to Hue and Saturation (0x06), saturation part.
    Saturation,
    /// Window Covering Go To Lift Percentage (0x05).
    CoverLift,
    /// Window Covering Go To Tilt Percentage (0x08).
    CoverTilt,
    /// Door Lock 0x00 Lock / 0x01 Unlock.
    LockCommands,
    /// Not writable.
    ReadOnly,
}

/// `zcl:` binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZclBinding {
    /// Cluster.
    pub cluster: u16,
    /// Attribute.
    pub attr: u16,
    /// Wire type.
    pub ty: ZclType,
    /// Manufacturer code.
    pub mfr: Option<u16>,
    /// Write path.
    pub write: ZclWrite,
}

/// Tuya DP type (10.1, 8.3).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum TuyaType {
    Raw = 0,
    Bool = 1,
    Value = 2,
    String = 3,
    Enum = 4,
    Bitmap = 5,
}

impl TuyaType {
    /// From the wire code.
    pub fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            0 => Self::Raw,
            1 => Self::Bool,
            2 => Self::Value,
            3 => Self::String,
            4 => Self::Enum,
            5 => Self::Bitmap,
            _ => return None,
        })
    }
    /// From the definition name.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "raw" => Self::Raw,
            "bool" => Self::Bool,
            "value" => Self::Value,
            "string" => Self::String,
            "enum" => Self::Enum,
            "bitmap" => Self::Bitmap,
            _ => return None,
        })
    }
}

pub use zdef::enums::ScheduleVariant;

/// Named codecs (6.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Codec {
    /// `(tag u8, ZCL type u8, value)*` → tag → value.
    ZclTagged,
    /// Tuya per-day schedule ↔ `HH:MM/T …` (`multi_dp`) or `HH:MM/T/P …`
    /// (`trv602z`); the day number is written as the first byte.
    TuyaScheduleDay {
        /// Day number written as the first byte.
        day: u8,
        /// Transitions per day.
        transitions: u8,
        /// Byte layout.
        variant: ScheduleVariant,
    },
    /// Tuya schedule DP without a day number (`multi_dp`, `single_dp`,
    /// `trv602z`, `trv603wz`; see [`crate::codec`]).
    TuyaScheduleWeek {
        /// Byte layout.
        variant: ScheduleVariant,
        /// Transitions (`multi_dp`: exact count; others: the variant's).
        transitions: u8,
        /// Temperature scale (`multi_dp`).
        scale: u16,
        /// Lowest accepted temperature, °C (`multi_dp`).
        min: i16,
        /// Highest accepted temperature, °C (`multi_dp`).
        max: i16,
    },
    /// Packed BCD time `HH:MM`.
    BcdTime,
    /// `[hour, minute]` → `HH:MM`.
    Hhmm,
    /// Bytes as ASCII/UTF-8 text.
    Ascii,
    /// UTF-16BE hex string (herdsman `utf16BEHexString`).
    Utf16BeHex,
    /// Bitmap → list of labels.
    BitmapFlags(Vec<(u8, String)>),
}

impl Codec {
    /// The `requires` feature name of the codec.
    pub fn feature(&self) -> &'static str {
        match self {
            Self::ZclTagged => "struct.zcl_tagged",
            Self::TuyaScheduleDay { .. } => "codec.tuya_schedule_day",
            Self::TuyaScheduleWeek { .. } => "codec.tuya_schedule_week",
            Self::BcdTime => "codec.bcd_time",
            Self::Hhmm => "codec.hhmm",
            Self::Ascii => "codec.ascii",
            Self::Utf16BeHex => "codec.utf16be_hex",
            Self::BitmapFlags(_) => "codec.bitmap_flags",
        }
    }
}

/// `gp:` binding (13.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpBinding {
    /// An attribute of 0xA0–0xA3 reports.
    Report {
        /// Cluster.
        cluster: u16,
        /// Attribute.
        attr: u16,
        /// Type.
        ty: ZclType,
        /// Manufacturer code (0xA1/0xA3).
        mfr: Option<u16>,
    },
    /// A GPD command; `payload` is its payload.
    Cmd(u8),
}

/// Value source of an entity or a part.
#[derive(Debug, Clone, Default)]
pub enum Binding {
    /// No value (buttons, events, composites with parts only).
    #[default]
    None,
    /// ZCL attribute.
    Zcl(ZclBinding),
    /// Tuya DP.
    Tuya {
        /// DP.
        dp: u8,
        /// Type.
        ty: TuyaType,
    },
    /// Tagged field of a struct attribute.
    Struct {
        /// Cluster.
        cluster: u16,
        /// Attribute.
        attr: u16,
        /// Manufacturer code.
        mfr: Option<u16>,
        /// Codec (`zcl_tagged`).
        codec: Codec,
        /// Tag.
        tag: u8,
    },
    /// GP report or command.
    Gp(GpBinding),
    /// Value kept by the hub (`opt.<key>`).
    Hub {
        /// Default value.
        default: Value,
    },
    /// IAS WD siren (Start Warning / Squawk), command-only.
    IasWarning,
    /// Device-level diagnostic kept by the hub (`linkquality`, `rssi`,
    /// `last_seen`, GP `frame_counter`/`security_level`, Tuya
    /// `mcu_version`).
    DeviceField(DeviceField),
}

/// Device fields exposed as diagnostic sensors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum DeviceField {
    Linkquality,
    Rssi,
    LastSeen,
    FrameCounter,
    SecurityLevel,
    McuVersion,
}

/// Dynamic scaling from other attributes of the same cluster (generic
/// Metering / Electrical Measurement, 5.1): `x × mul_attr / div_attr ×
/// factor`, used only when the layer fixes neither `mul` nor `div`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AttrScale {
    /// Cluster holding the multiplier/divisor.
    pub cluster: u16,
    /// Multiplier attribute.
    pub mul_attr: Option<u16>,
    /// Divisor attribute.
    pub div_attr: Option<u16>,
    /// Constant factor (kW → W: 1000).
    pub factor: f64,
}

/// Value transforms (6.4).
#[derive(Debug, Clone, Default)]
pub struct Transforms {
    /// Named codec.
    pub codec: Option<Codec>,
    /// Raw value meaning "unknown".
    pub invalid: Option<f64>,
    /// raw → label.
    pub map: Vec<(i64, String)>,
    /// Raw value for "on".
    pub value_on: Option<Value>,
    /// Raw value for "off".
    pub value_off: Option<Value>,
    /// Decode expression.
    pub from: Option<Expr>,
    /// Encode expression.
    pub to: Option<Expr>,
    /// Multiplier.
    pub mul: Option<f64>,
    /// Divisor.
    pub div: Option<f64>,
    /// Offset.
    pub offset: Option<f64>,
    /// Decimals.
    pub precision: Option<u8>,
    /// Generic dynamic scaling.
    pub attr_scale: Option<AttrScale>,
}

/// Limits (number, text, climate setpoints).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Limits {
    /// Minimum.
    pub min: Option<f64>,
    /// Maximum.
    pub max: Option<f64>,
    /// Step.
    pub step: Option<f64>,
    /// `auto`, `box`, `slider` (number) / `text`, `password` (text).
    pub mode: Option<String>,
}

/// A binding with its transforms: the entity's own value or one part.
#[derive(Debug, Clone, Default)]
pub struct ValueDef {
    /// Source.
    pub binding: Binding,
    /// Transforms.
    pub transforms: Transforms,
    /// Limits.
    pub limits: Limits,
    /// Reporting.
    pub report: Option<ReportSpec>,
    /// Access letters (default per platform).
    pub access: Option<Access>,
}

/// Where an event trigger listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerSource {
    /// A ZCL command (either direction by default).
    Cmd {
        /// Endpoint filter.
        ep: Option<u8>,
        /// Cluster.
        cluster: u16,
        /// Command ID.
        id: u8,
        /// Manufacturer code the frame must carry.
        mfr: Option<u16>,
        /// `true` for foundation commands (generic tier only).
        global: bool,
    },
    /// A GPD command.
    GpCmd(u8),
    /// A Tuya DP (`payload` = DP data).
    Tuya {
        /// DP.
        dp: u8,
    },
}

/// How a trigger chooses its event type.
#[derive(Debug, Clone)]
pub enum EventSel {
    /// Fixed type.
    Fixed(String),
    /// Expression yielding a string.
    Expr(Expr),
}

/// One trigger of an event entity (6.7).
#[derive(Debug, Clone)]
pub struct Trigger {
    /// Source.
    pub source: TriggerSource,
    /// Filter.
    pub when: Option<Expr>,
    /// Event type.
    pub event: EventSel,
    /// Event attributes.
    pub attrs: Vec<(String, Expr)>,
}

/// Press payload.
#[derive(Debug, Clone)]
pub enum PressPayload {
    /// Literal bytes.
    Bytes(Vec<u8>),
    /// `payload` expression (must yield bytes).
    Expr(Expr),
}

/// `press:` of a button (6.7).
#[derive(Debug, Clone)]
pub enum Press {
    /// A cluster command.
    Cmd {
        /// Cluster.
        cluster: u16,
        /// Command ID.
        id: u8,
        /// Manufacturer code.
        mfr: Option<u16>,
        /// Endpoint.
        ep: Option<u8>,
        /// Payload.
        payload: Option<PressPayload>,
    },
    /// A one-shot attribute write.
    Write {
        /// Cluster.
        cluster: u16,
        /// Attribute.
        attr: u16,
        /// Type.
        ty: ZclType,
        /// Value.
        value: Value,
        /// Manufacturer code.
        mfr: Option<u16>,
    },
    /// A Tuya DP write.
    Tuya {
        /// DP.
        dp: u8,
        /// Type.
        ty: TuyaType,
        /// Value.
        value: Value,
    },
}

/// One entity definition (6.3).
#[derive(Debug, Clone)]
pub struct EntityDef {
    /// Key.
    pub key: String,
    /// Platform.
    pub platform: Platform,
    /// HA device class.
    pub device_class: Option<String>,
    /// HA unit.
    pub unit: Option<String>,
    /// HA state class.
    pub state_class: Option<StateClass>,
    /// Entity category.
    pub category: Option<Category>,
    /// Display name (`None`: HA derives it).
    pub name: Option<String>,
    /// HA translation key (generic entities).
    pub translation_key: Option<String>,
    /// `enabled` (HA `entity_registry_enabled_default`).
    pub enabled: bool,
    /// Endpoint; resolved from the bindings when absent.
    pub ep: Option<u8>,
    /// The entity's own value (simple platforms).
    pub value: ValueDef,
    /// Parts (composite platforms), in definition order.
    pub parts: Vec<(String, ValueDef)>,
    /// Event triggers.
    pub triggers: Vec<Trigger>,
    /// Event types.
    pub event_types: Vec<String>,
    /// Button action.
    pub press: Option<Press>,
    /// Auto-reset of binary sensors (seconds expression).
    pub reset_after: Option<Expr>,
    /// Existence condition over the fingerprint.
    pub present_if: Option<Expr>,
    /// Cover: device reports the other way round.
    pub invert: bool,
    /// Static capabilities computed by the generic tier (supported color
    /// modes, hvac modes …), merged with the derived ones.
    pub capabilities: serde_json::Map<String, serde_json::Value>,
}

impl EntityDef {
    /// A new entity with defaults.
    pub fn new(key: impl Into<String>, platform: Platform) -> Self {
        Self {
            key: key.into(),
            platform,
            device_class: None,
            unit: None,
            state_class: None,
            category: None,
            name: None,
            translation_key: None,
            enabled: true,
            ep: None,
            value: ValueDef::default(),
            parts: Vec::new(),
            triggers: Vec::new(),
            event_types: Vec::new(),
            press: None,
            reset_after: None,
            present_if: None,
            invert: false,
            capabilities: serde_json::Map::new(),
        }
    }

    /// Effective access of a value.
    pub fn access_of(&self, v: &ValueDef) -> Access {
        v.access.unwrap_or_else(|| self.platform.default_access())
    }

    /// All value definitions (own value and parts) with their part name.
    pub fn values(&self) -> impl Iterator<Item = (Option<&str>, &ValueDef)> {
        std::iter::once((None, &self.value))
            .chain(self.parts.iter().map(|(n, v)| (Some(n.as_str()), v)))
    }
}

/// Power source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum PowerKind {
    Mains,
    Battery,
    EnergyHarvesting,
}

impl PowerKind {
    /// Parses a definition name.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "mains" => Self::Mains,
            "battery" => Self::Battery,
            "energy_harvesting" => Self::EnergyHarvesting,
            _ => return None,
        })
    }
}

/// Battery percent curve (`device.battery`).
#[derive(Debug, Clone, PartialEq)]
pub enum BatteryCurve {
    /// Linear between `[min_mV, max_mV]`.
    Linear(f64, f64),
    /// Named curve (`3V_2100`, `3V_1500_2800`, `CR2032`).
    Named(String),
}

impl BatteryCurve {
    /// Percent for a voltage in mV, clamped to 0–100.
    pub fn percent(&self, mv: f64) -> f64 {
        let (lo, hi) = match self {
            Self::Linear(lo, hi) => (*lo, *hi),
            Self::Named(n) => match n.as_str() {
                "3V_2100" => (2100.0, 3000.0),
                "3V_1500_2800" => (1500.0, 2800.0),
                "CR2032" => (2000.0, 3000.0),
                _ => (2100.0, 3000.0),
            },
        };
        if hi <= lo {
            return 0.0;
        }
        (((mv - lo) / (hi - lo)) * 100.0).clamp(0.0, 100.0).round()
    }
}

/// `device:` block.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeviceFields {
    /// Power source.
    pub power: Option<PowerKind>,
    /// Sleepy end device.
    pub sleepy: Option<bool>,
    /// Battery curve.
    pub battery: Option<BatteryCurve>,
    /// Poll Control check-in interval, s.
    pub checkin_s: Option<u32>,
    /// Availability timeout override, s.
    pub availability_timeout_s: Option<u64>,
    /// OTA support.
    pub ota: Option<bool>,
}

impl DeviceFields {
    /// Field-wise override by a higher layer.
    pub fn overlay(&mut self, hi: &Self) {
        if hi.power.is_some() {
            self.power = hi.power;
        }
        if hi.sleepy.is_some() {
            self.sleepy = hi.sleepy;
        }
        if hi.battery.is_some() {
            self.battery.clone_from(&hi.battery);
        }
        if hi.checkin_s.is_some() {
            self.checkin_s = hi.checkin_s;
        }
        if hi.availability_timeout_s.is_some() {
            self.availability_timeout_s = hi.availability_timeout_s;
        }
        if hi.ota.is_some() {
            self.ota = hi.ota;
        }
    }
}

/// Field-level patch of a generic entity (`generic.patch`, 5.5).
#[derive(Debug, Clone, Default)]
pub struct EntityPatch {
    /// Reporting.
    pub report: Option<ReportSpec>,
    /// `mul`.
    pub mul: Option<f64>,
    /// `div`.
    pub div: Option<f64>,
    /// `offset`.
    pub offset: Option<f64>,
    /// `precision`.
    pub precision: Option<u8>,
    /// `invalid`.
    pub invalid: Option<f64>,
    /// `min`.
    pub min: Option<f64>,
    /// `max`.
    pub max: Option<f64>,
    /// `step`.
    pub step: Option<f64>,
    /// `name`.
    pub name: Option<String>,
    /// `enabled`.
    pub enabled: Option<bool>,
    /// `category`.
    pub category: Option<Category>,
    /// `device_class`.
    pub device_class: Option<String>,
    /// `unit`.
    pub unit: Option<String>,
    /// `invert` (cover).
    pub invert: Option<bool>,
}

/// `generic:` knobs.
#[derive(Debug, Clone, Default)]
pub struct GenericKnobs {
    /// Keys to drop.
    pub exclude: Vec<String>,
    /// Restrict layer 1 to these endpoints.
    pub only_endpoints: Option<Vec<u8>>,
    /// Field-level patches by key.
    pub patch: Vec<(String, EntityPatch)>,
    /// Drop layer 1.
    pub disable: bool,
}

/// Configure op (6.8).
#[derive(Debug, Clone)]
pub enum ConfigureOp {
    /// Bind clusters to the hub.
    Bind {
        /// Endpoint.
        ep: Option<u8>,
        /// Clusters.
        clusters: Vec<u16>,
    },
    /// Write an attribute.
    Write {
        /// Endpoint.
        ep: Option<u8>,
        /// Cluster.
        cluster: u16,
        /// Attribute.
        attr: u16,
        /// Type.
        ty: ZclType,
        /// Value.
        value: Value,
        /// Manufacturer code.
        mfr: Option<u16>,
    },
    /// Read attributes.
    Read {
        /// Endpoint.
        ep: Option<u8>,
        /// Cluster.
        cluster: u16,
        /// Attributes.
        attrs: Vec<u16>,
        /// Manufacturer code.
        mfr: Option<u16>,
    },
    /// Configure Reporting of one attribute.
    Report {
        /// Endpoint.
        ep: Option<u8>,
        /// Cluster.
        cluster: u16,
        /// Attribute.
        attr: u16,
        /// Type.
        ty: ZclType,
        /// `[min, max, change]`.
        report: ReportSpec,
        /// Manufacturer code.
        mfr: Option<u16>,
    },
    /// A cluster command.
    Cmd {
        /// Endpoint.
        ep: Option<u8>,
        /// Cluster.
        cluster: u16,
        /// Command.
        id: u8,
        /// Manufacturer code.
        mfr: Option<u16>,
        /// Payload: bytes, or an expression evaluated when the configure
        /// plan is built (options and state, no time).
        payload: PressPayload,
    },
    /// The hub joins a group the remote sends to.
    Group {
        /// Hub endpoint.
        ep: Option<u8>,
        /// Group.
        group: u16,
    },
    /// Poll Control check-in interval, quarter-seconds.
    Checkin {
        /// Interval.
        interval_qs: u32,
    },
}

/// `ignore:` rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IgnoreRule {
    /// Endpoint.
    pub ep: Option<u8>,
    /// Cluster.
    pub cluster: u16,
    /// Command.
    pub cmd: Option<u8>,
}

/// Tuya time sync epoch (10.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeSync {
    /// Do not answer.
    #[default]
    Off,
    /// Seconds since 1970.
    Epoch1970,
    /// Seconds since 2000-01-01.
    Epoch2000,
}

/// Tuya write command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TuyaSend {
    /// dataRequest (0x00).
    #[default]
    DataRequest,
    /// sendData (0x04).
    SendData,
}

/// `tuya:` block (10.3).
#[derive(Debug, Clone, PartialEq)]
pub struct TuyaFlags {
    /// mcuSyncTime answer.
    pub time_sync: TimeSync,
    /// Push time at most this often on any message.
    pub force_time_sync_s: Option<u32>,
    /// Basic magic-packet read at configure.
    pub magic_packet: bool,
    /// dataQuery after configure.
    pub query_on_configure: bool,
    /// dataQuery after Device_annce.
    pub query_on_announce: bool,
    /// Periodic dataQuery.
    pub query_interval_s: Option<u32>,
    /// Bind Basic at configure.
    pub bind_basic: bool,
    /// Answer mcuGatewayConnectionStatus.
    pub gateway_status: bool,
    /// Write command.
    pub send: TuyaSend,
    /// Read MCU version at configure.
    pub mcu_version: bool,
    /// Update entity state on send.
    pub optimistic: bool,
    /// Drop identical `(seq, dp, data)` within 2 s.
    pub dedup: bool,
}

impl Default for TuyaFlags {
    fn default() -> Self {
        Self {
            time_sync: TimeSync::Off,
            force_time_sync_s: None,
            magic_packet: false,
            query_on_configure: false,
            query_on_announce: false,
            query_interval_s: None,
            bind_basic: false,
            gateway_status: true,
            send: TuyaSend::DataRequest,
            mcu_version: false,
            optimistic: false,
            dedup: true,
        }
    }
}

/// `gp:` device block (13.5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpFlags {
    /// Report interval for availability.
    pub report_interval_s: Option<u32>,
    /// Warn below this security level.
    pub security_min: Option<u8>,
    /// GPD commands that create nothing.
    pub ignore_commands: Vec<u8>,
}

/// String match (6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrMatch {
    /// Exact (after trimming trailing NULs and spaces of the device value).
    Exact(String),
    /// One of.
    Any(Vec<String>),
    /// Prefix.
    Prefix(String),
}

impl StrMatch {
    /// Matches a device string.
    pub fn matches(&self, v: &str) -> bool {
        let v = zdef::matching::trim_name(v);
        match self {
            Self::Exact(s) => zdef::matching::trim_name(s) == v,
            Self::Any(l) => l.iter().any(|s| zdef::matching::trim_name(s) == v),
            Self::Prefix(p) => v.starts_with(p.as_str()),
        }
    }
    /// `true` for exact forms.
    pub fn is_exact(&self) -> bool {
        !matches!(self, Self::Prefix(_))
    }
}

/// Endpoint constraint of a fingerprint clause.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EpMatch {
    /// Endpoint.
    pub ep: u8,
    /// Profile.
    pub profile: Option<u16>,
    /// Device ID.
    pub device_id: Option<u16>,
    /// Required server clusters.
    pub in_clusters: Vec<u16>,
    /// Required client clusters.
    pub out_clusters: Vec<u16>,
}

/// GP match (13.3).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GpMatch {
    /// DeviceID.
    pub device_id: Option<u8>,
    /// Manufacturer ID.
    pub manufacturer_id: Option<u16>,
    /// Model ID `(value, mask)`.
    pub model_id: Option<(u16, u16)>,
    /// SrcID range.
    pub src_id: Option<(u32, u32)>,
    /// IEEE prefix (high bytes) and its length in bytes.
    pub ieee_prefix: Option<(u64, u8)>,
    /// Commands that must all be in the GPD command list.
    pub commands: Vec<u8>,
    /// Server clusters subset.
    pub server_clusters: Vec<u16>,
    /// Client clusters subset.
    pub client_clusters: Vec<u16>,
}

/// One match clause (6.2); fields AND-ed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MatchClause {
    /// Basic ManufacturerName.
    pub manufacturer: Option<StrMatch>,
    /// Basic ModelIdentifier.
    pub model: Option<StrMatch>,
    /// Node descriptor manufacturer code.
    pub mfr_code: Option<u16>,
    /// Basic ApplicationVersion range.
    pub app_version: Option<(u32, u32)>,
    /// Basic StackVersion range.
    pub stack_version: Option<(u32, u32)>,
    /// Basic HWVersion range.
    pub hw_version: Option<(u32, u32)>,
    /// Basic SWBuildID.
    pub sw_build: Option<StrMatch>,
    /// Power source.
    pub power: Option<PowerKind>,
    /// Endpoint structure.
    pub endpoints: Vec<EpMatch>,
    /// Green Power.
    pub gp: Option<GpMatch>,
}

/// White label (6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhiteLabel {
    /// Vendor.
    pub vendor: String,
    /// Model.
    pub model: String,
    /// Manufacturer names that select it.
    pub manufacturer: Vec<String>,
    /// Basic ModelIdentifiers that select it.
    pub model_id: Vec<String>,
}

impl WhiteLabel {
    /// Whether a device with these Basic names is this white label (every
    /// list given contains its name, at least one list given).
    pub fn selects(&self, manufacturer_name: Option<&str>, model_id: Option<&str>) -> bool {
        use zdef::matching::trim_name;
        let has = |list: &[String], v: Option<&str>| {
            list.is_empty() || v.is_some_and(|v| list.iter().any(|n| trim_name(n) == trim_name(v)))
        };
        (!self.manufacturer.is_empty() || !self.model_id.is_empty())
            && has(&self.manufacturer, manufacturer_name)
            && has(&self.model_id, model_id)
    }
}

/// `self_description:` (3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelfDescMode {
    /// Above the catalog (default, D11).
    #[default]
    Over,
    /// Below the catalog.
    Under,
    /// Dropped.
    Ignore,
}

/// Layer source (4.2 `source`, 3.3).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum LayerSource {
    /// Tier a.
    Generic,
    /// Built-in catalog bank.
    Catalog,
    /// Fetched on demand.
    Fetched,
    /// Device schema.
    SelfDescription,
    /// Uploaded by the user.
    User,
}

/// A definition or a self-description fragment.
#[derive(Debug, Clone)]
pub struct Definition {
    /// `id`.
    pub id: String,
    /// `revision`.
    pub revision: u32,
    /// `vendor`.
    pub vendor: String,
    /// `model`.
    pub model: String,
    /// Match clauses (empty for fragments).
    pub matches: Vec<MatchClause>,
    /// White labels.
    pub white_label: Vec<WhiteLabel>,
    /// Required hub features (8.5).
    pub requires: Vec<String>,
    /// Tie-break.
    pub priority: i32,
    /// `device`.
    pub device: DeviceFields,
    /// `generic`.
    pub generic: GenericKnobs,
    /// Endpoint display names.
    pub endpoints: BTreeMap<u8, String>,
    /// `self_description`.
    pub self_description: SelfDescMode,
    /// Configure ops.
    pub configure: Vec<ConfigureOp>,
    /// Ignore rules.
    pub ignore: Vec<IgnoreRule>,
    /// Entities.
    pub entities: Vec<EntityDef>,
    /// Tuya flags.
    pub tuya: Option<TuyaFlags>,
    /// GP flags.
    pub gp: Option<GpFlags>,
    /// `mfr` of a fragment.
    pub mfr: Option<u16>,
    /// Source.
    pub source: LayerSource,
    /// SHA-256 of the standalone CBOR (8.6), hex; set by the store.
    pub content_hash: Option<String>,
    /// The `zdef` definition this was adapted from (matching, VM
    /// constants).
    pub zdef: Option<std::sync::Arc<zdef::Definition>>,
}

impl Definition {
    /// An empty definition.
    pub fn new(id: impl Into<String>, source: LayerSource) -> Self {
        Self {
            id: id.into(),
            revision: 1,
            vendor: String::new(),
            model: String::new(),
            matches: Vec::new(),
            white_label: Vec::new(),
            requires: Vec::new(),
            priority: 0,
            device: DeviceFields::default(),
            generic: GenericKnobs::default(),
            endpoints: BTreeMap::new(),
            self_description: SelfDescMode::Over,
            configure: Vec::new(),
            ignore: Vec::new(),
            entities: Vec::new(),
            tuya: None,
            gp: None,
            mfr: None,
            source,
            content_hash: None,
            zdef: None,
        }
    }

    /// Features this definition uses (what `zdefc` would compute for
    /// `requires`, 8.5), merged with the declared ones.
    pub fn used_features(&self) -> Vec<String> {
        let mut f: Vec<String> = self.requires.clone();
        let mut add = |s: &str| {
            if !f.iter().any(|x| x == s) {
                f.push(s.to_string());
            }
        };
        if self.tuya.is_some() {
            add("tuya");
        }
        if self.gp.is_some() || self.matches.iter().any(|m| m.gp.is_some()) {
            add("gp");
        }
        for e in &self.entities {
            for (_, v) in e.values() {
                if let Some(c) = &v.transforms.codec {
                    add(c.feature());
                }
                if let Binding::Struct { codec, .. } = &v.binding {
                    add(codec.feature());
                }
                if matches!(v.binding, Binding::Tuya { .. }) {
                    add("tuya");
                }
                if v.transforms.from.is_some() || v.transforms.to.is_some() {
                    add("expr-v1");
                }
            }
            if e.reset_after.is_some() || e.present_if.is_some() || !e.triggers.is_empty() {
                add("expr-v1");
            }
        }
        f
    }
}
