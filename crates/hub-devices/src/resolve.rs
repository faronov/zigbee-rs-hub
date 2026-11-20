//! Resolution (spec 3.3): choose the definitions for a fingerprint, merge
//! the layers generic < catalog < self-description < user < overrides by
//! entity key, apply the generic knobs, and produce one resolved entity
//! set with stable endpoints.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::expr::{ExprCtx, ExprEnv, Value};
use crate::fingerprint::Fingerprint;
use crate::model::*;
use crate::zcl::{PROFILE_GP, cluster as cl};

/// Features this engine build implements (8.5 `requires`).
pub const HUB_FEATURES: &[&str] = &[
    "expr-v1",
    "tuya",
    "gp",
    "struct.zcl_tagged",
    "codec.tuya_schedule_day",
    "codec.bcd_time",
    "codec.hhmm",
    "codec.ascii",
    "codec.utf16be_hex",
    "codec.bitmap_flags",
];

/// A source of definitions (bank + fetched cache, or user uploads). The
/// production store (`zdef`, section 9) answers from its index; it may
/// return a superset, the resolver re-checks every clause.
pub trait Catalog {
    /// Candidate definitions for a fingerprint.
    fn candidates(&self, fp: &Fingerprint) -> Vec<Arc<Definition>>;
    /// A definition by ID (pinning, API).
    fn get(&self, id: &str) -> Option<Arc<Definition>>;
    /// All definitions (API listing).
    fn all(&self) -> Vec<Arc<Definition>>;
}

/// An in-memory catalog.
#[derive(Debug, Default, Clone)]
pub struct MemoryCatalog {
    /// Definitions.
    pub defs: Vec<Arc<Definition>>,
}

impl MemoryCatalog {
    /// Adds or replaces (same `id`) a definition.
    pub fn insert(&mut self, d: Definition) {
        self.defs.retain(|x| x.id != d.id);
        self.defs.push(Arc::new(d));
    }
    /// Removes by ID.
    pub fn remove(&mut self, id: &str) -> bool {
        let n = self.defs.len();
        self.defs.retain(|x| x.id != id);
        n != self.defs.len()
    }
}

impl Catalog for MemoryCatalog {
    fn candidates(&self, _fp: &Fingerprint) -> Vec<Arc<Definition>> {
        self.defs.clone()
    }
    fn get(&self, id: &str) -> Option<Arc<Definition>> {
        self.defs.iter().find(|d| d.id == id).cloned()
    }
    fn all(&self) -> Vec<Arc<Definition>> {
        self.defs.clone()
    }
}

/// `gpd.*` and `fp.*` for `present_if` (7.3).
pub struct FpEnv<'a>(pub &'a Fingerprint);

impl ExprEnv for FpEnv<'_> {
    fn gpd(&self, f: &str) -> Value {
        let Some(g) = &self.0.gp else {
            return Value::Null;
        };
        match f {
            "device_id" => Value::Int(i64::from(g.device_id)),
            "manufacturer_id" => g
                .manufacturer_id
                .map_or(Value::Null, |v| Value::Int(i64::from(v))),
            "model_id" => g.model_id.map_or(Value::Null, |v| Value::Int(i64::from(v))),
            "src_id" => g
                .gpd_id
                .src_id()
                .map_or(Value::Null, |v| Value::Int(i64::from(v))),
            _ => Value::Null,
        }
    }
    fn fp(&self, f: &str) -> Value {
        let o = |v: Option<u32>| v.map_or(Value::Null, |v| Value::Int(i64::from(v)));
        match f {
            "app_version" => o(self.0.app_version),
            "hw_version" => o(self.0.hw_version),
            "stack_version" => o(self.0.stack_version),
            "mfr_code" => o(self.0.mfr_code.map(u32::from)),
            "sw_build" => self.0.sw_build_id.clone().map_or(Value::Null, Value::Str),
            "zcl_version" => o(self.0.zcl_version),
            "date_code" => self.0.date_code.clone().map_or(Value::Null, Value::Str),
            "manufacturer" => self
                .0
                .manufacturer_name
                .as_deref()
                .map_or(Value::Null, |m| {
                    Value::Str(zdef::matching::trim_name(m).to_string())
                }),
            "model" => self.0.model_id.as_deref().map_or(Value::Null, |m| {
                Value::Str(zdef::matching::trim_name(m).to_string())
            }),
            _ => Value::Null,
        }
    }
}

