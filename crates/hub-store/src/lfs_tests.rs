//! The files region with real LittleFS images (upstream littlefs through
//! `littlefs2-sys`, configured as esp_littlefs on the Dongle-M: 4 KiB
//! blocks over the whole partition, 128 B reads and programs, 512 B cache):
//! the walk against littlefs's own traversal (also after a power cut at
//! any program or erase of littlefs, and with a torn commit), the claim of
//! a file system with freed blocks, and restored images mounted and read
//! back.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, c_int, c_void};

use littlefs2_sys as sys;

use crate::files::{BLOCK, BLOCKS, Claimed, CopyState, FilesError, FilesRegion, INDEX_BLOCK, Need, Restored, SLOTS, ScanMethod};
use crate::flash::Flash;
use crate::littlefs;
use crate::testutil::{CutFlash, cut_points};

/// littlefs's flash: the image and, for power cuts, how many programs and
/// erases still reach it.
struct Disk {
    data: Vec<u8>,
    budget: Option<usize>,
    /// Programs and erases done.
    ops: usize,
}

impl Disk {
    /// One more program or erase, unless the power is gone.
    fn spend(&mut self) -> bool {
        match &mut self.budget {
            Some(0) => return false,
            Some(n) => *n -= 1,
            None => {}
        }
        self.ops += 1;
        true
    }
}

const LFS_ERR_IO: c_int = -5;

unsafe extern "C" fn read(c: *const sys::lfs_config, block: sys::lfs_block_t, off: sys::lfs_off_t, buf: *mut c_void, size: sys::lfs_size_t) -> c_int {
    let d = unsafe { &*((*c).context as *const Disk) };
    let at = (block * BLOCK + off) as usize;
    unsafe { core::ptr::copy_nonoverlapping(d.data[at..].as_ptr(), buf.cast::<u8>(), size as usize) };
    0
}

unsafe extern "C" fn prog(c: *const sys::lfs_config, block: sys::lfs_block_t, off: sys::lfs_off_t, buf: *const c_void, size: sys::lfs_size_t) -> c_int {
    let d = unsafe { &mut *((*c).context as *mut Disk) };
    if !d.spend() {
        return LFS_ERR_IO;
    }
    let at = (block * BLOCK + off) as usize;
    let src = unsafe { core::slice::from_raw_parts(buf.cast::<u8>(), size as usize) };
    for (d, s) in d.data[at..at + src.len()].iter_mut().zip(src) {
        *d &= s;
    }
    0
}

unsafe extern "C" fn erase(c: *const sys::lfs_config, block: sys::lfs_block_t) -> c_int {
    let d = unsafe { &mut *((*c).context as *mut Disk) };
    if !d.spend() {
        return LFS_ERR_IO;
    }
    let at = (block * BLOCK) as usize;
    d.data[at..at + BLOCK as usize].fill(0xFF);
    0
}

unsafe extern "C" fn sync(_c: *const sys::lfs_config) -> c_int {
    0
}

unsafe extern "C" fn collect(data: *mut c_void, block: sys::lfs_block_t) -> c_int {
    unsafe { &mut *(data as *mut BTreeSet<u32>) }.insert(block);
    0
}

/// The file system's geometry and wear levelling.
#[derive(Clone, Copy)]
struct Geometry {
    blocks: u32,
    block_cycles: i32,
    metadata_max: u32,
}

/// esp_littlefs on the Dongle-M's `files` partition.
const ESP: Geometry = Geometry { blocks: BLOCKS, block_cycles: 512, metadata_max: 0 };

/// A mounted LittleFS.
struct Fs {
    // Boxed: littlefs's context points at the disk, which must not move.
    disk: Box<Disk>,
    cfg: Box<sys::lfs_config>,
    lfs: Box<sys::lfs_t>,
}

fn ok(r: c_int, what: &str) {
    assert!(r >= 0, "{what}: littlefs error {r}");
}

fn path(p: &str) -> CString {
    CString::new(p).unwrap()
}

impl Fs {
    fn on(disk: Vec<u8>, format: bool) -> Fs {
        Fs::with(ESP, disk, None, format).unwrap()
    }

