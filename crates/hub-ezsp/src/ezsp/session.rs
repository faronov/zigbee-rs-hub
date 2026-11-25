//! Sans-IO EZSP session: sequence numbers, the legacy first `version`
//! frame, request/response correlation, callback classification and the
//! NCP status flags (UG100 successor §2, §3).
//!
//! EZSP is a two-message transaction protocol: the host sends a command
//! and the NCP answers with a response carrying the command's sequence
//! number (UG100 successor §2, §3 "EZSP Frame Format"); the SDK host keeps
//! one command outstanding (`ezsp.c` asserts it). This session does the
//! same. Callbacks carry "the sequence number of the last command seen"
//! and are recognised by their callback type, never by sequence.

use super::command::{Callback, Command, EncodeError, Response, id};
use super::frame::{self, CallbackType, Header, HeaderError};
use super::version::{EzspVersion, MIN_VERSION};
use super::wire::DecodeError;

/// Status bits of a response frame control (UG100 successor §2 "NCP Status").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags {
    /// The NCP ran out of memory since the previous response: messages or
    /// callbacks may have been lost.
    pub overflow: bool,
    /// The NCP truncated the response.
    pub truncated: bool,
    /// More callbacks are pending on the NCP.
    pub callback_pending: bool,
}

/// A decoded EZSP frame from the NCP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    /// The response to the outstanding command.
    Response {
        sequence: u8,
        response: Response,
        flags: Flags,
    },
    /// The outstanding command's response was truncated (not decoded).
    Truncated {
        sequence: u8,
        frame_id: u16,
        flags: Flags,
    },
    /// The outstanding command's response did not decode.
    BadResponse {
        sequence: u8,
        frame_id: u16,
        error: DecodeError,
        flags: Flags,
    },
    /// An asynchronous (or synchronous) callback.
    Callback {
        callback: Callback,
        flags: Flags,
        /// Bytes after the fields the callback is known to have (ignored,
        /// [`Callback::decode_with_trailing`]).
        trailing: usize,
    },
    /// A callback that did not decode; dropped.
    BadCallback {
        frame_id: u16,
        error: DecodeError,
        flags: Flags,
    },
    /// A response that matches no outstanding command (late after a
    /// timeout, or stray).
    Unexpected {
        sequence: u8,
        frame_id: u16,
        flags: Flags,
    },
}

impl Inbound {
    pub fn flags(&self) -> Flags {
        match self {
            Inbound::Response { flags, .. }
            | Inbound::Truncated { flags, .. }
            | Inbound::BadResponse { flags, .. }
            | Inbound::Callback { flags, .. }
            | Inbound::BadCallback { flags, .. }
            | Inbound::Unexpected { flags, .. } => *flags,
        }
    }
}

