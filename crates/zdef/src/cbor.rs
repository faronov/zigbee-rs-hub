//! Minimal deterministic CBOR (RFC 8949) reader and writer.
//!
//! Only what the definition format needs (spec 8.1): unsigned and negative
//! integers, byte and text strings, arrays, maps, tags, simple values,
//! booleans, null and floats. The writer always produces the core
//! deterministic encoding (shortest heads, definite lengths, shortest exact
//! float). The reader rejects everything that is not in that encoding:
//! non-shortest heads, indefinite lengths, non-shortest floats, unsorted or
//! duplicate map keys (checked by the callers through [`KeyOrder`]).

use alloc::vec::Vec;

/// Nesting limit of the reader (definitions are shallow; this bounds recursion).
pub const MAX_DEPTH: u8 = 24;

/// A decoding error with the byte offset where it was detected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CborError {
    pub offset: usize,
    pub kind: CborErrorKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CborErrorKind {
    /// Input ended inside an item.
    Eof,
    /// Additional information 28..30 or a reserved encoding.
    Malformed,
    /// Indefinite-length item (not allowed in the deterministic encoding).
    Indefinite,
    /// A head or float that is not in its shortest form.
    NotShortest,
    /// Map keys not in ascending encoded order, or duplicated.
    KeyOrder,
    /// The item has a different major type than expected.
    UnexpectedType,
    /// A text string that is not valid UTF-8.
    Utf8,
    /// Integer outside the range of the target type.
    Range,
    /// Nesting deeper than [`MAX_DEPTH`].
    Depth,
    /// Bytes left after the top-level item.
    Trailing,
}

/// Major type and argument of a decoded head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Head {
    Uint(u64),
    /// Negative integer `-1 - n`.
    Nint(u64),
    Bytes(u64),
    Text(u64),
    Array(u64),
    Map(u64),
    Tag(u64),
    /// Simple value (major 7, not false/true/null/undefined/float).
    Simple(u8),
    False,
    True,
    Null,
    Undefined,
    F16(u16),
    F32(u32),
    F64(u64),
}

