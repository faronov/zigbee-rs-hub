//! Zigbee2MQTT's files → the normalized import document: `database.db`
//! (zigbee-herdsman's database, one JSON object per line: devices and
//! groups, `src/controller/database.ts`, `model/device.ts`,
//! `model/endpoint.ts`, `model/group.ts`), `coordinator_backup.json` (the
//! Open Coordinator Backup herdsman writes, Z-Stack and EmberZNet forms),
//! and the friendly names of `configuration.yaml`'s `devices:`/`groups:`
//! (or the `devices.yaml`/`groups.yaml` files they name).
//!
//! What is not imported: bindings and reporting (they live on the
//! devices), Green Power devices (commissioned again), external
//! converters and device options (Zigbee2MQTT's own).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value as J, json};
use yaml_rust2::{Yaml, YamlLoader};

use super::{document, ieee_hex, json_file};

/// Friendly names by IEEE (big-endian hex) and by group ID.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Names {
    pub devices: BTreeMap<String, String>,
    pub groups: BTreeMap<u16, String>,
}

/// herdsman's attribute names of the cached values the hub keeps
/// (`hub_devices::persist::persistent_value`).
const ATTRS: &[(&str, u16, &str, u16)] = &[
    ("genPowerCfg", 0x0001, "batterySize", 0x0031),
    ("genPowerCfg", 0x0001, "batteryQuantity", 0x0033),
    ("genPowerCfg", 0x0001, "batteryRatedVoltage", 0x0034),
    ("hvacThermostat", 0x0201, "minHeatSetpointLimit", 0x0015),
    ("hvacThermostat", 0x0201, "maxHeatSetpointLimit", 0x0016),
    ("hvacThermostat", 0x0201, "ctrlSeqeOfOper", 0x001B),
    ("hvacFanCtrl", 0x0202, "fanModeSequence", 0x0001),
    ("lightingColorCtrl", 0x0300, "colorCapabilities", 0x400A),
    ("lightingColorCtrl", 0x0300, "colorTempPhysicalMin", 0x400B),
    ("lightingColorCtrl", 0x0300, "colorTempPhysicalMax", 0x400C),
    ("seMetering", 0x0702, "unitOfMeasure", 0x0300),
    ("seMetering", 0x0702, "multiplier", 0x0301),
    ("seMetering", 0x0702, "divisor", 0x0302),
    ("seMetering", 0x0702, "summationFormatting", 0x0303),
    ("seMetering", 0x0702, "meteringDeviceType", 0x0306),
    (
        "haElectricalMeasurement",
        0x0B04,
        "acVoltageMultiplier",
        0x0600,
    ),
    (
        "haElectricalMeasurement",
        0x0B04,
        "acVoltageDivisor",
        0x0601,
    ),
    (
        "haElectricalMeasurement",
        0x0B04,
        "acCurrentMultiplier",
        0x0602,
    ),
    (
        "haElectricalMeasurement",
        0x0B04,
        "acCurrentDivisor",
        0x0603,
    ),
    (
        "haElectricalMeasurement",
        0x0B04,
        "acPowerMultiplier",
        0x0604,
    ),
    ("haElectricalMeasurement", 0x0B04, "acPowerDivisor", 0x0605),
];

const POWER: [&str; 7] = [
    "Unknown",
    "Mains (single phase)",
    "Mains (3 phase)",
    "Battery",
    "DC Source",
    "Emergency mains constantly powered",
    "Emergency mains and transfer switch",
];

fn power_source(v: &J) -> Option<u8> {
    match v {
        J::Number(n) => n.as_u64().and_then(|n| u8::try_from(n).ok()),
        J::String(s) => POWER
            .iter()
            .position(|p| p.eq_ignore_ascii_case(s.trim()))
            .map(|p| p as u8),
        _ => None,
    }
}

fn kind(t: &str) -> Option<&'static str> {
    Some(match t {
        "Router" => "router",
        "EndDevice" => "end_device",
        "Coordinator" => "coordinator",
        "GreenPower" => "green_power",
        _ => return None,
    })
}

fn set(o: &mut serde_json::Map<String, J>, k: &str, v: Option<J>) {
    if let Some(v) = v.filter(|v| !v.is_null()) {
        o.insert(k.into(), v);
    }
}

