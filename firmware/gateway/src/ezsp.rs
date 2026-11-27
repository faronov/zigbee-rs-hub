//! The NCP UART as the EZSP backend's transport ([`BackendPort`]), and the
//! read-only EZSP probe of the NCP (`mg24 ezsp info`, in a bring-up boot
//! without the hub), with the host core of `hub-ezsp` over the NCP UART: ASH reset (the NCP restarts), EZSP
//! version negotiation, then queries only, nothing written:
//!
//! | Step | EZSP frame |
//! |---|---|
//! | protocol version, stack type, stack version | `version` (0x0000) |
//! | stack version and build | `getValue(VERSION_INFO)` (0x00AA, value 0x11) |
//! | EUI-64 | `getEui64` (0x0026) |
//! | manufacturing string, board name | `getMfgToken` (0x000B, tokens 0x01, 0x02) |
//! | custom EUI-64, crystal tuning (CTUNE) | `getMfgToken` (tokens 0x0C, 0x0D) |
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

use hub_ezsp::ezsp::{Callback, Command, ConfigId, NetworkInitBitmask, NetworkStatus, Response, StatusKind, ValueId};
use hub_ezsp::{EzspHost, HostConfig, HostEvent, LineErrors, SystemClock, Transport, VersionPolicy};

use crate::{lines, uart};

/// `EZSP_GET_MFG_TOKEN` (Gecko SDK 4.4.6 `ezsp-enum.h`).
const GET_MFG_TOKEN: u16 = 0x000B;
/// `EZSP_MFG_STRING`, `EZSP_MFG_BOARD_NAME` (same header).
const MFG_TOKENS: [(&str, u8); 2] = [("mfg string", 0x01), ("board name", 0x02)];
/// `EZSP_MFG_CUSTOM_EUI_64`: an EUI-64 that replaces the chip's own
/// (DEVINFO) one; all `FF` when unset.
const MFG_CUSTOM_EUI_64: u8 = 0x0C;
/// `EZSP_MFG_CTUNE`: the HFXO tuning the board's manufacturer measured
/// (`u16`); `FFFF` when unset (then DEVINFO or the firmware's default).
const MFG_CTUNE: u8 = 0x0D;

/// The NCP UART for probes (no parking: the caller has the UART).
pub(crate) struct Port;

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
    fn line_errors(&self) -> LineErrors {
        line_errors()
    }
}

/// The UART driver's receive errors since boot.
fn line_errors() -> LineErrors {
    use std::sync::atomic::Ordering::Relaxed;
    LineErrors { overruns: uart::OVERRUNS.load(Relaxed), errors: uart::LINE_ERRORS.load(Relaxed) }
}

/// The NCP UART as the hub's EZSP backend's transport: the UART is the
/// backend's while this lives (the whole boot).
pub struct BackendPort {
    _claim: uart::Claim,
}

impl BackendPort {
    /// The backend's port on the UART it claimed.
    pub fn from_claim(claim: uart::Claim) -> Self {
        Self { _claim: claim }
    }
}

/// While the UART is parked (an NCP update): nothing goes out, nothing
/// comes in (a silent NCP).
impl Transport for BackendPort {
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        if uart::parked() {
            return Ok(());
        }
        Port.write_all(data)
    }
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        if uart::parked() {
            std::thread::sleep(timeout.min(Duration::from_millis(100)));
            return Ok(0);
        }
        Port.read(buf, timeout)
    }
    fn discard_input(&mut self) -> io::Result<()> {
        if uart::parked() {
            return Ok(());
        }
        Port.discard_input()
    }
    fn line_errors(&self) -> LineErrors {
        line_errors()
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
    let st = host.start_times();
    out += &format!(
        "ASH v{} (RSTACK {:?}) at {} Bd; {}{} after {} version exchange(s), stack type {}, stack version {}; {} ms (reset {}, version {})\r\n",
        info.ash_version,
        info.reset_code,
        uart::baud(),
        ezsp,
        if ezsp.is_verified() { "" } else { " (not verified by hub-ezsp: EZSP >= 14 layout assumed)" },
        info.version_exchanges,
        info.stack_type,
        info.stack_version,
        t0.elapsed().as_millis(),
        st.reset_ms,
        st.version_ms
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
        match mfg_token(&mut host, token) {
            Ok(t) => out += &format!("{name}: {:?}\r\n", printable(&t)),
            Err(e) => out += &format!("{name}: {e}\r\n"),
        }
    }
    out += &custom_eui64(&mut host);
    out += &ctune(&mut host);
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
    out += &format!("host: {}; {} ms in all\r\n", host.counters(), t0.elapsed().as_millis());
    if init {
        drop(host);
        lines::reset_to_app();
        out += "MG24 reset (the resumed network does not keep running without a host)\r\n";
    }
    out
}

