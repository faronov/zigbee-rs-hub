//! Hang supervisor and breadcrumbs.
//!
//! Each bridge thread and TinyUSB callback records where it is (a short
//! stage code and a millisecond timestamp) before anything that may block.
//! A supervisor thread restarts the S3 if the console loop stops iterating
//! or a TinyUSB callback stays inside our code too long, after saving the
//! breadcrumbs and the FreeRTOS task states to RTC memory that survives the
//! software reset. `diag` on the console prints that record.

use std::fmt::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use esp_idf_svc::sys;

#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Site {
    Console = 0,
    Uart2Usb,
    Usb2Uart,
    CbLineState,
    CbLineCoding,
    CbRx,
}
const SITES: [&str; 6] = ["console", "uart2usb", "usb2uart", "cb_line_state", "cb_line_coding", "cb_rx"];

/// Stage 0 = idle (callbacks: not inside our code).
static STAGE: [AtomicU32; 6] = [const { AtomicU32::new(0) }; 6];
static SINCE: [AtomicU32; 6] = [const { AtomicU32::new(0) }; 6];

const CONSOLE_STALL_MS: u32 = 15_000;
const CALLBACK_STALL_MS: u32 = 5_000;
const MAGIC: u32 = 0x4842_5244; // "HBRD"

#[repr(C)]
struct Record {
    magic: u32,
    reason: u32,
    len: u32,
    text: [u8; 1500],
}

#[unsafe(link_section = ".rtc_noinit")]
static mut LAST: Record = Record { magic: 0, reason: 0, len: 0, text: [0; 1500] };

pub fn now_ms() -> u32 {
    (unsafe { sys::esp_timer_get_time() } / 1000) as u32
}

pub fn mark(site: Site, stage: u32) {
    SINCE[site as usize].store(now_ms(), Ordering::Relaxed);
    STAGE[site as usize].store(stage, Ordering::Relaxed);
}

fn snapshot(reason: &str) -> String {
    let now = now_ms();
    let mut s = format!("{reason} at {now} ms\r\n");
    for (i, name) in SITES.iter().enumerate() {
        let st = STAGE[i].load(Ordering::Relaxed);
        let age = now.wrapping_sub(SINCE[i].load(Ordering::Relaxed));
        let _ = write!(s, "  {name}: stage {st}, {age} ms ago\r\n");
    }
    unsafe {
        let n = sys::uxTaskGetNumberOfTasks() as usize;
        let mut tasks: Vec<sys::TaskStatus_t> = vec![core::mem::zeroed(); n + 4];
        let got = sys::uxTaskGetSystemState(tasks.as_mut_ptr(), tasks.len() as u32, core::ptr::null_mut()) as usize;
        for t in &tasks[..got] {
            let name = core::ffi::CStr::from_ptr(t.pcTaskName).to_string_lossy();
            let _ = write!(s, "  task {name}: state {} prio {} stack-free {}\r\n", t.eCurrentState, t.uxCurrentPriority, t.usStackHighWaterMark);
        }
    }
    s
}

fn save_and_restart(reason: &str) -> ! {
    let text = snapshot(reason);
    unsafe {
        let rec = &mut *core::ptr::addr_of_mut!(LAST);
        let n = text.len().min(rec.text.len());
        rec.text[..n].copy_from_slice(&text.as_bytes()[..n]);
        rec.len = n as u32;
        rec.reason = 1;
        rec.magic = MAGIC;
        sys::esp_restart();
    }
}

/// The record saved before the last supervisor restart, if any.
pub fn last_record() -> Option<String> {
    unsafe {
        let rec = &*core::ptr::addr_of!(LAST);
        if rec.magic != MAGIC || rec.len as usize > rec.text.len() {
            return None;
        }
        Some(String::from_utf8_lossy(&rec.text[..rec.len as usize]).into_owned())
    }
}

pub fn current() -> String {
    snapshot("now")
}

pub fn supervise() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let now = now_ms();
        let console_age = now.wrapping_sub(SINCE[Site::Console as usize].load(Ordering::Relaxed));
        if console_age > CONSOLE_STALL_MS {
            save_and_restart("console loop stalled");
        }
        for site in [Site::CbLineState, Site::CbLineCoding, Site::CbRx] {
            let i = site as usize;
            if STAGE[i].load(Ordering::Relaxed) != 0
                && now.wrapping_sub(SINCE[i].load(Ordering::Relaxed)) > CALLBACK_STALL_MS
            {
                save_and_restart("TinyUSB callback stalled");
            }
        }
    }
}
