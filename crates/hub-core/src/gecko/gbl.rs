//! GBL3 (Gecko Bootloader upgrade file) validation, streamed: the hub
//! checks an MG24 image while it is uploaded (chunk by chunk, nothing held
//! in RAM) and again before it flashes it, so that only an image the
//! resident Gecko bootloader will take, and that cannot touch the
//! bootloader itself, ever reaches the MG24.
//!
//! Format (Silicon Labs `platform/bootloader/parser/gbl/btl_gbl_format.h`,
//! GSDK 4.x; UG489): a sequence of `{u32 tag, u32 length, length bytes}`
//! records, little endian, ending with the end tag whose CRC-32 (IEEE,
//! zlib) covers every byte before the CRC itself:
//!
//! | Tag | Record | Here |
//! |---|---|---|
//! | `0x03A617EB` | header: `u32 version` (major `0x03`), `u32 type` (bit 0 AES-CCM encrypted, bit 8 ECDSA signed) | first; encrypted refused, signed noted |
//! | `0xF40A0AF4` | application: `ApplicationData_t` (type, version, capabilities, product ID) | at most once; a Zigbee application, or type 0 (none: a maintenance image) |
//! | `0x76A617EB` | version dependency | accepted |
//! | `0xF60808F6` | metadata | accepted; the ZRHP record parsed |
//! | `0xFE0101FE`, `0xFD0303FD` | program, erase-and-program: `u32 address`, data | only inside the application area |
//! | `0xFD0505FD`, `0xFD0707FD` | program, LZ4 / LZMA compressed | refused (size unknown before decompression; the stock bootloader may lack the decompressor) |
//! | `0xF50909F5` | bootloader upgrade | refused: the hub never replaces the bootloader |
//! | `0x5EA617EB` | Secure Engine upgrade | refused |
//! | `0xFB0505FB`, `0xFA0606FA`, `0xF90707F9` | encryption header, init, data | refused |
//! | `0xF30B0BF3`, `0xF70A0AF7` | ECDSA certificate, signature | accepted, noted ([`GblInfo::signed`]): a bootloader without the signing key refuses the image |
//! | `0xFC0404FC` | end: `u32 crc32` | last; only `0xFF` padding may follow |
//!
//! zigbee-rs ZRHP NCP images carry a metadata record (zigbee-rs
//! `docs/book/src/advanced/host-protocol.md` 7.9, written by
//! `tools/efr32-gbl`): `"ZRHP", u8 version = 1, [8] build_id, u8 n, n
//! bytes firmware version` ([`ZrhpRecord`]). Such an application image
//! must program only the ZRHP application window ([`Policy::zrhp_app_end`]:
//! its NV journals sit above it); any other image may program up to the
//! end of flash. An image without an application (no application tag, or
//! one of type 0) is a maintenance image ([`ImageKind::Maintenance`]): it
//! writes data only, e.g. zigbee-rs `efr32mg24-nv-clear.gbl` (`0xFF` over
//! the NV journals) or Nerivec's NVM3 clears; the application already on
//! the chip stays and boots after it.

use core::fmt;

/// GBL header tag.
pub const TAG_HEADER: u32 = 0x03A6_17EB;
/// Bootloader upgrade.
pub const TAG_BOOTLOADER: u32 = 0xF509_09F5;
/// Application information.
pub const TAG_APPLICATION: u32 = 0xF40A_0AF4;
/// Metadata.
pub const TAG_METADATA: u32 = 0xF608_08F6;
/// Program.
pub const TAG_PROG: u32 = 0xFE01_01FE;
/// Program, LZ4 compressed.
pub const TAG_PROG_LZ4: u32 = 0xFD05_05FD;
/// Program, LZMA compressed.
pub const TAG_PROG_LZMA: u32 = 0xFD07_07FD;
/// Erase and program.
pub const TAG_ERASEPROG: u32 = 0xFD03_03FD;
/// End, with the file's CRC-32.
pub const TAG_END: u32 = 0xFC04_04FC;
/// Secure Engine upgrade.
pub const TAG_SE_UPGRADE: u32 = 0x5EA6_17EB;
/// Version dependency.
pub const TAG_VERSION_DEPENDENCY: u32 = 0x76A6_17EB;
/// Encryption header.
pub const TAG_ENC_HEADER: u32 = 0xFB05_05FB;
/// Encryption init.
pub const TAG_ENC_INIT: u32 = 0xFA06_06FA;
/// Encrypted data.
pub const TAG_ENC_GBL_DATA: u32 = 0xF907_07F9;
/// ECDSA P-256 signature.
pub const TAG_SIGNATURE_ECDSA_P256: u32 = 0xF70A_0AF7;
/// ECDSA P-256 certificate.
pub const TAG_CERTIFICATE_ECDSA_P256: u32 = 0xF30B_0BF3;

