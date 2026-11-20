//! Device identity, match clauses, scoring and index keys (spec 3.2, 3.3, 6.2, 13.3).

use crate::cbor::Writer;
use crate::enums::HubFeatures;
use crate::model::{
    Definition, EpMatch, GpMatch, Kind, MatchClause, NameMatch, Power, VersionRange,
};
use alloc::string::String;
use alloc::vec::Vec;

/// Trims trailing NULs and spaces (spec 6.2).
pub fn trim_name(s: &str) -> &str {
    s.trim_end_matches(['\0', ' '])
}

/// One endpoint of the interview (Simple descriptor).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Endpoint {
    pub ep: u8,
    pub profile: u16,
    pub device_id: u16,
    pub in_clusters: Vec<u16>,
    pub out_clusters: Vec<u16>,
}

/// Canonical interview record of a Zigbee device (spec 3.2).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Fingerprint {
    pub manufacturer_name: Option<String>,
    pub model_id: Option<String>,
    /// Node descriptor manufacturer code.
    pub mfr_code: Option<u16>,
    pub logical_type: Option<u8>,
    /// Basic 0x0007 PowerSource.
    pub power_source: Option<u8>,
    pub rx_on_when_idle: Option<bool>,
    pub app_version: Option<u8>,
    pub stack_version: Option<u8>,
    pub hw_version: Option<u8>,
    pub zcl_version: Option<u8>,
    pub date_code: Option<String>,
    pub sw_build_id: Option<String>,
    pub endpoints: Vec<Endpoint>,
    pub self_description: Option<[u8; 8]>,
}

impl Fingerprint {
    /// Mains or battery, from Basic PowerSource, else from RxOnWhenIdle.
    pub fn power(&self) -> Option<Power> {
        match self.power_source.map(|p| p & 0x7f) {
            Some(3) => Some(Power::Battery),
            Some(1 | 2 | 4 | 5 | 6) => Some(Power::Mains),
            _ => self
                .rx_on_when_idle
                .map(|rx| if rx { Power::Mains } else { Power::Battery }),
        }
    }

    /// Canonical CBOR (integer keys 0..13 in field order; endpoints sorted).
    pub fn canonical_cbor(&self) -> Vec<u8> {
        let mut n = 0;
        let mut body = Writer::new();
        macro_rules! opt {
            ($k:expr, $v:expr, $f:expr) => {
                if let Some(v) = &$v {
                    body.uint($k);
                    #[allow(clippy::redundant_closure_call)]
                    ($f)(&mut body, v);
                    n += 1;
                }
            };
        }
        opt!(0, self.manufacturer_name, |w: &mut Writer, v: &String| w
            .text(v));
        opt!(1, self.model_id, |w: &mut Writer, v: &String| w.text(v));
        opt!(2, self.mfr_code, |w: &mut Writer, v: &u16| w
            .uint(*v as u64));
        opt!(3, self.logical_type, |w: &mut Writer, v: &u8| w
            .uint(*v as u64));
        opt!(4, self.power_source, |w: &mut Writer, v: &u8| w
            .uint(*v as u64));
        opt!(5, self.rx_on_when_idle, |w: &mut Writer, v: &bool| w
            .bool(*v));
        opt!(6, self.app_version, |w: &mut Writer, v: &u8| w
            .uint(*v as u64));
        opt!(7, self.stack_version, |w: &mut Writer, v: &u8| w
            .uint(*v as u64));
        opt!(8, self.hw_version, |w: &mut Writer, v: &u8| w
            .uint(*v as u64));
        opt!(9, self.zcl_version, |w: &mut Writer, v: &u8| w
            .uint(*v as u64));
        opt!(10, self.date_code, |w: &mut Writer, v: &String| w.text(v));
        opt!(11, self.sw_build_id, |w: &mut Writer, v: &String| w.text(v));
        if !self.endpoints.is_empty() {
            let mut eps: Vec<&Endpoint> = self.endpoints.iter().collect();
            crate::sort_small_by_key(&mut eps, |e| e.ep);
            body.uint(12);
            body.array(eps.len());
            for e in eps {
                body.array(5);
                body.uint(e.ep as u64);
                body.uint(e.profile as u64);
                body.uint(e.device_id as u64);
                body.array(e.in_clusters.len());
                for c in &e.in_clusters {
                    body.uint(*c as u64);
                }
                body.array(e.out_clusters.len());
                for c in &e.out_clusters {
                    body.uint(*c as u64);
                }
            }
            n += 1;
        }
        opt!(13, self.self_description, |w: &mut Writer, v: &[u8; 8]| w
            .bytes(v));
        let mut w = Writer::new();
        w.map(n);
        w.raw(&body.buf);
        w.into_vec()
    }

