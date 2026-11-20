//! YAML → engine model loader for host tests (the role `zdefc` + `zdef`
//! decoding play in production; the hub itself accepts CBOR only, D15).
//! Expressions are compiled with the reference interpreter.

use serde_yaml::Value as Y;

use crate::expr::{Expr, Value};
use crate::model::*;
use crate::testutil::expr::compile;
use crate::zcl::ZclType;

type R<T> = Result<T, String>;

fn get<'a>(m: &'a Y, k: &str) -> Option<&'a Y> {
    m.as_mapping().and_then(|m| m.get(Y::String(k.to_string())))
}

fn int(v: &Y) -> R<i64> {
    v.as_i64().ok_or_else(|| format!("integer expected, got {v:?}"))
}

fn num(v: &Y) -> R<f64> {
    v.as_f64().or_else(|| v.as_i64().map(|i| i as f64)).ok_or_else(|| format!("number expected, got {v:?}"))
}

fn s(v: &Y) -> R<String> {
    match v {
        Y::String(s) => Ok(s.clone()),
        Y::Number(n) => Ok(n.to_string()),
        Y::Bool(b) => Ok(b.to_string()),
        _ => Err(format!("string expected, got {v:?}")),
    }
}

fn opt_int(m: &Y, k: &str) -> R<Option<i64>> {
    get(m, k).map(int).transpose()
}

fn opt_u16(m: &Y, k: &str) -> R<Option<u16>> {
    Ok(opt_int(m, k)?.map(|v| v as u16))
}

fn opt_s(m: &Y, k: &str) -> R<Option<String>> {
    get(m, k).map(s).transpose()
}

fn opt_num(m: &Y, k: &str) -> R<Option<f64>> {
    get(m, k).map(num).transpose()
}

fn expr(v: &Y) -> R<Expr> {
    compile(&s(v)?)
}

fn opt_expr(m: &Y, k: &str) -> R<Option<Expr>> {
    get(m, k).map(expr).transpose()
}

fn scalar(v: &Y) -> Value {
    match v {
        Y::Bool(b) => Value::Bool(*b),
        Y::Number(n) => n.as_i64().map_or_else(|| Value::Float(n.as_f64().unwrap_or(0.0)), Value::Int),
        Y::String(s) => Value::Str(s.clone()),
        _ => Value::Null,
    }
}

fn ty(v: &Y) -> R<ZclType> {
    let n = s(v)?;
    ZclType::from_name(&n).ok_or_else(|| format!("unknown ZCL type {n}"))
}

fn report(v: &Y) -> R<ReportSpec> {
    match v {
        Y::String(x) if x == "off" => Ok(ReportSpec::Off),
        Y::Bool(false) => Ok(ReportSpec::Off),
        Y::Sequence(l) if l.len() == 3 => Ok(ReportSpec::Config {
            min: int(&l[0])? as u16,
            max: int(&l[1])? as u16,
            change: Some(num(&l[2])?),
        }),
        Y::Sequence(l) if l.len() == 2 => Ok(ReportSpec::Config { min: int(&l[0])? as u16, max: int(&l[1])? as u16, change: None }),
        _ => Err(format!("bad report {v:?}")),
    }
}

fn str_match(v: &Y) -> R<StrMatch> {
    if let Some(a) = get(v, "any") {
        return Ok(StrMatch::Any(a.as_sequence().ok_or("any: list")?.iter().map(s).collect::<R<_>>()?));
    }
    if let Some(p) = get(v, "prefix") {
        return Ok(StrMatch::Prefix(s(p)?));
    }
    Ok(StrMatch::Exact(s(v)?))
}

fn range(v: &Y) -> R<(u32, u32)> {
    Ok((opt_int(v, "min")?.unwrap_or(0) as u32, opt_int(v, "max")?.unwrap_or(i64::from(u32::MAX)) as u32))
}