/// Major version of the GBL format (`GBL_COMPATIBILITY_MAJOR_VERSION`).
pub const VERSION_MAJOR: u32 = 0x0300_0000;
/// Header type bit: AES-CCM encrypted.
pub const TYPE_ENCRYPTED: u32 = 0x0000_0001;
/// Header type bit: ECDSA P-256 signed.
pub const TYPE_SIGNED: u32 = 0x0000_0100;
/// `APPLICATION_TYPE_ZIGBEE` (`application_properties.h`).
pub const APP_TYPE_ZIGBEE: u32 = 1 << 0;

/// Size of `ApplicationData_t`.
const APP_DATA_LEN: usize = 28;
/// Magic of the ZRHP metadata record.
pub const ZRHP_MAGIC: [u8; 4] = *b"ZRHP";
/// Version of the ZRHP metadata record this parser knows.
pub const ZRHP_RECORD_VERSION: u8 = 1;
/// Longest firmware version a ZRHP record holds.
pub const ZRHP_FW_MAX: usize = 31;
/// Trailing `0xFF` bytes accepted after the end tag (Simplicity Commander
/// pads to 4 bytes; a file saved from an XMODEM transfer, to 128).
const MAX_PADDING: u32 = 127;

/// Where an image may write, and how large the file may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Lowest address a program record may write: the application base
    /// (the Gecko bootloader lies below it and is never written).
    pub app_start: u32,
    /// End of flash.
    pub flash_end: u32,
    /// End of the zigbee-rs ZRHP NCP's application window (its NV journals
    /// lie above it, outside every GBL).
    pub zrhp_app_end: u32,
    /// Largest file accepted (the store it is uploaded into).
    pub max_file: u32,
}

impl Policy {
    /// EFR32MG24 with 1536 KiB of flash and a 24 KiB Gecko bootloader, as
    /// on the SONOFF Dongle-M: application from `0x0800_6000`, the ZRHP
    /// NCP's NV journals from `0x0815_8000` (zigbee-rs
    /// `products/efr32mg24/src/layout.rs`), `max_file` the image store.
    pub const fn mg24_1536k(max_file: u32) -> Policy {
        Policy {
            app_start: 0x0800_6000,
            flash_end: 0x0818_0000,
            zrhp_app_end: 0x0815_8000,
            max_file,
        }
    }
}

/// `ApplicationData_t` of the application tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplicationData {
    /// `APPLICATION_TYPE_*` bits.
    pub app_type: u32,
    /// Application version (EmberZNet: `0xMMmmpp00`, 7.4.5.0 = `0x0704_0500`).
    pub version: u32,
    /// Capabilities.
    pub capabilities: u32,
    /// Product ID.
    pub product_id: [u8; 16],
}

/// The ZRHP metadata record of a zigbee-rs NCP image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZrhpRecord {
    /// First 8 bytes of the SHA-256 of the raw image with this field
    /// zeroed; the NCP reports it in `SYS_NEGOTIATE.build_id`.
    pub build_id: [u8; 8],
    fw: [u8; ZRHP_FW_MAX],
    fw_len: u8,
}

impl ZrhpRecord {
    /// The firmware version (`SYS_NEGOTIATE.fw`), `?` if not UTF-8.
    pub fn fw(&self) -> &str {
        core::str::from_utf8(self.fw_bytes()).unwrap_or("?")
    }

    /// The firmware version bytes.
    pub fn fw_bytes(&self) -> &[u8] {
        &self.fw[..usize::from(self.fw_len)]
    }

    fn decode(m: &[u8]) -> Result<Option<Self>, GblError> {
        if !m.starts_with(&ZRHP_MAGIC) {
            return Ok(None);
        }
        if m.len() < 14 || m[4] != ZRHP_RECORD_VERSION {
            return Err(GblError::ZrhpRecord);
        }
        let n = usize::from(m[13]);
        if n > ZRHP_FW_MAX || m.len() != 14 + n {
            return Err(GblError::ZrhpRecord);
        }
        let mut fw = [0u8; ZRHP_FW_MAX];
        fw[..n].copy_from_slice(&m[14..]);
        let mut build_id = [0u8; 8];
        build_id.copy_from_slice(&m[5..13]);
        Ok(Some(ZrhpRecord {
            build_id,
            fw,
            fw_len: n as u8,
        }))
    }
}

