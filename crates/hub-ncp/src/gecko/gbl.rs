//! Gecko bootloader image (`.gbl`) checker.
//!
//! A GBL file is a sequence of tags `u32 id, u32 length, data` (little
//! endian): a header tag first, an end tag last whose value is the CRC-32
//! (IEEE, as zlib) of every byte before that value. The checker is
//! streaming, so a 1.5 MiB image can be checked from flash in small chunks.
//!
//! Besides the framing it extracts what the hub needs: the application tag
//! (type, version, capabilities, product ID), whether the image is
//! encrypted or signed, whether it carries a bootloader or Secure Engine
//! upgrade (refused by the hub: the Gecko bootloader is never replaced
//! from the hub), and the zigbee-rs metadata record ([`ZrhpMeta`]) used to
//! verify the NCP after the update.

use core::fmt;

/// Tag IDs.
pub mod tag {
    /// Header (`u32 version, u32 type`), first tag.
    pub const HEADER: u32 = 0x03A6_17EB;
    /// Application properties (`u32 type, u32 version, u32 capabilities, [16] product ID`).
    pub const APPLICATION: u32 = 0xF40A_0AF4;
    /// Bootloader upgrade.
    pub const BOOTLOADER: u32 = 0xF509_09F5;
    /// Secure Engine upgrade.
    pub const SE_UPGRADE: u32 = 0x5EA6_17EB;
    /// Free-form metadata (Simplicity Commander `gbl create --metadata`).
    pub const METADATA: u32 = 0xF608_08F6;
    /// Program data.
    pub const PROG: u32 = 0xFE01_01FE;
    /// Erase, then program data.
    pub const ERASE_PROG: u32 = 0xFD03_03FD;
    /// LZ4-compressed program data.
    pub const PROG_LZ4: u32 = 0xFD05_05FD;
    /// LZMA-compressed program data.
    pub const PROG_LZMA: u32 = 0xFD07_07FD;
    /// ECDSA-P256 signature.
    pub const SIGNATURE: u32 = 0xF70A_0AF7;
    /// Certificate.
    pub const CERTIFICATE: u32 = 0xF30B_0BF3;
    /// Version dependency.
    pub const VERSION_DEPENDENCY: u32 = 0x76A6_17EB;
    /// Encryption header.
    pub const ENC_HEADER: u32 = 0xFA06_06FA;
    /// Encryption init.
    pub const ENC_INIT: u32 = 0xF907_07F9;
    /// Encrypted data.
    pub const ENC_DATA: u32 = 0xF806_06F8;
    /// End tag (`u32 crc32`), last tag.
    pub const END: u32 = 0xFC04_04FC;
}

/// Header `type` bit: AES-CCM encrypted.
pub const TYPE_ENCRYPTED: u32 = 0x0000_0001;
/// Header `type` bit: ECDSA-P256 signed.
pub const TYPE_SIGNED: u32 = 0x0000_0100;

/// Magic of the zigbee-rs metadata record.
pub const ZRHP_META_MAGIC: [u8; 4] = *b"ZRHP";

/// CRC-32 (IEEE 802.3, reflected, as zlib).
#[derive(Debug, Clone, Copy)]
pub struct Crc32(u32);

impl Default for Crc32 {
    fn default() -> Self {
        Self(0xFFFF_FFFF)
    }
}

impl Crc32 {
    /// Feed bytes.
    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.0;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            }
        }
        self.0 = c;
    }
    /// The CRC so far.
    pub fn value(&self) -> u32 {
        !self.0
    }
    /// CRC of one buffer.
    pub fn of(data: &[u8]) -> u32 {
        let mut c = Self::default();
        c.update(data);
        c.value()
    }
}

/// The application tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppInfo {
    /// Application type bits (1 Zigbee, 2 Thread, 4 Flex, 8 Bluetooth, …).
    pub app_type: u32,
    /// Application version.
    pub version: u32,
    /// Capabilities.
    pub capabilities: u32,
    /// Product ID.
    pub product_id: [u8; 16],
}

/// zigbee-rs metadata record in the GBL metadata tag:
/// `"ZRHP", u8 format (1), [8] build_id, u8 n, [n] fw` — the `build_id`
/// and firmware string the NCP reports in `SYS_NEGOTIATE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZrhpMeta {
    /// `SYS_NEGOTIATE.build_id`.
    pub build_id: [u8; 8],
    /// `SYS_NEGOTIATE.fw`.
    pub fw: String,
}

