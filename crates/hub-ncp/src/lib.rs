//! Portable ZRHP host core of `zigbee-rs-hub` (phase 1, `std`).
//!
//! The same code runs on the ESP32-S3 of the Thread BR board (esp-idf-svc,
//! UART1 to the H2 at 921 600 with RTS/CTS) and on a desktop through the
//! phase-0 USB bridge (`hubctl`), so the NCP lifecycle can be developed and
//! tested on a Mac against the real NCP.
//!
//! | Module | Contents | Spec |
//! |---|---|---|
//! | [`transport`] | byte-stream halves, line settings, EN/BOOT control, in-memory pipe | 4, 11.5 |
//! | [`driver`] | link thread: `zigbee-zrhp` [`Link`](zigbee_zrhp::link::Link) (embedded profile), credits, sessions, negotiation, request gate, `RETRY` after a resync, line hunting | 5, 6 |
//! | [`lifecycle`] | [`Hub`]: seamless resume, network ownership, `NWK_INIT` floors, replay, events → store, form, permit join, APS data, NCP update/confirm, reset, ROM-loader hand-off | 7, 8.4, 8.7, 12.1, 12.4 |
//! | [`fw`] | `FW_*` streaming into the NCP's inactive slot | 7.9 |
//! | [`gecko`] | EFR32 NCP update through the Gecko bootloader: GBL checker, XMODEM-CRC sender, radio image banks (last-known-good + staged) in `hubdata`, update/rollback job, simulated bootloader | — |
//! | [`store`] | host store: log-structured journal on raw flash (`hubdata`), file or RAM | 3.4, 8.8 |
//! | `sim` (feature `sim`) | simulated NCP for tests and `hubctl --sim` | — |

pub mod driver;
pub mod fw;
pub mod gecko;
pub mod lifecycle;
#[cfg(any(test, feature = "sim"))]
pub mod sim;
pub mod store;
pub mod transport;

pub use driver::{CallOpts, Client, DriverConfig, DriverEvent, Error, Message, NcpSession, SessionState};
pub use lifecycle::{FormRequest, FwState, Hub, HubConfig, HubStatus, Ownership, ResumePolicy, Rng};
pub use zigbee_zrhp as zrhp;

#[cfg(test)]
mod tests;
