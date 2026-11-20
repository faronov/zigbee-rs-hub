//! Catalog build from a plan (spec 9.1, 9.3): banks (largest priority
//! prefix that fits each partition), 256 shards of every definition, the
//! manifest, signing and verification. The binary formats are `zdef`'s
//! (`zdef::bank`, `zdef::sign`); `faronov/zigbee-rs-definitions` drives this
//! through `tools/catalog/build.py`.

use crate::bank::{self, BankFile, BankParams, Item, SigningKey};
use crate::tree::{Node, Value};
use crate::{Format, Options};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use zdef::sign::{Manifest, TrustedKey};

/// One bank of the plan.
pub struct BankPlan {
    pub name: String,
    pub max_size: usize,
    /// Definition files in priority order (most popular first).
    pub candidates: Vec<PathBuf>,
}

/// `plan.json`:
///
/// ```json
/// {"date": 20261120, "serial": 0, "generation": 2026112000,
///  "requires": ["expr-v1"], "source": {"zhc": "25.1.0", "commit": "…"},
///  "definitions": ["definitions/…/a.yaml", …],
///  "banks": [{"name": "bank-4m.zdb", "max_size": 262144, "candidates": [ … ]}]}
/// ```
///
/// Paths are relative to the plan file. `definitions` go to the shards;
/// bank candidates must be among them. `generation` defaults to
/// `date * 100 + serial`.
pub struct Plan {
    pub params: BankParams,
    pub requires: Vec<u16>,
    pub source: Vec<(String, String)>,
    pub definitions: Vec<PathBuf>,
    pub banks: Vec<BankPlan>,
}

fn text(n: &Node) -> Option<&str> {
    match &n.v {
        Value::Str(s) => Some(s),
        _ => None,
    }
}

fn field<'n>(n: &'n Node, k: &str) -> Result<&'n Node, String> {
    n.get(k).ok_or_else(|| format!("plan: missing {k}"))
}

fn uint(n: &Node, what: &str) -> Result<u64, String> {
    match n.v {
        Value::Int(i) if i >= 0 => Ok(i as u64),
        _ => Err(format!("plan: {what} is not a non-negative integer")),
    }
}

fn paths(n: &Node, base: &Path, what: &str) -> Result<Vec<PathBuf>, String> {
    let Value::Seq(v) = &n.v else {
        return Err(format!("plan: {what} is not a list"));
    };
    v.iter()
        .map(|x| {
            text(x)
                .map(|s| base.join(s))
                .ok_or_else(|| format!("plan: {what} items are paths"))
        })
        .collect()
}

impl Plan {
    pub fn parse(src: &str, base: &Path) -> Result<Plan, String> {
        let root = crate::parse_source(src, Format::Json)
            .map_err(|e| format!("plan: {}", crate::format_diags("plan", &e)))?;
        let date = uint(field(&root, "date")?, "date")? as u32;
        let serial = root
            .get("serial")
            .map(|n| uint(n, "serial"))
            .transpose()?
            .unwrap_or(0) as u16;
        let generation = root
            .get("generation")
            .map(|n| uint(n, "generation"))
            .transpose()?
            .map(|g| g as u32)
            .unwrap_or(date.saturating_mul(100).saturating_add(serial as u32));
        let mut requires = Vec::new();
        if let Some(Value::Seq(v)) = root.get("requires").map(|n| &n.v) {
            for f in v {
                let name = text(f).ok_or("plan: requires are feature names")?;
                let f = zdef::Feature::parse(name)
                    .ok_or_else(|| format!("plan: unknown feature {name}"))?;
                requires.push(f.code() as u16);
            }
        }
        let mut source = Vec::new();
        if let Some(Value::Map(m)) = root.get("source").map(|n| &n.v) {
            for (k, _, v) in m {
                let v = text(v).ok_or("plan: source values are strings")?;
                source.push((k.as_json(), v.to_string()));
            }
        }
        let definitions = paths(field(&root, "definitions")?, base, "definitions")?;
        let mut banks = Vec::new();
        if let Some(Value::Seq(v)) = root.get("banks").map(|n| &n.v) {
            for b in v {
                banks.push(BankPlan {
                    name: text(field(b, "name")?)
                        .ok_or("plan: bank name is a string")?
                        .to_string(),
                    max_size: uint(field(b, "max_size")?, "max_size")? as usize,
                    candidates: paths(field(b, "candidates")?, base, "candidates")?,
                });
            }
        }
        Ok(Plan {
            params: BankParams {
                generation,
                catalog_date: date,
                catalog_serial: serial,
                max_size: 0,
            },
            requires,
            source,
            definitions,
            banks,
        })
    }
}

