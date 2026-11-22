//! Zigbee OTA upgrade file format (ZCL r8 11.4): the header and the
//! sub-element chain.
//!
//! ```text
//! 0  u32 magic 0x0BEEF11E      20 [u8; 32] header string
//! 4  u16 header version 0x0100 52 u32 total image size (header included)
//! 6  u16 header length         56 [u8 security credential version]   field control bit 0
//! 8  u16 field control            [EUI64 upgrade file destination]   bit 1
//! 10 u16 manufacturer code        [u16 min, u16 max hardware version] bit 2
//! 12 u16 image type            then sub-elements {u16 tag, u32 length, data}
//! 14 u32 file version
//! 18 u16 ZigBee stack version
//! ```
//!
//! Everything is checked in a stream ([`Walker`]): the store never holds a
//! whole image in RAM.

/// Upgrade file identifier.
pub const MAGIC: u32 = 0x0BEE_F11E;
/// The only header version defined.
pub const HEADER_VERSION: u16 = 0x0100;
/// Header length without the optional fields.
pub const BASE_HEADER_LEN: usize = 56;
/// Longest header (all optional fields).
pub const MAX_HEADER_LEN: usize = BASE_HEADER_LEN + 1 + 8 + 4;

/// Field control bits.
pub mod fc {
    /// Security credential version present.
    pub const SECURITY_CREDENTIAL: u16 = 1 << 0;
    /// Device specific file (upgrade file destination present).
    pub const DEVICE_SPECIFIC: u16 = 1 << 1;
    /// Minimum and maximum hardware version present.
    pub const HARDWARE_VERSIONS: u16 = 1 << 2;
}

/// Sub-element tags (ZCL r8 11.4.4).
pub mod tag {
    /// Upgrade image.
    pub const UPGRADE_IMAGE: u16 = 0x0000;
    /// ECDSA signature (crypto suite 1).
    pub const ECDSA_SIGNATURE_1: u16 = 0x0001;
    /// ECDSA signing certificate (crypto suite 1).
    pub const ECDSA_CERT_1: u16 = 0x0002;
    /// Image integrity code.
    pub const INTEGRITY_CODE: u16 = 0x0003;
    /// Picture data.
    pub const PICTURE: u16 = 0x0004;
    /// ECDSA signature (crypto suite 2).
    pub const ECDSA_SIGNATURE_2: u16 = 0x0005;
    /// ECDSA signing certificate (crypto suite 2).
    pub const ECDSA_CERT_2: u16 = 0x0006;
}

/// What is wrong with a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileError {
    /// Fewer bytes than the header needs.
    Short(usize),
    /// Not an OTA upgrade file.
    BadMagic(u32),
    /// Unknown header version.
    BadHeaderVersion(u16),
    /// The header length disagrees with the field control.
    BadHeaderLength(u16),
    /// The header's total image size is not the file size.
    SizeMismatch {
        /// From the header.
        header: u32,
        /// Actual.
        file: u32,
    },
    /// A sub-element runs past the end of the file.
    SubElementOverrun {
        /// Offset of its tag.
        offset: u32,
        /// Its tag.
        tag: u16,
        /// Its length.
        len: u32,
    },
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Short(n) => write!(f, "the OTA header needs {n} bytes"),
            Self::BadMagic(m) => write!(
                f,
                "not a Zigbee OTA file (identifier 0x{m:08X}, expected 0x0BEEF11E)"
            ),
            Self::BadHeaderVersion(v) => write!(f, "unsupported OTA header version 0x{v:04X}"),
            Self::BadHeaderLength(l) => {
                write!(f, "OTA header length {l} does not match its field control")
            }
            Self::SizeMismatch { header, file } => {
                write!(
                    f,
                    "the header's total image size is {header} B, the file has {file} B"
                )
            }
            Self::SubElementOverrun { offset, tag, len } => {
                write!(
                    f,
                    "sub-element 0x{tag:04X} at {offset} ({len} B) runs past the end of the file"
                )
            }
        }
    }
}

