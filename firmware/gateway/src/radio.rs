//! NCP (EFR32MG24) firmware through the Gecko bootloader, with the images in
//! the `hubdata` radio banks (`hub_core::layout::hubdata16::RADIO`).
//!
//! - Upload (`POST /radio/gbl`): streamed into the bank that is not the
//!   last-known-good one, checked (GBL framing and CRC, no bootloader
//!   upgrade), read back, recorded as staged.
//! - Install (`radio install`, `POST /radio/install`): stores the job in
//!   NVS and restarts the host; the next boot installs the bank through the
//!   bootloader **before** the hub opens the UART ([`pre_hub`]).
//! - Verify: the hub starts; [`start_monitor`] waits for a ZRHP session
//!   and compares `SYS_NEGOTIATE.build_id` with the image's zigbee-rs
//!   metadata. Success promotes the bank to last-known-good; failure marks
//!   it bad and schedules the last-known-good image (one rollback, no
//!   loops).

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use hub_core::layout::hubdata16;
use hub_core::profile::{BoardProfile, NcpLoader};
use hub_ncp::Hub;
use hub_ncp::gecko::bank::{BankError, BankState, Banks, Slot, bank_name};
use hub_ncp::gecko::job::{self, Identity, Job, Outcome};
use hub_ncp::gecko::{self, Config};
use hub_ncp::transport::{Line, PortTx};
use log::{error, info, warn};

use crate::esp::Region;
use crate::lines::BoardLines;
use crate::settings;
use crate::uart::{UartRx, UartTx};

/// Wait this long for a verified session after an install.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(90);

struct Radio {
    banks: Mutex<Banks<Region>>,
    lines: BoardLines,
    baud: u32,
}

static RADIO: OnceLock<Radio> = OnceLock::new();
/// An install is running (LED).
pub static BUSY: AtomicBool = AtomicBool::new(false);
/// The last update failed (LED).
pub static FAILED: AtomicBool = AtomicBool::new(false);
static PROGRESS: Mutex<String> = Mutex::new(String::new());

/// Open the radio banks if the board updates its NCP through the Gecko
/// bootloader and the 16 MB layout is in place.
pub fn init(board: &BoardProfile, lines: BoardLines, layout_ok: bool) -> bool {
    let NcpLoader::GeckoUart { baud, .. } = board.control.loader else { return false };
    if !layout_ok {
        warn!("radio: no 16 MB hub layout, radio banks unavailable");
        return false;
    }
    let Some(region) = Region::find(hubdata16::RADIO.0, hubdata16::RADIO.1) else { return false };
    let _ = RADIO.set(Radio { banks: Mutex::new(Banks::new(region)), lines, baud });
    FAILED.store(settings::get("radio_last").is_some_and(|s| s.starts_with("failed")), Ordering::Relaxed);
    true
}

/// Radio banks available.
pub fn available() -> bool {
    RADIO.get().is_some()
}

fn job() -> Option<Job> {
    settings::get("radio_job").and_then(|s| Job::decode(&s))
}

fn store_job(job: Option<Job>) {
    if let Err(e) = settings::set("radio_job", job.map(|j| j.encode()).as_deref()) {
        error!("radio: job not stored: {e}");
    }
}

fn last(text: &str) {
    info!("radio: {text}");
    FAILED.store(text.starts_with("failed"), Ordering::Relaxed);
    let _ = settings::set("radio_last", Some(text));
}

fn slot_text(i: u8, s: &Option<Slot>) -> String {
    match s {
        None => format!("bank {}: empty\r\n", bank_name(i)),
        Some(s) => format!(
            "bank {}: {:?} gen {} {} B crc 0x{:08X} app {} build {}\r\n",
            bank_name(i),
            s.state,
            s.seq,
            s.len,
            s.crc32,
            s.app_version.map_or("-".into(), |v| format!("0x{v:08X}")),
            s.build_id.map_or("-".into(), |b| String::from_utf8_lossy(&b).into_owned()),
        ),
    }
}

