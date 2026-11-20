//! XMODEM-CRC sender (128-byte blocks), as the Gecko bootloader's UART
//! XMODEM receiver expects it.
//!
//! Packet: `SOH, n, 255 - n, 128 data bytes, CRC-16/XMODEM (big endian)`,
//! `n` starting at 1 and wrapping. The receiver starts the transfer by
//! sending `C`, answers `ACK` or `NAK` per packet and `CAN` to abort; the
//! sender ends with `EOT`. Stop-and-wait, so it needs no flow control: a
//! damaged or lost packet or answer only costs a retransmission (the
//! receiver ACKs a repeated packet without writing it again).

use std::io;
use std::time::{Duration, Instant};

use crate::transport::{PortRx, PortTx};

/// Start of a 128-byte packet.
pub const SOH: u8 = 0x01;
/// End of transmission.
pub const EOT: u8 = 0x04;
/// Acknowledge.
pub const ACK: u8 = 0x06;
/// Negative acknowledge.
pub const NAK: u8 = 0x15;
/// Cancel.
pub const CAN: u8 = 0x18;
/// Receiver ready for XMODEM-CRC.
pub const CRC_READY: u8 = b'C';
/// Data bytes per packet.
pub const BLOCK: usize = 128;
/// Padding of the last packet.
pub const PAD: u8 = 0xFF;

/// CRC-16/XMODEM (poly 0x1021, init 0).
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// One packet for block number `n` (data shorter than [`BLOCK`] is padded).
pub fn packet(n: u8, data: &[u8]) -> [u8; BLOCK + 5] {
    let mut p = [PAD; BLOCK + 5];
    p[0] = SOH;
    p[1] = n;
    p[2] = 255 - n;
    p[3..3 + data.len()].copy_from_slice(data);
    let crc = crc16(&p[3..3 + BLOCK]);
    p[BLOCK + 3..].copy_from_slice(&crc.to_be_bytes());
    p
}

/// Sender settings.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Wait this long for the receiver's first `C`.
    pub start_timeout: Duration,
    /// Wait this long for the answer to one packet.
    pub answer_timeout: Duration,
    /// Attempts per packet (and for `EOT`).
    pub attempts: u32,
}

impl Default for Config {
    fn default() -> Self {
        // A Gecko bootloader may erase a flash page while it holds a
        // packet's ACK; 2 s covers the MG24's page erase many times over.
        Self { start_timeout: Duration::from_secs(10), answer_timeout: Duration::from_secs(2), attempts: 10 }
    }
}

/// Transfer statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Packets acknowledged.
    pub blocks: u32,
    /// Retransmissions (NAK, timeout, garbage).
    pub retries: u32,
}

/// Transfer failure.
#[derive(Debug)]
pub enum XmodemError {
    /// Port error.
    Io(io::Error),
    /// No `C` from the receiver.
    NoStart,
    /// The receiver cancelled.
    Cancelled {
        /// Block that was cancelled (1-based count).
        block: u32,
    },
    /// Too many failed attempts.
    Retries {
        /// Block that failed (1-based count, 0 = `EOT`).
        block: u32,
    },
}

impl From<io::Error> for XmodemError {
    fn from(e: io::Error) -> Self {
        XmodemError::Io(e)
    }
}

impl std::fmt::Display for XmodemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            XmodemError::Io(e) => write!(f, "XMODEM port: {e}"),
            XmodemError::NoStart => write!(f, "XMODEM: the receiver never sent 'C'"),
            XmodemError::Cancelled { block } => write!(f, "XMODEM: cancelled by the receiver at block {block}"),
            XmodemError::Retries { block: 0 } => write!(f, "XMODEM: EOT not acknowledged"),
            XmodemError::Retries { block } => write!(f, "XMODEM: block {block} failed after all retries"),
        }
    }
}

/// Read one byte with a deadline.
fn byte(rx: &mut dyn PortRx, deadline: Instant) -> io::Result<Option<u8>> {
    let mut b = [0u8; 1];
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        if rx.read(&mut b, deadline - now)? == 1 {
            return Ok(Some(b[0]));
        }
    }
}

/// Wait for `C`. Other bytes (the bootloader's text) are skipped.
pub fn wait_start(rx: &mut dyn PortRx, timeout: Duration) -> Result<(), XmodemError> {
    let deadline = Instant::now() + timeout;
    while let Some(b) = byte(rx, deadline)? {
        if b == CRC_READY {
            return Ok(());
        }
    }
    Err(XmodemError::NoStart)
}

/// Send `len` bytes produced by `read(offset, buf)` after the receiver's
/// `C` was seen (see [`wait_start`]). `progress(sent, total)` after every
/// packet.
pub fn send(
    rx: &mut dyn PortRx,
    tx: &mut dyn PortTx,
    len: u32,
    read: &mut dyn FnMut(u32, &mut [u8]) -> io::Result<()>,
    cfg: &Config,
    progress: &mut dyn FnMut(u32, u32),
) -> Result<Stats, XmodemError> {
    let mut stats = Stats::default();
    let blocks = len.div_ceil(BLOCK as u32);
    let mut data = [0u8; BLOCK];
    for i in 0..blocks {
        let off = i * BLOCK as u32;
        let n = (len - off).min(BLOCK as u32) as usize;
        read(off, &mut data[..n])?;
        let pkt = packet((i + 1) as u8, &data[..n]);
        let mut acked = false;
        let mut cans = 0;
        for attempt in 0..cfg.attempts {
            if attempt > 0 {
                stats.retries += 1;
                // A late answer to the previous copy must not be taken for
                // the answer to this one.
                rx.clear_input()?;
            }
            tx.write_all(&pkt)?;
            let deadline = Instant::now() + cfg.answer_timeout;
            let answer = loop {
                match byte(rx, deadline)? {
                    Some(ACK) => break Some(ACK),
                    Some(NAK) => break Some(NAK),
                    Some(CAN) => {
                        cans += 1;
                        if cans >= 2 {
                            return Err(XmodemError::Cancelled { block: i + 1 });
                        }
                    }
                    // A late `C` (the receiver's start timer) or text: wait on.
                    Some(_) => {}
                    None => break None,
                }
            };
            if answer == Some(ACK) {
                acked = true;
                break;
            }
        }
        if !acked {
            let _ = tx.write_all(&[CAN, CAN, CAN]);
            return Err(XmodemError::Retries { block: i + 1 });
        }
        stats.blocks += 1;
        progress(off + n as u32, len);
    }
    for _ in 0..cfg.attempts {
        tx.write_all(&[EOT])?;
        let deadline = Instant::now() + cfg.answer_timeout;
        loop {
            match byte(rx, deadline)? {
                Some(ACK) => return Ok(stats),
                Some(NAK) | None => break,
                Some(_) => {}
            }
        }
        stats.retries += 1;
    }
    Err(XmodemError::Retries { block: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_xmodem_check_value() {
        assert_eq!(crc16(b"123456789"), 0x31C3);
    }

    #[test]
    fn packet_layout() {
        let p = packet(0xFF, b"abc");
        assert_eq!(&p[..6], &[SOH, 0xFF, 0x00, b'a', b'b', b'c']);
        assert!(p[6..BLOCK + 3].iter().all(|&b| b == PAD));
        assert_eq!(u16::from_be_bytes([p[BLOCK + 3], p[BLOCK + 4]]), crc16(&p[3..BLOCK + 3]));
    }
}
