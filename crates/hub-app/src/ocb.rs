//! Open ZigBee Coordinator Backup (`zigpy/open-coordinator-backup` v1, the
//! format of zigpy/ZHA and zigbee-herdsman/Z2M; ZRHP spec 9.1/9.2) ⇄ the
//! coordinator backend's backup items ([`BackupItem`]; the backend encodes
//! them, ZRHP as record streams, spec 9.3).
//!
//! Encoding rules: IEEE, PAN ID, EPID and NWK addresses are big-endian hex
//! (reversed from the wire), keys are hex in key-byte order. Import
//! tolerates the known quirks: Z2M's unpadded PAN ID and NWK addresses,
//! `internal.date` instead of `creation_time`, no TCLK section (the default
//! `ZigBeeAlliance09` and an APS margin of 2²⁰, spec 9.4 rule 6).
//!
//! Byte order as zigpy reads every OCB file (`zigpy/backups.py`
//! `_open_coordinator_backup_to_network_backup`, which ZHA's backup upload
//! uses through `NetworkBackup.from_dict`): Z2M's files are read exactly
//! like zigpy's, with no byte-order detection or swap; zigpy's only Z2M
//! handling is `internal.date` (the backup time), the unpadded
//! `nwk_address`, the optional `is_child` and the default TCLK, as here.
//! The PAN ID is left-padded too, where zigpy refuses Z2M's unpadded one.
//!
//! EmberZNet backups follow bellows (zigpy/ZHA) and zigbee-herdsman's
//! ember adapter (Z2M): with hashed trust center link keys the TCLK is
//! `ZigBeeAlliance09` and its root key is `stack_specific.ezsp.hashed_tclk`;
//! `metadata.internal.ezspVersion` names the EZSP version (Z2M loads no
//! EmberZNet backup without it and `stack_specific.ezsp`).
//!
//! The export is written as text directly (no JSON tree): a backup of a
//! hundred devices would otherwise need tens of kilobytes of heap on a hub
//! without PSRAM. Nothing here logs a key.

use std::fmt::Write as _;

use hub_coord::{BackupChild, BackupDevice, BackupItem, BackupLinkKey, BackupSink, CoordError, RestoreEncoder};
use serde_json::Value as J;

/// `metadata.format`.
pub const FORMAT: &str = "zigpy/open-coordinator-backup";
/// The global default Trust Center link key.
pub const ZIGBEE_ALLIANCE_09: [u8; 16] = *b"ZigBeeAlliance09";
/// APS margin when the backup has no TCLK frame counter (spec 9.4 rule 6).
pub const UNKNOWN_APS_MARGIN: u32 = 1 << 20;

/// A backup converted for the backend's restore.
pub struct Restore {
    /// The backend's encoding of the items so far.
    pub encoder: Box<dyn RestoreEncoder>,
    /// The backup carries a TCLK frame counter (else use
    /// [`UNKNOWN_APS_MARGIN`]).
    pub tclk_counter_known: bool,
    /// Coordinator IEEE (wire order).
    pub coordinator_ieee: [u8; 8],
    /// PAN ID.
    pub pan_id: u16,
    /// Extended PAN ID (wire order): the records are tagged with it before
    /// the NCP forms the network (`NcpNv`).
    pub ext_pan_id: [u8; 8],
    /// Channel.
    pub channel: u8,
    /// Devices: IEEE (wire order) and NWK address.
    pub devices: Vec<([u8; 8], Option<u16>)>,
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    let s: String = s.chars().filter(|c| *c != ':' && *c != '-').collect();
    if !s.len().is_multiple_of(2) {
        return Err(format!("odd hex length: {s:?}"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| format!("not hex: {s:?}")))
        .collect()
}

/// Exactly `N` bytes in key-byte order.
fn key_bytes<const N: usize>(s: &str, what: &str) -> Result<[u8; N], String> {
    unhex(s)?.try_into().map_err(|_| format!("{what}: expected {N} bytes"))
}

/// Big-endian hex, left-padded (Z2M writes `toString(16)`), returned in
/// wire (little-endian) order.
fn be_wire<const N: usize>(s: &str, what: &str) -> Result<[u8; N], String> {
    let digits = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    let padded = format!("{digits:0>width$}", width = 2 * N);
    let mut b: [u8; N] = unhex(&padded)?.try_into().map_err(|_| format!("{what}: expected {N} bytes"))?;
    b.reverse();
    Ok(b)
}

fn path<'a>(v: Option<&'a J>, keys: &[&str]) -> Option<&'a J> {
    keys.iter().try_fold(v?, |v, k| v.get(k))
}

/// A required string field.
fn text<'a>(o: &'a J, k: &str) -> Result<&'a str, String> {
    o.get(k).and_then(J::as_str).ok_or_else(|| format!("{k}: missing or not a string"))
}

/// An optional unsigned field (`default` when absent or null).
fn uint(o: &J, k: &str, default: u64) -> Result<u64, String> {
    match o.get(k) {
        None | Some(J::Null) => Ok(default),
        Some(v) => v.as_u64().ok_or_else(|| format!("{k}: not a non-negative integer")),
    }
}

fn u32_of(o: &J, k: &str) -> Result<u32, String> {
    Ok(uint(o, k, 0)?.min(u64::from(u32::MAX)) as u32)
}

/// Append one item to the restore.
fn push(enc: &mut dyn RestoreEncoder, item: BackupItem) -> Result<(), String> {
    enc.push(&item).map_err(|e| e.to_string())
}

