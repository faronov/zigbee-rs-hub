//! The catalog store in `hubdata` (device-support 9.1–9.4): two bank
//! slots, the active-bank record, the installed manifest, local (fetched)
//! definitions.
//!
//! Active-bank record: two 4 KiB slots in `catalog_state`, written
//! alternately; the valid one with the higher sequence number counts.
//!
//! ```text
//! 0 "ZCS1" | 4 u32 seq | 8 u8 active slot (0, 1, 0xFF none) | 9..12 0xFF
//! 12 u32 bank size | 16 u32 lowest bank generation accepted (anti-rollback)
//! 20 u32 manifest length (0: none) | 24 u64 lowest manifest version accepted
//! 32 [32] SHA-256 of the bank file | 64 u32 crc32c(bytes 0..64)
//! ```
//!
//! A bank is written into the inactive slot (download or chunked upload),
//! checked (SHA-256 of the file against the manifest, COSE signature of the
//! header with the built-in keys, body hash, every definition decoded and
//! validated), then the record flips. At boot the active bank is checked
//! again; a bank that fails is not used ("no catalog": generic, user and
//! fetched definitions still work).

use std::io;

use hub_core::layout::{Area, HubdataMap};
use hub_devices::bankcat::BankCatalog;
use serde_json::Value as J;
use sha2::{Digest, Sha256};
use zdef::bank::{Bank, Inflate, MinizInflate};
use zdef::codec::DecodeOptions;
use zdef::sign::{Manifest, TrustedKey};
use zigbee_zrhp::crc::crc32c;

use super::data::{AreaWriter, DataFlash, SECTOR};
use super::local::LocalDefs;

const MAGIC: &[u8; 4] = b"ZCS1";

/// The active-bank record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// Sequence number.
    pub seq: u32,
    /// Active slot.
    pub active: Option<u8>,
    /// Size of the active bank file.
    pub size: u32,
    /// Lowest bank generation accepted.
    pub min_generation: u32,
    /// Length of the installed manifest (0: none).
    pub manifest_len: u32,
    /// Lowest manifest version accepted (`date << 16 | serial`).
    pub min_manifest: u64,
    /// SHA-256 of the active bank file.
    pub sha256: [u8; 32],
}

impl Default for Record {
    fn default() -> Self {
        Record { seq: 0, active: None, size: 0, min_generation: 0, manifest_len: 0, min_manifest: 0, sha256: [0; 32] }
    }
}

impl Record {
    fn encode(&self) -> [u8; 68] {
        let mut b = [0xFFu8; 68];
        b[..4].copy_from_slice(MAGIC);
        b[4..8].copy_from_slice(&self.seq.to_le_bytes());
        b[8] = self.active.unwrap_or(0xFF);
        b[12..16].copy_from_slice(&self.size.to_le_bytes());
        b[16..20].copy_from_slice(&self.min_generation.to_le_bytes());
        b[20..24].copy_from_slice(&self.manifest_len.to_le_bytes());
        b[24..32].copy_from_slice(&self.min_manifest.to_le_bytes());
        b[32..64].copy_from_slice(&self.sha256);
        let c = crc32c(&b[..64]);
        b[64..68].copy_from_slice(&c.to_le_bytes());
        b
    }

