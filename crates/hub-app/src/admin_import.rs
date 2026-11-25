//! `network/import/preview` and `network/import` (`docs/migration.md`): a
//! migration from Zigbee2MQTT or ZHA in the normalized import document
//! ([`migrate`]), staged request by request like `network/restore`.
//!
//! The apply runs on the job thread in this order: the backend's restore
//! check (a refusal changes nothing), the running network's backup, kept
//! as `bkp/replaced/<ext_pan_id>` before the network is left, the restore
//! with the import margins, then the devices and groups on the app thread
//! (the registry flushed), and only then the NCP's own device inventory
//! (which would interview them all). Meanwhile the app imports nothing of
//! the NCP's tables ([`Admin::import_hold`]). Running it again over the
//! same network skips the restore and changes no device it imported
//! before.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use hub_coord::{
    CoordStatus, NetState, NetworkIdentity, NetworkOwnership, OwnerTag, OwnershipModel,
    RegistryState, RestoreOptions, restore_note,
};
use hub_devices::api::SessionId;
use hub_devices::hub::ApiError;
use hub_devices::hub::import::{IMPORT_RECONFIGURE, ImportedDevice};
use hub_store::{OWNER_KEY, kv};
use log::info;
use serde_json::{Value as J, json};

use super::{
    Admin, Ctx, JobOut, Reply, bad, be_hex, failed_with_backup, field, flag, frame, keep_replaced,
    keys_may_leave, ncp_error, network_state, replace, restore_error, take_backup,
};
use crate::migrate::{self, Counts, Entry, NetworkFacts, Reconfigure};
use crate::ocb;

/// A staged import is dropped this long after its last request.
const STAGE_TTL: Duration = Duration::from_secs(600);
/// How long the apply waits for the app thread to take the devices.
const APPLY_WAIT: Duration = Duration::from_secs(120);

/// The operator's options (the first request's).
#[derive(Debug, Clone, Copy, Default)]
struct Opts {
    force: bool,
    confirm: bool,
    skip_backup: bool,
    ieee_write_once: bool,
    allow_ieee_change: bool,
    remove_missing: bool,
    reconfigure: Reconfigure,
}

impl Opts {
    fn from(v: &J) -> Result<Self, ApiError> {
        Ok(Self {
            force: flag(v, "force", false)?,
            confirm: flag(v, "confirm", false)?,
            skip_backup: flag(v, "skip_backup", false)?,
            ieee_write_once: flag(v, "confirm_ieee_write_once", false)?,
            allow_ieee_change: flag(v, "allow_ieee_change", true)?,
            remove_missing: flag(v, "remove_missing", false)?,
            reconfigure: Reconfigure::parse(v.get("reconfigure")).map_err(bad)?,
        })
    }
}

/// An import being staged.
pub(super) struct Stage {
    sid: SessionId,
    dry_run: bool,
    header: migrate::Header,
    restore: ocb::Restore,
    facts: NetworkFacts,
    link_keys: usize,
    /// Short addresses of the network part's devices.
    addresses: Vec<(u64, u16)>,
    /// Devices to apply (not kept for a preview).
    devices: Vec<ImportedDevice>,
    bytes: usize,
    ieees: Vec<u64>,
    groups: Vec<J>,
    counts: Counts,
    opts: Opts,
    at: Instant,
}

/// Clears [`Admin::import_hold`] when the apply ends, however it ends.
struct Hold(Arc<AtomicBool>);

