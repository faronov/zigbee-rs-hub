//! The `Resources` of this boot (`zrh_espapp::resources`) and what the
//! image was built with.

pub use zrh_espapp::resources::{app_allowed, get};

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

/// The other PSRAM interface's image, for the wrong-image message.
pub fn other_psram_mode() -> &'static str {
    if psram_mode() == "octal" { "quad" } else { "octal" }
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
            "{} KiB PSRAM ({}): {} HTTP sockets, {} KiB queued per WebSocket client",
            r.psram_bytes / 1024,
            psram_mode(),
            r.http_sockets,
            r.ws_client_bytes / 1024,
        );
    }
}
