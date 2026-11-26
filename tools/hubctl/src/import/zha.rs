//! ZHA's `zigbee.db` (zigpy's application database, `zigpy/appdb.py`) →
//! the normalized import document. Schemas v12 to v17 (zigpy 0.5x to
//! 2.5): the newest version's tables are read (`devices_vN`, or
//! `zigbee_devices_v17`; `endpoints_vN`; `clusters_vN` or v12's
//! `in_clusters`/`out_clusters`; `node_descriptors_vN`;
//! `attributes_cache_vN`; `groups_vN`, `group_members_vN`;
//! `network_backups_vN`; `gp_devices_vN`, listed and skipped).
//!
//! Names and areas are Home Assistant's, not zigpy's: `--ha-storage`
//! reads `core.device_registry` (devices with the identifier `("zha",
//! ieee)`) and `core.area_registry`.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value as J, json};

use super::{document, ieee_hex, json_file, zigpy};

/// Names and areas of ZHA's devices in Home Assistant, by IEEE.
#[derive(Debug, Default, Clone)]
pub struct HaNames {
    pub devices: BTreeMap<String, (Option<String>, Option<String>)>,
}

impl HaNames {
    /// From Home Assistant's `.storage` directory.
    pub fn read(dir: &Path) -> Result<Self, String> {
        let reg = json_file(&dir.join("core.device_registry"))?;
        let areas: BTreeMap<String, String> = json_file(&dir.join("core.area_registry"))
            .ok()
            .and_then(|a| a.pointer("/data/areas").and_then(J::as_array).cloned())
            .into_iter()
            .flatten()
            .filter_map(|a| {
                Some((
                    a.get("id")?.as_str()?.to_string(),
                    a.get("name")?.as_str()?.to_string(),
                ))
            })
            .collect();
        Ok(Self::from_registry(&reg, &areas))
    }

    /// From the device registry's JSON and the area names by ID.
    pub fn from_registry(reg: &J, areas: &BTreeMap<String, String>) -> Self {
        let mut devices = BTreeMap::new();
        for d in reg
            .pointer("/data/devices")
            .and_then(J::as_array)
            .into_iter()
            .flatten()
        {
            let zha = d
                .get("identifiers")
                .and_then(J::as_array)
                .into_iter()
                .flatten()
                .find_map(|i| {
                    let i = i.as_array()?;
                    (i.first()?.as_str()? == "zha")
                        .then(|| i.get(1)?.as_str().and_then(ieee_hex))?
                });
            let Some(ieee) = zha else { continue };
            let name = d
                .get("name_by_user")
                .and_then(J::as_str)
                .or_else(|| d.get("name").and_then(J::as_str))
                .map(str::to_string);
            let area = d
                .get("area_id")
                .and_then(J::as_str)
                .map(|a| areas.get(a).cloned().unwrap_or_else(|| a.to_string()));
            devices.insert(ieee, (name, area));
        }
        Self { devices }
    }
}

/// The newest schema version of the tables in `db`.
fn version(db: &Connection) -> Result<u32, String> {
    let mut st = db
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .map_err(|e| e.to_string())?;
    let names: Vec<String> = st
        .query_map([], |r| r.get(0))
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    names
        .iter()
        .filter_map(|n| n.strip_prefix("endpoints_v").and_then(|v| v.parse().ok()))
        .max()
        .filter(|v| *v >= 12)
        .ok_or_else(|| "not a zigpy database (no endpoints_v12 or newer)".to_string())
}

fn has_table(db: &Connection, t: &str) -> bool {
    db.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [t],
        |_| Ok(()),
    )
    .is_ok()
}

fn columns(db: &Connection, t: &str) -> Vec<String> {
    db.prepare(&format!("PRAGMA table_info({t})"))
        .and_then(|mut s| {
            s.query_map([], |r| r.get::<_, String>(1))
                .map(|r| r.filter_map(Result::ok).collect())
        })
        .unwrap_or_default()
}