    /// littlefs on `data` (formatted first if `format`); `budget` programs
    /// and erases reach the disk, then the power is gone. `Err` with what
    /// failed and the disk as it is.
    fn with(g: Geometry, data: Vec<u8>, budget: Option<usize>, format: bool) -> Result<Fs, (c_int, Vec<u8>)> {
        let mut disk = Box::new(Disk { data, budget, ops: 0 });
        let cfg = sys::lfs_config {
            context: (&mut *disk as *mut Disk).cast(),
            read: Some(read),
            prog: Some(prog),
            erase: Some(erase),
            sync: Some(sync),
            read_size: 128,
            prog_size: 128,
            block_size: BLOCK,
            block_count: g.blocks,
            block_cycles: g.block_cycles,
            cache_size: 512,
            lookahead_size: 128,
            metadata_max: g.metadata_max,
            ..Default::default()
        };
        let mut fs = Fs { disk, cfg: Box::new(cfg), lfs: Box::default() };
        if format {
            let r = unsafe { sys::lfs_format(&mut *fs.lfs, &*fs.cfg) };
            if r < 0 {
                return Err((r, fs.disk.data));
            }
        }
        let r = unsafe { sys::lfs_mount(&mut *fs.lfs, &*fs.cfg) };
        if r < 0 {
            return Err((r, fs.disk.data));
        }
        Ok(fs)
    }

    /// A new file system on an erased region.
    fn new() -> Fs {
        Fs::on(vec![0xFF; (BLOCK * BLOCKS) as usize], true)
    }

    /// Writes `data` to `p` (created or truncated).
    fn try_write(&mut self, p: &str, data: &[u8]) -> Result<(), c_int> {
        let mut f: sys::lfs_file_t = Default::default();
        let flags = (sys::lfs_open_flags_LFS_O_WRONLY | sys::lfs_open_flags_LFS_O_CREAT | sys::lfs_open_flags_LFS_O_TRUNC) as c_int;
        let r = unsafe { sys::lfs_file_open(&mut *self.lfs, &mut f, path(p).as_ptr(), flags) };
        if r < 0 {
            return Err(r);
        }
        let n = unsafe { sys::lfs_file_write(&mut *self.lfs, &mut f, data.as_ptr().cast(), data.len() as u32) };
        let r = unsafe { sys::lfs_file_close(&mut *self.lfs, &mut f) };
        match (n, r) {
            (n, _) if n < 0 => Err(n),
            (_, r) if r < 0 => Err(r),
            _ => Ok(()),
        }
    }

    /// The contents of `p`.
    fn try_read(&mut self, p: &str) -> Result<Vec<u8>, c_int> {
        let mut f: sys::lfs_file_t = Default::default();
        let r = unsafe { sys::lfs_file_open(&mut *self.lfs, &mut f, path(p).as_ptr(), sys::lfs_open_flags_LFS_O_RDONLY as c_int) };
        if r < 0 {
            return Err(r);
        }
        let mut out = Vec::new();
        let mut buf = vec![0u8; 4096];
        let r = loop {
            let n = unsafe { sys::lfs_file_read(&mut *self.lfs, &mut f, buf.as_mut_ptr().cast(), buf.len() as u32) };
            if n <= 0 {
                break n;
            }
            out.extend_from_slice(&buf[..n as usize]);
        };
        unsafe { sys::lfs_file_close(&mut *self.lfs, &mut f) };
        if r < 0 { Err(r) } else { Ok(out) }
    }

    fn write(&mut self, p: &str, data: &[u8]) {
        if let Err(r) = self.try_write(p, data) {
            panic!("{p}: littlefs error {r}");
        }
    }

    fn read(&mut self, p: &str) -> Vec<u8> {
        self.try_read(p).unwrap_or_else(|r| panic!("{p}: littlefs error {r}"))
    }

    fn remove(&mut self, p: &str) {
        ok(unsafe { sys::lfs_remove(&mut *self.lfs, path(p).as_ptr()) }, p);
    }

    fn mkdir(&mut self, p: &str) {
        ok(unsafe { sys::lfs_mkdir(&mut *self.lfs, path(p).as_ptr()) }, p);
    }

    /// littlefs's own traversal (orphans included).
    fn traverse(&mut self) -> BTreeSet<u32> {
        let mut set = BTreeSet::new();
        ok(unsafe { sys::lfs_fs_traverse(&mut *self.lfs, Some(collect), (&mut set as *mut BTreeSet<u32>).cast()) }, "traverse");
        set
    }

    fn unmount(mut self) -> Vec<u8> {
        ok(unsafe { sys::lfs_unmount(&mut *self.lfs) }, "unmount");
        self.disk.data
    }
}

fn pattern(seed: u32, n: usize) -> Vec<u8> {
    (0..n).map(|i| (i as u32).wrapping_mul(2_654_435_761).wrapping_add(seed).rotate_left(seed % 31) as u8).collect()
}

