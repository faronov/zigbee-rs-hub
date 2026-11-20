//! RAM per loaded definition: live heap of the decoded model, measured with a
//! counting allocator over the compiled examples. Prints the table used in
//! docs/zdef.md (`cargo test -p zdef --test ram -- --nocapture`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

struct Counting;
static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let n = LIVE.fetch_add(l.size() as isize, Ordering::SeqCst) + l.size() as isize;
        PEAK.fetch_max(n, Ordering::SeqCst);
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Ordering::SeqCst);
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

#[test]
fn ram_per_definition() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../zdefc/tests/golden");
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "cbor"))
        .collect();
    files.sort();
    println!(
        "size_of: Definition {} B, Entity {} B, Vm {} B (arena {})",
        size_of::<zdef::Definition>(),
        size_of::<zdef::Entity>(),
        size_of::<zdef::expr::Vm>(),
        zdef::expr::ARENA_SIZE
    );
    println!("| Definition | CBOR | Live heap after load | Peak during load | Allocations |");
    println!("|---|---:|---:|---:|---:|");
    let mut total_live = 0;
    for p in &files {
        let b = fs::read(p).unwrap();
        let base = LIVE.load(Ordering::SeqCst);
        PEAK.store(base, Ordering::SeqCst);
        let a0 = ALLOCS.load(Ordering::SeqCst);
        let d = zdef::load(&b, &zdef::HubFeatures::v1()).unwrap();
        let live = LIVE.load(Ordering::SeqCst) - base;
        let peak = PEAK.load(Ordering::SeqCst) - base;
        let n = ALLOCS.load(Ordering::SeqCst) - a0;
        let stack_part = size_of::<zdef::Definition>() as isize;
        println!(
            "| `{}` | {} | {} | {} | {} |",
            p.file_stem().unwrap().to_string_lossy(),
            b.len(),
            live + stack_part,
            peak + stack_part,
            n
        );
        total_live += live + stack_part;
        // the decoded model is transient (spec 16.4); keep it bounded
        assert!(
            live + stack_part < 16 * b.len() as isize + 2048,
            "{}",
            p.display()
        );
        drop(d);
    }
    println!("average live: {} B", total_live / files.len() as isize);
}
