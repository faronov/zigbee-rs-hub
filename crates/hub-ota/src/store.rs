//! OTA image store on a raw flash region (on the 8 MB layout: the inactive
//! app slot, see `docs/ota.md`).
//!
//! ```text
//! sector 0, 1   index slots A/B: {magic "ZOTA", format, n, generation, checksum} + n entries
//! sector 2..    image data, one contiguous run of sectors per image
//! ```
//!
//! Index updates alternate between the two slots with an increasing
//! generation; the entries are written before the slot header, so a power
//! cut leaves the previous slot valid. Uploads stream into flash chunk by
//! chunk (no image is ever held in RAM); every [`CHECKPOINT`] bytes the
//! index records how far the upload got, so an upload interrupted by a hub
//! restart continues from the last checkpoint. A commit reads the image
//! back from flash: SHA-256 and the full format check run over what was
//! actually stored.
//!
//! A region that holds neither a valid index nor erased flash is
//! *foreign* (another area's data): the store refuses to write there until
//! [`ImageStore::format`] — unless it was opened as *expendable*
//! ([`ImageStore::open_expendable`], the inactive app slot: whatever is
//! there, e.g. the previous app after a hub update, may be overwritten).
//! At open every ready image's OTA header is read back from flash and
//! checked against its entry; an image that fails is dropped, so nothing
//! is served from a slot whose content changed underneath.

use std::io;

use hub_ncp::store::Flash;
use sha2::{Digest, Sha256};

use crate::file::{FileError, Header, MAX_HEADER_LEN, Walker};

/// Index sectors (two slots).
const INDEX_SECTORS: u32 = 2;
/// Most images.
pub const MAX_IMAGES: usize = 16;
/// Upload progress is recorded in the index every this many bytes.
pub const CHECKPOINT: u32 = 64 * 1024;
const MAGIC: &[u8; 4] = b"ZOTA";
const FORMAT: u8 = 1;
const HEAD_LEN: usize = 16;
const ENTRY_LEN: usize = 112;
const NAME_LEN: usize = 48;
/// Read-back buffer of a commit.
const READ_BUF: usize = 1024;

/// State of a stored image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageState {
    /// Being uploaded.
    Uploading,
    /// Checked and ready to serve.
    Ready,
}

/// One image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Store-assigned ID (stable while the image exists).
    pub id: u16,
    /// State.
    pub state: ImageState,
    first: u16,
    sectors: u16,
    /// File size.
    pub size: u32,
    /// Bytes stored (an upload's last checkpoint, or `size`).
    pub written: u32,
    /// Upload start (Unix s, 0 unknown).
    pub added: u32,
    /// File name given by the uploader.
    pub name: String,
    /// SHA-256 of the file (expected while uploading, when given).
    pub sha256: Option<[u8; 32]>,
    /// Offered only to devices whose current file version is at least this.
    pub min_file_version: Option<u32>,
    /// Offered only to devices whose current file version is at most this.
    pub max_file_version: Option<u32>,
    /// The OTA header (ready images, and uploads once it arrived).
    pub header: Option<Header>,
    /// Non-fatal format findings of the commit (not persisted).
    pub warnings: Vec<String>,
}

impl Image {
    /// `(manufacturer, image type, file version)` of a ready image.
    pub fn ident(&self) -> Option<(u16, u16, u32)> {
        let h = self.header.as_ref()?;
        Some((h.manufacturer, h.image_type, h.file_version))
    }

    /// Bytes of flash the image occupies.
    pub fn footprint(&self, sector: u32) -> u32 {
        u32::from(self.sectors) * sector
    }
}

/// What an upload is told about the file up front.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UploadMeta {
    /// File name.
    pub name: String,
    /// Expected SHA-256.
    pub sha256: Option<[u8; 32]>,
    /// See [`Image::min_file_version`].
    pub min_file_version: Option<u32>,
    /// See [`Image::max_file_version`].
    pub max_file_version: Option<u32>,
    /// A stored image of the same identity is replaced at the commit;
    /// without it the upload fails with [`StoreError::Exists`] as soon as
    /// its header shows the duplicate.
    pub replace: bool,
}

