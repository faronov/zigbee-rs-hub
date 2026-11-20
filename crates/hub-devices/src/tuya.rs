//! Tuya EF00 (spec 10): frames, datapoints and the built-in behaviours
//! (time sync, gateway status, MCU version). DP tables live in
//! definitions; this module is the protocol, implemented once.

use crate::expr::Value;
use crate::model::{TimeSync, TuyaType};
use crate::zcl::types::{CodecError, Reader};

/// EF00 command IDs (zigbee-herdsman `manuSpecificTuya`).
#[allow(missing_docs)]
pub mod cmd {
    pub const DATA_REQUEST: u8 = 0x00;
    pub const DATA_RESPONSE: u8 = 0x01;
    pub const DATA_REPORT: u8 = 0x02;
    pub const DATA_QUERY: u8 = 0x03;
    pub const SEND_DATA: u8 = 0x04;
    pub const ACTIVE_STATUS_REPORT_ALT: u8 = 0x05;
    pub const ACTIVE_STATUS_REPORT: u8 = 0x06;
    pub const DATA_REPORT_ALT: u8 = 0x0A;
    pub const MCU_VERSION_REQUEST: u8 = 0x10;
    pub const MCU_VERSION_RESPONSE: u8 = 0x11;
    pub const MCU_SYNC_TIME: u8 = 0x24;
    pub const MCU_GATEWAY_CONNECTION_STATUS: u8 = 0x25;
}

/// Device → hub commands that carry DP records.
pub fn is_data_command(c: u8) -> bool {
    matches!(
        c,
        cmd::DATA_RESPONSE | cmd::DATA_REPORT | cmd::ACTIVE_STATUS_REPORT_ALT | cmd::ACTIVE_STATUS_REPORT | cmd::DATA_REPORT_ALT
    )
}

/// One DP record.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DpRecord {
    /// DP number.
    pub dp: u8,
    /// Type code (0 raw … 5 bitmap).
    pub ty: u8,
    /// Data.
    pub data: Vec<u8>,
}

/// Parses `u16 seq` + DP records (all big-endian).
pub fn parse_data(p: &[u8]) -> Result<(u16, Vec<DpRecord>), CodecError> {
    let mut r = Reader::new(p);
    let s = r.take(2)?;
    let seq = u16::from_be_bytes([s[0], s[1]]);
    let mut out = Vec::new();
    while r.remaining() > 0 {
        let dp = r.u8()?;
        let ty = r.u8()?;
        let l = r.take(2)?;
        let len = usize::from(u16::from_be_bytes([l[0], l[1]]));
        out.push(DpRecord { dp, ty, data: r.take(len)?.to_vec() });
    }
    Ok((seq, out))
}

/// Encodes `u16 seq` + DP records.
pub fn encode_data(seq: u16, records: &[DpRecord]) -> Vec<u8> {
    let mut out = seq.to_be_bytes().to_vec();
    for r in records {
        out.push(r.dp);
        out.push(r.ty);
        out.extend_from_slice(&(r.data.len() as u16).to_be_bytes());
        out.extend_from_slice(&r.data);
    }
    out
}

/// Decodes DP data by its wire type (10.2).
pub fn dp_value(ty: u8, data: &[u8]) -> Value {
    let be = |d: &[u8]| d.iter().fold(0u64, |a, &b| (a << 8) | u64::from(b));
    match TuyaType::from_code(ty) {
        Some(TuyaType::Bool) => data.first().map_or(Value::Null, |b| Value::Bool(*b != 0)),
        Some(TuyaType::Value) => match data.len() {
            4 => Value::Int(i64::from(i32::from_be_bytes([data[0], data[1], data[2], data[3]]))),
            1..=8 => Value::Int(be(data) as i64),
            _ => Value::Null,
        },
        Some(TuyaType::Enum) => data.first().map_or(Value::Null, |b| Value::Int(i64::from(*b))),
        Some(TuyaType::Bitmap) if matches!(data.len(), 1 | 2 | 4) => Value::Int(be(data) as i64),
        Some(TuyaType::Raw | TuyaType::String) => Value::Bytes(data.to_vec()),
        _ => Value::Null,
    }
}

