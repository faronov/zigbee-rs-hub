//! Board profiles: what a firmware needs to know about a host board (NCP
//! UART, NCP reset/loader lines, Ethernet, LED, button, console, flash and
//! PSRAM), as plain data checked on the host. The single source of the
//! pins of every board a firmware drives through a profile; the memory
//! limits of a boot still come from [`resources`](crate::resources).
//!
//! | Profile | Host | NCP | Uplink | Console | Flash / PSRAM |
//! |---|---|---|---|---|---|
//! | [`THREAD_BR`] | ESP32-S3 | ESP32-H2 | Wi-Fi (W5500 optional) | TinyUSB CDC | 8 MB (4 MB samples) / 2–8 MB |
//! | [`DONGLE_M`] | ESP32-D0WDR2-V3 | EFR32MG24 | IP101 RMII Ethernet, Wi-Fi | UART0 (CP2102N) | 16 MB / 2 MB |
//!
//! Sources: [`board`](crate::board) for the Thread BR board (`firmware/hub`
//! still uses those constants directly). SONOFF Dongle Max (Dongle-M): the
//! GPIO table of SONOFF's guide "Running ESPHome firmware on Dongle-M"
//! (dongle.sonoff.tech, MG24 UART/reset/bootloader, RGB LED, button,
//! Ethernet MDC/MDIO/REF_CLK) and its ESPHome configuration (IP101, PHY
//! address 1, `CLK_EXT_IN` on GPIO0, PHY power on GPIO5, MG24 UART at
//! 115 200, bootloader entry = GPIO12 high across a GPIO15 reset pulse of
//! 100 ms); the classic ESP32's fixed RMII data pins (ESP32 TRM, EMAC); the
//! D0WDR2-V3 in-package PSRAM pins (ESP-IDF v5.5.1
//! `esp_psram_impl_quad.c`: CLK GPIO6, CS GPIO16); the unit's eFuses read
//! with hwlab `esprom` (flash on GPIO6–11, VDD_SDIO from the GPIO12 strap).

use core::fmt;

/// Host SoC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostChip {
    /// Classic ESP32 (Xtensa LX6).
    Esp32,
    /// ESP32-S3 (Xtensa LX7).
    Esp32S3,
}

impl HostChip {
    /// Strapping pins sampled at reset.
    pub const fn strapping_pins(self) -> &'static [u8] {
        match self {
            HostChip::Esp32 => &[0, 2, 5, 12, 15],
            HostChip::Esp32S3 => &[0, 3, 45, 46],
        }
    }

    /// Pins the firmware must never touch: SPI flash and quad PSRAM. On the
    /// ESP32 that is GPIO6–11 plus GPIO16, the chip select of the
    /// D0WDR2-V3's in-package PSRAM (its clock shares GPIO6).
    pub const fn reserved_pins(self) -> &'static [u8] {
        match self {
            HostChip::Esp32 => &[6, 7, 8, 9, 10, 11, 16],
            HostChip::Esp32S3 => &[26, 27, 28, 29, 30, 31, 32],
        }
    }

    /// Whether `gpio` exists and can drive an output.
    pub const fn is_output_capable(self, gpio: u8) -> bool {
        match self {
            // GPIO34–39 are input only.
            HostChip::Esp32 => matches!(gpio, 0..=5 | 12..=19 | 21..=23 | 25..=27 | 32 | 33),
            HostChip::Esp32S3 => matches!(gpio, 0..=21 | 33..=48),
        }
    }

    /// Whether `gpio` exists as an input.
    pub const fn is_input_capable(self, gpio: u8) -> bool {
        match self {
            HostChip::Esp32 => self.is_output_capable(gpio) || matches!(gpio, 34..=39),
            HostChip::Esp32S3 => self.is_output_capable(gpio),
        }
    }

    /// ESP-IDF target name.
    pub const fn idf_target(self) -> &'static str {
        match self {
            HostChip::Esp32 => "esp32",
            HostChip::Esp32S3 => "esp32s3",
        }
    }
}

/// NCP SoC. The chip does not select the coordinator protocol: an
/// EFR32MG24 with stock EmberZNet speaks EZSP, a zigbee-rs NCP ZRHP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NcpChip {
    /// ESP32-H2 (zigbee-rs `esp32-zigbee-ncp`).
    Esp32H2,
    /// Silicon Labs EFR32MG24.
    Efr32Mg24,
}