/// Validate a backup (OCB JSON value, spec 9.1/9.2) and convert it into
/// restore items, encoded by `encoder`. `devices` may be absent: a large
/// backup's devices can follow in batches ([`add_devices`]), so no request
/// needs the whole file in RAM.
pub fn to_restore(o: &J, encoder: Box<dyn RestoreEncoder>) -> Result<Restore, String> {
    let meta = o.get("metadata").ok_or("not an open coordinator backup (no metadata)")?;
    let (format, version) = (meta.get("format").and_then(J::as_str).unwrap_or(""), uint(meta, "version", 0)?);
    if format != FORMAT || version != 1 {
        return Err(format!("not an open coordinator backup v1 ({format} v{version})"));
    }
    let security_level = uint(o, "security_level", 5)?;
    if security_level != 5 {
        return Err(format!("security level {security_level} (only 5 is supported)"));
    }
    let channel = uint(o, "channel", 0)?;
    if !(11..=26).contains(&channel) {
        return Err(format!("channel {channel} outside 11–26"));
    }
    let channel = channel as u8;
    let ieee: [u8; 8] = be_wire(text(o, "coordinator_ieee")?, "coordinator_ieee")?;
    let pan_id = u16::from_le_bytes(be_wire(text(o, "pan_id")?, "pan_id")?);
    if pan_id == 0 || pan_id == 0xFFFF {
        return Err(format!("PAN ID 0x{pan_id:04X}"));
    }
    let epid: [u8; 8] = be_wire(text(o, "extended_pan_id")?, "extended_pan_id")?;
    if epid == [0; 8] || epid == [0xFF; 8] {
        return Err("extended PAN ID all zero or all ones".into());
    }
    let nk = o.get("network_key").filter(|k| k.is_object()).ok_or("the backup has no network key (incomplete backup)")?;
    let key: [u8; 16] = key_bytes(text(nk, "key")?, "network_key")?;
    if key == [0; 16] {
        return Err("all-zero network key (incomplete backup)".into());
    }
    let internal = meta.get("internal");
    let tclk = path(internal, &["network", "tc_link_key"]);
    let tclk_key = match tclk.and_then(|t| t.get("key")).and_then(J::as_str) {
        Some(k) => key_bytes(k, "tc_link_key")?,
        None => ZIGBEE_ALLIANCE_09,
    };
    let tclk_counter = tclk.and_then(|t| t.get("frame_counter")).and_then(J::as_u64);
    let tc_ieee = match path(internal, &["network", "tc_address"]).and_then(J::as_str) {
        Some(a) => be_wire(a, "tc_address")?,
        None => ieee,
    };
    let tx_power = path(internal, &["tx_power"]).and_then(J::as_i64).map(|p| p.clamp(-128, 127) as i8);
    let mask = o
        .get("channel_mask")
        .and_then(J::as_array)
        .map_or(0, |a| a.iter().filter_map(J::as_u64).filter(|c| (11..=26).contains(c)).fold(0u32, |m, c| m | 1 << c));
    let mut r = Restore { encoder, tclk_counter_known: tclk_counter.is_some(), coordinator_ieee: ieee, pan_id, ext_pan_id: epid, channel, devices: Vec::new() };
    push(&mut *r.encoder, BackupItem::Node { ieee, ieee_override: true })?;
    push(
        &mut *r.encoder,
        BackupItem::Network {
            pan_id,
            ext_pan_id: epid,
            channel,
            channel_mask: if mask == 0 { 1 << channel } else { mask },
            nwk_update_id: uint(o, "nwk_update_id", 0)? as u8,
            nwk_manager: 0,
            security_level: 5,
            tx_power,
        },
    )?;
    let seq = uint(nk, "sequence_number", 0)? as u8;
    push(&mut *r.encoder, BackupItem::NetworkKey { key, seq, tx_counter: u32_of(nk, "frame_counter")?, active: true })?;
    push(
        &mut *r.encoder,
        BackupItem::TcLinkKey {
            key: tclk_key,
            tc_ieee,
            tx_counter: tclk_counter.map_or(0, |c| c.min(u64::from(u32::MAX)) as u32),
            hashed: false,
        },
    )?;
    let ss = o.get("stack_specific");
    if let Some(seed) = path(ss, &["ezsp", "hashed_tclk"]).and_then(J::as_str) {
        push(&mut *r.encoder, BackupItem::HashedTclkSeed(key_bytes::<16>(seed, "hashed_tclk")?))?;
    }
    if let Some(rs) = path(ss, &["zigbee_rs"]) {
        push(
            &mut *r.encoder,
            BackupItem::RsParams {
                fc_stride: u32_of(rs, "frame_counter_stride")?,
                table_epoch: u32_of(rs, "table_epoch")?,
                nwk_fc_limit: u32_of(rs, "nwk_frame_counter_limit")?,
                aps_fc_limit: u32_of(rs, "aps_frame_counter_limit")?,
                host_backed_tables: rs.get("host_backed_tables").and_then(J::as_bool).unwrap_or(false),
            },
        )?;
    }
    if let Some(devices) = o.get("devices").filter(|d| !d.is_null()) {
        add_devices(&mut r, devices)?;
    }
    Ok(r)
}