/// Status for the console and `GET /radio`.
pub fn status_text() -> String {
    let Some(r) = RADIO.get() else {
        return "radio: no Gecko bootloader banks on this board/layout\r\n".into();
    };
    let mut out = String::new();
    match r.banks.lock().unwrap().slots() {
        Ok(s) => {
            out += &slot_text(0, &s[0]);
            out += &slot_text(1, &s[1]);
        }
        Err(e) => out += &format!("banks: {e}\r\n"),
    }
    out += &format!("job: {}\r\n", job().map_or("none".into(), |j| j.encode()));
    out += &format!("last: {}\r\n", settings::get("radio_last").unwrap_or_else(|| "-".into()));
    let p = PROGRESS.lock().unwrap();
    if !p.is_empty() {
        out += &format!("progress: {p}\r\n");
    }
    out
}

/// Stream an upload of `len` bytes into the upload bank.
pub fn upload(len: u32, mut read: impl FnMut(&mut [u8]) -> io::Result<usize>) -> Result<String, String> {
    let r = RADIO.get().ok_or("no radio banks on this board/layout")?;
    if BUSY.load(Ordering::SeqCst) {
        return Err("a radio install is running".into());
    }
    let mut banks = r.banks.lock().unwrap();
    let mut w = banks.writer(len).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 4096];
    let mut got = 0u32;
    while got < len {
        let want = ((len - got) as usize).min(buf.len());
        let n = read(&mut buf[..want]).map_err(|e| format!("receive: {e}"))?;
        if n == 0 {
            return Err(format!("upload ended after {got} of {len} B"));
        }
        w.write(&buf[..n]).map_err(|e| e.to_string())?;
        got += n as u32;
    }
    let (i, slot, info) = w.finish().map_err(|e| e.to_string())?;
    if info.bootloader || info.se_upgrade {
        let _ = banks.set_state(i, BankState::Bad);
        return Err(gecko::GeckoError::BootloaderUpgrade.to_string());
    }
    let msg = format!(
        "staged in bank {}: {} B, app {}, zigbee-rs build {}{}",
        bank_name(i),
        slot.len,
        slot.app_version.map_or("-".into(), |v| format!("0x{v:08X}")),
        slot.build_id.map_or("- (cannot verify the running image)".into(), |b| String::from_utf8_lossy(&b).into_owned()),
        if info.encrypted() { ", encrypted" } else { "" },
    );
    info!("radio: {msg}");
    Ok(msg)
}

/// Schedule an install of bank `bank` (default: the staged bank) for the
/// next boot.
pub fn schedule(bank: Option<u8>) -> Result<String, String> {
    let r = RADIO.get().ok_or("no radio banks on this board/layout")?;
    let mut banks = r.banks.lock().unwrap();
    let slots = banks.slots().map_err(|e| e.to_string())?;
    let bank = match bank {
        Some(b) => b,
        None => (0..2u8)
            .find(|&i| slots[i as usize].is_some_and(|s| s.state == BankState::Staged))
            .ok_or("no staged image (upload one first)")?,
    };
    banks.verify(bank).map_err(|e| e.to_string())?;
    store_job(Some(Job::Install { bank, rollback: false }));
    Ok(format!("install of bank {} scheduled; restarting", bank_name(bank)))
}

/// Run pending installs before the hub starts. `line` is the ZRHP line the
/// UART goes back to afterwards.
pub fn pre_hub(line: Line) {
    let Some(r) = RADIO.get() else { return };
    while let Some(Job::Install { bank, rollback }) = job() {
        BUSY.store(true, Ordering::SeqCst);
        let outcome = install(r, bank);
        BUSY.store(false, Ordering::SeqCst);
        let mut banks = r.banks.lock().unwrap();
        let lkg = banks.last_known_good().ok().flatten().map(|(i, _)| i);
        let o = if outcome.is_ok() { Outcome::Installed } else { Outcome::InstallFailed };
        let step = job::next(Job::Install { bank, rollback }, o, lkg);
        if let Some((i, st)) = step.mark {
            let _ = banks.set_state(i, st);
        }
        drop(banks);
        if let Err(e) = &outcome {
            last(&format!(
                "failed: install of bank {}{}: {e}",
                bank_name(bank),
                if rollback { " (rollback)" } else { "" }
            ));
        }
        store_job(step.job);
    }
    let _ = UartTx.set_line(line);
}

