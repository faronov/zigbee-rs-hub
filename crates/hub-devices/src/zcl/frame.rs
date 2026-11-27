//! ZCL frames (ZCL r8 2.4) and the foundation commands the hub sends and
//! receives (2.5): Read Attributes, Write Attributes, Configure Reporting,
//! Read Reporting Configuration, Report Attributes, Default Response.

use super::types::{CodecError, Reader, TypedValue, ZclType, ZclValue, decode_value, encode_value};

/// Frame type bits 0-1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FrameType {
    /// Foundation (profile-wide) command.
    Global,
    /// Cluster-specific command.
    Cluster,
}

/// Direction bit 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Direction {
    /// Client to server (0).
    ClientToServer,
    /// Server to client (1).
    ServerToClient,
}

/// ZCL header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ZclHeader {
    /// Frame type.
    pub frame_type: FrameType,
    /// Manufacturer code (bit 2).
    pub mfr: Option<u16>,
    /// Direction.
    pub direction: Direction,
    /// Disable Default Response (bit 4).
    pub disable_default_response: bool,
    /// Transaction sequence number.
    pub tsn: u8,
    /// Command identifier.
    pub cmd: u8,
}

/// A ZCL frame with its payload.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ZclFrame {
    /// Header.
    pub header: ZclHeader,
    /// Payload after the header.
    pub payload: Vec<u8>,
}

impl ZclFrame {
    /// Parses a frame.
    pub fn parse(data: &[u8]) -> Result<Self, CodecError> {
        let mut r = Reader::new(data);
        let fc = r.u8()?;
        let frame_type = match fc & 0x03 {
            0 => FrameType::Global,
            1 => FrameType::Cluster,
            _ => return Err(CodecError::Invalid),
        };
        let mfr = if fc & 0x04 != 0 { Some(r.u16()?) } else { None };
        let tsn = r.u8()?;
        let cmd = r.u8()?;
        Ok(Self {
            header: ZclHeader {
                frame_type,
                mfr,
                direction: if fc & 0x08 != 0 {
                    Direction::ServerToClient
                } else {
                    Direction::ClientToServer
                },
                disable_default_response: fc & 0x10 != 0,
                tsn,
                cmd,
            },
            payload: r.rest().to_vec(),
        })
    }

    /// Serializes the frame.
    pub fn encode(&self) -> Vec<u8> {
        let h = &self.header;
        let mut fc = match h.frame_type {
            FrameType::Global => 0,
            FrameType::Cluster => 1,
        };
        if h.mfr.is_some() {
            fc |= 0x04;
        }
        if h.direction == Direction::ServerToClient {
            fc |= 0x08;
        }
        if h.disable_default_response {
            fc |= 0x10;
        }
        let mut out = Vec::with_capacity(5 + self.payload.len());
        out.push(fc);
        if let Some(m) = h.mfr {
            out.extend_from_slice(&m.to_le_bytes());
        }
        out.push(h.tsn);
        out.push(h.cmd);
        out.extend_from_slice(&self.payload);
        out
    }

    /// A client-to-server command from the hub.
    pub fn command(
        frame_type: FrameType,
        tsn: u8,
        cmd: u8,
        mfr: Option<u16>,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            header: ZclHeader {
                frame_type,
                mfr,
                direction: Direction::ClientToServer,
                disable_default_response: false,
                tsn,
                cmd,
            },
            payload,
        }
    }
}

