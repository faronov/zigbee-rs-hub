//! The [`Resources`] of this boot and what the image was built with.

use std::sync::OnceLock;

use esp_idf_svc::sys;
use hub_core::resources::Resources;

/// Cargo features and PSRAM interface of the image, also as a marker in the
/// binary that tools/release-images.py checks against the board entry.
#[cfg(esp_idf_spiram_mode_oct)]
pub const BUILD: &str = concat!("tbr-hub-build:features=", env!("TBR_HUB_FEATURES"), ";psram=octal;");
#[cfg(not(esp_idf_spiram_mode_oct))]
pub const BUILD: &str = concat!("tbr-hub-build:features=", env!("TBR_HUB_FEATURES"), ";psram=quad;");

fn build_field(key: &str) -> &'static str {
    BUILD
        .split(';')
        .find_map(|f| f.rsplit(':').next().and_then(|f| f.strip_prefix(key)).and_then(|v| v.strip_prefix('=')))
        .unwrap_or("")
}

/// Cargo features of the image (`mqtt,advanced`).
pub fn features() -> &'static str {
    build_field("features")
}

/// PSRAM interface the image was built for (`quad`/`octal`).
pub fn psram_mode() -> &'static str {
    build_field("psram")
}

// One socket budget (hub_core::resources): the lwIP build has exactly it.
const _: () = assert!(hub_core::resources::LWIP_MAX_SOCKETS == sys::CONFIG_LWIP_MAX_SOCKETS as usize);
const _: () = assert!(Resources::for_psram(1).sockets() <= sys::CONFIG_LWIP_MAX_SOCKETS as usize);
const _: () = assert!(Resources::for_psram(0).sockets() <= sys::CONFIG_LWIP_MAX_SOCKETS as usize);

pub fn get() -> &'static Resources {
    static RESOURCES: OnceLock<Resources> = OnceLock::new();
    RESOURCES.get_or_init(|| {
        let psram = unsafe { sys::heap_caps_get_total_size(sys::MALLOC_CAP_SPIRAM) };
        Resources::for_psram(psram)
    })
}

/// The other PSRAM interface's image, for the wrong-image message.
pub fn other_psram_mode() -> &'static str {
    if psram_mode() == "octal" { "quad" } else { "octal" }
}

/// Without PSRAM the app is not started (it needs about 100 KB more
/// internal RAM than the module has left): the hub core keeps the Zigbee
/// network, the console and a web page say which image to flash.
pub fn app_allowed() -> bool {
    !get().internal_fallback()
}

/// The wrong-image message (console, log, web page).
pub fn wrong_image() -> String {
    format!(
        "PSRAM not detected: this image is for {} PSRAM. The Zigbee network keeps running, but the hub application \
         (web UI, Home Assistant API, MQTT) is not started. Flash the {} PSRAM image (the web flasher picks it by the chip).",
        psram_mode(),
        other_psram_mode(),
    )
}

pub fn log() {
    let r = get();
    if r.internal_fallback() {
        log::warn!("{}", wrong_image());
    } else {
        log::info!(
            "{} KiB PSRAM ({}): {} HTTP sockets ({} lwIP sockets in all), {} KiB queued per WebSocket client",
            r.psram_bytes / 1024,
            psram_mode(),
            r.http_sockets,
            r.sockets(),
            r.ws_client_bytes / 1024,
        );
    }
}
