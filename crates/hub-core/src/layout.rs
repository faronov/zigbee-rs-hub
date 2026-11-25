//! S3 flash layouts and their check at boot (ZRHP spec 3.4/12.4, decision
//! N9 as amended 2026-11-20: 8 MB boards are the product, 4 MB samples get a
//! development layout).
//!
//! | Layout | Flash | App | `hubdata` |
//! |---|---|---|---|
//! | [`Layout::Production`] | 8 MB | `ota_0` + `ota_1`, 3 MiB each (A/B OTA) | 1 920 KiB at 0x620000 |
//! | [`Layout::Dev`] | 4 MB | one `factory` slot, 2 944 KiB, no OTA | 1 MiB at 0x300000 |
//!
//! The tables are `firmware/hub/partitions-8mb.csv` and
//! `partitions-4mb-dev.csv`. A table that does not belong to the detected
//! flash size is a fatal configuration error; any other table (the stock
//! Thread BR table during bring-up) runs with the host store in RAM.
//!
//! The SONOFF Dongle-M keeps SONOFF's stock table (no `hubdata`, so
//! [`Verdict::Foreign`]); its hub data go into the stock `files` partition
//! once the hub has claimed it ([`dongle_files`]).

/// App partition subtypes.
pub mod subtype {
    /// `factory`.
    pub const FACTORY: u8 = 0x00;
    /// `ota_0`.
    pub const OTA_0: u8 = 0x10;
    /// `ota_1`.
    pub const OTA_1: u8 = 0x11;
}

/// Smallest app slot of the production layout (3 MiB).
pub const PRODUCTION_APP: u32 = 3 * 1024 * 1024;
/// App slot of the dev layout (2 944 KiB).
pub const DEV_APP: u32 = 0x2E_0000;
/// `hubdata` of the production layout.
pub const PRODUCTION_HUBDATA: u32 = 0x1E_0000;
/// `hubdata` of the dev layout.
pub const DEV_HUBDATA: u32 = 0x10_0000;

/// One partition table entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Part<'a> {
    /// Label.
    pub label: &'a str,
    /// App partition.
    pub app: bool,
    /// Subtype.
    pub subtype: u8,
    /// Offset.
    pub offset: u32,
    /// Size.
    pub size: u32,
}

/// What the table is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// 8 MB production layout.
    Production,
    /// 4 MB development layout.
    Dev,
    /// Neither (stock table, older layouts).
    Other,
}

/// One area of `hubdata`: offset from the partition start and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Area {
    /// Offset in `hubdata`.
    pub offset: u32,
    /// Size in bytes.
    pub size: u32,
}

impl Area {
    /// First byte after the area.
    pub const fn end(&self) -> u32 {
        self.offset + self.size
    }
}

/// The areas of `hubdata` (device-support 16.1, 16.2). Every area starts
/// and ends on a 4 KiB sector; the journal stays where earlier builds put
/// it, so a firmware update keeps the network and the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubdataMap {
    /// Host store journal (ZRHP host tables, registry, user definitions).
    pub journal: Area,
    /// Catalog state: two 4 KiB slots of the active-bank record (9.1).
    pub catalog_state: Area,
    /// The verified `manifest.cose` of the installed catalog (9.3).
    pub manifest: Area,
    /// Converter bank slot A.
    pub bank_a: Area,
    /// Converter bank slot B.
    pub bank_b: Area,
    /// Fetched definitions and self-description schemas (9.2): two halves
    /// written alternately.
    pub local: Area,
    /// Staging of one downloaded shard (9.3).
    pub scratch: Area,
    /// Web tools: the definition compiler for the web UI (zdefc WebAssembly).
    pub web: Area,
}

const KIB: u32 = 1024;

impl HubdataMap {
    /// The map of a `hubdata` partition of `size` bytes: the 8 MB layout
    /// (1 920 KiB) gets 576 KiB banks and 128 KiB of local definitions, the
    /// 4 MB dev layout (1 MiB) 256 KiB banks and 64 KiB; `None` below 1 MiB.
    pub fn for_size(size: u32) -> Option<HubdataMap> {
        let (bank, local) = if size >= PRODUCTION_HUBDATA {
            (576 * KIB, 128 * KIB)
        } else if size >= DEV_HUBDATA {
            (256 * KIB, 64 * KIB)
        } else {
            return None;
        };
        let mut at = 0;
        let mut area = |n: u32| {
            let a = Area { offset: at, size: n };
            at += n;
            a
        };
        let map = HubdataMap {
            journal: area(128 * KIB),
            catalog_state: area(8 * KIB),
            manifest: area(24 * KIB),
            bank_a: area(bank),
            bank_b: area(bank),
            local: area(local),
            scratch: area(32 * KIB),
            web: area(256 * KIB),
        };
        (map.web.end() <= size).then_some(map)
    }

