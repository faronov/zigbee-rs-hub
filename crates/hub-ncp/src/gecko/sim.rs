//! Simulated Gecko bootloader (UART XMODEM) on an in-memory pipe, for the
//! host tests of [`install`](super::install) and for desktop dry runs.
//!
//! It prints the menu after a newline, starts an XMODEM-CRC receive on `1`
//! (sending `C` until the first packet), ACKs duplicates without storing
//! them, checks the received GBL like the real bootloader's parser (framing
//! and CRC) and reports `Serial upload complete` / `aborted`; `2` boots the
//! stored image. Faults can be injected per packet.

use std::collections::{BTreeSet, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::gbl;
use super::xmodem::{ACK, BLOCK, CAN, CRC_READY, EOT, NAK, SOH, crc16};
use crate::transport::pipe::{End, Rx, Tx};
use crate::transport::{NcpControl, PortRx};

/// Fault injection and identity.
#[derive(Debug, Clone)]
pub struct SimConfig {
    /// Version in the menu.
    pub version: String,
    /// Packets (1-based count) answered with NAK the first time.
    pub nak_once: BTreeSet<u32>,
    /// Packets whose first ACK is lost.
    pub drop_ack_once: BTreeSet<u32>,
    /// Abort every upload after `EOT` regardless of the image.
    pub reject: bool,
    /// Interval of the `C` start requests.
    pub c_interval: Duration,
    /// No bootloader at all (the lines do nothing).
    pub absent: bool,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            version: "2.4.2".into(),
            nak_once: BTreeSet::new(),
            drop_ack_once: BTreeSet::new(),
            reject: false,
            c_interval: Duration::from_millis(50),
            absent: false,
        }
    }
}

/// What the simulated NCP saw and runs.
#[derive(Debug, Default, Clone)]
pub struct SimState {
    /// In the bootloader (else running the application).
    pub in_bootloader: bool,
    /// The application image (as received, padding included).
    pub app: Option<Vec<u8>>,
    /// Application boots after an upload (`2`).
    pub boots: u32,
    /// Accepted uploads.
    pub uploads: u32,
    /// Aborted uploads.
    pub aborted: u32,
    /// Packets received (including duplicates and bad ones).
    pub packets: u32,
    /// Duplicate packets ACKed without storing.
    pub duplicates: u32,
    /// Bootloader entries through the lines.
    pub entries: u32,
}

/// The NCP's reset/boot lines as seen by the simulated bootloader.
pub struct SimLines {
    state: Arc<Mutex<SimState>>,
    absent: bool,
}

impl NcpControl for SimLines {
    fn reset(&mut self) -> io::Result<()> {
        self.state.lock().unwrap().in_bootloader = false;
        Ok(())
    }
    fn enter_rom_loader(&mut self) -> io::Result<()> {
        let mut s = self.state.lock().unwrap();
        if !self.absent {
            s.in_bootloader = true;
            s.entries += 1;
        }
        Ok(())
    }
}

