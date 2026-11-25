//! ESP32-S3 platform glue for the host core: UART1 to the H2 (through the
//! bridge's UART driver: IRAM ISR, 16 KiB RX ring, RTS/CTS on the reserved
//! lines), H2 EN/BOOT, the `hubdata` journal (`zrh_espapp::flash`), the
//! inactive app slot, the hardware RNG and the flash-size check.

use std::ffi::CStr;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use esp_idf_svc::sys;
use hub_core::layout::{self, Part, Verdict};
use hub_ncp::Rng;
use hub_ncp::store::JOURNAL_BYTES;
#[cfg(feature = "ota-store")]
use hub_ncp::store::{Flash, SECTOR};
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

/// The first 128 KiB of the `hubdata` data partition: the host store
/// journal.
pub fn journal() -> Option<zrh_espapp::flash::Region> {
    let r = zrh_espapp::flash::Region::find(c"hubdata", 0, JOURNAL_BYTES);
    if r.is_none() {
        warn!("hubdata missing or smaller than the journal ({JOURNAL_BYTES} B)");
    }
    r
}

#[cfg(feature = "ota-store")]
fn check(err: sys::esp_err_t) -> io::Result<()> {
    if err == sys::ESP_OK { Ok(()) } else { Err(io::Error::other(format!("esp_partition: {err}"))) }
}

/// The inactive app slot of the 8 MB layout: the store of the devices'
/// OTA images (`hub_app::ota_store`). Shared with the hub's own firmware
/// update through `hub_app::ota_store::APP_SLOT`.
#[cfg(feature = "ota-store")]
pub struct InactiveSlot {
    part: *const sys::esp_partition_t,
    sectors: u32,
    /// The slot's otadata entry is invalid (never booted from).
    invalidated: bool,
}

// The partition descriptor is a static ESP-IDF table entry.
#[cfg(feature = "ota-store")]
unsafe impl Send for InactiveSlot {}

#[cfg(feature = "ota-store")]
impl InactiveSlot {
    /// The slot, unless the running app still waits for its confirmation
    /// (then the slot holds the previous app for a rollback).
    pub fn find() -> Result<Self, String> {
        let running = unsafe { sys::esp_ota_get_running_partition() };
        let part = unsafe { sys::esp_ota_get_next_update_partition(core::ptr::null()) };
        if part.is_null() || part == running {
            return Err("no inactive app slot".into());
        }
        let mut state: sys::esp_ota_img_states_t = 0;
        if unsafe { sys::esp_ota_get_state_partition(running, &mut state) } == sys::ESP_OK
            && state == sys::esp_ota_img_states_t_ESP_OTA_IMG_PENDING_VERIFY
        {
            return Err("the running hub firmware is not confirmed yet: the inactive slot keeps the previous one for a rollback".into());
        }
        let size = unsafe { (*part).size };
        let label = unsafe { CStr::from_ptr((*part).label.as_ptr()) }.to_string_lossy().into_owned();
        info!("OTA image store: inactive app slot {label}, {} KiB", size / 1024);
        Ok(Self { part, sectors: size / SECTOR, invalidated: false })
    }
}

#[cfg(feature = "ota-store")]
impl Flash for InactiveSlot {
    fn sector_size(&self) -> u32 {
        SECTOR
    }
    fn sectors(&self) -> u32 {
        self.sectors
    }
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        check(unsafe { sys::esp_partition_read(self.part, offset as usize, buf.as_mut_ptr().cast(), buf.len()) })
    }
    fn write(&mut self, offset: u32, data: &[u8]) -> io::Result<()> {
        check(unsafe { sys::esp_partition_write(self.part, offset as usize, data.as_ptr().cast(), data.len()) })
    }
    fn erase(&mut self, sector: u32) -> io::Result<()> {
        // Before the first change: the bootloader must never fall back to
        // this slot (it no longer holds an app).
        if !self.invalidated {
            self.invalidated = true;
            let e = unsafe { sys::esp_ota_invalidate_inactive_ota_data_slot() };
            if e != sys::ESP_OK {
                info!("OTA image store: otadata of the inactive slot: {e}");
            }
        }
        check(unsafe { sys::esp_partition_erase_range(self.part, (sector * SECTOR) as usize, SECTOR as usize) })
    }
}

/// Detected S3 flash size and the boot check of the partition table
/// (`hub_core::layout`: 8 MB production, 4 MB dev; spec 3.4, decision N9).
pub fn flash_layout() -> (u32, Verdict) {
    let mut size = 0u32;
    unsafe { sys::esp_flash_get_size(core::ptr::null_mut(), &mut size) };
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
    (size, verdict)
}