    /// Bank slot `0` (A) or `1` (B).
    pub fn bank(&self, slot: u8) -> Area {
        if slot == 0 { self.bank_a } else { self.bank_b }
    }

    /// All areas in order, with names.
    pub fn areas(&self) -> [(&'static str, Area); 8] {
        [
            ("journal", self.journal),
            ("catalog_state", self.catalog_state),
            ("manifest", self.manifest),
            ("bank_a", self.bank_a),
            ("bank_b", self.bank_b),
            ("local", self.local),
            ("scratch", self.scratch),
            ("web", self.web),
        ]
    }
}

/// The SONOFF Dongle-M's stock `files` partition (SONOFF's LittleFS:
/// configuration, MG24 update staging; 3 712 KiB at 0xC60000) as the
/// hub's data region. The stock partition table never changes (users
/// install the hub through SONOFF's web console into `ota_1`), so this is
/// the only space the hub can own. It does so only through an explicit
/// claim (docs/dongle-max.md): the claim keeps a copy of SONOFF's LittleFS
/// blocks in [`STOCK_COPY`] for the way back to the stock firmware and
/// replaces the LittleFS superblock pair (blocks 0 and 1) with
/// [`CLAIM`]. Until then the region is only read ([`classify`]). The claim
/// and the way back are `hub_store::files`.
pub mod dongle_files {
    use super::Area;

    /// Partition label.
    pub const LABEL: &str = "files";
    /// Partition size.
    pub const SIZE: u32 = 0x3A_0000;
    /// Claim record, two sectors written alternately (where LittleFS keeps
    /// its superblock pair).
    pub const CLAIM: Area = Area { offset: 0, size: 0x2000 };
    /// `hubdata` window: the 8 MB map ([`super::HubdataMap::for_size`] of
    /// [`super::PRODUCTION_HUBDATA`]).
    pub const HUBDATA: Area = Area { offset: 0x2000, size: super::PRODUCTION_HUBDATA };
    /// Zigbee device OTA images (the store of docs/ota.md); larger images
    /// are streamed from a client.
    pub const OTA_STORE: Area = Area { offset: 0x1E_2000, size: 0x10_0000 };
    /// MG24 firmware image, reserved for the Gecko bootloader updater.
    pub const RADIO: Area = Area { offset: 0x2E_2000, size: 0xA_0000 };
    /// SONOFF's LittleFS blocks in use at the claim (an index sector and up
    /// to 29 blocks), written back by "return to stock". The claim requires
    /// these blocks to be free in SONOFF's file system.
    pub const STOCK_COPY: Area = Area { offset: 0x38_2000, size: 0x1_E000 };
    /// Every area, in order.
    pub const AREAS: [(&str, Area); 5] =
        [("claim", CLAIM), ("hubdata", HUBDATA), ("ota_store", OTA_STORE), ("radio", RADIO), ("stock_copy", STOCK_COPY)];

    /// First bytes of a claim record sector.
    pub const CLAIM_MAGIC: [u8; 4] = *b"ZRHF";
    /// LittleFS v2 superblock: revision count (4 bytes), tag (4 bytes),
    /// then the name "littlefs".
    pub const LITTLEFS_MAGIC: [u8; 8] = *b"littlefs";

    /// What the region holds, from the first bytes of its blocks 0 and 1.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Content {
        /// Erased.
        Blank,
        /// SONOFF's LittleFS (the stock firmware's data).
        LittleFs,
        /// Claimed by the hub.
        Claimed,
        /// Anything else: never claimed without a backup.
        Unknown,
    }

