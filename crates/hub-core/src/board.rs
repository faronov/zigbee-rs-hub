//! ESP Thread Border Router / Zigbee Gateway board, S3 side.
//!
//! Sources: Espressif "Hardware Platforms" (esp-thread-br docs, 1.2.1 GPIO
//! allocation) and the board schematic v1.3
//! (`esp_thread_br_zigbee_gw_schematics_v1.3.pdf`, "Interface" block: 0 Ω
//! links R14–R24). Both agree on every pin below.
//!
//! H2 side passives (schematic v1.3): `H2_EN` has a 10 kΩ pull-up (R2) and
//! 1 µF to GND (C5), so EN rises with τ ≈ 10 ms after release. `H2_GPIO8` has a
//! 10 kΩ pull-up (R3), as the H2 download strap needs. `H2_GPIO9` (BOOT) has
//! only the H2 button and the chip's internal pull-up.

/// S3 GPIO18 → R17 → H2 GPIO23 (U0RXD).
pub const UART_TX_TO_H2: i32 = 18;
/// S3 GPIO17 ← R16 ← H2 GPIO24 (U0TXD).
pub const UART_RX_FROM_H2: i32 = 17;
/// S3 GPIO7 → R14 → H2 EN (active low reset).
pub const H2_EN: i32 = 7;
/// S3 GPIO8 → R15 → H2 GPIO9 (boot strap, low at reset = ROM download).
pub const H2_BOOT: i32 = 8;

/// 4-wire PTA (optional, disabled by default in Espressif's firmware).
pub mod pta {
    /// S3 GPIO12 ↔ R23 ↔ H2 GPIO0.
    pub const REQUEST: i32 = 12;
    /// S3 GPIO10 ↔ R21 ↔ H2 GPIO2.
    pub const PRIORITY: i32 = 10;
    /// S3 GPIO13 ↔ R24 ↔ H2 GPIO1.
    pub const GRANT: i32 = 13;
    /// S3 GPIO11 ↔ R22 ↔ H2 GPIO3 (fourth wire, needed with an S3 host).
    pub const TX_LINE: i32 = 11;
}

/// "Reserved" 0 Ω links; candidates for RTS/CTS (ZRHP spec 11.5).
pub mod reserved {
    /// S3 GPIO6 ↔ R18 ↔ H2 GPIO12.
    pub const R18_S3: i32 = 6;
    /// S3 GPIO5 ↔ R19 ↔ H2 GPIO4.
    pub const R19_S3: i32 = 5;
    /// S3 GPIO4 ↔ R20 ↔ H2 GPIO5.
    pub const R20_S3: i32 = 4;
}

/// Optional W5500 Sub-Ethernet daughter board on SPI2 (J4).
pub mod w5500 {
    pub const SCLK: i32 = 21;
    pub const MOSI: i32 = 45;
    pub const MISO: i32 = 38;
    pub const CS: i32 = 41;
    pub const INT: i32 = 39;
    pub const RESET: i32 = 40;
}

/// Default H2 UART rate of Espressif's RCP firmware and of ZRHP on this board.
pub const DEFAULT_BAUD: u32 = 460_800;
/// Rate of the ESP32-H2 ROM boot log and of the ROM loader's first SYNC.
pub const ROM_BAUD: u32 = 115_200;

/// EN low time for a reset (RC discharge through the S3 pin is fast; the
/// ZRHP spec asks for at least 10 ms).
pub const RESET_PULSE_MS: u32 = 20;
/// BOOT stays low this long after EN is released, so the strap is still low
/// when EN crosses its threshold (τ ≈ 10 ms; esptool uses 50 ms).
pub const BOOT_HOLD_MS: u32 = 100;
