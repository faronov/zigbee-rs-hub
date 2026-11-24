//! WebSocket JSON API (spec 14): message model ([`messages`]) and a
//! transport-free server ([`ApiServer`]): sessions, authentication,
//! request handlers over the [`Hub`], subscriptions with snapshot and
//! `seq`, and deferred command results. The network server (later) only
//! moves text frames between sockets and [`ApiServer::handle_text`] /
//! [`ApiServer::pump`].

pub mod messages;
mod snapshot;

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use serde_json::{Value as J, json};

use crate::device::{DeviceKind, Millis};
use crate::hub::control::GpRemoved;
use crate::hub::{ApiError, Hub, HubEvent};
use crate::model::{Binding, Definition, LayerSource};
use crate::resolve::{Catalog, MemoryCatalog, ResolveInput, resolve};
use crate::transport::ZigbeeTransport;
pub use messages::*;
pub use snapshot::SnapshotStream;

/// A frame for a session: text, or a snapshot the sink takes whole
/// ([`SnapshotStream::render`]) or piece by piece.
#[derive(Debug)]
pub enum Frame {
    /// A whole message.
    Text(String),
    /// The `snapshot` event of a subscription.
    Snapshot(SnapshotStream),
}

fn one(m: ServerMessage) -> Vec<Frame> {
    vec![Frame::Text(m.into_text())]
}

impl Frame {
    /// The whole text.
    pub fn render<T: ZigbeeTransport>(self, hub: &mut Hub<T>, now: Millis) -> String {
        match self {
            Self::Text(t) => t,
            Self::Snapshot(s) => s.render(hub, now),
        }
    }
}

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

