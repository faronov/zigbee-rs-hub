//! Catalog service (device-support 9.3, 9.4, 14.4): the hub never downloads
//! anything itself. Clients with Internet access — the web UI (from the
//! catalog's GitHub Pages mirror, which allows cross-origin reads), the
//! Home Assistant integration, `hubctl catalog-push` — fetch the signed
//! release files and upload them in chunks; the hub streams each upload
//! into flash, checks it in place against the signed manifest and installs
//! it:
//!
//! ```text
//! client: manifest.cose ─> upload target "manifest": signature, anti-rollback → installed
//!         catalog/status "wants": the bank for this slot size if newer, web tools if different
//! client: bank-4m.zdb ─> target "bank": SHA-256 from the manifest, signature, full decode → A/B flip
//! client: zdefc-web.zwb ─> target "web": SHA-256 from the manifest → served at /zdefc/
//! client: shard-hh.cbor ─> target "shard": SHA-256 from the manifest; records with the keys of
//!         devices that resolve to generic only → local definitions, re-resolution
//! ```
//!
//! An upload survives a dropped connection (`catalog/status` `upload`
//! tells where to continue); a new upload of another target replaces it.

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use hub_devices::api::ApiExtension;
use hub_devices::bankcat::{StoreCatalog, fingerprint_keys};
use hub_devices::device::{Device, InterviewStep, Millis, Refused};
use hub_devices::hub::{ApiError, Hub};
use hub_devices::model::LayerSource;
use hub_devices::transport::ZigbeeTransport;
use log::{info, warn};
use serde_json::Value as J;
use sha2::{Digest, Sha256};
use zdef::sign::ManifestAsset;

use super::data::{AreaWriter, SharedFlash};
use super::store::{CatalogStore, Install, bank_asset};
use super::web::{self, SharedWeb, WebInstall};

/// Default mirror of the catalog releases for clients (GitHub Pages).
pub const DEFAULT_BASE: &str = "https://faronov.github.io/zigbee-rs-definitions/latest";
/// Journal key of the settings.
pub const CONFIG_KEY: &str = "cfg/catalog";
/// Largest chunk of `catalog/upload_chunk` (decoded). A hub without PSRAM
/// holds the WebSocket frame, the parsed request and the decoded bytes at
/// once (BR board: an 8 KiB chunk after a manifest install ran it out of
/// heap).
pub const UPLOAD_CHUNK_MAX: usize = 4 * 1024;
/// An upload idle this long may be replaced by another target's.
pub const UPLOAD_IDLE_MS: Millis = 30_000;

/// Settings the clients follow (stored on the hub so the web UI and the
/// Home Assistant integration agree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogConfig {
    /// Mirror base URL.
    pub base_url: String,
    /// Clients check daily and install newer catalogs.
    pub auto_update: bool,
    /// Clients fetch definitions for devices that resolve to generic only.
    pub auto_fetch: bool,
}

impl Default for CatalogConfig {
    fn default() -> Self {
        Self { base_url: DEFAULT_BASE.into(), auto_update: true, auto_fetch: true }
    }
}

impl CatalogConfig {
    /// As JSON.
    pub fn to_json(&self) -> J {
        obj!({"base_url": self.base_url.clone(), "auto_update": self.auto_update, "auto_fetch": self.auto_fetch})
    }

    /// Applies the fields present in `v`.
    pub fn apply(&mut self, v: &J) -> Result<(), String> {
        if let Some(u) = v.get("base_url") {
            let u = u.as_str().ok_or("base_url: string")?.trim_end_matches('/');
            if !u.starts_with("https://") && !u.starts_with("http://") {
                return Err("base_url: http(s) URL".into());
            }
            self.base_url = u.into();
        }
        for (k, f) in [("auto_update", &mut self.auto_update), ("auto_fetch", &mut self.auto_fetch)] {
            if let Some(b) = v.get(k) {
                *f = b.as_bool().ok_or(format!("{k}: boolean"))?;
            }
        }
        Ok(())
    }
}

