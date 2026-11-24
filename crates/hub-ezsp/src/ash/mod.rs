//! ASH: the UART data-link layer below EZSP (UG101).
//!
//! [`codec`] is the frame format (CRC, control byte, randomization, byte
//! stuffing, deframing); [`link`] is the sans-IO state machine (reset,
//! frame numbers, window, ACK/NAK, retransmission, adaptive ACK timer,
//! nRdy flow control).

pub mod codec;
pub mod link;

pub use codec::{Control, Frame, FrameError};
pub use link::{
    Ash, AshConfig, AshEvent, AshStats, ConfigError, FailReason, LinkState, ResetCode, SendError,
};

#[cfg(test)]
mod tests;
