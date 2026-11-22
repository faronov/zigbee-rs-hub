//! A small JSON text writer: records are written as text, not as
//! `serde_json::Value` trees (a `Value` object costs a B-tree node and an
//! allocation per key, too much heap for a hub without PSRAM, and its
//! construction is code-heavy).

use std::fmt::{Display, Write as _};

/// Appends `s` as a JSON string.
pub fn quote(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A JSON object being written.
#[derive(Debug, Default)]
pub struct Obj {
    s: String,
}

impl Obj {
    /// `{`.
    pub fn new() -> Self {
        let mut s = String::with_capacity(256);
        s.push('{');
        Self { s }
    }

    fn key(&mut self, k: &str) {
        if self.s.len() > 1 {
            self.s.push(',');
        }
        quote(&mut self.s, k);
        self.s.push(':');
    }

    /// A value that is JSON text already.
    pub fn raw(&mut self, k: &str, v: &str) -> &mut Self {
        self.key(k);
        self.s.push_str(v);
        self
    }

    /// A number (or anything whose `Display` is JSON).
    pub fn num(&mut self, k: &str, v: impl Display) -> &mut Self {
        self.key(k);
        let _ = write!(self.s, "{v}");
        self
    }

    /// A number or `null`.
    pub fn opt(&mut self, k: &str, v: Option<impl Display>) -> &mut Self {
        match v {
            Some(v) => self.num(k, v),
            None => self.raw(k, "null"),
        }
    }

    /// A string.
    pub fn str(&mut self, k: &str, v: &str) -> &mut Self {
        self.key(k);
        quote(&mut self.s, v);
        self
    }

    /// A string or `null`.
    pub fn ostr(&mut self, k: &str, v: Option<&str>) -> &mut Self {
        match v {
            Some(v) => self.str(k, v),
            None => self.raw(k, "null"),
        }
    }

    /// A boolean.
    pub fn bool(&mut self, k: &str, v: bool) -> &mut Self {
        self.raw(k, if v { "true" } else { "false" })
    }

    /// `}`: the text.
    pub fn end(mut self) -> String {
        self.s.push('}');
        self.s
    }
}

/// `[a,b,…]` of JSON texts.
pub fn array(items: impl IntoIterator<Item = String>) -> String {
    try_array(items).unwrap_or_else(|_| "[]".into())
}

/// The text did not fit the free heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoSpace;

/// `[a,b,…]` of JSON texts with fallible allocation (a long list on a hub
/// without PSRAM reports an error instead of aborting); only one item is
/// held besides the result.
pub fn try_array(items: impl IntoIterator<Item = String>) -> Result<String, NoSpace> {
    let mut s = String::new();
    s.try_reserve(256).map_err(|_| NoSpace)?;
    s.push('[');
    for (i, x) in items.into_iter().enumerate() {
        s.try_reserve(x.len() + 2).map_err(|_| NoSpace)?;
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x);
    }
    s.push(']');
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_valid_json() {
        let mut o = Obj::new();
        o.num("a", 1u32)
            .opt("b", None::<u8>)
            .str("c", "q\"\\\n")
            .ostr("d", None)
            .bool("e", true)
            .raw("f", "[1]");
        let t = o.end();
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"a": 1, "b": null, "c": "q\"\\\n", "d": null, "e": true, "f": [1]})
        );
        assert_eq!(array(vec![t.clone(), "2".into()]), format!("[{t},2]"));
        assert_eq!(Obj::new().end(), "{}");
    }
}

/// Decodes standard, padded base64 (RFC 4648 §4), the encoding of the
/// API's binary fields. A few dozen bytes of code instead of a base64
/// crate decoder instantiation.
pub fn base64_decode(s: &str) -> Result<Vec<u8>, &'static str> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let b = s.as_bytes();
    if !b.len().is_multiple_of(4) {
        return Err("base64: length is not a multiple of 4");
    }
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    for (i, q) in b.as_chunks::<4>().0.iter().enumerate() {
        let last = i + 1 == b.len() / 4;
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return Err("base64: misplaced padding");
        }
        let mut n = 0u32;
        for &c in &q[..4 - pad] {
            n = n << 6 | val(c).ok_or("base64: invalid character")?;
        }
        n <<= 6 * pad as u32;
        let bytes = n.to_be_bytes();
        out.extend_from_slice(&bytes[1..4 - pad]);
    }
    Ok(out)
}

#[cfg(test)]
mod b64_tests {
    use super::base64_decode as d;

    #[test]
    fn decodes_padded_base64() {
        assert_eq!(d("").unwrap(), b"");
        assert_eq!(d("Zg==").unwrap(), b"f");
        assert_eq!(d("Zm8=").unwrap(), b"fo");
        assert_eq!(d("Zm9v").unwrap(), b"foo");
        assert_eq!(d("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(d("AP/+").unwrap(), [0x00, 0xFF, 0xFE]);
        assert!(d("Zm9").is_err());
        assert!(d("Z===").is_err());
        assert!(d("Zg==Zm9v").is_err());
        assert!(d("Zm9*").is_err());
    }
}
