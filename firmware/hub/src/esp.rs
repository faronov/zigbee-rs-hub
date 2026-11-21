//! ESP32-S3 platform glue for the host core: UART1 to the H2 (through the
//! bridge's UART driver: IRAM ISR, 16 KiB RX ring, RTS/CTS on the reserved
//! lines), H2 EN/BOOT, the `hubdata` partition, the hardware RNG and the
//! flash-size check.

use std::ffi::CStr;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use esp_idf_svc::sys;
use hub_core::layout::{self, Layout, Notice, Part, Verdict};
use hub_ncp::Rng;
use hub_ncp::store::{Flash, JOURNAL_BYTES, SECTOR};
use hub_ncp::transport::{Line, NcpControl, PortRx, PortTx};
use log::{error, info, warn};
use tbr_bridge::{h2, uart};

pub struct UartRx;
pub struct UartTx;
pub struct H2Lines;

impl PortRx for UartRx {
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        Ok(uart::read(buf, timeout.as_millis().clamp(1, 1000) as u32))
    }
    fn clear_input(&mut self) -> io::Result<()> {
        // This thread is the only reader, so it may flush directly.
        unsafe { sys::uart_flush_input(uart::PORT) };
        Ok(())
    }
}

impl PortTx for UartTx {
    fn write_all(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let n = uart::write(data);
            if n == 0 {
                return Err(io::Error::other("uart_write_bytes failed"));
            }
            data = &data[n..];
        }
        Ok(())
    }
    fn set_line(&mut self, line: Line) -> io::Result<()> {
        uart::set_baud(line.baud);
        if uart::flow() != line.flow {
            uart::set_flow(line.flow);
        }
        Ok(())
    }
}

impl NcpControl for H2Lines {
    fn reset(&mut self) -> io::Result<()> {
        h2::reset();
        Ok(())
    }
    fn enter_rom_loader(&mut self) -> io::Result<()> {
        h2::enter_download();
        Ok(())
    }
}

pub fn rng() -> Rng {
    Arc::new(|buf: &mut [u8]| unsafe { sys::esp_fill_random(buf.as_mut_ptr().cast(), buf.len()) })
}

/// The first 128 KiB of the `hubdata` data partition.
pub struct Partition {
    part: *const sys::esp_partition_t,
}

// The partition descriptor is a static ESP-IDF table entry.
unsafe impl Send for Partition {}

impl Partition {
    pub fn find() -> Option<Self> {
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
        let size = unsafe { (*part).size };
        if size < JOURNAL_BYTES {
            warn!("hubdata is {size} B, the journal needs {JOURNAL_BYTES}");
            return None;
        }
        Some(Self { part })
    }
}

fn check(err: sys::esp_err_t) -> io::Result<()> {
    if err == sys::ESP_OK { Ok(()) } else { Err(io::Error::other(format!("esp_partition: {err}"))) }
}

impl Flash for Partition {
    fn sector_size(&self) -> u32 {
        SECTOR
    }
    fn sectors(&self) -> u32 {
        JOURNAL_BYTES / SECTOR
    }
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        check(unsafe { sys::esp_partition_read(self.part, offset as usize, buf.as_mut_ptr().cast(), buf.len()) })
    }
    fn write(&mut self, offset: u32, data: &[u8]) -> io::Result<()> {
        check(unsafe { sys::esp_partition_write(self.part, offset as usize, data.as_ptr().cast(), data.len()) })
    }
    fn erase(&mut self, sector: u32) -> io::Result<()> {
        check(unsafe { sys::esp_partition_erase_range(self.part, (sector * SECTOR) as usize, SECTOR as usize) })
    }
}

/// Flash facts of this boot: image header size, chip size, layout, warning.
#[cfg_attr(not(feature = "app"), allow(dead_code))]
pub struct FlashInfo {
    pub image: u32,
    pub chip: u32,
    pub layout: Option<Layout>,
    pub warning: Option<String>,
}

pub static FLASH: std::sync::OnceLock<FlashInfo> = std::sync::OnceLock::new();

fn mb(bytes: u32) -> u32 {
    bytes >> 20
}

fn warning(image: u32, chip: u32, notice: Notice) -> String {
    match notice {
        Notice::OtherImage { want } => format!(
            "This board has {} MB of flash but runs the {} MB image: flash the {} image from the web flasher \
             (tick Erase, then set up Wi-Fi again) for OTA updates and the full storage.",
            mb(chip),
            mb(image),
            if want == Layout::Production { "8 MB" } else { "4 MB" }
        ),
        Notice::Foreign => "Not a hub partition table: the hub keeps its network and devices in RAM only. \
             Flash the hub image for this board from the web flasher (tick Erase)."
            .into(),
        Notice::Fatal(m) => format!(
            "The partition table does not belong to this {} MB flash ({m:?}): the hub does not start. \
             Flash the image for this board.",
            mb(chip.max(image))
        ),
    }
}

/// The flash warning of this boot, if any.
pub fn flash_warning() -> Option<&'static str> {
    FLASH.get()?.warning.as_deref()
}

/// Detected S3 flash size and the boot check of the partition table
/// (`hub_core::layout`: 8 MB production, 4 MB dev; spec 3.4, decision N9).
/// The size in the image header decides the table check; the chip's own
/// size (JEDEC ID) only whether a different image would suit the board.
pub fn flash_layout() -> (u32, Verdict) {
    let mut size = 0u32;
    let mut chip = 0u32;
    unsafe {
        sys::esp_flash_get_size(core::ptr::null_mut(), &mut size);
        if sys::esp_flash_get_physical_size(core::ptr::null_mut(), &mut chip) != sys::ESP_OK {
            chip = 0;
        }
    }
    let mut labels = Vec::new();
    let mut parts = Vec::new();
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
            parts.push((p.type_ == sys::esp_partition_type_t_ESP_PARTITION_TYPE_APP, p.subtype as u8, p.address, p.size));
            labels.push(label);
            it = sys::esp_partition_next(it);
        }
    }
    let parts: Vec<Part<'_>> = labels
        .iter()
        .zip(&parts)
        .map(|(label, &(app, subtype, offset, size))| Part { label, app, subtype, offset, size })
        .collect();
    let verdict = layout::check(size, &parts);
    match verdict {
        Verdict::Ok(l) => info!("S3 flash {} KiB, {l:?} layout", size / 1024),
        Verdict::Foreign => warn!("S3 flash {} KiB, not a hub layout (stock table?): host store in RAM", size / 1024),
        Verdict::Fatal(m) => error!("S3 flash {} KiB: partition table does not belong to this flash ({m:?})", size / 1024),
    }
    let warning = layout::notice(chip, verdict).map(|n| warning(size, chip, n));
    if let Some(w) = &warning {
        warn!("flash: chip {} KiB, image {} KiB: {w}", chip / 1024, size / 1024);
    }
    let layout = match verdict {
        Verdict::Ok(l) => Some(l),
        _ => None,
    };
    let _ = FLASH.set(FlashInfo { image: size, chip, layout, warning });
    (size, verdict)
}