/// What the image is, for the hub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// A zigbee-rs ZRHP NCP application (it carries the ZRHP record).
    Zrhp,
    /// Another application: EmberZNet (EZSP), a zigbee-rs router.
    Other,
    /// No application: data only (an NV or NVM3 clear); the application
    /// on the chip stays.
    Maintenance,
}

impl ImageKind {
    /// API name.
    pub fn as_str(self) -> &'static str {
        match self {
            ImageKind::Zrhp => "zrhp",
            ImageKind::Other => "other",
            ImageKind::Maintenance => "maintenance",
        }
    }
}

/// A valid image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GblInfo {
    /// File size.
    pub size: u32,
    /// Bytes up to the end of the end tag (its CRC included): what to send
    /// the bootloader. Its parser takes the end tag's packet and refuses
    /// every packet after it (`BOOTLOADER_ERROR_PARSER_EOF`), so padding
    /// that would fill a packet of its own must not be sent.
    pub gbl_len: u32,
    /// The end tag's CRC-32.
    pub crc32: u32,
    /// The header's type (bit 8: signed).
    pub gbl_type: u32,
    /// The application tag.
    pub app: Option<ApplicationData>,
    /// Program records.
    pub prog_tags: u32,
    /// Bytes they program.
    pub prog_bytes: u32,
    /// Lowest address programmed.
    pub prog_start: u32,
    /// End of the highest range programmed (exclusive).
    pub prog_end: u32,
    /// Signature or certificate tags, or the signed type bit.
    pub signed: bool,
    /// A version dependency tag.
    pub version_dependency: bool,
    /// Metadata records.
    pub metadata: u32,
    /// The ZRHP record.
    pub zrhp: Option<ZrhpRecord>,
}

impl GblInfo {
    /// An application image: an application tag of a type other than 0.
    pub fn has_application(&self) -> bool {
        self.app.is_some_and(|a| a.app_type != 0)
    }

    /// zigbee-rs ZRHP NCP, another application, or a maintenance image.
    pub fn kind(&self) -> ImageKind {
        if !self.has_application() {
            ImageKind::Maintenance
        } else if self.zrhp.is_some() {
            ImageKind::Zrhp
        } else {
            ImageKind::Other
        }
    }
}

/// Why an image is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GblError {
    /// The file does not start with a GBL3 header tag.
    NotGbl,
    /// Another major version of the format.
    Version(u32),
    /// AES-CCM encrypted.
    Encrypted,
    /// Compressed program records.
    Compressed,
    /// A bootloader upgrade.
    BootloaderUpgrade,
    /// A Secure Engine upgrade.
    SeUpgrade,
    /// A tag this parser does not know.
    UnknownTag(u32),
    /// A tag with an impossible length.
    BadLength {
        /// The tag.
        tag: u32,
        /// Its length.
        len: u32,
    },
    /// A second application tag.
    DuplicateApplication,
    /// The application is not a Zigbee one (its type bits).
    NotZigbee(u32),
    /// A program record below the application base: it would write the
    /// bootloader.
    BelowApplication {
        /// Its address.
        address: u32,
    },
    /// A program record past the end of flash.
    BeyondFlash {
        /// Its end.
        end: u32,
    },
    /// A ZRHP application image programming above its application window
    /// (its NV journals).
    BeyondZrhpWindow {
        /// The image's highest end.
        end: u32,
    },
    /// A ZRHP record of another version, or malformed.
    ZrhpRecord,
    /// The image programs nothing.
    NoProgram,
    /// The end tag's CRC differs.
    Crc {
        /// In the end tag.
        stored: u32,
        /// Of the file.
        computed: u32,
    },
    /// The file ends before its end tag.
    Truncated,
    /// Bytes after the end tag other than a little `0xFF` padding.
    TrailingData,
    /// Larger than the store.
    TooLarge {
        /// The store's size.
        max: u32,
    },
}