impl std::error::Error for FileError {}

/// The OTA header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// Header length.
    pub header_len: u16,
    /// Field control.
    pub field_control: u16,
    /// Manufacturer code.
    pub manufacturer: u16,
    /// Image type.
    pub image_type: u16,
    /// File version.
    pub file_version: u32,
    /// ZigBee stack version.
    pub stack_version: u16,
    /// Header string (NUL padded).
    pub header_string: [u8; 32],
    /// Total image size, header included.
    pub total_size: u32,
    /// Security credential version.
    pub security_credential: Option<u8>,
    /// Upgrade file destination (device specific file).
    pub destination: Option<u64>,
    /// Minimum and maximum hardware version.
    pub hardware: Option<(u16, u16)>,
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

impl Header {
    /// Parses the header at the start of `b`; [`FileError::Short`] tells
    /// how many bytes it needs when `b` is too short.
    pub fn parse(b: &[u8]) -> Result<Header, FileError> {
        if b.len() < 10 {
            return Err(FileError::Short(BASE_HEADER_LEN));
        }
        let magic = u32_at(b, 0);
        if magic != MAGIC {
            return Err(FileError::BadMagic(magic));
        }
        let version = u16_at(b, 4);
        if version != HEADER_VERSION {
            return Err(FileError::BadHeaderVersion(version));
        }
        let header_len = u16_at(b, 6);
        let field_control = u16_at(b, 8);
        let mut need = BASE_HEADER_LEN;
        if field_control & fc::SECURITY_CREDENTIAL != 0 {
            need += 1;
        }
        if field_control & fc::DEVICE_SPECIFIC != 0 {
            need += 8;
        }
        if field_control & fc::HARDWARE_VERSIONS != 0 {
            need += 4;
        }
        // Longer headers (fields of a later revision) are skipped.
        if usize::from(header_len) < need {
            return Err(FileError::BadHeaderLength(header_len));
        }
        if b.len() < need {
            return Err(FileError::Short(need));
        }
        let mut at = BASE_HEADER_LEN;
        let mut take = |n: usize| {
            let s = at;
            at += n;
            s
        };
        let security_credential =
            (field_control & fc::SECURITY_CREDENTIAL != 0).then(|| b[take(1)]);
        let destination = (field_control & fc::DEVICE_SPECIFIC != 0).then(|| {
            let i = take(8);
            u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
        });
        let hardware = (field_control & fc::HARDWARE_VERSIONS != 0).then(|| {
            let i = take(4);
            (u16_at(b, i), u16_at(b, i + 2))
        });
        let total_size = u32_at(b, 52);
        if total_size < u32::from(header_len) {
            return Err(FileError::SizeMismatch {
                header: total_size,
                file: u32::from(header_len),
            });
        }
        Ok(Header {
            header_len,
            field_control,
            manufacturer: u16_at(b, 10),
            image_type: u16_at(b, 12),
            file_version: u32_at(b, 14),
            stack_version: u16_at(b, 18),
            header_string: b[20..52].try_into().unwrap(),
            total_size,
            security_credential,
            destination,
            hardware,
        })
    }

    /// The header string without padding (lossy UTF-8).
    pub fn label(&self) -> String {
        let end = self
            .header_string
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(32);
        String::from_utf8_lossy(&self.header_string[..end])
            .trim()
            .to_string()
    }

    /// Whether a device with hardware version `hw` may take this file: the
    /// range applies only when both are known (ZCL r8 11.4.2.13).
    pub fn hardware_matches(&self, hw: Option<u16>) -> bool {
        match (self.hardware, hw) {
            (Some((min, max)), Some(hw)) => (min..=max).contains(&hw),
            _ => true,
        }
    }

