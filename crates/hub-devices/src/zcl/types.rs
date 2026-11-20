//! ZCL data types (ZCL r8 chapter 2.6) with a host-side codec.
//!
//! The hub decodes every type a device may report, including the odd widths
//! (`int24`, `uint40`…) and the composite types (array, structure, set,
//! bag), and keeps strings of any length (self-description chunks are 48 B,
//! Lumi structures are longer than the 32 B of a device-side buffer).

use std::fmt;

/// A ZCL data type code (`0x10` bool, `0x21` uint16, …).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct ZclType(pub u8);

#[allow(missing_docs)]
impl ZclType {
    pub const NO_DATA: Self = Self(0x00);
    pub const DATA8: Self = Self(0x08);
    pub const BOOL: Self = Self(0x10);
    pub const MAP8: Self = Self(0x18);
    pub const MAP16: Self = Self(0x19);
    pub const MAP24: Self = Self(0x1A);
    pub const MAP32: Self = Self(0x1B);
    pub const MAP64: Self = Self(0x1F);
    pub const U8: Self = Self(0x20);
    pub const U16: Self = Self(0x21);
    pub const U24: Self = Self(0x22);
    pub const U32: Self = Self(0x23);
    pub const U40: Self = Self(0x24);
    pub const U48: Self = Self(0x25);
    pub const U56: Self = Self(0x26);
    pub const U64: Self = Self(0x27);
    pub const I8: Self = Self(0x28);
    pub const I16: Self = Self(0x29);
    pub const I24: Self = Self(0x2A);
    pub const I32: Self = Self(0x2B);
    pub const I40: Self = Self(0x2C);
    pub const I48: Self = Self(0x2D);
    pub const I56: Self = Self(0x2E);
    pub const I64: Self = Self(0x2F);
    pub const ENUM8: Self = Self(0x30);
    pub const ENUM16: Self = Self(0x31);
    pub const SEMI: Self = Self(0x38);
    pub const SINGLE: Self = Self(0x39);
    pub const DOUBLE: Self = Self(0x3A);
    pub const OCTET_STR: Self = Self(0x41);
    pub const CHAR_STR: Self = Self(0x42);
    pub const LONG_OCTET_STR: Self = Self(0x43);
    pub const LONG_CHAR_STR: Self = Self(0x44);
    pub const ARRAY: Self = Self(0x48);
    pub const STRUCT: Self = Self(0x4C);
    pub const SET: Self = Self(0x50);
    pub const BAG: Self = Self(0x51);
    pub const TOD: Self = Self(0xE0);
    pub const DATE: Self = Self(0xE1);
    pub const UTC: Self = Self(0xE2);
    pub const CLUSTER_ID: Self = Self(0xE8);
    pub const ATTR_ID: Self = Self(0xE9);
    pub const BACNET_OID: Self = Self(0xEA);
    pub const IEEE: Self = Self(0xF0);
    pub const KEY128: Self = Self(0xF1);
    pub const UNKNOWN: Self = Self(0xFF);

