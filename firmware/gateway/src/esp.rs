//! Platform glue: boot report (chip, reset reason, memory, partition
//! table, running slot, straps) and the memory report.

use std::ffi::CStr;

use esp_idf_svc::sys;
use hub_core::layout::dongle_files;
use hub_core::layout::{self, Part, Verdict};
use hub_core::profile::{BoardProfile, HostChip, Strap};
use hub_core::resources::Resources;

/// Where the ESP32's VDD_SDIO (flash and PSRAM supply) voltage comes from.
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

/// The VDD_SDIO eFuses (ESP32 BLK0: XPD_SDIO_REG bit 14, XPD_SDIO_TIEH bit
/// 15, SDIO_FORCE bit 16 of EFUSE_BLK0_RDATA4_REG, 0x3FF5A010).
pub fn vdd_sdio(chip: HostChip) -> SdioVoltage {
    if chip != HostChip::Esp32 {
        return SdioVoltage::Forced33;
    }
    let w = unsafe { core::ptr::read_volatile(0x3FF5_A010 as *const u32) };
    match (w & (1 << 16) != 0, w & (1 << 14) != 0, w & (1 << 15) != 0) {
        (false, _, _) => SdioVoltage::Strap,
        (true, false, _) => SdioVoltage::ForcedOff,
        (true, true, true) => SdioVoltage::Forced33,
        (true, true, false) => SdioVoltage::Forced18,
    }
}

/// The strapping pins the board uses and what protects them.
pub fn strapping_report(board: &BoardProfile) -> Vec<String> {
    let sdio = vdd_sdio(board.host);
    board
        .straps()
        .map(|s| match s {
            Strap::FlashVoltage(g) if sdio == SdioVoltage::Strap => format!(
                "GPIO{g} (MTDI) selects the flash/PSRAM voltage at reset (no eFuse override): driven low from \
                 the first instructions, high only during an NCP bootloader entry"
            ),
            Strap::FlashVoltage(g) => format!("GPIO{g} (MTDI): VDD_SDIO fixed by eFuse ({sdio:?}), strap ignored"),
            Strap::DownloadMode(g) => format!("GPIO{g}: input only (low at reset = ROM download mode)"),
            Strap::Benign(g) => format!("GPIO{g}: strapping pin, released at reset"),
        })
        .collect()
}

fn reset_reason() -> &'static str {
    match unsafe { sys::esp_reset_reason() } {
        sys::esp_reset_reason_t_ESP_RST_POWERON => "power-on",
        sys::esp_reset_reason_t_ESP_RST_EXT => "external pin",
        sys::esp_reset_reason_t_ESP_RST_SW => "software",
        sys::esp_reset_reason_t_ESP_RST_PANIC => "panic",
        sys::esp_reset_reason_t_ESP_RST_INT_WDT => "interrupt watchdog",
        sys::esp_reset_reason_t_ESP_RST_TASK_WDT => "task watchdog",
        sys::esp_reset_reason_t_ESP_RST_WDT => "other watchdog",
        sys::esp_reset_reason_t_ESP_RST_DEEPSLEEP => "deep sleep",
        sys::esp_reset_reason_t_ESP_RST_BROWNOUT => "brownout",
        _ => "other",
    }
}

fn label(p: *const sys::esp_partition_t) -> String {
    if p.is_null() {
        return "-".into();
    }
    let p = unsafe { &*p };
    format!("{} @0x{:06X}", unsafe { CStr::from_ptr(p.label.as_ptr()) }.to_string_lossy(), p.address)
}

/// Detected flash size, the partition table and its verdict
/// (`hub_core::layout`: the Dongle-M's stock table is `Foreign`, i.e. no
/// `hubdata`).
pub fn partitions() -> (u32, Vec<String>, Verdict) {
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
            labels.push(CStr::from_ptr(p.label.as_ptr()).to_string_lossy().into_owned());
            raw.push((p.type_ == sys::esp_partition_type_t_ESP_PARTITION_TYPE_APP, p.subtype as u8, p.address, p.size));
            it = sys::esp_partition_next(it);
        }
    }
    let parts: Vec<Part<'_>> = labels
        .iter()
        .zip(&raw)
        .map(|(label, &(app, subtype, offset, size))| Part { label, app, subtype, offset, size })
        .collect();
    let verdict = layout::check(size, &parts);
    let lines = parts
        .iter()
        .map(|p| format!("{:<9} {} 0x{:02X} 0x{:06X} {:>5} KiB", p.label, if p.app { "app " } else { "data" }, p.subtype, p.offset, p.size / 1024))
        .collect();
    (size, lines, verdict)
}

/// `esp_chip_info_t` (`esp_chip_info.h`, not in esp-idf-sys's bindings).
#[repr(C)]
struct ChipInfo {
    model: u32,
    features: u32,
    /// Major * 100 + minor.
    revision: u16,
    cores: u8,
}

unsafe extern "C" {
    fn esp_chip_info(out: *mut ChipInfo);
}

fn files_text() -> String {
    match crate::files::status() {
        Ok(st) => format!("files ({} KiB): {}", dongle_files::SIZE / 1024, st.describe()),
        Err(e) => format!("files: {e}"),
    }
}

/// The boot report (log and `info`).
pub fn boot_report(board: &BoardProfile) -> Vec<String> {
    let mut out = Vec::new();
    let mut chip = ChipInfo { model: 0, features: 0, revision: 0, cores: 0 };
    unsafe { esp_chip_info(&mut chip) };
    let mut mac = [0u8; 6];
    unsafe { sys::esp_efuse_mac_get_default(mac.as_mut_ptr()) };
    let desc = unsafe { &*sys::esp_app_get_description() };
    let text = |b: &[core::ffi::c_char]| unsafe { CStr::from_ptr(b.as_ptr()) }.to_string_lossy().into_owned();
    out.push(format!(
        "app {} (ESP-IDF {}), chip model {} package {} rev v{}.{}, {} cores, MAC {}, reset: {}",
        text(&desc.version),
        text(&desc.idf_ver),
        chip.model,
        unsafe { sys::esp_efuse_get_pkg_ver() },
        chip.revision / 100,
        chip.revision % 100,
        chip.cores,
        mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":"),
        reset_reason()
    ));
    let psram = unsafe { sys::heap_caps_get_total_size(sys::MALLOC_CAP_SPIRAM) };
    let r = Resources::for_psram(psram);
    out.push(format!("memory mode {} ({} KiB PSRAM; the board has {} KiB)", r.memory_mode(), psram / 1024, board.memory.psram / 1024));
    let (size, parts, verdict) = partitions();
    out.push(format!(
        "flash {} KiB, table {:?}, running {}, boot {}",
        size / 1024,
        verdict,
        label(unsafe { sys::esp_ota_get_running_partition() }),
        label(unsafe { sys::esp_ota_get_boot_partition() })
    ));
    out.extend(parts.into_iter().map(|p| format!("  {p}")));
    out.push(crate::slots::ota0_text());
    out.push(files_text());
    out.extend(strapping_report(board).into_iter().map(|s| format!("strap: {s}")));
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
