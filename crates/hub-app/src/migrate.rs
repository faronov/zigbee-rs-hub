//! Migration from another coordinator (Zigbee2MQTT, ZHA) without pairing
//! the devices again (`docs/migration.md`): the normalized import document
//! (`zigbee-rs-hub/import` v1) that the clients (the web UI, `hubctl
//! import`, the Home Assistant integration) make from the source's files,
//! and what the hub stages of it.
//!
//! ```text
//! {"format": "zigbee-rs-hub/import", "version": 1,
//!  "source": {"kind": "z2m", "name": "Zigbee2MQTT 2.1.3", "created": "…"},
//!  "network": {…Open Coordinator Backup…},
//!  "devices": [{"ieee": "00158d0001a2b3c4", "nwk": 4660, "name": "Kitchen", …}],
//!  "groups": [{"id": 4097, "name": "Living room", "members": [{"ieee": "…", "ep": 1}]}]}
//! ```
//!
//! A document goes in requests of a few kilobytes (`document`, then
//! `devices`, `network_devices` and `groups` batches while `more` is true):
//! no request needs the whole file in RAM, and only the apply keeps the
//! devices (a few hundred bytes each).

use hub_devices::fingerprint::{EndpointFp, Fingerprint};
use hub_devices::hub::import::{ImportAction, ImportPreview, ImportedDevice};
use hub_devices::model::LayerSource;
use hub_devices::zcl::ZclValue;
use hub_devices::zdo::{LogicalType, NodeDescriptor};
use serde_json::{Value as J, json};

/// `format` of the document.
pub const FORMAT: &str = "zigbee-rs-hub/import";
/// `version` of the document.
pub const VERSION: u64 = 1;
/// Devices one import takes.
pub const MAX_DEVICES: usize = 500;
/// Groups one import takes.
pub const MAX_GROUPS: usize = 256;
/// Bytes of the devices an apply stages at most (an estimate of their heap).
pub const MAX_STAGED_BYTES: usize = 384 * 1024;
/// Longest name kept (characters).
pub const MAX_NAME: usize = 64;
/// NWK frame counter margin of an import: the files may be hours or days
/// older than the moment the source stopped (zigpy's 10 000 assumes a
/// backup taken right before), and a counter jump costs nothing.
pub const NWK_MARGIN: u32 = 1 << 20;
/// APS margin when the backup has a trust center link key counter.
pub const APS_MARGIN_KNOWN: u32 = 1 << 16;

/// An import's devices and groups, handed to the app thread
/// ([`JobOut::Import`](crate::admin::JobOut::Import)).
pub struct Batch {
    /// Devices.
    pub devices: Vec<ImportedDevice>,
    /// Groups in their stored form ([`group`]).
    pub groups: Vec<J>,
    /// [`IMPORT_VERIFY`](hub_devices::hub::import::IMPORT_VERIFY) or
    /// [`IMPORT_RECONFIGURE`](hub_devices::hub::import::IMPORT_RECONFIGURE).
    pub mode: u8,
    /// Forget the hub's devices the import does not list.
    pub remove_missing: bool,
    /// The app's answer.
    pub done: std::sync::mpsc::Sender<Applied>,
}

/// What the app did with a [`Batch`].
#[derive(Debug, Clone, Default)]
pub struct Applied {
    /// By action.
    pub counts: Counts,
    /// Refused by the engine.
    pub failed: usize,
    /// Groups added or changed.
    pub groups: usize,
    /// Devices forgotten (`remove_missing`).
    pub removed: usize,
    /// The registry was written (else it is once the records are this
    /// hub's).
    pub stored: bool,
    /// Imported devices not verified yet.
    pub pending: usize,
}

/// What the source's records say a device is.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// A Zigbee device to import.
    Device(Box<ImportedDevice>),
    /// Not imported, and why (`green_power`, `coordinator`, `no_nwk`,
    /// `invalid`).
    Skip {
        /// IEEE, if it could be read.
        ieee: Option<u64>,
        /// Its name at the source.
        name: Option<String>,
        /// Why.
        reason: &'static str,
        /// Details for `invalid`.
        detail: String,
    },
}

