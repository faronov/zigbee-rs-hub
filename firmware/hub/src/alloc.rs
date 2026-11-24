//! Rust heap allocator that prefers PSRAM when the module has it.
//!
//! ESP-IDF's `malloc` keeps every request below
//! `CONFIG_SPIRAM_MALLOC_ALWAYSINTERNAL` in internal RAM. The hub's data are
//! many small Rust allocations (decoded definitions, device records, JSON
//! values), so on an S3 with PSRAM they filled the ~120 KB of internal heap
//! while the 2 MB of PSRAM stayed empty (an N4R2 board: 4 KB internal free a
//! minute after a fresh boot, every maintenance request refused). Rust data
//! never needs internal RAM here: it is not touched by IRAM interrupt
//! handlers, flash writes copy a PSRAM source through a bounce buffer, and
//! thread stacks are allocated by FreeRTOS, not through this allocator.
//!
//! Atomics are the exception the image is built for: on the S3 the S32C1I
//! instruction (compare-and-set, behind every atomic read-modify-write) is
//! not atomic between the two cores on PSRAM. ESP-IDF builds its own C code
//! without it (`CONFIG_STDATOMIC_S32C1I_SPIRAM_WORKAROUND`): its `__atomic_*`
//! functions take a lock for PSRAM addresses. With S32C1I, every `Arc`
//! count, channel index and lazily created `Mutex` in this heap could race
//! between the cores: lost updates, a channel slot claimed twice, two
//! threads in one mutex, then freed memory in use (the likely cause of
//! zeroed `Sender`s, failed mutex unlocks, cache and MMU faults under load).
//! `.cargo/config.toml` turns the instruction off for all Rust code (std
//! included), so Rust atomics call those functions too; `hub atomics` on the
//! console checks it.
//!
//! Without PSRAM (or before [`init`]) every request goes to the default
//! heap, as before.

#[cfg(target_feature = "s32c1i")]
compile_error!(
    "Rust atomics would use S32C1I, which is not atomic on PSRAM: build the firmware from its directory \
     (firmware/hub, firmware/donglem: their .cargo/config.toml sets -C target-feature=-s32c1i)"
);

use core::alloc::{GlobalAlloc, Layout};
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};

use esp_idf_svc::sys;

static PSRAM: AtomicBool = AtomicBool::new(false);

/// `malloc`'s alignment on the S3 (larger alignments go through
/// `heap_caps_aligned_alloc`).
const MIN_ALIGN: usize = 4;

pub struct PreferPsram;

/// Rust allocations go to PSRAM from now on, if the module has some.
pub fn init() -> bool {
    let has = unsafe { sys::heap_caps_get_total_size(sys::MALLOC_CAP_SPIRAM) } > 0;
    PSRAM.store(has, Ordering::Release);
    has
}

unsafe fn caps_alloc(layout: Layout, caps: u32) -> *mut u8 {
    unsafe {
        if layout.align() <= MIN_ALIGN {
            sys::heap_caps_malloc(layout.size(), caps).cast()
        } else {
            sys::heap_caps_aligned_alloc(layout.align(), layout.size(), caps).cast()
        }
    }
}

unsafe impl GlobalAlloc for PreferPsram {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe {
            if PSRAM.load(Ordering::Acquire) {
                let p = caps_alloc(layout, sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT);
                if !p.is_null() {
                    return p;
                }
            }
            caps_alloc(layout, sys::MALLOC_CAP_DEFAULT)
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // `heap_caps_free` releases memory of any heap, aligned or not.
        unsafe { sys::heap_caps_free(ptr.cast::<c_void>()) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        unsafe {
            if layout.align() <= MIN_ALIGN {
                // Grows in place or moves within the caps it has.
                let a = ptr as usize as u32;
                let external = (sys::SOC_EXTRAM_DATA_LOW..sys::SOC_EXTRAM_DATA_HIGH).contains(&a);
                let caps =
                    if external { sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT } else { sys::MALLOC_CAP_DEFAULT };
                let p = sys::heap_caps_realloc(ptr.cast(), new_size, caps).cast::<u8>();
                if !p.is_null() {
                    return p;
                }
            }
            let new = Layout::from_size_align_unchecked(new_size, layout.align());
            let p = self.alloc(new);
            if !p.is_null() {
                core::ptr::copy_nonoverlapping(ptr, p, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
            p
        }
    }
}
