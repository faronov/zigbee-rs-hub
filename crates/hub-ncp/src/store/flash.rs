//! Raw flash regions for the host store.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

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
