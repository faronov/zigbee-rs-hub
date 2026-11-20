//! NCP firmware update through the Silicon Labs Gecko bootloader (UART
//! XMODEM), for EFR32 NCPs such as the EFR32MG24 of the SONOFF Dongle-M.
//! The alternative to `FW_*` A/B updates ([`fw`](crate::fw)) for NCPs
//! whose flash layout is the Gecko bootloader's: the host keeps the images
//! ([`bank`]: last-known-good + staged in `hubdata`) and rolls back by
//! installing the last-known-good image again ([`job`]).
//!
//! The bootloader's UART interface (Silicon Labs UG266/UG489) is a text
//! menu at a fixed rate (115 200 8N1, no flow control):
//!
//! ```text
//! \r\nGecko Bootloader v2.4.2\r\n1. upload gbl\r\n2. run\r\n3. ebl info\r\nBL >
//! ```
//!
//! `1` answers `\r\nbegin upload\r\n` and then `C` until an XMODEM-CRC
//! transfer starts ([`xmodem`]); after `EOT` the bootloader prints
//! `\r\nSerial upload complete\r\n` (or `aborted`) and the menu again; `2`
//! boots the application. The menu is printed after a newline.
//!
//! [`install`] runs the whole sequence: check the image ([`gbl`]: framing,
//! CRC, no bootloader or Secure Engine upgrade), switch the host UART to
//! the bootloader rate, enter the bootloader with the board's lines
//! ([`NcpControl::enter_rom_loader`]: on the Dongle-M BOOT=GPIO12 high,
//! RESET=GPIO15 pulse, BOOT released 500 ms later), probe the menu, upload,
//! run. [`sim`] is a simulated bootloader for the host tests.

pub mod bank;
pub mod gbl;
pub mod job;
#[cfg(any(test, feature = "sim"))]
pub mod sim;
pub mod xmodem;

use std::io;
use std::time::{Duration, Instant};

use crate::transport::{Line, NcpControl, PortRx, PortTx};
use gbl::{GblError, GblInfo};
use xmodem::XmodemError;

/// Random-access image (a radio bank, a file, a buffer).
pub trait ImageSource {
    /// Length in bytes.
    fn size(&self) -> u32;
    /// Read `buf.len()` bytes at `offset`.
    fn read_at(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()>;
}

impl ImageSource for &[u8] {
    fn size(&self) -> u32 {
        self.len() as u32
    }
    fn read_at(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        let o = offset as usize;
        let src = self.get(o..o + buf.len()).ok_or(io::ErrorKind::InvalidInput)?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

/// Bootloader client settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// Bootloader UART rate.
    pub baud: u32,
    /// Wait this long for the menu after a newline.
    pub menu_timeout: Duration,
    /// Newlines sent before giving up on the menu.
    pub menu_attempts: u32,
    /// Wait this long for the upload status after `EOT` (the bootloader
    /// may still be writing the last pages).
    pub status_timeout: Duration,
    /// XMODEM settings.
    pub xmodem: xmodem::Config,
    /// Accept images that upgrade the bootloader or the Secure Engine. The
    /// hub never sets it: a broken bootloader cannot be recovered without
    /// SWD.
    pub allow_bootloader: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            baud: 115_200,
            menu_timeout: Duration::from_secs(2),
            menu_attempts: 4,
            status_timeout: Duration::from_secs(30),
            xmodem: xmodem::Config::default(),
            allow_bootloader: false,
        }
    }
}

/// Update failure.
#[derive(Debug)]
pub enum GeckoError {
    /// Port or line error.
    Io(io::Error),
    /// Not a valid GBL.
    Image(GblError),
    /// The image would replace the bootloader or the Secure Engine.
    BootloaderUpgrade,
    /// No bootloader menu.
    NoMenu,
    /// The bootloader did not start the upload.
    NoUploadStart,
    /// XMODEM failure.
    Xmodem(XmodemError),
    /// The bootloader rejected the image (its message).
    Aborted(String),
    /// No upload status after `EOT`.
    NoStatus,
}

