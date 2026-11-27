//! The Silicon Labs Gecko bootloader's UART XMODEM interface, as the hub
//! drives it to replace an EFR32 NCP's application (the SONOFF Dongle-M's
//! EFR32MG24): image validation ([`gbl`]), the XMODEM-CRC transfer
//! ([`xmodem`]) and the bootloader's menu around it (this module). Portable
//! and `no_std`: the firmware supplies the UART as a [`Port`]; the tests
//! drive a simulated bootloader.
//!
//! The menu (GSDK `btl_comm_xmodem_common.c`, UG489), at 115 200 8N1
//! whatever rate the application used. On entry the bootloader prints it by
//! itself; any byte other than `1` or `2` prints it again; every string
//! ends with a NUL:
//!
//! ```text
//! \r\nGecko Bootloader v2.04.02\r\n1. upload gbl\r\n2. run\r\n3. ebl info\r\nBL > \0
//! ```
//!
//! `1` answers `\r\nbegin upload\r\n` and starts an XMODEM-CRC receive;
//! afterwards it prints `\r\nSerial upload complete\r\n`, or
//! `\r\nSerial upload aborted\r\n` followed by `\r\nblock error 0x` or
//! `\r\nfile error 0x` and two hex digits ([`upload_error`]), then the menu.
//! `2` boots the uploaded image (`\r\nFailed to boot\r\n` if it cannot).

pub mod gbl;
pub mod xmodem;

/// Rate of the bootloader's UART.
pub const BAUD: u32 = 115_200;
/// End of the menu.
pub const PROMPT: &[u8] = b"BL >";

/// A UART failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortError(pub &'static str);

/// The NCP's UART, at the bootloader's rate.
pub trait Port {
    /// Writes every byte.
    fn write(&mut self, data: &[u8]) -> Result<(), PortError>;
    /// One byte, waiting up to `timeout_ms` (`None`: none came).
    fn read_byte(&mut self, timeout_ms: u32) -> Result<Option<u8>, PortError>;
    /// Drops what was received and not read yet.
    fn discard_input(&mut self);
    /// A millisecond clock (wrapping).
    fn now_ms(&self) -> u32;
}

/// What the bootloader printed (the first [`Text::CAP`] bytes, NULs left
/// out), for matching and for the operator.
#[derive(Clone)]
pub struct Text {
    buf: [u8; Text::CAP],
    len: usize,
    /// Bytes received in all (NULs included).
    pub received: usize,
}

impl Default for Text {
    fn default() -> Self {
        Text {
            buf: [0; Text::CAP],
            len: 0,
            received: 0,
        }
    }
}

impl core::fmt::Debug for Text {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

impl Text {
    /// Bytes kept.
    pub const CAP: usize = 384;

    fn push(&mut self, b: u8) {
        self.received += 1;
        if b != 0 && self.len < Self::CAP {
            self.buf[self.len] = b;
            self.len += 1;
        }
    }

    /// The bytes kept.
    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// As text (up to the first non-ASCII byte).
    pub fn as_str(&self) -> &str {
        let b = self.bytes();
        let end = b.iter().position(|c| !c.is_ascii()).unwrap_or(b.len());
        core::str::from_utf8(&b[..end]).unwrap_or("")
    }

    /// Whether `needle` was printed at or after byte `from` (of the kept).
    pub fn has_from(&self, from: usize, needle: &[u8]) -> bool {
        self.bytes()
            .get(from..)
            .is_some_and(|h| find(h, needle).is_some())
    }

    /// Whether `needle` was printed.
    pub fn has(&self, needle: &[u8]) -> bool {
        self.has_from(0, needle)
    }

