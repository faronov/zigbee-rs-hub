//! The MG24's firmware and the backend the hub runs on it (Dongle-M).
//!
//! **Backend at boot** ([`boot_detect`]): the setting `ncp_backend` (NVS
//! `zrh`: `auto`, the default, `ezsp` or `zrhp`). `auto` asks the NCP
//! (`hub_app::backend::select`): the backend seen last (`ncp_seen`) first,
//! then the other, then the Gecko bootloader's menu; the bootloader first
//! when the last update did not finish. A bootloader found starts the last
//! backend with its UART parked (nothing reaches the menu): the hub runs
//! and offers to flash again; it never boot-loops on it.
//!
//! **Image** (`ncp/update/begin`, `chunk`, `end`, as `firmware/*`): a GBL
//! uploaded into the `radio` area of the claimed `files` region (640 KiB),
//! validated while it arrives (`hub_core::gecko::gbl`: no bootloader or
//! Secure Engine upgrade, nothing below the application base 0x08006000,
//! no encryption or compression, the end tag's CRC) and described in NVS
//! (`ncp_image`).
//!
//! **Flash** (`ncp/update/start`, `"confirm": true`; `foreign` for any
//! image but a ZRHP NCP application: SONOFF's EmberZNet, an NV clear for
//! the way back; `force` without a backup; `restore`, default true), on a
//! thread:
//!
//! 1. the stored image read back and validated again (its SHA-256 too);
//! 2. any network the NCP holds (up, or formed and not up) backed up with
//!    keys and kept hub-scoped (`network/backup` with `keep`): a new NCP
//!    firmware starts without a network, so the backup is what carries it
//!    over. A failed backup refuses the flash, as an NCP that does not
//!    answer or the bring-up boot (nothing known, nothing backed up) do,
//!    unless `force` or a network kept by an earlier update
//!    (`select::before_flash`);
//! 3. the state `flashing` persisted (`ncp_update`), the UART taken from
//!    the backend (parked), the MG24 reset into its bootloader (GPIO12 high
//!    across a GPIO15 pulse), the menu it prints taken, `1`, the image over
//!    XMODEM-CRC, the bootloader's verdict; up to 3 attempts, each from a
//!    fresh bootloader entry. A menu that never comes changes nothing (the
//!    MG24 is reset into its application, the UART given back). A failed
//!    upload leaves the MG24 in its bootloader (which survives every
//!    upload) and the UART parked: flash again, or another image;
//! 4. `2` (run), the new firmware asked for (ZRHP: its `build_id` and
//!    version must be the image's ZRHP record; EZSP: its version; after a
//!    maintenance image without an application, the application that was
//!    there), the NCP seen remembered (`ncp_seen`), and the hub restarts on
//!    it (the registry saved first).
//!
//! A power cut during the flash leaves `flashing` in NVS: the next boot
//! calls it `interrupted`, looks for the bootloader first and offers to
//! flash the stored image again.
//!
//! **Crash guard** (`select::ZrhpGuard`, NVS `zrh/ncp_zguard`): a ZRHP
//! code path (the probe, at boot or after a flash, and the ZRHP backend's
//! start until its session is up and 10 s later, at most a minute) runs
//! armed; a boot that finds it armed after a panic or watchdog reset
//! counts a crash. Only a boot whose ZRHP path ran through ends the
//! streak (not a probe that answered: the backend after it may crash). At
//! `select::ZRHP_CRASH_LIMIT` crashes in a row the boot skips every ZRHP
//! path and starts EZSP with the UART parked, so nothing ZRHP can
//! boot-loop the hub; after 10 minutes without a crash the next boot tries
//! ZRHP once more. Another hub firmware starts afresh, and
//! `ncp/update/reset`, `ncp/backend/set` and a new NCP image end the
//! streak; the check of a freshly flashed firmware always runs. The
//! detection runs on a thread of its own (16 KiB), not on the 8 KiB main
//! task.
//!
//! **After a switch** ([`after_start`]): on the new backend, once its NCP
//! session is up, the kept network goes onto the NCP (`network/restore`
//! with `confirm`) when the NCP has none and takes the backup's
//! coordinator IEEE (`select::after_switch`); otherwise the reason is
//! shown and `ncp/update/restore` retries. A maintenance image (an NV
//! clear) restores nothing, automatically or by hand: the kept network
//! waits for the next application image (`restore: held`). The devices keep working
//! without pairing again: same network key, PAN, channel, frame counters
//! from the backup's plus 10 000, the same Trust Center IEEE.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use base64::Engine;
use hub_app::backend::select::{
    self, AfterSwitch, BeforeFlash, Detection, Found, GuardCell, GuardedProber, Probe, ProbeTimes, Prober, Setting, ZrhpGuard,
};
use hub_app::{App, SessionSink};
use hub_coord::BackendKind;
use hub_core::gecko::gbl::{GblInfo, GblParser, ImageKind, Policy};
use hub_core::gecko::{self, xmodem};
use hub_core::layout::dongle_files as layout;
use hub_core::profile::BoardProfile;
use hub_devices::hub::ApiError;
use hub_store::{Flash, Gated};
use log::{error, info, warn};
use serde_json::{Value as J, json};
use sha2::{Digest, Sha256};
use zrh_espapp::flash::{Region, SECTOR};

use crate::gecko::GeckoPort;
use crate::{ezsp, files, lines, settings, uart, zrhp};

/// NVS: the backend setting.
const SETTING_KEY: &str = "ncp_backend";
/// NVS: the backend whose NCP answered last.
const SEEN_KEY: &str = "ncp_seen";
/// NVS: the update's state.
const STATE_KEY: &str = "ncp_update";
/// NVS: the stored image's description.
const IMAGE_KEY: &str = "ncp_image";
/// Largest NVS value read back.
const NVS_MAX: usize = 2048;
/// Largest decoded upload chunk.
pub const CHUNK_MAX: usize = 4096;
/// Uploads (and restores) after the flash attempts.
const ATTEMPTS: u32 = 3;