/// One `database.db` device entry.
pub fn device(e: &J, names: &Names) -> Option<J> {
    let ieee = ieee_hex(e.get("ieeeAddr")?.as_str()?)?;
    let mut o = serde_json::Map::new();
    o.insert("ieee".into(), ieee.clone().into());
    set(&mut o, "nwk", e.get("nwkAddr").cloned());
    set(
        &mut o,
        "name",
        names.devices.get(&ieee).map(|n| n.clone().into()),
    );
    set(
        &mut o,
        "type",
        e.get("type")
            .and_then(J::as_str)
            .and_then(kind)
            .map(Into::into),
    );
    set(&mut o, "manufacturer", e.get("manufName").cloned());
    set(&mut o, "model", e.get("modelId").cloned());
    set(&mut o, "manufacturer_code", e.get("manufId").cloned());
    set(
        &mut o,
        "power_source",
        e.get("powerSource").and_then(power_source).map(Into::into),
    );
    for (k, z) in [
        ("app_version", "appVersion"),
        ("stack_version", "stackVersion"),
        ("hw_version", "hwVersion"),
        ("zcl_version", "zclVersion"),
        ("date_code", "dateCode"),
        ("sw_build_id", "swBuildId"),
    ] {
        set(&mut o, k, e.get(z).cloned());
    }
    let mut eps: Vec<(u64, J)> = Vec::new();
    let mut attrs = Vec::new();
    for (k, ep) in e
        .get("endpoints")
        .and_then(J::as_object)
        .into_iter()
        .flatten()
    {
        let Some(id) = ep
            .get("epId")
            .and_then(J::as_u64)
            .or_else(|| k.parse().ok())
        else {
            continue;
        };
        let list = |k: &str| {
            ep.get(k)
                .filter(|l| l.is_array())
                .cloned()
                .unwrap_or_else(|| json!([]))
        };
        eps.push((
            id,
            json!({"id": id, "profile": ep.get("profId"), "device_id": ep.get("devId"), "in": list("inClusterList"), "out": list("outClusterList")}),
        ));
        let clusters = ep.get("clusters");
        for (cname, cid, aname, aid) in ATTRS {
            let c = clusters.and_then(|c| c.get(*cname));
            // `attrs` in databases of herdsman before 0.14.
            let a = c.and_then(|c| c.get("attributes").or_else(|| c.get("attrs")));
            if let Some(v) = a
                .and_then(|a| a.get(*aname))
                .filter(|v| v.is_number() || v.is_string())
            {
                attrs.push(json!({"ep": id, "cluster": cid, "attr": aid, "value": v}));
            }
        }
    }
    eps.sort_by_key(|e| e.0);
    o.insert("endpoints".into(), eps.into_iter().map(|e| e.1).collect());
    if !attrs.is_empty() {
        o.insert("attributes".into(), attrs.into());
    }
    let interviewed = e
        .get("interviewCompleted")
        .and_then(J::as_bool)
        .unwrap_or(false)
        || e.get("interviewState").and_then(J::as_str) == Some("SUCCESSFUL");
    o.insert("interviewed".into(), interviewed.into());
    set(&mut o, "source_id", e.get("id").cloned());
    Some(J::Object(o))
}

/// One `database.db` group entry.
pub fn group(e: &J, names: &Names) -> Option<J> {
    let id = e
        .get("groupID")
        .and_then(J::as_u64)
        .and_then(|i| u16::try_from(i).ok())?;
    let members: Vec<J> = e
        .get("members")
        .and_then(J::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| Some(json!({"ieee": ieee_hex(m.get("deviceIeeeAddr")?.as_str()?)?, "ep": m.get("endpointID")?})))
        .collect();
    let name = names.groups.get(&id).cloned();
    Some(json!({"id": id, "name": name, "members": members}))
}

/// The devices and groups of `database.db`'s text; lines that are not
/// JSON (herdsman skips them too) are counted.
pub fn database(text: &str, names: &Names) -> (Vec<J>, Vec<J>, Option<String>, usize) {
    let (mut devices, mut groups, mut coordinator, mut bad) = (Vec::new(), Vec::new(), None, 0);
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(e) = serde_json::from_str::<J>(line) else {
            bad += 1;
            continue;
        };
        match e.get("type").and_then(J::as_str) {
            Some("Group") => groups.extend(group(&e, names)),
            Some("Coordinator") => {
                coordinator = e.get("ieeeAddr").and_then(J::as_str).and_then(ieee_hex)
            }
            Some(_) => match device(&e, names) {
                Some(d) => devices.push(d),
                None => bad += 1,
            },
            None => bad += 1,
        }
    }
    (devices, groups, coordinator, bad)
}

