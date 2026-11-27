//! The NCP UART as the ZRHP host core's port (`hub_ncp::transport`), for
//! the zigbee-rs NCP on the MG24 (460 800 Bd product rate, 115 200 as the
//! host's fallback, no flow control: zigbee-rs `docs/book/src/advanced/
//! host-protocol.md` 4.4 and 7.9), and the MG24's reset line as its
//! `NcpControl`.
//!
//! The backend's ports ([`Gate::Backend`]) are parked while the NCP
//! updater has the UART (`uart::lease`): nothing goes out, nothing comes
//! in, line changes wait for the UART to be given back, and a reset the
//! host core asks for is not made. The probes' ports ([`Gate::Probe`])
//! are used by whoever has the UART.

use std::io;
use std::time::Duration;

use hub_ncp::transport::{Line, NcpControl, PortRx, PortTx};

use crate::{lines, uart};

/// The MG24 ZRHP NCP's product rate, then the host's fallback.
pub const LINES: [Line; 2] = [Line { baud: 460_800, flow: false }, Line { baud: 115_200, flow: false }];

/// Whose port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// The backend's: parked while the NCP updater has the UART.
    Backend,
    /// A probe's (its caller has the UART).
    Probe,
}

impl Gate {
    fn parked(self) -> bool {
        self == Gate::Backend && uart::parked()
    }
}

/// Receive half.
pub struct Rx(pub Gate);

impl PortRx for Rx {
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        if self.0.parked() {
            std::thread::sleep(timeout.min(Duration::from_millis(100)));
            return Ok(0);
        }
        uart::read(buf, timeout.as_millis().min(1000) as u32)
    }

    fn clear_input(&mut self) -> io::Result<()> {
        if !self.0.parked() {
            uart::flush_input();
        }
        Ok(())
    }
}

/// Transmit half.
pub struct Tx(pub Gate);

impl PortTx for Tx {
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        if self.0.parked() {
            return Ok(());
        }
        uart::write_all(data)
    }

    /// No RTS/CTS on the Dongle-M: only the rate changes.
    fn set_line(&mut self, line: Line) -> io::Result<()> {
        match self.0 {
            Gate::Backend => uart::set_backend_baud(line.baud),
            Gate::Probe => uart::set_baud(line.baud),
        }
        Ok(())
    }
}

/// The MG24's reset line. The Gecko bootloader is the NCP updater's
/// (`ncp`), not the host core's.
pub struct Mg24Lines;

impl NcpControl for Mg24Lines {
    fn reset(&mut self) -> io::Result<()> {
        if !uart::parked() {
            lines::reset_to_app();
        }
        Ok(())
    }

    fn enter_rom_loader(&mut self) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "the MG24's Gecko bootloader is entered by the NCP updater (ncp/update/start)"))
    }
}