/// The files of SONOFF's stock firmware, small and inlined.
fn sonoff_files(fs: &mut Fs, files: &mut BTreeMap<String, Vec<u8>>) {
    for (p, n) in [("/config.json", 380), ("/device_info.json", 210), ("/indicator.json", 90)] {
        let d = pattern(n as u32, n);
        fs.write(p, &d);
        files.insert(p.into(), d);
    }
    fs.mkdir("/mg24");
    for (p, n) in [("/mg24/info.json", 700), ("/mg24/ieee.cfg", 24)] {
        let d = pattern(n as u32 + 1, n);
        fs.write(p, &d);
        files.insert(p.into(), d);
    }
}

/// Every file reads back as expected from `image`.
fn assert_files(image: Vec<u8>, files: &BTreeMap<String, Vec<u8>>) {
    let mut fs = Fs::on(image, false);
    for (p, d) in files {
        assert!(fs.read(p) == *d, "{p} differs");
    }
    fs.unmount();
}

fn walk(image: &[u8]) -> Result<BTreeSet<u32>, littlefs::LfsError> {
    littlefs::blocks_in_use(
        &mut |b, off, buf| {
            let at = (b * BLOCK + off) as usize;
            buf.copy_from_slice(&image[at..at + buf.len()]);
            Ok(())
        },
        BLOCK,
        BLOCKS,
    )
}

fn blank(image: &[u8], block: u32) -> bool {
    image[(block * BLOCK) as usize..((block + 1) * BLOCK) as usize].iter().all(|&x| x == 0xFF)
}

fn written(image: &[u8]) -> usize {
    image.chunks(BLOCK as usize).filter(|b| b.iter().any(|&x| x != 0xFF)).count()
}

#[test]
fn the_walk_finds_what_littlefs_traverses() {
    let mut fs = Fs::new();
    let mut files = BTreeMap::new();
    sonoff_files(&mut fs, &mut files);
    let mut seen = Vec::new();
    // Files around the CTZ boundaries (4096, 4096 + 4092, + 4088 …), files
    // rewritten and removed, nested directories, many commits.
    for (step, size) in [0usize, 1, 4095, 4096, 4097, 8187, 8188, 8189, 12_276, 12_277, 40_000, 100_000].into_iter().enumerate() {
        fs.write(&format!("/f{step}"), &pattern(step as u32, size));
        if step % 3 == 0 {
            fs.write("/config.json", &pattern(step as u32 + 7, 300 + step));
        }
        if step == 6 {
            fs.mkdir("/a");
            fs.mkdir("/a/b");
            fs.write("/a/b/deep", &pattern(99, 9000));
        }
        if step >= 8 {
            fs.remove(&format!("/f{}", step - 4));
        }
        let truth = fs.traverse();
        let image = fs.disk.data.clone();
        let walked = walk(&image).unwrap_or_else(|e| panic!("step {step}: {e}"));
        assert_eq!(walked, truth, "step {step}");
        seen.push((walked.len(), written(&image)));
    }
    fs.unmount();
    // Freed blocks stay written: the walk counts fewer than "not erased".
    let (used, written) = *seen.last().unwrap();
    assert!(used < written, "{used} in use, {written} written");
}

#[test]
fn a_damaged_superblock_falls_back_to_every_written_block() {
    let mut fs = Fs::new();
    let mut files = BTreeMap::new();
    sonoff_files(&mut fs, &mut files);
    let mut image = fs.unmount();
    // Clear a byte in each superblock commit (the CRCs no longer match).
    for block in 0..2 {
        image[(block * BLOCK) as usize + 40] = 0;
    }
    assert!(walk(&image).is_err());
    let mut r = FilesRegion::new(CutFlash::new(image.clone()));
    let scan = r.scan().unwrap();
    assert_eq!(scan.method, ScanMethod::NonErased);
    assert!(scan.fallback.is_some());
    assert_eq!(scan.used.len(), written(&image));
}

/// SONOFF's files plus a large file written and deleted (an MG24 update
/// staged and done): many written blocks, few in use.
fn with_freed_blocks() -> (Vec<u8>, BTreeMap<String, Vec<u8>>, BTreeSet<u32>) {
    let mut fs = Fs::new();
    let mut files = BTreeMap::new();
    sonoff_files(&mut fs, &mut files);
    fs.write("/mg24/update.gbl", &pattern(5, 300_000));
    fs.remove("/mg24/update.gbl");
    let d = pattern(6, 20_000);
    fs.write("/backup.json", &d);
    files.insert("/backup.json".into(), d);
    let truth = fs.traverse();
    (fs.unmount(), files, truth)
}