/// A host output driving a line of the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Output {
    /// GPIO number.
    pub gpio: u8,
    /// The line's active level is high.
    pub active_high: bool,
    /// Drive only the active-low level and float otherwise (the target has
    /// its own pull-up), so the host never fights the target's level.
    pub open_drain: bool,
}

impl Output {
    /// Push-pull, active high.
    pub const fn high(gpio: u8) -> Self {
        Self { gpio, active_high: true, open_drain: false }
    }
    /// Push-pull, active low.
    pub const fn low(gpio: u8) -> Self {
        Self { gpio, active_high: false, open_drain: false }
    }
    /// Open drain, active low.
    pub const fn low_od(gpio: u8) -> Self {
        Self { gpio, active_high: false, open_drain: true }
    }
    /// The pin level for "asserted" or "released".
    pub const fn level(&self, asserted: bool) -> bool {
        asserted == self.active_high
    }
}

/// A host input (button).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Input {
    /// GPIO number.
    pub gpio: u8,
    /// Pressed reads low.
    pub active_low: bool,
    /// Enable the internal pull-up (GPIO34–39 on the ESP32 have none; the
    /// board must pull them).
    pub pull_up: bool,
}

/// One UART line setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineSpec {
    /// Bit rate.
    pub baud: u32,
    /// RTS/CTS.
    pub flow: bool,
}

/// The NCP UART on the host side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NcpUart {
    /// Host UART peripheral.
    pub port: u8,
    /// Host TX → NCP RX.
    pub tx: u8,
    /// Host RX ← NCP TX.
    pub rx: u8,
    /// Host RTS → NCP CTS, if wired.
    pub rts: Option<u8>,
    /// Host CTS ← NCP RTS, if wired.
    pub cts: Option<u8>,
    /// First line setting.
    pub line: LineSpec,
    /// Tried after [`NcpUart::line`] when no session comes up.
    pub fallback: Option<LineSpec>,
}

impl NcpUart {
    /// The line settings to try, in order.
    pub fn lines(&self) -> impl Iterator<Item = LineSpec> + '_ {
        core::iter::once(self.line).chain(self.fallback)
    }
    /// Whether RTS/CTS can be used at all.
    pub const fn flow_wired(&self) -> bool {
        self.rts.is_some() && self.cts.is_some()
    }
}

/// How the host gets the NCP into a serial loader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NcpLoader {
    /// ESP ROM serial loader: `boot` held during the reset release
    /// (ESP32-H2 GPIO9 strap).
    EspRom {
        /// BOOT strap line.
        boot: Output,
        /// BOOT stays asserted this long after the reset release.
        hold_ms: u32,
    },
    /// Silicon Labs Gecko bootloader (UART XMODEM menu): `boot` asserted,
    /// reset pulse, `boot` released after `release_ms`.
    GeckoUart {
        /// Bootloader entry line.
        boot: Output,
        /// Bootloader UART rate (fixed in the bootloader image).
        baud: u32,
        /// `boot` asserted this long before the reset pulse.
        setup_ms: u32,
        /// `boot` released this long after the reset release.
        release_ms: u32,
    },
}

/// NCP reset and loader lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NcpControl {
    /// Reset line (active level = in reset).
    pub reset: Option<Output>,
    /// Reset pulse length.
    pub reset_ms: u32,
    /// Serial loader.
    pub loader: NcpLoader,
}

impl NcpControl {
    /// The loader's entry line.
    pub const fn boot(&self) -> Output {
        match self.loader {
            NcpLoader::EspRom { boot, .. } | NcpLoader::GeckoUart { boot, .. } => boot,
        }
    }
}

/// Ethernet PHY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phy {
    /// IC Plus IP101GA/GRI.
    Ip101,
}

/// Ethernet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ethernet {
    /// None: Wi-Fi only.
    None,
    /// Classic ESP32 internal EMAC over RMII; data pins [`RMII_DATA_PINS`].
    Rmii {
        /// PHY.
        phy: Phy,
        /// MDC.
        mdc: u8,
        /// MDIO.
        mdio: u8,
        /// 50 MHz reference clock from an external oscillator into GPIO0
        /// (ESP-IDF `EMAC_CLK_EXT_IN`).
        clk_in_gpio0: bool,
        /// PHY (and oscillator) power, asserted before the EMAC starts.
        power: Option<Output>,
        /// PHY MDIO address.
        phy_addr: u8,
    },
    /// W5500 on SPI (Thread BR daughter board, not driven by any firmware
    /// yet): SCLK, MOSI, MISO, CS, INT, RESET.
    W5500 {
        /// Pins.
        pins: [u8; 6],
    },
}

