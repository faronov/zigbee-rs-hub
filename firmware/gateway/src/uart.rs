//! NCP UART from the board profile: ESP-IDF driver with its ISR in IRAM,
//! low RX FIFO threshold and timeout so the ISR drains the FIFO early. No
//! flow control (the Dongle-M has no RTS/CTS between the chips).
//!
//! One user at a time ([`claim`]): the hub's backend for the whole boot,
//! or (in a bring-up boot without the hub) the console's probes. The NCP
//! updater ([`lease`]) borrows the UART from the backend by parking it
//! ([`parked`]): the backend's ports then send nothing and receive nothing
//! (its link sees a silent NCP, as when the MG24 resets) until the UART is
//! given back ([`Lease::give_back`]) or the hub restarts. While the MG24
//! sits in its bootloader the UART stays parked, so nothing of the
//! backend's reaches the bootloader's menu.

use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use esp_idf_svc::sys::{self, EspError, esp};
use hub_core::profile::NcpUart;

const RX_RING: i32 = 4 * 1024;
const TX_RING: i32 = 2 * 1024;
const RX_FULL_THRESHOLD: u16 = 64;
/// RX timeout in symbol times.
const RX_TIMEOUT_SYMBOLS: u8 = 2;

static PORT: AtomicU32 = AtomicU32::new(u32::MAX);
/// Current rate.
pub static BAUD: AtomicU32 = AtomicU32::new(0);
/// RX FIFO overflows and ring-full events.
pub static OVERRUNS: AtomicU32 = AtomicU32::new(0);
/// Frame/parity errors.
pub static LINE_ERRORS: AtomicU32 = AtomicU32::new(0);

/// Who uses the UART.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum User {
    /// A console command (probe, bootloader banner).
    Console,
    /// The hub's backend (EZSP or ZRHP).
    Backend,
    /// The NCP updater (in a boot without the hub).
    Updater,
}

static OWNER: Mutex<Option<User>> = Mutex::new(None);

/// Exclusive use of the UART until dropped.
pub struct Claim(());

impl Drop for Claim {
    fn drop(&mut self) {
        *OWNER.lock().unwrap() = None;
    }
}

/// Take the UART, or say who has it.
pub fn claim(user: User) -> Result<Claim, User> {
    let mut o = OWNER.lock().unwrap();
    match *o {
        Some(u) => Err(u),
        None => {
            *o = Some(user);
            Ok(Claim(()))
        }
    }
}

/// Current user.
pub fn owner() -> Option<User> {
    *OWNER.lock().unwrap()
}

/// The backend's ports are parked (the NCP updater has the UART).
static PARKED: AtomicBool = AtomicBool::new(false);
/// The rate the backend uses (restored when the UART is given back).
static BACKEND_BAUD: AtomicU32 = AtomicU32::new(0);

/// The backend's ports are parked.
pub fn parked() -> bool {
    PARKED.load(Ordering::Acquire)
}

/// The backend sets its line rate: now, or when the UART is given back.
pub fn set_backend_baud(baud: u32) {
    BACKEND_BAUD.store(baud, Ordering::Relaxed);
    if !parked() {
        set_baud(baud);
    }
}

/// Parks the backend's ports from the start of the boot (the MG24 was
/// found in its bootloader).
pub fn park_from_boot() {
    PARKED.store(true, Ordering::Release);
}

