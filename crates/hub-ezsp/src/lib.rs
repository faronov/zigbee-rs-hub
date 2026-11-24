//! Host-side ASH + EZSP protocol core for Silicon Labs EmberZNet NCPs
//! (EFR32MG24 running stock EmberZNet NCP-UART firmware).
//!
//! **Status: host-tested only.** The crate is not wired into the hub and
//! has not run against a real NCP. Every behaviour below is implemented
//! from the public documents cited and exercised against the simulated
//! NCP in `sim`; nothing here is a hardware or interoperability claim.
//!
//! # Layering
//!
//! | Layer | Module | Shape |
//! |---|---|---|
//! | ASH framing | [`ash::codec`] | CRC-CCITT, control byte, randomization, byte stuffing, deframer |
//! | ASH link | [`ash::link`] ([`Ash`]) | sans-IO: `handle_rx(bytes, now)`, `handle_timeout(now)`, `poll_transmit(now, out)`, `poll_event()`, `next_timeout()` |
//! | EZSP frame | [`ezsp::frame`] | header, frame control, legacy/extended format |
//! | EZSP types | [`ezsp::types`], [`ezsp::command`] | typed core-stage commands, responses, callbacks; version-gated layouts |
//! | EZSP session | [`ezsp::session`] ([`Session`]) | sans-IO: sequence numbers, correlation, callback routing, NCP status flags |
//! | Host | [`host`] ([`EzspHost`]) | blocking, one thread, [`Transport`] + [`Clock`], bounded event queue |
//! | Simulator | `sim` (feature `sim`) | NCP model over a faulty in-memory UART on a virtual clock |
//!
//! The crate depends on nothing but `std`, and on no other hub crate.
//!
//! # Sources
//!
//! Silicon Labs moved the user guides to docs.silabs.com; the HTML pages
//! (version 9.1.2) state that they replace the PDFs:
//!
//! - **UG101** "UART-EZSP Gateway Protocol Reference" (ASH): §2 Frames,
//!   §3 Frame Formats, §4 Protocol Operations, §5 CONNECTED State, §6
//!   FAILED State, §7 Configuration Parameters.
//! - **UG100 successor**, "Simplicity SDK EZSP Reference Guide" (EZSP for
//!   EmberZNet ≥ 8.0; based on UG100): §2 EmberZNet Serial Protocol, §3
//!   Protocol Format, §4 Configuration, §5 Utilities, §6 Networking, §8
//!   Messaging, §10 Trust Center frames.
//! - **AN706 successor**, "EZSP-UART Host Interfacing Guide": §2 physical
//!   interfaces (flow control, nRESET), §5 power-on and reset, §6
//!   bootloading.
//! - Numeric facts only (frame IDs, enum values, field order and widths
//!   per version) from the public SDK sources on GitHub: Gecko SDK 4.4.6
//!   (EZSP 13) and Simplicity SDK 2024.6.0 (14), 2024.12.0 (16), 2025.6.0
//!   (17), 2025.6.3 (18): `ezsp-protocol.h`, `ezsp-enum.h`,
//!   `command-functions.h`, `ezsp-frame-utilities.c`, `ash-protocol.h`,
//!   `ember-types.h`, `error-def.h`, `sl_zigbee_types.h`, `sl_status.h`.
//!   No code was taken from GPL hosts.
//!
//! # Protocol rules implemented
//!
//! ASH (UG101):
//! - Frame = control, data, big-endian CRC-CCITT (init 0xFFFF) over both,
//!   stuffing, Flag 0x7E (§2, §4). Reserved bytes 0x7E 0x7D 0x11 0x13
//!   0x18 0x1A are escaped as 0x7D, byte ^ 0x20 (§4).
//! - DATA fields are XORed with the LFSR sequence 0x42, 0x21, 0xA8, …
//!   (§4 "Data Randomization").
//! - Receive: Cancel discards the partial frame, Substitute discards up
//!   to the next flag and sets the Reject Condition, XON/XOFF are
//!   ignored, 0xFF between frames is ignored (§4).
//! - A frame is valid with a correct CRC, a host-receivable control byte
//!   (DATA, ACK, NAK, RSTACK, ERROR), a valid length and, when connected,
//!   a valid ackNum (between the last received ackNum and the last sent
//!   frmNum + 1); otherwise it is discarded and, when connected and the
//!   Reject Condition was clear, a NAK is sent (§4, §5).
//! - Reset: Cancel + RST, every frame before a valid RSTACK is discarded,
//!   RSTACK version must be 2, up to five retries (§3, §5). Input pending
//!   before the reset is discarded, and only an RSTACK with the software
//!   reset code (0x0B) answers the RST; an unprompted RSTACK after nRESET
//!   or power-on is stale. UG101 does not state these two rules; the SDK
//!   reference host `ash-host.c` applies both for its RST reset method.
//! - CONNECTED: 3-bit frame numbers, sliding window < 8, ackNum always
//!   processed, immediate ACK (the host never piggybacks), duplicates
//!   (reTx set, already received) ACKed without Reject, out-of-sequence
//!   and no-space frames set the Reject Condition (§5).
//! - Retransmission from the oldest unacknowledged frame with reTx set, on
//!   NAK or ACK timeout; the adaptive timeout t = 7/8 t + 1/2 measured,
//!   doubled on timeout, clamped to [0.4 s, 3.2 s], initially 1.6 s (§5,
//!   §7). More than ACK_TIMEOUTS = 4 consecutive timeouts fail the link.
//! - nRdy flow control: ACK/NAK carry nRdy while the host is short of
//!   buffers and repeat it periodically (§5).
//! - ERROR frame → [`FailReason::NcpError`]; RSTACK while connected →
//!   [`FailReason::NcpReset`] (§5, §6).
//!
//! EZSP (UG100 successor):
//! - Header `sequence, frame control (2), frame ID (2)` from EZSP 8; the
//!   first `version` after reset uses the legacy `sequence, frame control,
//!   frame ID (0x00)` format and the response may come in either format
//!   (§3). The host requests [`MAX_VERIFIED_VERSION`]; if the NCP answers
//!   with another version that the [`VersionPolicy`] accepts, a second,
//!   extended `version` with that number confirms it (§4 version: "must be
//!   same as EZSP_PROTOCOL_VERSION").
//! - Frame control: direction, network index, overflow, truncated,
//!   callback pending, callback type, frame-format version 1; security
//!   and padding are refused (§3).
//! - UART callbacks arrive asynchronously by default (callback type
//!   "asynchronous"); no `callback` polling is needed (§2 "UART Interface
//!   Callbacks"). Callbacks carry the last seen sequence number and are
//!   never correlated by it (§3).
//! - `invalidCommand` (0x0058) carries an `EzspStatus` reason (§5).
//! - All fields little endian, EUI64 included (§2 "Byte Order").
//!
//! # Version policy
//!
//! EZSP < 13 is refused with [`VersionError::TooOld`]. Verified: 13
//! (EmberZNet 7.4–7.5), 14 (8.0.0–8.0.2), 16 (8.0.3, 8.1), 17 (8.2.0–8.2.1),
//! 18 (8.2.2–8.2.3); see [`ezsp::version`]. 15 (no public SDK tag) and
//! versions above 18 are refused with [`VersionError::Unverified`] unless
//! [`VersionPolicy::accept_unverified`] is set, in which case they use
//! the ≥ 14 layout. Layout switches (all verified in the SDK sources):
//!
//! | Item | EZSP 13 | EZSP 14–18 |
//! |---|---|---|
//! | Status | `EzspStatus`/`EmberStatus` u8 | `sl_status_t` u32 |
//! | Message tag | u8 | u16 |
//! | sendBroadcast | dest, aps, radius, tag, msg | alias, dest, nwkSeq, aps, radius, tag, msg |
//! | sendMulticast | aps, hops, nonmemberRadius, tag, msg | aps, hops, broadcastAddr, alias, nwkSeq, tag, msg |
//! | messageSentHandler | type, dest, aps, tag, status, msg | status, type, dest, aps, tag, msg |
//! | incomingMessageHandler | type, aps, lqi, rssi, sender, binding, address, msg | type, aps, `rx_packet_info_t` (18 B), msg |
//! | 0x0024 | trustCenterJoinHandler | trustCenterPostJoinHandler (same layout) |
//!
//! Frame IDs, `EzspStatus` values, config/value IDs and the APS frame
//! (11 bytes), network parameters (20 bytes) and multicast entry (3
//! bytes) layouts are the same in 13–18.
//!
//! # Bounds
//!
//! | Buffer | Bound |
//! |---|---|
//! | Deframer | [`ash::codec::MAX_FRAME`] = 223 bytes |
//! | DATA field | 3..=220 bytes ([`ash::codec::MAX_DATA_FIELD`]; UG101 says 128, the EZSP ≥ 13 SDKs use 220) |
//! | Unacknowledged frames | [`AshConfig::tx_window`], 1..=7 (default 3) |
//! | Frames waiting for the window | [`AshConfig::tx_queue_frames`] (default 4) |
//! | Delivered, unread DATA | [`AshConfig::rx_queue_frames`] (default 8; nRdy from 4) |
//! | ASH control events | 8, oldest dropped |
//! | One `poll_transmit` | one frame, ≤ [`ash::codec::MAX_ENCODED_FRAME`] bytes |
//! | EZSP commands in flight | 1 |
//! | Host event queue | [`HostConfig::event_queue`] (default 64; nRdy from 48; overflow counted, [`HostEvent::Dropped`]) |
//!
//! # Verified vs. unverified
//!
//! Verified against the documents: everything in "Protocol rules"; the
//! UG101 §3 example frames RST, RSTACK, ACK(1)+, ACK(6)−, NAK(6)+,
//! NAK(5)−, DATA(2,5,0) plain and randomized, DATA(5,3,0) plain (unit
//! tests use them verbatim). The two plain DATA listings are printed
//! without byte stuffing (they contain 0x1A and 0x11); tests check the
//! unstuffed bytes and the stuffed wire form. Two UG101 §3 examples are
//! inconsistent with the CRC that all other examples confirm: `ERROR C2 01
//! 52 FA BD` and the randomized `DATA(5,3,0) … 96 23` (its randomized
//! bytes match); tests pin that they fail the CRC.
//!
//! Document inconsistencies resolved in favour of the SDK headers:
//! UG101's 128-byte DATA limit (220 used), UG100 successor's
//! `sl_zigbee_aps_frame_t.radius` (not serialized), `LEAVING_NETWORK = 4`
//! and `OUTGOING_BROADCAST = 4` (headers: 6 and 6).
//!
//! `UNVERIFIED` (marked in the code):
//! - T_RSTACK_MAX and T_LOCAL_NOTRDY have no value in UG101 (3.2 s and
//!   0.5 s chosen); the host TX window (3) is not documented; ACK_TIMEOUTS
//!   is defined for the NCP and applied to the host.
//! - Measuring the ACK time only on frames sent once (UG101 does not say).
//! - Deframer: Cancel also clears a pending Substitute; the host drops
//!   XON/XOFF bytes (UG101 only says the NCP ignores them).
//! - An RSTACK while connected is treated as an NCP reboot.
//! - Version negotiation by re-sending `version` with the NCP's number,
//!   and the format of the NCP's answer to the legacy command (both
//!   formats are accepted).
//! - The meaning of sendMulticast `broadcastAddr` in EZSP ≥ 14 (0xFFFD used).
//! - In EZSP ≥ 14 an EZSP-level error is reported as
//!   `SL_STATUS_ZIGBEE_EZSP_ERROR` without the detail code.
//! - Simulator behaviour without a document: the invalidCommand reason for
//!   malformed parameters, `getNetworkParameters` without a network,
//!   stack status after `permitJoining`.
//! - Callback ordering, timing and overflow behaviour of a real NCP.
//!
//! [`Ash`]: ash::Ash
//! [`Session`]: ezsp::Session
//! [`EzspHost`]: host::EzspHost
//! [`Transport`]: host::Transport
//! [`Clock`]: host::Clock
//! [`FailReason::NcpError`]: ash::FailReason::NcpError
//! [`FailReason::NcpReset`]: ash::FailReason::NcpReset
//! [`MAX_VERIFIED_VERSION`]: ezsp::version::MAX_VERIFIED_VERSION
//! [`VersionPolicy`]: ezsp::VersionPolicy
//! [`VersionPolicy::accept_unverified`]: ezsp::VersionPolicy::accept_unverified
//! [`VersionError::TooOld`]: ezsp::VersionError::TooOld
//! [`VersionError::Unverified`]: ezsp::VersionError::Unverified
//! [`AshConfig::tx_window`]: ash::AshConfig::tx_window
//! [`AshConfig::tx_queue_frames`]: ash::AshConfig::tx_queue_frames
//! [`AshConfig::rx_queue_frames`]: ash::AshConfig::rx_queue_frames
//! [`HostConfig::event_queue`]: host::HostConfig::event_queue
//! [`HostEvent::Dropped`]: host::HostEvent::Dropped

pub mod ash;
pub mod ezsp;
pub mod host;
#[cfg(any(test, feature = "sim"))]
pub mod sim;

pub use ash::{Ash, AshConfig, AshEvent, FailReason, LinkState, ResetCode};
pub use ezsp::{
    Callback, Command, EzspVersion, Response, Session, Status, StatusKind, VersionPolicy,
};
pub use host::{
    Clock, EventReceiver, EzspHost, HostConfig, HostError, HostEvent, NcpInfo, SystemClock,
    Transport,
};

#[cfg(test)]
mod tests;
