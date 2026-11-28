//! Vendor templates the hub implements (spec 17.4): hub code that a
//! definition turns on by name (`templates: [{name: …, params: …}]`,
//! feature `template.<name>`).
//!
//! The family template `tuya` ([`tuya`]) stands for
//! zigbee-herdsman-converters helpers the definition generator could not
//! express in the v1 format: `params.features` names each helper (zhc's
//! name in snake case, e.g. `tuya_on_off` for `tuya.modernExtend.tuyaOnOff`)
//! with its literal arguments, `params.keys` the exposes the helpers give.
//! The hub expands them at resolution into ordinary entities, configure
//! ops and triggers ([`expand`]); what this firmware does not
//! implement (a helper, an argument, a key) is a required feature it lacks
//! (`template.<family>.<item>`, [`unsupported`]), so such a definition is
//! refused and named as before.
//!
//! `tuya_metering_zero_debounce` is zigbee-herdsman-converters'
//! `tuya.fz.TS011F_electrical_measurement` (Tuya TS011F plugs,
//! `TS011F_plug_1` and `TS011F_with_threshold`): these plugs report short
//! spurious zeros, so a zero power, current or voltage reading (Electrical
//! Measurement 0x050B, 0x0508, 0x0505) is held back for twice the
//! attribute's minimum reporting interval plus one second (5 s when it
//! has none) and published only if no other reading of it came meanwhile;
//! a held zero current also sets the power to 0 (the plug is slow to
//! report that). While the plug's switch is off, every Electrical
//! Measurement report sets the power to 0 at once.

use crate::device::{Device, Millis};
use crate::expr::Value;
use crate::model::{Binding, ReportSpec};
use crate::values::DecodeFlags;

/// Template name of the TS011F metering debounce.
pub const METERING_ZERO_DEBOUNCE: &str = "tuya_metering_zero_debounce";

const ELECTRICAL: u16 = 0x0B04;
const ACTIVE_POWER: u16 = 0x050B;
const RMS_CURRENT: u16 = 0x0508;
const RMS_VOLTAGE: u16 = 0x0505;

/// Decoded values of one dispatch: `(entity, part, value, flags)`.
pub(crate) type Decoded = Vec<(usize, Option<usize>, Value, DecodeFlags)>;

/// The `(endpoint, attribute)` of an entity whose own value is a ZCL
/// attribute of `cluster`.
fn zcl_of(d: &Device, i: usize, cluster: u16) -> Option<(u8, u16)> {
    let e = &d.entities[i];
    match &e.r.def.value.binding {
        Binding::Zcl(z) if z.cluster == cluster && z.mfr.is_none() && !e.orphaned => {
            Some((e.r.ep, z.attr))
        }
        _ => None,
    }
}

fn find(d: &Device, ep: u8, cluster: u16, attr: u16) -> Option<usize> {
    (0..d.entities.len()).find(|&i| zcl_of(d, i, cluster) == Some((ep, attr)))
}

/// A zero of the same kind as `v` (scaled values are floats).
fn zero_like(v: &Value) -> Value {
    match v {
        Value::Float(_) => Value::Float(0.0),
        _ => Value::Int(0),
    }
}

/// How long a zero of entity `i` is held: zhc's (minimum reporting
/// interval × 2 + 1) s.
fn hold_ms(d: &Device, i: usize) -> Millis {
    let e = &d.entities[i];
    let min = match crate::reporting::value_report(d, e.r.ep, &e.r.def.value) {
        Some(ReportSpec::Config { min, .. }) => u64::from(min),
        _ => 5,
    };
    (min * 2 + 1) * 1000
}