    /// Parses a definition type name (spec 6.3: `bool`, `u8`…`u48`,
    /// `i8`…`i32`, `enum8`, `enum16`, `map8`, `map16`, `f32`, `str`, `ostr`)
    /// and the ZCL spellings used by herdsman (`uint16`, `int8`, `bitmap8`,
    /// `single`, `octet_str`, `char_str`).
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "bool" | "boolean" => Self::BOOL,
            "u8" | "uint8" => Self::U8,
            "u16" | "uint16" => Self::U16,
            "u24" | "uint24" => Self::U24,
            "u32" | "uint32" => Self::U32,
            "u40" | "uint40" => Self::U40,
            "u48" | "uint48" => Self::U48,
            "u56" | "uint56" => Self::U56,
            "u64" | "uint64" => Self::U64,
            "i8" | "int8" => Self::I8,
            "i16" | "int16" => Self::I16,
            "i24" | "int24" => Self::I24,
            "i32" | "int32" => Self::I32,
            "i40" | "int40" => Self::I40,
            "i48" | "int48" => Self::I48,
            "i56" | "int56" => Self::I56,
            "i64" | "int64" => Self::I64,
            "enum8" => Self::ENUM8,
            "enum16" => Self::ENUM16,
            "map8" | "bitmap8" => Self::MAP8,
            "map16" | "bitmap16" => Self::MAP16,
            "map24" | "bitmap24" => Self::MAP24,
            "map32" | "bitmap32" => Self::MAP32,
            "map64" | "bitmap64" => Self::MAP64,
            "f16" | "semi" => Self::SEMI,
            "f32" | "single" => Self::SINGLE,
            "f64" | "double" => Self::DOUBLE,
            "str" | "char_str" | "string" => Self::CHAR_STR,
            "ostr" | "octet_str" => Self::OCTET_STR,
            "utc" => Self::UTC,
            "ieee" => Self::IEEE,
            _ => return None,
        })
    }

    /// Fixed wire size, `None` for variable-length types.
    pub fn fixed_size(self) -> Option<usize> {
        Some(match self.0 {
            0x00 => 0,
            0x08 | 0x10 | 0x18 | 0x20 | 0x28 | 0x30 => 1,
            0x09 | 0x19 | 0x21 | 0x29 | 0x31 | 0x38 | 0xE8 | 0xE9 => 2,
            0x0A | 0x1A | 0x22 | 0x2A => 3,
            0x0B | 0x1B | 0x23 | 0x2B | 0x39 | 0xE0 | 0xE1 | 0xE2 | 0xEA => 4,
            0x0C | 0x1C | 0x24 | 0x2C => 5,
            0x0D | 0x1D | 0x25 | 0x2D => 6,
            0x0E | 0x1E | 0x26 | 0x2E => 7,
            0x0F | 0x1F | 0x27 | 0x2F | 0x3A | 0xF0 => 8,
            0xF1 => 16,
            _ => return None,
        })
    }

    /// Unsigned integer width in bytes (uint8…uint64).
    pub fn uint_width(self) -> Option<usize> {
        (0x20..=0x27)
            .contains(&self.0)
            .then(|| usize::from(self.0 - 0x1F))
    }

    /// Signed integer width in bytes (int8…int64).
    pub fn int_width(self) -> Option<usize> {
        (0x28..=0x2F)
            .contains(&self.0)
            .then(|| usize::from(self.0 - 0x27))
    }

    /// Bitmap width in bytes (map8…map64).
    pub fn map_width(self) -> Option<usize> {
        (0x18..=0x1F)
            .contains(&self.0)
            .then(|| usize::from(self.0 - 0x17))
    }

    /// General data width in bytes (data8…data64).
    pub fn data_width(self) -> Option<usize> {
        (0x08..=0x0F)
            .contains(&self.0)
            .then(|| usize::from(self.0 - 0x07))
    }

    /// Analog types carry a Reportable Change field in Configure Reporting
    /// (ZCL r8 2.5.7.1.7): integers, floats and time/date types.
    pub fn is_analog(self) -> bool {
        matches!(self.0, 0x20..=0x2F | 0x38..=0x3A | 0xE0..=0xE2)
    }

    /// Floating-point types.
    pub fn is_float(self) -> bool {
        matches!(self.0, 0x38..=0x3A)
    }
}

impl fmt::Display for ZclType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:02X}", self.0)
    }
}

/// A decoded ZCL value.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ZclValue {
    /// No data (`0x00`).
    NoData,
    /// Boolean; `0xFF` (invalid) decodes as `None`.
    Bool(Option<bool>),
    /// Unsigned integers, bitmaps, enums, general data, IDs and UTC time.
    Uint(u64),
    /// Signed integers.
    Int(i64),
    /// Semi, single and double precision.
    Float(f64),
    /// Octet strings (also the 0xFF "invalid" length as an empty string).
    Octets(Vec<u8>),
    /// Character strings (lossy UTF-8).
    String(String),
    /// Array, set or bag: element type and elements.
    List(ZclType, Vec<ZclValue>),
    /// Structure: `(type, value)` per element.
    Struct(Vec<(ZclType, ZclValue)>),
    /// 128-bit security key.
    Key([u8; 16]),
}

