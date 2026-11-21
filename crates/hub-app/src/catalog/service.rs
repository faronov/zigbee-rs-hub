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
use hub_devices::device::{InterviewStep, Millis};
use hub_devices::hub::{ApiError, Hub};
use hub_devices::model::LayerSource;
use hub_devices::resolve::Catalog;
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
fn generic_only<T: ZigbeeTransport>(hub: &Hub<T>) -> impl Iterator<Item = &hub_devices::device::Device> {
    hub.devices().filter(|d| {
        d.interview == InterviewStep::Done
            && (d.fp.model_id.is_some() || d.fp.gp.is_some())
            && d.resolution.layers.iter().all(|l| l.source == LayerSource::Generic)
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
        generic_only(hub)
            .map(|d| obj!({"device": d.id.clone(), "shards": shards_of(&self.store, &fingerprint_keys(&d.fp))}))
            .collect()
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

    #[inline(never)]
    fn upload_chunk(&mut self, req: &J) -> Result<J, ApiError> {
        let target = req["target"].as_str().unwrap_or("bank");
        let offset = req["offset"].as_u64().ok_or_else(|| err("invalid_format", "offset: number"))?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(req["data"].as_str().ok_or_else(|| err("invalid_format", "data: base64"))?.trim())
            .map_err(|e| err("invalid_format", format!("base64: {e}")))?;
        if data.len() > UPLOAD_CHUNK_MAX {
            return Err(err("invalid_value", format!("chunks are at most {UPLOAD_CHUNK_MAX} bytes")));
        }
        if offset == 0 {
            self.begin(target, req)?;
        }
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
                        hash.update(&data);
                        writer.write(f, &data)
                    }
                }
                Upload::Bank(i) => i.write(f, &data),
                Upload::Web(w) => w.write(f, &data),
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
    fn begin(&mut self, target: &str, req: &J) -> Result<(), ApiError> {
        self.abort();
        let size = req["size"].as_u64().map(|s| s as u32);
        let sha = req["sha256"].as_str().and_then(unhex32);
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
    /// generic only (or `device`), keeps those that score and that the hub
    /// can run, re-resolves those devices.
    #[inline(never)]
    fn install_shard<T: ZigbeeTransport>(
        &mut self,
        hub: &mut Hub<T>,
        shard: &[u8],
        device: Option<&str>,
        now: Millis,
    ) -> Result<J, ApiError> {
        let targets: Vec<(String, hub_devices::fingerprint::Fingerprint)> = match device {
            Some(id) => {
                vec![(
                    id.to_string(),
                    hub.device(id).ok_or_else(|| err("not_found", format!("no device {id}")))?.fp.clone(),
                )]
            }
            None => generic_only(hub).map(|d| (d.id.clone(), d.fp.clone())).collect(),
        };
        let keys: Vec<u64> = targets.iter().flat_map(|(_, fp)| fingerprint_keys(fp)).collect();
        let recs =
            zdef::bank::shard_matches(shard, &keys).map_err(|e| err("verification_failed", format!("shard: {e}")))?;
        let used: Vec<String> = hub
            .devices()
            .flat_map(|d| d.resolution.layers.iter().filter(|l| l.source == LayerSource::Fetched).map(|l| l.id.clone()))
            .collect();
        let mut have: Vec<String> = self.store.local.catalog().all().iter().map(|d| d.id.clone()).collect();
        let (mut stored, mut skipped) = (Vec::new(), Vec::new());
        let mut affected = BTreeMap::new();
        for (rk, _req, cbor) in recs {
            let d = match hub_devices::zdef_adapter::load_cbor(cbor, LayerSource::Fetched) {
                Ok(d) => d,
                Err(e) => {
                    skipped.push(format!("invalid: {}", e.0));
                    continue;
                }
            };
            let hits: Vec<&String> = targets
                .iter()
                .filter(|(_, fp)| hub_devices::resolve::score(&d, fp).is_some())
                .map(|(id, _)| id)
                .collect();
            if hits.is_empty() || have.contains(&d.id) {
                continue;
            }
            let missing = hub_devices::resolve::missing_features(&d, &hub.features);
            if !missing.is_empty() {
                skipped.push(format!("{} requires {}", d.id, missing.join(", ")));
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
                    for h in hits {
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
        let r = obj!({"definitions": stored, "skipped": skipped, "devices": devices});
        self.event("definition_fetched", r.clone());
        Ok(r)
    }
}

impl<T: ZigbeeTransport> ApiExtension<T> for CatalogService {
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
            "catalog/upload_chunk" => self.upload_chunk(req),
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
                    (Some(d), Some(_)) => {
                        Ok(obj!({"device": id, "shards": shards_of(&self.store, &fingerprint_keys(&d.fp))}))
                    }
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