fn clusters(v: Option<&Y>) -> R<Vec<u16>> {
    v.map_or(Ok(Vec::new()), |l| l.as_sequence().ok_or("cluster list")?.iter().map(|x| int(x).map(|i| i as u16)).collect())
}

fn gp_match(v: &Y) -> R<GpMatch> {
    let model_id = match get(v, "model_id") {
        None => None,
        Some(m @ Y::Mapping(_)) => {
            Some((opt_int(m, "value")?.ok_or("model_id.value")? as u16, opt_int(m, "mask")?.unwrap_or(0xFFFF) as u16))
        }
        Some(m) => Some((int(m)? as u16, 0xFFFF)),
    };
    let src_id = match get(v, "src_id") {
        Some(Y::Sequence(l)) if l.len() == 2 => Some((int(&l[0])? as u32, int(&l[1])? as u32)),
        Some(x) => {
            let i = int(x)? as u32;
            Some((i, i))
        }
        None => None,
    };
    let cl = get(v, "clusters");
    Ok(GpMatch {
        device_id: opt_int(v, "device_id")?.map(|x| x as u8),
        manufacturer_id: opt_u16(v, "manufacturer_id")?,
        model_id,
        src_id,
        ieee_prefix: None,
        commands: clusters(get(v, "commands"))?.into_iter().map(|c| c as u8).collect(),
        server_clusters: clusters(cl.and_then(|c| get(c, "server")))?,
        client_clusters: clusters(cl.and_then(|c| get(c, "client")))?,
    })
}

fn match_clause(v: &Y) -> R<MatchClause> {
    let mut m = MatchClause {
        manufacturer: get(v, "manufacturer").map(str_match).transpose()?,
        model: get(v, "model").map(str_match).transpose()?,
        mfr_code: opt_u16(v, "mfr_code")?,
        app_version: get(v, "app_version").map(range).transpose()?,
        stack_version: get(v, "stack_version").map(range).transpose()?,
        hw_version: get(v, "hw_version").map(range).transpose()?,
        sw_build: get(v, "sw_build").map(str_match).transpose()?,
        power: opt_s(v, "power")?.and_then(|p| PowerKind::parse(&p)),
        endpoints: Vec::new(),
        gp: get(v, "gp").map(gp_match).transpose()?,
    };
    if let Some(Y::Mapping(eps)) = get(v, "endpoints") {
        for (k, e) in eps {
            m.endpoints.push(EpMatch {
                ep: int(k)? as u8,
                profile: opt_u16(e, "profile")?,
                device_id: opt_u16(e, "device_id")?,
                in_clusters: clusters(get(e, "in"))?,
                out_clusters: clusters(get(e, "out"))?,
            });
        }
    }
    Ok(m)
}

fn codec(v: &Y) -> R<Codec> {
    match v {
        Y::String(n) => Ok(match n.as_str() {
            "zcl_tagged" => Codec::ZclTagged,
            "bcd_time" => Codec::BcdTime,
            "hhmm" => Codec::Hhmm,
            "ascii" => Codec::Ascii,
            "utf16be_hex" => Codec::Utf16BeHex,
            _ => return Err(format!("unknown codec {n}")),
        }),
        Y::Mapping(m) => {
            let (k, p) = m.iter().next().ok_or("empty codec")?;
            match s(k)?.as_str() {
                "tuya_schedule_day" => Ok(Codec::TuyaScheduleDay {
                    day: opt_int(p, "day")?.ok_or("day")? as u8,
                    transitions: opt_int(p, "transitions")?.unwrap_or(4) as u8,
                }),
                "bitmap_flags" => Ok(Codec::BitmapFlags(
                    p.as_mapping()
                        .ok_or("bitmap_flags map")?
                        .iter()
                        .map(|(b, l)| Ok((int(b)? as u8, s(l)?)))
                        .collect::<R<_>>()?,
                )),
                other => Err(format!("unknown codec {other}")),
            }
        }
        _ => Err("bad codec".into()),
    }
}

