//! The application slots of SONOFF's stock table and the way back to the
//! stock firmware.
//!
//! The hub runs from `ota_1` (installed through SONOFF's web console or
//! over USB); `ota_0` keeps SONOFF's application until the hub's first
//! self-update writes over it. While it is there, `stock confirm` gives
//! SONOFF's `files` data back if the hub claimed the region (the hub stops
//! first; `crate::files::restore`), selects `ota_0` again
//! (`esp_ota_set_boot_partition`, which verifies the image first, writes
//! one `otadata` entry) and restarts.

use std::ffi::CStr;

use esp_idf_svc::sys;

/// `esp_app_desc_t.project_name` of SONOFF's Dongle-M application.
pub const STOCK_PROJECT: &str = "Dongle-M";

fn ota0() -> *const sys::esp_partition_t {
    unsafe {
        sys::esp_partition_find_first(
            sys::esp_partition_type_t_ESP_PARTITION_TYPE_APP,
            sys::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_APP_OTA_0,
            core::ptr::null(),
        )
    }
}

fn text(b: &[core::ffi::c_char]) -> String {
    unsafe { CStr::from_ptr(b.as_ptr()) }.to_string_lossy().into_owned()
}

/// Project, version and ESP-IDF version of the image in `ota_0`.
fn ota0_app() -> Result<(String, String, String), String> {
    let p = ota0();
    if p.is_null() {
        return Err("no ota_0 partition".into());
    }
    let mut d: sys::esp_app_desc_t = unsafe { core::mem::zeroed() };
    match unsafe { sys::esp_ota_get_partition_description(p, &mut d) } {
        sys::ESP_OK => Ok((text(&d.project_name), text(&d.version), text(&d.idf_ver))),
        e => Err(format!("no application image in ota_0 (error {e})")),
    }
}

/// `ota_0` for `info`.
pub fn ota0_text() -> String {
    match ota0_app() {
        Ok((project, version, idf)) if project == STOCK_PROJECT => {
            format!("ota_0: SONOFF {project} {version} ({idf}): `stock` returns to it")
        }
        Ok((project, version, idf)) => format!("ota_0: {project} {version} ({idf}): not SONOFF's application"),
        Err(e) => format!("ota_0: {e}"),
    }
}

fn running_is_ota0() -> bool {
    let (r, p) = (unsafe { sys::esp_ota_get_running_partition() }, ota0());
    !r.is_null() && !p.is_null() && unsafe { (*r).address == (*p).address }
}

/// What `stock confirm` would do (SONOFF's application, what happens to
/// `files` first), or why it cannot.
fn check() -> Result<(String, Option<String>), String> {
    if running_is_ota0() {
        return Err("this firmware runs from ota_0: SONOFF's application is gone; reinstall it with SONOFF Dongle Flasher (USB) or from the esprom backup".into());
    }
    let (project, version, idf) = ota0_app()?;
    if project != STOCK_PROJECT {
        return Err(format!(
            "ota_0 holds {project} {version}, not SONOFF's application (a self-update replaced it); reinstall it with SONOFF Dongle Flasher (USB) or from the esprom backup"
        ));
    }
    let files = crate::files::restore_plan()?;
    Ok((format!("SONOFF {project} {version} ({idf}) in ota_0"), files))
}

/// `stock`: the plan; `stock confirm`: SONOFF's data back, switch to
/// `ota_0` and restart.
pub fn stock(confirm: bool) -> String {
    let (what, files) = match check() {
        Ok(c) => c,
        Err(e) => return format!("cannot return to the stock firmware: {e}\r\n"),
    };
    if !confirm {
        return format!(
            "{what}.\r\n{}`stock confirm` selects it in otadata and restarts into it; SONOFF's settings in nvs are untouched. \
             Install the hub again through SONOFF's web console (Firmware, ESP32-D0WDR2-V3).\r\n",
            files.map_or("SONOFF's `files` partition is untouched.\r\n".into(), |f| format!("{f}.\r\n"))
        );
    }
    if files.is_some() {
        crate::console::reply("stopping the hub and writing SONOFF's data back to `files` (up to a minute)\r\n");
        if !crate::hub::stop() {
            return "the hub did not stop within 5 s; nothing written back, stock firmware not selected (`restart`, then `stock confirm`)\r\n".into();
        }
        match crate::files::restore() {
            Ok(r) => crate::console::reply(&format!("files: {r:?}\r\n")),
            Err(e) => {
                return format!("SONOFF's data were not written back ({e}); stock firmware not selected. `stock confirm` again, or `restart`\r\n");
            }
        }
    }
    match unsafe { sys::esp_ota_set_boot_partition(ota0()) } {
        sys::ESP_OK => {
            crate::console::reply(&format!("{what} selected; restarting into the stock firmware\r\n"));
            crate::restart()
        }
        e => format!("esp_ota_set_boot_partition failed (error {e})\r\n"),
    }
}
