//! The definitions catalog on the S3 (device-support 9): `hubdata`
//! memory-mapped for in-place reads of the banks, local definitions and web
//! tools, written through `esp_partition`. The hub downloads nothing:
//! clients upload the signed release files through the API.

use std::io;

use esp_idf_svc::sys;
use hub_app::catalog::data::DataFlash;
use log::{info, warn};

fn check(err: sys::esp_err_t) -> io::Result<()> {
    if err == sys::ESP_OK { Ok(()) } else { Err(io::Error::other(format!("esp_partition: {err}"))) }
}

/// The whole `hubdata` partition: reads in place through one read-only
/// data mapping (made at start, kept for the process), writes and erases
/// through `esp_partition` (which flushes the cache of the range).
pub struct HubData {
    part: *const sys::esp_partition_t,
    base: *const u8,
    size: u32,
}

// The partition descriptor is a static ESP-IDF table entry; the mapping is
// never released.
unsafe impl Send for HubData {}

impl HubData {
    /// Maps `hubdata`; `None` without the partition or the mapping.
    pub fn open() -> Option<Self> {
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
        let mut ptr: *const core::ffi::c_void = core::ptr::null();
        let mut handle: sys::esp_partition_mmap_handle_t = 0;
        let err = unsafe {
            sys::esp_partition_mmap(
                part,
                0,
                size as usize,
                sys::esp_partition_mmap_memory_t_ESP_PARTITION_MMAP_DATA,
                &mut ptr,
                &mut handle,
            )
        };
        if err != sys::ESP_OK {
            warn!("catalog: hubdata mmap ({size} B): {err}");
            return None;
        }
        info!("catalog: hubdata mapped at {ptr:?} ({} KiB)", size / 1024);
        Some(Self { part, base: ptr.cast(), size })
    }
}

impl DataFlash for HubData {
    fn size(&self) -> u32 {
        self.size
    }
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        check(unsafe { sys::esp_partition_read(self.part, offset as usize, buf.as_mut_ptr().cast(), buf.len()) })
    }
    fn write(&mut self, offset: u32, data: &[u8]) -> io::Result<()> {
        check(unsafe { sys::esp_partition_write(self.part, offset as usize, data.as_ptr().cast(), data.len()) })
    }
    fn erase(&mut self, offset: u32, len: u32) -> io::Result<()> {
        check(unsafe { sys::esp_partition_erase_range(self.part, offset as usize, len as usize) })
    }
    fn map(&self, offset: u32, len: u32) -> &'static [u8] {
        let end = offset.saturating_add(len).min(self.size);
        let offset = offset.min(end);
        // SAFETY: inside the mapping, which lives for the process; see the
        // trait's contract for writes.
        unsafe { core::slice::from_raw_parts(self.base.add(offset as usize), (end - offset) as usize) }
    }
}