fn binding(v: &Y) -> R<Binding> {
    if let Some(z) = get(v, "zcl") {
        return Ok(Binding::Zcl(ZclBinding {
            cluster: opt_u16(z, "cluster")?.ok_or("zcl.cluster")?,
            attr: opt_u16(z, "attr")?.ok_or("zcl.attr")?,
            ty: ty(get(z, "type").ok_or("zcl.type")?)?,
            mfr: opt_u16(z, "mfr")?,
            write: ZclWrite::Attribute,
        }));
    }
    if let Some(Y::Sequence(t)) = get(v, "tuya") {
        let tt = s(&t[1])?;
        return Ok(Binding::Tuya { dp: int(&t[0])? as u8, ty: TuyaType::parse(&tt).ok_or(format!("tuya type {tt}"))? });
    }
    if let Some(st) = get(v, "struct") {
        return Ok(Binding::Struct {
            cluster: opt_u16(st, "cluster")?.ok_or("struct.cluster")?,
            attr: opt_u16(st, "attr")?.ok_or("struct.attr")?,
            mfr: opt_u16(st, "mfr")?,
            codec: codec(get(st, "codec").ok_or("struct.codec")?)?,
            tag: opt_int(st, "tag")?.ok_or("struct.tag")? as u8,
        });
    }
    if let Some(g) = get(v, "gp") {
        if let Some(r) = get(g, "report") {
            return Ok(Binding::Gp(GpBinding::Report {
                cluster: opt_u16(r, "cluster")?.ok_or("gp.report.cluster")?,
                attr: opt_u16(r, "attr")?.ok_or("gp.report.attr")?,
                ty: ty(get(r, "type").ok_or("gp.report.type")?)?,
                mfr: opt_u16(r, "mfr")?,
            }));
        }
        return Ok(Binding::Gp(GpBinding::Cmd(opt_int(g, "cmd")?.ok_or("gp.cmd")? as u8)));
    }
    if let Some(h) = get(v, "hub") {
        return Ok(Binding::Hub { default: get(h, "default").map_or(Value::Null, scalar) });
    }
    Ok(Binding::None)
}

fn value_def(v: &Y) -> R<ValueDef> {
    let map = match get(v, "map") {
        Some(Y::Mapping(m)) => m.iter().map(|(k, l)| Ok((int(k)?, s(l)?))).collect::<R<Vec<_>>>()?,
        _ => Vec::new(),
    };
    Ok(ValueDef {
        binding: binding(v)?,
        transforms: Transforms {
            codec: get(v, "codec").map(codec).transpose()?,
            invalid: opt_num(v, "invalid")?,
            map,
            value_on: get(v, "value_on").map(scalar),
            value_off: get(v, "value_off").map(scalar),
            from: opt_expr(v, "from")?,
            to: opt_expr(v, "to")?,
            mul: opt_num(v, "mul")?,
            div: opt_num(v, "div")?,
            offset: opt_num(v, "offset")?,
            precision: opt_int(v, "precision")?.map(|p| p as u8),
            attr_scale: None,
        },
        limits: Limits {
            min: opt_num(v, "min")?,
            max: opt_num(v, "max")?,
            step: opt_num(v, "step")?,
            mode: opt_s(v, "mode")?,
        },
        report: get(v, "report").map(report).transpose()?,
        access: opt_s(v, "access")?.map(|a| Access::parse(&a).ok_or(format!("access {a}"))).transpose()?,
    })
}

