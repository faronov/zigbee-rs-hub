//! zigbee-rs-hub gateway firmware for the SONOFF Dongle Max (Dongle-M:
//! ESP32-D0WDR2-V3 + EFR32MG24), board bring-up stage: see
//! `firmware/gateway` and `docs/dongle-max.md`.

fn main() {
    zrh_gateway::run(&hub_core::profile::DONGLE_M)
}
