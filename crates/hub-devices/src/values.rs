//! The value pipeline (spec 6.4): decode device values into entity states
//! and encode commanded values back into raw values, for every binding
//! kind.

use crate::codec;
use crate::expr::{ExprCtx, ExprEnv, Value};
use crate::model::{Codec, Platform, Transforms, ValueDef};
use crate::zcl::types::int_range;
use crate::zcl::{ZclType, ZclValue};

/// Converts a decoded ZCL value into an expression value.
pub fn from_zcl(v: &ZclValue) -> Value {
    match v {
        ZclValue::Bool(Some(b)) => Value::Bool(*b),
        ZclValue::Uint(u) => i64::try_from(*u).map_or(Value::Float(*u as f64), Value::Int),
        ZclValue::Int(i) => Value::Int(*i),
        ZclValue::Float(f) if f.is_finite() => Value::Float(*f),
        ZclValue::Octets(b) => Value::Bytes(b.clone()),
        ZclValue::String(s) => Value::Str(s.clone()),
        _ => Value::Null,
    }
}

/// Converts an encoded value into a ZCL value of type `ty` with a range
/// check; integers are rounded.
pub fn to_zcl(ty: ZclType, v: &Value) -> Result<ZclValue, String> {
    if ty == ZclType::BOOL {
        return v.truthy().map(|b| ZclValue::Bool(Some(b))).ok_or_else(|| format!("bool expected, got {v:?}"));
    }
    if ty.is_float() {
        return v.as_f64().map(ZclValue::Float).ok_or_else(|| "number expected".into());
    }
    if matches!(ty, ZclType::OCTET_STR | ZclType::LONG_OCTET_STR) {
        return match v {
            Value::Bytes(b) => Ok(ZclValue::Octets(b.clone())),
            Value::Str(s) => Ok(ZclValue::Octets(s.as_bytes().to_vec())),
            _ => Err("bytes expected".into()),
        };
    }
    if matches!(ty, ZclType::CHAR_STR | ZclType::LONG_CHAR_STR) {
        return match v {
            Value::Str(s) => Ok(ZclValue::String(s.clone())),
            _ => Err("string expected".into()),
        };
    }
    let (lo, hi) = int_range(ty).ok_or_else(|| format!("type {ty} not writable"))?;
    let f = v.as_f64().ok_or_else(|| format!("number expected, got {v:?}"))?;
    let i = f.round() as i128;
    if i < lo || i > hi {
        return Err(format!("{f} out of range {lo}..={hi} for type {ty}"));
    }
    Ok(if ty.int_width().is_some() { ZclValue::Int(i as i64) } else { ZclValue::Uint(i as u64) })
}

/// Outcome flags of one decode, for diagnostics counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecodeFlags {
    /// A raw value not in `map`.
    pub map_miss: bool,
    /// An expression yielded `null` on a non-null input.
    pub expr_error: bool,
}

fn eq_num(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) if !matches!(a, Value::Str(_)) && !matches!(b, Value::Str(_)) => x == y,
        _ => a == b,
    }
}

