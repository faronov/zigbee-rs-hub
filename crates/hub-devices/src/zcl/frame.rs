//! ZCL frames (ZCL r8 2.4) and the foundation commands the hub sends and
//! receives (2.5): Read Attributes, Write Attributes, Configure Reporting,
//! Report Attributes, Default Response.

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
                direction: if fc & 0x08 != 0 { Direction::ServerToClient } else { Direction::ClientToServer },
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
    pub fn command(frame_type: FrameType, tsn: u8, cmd: u8, mfr: Option<u16>, payload: Vec<u8>) -> Self {
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
    if p.len() % 2 != 0 {
        return Err(CodecError::Truncated);
    }
    Ok(p.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect())
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
        out.push(ReadRecord { attr, status, value });
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
        let attr = if r.remaining() >= 2 { Some(r.u16()?) } else { None };
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
        let change = if ty.is_analog() { decode_value(ty, &mut r)?.as_f64() } else { None };
        out.push(ReportingConfig { attr, ty, min, max, change });
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
        let rr = parse_read_attributes_rsp(&[0x05, 0x00, 0x00, 0x42, 0x02, b'h', b'i', 0x07, 0x00, 0x86]).unwrap();
        assert_eq!(rr[0].value.as_ref().unwrap().value, ZclValue::String("hi".into()));
        assert_eq!(rr[1].status, Status::UNSUPPORTED_ATTRIBUTE);
        let cfg = ReportingConfig { attr: 0, ty: ZclType::I16, min: 30, max: 900, change: Some(50.0) };
        let p = configure_reporting(std::slice::from_ref(&cfg)).unwrap();
        assert_eq!(p, vec![0x00, 0x00, 0x00, 0x29, 30, 0, 0x84, 0x03, 50, 0]);
        assert_eq!(parse_configure_reporting(&p).unwrap(), vec![cfg]);
        let onoff = ReportingConfig { attr: 0, ty: ZclType::BOOL, min: 0, max: 900, change: None };
        assert_eq!(configure_reporting(&[onoff]).unwrap().len(), 8);
        assert_eq!(parse_write_attributes_rsp(&[0x00]).unwrap(), vec![(Status::SUCCESS, None)]);
        assert_eq!(
            parse_configure_reporting_rsp(&[0x8C, 0x00, 0x21, 0x00]).unwrap(),
            vec![(Status::UNREPORTABLE_ATTRIBUTE, Some(0x0021))]
        );
    }
}
