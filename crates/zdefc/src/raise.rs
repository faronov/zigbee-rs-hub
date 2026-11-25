//! `zdef` model -> source tree (decompiler, spec 14.4: lossless except comments).

use crate::expr;
use crate::tree::{Key, Node, Value};
use zdef::cbor::{Head, Reader};
use zdef::enums::{self, Feature, TableRef};
use zdef::keys;
use zdef::model::*;

struct R<'d> {
    def: &'d Definition,
    errs: Vec<String>,
}

fn map(entries: Vec<(&str, Node)>) -> Node {
    Node::synth(Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (Key::Str(k.to_string()), Default::default(), v))
            .collect(),
    ))
}

fn flow(mut n: Node) -> Node {
    n.flow = true;
    n
}

fn seq(v: Vec<Node>) -> Node {
    Node::synth(Value::Seq(v))
}

fn hex_list<T: Copy + Into<i64>>(v: &[T], digits: u8) -> Node {
    flow(seq(v
        .iter()
        .map(|x| Node::hex((*x).into(), digits))
        .collect()))
}

fn s(v: &str) -> Node {
    Node::str(v)
}

fn num(n: Num) -> Node {
    match n.normalized() {
        Num::Int(i) => Node::int(i),
        Num::Float(f) => Node::synth(Value::Float(f)),
    }
}

fn scalar(v: &Scalar) -> Node {
    Node::synth(match v {
        Scalar::Null => Value::Null,
        Scalar::Bool(b) => Value::Bool(*b),
        Scalar::Int(i) => Value::Int(*i),
        Scalar::Float(f) => Value::Float(*f),
        Scalar::Str(x) => Value::Str(x.clone()),
    })
}

