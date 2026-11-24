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
//! The SONOFF Dongle-M's 16 MB product table
//! (`firmware/donglem/partitions-16mb.csv`: 4 MiB slots, 2 MiB `hubdata`)
//! is a [`Layout::Production`] with the 8 MB `hubdata` map; its stock
//! table (no `hubdata`) is foreign.

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
    // firmware/donglem/partitions-16mb.csv (product) and partitions.csv (stock).
    const DONGLE_PRODUCT: [Part<'static>; 6] = [
        data("nvs", 0x9000, 0x14000),
        app("ota_0", subtype::OTA_0, 0x30000, 0x400000),
        app("ota_1", subtype::OTA_1, 0x430000, 0x400000),
        data("hubdata", 0x830000, 0x200000),
        data("otastore", 0xA30000, 0x400000),
        data("radio", 0xE30000, 0x1D0000),
    ];
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
    fn dongle_m_product_table_is_a_production_layout_and_the_stock_one_foreign() {
        let mb16 = 16 << 20;
        assert_eq!(check(mb16, &DONGLE_PRODUCT), Verdict::Ok(Layout::Production));
        assert_eq!(HubdataMap::for_size(0x200000), HubdataMap::for_size(PRODUCTION_HUBDATA));
        assert_eq!(check(MB8, &DONGLE_PRODUCT), Verdict::Fatal(Mismatch::BeyondFlash));
        assert_eq!(check(mb16, &DONGLE_STOCK), Verdict::Foreign);
    }
}