/// Store errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// Not enough contiguous free flash.
    NoSpace {
        /// Bytes needed (whole sectors).
        need: u32,
        /// Largest free run.
        largest: u32,
        /// Free in total.
        free: u32,
    },
    /// [`MAX_IMAGES`] reached.
    TooMany,
    /// No such image or upload.
    NotFound,
    /// A chunk not at the upload's current length.
    Offset {
        /// Where the next chunk must start.
        expected: u32,
    },
    /// Chunk past the declared size, or commit before all bytes arrived.
    Size,
    /// Not an acceptable OTA file.
    File(FileError),
    /// SHA-256 of the stored bytes differs from the expected one.
    Hash,
    /// An image with this manufacturer, image type, version (and
    /// destination) is stored already.
    Exists(u16),
    /// The region holds other data.
    Foreign,
    /// Flash error.
    Io(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSpace {
                need,
                largest,
                free,
            } => {
                write!(
                    f,
                    "no space: {need} B needed, largest free run {largest} B, {free} B free"
                )
            }
            Self::TooMany => write!(f, "at most {MAX_IMAGES} images"),
            Self::NotFound => write!(f, "no such image or upload"),
            Self::Offset { expected } => write!(f, "the next chunk starts at {expected}"),
            Self::Size => write!(f, "size mismatch"),
            Self::File(e) => write!(f, "{e}"),
            Self::Hash => write!(f, "SHA-256 of the stored image differs"),
            Self::Exists(id) => write!(f, "the same image is stored as {id}"),
            Self::Foreign => write!(f, "the OTA area holds other data (format it to use it)"),
            Self::Io(e) => write!(f, "flash: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// RAM state of the running upload.
#[derive(Debug)]
struct Up {
    id: u16,
    written: u32,
    /// Sectors of the image (relative) erased since this upload (re)started.
    erased: u32,
    /// The first bytes until the header could be checked.
    head: Option<Vec<u8>>,
    /// [`UploadMeta::replace`] of the `begin` this upload (re)started with.
    replace: bool,
}

/// The store.
pub struct ImageStore {
    flash: Box<dyn Flash>,
    sector: u32,
    sectors: u32,
    generation: u32,
    /// Slot of the current index (`None`: none written yet).
    slot: Option<u32>,
    images: Vec<Image>,
    next_id: u16,
    up: Option<Up>,
    foreign: bool,
}

impl std::fmt::Debug for ImageStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageStore")
            .field("sectors", &self.sectors)
            .field("images", &self.images.len())
            .finish()
    }
}

fn checksum(generation: u32, n: u8, entries: &[u8]) -> [u8; 4] {
    let mut h = Sha256::new();
    h.update(generation.to_le_bytes());
    h.update([n]);
    h.update(entries);
    h.finalize()[..4].try_into().unwrap()
}

impl ImageStore {
    /// Opens the store in `flash` (at least 3 sectors).
    pub fn open(flash: Box<dyn Flash>) -> io::Result<Self> {
        Self::open_with(flash, false)
    }

    /// Opens a region whose foreign content may be overwritten (an empty
    /// store is formatted with the first upload).
    pub fn open_expendable(flash: Box<dyn Flash>) -> io::Result<Self> {
        Self::open_with(flash, true)
    }

