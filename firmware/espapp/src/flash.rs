//! A window of a data partition as [`hub_store::Flash`]: 4 KiB sectors,
//! offsets relative to the window, nothing outside it reachable. The S3
//! uses the first 128 KiB of `hubdata` (the host store journal); the
//! Dongle-M uses windows of SONOFF's `files` partition (the whole region
//! for the claim, then the journal and the OTA image store in it).

use std::ffi::CStr;
use std::io;

use esp_idf_svc::sys;
use hub_store::Flash;

/// Sector (erase unit).
pub const SECTOR: u32 = 4096;

/// `size` bytes at `offset` of a data partition.
pub struct Region {
    part: *const sys::esp_partition_t,
    offset: u32,
    sectors: u32,
}

// The partition descriptor is a static ESP-IDF table entry.
unsafe impl Send for Region {}

fn check(err: sys::esp_err_t) -> io::Result<()> {
    if err == sys::ESP_OK { Ok(()) } else { Err(io::Error::other(format!("esp_partition: {err}"))) }
}

/// The data partition `label`.
pub fn partition(label: &CStr) -> Option<*const sys::esp_partition_t> {
    let part = unsafe {
        sys::esp_partition_find_first(
            sys::esp_partition_type_t_ESP_PARTITION_TYPE_DATA,
            sys::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_ANY,
            label.as_ptr(),
        )
    };
    (!part.is_null()).then_some(part)
}

impl Region {
    /// `size` bytes at `offset` of the data partition `label` (both
    /// multiples of [`SECTOR`]); `None` without the partition, or when the
    /// window does not fit it.
    pub fn find(label: &CStr, offset: u32, size: u32) -> Option<Self> {
        let part = partition(label)?;
        let total = unsafe { (*part).size };
        let fits = offset.checked_add(size).is_some_and(|end| end <= total);
        (fits && offset % SECTOR == 0 && size % SECTOR == 0).then_some(Self { part, offset, sectors: size / SECTOR })
    }

    /// Size in bytes.
    pub fn size(&self) -> u32 {
        self.sectors * SECTOR
    }

    fn at(&self, offset: u32, len: usize) -> io::Result<usize> {
        if offset as u64 + len as u64 > u64::from(self.size()) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "beyond the flash window"));
        }
        Ok((self.offset + offset) as usize)
    }
}

impl Flash for Region {
    fn sector_size(&self) -> u32 {
        SECTOR
    }
    fn sectors(&self) -> u32 {
        self.sectors
    }
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        let at = self.at(offset, buf.len())?;
        check(unsafe { sys::esp_partition_read(self.part, at, buf.as_mut_ptr().cast(), buf.len()) })
    }
    fn write(&mut self, offset: u32, data: &[u8]) -> io::Result<()> {
        let at = self.at(offset, data.len())?;
        check(unsafe { sys::esp_partition_write(self.part, at, data.as_ptr().cast(), data.len()) })
    }
    fn erase(&mut self, sector: u32) -> io::Result<()> {
        if sector >= self.sectors {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "beyond the flash window"));
        }
        check(unsafe {
            sys::esp_partition_erase_range(self.part, (self.offset + sector * SECTOR) as usize, SECTOR as usize)
        })
    }
}
