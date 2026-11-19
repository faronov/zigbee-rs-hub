//! H2 UART (S3 UART1: TX GPIO18 → H2 U0RXD, RX GPIO17 ← H2 U0TXD).
//!
//! ESP-IDF UART driver with its ISR in IRAM (`CONFIG_UART_ISR_IN_IRAM`) and
//! large ring buffers in internal RAM; the RX FIFO threshold and timeout are
//! lowered so the ISR drains the 128-byte hardware FIFO well before it fills.
//!
//! Optional hardware flow control on the board's "reserved" 0 Ω links
//! (schematic v1.3): S3 RTS on GPIO4 (R20 → H2 GPIO5, the H2's CTS) and S3
//! CTS on GPIO5 (R19 ← H2 GPIO4, the H2's RTS). The CTS input has a
//! pull-down, so an H2 image without flow control (pin undriven) never
//! blocks the S3 transmitter.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use esp_idf_svc::sys::{self, EspError, esp};
use hub_core::board;

use crate::stats;

pub const PORT: sys::uart_port_t = 1;
const RX_RING: i32 = 16 * 1024;
const TX_RING: i32 = 8 * 1024;
const RX_FULL_THRESHOLD: u8 = 64;
/// RX timeout in symbol times (1 symbol = 10 bits at 8N1).
const RX_TIMEOUT_SYMBOLS: u8 = 2;

/// S3 UART1 RTS output (S3 GPIO4 → R20 → H2 GPIO5 = H2 U0CTS).
pub const RTS_PIN: i32 = board::reserved::R20_S3;
/// S3 UART1 CTS input (S3 GPIO5 ← R19 ← H2 GPIO4 = H2 U0RTS).
pub const CTS_PIN: i32 = board::reserved::R19_S3;
/// RTS is deasserted when the 128-byte RX FIFO holds this many bytes.
const RX_FLOW_THRESHOLD: u8 = 100;
/// Flow control at boot; R19/R20 are fitted on the hwlab board (see README).
pub const DEFAULT_FLOW: bool = true;

static FLOW: AtomicBool = AtomicBool::new(false);

pub static BAUD: AtomicU32 = AtomicU32::new(board::ROM_BAUD);
/// Line coding as text, e.g. "8N1".
pub static FRAMING: AtomicU32 = AtomicU32::new(u32::from_le_bytes(*b"8N1\0"));

pub fn init(baud: u32) -> Result<(), EspError> {
    unsafe {
        let mut cfg: sys::uart_config_t = core::mem::zeroed();
        cfg.baud_rate = baud as i32;
        cfg.data_bits = sys::uart_word_length_t_UART_DATA_8_BITS;
        cfg.parity = sys::uart_parity_t_UART_PARITY_DISABLE;
        cfg.stop_bits = sys::uart_stop_bits_t_UART_STOP_BITS_1;
        cfg.flow_ctrl = sys::uart_hw_flowcontrol_t_UART_HW_FLOWCTRL_DISABLE;
        cfg.__bindgen_anon_1.source_clk = sys::soc_periph_uart_clk_src_legacy_t_UART_SCLK_DEFAULT;
        let mut queue: sys::QueueHandle_t = core::ptr::null_mut();
        esp!(sys::uart_driver_install(
            PORT,
            RX_RING,
            TX_RING,
            32,
            &mut queue,
            sys::ESP_INTR_FLAG_IRAM as i32,
        ))?;
        let q = queue as usize;
        std::thread::Builder::new()
            .stack_size(3072)
            .spawn(move || count_events(q as sys::QueueHandle_t))
            .expect("uart event thread");
        esp!(sys::uart_param_config(PORT, &cfg))?;
        esp!(sys::uart_set_pin(PORT, board::UART_TX_TO_H2, board::UART_RX_FROM_H2, -1, -1))?;
        // The H2 TX floats while the H2 is held in reset.
        esp!(sys::gpio_set_pull_mode(board::UART_RX_FROM_H2, sys::gpio_pull_mode_t_GPIO_PULLUP_ONLY))?;
        esp!(sys::uart_set_rx_full_threshold(PORT, RX_FULL_THRESHOLD.into()))?;
        esp!(sys::uart_set_rx_timeout(PORT, RX_TIMEOUT_SYMBOLS))?;
    }
    BAUD.store(baud, Ordering::Relaxed);
    set_flow(DEFAULT_FLOW);
    Ok(())
}