/// Applies the template to the values decoded from one report (or read):
/// zero readings are held back, other readings cancel a held zero, and
/// power is 0 while the switch of its endpoint is off.
pub(crate) fn zero_debounce(d: &mut Device, decoded: &mut Decoded, now: Millis) {
    let mut out: Decoded = Vec::with_capacity(decoded.len());
    let mut eps: Vec<u8> = Vec::new();
    for (i, p, v, f) in decoded.drain(..) {
        let m = zcl_of(d, i, ELECTRICAL)
            .filter(|(_, a)| p.is_none() && matches!(*a, ACTIVE_POWER | RMS_CURRENT | RMS_VOLTAGE));
        if let Some((ep, _)) = m {
            if !eps.contains(&ep) {
                eps.push(ep);
            }
            if v.as_f64() == Some(0.0) {
                let at = now + hold_ms(d, i);
                d.entities[i].state.held = Some((at, v));
                continue;
            }
            d.entities[i].state.held = None;
        }
        out.push((i, p, v, f));
    }
    for ep in eps {
        let off = find(d, ep, 0x0006, 0x0000)
            .is_some_and(|s| d.entities[s].state.value == Value::Bool(false));
        if let (true, Some(pw)) = (off, find(d, ep, ELECTRICAL, ACTIVE_POWER)) {
            let zero = match out.iter().position(|w| w.0 == pw && w.1.is_none()) {
                Some(k) => zero_like(&out.remove(k).2),
                None => d.entities[pw]
                    .state
                    .held
                    .take()
                    .map_or_else(|| zero_like(&d.entities[pw].state.value), |h| h.1),
            };
            d.entities[pw].state.held = None;
            out.push((pw, None, zero, DecodeFlags::default()));
        }
    }
    *decoded = out;
}

/// The held zeros that are due, as `(entity, value)`: a zero current also
/// zeroes the power of its endpoint.
pub(crate) fn due(d: &mut Device, now: Millis) -> Vec<(usize, Value)> {
    let mut out = Vec::new();
    for i in 0..d.entities.len() {
        let Some((_, v)) = d.entities[i].state.held.take_if(|h| now >= h.0) else {
            continue;
        };
        if let Some((ep, RMS_CURRENT)) = zcl_of(d, i, ELECTRICAL) {
            if let Some(pw) = find(d, ep, ELECTRICAL, ACTIVE_POWER) {
                d.entities[pw].state.held = None;
                out.retain(|(j, _)| *j != pw);
                let z = zero_like(&d.entities[pw].state.value);
                out.push((pw, z));
            }
        }
        out.retain(|(j, _)| *j != i);
        out.push((i, v));
    }
    out
}

// ---------------------------------------------------------------------------
// Family templates (`tuya`).

pub mod tuya;

use serde_json::Value as J;

use crate::expr::{Expr, ExprCtx};
use crate::model::{
    Category, ConfigureOp, Definition, EntityDef, EventSel, Platform, Template, Trigger,
    TriggerSource, ZclBinding, ZclWrite,
};
use crate::zcl::ZclType;

/// `params` of a template from its CBOR (text keys; anything a JSON value
/// cannot hold is dropped).
pub fn params_from_cbor(b: &[u8]) -> J {
    fn item(r: &mut zdef::cbor::Reader<'_>, depth: u8) -> Result<J, zdef::cbor::CborError> {
        use zdef::cbor::Head;
        let at = r.position();
        let h = r.peek()?;
        Ok(match h {
            Head::Map(_) if depth < 8 => {
                let n = r.map()?;
                let mut m = serde_json::Map::new();
                for _ in 0..n {
                    let k = r.text()?.to_string();
                    m.insert(k, item(r, depth + 1)?);
                }
                J::Object(m)
            }
            Head::Array(_) if depth < 8 => {
                let n = r.array()?;
                J::Array(
                    (0..n)
                        .map(|_| item(r, depth + 1))
                        .collect::<Result<_, _>>()?,
                )
            }
            Head::Text(_) => J::from(r.text()?),
            Head::Uint(_) | Head::Nint(_) => J::from(r.int()?),
            Head::True | Head::False => J::from(r.bool()?),
            Head::F16(_) | Head::F32(_) | Head::F64(_) => {
                r.head()?;
                r.float_head(h, at).map_or(J::Null, J::from)
            }
            _ => {
                r.skip()?;
                J::Null
            }
        })
    }
    item(&mut zdef::cbor::Reader::new(b), 0).unwrap_or(J::Null)
}

/// What the family templates of a definition add to its resolution.
#[derive(Debug, Default)]
pub struct Expansion {
    /// Entities.
    pub entities: Vec<EntityDef>,
    /// Configure ops.
    pub configure: Vec<ConfigureOp>,
    /// Generic entities dropped: key, endpoint (`None`: all).
    pub drop: Vec<(&'static str, Option<u8>)>,
    /// The generic remote events (5.4) stay on the `action` event
    /// entity: the device sends ZCL commands in one of its modes.
    pub remote: bool,
    /// zhc `tuya.modernExtend.electricityMeasurementPoll`: bit 0 Electrical
    /// Measurement voltage, current and power, bit 1 Metering summation,
    /// read every 60 s.
    pub poll: u8,
}

/// One family template's expansion in progress.
pub(crate) struct Cx<'a> {
    pub d: &'a Definition,
    fam: &'static str,
    pub x: Expansion,
    /// The `action` event entity the features build together.
    pub action: Option<EntityDef>,
    /// `params.keys`; `None` for a partial definition (no list).
    pub keys: Option<Vec<&'a str>>,
    /// Exposes provided (an endpoint name may follow as `_<name>`).
    provided: Vec<String>,
    /// Endpoint names the arguments use.
    ep_names: Vec<&'a str>,
    /// What this firmware lacks: `template.<family>.<item>`.
    missing: Vec<String>,
}

