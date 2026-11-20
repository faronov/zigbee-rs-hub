//! Definition data model (spec 6, 8.2).
//!
//! One model for full definitions (`zdef: 1`) and self-description fragments
//! (`zdef_fragment: 1`). Optional fields are `Option`; lists use "empty =
//! absent" (the compiler rejects empty lists in the source, so this is
//! lossless). Strings are resolved: packed Tuya names, white-label name
//! indices, `event_types` and `attrs` constant indices are decoded into
//! plain strings and re-derived by the encoder.
//!
//! Expressions are verified bytecode ([`Expr`]) whose constant indices refer
//! to [`Definition::consts`].

use alloc::string::String;
use alloc::vec::Vec;

pub use crate::enums::{Category, Platform, StateClass, TableRef, TuyaType};

/// A number field: `mul`, `div`, `offset`, `min`, `max`, `step`, `invalid`,
/// reportable change. Integral values are always `Int` (spec 8.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    pub fn as_f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }

    /// Normalises an integral float to `Int` (spec 8.1).
    pub fn normalized(self) -> Num {
        match self {
            Num::Float(f)
                if f.is_finite()
                    && f == libm::trunc(f)
                    && libm::fabs(f) < 9.007_199_254_740_992e15 =>
            {
                Num::Int(f as i64)
            }
            n => n,
        }
    }
}

/// A literal value: `value_on`/`value_off`, written values, `hub.default`,
/// constant-pool map entries.
#[derive(Clone, Debug, PartialEq)]
pub enum Scalar {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

/// Verified expression bytecode (spec 7.5). Constant indices refer to the
/// owning definition's [`Definition::consts`].
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Expr(pub Vec<u8>);

impl Expr {
    pub fn code(&self) -> &[u8] {
        &self.0
    }
}

/// Constant-pool entry (top-level key 17).
#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Str(String),
    /// Always encoded as a float (or tag-4 decimal), never as an integer, so
    /// `x * 2.0` keeps float semantics.
    Float(f64),
    Int(i64),
    /// `lookup` map literal, encoded as `[[key, value], ...]` in source order.
    Map(Vec<(Scalar, Scalar)>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `zdef: <major>`
    Definition,
    /// `zdef_fragment: <major>` (self-description, spec 12.1)
    Fragment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    Mains,
    Battery,
    EnergyHarvesting,
}

impl Power {
    pub fn as_str(self) -> &'static str {
        match self {
            Power::Mains => "mains",
            Power::Battery => "battery",
            Power::EnergyHarvesting => "energy_harvesting",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "mains" => Power::Mains,
            "battery" => Power::Battery,
            "energy_harvesting" => Power::EnergyHarvesting,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelfDescription {
    Ignore,
    Under,
    Over,
}

impl SelfDescription {
    pub fn as_str(self) -> &'static str {
        match self {
            SelfDescription::Ignore => "ignore",
            SelfDescription::Under => "under",
            SelfDescription::Over => "over",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ignore" => SelfDescription::Ignore,
            "under" => SelfDescription::Under,
            "over" => SelfDescription::Over,
            _ => return None,
        })
    }
}

/// A complete definition or a self-description fragment.
#[derive(Clone, Debug, PartialEq)]
pub struct Definition {
    pub kind: Kind,
    /// Schema major version (`zdef` or `zdef_fragment` value).
    pub major: u8,
    pub id: Option<String>,
    pub revision: Option<u32>,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub description: Option<String>,
    pub match_: Vec<MatchClause>,
    pub white_label: Vec<WhiteLabel>,
    /// Feature IDs (spec 8.5), ascending, unique.
    pub requires: Vec<u16>,
    pub device: Option<Device>,
    pub generic: Option<Generic>,
    pub endpoints: Vec<(u8, String)>,
    pub tuya: Option<Tuya>,
    pub configure: Vec<ConfigureOp>,
    pub entities: Vec<Entity>,
    pub gp: Option<GpDevice>,
    pub ignore: Vec<IgnoreRule>,
    pub consts: Vec<Const>,
    pub source: Option<String>,
    pub mfr: Option<u16>,
    pub priority: Option<i64>,
    pub self_description: Option<SelfDescription>,
    /// Reserved `x-wasm` block (spec 6.9), kept as raw CBOR.
    pub x_wasm: Option<Vec<u8>>,
    /// Optional-extension keys 64..=127 (raw CBOR values), ascending.
    pub ext: Vec<(u64, Vec<u8>)>,
}

impl Definition {
    pub fn new(kind: Kind) -> Self {
        Definition {
            kind,
            major: 1,
            id: None,
            revision: None,
            vendor: None,
            model: None,
            description: None,
            match_: Vec::new(),
            white_label: Vec::new(),
            requires: Vec::new(),
            device: None,
            generic: None,
            endpoints: Vec::new(),
            tuya: None,
            configure: Vec::new(),
            entities: Vec::new(),
            gp: None,
            ignore: Vec::new(),
            consts: Vec::new(),
            source: None,
            mfr: None,
            priority: None,
            self_description: None,
            x_wasm: None,
            ext: Vec::new(),
        }
    }

