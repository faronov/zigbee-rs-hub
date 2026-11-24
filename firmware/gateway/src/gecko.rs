//! Gecko bootloader banner (`mg24 bootloader`): enter the NCP's Silicon
//! Labs Gecko bootloader with the board's lines, ask for its menu, print
//! it, reset the NCP back into its application. Nothing is uploaded: the
//! only byte sequence sent is a newline (UG266/UG489 UART XMODEM
//! bootloader: a newline prints the menu `1. upload gbl`, `2. run`,
//! `3. ebl info`, `BL >`).
//!
//! Installing NCP firmware through the bootloader is not part of this
//! firmware yet (docs/dongle-max.md, follow-ups).

use std::time::{Duration, Instant};

use hub_core::profile::{BoardProfile, NcpLoader};

use crate::{lines, uart};

const MAX_TEXT: usize = 1024;

fn collect(out: &mut Vec<u8>, window: Duration, until: Option<&[u8]>) {
    let end = Instant::now() + window;
    let mut buf = [0u8; 128];
    while Instant::now() < end && out.len() < MAX_TEXT {
        let n = uart::read(&mut buf, 50).unwrap_or(0);
        out.extend_from_slice(&buf[..n]);
        if until.is_some_and(|u| out.windows(u.len()).any(|w| w == u)) {
            break;
        }
    }
    out.truncate(MAX_TEXT);
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
    // What it prints by itself, then the menu after a newline.
    collect(&mut raw, Duration::from_millis(300), None);
    let unprompted = raw.len();
    let sent = uart::write_all(b"\r\n");
    collect(&mut raw, Duration::from_millis(1500), Some(b"BL >"));
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
