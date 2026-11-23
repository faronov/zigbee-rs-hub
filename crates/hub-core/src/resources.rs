//! Buffer limits from the memory the module has, detected at boot by every
//! image. PSRAM changes buffering, not Zigbee device support or the flash
//! layout. Without PSRAM (a module without it, or an image built for the
//! other PSRAM interface) the hub keeps running on internal RAM with the
//! reduced limits and says so in its log and `hub/system`.

/// Limits of one boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resources {
    pub psram_bytes: usize,
    /// Open sockets of the API/web server.
    pub http_sockets: usize,
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
    fn snapshot_pieces_leave_room_in_the_client_budget() {
        for r in [Resources::for_psram(0), Resources::for_psram(1)] {
            assert!(2 * r.snapshot_chunk_bytes <= r.ws_client_bytes / 2);
            assert!(r.ws_client_bytes <= r.ws_total_bytes);
        }
    }
}