/// What went into one bank.
#[derive(Debug, Default)]
pub struct BankOutcome {
    pub name: String,
    pub size: usize,
    pub max_size: usize,
    pub report: bank::BankReport,
    /// IDs in the bank.
    pub included: Vec<String>,
    /// Candidates larger than one block (shard-only).
    pub too_large: Vec<String>,
    /// Candidates that did not fit the partition.
    pub no_room: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub definitions: usize,
    pub shards: usize,
    pub content: [u8; 32],
    pub banks: Vec<BankOutcome>,
    pub signed: bool,
}

impl Outcome {
    /// Machine-readable report (`catalog.json`).
    pub fn json(&self) -> String {
        let list = |v: &[String]| {
            let items: Vec<String> = v.iter().map(|s| json_str(s)).collect();
            format!("[{}]", items.join(","))
        };
        let banks: Vec<String> = self
            .banks
            .iter()
            .map(|b| {
                format!(
                    "{{\"name\":{},\"size\":{},\"max_size\":{},\"definitions\":{},\"keys\":{},\"blocks\":{},\"strings\":{},\"included\":{},\"too_large\":{},\"no_room\":{}}}",
                    json_str(&b.name),
                    b.size,
                    b.max_size,
                    b.report.definitions,
                    b.report.keys,
                    b.report.blocks,
                    b.report.strings,
                    list(&b.included),
                    list(&b.too_large),
                    list(&b.no_room)
                )
            })
            .collect();
        format!(
            "{{\"definitions\":{},\"shards\":{},\"content\":\"{}\",\"signed\":{},\"banks\":[{}]}}\n",
            self.definitions,
            self.shards,
            crate::hex(&self.content),
            self.signed,
            banks.join(",")
        )
    }
}

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn compile(p: &Path) -> Result<Item, String> {
    let name = p.to_string_lossy().to_string();
    let bytes = fs::read(p).map_err(|e| format!("{name}: {e}"))?;
    let cbor = if p.extension().is_some_and(|e| e == "cbor") {
        bytes
    } else {
        let src = String::from_utf8(bytes).map_err(|_| format!("{name}: not UTF-8"))?;
        crate::compile_str(&src, Format::from_path(&name), &Options::default())
            .map_err(|e| crate::format_diags(&name, &e).trim_end().to_string())?
            .cbor
    };
    Item::new(&name, cbor)
}

fn id_of(it: &Item) -> String {
    it.def.id.clone().unwrap_or_else(|| it.name.clone())
}