fn mfg_token<T: Transport, C: hub_ezsp::host::Clock>(host: &mut EzspHost<T, C>, token: u8) -> Result<Vec<u8>, String> {
    match host.call(&Command::Raw { frame_id: GET_MFG_TOKEN, params: vec![token] }) {
        Ok(Response::Raw { params, .. }) if !params.is_empty() => {
            let len = usize::from(params[0]).min(params.len() - 1);
            Ok(params[1..1 + len].to_vec())
        }
        Ok(r) => Err(format!("{r:?}")),
        Err(e) => Err(e.to_string()),
    }
}

/// The custom EUI-64 token: whether the EUI-64 in use is a token or the
/// chip's own (a firmware that replaces EmberZNet must keep the coordinator
/// IEEE the devices know).
fn custom_eui64<T: Transport, C: hub_ezsp::host::Clock>(host: &mut EzspHost<T, C>) -> String {
    match mfg_token(host, MFG_CUSTOM_EUI_64) {
        Ok(t) if t.len() == 8 && t.iter().all(|&b| b == 0xFF) => {
            "custom EUI-64 token: unset (the EUI-64 above is the chip's own, DEVINFO)\r\n".into()
        }
        Ok(t) if t.len() == 8 => {
            let be: String = t.iter().rev().map(|b| format!("{b:02X}")).collect();
            format!("custom EUI-64 token: {be} (the EUI-64 in use comes from this token, not from the chip)\r\n")
        }
        Ok(t) => format!("custom EUI-64 token: {} bytes {t:02X?}\r\n", t.len()),
        Err(e) => format!("custom EUI-64 token: {e}\r\n"),
    }
}

/// The CTUNE token (HFXO load capacitance tuning).
fn ctune<T: Transport, C: hub_ezsp::host::Clock>(host: &mut EzspHost<T, C>) -> String {
    match mfg_token(host, MFG_CTUNE) {
        Ok(t) if t.len() >= 2 => match u16::from_le_bytes([t[0], t[1]]) {
            0xFFFF => "MFG_CTUNE token: unset (the chip's DEVINFO value or the firmware's default applies)\r\n".into(),
            v => format!("MFG_CTUNE token: {v} (0x{v:03X})\r\n"),
        },
        Ok(t) => format!("MFG_CTUNE token: {} bytes {t:02X?}\r\n", t.len()),
        Err(e) => format!("MFG_CTUNE token: {e}\r\n"),
    }
}