impl From<io::Error> for GeckoError {
    fn from(e: io::Error) -> Self {
        GeckoError::Io(e)
    }
}

impl From<XmodemError> for GeckoError {
    fn from(e: XmodemError) -> Self {
        match e {
            XmodemError::Io(e) => GeckoError::Io(e),
            e => GeckoError::Xmodem(e),
        }
    }
}

impl std::fmt::Display for GeckoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GeckoError::Io(e) => write!(f, "bootloader port: {e}"),
            GeckoError::Image(e) => write!(f, "{e}"),
            GeckoError::BootloaderUpgrade => {
                write!(f, "the image upgrades the bootloader or Secure Engine; refused (never replace the Gecko bootloader from the hub)")
            }
            GeckoError::NoMenu => write!(f, "no Gecko bootloader menu (wrong rate, lines or no bootloader)"),
            GeckoError::NoUploadStart => write!(f, "the bootloader did not start the upload"),
            GeckoError::Xmodem(e) => write!(f, "{e}"),
            GeckoError::Aborted(m) => write!(f, "the bootloader aborted the upload: {m}"),
            GeckoError::NoStatus => write!(f, "no upload status from the bootloader"),
        }
    }
}

/// The bootloader menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    /// `Gecko Bootloader v…` version text (empty if not printed).
    pub version: String,
}

/// Talks to the bootloader menu.
pub struct Gecko<'a> {
    rx: &'a mut dyn PortRx,
    tx: &'a mut dyn PortTx,
    cfg: &'a Config,
    text: Vec<u8>,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

impl<'a> Gecko<'a> {
    /// A client on a port already at the bootloader rate.
    pub fn new(rx: &'a mut dyn PortRx, tx: &'a mut dyn PortTx, cfg: &'a Config) -> Self {
        Self { rx, tx, cfg, text: Vec::new() }
    }