impl ZclValue {
    /// The value as a signed 64-bit integer when it is integral.
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Self::Uint(v) => i64::try_from(v).ok(),
            Self::Int(v) => Some(v),
            Self::Bool(Some(b)) => Some(i64::from(b)),
            _ => None,
        }
    }

    /// The value as `f64` when it is numeric.
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Self::Uint(v) => Some(v as f64),
            Self::Int(v) => Some(v as f64),
            Self::Float(v) => Some(v),
            Self::Bool(Some(b)) => Some(f64::from(u8::from(b))),
            _ => None,
        }
    }
}

/// A value with its wire type.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TypedValue {
    /// Wire type.
    pub ty: ZclType,
    /// Value.
    pub value: ZclValue,
}

impl TypedValue {
    /// Shorthand constructor.
    pub fn new(ty: ZclType, value: ZclValue) -> Self {
        Self { ty, value }
    }
    /// An unsigned value.
    pub fn uint(ty: ZclType, v: u64) -> Self {
        Self::new(ty, ZclValue::Uint(v))
    }
    /// A signed value.
    pub fn int(ty: ZclType, v: i64) -> Self {
        Self::new(ty, ZclValue::Int(v))
    }
}

/// Codec errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// The input ended inside a field.
    Truncated,
    /// The type code is not known to this decoder.
    UnknownType(u8),
    /// A value does not fit its declared type.
    OutOfRange,
    /// Composite nesting deeper than the decoder allows.
    TooDeep,
    /// Structurally invalid content.
    Invalid,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "truncated"),
            Self::UnknownType(t) => write!(f, "unknown ZCL type 0x{t:02X}"),
            Self::OutOfRange => write!(f, "value out of range for its type"),
            Self::TooDeep => write!(f, "composite nesting too deep"),
            Self::Invalid => write!(f, "invalid content"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Little-endian reader over a byte slice.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// New reader.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    /// Remaining bytes.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    /// The unread rest.
    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }
    /// Current offset.
    pub fn pos(&self) -> usize {
        self.pos
    }
    /// Takes `n` bytes.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        if self.remaining() < n {
            return Err(CodecError::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    /// u8.
    pub fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }
    /// u16 LE.
    pub fn u16(&mut self) -> Result<u16, CodecError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    /// u32 LE.
    pub fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(self.uint_le(4)? as u32)
    }
    /// u64 LE.
    pub fn u64(&mut self) -> Result<u64, CodecError> {
        self.uint_le(8)
    }
    /// Unsigned LE integer of `n` ≤ 8 bytes.
    pub fn uint_le(&mut self, n: usize) -> Result<u64, CodecError> {
        let b = self.take(n)?;
        Ok(b.iter()
            .rev()
            .fold(0u64, |acc, &x| (acc << 8) | u64::from(x)))
    }
}

const MAX_DEPTH: usize = 4;

/// Decodes one value of type `ty` from `r`.
pub fn decode_value(ty: ZclType, r: &mut Reader<'_>) -> Result<ZclValue, CodecError> {
    decode_depth(ty, r, 0)
}

/// Decodes a value of type `ty` at the start of `buf`; returns it and the
/// number of bytes used.
pub fn decode_slice(ty: ZclType, buf: &[u8]) -> Result<(ZclValue, usize), CodecError> {
    let mut r = Reader::new(buf);
    let v = decode_value(ty, &mut r)?;
    Ok((v, r.pos()))
}

fn sext(v: u64, bytes: usize) -> i64 {
    let shift = 64 - 8 * bytes as u32;
    ((v << shift) as i64) >> shift
}