    /// First 16 bytes of SHA-256 over [`Fingerprint::canonical_cbor`] (spec 3.2).
    #[cfg(feature = "soft-sha256")]
    pub fn hash16(&self) -> [u8; 16] {
        self.hash16_with::<crate::hash::SoftSha256>()
    }

    /// [`Fingerprint::hash16`] with a SHA-256 implementation of the hub.
    pub fn hash16_with<H: crate::hash::Sha256>(&self) -> [u8; 16] {
        let h = crate::hash::sha256_with::<H>(&self.canonical_cbor());
        let mut o = [0; 16];
        o.copy_from_slice(&h[..16]);
        o
    }

    /// Lowest endpoint whose Simple descriptor has `cluster` (server side, or
    /// either side when `either`).
    pub fn endpoint_with(&self, cluster: u16, either: bool) -> Option<u8> {
        self.endpoints
            .iter()
            .filter(|e| {
                e.in_clusters.contains(&cluster) || (either && e.out_clusters.contains(&cluster))
            })
            .map(|e| e.ep)
            .min()
    }
}

/// GPD identifier (ZRHP 7.11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpdId {
    SrcId(u32),
    Ieee { ieee: u64, ep: u8 },
}

/// GP identity from the commissioning payload (spec 13.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpFingerprint {
    pub gpd_id: GpdId,
    pub device_id: u8,
    pub manufacturer_id: Option<u16>,
    pub model_id: Option<u16>,
    pub commands: Vec<u8>,
    pub server_clusters: Vec<u16>,
    pub client_clusters: Vec<u16>,
    /// Switch information of the commissioning payload (spec 13.2).
    pub switch_info: Option<GpSwitchInfo>,
    /// Security level the sink accepted (0-3, `EVT_GP_COMMISSIONED`).
    pub security_level: Option<u8>,
}

/// GPD switch information (Application information, spec 13.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct GpSwitchInfo {
    /// Number of contacts.
    pub contacts: u8,
    /// Switch type (0 button, 1 rocker, 2 unknown).
    pub switch_type: u8,
    /// Current contact bit vector.
    pub contact_status: u8,
}

