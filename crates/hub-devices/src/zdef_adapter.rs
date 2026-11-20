//! Adapter from the `zdef` crate (definition model, canonical CBOR,
//! expression VM, matching) to the engine model.
//!
//! The hub accepts CBOR only (D15): `zdef::load` decodes, checks the
//! canonical form, validates (every expression verified) and checks
//! `requires`; this module converts the result into [`crate::model`]
//! records and wraps each bytecode expression so the engine runs it on the
//! `zdef` VM. Matching uses `zdef::matching` for definitions that carry
//! their `zdef` source.

use std::sync::Arc;

use typed_arena::Arena;
use zdef::expr::{FP_FIELDS, GPD_FIELDS, Host, Inputs, Value as ZValue, Vm};
use zdef::model as z;

use crate::expr::{Expr, ExprCtx, ExprEnv, Expression, Value};
use crate::fingerprint::Fingerprint;
use crate::model::*;
use crate::zcl::ZclType;

/// A `zdef` bytecode expression run on the `zdef` VM.
pub struct ZdefExpr {
    def: Arc<z::Definition>,
    code: z::Expr,
}

impl std::fmt::Debug for ZdefExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "zdef:{}", crate::expr::hex(self.code.code()))
    }
}

struct ZHost<'a> {
    env: &'a dyn ExprEnv,
    strs: &'a Arena<String>,
    bytes: &'a Arena<Vec<u8>>,
}

impl<'a> ZHost<'a> {
    fn borrow(&self, v: Value) -> ZValue<'a> {
        match v {
            Value::Null => ZValue::Null,
            Value::Bool(b) => ZValue::Bool(b),
            Value::Int(i) => ZValue::Int(i),
            Value::Float(f) => ZValue::Float(f),
            Value::Str(s) => ZValue::Str(self.strs.alloc(s).as_str()),
            Value::Bytes(b) => ZValue::Bytes(self.bytes.alloc(b).as_slice()),
        }
    }
}

impl<'a> Host<'a> for ZHost<'a> {
    fn state(&self, key: &str) -> ZValue<'a> {
        self.borrow(self.env.state(key))
    }
    fn opt(&self, key: &str) -> ZValue<'a> {
        self.borrow(self.env.opt(key))
    }
    fn gpd(&self, field: u8) -> ZValue<'a> {
        GPD_FIELDS.get(usize::from(field)).map_or(ZValue::Null, |f| self.borrow(self.env.gpd(f)))
    }
    fn fp(&self, field: u8) -> ZValue<'a> {
        FP_FIELDS.get(usize::from(field)).map_or(ZValue::Null, |(f, _)| self.borrow(self.env.fp(f)))
    }
    fn now(&self) -> Option<i64> {
        Some(self.env.now())
    }
    fn tz_offset(&self) -> Option<i64> {
        Some(self.env.tz_offset())
    }
    fn battery_percent(&self, mv: f64) -> Option<f64> {
        self.env.battery_percent(mv).as_f64()
    }
}

impl Expression for ZdefExpr {
    fn eval(&self, ctx: &ExprCtx<'_>) -> Value {
        let strs = Arena::new();
        let bytes = Arena::new();
        let host = ZHost { env: ctx.env, strs: &strs, bytes: &bytes };
        let x = host.borrow(ctx.x.clone());
        let inputs = Inputs { x, payload: ctx.payload, last: ctx.last };
        let mut vm = Vm::new();
        match vm.eval_or_null(self.code.code(), &self.def.consts, &inputs, &host) {
            ZValue::Null => Value::Null,
            ZValue::Bool(b) => Value::Bool(b),
            ZValue::Int(i) => Value::Int(i),
            ZValue::Float(f) => Value::Float(f),
            ZValue::Str(s) => Value::Str(s.to_string()),
            ZValue::Bytes(b) => Value::Bytes(b.to_vec()),
        }
    }
}

/// Conversion failure: the definition uses something this engine cannot
/// run (it is then skipped like a `requires` miss).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptError(pub String);

struct Cx {
    def: Arc<z::Definition>,
}

