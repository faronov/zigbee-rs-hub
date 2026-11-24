//! Read-only EZSP probe of the NCP (`mg24 ezsp info`), with the host core
//! of `hub-ezsp` over the NCP UART: ASH reset (the NCP restarts), EZSP
//! version negotiation, then queries only, nothing written:
//!
//! | Step | EZSP frame |
//! |---|---|
//! | protocol version, stack type, stack version | `version` (0x0000) |
//! | stack version and build | `getValue(VERSION_INFO)` (0x00AA, value 0x11) |
//! | EUI-64 | `getEui64` (0x0026) |
//! | manufacturing string, board name | `getMfgToken` (0x000B, tokens 0x01, 0x02) |
//! | network state | `networkState` (0x0018) |
//! | node type, PAN, channel, … (joined only) | `getNetworkParameters` (0x0028), `getNodeId` (0x0027) |
//!
//! After an ASH reset an EmberZNet NCP has no network up until the host
//! calls `networkInit`, so the state is normally `NO_NETWORK` even when
//! the NCP stores one. `mg24 ezsp info init` also calls `networkInit` (it
//! resumes a stored network; it does not form, leave or change any
//! configuration) and resets the MG24 afterwards, so the network is not
//! left running without a host. No `setConfigurationValue`, `setValue`,
//! policies, keys, form, leave or bootloader launch.

use std::io;
use std::time::{Duration, Instant};

use hub_ezsp::ezsp::{Callback, Command, NetworkInitBitmask, NetworkStatus, Response, StatusKind, ValueId};
use hub_ezsp::{EzspHost, HostConfig, HostEvent, SystemClock, Transport, VersionPolicy};

use crate::{lines, uart};

/// `EZSP_GET_MFG_TOKEN` (Gecko SDK 4.4.6 `ezsp-enum.h`).
const GET_MFG_TOKEN: u16 = 0x000B;
/// `EZSP_MFG_STRING`, `EZSP_MFG_BOARD_NAME` (same header).
const MFG_TOKENS: [(&str, u8); 2] = [("mfg string", 0x01), ("board name", 0x02)];

struct Port;

impl Transport for Port {
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        uart::write_all(data)
    }
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        uart::read(buf, timeout.as_millis().min(1000) as u32)
    }
    fn discard_input(&mut self) -> io::Result<()> {
        // Let a frame in progress finish, then drop everything.
        std::thread::sleep(Duration::from_millis(20));
        uart::flush_input();
        Ok(())
    }
}

fn printable(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0 || c == 0xFF).unwrap_or(b.len());
    b[..end].iter().map(|&c| if (0x20..0x7F).contains(&c) { c as char } else { '.' }).collect()
}

/// Run the probe (on its own thread: the host keeps a few KiB on the stack).
pub fn info(init: bool) -> String {
    let run = std::thread::Builder::new().name("ezsp-probe".into()).stack_size(12 * 1024).spawn(move || probe(init));
    match run.map(|h| h.join()) {
        Ok(Ok(s)) => s,
        Ok(Err(_)) => "probe thread panicked\r\n".into(),
        Err(e) => format!("probe thread: {e}\r\n"),
    }
}