/// Images known by their SHA-256 (the files of `docs/dongle-max.md`,
/// "Replacing the MG24 firmware"): named in the status; any image but a
/// ZRHP NCP application is flashed only with `foreign` too.
const KNOWN_IMAGES: [(&str, &str); 3] = [
    (
        "44b81c1ecf81032b98d625388ae44c10d7c8a6abd1dcea10b2e2d934d7a311fa",
        "SONOFF's stock EmberZNet 7.4.5 NCP for the Dongle-M (zigbee_ncp_uart_hw_donglem_v7.4.5.gbl)",
    ),
    (
        "59a8130a2be7e7d967b627747492dd5db3dcf57df2d91d2db1e928ba223a0e9a",
        "Nerivec's NVM3 clear for the EFR32MG24A420F1536IM48, 32 KiB (0x08176000..0x0817E000)",
    ),
    (
        "06b7e7e2f6718cd2e1ecfd2f1f7e9121462a49fac6b9261e986149834ea4f9c6",
        "Nerivec's NVM3 clear for the EFR32MG24A420F1536IM48, 40 KiB (0x08176000..0x08180000)",
    ),
];

/// What the boot decided.
static DETECTION: OnceLock<Detection> = OnceLock::new();

/// NVS: the crash guard of the ZRHP code paths (`select::ZrhpGuard`).
const GUARD_KEY: &str = "ncp_zguard";
/// ZRHP is skipped in this boot (the crash guard tripped).
static ZRHP_SKIPPED: AtomicBool = AtomicBool::new(false);
/// One change of the guard at a time.
static GUARD_LOCK: Mutex<()> = Mutex::new(());

/// The guard in NVS: every change stored before the code it guards runs.
struct NvsGuard;

impl GuardCell for NvsGuard {
    fn update(&self, f: &mut dyn FnMut(&mut ZrhpGuard)) {
        let _one = GUARD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let before = guard();
        let mut g = before.clone();
        f(&mut g);
        if g != before {
            store(GUARD_KEY, &g.to_json());
        }
    }
}

fn guard() -> ZrhpGuard {
    ZrhpGuard::from_json(&load(GUARD_KEY))
}

/// The running hub firmware (its ELF SHA-256, as `esp_app_desc_t` has
/// it): another one starts the crash guard afresh.
fn firmware_id() -> String {
    hex(&unsafe { &*esp_idf_svc::sys::esp_app_get_description() }.app_elf_sha256[..8])
}

/// The last boot ended in a panic or a watchdog reset.
fn crash_reset() -> bool {
    use esp_idf_svc::sys;
    matches!(
        unsafe { sys::esp_reset_reason() },
        sys::esp_reset_reason_t_ESP_RST_PANIC
            | sys::esp_reset_reason_t_ESP_RST_INT_WDT
            | sys::esp_reset_reason_t_ESP_RST_TASK_WDT
            | sys::esp_reset_reason_t_ESP_RST_WDT
    )
}

/// The operator (`ncp/update/reset`, `ncp/backend/set`) and a new NCP
/// image end the crash streak.
fn clear_guard() {
    NvsGuard.update(&mut ZrhpGuard::end_streak);
}