impl Cx {
    fn expr(&self, e: &z::Expr) -> Expr {
        Arc::new(ZdefExpr { def: self.def.clone(), code: e.clone() })
    }
    fn opt_expr(&self, e: &Option<z::Expr>) -> Option<Expr> {
        e.as_ref().map(|e| self.expr(e))
    }
}

fn num(n: &Option<z::Num>) -> Option<f64> {
    n.map(z::Num::as_f64)
}

fn scalar(s: &z::Scalar) -> Value {
    match s {
        z::Scalar::Null => Value::Null,
        z::Scalar::Bool(b) => Value::Bool(*b),
        z::Scalar::Int(i) => Value::Int(*i),
        z::Scalar::Float(f) => Value::Float(*f),
        z::Scalar::Str(s) => Value::Str(s.clone()),
    }
}

fn report(r: &z::Report) -> ReportSpec {
    match r {
        z::Report::Off => ReportSpec::Off,
        z::Report::Config { min, max, change } => ReportSpec::Config { min: *min, max: *max, change: num(change) },
    }
}

fn table(t: Option<zdef::enums::TableRef>, tab: &[&str]) -> Option<String> {
    match t? {
        zdef::enums::TableRef::Known(c) => tab.get(usize::from(c)).map(|s| (*s).to_string()),
        zdef::enums::TableRef::Unknown(_) => None,
    }
}

fn access(bits: Option<u8>) -> Option<Access> {
    bits.map(|b| Access {
        read: b & zdef::enums::ACCESS_READ != 0,
        write: b & zdef::enums::ACCESS_WRITE != 0,
        report: b & zdef::enums::ACCESS_REPORT != 0,
    })
}

fn tuya_ty(t: zdef::enums::TuyaType) -> TuyaType {
    TuyaType::from_code(t.code()).unwrap_or(TuyaType::Raw)
}

fn codec(c: &z::Codec) -> Result<Codec, AdaptError> {
    Ok(match c {
        z::Codec::ZclTagged => Codec::ZclTagged,
        z::Codec::TuyaScheduleDay { day, transitions } => Codec::TuyaScheduleDay { day: *day, transitions: *transitions },
        z::Codec::BcdTime => Codec::BcdTime,
        z::Codec::Hhmm => Codec::Hhmm,
        z::Codec::Ascii => Codec::Ascii,
        z::Codec::Utf16beHex => Codec::Utf16BeHex,
        z::Codec::BitmapFlags(v) => Codec::BitmapFlags(v.clone()),
        z::Codec::TuyaScheduleWeek { variant } => return Err(AdaptError(format!("codec tuya_schedule_week/{variant} not implemented"))),
    })
}

fn value_def(cx: &Cx, e: &z::Entity) -> Result<ValueDef, AdaptError> {
    let binding = if let Some(b) = &e.zcl {
        Binding::Zcl(ZclBinding { cluster: b.cluster, attr: b.attr, ty: ZclType(b.zcl_type), mfr: b.mfr, write: ZclWrite::Attribute })
    } else if let Some(t) = &e.tuya {
        Binding::Tuya { dp: t.dp, ty: tuya_ty(t.ty) }
    } else if let Some(s) = &e.struct_ {
        if s.codec != "zcl_tagged" {
            return Err(AdaptError(format!("struct codec {}", s.codec)));
        }
        Binding::Struct { cluster: s.cluster, attr: s.attr, mfr: s.mfr, codec: Codec::ZclTagged, tag: s.tag.unwrap_or(0) as u8 }
    } else if let Some(g) = &e.gp {
        Binding::Gp(match g {
            z::GpBinding::Report { cluster, attr, zcl_type, mfr } => GpBinding::Report { cluster: *cluster, attr: *attr, ty: ZclType(*zcl_type), mfr: *mfr },
            z::GpBinding::Cmd(c) => GpBinding::Cmd(*c),
        })
    } else if let Some(h) = &e.hub {
        Binding::Hub { default: h.default.as_ref().map_or(Value::Null, scalar) }
    } else {
        Binding::None
    };
    Ok(ValueDef {
        binding,
        transforms: Transforms {
            codec: e.codec.as_ref().map(codec).transpose()?,
            invalid: num(&e.invalid),
            map: e.map.clone(),
            value_on: e.value_on.as_ref().map(scalar),
            value_off: e.value_off.as_ref().map(scalar),
            from: cx.opt_expr(&e.from),
            to: cx.opt_expr(&e.to),
            mul: num(&e.mul),
            div: num(&e.div),
            offset: num(&e.offset),
            precision: e.precision,
            attr_scale: None,
        },
        limits: Limits { min: num(&e.min), max: num(&e.max), step: num(&e.step), mode: e.mode.clone() },
        report: e.report.as_ref().map(report),
        access: access(e.access),
    })
}

