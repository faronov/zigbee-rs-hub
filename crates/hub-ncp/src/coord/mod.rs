//! The ZRHP coordinator backend: the host core behind the backend-neutral
//! interfaces of `hub-coord` (`docs/coordinator-boundary.md` §4).
//!
//! - [`ZrhpAdmin`]: [`NetworkAdmin`](hub_coord::NetworkAdmin) with the same
//!   [`Hub`](crate::Hub) calls and call options as before;
//! - [`coord_error`]: host core errors as [`CoordError`](hub_coord::CoordError)s,
//!   worded as before;
//! - backups as ZRHP record streams ⇄ [`BackupItem`](hub_coord::BackupItem)s
//!   ([`RecordEncoder`], [`export_items`]);
//! - [`scan_progress`]: the NCP's optional scan progress event.

mod admin;
mod backup;
mod error;

pub use admin::ZrhpAdmin;
pub use backup::{RecordEncoder, export_items};
pub use error::coord_error;

use hub_coord::CoordEvent;

use crate::driver::Message;

/// Message ID of the optional `EVT_SCAN_PROGRESS` (NETWORK event, proposed:
/// `u16 req_tid, u8 scan (0 energy, 1 active), u8 channel, u8 done, u8
/// total, u8 networks_found`).
pub const EVT_SCAN_PROGRESS: u16 = 0x018B;

/// The [`CoordEvent::ScanProgress`] of an `EVT_SCAN_PROGRESS`.
pub fn scan_progress(m: &Message) -> Option<CoordEvent> {
    if m.msg_id != EVT_SCAN_PROGRESS {
        return None;
    }
    let [_, _, scan, _, done, total, ..] = m.payload[..] else {
        return None;
    };
    Some(CoordEvent::ScanProgress { scan, done, total })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zigbee_zrhp::msg::Kind;

    #[test]
    fn scan_progress_events() {
        let m = |msg_id, payload: &[u8]| Message {
            kind: Kind::Event,
            msg_id,
            tid: 0,
            payload: payload.to_vec(),
        };
        // tid, scan 0, channel 18, 8 of 16 channels, 0 networks.
        assert_eq!(
            scan_progress(&m(EVT_SCAN_PROGRESS, &[1, 0, 0, 18, 8, 16, 0])),
            Some(CoordEvent::ScanProgress {
                scan: 0,
                done: 8,
                total: 16
            })
        );
        assert_eq!(
            scan_progress(&m(EVT_SCAN_PROGRESS, &[1, 0, 1, 18, 8, 16])),
            Some(CoordEvent::ScanProgress {
                scan: 1,
                done: 8,
                total: 16
            })
        );
        assert_eq!(
            scan_progress(&m(EVT_SCAN_PROGRESS, &[1, 0, 0, 18, 8])),
            None,
            "truncated"
        );
        assert_eq!(
            scan_progress(&m(EVT_SCAN_PROGRESS + 1, &[1, 0, 0, 18, 8, 16])),
            None
        );
    }
}