fn in_range(v: Option<u32>, r: Option<(u32, u32)>) -> bool {
    match r {
        None => true,
        Some((lo, hi)) => v.is_some_and(|v| v >= lo && v <= hi),
    }
}

/// Specificity score of one clause (3.3, 13.3), `None` if it does not
/// match.
pub fn score_clause(c: &MatchClause, fp: &Fingerprint) -> Option<u32> {
    if let Some(g) = &c.gp {
        let gf = fp.gp.as_ref()?;
        if g.device_id.is_some_and(|d| d != gf.device_id) {
            return None;
        }
        if let Some(m) = g.manufacturer_id {
            if gf.manufacturer_id != Some(m) {
                return None;
            }
        }
        if let Some((v, mask)) = g.model_id {
            if gf.model_id.is_none_or(|x| x & mask != v & mask) {
                return None;
            }
        }
        if let Some((lo, hi)) = g.src_id {
            if gf.gpd_id.src_id().is_none_or(|s| s < lo || s > hi) {
                return None;
            }
        }
        if let Some((prefix, n)) = g.ieee_prefix {
            let crate::gp::types::GpdId::Ieee { ieee, .. } = gf.gpd_id else {
                return None;
            };
            let shift = 64 - 8 * u32::from(n.min(8));
            if n > 0 && (ieee >> shift) != (prefix >> shift) {
                return None;
            }
        }
        if !g.commands.iter().all(|x| gf.commands.contains(x))
            || !g
                .server_clusters
                .iter()
                .all(|x| gf.server_clusters.contains(x))
            || !g
                .client_clusters
                .iter()
                .all(|x| gf.client_clusters.contains(x))
        {
            return None;
        }
        let base = if (g.manufacturer_id.is_some() && g.model_id.is_some())
            || (g.src_id.is_some() && g.device_id.is_some())
        {
            600
        } else if g.device_id.is_some() && !g.commands.is_empty() {
            300
        } else {
            200
        };
        return Some(base);
    }
    if fp.gp.is_some() {
        return None;
    }
    let man = fp.manufacturer_name.as_deref();
    let model = fp.model_id.as_deref();
    if let Some(m) = &c.manufacturer {
        if !man.is_some_and(|v| m.matches(v)) {
            return None;
        }
    }
    if let Some(m) = &c.model {
        if !model.is_some_and(|v| m.matches(v)) {
            return None;
        }
    }
    if c.mfr_code.is_some_and(|m| fp.mfr_code != Some(m))
        || !in_range(fp.app_version, c.app_version)
        || !in_range(fp.stack_version, c.stack_version)
        || !in_range(fp.hw_version, c.hw_version)
        || c.sw_build
            .as_ref()
            .is_some_and(|m| !fp.sw_build_id.as_deref().is_some_and(|v| m.matches(v)))
        || c.power.is_some_and(|p| fp.power() != p)
    {
        return None;
    }
    for e in &c.endpoints {
        let ep = fp.endpoint(e.ep)?;
        if e.profile.is_some_and(|p| p != ep.profile)
            || e.device_id.is_some_and(|d| d != ep.device_id)
            || !e.in_clusters.iter().all(|x| ep.in_clusters.contains(x))
            || !e.out_clusters.iter().all(|x| ep.out_clusters.contains(x))
        {
            return None;
        }
    }
    let model_exact = c.model.as_ref().is_some_and(StrMatch::is_exact);
    let man_exact = c.manufacturer.as_ref().is_some_and(StrMatch::is_exact);
    let man_prefix = c.manufacturer.as_ref().is_some_and(|m| !m.is_exact());
    let base = match (model_exact, man_exact, man_prefix) {
        (true, true, _) => 500,
        (true, false, true) => 400,
        (true, false, false) => 300,
        _ => 200,
    };
    let names = c.model.is_some() || c.manufacturer.is_some();
    let extras = [
        c.app_version.is_some(),
        c.stack_version.is_some(),
        c.hw_version.is_some(),
        c.sw_build.is_some(),
        c.power.is_some(),
        names && c.mfr_code.is_some(),
        names && !c.endpoints.is_empty(),
    ]
    .into_iter()
    .filter(|x| *x)
    .count() as u32;
    Some(base + (extras * 10).min(50))
}