/// The document header (the first request's `document` without its
/// arrays).
#[derive(Debug, Clone, Default)]
pub struct Header {
    /// `source.kind`.
    pub kind: String,
    /// `source.name`.
    pub name: String,
}

/// Reads the header of a document (`format`, `version`, `source`).
pub fn header(doc: &J) -> Result<Header, String> {
    let format = doc.get("format").and_then(J::as_str).unwrap_or("");
    let version = doc.get("version").and_then(J::as_u64).unwrap_or(0);
    if format != FORMAT || version != VERSION {
        return Err(format!(
            "not a {FORMAT} v{VERSION} document ({format:?} v{version})"
        ));
    }
    let src = doc.get("source");
    let s = |k: &str| {
        src.and_then(|s| s.get(k))
            .and_then(J::as_str)
            .unwrap_or("")
            .chars()
            .take(80)
            .collect::<String>()
    };
    Ok(Header {
        kind: s("kind"),
        name: s("name"),
    })
}

/// An IEEE address: 16 hex digits, big-endian, with or without `0x` and
/// `:`/`-` separators.
pub fn ieee(v: &J) -> Option<u64> {
    let s = v.as_str()?.trim();
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    let digits: String = s.chars().filter(|c| *c != ':' && *c != '-').collect();
    if digits.len() != 16 {
        return None;
    }
    u64::from_str_radix(&digits, 16).ok()
}

/// A short address: a number, or hex (`"4d32"`, `"0x4d32"`).
fn nwk(v: &J) -> Option<u16> {
    match v {
        J::Number(n) => n.as_u64().and_then(|n| u16::try_from(n).ok()),
        J::String(s) => {
            let s = s.trim();
            let s = s
                .strip_prefix("0x")
                .or_else(|| s.strip_prefix("0X"))
                .unwrap_or(s);
            u16::from_str_radix(s, 16).ok()
        }
        _ => None,
    }
    .filter(|n| *n < 0xFFF8)
}

fn uint<T: TryFrom<u64>>(v: Option<&J>) -> Option<T> {
    v.and_then(J::as_u64).and_then(|n| T::try_from(n).ok())
}

fn text(v: Option<&J>) -> Option<String> {
    let s = v.and_then(J::as_str)?.trim_matches(char::from(0)).trim();
    (!s.is_empty()).then(|| s.chars().take(MAX_NAME).collect())
}

/// Basic PowerSource as a number, or the names zigbee-herdsman writes.
fn power_source(v: Option<&J>) -> Option<u8> {
    match v? {
        J::Number(n) => n.as_u64().and_then(|n| u8::try_from(n).ok()),
        J::String(s) => [
            "Unknown",
            "Mains (single phase)",
            "Mains (3 phase)",
            "Battery",
            "DC Source",
            "Emergency mains constantly powered",
            "Emergency mains and transfer switch",
        ]
        .iter()
        .position(|n| n.eq_ignore_ascii_case(s.trim()))
        .map(|p| p as u8),
        _ => None,
    }
}

fn clusters(v: Option<&J>) -> Vec<u16> {
    let mut c: Vec<u16> = v
        .and_then(J::as_array)
        .map(|a| a.iter().filter_map(|x| uint::<u16>(Some(x))).collect())
        .unwrap_or_default();
    c.dedup();
    c
}

/// A cached attribute value as the hub keeps it.
fn value(v: &J) -> Option<ZclValue> {
    Some(match v {
        J::Bool(b) => ZclValue::Bool(Some(*b)),
        J::Number(n) => match (n.as_u64(), n.as_i64(), n.as_f64()) {
            (Some(u), _, _) => ZclValue::Uint(u),
            (None, Some(i), _) => ZclValue::Int(i),
            (_, _, Some(f)) => ZclValue::Float(f),
            _ => return None,
        },
        J::String(s) => ZclValue::String(s.chars().take(64).collect()),
        _ => return None,
    })
}

