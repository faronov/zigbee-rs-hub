//! A compact JSON text writer for API results.
//!
//! A `serde_json::Value` object costs a B-tree node (about 700 bytes on the
//! S3) plus an allocation per key: a result of a few dozen objects needs
//! tens of kilobytes of heap, more than a hub without PSRAM has (the
//! diagnostics result ran it out of memory). Results are written as text
//! instead, straight into the result frame, with fallible allocation: a
//! result that does not fit reports `no_space` instead of aborting the
//! firmware.

use std::fmt::{Display, Write as _};

use hub_devices::hub::ApiError;

/// The writer. Values after [`key`](Self::key) or inside arrays get their
/// commas automatically.
pub struct Jw {
    s: String,
    comma: bool,
    after_key: bool,
    oom: bool,
}

impl Jw {
    /// A result frame for request `id`, `capacity` bytes reserved up front;
    /// the result value follows.
    pub fn result(id: u64, capacity: usize) -> Result<Self, ApiError> {
        let mut s = String::new();
        s.try_reserve_exact(capacity.max(64)).map_err(|_| oom())?;
        let _ = write!(s, "{{\"id\":{id},\"type\":\"result\",\"success\":true,\"result\":");
        Ok(Self { s, comma: false, after_key: true, oom: false })
    }

    /// A bare JSON value (no frame around it), `capacity` bytes reserved.
    pub fn value(capacity: usize) -> Result<Self, ApiError> {
        let mut s = String::new();
        s.try_reserve_exact(capacity.max(64)).map_err(|_| oom())?;
        Ok(Self { s, comma: false, after_key: true, oom: false })
    }

    /// The value's text (see [`value`](Self::value)).
    pub fn finish_value(self) -> Result<String, ApiError> {
        if self.oom { Err(oom()) } else { Ok(self.s) }
    }

    fn push(&mut self, t: &str) {
        if self.s.capacity() - self.s.len() < t.len() && self.s.try_reserve(t.len().max(self.s.len() / 2)).is_err() {
            self.oom = true;
            return;
        }
        self.s.push_str(t);
    }

    fn sep(&mut self) {
        if !self.after_key && self.comma {
            self.push(",");
        }
        self.after_key = false;
    }

    /// `"k":` (the value follows).
    pub fn key(&mut self, k: &str) -> &mut Self {
        if self.comma {
            self.push(",");
        }
        self.string(k);
        self.push(":");
        self.after_key = true;
        self
    }

    /// `{`.
    pub fn obj(&mut self) -> &mut Self {
        self.sep();
        self.push("{");
        self.comma = false;
        self
    }

    /// `}`.
    pub fn end_obj(&mut self) -> &mut Self {
        self.push("}");
        self.comma = true;
        self
    }

    /// `[`.
    pub fn arr(&mut self) -> &mut Self {
        self.sep();
        self.push("[");
        self.comma = false;
        self
    }

    /// `]`.
    pub fn end_arr(&mut self) -> &mut Self {
        self.push("]");
        self.comma = true;
        self
    }

    fn string(&mut self, v: &str) {
        self.push("\"");
        let mut start = 0;
        for (i, c) in v.char_indices() {
            let esc = match c {
                '"' => "\\\"",
                '\\' => "\\\\",
                '\n' => "\\n",
                c if (c as u32) < 0x20 => "",
                _ => continue,
            };
            self.push(&v[start..i]);
            if esc.is_empty() {
                let mut u = String::new();
                let _ = write!(u, "\\u{:04x}", c as u32);
                self.push(&u);
            } else {
                self.push(esc);
            }
            start = i + c.len_utf8();
        }
        self.push(&v[start..]);
        self.push("\"");
    }

    /// A string value.
    pub fn str(&mut self, v: &str) -> &mut Self {
        self.sep();
        self.string(v);
        self.comma = true;
        self
    }

    /// A number (or anything whose `Display` is valid JSON: `bool`).
    pub fn num(&mut self, v: impl Display) -> &mut Self {
        self.sep();
        let mut t = String::new();
        let _ = write!(t, "{v}");
        self.push(&t);
        self.comma = true;
        self
    }

    /// `null`.
    pub fn null(&mut self) -> &mut Self {
        self.raw("null")
    }

    /// A value already in JSON form.
    pub fn raw(&mut self, json: &str) -> &mut Self {
        self.sep();
        self.push(json);
        self.comma = true;
        self
    }

    /// `"k": v` for an optional string (null when absent).
    pub fn opt_str(&mut self, k: &str, v: Option<&str>) -> &mut Self {
        self.key(k);
        match v {
            Some(v) => self.str(v),
            None => self.null(),
        }
    }

    /// The frame text; `no_space` if memory ran out on the way.
    pub fn finish(mut self) -> Result<String, ApiError> {
        self.push("}");
        if self.oom { Err(oom()) } else { Ok(self.s) }
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.s.len()
    }

    /// Nothing written yet besides the frame head.
    pub fn is_empty(&self) -> bool {
        self.s.is_empty()
    }
}

/// The error for results too large for the hub's free memory.
pub fn oom() -> ApiError {
    ApiError::new("no_space", "not enough free memory on the hub for this result")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value as J, json};

    #[test]
    fn a_result_too_large_is_an_error_not_an_abort() {
        assert_eq!(Jw::result(1, usize::MAX / 2).err().map(|e| e.code), Some("no_space".to_string()));
    }

    #[test]
    fn writes_valid_compact_json() {
        let mut w = Jw::result(7, 8).unwrap();
        w.obj().key("a").num(1).key("s").str("q\"\\\n\u{1}é").key("l").arr();
        for i in 0..3 {
            w.obj().key("i").num(i).key("t").num(i % 2 == 0).end_obj();
        }
        w.end_arr().opt_str("n", None).key("e").arr().end_arr().key("r").raw("{\"x\":[1]}").end_obj();
        let text = w.finish().unwrap();
        let v: J = serde_json::from_str(&text).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["success"], true);
        assert_eq!(
            v["result"],
            json!({"a": 1, "s": "q\"\\\n\u{1}é", "l": [{"i": 0, "t": true}, {"i": 1, "t": false}, {"i": 2, "t": true}], "n": null, "e": [], "r": {"x": [1]}})
        );
    }
}
