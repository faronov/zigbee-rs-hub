//! Semantic validation (spec 6, 7.5, 8.5, 12.1) and `requires` computation.

use crate::enums::{Feature, Platform, zcl_type_name};
use crate::expr::{self, Context, Site, Ty};
use crate::model::*;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    /// Field path, e.g. `entities[3].from`.
    pub path: String,
    pub message: String,
}

/// Static type of a ZCL type's decoded value.
pub fn zcl_value_ty(code: u8) -> Ty {
    match code {
        0x10 => Ty::BOOL,
        0x38..=0x3A => Ty::FLOAT,
        0x41 | 0x43 => Ty::BYTES,
        0x42 | 0x44 => Ty::STR,
        _ => Ty::INT,
    }
}

fn tuya_ty(t: TuyaType) -> Ty {
    match t {
        TuyaType::Bool => Ty::BOOL,
        TuyaType::Raw | TuyaType::String => Ty::BYTES,
        _ => Ty::INT,
    }
}

/// Type of `x` in `from` for an entity (after codec, map, value_on/off).
pub fn from_x_ty(e: &Entity) -> Ty {
    if !e.map.is_empty() {
        return Ty::STR;
    }
    if e.value_on.is_some() || e.value_off.is_some() {
        return Ty::BOOL;
    }
    if let Some(c) = &e.codec {
        return match c {
            Codec::BcdTime => Ty::INT,
            _ => Ty::STR,
        };
    }
    if let Some(z) = &e.zcl {
        return zcl_value_ty(z.zcl_type);
    }
    if let Some(t) = &e.tuya {
        return tuya_ty(t.ty);
    }
    match &e.gp {
        Some(GpBinding::Report { zcl_type, .. }) => return zcl_value_ty(*zcl_type),
        Some(GpBinding::Cmd(_)) => return Ty::BYTES,
        None => {}
    }
    Ty::ANY
}

/// Expression context for a site of an entity.
pub fn context_for(e: &Entity, site: Site, gp_def: bool) -> Context {
    let mut c = Context::new(site).with_gpd(gp_def);
    match site {
        Site::From => {
            c = c.with_x(from_x_ty(e));
            let raw = matches!(e.gp, Some(GpBinding::Cmd(_)))
                || e.tuya.is_some_and(|t| t.ty == TuyaType::Raw);
            c = c.with_payload(raw);
        }
        Site::To => {
            c = c.with_x(match e.platform {
                Some(Platform::Number) => Ty::NUM,
                Some(Platform::Switch) => Ty::BOOL,
                Some(Platform::Select | Platform::Text) => Ty::STR,
                _ => Ty::ANY,
            })
        }
        _ => {}
    }
    c
}

/// True if the definition is a GP definition (GP match or GP bindings).
pub fn is_gp(d: &Definition) -> bool {
    d.match_.iter().any(|c| c.gp.is_some())
        || d.gp.is_some()
        || d.entities
            .iter()
            .any(|e| e.gp.is_some() || e.triggers.iter().any(|t| t.gp_cmd.is_some()))
}

/// Features the definition uses (spec 8.5), ascending.
pub fn compute_requires(d: &Definition) -> Vec<u16> {
    let mut f: Vec<u16> = Vec::new();
    let mut add = |x: Feature| {
        let c = x.code() as u16;
        if !f.contains(&c) {
            f.push(c);
        }
    };
    let mut any_expr = false;
    let mut visit = |e: &Entity, add: &mut dyn FnMut(Feature)| {
        e.for_each_expr(&mut |_, _| any_expr = true);
        fn walk(e: &Entity, add: &mut dyn FnMut(Feature)) {
            if e.tuya.is_some()
                || e.triggers.iter().any(|t| t.tuya.is_some())
                || matches!(e.press, Some(Press::Tuya { .. }))
            {
                add(Feature::Tuya);
            }
            if e.gp.is_some() || e.triggers.iter().any(|t| t.gp_cmd.is_some()) {
                add(Feature::Gp);
            }
            if let Some(s) = &e.struct_ {
                if s.codec == "zcl_tagged" {
                    add(Feature::StructZclTagged);
                }
            }
            if let Some(c) = &e.codec {
                add(c.feature());
            }
            for (_, p) in &e.parts {
                walk(p, add);
            }
        }
        walk(e, add);
    };
    for e in &d.entities {
        visit(e, &mut add);
    }
    if let Some(g) = &d.generic {
        for (_, e) in &g.patch {
            visit(e, &mut add);
        }
    }
    if any_expr {
        add(Feature::ExprV1);
    }
    if d.tuya.is_some() {
        add(Feature::Tuya);
    }
    if d.gp.is_some() || d.match_.iter().any(|c| c.gp.is_some()) {
        add(Feature::Gp);
    }
    if d.x_wasm.is_some() {
        add(Feature::Wasm);
    }
    f.sort_unstable();
    f
}