fn decode_depth(ty: ZclType, r: &mut Reader<'_>, depth: usize) -> Result<ZclValue, CodecError> {
    if depth > MAX_DEPTH {
        return Err(CodecError::TooDeep);
    }
    if let Some(n) = ty.int_width() {
        return Ok(ZclValue::Int(sext(r.uint_le(n)?, n)));
    }
    if let Some(n) = ty.uint_width().or(ty.map_width()).or(ty.data_width()) {
        return Ok(ZclValue::Uint(r.uint_le(n)?));
    }
    Ok(match ty.0 {
        0x00 => ZclValue::NoData,
        0x10 => match r.u8()? {
            0 => ZclValue::Bool(Some(false)),
            1 => ZclValue::Bool(Some(true)),
            _ => ZclValue::Bool(None),
        },
        0x30 => ZclValue::Uint(u64::from(r.u8()?)),
        0x31 | 0xE8 | 0xE9 => ZclValue::Uint(u64::from(r.u16()?)),
        0x38 => ZclValue::Float(f16_to_f64(r.u16()?)),
        0x39 => ZclValue::Float(f64::from(f32::from_bits(r.u32()?))),
        0x3A => ZclValue::Float(f64::from_bits(r.u64()?)),
        0x41 | 0x42 => {
            let n = r.u8()?;
            let n = if n == 0xFF { 0 } else { usize::from(n) };
            let b = r.take(n)?;
            if ty.0 == 0x41 {
                ZclValue::Octets(b.to_vec())
            } else {
                ZclValue::String(String::from_utf8_lossy(b).into_owned())
            }
        }
        0x43 | 0x44 => {
            let n = r.u16()?;
            let n = if n == 0xFFFF { 0 } else { usize::from(n) };
            let b = r.take(n)?;
            if ty.0 == 0x43 {
                ZclValue::Octets(b.to_vec())
            } else {
                ZclValue::String(String::from_utf8_lossy(b).into_owned())
            }
        }
        0x48 | 0x50 | 0x51 => {
            let et = ZclType(r.u8()?);
            let n = r.u16()?;
            let n = if n == 0xFFFF { 0 } else { n };
            let mut items = Vec::with_capacity(usize::from(n).min(64));
            for _ in 0..n {
                items.push(decode_depth(et, r, depth + 1)?);
            }
            ZclValue::List(et, items)
        }
        0x4C => {
            let n = r.u16()?;
            let n = if n == 0xFFFF { 0 } else { n };
            let mut items = Vec::with_capacity(usize::from(n).min(64));
            for _ in 0..n {
                let et = ZclType(r.u8()?);
                items.push((et, decode_depth(et, r, depth + 1)?));
            }
            ZclValue::Struct(items)
        }
        0xE0 | 0xE1 | 0xE2 | 0xEA => ZclValue::Uint(u64::from(r.u32()?)),
        0xF0 => ZclValue::Uint(r.u64()?),
        0xF1 => {
            let mut k = [0u8; 16];
            k.copy_from_slice(r.take(16)?);
            ZclValue::Key(k)
        }
        t => return Err(CodecError::UnknownType(t)),
    })
}

fn as_uint(value: &ZclValue) -> Result<u64, CodecError> {
    match value {
        ZclValue::Uint(v) => Ok(*v),
        ZclValue::Int(v) => u64::try_from(*v).map_err(|_| CodecError::OutOfRange),
        ZclValue::Bool(Some(b)) => Ok(u64::from(*b)),
        _ => Err(CodecError::OutOfRange),
    }
}

fn put_uint(out: &mut Vec<u8>, v: u64, n: usize) -> Result<(), CodecError> {
    if n < 8 && v >> (8 * n) != 0 {
        return Err(CodecError::OutOfRange);
    }
    out.extend_from_slice(&v.to_le_bytes()[..n]);
    Ok(())
}