#[test]
fn the_claim_copies_the_blocks_littlefs_uses_not_every_written_one() {
    let (image, files, truth) = with_freed_blocks();
    assert!(written(&image) > SLOTS, "{} written", written(&image));
    let mut r = FilesRegion::new(CutFlash::new(image.clone()));
    let scan = r.scan().unwrap();
    assert_eq!(scan.method, ScanMethod::LittleFs);
    // A pair's second block may be erased still: nothing to copy there.
    let expect: Vec<u32> = truth.iter().copied().filter(|b| *b < INDEX_BLOCK && !blank(&image, *b)).collect();
    assert_eq!(scan.used, expect);
    assert!(scan.used.len() <= SLOTS);
    match r.claim() {
        Ok(Claimed::Now { copied }) => assert_eq!(copied, expect.len()),
        other => panic!("{other:?}"),
    }
    assert!(r.status().unwrap().ready());
    assert_eq!(r.restore().unwrap(), Restored::Restored { blocks: expect.len() });
    let back = r.into_inner().data;
    for b in &truth {
        let at = (*b * BLOCK) as usize..((*b + 1) * BLOCK) as usize;
        assert!(back[at.clone()] == image[at], "block {b}");
    }
    assert!(back[(INDEX_BLOCK * BLOCK) as usize..].iter().all(|&x| x == 0xFF), "stock_copy erased");
    assert_files(back, &files);
}

/// SONOFF's files, then files written into `/fill` until littlefs
/// allocates a block of `stock_copy`.
fn reaching_stock_copy() -> (Fs, Vec<String>, BTreeMap<String, Vec<u8>>) {
    let mut fs = Fs::new();
    let mut files = BTreeMap::new();
    sonoff_files(&mut fs, &mut files);
    fs.mkdir("/fill");
    let mut names = Vec::new();
    for i in 0.. {
        let p = format!("/fill/f{i}");
        fs.write(&p, &pattern(i, 16_000));
        names.push(p);
        if fs.traverse().iter().any(|b| *b >= INDEX_BLOCK) {
            return (fs, names, files);
        }
        assert!(i < 300, "littlefs never reached stock_copy");
    }
    unreachable!()
}

#[test]
fn littlefs_blocks_in_stock_copy_refuse_the_claim() {
    let (fs, _, _) = reaching_stock_copy();
    let image = fs.unmount();
    let mut r = FilesRegion::new(CutFlash::new(image.clone()));
    match r.claim() {
        Err(FilesError::CopyAreaInUse { blocks }) => assert!(!blocks.is_empty() && blocks.iter().all(|b| *b >= INDEX_BLOCK)),
        other => panic!("{other:?}"),
    }
    assert!(r.into_inner().data == image, "nothing written");
}

#[test]
fn freed_littlefs_blocks_in_stock_copy_do_not_refuse_the_claim() {
    let (mut fs, names, files) = reaching_stock_copy();
    for p in &names {
        fs.remove(p);
    }
    fs.remove("/fill");
    assert!(fs.traverse().iter().all(|b| *b < INDEX_BLOCK));
    let image = fs.unmount();
    assert!(image[(INDEX_BLOCK * BLOCK) as usize..].iter().any(|&x| x != 0xFF), "freed blocks written in stock_copy");
    let mut r = FilesRegion::new(CutFlash::new(image));
    let scan = r.scan().unwrap();
    assert!(scan.copy_area_used.is_empty() && !scan.copy_area_written.is_empty(), "{scan:?}");
    assert!(matches!(r.claim(), Ok(Claimed::Now { .. })));
    r.restore().unwrap();
    assert_files(r.into_inner().data, &files);
}

