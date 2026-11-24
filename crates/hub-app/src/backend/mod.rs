//! The coordinator backend the app runs on (`docs/coordinator-boundary.md`
//! §2, §6): its [`Coordinator`] and [`NetworkAdmin`], started on the app's
//! event queue, and the hub's own records in the backend's store with
//! whose they are ([`Records`]).
//!
//! | Backend | Feature | Records |
//! |---|---|---|
//! | ZRHP (`hub-ncp`) | `zrhp` | the host core's store; the backend decides ownership (`HostStore`) |
//! | EZSP (`hub-ezsp`) | `ezsp` | [`NcpNvRecords`]: the app decides ownership from what the NCP reports (`NcpNv`) |

use std::io;
use std::sync::Arc;

use hub_coord::{
    BackendConfig, CoordStatus, Coordinator, EventQueue, NetworkAdmin, NetworkOwnership,
    RadioControl, RegistryState,
};
use hub_store::KvStore;

#[cfg(feature = "ezsp")]
pub mod ezsp;
mod ncpnv;
#[cfg(feature = "zrhp")]
mod zrhp;

pub use ncpnv::NcpNvRecords;

/// The hub's records in the backend's store, and whose the network-scoped
/// ones are (design §2.5, §2.7). The store's gate follows
/// [`registry`](Self::registry): foreign records are neither visible nor
/// writable through [`KvStore`], unverified ones are read-only.
pub trait Records: KvStore {
    /// Whose the network-scoped records are (`network/info.registry`).
    fn registry(&self) -> RegistryState;

    /// `network/registry/adopt`: the records become this backend's, of the
    /// NCP's network.
    fn adopt(&self) -> io::Result<RegistryState>;

    /// `network/registry/clear`: every network-scoped record deleted, the
    /// rest adopted; how many were deleted.
    fn clear(&self) -> io::Result<usize>;

    /// The backend's [`NetworkOwnership`] report, `None` when its session
    /// ended. `HostStore` backends decided already; `NcpNv` records compare
    /// the NCP's network with the ownership tag here.
    fn observe(&self, ownership: Option<&NetworkOwnership>) {
        let _ = ownership;
    }

    /// Whether device traffic may go to the NCP: `NcpNv` records allow it
    /// only once the NCP's network of this session matches the tag.
    fn verified(&self) -> bool {
        true
    }

    /// `network/info.ownership` when the hub decides it (`NcpNv`: owned or
    /// mismatch instead of the backend's `observed`); `None`: as reported.
    fn ownership(&self, reported: &NetworkOwnership) -> Option<NetworkOwnership> {
        let _ = reported;
        None
    }

    /// The operator replaces the NCP's network (`network/reset`,
    /// `network/restore`, `network/form`; on a job thread): until
    /// [`replaced`](Self::replaced), what the NCP reports decides nothing.
    fn replacing(&self) {}

    /// The operator's network is about to be formed with `ext_pan_id`
    /// (`None`: the backend picks one), after [`replacing`](Self::replacing);
    /// returns the extended PAN ID to form with. `NcpNv` records pick one
    /// when none is given and tag the records for it now (its key ID not
    /// known yet), so that the network that comes up is theirs even when
    /// the job is interrupted (a timeout, a reboot). `HostStore` backends
    /// keep it themselves (ZRHP stores the network before `NWK_FORM`).
    fn forming(&self, ext_pan_id: Option<[u8; 8]>) -> Option<[u8; 8]> {
        ext_pan_id
    }

    /// The replacement is over; `now` is what the backend reports then
    /// (`None`: it failed). The NCP's new network becomes the records'.
    fn replaced(&self, now: Option<&NetworkOwnership>) {
        let _ = now;
    }

    /// The operator set the TX power (`network/set_tx_power`): backends
    /// that do not keep it themselves (EZSP) have it stored here.
    fn tx_power_set(&self, dbm: i8) {
        let _ = dbm;
    }

    /// `hub/system`'s `store` and `ncp` objects.
    fn system(&self, st: &CoordStatus) -> (serde_json::Value, serde_json::Value);
}

/// Starts a backend's coordinator: given the hub's endpoints and the app's
/// event queue, the running [`Coordinator`] and its [`NetworkAdmin`]
/// (EZSP makes both from one worker).
pub type StartCoordinator = Box<
    dyn FnOnce(BackendConfig, Arc<EventQueue>) -> (Box<dyn Coordinator>, Arc<dyn NetworkAdmin>)
        + Send,
>;

/// A coordinator backend for [`App::start_backend`](crate::App::start_backend).
pub struct Backend {
    /// Starts the coordinator and its administration on the app's event
    /// queue.
    pub coordinator: StartCoordinator,
    /// The hub's records and their ownership.
    pub kv: Arc<dyn Records>,
    /// Wi-Fi coexistence, for the firmware's Wi-Fi callbacks
    /// ([`App::radio`](crate::App::radio)); `None`: the NCP has none.
    pub radio: Option<Arc<dyn RadioControl>>,
}