/// Append a batch of `devices[]` entries (spec 9.1: `nwk_address` may be
/// null, `is_child` defaults to true).
pub fn add_devices(r: &mut Restore, devices: &J) -> Result<(), String> {
    let list = devices.as_array().ok_or("devices: not a list")?;
    for d in list {
        let dev_ieee: [u8; 8] = be_wire(text(d, "ieee_address")?, "ieee_address")?;
        let nwk = d.get("nwk_address").and_then(J::as_str).map(|a| be_wire::<2>(a, "nwk_address").map(u16::from_le_bytes)).transpose()?;
        let is_child = d.get("is_child").and_then(J::as_bool).unwrap_or(true);
        let link = match d.get("link_key").filter(|l| l.is_object()) {
            Some(l) => Some(BackupLinkKey {
                key: key_bytes::<16>(text(l, "key")?, "link_key")?,
                key_seq: 0,
                attrs: 2,
                origin: 3,
                tx_counter: u32_of(l, "tx_counter")?,
                rx_floor: u32_of(l, "rx_counter")?,
            }),
            None => None,
        };
        push(
            &mut *r.encoder,
            BackupItem::Device(BackupDevice {
                ieee: dev_ieee,
                nwk,
                is_child,
                // OCB has no capability: rx-off end device, 256 min (spec 9.3).
                child: is_child.then_some(BackupChild { capability: 0x80, timeout_exp: 8 }),
                link,
            }),
        )?;
        r.devices.try_reserve(1).map_err(|_| "out of memory".to_string())?;
        r.devices.push((dev_ieee, nwk));
    }
    Ok(())
}

/// Facts the export adds besides the NCP's items.
#[derive(Debug, Clone, Default)]
pub struct ExportInfo {
    /// `metadata.source` (`zigbee-rs-hub <version>`).
    pub source: String,
    /// `metadata.internal.creation_time` (RFC 3339), if the clock is set.
    pub creation_time: Option<String>,
    /// NCP firmware, chip and build ID (`metadata.internal.zigbee_rs`).
    pub ncp_fw: String,
    /// Chip ID.
    pub ncp_chip: u8,
    /// Build ID (hex).
    pub ncp_build_id: String,
    /// Devices the hub knows (IEEE in wire order, NWK) that the NCP did not
    /// export; added without keys.
    pub host_devices: Vec<([u8; 8], Option<u16>)>,
    /// The hub's groups, a JSON array (`metadata.internal.zigbee_rs.groups`);
    /// empty: none written.
    pub groups: String,
    /// The EZSP protocol version of an EmberZNet NCP
    /// (`metadata.internal.ezspVersion`, as bellows and zigbee-herdsman
    /// write it: Z2M's ember adapter loads no backup without it).
    pub ezsp_version: Option<u8>,
}

