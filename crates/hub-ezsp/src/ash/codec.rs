//! ASH framing: CRC, control byte, data randomization, byte stuffing and
//! deframing (UG101 §2–§4).

/// Flag Byte: marks the end of a frame (UG101 §2 "Flag Byte", §4 table).
pub const FLAG: u8 = 0x7E;
/// Escape Byte (UG101 §4 "Reserved Bytes and Byte Stuffing").
pub const ESCAPE: u8 = 0x7D;
/// XON (UG101 §4).
pub const XON: u8 = 0x11;
/// XOFF (UG101 §4).
pub const XOFF: u8 = 0x13;
/// Substitute Byte: replaces a byte received with a UART error (UG101 §4).
pub const SUBSTITUTE: u8 = 0x18;
/// Cancel Byte: terminates a frame in progress (UG101 §4).
pub const CANCEL: u8 = 0x1A;
/// Wake byte, only meaningful between frames (UG101 §4).
pub const WAKE: u8 = 0xFF;
/// Bit 5 is complemented on an escaped byte (UG101 §4).
const FLIP: u8 = 0x20;

/// ASH protocol version carried in RSTACK and ERROR (UG101 §3).
pub const ASH_VERSION: u8 = 2;

/// Smallest DATA field: an EZSP legacy frame header (sequence, frame
/// control, frame ID). Gecko SDK `ash-protocol.h`: `ASH_MIN_DATA_FIELD_LEN =
/// EZSP_MIN_FRAME_LENGTH` (3). UG101 §2 also gives 3.
pub const MIN_DATA_FIELD: usize = 3;
/// Largest DATA field. UG101 §2 says 128, but the EZSP ≥ 13 SDKs set
/// `ASH_MAX_DATA_FIELD_LEN = EZSP_MAX_FRAME_LENGTH = 218 + 1 + 1` (Gecko SDK
/// 4.4 and Simplicity SDK 2024.6–2025.6 `ezsp-protocol.h`), so the host
/// must accept up to 220 bytes.
pub const MAX_DATA_FIELD: usize = 220;
/// Largest frame before stuffing: control + data + CRC.
pub const MAX_FRAME: usize = 1 + MAX_DATA_FIELD + 2;
/// Upper bound of one encoded frame on the wire: a leading Cancel Byte,
/// every byte escaped, and the Flag Byte.
pub const MAX_ENCODED_FRAME: usize = 1 + 2 * MAX_FRAME + 1;

/// CRC-CCITT, g(x) = x^16 + x^12 + x^5 + 1, initialised to 0xFFFF, over
/// the control byte and data field before stuffing (UG101 §2 "CRC").
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Exclusive-ORs `data` with the ASH pseudo-random sequence (UG101 §4
/// "Data Randomization": rand0 = 0x42; rand(i+1) = rand(i) >> 1, xor 0xB8
/// when bit 0 was set). The operation is its own inverse.
pub fn randomize(data: &mut [u8]) {
    let mut r: u8 = 0x42;
    for b in data {
        *b ^= r;
        r = if r & 1 != 0 { (r >> 1) ^ 0xB8 } else { r >> 1 };
    }
}

/// Byte values that must be escaped inside a frame (UG101 §4 table).
pub fn is_reserved(b: u8) -> bool {
    matches!(b, FLAG | ESCAPE | XON | XOFF | SUBSTITUTE | CANCEL)
}

/// Appends `b`, escaped if it is reserved (UG101 §4).
pub fn stuff_byte(b: u8, out: &mut Vec<u8>) {
    if is_reserved(b) {
        out.push(ESCAPE);
        out.push(b ^ FLIP);
    } else {
        out.push(b);
    }
}

/// Decoded control byte (UG101 §2 "Control Byte" table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// `0 frmNum(3) reTx ackNum(3)`.
    Data {
        frm_num: u8,
        re_tx: bool,
        ack_num: u8,
    },
    /// `1 0 0 res nRdy ackNum(3)`.
    Ack { ack_num: u8, not_ready: bool },
    /// `1 0 1 res nRdy ackNum(3)`.
    Nak { ack_num: u8, not_ready: bool },
    /// `0xC0`, host → NCP.
    Rst,
    /// `0xC1`, NCP → host.
    RstAck,
    /// `0xC2`, NCP → host.
    Error,
}

impl Control {
    /// Encodes the control byte; `res` is sent as 0.
    pub fn encode(self) -> u8 {
        match self {
            Control::Data {
                frm_num,
                re_tx,
                ack_num,
            } => ((frm_num & 7) << 4) | (u8::from(re_tx) << 3) | (ack_num & 7),
            Control::Ack { ack_num, not_ready } => {
                0x80 | (u8::from(not_ready) << 3) | (ack_num & 7)
            }
            Control::Nak { ack_num, not_ready } => {
                0xA0 | (u8::from(not_ready) << 3) | (ack_num & 7)
            }
            Control::Rst => 0xC0,
            Control::RstAck => 0xC1,
            Control::Error => 0xC2,
        }
    }