fn count_events(queue: sys::QueueHandle_t) {
    let mut ev: sys::uart_event_t = unsafe { core::mem::zeroed() };
    loop {
        let got = unsafe { sys::xQueueReceive(queue, (&mut ev as *mut sys::uart_event_t).cast(), u32::MAX) };
        if got == 0 {
            continue;
        }
        match ev.type_ {
            sys::uart_event_type_t_UART_FIFO_OVF => stats::add(&stats::RX_FIFO_OVF, 1),
            sys::uart_event_type_t_UART_BUFFER_FULL => stats::add(&stats::RX_BUFFER_FULL, 1),
            sys::uart_event_type_t_UART_FRAME_ERR => stats::add(&stats::FRAME_ERR, 1),
            sys::uart_event_type_t_UART_PARITY_ERR => stats::add(&stats::PARITY_ERR, 1),
            _ => {}
        }
    }
}

/// Enable or disable RTS/CTS on the reserved lines.
///
/// Off: RTS is a plain GPIO held low ("ready"), so an H2 image that does use
/// flow control keeps transmitting; CTS is ignored.
pub fn set_flow(on: bool) {
    unsafe {
        if on {
            sys::uart_set_pin(PORT, board::UART_TX_TO_H2, board::UART_RX_FROM_H2, RTS_PIN, CTS_PIN);
            sys::gpio_set_pull_mode(CTS_PIN, sys::gpio_pull_mode_t_GPIO_PULLDOWN_ONLY);
            sys::uart_set_hw_flow_ctrl(PORT, sys::uart_hw_flowcontrol_t_UART_HW_FLOWCTRL_CTS_RTS, RX_FLOW_THRESHOLD);
        } else {
            sys::uart_set_hw_flow_ctrl(PORT, sys::uart_hw_flowcontrol_t_UART_HW_FLOWCTRL_DISABLE, 0);
            sys::gpio_reset_pin(RTS_PIN);
            sys::gpio_set_direction(RTS_PIN, sys::gpio_mode_t_GPIO_MODE_INPUT_OUTPUT);
            sys::gpio_set_level(RTS_PIN, 0);
            sys::gpio_reset_pin(CTS_PIN);
            sys::gpio_set_direction(CTS_PIN, sys::gpio_mode_t_GPIO_MODE_INPUT);
            sys::gpio_set_pull_mode(CTS_PIN, sys::gpio_pull_mode_t_GPIO_PULLDOWN_ONLY);
        }
    }
    FLOW.store(on, Ordering::Relaxed);
}

pub fn flow() -> bool {
    FLOW.load(Ordering::Relaxed)
}

/// Pad levels of the flow-control pins: (RTS out, CTS in).
pub fn flow_levels() -> (bool, bool) {
    unsafe { (sys::gpio_get_level(RTS_PIN) != 0, sys::gpio_get_level(CTS_PIN) != 0) }
}