/// A running simulated bootloader.
pub struct SimBootloader {
    state: Arc<Mutex<SimState>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SimBootloader {
    /// Start on the NCP end of a pipe; returns the bootloader and its lines.
    pub fn start(ncp: End, cfg: SimConfig) -> (Self, SimLines) {
        let state = Arc::new(Mutex::new(SimState::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (rx, tx) = ncp.split();
        let (s, st) = (state.clone(), stop.clone());
        let absent = cfg.absent;
        let handle = thread::spawn(move || Machine::new(rx, tx, cfg, s, st).run());
        (Self { state: state.clone(), stop, handle: Some(handle) }, SimLines { state, absent })
    }

    /// Snapshot of the state.
    pub fn state(&self) -> SimState {
        self.state.lock().unwrap().clone()
    }
}

impl Drop for SimBootloader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

enum Mode {
    Menu,
    Receive { expected: u8, count: u32, data: Vec<u8>, started: bool, last_c: Instant },
}

struct Machine {
    rx: Rx,
    tx: Tx,
    cfg: SimConfig,
    state: Arc<Mutex<SimState>>,
    stop: Arc<AtomicBool>,
    input: VecDeque<u8>,
    mode: Mode,
}

impl Machine {
    fn new(rx: Rx, tx: Tx, cfg: SimConfig, state: Arc<Mutex<SimState>>, stop: Arc<AtomicBool>) -> Self {
        Self { rx, tx, cfg, state, stop, input: VecDeque::new(), mode: Mode::Menu }
    }

    fn say(&self, s: &str) {
        let _ = self.tx.send(s.as_bytes());
    }

    fn menu(&self) {
        self.say(&format!(
            "\r\nGecko Bootloader v{}\r\n1. upload gbl\r\n2. run\r\n3. ebl info\r\nBL > ",
            self.cfg.version
        ));
    }

    fn run(mut self) {
        let mut buf = [0u8; 512];
        while !self.stop.load(Ordering::SeqCst) {
            match self.rx.read(&mut buf, Duration::from_millis(10)) {
                Ok(n) => self.input.extend(&buf[..n]),
                Err(_) => return,
            }
            if !self.state.lock().unwrap().in_bootloader {
                // The application ignores (and here drops) host bytes.
                self.input.clear();
                self.mode = Mode::Menu;
                continue;
            }
            self.step();
        }
    }

    fn step(&mut self) {
        match &mut self.mode {
            Mode::Menu => {
                while let Some(b) = self.input.pop_front() {
                    match b {
                        b'\r' | b'\n' => {
                            // One menu per CR LF pair.
                            if self.input.front() == Some(&b'\n') {
                                self.input.pop_front();
                            }
                            self.menu();
                        }
                        b'1' => {
                            self.say("\r\nbegin upload\r\n");
                            self.mode = Mode::Receive {
                                expected: 1,
                                count: 0,
                                data: Vec::new(),
                                started: false,
                                last_c: Instant::now() - Duration::from_secs(1),
                            };
                            return;
                        }
                        b'2' => {
                            let mut s = self.state.lock().unwrap();
                            s.in_bootloader = false;
                            s.boots += 1;
                            return;
                        }
                        b'3' => self.menu(),
                        _ => {}
                    }
                }
            }
            Mode::Receive { started, last_c, .. } => {
                if !*started && self.input.is_empty() && last_c.elapsed() >= self.cfg.c_interval {
                    *last_c = Instant::now();
                    let _ = self.tx.send(&[CRC_READY]);
                }
                self.receive();
            }
        }
    }

    fn receive(&mut self) {
        loop {
            let Some(&first) = self.input.front() else { return };
            match first {
                EOT => {
                    self.input.pop_front();
                    let _ = self.tx.send(&[ACK]);
                    self.end_upload();
                    return;
                }
                CAN => {
                    self.input.clear();
                    self.say("\r\nSerial upload aborted\r\n");
                    self.state.lock().unwrap().aborted += 1;
                    self.mode = Mode::Menu;
                    self.menu();
                    return;
                }
                SOH if self.input.len() >= BLOCK + 5 => {
                    let p: Vec<u8> = self.input.drain(..BLOCK + 5).collect();
                    self.packet(&p);
                }
                SOH => return,
                _ => {
                    self.input.pop_front();
                }
            }
        }
    }

    fn packet(&mut self, p: &[u8]) {
        let Mode::Receive { expected, count, data, started, .. } = &mut self.mode else { return };
        *started = true;
        self.state.lock().unwrap().packets += 1;
        let crc_ok = u16::from_be_bytes([p[BLOCK + 3], p[BLOCK + 4]]) == crc16(&p[3..BLOCK + 3]);
        if p[1] != 255 - p[2] || !crc_ok {
            let _ = self.tx.send(&[NAK]);
            return;
        }
        if p[1] == expected.wrapping_sub(1) && *count > 0 {
            self.state.lock().unwrap().duplicates += 1;
            let _ = self.tx.send(&[ACK]);
            return;
        }
        if p[1] != *expected {
            let _ = self.tx.send(&[CAN, CAN]);
            self.mode = Mode::Menu;
            return;
        }
        let n = *count + 1;
        if self.cfg.nak_once.remove(&n) {
            let _ = self.tx.send(&[NAK]);
            return;
        }
        data.extend_from_slice(&p[3..BLOCK + 3]);
        *count = n;
        *expected = expected.wrapping_add(1);
        if self.cfg.drop_ack_once.remove(&n) {
            return;
        }
        let _ = self.tx.send(&[ACK]);
    }

    fn end_upload(&mut self) {
        let Mode::Receive { data, .. } = std::mem::replace(&mut self.mode, Mode::Menu) else { return };
        // The real bootloader parses (and writes) while receiving; the
        // verdict comes after EOT.
        thread::sleep(Duration::from_millis(5));
        match gbl::check(&data) {
            Ok(_) if !self.cfg.reject => {
                let mut s = self.state.lock().unwrap();
                s.app = Some(data);
                s.uploads += 1;
                drop(s);
                self.say("\r\nSerial upload complete\r\n");
            }
            _ => {
                // The application area is no longer valid.
                let mut s = self.state.lock().unwrap();
                s.app = None;
                s.aborted += 1;
                drop(s);
                self.say("\r\nSerial upload aborted\r\n\r\nGBL parse error 0x0C\r\n");
            }
        }
        self.menu();
    }
}