/// The ZRHP backend starts: guarded until its session is up and
/// `select::ZRHP_BACKEND_SETTLE` later (its first exchanges), at most
/// `select::ZRHP_BACKEND_WINDOW`; then the boot's ZRHP path ran through.
pub fn guard_backend_start() {
    NvsGuard.update(&mut ZrhpGuard::arm);
    let spawned = std::thread::Builder::new().name("ncp-guard".into()).stack_size(6144).spawn(|| {
        let t0 = Instant::now();
        let left = || select::ZRHP_BACKEND_WINDOW.saturating_sub(t0.elapsed());
        let app = loop {
            if let Some(a) = crate::hub::APP.get() {
                break Some(a.clone());
            }
            if left().is_zero() {
                break None;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        if let Some(app) = app
            && app.wait(left(), |s| s.session_up)
        {
            std::thread::sleep(select::ZRHP_BACKEND_SETTLE.min(left()));
        }
        NvsGuard.update(&mut ZrhpGuard::end_streak);
    });
    if spawned.is_err() {
        // Not counted, not ended.
        NvsGuard.update(&mut ZrhpGuard::disarm);
    }
}

/// A boot that skips ZRHP: once it ran `select::ZRHP_RETRY_AFTER` without
/// a crash, the next boot tries ZRHP once more.
fn guard_retry_later() {
    let spawned = std::thread::Builder::new().name("ncp-guard".into()).stack_size(6144).spawn(|| {
        std::thread::sleep(select::ZRHP_RETRY_AFTER);
        NvsGuard.update(&mut ZrhpGuard::clean_uptime);
        info!("ncp: no crash in {} min with ZRHP skipped: the next boot tries ZRHP once more", select::ZRHP_RETRY_AFTER.as_secs() / 60);
    });
    if let Err(e) = spawned {
        warn!("ncp: the crash guard's retry timer: {e}");
    }
}

fn api(code: &str, msg: impl Into<String>) -> ApiError {
    ApiError::new(code, msg)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn kind_of(s: &str) -> Option<BackendKind> {
    match s {
        "ezsp" => Some(BackendKind::Ezsp),
        "zrhp" => Some(BackendKind::Zrhp),
        _ => None,
    }
}

/// The image policy of the Dongle-M's MG24 and the `radio` area.
fn policy() -> Policy {
    Policy::mg24_1536k(layout::RADIO.size)
}

fn load(key: &str) -> J {
    settings::get_long(key, NVS_MAX).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(J::Null)
}

fn store(key: &str, v: &J) {
    let text = v.to_string();
    if let Err(e) = settings::set(key, if v.is_null() { None } else { Some(&text) }) {
        warn!("ncp: {key} not stored: {e}");
    }
}

/// The update's persisted state (`J::Null`: none).
fn state() -> J {
    load(STATE_KEY)
}

fn save_state(v: &J) {
    store(STATE_KEY, v);
}

/// The stored image's description.
fn stored_image() -> Option<J> {
    Some(load(IMAGE_KEY)).filter(|v| v.is_object())
}

/// The backend setting.
pub fn setting() -> Setting {
    settings::get(SETTING_KEY).and_then(|s| Setting::parse(&s)).unwrap_or_default()
}

/// The backend this boot runs (`None`: no hub).
pub fn running() -> Option<BackendKind> {
    DETECTION.get().map(|d| d.backend)
}

/// A random non-zero ZRHP host cookie.
fn cookie() -> u32 {
    let mut b = [0u8; 4];
    while u32::from_le_bytes(b) == 0 {
        crate::entropy::fill(&mut b);
    }
    u32::from_le_bytes(b)
}

/// Asks the MG24 over the UART (the caller has it).
struct UartProber {
    ezsp_baud: u32,
}

impl Prober for UartProber {
    fn reset_to_app(&mut self) {
        lines::reset_to_app();
    }

    fn probe(&mut self, p: Probe) -> Result<Found, String> {
        let t0 = Instant::now();
        let r = match p {
            Probe::Ezsp => {
                uart::set_baud(self.ezsp_baud);
                select::probe_ezsp(ezsp::Port, hub_ezsp::SystemClock::default(), ProbeTimes::default())
            }
            Probe::Zrhp => {
                select::probe_zrhp(zrhp::Rx(zrhp::Gate::Probe), zrhp::Tx(zrhp::Gate::Probe), zrhp::LINES.to_vec(), cookie(), ProbeTimes::default())
            }
            Probe::Bootloader => {
                uart::set_baud(gecko::BAUD);
                match gecko::probe(&mut GeckoPort::default()) {
                    Ok(Some(m)) => Ok(Found::Bootloader { version: m.version.map(|(a, b, c)| format!("{a}.{b:02}.{c:02}")) }),
                    Ok(None) => Err("no bootloader menu".into()),
                    Err(e) => Err(e.0.into()),
                }
            }
        };
        info!("ncp: probe {}: {} ({} ms)", p.as_str(), r.as_ref().map_or_else(|e| e.clone(), Found::describe), t0.elapsed().as_millis());
        r
    }
}

/// The UART is someone else's: nothing is asked.
struct Busy(uart::User);

impl Prober for Busy {
    fn reset_to_app(&mut self) {
        lines::reset_to_app();
    }

    fn probe(&mut self, _: Probe) -> Result<Found, String> {
        Err(format!("the MG24 UART is in use ({:?})", self.0))
    }
}

/// Stack of the boot's detection thread: the EZSP probe keeps its host on
/// the stack (the main task has 8 KiB).
const DETECT_STACK: usize = 16 * 1024;

/// At boot, before the hub takes the UART: the backend to start (on a
/// thread of its own, joined).
pub fn boot_detect(board: &BoardProfile) -> Detection {
    let b = *board;
    let run = std::thread::Builder::new().name("ncp-detect".into()).stack_size(DETECT_STACK).spawn(move || detect_now(&b));
    match run.map(|h| h.join()) {
        Ok(Ok(d)) => d,
        Ok(Err(_)) | Err(_) => {
            // No probe ran (or it panicked): the last backend, parked.
            error!("ncp: the backend detection failed; the UART stays parked");
            lines::reset_to_app();
            uart::park_from_boot();
            let backend = settings::get(SEEN_KEY).and_then(|s| kind_of(&s)).unwrap_or(BackendKind::Ezsp);
            let d = Detection { setting: setting(), backend, found: None, tried: Vec::new(), park: true, why: "the backend detection failed" };
            let _ = DETECTION.set(d.clone());
            d
        }
    }
}

fn detect_now(board: &BoardProfile) -> Detection {
    let setting = setting();
    let hint = settings::get(SEEN_KEY).and_then(|s| kind_of(&s));
    let mut st = state();
    let phase = st["phase"].as_str().unwrap_or("idle").to_string();
    if phase == "flashing" {
        st["phase"] = "interrupted".into();
        st["message"] = "the hub restarted during the MG24 update (a power cut or a reset): the MG24 may wait in its bootloader; flash the stored image again".into();
        save_state(&st);
        warn!("ncp: the last MG24 update was interrupted");
    }
    let suspect = matches!(phase.as_str(), "flashing" | "interrupted" | "failed") && st["in_bootloader"] != false;
    // Under the crash guard: a ZRHP path that crashed the last boots does
    // not run again.
    let last = guard();
    let (crashed, fw) = (crash_reset(), firmware_id());
    let (d, skip) = match uart::claim(uart::User::Updater) {
        Ok(_claim) => {
            select::detect_guarded(setting, hint, suspect, &mut UartProber { ezsp_baud: board.uart.line.baud }, &NvsGuard, crashed, &fw)
        }
        Err(u) => select::detect_guarded(setting, hint, suspect, &mut Busy(u), &NvsGuard, crashed, &fw),
    };
    ZRHP_SKIPPED.store(skip, Ordering::Relaxed);
    if last.armed && crashed && last.firmware == fw {
        warn!("ncp: a ZRHP path crashed the last boot ({} in a row)", last.crashes.saturating_add(1));
    }
    if skip {
        error!("ncp: {}", select::ZRHP_GUARD_WHY);
        guard_retry_later();
    }
    uart::set_baud(board.uart.line.baud);
    if let Some(b) = d.found.as_ref().and_then(Found::backend)
        && hint != Some(b)
        && let Err(e) = settings::set(SEEN_KEY, Some(b.as_str()))
    {
        warn!("ncp: {SEEN_KEY} not stored: {e}");
    }
    if st.is_object() && matches!(phase.as_str(), "flashing" | "interrupted" | "failed") {
        st["in_bootloader"] = d.park.into();
        // The flash may have completed (cut short while the new firmware
        // was asked): an application answers, the kept network follows.
        let app = d.found.as_ref().and_then(Found::backend).is_some();
        if let Some(r) = select::restore_after_interruption(st["restore"].as_str().unwrap_or_default(), app) {
            st["restore"] = r.into();
        }
        save_state(&st);
    }
    if d.park {
        uart::park_from_boot();
        error!("ncp: the MG24 waits in its Gecko bootloader: the hub runs without its NCP; flash the image again (ncp/update/start, console `ncp flash confirm`)");
    }
    info!(
        "ncp: backend {} ({}, setting {}){}",
        d.backend,
        d.why,
        d.setting.as_str(),
        d.found.as_ref().map_or(String::new(), |f| format!(": {}", f.describe()))
    );
    let _ = DETECTION.set(d.clone());
    d
}

// ---- the image store ----

struct Upload {
    region: Gated<Region>,
    size: u32,
    written: u32,
    erased: u32,
    sha: Sha256,
    expect: Option<[u8; 32]>,
    parser: GblParser,
}

static UPLOAD: Mutex<Option<Upload>> = Mutex::new(None);

fn unhex32(s: &str) -> Option<[u8; 32]> {
    let v: Option<Vec<u8>> = (0..32).map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()).collect();
    v?.try_into().ok()
}

/// `0xMMmmppbb` as `M.m.p.b` (EmberZNet's application version form).
fn version_text(v: u32) -> String {
    format!("{}.{}.{}.{}", v >> 24, (v >> 16) & 0xFF, (v >> 8) & 0xFF, v & 0xFF)
}

fn image_json(info: &GblInfo, sha: &[u8; 32]) -> J {
    json!({
        "size": info.size,
        "sha256": hex(sha),
        "crc32": format!("{:08x}", info.crc32),
        "kind": info.kind().as_str(),
        "fw": info.zrhp.map(|z| z.fw().to_string()),
        "build_id": info.zrhp.map(|z| hex(&z.build_id)),
        "app_type": info.app.map(|a| a.app_type),
        "app_version": info.app.map(|a| version_text(a.version)),
        "program": format!("0x{:08X}..0x{:08X}", info.prog_start, info.prog_end),
        "signed": info.signed,
        "known": KNOWN_IMAGES.iter().find(|(h, _)| *h == hex(sha)).map(|(_, name)| *name),
    })
}

fn flashing() -> bool {
    JOB.lock().unwrap().running
}

fn begin(req: &J) -> Result<J, ApiError> {
    if !files::in_use() {
        return Err(api("unsupported", "the MG24 image store is in the claimed files region; the hub's data are in RAM only (storage/status)"));
    }
    if flashing() {
        return Err(api("busy", "the MG24 is being flashed"));
    }
    let size = req["size"].as_u64().ok_or_else(|| api("invalid_format", "size missing"))?;
    let expect = match req["sha256"].as_str() {
        Some(h) => Some(unhex32(h).ok_or_else(|| api("invalid_value", "sha256: 64 hex digits"))?),
        None => None,
    };
    if size < 64 || size > u64::from(layout::RADIO.size) {
        return Err(api("too_large", format!("size {size}: the MG24 image store holds {} bytes", layout::RADIO.size)));
    }
    let region = files::radio().ok_or_else(|| api("unsupported", "no radio area in the files region"))?;
    store(IMAGE_KEY, &J::Null);
    *UPLOAD.lock().unwrap() =
        Some(Upload { region, size: size as u32, written: 0, erased: 0, sha: Sha256::new(), expect, parser: GblParser::new(policy()) });
    info!("ncp: MG24 image upload of {size} B begins");
    Ok(json!({"chunk": CHUNK_MAX, "store": layout::RADIO.size}))
}

fn chunk(req: &J) -> Result<J, ApiError> {
    let offset = req["offset"].as_u64().ok_or_else(|| api("invalid_format", "offset missing"))?;
    let data = req["data"].as_str().ok_or_else(|| api("invalid_format", "data missing"))?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| api("invalid_value", format!("data: {e}")))?;
    let mut guard = UPLOAD.lock().unwrap();
    let up = guard.as_mut().ok_or_else(|| api("not_found", "no upload in progress (ncp/update/begin)"))?;
    if offset != u64::from(up.written) {
        return Err(api("invalid_value", format!("offset {offset}: expected {}", up.written)));
    }
    if bytes.is_empty() || bytes.len() > CHUNK_MAX || up.written as usize + bytes.len() > up.size as usize {
        return Err(api("invalid_value", format!("chunk of {} B at {offset} of {}", bytes.len(), up.size)));
    }
    if let Err(e) = up.parser.feed(&bytes) {
        *guard = None;
        return Err(api(e.code(), format!("not an image for the MG24: {e} (upload aborted)")));
    }
    let end = up.written + bytes.len() as u32;
    let written = (|| {
        while up.erased < end {
            up.region.erase(up.erased / SECTOR)?;
            up.erased += SECTOR;
        }
        up.region.write(up.written, &bytes)
    })();
    if let Err(e) = written {
        *guard = None;
        return Err(api("failed", format!("flash: {e} (upload aborted)")));
    }
    up.sha.update(&bytes);
    up.written = end;
    Ok(json!({"written": end}))
}

fn end() -> Result<J, ApiError> {
    let up = UPLOAD.lock().unwrap().take().ok_or_else(|| api("not_found", "no upload in progress (ncp/update/begin)"))?;
    if up.written != up.size {
        return Err(api("invalid_value", format!("{} of {} B written (upload aborted)", up.written, up.size)));
    }
    let digest: [u8; 32] = up.sha.finalize().into();
    if up.expect.is_some_and(|e| e != digest) {
        return Err(api("verification_failed", "SHA-256 of the upload differs (upload aborted)"));
    }
    let info = up.parser.finish().map_err(|e| api(e.code(), format!("not an image for the MG24: {e}")))?;
    let image = image_json(&info, &digest);
    store(IMAGE_KEY, &image);
    info!("ncp: MG24 image stored: {image}");
    let mut r = image;
    r["note"] = match info.kind() {
        ImageKind::Zrhp => "a zigbee-rs ZRHP NCP image: ncp/update/start flashes it",
        ImageKind::Other => "not a zigbee-rs ZRHP image (EmberZNet or another firmware): ncp/update/start needs foreign",
        ImageKind::Maintenance => {
            "a maintenance image without an application (e.g. an NV clear): the MG24's application stays; ncp/update/start needs foreign"
        }
    }
    .into();
    if info.signed {
        r["note"] = format!("{}; signed: a bootloader without its signing key refuses it", r["note"].as_str().unwrap_or_default()).into();
    }
    Ok(r)
}

/// Reads the stored image back: valid, and the upload's SHA-256.
fn verify_stored(image: &J) -> Result<GblInfo, String> {
    let size = image["size"].as_u64().ok_or("no stored image")? as u32;
    let mut region = files::radio().ok_or("no radio area in the files region")?;
    let mut parser = GblParser::new(policy());
    let mut sha = Sha256::new();
    let mut buf = vec![0u8; SECTOR as usize];
    let mut at = 0;
    while at < size {
        let n = (size - at).min(SECTOR) as usize;
        region.read(at, &mut buf[..n]).map_err(|e| format!("flash: {e}"))?;
        parser.feed(&buf[..n]).map_err(|e| format!("the stored image is damaged: {e}"))?;
        sha.update(&buf[..n]);
        at += n as u32;
    }
    let info = parser.finish().map_err(|e| format!("the stored image is damaged: {e}"))?;
    let digest: [u8; 32] = sha.finalize().into();
    if image["sha256"].as_str() != Some(hex(&digest).as_str()) {
        return Err("the stored image differs from the upload (SHA-256): upload it again".into());
    }
    Ok(info)
}

// ---- the in-process API session ----

struct ChanSink(mpsc::Sender<String>);

impl SessionSink for ChanSink {
    fn send(&mut self, text: &str) -> bool {
        self.0.send(text.to_string()).is_ok()
    }
    fn send_owned(&mut self, text: String) -> bool {
        self.0.send(text).is_ok()
    }
}

/// A trusted in-process session of the app's API (backup, restore, reboot).
struct Local {
    app: App,
    sid: u64,
    rx: mpsc::Receiver<String>,
    next: u64,
}

impl Local {
    fn open() -> Result<Local, String> {
        let app = crate::hub::APP.get().cloned().ok_or("the hub is not running")?;
        let (tx, rx) = mpsc::channel();
        let sid = app.connect_trusted(Box::new(ChanSink(tx))).ok_or("the hub refused a session")?;
        Ok(Local { app, sid, rx, next: 1 })
    }

    /// `ty` with `body`: its result, or `code: message`.
    fn call(&mut self, ty: &str, mut body: J, timeout: Duration) -> Result<J, String> {
        let id = 0x4E43_0000 + self.next;
        self.next += 1;
        body["id"] = id.into();
        body["type"] = ty.into();
        self.app.text(self.sid, body.to_string());
        let until = Instant::now() + timeout;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let text = self.rx.recv_timeout(left).map_err(|_| format!("{ty}: no answer within {} s", timeout.as_secs()))?;
            if !text.contains("\"result\"") {
                continue;
            }
            let Ok(mut v) = serde_json::from_str::<J>(&text) else { continue };
            if v["type"] != "result" || v["id"] != id {
                continue;
            }
            if v["success"] == true {
                return Ok(v["result"].take());
            }
            return Err(format!("{}: {}", v["error"]["code"].as_str().unwrap_or("error"), v["error"]["message"].as_str().unwrap_or("")));
        }
    }
}

