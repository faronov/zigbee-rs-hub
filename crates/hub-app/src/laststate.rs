//! The last known values of every device (entity states, `last_seen`,
//! LQI, RSSI) in the host store, so that a restart does not blank the
//! dashboard: `st/<device id>` → the engine's
//! [`last_values`](hub_devices::hub::Hub::last_values) text.
//!
//! Wear: a device whose states changed is written at most once per
//! [`SAVE_MS`]; one whose only news is diagnostic (`last_seen`, LQI, RSSI:
//! every frame changes them) once per [`SLOW_MS`]; everything on a clean
//! reboot or stop. The store skips identical values and its journal spreads
//! the writes over the `hubdata` sectors. A write is one journal record, so
//! a crash leaves the previous one.

use std::collections::BTreeSet;

use hub_devices::device::Millis;
use hub_devices::hub::{Hub as Engine, HubEvent, device_of_uid};
use hub_devices::model::Category;
use hub_devices::transport::ZigbeeTransport;
use hub_ncp::Hub as Ncp;
use log::{info, warn};

/// Key prefix.
pub const KEY: &str = "st/";
/// A device whose states changed is written at most this often.
pub const SAVE_MS: Millis = 15 * 60_000;
/// A device with only diagnostic news (`last_seen`, LQI, RSSI).
pub const SLOW_MS: Millis = 6 * 3600_000;

/// Bookkeeping of the kept values.
#[derive(Debug, Default)]
pub struct LastValues {
    dirty: BTreeSet<String>,
    slow: BTreeSet<String>,
    removed: BTreeSet<String>,
    last_save: Millis,
    last_slow: Millis,
    /// States restored at start.
    pub restored: usize,
    /// Records written, bytes written, store errors.
    pub writes: u64,
    /// See `writes`.
    pub bytes: u64,
    /// See `writes`.
    pub errors: u64,
}

impl LastValues {
    /// Puts the kept values back (after the registry restored the devices).
    #[inline(never)]
    pub fn load<T: ZigbeeTransport>(&mut self, ncp: &Ncp, engine: &mut Engine<T>, now: Millis) {
        let recs: Vec<(String, Vec<u8>)> =
            ncp.with_store(|s| s.kv_prefix(KEY).map(|(k, v)| (k[KEY.len()..].to_string(), v.to_vec())).collect());
        for (id, v) in recs {
            if let Ok(t) = std::str::from_utf8(&v) {
                self.restored += engine.restore_last_values(&id, t, now);
            }
        }
        self.last_save = now;
        self.last_slow = now;
        if self.restored > 0 {
            info!("last values: {} states restored", self.restored);
        }
    }

    /// Notes engine events (before they go to the API).
    #[inline(never)]
    pub fn note<T: ZigbeeTransport>(&mut self, events: &[HubEvent], engine: &Engine<T>) {
        for ev in events {
            match ev {
                HubEvent::EntityState(s) => {
                    let Some(uid) = s.get("uid").and_then(|u| u.as_str()) else { continue };
                    let diag = engine.entity(uid).is_some_and(|(_, e)| e.r.def.category == Some(Category::Diagnostic));
                    let id = device_of_uid(uid).to_string();
                    if diag {
                        self.slow.insert(id);
                    } else {
                        self.dirty.insert(id);
                    }
                }
                HubEvent::DeviceUpdated(id) => {
                    self.slow.insert(id.clone());
                }
                HubEvent::DeviceRemoved(id) => {
                    self.dirty.remove(id);
                    self.slow.remove(id);
                    self.removed.insert(id.clone());
                }
                _ => {}
            }
        }
    }

    /// Writes the changed devices every [`SAVE_MS`] (`force`: now).
    #[inline(never)]
    pub fn flush<T: ZigbeeTransport>(&mut self, ncp: &Ncp, engine: &Engine<T>, now: Millis, force: bool) {
        let slow = force || now.saturating_sub(self.last_slow) >= SLOW_MS;
        if !slow && now.saturating_sub(self.last_save) < SAVE_MS {
            return;
        }
        self.last_save = now;
        let mut ids = std::mem::take(&mut self.dirty);
        if slow {
            self.last_slow = now;
            for id in std::mem::take(&mut self.slow) {
                ids.insert(id);
            }
        } else {
            self.slow.retain(|id| !ids.contains(id));
        }
        if ids.is_empty() && self.removed.is_empty() {
            return;
        }
        let puts: Vec<(String, String)> = ids
            .into_iter()
            .filter_map(|id| engine.last_values(&id).map(|t| (format!("{KEY}{id}"), t)))
            .collect();
        let dels: Vec<String> = std::mem::take(&mut self.removed).into_iter().map(|id| format!("{KEY}{id}")).collect();
        let (mut w, mut b, mut e) = (0, 0, 0);
        ncp.with_store(|s| {
            for (k, t) in &puts {
                if s.kv_get(k) == Some(t.as_bytes()) {
                    continue;
                }
                match s.kv_put(k, t.as_bytes()) {
                    Ok(()) => {
                        w += 1;
                        b += (k.len() + t.len()) as u64;
                    }
                    Err(_) => e += 1,
                }
            }
            for k in &dels {
                if s.kv_get(k).is_some() && s.kv_delete(k).is_err() {
                    e += 1;
                }
            }
        });
        self.writes += w;
        self.bytes += b;
        if e > 0 {
            self.errors += e;
            warn!("last values: {e} store writes failed");
        }
    }
}
