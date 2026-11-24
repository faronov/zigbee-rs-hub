//! EZSP protocol versions and the version policy.
//!
//! Mapping of public SDK releases to `EZSP_PROTOCOL_VERSION`
//! (`protocol/zigbee/app/util/ezsp/ezsp-protocol.h` of each tag):
//!
//! | SDK tag | EmberZNet | EZSP |
//! |---|---|---|
//! | Gecko SDK 4.3.0 | 7.3 | 12 (refused) |
//! | Gecko SDK 4.4.0 – 4.4.6, 4.5.0 | 7.4.0 – 7.5.1 | 13 |
//! | Simplicity SDK 2024.6.0 – 2024.6.2 | 8.0.0 – 8.0.2 | 14 |
//! | Simplicity SDK 2024.6.3, 2024.12.0 – 2024.12.3 | 8.0.3, 8.1.x | 16 |
//! | Simplicity SDK 2025.6.0 – 2025.6.1 | 8.2.0 – 8.2.1 | 17 |
//! | Simplicity SDK 2025.6.2 – 2025.6.3 | 8.2.2 – 8.2.3 | 18 |
//!
//! No public tag ships EZSP 15. The frame layouts used by this crate were
//! compared in the generated `command-functions.h` and
//! `ezsp-frame-utilities.c` of Gecko SDK 4.4.6 (13) and Simplicity SDK
//! 2024.6.0 (14), 2024.12.0 (16), 2025.6.0 (17) and 2025.6.3 (18): they
//! change between 13 and 14 and are identical from 14 to 18.

/// Oldest accepted version: EmberZNet 7.4 (task baseline: MG24 is only
/// supported from EmberZNet 7.x, and EZSP 13 is the first version this
/// crate implements).
pub const MIN_VERSION: u8 = 13;
/// Newest version whose layouts were checked.
pub const MAX_VERIFIED_VERSION: u8 = 18;
/// Versions whose layouts were checked against the SDK sources.
pub const VERIFIED_VERSIONS: &[u8] = &[13, 14, 16, 17, 18];
/// Stack type reported by the version command for EmberZNet PRO (UG100
/// successor §2 "EZSP Protocol Version": stack type 2, mesh).
pub const STACK_TYPE_MESH: u8 = 2;

/// A negotiated EZSP protocol version and the layout switches it implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EzspVersion(u8);

impl EzspVersion {
    /// Wraps a version number that passed a [`VersionPolicy`].
    pub fn new(v: u8) -> EzspVersion {
        EzspVersion(v)
    }

    pub fn get(self) -> u8 {
        self.0
    }

    /// EZSP ≥ 14 returns `sl_status_t` (u32) where 13 returned
    /// `EzspStatus`/`EmberStatus` (u8).
    pub fn uses_sl_status(self) -> bool {
        self.0 >= 14
    }

    /// EZSP ≥ 14 uses a 16-bit message tag; 13 uses 8 bits.
    pub fn wide_message_tag(self) -> bool {
        self.0 >= 14
    }

    /// EZSP ≥ 14 sendBroadcast/sendMulticast carry alias and NWK sequence
    /// fields, and incomingMessageHandler carries `sl_zigbee_rx_packet_info_t`.
    pub fn alias_fields(self) -> bool {
        self.0 >= 14
    }

    pub fn is_verified(self) -> bool {
        VERIFIED_VERSIONS.contains(&self.0)
    }
}

impl std::fmt::Display for EzspVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EZSP {}", self.0)
    }
}

/// `stackVersion` of the version response, one nibble per field
/// (`SL_ZIGBEE_FULL_VERSION`: major << 12 | minor << 8 | patch << 4 |
/// special, Simplicity SDK `config.h`).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct StackVersion(pub u16);

impl StackVersion {
    pub fn major(self) -> u8 {
        (self.0 >> 12) as u8
    }
    pub fn minor(self) -> u8 {
        ((self.0 >> 8) & 0xF) as u8
    }
    pub fn patch(self) -> u8 {
        ((self.0 >> 4) & 0xF) as u8
    }
    pub fn special(self) -> u8 {
        (self.0 & 0xF) as u8
    }
}

impl std::fmt::Debug for StackVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StackVersion({self})")
    }
}

impl std::fmt::Display for StackVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{}.{}.{}",
            self.major(),
            self.minor(),
            self.patch(),
            self.special()
        )
    }
}

/// Which NCP versions the host accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VersionPolicy {
    /// Also accept versions ≥ 13 that are not in [`VERIFIED_VERSIONS`],
    /// using the EZSP ≥ 14 layout. Off by default.
    pub accept_unverified: bool,
}

/// The NCP's EZSP version was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionError {
    /// Older than [`MIN_VERSION`].
    TooOld { ncp: u8 },
    /// Not in [`VERIFIED_VERSIONS`] and the policy does not accept it.
    Unverified { ncp: u8 },
    /// The NCP reports a stack type other than [`STACK_TYPE_MESH`].
    StackType { stack_type: u8 },
    /// The second version exchange did not confirm the version.
    NotConfirmed { requested: u8, ncp: u8 },
}

impl std::fmt::Display for VersionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VersionError::TooOld { ncp } => write!(
                f,
                "NCP speaks EZSP {ncp}, at least {MIN_VERSION} is required"
            ),
            VersionError::Unverified { ncp } => {
                write!(
                    f,
                    "NCP speaks EZSP {ncp}, not among the verified versions {VERIFIED_VERSIONS:?}"
                )
            }
            VersionError::StackType { stack_type } => {
                write!(f, "NCP stack type {stack_type}, expected {STACK_TYPE_MESH}")
            }
            VersionError::NotConfirmed { requested, ncp } => {
                write!(f, "requested EZSP {requested}, NCP answered {ncp}")
            }
        }
    }
}

impl std::error::Error for VersionError {}

impl VersionPolicy {
    /// Accepts or refuses the version an NCP reported.
    pub fn check(self, ncp: u8) -> Result<EzspVersion, VersionError> {
        if ncp < MIN_VERSION {
            return Err(VersionError::TooOld { ncp });
        }
        let v = EzspVersion(ncp);
        if !v.is_verified() && !self.accept_unverified {
            return Err(VersionError::Unverified { ncp });
        }
        Ok(v)
    }
}