impl GblError {
    /// Short code for the API.
    pub fn code(&self) -> &'static str {
        match self {
            GblError::NotGbl
            | GblError::Version(_)
            | GblError::UnknownTag(_)
            | GblError::BadLength { .. } => "not_gbl",
            GblError::Encrypted => "encrypted",
            GblError::Compressed => "compressed",
            GblError::BootloaderUpgrade | GblError::SeUpgrade => "bootloader_upgrade",
            GblError::DuplicateApplication | GblError::NotZigbee(_) | GblError::NoProgram => {
                "not_ncp_image"
            }
            GblError::BelowApplication { .. }
            | GblError::BeyondFlash { .. }
            | GblError::BeyondZrhpWindow { .. } => "address_range",
            GblError::ZrhpRecord => "zrhp_record",
            GblError::Crc { .. } | GblError::Truncated | GblError::TrailingData => "corrupt",
            GblError::TooLarge { .. } => "too_large",
        }
    }
}

impl fmt::Display for GblError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GblError::NotGbl => write!(f, "not a GBL file (no GBL3 header tag)"),
            GblError::Version(v) => {
                write!(f, "GBL format version 0x{v:08X}, only 3.x is supported")
            }
            GblError::Encrypted => write!(
                f,
                "encrypted GBL: the MG24's bootloader holds no decryption key for it"
            ),
            GblError::Compressed => write!(
                f,
                "compressed program records (LZ4/LZMA): repackage the image uncompressed"
            ),
            GblError::BootloaderUpgrade => write!(
                f,
                "the GBL upgrades the Gecko bootloader: refused, the bootloader is never replaced"
            ),
            GblError::SeUpgrade => {
                write!(f, "the GBL upgrades the Secure Engine firmware: refused")
            }
            GblError::UnknownTag(t) => write!(f, "unknown GBL tag 0x{t:08X}"),
            GblError::BadLength { tag, len } => {
                write!(f, "GBL tag 0x{tag:08X} with an impossible length {len}")
            }
            GblError::DuplicateApplication => write!(f, "two application tags"),
            GblError::NotZigbee(t) => {
                write!(f, "not a Zigbee application (application type 0x{t:08X})")
            }
            GblError::BelowApplication { address } => {
                write!(
                    f,
                    "the image writes 0x{address:08X}, below the application base: it would overwrite the bootloader"
                )
            }
            GblError::BeyondFlash { end } => write!(
                f,
                "the image writes up to 0x{end:08X}, past the end of the MG24's flash"
            ),
            GblError::BeyondZrhpWindow { end } => {
                write!(
                    f,
                    "the ZRHP image writes up to 0x{end:08X}, into the NCP's NV journals above its application window"
                )
            }
            GblError::ZrhpRecord => {
                write!(f, "a ZRHP metadata record of another version, or malformed")
            }
            GblError::NoProgram => write!(f, "the image programs nothing"),
            GblError::Crc { stored, computed } => write!(
                f,
                "CRC-32 mismatch: stored 0x{stored:08X}, computed 0x{computed:08X}"
            ),
            GblError::Truncated => write!(f, "the file ends before its end tag"),
            GblError::TrailingData => write!(f, "data after the end tag"),
            GblError::TooLarge { max } => write!(f, "larger than the image store ({max} bytes)"),
        }
    }
}

/// CRC-32 (IEEE 802.3, reflected, as zlib), streamed.
#[derive(Debug, Clone, Copy)]
pub struct Crc32(u32);

const CRC32_NIBBLES: [u32; 16] = {
    let mut t = [0u32; 16];
    let mut i = 0;
    while i < 16 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 4 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

impl Default for Crc32 {
    fn default() -> Self {
        Crc32(0xFFFF_FFFF)
    }
}

impl Crc32 {
    /// Feed bytes.
    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.0;
        for &b in data {
            c ^= u32::from(b);
            c = CRC32_NIBBLES[(c & 0xF) as usize] ^ (c >> 4);
            c = CRC32_NIBBLES[(c & 0xF) as usize] ^ (c >> 4);
        }
        self.0 = c;
    }

    /// The CRC of what was fed.
    pub fn value(&self) -> u32 {
        !self.0
    }
}

/// CRC-32 of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::default();
    c.update(data);
    c.value()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Collecting a tag header.
    Tag,
    /// Inside a tag's body: `left` bytes to go.
    Body { tag: u32, len: u32, left: u32 },
    /// After the end tag.
    Padding,
}

/// Streamed GBL validator: [`feed`](Self::feed) the file in pieces of any
/// size, then [`finish`](Self::finish). The first error sticks.
#[derive(Debug, Clone)]
pub struct GblParser {
    policy: Policy,
    stage: Stage,
    head: [u8; 8],
    head_len: usize,
    /// The start of the current tag's body (as much as it needs).
    body: [u8; 64],
    body_len: usize,
    crc: Crc32,
    /// CRC of everything up to the end tag's header.
    crc_at_end: Option<u32>,
    offset: u32,
    tags: u32,
    padding: u32,
    info: GblInfo,
    error: Option<GblError>,
}