impl<'a> Cx<'a> {
    /// Notes an item this firmware does not implement.
    pub fn lack(&mut self, item: &str) {
        self.missing
            .push(cat("template.", self.fam, &cat(".", item, "")));
    }
    /// Notes exposes given (by these entities or the generic tier).
    pub fn provide(&mut self, keys: &[&str]) {
        self.provided.extend(keys.iter().map(|k| k.to_string()));
    }
    /// An endpoint name of the definition, its helpers' arguments, or a
    /// conventional one (zhc's `endpoint` functions the definition could
    /// not carry, e.g. two names for one endpoint).
    fn is_ep_name(&self, s: &str) -> bool {
        const USUAL: [&str; 7] = ["left", "right", "center", "top", "bottom", "both", "all"];
        self.ep_names.contains(&s)
            || self.d.endpoints.values().any(|n| n == s)
            || USUAL.contains(&s)
    }
    /// The endpoint of an endpoint name (`endpoints:` of the definition, or
    /// a number).
    pub fn ep_of(&self, name: &str) -> Option<u8> {
        self.d
            .endpoints
            .iter()
            .find(|(_, n)| *n == name)
            .map(|(e, _)| *e)
            .or_else(|| name.parse().ok())
    }
    /// Endpoints of an argument listing endpoint names (`[None]` without
    /// it); an unknown name is an argument this firmware lacks.
    pub fn eps(&mut self, p: &'a J, arg: &str, feature: &str) -> Vec<Option<u8>> {
        let Some(l) = p.get(arg).and_then(J::as_array) else {
            return vec![None];
        };
        let mut v = Vec::new();
        for n in l.iter().filter_map(J::as_str) {
            self.ep_names.push(n);
            match self.ep_of(n) {
                Some(e) => v.push(Some(e)),
                None => self.lack(&cat(feature, ".", arg)),
            }
        }
        v
    }
    /// Arguments of `p` this firmware does not implement (any truthy one
    /// not in `known`).
    pub fn args(&mut self, feature: &str, p: &J, known: &[u32]) {
        for (k, v) in p.as_object().into_iter().flatten() {
            if !known.contains(&h(k)) && !matches!(v, J::Bool(false) | J::Null) {
                self.lack(&cat(feature, ".", k));
            }
        }
    }
    /// Adds an entity and its expose.
    pub fn add(&mut self, e: EntityDef) {
        self.provided.push(e.key.clone());
        self.x.entities.push(e);
    }
    /// Adds a command trigger to the `action` event entity with its event
    /// types.
    #[inline(never)]
    pub fn on(
        &mut self,
        ep: Option<u8>,
        cluster: u16,
        id: u8,
        ev: EventSel,
        types: &[String],
    ) -> &mut Trigger {
        let source = TriggerSource::Cmd {
            ep,
            cluster,
            id,
            mfr: None,
            global: false,
        };
        let a = self.action.get_or_insert_with(|| {
            let mut e = EntityDef::new("action", Platform::Event);
            e.device_class = Some("button".into());
            e
        });
        for ty in types {
            if !a.event_types.contains(ty) {
                a.event_types.push(ty.clone());
            }
        }
        a.triggers.push(Trigger {
            source,
            when: None,
            event: ev,
            attrs: Vec::new(),
        });
        a.triggers.last_mut().unwrap()
    }
}

/// The kind of a template entity on one attribute ([`Row`]).
#[derive(Debug, Clone, Copy)]
pub(crate) enum K {
    /// A select over raw values.
    Select(&'static [(i64, &'static str)]),
    /// A switch whose raw on is this (off: the other of 0 and 1); `-1`: a
    /// boolean attribute.
    Switch(i8),
    /// A number: minimum, maximum, unit. On/Off 0x4001 is the Tuya
    /// countdown, set with On With Timed Off.
    Number(f32, f32, &'static str),
}