/// A binding as one line: `zcl ep 2 0x0402/0x0000`, `tuya dp 2`, ….
fn binding_text(b: &Binding, ep: u8) -> String {
    let mfr = |m: &Option<u16>| m.map(|m| format!(" mfr 0x{m:04X}")).unwrap_or_default();
    match b {
        Binding::None => "none".into(),
        Binding::Zcl(z) => format!(
            "zcl ep {ep} 0x{:04X}/0x{:04X}{}",
            z.cluster,
            z.attr,
            mfr(&z.mfr)
        ),
        Binding::Tuya { dp, .. } => format!("tuya dp {dp}"),
        Binding::Struct {
            cluster,
            attr,
            mfr: m,
            tag,
            ..
        } => format!(
            "struct ep {ep} 0x{cluster:04X}/0x{attr:04X}{} tag {tag}",
            mfr(m)
        ),
        Binding::Gp(crate::model::GpBinding::Report { cluster, attr, .. }) => {
            format!("gp report 0x{cluster:04X}/0x{attr:04X}")
        }
        Binding::Gp(crate::model::GpBinding::Cmd(c)) => format!("gp command 0x{c:02X}"),
        Binding::Hub { .. } => "hub option".into(),
        Binding::IasWarning => format!("ias warning ep {ep}"),
        Binding::DeviceField(f) => format!(
            "device {}",
            serde_json::to_value(f)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        ),
    }
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
        (id, self.connect_as(id))
    }

    /// A new connection under an ID the caller allocated (unique, e.g. from
    /// a counter shared with the transport thread, so that thread never
    /// waits for the API's); returns the `auth_required` frame.
    pub fn connect_as(&mut self, id: SessionId) -> String {
        self.next_session = self.next_session.max(id + 1);
        self.sessions.insert(id, Session::default());
        ServerMessage::AuthRequired {
            hub: self.hub_name.clone(),
            api_version: API_VERSION.into(),
        }
        .to_text()
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

    /// Whether session `s` has authenticated (requests the application
    /// answers itself, e.g. network maintenance, need it too).
    pub fn is_authed(&self, s: SessionId) -> bool {
        self.sessions.get(&s).is_some_and(|x| x.authed)
    }

    /// The API token new connections must present.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Replaces the token (rotation); sessions already authenticated stay.
    pub fn set_token(&mut self, token: impl Into<String>) {
        self.token = token.into();
    }

    /// Handles one text frame; returns the frames to send back.
    pub fn handle_text<T: ZigbeeTransport>(
        &mut self,
        hub: &mut Hub<T>,
        s: SessionId,
        text: &str,
        now: Millis,
    ) -> Vec<String> {
        self.handle_text_ext(hub, s, text, now, None)
    }

    /// [`handle_text`](Self::handle_text) with requests an
    /// [`ApiExtension`] serves.
    pub fn handle_text_ext<T: ZigbeeTransport>(
        &mut self,
        hub: &mut Hub<T>,
        s: SessionId,
        text: &str,
        now: Millis,
        ext: Option<&mut dyn ApiExtension<T>>,
    ) -> Vec<String> {
        self.handle_text_frames(hub, s, text, now, ext)
            .into_iter()
            .map(|f| f.render(hub, now))
            .collect()
    }

    /// [`handle_text_ext`](Self::handle_text_ext) with a subscription
    /// snapshot left to the caller to render ([`Frame::Snapshot`]).
    pub fn handle_text_frames<T: ZigbeeTransport>(
        &mut self,
        hub: &mut Hub<T>,
        s: SessionId,
        text: &str,
        now: Millis,
        ext: Option<&mut dyn ApiExtension<T>>,
    ) -> Vec<Frame> {
        let Some(sess) = self.sessions.get_mut(&s) else {
            return Vec::new();
        };
        if !sess.authed {
            return match serde_json::from_str::<Auth>(text) {
                Ok(a) if a.token == self.token => {
                    sess.authed = true;
                    one(ServerMessage::AuthOk {
                        hub: self.hub_name.clone(),
                        api_version: API_VERSION.into(),
                        zdef_versions: vec![1],
                        features: hub.features.iter().map(|f| f.to_string()).collect(),
                    })
                }
                _ => one(ServerMessage::AuthInvalid {
                    message: "unauthorized".into(),
                }),
            };
        }
        // Groups, scenes and bindings (14.9): parsed from the JSON value.
        if text.contains("groups/") || text.contains("scenes/") || text.contains("bindings/") {
            if let Ok(raw) = serde_json::from_str::<J>(text)
                && let (Some(id), Some(kind)) = (raw["id"].as_u64(), raw["type"].as_str())
                && (kind.starts_with("groups/")
                    || kind.starts_with("scenes/")
                    || kind.starts_with("bindings/"))
            {
                hub.deferred = None;
                let r = hub.zg_request(kind, &raw, now);
                return self
                    .answer(hub, s, id, r)
                    .map(|m| Frame::Text(m.into_text()))
                    .into_iter()
                    .collect();
            }
        }
        // An extension may take a request from its text (uploads: no copy
        // of the payload in a `Value`).
        let mut ext = ext;
        if let Some(ext) = ext.as_deref_mut()
            && let Some((id, r)) = ext.request_text(hub, text, now)
        {
            return one(match r {
                Ok(v) => ServerMessage::ok(id, v),
                Err(e) => ServerMessage::err(id, e),
            });
        }
        // Only requests an extension may serve are parsed twice.
        if let Some(ext) = ext
            && (text.contains("catalog/") || text.contains("definitions/fetch"))
            && let Ok(raw) = serde_json::from_str::<J>(text)
            && let (Some(id), Some(kind)) = (raw["id"].as_u64(), raw["type"].as_str())
            && let Some(r) = ext.request(hub, kind, &raw, now)
        {
            return one(match r {
                Ok(v) => ServerMessage::ok(id, v),
                Err(e) => ServerMessage::err(id, e),
            });
        }
        let env: Envelope = match serde_json::from_str(text) {
            Ok(e) => e,
            Err(e) => {
                let id = serde_json::from_str::<J>(text)
                    .ok()
                    .and_then(|v| v["id"].as_u64())
                    .unwrap_or(0);
                return one(ServerMessage::err(
                    id,
                    ApiError::new("invalid_format", e.to_string()),
                ));
            }
        };
        self.handle_frames(hub, s, env, now)
    }

    /// Handles one typed request.
    pub fn handle<T: ZigbeeTransport>(
        &mut self,
        hub: &mut Hub<T>,
        s: SessionId,
        env: Envelope,
        now: Millis,
    ) -> Vec<ServerMessage> {
        if let Request::Subscribe { topics } = &env.request {
            return match self.subscribe(hub, s, env.id, topics) {
                Ok((ok, snap)) => vec![ok, ServerMessage::Text(snap.render(hub, now))],
                Err(e) => vec![e],
            };
        }
        self.request(hub, s, env, now)
    }

    fn handle_frames<T: ZigbeeTransport>(
        &mut self,
        hub: &mut Hub<T>,
        s: SessionId,
        env: Envelope,
        now: Millis,
    ) -> Vec<Frame> {
        if let Request::Subscribe { topics } = &env.request {
            return match self.subscribe(hub, s, env.id, topics) {
                Ok((ok, snap)) => vec![Frame::Text(ok.into_text()), Frame::Snapshot(snap)],
                Err(e) => one(e),
            };
        }
        self.request(hub, s, env, now)
            .into_iter()
            .map(|m| Frame::Text(m.into_text()))
            .collect()
    }

    /// A subscription: its result and snapshot, then deltas with increasing
    /// seq (14.1).
    fn subscribe<T: ZigbeeTransport>(
        &mut self,
        hub: &Hub<T>,
        s: SessionId,
        id: u64,
        topics: &[String],
    ) -> Result<(ServerMessage, SnapshotStream), ServerMessage> {
        #[cfg(not(feature = "advanced"))]
        if topics.iter().any(|topic| topic == "frames") {
            return Err(ServerMessage::err(
                id,
                ApiError::new("unsupported", "frame tracing requires an advanced image"),
            ));
        }
        let bad: Vec<&String> = topics
            .iter()
            .filter(|t| !TOPICS.contains(&t.as_str()))
            .collect();
        if !bad.is_empty() {
            return Err(ServerMessage::err(
                id,
                ApiError::new("invalid_value", format!("unknown topics {bad:?}")),
            ));
        }
        if let Some(sess) = self.sessions.get_mut(&s) {
            sess.subs.insert(id, topics.iter().cloned().collect());
        }
        Ok((
            ServerMessage::ok(id, json!({"seq": self.seq})),
            SnapshotStream::new(hub, topics, id, self.seq),
        ))
    }

    fn request<T: ZigbeeTransport>(
        &mut self,
        hub: &mut Hub<T>,
        s: SessionId,
        env: Envelope,
        now: Millis,
    ) -> Vec<ServerMessage> {
        let id = env.id;
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
        let console = match &env.request {
            Request::EntitiesCommand {
                uid,
                command,
                params,
            } => Some(hub.entity_command(uid, command, params, now)),
            #[cfg(feature = "advanced")]
            Request::TuyaSend(a) => Some(crate::console::parse_dps(a).and_then(|dps| {
                hub.tuya_send_dps(a["device"].as_str().unwrap_or_default(), &dps, now)
            })),
            #[cfg(feature = "advanced")]
            r => crate::console::request(r).map(|c| c.and_then(|c| hub.console(c, now))),
            #[cfg(not(feature = "advanced"))]
            _ => None,
        };
        if let Some(res) = console {
            return match res {
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
        hub.deferred = None;
        let r = self.dispatch(hub, env.request, now);
        self.answer(hub, s, id, r).into_iter().collect()
    }

    /// The result of request `id`, or `None` when a control request
    /// decides it: its [`HubEvent::Answer`] follows as the deferred result.
    fn answer<T: ZigbeeTransport>(
        &mut self,
        hub: &mut Hub<T>,
        s: SessionId,
        id: u64,
        r: Result<J, ApiError>,
    ) -> Option<ServerMessage> {
        if let Some(ticket) = hub.take_deferred() {
            if let Some(sess) = self.sessions.get_mut(&s) {
                sess.tickets.insert(ticket, id);
            }
            return None;
        }
        Some(match r {
            Ok(r) => ServerMessage::ok(id, r),
            Err(e) => ServerMessage::err(id, e),
        })
    }

    /// The deferred result of request `id` of session `s` that a control
    /// request already decided (its [`HubEvent::Answer`] is still among
    /// the hub's events), taken now: for a caller that needs the result
    /// in place (the application's automations).
    pub fn take_result<T: ZigbeeTransport>(
        &mut self,
        hub: &mut Hub<T>,
        s: SessionId,
        id: u64,
    ) -> Option<Result<J, ApiError>> {
        let tickets = &mut self.sessions.get_mut(&s)?.tickets;
        let i = hub.events.iter().position(
            |e| matches!(e, HubEvent::Answer { ticket, .. } if tickets.get(ticket) == Some(&id)),
        )?;
        let HubEvent::Answer { ticket, result } = hub.events.remove(i) else {
            return None;
        };
        tickets.remove(&ticket);
        Some(result)
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
            R::ZclRead(_)
            | R::ZclWrite(_)
            | R::ZclConfigureReporting(_)
            | R::ZclReadReporting(_)
            | R::ZclDiscover(_)
            | R::ZclCommand(_)
            | R::TuyaSend(_) => {
                return Err(ApiError::new(
                    "unsupported",
                    "manual ZCL/Tuya commands require an advanced image",
                ));
            }
            R::HubInfo => {
                let mut r = json!({
                    "name": self.hub_name,
                    "api_version": API_VERSION,
                    "advanced": cfg!(feature = "advanced"),
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
                hub.permit_join(duration_s, nwk, include_gp, now)?
            }
            R::DevicesList => hub
                .devices()
                .map(|d| d.record())
                .chain(hub.zg_device_records())
                .collect(),
            R::DevicesGet { id } if id.starts_with("grp:") => {
                hub.zg_device_record(&id).ok_or_else(|| missing(&id))?
            }
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
                    "pinned": d.pinned,
                    // Where each entity's value comes from (converters view).
                    "mappings": d.entities.iter().filter(|e| !e.orphaned).map(|e| {
                        let mut m = serde_json::Map::new();
                        m.insert("uid".into(), e.uid.clone().into());
                        m.insert("source".into(), serde_json::to_value(e.r.source).unwrap_or_default());
                        m.insert("binding".into(), binding_text(&e.r.def.value.binding, e.r.ep).into());
                        J::Object(m)
                    }).collect::<Vec<_>>(),
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
            R::DevicesRemove { id, force } if id.starts_with("grp:") => {
                let g = u16::from_str_radix(id.get(4..8).unwrap_or(""), 16)
                    .map_err(|_| missing(&id))?;
                hub.group_delete(g, force, now)?;
                J::Null
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
                .chain(
                    hub.zg_entity_records()
                        .into_iter()
                        .filter(|e| device.as_ref().is_none_or(|x| e["device"] == **x)),
                )
                .collect(),
            R::EntitiesStates { uids } => hub
                .devices()
                .flat_map(|d| d.entities.iter().map(move |e| (d, e)))
                .filter(|(_, e)| uids.as_ref().is_none_or(|u| u.contains(&e.uid)))
                .map(|(d, e)| e.state_msg(d.available))
                .chain(hub.zg_states().into_iter().filter(|s| {
                    uids.as_ref()
                        .is_none_or(|u| u.iter().any(|x| s["uid"] == **x))
                }))
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
                report,
                part,
            } => {
                if let Some(r) = &report {
                    hub.set_entity_report(&uid, part.as_deref(), r, now)?;
                }
                hub.update_entity(&uid, name, enabled, value.as_ref(), now)?
            }
            R::EntitiesDelete { uid } => {
                hub.delete_entity(&uid)?;
                J::Null
            }
            R::EntitiesReporting { uid, device, read } => {
                hub.entity_reporting(uid.as_deref(), device.as_deref(), read, now)?
            }
            R::ReportingDefaultsGet => hub.reporting_defaults().record(),
            R::ReportingDefaultsSet { defaults } => hub.set_reporting_defaults(&defaults, now)?,
            R::DefinitionsValidate { cbor } => Self::validate(hub, &b64(&cbor)?),
            R::DefinitionsUpload {
                cbor,
                replace,
                source,
                source_format,
            } => {
                // With the compiled definition the request must still fit
                // the hub's 16 KiB WebSocket frame.
                if source.as_ref().is_some_and(|t| t.len() > 8 * 1024) {
                    return Err(ApiError::new("no_space", "source text above 8 KiB"));
                }
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
                match source {
                    Some(t) => {
                        let f = source_format.unwrap_or_else(|| "yaml".into());
                        hub.user_sources.insert(id.clone(), (f, t));
                    }
                    None => {
                        hub.user_sources.remove(&id);
                    }
                }
                let diffs: Vec<J> = matched
                    .iter()
                    .map(|dev| {
                        let now_uids = uids_of(hub, dev);
                        let old = before.get(dev).cloned().unwrap_or_default();
                        json!({
                            "device": dev,
                            "added": now_uids.iter().filter(|u| !old.contains(u)).collect::<Vec<_>>(),
                            "removed": old.iter().filter(|u| !now_uids.contains(u)).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                json!({"id": id, "matches": matched, "resolution": diffs})
            }
            R::DefinitionsDelete { id } => {
                hub.delete_user_definition(&id, now)?;
                J::Null
            }
            R::DefinitionsList {
                source,
                device,
                query,
                offset,
                limit,
            } => {
                let want = |src: LayerSource| {
                    source
                        .as_deref()
                        .is_none_or(|s| serde_json::to_value(src).ok().is_some_and(|v| v == s))
                };
                if let Some(dev) = &device {
                    // Candidates by index key, re-checked against the
                    // fingerprint (a bank is never listed whole).
                    let fp = hub
                        .device(dev)
                        .map(|x| x.fp.clone())
                        .ok_or_else(|| missing(dev))?;
                    let mut all = hub.catalog().candidates(&fp);
                    all.extend(hub.user_definitions().candidates(&fp));
                    return Ok(all
                        .into_iter()
                        .filter(|d| want(d.source))
                        .filter(|d| crate::resolve::score(d, &fp).is_some())
                        .map(|d| def_summary(&d))
                        .collect());
                }
                // User definitions first, then the catalog (built-in,
                // fetched, bank) page by page.
                let paged = offset.is_some() || limit.is_some() || query.is_some();
                let (offset, limit) = (offset.unwrap_or(0), limit.unwrap_or(200).min(500));
                let user: Vec<_> = hub.user_definitions().all();
                let (un, mut items) = if want(LayerSource::User) {
                    crate::resolve::list_defs(&user, offset, limit, query.as_deref())
                } else {
                    (0, Vec::new())
                };
                let mut total = un;
                let wants_catalog = [LayerSource::Catalog, LayerSource::Fetched]
                    .iter()
                    .any(|s| want(*s));
                if wants_catalog {
                    let (n, more) = hub.catalog().list(
                        offset.saturating_sub(un),
                        limit - items.len(),
                        query.as_deref(),
                    );
                    total += n;
                    items.extend(more.into_iter().filter(|d| want(d.source)));
                }
                let items: Vec<J> = items
                    .iter()
                    .map(|d| json!({"id": d.id, "revision": d.revision, "vendor": d.vendor, "model": d.model, "source": d.source}))
                    .collect();
                if paged {
                    json!({"total": total, "offset": offset, "items": items})
                } else {
                    J::Array(items)
                }
            }
            R::DefinitionsGet {
                id,
                content_hash,
                format,
            } => {
                let mut all = hub.user_definitions().all();
                all.extend(hub.catalog().all());
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
                    // A bank is searched by `id` only.
                    .or_else(|| id.as_deref().and_then(|i| hub.catalog().get(i)))
                    .ok_or_else(|| ApiError::new("not_found", "no such definition"))?;
                let mut r = def_summary(&d);
                if let Some((f, t)) = hub
                    .user_sources
                    .get(&d.id)
                    .filter(|_| d.source == LayerSource::User)
                {
                    r["source_format"] = f.clone().into();
                    r["source_text"] = t.clone().into();
                }
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
                hub.gp_commissioning_open(duration_s, involve_proxies, exit_on_first_pairing, now)?;
                J::Null
            }
            R::GpCommissioningClose => {
                hub.gp_commissioning_close(now)?;
                J::Null
            }
            R::GpCommissioningStatus => hub.gp_commissioning_status(now),
            R::GpDevicesList => hub
                .devices()
                .filter(|d| d.gp.is_some())
                .map(|d| {
                    let mut r = d.record();
                    if let Some(g) = &d.fp.gp {
                        let mut cmds = g.commands.clone();
                        cmds.extend(d.attrs.map.keys().filter_map(|(ep, c, a, _)| {
                            (*ep == crate::gp::GP_ENDPOINT
                                && *c == crate::gp::generic::SEEN_COMMANDS)
                                .then_some(*a as u8)
                        }));
                        cmds.sort_unstable();
                        cmds.dedup();
                        r["device_id"] = g.device_id.into();
                        r["commands"] = cmds.into();
                    }
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
                // The record goes once the Sink Table entry is gone.
                hub.gp_remove(gpd, send_remove, GpRemoved::Entry(id), now)?
            }
            R::GpConfig {
                security_level,
                communication_mode,
            } => hub.gp_set_config(security_level, communication_mode, now)?,
            R::GpConfigGet => hub.gp_config(),
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
            "removed": old.iter().filter(|u| !new.contains(u)).collect::<Vec<_>>(),
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
        #[cfg(feature = "advanced")]
        {
            // Frames are tapped only while someone listens.
            hub.frame_tap = self
                .sessions
                .values()
                .any(|s| s.subs.values().any(|t| t.contains("frames")));
        }
        let mut out = Vec::new();
        for ev in events {
            let ev = match ev {
                HubEvent::CommandResult { ticket, result } => {
                    self.deliver(ticket, result.map(|_| json!({"status": "ok"})), &mut out);
                    continue;
                }
                HubEvent::Answer { ticket, result } => {
                    self.deliver(ticket, result, &mut out);
                    continue;
                }
                #[cfg(feature = "advanced")]
                HubEvent::ConsoleResult { ticket, result } => {
                    self.deliver(ticket, result, &mut out);
                    continue;
                }
                ev => ev,
            };
            let (topic, body) = match ev {
                HubEvent::DeviceAdded(id) => (
                    "devices",
                    json!({"topic": "device_added", "device": hub.device(&id).map(|d| d.record()).or_else(|| hub.zg_device_record(&id))}),
                ),
                HubEvent::DeviceUpdated(id) => (
                    "devices",
                    json!({"topic": "device_updated", "device": hub.device(&id).map(|d| d.record()).or_else(|| hub.zg_device_record(&id))}),
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
                    removed,
                } => (
                    "devices",
                    json!({"topic": "resolution_changed", "device": device, "added": added, "orphaned": orphaned, "removed": removed, "layers": hub.device(&device).map(|d| d.resolution.layers.clone())}),
                ),
                HubEvent::EntityAdded(uid) => (
                    "entities",
                    json!({"topic": "entity_added", "entity": hub.entity(&uid).map(|(d, e)| e.record(&d.id)).or_else(|| hub.zg_entity_record(&uid))}),
                ),
                HubEvent::EntityUpdated(uid) => (
                    "entities",
                    json!({"topic": "entity_updated", "entity": hub.entity(&uid).map(|(d, e)| e.record(&d.id)).or_else(|| hub.zg_entity_record(&uid))}),
                ),
                HubEvent::EntityRemoved(uid) => {
                    ("entities", json!({"topic": "entity_removed", "uid": uid}))
                }
                HubEvent::EntityState(mut s) => {
                    s["topic"] = "entity_state".into();
                    ("entities", s)
                }
                HubEvent::EntityReporting(mut b) => {
                    b["topic"] = "entity_reporting".into();
                    ("entities", b)
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
                HubEvent::Groups { kind, mut body } => {
                    body["topic"] = kind.into();
                    ("groups", body)
                }
                HubEvent::Gp { kind, mut body } => {
                    if let Some(o) = body.as_object_mut() {
                        o.insert("topic".into(), kind.into());
                    } else {
                        body = json!({"topic": kind, "body": body});
                    }
                    ("gp", body)
                }
                HubEvent::CommandResult { .. } | HubEvent::Answer { .. } => continue,
                #[cfg(feature = "advanced")]
                HubEvent::ConsoleResult { .. } => continue,
                #[cfg(feature = "advanced")]
                HubEvent::Frame(body) => ("frames", body),
            };
            out.extend(self.publish(topic, body));
        }
        out
    }

    /// The deferred result of a ticket to the request that started it.
    #[inline(never)]
    fn deliver(
        &mut self,
        ticket: u64,
        result: Result<J, ApiError>,
        out: &mut Vec<(SessionId, String)>,
    ) {
        for (sid, sess) in &mut self.sessions {
            if let Some(rid) = sess.tickets.remove(&ticket) {
                let m = match result {
                    Ok(r) => ServerMessage::ok(rid, r),
                    Err(e) => ServerMessage::err(rid, e),
                };
                out.push((*sid, m.to_text()));
                return;
            }
        }
    }

    /// An event on `topic` for its subscribers (next `seq`), e.g. from a
    /// service outside the engine (catalog updates).
    pub fn publish(&mut self, topic: &str, body: J) -> Vec<(SessionId, String)> {
        let mut out = Vec::new();
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
        out
    }
}

/// Requests served outside the engine (the application's catalog store:
/// `catalog/*`, `definitions/fetch`). Asked first for every request of an
/// authenticated session; `None` = not handled here.
pub trait ApiExtension<T: ZigbeeTransport> {
    /// Handles request `kind`; `req` is the whole request object.
    fn request(
        &mut self,
        hub: &mut Hub<T>,
        kind: &str,
        req: &J,
        now: Millis,
    ) -> Option<Result<J, ApiError>>;

    /// Handles a request from its text before any parsing (`None`: not
    /// this extension's, or not handled this way); returns the request id
    /// and the result.
    fn request_text(
        &mut self,
        _hub: &mut Hub<T>,
        _text: &str,
        _now: Millis,
    ) -> Option<(u64, Result<J, ApiError>)> {
        None
    }
}

/// The host core's facts plus `time`, the hub's clock (s, the time base of
/// `last_seen` and state timestamps), so a client can show relative times
/// even before the hub has synchronised its clock; once it has, `time` is
/// Unix time and `time_utc` repeats it (null before), `time_synced` says
/// which.
pub(crate) fn network_info<T: ZigbeeTransport>(hub: &mut Hub<T>, now: Millis) -> Option<J> {
    let mut v = hub.transport.network_info()?;
    if let Some(o) = v.as_object_mut() {
        let t = crate::device::ts(now);
        let synced = crate::device::time_synced();
        o.insert("time".into(), t.into());
        o.insert("time_utc".into(), if synced { t.into() } else { J::Null });
        o.insert("time_synced".into(), synced.into());
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