    /// Decodes a control byte; `None` for values that are no frame type.
    /// The reserved bit (b4) of ACK/NAK "may be either 0 or 1" (UG101 §2).
    pub fn decode(b: u8) -> Option<Control> {
        if b & 0x80 == 0 {
            return Some(Control::Data {
                frm_num: (b >> 4) & 7,
                re_tx: b & 0x08 != 0,
                ack_num: b & 7,
            });
        }
        match b & 0xE0 {
            0x80 => Some(Control::Ack {
                ack_num: b & 7,
                not_ready: b & 0x08 != 0,
            }),
            0xA0 => Some(Control::Nak {
                ack_num: b & 7,
                not_ready: b & 0x08 != 0,
            }),
            _ => match b {
                0xC0 => Some(Control::Rst),
                0xC1 => Some(Control::RstAck),
                0xC2 => Some(Control::Error),
                _ => None,
            },
        }
    }
}

/// A validated ASH frame (CRC checked, DATA field de-randomized).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Carries one EZSP frame.
    Data {
        frm_num: u8,
        re_tx: bool,
        ack_num: u8,
        payload: Vec<u8>,
    },
    /// Acknowledges DATA frames up to, not including, `ack_num`.
    Ack { ack_num: u8, not_ready: bool },
    /// Negative acknowledgement.
    Nak { ack_num: u8, not_ready: bool },
    /// Reset request.
    Rst,
    /// Reset acknowledgement: ASH version and reset code (UG101 §3, §6).
    RstAck { version: u8, reset_code: u8 },
    /// NCP entered the FAILED state (UG101 §3, §6).
    Error { version: u8, code: u8 },
}

/// Why a received frame was discarded (UG101 §4 "Receiving Frames").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Fewer bytes than control + CRC.
    TooShort,
    /// CRC mismatch.
    BadCrc,
    /// Control byte of no known frame type.
    BadControl(u8),
    /// Data field length invalid for the frame type.
    BadLength { control: u8, len: usize },
}

impl Frame {
    /// The control byte of this frame.
    pub fn control(&self) -> Control {
        match *self {
            Frame::Data {
                frm_num,
                re_tx,
                ack_num,
                ..
            } => Control::Data {
                frm_num,
                re_tx,
                ack_num,
            },
            Frame::Ack { ack_num, not_ready } => Control::Ack { ack_num, not_ready },
            Frame::Nak { ack_num, not_ready } => Control::Nak { ack_num, not_ready },
            Frame::Rst => Control::Rst,
            Frame::RstAck { .. } => Control::RstAck,
            Frame::Error { .. } => Control::Error,
        }
    }

    /// Appends the frame as sent on the wire: control and data field (DATA
    /// randomized if `randomize`), big-endian CRC, everything stuffed,
    /// then the Flag Byte (UG101 §4 "Sending Frames"). No Cancel Byte.
    pub fn encode(&self, randomize_data: bool, out: &mut Vec<u8>) {
        let mut raw = [0u8; MAX_FRAME];
        raw[0] = self.control().encode();
        let mut len = 1;
        match self {
            Frame::Data { payload, .. } => {
                debug_assert!((MIN_DATA_FIELD..=MAX_DATA_FIELD).contains(&payload.len()));
                let n = payload.len().min(MAX_DATA_FIELD);
                raw[1..1 + n].copy_from_slice(&payload[..n]);
                if randomize_data {
                    randomize(&mut raw[1..1 + n]);
                }
                len += n;
            }
            Frame::RstAck {
                version,
                reset_code,
            } => {
                raw[1] = *version;
                raw[2] = *reset_code;
                len = 3;
            }
            Frame::Error { version, code } => {
                raw[1] = *version;
                raw[2] = *code;
                len = 3;
            }
            Frame::Ack { .. } | Frame::Nak { .. } | Frame::Rst => {}
        }
        let crc = crc16(&raw[..len]);
        raw[len] = (crc >> 8) as u8;
        raw[len + 1] = crc as u8;
        len += 2;
        for &b in &raw[..len] {
            stuff_byte(b, out);
        }
        out.push(FLAG);
    }

