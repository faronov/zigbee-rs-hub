//! Source tree -> `zdef` model (spec 6, 8).
//!
//! Expressions are compiled in the canonical traversal order, which fixes
//! the constant pool independently of the key order in the source:
//! top level in key-table order (`generic.patch` entries in encoded-key
//! order, then `entities`), and inside an entity: `from`, `to`,
//! `reset_after`, `parts` (part-key order), `event_types`, `triggers`
//! (`attrs` by name, `when`, `event`), `press.payload`, `present_if`,
//! `attrs` (by name).

use crate::expr;
use crate::tree::{Diag, Key, Node, Pos, Value};
use zdef::enums::{self, Category, Feature, Platform, StateClass, TableRef, TuyaType};
use zdef::expr::Site;
use zdef::keys;
use zdef::model::*;

#[derive(Default)]
struct ESrc {
    from: Option<(String, Pos)>,
    to: Option<(String, Pos)>,
    reset_after: Option<(String, Pos)>,
    present_if: Option<(String, Pos)>,
    press_payload: Option<(String, Pos)>,
    attrs: Vec<(String, String, Pos)>,
    parts: Vec<(u16, ESrc)>,
    triggers: Vec<TSrc>,
}

#[derive(Default)]
struct TSrc {
    attrs: Vec<(String, String, Pos)>,
    when: Option<(String, Pos)>,
    event: Option<(String, Pos)>,
}

pub struct Lowered {
    pub def: Definition,
    /// Longest expression in bytes (measurement).
    pub longest_expr: usize,
}

struct L {
    errs: Vec<Diag>,
    /// Source of each `configure` cmd payload, in op order (`None` for
    /// cmd ops without one).
    cmd_payload: Vec<Option<(String, Pos)>>,
}

type Fields<'n> = &'n [(Key, Pos, Node)];

impl L {
    fn e(&mut self, n: &Node, path: &str, m: impl Into<String>) {
        self.errs.push(Diag {
            pos: n.pos,
            path: path.to_string(),
            message: m.into(),
        });
    }