impl Drop for Local {
    fn drop(&mut self) {
        self.app.disconnect(self.sid);
    }
}

// ---- the flash ----

/// The flash under way (or the last one in this boot).
#[derive(Debug, Clone, Default)]
struct Job {
    running: bool,
    phase: &'static str,
    attempt: u32,
    sent: u32,
    total: u32,
    message: String,
}

static JOB: Mutex<Job> = Mutex::new(Job { running: false, phase: "", attempt: 0, sent: 0, total: 0, message: String::new() });

fn phase(p: &'static str) {
    info!("ncp: {p}");
    let mut j = JOB.lock().unwrap();
    j.phase = p;
}

/// `ncp/update/start` options.
#[derive(Debug, Clone, Copy)]
pub struct StartOpts {
    /// Flash an image without the ZRHP record.
    pub foreign: bool,
    /// Without a backup of the running network.
    pub force: bool,
    /// Restore the kept network on the new backend.
    pub restore: bool,
}

/// Starts the flash of the stored image on its own thread.
pub fn start(o: StartOpts) -> Result<J, ApiError> {
    let image = stored_image().ok_or_else(|| api("not_found", "no MG24 image stored: upload one first (ncp/update/begin, chunk, end)"))?;
    if UPLOAD.lock().unwrap().is_some() {
        return Err(api("busy", "an image upload is in progress"));
    }
    if image["kind"] != "zrhp" && !o.foreign {
        return Err(api(
            "confirm_required",
            "the stored image is not a zigbee-rs ZRHP NCP application (EmberZNet, another firmware, or a maintenance image such as an \
             NV clear): after it the hub runs on what the MG24 answers. Send again with \"foreign\": true.",
        ));
    }
    {
        let mut j = JOB.lock().unwrap();
        if j.running {
            return Err(api("busy", "the MG24 is being flashed"));
        }
        *j = Job { running: true, phase: "starting", ..Job::default() };
    }
    let spawned = std::thread::Builder::new().name("ncp-flash".into()).stack_size(16 * 1024).spawn(move || {
        let r = job(&image, o);
        let mut j = JOB.lock().unwrap();
        j.running = false;
        match r {
            Ok(m) => {
                info!("ncp: {m}");
                j.phase = "done";
                j.message = m;
            }
            Err(m) => {
                error!("ncp: MG24 update failed: {m}");
                j.phase = "failed";
                j.message = m;
            }
        }
    });
    if let Err(e) = spawned {
        JOB.lock().unwrap().running = false;
        return Err(api("failed", format!("thread: {e}")));
    }
    Ok(json!({"state": "started"}))
}

