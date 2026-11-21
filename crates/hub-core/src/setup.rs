//! The setup access point's rules (README "First-time setup without a
//! console"): when the open AP is up, when the setup page needs the API
//! token, how it proves it has it, and the one-time setup key that hands a
//! fresh hub's token to its owner over the home network. The firmware feeds
//! [`Policy::tick`] about every 100 ms and opens or closes the AP to follow
//! it.
//!
//! The AP is open (no Wi-Fi password): anyone in range can use the page and
//! read its traffic. So:
//!
//! - Neither the API token nor anything that yields it crosses the AP.
//! - Moving the hub to another Wi-Fi network needs the API token, unless
//!   the hub has nothing to protect yet ([`HubState::fresh`]: its token was
//!   never handed out, no registry, no Zigbee network) or someone at the hub
//!   asked for the AP ([`Reason::Requested`]: the S3 BOOT button or the USB
//!   console). The page proves it has the token with [`token_proof`] (an
//!   HMAC over a single-use nonce and the credentials), not by sending it.
//! - A fresh hub's token goes to whoever set the hub up from the page, but
//!   only over the home network: the page keeps a random key and sends only
//!   its SHA-256 ([`Claim::commitment`]); the hub's web UI on the station
//!   address trades the key for the token, once ([`Claim`]).
//!
//! When the AP is open:
//!
//! - No Wi-Fi credentials: at once ([`Reason::Unconfigured`]). A fresh hub
//!   keeps it open until the station is up (a time limit would only force a
//!   power cycle); any other hub (credentials removed with `wifi clear`, a
//!   hub used over USB only) gets the time limit below, once per boot.
//! - Credentials, but the station has not been up since boot for
//!   [`FALLBACK_MS`]: once per boot ([`Reason::NoConnection`]).
//! - A request (S3 BOOT button held, console `wifi setup`):
//!   [`Reason::Requested`]; it does not shorten a fresh hub's open-ended
//!   opening.
//! - Time-limited openings close after [`OPEN_MS`] without an associated
//!   client (at the latest after [`MAX_OPEN_MS`]). Any reason closes
//!   [`GRACE_MS`] after the station came up while the AP was open, so the
//!   page can still show the result.

use sha2::{Digest, Sha256};

/// The station has not been up since boot for this long: open the AP.
pub const FALLBACK_MS: u64 = 5 * 60_000;
/// Time-limited AP: open for this long without clients.
pub const OPEN_MS: u64 = 15 * 60_000;
/// Time-limited AP: open at most this long, even with clients.
pub const MAX_OPEN_MS: u64 = 30 * 60_000;
/// The AP stays this long after the station came up.
pub const GRACE_MS: u64 = 2 * 60_000;
/// A setup key can be traded for the token for this long.
pub const CLAIM_MS: u64 = 30 * 60_000;
/// Wrong setup keys before the registered one is void.
pub const CLAIM_TRIES: u8 = 5;

/// What the hub has that an open AP must not give away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HubState {
    /// The API token exists and was never handed out: no console `token`,
    /// no rotation, no [`Claim`] yet. A token stored by a firmware that did
    /// not track this counts as handed out.
    pub token_unclaimed: bool,
    /// Devices in the registry (host store or application).
    pub devices: usize,
    /// A Zigbee network: stored by the hub or running on the NCP.
    pub network: bool,
}

impl HubState {
    /// Nothing to protect yet: the setup page needs no token and the hub's
    /// token goes to whoever sets it up ([`Claim`]).
    pub fn fresh(&self) -> bool {
        self.token_unclaimed && self.devices == 0 && !self.network
    }
}

/// Why the setup AP is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// No credentials stored.
    Unconfigured,
    /// Credentials, but no connection since boot.
    NoConnection,
    /// Button or console: someone is at the hub.
    Requested,
}

impl Reason {
    /// Whether new credentials from the setup page need the API token.
    pub fn needs_token(self, fresh: bool) -> bool {
        self != Reason::Requested && !fresh
    }

    /// Whether the AP closes after [`OPEN_MS`] / [`MAX_OPEN_MS`].
    pub fn time_limited(self, fresh: bool) -> bool {
        self != Reason::Unconfigured || !fresh
    }
}

