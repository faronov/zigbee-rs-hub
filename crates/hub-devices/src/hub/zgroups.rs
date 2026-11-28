//! Zigbee groups, scenes and bindings (device-support 14.9).
//!
//! - Groups: IDs the hub allocates (0x0001–0xFFF7, not the GP group),
//!   membership of device endpoints through the ZCL Groups cluster
//!   (0x0004: Add/View/Get Group Membership/Remove Group) and of the hub's
//!   own endpoint through ZRHP `NWK_GROUP_MEMBERSHIP` (so the hub hears
//!   what remotes send to the group). A group with on/off members gets a
//!   group device (`grp:<hex id>`) with a `light` or `switch` entity whose
//!   state is aggregated from the members; its commands are encoded like
//!   the representative member's and sent as one APS groupcast.
//! - Scenes (ZCL Scenes 0x0005): per group, stored on every member
//!   (unicast Store Scene, so each answer is seen), recalled with one
//!   groupcast; scene entities (platform `scene`, command `activate`).
//! - Bindings: ZDO Bind_req/Unbind_req on the source device, Mgmt_Bind_req
//!   pages to read its table.
//!
//! Membership changes and binding requests are desired state: a pending
//! one is (re)sent until the device answers, also after a restart. A
//! sleepy device's request waits in its send queue until it is next heard
//! from (`queued` in the records). Records are JSON values throughout
//! (API, store, backup): no serde derives, which keeps the flash small.

use super::control::{AnswerTo, GroupTail, Then};
use super::*;
use crate::commands::{Action, CommandAck};
use crate::transport::{CoordError, Request};
use crate::zcl::types::Reader;
use crate::zdo::BindDst;

/// Highest group ID (0xFFF8–0xFFFF are reserved).
pub const GROUP_MAX: u16 = 0xFFF7;
/// Store key prefix of groups (`grp/<4 hex>`).
pub const GROUP_KEY: &str = "grp/";
/// Store key prefix of binding records (`bnd/<device id>`).
pub const BIND_KEY: &str = "bnd/";
const MGMT_BIND_REQ: u16 = 0x0033;
const MGMT_BIND_RSP: u16 = 0x8033;
/// Endpoint part of group entity uids.
const GROUP_EP: u8 = 0;

/// Membership of one device endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberState {
    /// Add Group not confirmed yet.
    Pending,
    /// Confirmed.
    Ok,
    /// Remove Group not confirmed yet.
    Removing,
    /// The device refused or did not answer.
    Failed,
}

const STATES: [(MemberState, &str); 4] = [
    (MemberState::Pending, "pending"),
    (MemberState::Ok, "ok"),
    (MemberState::Removing, "removing"),
    (MemberState::Failed, "failed"),
];

impl MemberState {
    fn name(self) -> &'static str {
        STATES
            .iter()
            .find(|s| s.0 == self)
            .map_or("pending", |s| s.1)
    }
}

/// A member endpoint.
#[derive(Debug, Clone)]
pub struct Member {
    /// Device id.
    pub device: String,
    /// Endpoint.
    pub endpoint: u8,
    /// State.
    pub state: MemberState,
    /// Why it failed.
    pub error: Option<String>,
}

/// A scene of a group.
#[derive(Debug, Clone)]
pub struct Scene {
    /// Scene ID.
    pub id: u8,
    /// Name (kept by the hub).
    pub name: String,
    /// Last store per member `<device>/<ep>`: `pending`, `ok` or the
    /// status (runtime only).
    pub results: Vec<(String, String)>,
}

/// A group.
#[derive(Debug, Clone)]
pub struct Group {
    /// Group ID.
    pub id: u16,
    /// Name.
    pub name: String,
    /// The hub's endpoint is a member (it hears groupcasts to the group).
    pub hub_member: bool,
    /// Members.
    pub members: Vec<Member>,
    /// Scenes.
    pub scenes: Vec<Scene>,
    /// Being deleted: gone once every member has left.
    pub deleting: bool,
    /// `NWK_GROUP_MEMBERSHIP` failure (runtime only).
    pub hub_error: Option<String>,
    ents: Option<GroupEntities>,
}

/// One binding table entry of a source device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindEntry {
    /// Source endpoint.
    pub ep: u8,
    /// Cluster.
    pub cluster: u16,
    /// Destination.
    pub dst: BindDst,
}

/// A Bind_req or Unbind_req to send.
#[derive(Debug, Clone)]
pub struct BindOp {
    /// Entry.
    pub entry: BindEntry,
    /// Unbind.
    pub unbind: bool,
    /// Why it failed (then it is not resent).
    pub failed: Option<String>,
}

/// What the hub knows about a device's binding table.
#[derive(Debug, Clone, Default)]
pub struct DeviceBindings {
    /// Source device.
    pub device: String,
    /// Entries as last read (and changed by confirmed requests since).
    pub table: Vec<BindEntry>,
    /// Time of the last complete read, s.
    pub read_at: Option<f64>,
    /// Entries the device reported.
    pub total: Option<u64>,
    /// Requests not confirmed yet.
    pub ops: Vec<BindOp>,
    reading: bool,
    read_error: Option<String>,
    pages: Vec<BindEntry>,
}

/// An outstanding groups/scenes/bindings request (the purpose of a sent
/// frame); endpoints last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZgOp {
    /// Add Group.
    AddGroup(u16, u8),
    /// Remove Group.
    RemoveGroup(u16, u8),
    /// View Group.
    ViewGroup(u16, u8),
    /// Get Group Membership (all groups).
    Membership(u8),
    /// Store Scene.
    StoreScene(u16, u8, u8),
    /// Remove Scene.
    RemoveScene(u16, u8, u8),
    /// View Scene.
    ViewScene(u16, u8, u8),
    /// Bind_req (false) / Unbind_req (true).
    Bind(BindEntry, bool),
    /// Mgmt_Bind_req from this index.
    MgmtBind(u8),
}

/// Runtime group entities. Their records are built when asked for (from
/// the representative member's definition), so a group costs no copy of
/// an entity definition in RAM.
#[derive(Debug, Clone, Default)]
struct GroupEntities {
    /// Change signature of the records.
    sig: String,
    /// The on/off entity's platform and the representative member entity.
    main: Option<(Platform, String)>,
    /// Entity uids (main first, then scenes).
    uids: Vec<String>,
    /// Aggregated state of the on/off entity.
    state: EntityState,
    /// Hash of the last published state message.
    last: u64,
}

/// Groups, scenes and bindings of the hub.
#[derive(Debug, Default)]
pub struct ZgState {
    /// Groups, by ID.
    pub groups: Vec<Group>,
    /// Binding records.
    pub bindings: Vec<DeviceBindings>,
    inflight: Vec<(String, ZgOp)>,
    refresh_at: Vec<(Millis, String)>,
    dirty: bool,
}

impl ZgState {
    fn get(&self, id: u16) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == id)
    }
    fn get_mut(&mut self, id: u16) -> Option<&mut Group> {
        self.groups.iter_mut().find(|g| g.id == id)
    }
    fn busy(&self, dev: &str, op: &ZgOp) -> bool {
        self.inflight.iter().any(|(d, o)| d == dev && o == op)
    }
    fn done(&mut self, dev: &str, op: &ZgOp) {
        self.inflight.retain(|(d, o)| !(d == dev && o == op));
    }
    fn binds(&mut self, dev: &str) -> &mut DeviceBindings {
        let i = match self.bindings.iter().position(|b| b.device == dev) {
            Some(i) => i,
            None => {
                self.bindings.push(DeviceBindings {
                    device: dev.to_string(),
                    ..Default::default()
                });
                self.bindings.len() - 1
            }
        };
        &mut self.bindings[i]
    }
    fn insert(&mut self, g: Group) {
        self.groups.retain(|x| x.id != g.id);
        let i = self.groups.partition_point(|x| x.id < g.id);
        self.groups.insert(i, g);
    }
}

/// `grp:<4 hex>`.
pub fn group_device_id(id: u16) -> String {
    format!("grp:{id:04x}")
}

fn group_of(id: &str) -> Option<u16> {
    u16::from_str_radix(id.strip_prefix("grp:")?.get(..4)?, 16).ok()
}

fn bad(m: impl Into<String>) -> ApiError {
    ApiError::new("invalid_value", m)
}

fn no_group(id: u16) -> ApiError {
    ApiError::new("not_found", format!("no group 0x{id:04X}"))
}

/// A number, or a hex string `0x…`.
fn num_of(v: &J) -> Option<u64> {
    match v {
        J::String(s) => u64::from_str_radix(s.strip_prefix("0x")?, 16).ok(),
        v => v.as_u64(),
    }
}

fn num(v: &J, k: &str) -> Option<u64> {
    num_of(&v[k])
}

fn req_num(v: &J, k: &str, max: u64) -> Result<u64, ApiError> {
    match num(v, k) {
        Some(n) if n <= max => Ok(n),
        Some(n) => Err(bad(format!("{k} {n} out of range"))),
        None => Err(bad(format!("missing {k}"))),
    }
}

fn opt_num(v: &J, k: &str, max: u64) -> Result<Option<u64>, ApiError> {
    match num(v, k) {
        None => Ok(None),
        Some(_) => req_num(v, k, max).map(Some),
    }
}

fn text<'a>(v: &'a J, k: &str) -> Result<&'a str, ApiError> {
    v[k].as_str().ok_or_else(|| bad(format!("missing {k}")))
}

