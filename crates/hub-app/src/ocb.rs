//! Open ZigBee Coordinator Backup (`zigpy/open-coordinator-backup` v1, the
//! format of zigpy/ZHA and zigbee-herdsman/Z2M; ZRHP spec 9.1/9.2) ⇄ ZRHP
//! record streams (spec 9.3).
//!
//! Encoding rules: IEEE, PAN ID, EPID and NWK addresses are big-endian hex
//! (reversed from the wire), keys are hex in key-byte order. Import
//! tolerates the known quirks: Z2M's unpadded PAN ID and NWK addresses,
//! `internal.date` instead of `creation_time`, no TCLK section (the default
//! `ZigBeeAlliance09` and an APS margin of 2²⁰, spec 9.4 rule 6).
//!
//! The export is written as text directly (no JSON tree): a backup of a
//! hundred devices would otherwise need tens of kilobytes of heap on a hub
//! without PSRAM. Nothing here logs a key.

use std::fmt::Write as _;

use serde::Deserialize;
use serde_json::Value as J;
use zigbee_zrhp::codec::{Reader, Writer};
use zigbee_zrhp::records::{
    ChildInfo, DeviceRecord, LinkBlock, NetworkKeyRecord, NetworkRecord, NodeRecord, RawRecords, Record, RsParamsRecord,
    StreamCrc, TcLinkKeyRecord, device_flags, record_type,
};
use zigbee_zrhp::types::Ieee;

/// `metadata.format`.
pub const FORMAT: &str = "zigpy/open-coordinator-backup";
/// The global default Trust Center link key.
pub const ZIGBEE_ALLIANCE_09: [u8; 16] = *b"ZigBeeAlliance09";
/// APS margin when the backup has no TCLK frame counter (spec 9.4 rule 6).
pub const UNKNOWN_APS_MARGIN: u32 = 1 << 20;

/// A backup file (fields the hub uses; others are ignored).
#[derive(Debug, Clone, Deserialize)]
pub struct Ocb {
    /// `format`, `version`, `source`, `internal`.
    pub metadata: Metadata,
    /// Coordinator IEEE (big-endian hex).
    pub coordinator_ieee: String,
    /// PAN ID (big-endian hex, maybe unpadded).
    pub pan_id: String,
    /// Extended PAN ID (big-endian hex).
    pub extended_pan_id: String,
    /// NWK update ID.
    #[serde(default)]
    pub nwk_update_id: u8,
    /// Must be 5.
    #[serde(default = "five")]
    pub security_level: u8,
    /// Channel.
    pub channel: u8,
    /// Channel numbers.
    #[serde(default)]
    pub channel_mask: Vec<u8>,
    /// Network key.
    pub network_key: Option<NetworkKey>,
    /// Devices.
    #[serde(default)]
    pub devices: Vec<Device>,
    /// Stack-specific sections (`zigbee_rs`, `ezsp`, …).
    #[serde(default)]
    pub stack_specific: Option<J>,
}

fn five() -> u8 {
    5
}

fn yes() -> bool {
    true
}

/// `metadata`.
#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    /// [`FORMAT`].
    pub format: String,
    /// 1.
    pub version: u32,
    /// Producer.
    #[serde(default)]
    pub source: Option<String>,
    /// Producer-specific facts (zigpy: `network.tc_link_key`, `tx_power`).
    #[serde(default)]
    pub internal: Option<J>,
}

/// `network_key`.
#[derive(Debug, Clone, Deserialize)]
pub struct NetworkKey {
    /// Hex, key-byte order.
    pub key: String,
    /// Key sequence number.
    #[serde(default)]
    pub sequence_number: u8,
    /// Outgoing NWK frame counter.
    #[serde(default)]
    pub frame_counter: u32,
}

/// `devices[]`.
#[derive(Debug, Clone, Deserialize)]
pub struct Device {
    /// Big-endian hex.
    pub ieee_address: String,
    /// Big-endian hex or null.
    #[serde(default)]
    pub nwk_address: Option<String>,
    /// Defaults to true (spec).
    #[serde(default = "yes")]
    pub is_child: bool,
    /// Unique TCLK.
    #[serde(default)]
    pub link_key: Option<LinkKey>,
}