    fn open_with(mut flash: Box<dyn Flash>, expendable: bool) -> io::Result<Self> {
        let sector = flash.sector_size();
        let sectors = flash.sectors();
        if sectors <= INDEX_SECTORS || sectors > u32::from(u16::MAX) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "OTA area size"));
        }
        let mut best: Option<(u32, u32, Vec<u8>)> = None;
        let mut blank_index = true;
        for slot in 0..INDEX_SECTORS {
            let mut head = [0u8; HEAD_LEN];
            flash.read(slot * sector, &mut head)?;
            if head.iter().any(|&b| b != 0xFF) {
                blank_index = false;
            }
            if &head[..4] != MAGIC || head[4] != FORMAT || usize::from(head[5]) > MAX_IMAGES {
                continue;
            }
            let n = head[5];
            let generation = u32::from_le_bytes(head[8..12].try_into().unwrap());
            let mut entries = vec![0u8; usize::from(n) * ENTRY_LEN];
            flash.read(slot * sector + HEAD_LEN as u32, &mut entries)?;
            if checksum(generation, n, &entries) != head[12..16] {
                continue;
            }
            if best.as_ref().is_none_or(|b| generation > b.0) {
                best = Some((generation, slot, entries));
            }
        }
        let mut s = Self {
            flash,
            sector,
            sectors,
            generation: 0,
            slot: None,
            images: Vec::new(),
            next_id: 1,
            up: None,
            foreign: false,
        };
        match best {
            Some((generation, slot, entries)) => {
                s.generation = generation;
                s.slot = Some(slot);
                for e in entries.chunks(ENTRY_LEN) {
                    if let Some(img) = s.decode(e) {
                        s.next_id = s.next_id.max(img.id.wrapping_add(1).max(1));
                        s.insert(img);
                    }
                }
                for i in 0..s.images.len() {
                    if s.images[i].state == ImageState::Ready || s.images[i].written > 0 {
                        let mut b = [0u8; MAX_HEADER_LEN];
                        let n = (s.images[i].size as usize).min(MAX_HEADER_LEN);
                        let at = s.data_offset(&s.images[i], 0);
                        s.flash.read(at, &mut b[..n])?;
                        s.images[i].header = Header::parse(&b[..n])
                            .ok()
                            .filter(|h| h.total_size == s.images[i].size);
                    }
                }
                // A ready image whose header no longer reads back is not served.
                let before = s.images.len();
                s.images
                    .retain(|i| i.state != ImageState::Ready || i.header.is_some());
                if s.images.len() != before {
                    log::warn!(
                        "ota store: {} images dropped (header check failed)",
                        before - s.images.len()
                    );
                    if s.save().is_err() {
                        log::warn!("ota store: index not rewritten");
                    }
                }
            }
            None if expendable => {
                // Nothing of ours: starts empty, the index is written with
                // the first upload.
            }
            None => {
                // No index: a fresh area is erased; anything else is not ours.
                s.foreign = !blank_index || !s.region_blank()?;
            }
        }
        Ok(s)
    }

    fn region_blank(&mut self) -> io::Result<bool> {
        let mut buf = vec![0u8; READ_BUF];
        let total = self.sectors * self.sector;
        let mut at = INDEX_SECTORS * self.sector;
        while at < total {
            let n = (total - at).min(READ_BUF as u32) as usize;
            self.flash.read(at, &mut buf[..n])?;
            if buf[..n].iter().any(|&b| b != 0xFF) {
                return Ok(false);
            }
            at += n as u32;
        }
        Ok(true)
    }

    /// The region holds other data (see the module docs).
    pub fn is_foreign(&self) -> bool {
        self.foreign
    }

    /// Erases the index and forgets every image (also takes over a
    /// foreign region).
    pub fn format(&mut self) -> Result<(), StoreError> {
        for slot in 0..INDEX_SECTORS {
            self.flash.erase(slot)?;
        }
        self.images.clear();
        self.up = None;
        self.slot = None;
        self.foreign = false;
        self.save()
    }

    fn decode(&self, e: &[u8]) -> Option<Image> {
        let u16_ = |i: usize| u16::from_le_bytes([e[i], e[i + 1]]);
        let u32_ = |i: usize| u32::from_le_bytes(e[i..i + 4].try_into().unwrap());
        let state = match e[2] {
            1 => ImageState::Uploading,
            2 => ImageState::Ready,
            _ => return None,
        };
        let flags = e[3];
        let first = u16_(4);
        let sectors = u16_(6);
        if u32::from(first) < INDEX_SECTORS || u32::from(first) + u32::from(sectors) > self.sectors
        {
            return None;
        }
        let name_end = e[60..60 + NAME_LEN]
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(NAME_LEN);
        Some(Image {
            id: u16_(0),
            state,
            first,
            sectors,
            size: u32_(8),
            written: u32_(12),
            added: u32_(16),
            min_file_version: (flags & 2 != 0).then(|| u32_(20)),
            max_file_version: (flags & 4 != 0).then(|| u32_(24)),
            sha256: (flags & 1 != 0).then(|| e[28..60].try_into().unwrap()),
            name: String::from_utf8_lossy(&e[60..60 + name_end]).into_owned(),
            header: None,
            warnings: Vec::new(),
        })
    }

    fn encode(img: &Image, out: &mut Vec<u8>) {
        let start = out.len();
        out.resize(start + ENTRY_LEN, 0);
        let e = &mut out[start..];
        e[0..2].copy_from_slice(&img.id.to_le_bytes());
        e[2] = match img.state {
            ImageState::Uploading => 1,
            ImageState::Ready => 2,
        };
        e[3] = u8::from(img.sha256.is_some())
            | u8::from(img.min_file_version.is_some()) << 1
            | u8::from(img.max_file_version.is_some()) << 2;
        e[4..6].copy_from_slice(&img.first.to_le_bytes());
        e[6..8].copy_from_slice(&img.sectors.to_le_bytes());
        e[8..12].copy_from_slice(&img.size.to_le_bytes());
        e[12..16].copy_from_slice(&img.written.to_le_bytes());
        e[16..20].copy_from_slice(&img.added.to_le_bytes());
        e[20..24].copy_from_slice(&img.min_file_version.unwrap_or(0).to_le_bytes());
        e[24..28].copy_from_slice(&img.max_file_version.unwrap_or(0).to_le_bytes());
        e[28..60].copy_from_slice(&img.sha256.unwrap_or([0; 32]));
        let name = img.name.as_bytes();
        let mut n = name.len().min(NAME_LEN);
        while !img.name.is_char_boundary(n) {
            n -= 1;
        }
        e[60..60 + n].copy_from_slice(&name[..n]);
    }

    /// Writes the index into the other slot.
    fn save(&mut self) -> Result<(), StoreError> {
        if self.foreign {
            return Err(StoreError::Foreign);
        }
        let slot = self.slot.map_or(0, |s| 1 - s);
        let generation = self.generation.wrapping_add(1);
        let mut entries = Vec::with_capacity(self.images.len() * ENTRY_LEN);
        for img in &self.images {
            Self::encode(img, &mut entries);
        }
        let n = self.images.len() as u8;
        let mut head = [0xFFu8; HEAD_LEN];
        head[..4].copy_from_slice(MAGIC);
        head[4] = FORMAT;
        head[5] = n;
        head[8..12].copy_from_slice(&generation.to_le_bytes());
        head[12..16].copy_from_slice(&checksum(generation, n, &entries));
        self.flash.erase(slot)?;
        if !entries.is_empty() {
            self.flash
                .write(slot * self.sector + HEAD_LEN as u32, &entries)?;
        }
        self.flash.write(slot * self.sector, &head)?;
        self.slot = Some(slot);
        self.generation = generation;
        Ok(())
    }

    fn data_offset(&self, img: &Image, offset: u32) -> u32 {
        u32::from(img.first) * self.sector + offset
    }

    /// Images, by flash position.
    pub fn images(&self) -> &[Image] {
        &self.images
    }

    /// One image.
    pub fn image(&self, id: u16) -> Option<&Image> {
        self.images.iter().find(|i| i.id == id)
    }

    /// The ready image with this identity.
    pub fn find(&self, manufacturer: u16, image_type: u16, file_version: u32) -> Option<&Image> {
        self.images.iter().find(|i| {
            i.state == ImageState::Ready
                && i.ident() == Some((manufacturer, image_type, file_version))
        })
    }

    /// Bytes for image data.
    pub fn capacity(&self) -> u32 {
        (self.sectors - INDEX_SECTORS) * self.sector
    }

    /// Bytes occupied by images (whole sectors).
    pub fn used(&self) -> u32 {
        self.images.iter().map(|i| i.footprint(self.sector)).sum()
    }

    /// Largest contiguous free run (the largest image that fits).
    pub fn largest_free(&self) -> u32 {
        self.free_runs().map(|(_, n)| n).max().unwrap_or(0) * self.sector
    }

    /// Free runs `(first sector, sectors)`.
    fn free_runs(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        let mut at = INDEX_SECTORS;
        let mut ends: Vec<(u32, u32)> = self
            .images
            .iter()
            .map(|i| (u32::from(i.first), u32::from(i.sectors)))
            .collect();
        ends.push((self.sectors, 0));
        ends.into_iter().filter_map(move |(first, n)| {
            let run = (at, first.saturating_sub(at));
            at = at.max(first + n);
            (run.1 > 0).then_some(run)
        })
    }

    /// The upload in progress.
    pub fn upload(&self) -> Option<&Image> {
        self.images
            .iter()
            .find(|i| i.state == ImageState::Uploading)
    }

    /// Starts (or resumes) an upload of `size` bytes; returns its ID and
    /// where the next chunk starts. Resumed: an unfinished upload of the
    /// same size and name (and SHA-256, when given). Any other unfinished
    /// upload is dropped.
    pub fn begin(
        &mut self,
        size: u32,
        meta: UploadMeta,
        now_s: u32,
    ) -> Result<(u16, u32), StoreError> {
        if self.foreign {
            return Err(StoreError::Foreign);
        }
        if size < crate::file::BASE_HEADER_LEN as u32 {
            return Err(StoreError::File(FileError::Short(
                crate::file::BASE_HEADER_LEN,
            )));
        }
        if let Some(u) = self.upload().cloned() {
            let same = u.size == size
                && u.name == meta.name
                && (meta.sha256.is_none() || meta.sha256 == u.sha256);
            if same {
                let written = self
                    .up
                    .as_ref()
                    .filter(|x| x.id == u.id)
                    .map_or(u.written, |x| x.written);
                if self.up.as_ref().is_none_or(|x| x.id != u.id) {
                    // After a restart: continue at the checkpoint, erasing
                    // its sector again.
                    self.up = Some(Up {
                        id: u.id,
                        written: u.written,
                        erased: u.written / self.sector,
                        head: None,
                        replace: meta.replace,
                    });
                }
                if let Some(up) = self.up.as_mut() {
                    up.replace = meta.replace;
                }
                return Ok((u.id, written));
            }
            self.remove(u.id)?;
        }
        if self.images.len() >= MAX_IMAGES {
            return Err(StoreError::TooMany);
        }
        let need = size.div_ceil(self.sector);
        let Some((first, _)) = self.free_runs().find(|&(_, n)| n >= need) else {
            return Err(StoreError::NoSpace {
                need: need * self.sector,
                largest: self.largest_free(),
                free: self.capacity() - self.used(),
            });
        };
        let id = self.alloc_id();
        let img = Image {
            id,
            state: ImageState::Uploading,
            first: first as u16,
            sectors: need as u16,
            size,
            written: 0,
            added: now_s,
            name: meta.name,
            sha256: meta.sha256,
            min_file_version: meta.min_file_version,
            max_file_version: meta.max_file_version,
            header: None,
            warnings: Vec::new(),
        };
        self.insert(img);
        self.save()?;
        self.up = Some(Up {
            id,
            written: 0,
            erased: 0,
            head: Some(Vec::new()),
            replace: meta.replace,
        });
        Ok((id, 0))
    }

    /// Inserts by flash position.
    fn insert(&mut self, img: Image) {
        let at = self
            .images
            .iter()
            .position(|i| i.first > img.first)
            .unwrap_or(self.images.len());
        self.images.insert(at, img);
    }

    fn alloc_id(&mut self) -> u16 {
        loop {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
            if self.image(id).is_none() {
                return id;
            }
        }
    }

    /// Stores the chunk `data` at `offset` of upload `id`; returns the
    /// bytes stored so far.
    pub fn write(&mut self, id: u16, offset: u32, data: &[u8]) -> Result<u32, StoreError> {
        let Some(idx) = self
            .images
            .iter()
            .position(|i| i.id == id && i.state == ImageState::Uploading)
        else {
            return Err(StoreError::NotFound);
        };
        if self.up.as_ref().is_none_or(|u| u.id != id) {
            let w = self.images[idx].written;
            self.up = Some(Up {
                id,
                written: w,
                erased: w / self.sector,
                head: None,
                replace: false,
            });
        }
        let img = self.images[idx].clone();
        let up = self.up.as_mut().unwrap();
        if offset != up.written {
            return Err(StoreError::Offset {
                expected: up.written,
            });
        }
        let end = u64::from(offset) + data.len() as u64;
        if end > u64::from(img.size) {
            return Err(StoreError::Size);
        }
        // The header is checked as soon as it is complete.
        if let Some(head) = up.head.as_mut() {
            let take = MAX_HEADER_LEN.saturating_sub(head.len()).min(data.len());
            head.extend_from_slice(&data[..take]);
            let complete = head.len() >= MAX_HEADER_LEN || end == u64::from(img.size);
            match Header::parse(head) {
                Ok(h) => {
                    up.head = None;
                    let replace = up.replace;
                    let err = if h.total_size != img.size {
                        Some(StoreError::File(FileError::SizeMismatch {
                            header: h.total_size,
                            file: img.size,
                        }))
                    } else if replace {
                        None
                    } else {
                        self.images
                            .iter()
                            .find(|i| {
                                i.state == ImageState::Ready
                                    && i.ident()
                                        == Some((h.manufacturer, h.image_type, h.file_version))
                                    && i.header.as_ref().map(|x| x.destination)
                                        == Some(h.destination)
                            })
                            .map(|i| StoreError::Exists(i.id))
                    };
                    if let Some(e) = err {
                        self.remove(id)?;
                        return Err(e);
                    }
                    self.images[idx].header = Some(h);
                }
                Err(FileError::Short(_)) if !complete => {}
                Err(e) => {
                    self.remove(id)?;
                    return Err(StoreError::File(e));
                }
            }
        }
        let up = self.up.as_mut().unwrap();
        let base = u32::from(img.first) * self.sector;
        let last_sector = (end as u32).div_ceil(self.sector);
        while up.erased < last_sector {
            self.flash.erase(u32::from(img.first) + up.erased)?;
            up.erased += 1;
        }
        if !data.is_empty() {
            self.flash.write(base + offset, data)?;
        }
        up.written = end as u32;
        let written = up.written;
        let checkpoint = written / CHECKPOINT * CHECKPOINT;
        if checkpoint > self.images[idx].written && written < img.size {
            self.images[idx].written = checkpoint;
            self.save()?;
        }
        Ok(written)
    }

    /// Finishes upload `id`: reads it back, checks SHA-256 (when given)
    /// and the format. A stored image of the same identity is deleted when
    /// the upload began with [`UploadMeta::replace`]; otherwise it makes
    /// the commit fail with [`StoreError::Exists`] (an upload resumed after
    /// a restart is checked here, its header was checked before). A failed
    /// check deletes the upload.
    pub fn commit(&mut self, id: u16) -> Result<&Image, StoreError> {
        let Some(idx) = self
            .images
            .iter()
            .position(|i| i.id == id && i.state == ImageState::Uploading)
        else {
            return Err(StoreError::NotFound);
        };
        let written = self
            .up
            .as_ref()
            .filter(|u| u.id == id)
            .map_or(self.images[idx].written, |u| u.written);
        let replace = self.up.as_ref().is_some_and(|u| u.id == id && u.replace);
        let img = self.images[idx].clone();
        if written != img.size {
            return Err(StoreError::Size);
        }
        let mut hash = Sha256::new();
        let mut walker = Walker::new(img.size);
        let mut buf = vec![0u8; READ_BUF];
        let mut at = 0;
        while at < img.size {
            let n = (img.size - at).min(READ_BUF as u32) as usize;
            self.flash.read(self.data_offset(&img, at), &mut buf[..n])?;
            hash.update(&buf[..n]);
            walker.feed(&buf[..n]);
            at += n as u32;
        }
        let sha: [u8; 32] = hash.finalize().into();
        let checked = match walker.finish() {
            Ok(c) => c,
            Err(e) => {
                self.remove(id)?;
                return Err(StoreError::File(e));
            }
        };
        if img.sha256.is_some_and(|s| s != sha) {
            self.remove(id)?;
            return Err(StoreError::Hash);
        }
        let h = &checked.header;
        if let Some(old) = self
            .images
            .iter()
            .find(|i| {
                i.id != id
                    && i.state == ImageState::Ready
                    && i.ident() == Some((h.manufacturer, h.image_type, h.file_version))
                    && i.header.as_ref().map(|x| x.destination) == Some(h.destination)
            })
            .map(|i| i.id)
        {
            if !replace {
                self.remove(id)?;
                return Err(StoreError::Exists(old));
            }
            self.images.retain(|i| i.id != old);
        }
        let idx = self.images.iter().position(|i| i.id == id).unwrap();
        let i = &mut self.images[idx];
        i.state = ImageState::Ready;
        i.written = i.size;
        i.sha256 = Some(sha);
        i.header = Some(checked.header);
        i.warnings = checked.warnings;
        self.up = None;
        self.save()?;
        Ok(self.image(id).unwrap())
    }

    fn remove(&mut self, id: u16) -> Result<(), StoreError> {
        let before = self.images.len();
        self.images.retain(|i| i.id != id);
        if self.up.as_ref().is_some_and(|u| u.id == id) {
            self.up = None;
        }
        if self.images.len() == before {
            return Err(StoreError::NotFound);
        }
        self.save()
    }

    /// Deletes an image or drops an upload (its flash is erased when
    /// reused).
    pub fn delete(&mut self, id: u16) -> Result<(), StoreError> {
        self.remove(id)
    }

    /// Ready images as server offers.
    pub fn offers(&self) -> Vec<crate::images::Offer> {
        self.images
            .iter()
            .filter(|i| i.state == ImageState::Ready)
            .filter_map(|i| {
                Some(crate::images::Offer {
                    id: u32::from(i.id),
                    header: i.header.clone()?,
                    size: i.size,
                    min_file_version: i.min_file_version,
                    max_file_version: i.max_file_version,
                    name: i.name.clone(),
                    streamed: false,
                })
            })
            .collect()
    }

    /// [`read`](Self::read) for the server (see [`crate::Images`]).
    pub fn read_offer(&mut self, id: u32, offset: u32, buf: &mut [u8]) -> crate::images::Fetch {
        match u16::try_from(id).ok().map(|id| self.read(id, offset, buf)) {
            Some(Ok(())) => crate::images::Fetch::Ready,
            _ => crate::images::Fetch::Gone,
        }
    }

    /// Reads image bytes (a block for a device).
    pub fn read(&mut self, id: u16, offset: u32, buf: &mut [u8]) -> Result<(), StoreError> {
        let img = self
            .images
            .iter()
            .find(|i| i.id == id && i.state == ImageState::Ready)
            .ok_or(StoreError::NotFound)?;
        if u64::from(offset) + buf.len() as u64 > u64::from(img.size) {
            return Err(StoreError::Size);
        }
        let at = self.data_offset(img, offset);
        self.flash.read(at, buf)?;
        Ok(())
    }

    /// Sector size.
    pub fn sector_size(&self) -> u32 {
        self.sector
    }
}

