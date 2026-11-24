//! EZSP: the command/response protocol above ASH (UG100 and its
//! Simplicity SDK successor).
//!
//! - [`frame`]: header (sequence, frame control, 16-bit frame ID; legacy
//!   format of the first `version`).
//! - [`version`]: protocol versions, the layout switches they imply and
//!   the acceptance policy.
//! - [`types`]: EmberZNet structs, enumerations and the version-typed
//!   [`Status`].
//! - [`command`]: typed core-stage commands, responses and callbacks.
//! - [`session`]: sequence numbers and request/response correlation.

pub mod command;
pub mod frame;
pub mod session;
pub mod types;
pub mod version;
pub mod wire;

pub use command::{
    BroadcastExtra, Callback, Command, EncodeError, Endpoint, MulticastExtra, Response,
};
pub use session::{Flags, Inbound, Session, SessionError};
pub use types::*;
pub use version::{EzspVersion, StackVersion, VersionError, VersionPolicy};
pub use wire::DecodeError;

#[cfg(test)]
mod tests;