/// What the firmware sees at a tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct Inputs {
    /// Milliseconds since boot.
    pub now_ms: u64,
    /// The station has credentials.
    pub configured: bool,
    /// The station is connected and has an address.
    pub sta_up: bool,
    /// Stations associated with the setup AP.
    pub clients: u8,
    /// Open the AP now (button, console).
    pub request: bool,
    /// [`HubState::fresh`].
    pub fresh: bool,
}

/// The open/close decisions.
#[derive(Debug, Default)]
pub struct Policy {
    open: Option<(Reason, u64)>,
    up_since: Option<u64>,
    ever_up: bool,
    /// The AP opened by itself this boot (no credentials, no connection).
    auto_used: bool,
}

impl Policy {
    /// Feed the current inputs; returns why the AP should be open, or
    /// `None` to have it closed.
    pub fn tick(&mut self, i: Inputs) -> Option<Reason> {
        let now = i.now_ms;
        if i.sta_up {
            self.up_since.get_or_insert(now);
            self.ever_up = true;
        } else {
            self.up_since = None;
        }
        let unlimited = matches!(self.open, Some((r, _)) if !r.time_limited(i.fresh));
        if i.request && !unlimited {
            self.open = Some((Reason::Requested, now));
        } else if self.open.is_none() && !self.auto_used {
            let reason = if !i.configured {
                Some(Reason::Unconfigured)
            } else if !self.ever_up && now >= FALLBACK_MS {
                Some(Reason::NoConnection)
            } else {
                None
            };
            if let Some(r) = reason {
                self.auto_used = true;
                self.open = Some((r, now));
            }
        }
        if let Some((reason, since)) = self.open {
            let open_for = now.saturating_sub(since);
            let close = match self.up_since {
                // The station came up while the AP was open.
                Some(up) if up > since => now - up >= GRACE_MS,
                _ => reason.time_limited(i.fresh) && open_for >= OPEN_MS && (i.clients == 0 || open_for >= MAX_OPEN_MS),
            };
            if close {
                self.open = None;
            }
        }
        self.open.map(|(r, _)| r)
    }
}

/// HMAC-SHA256 (RFC 2104) of the concatenated `parts`.
pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > k.len() {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    for p in parts {
        inner.update(p);
    }
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// The setup page's proof that it has the API token (`/setup/wifi`
/// `proof`): HMAC-SHA256(token, nonce "\n" ssid "\n" password). The nonce
/// comes from `/setup/state` and is replaced after every attempt, so a
/// proof read on the AP cannot be replayed, and the token itself never
/// crosses it.
pub fn token_proof(token: &str, nonce: &str, ssid: &str, pass: &str) -> [u8; 32] {
    hmac_sha256(token.as_bytes(), &[nonce.as_bytes(), b"\n", ssid.as_bytes(), b"\n", pass.as_bytes()])
}

/// Whether `proof_hex` is [`token_proof`] (constant time).
pub fn proof_ok(token: &str, nonce: &str, ssid: &str, pass: &str, proof_hex: &str) -> bool {
    match unhex::<32>(proof_hex) {
        Some(got) => same(&got, &token_proof(token, nonce, ssid, pass)),
        None => false,
    }
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `N` bytes from `2 * N` hex digits (either case).
pub fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.as_bytes();
    if s.len() != 2 * N {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = nibble(s[2 * i])? << 4 | nibble(s[2 * i + 1])?;
    }
    Some(out)
}

/// Why a setup key was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimError {
    /// The request came through the setup AP: the token goes only over the
    /// home network.
    ViaSetupAp,
    /// The token was handed out already.
    Claimed,
    /// No key registered, used, expired or void after [`CLAIM_TRIES`].
    NoKey,
    /// Two different keys were registered: neither works until someone at
    /// the hub resets the claim ([`Claim::reset`]).
    Conflict,
    /// Not the key.
    Wrong,
}