/// Session-level failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// Malformed header.
    Header(HeaderError),
    /// A command frame (direction bit clear) arrived from the NCP.
    WrongDirection,
    /// Reserved callback type `11`.
    ReservedCallbackType,
    /// A command is already outstanding.
    Busy,
    /// No version negotiated yet; only `version` may be sent.
    VersionNotSet,
    /// The command does not encode for the negotiated version.
    Encode(EncodeError),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Header(e) => e.fmt(f),
            SessionError::WrongDirection => write!(f, "command frame received from the NCP"),
            SessionError::ReservedCallbackType => write!(f, "reserved callback type"),
            SessionError::Busy => write!(f, "an EZSP command is already outstanding"),
            SessionError::VersionNotSet => write!(f, "EZSP version not negotiated"),
            SessionError::Encode(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for SessionError {}

#[derive(Debug, Clone, Copy)]
struct Outstanding {
    sequence: u8,
    frame_id: u16,
    raw: bool,
}

/// Per-connection EZSP state. Call [`reset`](Self::reset) after every
/// ASH reset: the NCP forgets the version (UG100 successor §2 "Stack
/// Configuration": `version` must be the first command after reset).
#[derive(Debug, Default)]
pub struct Session {
    version: Option<EzspVersion>,
    next_sequence: u8,
    outstanding: Option<Outstanding>,
    legacy_version_sent: bool,
}

impl Session {
    pub fn new() -> Session {
        Session::default()
    }

    /// Forget the version and any outstanding command.
    pub fn reset(&mut self) {
        self.version = None;
        self.outstanding = None;
        self.legacy_version_sent = false;
    }

    pub fn version(&self) -> Option<EzspVersion> {
        self.version
    }

    /// Records the negotiated version (after a [`VersionPolicy`] check).
    ///
    /// [`VersionPolicy`]: super::version::VersionPolicy
    pub fn set_version(&mut self, v: EzspVersion) {
        self.version = Some(v);
    }

    /// Sequence number of the outstanding command.
    pub fn outstanding(&self) -> Option<u8> {
        self.outstanding.map(|o| o.sequence)
    }

    /// Gives up on the outstanding command (timeout); its late response
    /// will be reported as [`Inbound::Unexpected`].
    pub fn abandon(&mut self) -> Option<u8> {
        self.outstanding.take().map(|o| o.sequence)
    }

    /// Encodes `cmd` as the next frame. The first `version` after a reset
    /// uses the legacy format (UG100 successor §3: "For practical purposes,
    /// the Version command should be executed using that legacy frame
    /// format"); every later frame uses the extended format.
    pub fn encode(&mut self, cmd: &Command) -> Result<(u8, Vec<u8>), SessionError> {
        if self.outstanding.is_some() {
            return Err(SessionError::Busy);
        }
        let sequence = self.next_sequence;
        let bytes = match (cmd, self.version) {
            (Command::Version { desired }, _) if !self.legacy_version_sent => {
                self.legacy_version_sent = true;
                frame::legacy_version_command(sequence, *desired)
            }
            (Command::Version { desired }, _) => frame::command(sequence, id::VERSION, &[*desired]),
            (_, None) => return Err(SessionError::VersionNotSet),
            (cmd, Some(v)) => frame::command(
                sequence,
                cmd.frame_id(),
                &cmd.encode_params(v).map_err(SessionError::Encode)?,
            ),
        };
        self.next_sequence = sequence.wrapping_add(1);
        self.outstanding = Some(Outstanding {
            sequence,
            frame_id: cmd.frame_id(),
            raw: matches!(cmd, Command::Raw { .. }),
        });
        Ok((sequence, bytes))
    }

    /// Classifies and decodes one EZSP frame from the NCP.
    pub fn on_frame(&mut self, bytes: &[u8]) -> Result<Inbound, SessionError> {
        let (h, params) = frame::parse(bytes).map_err(SessionError::Header)?;
        if !h.control.is_response() {
            return Err(SessionError::WrongDirection);
        }
        let flags = Flags {
            overflow: h.control.overflow(),
            truncated: h.control.truncated(),
            callback_pending: h.control.callback_pending(),
        };
        match h.control.callback_type() {
            CallbackType::Reserved => Err(SessionError::ReservedCallbackType),
            CallbackType::Async | CallbackType::Sync => Ok(self.callback(&h, params, flags)),
            CallbackType::None => Ok(self.response(&h, params, flags)),
        }
    }

    fn callback(&self, h: &Header, params: &[u8], flags: Flags) -> Inbound {
        let Some(v) = self.version else {
            return Inbound::Callback {
                callback: Callback::Unknown {
                    frame_id: h.frame_id,
                    params: params.to_vec(),
                },
                flags,
                trailing: 0,
            };
        };
        match Callback::decode_with_trailing(h.frame_id, v, params) {
            Ok((callback, trailing)) => Inbound::Callback {
                callback,
                flags,
                trailing,
            },
            Err(error) => Inbound::BadCallback {
                frame_id: h.frame_id,
                error,
                flags,
            },
        }
    }

    fn response(&mut self, h: &Header, params: &[u8], flags: Flags) -> Inbound {
        let (sequence, frame_id) = (h.sequence, h.frame_id);
        let matched = self.outstanding.filter(|o| {
            o.sequence == sequence && (o.frame_id == frame_id || frame_id == id::INVALID_COMMAND)
        });
        let Some(o) = matched else {
            return Inbound::Unexpected {
                sequence,
                frame_id,
                flags,
            };
        };
        self.outstanding = None;
        if flags.truncated {
            return Inbound::Truncated {
                sequence,
                frame_id,
                flags,
            };
        }
        if o.raw && frame_id != id::INVALID_COMMAND {
            return Inbound::Response {
                sequence,
                response: Response::Raw {
                    frame_id,
                    params: params.to_vec(),
                },
                flags,
            };
        }
        // The version response layout does not depend on the version.
        let v = self.version.unwrap_or(EzspVersion::new(MIN_VERSION));
        if frame_id != id::VERSION && frame_id != id::INVALID_COMMAND && self.version.is_none() {
            let error = DecodeError::Invalid("response before version negotiation");
            return Inbound::BadResponse {
                sequence,
                frame_id,
                error,
                flags,
            };
        }
        match Response::decode(frame_id, v, params) {
            Ok(response) => Inbound::Response {
                sequence,
                response,
                flags,
            },
            Err(error) => Inbound::BadResponse {
                sequence,
                frame_id,
                error,
                flags,
            },
        }
    }
}
