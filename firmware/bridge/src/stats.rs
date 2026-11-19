//! Bridge counters.

use std::sync::atomic::{AtomicU32, Ordering};

pub static UART_TO_USB: AtomicU32 = AtomicU32::new(0);
pub static UART_TO_USB_DROPPED: AtomicU32 = AtomicU32::new(0);
pub static USB_TO_UART: AtomicU32 = AtomicU32::new(0);
pub static LINE_STATE_EVENTS: AtomicU32 = AtomicU32::new(0);
pub static LINE_CODING_EVENTS: AtomicU32 = AtomicU32::new(0);
/// UART driver events: hardware RX FIFO overflow (bytes lost on the wire).
pub static RX_FIFO_OVF: AtomicU32 = AtomicU32::new(0);
/// UART driver events: RX ring buffer full (the ISR stops draining the FIFO).
pub static RX_BUFFER_FULL: AtomicU32 = AtomicU32::new(0);
pub static FRAME_ERR: AtomicU32 = AtomicU32::new(0);
pub static PARITY_ERR: AtomicU32 = AtomicU32::new(0);

const ALL: [&AtomicU32; 9] = [
    &UART_TO_USB,
    &UART_TO_USB_DROPPED,
    &USB_TO_UART,
    &LINE_STATE_EVENTS,
    &LINE_CODING_EVENTS,
    &RX_FIFO_OVF,
    &RX_BUFFER_FULL,
    &FRAME_ERR,
    &PARITY_ERR,
];

pub struct Snapshot {
    pub uart_to_usb: u32,
    pub uart_to_usb_dropped: u32,
    pub usb_to_uart: u32,
    pub line_state_events: u32,
    pub line_coding_events: u32,
    pub rx_fifo_ovf: u32,
    pub rx_buffer_full: u32,
    pub frame_err: u32,
    pub parity_err: u32,
}

pub fn add(c: &AtomicU32, n: usize) {
    c.fetch_add(n as u32, Ordering::Relaxed);
}

pub fn reset() {
    for c in ALL {
        c.store(0, Ordering::Relaxed);
    }
}

pub fn snapshot() -> Snapshot {
    let g = |c: &AtomicU32| c.load(Ordering::Relaxed);
    Snapshot {
        uart_to_usb: g(&UART_TO_USB),
        uart_to_usb_dropped: g(&UART_TO_USB_DROPPED),
        usb_to_uart: g(&USB_TO_UART),
        line_state_events: g(&LINE_STATE_EVENTS),
        line_coding_events: g(&LINE_CODING_EVENTS),
        rx_fifo_ovf: g(&RX_FIFO_OVF),
        rx_buffer_full: g(&RX_BUFFER_FULL),
        frame_err: g(&FRAME_ERR),
        parity_err: g(&PARITY_ERR),
    }
}
