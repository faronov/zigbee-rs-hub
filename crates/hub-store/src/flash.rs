//! Raw flash regions for the host store.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, RwLock};

/// A NOR-flash region: erase sets a sector to `0xFF`, writes can only
/// clear bits (a write ANDs into the current contents).
pub trait Flash: Send {
    /// Sector (erase unit) size in bytes.
    fn sector_size(&self) -> u32;
    /// Number of sectors.
    fn sectors(&self) -> u32;
    /// Read `buf.len()` bytes at `offset`.
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()>;
    /// Program `data` at `offset`.
    fn write(&mut self, offset: u32, data: &[u8]) -> io::Result<()>;
    /// Erase one sector.
    fn erase(&mut self, sector: u32) -> io::Result<()>;
}

/// RAM flash with NOR semantics and optional power-cut injection.
#[derive(Debug, Clone)]
pub struct MemFlash {
    data: Vec<u8>,
    sector_size: u32,
    /// Remaining byte programs before a simulated power cut (`None` = no
    /// limit). When it reaches 0, further writes and erases fail and the
    /// interrupted write keeps only its first bytes.
    pub budget: Option<usize>,
}

impl MemFlash {
    /// Erased region of `sectors` × `sector_size`.
    pub fn new(sectors: u32, sector_size: u32) -> Self {
        Self { data: vec![0xFF; (sectors * sector_size) as usize], sector_size, budget: None }
    }

    /// Raw contents.
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    fn cut() -> io::Error {
        io::Error::other("power cut (simulated)")
    }
}

impl Flash for MemFlash {
    fn sector_size(&self) -> u32 {
        self.sector_size
    }
    fn sectors(&self) -> u32 {
        self.data.len() as u32 / self.sector_size
    }
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        let o = offset as usize;
        let src = self.data.get(o..o + buf.len()).ok_or(io::ErrorKind::InvalidInput)?;
        buf.copy_from_slice(src);
        Ok(())
    }
    fn write(&mut self, offset: u32, data: &[u8]) -> io::Result<()> {
        let o = offset as usize;
        if o + data.len() > self.data.len() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        for (i, b) in data.iter().enumerate() {
            if let Some(budget) = self.budget.as_mut() {
                if *budget == 0 {
                    return Err(Self::cut());
                }
                *budget -= 1;
            }
            self.data[o + i] &= *b;
        }
        Ok(())
    }
    fn erase(&mut self, sector: u32) -> io::Result<()> {
        if self.budget == Some(0) {
            return Err(Self::cut());
        }
        let s = (sector * self.sector_size) as usize;
        let e = s + self.sector_size as usize;
        self.data.get_mut(s..e).ok_or(io::ErrorKind::InvalidInput)?.fill(0xFF);
        Ok(())
    }
}

/// A file standing in for the `hubdata` journal region (desktop runs).
pub struct FileFlash {
    file: File,
    sectors: u32,
    sector_size: u32,
}

impl FileFlash {
    /// Open or create `path` holding `sectors` × `sector_size` bytes.
    pub fn open(path: &Path, sectors: u32, sector_size: u32) -> io::Result<Self> {
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let want = u64::from(sectors) * u64::from(sector_size);
        let have = file.metadata()?.len();
        if have < want {
            file.seek(SeekFrom::Start(have))?;
            let fill = vec![0xFF; (want - have) as usize];
            file.write_all(&fill)?;
            file.sync_all()?;
        }
        Ok(Self { file, sectors, sector_size })
    }
}

impl Flash for FileFlash {
    fn sector_size(&self) -> u32 {
        self.sector_size
    }
    fn sectors(&self) -> u32 {
        self.sectors
    }
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset.into()))?;
        self.file.read_exact(buf)
    }
    fn write(&mut self, offset: u32, data: &[u8]) -> io::Result<()> {
        let mut cur = vec![0u8; data.len()];
        self.read(offset, &mut cur)?;
        for (c, d) in cur.iter_mut().zip(data) {
            *c &= *d;
        }
        self.file.seek(SeekFrom::Start(offset.into()))?;
        self.file.write_all(&cur)?;
        self.file.sync_data()
    }
    fn erase(&mut self, sector: u32) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(u64::from(sector) * u64::from(self.sector_size)))?;
        self.file.write_all(&vec![0xFF; self.sector_size as usize])?;
        self.file.sync_data()
    }
}