/// Fixed RMII data pins of the classic ESP32 EMAC: TXD0 19, TX_EN 21,
/// TXD1 22, RXD0 25, RXD1 26, CRS_DV 27.
pub const RMII_DATA_PINS: [u8; 6] = [19, 21, 22, 25, 26, 27];

/// Status LED.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Led {
    /// None the firmware drives.
    None,
    /// Discrete RGB LED, one GPIO per colour (PWM capable).
    Rgb {
        /// Red, green, blue.
        rgb: [Output; 3],
    },
}

/// Firmware console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Console {
    /// UART0 (on the Dongle-M behind the CP2102N USB bridge).
    Uart0,
    /// TinyUSB CDC-ACM 1 (the Thread BR firmware's console).
    TinyUsbCdc,
}

/// Partition table a board's firmware runs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashLayout {
    /// 4 MB development layout (`firmware/hub/partitions-4mb-dev.csv`).
    Dev4,
    /// 8 MB production layout (`firmware/hub/partitions-8mb.csv`).
    Production8,
    /// The Dongle-M's stock 16 MB table, unchanged
    /// (`firmware/donglem/partitions.csv`): `nvs`, `otadata`, `phy_init`,
    /// two 6 MiB app slots, SONOFF's `files` LittleFS; no `hubdata`.
    DongleStock16,
}

/// Memory of the host module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    /// Flash size.
    pub flash: u32,
    /// PSRAM size (0: none).
    pub psram: u32,
    /// Partition table.
    pub layout: FlashLayout,
}

/// One host board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoardProfile {
    /// Short id (`thread-br`, `dongle-m`).
    pub id: &'static str,
    /// Human-readable name.
    pub name: &'static str,
    /// Host SoC.
    pub host: HostChip,
    /// NCP SoC.
    pub ncp: NcpChip,
    /// NCP UART.
    pub uart: NcpUart,
    /// NCP reset and loader lines.
    pub control: NcpControl,
    /// Ethernet.
    pub ethernet: Ethernet,
    /// Status LED.
    pub led: Led,
    /// User button.
    pub button: Option<Input>,
    /// Console.
    pub console: Console,
    /// Flash and PSRAM.
    pub memory: Memory,
    /// 4-wire PTA between host Wi-Fi and the NCP radio.
    pub pta: bool,
}

const MIB: u32 = 1024 * 1024;

/// Espressif Thread Border Router / Zigbee Gateway board (S3 + H2).
pub const THREAD_BR: BoardProfile = {
    use crate::board as b;
    BoardProfile {
        id: "thread-br",
        name: "Espressif Thread BR / Zigbee Gateway",
        host: HostChip::Esp32S3,
        ncp: NcpChip::Esp32H2,
        uart: NcpUart {
            port: 1,
            tx: b::UART_TX_TO_H2 as u8,
            rx: b::UART_RX_FROM_H2 as u8,
            rts: Some(b::reserved::R20_S3 as u8),
            cts: Some(b::reserved::R19_S3 as u8),
            line: LineSpec { baud: 921_600, flow: true },
            fallback: Some(LineSpec { baud: b::DEFAULT_BAUD, flow: false }),
        },
        control: NcpControl {
            // Open drain with pull-ups, so the H2's buttons never fight the S3.
            reset: Some(Output::low_od(b::H2_EN as u8)),
            reset_ms: b::RESET_PULSE_MS,
            loader: NcpLoader::EspRom { boot: Output::low_od(b::H2_BOOT as u8), hold_ms: b::BOOT_HOLD_MS },
        },
        ethernet: Ethernet::W5500 {
            pins: [
                b::w5500::SCLK as u8,
                b::w5500::MOSI as u8,
                b::w5500::MISO as u8,
                b::w5500::CS as u8,
                b::w5500::INT as u8,
                b::w5500::RESET as u8,
            ],
        },
        led: Led::None,
        button: None,
        console: Console::TinyUsbCdc,
        memory: Memory { flash: 8 * MIB, psram: 2 * MIB, layout: FlashLayout::Production8 },
        pta: true,
    }
};