fn trigger(v: &Y) -> R<Trigger> {
    let source = if let Some(c) = get(v, "cmd") {
        TriggerSource::Cmd {
            ep: opt_int(c, "ep")?.map(|e| e as u8),
            cluster: opt_u16(c, "cluster")?.ok_or("cmd.cluster")?,
            id: opt_int(c, "id")?.ok_or("cmd.id")? as u8,
            mfr: opt_u16(c, "mfr")?,
            global: false,
        }
    } else if let Some(g) = get(v, "gp_cmd") {
        TriggerSource::GpCmd(int(g)? as u8)
    } else if let Some(Y::Sequence(t)) = get(v, "tuya") {
        TriggerSource::Tuya { dp: int(&t[0])? as u8 }
    } else {
        return Err("trigger source".into());
    };
    let attrs = match get(v, "attrs") {
        Some(Y::Mapping(m)) => m.iter().map(|(k, e)| Ok((s(k)?, expr(e)?))).collect::<R<_>>()?,
        _ => Vec::new(),
    };
    Ok(Trigger {
        source,
        when: opt_expr(v, "when")?,
        event: EventSel::Expr(expr(get(v, "event").ok_or("trigger.event")?)?),
        attrs,
    })
}

fn press(v: &Y) -> R<Press> {
    if let Some(c) = get(v, "cmd") {
        return Ok(Press::Cmd {
            cluster: opt_u16(c, "cluster")?.ok_or("press.cmd.cluster")?,
            id: opt_int(c, "id")?.ok_or("press.cmd.id")? as u8,
            mfr: opt_u16(c, "mfr")?,
            ep: opt_int(c, "ep")?.map(|e| e as u8),
            payload: get(v, "payload").map(|p| expr(p).map(PressPayload::Expr)).transpose()?,
        });
    }
    if let Some(w) = get(v, "write") {
        return Ok(Press::Write {
            cluster: opt_u16(w, "cluster")?.ok_or("press.write.cluster")?,
            attr: opt_u16(w, "attr")?.ok_or("press.write.attr")?,
            ty: ty(get(w, "type").ok_or("press.write.type")?)?,
            value: get(w, "value").map_or(Value::Null, scalar),
            mfr: opt_u16(w, "mfr")?,
        });
    }
    if let Some(Y::Sequence(t)) = get(v, "tuya") {
        let tt = s(&t[1])?;
        return Ok(Press::Tuya {
            dp: int(&t[0])? as u8,
            ty: TuyaType::parse(&tt).ok_or("tuya type")?,
            value: get(v, "value").map_or(Value::Null, scalar),
        });
    }
    Err("bad press".into())
}

fn entity(v: &Y) -> R<EntityDef> {
    let key = opt_s(v, "key")?.ok_or("entity.key")?;
    let p = opt_s(v, "platform")?.ok_or("entity.platform")?;
    let platform = Platform::parse(&p).ok_or(format!("platform {p}"))?;
    let mut e = EntityDef::new(key, platform);
    e.device_class = opt_s(v, "device_class")?;
    e.unit = opt_s(v, "unit")?;
    e.state_class = opt_s(v, "state_class")?.and_then(|x| StateClass::parse(&x));
    e.category = opt_s(v, "category")?.and_then(|x| Category::parse(&x));
    e.name = opt_s(v, "name")?;
    e.enabled = get(v, "enabled").and_then(Y::as_bool).unwrap_or(true);
    e.ep = opt_int(v, "ep")?.map(|x| x as u8).or(get(v, "zcl").and_then(|z| get(z, "ep")).and_then(Y::as_i64).map(|x| x as u8));
    e.value = value_def(v)?;
    if let Some(Y::Mapping(parts)) = get(v, "parts") {
        for (n, pv) in parts {
            e.parts.push((s(n)?, value_def(pv)?));
        }
    }
    if let Some(Y::Sequence(t)) = get(v, "triggers") {
        e.triggers = t.iter().map(trigger).collect::<R<_>>()?;
    }
    if let Some(Y::Sequence(t)) = get(v, "event_types") {
        e.event_types = t.iter().map(s).collect::<R<_>>()?;
    }
    e.press = get(v, "press").map(press).transpose()?;
    e.reset_after = opt_expr(v, "reset_after")?;
    e.present_if = opt_expr(v, "present_if")?;
    e.invert = get(v, "invert").and_then(Y::as_bool).unwrap_or(false);
    Ok(e)
}

