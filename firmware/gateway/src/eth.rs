//! Ethernet on the classic ESP32: internal EMAC, RMII PHY from the board
//! profile (Dongle-M: IP101GA at MDIO address 1, MDC GPIO23, MDIO GPIO18,
//! 50 MHz reference clock from the board's oscillator into GPIO0, PHY
//! power/reset on GPIO5; data pins fixed: TX_EN 21, TXD0 19, TXD1 22,
//! RXD0 25, RXD1 26, CRS_DV 27).
//!
//! GPIO0 is also the ESP32's download-mode strap. The oscillator is enabled
//! by the PHY power line, which is only driven high here, after boot, so the
//! clock never toggles GPIO0 while the chip samples its straps (an
//! `esp_restart` releases GPIO5 with the rest of the IO MUX).

use std::time::Duration;

use esp_idf_svc::eth::{BlockingEth, EspEth, EthDriver, RmiiClockConfig, RmiiEth, RmiiEthChipset};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{AnyIOPin, AnyOutputPin, Gpio0, Gpio19, Gpio21, Gpio22, Gpio25, Gpio26, Gpio27};
use esp_idf_svc::hal::mac::MAC;
use esp_idf_svc::sys::{self, EspError};
use hub_core::profile::{Ethernet, Output, Phy};
use log::{info, warn};

/// The peripherals Ethernet takes.
pub struct EthParts {
    /// EMAC.
    pub mac: MAC<'static>,
    /// RMII fixed pins.
    pub pins: (Gpio19<'static>, Gpio21<'static>, Gpio22<'static>, Gpio25<'static>, Gpio26<'static>, Gpio27<'static>),
    /// Reference clock input.
    pub clk: Gpio0<'static>,
    /// MDC, MDIO, PHY address, power line.
    pub mdc: u8,
    /// MDIO.
    pub mdio: u8,
    /// PHY address.
    pub phy_addr: u8,
    /// PHY power/reset.
    pub power: Option<Output>,
}

impl EthParts {
    /// The board's RMII Ethernet, if any.
    pub fn from_profile(
        eth: &Ethernet,
        mac: MAC<'static>,
        pins: (Gpio19<'static>, Gpio21<'static>, Gpio22<'static>, Gpio25<'static>, Gpio26<'static>, Gpio27<'static>),
        clk: Gpio0<'static>,
    ) -> Option<Self> {
        match *eth {
            Ethernet::Rmii { phy: Phy::Ip101, mdc, mdio, clk_in_gpio0: true, power, phy_addr } => {
                Some(Self { mac, pins, clk, mdc, mdio, phy_addr, power })
            }
            _ => None,
        }
    }
}

fn power_on(o: &Output) {
    unsafe {
        let g = i32::from(o.gpio);
        sys::gpio_reset_pin(g);
        sys::gpio_set_direction(g, sys::gpio_mode_t_GPIO_MODE_OUTPUT);
        sys::gpio_set_level(g, o.level(false).into());
        std::thread::sleep(Duration::from_millis(10));
        sys::gpio_set_level(g, o.level(true).into());
    }
    // Oscillator start-up and the IP101's reset release (10 ms).
    std::thread::sleep(Duration::from_millis(20));
}

/// Start the EMAC and a thread that reports the link state.
pub fn start(parts: EthParts, sys_loop: EspSystemEventLoop, report: impl Fn(String) + Send + 'static) {
    if let Some(p) = &parts.power {
        power_on(p);
    }
    let result = (|| -> Result<BlockingEth<EspEth<'static, RmiiEth>>, EspError> {
        let (txd0, tx_en, txd1, rxd0, rxd1, crs_dv) = parts.pins;
        let driver = EthDriver::new_rmii(
            parts.mac,
            rxd0,
            rxd1,
            crs_dv,
            unsafe { AnyOutputPin::steal(parts.mdc) },
            txd1,
            tx_en,
            txd0,
            unsafe { AnyIOPin::steal(parts.mdio) },
            RmiiClockConfig::Input(parts.clk),
            None::<AnyOutputPin>,
            RmiiEthChipset::IP101,
            Some(parts.phy_addr.into()),
            sys_loop.clone(),
        )?;
        let mut eth = BlockingEth::wrap(EspEth::wrap(driver)?, sys_loop)?;
        eth.start()?;
        Ok(eth)
    })();
    let eth = match result {
        Ok(e) => e,
        Err(e) => {
            warn!("ethernet: {e}");
            report(format!("error: {e}"));
            return;
        }
    };
    report("down (no link)".into());
    let spawned = std::thread::Builder::new().name("eth".into()).stack_size(4096).spawn(move || {
        let mut last = String::new();
        loop {
            let text = match (eth.is_connected().unwrap_or(false), eth.is_up().unwrap_or(false)) {
                (_, true) => {
                    let ip = eth.eth().netif().get_ip_info().map(|i| i.ip.to_string()).unwrap_or_default();
                    format!("up {ip}")
                }
                (true, false) => "link up, waiting for DHCP".into(),
                (false, _) => "down (no link)".into(),
            };
            if text != last {
                info!("ethernet: {text}");
                report(text.clone());
                last = text;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    if let Err(e) = spawned {
        warn!("ethernet thread: {e}");
    }
}
