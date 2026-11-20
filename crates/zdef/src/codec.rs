//! Canonical CBOR encoding and decoding of the model (spec 8).

use crate::cbor::{CborError, CborErrorKind, Head, Reader, Writer};
use crate::enums::{self, Category, Platform, StateClass, TableRef, TuyaType};
use crate::keys::{self, is_extension};
use crate::model::*;
use crate::num::{decimal_value, pack_tuya_name, unpack_tuya_name, write_float, write_num};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Supported schema majors.
pub const SUPPORTED_MAJORS: &[u8] = &[1];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError {
    pub offset: usize,
    pub kind: DecodeErrorKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeErrorKind {
    Cbor(CborErrorKind),
    /// A key outside the optional-extension range that this version does not know.
    UnknownKey {
        map: &'static str,
        key: u64,
    },
    Missing(&'static str),
    /// A value of the wrong type or out of range for the field.
    Invalid(&'static str),
    UnsupportedMajor(u8),
    /// A shared-string reference without a string table, or out of range.
    StringRef(u64),
    /// Decodes, but the bytes are not the canonical encoding of the result.
    NonCanonical,
}

impl From<CborError> for DecodeError {
    fn from(e: CborError) -> Self {
        DecodeError {
            offset: e.offset,
            kind: DecodeErrorKind::Cbor(e.kind),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// A string referenced by `event_types`/`attrs` is not in the constant pool.
    MissingConst(String),
    /// A white-label manufacturer name is not in the match name list.
    WhiteLabelName(String),
    Invalid(&'static str),
}

/// Bank string table for catalog-form decoding (spec 8.4).
pub trait StringTable {
    fn get(&self, index: u64) -> Option<&str>;
}

#[derive(Clone, Copy, Default)]
pub struct DecodeOptions<'t> {
    /// Resolve shared-string references (catalog form).
    pub strings: Option<&'t dyn StringTable>,
}

/// Decodes a standalone definition or fragment.
pub fn decode(bytes: &[u8]) -> Result<Definition, DecodeError> {
    decode_with(bytes, &DecodeOptions::default())
}

/// Decodes and requires the input to be the canonical encoding of the result.
pub fn decode_canonical(bytes: &[u8]) -> Result<Definition, DecodeError> {
    let d = decode(bytes)?;
    match encode(&d) {
        Ok(b) if b == bytes => Ok(d),
        _ => Err(DecodeError {
            offset: 0,
            kind: DecodeErrorKind::NonCanonical,
        }),
    }
}

pub fn decode_with(bytes: &[u8], opts: &DecodeOptions<'_>) -> Result<Definition, DecodeError> {
    // Pass 1: the constant pool (key 17) is needed to resolve event types and attrs names.
    let consts = {
        let mut d = D {
            r: Reader::new(bytes),
            strings: opts.strings,
            consts: &[],
            names: Vec::new(),
        };
        let mut found = Vec::new();
        d.map(
            |d, k| {
                if k == 17 {
                    found = d.consts()?;
                } else {
                    d.r.skip()?;
                }
                Ok(true)
            },
            "top",
        )?;
        d.r.finish()?;
        found
    };
    let mut d = D {
        r: Reader::new(bytes),
        strings: opts.strings,
        consts: &consts,
        names: Vec::new(),
    };
    let mut def = Definition::new(Kind::Definition);
    let mut have_kind = false;
    d.map(
        |d, k| {
            match k {
                0 | 20 => {
                    let at = d.r.position();
                    let m = d.small(at, "zdef")?;
                    if have_kind {
                        return Err(d.inv("zdef"));
                    }
                    have_kind = true;
                    def.kind = if k == 0 {
                        Kind::Definition
                    } else {
                        Kind::Fragment
                    };
                    def.major = m as u8;
                    if !SUPPORTED_MAJORS.contains(&def.major) {
                        return Err(DecodeError {
                            offset: at,
                            kind: DecodeErrorKind::UnsupportedMajor(def.major),
                        });
                    }
                }
                1 => def.id = Some(d.text()?),
                2 => def.revision = Some(d.uint_as("revision")?),
                3 => def.vendor = Some(d.text()?),
                4 => def.model = Some(d.text()?),
                5 => def.description = Some(d.text()?),
                6 => {
                    def.match_ = d.list(|d| d.clause())?;
                    d.names = def.match_names().into_iter().map(String::from).collect();
                }
                7 => def.white_label = d.list(|d| d.white_label())?,
                8 => def.requires = d.list(|d| d.uint_as("requires"))?,
                9 => def.device = Some(d.device()?),
                10 => def.generic = Some(d.generic()?),
                11 => {
                    let n = d.r.map()?;
                    let mut prev: Option<u64> = None;
                    for _ in 0..n {
                        let at = d.r.position();
                        let ep = d.r.uint()?;
                        if prev.is_some_and(|p| p >= ep) || ep > 255 {
                            return Err(DecodeError {
                                offset: at,
                                kind: DecodeErrorKind::Cbor(CborErrorKind::KeyOrder),
                            });
                        }
                        prev = Some(ep);
                        def.endpoints.push((ep as u8, d.text()?));
                    }
                }
                12 => def.tuya = Some(d.tuya()?),
                13 => def.configure = d.list(|d| d.configure_op())?,
                14 => def.entities = d.list(|d| d.entity())?,
                15 => def.gp = Some(d.gp_device()?),
                16 => def.ignore = d.list(|d| d.ignore_rule())?,
                17 => {
                    d.r.skip()?;
                    def.consts = consts.clone();
                }
                18 => def.source = Some(d.text()?),
                19 => def.mfr = Some(d.uint_as("mfr")?),
                22 => def.priority = Some(d.r.int()?),
                23 => {
                    let s = d.text()?;
                    def.self_description =
                        Some(SelfDescription::parse(&s).ok_or(d.inv("self_description"))?);
                }
                31 => def.x_wasm = Some(d.r.skip()?.to_vec()),
                k if is_extension(k) => def.ext.push((k, d.r.skip()?.to_vec())),
                _ => return Ok(false),
            }
            Ok(true)
        },
        "top",
    )?;
    d.r.finish()?;
    if !have_kind {
        return Err(DecodeError {
            offset: 0,
            kind: DecodeErrorKind::Missing("zdef"),
        });
    }
    Ok(def)
}

struct D<'a, 't, 'c> {
    r: Reader<'a>,
    strings: Option<&'t dyn StringTable>,
    consts: &'c [Const],
    names: Vec<String>,
}

type DR<T> = Result<T, DecodeError>;

impl<'a> D<'a, '_, '_> {
    fn inv(&self, what: &'static str) -> DecodeError {
        DecodeError {
            offset: self.r.position(),
            kind: DecodeErrorKind::Invalid(what),
        }
    }

    /// Integer-keyed map; `f` returns false for keys it does not know.
    fn map(&mut self, mut f: impl FnMut(&mut Self, u64) -> DR<bool>, what: &'static str) -> DR<()> {
        let n = self.r.map()?;
        let mut prev: Option<u64> = None;
        for _ in 0..n {
            let at = self.r.position();
            let k = self.r.uint()?;
            if prev.is_some_and(|p| p >= k) {
                return Err(DecodeError {
                    offset: at,
                    kind: DecodeErrorKind::Cbor(CborErrorKind::KeyOrder),
                });
            }
            prev = Some(k);
            if !f(self, k)? {
                if is_extension(k) {
                    self.r.skip()?;
                } else {
                    return Err(DecodeError {
                        offset: at,
                        kind: DecodeErrorKind::UnknownKey { map: what, key: k },
                    });
                }
            }
        }
        Ok(())
    }

    fn list<T>(&mut self, mut f: impl FnMut(&mut Self) -> DR<T>) -> DR<Vec<T>> {
        let n = self.r.array()?;
        let mut v = Vec::with_capacity(n.min(256));
        for _ in 0..n {
            v.push(f(self)?);
        }
        Ok(v)
    }

    fn small(&mut self, _at: usize, what: &'static str) -> DR<u64> {
        let v = self.r.uint()?;
        if v > 255 {
            return Err(self.inv(what));
        }
        Ok(v)
    }

    fn uint_as<T: TryFrom<u64>>(&mut self, what: &'static str) -> DR<T> {
        let at = self.r.position();
        let v = self.r.uint()?;
        T::try_from(v).map_err(|_| DecodeError {
            offset: at,
            kind: DecodeErrorKind::Invalid(what),
        })
    }

    fn bool(&mut self) -> DR<bool> {
        Ok(self.r.bool()?)
    }

    fn text(&mut self) -> DR<String> {
        let at = self.r.position();
        match self.r.peek()? {
            Head::Text(_) => Ok(self.r.text()?.to_string()),
            Head::Simple(n) if n < 16 => {
                self.r.head()?;
                self.shared(n as u64, at)
            }
            Head::Tag(6) => {
                self.r.head()?;
                let m = self.r.uint()?;
                self.shared(m.checked_add(16).ok_or(self.inv("string ref"))?, at)
            }
            _ => Err(DecodeError {
                offset: at,
                kind: DecodeErrorKind::Cbor(CborErrorKind::UnexpectedType),
            }),
        }
    }

    fn shared(&self, i: u64, at: usize) -> DR<String> {
        self.strings
            .and_then(|t| t.get(i))
            .map(|s| s.to_string())
            .ok_or(DecodeError {
                offset: at,
                kind: DecodeErrorKind::StringRef(i),
            })
    }

    fn num(&mut self) -> DR<Num> {
        let at = self.r.position();
        let h = self.r.head()?;
        Ok(match h {
            Head::Uint(v) => Num::Int(i64::try_from(v).map_err(|_| self.inv("number"))?),
            Head::Nint(v) => Num::Int(-1 - i64::try_from(v).map_err(|_| self.inv("number"))?),
            Head::F16(_) | Head::F32(_) | Head::F64(_) => Num::Float(self.r.float_head(h, at)?),
            Head::Tag(4) => {
                if self.r.array()? != 2 {
                    return Err(self.inv("decimal fraction"));
                }
                let e = self.r.int()?;
                let m = self.r.int()?;
                Num::Float(decimal_value(e, m).ok_or(self.inv("decimal fraction"))?)
            }
            _ => {
                return Err(DecodeError {
                    offset: at,
                    kind: DecodeErrorKind::Cbor(CborErrorKind::UnexpectedType),
                });
            }
        })
    }

    fn scalar(&mut self) -> DR<Scalar> {
        Ok(match self.r.peek()? {
            Head::Null => {
                self.r.head()?;
                Scalar::Null
            }
            Head::True | Head::False => Scalar::Bool(self.r.bool()?),
            Head::Text(_) | Head::Simple(_) | Head::Tag(6) => Scalar::Str(self.text()?),
            _ => match self.num()? {
                Num::Int(i) => Scalar::Int(i),
                Num::Float(f) => Scalar::Float(f),
            },
        })
    }

    fn expr(&mut self) -> DR<Expr> {
        Ok(Expr(self.r.bytes()?.to_vec()))
    }

    fn const_str(&mut self) -> DR<String> {
        let at = self.r.position();
        let i = self.r.uint()?;
        match self.consts.get(i as usize) {
            Some(Const::Str(s)) => Ok(s.clone()),
            _ => Err(DecodeError {
                offset: at,
                kind: DecodeErrorKind::Invalid("constant reference"),
            }),
        }
    }

    fn consts(&mut self) -> DR<Vec<Const>> {
        self.list(|d| {
            Ok(match d.r.peek()? {
                Head::Array(_) => Const::Map(d.list(|d| {
                    if d.r.array()? != 2 {
                        return Err(d.inv("lookup map entry"));
                    }
                    Ok((d.scalar()?, d.scalar()?))
                })?),
                Head::Text(_) | Head::Simple(_) | Head::Tag(6) => Const::Str(d.text()?),
                _ => match d.num()? {
                    Num::Int(i) => Const::Int(i),
                    Num::Float(f) => Const::Float(f),
                },
            })
        })
    }

    fn u16s(&mut self, what: &'static str) -> DR<Vec<u16>> {
        self.list(|d| d.uint_as(what))
    }

    fn name_match(&mut self, packed: bool) -> DR<NameMatch> {
        let item = |d: &mut Self| -> DR<String> {
            if packed && matches!(d.r.peek()?, Head::Bytes(_)) {
                let b = d.r.bytes()?;
                unpack_tuya_name(b).ok_or(d.inv("packed Tuya name"))
            } else {
                d.text()
            }
        };
        Ok(match self.r.peek()? {
            Head::Array(_) => NameMatch::Any(self.list(item)?),
            Head::Map(_) => {
                let mut p = None;
                self.map(
                    |d, k| {
                        if k == 18 {
                            p = Some(d.text()?);
                            Ok(true)
                        } else {
                            Ok(false)
                        }
                    },
                    "name match",
                )?;
                NameMatch::Prefix(p.ok_or(self.inv("prefix"))?)
            }
            _ => NameMatch::Exact(item(self)?),
        })
    }

    fn range(&mut self) -> DR<VersionRange> {
        let mut r = VersionRange::default();
        self.map(
            |d, k| {
                match k {
                    60 => r.min = Some(d.r.int()?),
                    61 => r.max = Some(d.r.int()?),
                    _ => return Ok(false),
                }
                Ok(true)
            },
            "range",
        )?;
        Ok(r)
    }

    fn power(&mut self) -> DR<Power> {
        let s = self.text()?;
        Power::parse(&s).ok_or(self.inv("power"))
    }

    fn clause(&mut self) -> DR<MatchClause> {
        let mut c = MatchClause::default();
        self.map(
            |d, k| {
                match k {
                    19 => c.manufacturer = Some(d.name_match(true)?),
                    20 => c.model = Some(d.name_match(false)?),
                    26 => c.gp = Some(d.gp_match()?),
                    29 => c.power = Some(d.power()?),
                    51 => c.mfr_code = Some(d.uint_as("mfr_code")?),
                    52 => c.app_version = Some(d.range()?),
                    53 => c.stack_version = Some(d.range()?),
                    54 => c.hw_version = Some(d.range()?),
                    55 => c.sw_build = Some(d.name_match(false)?),
                    56 => {
                        let n = d.r.map()?;
                        let mut prev: Option<u64> = None;
                        for _ in 0..n {
                            let at = d.r.position();
                            let ep = d.r.uint()?;
                            if prev.is_some_and(|p| p >= ep) || ep > 255 {
                                return Err(DecodeError {
                                    offset: at,
                                    kind: DecodeErrorKind::Cbor(CborErrorKind::KeyOrder),
                                });
                            }
                            prev = Some(ep);
                            let mut m = EpMatch::default();
                            d.map(
                                |d, k| {
                                    match k {
                                        21 => m.device_id = Some(d.uint_as("device_id")?),
                                        57 => m.profile = Some(d.uint_as("profile")?),
                                        58 => m.in_clusters = d.u16s("in")?,
                                        59 => m.out_clusters = d.u16s("out")?,
                                        _ => return Ok(false),
                                    }
                                    Ok(true)
                                },
                                "endpoint match",
                            )?;
                            c.endpoints.push((ep as u8, m));
                        }
                    }
                    _ => return Ok(false),
                }
                Ok(true)
            },
            "match",
        )?;
        Ok(c)
    }

    fn gp_match(&mut self) -> DR<GpMatch> {
        let mut g = GpMatch::default();
        self.map(
            |d, k| {
                match k {
                    7 => {
                        let mut c = GpClusters::default();
                        d.map(
                            |d, k| {
                                match k {
                                    137 => c.server = d.u16s("server")?,
                                    138 => c.client = d.u16s("client")?,
                                    _ => return Ok(false),
                                }
                                Ok(true)
                            },
                            "gp clusters",
                        )?;
                        g.clusters = Some(c);
                    }
                    21 => g.device_id = Some(d.uint_as("device_id")?),
                    22 => g.manufacturer_id = Some(d.uint_as("manufacturer_id")?),
                    23 => {
                        g.model_id = Some(if matches!(d.r.peek()?, Head::Map(_)) {
                            let (mut v, mut m) = (None, None);
                            d.map(
                                |d, k| {
                                    match k {
                                        8 => v = Some(d.uint_as("model_id")?),
                                        25 => m = Some(d.uint_as("mask")?),
                                        _ => return Ok(false),
                                    }
                                    Ok(true)
                                },
                                "model_id",
                            )?;
                            ModelId {
                                value: v.ok_or(d.inv("model_id.value"))?,
                                mask: Some(m.ok_or(d.inv("model_id.mask"))?),
                            }
                        } else {
                            ModelId {
                                value: d.uint_as("model_id")?,
                                mask: None,
                            }
                        })
                    }
                    24 => {
                        if d.r.array()? != 2 {
                            return Err(d.inv("src_id"));
                        }
                        g.src_id = Some((d.uint_as("src_id")?, d.uint_as("src_id")?));
                    }
                    135 => g.ieee = Some(d.text()?),
                    136 => g.commands = d.list(|d| d.uint_as("commands"))?,
                    _ => return Ok(false),
                }
                Ok(true)
            },
            "gp match",
        )?;
        Ok(g)
    }

    fn white_label(&mut self) -> DR<WhiteLabel> {
        let mut w = WhiteLabel::default();
        self.map(
            |d, k| {
                match k {
                    19 => {
                        let idx: Vec<u64> = d.list(|d| Ok(d.r.uint()?))?;
                        for i in idx {
                            let n = d
                                .names
                                .get(i as usize)
                                .cloned()
                                .ok_or(d.inv("white_label manufacturer index"))?;
                            w.manufacturer.push(n);
                        }
                    }
                    20 => w.model = Some(d.text()?),
                    43 => w.vendor = Some(d.text()?),
                    _ => return Ok(false),
                }
                Ok(true)
            },
            "white_label",
        )?;
        Ok(w)
    }

    fn device(&mut self) -> DR<Device> {
        let mut v = Device::default();
        self.map(
            |d, k| {
                match k {
                    29 => v.power = Some(d.power()?),
                    30 => v.sleepy = Some(d.bool()?),
                    31 => {
                        v.battery = Some(if matches!(d.r.peek()?, Head::Map(_)) {
                            let mut lin = None;
                            d.map(
                                |d, k| {
                                    if k != 32 {
                                        return Ok(false);
                                    }
                                    if d.r.array()? != 2 {
                                        return Err(d.inv("voltage_to_percent"));
                                    }
                                    lin = Some(Battery::Linear {
                                        min_mv: d.uint_as("voltage_to_percent")?,
                                        max_mv: d.uint_as("voltage_to_percent")?,
                                    });
                                    Ok(true)
                                },
                                "battery",
                            )?;
                            lin.ok_or(d.inv("battery"))?
                        } else {
                            Battery::Curve(d.text()?)
                        })
                    }
                    33 => v.checkin_s = Some(d.uint_as("checkin_s")?),
                    131 => {
                        let mut a = Availability::default();
                        d.map(
                            |d, k| {
                                if k != 132 {
                                    return Ok(false);
                                }
                                a.timeout_s = Some(d.uint_as("timeout_s")?);
                                Ok(true)
                            },
                            "availability",
                        )?;
                        v.availability = Some(a);
                    }
                    133 => v.ota = Some(d.bool()?),
                    _ => return Ok(false),
                }
                Ok(true)
            },
            "device",
        )?;
        Ok(v)
    }

    fn generic(&mut self) -> DR<Generic> {
        let mut g = Generic::default();
        self.map(
            |d, k| {
                match k {
                    34 => g.exclude = d.list(|d| d.text())?,
                    35 => {
                        let n = d.r.map()?;
                        let mut order = crate::cbor::KeyOrder::new();
                        let buf_start = d.r.clone();
                        let _ = buf_start;
                        for _ in 0..n {
                            let at = d.r.position();
                            let key_bytes_start = d.r.remaining();
                            let key = d.r.text()?.to_string();
                            let used = key_bytes_start.len() - d.r.remaining().len();
                            order.check(&key_bytes_start[..used], at)?;
                            g.patch.push((key, d.entity()?));
                        }
                    }
                    36 => g.disable = Some(d.bool()?),
                    37 => g.only_endpoints = d.list(|d| d.uint_as("only_endpoints"))?,
                    _ => return Ok(false),
                }
                Ok(true)
            },
            "generic",
        )?;
        Ok(g)
    }

    fn tuya(&mut self) -> DR<Tuya> {
        let mut t = Tuya::default();
        self.map(
            |d, k| {
                match k {
                    38 => {
                        t.time_sync = Some(match d.r.peek()? {
                            Head::Uint(1970) => {
                                d.r.uint()?;
                                TimeSync::Epoch1970
                            }
                            Head::Uint(2000) => {
                                d.r.uint()?;
                                TimeSync::Epoch2000
                            }
                            _ => {
                                if d.text()? != "off" {
                                    return Err(d.inv("time_sync"));
                                }
                                TimeSync::Off
                            }
                        })
                    }
                    39 => t.magic_packet = Some(d.bool()?),
                    40 => {
                        t.query = Some(if matches!(d.r.peek()?, Head::Map(_)) {
                            let mut iv = None;
                            d.map(
                                |d, k| {
                                    if k != 146 {
                                        return Ok(false);
                                    }
                                    iv = Some(d.uint_as("interval_s")?);
                                    Ok(true)
                                },
                                "query",
                            )?;
                            TuyaQuery::Interval(iv.ok_or(d.inv("query"))?)
                        } else {
                            let (mut c, mut a) = (false, false);
                            for s in d.list(|d| d.text())? {
                                match s.as_str() {
                                    "configure" => c = true,
                                    "announce" => a = true,
                                    _ => return Err(d.inv("query")),
                                }
                            }
                            TuyaQuery::On {
                                configure: c,
                                announce: a,
                            }
                        })
                    }
                    139 => t.force_time_sync_s = Some(d.uint_as("force_time_sync_s")?),
                    140 => t.bind_basic = Some(d.bool()?),
                    141 => t.gateway_status = Some(d.bool()?),
                    142 => {
                        t.send = Some(match d.text()?.as_str() {
                            "dataRequest" => TuyaSend::DataRequest,
                            "sendData" => TuyaSend::SendData,
                            _ => return Err(d.inv("send")),
                        })
                    }
                    143 => t.mcu_version = Some(d.bool()?),
                    144 => t.optimistic = Some(d.bool()?),
                    145 => t.dedup = Some(d.bool()?),
                    _ => return Ok(false),
                }
                Ok(true)
            },
            "tuya",
        )?;
        Ok(t)
    }

    fn report(&mut self) -> DR<Report> {
        if matches!(self.r.peek()?, Head::False) {
            self.r.bool()?;
            return Ok(Report::Off);
        }
        let n = self.r.array()?;
        if !(2..=3).contains(&n) {
            return Err(self.inv("report"));
        }
        let min = self.uint_as("report")?;
        let max = self.uint_as("report")?;
        let change = if n == 3 { Some(self.num()?) } else { None };
        Ok(Report::Config { min, max, change })
    }

    fn write_attr(&mut self) -> DR<WriteAttr> {
        let (mut ep, mut cluster, mut attr, mut ty, mut value, mut mfr) =
            (None, None, None, None, None, None);
        self.map(
            |d, k| {
                match k {
                    0 => cluster = Some(d.uint_as("cluster")?),
                    1 => attr = Some(d.uint_as("attr")?),
                    2 => ty = Some(d.uint_as("type")?),
                    3 => mfr = Some(d.uint_as("mfr")?),
                    4 => ep = Some(d.uint_as("ep")?),
                    8 => value = Some(d.scalar()?),
                    _ => return Ok(false),
                }
                Ok(true)
            },
            "write",
        )?;
        Ok(WriteAttr {
            ep,
            cluster: cluster.ok_or(self.inv("write.cluster"))?,
            attr: attr.ok_or(self.inv("write.attr"))?,
            zcl_type: ty.ok_or(self.inv("write.type"))?,
            value: value.ok_or(self.inv("write.value"))?,
            mfr,
        })
    }

    fn cmd_ref(&mut self) -> DR<CmdRef> {
        let (mut cluster, mut id, mut mfr, mut ep) = (None, None, None, None);
        self.map(
            |d, k| {
                match k {
                    0 => cluster = Some(d.uint_as("cluster")?),
                    3 => mfr = Some(d.uint_as("mfr")?),
                    4 => ep = Some(d.uint_as("ep")?),
                    5 => id = Some(d.uint_as("id")?),
                    _ => return Ok(false),
                }
                Ok(true)
            },
            "cmd",
        )?;
        Ok(CmdRef {
            cluster: cluster.ok_or(self.inv("cmd.cluster"))?,
            id: id.ok_or(self.inv("cmd.id"))?,
            mfr,
            ep,
        })
    }

    fn configure_op(&mut self) -> DR<ConfigureOp> {
        let mut op = None;
        self.map(
            |d, k| {
                if op.is_some() {
                    return Err(d.inv("configure op with several kinds"));
                }
                op = Some(match k {
                    11 => {
                        let c = d.cmd_ref()?;
                        ConfigureOp::Cmd {
                            ep: c.ep,
                            cluster: c.cluster,
                            id: c.id,
                            mfr: c.mfr,
                        }
                    }
                    12 => {
                        let (mut ep, mut cluster, mut attr, mut ty, mut rep, mut mfr) =
                            (None, None, None, None, None, None);
                        d.map(
                            |d, k| {
                                match k {
                                    0 => cluster = Some(d.uint_as("cluster")?),
                                    1 => attr = Some(d.uint_as("attr")?),
                                    2 => ty = Some(d.uint_as("type")?),
                                    3 => mfr = Some(d.uint_as("mfr")?),
                                    4 => ep = Some(d.uint_as("ep")?),
                                    12 => rep = Some(d.report()?),
                                    _ => return Ok(false),
                                }
                                Ok(true)
                            },
                            "report op",
                        )?;
                        ConfigureOp::Report {
                            ep,
                            cluster: cluster.ok_or(d.inv("report.cluster"))?,
                            attr: attr.ok_or(d.inv("report.attr"))?,
                            zcl_type: ty.ok_or(d.inv("report.type"))?,
                            report: rep.ok_or(d.inv("report.report"))?,
                            mfr,
                        }
                    }
                    16 => ConfigureOp::Write(d.write_attr()?),
                    27 => {
                        let (mut ep, mut clusters) = (None, Vec::new());
                        d.map(
                            |d, k| {
                                match k {
                                    4 => ep = Some(d.uint_as("ep")?),
                                    7 => clusters = d.u16s("clusters")?,
                                    _ => return Ok(false),
                                }
                                Ok(true)
                            },
                            "bind",
                        )?;
                        ConfigureOp::Bind { ep, clusters }
                    }
                    28 => {
                        let (mut ep, mut cluster, mut attrs, mut mfr) =
                            (None, None, Vec::new(), None);
                        d.map(
                            |d, k| {
                                match k {
                                    0 => cluster = Some(d.uint_as("cluster")?),
                                    3 => mfr = Some(d.uint_as("mfr")?),
                                    4 => ep = Some(d.uint_as("ep")?),
                                    6 => attrs = d.u16s("attrs")?,
                                    _ => return Ok(false),
                                }
                                Ok(true)
                            },
                            "read",
                        )?;
                        ConfigureOp::Read {
                            ep,
                            cluster: cluster.ok_or(d.inv("read.cluster"))?,
                            attrs,
                            mfr,
                        }
                    }
                    128 => {
                        let (mut ep, mut group) = (None, None);
                        d.map(
                            |d, k| {
                                match k {
                                    4 => ep = Some(d.uint_as("ep")?),
                                    128 => group = Some(d.uint_as("group")?),
                                    _ => return Ok(false),
                                }
                                Ok(true)
                            },
                            "group",
                        )?;
                        ConfigureOp::Group {
                            ep,
                            group: group.ok_or(d.inv("group.group"))?,
                        }
                    }
                    129 => {
                        let mut iv = None;
                        d.map(
                            |d, k| {
                                if k != 130 {
                                    return Ok(false);
                                }
                                iv = Some(d.uint_as("interval_qs")?);
                                Ok(true)
                            },
                            "checkin",
                        )?;
                        ConfigureOp::Checkin {
                            interval_qs: iv.ok_or(d.inv("checkin.interval_qs"))?,
                        }
                    }
                    _ => return Ok(false),
                });
                Ok(true)
            },
            "configure",
        )?;
        op.ok_or(self.inv("configure op"))
    }

    fn gp_device(&mut self) -> DR<GpDevice> {
        let mut g = GpDevice::default();
        self.map(
            |d, k| {
                match k {
                    41 => g.security_min = Some(d.uint_as("security_min")?),
                    42 => g.report_interval_s = Some(d.uint_as("report_interval_s")?),
                    134 => g.ignore_commands = d.list(|d| d.uint_as("ignore_commands"))?,
                    _ => return Ok(false),
                }
                Ok(true)
            },
            "gp",
        )?;
        Ok(g)
    }

    fn ignore_rule(&mut self) -> DR<IgnoreRule> {
        let (mut ep, mut cluster, mut cmd) = (None, None, None);
        self.map(
            |d, k| {
                match k {
                    0 => cluster = Some(d.uint_as("cluster")?),
                    4 => ep = Some(d.uint_as("ep")?),
                    11 => cmd = Some(d.uint_as("cmd")?),
                    _ => return Ok(false),
                }
                Ok(true)
            },
            "ignore",
        )?;
        Ok(IgnoreRule {
            ep,
            cluster: cluster.ok_or(self.inv("ignore.cluster"))?,
            cmd,
        })
    }

    fn tuya_dp(&mut self) -> DR<TuyaDp> {
        if self.r.array()? != 2 {
            return Err(self.inv("tuya"));
        }
        let dp = self.uint_as("tuya dp")?;
        let t: u64 = self.r.uint()?;
        Ok(TuyaDp {
            dp,
            ty: TuyaType::from_code(t).ok_or(self.inv("tuya type"))?,
        })
    }

    fn named_exprs(&mut self) -> DR<Vec<(String, Expr)>> {
        let n = self.r.map()?;
        let mut prev: Option<u64> = None;
        let mut out = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            let at = self.r.position();
            let mut probe = self.r.clone();
            let k = probe.uint()?;
            if prev.is_some_and(|p| p >= k) {
                return Err(DecodeError {
                    offset: at,
                    kind: DecodeErrorKind::Cbor(CborErrorKind::KeyOrder),
                });
            }
            prev = Some(k);
            let name = self.const_str()?;
            out.push((name, self.expr()?));
        }
        Ok(out)
    }

    fn codec(&mut self) -> DR<Codec> {
        if !matches!(self.r.peek()?, Head::Map(_)) {
            let s = self.text()?;
            return Ok(match s.as_str() {
                "zcl_tagged" => Codec::ZclTagged,
                "bcd_time" => Codec::BcdTime,
                "hhmm" => Codec::Hhmm,
                "ascii" => Codec::Ascii,
                "utf16be_hex" => Codec::Utf16beHex,
                _ => return Err(self.inv("codec")),
            });
        }
        let mut c = None;
        self.map(
            |d, k| {
                if c.is_some() {
                    return Err(d.inv("codec with several names"));
                }
                c = Some(match k {
                    44 => {
                        let (mut day, mut tr) = (None, None);
                        d.map(
                            |d, k| {
                                match k {
                                    45 => day = Some(d.uint_as("day")?),
                                    46 => tr = Some(d.uint_as("transitions")?),
                                    _ => return Ok(false),
                                }
                                Ok(true)
                            },
                            "tuya_schedule_day",
                        )?;
                        Codec::TuyaScheduleDay {
                            day: day.ok_or(d.inv("day"))?,
                            transitions: tr.ok_or(d.inv("transitions"))?,
                        }
                    }
                    149 => {
                        let mut v = None;
                        d.map(
                            |d, k| {
                                if k != 150 {
                                    return Ok(false);
                                }
                                v = Some(d.text()?);
                                Ok(true)
                            },
                            "tuya_schedule_week",
                        )?;
                        Codec::TuyaScheduleWeek {
                            variant: v.ok_or(d.inv("variant"))?,
                        }
                    }
                    155 => {
                        let n = d.r.map()?;
                        let mut out = Vec::new();
                        let mut prev: Option<u64> = None;
                        for _ in 0..n {
                            let at = d.r.position();
                            let b = d.r.uint()?;
                            if prev.is_some_and(|p| p >= b) || b > 63 {
                                return Err(DecodeError {
                                    offset: at,
                                    kind: DecodeErrorKind::Invalid("bitmap_flags bit"),
                                });
                            }
                            prev = Some(b);
                            out.push((b as u8, d.text()?));
                        }
                        Codec::BitmapFlags(out)
                    }
                    _ => return Ok(false),
                });
                Ok(true)
            },
            "codec",
        )?;
        c.ok_or(self.inv("codec"))
    }

    fn table_ref(&mut self, table: &[&str]) -> DR<TableRef> {
        let c: u16 = self.uint_as("table index")?;
        Ok(if (c as usize) < table.len() {
            TableRef::Known(c)
        } else {
            TableRef::Unknown(c)
        })
    }

    fn entity(&mut self) -> DR<Entity> {
        let mut e = Entity::default();
        self.map(
            |d, k| {
                match k {
                    0 => e.key = Some(d.text()?),
                    1 => {
                        let at = d.r.position();
                        let c = d.r.uint()?;
                        e.platform = Some(Platform::from_code(c).ok_or(DecodeError {
                            offset: at,
                            kind: DecodeErrorKind::Invalid("platform"),
                        })?);
                    }
                    2 => e.device_class = Some(d.table_ref(enums::DEVICE_CLASSES)?),
                    3 => e.unit = Some(d.table_ref(enums::UNITS)?),
                    4 => {
                        let c = d.r.uint()?;
                        e.state_class = Some(StateClass::from_code(c).ok_or(d.inv("state_class"))?);
                    }
                    5 => {
                        let c = d.r.uint()?;
                        e.category = Some(Category::from_code(c).ok_or(d.inv("category"))?);
                    }
                    6 => e.name = Some(d.text()?),
                    7 => e.ep = Some(d.uint_as("ep")?),
                    8 => {
                        let (mut cl, mut at, mut ty, mut mfr, mut ep) =
                            (None, None, None, None, None);
                        d.map(
                            |d, k| {
                                match k {
                                    0 => cl = Some(d.uint_as("cluster")?),
                                    1 => at = Some(d.uint_as("attr")?),
                                    2 => ty = Some(d.uint_as("type")?),
                                    3 => mfr = Some(d.uint_as("mfr")?),
                                    4 => ep = Some(d.uint_as("ep")?),
                                    _ => return Ok(false),
                                }
                                Ok(true)
                            },
                            "zcl",
                        )?;
                        e.zcl = Some(ZclBinding {
                            cluster: cl.ok_or(d.inv("zcl.cluster"))?,
                            attr: at.ok_or(d.inv("zcl.attr"))?,
                            zcl_type: ty.ok_or(d.inv("zcl.type"))?,
                            mfr,
                            ep,
                        });
                    }
                    9 => e.tuya = Some(d.tuya_dp()?),
                    10 => {
                        let (mut cl, mut at, mut mfr, mut ep, mut codec, mut tag) =
                            (None, None, None, None, None, None);
                        d.map(
                            |d, k| {
                                match k {
                                    0 => cl = Some(d.uint_as("cluster")?),
                                    1 => at = Some(d.uint_as("attr")?),
                                    3 => mfr = Some(d.uint_as("mfr")?),
                                    4 => ep = Some(d.uint_as("ep")?),
                                    9 => codec = Some(d.text()?),
                                    10 => tag = Some(d.r.int()?),
                                    _ => return Ok(false),
                                }
                                Ok(true)
                            },
                            "struct",
                        )?;
                        e.struct_ = Some(StructBinding {
                            cluster: cl.ok_or(d.inv("struct.cluster"))?,
                            attr: at.ok_or(d.inv("struct.attr"))?,
                            mfr,
                            ep,
                            codec: codec.ok_or(d.inv("struct.codec"))?,
                            tag,
                        });
                    }
                    11 => {
                        let mut b = None;
                        d.map(
                            |d, k| {
                                if b.is_some() {
                                    return Err(d.inv("gp binding"));
                                }
                                match k {
                                    11 => b = Some(GpBinding::Cmd(d.uint_as("gp.cmd")?)),
                                    12 => {
                                        let (mut cl, mut at, mut ty, mut mfr) =
                                            (None, None, None, None);
                                        d.map(
                                            |d, k| {
                                                match k {
                                                    0 => cl = Some(d.uint_as("cluster")?),
                                                    1 => at = Some(d.uint_as("attr")?),
                                                    2 => ty = Some(d.uint_as("type")?),
                                                    3 => mfr = Some(d.uint_as("mfr")?),
                                                    _ => return Ok(false),
                                                }
                                                Ok(true)
                                            },
                                            "gp report",
                                        )?;
                                        b = Some(GpBinding::Report {
                                            cluster: cl.ok_or(d.inv("gp.report.cluster"))?,
                                            attr: at.ok_or(d.inv("gp.report.attr"))?,
                                            zcl_type: ty.ok_or(d.inv("gp.report.type"))?,
                                            mfr,
                                        });
                                    }
                                    _ => return Ok(false),
                                }
                                Ok(true)
                            },
                            "gp binding",
                        )?;
                        e.gp = Some(b.ok_or(d.inv("gp binding"))?);
                    }
                    12 => e.from = Some(d.expr()?),
                    13 => e.to = Some(d.expr()?),
                    14 => e.mul = Some(d.num()?),
                    15 => e.div = Some(d.num()?),
                    16 => e.offset = Some(d.num()?),
                    17 => e.precision = Some(d.uint_as("precision")?),
                    18 => e.invalid = Some(d.num()?),
                    19 => {
                        let n = d.r.array()?;
                        if n % 2 != 0 {
                            return Err(d.inv("map"));
                        }
                        for _ in 0..n / 2 {
                            let raw = d.r.int()?;
                            e.map.push((raw, d.text()?));
                        }
                    }
                    20 => e.value_on = Some(d.scalar()?),
                    21 => e.value_off = Some(d.scalar()?),
                    22 => e.min = Some(d.num()?),
                    23 => e.max = Some(d.num()?),
                    24 => e.step = Some(d.num()?),
                    25 => e.mode = Some(d.text()?),
                    26 => e.access = Some(d.uint_as("access")?),
                    27 => e.report = Some(d.report()?),
                    28 => e.enabled = Some(d.bool()?),
                    29 => e.reset_after = Some(d.expr()?),
                    30 => {
                        let n = d.r.map()?;
                        let mut prev: Option<u64> = None;
                        for _ in 0..n {
                            let at = d.r.position();
                            let pk = d.r.uint()?;
                            if prev.is_some_and(|p| p >= pk) {
                                return Err(DecodeError {
                                    offset: at,
                                    kind: DecodeErrorKind::Cbor(CborErrorKind::KeyOrder),
                                });
                            }
                            prev = Some(pk);
                            let name = keys::sub_name(pk).filter(|n| keys::PART_NAMES.contains(n));
                            if name.is_none() {
                                return Err(DecodeError {
                                    offset: at,
                                    kind: DecodeErrorKind::Invalid("part name"),
                                });
                            }
                            e.parts.push((pk as u16, d.entity()?));
                        }
                    }
                    31 => e.event_types = d.list(|d| d.const_str())?,
                    32 => {
                        e.triggers = d.list(|d| {
                            let mut t = Trigger {
                                attrs: Vec::new(),
                                cmd: None,
                                gp_cmd: None,
                                tuya: None,
                                when: None,
                                event: Expr::default(),
                            };
                            let mut has_event = false;
                            d.map(
                                |d, k| {
                                    match k {
                                        6 => t.attrs = d.named_exprs()?,
                                        11 => t.cmd = Some(d.cmd_ref()?),
                                        13 => t.gp_cmd = Some(d.uint_as("gp_cmd")?),
                                        14 => t.when = Some(d.expr()?),
                                        15 => {
                                            t.event = d.expr()?;
                                            has_event = true;
                                        }
                                        147 => t.tuya = Some(d.tuya_dp()?),
                                        _ => return Ok(false),
                                    }
                                    Ok(true)
                                },
                                "trigger",
                            )?;
                            if !has_event {
                                return Err(d.inv("trigger.event"));
                            }
                            Ok(t)
                        })?
                    }
                    33 => {
                        let (mut cmd, mut payload, mut write, mut tuya, mut value) =
                            (None, None, None, None, None);
                        d.map(
                            |d, k| {
                                match k {
                                    8 => value = Some(d.scalar()?),
                                    11 => cmd = Some(d.cmd_ref()?),
                                    16 => write = Some(d.write_attr()?),
                                    62 => payload = Some(d.expr()?),
                                    147 => tuya = Some(d.tuya_dp()?),
                                    _ => return Ok(false),
                                }
                                Ok(true)
                            },
                            "press",
                        )?;
                        e.press = Some(match (cmd, write, tuya) {
                            (Some(cmd), None, None) if value.is_none() => {
                                Press::Cmd { cmd, payload }
                            }
                            (None, Some(w), None) if value.is_none() && payload.is_none() => {
                                Press::Write(w)
                            }
                            (None, None, Some(dp)) if payload.is_none() => Press::Tuya {
                                dp,
                                value: value.ok_or(d.inv("press.value"))?,
                            },
                            _ => return Err(d.inv("press")),
                        });
                    }
                    34 => e.codec = Some(d.codec()?),
                    35 => e.present_if = Some(d.expr()?),
                    36 => e.attrs = d.named_exprs()?,
                    37 => {
                        let mut h = HubBinding { default: None };
                        d.map(
                            |d, k| {
                                if k != 63 {
                                    return Ok(false);
                                }
                                h.default = Some(d.scalar()?);
                                Ok(true)
                            },
                            "hub",
                        )?;
                        e.hub = Some(h);
                    }
                    38 => e.invert = Some(d.bool()?),
                    k if is_extension(k) => e.ext.push((k, d.r.skip()?.to_vec())),
                    _ => return Ok(false),
                }
                Ok(true)
            },
            "entity",
        )?;
        Ok(e)
    }
}

// ------------------------------------------------------------------ encoder

/// Map under construction: entries are sorted by key on finish.
struct M {
    e: Vec<(u64, Writer)>,
}

impl M {
    fn new() -> Self {
        M { e: Vec::new() }
    }
    fn put(&mut self, k: u16) -> &mut Writer {
        self.e.push((k as u64, Writer::new()));
        &mut self.e.last_mut().unwrap().1
    }
    fn raw(&mut self, k: u64, item: &[u8]) {
        let mut w = Writer::new();
        w.raw(item);
        self.e.push((k, w));
    }
    fn finish(mut self, w: &mut Writer) {
        self.e.sort_by_key(|x| x.0);
        w.map(self.e.len());
        for (k, v) in self.e {
            w.uint(k);
            w.raw(&v.buf);
        }
    }
}

type ER = Result<(), EncodeError>;

struct E<'d> {
    def: &'d Definition,
    names: Vec<&'d str>,
}

/// Encodes the canonical standalone CBOR of a definition or fragment.
pub fn encode(def: &Definition) -> Result<Vec<u8>, EncodeError> {
    let enc = E {
        def,
        names: def.match_names(),
    };
    let mut w = Writer::new();
    enc.top(&mut w)?;
    Ok(w.into_vec())
}

fn text_list(w: &mut Writer, v: &[String]) {
    w.array(v.len());
    for s in v {
        w.text(s);
    }
}

fn uint_list<T: Copy + Into<u64>>(w: &mut Writer, v: &[T]) {
    w.array(v.len());
    for x in v {
        w.uint((*x).into());
    }
}

fn scalar(w: &mut Writer, s: &Scalar) {
    match s {
        Scalar::Null => w.null(),
        Scalar::Bool(b) => w.bool(*b),
        Scalar::Int(i) => w.int(*i),
        Scalar::Float(f) => write_num(w, Num::Float(*f)),
        Scalar::Str(s) => w.text(s),
    }
}

/// Constant-pool literals keep float-ness (never encoded as integers).
fn const_scalar(w: &mut Writer, s: &Scalar) {
    match s {
        Scalar::Float(f) => write_float(w, *f),
        s => scalar(w, s),
    }
}

impl E<'_> {
    fn const_index(&self, s: &str) -> Result<u64, EncodeError> {
        self.def
            .consts
            .iter()
            .position(|c| matches!(c, Const::Str(x) if x == s))
            .map(|i| i as u64)
            .ok_or_else(|| EncodeError::MissingConst(s.to_string()))
    }

    fn top(&self, w: &mut Writer) -> ER {
        let d = self.def;
        let mut m = M::new();
        match d.kind {
            Kind::Definition => m.put(0).uint(d.major as u64),
            Kind::Fragment => m.put(20).uint(d.major as u64),
        }
        if let Some(v) = &d.id {
            m.put(1).text(v);
        }
        if let Some(v) = d.revision {
            m.put(2).uint(v as u64);
        }
        if let Some(v) = &d.vendor {
            m.put(3).text(v);
        }
        if let Some(v) = &d.model {
            m.put(4).text(v);
        }
        if let Some(v) = &d.description {
            m.put(5).text(v);
        }
        if !d.match_.is_empty() {
            let w = m.put(6);
            w.array(d.match_.len());
            for c in &d.match_ {
                self.clause(w, c);
            }
        }
        if !d.white_label.is_empty() {
            let w = m.put(7);
            w.array(d.white_label.len());
            for wl in &d.white_label {
                let mut mm = M::new();
                if !wl.manufacturer.is_empty() {
                    let w = mm.put(19);
                    w.array(wl.manufacturer.len());
                    for n in &wl.manufacturer {
                        let i = self
                            .names
                            .iter()
                            .position(|x| x == n)
                            .ok_or_else(|| EncodeError::WhiteLabelName(n.clone()))?;
                        w.uint(i as u64);
                    }
                }
                if let Some(v) = &wl.model {
                    mm.put(20).text(v);
                }
                if let Some(v) = &wl.vendor {
                    mm.put(43).text(v);
                }
                mm.finish(w);
            }
        }
        if !d.requires.is_empty() {
            uint_list(m.put(8), &d.requires);
        }
        if let Some(dev) = &d.device {
            self.device(m.put(9), dev);
        }
        if let Some(g) = &d.generic {
            self.generic(m.put(10), g)?;
        }
        if !d.endpoints.is_empty() {
            let w = m.put(11);
            let mut eps = d.endpoints.clone();
            eps.sort_by_key(|e| e.0);
            w.map(eps.len());
            for (ep, n) in &eps {
                w.uint(*ep as u64);
                w.text(n);
            }
        }
        if let Some(t) = &d.tuya {
            self.tuya(m.put(12), t);
        }
        if !d.configure.is_empty() {
            let w = m.put(13);
            w.array(d.configure.len());
            for op in &d.configure {
                self.configure_op(w, op);
            }
        }
        if !d.entities.is_empty() {
            let w = m.put(14);
            w.array(d.entities.len());
            for e in &d.entities {
                self.entity(w, e)?;
            }
        }
        if let Some(g) = &d.gp {
            let mut mm = M::new();
            if let Some(v) = g.security_min {
                mm.put(41).uint(v as u64);
            }
            if let Some(v) = g.report_interval_s {
                mm.put(42).uint(v as u64);
            }
            if !g.ignore_commands.is_empty() {
                uint_list(mm.put(134), &g.ignore_commands);
            }
            mm.finish(m.put(15));
        }
        if !d.ignore.is_empty() {
            let w = m.put(16);
            w.array(d.ignore.len());
            for r in &d.ignore {
                let mut mm = M::new();
                mm.put(0).uint(r.cluster as u64);
                if let Some(v) = r.ep {
                    mm.put(4).uint(v as u64);
                }
                if let Some(v) = r.cmd {
                    mm.put(11).uint(v as u64);
                }
                mm.finish(w);
            }
        }
        if !d.consts.is_empty() {
            let w = m.put(17);
            w.array(d.consts.len());
            for c in &d.consts {
                match c {
                    Const::Str(s) => w.text(s),
                    Const::Float(f) => write_float(w, *f),
                    Const::Int(i) => w.int(*i),
                    Const::Map(pairs) => {
                        w.array(pairs.len());
                        for (k, v) in pairs {
                            w.array(2);
                            const_scalar(w, k);
                            const_scalar(w, v);
                        }
                    }
                }
            }
        }
        if let Some(v) = &d.source {
            m.put(18).text(v);
        }
        if let Some(v) = d.mfr {
            m.put(19).uint(v as u64);
        }
        if let Some(v) = d.priority {
            m.put(22).int(v);
        }
        if let Some(v) = d.self_description {
            m.put(23).text(v.as_str());
        }
        if let Some(v) = &d.x_wasm {
            m.raw(31, v);
        }
        for (k, v) in &d.ext {
            if !is_extension(*k) {
                return Err(EncodeError::Invalid("extension key"));
            }
            m.raw(*k, v);
        }
        m.finish(w);
        Ok(())
    }

    fn name_match(&self, w: &mut Writer, n: &NameMatch, pack: bool) {
        let item = |w: &mut Writer, s: &str| match pack.then(|| pack_tuya_name(s)).flatten() {
            Some(p) => w.bytes(&p),
            None => w.text(s),
        };
        match n {
            NameMatch::Exact(s) => item(w, s),
            NameMatch::Any(v) => {
                w.array(v.len());
                for s in v {
                    item(w, s);
                }
            }
            NameMatch::Prefix(p) => {
                w.map(1);
                w.uint(18);
                w.text(p);
            }
        }
    }

    fn range(&self, w: &mut Writer, r: &VersionRange) {
        let mut m = M::new();
        if let Some(v) = r.min {
            m.put(60).int(v);
        }
        if let Some(v) = r.max {
            m.put(61).int(v);
        }
        m.finish(w);
    }

    fn clause(&self, w: &mut Writer, c: &MatchClause) {
        let mut m = M::new();
        if let Some(n) = &c.manufacturer {
            self.name_match(m.put(19), n, true);
        }
        if let Some(n) = &c.model {
            self.name_match(m.put(20), n, false);
        }
        if let Some(g) = &c.gp {
            let mut mm = M::new();
            if let Some(cl) = &g.clusters {
                let mut c3 = M::new();
                if !cl.server.is_empty() {
                    uint_list(c3.put(137), &cl.server);
                }
                if !cl.client.is_empty() {
                    uint_list(c3.put(138), &cl.client);
                }
                c3.finish(mm.put(7));
            }
            if let Some(v) = g.device_id {
                mm.put(21).uint(v as u64);
            }
            if let Some(v) = g.manufacturer_id {
                mm.put(22).uint(v as u64);
            }
            if let Some(v) = g.model_id {
                let w = mm.put(23);
                match v.mask {
                    None => w.uint(v.value as u64),
                    Some(mask) => {
                        w.map(2);
                        w.uint(8);
                        w.uint(v.value as u64);
                        w.uint(25);
                        w.uint(mask as u64);
                    }
                }
            }
            if let Some((lo, hi)) = g.src_id {
                let w = mm.put(24);
                w.array(2);
                w.uint(lo as u64);
                w.uint(hi as u64);
            }
            if let Some(v) = &g.ieee {
                mm.put(135).text(v);
            }
            if !g.commands.is_empty() {
                uint_list(mm.put(136), &g.commands);
            }
            mm.finish(m.put(26));
        }
        if let Some(p) = c.power {
            m.put(29).text(p.as_str());
        }
        if let Some(v) = c.mfr_code {
            m.put(51).uint(v as u64);
        }
        if let Some(r) = &c.app_version {
            self.range(m.put(52), r);
        }
        if let Some(r) = &c.stack_version {
            self.range(m.put(53), r);
        }
        if let Some(r) = &c.hw_version {
            self.range(m.put(54), r);
        }
        if let Some(n) = &c.sw_build {
            self.name_match(m.put(55), n, false);
        }
        if !c.endpoints.is_empty() {
            let w = m.put(56);
            let mut eps: Vec<&(u8, EpMatch)> = c.endpoints.iter().collect();
            eps.sort_by_key(|e| e.0);
            w.map(eps.len());
            for (ep, em) in eps {
                w.uint(*ep as u64);
                let mut mm = M::new();
                if let Some(v) = em.device_id {
                    mm.put(21).uint(v as u64);
                }
                if let Some(v) = em.profile {
                    mm.put(57).uint(v as u64);
                }
                if !em.in_clusters.is_empty() {
                    uint_list(mm.put(58), &em.in_clusters);
                }
                if !em.out_clusters.is_empty() {
                    uint_list(mm.put(59), &em.out_clusters);
                }
                mm.finish(w);
            }
        }
        m.finish(w);
    }

    fn device(&self, w: &mut Writer, d: &Device) {
        let mut m = M::new();
        if let Some(p) = d.power {
            m.put(29).text(p.as_str());
        }
        if let Some(v) = d.sleepy {
            m.put(30).bool(v);
        }
        match &d.battery {
            Some(Battery::Linear { min_mv, max_mv }) => {
                let w = m.put(31);
                w.map(1);
                w.uint(32);
                w.array(2);
                w.uint(*min_mv as u64);
                w.uint(*max_mv as u64);
            }
            Some(Battery::Curve(c)) => m.put(31).text(c),
            None => {}
        }
        if let Some(v) = d.checkin_s {
            m.put(33).uint(v as u64);
        }
        if let Some(a) = &d.availability {
            let mut mm = M::new();
            if let Some(t) = a.timeout_s {
                mm.put(132).uint(t as u64);
            }
            mm.finish(m.put(131));
        }
        if let Some(v) = d.ota {
            m.put(133).bool(v);
        }
        m.finish(w);
    }

    fn generic(&self, w: &mut Writer, g: &Generic) -> ER {
        let mut m = M::new();
        if !g.exclude.is_empty() {
            text_list(m.put(34), &g.exclude);
        }
        if !g.patch.is_empty() {
            let mut entries: Vec<(Vec<u8>, Writer)> = Vec::new();
            for (k, e) in &g.patch {
                let mut kw = Writer::new();
                kw.text(k);
                let mut vw = Writer::new();
                self.entity(&mut vw, e)?;
                entries.push((kw.buf, vw));
            }
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let w = m.put(35);
            w.map(entries.len());
            for (k, v) in entries {
                w.raw(&k);
                w.raw(&v.buf);
            }
        }
        if let Some(v) = g.disable {
            m.put(36).bool(v);
        }
        if !g.only_endpoints.is_empty() {
            uint_list(m.put(37), &g.only_endpoints);
        }
        m.finish(w);
        Ok(())
    }

    fn tuya(&self, w: &mut Writer, t: &Tuya) {
        let mut m = M::new();
        match t.time_sync {
            Some(TimeSync::Off) => m.put(38).text("off"),
            Some(TimeSync::Epoch1970) => m.put(38).uint(1970),
            Some(TimeSync::Epoch2000) => m.put(38).uint(2000),
            None => {}
        }
        if let Some(v) = t.magic_packet {
            m.put(39).bool(v);
        }
        match &t.query {
            Some(TuyaQuery::On {
                configure,
                announce,
            }) => {
                let w = m.put(40);
                w.array(*configure as usize + *announce as usize);
                if *configure {
                    w.text("configure");
                }
                if *announce {
                    w.text("announce");
                }
            }
            Some(TuyaQuery::Interval(s)) => {
                let w = m.put(40);
                w.map(1);
                w.uint(146);
                w.uint(*s as u64);
            }
            None => {}
        }
        if let Some(v) = t.force_time_sync_s {
            m.put(139).uint(v as u64);
        }
        for (k, v) in [
            (140, t.bind_basic),
            (141, t.gateway_status),
            (143, t.mcu_version),
            (144, t.optimistic),
            (145, t.dedup),
        ] {
            if let Some(v) = v {
                m.put(k).bool(v);
            }
        }
        if let Some(s) = t.send {
            m.put(142).text(match s {
                TuyaSend::DataRequest => "dataRequest",
                TuyaSend::SendData => "sendData",
            });
        }
        m.finish(w);
    }

    fn report(&self, w: &mut Writer, r: &Report) {
        match r {
            Report::Off => w.bool(false),
            Report::Config { min, max, change } => {
                w.array(if change.is_some() { 3 } else { 2 });
                w.uint(*min as u64);
                w.uint(*max as u64);
                if let Some(c) = change {
                    write_num(w, *c);
                }
            }
        }
    }

    fn write_attr(&self, w: &mut Writer, a: &WriteAttr) {
        let mut m = M::new();
        m.put(0).uint(a.cluster as u64);
        m.put(1).uint(a.attr as u64);
        m.put(2).uint(a.zcl_type as u64);
        if let Some(v) = a.mfr {
            m.put(3).uint(v as u64);
        }
        if let Some(v) = a.ep {
            m.put(4).uint(v as u64);
        }
        scalar(m.put(8), &a.value);
        m.finish(w);
    }

    fn cmd_ref(&self, w: &mut Writer, c: &CmdRef) {
        let mut m = M::new();
        m.put(0).uint(c.cluster as u64);
        if let Some(v) = c.mfr {
            m.put(3).uint(v as u64);
        }
        if let Some(v) = c.ep {
            m.put(4).uint(v as u64);
        }
        m.put(5).uint(c.id as u64);
        m.finish(w);
    }

    fn configure_op(&self, w: &mut Writer, op: &ConfigureOp) {
        let mut outer = M::new();
        match op {
            ConfigureOp::Cmd {
                ep,
                cluster,
                id,
                mfr,
            } => self.cmd_ref(
                outer.put(11),
                &CmdRef {
                    cluster: *cluster,
                    id: *id,
                    mfr: *mfr,
                    ep: *ep,
                },
            ),
            ConfigureOp::Report {
                ep,
                cluster,
                attr,
                zcl_type,
                report,
                mfr,
            } => {
                let mut m = M::new();
                m.put(0).uint(*cluster as u64);
                m.put(1).uint(*attr as u64);
                m.put(2).uint(*zcl_type as u64);
                if let Some(v) = mfr {
                    m.put(3).uint(*v as u64);
                }
                if let Some(v) = ep {
                    m.put(4).uint(*v as u64);
                }
                self.report(m.put(12), report);
                m.finish(outer.put(12));
            }
            ConfigureOp::Write(a) => self.write_attr(outer.put(16), a),
            ConfigureOp::Bind { ep, clusters } => {
                let mut m = M::new();
                if let Some(v) = ep {
                    m.put(4).uint(*v as u64);
                }
                if !clusters.is_empty() {
                    uint_list(m.put(7), clusters);
                }
                m.finish(outer.put(27));
            }
            ConfigureOp::Read {
                ep,
                cluster,
                attrs,
                mfr,
            } => {
                let mut m = M::new();
                m.put(0).uint(*cluster as u64);
                if let Some(v) = mfr {
                    m.put(3).uint(*v as u64);
                }
                if let Some(v) = ep {
                    m.put(4).uint(*v as u64);
                }
                if !attrs.is_empty() {
                    uint_list(m.put(6), attrs);
                }
                m.finish(outer.put(28));
            }
            ConfigureOp::Group { ep, group } => {
                let mut m = M::new();
                if let Some(v) = ep {
                    m.put(4).uint(*v as u64);
                }
                m.put(128).uint(*group as u64);
                m.finish(outer.put(128));
            }
            ConfigureOp::Checkin { interval_qs } => {
                let mut m = M::new();
                m.put(130).uint(*interval_qs as u64);
                m.finish(outer.put(129));
            }
        }
        outer.finish(w);
    }

    fn tuya_dp(&self, w: &mut Writer, t: &TuyaDp) {
        w.array(2);
        w.uint(t.dp as u64);
        w.uint(t.ty.code() as u64);
    }

    fn named_exprs(&self, w: &mut Writer, v: &[(String, Expr)]) -> ER {
        let mut entries = Vec::with_capacity(v.len());
        for (n, e) in v {
            entries.push((self.const_index(n)?, e));
        }
        entries.sort_by_key(|x| x.0);
        w.map(entries.len());
        for (i, e) in entries {
            w.uint(i);
            w.bytes(&e.0);
        }
        Ok(())
    }

    fn entity(&self, w: &mut Writer, e: &Entity) -> ER {
        let mut m = M::new();
        if let Some(v) = &e.key {
            m.put(0).text(v);
        }
        if let Some(v) = e.platform {
            m.put(1).uint(v.code() as u64);
        }
        if let Some(v) = e.device_class {
            m.put(2).uint(v.code() as u64);
        }
        if let Some(v) = e.unit {
            m.put(3).uint(v.code() as u64);
        }
        if let Some(v) = e.state_class {
            m.put(4).uint(v.code() as u64);
        }
        if let Some(v) = e.category {
            m.put(5).uint(v.code() as u64);
        }
        if let Some(v) = &e.name {
            m.put(6).text(v);
        }
        if let Some(v) = e.ep {
            m.put(7).uint(v as u64);
        }
        if let Some(z) = &e.zcl {
            let mut mm = M::new();
            mm.put(0).uint(z.cluster as u64);
            mm.put(1).uint(z.attr as u64);
            mm.put(2).uint(z.zcl_type as u64);
            if let Some(v) = z.mfr {
                mm.put(3).uint(v as u64);
            }
            if let Some(v) = z.ep {
                mm.put(4).uint(v as u64);
            }
            mm.finish(m.put(8));
        }
        if let Some(t) = &e.tuya {
            self.tuya_dp(m.put(9), t);
        }
        if let Some(s) = &e.struct_ {
            let mut mm = M::new();
            mm.put(0).uint(s.cluster as u64);
            mm.put(1).uint(s.attr as u64);
            if let Some(v) = s.mfr {
                mm.put(3).uint(v as u64);
            }
            if let Some(v) = s.ep {
                mm.put(4).uint(v as u64);
            }
            mm.put(9).text(&s.codec);
            if let Some(v) = s.tag {
                mm.put(10).int(v);
            }
            mm.finish(m.put(10));
        }
        if let Some(g) = &e.gp {
            let w = m.put(11);
            w.map(1);
            match g {
                GpBinding::Cmd(c) => {
                    w.uint(11);
                    w.uint(*c as u64);
                }
                GpBinding::Report {
                    cluster,
                    attr,
                    zcl_type,
                    mfr,
                } => {
                    w.uint(12);
                    let mut mm = M::new();
                    mm.put(0).uint(*cluster as u64);
                    mm.put(1).uint(*attr as u64);
                    mm.put(2).uint(*zcl_type as u64);
                    if let Some(v) = mfr {
                        mm.put(3).uint(*v as u64);
                    }
                    mm.finish(w);
                }
            }
        }
        if let Some(x) = &e.from {
            m.put(12).bytes(&x.0);
        }
        if let Some(x) = &e.to {
            m.put(13).bytes(&x.0);
        }
        for (k, v) in [(14, e.mul), (15, e.div), (16, e.offset)] {
            if let Some(v) = v {
                write_num(m.put(k), v);
            }
        }
        if let Some(v) = e.precision {
            m.put(17).uint(v as u64);
        }
        if let Some(v) = e.invalid {
            write_num(m.put(18), v);
        }
        if !e.map.is_empty() {
            let w = m.put(19);
            w.array(e.map.len() * 2);
            for (raw, label) in &e.map {
                w.int(*raw);
                w.text(label);
            }
        }
        if let Some(v) = &e.value_on {
            scalar(m.put(20), v);
        }
        if let Some(v) = &e.value_off {
            scalar(m.put(21), v);
        }
        for (k, v) in [(22, e.min), (23, e.max), (24, e.step)] {
            if let Some(v) = v {
                write_num(m.put(k), v);
            }
        }
        if let Some(v) = &e.mode {
            m.put(25).text(v);
        }
        if let Some(v) = e.access {
            m.put(26).uint(v as u64);
        }
        if let Some(r) = &e.report {
            self.report(m.put(27), r);
        }
        if let Some(v) = e.enabled {
            m.put(28).bool(v);
        }
        if let Some(x) = &e.reset_after {
            m.put(29).bytes(&x.0);
        }
        if !e.parts.is_empty() {
            let mut parts: Vec<&(u16, Entity)> = e.parts.iter().collect();
            parts.sort_by_key(|p| p.0);
            let w = m.put(30);
            w.map(parts.len());
            for (k, p) in parts {
                w.uint(*k as u64);
                self.entity(w, p)?;
            }
        }
        if !e.event_types.is_empty() {
            let w = m.put(31);
            w.array(e.event_types.len());
            for s in &e.event_types {
                w.uint(self.const_index(s)?);
            }
        }
        if !e.triggers.is_empty() {
            let w = m.put(32);
            w.array(e.triggers.len());
            for t in &e.triggers {
                let mut mm = M::new();
                if !t.attrs.is_empty() {
                    self.named_exprs(mm.put(6), &t.attrs)?;
                }
                if let Some(c) = &t.cmd {
                    self.cmd_ref(mm.put(11), c);
                }
                if let Some(v) = t.gp_cmd {
                    mm.put(13).uint(v as u64);
                }
                if let Some(x) = &t.when {
                    mm.put(14).bytes(&x.0);
                }
                mm.put(15).bytes(&t.event.0);
                if let Some(dp) = &t.tuya {
                    self.tuya_dp(mm.put(147), dp);
                }
                mm.finish(w);
            }
        }
        if let Some(p) = &e.press {
            let mut mm = M::new();
            match p {
                Press::Cmd { cmd, payload } => {
                    self.cmd_ref(mm.put(11), cmd);
                    if let Some(x) = payload {
                        mm.put(62).bytes(&x.0);
                    }
                }
                Press::Write(a) => self.write_attr(mm.put(16), a),
                Press::Tuya { dp, value } => {
                    scalar(mm.put(8), value);
                    self.tuya_dp(mm.put(147), dp);
                }
            }
            mm.finish(m.put(33));
        }
        if let Some(c) = &e.codec {
            let w = m.put(34);
            match c {
                Codec::ZclTagged
                | Codec::BcdTime
                | Codec::Hhmm
                | Codec::Ascii
                | Codec::Utf16beHex => w.text(c.name()),
                Codec::TuyaScheduleDay { day, transitions } => {
                    w.map(1);
                    w.uint(44);
                    w.map(2);
                    w.uint(45);
                    w.uint(*day as u64);
                    w.uint(46);
                    w.uint(*transitions as u64);
                }
                Codec::TuyaScheduleWeek { variant } => {
                    w.map(1);
                    w.uint(149);
                    w.map(1);
                    w.uint(150);
                    w.text(variant);
                }
                Codec::BitmapFlags(bits) => {
                    let mut b = bits.clone();
                    b.sort_by_key(|x| x.0);
                    w.map(1);
                    w.uint(155);
                    w.map(b.len());
                    for (bit, label) in b {
                        w.uint(bit as u64);
                        w.text(&label);
                    }
                }
            }
        }
        if let Some(x) = &e.present_if {
            m.put(35).bytes(&x.0);
        }
        if !e.attrs.is_empty() {
            self.named_exprs(m.put(36), &e.attrs)?;
        }
        if let Some(h) = &e.hub {
            let mut mm = M::new();
            if let Some(v) = &h.default {
                scalar(mm.put(63), v);
            }
            mm.finish(m.put(37));
        }
        if let Some(v) = e.invert {
            m.put(38).bool(v);
        }
        for (k, v) in &e.ext {
            if !is_extension(*k) {
                return Err(EncodeError::Invalid("extension key"));
            }
            m.raw(*k, v);
        }
        m.finish(w);
        Ok(())
    }
}

/// Removes key `drop` from the top-level map of an encoded definition
/// (diagnostics and tests; e.g. compare without `requires`).
pub fn without_top_key(bytes: &[u8], drop: u64) -> Result<Vec<u8>, DecodeError> {
    let mut r = Reader::new(bytes);
    let n = r.map()?;
    let mut items = Vec::new();
    for _ in 0..n {
        let k = r.uint()?;
        let v = r.skip()?;
        if k != drop {
            items.push((k, v));
        }
    }
    r.finish()?;
    let mut w = Writer::new();
    w.map(items.len());
    for (k, v) in items {
        w.uint(k);
        w.raw(v);
    }
    Ok(w.into_vec())
}