impl GpFingerprint {
    /// A GP identity without application information.
    pub fn new(gpd_id: GpdId, device_id: u8) -> Self {
        GpFingerprint {
            gpd_id,
            device_id,
            manufacturer_id: None,
            model_id: None,
            commands: Vec::new(),
            server_clusters: Vec::new(),
            client_clusters: Vec::new(),
            switch_info: None,
            security_level: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Identity<'a> {
    Zigbee(&'a Fingerprint),
    Gp(&'a GpFingerprint),
}

fn names_match(m: &NameMatch, v: Option<&str>) -> bool {
    let Some(v) = v.map(trim_name) else {
        return false;
    };
    match m {
        NameMatch::Exact(s) => s == v,
        NameMatch::Any(list) => list.iter().any(|s| s == v),
        NameMatch::Prefix(p) => v.starts_with(p.as_str()),
    }
}

fn in_range(r: &VersionRange, v: Option<u8>) -> bool {
    let Some(v) = v else {
        return false;
    };
    let v = v as i64;
    r.min.is_none_or(|m| v >= m) && r.max.is_none_or(|m| v <= m)
}

fn subset(need: &[u16], have: &[u16]) -> bool {
    need.iter().all(|c| have.contains(c))
}

fn ep_matches(m: &EpMatch, e: &Endpoint) -> bool {
    m.profile.is_none_or(|p| p == e.profile)
        && m.device_id.is_none_or(|d| d == e.device_id)
        && subset(&m.in_clusters, &e.in_clusters)
        && subset(&m.out_clusters, &e.out_clusters)
}

/// Score points (spec 3.3).
pub mod score {
    pub const GP_STRONG: u16 = 600;
    pub const NAMES_EXACT: u16 = 500;
    pub const MODEL_EXACT_MFR_PREFIX: u16 = 400;
    pub const MODEL_EXACT: u16 = 300;
    pub const GP_DEVICE_COMMANDS: u16 = 300;
    pub const NAMES_WEAK: u16 = 250;
    pub const FINGERPRINT: u16 = 200;
    pub const GP_DEVICE: u16 = 200;
    pub const GP_WEAK: u16 = 100;
    pub const EXTRA: u16 = 10;
    pub const EXTRA_MAX: u16 = 50;
}

fn level(m: &Option<NameMatch>) -> u8 {
    match m {
        None => 0,
        Some(NameMatch::Prefix(_)) => 1,
        Some(_) => 2,
    }
}

/// Score of one clause for a device, `None` if it does not match.
pub fn score_clause(c: &MatchClause, id: Identity<'_>) -> Option<u16> {
    match id {
        Identity::Zigbee(fp) => {
            if c.gp.is_some() {
                return None;
            }
            let mut extra = 0u16;
            if let Some(m) = &c.manufacturer {
                if !names_match(m, fp.manufacturer_name.as_deref()) {
                    return None;
                }
            }
            if let Some(m) = &c.model {
                if !names_match(m, fp.model_id.as_deref()) {
                    return None;
                }
            }
            if let Some(code) = c.mfr_code {
                if fp.mfr_code != Some(code) {
                    return None;
                }
                extra += 1;
            }
            for (r, v) in [
                (&c.app_version, fp.app_version),
                (&c.stack_version, fp.stack_version),
                (&c.hw_version, fp.hw_version),
            ] {
                if let Some(r) = r {
                    if !in_range(r, v) {
                        return None;
                    }
                    extra += 1;
                }
            }
            if let Some(m) = &c.sw_build {
                if !names_match(m, fp.sw_build_id.as_deref()) {
                    return None;
                }
                extra += 1;
            }
            if let Some(t) = c.logical_type {
                if fp.logical_type != Some(t.code()) {
                    return None;
                }
                extra += 1;
            }
            if let Some(p) = c.power {
                if fp.power() != Some(p) {
                    return None;
                }
                extra += 1;
            }
            if !c.endpoints.is_empty() {
                for (ep, m) in &c.endpoints {
                    let e = fp.endpoints.iter().find(|e| e.ep == *ep)?;
                    if !ep_matches(m, e) {
                        return None;
                    }
                }
            }
            let base = match (level(&c.manufacturer), level(&c.model)) {
                (2, 2) => score::NAMES_EXACT,
                (1, 2) | (2, 1) => score::MODEL_EXACT_MFR_PREFIX,
                (0, 2) => score::MODEL_EXACT,
                (0, 0) => {
                    if c.endpoints.is_empty() && c.mfr_code.is_none() {
                        return None; // an empty clause never matches
                    }
                    // fingerprint-only: the mfr code is part of the base
                    if c.mfr_code.is_some() {
                        extra -= 1;
                    }
                    score::FINGERPRINT
                }
                _ => score::NAMES_WEAK,
            };
            if !c.endpoints.is_empty() && base != score::FINGERPRINT {
                extra += 1;
            }
            Some(base + (extra * score::EXTRA).min(score::EXTRA_MAX))
        }
        Identity::Gp(g) => {
            let gm = c.gp.as_ref()?;
            if c.manufacturer.is_some()
                || c.model.is_some()
                || !c.endpoints.is_empty()
                || c.mfr_code.is_some()
            {
                return None;
            }
            score_gp(gm, g)
        }
    }
}

fn score_gp(m: &GpMatch, g: &GpFingerprint) -> Option<u16> {
    if let Some(d) = m.device_id {
        if d != g.device_id {
            return None;
        }
    }
    if let Some(mf) = m.manufacturer_id {
        if g.manufacturer_id != Some(mf) {
            return None;
        }
    }
    if let Some(mo) = &m.model_id {
        if !g.model_id.is_some_and(|v| mo.matches(v)) {
            return None;
        }
    }
    let src_or_ieee = m.src_id.is_some() || m.ieee.is_some();
    if let Some((lo, hi)) = m.src_id {
        match g.gpd_id {
            GpdId::SrcId(s) if s >= lo && s <= hi => {}
            _ => return None,
        }
    }
    if let Some(prefix) = &m.ieee {
        match g.gpd_id {
            GpdId::Ieee { ieee, .. } => {
                let mut hex = crate::num::Buf::<16>::new();
                let _ = core::fmt::Write::write_fmt(&mut hex, format_args!("{:016x}", ieee));
                if !hex.as_str().starts_with(prefix.as_str()) {
                    return None;
                }
            }
            _ => return None,
        }
    }
    if !m.commands.iter().all(|c| g.commands.contains(c)) {
        return None;
    }
    if let Some(cl) = &m.clusters {
        if !subset(&cl.server, &g.server_clusters) || !subset(&cl.client, &g.client_clusters) {
            return None;
        }
    }
    // Spec 13.3: the clause's strongest identity decides; GP clauses get no
    // +10 extras (FlowerX GP, DeviceID + manufacturer + masked model: 600).
    let mfr_model = m.manufacturer_id.is_some() && m.model_id.is_some();
    if mfr_model || (src_or_ieee && m.device_id.is_some()) {
        Some(score::GP_STRONG)
    } else if m.device_id.is_some() && !m.commands.is_empty() {
        Some(score::GP_DEVICE_COMMANDS)
    } else if m.device_id.is_some() || src_or_ieee {
        Some(score::GP_DEVICE)
    } else if m.manufacturer_id.is_some()
        || m.model_id.is_some()
        || !m.commands.is_empty()
        || m.clusters.is_some()
    {
        Some(score::GP_WEAK)
    } else {
        None
    }
}

impl Definition {
    /// Best clause score for a device; fragments never match.
    pub fn match_score(&self, id: Identity<'_>) -> Option<u16> {
        if self.kind != Kind::Definition {
            return None;
        }
        self.match_.iter().filter_map(|c| score_clause(c, id)).max()
    }
}

/// Where a candidate definition comes from (tie-break order, spec 3.3, 9.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    Generic = 0,
    Catalog = 1,
    Fetched = 2,
    SelfDescription = 3,
    User = 4,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Generic => "generic",
            Source::Catalog => "catalog",
            Source::Fetched => "fetched",
            Source::SelfDescription => "self_description",
            Source::User => "user",
        }
    }
}

/// Result of [`select_best`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    /// Index of the winner in the candidate list.
    pub index: usize,
    pub score: u16,
}