impl crate::images::Images for ImageStore {
    fn offers(&self) -> Vec<crate::images::Offer> {
        ImageStore::offers(self)
    }
    fn read(&mut self, id: u32, offset: u32, buf: &mut [u8], _now: u64) -> crate::images::Fetch {
        self.read_offer(id, offset, buf)
    }
}

#[cfg(test)]
mod tests {
    use hub_ncp::store::MemFlash;

    use super::*;
    use crate::file::build;

    /// A flash shared with the test, so a "restart" reopens the same bytes.
    #[derive(Clone)]
    struct Shared(std::sync::Arc<std::sync::Mutex<MemFlash>>);

    impl Flash for Shared {
        fn sector_size(&self) -> u32 {
            self.0.lock().unwrap().sector_size()
        }
        fn sectors(&self) -> u32 {
            self.0.lock().unwrap().sectors()
        }
        fn read(&mut self, o: u32, b: &mut [u8]) -> io::Result<()> {
            self.0.lock().unwrap().read(o, b)
        }
        fn write(&mut self, o: u32, d: &[u8]) -> io::Result<()> {
            self.0.lock().unwrap().write(o, d)
        }
        fn erase(&mut self, s: u32) -> io::Result<()> {
            self.0.lock().unwrap().erase(s)
        }
    }

