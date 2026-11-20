//! NCP UART from the board profile: ESP-IDF driver with its ISR in IRAM,
//! RX/TX rings in internal RAM, low RX FIFO threshold and timeout so the
//! ISR drains the 128-byte FIFO early. RTS/CTS only where the profile wires
//! them; without flow control the CTS input is never routed, so an NCP
//! image that does or does not drive RTS cannot block the host.

use std::io;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use esp_idf_svc::sys::{self, EspError, esp};
use hub_core::profile::NcpUart;
use hub_ncp::transport::{Line, PortRx, PortTx};

const RX_RING: i32 = 8 * 1024;
const TX_RING: i32 = 4 * 1024;
const RX_FULL_THRESHOLD: u8 = 64;
/// RX timeout in symbol times.
const RX_TIMEOUT_SYMBOLS: u8 = 2;
/// RTS deasserted at this RX FIFO level.
const RX_FLOW_THRESHOLD: u8 = 100;

static CFG: OnceLock<NcpUart> = OnceLock::new();
static FLOW: AtomicBool = AtomicBool::new(false);
/// Current rate.
pub static BAUD: AtomicU32 = AtomicU32::new(0);
/// RX FIFO overflows and ring-full events.
pub static OVERRUNS: AtomicU32 = AtomicU32::new(0);
/// Frame/parity errors.
pub static LINE_ERRORS: AtomicU32 = AtomicU32::new(0);

fn port() -> sys::uart_port_t {
    CFG.get().map_or(1, |c| c.port.into())
}

fn pins(c: &NcpUart, flow: bool) -> (i32, i32) {
    match (flow, c.rts, c.cts) {
        (true, Some(r), Some(t)) => (r.into(), t.into()),
        _ => (-1, -1),
    }
}

/// Install the driver.
pub fn init(c: &NcpUart, line: Line) -> Result<(), EspError> {
    let _ = CFG.set(*c);
    let port = port();
    unsafe {
        let mut cfg: sys::uart_config_t = core::mem::zeroed();
        cfg.baud_rate = line.baud as i32;
        cfg.data_bits = sys::uart_word_length_t_UART_DATA_8_BITS;
        cfg.parity = sys::uart_parity_t_UART_PARITY_DISABLE;
        cfg.stop_bits = sys::uart_stop_bits_t_UART_STOP_BITS_1;
        cfg.flow_ctrl = sys::uart_hw_flowcontrol_t_UART_HW_FLOWCTRL_DISABLE;
        cfg.__bindgen_anon_1.source_clk = sys::soc_periph_uart_clk_src_legacy_t_UART_SCLK_DEFAULT;
        let mut queue: sys::QueueHandle_t = core::ptr::null_mut();
        esp!(sys::uart_driver_install(port, RX_RING, TX_RING, 32, &mut queue, sys::ESP_INTR_FLAG_IRAM as i32))?;
        let q = queue as usize;
        std::thread::Builder::new()
            .stack_size(3072)
            .spawn(move || count_events(q as sys::QueueHandle_t))
            .expect("uart event thread");
        esp!(sys::uart_param_config(port, &cfg))?;
        esp!(sys::uart_set_pin(port, c.tx.into(), c.rx.into(), -1, -1))?;
        // The NCP TX floats while the NCP is in reset.
        esp!(sys::gpio_set_pull_mode(c.rx.into(), sys::gpio_pull_mode_t_GPIO_PULLUP_ONLY))?;
        esp!(sys::uart_set_rx_full_threshold(port, RX_FULL_THRESHOLD.into()))?;
        esp!(sys::uart_set_rx_timeout(port, RX_TIMEOUT_SYMBOLS))?;
    }
    BAUD.store(line.baud, Ordering::Relaxed);
    set_flow(line.flow);
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
            sys::uart_event_type_t_UART_FIFO_OVF | sys::uart_event_type_t_UART_BUFFER_FULL => {
                OVERRUNS.fetch_add(1, Ordering::Relaxed);
            }
            sys::uart_event_type_t_UART_FRAME_ERR | sys::uart_event_type_t_UART_PARITY_ERR => {
                LINE_ERRORS.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

/// Switch RTS/CTS (ignored on boards without both pins).
pub fn set_flow(on: bool) {
    let Some(c) = CFG.get() else { return };
    let on = on && c.flow_wired();
    let (rts, cts) = pins(c, on);
    unsafe {
        if on {
            sys::uart_set_pin(port(), c.tx.into(), c.rx.into(), rts, cts);
            sys::gpio_set_pull_mode(cts, sys::gpio_pull_mode_t_GPIO_PULLDOWN_ONLY);
            sys::uart_set_hw_flow_ctrl(port(), sys::uart_hw_flowcontrol_t_UART_HW_FLOWCTRL_CTS_RTS, RX_FLOW_THRESHOLD);
        } else {
            sys::uart_set_hw_flow_ctrl(port(), sys::uart_hw_flowcontrol_t_UART_HW_FLOWCTRL_DISABLE, 0);
            if let (Some(r), Some(t)) = (c.rts, c.cts) {
                // RTS held low ("ready") so an NCP using flow control keeps
                // sending; CTS left as a pulled-down input.
                let (r, t) = (i32::from(r), i32::from(t));
                sys::gpio_reset_pin(r);
                sys::gpio_set_direction(r, sys::gpio_mode_t_GPIO_MODE_INPUT_OUTPUT);
                sys::gpio_set_level(r, 0);
                sys::gpio_reset_pin(t);
                sys::gpio_set_direction(t, sys::gpio_mode_t_GPIO_MODE_INPUT);
                sys::gpio_set_pull_mode(t, sys::gpio_pull_mode_t_GPIO_PULLDOWN_ONLY);
            }
        }
    }
    FLOW.store(on, Ordering::Relaxed);
}

/// RTS/CTS active.
pub fn flow() -> bool {
    FLOW.load(Ordering::Relaxed)
}

/// Change the rate.
pub fn set_baud(baud: u32) {
    unsafe { sys::uart_set_baudrate(port(), baud) };
    BAUD.store(baud, Ordering::Relaxed);
}

/// Read what is buffered, waiting up to `ms` for the first byte.
pub fn read(buf: &mut [u8], ms: u32) -> usize {
    let n = unsafe { sys::uart_read_bytes(port(), buf.as_mut_ptr().cast(), buf.len() as u32, ms) };
    n.max(0) as usize
}

/// Queue bytes for transmission (blocks while the TX ring is full).
pub fn write(data: &[u8]) -> usize {
    let n = unsafe { sys::uart_write_bytes(port(), data.as_ptr().cast(), data.len()) };
    n.max(0) as usize
}

/// Drop buffered input.
pub fn flush_input() {
    unsafe { sys::uart_flush_input(port()) };
}

/// Receive half for the ZRHP driver and the bootloader client.
pub struct UartRx;
/// Transmit half.
pub struct UartTx;

impl PortRx for UartRx {
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        Ok(read(buf, timeout.as_millis().clamp(1, 1000) as u32))
    }
    fn clear_input(&mut self) -> io::Result<()> {
        flush_input();
        Ok(())
    }
}

impl PortTx for UartTx {
    fn write_all(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let n = write(data);
            if n == 0 {
                return Err(io::Error::other("uart_write_bytes failed"));
            }
            data = &data[n..];
        }
        Ok(())
    }
    fn set_line(&mut self, line: Line) -> io::Result<()> {
        // Let queued bytes leave at the old rate first.
        unsafe { sys::uart_wait_tx_done(port(), 100) };
        set_baud(line.baud);
        if flow() != line.flow {
            set_flow(line.flow);
        }
        Ok(())
    }
}