fn ostr(v: &J) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// ZDP status names (R22 Table 2-138).
fn zdp_status(s: u8) -> String {
    match s {
        0x82 => "INVALID_EP".into(),
        0x84 => "NOT_SUPPORTED".into(),
        0x88 => "NO_ENTRY".into(),
        0x8C => "TABLE_FULL".into(),
        0x8D => "NOT_AUTHORIZED".into(),
        s => format!("ZDP status 0x{s:02X}"),
    }
}

/// A ZCL status that completes `op`: SUCCESS, an Add Group of a group the
/// device has (ZCL 7 DUPLICATE_EXISTS 0x8A), a removal of what it does not
/// have (NOT_FOUND).
fn zcl_result(op: &ZgOp, st: u8) -> Result<(), String> {
    match (op, st) {
        (_, 0x00) | (ZgOp::AddGroup(..), 0x8A) => Ok(()),
        (ZgOp::RemoveGroup(..) | ZgOp::RemoveScene(..), 0x8B) => Ok(()),
        (_, 0x89) => Err("INSUFFICIENT_SPACE (table full)".into()),
        (_, s) => Err(match Status(s).name() {
            "UNKNOWN_STATUS" => format!("ZCL status 0x{s:02X}"),
            n => n.into(),
        }),
    }
}

fn zcl_string(r: &mut Reader<'_>) -> Option<String> {
    let n = r.u8().ok()?;
    r.take(usize::from(n))
        .ok()
        .map(|b| String::from_utf8_lossy(b).into_owned())
}

fn mkey(device: &str, ep: u8) -> String {
    format!("{device}/{ep}")
}

fn group_entity(uid: String, def: EntityDef) -> Entity {
    Entity {
        uid,
        r: crate::resolve::ResolvedEntity {
            def,
            ep: GROUP_EP,
            source: LayerSource::Generic,
            ep_name: None,
        },
        state: Default::default(),
        orphaned: false,
        user_name: None,
        user_enabled: None,
    }
}

/// The stored form of a group (also the base of its API record).
fn group_json(g: &Group) -> J {
    json!({
        "id": g.id,
        "name": g.name,
        "hub_member": g.hub_member,
        "deleting": g.deleting,
        "members": g.members.iter().map(|m| json!({
            "device": m.device, "endpoint": m.endpoint, "state": m.state.name(), "error": m.error,
        })).collect::<Vec<_>>(),
        "scenes": g.scenes.iter().map(|s| json!({"id": s.id, "name": s.name})).collect::<Vec<_>>(),
    })
}

/// A group from its stored form.
fn group_from(v: &J) -> Option<Group> {
    let id = num(v, "id").filter(|i| (1..=u64::from(GROUP_MAX)).contains(i))? as u16;
    let list = |k: &str| v[k].as_array().cloned().unwrap_or_default();
    let members = list("members")
        .iter()
        .filter_map(|m| {
            let st = m["state"].as_str().unwrap_or("pending");
            Some(Member {
                device: ostr(&m["device"])?,
                endpoint: num(m, "endpoint")? as u8,
                state: STATES
                    .iter()
                    .find(|s| s.1 == st)
                    .map_or(MemberState::Pending, |s| s.0),
                error: ostr(&m["error"]),
            })
        })
        .collect();
    let scenes = list("scenes")
        .iter()
        .filter_map(|s| {
            Some(Scene {
                id: num(s, "id").filter(|i| *i <= 255)? as u8,
                name: ostr(&s["name"]).unwrap_or_default(),
                results: Vec::new(),
            })
        })
        .collect();
    Some(Group {
        id,
        name: ostr(&v["name"]).unwrap_or_default(),
        hub_member: v["hub_member"] != false,
        members,
        scenes,
        deleting: v["deleting"] == true,
        hub_error: None,
        ents: None,
    })
}

impl<T: ZigbeeTransport> Hub<T> {
    // ------------------------------------------------------------------
    // Records.

    fn zg_queued(&self, dev: &str) -> bool {
        self.devices.get(dev).is_some_and(|d| {
            d.queue
                .iter()
                .any(|q| matches!(q.purpose, Purpose::Group(_)))
        })
    }

    fn group_record(&self, g: &Group) -> J {
        let dev = group_device_id(g.id);
        let mut r = group_json(g);
        r["device"] = dev.clone().into();
        r["hub_error"] = g.hub_error.clone().into();
        r["entity"] = g
            .ents
            .as_ref()
            .filter(|e| e.main.is_some())
            .map(|e| e.uids[0].clone())
            .into();
        for (i, m) in g.members.iter().enumerate() {
            let q = matches!(m.state, MemberState::Pending | MemberState::Removing)
                && self.zg_queued(&m.device);
            r["members"][i]["queued"] = q.into();
        }
        for (i, s) in g.scenes.iter().enumerate() {
            r["scenes"][i]["uid"] = format!("{dev}:{GROUP_EP}:scene_{}", s.id).into();
            let mut res = json!({});
            for (k, v) in &s.results {
                res[k] = v.clone().into();
            }
            r["scenes"][i]["results"] = res;
        }
        r
    }

    fn target_json(&self, dst: &BindDst) -> J {
        match *dst {
            BindDst::Group(g) => json!({"group": g}),
            BindDst::Ieee(ieee, ep) if ieee == self.cfg.coordinator_ieee => {
                json!({"hub": true, "endpoint": ep})
            }
            BindDst::Ieee(ieee, ep) => json!({"device": zigbee_id(ieee), "endpoint": ep}),
        }
    }

    /// A destination: `{group}`, `{hub: true}` (or device `hub`), or
    /// `{device: "zb:<ieee>", endpoint}`.
    fn target_from(&self, t: &J) -> Option<BindDst> {
        if let Some(g) = num(t, "group") {
            return (1..=u64::from(GROUP_MAX))
                .contains(&g)
                .then_some(BindDst::Group(g as u16));
        }
        if t["hub"] == true || t["device"] == "hub" {
            return Some(BindDst::Ieee(
                self.cfg.coordinator_ieee,
                self.cfg.hub_endpoint,
            ));
        }
        let ieee = u64::from_str_radix(t["device"].as_str()?.strip_prefix("zb:")?, 16).ok()?;
        Some(BindDst::Ieee(ieee, num(t, "endpoint")? as u8))
    }

    fn entry_json(&self, e: &BindEntry) -> J {
        json!({"endpoint": e.ep, "cluster": e.cluster, "target": self.target_json(&e.dst)})
    }

    fn entry_from(&self, v: &J) -> Option<BindEntry> {
        Some(BindEntry {
            ep: num(v, "endpoint")? as u8,
            cluster: num(v, "cluster").filter(|c| *c <= 0xFFFF)? as u16,
            dst: self.target_from(&v["target"])?,
        })
    }

    /// The stored form of a binding record (also the base of the API one).
    fn bindings_json(&self, b: &DeviceBindings) -> J {
        json!({
            "device": b.device,
            "read_at": b.read_at,
            "total": b.total,
            "table": b.table.iter().map(|e| self.entry_json(e)).collect::<Vec<_>>(),
            "pending": b.ops.iter().map(|o| {
                let mut j = self.entry_json(&o.entry);
                j["op"] = (if o.unbind { "unbind" } else { "bind" }).into();
                j["error"] = o.failed.clone().into();
                j
            }).collect::<Vec<_>>(),
        })
    }

    fn bindings_record(&self, b: &DeviceBindings) -> J {
        let mut r = self.bindings_json(b);
        let queued = self.zg_queued(&b.device);
        r["reading"] = b.reading.into();
        r["error"] = b.read_error.clone().into();
        for (i, o) in b.ops.iter().enumerate() {
            r["pending"][i]["state"] = match (&o.failed, queued) {
                (Some(_), _) => "failed",
                (None, true) => "queued",
                _ => "pending",
            }
            .into();
        }
        r
    }

    /// `groups/list`.
    pub fn groups_list(&self) -> Vec<J> {
        self.zg
            .groups
            .iter()
            .map(|g| self.group_record(g))
            .collect()
    }

    /// `bindings/list`.
    pub fn bindings_list(&self, device: Option<&str>) -> Vec<J> {
        self.zg
            .bindings
            .iter()
            .filter(|b| device.is_none_or(|x| x == b.device))
            .map(|b| self.bindings_record(b))
            .collect()
    }

    fn zg_event(&mut self, kind: &str, body: J) {
        self.emit(HubEvent::Groups {
            kind: kind.into(),
            body,
        });
    }

    fn group_event(&mut self, id: u16) {
        self.zg.dirty = true;
        match self.zg.get(id) {
            Some(g) => {
                let r = json!({"group": self.group_record(g)});
                self.zg_event("group_updated", r);
            }
            None => self.zg_event("group_removed", json!({"group": id})),
        }
    }

    fn bindings_event(&mut self, dev: &str) {
        self.zg.dirty = true;
        let body = match self.zg.bindings.iter().find(|b| b.device == dev) {
            Some(b) => self.bindings_record(b),
            None => json!({"device": dev, "table": [], "pending": []}),
        };
        self.zg_event("bindings_updated", body);
    }

