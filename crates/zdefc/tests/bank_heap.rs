//! Heap of bank lookups (spec 9.1, 16.4) with a counting allocator: a
//! format 2 bank is read in place (memory-mapped flash on the hub), so only
//! the inflater, the 4 KiB block buffer and the decoded definition use
//! heap; format 1 adds its inflated string table. Run with `--nocapture` to
//! print the table; `ZDEF_BANK=<bank.zdb>` measures a release bank too
//! (every index key is looked up).

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, Ordering};

use zdef::bank::{BLOCK_MAX, Bank, FORMAT_V1, FORMAT_V2, MinizInflate};
use zdefc::{Format, Options, compile_str};

struct Counting;
static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);
/// Serializes the measurements (tests run on several threads).
static LOCK: Mutex<()> = Mutex::new(());

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let n = LIVE.fetch_add(l.size() as isize, Ordering::SeqCst) + l.size() as isize;
        PEAK.fetch_max(n, Ordering::SeqCst);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Ordering::SeqCst);
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// Peak heap above the level at entry while `f` runs, and what `f` leaves.
fn measure<R>(f: impl FnOnce() -> R) -> (R, isize, isize) {
    let base = LIVE.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let r = f();
    (
        r,
        PEAK.load(Ordering::SeqCst) - base,
        LIVE.load(Ordering::SeqCst) - base,
    )
}

fn example_bank(format: u16) -> Vec<u8> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../definitions/examples");
    let mut items = Vec::new();
    let mut paths: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    paths.sort();
    for p in paths {
        let c = compile_str(
            &fs::read_to_string(&p).unwrap(),
            Format::Yaml,
            &Options::default(),
        )
        .unwrap();
        if !c.def.is_fragment() {
            items.push(zdefc::bank::Item::new(&p.display().to_string(), c.cbor).unwrap());
        }
    }
    let refs: Vec<&zdefc::bank::Item> = items.iter().collect();
    let params = zdefc::bank::BankParams {
        format,
        ..Default::default()
    };
    zdefc::bank::build_bank(&refs, &params).unwrap().bytes
}

struct Row {
    strings_heap: isize,
    lookup_peak: isize,
    median: isize,
    loads: usize,
}

/// Opens the bank's string table and loads the definition of every index
/// entry; returns the string table's heap and the peak of one load.
fn lookups(bytes: &[u8]) -> Row {
    let bank = Bank::parse(bytes).unwrap();
    let mut inf = MinizInflate::new();
    let (st, _, strings_heap) = measure(|| bank.strings(&mut inf).unwrap());
    let mut peaks = Vec::new();
    for i in 0..bank.index_len() {
        let e = bank.entry(i);
        let ((), peak, left) = measure(|| {
            // What one lookup allocates on the hub: the block buffer and the
            // decoded definition (the inflater is created once per lookup
            // batch, measured separately below).
            let mut buf = Box::new([0u8; BLOCK_MAX]);
            let d = bank.load(&mut inf, &st, &e, &mut buf).unwrap();
            assert!(d.id.is_some());
        });
        assert_eq!(left, 0, "a lookup leaks");
        peaks.push(peak);
    }
    peaks.sort_unstable();
    Row {
        strings_heap,
        lookup_peak: *peaks.last().unwrap(),
        median: peaks[peaks.len() / 2],
        loads: peaks.len(),
    }
}

#[test]
fn format_2_strings_take_no_heap() {
    let _g = LOCK.lock().unwrap();
    let v2 = example_bank(FORMAT_V2);
    let v1 = example_bank(FORMAT_V1);
    assert_eq!(Bank::parse(&v2).unwrap().header.format, FORMAT_V2);
    assert_eq!(Bank::parse(&v1).unwrap().header.format, FORMAT_V1);
    let (_, inflater, _) = measure(|| drop(MinizInflate::new()));
    let r2 = lookups(&v2);
    let r1 = lookups(&v1);
    println!("inflater (miniz_oxide state): {inflater} B");
    println!("| Bank | Size | String table heap | Peak per load (max / median) | Loads |");
    println!("|---|---:|---:|---:|---:|");
    for (name, b, r) in [("examples v2", &v2, &r2), ("examples v1", &v1, &r1)] {
        println!(
            "| {name} | {} | {} | {} / {} | {} |",
            b.len(),
            r.strings_heap,
            r.lookup_peak,
            r.median,
            r.loads
        );
    }
    assert_eq!(r2.strings_heap, 0, "format 2 reads strings in place");
    assert!(r1.strings_heap > 0);
    assert_eq!(r1.loads, r2.loads);
    if let Ok(p) = std::env::var("ZDEF_BANK") {
        let b = fs::read(&p).unwrap();
        let r = lookups(&b);
        let h = Bank::parse(&b).unwrap().header;
        println!(
            "| {p} (format {}, {} definitions) | {} | {} | {} / {} | {} |",
            h.format,
            h.definitions,
            b.len(),
            r.strings_heap,
            r.lookup_peak,
            r.median,
            r.loads
        );
    }
}

#[test]
fn format_2_rejects_a_broken_string_table() {
    let _g = LOCK.lock().unwrap();
    let good = example_bank(FORMAT_V2);
    let h = Bank::parse(&good).unwrap().header;
    let body = zdef::bank::HEADER_LEN + h.sig_len as usize;
    // count, then offset 1 of the first string (string 0 starts at 0)
    let n = h.strings as usize;
    assert!(n >= 2, "examples share strings");
    let mut bad = good.clone();
    bad[body..body + 4].copy_from_slice(&(h.strings as u32 + 1).to_le_bytes());
    assert!(Bank::parse(&bad).is_err(), "count mismatch");
    let mut bad = good.clone();
    let o1 = body + 4 + 4;
    let v = u32::from_le_bytes(bad[o1..o1 + 4].try_into().unwrap());
    let o2 = u32::from_le_bytes(bad[o1 + 4..o1 + 8].try_into().unwrap());
    bad[o1..o1 + 4].copy_from_slice(&(o2 + 1).to_le_bytes());
    assert!(Bank::parse(&bad).is_err(), "offsets go backwards");
    let mut bad = good.clone();
    let text = body + 4 + (n + 1) * 4;
    bad[text] = 0xFF;
    let _ = v;
    assert!(Bank::parse(&bad).is_err(), "not UTF-8");
}