impl GblParser {
    /// A parser for a file checked against `policy`.
    pub fn new(policy: Policy) -> Self {
        GblParser {
            policy,
            stage: Stage::Tag,
            head: [0; 8],
            head_len: 0,
            body: [0; 64],
            body_len: 0,
            crc: Crc32::default(),
            crc_at_end: None,
            offset: 0,
            tags: 0,
            padding: 0,
            info: GblInfo {
                size: 0,
                gbl_len: 0,
                crc32: 0,
                gbl_type: 0,
                app: None,
                prog_tags: 0,
                prog_bytes: 0,
                prog_start: u32::MAX,
                prog_end: 0,
                signed: false,
                version_dependency: false,
                metadata: 0,
                zrhp: None,
            },
            error: None,
        }
    }

    /// Bytes fed so far.
    pub fn offset(&self) -> u32 {
        self.offset
    }

    /// The next piece of the file.
    pub fn feed(&mut self, data: &[u8]) -> Result<(), GblError> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let r = self.feed_inner(data);
        if let Err(e) = r {
            self.error = Some(e);
        }
        r
    }

    fn feed_inner(&mut self, mut data: &[u8]) -> Result<(), GblError> {
        let total = u64::from(self.offset) + data.len() as u64;
        if total > u64::from(self.policy.max_file) {
            return Err(GblError::TooLarge {
                max: self.policy.max_file,
            });
        }
        while !data.is_empty() {
            match self.stage {
                Stage::Tag => {
                    let take = (8 - self.head_len).min(data.len());
                    self.head[self.head_len..self.head_len + take].copy_from_slice(&data[..take]);
                    self.eat(&data[..take]);
                    self.head_len += take;
                    data = &data[take..];
                    if self.head_len == 8 {
                        self.head_len = 0;
                        self.tag_start()?;
                    }
                }
                Stage::Body { tag, len, left } => {
                    let take = (left as usize).min(data.len());
                    let keep = needed(tag, len).saturating_sub(self.body_len).min(take);
                    self.body[self.body_len..self.body_len + keep].copy_from_slice(&data[..keep]);
                    self.body_len += keep;
                    self.eat(&data[..take]);
                    data = &data[take..];
                    let left = left - take as u32;
                    self.stage = Stage::Body { tag, len, left };
                    if left == 0 {
                        self.tag_end(tag, len)?;
                    }
                }
                Stage::Padding => {
                    for &b in data {
                        self.padding += 1;
                        if b != 0xFF || self.padding > MAX_PADDING {
                            return Err(GblError::TrailingData);
                        }
                    }
                    self.offset += data.len() as u32;
                    data = &[];
                }
            }
        }
        Ok(())
    }

    fn eat(&mut self, d: &[u8]) {
        if self.crc_at_end.is_none() {
            self.crc.update(d);
        }
        self.offset += d.len() as u32;
    }

    fn tag_start(&mut self) -> Result<(), GblError> {
        let tag = u32::from_le_bytes([self.head[0], self.head[1], self.head[2], self.head[3]]);
        let len = u32::from_le_bytes([self.head[4], self.head[5], self.head[6], self.head[7]]);
        self.tags += 1;
        if self.tags == 1 && tag != TAG_HEADER {
            return Err(GblError::NotGbl);
        }
        let ok_len = match tag {
            TAG_HEADER => {
                if self.tags != 1 {
                    return Err(GblError::NotGbl);
                }
                len == 8
            }
            TAG_APPLICATION => {
                if self.info.app.is_some() {
                    return Err(GblError::DuplicateApplication);
                }
                len as usize >= APP_DATA_LEN
            }
            TAG_PROG | TAG_ERASEPROG => len >= 4,
            TAG_END => {
                self.crc_at_end = Some(self.crc.value());
                len == 4
            }
            TAG_METADATA | TAG_VERSION_DEPENDENCY => true,
            TAG_SIGNATURE_ECDSA_P256 | TAG_CERTIFICATE_ECDSA_P256 => {
                self.info.signed = true;
                true
            }
            TAG_PROG_LZ4 | TAG_PROG_LZMA => return Err(GblError::Compressed),
            TAG_BOOTLOADER => return Err(GblError::BootloaderUpgrade),
            TAG_SE_UPGRADE => return Err(GblError::SeUpgrade),
            TAG_ENC_HEADER | TAG_ENC_INIT | TAG_ENC_GBL_DATA => return Err(GblError::Encrypted),
            other => return Err(GblError::UnknownTag(other)),
        };
        if !ok_len || len > self.policy.max_file {
            return Err(GblError::BadLength { tag, len });
        }
        self.body_len = 0;
        self.stage = Stage::Body {
            tag,
            len,
            left: len,
        };
        if len == 0 {
            self.tag_end(tag, len)?;
        }
        Ok(())
    }

    fn tag_end(&mut self, tag: u32, len: u32) -> Result<(), GblError> {
        let b = &self.body[..self.body_len];
        let word = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        self.stage = Stage::Tag;
        match tag {
            TAG_HEADER => {
                let (version, ty) = (word(0), word(4));
                if version & 0xFF00_0000 != VERSION_MAJOR {
                    return Err(GblError::Version(version));
                }
                if ty & TYPE_ENCRYPTED != 0 {
                    return Err(GblError::Encrypted);
                }
                self.info.gbl_type = ty;
                self.info.signed |= ty & TYPE_SIGNED != 0;
            }
            TAG_APPLICATION => {
                let mut product_id = [0u8; 16];
                product_id.copy_from_slice(&b[12..28]);
                let app = ApplicationData {
                    app_type: word(0),
                    version: word(4),
                    capabilities: word(8),
                    product_id,
                };
                // Type 0 (none given): recovery images such as Nerivec's
                // NVM3 clear.
                if app.app_type != 0 && app.app_type & APP_TYPE_ZIGBEE == 0 {
                    return Err(GblError::NotZigbee(app.app_type));
                }
                self.info.app = Some(app);
            }
            TAG_PROG | TAG_ERASEPROG => {
                let address = word(0);
                let end = u64::from(address) + u64::from(len - 4);
                if address < self.policy.app_start {
                    return Err(GblError::BelowApplication { address });
                }
                if end > u64::from(self.policy.flash_end) {
                    return Err(GblError::BeyondFlash {
                        end: end.min(u64::from(u32::MAX)) as u32,
                    });
                }
                self.info.prog_tags += 1;
                self.info.prog_bytes += len - 4;
                self.info.prog_start = self.info.prog_start.min(address);
                self.info.prog_end = self.info.prog_end.max(end as u32);
            }
            TAG_METADATA => {
                self.info.metadata += 1;
                // A ZRHP record is short; longer metadata is someone else's.
                if len as usize <= self.body.len()
                    && let Some(z) = ZrhpRecord::decode(b)?
                {
                    if self.info.zrhp.is_some() {
                        return Err(GblError::ZrhpRecord);
                    }
                    self.info.zrhp = Some(z);
                }
            }
            TAG_VERSION_DEPENDENCY => self.info.version_dependency = true,
            TAG_END => {
                let stored = word(0);
                let computed = self.crc_at_end.unwrap_or(0);
                if stored != computed {
                    return Err(GblError::Crc { stored, computed });
                }
                self.info.crc32 = stored;
                self.info.gbl_len = self.offset;
                self.stage = Stage::Padding;
            }
            _ => {}
        }
        Ok(())
    }

    /// The whole file was fed: the image, or why it is refused.
    pub fn finish(&self) -> Result<GblInfo, GblError> {
        if let Some(e) = self.error {
            return Err(e);
        }
        if self.stage != Stage::Padding {
            return Err(if self.tags == 0 {
                GblError::NotGbl
            } else {
                GblError::Truncated
            });
        }
        let mut info = self.info;
        if info.prog_tags == 0 {
            return Err(GblError::NoProgram);
        }
        if info.zrhp.is_some() && info.has_application() && info.prog_end > self.policy.zrhp_app_end
        {
            return Err(GblError::BeyondZrhpWindow { end: info.prog_end });
        }
        info.size = self.offset;
        Ok(info)
    }
}

/// How many bytes of a tag's body the parser keeps.
fn needed(tag: u32, len: u32) -> usize {
    match tag {
        TAG_HEADER => 8,
        TAG_APPLICATION => APP_DATA_LEN,
        TAG_PROG | TAG_ERASEPROG | TAG_END => 4,
        TAG_METADATA => len.min(64) as usize,
        _ => 0,
    }
}

/// Validates a whole file.
pub fn parse(data: &[u8], policy: Policy) -> Result<GblInfo, GblError> {
    let mut p = GblParser::new(policy);
    p.feed(data)?;
    p.finish()
}