    fn flash(sectors: u32) -> Shared {
        Shared(std::sync::Arc::new(std::sync::Mutex::new(MemFlash::new(
            sectors, 4096,
        ))))
    }

    fn upload(s: &mut ImageStore, f: &[u8], name: &str, chunk: usize) -> Result<u16, StoreError> {
        let sha: [u8; 32] = Sha256::digest(f).into();
        let (id, mut at) = s.begin(
            f.len() as u32,
            UploadMeta {
                name: name.into(),
                sha256: Some(sha),
                ..Default::default()
            },
            1,
        )?;
        while (at as usize) < f.len() {
            let end = (at as usize + chunk).min(f.len());
            at = s.write(id, at, &f[at as usize..end])?;
        }
        s.commit(id)?;
        Ok(id)
    }

    #[test]
    fn an_upload_begun_with_replace_replaces_the_stored_image() {
        let fl = flash(40);
        let mut s = ImageStore::open(Box::new(fl.clone())).unwrap();
        let f = build(0x6D6B, 2, 0x10, "a", &[7; 9000], None);
        let old = upload(&mut s, &f, "a.ota", 4096).unwrap();
        let begin = |s: &mut ImageStore, replace| {
            s.begin(
                f.len() as u32,
                UploadMeta {
                    name: "b.ota".into(),
                    replace,
                    ..Default::default()
                },
                2,
            )
            .unwrap()
            .0
        };
        // Without `replace` the header chunk already fails.
        let id = begin(&mut s, false);
        assert_eq!(s.write(id, 0, &f[..4096]), Err(StoreError::Exists(old)));
        assert!(s.image(id).is_none());
        // With it the upload goes through and takes the old image's place.
        let id = begin(&mut s, true);
        s.write(id, 0, &f[..4096]).unwrap();
        s.write(id, 4096, &f[4096..]).unwrap();
        assert_eq!(s.commit(id).unwrap().id, id);
        assert!(s.image(old).is_none());
        assert_eq!(s.find(0x6D6B, 2, 0x10).map(|i| i.id), Some(id));
        drop(s);
        let s = ImageStore::open(Box::new(fl)).unwrap();
        assert_eq!(
            s.images().iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![id]
        );
    }

