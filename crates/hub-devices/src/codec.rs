//! Named codecs (spec 6.6): a closed, versioned set implemented once in
//! the hub.

use crate::expr::Value;
use crate::model::{Codec, ScheduleVariant};
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

/// Byte layout of a Tuya schedule DP (zigbee-herdsman-converters
/// `valueConverter.thermostatScheduleDay*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleLayout {
    /// Variant.
    pub variant: ScheduleVariant,
    /// Day number written as the first byte (`*WithDayNumber`).
    pub day: Option<u8>,
    /// Transitions (`multi_dp`).
    pub transitions: u8,
    /// Temperature scale (`multi_dp`).
    pub scale: u16,
    /// Accepted temperature range, °C (`multi_dp`).
    pub min: i16,
    /// Accepted temperature range, °C (`multi_dp`).
    pub max: i16,
}

impl ScheduleLayout {
    /// The layout of a codec, if it is a schedule codec.
    pub fn of(c: &Codec) -> Option<Self> {
        let d = |variant, day, transitions| ScheduleLayout {
            variant,
            day,
            transitions,
            scale: 10,
            min: 5,
            max: 35,
        };
        match *c {
            Codec::TuyaScheduleDay {
                day,
                transitions,
                variant,
            } => Some(d(variant, Some(day), transitions)),
            Codec::TuyaScheduleWeek {
                variant,
                transitions,
                scale,
                min,
                max,
            } => Some(ScheduleLayout {
                scale,
                min,
                max,
                ..d(variant, None, transitions)
            }),
            _ => None,
        }
    }
}

/// JavaScript `String(number)` for one decimal (herdsman `single_dp` text).
fn js_num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v}")
    }
}

fn hm(h: u32, m: u32) -> String {
    format!("{h:02}:{m:02}")
}

