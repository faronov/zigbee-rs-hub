//! JSON objects built with out-of-line inserts: `json!` and
//! `Map::insert("k".into(), v.into())` expand to a key allocation, a
//! serialiser call and a map insert at every use, which cost the firmware
//! kilobytes; here every insert is one call.

use serde_json::{Map, Value as J};

/// Inserts `k: v`.
#[inline(never)]
pub fn put(m: &mut Map<String, J>, k: &str, v: J) {
    m.insert(k.to_string(), v);
}

/// An object under construction.
#[derive(Default)]
pub struct Obj(pub Map<String, J>);

impl Obj {
    /// An empty object.
    pub fn new() -> Self {
        Self(Map::new())
    }

    /// Adds `k: v`.
    pub fn with(mut self, k: &str, v: impl Into<J>) -> Self {
        put(&mut self.0, k, v.into());
        self
    }

    /// The value.
    pub fn j(self) -> J {
        J::Object(self.0)
    }
}
