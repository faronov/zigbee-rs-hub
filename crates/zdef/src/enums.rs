//! Enumerations (spec 8.3) and the feature table (spec 8.5).
//!
//! Integer-coded tables are append-only within `zdef: 1`. Indices unknown to
//! this build decode as `Unknown(n)` so a newer catalog still produces the
//! entity, without that field (spec 8.3).

macro_rules! str_enum {
    ($(#[$m:meta])* $name:ident { $($v:ident = $n:literal => $s:literal),* $(,)? }) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name { $($v = $n),* }
        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$v),*];
            pub fn code(self) -> u8 { self as u8 }
            pub fn from_code(c: u64) -> Option<Self> {
                match c { $($n => Some($name::$v),)* _ => None }
            }
            pub fn as_str(self) -> &'static str {
                match self { $($name::$v => $s),* }
            }
            pub fn parse(s: &str) -> Option<Self> {
                match s { $($s => Some($name::$v),)* _ => None }
            }
        }
    };
}

str_enum!(
    /// HA platform of an entity (spec 4.3, 8.3).
    Platform {
        Sensor = 0 => "sensor", BinarySensor = 1 => "binary_sensor", Number = 2 => "number",
        Select = 3 => "select", Switch = 4 => "switch", Button = 5 => "button", Text = 6 => "text",
        Light = 7 => "light", Cover = 8 => "cover", Climate = 9 => "climate", Lock = 10 => "lock",
        Fan = 11 => "fan", Event = 12 => "event", Siren = 13 => "siren", Valve = 14 => "valve",
        Update = 15 => "update",
    }
);

impl Platform {
    /// Composite platforms are built from `parts` (spec 4.3).
    pub fn is_composite(self) -> bool {
        matches!(
            self,
            Platform::Light
                | Platform::Cover
                | Platform::Climate
                | Platform::Lock
                | Platform::Fan
                | Platform::Siren
                | Platform::Valve
        )
    }

    /// Default `access` bits per platform (spec 6.3).
    pub fn default_access(self) -> u8 {
        match self {
            Platform::Sensor | Platform::BinarySensor => ACCESS_READ | ACCESS_REPORT,
            Platform::Number | Platform::Select | Platform::Switch | Platform::Text => {
                ACCESS_READ | ACCESS_WRITE | ACCESS_REPORT
            }
            Platform::Button => ACCESS_WRITE,
            _ => ACCESS_READ | ACCESS_WRITE | ACCESS_REPORT,
        }
    }
}

str_enum!(
    StateClass {
        Measurement = 0 => "measurement", Total = 1 => "total", TotalIncreasing = 2 => "total_increasing",
        MeasurementAngle = 3 => "measurement_angle",
    }
);

str_enum!(
    /// HA entity category (`config` 1, `diagnostic` 2).
    Category { Config = 1 => "config", Diagnostic = 2 => "diagnostic" }
);

str_enum!(
    /// Power source (`device.power`, match `power`).
    Power { Mains = 0 => "mains", Battery = 1 => "battery", EnergyHarvesting = 2 => "energy_harvesting" }
);

str_enum!(
    /// `mode` of number and text entities (HA `NumberMode` / `TextMode`).
    Mode { Auto = 0 => "auto", Box = 1 => "box", Slider = 2 => "slider", Text = 3 => "text", Password = 4 => "password" }
);

str_enum!(
    /// `self_description` policy of a catalog or user definition (spec 3.3).
    SelfDescription { Ignore = 0 => "ignore", Under = 1 => "under", Over = 2 => "over" }
);

str_enum!(
    /// Tuya `time_sync` epoch (spec 10.3); the source writes `off`, `1970`, `2000`.
    TimeSync { Off = 0 => "off", Epoch1970 = 1 => "1970", Epoch2000 = 2 => "2000" }
);

str_enum!(
    /// Tuya write command (spec 10.3).
    TuyaSend { DataRequest = 0 => "dataRequest", SendData = 1 => "sendData" }
);

str_enum!(
    /// Node descriptor logical type (match `logical_type`).
    LogicalType { Coordinator = 0 => "coordinator", Router = 1 => "router", EndDevice = 2 => "end_device" }
);