    /// Classify from the first 16 or more bytes of blocks 0 and 1.
    pub fn classify(block0: &[u8], block1: &[u8]) -> Content {
        let claim = |b: &[u8]| b.starts_with(&CLAIM_MAGIC);
        let lfs = |b: &[u8]| b.get(8..16) == Some(&LITTLEFS_MAGIC[..]);
        let blank = |b: &[u8]| b.len() >= 16 && b.iter().all(|&x| x == 0xFF);
        if claim(block0) || claim(block1) {
            Content::Claimed
        } else if lfs(block0) || lfs(block1) {
            Content::LittleFs
        } else if blank(block0) && blank(block1) {
            Content::Blank
        } else {
            Content::Unknown
        }
    }
}

/// Result of the boot check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The layout expected for this flash size.
    Ok(Layout),
    /// Not a hub layout: run with the host store in RAM.
    Foreign,
    /// The table does not belong to this flash size.
    Fatal(Mismatch),
}

/// Why a table is fatal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mismatch {
    /// Partitions end beyond the flash.
    BeyondFlash,
    /// A hub layout for another flash size.
    WrongSize(Layout),
}

/// Classify a table.
pub fn layout(parts: &[Part<'_>]) -> Layout {
    let app = |st: u8| parts.iter().find(|p| p.app && p.subtype == st);
    let hubdata = parts.iter().find(|p| !p.app && p.label == "hubdata").map_or(0, |p| p.size);
    match (app(subtype::OTA_0), app(subtype::OTA_1), app(subtype::FACTORY)) {
        (Some(a), Some(b), None)
            if a.size >= PRODUCTION_APP && b.size >= PRODUCTION_APP && hubdata >= PRODUCTION_HUBDATA =>
        {
            Layout::Production
        }
        (None, None, Some(f)) if f.size >= DEV_APP && hubdata >= DEV_HUBDATA => Layout::Dev,
        _ => Layout::Other,
    }
}

/// The boot check for a detected flash size.
pub fn check(flash_size: u32, parts: &[Part<'_>]) -> Verdict {
    let end = parts.iter().map(|p| p.offset.saturating_add(p.size)).max().unwrap_or(0);
    if end > flash_size {
        return Verdict::Fatal(Mismatch::BeyondFlash);
    }
    match (layout(parts), flash_size >= 8 * 1024 * 1024) {
        (Layout::Production, true) => Verdict::Ok(Layout::Production),
        (Layout::Dev, false) => Verdict::Ok(Layout::Dev),
        (Layout::Other, _) => Verdict::Foreign,
        (l, _) => Verdict::Fatal(Mismatch::WrongSize(l)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn data(label: &str, offset: u32, size: u32) -> Part<'_> {
        Part { label, app: false, subtype: 0x40, offset, size }
    }
    const fn app(label: &str, subtype: u8, offset: u32, size: u32) -> Part<'_> {
        Part { label, app: true, subtype, offset, size }
    }

    // firmware/hub/partitions-8mb.csv and partitions-4mb-dev.csv.
    const PRODUCTION: [Part<'static>; 4] = [
        data("nvs", 0x9000, 0x6000),
        app("ota_0", subtype::OTA_0, 0x20000, 0x300000),
        app("ota_1", subtype::OTA_1, 0x320000, 0x300000),
        data("hubdata", 0x620000, 0x1E0000),
    ];
    const DEV: [Part<'static>; 3] =
        [data("nvs", 0x9000, 0x6000), app("factory", subtype::FACTORY, 0x10000, 0x2E0000), data("hubdata", 0x300000, 0x100000)];
    // The stock Thread BR table of the hwlab board.
    const STOCK: [Part<'static>; 4] = [
        data("nvs", 0x9000, 0x6000),
        app("ota_0", subtype::OTA_0, 0x20000, 0x190000),
        app("ota_1", subtype::OTA_1, 0x1B0000, 0x190000),
        data("rcp_fw", 0x340000, 0x7D000),
    ];
    // firmware/donglem/partitions.csv: SONOFF's stock table.
    const DONGLE_STOCK: [Part<'static>; 4] = [
        data("nvs", 0x1C000, 0x40000),
        app("ota_0", subtype::OTA_0, 0x60000, 0x600000),
        app("ota_1", subtype::OTA_1, 0x660000, 0x600000),
        data("files", 0xC60000, 0x3A0000),
    ];
    const MB4: u32 = 4 << 20;
    const MB8: u32 = 8 << 20;

    #[test]
    fn hubdata_maps_are_contiguous_sector_aligned_and_fit() {
        for (size, bank, local) in [(DEV_HUBDATA, 256 * KIB, 64 * KIB), (PRODUCTION_HUBDATA, 576 * KIB, 128 * KIB)] {
            let m = HubdataMap::for_size(size).unwrap();
            assert_eq!(m.journal, Area { offset: 0, size: 128 * KIB }, "journal stays at the start");
            assert_eq!((m.bank_a.size, m.bank_b.size, m.local.size), (bank, bank, local));
            let mut at = 0;
            for (name, a) in m.areas() {
                assert_eq!(a.offset, at, "{name} follows the previous area");
                assert_eq!(a.offset % 4096, 0, "{name}");
                assert_eq!(a.size % 4096, 0, "{name}");
                at = a.end();
            }
            assert!(at <= size);
            assert_eq!(m.bank(0), m.bank_a);
            assert_eq!(m.bank(1), m.bank_b);
        }
        assert_eq!(HubdataMap::for_size(DEV_HUBDATA).unwrap().web.end(), DEV_HUBDATA, "the 4 MB map is full");
        assert_eq!(HubdataMap::for_size(PRODUCTION_HUBDATA).unwrap().web.end(), 0x1B_0000, "192 KiB spare on 8 MB");
        assert_eq!(HubdataMap::for_size(DEV_HUBDATA - 1), None);
    }

    #[test]
    fn each_layout_belongs_to_its_flash_size() {
        assert_eq!(check(MB8, &PRODUCTION), Verdict::Ok(Layout::Production));
        assert_eq!(check(MB4, &DEV), Verdict::Ok(Layout::Dev));
        assert_eq!(check(MB4, &PRODUCTION), Verdict::Fatal(Mismatch::BeyondFlash));
        assert_eq!(check(MB8, &DEV), Verdict::Fatal(Mismatch::WrongSize(Layout::Dev)));
        assert_eq!(check(MB4, &STOCK), Verdict::Foreign);
    }

    #[test]
    fn dongle_m_stock_table_is_foreign_and_its_files_region_holds_the_hub_areas() {
        assert_eq!(check(16 << 20, &DONGLE_STOCK), Verdict::Foreign);
        let files = DONGLE_STOCK.iter().find(|p| p.label == dongle_files::LABEL).unwrap();
        assert_eq!(files.size, dongle_files::SIZE);
        let mut at = 0;
        for (name, a) in dongle_files::AREAS {
            assert_eq!(a.offset, at, "{name} follows the previous area");
            assert_eq!((a.offset % 4096, a.size % 4096), (0, 0), "{name}");
            at = a.end();
        }
        assert_eq!(at, dongle_files::SIZE, "the areas fill the region");
        assert_eq!(HubdataMap::for_size(dongle_files::HUBDATA.size), HubdataMap::for_size(PRODUCTION_HUBDATA));
        assert!(HubdataMap::for_size(dongle_files::HUBDATA.size).is_some());
    }

    // The stock copy holds an index sector and 29 LittleFS blocks.
    const _: () = assert!(dongle_files::STOCK_COPY.size >= 30 * 4096);

    #[test]
    fn dongle_files_content_is_classified_from_the_first_blocks() {
        use dongle_files::{Content, classify};
        // A LittleFS superblock: revision count, tag, "littlefs" (block 1 may be the newer one).
        let mut lfs = [0xFFu8; 32];
        lfs[..8].copy_from_slice(&[1, 0, 0, 0, 0xF0, 0x0F, 0xFF, 0xF7]);
        lfs[8..16].copy_from_slice(b"littlefs");
        let blank = [0xFFu8; 32];
        let mut claimed = [0xFFu8; 32];
        claimed[..4].copy_from_slice(b"ZRHF");
        assert_eq!(classify(&lfs, &lfs), Content::LittleFs);
        assert_eq!(classify(&blank, &lfs), Content::LittleFs);
        assert_eq!(classify(&blank, &blank), Content::Blank);
        assert_eq!(classify(&claimed, &lfs), Content::Claimed);
        assert_eq!(classify(&[0u8; 32], &blank), Content::Unknown);
        assert_eq!(classify(&blank[..8], &blank[..8]), Content::Unknown, "too short to call blank");
    }
}
