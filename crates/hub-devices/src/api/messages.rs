//! WebSocket JSON API messages (spec 14), modelled on Home Assistant's
//! WebSocket API: `auth` / `auth_ok`, client-chosen `id`s, `result`
//! frames, `subscribe` with a snapshot and `seq`-numbered deltas.

use serde::{Deserialize, Serialize};
use serde_json::Value as J;

use crate::gp::bindings::{BindAction, BindTrigger, CreateBinding, ExecutorChoice};
use crate::hub::ApiError;

/// API version (`major.minor`; minors only add fields and types).
pub const API_VERSION: &str = "1.3";

/// Topics of `subscribe`.
pub const TOPICS: &[&str] = &[
    "devices",
    "entities",
    "events",
    "gp",
    "network",
    "catalog",
    #[cfg(feature = "advanced")]
    "frames",
    "automations",
    "groups",
    "ota",
];

/// The first frame from the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "auth")]
pub struct Auth {
    /// Access token.
    pub token: String,
}

/// A request with its client-chosen ID.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Envelope {
    /// Unique per connection.
    pub id: u64,
    /// The request.
    #[serde(flatten)]
    pub request: Request,
}

/// Requests (14.2–14.5, 13.7).
///
/// The envelope's `id` is the request ID (HA style), so parameters that
/// the spec tables also call `id` are named after what they address:
/// `device` (devices/*, gp/devices/remove), `definition`
/// (definitions/delete, definitions/get) and `binding` (gp/bindings/*).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
#[allow(missing_docs)]
pub enum Request {
    #[serde(rename = "subscribe")]
    Subscribe { topics: Vec<String> },
    #[serde(rename = "unsubscribe")]
    Unsubscribe { subscription: u64 },

    #[serde(rename = "hub/info")]
    HubInfo,

    #[serde(rename = "network/info")]
    NetworkInfo,
    #[serde(rename = "network/permit_join")]
    PermitJoin {
        duration_s: u8,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        include_gp: bool,
    },
    #[serde(rename = "devices/list")]
    DevicesList,
    #[serde(rename = "devices/get")]
    DevicesGet {
        #[serde(rename = "device")]
        id: String,
    },
    #[serde(rename = "devices/update")]
    DevicesUpdate {
        #[serde(rename = "device")]
        id: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        disabled: Option<bool>,
    },
    #[serde(rename = "devices/remove")]
    DevicesRemove {
        #[serde(rename = "device")]
        id: String,
        #[serde(default)]
        force: bool,
    },
    #[serde(rename = "devices/reinterview")]
    DevicesReinterview {
        #[serde(rename = "device")]
        id: String,
    },
    #[serde(rename = "devices/reconfigure")]
    DevicesReconfigure {
        #[serde(rename = "device")]
        id: String,
    },
    #[serde(rename = "devices/diagnostics")]
    DevicesDiagnostics {
        #[serde(rename = "device")]
        id: String,
    },
    #[serde(rename = "devices/pin_definition")]
    DevicesPinDefinition {
        #[serde(rename = "device")]
        id: String,
        definition: Option<String>,
    },