    #[test]
    fn upload_read_delete_and_reopen() {
        let fl = flash(40);
        let mut s = ImageStore::open(Box::new(fl.clone())).unwrap();
        assert!(!s.is_foreign());
        assert_eq!(s.capacity(), 38 * 4096);
        let f = build(
            0x6D6B,
            2,
            0x10,
            "a",
            &(0..20_000u32).map(|i| i as u8).collect::<Vec<_>>(),
            None,
        );
        let id = upload(&mut s, &f, "a.ota", 3000).unwrap();
        let img = s.image(id).unwrap();
        assert_eq!(img.state, ImageState::Ready);
        assert_eq!(img.ident(), Some((0x6D6B, 2, 0x10)));
        assert_eq!(s.used(), 5 * 4096);
        let mut b = [0u8; 50];
        s.read(id, 1000, &mut b).unwrap();
        assert_eq!(&b[..], &f[1000..1050]);
        // Reopen: the index survives.
        drop(s);
        let mut s = ImageStore::open(Box::new(fl.clone())).unwrap();
        assert_eq!(s.find(0x6D6B, 2, 0x10).map(|i| i.id), Some(id));
        assert_eq!(s.image(id).unwrap().name, "a.ota");
        s.read(id, f.len() as u32 - 50, &mut b).unwrap();
        assert_eq!(&b[..], &f[f.len() - 50..]);
        // The same identity again is refused, a new version is not.
        assert_eq!(
            upload(&mut s, &f, "again", 4096),
            Err(StoreError::Exists(id))
        );
        let f2 = build(0x6D6B, 2, 0x11, "b", &[1; 9000], None);
        let id2 = upload(&mut s, &f2, "b", 8192).unwrap();
        assert_eq!(s.images().len(), 2);
        s.delete(id).unwrap();
        let s = ImageStore::open(Box::new(fl)).unwrap();
        assert_eq!(
            s.images().iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![id2]
        );
    }

