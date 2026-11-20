//! Catalog bank builder (spec 9.1, 9.3): catalog form, string table, index,
//! 4 KiB deflate blocks, signed header, shards and the signed manifest.

use std::collections::{BTreeMap, HashMap};
use zdef::bank::{self as zb, BLOCK_MAX, FORMAT, HEADER_LEN, Header, IndexEntry, ShardRecord};
use zdef::cbor::{Head, Reader, Writer};
use zdef::matching::{clause_keys, key_flags};
use zdef::model::Definition;
use zdef::sign::{self, Manifest, ManifestAsset};

/// Ed25519 signing key (32-byte seed). Never commit one.
pub struct SigningKey {
    kp: ed25519_compact::KeyPair,
}

impl SigningKey {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        SigningKey {
            kp: ed25519_compact::KeyPair::from_seed(ed25519_compact::Seed::new(seed)),
        }
    }

    /// Parses a key file / `ZDEF_SIGNING_KEY` value: 64 hex digits (the seed);
    /// whitespace and `#` comment lines are ignored.
    pub fn parse(text: &str) -> Result<Self, String> {
        let hex: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<String>()
            .split_whitespace()
            .collect();
        let b = unhex(&hex).ok_or("signing key: expected 64 hex digits (Ed25519 seed)")?;
        let seed: [u8; 32] = b.try_into().map_err(|_| "signing key: expected 32 bytes")?;
        Ok(Self::from_seed(seed))
    }

    pub fn public(&self) -> [u8; 32] {
        *self.kp.pk
    }

    pub fn kid(&self) -> [u8; 8] {
        sign::key_id(&self.public())
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        *self.kp.sk.sign(msg, None)
    }

    /// COSE_Sign1; `detached` = payload not embedded.
    pub fn cose_sign1(&self, payload: &[u8], detached: bool) -> Vec<u8> {
        let tbs = sign::to_be_signed(&sign::protected_header(), payload);
        let sig = self.sign(&tbs);
        sign::cose_sign1(
            &self.kid(),
            if detached { None } else { Some(payload) },
            &sig,
        )
    }
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Bank build parameters.
pub struct BankParams {
    pub generation: u32,
    pub catalog_date: u32,
    pub catalog_serial: u16,
    pub max_size: usize,
}

/// Size table of a built bank.
#[derive(Debug, Default, Clone)]
pub struct BankReport {
    pub definitions: usize,
    pub keys: usize,
    pub blocks: usize,
    pub strings: usize,
    pub strings_bytes: usize,
    pub index_bytes: usize,
    pub blocks_bytes: usize,
    pub catalog_form_bytes: usize,
    pub total: usize,
}

pub struct BuiltBank {
    pub bytes: Vec<u8>,
    pub report: BankReport,
    pub shards: BTreeMap<u8, Vec<u8>>,
}

fn ce(x: zdef::cbor::CborError) -> String {
    format!("{x:?}")
}

/// Visits text strings (not map keys) in an encoded item.
fn walk_texts(r: &mut Reader<'_>, f: &mut dyn FnMut(&str)) -> Result<(), String> {
    match r.peek().map_err(ce)? {
        Head::Text(_) => f(r.text().map_err(ce)?),
        Head::Array(_) => {
            for _ in 0..r.array().map_err(ce)? {
                walk_texts(r, f)?;
            }
        }
        Head::Map(_) => {
            for _ in 0..r.map().map_err(ce)? {
                r.skip().map_err(ce)?;
                walk_texts(r, f)?;
            }
        }
        Head::Tag(_) => {
            r.head().map_err(ce)?;
            walk_texts(r, f)?;
        }
        _ => {
            r.skip().map_err(ce)?;
        }
    }
    Ok(())
}

fn ref_len(i: usize) -> usize {
    if i < 16 {
        1
    } else if i - 16 < 24 {
        2
    } else if i - 16 < 256 {
        3
    } else {
        4
    }
}

