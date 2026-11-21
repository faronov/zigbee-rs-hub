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

    /// Parses a key file / `ZDEF_SIGNING_KEY` value: an Ed25519 PKCS#8 PEM
    /// (`openssl genpkey -algorithm ed25519`) or 64 hex digits (the seed);
    /// see [`crate::keys`].
    pub fn parse(text: &str) -> Result<Self, String> {
        let k = crate::keys::parse_private(text)?;
        let key = Self::from_seed(k.seed);
        if k.public.is_some_and(|p| p != key.public()) {
            return Err(
                "signing key: the embedded public key does not match the private key".into(),
            );
        }
        Ok(key)
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
#[derive(Clone, Copy, Debug)]
pub struct BankParams {
    pub generation: u32,
    pub catalog_date: u32,
    pub catalog_serial: u16,
    pub max_size: usize,
    /// Bank format: 2 (string table read in place; default) or 1 (deflated).
    pub format: u16,
}

impl Default for BankParams {
    fn default() -> Self {
        BankParams {
            generation: 0,
            catalog_date: 0,
            catalog_serial: 0,
            max_size: usize::MAX,
            format: FORMAT,
        }
    }
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
    /// Unsigned bank (signature length 0).
    pub bytes: Vec<u8>,
    pub report: BankReport,
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

/// A compiled definition ready for banking: `(name, model, standalone CBOR)`.
pub struct Item {
    pub name: String,
    pub def: Definition,
    pub standalone: Vec<u8>,
}

impl Item {
    /// Checks canonical standalone CBOR (the shard and content-hash form).
    pub fn new(name: &str, cbor: Vec<u8>) -> Result<Item, String> {
        let def = zdef::load_canonical(&cbor, &zdef::HubFeatures(u128::MAX))
            .map_err(|e| format!("{name}: {e}"))?;
        if def.is_fragment() {
            return Err(format!(
                "{name}: self-description fragments are not catalogued"
            ));
        }
        Ok(Item {
            name: name.into(),
            def,
            standalone: cbor,
        })
    }

    /// Catalog form before string sharing (`description` removed).
    fn catalog_bytes(&self) -> Result<Vec<u8>, String> {
        let mut cat = self.def.clone();
        cat.description = None;
        cat.encode().map_err(|e| format!("{}: {e:?}", self.name))
    }

    /// Index keys of every match clause, with flags.
    fn keys(&self) -> Result<Vec<(u64, u8)>, String> {
        let wl: Vec<&str> = self
            .def
            .white_label
            .iter()
            .flat_map(|w| w.manufacturer.iter().map(|s| s.as_str()))
            .collect();
        let mut out = Vec::new();
        for c in &self.def.match_ {
            let keys = clause_keys(c).map_err(|_| format!("{}: a match clause cannot be indexed (needs names, a GP identity, or an endpoint with profile and device_id)", self.name))?;
            for (k, flags) in keys {
                let mut fl = flags;
                if let zdef::matching::IndexKey::Names(m, _) = k {
                    if wl.contains(&m) {
                        fl |= key_flags::WHITE_LABEL;
                    }
                }
                out.push((k.hash(), fl));
            }
        }
        Ok(out)
    }
}

fn sort_items(items: &mut [&Item]) -> Result<(), String> {
    items.sort_by(|a, b| {
        a.def
            .id
            .cmp(&b.def.id)
            .then(a.def.revision.cmp(&b.def.revision))
            .then(a.standalone.cmp(&b.standalone))
    });
    for w in items.windows(2) {
        if w[0].def.id == w[1].def.id && w[0].def.requires == w[1].def.requires {
            return Err(format!(
                "duplicate id {:?} with the same requires ({} and {})",
                w[0].def.id, w[0].name, w[1].name
            ));
        }
    }
    Ok(())
}

/// Largest catalog-form definition a block holds (`u16` length + form).
pub const BLOCK_ITEM_MAX: usize = BLOCK_MAX - 2;

/// Whether a definition fits a bank block (larger ones are shard-only).
pub fn fits_block(it: &Item) -> Result<bool, String> {
    Ok(it.catalog_bytes()?.len() <= BLOCK_ITEM_MAX)
}

/// Builds an unsigned bank (signature length 0); see [`sign_bank`].
pub fn build_bank(items: &[&Item], params: &BankParams) -> Result<BuiltBank, String> {
    let mut items: Vec<&Item> = items.to_vec();
    sort_items(&mut items)?;
    let mut forms: Vec<Vec<u8>> = Vec::with_capacity(items.len());
    for it in &items {
        forms.push(it.catalog_bytes()?);
    }

    // String table: strings that occur in at least two definitions, largest saving first.
    let mut count: HashMap<String, usize> = HashMap::new();
    for b in &forms {
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
    let strings_sec = match params.format {
        zb::FORMAT_V1 => {
            let mut sw = Writer::new();
            sw.array(strings.len());
            for (s, _) in &strings {
                sw.text(s);
            }
            let strings_raw = sw.into_vec();
            let mut sec = (strings_raw.len() as u32).to_le_bytes().to_vec();
            sec.extend(deflate(&strings_raw));
            sec
        }
        zb::FORMAT_V2 => {
            zb::FlashStrings::encode(&strings.iter().map(|s| s.0.as_str()).collect::<Vec<_>>())
        }
        f => return Err(format!("unknown bank format {f}")),
    };

    // Catalog form, packed into blocks of at most 4 KiB.
    let mut blocks: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut slots = 0u8;
    let mut placed: Vec<(u16, u8)> = Vec::new();
    let mut catalog_total = 0;
    for (it, b) in items.iter().zip(&forms) {
        let mut w = Writer::new();
        to_catalog_form(&mut Reader::new(b), &mut w, &table)?;
        let cf = w.into_vec();
        catalog_total += cf.len();
        if cf.len() > BLOCK_ITEM_MAX {
            return Err(format!(
                "{}: {} bytes in catalog form, larger than one {BLOCK_MAX} B block",
                it.name,
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

    // Index.
    let mut index: Vec<IndexEntry> = Vec::new();
    for (it, (block, slot)) in items.iter().zip(&placed) {
        for (h, flags) in it.keys()? {
            index.push(IndexEntry {
                key_hash: h,
                block: *block,
                slot: *slot,
                flags,
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

    let header = Header {
        format: params.format,
        sig_len: 0,
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
    let mut bytes = header.to_bytes().to_vec();
    bytes.extend(&body);
    // room for the signature added by `sign_bank`
    let total_signed = bytes.len() + signature_len();
    if total_signed > params.max_size {
        return Err(format!(
            "bank is {total_signed} bytes signed, above the {} byte partition",
            params.max_size
        ));
    }

    // Self-check with the hub-side reader: parse, body hash, load every definition.
    let bank = zb::Bank::parse(&bytes).map_err(|e| format!("self-check: {e:?}"))?;
    if zdef::hash::sha256(bank.body()) != bank.header.body_sha256 {
        return Err("self-check: body hash".into());
    }
    let mut inf = zb::MinizInflate::new();
    let st = bank
        .strings(&mut inf)
        .map_err(|e| format!("self-check: {e:?}"))?;
    let mut buf = [0u8; BLOCK_MAX];
    for (it, (block, slot)) in items.iter().zip(&placed) {
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
            .map_err(|e| format!("self-check {}: {e:?}", it.name))?;
        let mut want = it.def.clone();
        want.description = None;
        if back != want {
            return Err(format!("self-check {}: bank round trip differs", it.name));
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
        total: total_signed,
    };
    Ok(BuiltBank { bytes, report })
}

/// Length of the detached COSE_Sign1 of a bank header (fixed for Ed25519:
/// 8-byte kid, 64-byte signature).
pub fn signature_len() -> usize {
    SigningKey::from_seed([1; 32])
        .cose_sign1(&[0u8; HEADER_LEN], true)
        .len()
}

/// Signs (or re-signs) a bank: the header's signature length changes, the
/// body and its hash do not.
pub fn sign_bank(bank: &[u8], key: &SigningKey) -> Result<Vec<u8>, String> {
    let b = zb::Bank::parse(bank).map_err(|e| format!("bank: {e:?}"))?;
    let mut h = b.header;
    let sig_len = signature_len();
    h.sig_len = sig_len as u16;
    let hb = h.to_bytes();
    let sig = key.cose_sign1(&hb, true);
    if sig.len() != sig_len {
        return Err("internal: signature length changed".into());
    }
    let mut out = hb.to_vec();
    out.extend(sig);
    out.extend(b.body());
    let back = zb::Bank::parse(&out).map_err(|e| format!("self-check: {e:?}"))?;
    back.verify(&[sign::TrustedKey::new(key.public())])
        .map_err(|e| format!("self-check: {e:?}"))?;
    Ok(out)
}

/// Shards (spec 9.3) of every definition, and the content digest: SHA-256
/// of the ascending content hashes.
/// Encoded shards by shard number.
pub type Shards = BTreeMap<u8, Vec<u8>>;

pub fn build_shards(items: &[&Item]) -> Result<(Shards, [u8; 32]), String> {
    let mut items: Vec<&Item> = items.to_vec();
    sort_items(&mut items)?;
    let mut shards: BTreeMap<u8, Vec<ShardRecord>> = BTreeMap::new();
    let mut hashes: Vec<[u8; 32]> = Vec::new();
    for it in &items {
        hashes.push(zdef::hash::sha256(&it.standalone));
        let mut by_shard: BTreeMap<u8, Vec<u64>> = BTreeMap::new();
        for (h, _) in it.keys()? {
            let v = by_shard.entry(zb::shard_of(h)).or_default();
            if !v.contains(&h) {
                v.push(h);
            }
        }
        for (sh, mut key_hashes) in by_shard {
            key_hashes.sort_unstable();
            shards.entry(sh).or_default().push(ShardRecord {
                key_hashes,
                requires: it.def.requires.clone(),
                definition: it.standalone.clone(),
            });
        }
    }
    hashes.sort_unstable();
    hashes.dedup();
    let digest = zdef::hash::sha256(&hashes.concat());
    Ok((
        shards
            .into_iter()
            .map(|(k, v)| (k, zb::encode_shard(&v)))
            .collect(),
        digest,
    ))
}

/// A bank file of the manifest: `(name, bytes as published, definitions, generation)`.
pub struct BankFile<'a> {
    pub name: String,
    pub bytes: &'a [u8],
    pub definitions: u32,
    pub generation: u32,
}

/// The manifest payload.
pub fn manifest(
    params: &BankParams,
    banks: &[BankFile<'_>],
    shards: &BTreeMap<u8, Vec<u8>>,
    requires: Vec<u16>,
    content: Option<[u8; 32]>,
    mut source: Vec<(String, String)>,
) -> Manifest {
    // canonical CBOR map order (the decoder returns this order)
    source.sort_by(|a, b| (a.0.len(), a.0.as_bytes()).cmp(&(b.0.len(), b.0.as_bytes())));
    Manifest {
        catalog_date: params.catalog_date,
        catalog_serial: params.catalog_serial,
        requires,
        banks: banks
            .iter()
            .map(|b| ManifestAsset {
                name: b.name.clone(),
                sha256: zdef::hash::sha256(b.bytes),
                size: b.bytes.len() as u64,
                definitions: b.definitions,
                generation: b.generation,
            })
            .collect(),
        shards: (0..=255u8)
            .map(|i| shards.get(&i).map(|s| zdef::hash::sha256(s)))
            .collect(),
        content,
        source,
    }
}
