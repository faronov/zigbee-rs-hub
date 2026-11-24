//! Backend-neutral errors (design §2.3). API error codes stay per context
//! (engine, admin) and are mapped from these by the application.

use std::borrow::Cow;
use std::fmt;

/// The coordinator protocol a backend speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// zigbee-rs host protocol (ZRHP).
    Zrhp,
    /// Silicon Labs EmberZNet Serial Protocol over ASH.
    Ezsp,
    /// TI Z-Stack ZNP (MT commands).
    Znp,
}

impl BackendKind {
    /// Name stored in the hub's KV ownership tag (`hub/owner.backend`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zrhp => "zrhp",
            Self::Ezsp => "ezsp",
            Self::Znp => "znp",
        }
    }
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Zrhp => "ZRHP",
            Self::Ezsp => "EZSP",
            Self::Znp => "ZNP",
        })
    }
}

/// A backend status code, kept verbatim for diagnostics and messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Code {
    /// Protocol the value belongs to.
    pub backend: BackendKind,
    /// The status value.
    pub value: u16,
    /// Human-readable meaning, when the backend knows it (for ZRHP the
    /// `zb_status_text` of the NCP's numbering).
    pub text: Option<&'static str>,
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.text {
            Some(t) => f.write_str(t),
            None => write!(f, "{} status 0x{:02X}", self.backend, self.value),
        }
    }
}

/// What a backend failure means, so that callers can choose an API code
/// without knowing the backend's numbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// No route to the destination.
    NoRoute,
    /// A table of the NCP is full.
    TableFull,
    /// The payload or image is too large.
    TooLarge,
    /// Security failure (missing key, frame counter).
    Security,
    /// A request parameter is invalid.
    InvalidParameter,
    /// Not possible in the current state.
    InvalidState,
    /// Refused by policy.
    Denied,
    /// The addressed entry does not exist.
    NotFound,
    /// Anything else.
    Other,
}

/// A request or administration failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordError {
    /// The backend or its NCP does not implement this.
    Unsupported,
    /// Queue or in-flight capacity exhausted; retry later.
    Busy,
    /// No usable session with the NCP.
    LinkDown,
    /// The destination did not acknowledge.
    NoAck(Code),
    /// No answer in time.
    Timeout,
    /// The request may or may not have been delivered (reset, local error).
    DeliveryUnknown,
    /// Any other backend status.
    Backend {
        /// The backend's status.
        code: Code,
        /// Its meaning.
        kind: Kind,
    },
    /// Any other failure, in the backend's words (a host-side check, a
    /// response it cannot decode).
    Other(Cow<'static, str>),
    /// A failure the backend words itself: [`Display`](fmt::Display) shows
    /// its text, callers classify it by [`CoordError::meaning`].
    Described(Box<Described>),
}

/// The content of [`CoordError::Described`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Described {
    /// What the failure means (never `Described`).
    pub error: CoordError,
    /// The backend's message.
    pub text: Cow<'static, str>,
}

impl CoordError {
    /// `error`, shown as `text`.
    pub fn described(error: CoordError, text: impl Into<Cow<'static, str>>) -> Self {
        let error = match error {
            Self::Described(d) => d.error,
            e => e,
        };
        Self::Described(Box::new(Described {
            error,
            text: text.into(),
        }))
    }

    /// The error to classify: [`Described`] errors without their wording.
    pub fn meaning(&self) -> &CoordError {
        match self {
            Self::Described(d) => d.error.meaning(),
            e => e,
        }
    }
}

impl fmt::Display for CoordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("not supported by the coordinator"),
            Self::Busy => f.write_str("busy"),
            Self::LinkDown => f.write_str("NCP link down"),
            Self::NoAck(c) if c.text.is_some() => write!(f, "{c}"),
            Self::NoAck(c) => write!(f, "no acknowledgement ({c})"),
            Self::Timeout => f.write_str("timeout"),
            Self::DeliveryUnknown => f.write_str("delivery unknown"),
            Self::Backend { code, .. } => write!(f, "{code}"),
            Self::Other(t) => f.write_str(t),
            Self::Described(d) => f.write_str(&d.text),
        }
    }
}

impl std::error::Error for CoordError {}