    #[serde(rename = "entities/list")]
    EntitiesList {
        #[serde(default)]
        device: Option<String>,
    },
    #[serde(rename = "entities/states")]
    EntitiesStates {
        #[serde(default)]
        uids: Option<Vec<String>>,
    },
    #[serde(rename = "entities/command")]
    EntitiesCommand {
        uid: String,
        command: String,
        #[serde(default)]
        params: J,
    },
    #[serde(rename = "entities/refresh")]
    EntitiesRefresh { uid: String },
    #[serde(rename = "entities/update")]
    EntitiesUpdate {
        uid: String,
        #[serde(serialize_with = "double_option::serialize")]
        name: Option<Option<String>>,
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(default)]
        value: Option<J>,
    },
    #[serde(rename = "entities/delete")]
    EntitiesDelete { uid: String },

    #[serde(rename = "definitions/validate")]
    DefinitionsValidate { cbor: String },
    #[serde(rename = "definitions/upload")]
    DefinitionsUpload {
        cbor: String,
        #[serde(default)]
        replace: Option<String>,
        /// The user's source text (YAML …), kept for editing (≤ 8 KiB).
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        source_format: Option<String>,
    },
    #[serde(rename = "definitions/delete")]
    DefinitionsDelete {
        #[serde(rename = "definition")]
        id: String,
    },
    #[serde(rename = "definitions/list")]
    DefinitionsList {
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        device: Option<String>,
        /// Substring of `id`, vendor or model (case-insensitive).
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },
    #[serde(rename = "definitions/get")]
    DefinitionsGet {
        #[serde(default, rename = "definition")]
        id: Option<String>,
        #[serde(default)]
        content_hash: Option<String>,
        #[serde(default)]
        format: Option<String>,
    },
    #[serde(rename = "definitions/preview")]
    DefinitionsPreview { device: String, cbor: String },
    #[serde(rename = "definitions/scaffold")]
    DefinitionsScaffold { device: String },
    #[serde(rename = "definitions/fetch")]
    DefinitionsFetch { device: String },
    #[serde(rename = "catalog/status")]
    CatalogStatus,

    #[serde(rename = "gp/commissioning/open")]
    GpCommissioningOpen {
        duration_s: u16,
        involve_proxies: bool,
        #[serde(default)]
        exit_on_first_pairing: bool,
    },
    #[serde(rename = "gp/commissioning/close")]
    GpCommissioningClose,
    #[serde(rename = "gp/commissioning/status")]
    GpCommissioningStatus,
    #[serde(rename = "gp/devices/list")]
    GpDevicesList,
    #[serde(rename = "gp/devices/remove")]
    GpDevicesRemove {
        #[serde(rename = "device")]
        id: String,
        send_remove: bool,
    },
    #[serde(rename = "gp/config")]
    GpConfig {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        security_level: Option<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        communication_mode: Option<u8>,
    },
    #[serde(rename = "gp/config/get")]
    GpConfigGet,
    #[serde(rename = "gp/bindings/list")]
    GpBindingsList {
        #[serde(default)]
        gpd: Option<String>,
        #[serde(default)]
        target: Option<String>,
    },
    #[serde(rename = "gp/bindings/create")]
    GpBindingsCreate(CreateBinding),
    #[serde(rename = "gp/bindings/update")]
    GpBindingsUpdate {
        #[serde(rename = "binding")]
        binding_id: u32,
        #[serde(default)]
        executor: Option<ExecutorChoice>,
        #[serde(default)]
        action: Option<BindAction>,
        #[serde(default)]
        trigger: Option<BindTrigger>,
    },
    #[serde(rename = "gp/bindings/delete")]
    GpBindingsDelete {
        #[serde(rename = "binding")]
        binding_id: u32,
    },
    #[serde(rename = "gp/bindings/verify")]
    GpBindingsVerify {
        #[serde(default, rename = "binding")]
        binding_id: Option<u32>,
    },
    #[serde(rename = "gp/targets/probe")]
    GpTargetsProbe { device: String },

    // ZCL console (14.7): the arguments stay JSON (parsed by
    // `console::request`; a derived struct costs ~9 KB of flash).
    #[serde(rename = "zcl/read")]
    ZclRead(J),
    #[serde(rename = "zcl/write")]
    ZclWrite(J),
    #[serde(rename = "zcl/configure_reporting")]
    ZclConfigureReporting(J),
    #[serde(rename = "zcl/read_reporting")]
    ZclReadReporting(J),
    #[serde(rename = "zcl/discover")]
    ZclDiscover(J),
    #[serde(rename = "zcl/command")]
    ZclCommand(J),
    #[serde(rename = "tuya/send")]
    TuyaSend(J),
}

// Requests are read by hand from one JSON object: the derived
// internally-tagged `Deserialize` buffered every frame as serde `Content`
// and took ~66 KB of flash for the 60 variants. Same rules as the derive:
// unknown fields are ignored, `Option`s and `default`s may be missing or
// null.

type Fields = serde_json::Map<String, J>;

fn take<T: serde::de::DeserializeOwned>(m: &mut Fields, k: &str) -> Result<T, String> {
    let v = m.remove(k).ok_or_else(|| format!("missing field `{k}`"))?;
    serde_json::from_value(v).map_err(|e| format!("{k}: {e}"))
}

fn opt<T: serde::de::DeserializeOwned>(m: &mut Fields, k: &str) -> Result<Option<T>, String> {
    match m.remove(k) {
        None | Some(J::Null) => Ok(None),
        Some(v) => serde_json::from_value(v)
            .map(Some)
            .map_err(|e| format!("{k}: {e}")),
    }
}