/// An upload in progress.
enum Upload {
    /// Manifest or shard into the scratch area.
    Scratch { target: &'static str, writer: AreaWriter, hash: Sha256, size: Option<u32> },
    /// Bank into the inactive slot.
    Bank(Install),
    /// Web tools into their area.
    Web(WebInstall),
}

impl Upload {
    fn target(&self) -> &'static str {
        match self {
            Upload::Scratch { target, .. } => target,
            Upload::Bank(_) => "bank",
            Upload::Web(_) => "web",
        }
    }

    fn written(&self) -> u32 {
        match self {
            Upload::Scratch { writer, .. } => writer.len(),
            Upload::Bank(i) => i.written(),
            Upload::Web(w) => w.written(),
        }
    }
}

/// The service, owned by the app thread.
pub struct CatalogService {
    flash: SharedFlash,
    /// The store.
    pub store: CatalogStore,
    /// Settings.
    pub cfg: CatalogConfig,
    web: SharedWeb,
    upload: Option<Upload>,
    /// When the upload last advanced.
    upload_at: Millis,
    /// Devices announced as needing a definition.
    announced: BTreeSet<String>,
    /// Events for the `catalog` topic, drained by the app.
    pub events: Vec<J>,
    /// Settings changed (to persist).
    pub config_dirty: bool,
}

fn err(code: &str, m: impl Into<String>) -> ApiError {
    ApiError::new(code, m)
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    let v: Option<Vec<u8>> = (0..32).map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()).collect();
    v?.try_into().ok()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn asset_json(a: &ManifestAsset) -> J {
    obj!({"name": a.name.clone(), "size": a.size, "sha256": hex(&a.sha256)})
}

/// Devices that need a definition: interviewed, identified, generic only.
fn generic_only<T: ZigbeeTransport>(hub: &Hub<T>) -> impl Iterator<Item = &Device> {
    shard_targets(hub).filter(|d| d.resolution.layers.iter().all(|l| l.source == LayerSource::Generic))
}

/// Devices an uploaded shard is matched against: those generic only and
/// those that got their definition from a shard. A device's index keys
/// span several shards, and a later one may hold a better definition (the
/// zhc choice for a TS011F with application version 69 is in shard d9, a
/// white-label match in b0): matching only generic devices kept the first.
fn shard_targets<T: ZigbeeTransport>(hub: &Hub<T>) -> impl Iterator<Item = &Device> {
    hub.devices().filter(|d| {
        d.interview == InterviewStep::Done
            && (d.fp.model_id.is_some() || d.fp.gp.is_some())
            && d.resolution.layers.iter().all(|l| matches!(l.source, LayerSource::Generic | LayerSource::Fetched))
    })
}

/// [`Device::refused_better`] as messages (`catalog/status` `missing`,
/// `definitions/fetch`, a shard's commit).
fn refused_messages(d: &Device) -> Vec<String> {
    d.refused_better().iter().map(|r| r.message()).collect()
}

/// A device's entry of `missing` or `definitions/fetch`: the shards that
/// hold its keys and, once fetched, what matched that it cannot run.
fn fetch_entry(store: &CatalogStore, d: &Device) -> J {
    let mut e = obj!({"device": d.id.clone(), "shards": shards_of(store, &fingerprint_keys(&d.fp))});
    let why = refused_messages(d);
    if !why.is_empty() {
        e["refused"] = why.into();
    }
    e
}

