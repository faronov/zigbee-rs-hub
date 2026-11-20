//! Platform glue: RNG, `hubdata` regions as raw flash, the partition-table
//! check, strapping/eFuse report and the memory report.

use std::ffi::CStr;
use std::io;
use std::sync::Arc;

use esp_idf_svc::sys;
use hub_core::layout::{self, Part, Verdict};
use hub_core::profile::{BoardProfile, Strap};
use hub_ncp::Rng;
use hub_ncp::store::{Flash, SECTOR};
use log::{error, info, warn};

/// Hardware RNG.
pub fn rng() -> Rng {
    Arc::new(|buf: &mut [u8]| unsafe { sys::esp_fill_random(buf.as_mut_ptr().cast(), buf.len()) })
}

/// A window `[offset, offset + size)` of the `hubdata` partition.
pub struct Region {
    part: *const sys::esp_partition_t,
    offset: u32,
    size: u32,
}

// The partition descriptor is a static ESP-IDF table entry.
unsafe impl Send for Region {}

impl Region {
    /// The window, if `hubdata` exists and is large enough.
    pub fn find(offset: u32, size: u32) -> Option<Self> {
        let part = unsafe {
            sys::esp_partition_find_first(
                sys::esp_partition_type_t_ESP_PARTITION_TYPE_DATA,
                sys::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_ANY,
                c"hubdata".as_ptr(),
            )
        };
        if part.is_null() {
            return None;
        }
        let total = unsafe { (*part).size };
        if offset.checked_add(size).is_none_or(|end| end > total) {
            warn!("hubdata is {total} B; the area 0x{offset:X}+{size} does not fit");
            return None;
        }
        Some(Self { part, offset, size })
    }
}

fn check(err: sys::esp_err_t) -> io::Result<()> {
    if err == sys::ESP_OK { Ok(()) } else { Err(io::Error::other(format!("esp_partition: {err}"))) }
}

impl Flash for Region {
    fn sector_size(&self) -> u32 {
        SECTOR
    }
    fn sectors(&self) -> u32 {
        self.size / SECTOR
    }
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        check(unsafe {
            sys::esp_partition_read(self.part, (self.offset + offset) as usize, buf.as_mut_ptr().cast(), buf.len())
        })
    }
    fn write(&mut self, offset: u32, data: &[u8]) -> io::Result<()> {
        check(unsafe {
            sys::esp_partition_write(self.part, (self.offset + offset) as usize, data.as_ptr().cast(), data.len())
        })
    }
    fn erase(&mut self, sector: u32) -> io::Result<()> {
        check(unsafe {
            sys::esp_partition_erase_range(self.part, (self.offset + sector * SECTOR) as usize, SECTOR as usize)
        })
    }
}

/// Detected flash size and the boot check of the partition table.
pub fn flash_layout() -> (u32, Verdict) {
    let mut size = 0u32;
    unsafe { sys::esp_flash_get_size(core::ptr::null_mut(), &mut size) };
    let mut labels = Vec::new();
    let mut raw = Vec::new();
    unsafe {
        let mut it = sys::esp_partition_find(
            sys::esp_partition_type_t_ESP_PARTITION_TYPE_ANY,
            sys::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_ANY,
            core::ptr::null(),
        );
        while !it.is_null() {
            let p = &*sys::esp_partition_get(it);
            let label = CStr::from_ptr(p.label.as_ptr()).to_string_lossy().into_owned();
            info!("partition {label:<10} 0x{:06X} {:>8} B", p.address, p.size);
            raw.push((p.type_ == sys::esp_partition_type_t_ESP_PARTITION_TYPE_APP, p.subtype as u8, p.address, p.size));
            labels.push(label);
            it = sys::esp_partition_next(it);
        }
    }
    let parts: Vec<Part<'_>> = labels
        .iter()
        .zip(&raw)
        .map(|(label, &(app, subtype, offset, size))| Part { label, app, subtype, offset, size })
        .collect();
    let verdict = layout::check(size, &parts);
    match verdict {
        Verdict::Ok(l) => info!("flash {} KiB, {l:?} layout", size / 1024),
        Verdict::Foreign => warn!("flash {} KiB, not a hub layout (stock table?): host store in RAM", size / 1024),
        Verdict::Fatal(m) => error!("flash {} KiB: partition table does not belong to this flash ({m:?})", size / 1024),
    }
    (size, verdict)
}

