//! The last known values of a device (entity states, `last_seen`, LQI,
//! RSSI) as a small JSON text, for the app to keep in the host store and
//! hand back after a restart. Restored states are marked: the API shows
//! them with `restored: true` until the device reports again.
//!
//! Times are kept as Unix milliseconds, and only while the hub knows the
//! time ([`crate::device::time_synced`]): the engine's own clock restarts
//! with the hub.
//!
//! `{"seen": ms, "lqi": n, "rssi": n, "e": {"<uid after the device id>":
//! {"v": value, "p": {part: value}, "c": changed ms, "r": reported ms}}}`.

use serde_json::{Map, Value as J};

use crate::device::{Millis, time_synced, wall_ms};
use crate::expr::Value;
use crate::hub::Hub;
use crate::transport::ZigbeeTransport;

/// Engine time of a Unix time, while the hub knows the time.
fn engine_ms(unix_ms: Option<&J>, now: Millis) -> Option<Millis> {
    let u = unix_ms?.as_i64()?;
    time_synced().then(|| (u - (wall_ms(0))).clamp(0, now as i64) as Millis)
}

impl<T: ZigbeeTransport> Hub<T> {
    /// A device's last values (None: unknown device or nothing to keep).
    pub fn last_values(&self, id: &str) -> Option<String> {
        let d = self.device(id)?;
        let synced = time_synced();
        let unix = |t: Option<Millis>| t.filter(|_| synced).map(|t| J::from(wall_ms(t)));
        let mut es = Map::new();
        for e in d.entities.iter().filter(|e| !e.orphaned) {
            let s = &e.state;
            if s.value == Value::Null && s.parts.is_empty() {
                continue;
            }
            let mut o = Map::new();
            if s.value != Value::Null {
                o.insert("v".into(), s.value.to_json());
            }
            if !s.parts.is_empty() {
                o.insert(
                    "p".into(),
                    s.parts
                        .iter()
                        .map(|(k, v)| (k.clone(), v.to_json()))
                        .collect::<Map<_, _>>()
                        .into(),
                );
            }
            for (k, t) in [("c", s.last_changed), ("r", s.last_reported)] {
                if let Some(t) = unix(t) {
                    o.insert(k.into(), t);
                }
            }
            let key = e
                .uid
                .strip_prefix(&d.id)
                .map_or(e.uid.as_str(), |k| k.trim_start_matches(':'));
            es.insert(key.into(), o.into());
        }
        let mut v = Map::new();
        if let Some(t) = unix(d.last_seen) {
            v.insert("seen".into(), t);
        }
        if let Some(q) = d.lqi {
            v.insert("lqi".into(), q.into());
        }
        if let Some(r) = d.rssi {
            v.insert("rssi".into(), r.into());
        }
        if es.is_empty() && v.is_empty() {
            return None;
        }
        v.insert("e".into(), es.into());
        serde_json::to_string(&v).ok()
    }

    /// Puts kept values back on a restored device: entity states not
    /// reported since start (marked `restored`), `last_seen`, LQI, RSSI.
    /// Returns the number of states restored.
    pub fn restore_last_values(&mut self, id: &str, text: &str, now: Millis) -> usize {
        let Ok(J::Object(v)) = serde_json::from_str::<J>(text) else {
            return 0;
        };
        let Some(d) = self.device_mut(id) else {
            return 0;
        };
        if let Some(t) = engine_ms(v.get("seen"), now) {
            d.last_seen = Some(d.last_seen.map_or(t, |s| s.max(t).min(now)));
        }
        if d.lqi.is_none() {
            d.lqi = v
                .get("lqi")
                .and_then(J::as_u64)
                .and_then(|q| u8::try_from(q).ok());
        }
        if d.rssi.is_none() {
            d.rssi = v
                .get("rssi")
                .and_then(J::as_i64)
                .and_then(|r| i8::try_from(r).ok());
        }
        let mut n = 0;
        let Some(J::Object(es)) = v.get("e") else {
            return 0;
        };
        let prefix = format!("{id}:");
        for e in d.entities.iter_mut() {
            let s = &mut e.state;
            let Some(J::Object(o)) = e.uid.strip_prefix(&prefix).and_then(|k| es.get(k)) else {
                continue;
            };
            if s.value != Value::Null || !s.parts.is_empty() || s.last_reported.is_some() {
                continue;
            }
            s.value = o.get("v").map_or(Value::Null, Value::from_json);
            if let Some(J::Object(p)) = o.get("p") {
                s.parts = p
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::from_json(v)))
                    .collect();
            }
            s.last_changed = engine_ms(o.get("c"), now);
            s.last_reported = engine_ms(o.get("r"), now);
            s.restored = true;
            n += 1;
        }
        n
    }
}