    /// A sleepy device's groups or bindings requests left its send queue
    /// (it woke up) or went back into it: the records whose `queued`
    /// changed are published again.
    pub(crate) fn zg_queue_changed(&mut self, dev: &str) {
        if self
            .zg
            .bindings
            .iter()
            .any(|b| b.device == dev && (b.reading || !b.ops.is_empty()))
        {
            self.bindings_event(dev);
        }
        let groups: Vec<u16> = self
            .zg
            .groups
            .iter()
            .filter(|g| {
                g.members.iter().any(|m| {
                    m.device == dev
                        && matches!(m.state, MemberState::Pending | MemberState::Removing)
                })
            })
            .map(|g| g.id)
            .collect();
        for g in groups {
            self.group_event(g);
        }
    }

    // ------------------------------------------------------------------
    // Group devices and entities (in `devices/*` and `entities/*`).

    /// Device record of a group device (4.1 fields; `group` = its ID).
    #[inline(never)]
    pub fn zg_device_record(&self, id: &str) -> Option<J> {
        let g = self.zg.get(group_of(id)?)?;
        g.ents.as_ref()?;
        Some(json!({
            "id": group_device_id(g.id),
            "group": g.id,
            "manufacturer": "zigbee-rs",
            "model": "Zigbee group",
            "model_id": format!("0x{:04X}", g.id),
            "name": g.name,
            "via": "hub",
            "power": "mains",
            "available": self.group_available(g),
            "interview": "complete",
            "warnings": [],
            "definition": [],
        }))
    }

    /// Records of the group devices that have entities.
    pub fn zg_device_records(&self) -> Vec<J> {
        self.zg
            .groups
            .iter()
            .filter_map(|g| self.zg_device_record(&group_device_id(g.id)))
            .collect()
    }

    /// Group `g`'s entities, built from the representative member's
    /// definition (`full`; otherwise only what state messages need).
    fn zg_build(&self, g: &Group, full: bool) -> Vec<Entity> {
        let Some(ge) = &g.ents else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some((platform, rep)) = &ge.main {
            let mut def = match self.entity(rep) {
                Some((_, e)) if full => e.r.def.clone(),
                _ => EntityDef::new("", *platform),
            };
            def.key = platform.as_str().to_string();
            (def.name, def.translation_key, def.category) = (None, None, None);
            def.enabled = true;
            let mut e = group_entity(ge.uids[0].clone(), def);
            e.state = ge.state.clone();
            out.push(e);
        }
        for s in &g.scenes {
            let mut def = EntityDef::new(format!("scene_{}", s.id), Platform::Scene);
            def.name = Some(s.name.clone());
            let uid = format!("{}:{GROUP_EP}:scene_{}", group_device_id(g.id), s.id);
            out.push(group_entity(uid, def));
        }
        out
    }

    /// Entity records of the group entities.
    pub fn zg_entity_records(&self) -> Vec<J> {
        let mut out = Vec::new();
        for g in &self.zg.groups {
            let dev = group_device_id(g.id);
            out.extend(self.zg_build(g, true).iter().map(|e| e.record(&dev)));
        }
        out
    }

    /// The entity record of group entity `uid`.
    #[inline(never)]
    pub fn zg_entity_record(&self, uid: &str) -> Option<J> {
        let g = self.zg.get(group_of(uid)?)?;
        let e = self.zg_build(g, true).into_iter().find(|e| e.uid == uid)?;
        Some(e.record(&group_device_id(g.id)))
    }

    /// State messages of the group entities.
    pub fn zg_states(&self) -> Vec<J> {
        let mut out = Vec::new();
        for g in &self.zg.groups {
            let av = self.group_available(g);
            out.extend(self.zg_build(g, false).iter().map(|e| e.state_msg(av)));
        }
        out
    }

    fn group_available(&self, g: &Group) -> bool {
        g.members.iter().any(|m| {
            m.state == MemberState::Ok && self.devices.get(&m.device).is_some_and(|d| d.available)
        })
    }

    /// Member entities of group `g` on `platform`, `(device, index)`.
    fn member_entities(&self, g: &Group, platform: Platform) -> Vec<(String, usize)> {
        let mut out = Vec::new();
        for m in g.members.iter().filter(|m| m.state == MemberState::Ok) {
            if let Some(d) = self.devices.get(&m.device) {
                for (i, e) in d.entities.iter().enumerate() {
                    if !e.orphaned && e.r.ep == m.endpoint && e.r.def.platform == platform {
                        out.push((m.device.clone(), i));
                    }
                }
            }
        }
        out
    }

    fn zg_entities_removed(&mut self, id: u16, old: Option<GroupEntities>) {
        if let Some(old) = old {
            for u in old.uids {
                self.emit(HubEvent::EntityRemoved(u));
            }
            self.emit(HubEvent::DeviceRemoved(group_device_id(id)));
        }
    }

    /// Rebuilds group `id`'s entities when members, scenes or the members'
    /// entities changed; emits the device and entity events.
    fn zg_sync(&mut self, id: u16) {
        let Some(g) = self.zg.get(id).filter(|g| !g.deleting) else {
            let old = self.zg.get_mut(id).and_then(|g| g.ents.take());
            self.zg_entities_removed(id, old);
            return;
        };
        let dev = group_device_id(id);
        // Lights first, then On/Off switches: the representative is the
        // member entity with the most parts (colour capabilities).
        let main = [Platform::Light, Platform::Switch].iter().find_map(|p| {
            self.member_entities(g, *p)
                .into_iter()
                .map(|(d, i)| &self.devices[&d].entities[i])
                .filter(|e| {
                    *p == Platform::Light
                        || matches!(&e.r.def.value.binding, Binding::Zcl(z) if z.write == ZclWrite::OnOffCommands)
                })
                .max_by_key(|e| e.r.def.parts.len())
                .map(|e| (*p, e.uid.clone(), e.r.def.parts.len()))
        });
        let mut sig = format!("{}|", g.name);
        let mut uids = Vec::new();
        if let Some((p, rep, n)) = &main {
            sig += &format!("{rep}:{n}");
            uids.push(format!("{dev}:{GROUP_EP}:{}", p.as_str()));
        }
        for s in &g.scenes {
            sig += &format!("|{}={}", s.id, s.name);
            uids.push(format!("{dev}:{GROUP_EP}:scene_{}", s.id));
        }
        let Some(g) = self.zg.get_mut(id) else {
            return;
        };
        let old = g.ents.take();
        if uids.is_empty() {
            self.zg_entities_removed(id, old);
            return;
        }
        let changed = old.as_ref().is_none_or(|o| o.sig != sig);
        let (old_uids, last) = old
            .as_ref()
            .map_or((Vec::new(), 0), |o| (o.uids.clone(), o.last));
        g.ents = Some(GroupEntities {
            sig,
            main: main.map(|(p, r, _)| (p, r)),
            uids: uids.clone(),
            state: EntityState::default(),
            last,
        });
        if old.is_none() {
            self.emit(HubEvent::DeviceAdded(dev));
        } else if changed {
            self.emit(HubEvent::DeviceUpdated(dev));
        }
        for u in old_uids.iter().filter(|u| !uids.contains(u)) {
            self.emit(HubEvent::EntityRemoved(u.clone()));
        }
        for u in uids {
            if !old_uids.contains(&u) {
                self.emit(HubEvent::EntityAdded(u));
            } else if changed {
                self.emit(HubEvent::EntityUpdated(u));
            }
        }
        self.zg_aggregate(id, !changed);
    }