fn job(image: &J, o: StartOpts) -> Result<String, String> {
    phase("checking the stored image");
    let info = verify_stored(image)?;
    // Up to the end tag: padding after it would be refused by the
    // bootloader in a packet of its own.
    let size = info.gbl_len;
    JOB.lock().unwrap().total = size;

    // The network carried over: a backup kept hub-scoped of any network
    // the NCP holds (up, or formed and not up); a network kept by an
    // earlier update and not restored yet stays the one to restore (a
    // retry after a failed upload; the second image of a rollback, after
    // which the NCP runs no network to back up).
    let mut backup_epid = select::carried(&state());
    let net = match crate::hub::APP.get() {
        Some(_) => Some(Local::open()?.call("network/info", json!({}), Duration::from_secs(10))?),
        None => None,
    };
    match select::before_flash(net.as_ref(), backup_epid.is_some(), o.force) {
        BeforeFlash::Refuse(why) => return Err(why),
        BeforeFlash::Skip => {}
        BeforeFlash::Backup { required } => {
            phase("backing up the network");
            let r = Local::open()?.call("network/backup", json!({"keys": true, "keep": true}), Duration::from_secs(120));
            if let Some(epid) = select::backed_up(required, r)? {
                info!("ncp: network {epid} backed up and kept");
                backup_epid = Some(epid);
            }
        }
    }
    let mut region = files::radio().ok_or("no radio area in the files region")?;
    let lease = uart::lease().map_err(|u| format!("the MG24 UART is in use ({u:?})"))?;
    let from = running();
    let maintenance = info.kind() == ImageKind::Maintenance;
    // A maintenance image (an NV clear) restores nothing: the firmware that
    // stays is often the one being replaced, and on the way back the
    // zigbee-rs NCP's NV overlaps EmberZNet's NVM3. The kept network waits
    // for the next application image.
    let restore = select::restore_at_start(o.restore, backup_epid.is_some(), maintenance);
    let mut st = json!({
        "phase": "flashing",
        "image": image,
        "from": from.map(BackendKind::as_str),
        "backup_epid": backup_epid,
        "restore": restore,
        "attempts": 0,
    });
    save_state(&st);

    let mut port = GeckoPort::default();
    let mut touched = false;
    let mut last = String::new();
    let mut done = None;
    for attempt in 1..=ATTEMPTS {
        st["attempts"] = attempt.into();
        save_state(&st);
        {
            let mut j = JOB.lock().unwrap();
            j.attempt = attempt;
            j.sent = 0;
        }
        phase("entering the bootloader");
        uart::set_baud(gecko::BAUD);
        uart::flush_input();
        if let Err(e) = lines::enter_bootloader() {
            last = format!("bootloader entry: {e}");
            break;
        }
        let menu = match gecko::await_menu(&mut port) {
            Ok(m) => m,
            Err(e) => {
                last = format!("{e} after the bootloader entry");
                warn!("ncp: attempt {attempt}: {last}");
                continue;
            }
        };
        info!("ncp: Gecko bootloader {:?}{}", menu.version, if menu.was_receiving { " (an upload left waiting was cancelled)" } else { "" });
        touched = true;
        phase("uploading");
        let mut src = |off: u32, buf: &mut [u8]| region.read(off, buf).is_ok();
        let mut progress = |sent: u32, _len: u32| JOB.lock().unwrap().sent = sent;
        match gecko::upload(&mut port, size, &mut src, &mut progress, &xmodem::Config::default()) {
            Ok(u) => {
                info!("ncp: uploaded in {} blocks ({} sent again)", u.xmodem.blocks, u.xmodem.resent);
                done = Some(u);
                break;
            }
            Err(e) => {
                last = e.to_string();
                warn!("ncp: attempt {attempt}: {last}");
            }
        }
    }
    if done.is_none() {
        st["phase"] = "failed".into();
        if !touched {
            // The menu never came: nothing was sent to the MG24.
            lines::reset_to_app();
            lease.give_back();
            st["in_bootloader"] = false.into();
            st["message"] = format!("{last}; nothing was written, the MG24 was reset into its application").into();
            save_state(&st);
            return Err(st["message"].as_str().unwrap_or_default().to_string());
        }
        // The bootloader survives every failed upload; nothing of the
        // backend's reaches it (the UART stays parked).
        drop(lease);
        st["in_bootloader"] = true.into();
        st["message"] =
            format!("{last}: the MG24 waits in its bootloader; flash the image again (ncp/update/start) or another one").into();
        save_state(&st);
        return Err(st["message"].as_str().unwrap_or_default().to_string());
    }

    // A new NCP firmware: the crashes of the old one's ZRHP path no longer
    // count (its check below runs armed).
    clear_guard();
    phase("starting the new firmware");
    if gecko::run(&mut port).is_err() {
        lines::reset_to_app();
    }
    std::thread::sleep(Duration::from_millis(1500));
    phase("asking the firmware");
    let kind = info.kind();
    let found = verify_running(kind, from);
    let detected = found.as_ref().ok().and_then(Found::backend);
    let verified = match (&found, kind, info.zrhp) {
        (Ok(Found::Zrhp { fw, build_id, .. }), ImageKind::Zrhp, Some(z)) => *build_id == z.build_id && fw.as_bytes() == z.fw_bytes(),
        (Ok(Found::Ezsp { .. }), ImageKind::Other, _) => true,
        // No new firmware: the application that was there answers.
        (Ok(_), ImageKind::Maintenance, _) => detected.is_some(),
        _ => false,
    };
    if let Some(b) = detected
        && let Err(e) = settings::set(SEEN_KEY, Some(b.as_str()))
    {
        warn!("ncp: {SEEN_KEY} not stored: {e}");
    }
    let said = found.as_ref().map_or_else(|e| e.clone(), Found::describe);
    st["phase"] = "done".into();
    st["in_bootloader"] = false.into();
    st["found"] = said.clone().into();
    st["verified"] = verified.into();
    // Whether the kept network goes onto the NCP is decided once its
    // session is up (`select::after_switch`: only onto an NCP without a
    // network; nothing when it runs that network already).
    st["restore"] = select::restore_after_flash(restore).into();
    st["message"] = match (&found, verified, kind) {
        (Ok(_), true, ImageKind::Maintenance) => format!("written (no application in the image); the MG24's application runs: {said}"),
        (Err(_), _, ImageKind::Maintenance) => {
            format!("written (no application in the image); the MG24's application answers neither EZSP nor ZRHP ({said})")
        }
        (Ok(_), true, _) => format!("flashed and running: {said}"),
        (Ok(_), false, _) => format!("flashed; the NCP answers but not as the image says: {said}"),
        (Err(_), _, _) => format!("flashed; the new firmware answers neither EZSP nor ZRHP ({said})"),
    }
    .into();
    save_state(&st);
    phase("restarting the hub on the new firmware");
    restart_hub();
    Ok(st["message"].as_str().unwrap_or_default().to_string())
}