impl ZrhpMeta {
    /// Parse a metadata tag body; `None` if it is not a zigbee-rs record.
    pub fn parse(body: &[u8]) -> Option<Self> {
        let rest = body.strip_prefix(&ZRHP_META_MAGIC)?;
        let (&format, rest) = rest.split_first()?;
        if format != 1 || rest.len() < 9 {
            return None;
        }
        let build_id = rest[..8].try_into().ok()?;
        let n = usize::from(rest[8]);
        let fw = core::str::from_utf8(rest.get(9..9 + n)?).ok()?.into();
        Some(Self { build_id, fw })
    }

    /// Encode (for NCP build tools and tests).
    pub fn encode(&self) -> Vec<u8> {
        let fw = &self.fw.as_bytes()[..self.fw.len().min(255)];
        let mut v = Vec::with_capacity(14 + fw.len());
        v.extend_from_slice(&ZRHP_META_MAGIC);
        v.push(1);
        v.extend_from_slice(&self.build_id);
        v.push(fw.len() as u8);
        v.extend_from_slice(fw);
        v
    }
}

/// What a valid image contains.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GblInfo {
    /// Bytes up to and including the end tag.
    pub size: u32,
    /// Header `version`.
    pub gbl_version: u32,
    /// Header `type`.
    pub gbl_type: u32,
    /// Application tag (absent in encrypted images, where it is inside the
    /// encrypted data).
    pub app: Option<AppInfo>,
    /// Contains a bootloader upgrade.
    pub bootloader: bool,
    /// Contains a Secure Engine upgrade.
    pub se_upgrade: bool,
    /// Has a signature tag.
    pub signed: bool,
    /// Program data bytes (all program tags, compressed size).
    pub program_bytes: u32,
    /// zigbee-rs metadata.
    pub zrhp: Option<ZrhpMeta>,
    /// The CRC-32 from the end tag.
    pub crc32: u32,
    /// Bytes after the end tag (padding).
    pub trailing: u32,
}

impl GblInfo {
    /// Encrypted image.
    pub fn encrypted(&self) -> bool {
        self.gbl_type & TYPE_ENCRYPTED != 0
    }
}

/// Why an image is not acceptable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GblError {
    /// The first tag is not a header tag.
    NoHeader,
    /// The file ends inside a tag.
    Truncated,
    /// No end tag.
    NoEnd,
    /// End tag CRC mismatch.
    Crc {
        /// In the file.
        stored: u32,
        /// Computed.
        computed: u32,
    },
    /// Non-padding bytes after the end tag.
    TrailingData,
    /// A tag whose fixed-size body is malformed.
    BadTag(u32),
    /// No program data at all.
    NoProgram,
}

impl fmt::Display for GblError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GblError::NoHeader => write!(f, "not a GBL file (no header tag)"),
            GblError::Truncated => write!(f, "GBL truncated"),
            GblError::NoEnd => write!(f, "GBL has no end tag"),
            GblError::Crc { stored, computed } => {
                write!(f, "GBL CRC mismatch (file 0x{stored:08X}, computed 0x{computed:08X})")
            }
            GblError::TrailingData => write!(f, "data after the GBL end tag"),
            GblError::BadTag(t) => write!(f, "malformed GBL tag 0x{t:08X}"),
            GblError::NoProgram => write!(f, "GBL has no program data"),
        }
    }
}

/// Largest tag body the checker keeps for parsing.
const KEEP: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Collecting the 8-byte tag header.
    TagHeader,
    /// Inside a tag body.
    Body { id: u32, left: u32 },
    /// Inside the end tag's CRC value.
    EndCrc,
    /// After the end tag.
    Done,
}

/// Streaming checker: [`Checker::feed`] the file in any chunking, then
/// [`Checker::finish`].
#[derive(Debug, Clone)]
pub struct Checker {
    state: State,
    crc: Crc32,
    hdr: [u8; 8],
    hdr_len: usize,
    keep: Vec<u8>,
    tags: u32,
    offset: u32,
    info: GblInfo,
    error: Option<GblError>,
}

impl Default for Checker {
    fn default() -> Self {
        Self {
            state: State::TagHeader,
            crc: Crc32::default(),
            hdr: [0; 8],
            hdr_len: 0,
            keep: Vec::new(),
            tags: 0,
            offset: 0,
            info: GblInfo::default(),
            error: None,
        }
    }
}

fn le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

