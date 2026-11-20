//! Host flash layouts and their check at boot (ZRHP spec 3.4/12.4, decision
//! N9 as amended 2026-11-20: 8 MB boards are the product, 4 MB samples get a
//! development layout; 16 MB gateways such as the SONOFF Dongle-M get a
//! large production layout).
//!
//! | Layout | Flash | App | `hubdata` |
//! |---|---|---|---|
//! | [`Layout::Production16`] | 16 MB | `ota_0` + `ota_1`, 4 MiB each (A/B OTA) | 8 064 KiB at 0x820000 ([`hubdata16`]) |
//! | [`Layout::Production`] | 8 MB | `ota_0` + `ota_1`, 3 MiB each (A/B OTA) | 1 920 KiB at 0x620000 |
//! | [`Layout::Dev`] | 4 MB | one `factory` slot, 2 944 KiB, no OTA | 1 MiB at 0x300000 |
//!
//! The tables are `firmware/gateway/partitions-16mb.csv`,
//! `firmware/hub/partitions-8mb.csv` and `partitions-4mb-dev.csv`. A table
//! that does not belong to the detected flash size is a fatal configuration
//! error; any other table (a stock table during bring-up) runs with the host
//! store in RAM.

/// App partition subtypes.
pub mod subtype {
    /// `factory`.
    pub const FACTORY: u8 = 0x00;
    /// `ota_0`.
    pub const OTA_0: u8 = 0x10;
    /// `ota_1`.
    pub const OTA_1: u8 = 0x11;
}

/// App slot of the 16 MB production layout (4 MiB).
pub const PRODUCTION16_APP: u32 = 4 * 1024 * 1024;
/// `hubdata` of the 16 MB production layout.
pub const PRODUCTION16_HUBDATA: u32 = 0x7E_0000;
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
    /// 16 MB production layout.
    Production16,
    /// 8 MB production layout.
    Production,
    /// 4 MB development layout.
    Dev,
    /// Neither (stock table, older layouts).
    Other,
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
            if a.size >= PRODUCTION16_APP && b.size >= PRODUCTION16_APP && hubdata >= PRODUCTION16_HUBDATA =>
        {
            Layout::Production16
        }
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
    const MB: u32 = 1024 * 1024;
    match (layout(parts), flash_size) {
        (Layout::Production16, s) if s >= 16 * MB => Verdict::Ok(Layout::Production16),
        // An 8 MB table on a larger flash still works (the rest stays unused).
        (Layout::Production, s) if s >= 8 * MB => Verdict::Ok(Layout::Production),
        (Layout::Dev, s) if s < 8 * MB => Verdict::Ok(Layout::Dev),
        (Layout::Other, _) => Verdict::Foreign,
        (l, _) => Verdict::Fatal(Mismatch::WrongSize(l)),
    }
}

/// The areas of `hubdata` on the 16 MB layout (offsets inside the
/// partition). Device-support 16.1/16.2 sizes scaled up; the two radio
/// banks hold Gecko bootloader images (`.gbl`) for EFR32 NCPs: the
/// last-known-good image and the staged one (`hub_ncp::gecko::bank`).
pub mod hubdata16 {
    /// Host store journal (the store uses its first 128 KiB today).
    pub const JOURNAL: (u32, u32) = (0x00_0000, 0x4_0000);
    /// Configuration: active catalog bank, trusted keys, anti-rollback.
    pub const CONFIG: (u32, u32) = (0x04_0000, 0x1_0000);
    /// Converter (catalog) bank A.
    pub const BANK_A: (u32, u32) = (0x05_0000, 0x10_0000);
    /// Converter (catalog) bank B.
    pub const BANK_B: (u32, u32) = (0x15_0000, 0x10_0000);
    /// Local definitions.
    pub const LOCAL: (u32, u32) = (0x25_0000, 0x8_0000);
    /// Radio image banks A and B (one 4 KiB header sector + the `.gbl`
    /// each; the EFR32MG24 has 1 536 KiB of flash).
    pub const RADIO: (u32, u32) = (0x2D_0000, 2 * RADIO_BANK);
    /// One radio bank.
    pub const RADIO_BANK: u32 = 0x1C_0000;
    /// Every area, in order.
    pub const AREAS: [(u32, u32); 6] = [JOURNAL, CONFIG, BANK_A, BANK_B, LOCAL, RADIO];
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
    // firmware/gateway/partitions-16mb.csv.
    const PRODUCTION16: [Part<'static>; 4] = [
        data("nvs", 0x9000, 0x6000),
        app("ota_0", subtype::OTA_0, 0x20000, 0x400000),
        app("ota_1", subtype::OTA_1, 0x420000, 0x400000),
        data("hubdata", 0x820000, 0x7E0000),
    ];
    const MB4: u32 = 4 << 20;
    const MB8: u32 = 8 << 20;
    const MB16: u32 = 16 << 20;

    #[test]
    fn each_layout_belongs_to_its_flash_size() {
        assert_eq!(check(MB8, &PRODUCTION), Verdict::Ok(Layout::Production));
        assert_eq!(check(MB4, &DEV), Verdict::Ok(Layout::Dev));
        assert_eq!(check(MB4, &PRODUCTION), Verdict::Fatal(Mismatch::BeyondFlash));
        assert_eq!(check(MB8, &DEV), Verdict::Fatal(Mismatch::WrongSize(Layout::Dev)));
        assert_eq!(check(MB4, &STOCK), Verdict::Foreign);
        assert_eq!(check(MB16, &PRODUCTION16), Verdict::Ok(Layout::Production16));
        assert_eq!(check(MB8, &PRODUCTION16), Verdict::Fatal(Mismatch::BeyondFlash));
        assert_eq!(check(MB16, &PRODUCTION), Verdict::Ok(Layout::Production));
        assert_eq!(check(MB16, &DEV), Verdict::Fatal(Mismatch::WrongSize(Layout::Dev)));
    }

    #[test]
    fn hubdata16_areas_are_ordered_aligned_and_fit() {
        let mut end = 0;
        for (off, size) in hubdata16::AREAS {
            assert!(off >= end && off % 0x1000 == 0 && size % 0x1000 == 0);
            end = off + size;
        }
        assert!(end <= PRODUCTION16_HUBDATA);
    }

    // The journal holds the store's 128 KiB; a radio bank holds a header
    // sector plus an image as large as the EFR32MG24's 1 536 KiB flash.
    const _: () = assert!(hubdata16::JOURNAL.1 >= 128 * 1024);
    const _: () = assert!(hubdata16::RADIO_BANK >= 1536 * 1024 + 0x1000);
}