/// Where the ESP32's VDD_SDIO (flash supply) voltage comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdioVoltage {
    /// The MTDI (GPIO12) strap at reset: low = 3.3 V, high = 1.8 V.
    Strap,
    /// eFuse: 3.3 V, strap ignored.
    Forced33,
    /// eFuse: 1.8 V, strap ignored.
    Forced18,
    /// eFuse: internal regulator off (flash on VDD3P3_RTC), strap ignored.
    ForcedOff,
}

/// Read the VDD_SDIO eFuses (ESP32 eFuse table: XPD_SDIO_REG bit 142,
/// XPD_SDIO_TIEH bit 143, XPD_SDIO_FORCE bit 144, i.e. bits 14–16 of
/// EFUSE_BLK0_RDATA4_REG at 0x3FF5A010; `espefuse.py set_flash_voltage`
/// burns them).
#[cfg(esp32)]
pub fn vdd_sdio() -> SdioVoltage {
    let w = unsafe { core::ptr::read_volatile(0x3FF5_A010 as *const u32) };
    match (w & (1 << 16) != 0, w & (1 << 14) != 0, w & (1 << 15) != 0) {
        (false, _, _) => SdioVoltage::Strap,
        (true, false, _) => SdioVoltage::ForcedOff,
        (true, true, true) => SdioVoltage::Forced33,
        (true, true, false) => SdioVoltage::Forced18,
    }
}

/// Not an ESP32: no MTDI flash-voltage strap.
#[cfg(not(esp32))]
pub fn vdd_sdio() -> SdioVoltage {
    SdioVoltage::Forced33
}

/// Log the strapping pins the board drives and what protects them.
pub fn strapping_report(board: &BoardProfile) -> Vec<String> {
    let mut out = Vec::new();
    for s in board.straps() {
        let hazard = matches!(s, Strap::FlashVoltage(_)) && vdd_sdio() == SdioVoltage::Strap;
        let line = match s {
            Strap::FlashVoltage(g) if hazard => format!(
                "GPIO{g} (MTDI) selects the flash voltage at reset and is not overridden by eFuse: the \
                 firmware drives it low from its first instructions and high only during the ~0.7 s MG24 \
                 bootloader entry; `espefuse.py set_flash_voltage 3.3V` removes the hazard (irreversible, \
                 optional, check the module's flash voltage first)"
            ),
            Strap::FlashVoltage(g) => format!("GPIO{g} (MTDI): VDD_SDIO set by eFuse ({:?}), strap ignored", vdd_sdio()),
            Strap::DownloadMode(g) => format!("GPIO{g}: input only (low at reset = ROM download mode)"),
            Strap::Benign(g) => format!("GPIO{g}: strapping pin, released at reset"),
        };
        if hazard {
            warn!("strap: {line}");
        } else {
            info!("strap: {line}");
        }
        out.push(line);
    }
    out
}

/// Free heap, largest block, internal and PSRAM.
pub fn memory_report() -> String {
    unsafe {
        let internal = sys::heap_caps_get_free_size(sys::MALLOC_CAP_INTERNAL);
        let largest = sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_INTERNAL);
        let min = sys::heap_caps_get_minimum_free_size(sys::MALLOC_CAP_INTERNAL);
        let psram = sys::heap_caps_get_free_size(sys::MALLOC_CAP_SPIRAM);
        let psram_total = sys::heap_caps_get_total_size(sys::MALLOC_CAP_SPIRAM);
        format!(
            "heap: internal free {} KiB (min {} KiB, largest block {} KiB), PSRAM free {} / {} KiB",
            internal / 1024,
            min / 1024,
            largest / 1024,
            psram / 1024,
            psram_total / 1024
        )
    }
}