    fn decode(b: &[u8]) -> Option<Record> {
        if b.len() < 68 || &b[..4] != MAGIC || crc32c(&b[..64]).to_le_bytes() != b[64..68] {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        Some(Record {
            seq: u32_at(4),
            active: (b[8] < 2).then_some(b[8]),
            size: u32_at(12),
            min_generation: u32_at(16),
            manifest_len: u32_at(20),
            min_manifest: u64::from_le_bytes(b[24..32].try_into().unwrap()),
            sha256: b[32..64].try_into().unwrap(),
        })
    }
}

/// Result of checking an installed bank.
#[derive(Debug, Clone)]
pub struct Checked {
    /// Bank generation.
    pub generation: u32,
    /// Catalog date and serial.
    pub version: (u32, u16),
    /// Definitions.
    pub definitions: u16,
    /// Signing key (hex kid).
    pub kid: String,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Layout, body hash and COSE signature of a bank file in place.
#[inline(never)]
pub fn verify_bank(bytes: &[u8], keys: &[TrustedKey]) -> Result<Checked, String> {
    let bank = Bank::parse(bytes).map_err(|e| format!("bank: {e:?}"))?;
    bank.verify(keys).map_err(|e| format!("bank: {e:?}"))?;
    Ok(Checked {
        generation: bank.header.generation,
        version: (bank.header.catalog_date, bank.header.catalog_serial),
        definitions: bank.header.definitions,
        kid: zdef::sign::parse_cose_sign1(bank.signature).map(|s| hex(&s.kid)).unwrap_or_default(),
    })
}

/// Full check of a bank file in place: [`verify_bank`], then every
/// definition decoded and validated (at install).
#[inline(never)]
pub fn check_bank(bytes: &[u8], keys: &[TrustedKey]) -> Result<Checked, String> {
    let checked = verify_bank(bytes, keys)?;
    let bank = Bank::parse(bytes).map_err(|e| format!("bank: {e:?}"))?;
    let mut inf = MinizInflate::new();
    let strings = bank.strings(&mut inf).map_err(|e| format!("strings: {e:?}"))?;
    let mut buf = Box::new([0u8; zdef::bank::BLOCK_MAX]);
    let mut n = 0usize;
    for b in 0..bank.header.blocks {
        let len = bank.inflate_block(&mut inf, b, &mut buf).map_err(|e| format!("block {b}: {e:?}"))?;
        let slots = zdef::bank::block_slots(&buf[..len]).map_err(|e| format!("block {b}: {e:?}"))?;
        for s in 0..slots {
            let item = zdef::bank::block_slot(&buf[..len], s as u8).map_err(|e| format!("block {b}: {e:?}"))?;
            let d = zdef::codec::decode_with(item, &DecodeOptions { strings: Some(&strings) })
                .map_err(|e| format!("block {b} slot {s}: {e}"))?;
            if let Err(v) = d.validate() {
                return Err(format!("block {b} slot {s} ({}): {}", d.id.unwrap_or_default(), v[0]));
            }
            n += 1;
        }
    }
    if n != bank.header.definitions as usize {
        return Err(format!("{n} definitions, the header says {}", bank.header.definitions));
    }
    Ok(checked)
}

/// A bank being written into the inactive slot.
pub struct Install {
    /// Target slot.
    pub slot: u8,
    area: Area,
    writer: AreaWriter,
    hash: Sha256,
    /// Expected file size (from the manifest), if known.
    pub expect_size: Option<u32>,
    /// Expected SHA-256 (from the manifest), if known.
    pub expect_sha: Option<[u8; 32]>,
    keys: Vec<TrustedKey>,
}

impl Install {
    /// Bytes written.
    pub fn written(&self) -> u32 {
        self.writer.len()
    }

    /// Appends a chunk.
    pub fn write(&mut self, f: &mut dyn DataFlash, data: &[u8]) -> io::Result<()> {
        if self.expect_size.is_some_and(|n| self.writer.len() as u64 + data.len() as u64 > n as u64) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "more bytes than the manifest lists"));
        }
        self.writer.write(f, data)?;
        self.hash.update(data);
        Ok(())
    }

    /// Ends the stream and checks the file in place; returns what the
    /// record needs to activate it.
    #[inline(never)]
    pub fn finish(self, f: &dyn DataFlash) -> Result<Installed, String> {
        let keys = self.keys.clone();
        let size = self.writer.len();
        if self.expect_size.is_some_and(|n| n != size) {
            return Err(format!("{size} bytes, the manifest lists {}", self.expect_size.unwrap()));
        }
        let sha: [u8; 32] = self.hash.finalize().into();
        if self.expect_sha.is_some_and(|h| h != sha) {
            return Err("SHA-256 differs from the manifest".into());
        }
        let checked = check_bank(f.map(self.area.offset, size), &keys)?;
        Ok(Installed { slot: self.slot, size, sha256: sha, checked })
    }
}

/// A checked bank in the inactive slot, ready to activate.
#[derive(Debug, Clone)]
pub struct Installed {
    /// Slot.
    pub slot: u8,
    /// File size.
    pub size: u32,
    /// SHA-256 of the file.
    pub sha256: [u8; 32],
    /// What the check found.
    pub checked: Checked,
}

/// Factory of the inflater a catalog uses (ROM `tinfl` on the S3).
pub type InflaterFactory = fn() -> Box<dyn Inflate + Send>;

fn miniz() -> Box<dyn Inflate + Send> {
    Box::new(MinizInflate::new())
}