fn yaml_key(k: &Yaml) -> Option<String> {
    match k {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        _ => None,
    }
}

fn friendly(v: &Yaml) -> Option<String> {
    match &v["friendly_name"] {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        _ => None,
    }
}

/// Names of a `devices:` mapping (or a `devices.yaml` file).
fn device_names(y: &Yaml, out: &mut Names) {
    for (k, v) in y.as_hash().into_iter().flatten() {
        // An unquoted 0x… key is a YAML integer.
        let key = match k {
            Yaml::Integer(i) => Some(format!("{:016x}", *i as u64)),
            k => yaml_key(k).and_then(|s| ieee_hex(&s)),
        };
        if let (Some(ieee), Some(n)) = (key, friendly(v)) {
            out.devices.insert(ieee, n);
        }
    }
}

fn group_names(y: &Yaml, out: &mut Names) {
    for (k, v) in y.as_hash().into_iter().flatten() {
        let id = yaml_key(k)
            .and_then(|s| s.parse::<u16>().ok())
            .or_else(|| match &v["ID"] {
                Yaml::Integer(i) => u16::try_from(*i).ok(),
                _ => None,
            });
        if let (Some(id), Some(n)) = (id, friendly(v)) {
            out.groups.insert(id, n);
        }
    }
}

/// The names of a YAML text: `configuration.yaml` (its `devices:` and
/// `groups:`, inline or the files they name, read from `dir`) or a
/// `devices.yaml`/`groups.yaml` itself (`what`).
pub fn names(text: &str, what: &str, dir: &Path, out: &mut Names) -> Result<(), String> {
    let docs = YamlLoader::load_from_str(text).map_err(|e| format!("{what}: {e}"))?;
    let Some(y) = docs.first() else { return Ok(()) };
    match what {
        "devices.yaml" => device_names(y, out),
        "groups.yaml" => group_names(y, out),
        _ => {
            for (key, read) in [
                ("devices", device_names as fn(&Yaml, &mut Names)),
                ("groups", group_names),
            ] {
                let files: Vec<String> = match &y[key] {
                    Yaml::Hash(_) => {
                        read(&y[key], out);
                        Vec::new()
                    }
                    Yaml::String(f) => vec![f.clone()],
                    Yaml::Array(a) => a
                        .iter()
                        .filter_map(|f| f.as_str().map(str::to_string))
                        .collect(),
                    _ => Vec::new(),
                };
                for f in files {
                    let p = dir.join(&f);
                    let t =
                        std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                    let docs = YamlLoader::load_from_str(&t).map_err(|e| format!("{f}: {e}"))?;
                    if let Some(y) = docs.first() {
                        read(y, out);
                    }
                }
            }
        }
    }
    Ok(())
}

