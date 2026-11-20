//! zigbee-rs-hub gateway firmware: ESP32-S3 SuperMini + RF-BM-MG24B1 bench setup.

fn main() {
    zrh_gateway::run(&hub_core::profile::S3_MG24_DEV)
}