impl Drop for Hold {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The NCP's network as the backend reports it.
fn running(st: &CoordStatus) -> Option<&NetworkIdentity> {
    match &st.ownership {
        NetworkOwnership::Owned(i) | NetworkOwnership::Observed(i) => Some(i),
        NetworkOwnership::Mismatch { found: Some(i), .. } => Some(i),
        _ => None,
    }
}

/// Whether the NCP runs the network of the import (extended PAN ID and
/// key): no restore then. A backend that does not report the key (ZRHP)
/// runs the network of the ownership tag `tag` when the records are its.
fn same_network(
    st: &CoordStatus,
    tag: Option<&NetworkIdentity>,
    owned: bool,
    r: &ocb::Restore,
) -> bool {
    let Some(i) =
        running(st).filter(|i| st.network_state == NetState::Up && i.ext_pan_id == r.ext_pan_id)
    else {
        return false;
    };
    match i.key_id {
        Some(k) => k == r.key_id,
        None => {
            owned && tag.is_some_and(|t| t.ext_pan_id == r.ext_pan_id && t.key_id == Some(r.key_id))
        }
    }
}

/// The ownership tag's network.
fn tag(ctx: &Ctx<'_>) -> (Option<NetworkIdentity>, bool) {
    let t = kv::with(&**ctx.records, |s| {
        s.get(OWNER_KEY).and_then(OwnerTag::decode)
    })
    .and_then(|t| t.identity);
    (t, ctx.records.registry() == RegistryState::Owned)
}

fn margins(r: &ocb::Restore) -> (u32, u32) {
    (
        migrate::NWK_MARGIN,
        if r.tclk_counter_known {
            migrate::APS_MARGIN_KNOWN
        } else {
            ocb::UNKNOWN_APS_MARGIN
        },
    )
}

impl Stage {
    /// The network part as the preview and the apply describe it (no keys).
    fn network_json(&self, st: &CoordStatus, same: bool, may_restore: bool) -> J {
        let r = &self.restore;
        let coord_now = st.ncp.as_ref().and_then(|n| n.ieee);
        let mut needs = Vec::new();
        if !same && st.network_state != NetState::None && !self.opts.force {
            needs.push("force");
        }
        if !same && !may_restore && !self.opts.confirm {
            needs.push("confirm");
        }
        let (nwk_margin, aps_margin) = margins(r);
        let backend = st.caps.as_ref().map(|c| c.backend.as_str());
        json!({
            "ext_pan_id": be_hex(&r.ext_pan_id),
            "pan_id": r.pan_id,
            "channel": r.channel,
            "coordinator_ieee": be_hex(&r.coordinator_ieee),
            "network_key": true,
            "nwk_frame_counter": self.facts.nwk_counter,
            "tclk_frame_counter": self.facts.tclk_counter,
            "nwk_margin": nwk_margin,
            "aps_margin": aps_margin,
            "devices": r.devices.len(),
            "link_keys": self.link_keys,
            "hashed_tclk": self.facts.hashed_tclk,
            "stack": self.facts.stacks,
            "created": self.facts.created,
            "made_by": self.facts.source,
            "action": if same { "same" } else { "restore" },
            "running": running(st).map(|i| json!({"ext_pan_id": be_hex(&i.ext_pan_id), "up": st.network_state == NetState::Up})),
            "coordinator": match coord_now {
                Some(i) if i == r.coordinator_ieee => "same",
                Some(_) => "differs",
                None => "unknown",
            },
            "backend": backend,
            "needs": needs,
        })
    }