    pub fn is_fragment(&self) -> bool {
        self.kind == Kind::Fragment
    }

    /// The manufacturer names of all match clauses in order (exact names and
    /// `any` lists): the list white-label indices refer to (spec 8.4).
    pub fn match_names(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for c in &self.match_ {
            match &c.manufacturer {
                Some(NameMatch::Exact(s)) => out.push(s.as_str()),
                Some(NameMatch::Any(v)) => out.extend(v.iter().map(|s| s.as_str())),
                _ => {}
            }
        }
        out
    }

    /// Display identity for a device's manufacturer name (white labels, spec 6.1).
    pub fn display_identity<'s>(
        &'s self,
        manufacturer_name: Option<&str>,
    ) -> (Option<&'s str>, Option<&'s str>) {
        if let Some(m) = manufacturer_name {
            let m = crate::matching::trim_name(m);
            for wl in &self.white_label {
                if wl.manufacturer.iter().any(|n| n == m) {
                    return (
                        wl.vendor.as_deref().or(self.vendor.as_deref()),
                        wl.model.as_deref().or(self.model.as_deref()),
                    );
                }
            }
        }
        (self.vendor.as_deref(), self.model.as_deref())
    }

    pub fn entity(&self, key: &str) -> Option<&Entity> {
        self.entities.iter().find(|e| e.key.as_deref() == Some(key))
    }
}

/// One match clause; all present fields must match (spec 6.2, 13.3).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct MatchClause {
    pub manufacturer: Option<NameMatch>,
    pub model: Option<NameMatch>,
    pub mfr_code: Option<u16>,
    pub app_version: Option<VersionRange>,
    pub stack_version: Option<VersionRange>,
    pub hw_version: Option<VersionRange>,
    pub sw_build: Option<NameMatch>,
    pub power: Option<Power>,
    pub endpoints: Vec<(u8, EpMatch)>,
    pub gp: Option<GpMatch>,
}

/// String matcher. CBOR: text = exact, array = any of, `{18: s}` = prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameMatch {
    Exact(String),
    Any(Vec<String>),
    Prefix(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct VersionRange {
    pub min: Option<i64>,
    pub max: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct EpMatch {
    pub profile: Option<u16>,
    pub device_id: Option<u16>,
    pub in_clusters: Vec<u16>,
    pub out_clusters: Vec<u16>,
}

/// Green Power match fields (spec 13.3).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct GpMatch {
    pub device_id: Option<u8>,
    pub manufacturer_id: Option<u16>,
    pub model_id: Option<ModelId>,
    /// Inclusive SrcID range.
    pub src_id: Option<(u32, u32)>,
    /// IEEE prefix as lowercase hex digits (app ID 2 GPDs).
    pub ieee: Option<String>,
    pub commands: Vec<u8>,
    pub clusters: Option<GpClusters>,
}

/// `model_id: 0xF300` (`mask: None`) or `{value, mask}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelId {
    pub value: u16,
    pub mask: Option<u16>,
}

