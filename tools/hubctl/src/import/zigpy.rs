//! zigpy's own network backup (`NetworkBackup.as_dict()`, what ZHA keeps in
//! `zigbee.db`'s `network_backups_v*` table) → Open Coordinator Backup, as
//! zigpy's `_network_backup_to_open_coordinator_backup` writes it (the
//! format the hub restores). Backups that are OCB already pass through.
//!
//! zigpy's dictionary writes EUI64s and keys as `aa:bb:…` (EUI64s
//! most significant byte first, keys in key order), NWK addresses and
//! PAN IDs as hex without `0x`.

use serde_json::{Map, Value as J, json};

fn plain(s: &str) -> String {
    s.chars()
        .filter(|c| *c != ':')
        .collect::<String>()
        .to_ascii_lowercase()
}

fn nwk(s: &str) -> String {
    format!("{:0>4}", s.trim_start_matches("0x").to_ascii_lowercase())
}

fn st<'a>(v: &'a J, k: &str) -> Result<&'a str, String> {
    v.get(k)
        .and_then(J::as_str)
        .ok_or_else(|| format!("network backup: no {k}"))
}

/// The OCB of a zigpy backup dictionary (or the OCB itself).
pub fn to_ocb(b: &J) -> Result<J, String> {
    if b.get("metadata").is_some() {
        return Ok(b.clone());
    }
    let ni = b
        .get("network_info")
        .ok_or("not a zigpy network backup (no network_info)")?;
    let node = b
        .get("node_info")
        .ok_or("not a zigpy network backup (no node_info)")?;
    let coord = plain(st(node, "ieee")?);
    let mut devices: Vec<(String, Map<String, J>)> = Vec::new();
    fn dev(devices: &mut Vec<(String, Map<String, J>)>, ieee: String) -> usize {
        match devices.iter().position(|(i, _)| *i == ieee) {
            Some(p) => p,
            None => {
                let mut m = Map::new();
                m.insert("ieee_address".into(), ieee.clone().into());
                m.insert("nwk_address".into(), J::Null);
                m.insert("is_child".into(), false.into());
                devices.push((ieee, m));
                devices.len() - 1
            }
        }
    }
    let mut todo: Vec<(String, String, J)> = Vec::new();
    for (ieee, n) in ni
        .get("nwk_addresses")
        .and_then(J::as_object)
        .into_iter()
        .flatten()
    {
        todo.push((
            plain(ieee),
            "nwk_address".into(),
            n.as_str().map(nwk).map_or(J::Null, J::from),
        ));
    }
    for c in ni
        .get("children")
        .and_then(J::as_array)
        .into_iter()
        .flatten()
        .filter_map(J::as_str)
    {
        todo.push((plain(c), "is_child".into(), true.into()));
    }
    let mut seqs = Map::new();
    for k in ni
        .get("key_table")
        .and_then(J::as_array)
        .into_iter()
        .flatten()
    {
        let ieee = plain(st(k, "partner_ieee")?);
        seqs.insert(ieee.clone(), k.get("seq").cloned().unwrap_or(0.into()));
        let key = json!({"key": plain(st(k, "key")?), "tx_counter": k.get("tx_counter").cloned().unwrap_or(0.into()), "rx_counter": k.get("rx_counter").cloned().unwrap_or(0.into())});
        todo.push((ieee, "link_key".into(), key));
    }
    for (ieee, k, v) in todo {
        let i = dev(&mut devices, ieee);
        devices[i].1.insert(k, v);
    }
    devices.sort_by(|a, b| a.0.cmp(&b.0));
    let tclk = ni.get("tc_link_key").cloned().unwrap_or(J::Null);
    let nk = ni
        .get("network_key")
        .ok_or("network backup: no network_key")?;
    let mut internal = Map::new();
    internal.insert(
        "creation_time".into(),
        b.get("backup_time").cloned().unwrap_or(J::Null),
    );
    internal.insert(
        "node".into(),
        json!({"ieee": coord, "nwk": node.get("nwk").and_then(J::as_str).map_or("0000".into(), nwk), "type": node.get("logical_type").cloned().unwrap_or("coordinator".into()),
               "model": node.get("model"), "manufacturer": node.get("manufacturer"), "version": node.get("version")}),
    );
    if tclk.is_object() {
        internal.insert(
            "network".into(),
            json!({"tc_link_key": {"key": plain(st(&tclk, "key")?), "frame_counter": tclk.get("tx_counter").cloned().unwrap_or(0.into())},
                   "tc_address": plain(tclk.get("partner_ieee").and_then(J::as_str).unwrap_or(&coord)),
                   "nwk_manager": ni.get("nwk_manager_id").and_then(J::as_str).map_or("0000".into(), nwk)}),
        );
    }
    internal.insert("link_key_seqs".into(), J::Object(seqs));
    internal.insert(
        "tx_power".into(),
        ni.get("tx_power").cloned().unwrap_or(J::Null),
    );
    for (k, v) in ni
        .get("metadata")
        .and_then(J::as_object)
        .into_iter()
        .flatten()
    {
        internal.insert(k.clone(), v.clone());
    }
    Ok(json!({
        "metadata": {"version": 1, "format": hub_app::ocb::FORMAT, "source": ni.get("source"), "internal": internal},
        "stack_specific": ni.get("stack_specific").cloned().unwrap_or(json!({})),
        "coordinator_ieee": coord,
        "pan_id": nwk(st(ni, "pan_id")?),
        "extended_pan_id": plain(st(ni, "extended_pan_id")?),
        "nwk_update_id": ni.get("nwk_update_id").cloned().unwrap_or(0.into()),
        "security_level": ni.get("security_level").cloned().unwrap_or(5.into()),
        "channel": ni.get("channel"),
        "channel_mask": ni.get("channel_mask").cloned().unwrap_or(json!([])),
        "network_key": {"key": plain(st(nk, "key")?), "sequence_number": nk.get("seq").cloned().unwrap_or(0.into()), "frame_counter": nk.get("tx_counter").cloned().unwrap_or(0.into())},
        "devices": devices.into_iter().map(|(_, m)| J::Object(m)).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A zigpy (bellows) backup as `NetworkBackup.as_dict()` writes it
    /// (generated values).
    pub const ZIGPY: &str = r#"{"version": 1, "backup_time": "2026-11-20T10:00:00+00:00",
      "network_info": {"extended_pan_id": "a1:b2:c3:d4:e5:f6:07:18", "pan_id": "0F3A", "nwk_update_id": 4, "nwk_manager_id": "0000",
        "channel": 20, "channel_mask": [20], "security_level": 5,
        "network_key": {"key": "88:99:aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77", "tx_counter": 123456, "rx_counter": 0, "seq": 3, "partner_ieee": "ff:ff:ff:ff:ff:ff:ff:ff"},
        "tc_link_key": {"key": "5a:69:67:42:65:65:41:6c:6c:69:61:6e:63:65:30:39", "tx_counter": 4096, "rx_counter": 0, "seq": 0, "partner_ieee": "00:0b:57:ff:fe:aa:00:01"},
        "key_table": [{"key": "01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef", "tx_counter": 12, "rx_counter": 34, "seq": 1, "partner_ieee": "00:15:8d:00:03:c4:d5:e6"}],
        "children": ["00:15:8d:00:03:c4:d5:e6"], "route_table": {}, "tx_power": 8,
        "nwk_addresses": {"00:15:8d:00:03:c4:d5:e6": "04B2", "00:17:88:01:00:f1:e2:d3": "7A10"},
        "stack_specific": {"ezsp": {"hashed_tclk": "33333333333333333333333333333333"}},
        "metadata": {"ezsp": {"stack_version": 13}}, "source": "bellows@0.47.0"},
      "node_info": {"nwk": "0000", "ieee": "00:0b:57:ff:fe:aa:00:01", "logical_type": "coordinator", "model": "MGM24", "manufacturer": "Silicon Labs", "version": "7.4.5.0"}}"#;

    #[test]
    fn a_zigpy_backup_becomes_the_ocb_zigpy_would_export() {
        let o = to_ocb(&serde_json::from_str(ZIGPY).unwrap()).unwrap();
        assert_eq!(
            (
                o["coordinator_ieee"].as_str(),
                o["pan_id"].as_str(),
                o["extended_pan_id"].as_str()
            ),
            (
                Some("000b57fffeaa0001"),
                Some("0f3a"),
                Some("a1b2c3d4e5f60718")
            )
        );
        assert_eq!(
            o["network_key"],
            json!({"key": "8899aabbccddeeff0011223344556677", "sequence_number": 3, "frame_counter": 123456})
        );
        assert_eq!(
            o["metadata"]["internal"]["network"]["tc_link_key"]["frame_counter"],
            4096
        );
        assert_eq!(o["metadata"]["internal"]["ezsp"]["stack_version"], 13);
        assert_eq!(
            o["stack_specific"]["ezsp"]["hashed_tclk"],
            "33333333333333333333333333333333"
        );
        let d = o["devices"].as_array().unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(
            d[0],
            json!({"ieee_address": "00158d0003c4d5e6", "nwk_address": "04b2", "is_child": true,
            "link_key": {"key": "0123456789abcdef0123456789abcdef", "tx_counter": 12, "rx_counter": 34}})
        );
        assert_eq!(
            (d[1]["nwk_address"].as_str(), d[1]["is_child"].as_bool()),
            (Some("7a10"), Some(false))
        );
        // What the hub restores.
        struct Keep;
        impl hub_coord::RestoreEncoder for Keep {
            fn push(&mut self, _: &hub_coord::BackupItem) -> Result<(), hub_coord::CoordError> {
                Ok(())
            }
            fn finish(self: Box<Self>) -> hub_coord::RestoreBlob {
                hub_coord::RestoreBlob::default()
            }
        }
        let r = hub_app::ocb::to_restore(&o, Box::new(Keep), hub_app::ocb::DevicePolicy::default()).unwrap();
        assert_eq!((r.pan_id, r.channel, r.devices.len()), (0x0F3A, 20, 2));
        assert!(r.tclk_counter_known);
        // OCB passes through.
        assert_eq!(to_ocb(&o).unwrap(), o);
        assert!(to_ocb(&json!({"x": 1})).is_err());
    }
}
