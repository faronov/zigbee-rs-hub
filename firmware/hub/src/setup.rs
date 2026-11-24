//! The setup access point and its captive portal (README "First-time setup
//! without a console"; the rules: `hub_core::setup`).
//!
//! An open AP `zigbee-rs-hub-XXXX` (the end of the Wi-Fi MAC) next to the
//! station, its DHCP server naming the hub as DNS server and a DNS server
//! that answers every name with the AP's address, so a phone's connectivity
//! probe gets a redirect to the setup page and the phone opens it by itself.
//! The page (`hub_web::SETUP_HTML_GZ`) lists the networks of a scan, takes
//! SSID and password (and proves it has the API token, unless the hub is
//! fresh or someone at the hub opened the AP) and shows the station's
//! address and the mDNS names. Nothing that yields the API token crosses
//! the AP: the page sends an HMAC over a single-use nonce instead of the
//! token, and a fresh hub's page keeps a random setup key and sends only
//! its SHA-256; the web UI trades the key for the token on the station
//! address (`POST /setup/claim`, only from the home network).
//!
//! The station's mDNS responder (`zigbee-rs-hub.local` and the unique
//! `zigbee-rs-hub-xxxx.local`) lives here too. The Wi-Fi thread polls it
//! all every 100 ms: no threads of its own. Without the app (wrong-image
//! mode) only the mDNS responder runs.

use std::ffi::{CStr, CString};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use embedded_svc::wifi::{AccessPointConfiguration, AuthMethod, ClientConfiguration, Configuration};
use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::http::server::EspHttpServer;
use esp_idf_svc::sys::{self, EspError};
use esp_idf_svc::wifi::{BlockingWifi, EspWifi};
use hub_core::dns;
use hub_core::setup::{self, Claim, ClaimError, HubState, Inputs, Policy, Reason};
use log::{info, warn};
use serde_json::{Value as J, json};

use crate::mode;

/// S3 BOOT button (GPIO0, pulled up, low while pressed).
const BUTTON: sys::gpio_num_t = 0;
/// Polls (100 ms) the button must be held to open the AP.
const HOLD_POLLS: u16 = 30;
/// Networks listed on the page.
const MAX_NETWORKS: usize = 20;
const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);

/// AP SSID and host names.
pub struct Names {
    pub ap_ssid: String,
    pub host: String,
    mdns: [String; 2],
}

/// The names of this board (from its Wi-Fi MAC).
pub fn names() -> &'static Names {
    static NAMES: OnceLock<Names> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut mac = [0u8; 6];
        unsafe { sys::esp_read_mac(mac.as_mut_ptr(), sys::esp_mac_type_t_ESP_MAC_WIFI_STA) };
        let id = format!("{:02x}{:02x}", mac[4], mac[5]);
        Names {
            ap_ssid: format!("zigbee-rs-hub-{:02X}{:02X}", mac[4], mac[5]),
            host: format!("zigbee-rs-hub-{id}"),
            mdns: ["zigbee-rs-hub.local".into(), format!("zigbee-rs-hub-{id}.local")],
        }
    })
}

struct Shared {
    open: Option<Reason>,
    ap_ip: [u8; 4],
    /// SSID, RSSI, open network.
    networks: Vec<(String, i8, bool)>,
    scan: bool,
    scanning: bool,
    /// The setup key's commitment of a fresh hub (kept when the AP closes:
    /// the owner claims the token from the home network).
    claim: Claim,
    /// The token proof's nonce (`hub_core::setup::token_proof`), replaced
    /// at every opening and after every attempt.
    nonce: String,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared {
    open: None,
    ap_ip: [0; 4],
    networks: Vec::new(),
    scan: false,
    scanning: false,
    claim: Claim::new(),
    nonce: String::new(),
});
static REQUEST: AtomicBool = AtomicBool::new(false);

/// Open the AP now (console `wifi setup`; the BOOT button does the same).
pub fn request() {
    REQUEST.store(true, Ordering::SeqCst);
}

fn now_ms() -> u64 {
    unsafe { sys::esp_timer_get_time() as u64 / 1000 }
}