fn configure_op(v: &Y) -> R<ConfigureOp> {
    let ep = |m: &Y| opt_int(m, "ep").map(|e| e.map(|x| x as u8));
    if let Some(b) = get(v, "bind") {
        return Ok(ConfigureOp::Bind { ep: ep(b)?, clusters: clusters(get(b, "clusters"))? });
    }
    if let Some(w) = get(v, "write") {
        return Ok(ConfigureOp::Write {
            ep: ep(w)?,
            cluster: opt_u16(w, "cluster")?.ok_or("write.cluster")?,
            attr: opt_u16(w, "attr")?.ok_or("write.attr")?,
            ty: ty(get(w, "type").ok_or("write.type")?)?,
            value: get(w, "value").map_or(Value::Null, scalar),
            mfr: opt_u16(w, "mfr")?,
        });
    }
    if let Some(r) = get(v, "read") {
        return Ok(ConfigureOp::Read {
            ep: ep(r)?,
            cluster: opt_u16(r, "cluster")?.ok_or("read.cluster")?,
            attrs: clusters(get(r, "attrs"))?,
            mfr: opt_u16(r, "mfr")?,
        });
    }
    if let Some(r) = get(v, "report") {
        return Ok(ConfigureOp::Report {
            ep: ep(r)?,
            cluster: opt_u16(r, "cluster")?.ok_or("report.cluster")?,
            attr: opt_u16(r, "attr")?.ok_or("report.attr")?,
            ty: ty(get(r, "type").ok_or("report.type")?)?,
            report: report(get(r, "report").ok_or("report.report")?)?,
            mfr: opt_u16(r, "mfr")?,
        });
    }
    if let Some(c) = get(v, "cmd") {
        let payload = match get(c, "payload") {
            Some(Y::Sequence(b)) => b.iter().map(|x| int(x).map(|i| i as u8)).collect::<R<_>>()?,
            _ => Vec::new(),
        };
        return Ok(ConfigureOp::Cmd {
            ep: ep(c)?,
            cluster: opt_u16(c, "cluster")?.ok_or("cmd.cluster")?,
            id: opt_int(c, "id")?.ok_or("cmd.id")? as u8,
            mfr: opt_u16(c, "mfr")?,
            payload,
        });
    }
    if let Some(g) = get(v, "group") {
        return Ok(ConfigureOp::Group { ep: ep(g)?, group: opt_u16(g, "group")?.ok_or("group.group")? });
    }
    if let Some(c) = get(v, "checkin") {
        return Ok(ConfigureOp::Checkin { interval_qs: opt_int(c, "interval_qs")?.ok_or("interval_qs")? as u32 });
    }
    Err(format!("unknown configure op {v:?}"))
}

fn patch(v: &Y) -> R<EntityPatch> {
    Ok(EntityPatch {
        report: get(v, "report").map(report).transpose()?,
        mul: opt_num(v, "mul")?,
        div: opt_num(v, "div")?,
        offset: opt_num(v, "offset")?,
        precision: opt_int(v, "precision")?.map(|p| p as u8),
        invalid: opt_num(v, "invalid")?,
        min: opt_num(v, "min")?,
        max: opt_num(v, "max")?,
        step: opt_num(v, "step")?,
        name: opt_s(v, "name")?,
        enabled: get(v, "enabled").and_then(Y::as_bool),
        category: opt_s(v, "category")?.and_then(|c| Category::parse(&c)),
        device_class: opt_s(v, "device_class")?,
        unit: opt_s(v, "unit")?,
        invert: get(v, "invert").and_then(Y::as_bool),
    })
}