impl Checker {
    /// A fresh checker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the next bytes of the file.
    pub fn feed(&mut self, mut data: &[u8]) {
        while !data.is_empty() && self.error.is_none() {
            match self.state {
                State::TagHeader => {
                    let n = (8 - self.hdr_len).min(data.len());
                    self.hdr[self.hdr_len..self.hdr_len + n].copy_from_slice(&data[..n]);
                    self.crc.update(&data[..n]);
                    self.hdr_len += n;
                    self.offset += n as u32;
                    data = &data[n..];
                    if self.hdr_len == 8 {
                        self.hdr_len = 0;
                        self.tag_start(le(&self.hdr[..4]), le(&self.hdr[4..]));
                    }
                }
                State::Body { id, left } => {
                    let n = (left as usize).min(data.len());
                    self.crc.update(&data[..n]);
                    if self.keep.len() < KEEP {
                        let k = n.min(KEEP - self.keep.len());
                        self.keep.extend_from_slice(&data[..k]);
                    }
                    self.offset += n as u32;
                    data = &data[n..];
                    let left = left - n as u32;
                    self.state = State::Body { id, left };
                    if left == 0 {
                        self.tag_end(id);
                    }
                }
                State::EndCrc => {
                    let n = (4 - self.keep.len()).min(data.len());
                    self.keep.extend_from_slice(&data[..n]);
                    self.offset += n as u32;
                    data = &data[n..];
                    if self.keep.len() == 4 {
                        let stored = le(&self.keep);
                        let computed = self.crc.value();
                        self.info.crc32 = stored;
                        self.info.size = self.offset;
                        self.state = State::Done;
                        if stored != computed {
                            self.error = Some(GblError::Crc { stored, computed });
                        }
                    }
                }
                State::Done => {
                    // XMODEM pads with 0x1A (SUB) or 0xFF.
                    if data.iter().any(|&b| !matches!(b, 0x00 | 0x1A | 0xFF)) {
                        self.error = Some(GblError::TrailingData);
                    }
                    self.info.trailing += data.len() as u32;
                    data = &[];
                }
            }
        }
    }

    fn tag_start(&mut self, id: u32, len: u32) {
        if self.tags == 0 && id != tag::HEADER {
            self.error = Some(GblError::NoHeader);
            return;
        }
        self.tags += 1;
        self.keep.clear();
        if id == tag::END {
            if len != 4 {
                self.error = Some(GblError::BadTag(id));
            }
            self.state = State::EndCrc;
            return;
        }
        match id {
            tag::BOOTLOADER => self.info.bootloader = true,
            tag::SE_UPGRADE => self.info.se_upgrade = true,
            tag::SIGNATURE => self.info.signed = true,
            tag::PROG | tag::ERASE_PROG | tag::PROG_LZ4 | tag::PROG_LZMA | tag::ENC_DATA => {
                self.info.program_bytes = self.info.program_bytes.saturating_add(len)
            }
            _ => {}
        }
        self.state = State::Body { id, left: len };
        if len == 0 {
            self.tag_end(id);
        }
    }

    fn tag_end(&mut self, id: u32) {
        let b = &self.keep;
        match id {
            tag::HEADER if b.len() >= 8 => {
                self.info.gbl_version = le(&b[..4]);
                self.info.gbl_type = le(&b[4..8]);
            }
            tag::HEADER => self.error = Some(GblError::BadTag(id)),
            tag::APPLICATION if b.len() >= 28 => {
                self.info.app = Some(AppInfo {
                    app_type: le(&b[..4]),
                    version: le(&b[4..8]),
                    capabilities: le(&b[8..12]),
                    product_id: b[12..28].try_into().unwrap_or_default(),
                })
            }
            tag::APPLICATION => self.error = Some(GblError::BadTag(id)),
            tag::METADATA if self.info.zrhp.is_none() => self.info.zrhp = ZrhpMeta::parse(b),
            _ => {}
        }
        self.keep.clear();
        self.state = State::TagHeader;
    }

    /// Bytes fed so far.
    pub fn offset(&self) -> u32 {
        self.offset
    }

    /// The result.
    pub fn finish(self) -> Result<GblInfo, GblError> {
        if let Some(e) = self.error {
            return Err(e);
        }
        match self.state {
            State::Done if self.info.program_bytes == 0 => Err(GblError::NoProgram),
            State::Done => Ok(self.info),
            State::TagHeader if self.hdr_len == 0 && self.tags == 0 => Err(GblError::NoHeader),
            State::TagHeader if self.hdr_len == 0 => Err(GblError::NoEnd),
            _ => Err(GblError::Truncated),
        }
    }
}

/// Check a whole image in memory.
pub fn check(image: &[u8]) -> Result<GblInfo, GblError> {
    let mut c = Checker::new();
    c.feed(image);
    c.finish()
}