    /// Encodes the header (tests and tools).
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(usize::from(self.header_len));
        b.extend(MAGIC.to_le_bytes());
        b.extend(HEADER_VERSION.to_le_bytes());
        b.extend(self.header_len.to_le_bytes());
        b.extend(self.field_control.to_le_bytes());
        b.extend(self.manufacturer.to_le_bytes());
        b.extend(self.image_type.to_le_bytes());
        b.extend(self.file_version.to_le_bytes());
        b.extend(self.stack_version.to_le_bytes());
        b.extend(self.header_string);
        b.extend(self.total_size.to_le_bytes());
        if let Some(s) = self.security_credential {
            b.push(s);
        }
        if let Some(d) = self.destination {
            b.extend(d.to_le_bytes());
        }
        if let Some((min, max)) = self.hardware {
            b.extend(min.to_le_bytes());
            b.extend(max.to_le_bytes());
        }
        b.resize(usize::from(self.header_len), 0);
        b
    }
}

/// Builds an OTA file with one upgrade-image sub-element (tests, the
/// simulated client and documentation examples).
pub fn build(
    manufacturer: u16,
    image_type: u16,
    file_version: u32,
    label: &str,
    image: &[u8],
    hardware: Option<(u16, u16)>,
) -> Vec<u8> {
    let header_len = BASE_HEADER_LEN + if hardware.is_some() { 4 } else { 0 };
    let total = header_len + 6 + image.len();
    let mut header_string = [0u8; 32];
    let n = label.len().min(32);
    header_string[..n].copy_from_slice(&label.as_bytes()[..n]);
    let h = Header {
        header_len: header_len as u16,
        field_control: if hardware.is_some() {
            fc::HARDWARE_VERSIONS
        } else {
            0
        },
        manufacturer,
        image_type,
        file_version,
        stack_version: 0x0002,
        header_string,
        total_size: total as u32,
        security_credential: None,
        destination: None,
        hardware,
    };
    let mut out = h.encode();
    out.extend(tag::UPGRADE_IMAGE.to_le_bytes());
    out.extend((image.len() as u32).to_le_bytes());
    out.extend(image);
    out
}

/// One sub-element: tag, offset of its data, length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubElement {
    /// Tag.
    pub tag: u16,
    /// Offset of the data in the file.
    pub offset: u32,
    /// Data length.
    pub len: u32,
}

/// Streaming check of a whole file fed in order: header, sizes and the
/// sub-element chain. Keeps at most [`MAX_HEADER_LEN`] bytes.
#[derive(Debug, Clone)]
pub struct Walker {
    size: u32,
    pos: u32,
    head: Vec<u8>,
    header: Option<Header>,
    /// Offset of the next sub-element header.
    next: u32,
    sub: [u8; 6],
    sub_fill: usize,
    /// Sub-elements found.
    pub elements: Vec<SubElement>,
    /// Non-fatal findings (trailing bytes, no upgrade image).
    pub warnings: Vec<String>,
    error: Option<FileError>,
}

/// Most sub-elements recorded (a file with more is still accepted).
const MAX_ELEMENTS: usize = 16;

impl Walker {
    /// A check of a file of `size` bytes.
    pub fn new(size: u32) -> Self {
        Self {
            size,
            pos: 0,
            head: Vec::new(),
            header: None,
            next: 0,
            sub: [0; 6],
            sub_fill: 0,
            elements: Vec::new(),
            warnings: Vec::new(),
            error: None,
        }
    }

    /// The header, once enough bytes arrived.
    pub fn header(&self) -> Option<&Header> {
        self.header.as_ref()
    }

    /// The first error.
    pub fn error(&self) -> Option<&FileError> {
        self.error.as_ref()
    }