/// Foundation command identifiers (ZCL r8 Table 2-3).
#[allow(missing_docs)]
pub mod global {
    pub const READ_ATTRIBUTES: u8 = 0x00;
    pub const READ_ATTRIBUTES_RSP: u8 = 0x01;
    pub const WRITE_ATTRIBUTES: u8 = 0x02;
    pub const WRITE_ATTRIBUTES_UNDIVIDED: u8 = 0x03;
    pub const WRITE_ATTRIBUTES_RSP: u8 = 0x04;
    pub const WRITE_ATTRIBUTES_NO_RSP: u8 = 0x05;
    pub const CONFIGURE_REPORTING: u8 = 0x06;
    pub const CONFIGURE_REPORTING_RSP: u8 = 0x07;
    pub const READ_REPORTING_CONFIG: u8 = 0x08;
    pub const READ_REPORTING_CONFIG_RSP: u8 = 0x09;
    pub const REPORT_ATTRIBUTES: u8 = 0x0A;
    pub const DEFAULT_RSP: u8 = 0x0B;
}

/// ZCL status codes (ZCL r8 Table 2-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Status(pub u8);

#[allow(missing_docs)]
impl Status {
    pub const SUCCESS: Self = Self(0x00);
    pub const FAILURE: Self = Self(0x01);
    pub const NOT_AUTHORIZED: Self = Self(0x7E);
    pub const MALFORMED_COMMAND: Self = Self(0x80);
    pub const UNSUP_COMMAND: Self = Self(0x81);
    pub const UNSUP_GENERAL_COMMAND: Self = Self(0x82);
    pub const UNSUP_MANUF_CLUSTER_COMMAND: Self = Self(0x83);
    pub const UNSUP_MANUF_GENERAL_COMMAND: Self = Self(0x84);
    pub const INVALID_FIELD: Self = Self(0x85);
    pub const UNSUPPORTED_ATTRIBUTE: Self = Self(0x86);
    pub const INVALID_VALUE: Self = Self(0x87);
    pub const READ_ONLY: Self = Self(0x88);
    pub const INSUFFICIENT_SPACE: Self = Self(0x89);
    pub const NOT_FOUND: Self = Self(0x8B);
    pub const UNREPORTABLE_ATTRIBUTE: Self = Self(0x8C);
    pub const INVALID_DATA_TYPE: Self = Self(0x8D);
    pub const INVALID_SELECTOR: Self = Self(0x8E);
    pub const TIMEOUT: Self = Self(0x94);
    pub const ABORT: Self = Self(0x95);
    pub const UNSUPPORTED_CLUSTER: Self = Self(0xC3);

    /// The ZCL name (`UNSUPPORTED_ATTRIBUTE`), as used in API errors (14.1).
    pub fn name(self) -> &'static str {
        match self.0 {
            0x00 => "SUCCESS",
            0x01 => "FAILURE",
            0x7E => "NOT_AUTHORIZED",
            0x80 => "MALFORMED_COMMAND",
            0x81 => "UNSUP_COMMAND",
            0x82 => "UNSUP_GENERAL_COMMAND",
            0x83 => "UNSUP_MANUF_CLUSTER_COMMAND",
            0x84 => "UNSUP_MANUF_GENERAL_COMMAND",
            0x85 => "INVALID_FIELD",
            0x86 => "UNSUPPORTED_ATTRIBUTE",
            0x87 => "INVALID_VALUE",
            0x88 => "READ_ONLY",
            0x89 => "INSUFFICIENT_SPACE",
            0x8B => "NOT_FOUND",
            0x8C => "UNREPORTABLE_ATTRIBUTE",
            0x8D => "INVALID_DATA_TYPE",
            0x8E => "INVALID_SELECTOR",
            0x94 => "TIMEOUT",
            0x95 => "ABORT",
            0xC3 => "UNSUPPORTED_CLUSTER",
            _ => "UNKNOWN_STATUS",
        }
    }
}

/// One Read Attributes Response record.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadRecord {
    /// Attribute.
    pub attr: u16,
    /// Status.
    pub status: Status,
    /// Value for `SUCCESS`.
    pub value: Option<TypedValue>,
}

/// One Configure Reporting record (direction 0, the device reports).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReportingConfig {
    /// Attribute.
    pub attr: u16,
    /// Attribute type.
    pub ty: ZclType,
    /// Minimum interval, s.
    pub min: u16,
    /// Maximum interval, s.
    pub max: u16,
    /// Reportable change in raw units (analog types only).
    pub change: Option<f64>,
}

