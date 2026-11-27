//! GBL validation against Simplicity Commander output and synthetic
//! images, and the bootloader session (menu, XMODEM-CRC upload, run)
//! against a simulated Gecko bootloader that follows GSDK
//! `btl_comm_xmodem_common.c` / `btl_xmodem.c` byte for byte, on a virtual
//! clock.

use std::collections::VecDeque;
use std::vec;
use std::vec::Vec;

use super::gbl::*;
use super::xmodem::{self, ACK, CAN, EOT, NAK, SOH};
use super::*;

const STORE: u32 = 0xA_0000;
const POLICY: Policy = Policy::mg24_1536k(STORE);

// Simplicity Commander 1v22p1 `gbl create --app` of a 512-byte synthetic
// application at 0x0800_6000 (zigbee-rs tools/efr32-gbl/tests/fixtures):
// plain, with JSON metadata, and of a 515-byte image (one byte of 0xFF
// padding after the end tag).
const COMMANDER_PLAIN: &[u8] = include_bytes!("fixtures/golden-plain.gbl");
const COMMANDER_META: &[u8] = include_bytes!("fixtures/golden-meta.gbl");
const COMMANDER_ODD: &[u8] = include_bytes!("fixtures/golden-odd.gbl");

fn tag(out: &mut Vec<u8>, id: u32, body: &[u8]) {
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
}