/// The one-time setup key of a fresh hub. The setup page draws a random
/// key, keeps it and sends only its SHA-256 ([`Claim::commitment`]) with
/// the credentials; the hub's web UI on the home network sends the key
/// itself (`#claim=` link or typed) and gets the token, once, within
/// [`CLAIM_MS`]. Someone who reads the AP sees the commitment, which does
/// not give the key (64 random bits behind SHA-256). Someone who also sends
/// credentials (the owner's, read on the AP) with a commitment of their own
/// makes the claim void instead of taking it over: a second, different
/// commitment voids both until the BOOT button or the console resets the
/// claim. On an open AP that leaves denial of service, never the token.
#[derive(Debug, Default)]
pub struct Claim {
    commitment: Option<([u8; 32], u64)>,
    void: bool,
    tries: u8,
}

impl Claim {
    /// Hex digits of a key (64 bits; shown as `xxxx-xxxx-xxxx-xxxx`).
    pub const KEY_HEX: usize = 16;

    /// No key registered.
    pub const fn new() -> Self {
        Self { commitment: None, void: false, tries: 0 }
    }

    /// SHA-256 of a key in its plain form (16 lowercase hex digits).
    pub fn commitment(key: &[u8; Self::KEY_HEX]) -> [u8; 32] {
        Sha256::digest(key).into()
    }

