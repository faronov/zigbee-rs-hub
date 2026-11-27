//! Which coordinator backend a board whose NCP firmware can change runs
//! (the SONOFF Dongle-M's MG24: SONOFF's EmberZNet NCP, EZSP, or the
//! zigbee-rs ZRHP NCP after the hub replaced it through the Gecko
//! bootloader), decided at boot from a setting ([`Setting`]: `auto`,
//! `ezsp`, `zrhp`) and, in `auto`, by asking the NCP ([`detect`]).
//!
//! **Probe order** ([`order`]). The NCP that answered last time
//! (`hint`) is asked first, so a normal boot costs one probe; without a
//! hint EZSP goes first (the stock firmware). The other protocol follows,
//! then the Gecko bootloader's menu. When the last NCP update did not
//! finish (`suspect_bootloader`) the bootloader goes first.
//!
//! **A probe of the wrong protocol changes nothing on the NCP**: the EZSP
//! probe is an ASH reset (`RST`, `1A C0 38 BC 7E` at 115 200 Bd) and the
//! version command; a ZRHP NCP drops the bytes (no COBS delimiter, a CRC
//! that does not match; at 460 800 Bd they are not even characters). The
//! ZRHP probe is a link handshake and `SYS_NEGOTIATE`/`SYS_GET_INFO`; an
//! EmberZNet NCP drops them as ASH frames with bad CRCs, and the EZSP
//! probe's reset starts its link afresh. Neither writes NVM. The
//! bootloader probe sends a newline: an application ignores it, the
//! bootloader prints its menu (never `1` or `2`, which would start an
//! upload or boot).
//!
//! **A bootloader found** (an interrupted update): the hint's backend (or
//! the setting's) starts with its UART parked ([`Detection::park`]): the
//! hub runs, says so, and offers to flash again; nothing is sent to the
//! bootloader's menu.
//!
//! [`after_switch`] decides what happens once the hub runs on another
//! backend than before an NCP update: restore the network kept before it,
//! or why not.

use hub_coord::BackendKind;
use serde_json::Value as J;

/// The persisted choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Setting {
    /// Ask the NCP at boot.
    #[default]
    Auto,
    /// EmberZNet (EZSP), no probe.
    Ezsp,
    /// zigbee-rs (ZRHP), no probe.
    Zrhp,
}

impl Setting {
    /// From the stored or requested text.
    pub fn parse(s: &str) -> Option<Setting> {
        match s {
            "auto" => Some(Setting::Auto),
            "ezsp" => Some(Setting::Ezsp),
            "zrhp" => Some(Setting::Zrhp),
            _ => None,
        }
    }

    /// Stored and API text.
    pub fn as_str(self) -> &'static str {
        match self {
            Setting::Auto => "auto",
            Setting::Ezsp => "ezsp",
            Setting::Zrhp => "zrhp",
        }
    }

    /// The backend a fixed setting names.
    pub fn fixed(self) -> Option<BackendKind> {
        match self {
            Setting::Auto => None,
            Setting::Ezsp => Some(BackendKind::Ezsp),
            Setting::Zrhp => Some(BackendKind::Zrhp),
        }
    }
}

/// One question to the NCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// ASH reset and EZSP version (115 200 Bd).
    Ezsp,
    /// ZRHP link handshake and negotiation (460 800 Bd, then 115 200).
    Zrhp,
    /// The Gecko bootloader's menu (115 200 Bd).
    Bootloader,
}

impl Probe {
    /// API text.
    pub fn as_str(self) -> &'static str {
        match self {
            Probe::Ezsp => "ezsp",
            Probe::Zrhp => "zrhp",
            Probe::Bootloader => "bootloader",
        }
    }

    fn of(kind: BackendKind) -> Probe {
        if kind == BackendKind::Zrhp { Probe::Zrhp } else { Probe::Ezsp }
    }
}