/// The store.
pub struct CatalogStore {
    /// Areas.
    pub map: HubdataMap,
    /// Current record.
    pub record: Record,
    /// Local definitions.
    pub local: LocalDefs,
    /// Facts of the active bank (from its check).
    pub active: Option<Checked>,
    /// The installed manifest, decoded.
    pub manifest: Option<Manifest>,
    /// Problems found at boot.
    pub warnings: Vec<String>,
    /// Inflater factory.
    pub inflater: InflaterFactory,
    /// A slot is being written (download or upload).
    pub busy: bool,
    /// Trusted signing keys: the built-in slots and the user's own keys
    /// (private catalogs).
    pub keys: Vec<TrustedKey>,
}

impl CatalogStore {
    /// Opens the store; returns it and the active bank's catalog (if one
    /// passes its check).
    #[inline(never)]
    pub fn open(
        f: &mut dyn DataFlash,
        map: HubdataMap,
        keys: Vec<TrustedKey>,
    ) -> io::Result<(CatalogStore, Option<BankCatalog>)> {
        let st = map.catalog_state;
        let recs: Vec<Option<Record>> = (0..2).map(|i| Record::decode(f.map(st.offset + i * SECTOR, 68))).collect();
        let record = recs.iter().flatten().max_by_key(|r| r.seq).copied().unwrap_or_default();
        let local = LocalDefs::open(f, map.local.offset, map.local.size)?;
        let mut s = CatalogStore {
            map,
            record,
            local,
            active: None,
            manifest: None,
            warnings: Vec::new(),
            inflater: miniz,
            busy: false,
            keys,
        };
        if record.manifest_len > 0 && record.manifest_len <= map.manifest.size {
            match Manifest::verify(f.map(map.manifest.offset, record.manifest_len), &s.keys) {
                Ok(m) => s.manifest = Some(m),
                Err(e) => s.warnings.push(format!("installed manifest: {e:?}")),
            }
        }
        let cat = s.open_active(f);
        Ok((s, cat))
    }

    #[inline(never)]
    fn open_active(&mut self, f: &dyn DataFlash) -> Option<BankCatalog> {
        let slot = self.record.active?;
        let area = self.map.bank(slot);
        if self.record.size == 0 || self.record.size > area.size {
            self.warnings.push(format!("bank slot {slot}: size {} out of range", self.record.size));
            return None;
        }
        let bytes = f.map(area.offset, self.record.size);
        // At boot: layout, body hash and signature only; every definition
        // was decoded and validated when the bank was installed.
        match verify_bank(bytes, &self.keys) {
            Ok(c) => self.active = Some(c),
            Err(e) => {
                self.warnings.push(format!("bank slot {slot}: {e} (not used)"));
                return None;
            }
        }
        self.catalog(slot, bytes)
    }

    fn catalog(&mut self, slot: u8, bytes: &'static [u8]) -> Option<BankCatalog> {
        match BankCatalog::open(bytes, self.inflater) {
            Ok(c) => Some(c),
            Err(e) => {
                self.warnings.push(format!("bank slot {slot}: {e}"));
                None
            }
        }
    }

    /// The slot an install writes to.
    pub fn inactive_slot(&self) -> u8 {
        match self.record.active {
            Some(0) => 1,
            _ => 0,
        }
    }

    /// Starts writing a bank into the inactive slot.
    #[inline(never)]
    pub fn begin_install(&mut self, expect_size: Option<u32>, expect_sha: Option<[u8; 32]>) -> Result<Install, String> {
        if self.busy {
            return Err("busy: a catalog update is in progress".into());
        }
        let slot = self.inactive_slot();
        let area = self.map.bank(slot);
        if expect_size.is_some_and(|n| n > area.size) {
            return Err(format!("bank of {} bytes does not fit the {} byte slot", expect_size.unwrap(), area.size));
        }
        self.busy = true;
        Ok(Install {
            slot,
            area,
            writer: AreaWriter::new(area.offset, area.size),
            hash: Sha256::new(),
            expect_size,
            expect_sha,
            keys: self.keys.clone(),
        })
    }

    /// Ends an install without activating (failure, abort).
    pub fn end_install(&mut self) {
        self.busy = false;
    }

    fn write_record(&mut self, f: &mut dyn DataFlash, mut r: Record) -> io::Result<()> {
        r.seq = self.record.seq.wrapping_add(1);
        let at = self.map.catalog_state.offset + (r.seq % 2) * SECTOR;
        f.erase(at, SECTOR)?;
        f.write(at, &r.encode())?;
        self.record = r;
        Ok(())
    }