/// Encodes `value` as type `ty`, appending to `out`.
pub fn encode_value(ty: ZclType, value: &ZclValue, out: &mut Vec<u8>) -> Result<(), CodecError> {
    if let Some(n) = ty.int_width() {
        let v = match value {
            ZclValue::Int(v) => *v,
            ZclValue::Uint(v) => i64::try_from(*v).map_err(|_| CodecError::OutOfRange)?,
            _ => return Err(CodecError::OutOfRange),
        };
        if n < 8 {
            let lim = 1i64 << (8 * n - 1);
            if v < -lim || v >= lim {
                return Err(CodecError::OutOfRange);
            }
        }
        out.extend_from_slice(&v.to_le_bytes()[..n]);
        return Ok(());
    }
    if let Some(n) = ty.uint_width().or(ty.map_width()).or(ty.data_width()) {
        return put_uint(out, as_uint(value)?, n);
    }
    match ty.0 {
        0x00 => Ok(()),
        0x10 => {
            let b = match value {
                ZclValue::Bool(Some(b)) => u8::from(*b),
                ZclValue::Bool(None) => 0xFF,
                other => match as_uint(other)? {
                    0 => 0,
                    1 => 1,
                    _ => return Err(CodecError::OutOfRange),
                },
            };
            out.push(b);
            Ok(())
        }
        0x30 => put_uint(out, as_uint(value)?, 1),
        0x31 | 0xE8 | 0xE9 => put_uint(out, as_uint(value)?, 2),
        0xE0 | 0xE1 | 0xE2 | 0xEA => put_uint(out, as_uint(value)?, 4),
        0xF0 => put_uint(out, as_uint(value)?, 8),
        0x38..=0x3A => {
            let f = match value {
                ZclValue::Float(f) => *f,
                ZclValue::Int(i) => *i as f64,
                ZclValue::Uint(u) => *u as f64,
                _ => return Err(CodecError::OutOfRange),
            };
            match ty.0 {
                0x38 => out.extend_from_slice(&f64_to_f16(f).to_le_bytes()),
                0x39 => out.extend_from_slice(&(f as f32).to_bits().to_le_bytes()),
                _ => out.extend_from_slice(&f.to_bits().to_le_bytes()),
            }
            Ok(())
        }
        0x41..=0x44 => {
            let bytes: &[u8] = match value {
                ZclValue::Octets(b) => b,
                ZclValue::String(s) => s.as_bytes(),
                _ => return Err(CodecError::OutOfRange),
            };
            if ty.0 <= 0x42 {
                if bytes.len() > 254 {
                    return Err(CodecError::OutOfRange);
                }
                out.push(bytes.len() as u8);
            } else {
                if bytes.len() > 0xFFFE {
                    return Err(CodecError::OutOfRange);
                }
                out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
            }
            out.extend_from_slice(bytes);
            Ok(())
        }
        0x48 | 0x50 | 0x51 => {
            let ZclValue::List(et, items) = value else {
                return Err(CodecError::OutOfRange);
            };
            out.push(et.0);
            out.extend_from_slice(&(items.len() as u16).to_le_bytes());
            for it in items {
                encode_value(*et, it, out)?;
            }
            Ok(())
        }
        0x4C => {
            let ZclValue::Struct(items) = value else {
                return Err(CodecError::OutOfRange);
            };
            out.extend_from_slice(&(items.len() as u16).to_le_bytes());
            for (et, it) in items {
                out.push(et.0);
                encode_value(*et, it, out)?;
            }
            Ok(())
        }
        0xF1 => {
            let ZclValue::Key(k) = value else {
                return Err(CodecError::OutOfRange);
            };
            out.extend_from_slice(k);
            Ok(())
        }
        t => Err(CodecError::UnknownType(t)),
    }
}

/// Integer range of an integer-like type (`None` for floats, strings…).
pub fn int_range(ty: ZclType) -> Option<(i128, i128)> {
    if let Some(n) = ty.int_width() {
        let lim = 1i128 << (8 * n - 1);
        return Some((-lim, lim - 1));
    }
    if ty == ZclType::BOOL {
        return Some((0, 1));
    }
    let n = ty
        .uint_width()
        .or(ty.map_width())
        .or(ty.data_width())
        .or(match ty.0 {
            0x30 => Some(1),
            0x31 | 0xE8 | 0xE9 => Some(2),
            0xE2 => Some(4),
            _ => None,
        })?;
    Some((0, (1i128 << (8 * n)) - 1))
}

/// IEEE 754 binary16 to f64.
pub fn f16_to_f64(h: u16) -> f64 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = (h >> 10) & 0x1F;
    let frac = f64::from(h & 0x3FF);
    match exp {
        0 => sign * frac * 2f64.powi(-24),
        0x1F => {
            if frac == 0.0 {
                sign * f64::INFINITY
            } else {
                f64::NAN
            }
        }
        e => sign * (1.0 + frac / 1024.0) * 2f64.powi(i32::from(e) - 15),
    }
}

