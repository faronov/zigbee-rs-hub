//! Semantic validation (spec 6, 7.5, 8.5, 12.1) and `requires` computation.
//!
//! Errors carry a structured path and a static message, so validation links
//! no formatting code; `Display` renders them on demand.

use crate::enums::{Feature, Platform, zcl_type_name};
use crate::expr::{self, Context, Site, Ty, VerifyError};
use crate::model::*;
use alloc::vec::Vec;

/// Where an error is: `entities[3].parts.hvac_mode.triggers[0].event`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ErrPath {
    /// Top-level section (`entities`, `match`, `configure`, `generic.patch`,
    /// `requires`, `id`, ...); empty for the whole definition.
    pub section: &'static str,
    /// Index in the section (entity, clause, op, patch entry).
    pub index: Option<u16>,
    /// Part key (a `keys::SUB` part name key).
    pub part: Option<u16>,
    pub trigger: Option<u16>,
    /// Field inside the item (`from`, `event`, `press.payload`, ...).
    pub field: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    pub path: ErrPath,
    pub message: &'static str,
    /// The verifier's error for bytecode.
    pub verify: Option<VerifyError>,
    /// The feature ID a `requires` error is about.
    pub feature: Option<u16>,
}

impl core::fmt::Display for ErrPath {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.section)?;
        if let Some(i) = self.index {
            write!(f, "[{i}]")?;
        }
        if let Some(p) = self.part {
            write!(
                f,
                ".parts.{}",
                crate::keys::sub_name(p as u64).unwrap_or("?")
            )?;
        }
        if let Some(t) = self.trigger {
            write!(f, ".triggers[{t}]")?;
        }
        if !self.field.is_empty() {
            if !self.section.is_empty() {
                f.write_str(".")?;
            }
            f.write_str(self.field)?;
        }
        Ok(())
    }
}

impl core::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)?;
        if let Some(v) = &self.verify {
            write!(f, " (offset {}: {:?})", v.offset, v.kind)?;
        }
        if let Some(x) = self.feature {
            write!(
                f,
                " ({})",
                Feature::from_code(x as u64)
                    .map(|x| x.as_str())
                    .unwrap_or("?")
            )?;
        }
        Ok(())
    }
}