/// Payload of Read Attributes.
pub fn read_attributes(attrs: &[u16]) -> Vec<u8> {
    attrs.iter().flat_map(|a| a.to_le_bytes()).collect()
}

/// Parses Read Attributes (the device-side request; used by tests and the
/// simulator).
pub fn parse_read_attributes(p: &[u8]) -> Result<Vec<u16>, CodecError> {
    if !p.len().is_multiple_of(2) {
        return Err(CodecError::Truncated);
    }
    Ok(p.chunks(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect())
}

/// Parses Read Attributes Response.
pub fn parse_read_attributes_rsp(p: &[u8]) -> Result<Vec<ReadRecord>, CodecError> {
    let mut r = Reader::new(p);
    let mut out = Vec::new();
    while r.remaining() > 0 {
        let attr = r.u16()?;
        let status = Status(r.u8()?);
        let value = if status == Status::SUCCESS {
            let ty = ZclType(r.u8()?);
            Some(TypedValue::new(ty, decode_value(ty, &mut r)?))
        } else {
            None
        };
        out.push(ReadRecord {
            attr,
            status,
            value,
        });
    }
    Ok(out)
}

/// Encodes Read Attributes Response (simulator side).
pub fn read_attributes_rsp(records: &[ReadRecord]) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::new();
    for rec in records {
        out.extend_from_slice(&rec.attr.to_le_bytes());
        out.push(rec.status.0);
        if let Some(v) = rec.value.as_ref().filter(|_| rec.status == Status::SUCCESS) {
            out.push(v.ty.0);
            encode_value(v.ty, &v.value, &mut out)?;
        }
    }
    Ok(out)
}

/// Encodes Write Attributes.
pub fn write_attributes(records: &[(u16, TypedValue)]) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::new();
    for (attr, v) in records {
        out.extend_from_slice(&attr.to_le_bytes());
        out.push(v.ty.0);
        encode_value(v.ty, &v.value, &mut out)?;
    }
    Ok(out)
}

/// Parses Write Attributes (simulator side).
pub fn parse_write_attributes(p: &[u8]) -> Result<Vec<(u16, TypedValue)>, CodecError> {
    let mut r = Reader::new(p);
    let mut out = Vec::new();
    while r.remaining() > 0 {
        let attr = r.u16()?;
        let ty = ZclType(r.u8()?);
        out.push((attr, TypedValue::new(ty, decode_value(ty, &mut r)?)));
    }
    Ok(out)
}

/// Parses Write Attributes Response: a single `SUCCESS` byte, or
/// `(status, attr)` records for the failures.
pub fn parse_write_attributes_rsp(p: &[u8]) -> Result<Vec<(Status, Option<u16>)>, CodecError> {
    if p.len() == 1 {
        return Ok(vec![(Status(p[0]), None)]);
    }
    let mut r = Reader::new(p);
    let mut out = Vec::new();
    while r.remaining() > 0 {
        let st = Status(r.u8()?);
        let attr = if r.remaining() >= 2 {
            Some(r.u16()?)
        } else {
            None
        };
        out.push((st, attr));
    }
    Ok(out)
}

/// Encodes Write Attributes Response.
pub fn write_attributes_rsp(failures: &[(Status, u16)]) -> Vec<u8> {
    if failures.is_empty() {
        return vec![0x00];
    }
    let mut out = Vec::new();
    for (st, attr) in failures {
        out.push(st.0);
        out.extend_from_slice(&attr.to_le_bytes());
    }
    out
}

/// Encodes a reportable change of type `ty` from a raw-unit number.
fn encode_change(ty: ZclType, change: f64, out: &mut Vec<u8>) -> Result<(), CodecError> {
    let v = if ty.is_float() {
        ZclValue::Float(change)
    } else if ty.int_width().is_some() {
        ZclValue::Int(change.round() as i64)
    } else {
        ZclValue::Uint(change.round().max(0.0) as u64)
    };
    encode_value(ty, &v, out)
}