/// The NCP updater's use of the UART.
pub enum Lease {
    /// Borrowed from the backend (parked).
    Parked,
    /// Claimed (no backend in this boot).
    Claimed(#[allow(dead_code)] Claim),
}

/// Takes the UART for the NCP updater: claimed when free, borrowed from
/// the backend (parked) when the hub runs; the console's probes keep it.
pub fn lease() -> Result<Lease, User> {
    match claim(User::Updater) {
        Ok(c) => Ok(Lease::Claimed(c)),
        Err(User::Backend) => {
            if !PARKED.swap(true, Ordering::AcqRel) {
                if BACKEND_BAUD.load(Ordering::Relaxed) == 0 {
                    BACKEND_BAUD.store(baud(), Ordering::Relaxed);
                }
                // Lets a write of the backend's in progress finish.
                std::thread::sleep(std::time::Duration::from_millis(50));
                if let Some(p) = port() {
                    unsafe { sys::uart_wait_tx_done(p, 100) };
                }
            }
            Ok(Lease::Parked)
        }
        Err(u) => Err(u),
    }
}

impl Lease {
    /// Gives the UART back to the backend (at its rate). Dropping the
    /// lease instead keeps a borrowed UART parked: the MG24 is in its
    /// bootloader, nothing of the backend's may reach it.
    pub fn give_back(self) {
        if let Lease::Parked = self {
            let b = BACKEND_BAUD.load(Ordering::Relaxed);
            if b != 0 {
                set_baud(b);
            }
            flush_input();
            PARKED.store(false, Ordering::Release);
        }
    }
}

fn port() -> Option<sys::uart_port_t> {
    let p = PORT.load(Ordering::Relaxed);
    (p != u32::MAX).then_some(p as sys::uart_port_t)
}

/// Install the driver.
pub fn init(c: &NcpUart, baud: u32) -> Result<(), EspError> {
    let port = sys::uart_port_t::from(c.port);
    unsafe {
        let mut cfg: sys::uart_config_t = core::mem::zeroed();
        cfg.baud_rate = baud as i32;
        cfg.data_bits = sys::uart_word_length_t_UART_DATA_8_BITS;
        cfg.parity = sys::uart_parity_t_UART_PARITY_DISABLE;
        cfg.stop_bits = sys::uart_stop_bits_t_UART_STOP_BITS_1;
        cfg.flow_ctrl = sys::uart_hw_flowcontrol_t_UART_HW_FLOWCTRL_DISABLE;
        cfg.__bindgen_anon_1.source_clk = sys::soc_periph_uart_clk_src_legacy_t_UART_SCLK_DEFAULT;
        let mut queue: sys::QueueHandle_t = core::ptr::null_mut();
        esp!(sys::uart_driver_install(port, RX_RING, TX_RING, 16, &mut queue, sys::ESP_INTR_FLAG_IRAM as i32))?;
        esp!(sys::uart_param_config(port, &cfg))?;
        esp!(sys::uart_set_pin(port, c.tx.into(), c.rx.into(), -1, -1))?;
        // The NCP's TX floats while the NCP is in reset.
        esp!(sys::gpio_set_pull_mode(c.rx.into(), sys::gpio_pull_mode_t_GPIO_PULLUP_ONLY))?;
        esp!(sys::uart_set_rx_full_threshold(port, RX_FULL_THRESHOLD.into()))?;
        esp!(sys::uart_set_rx_timeout(port, RX_TIMEOUT_SYMBOLS))?;
        let q = queue as usize;
        std::thread::Builder::new()
            .name("ncp-uart-ev".into())
            .stack_size(3072)
            .spawn(move || count_events(q as sys::QueueHandle_t))
            .map_err(|_| EspError::from_infallible::<{ sys::ESP_ERR_NO_MEM }>())?;
    }
    PORT.store(c.port.into(), Ordering::Relaxed);
    BAUD.store(baud, Ordering::Relaxed);
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

fn not_installed() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "NCP UART not installed")
}

/// Change the rate (queued bytes leave at the old rate first).
pub fn set_baud(baud: u32) {
    if let Some(p) = port() {
        unsafe {
            sys::uart_wait_tx_done(p, 100);
            sys::uart_set_baudrate(p, baud);
        }
        BAUD.store(baud, Ordering::Relaxed);
    }
}

/// Current rate.
pub fn baud() -> u32 {
    BAUD.load(Ordering::Relaxed)
}

/// Read what is buffered, waiting up to `ms` for the first byte.
/// `uart_read_bytes` itself returns only once `buf` is full or the line
/// has been quiet for its whole timeout: asked for 256 bytes it held every
/// short ASH frame (ACK, EZSP response) until the timeout, up to 1 s per
/// command. So: one byte with the timeout, then what the ring holds,
/// without waiting (as `firmware/bridge`).
pub fn read(buf: &mut [u8], ms: u32) -> io::Result<usize> {
    let p = port().ok_or_else(not_installed)?;
    if buf.is_empty() {
        return Ok(0);
    }
    let ticks = ms.saturating_mul(sys::configTICK_RATE_HZ) / 1000;
    let n = unsafe { sys::uart_read_bytes(p, buf.as_mut_ptr().cast(), 1, ticks.max(u32::from(ms > 0))) };
    if n < 0 {
        return Err(io::Error::other("uart_read_bytes failed"));
    }
    if n == 0 {
        return Ok(0);
    }
    let mut avail: usize = 0;
    unsafe { sys::uart_get_buffered_data_len(p, &mut avail) };
    let more = avail.min(buf.len() - 1);
    if more == 0 {
        return Ok(1);
    }
    let m = unsafe { sys::uart_read_bytes(p, buf[1..].as_mut_ptr().cast(), more as u32, 0) };
    Ok(1 + m.max(0) as usize)
}

/// Write every byte (blocks while the TX ring is full).
pub fn write_all(mut data: &[u8]) -> io::Result<()> {
    let p = port().ok_or_else(not_installed)?;
    while !data.is_empty() {
        let n = unsafe { sys::uart_write_bytes(p, data.as_ptr().cast(), data.len()) };
        if n <= 0 {
            return Err(io::Error::other("uart_write_bytes failed"));
        }
        data = &data[n as usize..];
    }
    Ok(())
}

/// Drop buffered input.
pub fn flush_input() {
    if let Some(p) = port() {
        unsafe { sys::uart_flush_input(p) };
    }
}
