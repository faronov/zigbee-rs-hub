//! Host tests of the Gecko bootloader client against the simulated
//! bootloader.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::sim::{SimBootloader, SimConfig};
use super::*;
use crate::store::MemFlash;
use crate::transport::pipe;

/// The host's transmit half, recording line changes.
struct HostTx {
    tx: pipe::Tx,
    lines: Arc<Mutex<Vec<Line>>>,
}

impl PortTx for HostTx {
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.tx.write_all(data)
    }
    fn set_line(&mut self, line: Line) -> io::Result<()> {
        self.lines.lock().unwrap().push(line);
        Ok(())
    }
}

struct Rig {
    sim: SimBootloader,
    lines: sim::SimLines,
    rx: pipe::Rx,
    tx: HostTx,
    set: Arc<Mutex<Vec<Line>>>,
}

fn rig(cfg: SimConfig) -> Rig {
    let (host, ncp) = pipe::pair();
    let (sim, lines) = SimBootloader::start(ncp, cfg);
    let (rx, tx) = host.split();
    let set = Arc::new(Mutex::new(Vec::new()));
    Rig { sim, lines, rx, tx: HostTx { tx, lines: set.clone() }, set }
}

fn fast() -> Config {
    Config {
        menu_timeout: Duration::from_millis(300),
        status_timeout: Duration::from_secs(2),
        xmodem: xmodem::Config {
            start_timeout: Duration::from_secs(2),
            answer_timeout: Duration::from_millis(150),
            attempts: 5,
        },
        ..Config::default()
    }
}

/// The simulated NCP handles the final `2` asynchronously.
fn settled(r: &Rig, f: impl Fn(&sim::SimState) -> bool) -> sim::SimState {
    for _ in 0..100 {
        let s = r.sim.state();
        if f(&s) {
            return s;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    r.sim.state()
}

fn run(r: &mut Rig, image: &[u8], cfg: &Config) -> Result<Report, GeckoError> {
    let mut src: &[u8] = image;
    let mut last = 0;
    install(&mut r.rx, &mut r.tx, &mut r.lines, &mut src, cfg, &mut |sent, total| {
        assert!(sent > last && sent <= total);
        last = sent;
    })
}

#[test]
fn installs_and_boots_an_image() {
    // 40 KiB: more than 256 packets, so the block number wraps.
    let image = gbl::sample(0x0102_0304, *b"mg24-001", 40 * 1024);
    let mut r = rig(SimConfig::default());
    let report = run(&mut r, &image, &fast()).unwrap();
    assert_eq!(report.menu.version, "2.4.2");
    assert_eq!(report.info.app.map(|a| a.version), Some(0x0102_0304));
    assert_eq!(report.stats.blocks, image.len().div_ceil(128) as u32);
    assert_eq!(report.stats.retries, 0);
    let s = settled(&r, |s| s.boots == 1);
    assert_eq!((s.entries, s.uploads, s.boots, s.in_bootloader), (1, 1, 1, false));
    let app = s.app.unwrap();
    assert_eq!(&app[..image.len()], &image[..]);
    assert!(app[image.len()..].iter().all(|&b| b == xmodem::PAD));
    // The host UART went to the bootloader rate without flow control.
    assert_eq!(*r.set.lock().unwrap(), vec![Line { baud: 115_200, flow: false }]);
}

#[test]
fn recovers_from_nak_and_lost_acks() {
    let image = gbl::sample(7, *b"mg24-002", 4000);
    let cfg = SimConfig {
        nak_once: [1, 5, 30].into_iter().collect(),
        drop_ack_once: [2, 17].into_iter().collect(),
        ..SimConfig::default()
    };
    let mut r = rig(cfg);
    let report = run(&mut r, &image, &fast()).unwrap();
    assert_eq!(report.stats.retries, 5);
    let s = r.sim.state();
    assert_eq!(s.duplicates, 2);
    assert_eq!(&s.app.unwrap()[..image.len()], &image[..]);
}

#[test]
fn reports_a_rejected_image() {
    let image = gbl::sample(7, *b"mg24-003", 1000);
    let mut r = rig(SimConfig { reject: true, ..SimConfig::default() });
    match run(&mut r, &image, &fast()) {
        Err(GeckoError::Aborted(msg)) => assert!(msg.contains("GBL parse error"), "{msg}"),
        other => panic!("{other:?}"),
    }
    let s = r.sim.state();
    assert_eq!((s.aborted, s.boots, s.in_bootloader), (1, 0, true));
}

#[test]
fn refuses_bad_images_before_touching_the_ncp() {
    let mut r = rig(SimConfig::default());
    let mut image = gbl::sample(7, *b"mg24-004", 1000);
    image[40] ^= 1;
    assert!(matches!(run(&mut r, &image, &fast()), Err(GeckoError::Image(gbl::GblError::Crc { .. }))));
    let bootloader = gbl::build(&[
        (gbl::tag::HEADER, &gbl::header_body(0)),
        (gbl::tag::BOOTLOADER, &[0; 32]),
        (gbl::tag::PROG, &[0; 32]),
    ]);
    assert!(matches!(run(&mut r, &bootloader, &fast()), Err(GeckoError::BootloaderUpgrade)));
    assert_eq!(r.sim.state().entries, 0);
    assert!(r.set.lock().unwrap().is_empty());
}

#[test]
fn no_bootloader_is_reported() {
    let image = gbl::sample(7, *b"mg24-005", 1000);
    let mut r = rig(SimConfig { absent: true, ..SimConfig::default() });
    let mut cfg = fast();
    cfg.menu_attempts = 2;
    cfg.menu_timeout = Duration::from_millis(100);
    assert!(matches!(run(&mut r, &image, &cfg), Err(GeckoError::NoMenu)));
}

#[test]
fn installs_from_a_radio_bank() {
    let image = gbl::sample(9, *b"mg24-006", 9000);
    let mut banks = bank::Banks::new(MemFlash::new(32, 4096));
    let mut w = banks.writer(image.len() as u32).unwrap();
    w.write(&image).unwrap();
    let (i, slot, _) = w.finish().unwrap();
    let mut r = rig(SimConfig::default());
    let mut src = banks.image(i).unwrap();
    let report = install(&mut r.rx, &mut r.tx, &mut r.lines, &mut src, &fast(), &mut |_, _| {}).unwrap();
    assert_eq!(report.info.crc32, slot.crc32);
    assert_eq!(job::identity(&slot, b"mg24-006"), job::Identity::Match);
    assert_eq!(&r.sim.state().app.unwrap()[..image.len()], &image[..]);
}
