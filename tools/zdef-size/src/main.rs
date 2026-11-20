//! Links the zdef entry points a hub uses into one image so the linker's
//! dead-code elimination shows what they cost. Never run.
#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::hint::black_box;
use core::sync::atomic::{AtomicUsize, Ordering};

struct Bump;
static NEXT: AtomicUsize = AtomicUsize::new(0);
static mut HEAP: [u8; 4096] = [0; 4096];

unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let off = NEXT.fetch_add(l.size() + l.align(), Ordering::Relaxed);
        unsafe { (&raw mut HEAP).cast::<u8>().add(off) }
    }
    unsafe fn dealloc(&self, _: *mut u8, _: Layout) {}
}

#[global_allocator]
static A: Bump = Bump;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

/// SHA-256 and inflate: software (`soft`) or stand-ins for the ESP32-S3
/// SHA accelerator / ROM `tinfl` (calls through fixed addresses, as ROM
/// functions are linked, so they cost no flash here).
#[cfg(feature = "soft")]
type H = zdef::hash::SoftSha256;
#[cfg(feature = "soft")]
#[allow(dead_code)]
fn inflater() -> zdef::bank::MinizInflate {
    zdef::bank::MinizInflate::new()
}

#[cfg(not(feature = "soft"))]
#[derive(Default)]
struct H;
#[cfg(not(feature = "soft"))]
impl zdef::hash::Sha256 for H {
    fn update(&mut self, d: &[u8]) {
        let f: extern "C" fn(*const u8, usize) =
            unsafe { core::mem::transmute(black_box(0x4000_1000usize)) };
        f(d.as_ptr(), d.len());
    }
    fn finish(self) -> [u8; 32] {
        let mut o = [0; 32];
        let f: extern "C" fn(*mut u8) = unsafe { core::mem::transmute(black_box(0x4000_1004usize)) };
        f(o.as_mut_ptr());
        o
    }
}
#[cfg(not(feature = "soft"))]
struct RomInflate;
#[cfg(not(feature = "soft"))]
impl zdef::bank::Inflate for RomInflate {
    fn inflate(&mut self, i: &[u8], o: &mut [u8]) -> Result<usize, zdef::bank::BankError> {
        let f: extern "C" fn(*const u8, usize, *mut u8, usize) -> isize =
            unsafe { core::mem::transmute(black_box(0x4000_1008usize)) };
        let n = f(i.as_ptr(), i.len(), o.as_mut_ptr(), o.len());
        usize::try_from(n).map_err(|_| zdef::bank::BankError::Inflate)
    }
}
#[cfg(not(feature = "soft"))]
#[allow(dead_code)]
fn inflater() -> RomInflate {
    RomInflate
}

#[allow(dead_code)]
fn input() -> &'static [u8] {
    unsafe { core::slice::from_raw_parts(black_box(0x1000 as *const u8), black_box(64)) }
}

/// size_of on the target: Definition, Entity, Vm, Value.
#[unsafe(no_mangle)]
#[used]
pub static ZDEF_SIZES: [u32; 4] = [
    size_of::<zdef::Definition>() as u32,
    size_of::<zdef::Entity>() as u32,
    size_of::<zdef::expr::Vm>() as u32,
    size_of::<zdef::expr::Value<'static>>() as u32,
];

#[unsafe(no_mangle)]
pub extern "C" fn probe_main() -> usize {
    let mut n = 0usize;
    #[cfg(feature = "load")]
    {
        // decode_canonical (decode + encode) + validate (all expressions verified) + requires
        if let Ok(d) = zdef::load(input(), &zdef::HubFeatures::v1()) {
            n += d.entities.len();
            n += zdef::ContentHash::of_with::<H>(input()).0[0] as usize;
        }
    }
    #[cfg(feature = "vm")]
    {
        let mut vm = zdef::expr::Vm::new();
        let v = vm.eval_or_null(
            input(),
            &[],
            &zdef::expr::Inputs::payload(input()),
            &zdef::expr::NoHost,
        );
        n += !v.is_null() as usize;
    }
    #[cfg(feature = "matching")]
    {
        let fp = black_box(zdef::Fingerprint::default());
        let d = black_box(zdef::Definition::new(zdef::model::Kind::Definition));
        let c = [(zdef::Source::Catalog, &d)];
        n += zdef::matching::select_best(
            &c,
            zdef::Identity::Zigbee(&fp),
            &zdef::HubFeatures::v1(),
            None,
        )
        .map(|s| s.score as usize)
        .unwrap_or(0);
        n += zdef::matching::lookup_hashes(zdef::Identity::Zigbee(&fp)).len();
    }
    #[cfg(feature = "resolve")]
    {
        let fp = black_box(zdef::Fingerprint::default());
        let d = black_box(zdef::Definition::new(zdef::model::Kind::Definition));
        let layers = zdef::resolve::Layers {
            generic: &[],
            generic_consts: &[],
            catalog: Some((zdef::Source::Catalog, &d)),
            self_description: None,
            user: None,
            overrides: None,
        };
        n += zdef::resolve::resolve(&layers, zdef::resolve::Target::Zigbee(&fp))
            .entities
            .len();
    }
    #[cfg(feature = "bank")]
    {
        if let Ok(b) = zdef::bank::Bank::parse(input()) {
            let keys = [zdef::sign::TrustedKey::new_with::<H>([7; 32])];
            if b.verify_with::<H>(&keys).is_ok() {
                let mut inf = inflater();
                if let Ok(st) = b.load_strings(&mut inf) {
                    let mut buf = [0u8; zdef::bank::BLOCK_MAX];
                    for e in b.lookup(black_box(5)) {
                        n += b
                            .load(&mut inf, &st, &e, &mut buf)
                            .map(|d| d.entities.len())
                            .unwrap_or(0);
                    }
                }
            }
        }
    }
    n + ZDEF_SIZES[0] as usize
}