    /// Validates an unstuffed frame (control, data field, CRC; no flag):
    /// CRC, control byte and data field length (UG101 §4 "Receiving
    /// Frames"). The ackNum range check belongs to the link state machine.
    pub fn decode(raw: &[u8], randomize_data: bool) -> Result<Frame, FrameError> {
        if raw.len() < 3 {
            return Err(FrameError::TooShort);
        }
        let (body, crc) = raw.split_at(raw.len() - 2);
        if crc16(body) != u16::from_be_bytes([crc[0], crc[1]]) {
            return Err(FrameError::BadCrc);
        }
        let control = body[0];
        let data = &body[1..];
        let bad_len = || FrameError::BadLength {
            control,
            len: data.len(),
        };
        match Control::decode(control).ok_or(FrameError::BadControl(control))? {
            Control::Data {
                frm_num,
                re_tx,
                ack_num,
            } => {
                if !(MIN_DATA_FIELD..=MAX_DATA_FIELD).contains(&data.len()) {
                    return Err(bad_len());
                }
                let mut payload = data.to_vec();
                if randomize_data {
                    randomize(&mut payload);
                }
                Ok(Frame::Data {
                    frm_num,
                    re_tx,
                    ack_num,
                    payload,
                })
            }
            Control::Ack { ack_num, not_ready } if data.is_empty() => {
                Ok(Frame::Ack { ack_num, not_ready })
            }
            Control::Nak { ack_num, not_ready } if data.is_empty() => {
                Ok(Frame::Nak { ack_num, not_ready })
            }
            Control::Rst if data.is_empty() => Ok(Frame::Rst),
            Control::RstAck if data.len() == 2 => Ok(Frame::RstAck {
                version: data[0],
                reset_code: data[1],
            }),
            Control::Error if data.len() == 2 => Ok(Frame::Error {
                version: data[0],
                code: data[1],
            }),
            _ => Err(bad_len()),
        }
    }
}

/// Outcome of feeding one byte to the [`Deframer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deframed {
    /// A Flag Byte closed a frame; read it with [`Deframer::frame`].
    Frame,
    /// A Cancel Byte discarded a partial frame.
    Cancelled,
    /// The frame contained a Substitute Byte (UART error) and was dropped.
    Substituted,
    /// The frame exceeded [`MAX_FRAME`] bytes and was dropped.
    TooLong,
}

/// Byte-level receiver: reverses stuffing and splits frames on Flag Bytes
/// (UG101 §4). Holds at most [`MAX_FRAME`] bytes.
#[derive(Debug, Default)]
pub struct Deframer {
    buf: Vec<u8>,
    escaped: bool,
    substituted: bool,
    overflow: bool,
    complete: bool,
}

impl Deframer {
    /// Empty receiver.
    pub fn new() -> Self {
        Deframer {
            buf: Vec::with_capacity(MAX_FRAME),
            ..Default::default()
        }
    }

    /// Drops any partial frame.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.escaped = false;
        self.substituted = false;
        self.overflow = false;
        self.complete = false;
    }

    /// The frame returned by the last [`Deframed::Frame`]; valid until the
    /// next [`push`](Self::push).
    pub fn frame(&self) -> &[u8] {
        &self.buf
    }

    /// Feeds one received byte.
    ///
    /// - Flag: ends the frame; consecutive flags are ignored (UG101 §2).
    /// - Cancel: discards the bytes since the previous flag (UG101 §4). It
    ///   also clears a pending Substitute, because RST/RSTACK are sent
    ///   after a Cancel Byte to get past link noise (our reading of §4).
    /// - Substitute: the frame is dropped up to the next flag (UG101 §4).
    /// - XON/XOFF: flow control, never frame data; ignored here.
    /// - Escape: complements bit 5 of the next byte unless that byte is
    ///   reserved, in which case the escape has no effect (UG101 §4).
    /// - 0xFF between frames: wake / callback-pending signal (UG101 §4);
    ///   ignored. Inside a frame it is ordinary data.
    // UNVERIFIED: Cancel clearing a pending Substitute, and the host
    // dropping XON/XOFF (UG101 only says the NCP ignores them), are our
    // reading of UG101 §4.
    pub fn push(&mut self, b: u8) -> Option<Deframed> {
        if self.complete {
            self.buf.clear();
            self.complete = false;
        }
        match b {
            FLAG => {
                self.escaped = false;
                let result = if self.substituted {
                    Some(Deframed::Substituted)
                } else if self.overflow {
                    Some(Deframed::TooLong)
                } else if self.buf.is_empty() {
                    None
                } else {
                    self.complete = true;
                    return Some(Deframed::Frame);
                };
                self.buf.clear();
                self.substituted = false;
                self.overflow = false;
                result
            }
            CANCEL => {
                let had_data = !self.buf.is_empty() || self.substituted || self.overflow;
                self.clear();
                had_data.then_some(Deframed::Cancelled)
            }
            SUBSTITUTE => {
                self.buf.clear();
                self.escaped = false;
                self.substituted = true;
                None
            }
            XON | XOFF => {
                self.escaped = false;
                None
            }
            _ if self.substituted => None,
            ESCAPE => {
                self.escaped = true;
                None
            }
            WAKE if self.buf.is_empty() && !self.escaped && !self.overflow => None,
            _ => {
                let v = if self.escaped { b ^ FLIP } else { b };
                self.escaped = false;
                if self.buf.len() < MAX_FRAME {
                    self.buf.push(v);
                } else {
                    self.overflow = true;
                }
                None
            }
        }
    }
}
