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

/// The layout made for a chip of `chip_size` bytes (the size its JEDEC ID
/// reports): [`Layout::Production`] from 8 MB, [`Layout::Dev`] from 4 MB,
/// `None` below (or when the size is unknown).
pub fn layout_for(chip_size: u32) -> Option<Layout> {
    match chip_size {
        s if s >= 8 * 1024 * 1024 => Some(Layout::Production),
        s if s >= 4 * 1024 * 1024 => Some(Layout::Dev),
        _ => None,
    }
}

/// A flash layout warning for the console and the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// A hub image for another flash size than the chip's (the 4 MB image on
    /// an 8 MB board: it runs, without OTA and with less storage); `want` is
    /// the image to flash.
    OtherImage {
        /// The layout for this chip.
        want: Layout,
    },
    /// Not a hub table: the host store is in RAM.
    Foreign,
    /// The table does not belong to the flash: the hub does not start.
    Fatal(Mismatch),
}

/// The boot [`check`] (made with the image header's flash size) against
/// the chip's real size. ESP-IDF itself refuses to start an image whose
/// header names more flash than the chip has, so a too large image never
/// gets here.
pub fn notice(chip_size: u32, verdict: Verdict) -> Option<Notice> {
    match verdict {
        Verdict::Fatal(m) => Some(Notice::Fatal(m)),
        Verdict::Foreign => Some(Notice::Foreign),
        Verdict::Ok(l) => layout_for(chip_size).filter(|&want| want != l).map(|want| Notice::OtherImage { want }),
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
    const MB4: u32 = 4 << 20;
    const MB8: u32 = 8 << 20;

    #[test]
    fn each_layout_belongs_to_its_flash_size() {
        assert_eq!(check(MB8, &PRODUCTION), Verdict::Ok(Layout::Production));
        assert_eq!(check(MB4, &DEV), Verdict::Ok(Layout::Dev));
        assert_eq!(check(MB4, &PRODUCTION), Verdict::Fatal(Mismatch::BeyondFlash));
        assert_eq!(check(MB8, &DEV), Verdict::Fatal(Mismatch::WrongSize(Layout::Dev)));
        assert_eq!(check(MB4, &STOCK), Verdict::Foreign);
    }

    #[test]
    fn notice_compares_the_image_with_the_chip() {
        // The 4 MB release image (header 4 MB) on a retail 8 MB board.
        assert_eq!(notice(MB8, check(MB4, &DEV)), Some(Notice::OtherImage { want: Layout::Production }));
        assert_eq!(notice(MB4, check(MB4, &DEV)), None);
        assert_eq!(notice(MB8, check(MB8, &PRODUCTION)), None);
        assert_eq!(notice(16 << 20, check(MB8, &PRODUCTION)), None, "8 MB image on 16 MB");
        assert_eq!(notice(0, check(MB4, &DEV)), None, "chip size unknown");
        assert_eq!(notice(MB8, check(MB8, &DEV)), Some(Notice::Fatal(Mismatch::WrongSize(Layout::Dev))));
        assert_eq!(notice(MB4, check(MB4, &STOCK)), Some(Notice::Foreign));
    }
}
