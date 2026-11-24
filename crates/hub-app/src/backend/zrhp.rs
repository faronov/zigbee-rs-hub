//! ZRHP (`hub-ncp`): the host core's store holds the hub's records and the
//! backend decides their ownership (`HostStore`: migration, upkeep, the
//! gate; design §2.7, PR I).

use std::io;
use std::sync::Arc;

use hub_coord::{CoordStatus, NetworkAdmin, RegistryState};
use hub_ncp::Hub;
use hub_ncp::coord::{ZrhpAdmin, ZrhpCoordinator, ZrhpRadio};
use serde_json::{Value as J, json};

use super::{Backend, Records};

impl Records for Hub {
    fn registry(&self) -> RegistryState {
        Hub::registry(self)
    }

    fn adopt(&self) -> io::Result<RegistryState> {
        self.registry_adopt()
    }

    fn clear(&self) -> io::Result<usize> {
        self.registry_clear()
    }

    fn system(&self, _: &CoordStatus) -> (J, J) {
        let st = self.status();
        let store = json!({
            "volatile": st.store_volatile,
            "journal_sectors": st.journal.sectors,
            "journal_live_sectors": st.journal.live_sectors,
            "journal_generation": st.journal.generation,
        });
        let ncp = json!({
            "fw": st.ncp.as_ref().map(|s| s.negotiated.fw.clone()),
            "resumes": st.resumes,
            "restarts": st.ncp_restarts,
            "last_error": st.last_error,
            "slots": st.slots.iter().map(|s| json!({"slot": s.slot, "state": s.state, "version": format!("0x{:08X}", s.version)})).collect::<Vec<_>>(),
        });
        (store, ncp)
    }
}

impl Backend {
    /// The ZRHP backend on a running host core: the `zrhp-pump`
    /// coordinator, [`ZrhpAdmin`], the host store's records and PTA
    /// coexistence.
    pub fn zrhp(hub: Hub) -> Backend {
        let h = hub.clone();
        Backend {
            coordinator: Box::new(move |cfg, events| {
                let admin: Arc<dyn NetworkAdmin> = Arc::new(ZrhpAdmin::new(h.clone()));
                (Box::new(ZrhpCoordinator::start(h, cfg, events)), admin)
            }),
            kv: Arc::new(hub.clone()),
            radio: Some(Arc::new(ZrhpRadio(hub))),
        }
    }
}