/// SONOFF Dongle Max (Dongle-M): ESP32-D0WDR2-V3 + EFR32MG24.
pub const DONGLE_M: BoardProfile = BoardProfile {
    id: "dongle-m",
    name: "SONOFF Dongle Max (Dongle-M)",
    host: HostChip::Esp32,
    ncp: NcpChip::Efr32Mg24,
    uart: NcpUart {
        port: 1,
        tx: 17,
        rx: 13,
        // No RTS/CTS between the chips.
        rts: None,
        cts: None,
        // The stock MG24 firmware (EmberZNet NCP, "1.0.0", work mode 0)
        // runs at 115 200; other rates only with other MG24 firmware.
        line: LineSpec { baud: 115_200, flow: false },
        fallback: None,
    },
    control: NcpControl {
        // GPIO15 (MTDO strap) has its internal pull-up at an ESP32 reset:
        // the MG24 stays out of reset while the ESP32 boots.
        reset: Some(Output::low(15)),
        reset_ms: 100,
        loader: NcpLoader::GeckoUart {
            // GPIO12 is the ESP32's MTDI strap (VDD_SDIO voltage, not
            // overridden by eFuse on this unit): low at every ESP32 reset;
            // the firmware drives it low first and high only around a
            // bootloader entry.
            boot: Output::high(12),
            baud: 115_200,
            setup_ms: 100,
            release_ms: 500,
        },
    },
    ethernet: Ethernet::Rmii {
        phy: Phy::Ip101,
        mdc: 23,
        mdio: 18,
        clk_in_gpio0: true,
        power: Some(Output::high(5)),
        phy_addr: 1,
    },
    led: Led::Rgb { rgb: [Output::high(4), Output::high(14), Output::high(2)] },
    button: Some(Input { gpio: 34, active_low: true, pull_up: false }),
    console: Console::Uart0,
    memory: Memory { flash: 16 * MIB, psram: 2 * MIB, layout: FlashLayout::DongleStock16 },
    pta: false,
};

/// All profiles.
pub const ALL: [&BoardProfile; 2] = [&THREAD_BR, &DONGLE_M];

/// Look a profile up by [`BoardProfile::id`].
pub fn by_id(id: &str) -> Option<&'static BoardProfile> {
    ALL.into_iter().find(|p| p.id == id)
}

/// A wiring problem found by [`BoardProfile::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Issue {
    /// Two functions share a GPIO.
    Conflict(u8),
    /// The GPIO is flash/PSRAM.
    Reserved(u8),
    /// An output on an input-only or missing GPIO.
    NotOutput(u8),
    /// An input on a missing GPIO.
    NotInput(u8),
    /// Flow control on without RTS/CTS pins.
    FlowWithoutPins,
    /// The NCP UART is the console UART.
    ConsoleUart,
    /// The serial loader has no reset line.
    LoaderLines,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Issue::Conflict(g) => write!(f, "GPIO{g} is used twice"),
            Issue::Reserved(g) => write!(f, "GPIO{g} belongs to flash/PSRAM"),
            Issue::NotOutput(g) => write!(f, "GPIO{g} cannot drive an output"),
            Issue::NotInput(g) => write!(f, "GPIO{g} is not an input"),
            Issue::FlowWithoutPins => write!(f, "flow control needs both RTS and CTS"),
            Issue::ConsoleUart => write!(f, "the NCP UART is the console UART"),
            Issue::LoaderLines => write!(f, "the serial loader needs a reset line"),
        }
    }
}

/// A strapping pin the profile uses, with what goes wrong if it is not in
/// its default state at a host reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strap {
    /// ESP32 MTDI (GPIO12): high at reset selects a 1.8 V VDD_SDIO and the
    /// 3.3 V flash and PSRAM do not work, unless an eFuse fixes the voltage.
    FlashVoltage(u8),
    /// GPIO0: low at reset enters the ROM download mode (on the Dongle-M
    /// also the RMII reference clock input).
    DownloadMode(u8),
    /// Boot-mode or log straps that only matter if the line is held in a
    /// non-default state at reset (ESP32 GPIO2/5/15, S3 GPIO3/45/46).
    Benign(u8),
}

/// Error of [`BoardProfile::apply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideError<'a> {
    /// Not `key=value`.
    Syntax(&'a str),
    /// Unknown key.
    Key(&'a str),
    /// Bad value.
    Value(&'a str),
    /// The result does not pass [`BoardProfile::check`].
    Wiring(Issue),
}

impl fmt::Display for OverrideError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OverrideError::Syntax(s) => write!(f, "expected key=value, got {s:?}"),
            OverrideError::Key(s) => write!(f, "unknown key {s:?} (tx rx rts cts reset reset_od baud flow fallback)"),
            OverrideError::Value(s) => write!(f, "bad value in {s:?}"),
            OverrideError::Wiring(i) => write!(f, "{i}"),
        }
    }
}