#[test]
fn a_power_cut_anywhere_in_a_littlefs_claim_or_return_keeps_the_files() {
    let (image, files, _) = with_freed_blocks();
    let total = {
        let mut r = FilesRegion::new(CutFlash::new(image.clone()));
        r.claim().unwrap();
        r.into_inner().ops
    };
    for k in cut_points(total) {
        for torn in [0, 9, 2048] {
            let mut r = FilesRegion::new(CutFlash::new(image.clone()).cut_at(k, torn));
            assert!(r.claim().is_err(), "cut at {k}");
            let mut flash = r.into_inner();
            flash.power_on();
            // Back to stock right away ...
            let mut back = FilesRegion::new(flash.clone());
            back.restore().unwrap_or_else(|e| panic!("restore after a cut at {k}/{torn}: {e}"));
            assert_files(back.into_inner().data, &files);
            // ... or claim again, then a return cut short and finished.
            let mut r = FilesRegion::new(flash);
            r.claim().unwrap_or_else(|e| panic!("claim after a cut at {k}/{torn}: {e}"));
            assert!(r.status().unwrap().ready());
            let claimed = r.into_inner().data;
            let mut r = FilesRegion::new(CutFlash::new(claimed).cut_at(k % 7, torn));
            let _ = r.restore();
            let mut flash = r.into_inner();
            flash.power_on();
            let mut r = FilesRegion::new(flash);
            r.restore().unwrap_or_else(|e| panic!("restore after a cut at {k}/{torn} and {}: {e}", k % 7));
            assert!(!matches!(r.status().unwrap().copy, CopyState::Hub(_)));
            assert_files(r.into_inner().data, &files);
        }
    }
}

#[test]
fn a_flash_read_error_is_not_a_fallback() {
    struct Failing(CutFlash);
    impl Flash for Failing {
        fn sector_size(&self) -> u32 {
            BLOCK
        }
        fn sectors(&self) -> u32 {
            BLOCKS
        }
        fn read(&mut self, offset: u32, buf: &mut [u8]) -> std::io::Result<()> {
            // The scan's blank checks (512 B) and the superblock pair read;
            // the walk's small reads of the other blocks fail.
            if offset >= 2 * BLOCK && buf.len() <= 8 {
                return Err(std::io::Error::other("read error"));
            }
            self.0.read(offset, buf)
        }
        fn write(&mut self, offset: u32, data: &[u8]) -> std::io::Result<()> {
            self.0.write(offset, data)
        }
        fn erase(&mut self, sector: u32) -> std::io::Result<()> {
            self.0.erase(sector)
        }
    }
    let (image, _, _) = with_freed_blocks();
    let mut r = FilesRegion::new(Failing(CutFlash::new(image)));
    assert!(r.scan().is_err());
}

/// The lab unit's case (2026-11-26): the hub claimed SONOFF's LittleFS and
/// used the region, then SONOFF's firmware ran (the hub's image in `ota_1`
/// did not boot): esp_littlefs cannot mount the claimed region, formats it
/// and SONOFF writes its default files. The claim refuses that state;
/// recover keeps the first claim's copy, and the return to stock mounts
/// SONOFF's original files.
#[test]
fn a_region_reformatted_by_sonoffs_littlefs_is_recovered_with_the_original_files() {
    let mut fs = Fs::new();
    let mut files = BTreeMap::new();
    sonoff_files(&mut fs, &mut files);
    let mut r = FilesRegion::new(CutFlash::new(fs.unmount()));
    let Ok(Claimed::Now { copied }) = r.claim() else { panic!("claim") };
    let mut flash = r.into_inner();
    // The hub's data all over its areas.
    for block in (2..INDEX_BLOCK).step_by(7) {
        flash.write(block * BLOCK, &pattern(block, 256)).unwrap();
    }
    let claimed = flash.data;
    assert!(Fs::with(ESP, claimed.clone(), None, false).is_err(), "no LittleFS to mount");
    let mut fs = Fs::on(claimed.clone(), true);
    fs.write("/config.json", &pattern(3, 150));
    fs.mkdir("/mg24");
    fs.write("/mg24/info.json", &pattern(4, 700));
    let reformatted = fs.unmount();
    let copy_area = (INDEX_BLOCK * BLOCK) as usize..;
    assert!(reformatted[copy_area.clone()] == claimed[copy_area], "stock_copy untouched");

    let mut r = FilesRegion::new(CutFlash::new(reformatted.clone()));
    assert_eq!(r.status().unwrap().content, hub_core::layout::dongle_files::Content::LittleFs);
    assert!(matches!(r.claim(), Err(FilesError::Inconsistent)));
    assert!(r.into_inner().data == reformatted, "nothing written");
    let mut r = FilesRegion::new(CutFlash::new(reformatted));
    assert_eq!(r.assess().unwrap(), Need::Changed { copy_ok: true, content_ok: true });
    assert_eq!(r.recover().unwrap(), Claimed::Now { copied });
    assert!(r.status().unwrap().ready());
    r.restore().unwrap();
    assert_files(r.into_inner().data, &files);
}