    fn obj<'n>(&mut self, n: &'n Node, path: &str, allowed: &[&str]) -> Option<Fields<'n>> {
        match &n.v {
            Value::Map(m) => {
                for (k, kp, _) in m {
                    let ok = k.as_str().is_some_and(|s| allowed.contains(&s));
                    if !ok {
                        self.errs.push(Diag {
                            pos: *kp,
                            path: path.to_string(),
                            message: format!("unknown field {:?}", k.as_json()),
                        });
                    }
                }
                Some(m)
            }
            _ => {
                self.e(n, path, "expected an object");
                None
            }
        }
    }

    fn int(&mut self, n: &Node, path: &str, min: i64, max: i64) -> Option<i64> {
        let v = match n.v {
            Value::Int(i) => i,
            Value::Float(f) if f == f.trunc() && f.abs() < 9.0e15 => f as i64,
            _ => {
                self.e(n, path, "expected an integer");
                return None;
            }
        };
        if v < min || v > max {
            self.e(n, path, format!("{v} is outside {min}..{max}"));
            return None;
        }
        Some(v)
    }

    fn u8_(&mut self, n: &Node, p: &str) -> Option<u8> {
        self.int(n, p, 0, 0xff).map(|v| v as u8)
    }
    fn u16_(&mut self, n: &Node, p: &str) -> Option<u16> {
        self.int(n, p, 0, 0xffff).map(|v| v as u16)
    }
    fn u32_(&mut self, n: &Node, p: &str) -> Option<u32> {
        self.int(n, p, 0, 0xffff_ffff).map(|v| v as u32)
    }

    fn str_(&mut self, n: &Node, path: &str) -> Option<String> {
        match &n.v {
            Value::Str(s) => Some(s.clone()),
            _ => {
                self.e(n, path, "expected a string");
                None
            }
        }
    }

    fn bool_(&mut self, n: &Node, path: &str) -> Option<bool> {
        match n.v {
            Value::Bool(b) => Some(b),
            _ => {
                self.e(n, path, "expected true or false");
                None
            }
        }
    }

    fn num(&mut self, n: &Node, path: &str) -> Option<Num> {
        match n.v {
            Value::Int(i) => Some(Num::Int(i)),
            Value::Float(f) => Some(Num::Float(f).normalized()),
            _ => {
                self.e(n, path, "expected a number");
                None
            }
        }
    }

    fn scalar(&mut self, n: &Node, path: &str) -> Option<Scalar> {
        Some(match &n.v {
            Value::Null => Scalar::Null,
            Value::Bool(b) => Scalar::Bool(*b),
            Value::Int(i) => Scalar::Int(*i),
            Value::Float(f) => match Num::Float(*f).normalized() {
                Num::Int(i) => Scalar::Int(i),
                Num::Float(f) => Scalar::Float(f),
            },
            Value::Str(s) => Scalar::Str(s.clone()),
            _ => {
                self.e(n, path, "expected a scalar");
                return None;
            }
        })
    }

    fn seq<'n>(&mut self, n: &'n Node, path: &str) -> &'n [Node] {
        match &n.v {
            Value::Seq(v) => {
                if v.is_empty() {
                    self.e(n, path, "empty list (omit the field instead)");
                }
                v
            }
            _ => {
                self.e(n, path, "expected a list");
                &[]
            }
        }
    }

    fn list<T>(
        &mut self,
        n: &Node,
        path: &str,
        mut f: impl FnMut(&mut Self, &Node, &str) -> Option<T>,
    ) -> Vec<T> {
        let items = self.seq(n, path).to_vec();
        let mut out = Vec::new();
        for (i, it) in items.iter().enumerate() {
            if let Some(v) = f(self, it, &format!("{path}/{i}")) {
                out.push(v);
            }
        }
        out
    }

    fn expr_src(&mut self, n: &Node, path: &str) -> Option<(String, Pos)> {
        let s = match &n.v {
            Value::Str(s) => s.clone(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => expr::fmt_float(*f),
            Value::Bool(b) => b.to_string(),
            _ => {
                self.e(n, path, "expected an expression (string)");
                return None;
            }
        };
        Some((s, n.pos))
    }

    fn enum_str<T>(
        &mut self,
        n: &Node,
        path: &str,
        parse: impl Fn(&str) -> Option<T>,
        what: &str,
    ) -> Option<T> {
        let s = self.str_(n, path)?;
        let r = parse(&s);
        if r.is_none() {
            self.e(n, path, format!("unknown {what} {s:?}"));
        }
        r
    }

    fn zcl_type(&mut self, n: &Node, path: &str) -> Option<u8> {
        self.enum_str(n, path, enums::zcl_type_code, "ZCL type")
    }

    fn name_match(&mut self, n: &Node, path: &str) -> Option<NameMatch> {
        match &n.v {
            Value::Str(s) => Some(NameMatch::Exact(s.clone())),
            Value::Map(_) => {
                let f = self.obj(n, path, &["any", "prefix"])?;
                if f.len() != 1 {
                    self.e(n, path, "use exactly one of any, prefix");
                    return None;
                }
                let (k, _, v) = &f[0];
                match k.as_str() {
                    Some("any") => Some(NameMatch::Any(self.list(
                        v,
                        &format!("{path}/any"),
                        |l, n, p| l.str_(n, p),
                    ))),
                    _ => Some(NameMatch::Prefix(self.str_(v, &format!("{path}/prefix"))?)),
                }
            }
            _ => {
                self.e(n, path, "expected a string, {any: [...]} or {prefix: ...}");
                None
            }
        }
    }

    fn range(&mut self, n: &Node, path: &str) -> Option<VersionRange> {
        let f = self.obj(n, path, &["min", "max"])?;
        let mut r = VersionRange::default();
        for (k, _, v) in f {
            match k.as_str() {
                Some("min") => r.min = self.int(v, &format!("{path}/min"), 0, 0xffff_ffff),
                Some("max") => r.max = self.int(v, &format!("{path}/max"), 0, 0xffff_ffff),
                _ => {}
            }
        }
        Some(r)
    }

    fn int_keyed<'n>(
        &mut self,
        n: &'n Node,
        path: &str,
        min: i64,
        max: i64,
    ) -> Vec<(i64, &'n Node)> {
        let mut out: Vec<(i64, &'n Node)> = Vec::new();
        match &n.v {
            Value::Map(m) => {
                for (k, kp, v) in m {
                    match k.as_int() {
                        Some(i) if i >= min && i <= max => {
                            if out.iter().any(|x| x.0 == i) {
                                self.errs.push(Diag {
                                    pos: *kp,
                                    path: path.into(),
                                    message: format!("duplicate key {i}"),
                                });
                            }
                            out.push((i, v));
                        }
                        _ => self.errs.push(Diag {
                            pos: *kp,
                            path: path.into(),
                            message: format!(
                                "key {:?} must be an integer in {min}..{max}",
                                k.as_json()
                            ),
                        }),
                    }
                }
            }
            _ => self.e(n, path, "expected an object"),
        }
        out
    }

    /// `map` keys: integers, or `true`/`false` (lowered to 1/0).
    fn map_keyed<'n>(&mut self, n: &'n Node, path: &str) -> Vec<(i64, &'n Node)> {
        let mut out: Vec<(i64, &'n Node)> = Vec::new();
        let Value::Map(m) = &n.v else {
            self.e(n, path, "expected an object");
            return out;
        };
        for (k, kp, v) in m {
            let raw = k.as_int().or(match k.as_json().as_str() {
                "true" => Some(1),
                "false" => Some(0),
                _ => None,
            });
            match raw {
                Some(i) if out.iter().all(|x| x.0 != i) => out.push((i, v)),
                Some(i) => self.errs.push(Diag {
                    pos: *kp,
                    path: path.into(),
                    message: format!("duplicate key {i}"),
                }),
                None => self.errs.push(Diag {
                    pos: *kp,
                    path: path.into(),
                    message: format!("map key {:?} must be an integer or a boolean", k.as_json()),
                }),
            }
        }
        out
    }

    fn clause(&mut self, n: &Node, path: &str) -> Option<MatchClause> {
        let f = self.obj(
            n,
            path,
            &[
                "manufacturer",
                "model",
                "mfr_code",
                "app_version",
                "stack_version",
                "hw_version",
                "sw_build",
                "date_code",
                "power",
                "logical_type",
                "endpoints",
                "gp",
                "x-priority",
                "x-white-label",
            ],
        )?;
        let mut c = MatchClause::default();
        for (k, _, v) in f {
            let p = format!("{path}/{}", k.as_json());
            let ks = k.as_str().unwrap_or("");
            if let Some(ek) = ext_key(keys::CLAUSE_EXT, ks) {
                c.ext.push((ek, ext_value(v)));
                continue;
            }
            match ks {
                "manufacturer" => c.manufacturer = self.name_match(v, &p),
                "model" => c.model = self.name_match(v, &p),
                "mfr_code" => c.mfr_code = self.u16_(v, &p),
                "app_version" => c.app_version = self.range(v, &p),
                "stack_version" => c.stack_version = self.range(v, &p),
                "hw_version" => c.hw_version = self.range(v, &p),
                "sw_build" => {
                    c.sw_build = self.name_match(v, &p);
                    if matches!(c.sw_build, Some(NameMatch::Any(_))) {
                        self.e(v, &p, "sw_build is a string or {prefix}");
                    }
                }
                "date_code" => {
                    c.date_code = self.name_match(v, &p);
                    if matches!(c.date_code, Some(NameMatch::Any(_))) {
                        self.e(v, &p, "date_code is a string or {prefix}");
                    }
                }
                "power" => {
                    c.power = self.enum_str(
                        v,
                        &p,
                        |s| Power::parse(s).filter(|x| *x != Power::EnergyHarvesting),
                        "power",
                    )
                }
                "logical_type" => {
                    c.logical_type = self.enum_str(v, &p, LogicalType::parse, "logical type")
                }
                "endpoints" => c.endpoints = self.endpoints(v, &p),
                "gp" => c.gp = self.gp_match(v, &p),
                _ => {}
            }
        }
        Some(c)
    }

    /// `{ep: {profile, device_id, in, out}}` (a clause's or a white label's).
    fn endpoints(&mut self, v: &Node, p: &str) -> Vec<(u8, EpMatch)> {
        let mut out = Vec::new();
        for (ep, en) in self.int_keyed(v, p, 1, 254) {
            let ep_p = format!("{p}/{ep}");
            let Some(ef) = self.obj(en, &ep_p, &["profile", "device_id", "in", "out"]) else {
                continue;
            };
            let mut m = EpMatch::default();
            for (k2, _, v2) in ef {
                let p2 = format!("{ep_p}/{}", k2.as_json());
                match k2.as_str().unwrap_or("") {
                    "profile" => m.profile = self.u16_(v2, &p2),
                    "device_id" => m.device_id = self.u16_(v2, &p2),
                    "in" => m.in_clusters = self.list(v2, &p2, |l, n, p| l.u16_(n, p)),
                    "out" => m.out_clusters = self.list(v2, &p2, |l, n, p| l.u16_(n, p)),
                    _ => {}
                }
            }
            out.push((ep as u8, m));
        }
        out.sort_by_key(|e| e.0);
        out
    }

    fn gp_match(&mut self, n: &Node, path: &str) -> Option<GpMatch> {
        let f = self.obj(
            n,
            path,
            &[
                "device_id",
                "manufacturer_id",
                "model_id",
                "src_id",
                "ieee",
                "commands",
                "clusters",
            ],
        )?;
        let mut g = GpMatch::default();
        for (k, _, v) in f {
            let p = format!("{path}/{}", k.as_json());
            match k.as_str().unwrap_or("") {
                "device_id" => g.device_id = self.u8_(v, &p),
                "manufacturer_id" => g.manufacturer_id = self.u16_(v, &p),
                "model_id" => {
                    g.model_id = match &v.v {
                        Value::Map(_) => {
                            let mf = self.obj(v, &p, &["value", "mask"])?;
                            let (mut val, mut mask) = (None, None);
                            for (k2, _, v2) in mf {
                                match k2.as_str().unwrap_or("") {
                                    "value" => val = self.u16_(v2, &format!("{p}/value")),
                                    _ => mask = self.u16_(v2, &format!("{p}/mask")),
                                }
                            }
                            match (val, mask) {
                                (Some(value), Some(0xffff)) => Some(ModelId { value, mask: None }),
                                (Some(value), Some(m)) => Some(ModelId {
                                    value,
                                    mask: Some(m),
                                }),
                                _ => {
                                    self.e(v, &p, "model_id needs value and mask");
                                    None
                                }
                            }
                        }
                        _ => self.u16_(v, &p).map(|value| ModelId { value, mask: None }),
                    }
                }
                "src_id" => {
                    let r = self.list(v, &p, |l, n, p| l.u32_(n, p));
                    if r.len() == 2 && r[0] <= r[1] {
                        g.src_id = Some((r[0], r[1]));
                    } else {
                        self.e(v, &p, "src_id is [lo, hi] with lo <= hi");
                    }
                }
                "ieee" => {
                    let s = self
                        .str_(v, &p)
                        .map(|s| s.to_ascii_lowercase().replace(':', ""));
                    if let Some(s) = &s {
                        if s.is_empty() || s.len() > 16 || !s.bytes().all(|b| b.is_ascii_hexdigit())
                        {
                            self.e(v, &p, "ieee is a hex prefix of up to 16 digits");
                        }
                    }
                    g.ieee = s;
                }
                "commands" => g.commands = self.list(v, &p, |l, n, p| l.u8_(n, p)),
                "clusters" => {
                    let cf = self.obj(v, &p, &["server", "client"])?;
                    let mut cl = GpClusters::default();
                    for (k2, _, v2) in cf {
                        let p2 = format!("{p}/{}", k2.as_json());
                        match k2.as_str().unwrap_or("") {
                            "server" => cl.server = self.list(v2, &p2, |l, n, p| l.u16_(n, p)),
                            _ => cl.client = self.list(v2, &p2, |l, n, p| l.u16_(n, p)),
                        }
                    }
                    g.clusters = Some(cl);
                }
                _ => {}
            }
        }
        Some(g)
    }

    /// `query`: a list of `configure`, `announce` and `{interval_s: N}`
    /// (a bare `{interval_s: N}` is the one-item list).
    fn tuya_query(&mut self, n: &Node, path: &str) -> Option<TuyaQuery> {
        let mut q = TuyaQuery {
            configure: false,
            announce: false,
            interval_s: None,
        };
        let one = |l: &mut Self, q: &mut TuyaQuery, n: &Node, p: &str| match &n.v {
            Value::Str(s) if s == "configure" && !q.configure => q.configure = true,
            Value::Str(s) if s == "announce" && !q.announce => q.announce = true,
            Value::Map(_) if q.interval_s.is_none() => {
                let f = l.obj(n, p, &["interval_s"]);
                q.interval_s = f
                    .and_then(|f| f.first())
                    .map(|x| x.2.clone())
                    .and_then(|x| l.int(&x, &format!("{p}/interval_s"), 1, 0xffff_ffff))
                    .map(|x| x as u32);
            }
            _ => l.e(
                n,
                p,
                "query items are configure, announce and {interval_s} (once each)",
            ),
        };
        match &n.v {
            Value::Map(_) => one(self, &mut q, n, path),
            _ => {
                let items = self.seq(n, path).to_vec();
                for (i, it) in items.iter().enumerate() {
                    one(self, &mut q, it, &format!("{path}/{i}"));
                }
            }
        }
        Some(q)
    }

    fn report(&mut self, n: &Node, path: &str) -> Option<Report> {
        match &n.v {
            Value::Str(s) if s == "off" => Some(Report::Off),
            Value::Seq(v) if v.len() == 2 || v.len() == 3 => {
                let min = self.u16_(&v[0], &format!("{path}/0"))?;
                let max = self.u16_(&v[1], &format!("{path}/1"))?;
                let change = if v.len() == 3 {
                    Some(self.num(&v[2], &format!("{path}/2"))?)
                } else {
                    None
                };
                Some(Report::Config { min, max, change })
            }
            _ => {
                self.e(n, path, "report is [min, max, change] or \"off\"");
                None
            }
        }
    }

    fn tuya_dp(&mut self, n: &Node, path: &str) -> Option<TuyaDp> {
        match &n.v {
            Value::Seq(v) if v.len() == 2 => {
                let dp = self.u8_(&v[0], &format!("{path}/0"))?;
                let ty =
                    self.enum_str(&v[1], &format!("{path}/1"), TuyaType::parse, "Tuya DP type")?;
                Some(TuyaDp { dp, ty })
            }
            _ => {
                self.e(n, path, "tuya is [dp, type]");
                None
            }
        }
    }

    fn cmd_ref(&mut self, n: &Node, path: &str) -> Option<CmdRef> {
        let f = self.obj(n, path, &["cluster", "id", "mfr", "ep"])?;
        let (mut cluster, mut id, mut mfr, mut ep) = (None, None, None, None);
        for (k, _, v) in f {
            let p = format!("{path}/{}", k.as_json());
            match k.as_str().unwrap_or("") {
                "cluster" => cluster = self.u16_(v, &p),
                "id" => id = self.u8_(v, &p),
                "mfr" => mfr = self.u16_(v, &p),
                _ => ep = self.u8_(v, &p),
            }
        }
        match (cluster, id) {
            (Some(cluster), Some(id)) => Some(CmdRef {
                cluster,
                id,
                mfr,
                ep,
            }),
            _ => {
                self.e(n, path, "cmd needs cluster and id");
                None
            }
        }
    }

    fn write_attr(&mut self, n: &Node, path: &str) -> Option<WriteAttr> {
        let f = self.obj(n, path, &["cluster", "attr", "type", "value", "mfr", "ep"])?;
        let (mut cluster, mut attr, mut ty, mut value, mut mfr, mut ep) =
            (None, None, None, None, None, None);
        for (k, _, v) in f {
            let p = format!("{path}/{}", k.as_json());
            match k.as_str().unwrap_or("") {
                "cluster" => cluster = self.u16_(v, &p),
                "attr" => attr = self.u16_(v, &p),
                "type" => ty = self.zcl_type(v, &p),
                "value" => value = self.scalar(v, &p),
                "mfr" => mfr = self.u16_(v, &p),
                _ => ep = self.u8_(v, &p),
            }
        }
        match (cluster, attr, ty, value) {
            (Some(cluster), Some(attr), Some(zcl_type), Some(value)) => Some(WriteAttr {
                ep,
                cluster,
                attr,
                zcl_type,
                value,
                mfr,
            }),
            _ => {
                self.e(n, path, "write needs cluster, attr, type and value");
                None
            }
        }
    }

    fn configure_op(&mut self, n: &Node, path: &str) -> Option<ConfigureOp> {
        let f = self.obj(
            n,
            path,
            &["bind", "write", "read", "report", "cmd", "group", "checkin"],
        )?;
        if f.len() != 1 {
            self.e(n, path, "a configure op has exactly one kind");
            return None;
        }
        let (k, _, v) = &f[0];
        let kind = k.as_str().unwrap_or("");
        let p = format!("{path}/{kind}");
        let get = |f: Fields<'_>, name: &str| {
            f.iter()
                .find(|e| e.0.as_str() == Some(name))
                .map(|e| e.2.clone())
        };
        Some(match kind {
            "write" => ConfigureOp::Write(self.write_attr(v, &p)?),
            "cmd" => {
                let payload = match &v.v {
                    Value::Map(m) => m
                        .iter()
                        .position(|x| x.0.as_str() == Some("payload"))
                        .map(|i| m[i].2.clone()),
                    _ => None,
                };
                let mut bare = v.clone();
                if let Value::Map(m) = &mut bare.v {
                    m.retain(|x| x.0.as_str() != Some("payload"));
                }
                let c = self.cmd_ref(&bare, &p)?;
                let src = payload.and_then(|pl| self.expr_src(&pl, &format!("{p}/payload")));
                self.cmd_payload.push(src);
                ConfigureOp::Cmd {
                    ep: c.ep,
                    cluster: c.cluster,
                    id: c.id,
                    mfr: c.mfr,
                    payload: None,
                }
            }
            "bind" => {
                let of = self.obj(v, &p, &["ep", "clusters"])?;
                let ep = get(of, "ep").and_then(|n| self.u8_(&n, &format!("{p}/ep")));
                let clusters = get(of, "clusters")
                    .map(|n| self.list(&n, &format!("{p}/clusters"), |l, n, p| l.u16_(n, p)))
                    .unwrap_or_default();
                if clusters.is_empty() {
                    self.e(v, &p, "bind needs clusters");
                }
                ConfigureOp::Bind { ep, clusters }
            }
            "read" => {
                let of = self.obj(v, &p, &["ep", "cluster", "attrs", "mfr"])?;
                let ep = get(of, "ep").and_then(|n| self.u8_(&n, &format!("{p}/ep")));
                let mfr = get(of, "mfr").and_then(|n| self.u16_(&n, &format!("{p}/mfr")));
                let cluster =
                    get(of, "cluster").and_then(|n| self.u16_(&n, &format!("{p}/cluster")));
                let attrs = get(of, "attrs")
                    .map(|n| self.list(&n, &format!("{p}/attrs"), |l, n, p| l.u16_(n, p)))
                    .unwrap_or_default();
                let Some(cluster) = cluster else {
                    self.e(v, &p, "read needs cluster");
                    return None;
                };
                if attrs.is_empty() {
                    self.e(v, &p, "read needs attrs");
                }
                ConfigureOp::Read {
                    ep,
                    cluster,
                    attrs,
                    mfr,
                }
            }
            "report" => {
                let of = self.obj(v, &p, &["ep", "cluster", "attr", "type", "report", "mfr"])?;
                let ep = get(of, "ep").and_then(|n| self.u8_(&n, &format!("{p}/ep")));
                let mfr = get(of, "mfr").and_then(|n| self.u16_(&n, &format!("{p}/mfr")));
                let cluster =
                    get(of, "cluster").and_then(|n| self.u16_(&n, &format!("{p}/cluster")));
                let attr = get(of, "attr").and_then(|n| self.u16_(&n, &format!("{p}/attr")));
                let ty = get(of, "type").and_then(|n| self.zcl_type(&n, &format!("{p}/type")));
                let rep = get(of, "report").and_then(|n| self.report(&n, &format!("{p}/report")));
                match (cluster, attr, ty, rep) {
                    (Some(cluster), Some(attr), Some(zcl_type), Some(report)) => {
                        ConfigureOp::Report {
                            ep,
                            cluster,
                            attr,
                            zcl_type,
                            report,
                            mfr,
                        }
                    }
                    _ => {
                        self.e(v, &p, "report needs cluster, attr, type and report");
                        return None;
                    }
                }
            }
            "group" => {
                let of = self.obj(v, &p, &["ep", "group"])?;
                let ep = get(of, "ep").and_then(|n| self.u8_(&n, &format!("{p}/ep")));
                let Some(group) =
                    get(of, "group").and_then(|n| self.u16_(&n, &format!("{p}/group")))
                else {
                    self.e(v, &p, "group needs group");
                    return None;
                };
                ConfigureOp::Group { ep, group }
            }
            _ => {
                let of = self.obj(v, &p, &["interval_qs"])?;
                let Some(interval_qs) =
                    get(of, "interval_qs").and_then(|n| self.u32_(&n, &format!("{p}/interval_qs")))
                else {
                    self.e(v, &p, "checkin needs interval_qs");
                    return None;
                };
                ConfigureOp::Checkin { interval_qs }
            }
        })
    }

    fn codec(&mut self, n: &Node, path: &str) -> Option<Codec> {
        match &n.v {
            Value::Str(s) => Some(match CodecName::parse(s) {
                Some(CodecName::ZclTagged) => Codec::ZclTagged,
                Some(CodecName::BcdTime) => Codec::BcdTime,
                Some(CodecName::Hhmm) => Codec::Hhmm,
                Some(CodecName::Ascii) => Codec::Ascii,
                Some(CodecName::Utf16beHex) => Codec::Utf16beHex,
                _ => {
                    self.e(
                        n,
                        path,
                        format!("unknown codec {s:?} (or it needs parameters)"),
                    );
                    return None;
                }
            }),
            Value::Map(_) => {
                let f = self.obj(
                    n,
                    path,
                    &["tuya_schedule_day", "tuya_schedule_week", "bitmap_flags"],
                )?;
                if f.len() != 1 {
                    self.e(n, path, "one codec per entity");
                    return None;
                }
                let (k, _, v) = &f[0];
                let p = format!("{path}/{}", k.as_json());
                let variant = |l: &mut Self, n: &Node, p: &str| {
                    l.enum_str(n, p, ScheduleVariant::parse, "schedule variant")
                };
                match k.as_str().unwrap_or("") {
                    "tuya_schedule_day" => {
                        let pf = self.obj(v, &p, &["day", "transitions", "variant"])?;
                        let (mut day, mut tr, mut var) = (None, None, ScheduleVariant::MultiDp);
                        for (k2, _, v2) in pf {
                            let p2 = format!("{p}/{}", k2.as_json());
                            match k2.as_str().unwrap_or("") {
                                "day" => day = self.int(v2, &p2, 1, 7).map(|x| x as u8),
                                "variant" => var = variant(self, v2, &p2)?,
                                _ => tr = self.int(v2, &p2, 1, 10).map(|x| x as u8),
                            }
                        }
                        match (day, tr) {
                            (Some(day), Some(transitions)) => Some(Codec::TuyaScheduleDay {
                                day,
                                transitions,
                                variant: var,
                            }),
                            _ => {
                                self.e(v, &p, "needs day and transitions");
                                None
                            }
                        }
                    }
                    "tuya_schedule_week" => {
                        let pf =
                            self.obj(v, &p, &["variant", "transitions", "scale", "min", "max"])?;
                        let mut w = ScheduleWeek {
                            variant: ScheduleVariant::MultiDp,
                            transitions: None,
                            scale: None,
                            min: None,
                            max: None,
                        };
                        let mut has_variant = false;
                        for (k2, _, v2) in pf {
                            let p2 = format!("{p}/{}", k2.as_json());
                            match k2.as_str().unwrap_or("") {
                                "variant" => {
                                    w.variant = variant(self, v2, &p2)?;
                                    has_variant = true;
                                }
                                "transitions" => {
                                    w.transitions = self.int(v2, &p2, 1, 10).map(|x| x as u8)
                                }
                                "scale" => w.scale = self.int(v2, &p2, 1, 1000).map(|x| x as u16),
                                "min" => w.min = self.int(v2, &p2, -1000, 1000).map(|x| x as i16),
                                _ => w.max = self.int(v2, &p2, -1000, 1000).map(|x| x as i16),
                            }
                        }
                        if !has_variant {
                            self.e(v, &p, "tuya_schedule_week needs variant");
                            return None;
                        }
                        Some(Codec::TuyaScheduleWeek(w))
                    }
                    _ => {
                        let mut bits: Vec<(u8, String)> = Vec::new();
                        for (b, ln) in self.int_keyed(v, &p, 0, 63) {
                            if let Some(s) = self.str_(ln, &format!("{p}/{b}")) {
                                bits.push((b as u8, s));
                            }
                        }
                        bits.sort_by_key(|x| x.0);
                        Some(Codec::BitmapFlags(bits))
                    }
                }
            }
            _ => {
                self.e(n, path, "codec is a name or {name: parameters}");
                None
            }
        }
    }

    fn named_exprs(&mut self, n: &Node, path: &str) -> Vec<(String, String, Pos)> {
        let mut out = Vec::new();
        if let Value::Map(m) = &n.v {
            for (k, kp, v) in m {
                let Some(name) = k.as_str() else {
                    self.errs.push(Diag {
                        pos: *kp,
                        path: path.into(),
                        message: "attribute names are strings".into(),
                    });
                    continue;
                };
                if let Some((s, p)) = self.expr_src(v, &format!("{path}/{name}")) {
                    out.push((name.to_string(), s, p));
                }
            }
            if m.is_empty() {
                self.e(n, path, "empty map (omit the field instead)");
            }
        } else {
            self.e(n, path, "expected an object of name: expression");
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn entity(&mut self, n: &Node, path: &str, full: bool) -> Option<(Entity, ESrc)> {
        const FIELDS: &[&str] = &[
            "key",
            "platform",
            "device_class",
            "unit",
            "state_class",
            "category",
            "name",
            "ep",
            "zcl",
            "tuya",
            "struct",
            "gp",
            "from",
            "to",
            "mul",
            "div",
            "offset",
            "precision",
            "invalid",
            "map",
            "value_on",
            "value_off",
            "min",
            "max",
            "step",
            "mode",
            "access",
            "report",
            "enabled",
            "reset_after",
            "parts",
            "event_types",
            "triggers",
            "press",
            "codec",
            "present_if",
            "attrs",
            "hub",
            "invert",
        ];
        let f = self.obj(n, path, FIELDS)?;
        let mut e = Entity::default();
        let mut s = ESrc::default();
        for (k, _, v) in f {
            let key = k.as_str().unwrap_or("");
            let p = format!("{path}/{key}");
            match key {
                "key" => e.key = self.str_(v, &p),
                "platform" => e.platform = self.enum_str(v, &p, Platform::parse, "platform"),
                "device_class" => {
                    e.device_class = self
                        .enum_str(v, &p, enums::device_class_code, "device class")
                        .map(TableRef::Known)
                }
                "unit" => {
                    e.unit = self
                        .enum_str(v, &p, enums::unit_code, "unit")
                        .map(TableRef::Known)
                }
                "state_class" => {
                    e.state_class = self.enum_str(v, &p, StateClass::parse, "state class")
                }
                "category" => e.category = self.enum_str(v, &p, Category::parse, "category"),
                "name" => e.more_mut().name = self.str_(v, &p),
                "ep" => e.ep = self.int(v, &p, 1, 254).map(|x| x as u8),
                "zcl" => {
                    let zf = self.obj(v, &p, &["cluster", "attr", "type", "mfr", "ep"])?;
                    let (mut cl, mut at, mut ty, mut mfr, mut ep) = (None, None, None, None, None);
                    for (k2, _, v2) in zf {
                        let p2 = format!("{p}/{}", k2.as_json());
                        match k2.as_str().unwrap_or("") {
                            "cluster" => cl = self.u16_(v2, &p2),
                            "attr" => at = self.u16_(v2, &p2),
                            "type" => ty = self.zcl_type(v2, &p2),
                            "mfr" => mfr = self.u16_(v2, &p2),
                            _ => ep = self.u8_(v2, &p2),
                        }
                    }
                    match (cl, at, ty) {
                        (Some(cluster), Some(attr), Some(zcl_type)) => {
                            e.zcl = Some(ZclBinding {
                                cluster,
                                attr,
                                zcl_type,
                                mfr,
                                ep,
                            })
                        }
                        _ => self.e(v, &p, "zcl needs cluster, attr and type"),
                    }
                }
                "tuya" => e.tuya = self.tuya_dp(v, &p),
                "struct" => {
                    let sf = self.obj(v, &p, &["cluster", "attr", "mfr", "ep", "codec", "tag"])?;
                    let (mut cl, mut at, mut mfr, mut ep, mut codec, mut tag) =
                        (None, None, None, None, None, None);
                    for (k2, _, v2) in sf {
                        let p2 = format!("{p}/{}", k2.as_json());
                        match k2.as_str().unwrap_or("") {
                            "cluster" => cl = self.u16_(v2, &p2),
                            "attr" => at = self.u16_(v2, &p2),
                            "mfr" => mfr = self.u16_(v2, &p2),
                            "ep" => ep = self.u8_(v2, &p2),
                            "codec" => {
                                codec = self.enum_str(
                                    v2,
                                    &p2,
                                    |s| CodecName::parse(s).filter(|c| STRUCT_CODECS.contains(c)),
                                    "struct codec",
                                )
                            }
                            _ => tag = self.int(v2, &p2, i64::MIN, i64::MAX),
                        }
                    }
                    match (cl, at, codec) {
                        (Some(cluster), Some(attr), Some(codec)) => {
                            e.more_mut().struct_ = Some(StructBinding {
                                cluster,
                                attr,
                                mfr,
                                ep,
                                codec,
                                tag,
                            })
                        }
                        _ => self.e(v, &p, "struct needs cluster, attr and codec"),
                    }
                }
                "gp" => {
                    let gf = self.obj(v, &p, &["report", "cmd"])?;
                    if gf.len() != 1 {
                        self.e(v, &p, "gp is {report: ...} or {cmd: ...}");
                        continue;
                    }
                    let (k2, _, v2) = &gf[0];
                    if k2.as_str() == Some("cmd") {
                        e.gp = self.u8_(v2, &format!("{p}/cmd")).map(GpBinding::Cmd);
                    } else {
                        let p2 = format!("{p}/report");
                        let rf = self.obj(v2, &p2, &["cluster", "attr", "type", "mfr"])?;
                        let (mut cl, mut at, mut ty, mut mfr) = (None, None, None, None);
                        for (k3, _, v3) in rf {
                            let p3 = format!("{p2}/{}", k3.as_json());
                            match k3.as_str().unwrap_or("") {
                                "cluster" => cl = self.u16_(v3, &p3),
                                "attr" => at = self.u16_(v3, &p3),
                                "type" => ty = self.zcl_type(v3, &p3),
                                _ => mfr = self.u16_(v3, &p3),
                            }
                        }
                        match (cl, at, ty) {
                            (Some(cluster), Some(attr), Some(zcl_type)) => {
                                e.gp = Some(GpBinding::Report {
                                    cluster,
                                    attr,
                                    zcl_type,
                                    mfr,
                                })
                            }
                            _ => self.e(v2, &p2, "gp.report needs cluster, attr and type"),
                        }
                    }
                }
                "from" => s.from = self.expr_src(v, &p),
                "to" => s.to = self.expr_src(v, &p),
                "reset_after" => s.reset_after = self.expr_src(v, &p),
                "present_if" => s.present_if = self.expr_src(v, &p),
                "mul" => e.mul = self.num(v, &p),
                "div" => {
                    e.div = self.num(v, &p);
                    if e.div.is_some_and(|d| d.as_f64() == 0.0) {
                        self.e(v, &p, "div must not be 0");
                    }
                }
                "offset" => e.offset = self.num(v, &p),
                "precision" => e.precision = self.int(v, &p, 0, 15).map(|x| x as u8),
                "invalid" => e.invalid = self.num(v, &p),
                "map" => {
                    for (raw, ln) in self.map_keyed(v, &p) {
                        if let Some(l) = self.str_(ln, &format!("{p}/{raw}")) {
                            e.map.push((raw, l));
                        }
                    }
                    if e.map.is_empty() {
                        self.e(v, &p, "empty map");
                    }
                }
                "value_on" => e.more_mut().value_on = self.scalar(v, &p),
                "value_off" => e.more_mut().value_off = self.scalar(v, &p),
                "min" => e.min = self.num(v, &p),
                "max" => e.max = self.num(v, &p),
                "step" => e.step = self.num(v, &p),
                "mode" => e.more_mut().mode = self.enum_str(v, &p, Mode::parse, "mode"),
                "access" => {
                    if let Some(a) = self.str_(v, &p) {
                        let mut bits = 0u8;
                        for c in a.chars() {
                            let b = match c {
                                'r' => enums::ACCESS_READ,
                                'w' => enums::ACCESS_WRITE,
                                'p' => enums::ACCESS_REPORT,
                                _ => 0,
                            };
                            if b == 0 || bits & b != 0 {
                                self.e(v, &p, "access is a combination of r, w, p");
                            }
                            bits |= b;
                        }
                        e.access = Some(bits);
                    }
                }
                "report" => e.report = self.report(v, &p),
                "enabled" => e.enabled = self.bool_(v, &p),
                "invert" => e.more_mut().invert = self.bool_(v, &p),
                "parts" => {
                    if let Some(pf) = self.obj(v, &p, keys::PART_NAMES) {
                        for (k2, _, v2) in pf {
                            let name = k2.as_str().unwrap_or("");
                            let Some(pk) = keys::sub_key(name) else {
                                continue;
                            };
                            if let Some((pe, ps)) = self.entity(v2, &format!("{p}/{name}"), false) {
                                e.more_mut().parts.push((pk, pe));
                                s.parts.push((pk, ps));
                            }
                        }
                    }
                    e.more_mut().parts.sort_by_key(|x| x.0);
                    s.parts.sort_by_key(|x| x.0);
                }
                "event_types" => {
                    e.more_mut().event_types = self.list(v, &p, |l, n, p| l.str_(n, p))
                }
                "triggers" => {
                    let items = self.seq(v, &p).to_vec();
                    for (i, t) in items.iter().enumerate() {
                        let tp = format!("{p}/{i}");
                        let Some(tf) =
                            self.obj(t, &tp, &["cmd", "gp_cmd", "tuya", "when", "event", "attrs"])
                        else {
                            continue;
                        };
                        let mut tr = Trigger {
                            attrs: Vec::new(),
                            cmd: None,
                            gp_cmd: None,
                            tuya: None,
                            when: None,
                            event: Expr::default(),
                        };
                        let mut ts = TSrc::default();
                        for (k2, _, v2) in tf {
                            let p2 = format!("{tp}/{}", k2.as_json());
                            match k2.as_str().unwrap_or("") {
                                "cmd" => tr.cmd = self.cmd_ref(v2, &p2),
                                "gp_cmd" => tr.gp_cmd = self.u8_(v2, &p2),
                                "tuya" => tr.tuya = self.tuya_dp(v2, &p2),
                                "when" => ts.when = self.expr_src(v2, &p2),
                                "event" => ts.event = self.expr_src(v2, &p2),
                                _ => ts.attrs = self.named_exprs(v2, &p2),
                            }
                        }
                        if ts.event.is_none() {
                            self.e(t, &tp, "trigger needs event");
                        }
                        e.more_mut().triggers.push(tr);
                        s.triggers.push(ts);
                    }
                }
                "press" => {
                    let Some(pf) = self.obj(v, &p, &["cmd", "payload", "write", "tuya", "value"])
                    else {
                        continue;
                    };
                    let get = |name: &str| {
                        pf.iter()
                            .find(|x| x.0.as_str() == Some(name))
                            .map(|x| x.2.clone())
                    };
                    if let Some(c) = get("cmd") {
                        if let Some(cmd) = self.cmd_ref(&c, &format!("{p}/cmd")) {
                            e.more_mut().press = Some(Press::Cmd { cmd, payload: None });
                        }
                        if let Some(pl) = get("payload") {
                            s.press_payload = self.expr_src(&pl, &format!("{p}/payload"));
                        }
                        if pf.len() != 1 + get("payload").is_some() as usize {
                            self.e(v, &p, "press is {cmd, payload?}, {write} or {tuya, value}");
                        }
                    } else if let Some(w) = get("write") {
                        if pf.len() != 1 {
                            self.e(v, &p, "press is {cmd, payload?}, {write} or {tuya, value}");
                        }
                        e.more_mut().press =
                            self.write_attr(&w, &format!("{p}/write")).map(Press::Write);
                    } else if let (Some(t), Some(val)) = (get("tuya"), get("value")) {
                        if pf.len() != 2 {
                            self.e(v, &p, "press is {cmd, payload?}, {write} or {tuya, value}");
                        }
                        if let (Some(dp), Some(value)) = (
                            self.tuya_dp(&t, &format!("{p}/tuya")),
                            self.scalar(&val, &format!("{p}/value")),
                        ) {
                            e.more_mut().press = Some(Press::Tuya { dp, value });
                        }
                    } else {
                        self.e(v, &p, "press is {cmd, payload?}, {write} or {tuya, value}");
                    }
                }
                "codec" => e.more_mut().codec = self.codec(v, &p),
                "attrs" => s.attrs = self.named_exprs(v, &p),
                "hub" => {
                    let Some(hf) = self.obj(v, &p, &["default"]) else {
                        continue;
                    };
                    let default = hf
                        .first()
                        .and_then(|x| self.scalar(&x.2.clone(), &format!("{p}/default")));
                    e.more_mut().hub = Some(HubBinding { default });
                }
                _ => {}
            }
        }
        e.compact();
        if full && (e.key.is_none() || e.platform.is_none()) {
            self.e(n, path, "an entity needs key and platform");
        }
        if !full && (e.key.is_some() || e.platform.is_some()) && path.contains("/parts/") {
            self.e(n, path, "parts have no key or platform");
        }
        Some((e, s))
    }
}

struct Fill<'p> {
    pool: &'p mut Vec<Const>,
    gp: bool,
    errs: Vec<Diag>,
    longest: usize,
}