/// Byte range of the string value of key `key` in a JSON text (the first
/// `"key": "` occurrence; good enough for the flat objects clients send).
fn string_span(text: &str, key: &str) -> Option<(usize, usize)> {
    let pat = format!("\"{key}\"");
    text.match_indices(&pat).find_map(|(at, _)| {
        let rest = text[at + pat.len()..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
        let start = text.len() - rest.len();
        Some((start, start + rest.find('"')?))
    })
}

/// Shards holding a device's keys (only those the manifest lists),
/// ascending (insertion: a sort or a collected set costs kilobytes).
fn shards_of(store: &CatalogStore, keys: &[u64]) -> Vec<u8> {
    let mut v: Vec<u8> = Vec::new();
    for n in keys.iter().map(|k| zdef::bank::shard_of(*k)) {
        if let Err(i) = v.binary_search(&n)
            && store.manifest_shard(n).is_some()
        {
            v.insert(i, n);
        }
    }
    v
}

impl CatalogService {
    /// The service over an opened store.
    #[inline(never)]
    pub fn new(flash: SharedFlash, store: CatalogStore, cfg: CatalogConfig, web: SharedWeb) -> Self {
        {
            let f = flash.lock().unwrap();
            if let Err(e) = web.load(&**f, store.map.web.offset, store.map.web.size) {
                log::debug!("catalog: {e}");
            }
        }
        Self {
            flash,
            store,
            cfg,
            web,
            upload: None,
            upload_at: 0,
            announced: BTreeSet::new(),
            events: Vec::new(),
            config_dirty: false,
        }
    }

    fn event(&mut self, topic: &str, mut body: J) {
        body["topic"] = topic.into();
        self.events.push(body);
    }

    /// What the installed manifest offers that this hub lacks.
    #[inline(never)]
    fn wants(&self) -> J {
        let Some(m) = &self.store.manifest else { return J::Null };
        let bank = bank_asset(m, self.store.map.bank_a.size)
            .filter(|a| self.store.active.as_ref().is_none_or(|c| a.generation > c.generation));
        let web = m
            .tools
            .iter()
            .find(|t| t.name == web::ASSET && t.size <= self.store.map.web.size as u64)
            .filter(|t| self.web.installed().is_none_or(|i| i.sha256 != t.sha256));
        obj!({"bank": bank.map(asset_json), "web": web.map(asset_json)})
    }

    /// `[{device, shards}]` for devices that resolve to generic only.
    #[inline(never)]
    fn missing<T: ZigbeeTransport>(&self, hub: &Hub<T>) -> J {
        if self.store.manifest.is_none() {
            return J::Array(Vec::new());
        }
        generic_only(hub).map(|d| fetch_entry(&self.store, d)).collect()
    }

    /// Announces devices that newly need a definition (event
    /// `definition_needed`), once per device and start.
    #[inline(never)]
    pub fn tick<T: ZigbeeTransport>(&mut self, hub: &Hub<T>) {
        if self.store.manifest.is_none() {
            return;
        }
        let new: Vec<(String, Vec<u8>)> = generic_only(hub)
            .filter(|d| !self.announced.contains(&d.id))
            .map(|d| (d.id.clone(), shards_of(&self.store, &fingerprint_keys(&d.fp))))
            .collect();
        for (id, shards) in new {
            self.announced.insert(id.clone());
            if !shards.is_empty() {
                self.event("definition_needed", obj!({"device": id, "shards": shards}));
            }
        }
    }

    /// `catalog/status`.
    #[inline(never)]
    pub fn status<T: ZigbeeTransport>(&self, hub: &Hub<T>) -> J {
        let mut s = self.store.status();
        s["config"] = self.cfg.to_json();
        s["wants"] = self.wants();
        s["missing"] = self.missing(hub);
        s["upload"] = self.upload.as_ref().map_or(J::Null, |u| obj!({"target": u.target(), "written": u.written()}));
        s["web_tools"] = self.web.installed().map_or(
            obj!({"installed": false}),
            |i| obj!({"installed": true, "size": i.size(), "sha256": hex(&i.sha256), "files": i.files()}),
        );
        s["engine"] = hub.catalog().status();
        s
    }

    /// `catalog/upload_chunk`, parsed from the request text: the base64
    /// stays in the WebSocket frame and decodes into a stack buffer (a hub
    /// without PSRAM ran out of heap with a `Value` copy and a decoded
    /// `Vec` per chunk next to the frame).
    #[inline(never)]
    fn upload_chunk(&mut self, req: &J, data: &str, now: Millis) -> Result<J, ApiError> {
        let target = req["target"].as_str().unwrap_or("bank");
        let offset = req["offset"].as_u64().ok_or_else(|| err("invalid_format", "offset: number"))?;
        let mut buf = [0u8; UPLOAD_CHUNK_MAX + 3];
        let n = base64::engine::general_purpose::STANDARD.decode_slice(data.trim(), &mut buf).map_err(|e| match e {
            base64::DecodeSliceError::OutputSliceTooSmall => {
                err("invalid_value", format!("chunks are at most {UPLOAD_CHUNK_MAX} bytes"))
            }
            e => err("invalid_format", format!("base64: {e}")),
        })?;
        if n > UPLOAD_CHUNK_MAX {
            return Err(err("invalid_value", format!("chunks are at most {UPLOAD_CHUNK_MAX} bytes")));
        }
        let data = &buf[..n];
        if offset == 0 {
            // Another target's upload that is still advancing belongs to
            // another client (or another task of the same page): refuse
            // rather than drop it; an idle one is taken over.
            if let Some(u) = &self.upload
                && u.target() != target
                && now.saturating_sub(self.upload_at) < UPLOAD_IDLE_MS
            {
                return Err(err("busy", format!("a {} upload is in progress", u.target())));
            }
            self.begin(target, req["size"].as_u64().map(|s| s as u32), req["sha256"].as_str().and_then(unhex32))?;
        }
        self.upload_at = now;
        let up = self
            .upload
            .as_mut()
            .filter(|u| u.target() == target)
            .ok_or_else(|| err("invalid_value", format!("no {target} upload in progress (start at offset 0)")))?;
        if offset != up.written() as u64 {
            return Err(err("invalid_value", format!("expected offset {}", up.written())));
        }
        let r = {
            let f = &mut **self.flash.lock().unwrap();
            match up {
                Upload::Scratch { writer, hash, size, .. } => {
                    if size.is_some_and(|n| writer.len() as u64 + data.len() as u64 > n as u64) {
                        Err(std::io::Error::other("more bytes than announced"))
                    } else {
                        hash.update(data);
                        writer.write(f, data)
                    }
                }
                Upload::Bank(i) => i.write(f, data),
                Upload::Web(w) => w.write(f, data),
            }
        };
        let written = up.written();
        if let Err(e) = r {
            self.abort();
            return Err(err("no_space", e.to_string()));
        }
        Ok(obj!({"written": written}))
    }

    #[inline(never)]
    fn abort(&mut self) {
        if let Some(Upload::Bank(_)) = self.upload.take() {
            self.store.end_install();
        }
    }

    #[inline(never)]
    fn begin(&mut self, target: &str, size: Option<u32>, sha: Option<[u8; 32]>) -> Result<(), ApiError> {
        self.abort();
        let map = self.store.map;
        self.upload = Some(match target {
            "manifest" | "shard" => {
                let area =
                    if target == "manifest" { map.manifest.size.min(map.scratch.size) } else { map.scratch.size };
                if size.is_some_and(|n| n > area) {
                    return Err(err("no_space", format!("{target} larger than {area} bytes")));
                }
                Upload::Scratch {
                    target: if target == "manifest" { "manifest" } else { "shard" },
                    writer: AreaWriter::new(map.scratch.offset, area),
                    hash: Sha256::new(),
                    size,
                }
            }
            "bank" => {
                // The manifest's entry for this file, if one is installed.
                let listed = self.store.manifest.as_ref().and_then(|m| {
                    m.banks
                        .iter()
                        .find(|b| sha.is_none_or(|h| h == b.sha256) && size.is_none_or(|n| n as u64 == b.size))
                });
                let (size, sha) = match listed {
                    Some(b) => (Some(b.size as u32), Some(b.sha256)),
                    None => (size, sha),
                };
                Upload::Bank(self.store.begin_install(size, sha).map_err(|e| err("busy", e))?)
            }
            "web" => {
                // Code for the browser: only the bundle the signed manifest lists.
                let listed = self.store.manifest.as_ref().and_then(|m| m.tools.iter().find(|t| t.name == web::ASSET));
                let Some(t) = listed else {
                    return Err(err("invalid_value", "install a manifest that lists zdefc-web.zwb first"));
                };
                let f = &mut **self.flash.lock().unwrap();
                Upload::Web(
                    WebInstall::begin(&self.web, f, map.web.offset, map.web.size, Some(t.size as u32), Some(t.sha256))
                        .map_err(|e| err("no_space", e.to_string()))?,
                )
            }
            t => return Err(err("invalid_value", format!("unknown target {t}"))),
        });
        Ok(())
    }

    #[inline(never)]
    fn commit<T: ZigbeeTransport>(&mut self, hub: &mut Hub<T>, req: &J, now: Millis) -> Result<J, ApiError> {
        let target = req["target"].as_str().unwrap_or("bank");
        if self.upload.as_ref().is_none_or(|u| u.target() != target) {
            return Err(err("invalid_value", format!("no {target} upload in progress")));
        }
        let up = self.upload.take().unwrap();
        let force = req["force"].as_bool().unwrap_or(false);
        let vf = |e: String| err("verification_failed", e);
        match up {
            Upload::Scratch { target: "manifest", writer, size, .. } => {
                if size.is_some_and(|n| n != writer.len()) {
                    return Err(vf(format!("{} bytes, announced {}", writer.len(), size.unwrap())));
                }
                let m = {
                    let f = &mut **self.flash.lock().unwrap();
                    self.store.install_manifest(f, writer.len()).map_err(vf)?
                };
                self.announced.clear();
                // The new release's shards decide again.
                let ids: Vec<String> = hub.devices().filter(|d| !d.diag.refused.is_empty()).map(|d| d.id.clone()).collect();
                for id in ids {
                    if let Some(d) = hub.device_mut(&id) {
                        d.diag.refused.clear();
                    }
                }
                let r = obj!({"version": m.version(), "catalog_date": m.catalog_date, "catalog_serial": m.catalog_serial, "wants": self.wants()});
                self.event("catalog_manifest", r.clone());
                Ok(r)
            }
            Upload::Scratch { writer, hash, .. } => {
                let n =
                    req["shard"].as_u64().filter(|n| *n < 256).ok_or_else(|| err("invalid_format", "shard: 0..255"))?;
                let want = self.store.manifest_shard(n as u8);
                if want != Some(hash.finalize().into()) {
                    return Err(vf(format!("shard {n:02x}: SHA-256 differs from the installed manifest")));
                }
                let bytes = self.flash.lock().unwrap().map(self.store.map.scratch.offset, writer.len());
                self.install_shard(hub, bytes, req["device"].as_str(), now)
            }
            Upload::Bank(ins) => {
                let installed = {
                    let f = &**self.flash.lock().unwrap();
                    ins.finish(f)
                };
                let ins = installed.map_err(|e| {
                    self.store.end_install();
                    vf(e)
                })?;
                let sha = hex(&ins.sha256);
                let (generation, definitions) = (ins.checked.generation, ins.checked.definitions);
                let bank = {
                    let f = &mut **self.flash.lock().unwrap();
                    self.store.activate(f, ins, force).map_err(vf)?
                };
                let devices = match hub.catalog_mut().as_any_mut().and_then(|a| a.downcast_mut::<StoreCatalog>()) {
                    Some(c) => {
                        c.bank = Some(bank);
                        // The examples only stand in until a bank exists.
                        c.builtin = Default::default();
                        hub.reresolve(None, now)
                    }
                    None => hub.set_catalog(
                        Box::new(StoreCatalog {
                            bank: Some(bank),
                            fetched: self.store.local.catalog(),
                            ..Default::default()
                        }),
                        now,
                    ),
                };
                self.announced.clear();
                info!(
                    "catalog: bank generation {generation} active ({definitions} definitions), {} devices re-resolved",
                    devices.len()
                );
                let r = obj!({"sha256": sha, "generation": generation, "definitions": definitions, "devices": devices});
                self.event("catalog_updated", r.clone());
                Ok(r)
            }
            Upload::Web(w) => {
                {
                    let f = &mut **self.flash.lock().unwrap();
                    w.finish(&self.web, f).map_err(vf)?;
                }
                let i = self.web.installed();
                let r = obj!({"size": i.as_ref().map(|i| i.size()), "files": i.map(|i| i.files())});
                self.event("web_tools_updated", r.clone());
                Ok(r)
            }
        }
    }

    /// Takes the records of a checked shard that match devices resolving to
    /// generic only (or `device`), or that would outrank the fetched
    /// definition of a device, keeps those the hub can run, re-resolves
    /// those devices.
    #[inline(never)]
    fn install_shard<T: ZigbeeTransport>(
        &mut self,
        hub: &mut Hub<T>,
        shard: &[u8],
        device: Option<&str>,
        now: Millis,
    ) -> Result<J, ApiError> {
        // (device, fingerprint, the (score, priority) a record must beat).
        type Target = (String, hub_devices::fingerprint::Fingerprint, Option<(u32, i32)>);
        let targets: Vec<Target> = match device {
            Some(id) => {
                vec![(
                    id.to_string(),
                    hub.device(id).ok_or_else(|| err("not_found", format!("no device {id}")))?.fp.clone(),
                    None,
                )]
            }
            None => shard_targets(hub)
                .map(|d| {
                    let bar = d.resolution.layers.iter().rev().find(|l| l.source == LayerSource::Fetched);
                    (d.id.clone(), d.fp.clone(), bar.map(|l| (l.score.unwrap_or(u32::MAX), l.priority)))
                })
                .collect(),
        };
        let keys: Vec<u64> = targets.iter().flat_map(|(_, fp, _)| fingerprint_keys(fp)).collect();
        let recs =
            zdef::bank::shard_matches(shard, &keys).map_err(|e| err("verification_failed", format!("shard: {e}")))?;
        let used: Vec<String> = hub
            .devices()
            .flat_map(|d| d.resolution.layers.iter().filter(|l| l.source == LayerSource::Fetched).map(|l| l.id.clone()))
            .collect();
        // Every stored record by id, loadable or not: a record must not be
        // appended again with each install of its shard.
        let mut have: Vec<String> = self
            .store
            .local
            .catalog()
            .defs
            .iter()
            .filter_map(|f| zdef::codec::peek_summary(f.cbor, &Default::default()).ok()?.id)
            .collect();
        let (mut stored, mut skipped) = (Vec::new(), Vec::new());
        let mut refused: Vec<(String, Refused)> = Vec::new();
        let mut affected = BTreeMap::new();
        // Decoded whatever features they need, so that a definition this
        // hub cannot run is still matched and named (not silently dropped).
        let every = zdef::HubFeatures(u128::MAX);
        for (rk, _req, cbor) in recs {
            let d = match zdef::load(cbor, &every)
                .map_err(|e| e.to_string())
                .and_then(|zd| hub_devices::zdef_adapter::adapt(zd, LayerSource::Fetched, None).map_err(|e| e.0))
            {
                Ok(d) => d,
                Err(e) => {
                    skipped.push(format!("invalid: {e}"));
                    continue;
                }
            };
            let hits: Vec<(&String, &hub_devices::fingerprint::Fingerprint, u32)> = targets
                .iter()
                .filter_map(|(id, fp, bar)| {
                    let s = hub_devices::resolve::score(&d, fp)?;
                    bar.is_none_or(|b| (s, d.priority) > b).then_some((id, fp, s))
                })
                .collect();
            if hits.is_empty() || have.contains(&d.id) {
                continue;
            }
            let missing = hub_devices::resolve::missing_features(&d, &hub.features);
            if !missing.is_empty() {
                skipped.push(format!("{} requires {}", d.id, missing.join(", ")));
                for (h, fp, score) in hits {
                    // Named as the user knows the device: its white label.
                    let zfp = hub_devices::zdef_adapter::zdef_fingerprint(fp);
                    let (vendor, model) = d
                        .white_label
                        .iter()
                        .find(|w| w.selects(&zfp))
                        .map_or((&d.vendor, &d.model), |w| (&w.vendor, &w.model));
                    let r = Refused {
                        id: d.id.clone(),
                        vendor: vendor.clone(),
                        model: model.clone(),
                        requires: missing.clone(),
                        score,
                        priority: d.priority,
                    };
                    refused.push((h.clone(), r));
                }
                continue;
            }
            let keep = |x: &hub_devices::bankcat::Fetched| {
                zdef::codec::peek_summary(x.cbor, &Default::default())
                    .ok()
                    .and_then(|s| s.id)
                    .is_some_and(|id| used.contains(&id) || id == d.id)
            };
            let r = {
                let f = &mut **self.flash.lock().unwrap();
                self.store.local.append(f, &rk, cbor, &keep)
            };
            match r {
                Ok(()) => {
                    for (h, _, _) in hits {
                        affected.insert(h.clone(), ());
                    }
                    have.push(d.id.clone());
                    stored.push(d.id);
                }
                Err(e) => skipped.push(format!("{}: {e}", d.id)),
            }
        }
        let fetched = self.store.local.catalog();
        match hub.catalog_mut().as_any_mut().and_then(|a| a.downcast_mut::<StoreCatalog>()) {
            Some(c) => c.fetched = fetched,
            None => {
                hub.set_catalog(Box::new(StoreCatalog { fetched, ..Default::default() }), now);
            }
        }
        let ids: Vec<String> = affected.into_keys().collect();
        let devices = hub.reresolve(Some(&ids), now);
        if !stored.is_empty() {
            info!("catalog: {} fetched definitions stored", stored.len());
        }
        for (id, r) in refused {
            if let Some(d) = hub.device_mut(&id) {
                info!("catalog: {id} matches {}, which this firmware cannot run", r.message());
                d.diag.refused.retain(|x| x.id != r.id);
                d.diag.refused.push(r);
            }
        }
        // Per target device, what it would use if this hub could run it.
        let mut why = serde_json::Map::new();
        for (id, _, _) in &targets {
            if let Some(m) = hub.device(id).map(refused_messages).filter(|m| !m.is_empty()) {
                why.insert(id.clone(), m.into());
            }
        }
        let r = obj!({"definitions": stored, "skipped": skipped, "refused": why, "devices": devices});
        self.event("definition_fetched", r.clone());
        Ok(r)
    }
}

impl<T: ZigbeeTransport> ApiExtension<T> for CatalogService {
    fn request_text(&mut self, _hub: &mut Hub<T>, text: &str, now: Millis) -> Option<(u64, Result<J, ApiError>)> {
        if !text.contains("catalog/upload_chunk") {
            return None;
        }
        // The base64 stays in the frame; the rest is parsed without it
        // (a serde derive would cost kilobytes of firmware).
        let (a, b) = string_span(text, "data")?;
        let data = &text[a..b];
        if data.contains('\\') {
            return None; // escaped: the `Value` path
        }
        let mut meta = String::with_capacity(text.len() - data.len());
        meta.push_str(&text[..a]);
        meta.push_str(&text[b..]);
        let req = serde_json::from_str::<J>(&meta).ok()?;
        let id = req["id"].as_u64()?;
        (req["type"] == "catalog/upload_chunk").then(|| (id, self.upload_chunk(&req, data, now)))
    }

    fn request(&mut self, hub: &mut Hub<T>, kind: &str, req: &J, now: Millis) -> Option<Result<J, ApiError>> {
        Some(match kind {
            "catalog/status" => Ok(self.status(hub)),
            "catalog/config" => {
                let before = self.cfg.clone();
                self.cfg.apply(req).map_err(|e| err("invalid_value", e)).map(|_| {
                    self.config_dirty |= self.cfg != before;
                    self.cfg.to_json()
                })
            }
            "catalog/upload_chunk" => match req["data"].as_str() {
                Some(d) => self.upload_chunk(req, d, now),
                None => Err(err("invalid_format", "data: base64")),
            },
            "catalog/commit" => self.commit(hub, req, now),
            "catalog/abort" => {
                self.abort();
                Ok(J::Null)
            }
            // The client fetches; the hub says what to fetch.
            "definitions/fetch" => {
                let id = req["device"].as_str().unwrap_or_default();
                match (hub.device(id), &self.store.manifest) {
                    (None, _) => Err(err("not_found", format!("no device {id}"))),
                    (Some(_), None) => Err(err("not_found", "no catalog manifest installed")),
                    (Some(d), Some(_)) => Ok(fetch_entry(&self.store, d)),
                }
            }
            _ => return None,
        })
    }
}

/// Logs a failed settings load.
pub fn load_config(bytes: Option<Vec<u8>>) -> CatalogConfig {
    let mut cfg = CatalogConfig::default();
    if let Some(v) = bytes
        && let Ok(j) = serde_json::from_slice::<J>(&v)
        && let Err(e) = cfg.apply(&j)
    {
        warn!("catalog: settings: {e}");
    }
    cfg
}

#[cfg(test)]
mod span_tests {
    use super::string_span;

    #[test]
    fn string_span_finds_the_value() {
        let t = r#"{"id":3,"type":"catalog/upload_chunk","target":"data","offset":0, "data" : "QUJD","size":3}"#;
        let (a, b) = string_span(t, "data").unwrap();
        assert_eq!(&t[a..b], "QUJD");
        assert_eq!(string_span(r#"{"data":3}"#, "data"), None);
        assert_eq!(string_span(r#"{"x":"y"}"#, "data"), None);
    }
}