/// Best score of a definition (OR over clauses).
pub fn score(d: &Definition, fp: &Fingerprint) -> Option<u32> {
    if let Some(z) = &d.zdef {
        return crate::zdef_adapter::zdef_score(z, fp);
    }
    d.matches.iter().filter_map(|c| score_clause(c, fp)).max()
}

/// The features of `d` this hub lacks.
pub fn missing_features(d: &Definition, features: &[&str]) -> Vec<String> {
    d.used_features()
        .into_iter()
        .filter(|f| !features.contains(&f.as_str()))
        .collect()
}

/// Picks the best definition among candidates (3.3 ties: priority, user
/// over catalog, fetched over bank, newer revision). Skipped definitions
/// are reported in `skipped`.
pub fn best_match(
    cands: impl IntoIterator<Item = Arc<Definition>>,
    fp: &Fingerprint,
    features: &[&str],
    skipped: &mut Vec<String>,
) -> Option<(Arc<Definition>, u32)> {
    let mut best: Option<(Arc<Definition>, u32)> = None;
    for d in cands {
        let Some(s) = score(&d, fp) else { continue };
        let missing = missing_features(&d, features);
        if !missing.is_empty() {
            skipped.push(format!(
                "definition_skipped: {} requires {}",
                d.id,
                missing.join(", ")
            ));
            continue;
        }
        let better = match &best {
            None => true,
            Some((b, bs)) => {
                (s, d.priority, d.source, d.revision) > (*bs, b.priority, b.source, b.revision)
            }
        };
        if better {
            best = Some((d, s));
        }
    }
    best
}

/// One resolved entity.
#[derive(Debug, Clone)]
pub struct ResolvedEntity {
    /// The definition with `ep` filled in.
    pub def: EntityDef,
    /// Endpoint.
    pub ep: u8,
    /// Layer that produced it.
    pub source: LayerSource,
    /// Endpoint display name (`endpoints:`).
    pub ep_name: Option<String>,
}

/// A referenced layer (resolution record, 3.4).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LayerRef {
    /// Source.
    pub source: LayerSource,
    /// Definition ID.
    pub id: String,
    /// Revision.
    pub revision: u32,
    /// Content hash.
    pub content_hash: Option<String>,
    /// Match score.
    pub score: Option<u32>,
}

/// The merged result for one device.
#[derive(Debug, Clone, Default)]
pub struct Resolution {
    /// Layers used, bottom to top.
    pub layers: Vec<LayerRef>,
    /// Entities in a stable order.
    pub entities: Vec<ResolvedEntity>,
    /// Effective device fields.
    pub device: DeviceFields,
    /// Effective power.
    pub power: Option<PowerKind>,
    /// Configure ops of the definitions, in layer order.
    pub configure: Vec<ConfigureOp>,
    /// Tuya flags.
    pub tuya: Option<TuyaFlags>,
    /// GP flags.
    pub gp: Option<GpFlags>,
    /// Ignore rules.
    pub ignore: Vec<IgnoreRule>,
    /// Display vendor.
    pub vendor: Option<String>,
    /// Display model.
    pub model: Option<String>,
    /// Diagnostics (`definition_skipped: …`).
    pub diagnostics: Vec<String>,
}