fn new_nonce() -> String {
    let mut b = [0u8; 8];
    unsafe { sys::esp_fill_random(b.as_mut_ptr().cast(), b.len()) };
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// What the open AP must not give away (`HubState::fresh`). Without the
/// hub core nothing counts as fresh.
pub fn hub_state() -> HubState {
    let token_unclaimed = mode::token_unclaimed();
    let Some(hub) = crate::HUB.get() else { return HubState { token_unclaimed, devices: 0, network: true } };
    let (stored, devices) = hub.with_store(|s| (s.state().network.is_some(), s.state().devices.len()));
    let ncp = hub.status();
    let app_devices = crate::APP.get().map_or(0, |a| a.status().devices);
    HubState {
        token_unclaimed,
        devices: devices.max(app_devices),
        network: stored || ncp.network_state != 0 || ncp.network.is_some(),
    }
}

/// `wifi.setup_ap` of `hub/system`, while the AP is open.
pub fn info() -> Option<J> {
    let s = SHARED.lock().unwrap();
    let reason = s.open?;
    Some(json!({"ssid": names().ap_ssid, "ip": Ipv4Addr::from(s.ap_ip).to_string(), "reason": format!("{reason:?}")}))
}

/// Station host name (DHCP), and the hub as the DNS server the AP's DHCP
/// server offers (esp-idf-svc offers 8.8.8.8, which a phone on the AP
/// cannot reach: its probes would fail instead of meeting the catch-all);
/// before the driver starts.
pub fn prepare(wifi: &EspWifi<'static>) {
    let host = CString::new(names().host.as_str()).unwrap();
    if let Err(e) = sys::esp!(unsafe { sys::esp_netif_set_hostname(wifi.sta_netif().handle(), host.as_ptr()) }) {
        warn!("setup: hostname: {e}");
    }
    let ip = wifi.ap_netif().get_ip_info().map(|i| i.ip.octets()).unwrap_or([192, 168, 71, 1]);
    SHARED.lock().unwrap().ap_ip = ip;
    let mut dns = sys::esp_netif_dns_info_t::default();
    // Network byte order in memory; `type_` 0 = IPv4.
    dns.ip.u_addr.ip4 = sys::esp_ip4_addr_t { addr: u32::from_ne_bytes(ip) };
    dns.ip.type_ = 0;
    let r = unsafe { sys::esp_netif_set_dns_info(wifi.ap_netif().handle(), sys::esp_netif_dns_type_t_ESP_NETIF_DNS_MAIN, &mut dns) };
    if r != sys::ESP_OK {
        warn!("setup: AP DNS server: {r}");
    }
    // GPIO function with the pull-up, then input.
    unsafe {
        sys::gpio_reset_pin(BUTTON);
        sys::gpio_set_direction(BUTTON, sys::gpio_mode_t_GPIO_MODE_INPUT);
    }
}

fn ap_clients() -> u8 {
    let mut list: sys::wifi_sta_list_t = unsafe { core::mem::zeroed() };
    if unsafe { sys::esp_wifi_ap_get_sta_list(&mut list) } == sys::ESP_OK { list.num as u8 } else { 0 }
}

/// The driver configuration: the station, plus the setup AP while it is open.
pub fn configuration(client: ClientConfiguration, ap_open: bool) -> Configuration {
    if !ap_open {
        return Configuration::Client(client);
    }
    let ap = AccessPointConfiguration {
        ssid: names().ap_ssid.as_str().try_into().unwrap_or_default(),
        auth_method: AuthMethod::None,
        max_connections: 2,
        ..Default::default()
    };
    Configuration::Mixed(client, ap)
}

/// Whether the setup AP is open.
pub fn is_open() -> bool {
    SHARED.lock().unwrap().open.is_some()
}

/// The Wi-Fi thread's part: AP on/off, scans, DNS and mDNS.
pub struct Portal {
    /// The setup AP may open (the app runs).
    ap: bool,
    policy: Policy,
    /// `hub_state().fresh()`, refreshed every second.
    fresh: bool,
    next_state: Instant,
    held: u16,
    dns: Option<UdpSocket>,
    mdns: Option<UdpSocket>,
    /// The station address `mdns` was opened for (also after a failure).
    mdns_ip: Option<Ipv4Addr>,
    next_scan: Instant,
    next_switch: Instant,
}

impl Portal {
    /// `ap`: whether the setup AP may open (not in wrong-image mode).
    pub fn new(ap: bool) -> Self {
        Self {
            ap,
            policy: Policy::default(),
            fresh: false,
            next_state: Instant::now(),
            held: 0,
            dns: None,
            mdns: None,
            mdns_ip: None,
            next_scan: Instant::now(),
            next_switch: Instant::now(),
        }
    }

    /// One poll; returns whether the AP is open and how many stations use it.
    pub fn poll(&mut self, wifi: &mut BlockingWifi<EspWifi<'static>>, client: &ClientConfiguration, sta_ip: Option<Ipv4Addr>) -> (bool, u8) {
        if !self.ap {
            self.serve_mdns(sta_ip);
            return (false, 0);
        }
        let configured = !client.ssid.is_empty();
        if Instant::now() >= self.next_state {
            self.fresh = hub_state().fresh();
            self.next_state = Instant::now() + Duration::from_secs(1);
        }
        let pressed = unsafe { sys::gpio_get_level(BUTTON) } == 0;
        self.held = if pressed { self.held.saturating_add(1) } else { 0 };
        if self.held == HOLD_POLLS {
            info!("setup: BOOT button held");
            request();
        }
        let is_open = SHARED.lock().unwrap().open.is_some();
        let clients = if is_open { ap_clients() } else { 0 };
        let request = REQUEST.swap(false, Ordering::SeqCst);
        if request {
            // Someone at the hub: a voided claim may start over.
            SHARED.lock().unwrap().claim.reset();
        }
        let inputs = Inputs { now_ms: now_ms(), configured, sta_up: sta_ip.is_some(), clients, request, fresh: self.fresh };
        let want = self.policy.tick(inputs);
        if want.is_some() != is_open {
            if Instant::now() >= self.next_switch {
                match self.switch(wifi, client, want.is_some()) {
                    Ok(()) => {
                        info!("setup: AP {} ({want:?})", if want.is_some() { "open" } else { "closed" });
                        SHARED.lock().unwrap().open = want;
                    }
                    Err(e) => {
                        warn!("setup: AP switch: {e}");
                        self.next_switch = Instant::now() + Duration::from_secs(5);
                    }
                }
            }
        } else if want.is_some() {
            SHARED.lock().unwrap().open = want;
        }
        let open = SHARED.lock().unwrap().open.is_some();
        if open {
            self.scan();
            self.answer_dns();
        }
        self.serve_mdns(sta_ip);
        (open, clients)
    }

    fn switch(&mut self, wifi: &mut BlockingWifi<EspWifi<'static>>, client: &ClientConfiguration, open: bool) -> Result<(), EspError> {
        wifi.set_configuration(&configuration(client.clone(), open))?;
        if open {
            // Phones follow the AP when the station moves it to its channel.
            let mut cfg: sys::wifi_config_t = unsafe { core::mem::zeroed() };
            if unsafe { sys::esp_wifi_get_config(sys::wifi_interface_t_WIFI_IF_AP, &mut cfg) } == sys::ESP_OK {
                cfg.ap.csa_count = 3;
                unsafe { sys::esp_wifi_set_config(sys::wifi_interface_t_WIFI_IF_AP, &mut cfg) };
            }
            let dns = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 53))).and_then(|d| d.set_nonblocking(true).map(|_| d));
            self.dns = dns.map_err(|e| warn!("setup: DNS socket: {e}")).ok();
            let mut s = SHARED.lock().unwrap();
            s.scan = true;
            s.nonce = new_nonce();
        } else {
            let mut s = SHARED.lock().unwrap();
            s.networks = Vec::new();
            s.open = None;
            drop(s);
            self.dns = None;
        }
        Ok(())
    }

    fn scan(&mut self) {
        {
            let mut s = SHARED.lock().unwrap();
            if !s.scan || Instant::now() < self.next_scan {
                return;
            }
            s.scanning = true;
        }
        let found = scan_now();
        let mut s = SHARED.lock().unwrap();
        s.scanning = false;
        match found {
            Ok(nets) => {
                s.networks = nets;
                s.scan = false;
            }
            // The station is busy connecting: try again shortly.
            Err(e) => {
                warn!("setup: scan: {e}");
                self.next_scan = Instant::now() + Duration::from_secs(3);
            }
        }
    }

    fn answer_dns(&mut self) {
        let Some(sock) = &self.dns else { return };
        let ip = SHARED.lock().unwrap().ap_ip;
        let mut buf = [0u8; 512];
        let mut out = [0u8; 512];
        while let Ok((n, src)) = sock.recv_from(&mut buf) {
            // Only the AP's own clients.
            let SocketAddr::V4(v4) = src else { continue };
            if v4.ip().octets()[..3] != ip[..3] {
                continue;
            }
            if let Some(len) = dns::catch_all(&buf[..n], ip, &mut out) {
                let _ = sock.send_to(&out[..len], src);
            }
        }
    }

    fn serve_mdns(&mut self, sta_ip: Option<Ipv4Addr>) {
        if self.mdns_ip != sta_ip {
            self.mdns_ip = sta_ip;
            self.mdns = sta_ip.and_then(|ip| mdns_socket(ip).map_err(|e| warn!("mdns: {e}")).ok());
        }
        let (Some(sock), Some(ip)) = (&self.mdns, self.mdns_ip) else { return };
        let names: [&str; 2] = [&names().mdns[0], &names().mdns[1]];
        let mut buf = [0u8; 512];
        let mut out = [0u8; 256];
        while let Ok((n, src)) = sock.recv_from(&mut buf) {
            let legacy = src.port() != 5353;
            if let Some(len) = dns::mdns(&buf[..n], &names, ip.octets(), legacy, &mut out) {
                let to = if legacy { src } else { SocketAddr::from((MDNS_GROUP, 5353)) };
                let _ = sock.send_to(&out[..len], to);
            }
        }
    }
}