fn hex(b: &[u8]) -> String {
    b.iter().fold(String::with_capacity(2 * b.len()), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

fn be_hex(b: &[u8]) -> String {
    let mut r = b.to_vec();
    r.reverse();
    hex(&r)
}

/// JSON string literal (the producer strings are the only free text).
fn js(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Summary of an export (no keys), for logs and the UI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportSummary {
    /// Devices in the file.
    pub devices: usize,
    /// Devices with a link key.
    pub link_keys: usize,
    /// The NCP exported the keys (else the backup cannot be restored).
    pub complete: bool,
}

/// Why [`from_export`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    /// The output buffer could not be reserved.
    NoMemory,
    /// The items are not a usable export.
    Invalid(String),
}

impl From<String> for ExportError {
    fn from(e: String) -> Self {
        Self::Invalid(e)
    }
}

impl From<&str> for ExportError {
    fn from(e: &str) -> Self {
        Self::Invalid(e.into())
    }
}

/// The items of a backup export (the last of each kind, every device), as
/// the backend delivers them; [`from_export`] writes the file.
#[derive(Debug, Default)]
pub struct Export {
    node: Option<BackupItem>,
    network: Option<BackupItem>,
    nkey: Option<BackupItem>,
    tclk: Option<BackupItem>,
    rs: Option<BackupItem>,
    seed: Option<BackupItem>,
    devices: Vec<BackupDevice>,
}

impl BackupSink for Export {
    fn item(&mut self, item: &BackupItem) -> Result<(), CoordError> {
        let slot = match item {
            BackupItem::Node { .. } => &mut self.node,
            BackupItem::Network { .. } => &mut self.network,
            BackupItem::NetworkKey { .. } => &mut self.nkey,
            BackupItem::TcLinkKey { .. } => &mut self.tclk,
            BackupItem::RsParams { .. } => &mut self.rs,
            BackupItem::HashedTclkSeed(_) => &mut self.seed,
            BackupItem::Device(d) => {
                self.devices.push(*d);
                return Ok(());
            }
        };
        *slot = Some(*item);
        Ok(())
    }
}

/// Append the backup file for an export to `out` (its room is reserved
/// fallibly first).
pub fn from_export(export: Export, info: &ExportInfo, out: &mut String) -> Result<ExportSummary, ExportError> {
    let Export { node, network, nkey, tclk, rs, seed, mut devices } = export;
    let Some(BackupItem::Node { ieee: node_ieee, .. }) = node else {
        return Err("no NODE record".into());
    };
    let Some(BackupItem::Network { pan_id, ext_pan_id, channel, channel_mask, nwk_update_id, security_level, tx_power, .. }) = network else {
        return Err("no NETWORK record".into());
    };
    for &(ieee, nwk) in &info.host_devices {
        if ieee != node_ieee && !devices.iter().any(|d| d.ieee == ieee) {
            devices.push(BackupDevice { ieee, nwk, is_child: false, child: None, link: None });
        }
    }
    out.try_reserve(1200 + 230 * devices.len()).map_err(|_| ExportError::NoMemory)?;
    let coord = be_hex(&node_ieee);
    *out += "{\"metadata\":{\"format\":";
    *out += &js(FORMAT);
    *out += ",\"version\":1,\"source\":";
    *out += &js(&info.source);
    *out += ",\"internal\":{";
    if let Some(t) = &info.creation_time {
        let _ = write!(out, "\"creation_time\":{},", js(t));
    }
    let _ = write!(
        out,
        "\"zigbee_rs\":{{\"fw\":{},\"chip\":{},\"build_id\":{}{}{}}},\"node\":{{\"ieee\":\"{coord}\",\"nwk\":\"0000\",\"type\":\"coordinator\"}}",
        js(&info.ncp_fw),
        info.ncp_chip,
        js(&info.ncp_build_id),
        if info.groups.is_empty() { "" } else { ",\"groups\":" },
        info.groups
    );
    if let Some(BackupItem::TcLinkKey { key, tc_ieee, tx_counter, .. }) = &tclk {
        let _ = write!(
            out,
            ",\"network\":{{\"tc_link_key\":{{\"key\":\"{}\",\"frame_counter\":{}}},\"tc_address\":\"{}\",\"nwk_manager\":\"0000\"}}",
            hex(key),
            tx_counter,
            be_hex(tc_ieee)
        );
    }
    if let Some(p) = tx_power {
        let _ = write!(out, ",\"tx_power\":{}", p);
    }
    if let Some(v) = info.ezsp_version {
        let _ = write!(out, ",\"ezspVersion\":{v}");
    }
    if nkey.is_none() {
        *out += ",\"incomplete\":true";
    }
    *out += "}},\"stack_specific\":{";
    let mut first = true;
    if let Some(BackupItem::RsParams { fc_stride, table_epoch, nwk_fc_limit, aps_fc_limit, host_backed_tables }) = &rs {
        let _ = write!(
            out,
            "\"zigbee_rs\":{{\"version\":1,\"frame_counter_stride\":{},\"table_epoch\":{},\"nwk_frame_counter_limit\":{},\"aps_frame_counter_limit\":{},\"host_backed_tables\":{}}}",
            fc_stride,
            table_epoch,
            nwk_fc_limit,
            aps_fc_limit,
            host_backed_tables
        );
        first = false;
    }
    if let Some(BackupItem::HashedTclkSeed(s)) = &seed {
        let _ = write!(out, "{}\"ezsp\":{{\"hashed_tclk\":\"{}\"}}", if first { "" } else { "," }, hex(s));
    }
    let channels: Vec<String> = (11..=26u8).filter(|c| channel_mask & (1 << c) != 0).map(|c| c.to_string()).collect();
    let _ = write!(
        out,
        "}},\"coordinator_ieee\":\"{coord}\",\"pan_id\":\"{:04x}\",\"extended_pan_id\":\"{}\",\"nwk_update_id\":{},\"security_level\":{},\"channel\":{},\"channel_mask\":[{}],\"network_key\":",
        pan_id,
        be_hex(&ext_pan_id),
        nwk_update_id,
        security_level,
        channel,
        if channels.is_empty() { channel.to_string() } else { channels.join(",") }
    );
    match &nkey {
        Some(BackupItem::NetworkKey { key, seq, tx_counter, .. }) => {
            let _ = write!(
                out,
                "{{\"key\":\"{}\",\"sequence_number\":{},\"frame_counter\":{}}}",
                hex(key),
                seq,
                tx_counter
            );
        }
        _ => *out += "null",
    }
    *out += ",\"devices\":[";
    let mut summary = ExportSummary { devices: devices.len(), link_keys: 0, complete: nkey.is_some() };
    for (i, d) in devices.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let nwk = match d.nwk {
            Some(n) => format!("\"{n:04x}\""),
            None => "null".into(),
        };
        let _ = write!(
            out,
            "{{\"ieee_address\":\"{}\",\"nwk_address\":{nwk},\"is_child\":{}",
            be_hex(&d.ieee),
            d.is_child
        );
        if let Some(l) = &d.link {
            summary.link_keys += 1;
            let _ = write!(
                out,
                ",\"link_key\":{{\"key\":\"{}\",\"tx_counter\":{},\"rx_counter\":{}}}",
                hex(&l.key),
                l.tx_counter,
                l.rx_floor
            );
        }
        out.push('}');
    }
    *out += "]}";
    Ok(summary)
}

/// RFC 3339 UTC time of a Unix timestamp (s); `None` before 2020 (the
/// hub's clock is not set).
pub fn rfc3339(unix_s: u64) -> Option<String> {
    if unix_s < 1_577_836_800 {
        return None;
    }
    let days = (unix_s / 86_400) as i64;
    let secs = unix_s % 86_400;
    // Civil from days (H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    Some(format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+00:00", secs / 3600, secs / 60 % 60, secs % 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// zigpy's format, abbreviated from the OCB specification's example.
    const ZIGPY: &str = r#"{
      "metadata": {"version": 1, "format": "zigpy/open-coordinator-backup", "source": "zigpy-znp@0.5.1",
        "internal": {"creation_time": "2021-02-16T22:29:28+00:00",
          "network": {"tc_link_key": {"key": "5a6967426565416c6c69616e63653039", "frame_counter": 4096},
                      "tc_address": "00124b0012345678", "nwk_manager": "0000"},
          "tx_power": 4}},
      "stack_specific": {"zstack": {"tclk_seed": "abc"}},
      "coordinator_ieee": "00124b0012345678",
      "pan_id": "1a62",
      "extended_pan_id": "dddddddddddddddd",
      "nwk_update_id": 2,
      "security_level": 5,
      "channel": 15,
      "channel_mask": [15, 20, 25],
      "network_key": {"key": "37668fd64e35e03342e5ef9f35ccf4ab", "sequence_number": 1, "frame_counter": 112233},
      "devices": [
        {"nwk_address": "4d32", "ieee_address": "00158d00017c1a5b", "is_child": true,
         "link_key": {"key": "0fa2e6f9c6f6d3c5e9c8f3b2a1d0c9b8", "tx_counter": 120, "rx_counter": 6}},
        {"nwk_address": null, "ieee_address": "000b57fffe36b9a0", "is_child": false},
        {"ieee_address": "0017880100aabbcc", "nwk_address": "abc"}
      ]
    }"#;

    /// Z2M (herdsman `toUnifiedBackup`): unpadded hex, no TCLK section.
    const Z2M: &str = r#"{"metadata":{"format":"zigpy/open-coordinator-backup","version":1,"source":"zigbee-herdsman@0.50.0",
      "internal":{"date":"2024-01-01T00:00:00.000Z","znpVersion":2}},"stack_specific":{"zstack":{"tclk_seed":"00"}},
      "coordinator_ieee":"00124b00aabbccdd","pan_id":"a62","extended_pan_id":"00124b00aabbccdd","nwk_update_id":0,
      "security_level":5,"channel":11,"channel_mask":[11],
      "network_key":{"key":"01030507090b0d0f00020406080a0c0d","sequence_number":0,"frame_counter":5000},"devices":[]}"#;

    /// A restore encoder that keeps the items.
    #[derive(Clone, Default)]
    struct Items(std::sync::Arc<std::sync::Mutex<Vec<BackupItem>>>);

    impl RestoreEncoder for Items {
        fn push(&mut self, item: &BackupItem) -> Result<(), CoordError> {
            self.0.lock().unwrap().push(*item);
            Ok(())
        }

        fn finish(self: Box<Self>) -> hub_coord::RestoreBlob {
            hub_coord::RestoreBlob::default()
        }
    }

    fn import(o: &J) -> Result<(Restore, Vec<BackupItem>), String> {
        let items = Items::default();
        let r = to_restore(o, Box::new(items.clone()))?;
        let list = items.0.lock().unwrap().clone();
        Ok((r, list))
    }

    fn export(items: &[BackupItem], info: &ExportInfo) -> (ExportSummary, String) {
        let mut e = Export::default();
        for i in items {
            e.item(i).unwrap();
        }
        let mut text = String::new();
        let summary = from_export(e, info, &mut text).unwrap();
        (summary, text)
    }

    #[test]
    fn zigpy_backup_becomes_items_with_wire_byte_order() {
        let o: J = serde_json::from_str(ZIGPY).unwrap();
        let (r, items) = import(&o).unwrap();
        assert!(r.tclk_counter_known);
        assert_eq!(r.coordinator_ieee, [0x78, 0x56, 0x34, 0x12, 0x00, 0x4b, 0x12, 0x00]);
        assert_eq!((r.pan_id, r.channel), (0x1A62, 15));
        assert_eq!(items[0], BackupItem::Node { ieee: r.coordinator_ieee, ieee_override: true });
        assert!(matches!(items[1], BackupItem::Network { pan_id: 0x1A62, channel_mask, nwk_update_id: 2, tx_power: Some(4), .. } if channel_mask == (1 << 15 | 1 << 20 | 1 << 25)));
        assert!(matches!(items[2], BackupItem::NetworkKey { key, seq: 1, tx_counter: 112_233, active: true } if key[0] == 0x37 && key[15] == 0xab));
        assert!(matches!(items[3], BackupItem::TcLinkKey { key: ZIGBEE_ALLIANCE_09, tx_counter: 4096, hashed: false, .. }));
        let BackupItem::Device(d) = items[4] else { panic!() };
        assert_eq!((d.ieee[0], d.nwk), (0x5b, Some(0x4D32)));
        assert_eq!(d.child, Some(BackupChild { capability: 0x80, timeout_exp: 8 }));
        let l = d.link.unwrap();
        assert_eq!((l.key[0], l.key_seq, l.attrs, l.origin, l.tx_counter, l.rx_floor), (0x0f, 0, 2, 3, 120, 6));
        let BackupItem::Device(d) = items[5] else { panic!() };
        assert_eq!(d, BackupDevice { ieee: d.ieee, nwk: None, is_child: false, child: None, link: None }, "nwk null, not a child, no key");
        let BackupItem::Device(d) = items[6] else { panic!() };
        assert_eq!(d.nwk, Some(0x0ABC), "unpadded NWK");
        assert!(d.is_child, "is_child defaults to true");
        assert_eq!(r.devices.len(), 3);
    }

    #[test]
    fn z2m_backup_without_tclk_uses_the_default_and_the_large_margin() {
        let o: J = serde_json::from_str(Z2M).unwrap();
        let (r, items) = import(&o).unwrap();
        assert!(!r.tclk_counter_known);
        assert_eq!(r.pan_id, 0x0A62, "left-padded PAN ID");
        assert!(matches!(items[1], BackupItem::Network { tx_power: None, .. }));
        assert_eq!(items[3], BackupItem::TcLinkKey { key: ZIGBEE_ALLIANCE_09, tc_ieee: r.coordinator_ieee, tx_counter: 0, hashed: false });
    }

    #[test]
    fn invalid_backups_are_refused() {
        for (from, to) in [
            ("\"security_level\": 5", "\"security_level\": 1"),
            ("\"channel\": 15", "\"channel\": 27"),
            ("\"pan_id\": \"1a62\"", "\"pan_id\": \"ffff\""),
            ("\"37668fd64e35e03342e5ef9f35ccf4ab\"", "\"00000000000000000000000000000000\""),
            ("\"37668fd64e35e03342e5ef9f35ccf4ab\"", "\"3766\""),
            ("zigpy/open-coordinator-backup", "something/else"),
        ] {
            let o: J = serde_json::from_str(&ZIGPY.replace(from, to)).unwrap();
            assert!(import(&o).is_err(), "{to}");
        }
    }

    #[test]
    fn export_round_trips_through_import() {
        let o: J = serde_json::from_str(ZIGPY).unwrap();
        let (r, a) = import(&o).unwrap();
        let info = ExportInfo {
            source: "zigbee-rs-hub 0.1.1".into(),
            creation_time: rfc3339(1_700_000_000),
            ncp_fw: "0.1.0".into(),
            ncp_chip: 1,
            ncp_build_id: "5151515151515151".into(),
            host_devices: vec![([9; 8], Some(0x1234)), (r.devices[0].0, Some(0x4D32))],
            groups: r#"[{"id":1,"name":"Kitchen","members":[]}]"#.into(),
            ezsp_version: None,
        };
        let (summary, text) = export(&a, &info);
        assert_eq!(summary, ExportSummary { devices: 4, link_keys: 1, complete: true });
        let v: J = serde_json::from_str(&text).unwrap();
        assert_eq!(v["metadata"]["internal"]["creation_time"], "2023-11-14T22:13:20+00:00");
        assert_eq!(v["metadata"]["internal"]["zigbee_rs"]["groups"][0]["name"], "Kitchen");
        assert_eq!(v["coordinator_ieee"], "00124b0012345678");
        assert_eq!(v["pan_id"], "1a62");
        assert_eq!(v["channel_mask"], serde_json::json!([15, 20, 25]));
        assert_eq!(v["devices"][1]["nwk_address"], J::Null);
        assert_eq!(v["devices"][3]["ieee_address"], "0909090909090909");
        let back: J = serde_json::from_str(&text).unwrap();
        let (_, b) = import(&back).unwrap();
        // Same items apart from the host-only device.
        assert_eq!(&a[..], &b[..a.len()]);
        assert_eq!(b.len(), a.len() + 1);
    }

    /// What the EZSP backend exports (bellows' form; no real keys).
    fn ezsp_items() -> Vec<BackupItem> {
        let ieee = [0x01, 0x00, 0xAA, 0xFE, 0xFF, 0x57, 0x0B, 0x00];
        vec![
            BackupItem::Node { ieee, ieee_override: false },
            BackupItem::Network {
                pan_id: 0x1A62,
                ext_pan_id: [1, 2, 3, 4, 5, 6, 7, 8],
                channel: 15,
                channel_mask: 1 << 15,
                nwk_update_id: 3,
                nwk_manager: 0,
                security_level: 5,
                tx_power: Some(8),
            },
            BackupItem::NetworkKey { key: [0x11; 16], seq: 2, tx_counter: 50_000, active: true },
            BackupItem::HashedTclkSeed([0x22; 16]),
            BackupItem::TcLinkKey { key: ZIGBEE_ALLIANCE_09, tc_ieee: ieee, tx_counter: 700, hashed: true },
            BackupItem::Device(BackupDevice {
                ieee: [0x5B, 0x1A, 0x7C, 0x01, 0x00, 0x8D, 0x15, 0x00],
                nwk: Some(0x4D32),
                is_child: true,
                child: Some(BackupChild { capability: 0x80, timeout_exp: 8 }),
                link: None,
            }),
        ]
    }

    #[test]
    fn an_ezsp_export_carries_what_bellows_and_z2m_need() {
        let info = ExportInfo { source: "zigbee-rs-hub 0.2.0".into(), ezsp_version: Some(13), ..ExportInfo::default() };
        let (summary, text) = export(&ezsp_items(), &info);
        assert!(summary.complete);
        let v: J = serde_json::from_str(&text).unwrap();
        assert_eq!(v["metadata"]["internal"]["ezspVersion"], 13, "Z2M's ember adapter needs it");
        assert_eq!(v["stack_specific"]["ezsp"]["hashed_tclk"], "22".repeat(16));
        let tclk = &v["metadata"]["internal"]["network"]["tc_link_key"];
        assert_eq!(tclk["key"], "5a6967426565416c6c69616e63653039", "ZigBeeAlliance09 as bellows writes it");
        assert_eq!(tclk["frame_counter"], 700);
        assert_eq!(v["metadata"]["internal"]["network"]["tc_address"], "000b57fffeaa0001");
        assert_eq!(v["extended_pan_id"], "0807060504030201");
        assert_eq!(v["network_key"], serde_json::json!({"key": "11".repeat(16), "sequence_number": 2, "frame_counter": 50_000}));
        // And back: the same items for the backend.
        let (r, back) = import(&v).unwrap();
        assert_eq!(r.ext_pan_id, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(r.tclk_counter_known);
        for item in ezsp_items().iter().filter(|i| !matches!(i, BackupItem::Node { .. } | BackupItem::TcLinkKey { .. })) {
            assert!(back.contains(item), "{item:?}");
        }
        assert!(back.contains(&BackupItem::HashedTclkSeed([0x22; 16])));
        assert!(back.iter().any(|i| matches!(i, BackupItem::TcLinkKey { key: ZIGBEE_ALLIANCE_09, tx_counter: 700, .. })));
    }

    /// bellows (zigpy 0.7x) on EZSP 13 and zigbee-herdsman's ember adapter
    /// (`toUnifiedBackup`), abbreviated, with generated keys.
    const BELLOWS: &str = r#"{"metadata":{"version":1,"format":"zigpy/open-coordinator-backup","source":"bellows@0.44.0",
      "internal":{"creation_time":"2025-01-01T00:00:00+00:00","node":{"ieee":"000b57fffeaa0001","nwk":"0000","type":"coordinator"},
        "network":{"tc_link_key":{"key":"5a6967426565416c6c69616e63653039","frame_counter":0},"tc_address":"000b57fffeaa0001","nwk_manager":"0000"},
        "link_key_seqs":{},"tx_power":8,"ezsp":{"stack_version":13,"can_burn_userdata_custom_eui64":true,"can_rewrite_custom_eui64":false},"ezspVersion":13}},
      "stack_specific":{"ezsp":{"hashed_tclk":"33333333333333333333333333333333"}},
      "coordinator_ieee":"000b57fffeaa0001","pan_id":"1a62","extended_pan_id":"0807060504030201","nwk_update_id":3,"security_level":5,
      "channel":15,"channel_mask":[11,15,20,25],"network_key":{"key":"44444444444444444444444444444444","sequence_number":0,"frame_counter":81920},
      "devices":[{"ieee_address":"00158d00017c1a5b","nwk_address":"4d32","is_child":true}]}"#;

    const Z2M_EMBER: &str = r#"{"metadata":{"format":"zigpy/open-coordinator-backup","version":1,"source":"zigbee-herdsman@3.2.0",
      "internal":{"date":"2025-01-01T00:00:00.000Z","ezspVersion":13}},"stack_specific":{"ezsp":{"hashed_tclk":"55555555555555555555555555555555"}},
      "coordinator_ieee":"000b57fffeaa0001","pan_id":"1a62","extended_pan_id":"0807060504030201","nwk_update_id":0,"security_level":5,
      "channel":15,"channel_mask":[15],"network_key":{"key":"66666666666666666666666666666666","sequence_number":0,"frame_counter":4096},"devices":[]}"#;

    /// One network (generated values), as zigpy writes it and as
    /// zigbee-herdsman (Z2M) writes it for Z-Stack and for EmberZNet:
    /// `internal.date`, unpadded PAN ID and NWK addresses, `is_child` left
    /// out, no TCLK section, the stack's own `stack_specific`.
    const SAME_ZIGPY: &str = r#"{"metadata":{"version":1,"format":"zigpy/open-coordinator-backup","source":"zigpy-znp@0.13.0",
      "internal":{"creation_time":"2025-03-01T10:00:00+00:00","node":{"ieee":"00124b00c0ffee01","nwk":"0000","type":"coordinator"},
        "network":{"tc_link_key":{"key":"5a6967426565416c6c69616e63653039","frame_counter":0},"tc_address":"00124b00c0ffee01","nwk_manager":"0000"}}},
      "stack_specific":{"zstack":{"tclk_seed":"77777777777777777777777777777777"}},
      "coordinator_ieee":"00124b00c0ffee01","pan_id":"0f3a","extended_pan_id":"a1b2c3d4e5f60718","nwk_update_id":4,"security_level":5,
      "channel":20,"channel_mask":[20],"network_key":{"key":"8899aabbccddeeff0011223344556677","sequence_number":3,"frame_counter":123456},
      "devices":[{"ieee_address":"00158d0003c4d5e6","nwk_address":"04b2","is_child":true,
                  "link_key":{"key":"0123456789abcdef0123456789abcdef","tx_counter":12,"rx_counter":34}},
                 {"ieee_address":"0017880100f1e2d3","nwk_address":"7a10","is_child":false}]}"#;

    const SAME_Z2M_ZSTACK: &str = r#"{"metadata":{"format":"zigpy/open-coordinator-backup","version":1,"source":"zigbee-herdsman@3.2.0",
      "internal":{"date":"2025-03-01T10:00:00.000Z","znpVersion":2}},"stack_specific":{"zstack":{"tclk_seed":"77777777777777777777777777777777"}},
      "coordinator_ieee":"00124b00c0ffee01","pan_id":"f3a","extended_pan_id":"a1b2c3d4e5f60718","nwk_update_id":4,"security_level":5,
      "channel":20,"channel_mask":[20],"network_key":{"key":"8899aabbccddeeff0011223344556677","sequence_number":3,"frame_counter":123456},
      "devices":[{"nwk_address":"4b2","ieee_address":"00158d0003c4d5e6",
                  "link_key":{"key":"0123456789abcdef0123456789abcdef","rx_counter":34,"tx_counter":12}},
                 {"nwk_address":"7a10","ieee_address":"0017880100f1e2d3","is_child":false}]}"#;

    const SAME_Z2M_EMBER: &str = r#"{"metadata":{"format":"zigpy/open-coordinator-backup","version":1,"source":"zigbee-herdsman@3.2.0",
      "internal":{"date":"2025-03-01T10:00:00.000Z","ezspVersion":13}},"stack_specific":{"ezsp":{"hashed_tclk":"88888888888888888888888888888888"}},
      "coordinator_ieee":"00124b00c0ffee01","pan_id":"f3a","extended_pan_id":"a1b2c3d4e5f60718","nwk_update_id":4,"security_level":5,
      "channel":20,"channel_mask":[20],"network_key":{"key":"8899aabbccddeeff0011223344556677","sequence_number":3,"frame_counter":123456},
      "devices":[{"nwk_address":"4b2","ieee_address":"00158d0003c4d5e6",
                  "link_key":{"key":"0123456789abcdef0123456789abcdef","rx_counter":34,"tx_counter":12}},
                 {"nwk_address":"7a10","ieee_address":"0017880100f1e2d3","is_child":false}]}"#;

    /// zigpy reads a Z2M file as its own (its `test_z2m_backup_parsing`):
    /// the same EPID, IEEEs, addresses, keys and devices, no byte swap.
    #[test]
    fn a_network_reads_the_same_from_zigpy_and_z2m_files() {
        let (zr, zigpy) = import(&serde_json::from_str(SAME_ZIGPY).unwrap()).unwrap();
        // Big-endian hex, reversed into wire order, in every form.
        assert_eq!(zr.ext_pan_id, [0x18, 0x07, 0xF6, 0xE5, 0xD4, 0xC3, 0xB2, 0xA1]);
        assert_eq!(zr.coordinator_ieee, [0x01, 0xEE, 0xFF, 0xC0, 0x00, 0x4B, 0x12, 0x00]);
        assert_eq!((zr.pan_id, zr.channel), (0x0F3A, 20));
        assert_eq!(zr.devices, [([0xE6, 0xD5, 0xC4, 0x03, 0x00, 0x8D, 0x15, 0x00], Some(0x04B2)), ([0xD3, 0xE2, 0xF1, 0x00, 0x01, 0x88, 0x17, 0x00], Some(0x7A10))]);
        // What differs between the forms: the TCLK section (Z2M has none:
        // the default key, the coordinator as the trust center, counter
        // unknown) and the stack's seed.
        let common = |items: &[BackupItem]| -> Vec<BackupItem> {
            items.iter().filter(|i| !matches!(i, BackupItem::TcLinkKey { .. } | BackupItem::HashedTclkSeed(_))).copied().collect()
        };
        for (form, text) in [("zstack", SAME_Z2M_ZSTACK), ("ember", SAME_Z2M_EMBER)] {
            let (r, z2m) = import(&serde_json::from_str(text).unwrap()).unwrap();
            assert_eq!((r.ext_pan_id, r.coordinator_ieee, r.pan_id, r.channel), (zr.ext_pan_id, zr.coordinator_ieee, zr.pan_id, zr.channel), "{form}");
            assert_eq!(r.devices, zr.devices, "{form}");
            assert_eq!(common(&z2m), common(&zigpy), "{form}");
            assert!(!r.tclk_counter_known && zr.tclk_counter_known, "{form}");
            assert!(z2m.contains(&BackupItem::TcLinkKey { key: ZIGBEE_ALLIANCE_09, tc_ieee: zr.coordinator_ieee, tx_counter: 0, hashed: false }), "{form}");
        }
        let (_, ember) = import(&serde_json::from_str(SAME_Z2M_EMBER).unwrap()).unwrap();
        assert!(ember.contains(&BackupItem::HashedTclkSeed([0x88; 16])));
        assert!(!zigpy.iter().any(|i| matches!(i, BackupItem::HashedTclkSeed(_))), "a Z-Stack seed is not EmberZNet's");
    }

    #[test]
    fn bellows_and_z2m_ember_backups_bring_their_hashed_link_key_root() {
        let (r, items) = import(&serde_json::from_str(BELLOWS).unwrap()).unwrap();
        assert!(r.tclk_counter_known, "bellows writes a TCLK frame counter");
        assert!(items.contains(&BackupItem::HashedTclkSeed([0x33; 16])));
        assert!(items.contains(&BackupItem::NetworkKey { key: [0x44; 16], seq: 0, tx_counter: 81_920, active: true }));
        assert!(matches!(items[1], BackupItem::Network { nwk_update_id: 3, tx_power: Some(8), .. }));
        let (r, items) = import(&serde_json::from_str(Z2M_EMBER).unwrap()).unwrap();
        assert!(!r.tclk_counter_known, "no TCLK section: the large APS margin");
        assert!(items.contains(&BackupItem::HashedTclkSeed([0x55; 16])));
        assert_eq!((r.pan_id, r.ext_pan_id), (0x1A62, [1, 2, 3, 4, 5, 6, 7, 8]));
    }

    #[test]
    fn export_without_keys_is_marked_incomplete() {
        let o: J = serde_json::from_str(ZIGPY).unwrap();
        let (_, items) = import(&o).unwrap();
        let items: Vec<BackupItem> =
            items.into_iter().filter(|i| !matches!(i, BackupItem::NetworkKey { .. } | BackupItem::TcLinkKey { .. })).collect();
        let (summary, text) = export(&items, &ExportInfo::default());
        assert!(!summary.complete);
        let v: J = serde_json::from_str(&text).unwrap();
        assert_eq!(v["network_key"], J::Null);
        assert_eq!(v["metadata"]["internal"]["incomplete"], true);
        assert!(import(&serde_json::from_str::<J>(&text).unwrap()).is_err());
    }

    #[test]
    fn export_needs_the_node_and_the_network() {
        let o: J = serde_json::from_str(ZIGPY).unwrap();
        let (_, items) = import(&o).unwrap();
        for (skip, why) in [(0, "no NODE record"), (1, "no NETWORK record")] {
            let mut e = Export::default();
            for (_, i) in items.iter().enumerate().filter(|(n, _)| *n != skip) {
                e.item(i).unwrap();
            }
            let r = from_export(e, &ExportInfo::default(), &mut String::new());
            assert_eq!(r, Err(ExportError::Invalid(why.into())));
        }
    }
}