/// Apply a CDC line coding (bit rate, stop bits 0/1/2 = 1/1.5/2, parity
/// 0..4 = none/odd/even/mark/space, data bits 5..8).
pub fn set_line_coding(baud: u32, stop_bits: u8, parity: u8, data_bits: u8) {
    if baud == 0 {
        return;
    }
    let (word, d) = match data_bits {
        5 => (sys::uart_word_length_t_UART_DATA_5_BITS, b'5'),
        6 => (sys::uart_word_length_t_UART_DATA_6_BITS, b'6'),
        7 => (sys::uart_word_length_t_UART_DATA_7_BITS, b'7'),
        _ => (sys::uart_word_length_t_UART_DATA_8_BITS, b'8'),
    };
    // Mark/space parity has no UART equivalent; treated as none.
    let (par, p) = match parity {
        1 => (sys::uart_parity_t_UART_PARITY_ODD, b'O'),
        2 => (sys::uart_parity_t_UART_PARITY_EVEN, b'E'),
        _ => (sys::uart_parity_t_UART_PARITY_DISABLE, b'N'),
    };
    let (stop, s) = match stop_bits {
        1 => (sys::uart_stop_bits_t_UART_STOP_BITS_1_5, b'H'),
        2 => (sys::uart_stop_bits_t_UART_STOP_BITS_2, b'2'),
        _ => (sys::uart_stop_bits_t_UART_STOP_BITS_1, b'1'),
    };
    let framing = u32::from_le_bytes([d, p, s, 0]);
    // Hosts re-send SET_LINE_CODING on every termios change (pyserial does so
    // for each timeout change); touching the UART mid-frame corrupts RX.
    if BAUD.load(Ordering::Relaxed) == baud && FRAMING.load(Ordering::Relaxed) == framing {
        return;
    }
    unsafe {
        // Let bytes queued at the old rate leave first (bounded wait).
        sys::uart_wait_tx_done(PORT, 20);
        sys::uart_set_baudrate(PORT, baud);
        sys::uart_set_word_length(PORT, word);
        sys::uart_set_parity(PORT, par);
        sys::uart_set_stop_bits(PORT, stop);
    }
    BAUD.store(baud, Ordering::Relaxed);
    FRAMING.store(framing, Ordering::Relaxed);
}

pub fn set_baud(baud: u32) {
    set_line_coding(baud, 0, 0, 8);
}

pub fn framing() -> String {
    let b = FRAMING.load(Ordering::Relaxed).to_le_bytes();
    let s = if b[2] == b'H' { "1.5".to_string() } else { (b[2] as char).to_string() };
    format!("{}{}{}", b[0] as char, b[1] as char, s)
}

/// Only the reader thread may call this (it owns the RX side).
/// Block up to `timeout_ms` for the first byte, then take whatever else is
/// already buffered. Returns the number of bytes read.
pub fn read(buf: &mut [u8], timeout_ms: u32) -> usize {
    service_flush();
    unsafe {
        let n = sys::uart_read_bytes(PORT, buf.as_mut_ptr().cast(), 1, timeout_ms);
        if n <= 0 {
            return 0;
        }
        let mut avail: usize = 0;
        sys::uart_get_buffered_data_len(PORT, &mut avail);
        let more = avail.min(buf.len() - 1);
        if more == 0 {
            return 1;
        }
        let m = sys::uart_read_bytes(PORT, buf[1..].as_mut_ptr().cast(), more as u32, 0);
        1 + m.max(0) as usize
    }
}

/// Queue bytes for transmission; blocks while the TX ring is full.
pub fn write(data: &[u8]) -> usize {
    let n = unsafe { sys::uart_write_bytes(PORT, data.as_ptr().cast(), data.len()) };
    n.max(0) as usize
}

static FLUSH_REQ: AtomicBool = AtomicBool::new(false);

/// Ask the reader thread to discard buffered input and wait (≤ 500 ms) until
/// it has. The reader owns the driver's RX mutex almost continuously, and a
/// FreeRTOS mutex is not handed over to a lower-priority waiter, so calling
/// `uart_flush_input` from the console starved it (bridge 0.2.0 hang).
pub fn flush_input() {
    FLUSH_REQ.store(true, Ordering::SeqCst);
    for _ in 0..50 {
        if !FLUSH_REQ.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Called by the reader thread between reads.
pub fn service_flush() {
    if FLUSH_REQ.load(Ordering::SeqCst) {
        unsafe { sys::uart_flush_input(PORT) };
        FLUSH_REQ.store(false, Ordering::SeqCst);
    }
}