    /// One batch: `network_devices`, `devices` and `groups` of `v` (the
    /// `document` or a following request); the rows of its devices.
    fn batch(
        &mut self,
        v: &J,
        preview: &dyn Fn(&ImportedDevice) -> hub_devices::hub::import::ImportPreview,
    ) -> Result<Vec<J>, ApiError> {
        if let Some(nd) = field(v, "network_devices") {
            ocb::add_devices(&mut self.restore, nd).map_err(bad)?;
            self.link_keys += migrate::link_keys(nd);
            migrate::ocb_addresses(nd, &mut self.addresses);
            if self.restore.devices.len() > super::MAX_RESTORE_DEVICES {
                return Err(ApiError::new(
                    "too_large",
                    format!("more than {} network devices", super::MAX_RESTORE_DEVICES),
                ));
            }
        }
        let mut rows = Vec::new();
        if let Some(list) = field(v, "devices") {
            let list = list.as_array().ok_or_else(|| bad("devices: a list"))?;
            for d in list {
                let mut e = migrate::entry(d);
                // A device entry without a short address takes the network part's.
                if let Entry::Skip {
                    ieee: Some(i),
                    reason: "no_nwk",
                    ..
                } = e
                    && let Some(&(_, n)) = self.addresses.iter().rev().find(|(x, _)| *x == i)
                    && let Some(d) = d.as_object()
                {
                    let mut d = d.clone();
                    d.insert("nwk".into(), n.into());
                    e = migrate::entry(&J::Object(d));
                }
                match e {
                    Entry::Skip {
                        ieee,
                        name,
                        reason,
                        detail,
                    } => {
                        self.counts.skip += 1;
                        rows.push(migrate::skip_row(ieee, &name, reason, &detail));
                    }
                    Entry::Device(d) => {
                        if self.ieees.contains(&d.ieee) {
                            self.counts.skip += 1;
                            rows.push(migrate::skip_row(
                                Some(d.ieee),
                                &d.name,
                                "duplicate",
                                "listed before",
                            ));
                            continue;
                        }
                        let p = preview(&d);
                        self.counts.add(p.action);
                        rows.push(migrate::row(&d, &p));
                        self.ieees.push(d.ieee);
                        if !self.dry_run {
                            self.bytes += migrate::cost(&d);
                            self.devices.push(*d);
                        }
                    }
                }
            }
            if self.ieees.len() > migrate::MAX_DEVICES || self.bytes > migrate::MAX_STAGED_BYTES {
                return Err(ApiError::new(
                    "too_large",
                    format!(
                        "more than {} devices, or their records exceed {} KiB",
                        migrate::MAX_DEVICES,
                        migrate::MAX_STAGED_BYTES / 1024
                    ),
                ));
            }
        }
        if let Some(list) = field(v, "groups") {
            let list = list.as_array().ok_or_else(|| bad("groups: a list"))?;
            let coord = u64::from_le_bytes(self.restore.coordinator_ieee);
            for g in list {
                let g = migrate::group(g, Some(coord)).map_err(bad)?;
                self.groups.retain(|x| x["id"] != g["id"]);
                self.groups.push(g);
            }
            if self.groups.len() > migrate::MAX_GROUPS {
                return Err(ApiError::new(
                    "too_large",
                    format!("more than {} groups", migrate::MAX_GROUPS),
                ));
            }
        }
        Ok(rows)
    }
}

impl Admin {
    /// Whether an import is being applied: the app does not import the
    /// NCP's devices meanwhile (they would all be interviewed).
    pub fn import_holds(&self) -> bool {
        self.import_hold.load(Ordering::SeqCst)
    }

    /// `network/import/preview` (`dry_run`) and `network/import`.
    pub(super) fn import(
        &mut self,
        ctx: Ctx<'_>,
        sid: SessionId,
        id: u64,
        v: &J,
        dry_run: bool,
    ) -> Vec<String> {
        let fail = |e: ApiError| vec![frame(id, Err(e))];
        if !keys_may_leave(ctx.api) {
            return fail(ApiError::new(
                "unauthorized",
                "an import restores network keys: the hub needs an API token (hub/token/rotate)",
            ));
        }
        let more = match flag(v, "more", false) {
            Ok(m) => m,
            Err(e) => return fail(e),
        };
        let first = field(v, "document");
        if let Some(doc) = first {
            self.import_stage = None;
            match self.new_stage(&ctx, sid, v, doc, dry_run) {
                Ok(s) => self.import_stage = Some(Box::new(s)),
                Err(e) => return fail(e),
            }
        }
        if self
            .import_stage
            .as_ref()
            .is_some_and(|s| s.at.elapsed() >= STAGE_TTL)
        {
            self.import_stage = None;
        }
        // Another session's staging stays.
        let Some(stage) = self
            .import_stage
            .as_mut()
            .filter(|s| s.sid == sid && s.dry_run == dry_run)
        else {
            return fail(bad(
                "no import staged in this session: send the document first",
            ));
        };
        stage.at = Instant::now();
        // The document's arrays, then the request's own (`network_devices`,
        // `devices`, `groups`): the first request may carry either.
        let mut rows = Vec::new();
        for part in first.into_iter().chain(Some(v)) {
            match stage.batch(part, ctx.preview) {
                Ok(r) => rows.extend(r),
                Err(e) => {
                    self.import_stage = None;
                    return fail(e);
                }
            }
        }
        let may_restore = ctx.records.may_restore(stage.restore.ext_pan_id);
        let (t, owned) = tag(&ctx);
        let same = same_network(ctx.status, t.as_ref(), owned, &stage.restore);
        if more {
            let mut r = json!({"staged_devices": stage.ieees.len(), "devices": rows});
            if first.is_some() {
                r["network"] = stage.network_json(ctx.status, same, may_restore);
            }
            return vec![frame(id, Ok(r))];
        }
        let Some(stage) = self.import_stage.take() else {
            return fail(bad("no import staged"));
        };
        if dry_run {
            self.import_preview(ctx, sid, id, *stage, rows, same, may_restore)
        } else {
            self.import_apply(ctx, sid, id, *stage, rows, same, may_restore)
        }
    }