    /// Recomputes group `id`'s on/off state from its members and publishes
    /// it (always, or only when it changed).
    fn zg_aggregate(&mut self, id: u16, only_changed: bool) {
        let Some(g) = self.zg.get(id) else {
            return;
        };
        let Some(platform) = g.ents.as_ref().and_then(|e| e.main.as_ref()).map(|m| m.0) else {
            return;
        };
        let members = self.member_entities(g, platform);
        let states: Vec<&EntityState> = members
            .iter()
            .map(|(d, i)| &self.devices[d].entities[*i].state)
            .collect();
        let on = |s: &EntityState| match platform {
            Platform::Light => s.parts.get("on_off").and_then(Value::truthy),
            _ => s.value.truthy(),
        };
        let any_on = states.iter().any(|s| on(s) == Some(true));
        let all_known = !states.is_empty() && states.iter().all(|s| on(s).is_some());
        let onoff = if any_on || all_known {
            Value::Bool(any_on)
        } else {
            Value::Null
        };
        let mut st = EntityState {
            last_changed: states.iter().filter_map(|s| s.last_changed).max(),
            last_reported: states.iter().filter_map(|s| s.last_reported).max(),
            ..Default::default()
        };
        if platform == Platform::Light {
            // Brightness: the mean over the lit members (all when none is).
            let lit: Vec<&EntityState> = states
                .iter()
                .copied()
                .filter(|s| !any_on || on(s) == Some(true))
                .collect();
            let b: Vec<f64> = lit
                .iter()
                .filter_map(|s| s.parts.get("brightness").and_then(Value::as_f64))
                .collect();
            if !b.is_empty() {
                let mean = b.iter().sum::<f64>() / b.len() as f64;
                st.parts
                    .insert("brightness".into(), Value::Int(mean.round() as i64));
            }
            for k in [
                "color_temp",
                "color_x",
                "color_y",
                "hue",
                "saturation",
                "color_mode",
            ] {
                if let Some(v) = lit
                    .iter()
                    .find_map(|s| s.parts.get(k).filter(|v| !v.is_null()))
                {
                    st.parts.insert(k.into(), v.clone());
                }
            }
            st.parts.insert("on_off".into(), onoff);
        } else {
            st.value = onoff;
        }
        let available = self.group_available(g);
        let Some(ge) = self.zg.get_mut(id).and_then(|g| g.ents.as_mut()) else {
            return;
        };
        ge.state = st;
        let mut e = group_entity(ge.uids[0].clone(), EntityDef::new("", platform));
        e.state = ge.state.clone();
        let msg = e.state_msg(available);
        let key = format!("{}{}{}", msg["state"], msg["attributes"], available);
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            key.hash(&mut h);
            h.finish()
        };
        if only_changed && ge.last == hash {
            return;
        }
        ge.last = hash;
        self.emit(HubEvent::EntityState(msg));
    }

    /// A member entity's state was published: the groups it is in follow.
    #[inline(never)]
    pub(crate) fn zg_member_changed(&mut self, uid: &str) {
        if self.zg.groups.is_empty() {
            return;
        }
        let dev = device_of_uid(uid);
        let Some(ep) = self
            .devices
            .get(dev)
            .and_then(|d| d.entities.iter().find(|e| e.uid == uid))
            .map(|e| e.r.ep)
        else {
            return;
        };
        let ids: Vec<u16> = self
            .zg
            .groups
            .iter()
            .filter(|g| {
                g.members
                    .iter()
                    .any(|m| m.device == dev && m.endpoint == ep)
            })
            .map(|g| g.id)
            .collect();
        for id in ids {
            self.zg_aggregate(id, true);
        }
    }

    // ------------------------------------------------------------------
    // Sending.

    /// Sends op `op` to `dev` (tracked; queued for a sleepy device).
    fn zg_send(&mut self, dev: &str, op: ZgOp, now: Millis) {
        self.zg.inflight.push((dev.to_string(), op.clone()));
        let le = |g: &u16, x: &[u8]| [&g.to_le_bytes()[..], x].concat();
        let (ep, cluster, cmd, payload) = match &op {
            ZgOp::Bind(e, unbind) => {
                let Some(ieee) = self.devices.get(dev).and_then(Device::ieee) else {
                    return;
                };
                let p = zdo::bind_req(self.next_zdo_tsn(), ieee, e.ep, e.cluster, e.dst);
                let c = if *unbind {
                    zdo::cluster::UNBIND_REQ
                } else {
                    zdo::cluster::BIND_REQ
                };
                self.send_zdo(dev, c, p, Purpose::Group(op), now);
                return;
            }
            ZgOp::MgmtBind(start) => {
                let p = vec![self.next_zdo_tsn(), *start];
                self.send_zdo(dev, MGMT_BIND_REQ, p, Purpose::Group(op), now);
                return;
            }
            // Add Group with an empty name (names are the hub's).
            ZgOp::AddGroup(g, ep) => (*ep, cl::GROUPS, 0x00, le(g, &[0])),
            ZgOp::ViewGroup(g, ep) => (*ep, cl::GROUPS, 0x01, le(g, &[])),
            ZgOp::Membership(ep) => (*ep, cl::GROUPS, 0x02, vec![0]),
            ZgOp::RemoveGroup(g, ep) => (*ep, cl::GROUPS, 0x03, le(g, &[])),
            ZgOp::ViewScene(g, s, ep) => (*ep, cl::SCENES, 0x01, le(g, &[*s])),
            ZgOp::RemoveScene(g, s, ep) => (*ep, cl::SCENES, 0x02, le(g, &[*s])),
            ZgOp::StoreScene(g, s, ep) => (*ep, cl::SCENES, 0x04, le(g, &[*s])),
        };
        let f = ZclFrame::command(FrameType::Cluster, self.next_zcl_tsn(), cmd, None, payload);
        self.send_zcl(dev, ep, cluster, f, Purpose::Group(op), now);
    }

    /// One ZCL command to the group as an APS groupcast (no response).
    fn groupcast(
        &mut self,
        group: u16,
        cluster: u16,
        cmd: u8,
        payload: Vec<u8>,
    ) -> Result<(), ApiError> {
        let mut f = ZclFrame::command(FrameType::Cluster, self.next_zcl_tsn(), cmd, None, payload);
        f.header.disable_default_response = true;
        let req = ApsRequest {
            dst: ApsDst::Group(group),
            dst_ep: 0xFF,
            src_ep: self.cfg.hub_endpoint,
            profile: crate::zcl::PROFILE_HA,
            cluster,
            payload: f.encode(),
            options: TxOptions::default(),
        };
        self.transport
            .submit(Request::Aps(req))
            .map(drop)
            .map_err(|e| ApiError::new("busy", e.to_string()))
    }

    /// (Re)sends what is not confirmed: memberships, removals, bindings;
    /// drops members and records of devices that are gone.
    fn zg_pump(&mut self, now: Millis) {
        let mut todo = Vec::new();
        let mut changed = Vec::new();
        let devices = &self.devices;
        let z = &mut self.zg;
        for g in &mut z.groups {
            let n = g.members.len();
            g.members.retain(|m| devices.contains_key(&m.device));
            if g.members.len() != n {
                changed.push(g.id);
            }
            for m in &g.members {
                let op = match m.state {
                    MemberState::Pending => ZgOp::AddGroup(g.id, m.endpoint),
                    MemberState::Removing => ZgOp::RemoveGroup(g.id, m.endpoint),
                    _ => continue,
                };
                todo.push((m.device.clone(), op));
            }
        }
        let n = z.bindings.len();
        z.bindings.retain(|b| devices.contains_key(&b.device));
        z.dirty |= z.bindings.len() != n;
        for b in &z.bindings {
            for o in b.ops.iter().filter(|o| o.failed.is_none()) {
                todo.push((b.device.clone(), ZgOp::Bind(o.entry, o.unbind)));
            }
        }
        for (d, op) in todo {
            if !self.zg.busy(&d, &op) {
                self.zg_send(&d, op, now);
            }
        }
        for id in changed {
            self.zg_finish_delete(id);
            self.group_event(id);
            self.zg_sync(id);
        }
    }

    /// Drops a deleting group once no member is left.
    fn zg_finish_delete(&mut self, id: u16) {
        self.zg
            .groups
            .retain(|g| g.id != id || !g.deleting || !g.members.is_empty());
    }

    // ------------------------------------------------------------------
    // Answers.

    /// A Groups or Scenes cluster response (server to client); `false`
    /// when it answers nothing of this module.
    #[inline(never)]
    pub(crate) fn zg_zcl(
        &mut self,
        id: &str,
        cluster: u16,
        tsn: u8,
        cmd: u8,
        payload: &[u8],
    ) -> bool {
        let Some(p) = self.take_pending(id, tsn) else {
            return false;
        };
        let Purpose::Group(op) = p.purpose.clone() else {
            self.pending.insert((false, tsn), p);
            return false;
        };
        let mut r = Reader::new(payload);
        let status = r.u8().unwrap_or(0xFF);
        let (kind, mut body) = match (cluster, cmd, &op) {
            (cl::GROUPS, 0x00, ZgOp::AddGroup(..))
            | (cl::GROUPS, 0x03, ZgOp::RemoveGroup(..))
            | (cl::SCENES, 0x02, ZgOp::RemoveScene(..))
            | (cl::SCENES, 0x04, ZgOp::StoreScene(..)) => {
                self.zg_done(id, op.clone(), zcl_result(&op, status));
                return true;
            }
            (cl::GROUPS, 0x01, ZgOp::ViewGroup(g, ep)) => {
                let _ = r.u16();
                let name = zcl_string(&mut r);
                (
                    "group_view",
                    json!({"device": id, "endpoint": ep, "group": g, "name": name}),
                )
            }
            (cl::GROUPS, 0x02, ZgOp::Membership(ep)) => {
                // Capacity (0xFE: at least one more, 0xFF: unknown), count, list.
                let n = r.u8().unwrap_or(0);
                let groups: Vec<u16> = (0..n).map_while(|_| r.u16().ok()).collect();
                self.zg_reconcile(id, *ep, &groups);
                (
                    "group_membership",
                    json!({"device": id, "endpoint": ep, "capacity": status, "groups": groups}),
                )
            }
            (cl::SCENES, 0x01, ZgOp::ViewScene(g, s, ep)) => {
                let mut body = json!({"device": id, "endpoint": ep, "group": g, "scene": s});
                if status == 0 {
                    let _ = (r.u16(), r.u8());
                    body["transition_s"] = r.u16().ok().map(|t| f64::from(t) / 10.0).into();
                    body["name"] = zcl_string(&mut r).into();
                    // Extension field sets: cluster, length, attribute values.
                    let mut sets = Vec::new();
                    while let (Ok(c), Ok(n)) = (r.u16(), r.u8()) {
                        let Ok(b) = r.take(usize::from(n)) else {
                            break;
                        };
                        sets.push(json!({"cluster": c, "data": crate::expr::hex(b)}));
                    }
                    body["extension_fields"] = sets.into();
                }
                ("scene_view", body)
            }
            _ => {
                self.pending.insert((false, tsn), p);
                return false;
            }
        };
        self.zg.done(id, &op);
        if kind != "group_membership" {
            body["status"] = zcl_result(&op, status)
                .err()
                .unwrap_or_else(|| "SUCCESS".into())
                .into();
        }
        self.zg_event(kind, body);
        true
    }

    /// Get Group Membership answered: confirms pending members, notes
    /// members the device no longer has.
    fn zg_reconcile(&mut self, dev: &str, ep: u8, groups: &[u16]) {
        let mut changed = Vec::new();
        for g in &mut self.zg.groups {
            for m in g
                .members
                .iter_mut()
                .filter(|m| m.device == dev && m.endpoint == ep)
            {
                let new = match (m.state, groups.contains(&g.id)) {
                    (MemberState::Pending | MemberState::Failed, true) => MemberState::Ok,
                    (MemberState::Ok, false) => MemberState::Pending,
                    (s, _) => s,
                };
                if new != m.state {
                    m.state = new;
                    m.error = None;
                    changed.push(g.id);
                }
            }
        }
        for id in changed {
            self.group_event(id);
            self.zg_sync(id);
        }
    }

    /// A ZDO response to a request of this module.
    #[inline(never)]
    pub(crate) fn zg_zdo(&mut self, id: &str, op: ZgOp, ind: &ApsIndication, now: Millis) {
        let p = &ind.payload;
        let status = p.get(1).copied().unwrap_or(0xFF);
        match &op {
            ZgOp::MgmtBind(_) if ind.cluster == MGMT_BIND_RSP => self.zg_mgmt_bind(id, op, p, now),
            ZgOp::MgmtBind(_) => {
                self.zg_failed(id, op, ApiError::new("failed", zdp_status(status)))
            }
            // Unbinding an entry the device does not have is done too.
            ZgOp::Bind(_, unbind) if status == 0 || (*unbind && status == 0x88) => {
                self.zg_done(id, op, Ok(()))
            }
            _ => self.zg_done(id, op, Err(zdp_status(status))),
        }
    }

    fn zg_mgmt_bind(&mut self, id: &str, op: ZgOp, p: &[u8], now: Millis) {
        self.zg.done(id, &op);
        let ieee = self.devices.get(id).and_then(Device::ieee);
        let mut r = Reader::new(p.get(1..).unwrap_or_default());
        let status = r.u8().unwrap_or(0xFF);
        if status != 0 {
            return self.zg_failed(id, op, ApiError::new("failed", zdp_status(status)));
        }
        let (total, start, count) = (
            r.u8().unwrap_or(0),
            r.u8().unwrap_or(0),
            r.u8().unwrap_or(0),
        );
        let b = self.zg.binds(id);
        if start == 0 {
            b.pages.clear();
        }
        let mut got = 0u8;
        for _ in 0..count {
            let (Ok(src), Ok(ep), Ok(cluster), Ok(mode)) = (r.u64(), r.u8(), r.u16(), r.u8())
            else {
                break;
            };
            let dst = match (mode, r.u16()) {
                (0x01, Ok(g)) => BindDst::Group(g),
                // IEEE: the u16 read was its low half.
                (0x03, Ok(lo)) => match (r.take(6), r.u8()) {
                    (Ok(hi), Ok(e)) => {
                        let mut b8 = [0u8; 8];
                        b8[..2].copy_from_slice(&lo.to_le_bytes());
                        b8[2..].copy_from_slice(hi);
                        BindDst::Ieee(u64::from_le_bytes(b8), e)
                    }
                    _ => break,
                },
                _ => break,
            };
            got += 1;
            if ieee.is_none_or(|i| i == src) {
                b.pages.push(BindEntry { ep, cluster, dst });
            }
        }
        let next = start.saturating_add(got);
        if got > 0 && next < total {
            return self.zg_send(id, ZgOp::MgmtBind(next), now);
        }
        b.table = std::mem::take(&mut b.pages);
        b.total = Some(u64::from(total));
        b.read_at = Some(ts(now));
        b.reading = false;
        b.read_error = None;
        self.bindings_event(id);
    }

    /// A Default Response to a request of this module.
    pub(crate) fn zg_status(&mut self, id: &str, op: ZgOp, st: Status) {
        match zcl_result(&op, st.0) {
            Err(e)
                if matches!(
                    op,
                    ZgOp::ViewGroup(..) | ZgOp::ViewScene(..) | ZgOp::Membership(_)
                ) =>
            {
                self.zg_failed(id, op, ApiError::new("failed", e))
            }
            r => self.zg_done(id, op, r),
        }
    }

    /// A request of this module was not delivered or not answered.
    pub(crate) fn zg_failed(&mut self, id: &str, op: ZgOp, err: ApiError) {
        self.zg.done(id, &op);
        match op {
            // Refused by the transport: the next pump tries again.
            _ if err.code == "busy" => {}
            ZgOp::MgmtBind(_) => {
                let b = self.zg.binds(id);
                b.reading = false;
                b.read_error = Some(err.message);
                self.bindings_event(id);
            }
            ZgOp::ViewGroup(..) | ZgOp::ViewScene(..) | ZgOp::Membership(_) => {
                self.zg_event("group_error", json!({"device": id, "error": err.message}))
            }
            op => self.zg_done(id, op, Err(err.message)),
        }
    }

    fn zg_done(&mut self, id: &str, op: ZgOp, r: Result<(), String>) {
        self.zg.done(id, &op);
        match op {
            ZgOp::AddGroup(gid, ep) | ZgOp::RemoveGroup(gid, ep) => {
                let adding = matches!(op, ZgOp::AddGroup(..));
                let Some(g) = self.zg.get_mut(gid) else {
                    return;
                };
                let Some(i) = g
                    .members
                    .iter()
                    .position(|m| m.device == id && m.endpoint == ep)
                else {
                    return;
                };
                let m = &mut g.members[i];
                match (adding, m.state, r) {
                    (true, MemberState::Pending, Ok(())) => {
                        m.state = MemberState::Ok;
                        m.error = None;
                    }
                    (false, MemberState::Removing, Ok(())) => {
                        g.members.remove(i);
                    }
                    (true, MemberState::Pending, Err(e))
                    | (false, MemberState::Removing, Err(e)) => {
                        m.state = MemberState::Failed;
                        m.error = Some(e);
                    }
                    // Superseded (removed while the add was in flight).
                    _ => return,
                }
                self.zg_finish_delete(gid);
                self.group_event(gid);
                self.zg_sync(gid);
            }
            ZgOp::StoreScene(gid, sid, ep) => {
                if let Some(s) = self
                    .zg
                    .get_mut(gid)
                    .and_then(|g| g.scenes.iter_mut().find(|s| s.id == sid))
                {
                    let k = mkey(id, ep);
                    s.results.retain(|(x, _)| *x != k);
                    s.results.push((k, r.err().unwrap_or_else(|| "ok".into())));
                    self.group_event(gid);
                }
            }
            ZgOp::Bind(entry, unbind) => {
                let b = self.zg.binds(id);
                let Some(i) = b
                    .ops
                    .iter()
                    .position(|o| o.entry == entry && o.unbind == unbind)
                else {
                    return;
                };
                match r {
                    Ok(()) => {
                        b.ops.remove(i);
                        b.table.retain(|e| *e != entry);
                        if !unbind {
                            b.table.push(entry);
                        }
                    }
                    Err(e) => b.ops[i].failed = Some(e),
                }
                self.bindings_event(id);
            }
            _ => {}
        }
    }

    /// A binding a configure step made (a remote's, `configure_done`):
    /// in the device's binding record like a confirmed Bind_req.
    pub(crate) fn zg_bound(&mut self, id: &str, entry: BindEntry) {
        let b = self.zg.binds(id);
        if !b.table.contains(&entry) {
            b.table.push(entry);
            self.bindings_event(id);
        }
    }

    /// Timers: re-sends, scheduled reads after a recall, entity upkeep.
    #[inline(never)]
    pub(crate) fn zg_tick(&mut self, now: Millis) {
        if self.zg.groups.is_empty() && self.zg.bindings.is_empty() {
            return;
        }
        self.zg_pump(now);
        let due: Vec<String> = self
            .zg
            .refresh_at
            .iter()
            .filter(|(t, _)| *t <= now)
            .map(|(_, u)| u.clone())
            .collect();
        self.zg.refresh_at.retain(|(t, _)| *t > now);
        for uid in due {
            let _ = self.refresh(&uid, now);
        }
        // Members' entities change with re-resolution: a signature check.
        let ids: Vec<u16> = self.zg.groups.iter().map(|g| g.id).collect();
        for id in ids {
            self.zg_sync(id);
        }
    }

    // ------------------------------------------------------------------
    // Requests.

    fn group_mut(&mut self, id: u16) -> Result<&mut Group, ApiError> {
        self.zg
            .get_mut(id)
            .filter(|g| !g.deleting)
            .ok_or_else(|| no_group(id))
    }

    /// `NWK_GROUP_MEMBERSHIP` of the hub endpoint; `tail` and `answer`
    /// follow its outcome ([`Self::membership_done`]).
    fn hub_membership(
        &mut self,
        id: u16,
        add: bool,
        tail: GroupTail,
        answer: Option<AnswerTo>,
        now: Millis,
    ) {
        // A device's configure needs the hub in the group: it stays.
        let add = add || self.configure_groups().contains(&id);
        let req = Request::Group {
            endpoint: self.cfg.hub_endpoint,
            group: id,
            add,
        };
        let then = Then::Membership { group: id, tail };
        self.control(req, then, answer, now);
    }

    /// The hub endpoint's membership changed (or not: `error`), then the
    /// rest of the group request.
    pub(crate) fn membership_done(
        &mut self,
        id: u16,
        tail: GroupTail,
        error: Option<CoordError>,
        now: Millis,
    ) -> Option<Result<J, ApiError>> {
        if let Some(g) = self.zg.get_mut(id) {
            g.hub_error = error.map(|e| e.to_string());
        }
        match tail {
            GroupTail::None => None,
            GroupTail::Create => {
                self.group_event(id);
                Some(self.group_result(id))
            }
            GroupTail::Update => {
                self.group_event(id);
                self.zg_sync(id);
                Some(self.group_result(id))
            }
            GroupTail::Delete => {
                self.zg_pump(now);
                self.zg_finish_delete(id);
                self.group_event(id);
                self.zg_sync(id);
                None
            }
        }
    }

    /// `NWK_GROUP_MEMBERSHIP` add of the hub endpoint for every group that
    /// has it, and for the groups devices' configure joined it to
    /// ([`Self::configure_groups`]): at the start of an NCP session (the
    /// NCP keeps its APS group table across reboots, not across a reset or
    /// for another NCP).
    pub fn groups_resync(&mut self, now: Millis) -> usize {
        let ids = self.hub_groups();
        for id in &ids {
            self.hub_membership(*id, true, GroupTail::None, None, now);
        }
        ids.len()
    }

    /// Groups the hub endpoint is in for devices' configure steps: those of
    /// definitions' `group` ops and of IKEA remotes on group-only firmware
    /// ([`super::ikea`]).
    pub(crate) fn configure_groups(&self) -> Vec<u16> {
        let mut v: Vec<u16> = Vec::new();
        for d in self.devices.values().filter(|d| d.gp.is_none()) {
            let ikea = super::ikea::group_only(&d.fp).then_some(super::ikea::HUB_GROUPS);
            let defs = d.resolution.configure.iter().filter_map(|op| match op {
                ConfigureOp::Group { ep, group } => ep
                    .is_none_or(|e| e == self.cfg.hub_endpoint)
                    .then_some(*group),
                _ => None,
            });
            for g in ikea.into_iter().flatten().chain(defs) {
                if !v.contains(&g) {
                    v.push(g);
                }
            }
        }
        v
    }

    /// Every group the hub endpoint is a member of.
    fn hub_groups(&self) -> Vec<u16> {
        let mut v = self.configure_groups();
        for g in self
            .zg
            .groups
            .iter()
            .filter(|g| g.hub_member && !g.deleting)
        {
            if !v.contains(&g.id) {
                v.push(g.id);
            }
        }
        v
    }

    /// Refuses one more group with the hub endpoint as a member when the
    /// coordinator's group table is full ([`HubConfig::hub_groups_max`]).
    ///
    /// [`HubConfig::hub_groups_max`]: crate::hub::HubConfig::hub_groups_max
    fn hub_group_room(&self) -> Result<(), ApiError> {
        let Some(max) = self.cfg.hub_groups_max else {
            return Ok(());
        };
        if self.hub_groups().len() >= usize::from(max) {
            return Err(ApiError::new(
                "no_space",
                format!(
                    "the coordinator's group table holds {max} groups with the hub as a member: \
                     use \"hub_member\": false, or take the hub out of another group"
                ),
            ));
        }
        Ok(())
    }

    /// `groups/create`.
    pub fn group_create(
        &mut self,
        name: &str,
        id: Option<u16>,
        hub_member: bool,
        now: Millis,
    ) -> Result<J, ApiError> {
        let free = |i: &u16| self.zg.get(*i).is_none() && *i != self.cfg.gp_group;
        let id = match id {
            Some(i) if i == 0 || i > GROUP_MAX => {
                return Err(bad("group IDs are 0x0001–0xFFF7"));
            }
            Some(i) if self.zg.get(i).is_some() => {
                return Err(bad(format!("group 0x{i:04X} exists")));
            }
            Some(i) => i,
            None => (1..=GROUP_MAX)
                .find(free)
                .ok_or_else(|| ApiError::new("no_space", "no free group ID"))?,
        };
        if hub_member {
            self.hub_group_room()?;
        }
        self.zg.insert(Group {
            id,
            name: name.to_string(),
            hub_member,
            members: Vec::new(),
            scenes: Vec::new(),
            deleting: false,
            hub_error: None,
            ents: None,
        });
        if hub_member {
            let ticket = self.answer_ticket();
            self.hub_membership(id, true, GroupTail::Create, Some(ticket), now);
            return self.answer(ticket);
        }
        self.group_event(id);
        self.group_result(id)
    }

    fn group_result(&self, id: u16) -> Result<J, ApiError> {
        self.zg
            .get(id)
            .map(|g| self.group_record(g))
            .ok_or_else(|| no_group(id))
    }

    /// `groups/update`.
    pub fn group_update(
        &mut self,
        id: u16,
        name: Option<&str>,
        hub_member: Option<bool>,
        now: Millis,
    ) -> Result<J, ApiError> {
        let joins = hub_member == Some(true) && self.group_mut(id).is_ok_and(|g| !g.hub_member);
        if joins {
            self.hub_group_room()?;
        }
        let g = self.group_mut(id)?;
        if let Some(n) = name {
            g.name = n.to_string();
        }
        if let Some(h) = hub_member.filter(|h| *h != g.hub_member) {
            g.hub_member = h;
            let ticket = self.answer_ticket();
            self.hub_membership(id, h, GroupTail::Update, Some(ticket), now);
            return self.answer(ticket);
        }
        self.group_event(id);
        self.zg_sync(id);
        self.group_result(id)
    }

    /// `groups/delete`: every member leaves the group (Remove Group, which
    /// also removes its scenes there, ZCL 3.6.2.3.5), then the group is
    /// gone; `force` drops it at once without asking the devices.
    pub fn group_delete(&mut self, id: u16, force: bool, now: Millis) -> Result<(), ApiError> {
        let g = self.zg.get_mut(id).ok_or_else(|| no_group(id))?;
        let hub = g.hub_member && !g.deleting;
        g.deleting = true;
        g.members
            .retain(|m| !force && m.state != MemberState::Failed);
        for m in &mut g.members {
            m.state = MemberState::Removing;
            m.error = None;
        }
        if hub {
            self.hub_membership(id, false, GroupTail::Delete, None, now);
            return Ok(());
        }
        self.zg_pump(now);
        self.zg_finish_delete(id);
        self.group_event(id);
        self.zg_sync(id);
        Ok(())
    }

    /// `groups/add_member`.
    pub fn group_add_member(
        &mut self,
        id: u16,
        device: &str,
        ep: u8,
        now: Millis,
    ) -> Result<J, ApiError> {
        if self.eps_with(device, Some(ep), cl::GROUPS).is_empty() {
            return Err(ApiError::new(
                "unsupported",
                format!("{device} endpoint {ep} has no Groups cluster (0x0004)"),
            ));
        }
        let g = self.group_mut(id)?;
        match g
            .members
            .iter_mut()
            .find(|m| m.device == device && m.endpoint == ep)
        {
            Some(m) if m.state == MemberState::Ok => {}
            Some(m) => {
                m.state = MemberState::Pending;
                m.error = None;
            }
            None => g.members.push(Member {
                device: device.to_string(),
                endpoint: ep,
                state: MemberState::Pending,
                error: None,
            }),
        }
        self.zg.done(device, &ZgOp::AddGroup(id, ep));
        self.zg_pump(now);
        self.group_event(id);
        self.group_result(id)
    }

    /// `groups/remove_member`; `force` forgets it without asking.
    pub fn group_remove_member(
        &mut self,
        id: u16,
        device: &str,
        ep: u8,
        force: bool,
        now: Millis,
    ) -> Result<J, ApiError> {
        let g = self.group_mut(id)?;
        let i = g
            .members
            .iter()
            .position(|m| m.device == device && m.endpoint == ep)
            .ok_or_else(|| {
                ApiError::new("not_found", format!("{device} ep {ep} is not a member"))
            })?;
        if force || g.members[i].state == MemberState::Failed {
            g.members.remove(i);
        } else {
            g.members[i].state = MemberState::Removing;
            g.members[i].error = None;
        }
        self.zg.done(device, &ZgOp::RemoveGroup(id, ep));
        self.zg_pump(now);
        self.group_event(id);
        self.zg_sync(id);
        self.group_result(id)
    }

    fn zg_device_ep(&self, v: &J) -> Result<(String, Option<u8>), ApiError> {
        let dev = text(v, "device")?.to_string();
        let d = self
            .devices
            .get(&dev)
            .filter(|d| d.ieee().is_some())
            .ok_or_else(|| ApiError::new("not_found", format!("no Zigbee device {dev}")))?;
        let ep = num(v, "endpoint").map(|e| e as u8);
        if let Some(e) = ep.filter(|e| d.fp.endpoint(*e).is_none()) {
            return Err(bad(format!("{dev} has no endpoint {e}")));
        }
        Ok((dev, ep))
    }

    /// Endpoints of `dev` (or only `ep`) with server cluster `cluster`.
    fn eps_with(&self, dev: &str, ep: Option<u8>, cluster: u16) -> Vec<u8> {
        self.devices
            .get(dev)
            .filter(|d| d.ieee().is_some())
            .map_or(Vec::new(), |d| {
                d.fp.endpoints
                    .iter()
                    .filter(|e| ep.is_none_or(|x| x == e.ep) && e.in_clusters.contains(&cluster))
                    .map(|e| e.ep)
                    .collect()
            })
    }

    /// Confirmed members of group `id` with server cluster `cluster`.
    fn members_with(&self, id: u16, cluster: u16) -> Vec<(String, u8)> {
        self.zg.get(id).map_or(Vec::new(), |g| {
            g.members
                .iter()
                .filter(|m| {
                    m.state == MemberState::Ok
                        && !self
                            .eps_with(&m.device, Some(m.endpoint), cluster)
                            .is_empty()
                })
                .map(|m| (m.device.clone(), m.endpoint))
                .collect()
        })
    }

    /// `scenes/store`: the members' current state becomes scene `scene`
    /// (a new ID when absent) of the group.
    pub fn scene_store(
        &mut self,
        id: u16,
        scene: Option<u8>,
        name: Option<&str>,
        now: Millis,
    ) -> Result<J, ApiError> {
        let members = self.members_with(id, cl::SCENES);
        let g = self.group_mut(id)?;
        if members.is_empty() {
            return Err(ApiError::new(
                "unsupported",
                "no confirmed member has the Scenes cluster (0x0005)",
            ));
        }
        let sid = match scene {
            Some(s) => s,
            None => (1..=255u8)
                .find(|s| !g.scenes.iter().any(|x| x.id == *s))
                .ok_or_else(|| ApiError::new("no_space", "no free scene ID"))?,
        };
        let i = match g.scenes.iter().position(|s| s.id == sid) {
            Some(i) => i,
            None => {
                let i = g.scenes.partition_point(|s| s.id < sid);
                g.scenes.insert(
                    i,
                    Scene {
                        id: sid,
                        name: format!("Scene {sid}"),
                        results: Vec::new(),
                    },
                );
                i
            }
        };
        let s = &mut g.scenes[i];
        if let Some(n) = name {
            s.name = n.to_string();
        }
        s.results = members
            .iter()
            .map(|(d, e)| (mkey(d, *e), "pending".to_string()))
            .collect();
        for (d, e) in members {
            self.zg_send(&d, ZgOp::StoreScene(id, sid, e), now);
        }
        self.group_event(id);
        self.zg_sync(id);
        self.group_result(id)
    }

    /// `scenes/recall`: one groupcast; the members' states are read again
    /// shortly after (for those that do not report).
    pub fn scene_recall(
        &mut self,
        id: u16,
        scene: u8,
        transition_s: Option<f64>,
        now: Millis,
    ) -> Result<(), ApiError> {
        let g = self
            .zg
            .get(id)
            .filter(|g| !g.deleting && g.scenes.iter().any(|s| s.id == scene))
            .ok_or_else(|| {
                ApiError::new("not_found", format!("no scene {scene} in group 0x{id:04X}"))
            })?;
        let mut uids = Vec::new();
        for p in [Platform::Light, Platform::Switch] {
            for (d, i) in self.member_entities(g, p) {
                if !self.devices[&d].sleepy() {
                    uids.push(self.devices[&d].entities[i].uid.clone());
                }
            }
        }
        let mut pl = id.to_le_bytes().to_vec();
        pl.push(scene);
        let tr = transition_s.map(|t| t.clamp(0.0, 6553.4));
        if let Some(t) = tr {
            pl.extend_from_slice(&((t * 10.0).round() as u16).to_le_bytes());
        }
        self.groupcast(id, cl::SCENES, 0x05, pl)?;
        let at = now + 1500 + tr.map_or(0, |t| (t * 1000.0) as Millis);
        self.zg.refresh_at.extend(uids.into_iter().map(|u| (at, u)));
        Ok(())
    }

    /// `scenes/remove` (also from every member).
    pub fn scene_remove(&mut self, id: u16, scene: u8, now: Millis) -> Result<(), ApiError> {
        let members = self.members_with(id, cl::SCENES);
        let g = self.group_mut(id)?;
        let n = g.scenes.len();
        g.scenes.retain(|s| s.id != scene);
        if g.scenes.len() == n {
            return Err(ApiError::new("not_found", format!("no scene {scene}")));
        }
        for (d, e) in members {
            self.zg_send(&d, ZgOp::RemoveScene(id, scene, e), now);
        }
        self.group_event(id);
        self.zg_sync(id);
        Ok(())
    }

    /// `bindings/create` and `bindings/delete`: Bind_req/Unbind_req for each
    /// cluster on the source device (queued while a sleepy one sleeps).
    pub fn binding_change(&mut self, v: &J, unbind: bool, now: Millis) -> Result<J, ApiError> {
        let (dev, ep) = self.zg_device_ep(v)?;
        let ep = ep.ok_or_else(|| bad("missing endpoint"))?;
        let one = [v["cluster"].clone()];
        let clusters: Vec<u16> = v["clusters"]
            .as_array()
            .map_or(&one[..], Vec::as_slice)
            .iter()
            .map(|c| num_of(c).filter(|c| *c <= 0xFFFF).map(|c| c as u16))
            .collect::<Option<_>>()
            .filter(|c: &Vec<u16>| !c.is_empty())
            .ok_or_else(|| bad("missing or bad clusters"))?;
        let t = &v["target"];
        let dst = self.target_from(t).ok_or_else(|| bad("bad target"))?;
        if !unbind {
            if let BindDst::Ieee(i, _) = dst {
                if i != self.cfg.coordinator_ieee && !self.devices.contains_key(&zigbee_id(i)) {
                    return Err(ApiError::new(
                        "not_found",
                        format!("no device {}", t["device"]),
                    ));
                }
            }
            let e = self.devices[&dev]
                .fp
                .endpoint(ep)
                .ok_or_else(|| bad("missing endpoint"))?;
            if let Some(c) = clusters
                .iter()
                .find(|c| !e.in_clusters.contains(c) && !e.out_clusters.contains(c))
            {
                return Err(bad(format!("endpoint {ep} has no cluster 0x{c:04X}")));
            }
        }
        let b = self.zg.binds(&dev);
        let mut new = Vec::new();
        for cluster in clusters {
            let entry = BindEntry { ep, cluster, dst };
            // A newer request replaces an unconfirmed one for the entry.
            let had = b.ops.len();
            b.ops.retain(|o| o.entry != entry);
            let cancelled = b.ops.len() != had;
            let known = b.table.contains(&entry);
            if (!unbind && known) || (unbind && cancelled && !known) {
                continue;
            }
            b.ops.push(BindOp {
                entry,
                unbind,
                failed: None,
            });
            new.push(ZgOp::Bind(entry, unbind));
        }
        for op in new {
            self.zg.done(&dev, &op);
        }
        self.zg_pump(now);
        self.bindings_event(&dev);
        Ok(self.bindings_list(Some(&dev)).swap_remove(0))
    }

    /// `bindings/read`: Mgmt_Bind_req pages.
    pub fn bindings_read(&mut self, dev: &str, now: Millis) -> Result<J, ApiError> {
        let b = self.zg.binds(dev);
        b.reading = true;
        b.read_error = None;
        b.pages.clear();
        self.zg
            .inflight
            .retain(|(d, o)| !(d == dev && matches!(o, ZgOp::MgmtBind(_))));
        self.zg_send(dev, ZgOp::MgmtBind(0), now);
        self.bindings_event(dev);
        Ok(self.bindings_list(Some(dev)).swap_remove(0))
    }

    /// `entities/command` on a group entity: the representative member's
    /// encoding, sent as groupcasts; the members' states follow
    /// optimistically (and with their next reports).
    #[inline(never)]
    pub(crate) fn group_command(
        &mut self,
        uid: &str,
        command: &str,
        params: &J,
        now: Millis,
    ) -> Result<CommandAck, ApiError> {
        let missing = || ApiError::new("not_found", format!("no entity {uid}"));
        let id = group_of(uid).ok_or_else(missing)?;
        let g = self.zg.get(id).ok_or_else(missing)?;
        let ok = CommandAck {
            ticket: 0,
            status: "ok",
        };
        let ent = self
            .zg_build(g, true)
            .into_iter()
            .find(|e| e.uid == uid)
            .ok_or_else(missing)?;
        if ent.r.def.platform == Platform::Scene {
            if command != "activate" {
                return Err(ApiError::new("unsupported", "scenes take activate"));
            }
            let sid = ent.r.def.key["scene_".len()..]
                .parse()
                .map_err(|_| missing())?;
            let tr = params.get("transition").and_then(J::as_f64);
            self.scene_recall(id, sid, tr, now)?;
            return Ok(ok);
        }
        let rep = g
            .ents
            .as_ref()
            .and_then(|e| e.main.as_ref())
            .map(|m| m.1.clone())
            .ok_or_else(missing)?;
        let d = self.devices.get(device_of_uid(&rep)).ok_or_else(missing)?;
        let plan = self.plan(d, &ent, command, params, now)?;
        let mut sent = false;
        for a in plan.actions {
            if let Action::Cmd {
                cluster,
                mfr: None,
                id: cmd,
                payload,
                ..
            } = a
            {
                self.groupcast(id, cluster, cmd, payload)?;
                sent = true;
            }
        }
        if !sent {
            return Err(ApiError::new(
                "unsupported",
                format!("{command} cannot be sent to a group"),
            ));
        }
        let members = match self.zg.get(id) {
            Some(g) => self.member_entities(g, ent.r.def.platform),
            None => Vec::new(),
        };
        let mut uids = Vec::new();
        for (dev, i) in members {
            let Some(d) = self.devices.get_mut(&dev) else {
                continue;
            };
            for (part, v) in &plan.optimistic {
                let has = part
                    .as_ref()
                    .is_none_or(|p| d.entities[i].r.def.parts.iter().any(|(k, _)| k == p));
                if has {
                    set_value(d, i, part.as_deref(), v.clone(), now);
                }
            }
            uids.push(d.entities[i].uid.clone());
        }
        for u in uids {
            self.publish_state(&u);
        }
        Ok(ok)
    }

    /// Requests `groups/*`, `scenes/*`, `bindings/*`.
    #[inline(never)]
    pub fn zg_request(&mut self, kind: &str, v: &J, now: Millis) -> Result<J, ApiError> {
        let gid = || req_num(v, "group", u64::from(GROUP_MAX)).map(|g| g as u16);
        let sid = || req_num(v, "scene", 255).map(|s| s as u8);
        let ep_of = |x: Option<u8>| x.ok_or_else(|| bad("missing endpoint"));
        Ok(match kind {
            "groups/list" => J::Array(self.groups_list()),
            // The pickers of the web UI: every Zigbee endpoint's clusters.
            "groups/endpoints" => self
                .devices
                .values()
                .filter(|d| d.ieee().is_some())
                .flat_map(|d| {
                    d.fp.endpoints.iter().filter(|e| e.profile != crate::zcl::PROFILE_GP).map(move |e| {
                        json!({"device": d.id, "endpoint": e.ep, "in": e.in_clusters, "out": e.out_clusters})
                    })
                })
                .collect(),
            "groups/create" => {
                let id = opt_num(v, "group", 0xFFFF)?.map(|g| g as u16);
                let hub = v["hub_member"] != false;
                self.group_create(text(v, "name")?, id, hub, now)?
            }
            "groups/update" => self.group_update(
                gid()?,
                v["name"].as_str(),
                v["hub_member"].as_bool(),
                now,
            )?,
            "groups/delete" => {
                self.group_delete(gid()?, v["force"] == true, now)?;
                J::Null
            }
            "groups/add_member" => {
                let (dev, ep) = self.zg_device_ep(v)?;
                self.group_add_member(gid()?, &dev, ep_of(ep)?, now)?
            }
            "groups/remove_member" => {
                let (dev, ep) = self.zg_device_ep(v)?;
                let force = v["force"] == true;
                self.group_remove_member(gid()?, &dev, ep_of(ep)?, force, now)?
            }
            "groups/membership" => {
                let (dev, ep) = self.zg_device_ep(v)?;
                let eps = self.eps_with(&dev, ep, cl::GROUPS);
                if eps.is_empty() {
                    return Err(ApiError::new("unsupported", "no Groups cluster"));
                }
                for e in &eps {
                    self.zg_send(&dev, ZgOp::Membership(*e), now);
                }
                json!({"endpoints": eps})
            }
            "groups/view" => {
                let (dev, ep) = self.zg_device_ep(v)?;
                self.zg_send(&dev, ZgOp::ViewGroup(gid()?, ep_of(ep)?), now);
                J::Null
            }
            "groups/identify" => {
                let t = opt_num(v, "time_s", 0xFFFF)?.unwrap_or(10) as u16;
                self.groupcast(gid()?, cl::IDENTIFY, 0x00, t.to_le_bytes().to_vec())?;
                J::Null
            }
            "scenes/store" => {
                let s = opt_num(v, "scene", 255)?.map(|s| s as u8);
                self.scene_store(gid()?, s, v["name"].as_str(), now)?
            }
            "scenes/recall" => {
                self.scene_recall(gid()?, sid()?, v["transition"].as_f64(), now)?;
                J::Null
            }
            "scenes/remove" => {
                self.scene_remove(gid()?, sid()?, now)?;
                J::Null
            }
            "scenes/update" => {
                let (g, s) = (gid()?, sid()?);
                let n = text(v, "name")?.to_string();
                self.group_mut(g)?
                    .scenes
                    .iter_mut()
                    .find(|x| x.id == s)
                    .ok_or_else(|| ApiError::new("not_found", format!("no scene {s}")))?
                    .name = n;
                self.group_event(g);
                self.zg_sync(g);
                self.group_result(g)?
            }
            "scenes/view" => {
                let (dev, ep) = self.zg_device_ep(v)?;
                self.zg_send(&dev, ZgOp::ViewScene(gid()?, sid()?, ep_of(ep)?), now);
                J::Null
            }
            "bindings/list" => J::Array(self.bindings_list(v["device"].as_str())),
            "bindings/read" => {
                let (dev, _) = self.zg_device_ep(v)?;
                self.bindings_read(&dev, now)?
            }
            "bindings/create" => self.binding_change(v, false, now)?,
            "bindings/delete" => self.binding_change(v, true, now)?,
            _ => return Err(ApiError::new("invalid_format", format!("unknown request {kind}"))),
        })
    }

    // ------------------------------------------------------------------
    // Persistence (`grp/`, `bnd/` in the host store).

    /// Takes the "changed since the last save" flag.
    pub fn zg_take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.zg.dirty)
    }

    /// Records to store: `grp/<4 hex>` → group JSON, `bnd/<device>` →
    /// binding record JSON.
    #[inline(never)]
    pub fn zg_export(&self) -> Vec<(String, Vec<u8>)> {
        let groups = self
            .zg
            .groups
            .iter()
            .map(|g| (format!("{GROUP_KEY}{:04x}", g.id), group_json(g)));
        let binds = self
            .zg
            .bindings
            .iter()
            .filter(|b| !b.table.is_empty() || !b.ops.is_empty() || b.read_at.is_some())
            .map(|b| (format!("{BIND_KEY}{}", b.device), self.bindings_json(b)));
        groups
            .chain(binds)
            .map(|(k, v)| (k, v.to_string().into_bytes()))
            .collect()
    }

    /// Restores one stored record (after the devices); unconfirmed
    /// requests are sent again with the next tick.
    #[inline(never)]
    pub fn zg_import(&mut self, key: &str, bytes: &[u8]) -> Result<(), String> {
        let v: J = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if key.starts_with(GROUP_KEY) {
            let g = group_from(&v).ok_or("not a group")?;
            let id = g.id;
            self.zg.insert(g);
            self.zg_sync(id);
        } else if let Some(d) = key.strip_prefix(BIND_KEY) {
            let entries = |k: &str| -> Vec<(BindEntry, &J)> {
                v[k].as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|e| Some((self.entry_from(e)?, e)))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let table = entries("table").into_iter().map(|(e, _)| e).collect();
            let ops = entries("pending")
                .into_iter()
                .map(|(entry, j)| BindOp {
                    entry,
                    unbind: j["op"] == "unbind",
                    failed: ostr(&j["error"]),
                })
                .collect();
            let (read_at, total) = (v["read_at"].as_f64(), v["total"].as_u64());
            let b = self.zg.binds(d);
            (b.table, b.ops, b.read_at, b.total) = (table, ops, read_at, total);
        }
        Ok(())
    }

    /// Replaces all groups (a restored backup's, in their stored form), and
    /// the hub endpoint's memberships with them.
    #[inline(never)]
    pub fn zg_replace_groups(&mut self, groups: &[J], now: Millis) -> usize {
        let old: Vec<u16> = self.zg.groups.iter().map(|g| g.id).collect();
        for id in old {
            if let Some(g) = self.zg.get_mut(id) {
                g.deleting = true;
            }
            self.zg_sync(id);
            self.zg.groups.retain(|g| g.id != id);
            self.group_event(id);
        }
        for g in groups.iter().filter_map(group_from) {
            let id = g.id;
            self.zg.insert(g);
            self.zg_sync(id);
            self.group_event(id);
        }
        self.groups_resync(now)
    }

    /// Adds imported groups (in their stored form) to the hub's: a group
    /// the hub has gets the members and, if it has none, the name; the
    /// members already hold the group (it is device-side), so nothing is
    /// sent to them. The hub endpoint joins the new groups while the
    /// coordinator's group table has room. How many groups were added or
    /// changed.
    #[inline(never)]
    pub fn zg_merge_groups(&mut self, groups: &[J], now: Millis) -> usize {
        let mut changed = 0;
        for g in groups.iter().filter_map(group_from) {
            let id = g.id;
            let join = match self.zg.get_mut(id) {
                Some(have) if !have.deleting => {
                    let before = (have.name.clone(), have.members.len());
                    if have.name.is_empty() {
                        have.name = g.name;
                    }
                    for m in g.members {
                        if !have
                            .members
                            .iter()
                            .any(|x| x.device == m.device && x.endpoint == m.endpoint)
                        {
                            have.members.push(m);
                        }
                    }
                    if (have.name.clone(), have.members.len()) == before {
                        continue;
                    }
                    false
                }
                _ => {
                    let mut g = g;
                    g.hub_member = g.hub_member && self.hub_group_room().is_ok();
                    let join = g.hub_member;
                    self.zg.insert(g);
                    join
                }
            };
            changed += 1;
            self.zg_sync(id);
            self.group_event(id);
            if join {
                self.hub_membership(id, true, GroupTail::None, None, now);
            }
        }
        changed
    }
}
