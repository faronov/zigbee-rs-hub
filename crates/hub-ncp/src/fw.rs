//! NCP firmware update (spec 7.9, 11.5): the image is streamed from its
//! source straight into `FW_WRITE` chunks of the NCP's inactive slot; the
//! hub stores no NCP images. Activation, the trial boot and `FW_CONFIRM`
//! after the resume are handled by [`crate::Hub`].

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};
use zigbee_zrhp::codec::Rest;
use zigbee_zrhp::msg::firmware::{FwBegin, FwBeginRsp, FwFinishRsp, FwGetSlotsRsp, FwWrite, FwWriteRsp};
use zigbee_zrhp::msg::ids;

use crate::driver::{CallOpts, Client, Error, Ticket, encode_body};

/// `FW_GET_SLOTS` states (`products/esp32-zigbee-ncp`).
pub mod slot_state {
    /// Empty or invalid.
    pub const EMPTY: u8 = 0;
    /// Running, confirmed.
    pub const RUNNING: u8 = 1;
    /// Running on trial (needs `FW_CONFIRM`).
    pub const RUNNING_TRIAL: u8 = 2;
    /// Staged (verified, not active).
    pub const STAGED: u8 = 3;
    /// Valid, inactive.
    pub const INACTIVE: u8 = 4;
}

/// One NCP application slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// 0/1 = `ota_0`/`ota_1`.
    pub slot: u8,
    /// [`slot_state`].
    pub state: u8,
    /// Image version.
    pub version: u32,
    /// Image SHA-256.
    pub sha256: [u8; 32],
}

/// `FW_BEGIN` parameters of an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FwImage {
    /// Size in bytes.
    pub size: u32,
    /// SHA-256 of the whole image.
    pub sha256: [u8; 32],
    /// Version (`0xMMmmrrrr`).
    pub version: u32,
    /// 1 ESP app image (+ MCUboot TLVs), 2 MCUboot.
    pub format: u8,
    /// Bit0 allow downgrade (development NCP builds only).
    pub flags: u8,
}

/// Where image bytes come from. Streaming sources (an upload) only support
/// increasing offsets; an attempt to go back fails the update.
pub trait FwSource: Send {
    /// Read up to `buf.len()` bytes at `offset`.
    fn read_at(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<usize>;
}

/// An image file (desktop).
pub struct FileSource(File);

impl FileSource {
    /// Open `path`.
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self(File::open(path)?))
    }

    /// Size and SHA-256 of the whole file.
    pub fn measure(&mut self) -> io::Result<(u32, [u8; 32])> {
        self.0.seek(SeekFrom::Start(0))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 16 * 1024];
        let mut size = 0u64;
        loop {
            let n = self.0.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            size += n as u64;
        }
        let size = u32::try_from(size).map_err(|_| io::Error::other("image larger than 4 GiB"))?;
        Ok((size, hasher.finalize().into()))
    }
}

impl FwSource for FileSource {
    fn read_at(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<usize> {
        self.0.seek(SeekFrom::Start(offset.into()))?;
        let mut n = 0;
        while n < buf.len() {
            let k = self.0.read(&mut buf[n..])?;
            if k == 0 {
                break;
            }
            n += k;
        }
        Ok(n)
    }
}

/// An image held in memory (tests).
pub struct SliceSource(pub Vec<u8>);

impl FwSource for SliceSource {
    fn read_at(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<usize> {
        let data = self.0.get(offset as usize..).unwrap_or(&[]);
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }
}

/// SHA-256 of a byte slice.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// `FW_GET_SLOTS`.
pub fn slots(client: &Client, opts: CallOpts) -> Result<Vec<Slot>, Error> {
    let rsp = client.call_empty(ids::FW_GET_SLOTS, opts)?;
    let body: FwGetSlotsRsp = rsp.body()?;
    Ok(body
        .slots
        .iter()
        .map(|s| Slot { slot: s.slot, state: s.state, version: s.version, sha256: s.sha256 })
        .collect())
}

/// The running slot.
pub fn running(slots: &[Slot]) -> Option<Slot> {
    slots
        .iter()
        .copied()
        .find(|s| matches!(s.state, slot_state::RUNNING | slot_state::RUNNING_TRIAL))
}

/// `FW_BEGIN` … `FW_FINISH`: stage `image` from `src` in the inactive slot.
/// Up to `window` writes are kept in flight. Returns the slot.
pub fn stage(
    client: &Client,
    image: &FwImage,
    src: &mut dyn FwSource,
    window: usize,
    progress: &mut dyn FnMut(u32, u32),
) -> Result<u8, Error> {
    let begin = FwBegin {
        image_size: image.size,
        format: image.format,
        sha256: image.sha256,
        version: image.version,
        flags: image.flags,
    };
    let rsp = client.call(&begin, CallOpts::timeout(Duration::from_secs(30)))?;
    let begin: FwBeginRsp = rsp.body()?;
    let chunk = usize::from(begin.max_chunk.clamp(16, 512));
    let result = write_all(client, image.size, src, chunk, window.max(1), progress);
    if let Err(e) = result {
        let _ = client.call_empty(ids::FW_ABORT, CallOpts::default());
        return Err(e);
    }
    let rsp = client.call_empty(ids::FW_FINISH, CallOpts::timeout(Duration::from_secs(60)))?;
    let finish: FwFinishRsp = rsp.body()?;
    if finish.verify_result != 0 {
        return Err(Error::Other(format!("FW_FINISH verify_result {}", finish.verify_result)));
    }
    Ok(begin.slot)
}

fn write_all(
    client: &Client,
    size: u32,
    src: &mut dyn FwSource,
    chunk: usize,
    window: usize,
    progress: &mut dyn FnMut(u32, u32),
) -> Result<(), Error> {
    let mut inflight: std::collections::VecDeque<(u32, u32, Ticket)> = Default::default();
    let mut next = 0u32;
    let mut acked = 0u32;
    let mut buf = vec![0u8; chunk];
    loop {
        while inflight.len() < window && next < size {
            let want = chunk.min((size - next) as usize);
            let n = src.read_at(next, &mut buf[..want]).map_err(|e| Error::Other(format!("image read: {e}")))?;
            if n == 0 {
                return Err(Error::Other(format!("image source ended at {next} of {size}")));
            }
            let body = encode_body(&FwWrite { offset: next, data: Rest(&buf[..n]) })?;
            let ticket = client.start(ids::FW_WRITE, body, CallOpts::timeout(Duration::from_secs(15)))?;
            inflight.push_back((next, n as u32, ticket));
            next += n as u32;
        }
        let Some((offset, len, ticket)) = inflight.pop_front() else {
            return Ok(());
        };
        let outcome = ticket.wait()?;
        if !outcome.reply.status().is_success() {
            return Err(Error::Status(outcome.reply.status()));
        }
        let rsp: FwWriteRsp = outcome.reply.body()?;
        if rsp.next_offset != offset + len {
            // The NCP disagrees about the position (lost write): drain and
            // continue where it stands; earlier identical bytes are no-ops.
            for (_, _, t) in inflight.drain(..) {
                let _ = t.wait();
            }
            if rsp.next_offset > size {
                return Err(Error::Other(format!("FW_WRITE next_offset {} beyond {size}", rsp.next_offset)));
            }
            next = rsp.next_offset;
        }
        acked = acked.max(rsp.next_offset);
        progress(acked, size);
    }
}
