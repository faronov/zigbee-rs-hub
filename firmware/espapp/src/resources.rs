//! The [`Resources`] of this boot: buffer limits from the PSRAM detected
//! (`hub_core::resources`).

use std::sync::OnceLock;

use esp_idf_svc::sys;
use hub_core::resources::Resources;

/// This boot's limits.
pub fn get() -> &'static Resources {
    static RESOURCES: OnceLock<Resources> = OnceLock::new();
    RESOURCES.get_or_init(|| {
        let psram = unsafe { sys::heap_caps_get_total_size(sys::MALLOC_CAP_SPIRAM) };
        Resources::for_psram(psram)
    })
}

/// Without PSRAM the app is not started (it needs about 100 KB more
/// internal RAM than the module has left).
pub fn app_allowed() -> bool {
    !get().internal_fallback()
}
