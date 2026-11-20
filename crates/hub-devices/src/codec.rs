//! Named codecs (spec 6.6): a closed, versioned set implemented once in
//! the hub.

use crate::expr::Value;
use crate::model::Codec;
use crate::zcl::types::{Reader, TypedValue, ZclType, decode_value};

/// Codec failure (the state becomes `null`, the command fails with
/// `INVALID_VALUE`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecFailure(pub String);

/// Decodes the Lumi / genBasic 0xFF01 TLV: `(tag u8, ZCL type u8, value)*`.
/// Stops at the first undecodable record and returns what it has.
pub fn decode_tagged(b: &[u8]) -> Vec<(u8, TypedValue)> {
    let mut r = Reader::new(b);
    let mut out = Vec::new();
    while r.remaining() >= 2 {
        let Ok(tag) = r.u8() else { break };
        let Ok(t) = r.u8() else { break };
        let ty = ZclType(t);
        match decode_value(ty, &mut r) {
            Ok(v) => out.push((tag, TypedValue::new(ty, v))),
            Err(_) => break,
        }
    }
    out
}

fn bytes_of(v: &Value) -> Option<&[u8]> {
    match v {
        Value::Bytes(b) => Some(b),
        _ => None,
    }
}

/// Tuya per-day schedule (herdsman
/// `thermostatScheduleDayMultiDPWithDayNumber(day, transitions)`): byte 0
/// day number, then per transition `hour, minute, temp×10 (u16 BE)`; text
/// `HH:MM/T.T HH:MM/T.T …`.
pub fn schedule_day_decode(b: &[u8], transitions: u8) -> Option<String> {
    let body = b.get(1..)?;
    let n = usize::from(transitions).min(body.len() / 4);
    if n == 0 {
        return None;
    }
    let parts: Vec<String> = body
        .as_chunks::<4>()
        .0
        .iter()
        .take(n)
        .map(|c| {
            format!(
                "{:02}:{:02}/{:.1}",
                c[0],
                c[1],
                f64::from(u16::from_be_bytes([c[2], c[3]])) / 10.0
            )
        })
        .collect();
    Some(parts.join(" "))
}

/// Inverse of [`schedule_day_decode`]; the text must have exactly
/// `transitions` entries.
pub fn schedule_day_encode(text: &str, day: u8, transitions: u8) -> Result<Vec<u8>, CodecFailure> {
    let items: Vec<&str> = text.split_whitespace().collect();
    if items.len() != usize::from(transitions) {
        return Err(CodecFailure(format!(
            "schedule needs {transitions} transitions, got {}",
            items.len()
        )));
    }
    let mut out = vec![day];
    for it in items {
        let bad = || CodecFailure(format!("bad transition {it:?}, expected HH:MM/T"));
        let (hm, t) = it.split_once('/').ok_or_else(bad)?;
        let (h, m) = hm.split_once(':').ok_or_else(bad)?;
        let h: u8 = h.parse().map_err(|_| bad())?;
        let m: u8 = m.parse().map_err(|_| bad())?;
        let t: f64 = t.trim_end_matches("°C").parse().map_err(|_| bad())?;
        if h > 24 || m > 59 || !(0.0..=6553.5).contains(&t) {
            return Err(bad());
        }
        let t10 = (t * 10.0).round() as u16;
        out.extend_from_slice(&[h, m]);
        out.extend_from_slice(&t10.to_be_bytes());
    }
    Ok(out)
}

fn bcd(b: u8) -> Option<u8> {
    let (hi, lo) = (b >> 4, b & 0x0F);
    (hi < 10 && lo < 10).then_some(hi * 10 + lo)
}