    /// Read until one of `pats` appears; returns its index and the text
    /// before it, and keeps the text after the match in the buffer.
    fn until(&mut self, pats: &[&[u8]], timeout: Duration) -> io::Result<Option<(usize, Vec<u8>)>> {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 256];
        loop {
            for (i, p) in pats.iter().enumerate() {
                if let Some(at) = find(&self.text, p) {
                    let before = self.text[..at].to_vec();
                    self.text.drain(..at + p.len());
                    return Ok(Some((i, before)));
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let n = self.rx.read(&mut buf, deadline - now)?;
            self.text.extend_from_slice(&buf[..n]);
            if self.text.len() > 4096 {
                self.text.drain(..2048);
            }
        }
    }

    /// Send a newline until the menu prompt appears.
    pub fn probe(&mut self) -> Result<Menu, GeckoError> {
        for _ in 0..self.cfg.menu_attempts {
            self.text.clear();
            self.tx.write_all(b"\r\n")?;
            let deadline = Instant::now() + self.cfg.menu_timeout;
            let mut seen = Vec::new();
            let mut buf = [0u8; 256];
            while Instant::now() < deadline {
                let n = self.rx.read(&mut buf, deadline - Instant::now())?;
                seen.extend_from_slice(&buf[..n]);
                if let Some(at) = find(&seen, b"BL >") {
                    self.text = seen[at + 4..].to_vec();
                    let version = find(&seen, b"Gecko Bootloader v")
                        .map(|v| {
                            let rest = &seen[v + 18..];
                            let end = rest.iter().position(|&b| b == b'\r' || b == b'\n').unwrap_or(rest.len());
                            String::from_utf8_lossy(&rest[..end]).into_owned()
                        })
                        .unwrap_or_default();
                    return Ok(Menu { version });
                }
            }
        }
        Err(GeckoError::NoMenu)
    }

    /// Menu option 1: upload `len` bytes of `image` by XMODEM and wait for
    /// the bootloader's verdict.
    pub fn upload(
        &mut self,
        image: &mut dyn ImageSource,
        len: u32,
        progress: &mut dyn FnMut(u32, u32),
    ) -> Result<xmodem::Stats, GeckoError> {
        self.text.clear();
        self.tx.write_all(b"1")?;
        if self.until(&[b"begin upload"], self.cfg.menu_timeout)?.is_none() {
            return Err(GeckoError::NoUploadStart);
        }
        // The first `C` may already be in the buffer.
        if !self.text.contains(&xmodem::CRC_READY) {
            xmodem::wait_start(self.rx, self.cfg.xmodem.start_timeout)?;
        }
        self.text.clear();
        let stats = xmodem::send(
            self.rx,
            self.tx,
            len,
            &mut |off, buf| image.read_at(off, buf),
            &self.cfg.xmodem,
            progress,
        )?;
        match self.until(&[b"Serial upload complete", b"Serial upload aborted"], self.cfg.status_timeout)? {
            Some((0, _)) => {
                let _ = self.until(&[b"BL >"], self.cfg.menu_timeout)?;
                Ok(stats)
            }
            Some(_) => {
                // The bootloader's error text, up to the menu it prints next.
                let text = self.until(&[b"BL >"], self.cfg.menu_timeout)?.map(|(_, t)| t).unwrap_or_default();
                let end = find(&text, b"Gecko Bootloader").or_else(|| find(&text, b"1. upload")).unwrap_or(text.len());
                let msg = String::from_utf8_lossy(&text[..end]).trim().to_string();
                Err(GeckoError::Aborted(if msg.is_empty() { "Serial upload aborted".into() } else { msg }))
            }
            None => Err(GeckoError::NoStatus),
        }
    }

    /// Menu option 2: boot the application.
    pub fn run(&mut self) -> io::Result<()> {
        self.tx.write_all(b"2")
    }
}

/// Check an image as the hub requires it.
pub fn check_image(image: &mut dyn ImageSource, cfg: &Config) -> Result<GblInfo, GeckoError> {
    let info = bank::check_source(image).map_err(|e| match e {
        bank::BankError::Image(e) => GeckoError::Image(e),
        bank::BankError::Io(e) => GeckoError::Io(e),
        e => GeckoError::Io(io::Error::other(e.to_string())),
    })?;
    if (info.bootloader || info.se_upgrade) && !cfg.allow_bootloader {
        return Err(GeckoError::BootloaderUpgrade);
    }
    Ok(info)
}

/// A finished install.
#[derive(Debug, Clone)]
pub struct Report {
    /// Bootloader menu.
    pub menu: Menu,
    /// The image.
    pub info: GblInfo,
    /// Transfer statistics.
    pub stats: xmodem::Stats,
    /// Duration of the whole sequence.
    pub elapsed: Duration,
}

/// Install `image` through the Gecko bootloader and boot it. The host
/// UART is left at the bootloader rate; the caller restores its own line
/// (the ZRHP driver sets its line when it starts).
pub fn install(
    rx: &mut dyn PortRx,
    tx: &mut dyn PortTx,
    control: &mut dyn NcpControl,
    image: &mut dyn ImageSource,
    cfg: &Config,
    progress: &mut dyn FnMut(u32, u32),
) -> Result<Report, GeckoError> {
    let start = Instant::now();
    let info = check_image(image, cfg)?;
    tx.set_line(Line { baud: cfg.baud, flow: false })?;
    control.enter_rom_loader()?;
    rx.clear_input()?;
    let mut g = Gecko::new(rx, tx, cfg);
    let menu = g.probe()?;
    let stats = g.upload(image, info.size, progress)?;
    g.run()?;
    Ok(Report { menu, info, stats, elapsed: start.elapsed() })
}

#[cfg(test)]
mod tests;
