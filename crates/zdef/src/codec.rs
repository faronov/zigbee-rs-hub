//! Canonical CBOR encoding and decoding of the model (spec 8).
//!
//! The decoder is a hand-written recursive descent without closures or
//! generics per field: one `match` per map, shared helpers for keys,
//! ranges and errors, so it stays small on the hub.

use crate::cbor::{CborError, CborErrorKind, Head, Reader, Writer};
use crate::enums::{self, Category, Platform, StateClass, TableRef, TuyaType};
use crate::keys::is_extension;
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
        key: u32,
    },
    Missing(&'static str),
    /// A value of the wrong type or out of range for the field.
    Invalid(&'static str),
    UnsupportedMajor(u8),
    /// A shared-string reference without a string table, or out of range.
    StringRef(u32),
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

/// Identity fields of a definition, read without decoding the rest
/// (catalog listings, lookups by `id` in a bank).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub id: Option<String>,
    pub revision: Option<u64>,
    pub vendor: Option<String>,
    pub model: Option<String>,
}

/// Reads `id`, `revision`, `vendor` and `model` of a definition (standalone
/// or catalog form) and skips everything else.
pub fn peek_summary(bytes: &[u8], opts: &DecodeOptions<'_>) -> Result<Summary, DecodeError> {
    let mut d = D {
        r: Reader::new(bytes),
        strings: opts.strings,
        consts: &[],
        names: Vec::new(),
    };
    let mut out = Summary::default();
    let mut k = d.keys()?;
    while let Some(key) = d.key(&mut k, "top")? {
        match key {
            1 => out.id = Some(d.text()?),
            2 => out.revision = Some(d.r.uint()?),
            3 => out.vendor = Some(d.text()?),
            4 => out.model = Some(d.text()?),
            _ => {
                d.r.skip()?;
            }
        }
        if key >= 4 {
            break;
        }
    }
    Ok(out)
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

type DR<T> = Result<T, DecodeError>;

/// Remaining entries of a map being read.
struct Keys {
    left: usize,
    prev: Option<u64>,
}

struct D<'a, 't, 'c> {
    r: Reader<'a>,
    strings: Option<&'t dyn StringTable>,
    consts: &'c [Const],
    names: Vec<String>,
}

/// Sets a field read once (map keys are strictly ascending, so the slot is
/// always empty: no drop glue for the old value).
#[inline(always)]
fn set<T>(slot: &mut Option<T>, v: T) {
    core::mem::forget(slot.replace(v));
}

/// [`set`] for list fields (empty until their key is read).
#[inline(always)]
fn put<T>(slot: &mut Vec<T>, v: Vec<T>) {
    core::mem::forget(core::mem::replace(slot, v));
}

fn err<T>(offset: usize, kind: DecodeErrorKind) -> DR<T> {
    Err(DecodeError { offset, kind })
}

fn req<T>(v: Option<T>, what: &'static str, at: usize) -> DR<T> {
    match v {
        Some(v) => Ok(v),
        None => err(at, DecodeErrorKind::Missing(what)),
    }
}

pub fn decode_with(bytes: &[u8], opts: &DecodeOptions<'_>) -> Result<Definition, DecodeError> {
    // Pass 1: the constant pool (key 17) resolves event types and attrs names.
    let consts = {
        let mut d = D {
            r: Reader::new(bytes),
            strings: opts.strings,
            consts: &[],
            names: Vec::new(),
        };
        let mut found = Vec::new();
        let mut k = d.keys()?;
        while let Some(key) = d.key(&mut k, "top")? {
            if key == 17 {
                found = d.consts()?;
            } else {
                d.r.skip()?;
            }
        }
        d.r.finish()?;
        found
    };
    let mut d = D {
        r: Reader::new(bytes),
        strings: opts.strings,
        consts: &consts,
        names: Vec::new(),
    };
    let def = d.top(&consts)?;
    d.r.finish()?;
    Ok(def)
}

impl D<'_, '_, '_> {
    fn at(&self) -> usize {
        self.r.position()
    }

    fn inv<T>(&self, what: &'static str) -> DR<T> {
        err(self.at(), DecodeErrorKind::Invalid(what))
    }

    fn keys(&mut self) -> DR<Keys> {
        Ok(Keys {
            left: self.r.map()?,
            prev: None,
        })
    }

    /// Next integer key, strictly ascending.
    fn key(&mut self, k: &mut Keys, _map: &'static str) -> DR<Option<u64>> {
        if k.left == 0 {
            return Ok(None);
        }
        k.left -= 1;
        let at = self.at();
        let key = self.r.uint()?;
        if k.prev.is_some_and(|p| p >= key) {
            return err(at, DecodeErrorKind::Cbor(CborErrorKind::KeyOrder));
        }
        k.prev = Some(key);
        Ok(Some(key))
    }

    /// Unknown key: skipped in the extension range, an error otherwise.
    fn other(&mut self, key: u64, map: &'static str) -> DR<()> {
        if is_extension(key) {
            self.r.skip()?;
            Ok(())
        } else {
            err(
                self.at(),
                DecodeErrorKind::UnknownKey {
                    map,
                    key: key.min(u32::MAX as u64) as u32,
                },
            )
        }
    }

    /// Unknown key: kept raw in the extension range, an error otherwise.
    fn keep(&mut self, key: u64, ext: &mut Vec<(u64, Vec<u8>)>, map: &'static str) -> DR<()> {
        if is_extension(key) {
            ext.push((key, self.r.skip()?.to_vec()));
            Ok(())
        } else {
            err(
                self.at(),
                DecodeErrorKind::UnknownKey {
                    map,
                    key: key.min(u32::MAX as u64) as u32,
                },
            )
        }
    }

    fn uint(&mut self, max: u64, what: &'static str) -> DR<u64> {
        let at = self.at();
        let v = self.r.uint()?;
        if v > max {
            return err(at, DecodeErrorKind::Invalid(what));
        }
        Ok(v)
    }

    fn u8_(&mut self) -> DR<u8> {
        Ok(self.uint(0xff, "integer range")? as u8)
    }

    fn u16_(&mut self) -> DR<u16> {
        Ok(self.uint(0xffff, "integer range")? as u16)
    }

    fn u32_(&mut self) -> DR<u32> {
        Ok(self.uint(0xffff_ffff, "integer range")? as u32)
    }

    fn code<T>(&mut self, f: fn(u64) -> Option<T>, what: &'static str) -> DR<T> {
        let at = self.at();
        let v = self.r.uint()?;
        match f(v) {
            Some(x) => Ok(x),
            None => err(at, DecodeErrorKind::Invalid(what)),
        }
    }

    fn bool(&mut self) -> DR<bool> {
        Ok(self.r.bool()?)
    }

    fn text(&mut self) -> DR<String> {
        let at = self.at();
        let i = match self.r.peek()? {
            Head::Text(_) => return Ok(self.r.text()?.to_string()),
            Head::Simple(n) if n < 16 => {
                self.r.head()?;
                n as u64
            }
            Head::Tag(6) => {
                self.r.head()?;
                match self.r.uint()?.checked_add(16) {
                    Some(i) => i,
                    None => return self.inv("string ref"),
                }
            }
            _ => return err(at, DecodeErrorKind::Cbor(CborErrorKind::UnexpectedType)),
        };
        match self.strings.and_then(|t| t.get(i)) {
            Some(s) => Ok(s.to_string()),
            None => err(
                at,
                DecodeErrorKind::StringRef(i.min(u32::MAX as u64) as u32),
            ),
        }
    }

    fn num(&mut self) -> DR<Num> {
        let at = self.at();
        let h = self.r.head()?;
        Ok(match h {
            Head::Uint(v) if v <= i64::MAX as u64 => Num::Int(v as i64),
            Head::Nint(v) if v <= i64::MAX as u64 => Num::Int(-1 - v as i64),
            Head::F16(_) | Head::F32(_) | Head::F64(_) => Num::Float(self.r.float_head(h, at)?),
            Head::Tag(4) => {
                if self.r.array()? != 2 {
                    return self.inv("decimal fraction");
                }
                let e = self.r.int()?;
                let m = self.r.int()?;
                match decimal_value(e, m) {
                    Some(v) => Num::Float(v),
                    None => return self.inv("decimal fraction"),
                }
            }
            _ => return err(at, DecodeErrorKind::Invalid("number")),
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
        let at = self.at();
        let i = self.r.uint()?;
        match self.consts.get(i as usize) {
            Some(Const::Str(s)) => Ok(s.clone()),
            _ => err(at, DecodeErrorKind::Invalid("constant reference")),
        }
    }

    fn consts(&mut self) -> DR<Vec<Const>> {
        let n = self.r.array()?;
        let mut v = Vec::with_capacity(n.min(256));
        for _ in 0..n {
            v.push(match self.r.peek()? {
                Head::Array(_) => {
                    let m = self.r.array()?;
                    let mut pairs = Vec::with_capacity(m.min(256));
                    for _ in 0..m {
                        if self.r.array()? != 2 {
                            return self.inv("lookup map entry");
                        }
                        pairs.push((self.scalar()?, self.scalar()?));
                    }
                    Const::Map(pairs)
                }
                Head::Text(_) | Head::Simple(_) | Head::Tag(6) => Const::Str(self.text()?),
                _ => match self.num()? {
                    Num::Int(i) => Const::Int(i),
                    Num::Float(f) => Const::Float(f),
                },
            });
        }
        Ok(v)
    }

    fn u16s(&mut self) -> DR<Vec<u16>> {
        let n = self.r.array()?;
        let mut v = Vec::with_capacity(n.min(256));
        for _ in 0..n {
            v.push(self.u16_()?);
        }
        Ok(v)
    }

    fn u8s(&mut self) -> DR<Vec<u8>> {
        let n = self.r.array()?;
        let mut v = Vec::with_capacity(n.min(256));
        for _ in 0..n {
            v.push(self.u8_()?);
        }
        Ok(v)
    }

    fn texts(&mut self) -> DR<Vec<String>> {
        let n = self.r.array()?;
        let mut v = Vec::with_capacity(n.min(256));
        for _ in 0..n {
            v.push(self.text()?);
        }
        Ok(v)
    }

    fn name_item(&mut self, packed: bool) -> DR<String> {
        if packed && matches!(self.r.peek()?, Head::Bytes(_)) {
            let at = self.at();
            let b = self.r.bytes()?;
            return match unpack_tuya_name(b) {
                Some(s) => Ok(s),
                None => err(at, DecodeErrorKind::Invalid("packed Tuya name")),
            };
        }
        self.text()
    }

    fn name_match(&mut self, packed: bool) -> DR<NameMatch> {
        Ok(match self.r.peek()? {
            Head::Array(_) => {
                let n = self.r.array()?;
                let mut v = Vec::with_capacity(n.min(256));
                for _ in 0..n {
                    v.push(self.name_item(packed)?);
                }
                NameMatch::Any(v)
            }
            Head::Map(_) => {
                let at = self.at();
                let mut p = None;
                let mut k = self.keys()?;
                while let Some(key) = self.key(&mut k, "name match")? {
                    if key == 18 {
                        p = Some(self.text()?);
                    } else {
                        self.other(key, "name match")?;
                    }
                }
                NameMatch::Prefix(req(p, "prefix", at)?)
            }
            _ => NameMatch::Exact(self.name_item(packed)?),
        })
    }

    fn range(&mut self) -> DR<VersionRange> {
        let mut r = VersionRange::default();
        let mut k = self.keys()?;
        while let Some(key) = self.key(&mut k, "range")? {
            match key {
                60 => set(&mut r.min, self.r.int()?),
                61 => set(&mut r.max, self.r.int()?),
                _ => self.other(key, "range")?,
            }
        }
        Ok(r)
    }

    fn endpoint_match(&mut self) -> DR<EpMatch> {
        let mut m = EpMatch::default();
        let mut k = self.keys()?;
        while let Some(key) = self.key(&mut k, "endpoint match")? {
            match key {
                21 => set(&mut m.device_id, self.u16_()?),
                57 => set(&mut m.profile, self.u16_()?),
                58 => put(&mut m.in_clusters, self.u16s()?),
                59 => put(&mut m.out_clusters, self.u16s()?),
                _ => self.other(key, "endpoint match")?,
            }
        }
        Ok(m)
    }

    fn clause(&mut self) -> DR<MatchClause> {
        let mut c = MatchClause::default();
        let mut k = self.keys()?;
        while let Some(key) = self.key(&mut k, "match")? {
            match key {
                19 => set(&mut c.manufacturer, self.name_match(true)?),
                20 => set(&mut c.model, self.name_match(false)?),
                26 => set(&mut c.gp, self.gp_match()?),
                29 => set(&mut c.power, self.code(Power::from_code, "power")?),
                51 => set(&mut c.mfr_code, self.u16_()?),
                52 => set(&mut c.app_version, self.range()?),
                53 => set(&mut c.stack_version, self.range()?),
                54 => set(&mut c.hw_version, self.range()?),
                55 => set(&mut c.sw_build, self.name_match(false)?),
                190 => set(&mut c.date_code, self.name_match(false)?),
                56 => {
                    let mut eps = self.keys()?;
                    while let Some(ep) = self.key(&mut eps, "endpoints")? {
                        if ep > 255 {
                            return self.inv("endpoint");
                        }
                        let m = self.endpoint_match()?;
                        c.endpoints.push((ep as u8, m));
                    }
                }
                156 => set(
                    &mut c.logical_type,
                    self.code(LogicalType::from_code, "logical_type")?,
                ),
                _ => self.keep(key, &mut c.ext, "match")?,
            }
        }
        Ok(c)
    }

    fn gp_match(&mut self) -> DR<GpMatch> {
        let mut g = GpMatch::default();
        let mut k = self.keys()?;
        while let Some(key) = self.key(&mut k, "gp match")? {
            match key {
                7 => {
                    let mut c = GpClusters::default();
                    let mut k2 = self.keys()?;
                    while let Some(key2) = self.key(&mut k2, "gp clusters")? {
                        match key2 {
                            137 => put(&mut c.server, self.u16s()?),
                            138 => put(&mut c.client, self.u16s()?),
                            _ => self.other(key2, "gp clusters")?,
                        }
                    }
                    g.clusters = Some(c);
                }
                21 => set(&mut g.device_id, self.u8_()?),
                22 => set(&mut g.manufacturer_id, self.u16_()?),
                23 => {
                    g.model_id = Some(if matches!(self.r.peek()?, Head::Map(_)) {
                        let at = self.at();
                        let (mut v, mut m) = (None, None);
                        let mut k2 = self.keys()?;
                        while let Some(key2) = self.key(&mut k2, "model_id")? {
                            match key2 {
                                8 => v = Some(self.u16_()?),
                                25 => m = Some(self.u16_()?),
                                _ => self.other(key2, "model_id")?,
                            }
                        }
                        ModelId {
                            value: req(v, "model_id.value", at)?,
                            mask: Some(req(m, "model_id.mask", at)?),
                        }
                    } else {
                        ModelId {
                            value: self.u16_()?,
                            mask: None,
                        }
                    })
                }
                24 => {
                    if self.r.array()? != 2 {
                        return self.inv("src_id");
                    }
                    g.src_id = Some((self.u32_()?, self.u32_()?));
                }
                135 => set(&mut g.ieee, self.text()?),
                136 => put(&mut g.commands, self.u8s()?),
                _ => self.other(key, "gp match")?,
            }
        }
        Ok(g)
    }

    fn white_label(&mut self) -> DR<WhiteLabel> {
        let mut w = WhiteLabel::default();
        let mut k = self.keys()?;
        while let Some(key) = self.key(&mut k, "white_label")? {
            match key {
                19 => {
                    let n = self.r.array()?;
                    for _ in 0..n {
                        let at = self.at();
                        let i = self.r.uint()? as usize;
                        match self.names.get(i) {
                            Some(name) => w.manufacturer.push(name.clone()),
                            None => {
                                return err(
                                    at,
                                    DecodeErrorKind::Invalid("white_label manufacturer index"),
                                );
                            }
                        }
                    }
                }
                20 => set(&mut w.model, self.text()?),
                43 => set(&mut w.vendor, self.text()?),
                // keys::WHITE_LABEL_EXT: hubs before it skip the key.
                64 => {
                    let n = self.r.array()?;
                    for _ in 0..n {
                        w.model_id.push(self.text()?);
                    }
                }
                _ => self.other(key, "white_label")?,
            }
        }
        Ok(w)
    }

    fn device(&mut self) -> DR<Device> {
        let mut v = Device::default();
        let mut k = self.keys()?;
        while let Some(key) = self.key(&mut k, "device")? {
            match key {
                29 => set(&mut v.power, self.code(Power::from_code, "power")?),
                30 => set(&mut v.sleepy, self.bool()?),
                31 => {
                    v.battery = Some(if matches!(self.r.peek()?, Head::Map(_)) {
                        let at = self.at();
                        let mut lin = None;
                        let mut k2 = self.keys()?;
                        while let Some(key2) = self.key(&mut k2, "battery")? {
                            if key2 != 32 {
                                self.other(key2, "battery")?;
                                continue;
                            }
                            if self.r.array()? != 2 {
                                return self.inv("voltage_to_percent");
                            }
                            lin = Some(Battery::Linear {
                                min_mv: self.u32_()?,
                                max_mv: self.u32_()?,
                            });
                        }
                        req(lin, "voltage_to_percent", at)?
                    } else {
                        Battery::Curve(self.code(BatteryCurve::from_code, "battery curve")?)
                    })
                }
                33 => set(&mut v.checkin_s, self.u32_()?),
                131 => {
                    let mut a = Availability::default();
                    let mut k2 = self.keys()?;
                    while let Some(key2) = self.key(&mut k2, "availability")? {
                        if key2 == 132 {
                            a.timeout_s = Some(self.u32_()?);
                        } else {
                            self.other(key2, "availability")?;
                        }
                    }
                    v.availability = Some(a);
                }
                133 => set(&mut v.ota, self.bool()?),
                _ => self.other(key, "device")?,
            }
        }
        Ok(v)
    }

    fn generic(&mut self) -> DR<Generic> {
        let mut g = Generic::default();
        let mut k = self.keys()?;
        while let Some(key) = self.key(&mut k, "generic")? {
            match key {
                34 => put(&mut g.exclude, self.texts()?),
                35 => {
                    let n = self.r.map()?;
                    let mut prev: &[u8] = &[];
                    for _ in 0..n {
                        let at = self.at();
                        let before = self.r.remaining();
                        let name = self.r.text()?.to_string();
                        let enc = &before[..before.len() - self.r.remaining().len()];
                        if !prev.is_empty() && prev >= enc {
                            return err(at, DecodeErrorKind::Cbor(CborErrorKind::KeyOrder));
                        }
                        prev = enc;
                        g.patch.push((name, self.entity()?));
                    }
                }
                36 => set(&mut g.disable, self.bool()?),
                37 => put(&mut g.only_endpoints, self.u8s()?),
                _ => self.other(key, "generic")?,
            }
        }
        Ok(g)
    }

    fn tuya(&mut self) -> DR<Tuya> {
        let mut t = Tuya::default();
        let mut q = TuyaQuery::default();
        let mut has_q = false;
        let mut k = self.keys()?;
        while let Some(key) = self.key(&mut k, "tuya")? {
            match key {
                38 => set(
                    &mut t.time_sync,
                    self.code(TimeSync::from_code, "time_sync")?,
                ),
                39 => set(&mut t.magic_packet, self.bool()?),
                40 => {
                    let at = self.at();
                    let m = self.r.uint()?;
                    if m == 0 || m > 3 {
                        return err(at, DecodeErrorKind::Invalid("query"));
                    }
                    q.configure = m & 1 != 0;
                    q.announce = m & 2 != 0;
                    has_q = true;
                }
                139 => set(&mut t.force_time_sync_s, self.u32_()?),
                140 => set(&mut t.bind_basic, self.bool()?),
                141 => set(&mut t.gateway_status, self.bool()?),
                142 => set(&mut t.send, self.code(TuyaSend::from_code, "send")?),
                143 => set(&mut t.mcu_version, self.bool()?),
                144 => set(&mut t.optimistic, self.bool()?),
                145 => set(&mut t.dedup, self.bool()?),
                146 => {
                    q.interval_s = Some(self.u32_()?);
                    has_q = true;
                }
                _ => self.other(key, "tuya")?,
            }
        }
        if has_q {
            t.query = Some(q);
        }
        Ok(t)
    }

    fn report(&mut self) -> DR<Report> {
        if matches!(self.r.peek()?, Head::False) {
            self.r.bool()?;
            return Ok(Report::Off);
        }
        let n = self.r.array()?;
        if !(2..=3).contains(&n) {
            return self.inv("report");
        }
        let min = self.u16_()?;
        let max = self.u16_()?;
        let change = if n == 3 { Some(self.num()?) } else { None };
        Ok(Report::Config { min, max, change })
    }

    /// Fields shared by attribute and command references.
    fn attr_ref(&mut self, what: &'static str) -> DR<AttrRef> {
        let at = self.at();
        let mut a = AttrRef::default();
        let mut k = self.keys()?;
        while let Some(key) = self.key(&mut k, what)? {
            match key {
                0 => set(&mut a.cluster, self.u16_()?),
                1 => set(&mut a.attr, self.u16_()?),
                2 => set(&mut a.ty, self.u8_()?),
                3 => set(&mut a.mfr, self.u16_()?),
                4 => set(&mut a.ep, self.u8_()?),
                5 => set(&mut a.id, self.u8_()?),
                11 => set(&mut a.cmd_id, self.u8_()?),
                6 => put(&mut a.attrs, self.u16s()?),
                7 => put(&mut a.clusters, self.u16s()?),
                8 => set(&mut a.value, self.scalar()?),
                9 => set(&mut a.codec, self.code(CodecName::from_code, "codec")?),
                10 => set(&mut a.tag, self.r.int()?),
                12 => set(&mut a.report, self.report()?),
                62 => set(&mut a.payload, self.expr()?),
                128 => set(&mut a.group, self.u16_()?),
                130 => set(&mut a.interval, self.u32_()?),
                _ => self.other(key, what)?,
            }
        }
        a.at = at;
        Ok(a)
    }

    fn write_attr(&mut self) -> DR<WriteAttr> {
        let a = self.attr_ref("write")?;
        Ok(WriteAttr {
            ep: a.ep,
            cluster: req(a.cluster, "write.cluster", a.at)?,
            attr: req(a.attr, "write.attr", a.at)?,
            zcl_type: req(a.ty, "write.type", a.at)?,
            value: req(a.value, "write.value", a.at)?,
            mfr: a.mfr,
        })
    }

    fn cmd_ref(&mut self) -> DR<CmdRef> {
        let a = self.attr_ref("cmd")?;
        Ok(CmdRef {
            cluster: req(a.cluster, "cmd.cluster", a.at)?,
            id: req(a.id, "cmd.id", a.at)?,
            mfr: a.mfr,
            ep: a.ep,
        })
    }

    fn configure_op(&mut self) -> DR<ConfigureOp> {
        let at = self.at();
        if self.r.map()? != 1 {
            return err(at, DecodeErrorKind::Invalid("configure op"));
        }
        let kind = self.r.uint()?;
        let a = self.attr_ref("configure op")?;
        Ok(match kind {
            11 => ConfigureOp::Cmd {
                ep: a.ep,
                cluster: req(a.cluster, "cmd.cluster", a.at)?,
                id: req(a.id, "cmd.id", a.at)?,
                mfr: a.mfr,
                payload: a.payload,
            },
            12 => ConfigureOp::Report {
                ep: a.ep,
                cluster: req(a.cluster, "report.cluster", a.at)?,
                attr: req(a.attr, "report.attr", a.at)?,
                zcl_type: req(a.ty, "report.type", a.at)?,
                report: req(a.report, "report.report", a.at)?,
                mfr: a.mfr,
            },
            16 => ConfigureOp::Write(WriteAttr {
                ep: a.ep,
                cluster: req(a.cluster, "write.cluster", a.at)?,
                attr: req(a.attr, "write.attr", a.at)?,
                zcl_type: req(a.ty, "write.type", a.at)?,
                value: req(a.value, "write.value", a.at)?,
                mfr: a.mfr,
            }),
            27 => ConfigureOp::Bind {
                ep: a.ep,
                clusters: a.clusters,
            },
            28 => ConfigureOp::Read {
                ep: a.ep,
                cluster: req(a.cluster, "read.cluster", a.at)?,
                attrs: a.attrs,
                mfr: a.mfr,
            },
            128 => ConfigureOp::Group {
                ep: a.ep,
                group: req(a.group, "group.group", a.at)?,
            },
            129 => ConfigureOp::Checkin {
                interval_qs: req(a.interval, "checkin.interval_qs", a.at)?,
            },
            _ => {
                return err(
                    at,
                    DecodeErrorKind::UnknownKey {
                        map: "configure op",
                        key: kind.min(u32::MAX as u64) as u32,
                    },
                );
            }
        })
    }

    fn gp_device(&mut self) -> DR<GpDevice> {
        let mut g = GpDevice::default();
        let mut k = self.keys()?;
        while let Some(key) = self.key(&mut k, "gp")? {
            match key {
                41 => set(&mut g.security_min, self.u8_()?),
                42 => set(&mut g.report_interval_s, self.u32_()?),
                134 => put(&mut g.ignore_commands, self.u8s()?),
                _ => self.other(key, "gp")?,
            }
        }
        Ok(g)
    }

    fn ignore_rule(&mut self) -> DR<IgnoreRule> {
        let a = self.attr_ref("ignore")?;
        let mut cmd = None;
        // `cmd` (11) of an ignore rule is a plain command ID
        if let Some(c) = a.cmd_id {
            cmd = Some(c);
        }
        Ok(IgnoreRule {
            ep: a.ep,
            cluster: req(a.cluster, "ignore.cluster", a.at)?,
            cmd,
        })
    }

    fn tuya_dp(&mut self) -> DR<TuyaDp> {
        if self.r.array()? != 2 {
            return self.inv("tuya");
        }
        let dp = self.u8_()?;
        let ty = self.code(TuyaType::from_code, "tuya type")?;
        Ok(TuyaDp { dp, ty })
    }

    fn named_exprs(&mut self) -> DR<Vec<(String, Expr)>> {
        let mut out = Vec::new();
        let mut k = self.keys()?;
        while k.left > 0 {
            // keys are constant indices; peek to keep the order check
            let mut probe = self.r.clone();
            let i = probe.uint()?;
            if k.prev.is_some_and(|p| p >= i) {
                return err(self.at(), DecodeErrorKind::Cbor(CborErrorKind::KeyOrder));
            }
            k.prev = Some(i);
            k.left -= 1;
            let name = self.const_str()?;
            out.push((name, self.expr()?));
        }
        Ok(out)
    }

    fn codec(&mut self) -> DR<Codec> {
        let at = self.at();
        if !matches!(self.r.peek()?, Head::Map(_)) {
            return Ok(match self.code(CodecName::from_code, "codec")? {
                CodecName::ZclTagged => Codec::ZclTagged,
                CodecName::BcdTime => Codec::BcdTime,
                CodecName::Hhmm => Codec::Hhmm,
                CodecName::Ascii => Codec::Ascii,
                CodecName::Utf16beHex => Codec::Utf16beHex,
                _ => return err(at, DecodeErrorKind::Invalid("codec parameters missing")),
            });
        }
        if self.r.map()? != 1 {
            return err(at, DecodeErrorKind::Invalid("codec"));
        }
        let name = self.code(CodecName::from_code, "codec")?;
        let pat = self.at();
        if name == CodecName::BitmapFlags {
            let mut bits = Vec::new();
            let mut k = self.keys()?;
            while let Some(b) = self.key(&mut k, "bitmap_flags")? {
                if b > 63 {
                    return self.inv("bitmap_flags bit");
                }
                bits.push((b as u8, self.text()?));
            }
            return Ok(Codec::BitmapFlags(bits));
        }
        let (mut day, mut tr, mut variant, mut scale, mut min, mut max) =
            (None, None, None, None, None, None);
        let mut k = self.keys()?;
        while let Some(key) = self.key(&mut k, "codec parameters")? {
            match key {
                45 => day = Some(self.u8_()?),
                46 => tr = Some(self.u8_()?),
                60 => min = Some(self.r.int()? as i16),
                61 => max = Some(self.r.int()? as i16),
                150 => variant = Some(self.code(ScheduleVariant::from_code, "variant")?),
                159 => scale = Some(self.u16_()?),
                _ => self.other(key, "codec parameters")?,
            }
        }
        Ok(match name {
            CodecName::TuyaScheduleDay => Codec::TuyaScheduleDay {
                day: req(day, "day", pat)?,
                transitions: req(tr, "transitions", pat)?,
                variant: variant.unwrap_or(ScheduleVariant::MultiDp),
            },
            CodecName::TuyaScheduleWeek => Codec::TuyaScheduleWeek(ScheduleWeek {
                variant: req(variant, "variant", pat)?,
                transitions: tr,
                scale,
                min,
                max,
            }),
            _ => return err(at, DecodeErrorKind::Invalid("codec parameters")),
        })
    }

    fn table_ref(&mut self, len: usize) -> DR<TableRef> {
        let c = self.u16_()?;
        Ok(if (c as usize) < len {
            TableRef::Known(c)
        } else {
            TableRef::Unknown(c)
        })
    }

    fn gp_binding(&mut self) -> DR<GpBinding> {
        let at = self.at();
        if self.r.map()? != 1 {
            return err(at, DecodeErrorKind::Invalid("gp binding"));
        }
        Ok(match self.r.uint()? {
            11 => GpBinding::Cmd(self.u8_()?),
            12 => {
                let a = self.attr_ref("gp report")?;
                GpBinding::Report {
                    cluster: req(a.cluster, "gp.report.cluster", a.at)?,
                    attr: req(a.attr, "gp.report.attr", a.at)?,
                    zcl_type: req(a.ty, "gp.report.type", a.at)?,
                    mfr: a.mfr,
                }
            }
            _ => return err(at, DecodeErrorKind::Invalid("gp binding")),
        })
    }

    fn trigger(&mut self) -> DR<Trigger> {
        let at = self.at();
        let mut t = Trigger {
            attrs: Vec::new(),
            cmd: None,
            gp_cmd: None,
            tuya: None,
            when: None,
            event: Expr::default(),
        };
        let mut has_event = false;
        let mut k = self.keys()?;
        while let Some(key) = self.key(&mut k, "trigger")? {
            match key {
                6 => put(&mut t.attrs, self.named_exprs()?),
                11 => set(&mut t.cmd, self.cmd_ref()?),
                13 => set(&mut t.gp_cmd, self.u8_()?),
                14 => set(&mut t.when, self.expr()?),
                15 => {
                    t.event = self.expr()?;
                    has_event = true;
                }
                147 => set(&mut t.tuya, self.tuya_dp()?),
                _ => self.other(key, "trigger")?,
            }
        }
        if !has_event {
            return err(at, DecodeErrorKind::Missing("trigger.event"));
        }
        Ok(t)
    }

    fn press(&mut self) -> DR<Press> {
        let at = self.at();
        let (mut cmd, mut payload, mut write, mut tuya, mut value) = (None, None, None, None, None);
        let mut k = self.keys()?;
        while let Some(key) = self.key(&mut k, "press")? {
            match key {
                8 => value = Some(self.scalar()?),
                11 => cmd = Some(self.cmd_ref()?),
                16 => write = Some(self.write_attr()?),
                62 => payload = Some(self.expr()?),
                147 => tuya = Some(self.tuya_dp()?),
                _ => self.other(key, "press")?,
            }
        }
        Ok(match (cmd, write, tuya) {
            (Some(cmd), None, None) if value.is_none() => Press::Cmd { cmd, payload },
            (None, Some(w), None) if value.is_none() && payload.is_none() => Press::Write(w),
            (None, Some(_), None) | (Some(_), None, None) => {
                return err(at, DecodeErrorKind::Invalid("press"));
            }
            (None, None, Some(dp)) if payload.is_none() => Press::Tuya {
                dp,
                value: req(value, "press.value", at)?,
            },
            _ => return err(at, DecodeErrorKind::Invalid("press")),
        })
    }

    fn entity(&mut self) -> DR<Entity> {
        let mut e = Entity::default();
        let mut k = self.keys()?;
        while let Some(key) = self.key(&mut k, "entity")? {
            match key {
                0 => set(&mut e.key, self.text()?),
                1 => set(&mut e.platform, self.code(Platform::from_code, "platform")?),
                2 => set(
                    &mut e.device_class,
                    self.table_ref(enums::DEVICE_CLASSES.len())?,
                ),
                3 => set(&mut e.unit, self.table_ref(enums::UNITS.len())?),
                4 => set(
                    &mut e.state_class,
                    self.code(StateClass::from_code, "state_class")?,
                ),
                5 => set(&mut e.category, self.code(Category::from_code, "category")?),
                6 => set(&mut e.more_mut().name, self.text()?),
                7 => set(&mut e.ep, self.u8_()?),
                8 => {
                    let a = self.attr_ref("zcl")?;
                    e.zcl = Some(ZclBinding {
                        cluster: req(a.cluster, "zcl.cluster", a.at)?,
                        attr: req(a.attr, "zcl.attr", a.at)?,
                        zcl_type: req(a.ty, "zcl.type", a.at)?,
                        mfr: a.mfr,
                        ep: a.ep,
                    });
                }
                9 => set(&mut e.tuya, self.tuya_dp()?),
                10 => {
                    let a = self.attr_ref("struct")?;
                    e.more_mut().struct_ = Some(StructBinding {
                        cluster: req(a.cluster, "struct.cluster", a.at)?,
                        attr: req(a.attr, "struct.attr", a.at)?,
                        mfr: a.mfr,
                        ep: a.ep,
                        codec: req(a.codec, "struct.codec", a.at)?,
                        tag: a.tag,
                    });
                }
                11 => set(&mut e.gp, self.gp_binding()?),
                12 => set(&mut e.from, self.expr()?),
                13 => set(&mut e.more_mut().to, self.expr()?),
                14 => set(&mut e.mul, self.num()?),
                15 => set(&mut e.div, self.num()?),
                16 => set(&mut e.offset, self.num()?),
                17 => set(&mut e.precision, self.u8_()?),
                18 => set(&mut e.invalid, self.num()?),
                19 => {
                    let n = self.r.array()?;
                    if n % 2 != 0 {
                        return self.inv("map");
                    }
                    for _ in 0..n / 2 {
                        let raw = self.r.int()?;
                        e.map.push((raw, self.text()?));
                    }
                }
                20 => set(&mut e.more_mut().value_on, self.scalar()?),
                21 => set(&mut e.more_mut().value_off, self.scalar()?),
                22 => set(&mut e.min, self.num()?),
                23 => set(&mut e.max, self.num()?),
                24 => set(&mut e.step, self.num()?),
                25 => set(&mut e.more_mut().mode, self.code(Mode::from_code, "mode")?),
                26 => set(&mut e.access, self.u8_()?),
                27 => set(&mut e.report, self.report()?),
                28 => set(&mut e.enabled, self.bool()?),
                29 => set(&mut e.more_mut().reset_after, self.expr()?),
                30 => {
                    let mut pk = self.keys()?;
                    while let Some(part) = self.key(&mut pk, "parts")? {
                        if !crate::keys::sub_name(part)
                            .is_some_and(|n| crate::keys::PART_NAMES.contains(&n))
                        {
                            return self.inv("part name");
                        }
                        let p = self.entity()?;
                        e.more_mut().parts.push((part as u16, p));
                    }
                }
                31 => {
                    let n = self.r.array()?;
                    for _ in 0..n {
                        let t = self.const_str()?;
                        e.more_mut().event_types.push(t);
                    }
                }
                32 => {
                    let n = self.r.array()?;
                    for _ in 0..n {
                        let t = self.trigger()?;
                        e.more_mut().triggers.push(t);
                    }
                }
                33 => set(&mut e.more_mut().press, self.press()?),
                34 => set(&mut e.more_mut().codec, self.codec()?),
                35 => set(&mut e.more_mut().present_if, self.expr()?),
                36 => put(&mut e.more_mut().attrs, self.named_exprs()?),
                37 => {
                    let mut h = HubBinding { default: None };
                    let mut k2 = self.keys()?;
                    while let Some(key2) = self.key(&mut k2, "hub")? {
                        if key2 == 63 {
                            h.default = Some(self.scalar()?);
                        } else {
                            self.other(key2, "hub")?;
                        }
                    }
                    e.more_mut().hub = Some(h);
                }
                38 => set(&mut e.more_mut().invert, self.bool()?),
                _ => {
                    if is_extension(key) {
                        let raw = self.r.skip()?.to_vec();
                        e.more_mut().ext.push((key, raw));
                    } else {
                        return err(
                            self.at(),
                            DecodeErrorKind::UnknownKey {
                                map: "entity",
                                key: key.min(u32::MAX as u64) as u32,
                            },
                        );
                    }
                }
            }
        }
        Ok(e)
    }

    fn template(&mut self) -> DR<Template> {
        let at = self.at();
        let (mut name, mut params) = (None, None);
        let mut k = self.keys()?;
        while let Some(key) = self.key(&mut k, "template")? {
            match key {
                157 => name = Some(self.code(TemplateName::from_code, "template name")?),
                158 => {
                    if !matches!(self.r.peek()?, Head::Map(_)) {
                        return self.inv("template params");
                    }
                    params = Some(self.r.skip()?.to_vec());
                }
                _ => self.other(key, "template")?,
            }
        }
        Ok(Template {
            name: req(name, "template name", at)?,
            params,
        })
    }

    fn top(&mut self, consts: &[Const]) -> DR<Definition> {
        let mut def = Definition::new(Kind::Definition);
        let mut have_kind = false;
        let mut k = self.keys()?;
        while let Some(key) = self.key(&mut k, "top")? {
            match key {
                0 | 20 => {
                    let at = self.at();
                    let m = self.u8_()?;
                    if have_kind {
                        return self.inv("zdef");
                    }
                    have_kind = true;
                    def.kind = if key == 0 {
                        Kind::Definition
                    } else {
                        Kind::Fragment
                    };
                    def.major = m;
                    if !SUPPORTED_MAJORS.contains(&m) {
                        return err(at, DecodeErrorKind::UnsupportedMajor(m));
                    }
                }
                1 => set(&mut def.id, self.text()?),
                2 => set(&mut def.revision, self.u32_()?),
                3 => set(&mut def.vendor, self.text()?),
                4 => set(&mut def.model, self.text()?),
                5 => set(&mut def.description, self.text()?),
                6 => {
                    let n = self.r.array()?;
                    for _ in 0..n {
                        let c = self.clause()?;
                        def.match_.push(c);
                    }
                    self.names = def.match_names().into_iter().map(String::from).collect();
                }
                7 => {
                    let n = self.r.array()?;
                    for _ in 0..n {
                        let w = self.white_label()?;
                        def.white_label.push(w);
                    }
                }
                8 => put(&mut def.requires, self.u16s()?),
                9 => set(&mut def.device, self.device()?),
                10 => set(&mut def.generic, self.generic()?),
                11 => {
                    let mut eps = self.keys()?;
                    while let Some(ep) = self.key(&mut eps, "endpoints")? {
                        if ep > 255 {
                            return self.inv("endpoint");
                        }
                        let n = self.text()?;
                        def.endpoints.push((ep as u8, n));
                    }
                }
                12 => set(&mut def.tuya, self.tuya()?),
                13 => {
                    let n = self.r.array()?;
                    for _ in 0..n {
                        let op = self.configure_op()?;
                        def.configure.push(op);
                    }
                }
                14 => {
                    let n = self.r.array()?;
                    for _ in 0..n {
                        let e = self.entity()?;
                        def.entities.push(e);
                    }
                }
                15 => set(&mut def.gp, self.gp_device()?),
                16 => {
                    let n = self.r.array()?;
                    for _ in 0..n {
                        let r = self.ignore_rule()?;
                        def.ignore.push(r);
                    }
                }
                17 => {
                    self.r.skip()?;
                    def.consts = consts.to_vec();
                }
                18 => set(&mut def.source, self.text()?),
                19 => set(&mut def.mfr, self.u16_()?),
                21 => {
                    let n = self.r.array()?;
                    for _ in 0..n {
                        let t = self.template()?;
                        def.templates.push(t);
                    }
                }
                22 => set(&mut def.priority, self.r.int()?),
                23 => set(
                    &mut def.self_description,
                    self.code(SelfDescription::from_code, "self_description")?,
                ),
                31 => set(&mut def.x_wasm, self.r.skip()?.to_vec()),
                _ => self.keep(key, &mut def.ext, "top")?,
            }
        }
        if !have_kind {
            return err(0, DecodeErrorKind::Missing("zdef"));
        }
        Ok(def)
    }
}

/// Union of the sub-map fields of attribute, command and op references.
#[derive(Default)]
struct AttrRef {
    at: usize,
    cluster: Option<u16>,
    attr: Option<u16>,
    ty: Option<u8>,
    mfr: Option<u16>,
    ep: Option<u8>,
    id: Option<u8>,
    cmd_id: Option<u8>,
    attrs: Vec<u16>,
    clusters: Vec<u16>,
    value: Option<Scalar>,
    codec: Option<CodecName>,
    tag: Option<i64>,
    report: Option<Report>,
    payload: Option<Expr>,
    group: Option<u16>,
    interval: Option<u32>,
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
    fn uint(&mut self, k: u16, v: Option<u64>) {
        if let Some(v) = v {
            self.put(k).uint(v);
        }
    }
    fn finish(mut self, w: &mut Writer) {
        crate::small_sort::sort_by_key(&mut self.e, |x| x.0);
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

fn ext(m: &mut M, ext: &[(u64, Vec<u8>)]) -> ER {
    for (k, v) in ext {
        if !is_extension(*k) {
            return Err(EncodeError::Invalid("extension key"));
        }
        m.raw(*k, v);
    }
    Ok(())
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
        m.uint(2, d.revision.map(u64::from));
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
                self.clause(w, c)?;
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
                if !wl.model_id.is_empty() {
                    let w = mm.put(64);
                    w.array(wl.model_id.len());
                    for n in &wl.model_id {
                        w.text(n);
                    }
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
            crate::small_sort::sort_by_key(&mut eps, |e| e.0);
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
            mm.uint(41, g.security_min.map(u64::from));
            mm.uint(42, g.report_interval_s.map(u64::from));
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
                mm.uint(4, r.ep.map(u64::from));
                mm.uint(11, r.cmd.map(u64::from));
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
        m.uint(19, d.mfr.map(u64::from));
        if !d.templates.is_empty() {
            let w = m.put(21);
            w.array(d.templates.len());
            for t in &d.templates {
                let mut mm = M::new();
                mm.put(157).uint(t.name.code() as u64);
                if let Some(p) = &t.params {
                    mm.raw(158, p);
                }
                mm.finish(w);
            }
        }
        if let Some(v) = d.priority {
            m.put(22).int(v);
        }
        m.uint(23, d.self_description.map(|v| v.code() as u64));
        if let Some(v) = &d.x_wasm {
            m.raw(31, v);
        }
        ext(&mut m, &d.ext)?;
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

    fn clause(&self, w: &mut Writer, c: &MatchClause) -> ER {
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
            mm.uint(21, g.device_id.map(u64::from));
            mm.uint(22, g.manufacturer_id.map(u64::from));
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
        m.uint(29, c.power.map(|p| p.code() as u64));
        m.uint(51, c.mfr_code.map(u64::from));
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
        if let Some(n) = &c.date_code {
            self.name_match(m.put(190), n, false);
        }
        if !c.endpoints.is_empty() {
            let w = m.put(56);
            let mut eps: Vec<&(u8, EpMatch)> = c.endpoints.iter().collect();
            crate::small_sort::sort_by_key(&mut eps, |e| e.0);
            w.map(eps.len());
            for (ep, em) in eps {
                w.uint(*ep as u64);
                let mut mm = M::new();
                mm.uint(21, em.device_id.map(u64::from));
                mm.uint(57, em.profile.map(u64::from));
                if !em.in_clusters.is_empty() {
                    uint_list(mm.put(58), &em.in_clusters);
                }
                if !em.out_clusters.is_empty() {
                    uint_list(mm.put(59), &em.out_clusters);
                }
                mm.finish(w);
            }
        }
        m.uint(156, c.logical_type.map(|v| v.code() as u64));
        ext(&mut m, &c.ext)?;
        m.finish(w);
        Ok(())
    }

    fn device(&self, w: &mut Writer, d: &Device) {
        let mut m = M::new();
        m.uint(29, d.power.map(|p| p.code() as u64));
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
            Some(Battery::Curve(c)) => m.put(31).uint(c.code() as u64),
            None => {}
        }
        m.uint(33, d.checkin_s.map(u64::from));
        if let Some(a) = &d.availability {
            let mut mm = M::new();
            mm.uint(132, a.timeout_s.map(u64::from));
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
            crate::small_sort::sort_by(&mut entries, |a, b| a.0 < b.0);
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
        m.uint(38, t.time_sync.map(|v| v.code() as u64));
        if let Some(v) = t.magic_packet {
            m.put(39).bool(v);
        }
        if let Some(q) = &t.query {
            let mask = q.configure as u64 | (q.announce as u64) << 1;
            if mask != 0 {
                m.put(40).uint(mask);
            }
            m.uint(146, q.interval_s.map(u64::from));
        }
        m.uint(139, t.force_time_sync_s.map(u64::from));
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
        m.uint(142, t.send.map(|v| v.code() as u64));
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
        m.uint(3, a.mfr.map(u64::from));
        m.uint(4, a.ep.map(u64::from));
        scalar(m.put(8), &a.value);
        m.finish(w);
    }

    fn cmd_ref(&self, w: &mut Writer, c: &CmdRef, payload: Option<&Expr>) {
        let mut m = M::new();
        m.put(0).uint(c.cluster as u64);
        m.uint(3, c.mfr.map(u64::from));
        m.uint(4, c.ep.map(u64::from));
        m.put(5).uint(c.id as u64);
        if let Some(p) = payload {
            m.put(62).bytes(&p.0);
        }
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
                payload,
            } => self.cmd_ref(
                outer.put(11),
                &CmdRef {
                    cluster: *cluster,
                    id: *id,
                    mfr: *mfr,
                    ep: *ep,
                },
                payload.as_ref(),
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
                m.uint(3, mfr.map(u64::from));
                m.uint(4, ep.map(u64::from));
                self.report(m.put(12), report);
                m.finish(outer.put(12));
            }
            ConfigureOp::Write(a) => self.write_attr(outer.put(16), a),
            ConfigureOp::Bind { ep, clusters } => {
                let mut m = M::new();
                m.uint(4, ep.map(u64::from));
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
                m.uint(3, mfr.map(u64::from));
                m.uint(4, ep.map(u64::from));
                if !attrs.is_empty() {
                    uint_list(m.put(6), attrs);
                }
                m.finish(outer.put(28));
            }
            ConfigureOp::Group { ep, group } => {
                let mut m = M::new();
                m.uint(4, ep.map(u64::from));
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
        crate::small_sort::sort_by_key(&mut entries, |x| x.0);
        w.map(entries.len());
        for (i, e) in entries {
            w.uint(i);
            w.bytes(&e.0);
        }
        Ok(())
    }

    fn codec(&self, w: &mut Writer, c: &Codec) {
        let code = c.kind().code() as u64;
        match c {
            Codec::TuyaScheduleDay {
                day,
                transitions,
                variant,
            } => {
                w.map(1);
                w.uint(code);
                let mut m = M::new();
                m.put(45).uint(*day as u64);
                m.put(46).uint(*transitions as u64);
                if *variant != ScheduleVariant::MultiDp {
                    m.put(150).uint(variant.code() as u64);
                }
                m.finish(w);
            }
            Codec::TuyaScheduleWeek(s) => {
                w.map(1);
                w.uint(code);
                let mut m = M::new();
                m.uint(46, s.transitions.map(u64::from));
                if let Some(v) = s.min {
                    m.put(60).int(v as i64);
                }
                if let Some(v) = s.max {
                    m.put(61).int(v as i64);
                }
                m.put(150).uint(s.variant.code() as u64);
                m.uint(159, s.scale.map(u64::from));
                m.finish(w);
            }
            Codec::BitmapFlags(bits) => {
                let mut b = bits.clone();
                crate::small_sort::sort_by_key(&mut b, |x| x.0);
                w.map(1);
                w.uint(code);
                w.map(b.len());
                for (bit, label) in b {
                    w.uint(bit as u64);
                    w.text(&label);
                }
            }
            _ => w.uint(code),
        }
    }

    fn entity(&self, w: &mut Writer, e: &Entity) -> ER {
        let x = e.more();
        let mut m = M::new();
        if let Some(v) = &e.key {
            m.put(0).text(v);
        }
        m.uint(1, e.platform.map(|v| v.code() as u64));
        m.uint(2, e.device_class.map(|v| v.code() as u64));
        m.uint(3, e.unit.map(|v| v.code() as u64));
        m.uint(4, e.state_class.map(|v| v.code() as u64));
        m.uint(5, e.category.map(|v| v.code() as u64));
        if let Some(v) = &x.name {
            m.put(6).text(v);
        }
        m.uint(7, e.ep.map(u64::from));
        if let Some(z) = &e.zcl {
            let mut mm = M::new();
            mm.put(0).uint(z.cluster as u64);
            mm.put(1).uint(z.attr as u64);
            mm.put(2).uint(z.zcl_type as u64);
            mm.uint(3, z.mfr.map(u64::from));
            mm.uint(4, z.ep.map(u64::from));
            mm.finish(m.put(8));
        }
        if let Some(t) = &e.tuya {
            self.tuya_dp(m.put(9), t);
        }
        if let Some(s) = &x.struct_ {
            let mut mm = M::new();
            mm.put(0).uint(s.cluster as u64);
            mm.put(1).uint(s.attr as u64);
            mm.uint(3, s.mfr.map(u64::from));
            mm.uint(4, s.ep.map(u64::from));
            mm.put(9).uint(s.codec.code() as u64);
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
                    mm.uint(3, mfr.map(u64::from));
                    mm.finish(w);
                }
            }
        }
        if let Some(v) = &e.from {
            m.put(12).bytes(&v.0);
        }
        if let Some(v) = &x.to {
            m.put(13).bytes(&v.0);
        }
        for (k, v) in [(14, e.mul), (15, e.div), (16, e.offset)] {
            if let Some(v) = v {
                write_num(m.put(k), v);
            }
        }
        m.uint(17, e.precision.map(u64::from));
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
        if let Some(v) = &x.value_on {
            scalar(m.put(20), v);
        }
        if let Some(v) = &x.value_off {
            scalar(m.put(21), v);
        }
        for (k, v) in [(22, e.min), (23, e.max), (24, e.step)] {
            if let Some(v) = v {
                write_num(m.put(k), v);
            }
        }
        m.uint(25, x.mode.map(|v| v.code() as u64));
        m.uint(26, e.access.map(u64::from));
        if let Some(r) = &e.report {
            self.report(m.put(27), r);
        }
        if let Some(v) = e.enabled {
            m.put(28).bool(v);
        }
        if let Some(v) = &x.reset_after {
            m.put(29).bytes(&v.0);
        }
        if !x.parts.is_empty() {
            let mut parts: Vec<&(u16, Entity)> = x.parts.iter().collect();
            crate::small_sort::sort_by_key(&mut parts, |p| p.0);
            let w = m.put(30);
            w.map(parts.len());
            for (k, p) in parts {
                w.uint(*k as u64);
                self.entity(w, p)?;
            }
        }
        if !x.event_types.is_empty() {
            let w = m.put(31);
            w.array(x.event_types.len());
            for s in &x.event_types {
                w.uint(self.const_index(s)?);
            }
        }
        if !x.triggers.is_empty() {
            let w = m.put(32);
            w.array(x.triggers.len());
            for t in &x.triggers {
                let mut mm = M::new();
                if !t.attrs.is_empty() {
                    self.named_exprs(mm.put(6), &t.attrs)?;
                }
                if let Some(c) = &t.cmd {
                    self.cmd_ref(mm.put(11), c, None);
                }
                mm.uint(13, t.gp_cmd.map(u64::from));
                if let Some(v) = &t.when {
                    mm.put(14).bytes(&v.0);
                }
                mm.put(15).bytes(&t.event.0);
                if let Some(dp) = &t.tuya {
                    self.tuya_dp(mm.put(147), dp);
                }
                mm.finish(w);
            }
        }
        if let Some(p) = &x.press {
            let mut mm = M::new();
            match p {
                Press::Cmd { cmd, payload } => {
                    self.cmd_ref(mm.put(11), cmd, None);
                    if let Some(v) = payload {
                        mm.put(62).bytes(&v.0);
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
        if let Some(c) = &x.codec {
            self.codec(m.put(34), c);
        }
        if let Some(v) = &x.present_if {
            m.put(35).bytes(&v.0);
        }
        if !x.attrs.is_empty() {
            self.named_exprs(m.put(36), &x.attrs)?;
        }
        if let Some(h) = &x.hub {
            let mut mm = M::new();
            if let Some(v) = &h.default {
                scalar(mm.put(63), v);
            }
            mm.finish(m.put(37));
        }
        if let Some(v) = x.invert {
            m.put(38).bool(v);
        }
        ext(&mut m, &x.ext)?;
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