fn parse_gpio(v: &str) -> Option<Option<u8>> {
    match v {
        "none" | "-" => Some(None),
        _ => v.strip_prefix("gpio").unwrap_or(v).parse().ok().map(Some),
    }
}

fn parse_baud(v: &str) -> Option<u32> {
    v.parse().ok().filter(|b| (9_600..=3_000_000).contains(b))
}

impl BoardProfile {
    /// Every GPIO the profile uses, with whether it is driven.
    fn pins(&self, mut f: impl FnMut(u8, bool)) {
        f(self.uart.tx, true);
        f(self.uart.rx, false);
        if let Some(p) = self.uart.rts {
            f(p, true);
        }
        if let Some(p) = self.uart.cts {
            f(p, false);
        }
        if let Some(r) = self.control.reset {
            f(r.gpio, true);
        }
        f(self.control.boot().gpio, true);
        match self.ethernet {
            Ethernet::None => {}
            Ethernet::Rmii { mdc, mdio, clk_in_gpio0, power, .. } => {
                f(mdc, true);
                f(mdio, true);
                if clk_in_gpio0 {
                    f(0, false);
                }
                if let Some(p) = power {
                    f(p.gpio, true);
                }
                for p in RMII_DATA_PINS {
                    f(p, true);
                }
            }
            Ethernet::W5500 { pins } => {
                for (i, p) in pins.into_iter().enumerate() {
                    // MISO and INT are inputs.
                    f(p, !matches!(i, 2 | 4));
                }
            }
        }
        if let Led::Rgb { rgb } = self.led {
            for o in rgb {
                f(o.gpio, true);
            }
        }
        if let Some(b) = self.button {
            f(b.gpio, false);
        }
    }

    /// Check the wiring against the host chip: conflicts, flash/PSRAM pins,
    /// pin capabilities, flow control and loader lines. Returns the first
    /// issue.
    pub fn check(&self) -> Result<(), Issue> {
        let mut seen = 0u64;
        let mut issue = None;
        // The S3's USB D-/D+ belong to the USB console.
        let usb = self.host == HostChip::Esp32S3 && self.console == Console::TinyUsbCdc;
        self.pins(|g, out| {
            if issue.is_some() {
                return;
            }
            let bit = 1u64 << (g & 63);
            issue = if g > 48 {
                Some(Issue::NotInput(g))
            } else if seen & bit != 0 {
                Some(Issue::Conflict(g))
            } else if self.host.reserved_pins().contains(&g) || (usb && matches!(g, 19 | 20)) {
                Some(Issue::Reserved(g))
            } else if out && !self.host.is_output_capable(g) {
                Some(Issue::NotOutput(g))
            } else if !out && !self.host.is_input_capable(g) {
                Some(Issue::NotInput(g))
            } else {
                None
            };
            seen |= bit;
        });
        if let Some(i) = issue {
            return Err(i);
        }
        if self.uart.lines().any(|l| l.flow) && !self.uart.flow_wired() {
            return Err(Issue::FlowWithoutPins);
        }
        if self.console == Console::Uart0 && self.uart.port == 0 {
            return Err(Issue::ConsoleUart);
        }
        if self.control.reset.is_none() {
            return Err(Issue::LoaderLines);
        }
        Ok(())
    }