/// A matching candidate skipped because this hub lacks a feature (spec 3.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub index: usize,
    pub missing_feature: u16,
}

/// Chooses the best definition among `candidates` for one layer (catalog +
/// fetched for layer 2, user uploads for layer 4). Ties: higher `priority`,
/// then source (user > fetched > catalog), then higher `revision`.
pub fn select_best(
    candidates: &[(Source, &Definition)],
    id: Identity<'_>,
    hub: &HubFeatures,
    skipped: Option<&mut Vec<Skipped>>,
) -> Option<Selection> {
    let mut best: Option<(Selection, (i64, Source, u32))> = None;
    let mut skipped = skipped;
    for (i, (src, d)) in candidates.iter().enumerate() {
        let Some(score) = d.match_score(id) else {
            continue;
        };
        if let Some(f) = hub.missing(&d.requires) {
            if let Some(s) = skipped.as_deref_mut() {
                s.push(Skipped {
                    index: i,
                    missing_feature: f,
                });
            }
            continue;
        }
        let tie = (d.priority.unwrap_or(0), *src, d.revision.unwrap_or(0));
        let better = match &best {
            None => true,
            Some((b, bt)) => score > b.score || (score == b.score && tie > *bt),
        };
        if better {
            best = Some((Selection { index: i, score }, tie));
        }
    }
    best.map(|b| b.0)
}