/// Strict reader over a byte slice.
#[derive(Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    pub fn remaining(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    pub fn err(&self, kind: CborErrorKind) -> CborError {
        CborError {
            offset: self.pos,
            kind,
        }
    }

    /// Fails unless the whole input has been consumed.
    pub fn finish(&self) -> Result<(), CborError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(self.err(CborErrorKind::Trailing))
        }
    }

    fn byte(&mut self) -> Result<u8, CborError> {
        let b = *self.buf.get(self.pos).ok_or(self.err(CborErrorKind::Eof))?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CborError> {
        if self.buf.len() - self.pos < n {
            return Err(self.err(CborErrorKind::Eof));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn be(&mut self, n: usize) -> Result<u64, CborError> {
        let s = self.take(n)?;
        Ok(s.iter().fold(0u64, |a, &b| (a << 8) | b as u64))
    }

    /// Peeks at the next head without consuming it.
    pub fn peek(&self) -> Result<Head, CborError> {
        self.clone().head()
    }

    /// Reads one head (strict: shortest argument encoding, definite lengths).
    pub fn head(&mut self) -> Result<Head, CborError> {
        let start = self.pos;
        let ib = self.byte()?;
        let major = ib >> 5;
        let ai = ib & 0x1f;
        if major == 7 {
            return match ai {
                0..=19 => Ok(Head::Simple(ai)),
                20 => Ok(Head::False),
                21 => Ok(Head::True),
                22 => Ok(Head::Null),
                23 => Ok(Head::Undefined),
                24 => {
                    let v = self.byte()?;
                    if v < 32 {
                        Err(CborError {
                            offset: start,
                            kind: CborErrorKind::NotShortest,
                        })
                    } else {
                        Ok(Head::Simple(v))
                    }
                }
                25 => Ok(Head::F16(self.be(2)? as u16)),
                26 => Ok(Head::F32(self.be(4)? as u32)),
                27 => Ok(Head::F64(self.be(8)?)),
                31 => Err(CborError {
                    offset: start,
                    kind: CborErrorKind::Indefinite,
                }),
                _ => Err(CborError {
                    offset: start,
                    kind: CborErrorKind::Malformed,
                }),
            };
        }
        let arg = match ai {
            0..=23 => ai as u64,
            24 => {
                let v = self.be(1)?;
                if v < 24 {
                    return Err(CborError {
                        offset: start,
                        kind: CborErrorKind::NotShortest,
                    });
                }
                v
            }
            25 => {
                let v = self.be(2)?;
                if v <= 0xff {
                    return Err(CborError {
                        offset: start,
                        kind: CborErrorKind::NotShortest,
                    });
                }
                v
            }
            26 => {
                let v = self.be(4)?;
                if v <= 0xffff {
                    return Err(CborError {
                        offset: start,
                        kind: CborErrorKind::NotShortest,
                    });
                }
                v
            }
            27 => {
                let v = self.be(8)?;
                if v <= 0xffff_ffff {
                    return Err(CborError {
                        offset: start,
                        kind: CborErrorKind::NotShortest,
                    });
                }
                v
            }
            31 => {
                return Err(CborError {
                    offset: start,
                    kind: CborErrorKind::Indefinite,
                });
            }
            _ => {
                return Err(CborError {
                    offset: start,
                    kind: CborErrorKind::Malformed,
                });
            }
        };
        Ok(match major {
            0 => Head::Uint(arg),
            1 => Head::Nint(arg),
            2 => Head::Bytes(arg),
            3 => Head::Text(arg),
            4 => Head::Array(arg),
            5 => Head::Map(arg),
            _ => Head::Tag(arg),
        })
    }

    /// Unsigned integer.
    pub fn uint(&mut self) -> Result<u64, CborError> {
        let at = self.pos;
        match self.head()? {
            Head::Uint(v) => Ok(v),
            _ => Err(CborError {
                offset: at,
                kind: CborErrorKind::UnexpectedType,
            }),
        }
    }

    /// Signed integer in the i64 range.
    pub fn int(&mut self) -> Result<i64, CborError> {
        let at = self.pos;
        match self.head()? {
            Head::Uint(v) if v <= i64::MAX as u64 => Ok(v as i64),
            Head::Nint(v) if v <= i64::MAX as u64 => Ok(-1 - v as i64),
            Head::Uint(_) | Head::Nint(_) => Err(CborError {
                offset: at,
                kind: CborErrorKind::Range,
            }),
            _ => Err(CborError {
                offset: at,
                kind: CborErrorKind::UnexpectedType,
            }),
        }
    }

    pub fn bool(&mut self) -> Result<bool, CborError> {
        let at = self.pos;
        match self.head()? {
            Head::True => Ok(true),
            Head::False => Ok(false),
            _ => Err(CborError {
                offset: at,
                kind: CborErrorKind::UnexpectedType,
            }),
        }
    }

    pub fn bytes(&mut self) -> Result<&'a [u8], CborError> {
        let at = self.pos;
        match self.head()? {
            Head::Bytes(n) => self.take(len(n, at)?),
            _ => Err(CborError {
                offset: at,
                kind: CborErrorKind::UnexpectedType,
            }),
        }
    }

    pub fn text(&mut self) -> Result<&'a str, CborError> {
        let at = self.pos;
        match self.head()? {
            Head::Text(n) => {
                let s = self.take(len(n, at)?)?;
                core::str::from_utf8(s).map_err(|_| CborError {
                    offset: at,
                    kind: CborErrorKind::Utf8,
                })
            }
            _ => Err(CborError {
                offset: at,
                kind: CborErrorKind::UnexpectedType,
            }),
        }
    }

    pub fn array(&mut self) -> Result<usize, CborError> {
        let at = self.pos;
        match self.head()? {
            Head::Array(n) => len(n, at),
            _ => Err(CborError {
                offset: at,
                kind: CborErrorKind::UnexpectedType,
            }),
        }
    }

    pub fn map(&mut self) -> Result<usize, CborError> {
        let at = self.pos;
        match self.head()? {
            Head::Map(n) => len(n, at),
            _ => Err(CborError {
                offset: at,
                kind: CborErrorKind::UnexpectedType,
            }),
        }
    }

    /// Reads a float in its shortest exact form (f16, f32 or f64).
    pub fn float_head(&mut self, h: Head, at: usize) -> Result<f64, CborError> {
        let v = match h {
            Head::F16(b) => f16_to_f64(b),
            Head::F32(b) => {
                let v = f32::from_bits(b) as f64;
                if f16_from_f64(v).is_some() {
                    return Err(CborError {
                        offset: at,
                        kind: CborErrorKind::NotShortest,
                    });
                }
                v
            }
            Head::F64(b) => {
                let v = f64::from_bits(b);
                if f16_from_f64(v).is_some() || f32_exact(v) {
                    return Err(CborError {
                        offset: at,
                        kind: CborErrorKind::NotShortest,
                    });
                }
                v
            }
            _ => {
                return Err(CborError {
                    offset: at,
                    kind: CborErrorKind::UnexpectedType,
                });
            }
        };
        Ok(v)
    }

    /// Skips one complete item and returns its encoded bytes.
    pub fn skip(&mut self) -> Result<&'a [u8], CborError> {
        let start = self.pos;
        self.skip_depth(0)?;
        Ok(&self.buf[start..self.pos])
    }

    fn skip_depth(&mut self, depth: u8) -> Result<(), CborError> {
        if depth > MAX_DEPTH {
            return Err(self.err(CborErrorKind::Depth));
        }
        let at = self.pos;
        match self.head()? {
            Head::Bytes(n) => {
                self.take(len(n, at)?)?;
            }
            Head::Text(n) => {
                let s = self.take(len(n, at)?)?;
                core::str::from_utf8(s).map_err(|_| CborError {
                    offset: at,
                    kind: CborErrorKind::Utf8,
                })?;
            }
            Head::Array(n) => {
                for _ in 0..n {
                    self.skip_depth(depth + 1)?;
                }
            }
            Head::Map(n) => {
                let mut order = KeyOrder::new();
                for _ in 0..n {
                    let k0 = self.pos;
                    self.skip_depth(depth + 1)?;
                    order.check(&self.buf[k0..self.pos], k0)?;
                    self.skip_depth(depth + 1)?;
                }
            }
            Head::Tag(_) => self.skip_depth(depth + 1)?,
            h @ (Head::F16(_) | Head::F32(_) | Head::F64(_)) => {
                self.float_head(h, at)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Validates that `buf` is exactly one well-formed deterministic item.
    pub fn validate(buf: &[u8]) -> Result<(), CborError> {
        let mut r = Reader::new(buf);
        r.skip()?;
        r.finish()
    }
}

fn len(n: u64, at: usize) -> Result<usize, CborError> {
    usize::try_from(n).map_err(|_| CborError {
        offset: at,
        kind: CborErrorKind::Range,
    })
}

/// Checks the deterministic order of map keys (bytewise ascending encoded keys).
pub struct KeyOrder<'k> {
    prev: Option<&'k [u8]>,
}

impl<'k> KeyOrder<'k> {
    pub fn new() -> Self {
        Self { prev: None }
    }

    pub fn check(&mut self, key: &'k [u8], at: usize) -> Result<(), CborError> {
        if let Some(p) = self.prev {
            if p >= key {
                return Err(CborError {
                    offset: at,
                    kind: CborErrorKind::KeyOrder,
                });
            }
        }
        self.prev = Some(key);
        Ok(())
    }
}

impl Default for KeyOrder<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Deterministic writer.
#[derive(Default, Clone, Debug)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    pub fn head(&mut self, major: u8, arg: u64) {
        let m = major << 5;
        if arg < 24 {
            self.buf.push(m | arg as u8);
        } else if arg <= 0xff {
            self.buf.extend_from_slice(&[m | 24, arg as u8]);
        } else if arg <= 0xffff {
            self.buf.push(m | 25);
            self.buf.extend_from_slice(&(arg as u16).to_be_bytes());
        } else if arg <= 0xffff_ffff {
            self.buf.push(m | 26);
            self.buf.extend_from_slice(&(arg as u32).to_be_bytes());
        } else {
            self.buf.push(m | 27);
            self.buf.extend_from_slice(&arg.to_be_bytes());
        }
    }

    pub fn uint(&mut self, v: u64) {
        self.head(0, v);
    }

    pub fn int(&mut self, v: i64) {
        if v >= 0 {
            self.head(0, v as u64);
        } else {
            self.head(1, (-1 - v) as u64);
        }
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.head(2, b.len() as u64);
        self.buf.extend_from_slice(b);
    }

    pub fn text(&mut self, s: &str) {
        self.head(3, s.len() as u64);
        self.buf.extend_from_slice(s.as_bytes());
    }

    pub fn array(&mut self, n: usize) {
        self.head(4, n as u64);
    }

    pub fn map(&mut self, n: usize) {
        self.head(5, n as u64);
    }

    pub fn tag(&mut self, t: u64) {
        self.head(6, t);
    }

    pub fn bool(&mut self, b: bool) {
        self.buf.push(if b { 0xf5 } else { 0xf4 });
    }

    pub fn null(&mut self) {
        self.buf.push(0xf6);
    }

    pub fn simple(&mut self, v: u8) {
        if v < 24 {
            self.buf.push(0xe0 | v);
        } else {
            self.buf.extend_from_slice(&[0xf8, v]);
        }
    }

    /// Shortest exact IEEE float (f16, f32, f64).
    pub fn float(&mut self, v: f64) {
        if let Some(h) = f16_from_f64(v) {
            self.buf.push(0xf9);
            self.buf.extend_from_slice(&h.to_be_bytes());
        } else if f32_exact(v) {
            self.buf.push(0xfa);
            self.buf
                .extend_from_slice(&(v as f32).to_bits().to_be_bytes());
        } else {
            self.buf.push(0xfb);
            self.buf.extend_from_slice(&v.to_bits().to_be_bytes());
        }
    }

    /// Appends an already encoded item.
    pub fn raw(&mut self, item: &[u8]) {
        self.buf.extend_from_slice(item);
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// Length in bytes of the shortest float encoding of `v`.
pub fn float_len(v: f64) -> usize {
    if f16_from_f64(v).is_some() {
        3
    } else if f32_exact(v) {
        5
    } else {
        9
    }
}

fn f32_exact(v: f64) -> bool {
    let f = v as f32;
    (f as f64) == v || (v.is_nan() && f.is_nan())
}

/// Half-precision bits if `v` is exactly representable as f16.
pub fn f16_from_f64(v: f64) -> Option<u16> {
    if v.is_nan() {
        return Some(0x7e00);
    }
    let f = v as f32;
    if (f as f64) != v {
        return None;
    }
    let bits = f.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let man = bits & 0x7f_ffff;
    if exp == 0xff {
        return Some(sign | 0x7c00); // infinity (NaN handled above)
    }
    if exp == 0 && man == 0 {
        return Some(sign);
    }
    let e = exp - 127; // unbiased
    if (-14..=15).contains(&e) {
        // normal f16: 10 mantissa bits
        if man & 0x1fff != 0 {
            return None;
        }
        return Some(sign | (((e + 15) as u16) << 10) | (man >> 13) as u16);
    }
    if (-24..-14).contains(&e) {
        // subnormal f16: value = m * 2^-24
        let full = man | 0x80_0000; // 24 bits, value = full * 2^(e-23)
        let shift = (-14 - e) + 13; // bits to drop
        if full & ((1 << shift) - 1) != 0 {
            return None;
        }
        return Some(sign | (full >> shift) as u16);
    }
    None
}

pub fn f16_to_f64(h: u16) -> f64 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = ((h >> 10) & 0x1f) as i32;
    let man = (h & 0x3ff) as f64;
    let mag = if exp == 0 {
        man * libm::pow(2.0, -24.0)
    } else if exp == 31 {
        if man == 0.0 { f64::INFINITY } else { f64::NAN }
    } else {
        (1.0 + man / 1024.0) * libm::pow(2.0, (exp - 15) as f64)
    };
    sign * mag
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heads_shortest() {
        let mut w = Writer::new();
        w.uint(23);
        w.uint(24);
        w.uint(256);
        w.int(-1);
        w.int(-1000);
        assert_eq!(
            w.buf,
            [0x17, 0x18, 0x18, 0x19, 0x01, 0x00, 0x20, 0x39, 0x03, 0xe7]
        );
        assert_eq!(
            Reader::new(&[0x18, 0x05]).uint().unwrap_err().kind,
            CborErrorKind::NotShortest
        );
        assert_eq!(
            Reader::new(&[0x9f]).array().unwrap_err().kind,
            CborErrorKind::Indefinite
        );
    }

    #[test]
    fn floats() {
        for (v, n) in [
            (0.5, 3),
            (1.5, 3),
            (0.1, 9),
            (65504.0, 3),
            (1e-6, 9),
            (3.4028234663852886e38, 5),
            (5.960464477539063e-8, 3),
        ] {
            let mut w = Writer::new();
            w.float(v);
            assert_eq!(w.len(), n, "{v}");
            let mut r = Reader::new(&w.buf);
            let h = r.head().unwrap();
            assert_eq!(r.float_head(h, 0).unwrap(), v);
        }
        // f32 encoding of a value that fits f16 is rejected
        let mut r = Reader::new(&[0xfa, 0x3f, 0x00, 0x00, 0x00]);
        let h = r.head().unwrap();
        assert_eq!(
            r.float_head(h, 0).unwrap_err().kind,
            CborErrorKind::NotShortest
        );
    }

    #[test]
    fn key_order() {
        // {1: 0, 0: 0} unsorted
        assert_eq!(
            Reader::validate(&[0xa2, 0x01, 0x00, 0x00, 0x00])
                .unwrap_err()
                .kind,
            CborErrorKind::KeyOrder
        );
        assert!(Reader::validate(&[0xa2, 0x00, 0x00, 0x01, 0x00]).is_ok());
    }
}