fn platform(p: zdef::enums::Platform) -> Platform {
    Platform::parse(p.as_str()).unwrap_or(Platform::Sensor)
}

fn entity(cx: &Cx, e: &z::Entity) -> Result<EntityDef, AdaptError> {
    let key = e.key.clone().ok_or_else(|| AdaptError("entity without key".into()))?;
    let p = e.platform.ok_or_else(|| AdaptError(format!("{key}: no platform")))?;
    let mut d = EntityDef::new(key, platform(p));
    d.device_class = table(e.device_class, zdef::enums::DEVICE_CLASSES);
    d.unit = table(e.unit, zdef::enums::UNITS);
    d.state_class = e.state_class.and_then(|s| StateClass::parse(s.as_str()));
    d.category = e.category.and_then(|c| Category::parse(c.as_str()));
    d.name = e.name.clone();
    d.enabled = e.enabled.unwrap_or(true);
    d.ep = e.ep.or(e.zcl.and_then(|z| z.ep)).or(e.struct_.as_ref().and_then(|s| s.ep));
    d.value = value_def(cx, e)?;
    for (k, pe) in &e.parts {
        let name = zdef::keys::sub_name(u64::from(*k)).ok_or_else(|| AdaptError(format!("unknown part key {k}")))?;
        d.parts.push((name.to_string(), value_def(cx, pe)?));
    }
    for t in &e.triggers {
        let source = if let Some(c) = t.cmd {
            TriggerSource::Cmd { ep: c.ep, cluster: c.cluster, id: c.id, mfr: c.mfr, global: false }
        } else if let Some(g) = t.gp_cmd {
            TriggerSource::GpCmd(g)
        } else if let Some(dp) = t.tuya {
            TriggerSource::Tuya { dp: dp.dp }
        } else {
            return Err(AdaptError("trigger without source".into()));
        };
        let mut attrs: Vec<(String, Expr)> = t.attrs.iter().map(|(k, x)| (k.clone(), cx.expr(x))).collect();
        attrs.extend(e.attrs.iter().map(|(k, x)| (k.clone(), cx.expr(x))));
        d.triggers.push(Trigger { source, when: cx.opt_expr(&t.when), event: EventSel::Expr(cx.expr(&t.event)), attrs });
    }
    d.event_types = e.event_types.clone();
    d.press = match &e.press {
        None => None,
        Some(z::Press::Cmd { cmd, payload }) => Some(Press::Cmd {
            cluster: cmd.cluster,
            id: cmd.id,
            mfr: cmd.mfr,
            ep: cmd.ep,
            payload: payload.as_ref().map(|p| PressPayload::Expr(cx.expr(p))),
        }),
        Some(z::Press::Write(w)) => Some(Press::Write { cluster: w.cluster, attr: w.attr, ty: ZclType(w.zcl_type), value: scalar(&w.value), mfr: w.mfr }),
        Some(z::Press::Tuya { dp, value }) => Some(Press::Tuya { dp: dp.dp, ty: tuya_ty(dp.ty), value: scalar(value) }),
    };
    d.reset_after = cx.opt_expr(&e.reset_after);
    d.present_if = cx.opt_expr(&e.present_if);
    d.invert = e.invert.unwrap_or(false);
    Ok(d)
}

fn patch(e: &z::Entity) -> EntityPatch {
    EntityPatch {
        report: e.report.as_ref().map(report),
        mul: num(&e.mul),
        div: num(&e.div),
        offset: num(&e.offset),
        precision: e.precision,
        invalid: num(&e.invalid),
        min: num(&e.min),
        max: num(&e.max),
        step: num(&e.step),
        name: e.name.clone(),
        enabled: e.enabled,
        category: e.category.and_then(|c| Category::parse(c.as_str())),
        device_class: table(e.device_class, zdef::enums::DEVICE_CLASSES),
        unit: table(e.unit, zdef::enums::UNITS),
        invert: e.invert,
    }
}