fn table_name(t: TableRef, table: &[&'static str]) -> Option<&'static str> {
    match t {
        TableRef::Known(c) => table.get(c as usize).copied(),
        TableRef::Unknown(_) => None,
    }
}

impl R<'_> {
    fn expr(&mut self, x: &Expr) -> Node {
        match expr::decompile(x.code(), &self.def.consts) {
            Ok(src) => Node::str(src),
            Err(e) => {
                self.errs.push(e);
                Node::str("null")
            }
        }
    }

    fn name_match(&self, n: &NameMatch) -> Node {
        match n {
            NameMatch::Exact(x) => s(x),
            NameMatch::Any(v) => flow(map(vec![(
                "any",
                flow(seq(v.iter().map(|x| s(x)).collect())),
            )])),
            NameMatch::Prefix(p) => flow(map(vec![("prefix", s(p))])),
        }
    }

    fn range(&self, r: &VersionRange) -> Node {
        let mut e = Vec::new();
        if let Some(v) = r.min {
            e.push(("min", Node::int(v)));
        }
        if let Some(v) = r.max {
            e.push(("max", Node::int(v)));
        }
        flow(map(e))
    }

    fn clause(&mut self, c: &MatchClause) -> Node {
        let mut e = Vec::new();
        if let Some(n) = &c.manufacturer {
            e.push(("manufacturer", self.name_match(n)));
        }
        if let Some(n) = &c.model {
            e.push(("model", self.name_match(n)));
        }
        if let Some(v) = c.mfr_code {
            e.push(("mfr_code", Node::hex(v as i64, 4)));
        }
        for (k, r) in [
            ("app_version", &c.app_version),
            ("stack_version", &c.stack_version),
            ("hw_version", &c.hw_version),
        ] {
            if let Some(r) = r {
                e.push((k, self.range(r)));
            }
        }
        if let Some(n) = &c.sw_build {
            e.push(("sw_build", self.name_match(n)));
        }
        if let Some(n) = &c.date_code {
            e.push(("date_code", self.name_match(n)));
        }
        if let Some(p) = c.power {
            e.push(("power", s(p.as_str())));
        }
        if let Some(p) = c.logical_type {
            e.push(("logical_type", s(p.as_str())));
        }
        if !c.endpoints.is_empty() {
            e.push(("endpoints", endpoints(&c.endpoints)));
        }
        if let Some(g) = &c.gp {
            let mut f = Vec::new();
            if let Some(v) = g.device_id {
                f.push(("device_id", Node::hex(v as i64, 2)));
            }
            if let Some(v) = g.manufacturer_id {
                f.push(("manufacturer_id", Node::hex(v as i64, 4)));
            }
            if let Some(m) = g.model_id {
                f.push((
                    "model_id",
                    match m.mask {
                        None => Node::hex(m.value as i64, 4),
                        Some(mask) => flow(map(vec![
                            ("value", Node::hex(m.value as i64, 4)),
                            ("mask", Node::hex(mask as i64, 4)),
                        ])),
                    },
                ));
            }
            if let Some((lo, hi)) = g.src_id {
                f.push((
                    "src_id",
                    flow(seq(vec![Node::hex(lo as i64, 8), Node::hex(hi as i64, 8)])),
                ));
            }
            if let Some(v) = &g.ieee {
                f.push(("ieee", s(v)));
            }
            if !g.commands.is_empty() {
                f.push(("commands", hex_list(&g.commands, 2)));
            }
            if let Some(cl) = &g.clusters {
                let mut cf = Vec::new();
                if !cl.server.is_empty() {
                    cf.push(("server", hex_list(&cl.server, 4)));
                }
                if !cl.client.is_empty() {
                    cf.push(("client", hex_list(&cl.client, 4)));
                }
                f.push(("clusters", flow(map(cf))));
            }
            e.push(("gp", flow(map(f))));
        }
        self.ext(&mut e, keys::CLAUSE_EXT, &c.ext);
        flow(map(e))
    }

    fn report(&self, r: &Report) -> Node {
        match r {
            Report::Off => s("off"),
            Report::Config { min, max, change } => {
                let mut v = vec![Node::int(*min as i64), Node::int(*max as i64)];
                if let Some(c) = change {
                    v.push(num(*c));
                }
                flow(seq(v))
            }
        }
    }

    fn tuya_dp(&self, t: &TuyaDp) -> Node {
        flow(seq(vec![Node::int(t.dp as i64), s(t.ty.as_str())]))
    }

    fn cmd_ref(&self, c: &CmdRef) -> Node {
        let mut f = Vec::new();
        if let Some(v) = c.ep {
            f.push(("ep", Node::int(v as i64)));
        }
        f.push(("cluster", Node::hex(c.cluster as i64, 4)));
        f.push(("id", Node::hex(c.id as i64, 2)));
        if let Some(v) = c.mfr {
            f.push(("mfr", Node::hex(v as i64, 4)));
        }
        flow(map(f))
    }

    fn write_attr(&self, w: &WriteAttr) -> Node {
        let mut f = Vec::new();
        if let Some(v) = w.ep {
            f.push(("ep", Node::int(v as i64)));
        }
        f.push(("cluster", Node::hex(w.cluster as i64, 4)));
        f.push(("attr", Node::hex(w.attr as i64, 4)));
        f.push(("type", s(enums::zcl_type_name(w.zcl_type).unwrap_or("?"))));
        f.push(("value", scalar(&w.value)));
        if let Some(v) = w.mfr {
            f.push(("mfr", Node::hex(v as i64, 4)));
        }
        flow(map(f))
    }

    fn named(&mut self, v: &[(String, Expr)]) -> Node {
        let mut sorted: Vec<&(String, Expr)> = v.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let entries = sorted
            .into_iter()
            .map(|(n, x)| (Key::Str(n.clone()), Default::default(), self.expr(x)))
            .collect();
        flow(Node::synth(Value::Map(entries)))
    }

    fn entity(&mut self, e: &Entity) -> Node {
        let mut f: Vec<(&str, Node)> = Vec::new();
        let m = e.more();
        if let Some(v) = &e.key {
            f.push(("key", s(v)));
        }
        if let Some(v) = e.platform {
            f.push(("platform", s(v.as_str())));
        }
        if let Some(v) = e
            .device_class
            .and_then(|t| table_name(t, enums::DEVICE_CLASSES))
        {
            f.push(("device_class", s(v)));
        }
        if let Some(v) = e.unit.and_then(|t| table_name(t, enums::UNITS)) {
            f.push(("unit", s(v)));
        }
        if let Some(v) = e.state_class {
            f.push(("state_class", s(v.as_str())));
        }
        if let Some(v) = e.category {
            f.push(("category", s(v.as_str())));
        }
        if let Some(v) = &m.name {
            f.push(("name", s(v)));
        }
        if let Some(v) = e.ep {
            f.push(("ep", Node::int(v as i64)));
        }
        if let Some(z) = &e.zcl {
            let mut b = vec![
                ("cluster", Node::hex(z.cluster as i64, 4)),
                ("attr", Node::hex(z.attr as i64, 4)),
                ("type", s(enums::zcl_type_name(z.zcl_type).unwrap_or("?"))),
            ];
            if let Some(v) = z.mfr {
                b.push(("mfr", Node::hex(v as i64, 4)));
            }
            if let Some(v) = z.ep {
                b.push(("ep", Node::int(v as i64)));
            }
            f.push(("zcl", flow(map(b))));
        }
        if let Some(t) = &e.tuya {
            f.push(("tuya", self.tuya_dp(t)));
        }
        if let Some(st) = &m.struct_ {
            let mut b = vec![
                ("cluster", Node::hex(st.cluster as i64, 4)),
                ("attr", Node::hex(st.attr as i64, 4)),
            ];
            if let Some(v) = st.mfr {
                b.push(("mfr", Node::hex(v as i64, 4)));
            }
            if let Some(v) = st.ep {
                b.push(("ep", Node::int(v as i64)));
            }
            b.push(("codec", s(st.codec.as_str())));
            if let Some(v) = st.tag {
                b.push(("tag", Node::int(v)));
            }
            f.push(("struct", flow(map(b))));
        }
        match &e.gp {
            Some(GpBinding::Cmd(c)) => {
                f.push(("gp", flow(map(vec![("cmd", Node::hex(*c as i64, 2))]))))
            }
            Some(GpBinding::Report {
                cluster,
                attr,
                zcl_type,
                mfr,
            }) => {
                let mut b = Vec::new();
                if let Some(v) = mfr {
                    b.push(("mfr", Node::hex(*v as i64, 4)));
                }
                b.extend([
                    ("cluster", Node::hex(*cluster as i64, 4)),
                    ("attr", Node::hex(*attr as i64, 4)),
                    ("type", s(enums::zcl_type_name(*zcl_type).unwrap_or("?"))),
                ]);
                f.push(("gp", flow(map(vec![("report", flow(map(b)))]))));
            }
            None => {}
        }
        if let Some(x) = &m.present_if {
            f.push(("present_if", self.expr(x)));
        }
        if let Some(x) = &e.from {
            f.push(("from", self.expr(x)));
        }
        if let Some(x) = &m.to {
            f.push(("to", self.expr(x)));
        }
        for (k, v) in [("mul", e.mul), ("div", e.div), ("offset", e.offset)] {
            if let Some(v) = v {
                f.push((k, num(v)));
            }
        }
        if let Some(v) = e.precision {
            f.push(("precision", Node::int(v as i64)));
        }
        if let Some(v) = e.invalid {
            f.push(("invalid", num(v)));
        }
        if !e.map.is_empty() {
            let m = e
                .map
                .iter()
                .map(|(r, l)| (Key::Int(*r), Default::default(), s(l)))
                .collect();
            f.push(("map", flow(Node::synth(Value::Map(m)))));
        }
        if let Some(v) = &m.value_on {
            f.push(("value_on", scalar(v)));
        }
        if let Some(v) = &m.value_off {
            f.push(("value_off", scalar(v)));
        }
        for (k, v) in [("min", e.min), ("max", e.max), ("step", e.step)] {
            if let Some(v) = v {
                f.push((k, num(v)));
            }
        }
        if let Some(v) = m.mode {
            f.push(("mode", s(v.as_str())));
        }
        if let Some(a) = e.access {
            let mut t = String::new();
            for (b, c) in [
                (enums::ACCESS_READ, 'r'),
                (enums::ACCESS_WRITE, 'w'),
                (enums::ACCESS_REPORT, 'p'),
            ] {
                if a & b != 0 {
                    t.push(c);
                }
            }
            f.push(("access", s(&t)));
        }
        if let Some(r) = &e.report {
            f.push(("report", self.report(r)));
        }
        if let Some(v) = e.enabled {
            f.push(("enabled", Node::synth(Value::Bool(v))));
        }
        if let Some(x) = &m.reset_after {
            f.push(("reset_after", self.expr(x)));
        }
        if let Some(v) = m.invert {
            f.push(("invert", Node::synth(Value::Bool(v))));
        }
        if let Some(h) = &m.hub {
            f.push((
                "hub",
                flow(map(h
                    .default
                    .iter()
                    .map(|d| ("default", scalar(d)))
                    .collect())),
            ));
        }
        if let Some(c) = &m.codec {
            f.push((
                "codec",
                match c {
                    Codec::TuyaScheduleDay {
                        day,
                        transitions,
                        variant,
                    } => {
                        let mut b = vec![
                            ("day", Node::int(*day as i64)),
                            ("transitions", Node::int(*transitions as i64)),
                        ];
                        if *variant != ScheduleVariant::MultiDp {
                            b.push(("variant", s(variant.as_str())));
                        }
                        flow(map(vec![("tuya_schedule_day", flow(map(b)))]))
                    }
                    Codec::TuyaScheduleWeek(w) => {
                        let mut b = vec![("variant", s(w.variant.as_str()))];
                        if let Some(v) = w.transitions {
                            b.push(("transitions", Node::int(v as i64)));
                        }
                        if let Some(v) = w.scale {
                            b.push(("scale", Node::int(v as i64)));
                        }
                        if let Some(v) = w.min {
                            b.push(("min", Node::int(v as i64)));
                        }
                        if let Some(v) = w.max {
                            b.push(("max", Node::int(v as i64)));
                        }
                        flow(map(vec![("tuya_schedule_week", flow(map(b)))]))
                    }
                    Codec::BitmapFlags(b) => {
                        let m = b
                            .iter()
                            .map(|(bit, l)| (Key::Int(*bit as i64), Default::default(), s(l)))
                            .collect();
                        flow(map(vec![(
                            "bitmap_flags",
                            flow(Node::synth(Value::Map(m))),
                        )]))
                    }
                    c => s(c.name()),
                },
            ));
        }
        if !m.parts.is_empty() {
            let mut parts = Vec::new();
            for (k, p) in &m.parts {
                let name = keys::sub_name(*k as u64).unwrap_or("?").to_string();
                parts.push((Key::Str(name), Default::default(), self.entity(p)));
            }
            f.push(("parts", Node::synth(Value::Map(parts))));
        }
        if !m.event_types.is_empty() {
            f.push((
                "event_types",
                flow(seq(m.event_types.iter().map(|x| s(x)).collect())),
            ));
        }
        if !m.triggers.is_empty() {
            let mut ts = Vec::new();
            for t in &m.triggers {
                let mut b = Vec::new();
                if let Some(c) = &t.cmd {
                    b.push(("cmd", self.cmd_ref(c)));
                }
                if let Some(c) = t.gp_cmd {
                    b.push(("gp_cmd", Node::hex(c as i64, 2)));
                }
                if let Some(dp) = &t.tuya {
                    b.push(("tuya", self.tuya_dp(dp)));
                }
                if let Some(x) = &t.when {
                    b.push(("when", self.expr(x)));
                }
                b.push(("event", self.expr(&t.event)));
                if !t.attrs.is_empty() {
                    b.push(("attrs", self.named(&t.attrs)));
                }
                ts.push(map(b));
            }
            f.push(("triggers", seq(ts)));
        }
        if let Some(p) = &m.press {
            f.push((
                "press",
                flow(match p {
                    Press::Cmd { cmd, payload } => {
                        let mut b = vec![("cmd", self.cmd_ref(cmd))];
                        if let Some(x) = payload {
                            b.push(("payload", self.expr(x)));
                        }
                        map(b)
                    }
                    Press::Write(w) => map(vec![("write", self.write_attr(w))]),
                    Press::Tuya { dp, value } => {
                        map(vec![("tuya", self.tuya_dp(dp)), ("value", scalar(value))])
                    }
                }),
            ));
        }
        if !m.attrs.is_empty() {
            f.push(("attrs", self.named(&m.attrs)));
        }
        if !m.ext.is_empty() {
            self.errs
                .push("unnamed entity extension keys (64..127) cannot be written as source".into());
        }
        map(f)
    }

    fn configure_op(&mut self, op: &ConfigureOp) -> Node {
        let ep_first = |ep: &Option<u8>, mut rest: Vec<(&'static str, Node)>| {
            if let Some(v) = ep {
                rest.insert(0, ("ep", Node::int(*v as i64)));
            }
            flow(map(rest))
        };
        let op_node = match op {
            ConfigureOp::Bind { ep, clusters } => map(vec![(
                "bind",
                ep_first(ep, vec![("clusters", hex_list(clusters, 4))]),
            )]),
            ConfigureOp::Write(w) => map(vec![("write", self.write_attr(w))]),
            ConfigureOp::Read {
                ep,
                cluster,
                attrs,
                mfr,
            } => {
                let mut b = vec![
                    ("cluster", Node::hex(*cluster as i64, 4)),
                    ("attrs", hex_list(attrs, 4)),
                ];
                if let Some(m) = mfr {
                    b.push(("mfr", Node::hex(*m as i64, 4)));
                }
                map(vec![("read", ep_first(ep, b))])
            }
            ConfigureOp::Report {
                ep,
                cluster,
                attr,
                zcl_type,
                report,
                mfr,
            } => {
                let mut b = vec![
                    ("cluster", Node::hex(*cluster as i64, 4)),
                    ("attr", Node::hex(*attr as i64, 4)),
                    ("type", s(enums::zcl_type_name(*zcl_type).unwrap_or("?"))),
                    ("report", self.report(report)),
                ];
                if let Some(m) = mfr {
                    b.push(("mfr", Node::hex(*m as i64, 4)));
                }
                map(vec![("report", ep_first(ep, b))])
            }
            ConfigureOp::Cmd {
                ep,
                cluster,
                id,
                mfr,
                payload,
            } => {
                let mut c = self.cmd_ref(&CmdRef {
                    cluster: *cluster,
                    id: *id,
                    mfr: *mfr,
                    ep: *ep,
                });
                if let (Some(x), Value::Map(m)) = (payload, &mut c.v) {
                    let pl = self.expr(x);
                    m.push((Key::Str("payload".into()), Default::default(), pl));
                }
                map(vec![("cmd", c)])
            }
            ConfigureOp::Group { ep, group } => map(vec![(
                "group",
                ep_first(ep, vec![("group", Node::hex(*group as i64, 4))]),
            )]),
            ConfigureOp::Checkin { interval_qs } => map(vec![(
                "checkin",
                flow(map(vec![("interval_qs", Node::int(*interval_qs as i64))])),
            )]),
        };
        flow(op_node)
    }

    fn ext(
        &mut self,
        out: &mut Vec<(&'static str, Node)>,
        table: &[(u16, &'static str)],
        ext: &[(u64, Vec<u8>)],
    ) {
        for (k, raw) in ext {
            match table.iter().find(|x| x.0 as u64 == *k) {
                Some((_, name)) => match raw_node(&mut Reader::new(raw)) {
                    Ok(n) => out.push((name, n)),
                    Err(e) => self.errs.push(e),
                },
                None => self.errs.push(format!(
                    "unnamed extension key {k} cannot be written as source"
                )),
            }
        }
    }
}

/// Generic CBOR -> tree (reserved `x-wasm`).
fn raw_node(r: &mut Reader<'_>) -> Result<Node, String> {
    let at = r.position();
    let e = |_| format!("bad CBOR at {at}");
    Ok(Node::synth(match r.peek().map_err(e)? {
        Head::Map(_) => {
            let n = r.map().map_err(e)?;
            let mut m = Vec::new();
            for _ in 0..n {
                let k = r.text().map_err(e)?.to_string();
                m.push((Key::Str(k), Default::default(), raw_node(r)?));
            }
            Value::Map(m)
        }
        Head::Array(_) => {
            let n = r.array().map_err(e)?;
            let mut v = Vec::new();
            for _ in 0..n {
                v.push(raw_node(r)?);
            }
            Value::Seq(v)
        }
        Head::Text(_) => Value::Str(r.text().map_err(e)?.to_string()),
        Head::True | Head::False => Value::Bool(r.bool().map_err(e)?),
        Head::Null => {
            r.head().map_err(e)?;
            Value::Null
        }
        Head::Uint(_) | Head::Nint(_) => Value::Int(r.int().map_err(e)?),
        h @ (Head::F16(_) | Head::F32(_) | Head::F64(_)) => {
            r.head().map_err(e)?;
            Value::Float(r.float_head(h, at).map_err(e)?)
        }
        _ => return Err(format!("unsupported CBOR item at {at}")),
    }))
}

/// Converts a definition to a source tree.
/// `{ep: {profile, device_id, in, out}}` (a clause's or a white label's).
fn endpoints(eps: &[(u8, EpMatch)]) -> Node {
    let eps = eps
        .iter()
        .map(|(ep, m)| {
            let mut f = Vec::new();
            if let Some(v) = m.profile {
                f.push(("profile", Node::hex(v as i64, 4)));
            }
            if let Some(v) = m.device_id {
                f.push(("device_id", Node::hex(v as i64, 4)));
            }
            if !m.in_clusters.is_empty() {
                f.push(("in", hex_list(&m.in_clusters, 4)));
            }
            if !m.out_clusters.is_empty() {
                f.push(("out", hex_list(&m.out_clusters, 4)));
            }
            (Key::Int(*ep as i64), Default::default(), flow(map(f)))
        })
        .collect();
    Node::synth(Value::Map(eps))
}

pub fn raise(def: &Definition) -> Result<Node, Vec<String>> {
    let mut r = R {
        def,
        errs: Vec::new(),
    };
    let mut f: Vec<(&str, Node)> = Vec::new();
    match def.kind {
        Kind::Definition => f.push(("zdef", Node::int(def.major as i64))),
        Kind::Fragment => f.push(("zdef_fragment", Node::int(def.major as i64))),
    }
    if let Some(v) = &def.source {
        f.push(("source", s(v)));
    }
    if let Some(v) = &def.id {
        f.push(("id", s(v)));
    }
    if let Some(v) = def.revision {
        f.push(("revision", Node::int(v as i64)));
    }
    if let Some(v) = &def.vendor {
        f.push(("vendor", s(v)));
    }
    if let Some(v) = &def.model {
        f.push(("model", s(v)));
    }
    if let Some(v) = &def.description {
        f.push(("description", s(v)));
    }
    if let Some(v) = def.mfr {
        f.push(("mfr", Node::hex(v as i64, 4)));
    }
    if !def.match_.is_empty() {
        f.push((
            "match",
            seq(def.match_.iter().map(|c| r.clause(c)).collect()),
        ));
    }
    if !def.white_label.is_empty() {
        let v = def
            .white_label
            .iter()
            .map(|w| {
                let mut b = Vec::new();
                if let Some(x) = &w.vendor {
                    b.push(("vendor", s(x)));
                }
                if let Some(x) = &w.model {
                    b.push(("model", s(x)));
                }
                if !w.manufacturer.is_empty() {
                    b.push((
                        "manufacturer",
                        flow(seq(w.manufacturer.iter().map(|x| s(x)).collect())),
                    ));
                }
                if !w.model_id.is_empty() {
                    b.push((
                        "model_id",
                        flow(seq(w.model_id.iter().map(|x| s(x)).collect())),
                    ));
                }
                if !w.sw_build.is_empty() {
                    b.push((
                        "sw_build",
                        flow(seq(w.sw_build.iter().map(|x| s(x)).collect())),
                    ));
                }
                if let Some(t) = w.logical_type {
                    b.push(("logical_type", s(t.as_str())));
                }
                if !w.hw_version.is_empty() {
                    b.push((
                        "hw_version",
                        flow(seq(w
                            .hw_version
                            .iter()
                            .map(|x| Node::int(*x as i64))
                            .collect())),
                    ));
                }
                if !w.endpoints.is_empty() {
                    b.push(("endpoints", flow(endpoints(&w.endpoints))));
                }
                flow(map(b))
            })
            .collect();
        f.push(("white_label", seq(v)));
    }
    let computed = zdef::validate::compute_requires(def);
    let extra: Vec<Node> = def
        .requires
        .iter()
        .filter(|x| !computed.contains(x))
        .map(|x| {
            s(Feature::from_code(*x as u64)
                .map(|f| f.as_str())
                .unwrap_or("?"))
        })
        .collect();
    if !extra.is_empty() {
        f.push(("requires", flow(seq(extra))));
    }
    if let Some(v) = def.priority {
        f.push(("priority", Node::int(v)));
    }
    if let Some(v) = def.self_description {
        f.push(("self_description", s(v.as_str())));
    }
    if let Some(d) = &def.device {
        let mut b = Vec::new();
        if let Some(p) = d.power {
            b.push(("power", s(p.as_str())));
        }
        if let Some(v) = d.sleepy {
            b.push(("sleepy", Node::synth(Value::Bool(v))));
        }
        match &d.battery {
            Some(Battery::Linear { min_mv, max_mv }) => b.push((
                "battery",
                flow(map(vec![(
                    "voltage_to_percent",
                    flow(seq(vec![
                        Node::int(*min_mv as i64),
                        Node::int(*max_mv as i64),
                    ])),
                )])),
            )),
            Some(Battery::Curve(c)) => b.push(("battery", s(c.as_str()))),
            None => {}
        }
        if let Some(v) = d.checkin_s {
            b.push(("checkin_s", Node::int(v as i64)));
        }
        if let Some(a) = &d.availability {
            b.push((
                "availability",
                flow(map(a
                    .timeout_s
                    .iter()
                    .map(|t| ("timeout_s", Node::int(*t as i64)))
                    .collect())),
            ));
        }
        if let Some(v) = d.ota {
            b.push(("ota", Node::synth(Value::Bool(v))));
        }
        f.push(("device", map(b)));
    }
    if let Some(g) = &def.gp {
        let mut b = Vec::new();
        if let Some(v) = g.report_interval_s {
            b.push(("report_interval_s", Node::int(v as i64)));
        }
        if let Some(v) = g.security_min {
            b.push(("security_min", Node::int(v as i64)));
        }
        if !g.ignore_commands.is_empty() {
            b.push(("ignore_commands", hex_list(&g.ignore_commands, 2)));
        }
        f.push(("gp", map(b)));
    }
    if let Some(t) = &def.tuya {
        let mut b = Vec::new();
        match t.time_sync {
            Some(TimeSync::Off) => b.push(("time_sync", s("off"))),
            Some(TimeSync::Epoch1970) => b.push(("time_sync", Node::int(1970))),
            Some(TimeSync::Epoch2000) => b.push(("time_sync", Node::int(2000))),
            None => {}
        }
        if let Some(v) = t.force_time_sync_s {
            b.push(("force_time_sync_s", Node::int(v as i64)));
        }
        if let Some(v) = t.magic_packet {
            b.push(("magic_packet", Node::synth(Value::Bool(v))));
        }
        if let Some(q) = &t.query {
            let mut v = Vec::new();
            if q.configure {
                v.push(s("configure"));
            }
            if q.announce {
                v.push(s("announce"));
            }
            if let Some(i) = q.interval_s {
                v.push(flow(map(vec![("interval_s", Node::int(i as i64))])));
            }
            b.push(("query", flow(seq(v))));
        }
        for (k, v) in [
            ("bind_basic", t.bind_basic),
            ("gateway_status", t.gateway_status),
            ("mcu_version", t.mcu_version),
            ("optimistic", t.optimistic),
            ("dedup", t.dedup),
        ] {
            if let Some(v) = v {
                b.push((k, Node::synth(Value::Bool(v))));
            }
        }
        if let Some(v) = t.send {
            b.push(("send", s(v.as_str())));
        }
        f.push(("tuya", map(b)));
    }
    if let Some(g) = &def.generic {
        let mut b = Vec::new();
        if let Some(v) = g.disable {
            b.push(("disable", Node::synth(Value::Bool(v))));
        }
        if !g.exclude.is_empty() {
            b.push((
                "exclude",
                flow(seq(g.exclude.iter().map(|x| s(x)).collect())),
            ));
        }
        if !g.only_endpoints.is_empty() {
            b.push((
                "only_endpoints",
                flow(seq(g
                    .only_endpoints
                    .iter()
                    .map(|x| Node::int(*x as i64))
                    .collect())),
            ));
        }
        if !g.patch.is_empty() {
            let p = g
                .patch
                .iter()
                .map(|(k, e)| (Key::Str(k.clone()), Default::default(), flow(r.entity(e))))
                .collect();
            b.push(("patch", Node::synth(Value::Map(p))));
        }
        f.push(("generic", map(b)));
    }
    if !def.endpoints.is_empty() {
        let m = def
            .endpoints
            .iter()
            .map(|(ep, n)| (Key::Int(*ep as i64), Default::default(), s(n)))
            .collect();
        f.push(("endpoints", flow(Node::synth(Value::Map(m)))));
    }
    if !def.configure.is_empty() {
        f.push((
            "configure",
            seq(def.configure.iter().map(|o| r.configure_op(o)).collect()),
        ));
    }
    if !def.entities.is_empty() {
        let v = def.entities.iter().map(|e| r.entity(e)).collect();
        f.push(("entities", seq(v)));
    }
    if !def.ignore.is_empty() {
        let v = def
            .ignore
            .iter()
            .map(|i| {
                let mut b = Vec::new();
                if let Some(e) = i.ep {
                    b.push(("ep", Node::int(e as i64)));
                }
                b.push(("cluster", Node::hex(i.cluster as i64, 4)));
                if let Some(c) = i.cmd {
                    b.push(("cmd", Node::hex(c as i64, 2)));
                }
                flow(map(b))
            })
            .collect();
        f.push(("ignore", seq(v)));
    }
    if !def.templates.is_empty() {
        let mut v = Vec::new();
        for t in &def.templates {
            let mut b = vec![("name", s(t.name.as_str()))];
            if let Some(p) = &t.params {
                match raw_node(&mut Reader::new(p)) {
                    Ok(n) => b.push(("params", n)),
                    Err(e) => r.errs.push(e),
                }
            }
            v.push(map(b));
        }
        f.push(("templates", seq(v)));
    }
    if let Some(x) = &def.x_wasm {
        match raw_node(&mut Reader::new(x)) {
            Ok(n) => f.push(("x-wasm", n)),
            Err(e) => r.errs.push(e),
        }
    }
    let mut ext = Vec::new();
    r.ext(&mut ext, keys::TOP_EXT, &def.ext);
    f.extend(ext);
    if r.errs.is_empty() {
        Ok(map(f))
    } else {
        Err(r.errs)
    }
}