/// A blocking scan (on the Wi-Fi thread, which owns the driver): the
/// strongest record per SSID, hidden networks left out; the page sorts them.
fn scan_now() -> Result<Vec<(String, i8, bool)>, EspError> {
    sys::esp!(unsafe { sys::esp_wifi_scan_start(core::ptr::null(), true) })?;
    let mut n = MAX_NETWORKS as u16;
    let mut recs: Vec<sys::wifi_ap_record_t> = vec![unsafe { core::mem::zeroed() }; MAX_NETWORKS];
    sys::esp!(unsafe { sys::esp_wifi_scan_get_ap_records(&mut n, recs.as_mut_ptr()) })?;
    let mut nets: Vec<(String, i8, bool)> = Vec::new();
    for r in &recs[..(n as usize).min(MAX_NETWORKS)] {
        let len = r.ssid.iter().position(|&b| b == 0).unwrap_or(r.ssid.len());
        let ssid = String::from_utf8_lossy(&r.ssid[..len]);
        if ssid.is_empty() {
            continue;
        }
        match nets.iter().position(|x| x.0 == ssid) {
            Some(i) => nets[i].1 = nets[i].1.max(r.rssi),
            None => nets.push((ssid.into_owned(), r.rssi, r.authmode == sys::wifi_auth_mode_t_WIFI_AUTH_OPEN)),
        }
    }
    Ok(nets)
}

