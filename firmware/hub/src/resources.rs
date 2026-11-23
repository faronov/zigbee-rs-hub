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

pub fn get() -> &'static Resources {
    static RESOURCES: OnceLock<Resources> = OnceLock::new();
    RESOURCES.get_or_init(|| {
        let psram = unsafe { sys::heap_caps_get_total_size(sys::MALLOC_CAP_SPIRAM) };
        Resources::for_psram(psram)
    })
}

pub fn log() {
    let r = get();
    if r.internal_fallback() {
        log::warn!(
            "PSRAM not detected ({} image): internal-RAM fallback with {} HTTP sockets, {} KiB queued per WebSocket client, no built-in examples; check the Quad/Octal image",
            psram_mode(),
            r.http_sockets,
            r.ws_client_bytes / 1024,
        );
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