/// A switch for the flash windows the hub's threads write (the Dongle-M's
/// `files` region: the journal, the OTA image store, the catalog): once
/// closed, no write or erase passes it, and [`close`](Self::close) returns
/// only when the writes that were under way have ended. Then the region
/// can be rewritten (the return to stock) while threads that still hold a
/// window (a maintenance job) get errors instead of writing into it.
#[derive(Debug, Clone, Default)]
pub struct WriteGate(Arc<RwLock<bool>>);

impl WriteGate {
    /// An open gate.
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `f` (a write or an erase) unless the gate is closed; the gate
    /// cannot close meanwhile.
    pub fn write<R>(&self, f: impl FnOnce() -> io::Result<R>) -> io::Result<R> {
        let closed = self.0.read().unwrap_or_else(|p| p.into_inner());
        if *closed {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "flash region closed (return to stock)"));
        }
        f()
    }

    /// Closes the gate once the writes under way have ended.
    pub fn close(&self) {
        *self.0.write().unwrap_or_else(|p| p.into_inner()) = true;
    }

    /// Whether it is closed.
    pub fn is_closed(&self) -> bool {
        *self.0.read().unwrap_or_else(|p| p.into_inner())
    }
}

/// `F` with its writes and erases behind a [`WriteGate`].
pub struct Gated<F> {
    flash: F,
    gate: WriteGate,
}

impl<F: Flash> Gated<F> {
    /// `flash` behind `gate`.
    pub fn new(flash: F, gate: WriteGate) -> Self {
        Self { flash, gate }
    }
}

impl<F: Flash> Flash for Gated<F> {
    fn sector_size(&self) -> u32 {
        self.flash.sector_size()
    }
    fn sectors(&self) -> u32 {
        self.flash.sectors()
    }
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        self.flash.read(offset, buf)
    }
    fn write(&mut self, offset: u32, data: &[u8]) -> io::Result<()> {
        let flash = &mut self.flash;
        self.gate.write(|| flash.write(offset, data))
    }
    fn erase(&mut self, sector: u32) -> io::Result<()> {
        let flash = &mut self.flash;
        self.gate.write(|| flash.erase(sector))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// RAM flash shared with the test.
    #[derive(Clone)]
    struct Shared(Arc<Mutex<MemFlash>>);

    impl Flash for Shared {
        fn sector_size(&self) -> u32 {
            self.0.lock().unwrap().sector_size()
        }
        fn sectors(&self) -> u32 {
            self.0.lock().unwrap().sectors()
        }
        fn read(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
            self.0.lock().unwrap().read(offset, buf)
        }
        fn write(&mut self, offset: u32, data: &[u8]) -> io::Result<()> {
            self.0.lock().unwrap().write(offset, data)
        }
        fn erase(&mut self, sector: u32) -> io::Result<()> {
            self.0.lock().unwrap().erase(sector)
        }
    }

    #[test]
    fn a_closed_gate_stops_a_stores_writes() {
        let mem = Shared(Arc::new(Mutex::new(MemFlash::new(32, 4096))));
        let gate = WriteGate::new();
        // A maintenance job holding the store (as NcpNvRecords does).
        let mut store = Store::open(Box::new(Gated::new(mem.clone(), gate.clone()))).unwrap();
        store.kv_put("hub/owner", b"tag").unwrap();
        let before = mem.0.lock().unwrap().bytes().to_vec();
        gate.close();
        assert!(gate.is_closed());
        let e = store.kv_put("ezsp/tx_power", b"8").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert!(mem.0.lock().unwrap().bytes() == &before[..], "nothing written after the close");
    }

    #[test]
    fn close_waits_for_a_write_under_way() {
        struct Slow(Arc<AtomicBool>, Arc<AtomicBool>);
        impl Flash for Slow {
            fn sector_size(&self) -> u32 {
                4096
            }
            fn sectors(&self) -> u32 {
                1
            }
            fn read(&mut self, _: u32, _: &mut [u8]) -> io::Result<()> {
                Ok(())
            }
            fn write(&mut self, _: u32, _: &[u8]) -> io::Result<()> {
                self.0.store(true, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(200));
                self.1.store(true, Ordering::SeqCst);
                Ok(())
            }
            fn erase(&mut self, _: u32) -> io::Result<()> {
                Ok(())
            }
        }
        let (started, ended) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let gate = WriteGate::new();
        let mut f = Gated::new(Slow(started.clone(), ended.clone()), gate.clone());
        let writer = std::thread::spawn(move || f.write(0, &[0]));
        while !started.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        gate.close();
        assert!(ended.load(Ordering::SeqCst), "the close came after the write under way");
        writer.join().unwrap().unwrap();
    }
}
