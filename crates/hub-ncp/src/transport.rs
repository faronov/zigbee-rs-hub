//! Byte transport to the NCP and NCP control lines.
//!
//! The core never touches a serial API directly. A platform supplies:
//!
//! - a receive half ([`PortRx`]), owned by the reader thread;
//! - a transmit half ([`PortTx`]), owned by the link driver, which also
//!   switches the line rate and RTS/CTS;
//! - optionally [`NcpControl`] for the NCP's EN/BOOT lines (the S3 owns
//!   H2 EN/BOOT on the Thread BR board; on a desktop the bridge maps
//!   DTR/RTS to them).
//!
//! Implementations: `hubctl` (macOS/Linux serial port through the phase-0
//! bridge), `firmware/hub` (S3 UART1 with RTS/CTS), [`pipe`] (in-memory,
//! tests and the simulated NCP).

use std::io;
use std::time::Duration;

/// UART line settings the host applies on its side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line {
    /// Bit rate.
    pub baud: u32,
    /// RTS/CTS hardware flow control.
    pub flow: bool,
}

impl Line {
    /// Thread BR board default (spec 4.1, decision N10).
    pub const BR_DEFAULT: Line = Line { baud: 921_600, flow: true };
    /// Fallback for NCP images without flow control (spec 11.5).
    pub const BR_FALLBACK: Line = Line { baud: 460_800, flow: false };
}

/// Receive half of the byte stream.
pub trait PortRx: Send {
    /// Wait up to `timeout` for the first byte, then return what is
    /// buffered. `Ok(0)` on timeout.
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize>;

    /// Discard bytes buffered by the OS or driver (spec 5.5 step 1).
    fn clear_input(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Transmit half of the byte stream.
pub trait PortTx: Send {
    /// Write every byte (blocking; RTS/CTS may pause it).
    fn write_all(&mut self, data: &[u8]) -> io::Result<()>;

    /// Switch the host side of the line. A transport that cannot change a
    /// setting (a USB bridge that owns flow control) ignores it.
    fn set_line(&mut self, line: Line) -> io::Result<()>;
}

/// NCP reset and boot-strap lines.
pub trait NcpControl: Send {
    /// Normal reset: BOOT released, EN pulsed low (≥ 10 ms).
    fn reset(&mut self) -> io::Result<()>;

    /// Reset into the ESP ROM serial loader (spec 11.5, recovery path steps
    /// 1–3). Manual/development path only: the hub never does this on its
    /// own, and after it the byte stream belongs to the operator's flasher.
    fn enter_rom_loader(&mut self) -> io::Result<()>;
}

/// In-memory full-duplex pipe (tests and the simulated NCP).
pub mod pipe {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Instant;

    #[derive(Default)]
    struct Inner {
        data: VecDeque<u8>,
        closed: bool,
    }

    #[derive(Default)]
    struct Chan {
        inner: Mutex<Inner>,
        cv: Condvar,
    }

    impl Chan {
        fn push(&self, data: &[u8]) -> io::Result<()> {
            let mut g = self.inner.lock().unwrap();
            if g.closed {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            g.data.extend(data);
            self.cv.notify_all();
            Ok(())
        }

        fn pop(&self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
            let deadline = Instant::now() + timeout;
            let mut g = self.inner.lock().unwrap();
            loop {
                if !g.data.is_empty() {
                    let n = buf.len().min(g.data.len());
                    for (slot, byte) in buf.iter_mut().zip(g.data.drain(..n)) {
                        *slot = byte;
                    }
                    return Ok(n);
                }
                if g.closed {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                let now = Instant::now();
                if now >= deadline {
                    return Ok(0);
                }
                g = self.cv.wait_timeout(g, deadline - now).unwrap().0;
            }
        }

        fn close(&self) {
            self.inner.lock().unwrap().closed = true;
            self.cv.notify_all();
        }
    }

    /// One end of a pipe.
    pub struct End {
        rx: Arc<Chan>,
        tx: Arc<Chan>,
    }

    /// Receive half of an [`End`].
    pub struct Rx(Arc<Chan>);
    /// Transmit half of an [`End`].
    pub struct Tx(Arc<Chan>);

    /// A connected pair of ends.
    pub fn pair() -> (End, End) {
        let a = Arc::new(Chan::default());
        let b = Arc::new(Chan::default());
        (End { rx: a.clone(), tx: b.clone() }, End { rx: b, tx: a })
    }

    impl End {
        /// Split into halves.
        pub fn split(self) -> (Rx, Tx) {
            (Rx(self.rx), Tx(self.tx))
        }
    }

    impl Rx {
        /// Close both directions (the peer sees `BrokenPipe`).
        pub fn close(&self) {
            self.0.close();
        }
    }

    impl PortRx for Rx {
        fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
            self.0.pop(buf, timeout)
        }
    }

    impl Tx {
        /// Write bytes.
        pub fn send(&self, data: &[u8]) -> io::Result<()> {
            self.0.push(data)
        }
    }

    impl PortTx for Tx {
        fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
            self.0.push(data)
        }
        fn set_line(&mut self, _line: Line) -> io::Result<()> {
            Ok(())
        }
    }
}
