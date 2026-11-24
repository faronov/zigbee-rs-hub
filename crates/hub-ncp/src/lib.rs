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
//! | [`lifecycle`] | [`Hub`]: seamless resume, network ownership, `NWK_INIT` floors, replay, events → store, form, permit join, APS data, NCP update/confirm, reset, ROM-loader hand-off; maintenance (`netops`): energy scan, channel change, TX power, diagnostics, coordinator tables, backup export/restore, leave/reset | 7, 8.4, 8.7, 9, 12.1, 12.4 |
//! | [`fw`] | `FW_*` streaming into the NCP's inactive slot | 7.9 |
//! | [`inventory`] | devices the NCP knows (`TBL_READ`, `DIAG_GET_CHILDREN`/`NEIGHBORS`) for import | 8.5, 10 |
//! | [`coord`] | the ZRHP coordinator backend (`hub-coord`): the non-blocking coordinator and its `zrhp-pump` thread ([`coord::ZrhpCoordinator`]), coexistence ([`coord::ZrhpRadio`]), network administration ([`coord::ZrhpAdmin`]), ZRHP ⇄ neutral events and requests, errors, backups as record streams | 7, 9 |
//! | [`store`] | re-export of `hub-store`: log-structured journal on raw flash (`hubdata`), file or RAM | 3.4, 8.8 |
//! | `sim` (feature `sim`) | simulated NCP for tests and `hubctl --sim` | — |

pub mod coord;
pub mod driver;
pub mod fw;
pub mod inventory;
pub mod lifecycle;
#[cfg(any(test, feature = "sim"))]
pub mod sim;
pub mod transport;

pub use driver::{CallOpts, Client, DriverConfig, DriverEvent, Error, Message, NcpSession, SessionState};
pub use lifecycle::{
    ChannelChange, EnergyChannel, FormRequest, FwState, Hub, HubConfig, HubStatus, Ownership, ResumePolicy, RestoreOptions,
    RestoreReport, Rng,
};
pub use hub_store as store;
pub use zigbee_zrhp as zrhp;

#[cfg(test)]
mod tests;
