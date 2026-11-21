//! Buffer limits from the memory the module has, detected at boot by every
//! image. PSRAM changes buffering, not Zigbee device support or the flash
//! layout. Without PSRAM (a module without it, or an image built for the
//! other PSRAM interface) the firmware does not start the application yet
//! (it needs about 100 KB more internal RAM than is left); the reduced
//! limits are for that future internal-RAM mode.
//!
//! Sockets are one budget: [`Resources::sockets`] of every mode fits
//! [`LWIP_MAX_SOCKETS`], which is `CONFIG_LWIP_MAX_SOCKETS` of
//! `firmware/hub/sdkconfig.defaults` (compile-time checks here and in the
//! firmware, a test against the file). With PSRAM:
//!
//! | Sockets | |
//! |---|---|
//! | 7 | API/web server clients ([`Resources::http_sockets`]; the setup page shares them while the setup AP is open) |
//! | 3 | esp_http_server itself ([`HTTPD_INTERNAL_SOCKETS`]: listen, control send and receive) |
//! | 2 | the setup AP's DNS server and the mDNS responder ([`Resources::setup_sockets`], UDP) |
//! | 1 | the MQTT bridge's connection ([`Resources::mqtt_sockets`]) |
//! | **13** | `CONFIG_LWIP_MAX_SOCKETS` |
//!
//! DHCP (client and the AP's server), SNTP and the DNS resolver use lwIP's
//! raw API, not sockets. The wrong-image mode (no app) has its own small
//! server: [`WRONG_IMAGE_HTTP_SOCKETS`] + 3 + mDNS.

/// Sockets esp_http_server opens itself: the listening socket and its
/// control socket pair (`httpd_start` refuses more than
/// `CONFIG_LWIP_MAX_SOCKETS` − 3 client sockets).
pub const HTTPD_INTERNAL_SOCKETS: usize = 3;

/// `CONFIG_LWIP_MAX_SOCKETS` of `firmware/hub/sdkconfig.defaults`.
pub const LWIP_MAX_SOCKETS: usize = 13;

/// Client sockets of the wrong-image page's server (no app, no setup AP).
pub const WRONG_IMAGE_HTTP_SOCKETS: usize = 2;

/// Sockets of the wrong-image mode: its server and the mDNS responder.
pub const WRONG_IMAGE_SOCKETS: usize = WRONG_IMAGE_HTTP_SOCKETS + HTTPD_INTERNAL_SOCKETS + 1;

// Every mode fits the budget; the PSRAM limits use all of it.
const _: () = assert!(Resources::for_psram(1).sockets() == LWIP_MAX_SOCKETS);
const _: () = assert!(Resources::for_psram(0).sockets() <= LWIP_MAX_SOCKETS);
const _: () = assert!(WRONG_IMAGE_SOCKETS <= LWIP_MAX_SOCKETS);

/// Limits of one boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resources {
    pub psram_bytes: usize,
    /// Open client sockets of the API/web server (also the setup page's
    /// while the setup AP is open).
    pub http_sockets: usize,
    /// UDP sockets of the setup AP's DNS server (while the AP is open) and
    /// the mDNS responder (while the station is up).
    pub setup_sockets: usize,
    /// The MQTT bridge's TCP connection.
    pub mqtt_sockets: usize,
    /// Whether the web server may close the least recently used socket for
    /// a new connection. Off without PSRAM: an authenticated client (Home
    /// Assistant) keeps its socket and a new connection is refused.
    pub http_lru_purge: bool,
    /// Bytes queued to one WebSocket client before it is closed (a single
    /// message to a client with nothing queued always goes).
    pub ws_client_bytes: usize,
    /// Bytes queued to all WebSocket clients together.
    pub ws_total_bytes: usize,
    /// Size of the pieces a subscription snapshot is sent in.
    pub snapshot_chunk_bytes: usize,
    /// The decoded built-in definition examples (~90 KB); a flash-backed
    /// catalog and generic device support remain available without them.
    pub builtin_catalog: bool,
    pub mqtt_queue_bytes: usize,
    pub mqtt_devices_bytes: usize,
    /// Optional services start only with enough free internal heap.
    pub heap_guard: bool,
}

