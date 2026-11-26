//! Catalog over a signed bank read in place (spec 9.1): the hub
//! memory-maps the active bank from `hubdata` and looks definitions up by
//! the index keys of a fingerprint; only the block buffer, the inflater and
//! the candidates' decoded definitions take heap (format 2 banks keep their
//! string table in flash as well).
//!
//! Fetched definitions (9.2, 9.3) are a [`FetchedCatalog`]: standalone CBOR
//! with their index keys, decoded when a fingerprint asks for them.
//! [`StoreCatalog`] combines both; resolution sees one catalog (9.5: "catalog
//! bank = fetched cache").

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::Value as J;
use zdef::bank::{BLOCK_MAX, Bank, BankError, IndexEntry, Inflate, Strings};
use zdef::codec::{DecodeOptions, peek_summary};
use zdef::matching::{Identity, lookup_hashes};

use crate::fingerprint::Fingerprint;
use crate::model::{Definition, LayerSource};
use crate::resolve::{Catalog, DefSummary};
use crate::zdef_adapter::{adapt, zdef_fingerprint, zdef_gp_fingerprint};

/// A JSON object from `{"key": expression, …}` through `Value::from`:
/// much less code than `serde_json::json!`, which serializes every value
/// through serde (kilobytes of flash on the S3).
macro_rules! obj {
    ({ $($k:literal : $v:expr),* $(,)? }) => {{
        #[allow(unused_mut)]
        let mut m = serde_json::Map::new();
        $( m.insert(String::from($k), serde_json::Value::from($v)); )*
        serde_json::Value::Object(m)
    }};
}

/// Index-key hashes of a device (spec 9.1 "Lookup").
pub fn fingerprint_keys(fp: &Fingerprint) -> Vec<u64> {
    match &fp.gp {
        Some(g) => lookup_hashes(Identity::Gp(&zdef_gp_fingerprint(g))),
        None => lookup_hashes(Identity::Zigbee(&zdef_fingerprint(fp))),
    }
}

/// Where a catalog definition comes from (API: which bank, which slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BankLocation {
    /// Block number.
    pub block: u16,
    /// Slot in the block.
    pub slot: u8,
}

/// Makes an inflater (ROM `tinfl` on the S3, `miniz_oxide` elsewhere).
pub type InflaterFactory = fn() -> Box<dyn Inflate + Send>;

/// The working set of one lookup: inflater state and block buffer, ~15 KB
/// that exist only while a lookup runs (no PSRAM on the dev board).
struct Work {
    inflate: Box<dyn Inflate + Send>,
    buf: Box<[u8; BLOCK_MAX]>,
}

/// A catalog over a bank in memory-mapped flash (or any `'static` bytes).
pub struct BankCatalog {
    bank: Bank<'static>,
    strings: Strings<'static>,
    inflater: InflaterFactory,
}

fn bank_err(e: BankError) -> String {
    format!("{e:?}")
}

impl BankCatalog {
    /// Opens a bank (layout checks; the signature is the store's job, done
    /// once when the bank is installed and at boot).
    pub fn open(bytes: &'static [u8], inflater: InflaterFactory) -> Result<Self, String> {
        let bank = Bank::parse(bytes).map_err(bank_err)?;
        let strings = bank.strings(inflater().as_mut()).map_err(bank_err)?;
        Ok(Self {
            bank,
            strings,
            inflater,
        })
    }

    fn work(&self) -> Work {
        Work {
            inflate: (self.inflater)(),
            buf: Box::new([0; BLOCK_MAX]),
        }
    }