/// A ZCL value of a typed v17 row (`data_type`, the serialized bytes).
fn typed(ty: i64, b: &[u8]) -> Option<J> {
    let le = |n: usize| {
        (b.len() >= n).then(|| {
            b[..n]
                .iter()
                .rev()
                .fold(0u64, |a, x| a << 8 | u64::from(*x))
        })
    };
    Some(match ty {
        0x10 => J::Bool(*b.first()? != 0),
        0x08..=0x0F | 0x18..=0x1F | 0x20..=0x27 | 0x30 | 0x31 | 0xE0..=0xE2 | 0xE8..=0xEA => {
            let n = match ty {
                0x08 | 0x18 | 0x20 | 0x30 => 1,
                0x09 | 0x19 | 0x21 | 0x31 => 2,
                t => usize::try_from((t & 0x07) + 1).ok()?,
            };
            le(n)?.into()
        }
        0x28..=0x2F => {
            let n = usize::try_from((ty & 0x07) + 1).ok()?;
            let u = le(n)?;
            let shift = 64 - 8 * n as u32;
            (((u << shift) as i64) >> shift).into()
        }
        0x42 | 0x41 => {
            let n = usize::from(*b.first()?);
            let s = b.get(1..1 + n.min(b.len() - 1))?;
            String::from_utf8_lossy(s)
                .trim_end_matches('\0')
                .to_string()
                .into()
        }
        _ => return None,
    })
}

/// A cached value as zigpy stored it.
fn value(v: ValueRef<'_>, ty: Option<i64>) -> Option<J> {
    match (v, ty) {
        (ValueRef::Integer(i), _) => Some(i.into()),
        (ValueRef::Real(f), _) => serde_json::Number::from_f64(f).map(J::Number),
        (ValueRef::Text(t), _) => Some(String::from_utf8_lossy(t).into_owned().into()),
        (ValueRef::Blob(b), Some(ty)) => typed(ty, b),
        // Untyped bytes (older schemas): text if it is text.
        (ValueRef::Blob(b), None) => std::str::from_utf8(b)
            .ok()
            .map(|s| s.trim_end_matches('\0').to_string().into()),
        (ValueRef::Null, _) => None,
    }
}

/// Basic attributes as document fields.
const BASIC: &[(i64, &str)] = &[
    (0x0000, "zcl_version"),
    (0x0001, "app_version"),
    (0x0002, "stack_version"),
    (0x0003, "hw_version"),
    (0x0004, "manufacturer"),
    (0x0005, "model"),
    (0x0006, "date_code"),
    (0x0007, "power_source"),
    (0x4000, "sw_build_id"),
];

fn kept(cluster: i64, attr: i64) -> bool {
    match (u16::try_from(cluster), u16::try_from(attr)) {
        (Ok(c), Ok(a)) => c != 0 && hub_devices::persist::persistent_value(c, a),
        _ => false,
    }
}