fn text_len(s: &str) -> usize {
    let n = s.len();
    n + if n < 24 {
        1
    } else if n < 256 {
        2
    } else {
        3
    }
}

/// Rewrites text items (not map keys) into shared references (spec 8.4):
/// simple value n for n < 16, tag 6 with n - 16 otherwise.
fn to_catalog_form(
    r: &mut Reader<'_>,
    w: &mut Writer,
    table: &HashMap<String, usize>,
) -> Result<(), String> {
    match r.peek().map_err(ce)? {
        Head::Text(_) => {
            let s = r.text().map_err(ce)?;
            match table.get(s) {
                Some(&i) if ref_len(i) < text_len(s) => {
                    if i < 16 {
                        w.simple(i as u8);
                    } else {
                        w.tag(6);
                        w.uint((i - 16) as u64);
                    }
                }
                _ => w.text(s),
            }
        }
        Head::Array(_) => {
            let n = r.array().map_err(ce)?;
            w.array(n);
            for _ in 0..n {
                to_catalog_form(r, w, table)?;
            }
        }
        Head::Map(_) => {
            let n = r.map().map_err(ce)?;
            w.map(n);
            for _ in 0..n {
                w.raw(r.skip().map_err(ce)?);
                to_catalog_form(r, w, table)?;
            }
        }
        Head::Tag(t) => {
            r.head().map_err(ce)?;
            w.tag(t);
            to_catalog_form(r, w, table)?;
        }
        _ => {
            let item = r.skip().map_err(ce)?;
            w.raw(item);
        }
    }
    Ok(())
}

fn deflate_with(data: &[u8], extra: u32) -> Vec<u8> {
    use miniz_oxide::deflate::core::{
        CompressorOxide, TDEFLFlush, compress_to_output, create_comp_flags_from_zip_params,
    };
    let mut c = CompressorOxide::new(create_comp_flags_from_zip_params(10, -15, 0) | extra);
    let mut out = Vec::new();
    let _ = compress_to_output(&mut c, data, TDEFLFlush::Finish, |b| {
        out.extend_from_slice(b);
        true
    });
    out
}

/// Raw deflate, best compression: the shorter of dynamic and static Huffman
/// coding (miniz alone always picks dynamic blocks, which loses on small inputs).
pub fn deflate(data: &[u8]) -> Vec<u8> {
    let d = deflate_with(data, 0);
    let s = deflate_with(
        data,
        miniz_oxide::deflate::core::deflate_flags::TDEFL_FORCE_ALL_STATIC_BLOCKS,
    );
    if s.len() < d.len() { s } else { d }
}