/// A small file system that relocates a directory every third compaction
/// (`block_cycles` 1) and compacts after three commits (`metadata_max`):
/// esp_littlefs's 512 cycles take the same path, just less often.
const TIRED: Geometry = Geometry { blocks: 48, block_cycles: 1, metadata_max: 512 };

/// `image` with every block the walk does not count erased (what a claim
/// may do to them).
fn only_walked(image: &[u8], walked: &BTreeSet<u32>) -> Vec<u8> {
    let mut out = image.to_vec();
    for (b, block) in out.chunks_mut(BLOCK as usize).enumerate() {
        if !walked.contains(&(b as u32)) {
            block.fill(0xFF);
        }
    }
    out
}

#[test]
fn a_power_cut_while_littlefs_relocates_a_directory_keeps_the_new_pair_s_files() {
    const PATHS: [&str; 2] = ["/keep", "/d/f"];
    let mut fs = Fs::with(TIRED, vec![0xFF; (BLOCK * TIRED.blocks) as usize], None, true).unwrap();
    fs.write("/keep", &pattern(1, 600));
    fs.mkdir("/d");
    fs.write("/d/f", &pattern(2, 5000));
    let base = fs.unmount();
    // A device rewriting a file in a directory, cut off after every program
    // and erase.
    let rewrite = |fs: &mut Fs| -> Result<(), c_int> {
        for i in 0..24 {
            fs.try_write("/d/f", &pattern(10 + i, 5000))?;
        }
        Ok(())
    };
    let total = {
        let mut fs = Fs::with(TIRED, base.clone(), None, false).unwrap();
        rewrite(&mut fs).unwrap();
        fs.disk.ops
    };
    let mut half_orphans = 0;
    for k in 0..total {
        let mut fs = Fs::with(TIRED, base.clone(), Some(k), false).unwrap();
        assert!(rewrite(&mut fs).is_err(), "cut at {k}");
        let image = fs.disk.data.clone();
        let walked = walk(&image).unwrap_or_else(|e| panic!("cut at {k}: {e}"));
        let mut fs = Fs::with(TIRED, image.clone(), None, false).unwrap_or_else(|(e, _)| panic!("cut at {k}: mount {e}"));
        let traversed = fs.traverse();
        assert!(walked.is_superset(&traversed), "cut at {k}");
        // The parent points at the relocated pair, the tails still at the
        // old one: littlefs's traversal misses the new pair's files.
        if walked.len() > traversed.len() {
            half_orphans += 1;
        }
        let mut kept = Fs::with(TIRED, only_walked(&image, &walked), None, false).unwrap_or_else(|(e, _)| panic!("cut at {k}: mount {e}"));
        for p in PATHS {
            let want = fs.try_read(p);
            assert!(want.is_ok(), "cut at {k}: {p}");
            assert!(kept.try_read(p) == want, "cut at {k}: {p} differs");
        }
    }
    assert!(half_orphans > 0, "no cut in a relocation's window ({total} cut points)");
}

#[test]
fn a_torn_commit_after_the_last_valid_one_does_not_stop_the_walk() {
    let (image, files, _) = with_freed_blocks();
    let before = walk(&image).unwrap();
    let mut rd = |b: u32, off: u32, buf: &mut [u8]| {
        let at = (b * BLOCK + off) as usize;
        buf.copy_from_slice(&image[at..at + buf.len()]);
        Ok(())
    };
    let (block, off, ptag) = littlefs::commit_end(&mut rd, [0, 1], BLOCK).unwrap();
    let at = (block * BLOCK + off) as usize;
    assert!(image[at..at + 8].iter().all(|&x| x == 0xFF), "room after the last commit");
    // Valid-looking tags of a commit cut short before its CRC: a delete of
    // an id the directory does not have, a tail of 4 bytes.
    for (tag, data) in [((0x4FFu32 << 20) | (100 << 10), &[][..]), ((0x600 << 20) | (0x3FF << 10) | 4, &[1, 0, 0, 0][..])] {
        let mut torn = image.clone();
        torn[at..at + 4].copy_from_slice(&(tag ^ ptag).to_be_bytes());
        torn[at + 4..at + 4 + data.len()].copy_from_slice(data);
        assert_files(torn.clone(), &files);
        assert_eq!(walk(&torn).unwrap(), before, "tag {tag:08x}");
        let mut r = FilesRegion::new(CutFlash::new(torn));
        assert_eq!(r.scan().unwrap().method, ScanMethod::LittleFs);
        assert!(matches!(r.claim(), Ok(Claimed::Now { .. })), "tag {tag:08x}");
    }
}