/// One `devices[]` entry.
pub fn entry(d: &J) -> Entry {
    let name = text(d.get("name"));
    let Some(ieee) = d.get("ieee").and_then(ieee) else {
        return Entry::Skip {
            ieee: None,
            name,
            reason: "invalid",
            detail: "ieee: 16 hex digits".into(),
        };
    };
    let skip = |reason, detail: &str| Entry::Skip {
        ieee: Some(ieee),
        name: name.clone(),
        reason,
        detail: detail.into(),
    };
    let ty = d.get("type").and_then(J::as_str).unwrap_or("");
    match ty {
        "green_power" => return skip("green_power", "Green Power devices are commissioned again"),
        "coordinator" => return skip("coordinator", ""),
        _ => {}
    }
    let Some(nwk) = d.get("nwk").and_then(nwk) else {
        return skip("no_nwk", "no short address");
    };
    let nd = d
        .get("node_descriptor")
        .filter(|n| n.is_object())
        .and_then(|n| {
            let mac: u8 = uint(n.get("mac_capabilities"))?;
            Some(NodeDescriptor {
                logical_type: match uint::<u8>(n.get("logical_type")) {
                    Some(0) => LogicalType::Coordinator,
                    Some(1) => LogicalType::Router,
                    Some(2) => LogicalType::EndDevice,
                    _ => LogicalType::Unknown,
                },
                mac_capabilities: mac,
                manufacturer_code: uint(n.get("manufacturer_code")).unwrap_or(0),
                max_buffer_size: uint(n.get("max_buffer_size")).unwrap_or(0),
                max_incoming_transfer: uint(n.get("max_incoming_transfer")).unwrap_or(0),
                server_mask: uint(n.get("server_mask")).unwrap_or(0),
                max_outgoing_transfer: uint(n.get("max_outgoing_transfer")).unwrap_or(0),
            })
        });
    let logical_type = match ty {
        "router" => Some(LogicalType::Router),
        "end_device" => Some(LogicalType::EndDevice),
        _ => nd
            .map(|n| n.logical_type)
            .filter(|t| *t != LogicalType::Unknown),
    };
    if logical_type == Some(LogicalType::Coordinator) {
        return skip("coordinator", "");
    }
    let power = power_source(d.get("power_source"));
    let mains_power = power.is_some_and(|p| matches!(p & 0x7F, 1 | 2 | 4 | 5 | 6));
    // Without a node descriptor (Zigbee2MQTT keeps none): routers and
    // mains-powered end devices listen, other end devices sleep (requests
    // wait for them: the safe side).
    let (rx_on, mac_mains) = match nd {
        Some(n) => (Some(n.rx_on_when_idle()), Some(n.mains_powered())),
        None => match logical_type {
            Some(LogicalType::Router) => (Some(true), Some(true)),
            Some(LogicalType::EndDevice) => (Some(mains_power), Some(mains_power)),
            _ => (None, None),
        },
    };
    let mut endpoints: Vec<EndpointFp> = Vec::new();
    for e in d
        .get("endpoints")
        .and_then(J::as_array)
        .into_iter()
        .flatten()
    {
        let Some(ep) = uint::<u8>(e.get("id")).filter(|ep| (1..=240).contains(ep) || *ep == 242)
        else {
            continue;
        };
        if endpoints.iter().any(|x| x.ep == ep) {
            continue;
        }
        endpoints.push(EndpointFp {
            ep,
            profile: uint(e.get("profile")).unwrap_or(0x0104),
            device_id: uint(e.get("device_id")).unwrap_or(0),
            in_clusters: clusters(e.get("in")),
            out_clusters: clusters(e.get("out")),
        });
    }
    hub_vecmap::sort::sort_by_key(&mut endpoints, |e| e.ep);
    let attrs = d
        .get("attributes")
        .and_then(J::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some((
                uint(a.get("ep"))?,
                uint(a.get("cluster"))?,
                uint(a.get("attr"))?,
                uint(a.get("mfr")),
                value(a.get("value")?)?,
            ))
        })
        .take(64)
        .collect();
    let num = |k: &str| uint::<u32>(d.get(k));
    let fp = Fingerprint {
        manufacturer_name: text(d.get("manufacturer")),
        model_id: text(d.get("model")),
        mfr_code: uint(d.get("manufacturer_code")).or(nd.map(|n| n.manufacturer_code)),
        logical_type,
        power_source: power,
        rx_on_when_idle: rx_on,
        mac_mains,
        app_version: num("app_version"),
        stack_version: num("stack_version"),
        hw_version: num("hw_version"),
        zcl_version: num("zcl_version"),
        date_code: text(d.get("date_code")),
        sw_build_id: text(d.get("sw_build_id")),
        endpoints,
        self_description: None,
        gp: None,
    };
    Entry::Device(Box::new(ImportedDevice {
        ieee,
        nwk,
        name,
        fp,
        node_desc: nd,
        attrs,
        partial: d.get("interviewed").and_then(J::as_bool) == Some(false),
    }))
}

