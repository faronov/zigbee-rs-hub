//! [`EzspAdmin`]: [`NetworkAdmin`] for the admin job threads. Each call is
//! a job for the worker, which owns the NCP link; the caller waits for its
//! answer. Jobs go before queued requests (they are not part of the
//! request order). While a job runs, the worker keeps reading the NCP and
//! passing its events on, but sends no queued request: scans, forming,
//! leaving and channel changes own the radio. A ZDO request only waits
//! for its answer, without holding the queue. A restore also resets the
//! NCP (a new session) where the stack must read its tokens again.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use hub_coord::{
    BackupItem, BackupSink, Beacon, ChannelChange, CoordError, CoordStatus, Diagnostics,
    EnergyChannel, FormParams, NetworkAdmin, NetworkParams, RestoreBlob, RestoreEncoder,
    RestoreOptions, RestoreReport, TablesRead,
};

use super::backup::{BlobEncoder, Plan};
use super::worker::{Msg, Shared};

/// What an admin job does.
#[derive(Debug, Clone)]
pub(crate) enum Op {
    Form(FormParams),
    LeaveReset,
    EnergyScan {
        mask: u32,
        exp: u8,
    },
    ActiveScan {
        mask: u32,
        exp: u8,
    },
    ChangeChannel(u8),
    SetTxPower(i8),
    Tables {
        routes: bool,
    },
    Zdo {
        nwk: u16,
        cluster: u16,
        zdp: Vec<u8>,
        t: Duration,
    },
    Diagnostics {
        reset: bool,
    },
    Backup {
        keys: bool,
    },
    RestoreCheck(Box<Plan>, RestoreOptions),
    Restore(Box<Plan>, RestoreOptions),
}

/// The answer to an [`Op`].
#[derive(Debug)]
pub(crate) enum Answer {
    Form(Result<NetworkParams, CoordError>),
    Unit(Result<(), CoordError>),
    Energy(Result<Vec<EnergyChannel>, CoordError>),
    Beacons(Result<Vec<Beacon>, CoordError>),
    Channel(Result<ChannelChange, CoordError>),
    TxPower(Result<i8, CoordError>),
    Tables(TablesRead),
    Zdo(Result<Vec<u8>, CoordError>),
    Diagnostics(Diagnostics),
    Backup(Result<Vec<BackupItem>, CoordError>),
    Restore(Result<RestoreReport, CoordError>),
}

/// Where the worker puts an [`Answer`].
#[derive(Default)]
pub(crate) struct Slot {
    answer: Mutex<Option<Answer>>,
    ready: Condvar,
}

impl Slot {
    pub(crate) fn answer(&self, a: Answer) {
        *self.answer.lock().unwrap() = Some(a);
        self.ready.notify_all();
    }

    /// The answer, or `None` once the worker has stopped without one.
    fn wait(&self, shared: &Shared) -> Option<Answer> {
        let mut a = self.answer.lock().unwrap();
        loop {
            if let Some(a) = a.take() {
                return Some(a);
            }
            if shared.stopped.load(Ordering::SeqCst) {
                return None;
            }
            a = self
                .ready
                .wait_timeout(a, Duration::from_millis(200))
                .unwrap()
                .0;
        }
    }
}

/// A job and where its answer goes.
pub(crate) struct Job {
    pub(crate) op: Op,
    pub(crate) slot: Arc<Slot>,
}

impl Job {
    /// The answer of a job that cannot run.
    pub(crate) fn fail(self, e: CoordError) {
        let a = match self.op {
            Op::Form(_) => Answer::Form(Err(e)),
            Op::LeaveReset | Op::RestoreCheck(..) => Answer::Unit(Err(e)),
            Op::Backup { .. } => Answer::Backup(Err(e)),
            Op::Restore(..) => Answer::Restore(Err(e)),
            Op::EnergyScan { .. } => Answer::Energy(Err(e)),
            Op::ActiveScan { .. } => Answer::Beacons(Err(e)),
            Op::ChangeChannel(_) => Answer::Channel(Err(e)),
            Op::SetTxPower(_) => Answer::TxPower(Err(e)),
            Op::Tables { routes } => Answer::Tables(TablesRead {
                neighbors: Err(e.clone()),
                children: Err(e.clone()),
                routes: routes.then_some(Err(e)),
            }),
            Op::Zdo { .. } => Answer::Zdo(Err(e)),
            Op::Diagnostics { .. } => Answer::Diagnostics(Diagnostics {
                errors: vec![e.to_string()],
                ..Diagnostics::default()
            }),
        };
        self.slot.answer(a);
    }
}

/// Network administration of an EZSP NCP (design §2.6), backup and
/// restore included (`docs/ezsp-backend.md` "Backup and restore"). NCP
/// firmware update is not offered yet (follow-up: Gecko bootloader).
#[derive(Clone)]
pub struct EzspAdmin {
    pub(crate) shared: Arc<Shared>,
}

