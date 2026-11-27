//! XMODEM-CRC sender (128-byte blocks, CRC-16/XMODEM), as the Gecko
//! bootloader's UART XMODEM receiver expects it (GSDK
//! `platform/bootloader/communication/xmodem-uart/btl_comm_xmodem_common.c`,
//! `xmodem-parser/btl_xmodem.c`; UG489):
//!
//! - the receiver asks for a CRC transfer with `C` once a second (for 60 s);
//! - a packet is `SOH, n, 255 - n, 128 data bytes, CRC high, CRC low`, `n`
//!   from 1, wrapping after 255; it answers `ACK`, `NAK` (bad number,
//!   complement or CRC: send it again), `ACK` for a repeated packet, or
//!   `CAN` (three of them) when the image it parses is refused;
//! - `EOT` ends the transfer: `ACK` when the image was complete and
//!   verified, `CAN` when it was not.
//!
//! The last block is padded with `0xFF` (the bootloader's parser stops at
//! the GBL end tag).

use super::Port;
use super::PortError;

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
/// The receiver's request for a CRC transfer.
pub const CRC_REQUEST: u8 = b'C';
/// Data bytes per packet.
pub const BLOCK: usize = 128;
/// A packet on the wire.
pub const PACKET: usize = BLOCK + 5;
/// Padding of the last block.
pub const PAD: u8 = 0xFF;

/// CRC-16/XMODEM (polynomial 0x1021, initial 0, not reflected).
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Packet `n` (1-based block number, wrapping) with `data` padded.
pub fn packet(n: u32, data: &[u8]) -> [u8; PACKET] {
    let mut p = [PAD; PACKET];
    let num = (n & 0xFF) as u8;
    p[0] = SOH;
    p[1] = num;
    p[2] = 0xFF - num;
    let len = data.len().min(BLOCK);
    p[3..3 + len].copy_from_slice(&data[..len]);
    let crc = crc16(&p[3..3 + BLOCK]);
    p[3 + BLOCK] = (crc >> 8) as u8;
    p[4 + BLOCK] = crc as u8;
    p
}

/// Timeouts and retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Wait this long for the receiver's first `C`.
    pub start_timeout_ms: u32,
    /// Wait this long for the answer to a packet (the Gecko receiver gives
    /// up on a packet after 3 s + 3 s).
    pub ack_timeout_ms: u32,
    /// Sends of one packet before the transfer is cancelled.
    pub retries: u32,
    /// Sends of `EOT`.
    pub eot_retries: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            start_timeout_ms: 10_000,
            ack_timeout_ms: 7_000,
            retries: 10,
            eot_retries: 5,
        }
    }
}

/// A completed transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Report {
    /// Packets sent once each.
    pub blocks: u32,
    /// Packets sent again (NAK or no answer).
    pub resent: u32,
}

/// Why a transfer failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XmodemError {
    /// No `C` from the receiver.
    NoStart,
    /// The receiver cancelled at `block` (0: before the first): the
    /// bootloader refused the image as it parsed it.
    Cancelled {
        /// The block (1-based).
        block: u32,
    },
    /// `block` was not acknowledged after every retry.
    Retries {
        /// The block (1-based).
        block: u32,
    },
    /// The receiver cancelled at `EOT`: the image was not complete or did
    /// not verify.
    Rejected,
    /// No answer to `EOT`.
    EotTimeout,
    /// The image could not be read at `offset`.
    Source {
        /// Offset in the image.
        offset: u32,
    },
    /// The port failed.
    Port(PortError),
}

impl core::fmt::Display for XmodemError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            XmodemError::NoStart => write!(f, "the bootloader did not start the transfer (no 'C')"),
            XmodemError::Cancelled { block } => {
                write!(f, "the bootloader cancelled the transfer at block {block}")
            }
            XmodemError::Retries { block } => {
                write!(f, "block {block} was not acknowledged after every retry")
            }
            XmodemError::Rejected => write!(
                f,
                "the bootloader refused the complete image (not verified)"
            ),
            XmodemError::EotTimeout => write!(f, "no answer to the end of the transfer"),
            XmodemError::Source { offset } => {
                write!(f, "the stored image could not be read at {offset}")
            }
            XmodemError::Port(e) => write!(f, "UART: {}", e.0),
        }
    }
}