/// Encodes Configure Reporting.
pub fn configure_reporting(records: &[ReportingConfig]) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::new();
    for rec in records {
        out.push(0x00);
        out.extend_from_slice(&rec.attr.to_le_bytes());
        out.push(rec.ty.0);
        out.extend_from_slice(&rec.min.to_le_bytes());
        out.extend_from_slice(&rec.max.to_le_bytes());
        if rec.ty.is_analog() {
            encode_change(rec.ty, rec.change.unwrap_or(0.0), &mut out)?;
        }
    }
    Ok(out)
}

/// Parses Configure Reporting (simulator side).
pub fn parse_configure_reporting(p: &[u8]) -> Result<Vec<ReportingConfig>, CodecError> {
    let mut r = Reader::new(p);
    let mut out = Vec::new();
    while r.remaining() > 0 {
        let dir = r.u8()?;
        let attr = r.u16()?;
        if dir != 0 {
            r.u16()?;
            continue;
        }
        let ty = ZclType(r.u8()?);
        let min = r.u16()?;
        let max = r.u16()?;
        let change = if ty.is_analog() {
            decode_value(ty, &mut r)?.as_f64()
        } else {
            None
        };
        out.push(ReportingConfig {
            attr,
            ty,
            min,
            max,
            change,
        });
    }
    Ok(out)
}

/// One Read Reporting Configuration Response record (direction 0).
#[derive(Debug, Clone, PartialEq)]
pub struct ReportingRecord {
    /// Status.
    pub status: Status,
    /// Attribute.
    pub attr: u16,
    /// The configuration, for `SUCCESS`.
    pub config: Option<ReportingConfig>,
}

/// Payload of Read Reporting Configuration (direction 0: what the device
/// reports).
pub fn read_reporting_config(attrs: &[u16]) -> Vec<u8> {
    attrs
        .iter()
        .flat_map(|a| {
            let [lo, hi] = a.to_le_bytes();
            [0x00, lo, hi]
        })
        .collect()
}

/// Parses Read Reporting Configuration (simulator side): the direction-0
/// attributes.
pub fn parse_read_reporting_config(p: &[u8]) -> Result<Vec<u16>, CodecError> {
    if !p.len().is_multiple_of(3) {
        return Err(CodecError::Truncated);
    }
    Ok(p.chunks(3)
        .filter(|c| c[0] == 0)
        .map(|c| u16::from_le_bytes([c[1], c[2]]))
        .collect())
}

/// Parses Read Reporting Configuration Response (ZCL r8 2.5.10); records
/// of direction 1 (timeouts of received reports) are skipped.
pub fn parse_read_reporting_config_rsp(p: &[u8]) -> Result<Vec<ReportingRecord>, CodecError> {
    let mut r = Reader::new(p);
    let mut out = Vec::new();
    while r.remaining() > 0 {
        let status = Status(r.u8()?);
        let dir = r.u8()?;
        let attr = r.u16()?;
        if status != Status::SUCCESS {
            if dir == 0 {
                out.push(ReportingRecord {
                    status,
                    attr,
                    config: None,
                });
            }
            continue;
        }
        if dir != 0 {
            r.u16()?;
            continue;
        }
        let ty = ZclType(r.u8()?);
        let min = r.u16()?;
        let max = r.u16()?;
        let change = if ty.is_analog() {
            decode_value(ty, &mut r)?.as_f64()
        } else {
            None
        };
        out.push(ReportingRecord {
            status,
            attr,
            config: Some(ReportingConfig {
                attr,
                ty,
                min,
                max,
                change,
            }),
        });
    }
    Ok(out)
}