    /// Feeds the next bytes of the file.
    pub fn feed(&mut self, mut data: &[u8]) {
        if self.error.is_some() || data.is_empty() {
            return;
        }
        if self.header.is_none() {
            let take = MAX_HEADER_LEN
                .saturating_sub(self.head.len())
                .min(data.len());
            self.head.extend_from_slice(&data[..take]);
            self.pos += take as u32;
            data = &data[take..];
            match Header::parse(&self.head) {
                Ok(h) => {
                    if h.total_size != self.size {
                        self.error = Some(FileError::SizeMismatch {
                            header: h.total_size,
                            file: self.size,
                        });
                        return;
                    }
                    // The chain starts after the header; a header longer
                    // than the fields we know is skipped by `chain`.
                    let hl = usize::from(h.header_len);
                    self.next = u32::from(h.header_len);
                    self.header = Some(h);
                    let head = std::mem::take(&mut self.head);
                    if hl < head.len() {
                        self.pos = hl as u32;
                        self.chain(&head[hl..]);
                    }
                }
                Err(FileError::Short(_)) if self.head.len() < MAX_HEADER_LEN => return,
                Err(e) => {
                    self.error = Some(e);
                    return;
                }
            }
        }
        self.chain(data);
    }

    /// Walks the sub-element chain over `data` at `self.pos`.
    fn chain(&mut self, mut data: &[u8]) {
        while !data.is_empty() && self.error.is_none() {
            if self.pos < self.next {
                // Inside a sub-element's data.
                let skip = ((self.next - self.pos) as usize).min(data.len());
                self.pos += skip as u32;
                data = &data[skip..];
                continue;
            }
            let want = (6 - self.sub_fill).min(data.len());
            self.sub[self.sub_fill..self.sub_fill + want].copy_from_slice(&data[..want]);
            self.sub_fill += want;
            self.pos += want as u32;
            data = &data[want..];
            if self.sub_fill == 6 {
                self.sub_fill = 0;
                let at = self.pos - 6;
                let tag = u16_at(&self.sub, 0);
                let len = u32_at(&self.sub, 2);
                let end = u64::from(self.pos) + u64::from(len);
                if end > u64::from(self.size) {
                    self.error = Some(FileError::SubElementOverrun {
                        offset: at,
                        tag,
                        len,
                    });
                    return;
                }
                if self.elements.len() < MAX_ELEMENTS {
                    self.elements.push(SubElement {
                        tag,
                        offset: self.pos,
                        len,
                    });
                }
                self.next = end as u32;
            }
        }
    }

    /// Ends the check after all `size` bytes were fed.
    pub fn finish(mut self) -> Result<Checked, FileError> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let Some(header) = self.header.take() else {
            return Err(FileError::Short(
                MAX_HEADER_LEN.min(self.size as usize).max(BASE_HEADER_LEN),
            ));
        };
        if self.sub_fill != 0 {
            self.warnings.push(format!(
                "{} trailing bytes after the last sub-element",
                self.sub_fill
            ));
        }
        if !self.elements.iter().any(|e| e.tag == tag::UPGRADE_IMAGE) {
            self.warnings
                .push("no upgrade image sub-element (tag 0x0000)".into());
        }
        Ok(Checked {
            header,
            elements: self.elements,
            warnings: self.warnings,
        })
    }
}

/// A checked file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    /// Header.
    pub header: Header,
    /// Sub-elements.
    pub elements: Vec<SubElement>,
    /// Non-fatal findings.
    pub warnings: Vec<String>,
}

/// Checks a whole file in memory (tests, tools).
pub fn check(file: &[u8]) -> Result<Checked, FileError> {
    let mut w = Walker::new(file.len() as u32);
    w.feed(file);
    w.finish()
}

