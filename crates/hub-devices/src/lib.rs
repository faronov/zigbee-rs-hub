//! Device and entity engine of zigbee-rs-hub (spec `docs/device-support.md`):
//! interview → fingerprint → resolution through the definition layers →
//! entities with stable uids, state updates, commands, configure,
//! availability, events, Tuya EF00, Green Power and GP bindings, and the
//! WebSocket API model. Host-testable: the engine is sans-IO behind
//! [`transport::ZigbeeTransport`].

#![warn(missing_docs)]
// Nested `if let` reads better than let-chains in the decoders (as in zdef);
// the handlers take the full request context.
#![allow(
    clippy::collapsible_if,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::if_same_then_else,
    clippy::new_ret_no_self,
    clippy::field_reassign_with_default
)]

pub mod api;
pub mod bankcat;
pub mod codec;
pub mod commands;
pub mod configure;
pub mod device;
pub mod expr;
pub mod fingerprint;
pub mod generic;
pub mod gp;
pub mod hub;
pub mod model;
pub mod persist;
pub mod resolve;
pub mod transport;
pub mod tuya;
mod tuya_rt;
pub mod values;
pub mod zcl;
pub mod zdef_adapter;
pub mod zdo;

#[cfg(test)]
mod scenarios;
#[cfg(test)]
pub mod testutil;

pub use hub::{ApiError, Hub, HubConfig, HubEvent};