impl Resolution {
    /// Entity by `(ep, key)`.
    pub fn entity(&self, ep: u8, key: &str) -> Option<&ResolvedEntity> {
        self.entities
            .iter()
            .find(|e| e.ep == ep && e.def.key == key)
    }
}

/// Everything resolution needs.
pub struct ResolveInput<'a> {
    /// The fingerprint.
    pub fp: &'a Fingerprint,
    /// Generic layer producer given the effective power.
    pub generic: &'a dyn Fn(PowerKind) -> Vec<EntityDef>,
    /// Catalog (bank + fetched).
    pub catalog: &'a dyn Catalog,
    /// User definitions.
    pub user: &'a dyn Catalog,
    /// Self-description fragment.
    pub self_description: Option<&'a Definition>,
    /// Pinned definition ID (layer 5).
    pub pinned: Option<&'a str>,
    /// Hub features.
    pub features: &'a [&'a str],
}

/// The cluster an entity is bound to and whether it must be a server
/// cluster (attributes) or may be either (commands).
fn bound_cluster(e: &EntityDef) -> Option<(u16, bool)> {
    let of = |b: &Binding| match b {
        Binding::Zcl(z) => Some((z.cluster, true)),
        Binding::Struct { cluster, .. } => Some((*cluster, true)),
        Binding::Tuya { .. } => Some((cl::TUYA, true)),
        Binding::IasWarning => Some((cl::IAS_WD, true)),
        _ => None,
    };
    if let Some(c) = e.values().find_map(|(_, v)| of(&v.binding)) {
        return Some(c);
    }
    if let Some(p) = &e.press {
        return match p {
            Press::Cmd { cluster, .. } => Some((*cluster, false)),
            Press::Write { cluster, .. } => Some((*cluster, true)),
            Press::Tuya { .. } => Some((cl::TUYA, true)),
        };
    }
    e.triggers.iter().find_map(|t| match t.source {
        TriggerSource::Cmd { cluster, .. } => Some((cluster, false)),
        TriggerSource::Tuya { .. } => Some((cl::TUYA, false)),
        TriggerSource::GpCmd(_) => None,
    })
}

fn is_gp_entity(e: &EntityDef) -> bool {
    e.values().any(|(_, v)| matches!(v.binding, Binding::Gp(_)))
        || e.triggers
            .iter()
            .any(|t| matches!(t.source, TriggerSource::GpCmd(_)))
}

/// Default endpoint of a definition entity (6.3).
pub fn default_endpoint(e: &EntityDef, fp: &Fingerprint) -> u8 {
    if let Some(ep) = e.ep {
        return ep;
    }
    if fp.gp.is_some() || is_gp_entity(e) {
        return crate::gp::GP_ENDPOINT;
    }
    if let Some(ep) = e.press.as_ref().and_then(|p| match p {
        Press::Cmd { ep, .. } => *ep,
        _ => None,
    }) {
        return ep;
    }
    if let Some(ep) = e.triggers.iter().find_map(|t| match t.source {
        TriggerSource::Cmd { ep, .. } => ep,
        _ => None,
    }) {
        return ep;
    }
    let app: Vec<_> = fp
        .endpoints
        .iter()
        .filter(|x| x.profile != PROFILE_GP)
        .collect();
    if let Some((c, server)) = bound_cluster(e) {
        if let Some(x) = app
            .iter()
            .filter(|x| x.in_clusters.contains(&c) || (!server && x.out_clusters.contains(&c)))
            .map(|x| x.ep)
            .min()
        {
            return x;
        }
    }
    app.iter().map(|x| x.ep).min().unwrap_or(1)
}