/// Heap an imported device takes, roughly.
pub fn cost(d: &ImportedDevice) -> usize {
    let s = |v: &Option<String>| v.as_ref().map_or(0, String::len);
    let f = &d.fp;
    320 + s(&d.name)
        + s(&f.manufacturer_name)
        + s(&f.model_id)
        + s(&f.date_code)
        + s(&f.sw_build_id)
        + f.endpoints
            .iter()
            .map(|e| 48 + 2 * (e.in_clusters.len() + e.out_clusters.len()))
            .sum::<usize>()
        + 48 * d.attrs.len()
}

/// Heap a staged group takes, roughly.
pub fn group_cost(g: &J) -> usize {
    96 + g["name"].as_str().map_or(0, str::len) + 64 * g["members"].as_array().map_or(0, Vec::len)
}

/// One `groups[]` entry in the hub's stored form (`zgroups`): members by
/// device ID, already members (state `ok`), the hub endpoint joining.
pub fn group(g: &J, coordinator: Option<u64>) -> Result<J, String> {
    let id = uint::<u16>(g.get("id"))
        .filter(|i| (1..=hub_devices::hub::zgroups::GROUP_MAX).contains(i))
        .ok_or("group id: 1–0xFFF7")?;
    let mut members: Vec<J> = Vec::new();
    for m in g.get("members").and_then(J::as_array).into_iter().flatten() {
        let (Some(ieee), Some(ep)) = (m.get("ieee").and_then(ieee), uint::<u8>(m.get("ep"))) else {
            continue;
        };
        // The coordinator's own membership is the hub endpoint's.
        if Some(ieee) == coordinator {
            continue;
        }
        let device = hub_devices::hub::zigbee_id(ieee);
        if !members
            .iter()
            .any(|x| x["device"] == device && x["endpoint"] == ep)
        {
            members.push(json!({"device": device, "endpoint": ep, "state": "ok"}));
        }
    }
    let name = text(g.get("name")).unwrap_or_else(|| format!("Group {id}"));
    Ok(json!({"id": id, "name": name, "hub_member": true, "members": members}))
}

fn source_name(s: LayerSource) -> &'static str {
    match s {
        LayerSource::Generic => "generic",
        LayerSource::Catalog => "catalog",
        LayerSource::User => "user",
        LayerSource::Fetched => "fetched",
        LayerSource::SelfDescription => "self_description",
    }
}