impl Fill<'_> {
    fn one(&mut self, e: &Entity, site: Site, src: &(String, Pos), path: &str) -> Option<Expr> {
        let ctx = zdef::validate::context_for(e, site, self.gp);
        self.compile(src, &ctx, path)
    }

    fn compile(
        &mut self,
        src: &(String, Pos),
        ctx: &zdef::expr::Context,
        path: &str,
    ) -> Option<Expr> {
        match expr::compile(&src.0, self.pool, ctx) {
            Ok(c) => {
                self.longest = self.longest.max(c.len());
                Some(Expr(c))
            }
            Err(er) => {
                self.errs.push(Diag {
                    pos: src.1,
                    path: path.into(),
                    message: format!("expression {:?}: {er}", src.0),
                });
                None
            }
        }
    }

    fn named(
        &mut self,
        e: &Entity,
        list: &[(String, String, Pos)],
        path: &str,
    ) -> Vec<(String, Expr)> {
        let mut out = Vec::new();
        for (name, src, pos) in list {
            expr::intern(self.pool, Const::Str(name.clone()));
            if let Some(x) = self.one(
                e,
                Site::Attr,
                &(src.clone(), *pos),
                &format!("{path}/{name}"),
            ) {
                out.push((name.clone(), x));
            }
        }
        out
    }

    fn entity(&mut self, e: &mut Entity, s: &ESrc, path: &str) {
        if let Some(x) = &s.from {
            e.from = self.one(e, Site::From, x, &format!("{path}/from"));
        }
        if let Some(x) = &s.to {
            let c = self.one(e, Site::To, x, &format!("{path}/to"));
            e.more_mut().to = c;
        }
        if let Some(x) = &s.reset_after {
            let c = self.one(e, Site::ResetAfter, x, &format!("{path}/reset_after"));
            e.more_mut().reset_after = c;
        }
        if let Some(m) = e.more.as_deref_mut() {
            for ((k, pe), (_, ps)) in m.parts.iter_mut().zip(&s.parts) {
                let name = keys::sub_name(*k as u64).unwrap_or("?");
                self.entity(pe, ps, &format!("{path}/parts/{name}"));
            }
        }
        for t in &e.more().event_types {
            expr::intern(self.pool, Const::Str(t.clone()));
        }
        let snapshot = e.clone();
        if let Some(m) = e.more.as_deref_mut() {
            for (i, (t, ts)) in m.triggers.iter_mut().zip(&s.triggers).enumerate() {
                let tp = format!("{path}/triggers/{i}");
                t.attrs = self.named(&snapshot, &ts.attrs, &format!("{tp}/attrs"));
                if let Some(x) = &ts.when {
                    t.when = self.one(&snapshot, Site::When, x, &format!("{tp}/when"));
                }
                if let Some(x) = &ts.event {
                    if let Some(c) = self.one(&snapshot, Site::Event, x, &format!("{tp}/event")) {
                        t.event = c;
                    }
                }
            }
            if let (Some(Press::Cmd { payload, .. }), Some(x)) = (&mut m.press, &s.press_payload) {
                *payload = self.one(
                    &snapshot,
                    Site::PressPayload,
                    x,
                    &format!("{path}/press/payload"),
                );
            }
        }
        if let Some(x) = &s.present_if {
            let c = self.one(&snapshot, Site::PresentIf, x, &format!("{path}/present_if"));
            e.more_mut().present_if = c;
        }
        let attrs = self.named(&snapshot, &s.attrs, &format!("{path}/attrs"));
        if !attrs.is_empty() {
            e.more_mut().attrs = attrs;
        }
        e.compact();
    }
}

