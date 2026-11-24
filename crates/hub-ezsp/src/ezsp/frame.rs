//! EZSP frame header: sequence, frame control, frame ID (UG100 successor
//! §3 "EZSP Frame Format", "EZSP Version Command – Legacy Frame Format",
//! "Frame Control Summary").
//!
//! Extended format (EZSP ≥ 8): `seq, fc_low, fc_high, id_low, id_high,
//! params`. Legacy format, used for the first `version` command and its
//! response: `seq, fc, id (0x00), params`. A received frame is extended
//! when the low two bits of byte 2 (the frame-format version in
//! `fc_high`) equal 1; the Gecko/Simplicity SDK host makes the same test
//! (`ezsp.c`, `responseReceived`).

/// Bit 7 of the low frame-control byte: response (NCP → host).
pub const FC_RESPONSE: u8 = 0x80;
const FC_NETWORK_INDEX_MASK: u8 = 0x60;
const FC_NETWORK_INDEX_SHIFT: u8 = 5;
const FC_OVERFLOW: u8 = 0x01;
const FC_TRUNCATED: u8 = 0x02;
const FC_CALLBACK_PENDING: u8 = 0x04;
const FC_CALLBACK_SYNC: u8 = 0x08;
const FC_CALLBACK_ASYNC: u8 = 0x10;
const FC_CALLBACK_TYPE_MASK: u8 = 0x18;
const FC_SLEEP_MASK: u8 = 0x03;
const FCH_SECURITY: u8 = 0x80;
const FCH_PADDING: u8 = 0x40;
const FCH_RESERVED: u8 = 0x3C;
const FCH_FORMAT_MASK: u8 = 0x03;
/// Frame format version carried in the high frame-control byte.
pub const FRAME_FORMAT_VERSION: u8 = 1;
/// Frame ID of `version` (0x0000; 0x00 in the legacy format).
pub const VERSION_FRAME_ID: u16 = 0x0000;

/// `callbackType` of a response (UG100 successor §3 "Callback Types").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackType {
    /// Not a callback: the response to a command.
    None,
    /// Response to the `callback` command (synchronous / polled mode).
    Sync,
    /// Sent unprompted by a UART NCP (asynchronous mode, the default:
    /// UG100 successor §2 "UART Interface Callbacks").
    Async,
    /// `11`, reserved.
    Reserved,
}

/// Frame control bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameControl {
    pub low: u8,
    /// `None` for the legacy format.
    pub high: Option<u8>,
}

impl FrameControl {
    pub fn is_response(self) -> bool {
        self.low & FC_RESPONSE != 0
    }

    pub fn network_index(self) -> u8 {
        (self.low & FC_NETWORK_INDEX_MASK) >> FC_NETWORK_INDEX_SHIFT
    }

    /// Response: the NCP ran out of memory since the previous response;
    /// callbacks may have been lost.
    pub fn overflow(self) -> bool {
        self.is_response() && self.low & FC_OVERFLOW != 0
    }

    /// Response: the NCP truncated this response.
    pub fn truncated(self) -> bool {
        self.is_response() && self.low & FC_TRUNCATED != 0
    }

    /// Response: at least one more callback is pending on the NCP.
    pub fn callback_pending(self) -> bool {
        self.is_response() && self.low & FC_CALLBACK_PENDING != 0
    }

    pub fn callback_type(self) -> CallbackType {
        if !self.is_response() {
            return CallbackType::None;
        }
        match self.low & FC_CALLBACK_TYPE_MASK {
            0 => CallbackType::None,
            FC_CALLBACK_SYNC => CallbackType::Sync,
            FC_CALLBACK_ASYNC => CallbackType::Async,
            _ => CallbackType::Reserved,
        }
    }

    /// Command: sleep mode after the response (SPI only; 0 = idle).
    pub fn sleep_mode(self) -> u8 {
        if self.is_response() {
            0
        } else {
            self.low & FC_SLEEP_MASK
        }
    }
}

/// Parsed header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub sequence: u8,
    pub control: FrameControl,
    pub frame_id: u16,
}

impl Header {
    pub fn is_legacy(&self) -> bool {
        self.control.high.is_none()
    }
}