fn valid_id(s: &str) -> bool {
    (3..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._/-".contains(&b))
}

fn valid_key(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

struct V<'d> {
    d: &'d Definition,
    gp: bool,
    errs: Vec<ValidationError>,
}

impl V<'_> {
    fn err(&mut self, path: &str, msg: impl Into<String>) {
        self.errs.push(ValidationError {
            path: path.to_string(),
            message: msg.into(),
        });
    }

    fn expr(&mut self, path: &str, x: &Expr, ctx: Context, want: Ty) {
        match expr::verify(x.code(), &self.d.consts, &ctx) {
            Ok(t) => {
                if !t.compatible(want) {
                    self.err(
                        path,
                        format!(
                            "expression has the wrong result type for {}",
                            ctx.site.name()
                        ),
                    );
                }
            }
            Err(e) => self.err(
                path,
                format!("bytecode rejected at offset {}: {:?}", e.offset, e.kind),
            ),
        }
    }

    fn entity(&mut self, path: &str, e: &Entity, full: bool, fragment: bool) {
        if full {
            match &e.key {
                Some(k) if valid_key(k) => {}
                Some(k) => self.err(path, format!("entity key \"{k}\" is not snake_case")),
                None => self.err(path, "entity without key"),
            }
            if e.platform.is_none() {
                self.err(path, "entity without platform");
            }
        }
        let bindings = e.zcl.is_some() as u8
            + e.tuya.is_some() as u8
            + e.struct_.is_some() as u8
            + e.gp.is_some() as u8
            + e.hub.is_some() as u8
            + (!e.parts.is_empty()) as u8
            + (!e.triggers.is_empty()) as u8
            + e.press.is_some() as u8;
        if bindings > 1 {
            self.err(
                path,
                "more than one binding (zcl, tuya, struct, gp, hub, parts, triggers, press)",
            );
        }
        if full && bindings == 0 {
            self.err(path, "entity without binding");
        }
        if let Some(p) = e.platform {
            if p == Platform::Event && (e.triggers.is_empty() || e.event_types.is_empty()) {
                self.err(path, "event entity needs triggers and event_types");
            }
            if p == Platform::Button && e.press.is_none() && full {
                self.err(path, "button entity needs press");
            }
            if !e.parts.is_empty() && !p.is_composite() {
                self.err(path, "parts on a non-composite platform");
            }
        }
        if fragment && (e.tuya.is_some() || e.struct_.is_some() || e.gp.is_some()) {
            self.err(path, "self-description fragments allow only zcl bindings");
        }
        if let Some(z) = &e.zcl {
            if zcl_type_name(z.zcl_type).is_none() {
                self.err(path, "unknown ZCL type");
            }
        }
        if let Some(s) = &e.struct_ {
            if !STRUCT_CODECS.contains(&s.codec.as_str()) {
                self.err(path, format!("unknown struct codec \"{}\"", s.codec));
            }
        }
        for (i, (a, _)) in e.map.iter().enumerate() {
            if e.map[..i].iter().any(|(b, _)| a == b) {
                self.err(path, format!("map: duplicate raw value {a}"));
            }
        }
        for (i, t) in e.event_types.iter().enumerate() {
            if e.event_types[..i].contains(t) {
                self.err(path, format!("event_types: duplicate \"{t}\""));
            }
        }
        let gp = self.gp;
        let p = |s: &str| format!("{path}.{s}");
        if let Some(x) = &e.from {
            self.expr(&p("from"), x, context_for(e, Site::From, gp), Ty::ANY);
        }
        if let Some(x) = &e.to {
            self.expr(&p("to"), x, context_for(e, Site::To, gp), Ty::ANY);
        }
        if let Some(x) = &e.reset_after {
            self.expr(
                &p("reset_after"),
                x,
                context_for(e, Site::ResetAfter, gp),
                Ty::NUM,
            );
        }
        if let Some(x) = &e.present_if {
            self.expr(
                &p("present_if"),
                x,
                context_for(e, Site::PresentIf, gp),
                Ty::BOOL,
            );
        }
        for (n, x) in &e.attrs {
            self.expr(
                &p(&format!("attrs.{n}")),
                x,
                context_for(e, Site::Attr, gp),
                Ty::ANY,
            );
        }
        for (i, t) in e.triggers.iter().enumerate() {
            let tp = format!("{path}.triggers[{i}]");
            let kinds = t.cmd.is_some() as u8 + t.gp_cmd.is_some() as u8 + t.tuya.is_some() as u8;
            if kinds != 1 {
                self.err(&tp, "a trigger needs exactly one of cmd, gp_cmd, tuya");
            }
            if fragment && t.cmd.is_none() {
                self.err(&tp, "self-description fragments allow only cmd triggers");
            }
            if let Some(x) = &t.when {
                self.expr(
                    &format!("{tp}.when"),
                    x,
                    context_for(e, Site::When, gp),
                    Ty::BOOL,
                );
            }
            self.expr(
                &format!("{tp}.event"),
                &t.event,
                context_for(e, Site::Event, gp),
                Ty::STR,
            );
            for (n, x) in &t.attrs {
                self.expr(
                    &format!("{tp}.attrs.{n}"),
                    x,
                    context_for(e, Site::Attr, gp),
                    Ty::ANY,
                );
            }
        }
        if let Some(pr) = &e.press {
            match pr {
                Press::Cmd {
                    payload: Some(x), ..
                } => self.expr(
                    &p("press.payload"),
                    x,
                    context_for(e, Site::PressPayload, gp),
                    Ty::BYTES,
                ),
                Press::Tuya { .. } if fragment => self.err(
                    &p("press"),
                    "self-description fragments allow only cmd/write presses",
                ),
                _ => {}
            }
        }
        for (k, part) in &e.parts {
            let name = crate::keys::sub_name(*k as u64).unwrap_or("?");
            self.entity(&format!("{path}.parts.{name}"), part, false, fragment);
        }
    }
}