    #[test]
    fn quota_and_bad_files() {
        let mut s = ImageStore::open(Box::new(flash(8))).unwrap();
        let big = build(1, 1, 1, "", &[0; 30_000], None);
        assert!(matches!(
            s.begin(big.len() as u32, UploadMeta::default(), 0),
            Err(StoreError::NoSpace {
                need: 32768,
                largest: 24576,
                free: 24576
            })
        ));
        // Not an OTA file: refused with the first chunk, nothing kept.
        let (id, _) = s.begin(1000, UploadMeta::default(), 0).unwrap();
        assert!(matches!(
            s.write(id, 0, &[0x55; 100]),
            Err(StoreError::File(FileError::BadMagic(_)))
        ));
        assert!(s.images().is_empty());
        // Wrong total size in the header.
        let f = build(1, 1, 1, "", &[0; 100], None);
        let (id, _) = s
            .begin(f.len() as u32 + 1, UploadMeta::default(), 0)
            .unwrap();
        assert!(matches!(
            s.write(id, 0, &f),
            Err(StoreError::File(FileError::SizeMismatch { .. }))
        ));
        // Wrong hash: refused at the commit.
        let (id, _) = s
            .begin(
                f.len() as u32,
                UploadMeta {
                    sha256: Some([0; 32]),
                    ..Default::default()
                },
                0,
            )
            .unwrap();
        s.write(id, 0, &f).unwrap();
        assert_eq!(s.commit(id), Err(StoreError::Hash));
        assert!(s.images().is_empty());
        // Chunks in order only, not past the size.
        let (id, _) = s.begin(f.len() as u32, UploadMeta::default(), 0).unwrap();
        assert_eq!(
            s.write(id, 10, &f[10..20]),
            Err(StoreError::Offset { expected: 0 })
        );
        s.write(id, 0, &f[..50]).unwrap();
        assert_eq!(s.commit(id).err(), Some(StoreError::Size));
        assert_eq!(s.write(id, 50, &[0; 200]), Err(StoreError::Size));
        // Fragmentation: the largest run counts, not the sum.
        let mut s = ImageStore::open(Box::new(flash(8))).unwrap();
        let a = upload(&mut s, &build(1, 1, 1, "", &[0; 4000], None), "a", 4096).unwrap();
        let _b = upload(&mut s, &build(1, 1, 2, "", &[0; 4000], None), "b", 4096).unwrap();
        let _c = upload(&mut s, &build(1, 1, 3, "", &[0; 4000], None), "c", 4096).unwrap();
        s.delete(a).unwrap();
        assert_eq!(s.largest_free(), 3 * 4096);
        assert!(upload(&mut s, &build(1, 1, 4, "", &[0; 4 * 4096], None), "d", 4096).is_err());
        assert!(upload(&mut s, &build(1, 1, 5, "", &[0; 2 * 4096], None), "e", 4096).is_ok());
    }

