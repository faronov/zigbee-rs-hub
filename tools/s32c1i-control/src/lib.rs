//! Atomics a firmware must not compile to S32C1I: built with it, for the
//! check's positive control (each must be found). Mangled functions and one
//! by its line table; word and sub-word widths, read-modify-write loops
//! (no fetch_max: with S32C1I the Xtensa LLVM backend cannot assemble it).
#![no_std]

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU8, Ordering};

#[inline(never)]
pub fn cas(a: &AtomicU32, old: u32, new: u32) -> bool {
    a.compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst).is_ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn s32c1i_control_add(a: &AtomicU32) -> u32 {
    a.fetch_add(1, Ordering::SeqCst)
}

#[inline(never)]
pub fn or8(a: &AtomicU8, v: u8) -> u8 {
    a.fetch_or(v, Ordering::Relaxed)
}

#[inline(never)]
pub fn cas16(a: &AtomicU16, old: u16, new: u16) -> bool {
    a.compare_exchange_weak(old, new, Ordering::SeqCst, Ordering::Relaxed).is_ok()
}

#[inline(never)]
pub fn swap(a: &AtomicBool, v: bool) -> bool {
    a.swap(v, Ordering::SeqCst)
}

#[inline(never)]
pub fn update(a: &AtomicU32) -> u32 {
    a.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |x| Some(x.wrapping_mul(3) ^ 5)).unwrap_or(0)
}