/// Decodes a raw value through the transforms (6.4).
pub fn decode(
    vd: &ValueDef,
    is_bool: bool,
    raw: Value,
    env: &dyn ExprEnv,
    scale: &dyn Fn(u16, u16) -> Option<f64>,
) -> (Value, DecodeFlags) {
    let t = &vd.transforms;
    let mut flags = DecodeFlags::default();
    // 1. codec (the binding's native decode already happened).
    let mut x = match &t.codec {
        Some(c) if !matches!(c, Codec::ZclTagged) => codec::decode(c, &raw),
        _ => raw,
    };
    if x.is_null() {
        return (Value::Null, flags);
    }
    // 2. invalid.
    if let Some(inv) = t.invalid {
        if x.as_f64() == Some(inv) {
            return (Value::Null, flags);
        }
    }
    // 3. map.
    if !t.map.is_empty() {
        match x.as_i64().and_then(|k| t.map.iter().find(|(r, _)| *r == k)) {
            Some((_, l)) => x = Value::Str(l.clone()),
            None => {
                flags.map_miss = true;
                return (Value::Null, flags);
            }
        }
    }
    // 4. value_on / value_off.
    if t.value_on.is_some() || t.value_off.is_some() {
        x = if t.value_on.as_ref().is_some_and(|on| eq_num(&x, on)) {
            Value::Bool(true)
        } else if t.value_off.as_ref().is_some_and(|off| eq_num(&x, off)) {
            Value::Bool(false)
        } else if t.value_on.is_some() && t.value_off.is_none() {
            Value::Bool(false)
        } else if t.value_off.is_some() && t.value_on.is_none() {
            Value::Bool(true)
        } else {
            Value::Null
        };
    } else if is_bool && t.from.is_none() && t.map.is_empty() {
        x = x.truthy().map_or(Value::Null, Value::Bool);
    }
    // 5. from.
    if let Some(f) = &t.from {
        let r = f.eval(&ExprCtx::with_x(x, env));
        if r.is_null() {
            flags.expr_error = true;
        }
        x = r;
    }
    // 6. mul / div / offset, or the generic attribute scaling.
    let (mut mul, mut div) = (t.mul, t.div);
    if mul.is_none() && div.is_none() {
        if let Some(s) = &t.attr_scale {
            let m = s.mul_attr.and_then(|a| scale(s.cluster, a)).filter(|v| *v != 0.0).unwrap_or(1.0);
            let d = s.div_attr.and_then(|a| scale(s.cluster, a)).filter(|v| *v != 0.0).unwrap_or(1.0);
            mul = Some(m * s.factor);
            div = Some(d);
        }
    }
    if mul.is_some() || div.is_some() || t.offset.is_some() {
        if let Some(v) = x.as_f64().filter(|_| !matches!(x, Value::Bool(_))) {
            let r = v * mul.unwrap_or(1.0) / div.unwrap_or(1.0) + t.offset.unwrap_or(0.0);
            x = Value::Float(r);
        }
    }
    // 7. precision.
    if let (Some(p), Value::Float(v)) = (t.precision, &x) {
        let m = 10f64.powi(i32::from(p));
        x = Value::Float((v * m).round() / m);
    }
    if let Value::Float(v) = x {
        // Clean float noise of exact ratios (21.37 rather than 21.370000000000001).
        x = Value::Float((v * 1e9).round() / 1e9);
    }
    if is_bool {
        x = x.truthy().map_or(Value::Null, Value::Bool);
    }
    (x, flags)
}

/// Encode failure: `INVALID_VALUE` before anything is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeError(pub String);

/// Encodes a commanded value (6.4, inverse order).
pub fn encode(vd: &ValueDef, is_bool: bool, v: &Value, env: &dyn ExprEnv) -> Result<Value, EncodeError> {
    let t: &Transforms = &vd.transforms;
    let err = |s: String| EncodeError(s);
    if let (Some(n), true) = (v.as_f64(), !matches!(v, Value::Bool(_) | Value::Str(_))) {
        if vd.limits.min.is_some_and(|m| n < m - 1e-9) || vd.limits.max.is_some_and(|m| n > m + 1e-9) {
            return Err(err(format!("{n} outside {:?}..{:?}", vd.limits.min, vd.limits.max)));
        }
    }
    let mut x = v.clone();
    if !t.map.is_empty() {
        let label = x.as_str().ok_or_else(|| err(format!("option expected, got {x:?}")))?;
        let (raw, _) = t.map.iter().find(|(_, l)| l == label).ok_or_else(|| err(format!("unknown option {label:?}")))?;
        x = Value::Int(*raw);
    } else if t.value_on.is_some() || t.value_off.is_some() {
        let b = x.truthy().ok_or_else(|| err("bool expected".into()))?;
        x = if b { t.value_on.clone() } else { t.value_off.clone() }.ok_or_else(|| err("no raw value for that state".into()))?;
    } else if is_bool {
        x = Value::Bool(x.truthy().ok_or_else(|| err("bool expected".into()))?);
    }
    if let Some(c) = &t.codec {
        x = codec::encode(c, &x).map_err(|e| err(e.0))?;
    }
    if t.mul.is_some() || t.div.is_some() || t.offset.is_some() {
        let n = x.as_f64().filter(|_| !matches!(x, Value::Bool(_))).ok_or_else(|| err("number expected".into()))?;
        let r = (n - t.offset.unwrap_or(0.0)) * t.div.unwrap_or(1.0) / t.mul.unwrap_or(1.0);
        x = Value::Float(r);
    }
    if let Some(to) = &t.to {
        x = to.eval(&ExprCtx::with_x(x, env));
        if x.is_null() {
            return Err(err("`to` expression failed".into()));
        }
    }
    if let Value::Float(f) = x {
        let r = f.round();
        if (f - r).abs() < 1e-6 {
            x = Value::Int(r as i64);
        }
    }
    Ok(x)
}