/// Builds the catalog into `out`: banks, shards, `manifest.cbor`,
/// `catalog.json`; with a key also signs (banks and `manifest.cose`).
pub fn build(plan: &Plan, out: &Path, key: Option<&SigningKey>) -> Result<Outcome, String> {
    let mut errs = Vec::new();
    let mut items: BTreeMap<PathBuf, Item> = BTreeMap::new();
    for p in &plan.definitions {
        match compile(p) {
            Ok(it) => {
                items.insert(p.clone(), it);
            }
            Err(e) => errs.push(e),
        }
    }
    if !errs.is_empty() {
        return Err(format!(
            "{} definitions do not compile:\n{}",
            errs.len(),
            errs.join("\n")
        ));
    }
    fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let all: Vec<&Item> = items.values().collect();
    let (shards, content) = bank::build_shards(&all)?;
    for (h, s) in &shards {
        fs::write(out.join(format!("shard-{h:02x}.cbor")), s).map_err(|e| e.to_string())?;
    }
    let mut outcome = Outcome {
        definitions: all.len(),
        shards: shards.len(),
        content,
        ..Default::default()
    };
    let mut files: Vec<(String, Vec<u8>, u32)> = Vec::new();
    for bp in &plan.banks {
        let mut o = BankOutcome {
            name: bp.name.clone(),
            max_size: bp.max_size,
            ..Default::default()
        };
        let mut cands: Vec<&Item> = Vec::new();
        for p in &bp.candidates {
            let it = items
                .get(p)
                .ok_or_else(|| format!("{}: bank candidate not in definitions", p.display()))?;
            if bank::fits_block(it)? {
                cands.push(it);
            } else {
                o.too_large.push(id_of(it));
            }
        }
        let params = BankParams {
            max_size: bp.max_size,
            ..plan.params
        };
        // Largest prefix that fits: the size grows with the prefix.
        let fits = |n: usize| -> Result<Option<bank::BuiltBank>, String> {
            match bank::build_bank(&cands[..n], &params) {
                Ok(b) => Ok(Some(b)),
                Err(e) if e.contains("above the") => Ok(None),
                Err(e) => Err(e),
            }
        };
        let built = match fits(cands.len())? {
            Some(b) => {
                o.included = cands.iter().map(|i| id_of(i)).collect();
                b
            }
            None => {
                let (mut lo, mut hi) = (0usize, cands.len());
                let mut best = fits(0)?.ok_or("an empty bank does not fit")?;
                while hi - lo > 1 {
                    let mid = (lo + hi) / 2;
                    match fits(mid)? {
                        Some(b) => {
                            lo = mid;
                            best = b;
                        }
                        None => hi = mid,
                    }
                }
                o.included = cands[..lo].iter().map(|i| id_of(i)).collect();
                o.no_room = cands[lo..].iter().map(|i| id_of(i)).collect();
                best
            }
        };
        let bytes = match key {
            Some(k) => bank::sign_bank(&built.bytes, k)?,
            None => built.bytes,
        };
        o.size = bytes.len();
        o.report = built.report;
        fs::write(out.join(&bp.name), &bytes).map_err(|e| e.to_string())?;
        files.push((bp.name.clone(), bytes, o.report.definitions as u32));
        outcome.banks.push(o);
    }
    let bank_files: Vec<BankFile<'_>> = files
        .iter()
        .map(|(n, b, d)| BankFile {
            name: n.clone(),
            bytes: b,
            definitions: *d,
            generation: plan.params.generation,
        })
        .collect();
    let m = bank::manifest(
        &plan.params,
        &bank_files,
        &shards,
        plan.requires.clone(),
        Some(content),
        plan.source.clone(),
    );
    let payload = m.encode();
    fs::write(out.join("manifest.cbor"), &payload).map_err(|e| e.to_string())?;
    if let Some(k) = key {
        fs::write(out.join("manifest.cose"), k.cose_sign1(&payload, false))
            .map_err(|e| e.to_string())?;
        outcome.signed = true;
    }
    fs::write(out.join("catalog.json"), outcome.json()).map_err(|e| e.to_string())?;
    Ok(outcome)
}

fn read_manifest(dir: &Path) -> Result<Manifest, String> {
    let b = fs::read(dir.join("manifest.cbor")).map_err(|e| format!("manifest.cbor: {e}"))?;
    Manifest::decode(&b).map_err(|e| format!("manifest.cbor: {e}"))
}

