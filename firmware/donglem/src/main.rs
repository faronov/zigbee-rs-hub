//! zigbee-rs-hub gateway firmware: SONOFF Dongle-M (ESP32-D0WDR2-V3 + EFR32MG24).

fn main() {
    zrh_gateway::run(&hub_core::profile::DONGLE_M)
}
