//! The definitions catalog's flash (device-support 9): a window of a data
//! partition (`hubdata` on the S3, the `hubdata` area of the Dongle-M's
//! claimed `files` region) memory-mapped for in-place reads of the banks,
//! local definitions and web tools, written through `esp_partition`. The
//! hub downloads nothing: clients upload the signed release files.

use std::ffi::CStr;
use std::io;

use esp_idf_svc::sys;
use hub_app::catalog::data::DataFlash;
use log::{info, warn};

fn check(err: sys::esp_err_t) -> io::Result<()> {
    if err == sys::ESP_OK { Ok(()) } else { Err(io::Error::other(format!("esp_partition: {err}"))) }
}

/// A window of a data partition: reads in place through one read-only
/// data mapping (made at start, kept for the process), writes and erases
/// through `esp_partition` (which flushes the cache of the range).
pub struct HubData {
    part: *const sys::esp_partition_t,
    /// Window start in the partition.
    offset: u32,
    base: *const u8,
    size: u32,
    /// Writes and erases pass it (the Dongle-M closes it before the
    /// return to stock).
    gate: Option<hub_store::WriteGate>,
}

// The partition descriptor is a static ESP-IDF table entry; the mapping is
// never released.
unsafe impl Send for HubData {}

impl HubData {
    /// Maps the whole `hubdata` partition (the S3); `None` without the
    /// partition or the mapping.
    pub fn open() -> Option<Self> {
        Self::window(c"hubdata", 0, None)
    }

    /// Maps `size` bytes (the rest of the partition when `None`) at
    /// `offset` of the data partition `label`.
    pub fn window(label: &CStr, offset: u32, size: Option<u32>) -> Option<Self> {
        let part = unsafe {
            sys::esp_partition_find_first(
                sys::esp_partition_type_t_ESP_PARTITION_TYPE_DATA,
                sys::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_ANY,
                label.as_ptr(),
            )
        };
        if part.is_null() {
            return None;
        }
        let name = label.to_str().unwrap_or("?");
        let total = unsafe { (*part).size };
        let size = size.unwrap_or(total.saturating_sub(offset));
        if offset.checked_add(size).is_none_or(|end| end > total) {
            warn!("catalog: window 0x{offset:X}+0x{size:X} beyond {name} ({total} B)");
            return None;
        }
        let mut ptr: *const core::ffi::c_void = core::ptr::null();
        let mut handle: sys::esp_partition_mmap_handle_t = 0;
        let err = unsafe {
            sys::esp_partition_mmap(
                part,
                offset as usize,
                size as usize,
                sys::esp_partition_mmap_memory_t_ESP_PARTITION_MMAP_DATA,
                &mut ptr,
                &mut handle,
            )
        };
        if err != sys::ESP_OK {
            warn!("catalog: {name} mmap ({size} B at 0x{offset:X}): {err}");
            return None;
        }
        info!("catalog: {name} at 0x{offset:X} mapped at {ptr:?} ({} KiB)", size / 1024);
        Some(Self { part, offset, base: ptr.cast(), size, gate: None })
    }
}

impl HubData {
    /// Writes and erases behind `gate`.
    pub fn with_gate(mut self, gate: hub_store::WriteGate) -> Self {
        self.gate = Some(gate);
        self
    }

    fn gated(&self, f: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
        match &self.gate {
            Some(g) => g.write(f),
            None => f(),
        }
    }

    /// The partition offset of window offset `offset`, if `len` bytes fit.
    fn at(&self, offset: u32, len: usize) -> io::Result<usize> {
        if offset as u64 + len as u64 > self.size as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "beyond the catalog window"));
        }
        Ok((self.offset + offset) as usize)
    }
}

impl DataFlash for HubData {
    fn size(&self) -> u32 {
        self.size
    }
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        let at = self.at(offset, buf.len())?;
        check(unsafe { sys::esp_partition_read(self.part, at, buf.as_mut_ptr().cast(), buf.len()) })
    }
    fn write(&mut self, offset: u32, data: &[u8]) -> io::Result<()> {
        let at = self.at(offset, data.len())?;
        let part = self.part;
        self.gated(|| check(unsafe { sys::esp_partition_write(part, at, data.as_ptr().cast(), data.len()) }))
    }
    fn erase(&mut self, offset: u32, len: u32) -> io::Result<()> {
        let at = self.at(offset, len as usize)?;
        let part = self.part;
        self.gated(|| check(unsafe { sys::esp_partition_erase_range(part, at, len as usize) }))
    }
    fn map(&self, offset: u32, len: u32) -> &'static [u8] {
        let end = offset.saturating_add(len).min(self.size);
        let offset = offset.min(end);
        // SAFETY: inside the mapping, which lives for the process; see the
        // trait's contract for writes.
        unsafe { core::slice::from_raw_parts(self.base.add(offset as usize), (end - offset) as usize) }
    }
}
