//! zigbee-rs-hub for the SONOFF Dongle Max (Dongle-M: ESP32-D0WDR2-V3 +
//! EFR32MG24): the hub application on the EZSP backend, see
//! `firmware/gateway` and `docs/dongle-max.md`.

fn main() {
    zrh_gateway::run(&hub_core::profile::DONGLE_M)
}