str_enum!(
    /// Named battery curve (`device.battery`, spec 6.1).
    BatteryCurve { V3_2100 = 0 => "3V_2100", V3_1500_2800 = 1 => "3V_1500_2800", Cr2032 = 2 => "CR2032" }
);

impl BatteryCurve {
    /// Linear range (mV) the hub uses for the curve.
    pub fn range_mv(self) -> (u32, u32) {
        match self {
            BatteryCurve::V3_2100 => (2100, 3000),
            BatteryCurve::V3_1500_2800 => (1500, 2800),
            BatteryCurve::Cr2032 => (2000, 3000),
        }
    }
}

str_enum!(
    /// Named codecs (spec 6.6); also the `struct.codec` value.
    CodecName {
        ZclTagged = 0 => "zcl_tagged", TuyaScheduleDay = 1 => "tuya_schedule_day",
        TuyaScheduleWeek = 2 => "tuya_schedule_week", BcdTime = 3 => "bcd_time", Hhmm = 4 => "hhmm",
        Ascii = 5 => "ascii", Utf16beHex = 6 => "utf16be_hex", BitmapFlags = 7 => "bitmap_flags",
    }
);

str_enum!(
    /// Byte layout of a Tuya schedule DP (zigbee-herdsman-converters value
    /// converters): `multi_dp` = `thermostatScheduleDayMultiDP[WithTransitionCount]`
    /// (`WithDayNumber` for `tuya_schedule_day`), `single_dp` =
    /// `thermostatScheduleDaySingleDP`, `trv602z` = `…_TRV602Z`, `trv603wz` = `…_TRV603WZ`.
    ScheduleVariant { MultiDp = 0 => "multi_dp", SingleDp = 1 => "single_dp", Trv602z = 2 => "trv602z", Trv603wz = 3 => "trv603wz" }
);

str_enum!(
    /// Vendor templates (C tier, spec 17.4); feature ID = 20 + code.
    TemplateName { HueLight = 0 => "hue_light", Lumi = 1 => "lumi", Tuya = 2 => "tuya", TuyaMeteringZeroDebounce = 3 => "tuya_metering_zero_debounce" }
);

impl TemplateName {
    pub fn feature(self) -> u16 {
        20 + self.code() as u16
    }
}

str_enum!(
    /// Tuya DP type codes (spec 10.1).
    TuyaType {
        Raw = 0 => "raw", Bool = 1 => "bool", Value = 2 => "value", String = 3 => "string",
        Enum = 4 => "enum", Bitmap = 5 => "bitmap",
    }
);

pub const ACCESS_READ: u8 = 1;
pub const ACCESS_WRITE: u8 = 2;
pub const ACCESS_REPORT: u8 = 4;

/// ZCL data types by source name -> ZCL type code (spec 6.3, 8.3).
pub const ZCL_TYPES: &[(&str, u8)] = &[
    ("data8", 0x08),
    ("data16", 0x09),
    ("data24", 0x0A),
    ("data32", 0x0B),
    ("bool", 0x10),
    ("map8", 0x18),
    ("map16", 0x19),
    ("map24", 0x1A),
    ("map32", 0x1B),
    ("u8", 0x20),
    ("u16", 0x21),
    ("u24", 0x22),
    ("u32", 0x23),
    ("u40", 0x24),
    ("u48", 0x25),
    ("u56", 0x26),
    ("u64", 0x27),
    ("i8", 0x28),
    ("i16", 0x29),
    ("i24", 0x2A),
    ("i32", 0x2B),
    ("i40", 0x2C),
    ("i48", 0x2D),
    ("i56", 0x2E),
    ("i64", 0x2F),
    ("enum8", 0x30),
    ("enum16", 0x31),
    ("f16", 0x38),
    ("f32", 0x39),
    ("f64", 0x3A),
    ("ostr", 0x41),
    ("str", 0x42),
    ("ostr16", 0x43),
    ("str16", 0x44),
    ("tod", 0xE0),
    ("date", 0xE1),
    ("utc", 0xE2),
    ("cluster_id", 0xE8),
    ("attr_id", 0xE9),
    ("ieee", 0xF0),
    ("key128", 0xF1),
];