/// Build a GBL from `(tag, body)` pairs, adding the end tag (tests and
/// tools; real images come from Simplicity Commander).
pub fn build(tags: &[(u32, &[u8])]) -> Vec<u8> {
    let mut v = Vec::new();
    for (id, body) in tags {
        v.extend_from_slice(&id.to_le_bytes());
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
    }
    v.extend_from_slice(&tag::END.to_le_bytes());
    v.extend_from_slice(&4u32.to_le_bytes());
    let crc = Crc32::of(&v);
    v.extend_from_slice(&crc.to_le_bytes());
    v
}

/// Header tag body for [`build`].
pub fn header_body(gbl_type: u32) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&0x0300_0000u32.to_le_bytes());
    b[4..].copy_from_slice(&gbl_type.to_le_bytes());
    b
}

/// Application tag body for [`build`].
pub fn app_body(app: &AppInfo) -> [u8; 28] {
    let mut b = [0u8; 28];
    b[..4].copy_from_slice(&app.app_type.to_le_bytes());
    b[4..8].copy_from_slice(&app.version.to_le_bytes());
    b[8..12].copy_from_slice(&app.capabilities.to_le_bytes());
    b[12..].copy_from_slice(&app.product_id);
    b
}

/// A small valid image (tests): header, application tag, zigbee-rs
/// metadata, `prog_len` bytes of program data.
pub fn sample(version: u32, build_id: [u8; 8], prog_len: usize) -> Vec<u8> {
    let app = AppInfo { app_type: 1, version, capabilities: 0, product_id: [0x24; 16] };
    let meta = ZrhpMeta { build_id, fw: format!("zigbee-rs-ncp mg24 {version:08x}") };
    let mut prog = 0u32.to_le_bytes().to_vec();
    prog.extend((0..prog_len).map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[3]));
    build(&[
        (tag::HEADER, &header_body(0)),
        (tag::APPLICATION, &app_body(&app)),
        (tag::METADATA, &meta.encode()),
        (tag::PROG, &prog),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_zlib() {
        assert_eq!(Crc32::of(b"123456789"), 0xCBF4_3926);
        assert_eq!(Crc32::of(b""), 0);
    }

    #[test]
    fn parses_any_chunking() {
        let image = sample(0x0001_0203, *b"build-01", 3000);
        let whole = check(&image).unwrap();
        let app = whole.app.unwrap();
        assert_eq!((app.app_type, app.version, app.product_id), (1, 0x0001_0203, [0x24; 16]));
        let meta = whole.zrhp.as_ref().unwrap();
        assert_eq!(meta.build_id, *b"build-01");
        assert_eq!(meta.fw, "zigbee-rs-ncp mg24 00010203");
        assert_eq!(whole.size, image.len() as u32);
        assert!(!whole.bootloader && !whole.encrypted() && !whole.signed);
        for chunk in [1, 3, 7, 128, 1000] {
            let mut c = Checker::new();
            for part in image.chunks(chunk) {
                c.feed(part);
            }
            assert_eq!(c.finish().unwrap(), whole, "chunk {chunk}");
        }
        // XMODEM padding after the end tag is fine.
        let mut padded = image.clone();
        padded.resize(image.len().next_multiple_of(128), 0x1A);
        assert_eq!(check(&padded).unwrap().trailing, (padded.len() - image.len()) as u32);
    }

    #[test]
    fn rejects_damage() {
        let image = sample(1, [0; 8], 500);
        let mut bad = image.clone();
        bad[100] ^= 1;
        assert!(matches!(check(&bad), Err(GblError::Crc { .. })));
        assert_eq!(check(&image[..image.len() - 2]), Err(GblError::Truncated));
        assert_eq!(check(&image[..image.len() - 12]), Err(GblError::NoEnd));
        assert_eq!(check(&image[8..]), Err(GblError::NoHeader));
        assert_eq!(check(b""), Err(GblError::NoHeader));
        let mut trailing = image.clone();
        trailing.extend_from_slice(b"junk");
        assert_eq!(check(&trailing), Err(GblError::TrailingData));
        let empty = build(&[(tag::HEADER, &header_body(0))]);
        assert_eq!(check(&empty), Err(GblError::NoProgram));
    }

    #[test]
    fn flags_bootloader_and_encryption() {
        let image = build(&[
            (tag::HEADER, &header_body(TYPE_ENCRYPTED | TYPE_SIGNED)),
            (tag::BOOTLOADER, &[0; 16]),
            (tag::ENC_DATA, &[1; 64]),
            (tag::SIGNATURE, &[0; 64]),
        ]);
        let info = check(&image).unwrap();
        assert!(info.bootloader && info.encrypted() && info.signed);
        assert_eq!(info.app, None);
    }
}