/// `true` when a platform's own value is a bool.
pub fn bool_platform(p: Platform) -> bool {
    p.is_bool()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::EmptyEnv;
    use crate::model::{Binding, Limits, ZclBinding, ZclWrite};

    fn vd(t: Transforms) -> ValueDef {
        ValueDef {
            binding: Binding::Zcl(ZclBinding { cluster: 0x0402, attr: 0x0410, ty: ZclType::I16, mfr: None, write: ZclWrite::Attribute }),
            transforms: t,
            limits: Limits { min: Some(-50.0), max: Some(50.0), step: Some(0.1), mode: None },
            ..Default::default()
        }
    }

    #[test]
    fn spec_6_5_example() {
        let v = vd(Transforms { div: Some(10.0), ..Default::default() });
        let none = |_: u16, _: u16| None;
        assert_eq!(decode(&v, false, Value::Int(-15), &EmptyEnv, &none).0, Value::Float(-1.5));
        assert_eq!(encode(&v, false, &Value::Float(2.3), &EmptyEnv).unwrap(), Value::Int(23));
        assert!(encode(&v, false, &Value::Float(51.0), &EmptyEnv).is_err());
        assert_eq!(to_zcl(ZclType::I16, &Value::Int(23)).unwrap(), ZclValue::Int(23));
        assert!(to_zcl(ZclType::U8, &Value::Int(300)).is_err());
    }

    #[test]
    fn map_bool_invalid_precision() {
        let none = |_: u16, _: u16| None;
        let m = vd(Transforms { map: vec![(1, "low".into()), (2, "high".into())], ..Default::default() });
        assert_eq!(decode(&m, false, Value::Int(2), &EmptyEnv, &none).0, Value::Str("high".into()));
        let (v, f) = decode(&m, false, Value::Int(9), &EmptyEnv, &none);
        assert!(v.is_null() && f.map_miss);
        assert_eq!(encode(&m, false, &Value::Str("low".into()), &EmptyEnv).unwrap(), Value::Int(1));
        assert!(encode(&m, false, &Value::Str("mid".into()), &EmptyEnv).is_err());
        let b = vd(Transforms { value_on: Some(Value::Int(1)), value_off: Some(Value::Int(0)), ..Default::default() });
        assert_eq!(decode(&b, true, Value::Int(1), &EmptyEnv, &none).0, Value::Bool(true));
        assert_eq!(encode(&b, true, &Value::Bool(false), &EmptyEnv).unwrap(), Value::Int(0));
        let inv = vd(Transforms { invalid: Some(-32768.0), div: Some(100.0), ..Default::default() });
        assert!(decode(&inv, false, Value::Int(-32768), &EmptyEnv, &none).0.is_null());
        assert_eq!(decode(&inv, false, Value::Int(2137), &EmptyEnv, &none).0, Value::Float(21.37));
        let p = vd(Transforms { mul: Some(360.0), div: Some(254.0), precision: Some(1), ..Default::default() });
        assert_eq!(decode(&p, false, Value::Int(127), &EmptyEnv, &none).0, Value::Float(180.0));
        let sc = vd(Transforms {
            attr_scale: Some(crate::model::AttrScale { cluster: 0x0B04, mul_attr: Some(1), div_attr: Some(2), factor: 1.0 }),
            ..Default::default()
        });
        let scale = |_: u16, a: u16| if a == 1 { Some(1.0) } else { Some(1000.0) };
        assert_eq!(decode(&sc, false, Value::Int(523), &EmptyEnv, &scale).0, Value::Float(0.523));
    }
}
