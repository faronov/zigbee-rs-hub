//! EZSP (`hub-ezsp`): an EmberZNet NCP keeps the network in its own NVM
//! (`NcpNv`); the hub's records are [`NcpNvRecords`] in the hub's own
//! store, and the app decides whose they are (`docs/ezsp-backend.md`).

use std::sync::Arc;

use hub_coord::BackendKind;
use hub_ezsp::coord::{EzspCoordinator, EzspOptions};
use hub_ezsp::host::{Clock, Transport};
use hub_store::Store;

use super::{Backend, NcpNvRecords};

impl Backend {
    /// The EZSP backend on `transport` (the NCP's UART) and `clock`, with
    /// the hub's records in `store`. The TX power the operator set in an
    /// earlier run (`ezsp/tx_power`) is applied unless `opts` sets one.
    ///
    /// # Panics
    ///
    /// When the app starts it, for an invalid host configuration in `opts`.
    pub fn ezsp<T, C>(transport: T, clock: C, mut opts: EzspOptions, store: Store) -> Backend
    where
        T: Transport + 'static,
        C: Clock + 'static,
    {
        let records = Arc::new(NcpNvRecords::open(BackendKind::Ezsp, store));
        if opts.tx_power.is_none() {
            opts.tx_power = records.tx_power();
        }
        // Nothing queued goes out to a network the records have not
        // vouched for (the worker closes it when a session ends or starts).
        opts.traffic = Some(records.traffic());
        Backend {
            coordinator: Box::new(move |cfg, events| {
                let (coord, admin) =
                    EzspCoordinator::start(transport, clock, opts, cfg, events).expect("EZSP host configuration");
                (Box::new(coord), Arc::new(admin))
            }),
            kv: records,
            radio: None,
        }
    }
}