impl From<PortError> for XmodemError {
    fn from(e: PortError) -> Self {
        XmodemError::Port(e)
    }
}

/// What came back for a packet.
enum Answer {
    Ack,
    Nak,
    Cancel,
    Timeout,
}

/// Wait for ACK/NAK/CAN; `C` (a start request sent before the receiver saw
/// the packet) and noise are skipped. Two `CAN` in a row cancel.
fn answer<P: Port + ?Sized>(port: &mut P, timeout_ms: u32) -> Result<Answer, PortError> {
    let mut left = timeout_ms;
    let mut cans = 0;
    loop {
        let t0 = port.now_ms();
        let Some(b) = port.read_byte(if cans > 0 { 1000.min(left) } else { left })? else {
            return Ok(if cans > 0 {
                Answer::Cancel
            } else {
                Answer::Timeout
            });
        };
        left = left.saturating_sub(port.now_ms().wrapping_sub(t0));
        match b {
            ACK => return Ok(Answer::Ack),
            NAK => return Ok(Answer::Nak),
            CAN => {
                cans += 1;
                if cans >= 2 {
                    return Ok(Answer::Cancel);
                }
            }
            _ => cans = 0,
        }
        if left == 0 {
            return Ok(if cans > 0 {
                Answer::Cancel
            } else {
                Answer::Timeout
            });
        }
    }
}

/// Sends `len` bytes of an image: `source(offset, buf)` fills `buf` from
/// the image (false: it could not be read), `progress(sent, len)` follows
/// each acknowledged packet. The receiver must be waiting (it sends `C`).
pub fn send<P: Port + ?Sized>(
    port: &mut P,
    len: u32,
    source: &mut dyn FnMut(u32, &mut [u8]) -> bool,
    progress: &mut dyn FnMut(u32, u32),
    cfg: &Config,
) -> Result<Report, XmodemError> {
    // The receiver's start request (its menu text before it is skipped).
    let mut left = cfg.start_timeout_ms;
    let mut cans = 0;
    loop {
        let t0 = port.now_ms();
        let Some(b) = port.read_byte(left)? else {
            return Err(XmodemError::NoStart);
        };
        left = left.saturating_sub(port.now_ms().wrapping_sub(t0));
        match b {
            CRC_REQUEST => break,
            CAN => {
                cans += 1;
                if cans >= 2 {
                    return Err(XmodemError::Cancelled { block: 0 });
                }
            }
            _ => cans = 0,
        }
        if left == 0 {
            return Err(XmodemError::NoStart);
        }
    }
    let mut report = Report::default();
    let blocks = len.div_ceil(BLOCK as u32);
    let mut data = [PAD; BLOCK];
    for i in 0..blocks {
        let offset = i * BLOCK as u32;
        let take = (len - offset).min(BLOCK as u32) as usize;
        data.fill(PAD);
        if !source(offset, &mut data[..take]) {
            cancel(port);
            return Err(XmodemError::Source { offset });
        }
        let p = packet(i + 1, &data);
        let mut tries = 0;
        loop {
            if tries == cfg.retries {
                cancel(port);
                return Err(XmodemError::Retries { block: i + 1 });
            }
            if tries > 0 {
                report.resent += 1;
            }
            tries += 1;
            port.write(&p)?;
            match answer(port, cfg.ack_timeout_ms)? {
                Answer::Ack => break,
                Answer::Nak | Answer::Timeout => {}
                Answer::Cancel => return Err(XmodemError::Cancelled { block: i + 1 }),
            }
        }
        report.blocks += 1;
        progress(offset + take as u32, len);
    }
    for _ in 0..cfg.eot_retries {
        port.write(&[EOT])?;
        match answer(port, cfg.ack_timeout_ms)? {
            Answer::Ack => return Ok(report),
            Answer::Cancel => return Err(XmodemError::Rejected),
            Answer::Nak | Answer::Timeout => {}
        }
    }
    Err(XmodemError::EotTimeout)
}

/// Ends a transfer from the sender's side.
pub fn cancel<P: Port + ?Sized>(port: &mut P) {
    let _ = port.write(&[CAN, CAN, CAN]);
}