/// Encodes Read Reporting Configuration Response (simulator side).
pub fn read_reporting_config_rsp(recs: &[ReportingRecord]) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::new();
    for rec in recs {
        out.push(rec.status.0);
        out.push(0x00);
        out.extend_from_slice(&rec.attr.to_le_bytes());
        if let (Status::SUCCESS, Some(c)) = (rec.status, &rec.config) {
            out.push(c.ty.0);
            out.extend_from_slice(&c.min.to_le_bytes());
            out.extend_from_slice(&c.max.to_le_bytes());
            if c.ty.is_analog() {
                encode_change(c.ty, c.change.unwrap_or(0.0), &mut out)?;
            }
        }
    }
    Ok(out)
}

/// Parses Configure Reporting Response: one `SUCCESS` byte, or
/// `(status, direction, attr)` records.
pub fn parse_configure_reporting_rsp(p: &[u8]) -> Result<Vec<(Status, Option<u16>)>, CodecError> {
    if p.len() == 1 {
        return Ok(vec![(Status(p[0]), None)]);
    }
    let mut r = Reader::new(p);
    let mut out = Vec::new();
    while r.remaining() > 0 {
        let st = Status(r.u8()?);
        if r.remaining() >= 3 {
            r.u8()?;
            out.push((st, Some(r.u16()?)));
        } else {
            out.push((st, None));
        }
    }
    Ok(out)
}

/// Encodes Configure Reporting Response.
pub fn configure_reporting_rsp(failures: &[(Status, u16)]) -> Vec<u8> {
    if failures.is_empty() {
        return vec![0x00];
    }
    let mut out = Vec::new();
    for (st, attr) in failures {
        out.push(st.0);
        out.push(0x00);
        out.extend_from_slice(&attr.to_le_bytes());
    }
    out
}

/// Parses Report Attributes.
pub fn parse_report_attributes(p: &[u8]) -> Result<Vec<(u16, TypedValue)>, CodecError> {
    parse_write_attributes(p)
}

/// Xiaomi's structs that come typed as a character string: Basic 0xFF01,
/// 0xFF02 and Lumi 0xFCC0/0x00F7.
fn xiaomi_blob(cluster: u16, attr: u16) -> bool {
    matches!(
        (cluster, attr),
        (0x0000, 0xFF01 | 0xFF02) | (0xFCC0, 0x00F7)
    )
}

/// `rest` is records up to a tail too short for one (zhaquirks' check of
/// a length guess: what follows it parses).
fn records_parse(rest: &[u8]) -> bool {
    let mut r = Reader::new(rest);
    while r.remaining() >= 3 {
        let ok = r.u16().is_ok()
            && r.u8()
                .is_ok_and(|t| decode_value(ZclType(t), &mut r).is_ok());
        if !ok {
            return false;
        }
    }
    true
}

/// Parses Report Attributes leniently, for what devices send: the records
/// before a malformed one are kept, a remainder too short for a record is
/// ignored. Xiaomi's structs ([`xiaomi_blob`]) typed as a character string
/// are octet strings whose length byte may be one off (zhaquirks tries the
/// length, one less and one more, and takes the first that leaves a
/// parsable rest; zigbee-herdsman skips a trailing byte); typed as a ZCL
/// structure, the elements before a malformed one are kept.
pub fn parse_report_lenient(cluster: u16, p: &[u8]) -> Vec<(u16, TypedValue)> {
    let mut r = Reader::new(p);
    let mut out = Vec::new();
    while r.remaining() >= 3 {
        let (Ok(attr), Ok(t)) = (r.u16(), r.u8()) else {
            break;
        };
        let ty = ZclType(t);
        let value = match t {
            0x41 | 0x42 if xiaomi_blob(cluster, attr) => {
                let Ok(n) = r.u8() else { break };
                let rest = r.rest();
                let n = usize::from(n);
                let len = [n, n.wrapping_sub(1), n + 1]
                    .into_iter()
                    .find(|&l| l <= rest.len() && records_parse(&rest[l..]))
                    .unwrap_or(n.min(rest.len()));
                let Ok(b) = r.take(len) else { break };
                TypedValue::new(ZclType::OCTET_STR, ZclValue::Octets(b.to_vec()))
            }
            0x4C if xiaomi_blob(cluster, attr) => {
                let Ok(n) = r.u16() else { break };
                let mut items = Vec::new();
                for _ in 0..n {
                    let Ok(et) = r.u8().map(ZclType) else { break };
                    match decode_value(et, &mut r) {
                        Ok(v) => items.push((et, v)),
                        Err(_) => break,
                    }
                }
                TypedValue::new(ty, ZclValue::Struct(items))
            }
            _ => match decode_value(ty, &mut r) {
                Ok(v) => TypedValue::new(ty, v),
                Err(_) => break,
            },
        };
        out.push((attr, value));
    }
    out
}