pub fn zcl_type_code(name: &str) -> Option<u8> {
    ZCL_TYPES.iter().find(|e| e.0 == name).map(|e| e.1)
}

pub fn zcl_type_name(code: u8) -> Option<&'static str> {
    ZCL_TYPES.iter().find(|e| e.1 == code).map(|e| e.0)
}

/// Fixed byte width of a ZCL type, `None` for strings.
pub fn zcl_type_width(code: u8) -> Option<u8> {
    Some(match code {
        0x08 | 0x10 | 0x18 | 0x20 | 0x28 | 0x30 => 1,
        0x09 | 0x19 | 0x21 | 0x29 | 0x31 | 0x38 | 0xE8 | 0xE9 => 2,
        0x0A | 0x1A | 0x22 | 0x2A => 3,
        0x0B | 0x1B | 0x23 | 0x2B | 0x39 | 0xE0 | 0xE1 | 0xE2 => 4,
        0x24 | 0x2C => 5,
        0x25 | 0x2D => 6,
        0x26 | 0x2E => 7,
        0x27 | 0x2F | 0x3A | 0xF0 => 8,
        0xF1 => 16,
        _ => return None,
    })
}

/// HA device classes: one append-only table over all platforms (spec 8.3).
/// The first 60 entries keep the prototype's order (byte compatibility of
/// the examples); the rest are appended from HA's `*DeviceClass` enums.
pub const DEVICE_CLASSES: &[&str] = &[
    "temperature",
    "humidity",
    "pressure",
    "illuminance",
    "battery",
    "voltage",
    "current",
    "power",
    "energy",
    "power_factor",
    "frequency",
    "carbon_dioxide",
    "carbon_monoxide",
    "pm25",
    "pm10",
    "pm1",
    "volatile_organic_compounds",
    "moisture",
    "occupancy",
    "motion",
    "door",
    "window",
    "opening",
    "smoke",
    "gas",
    "problem",
    "tamper",
    "vibration",
    "enum",
    "duration",
    "distance",
    "signal_strength",
    "button",
    "restart",
    "identify",
    "update",
    "outlet",
    "switch",
    "connectivity",
    "plug",
    "running",
    "safety",
    "sound",
    "lock",
    "cold",
    "heat",
    "light",
    "presence",
    "water",
    "blind",
    "curtain",
    "shade",
    "shutter",
    "garage",
    "gate",
    "awning",
    "damper",
    "doorbell",
    "water_heater",
    "atmospheric_pressure",
    // 60..
    "absolute_humidity",
    "apparent_power",
    "aqi",
    "area",
    "blood_glucose_concentration",
    "conductivity",
    "data_rate",
    "data_size",
    "date",
    "energy_distance",
    "energy_storage",
    "irradiance",
    "monetary",
    "nitrogen_dioxide",
    "nitrogen_monoxide",
    "nitrous_oxide",
    "ozone",
    "ph",
    "pm4",
    "precipitation",
    "precipitation_intensity",
    "reactive_energy",
    "reactive_power",
    "sound_pressure",
    "speed",
    "sulphur_dioxide",
    "timestamp",
    "volatile_organic_compounds_parts",
    "volume",
    "volume_flow_rate",
    "volume_storage",
    "weight",
    "wind_direction",
    "wind_speed",
    "battery_charging",
    "garage_door",
    "moving",
    "firmware",
];