/// `devices[].link_key`.
#[derive(Debug, Clone, Deserialize)]
pub struct LinkKey {
    /// Hex, key-byte order.
    pub key: String,
    /// Outgoing APS counter.
    #[serde(default)]
    pub tx_counter: u32,
    /// Incoming APS counter.
    #[serde(default)]
    pub rx_counter: u32,
}

/// A backup converted for `BKP_RESTORE_*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restore {
    /// Records (no `END`; the host core adds it).
    pub stream: Vec<u8>,
    /// The backup carries a TCLK frame counter (else use
    /// [`UNKNOWN_APS_MARGIN`]).
    pub tclk_counter_known: bool,
    /// Coordinator IEEE (wire order).
    pub coordinator_ieee: [u8; 8],
    /// PAN ID.
    pub pan_id: u16,
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

/// Validate a backup and convert it into restore records (spec 9.2/9.3).
pub fn to_records(o: &Ocb) -> Result<Restore, String> {
    if o.metadata.format != FORMAT || o.metadata.version != 1 {
        return Err(format!("not an open coordinator backup v1 ({} v{})", o.metadata.format, o.metadata.version));
    }
    if o.security_level != 5 {
        return Err(format!("security level {} (only 5 is supported)", o.security_level));
    }
    if !(11..=26).contains(&o.channel) {
        return Err(format!("channel {} outside 11–26", o.channel));
    }
    let ieee: [u8; 8] = be_wire(&o.coordinator_ieee, "coordinator_ieee")?;
    let pan_id = u16::from_le_bytes(be_wire(&o.pan_id, "pan_id")?);
    if pan_id == 0 || pan_id == 0xFFFF {
        return Err(format!("PAN ID 0x{pan_id:04X}"));
    }
    let epid: [u8; 8] = be_wire(&o.extended_pan_id, "extended_pan_id")?;
    if epid == [0; 8] || epid == [0xFF; 8] {
        return Err("extended PAN ID all zero or all ones".into());
    }
    let nk = o.network_key.as_ref().ok_or("the backup has no network key (incomplete backup)")?;
    let key: [u8; 16] = key_bytes(&nk.key, "network_key")?;
    if key == [0; 16] {
        return Err("all-zero network key (incomplete backup)".into());
    }
    let internal = o.metadata.internal.as_ref();
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
    let tx_power = path(internal, &["tx_power"]).and_then(J::as_i64).map_or(0x7F, |p| p.clamp(-128, 127) as i8);
    let mask = o.channel_mask.iter().filter(|c| (11..=26).contains(*c)).fold(0u32, |m, c| m | 1 << c);

    let mut records = vec![
        Record::Node(NodeRecord { ieee: Ieee(ieee), nwk: 0, logical_type: 0, flags: 1 }),
        Record::Network(NetworkRecord {
            pan_id,
            ext_pan_id: epid,
            channel: o.channel,
            channel_mask: if mask == 0 { 1 << o.channel } else { mask },
            nwk_update_id: o.nwk_update_id,
            nwk_manager: 0,
            security_level: 5,
            tx_power,
        }),
        Record::NetworkKey(NetworkKeyRecord { key, seq: nk.sequence_number, tx_counter: nk.frame_counter, flags: 1 }),
        Record::TcLinkKey(TcLinkKeyRecord {
            key: tclk_key,
            tc_ieee: Ieee(tc_ieee),
            tx_counter: tclk_counter.map_or(0, |c| c.min(u64::from(u32::MAX)) as u32),
            flags: 0,
        }),
    ];
    let ss = o.stack_specific.as_ref();
    let seed = path(ss, &["ezsp", "hashed_tclk"]).and_then(J::as_str).map(|s| key_bytes::<16>(s, "hashed_tclk")).transpose()?;
    if let Some(seed) = seed {
        records.push(Record::EzspHashedTclk(seed));
    }
    let mut devices = Vec::new();
    for d in &o.devices {
        let dev_ieee: [u8; 8] = be_wire(&d.ieee_address, "ieee_address")?;
        let nwk = d.nwk_address.as_deref().map(|a| be_wire::<2>(a, "nwk_address").map(u16::from_le_bytes)).transpose()?;
        let link = d
            .link_key
            .as_ref()
            .map(|l| {
                key_bytes::<16>(&l.key, "link_key").map(|key| LinkBlock {
                    key,
                    key_seq: 0,
                    attrs: 2,
                    origin: 3,
                    tx_counter: l.tx_counter,
                    rx_floor: l.rx_counter,
                })
            })
            .transpose()?;
        let mut flags = 0;
        if nwk.is_some() {
            flags |= device_flags::NWK_VALID;
        }
        if d.is_child {
            flags |= device_flags::IS_CHILD | device_flags::CHILD_INFO;
        }
        if link.is_some() {
            flags |= device_flags::LINK;
        }
        records.push(Record::Device(DeviceRecord {
            ieee: Ieee(dev_ieee),
            nwk: nwk.unwrap_or(0xFFFF),
            flags,
            // OCB has no capability: rx-off end device, 256 min (spec 9.3).
            child: d.is_child.then_some(ChildInfo { capability: 0x80, timeout_exp: 8 }),
            link,
        }));
        devices.push((dev_ieee, nwk));
    }
    if let Some(rs) = path(ss, &["zigbee_rs"]) {
        let num = |k: &str| rs.get(k).and_then(J::as_u64).map_or(0, |v| v.min(u64::from(u32::MAX)) as u32);
        records.push(Record::RsParams(RsParamsRecord {
            fc_stride: num("frame_counter_stride"),
            table_epoch: num("table_epoch"),
            nwk_fc_limit: num("nwk_frame_counter_limit"),
            aps_fc_limit: num("aps_frame_counter_limit"),
            flags: u8::from(rs.get("host_backed_tables").and_then(J::as_bool).unwrap_or(false)),
        }));
    }
    let mut buf = vec![0u8; 64 + 48 * records.len()];
    let mut w = Writer::new(&mut buf);
    let mut crc = StreamCrc::new();
    for r in &records {
        crc.write_record(r, &mut w).map_err(|e| format!("encoding: {e:?}"))?;
    }
    let n = w.len();
    buf.truncate(n);
    Ok(Restore { stream: buf, tclk_counter_known: tclk_counter.is_some(), coordinator_ieee: ieee, pan_id, channel: o.channel, devices })
}

