//! WebSocket JSON API messages (spec 14), modelled on Home Assistant's
//! WebSocket API: `auth` / `auth_ok`, client-chosen `id`s, `result`
//! frames, `subscribe` with a snapshot and `seq`-numbered deltas.

use serde::{Deserialize, Serialize};
use serde_json::Value as J;

use crate::gp::bindings::{BindAction, BindTrigger, CreateBinding, ExecutorChoice};
use crate::hub::ApiError;

/// API version (`major.minor`; minors only add fields and types).
pub const API_VERSION: &str = "1.1";

/// Topics of `subscribe`.
pub const TOPICS: &[&str] = &["devices", "entities", "events", "gp", "network"];

/// The first frame from the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "auth")]
pub struct Auth {
    /// Access token.
    pub token: String,
}

/// A request with its client-chosen ID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Unique per connection.
    pub id: u64,
    /// The request.
    #[serde(flatten)]
    pub request: Request,
}

fn yes() -> bool {
    true
}

/// Requests (14.2–14.5, 13.7).
///
/// The envelope's `id` is the request ID (HA style), so parameters that
/// the spec tables also call `id` are named after what they address:
/// `device` (devices/*, gp/devices/remove), `definition`
/// (definitions/delete, definitions/get) and `binding` (gp/bindings/*).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        #[serde(default, with = "double_option")]
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
        #[serde(default = "yes")]
        involve_proxies: bool,
        #[serde(default)]
        exit_on_first_pairing: bool,
    },
    #[serde(rename = "gp/commissioning/close")]
    GpCommissioningClose,
    #[serde(rename = "gp/devices/list")]
    GpDevicesList,
    #[serde(rename = "gp/devices/remove")]
    GpDevicesRemove {
        #[serde(rename = "device")]
        id: String,
        #[serde(default = "yes")]
        send_remove: bool,
    },
    #[serde(rename = "gp/config")]
    GpConfig {
        #[serde(default)]
        security_level: u8,
        #[serde(default = "two")]
        communication_mode: u8,
    },
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
}

fn two() -> u8 {
    2
}

mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<Option<String>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            None => s.serialize_none(),
            Some(x) => x.serialize(s),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<String>>, D::Error> {
        Ok(Some(Option::<String>::deserialize(d)?))
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
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}