/// What answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// An EmberZNet NCP.
    Ezsp {
        /// EZSP protocol and stack versions.
        version: String,
        /// Its EUI-64 (as `getEui64`, little endian), when it answered.
        eui64: Option<[u8; 8]>,
    },
    /// A zigbee-rs ZRHP NCP.
    Zrhp {
        /// `SYS_NEGOTIATE.fw`.
        fw: String,
        /// `SYS_NEGOTIATE.build_id`.
        build_id: [u8; 8],
        /// `SYS_NEGOTIATE.chip`.
        chip: u8,
        /// `SYS_GET_INFO.ieee_active` (little endian), when it answered.
        ieee: Option<[u8; 8]>,
        /// The host line of the session.
        baud: u32,
    },
    /// The Gecko bootloader's menu.
    Bootloader {
        /// Its version, e.g. `2.04.02`.
        version: Option<String>,
    },
}

impl Found {
    /// The backend that talks to it (`None`: the bootloader).
    pub fn backend(&self) -> Option<BackendKind> {
        match self {
            Found::Ezsp { .. } => Some(BackendKind::Ezsp),
            Found::Zrhp { .. } => Some(BackendKind::Zrhp),
            Found::Bootloader { .. } => None,
        }
    }

    /// One line for logs and the console.
    pub fn describe(&self) -> String {
        match self {
            Found::Ezsp { version, eui64 } => {
                format!("EmberZNet NCP, {version}{}", eui64.map_or(String::new(), |e| format!(", EUI-64 {}", be_hex(&e))))
            }
            Found::Zrhp { fw, build_id, chip, ieee, baud } => format!(
                "zigbee-rs ZRHP NCP {fw} (build {}, chip {chip}) at {baud} Bd{}",
                hex(build_id),
                ieee.map_or(String::new(), |e| format!(", IEEE {}", be_hex(&e)))
            ),
            Found::Bootloader { version } => format!("Gecko bootloader{}", version.as_ref().map_or(String::new(), |v| format!(" v{v}"))),
        }
    }
}

/// Lowercase hex.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// An EUI-64 (wire order) as the API shows it: big-endian hex.
pub fn be_hex(b: &[u8; 8]) -> String {
    let mut r = *b;
    r.reverse();
    hex(&r)
}

/// Asks the NCP (the firmware's UART; simulated NCPs in tests).
pub trait Prober {
    /// One probe: what answered, or why nothing did.
    fn probe(&mut self, p: Probe) -> Result<Found, String>;
}

/// The decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// The setting it was made with.
    pub setting: Setting,
    /// The backend to start.
    pub backend: BackendKind,
    /// What answered (`None`: nothing, or no probe).
    pub found: Option<Found>,
    /// The probes in order, with their failure.
    pub tried: Vec<(Probe, Option<String>)>,
    /// The NCP sits in its bootloader: start the backend with its UART
    /// parked (nothing sent to the menu).
    pub park: bool,
    /// Why this backend.
    pub why: &'static str,
}

impl Detection {
    /// `ncp/status` `backend` object.
    pub fn to_json(&self) -> J {
        serde_json::json!({
            "setting": self.setting.as_str(),
            "running": self.backend.as_str(),
            "found": self.found.as_ref().map(Found::describe),
            "why": self.why,
            "bootloader": self.park,
            "probes": self.tried.iter().map(|(p, e)| serde_json::json!({"probe": p.as_str(), "error": e})).collect::<Vec<_>>(),
        })
    }
}

/// The probes to make, in order.
pub fn order(setting: Setting, hint: Option<BackendKind>, suspect_bootloader: bool) -> Vec<Probe> {
    let mut v = Vec::new();
    if suspect_bootloader {
        v.push(Probe::Bootloader);
    }
    if setting == Setting::Auto {
        let first = hint.map_or(Probe::Ezsp, Probe::of);
        v.push(first);
        v.push(if first == Probe::Ezsp { Probe::Zrhp } else { Probe::Ezsp });
        if !suspect_bootloader {
            v.push(Probe::Bootloader);
        }
    }
    v
}

/// Decides the backend: a fixed setting without probes (but for the
/// bootloader when an update did not finish), `auto` by [`order`].
pub fn detect(setting: Setting, hint: Option<BackendKind>, suspect_bootloader: bool, prober: &mut dyn Prober) -> Detection {
    let fallback = setting.fixed().or(hint).unwrap_or(BackendKind::Ezsp);
    let mut d = Detection { setting, backend: fallback, found: None, tried: Vec::new(), park: false, why: "" };
    for p in order(setting, hint, suspect_bootloader) {
        match prober.probe(p) {
            Ok(found) => {
                d.tried.push((p, None));
                match found.backend() {
                    Some(b) => {
                        d.backend = b;
                        d.why = "detected";
                    }
                    None => {
                        d.park = true;
                        d.why = "the NCP is in its bootloader: an NCP update did not finish";
                    }
                }
                d.found = Some(found);
                if setting == Setting::Auto || d.park {
                    return d;
                }
            }
            Err(e) => d.tried.push((p, Some(e))),
        }
    }
    d.why = match (setting.fixed(), hint) {
        (Some(_), _) => "setting",
        (None, Some(_)) => "nothing answered: the backend of the last NCP seen",
        (None, None) => "nothing answered: EZSP, the stock firmware",
    };
    d
}