fn apply_patch(e: &mut EntityDef, p: &EntityPatch) {
    let apply_value = |v: &mut ValueDef| {
        if let Some(r) = p.report {
            v.report = Some(r);
        }
        if p.mul.is_some() || p.div.is_some() {
            v.transforms.attr_scale = None;
        }
        if p.mul.is_some() {
            v.transforms.mul = p.mul;
        }
        if p.div.is_some() {
            v.transforms.div = p.div;
        }
        if p.offset.is_some() {
            v.transforms.offset = p.offset;
        }
        if p.precision.is_some() {
            v.transforms.precision = p.precision;
        }
        if p.invalid.is_some() {
            v.transforms.invalid = p.invalid;
        }
        if p.min.is_some() {
            v.limits.min = p.min;
        }
        if p.max.is_some() {
            v.limits.max = p.max;
        }
        if p.step.is_some() {
            v.limits.step = p.step;
        }
    };
    if e.parts.is_empty() {
        apply_value(&mut e.value);
    } else {
        e.parts.iter_mut().for_each(|(_, v)| apply_value(v));
    }
    if let Some(n) = &p.name {
        e.name = Some(n.clone());
    }
    if let Some(en) = p.enabled {
        e.enabled = en;
    }
    if p.category.is_some() {
        e.category = p.category;
    }
    if let Some(dc) = &p.device_class {
        e.device_class = Some(dc.clone());
    }
    if let Some(u) = &p.unit {
        e.unit = Some(u.clone());
    }
    if let Some(i) = p.invert {
        e.invert = i;
    }
}