    /// Bytes kept so far (a mark for [`has_from`](Self::has_from)).
    pub fn mark(&self) -> usize {
        self.len
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    h.windows(n.len()).position(|w| w == n)
}

/// Reads into `text` until `until` appears after `from` (then until the
/// line is quiet for `settle_ms`, to take the prompt's trailing space and
/// NUL), until nothing came for `quiet_ms`, or for `max_ms` in all.
/// Whether `until` was seen.
fn collect<P: Port + ?Sized>(
    p: &mut P,
    text: &mut Text,
    from: usize,
    until: Option<&[u8]>,
    quiet_ms: u32,
    max_ms: u32,
) -> Result<bool, PortError> {
    let t0 = p.now_ms();
    let mut seen = false;
    loop {
        let spent = p.now_ms().wrapping_sub(t0);
        if spent >= max_ms {
            return Ok(seen);
        }
        let wait = if seen { 50 } else { quiet_ms }.min(max_ms - spent);
        match p.read_byte(wait)? {
            Some(b) => {
                text.push(b);
                if !seen && until.is_some_and(|u| text.has_from(from, u)) {
                    seen = true;
                }
            }
            None => return Ok(seen),
        }
    }
}

/// The bootloader's menu, as found.
#[derive(Debug, Clone, Default)]
pub struct Menu {
    /// `Gecko Bootloader vX.YY.ZZ`: `(X, YY, ZZ)`.
    pub version: Option<(u8, u8, u8)>,
    /// What it printed.
    pub text: Text,
    /// It was in an XMODEM receive (sending `C`) and was cancelled first.
    pub was_receiving: bool,
}

fn parse_version(t: &[u8]) -> Option<(u8, u8, u8)> {
    let at = find(t, b"Bootloader v")? + 12;
    let s = core::str::from_utf8(t.get(at..)?).ok()?;
    let mut it = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|x| !x.is_empty());
    Some((
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    ))
}

/// Why the bootloader session failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// No menu: the NCP is not in a Gecko bootloader (wrong entry line,
    /// another rate, or no Gecko bootloader).
    NoMenu,
    /// The transfer failed; `code` is the bootloader's error code when it
    /// printed one ([`upload_error`]).
    Upload {
        /// The XMODEM failure.
        xmodem: Option<xmodem::XmodemError>,
        /// `block error 0x..` / `file error 0x..`.
        code: Option<u8>,
    },
    /// The transfer ended but the bootloader printed no result.
    NoResult,
    /// The bootloader could not boot the image.
    BootFailed,
    /// The port failed.
    Port(PortError),
}

impl From<PortError> for SessionError {
    fn from(e: PortError) -> Self {
        SessionError::Port(e)
    }
}

impl core::fmt::Display for SessionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SessionError::NoMenu => write!(f, "no Gecko bootloader menu"),
            SessionError::Upload { xmodem, code } => {
                write!(f, "upload failed")?;
                if let Some(x) = xmodem {
                    write!(f, ": {x}")?;
                }
                if let Some(c) = code {
                    write!(f, "; bootloader error 0x{c:02X}: {}", upload_error(*c))?;
                }
                Ok(())
            }
            SessionError::NoResult => write!(f, "the bootloader printed no upload result"),
            SessionError::BootFailed => write!(f, "the bootloader failed to boot the image"),
            SessionError::Port(e) => write!(f, "UART: {}", e.0),
        }
    }
}

/// The bootloader's upload error codes (`btl_comm_xmodem_common.c`).
pub fn upload_error(code: u8) -> &'static str {
    match code {
        0x21 => "XMODEM: no SOH",
        0x22 => "XMODEM: bad packet number",
        0x23 | 0x24 => "XMODEM: bad CRC",
        0x25 => "XMODEM: packet out of sequence",
        0x27 => "XMODEM: duplicate packet",
        0x41 => "GBL: unsupported header or version",
        0x43 => "GBL: CRC mismatch",
        0x44 => "GBL: unknown tag",
        0x45 => "GBL: signature check failed",
        0x4F => "GBL: unexpected tag, file type or end of file",
        0x50 => "GBL: key error",
        _ => "unknown",
    }
}