/// f64 to IEEE 754 binary16 (round to nearest, saturating to infinity).
pub fn f64_to_f16(f: f64) -> u16 {
    if f.is_nan() {
        return 0x7E00;
    }
    let sign: u16 = if f.is_sign_negative() { 0x8000 } else { 0 };
    let a = f.abs();
    if a >= 65520.0 {
        return sign | 0x7C00;
    }
    if a < 2f64.powi(-14) {
        let m = (a / 2f64.powi(-24)).round() as u16;
        return sign | m;
    }
    let mut e = a.log2().floor() as i32;
    let mut m = ((a / 2f64.powi(e) - 1.0) * 1024.0).round() as u32;
    if m == 1024 {
        m = 0;
        e += 1;
    }
    sign | (((e + 15) as u16) << 10) | m as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(ty: ZclType, v: ZclValue) {
        let mut out = Vec::new();
        encode_value(ty, &v, &mut out).unwrap();
        if let Some(n) = ty.fixed_size() {
            assert_eq!(out.len(), n);
        }
        let (d, n) = decode_slice(ty, &out).unwrap();
        assert_eq!(n, out.len());
        assert_eq!(d, v);
    }

    #[test]
    fn integers_of_every_width() {
        rt(ZclType::I24, ZclValue::Int(-1));
        rt(ZclType::I24, ZclValue::Int(-8_388_608));
        rt(ZclType::U48, ZclValue::Uint(0xFFFF_FFFF_FFFF));
        rt(ZclType::I40, ZclValue::Int(-5));
        rt(ZclType::U8, ZclValue::Uint(0xFF));
        assert_eq!(
            decode_slice(ZclType::I16, &[0x00, 0x80]).unwrap().0,
            ZclValue::Int(-32768)
        );
        let mut out = Vec::new();
        assert_eq!(
            encode_value(ZclType::U8, &ZclValue::Uint(256), &mut out),
            Err(CodecError::OutOfRange)
        );
        assert_eq!(
            encode_value(ZclType::I8, &ZclValue::Int(128), &mut out),
            Err(CodecError::OutOfRange)
        );
    }

    #[test]
    fn strings_structs_and_floats() {
        rt(
            ZclType::CHAR_STR,
            ZclValue::String("lumi.motion.ac02".into()),
        );
        rt(ZclType::OCTET_STR, ZclValue::Octets(vec![7; 48]));
        rt(
            ZclType::STRUCT,
            ZclValue::Struct(vec![
                (ZclType::U8, ZclValue::Uint(1)),
                (ZclType::I16, ZclValue::Int(-2)),
            ]),
        );
        rt(
            ZclType::ARRAY,
            ZclValue::List(ZclType::U16, vec![ZclValue::Uint(1), ZclValue::Uint(2)]),
        );
        rt(ZclType::SINGLE, ZclValue::Float(f64::from(0.000_412_5f32)));
        rt(ZclType::SEMI, ZclValue::Float(1.5));
        assert_eq!(f16_to_f64(f64_to_f16(-2.0)), -2.0);
        assert_eq!(
            decode_slice(ZclType::CHAR_STR, &[0xFF]).unwrap().0,
            ZclValue::String(String::new())
        );
        assert_eq!(
            decode_slice(ZclType::BOOL, &[0xFF]).unwrap().0,
            ZclValue::Bool(None)
        );
    }

    #[test]
    fn names() {
        assert_eq!(ZclType::from_name("i16"), Some(ZclType::I16));
        assert_eq!(ZclType::from_name("map16"), Some(ZclType::MAP16));
        assert_eq!(ZclType::from_name("f32"), Some(ZclType::SINGLE));
        assert!(
            ZclType::U8.is_analog() && !ZclType::MAP8.is_analog() && !ZclType::ENUM8.is_analog()
        );
        assert_eq!(int_range(ZclType::I8), Some((-128, 127)));
        assert_eq!(int_range(ZclType::ENUM8), Some((0, 255)));
    }
}