/// Resolves one device.
pub fn resolve(input: &ResolveInput<'_>) -> Resolution {
    let fp = input.fp;
    let mut res = Resolution::default();
    let mut skipped = Vec::new();

    // Layers 2 and 4: catalog and user, or the pinned definition.
    let pinned = input
        .pinned
        .and_then(|id| input.user.get(id).or_else(|| input.catalog.get(id)));
    let (catalog, user) = match pinned {
        Some(p) if p.source == LayerSource::User => (None, Some((p, None))),
        Some(p) => (Some((p, None)), None),
        None => (
            best_match(
                input.catalog.candidates(fp),
                fp,
                input.features,
                &mut skipped,
            )
            .map(|(d, s)| (d, Some(s))),
            best_match(input.user.candidates(fp), fp, input.features, &mut skipped)
                .map(|(d, s)| (d, Some(s))),
        ),
    };
    // A user definition replaces the catalog definition (3.3).
    let chosen = user.or(catalog);
    let selfdesc_mode = chosen
        .as_ref()
        .map_or(SelfDescMode::Over, |(d, _)| d.self_description);
    let selfdesc = input
        .self_description
        .filter(|_| selfdesc_mode != SelfDescMode::Ignore);

    // Layer order, bottom to top (excluding generic).
    let mut layers: Vec<(&Definition, Option<u32>)> = Vec::new();
    let catalog_over_selfdesc = matches!(&chosen, Some((d, _)) if d.source == LayerSource::User)
        || selfdesc_mode == SelfDescMode::Under;
    if let Some(sd) = selfdesc.filter(|_| catalog_over_selfdesc) {
        layers.push((sd, None));
    }
    if let Some((d, s)) = &chosen {
        layers.push((d, *s));
    }
    if let Some(sd) = selfdesc.filter(|_| !catalog_over_selfdesc) {
        layers.push((sd, None));
    }

    // Device fields.
    let mut device = DeviceFields {
        power: Some(fp.power()),
        sleepy: Some(fp.sleepy()),
        ..Default::default()
    };
    for (d, _) in &layers {
        device.overlay(&d.device);
    }
    let power = device.power.unwrap_or(PowerKind::Mains);

    // Layer 1 with the knobs of the higher layers.
    res.layers.push(LayerRef {
        source: LayerSource::Generic,
        id: "generic".into(),
        revision: 1,
        content_hash: None,
        score: None,
    });
    let disable = layers.iter().any(|(d, _)| d.generic.disable);
    let mut merged: BTreeMap<(u8, String), ResolvedEntity> = BTreeMap::new();
    let mut order: Vec<(u8, String)> = Vec::new();
    let put = |merged: &mut BTreeMap<(u8, String), ResolvedEntity>,
               order: &mut Vec<(u8, String)>,
               r: ResolvedEntity| {
        let k = (r.ep, r.def.key.clone());
        if !merged.contains_key(&k) {
            order.push(k.clone());
        }
        merged.insert(k, r);
    };
    for mut e in (input.generic)(power) {
        let device_level = matches!(e.value.binding, Binding::DeviceField(_));
        let ep = e.ep.unwrap_or(0);
        if disable && !device_level {
            continue;
        }
        if layers
            .iter()
            .any(|(d, _)| d.generic.exclude.contains(&e.key))
        {
            continue;
        }
        if !device_level {
            if let Some(only) = layers
                .iter()
                .rev()
                .find_map(|(d, _)| d.generic.only_endpoints.as_ref())
            {
                if !only.contains(&ep) {
                    continue;
                }
            }
        }
        for (d, _) in &layers {
            for (k, p) in &d.generic.patch {
                if *k == e.key {
                    apply_patch(&mut e, p);
                }
            }
        }
        e.ep = Some(ep);
        put(
            &mut merged,
            &mut order,
            ResolvedEntity {
                def: e,
                ep,
                source: LayerSource::Generic,
                ep_name: None,
            },
        );
    }

    // Definition layers: replace by key, add new keys.
    let env = FpEnv(fp);
    for (d, s) in &layers {
        res.layers.push(LayerRef {
            source: d.source,
            id: d.id.clone(),
            revision: d.revision,
            content_hash: d.content_hash.clone(),
            score: *s,
        });
        for e in &d.entities {
            if let Some(p) = &e.present_if {
                let v = p.eval(&ExprCtx::with_x(Value::Null, &env));
                if v.truthy() != Some(true) {
                    continue;
                }
            }
            let ep = default_endpoint(e, fp);
            let mut def = e.clone();
            def.ep = Some(ep);
            put(
                &mut merged,
                &mut order,
                ResolvedEntity {
                    def,
                    ep,
                    source: d.source,
                    ep_name: None,
                },
            );
        }
        res.configure.extend(d.configure.iter().cloned());
        res.ignore.extend(d.ignore.iter().copied());
        if d.tuya.is_some() {
            res.tuya.clone_from(&d.tuya);
        }
        if d.gp.is_some() {
            res.gp.clone_from(&d.gp);
        }
        if !d.vendor.is_empty() {
            res.vendor = Some(d.vendor.clone());
            res.model = Some(d.model.clone());
            if let Some(wl) = fp.manufacturer_name.as_deref().and_then(|m| {
                d.white_label
                    .iter()
                    .find(|w| w.manufacturer.iter().any(|x| x == m))
            }) {
                res.vendor = Some(wl.vendor.clone());
                res.model = Some(wl.model.clone());
            }
        }
    }
    let ep_names: BTreeMap<u8, String> = layers
        .iter()
        .flat_map(|(d, _)| d.endpoints.clone())
        .collect();
    res.entities = order
        .into_iter()
        .filter_map(|k| merged.remove(&k))
        .map(|mut r| {
            r.ep_name = ep_names.get(&r.ep).cloned();
            r
        })
        .collect();
    res.device = device;
    res.power = Some(power);
    res.diagnostics = skipped;
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::EndpointFp;

    fn fp(man: &str, model: &str) -> Fingerprint {
        Fingerprint {
            manufacturer_name: Some(man.into()),
            model_id: Some(model.into()),
            endpoints: vec![EndpointFp {
                ep: 1,
                profile: 0x0104,
                device_id: 0x51,
                in_clusters: vec![0, 6],
                out_clusters: vec![],
            }],
            ..Default::default()
        }
    }

    fn def(id: &str, clause: MatchClause, src: LayerSource) -> Definition {
        let mut d = Definition::new(id, src);
        d.vendor = "V".into();
        d.model = id.into();
        d.matches.push(clause);
        d
    }

    #[test]
    fn scoring_and_ties() {
        let f = fp("_TZE200_abc", "TS0601");
        let exact = MatchClause {
            model: Some(StrMatch::Exact("TS0601".into())),
            manufacturer: Some(StrMatch::Any(vec!["_TZE200_abc".into()])),
            ..Default::default()
        };
        let prefix = MatchClause {
            model: Some(StrMatch::Exact("TS0601".into())),
            manufacturer: Some(StrMatch::Prefix("_TZE2".into())),
            ..Default::default()
        };
        let model_only = MatchClause {
            model: Some(StrMatch::Exact("TS0601".into())),
            ..Default::default()
        };
        assert_eq!(score_clause(&exact, &f), Some(500));
        assert_eq!(score_clause(&prefix, &f), Some(400));
        assert_eq!(score_clause(&model_only, &f), Some(300));
        let mut versioned = model_only.clone();
        versioned.app_version = Some((0, 10));
        assert_eq!(score_clause(&versioned, &f), None);
        let mut f2 = f.clone();
        f2.app_version = Some(5);
        assert_eq!(score_clause(&versioned, &f2), Some(310));
        let mut skipped = Vec::new();
        let a = Arc::new(def("a", model_only.clone(), LayerSource::Catalog));
        let mut bd = def("b", model_only.clone(), LayerSource::Catalog);
        bd.priority = 1;
        let b = Arc::new(bd);
        let mut wd = def("w", model_only, LayerSource::Catalog);
        wd.requires = vec!["wasm".into()];
        let (best, _) = best_match([a, b, Arc::new(wd)], &f, HUB_FEATURES, &mut skipped).unwrap();
        assert_eq!(best.id, "b");
        assert_eq!(skipped, ["definition_skipped: w requires wasm"]);
    }

    #[test]
    fn merge_by_key_with_knobs() {
        let f = fp("Innr", "SP 120");
        let generic = |_p: PowerKind| {
            let mut s = EntityDef::new("switch", Platform::Switch);
            s.ep = Some(1);
            let mut pob = EntityDef::new("power_on_behavior", Platform::Select);
            pob.ep = Some(1);
            let mut cur = EntityDef::new("current", Platform::Sensor);
            cur.ep = Some(1);
            cur.value.transforms.attr_scale = Some(AttrScale {
                cluster: 0x0B04,
                mul_attr: Some(1),
                div_attr: Some(2),
                factor: 1.0,
            });
            vec![s, pob, cur]
        };
        let mut d = def(
            "innr",
            MatchClause {
                model: Some(StrMatch::Exact("SP 120".into())),
                ..Default::default()
            },
            LayerSource::Catalog,
        );
        d.generic.exclude = vec!["power_on_behavior".into()];
        d.generic.patch = vec![(
            "current".into(),
            EntityPatch {
                div: Some(1000.0),
                ..Default::default()
            },
        )];
        let mut extra = EntityDef::new("switch", Platform::Switch);
        extra.name = Some("replaced".into());
        d.entities.push(extra);
        let mut cat = MemoryCatalog::default();
        cat.insert(d);
        let user = MemoryCatalog::default();
        let r = resolve(&ResolveInput {
            fp: &f,
            generic: &generic,
            catalog: &cat,
            user: &user,
            self_description: None,
            pinned: None,
            features: HUB_FEATURES,
        });
        let keys: Vec<_> = r
            .entities
            .iter()
            .map(|e| (e.ep, e.def.key.as_str(), e.source))
            .collect();
        assert_eq!(
            keys,
            [
                (1, "switch", LayerSource::Catalog),
                (1, "current", LayerSource::Generic)
            ]
        );
        assert_eq!(r.entities[0].def.name.as_deref(), Some("replaced"));
        let cur = &r.entities[1].def.value.transforms;
        assert_eq!(cur.div, Some(1000.0));
        assert!(cur.attr_scale.is_none());
        assert_eq!(r.vendor.as_deref(), Some("V"));
        assert_eq!(r.layers.len(), 2);
    }
}