/// A template entity on one attribute: expose key, kind, cluster,
/// attribute, type, with the Lumi manufacturer code 0x115F.
pub(crate) type Row = (&'static str, K, u16, u16, ZclType, bool);

/// The entity of a [`Row`] on `ep` (`None`: the definition's default);
/// settings are configuration entities.
pub(crate) fn build(r: &Row, ep: Option<u8>) -> EntityDef {
    let &(key, k, cluster, attr, ty, lumi) = r;
    let p = match k {
        K::Select(_) => Platform::Select,
        K::Switch(_) => Platform::Switch,
        K::Number(..) => Platform::Number,
    };
    let mut e = EntityDef::new(key, p);
    e.ep = ep;
    let countdown = (cluster, attr) == (0x0006, 0x4001);
    if !countdown {
        e.category = Some(Category::Config);
    }
    let s = |x: &str| (!x.is_empty()).then(|| x.to_string());
    let t = &mut e.value.transforms;
    match k {
        K::Select(m) => t.map = m.iter().map(|(r, l)| (*r, l.to_string())).collect(),
        K::Switch(on) if on >= 0 => {
            t.value_on = Some(Value::Int(i64::from(on)));
            t.value_off = Some(Value::Int(i64::from(1 - on)));
        }
        K::Number(min, max, unit) => {
            e.unit = s(unit);
            let l = &mut e.value.limits;
            (l.min, l.max, l.step) = (Some(f64::from(min)), Some(f64::from(max)), Some(1.0));
        }
        K::Switch(_) => {}
    }
    e.value.binding = crate::model::Binding::Zcl(ZclBinding {
        cluster,
        attr,
        ty,
        mfr: lumi.then_some(0x115F),
        write: if countdown {
            ZclWrite::OnWithTimedOff
        } else {
            ZclWrite::Attribute
        },
    });
    e
}

/// The family templates' expressions: one type (one copy of the code in
/// the firmware).
pub(crate) enum TExpr {
    /// `<pre><label><post>` of the raw value (`x`; `payload[0]` for a
    /// command); `other` for a value not in `map`.
    Label {
        map: Vec<(i64, String)>,
        pre: String,
        post: String,
        cmd: bool,
        other: Option<String>,
    },
    /// `payload[0]`.
    Byte,
}

impl std::fmt::Debug for TExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("template")
    }
}

impl crate::expr::Expression for TExpr {
    fn eval(&self, c: &ExprCtx<'_>) -> Value {
        let first = c.payload.and_then(|p| p.first()).map(|b| f64::from(*b));
        let x = c.x.as_f64();
        match self {
            Self::Label {
                map,
                pre,
                post,
                cmd,
                other,
            } => {
                let v = if *cmd { first } else { x };
                let hit = map.iter().find(|(r, _)| Some(*r as f64) == v);
                hit.map(|m| &m.1)
                    .or(other.as_ref())
                    .map_or(Value::Null, |l| Value::Str(cat(pre, l, post)))
            }
            Self::Byte => first.map_or(Value::Null, |b| Value::Int(b as i64)),
        }
    }
}

/// FNV-1a hash of a name: the firmware keeps the hashes of the helper,
/// argument and model names it matches, not the names.
pub(crate) const fn h(s: &str) -> u32 {
    let b = s.as_bytes();
    let mut x: u32 = 0x811C_9DC5;
    let mut i = 0;
    while i < b.len() {
        x = (x ^ b[i] as u32).wrapping_mul(0x0100_0193);
        i += 1;
    }
    x
}

/// `a`, `b` and `c` in one string.
#[inline(never)]
pub(crate) fn cat(a: &str, b: &str, c: &str) -> String {
    let mut s = String::with_capacity(a.len() + b.len() + c.len());
    s.push_str(a);
    s.push_str(b);
    s.push_str(c);
    s
}

/// A shared [`TExpr`].
pub(crate) fn tx(e: TExpr) -> Expr {
    std::sync::Arc::new(e)
}

/// [`TExpr::Label`] (`cmd`: of `payload[0]`).
#[inline(never)]
pub(crate) fn label(map: &[(i64, &str)], post: &str, cmd: bool, other: Option<&str>) -> TExpr {
    TExpr::Label {
        map: map.iter().map(|(r, l)| (*r, l.to_string())).collect(),
        pre: String::new(),
        post: post.into(),
        cmd,
        other: other.map(String::from),
    }
}