// ------------------------------------------------------------------ index keys (spec 6.2, 9.1)

/// An index key. Its hash is SipHash-2-4 over its canonical CBOR.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexKey<'a> {
    /// `[0, manufacturer, model]`
    Names(&'a str, &'a str),
    /// `[1, model]` (model-only and manufacturer-prefix clauses)
    Model(&'a str),
    /// `[2, mfr_code|null, profile, device_id]` (fingerprint-only clauses)
    Fingerprint(Option<u16>, u16, u16),
    /// `[3, manufacturer_id, model_id]` (GP, unmasked model ID)
    GpModel(u16, u16),
    /// `[4, device_id]` (GP DeviceID; masked model IDs and SrcID ranges are rechecked)
    GpDevice(u8),
    /// `[5, manufacturer_id]` (GP, masked model ID without DeviceID)
    GpManufacturer(u16),
    /// `[6, manufacturer]` (manufacturer-only clauses, e.g. Tuya names; rechecked)
    Manufacturer(&'a str),
}

impl IndexKey<'_> {
    pub fn canonical(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            IndexKey::Names(m, n) => {
                w.array(3);
                w.uint(0);
                w.text(m);
                w.text(n);
            }
            IndexKey::Model(n) => {
                w.array(2);
                w.uint(1);
                w.text(n);
            }
            IndexKey::Fingerprint(c, p, d) => {
                w.array(4);
                w.uint(2);
                match c {
                    Some(c) => w.uint(*c as u64),
                    None => w.null(),
                }
                w.uint(*p as u64);
                w.uint(*d as u64);
            }
            IndexKey::GpModel(m, n) => {
                w.array(3);
                w.uint(3);
                w.uint(*m as u64);
                w.uint(*n as u64);
            }
            IndexKey::GpDevice(d) => {
                w.array(2);
                w.uint(4);
                w.uint(*d as u64);
            }
            IndexKey::GpManufacturer(m) => {
                w.array(2);
                w.uint(5);
                w.uint(*m as u64);
            }
            IndexKey::Manufacturer(m) => {
                w.array(2);
                w.uint(6);
                w.text(m);
            }
        }
        w.into_vec()
    }

    pub fn hash(&self) -> u64 {
        crate::siphash::siphash24(&crate::siphash::INDEX_KEY, &self.canonical())
    }
}

/// Index-entry flags (spec 9.1).
pub mod key_flags {
    /// Prefix, range or mask constraints must be rechecked (always rechecked anyway).
    pub const RECHECK: u8 = 1;
    pub const FINGERPRINT: u8 = 2;
    pub const WHITE_LABEL: u8 = 4;
}

/// Why a clause cannot be indexed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotIndexable;