    fn new_stage(
        &self,
        ctx: &Ctx<'_>,
        sid: SessionId,
        v: &J,
        doc: &J,
        dry_run: bool,
    ) -> Result<Stage, ApiError> {
        let header = migrate::header(doc).map_err(bad)?;
        let opts = Opts::from(v)?;
        let network = doc.get("network").filter(|n| n.is_object()).ok_or_else(|| {
            ApiError::new("network_required", "the document has no network part (coordinator_backup.json, the ZHA backup): devices cannot be imported without their network")
        })?;
        let encoder = ctx
            .net
            .restore_encoder()
            .map_err(|e| ncp_error("import", e))?;
        let restore = ocb::to_restore(network, encoder)
            .map_err(|e| ApiError::new("invalid_value", format!("network: {e}")))?;
        let mut addresses = Vec::new();
        let nd = network.get("devices").cloned().unwrap_or(J::Null);
        migrate::ocb_addresses(&nd, &mut addresses);
        Ok(Stage {
            sid,
            dry_run,
            header,
            facts: migrate::network_facts(network),
            link_keys: migrate::link_keys(&nd),
            addresses,
            restore,
            devices: Vec::new(),
            bytes: 0,
            ieees: Vec::new(),
            groups: Vec::new(),
            counts: Counts::default(),
            opts,
            at: Instant::now(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn import_preview(
        &mut self,
        ctx: Ctx<'_>,
        sid: SessionId,
        id: u64,
        st: Stage,
        rows: Vec<J>,
        same: bool,
        may_restore: bool,
    ) -> Vec<String> {
        let network = st.network_json(ctx.status, same, may_restore);
        let known = (ctx.known)();
        let missing: Vec<J> = known
            .iter()
            .filter(|k| !st.ieees.contains(&k.ieee))
            .take(100)
            .map(|k| json!({"id": k.id, "name": k.name}))
            .collect();
        let missing_n = known.iter().filter(|k| !st.ieees.contains(&k.ieee)).count();
        let mut r = json!({
            "source": {"kind": st.header.kind, "name": st.header.name},
            "network": network,
            "devices": rows,
            "staged_devices": st.ieees.len(),
            "summary": st.counts.json(),
            "groups": st.groups.len(),
            "not_imported": {"count": missing_n, "devices": missing, "removed": st.opts.remove_missing},
            "restore_check": J::Null,
        });
        if same {
            r["restore_check"] = json!("not_needed");
            return vec![frame(id, Ok(r))];
        }
        if !ctx.status.caps.as_ref().is_some_and(|c| c.backup.restore) {
            r["restore_check"] =
                json!({"code": "unsupported", "message": "the NCP has no BACKUP_RESTORE"});
            return vec![frame(id, Ok(r))];
        }
        let net = ctx.net.clone();
        let (nwk_margin, aps_margin) = margins(&st.restore);
        let opts = RestoreOptions {
            allow_ieee_change: st.opts.allow_ieee_change,
            ieee_write_once: st.opts.ieee_write_once,
            nwk_margin,
            aps_margin,
        };
        let encoder = st.restore.encoder;
        self.spawn("import preview", ctx.out.clone(), sid, id, move |_| {
            let blob = encoder.finish();
            r["restore_check"] = match net.restore_check(&blob, opts) {
                Ok(()) => json!("ok"),
                Err(e) => {
                    let e = restore_error(e);
                    json!({"code": e.code, "message": e.message})
                }
            };
            Ok(Reply::Json(r))
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn import_apply(
        &mut self,
        ctx: Ctx<'_>,
        sid: SessionId,
        id: u64,
        st: Stage,
        rows: Vec<J>,
        same: bool,
        may_restore: bool,
    ) -> Vec<String> {
        let fail = |e: ApiError| vec![frame(id, Err(e))];
        let s = ctx.status;
        if !same {
            if !s.caps.as_ref().is_some_and(|c| c.backup.restore) {
                return fail(ApiError::new(
                    "unsupported",
                    "the NCP has no BACKUP_RESTORE: the network cannot be imported",
                ));
            }
            if s.network_state != NetState::None && !st.opts.force {
                let pan = s
                    .network
                    .map_or(String::from("?"), |n| format!("0x{:04X}", n.pan_id));
                return fail(bad(format!(
                    "the NCP runs another network (PAN {pan}); set force to replace it with the imported one (it is backed up first unless skip_backup)"
                )));
            }
            if !may_restore && !st.opts.confirm {
                return fail(ApiError::new(
                    "registry_read_only",
                    format!(
                        "the device registry ({}) holds the records of another network: the import re-tags them as the imported network's; set confirm to do so",
                        ctx.records.registry().as_str()
                    ),
                ));
            }
        } else if st.devices.is_empty() && st.groups.is_empty() {
            return fail(bad(
                "nothing to import: the hub runs this network already and the document has no devices",
            ));
        }
        let records = ctx.records.clone();
        let net = ctx.net.clone();
        let ncp_nv = s
            .caps
            .as_ref()
            .is_some_and(|c| c.ownership == OwnershipModel::NcpNv);
        let keys_out = keys_may_leave(ctx.api);
        let known = (ctx.known)();
        let source = self.source.clone();
        let now_s = ctx.now_s;
        let hold = self.import_hold.clone();
        hold.store(true, Ordering::SeqCst);
        let Stage {
            restore,
            devices,
            groups,
            opts: o,
            counts: staged,
            header,
            ..
        } = st;
        let skipped = staged.skip;
        let (nwk_margin, aps_margin) = margins(&restore);
        let ropts = RestoreOptions {
            allow_ieee_change: o.allow_ieee_change,
            ieee_write_once: o.ieee_write_once,
            nwk_margin,
            aps_margin,
        };
        drop(rows);
        info!(
            "admin: import from {} ({}): {} devices, {} groups, network {}",
            header.kind,
            header.name,
            devices.len(),
            groups.len(),
            if same { "running" } else { "restored" }
        );
        let started = self.spawn("import", ctx.out.clone(), sid, id, move |out| {
            let _hold = Hold(hold);
            let ocb::Restore { encoder, ext_pan_id, key_id, coordinator_ieee, pan_id, channel, devices: net_devices, .. } = restore;
            let mut previous = None;
            let mut report = None;
            if !same {
                let blob = encoder.finish();
                // A refusal (e.g. an IEEE the NCP cannot take) changes nothing.
                net.restore_check(&blob, ropts).map_err(restore_error)?;
                if network_state(&*net) == NetState::Up && !o.skip_backup {
                    let b = take_backup(&*net, &*records, true, &known, &source, now_s, id).map_err(|e| {
                        ApiError::new(&e.code, format!("automatic backup before the import failed ({}); set skip_backup to import anyway", e.message))
                    })?;
                    if !(b.stored && b.ext_pan_id == ext_pan_id && b.key_id == Some(key_id)) {
                        keep_replaced(&*records, &b)?;
                    }
                    previous = Some(b);
                }
                let replaced = replace(&*net, &*records, || {
                    if ncp_nv && network_state(&*net) != NetState::None {
                        net.leave_reset().map_err(|e| ncp_error("leave/reset", e))?;
                    }
                    records.forming(Some(ext_pan_id));
                    net.backup_restore(blob, ropts).map_err(restore_error)
                });
                match replaced {
                    Ok(r) => report = Some(r),
                    Err(e) => return failed_with_backup(id, e, previous.as_ref(), keys_out),
                }
            }
            // Binds and the IAS CIE address name the coordinator the devices
            // knew: configured again only when the hub's is another.
            let ieee_now = net.status().and_then(|s| s.ncp.and_then(|n| n.ieee));
            let kept = report.as_ref().is_some_and(|r| r.notes.iter().any(|(_, n)| *n == restore_note::IEEE_KEPT));
            let changed = kept || ieee_now != Some(coordinator_ieee);
            let mode = o.reconfigure.mode(changed);
            let (tx, rx) = mpsc::channel();
            out(JobOut::Import(Box::new(migrate::Batch { devices, groups, mode, remove_missing: o.remove_missing, done: tx })));
            let applied = rx.recv_timeout(APPLY_WAIT).map_err(|_| ApiError::new("timeout", "the devices were not taken in time"))?;
            if !same {
                out(JobOut::Reimport);
            }
            out(JobOut::Event(json!({"topic": "network_updated", "reason": "imported"})));
            let file = previous.as_ref().filter(|_| keys_out).map_or("null", |b| b.file());
            let mut c = applied.counts;
            c.skip += skipped;
            let mut result = json!({
                "network": {
                    "action": if same { "same" } else { "restored" },
                    "pan_id": pan_id,
                    "channel": channel,
                    "ext_pan_id": be_hex(&ext_pan_id),
                    "coordinator_ieee": be_hex(&coordinator_ieee),
                    "devices": net_devices.len(),
                },
                "coordinator_changed": changed,
                "reconfigure": mode == IMPORT_RECONFIGURE,
                "devices": c.json(),
                "failed": applied.failed,
                "removed": applied.removed,
                "groups": applied.groups,
                "stored": applied.stored,
                "pending_verification": applied.pending,
                "previous_network_backed_up": previous.is_some(),
            });
            if let Some(r) = &report {
                let n = &mut result["network"];
                n["nwk_fc_start"] = r.nwk_fc_start.into();
                n["aps_fc_start"] = r.aps_fc_start.into();
                n["link_keys_on_ncp"] = r.link_keys_on_ncp.into();
                n["link_keys_host_backed"] = r.link_keys_host_backed.into();
                n["children"] = r.children.into();
                n["notes"] = r.notes.iter().map(|(i, n)| json!({"index": i, "note": super::note_name(*n)})).collect();
            }
            info!("admin: import done: {}", result["devices"]);
            // The previous network's backup holds keys: the text is put in
            // as it is, not parsed.
            let body = result.to_string();
            let mut t = String::new();
            t.try_reserve(body.len() + file.len() + 64).map_err(|_| crate::jw::oom())?;
            t.push_str(&format!("{{\"id\":{id},\"type\":\"result\",\"success\":true,\"result\":"));
            t.push_str(&body[..body.len() - 1]);
            t.push_str(",\"backup\":");
            t.push_str(file);
            t.push_str("}}");
            drop(previous);
            Ok(Reply::Frame(t))
        });
        // Frames now: the job did not start (busy, low memory).
        if !started.is_empty() {
            self.import_hold.store(false, Ordering::SeqCst);
        }
        started
    }
}