/// What a query of `db` returned, or the error as text.
fn rows<T>(
    db: &Connection,
    sql: &str,
    f: impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, String> {
    let mut st = db.prepare(sql).map_err(|e| format!("{sql}: {e}"))?;
    let r = st.query_map([], f).map_err(|e| format!("{sql}: {e}"))?;
    r.collect::<Result<_, _>>()
        .map_err(|e| format!("{sql}: {e}"))
}

/// The document of a ZHA `zigbee.db`; `backup`: a backup file instead of
/// the database's newest (ZHA's "download backup", OCB or zigpy's form).
pub fn from_db(path: &Path, backup: Option<J>, ha: Option<&HaNames>) -> Result<J, String> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;
    let v = version(&db)?;
    let devtable = if has_table(&db, &format!("zigbee_devices_v{v}")) {
        format!("zigbee_devices_v{v}")
    } else {
        format!("devices_v{v}")
    };
    // zigpy's `Status`: 2 (ENDPOINTS_INIT) is a finished interview; 0 (NEW)
    // and 1 (ZDO_INIT) are not.
    let status = if columns(&db, &devtable).iter().any(|c| c == "status") {
        "status"
    } else {
        "NULL"
    };
    let devs = rows(
        &db,
        &format!("SELECT ieee, nwk, {status} FROM {devtable}"),
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        },
    )?;
    let mut out: BTreeMap<String, serde_json::Map<String, J>> = BTreeMap::new();
    for (ieee, nwk, status) in devs {
        let Some(i) = ieee_hex(&ieee) else { continue };
        let mut m = serde_json::Map::new();
        m.insert("ieee".into(), i.clone().into());
        m.insert("nwk".into(), nwk.into());
        if let Some(s) = status {
            m.insert("interviewed".into(), (s == 2).into());
        }
        m.insert("endpoints".into(), json!([]));
        m.insert("source_id".into(), ieee.into());
        out.insert(i, m);
    }
    let key = |s: String| ieee_hex(&s).unwrap_or_default();
    let eps = rows(
        &db,
        &format!(
            "SELECT ieee, endpoint_id, profile_id, device_type FROM endpoints_v{v} ORDER BY ieee, endpoint_id"
        ),
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        },
    )?;
    for (ieee, ep, profile, dev) in eps {
        if let Some(m) = out.get_mut(&key(ieee)).filter(|_| ep > 0) {
            m["endpoints"]
                .as_array_mut()
                .unwrap()
                .push(json!({"id": ep, "profile": profile, "device_id": dev, "in": [], "out": []}));
        }
    }
    let clusters: Vec<(String, i64, i64, i64)> = if has_table(&db, &format!("clusters_v{v}")) {
        rows(
            &db,
            &format!(
                "SELECT ieee, endpoint_id, cluster_type, cluster_id FROM clusters_v{v} ORDER BY rowid"
            ),
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?
    } else {
        let mut c = rows(
            &db,
            &format!("SELECT ieee, endpoint_id, 0, cluster FROM in_clusters_v{v}"),
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        c.extend(rows(
            &db,
            &format!("SELECT ieee, endpoint_id, 1, cluster FROM out_clusters_v{v}"),
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?);
        c
    };
    for (ieee, ep, ty, c) in clusters {
        let Some(m) = out.get_mut(&key(ieee)) else {
            continue;
        };
        if let Some(e) = m["endpoints"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|e| e["id"] == ep)
        {
            e[if ty == 0 { "in" } else { "out" }]
                .as_array_mut()
                .unwrap()
                .push(c.into());
        }
    }
    let nd = format!("node_descriptors_v{v}");
    if has_table(&db, &nd) {
        let nds = rows(
            &db,
            &format!(
                "SELECT ieee, logical_type, mac_capability_flags, manufacturer_code, maximum_buffer_size, maximum_incoming_transfer_size, server_mask, maximum_outgoing_transfer_size FROM {nd}"
            ),
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    [
                        r.get::<_, i64>(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ],
                ))
            },
        )?;
        for (ieee, n) in nds {
            if let Some(m) = out.get_mut(&key(ieee)) {
                let t = ["coordinator", "router", "end_device"]
                    .get(usize::try_from(n[0]).unwrap_or(9))
                    .copied();
                if let Some(t) = t {
                    m.insert("type".into(), t.into());
                }
                m.insert(
                    "node_descriptor".into(),
                    json!({"logical_type": n[0], "mac_capabilities": n[1], "manufacturer_code": n[2], "max_buffer_size": n[3],
                           "max_incoming_transfer": n[4], "server_mask": n[5], "max_outgoing_transfer": n[6]}),
                );
                m.insert("manufacturer_code".into(), n[2].into());
            }
        }
    }
    let at = format!("attributes_cache_v{v}");
    let cols = columns(&db, &at);
    let has = |c: &str| cols.iter().any(|x| x == c);
    let (cl, attr) = if has("cluster_id") {
        ("cluster_id", "attr_id")
    } else {
        ("cluster", "attrid")
    };
    let ctype = if has("cluster_type") {
        "cluster_type"
    } else {
        "0"
    };
    let dtype = if has("data_type") {
        "data_type"
    } else {
        "NULL"
    };
    let status = if has("status") {
        "AND (status IS NULL OR status = 0)"
    } else {
        ""
    };
    let mfr = if has("manufacturer_code") {
        "AND manufacturer_code IS NULL"
    } else {
        ""
    };
    let sql = format!(
        "SELECT ieee, endpoint_id, {cl}, {attr}, value, {dtype} FROM {at} WHERE {ctype} = 0 {status} {mfr} ORDER BY ieee, endpoint_id"
    );
    let mut st = db.prepare(&sql).map_err(|e| format!("{sql}: {e}"))?;
    let mut q = st.query([]).map_err(|e| e.to_string())?;
    while let Some(r) = q.next().map_err(|e| e.to_string())? {
        let (Ok(ieee), Ok(ep), Ok(c), Ok(a)) = (
            r.get::<_, String>(0),
            r.get::<_, Option<i64>>(1),
            r.get::<_, i64>(2),
            r.get::<_, i64>(3),
        ) else {
            continue;
        };
        let Some(m) = out.get_mut(&key(ieee)) else {
            continue;
        };
        let Some(val) = value(
            r.get_ref(4).map_err(|e| e.to_string())?,
            r.get::<_, Option<i64>>(5).ok().flatten(),
        ) else {
            continue;
        };
        if c == 0 {
            if let Some((_, k)) = BASIC.iter().find(|(id, _)| *id == a) {
                m.entry(*k).or_insert(val);
            }
        } else if kept(c, a)
            && let Some(ep) = ep
        {
            let list = m.entry("attributes").or_insert_with(|| json!([]));
            list.as_array_mut()
                .unwrap()
                .push(json!({"ep": ep, "cluster": c, "attr": a, "value": val}));
        }
    }
    if let Some(ha) = ha {
        for (ieee, (name, area)) in &ha.devices {
            if let Some(m) = out.get_mut(ieee) {
                if let Some(n) = name {
                    m.insert("name".into(), n.clone().into());
                }
                if let Some(a) = area {
                    m.insert("area".into(), a.clone().into());
                }
            }
        }
    }
    let gp = format!("gp_devices_v{v}");
    if has_table(&db, &gp) {
        for ieee in rows(&db, &format!("SELECT ieee FROM {gp}"), |r| {
            r.get::<_, String>(0)
        })? {
            if let Some(i) = ieee_hex(&ieee) {
                out.insert(
                    i.clone(),
                    [
                        ("ieee".to_string(), J::from(i)),
                        ("type".into(), "green_power".into()),
                    ]
                    .into_iter()
                    .collect(),
                );
            }
        }
    }
    let mut groups: BTreeMap<i64, J> = BTreeMap::new();
    if has_table(&db, &format!("groups_v{v}")) {
        for (id, name) in rows(
            &db,
            &format!("SELECT group_id, name FROM groups_v{v}"),
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )? {
            groups.insert(id, json!({"id": id, "name": name, "members": []}));
        }
        for (id, ieee, ep) in rows(
            &db,
            &format!("SELECT group_id, ieee, endpoint_id FROM group_members_v{v}"),
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )? {
            if let (Some(g), Some(i)) = (groups.get_mut(&id), ieee_hex(&ieee)) {
                g["members"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"ieee": i, "ep": ep}));
            }
        }
    }
    let network = match backup {
        Some(b) => Some(zigpy::to_ocb(&b)?),
        None => {
            let t = format!("network_backups_v{v}");
            let last = if has_table(&db, &t) {
                rows(
                    &db,
                    &format!("SELECT backup_json FROM {t} ORDER BY id DESC LIMIT 1"),
                    |r| r.get::<_, String>(0),
                )?
            } else {
                Vec::new()
            };
            match last.first() {
                Some(j) => Some(zigpy::to_ocb(
                    &serde_json::from_str(j).map_err(|e| format!("{t}: {e}"))?,
                )?),
                None => {
                    eprintln!(
                        "{}: no network backup; pass ZHA's with --backup",
                        path.display()
                    );
                    None
                }
            }
        }
    };
    // The coordinator is the backup's node (and NWK 0x0000).
    let coord = network
        .as_ref()
        .and_then(|n| n["coordinator_ieee"].as_str())
        .map(str::to_string);
    for (i, m) in out.iter_mut() {
        if Some(i) == coord.as_ref() || m.get("nwk") == Some(&J::from(0)) {
            m.insert("type".into(), "coordinator".into());
        }
    }
    let name = format!("ZHA (zigpy database v{v})");
    Ok(document(
        "zha",
        &name,
        network,
        out.into_values().map(J::Object).collect(),
        groups.into_values().collect(),
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// zigpy's tables of schema `v` (v16 and v17 differ in the device
    /// table and the typed attribute values), with generated devices: the
    /// coordinator, a colour light, a sleepy sensor and a group.
    pub fn make_db(path: &Path, v: u32) {
        let _ = std::fs::remove_file(path);
        let db = Connection::open(path).unwrap();
        let dev = if v >= 17 { "zigbee_devices" } else { "devices" };
        let typed = if v >= 17 { ", data_type INTEGER" } else { "" };
        let mut sql = format!(
            "PRAGMA user_version = {v};
             CREATE TABLE {dev}_v{v} (ieee ieee NOT NULL, nwk INTEGER NOT NULL, status INTEGER NOT NULL, last_seen REAL NOT NULL);
             CREATE TABLE endpoints_v{v} (ieee ieee NOT NULL, endpoint_id INTEGER NOT NULL, profile_id INTEGER NOT NULL, device_type INTEGER NOT NULL, status INTEGER NOT NULL);
             CREATE TABLE clusters_v{v} (ieee ieee NOT NULL, endpoint_id INTEGER NOT NULL, cluster_type INTEGER NOT NULL, cluster_id INTEGER NOT NULL);
             CREATE TABLE attributes_cache_v{v} (ieee ieee NOT NULL, endpoint_id INTEGER, cluster_type INTEGER NOT NULL, cluster_id INTEGER NOT NULL,
                 attr_id INTEGER NOT NULL, manufacturer_code INTEGER, status INTEGER{typed}, value BLOB, last_updated REAL NOT NULL);
             CREATE TABLE node_descriptors_v{v} (ieee ieee NOT NULL, logical_type INTEGER NOT NULL, complex_descriptor_available INTEGER NOT NULL,
                 user_descriptor_available INTEGER NOT NULL, reserved INTEGER NOT NULL, aps_flags INTEGER NOT NULL, frequency_band INTEGER NOT NULL,
                 mac_capability_flags INTEGER NOT NULL, manufacturer_code INTEGER NOT NULL, maximum_buffer_size INTEGER NOT NULL,
                 maximum_incoming_transfer_size INTEGER NOT NULL, server_mask INTEGER NOT NULL, maximum_outgoing_transfer_size INTEGER NOT NULL,
                 descriptor_capability_field INTEGER NOT NULL);
             CREATE TABLE groups_v{v} (group_id INTEGER NOT NULL, name TEXT NOT NULL);
             CREATE TABLE group_members_v{v} (group_id INTEGER NOT NULL, ieee ieee NOT NULL, endpoint_id INTEGER NOT NULL);
             CREATE TABLE network_backups_v{v} (id INTEGER PRIMARY KEY AUTOINCREMENT, backup_json TEXT NOT NULL);
             CREATE TABLE gp_devices_v{v} (ieee ieee NOT NULL, application_id INTEGER NOT NULL, src_id INTEGER, last_seen REAL NOT NULL);
             CREATE TABLE endpoints_v15 (ieee ieee NOT NULL);"
        );
        if v >= 17 {
            sql += &format!("CREATE TABLE devices_v{v} (ieee ieee NOT NULL);");
        }
        db.execute_batch(&sql).unwrap();
        let coord = "00:0b:57:ff:fe:aa:00:01";
        let light = "00:17:88:01:00:f1:e2:d3";
        let sensor = "00:15:8d:00:03:c4:d5:e6";
        // ZDO_INIT (1): its interview stopped before the simple descriptors.
        let unfinished = "00:12:4b:00:aa:bb:cc:01";
        for (ieee, nwk, status) in [
            (coord, 0, 2),
            (light, 0x7A10, 2),
            (sensor, 0x04B2, 2),
            (unfinished, 0x1111, 1),
        ] {
            db.execute(
                &format!("INSERT INTO {dev}_v{v} VALUES (?1, ?2, ?3, 1764000000.0)"),
                (ieee, nwk, status),
            )
            .unwrap();
        }
        for (ieee, ep, prof, dt, ins, outs) in [
            (light, 11, 260, 269, vec![0, 3, 4, 5, 6, 8, 768], vec![25]),
            (light, 242, 41440, 97, vec![], vec![33]),
            (sensor, 1, 260, 24321, vec![0, 3, 1026, 1029], vec![25]),
            (unfinished, 1, 260, 0, vec![], vec![]),
        ] {
            db.execute(
                &format!("INSERT INTO endpoints_v{v} VALUES (?1, ?2, ?3, ?4, 1)"),
                (ieee, ep, prof, dt),
            )
            .unwrap();
            for (t, list) in [(0, ins), (1, outs)] {
                for c in list {
                    db.execute(
                        &format!("INSERT INTO clusters_v{v} VALUES (?1, ?2, ?3, ?4)"),
                        (ieee, ep, t, c),
                    )
                    .unwrap();
                }
            }
        }
        for (ieee, lt, mac, mfr) in [
            (coord, 0, 0x8E, 0x1049),
            (light, 1, 0x8E, 0x100B),
            (sensor, 2, 0x80, 0x1037),
        ] {
            db.execute(&format!("INSERT INTO node_descriptors_v{v} VALUES (?1, ?2, 0, 0, 0, 0, 8, ?3, ?4, 82, 82, 11264, 82, 0)"), (ieee, lt, mac, mfr)).unwrap();
        }
        let attr = |ieee: &str,
                    ep: i64,
                    c: i64,
                    a: i64,
                    plain: rusqlite::types::Value,
                    typed: (i64, Vec<u8>)| {
            if v >= 17 {
                db.execute(&format!("INSERT INTO attributes_cache_v{v} VALUES (?1, ?2, 0, ?3, ?4, NULL, 0, ?5, ?6, 1764000000.0)"), (ieee, ep, c, a, typed.0, typed.1))
                    .unwrap();
            } else {
                db.execute(&format!("INSERT INTO attributes_cache_v{v} VALUES (?1, ?2, 0, ?3, ?4, NULL, 0, ?5, 1764000000.0)"), (ieee, ep, c, a, plain)).unwrap();
            }
        };
        let s = |t: &str| {
            (
                rusqlite::types::Value::Text(t.into()),
                (0x42, [&[t.len() as u8][..], t.as_bytes()].concat()),
            )
        };
        let u = |n: i64, ty: i64, len: usize| {
            (
                rusqlite::types::Value::Integer(n),
                (ty, n.to_le_bytes()[..len].to_vec()),
            )
        };
        for (ieee, ep, c, a, (p, t)) in [
            (light, 11, 0, 4, s("Signify Netherlands B.V.")),
            (light, 11, 0, 5, s("LCT015")),
            (light, 11, 0, 7, u(1, 0x30, 1)),
            (light, 11, 0, 0x4000, s("1.50.2_r30933")),
            (light, 11, 0x0300, 0x400A, u(31, 0x19, 2)),
            (light, 11, 0x0300, 0x0003, u(1000, 0x21, 2)),
            (sensor, 1, 0, 4, s("LUMI")),
            (sensor, 1, 0, 5, s("lumi.weather")),
            (sensor, 1, 0, 7, u(3, 0x30, 1)),
        ] {
            attr(ieee, ep, c, a, p, t);
        }
        db.execute(
            &format!("INSERT INTO groups_v{v} VALUES (4097, 'Hall')"),
            [],
        )
        .unwrap();
        db.execute(
            &format!("INSERT INTO group_members_v{v} VALUES (4097, ?1, 11), (4097, ?2, 1)"),
            (light, coord),
        )
        .unwrap();
        db.execute(
            &format!(
                "INSERT INTO network_backups_v{v} (backup_json) VALUES ('{{\"old\": true}}'), (?1)"
            ),
            [super::zigpy::tests::ZIGPY],
        )
        .unwrap();
        db.execute(&format!("INSERT INTO gp_devices_v{v} VALUES ('00:00:00:00:01:78:80:ff', 0, 25722111, 1764000000.0)"), []).unwrap();
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/hubctl-test");
        std::fs::create_dir_all(&d).unwrap();
        d.join(format!("{name}-{}", std::process::id()))
    }

    #[test]
    fn zigpy_databases_become_document_devices() {
        for v in [16, 17] {
            let p = tmp(&format!("zigbee-v{v}.db"));
            make_db(&p, v);
            let reg = json!({"data": {"devices": [
                {"identifiers": [["zha", "00:17:88:01:00:f1:e2:d3"]], "name": "LCT015", "name_by_user": "Hall light", "area_id": "hall"},
                {"identifiers": [["mqtt", "zigbee2mqtt_0x00158d0003c4d5e6"]], "name": "other"}]}});
            let areas = [("hall".to_string(), "Hall".to_string())]
                .into_iter()
                .collect();
            let ha = HaNames::from_registry(&reg, &areas);
            let doc = from_db(&p, None, Some(&ha)).unwrap();
            assert_eq!(doc["source"]["name"], format!("ZHA (zigpy database v{v})"));
            assert_eq!(
                doc["network"]["extended_pan_id"], "a1b2c3d4e5f60718",
                "v{v}: the newest backup"
            );
            let d = doc["devices"].as_array().unwrap();
            let by = |i: &str| {
                d.iter()
                    .find(|x| x["ieee"] == i)
                    .unwrap_or_else(|| panic!("v{v}: {i}"))
            };
            assert_eq!(by("000b57fffeaa0001")["type"], "coordinator");
            let light = by("0017880100f1e2d3");
            assert_eq!(
                (
                    light["name"].as_str(),
                    light["area"].as_str(),
                    light["type"].as_str()
                ),
                (Some("Hall light"), Some("Hall"), Some("router")),
                "v{v}"
            );
            assert_eq!(
                (
                    light["model"].as_str(),
                    light["power_source"].as_u64(),
                    light["sw_build_id"].as_str()
                ),
                (Some("LCT015"), Some(1), Some("1.50.2_r30933")),
                "v{v}"
            );
            assert_eq!(
                light["endpoints"][0],
                json!({"id": 11, "profile": 260, "device_id": 269, "in": [0, 3, 4, 5, 6, 8, 768], "out": [25]})
            );
            assert_eq!(
                light["attributes"],
                json!([{"ep": 11, "cluster": 768, "attr": 16394, "value": 31}]),
                "v{v}: currentX is not kept"
            );
            assert_eq!(light["node_descriptor"]["mac_capabilities"], 0x8E);
            let sensor = by("00158d0003c4d5e6");
            assert_eq!(
                (
                    sensor["type"].as_str(),
                    sensor["model"].as_str(),
                    sensor["nwk"].as_u64()
                ),
                (Some("end_device"), Some("lumi.weather"), Some(0x04B2))
            );
            assert!(
                sensor.get("name").is_none(),
                "an MQTT device's name is not ZHA's"
            );
            assert_eq!(by("00000000017880ff")["type"], "green_power");
            assert_eq!(
                (
                    light["interviewed"].as_bool(),
                    sensor["interviewed"].as_bool()
                ),
                (Some(true), Some(true))
            );
            let unfinished = by("00124b00aabbcc01");
            assert_eq!(unfinished["interviewed"], false, "v{v}: ZDO_INIT");
            match hub_app::migrate::entry(unfinished) {
                hub_app::migrate::Entry::Device(d) => assert!(d.incomplete()),
                _ => panic!("v{v}: imported, interviewed when heard from"),
            }
            assert_eq!(
                doc["groups"],
                json!([{"id": 4097, "name": "Hall", "members": [{"ieee": "0017880100f1e2d3", "ep": 11}, {"ieee": "000b57fffeaa0001", "ep": 1}]}])
            );
            // The hub takes every entry.
            for x in d {
                let e = hub_app::migrate::entry(x);
                assert!(
                    !matches!(
                        e,
                        hub_app::migrate::Entry::Skip {
                            reason: "invalid" | "no_nwk",
                            ..
                        }
                    ),
                    "v{v}: {x}"
                );
            }
            std::fs::remove_file(&p).unwrap();
        }
    }

    #[test]
    fn typed_values_decode() {
        assert_eq!(typed(0x42, b"\x05LUMI\0"), Some(json!("LUMI")));
        assert_eq!(typed(0x21, &[0x10, 0x27]), Some(json!(10000)));
        assert_eq!(typed(0x29, &[0xFE, 0xFF]), Some(json!(-2)));
        assert_eq!(typed(0x10, &[1]), Some(json!(true)));
        assert_eq!(typed(0x23, &[1, 0]), None, "short");
        assert!(from_db(&tmp("missing.db"), None, None).is_err());
    }
}