/// Short answers of the probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeTimes {
    /// ASH `RSTACK` wait per reset (EmberZNet answers within ~1 s).
    pub ezsp_rstack_ms: u32,
    /// ASH resets after the first.
    pub ezsp_reset_retries: u8,
    /// ZRHP: time on each line for a session.
    pub zrhp_per_line: std::time::Duration,
}

impl Default for ProbeTimes {
    fn default() -> Self {
        ProbeTimes { ezsp_rstack_ms: 1500, ezsp_reset_retries: 1, zrhp_per_line: std::time::Duration::from_millis(1800) }
    }
}

/// The EZSP probe: ASH reset, version negotiation, `getEui64`; nothing
/// written. The host is dropped afterwards (the backend resets the NCP
/// again when it starts).
#[cfg(feature = "ezsp")]
pub fn probe_ezsp<T, C>(transport: T, clock: C, times: ProbeTimes) -> Result<Found, String>
where
    T: hub_ezsp::Transport,
    C: hub_ezsp::host::Clock,
{
    use hub_ezsp::{EzspHost, HostConfig, VersionPolicy};
    let mut cfg = HostConfig { policy: VersionPolicy { accept_unverified: true }, ..HostConfig::default() };
    cfg.ash.rstack_timeout_ms = times.ezsp_rstack_ms;
    cfg.ash.reset_retries = times.ezsp_reset_retries;
    cfg.command_timeout = std::time::Duration::from_secs(2);
    let (mut host, _events) = EzspHost::new(transport, clock, cfg).map_err(|e| format!("EZSP host: {e}"))?;
    let info = host.start().map_err(|e| format!("no EZSP answer: {e}"))?;
    let eui64 = host.get_eui64().ok().map(|e| e.0);
    Ok(Found::Ezsp { version: format!("{}, stack {}", info.ezsp_version, info.stack_version), eui64 })
}

/// The ZRHP probe: a host link session on each of `lines` in turn
/// (`SYS_NEGOTIATE`, `SYS_GET_INFO`), then the driver stops. `cookie`:
/// random and non-zero (spec 5.5); the backend's own session later uses
/// another one, as after a host restart.
#[cfg(feature = "zrhp")]
pub fn probe_zrhp<R, T>(rx: R, tx: T, lines: Vec<hub_ncp::transport::Line>, cookie: u32, times: ProbeTimes) -> Result<Found, String>
where
    R: hub_ncp::transport::PortRx + 'static,
    T: hub_ncp::transport::PortTx + 'static,
{
    use hub_ncp::{DriverConfig, SessionState};
    let n = lines.len().max(1) as u32;
    let cfg = DriverConfig {
        host_name: "zigbee-rs-hub probe".into(),
        lines,
        hunt_after: times.zrhp_per_line,
        ready_timeout: std::time::Duration::from_millis(300),
        auto_ready: true,
        stack_size: 16 * 1024,
        ..DriverConfig::default()
    };
    let (client, _events) = hub_ncp::driver::start(cfg, rx, tx, cookie.max(1));
    let got = client.wait_state(times.zrhp_per_line * n, |s| matches!(s, SessionState::Ready | SessionState::Incompatible));
    let session = client.session();
    client.shutdown();
    // The port is the next probe's or the backend's: the driver stops
    // writing first, the reader after its current read (100 ms).
    client.wait_state(std::time::Duration::from_secs(1), |s| s == SessionState::Stopped);
    std::thread::sleep(std::time::Duration::from_millis(150));
    match (got, session) {
        (Some(SessionState::Ready), Some(s)) => Ok(Found::Zrhp {
            fw: s.negotiated.fw.clone(),
            build_id: s.negotiated.build_id,
            chip: s.negotiated.chip,
            ieee: s.info.as_ref().map(|i| i.ieee_active),
            baud: s.line.baud,
        }),
        (Some(SessionState::Incompatible), _) => Err("a ZRHP NCP of an incompatible protocol version".into()),
        _ => Err("no ZRHP session".into()),
    }
}