/// Encodes a value for a DP of type `ty`.
pub fn dp_encode(ty: TuyaType, v: &Value) -> Result<Vec<u8>, String> {
    let i = || v.as_i64().ok_or_else(|| format!("integer value expected for {ty:?}"));
    Ok(match ty {
        TuyaType::Bool => vec![u8::from(v.truthy().ok_or("bool expected")?)],
        TuyaType::Value => {
            let x = i()?;
            i32::try_from(x).map_err(|_| format!("{x} out of i32 range"))?.to_be_bytes().to_vec()
        }
        TuyaType::Enum => vec![u8::try_from(i()?).map_err(|_| "enum out of range")?],
        TuyaType::Bitmap => {
            let x = u32::try_from(i()?).map_err(|_| "bitmap out of range")?;
            if x <= 0xFF {
                vec![x as u8]
            } else if x <= 0xFFFF {
                (x as u16).to_be_bytes().to_vec()
            } else {
                x.to_be_bytes().to_vec()
            }
        }
        TuyaType::Raw | TuyaType::String => match v {
            Value::Bytes(b) => b.clone(),
            Value::Str(s) => s.as_bytes().to_vec(),
            _ => return Err("bytes or string expected".into()),
        },
    })
}

/// Seconds between 1970-01-01 and 2000-01-01.
pub const EPOCH_2000: i64 = 946_684_800;

/// mcuSyncTime (0x24) answer: `payloadSize` (u16 LE, 8) then
/// `[u32 utc, u32 local]` big-endian (herdsman `onEventSetTime`, zha
/// `TuyaTimePayload`).
pub fn time_sync_payload(now_unix: i64, tz_offset_s: i64, epoch: TimeSync) -> Option<Vec<u8>> {
    let base = match epoch {
        TimeSync::Off => return None,
        TimeSync::Epoch1970 => 0,
        TimeSync::Epoch2000 => EPOCH_2000,
    };
    let utc = (now_unix - base).max(0) as u32;
    let local = (now_unix + tz_offset_s - base).max(0) as u32;
    let mut out = vec![0x08, 0x00];
    out.extend_from_slice(&utc.to_be_bytes());
    out.extend_from_slice(&local.to_be_bytes());
    Some(out)
}

/// mcuGatewayConnectionStatus (0x25) answer: `payloadSize` 1, status 1
/// (connected).
pub fn gateway_status_payload() -> Vec<u8> {
    vec![0x01, 0x00, 0x01]
}

/// MCU version byte → `a.b.c` (herdsman `mcuVersionResponse`).
pub fn mcu_version(v: u8) -> String {
    format!("{}.{}.{}", (v & 0xC0) >> 6, (v & 0x30) >> 4, v & 0x0F)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dp_frames() {
        // seq 0x0102; DP 4 value 215 (21.5 degC); DP 2 enum 1.
        let p = [0x01, 0x02, 0x04, 0x02, 0x00, 0x04, 0x00, 0x00, 0x00, 0xD7, 0x02, 0x04, 0x00, 0x01, 0x01];
        let (seq, recs) = parse_data(&p).unwrap();
        assert_eq!(seq, 0x0102);
        assert_eq!(recs.len(), 2);
        assert_eq!(dp_value(recs[0].ty, &recs[0].data), Value::Int(215));
        assert_eq!(dp_value(recs[1].ty, &recs[1].data), Value::Int(1));
        assert_eq!(encode_data(seq, &recs), p);
        assert_eq!(dp_value(2, &(-25i32).to_be_bytes()), Value::Int(-25));
        assert_eq!(dp_encode(TuyaType::Value, &Value::Int(-25)).unwrap(), (-25i32).to_be_bytes());
        assert_eq!(dp_encode(TuyaType::Bool, &Value::Bool(true)).unwrap(), [1]);
        assert!(parse_data(&[0, 1, 4, 2, 0, 4, 0]).is_err());
    }

    #[test]
    fn behaviours() {
        let p = time_sync_payload(EPOCH_2000 + 100, 3600, TimeSync::Epoch2000).unwrap();
        assert_eq!(p, [0x08, 0x00, 0, 0, 0, 100, 0, 0, 0x0E, 0x74]);
        assert!(time_sync_payload(0, 0, TimeSync::Off).is_none());
        assert_eq!(mcu_version(0x45), "1.0.5");
    }
}