    /// A key as linked or typed: hex digits (either case), dashes and
    /// spaces ignored; its plain form.
    pub fn parse_key(s: &str) -> Option<[u8; Self::KEY_HEX]> {
        let mut out = [0u8; Self::KEY_HEX];
        let mut n = 0;
        for b in s.bytes() {
            match b {
                b'-' | b' ' => continue,
                b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F' if n < out.len() => {
                    out[n] = b.to_ascii_lowercase();
                    n += 1;
                }
                _ => return None,
            }
        }
        (n == out.len()).then_some(out)
    }

    /// Register a commitment; `false`: it conflicts with another live one
    /// (or an earlier conflict), and the claim is void.
    pub fn issue(&mut self, commitment: [u8; 32], now_ms: u64) -> bool {
        let live = self.commitment.filter(|&(_, at)| now_ms.saturating_sub(at) < CLAIM_MS);
        match live {
            _ if self.void => {}
            Some((c, _)) if !same(&c, &commitment) => {
                self.void = true;
                self.commitment = None;
            }
            // The same page again keeps its key and its tries.
            Some(_) => {}
            None => {
                self.commitment = Some((commitment, now_ms));
                self.tries = 0;
            }
        }
        !self.void
    }

    /// Someone at the hub (BOOT, console) asked for the AP: forget the
    /// commitment and a conflict.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Whether two different keys were registered.
    pub fn is_void(&self) -> bool {
        self.void
    }

    /// Trade `key` for the token: `Ok` once, then the commitment is gone.
    /// `via_ap`: the request came through the setup AP; `unclaimed`:
    /// [`HubState::token_unclaimed`] now.
    pub fn redeem(&mut self, key: Option<[u8; Self::KEY_HEX]>, now_ms: u64, via_ap: bool, unclaimed: bool) -> Result<(), ClaimError> {
        if via_ap {
            return Err(ClaimError::ViaSetupAp);
        }
        if !unclaimed {
            self.commitment = None;
            return Err(ClaimError::Claimed);
        }
        if self.void {
            return Err(ClaimError::Conflict);
        }
        let Some((want, issued)) = self.commitment else { return Err(ClaimError::NoKey) };
        if now_ms.saturating_sub(issued) >= CLAIM_MS {
            self.commitment = None;
            return Err(ClaimError::NoKey);
        }
        if !key.is_some_and(|k| same(&Self::commitment(&k), &want)) {
            self.tries += 1;
            if self.tries >= CLAIM_TRIES {
                self.commitment = None;
            }
            return Err(ClaimError::Wrong);
        }
        self.commitment = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    const S: u64 = 1000;

    fn at(now_ms: u64, configured: bool, sta_up: bool) -> Inputs {
        Inputs { now_ms, configured, sta_up, ..Default::default() }
    }

    fn fresh(now_ms: u64, configured: bool, sta_up: bool) -> Inputs {
        Inputs { fresh: true, ..at(now_ms, configured, sta_up) }
    }

    const FRESH: HubState = HubState { token_unclaimed: true, devices: 0, network: false };

    #[test]
    fn a_fresh_hub_needs_no_token_and_its_token_is_claimed_once_over_the_home_network() {
        assert!(FRESH.fresh());
        let mut p = Policy::default();
        assert_eq!(p.tick(fresh(0, false, false)), Some(Reason::Unconfigured));
        assert!(!Reason::Unconfigured.needs_token(FRESH.fresh()));
        assert_eq!(p.tick(fresh(MAX_OPEN_MS * 2, false, false)), Some(Reason::Unconfigured), "no time limit");
        // BOOT or `wifi setup` does not shorten it.
        assert_eq!(p.tick(Inputs { request: true, ..fresh(MAX_OPEN_MS * 2 + S, false, false) }), Some(Reason::Unconfigured));
        assert_eq!(p.tick(fresh(MAX_OPEN_MS * 4, false, false)), Some(Reason::Unconfigured));
        // Credentials entered on the page; the station comes up.
        let up = MAX_OPEN_MS * 2 + 20 * S;
        assert_eq!(p.tick(fresh(up, true, true)), Some(Reason::Unconfigured));
        assert_eq!(p.tick(fresh(up + GRACE_MS - S, true, true)), Some(Reason::Unconfigured));
        assert_eq!(p.tick(fresh(up + GRACE_MS, true, true)), None);
        assert_eq!(p.tick(fresh(up + GRACE_MS + S, true, true)), None, "stays closed");

        // The page sent the key's commitment; the web UI trades the key on
        // the station address.
        let key = *b"0123456789abcdef";
        let mut c = Claim::default();
        assert!(c.issue(Claim::commitment(&key), up - 10 * S));
        assert!(c.issue(Claim::commitment(&key), up - 5 * S), "the same page again");
        assert_eq!(c.redeem(Some(key), up, true, true), Err(ClaimError::ViaSetupAp), "never over the AP");
        assert_eq!(c.redeem(Claim::parse_key("0123-4567-89AB-CDEF"), up, false, true), Ok(()));
        assert_eq!(c.redeem(Some(key), up, false, true), Err(ClaimError::NoKey), "once");
        // Claimed: the hub is no longer fresh.
        assert!(!HubState { token_unclaimed: false, ..FRESH }.fresh());
    }

    #[test]
    fn a_hub_whose_token_was_handed_out_needs_it_and_its_ap_is_time_limited() {
        let state = HubState { token_unclaimed: false, ..FRESH };
        assert!(!state.fresh());
        let mut p = Policy::default();
        let i = |now| Inputs { fresh: state.fresh(), ..at(now, false, false) };
        assert_eq!(p.tick(i(0)), Some(Reason::Unconfigured));
        assert!(Reason::Unconfigured.needs_token(state.fresh()));
        assert!(Reason::NoConnection.needs_token(state.fresh()));
        assert_eq!(p.tick(i(OPEN_MS - S)), Some(Reason::Unconfigured));
        assert_eq!(p.tick(i(OPEN_MS)), None, "time limit");
        assert_eq!(p.tick(i(OPEN_MS * 4)), None, "once per boot");
        // Someone at the hub (BOOT, console) needs no token.
        assert_eq!(p.tick(Inputs { request: true, ..i(OPEN_MS * 5) }), Some(Reason::Requested));
        assert!(!Reason::Requested.needs_token(state.fresh()));
        // A proof, not the token, goes over the AP; it is bound to the
        // nonce and the credentials.
        let proof = token_proof("t0ken", "n1", "Home", "password");
        let hex: std::string::String = proof.iter().map(|b| std::format!("{b:02x}")).collect();
        assert!(proof_ok("t0ken", "n1", "Home", "password", &hex));
        assert!(proof_ok("t0ken", "n1", "Home", "password", &hex.to_uppercase()));
        assert!(!proof_ok("t0ken", "n2", "Home", "password", &hex), "replayed with the next nonce");
        assert!(!proof_ok("t0ken", "n1", "Evil", "password", &hex), "other credentials");
        assert!(!proof_ok("other", "n1", "Home", "password", &hex));
        assert!(!proof_ok("t0ken", "n1", "Home", "password", "t0ken"));
        // And a claim is refused even with a key.
        let key = *b"0000000000000001";
        let mut c = Claim::default();
        c.issue(Claim::commitment(&key), 0);
        assert_eq!(c.redeem(Some(key), S, false, state.token_unclaimed), Err(ClaimError::Claimed));
        assert_eq!(c.redeem(Some(key), S, false, true), Err(ClaimError::NoKey), "the commitment is gone");
    }

    #[test]
    fn after_wifi_clear_a_hub_with_a_registry_or_a_network_needs_the_token() {
        // The token was never shown (USB console unused), but the hub has a
        // registry or a network: not fresh.
        for state in [HubState { devices: 3, ..FRESH }, HubState { network: true, ..FRESH }] {
            assert!(!state.fresh(), "{state:?}");
            let mut p = Policy::default();
            let i = |now, clients| Inputs { fresh: state.fresh(), clients, ..at(now, false, false) };
            assert_eq!(p.tick(i(0, 0)), Some(Reason::Unconfigured));
            assert!(Reason::Unconfigured.needs_token(state.fresh()));
            assert_eq!(p.tick(i(OPEN_MS, 1)), Some(Reason::Unconfigured), "kept while a phone is associated");
            assert_eq!(p.tick(i(MAX_OPEN_MS, 1)), None, "but not forever");
            assert_eq!(p.tick(i(MAX_OPEN_MS * 2, 0)), None, "once per boot");
        }
        // A hub that becomes non-fresh while its AP is open gets the limit.
        let mut p = Policy::default();
        assert_eq!(p.tick(fresh(0, false, false)), Some(Reason::Unconfigured));
        assert_eq!(p.tick(at(OPEN_MS, false, false)), None);
    }

    #[test]
    fn no_connection_opens_once_after_the_fallback_time() {
        let mut p = Policy::default();
        assert_eq!(p.tick(at(FALLBACK_MS - S, true, false)), None);
        assert_eq!(p.tick(at(FALLBACK_MS, true, false)), Some(Reason::NoConnection));
        // Kept while a phone is associated, closed once it leaves.
        let busy = Inputs { clients: 1, ..at(FALLBACK_MS + OPEN_MS, true, false) };
        assert_eq!(p.tick(busy), Some(Reason::NoConnection));
        assert_eq!(p.tick(at(FALLBACK_MS + OPEN_MS + S, true, false)), None);
        assert_eq!(p.tick(at(FALLBACK_MS * 10, true, false)), None, "once per boot");
        // Also time-limited on a fresh hub (credentials exist).
        let mut p = Policy::default();
        assert_eq!(p.tick(fresh(FALLBACK_MS, true, false)), Some(Reason::NoConnection));
        assert!(!Reason::NoConnection.needs_token(true));
        assert_eq!(p.tick(fresh(FALLBACK_MS + OPEN_MS, true, false)), None);
    }

    #[test]
    fn a_client_cannot_hold_the_ap_open_forever() {
        let mut p = Policy::default();
        p.tick(at(FALLBACK_MS, true, false));
        let busy = Inputs { clients: 2, ..at(FALLBACK_MS + MAX_OPEN_MS, true, false) };
        assert_eq!(p.tick(busy), None);
    }

    #[test]
    fn no_fallback_once_the_station_was_up() {
        let mut p = Policy::default();
        assert_eq!(p.tick(at(10 * S, true, true)), None);
        assert_eq!(p.tick(at(FALLBACK_MS * 2, true, false)), None, "a later outage keeps the AP closed");
    }

    #[test]
    fn a_request_while_connected_is_time_limited_and_closes_after_a_reconnect() {
        let mut p = Policy::default();
        p.tick(at(10 * S, true, true));
        let t = 60 * S;
        assert_eq!(p.tick(Inputs { request: true, ..at(t, true, true) }), Some(Reason::Requested));
        assert_eq!(p.tick(at(t + GRACE_MS, true, true)), Some(Reason::Requested), "already up: no grace close");
        assert_eq!(p.tick(at(t + OPEN_MS, true, true)), None, "time limit");
        // Again, now with new credentials: the station drops and comes back.
        let t = t + OPEN_MS + S;
        p.tick(Inputs { request: true, ..at(t, true, true) });
        assert_eq!(p.tick(at(t + 5 * S, true, false)), Some(Reason::Requested));
        assert_eq!(p.tick(at(t + 10 * S, true, true)), Some(Reason::Requested));
        assert_eq!(p.tick(at(t + 10 * S + GRACE_MS, true, true)), None);
    }

    #[test]
    fn setup_keys_expire_and_wrong_guesses_void_them() {
        let (k, other) = (*b"00000000000000aa", *b"00000000000000ab");
        let mut c = Claim::default();
        assert_eq!(c.redeem(Some(k), 0, false, true), Err(ClaimError::NoKey), "none registered");
        c.issue(Claim::commitment(&k), 0);
        assert_eq!(c.redeem(Some(k), CLAIM_MS, false, true), Err(ClaimError::NoKey), "expired");
        c.issue(Claim::commitment(&k), 0);
        for _ in 0..CLAIM_TRIES - 1 {
            assert_eq!(c.redeem(Some(other), S, false, true), Err(ClaimError::Wrong));
        }
        assert_eq!(c.redeem(None, S, false, true), Err(ClaimError::Wrong));
        assert_eq!(c.redeem(Some(k), S, false, true), Err(ClaimError::NoKey), "void after the last try");
        // Requests through the AP count no try.
        c.issue(Claim::commitment(&k), 0);
        assert!(!c.is_void());
        for _ in 0..CLAIM_TRIES {
            assert_eq!(c.redeem(Some(other), S, true, true), Err(ClaimError::ViaSetupAp));
        }
        assert_eq!(c.redeem(Some(k), S, false, true), Ok(()));
    }

    #[test]
    fn a_second_key_voids_the_claim_until_a_reset() {
        // Someone read the owner's credentials on the AP and sends them
        // again with a key of their own: neither key works.
        let (owner, other) = (*b"1111111111111111", *b"2222222222222222");
        for (first, second) in [(owner, other), (other, owner)] {
            let mut c = Claim::default();
            assert!(c.issue(Claim::commitment(&first), 0));
            assert!(!c.issue(Claim::commitment(&second), S));
            assert!(!c.issue(Claim::commitment(&first), 2 * S), "stays void");
            assert!(c.is_void());
            for k in [owner, other] {
                assert_eq!(c.redeem(Some(k), 3 * S, false, true), Err(ClaimError::Conflict));
            }
            // BOOT or the console: a fresh start.
            c.reset();
            assert!(c.issue(Claim::commitment(&owner), 4 * S));
            assert_eq!(c.redeem(Some(owner), 5 * S, false, true), Ok(()));
        }
        // After the first one expired, a new key is no conflict.
        let mut c = Claim::default();
        c.issue(Claim::commitment(&owner), 0);
        assert!(c.issue(Claim::commitment(&other), CLAIM_MS));
        assert_eq!(c.redeem(Some(other), CLAIM_MS + S, false, true), Ok(()));
    }

    #[test]
    fn keys_parse_and_hmac_matches_rfc_4231() {
        assert_eq!(Claim::parse_key("ABCD-ef01 2345-6789"), Some(*b"abcdef0123456789"));
        for bad in ["", "abcdef012345678", "abcdef01234567890", "abcdef012345678g", "abcd_ef0123456789"] {
            assert_eq!(Claim::parse_key(bad), None, "{bad:?}");
        }
        assert_eq!(unhex::<2>("0aFf"), Some([0x0a, 0xff]));
        assert_eq!(unhex::<2>("0aF"), None);
        // RFC 4231 test cases 2 and 6 (a key longer than the block).
        let hex = |b: [u8; 32]| -> std::string::String { b.iter().map(|x| std::format!("{x:02x}")).collect() };
        assert_eq!(
            hex(hmac_sha256(b"Jefe", &[b"what do ya want ", b"for nothing?"])),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(hmac_sha256(&[0xaa; 131], &[b"Test Using Larger Than Block-Size Key - Hash Key First"])),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }
}