/// Signs an unsigned (or re-signs a signed) catalog directory in place:
/// every bank of the manifest, then `manifest.cbor` (bank hashes and sizes
/// of the signed files) and `manifest.cose`.
pub fn sign(dir: &Path, key: &SigningKey) -> Result<Manifest, String> {
    let mut m = read_manifest(dir)?;
    for a in &mut m.banks {
        let p = dir.join(&a.name);
        let b = fs::read(&p).map_err(|e| format!("{}: {e}", a.name))?;
        let s = bank::sign_bank(&b, key).map_err(|e| format!("{}: {e}", a.name))?;
        fs::write(&p, &s).map_err(|e| e.to_string())?;
        a.sha256 = zdef::hash::sha256(&s);
        a.size = s.len() as u64;
    }
    let payload = m.encode();
    fs::write(dir.join("manifest.cbor"), &payload).map_err(|e| e.to_string())?;
    fs::write(dir.join("manifest.cose"), key.cose_sign1(&payload, false))
        .map_err(|e| e.to_string())?;
    Ok(m)
}

/// Verifies a catalog directory as a hub would, plus everything a hub only
/// fetches on demand: `manifest.cose` against the key, `manifest.cbor` equal
/// to its payload, bank hashes, sizes and header signatures, every shard's
/// hash, and that every shard definition loads canonically and is filed
/// under its own index keys.
pub fn verify(dir: &Path, public: [u8; 32]) -> Result<Manifest, String> {
    let keys = [TrustedKey::new(public)];
    let cose = fs::read(dir.join("manifest.cose")).map_err(|e| format!("manifest.cose: {e}"))?;
    let m = Manifest::verify(&cose, &keys).map_err(|e| format!("manifest.cose: {e:?}"))?;
    if read_manifest(dir)? != m {
        return Err("manifest.cbor differs from the signed payload".into());
    }
    for a in &m.banks {
        let b = fs::read(dir.join(&a.name)).map_err(|e| format!("{}: {e}", a.name))?;
        if zdef::hash::sha256(&b) != a.sha256 || b.len() as u64 != a.size {
            return Err(format!(
                "{}: hash or size differs from the manifest",
                a.name
            ));
        }
        let bank = zdef::bank::Bank::parse(&b).map_err(|e| format!("{}: {e:?}", a.name))?;
        bank.verify(&keys)
            .map_err(|e| format!("{}: {e:?}", a.name))?;
        if u32::from(bank.header.definitions) != a.definitions
            || bank.header.generation != a.generation
        {
            return Err(format!("{}: header differs from the manifest", a.name));
        }
    }
    if m.shards.len() != 256 {
        return Err("manifest: 256 shard entries expected".into());
    }
    let mut hashes = Vec::new();
    for (i, h) in m.shards.iter().enumerate() {
        let p = dir.join(format!("shard-{i:02x}.cbor"));
        match h {
            None => {
                if p.exists() {
                    return Err(format!("{}: not in the manifest", p.display()));
                }
            }
            Some(h) => {
                let b = fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                if zdef::hash::sha256(&b) != *h {
                    return Err(format!("{}: hash differs from the manifest", p.display()));
                }
                for r in
                    zdef::bank::decode_shard(&b).map_err(|e| format!("{}: {e}", p.display()))?
                {
                    let it = Item::new(&p.to_string_lossy(), r.definition.clone())?;
                    if it.def.requires != r.requires {
                        return Err(format!("{}: requires differ", p.display()));
                    }
                    if r.key_hashes
                        .iter()
                        .any(|k| zdef::bank::shard_of(*k) != i as u8)
                    {
                        return Err(format!("{}: key in the wrong shard", p.display()));
                    }
                    hashes.push(zdef::hash::sha256(&r.definition));
                }
            }
        }
    }
    hashes.sort_unstable();
    hashes.dedup();
    if m.content
        .is_some_and(|c| c != zdef::hash::sha256(&hashes.concat()))
    {
        return Err("manifest: content digest differs from the shards".into());
    }
    Ok(m)
}