/// The preview row of a device (`network/import/preview`, `network/import`).
pub fn row(d: &ImportedDevice, p: &ImportPreview) -> J {
    let top = p
        .layers
        .iter()
        .rev()
        .find(|l| l.source != LayerSource::Generic);
    json!({
        "ieee": format!("{:016x}", d.ieee),
        "id": hub_devices::hub::zigbee_id(d.ieee),
        "name": d.name,
        "manufacturer": p.vendor,
        "model": p.model,
        "status": p.action.as_str(),
        "definition": top.map(|l| json!({"source": source_name(l.source), "id": l.id})),
        "entities": p.entities,
        "endpoints": d.fp.endpoints.len(),
        "interview": if d.incomplete() { "on_first_contact" } else { "imported" },
    })
}

/// The row of an entry that is not imported.
pub fn skip_row(ieee: Option<u64>, name: &Option<String>, reason: &str, detail: &str) -> J {
    json!({
        "ieee": ieee.map(|i| format!("{i:016x}")),
        "name": name,
        "status": "skip",
        "reason": reason,
        "detail": (!detail.is_empty()).then_some(detail),
    })
}

/// Counts of an import by status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Added.
    pub new: usize,
    /// Replaced (an earlier import, an unfinished interview).
    pub replace: usize,
    /// Kept as the hub has it.
    pub keep: usize,
    /// Not imported.
    pub skip: usize,
}

impl Counts {
    /// Counts `a`.
    pub fn add(&mut self, a: ImportAction) {
        match a {
            ImportAction::New => self.new += 1,
            ImportAction::Replace => self.replace += 1,
            ImportAction::Keep => self.keep += 1,
        }
    }

    /// As JSON.
    pub fn json(&self) -> J {
        json!({"new": self.new, "replace": self.replace, "keep": self.keep, "skip": self.skip})
    }
}

/// When the devices are configured again (`reconfigure`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reconfigure {
    /// When the coordinator IEEE changed.
    #[default]
    Auto,
    /// Always.
    Always,
    /// Never (binds and reporting are left as they are).
    Never,
}

impl Reconfigure {
    /// From the request.
    pub fn parse(v: Option<&J>) -> Result<Self, String> {
        match v.and_then(J::as_str) {
            None if v.is_none_or(J::is_null) => Ok(Self::Auto),
            Some("auto") => Ok(Self::Auto),
            Some("always") => Ok(Self::Always),
            Some("never") => Ok(Self::Never),
            _ => Err("reconfigure: auto, always or never".into()),
        }
    }

    /// The import mode of the devices when the coordinator IEEE `changed`.
    pub fn mode(self, changed: bool) -> u8 {
        use hub_devices::hub::import::{IMPORT_RECONFIGURE, IMPORT_VERIFY};
        match self {
            Self::Always => IMPORT_RECONFIGURE,
            Self::Never => IMPORT_VERIFY,
            Self::Auto if changed => IMPORT_RECONFIGURE,
            Self::Auto => IMPORT_VERIFY,
        }
    }
}

/// What an import's network part holds, for the preview (no keys).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkFacts {
    /// Network key frame counter.
    pub nwk_counter: u64,
    /// Trust center link key frame counter, if the backup has one.
    pub tclk_counter: Option<u64>,
    /// `stack_specific` keys (`zstack`, `ezsp`, `zigbee_rs`).
    pub stacks: Vec<String>,
    /// The EZSP hashed link key root is in the backup.
    pub hashed_tclk: bool,
    /// When the backup was made (`creation_time`, Z2M's `internal.date`).
    pub created: Option<String>,
    /// Who made it (`metadata.source`).
    pub source: String,
}

/// Facts of an Open Coordinator Backup that the restore items do not say.
pub fn network_facts(o: &J) -> NetworkFacts {
    let internal = o.pointer("/metadata/internal");
    let ss = o.get("stack_specific").and_then(J::as_object);
    NetworkFacts {
        nwk_counter: o
            .pointer("/network_key/frame_counter")
            .and_then(J::as_u64)
            .unwrap_or(0),
        tclk_counter: internal
            .and_then(|i| i.pointer("/network/tc_link_key/frame_counter"))
            .and_then(J::as_u64),
        stacks: ss.map(|m| m.keys().cloned().collect()).unwrap_or_default(),
        hashed_tclk: o
            .pointer("/stack_specific/ezsp/hashed_tclk")
            .is_some_and(J::is_string),
        created: internal
            .and_then(|i| i.get("creation_time").or_else(|| i.get("date")))
            .and_then(J::as_str)
            .map(|s| s.chars().take(40).collect()),
        source: o
            .pointer("/metadata/source")
            .and_then(J::as_str)
            .unwrap_or("")
            .chars()
            .take(80)
            .collect(),
    }
}