fn mdns_socket(ip: Ipv4Addr) -> std::io::Result<UdpSocket> {
    let sock = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 5353)))?;
    sock.set_nonblocking(true)?;
    sock.join_multicast_v4(&MDNS_GROUP, &ip)?;
    sock.set_multicast_ttl_v4(255)?;
    let names: [&str; 2] = [&names().mdns[0], &names().mdns[1]];
    let mut out = [0u8; 256];
    if let Some(len) = dns::announce(&names, ip.octets(), &mut out) {
        sock.send_to(&out[..len], SocketAddr::from((MDNS_GROUP, 5353)))?;
    }
    info!("mdns: {} and {} at {ip}", names[0], names[1]);
    Ok(sock)
}

/// The local (`peer` false) or remote address of the request's
/// connection: IPv4, also as an IPv4-mapped IPv6 address when lwIP has
/// IPv6.
fn sock_ip(req: *mut sys::httpd_req_t, peer: bool) -> Option<[u8; 4]> {
    let fd = unsafe { sys::httpd_req_to_sockfd(req) };
    let mut ss: sys::sockaddr_storage = unsafe { core::mem::zeroed() };
    let mut len = core::mem::size_of::<sys::sockaddr_storage>() as sys::socklen_t;
    let r = unsafe {
        if peer {
            sys::lwip_getpeername(fd, (&raw mut ss).cast(), &mut len)
        } else {
            sys::lwip_getsockname(fd, (&raw mut ss).cast(), &mut len)
        }
    };
    if r != 0 {
        return None;
    }
    let b: &[u8] = unsafe { core::slice::from_raw_parts((&raw const ss).cast(), core::mem::size_of::<sys::sockaddr_storage>()) };
    match u32::from(b[1]) {
        sys::AF_INET => b.get(4..8)?.try_into().ok(),
        // Without IPv6 lwIP defines AF_INET6 as AF_UNSPEC (0).
        f if f != 0 && f == sys::AF_INET6 && b.get(8..18)?.iter().all(|&x| x == 0) && b.get(18..20)? == [0xFF, 0xFF] => {
            b.get(20..24)?.try_into().ok()
        }
        _ => None,
    }
}

