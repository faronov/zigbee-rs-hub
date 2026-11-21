//! Raw access to the `hubdata` partition for the catalog areas
//! (`hub_core::layout::HubdataMap`): banks, manifest, local definitions,
//! scratch. Installed areas are read in place through [`DataFlash::map`]
//! (memory-mapped flash on the S3), so a catalog lookup copies nothing.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Flash sector (erase unit).
pub const SECTOR: u32 = 4096;

/// `hubdata` access. Offsets are from the start of the partition.
///
/// Contract of [`map`](Self::map): the returned bytes stay valid for the
/// whole process; their content changes when the area is written. The
/// catalog store never writes or erases an area while a catalog it opened
/// maps it (it installs into the inactive bank slot, swaps, and only then
/// reuses the old slot; local definitions alternate between two halves).
pub trait DataFlash: Send {
    /// Partition size.
    fn size(&self) -> u32;
    /// Reads `buf.len()` bytes at `offset`.
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()>;
    /// Writes `data` at `offset` (erased bytes only, as NOR flash).
    fn write(&mut self, offset: u32, data: &[u8]) -> io::Result<()>;
    /// Erases `[offset, offset + len)`; both multiples of [`SECTOR`].
    fn erase(&mut self, offset: u32, len: u32) -> io::Result<()>;
    /// `[offset, offset + len)` as memory.
    fn map(&self, offset: u32, len: u32) -> &'static [u8];
}

/// Shared handle (the app thread reads and swaps, the download worker
/// writes the inactive areas).
pub type SharedFlash = Arc<Mutex<Box<dyn DataFlash>>>;

fn range(size: u32, offset: u32, len: usize) -> io::Result<()> {
    if (offset as u64) + (len as u64) > size as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "beyond hubdata"));
    }
    Ok(())
}

/// `hubdata` in memory, optionally backed by a file (desktop runs of
/// `hubctl serve --hubdata FILE`, tests). Bytes start erased (0xFF).
pub struct MemData {
    ptr: *mut u8,
    size: u32,
    file: Option<File>,
}

// The buffer is leaked and only reached through this handle (and the
// read-only views of `map`, see the trait contract).
unsafe impl Send for MemData {}

impl MemData {
    /// An erased partition of `size` bytes in RAM.
    pub fn new(size: u32) -> Self {
        let buf = vec![0xFFu8; size as usize].into_boxed_slice();
        Self { ptr: Box::leak(buf).as_mut_ptr(), size, file: None }
    }

    /// A partition image in `path` (created erased, grown to `size`).
    pub fn file(path: &Path, size: u32) -> io::Result<Self> {
        let mut m = Self::new(size);
        let mut f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let len = f.metadata()?.len();
        if len > 0 {
            use io::Read;
            let n = (len as usize).min(size as usize);
            f.read_exact(&mut m.bytes_mut()[..n])?;
        }
        if len < size as u64 {
            use io::{Seek, Write};
            f.seek(io::SeekFrom::Start(len))?;
            f.write_all(&vec![0xFF; size as usize - len as usize])?;
        }
        m.file = Some(f);
        Ok(m)
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: the leaked buffer of `size` bytes, owned by this handle.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.size as usize) }
    }

    fn persist(&mut self, offset: u32, len: usize) -> io::Result<()> {
        let Some(f) = self.file.as_mut() else {
            return Ok(());
        };
        use io::{Seek, Write};
        f.seek(io::SeekFrom::Start(offset as u64))?;
        // SAFETY: as in `bytes_mut`, read-only.
        let data = unsafe { std::slice::from_raw_parts(self.ptr.add(offset as usize), len) };
        f.write_all(data)
    }
}

impl DataFlash for MemData {
    fn size(&self) -> u32 {
        self.size
    }
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        range(self.size, offset, buf.len())?;
        buf.copy_from_slice(&self.bytes_mut()[offset as usize..offset as usize + buf.len()]);
        Ok(())
    }
    fn write(&mut self, offset: u32, data: &[u8]) -> io::Result<()> {
        range(self.size, offset, data.len())?;
        let o = offset as usize;
        for (d, s) in self.bytes_mut()[o..o + data.len()].iter_mut().zip(data) {
            // NOR flash: a write only clears bits.
            *d &= *s;
        }
        self.persist(offset, data.len())
    }
    fn erase(&mut self, offset: u32, len: u32) -> io::Result<()> {
        if offset % SECTOR != 0 || len % SECTOR != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "erase not sector aligned"));
        }
        range(self.size, offset, len as usize)?;
        let o = offset as usize;
        self.bytes_mut()[o..o + len as usize].fill(0xFF);
        self.persist(offset, len as usize)
    }
    fn map(&self, offset: u32, len: u32) -> &'static [u8] {
        let end = (offset.saturating_add(len)).min(self.size);
        let offset = offset.min(end);
        // SAFETY: inside the leaked buffer; see the trait contract.
        unsafe { std::slice::from_raw_parts(self.ptr.add(offset as usize), (end - offset) as usize) }
    }
}

/// Writes a stream into an area, erasing sector by sector ahead of the
/// data (a download never holds more than one chunk in RAM).
pub struct AreaWriter {
    offset: u32,
    size: u32,
    written: u32,
    erased: u32,
}

impl AreaWriter {
    /// A writer for the area `[offset, offset + size)`.
    pub fn new(offset: u32, size: u32) -> Self {
        Self { offset, size, written: 0, erased: 0 }
    }

    /// Bytes written.
    pub fn len(&self) -> u32 {
        self.written
    }

    /// Nothing written yet.
    pub fn is_empty(&self) -> bool {
        self.written == 0
    }

    /// Appends `data`.
    pub fn write(&mut self, f: &mut dyn DataFlash, data: &[u8]) -> io::Result<()> {
        let end = self.written as u64 + data.len() as u64;
        if end > self.size as u64 {
            return Err(io::Error::new(io::ErrorKind::OutOfMemory, "larger than its flash area"));
        }
        let end = end as u32;
        while self.erased < end {
            f.erase(self.offset + self.erased, SECTOR)?;
            self.erased += SECTOR;
        }
        f.write(self.offset + self.written, data)?;
        self.written = end;
        Ok(())
    }
}
