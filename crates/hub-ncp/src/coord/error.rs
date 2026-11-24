//! Host core errors as [`CoordError`]s. Each one displays exactly as the
//! [`Error`] it comes from, so that messages built from it stay as they
//! were; the classification lets callers choose their API codes. Also the
//! engine's wording of host core errors and the NCP's `zb_status` texts.

use hub_coord::{BackendKind, Code, CoordError, Kind, Terminal};
use zigbee_zrhp::types::Status;

use crate::driver::Error;

/// `Display` of `Error::Status(s)` for the named statuses.
fn status_text(s: Status) -> Option<&'static str> {
    Some(match s {
        Status::Success => "NCP status Success",
        Status::Failure => "NCP status Failure",
        Status::Busy => "NCP status Busy",
        Status::InvalidParameter => "NCP status InvalidParameter",
        Status::UnsupportedCommand => "NCP status UnsupportedCommand",
        Status::InvalidState => "NCP status InvalidState",
        Status::NotFound => "NCP status NotFound",
        Status::NoSpace => "NCP status NoSpace",
        Status::Timeout => "NCP status Timeout",
        Status::LinkReset => "NCP status LinkReset",
        Status::AccessDenied => "NCP status AccessDenied",
        Status::NotFormed => "NCP status NotFormed",
        Status::AlreadyExists => "NCP status AlreadyExists",
        Status::StorageError => "NCP status StorageError",
        Status::SecurityError => "NCP status SecurityError",
        Status::VersionMismatch => "NCP status VersionMismatch",
        Status::InProgress => "NCP status InProgress",
        Status::TooLarge => "NCP status TooLarge",
        Status::WrongEndpoint => "NCP status WrongEndpoint",
        Status::Conflict => "NCP status Conflict",
        Status::Aborted => "NCP status Aborted",
        Status::LinkRejected => "NCP status LinkRejected",
        Status::Other(_) => return None,
    })
}

/// What an NCP status means. Only the statuses the maintenance API has
/// always told apart get a specific kind; `NotFormed`, `AlreadyExists`
/// and the rest stay [`Kind::Other`].
fn status_kind(s: Status) -> Kind {
    match s {
        Status::InvalidParameter => Kind::InvalidParameter,
        Status::InvalidState => Kind::InvalidState,
        Status::NotFound => Kind::NotFound,
        Status::AccessDenied => Kind::Denied,
        Status::NoSpace => Kind::TableFull,
        Status::TooLarge => Kind::TooLarge,
        Status::SecurityError => Kind::Security,
        _ => Kind::Other,
    }
}

/// The [`CoordError`] of a host core error; it displays as `e`.
pub fn coord_error(e: Error) -> CoordError {
    match e {
        Error::Status(Status::UnsupportedCommand) => {
            CoordError::described(CoordError::Unsupported, e.to_string())
        }
        Error::Status(Status::Busy | Status::InProgress) => {
            CoordError::described(CoordError::Busy, e.to_string())
        }
        Error::Status(s) => match status_text(s) {
            Some(text) => CoordError::Backend {
                code: Code {
                    backend: BackendKind::Zrhp,
                    value: u16::from(s.to_u8()),
                    text: Some(text),
                },
                kind: status_kind(s),
            },
            None => CoordError::Other(e.to_string().into()),
        },
        Error::Timeout => CoordError::Timeout,
        Error::LinkReset => CoordError::described(CoordError::DeliveryUnknown, e.to_string()),
        Error::Closed => CoordError::described(CoordError::LinkDown, e.to_string()),
        Error::Rejected(_) | Error::Codec => CoordError::Other(e.to_string().into()),
        Error::Other(s) => CoordError::Other(s.into()),
    }
}

/// A host core error as the engine has always shown it ("ZRHP status
/// 0x..", "NCP link down"), classified as [`coord_error`] does.
pub fn engine_error(e: Error) -> CoordError {
    match e {
        Error::Status(Status::Busy) => CoordError::Busy,
        Error::Status(s) => {
            CoordError::described(coord_error(e), format!("ZRHP status 0x{:02X}", s.to_u8()))
        }
        e => CoordError::described(coord_error(e), "NCP link down"),
    }
}

/// The outcome of a control request the host core answered with `r`.
pub fn control_outcome(r: Result<(), Error>) -> Terminal {
    match r {
        Ok(()) => Terminal::Done(hub_coord::Reply::None),
        Err(e @ Error::Status(_)) => Terminal::Refused(engine_error(e)),
        Err(e) => Terminal::Unknown(engine_error(e)),
    }
}

/// What an `EVT_APS_DATA_CONFIRM.zb_status` means (host protocol 7.3, the
/// APSDE-DATA.confirm status of R22 Table 2-27 plus NWK route failures
/// passed through). `legacy`: zigbee-rs NCP images up to 0.1.1, whose APS
/// numbering lacked INVALID_GROUP: 0xA6 was NO_ACK on some paths and 0xA8
/// a route failure (0xA7 stayed NO_ACK after the ACK retries).
pub fn zb_status_text(zb_status: u8, legacy: bool) -> Option<&'static str> {
    Some(match (zb_status, legacy) {
        (0xA6 | 0xA7, true) | (0xA7, false) => "no APS ACK",
        (0xA8, true) | (0xD0, _) => "no route to the device",
        (0xA9, true) | (0xAE, false) => "NCP table full",
        (0xA0, _) => "payload too long",
        (0xA8, false) => "no bound device",
        (0xA9, false) => "no short address for the IEEE address",
        (0xAD, _) => "APS security failed (no link key)",
        (0xD1, _) => "route error (the next hop did not take the frame)",
        _ => return None,
    })
}