/// The document of Zigbee2MQTT's files: a data directory, or the files
/// (`database.db`, `coordinator_backup.json`, `configuration.yaml`,
/// `devices.yaml`, `groups.yaml`).
pub fn from_files(paths: &[&Path]) -> Result<J, String> {
    let mut files: Vec<PathBuf> = Vec::new();
    for p in paths {
        if p.is_dir() {
            for n in [
                "database.db",
                "coordinator_backup.json",
                "configuration.yaml",
            ] {
                if p.join(n).exists() {
                    files.push(p.join(n));
                }
            }
        } else {
            files.push(p.to_path_buf());
        }
    }
    let name_of = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string()
    };
    let mut found = Names::default();
    let (mut db, mut network) = (None, None);
    for f in &files {
        let n = name_of(f);
        let dir = f.parent().unwrap_or(Path::new("."));
        if n.ends_with(".yaml") || n.ends_with(".yml") {
            let t = std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?;
            let what = if n.starts_with("devices") {
                "devices.yaml"
            } else if n.starts_with("groups") {
                "groups.yaml"
            } else {
                "configuration.yaml"
            };
            names(&t, what, dir, &mut found)?;
        } else if n.ends_with(".json") {
            let j = json_file(f)?;
            if j.pointer("/metadata/format").and_then(J::as_str) != Some(hub_app::ocb::FORMAT) {
                return Err(format!("{}: not an Open Coordinator Backup", f.display()));
            }
            network = Some(j);
        } else {
            db = Some(std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?);
        }
    }
    let db = db.ok_or("no database.db")?;
    let (devices, groups, _, bad) = database(&db, &found);
    if bad > 0 {
        eprintln!("database.db: {bad} lines skipped (not JSON, or not a device)");
    }
    let source = network
        .as_ref()
        .and_then(|n| n.pointer("/metadata/source"))
        .and_then(J::as_str)
        .map_or("Zigbee2MQTT".to_string(), |s| format!("Zigbee2MQTT ({s})"));
    if network.is_none() {
        eprintln!("no coordinator_backup.json: the hub refuses an import without the network");
    }
    Ok(document("z2m", &source, network, devices, groups))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// zigbee-herdsman's `database.db` (generated devices): the
    /// coordinator, a router with a colour light's cache, a sleepy sensor
    /// with an older `attrs` cache and no interview state, a Green Power
    /// switch, a device without endpoints, a group, a broken line.
    pub const DB: &str = r#"{"id":1,"type":"Coordinator","ieeeAddr":"0x00124b00c0ffee01","nwkAddr":0,"manufId":0,"epList":[1,242],"endpoints":{"1":{"profId":260,"epId":1,"devId":5,"inClusterList":[],"outClusterList":[],"clusters":{},"binds":[],"configuredReportings":[],"meta":{}}},"interviewCompleted":true,"meta":{}}
{"id":2,"type":"Router","ieeeAddr":"0x0017880100f1e2d3","nwkAddr":31248,"manufId":4107,"manufName":"Signify Netherlands B.V.","powerSource":"Mains (single phase)","modelId":"LCT015","epList":[11,242],"endpoints":{"11":{"profId":260,"epId":11,"devId":269,"inClusterList":[0,3,4,5,6,8,768,4096],"outClusterList":[25],"clusters":{"genBasic":{"attributes":{"modelId":"LCT015"}},"lightingColorCtrl":{"attributes":{"colorCapabilities":31,"colorTempPhysicalMin":153,"colorTempPhysicalMax":500,"currentX":1000}}},"binds":[{"cluster":6,"type":"endpoint","deviceIeeeAddress":"0x00124b00c0ffee01","endpointID":1}],"configuredReportings":[],"meta":{}},"242":{"profId":41440,"epId":242,"devId":97,"inClusterList":[],"outClusterList":[33],"clusters":{},"binds":[],"configuredReportings":[],"meta":{}}},"appVersion":2,"stackVersion":1,"hwVersion":1,"dateCode":"20191218","swBuildId":"1.50.2_r30933","zclVersion":1,"interviewCompleted":true,"interviewState":"SUCCESSFUL","meta":{},"lastSeen":1764000000000}
{"id":3,"type":"EndDevice","ieeeAddr":"0x00158d0003c4d5e6","nwkAddr":1202,"manufId":4151,"manufName":"LUMI","powerSource":"Battery","modelId":"lumi.weather","epList":[1],"endpoints":{"1":{"profId":260,"epId":1,"devId":24321,"inClusterList":[0,3,65535,1026,1027,1029],"outClusterList":[0,4,65535],"clusters":{"genBasic":{"attrs":{"modelId":"lumi.weather"}},"genPowerCfg":{"attrs":{"batteryVoltage":30}}}}},"appVersion":3,"meta":{}}
{"id":4,"type":"GreenPower","ieeeAddr":"0x00000000017880ff","nwkAddr":32767,"modelId":"GreenPower_2","epList":[],"endpoints":{},"interviewCompleted":true,"meta":{},"gpSecurityKey":[1,2,3]}
{"id":5,"type":"EndDevice","ieeeAddr":"0x00158d00aabbccdd","nwkAddr":4242,"epList":[],"endpoints":{},"interviewState":"FAILED","meta":{}}
not json
{"id":6,"type":"Group","groupID":4097,"members":[{"deviceIeeeAddr":"0x0017880100f1e2d3","endpointID":11},{"deviceIeeeAddr":"0x0000000000000bad","endpointID":1}],"meta":{}}"#;

    #[test]
    fn database_entries_become_document_devices() {
        let mut n = Names::default();
        n.devices
            .insert("0017880100f1e2d3".into(), "Hall light".into());
        n.groups.insert(4097, "Hall".into());
        let (devices, groups, coordinator, bad) = database(DB, &n);
        assert_eq!((devices.len(), groups.len(), bad), (4, 1, 1));
        assert_eq!(coordinator.as_deref(), Some("00124b00c0ffee01"));
        let light = &devices[0];
        assert_eq!(light["ieee"], "0017880100f1e2d3");
        assert_eq!(
            (
                light["name"].as_str(),
                light["type"].as_str(),
                light["power_source"].as_u64()
            ),
            (Some("Hall light"), Some("router"), Some(1))
        );
        assert_eq!(
            light["endpoints"][0],
            json!({"id": 11, "profile": 260, "device_id": 269, "in": [0, 3, 4, 5, 6, 8, 768, 4096], "out": [25]})
        );
        assert_eq!(
            light["endpoints"][1]["id"], 242,
            "the GP proxy endpoint, sorted after 11"
        );
        assert_eq!(
            light["attributes"].as_array().unwrap().len(),
            3,
            "the kept colour attributes, not currentX"
        );
        assert_eq!(
            (
                light["sw_build_id"].as_str(),
                light["interviewed"].as_bool()
            ),
            (Some("1.50.2_r30933"), Some(true))
        );
        let sensor = &devices[1];
        assert_eq!(
            (
                sensor["type"].as_str(),
                sensor["power_source"].as_u64(),
                sensor["interviewed"].as_bool()
            ),
            (Some("end_device"), Some(3), Some(false))
        );
        assert!(sensor.get("name").is_none() && sensor.get("attributes").is_none());
        assert_eq!(devices[2]["type"], "green_power");
        assert_eq!(devices[3]["endpoints"], json!([]));
        assert_eq!(
            groups[0],
            json!({"id": 4097, "name": "Hall", "members": [{"ieee": "0017880100f1e2d3", "ep": 11}, {"ieee": "0000000000000bad", "ep": 1}]})
        );
        // The hub reads every entry back.
        for d in &devices {
            assert!(
                !matches!(
                    hub_app::migrate::entry(d),
                    hub_app::migrate::Entry::Skip {
                        reason: "invalid",
                        ..
                    }
                ),
                "{d}"
            );
        }
    }

    #[test]
    fn friendly_names_come_from_configuration_or_their_own_files() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../target/hubctl-test/z2m-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("devices.yaml"),
            "'0x00158d0003c4d5e6':\n  friendly_name: Kitchen/temperature\n  retain: false\n",
        )
        .unwrap();
        let conf = "homeassistant: true\nmqtt:\n  base_topic: zigbee2mqtt\ndevices: devices.yaml\ngroups:\n  '4097':\n    friendly_name: Hall\n  7:\n    friendly_name: 7\n";
        let mut n = Names::default();
        names(conf, "configuration.yaml", &dir, &mut n).unwrap();
        assert_eq!(
            n.devices.get("00158d0003c4d5e6").map(String::as_str),
            Some("Kitchen/temperature")
        );
        assert_eq!(
            (
                n.groups.get(&4097).map(String::as_str),
                n.groups.get(&7).map(String::as_str)
            ),
            (Some("Hall"), Some("7"))
        );
        // Inline, with an unquoted (integer) key.
        let mut m = Names::default();
        names(
            "devices:\n  0x0017880100f1e2d3:\n    friendly_name: Hall light\n",
            "configuration.yaml",
            &dir,
            &mut m,
        )
        .unwrap();
        assert_eq!(
            m.devices.get("0017880100f1e2d3").map(String::as_str),
            Some("Hall light")
        );
        // The data directory as Zigbee2MQTT keeps it.
        std::fs::write(dir.join("configuration.yaml"), conf).unwrap();
        std::fs::write(dir.join("database.db"), DB).unwrap();
        let backup = include_str!("../../tests/fixtures/z2m_coordinator_backup.json");
        std::fs::write(dir.join("coordinator_backup.json"), backup).unwrap();
        let doc = from_files(&[dir.as_path()]).unwrap();
        assert_eq!(doc["format"], "zigbee-rs-hub/import");
        assert_eq!(doc["source"]["kind"], "z2m");
        assert_eq!(doc["network"]["extended_pan_id"], "a1b2c3d4e5f60718");
        assert_eq!(doc["devices"][1]["name"], "Kitchen/temperature");
        assert_eq!(doc["groups"][0]["name"], "Hall");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