/// Index keys of a clause with their flags.
pub fn clause_keys(c: &MatchClause) -> Result<Vec<(IndexKey<'_>, u8)>, NotIndexable> {
    let mut out = Vec::new();
    if let Some(g) = &c.gp {
        let recheck = g.src_id.is_some()
            || g.ieee.is_some()
            || !g.commands.is_empty()
            || g.clusters.is_some();
        match (g.manufacturer_id, g.model_id, g.device_id) {
            (Some(m), Some(mo), _) if mo.mask.is_none() => out.push((
                IndexKey::GpModel(m, mo.value),
                if recheck || g.device_id.is_some() {
                    key_flags::RECHECK
                } else {
                    0
                },
            )),
            (_, _, Some(d)) => out.push((IndexKey::GpDevice(d), key_flags::RECHECK)),
            (Some(m), _, None) => out.push((IndexKey::GpManufacturer(m), key_flags::RECHECK)),
            _ => return Err(NotIndexable),
        }
        return Ok(out);
    }
    let extra = c.logical_type.is_some()
        || c.mfr_code.is_some()
        || c.app_version.is_some()
        || c.stack_version.is_some()
        || c.hw_version.is_some()
        || c.sw_build.is_some()
        || c.power.is_some()
        || !c.endpoints.is_empty();
    let rf = if extra { key_flags::RECHECK } else { 0 };
    match (&c.manufacturer, &c.model) {
        (Some(NameMatch::Exact(m)), Some(NameMatch::Exact(n))) => {
            out.push((IndexKey::Names(m, n), rf))
        }
        (Some(NameMatch::Any(ms)), Some(NameMatch::Exact(n))) => {
            for m in ms {
                out.push((IndexKey::Names(m, n), rf));
            }
        }
        (Some(NameMatch::Exact(m)), Some(NameMatch::Any(ns))) => {
            for n in ns {
                out.push((IndexKey::Names(m, n), rf));
            }
        }
        (Some(NameMatch::Any(ms)), Some(NameMatch::Any(ns))) => {
            for m in ms {
                for n in ns {
                    out.push((IndexKey::Names(m, n), rf));
                }
            }
        }
        (None, Some(NameMatch::Exact(n))) => out.push((IndexKey::Model(n), rf)),
        (None, Some(NameMatch::Any(ns))) => {
            for n in ns {
                out.push((IndexKey::Model(n), rf));
            }
        }
        (Some(NameMatch::Prefix(_)), Some(NameMatch::Exact(n))) => {
            out.push((IndexKey::Model(n), key_flags::RECHECK))
        }
        (Some(NameMatch::Prefix(_)), Some(NameMatch::Any(ns))) => {
            for n in ns {
                out.push((IndexKey::Model(n), key_flags::RECHECK));
            }
        }
        (Some(NameMatch::Exact(m)), None) => {
            out.push((IndexKey::Manufacturer(m), key_flags::RECHECK))
        }
        (Some(NameMatch::Any(ms)), None) => {
            for m in ms {
                out.push((IndexKey::Manufacturer(m), key_flags::RECHECK));
            }
        }
        _ => {
            let mut eps: Vec<&(u8, EpMatch)> = c.endpoints.iter().collect();
            crate::sort_small_by_key(&mut eps, |e| e.0);
            let e = eps
                .iter()
                .find(|e| e.1.profile.is_some() && e.1.device_id.is_some())
                .ok_or(NotIndexable)?;
            out.push((
                IndexKey::Fingerprint(c.mfr_code, e.1.profile.unwrap(), e.1.device_id.unwrap()),
                key_flags::FINGERPRINT | key_flags::RECHECK,
            ));
        }
    }
    Ok(out)
}

/// The index-key hashes a hub looks up for a device (spec 9.1 "Lookup").
pub fn lookup_hashes(id: Identity<'_>) -> Vec<u64> {
    let mut out = Vec::new();
    match id {
        Identity::Zigbee(fp) => {
            let model = fp.model_id.as_deref().map(trim_name);
            let mfr = fp.manufacturer_name.as_deref().map(trim_name);
            if let (Some(m), Some(n)) = (mfr, model) {
                out.push(IndexKey::Names(m, n).hash());
            }
            if let Some(n) = model {
                out.push(IndexKey::Model(n).hash());
            }
            if let Some(m) = mfr {
                out.push(IndexKey::Manufacturer(m).hash());
            }
            for e in &fp.endpoints {
                out.push(IndexKey::Fingerprint(None, e.profile, e.device_id).hash());
                if let Some(c) = fp.mfr_code {
                    out.push(IndexKey::Fingerprint(Some(c), e.profile, e.device_id).hash());
                }
            }
        }
        Identity::Gp(g) => {
            if let (Some(m), Some(n)) = (g.manufacturer_id, g.model_id) {
                out.push(IndexKey::GpModel(m, n).hash());
            }
            out.push(IndexKey::GpDevice(g.device_id).hash());
            if let Some(m) = g.manufacturer_id {
                out.push(IndexKey::GpManufacturer(m).hash());
            }
        }
    }
    crate::sort_small_by_key(&mut out, |h| *h);
    out.dedup();
    out
}
