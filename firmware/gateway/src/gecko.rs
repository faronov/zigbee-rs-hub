//! Gecko bootloader banner (`mg24 bootloader`): enter the NCP's Silicon
//! Labs Gecko bootloader with the board's lines, print what it says by
//! itself on entry (its banner and menu, a NUL after the prompt) and its
//! menu again after a newline, reset the NCP back into its application.
//! The only byte sequence sent is a newline (UG266/UG489 UART XMODEM
//! bootloader: any key other than `1` or `2` prints the menu `1. upload
//! gbl`, `2. run`, `3. ebl info`, `BL >`).
//!
//! [`GeckoPort`]: the NCP UART as `hub_core::gecko::Port`, for the NCP
//! updater ([`ncp`](crate::ncp)).

use std::time::{Duration, Instant};

use hub_core::profile::{BoardProfile, NcpLoader};

use crate::{lines, uart};

const MAX_TEXT: usize = 1024;

/// Reads for `window`, or until `until` appears in what came after byte
/// `from` (then 100 ms more for the prompt's trailing space and NUL).
fn collect(out: &mut Vec<u8>, window: Duration, from: usize, until: Option<&[u8]>) {
    let end = Instant::now() + window;
    let mut buf = [0u8; 128];
    let mut settle: Option<Instant> = None;
    while Instant::now() < end && out.len() < MAX_TEXT && settle.is_none_or(|t| Instant::now() < t) {
        let n = uart::read(&mut buf, 50).unwrap_or(0);
        out.extend_from_slice(&buf[..n]);
        let fresh = out.get(from..).unwrap_or_default();
        if settle.is_none() && until.is_some_and(|u| fresh.windows(u.len()).any(|w| w == u)) {
            settle = Some(Instant::now() + Duration::from_millis(100));
        }
    }
    out.truncate(MAX_TEXT);
}

/// The NCP UART for the bootloader session (the caller has the UART:
/// claimed, or leased from the parked backend).
pub struct GeckoPort {
    t0: Instant,
}

impl Default for GeckoPort {
    fn default() -> Self {
        GeckoPort { t0: Instant::now() }
    }
}

impl hub_core::gecko::Port for GeckoPort {
    fn write(&mut self, data: &[u8]) -> Result<(), hub_core::gecko::PortError> {
        uart::write_all(data).map_err(|_| hub_core::gecko::PortError("uart_write_bytes failed"))
    }
    fn read_byte(&mut self, timeout_ms: u32) -> Result<Option<u8>, hub_core::gecko::PortError> {
        let mut b = [0u8; 1];
        match uart::read(&mut b, timeout_ms) {
            Ok(1) => Ok(Some(b[0])),
            Ok(_) => Ok(None),
            Err(_) => Err(hub_core::gecko::PortError("uart_read_bytes failed")),
        }
    }
    fn discard_input(&mut self) {
        uart::flush_input();
    }
    fn now_ms(&self) -> u32 {
        self.t0.elapsed().as_millis() as u32
    }
}

fn text(b: &[u8]) -> String {
    let mut s = String::new();
    for &c in b {
        match c {
            b'\n' => s += "\r\n",
            b'\r' => {}
            0x20..0x7F => s.push(c as char),
            _ => s += &format!("\\x{c:02X}"),
        }
    }
    s
}

/// Enter the bootloader, print its banner and menu, reset to the application.
pub fn banner(board: &BoardProfile) -> String {
    let NcpLoader::GeckoUart { baud, .. } = board.control.loader else {
        return "this board's NCP has no Gecko bootloader\r\n".into();
    };
    let _claim = match uart::claim(uart::User::Console) {
        Ok(c) => c,
        Err(u) => return format!("MG24 UART busy ({u:?})\r\n"),
    };
    let app_baud = uart::baud();
    uart::set_baud(baud);
    uart::flush_input();
    let mut raw = Vec::new();
    let entered = lines::enter_bootloader();
    // What it prints by itself (banner, menu, NUL), then the menu after a
    // newline: only a prompt after the newline ends the wait.
    collect(&mut raw, Duration::from_millis(600), 0, None);
    let unprompted = raw.len();
    let sent = uart::write_all(b"\r\n");
    collect(&mut raw, Duration::from_millis(1500), unprompted, Some(b"BL >"));
    lines::reset_to_app();
    uart::set_baud(app_baud);
    uart::flush_input();

    let mut out = match entered {
        Ok(()) => format!("Gecko bootloader at {baud} Bd ({} B unprompted, {} B after a newline):\r\n", unprompted, raw.len() - unprompted),
        Err(e) => return format!("bootloader entry: {e}\r\n"),
    };
    if let Err(e) = sent {
        out += &format!("newline not sent: {e}\r\n");
    }
    if raw.is_empty() {
        out += "(no answer: the bootloader may not use this entry pin, or the MG24 has no Gecko bootloader)\r\n";
    } else {
        out += &text(&raw);
        out += "\r\n";
    }
    out += "MG24 reset into its application\r\n";
    out
}
