//! Test support: NOR flash with power cuts.

use std::io;

use crate::files::{BLOCK, BLOCKS};
use crate::flash::Flash;

/// NOR flash whose power fails at operation `cut` (counting writes and
/// erases): that write programs only its first `torn` bytes, that
/// erase only the first `torn` bytes of the block; everything after
/// fails until [`CutFlash::power_on`].
#[derive(Clone)]
pub struct CutFlash {
    pub data: Vec<u8>,
    pub ops: usize,
    cut: Option<usize>,
    torn: usize,
    dead: bool,
}

impl CutFlash {
    pub fn new(data: Vec<u8>) -> Self {
        Self { data, ops: 0, cut: None, torn: 0, dead: false }
    }

    pub fn cut_at(mut self, op: usize, torn: usize) -> Self {
        self.cut = Some(op);
        self.torn = torn;
        self
    }

    pub fn power_on(&mut self) {
        self.dead = false;
        self.cut = None;
    }

    /// The operation fails (and the power is gone) if it is the cut.
    fn step(&mut self) -> Option<usize> {
        if self.dead {
            return Some(0);
        }
        let n = self.ops;
        self.ops += 1;
        if self.cut == Some(n) {
            self.dead = true;
            return Some(self.torn);
        }
        None
    }
}

fn cut() -> io::Error {
    io::Error::other("power cut (simulated)")
}

impl Flash for CutFlash {
    fn sector_size(&self) -> u32 {
        BLOCK
    }
    fn sectors(&self) -> u32 {
        BLOCKS
    }
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        if self.dead {
            return Err(cut());
        }
        let o = offset as usize;
        buf.copy_from_slice(&self.data[o..o + buf.len()]);
        Ok(())
    }
    fn write(&mut self, offset: u32, data: &[u8]) -> io::Result<()> {
        let o = offset as usize;
        let n = self.step().unwrap_or(data.len()).min(data.len());
        for (i, b) in data[..n].iter().enumerate() {
            self.data[o + i] &= b;
        }
        if self.dead { Err(cut()) } else { Ok(()) }
    }
    fn erase(&mut self, sector: u32) -> io::Result<()> {
        let s = (sector * BLOCK) as usize;
        let n = self.step().unwrap_or(BLOCK as usize).min(BLOCK as usize);
        self.data[s..s + n].fill(0xFF);
        if self.dead { Err(cut()) } else { Ok(()) }
    }
}


/// The cut points worth a run: all of them, except inside the long
/// runs of block erases, which are sampled.
pub fn cut_points(total: usize) -> impl Iterator<Item = usize> {
    (0..total).filter(move |&k| k < 48 || k + 48 > total || k % 61 == 0)
}

