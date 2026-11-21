//! WebSocket JSON API (spec 14): message model ([`messages`]) and a
//! transport-free server ([`ApiServer`]): sessions, authentication,
//! request handlers over the [`Hub`], subscriptions with snapshot and
//! `seq`, and deferred command results. The network server (later) only
//! moves text frames between sockets and [`ApiServer::handle_text`] /
//! [`ApiServer::pump`].

pub mod messages;

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use serde_json::{Value as J, json};

use crate::device::{DeviceKind, Millis};
use crate::gp::types::GpRequest;
use crate::hub::{ApiError, Hub, HubEvent};
use crate::model::{Binding, Definition, LayerSource};
use crate::resolve::{Catalog, MemoryCatalog, ResolveInput, resolve};
use crate::transport::ZigbeeTransport;
pub use messages::*;

/// A connection.
pub type SessionId = u64;

#[derive(Debug, Default)]
struct Session {
    authed: bool,
    /// Subscription ID → topics.
    subs: BTreeMap<u64, BTreeSet<String>>,
    /// Command ticket → request ID.
    tickets: BTreeMap<u64, u64>,
}

/// The API state of the hub.
pub struct ApiServer {
    hub_name: String,
    token: String,
    sessions: BTreeMap<SessionId, Session>,
    next_session: SessionId,
    seq: u64,
    /// Fields the application adds to `hub/info` (coordinator IEEE, NCP).
    info: serde_json::Map<String, J>,
}

fn b64(s: &str) -> Result<Vec<u8>, ApiError> {
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|e| ApiError::new("invalid_format", format!("base64: {e}")))
}

fn missing(id: &str) -> ApiError {
    ApiError::new("not_found", format!("no device {id}"))
}

fn def_summary(d: &Definition) -> J {
    json!({
        "id": d.id,
        "revision": d.revision,
        "vendor": d.vendor,
        "model": d.model,
        "source": d.source,
        "content_hash": d.content_hash,
        "size": d.zdef.as_ref().and_then(|z| z.encode().ok()).map(|b| b.len()),
        "requires": d.used_features(),
    })
}

fn uids_of<T: ZigbeeTransport>(hub: &Hub<T>, dev: &str) -> Vec<String> {
    hub.device(dev)
        .map(|x| {
            x.entities
                .iter()
                .filter(|e| !e.orphaned)
                .map(|e| e.uid.clone())
                .collect()
        })
        .unwrap_or_default()
}

