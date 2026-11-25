//! Atomics a firmware must not compile to S32C1I: built with it, for the
//! check's positive control. One mangled function, one by its line table.
#![no_std]

use core::sync::atomic::{AtomicU32, Ordering};

#[inline(never)]
pub fn cas(a: &AtomicU32, old: u32, new: u32) -> bool {
    a.compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst).is_ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn s32c1i_control_add(a: &AtomicU32) -> u32 {
    a.fetch_add(1, Ordering::SeqCst)
}