/// What to do once the hub runs on the NCP that replaced another
/// ([`after_switch`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterSwitch {
    /// Restore the kept backup (the NCP has no network).
    Restore,
    /// Nothing: the NCP runs the backup's network already.
    Done,
    /// Not automatically: why (the operator decides).
    Refuse(String),
}

/// Whether the network kept before an NCP update (`backup`, OCB) goes onto
/// the new NCP now, given its `network/info`. Only onto an NCP without a
/// network, and only under the backup's coordinator IEEE: the devices know
/// the Trust Center by it (APS security, the TC address), so a new NCP
/// under another IEEE (one without an IEEE override from the host, as the
/// zigbee-rs MG24 NCP, which takes EmberZNet's: the custom EUI-64 token,
/// else DEVINFO) would leave them unable to talk to it; that is the
/// operator's decision.
pub fn after_switch(info: &J, backup: &J) -> AfterSwitch {
    let epid = backup["extended_pan_id"].as_str().unwrap_or_default().to_ascii_lowercase();
    if epid.is_empty() || backup["network_key"]["key"].as_str().is_none() {
        return AfterSwitch::Refuse("the kept backup has no network or no network key".into());
    }
    let running = info["ext_pan_id"].as_str().map(str::to_ascii_lowercase);
    if info["state"] != "none" {
        return if running.as_deref() == Some(epid.as_str()) {
            AfterSwitch::Done
        } else {
            AfterSwitch::Refuse(format!(
                "the NCP runs another network ({}): restore the backup by hand (network/restore with force) if it is to replace it",
                running.unwrap_or_else(|| "unknown".into())
            ))
        };
    }
    let want = backup["coordinator_ieee"].as_str().map(str::to_ascii_lowercase);
    let have = info["ncp"]["ieee"].as_str().map(str::to_ascii_lowercase);
    if let (Some(want), Some(have)) = (want, have)
        && want != have
    {
        return AfterSwitch::Refuse(format!(
            "the backup's coordinator IEEE {want} differs from the NCP's {have}: the devices know the Trust Center by it; \
             restore by hand (network/restore) only onto an NCP that can take the backup's IEEE"
        ));
    }
    AfterSwitch::Restore
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Answers from a script; records the order asked.
    struct Script {
        ezsp: bool,
        zrhp: bool,
        bootloader: bool,
        asked: Vec<Probe>,
    }

    impl Prober for Script {
        fn probe(&mut self, p: Probe) -> Result<Found, String> {
            self.asked.push(p);
            let ok = match p {
                Probe::Ezsp => self.ezsp,
                Probe::Zrhp => self.zrhp,
                Probe::Bootloader => self.bootloader,
            };
            if !ok {
                return Err("no answer".into());
            }
            Ok(match p {
                Probe::Ezsp => Found::Ezsp { version: "EZSP 13".into(), eui64: None },
                Probe::Zrhp => Found::Zrhp { fw: "0.1.0".into(), build_id: [1; 8], chip: 12, ieee: None, baud: 460_800 },
                Probe::Bootloader => Found::Bootloader { version: Some("2.04.02".into()) },
            })
        }
    }

    fn ncp(ezsp: bool, zrhp: bool, bootloader: bool) -> Script {
        Script { ezsp, zrhp, bootloader, asked: Vec::new() }
    }

    #[test]
    fn the_last_ncp_seen_is_asked_first() {
        use Probe::*;
        assert_eq!(order(Setting::Auto, None, false), [Ezsp, Zrhp, Bootloader]);
        assert_eq!(order(Setting::Auto, Some(BackendKind::Ezsp), false), [Ezsp, Zrhp, Bootloader]);
        assert_eq!(order(Setting::Auto, Some(BackendKind::Zrhp), false), [Zrhp, Ezsp, Bootloader]);
        assert_eq!(order(Setting::Auto, Some(BackendKind::Zrhp), true), [Bootloader, Zrhp, Ezsp]);
        assert!(order(Setting::Ezsp, Some(BackendKind::Zrhp), false).is_empty(), "a fixed setting asks nothing");
        assert_eq!(order(Setting::Zrhp, None, true), [Bootloader]);
    }

    #[test]
    fn auto_takes_what_answers() {
        let mut s = ncp(false, true, false);
        let d = detect(Setting::Auto, Some(BackendKind::Ezsp), false, &mut s);
        assert_eq!((d.backend, d.park, d.why), (BackendKind::Zrhp, false, "detected"));
        assert_eq!(s.asked, [Probe::Ezsp, Probe::Zrhp], "stops at the first answer");
        assert_eq!(d.tried[0].1.as_deref(), Some("no answer"));

        let mut s = ncp(true, false, false);
        let d = detect(Setting::Auto, Some(BackendKind::Zrhp), false, &mut s);
        assert_eq!(d.backend, BackendKind::Ezsp);
        assert_eq!(d.to_json()["probes"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_bootloader_parks_the_last_backend() {
        let mut s = ncp(false, false, true);
        let d = detect(Setting::Auto, Some(BackendKind::Zrhp), false, &mut s);
        assert_eq!((d.backend, d.park), (BackendKind::Zrhp, true));
        assert_eq!(s.asked, [Probe::Zrhp, Probe::Ezsp, Probe::Bootloader]);
        // An update that did not finish: the bootloader first, nothing
        // else is sent.
        let mut s = ncp(true, true, true);
        let d = detect(Setting::Auto, Some(BackendKind::Ezsp), true, &mut s);
        assert_eq!((d.backend, d.park, s.asked.len()), (BackendKind::Ezsp, true, 1));
        // A fixed setting with an update that did not finish.
        let mut s = ncp(false, false, true);
        let d = detect(Setting::Zrhp, Some(BackendKind::Ezsp), true, &mut s);
        assert_eq!((d.backend, d.park), (BackendKind::Zrhp, true));
    }

    #[test]
    fn nothing_answers_the_last_backend_runs_unparked() {
        let mut s = ncp(false, false, false);
        let d = detect(Setting::Auto, Some(BackendKind::Zrhp), false, &mut s);
        assert_eq!((d.backend, d.park, d.found.is_none()), (BackendKind::Zrhp, false, true));
        let d = detect(Setting::Auto, None, false, &mut ncp(false, false, false));
        assert_eq!(d.backend, BackendKind::Ezsp);
        let d = detect(Setting::Zrhp, None, false, &mut ncp(true, false, false));
        assert_eq!((d.backend, d.why), (BackendKind::Zrhp, "setting"));
    }

    fn backup(ieee: &str) -> J {
        json!({"extended_pan_id": "dddddddddddddddd", "coordinator_ieee": ieee, "network_key": {"key": "00".repeat(16)}})
    }

    #[test]
    fn a_kept_network_goes_only_onto_an_empty_ncp_under_its_ieee() {
        let empty = json!({"state": "none", "ncp": {"ieee": "020100000000515a"}});
        assert_eq!(after_switch(&empty, &backup("020100000000515A")), AfterSwitch::Restore);
        assert!(matches!(after_switch(&empty, &backup("b0e8e8fffe5d4d03")), AfterSwitch::Refuse(w) if w.contains("coordinator IEEE")));
        // The IEEE unknown (no SYS_GET_INFO): the NCP decides.
        assert_eq!(after_switch(&json!({"state": "none"}), &backup("b0e8e8fffe5d4d03")), AfterSwitch::Restore);
        let same = json!({"state": "up", "ext_pan_id": "dddddddddddddddd"});
        assert_eq!(after_switch(&same, &backup("x")), AfterSwitch::Done);
        let other = json!({"state": "up", "ext_pan_id": "eeeeeeeeeeeeeeee"});
        assert!(matches!(after_switch(&other, &backup("x")), AfterSwitch::Refuse(w) if w.contains("another network")));
        assert!(matches!(after_switch(&empty, &json!({"extended_pan_id": "dd"})), AfterSwitch::Refuse(_)));
    }

    #[test]
    fn settings_round_trip() {
        for s in [Setting::Auto, Setting::Ezsp, Setting::Zrhp] {
            assert_eq!(Setting::parse(s.as_str()), Some(s));
        }
        assert_eq!(Setting::parse("znp"), None);
    }
}