    /// Makes a checked bank the active one (anti-rollback: its generation
    /// must not be lower than the last accepted one unless `force`).
    #[inline(never)]
    pub fn activate(&mut self, f: &mut dyn DataFlash, ins: Installed, force: bool) -> Result<BankCatalog, String> {
        self.busy = false;
        if ins.slot != self.inactive_slot() {
            return Err("internal: the bank is not in the inactive slot".into());
        }
        if ins.checked.generation < self.record.min_generation && !force {
            return Err(format!(
                "bank generation {} is older than the installed {} (anti-rollback)",
                ins.checked.generation, self.record.min_generation
            ));
        }
        let r = Record {
            active: Some(ins.slot),
            size: ins.size,
            sha256: ins.sha256,
            min_generation: self.record.min_generation.max(ins.checked.generation),
            ..self.record
        };
        self.write_record(f, r).map_err(|e| format!("record: {e}"))?;
        self.active = Some(ins.checked);
        let bytes = f.map(self.map.bank(ins.slot).offset, ins.size);
        self.catalog(ins.slot, bytes).ok_or_else(|| self.warnings.last().cloned().unwrap_or_default())
    }

    /// Installs a verified manifest staged in `scratch` (anti-rollback on
    /// its version).
    #[inline(never)]
    pub fn install_manifest(&mut self, f: &mut dyn DataFlash, len: u32) -> Result<Manifest, String> {
        if len > self.map.manifest.size || len > self.map.scratch.size {
            return Err(format!("manifest of {len} bytes is too large"));
        }
        let m = Manifest::verify(f.map(self.map.scratch.offset, len), &self.keys)
            .map_err(|e| format!("manifest: {e:?}"))?;
        if m.version() < self.record.min_manifest {
            return Err(format!(
                "manifest version {} is older than the installed {}",
                m.version(),
                self.record.min_manifest
            ));
        }
        let mut w = AreaWriter::new(self.map.manifest.offset, self.map.manifest.size);
        let mut buf = [0u8; 512];
        let mut at = 0;
        while at < len {
            let n = (len - at).min(buf.len() as u32) as usize;
            f.read(self.map.scratch.offset + at, &mut buf[..n]).map_err(|e| e.to_string())?;
            w.write(f, &buf[..n]).map_err(|e| e.to_string())?;
            at += n as u32;
        }
        let r = Record { manifest_len: len, min_manifest: self.record.min_manifest.max(m.version()), ..self.record };
        self.write_record(f, r).map_err(|e| format!("record: {e}"))?;
        self.manifest = Some(m.clone());
        Ok(m)
    }

    /// `catalog/status` facts of the store.
    #[inline(never)]
    pub fn status(&self) -> J {
        let (used, half) = self.local.usage();
        obj!({
            "active_slot": self.record.active.map(|s| if s == 0 { "a" } else { "b" }),
            "installed": self.active.as_ref().map(|c| obj!({
                "generation": c.generation, "catalog_date": c.version.0, "catalog_serial": c.version.1,
                "definitions": c.definitions, "kid": c.kid.clone(), "size": self.record.size,
            })),
            "manifest": self.manifest.as_ref().map(manifest_json),
            "slot_size": self.map.bank_a.size,
            "local": obj!({"definitions": self.local.defs().len(), "used": used, "size": half}),
            "busy": self.busy,
            "warnings": self.warnings.clone(),
        })
    }
}

/// A manifest for the API.
pub fn manifest_json(m: &Manifest) -> J {
    obj!({
        "version": m.version(),
        "catalog_date": m.catalog_date,
        "catalog_serial": m.catalog_serial,
                "source": m.source.iter().find(|(k, _)| k == "zhc").map(|(_, v)| v.clone()),
    })
}

/// The bank asset of a manifest for a slot of `slot_size` bytes: the one
/// built for this layout (`bank-8m.zdb` for 576 KiB slots, `bank-4m.zdb`
/// for 256 KiB), else the fitting bank with the most definitions.
pub fn bank_asset(m: &Manifest, slot_size: u32) -> Option<&zdef::sign::ManifestAsset> {
    let own = if slot_size >= 576 * 1024 { "bank-8m.zdb" } else { "bank-4m.zdb" };
    let fits = |b: &&zdef::sign::ManifestAsset| b.size <= slot_size as u64;
    m.banks
        .iter()
        .filter(fits)
        .find(|b| b.name == own)
        .or_else(|| m.banks.iter().filter(fits).max_by_key(|b| (b.definitions, b.size)))
}

/// Parses a bank's header only (for status of an unverified file).
pub fn peek_header(bytes: &[u8]) -> Option<zdef::bank::Header> {
    Bank::parse(bytes).ok().map(|b| b.header)
}
