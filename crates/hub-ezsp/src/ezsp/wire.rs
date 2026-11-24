//! Little-endian parameter encoding (UG100 successor §2 "Byte Order").

/// Parameters could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The frame ended before field `at`.
    Truncated { at: usize },
    /// Bytes left over after the last field: usually a layout mismatch.
    Trailing { extra: usize },
    /// A field value this crate cannot represent.
    Invalid(&'static str),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Truncated { at } => write!(f, "parameters end at byte {at}"),
            DecodeError::Trailing { extra } => write!(f, "{extra} unexpected trailing bytes"),
            DecodeError::Invalid(what) => write!(f, "invalid {what}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Cursor over received parameters.
#[derive(Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len());
        let end = end.ok_or(DecodeError::Truncated { at: self.pos })?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.bytes(N)?);
        Ok(a)
    }

    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.bytes(1)?[0])
    }

    pub fn i8(&mut self) -> Result<i8, DecodeError> {
        Ok(self.u8()? as i8)
    }

    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        Ok(self.u8()? != 0)
    }

    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    /// A `uint8_t length` followed by that many bytes.
    pub fn len_prefixed(&mut self) -> Result<Vec<u8>, DecodeError> {
        let n = self.u8()?;
        Ok(self.bytes(usize::from(n))?.to_vec())
    }

    /// Bytes not read yet.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Fails if bytes are left.
    pub fn finish(self) -> Result<(), DecodeError> {
        match self.buf.len() - self.pos {
            0 => Ok(()),
            extra => Err(DecodeError::Trailing { extra }),
        }
    }
}

/// Parameter builder.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Writer::default()
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn bool(&mut self, v: bool) {
        self.buf.push(u8::from(v));
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }

    /// `uint8_t length` + bytes. The caller checks `v.len() <= 255`.
    pub fn len_prefixed(&mut self, v: &[u8]) {
        debug_assert!(v.len() <= 255);
        self.u8(v.len() as u8);
        self.bytes(v);
    }
}
