//! Device registry persistence in the host store (spec device-support
//! 16.1: the registry lives in the 128 KiB host store journal in
//! `hubdata`).
//!
//! Keys: `dev/<device id>` → deflated JSON [`DeviceSnapshot`];
//! `udef/<definition id>` → zdef CBOR of a user definition. A device is
//! written when the engine reports it added or changed, and in a sweep
//! every [`SWEEP_MS`]; the store skips identical values, and a snapshot
//! holds no measured values, so reports alone never write flash.

use std::collections::{BTreeMap, BTreeSet};

use hub_devices::device::Millis;
use hub_devices::hub::{Hub as Engine, HubEvent, device_of_uid};
use hub_devices::model::LayerSource;
use hub_devices::persist::DeviceSnapshot;
use hub_devices::transport::ZigbeeTransport;
use hub_devices::zdef_adapter::{ZdefSchemaDecoder, load_cbor};
use hub_ncp::Hub as Ncp;
use log::{info, warn};

/// Device key prefix.
pub const DEV: &str = "dev/";
/// User definition key prefix.
pub const UDEF: &str = "udef/";
/// Full sweep period.
pub const SWEEP_MS: Millis = 30_000;
/// Coalescing delay of event-driven writes.
pub const FLUSH_MS: Millis = 2_000;

/// Snapshot bytes as stored.
pub fn encode(s: &DeviceSnapshot) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec(&s.to_bytes(), 6)
}

/// Stored bytes → snapshot.
pub fn decode(b: &[u8]) -> Result<DeviceSnapshot, String> {
    let json = miniz_oxide::inflate::decompress_to_vec_with_limit(b, 256 * 1024).map_err(|e| format!("inflate: {e:?}"))?;
    DeviceSnapshot::from_bytes(&json)
}

type Records = Vec<(String, Vec<u8>)>;

/// Registry bookkeeping.
#[derive(Debug, Default)]
pub struct Registry {
    dirty: BTreeSet<String>,
    removed: BTreeSet<String>,
    dirty_since: Option<Millis>,
    last_sweep: Millis,
    /// Devices restored at start.
    pub restored: usize,
    /// Store write failures.
    pub errors: u64,
    /// Values written.
    pub writes: u64,
}

impl Registry {
    /// Restores user definitions, then devices, from the store.
    pub fn load<T: ZigbeeTransport>(&mut self, ncp: &Ncp, engine: &mut Engine<T>, now: Millis) {
        let (udefs, devs): (Records, Records) = ncp.with_store(|s| {
            (
                s.kv_prefix(UDEF).map(|(k, v)| (k.to_string(), v.to_vec())).collect(),
                s.kv_prefix(DEV).map(|(k, v)| (k.to_string(), v.to_vec())).collect(),
            )
        });
        for (key, cbor) in udefs {
            match load_cbor(&cbor, LayerSource::User) {
                Ok(d) => {
                    if let Err(e) = engine.upload_user_definition(d, now) {
                        warn!("registry: {key}: {}", e.message);
                    }
                }
                Err(e) => warn!("registry: {key}: {e:?}"),
            }
        }
        for (key, bytes) in devs {
            match decode(&bytes).and_then(|s| engine.restore_device(s, Some(&ZdefSchemaDecoder), now)) {
                Ok(()) => self.restored += 1,
                Err(e) => warn!("registry: {key}: {e}"),
            }
        }
        self.last_sweep = now;
        info!("registry: restored {} devices", self.restored);
    }

    /// Notes engine events (before they go to the API).
    pub fn note(&mut self, events: &[HubEvent], now: Millis) {
        for ev in events {
            let id = match ev {
                HubEvent::DeviceAdded(id) | HubEvent::DeviceUpdated(id) => id.clone(),
                HubEvent::ResolutionChanged { device, .. } | HubEvent::InterviewProgress { device, .. } => device.clone(),
                HubEvent::EntityAdded(uid) | HubEvent::EntityUpdated(uid) => device_of_uid(uid).to_string(),
                HubEvent::DeviceRemoved(id) => {
                    self.dirty.remove(id);
                    self.removed.insert(id.clone());
                    self.dirty_since.get_or_insert(now);
                    continue;
                }
                _ => continue,
            };
            self.removed.remove(&id);
            self.dirty.insert(id);
            self.dirty_since.get_or_insert(now);
        }
    }

    /// Writes what changed: coalesced event-driven writes, and a sweep of
    /// every device and user definition every [`SWEEP_MS`].
    pub fn flush<T: ZigbeeTransport>(&mut self, ncp: &Ncp, engine: &Engine<T>, now: Millis, force: bool) {
        let sweep = force || now.saturating_sub(self.last_sweep) >= SWEEP_MS;
        let due = force || self.dirty_since.is_some_and(|t| now.saturating_sub(t) >= FLUSH_MS);
        if !sweep && !due {
            return;
        }
        let ids: Vec<String> = if sweep {
            engine.devices().map(|d| d.id.clone()).collect()
        } else {
            std::mem::take(&mut self.dirty).into_iter().collect()
        };
        let mut puts: Vec<(String, Vec<u8>)> = ids
            .iter()
            .filter_map(|id| engine.snapshot_device(id).map(|s| (format!("{DEV}{id}"), encode(&s))))
            .collect();
        let mut dels: Vec<String> = std::mem::take(&mut self.removed).into_iter().map(|id| format!("{DEV}{id}")).collect();
        if sweep {
            self.dirty.clear();
            self.last_sweep = now;
            let defs: BTreeMap<String, Vec<u8>> = engine
                .user_definitions()
                .defs
                .iter()
                .filter_map(|d| Some((format!("{UDEF}{}", d.id), d.zdef.as_ref()?.encode().ok()?)))
                .collect();
            let live: BTreeSet<String> = engine.devices().map(|d| format!("{DEV}{}", d.id)).collect();
            ncp.with_store(|s| {
                dels.extend(s.kv_prefix(UDEF).filter(|(k, _)| !defs.contains_key(*k)).map(|(k, _)| k.to_string()));
                dels.extend(s.kv_prefix(DEV).filter(|(k, _)| !live.contains(*k)).map(|(k, _)| k.to_string()));
            });
            puts.extend(defs);
        }
        self.dirty_since = None;
        ncp.with_store(|s| {
            for (k, v) in &puts {
                if s.kv_get(k) == Some(v.as_slice()) {
                    continue;
                }
                match s.kv_put(k, v) {
                    Ok(()) => self.writes += 1,
                    Err(e) => {
                        self.errors += 1;
                        warn!("registry: {k}: {e}");
                    }
                }
            }
            for k in &dels {
                if let Err(e) = s.kv_delete(k) {
                    self.errors += 1;
                    warn!("registry: delete {k}: {e}");
                }
            }
        });
    }
}