/// Context of a `configure` `cmd` payload: evaluated when the configure plan
/// is built, with `opt.*` and `state.*` but no `x`, `payload` or time
/// functions, so the plan (and its idempotence keys) is stable.
pub fn configure_payload_context(gp_def: bool) -> Context {
    Context::new(Site::Attr)
        .with_payload(false)
        .with_gpd(gp_def)
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
    let m = e.more();
    if !e.map.is_empty() {
        return Ty::STR;
    }
    if m.value_on.is_some() || m.value_off.is_some() {
        return Ty::BOOL;
    }
    if let Some(c) = &m.codec {
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
            .any(|e| e.gp.is_some() || e.more().triggers.iter().any(|t| t.gp_cmd.is_some()))
}

fn add(f: &mut Vec<u16>, x: u16) {
    if !f.contains(&x) {
        f.push(x);
    }
}

fn walk_features(e: &Entity, f: &mut Vec<u16>, any_expr: &mut bool) {
    e.for_each_expr(&mut |_, _| *any_expr = true);
    let m = e.more();
    if e.tuya.is_some()
        || m.triggers.iter().any(|t| t.tuya.is_some())
        || matches!(m.press, Some(Press::Tuya { .. }))
    {
        add(f, Feature::Tuya as u16);
    }
    if e.gp.is_some() || m.triggers.iter().any(|t| t.gp_cmd.is_some()) {
        add(f, Feature::Gp as u16);
    }
    if let Some(s) = &m.struct_ {
        add(f, codec_feature(s.codec) as u16);
    }
    if let Some(c) = &m.codec {
        add(f, c.feature() as u16);
    }
    for (_, p) in &m.parts {
        walk_features(p, f, any_expr);
    }
}

/// Features the definition uses (spec 8.5), ascending.
pub fn compute_requires(d: &Definition) -> Vec<u16> {
    let mut f: Vec<u16> = Vec::new();
    let mut any_expr = false;
    for e in &d.entities {
        walk_features(e, &mut f, &mut any_expr);
    }
    if let Some(g) = &d.generic {
        for (_, e) in &g.patch {
            walk_features(e, &mut f, &mut any_expr);
        }
    }
    if d.configure.iter().any(|op| {
        matches!(
            op,
            ConfigureOp::Cmd {
                payload: Some(_),
                ..
            }
        )
    }) {
        any_expr = true;
    }
    if any_expr {
        add(&mut f, Feature::ExprV1 as u16);
    }
    if d.tuya.is_some() {
        add(&mut f, Feature::Tuya as u16);
    }
    if d.gp.is_some() || d.match_.iter().any(|c| c.gp.is_some()) {
        add(&mut f, Feature::Gp as u16);
    }
    // Hubs before it would ignore no clause field: they skip the definition.
    if d.match_.iter().any(|c| c.date_code.is_some()) {
        add(&mut f, Feature::MatchDateCode as u16);
    }
    for t in &d.templates {
        add(&mut f, t.name.feature());
    }
    if d.x_wasm.is_some() {
        add(&mut f, Feature::Wasm as u16);
    }
    crate::sort_small_by_key(&mut f, |x| *x);
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
    fn err(&mut self, path: ErrPath, message: &'static str) {
        self.errs.push(ValidationError {
            path,
            message,
            verify: None,
            feature: None,
        });
    }

    fn expr(&mut self, path: ErrPath, x: &Expr, ctx: Context, want: Ty) {
        match expr::verify(x.code(), &self.d.consts, &ctx) {
            Ok(t) => {
                if !t.compatible(want) {
                    self.err(path, "expression has the wrong result type for its field");
                }
            }
            Err(e) => self.errs.push(ValidationError {
                path,
                message: "bytecode rejected",
                verify: Some(e),
                feature: None,
            }),
        }
    }

    fn entity(&mut self, p: ErrPath, e: &Entity, full: bool, fragment: bool) {
        let m = e.more();
        let at = |field| ErrPath { field, ..p };
        if full {
            match &e.key {
                Some(k) if valid_key(k) => {}
                Some(_) => self.err(at("key"), "entity key is not snake_case"),
                None => self.err(p, "entity without key"),
            }
            if e.platform.is_none() {
                self.err(p, "entity without platform");
            }
        }
        let bindings = e.zcl.is_some() as u8
            + e.tuya.is_some() as u8
            + m.struct_.is_some() as u8
            + e.gp.is_some() as u8
            + m.hub.is_some() as u8
            + (!m.parts.is_empty()) as u8
            + (!m.triggers.is_empty()) as u8
            + m.press.is_some() as u8;
        if bindings > 1 {
            self.err(
                p,
                "more than one binding (zcl, tuya, struct, gp, hub, parts, triggers, press)",
            );
        }
        if full && bindings == 0 {
            self.err(p, "entity without binding");
        }
        if let Some(pl) = e.platform {
            if pl == Platform::Event && (m.triggers.is_empty() || m.event_types.is_empty()) {
                self.err(p, "event entity needs triggers and event_types");
            }
            if pl == Platform::Button && m.press.is_none() && full {
                self.err(p, "button entity needs press");
            }
            if !m.parts.is_empty() && !pl.is_composite() {
                self.err(at("parts"), "parts on a non-composite platform");
            }
        }
        if fragment && (e.tuya.is_some() || m.struct_.is_some() || e.gp.is_some()) {
            self.err(p, "self-description fragments allow only zcl bindings");
        }
        if let Some(z) = &e.zcl {
            if zcl_type_name(z.zcl_type).is_none() {
                self.err(at("zcl.type"), "unknown ZCL type");
            }
        }
        if let Some(s) = &m.struct_ {
            if !STRUCT_CODECS.contains(&s.codec) {
                self.err(at("struct.codec"), "not a struct codec");
            }
        }
        for (i, (a, _)) in e.map.iter().enumerate() {
            if e.map[..i].iter().any(|(b, _)| a == b) {
                self.err(at("map"), "duplicate raw value");
            }
        }
        for (i, t) in m.event_types.iter().enumerate() {
            if m.event_types[..i].contains(t) {
                self.err(at("event_types"), "duplicate event type");
            }
        }
        let gp = self.gp;
        if let Some(x) = &e.from {
            self.expr(at("from"), x, context_for(e, Site::From, gp), Ty::ANY);
        }
        if let Some(x) = &m.to {
            self.expr(at("to"), x, context_for(e, Site::To, gp), Ty::ANY);
        }
        if let Some(x) = &m.reset_after {
            self.expr(
                at("reset_after"),
                x,
                context_for(e, Site::ResetAfter, gp),
                Ty::NUM,
            );
        }
        if let Some(x) = &m.present_if {
            self.expr(
                at("present_if"),
                x,
                context_for(e, Site::PresentIf, gp),
                Ty::BOOL,
            );
        }
        for (_, x) in &m.attrs {
            self.expr(at("attrs"), x, context_for(e, Site::Attr, gp), Ty::ANY);
        }
        for (i, t) in m.triggers.iter().enumerate() {
            let tp = ErrPath {
                trigger: Some(i as u16),
                ..p
            };
            let kinds = t.cmd.is_some() as u8 + t.gp_cmd.is_some() as u8 + t.tuya.is_some() as u8;
            if kinds != 1 {
                self.err(tp, "a trigger needs exactly one of cmd, gp_cmd, tuya");
            }
            if fragment && t.cmd.is_none() {
                self.err(tp, "self-description fragments allow only cmd triggers");
            }
            if let Some(x) = &t.when {
                self.expr(
                    ErrPath {
                        field: "when",
                        ..tp
                    },
                    x,
                    context_for(e, Site::When, gp),
                    Ty::BOOL,
                );
            }
            self.expr(
                ErrPath {
                    field: "event",
                    ..tp
                },
                &t.event,
                context_for(e, Site::Event, gp),
                Ty::STR,
            );
            for (_, x) in &t.attrs {
                self.expr(
                    ErrPath {
                        field: "attrs",
                        ..tp
                    },
                    x,
                    context_for(e, Site::Attr, gp),
                    Ty::ANY,
                );
            }
        }
        match &m.press {
            Some(Press::Cmd {
                payload: Some(x), ..
            }) => self.expr(
                at("press.payload"),
                x,
                context_for(e, Site::PressPayload, gp),
                Ty::BYTES,
            ),
            Some(Press::Tuya { .. }) if fragment => self.err(
                at("press"),
                "self-description fragments allow only cmd/write presses",
            ),
            _ => {}
        }
        for (k, part) in &m.parts {
            self.entity(
                ErrPath {
                    part: Some(*k),
                    ..p
                },
                part,
                false,
                fragment,
            );
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
    let top = |section| ErrPath {
        section,
        ..ErrPath::default()
    };
    let fragment = d.is_fragment();
    if fragment {
        if d.id.is_some() || !d.match_.is_empty() || !d.white_label.is_empty() {
            v.err(top(""), "a fragment has no id, match or white_label");
        }
        if d.tuya.is_some() || d.gp.is_some() || !d.templates.is_empty() {
            v.err(top(""), "a fragment has no tuya, gp or templates block");
        }
        for (i, op) in d.configure.iter().enumerate() {
            if !matches!(
                op,
                ConfigureOp::Report { .. } | ConfigureOp::Read { .. } | ConfigureOp::Checkin { .. }
            ) {
                v.err(
                    ErrPath {
                        section: "configure",
                        index: Some(i as u16),
                        ..ErrPath::default()
                    },
                    "fragments allow only report, read and checkin ops",
                );
            }
        }
    } else {
        match &d.id {
            Some(id) if valid_id(id) => {}
            Some(_) => v.err(top("id"), "invalid id"),
            None => v.err(top("id"), "missing"),
        }
        if d.revision.is_none() {
            v.err(top("revision"), "missing");
        }
        if d.vendor.is_none() {
            v.err(top("vendor"), "missing");
        }
        if d.model.is_none() {
            v.err(top("model"), "missing");
        }
        if d.match_.is_empty() {
            v.err(top("match"), "at least one clause is required");
        }
        for (i, c) in d.match_.iter().enumerate() {
            let p = ErrPath {
                section: "match",
                index: Some(i as u16),
                ..ErrPath::default()
            };
            let zig = c.manufacturer.is_some()
                || c.model.is_some()
                || c.mfr_code.is_some()
                || !c.endpoints.is_empty()
                || c.sw_build.is_some()
                || c.app_version.is_some()
                || c.hw_version.is_some()
                || c.stack_version.is_some()
                || c.power.is_some()
                || c.logical_type.is_some();
            if c.gp.is_some() && zig {
                v.err(p, "a GP clause cannot have Zigbee fields");
            }
            if c.gp.is_none()
                && c.manufacturer.is_none()
                && c.model.is_none()
                && c.endpoints.is_empty()
                && c.mfr_code.is_none()
            {
                v.err(p, "empty clause");
            }
        }
    }
    let mut seen: Vec<(Option<u8>, &str)> = Vec::new();
    for (i, e) in d.entities.iter().enumerate() {
        let p = ErrPath {
            section: "entities",
            index: Some(i as u16),
            ..ErrPath::default()
        };
        v.entity(p, e, true, fragment);
        if let Some(k) = e.key.as_deref() {
            if seen.contains(&(e.ep, k)) {
                v.err(p, "duplicate entity key");
            }
            seen.push((e.ep, k));
        }
    }
    if let Some(g) = &d.generic {
        for (i, (_, e)) in g.patch.iter().enumerate() {
            v.entity(
                ErrPath {
                    section: "generic.patch",
                    index: Some(i as u16),
                    ..ErrPath::default()
                },
                e,
                false,
                fragment,
            );
        }
    }
    for (i, op) in d.configure.iter().enumerate() {
        if let ConfigureOp::Cmd {
            payload: Some(x), ..
        } = op
        {
            v.expr(
                ErrPath {
                    section: "configure",
                    index: Some(i as u16),
                    field: "payload",
                    ..ErrPath::default()
                },
                x,
                configure_payload_context(v.gp),
                Ty::BYTES,
            );
        }
    }
    for f in compute_requires(d) {
        if !d.requires.contains(&f) {
            v.errs.push(ValidationError {
                path: top("requires"),
                message: "a used feature is not listed",
                verify: None,
                feature: Some(f),
            });
        }
    }
    if d.requires.windows(2).any(|w| w[0] >= w[1]) {
        v.err(top("requires"), "must be ascending and unique");
    }
    if v.errs.is_empty() {
        Ok(())
    } else {
        Err(v.errs)
    }
}
