//! Device registry persistence in the host store (spec device-support
//! 16.1: the registry lives in the 128 KiB host store journal in
//! `hubdata`).
//!
//! Keys: `dev/<device id>` → JSON [`DeviceSnapshot`] (plain: a deflate
//! compressor needs ~250 KB of RAM, which a hub without PSRAM does not
//! have; deflated values written by earlier builds are still read);
//! `udef/<definition id>` → zdef CBOR of a user definition, `usrc/<id>` →
//! its source text for the editor (`{"format", "text"}`). A device is
//! written when the engine reports it added or changed, and in a sweep
//! every [`SWEEP_MS`]; the store skips identical values, and a snapshot
//! holds no measured values, so reports alone never write flash.
//! Groups and binding records (device-support 14.9): `grp/<4 hex>`,
//! `bnd/<device id>` (JSON), written when the engine reports a change.

use std::collections::{BTreeMap, BTreeSet};

use hub_devices::device::Millis;
use hub_devices::hub::zgroups::{BIND_KEY, GROUP_KEY};
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
/// Source text of a user definition (`{"format", "text"}`).
pub const USRC: &str = "usrc/";
/// Full sweep period.
pub const SWEEP_MS: Millis = 30_000;
/// Coalescing delay of event-driven writes.
pub const FLUSH_MS: Millis = 2_000;

/// Snapshot bytes as stored.
pub fn encode(s: &DeviceSnapshot) -> Vec<u8> {
    s.to_bytes()
}

/// Stored bytes → snapshot (plain JSON, or raw deflate from earlier builds).
pub fn decode(b: &[u8]) -> Result<DeviceSnapshot, String> {
    if b.first() == Some(&b'{') {
        return DeviceSnapshot::from_bytes(b);
    }
    let json =
        miniz_oxide::inflate::decompress_to_vec_with_limit(b, 256 * 1024).map_err(|e| format!("inflate: {e:?}"))?;
    DeviceSnapshot::from_bytes(&json)
}

type Records = Vec<(String, Vec<u8>)>;

/// Registry bookkeeping.
#[derive(Debug, Default)]
pub struct Registry {
    dirty: BTreeSet<String>,
    removed: BTreeSet<String>,
    dirty_since: Option<Millis>,
    /// Groups or bindings changed.
    zg_dirty: bool,
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
        let (udefs, devs, srcs): (Records, Records, Records) = ncp.with_store(|s| {
            (
                s.kv_prefix(UDEF).map(|(k, v)| (k.to_string(), v.to_vec())).collect(),
                s.kv_prefix(DEV).map(|(k, v)| (k.to_string(), v.to_vec())).collect(),
                s.kv_prefix(USRC).map(|(k, v)| (k.to_string(), v.to_vec())).collect(),
            )
        });
        // `<format>\n<text>`.
        for (key, v) in srcs {
            if let Ok(v) = String::from_utf8(v)
                && let Some((f, t)) = v.split_once('\n')
            {
                engine.user_sources.insert(key[USRC.len()..].to_string(), (f.to_string(), t.to_string()));
            }
        }
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
        let zg: Records = ncp.with_store(|s| {
            s.kv_prefix(GROUP_KEY)
                .chain(s.kv_prefix(BIND_KEY))
                .map(|(k, v)| (k.to_string(), v.to_vec()))
                .collect()
        });
        for (key, v) in zg {
            if let Err(e) = engine.zg_import(&key, &v) {
                warn!("registry: {key}: {e}");
            }
        }
        self.last_sweep = now;
        info!("registry: restored {} devices, {} groups", self.restored, engine.zg.groups.len());
    }

    /// Notes engine events (before they go to the API).
    pub fn note(&mut self, events: &[HubEvent], now: Millis) {
        for ev in events {
            let id = match ev {
                HubEvent::DeviceAdded(id) | HubEvent::DeviceUpdated(id) => id.clone(),
                HubEvent::ResolutionChanged { device, .. } | HubEvent::InterviewProgress { device, .. } => {
                    device.clone()
                }
                HubEvent::EntityAdded(uid) | HubEvent::EntityUpdated(uid) => device_of_uid(uid).to_string(),
                // An entity of a kept device (re-resolution, `entities/delete`);
                // a removed device's entities go with it.
                HubEvent::EntityRemoved(uid) if !self.removed.contains(device_of_uid(uid)) => {
                    device_of_uid(uid).to_string()
                }
                HubEvent::Groups { kind, .. } if matches!(kind.as_str(), "group_updated" | "group_removed" | "bindings_updated") => {
                    self.zg_dirty = true;
                    self.dirty_since.get_or_insert(now);
                    continue;
                }
                // Group devices are not stored as devices.
                HubEvent::DeviceRemoved(id) if id.starts_with("grp:") => continue,
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

    /// User definitions changed (upload, delete): written with the next
    /// flush, [`FLUSH_MS`] from now, instead of at the sweep (a reboot
    /// right after an upload lost it on the BR board).
    pub fn definitions_changed(&mut self, now: Millis) {
        self.last_sweep = self.last_sweep.min(now.saturating_sub(SWEEP_MS - FLUSH_MS));
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
        let mut dels: Vec<String> =
            std::mem::take(&mut self.removed).into_iter().map(|id| format!("{DEV}{id}")).collect();
        if sweep || std::mem::take(&mut self.zg_dirty) {
            let zg = engine.zg_export();
            ncp.with_store(|s| {
                dels.extend(
                    s.kv_prefix(GROUP_KEY)
                        .chain(s.kv_prefix(BIND_KEY))
                        .filter(|(k, _)| !zg.iter().any(|(x, _)| x == k))
                        .map(|(k, _)| k.to_string()),
                );
            });
            puts.extend(zg);
        }
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
            // Sources of the user definitions still stored, `<format>\n<text>`
            // (ascending ids, as the map iterates).
            let srcs: Records = engine
                .user_sources
                .iter()
                .filter(|(id, _)| defs.contains_key(&format!("{UDEF}{id}")))
                .map(|(id, (f, t))| (format!("{USRC}{id}"), format!("{f}\n{t}").into_bytes()))
                .collect();
            ncp.with_store(|s| {
                dels.extend(
                    s.kv_prefix(USRC)
                        .filter(|(k, _)| srcs.binary_search_by(|(x, _)| x.as_str().cmp(k)).is_err())
                        .map(|(k, _)| k.to_string()),
                );
                dels.extend(s.kv_prefix(UDEF).filter(|(k, _)| !defs.contains_key(*k)).map(|(k, _)| k.to_string()));
                dels.extend(s.kv_prefix(DEV).filter(|(k, _)| !live.contains(*k)).map(|(k, _)| k.to_string()));
            });
            puts.extend(defs);
            puts.extend(srcs);
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