/// The firmware's answer after the flash: the image's protocol first; for
/// a maintenance image, the backend's that ran before.
fn verify_running(kind: ImageKind, from: Option<BackendKind>) -> Result<Found, String> {
    let zrhp_first = match kind {
        ImageKind::Zrhp => true,
        ImageKind::Other => false,
        ImageKind::Maintenance => from == Some(BackendKind::Zrhp),
    };
    let order = if zrhp_first { [Probe::Zrhp, Probe::Ezsp] } else { [Probe::Ezsp, Probe::Zrhp] };
    // Armed, never skipped: a crash counts, a guarded boot still checks
    // the firmware it just wrote.
    let mut uart = UartProber { ezsp_baud: crate::board().uart.line.baud };
    let mut prober = GuardedProber { inner: &mut uart, cell: &NvsGuard, skip: false };
    let mut why = String::new();
    for _ in 0..3 {
        for p in order {
            match prober.probe(p) {
                Ok(f) => return Ok(f),
                Err(e) => why = e,
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Err(why)
}

/// Restarts the hub (the app saves its registry first).
fn restart_hub() {
    if let Ok(mut l) = Local::open() {
        match l.call("hub/reboot", json!({}), Duration::from_secs(5)) {
            Ok(_) => {
                std::thread::sleep(Duration::from_secs(8));
                warn!("ncp: hub/reboot did not restart the hub");
            }
            Err(e) => warn!("ncp: hub/reboot: {e}"),
        }
    }
    crate::restart()
}

// ---- after a switch: the kept network onto the new NCP ----

/// The restore under way (`ncp/update/restore` and the boot's).
static RESTORING: Mutex<bool> = Mutex::new(false);

/// The hub started: the network kept before an NCP update goes onto the
/// new NCP when that update asked for it.
pub fn after_start() {
    if state()["restore"] == "pending" {
        spawn_restore();
    }
}

fn spawn_restore() -> bool {
    {
        let mut r = RESTORING.lock().unwrap();
        if *r {
            return false;
        }
        *r = true;
    }
    let spawned = std::thread::Builder::new().name("ncp-restore".into()).stack_size(12 * 1024).spawn(|| {
        let r = restore_kept();
        let mut st = state();
        match &r {
            Ok(m) => {
                info!("ncp: {m}");
                st["restore"] = "done".into();
                st["restore_message"] = m.clone().into();
            }
            Err(m) => {
                warn!("ncp: the kept network was not restored: {m}");
                st["restore"] = "failed".into();
                st["restore_message"] = m.clone().into();
            }
        }
        save_state(&st);
        *RESTORING.lock().unwrap() = false;
    });
    if spawned.is_err() {
        *RESTORING.lock().unwrap() = false;
    }
    spawned.is_ok()
}

fn restore_kept() -> Result<String, String> {
    let st = state();
    let epid = st["backup_epid"].as_str().ok_or("no network was kept before the update")?.to_string();
    let app = crate::hub::APP.get().cloned().ok_or("the hub is not running")?;
    if !app.wait(Duration::from_secs(120), |s| s.session_up) {
        return Err("the NCP's session did not come up within 2 minutes".into());
    }
    // The records' decision (foreign, another backend's) comes with the
    // first session.
    std::thread::sleep(Duration::from_secs(3));
    let mut l = Local::open()?;
    let info = l.call("network/info", json!({}), Duration::from_secs(10))?;
    let mut kept = l.call("network/backup/replaced", json!({"ext_pan_id": epid}), Duration::from_secs(30))?;
    let backup = kept["backup"].take();
    match select::after_switch(&info, &backup) {
        AfterSwitch::Done => Ok("the NCP runs the kept network already".into()),
        AfterSwitch::Refuse(why) => Err(why),
        AfterSwitch::Restore => {
            info!("ncp: restoring network {epid} onto the new NCP");
            let r = l.call("network/restore", json!({"backup": backup, "confirm": true}), Duration::from_secs(240))?;
            Ok(format!(
                "network {epid} restored onto the new NCP: PAN 0x{:04X}, channel {}, {} devices, NWK frame counter from {}",
                r["pan_id"].as_u64().unwrap_or(0),
                r["channel"],
                r["devices"],
                r["nwk_fc_start"]
            ))
        }
    }
}

// ---- status, requests ----

/// `ncp/update/status` (also in `hub/system` `ncp_update`).
pub fn status_json() -> J {
    let j = JOB.lock().unwrap().clone();
    let g = guard();
    let up = UPLOAD.lock().unwrap().as_ref().map(|u| json!({"written": u.written, "size": u.size}));
    let mut backend = DETECTION.get().map_or(J::Null, Detection::to_json);
    if backend.is_object() {
        backend["seen"] = settings::get(SEEN_KEY).into();
        backend["next_boot"] = setting().as_str().into();
        backend["uart_parked"] = uart::parked().into();
    }
    json!({
        "backend": backend,
        "image": stored_image(),
        "store": {"size": layout::RADIO.size, "available": files::in_use()},
        "upload": up,
        "job": {"running": j.running, "phase": j.phase, "attempt": j.attempt, "sent": j.sent, "total": j.total, "message": j.message},
        "update": state(),
        "restoring": *RESTORING.lock().unwrap(),
        "restore_offered": !flashing() && select::restore_offered(&state()),
        "zrhp_guard": {"crashes": g.crashes, "armed": g.armed, "skipped": ZRHP_SKIPPED.load(Ordering::Relaxed), "retry_next_boot": g.retry},
        "chunk": CHUNK_MAX,
    })
}

/// `ncp` on the console.
pub fn status_text() -> String {
    let v = status_json();
    let mut s = String::new();
    let b = &v["backend"];
    if b.is_object() {
        s += &format!(
            "NCP backend: {} ({}; setting {}, next boot {}){}\r\n",
            b["running"].as_str().unwrap_or("?"),
            b["why"].as_str().unwrap_or(""),
            b["setting"].as_str().unwrap_or("?"),
            b["next_boot"].as_str().unwrap_or("?"),
            if b["uart_parked"] == true { ", UART parked (the NCP updater has it, or the MG24 is in its bootloader)" } else { "" }
        );
        if let Some(f) = b["found"].as_str() {
            s += &format!("NCP found: {f}\r\n");
        }
    } else {
        s += "NCP backend: none in this boot\r\n";
    }
    match v["image"].as_object() {
        Some(i) => s += &format!("stored image: {}\r\n", J::Object(i.clone())),
        None => s += "stored image: none (upload one: ncp/update/begin, chunk, end)\r\n",
    }
    let j = &v["job"];
    if j["running"] == true {
        s += &format!("flashing: {} (attempt {}), {} of {} B\r\n", j["phase"].as_str().unwrap_or(""), j["attempt"], j["sent"], j["total"]);
    }
    if v["update"].is_object() {
        s += &format!("last update: {}\r\n", v["update"]);
    }
    s
}

/// Stores the backend setting (applied at the next boot).
pub fn set_backend(s: Setting) -> Result<(), String> {
    clear_guard();
    settings::set(SETTING_KEY, Some(s.as_str()))
}

/// Restores the network kept before the last NCP update (on a thread).
pub fn restore_request() -> Result<(), ApiError> {
    let st = state();
    if st["backup_epid"].as_str().is_none() {
        return Err(api("not_found", "no network was kept before an NCP update"));
    }
    if flashing() {
        return Err(api("busy", "the MG24 is being flashed"));
    }
    if !select::restore_offered(&st) {
        return Err(api(
            "invalid_state",
            match st["restore"].as_str() {
                Some("held") => {
                    "the last image was a maintenance image (an NV clear): the kept network waits for the next application image, whose \
                     flash restores it"
                }
                Some("done") => "the kept network was restored already",
                _ => "the kept network's restore is pending (it runs once the NCP's session is up)",
            },
        ));
    }
    if !spawn_restore() {
        return Err(api("busy", "a restore is running"));
    }
    Ok(())
}

/// Resets the MG24 into its application and restarts the hub (the
/// backend is detected again).
pub fn reset_request() -> Result<(), ApiError> {
    if flashing() {
        return Err(api("busy", "the MG24 is being flashed"));
    }
    clear_guard();
    lines::reset_to_app();
    std::thread::Builder::new().stack_size(8192).spawn(restart_hub).map(drop).map_err(|e| api("failed", format!("thread: {e}")))
}

/// `ncp/*` requests (maintenance feature `ncp_update`).
pub struct NcpHook;

fn confirmed(req: &J, what: &str) -> Result<(), ApiError> {
    if req["confirm"] == true { Ok(()) } else { Err(api("confirm_required", format!("{what} Send again with \"confirm\": true."))) }
}

impl hub_app::RequestHook for NcpHook {
    fn prefix(&self) -> &'static str {
        "ncp/"
    }
    fn feature(&self) -> Option<&'static str> {
        Some("ncp_update")
    }
    fn request(&mut self, ty: &str, req: &J) -> Result<J, ApiError> {
        match ty {
            "ncp/update/status" => Ok(status_json()),
            "ncp/backend/set" => {
                let s = req["backend"].as_str().and_then(Setting::parse).ok_or_else(|| api("invalid_value", "backend: auto, ezsp or zrhp"))?;
                set_backend(s).map_err(|e| api("failed", e))?;
                Ok(json!({"backend": s.as_str(), "applies": "at the next boot (hub/reboot)"}))
            }
            "ncp/update/begin" => begin(req),
            "ncp/update/chunk" => chunk(req),
            "ncp/update/end" => end(),
            "ncp/update/abort" => {
                *UPLOAD.lock().unwrap() = None;
                Ok(json!({}))
            }
            "ncp/update/start" => {
                confirmed(
                    req,
                    "Flashing replaces the MG24's firmware: the running network is backed up and kept first, the hub restarts on the \
                     new firmware and puts the network back onto it.",
                )?;
                let flag = |k: &str, d: bool| req[k].as_bool().unwrap_or(d);
                start(StartOpts { foreign: flag("foreign", false), force: flag("force", false), restore: flag("restore", true) })
            }
            "ncp/update/restore" => {
                confirmed(req, "The network kept before the NCP update goes onto the NCP (only onto one without a network).")?;
                restore_request().map(|()| json!({"state": "restoring"}))
            }
            "ncp/update/reset" => {
                confirmed(req, "The MG24 is reset into its application and the hub restarts (the backend is detected again).")?;
                reset_request().map(|()| json!({"state": "restarting"}))
            }
            _ => Err(api("invalid_format", format!("unknown request {ty}"))),
        }
    }
}