/// Right after the reset into the bootloader: takes what it prints by
/// itself (banner, menu, NUL), and asks for the menu with a newline if no
/// prompt came. A bootloader still in an XMODEM receive (sending `C`, from
/// an interrupted upload) is cancelled first. `Err(NoMenu)` when no prompt
/// comes.
pub fn await_menu<P: Port + ?Sized>(p: &mut P) -> Result<Menu, SessionError> {
    let mut m = Menu::default();
    if collect(p, &mut m.text, 0, Some(PROMPT), 400, 1500)? {
        m.version = parse_version(m.text.bytes());
        return Ok(m);
    }
    for _ in 0..3 {
        // `C`: an XMODEM receive is waiting (an interrupted upload); CAN
        // ends it (and only prints the menu at the menu).
        if m.text.bytes().contains(&xmodem::CRC_REQUEST) {
            m.was_receiving = true;
            xmodem::cancel(p);
        } else {
            p.write(b"\r\n")?;
        }
        // Only a prompt printed after the newline counts.
        let from = m.text.mark();
        if collect(p, &mut m.text, from, Some(PROMPT), 1200, 3000)? {
            m.version = parse_version(m.text.bytes());
            return Ok(m);
        }
    }
    Err(SessionError::NoMenu)
}

/// The NCP sits in the bootloader (a boot probe; the NCP may run an
/// application instead, which ignores a newline): its menu, or `None`.
pub fn probe<P: Port + ?Sized>(p: &mut P) -> Result<Option<Menu>, PortError> {
    p.discard_input();
    let mut m = Menu::default();
    for _ in 0..2 {
        p.write(b"\r\n")?;
        let from = m.text.mark();
        if collect(p, &mut m.text, from, Some(PROMPT), 600, 1500)? {
            m.version = parse_version(m.text.bytes());
            return Ok(Some(m));
        }
        if m.text.bytes().contains(&xmodem::CRC_REQUEST) {
            // An upload left waiting: cancel it, the menu follows.
            m.was_receiving = true;
            xmodem::cancel(p);
            let from = m.text.mark();
            if collect(p, &mut m.text, from, Some(PROMPT), 1200, 3000)? {
                m.version = parse_version(m.text.bytes());
                return Ok(Some(m));
            }
        }
    }
    Ok(None)
}

/// A completed upload.
#[derive(Debug, Clone)]
pub struct Upload {
    /// The transfer.
    pub xmodem: xmodem::Report,
    /// What the bootloader printed afterwards.
    pub text: Text,
}

fn error_code(t: &Text) -> Option<u8> {
    let b = t.bytes();
    let at = find(b, b"error 0x")? + 8;
    let hex = core::str::from_utf8(b.get(at..at + 2)?).ok()?;
    u8::from_str_radix(hex, 16).ok()
}

/// At the menu ([`await_menu`]): `1`, the image over XMODEM (`len` bytes
/// from `source`), then the bootloader's verdict. On a failure the
/// bootloader returns to its menu: nothing of the application is
/// guaranteed then, the bootloader itself is untouched.
pub fn upload<P: Port + ?Sized>(
    p: &mut P,
    len: u32,
    source: &mut dyn FnMut(u32, &mut [u8]) -> bool,
    progress: &mut dyn FnMut(u32, u32),
    cfg: &xmodem::Config,
) -> Result<Upload, SessionError> {
    p.discard_input();
    p.write(b"1")?;
    let sent = xmodem::send(p, len, source, progress, cfg);
    let mut text = Text::default();
    collect(p, &mut text, 0, Some(PROMPT), 1500, 5000)?;
    let complete = text.has(b"Serial upload complete");
    match sent {
        Ok(report) if complete => Ok(Upload {
            xmodem: report,
            text,
        }),
        Ok(_) if text.has(b"aborted") => Err(SessionError::Upload {
            xmodem: None,
            code: error_code(&text),
        }),
        Ok(_) => Err(SessionError::NoResult),
        Err(xmodem::XmodemError::Port(e)) => Err(SessionError::Port(e)),
        Err(e) => Err(SessionError::Upload {
            xmodem: Some(e),
            code: error_code(&text),
        }),
    }
}

/// At the menu: `2`, boot the application.
pub fn run<P: Port + ?Sized>(p: &mut P) -> Result<(), SessionError> {
    p.discard_input();
    p.write(b"2")?;
    let mut text = Text::default();
    collect(p, &mut text, 0, Some(b"Failed to boot"), 300, 800)?;
    if text.has(b"Failed to boot") {
        Err(SessionError::BootFailed)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