/// HA unit strings: one append-only table (spec 8.3). First 39 entries in
/// the prototype's order.
pub const UNITS: &[&str] = &[
    "°C",
    "°F",
    "K",
    "%",
    "hPa",
    "kPa",
    "Pa",
    "lx",
    "ppm",
    "ppb",
    "µg/m³",
    "W",
    "kW",
    "Wh",
    "kWh",
    "V",
    "mV",
    "A",
    "mA",
    "Hz",
    "VA",
    "var",
    "s",
    "min",
    "h",
    "d",
    "ms",
    "m",
    "cm",
    "mm",
    "km",
    "m³",
    "L",
    "m³/h",
    "L/min",
    "dB",
    "dBm",
    "mired",
    "°",
    // 39..
    "mW",
    "MW",
    "GW",
    "MWh",
    "GWh",
    "varh",
    "kvar",
    "kvarh",
    "kVA",
    "kV",
    "µV",
    "µA",
    "kHz",
    "MHz",
    "GHz",
    "µs",
    "w",
    "m/s",
    "km/h",
    "mph",
    "kn",
    "ft",
    "in",
    "yd",
    "mi",
    "µm",
    "g",
    "kg",
    "mg",
    "µg",
    "oz",
    "lb",
    "st",
    "mL",
    "gal",
    "fl. oz.",
    "ft³",
    "CCF",
    "gal/min",
    "ft³/min",
    "m³/s",
    "mm/h",
    "in/h",
    "mm/d",
    "in/d",
    "mbar",
    "bar",
    "psi",
    "inHg",
    "mmHg",
    "cbar",
    "µS/cm",
    "mS/cm",
    "S/cm",
    "lm",
    "W/m²",
    "B",
    "kB",
    "MB",
    "GB",
    "bit/s",
    "kbit/s",
    "Mbit/s",
    "mg/m³",
    "µg/ft³",
    "p/m³",
    "mg/dL",
    "mmol/L",
    "kWh/100km",
    "Wh/km",
    "mi/kWh",
    "km/kWh",
    "dBA",
    "pH",
    "m²",
];

pub fn device_class_code(s: &str) -> Option<u16> {
    DEVICE_CLASSES
        .iter()
        .position(|c| *c == s)
        .map(|i| i as u16)
}

pub fn unit_code(s: &str) -> Option<u16> {
    UNITS.iter().position(|c| *c == s).map(|i| i as u16)
}

/// Device-class or unit reference. `Unknown` keeps an index from a newer table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TableRef {
    Known(u16),
    Unknown(u16),
}

impl TableRef {
    pub fn code(self) -> u16 {
        match self {
            TableRef::Known(c) | TableRef::Unknown(c) => c,
        }
    }
}

str_enum!(
    /// Hub feature IDs for `requires` (spec 8.5), append-only.
    Feature {
        ExprV1 = 1 => "expr-v1", Tuya = 2 => "tuya", Gp = 3 => "gp",
        StructZclTagged = 4 => "struct.zcl_tagged", CodecTuyaScheduleDay = 5 => "codec.tuya_schedule_day",
        CodecTuyaScheduleWeek = 6 => "codec.tuya_schedule_week", CodecBcdTime = 7 => "codec.bcd_time",
        CodecHhmm = 8 => "codec.hhmm", CodecAscii = 9 => "codec.ascii",
        CodecUtf16beHex = 10 => "codec.utf16be_hex", CodecBitmapFlags = 11 => "codec.bitmap_flags",
        TemplateHueLight = 20 => "template.hue_light", TemplateLumi = 21 => "template.lumi",
        TemplateTuya = 22 => "template.tuya", TemplateTuyaMeteringZeroDebounce = 23 => "template.tuya_metering_zero_debounce",
        Wasm = 60 => "wasm",
    }
);

/// Set of features a hub build supports (bit n = feature ID n, IDs < 128).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct HubFeatures(pub u128);

impl HubFeatures {
    /// Everything this crate version implements (all v1 features except `wasm`).
    pub const fn v1() -> Self {
        let mut bits = 0u128;
        let mut i = 1;
        while i <= 11 {
            bits |= 1 << i;
            i += 1;
        }
        HubFeatures(bits)
    }

    pub fn with(mut self, f: u16) -> Self {
        if f < 128 {
            self.0 |= 1 << f;
        }
        self
    }

    pub fn without(mut self, f: u16) -> Self {
        if f < 128 {
            self.0 &= !(1 << f);
        }
        self
    }

    pub fn has(&self, f: u16) -> bool {
        f < 128 && self.0 & (1 << f) != 0
    }

    /// First listed feature this hub lacks.
    pub fn missing(&self, requires: &[u16]) -> Option<u16> {
        requires.iter().copied().find(|f| !self.has(*f))
    }
}

/// Tuya manufacturer-name prefixes that are packed (spec 8.4), append-only.
pub const TUYA_PREFIXES: &[&str] = &[
    "_TZE200_", "_TZE204_", "_TZE284_", "_TZ3000_", "_TZ3210_", "_TZE202_", "_TYZB01_", "_TZ3290_",
];