/// Whether the request came through the open setup AP: to its address, or
/// from its subnet (lwIP takes a packet for the station's address on any
/// interface: a phone on the AP reaches the station address too). While the
/// AP is open an unknown address counts as through it.
pub fn via_ap(req: *mut sys::httpd_req_t) -> bool {
    let (open, ap_ip) = {
        let s = SHARED.lock().unwrap();
        (s.open.is_some(), s.ap_ip)
    };
    if !open {
        return false;
    }
    match (sock_ip(req, false), sock_ip(req, true)) {
        (Some(local), Some(peer)) => local == ap_ip || peer[..3] == ap_ip[..3],
        _ => true,
    }
}

/// Whether the request is for the open AP's own address (the setup page and
/// its endpoints answer only there).
fn to_open_ap(req: *mut sys::httpd_req_t) -> bool {
    let (open, ap_ip) = {
        let s = SHARED.lock().unwrap();
        (s.open.is_some(), s.ap_ip)
    };
    open && sock_ip(req, false) == Some(ap_ip)
}

/// Where a request that came in through the open AP goes: the setup page
/// (also for the web UI on the station address, which would send the
/// browser's stored API token over the AP).
pub fn ap_redirect(req: *mut sys::httpd_req_t) -> Option<String> {
    let ap_ip = SHARED.lock().unwrap().ap_ip;
    via_ap(req).then(|| format!("http://{}/setup", Ipv4Addr::from(ap_ip)))
}

/// Sends a response (raw esp_http_server: far smaller than a generic
/// `EspHttpServer` handler per endpoint).
fn send(req: *mut sys::httpd_req_t, status: &CStr, ctype: &CStr, headers: &[(&CStr, &str)], body: &[u8]) -> sys::esp_err_t {
    crate::net::close_after(req);
    // esp_http_server keeps the pointers until the response is sent.
    let values: Vec<CString> = headers.iter().map(|(_, v)| CString::new(*v).unwrap_or_default()).collect();
    unsafe {
        sys::httpd_resp_set_status(req, status.as_ptr());
        sys::httpd_resp_set_type(req, ctype.as_ptr());
        for ((k, _), v) in headers.iter().zip(&values) {
            sys::httpd_resp_set_hdr(req, k.as_ptr(), v.as_ptr());
        }
        sys::httpd_resp_set_hdr(req, c"Connection".as_ptr(), c"close".as_ptr());
        sys::httpd_resp_send(req, body.as_ptr().cast(), body.len() as isize)
    }
}