fn name_match(m: &z::NameMatch) -> StrMatch {
    match m {
        z::NameMatch::Exact(s) => StrMatch::Exact(s.clone()),
        z::NameMatch::Any(v) => StrMatch::Any(v.clone()),
        z::NameMatch::Prefix(p) => StrMatch::Prefix(p.clone()),
    }
}

fn range(r: &z::VersionRange) -> (u32, u32) {
    (r.min.unwrap_or(0).clamp(0, i64::from(u32::MAX)) as u32, r.max.unwrap_or(i64::from(u32::MAX)).clamp(0, i64::from(u32::MAX)) as u32)
}

fn clause(c: &z::MatchClause) -> MatchClause {
    MatchClause {
        manufacturer: c.manufacturer.as_ref().map(name_match),
        model: c.model.as_ref().map(name_match),
        mfr_code: c.mfr_code,
        app_version: c.app_version.as_ref().map(range),
        stack_version: c.stack_version.as_ref().map(range),
        hw_version: c.hw_version.as_ref().map(range),
        sw_build: c.sw_build.as_ref().map(name_match),
        power: c.power.and_then(|p| PowerKind::parse(p.as_str())),
        endpoints: c
            .endpoints
            .iter()
            .map(|(ep, m)| EpMatch { ep: *ep, profile: m.profile, device_id: m.device_id, in_clusters: m.in_clusters.clone(), out_clusters: m.out_clusters.clone() })
            .collect(),
        gp: c.gp.as_ref().map(|g| GpMatch {
            device_id: g.device_id,
            manufacturer_id: g.manufacturer_id,
            model_id: g.model_id.map(|m| (m.value, m.mask.unwrap_or(0xFFFF))),
            src_id: g.src_id,
            ieee_prefix: g.ieee.as_ref().and_then(|h| {
                let n = (h.len() / 2).min(8) as u8;
                u64::from_str_radix(&format!("{h:0<16}")[..16], 16).ok().map(|v| (v, n))
            }),
            commands: g.commands.clone(),
            server_clusters: g.clusters.as_ref().map(|c| c.server.clone()).unwrap_or_default(),
            client_clusters: g.clusters.as_ref().map(|c| c.client.clone()).unwrap_or_default(),
        }),
    }
}

fn configure(op: &z::ConfigureOp) -> ConfigureOp {
    match op {
        z::ConfigureOp::Bind { ep, clusters } => ConfigureOp::Bind { ep: *ep, clusters: clusters.clone() },
        z::ConfigureOp::Write(w) => ConfigureOp::Write { ep: w.ep, cluster: w.cluster, attr: w.attr, ty: ZclType(w.zcl_type), value: scalar(&w.value), mfr: w.mfr },
        z::ConfigureOp::Read { ep, cluster, attrs, mfr } => ConfigureOp::Read { ep: *ep, cluster: *cluster, attrs: attrs.clone(), mfr: *mfr },
        z::ConfigureOp::Report { ep, cluster, attr, zcl_type, report: r, mfr } => {
            ConfigureOp::Report { ep: *ep, cluster: *cluster, attr: *attr, ty: ZclType(*zcl_type), report: report(r), mfr: *mfr }
        }
        z::ConfigureOp::Cmd { ep, cluster, id, mfr } => ConfigureOp::Cmd { ep: *ep, cluster: *cluster, id: *id, mfr: *mfr, payload: Vec::new() },
        z::ConfigureOp::Group { ep, group } => ConfigureOp::Group { ep: *ep, group: *group },
        z::ConfigureOp::Checkin { interval_qs } => ConfigureOp::Checkin { interval_qs: *interval_qs },
    }
}