/// Encodes Report Attributes.
pub fn report_attributes(records: &[(u16, TypedValue)]) -> Result<Vec<u8>, CodecError> {
    write_attributes(records)
}

/// Parses Default Response: `(command, status)`.
pub fn parse_default_rsp(p: &[u8]) -> Result<(u8, Status), CodecError> {
    let mut r = Reader::new(p);
    Ok((r.u8()?, Status(r.u8()?)))
}

/// Encodes Default Response.
pub fn default_rsp(cmd: u8, status: Status) -> Vec<u8> {
    vec![cmd, status.0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let f = ZclFrame {
            header: ZclHeader {
                frame_type: FrameType::Global,
                mfr: Some(0x115F),
                direction: Direction::ServerToClient,
                disable_default_response: true,
                tsn: 0x42,
                cmd: global::REPORT_ATTRIBUTES,
            },
            payload: vec![1, 2, 3],
        };
        let b = f.encode();
        assert_eq!(&b[..5], &[0x1C, 0x5F, 0x11, 0x42, 0x0A]);
        assert_eq!(ZclFrame::parse(&b).unwrap(), f);
        assert!(ZclFrame::parse(&[0x02, 0, 0]).is_err());
    }

    #[test]
    fn foundation_payloads() {
        // Temperature report 21.37 degC.
        let r = parse_report_attributes(&[0x00, 0x00, 0x29, 0x59, 0x08]).unwrap();
        assert_eq!(r, vec![(0, TypedValue::int(ZclType::I16, 2137))]);
        // Read response with an unsupported attribute.
        let rr = parse_read_attributes_rsp(&[
            0x05, 0x00, 0x00, 0x42, 0x02, b'h', b'i', 0x07, 0x00, 0x86,
        ])
        .unwrap();
        assert_eq!(
            rr[0].value.as_ref().unwrap().value,
            ZclValue::String("hi".into())
        );
        assert_eq!(rr[1].status, Status::UNSUPPORTED_ATTRIBUTE);
        let cfg = ReportingConfig {
            attr: 0,
            ty: ZclType::I16,
            min: 30,
            max: 900,
            change: Some(50.0),
        };
        let p = configure_reporting(std::slice::from_ref(&cfg)).unwrap();
        assert_eq!(p, vec![0x00, 0x00, 0x00, 0x29, 30, 0, 0x84, 0x03, 50, 0]);
        assert_eq!(parse_configure_reporting(&p).unwrap(), vec![cfg]);
        let onoff = ReportingConfig {
            attr: 0,
            ty: ZclType::BOOL,
            min: 0,
            max: 900,
            change: None,
        };
        assert_eq!(configure_reporting(&[onoff]).unwrap().len(), 8);
        assert_eq!(
            parse_write_attributes_rsp(&[0x00]).unwrap(),
            vec![(Status::SUCCESS, None)]
        );
        assert_eq!(
            parse_configure_reporting_rsp(&[0x8C, 0x00, 0x21, 0x00]).unwrap(),
            vec![(Status::UNREPORTABLE_ATTRIBUTE, Some(0x0021))]
        );
    }

    #[test]
    fn read_reporting_configuration() {
        assert_eq!(
            read_reporting_config(&[0x0000, 0x0021]),
            vec![0, 0, 0, 0, 0x21, 0]
        );
        assert_eq!(
            parse_read_reporting_config(&[0, 0, 0, 1, 5, 0, 0, 0x21, 0]).unwrap(),
            vec![0x0000, 0x0021]
        );
        // SUCCESS int16 [30, 900, 50], an unreportable attribute, a
        // direction-1 record (timeout) and NOT_FOUND (nothing configured).
        let p = [
            0x00, 0x00, 0x00, 0x00, 0x29, 30, 0, 0x84, 0x03, 50, 0, //
            0x8C, 0x00, 0x21, 0x00, //
            0x00, 0x01, 0x05, 0x00, 0x10, 0x0E, //
            0x8B, 0x00, 0x20, 0x00,
        ];
        let recs = parse_read_reporting_config_rsp(&p).unwrap();
        let cfg = ReportingConfig {
            attr: 0,
            ty: ZclType::I16,
            min: 30,
            max: 900,
            change: Some(50.0),
        };
        assert_eq!(
            recs,
            vec![
                ReportingRecord {
                    status: Status::SUCCESS,
                    attr: 0,
                    config: Some(cfg),
                },
                ReportingRecord {
                    status: Status::UNREPORTABLE_ATTRIBUTE,
                    attr: 0x0021,
                    config: None,
                },
                ReportingRecord {
                    status: Status::NOT_FOUND,
                    attr: 0x0020,
                    config: None,
                },
            ]
        );
        let back = read_reporting_config_rsp(&[recs[0].clone(), recs[1].clone()]).unwrap();
        assert_eq!(back, p[..15]);
        assert!(parse_read_reporting_config_rsp(&[0x00, 0x00, 0x00]).is_err());
    }

    /// Xiaomi's Basic 0xFF01 as a character string whose length byte is one
    /// off, followed by another record or by a stray byte; a malformed
    /// record keeps the ones before it.
    #[test]
    fn lenient_reports_take_xiaomi_structs() {
        let tlv = [0x01, 0x21, 0xBD, 0x0B, 0x64, 0x10, 0x01];
        let ff01 = |len: u8| [&[0x01, 0xFF, 0x42, len][..], &tlv].concat();
        let model = [0x05, 0x00, 0x42, 0x02, b'l', b'u'];
        for len in [7, 6, 8] {
            let recs = parse_report_lenient(0x0000, &[ff01(len), model.to_vec()].concat());
            assert_eq!(recs.len(), 2, "length byte {len}");
            assert_eq!(
                recs[0].1.value,
                ZclValue::Octets(tlv.to_vec()),
                "length byte {len}"
            );
            assert_eq!(recs[1].1.value, ZclValue::String("lu".into()));
        }
        let recs = parse_report_lenient(0x0000, &[ff01(7), vec![0x00]].concat());
        assert_eq!(recs.len(), 1);
        // Another cluster's string is a string.
        assert_eq!(
            parse_report_lenient(0x0006, &[0x01, 0xFF, 0x42, 0x01, b'a'])[0]
                .1
                .ty,
            ZclType::CHAR_STR
        );
        // A truncated second record: the first stays.
        let recs = parse_report_lenient(
            0x0402,
            &[0x00, 0x00, 0x29, 0x59, 0x08, 0x01, 0x00, 0x29, 0x59],
        );
        assert_eq!(
            recs,
            vec![(0, TypedValue::new(ZclType::I16, ZclValue::Int(2137)))]
        );
        assert!(
            parse_report_attributes(&[0x00, 0x00, 0x29, 0x59, 0x08, 0x01, 0x00, 0x29, 0x59])
                .is_err()
        );
    }
}