impl ApiServer {
    /// A server for hub `hub_name` accepting `token`.
    pub fn new(hub_name: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            hub_name: hub_name.into(),
            token: token.into(),
            sessions: BTreeMap::new(),
            next_session: 1,
            seq: 0,
            info: serde_json::Map::new(),
        }
    }

    /// A new connection; returns its ID and the `auth_required` frame.
    pub fn connect(&mut self) -> (SessionId, String) {
        let id = self.next_session;
        self.next_session += 1;
        self.sessions.insert(id, Session::default());
        (
            id,
            ServerMessage::AuthRequired {
                hub: self.hub_name.clone(),
                api_version: API_VERSION.into(),
            }
            .to_text(),
        )
    }

    /// A closed connection.
    pub fn disconnect(&mut self, s: SessionId) {
        self.sessions.remove(&s);
    }

    /// Sets a field of the `hub/info` result (`null` removes it).
    pub fn set_info(&mut self, key: &str, value: J) {
        if value.is_null() {
            self.info.remove(key);
        } else {
            self.info.insert(key.into(), value);
        }
    }

    /// The current event sequence number.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Handles one text frame; returns the frames to send back.
    pub fn handle_text<T: ZigbeeTransport>(
        &mut self,
        hub: &mut Hub<T>,
        s: SessionId,
        text: &str,
        now: Millis,
    ) -> Vec<String> {
        let Some(sess) = self.sessions.get_mut(&s) else {
            return Vec::new();
        };
        if !sess.authed {
            return match serde_json::from_str::<Auth>(text) {
                Ok(a) if a.token == self.token => {
                    sess.authed = true;
                    vec![
                        ServerMessage::AuthOk {
                            hub: self.hub_name.clone(),
                            api_version: API_VERSION.into(),
                            zdef_versions: vec![1],
                            features: hub.features.iter().map(|f| f.to_string()).collect(),
                        }
                        .to_text(),
                    ]
                }
                _ => vec![
                    ServerMessage::AuthInvalid {
                        message: "unauthorized".into(),
                    }
                    .to_text(),
                ],
            };
        }
        let env: Envelope = match serde_json::from_str(text) {
            Ok(e) => e,
            Err(e) => {
                let id = serde_json::from_str::<J>(text)
                    .ok()
                    .and_then(|v| v["id"].as_u64())
                    .unwrap_or(0);
                return vec![
                    ServerMessage::err(id, ApiError::new("invalid_format", e.to_string()))
                        .to_text(),
                ];
            }
        };
        self.handle(hub, s, env, now)
            .into_iter()
            .map(|m| m.to_text())
            .collect()
    }

    /// Handles one typed request.
    pub fn handle<T: ZigbeeTransport>(
        &mut self,
        hub: &mut Hub<T>,
        s: SessionId,
        env: Envelope,
        now: Millis,
    ) -> Vec<ServerMessage> {
        let id = env.id;
        if let Request::Subscribe { topics } = &env.request {
            let bad: Vec<&String> = topics
                .iter()
                .filter(|t| !TOPICS.contains(&t.as_str()))
                .collect();
            if !bad.is_empty() {
                return vec![ServerMessage::err(
                    id,
                    ApiError::new("invalid_value", format!("unknown topics {bad:?}")),
                )];
            }
            if let Some(sess) = self.sessions.get_mut(&s) {
                sess.subs.insert(id, topics.iter().cloned().collect());
            }
            // Snapshot first, then deltas with increasing seq (14.1).
            let snap = self.snapshot(hub, topics, now);
            return vec![
                ServerMessage::ok(id, json!({"seq": self.seq})),
                ServerMessage::Event {
                    id,
                    seq: self.seq,
                    event: snap,
                },
            ];
        }
        if let Request::Unsubscribe { subscription } = env.request {
            let found = self
                .sessions
                .get_mut(&s)
                .is_some_and(|x| x.subs.remove(&subscription).is_some());
            return vec![if found {
                ServerMessage::ok(id, J::Null)
            } else {
                ServerMessage::err(id, ApiError::new("not_found", "no subscription"))
            }];
        }
        if let Request::EntitiesCommand {
            uid,
            command,
            params,
        } = &env.request
        {
            return match hub.entity_command(uid, command, params, now) {
                Ok(ack) if ack.status == "ok" => {
                    vec![ServerMessage::ok(id, json!({"status": "ok"}))]
                }
                Ok(ack) => {
                    if let Some(sess) = self.sessions.get_mut(&s) {
                        sess.tickets.insert(ack.ticket, id);
                    }
                    if ack.status == "queued" {
                        vec![ServerMessage::ok(
                            id,
                            json!({"status": "queued", "ticket": ack.ticket}),
                        )]
                    } else {
                        Vec::new() // the result follows when the device confirms
                    }
                }
                Err(e) => vec![ServerMessage::err(id, e)],
            };
        }
        vec![match self.dispatch(hub, env.request, now) {
            Ok(r) => ServerMessage::ok(id, r),
            Err(e) => ServerMessage::err(id, e),
        }]
    }

    fn snapshot<T: ZigbeeTransport>(&self, hub: &mut Hub<T>, topics: &[String], now: Millis) -> J {
        let mut snap = json!({"topic": "snapshot"});
        if topics.iter().any(|t| t == "devices") {
            snap["devices"] = hub.devices().map(|d| d.record()).collect();
        }
        if topics.iter().any(|t| t == "entities") {
            snap["entities"] = hub
                .devices()
                .flat_map(|d| d.entities.iter().map(move |e| e.record(&d.id)))
                .collect();
            snap["states"] = hub
                .devices()
                .flat_map(|d| d.entities.iter().map(move |e| e.state_msg(d.available)))
                .collect();
        }
        if topics.iter().any(|t| t == "gp") {
            snap["bindings"] = J::Array(hub.list_bindings(None, None));
        }
        if topics.iter().any(|t| t == "network") {
            snap["network"] = network_info(hub, now).unwrap_or(J::Null);
        }
        snap
    }

    fn dispatch<T: ZigbeeTransport>(
        &mut self,
        hub: &mut Hub<T>,
        req: Request,
        now: Millis,
    ) -> Result<J, ApiError> {
        use Request as R;
        Ok(match req {
            R::Subscribe { .. } | R::Unsubscribe { .. } | R::EntitiesCommand { .. } => J::Null,
            R::HubInfo => {
                let mut r = json!({
                    "name": self.hub_name,
                    "api_version": API_VERSION,
                    "features": hub.features,
                    "seq": self.seq,
                    "devices": hub.devices().count(),
                    "entities": hub.devices().map(|d| d.entities.iter().filter(|e| !e.orphaned).count()).sum::<usize>(),
                });
                if let Some(o) = r.as_object_mut() {
                    o.extend(self.info.clone());
                }
                r
            }
            R::NetworkInfo => network_info(hub, now)
                .ok_or_else(|| ApiError::new("unsupported", "no network information"))?,
            R::PermitJoin {
                duration_s,
                target,
                include_gp,
            } => {
                let nwk = match target.as_deref() {
                    None | Some("all") => None,
                    Some(t) => Some(
                        hub.device(t)
                            .and_then(|d| d.nwk())
                            .ok_or_else(|| missing(t))?,
                    ),
                };
                hub.transport
                    .permit_join(duration_s, nwk)
                    .map_err(|e| ApiError::new("busy", e.to_string()))?;
                if include_gp {
                    hub.gp_commissioning_open(u16::from(duration_s), true, false)?;
                }
                J::Null
            }
            R::DevicesList => hub.devices().map(|d| d.record()).collect(),
            R::DevicesGet { id } => {
                let d = hub.device(&id).ok_or_else(|| missing(&id))?;
                let mut r = d.record();
                r["endpoints"] = serde_json::to_value(&d.fp.endpoints).unwrap_or_default();
                r["resolution"] = json!({
                    "layers": d.resolution.layers,
                    "fingerprint_hash": d.fp_hash,
                    "configure_hash": d.configure.hash,
                    "entities": d.entities.iter().filter(|e| !e.orphaned).count(),
                    "orphaned": d.entities.iter().filter(|e| e.orphaned).map(|e| e.uid.clone()).collect::<Vec<_>>(),
                });
                r
            }
            R::DevicesUpdate { id, name, disabled } => {
                let d = hub.device_mut(&id).ok_or_else(|| missing(&id))?;
                if let Some(n) = name {
                    d.user_name = Some(n);
                }
                if let Some(x) = disabled {
                    d.disabled = x;
                }
                let r = d.record();
                hub.events.push(HubEvent::DeviceUpdated(id));
                r
            }
            R::DevicesRemove { id, force } => {
                hub.remove_device(&id, force, now)?;
                J::Null
            }
            R::DevicesReinterview { id } => {
                hub.reinterview(&id, now)?;
                json!({"started": true})
            }
            R::DevicesReconfigure { id } => {
                hub.reconfigure(&id, now)?;
                let d = hub.device(&id).ok_or_else(|| missing(&id))?;
                json!({"configure_hash": d.configure.hash, "steps": d.diag.configure})
            }
            R::DevicesDiagnostics { id } => hub.diagnostics(&id).ok_or_else(|| missing(&id))?,
            R::DevicesPinDefinition { id, definition } => {
                hub.pin_definition(&id, definition, now)?;
                json!({"layers": hub.device(&id).map(|d| d.resolution.layers.clone())})
            }
            R::EntitiesList { device } => hub
                .devices()
                .filter(|d| device.as_ref().is_none_or(|x| *x == d.id))
                .flat_map(|d| d.entities.iter().map(move |e| e.record(&d.id)))
                .collect(),
            R::EntitiesStates { uids } => hub
                .devices()
                .flat_map(|d| d.entities.iter().map(move |e| (d, e)))
                .filter(|(_, e)| uids.as_ref().is_none_or(|u| u.contains(&e.uid)))
                .map(|(d, e)| e.state_msg(d.available))
                .collect(),
            R::EntitiesRefresh { uid } => {
                hub.refresh(&uid, now)?;
                J::Null
            }
            R::EntitiesUpdate {
                uid,
                name,
                enabled,
                value,
            } => hub.update_entity(&uid, name, enabled, value.as_ref(), now)?,
            R::EntitiesDelete { uid } => {
                hub.delete_entity(&uid)?;
                J::Null
            }
            R::DefinitionsValidate { cbor } => Self::validate(hub, &b64(&cbor)?),
            R::DefinitionsUpload { cbor, replace } => {
                let bytes = b64(&cbor)?;
                let d = crate::zdef_adapter::load_cbor(&bytes, LayerSource::User)
                    .map_err(|e| ApiError::new("verification_failed", e.0))?;
                if d.source == LayerSource::SelfDescription {
                    return Err(ApiError::new(
                        "invalid_value",
                        "fragments are device data, not uploads",
                    ));
                }
                if let Some(r) = replace.filter(|r| *r != d.id) {
                    hub.delete_user_definition(&r, now)?;
                }
                let before: BTreeMap<String, Vec<String>> = hub
                    .devices()
                    .map(|x| (x.id.clone(), uids_of(hub, &x.id)))
                    .collect();
                let id = d.id.clone();
                let matched = hub.upload_user_definition(d, now)?;
                let diffs: Vec<J> = matched
                    .iter()
                    .map(|dev| {
                        let now_uids = uids_of(hub, dev);
                        let old = before.get(dev).cloned().unwrap_or_default();
                        json!({
                            "device": dev,
                            "added": now_uids.iter().filter(|u| !old.contains(u)).collect::<Vec<_>>(),
                            "orphaned": old.iter().filter(|u| !now_uids.contains(u)).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                json!({"id": id, "matches": matched, "resolution": diffs})
            }
            R::DefinitionsDelete { id } => {
                hub.delete_user_definition(&id, now)?;
                J::Null
            }
            R::DefinitionsList { source, device } => {
                let fp = device
                    .as_ref()
                    .map(|d| {
                        hub.device(d)
                            .map(|x| x.fp.clone())
                            .ok_or_else(|| missing(d))
                    })
                    .transpose()?;
                let mut all: Vec<std::sync::Arc<Definition>> = hub.catalog().all();
                all.extend(hub.user_definitions().all());
                all.into_iter()
                    .filter(|d| {
                        source.as_deref().is_none_or(|s| {
                            serde_json::to_value(d.source).ok().is_some_and(|v| v == s)
                        })
                    })
                    .filter(|d| {
                        fp.as_ref()
                            .is_none_or(|f| crate::resolve::score(d, f).is_some())
                    })
                    .map(|d| def_summary(&d))
                    .collect()
            }
            R::DefinitionsGet {
                id,
                content_hash,
                format,
            } => {
                let mut all = hub.catalog().all();
                all.extend(hub.user_definitions().all());
                let d = all
                    .into_iter()
                    .find(|d| {
                        id.as_ref().is_some_and(|i| *i == d.id)
                            || content_hash.as_ref().is_some_and(|h| {
                                d.content_hash
                                    .as_deref()
                                    .is_some_and(|x| x.starts_with(h.as_str()))
                            })
                    })
                    .ok_or_else(|| ApiError::new("not_found", "no such definition"))?;
                let mut r = def_summary(&d);
                if format.as_deref() == Some("json") {
                    r["entities"] = d
                        .entities
                        .iter()
                        .map(|e| json!({"key": e.key, "platform": e.platform, "device_class": e.device_class, "unit": e.unit, "parts": e.parts.iter().map(|p| p.0.clone()).collect::<Vec<_>>()}))
                        .collect();
                } else {
                    let bytes = d
                        .zdef
                        .as_ref()
                        .and_then(|z| z.encode().ok())
                        .ok_or_else(|| ApiError::new("unsupported", "no CBOR form"))?;
                    r["cbor"] = base64::engine::general_purpose::STANDARD
                        .encode(bytes)
                        .into();
                }
                r
            }
            R::DefinitionsPreview { device, cbor } => {
                let d = crate::zdef_adapter::load_cbor(&b64(&cbor)?, LayerSource::User)
                    .map_err(|e| ApiError::new("verification_failed", e.0))?;
                Self::preview(hub, &device, d)?
            }
            R::DefinitionsScaffold { device } => Self::scaffold(hub, &device)?,
            R::DefinitionsFetch { .. } => {
                return Err(ApiError::new(
                    "unsupported",
                    "on-demand fetch is done by the definition store (spec 9.3)",
                ));
            }
            R::CatalogStatus => json!({
                "definitions": hub.catalog().all().len(),
                "user_definitions": hub.user_definitions().all().len(),
                "features": hub.features,
            }),
            R::GpCommissioningOpen {
                duration_s,
                involve_proxies,
                exit_on_first_pairing,
            } => {
                hub.gp_commissioning_open(duration_s, involve_proxies, exit_on_first_pairing)?;
                J::Null
            }
            R::GpCommissioningClose => {
                hub.gp_commissioning_close()?;
                J::Null
            }
            R::GpDevicesList => hub
                .devices()
                .filter(|d| d.gp.is_some())
                .map(|d| {
                    let mut r = d.record();
                    if let Some(g) = &d.gp {
                        r["security_level"] = g.security_level.into();
                        r["frame_counter"] = g.frame_counter.into();
                        r["sink"] = serde_json::to_value(&g.sink).unwrap_or_default();
                    }
                    r
                })
                .collect(),
            R::GpDevicesRemove { id, send_remove } => {
                let gpd = match hub.device(&id).map(|d| d.kind.clone()) {
                    Some(DeviceKind::Gp { gpd_id }) => gpd_id,
                    _ => return Err(missing(&id)),
                };
                hub.gp_remove(gpd, send_remove, now)?;
                hub.remove_device_entry(&id);
                J::Null
            }
            R::GpConfig {
                security_level,
                communication_mode,
            } => {
                let req = GpRequest::SinkConfig {
                    mode: 1,
                    communication_mode,
                    group: hub.cfg.gp_group,
                    exit_mode: 0x02,
                    window_s: 180,
                    security_level,
                    groupcast_radius: 0,
                };
                hub.transport
                    .gp_request(req)
                    .map_err(|e| ApiError::new("busy", e.to_string()))?;
                json!({"security_level": security_level, "warning": (security_level > 0).then_some("GPDs below this level (e.g. FlowerX GP) are rejected")})
            }
            R::GpBindingsList { gpd, target } => {
                J::Array(hub.list_bindings(gpd.as_deref(), target.as_deref()))
            }
            R::GpBindingsCreate(c) => hub.create_binding(c, now)?,
            R::GpBindingsUpdate {
                binding_id,
                executor,
                action,
                trigger,
            } => hub.update_binding(binding_id, executor, action, trigger, now)?,
            R::GpBindingsDelete { binding_id } => {
                hub.delete_binding(binding_id, now)?;
                J::Null
            }
            R::GpBindingsVerify { binding_id } => {
                hub.verify_bindings(binding_id, now);
                J::Null
            }
            R::GpTargetsProbe { device } => {
                if hub.device(&device).and_then(|d| d.nwk()).is_none() {
                    return Err(missing(&device));
                }
                match hub.gp.caps.get(&device).filter(|c| c.probed).cloned() {
                    Some(c) => serde_json::to_value(c).unwrap_or_default(),
                    None => {
                        hub.probe_target(&device, None, now);
                        json!({"probing": true})
                    }
                }
            }
        })
    }

    fn validate<T: ZigbeeTransport>(hub: &Hub<T>, bytes: &[u8]) -> J {
        match zdef::load(bytes, &crate::zdef_adapter::hub_features()) {
            Ok(zd) => match crate::zdef_adapter::adapt(
                zd,
                LayerSource::User,
                Some(format!("{}", zdef::ContentHash::of(bytes))),
            ) {
                Ok(d) => json!({
                    "ok": true,
                    "errors": [],
                    "warnings": [],
                    "requires": d.used_features(),
                    "content_hash": d.content_hash,
                    "size": bytes.len(),
                    "matches": hub.devices().filter(|x| crate::resolve::score(&d, &x.fp).is_some()).map(|x| x.id.clone()).collect::<Vec<_>>(),
                }),
                Err(e) => {
                    json!({"ok": false, "errors": [{"path": "", "message": e.0}], "warnings": [], "size": bytes.len()})
                }
            },
            Err(zdef::LoadError::Invalid(v)) => json!({
                "ok": false,
                "errors": v.iter().map(|e| {
                    let path = e.path.to_string();
                    let full = e.to_string();
                    let message = full.strip_prefix(&format!("{path}: ")).unwrap_or(&full).to_string();
                    json!({"path": path, "message": message})
                }).collect::<Vec<_>>(),
                "warnings": [],
                "size": bytes.len(),
            }),
            Err(e) => {
                json!({"ok": false, "errors": [{"path": "", "message": e.to_string()}], "warnings": [], "size": bytes.len()})
            }
        }
    }

    fn preview<T: ZigbeeTransport>(
        hub: &Hub<T>,
        device: &str,
        d: Definition,
    ) -> Result<J, ApiError> {
        let dev = hub.device(device).ok_or_else(|| missing(device))?;
        if crate::resolve::score(&d, &dev.fp).is_none() {
            return Ok(json!({"matches": false}));
        }
        let mut user = MemoryCatalog::default();
        user.defs.extend(hub.user_definitions().all());
        user.insert(d);
        let fp = dev.fp.clone();
        let attrs = &dev.attrs;
        let gen_zb = |power| {
            crate::generic::generic_entities(&fp, &crate::generic::GenericInput { attrs, power })
        };
        let gen_gp = |_| crate::gp::generic::gp_generic_entities(&fp, attrs);
        let generic: &dyn Fn(crate::model::PowerKind) -> Vec<crate::model::EntityDef> =
            if dev.gp.is_some() { &gen_gp } else { &gen_zb };
        let res = resolve(&ResolveInput {
            fp: &fp,
            generic,
            catalog: hub.catalog(),
            user: &user,
            self_description: dev.schema.fragment.as_deref(),
            pinned: dev.pinned.as_deref(),
            features: &hub.features,
        });
        let new: Vec<String> = res
            .entities
            .iter()
            .map(|e| format!("{}:{}:{}", dev.id, e.ep, e.def.key))
            .collect();
        let old: Vec<String> = dev
            .entities
            .iter()
            .filter(|e| !e.orphaned)
            .map(|e| e.uid.clone())
            .collect();
        let changed: Vec<String> = res
            .entities
            .iter()
            .filter_map(|r| {
                let uid = format!("{}:{}:{}", dev.id, r.ep, r.def.key);
                let cur = dev.entities.iter().find(|e| e.uid == uid && !e.orphaned)?;
                let mut probe = cur.clone();
                probe.r = r.clone();
                (probe.record(&dev.id) != cur.record(&dev.id)).then_some(uid)
            })
            .collect();
        Ok(json!({
            "matches": true,
            "layers": res.layers,
            "added": new.iter().filter(|u| !old.contains(u)).collect::<Vec<_>>(),
            "changed": changed,
            "orphaned": old.iter().filter(|u| !new.contains(u)).collect::<Vec<_>>(),
        }))
    }

    fn scaffold<T: ZigbeeTransport>(hub: &Hub<T>, device: &str) -> Result<J, ApiError> {
        let d = hub.device(device).ok_or_else(|| missing(device))?;
        let mut clause = json!({});
        if let Some(m) = &d.fp.model_id {
            clause["model"] = m.clone().into();
        }
        if let Some(m) = &d.fp.manufacturer_name {
            clause["manufacturer"] = m.clone().into();
        }
        if let Some(g) = &d.fp.gp {
            clause = json!({"gp": {"device_id": g.device_id, "manufacturer_id": g.manufacturer_id, "model_id": g.model_id}});
        }
        Ok(json!({
            "identity": {"manufacturer": d.fp.manufacturer_name, "model": d.fp.model_id, "mfr_code": d.fp.mfr_code, "sw_build": d.fp.sw_build_id},
            "match": [clause],
            "attributes": d.attrs.map.iter().filter(|(_, e)| e.value.is_some()).map(|((ep, c, a, m), e)| json!({
                "ep": ep, "cluster": format!("0x{c:04X}"), "attr": format!("0x{a:04X}"), "mfr": m.map(|m| format!("0x{m:04X}")), "value": format!("{:?}", e.value),
            })).collect::<Vec<_>>(),
            "tuya_dps": d.diag.tuya_dps.iter().map(|(dp, (t, hex, _))| json!({"dp": dp, "type": t, "data": hex})).collect::<Vec<_>>(),
            "generic_entities": d.entities.iter().filter(|e| e.r.source == LayerSource::Generic && !matches!(e.r.def.value.binding, Binding::DeviceField(_))).map(|e| json!({"key": e.r.def.key, "platform": e.r.def.platform, "ep": e.r.ep})).collect::<Vec<_>>(),
        }))
    }

    /// Drains the hub's events into frames: deltas for subscribers (with
    /// increasing `seq`) and deferred command results.
    pub fn pump<T: ZigbeeTransport>(&mut self, hub: &mut Hub<T>) -> Vec<(SessionId, String)> {
        let events = hub.drain_events();
        self.pump_events(hub, events)
    }

    /// [`pump`](Self::pump) for events the caller drained itself (so that
    /// it can also act on them, e.g. persist changed devices).
    pub fn pump_events<T: ZigbeeTransport>(
        &mut self,
        hub: &mut Hub<T>,
        events: Vec<HubEvent>,
    ) -> Vec<(SessionId, String)> {
        let mut out = Vec::new();
        for ev in events {
            if let HubEvent::CommandResult { ticket, result } = &ev {
                for (sid, sess) in &mut self.sessions {
                    if let Some(rid) = sess.tickets.remove(ticket) {
                        let m = match result {
                            Ok(_) => ServerMessage::ok(rid, json!({"status": "ok"})),
                            Err(e) => ServerMessage::err(rid, e.clone()),
                        };
                        out.push((*sid, m.to_text()));
                    }
                }
                continue;
            }
            let (topic, body) = match ev {
                HubEvent::DeviceAdded(id) => (
                    "devices",
                    json!({"topic": "device_added", "device": hub.device(&id).map(|d| d.record())}),
                ),
                HubEvent::DeviceUpdated(id) => (
                    "devices",
                    json!({"topic": "device_updated", "device": hub.device(&id).map(|d| d.record())}),
                ),
                HubEvent::DeviceRemoved(id) => {
                    ("devices", json!({"topic": "device_removed", "id": id}))
                }
                HubEvent::InterviewProgress { device, step } => (
                    "devices",
                    json!({"topic": "interview_progress", "device": device, "step": step}),
                ),
                HubEvent::ResolutionChanged {
                    device,
                    added,
                    orphaned,
                } => (
                    "devices",
                    json!({"topic": "resolution_changed", "device": device, "added": added, "orphaned": orphaned, "layers": hub.device(&device).map(|d| d.resolution.layers.clone())}),
                ),
                HubEvent::EntityAdded(uid) => (
                    "entities",
                    json!({"topic": "entity_added", "entity": hub.entity(&uid).map(|(d, e)| e.record(&d.id))}),
                ),
                HubEvent::EntityUpdated(uid) => (
                    "entities",
                    json!({"topic": "entity_updated", "entity": hub.entity(&uid).map(|(d, e)| e.record(&d.id))}),
                ),
                HubEvent::EntityRemoved(uid) => {
                    ("entities", json!({"topic": "entity_removed", "uid": uid}))
                }
                HubEvent::EntityState(mut s) => {
                    s["topic"] = "entity_state".into();
                    ("entities", s)
                }
                HubEvent::EntityEvent {
                    uid,
                    event_type,
                    attributes,
                    time,
                } => (
                    "events",
                    json!({"topic": "entity_event", "uid": uid, "event_type": event_type, "attributes": attributes, "time": time}),
                ),
                HubEvent::Network(mut body) => {
                    if !body.is_object() {
                        body = json!({"body": body});
                    }
                    if body.get("topic").is_none() {
                        body["topic"] = "network".into();
                    }
                    ("network", body)
                }
                HubEvent::Gp { kind, mut body } => {
                    if let Some(o) = body.as_object_mut() {
                        o.insert("topic".into(), kind.into());
                    } else {
                        body = json!({"topic": kind, "body": body});
                    }
                    ("gp", body)
                }
                HubEvent::CommandResult { .. } => continue,
            };
            self.seq += 1;
            for (sid, sess) in &self.sessions {
                for (sub, topics) in &sess.subs {
                    if topics.contains(topic) {
                        out.push((
                            *sid,
                            ServerMessage::Event {
                                id: *sub,
                                seq: self.seq,
                                event: body.clone(),
                            }
                            .to_text(),
                        ));
                    }
                }
            }
        }
        out
    }
}

/// The host core's facts plus `time`, the hub's clock (s, the time base of
/// `last_seen` and state timestamps), so a client can show relative times
/// even before the hub has synchronised its clock.
fn network_info<T: ZigbeeTransport>(hub: &mut Hub<T>, now: Millis) -> Option<J> {
    let mut v = hub.transport.network_info()?;
    if let Some(o) = v.as_object_mut() {
        o.insert("time".into(), crate::device::ts(now).into());
    }
    Some(v)
}

impl<T: ZigbeeTransport> Hub<T> {
    /// Drops a device record without network traffic.
    pub(crate) fn remove_device_entry(&mut self, id: &str) {
        if let Some(d) = self.devices.remove(id) {
            for e in &d.entities {
                self.events.push(HubEvent::EntityRemoved(e.uid.clone()));
            }
            self.dispatch.remove(id);
            self.events.push(HubEvent::DeviceRemoved(id.to_string()));
        }
    }
}

#[cfg(test)]
mod tests;