/// The outcome of an `EVT_APS_DATA_CONFIRM` with a failed `zb_status`,
/// named in the NCP's numbering ([`zb_status_text`]).
pub fn zrhp_not_delivered(zb_status: u8, legacy: bool) -> Terminal {
    Terminal::NotDelivered(CoordError::NoAck(Code {
        backend: BackendKind::Zrhp,
        value: u16::from(zb_status),
        text: zb_status_text(zb_status, legacy),
    }))
}

/// `true` for a zigbee-rs NCP firmware version up to 0.1.1 (the version is
/// the last word of `fw`, e.g. `0.1.1` or `sim-ncp 0.1.0`).
pub fn legacy_zb_status(fw: &str) -> bool {
    let v: Vec<u32> = fw
        .rsplit(' ')
        .next()
        .unwrap_or("")
        .split(['.', '-', '+'])
        .take(3)
        .map_while(|x| x.parse().ok())
        .collect();
    v.len() == 3 && (v[0], v[1], v[2]) <= (0, 1, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_errors() -> Vec<Error> {
        let mut all: Vec<Error> = (0..=0xFF)
            .map(|b| Error::Status(Status::from_u8(b)))
            .collect();
        all.extend([
            Error::LinkReset,
            Error::Rejected(3),
            Error::Timeout,
            Error::Codec,
            Error::Closed,
            Error::Other("formation failed: state 1 zb_status 0xC2".into()),
        ]);
        all
    }

    #[test]
    fn errors_display_as_before() {
        for e in all_errors() {
            assert_eq!(coord_error(e.clone()).to_string(), e.to_string(), "{e:?}");
        }
    }

    #[test]
    fn errors_are_classified() {
        let meaning = |e| coord_error(e).meaning().clone();
        assert_eq!(
            meaning(Error::Status(Status::UnsupportedCommand)),
            CoordError::Unsupported
        );
        assert_eq!(meaning(Error::Status(Status::Busy)), CoordError::Busy);
        assert_eq!(meaning(Error::Status(Status::InProgress)), CoordError::Busy);
        assert_eq!(meaning(Error::Timeout), CoordError::Timeout);
        assert_eq!(meaning(Error::LinkReset), CoordError::DeliveryUnknown);
        assert_eq!(meaning(Error::Closed), CoordError::LinkDown);
        assert!(matches!(meaning(Error::Codec), CoordError::Other(_)));
        assert!(matches!(
            meaning(Error::Status(Status::AccessDenied)),
            CoordError::Backend {
                code: Code {
                    backend: BackendKind::Zrhp,
                    value: 0x0A,
                    ..
                },
                kind: Kind::Denied
            }
        ));
        assert!(matches!(
            meaning(Error::Status(Status::NotFormed)),
            CoordError::Backend {
                kind: Kind::Other,
                ..
            }
        ));
        assert!(matches!(
            meaning(Error::Status(Status::Other(0x80))),
            CoordError::Other(_)
        ));
    }

    #[test]
    fn zb_status_texts_spec_and_legacy() {
        assert_eq!(zb_status_text(0xA7, false), Some("no APS ACK"));
        assert_eq!(zb_status_text(0xD0, false), Some("no route to the device"));
        assert_eq!(
            zb_status_text(0xD1, false).map(|t| t.starts_with("route error")),
            Some(true)
        );
        assert_eq!(zb_status_text(0xA8, false), Some("no bound device"));
        assert_eq!(
            zb_status_text(0xA9, false).map(|t| t.starts_with("no short")),
            Some(true)
        );
        assert_eq!(zb_status_text(0xAE, false), Some("NCP table full"));
        // zigbee-rs NCP <= 0.1.1 numbering.
        assert_eq!(zb_status_text(0xA6, true), Some("no APS ACK"));
        assert_eq!(zb_status_text(0xA7, true), Some("no APS ACK"));
        assert_eq!(zb_status_text(0xA8, true), Some("no route to the device"));
        assert_eq!(zb_status_text(0xA9, true), Some("NCP table full"));
        assert_eq!(zb_status_text(0xA6, false), None);
        assert_eq!(zb_status_text(0x42, false), None);
    }

    #[test]
    fn legacy_numbering_by_ncp_version() {
        for (fw, legacy) in [
            ("0.1.1", true),
            ("0.1.0", true),
            ("sim-ncp 0.1.0", true),
            ("0.1.2", false),
            ("0.2.0-rc.1", false),
            ("1.0.0", false),
            ("unknown", false),
        ] {
            assert_eq!(legacy_zb_status(fw), legacy, "{fw}");
        }
    }
}