/// Validates a decoded definition: required fields, bindings, fragment
/// restrictions, every expression through the bytecode verifier, and that
/// `requires` lists every used feature.
pub fn validate(d: &Definition) -> Result<(), Vec<ValidationError>> {
    let mut v = V {
        d,
        gp: is_gp(d),
        errs: Vec::new(),
    };
    let fragment = d.is_fragment();
    if fragment {
        if d.id.is_some() || !d.match_.is_empty() || !d.white_label.is_empty() {
            v.err("", "a fragment has no id, match or white_label");
        }
        if d.tuya.is_some() || d.gp.is_some() {
            v.err("", "a fragment has no tuya or gp block");
        }
        for (i, op) in d.configure.iter().enumerate() {
            if !matches!(
                op,
                ConfigureOp::Report { .. } | ConfigureOp::Read { .. } | ConfigureOp::Checkin { .. }
            ) {
                v.err(
                    &format!("configure[{i}]"),
                    "fragments allow only report, read and checkin ops",
                );
            }
        }
    } else {
        match &d.id {
            Some(id) if valid_id(id) => {}
            Some(id) => v.err("id", format!("invalid id \"{id}\"")),
            None => v.err("id", "missing"),
        }
        if d.revision.is_none() {
            v.err("revision", "missing");
        }
        if d.vendor.is_none() {
            v.err("vendor", "missing");
        }
        if d.model.is_none() {
            v.err("model", "missing");
        }
        if d.match_.is_empty() {
            v.err("match", "at least one clause is required");
        }
        for (i, c) in d.match_.iter().enumerate() {
            let zig = c.manufacturer.is_some()
                || c.model.is_some()
                || c.mfr_code.is_some()
                || !c.endpoints.is_empty()
                || c.sw_build.is_some()
                || c.app_version.is_some()
                || c.hw_version.is_some()
                || c.stack_version.is_some()
                || c.power.is_some();
            if c.gp.is_some() && zig {
                v.err(
                    &format!("match[{i}]"),
                    "a GP clause cannot have Zigbee fields",
                );
            }
            if c.gp.is_none()
                && c.manufacturer.is_none()
                && c.model.is_none()
                && c.endpoints.is_empty()
                && c.mfr_code.is_none()
            {
                v.err(&format!("match[{i}]"), "empty clause");
            }
        }
    }
    let mut seen: Vec<(Option<u8>, &str)> = Vec::new();
    for (i, e) in d.entities.iter().enumerate() {
        let path = format!("entities[{i}]");
        v.entity(&path, e, true, fragment);
        if let Some(k) = e.key.as_deref() {
            if seen.contains(&(e.ep, k)) {
                v.err(&path, format!("duplicate entity key \"{k}\""));
            }
            seen.push((e.ep, k));
        }
    }
    if let Some(g) = &d.generic {
        for (k, e) in &g.patch {
            v.entity(&format!("generic.patch.{k}"), e, false, fragment);
        }
    }
    for f in compute_requires(d) {
        if !d.requires.contains(&f) {
            let name = Feature::from_code(f as u64)
                .map(|x| x.as_str())
                .unwrap_or("?");
            v.err("requires", format!("feature {name} is used but not listed"));
        }
    }
    let mut sorted = d.requires.clone();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted != d.requires {
        v.err("requires", "must be ascending and unique");
    }
    if v.errs.is_empty() {
        Ok(())
    } else {
        Err(v.errs)
    }
}