/// Decodes a schedule DP into its text form:
/// - `multi_dp`: byte 0 (day number or 0), then per transition
///   `hour, minute, temp×scale (u16 BE)` → `HH:MM/T.T …`;
/// - `trv602z`: byte 0, then 6 × `0xC0|minutes>>8, minutes, preset<<4, temp×10`
///   → `HH:MM/T.T/P …`;
/// - `single_dp`: up to 10 × `segment (10 min), temp×10 (u16 BE)`, ending at
///   24:00 → `HH:MM/T …`;
/// - `trv603wz`: `meta, count, (hour×10 + minute/10, temp×2)*` → `HH:MM/T.T …`.
pub fn schedule_decode(b: &[u8], l: &ScheduleLayout) -> Option<String> {
    let parts: Vec<String> = match l.variant {
        ScheduleVariant::MultiDp => {
            let body = b.get(1..)?;
            let n = usize::from(l.transitions).min(body.len() / 4);
            let scale = f64::from(l.scale.max(1));
            body.as_chunks::<4>()
                .0
                .iter()
                .take(n)
                .map(|c| {
                    format!(
                        "{}/{:.1}",
                        hm(c[0].into(), c[1].into()),
                        f64::from(u16::from_be_bytes([c[2], c[3]])) / scale
                    )
                })
                .collect()
        }
        ScheduleVariant::Trv602z => {
            let body = b.get(1..)?;
            body.as_chunks::<4>()
                .0
                .iter()
                .take(6)
                .map(|c| {
                    let mins = (u32::from(c[0].wrapping_sub(192)) << 8) | u32::from(c[1]);
                    format!(
                        "{}/{:.1}/{}",
                        hm(mins / 60, mins % 60),
                        f64::from(c[3]) / 10.0,
                        c[2] >> 4
                    )
                })
                .collect()
        }
        ScheduleVariant::SingleDp => {
            let mut v = Vec::new();
            for c in b.as_chunks::<3>().0.iter().take(10) {
                let mins = u32::from(c[0]) * 10;
                v.push(format!(
                    "{}/{}",
                    hm(mins / 60, mins % 60),
                    js_num(f64::from(u16::from_be_bytes([c[1], c[2]])) / 10.0)
                ));
                if mins / 60 == 24 {
                    break;
                }
            }
            v
        }
        ScheduleVariant::Trv603wz => {
            let count = usize::from(*b.get(1)?);
            let body = b.get(2..2 + count)?;
            body.as_chunks::<2>()
                .0
                .iter()
                .map(|c| {
                    format!(
                        "{}/{:.1}",
                        hm(u32::from(c[0]) / 10, u32::from(c[0]) % 10 * 10),
                        f64::from(c[1]) / 2.0
                    )
                })
                .collect()
        }
    };
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// One `HH:MM/T[/P]` item.
fn transition(it: &str, fields: usize) -> Result<(u32, u32, f64, Option<u32>), CodecFailure> {
    let bad = || CodecFailure(format!("bad transition {it:?}"));
    let f: Vec<&str> = it.split('/').collect();
    if f.len() != fields {
        return Err(bad());
    }
    let (h, m) = f[0].split_once(':').ok_or_else(bad)?;
    let h: u32 = h.parse().map_err(|_| bad())?;
    let m: u32 = m.parse().map_err(|_| bad())?;
    let t: f64 = f[1].trim_end_matches("°C").parse().map_err(|_| bad())?;
    let p = f
        .get(2)
        .map(|p| p.parse::<u32>().map_err(|_| bad()))
        .transpose()?;
    if h > 24 || m > 59 || !t.is_finite() {
        return Err(bad());
    }
    Ok((h, m, t, p))
}

/// Inverse of [`schedule_decode`]. `single_dp` text starts with the day
/// selector (`monday`…`sunday`, `mon_sun`, `mon_fri`), which herdsman takes
/// from `week_day` and `working_day`.
pub fn schedule_encode(text: &str, l: &ScheduleLayout) -> Result<Vec<u8>, CodecFailure> {
    let mut items: Vec<&str> = text.split_whitespace().collect();
    let fail = |m: String| Err(CodecFailure(m));
    match l.variant {
        ScheduleVariant::MultiDp => {
            if items.len() != usize::from(l.transitions) {
                return fail(format!(
                    "schedule needs {} transitions, got {}",
                    l.transitions,
                    items.len()
                ));
            }
            let mut out = vec![l.day.unwrap_or(0)];
            let scale = i64::from(l.scale.max(1));
            for it in items {
                let (h, m, t, _) = transition(it, 2)?;
                let v = (t * scale as f64 + 1e-9).floor() as i64;
                if v < i64::from(l.min) * scale || v > i64::from(l.max) * scale || v > 0xFFFF {
                    return fail(format!("temperature out of range in {it:?}"));
                }
                out.extend_from_slice(&[h as u8, m as u8]);
                out.extend_from_slice(&(v as u16).to_be_bytes());
            }
            Ok(out)
        }
        ScheduleVariant::Trv602z => {
            if items.len() != 6 {
                return fail(format!("schedule needs 6 transitions, got {}", items.len()));
            }
            let mut out: Vec<u8> = l.day.into_iter().collect();
            for it in items {
                let (h, m, t, p) = transition(it, 3)?;
                let t10 = (t * 10.0 + 1e-9).floor() as i64;
                // herdsman allows up to 30.0 but the field is one byte
                if !(50..=255).contains(&t10) {
                    return fail(format!("temperature out of range in {it:?}"));
                }
                let p = p.unwrap_or(0);
                if p != 2 && p != 3 {
                    return fail(format!("preset must be 2 (eco) or 3 (comfort) in {it:?}"));
                }
                let mins = h * 60 + m;
                out.extend_from_slice(&[
                    (((mins & 0x0F00) >> 8) + 192) as u8,
                    (mins & 0xFF) as u8,
                    (p * 16) as u8,
                    t10 as u8,
                ]);
            }
            Ok(out)
        }
        ScheduleVariant::SingleDp => {
            if items.is_empty() {
                return fail("schedule needs a day selector".into());
            }
            let day = match items.remove(0) {
                "monday" => 1,
                "tuesday" => 2,
                "wednesday" => 4,
                "thursday" => 8,
                "friday" => 16,
                "saturday" => 32,
                "sunday" => 64,
                "mon_sun" => 127,
                "mon_fri" => 31,
                d => return fail(format!("unknown day selector {d:?}")),
            };
            if !(2..=10).contains(&items.len()) {
                return fail(format!("schedule needs 2..10 periods, got {}", items.len()));
            }
            let mut out = vec![day];
            let mut prev = 0;
            for it in &items {
                let (h, m, t, _) = transition(it, 2)?;
                if m % 10 != 0 || !(5.0..=30.0).contains(&t) || (t * 2.0).fract() != 0.0 || h < prev
                {
                    return fail(format!("bad period {it:?}"));
                }
                prev = h;
                out.push(((h * 60 + m) / 10) as u8);
                out.extend_from_slice(&((t * 10.0).round() as u16).to_be_bytes());
            }
            for _ in items.len()..10 {
                out.extend_from_slice(&[144, 0, 180]);
            }
            Ok(out)
        }
        ScheduleVariant::Trv603wz => {
            let mut out = vec![7, (items.len() * 2) as u8];
            for it in items {
                let (h, m, t, _) = transition(it, 2)?;
                if h > 23 || m % 10 != 0 || !(0.0..=127.5).contains(&t) {
                    return fail(format!("bad segment {it:?}"));
                }
                out.extend_from_slice(&[(h * 10 + m / 10) as u8, (t * 2.0).round() as u8]);
            }
            Ok(out)
        }
    }
}

fn bcd(b: u8) -> Option<u8> {
    let (hi, lo) = (b >> 4, b & 0x0F);
    (hi < 10 && lo < 10).then_some(hi * 10 + lo)
}

/// Decodes a raw value with a codec.
pub fn decode(codec: &Codec, raw: &Value) -> Value {
    match codec {
        Codec::ZclTagged => Value::Null,
        Codec::TuyaScheduleDay { .. } | Codec::TuyaScheduleWeek { .. } => bytes_of(raw)
            .zip(ScheduleLayout::of(codec))
            .and_then(|(b, l)| schedule_decode(b, &l))
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
        Codec::TuyaScheduleDay { .. } | Codec::TuyaScheduleWeek { .. } => {
            let l = ScheduleLayout::of(codec).ok_or_else(|| CodecFailure("schedule".into()))?;
            schedule_encode(text()?, &l).map(Value::Bytes)
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
            variant: ScheduleVariant::MultiDp,
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

    fn week(variant: ScheduleVariant, transitions: u8) -> Codec {
        Codec::TuyaScheduleWeek {
            variant,
            transitions,
            scale: 10,
            min: 5,
            max: 35,
        }
    }

    fn round_trip(c: &Codec, text: &str, bytes: &[u8]) {
        let raw = encode(c, &Value::Str(text.into())).unwrap();
        assert_eq!(raw, Value::Bytes(bytes.to_vec()), "{text}");
    }

    /// Byte vectors computed with zigbee-herdsman-converters' value
    /// converters (`tuya.valueConverter.thermostatScheduleDay*`).
    #[test]
    fn schedule_variants_match_herdsman() {
        // thermostatScheduleDayMultiDPWithTransitionCount(4)
        let c = week(ScheduleVariant::MultiDp, 4);
        let t = "06:00/21.0 08:00/16.0 17:00/21.5 22:00/16.0";
        let b = [0, 6, 0, 0, 210, 8, 0, 0, 160, 17, 0, 0, 215, 22, 0, 0, 160];
        round_trip(&c, t, &b);
        assert_eq!(decode(&c, &Value::Bytes(b.to_vec())), Value::Str(t.into()));
        assert!(
            encode(
                &c,
                &Value::Str("06:00/40.0 08:00/16.0 17:00/21.5 22:00/16.0".into())
            )
            .is_err()
        );
        // thermostatScheduleDayMultiDP_TRV602Z(_WithDayNumber(3))
        let t = "06:00/21.0/3 08:00/16.0/2 12:00/21.0/3 14:00/16.0/2 17:00/21.0/3 22:00/16.0/2";
        let b = [
            193, 104, 48, 210, 193, 224, 32, 160, 194, 208, 48, 210, 195, 72, 32, 160, 195, 252,
            48, 210, 197, 40, 32, 160,
        ];
        round_trip(&week(ScheduleVariant::Trv602z, 6), t, &b);
        let day = Codec::TuyaScheduleDay {
            day: 3,
            transitions: 6,
            variant: ScheduleVariant::Trv602z,
        };
        let mut with_day = vec![3];
        with_day.extend_from_slice(&b);
        round_trip(&day, t, &with_day);
        assert_eq!(decode(&day, &Value::Bytes(with_day)), Value::Str(t.into()));
        // thermostatScheduleDaySingleDP, week_day tuesday, working_day separate
        let c = week(ScheduleVariant::SingleDp, 10);
        let mut b = vec![2, 36, 0, 210, 144, 0, 160];
        for _ in 0..8 {
            b.extend_from_slice(&[144, 0, 180]);
        }
        round_trip(&c, "tuesday 06:00/21 24:00/16", &b);
        assert_eq!(
            decode(&c, &Value::Bytes(b[1..].to_vec())),
            Value::Str("06:00/21 24:00/16".into())
        );
        // thermostatScheduleDayMultiDP_TRV603WZ
        let c = week(ScheduleVariant::Trv603wz, 8);
        let t = "06:30/21.0 17:30/21.5";
        let b = [7, 4, 63, 42, 173, 43];
        round_trip(&c, t, &b);
        assert_eq!(decode(&c, &Value::Bytes(b.to_vec())), Value::Str(t.into()));
    }
}
