//! `tbr-bridge`: the phase-0 USB ↔ H2 UART bridge (see the library).

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    tbr_bridge::run()
}