/// Converts a loaded `zdef` definition.
pub fn adapt(zd: z::Definition, source: LayerSource, content_hash: Option<String>) -> Result<Definition, AdaptError> {
    let def = Arc::new(zd);
    let cx = Cx { def: def.clone() };
    let zd = &*def;
    let source = if zd.is_fragment() { LayerSource::SelfDescription } else { source };
    let id = zd.id.clone().unwrap_or_else(|| if zd.is_fragment() { "self-description".into() } else { "unnamed".into() });
    let mut d = Definition::new(id, source);
    d.revision = zd.revision.unwrap_or(1);
    d.vendor = zd.vendor.clone().unwrap_or_default();
    d.model = zd.model.clone().unwrap_or_default();
    d.matches = zd.match_.iter().map(clause).collect();
    d.white_label = zd
        .white_label
        .iter()
        .map(|w| WhiteLabel {
            vendor: w.vendor.clone().unwrap_or_else(|| d.vendor.clone()),
            model: w.model.clone().unwrap_or_else(|| d.model.clone()),
            manufacturer: w.manufacturer.clone(),
        })
        .collect();
    d.requires = zd.requires.iter().filter_map(|f| zdef::enums::Feature::from_code(u64::from(*f)).map(|x| x.as_str().to_string())).collect();
    d.priority = zd.priority.unwrap_or(0) as i32;
    if let Some(dev) = &zd.device {
        d.device = DeviceFields {
            power: dev.power.and_then(|p| PowerKind::parse(p.as_str())),
            sleepy: dev.sleepy,
            battery: dev.battery.as_ref().map(|b| match b {
                z::Battery::Linear { min_mv, max_mv } => BatteryCurve::Linear(f64::from(*min_mv), f64::from(*max_mv)),
                z::Battery::Curve(n) => BatteryCurve::Named(n.clone()),
            }),
            checkin_s: dev.checkin_s,
            availability_timeout_s: dev.availability.and_then(|a| a.timeout_s).map(u64::from),
            ota: dev.ota,
        };
    }
    if let Some(g) = &zd.generic {
        d.generic = GenericKnobs {
            exclude: g.exclude.clone(),
            only_endpoints: (!g.only_endpoints.is_empty()).then(|| g.only_endpoints.clone()),
            patch: g.patch.iter().map(|(k, e)| (k.clone(), patch(e))).collect(),
            disable: g.disable.unwrap_or(false),
        };
    }
    d.endpoints = zd.endpoints.iter().cloned().collect();
    d.self_description = match zd.self_description {
        Some(z::SelfDescription::Ignore) => SelfDescMode::Ignore,
        Some(z::SelfDescription::Under) => SelfDescMode::Under,
        _ => SelfDescMode::Over,
    };
    d.configure = zd.configure.iter().map(configure).collect();
    d.ignore = zd.ignore.iter().map(|r| IgnoreRule { ep: r.ep, cluster: r.cluster, cmd: r.cmd }).collect();
    d.entities = zd.entities.iter().map(|e| entity(&cx, e)).collect::<Result<_, _>>()?;
    d.tuya = zd.tuya.as_ref().map(|t| {
        let mut f = TuyaFlags::default();
        f.time_sync = match t.time_sync {
            Some(z::TimeSync::Epoch1970) => TimeSync::Epoch1970,
            Some(z::TimeSync::Epoch2000) => TimeSync::Epoch2000,
            _ => TimeSync::Off,
        };
        f.force_time_sync_s = t.force_time_sync_s;
        f.magic_packet = t.magic_packet.unwrap_or(false);
        match &t.query {
            Some(z::TuyaQuery::On { configure, announce }) => {
                f.query_on_configure = *configure;
                f.query_on_announce = *announce;
            }
            Some(z::TuyaQuery::Interval(s)) => f.query_interval_s = Some(*s),
            None => {}
        }
        f.bind_basic = t.bind_basic.unwrap_or(false);
        f.gateway_status = t.gateway_status.unwrap_or(true);
        f.send = if t.send == Some(z::TuyaSend::SendData) { TuyaSend::SendData } else { TuyaSend::DataRequest };
        f.mcu_version = t.mcu_version.unwrap_or(false);
        f.optimistic = t.optimistic.unwrap_or(false);
        f.dedup = t.dedup.unwrap_or(true);
        f
    });
    d.gp = zd.gp.as_ref().map(|g| GpFlags { report_interval_s: g.report_interval_s, security_min: g.security_min, ignore_commands: g.ignore_commands.clone() });
    d.mfr = zd.mfr;
    d.content_hash = content_hash;
    d.zdef = Some(def.clone());
    Ok(d)
}