/// Link keys in a batch of OCB `devices[]`.
pub fn link_keys(devices: &J) -> usize {
    devices.as_array().map_or(0, |a| {
        a.iter()
            .filter(|d| d.get("link_key").is_some_and(J::is_object))
            .count()
    })
}

/// NWK addresses of a batch of OCB `devices[]` (a device entry without
/// one takes the backup's).
pub fn ocb_addresses(devices: &J, out: &mut Vec<(u64, u16)>) {
    for d in devices.as_array().into_iter().flatten() {
        if let (Some(i), Some(n)) = (
            d.get("ieee_address").and_then(ieee),
            d.get("nwk_address").and_then(nwk),
        ) {
            out.push((i, n));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_entry_becomes_the_fingerprint_an_interview_would_give() {
        let d = json!({
            "ieee": "0x00158d0001a2b3c4", "nwk": "4d32", "name": "  Kitchen sensor ", "type": "end_device",
            "manufacturer": "LUMI", "model": "lumi.weather", "manufacturer_code": 4447, "power_source": "Battery",
            "app_version": 3, "date_code": "20191205", "sw_build_id": "3000-0001",
            "endpoints": [{"id": 1, "profile": 260, "device_id": 24321, "in": [0, 3, 1026, 1029], "out": [0, 25]},
                          {"id": 1, "profile": 260, "device_id": 1, "in": [], "out": []},
                          {"id": 300}],
            "attributes": [{"ep": 1, "cluster": 768, "attr": 16394, "value": 31}, {"ep": 1, "cluster": 0, "attr": 5, "value": "x"},
                           {"ep": 1, "cluster": 6, "attr": 0}]
        });
        let Entry::Device(e) = entry(&d) else {
            panic!()
        };
        assert_eq!((e.ieee, e.nwk), (0x00158d0001a2b3c4, 0x4D32));
        assert_eq!(e.name.as_deref(), Some("Kitchen sensor"));
        let f = &e.fp;
        assert_eq!(
            (f.model_id.as_deref(), f.mfr_code, f.power_source),
            (Some("lumi.weather"), Some(4447), Some(3))
        );
        assert_eq!(
            (f.logical_type, f.rx_on_when_idle, f.mac_mains),
            (Some(LogicalType::EndDevice), Some(false), Some(false))
        );
        assert_eq!(
            f.endpoints.len(),
            1,
            "duplicate and out-of-range endpoints dropped"
        );
        assert_eq!(f.endpoints[0].in_clusters, vec![0, 3, 1026, 1029]);
        assert_eq!(e.attrs.len(), 2, "a value-less attribute is dropped");
        assert_eq!(e.attrs[0], (1, 768, 16394, None, ZclValue::Uint(31)));
    }

    #[test]
    fn missing_and_odd_fields_are_tolerated_or_skipped() {
        let Entry::Device(e) = entry(&json!({"ieee": "00:15:8d:00:01:a2:b3:c4", "nwk": 4660}))
        else {
            panic!()
        };
        assert_eq!(
            (e.ieee, e.nwk, e.fp.endpoints.len()),
            (0x00158d0001a2b3c4, 0x1234, 0)
        );
        assert_eq!(e.fp.rx_on_when_idle, None);
        // A router without a node descriptor listens; a mains end device too.
        let Entry::Device(r) =
            entry(&json!({"ieee": "0017880100aabbcc", "nwk": 1, "type": "router"}))
        else {
            panic!()
        };
        assert_eq!(r.fp.rx_on_when_idle, Some(true));
        let Entry::Device(m) = entry(
            &json!({"ieee": "0017880100aabbcd", "nwk": 2, "type": "end_device", "power_source": 1}),
        ) else {
            panic!()
        };
        assert_eq!(m.fp.rx_on_when_idle, Some(true));
        // A node descriptor decides.
        let nd = json!({"logical_type": 2, "mac_capabilities": 0x8C, "manufacturer_code": 4476});
        let Entry::Device(n) =
            entry(&json!({"ieee": "0017880100aabbce", "nwk": 3, "node_descriptor": nd}))
        else {
            panic!()
        };
        assert_eq!(
            (n.fp.rx_on_when_idle, n.fp.mfr_code, n.fp.logical_type),
            (Some(true), Some(4476), Some(LogicalType::EndDevice))
        );
        for (d, reason) in [
            (json!({"ieee": "nope", "nwk": 1}), "invalid"),
            (json!({"nwk": 1}), "invalid"),
            (json!({"ieee": "00158d0001a2b3c4"}), "no_nwk"),
            (json!({"ieee": "00158d0001a2b3c4", "nwk": 65535}), "no_nwk"),
            (
                json!({"ieee": "00158d0001a2b3c4", "nwk": 1, "type": "green_power"}),
                "green_power",
            ),
            (
                json!({"ieee": "00124b0001020304", "nwk": 0, "type": "coordinator"}),
                "coordinator",
            ),
        ] {
            match entry(&d) {
                Entry::Skip { reason: r, .. } => assert_eq!(r, reason, "{d}"),
                e => panic!("{d}: {e:?}"),
            }
        }
    }

    #[test]
    fn groups_take_the_stored_form_without_the_coordinator() {
        let g = json!({"id": 4097, "name": "Living room", "members": [
            {"ieee": "0x0017880100aabbcc", "ep": 11}, {"ieee": "0017880100aabbcc", "ep": 11},
            {"ieee": "00124b0001020304", "ep": 1}, {"ieee": "bad", "ep": 1}]});
        let s = group(&g, Some(0x00124b0001020304)).unwrap();
        assert_eq!(
            s["members"],
            json!([{"device": "zb:0017880100aabbcc", "endpoint": 11, "state": "ok"}])
        );
        assert_eq!(
            (s["id"].as_u64(), s["hub_member"].as_bool()),
            (Some(4097), Some(true))
        );
        assert_eq!(group(&json!({"id": 7}), None).unwrap()["name"], "Group 7");
        assert!(
            group(&json!({"id": 0}), None).is_err() && group(&json!({"id": 65530}), None).is_err()
        );
    }

    #[test]
    fn the_header_names_the_format() {
        let h = header(&json!({"format": FORMAT, "version": 1, "source": {"kind": "z2m", "name": "Zigbee2MQTT 2.1"}})).unwrap();
        assert_eq!(
            (h.kind.as_str(), h.name.as_str()),
            ("z2m", "Zigbee2MQTT 2.1")
        );
        assert!(header(&json!({"format": FORMAT, "version": 2})).is_err());
        assert!(header(&json!({"metadata": {}})).is_err());
    }

    #[test]
    fn reconfigure_follows_the_coordinator_ieee() {
        use hub_devices::hub::import::{IMPORT_RECONFIGURE, IMPORT_VERIFY};
        assert_eq!(Reconfigure::parse(None), Ok(Reconfigure::Auto));
        assert_eq!(Reconfigure::Auto.mode(false), IMPORT_VERIFY);
        assert_eq!(Reconfigure::Auto.mode(true), IMPORT_RECONFIGURE);
        assert_eq!(Reconfigure::Never.mode(true), IMPORT_VERIFY);
        assert_eq!(Reconfigure::Always.mode(false), IMPORT_RECONFIGURE);
        assert!(Reconfigure::parse(Some(&json!("sometimes"))).is_err());
    }
}
