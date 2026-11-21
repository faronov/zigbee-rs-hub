//! When the hub's setup access point is open (README "First-time setup
//! without a console"). The firmware feeds [`Policy::tick`] about every
//! 100 ms and opens or closes the AP to follow it.
//!
//! - No Wi-Fi credentials: the AP opens at once and stays open until the
//!   station is up ([`Reason::Unconfigured`]; there is nothing to protect
//!   yet, and a time limit would only force a power cycle).
//! - Credentials, but the station has not been up since boot for
//!   [`FALLBACK_MS`]: the AP opens once per boot ([`Reason::NoConnection`]);
//!   changing the credentials then needs the API token.
//! - A request (S3 BOOT button held, console `wifi setup`):
//!   [`Reason::Requested`], no token needed (physical or console access).
//! - [`Reason::NoConnection`] and [`Reason::Requested`] close after
//!   [`OPEN_MS`] without an associated client (at the latest after
//!   [`MAX_OPEN_MS`]). Any reason closes [`GRACE_MS`] after the station came
//!   up while the AP was open, so the setup page can still show the result.

/// The station has not been up since boot for this long: open the AP.
pub const FALLBACK_MS: u64 = 5 * 60_000;
/// Time-limited AP: open for this long without clients.
pub const OPEN_MS: u64 = 15 * 60_000;
/// Time-limited AP: open at most this long, even with clients.
pub const MAX_OPEN_MS: u64 = 30 * 60_000;
/// The AP stays this long after the station came up.
pub const GRACE_MS: u64 = 2 * 60_000;

/// Why the setup AP is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// No credentials stored.
    Unconfigured,
    /// Credentials, but no connection since boot.
    NoConnection,
    /// Button or console.
    Requested,
}

impl Reason {
    /// Whether new credentials from the setup page need the API token.
    pub fn needs_token(self) -> bool {
        self == Reason::NoConnection
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
}

/// The open/close decisions.
#[derive(Debug, Default)]
pub struct Policy {
    open: Option<(Reason, u64)>,
    up_since: Option<u64>,
    ever_up: bool,
    fallback_used: bool,
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
        if i.request {
            self.open = Some((Reason::Requested, now));
        } else if self.open.is_none() {
            if !i.configured {
                self.open = Some((Reason::Unconfigured, now));
            } else if !self.ever_up && !self.fallback_used && now >= FALLBACK_MS {
                self.fallback_used = true;
                self.open = Some((Reason::NoConnection, now));
            }
        }
        if let Some((reason, since)) = self.open {
            let open_for = now.saturating_sub(since);
            let close = match self.up_since {
                // The station came up while the AP was open.
                Some(up) if up > since => now - up >= GRACE_MS,
                _ => {
                    reason != Reason::Unconfigured
                        && open_for >= OPEN_MS
                        && (i.clients == 0 || open_for >= MAX_OPEN_MS)
                }
            };
            if close {
                self.open = None;
            }
        }
        self.open.map(|(r, _)| r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1000;

    fn at(now_ms: u64, configured: bool, sta_up: bool) -> Inputs {
        Inputs { now_ms, configured, sta_up, ..Default::default() }
    }

    #[test]
    fn unconfigured_opens_at_once_and_closes_only_after_the_station_is_up() {
        let mut p = Policy::default();
        assert_eq!(p.tick(at(0, false, false)), Some(Reason::Unconfigured));
        assert_eq!(p.tick(at(MAX_OPEN_MS * 2, false, false)), Some(Reason::Unconfigured), "no time limit");
        // Credentials entered on the page; the station comes up.
        let up = MAX_OPEN_MS * 2 + 20 * S;
        assert_eq!(p.tick(at(up, true, true)), Some(Reason::Unconfigured));
        assert_eq!(p.tick(at(up + GRACE_MS - S, true, true)), Some(Reason::Unconfigured));
        assert_eq!(p.tick(at(up + GRACE_MS, true, true)), None);
        assert_eq!(p.tick(at(up + GRACE_MS + S, true, true)), None, "stays closed");
    }

    #[test]
    fn no_connection_opens_once_after_the_fallback_time() {
        let mut p = Policy::default();
        assert_eq!(p.tick(at(FALLBACK_MS - S, true, false)), None);
        assert_eq!(p.tick(at(FALLBACK_MS, true, false)), Some(Reason::NoConnection));
        assert!(Reason::NoConnection.needs_token());
        // Kept while a phone is associated, closed once it leaves.
        let busy = Inputs { clients: 1, ..at(FALLBACK_MS + OPEN_MS, true, false) };
        assert_eq!(p.tick(busy), Some(Reason::NoConnection));
        assert_eq!(p.tick(at(FALLBACK_MS + OPEN_MS + S, true, false)), None);
        assert_eq!(p.tick(at(FALLBACK_MS * 10, true, false)), None, "once per boot");
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
        assert!(!Reason::Requested.needs_token());
        assert_eq!(p.tick(at(t + GRACE_MS, true, true)), Some(Reason::Requested), "already up: no grace close");
        assert_eq!(p.tick(at(t + OPEN_MS, true, true)), None, "time limit");
        // Again, now with new credentials: the station drops and comes back.
        let t = t + OPEN_MS + S;
        p.tick(Inputs { request: true, ..at(t, true, true) });
        assert_eq!(p.tick(at(t + 5 * S, true, false)), Some(Reason::Requested));
        assert_eq!(p.tick(at(t + 10 * S, true, true)), Some(Reason::Requested));
        assert_eq!(p.tick(at(t + 10 * S + GRACE_MS, true, true)), None);
    }
}