/// Decodes a raw value with a codec.
pub fn decode(codec: &Codec, raw: &Value) -> Value {
    match codec {
        Codec::ZclTagged => Value::Null,
        Codec::TuyaScheduleDay { transitions, .. } => bytes_of(raw)
            .and_then(|b| schedule_day_decode(b, *transitions))
            .map_or(Value::Null, Value::Str),
        Codec::BcdTime => match bytes_of(raw) {
            Some([h, m, ..]) => match (bcd(*h), bcd(*m)) {
                (Some(h), Some(m)) => Value::Str(format!("{h:02}:{m:02}")),
                _ => Value::Null,
            },
            _ => Value::Null,
        },
        Codec::Hhmm => match bytes_of(raw) {
            Some([h, m, ..]) => Value::Str(format!("{h:02}:{m:02}")),
            _ => match raw.as_i64() {
                Some(v) if (0..1440).contains(&v) => {
                    Value::Str(format!("{:02}:{:02}", v / 60, v % 60))
                }
                _ => Value::Null,
            },
        },
        Codec::Ascii => bytes_of(raw).map_or_else(
            || raw.clone(),
            |b| {
                Value::Str(
                    String::from_utf8_lossy(b)
                        .trim_end_matches('\0')
                        .to_string(),
                )
            },
        ),
        Codec::Utf16BeHex => match bytes_of(raw) {
            Some(b) if b.len() % 2 == 0 => {
                let units: Vec<u16> = b
                    .chunks(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16(&units).map_or(Value::Null, |s| {
                    Value::Str(s.trim_end_matches('\0').to_string())
                })
            }
            _ => Value::Null,
        },
        Codec::BitmapFlags(labels) => match raw.as_i64() {
            Some(v) => Value::Str(
                labels
                    .iter()
                    .filter(|(bit, _)| (v >> bit) & 1 == 1)
                    .map(|(_, l)| l.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            None => Value::Null,
        },
    }
}

/// Encodes a value with a codec into raw bytes or an integer.
pub fn encode(codec: &Codec, v: &Value) -> Result<Value, CodecFailure> {
    let text = || {
        v.as_str()
            .ok_or_else(|| CodecFailure("text value expected".into()))
    };
    match codec {
        Codec::TuyaScheduleDay { day, transitions } => {
            schedule_day_encode(text()?, *day, *transitions).map(Value::Bytes)
        }
        Codec::BcdTime | Codec::Hhmm => {
            let t = text()?;
            let bad = || CodecFailure(format!("bad time {t:?}"));
            let (h, m) = t.split_once(':').ok_or_else(bad)?;
            let (h, m): (u8, u8) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
            if h > 23 || m > 59 {
                return Err(bad());
            }
            Ok(Value::Bytes(if matches!(codec, Codec::BcdTime) {
                vec![((h / 10) << 4) | (h % 10), ((m / 10) << 4) | (m % 10)]
            } else {
                vec![h, m]
            }))
        }
        Codec::Ascii => Ok(Value::Bytes(text()?.as_bytes().to_vec())),
        Codec::Utf16BeHex => Ok(Value::Bytes(
            text()?.encode_utf16().flat_map(u16::to_be_bytes).collect(),
        )),
        Codec::BitmapFlags(labels) => {
            let mut x = 0i64;
            for l in text()?.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let (bit, _) = labels
                    .iter()
                    .find(|(_, n)| n == l)
                    .ok_or_else(|| CodecFailure(format!("unknown flag {l}")))?;
                x |= 1 << bit;
            }
            Ok(Value::Int(x))
        }
        Codec::ZclTagged => Err(CodecFailure("zcl_tagged is read-only".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zcl::ZclValue;

    #[test]
    fn lumi_tagged_struct() {
        // tag 1 u16 3005 mV, tag 3 i8 26 degC, tag 5 u16 7, tag 0x64 bool.
        let b = [
            0x01, 0x21, 0xBD, 0x0B, 0x03, 0x28, 0x1A, 0x05, 0x21, 0x07, 0x00, 0x64, 0x10, 0x01,
        ];
        let t = decode_tagged(&b);
        assert_eq!(t[0], (1, TypedValue::uint(ZclType::U16, 3005)));
        assert_eq!(t[1], (3, TypedValue::int(ZclType::I8, 26)));
        assert_eq!(t[2].0, 5);
        assert_eq!(t[3].1.value, ZclValue::Bool(Some(true)));
        // Truncated tail is ignored.
        assert_eq!(decode_tagged(&b[..12]).len(), 3);
    }

    #[test]
    fn schedule_round_trip() {
        let c = Codec::TuyaScheduleDay {
            day: 1,
            transitions: 6,
        };
        let text = "06:00/20.0 08:00/16.5 12:00/20.0 14:00/16.0 17:00/21.0 22:00/16.0";
        let raw = encode(&c, &Value::Str(text.into())).unwrap();
        let Value::Bytes(b) = &raw else { panic!() };
        assert_eq!(b.len(), 25);
        assert_eq!(&b[..5], &[1, 6, 0, 0, 200]);
        assert_eq!(decode(&c, &raw), Value::Str(text.into()));
        assert!(encode(&c, &Value::Str("06:00/20".into())).is_err());
        assert_eq!(
            decode(&Codec::BcdTime, &Value::Bytes(vec![0x23, 0x59])),
            Value::Str("23:59".into())
        );
        assert_eq!(
            decode(
                &Codec::BitmapFlags(vec![(0, "a".into()), (2, "c".into())]),
                &Value::Int(5)
            ),
            Value::Str("a,c".into())
        );
        assert_eq!(
            decode(
                &Codec::Utf16BeHex,
                &encode(&Codec::Utf16BeHex, &Value::Str("Hé".into())).unwrap()
            ),
            Value::Str("Hé".into())
        );
    }
}