fn reply(req: *mut sys::httpd_req_t, status: &CStr, body: &J) -> sys::esp_err_t {
    send(req, status, c"application/json", &[(c"Cache-Control", "no-store")], body.to_string().as_bytes())
}

fn redirect(req: *mut sys::httpd_req_t, to: &str) -> sys::esp_err_t {
    send(req, c"302 Found", c"text/plain", &[(c"Location", to)], b"")
}

/// `/setup/state`: no token, ever (the AP is open).
fn state() -> J {
    let (open, scanning, networks, nonce, claim_void) = {
        let s = SHARED.lock().unwrap();
        let networks: Vec<J> = s.networks.iter().map(|(n, r, o)| json!([n, r, o])).collect();
        (s.open, s.scanning, networks, s.nonce.clone(), s.claim.is_void())
    };
    let fresh = hub_state().fresh();
    json!({
        "ap": names().ap_ssid,
        "host": format!("{}.local", names().host),
        "need_token": open.is_some_and(|r| r.needs_token(fresh)),
        "nonce": nonce,
        "claim_void": claim_void,
        "scanning": scanning,
        "networks": networks,
        "warning": crate::esp::flash_warning(),
        "wifi": crate::net::info(),
    })
}

/// New credentials from the page; whether a fresh hub registered the
/// page's setup key commitment (the browser trades the key for the token
/// over the home network).
fn submit(body: &[u8]) -> Result<bool, (&'static CStr, &'static str)> {
    let reason = SHARED.lock().unwrap().open.ok_or((c"403 Forbidden", "setup is closed"))?;
    let v: J = serde_json::from_slice(body).map_err(|_| (c"400 Bad Request", "bad request"))?;
    let field = |k: &str| v.get(k).and_then(J::as_str).unwrap_or("");
    let (ssid, pass) = (field("ssid"), field("pass"));
    if ssid.is_empty() || ssid.len() > 32 || ssid.contains('\n') {
        return Err((c"400 Bad Request", "the network name needs 1 to 32 bytes"));
    }
    if pass.contains('\n') || (!pass.is_empty() && !(8..=64).contains(&pass.len())) {
        return Err((c"400 Bad Request", "a WPA password has 8 to 63 characters"));
    }
    let fresh = hub_state().fresh();
    if reason.needs_token(fresh) {
        // Single use, whatever the outcome.
        let nonce = core::mem::replace(&mut SHARED.lock().unwrap().nonce, new_nonce());
        let token = mode::get_str("api_token").unwrap_or_default();
        if token.is_empty() || nonce.is_empty() || field("nonce") != nonce || !setup::proof_ok(&token, &nonce, ssid, pass, field("proof")) {
            return Err((c"403 Forbidden", "wrong API token (or an old page: try again)"));
        }
    }
    crate::net::configure(ssid, pass).map_err(|e| {
        warn!("setup: {e}");
        (c"500 Internal Server Error", "could not store the credentials")
    })?;
    info!("setup: credentials for {ssid:?} from the setup page ({reason:?}, fresh {fresh})");
    let Some(commitment) = setup::unhex::<32>(field("commit")).filter(|_| fresh) else { return Ok(false) };
    let registered = SHARED.lock().unwrap().claim.issue(commitment, now_ms());
    if !registered {
        warn!("setup: a second, different setup key: the claim is void until BOOT or `wifi setup`");
    }
    Ok(registered)
}

