//! The last ESP-IDF panic (exception, watchdog, abort) in RTC memory.
//!
//! Hub-mode logs go to UART0, which the lab host cannot see, and a panic
//! can leave the coredump partition empty. `esp_panic_handler` is wrapped
//! (`--wrap`, build.rs): the wrapper stores the exception, the PC, A0 (the
//! caller), EXCCAUSE/EXCVADDR, the core, the task and the reason or abort
//! text, then runs ESP-IDF's handler (print, coredump, reboot). `info`
//! shows it with the number of boots since, so a safe-mode boot in between
//! does not hide it.

use core::ffi::{c_char, c_void};

#[repr(C)]
struct PanicInfo {
    core: i32,
    exception: u32,
    reason: *const c_char,
    description: *const c_char,
    details: *const c_void,
    state: *const c_void,
    addr: *const c_void,
    frame: *const u32,
    pseudo_excause: bool,
}

#[repr(C)]
struct CacheErr {
    err_str: *const c_char,
    vaddr: u32,
    size: u32,
}

unsafe extern "C" {
    fn __real_esp_panic_handler(info: *mut PanicInfo);
    fn esp_cache_err_get_panic_info(err: *mut CacheErr);
    static g_panic_abort: bool;
    static g_panic_abort_details: *const c_char;
}

const MAGIC: u32 = u32::from_le_bytes(*b"CPN3");
const BOOTS_MAGIC: u32 = u32::from_le_bytes(*b"BTC1");

/// Boots since the RTC memory was last cleared (power-on): magic, count.
#[unsafe(link_section = ".rtc_noinit")]
static mut BOOTS: [u32; 2] = [0; 2];

#[repr(C)]
#[derive(Clone, Copy)]
struct Record {
    magic: u32,
    boot: u32,
    core: u32,
    exception: u32,
    pc: u32,
    a0: u32,
    a1: u32,
    exccause: u32,
    excvaddr: u32,
    cache_vaddr: u32,
    cache_size: u32,
    task: [u8; 16],
    text: [u8; 64],
}

#[unsafe(link_section = ".rtc_noinit")]
static mut RECORD: Record =
    Record { magic: 0, boot: 0, core: 0, exception: 0, pc: 0, a0: 0, a1: 0, exccause: 0, excvaddr: 0, cache_vaddr: 0, cache_size: 0, task: [0; 16], text: [0; 64] };

fn copy_c(dst: &mut [u8], src: *const c_char) {
    dst.fill(0);
    if src.is_null() {
        return;
    }
    let n = dst.len() - 1;
    for (i, d) in dst.iter_mut().take(n).enumerate() {
        // SAFETY: a NUL-terminated string from the panic path; read up to the buffer.
        let b = unsafe { *src.add(i) } as u8;
        if b == 0 {
            break;
        }
        *d = b;
    }
}

/// Runs in the panic handler (cache already re-enabled, other core stalled).
#[unsafe(no_mangle)]
unsafe extern "C" fn __wrap_esp_panic_handler(info: *mut PanicInfo) {
    // SAFETY: called once per panic with ESP-IDF's panic info; RECORD is only
    // read at the next boot.
    unsafe {
        let r = &mut *(&raw mut RECORD);
        let i = &*info;
        let f = i.frame;
        let word = |n: usize| if f.is_null() { 0 } else { *f.add(n) };
        r.boot = (*(&raw const BOOTS))[1];
        r.core = i.core as u32;
        r.exception = if g_panic_abort { 3 } else { i.exception };
        // XtExcFrame: exit, pc, ps, a0, a1, …, sar (19), exccause (20), excvaddr (21).
        r.pc = word(1);
        r.a0 = word(3);
        r.a1 = word(4);
        r.exccause = word(20);
        r.excvaddr = word(21);
        copy_c(&mut r.task, esp_idf_svc::sys::pcTaskGetName(core::ptr::null_mut()));
        let mut text = if g_panic_abort && !g_panic_abort_details.is_null() { g_panic_abort_details } else { i.reason };
        (r.cache_vaddr, r.cache_size) = (0, 0);
        // EXCCAUSE 7 with the pseudo cause: the cache error interrupt.
        if i.pseudo_excause && r.exccause == 7 {
            let mut e = CacheErr { err_str: core::ptr::null(), vaddr: 0, size: 0 };
            esp_cache_err_get_panic_info(&mut e);
            text = if e.err_str.is_null() { c"Cache disabled but cached memory region accessed".as_ptr() } else { e.err_str };
            (r.cache_vaddr, r.cache_size) = (e.vaddr, e.size);
        }
        copy_c(&mut r.text, text);
        r.magic = MAGIC;
        __real_esp_panic_handler(info);
    }
}

fn text(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

/// Counts this boot; call once at start-up.
pub fn count_boot() {
    // SAFETY: start-up, before any other thread or panic.
    let b = unsafe { &mut *(&raw mut BOOTS) };
    if b[0] == BOOTS_MAGIC {
        b[1] = b[1].wrapping_add(1);
    } else {
        *b = [BOOTS_MAGIC, 0];
    }
}

/// The last recorded panic and how many boots ago it happened.
pub fn last() -> Option<String> {
    // SAFETY: plain reads of RTC memory written only by the panic handler.
    let (r, now) = unsafe { (*(&raw const RECORD), (*(&raw const BOOTS))[1]) };
    if r.magic != MAGIC {
        return None;
    }
    let kind = ["debug", "interrupt watchdog", "task watchdog", "abort", "fault"].get(r.exception as usize).copied().unwrap_or("?");
    let core = if r.core < 2 { r.core.to_string() } else { "-".into() };
    Some(format!(
        "{} boot(s) ago: core {core} {kind} \"{}\" task {} PC 0x{:08x} A0 0x{:08x} SP 0x{:08x} EXCCAUSE {} EXCVADDR 0x{:08x}{}",
        now.wrapping_sub(r.boot),
        text(&r.text),
        text(&r.task),
        r.pc,
        r.a0,
        r.a1,
        r.exccause,
        r.excvaddr,
        if r.cache_vaddr != 0 || r.cache_size != 0 { format!(" cache address 0x{:08x} size {}", r.cache_vaddr, r.cache_size) } else { String::new() }
    ))
}
