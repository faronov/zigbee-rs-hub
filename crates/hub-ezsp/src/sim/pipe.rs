//! In-memory byte pipe with seeded fault injection.

use std::collections::VecDeque;

use crate::ash::codec::{FLAG, WAKE, is_reserved};

/// Deterministic PRNG (SplitMix64).
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// True with probability `permille / 1000`.
    pub fn chance(&mut self, permille: u16) -> bool {
        permille > 0 && self.below(1000) < u64::from(permille)
    }
}

/// Faults applied per frame (bytes up to and including a Flag Byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Faults {
    pub seed: u64,
    /// Frame lost.
    pub drop_permille: u16,
    /// One bit flipped in one byte, which breaks the CRC.
    pub corrupt_permille: u16,
    /// Frame delivered twice.
    pub duplicate_permille: u16,
    /// Frame held back by up to `max_delay_ms` (order is kept).
    pub delay_permille: u16,
    pub max_delay_ms: u32,
}

impl Faults {
    /// No faults.
    pub fn none() -> Faults {
        Faults::default()
    }

    /// The same rate for every fault kind.
    pub fn uniform(seed: u64, permille: u16, max_delay_ms: u32) -> Faults {
        Faults {
            seed,
            drop_permille: permille,
            corrupt_permille: permille,
            duplicate_permille: permille,
            delay_permille: permille,
            max_delay_ms,
        }
    }
}

/// One-shot fault for the next frame through a pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    Drop,
    Corrupt,
    Duplicate,
    Delay(u32),
}

/// Pipe counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PipeStats {
    pub frames: u32,
    pub dropped: u32,
    pub corrupted: u32,
    pub duplicated: u32,
    pub delayed: u32,
}

/// One direction of the simulated UART. Frames are delivered whole, in
/// order, after a transfer time of about 115 200 bit/s.
#[derive(Debug)]
pub struct Pipe {
    faults: Faults,
    rng: Rng,
    script: VecDeque<Fault>,
    partial: Vec<u8>,
    queue: VecDeque<(u64, Vec<u8>)>,
    carry: VecDeque<u8>,
    last_due: u64,
    stats: PipeStats,
}

impl Pipe {
    pub fn new(faults: Faults) -> Pipe {
        Pipe {
            rng: Rng::new(faults.seed),
            faults,
            script: VecDeque::new(),
            partial: Vec::new(),
            queue: VecDeque::new(),
            carry: VecDeque::new(),
            last_due: 0,
            stats: PipeStats::default(),
        }
    }

    pub fn stats(&self) -> PipeStats {
        self.stats
    }

    pub fn set_faults(&mut self, faults: Faults) {
        self.faults = faults;
    }

    /// Applies `fault` to the next complete frame.
    pub fn inject(&mut self, fault: Fault) {
        self.script.push_back(fault);
    }

    /// Writes bytes at time `now`.
    pub fn push(&mut self, data: &[u8], now: u64) {
        for &b in data {
            self.partial.push(b);
            if b == FLAG {
                let frame = std::mem::take(&mut self.partial);
                self.frame(frame, now);
            }
        }
    }

    fn frame(&mut self, mut frame: Vec<u8>, now: u64) {
        self.stats.frames += 1;
        let scripted = self.script.pop_front();
        let f = self.faults;
        let fault = scripted.or_else(|| {
            if self.rng.chance(f.drop_permille) {
                Some(Fault::Drop)
            } else if self.rng.chance(f.corrupt_permille) {
                Some(Fault::Corrupt)
            } else if self.rng.chance(f.duplicate_permille) {
                Some(Fault::Duplicate)
            } else if f.max_delay_ms > 0 && self.rng.chance(f.delay_permille) {
                Some(Fault::Delay(
                    1 + self.rng.below(u64::from(f.max_delay_ms)) as u32,
                ))
            } else {
                None
            }
        });
        let mut extra_delay = 0;
        let mut copies = 1;
        match fault {
            Some(Fault::Drop) => {
                self.stats.dropped += 1;
                return;
            }
            Some(Fault::Corrupt) if self.corrupt(&mut frame) => self.stats.corrupted += 1,
            Some(Fault::Duplicate) => {
                self.stats.duplicated += 1;
                copies = 2;
            }
            Some(Fault::Delay(ms)) => {
                self.stats.delayed += 1;
                extra_delay = u64::from(ms);
            }
            Some(Fault::Corrupt) | None => {}
        }
        let transfer = 1 + frame.len() as u64 / 11;
        for _ in 0..copies {
            let due = self.last_due.max(now + extra_delay) + transfer;
            self.last_due = due;
            self.queue.push_back((due, frame.clone()));
        }
    }

    /// Writes `data` at time `now` as one unit, outside the frame stream and
    /// without faults (XON/XOFF a UART inserts), in order with the frames.
    pub fn push_raw(&mut self, data: &[u8], now: u64) {
        let due = self.last_due.max(now) + 1;
        self.last_due = due;
        self.queue.push_back((due, data.to_vec()));
    }

    /// Flips one bit of a non-reserved byte so that it stays non-reserved:
    /// framing is kept and only the CRC check fails.
    fn corrupt(&mut self, frame: &mut [u8]) -> bool {
        let candidates: Vec<usize> = (0..frame.len())
            .filter(|&i| !is_reserved(frame[i]) && frame[i] != WAKE)
            .collect();
        if candidates.is_empty() {
            return false;
        }
        let i = candidates[self.rng.below(candidates.len() as u64) as usize];
        let start = self.rng.below(8) as u32;
        for k in 0..8 {
            let v = frame[i] ^ (1 << ((start + k) % 8));
            if !is_reserved(v) && v != WAKE {
                frame[i] = v;
                return true;
            }
        }
        false
    }

    /// Time of the next delivery, if any.
    pub fn next_due(&self) -> Option<u64> {
        if !self.carry.is_empty() {
            return Some(0);
        }
        self.queue.front().map(|(t, _)| *t)
    }

    /// Reads delivered bytes into `buf`.
    pub fn read(&mut self, now: u64, buf: &mut [u8]) -> usize {
        while let Some((t, _)) = self.queue.front() {
            if *t > now {
                break;
            }
            let (_, f) = self.queue.pop_front().expect("front exists");
            self.carry.extend(f);
        }
        let n = buf.len().min(self.carry.len());
        for (dst, src) in buf.iter_mut().zip(self.carry.drain(..n)) {
            *dst = src;
        }
        n
    }

    /// Drops everything in flight.
    pub fn clear(&mut self) {
        self.partial.clear();
        self.queue.clear();
        self.carry.clear();
    }
}