    /// Strapping pins the profile uses (report them at boot; see
    /// [`Strap`]).
    pub fn straps(&self) -> impl Iterator<Item = Strap> + '_ {
        let mut out = [None; 24];
        let mut n = 0;
        self.pins(|g, driven| {
            if n < out.len() && self.host.strapping_pins().contains(&g) {
                out[n] = Some(match (self.host, g, driven) {
                    (HostChip::Esp32, 12, true) => Strap::FlashVoltage(g),
                    (_, 0, _) => Strap::DownloadMode(g),
                    _ => Strap::Benign(g),
                });
                n += 1;
            }
        });
        out.into_iter().flatten()
    }

    /// Apply overrides of the NCP wiring, `key=value` separated by spaces
    /// or commas: `tx`, `rx`, `rts`, `cts`, `reset` (GPIO or `none`),
    /// `reset_od` (0/1), `baud`, `flow` (0/1), `fallback` (baud or `none`).
    /// The result must pass [`BoardProfile::check`]; on error the profile
    /// is unchanged.
    pub fn apply<'a>(&mut self, overrides: &'a str) -> Result<(), OverrideError<'a>> {
        let mut p = *self;
        for kv in overrides.split([' ', ',', '\n']).filter(|s| !s.is_empty()) {
            let (k, v) = kv.split_once('=').ok_or(OverrideError::Syntax(kv))?;
            let bad = OverrideError::Value(kv);
            let gpio = || parse_gpio(v).ok_or(bad);
            let flag = || match v {
                "0" | "off" | "false" => Ok(false),
                "1" | "on" | "true" => Ok(true),
                _ => Err(bad),
            };
            match k {
                "tx" => p.uart.tx = gpio()?.ok_or(bad)?,
                "rx" => p.uart.rx = gpio()?.ok_or(bad)?,
                "rts" => p.uart.rts = gpio()?,
                "cts" => p.uart.cts = gpio()?,
                "reset" => {
                    let od = p.control.reset.is_some_and(|r| r.open_drain);
                    p.control.reset = gpio()?.map(|g| Output { gpio: g, active_high: false, open_drain: od });
                }
                "reset_od" => {
                    let od = flag()?;
                    if let Some(r) = &mut p.control.reset {
                        r.open_drain = od;
                    }
                }
                "baud" => p.uart.line.baud = parse_baud(v).ok_or(bad)?,
                "flow" => p.uart.line.flow = flag()?,
                "fallback" => {
                    p.uart.fallback = match v {
                        "none" => None,
                        _ => Some(LineSpec { baud: parse_baud(v).ok_or(bad)?, flow: false }),
                    }
                }
                _ => return Err(OverrideError::Key(k)),
            }
        }
        p.check().map_err(OverrideError::Wiring)?;
        *self = p;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_profiles_pass_the_wiring_check() {
        for p in ALL {
            assert_eq!(p.check(), Ok(()), "{}", p.id);
            assert_eq!(by_id(p.id), Some(p));
        }
    }

    #[test]
    fn dongle_m_reports_its_straps_and_has_no_flow_control() {
        let mut straps = [None; 8];
        for (i, s) in DONGLE_M.straps().enumerate() {
            straps[i] = Some(s);
        }
        for s in [Strap::FlashVoltage(12), Strap::DownloadMode(0), Strap::Benign(15), Strap::Benign(5), Strap::Benign(2)] {
            assert!(straps.contains(&Some(s)), "{s:?}");
        }
        assert!(!DONGLE_M.uart.flow_wired());
        assert!(DONGLE_M.uart.lines().all(|l| !l.flow));
    }

    #[test]
    fn overrides_change_the_line_and_keep_the_rest() {
        let mut p = DONGLE_M;
        p.apply("baud=460800, fallback=115200").unwrap();
        assert_eq!(p.uart.line, LineSpec { baud: 460_800, flow: false });
        assert_eq!(p.uart.fallback, Some(LineSpec { baud: 115_200, flow: false }));
        assert_eq!((p.uart.tx, p.uart.rx, p.control), (17, 13, DONGLE_M.control));

        let mut p = THREAD_BR;
        p.apply("reset_od=0 flow=0 fallback=none").unwrap();
        assert_eq!(p.control.reset, Some(Output::low(7)));
        assert!(!p.uart.line.flow && p.uart.fallback.is_none());
    }

    #[test]
    fn bad_overrides_leave_the_profile_unchanged() {
        let mut d = DONGLE_M;
        // GPIO34 is input only; GPIO25 an RMII data pin; GPIO16 the PSRAM's CS.
        assert_eq!(d.apply("tx=34"), Err(OverrideError::Wiring(Issue::NotOutput(34))));
        assert_eq!(d.apply("rx=25"), Err(OverrideError::Wiring(Issue::Conflict(25))));
        assert_eq!(d.apply("rx=16"), Err(OverrideError::Wiring(Issue::Reserved(16))));
        assert_eq!(d.apply("flow=1"), Err(OverrideError::Wiring(Issue::FlowWithoutPins)));
        assert_eq!(d.apply("reset=none"), Err(OverrideError::Wiring(Issue::LoaderLines)));
        assert_eq!(d.apply("speed=1"), Err(OverrideError::Key("speed")));
        assert_eq!(d.apply("baud=fast"), Err(OverrideError::Value("baud=fast")));
        assert_eq!(d.apply("baud=1200"), Err(OverrideError::Value("baud=1200")));
        assert_eq!(d.apply("tx"), Err(OverrideError::Syntax("tx")));
        assert_eq!(d, DONGLE_M);
    }
}
