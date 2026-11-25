//! Layer resolution and merging (spec 3.3, 9.5, 12.5).
//!
//! Layers from lowest to highest precedence: generic (tier a), catalog
//! (bank or fetched), self-description, user definition, device overrides.
//! A user definition replaces the catalog definition; a catalog/user
//! definition can demote the self-description (`self_description: under`)
//! or drop it (`ignore`). Entities merge by `(endpoint, key)`: a higher
//! layer replaces the whole entity; `generic.patch` patches generic
//! entities field by field.

use crate::expr::{Host, Inputs, Value, Vm};
use crate::matching::{Fingerprint, GpFingerprint, GpdId, Identity, Source};
use crate::model::*;
use alloc::string::String;
use alloc::vec::Vec;

/// Endpoint of all GP entities.
pub const GP_ENDPOINT: u8 = 242;

/// Per-device user settings (layer 5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceOverrides {
    /// `(endpoint, key)` of entities the user disabled.
    pub disabled: Vec<(u8, String)>,
    /// `(endpoint, key, name)` user names.
    pub names: Vec<(u8, String, String)>,
}

/// Inputs of one resolution.
pub struct Layers<'a> {
    /// Tier-a entities with `key`, `platform` and `ep` set.
    pub generic: &'a [Entity],
    /// Constant pool of the generic entities' expressions (usually empty).
    pub generic_consts: &'a [Const],
    /// Best catalog definition and whether it came from the bank or a fetch.
    pub catalog: Option<(Source, &'a Definition)>,
    /// Self-description fragment published by the device.
    pub self_description: Option<&'a Definition>,
    /// Best user definition.
    pub user: Option<&'a Definition>,
    pub overrides: Option<&'a DeviceOverrides>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedEntity<'a> {
    pub ep: u8,
    pub entity: Entity,
    /// Layer that supplied the entity (generic for patched generic entities).
    pub source: Source,
    /// Constant pool its expressions refer to.
    pub consts: &'a [Const],
    /// A `generic.patch` was applied.
    pub patched: bool,
}

impl ResolvedEntity<'_> {
    pub fn key(&self) -> &str {
        self.entity.key.as_deref().unwrap_or("")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Diagnostic {
    /// A patch carried expressions from another constant pool; the generic
    /// entity's own expressions were dropped.
    PatchDroppedExpressions(String),
    /// `present_if` failed to evaluate (treated as false).
    PresentIfError(String),
    /// A patch names a generic key that does not exist.
    PatchUnknownKey(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct LayerRef {
    pub source: Source,
    pub id: Option<String>,
    pub revision: Option<u32>,
}

/// The merged result for one device.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolution<'a> {
    pub entities: Vec<ResolvedEntity<'a>>,
    pub device: Device,
    pub vendor: Option<&'a str>,
    pub model: Option<&'a str>,
    pub endpoint_names: Vec<(u8, &'a str)>,
    /// Definition configure ops in layer order (after the generic configure).
    pub configure: Vec<(Source, &'a ConfigureOp)>,
    pub ignore: Vec<IgnoreRule>,
    pub tuya: Option<&'a Tuya>,
    pub gp: Option<&'a GpDevice>,
    /// Layers that contributed, lowest first.
    pub layers: Vec<LayerRef>,
    pub diagnostics: Vec<Diagnostic>,
}

/// How to find default endpoints and `present_if` values.
#[derive(Clone, Copy)]
pub enum Target<'a> {
    Zigbee(&'a Fingerprint),
    Gp(&'a GpFingerprint),
}

impl<'a> From<Identity<'a>> for Target<'a> {
    fn from(i: Identity<'a>) -> Self {
        match i {
            Identity::Zigbee(f) => Target::Zigbee(f),
            Identity::Gp(g) => Target::Gp(g),
        }
    }
}

struct IdHost<'a>(Target<'a>);

impl<'a> Host<'a> for IdHost<'a> {
    fn gpd(&self, f: u8) -> Value<'a> {
        let Target::Gp(g) = self.0 else {
            return Value::Null;
        };
        match f {
            0 => Value::Int(g.device_id as i64),
            1 => g
                .manufacturer_id
                .map(|v| Value::Int(v as i64))
                .unwrap_or(Value::Null),
            2 => g
                .model_id
                .map(|v| Value::Int(v as i64))
                .unwrap_or(Value::Null),
            3 => match g.gpd_id {
                GpdId::SrcId(s) => Value::Int(s as i64),
                GpdId::Ieee { .. } => Value::Null,
            },
            _ => Value::Null,
        }
    }

    fn fp(&self, f: u8) -> Value<'a> {
        let Target::Zigbee(fp) = self.0 else {
            return Value::Null;
        };
        let int = |v: Option<u8>| v.map(|v| Value::Int(v as i64)).unwrap_or(Value::Null);
        match f {
            0 => int(fp.app_version),
            1 => int(fp.hw_version),
            2 => fp
                .mfr_code
                .map(|v| Value::Int(v as i64))
                .unwrap_or(Value::Null),
            3 => fp
                .sw_build_id
                .as_deref()
                .map(Value::Str)
                .unwrap_or(Value::Null),
            4 => int(fp.stack_version),
            5 => int(fp.zcl_version),
            6 => fp
                .date_code
                .as_deref()
                .map(Value::Str)
                .unwrap_or(Value::Null),
            7 | 8 => {
                let n = if f == 7 {
                    &fp.manufacturer_name
                } else {
                    &fp.model_id
                };
                n.as_deref()
                    .map(|n| Value::Str(crate::matching::trim_name(n)))
                    .unwrap_or(Value::Null)
            }
            _ => Value::Null,
        }
    }
}

/// Default endpoint of an entity (spec 6.3: the lowest endpoint whose Simple
/// descriptor has the bound cluster; GP: 242).
pub fn default_ep(e: &Entity, t: Target<'_>) -> u8 {
    let fp = match t {
        Target::Gp(_) => return GP_ENDPOINT,
        Target::Zigbee(fp) => fp,
    };
    if let Some(ep) = e.ep {
        return ep;
    }
    let lowest = fp.endpoints.iter().map(|e| e.ep).min().unwrap_or(1);
    let by = |cluster: u16, either: bool| fp.endpoint_with(cluster, either).unwrap_or(lowest);
    if let Some(z) = &e.zcl {
        return z.ep.unwrap_or_else(|| by(z.cluster, false));
    }
    let m = e.more();
    if let Some(s) = &m.struct_ {
        return s.ep.unwrap_or_else(|| by(s.cluster, false));
    }
    if e.tuya.is_some() {
        return by(0xEF00, false);
    }
    if let Some(p) = &m.press {
        return match p {
            Press::Cmd { cmd, .. } => cmd.ep.unwrap_or_else(|| by(cmd.cluster, true)),
            Press::Write(w) => w.ep.unwrap_or_else(|| by(w.cluster, false)),
            Press::Tuya { .. } => by(0xEF00, false),
        };
    }
    if let Some(t) = m.triggers.first() {
        if let Some(c) = &t.cmd {
            return c.ep.unwrap_or_else(|| by(c.cluster, true));
        }
        if t.tuya.is_some() {
            return by(0xEF00, false);
        }
    }
    if let Some((_, p)) = m.parts.first() {
        return default_ep(p, Target::Zigbee(fp));
    }
    lowest
}

fn has_exprs(e: &Entity) -> bool {
    let mut any = false;
    e.for_each_expr(&mut |_, _| any = true);
    any
}

fn clear_exprs(e: &mut Entity) {
    e.from = None;
    if e.more.is_some() {
        let m = e.more_mut();
        m.to = None;
        m.reset_after = None;
        m.present_if = None;
        m.attrs.clear();
        m.triggers.clear();
        if let Some(Press::Cmd { payload, .. }) = &mut m.press {
            *payload = None;
        }
        for (_, p) in &mut m.parts {
            clear_exprs(p);
        }
        e.compact();
    }
}

/// Applies a field-level patch (only fields present in `patch`).
pub fn apply_patch(base: &mut Entity, patch: &Entity) {
    macro_rules! opt {
        ($($f:ident),*) => { $( if patch.$f.is_some() { base.$f = patch.$f.clone(); } )* };
    }
    opt!(
        key,
        platform,
        device_class,
        unit,
        state_class,
        category,
        ep,
        zcl,
        tuya,
        gp,
        from,
        mul,
        div,
        offset,
        precision,
        invalid,
        min,
        max,
        step,
        access,
        report,
        enabled
    );
    if !patch.map.is_empty() {
        base.map = patch.map.clone();
    }
    let Some(pm) = patch.more.as_deref() else {
        return;
    };
    let bm = base.more_mut();
    macro_rules! more_opt {
        ($($f:ident),*) => { $( if pm.$f.is_some() { bm.$f = pm.$f.clone(); } )* };
    }
    macro_rules! more_list {
        ($($f:ident),*) => { $( if !pm.$f.is_empty() { bm.$f = pm.$f.clone(); } )* };
    }
    more_opt!(
        name,
        struct_,
        to,
        value_on,
        value_off,
        mode,
        reset_after,
        press,
        codec,
        present_if,
        hub,
        invert
    );
    more_list!(parts, event_types, triggers, attrs);
    base.compact();
}

fn merge_device(into: &mut Device, d: &Device) {
    macro_rules! opt {
        ($($f:ident),*) => { $( if d.$f.is_some() { into.$f = d.$f.clone(); } )* };
    }
    opt!(power, sleepy, battery, checkin_s, availability, ota);
}

/// Resolves the layers for one device.
pub fn resolve<'a>(layers: &Layers<'a>, target: Target<'a>) -> Resolution<'a> {
    // Definition layers, lowest first.
    let top_def = layers.user.or(layers.catalog.map(|c| c.1));
    let policy = top_def
        .and_then(|d| d.self_description)
        .unwrap_or(SelfDescription::Over);
    let mut defs: Vec<(Source, &'a Definition)> = Vec::new();
    let catalog = if layers.user.is_some() {
        None
    } else {
        layers.catalog
    };
    let sd = if policy == SelfDescription::Ignore {
        None
    } else {
        layers.self_description
    };
    if policy == SelfDescription::Under {
        if let Some(s) = sd {
            defs.push((Source::SelfDescription, s));
        }
        if let Some(c) = catalog {
            defs.push(c);
        }
    } else {
        if let Some(c) = catalog {
            defs.push(c);
        }
        if let Some(s) = sd {
            defs.push((Source::SelfDescription, s));
        }
    }
    if let Some(u) = layers.user {
        defs.push((Source::User, u));
    }

    let mut res = Resolution {
        entities: Vec::new(),
        device: Device::default(),
        vendor: None,
        model: None,
        endpoint_names: Vec::new(),
        configure: Vec::new(),
        ignore: Vec::new(),
        tuya: None,
        gp: None,
        layers: Vec::new(),
        diagnostics: Vec::new(),
    };
    res.layers.push(LayerRef {
        source: Source::Generic,
        id: None,
        revision: None,
    });

    // Generic knobs, lower layers first.
    let mut exclude: Vec<&str> = Vec::new();
    let mut disable = false;
    let mut only: Option<&[u8]> = None;
    let mut patches: Vec<(&str, Entity, Source, &'a [Const])> = Vec::new();
    for (src, d) in &defs {
        if let Some(g) = &d.generic {
            exclude.extend(g.exclude.iter().map(|s| s.as_str()));
            if let Some(v) = g.disable {
                disable = v;
            }
            if !g.only_endpoints.is_empty() {
                only = Some(&g.only_endpoints);
            }
            for (k, p) in &g.patch {
                if let Some(slot) = patches.iter_mut().find(|x| x.0 == k.as_str()) {
                    apply_patch(&mut slot.1, p);
                    if has_exprs(p) {
                        slot.2 = *src;
                        slot.3 = &d.consts;
                    }
                } else {
                    patches.push((k.as_str(), p.clone(), *src, &d.consts));
                }
            }
        }
    }

    // Layer 1.
    if !disable {
        for g in layers.generic {
            let key = g.key.as_deref().unwrap_or("");
            let ep = g.ep.unwrap_or_else(|| default_ep(g, target));
            if exclude.contains(&key) || only.is_some_and(|o| !o.contains(&ep)) {
                continue;
            }
            let mut ent = g.clone();
            let mut consts = layers.generic_consts;
            let mut patched = false;
            if let Some((_, p, _, pc)) = patches.iter().find(|x| x.0 == key) {
                if has_exprs(p) && has_exprs(&ent) && !core::ptr::eq(*pc, consts) {
                    clear_exprs(&mut ent);
                    res.diagnostics
                        .push(Diagnostic::PatchDroppedExpressions(key.into()));
                }
                apply_patch(&mut ent, p);
                if has_exprs(p) {
                    consts = pc;
                }
                patched = true;
            }
            ent.ep = Some(ep);
            res.entities.push(ResolvedEntity {
                ep,
                entity: ent,
                source: Source::Generic,
                consts,
                patched,
            });
        }
        for (k, ..) in &patches {
            if !layers.generic.iter().any(|g| g.key.as_deref() == Some(*k)) {
                res.diagnostics
                    .push(Diagnostic::PatchUnknownKey((*k).into()));
            }
        }
    }

    // Layers 2..4.
    let mut vm = Vm::new();
    let host = IdHost(target);
    for (src, d) in &defs {
        res.layers.push(LayerRef {
            source: *src,
            id: d.id.clone(),
            revision: d.revision,
        });
        if let Some(dev) = &d.device {
            merge_device(&mut res.device, dev);
        }
        if d.vendor.is_some() {
            res.vendor = d.vendor.as_deref();
        }
        if d.model.is_some() {
            res.model = d.model.as_deref();
        }
        for (ep, n) in &d.endpoints {
            res.endpoint_names.retain(|x| x.0 != *ep);
            res.endpoint_names.push((*ep, n.as_str()));
        }
        res.configure
            .extend(d.configure.iter().map(|op| (*src, op)));
        res.ignore.extend(d.ignore.iter().copied());
        if d.tuya.is_some() {
            res.tuya = d.tuya.as_ref();
        }
        if d.gp.is_some() {
            res.gp = d.gp.as_ref();
        }
        for e in &d.entities {
            if let Some(p) = &e.more().present_if {
                match vm.eval(p.code(), &d.consts, &Inputs::NONE, &host) {
                    Ok(Value::Bool(true)) => {}
                    Ok(_) => continue,
                    Err(_) => {
                        res.diagnostics.push(Diagnostic::PresentIfError(
                            e.key.clone().unwrap_or_default(),
                        ));
                        continue;
                    }
                }
            }
            let ep = default_ep(e, target);
            let key = e.key.as_deref().unwrap_or("");
            let mut ent = e.clone();
            ent.ep = Some(ep);
            let r = ResolvedEntity {
                ep,
                entity: ent,
                source: *src,
                consts: &d.consts,
                patched: false,
            };
            if let Some(slot) = res
                .entities
                .iter_mut()
                .find(|x| x.ep == ep && x.key() == key)
            {
                *slot = r;
            } else {
                res.entities.push(r);
            }
        }
    }
    if let (Target::Zigbee(fp), Some(d)) = (target, top_def) {
        let (v, m) = d.display_identity(fp);
        res.vendor = v.or(res.vendor);
        res.model = m.or(res.model);
    }

    // Layer 5.
    if let Some(o) = layers.overrides {
        for e in &mut res.entities {
            if o.disabled.iter().any(|(ep, k)| *ep == e.ep && k == e.key()) {
                e.entity.enabled = Some(false);
            }
            if let Some((_, _, n)) = o
                .names
                .iter()
                .find(|(ep, k, _)| *ep == e.ep && k == e.key())
            {
                e.entity.more_mut().name = Some(n.clone());
            }
        }
    }
    res
}