impl Resources {
    pub const fn for_psram(psram_bytes: usize) -> Self {
        if psram_bytes == 0 {
            Self {
                psram_bytes,
                // One browser, one integration and a short HTTP request.
                http_sockets: 3,
                setup_sockets: 2,
                mqtt_sockets: 1,
                http_lru_purge: false,
                ws_client_bytes: 16 * 1024,
                ws_total_bytes: 32 * 1024,
                snapshot_chunk_bytes: 4 * 1024,
                builtin_catalog: false,
                mqtt_queue_bytes: 12 * 1024,
                mqtt_devices_bytes: 16 * 1024,
                heap_guard: true,
            }
        } else {
            Self {
                psram_bytes,
                http_sockets: 7,
                setup_sockets: 2,
                mqtt_sockets: 1,
                http_lru_purge: true,
                ws_client_bytes: 128 * 1024,
                ws_total_bytes: 384 * 1024,
                snapshot_chunk_bytes: 8 * 1024,
                builtin_catalog: true,
                mqtt_queue_bytes: 64 * 1024,
                mqtt_devices_bytes: 256 * 1024,
                heap_guard: false,
            }
        }
    }

    /// lwIP sockets this boot may open at once.
    pub const fn sockets(self) -> usize {
        self.http_sockets + HTTPD_INTERNAL_SOCKETS + self.setup_sockets + self.mqtt_sockets
    }

    pub const fn internal_fallback(self) -> bool {
        self.psram_bytes == 0
    }

    /// `hub/system` `memory_mode`.
    pub const fn memory_mode(self) -> &'static str {
        if self.internal_fallback() { "internal-fallback" } else { "psram" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_psram_reduces_buffering_and_keeps_sessions() {
        let fallback = Resources::for_psram(0);
        let normal = Resources::for_psram(2 * 1024 * 1024);
        assert!(fallback.internal_fallback());
        assert_eq!(fallback.memory_mode(), "internal-fallback");
        assert!(!fallback.builtin_catalog);
        assert!(!fallback.http_lru_purge && normal.http_lru_purge);
        assert!(fallback.heap_guard && !normal.heap_guard);
        assert!(fallback.http_sockets >= 3 && fallback.http_sockets < normal.http_sockets);
        assert!(fallback.ws_client_bytes < normal.ws_client_bytes);
        assert!(fallback.mqtt_queue_bytes < normal.mqtt_queue_bytes);
    }

    #[test]
    fn every_psram_size_gets_the_same_limits() {
        // 2 MB Quad (N4R2/N8R2), 8 MB Octal (N4R8/N8R8), 16 MB.
        let two = Resources::for_psram(2 * 1024 * 1024);
        for mb in [4, 8, 16] {
            let r = Resources::for_psram(mb * 1024 * 1024);
            assert_eq!(r.memory_mode(), "psram");
            assert_eq!(Resources { psram_bytes: two.psram_bytes, ..r }, two);
        }
    }

    #[test]
    fn the_sdkconfig_has_the_socket_budget() {
        let sdkconfig = include_str!("../../../firmware/hub/sdkconfig.defaults");
        let line = sdkconfig.lines().find(|l| l.starts_with("CONFIG_LWIP_MAX_SOCKETS=")).expect("CONFIG_LWIP_MAX_SOCKETS set");
        assert_eq!(line.split('=').nth(1).and_then(|v| v.trim().parse().ok()), Some(LWIP_MAX_SOCKETS));
        let octal = include_str!("../../../firmware/hub/sdkconfig.octal");
        assert!(!octal.contains("CONFIG_LWIP_MAX_SOCKETS"), "one value for both PSRAM variants");
    }

    #[test]
    fn snapshot_pieces_leave_room_in_the_client_budget() {
        for r in [Resources::for_psram(0), Resources::for_psram(1)] {
            assert!(2 * r.snapshot_chunk_bytes <= r.ws_client_bytes / 2);
            assert!(r.ws_client_bytes <= r.ws_total_bytes);
        }
    }
}