/// The configuration values `mg24 ezsp config` reads.
const CONFIG_IDS: &[ConfigId] = &[
    ConfigId::PACKET_BUFFER_COUNT,
    ConfigId::NEIGHBOR_TABLE_SIZE,
    ConfigId::APS_UNICAST_MESSAGE_COUNT,
    ConfigId::BINDING_TABLE_SIZE,
    ConfigId::ADDRESS_TABLE_SIZE,
    ConfigId::MULTICAST_TABLE_SIZE,
    ConfigId::ROUTE_TABLE_SIZE,
    ConfigId::DISCOVERY_TABLE_SIZE,
    ConfigId::STACK_PROFILE,
    ConfigId::SECURITY_LEVEL,
    ConfigId::MAX_HOPS,
    ConfigId::MAX_END_DEVICE_CHILDREN,
    ConfigId::INDIRECT_TRANSMISSION_TIMEOUT,
    ConfigId::END_DEVICE_POLL_TIMEOUT,
    ConfigId::TRUST_CENTER_ADDRESS_CACHE_SIZE,
    ConfigId::SOURCE_ROUTE_TABLE_SIZE,
    ConfigId::FRAGMENT_WINDOW_SIZE,
    ConfigId::FRAGMENT_DELAY_MS,
    ConfigId::KEY_TABLE_SIZE,
    ConfigId::APS_ACK_TIMEOUT,
    ConfigId::APPLICATION_ZDO_FLAGS,
    ConfigId::BROADCAST_TABLE_SIZE,
    ConfigId::SUPPORTED_NETWORKS,
    ConfigId::TRANSIENT_KEY_TIMEOUT_S,
    ConfigId::GP_PROXY_TABLE_SIZE,
    ConfigId::GP_SINK_TABLE_SIZE,
];

/// `mg24 ezsp config [ID=value ...]` (bring-up boot): ASH reset, then the
/// given configuration values set in order (the NCP's RAM only, IDs in hex
/// or decimal), then every value the NCP reports. An ASH reset ends it, so
/// nothing stays set.
pub fn config(sets: &[&str]) -> String {
    let mut pairs = Vec::new();
    for a in sets {
        let parsed = a.split_once('=').and_then(|(k, v)| {
            let k = k.strip_prefix("0x").map_or_else(|| k.parse::<u8>().ok(), |h| u8::from_str_radix(h, 16).ok());
            Some((ConfigId(k?), v.parse::<u16>().ok()?))
        });
        match parsed {
            Some(p) => pairs.push(p),
            None => return format!("bad `{a}`: ID=value, e.g. 0x06=16\r\n"),
        }
    }
    let run = std::thread::Builder::new().name("ezsp-probe".into()).stack_size(12 * 1024).spawn(move || config_run(pairs));
    match run.map(|h| h.join()) {
        Ok(Ok(s)) => s,
        Ok(Err(_)) => "probe thread panicked\r\n".into(),
        Err(e) => format!("probe thread: {e}\r\n"),
    }
}

fn config_run(pairs: Vec<(ConfigId, u16)>) -> String {
    let _claim = match uart::claim(uart::User::Console) {
        Ok(c) => c,
        Err(u) => return format!("MG24 UART busy ({u:?})\r\n"),
    };
    let cfg = HostConfig { policy: VersionPolicy { accept_unverified: true }, ..HostConfig::default() };
    let (mut host, _events) = match EzspHost::new(Port, SystemClock::default(), cfg) {
        Ok(h) => h,
        Err(e) => return format!("EZSP host: {e}\r\n"),
    };
    let t0 = Instant::now();
    if let Err(e) = host.start() {
        return format!("EZSP start failed: {e}\r\n");
    }
    let mut out = format!("started in {} ms\r\n", t0.elapsed().as_millis());
    for (id, v) in pairs {
        let t = Instant::now();
        let r = host.set_configuration_value(id, v);
        let after = host.get_configuration_value(id);
        out += &format!("set {id:?} = {v}: {r:?} ({} ms), now {after:?}\r\n", t.elapsed().as_millis());
    }
    for id in CONFIG_IDS {
        match host.get_configuration_value(*id) {
            Ok(v) => out += &format!("{id:?} = {v}\r\n"),
            Err(e) => out += &format!("{id:?}: {e}\r\n"),
        }
    }
    out += &format!("host: {}; {} ms in all\r\n", host.counters(), t0.elapsed().as_millis());
    drop(host);
    lines::reset_to_app();
    out += "MG24 reset\r\n";
    out
}