impl ModelId {
    pub fn matches(&self, id: u16) -> bool {
        let m = self.mask.unwrap_or(0xffff);
        id & m == self.value & m
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct GpClusters {
    pub server: Vec<u16>,
    pub client: Vec<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WhiteLabel {
    pub vendor: Option<String>,
    pub model: Option<String>,
    /// Manufacturer names (encoded as indices into [`Definition::match_names`]).
    pub manufacturer: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Device {
    pub power: Option<Power>,
    pub sleepy: Option<bool>,
    pub battery: Option<Battery>,
    pub checkin_s: Option<u32>,
    pub availability: Option<Availability>,
    pub ota: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Battery {
    /// `{voltage_to_percent: [min_mV, max_mV]}`
    Linear { min_mv: u32, max_mv: u32 },
    /// Named curve (`3V_2100`, `3V_1500_2800`, `CR2032`).
    Curve(String),
}

pub const BATTERY_CURVES: &[&str] = &["3V_2100", "3V_1500_2800", "CR2032"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Availability {
    pub timeout_s: Option<u32>,
}

/// Tier-a knobs (spec 5.5).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Generic {
    pub exclude: Vec<String>,
    /// Field-level patches by generic entity key (sorted by encoded key).
    pub patch: Vec<(String, Entity)>,
    pub disable: Option<bool>,
    pub only_endpoints: Vec<u8>,
}

/// Tuya EF00 flags (spec 10.3).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Tuya {
    pub time_sync: Option<TimeSync>,
    pub magic_packet: Option<bool>,
    pub query: Option<TuyaQuery>,
    pub force_time_sync_s: Option<u32>,
    pub bind_basic: Option<bool>,
    pub gateway_status: Option<bool>,
    pub send: Option<TuyaSend>,
    pub mcu_version: Option<bool>,
    pub optimistic: Option<bool>,
    pub dedup: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeSync {
    Off,
    Epoch1970,
    Epoch2000,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TuyaQuery {
    /// `[configure, announce]`
    On { configure: bool, announce: bool },
    /// `{interval_s}`
    Interval(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuyaSend {
    DataRequest,
    SendData,
}

/// Configure op (spec 6.8), one kind per list item.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigureOp {
    Bind {
        ep: Option<u8>,
        clusters: Vec<u16>,
    },
    Write(WriteAttr),
    Read {
        ep: Option<u8>,
        cluster: u16,
        attrs: Vec<u16>,
        mfr: Option<u16>,
    },
    Report {
        ep: Option<u8>,
        cluster: u16,
        attr: u16,
        zcl_type: u8,
        report: Report,
        mfr: Option<u16>,
    },
    Cmd {
        ep: Option<u8>,
        cluster: u16,
        id: u8,
        mfr: Option<u16>,
    },
    Group {
        ep: Option<u8>,
        group: u16,
    },
    Checkin {
        interval_qs: u32,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct WriteAttr {
    pub ep: Option<u8>,
    pub cluster: u16,
    pub attr: u16,
    pub zcl_type: u8,
    pub value: Scalar,
    pub mfr: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct GpDevice {
    pub security_min: Option<u8>,
    pub report_interval_s: Option<u32>,
    pub ignore_commands: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IgnoreRule {
    pub ep: Option<u8>,
    pub cluster: u16,
    pub cmd: Option<u8>,
}

/// Entity, `parts` entry or `generic.patch` entry (spec 6.3). In a full
/// entity `key` and `platform` are present; parts and patches omit them.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Entity {
    pub key: Option<String>,
    pub platform: Option<Platform>,
    pub device_class: Option<TableRef>,
    pub unit: Option<TableRef>,
    pub state_class: Option<StateClass>,
    pub category: Option<Category>,
    pub name: Option<String>,
    pub ep: Option<u8>,
    pub zcl: Option<ZclBinding>,
    pub tuya: Option<TuyaDp>,
    pub struct_: Option<StructBinding>,
    pub gp: Option<GpBinding>,
    pub from: Option<Expr>,
    pub to: Option<Expr>,
    pub mul: Option<Num>,
    pub div: Option<Num>,
    pub offset: Option<Num>,
    pub precision: Option<u8>,
    pub invalid: Option<Num>,
    /// `map` transform: raw value -> label, in source (option) order.
    pub map: Vec<(i64, String)>,
    pub value_on: Option<Scalar>,
    pub value_off: Option<Scalar>,
    pub min: Option<Num>,
    pub max: Option<Num>,
    pub step: Option<Num>,
    pub mode: Option<String>,
    /// Access bits (`enums::ACCESS_*`).
    pub access: Option<u8>,
    pub report: Option<Report>,
    pub enabled: Option<bool>,
    pub reset_after: Option<Expr>,
    /// Composite parts by part key (a [`crate::keys::SUB`] key, see `keys::PART_NAMES`).
    pub parts: Vec<(u16, Entity)>,
    pub event_types: Vec<String>,
    pub triggers: Vec<Trigger>,
    pub press: Option<Press>,
    pub codec: Option<Codec>,
    pub present_if: Option<Expr>,
    pub attrs: Vec<(String, Expr)>,
    pub hub: Option<HubBinding>,
    pub invert: Option<bool>,
    /// Optional-extension keys 64..=127.
    pub ext: Vec<(u64, Vec<u8>)>,
}

impl Entity {
    /// Effective access bits.
    pub fn access_bits(&self) -> u8 {
        self.access
            .or_else(|| self.platform.map(|p| p.default_access()))
            .unwrap_or(0)
    }

    pub fn part(&self, name: &str) -> Option<&Entity> {
        let k = crate::keys::sub_key(name)?;
        self.parts.iter().find(|p| p.0 == k).map(|p| &p.1)
    }

    /// Visits every expression of this entity (and its parts) in canonical order.
    pub fn for_each_expr<'s>(&'s self, f: &mut dyn FnMut(crate::expr::Site, &'s Expr)) {
        use crate::expr::Site;
        if let Some(e) = &self.from {
            f(Site::From, e);
        }
        if let Some(e) = &self.to {
            f(Site::To, e);
        }
        if let Some(e) = &self.reset_after {
            f(Site::ResetAfter, e);
        }
        for (_, p) in &self.parts {
            p.for_each_expr(f);
        }
        for t in &self.triggers {
            for (_, e) in &t.attrs {
                f(Site::Attr, e);
            }
            if let Some(e) = &t.when {
                f(Site::When, e);
            }
            f(Site::Event, &t.event);
        }
        if let Some(Press::Cmd {
            payload: Some(e), ..
        }) = &self.press
        {
            f(Site::PressPayload, e);
        }
        if let Some(e) = &self.present_if {
            f(Site::PresentIf, e);
        }
        for (_, e) in &self.attrs {
            f(Site::Attr, e);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZclBinding {
    pub cluster: u16,
    pub attr: u16,
    pub zcl_type: u8,
    pub mfr: Option<u16>,
    pub ep: Option<u8>,
}

/// `tuya: [dp, type]`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TuyaDp {
    pub dp: u8,
    pub ty: TuyaType,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructBinding {
    pub cluster: u16,
    pub attr: u16,
    pub mfr: Option<u16>,
    pub ep: Option<u8>,
    /// Named struct codec (`zcl_tagged`).
    pub codec: String,
    pub tag: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpBinding {
    Report {
        cluster: u16,
        attr: u16,
        zcl_type: u8,
        mfr: Option<u16>,
    },
    Cmd(u8),
}

#[derive(Clone, Debug, PartialEq)]
pub struct HubBinding {
    pub default: Option<Scalar>,
}

/// `report: [min, max, change]` or `off`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Report {
    Off,
    Config {
        min: u16,
        max: u16,
        change: Option<Num>,
    },
}

/// Event trigger (spec 6.7).
#[derive(Clone, Debug, PartialEq)]
pub struct Trigger {
    pub attrs: Vec<(String, Expr)>,
    pub cmd: Option<CmdRef>,
    pub gp_cmd: Option<u8>,
    pub tuya: Option<TuyaDp>,
    pub when: Option<Expr>,
    pub event: Expr,
}

/// A ZCL command reference (`{cluster, id, mfr?, ep?}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CmdRef {
    pub cluster: u16,
    pub id: u8,
    pub mfr: Option<u16>,
    pub ep: Option<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Press {
    Cmd { cmd: CmdRef, payload: Option<Expr> },
    Write(WriteAttr),
    Tuya { dp: TuyaDp, value: Scalar },
}

/// Named codec (spec 6.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Codec {
    ZclTagged,
    TuyaScheduleDay { day: u8, transitions: u8 },
    TuyaScheduleWeek { variant: String },
    BcdTime,
    Hhmm,
    Ascii,
    Utf16beHex,
    BitmapFlags(Vec<(u8, String)>),
}

impl Codec {
    pub fn name(&self) -> &'static str {
        match self {
            Codec::ZclTagged => "zcl_tagged",
            Codec::TuyaScheduleDay { .. } => "tuya_schedule_day",
            Codec::TuyaScheduleWeek { .. } => "tuya_schedule_week",
            Codec::BcdTime => "bcd_time",
            Codec::Hhmm => "hhmm",
            Codec::Ascii => "ascii",
            Codec::Utf16beHex => "utf16be_hex",
            Codec::BitmapFlags(_) => "bitmap_flags",
        }
    }

    pub fn feature(&self) -> crate::enums::Feature {
        use crate::enums::Feature as F;
        match self {
            Codec::ZclTagged => F::StructZclTagged,
            Codec::TuyaScheduleDay { .. } => F::CodecTuyaScheduleDay,
            Codec::TuyaScheduleWeek { .. } => F::CodecTuyaScheduleWeek,
            Codec::BcdTime => F::CodecBcdTime,
            Codec::Hhmm => F::CodecHhmm,
            Codec::Ascii => F::CodecAscii,
            Codec::Utf16beHex => F::CodecUtf16beHex,
            Codec::BitmapFlags(_) => F::CodecBitmapFlags,
        }
    }
}

/// Struct codecs usable in `struct.codec`.
pub const STRUCT_CODECS: &[&str] = &["zcl_tagged"];