fn install(r: &Radio, bank: u8) -> Result<(), String> {
    let mut banks = r.banks.lock().unwrap();
    banks.verify(bank).map_err(|e: BankError| e.to_string())?;
    let mut src = banks.image(bank).map_err(|e| e.to_string())?;
    let cfg = Config { baud: r.baud, ..Config::default() };
    let mut lines = r.lines;
    let mut last_pct = u32::MAX;
    info!("radio: installing bank {} through the Gecko bootloader", bank_name(bank));
    let report = gecko::install(&mut UartRx, &mut UartTx, &mut lines, &mut src, &cfg, &mut |sent, total| {
        let pct = sent * 100 / total.max(1);
        if pct / 10 != last_pct / 10 {
            last_pct = pct;
            info!("radio: {pct} % ({sent}/{total} B)");
            *PROGRESS.lock().unwrap() = format!("bank {} {pct} %", bank_name(bank));
        }
    })
    .map_err(|e| e.to_string())?;
    PROGRESS.lock().unwrap().clear();
    info!(
        "radio: bootloader v{} accepted bank {} ({} blocks, {} retries, {:?})",
        report.menu.version,
        bank_name(bank),
        report.stats.blocks,
        report.stats.retries,
        report.elapsed
    );
    Ok(())
}

/// Verify a pending install once the hub runs.
pub fn start_monitor(hub: Hub) {
    let Some(r) = RADIO.get() else { return };
    let Some(Job::Verify { bank, rollback }) = job() else { return };
    let spawned = std::thread::Builder::new().name("radio-verify".into()).stack_size(6144).spawn(move || {
        let start = Instant::now();
        let slot = r.banks.lock().unwrap().slot(bank).ok().flatten();
        let mut result = None;
        while start.elapsed() < VERIFY_TIMEOUT {
            if let (Some(s), Some(slot)) = (hub.status().ncp, slot.as_ref()) {
                let id = job::identity(slot, &s.negotiated.build_id);
                info!("radio: NCP fw {:?} build {:02X?}: {id:?}", s.negotiated.fw, s.negotiated.build_id);
                result = Some(id);
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let ok = matches!(result, Some(Identity::Match | Identity::Unverifiable));
        let mut banks = r.banks.lock().unwrap();
        let lkg = banks.last_known_good().ok().flatten().map(|(i, _)| i);
        let step = job::next(Job::Verify { bank, rollback }, if ok { Outcome::Verified } else { Outcome::VerifyFailed }, lkg);
        if let Some((i, st)) = step.mark {
            let _ = banks.set_state(i, st);
        }
        if let Some(i) = step.promote {
            let _ = banks.promote(i);
        }
        drop(banks);
        let what = match result {
            None => "no ZRHP session".to_string(),
            Some(id) => format!("{id:?}"),
        };
        match (ok, rollback) {
            (true, false) => last(&format!("ok: bank {} verified ({what}), now last-known-good", bank_name(bank))),
            (true, true) => last(&format!("failed: update rolled back to bank {} ({what})", bank_name(bank))),
            (false, _) => last(&format!("failed: bank {} not verified ({what})", bank_name(bank))),
        }
        store_job(step.job);
        if step.job.is_some() {
            warn!("radio: rolling back, restarting the host");
            hub.shutdown();
            crate::restart();
        }
    });
    if let Err(e) = spawned {
        error!("radio: verify thread: {e}");
    }
}

/// Re-check bank `i` (console).
pub fn check(i: u8) -> String {
    let Some(r) = RADIO.get() else {
        return "no radio banks on this board/layout\r\n".into();
    };
    match r.banks.lock().unwrap().verify(i) {
        Ok(info) => format!(
            "bank {}: valid GBL, {} B, app {:?}, zigbee-rs {:?}, signed {}, encrypted {}\r\n",
            bank_name(i),
            info.size,
            info.app.map(|a| a.version),
            info.zrhp.as_ref().map(|m| &m.fw),
            info.signed,
            info.encrypted()
        ),
        Err(e) => format!("bank {}: {e}\r\n", bank_name(i)),
    }
}