/// Header could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// Shorter than the header.
    TooShort(usize),
    /// Legacy format used for a frame other than `version`, or a reserved
    /// frame-format version.
    UnsupportedFormat { byte2: u8 },
    /// Secure EZSP or padding requested: not supported by this crate.
    SecurityOrPadding { fc_high: u8 },
    /// Reserved bits set in the high frame-control byte.
    ReservedBits { fc_high: u8 },
}

impl std::fmt::Display for HeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeaderError::TooShort(n) => write!(f, "EZSP frame of {n} bytes"),
            HeaderError::UnsupportedFormat { byte2 } => {
                write!(f, "unsupported EZSP frame format (byte 2 = 0x{byte2:02X})")
            }
            HeaderError::SecurityOrPadding { fc_high } => {
                write!(
                    f,
                    "secure/padded EZSP frame not supported (fc high 0x{fc_high:02X})"
                )
            }
            HeaderError::ReservedBits { fc_high } => {
                write!(f, "reserved frame-control bits set (0x{fc_high:02X})")
            }
        }
    }
}

impl std::error::Error for HeaderError {}

/// Splits a received EZSP frame into header and parameters.
pub fn parse(bytes: &[u8]) -> Result<(Header, &[u8]), HeaderError> {
    if bytes.len() < 3 {
        return Err(HeaderError::TooShort(bytes.len()));
    }
    let byte2 = bytes[2];
    if byte2 & FCH_FORMAT_MASK == FRAME_FORMAT_VERSION {
        if bytes.len() < 5 {
            return Err(HeaderError::TooShort(bytes.len()));
        }
        if byte2 & (FCH_SECURITY | FCH_PADDING) != 0 {
            return Err(HeaderError::SecurityOrPadding { fc_high: byte2 });
        }
        if byte2 & FCH_RESERVED != 0 {
            return Err(HeaderError::ReservedBits { fc_high: byte2 });
        }
        let header = Header {
            sequence: bytes[0],
            control: FrameControl {
                low: bytes[1],
                high: Some(byte2),
            },
            frame_id: u16::from_le_bytes([bytes[3], bytes[4]]),
        };
        return Ok((header, &bytes[5..]));
    }
    if u16::from(byte2) != VERSION_FRAME_ID {
        return Err(HeaderError::UnsupportedFormat { byte2 });
    }
    let header = Header {
        sequence: bytes[0],
        control: FrameControl {
            low: bytes[1],
            high: None,
        },
        frame_id: VERSION_FRAME_ID,
    };
    Ok((header, &bytes[3..]))
}

/// Extended-format command: idle sleep mode, network index 0.
pub fn command(sequence: u8, frame_id: u16, params: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + params.len());
    let [lo, hi] = frame_id.to_le_bytes();
    out.extend_from_slice(&[sequence, 0x00, FRAME_FORMAT_VERSION, lo, hi]);
    out.extend_from_slice(params);
    out
}

/// Legacy-format `version` command: `seq, 0x00, 0x00, desired`.
pub fn legacy_version_command(sequence: u8, desired: u8) -> Vec<u8> {
    vec![sequence, 0x00, VERSION_FRAME_ID as u8, desired]
}

/// Response flags set by an NCP (simulator side).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResponseFlags {
    pub overflow: bool,
    pub truncated: bool,
    pub callback_pending: bool,
    pub async_callback: bool,
}

impl ResponseFlags {
    fn low(self) -> u8 {
        let mut fc = FC_RESPONSE;
        if self.overflow {
            fc |= FC_OVERFLOW;
        }
        if self.truncated {
            fc |= FC_TRUNCATED;
        }
        if self.callback_pending {
            fc |= FC_CALLBACK_PENDING;
        }
        if self.async_callback {
            fc |= FC_CALLBACK_ASYNC;
        }
        fc
    }
}

/// Extended-format response or callback.
pub fn response(sequence: u8, flags: ResponseFlags, frame_id: u16, params: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + params.len());
    let [lo, hi] = frame_id.to_le_bytes();
    out.extend_from_slice(&[sequence, flags.low(), FRAME_FORMAT_VERSION, lo, hi]);
    out.extend_from_slice(params);
    out
}

/// Legacy-format `version` response: `seq, fc, 0x00, params`.
pub fn legacy_version_response(sequence: u8, flags: ResponseFlags, params: &[u8]) -> Vec<u8> {
    let mut out = vec![sequence, flags.low(), VERSION_FRAME_ID as u8];
    out.extend_from_slice(params);
    out
}