/// Expands one family template (`None`: not a family template).
fn run<'a>(d: &'a Definition, t: &'a Template) -> Option<Cx<'a>> {
    if t.name != "tuya" {
        return None;
    }
    let fam = "tuya";
    let mut cx = Cx {
        d,
        fam,
        x: Expansion::default(),
        action: None,
        keys: t
            .params
            .get("keys")
            .and_then(J::as_array)
            .map(|k| k.iter().filter_map(J::as_str).collect()),
        provided: Vec::new(),
        ep_names: Vec::new(),
        missing: Vec::new(),
    };
    let feats = t.params.get("features").and_then(J::as_object);
    for (f, p) in feats.into_iter().flatten() {
        // A helper used with different arguments: `{instances: [...]}`.
        let one = std::slice::from_ref(p);
        let inst = match p.get("instances").and_then(J::as_array) {
            Some(l) if p.as_object().is_some_and(|o| o.len() == 1) => l.as_slice(),
            _ => one,
        };
        for p in inst {
            if !tuya::feature(&mut cx, f, p) {
                cx.lack(f);
            }
        }
    }
    if let Some(a) = cx.action.take() {
        cx.add(a);
    }
    Some(cx)
}

/// What the family templates of `d` need that this firmware lacks
/// (`template.<family>.<helper>[.<argument>]`, `template.<family>.expose.<key>`
/// for an expose no implemented helper gives).
#[inline(never)]
pub fn unsupported(d: &Definition) -> Vec<String> {
    let mut out = Vec::new();
    for t in &d.templates {
        let Some(cx) = run(d, t) else { continue };
        for k in cx.keys.iter().flatten() {
            let given = cx.provided.iter().any(|b| {
                k == b
                    || k.strip_prefix(b.as_str())
                        .and_then(|r| r.strip_prefix('_'))
                        .is_some_and(|s| cx.is_ep_name(s))
            });
            if !given {
                out.push(cat("template.", cx.fam, &cat(".expose.", k, "")));
            }
        }
        out.extend(cx.missing);
    }
    out
}

/// What the family templates of `d` add to its resolution.
#[inline(never)]
pub fn expand(d: &Definition) -> Expansion {
    let mut x = Expansion::default();
    for t in &d.templates {
        if let Some(cx) = run(d, t) {
            x.entities.extend(cx.x.entities);
            x.configure.extend(cx.x.configure);
            x.drop.extend(cx.x.drop);
            x.remote |= cx.x.remote;
            x.poll |= cx.x.poll;
        }
    }
    x
}

/// Drops the generic entities the templates stand for (3.3 merge, `merged`
/// by endpoint and key); returns the generic `action` entity whose remote
/// events (5.4) go to the template's or definition's.
#[inline(never)]
pub fn drop_generic(
    merged: &mut std::collections::BTreeMap<(u8, String), Box<crate::resolve::ResolvedEntity>>,
    x: &Expansion,
) -> Option<EntityDef> {
    let remote = |k: &str, r: &crate::resolve::ResolvedEntity| {
        x.remote && k == "action" && r.def.platform == Platform::Event
    };
    let mut gone = Vec::new();
    for ((e, k), r) in merged.iter() {
        if r.source == crate::model::LayerSource::Generic
            && (remote(k, r)
                || x.drop
                    .iter()
                    .any(|(dk, p)| dk == k && p.is_none_or(|p| p == *e)))
        {
            gone.push((*e, k.clone()));
        }
    }
    let mut out = None;
    for k in gone {
        if let Some(r) = merged.remove(&k) {
            if remote(&k.1, &r) {
                out = Some(r.def);
            }
        }
    }
    out
}

/// Adds the generic remote events (`g`, 5.4) to an `action` entity: its
/// triggers after the entity's own (those for commands the entity does not
/// handle), and their event types to a template's entity (a definition's
/// list stays zhc's).
#[inline(never)]
pub fn with_remote(e: &mut EntityDef, g: EntityDef, types: bool) {
    let own = |t: &Trigger| match t.source {
        TriggerSource::Cmd { cluster, id, .. } => Some((cluster, id)),
        _ => None,
    };
    let n = e.triggers.len();
    for t in g.triggers {
        let k = own(&t);
        if k.is_some() && !e.triggers[..n].iter().any(|o| own(o) == k) {
            e.triggers.push(t);
        }
    }
    if types && !e.event_types.is_empty() {
        for ty in g.event_types {
            if !e.event_types.contains(&ty) {
                e.event_types.push(ty);
            }
        }
    }
}