    #[test]
    fn upload_resumes_after_a_restart_from_the_checkpoint() {
        let fl = flash(64);
        let mut s = ImageStore::open(Box::new(fl.clone())).unwrap();
        let f = build(
            0x1234,
            7,
            3,
            "",
            &(0..150_000u32).map(|i| (i * 7) as u8).collect::<Vec<_>>(),
            None,
        );
        let meta = UploadMeta {
            name: "x".into(),
            ..Default::default()
        };
        let (id, _) = s.begin(f.len() as u32, meta.clone(), 0).unwrap();
        let mut at = 0;
        while at < 100_000 {
            at = s
                .write(id, at, &f[at as usize..at as usize + 5000])
                .unwrap();
        }
        // Restart: the index says 64 KiB.
        drop(s);
        let mut s = ImageStore::open(Box::new(fl.clone())).unwrap();
        assert_eq!(s.upload().map(|u| u.written), Some(CHECKPOINT));
        let (id2, mut at) = s.begin(f.len() as u32, meta.clone(), 0).unwrap();
        assert_eq!((id2, at), (id, CHECKPOINT));
        while (at as usize) < f.len() {
            let end = (at as usize + 7000).min(f.len());
            at = s.write(id, at, &f[at as usize..end]).unwrap();
        }
        s.commit(id).unwrap();
        let mut b = vec![0u8; f.len()];
        s.read(id, 0, &mut b).unwrap();
        assert_eq!(b, f);
        // Another upload replaces an unfinished one.
        let (u, _) = s
            .begin(
                5000,
                UploadMeta {
                    name: "y".into(),
                    ..Default::default()
                },
                0,
            )
            .unwrap();
        let (v, _) = s
            .begin(
                6000,
                UploadMeta {
                    name: "z".into(),
                    ..Default::default()
                },
                0,
            )
            .unwrap();
        assert!(s.image(u).is_none() && s.image(v).is_some());
    }

    #[test]
    fn power_cut_during_an_index_write_keeps_the_previous_index() {
        for budget in [0usize, 1, 17, 200, 2000, 4000] {
            let fl = flash(16);
            let mut s = ImageStore::open(Box::new(fl.clone())).unwrap();
            let f = build(9, 9, 1, "", &[3; 3000], None);
            let id = upload(&mut s, &f, "a", 4096).unwrap();
            fl.0.lock().unwrap().budget = Some(budget);
            let _ = s.delete(id);
            fl.0.lock().unwrap().budget = None;
            let s = ImageStore::open(Box::new(fl)).unwrap();
            assert!(!s.is_foreign(), "budget {budget}");
            // Either the old index (image kept) or the new one (deleted).
            assert!(s.images().len() <= 1, "budget {budget}");
            if let Some(i) = s.images().first() {
                assert_eq!(i.ident(), Some((9, 9, 1)));
            }
        }
    }

    #[test]
    fn expendable_regions_are_taken_over_and_bad_images_dropped() {
        let fl = flash(16);
        fl.0.lock().unwrap().write(5 * 4096, &[0xE9, 0x03]).unwrap(); // an app image
        let mut s = ImageStore::open_expendable(Box::new(fl.clone())).unwrap();
        assert!(!s.is_foreign());
        let f = build(9, 9, 1, "", &[3; 3000], None);
        let id = upload(&mut s, &f, "a", 4096).unwrap();
        assert_eq!(s.offers().len(), 1);
        let mut b = [0u8; 4];
        assert_eq!(
            s.read_offer(u32::from(id), 0, &mut b),
            crate::images::Fetch::Ready
        );
        assert_eq!(
            s.read_offer(0x1_0000, 0, &mut b),
            crate::images::Fetch::Gone
        );
        // The image's first sector is overwritten (a hub update began):
        // after a restart the image is not served.
        let first = s.images()[0].first;
        fl.0.lock().unwrap().erase(u32::from(first)).unwrap();
        let s = ImageStore::open_expendable(Box::new(fl.clone())).unwrap();
        assert!(s.images().is_empty());
        assert!(s.offers().is_empty());
    }

    #[test]
    fn a_region_with_other_data_is_left_alone() {
        let fl = flash(8);
        fl.0.lock().unwrap().write(5 * 4096 + 10, &[0x12]).unwrap();
        let mut s = ImageStore::open(Box::new(fl.clone())).unwrap();
        assert!(s.is_foreign());
        assert_eq!(
            s.begin(1000, UploadMeta::default(), 0),
            Err(StoreError::Foreign)
        );
        s.format().unwrap();
        assert!(!s.is_foreign());
        assert!(!ImageStore::open(Box::new(fl)).unwrap().is_foreign());
    }
}