fn or_default<T: serde::de::DeserializeOwned + Default>(
    m: &mut Fields,
    k: &str,
) -> Result<T, String> {
    Ok(opt(m, k)?.unwrap_or_default())
}

impl Request {
    /// The request of a JSON object (`type` and its fields).
    pub fn from_fields(mut m: Fields) -> Result<Self, String> {
        use Request as R;
        let ty: String = take(&mut m, "type")?;
        let m = &mut m;
        Ok(match ty.as_str() {
            "subscribe" => R::Subscribe {
                topics: take(m, "topics")?,
            },
            "unsubscribe" => R::Unsubscribe {
                subscription: take(m, "subscription")?,
            },
            "hub/info" => R::HubInfo,
            "network/info" => R::NetworkInfo,
            "network/permit_join" => R::PermitJoin {
                duration_s: take(m, "duration_s")?,
                target: opt(m, "target")?,
                include_gp: or_default(m, "include_gp")?,
            },
            "devices/list" => R::DevicesList,
            "devices/get" => R::DevicesGet {
                id: take(m, "device")?,
            },
            "devices/update" => R::DevicesUpdate {
                id: take(m, "device")?,
                name: opt(m, "name")?,
                disabled: opt(m, "disabled")?,
            },
            "devices/remove" => R::DevicesRemove {
                id: take(m, "device")?,
                force: or_default(m, "force")?,
            },
            "devices/reinterview" => R::DevicesReinterview {
                id: take(m, "device")?,
            },
            "devices/reconfigure" => R::DevicesReconfigure {
                id: take(m, "device")?,
            },
            "devices/diagnostics" => R::DevicesDiagnostics {
                id: take(m, "device")?,
            },
            "devices/pin_definition" => R::DevicesPinDefinition {
                id: take(m, "device")?,
                definition: opt(m, "definition")?,
            },
            "entities/list" => R::EntitiesList {
                device: opt(m, "device")?,
            },
            "entities/states" => R::EntitiesStates {
                uids: opt(m, "uids")?,
            },
            "entities/command" => R::EntitiesCommand {
                uid: take(m, "uid")?,
                command: take(m, "command")?,
                params: m.remove("params").unwrap_or(J::Null),
            },
            "entities/refresh" => R::EntitiesRefresh {
                uid: take(m, "uid")?,
            },
            "entities/update" => R::EntitiesUpdate {
                uid: take(m, "uid")?,
                // Missing: unchanged; null: back to the default name.
                name: match m.remove("name") {
                    None => None,
                    Some(J::Null) => Some(None),
                    Some(v) => Some(Some(
                        serde_json::from_value(v).map_err(|e| format!("name: {e}"))?,
                    )),
                },
                enabled: opt(m, "enabled")?,
                value: opt(m, "value")?,
            },
            "entities/delete" => R::EntitiesDelete {
                uid: take(m, "uid")?,
            },
            "definitions/validate" => R::DefinitionsValidate {
                cbor: take(m, "cbor")?,
            },
            "definitions/upload" => R::DefinitionsUpload {
                cbor: take(m, "cbor")?,
                replace: opt(m, "replace")?,
                source: opt(m, "source")?,
                source_format: opt(m, "source_format")?,
            },
            "definitions/delete" => R::DefinitionsDelete {
                id: take(m, "definition")?,
            },
            "definitions/list" => R::DefinitionsList {
                source: opt(m, "source")?,
                device: opt(m, "device")?,
                query: opt(m, "query")?,
                offset: opt(m, "offset")?,
                limit: opt(m, "limit")?,
            },
            "definitions/get" => R::DefinitionsGet {
                id: opt(m, "definition")?,
                content_hash: opt(m, "content_hash")?,
                format: opt(m, "format")?,
            },
            "definitions/preview" => R::DefinitionsPreview {
                device: take(m, "device")?,
                cbor: take(m, "cbor")?,
            },
            "definitions/scaffold" => R::DefinitionsScaffold {
                device: take(m, "device")?,
            },
            "definitions/fetch" => R::DefinitionsFetch {
                device: take(m, "device")?,
            },
            "catalog/status" => R::CatalogStatus,
            "gp/commissioning/open" => R::GpCommissioningOpen {
                duration_s: take(m, "duration_s")?,
                involve_proxies: opt(m, "involve_proxies")?.unwrap_or(true),
                exit_on_first_pairing: or_default(m, "exit_on_first_pairing")?,
            },
            "gp/commissioning/close" => R::GpCommissioningClose,
            "gp/commissioning/status" => R::GpCommissioningStatus,
            "gp/devices/list" => R::GpDevicesList,
            "gp/devices/remove" => R::GpDevicesRemove {
                id: take(m, "device")?,
                send_remove: opt(m, "send_remove")?.unwrap_or(true),
            },
            "gp/config" => R::GpConfig {
                security_level: opt(m, "security_level")?,
                communication_mode: opt(m, "communication_mode")?,
            },
            "gp/config/get" => R::GpConfigGet,
            "gp/bindings/list" => R::GpBindingsList {
                gpd: opt(m, "gpd")?,
                target: opt(m, "target")?,
            },
            "gp/bindings/create" => R::GpBindingsCreate(
                serde_json::from_value(J::Object(std::mem::take(m))).map_err(|e| e.to_string())?,
            ),
            "gp/bindings/update" => R::GpBindingsUpdate {
                binding_id: take(m, "binding")?,
                executor: opt(m, "executor")?,
                action: opt(m, "action")?,
                trigger: opt(m, "trigger")?,
            },
            "gp/bindings/delete" => R::GpBindingsDelete {
                binding_id: take(m, "binding")?,
            },
            "gp/bindings/verify" => R::GpBindingsVerify {
                binding_id: opt(m, "binding")?,
            },
            "gp/targets/probe" => R::GpTargetsProbe {
                device: take(m, "device")?,
            },
            "zcl/read" => R::ZclRead(J::Object(std::mem::take(m))),
            "zcl/write" => R::ZclWrite(J::Object(std::mem::take(m))),
            "zcl/configure_reporting" => R::ZclConfigureReporting(J::Object(std::mem::take(m))),
            "zcl/read_reporting" => R::ZclReadReporting(J::Object(std::mem::take(m))),
            "zcl/discover" => R::ZclDiscover(J::Object(std::mem::take(m))),
            "zcl/command" => R::ZclCommand(J::Object(std::mem::take(m))),
            "tuya/send" => R::TuyaSend(J::Object(std::mem::take(m))),
            other => return Err(format!("unknown variant `{other}`")),
        })
    }
}