/// Loads one definition (or fragment) from YAML.
pub fn load_definition(src: &str, source: LayerSource) -> R<Definition> {
    let v: Y = serde_yaml::from_str(src).map_err(|e| e.to_string())?;
    let fragment = get(&v, "zdef_fragment").is_some();
    let id = opt_s(&v, "id")?.unwrap_or_else(|| "self-description".into());
    let mut d = Definition::new(id, if fragment { LayerSource::SelfDescription } else { source });
    d.revision = opt_int(&v, "revision")?.unwrap_or(1) as u32;
    d.vendor = opt_s(&v, "vendor")?.unwrap_or_default();
    d.model = opt_s(&v, "model")?.unwrap_or_default();
    d.priority = opt_int(&v, "priority")?.unwrap_or(0) as i32;
    d.mfr = opt_u16(&v, "mfr")?;
    if let Some(Y::Sequence(m)) = get(&v, "match") {
        d.matches = m.iter().map(match_clause).collect::<R<_>>()?;
    }
    if let Some(Y::Sequence(w)) = get(&v, "white_label") {
        for x in w {
            d.white_label.push(WhiteLabel {
                vendor: opt_s(x, "vendor")?.unwrap_or_default(),
                model: opt_s(x, "model")?.unwrap_or_default(),
                manufacturer: match get(x, "manufacturer") {
                    Some(Y::Sequence(l)) => l.iter().map(s).collect::<R<_>>()?,
                    _ => Vec::new(),
                },
            });
        }
    }
    if let Some(Y::Sequence(r)) = get(&v, "requires") {
        d.requires = r.iter().map(s).collect::<R<_>>()?;
    }
    if let Some(dev) = get(&v, "device") {
        d.device = DeviceFields {
            power: opt_s(dev, "power")?.and_then(|p| PowerKind::parse(&p)),
            sleepy: get(dev, "sleepy").and_then(Y::as_bool),
            battery: match get(dev, "battery") {
                Some(Y::String(n)) => Some(BatteryCurve::Named(n.clone())),
                Some(b) => match get(b, "voltage_to_percent") {
                    Some(Y::Sequence(l)) if l.len() == 2 => Some(BatteryCurve::Linear(num(&l[0])?, num(&l[1])?)),
                    _ => None,
                },
                None => None,
            },
            checkin_s: opt_int(dev, "checkin_s")?.map(|x| x as u32),
            availability_timeout_s: get(dev, "availability").map(|a| opt_int(a, "timeout_s")).transpose()?.flatten().map(|x| x as u64),
            ota: get(dev, "ota").and_then(Y::as_bool),
        };
    }
    if let Some(g) = get(&v, "generic") {
        d.generic.disable = get(g, "disable").and_then(Y::as_bool).unwrap_or(false);
        if let Some(Y::Sequence(x)) = get(g, "exclude") {
            d.generic.exclude = x.iter().map(s).collect::<R<_>>()?;
        }
        if let Some(Y::Sequence(x)) = get(g, "only_endpoints") {
            d.generic.only_endpoints = Some(x.iter().map(|e| int(e).map(|i| i as u8)).collect::<R<_>>()?);
        }
        if let Some(Y::Mapping(p)) = get(g, "patch") {
            for (k, pv) in p {
                d.generic.patch.push((s(k)?, patch(pv)?));
            }
        }
    }
    if let Some(Y::Mapping(eps)) = get(&v, "endpoints") {
        for (k, n) in eps {
            d.endpoints.insert(int(k)? as u8, s(n)?);
        }
    }
    d.self_description = match opt_s(&v, "self_description")?.as_deref() {
        Some("ignore") => SelfDescMode::Ignore,
        Some("under") => SelfDescMode::Under,
        _ => SelfDescMode::Over,
    };
    if let Some(Y::Sequence(c)) = get(&v, "configure") {
        d.configure = c.iter().map(configure_op).collect::<R<_>>()?;
    }
    if let Some(Y::Sequence(c)) = get(&v, "ignore") {
        for x in c {
            d.ignore.push(IgnoreRule {
                ep: opt_int(x, "ep")?.map(|e| e as u8),
                cluster: opt_u16(x, "cluster")?.ok_or("ignore.cluster")?,
                cmd: opt_int(x, "cmd")?.map(|c| c as u8),
            });
        }
    }
    if let Some(Y::Sequence(es)) = get(&v, "entities") {
        d.entities = es.iter().map(entity).collect::<R<_>>()?;
    }
    if let Some(t) = get(&v, "tuya") {
        let mut f = TuyaFlags::default();
        f.time_sync = match get(t, "time_sync") {
            Some(Y::Number(n)) if n.as_i64() == Some(1970) => TimeSync::Epoch1970,
            Some(Y::Number(n)) if n.as_i64() == Some(2000) => TimeSync::Epoch2000,
            Some(Y::String(x)) if x == "1970" => TimeSync::Epoch1970,
            Some(Y::String(x)) if x == "2000" => TimeSync::Epoch2000,
            _ => TimeSync::Off,
        };
        f.force_time_sync_s = opt_int(t, "force_time_sync_s")?.map(|x| x as u32);
        f.magic_packet = get(t, "magic_packet").and_then(Y::as_bool).unwrap_or(false);
        match get(t, "query") {
            Some(Y::Sequence(q)) => {
                for x in q {
                    match s(x)?.as_str() {
                        "configure" => f.query_on_configure = true,
                        "announce" => f.query_on_announce = true,
                        o => return Err(format!("query {o}")),
                    }
                }
            }
            Some(m @ Y::Mapping(_)) => f.query_interval_s = opt_int(m, "interval_s")?.map(|x| x as u32),
            _ => {}
        }
        f.bind_basic = get(t, "bind_basic").and_then(Y::as_bool).unwrap_or(false);
        f.gateway_status = get(t, "gateway_status").and_then(Y::as_bool).unwrap_or(true);
        f.send = if opt_s(t, "send")?.as_deref() == Some("sendData") { TuyaSend::SendData } else { TuyaSend::DataRequest };
        f.mcu_version = get(t, "mcu_version").and_then(Y::as_bool).unwrap_or(false);
        f.optimistic = get(t, "optimistic").and_then(Y::as_bool).unwrap_or(false);
        f.dedup = get(t, "dedup").and_then(Y::as_bool).unwrap_or(true);
        d.tuya = Some(f);
    }
    if let Some(g) = get(&v, "gp") {
        d.gp = Some(GpFlags {
            report_interval_s: opt_int(g, "report_interval_s")?.map(|x| x as u32),
            security_min: opt_int(g, "security_min")?.map(|x| x as u8),
            ignore_commands: clusters(get(g, "ignore_commands"))?.into_iter().map(|c| c as u8).collect(),
        });
    }
    Ok(d)
}

/// Loads `definitions/examples/<name>.yaml` of the repository.
pub fn example(name: &str) -> Definition {
    let path = format!("{}/../../definitions/examples/{name}.yaml", env!("CARGO_MANIFEST_DIR"));
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    load_definition(&src, LayerSource::Catalog).unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_examples_load() {
        let dir = format!("{}/../../definitions/examples", env!("CARGO_MANIFEST_DIR"));
        let mut n = 0;
        for f in std::fs::read_dir(dir).unwrap() {
            let p = f.unwrap().path();
            let src = std::fs::read_to_string(&p).unwrap();
            let d = load_definition(&src, LayerSource::Catalog).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            assert!(!d.model.is_empty());
            n += 1;
        }
        assert_eq!(n, 13);
        let trv = example("tuya-ts0601-trv-me167");
        assert_eq!(trv.entities.len(), 15);
        assert_eq!(trv.entities[0].parts.len(), 4);
        assert!(trv.used_features().contains(&"codec.tuya_schedule_day".to_string()));
    }
}