/// Offset of the upgrade file identifier in the first bytes of `data`:
/// some vendors ship the OTA file behind a container header. Clients cut
/// such a prefix before uploading.
pub fn find_magic(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|w| w == MAGIC.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_and_parses_a_file() {
        let f = build(
            0x6D6B,
            0x0102,
            0x0002_0001,
            "zigbee-rs C6 sensor",
            &[0xAB; 300],
            Some((1, 3)),
        );
        let c = check(&f).unwrap();
        assert_eq!(c.header.manufacturer, 0x6D6B);
        assert_eq!(c.header.image_type, 0x0102);
        assert_eq!(c.header.file_version, 0x0002_0001);
        assert_eq!(c.header.hardware, Some((1, 3)));
        assert_eq!(c.header.label(), "zigbee-rs C6 sensor");
        assert_eq!(c.header.total_size as usize, f.len());
        assert_eq!(
            c.elements,
            vec![SubElement {
                tag: 0,
                offset: 66,
                len: 300
            }]
        );
        assert!(c.warnings.is_empty());
        assert!(c.header.hardware_matches(Some(2)));
        assert!(!c.header.hardware_matches(Some(4)));
        assert!(c.header.hardware_matches(None));
    }

    #[test]
    fn streaming_in_any_chunks_gives_the_same_result() {
        let mut f = build(0x1037, 0x0001, 7, "x", &[1; 100], None);
        // A second sub-element (manufacturer specific).
        f.extend(0xF000u16.to_le_bytes());
        f.extend(5u32.to_le_bytes());
        f.extend([9; 5]);
        let n = f.len() as u32;
        f[52..56].copy_from_slice(&n.to_le_bytes());
        let whole = check(&f).unwrap();
        assert_eq!(whole.elements.len(), 2);
        for chunk in [1, 2, 3, 5, 7, 56, 57, 64, 1000] {
            let mut w = Walker::new(n);
            for c in f.chunks(chunk) {
                w.feed(c);
            }
            assert_eq!(w.finish().unwrap(), whole, "chunk {chunk}");
        }
    }

    #[test]
    fn rejects_broken_files() {
        let f = build(1, 2, 3, "", &[0; 10], None);
        let mut bad = f.clone();
        bad[0] = 0;
        assert!(matches!(check(&bad), Err(FileError::BadMagic(_))));
        let mut bad = f.clone();
        bad[4] = 0x02;
        assert!(matches!(check(&bad), Err(FileError::BadHeaderVersion(_))));
        let mut bad = f.clone();
        bad[8] = fc::HARDWARE_VERSIONS as u8; // claims fields the length lacks
        assert!(matches!(check(&bad), Err(FileError::BadHeaderLength(56))));
        let mut bad = f.clone();
        bad.push(0);
        assert!(matches!(check(&bad), Err(FileError::SizeMismatch { .. })));
        let mut bad = f.clone();
        let l = bad.len();
        bad[58] = 200; // sub-element length past the end
        assert!(
            matches!(
                check(&bad),
                Err(FileError::SubElementOverrun { tag: 0, .. })
            ),
            "{l}"
        );
        assert!(matches!(
            check(&f[..20]),
            Err(FileError::SizeMismatch { .. }) | Err(FileError::Short(_))
        ));
    }

    #[test]
    fn device_specific_and_long_headers() {
        let mut h = Header::parse(&build(1, 2, 3, "", &[], None)).unwrap();
        h.field_control = fc::DEVICE_SPECIFIC | fc::SECURITY_CREDENTIAL;
        h.destination = Some(0x0011_2233_4455_6677);
        h.security_credential = Some(1);
        h.header_len = 80; // longer than any field set defines
        h.total_size = 80 + 6 + 4;
        let mut f = h.encode();
        f.extend(0u16.to_le_bytes());
        f.extend(4u32.to_le_bytes());
        f.extend([7; 4]);
        let c = check(&f).unwrap();
        assert_eq!(c.header.destination, Some(0x0011_2233_4455_6677));
        assert_eq!(c.header.security_credential, Some(1));
        assert_eq!(
            c.elements,
            vec![SubElement {
                tag: 0,
                offset: 86,
                len: 4
            }]
        );
        let mut w = Walker::new(f.len() as u32);
        for b in &f {
            w.feed(std::slice::from_ref(b));
        }
        assert_eq!(w.finish().unwrap(), c);
    }

    #[test]
    fn trailing_bytes_are_a_warning() {
        let mut f = build(1, 2, 3, "", &[0; 4], None);
        f.extend([0, 0]);
        let n = f.len() as u32;
        f[52..56].copy_from_slice(&n.to_le_bytes());
        let c = check(&f).unwrap();
        assert_eq!(c.warnings.len(), 1);
        assert_eq!(find_magic(&[[0u8; 7].as_slice(), &f].concat()), Some(7));
    }
}