/// Source key of a named optional extension -> CBOR key.
fn ext_key(table: &[(u16, &str)], name: &str) -> Option<u64> {
    table.iter().find(|x| x.1 == name).map(|x| x.0 as u64)
}

/// Extension value: canonical CBOR with text keys.
fn ext_value(n: &Node) -> Vec<u8> {
    let mut w = zdef::cbor::Writer::new();
    raw_cbor(n, &mut w);
    w.into_vec()
}

/// Generic tree -> CBOR (text keys), for the reserved `x-wasm` block.
fn raw_cbor(n: &Node, w: &mut zdef::cbor::Writer) {
    match &n.v {
        Value::Null => w.null(),
        Value::Bool(b) => w.bool(*b),
        Value::Int(i) => w.int(*i),
        Value::Float(f) => zdef::num::write_num(w, Num::Float(*f)),
        Value::Str(s) => w.text(s),
        Value::Seq(v) => {
            w.array(v.len());
            for x in v {
                raw_cbor(x, w);
            }
        }
        Value::Map(m) => {
            let mut e: Vec<(Vec<u8>, Vec<u8>)> = m
                .iter()
                .map(|(k, _, v)| {
                    let mut kw = zdef::cbor::Writer::new();
                    kw.text(&k.as_json());
                    let mut vw = zdef::cbor::Writer::new();
                    raw_cbor(v, &mut vw);
                    (kw.buf, vw.buf)
                })
                .collect();
            e.sort();
            w.map(e.len());
            for (k, v) in e {
                w.raw(&k);
                w.raw(&v);
            }
        }
    }
}