impl<'de> Deserialize<'de> for Request {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::from_fields(Fields::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

impl<'de> Deserialize<'de> for Envelope {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut m = Fields::deserialize(d)?;
        let id = take(&mut m, "id").map_err(serde::de::Error::custom)?;
        let request = Request::from_fields(m).map_err(serde::de::Error::custom)?;
        Ok(Self { id, request })
    }
}

mod double_option {
    use serde::{Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<Option<String>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            None => s.serialize_none(),
            Some(x) => x.serialize(s),
        }
    }
}

/// Server → client frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[allow(missing_docs)]
pub enum ServerMessage {
    #[serde(rename = "auth_required")]
    AuthRequired { hub: String, api_version: String },
    #[serde(rename = "auth_ok")]
    AuthOk {
        hub: String,
        api_version: String,
        zdef_versions: Vec<u8>,
        features: Vec<String>,
    },
    #[serde(rename = "auth_invalid")]
    AuthInvalid { message: String },
    #[serde(rename = "result")]
    Result {
        id: u64,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<J>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<ApiError>,
    },
    #[serde(rename = "event")]
    Event { id: u64, seq: u64, event: J },
    /// A message serialized piece by piece (a subscription snapshot: as one
    /// `Value` tree it took several times its text in heap).
    #[serde(skip)]
    Text(String),
}

impl ServerMessage {
    /// A success result.
    pub fn ok(id: u64, result: J) -> Self {
        Self::Result {
            id,
            success: true,
            result: Some(result),
            error: None,
        }
    }
    /// An error result.
    pub fn err(id: u64, error: ApiError) -> Self {
        Self::Result {
            id,
            success: false,
            result: None,
            error: Some(error),
        }
    }
    /// JSON text.
    pub fn to_text(&self) -> String {
        match self {
            Self::Text(t) => t.clone(),
            m => serde_json::to_string(m).unwrap_or_else(|_| "{}".into()),
        }
    }
    /// [`ServerMessage::to_text`] without copying a pre-serialized text.
    pub fn into_text(self) -> String {
        match self {
            Self::Text(t) => t,
            m => m.to_text(),
        }
    }
}