/// Facts the export adds besides the NCP's records.
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

/// Why [`from_records`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    /// The output buffer could not be reserved.
    NoMemory,
    /// The record stream is not a usable export.
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

/// Append the backup file for a record stream from `BKP_EXPORT_*` to
/// `out` (its room is reserved fallibly first).
pub fn from_records(stream: &[u8], info: &ExportInfo, out: &mut String) -> Result<ExportSummary, ExportError> {
    let mut node = None;
    let mut network = None;
    let mut nkey = None;
    let mut tclk = None;
    let mut rs = None;
    let mut seed = None;
    let mut devices: Vec<DeviceRecord> = Vec::new();
    for item in RawRecords::new(stream) {
        let (rtype, tlv) = item.map_err(|e| format!("record stream: {e:?}"))?;
        if rtype == record_type::END {
            break;
        }
        match Reader::new(tlv).read::<Record>() {
            Ok(Record::Node(n)) => node = Some(n),
            Ok(Record::Network(n)) => network = Some(n),
            Ok(Record::NetworkKey(k)) => nkey = Some(k),
            Ok(Record::TcLinkKey(k)) => tclk = Some(k),
            Ok(Record::RsParams(p)) => rs = Some(p),
            Ok(Record::EzspHashedTclk(s)) => seed = Some(s),
            Ok(Record::Device(d)) => devices.push(d),
            Ok(_) => {}
            Err(e) => return Err(format!("record: {e:?}").into()),
        }
    }
    let node = node.ok_or("no NODE record")?;
    let network = network.ok_or("no NETWORK record")?;
    for &(ieee, nwk) in &info.host_devices {
        if ieee != node.ieee.0 && !devices.iter().any(|d| d.ieee.0 == ieee) {
            devices.push(DeviceRecord {
                ieee: Ieee(ieee),
                nwk: nwk.unwrap_or(0xFFFF),
                flags: if nwk.is_some() { device_flags::NWK_VALID } else { 0 },
                child: None,
                link: None,
            });
        }
    }
    out.try_reserve(1200 + 230 * devices.len()).map_err(|_| ExportError::NoMemory)?;
    let coord = be_hex(&node.ieee.0);
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
        "\"zigbee_rs\":{{\"fw\":{},\"chip\":{},\"build_id\":{}}},\"node\":{{\"ieee\":\"{coord}\",\"nwk\":\"0000\",\"type\":\"coordinator\"}}",
        js(&info.ncp_fw),
        info.ncp_chip,
        js(&info.ncp_build_id)
    );
    if let Some(t) = &tclk {
        let _ = write!(
            out,
            ",\"network\":{{\"tc_link_key\":{{\"key\":\"{}\",\"frame_counter\":{}}},\"tc_address\":\"{}\",\"nwk_manager\":\"0000\"}}",
            hex(&t.key),
            t.tx_counter,
            be_hex(&t.tc_ieee.0)
        );
    }
    if network.tx_power != 0x7F {
        let _ = write!(out, ",\"tx_power\":{}", network.tx_power);
    }
    if nkey.is_none() {
        *out += ",\"incomplete\":true";
    }
    *out += "}},\"stack_specific\":{";
    let mut first = true;
    if let Some(p) = &rs {
        let _ = write!(
            out,
            "\"zigbee_rs\":{{\"version\":1,\"frame_counter_stride\":{},\"table_epoch\":{},\"nwk_frame_counter_limit\":{},\"aps_frame_counter_limit\":{},\"host_backed_tables\":{}}}",
            p.fc_stride,
            p.table_epoch,
            p.nwk_fc_limit,
            p.aps_fc_limit,
            p.flags & 1 != 0
        );
        first = false;
    }
    if let Some(s) = &seed {
        let _ = write!(out, "{}\"ezsp\":{{\"hashed_tclk\":\"{}\"}}", if first { "" } else { "," }, hex(s));
    }
    let channels: Vec<String> = (11..=26u8).filter(|c| network.channel_mask & (1 << c) != 0).map(|c| c.to_string()).collect();
    let _ = write!(
        out,
        "}},\"coordinator_ieee\":\"{coord}\",\"pan_id\":\"{:04x}\",\"extended_pan_id\":\"{}\",\"nwk_update_id\":{},\"security_level\":{},\"channel\":{},\"channel_mask\":[{}],\"network_key\":",
        network.pan_id,
        be_hex(&network.ext_pan_id),
        network.nwk_update_id,
        network.security_level,
        network.channel,
        if channels.is_empty() { network.channel.to_string() } else { channels.join(",") }
    );
    match &nkey {
        Some(k) => {
            let _ = write!(
                out,
                "{{\"key\":\"{}\",\"sequence_number\":{},\"frame_counter\":{}}}",
                hex(&k.key),
                k.seq,
                k.tx_counter
            );
        }
        None => *out += "null",
    }
    *out += ",\"devices\":[";
    let mut summary = ExportSummary { devices: devices.len(), link_keys: 0, complete: nkey.is_some() };
    for (i, d) in devices.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let nwk = if d.flags & device_flags::NWK_VALID != 0 { format!("\"{:04x}\"", d.nwk) } else { "null".into() };
        let _ = write!(
            out,
            "{{\"ieee_address\":\"{}\",\"nwk_address\":{nwk},\"is_child\":{}",
            be_hex(&d.ieee.0),
            d.flags & device_flags::IS_CHILD != 0
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

    fn records(stream: &[u8]) -> Vec<Record<'_>> {
        RawRecords::new(stream).flatten().map(|(_, t)| Reader::new(t).read::<Record>().unwrap()).collect()
    }

    #[test]
    fn zigpy_backup_becomes_records_with_wire_byte_order() {
        let o: Ocb = serde_json::from_str(ZIGPY).unwrap();
        let r = to_records(&o).unwrap();
        assert!(r.tclk_counter_known);
        assert_eq!(r.coordinator_ieee, [0x78, 0x56, 0x34, 0x12, 0x00, 0x4b, 0x12, 0x00]);
        assert_eq!((r.pan_id, r.channel), (0x1A62, 15));
        let recs = records(&r.stream);
        assert!(matches!(&recs[1], Record::Network(n) if n.pan_id == 0x1A62 && n.channel_mask == (1 << 15 | 1 << 20 | 1 << 25) && n.nwk_update_id == 2 && n.tx_power == 4));
        assert!(matches!(&recs[2], Record::NetworkKey(k) if k.key[0] == 0x37 && k.key[15] == 0xab && k.seq == 1 && k.tx_counter == 112_233));
        assert!(matches!(&recs[3], Record::TcLinkKey(k) if k.key == ZIGBEE_ALLIANCE_09 && k.tx_counter == 4096));
        let Record::Device(d) = &recs[4] else { panic!() };
        assert_eq!((d.ieee.0[0], d.nwk), (0x5b, 0x4D32));
        assert_eq!(d.flags, device_flags::NWK_VALID | device_flags::IS_CHILD | device_flags::CHILD_INFO | device_flags::LINK);
        assert_eq!(d.link.as_ref().unwrap().rx_floor, 6);
        let Record::Device(d) = &recs[5] else { panic!() };
        assert_eq!((d.flags, d.nwk), (0, 0xFFFF), "nwk null, not a child, no key");
        let Record::Device(d) = &recs[6] else { panic!() };
        assert_eq!(d.nwk, 0x0ABC, "unpadded NWK, is_child defaults to true");
        assert!(d.flags & device_flags::IS_CHILD != 0);
        assert_eq!(r.devices.len(), 3);
    }

    #[test]
    fn z2m_backup_without_tclk_uses_the_default_and_the_large_margin() {
        let o: Ocb = serde_json::from_str(Z2M).unwrap();
        let r = to_records(&o).unwrap();
        assert!(!r.tclk_counter_known);
        assert_eq!(r.pan_id, 0x0A62, "left-padded PAN ID");
        let recs = records(&r.stream);
        assert!(matches!(&recs[3], Record::TcLinkKey(k) if k.key == ZIGBEE_ALLIANCE_09 && k.tx_counter == 0 && k.tc_ieee.0 == r.coordinator_ieee));
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
            let o: Ocb = serde_json::from_str(&ZIGPY.replace(from, to)).unwrap();
            assert!(to_records(&o).is_err(), "{to}");
        }
    }

    #[test]
    fn export_round_trips_through_import() {
        let o: Ocb = serde_json::from_str(ZIGPY).unwrap();
        let r = to_records(&o).unwrap();
        let info = ExportInfo {
            source: "zigbee-rs-hub 0.1.1".into(),
            creation_time: rfc3339(1_700_000_000),
            ncp_fw: "0.1.0".into(),
            ncp_chip: 1,
            ncp_build_id: "5151515151515151".into(),
            host_devices: vec![([9; 8], Some(0x1234)), (r.devices[0].0, Some(0x4D32))],
        };
        let mut text = String::new();
        let summary = from_records(&r.stream, &info, &mut text).unwrap();
        assert_eq!(summary, ExportSummary { devices: 4, link_keys: 1, complete: true });
        let v: J = serde_json::from_str(&text).unwrap();
        assert_eq!(v["metadata"]["internal"]["creation_time"], "2023-11-14T22:13:20+00:00");
        assert_eq!(v["coordinator_ieee"], "00124b0012345678");
        assert_eq!(v["pan_id"], "1a62");
        assert_eq!(v["channel_mask"], serde_json::json!([15, 20, 25]));
        assert_eq!(v["devices"][1]["nwk_address"], J::Null);
        assert_eq!(v["devices"][3]["ieee_address"], "0909090909090909");
        let back: Ocb = serde_json::from_str(&text).unwrap();
        let r2 = to_records(&back).unwrap();
        // Same records apart from the host-only device.
        let a = records(&r.stream);
        let b = records(&r2.stream);
        assert_eq!(&a[..], &b[..a.len()]);
        assert_eq!(b.len(), a.len() + 1);
    }

    #[test]
    fn export_without_keys_is_marked_incomplete() {
        let o: Ocb = serde_json::from_str(ZIGPY).unwrap();
        let r = to_records(&o).unwrap();
        let stream: Vec<u8> = RawRecords::new(&r.stream)
            .flatten()
            .filter(|(t, _)| *t != record_type::NETWORK_KEY && *t != record_type::TC_LINK_KEY)
            .flat_map(|(_, tlv)| tlv.to_vec())
            .collect();
        let mut text = String::new();
        let summary = from_records(&stream, &ExportInfo::default(), &mut text).unwrap();
        assert!(!summary.complete);
        let v: J = serde_json::from_str(&text).unwrap();
        assert_eq!(v["network_key"], J::Null);
        assert_eq!(v["metadata"]["internal"]["incomplete"], true);
        assert!(to_records(&serde_json::from_str(&text).unwrap()).is_err());
    }
}