/// A GBL of `tags` after a plain header, with its end tag and Commander's
/// padding.
fn gbl_with(ty: u32, tags: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut h = 0x0300_0000u32.to_le_bytes().to_vec();
    h.extend_from_slice(&ty.to_le_bytes());
    tag(&mut out, TAG_HEADER, &h);
    for (id, body) in tags {
        tag(&mut out, *id, body);
    }
    out.extend_from_slice(&TAG_END.to_le_bytes());
    out.extend_from_slice(&4u32.to_le_bytes());
    let crc = crc32(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    while out.len() % 4 != 0 {
        out.push(0xFF);
    }
    out
}

fn app(app_type: u32) -> (u32, Vec<u8>) {
    let mut b = app_type.to_le_bytes().to_vec();
    b.extend_from_slice(&0x0001_0200u32.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(b"zrs-ncp-mg24\x9c\x1f\x4b\x07");
    (TAG_APPLICATION, b)
}

fn prog(id: u32, address: u32, len: usize) -> (u32, Vec<u8>) {
    let mut b = address.to_le_bytes().to_vec();
    b.extend((0..len).map(|i| (i * 7 + 3) as u8));
    (id, b)
}

fn zrhp_meta(version: u8, fw: &str) -> (u32, Vec<u8>) {
    let mut b = b"ZRHP".to_vec();
    b.push(version);
    b.extend_from_slice(&[0xB1, 0xD0, 1, 2, 3, 4, 5, 6]);
    b.push(fw.len() as u8);
    b.extend_from_slice(fw.as_bytes());
    (TAG_METADATA, b)
}

/// A ZRHP NCP image of `len` program bytes.
fn zrhp_image(len: usize) -> Vec<u8> {
    gbl_with(
        0,
        &[
            app(1),
            zrhp_meta(1, "0.1.0"),
            prog(TAG_ERASEPROG, 0x0800_6000, len),
        ],
    )
}

#[test]
fn commander_images_are_valid_applications() {
    for (name, g, len) in [
        ("plain", COMMANDER_PLAIN, 512),
        ("meta", COMMANDER_META, 512),
        ("odd", COMMANDER_ODD, 515),
    ] {
        let info = parse(g, POLICY).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(info.size as usize, g.len(), "{name}");
        assert_eq!(
            (info.prog_start, info.prog_end, info.prog_bytes),
            (0x0800_6000, 0x0800_6000 + len, len),
            "{name}"
        );
        assert_eq!(
            info.app.map(|a| a.app_type),
            Some(APP_TYPE_ZIGBEE),
            "{name}"
        );
        assert!(!info.signed && info.zrhp.is_none(), "{name}");
        assert_eq!(
            info.kind(),
            ImageKind::Other,
            "{name}: JSON metadata is not a ZRHP record"
        );
    }
    assert_eq!(parse(COMMANDER_META, POLICY).unwrap().metadata, 1);
}

#[test]
fn streamed_in_any_pieces_as_whole() {
    let g = zrhp_image(3000);
    let whole = parse(&g, POLICY).unwrap();
    for piece in [1, 3, 7, 128, 4096] {
        let mut p = GblParser::new(POLICY);
        for c in g.chunks(piece) {
            p.feed(c).unwrap();
        }
        assert_eq!(p.finish().unwrap(), whole, "pieces of {piece}");
    }
    let z = whole.zrhp.unwrap();
    assert_eq!(
        (z.fw(), z.build_id),
        ("0.1.0", [0xB1, 0xD0, 1, 2, 3, 4, 5, 6])
    );
    assert_eq!(whole.kind(), ImageKind::Zrhp);
    assert_eq!(whole.app.unwrap().version, 0x0001_0200);
}

#[test]
fn every_corruption_is_refused() {
    let g = zrhp_image(600);
    // Any flipped byte before the CRC: a tag error, or the CRC.
    for at in [0, 9, 20, 60, 70, 300, g.len() - 9] {
        let mut bad = g.clone();
        bad[at] ^= 0x40;
        assert!(parse(&bad, POLICY).is_err(), "byte {at} flipped");
    }
    let mut bad = g.clone();
    let n = bad.len();
    bad[n - 5] ^= 1;
    assert!(matches!(parse(&bad, POLICY), Err(GblError::Crc { .. })));
    assert_eq!(parse(&g[..g.len() - 12], POLICY), Err(GblError::Truncated));
    assert_eq!(parse(&[], POLICY), Err(GblError::NotGbl));
    assert_eq!(parse(b"\x7fELF and so on", POLICY), Err(GblError::NotGbl));
    let mut extra = g.clone();
    extra.extend_from_slice(&[0xFF; 100]);
    assert!(parse(&extra, POLICY).is_ok(), "XMODEM padding");
    extra.push(0);
    assert_eq!(parse(&extra, POLICY), Err(GblError::TrailingData));
}

#[test]
fn bootloader_and_secure_engine_are_never_written() {
    let below = gbl_with(0, &[app(1), prog(TAG_PROG, 0x0800_5FFC, 16)]);
    assert_eq!(
        parse(&below, POLICY),
        Err(GblError::BelowApplication {
            address: 0x0800_5FFC
        })
    );
    let at_zero = gbl_with(0, &[app(1), prog(TAG_ERASEPROG, 0, 16)]);
    assert_eq!(
        parse(&at_zero, POLICY),
        Err(GblError::BelowApplication { address: 0 })
    );
    let past = gbl_with(0, &[prog(TAG_PROG, 0x0817_FFF0, 32)]);
    assert!(matches!(
        parse(&past, POLICY),
        Err(GblError::BeyondFlash { end: 0x0818_0010 })
    ));
    assert_eq!(
        parse(&gbl_with(0, &[(TAG_BOOTLOADER, vec![0; 16])]), POLICY),
        Err(GblError::BootloaderUpgrade)
    );
    assert_eq!(
        parse(&gbl_with(0, &[(TAG_SE_UPGRADE, vec![0; 16])]), POLICY),
        Err(GblError::SeUpgrade)
    );
}

#[test]
fn encrypted_compressed_and_unknown_are_refused_signed_is_noted() {
    assert_eq!(
        parse(
            &gbl_with(TYPE_ENCRYPTED, &[prog(TAG_PROG, 0x0800_6000, 8)]),
            POLICY
        ),
        Err(GblError::Encrypted)
    );
    for t in [TAG_ENC_HEADER, TAG_ENC_INIT, TAG_ENC_GBL_DATA] {
        assert_eq!(
            parse(&gbl_with(0, &[(t, vec![0; 16])]), POLICY),
            Err(GblError::Encrypted)
        );
    }
    for t in [TAG_PROG_LZ4, TAG_PROG_LZMA] {
        assert_eq!(
            parse(&gbl_with(0, &[prog(t, 0x0800_6000, 8)]), POLICY),
            Err(GblError::Compressed)
        );
    }
    assert_eq!(
        parse(&gbl_with(0, &[(0x1234_5678, vec![1])]), POLICY),
        Err(GblError::UnknownTag(0x1234_5678))
    );
    let signed = gbl_with(
        TYPE_SIGNED,
        &[
            app(1),
            prog(TAG_PROG, 0x0800_6000, 8),
            (TAG_SIGNATURE_ECDSA_P256, vec![0; 64]),
        ],
    );
    assert!(parse(&signed, POLICY).unwrap().signed);
    let mut v2 = gbl_with(0, &[prog(TAG_PROG, 0x0800_6000, 8)]);
    v2[11] = 0x02;
    assert!(matches!(
        parse(&v2, POLICY),
        Err(GblError::Version(0x0200_0000))
    ));
}

#[test]
fn application_and_program_rules() {
    assert_eq!(
        parse(
            &gbl_with(0, &[app(1 << 1), prog(TAG_PROG, 0x0800_6000, 8)]),
            POLICY
        ),
        Err(GblError::NotZigbee(2))
    );
    assert_eq!(
        parse(&gbl_with(0, &[app(1), app(1)]), POLICY),
        Err(GblError::DuplicateApplication)
    );
    // Type 0 (none given), as Nerivec's NVM3 clear.
    assert!(
        parse(
            &gbl_with(0, &[app(0), prog(TAG_PROG, 0x0817_6000, 16)]),
            POLICY
        )
        .is_ok()
    );
    assert_eq!(
        parse(&gbl_with(0, &[app(1)]), POLICY),
        Err(GblError::NoProgram)
    );
    // An NVM3 clear (data only, at the top of flash): another image.
    let nvm3 = gbl_with(0, &[prog(TAG_PROG, 0x0817_6000, 0x8000)]);
    let info = parse(&nvm3, POLICY).unwrap();
    assert_eq!(
        (info.kind(), info.app, info.prog_end),
        (ImageKind::Other, None, 0x0817_E000)
    );
    // A ZRHP image stays below its NV journals.
    let high = gbl_with(0, &[zrhp_meta(1, "x"), prog(TAG_PROG, 0x0815_7FF0, 32)]);
    assert_eq!(
        parse(&high, POLICY),
        Err(GblError::BeyondZrhpWindow { end: 0x0815_8010 })
    );
    assert_eq!(
        parse(
            &gbl_with(0, &[zrhp_meta(2, "x"), prog(TAG_PROG, 0x0800_6000, 8)]),
            POLICY
        ),
        Err(GblError::ZrhpRecord)
    );
    let small = Policy::mg24_1536k(1024);
    assert_eq!(
        parse(&zrhp_image(2000), small),
        Err(GblError::TooLarge { max: 1024 })
    );
}

// ---- the simulated Gecko bootloader ----

const MENU: &[u8] =
    b"\r\nGecko Bootloader v2.04.02\r\n1. upload gbl\r\n2. run\r\n3. ebl info\r\nBL > \0";

#[derive(Default, Clone)]
struct Faults {
    /// No menu on entry (it waits for a key).
    quiet_entry: bool,
    /// Not a bootloader at all: never answers.
    silent: bool,
    /// Receive packets (by arrival count) answered NAK.
    nak: Vec<u32>,
    /// Received packets whose ACK is lost.
    lose_ack: Vec<u32>,
    /// Packets (arrival count) corrupted on the line (CRC error).
    corrupt: Vec<u32>,
    /// The parser refuses the image at this block (CAN).
    refuse_at: Option<u32>,
    /// Never sends `C`.
    no_start: bool,
    /// Starts in an XMODEM receive (an upload left waiting).
    receiving: bool,
}

#[derive(PartialEq, Debug)]
enum St {
    Idle,
    Wait,
    Receive,
    Booted,
}

struct SimBl {
    now: u64,
    out: VecDeque<(u64, u8)>,
    st: St,
    rx: Vec<u8>,
    image: Vec<u8>,
    started: bool,
    last: u8,
    next_c: u64,
    c_left: u32,
    arrivals: u32,
    verified: bool,
    f: Faults,
}

impl SimBl {
    fn new(f: Faults) -> SimBl {
        let mut s = SimBl {
            now: 0,
            out: VecDeque::new(),
            st: St::Idle,
            rx: Vec::new(),
            image: Vec::new(),
            started: false,
            last: 0,
            next_c: 0,
            c_left: 0,
            arrivals: 0,
            verified: false,
            f,
        };
        if s.f.receiving {
            s.begin();
        } else if !s.f.quiet_entry && !s.f.silent {
            s.emit(MENU);
        }
        s
    }

    fn emit(&mut self, b: &[u8]) {
        let at = self.out.back().map_or(self.now, |&(t, _)| t.max(self.now)) + 1;
        self.out.extend(b.iter().map(|&c| (at, c)));
    }

    fn begin(&mut self) {
        self.st = St::Wait;
        self.next_c = self.now + 5;
        self.c_left = 60;
        self.started = false;
        self.image.clear();
        self.verified = false;
    }

    fn complete(&mut self, ok: bool, err: &[u8]) {
        self.st = St::Idle;
        self.rx.clear();
        if ok {
            self.emit(b"\r\nSerial upload complete\r\n\0");
        } else {
            self.emit(b"\r\nSerial upload aborted\r\n\0");
            self.emit(err);
        }
        self.emit(MENU);
    }

    fn tick(&mut self) {
        if self.st == St::Wait && !self.f.no_start && self.now >= self.next_c && self.c_left > 0 {
            self.emit(b"C");
            self.c_left -= 1;
            self.next_c += 1000;
            if self.c_left == 0 {
                self.emit(&[CAN, CAN, CAN]);
                self.st = St::Idle;
                self.emit(MENU);
            }
        }
    }

    fn input(&mut self, b: u8) {
        if self.f.silent {
            return;
        }
        match self.st {
            St::Booted => {}
            St::Idle => match b {
                b'1' => {
                    self.emit(b"\r\nbegin upload\r\n\0");
                    self.begin();
                }
                b'2' if self.verified => self.st = St::Booted,
                b'2' => self.emit(b"\r\nFailed to boot\r\n\0"),
                _ => self.emit(MENU),
            },
            St::Wait | St::Receive => {
                self.st = St::Receive;
                self.rx.push(b);
                self.packet();
            }
        }
    }

    fn packet(&mut self) {
        match self.rx[0] {
            SOH if self.rx.len() < xmodem::PACKET => {}
            SOH => {
                self.arrivals += 1;
                let n = self.arrivals;
                let mut p = core::mem::take(&mut self.rx);
                if self.f.corrupt.contains(&n) {
                    p[50] ^= 0x10;
                }
                let num = p[1];
                let crc = xmodem::crc16(&p[3..131]);
                let out_of_order = if self.started {
                    num != self.last.wrapping_add(1)
                } else {
                    num != 1
                };
                let answer = if self.started && num == self.last {
                    ACK
                } else if out_of_order
                    || num.wrapping_add(p[2]) != 0xFF
                    || crc != u16::from_be_bytes([p[131], p[132]])
                    || self.f.nak.contains(&n)
                {
                    NAK
                } else {
                    self.started = true;
                    self.last = num;
                    self.image.extend_from_slice(&p[3..131]);
                    if self.f.refuse_at == Some(u32::from(num)) {
                        self.emit(&[CAN, CAN, CAN]);
                        self.complete(false, b"\r\nfile error 0x\x0044\r\n");
                        return;
                    }
                    if self.f.lose_ack.contains(&n) { 0 } else { ACK }
                };
                if answer != 0 {
                    self.emit(&[answer]);
                }
            }
            EOT => {
                self.verified = parse(&self.image, POLICY).is_ok();
                if self.verified {
                    self.emit(&[ACK]);
                    self.complete(true, b"");
                } else {
                    self.emit(&[CAN, CAN, CAN]);
                    self.complete(false, b"\r\nfile error 0x\x0043\r\n");
                }
            }
            CAN | b'C' | 0x03 => {
                self.emit(&[CAN, CAN, CAN]);
                self.complete(false, b"\r\nblock error 0x\x0021\r\n");
            }
            _ => {
                self.emit(&[CAN, CAN, CAN]);
                self.complete(false, b"\r\nblock error 0x\x0021\r\n");
            }
        }
    }
}

impl Port for SimBl {
    fn write(&mut self, data: &[u8]) -> Result<(), PortError> {
        for &b in data {
            self.input(b);
        }
        Ok(())
    }

    fn read_byte(&mut self, timeout_ms: u32) -> Result<Option<u8>, PortError> {
        let deadline = self.now + u64::from(timeout_ms);
        loop {
            self.tick();
            if let Some(&(t, b)) = self.out.front()
                && t <= self.now
            {
                self.out.pop_front();
                return Ok(Some(b));
            }
            let mut next = deadline;
            if let Some(&(t, _)) = self.out.front() {
                next = next.min(t);
            }
            if self.st == St::Wait && self.c_left > 0 && !self.f.no_start {
                next = next.min(self.next_c);
            }
            if self.now >= deadline {
                return Ok(None);
            }
            self.now = next.max(self.now + 1).min(deadline);
        }
    }

    fn discard_input(&mut self) {
        let now = self.now;
        self.out.retain(|&(t, _)| t > now);
    }

    fn now_ms(&self) -> u32 {
        self.now as u32
    }
}

fn flash(bl: &mut SimBl, image: &[u8]) -> Result<Upload, SessionError> {
    await_menu(bl)?;
    let mut src = |off: u32, buf: &mut [u8]| {
        buf.copy_from_slice(&image[off as usize..off as usize + buf.len()]);
        true
    };
    let mut last = 0;
    let mut progress = |sent, len| {
        assert!(sent > last && sent <= len);
        last = sent;
    };
    let r = upload(
        bl,
        image.len() as u32,
        &mut src,
        &mut progress,
        &xmodem::Config::default(),
    );
    if r.is_ok() {
        assert_eq!(last, image.len() as u32);
    }
    r
}

fn padded(image: &[u8]) -> Vec<u8> {
    let mut v = image.to_vec();
    v.resize(image.len().div_ceil(128) * 128, 0xFF);
    v
}

#[test]
fn crc16_is_xmodem() {
    assert_eq!(xmodem::crc16(b"123456789"), 0x31C3);
    let p = xmodem::packet(256, b"abc");
    assert_eq!((p[0], p[1], p[2], p[6]), (SOH, 0, 0xFF, 0xFF));
}

#[test]
fn the_auto_printed_menu_is_taken_without_a_newline() {
    let mut bl = SimBl::new(Faults::default());
    let m = await_menu(&mut bl).unwrap();
    assert_eq!(m.version, Some((2, 4, 2)));
    assert_eq!(m.text.received, MENU.len(), "one menu, the NUL included");
    assert!(m.text.as_str().ends_with("BL > "));
}

#[test]
fn a_quiet_bootloader_is_asked_for_its_menu() {
    let mut bl = SimBl::new(Faults {
        quiet_entry: true,
        ..Faults::default()
    });
    let m = await_menu(&mut bl).unwrap();
    assert_eq!(m.version, Some((2, 4, 2)));
    assert!(!m.was_receiving);
}

#[test]
fn no_bootloader_no_menu() {
    let mut bl = SimBl::new(Faults {
        silent: true,
        ..Faults::default()
    });
    assert_eq!(await_menu(&mut bl).unwrap_err(), SessionError::NoMenu);
    assert!(probe(&mut bl).unwrap().is_none());
}

#[test]
fn an_upload_left_waiting_is_cancelled_back_to_the_menu() {
    let mut bl = SimBl::new(Faults {
        receiving: true,
        ..Faults::default()
    });
    let m = await_menu(&mut bl).unwrap();
    assert!(m.was_receiving);
    assert_eq!(bl.st, St::Idle);
    // The probe's newline ends it too.
    let mut bl = SimBl::new(Faults {
        receiving: true,
        ..Faults::default()
    });
    assert!(probe(&mut bl).unwrap().is_some());
    assert_eq!(bl.st, St::Idle);
}

#[test]
fn an_image_uploads_verifies_and_runs() {
    let image = zrhp_image(40_000);
    let mut bl = SimBl::new(Faults::default());
    let up = flash(&mut bl, &image).unwrap();
    assert_eq!(up.xmodem.blocks as usize, image.len().div_ceil(128));
    assert_eq!(up.xmodem.resent, 0);
    assert!(up.text.has(b"Serial upload complete"));
    assert_eq!(bl.image, padded(&image));
    run(&mut bl).unwrap();
    assert_eq!(bl.st, St::Booted);
}

#[test]
fn naks_lost_acks_and_line_errors_are_retried() {
    let image = zrhp_image(6_000);
    let mut bl = SimBl::new(Faults {
        nak: vec![1, 5, 6],
        lose_ack: vec![3, 20],
        corrupt: vec![9, 10],
        ..Faults::default()
    });
    let up = flash(&mut bl, &image).unwrap();
    assert_eq!(up.xmodem.resent, 7);
    assert_eq!(bl.image, padded(&image), "duplicates are not stored twice");
    assert!(bl.verified);
}

#[test]
fn block_numbers_wrap_after_255() {
    let image = zrhp_image(300 * 128);
    let mut bl = SimBl::new(Faults {
        lose_ack: vec![256],
        ..Faults::default()
    });
    flash(&mut bl, &image).unwrap();
    assert_eq!(bl.image, padded(&image));
}

#[test]
fn a_block_never_acknowledged_cancels() {
    let image = zrhp_image(2_000);
    let mut bl = SimBl::new(Faults {
        nak: (4..40).collect(),
        ..Faults::default()
    });
    let e = flash(&mut bl, &image).unwrap_err();
    assert_eq!(
        e,
        SessionError::Upload {
            xmodem: Some(xmodem::XmodemError::Retries { block: 4 }),
            code: Some(0x21)
        }
    );
    assert_eq!(bl.st, St::Idle, "back at the menu");
}

#[test]
fn the_bootloader_refusing_the_image_is_reported_with_its_code() {
    let image = zrhp_image(2_000);
    let mut bl = SimBl::new(Faults {
        refuse_at: Some(3),
        ..Faults::default()
    });
    let e = flash(&mut bl, &image).unwrap_err();
    assert_eq!(
        e,
        SessionError::Upload {
            xmodem: Some(xmodem::XmodemError::Cancelled { block: 3 }),
            code: Some(0x44)
        }
    );
    assert!(std::format!("{e}").contains("unknown tag"));
    // An image that does not verify at the end (the CRC): CAN at EOT.
    let mut bad = zrhp_image(2_000);
    let n = bad.len();
    bad[n - 5] ^= 1;
    let mut bl = SimBl::new(Faults::default());
    let e = flash(&mut bl, &bad).unwrap_err();
    assert_eq!(
        e,
        SessionError::Upload {
            xmodem: Some(xmodem::XmodemError::Rejected),
            code: Some(0x43)
        }
    );
    assert_eq!(run(&mut bl), Err(SessionError::BootFailed));
}

#[test]
fn no_start_times_out() {
    let image = zrhp_image(500);
    let mut bl = SimBl::new(Faults {
        no_start: true,
        ..Faults::default()
    });
    let e = flash(&mut bl, &image).unwrap_err();
    assert!(
        matches!(
            e,
            SessionError::Upload {
                xmodem: Some(xmodem::XmodemError::NoStart),
                ..
            } | SessionError::NoResult
        ),
        "{e:?}"
    );
}

#[test]
fn a_source_failure_cancels() {
    let image = zrhp_image(2_000);
    let mut bl = SimBl::new(Faults::default());
    await_menu(&mut bl).unwrap();
    let mut src = |off: u32, buf: &mut [u8]| {
        if off >= 1024 {
            return false;
        }
        buf.copy_from_slice(&image[off as usize..off as usize + buf.len()]);
        true
    };
    let e = upload(
        &mut bl,
        image.len() as u32,
        &mut src,
        &mut |_, _| {},
        &xmodem::Config::default(),
    )
    .unwrap_err();
    assert_eq!(
        e,
        SessionError::Upload {
            xmodem: Some(xmodem::XmodemError::Source { offset: 1024 }),
            code: Some(0x21)
        }
    );
}

/// Real images, kept out of the repository (SONOFF's stock GBL and
/// Nerivec's NVM3 clear for the Dongle-M, `docs/dongle-max.md`), validated
/// and uploaded to the simulated bootloader:
/// `HUB_GBL_FILES=a.gbl,b.gbl cargo test -p hub-core -- --ignored real_images --nocapture`.
#[test]
#[ignore]
fn real_images() {
    let Ok(list) = std::env::var("HUB_GBL_FILES") else {
        return;
    };
    for path in list.split(',') {
        let g = std::fs::read(path).unwrap();
        let info = parse(&g, POLICY).unwrap_or_else(|e| panic!("{path}: {e}"));
        std::println!("{path}: {info:x?}");
        let mut bl = SimBl::new(Faults::default());
        let up = flash(&mut bl, &g).unwrap();
        assert!(bl.verified, "{path}");
        std::println!("{path}: {} blocks", up.xmodem.blocks);
    }
}