fn probe(init: bool) -> String {
    let _claim = match uart::claim(uart::User::Console) {
        Ok(c) => c,
        Err(u) => return format!("MG24 UART busy ({u:?})\r\n"),
    };
    let mut out = String::new();
    let cfg = HostConfig { policy: VersionPolicy { accept_unverified: true }, ..HostConfig::default() };
    let (mut host, events) = match EzspHost::new(Port, SystemClock::default(), cfg) {
        Ok(h) => h,
        Err(e) => return format!("EZSP host: {e}\r\n"),
    };
    let t0 = Instant::now();
    let info = match host.start() {
        Ok(i) => i,
        Err(e) => {
            return format!(
                "EZSP start failed after {} ms at {} Bd: {e}\r\n(ASH {:?})\r\nThe MG24 may run other firmware (RCP, router), another rate, or sit in its bootloader: try `mg24 bootloader`, `mg24 reset`.\r\n",
                t0.elapsed().as_millis(),
                uart::baud(),
                host.ash().stats()
            );
        }
    };
    let ezsp = info.ezsp_version;
    out += &format!(
        "ASH v{} (RSTACK {:?}) at {} Bd; {}{} after {} version exchange(s), stack type {}, stack version {}; {} ms\r\n",
        info.ash_version,
        info.reset_code,
        uart::baud(),
        ezsp,
        if ezsp.is_verified() { "" } else { " (not verified by hub-ezsp: EZSP >= 14 layout assumed)" },
        info.version_exchanges,
        info.stack_type,
        info.stack_version,
        t0.elapsed().as_millis()
    );
    // VERSION_INFO: build (u16), major, minor, patch, special, type (UG100 successor §5).
    match host.call(&Command::GetValue { id: ValueId::VERSION_INFO }) {
        Ok(Response::GetValue { status, value }) if status.is_success() && value.len() >= 7 => {
            out += &format!(
                "stack: EmberZNet {}.{}.{}.{} build {} (type {})\r\n",
                value[2],
                value[3],
                value[4],
                value[5],
                u16::from_le_bytes([value[0], value[1]]),
                value[6]
            )
        }
        Ok(r) => out += &format!("stack: getValue(VERSION_INFO) answered {r:?}\r\n"),
        Err(e) => out += &format!("stack: getValue(VERSION_INFO): {e}\r\n"),
    }
    match host.get_eui64() {
        Ok(e) => out += &format!("EUI-64: {e}\r\n"),
        Err(e) => out += &format!("EUI-64: {e}\r\n"),
    }
    for (name, token) in MFG_TOKENS {
        match host.call(&Command::Raw { frame_id: GET_MFG_TOKEN, params: vec![token] }) {
            Ok(Response::Raw { params, .. }) if !params.is_empty() => {
                let len = usize::from(params[0]).min(params.len() - 1);
                out += &format!("{name}: {:?}\r\n", printable(&params[1..1 + len]));
            }
            Ok(r) => out += &format!("{name}: {r:?}\r\n"),
            Err(e) => out += &format!("{name}: {e}\r\n"),
        }
    }
    let mut state = host.network_state();
    if init && matches!(state, Ok(NetworkStatus::NO_NETWORK)) {
        match host.network_init(NetworkInitBitmask::NO_OPTIONS) {
            Ok(s) if s.is_success() => {
                out += "networkInit: success, waiting for the stack status\r\n";
                let until = Instant::now() + Duration::from_secs(6);
                while Instant::now() < until {
                    if let Err(e) = host.pump(Duration::from_millis(200)) {
                        out += &format!("link: {e}\r\n");
                        break;
                    }
                    let up = events.drain().into_iter().any(|ev| {
                        matches!(ev, HostEvent::Callback(Callback::StackStatus { status }) if status.kind() == StatusKind::NetworkUp)
                    });
                    if up {
                        out += "stackStatus: NETWORK_UP\r\n";
                        break;
                    }
                }
            }
            Ok(s) => out += &format!("networkInit: {s:?} (no stored network)\r\n"),
            Err(e) => out += &format!("networkInit: {e}\r\n"),
        }
        state = host.network_state();
    }
    match state {
        Ok(s) => {
            out += &format!("network state: {s:?}\r\n");
            if matches!(s, NetworkStatus::JOINED_NETWORK | NetworkStatus::JOINED_NETWORK_NO_PARENT) {
                match host.get_network_parameters() {
                    Ok((node_type, p)) => {
                        out += &format!(
                            "network: {node_type:?}, PAN 0x{:04X}, extended PAN {:016X}, channel {}, TX power {} dBm, update id {}, manager 0x{:04X}\r\n",
                            p.pan_id,
                            u64::from_le_bytes(p.extended_pan_id),
                            p.radio_channel,
                            p.radio_tx_power,
                            p.nwk_update_id,
                            p.nwk_manager_id
                        )
                    }
                    Err(e) => out += &format!("getNetworkParameters: {e}\r\n"),
                }
                match host.get_node_id() {
                    Ok(id) => out += &format!("node id: 0x{id:04X}\r\n"),
                    Err(e) => out += &format!("getNodeId: {e}\r\n"),
                }
            } else if !init {
                out += "(no network is up before networkInit; `mg24 ezsp info init` resumes a stored one)\r\n";
            }
        }
        Err(e) => out += &format!("networkState: {e}\r\n"),
    }
    let s = host.stats();
    let a = host.ash().stats();
    out += &format!(
        "host: {} commands, {} callbacks; ASH tx {} retx {} rx {} nak {}/{} bad crc {} ack timeouts {}; UART overruns {} errors {}\r\n",
        s.commands,
        s.callbacks,
        a.tx_data,
        a.tx_retransmit,
        a.rx_data,
        a.tx_nak,
        a.rx_nak,
        a.rx_bad_crc,
        a.ack_timeouts,
        uart::OVERRUNS.load(std::sync::atomic::Ordering::Relaxed),
        uart::LINE_ERRORS.load(std::sync::atomic::Ordering::Relaxed),
    );
    if init {
        drop(host);
        lines::reset_to_app();
        out += "MG24 reset (the resumed network does not keep running without a host)\r\n";
    }
    out
}
