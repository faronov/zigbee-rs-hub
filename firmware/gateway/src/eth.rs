//! Ethernet on the classic ESP32: internal EMAC, RMII PHY from the board
//! profile (Dongle-M: IP101 at MDIO address 1, MDC GPIO23, MDIO GPIO18,
//! 50 MHz reference clock from the board's oscillator into GPIO0, PHY
//! power on GPIO5; data pins fixed: TXD0 19, TX_EN 21, TXD1 22, RXD0 25,
//! RXD1 26, CRS_DV 27).
//!
//! GPIO0 is also the ESP32's download-mode strap. The PHY power line is
//! driven high only here, after boot, as in SONOFF's ESPHome configuration.
//!
//! The Ethernet interface gets a higher route priority than the Wi-Fi
//! station, so it is the default route whenever it has an address.

use std::time::Duration;

use esp_idf_svc::eth::{BlockingEth, EspEth, EthDriver, RmiiClockConfig, RmiiEth, RmiiEthChipset};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{AnyIOPin, AnyOutputPin, Gpio0, Gpio19, Gpio21, Gpio22, Gpio25, Gpio26, Gpio27};
use esp_idf_svc::hal::mac::MAC;
use esp_idf_svc::netif::{EspNetif, NetifConfiguration};
use esp_idf_svc::sys::{self, EspError};
use hub_core::profile::{Ethernet, Output, Phy};
use log::{info, warn};

/// Route priority of the Ethernet interface (ESP-IDF defaults: Wi-Fi
/// station 100, Ethernet 50).
const ROUTE_PRIORITY: u32 = 150;

/// RMII fixed pins: TXD0, TX_EN, TXD1, RXD0, RXD1, CRS_DV.
pub type RmiiPins = (Gpio19<'static>, Gpio21<'static>, Gpio22<'static>, Gpio25<'static>, Gpio26<'static>, Gpio27<'static>);

/// The peripherals Ethernet takes.
pub struct EthParts {
    mac: MAC<'static>,
    pins: RmiiPins,
    clk: Gpio0<'static>,
    mdc: u8,
    mdio: u8,
    phy_addr: u8,
    power: Option<Output>,
}

impl EthParts {
    /// The board's RMII Ethernet, if any.
    pub fn from_profile(eth: &Ethernet, mac: MAC<'static>, pins: RmiiPins, clk: Gpio0<'static>) -> Option<Self> {
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

/// Start the EMAC and a thread that reports the link state: `report(text,
/// up, ip)`, `up` once the interface has an address.
pub fn start(parts: EthParts, sys_loop: EspSystemEventLoop, report: impl Fn(String, bool, String) + Send + 'static) {
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
        let conf = NetifConfiguration { route_priority: ROUTE_PRIORITY, ..NetifConfiguration::eth_default_client() };
        let mut eth = BlockingEth::wrap(EspEth::wrap_all(driver, EspNetif::new_with_conf(&conf)?)?, sys_loop)?;
        eth.start()?;
        Ok(eth)
    })();
    let eth = match result {
        Ok(e) => e,
        Err(e) => {
            warn!("ethernet: {e}");
            report(format!("error: {e}"), false, String::new());
            return;
        }
    };
    report("down (no link)".into(), false, String::new());
    let spawned = std::thread::Builder::new().name("eth".into()).stack_size(4096).spawn(move || {
        let mut last = String::new();
        loop {
            let up = eth.is_up().unwrap_or(false);
            let ip = if up { eth.eth().netif().get_ip_info().map(|i| i.ip.to_string()).unwrap_or_default() } else { String::new() };
            let text = match (eth.is_connected().unwrap_or(false), up) {
                (_, true) => format!("up {ip}"),
                (true, false) => "link up, waiting for DHCP".into(),
                (false, _) => "down (no link)".into(),
            };
            if text != last {
                info!("ethernet: {text}");
                report(text.clone(), up, ip);
                last = text;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    if let Err(e) = spawned {
        warn!("ethernet thread: {e}");
    }
}