/// Builds a bank from standalone definitions `(name, cbor)`.
pub fn build_bank(
    defs: &[(String, Vec<u8>)],
    params: &BankParams,
    key: &SigningKey,
) -> Result<BuiltBank, String> {
    let mut items: Vec<(String, Definition, Vec<u8>)> = Vec::new();
    for (name, cbor) in defs {
        let d = zdef::load_canonical(cbor, &zdef::HubFeatures(u128::MAX))
            .map_err(|e| format!("{name}: {e}"))?;
        if d.is_fragment() {
            return Err(format!("{name}: self-description fragments are not banked"));
        }
        let mut cat = d.clone();
        cat.description = None;
        let bytes = cat.encode().map_err(|e| format!("{name}: {e:?}"))?;
        items.push((name.clone(), d, bytes));
    }
    items.sort_by(|a, b| a.1.id.cmp(&b.1.id).then(a.1.revision.cmp(&b.1.revision)));
    for w in items.windows(2) {
        if w[0].1.id == w[1].1.id && w[0].1.requires == w[1].1.requires {
            return Err(format!(
                "duplicate id {:?} with the same requires",
                w[0].1.id
            ));
        }
    }

    // String table: strings that occur in at least two definitions, largest saving first.
    let mut count: HashMap<String, usize> = HashMap::new();
    for (_, _, b) in &items {
        let mut seen: Vec<String> = Vec::new();
        walk_texts(&mut Reader::new(b), &mut |s| {
            if !seen.iter().any(|x| x == s) {
                seen.push(s.to_string());
            }
        })?;
        for s in seen {
            *count.entry(s).or_default() += 1;
        }
    }
    let mut strings: Vec<(String, usize)> = count
        .into_iter()
        .filter(|(s, c)| *c >= 2 && text_len(s) > 1)
        .collect();
    strings.sort_by(|a, b| {
        (b.1 * text_len(&b.0))
            .cmp(&(a.1 * text_len(&a.0)))
            .then(a.0.cmp(&b.0))
    });
    let table: HashMap<String, usize> = strings
        .iter()
        .enumerate()
        .map(|(i, (s, _))| (s.clone(), i))
        .collect();
    let mut sw = Writer::new();
    sw.array(strings.len());
    for (s, _) in &strings {
        sw.text(s);
    }
    let strings_raw = sw.into_vec();
    let mut strings_sec = (strings_raw.len() as u32).to_le_bytes().to_vec();
    strings_sec.extend(deflate(&strings_raw));

    // Catalog form, packed into blocks of at most 4 KiB.
    let mut blocks: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut slots = 0u8;
    let mut placed: Vec<(u16, u8)> = Vec::new();
    let mut catalog_total = 0;
    for (name, _, b) in &items {
        let mut w = Writer::new();
        to_catalog_form(&mut Reader::new(b), &mut w, &table)?;
        let cf = w.into_vec();
        catalog_total += cf.len();
        if cf.len() + 2 > BLOCK_MAX {
            return Err(format!(
                "{name}: {} bytes in catalog form, larger than one {BLOCK_MAX} B block",
                cf.len()
            ));
        }
        if cur.len() + 2 + cf.len() > BLOCK_MAX || slots == u8::MAX {
            blocks.push(std::mem::take(&mut cur));
            slots = 0;
        }
        placed.push((blocks.len() as u16, slots));
        cur.extend_from_slice(&(cf.len() as u16).to_le_bytes());
        cur.extend_from_slice(&cf);
        slots += 1;
    }
    if !cur.is_empty() {
        blocks.push(cur);
    }
    let compressed: Vec<Vec<u8>> = blocks.iter().map(|b| deflate(b)).collect();

    // Index and shards.
    let mut index: Vec<IndexEntry> = Vec::new();
    let mut shards: BTreeMap<u8, Vec<ShardRecord>> = BTreeMap::new();
    for ((name, d, _), (block, slot)) in items.iter().zip(&placed) {
        let wl: Vec<&str> = d
            .white_label
            .iter()
            .flat_map(|w| w.manufacturer.iter().map(|s| s.as_str()))
            .collect();
        let mut hashes: Vec<u64> = Vec::new();
        for c in &d.match_ {
            let keys = clause_keys(c).map_err(|_| format!("{name}: a match clause cannot be indexed (needs names, a GP identity, or an endpoint with profile and device_id)"))?;
            for (k, flags) in keys {
                let mut fl = flags;
                if let zdef::matching::IndexKey::Names(m, _) = k {
                    if wl.contains(&m) {
                        fl |= key_flags::WHITE_LABEL;
                    }
                }
                let h = k.hash();
                index.push(IndexEntry {
                    key_hash: h,
                    block: *block,
                    slot: *slot,
                    flags: fl,
                });
                if !hashes.contains(&h) {
                    hashes.push(h);
                }
            }
        }
        let standalone = d.encode().map_err(|e| format!("{name}: {e:?}"))?;
        for h in hashes {
            shards
                .entry(zb::shard_of(h))
                .or_default()
                .push(ShardRecord {
                    key_hash: h,
                    requires: d.requires.clone(),
                    definition: standalone.clone(),
                });
        }
    }
    index.sort();
    index.dedup();

    let mut body = strings_sec.clone();
    for e in &index {
        body.extend_from_slice(&e.to_bytes());
    }
    let mut off = 0u32;
    for c in &compressed {
        body.extend_from_slice(&off.to_le_bytes());
        off += c.len() as u32;
    }
    body.extend_from_slice(&off.to_le_bytes());
    for c in &compressed {
        body.extend_from_slice(c);
    }

    let sig_len = key.cose_sign1(&[0u8; HEADER_LEN], true).len();
    let header = Header {
        format: FORMAT,
        sig_len: sig_len as u16,
        generation: params.generation,
        catalog_date: params.catalog_date,
        catalog_serial: params.catalog_serial,
        blocks: u16::try_from(compressed.len()).map_err(|_| "too many blocks")?,
        definitions: u16::try_from(items.len()).map_err(|_| "too many definitions")?,
        strings: u16::try_from(strings.len()).map_err(|_| "too many shared strings")?,
        keys: index.len() as u32,
        strings_len: strings_sec.len() as u32,
        body_sha256: zdef::hash::sha256(&body),
    };
    let hb = header.to_bytes();
    let sig = key.cose_sign1(&hb, true);
    if sig.len() != sig_len {
        return Err("internal: signature length changed".into());
    }
    let mut bytes = hb.to_vec();
    bytes.extend(sig);
    bytes.extend(&body);
    if bytes.len() > params.max_size {
        return Err(format!(
            "bank is {} bytes, above the {} byte partition",
            bytes.len(),
            params.max_size
        ));
    }

    // Self-check with the hub-side reader: parse, verify, load every definition.
    let bank = zb::Bank::parse(&bytes).map_err(|e| format!("self-check: {e:?}"))?;
    bank.verify(&[sign::TrustedKey::new(key.public())])
        .map_err(|e| format!("self-check: {e:?}"))?;
    let mut inf = zb::MinizInflate::new();
    let st = bank
        .load_strings(&mut inf)
        .map_err(|e| format!("self-check: {e:?}"))?;
    let mut buf = [0u8; BLOCK_MAX];
    for ((name, d, _), (block, slot)) in items.iter().zip(&placed) {
        let back = bank
            .load(
                &mut inf,
                &st,
                &IndexEntry {
                    key_hash: 0,
                    block: *block,
                    slot: *slot,
                    flags: 0,
                },
                &mut buf,
            )
            .map_err(|e| format!("self-check {name}: {e:?}"))?;
        let mut want = d.clone();
        want.description = None;
        if back != want {
            return Err(format!("self-check {name}: bank round trip differs"));
        }
    }

    let report = BankReport {
        definitions: items.len(),
        keys: index.len(),
        blocks: compressed.len(),
        strings: strings.len(),
        strings_bytes: strings_sec.len(),
        index_bytes: index.len() * zb::INDEX_ENTRY_LEN,
        blocks_bytes: compressed.iter().map(|c| c.len()).sum(),
        catalog_form_bytes: catalog_total,
        total: bytes.len(),
    };
    let shards = shards
        .into_iter()
        .map(|(k, v)| (k, zb::encode_shard(&v)))
        .collect();
    Ok(BuiltBank {
        bytes,
        report,
        shards,
    })
}

/// Builds and signs `manifest.cose`.
pub fn build_manifest(
    params: &BankParams,
    banks: &[(String, &[u8])],
    shards: &BTreeMap<u8, Vec<u8>>,
    requires: Vec<u16>,
    key: &SigningKey,
) -> Vec<u8> {
    let m = Manifest {
        catalog_date: params.catalog_date,
        catalog_serial: params.catalog_serial,
        requires,
        banks: banks
            .iter()
            .map(|(n, b)| ManifestAsset {
                name: n.clone(),
                sha256: zdef::hash::sha256(b),
                size: b.len() as u64,
            })
            .collect(),
        shards: (0..=255u8)
            .map(|i| shards.get(&i).map(|s| zdef::hash::sha256(s)))
            .collect(),
    };
    key.cose_sign1(&m.encode(), false)
}