    /// The bank.
    pub fn bank(&self) -> &Bank<'static> {
        &self.bank
    }

    /// Heap the open catalog holds between lookups (the string table of a
    /// format 1 bank; format 2: none).
    pub fn ram_bytes(&self) -> usize {
        self.strings.ram_bytes()
    }

    /// Decodes the definition in `(block, slot)`.
    pub fn load_at(&self, loc: BankLocation) -> Result<Definition, String> {
        let e = IndexEntry {
            key_hash: 0,
            block: loc.block,
            slot: loc.slot,
            flags: 0,
        };
        let zd = {
            let Work {
                mut inflate,
                mut buf,
            } = self.work();
            self.bank
                .load(inflate.as_mut(), &self.strings, &e, &mut buf)
                .map_err(bank_err)?
        };
        let hash = zd
            .encode()
            .ok()
            .map(|b| zdef::ContentHash::of(&b).to_string());
        let mut d = adapt(zd, LayerSource::Catalog, hash).map_err(|e| e.0)?;
        d.source = LayerSource::Catalog;
        Ok(d)
    }

    /// Calls `f(location, item bytes)` for every definition, block by block.
    pub fn for_each(&self, mut f: impl FnMut(BankLocation, &[u8]) -> bool) -> Result<(), String> {
        let Work {
            mut inflate,
            mut buf,
        } = self.work();
        for b in 0..self.bank.header.blocks {
            let n = self
                .bank
                .inflate_block(inflate.as_mut(), b, &mut buf)
                .map_err(bank_err)?;
            let slots = zdef::bank::block_slots(&buf[..n]).map_err(bank_err)?;
            for s in 0..slots {
                let item = zdef::bank::block_slot(&buf[..n], s as u8).map_err(bank_err)?;
                if !f(
                    BankLocation {
                        block: b,
                        slot: s as u8,
                    },
                    item,
                ) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Location of a definition by `id` (a scan of every block, reading only
    /// the identity fields).
    pub fn locate(&self, id: &str) -> Option<BankLocation> {
        let mut found = None;
        let opts = DecodeOptions {
            strings: Some(&self.strings),
        };
        let _ = self.for_each(|loc, item| {
            if peek_summary(item, &opts).is_ok_and(|s| s.id.as_deref() == Some(id)) {
                found = Some(loc);
                return false;
            }
            true
        });
        found
    }

    /// Index entries of a fingerprint, unique by location.
    pub fn locations(&self, fp: &Fingerprint) -> Vec<BankLocation> {
        let mut seen = BTreeSet::new();
        for h in fingerprint_keys(fp) {
            for e in self.bank.lookup(h) {
                seen.insert((e.block, e.slot));
            }
        }
        seen.into_iter()
            .map(|(block, slot)| BankLocation { block, slot })
            .collect()
    }

    /// Bank facts for `catalog/status`.
    pub fn status(&self) -> J {
        let h = &self.bank.header;
        obj!({
            "format": h.format,
            "definitions": h.definitions,
            "index_keys": h.keys,
            "ram_bytes": self.ram_bytes(),
        })
    }
}

impl Catalog for BankCatalog {
    fn candidates(&self, fp: &Fingerprint) -> Vec<Arc<Definition>> {
        self.locations(fp)
            .into_iter()
            .filter_map(|loc| self.load_at(loc).ok())
            .map(Arc::new)
            .collect()
    }

    fn get(&self, id: &str) -> Option<Arc<Definition>> {
        self.load_at(self.locate(id)?).ok().map(Arc::new)
    }

    fn all(&self) -> Vec<Arc<Definition>> {
        // Thousands of definitions: never decoded all at once (see `list`).
        Vec::new()
    }

    fn list(&self, offset: usize, limit: usize, query: Option<&str>) -> (usize, Vec<DefSummary>) {
        let q = query.map(str::to_lowercase);
        let opts = DecodeOptions {
            strings: Some(&self.strings),
        };
        let mut total = 0;
        let mut out = Vec::new();
        let _ = self.for_each(|_, item| {
            let Ok(s) = peek_summary(item, &opts) else {
                return true;
            };
            let hit = q.as_deref().is_none_or(|q| {
                [&s.id, &s.vendor, &s.model]
                    .iter()
                    .any(|f| f.as_deref().is_some_and(|x| x.to_lowercase().contains(q)))
            });
            if hit {
                if total >= offset && out.len() < limit {
                    out.push(DefSummary {
                        id: s.id.unwrap_or_default(),
                        revision: s.revision.unwrap_or(1) as u32,
                        vendor: s.vendor.unwrap_or_default(),
                        model: s.model.unwrap_or_default(),
                        source: LayerSource::Catalog,
                    });
                }
                total += 1;
            }
            true
        });
        (total, out)
    }

    fn status(&self) -> J {
        BankCatalog::status(self)
    }
}

/// One fetched definition: standalone CBOR (as published in the shard) and
/// the index-key hashes it was stored under.
#[derive(Debug, Clone)]
pub struct Fetched {
    /// Index-key hashes (from the shard record).
    pub keys: Vec<u64>,
    /// Standalone CBOR (in place in the local area of `hubdata`).
    pub cbor: &'static [u8],
}

/// Fetched definitions (9.2, 9.3), decoded on demand.
#[derive(Debug, Clone, Default)]
pub struct FetchedCatalog {
    /// Records, newest last.
    pub defs: Vec<Fetched>,
}

impl FetchedCatalog {
    /// The newest record of each definition (a definition stored again
    /// under more keys supersedes its earlier record).
    fn latest(&self) -> impl Iterator<Item = &Fetched> {
        let id = |f: &Fetched| {
            peek_summary(f.cbor, &DecodeOptions::default())
                .ok()
                .and_then(|s| s.id)
        };
        self.defs
            .iter()
            .enumerate()
            .filter(move |(i, f)| {
                let me = id(f);
                me.is_none() || !self.defs[i + 1..].iter().any(|g| id(g) == me)
            })
            .map(|(_, f)| f)
    }

    fn load(f: &Fetched) -> Option<Definition> {
        let mut d = crate::zdef_adapter::load_cbor(f.cbor, LayerSource::Fetched).ok()?;
        d.source = LayerSource::Fetched;
        Some(d)
    }

    /// Bytes held.
    pub fn bytes(&self) -> usize {
        self.defs
            .iter()
            .map(|f| f.cbor.len() + f.keys.len() * 8)
            .sum()
    }
}

impl Catalog for FetchedCatalog {
    fn candidates(&self, fp: &Fingerprint) -> Vec<Arc<Definition>> {
        let keys = fingerprint_keys(fp);
        self.latest()
            .filter(|f| f.keys.iter().any(|k| keys.contains(k)))
            .filter_map(Self::load)
            .map(Arc::new)
            .collect()
    }

    fn get(&self, id: &str) -> Option<Arc<Definition>> {
        self.defs.iter().rev().find_map(|f| {
            let s = zdef::codec::peek_summary(f.cbor, &DecodeOptions::default()).ok()?;
            (s.id.as_deref() == Some(id))
                .then(|| Self::load(f))
                .flatten()
                .map(Arc::new)
        })
    }

    fn all(&self) -> Vec<Arc<Definition>> {
        self.latest().filter_map(Self::load).map(Arc::new).collect()
    }
}

/// The catalog the engine resolves against: the active bank (if any), the
/// fetched definitions, and the built-in examples (hosts without a bank).
#[derive(Default)]
pub struct StoreCatalog {
    /// Active bank.
    pub bank: Option<BankCatalog>,
    /// Fetched definitions.
    pub fetched: FetchedCatalog,
    /// Built-in definitions (examples).
    pub builtin: crate::resolve::MemoryCatalog,
}

impl Catalog for StoreCatalog {
    fn candidates(&self, fp: &Fingerprint) -> Vec<Arc<Definition>> {
        let mut v = self.builtin.candidates(fp);
        if let Some(b) = &self.bank {
            v.extend(b.candidates(fp));
        }
        v.extend(self.fetched.candidates(fp));
        v
    }

    fn get(&self, id: &str) -> Option<Arc<Definition>> {
        self.fetched
            .get(id)
            .or_else(|| self.bank.as_ref().and_then(|b| b.get(id)))
            .or_else(|| self.builtin.get(id))
    }

    fn all(&self) -> Vec<Arc<Definition>> {
        let mut v = self.builtin.all();
        v.extend(self.fetched.all());
        v
    }

    fn list(&self, offset: usize, limit: usize, query: Option<&str>) -> (usize, Vec<DefSummary>) {
        let (mut total, mut out) = crate::resolve::list_defs(&self.all(), offset, limit, query);
        if let Some(b) = &self.bank {
            let skip = offset.saturating_sub(total);
            let room = limit.saturating_sub(out.len());
            let (n, more) = b.list(skip, room, query);
            total += n;
            out.extend(more);
        }
        (total, out)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    fn status(&self) -> J {
        obj!({
            "bank": self.bank.as_ref().map(|b| b.status()),
            "fetched": obj!({"definitions": self.fetched.defs.len(), "bytes": self.fetched.bytes()}),
            "builtin": self.builtin.defs.len(),
        })
    }
}