/// `POST /setup/claim`: a fresh hub's setup key for the API token, once,
/// only from the home network. The token is marked handed out first.
fn claim(req: *mut sys::httpd_req_t, body: &[u8]) -> (&'static CStr, J) {
    let key = serde_json::from_slice::<J>(body).ok().and_then(|v| v.get("key").and_then(J::as_str).and_then(Claim::parse_key));
    let via_ap = via_ap(req);
    let redeemed = SHARED.lock().unwrap().claim.redeem(key, now_ms(), via_ap, mode::token_unclaimed());
    let error = match redeemed {
        Ok(()) => match (mode::get_str("api_token"), mode::token_handed_out()) {
            (Some(token), Ok(())) => {
                info!("setup: API token claimed with the setup key");
                return (c"200 OK", json!({"token": token}));
            }
            (_, Err(e)) => {
                warn!("setup: claim: {e}");
                "could not record the claim; try the console command token"
            }
            (None, Ok(())) => "no API token",
        },
        Err(ClaimError::ViaSetupAp) => "connect this device to the home network first: the token never goes over the setup network",
        Err(ClaimError::Claimed) => "the API token was handed out already (console command token)",
        Err(ClaimError::NoKey) => "no valid setup key (used, expired or the hub restarted): hold BOOT for 3 s and set up again",
        Err(ClaimError::Conflict) => {
            "two devices sent a setup key on the open setup network, so none works: hold BOOT for 3 s and set up again, or use the console command token"
        }
        Err(ClaimError::Wrong) => "wrong setup key",
    };
    (c"403 Forbidden", json!({"error": error}))
}

fn uri(req: *mut sys::httpd_req_t) -> String {
    unsafe { CStr::from_ptr((*req).uri.as_ptr()) }.to_string_lossy().into_owned()
}

/// GET: the setup page, its state, and the captive portal's catch-all; all
/// only through the open AP.
unsafe extern "C" fn get(req: *mut sys::httpd_req_t) -> sys::esp_err_t {
    let uri = uri(req);
    let on_ap = to_open_ap(req);
    if uri.starts_with("/setup/state") {
        if !on_ap {
            return reply(req, c"404 Not Found", &json!({"error": "setup is closed"}));
        }
        return reply(req, c"200 OK", &state());
    }
    if uri == "/setup" {
        if !on_ap {
            return redirect(req, "/");
        }
        let headers = [(c"Content-Encoding", "gzip"), (c"Cache-Control", "no-store")];
        return send(req, c"200 OK", c"text/html; charset=utf-8", &headers, hub_web::SETUP_HTML_GZ);
    }
    // Captive portal probes (generate_204, hotspot-detect.html, …) and any
    // other page asked through the AP.
    match ap_redirect(req) {
        Some(to) => redirect(req, &to),
        None => send(req, c"404 Not Found", c"text/plain", &[], b""),
    }
}

/// POST: a new scan and the credentials (through the open AP), the setup
/// code (from the home network).
unsafe extern "C" fn post(req: *mut sys::httpd_req_t) -> sys::esp_err_t {
    let uri = uri(req);
    let mut body = [0u8; 512];
    let want = unsafe { (*req).content_len }.min(body.len());
    let mut n = 0;
    while n < want {
        match unsafe { sys::httpd_req_recv(req, body[n..].as_mut_ptr().cast(), want - n) } {
            k if k > 0 => n += k as usize,
            _ => break,
        }
    }
    if uri == "/setup/claim" {
        let (status, body) = claim(req, &body[..n]);
        return reply(req, status, &body);
    }
    if !to_open_ap(req) {
        return reply(req, c"403 Forbidden", &json!({"error": "setup is closed"}));
    }
    match uri.as_str() {
        "/setup/scan" => {
            SHARED.lock().unwrap().scan = true;
            reply(req, c"200 OK", &json!({}))
        }
        "/setup/wifi" => match submit(&body[..n]) {
            Ok(claim) => reply(req, c"200 OK", &json!({"claim": claim})),
            Err((status, e)) => reply(req, status, &json!({"error": e})),
        },
        _ => reply(req, c"404 Not Found", &json!({"error": "not found"})),
    }
}

/// The setup page and its endpoints, and the captive portal's catch-all
/// redirect: registered after every other handler of the server.
pub fn register(server: &EspHttpServer<'static>) -> Result<(), EspError> {
    for (uri, method, handler) in [
        (c"/*", sys::http_method_HTTP_GET, get as unsafe extern "C" fn(*mut sys::httpd_req_t) -> sys::esp_err_t),
        (c"/setup/*", sys::http_method_HTTP_POST, post),
    ] {
        let uri = sys::httpd_uri_t { uri: uri.as_ptr(), method: method as _, handler: Some(handler), ..Default::default() };
        sys::esp!(unsafe { sys::httpd_register_uri_handler(server.handle(), &uri) })?;
    }
    Ok(())
}
