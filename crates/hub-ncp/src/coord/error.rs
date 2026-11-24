//! Host core errors as [`CoordError`]s. Each one displays exactly as the
//! [`Error`] it comes from, so that messages built from it stay as they
//! were; the classification lets callers choose their API codes.

use hub_coord::{BackendKind, Code, CoordError, Kind};
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
}
