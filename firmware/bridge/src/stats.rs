//! Bridge counters.

use std::sync::atomic::{AtomicU32, Ordering};

pub static UART_TO_USB: AtomicU32 = AtomicU32::new(0);
pub static UART_TO_USB_DROPPED: AtomicU32 = AtomicU32::new(0);
pub static USB_TO_UART: AtomicU32 = AtomicU32::new(0);
pub static LINE_STATE_EVENTS: AtomicU32 = AtomicU32::new(0);
pub static LINE_CODING_EVENTS: AtomicU32 = AtomicU32::new(0);

pub struct Snapshot {
    pub uart_to_usb: u32,
    pub uart_to_usb_dropped: u32,
    pub usb_to_uart: u32,
    pub line_state_events: u32,
    pub line_coding_events: u32,
}

pub fn add(c: &AtomicU32, n: usize) {
    c.fetch_add(n as u32, Ordering::Relaxed);
}

pub fn snapshot() -> Snapshot {
    Snapshot {
        uart_to_usb: UART_TO_USB.load(Ordering::Relaxed),
        uart_to_usb_dropped: UART_TO_USB_DROPPED.load(Ordering::Relaxed),
        usb_to_uart: USB_TO_UART.load(Ordering::Relaxed),
        line_state_events: LINE_STATE_EVENTS.load(Ordering::Relaxed),
        line_coding_events: LINE_CODING_EVENTS.load(Ordering::Relaxed),
    }
}