impl EzspAdmin {
    fn run(&self, op: Op) -> Option<Answer> {
        let slot = Arc::new(Slot::default());
        let job = Job {
            op,
            slot: slot.clone(),
        };
        if let Err(Msg::Admin(job)) = self.shared.inbox.send(Msg::Admin(job)) {
            job.fail(CoordError::LinkDown);
        }
        slot.wait(&self.shared)
    }
}

fn stopped<T>() -> Result<T, CoordError> {
    Err(CoordError::described(
        CoordError::LinkDown,
        "EZSP backend stopped",
    ))
}

impl NetworkAdmin for EzspAdmin {
    fn status(&self) -> Option<CoordStatus> {
        Some(self.shared.status.lock().unwrap().clone())
    }

    fn form(&self, p: FormParams) -> Result<NetworkParams, CoordError> {
        match self.run(Op::Form(p)) {
            Some(Answer::Form(r)) => r,
            _ => stopped(),
        }
    }

    fn leave_reset(&self) -> Result<(), CoordError> {
        match self.run(Op::LeaveReset) {
            Some(Answer::Unit(r)) => r,
            _ => stopped(),
        }
    }

    fn energy_scan(&self, mask: u32, exp: u8) -> Result<Vec<EnergyChannel>, CoordError> {
        match self.run(Op::EnergyScan { mask, exp }) {
            Some(Answer::Energy(r)) => r,
            _ => stopped(),
        }
    }

    fn active_scan(&self, mask: u32, exp: u8) -> Result<Vec<Beacon>, CoordError> {
        match self.run(Op::ActiveScan { mask, exp }) {
            Some(Answer::Beacons(r)) => r,
            _ => stopped(),
        }
    }

    fn change_channel(&self, ch: u8) -> Result<ChannelChange, CoordError> {
        match self.run(Op::ChangeChannel(ch)) {
            Some(Answer::Channel(r)) => r,
            _ => stopped(),
        }
    }

    fn set_tx_power(&self, dbm: i8) -> Result<i8, CoordError> {
        match self.run(Op::SetTxPower(dbm)) {
            Some(Answer::TxPower(r)) => r,
            _ => stopped(),
        }
    }

    fn tx_power_setting(&self) -> Option<i8> {
        *self.shared.tx_power.lock().unwrap()
    }

    fn coordinator_tables(&self, routes: bool) -> TablesRead {
        match self.run(Op::Tables { routes }) {
            Some(Answer::Tables(t)) => t,
            _ => TablesRead {
                neighbors: stopped(),
                children: stopped(),
                routes: routes.then(stopped),
            },
        }
    }

    fn zdo_request(
        &self,
        nwk: u16,
        cluster: u16,
        zdp: &[u8],
        t: Duration,
    ) -> Result<Vec<u8>, CoordError> {
        if zdp.is_empty() {
            return Err(CoordError::Other(
                "no ZDP transaction sequence number".into(),
            ));
        }
        let op = Op::Zdo {
            nwk,
            cluster,
            zdp: zdp.to_vec(),
            t,
        };
        match self.run(op) {
            Some(Answer::Zdo(r)) => r,
            _ => stopped(),
        }
    }

    fn diagnostics(&self, _groups: u8, reset: bool) -> Diagnostics {
        match self.run(Op::Diagnostics { reset }) {
            Some(Answer::Diagnostics(d)) => d,
            _ => Diagnostics {
                errors: vec!["EZSP backend stopped".into()],
                ..Diagnostics::default()
            },
        }
    }

    fn backup_export(&self, keys: bool, sink: &mut dyn BackupSink) -> Result<(), CoordError> {
        match self.run(Op::Backup { keys }) {
            Some(Answer::Backup(Ok(items))) => items.iter().try_for_each(|i| sink.item(i)),
            Some(Answer::Backup(Err(e))) => Err(e),
            _ => stopped(),
        }
    }

    fn restore_encoder(&self) -> Result<Box<dyn RestoreEncoder>, CoordError> {
        Ok(Box::<BlobEncoder>::default())
    }

    fn restore_check(&self, blob: &RestoreBlob, o: RestoreOptions) -> Result<(), CoordError> {
        let plan = Plan::decode(&blob.0)?;
        match self.run(Op::RestoreCheck(Box::new(plan), o)) {
            Some(Answer::Unit(r)) => r,
            _ => stopped(),
        }
    }

    fn backup_restore(
        &self,
        blob: RestoreBlob,
        o: RestoreOptions,
    ) -> Result<RestoreReport, CoordError> {
        let plan = Plan::decode(&blob.0)?;
        drop(blob);
        match self.run(Op::Restore(Box::new(plan), o)) {
            Some(Answer::Restore(r)) => r,
            _ => stopped(),
        }
    }
}