/// The hub's load path for CBOR (catalog, uploads, fetched definitions,
/// self-descriptions): `zdef::load` + [`adapt`].
pub fn load_cbor(cbor: &[u8], source: LayerSource) -> Result<Definition, AdaptError> {
    let zd = zdef::load(cbor, &zdef::HubFeatures::v1()).map_err(|e| AdaptError(e.to_string()))?;
    let hash = zdef::ContentHash::of(cbor);
    adapt(zd, source, Some(format!("{hash}")))
}

/// The engine fingerprint as a `zdef` identity.
pub fn zdef_fingerprint(fp: &Fingerprint) -> zdef::Fingerprint {
    let u8v = |v: Option<u32>| v.map(|x| x.min(255) as u8);
    zdef::Fingerprint {
        manufacturer_name: fp.manufacturer_name.clone(),
        model_id: fp.model_id.clone(),
        mfr_code: fp.mfr_code,
        logical_type: fp.logical_type.map(|l| match l {
            crate::zdo::LogicalType::Coordinator => 0,
            crate::zdo::LogicalType::Router => 1,
            crate::zdo::LogicalType::EndDevice => 2,
            crate::zdo::LogicalType::Unknown => 7,
        }),
        power_source: fp.power_source,
        rx_on_when_idle: fp.rx_on_when_idle,
        app_version: u8v(fp.app_version),
        stack_version: u8v(fp.stack_version),
        hw_version: u8v(fp.hw_version),
        zcl_version: u8v(fp.zcl_version),
        date_code: fp.date_code.clone(),
        sw_build_id: fp.sw_build_id.clone(),
        endpoints: fp
            .endpoints
            .iter()
            .map(|e| zdef::matching::Endpoint { ep: e.ep, profile: e.profile, device_id: e.device_id, in_clusters: e.in_clusters.clone(), out_clusters: e.out_clusters.clone() })
            .collect(),
        self_description: fp.self_description.as_ref().and_then(|h| {
            let b: Vec<u8> = (0..h.len() / 2).filter_map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).ok()).collect();
            b.try_into().ok()
        }),
    }
}

/// The GP part of the fingerprint as a `zdef` identity.
pub fn zdef_gp_fingerprint(g: &crate::fingerprint::GpFingerprint) -> zdef::GpFingerprint {
    zdef::GpFingerprint {
        gpd_id: match g.gpd_id {
            crate::gp::types::GpdId::SrcId(s) => zdef::matching::GpdId::SrcId(s),
            crate::gp::types::GpdId::Ieee { ieee, endpoint } => zdef::matching::GpdId::Ieee { ieee, ep: endpoint },
        },
        device_id: g.device_id,
        manufacturer_id: g.manufacturer_id,
        model_id: g.model_id,
        commands: g.commands.clone(),
        server_clusters: g.server_clusters.clone(),
        client_clusters: g.client_clusters.clone(),
    }
}

/// `zdef::matching` score of a definition for a device.
pub fn zdef_score(zd: &z::Definition, fp: &Fingerprint) -> Option<u32> {
    let s = match &fp.gp {
        Some(g) => zd.match_score(zdef::Identity::Gp(&zdef_gp_fingerprint(g))),
        None => zd.match_score(zdef::Identity::Zigbee(&zdef_fingerprint(fp))),
    };
    s.map(u32::from)
}

/// Self-description decoder backed by `zdef` (12.3 step 5).
#[derive(Debug, Default, Clone, Copy)]
pub struct ZdefSchemaDecoder;

impl crate::hub::SchemaDecoder for ZdefSchemaDecoder {
    fn decode(&self, cbor: &[u8]) -> Result<Definition, String> {
        let d = load_cbor(cbor, LayerSource::SelfDescription).map_err(|e| e.0)?;
        if d.source != LayerSource::SelfDescription {
            return Err("not a definition fragment".into());
        }
        Ok(d)
    }
}