const TOP_FIELDS: &[&str] = &[
    "zdef",
    "zdef_fragment",
    "id",
    "revision",
    "vendor",
    "model",
    "description",
    "match",
    "white_label",
    "requires",
    "device",
    "generic",
    "endpoints",
    "tuya",
    "configure",
    "entities",
    "gp",
    "ignore",
    "source",
    "mfr",
    "priority",
    "self_description",
    "templates",
    "x-wasm",
    "x-partial",
];

fn source_is_gp(root: &Node) -> bool {
    if root.get("gp").is_some() {
        return true;
    }
    if let Some(Value::Seq(m)) = root.get("match").map(|n| &n.v) {
        if m.iter().any(|c| c.get("gp").is_some()) {
            return true;
        }
    }
    if let Some(Value::Seq(es)) = root.get("entities").map(|n| &n.v) {
        for e in es {
            if e.get("gp").is_some() {
                return true;
            }
            if let Some(Value::Seq(ts)) = e.get("triggers").map(|n| &n.v) {
                if ts.iter().any(|t| t.get("gp_cmd").is_some()) {
                    return true;
                }
            }
        }
    }
    false
}

/// Lowers a parsed source tree into a definition (expressions compiled,
/// `requires` computed and merged with the written list).
pub fn lower(root: &Node, emit_requires: bool) -> Result<Lowered, Vec<Diag>> {
    let mut l = L {
        errs: Vec::new(),
        cmd_payload: Vec::new(),
    };
    let Some(top) = l.obj(root, "", TOP_FIELDS) else {
        return Err(l.errs);
    };
    let kind = match (root.get("zdef"), root.get("zdef_fragment")) {
        (Some(_), None) => Kind::Definition,
        (None, Some(_)) => Kind::Fragment,
        _ => {
            l.e(root, "", "exactly one of zdef or zdef_fragment is required");
            return Err(l.errs);
        }
    };
    let mut d = Definition::new(kind);
    let mut esrc: Vec<ESrc> = Vec::new();
    let mut patch_src: Vec<ESrc> = Vec::new();
    let mut written_requires: Vec<u16> = Vec::new();
    for (k, _, v) in top {
        let key = k.as_str().unwrap_or("");
        let p = format!("/{key}");
        match key {
            "zdef" | "zdef_fragment" => {
                if let Some(m) = l.int(v, &p, 0, 255) {
                    if !zdef::codec::SUPPORTED_MAJORS.contains(&(m as u8)) {
                        l.e(v, &p, format!("unsupported schema major {m}"));
                    }
                    d.major = m as u8;
                }
            }
            "id" => d.id = l.str_(v, &p),
            "revision" => d.revision = l.u32_(v, &p),
            "vendor" => d.vendor = l.str_(v, &p),
            "model" => d.model = l.str_(v, &p),
            "description" => d.description = l.str_(v, &p),
            "source" => d.source = l.str_(v, &p),
            "mfr" => d.mfr = l.u16_(v, &p),
            "priority" => d.priority = l.int(v, &p, -1000, 1000),
            "self_description" => {
                d.self_description =
                    l.enum_str(v, &p, SelfDescription::parse, "self_description policy")
            }
            "match" => d.match_ = l.list(v, &p, |l, n, p| l.clause(n, p)),
            "white_label" => {
                d.white_label = l.list(v, &p, |l, n, p| {
                    let f = l.obj(
                        n,
                        p,
                        &[
                            "vendor",
                            "model",
                            "manufacturer",
                            "model_id",
                            "sw_build",
                            "logical_type",
                            "hw_version",
                            "endpoints",
                        ],
                    )?;
                    let mut w = WhiteLabel::default();
                    for (k2, _, v2) in f {
                        let p2 = format!("{p}/{}", k2.as_json());
                        match k2.as_str().unwrap_or("") {
                            "vendor" => w.vendor = l.str_(v2, &p2),
                            "model" => w.model = l.str_(v2, &p2),
                            "model_id" => w.model_id = l.list(v2, &p2, |l, n, p| l.str_(n, p)),
                            "sw_build" => w.sw_build = l.list(v2, &p2, |l, n, p| l.str_(n, p)),
                            "logical_type" => {
                                w.logical_type =
                                    l.enum_str(v2, &p2, LogicalType::parse, "logical type")
                            }
                            "hw_version" => w.hw_version = l.list(v2, &p2, |l, n, p| l.u8_(n, p)),
                            "endpoints" => w.endpoints = l.endpoints(v2, &p2),
                            _ => w.manufacturer = l.list(v2, &p2, |l, n, p| l.str_(n, p)),
                        }
                    }
                    Some(w)
                })
            }
            "requires" => {
                written_requires = l.list(v, &p, |l, n, p| {
                    l.enum_str(n, p, Feature::parse, "feature")
                        .map(|f| f.code() as u16)
                });
            }
            "device" => {
                let Some(f) = l.obj(
                    v,
                    &p,
                    &[
                        "power",
                        "sleepy",
                        "battery",
                        "checkin_s",
                        "availability",
                        "ota",
                    ],
                ) else {
                    continue;
                };
                let mut dev = Device::default();
                for (k2, _, v2) in f {
                    let p2 = format!("{p}/{}", k2.as_json());
                    match k2.as_str().unwrap_or("") {
                        "power" => dev.power = l.enum_str(v2, &p2, Power::parse, "power"),
                        "sleepy" => dev.sleepy = l.bool_(v2, &p2),
                        "battery" => {
                            dev.battery = match &v2.v {
                                Value::Str(_) => l
                                    .enum_str(v2, &p2, BatteryCurve::parse, "battery curve")
                                    .map(Battery::Curve),
                                _ => {
                                    let bf = l.obj(v2, &p2, &["voltage_to_percent"]);
                                    match bf.and_then(|b| b.first()).map(|x| x.2.clone()) {
                                        Some(vn) => {
                                            let r = l.list(
                                                &vn,
                                                &format!("{p2}/voltage_to_percent"),
                                                |l, n, p| l.u32_(n, p),
                                            );
                                            if r.len() == 2 && r[0] < r[1] {
                                                Some(Battery::Linear {
                                                    min_mv: r[0],
                                                    max_mv: r[1],
                                                })
                                            } else {
                                                l.e(
                                                    &vn,
                                                    &p2,
                                                    "voltage_to_percent is [min_mV, max_mV]",
                                                );
                                                None
                                            }
                                        }
                                        None => {
                                            l.e(
                                                v2,
                                                &p2,
                                                "battery is a curve name or {voltage_to_percent}",
                                            );
                                            None
                                        }
                                    }
                                }
                            }
                        }
                        "checkin_s" => dev.checkin_s = l.u32_(v2, &p2),
                        "availability" => {
                            let af = l.obj(v2, &p2, &["timeout_s"]);
                            let t = af
                                .and_then(|a| a.first())
                                .map(|x| x.2.clone())
                                .and_then(|n| l.u32_(&n, &format!("{p2}/timeout_s")));
                            dev.availability = Some(Availability { timeout_s: t });
                        }
                        _ => dev.ota = l.bool_(v2, &p2),
                    }
                }
                d.device = Some(dev);
            }
            "generic" => {
                let Some(f) = l.obj(v, &p, &["exclude", "patch", "disable", "only_endpoints"])
                else {
                    continue;
                };
                let mut g = Generic::default();
                for (k2, _, v2) in f {
                    let p2 = format!("{p}/{}", k2.as_json());
                    match k2.as_str().unwrap_or("") {
                        "exclude" => g.exclude = l.list(v2, &p2, |l, n, p| l.str_(n, p)),
                        "disable" => g.disable = l.bool_(v2, &p2),
                        "only_endpoints" => {
                            g.only_endpoints =
                                l.list(v2, &p2, |l, n, p| l.int(n, p, 1, 254).map(|x| x as u8))
                        }
                        _ => {
                            let mut entries: Vec<(String, Entity, ESrc)> = Vec::new();
                            if let Value::Map(m) = &v2.v {
                                for (pk, _, pv) in m {
                                    let name = pk.as_json();
                                    if let Some((e, s)) =
                                        l.entity(pv, &format!("{p2}/{name}"), false)
                                    {
                                        entries.push((name, e, s));
                                    }
                                }
                            } else {
                                l.e(v2, &p2, "patch is an object of key: fields");
                            }
                            // canonical (encoded-key) order: shorter first, then bytewise
                            entries.sort_by(|a, b| {
                                (a.0.len(), a.0.as_bytes()).cmp(&(b.0.len(), b.0.as_bytes()))
                            });
                            for (k3, e, s) in entries {
                                g.patch.push((k3, e));
                                patch_src.push(s);
                            }
                        }
                    }
                }
                d.generic = Some(g);
            }
            "endpoints" => {
                for (ep, n) in l.int_keyed(v, &p, 1, 255) {
                    if let Some(s) = l.str_(n, &format!("{p}/{ep}")) {
                        d.endpoints.push((ep as u8, s));
                    }
                }
                d.endpoints.sort_by_key(|e| e.0);
            }
            "tuya" => {
                let Some(f) = l.obj(
                    v,
                    &p,
                    &[
                        "time_sync",
                        "force_time_sync_s",
                        "magic_packet",
                        "query",
                        "bind_basic",
                        "gateway_status",
                        "send",
                        "mcu_version",
                        "optimistic",
                        "dedup",
                    ],
                ) else {
                    continue;
                };
                let mut t = Tuya::default();
                for (k2, _, v2) in f {
                    let p2 = format!("{p}/{}", k2.as_json());
                    match k2.as_str().unwrap_or("") {
                        "time_sync" => {
                            t.time_sync = match &v2.v {
                                Value::Int(1970) => Some(TimeSync::Epoch1970),
                                Value::Int(2000) => Some(TimeSync::Epoch2000),
                                Value::Str(s) if TimeSync::parse(s).is_some() => TimeSync::parse(s),
                                _ => {
                                    l.e(v2, &p2, "time_sync is \"off\", 1970 or 2000");
                                    None
                                }
                            }
                        }
                        "force_time_sync_s" => t.force_time_sync_s = l.u32_(v2, &p2),
                        "magic_packet" => t.magic_packet = l.bool_(v2, &p2),
                        "query" => t.query = l.tuya_query(v2, &p2),
                        "bind_basic" => t.bind_basic = l.bool_(v2, &p2),
                        "gateway_status" => t.gateway_status = l.bool_(v2, &p2),
                        "mcu_version" => t.mcu_version = l.bool_(v2, &p2),
                        "optimistic" => t.optimistic = l.bool_(v2, &p2),
                        "dedup" => t.dedup = l.bool_(v2, &p2),
                        _ => t.send = l.enum_str(v2, &p2, TuyaSend::parse, "send command"),
                    }
                }
                d.tuya = Some(t);
            }
            "configure" => d.configure = l.list(v, &p, |l, n, p| l.configure_op(n, p)),
            "entities" => {
                let items = l.seq(v, &p).to_vec();
                for (i, n) in items.iter().enumerate() {
                    if let Some((e, s)) = l.entity(n, &format!("{p}/{i}"), true) {
                        d.entities.push(e);
                        esrc.push(s);
                    }
                }
            }
            "gp" => {
                let Some(f) = l.obj(
                    v,
                    &p,
                    &["security_min", "report_interval_s", "ignore_commands"],
                ) else {
                    continue;
                };
                let mut g = GpDevice::default();
                for (k2, _, v2) in f {
                    let p2 = format!("{p}/{}", k2.as_json());
                    match k2.as_str().unwrap_or("") {
                        "security_min" => g.security_min = l.int(v2, &p2, 0, 3).map(|x| x as u8),
                        "report_interval_s" => g.report_interval_s = l.u32_(v2, &p2),
                        _ => g.ignore_commands = l.list(v2, &p2, |l, n, p| l.u8_(n, p)),
                    }
                }
                d.gp = Some(g);
            }
            "ignore" => {
                d.ignore = l.list(v, &p, |l, n, p| {
                    let f = l.obj(n, p, &["ep", "cluster", "cmd"])?;
                    let (mut ep, mut cluster, mut cmd) = (None, None, None);
                    for (k2, _, v2) in f {
                        let p2 = format!("{p}/{}", k2.as_json());
                        match k2.as_str().unwrap_or("") {
                            "ep" => ep = l.u8_(v2, &p2),
                            "cluster" => cluster = l.u16_(v2, &p2),
                            _ => cmd = l.u8_(v2, &p2),
                        }
                    }
                    let Some(cluster) = cluster else {
                        l.e(n, p, "ignore needs cluster");
                        return None;
                    };
                    Some(IgnoreRule { ep, cluster, cmd })
                })
            }
            "templates" => {
                d.templates = l.list(v, &p, |l, n, p| {
                    let f = l.obj(n, p, &["name", "params"])?;
                    let mut name = None;
                    let mut params = None;
                    for (k2, _, v2) in f {
                        let p2 = format!("{p}/{}", k2.as_json());
                        match k2.as_str().unwrap_or("") {
                            "name" => name = l.enum_str(v2, &p2, TemplateName::parse, "template"),
                            _ => params = Some(ext_value(v2)),
                        }
                    }
                    match name {
                        Some(name) => Some(Template { name, params }),
                        None => {
                            l.e(n, p, "a template needs name");
                            None
                        }
                    }
                })
            }
            "x-partial" => {
                let ek = ext_key(keys::TOP_EXT, key).unwrap_or(64);
                d.ext.push((ek, ext_value(v)));
            }
            "x-wasm" => {
                let mut w = zdef::cbor::Writer::new();
                raw_cbor(v, &mut w);
                d.x_wasm = Some(w.into_vec());
            }
            _ => {}
        }
    }
    // White-label names must be in the match name list, their model IDs
    // among the clauses' models.
    let names: Vec<String> = d.match_names().into_iter().map(String::from).collect();
    let models: Vec<&str> = d
        .match_
        .iter()
        .flat_map(|c| match &c.model {
            Some(zdef::model::NameMatch::Exact(s)) => vec![zdef::matching::trim_name(s)],
            Some(zdef::model::NameMatch::Any(v)) => {
                v.iter().map(|s| zdef::matching::trim_name(s)).collect()
            }
            _ => Vec::new(),
        })
        .collect();
    for (i, w) in d.white_label.iter().enumerate() {
        for m in &w.manufacturer {
            if !names.contains(m) {
                l.errs.push(Diag {
                    pos: root.pos,
                    path: format!("/white_label/{i}/manufacturer"),
                    message: format!("{m:?} is not a manufacturer name of any match clause"),
                });
            }
        }
        for m in &w.model_id {
            if !models.contains(&zdef::matching::trim_name(m)) {
                l.errs.push(Diag {
                    pos: root.pos,
                    path: format!("/white_label/{i}/model_id"),
                    message: format!("{m:?} is not a model of any match clause"),
                });
            }
        }
    }
    if !l.errs.is_empty() {
        return Err(l.errs);
    }

    // Canonical expression pass.
    let gp = source_is_gp(root);
    let mut pool = Vec::new();
    let mut fill = Fill {
        pool: &mut pool,
        gp,
        errs: Vec::new(),
        longest: 0,
    };
    if let Some(g) = &mut d.generic {
        for ((k, e), s) in g.patch.iter_mut().zip(&patch_src) {
            fill.entity(e, s, &format!("/generic/patch/{k}"));
        }
    }
    let mut cmd_src = l.cmd_payload.iter();
    for (i, op) in d.configure.iter_mut().enumerate() {
        if let ConfigureOp::Cmd { payload, .. } = op {
            if let Some(Some(x)) = cmd_src.next() {
                let ctx = zdef::validate::configure_payload_context(gp);
                *payload = fill.compile(x, &ctx, &format!("/configure/{i}/cmd/payload"));
            }
        }
    }
    for (i, (e, s)) in d.entities.iter_mut().zip(&esrc).enumerate() {
        fill.entity(e, s, &format!("/entities/{i}"));
    }
    let longest = fill.longest;
    if !fill.errs.is_empty() {
        return Err(fill.errs);
    }
    d.consts = pool;

    let mut req = written_requires;
    if emit_requires {
        req.extend(zdef::validate::compute_requires(&d));
    }
    req.sort_unstable();
    req.dedup();
    d.requires = req;
    Ok(Lowered {
        def: d,
        longest_expr: longest,
    })
}
