//! Backup and restore jobs (design §2.6, `docs/ezsp-backend.md` "Backup
//! and restore"): the network, its keys and frame counters, the child
//! table and the key table read from the NCP; a backup restored the way
//! bellows (zigpy) does on EmberZNet. No key is logged here: responses
//! that carry one are never formatted into an error or a log line.

use hub_coord::{
    BackupChild, BackupDevice, BackupItem, BackupLinkKey, RestoreOptions, RestoreReport,
    restore_note,
};

use super::*;
use crate::coord::backup::Plan;

/// TX power of a restored network when neither the operator nor the
/// backup names one (bellows' `DEFAULT_TX_POWER`).
const RESTORE_TX_POWER: i8 = 8;
/// An erased EUI64 token.
const ERASED: [u8; 8] = [0xFF; 8];

/// Where a restore goes on after an NCP reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    /// The IEEE was written: check it, then the network.
    Ieee { note: Option<u8> },
    /// The network, without a reset before.
    Network,
    /// The network was formed: it must come back. `used`: the NCP's NWK
    /// frame counter before the reset.
    Verify {
        update_id_written: bool,
        used: Option<u32>,
    },
    /// The restored network came back: one more reset, so that the NCP's
    /// frame counters go past those it used since the restore wrote them
    /// ([`PERSIST_STEP`]); it must come back again.
    Advance { used: Option<u32> },
}

/// The first frame counters of a restore ([`start_counters`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Counters {
    nwk_start: u32,
    aps_start: u32,
    /// The NCP's own counters were higher than the backup's.
    raised: bool,
    /// The NCP stored a network and did not report its counters.
    unknown: bool,
}

/// The NCP's own outgoing frame counters ([`Worker::read_live`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Live {
    pub(super) nwk: Option<u32>,
    pub(super) aps: Option<u32>,
}

/// The margin over a counter the NCP stored but did not report, as for a
/// backup without an APS counter: ~150 days of a busy coordinator's frames.
const UNKNOWN_MARGIN: u32 = 1 << 20;

/// EmberZNet keeps the NWK frame counter in its NVM truncated to 0x1000;
/// each initialization (`networkInit`) resumes from the stored value and
/// stores it plus 0x1000 for the next one (Silicon Labs, host-NCP to RCP
/// migration notes). `setValue` stores the value truncated and
/// `formNetwork` does not advance it, so the first initialization after a
/// restore resumes at the start again, below the counters the network
/// used before its reset (lab unit, EmberZNet 7.4.5: NWK 96 058 set,
/// 94 208 after the restore's reset). A restore therefore starts on the
/// boundary (stored as written) and resets the NCP twice: the second
/// initialization resumes 0x1000 above it. The APS counter alike
/// (UNVERIFIED: 7.4.5 does not report it).
const PERSIST_STEP: u32 = 0x1000;

/// The first frame counters of a restore: the backup's (`nwk`, `aps`) plus
/// the margins, from the NCP's own (`live`: `None` when it stores no
/// network) where those are higher, whatever network they belong to (a
/// higher counter is always safe; an older backup of the running network
/// would otherwise go back below what its devices have seen, and they
/// drop such frames). A counter of a stored network that the NCP does not
/// report gets a margin of at least 2²⁰ instead. Rounded up to
/// [`PERSIST_STEP`].
pub(super) fn start_counters(
    (nwk, aps): (u32, u32),
    live: Option<Live>,
    o: RestoreOptions,
) -> Result<Counters, CoordError> {
    let pick = |backup: u32, own: Option<Option<u32>>, margin: u32| match own {
        Some(Some(c)) => (backup.max(c), margin, c > backup, false),
        Some(None) => (backup, margin.max(UNKNOWN_MARGIN), false, true),
        None => (backup, margin, false, false),
    };
    let (n, n_margin, n_raised, n_unknown) = pick(nwk, live.map(|l| l.nwk), o.nwk_margin);
    let (a, a_margin, a_raised, a_unknown) = pick(aps, live.map(|l| l.aps), o.aps_margin);
    let up = |v: u32, m: u32| v.checked_add(m)?.checked_next_multiple_of(PERSIST_STEP);
    let Some(nwk_start) = up(n, n_margin) else {
        return Err(refused(
            "the network key's frame counter plus the margin passes 2^32: the network needs a new network key first",
        ));
    };
    let Some(aps_start) = up(a, a_margin) else {
        return Err(refused("the APS frame counter plus the margin passes 2^32"));
    };
    Ok(Counters {
        nwk_start,
        aps_start,
        raised: n_raised || a_raised,
        unknown: n_unknown || a_unknown,
    })
}

/// A restore in progress across NCP resets.
pub(super) struct Restoring {
    plan: Box<Plan>,
    report: RestoreReport,
    step: Step,
    slot: Arc<Slot>,
}

/// How the NCP takes the backup's coordinator IEEE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IeeePlan {
    /// The NCP has it already.
    Same,
    /// The NCP keeps its own (`allow_ieee_change` off).
    KeepOwn,
    /// The rewritable restored-EUI64 stack token, by this token key.
    Rewrite(u32),
    /// The custom EUI64 manufacturing token, erased now: written once.
    WriteOnce,
    /// The custom EUI64 token holds it already; a reset applies it.
    Reapply,
}

/// A refusal of the request in the NCP's current state.
fn refused(why: impl Into<std::borrow::Cow<'static, str>>) -> CoordError {
    CoordError::described(
        CoordError::Backend {
            code: Code {
                backend: hub_coord::BackendKind::Ezsp,
                value: u16::from(EmberStatus::INVALID_CALL.0),
                text: None,
            },
            kind: Kind::InvalidState,
        },
        why,
    )
}

/// A response of another type to a key frame: its content is not shown.
fn unexpected(frame_id: u16) -> HostError {
    HostError::Decode {
        frame_id,
        error: crate::ezsp::DecodeError::Invalid("response type"),
    }
}

fn ieee_hex(e: &[u8; 8]) -> String {
    e.iter().rev().map(|b| format!("{b:02X}")).collect()
}

impl<T: Transport, C: Clock> Worker<T, C> {
    /// A command whose non-success status is an error.
    fn expect_ok(&mut self, cmd: &Command) -> Result<(), HostError> {
        let frame_id = cmd.frame_id();
        let r = self.host.call(cmd)?;
        match r.status() {
            Some(s) if s.is_success() => Ok(()),
            Some(status) => Err(HostError::Status { frame_id, status }),
            None if r.has_secret() => Err(unexpected(frame_id)),
            None => Err(HostError::UnexpectedResponse(r)),
        }
    }

    /// An exported key (`None`: the security manager does not export it).
    fn export_key(&mut self, context: SecManContext) -> Result<Option<KeyData>, HostError> {
        Ok(match self.try_call(&Command::ExportKey(context))? {
            Some(Response::ExportKey { status, key, .. }) if status.is_success() => Some(key),
            _ => None,
        })
    }

    /// A 32-bit value (`getValue`), if the NCP has it.
    fn value_u32(&mut self, id: ValueId) -> Result<Option<u32>, HostError> {
        Ok(match self.try_call(&Command::GetValue { id })? {
            Some(Response::GetValue { status, value }) if status.is_success() => {
                <[u8; 4]>::try_from(value.as_slice())
                    .ok()
                    .map(u32::from_le_bytes)
            }
            _ => None,
        })
    }

    // ------------------------------------------------------------ export

    /// The NCP's network as backup items: the coordinator, the network,
    /// with `keys` the network key (`exportKey`, `getNetworkKeyInfo`: its
    /// sequence and frame counter), the trust center link key and the key
    /// table; the child table always. With hashed link keys (the hub's
    /// own networks, bellows' and Z2M's) the trust center link key is
    /// "ZigBeeAlliance09" and the root key is the EZSP hashed TCLK seed, as
    /// bellows exports it. Without a readable APS frame counter there is
    /// no TCLK item: a restore then adds the large APS margin.
    pub(super) fn backup_items(&mut self, keys: bool) -> Result<Vec<BackupItem>, CoordError> {
        if self.net != NetState::Up {
            return Err(Self::not_up());
        }
        let r = self.read_backup(keys);
        r.map_err(|e| self.admin_error(e))
    }

    fn read_backup(&mut self, keys: bool) -> Result<Vec<BackupItem>, HostError> {
        let (_, p) = self.host.get_network_parameters()?;
        let security_level = match self.host.get_configuration_value(ConfigId::SECURITY_LEVEL) {
            Ok(v) => v as u8,
            Err(e) if is_link_error(&e) => return Err(e),
            Err(_) => 5,
        };
        let ieee = self.eui64.0;
        let channels = p.channels & 0x07FF_F800;
        let mut items = vec![
            BackupItem::Node {
                ieee,
                ieee_override: false,
            },
            BackupItem::Network {
                pan_id: p.pan_id,
                ext_pan_id: p.extended_pan_id,
                channel: p.radio_channel,
                channel_mask: if channels & (1 << p.radio_channel) != 0 {
                    channels
                } else {
                    1 << p.radio_channel
                },
                nwk_update_id: p.nwk_update_id,
                nwk_manager: p.nwk_manager_id,
                security_level,
                tx_power: self.tx_power.or(Some(p.radio_tx_power)),
            },
        ];
        if keys {
            self.read_keys(ieee, &mut items)?;
        }
        let mut devices: Vec<BackupDevice> = self
            .children()?
            .into_iter()
            .map(|c| BackupDevice {
                ieee: c.ieee,
                nwk: Some(c.nwk),
                is_child: true,
                child: Some(BackupChild {
                    capability: c.capability,
                    timeout_exp: c.timeout_exp,
                }),
                link: None,
            })
            .collect();
        if keys {
            self.read_key_table(ieee, &mut devices)?;
        }
        items.extend(devices.into_iter().map(BackupItem::Device));
        Ok(items)
    }

    fn read_keys(&mut self, ieee: [u8; 8], items: &mut Vec<BackupItem>) -> Result<(), HostError> {
        let info = match self.try_call(&Command::GetNetworkKeyInfo)? {
            Some(Response::GetNetworkKeyInfo { status, info })
                if status.is_success() && info.network_key_set =>
            {
                info
            }
            _ => {
                warn!("ezsp: backup without keys: no network key info");
                return Ok(());
            }
        };
        let Some(key) = self.export_key(SecManContext::network_key())? else {
            warn!("ezsp: backup without keys: the network key is not exportable");
            return Ok(());
        };
        items.push(BackupItem::NetworkKey {
            key,
            seq: info.network_key_sequence_number,
            tx_counter: info.network_key_frame_counter,
            active: true,
        });
        let hashed = match self.try_call(&Command::GetCurrentSecurityState)? {
            Some(Response::GetCurrentSecurityState { status, state }) if status.is_success() => {
                state.hashed_link_keys()
            }
            // The hub's own networks hash their link keys.
            _ => true,
        };
        let tclk = self.export_key(SecManContext::tc_link_key())?;
        let aps = self.value_u32(ValueId::APS_FRAME_COUNTER)?;
        if hashed && let Some(root) = tclk {
            items.push(BackupItem::HashedTclkSeed(root));
        }
        match (aps, hashed, tclk) {
            (Some(tx_counter), true, _) => items.push(BackupItem::TcLinkKey {
                key: ZIGBEE_ALLIANCE_09,
                tc_ieee: ieee,
                tx_counter,
                hashed: true,
            }),
            (Some(tx_counter), false, Some(key)) => items.push(BackupItem::TcLinkKey {
                key,
                tc_ieee: ieee,
                tx_counter,
                hashed: false,
            }),
            _ => warn!("ezsp: no APS frame counter: the backup has no trust center link key"),
        }
        Ok(())
    }

    /// Key table entries (`exportLinkKeyByIndex`) as the devices' link
    /// keys.
    fn read_key_table(
        &mut self,
        ieee: [u8; 8],
        devices: &mut Vec<BackupDevice>,
    ) -> Result<(), HostError> {
        let size = self.cfg(ConfigId::KEY_TABLE_SIZE).unwrap_or(0).min(255) as u8;
        for index in 0..size {
            let Some(Response::ExportLinkKeyByIndex {
                status,
                eui64,
                key,
                metadata,
                ..
            }) = self.try_call(&Command::ExportLinkKeyByIndex { index })?
            else {
                continue;
            };
            if !status.is_success() || eui64.0 == [0; 8] || eui64.0 == [0xFF; 8] || eui64.0 == ieee
            {
                continue;
            }
            let link = BackupLinkKey {
                key,
                key_seq: 0,
                attrs: if metadata.bitmask & key_bitmask::IS_AUTHORIZED != 0 {
                    2
                } else {
                    1
                },
                // A key the trust center holds for one device.
                origin: 2,
                tx_counter: metadata.outgoing_frame_counter,
                rx_floor: metadata.incoming_frame_counter,
            };
            if let Some(d) = devices.iter_mut().find(|d| d.ieee == eui64.0) {
                d.link = Some(link);
                continue;
            }
            let nwk = self.nwk_of(eui64)?;
            devices.push(BackupDevice {
                ieee: eui64.0,
                nwk,
                is_child: false,
                child: None,
                link: Some(link),
            });
        }
        Ok(())
    }

    /// The short address of `eui64`: learned, or the NCP's address table.
    fn nwk_of(&mut self, eui64: Eui64) -> Result<Option<u16>, HostError> {
        let ieee = eui64.to_u64();
        if let Some((_, n)) = self.nwk_cache.iter().find(|(i, _)| *i == ieee) {
            return Ok(Some(*n));
        }
        Ok(
            match self.try_call(&Command::LookupNodeIdByEui64 { eui64 })? {
                Some(Response::LookupNodeIdByEui64 { status, node_id })
                    if status.is_none_or(|s| s.is_success()) && node_id < 0xFFF8 =>
                {
                    Some(node_id)
                }
                _ => None,
            },
        )
    }

    // ------------------------------------------------------------ restore

    /// Checks a restore without changing anything: the frame counters
    /// with their margins fit, and the NCP can take the backup's
    /// coordinator IEEE (bellows' rules): the rewritable restored-EUI64
    /// token if the NCP has the token interface, else the custom EUI64
    /// manufacturing token, written once only, with
    /// [`RestoreOptions::ieee_write_once`], and refused when it already
    /// holds another IEEE.
    pub(super) fn restore_check(
        &mut self,
        plan: &Plan,
        o: RestoreOptions,
    ) -> Result<IeeePlan, CoordError> {
        self.restore_plan(plan, o).map(|(ieee, _)| ieee)
    }

    /// [`Self::restore_check`], with the frame counters the restore
    /// starts from ([`Self::restore_counters`]).
    fn restore_plan(
        &mut self,
        plan: &Plan,
        o: RestoreOptions,
    ) -> Result<(IeeePlan, Counters), CoordError> {
        let counters = self.restore_counters(plan, o)?;
        let r = self.ieee_plan(plan.ieee, o);
        match r {
            Ok(r) => r.map(|ieee| (ieee, counters)),
            Err(e) => Err(self.admin_error(e)),
        }
    }

    /// The first NWK and APS frame counters of a restore
    /// ([`start_counters`]), with the NCP's own: of the network it stores,
    /// or of the one it left for the restore ([`Self::leave_reset`]). After
    /// a hub restart between that leave and the restore the worker has no
    /// such counters: the ones the NCP still reports without a network
    /// (EmberZNet keeps them in its tokens across `leaveNetwork`) are
    /// floors where they are not 0, and an unreported one is no floor.
    fn restore_counters(&mut self, plan: &Plan, o: RestoreOptions) -> Result<Counters, CoordError> {
        let live = if self.net != NetState::None {
            let r = self.read_live();
            Some(r.map_err(|e| self.admin_error(e))?)
        } else if self.left.is_some() {
            self.left
        } else {
            let r = self.read_live().map_err(|e| self.admin_error(e))?;
            Some(Live {
                nwk: Some(r.nwk.unwrap_or(0)),
                aps: Some(r.aps.unwrap_or(0)),
            })
        };
        let backup = (plan.nwk_counter, plan.tc_link.map_or(0, |t| t.tx_counter));
        start_counters(backup, live, o)
    }

    /// The outgoing NWK and APS frame counters of the network the NCP
    /// stores, each `None` where it does not answer. A 0 of a network that
    /// is not up counts as no answer: UNVERIFIED whether EmberZNet reports
    /// them before `networkInit` brought the network up.
    pub(super) fn read_live(&mut self) -> Result<Live, HostError> {
        let up = self.net == NetState::Up;
        let known = |v: Option<u32>| v.filter(|&c| up || c != 0);
        let info = match self.try_call(&Command::GetNetworkKeyInfo)? {
            Some(Response::GetNetworkKeyInfo { status, info })
                if status.is_success() && info.network_key_set =>
            {
                Some(info.network_key_frame_counter)
            }
            _ => None,
        };
        let nwk = match known(info) {
            Some(n) => Some(n),
            None => known(self.value_u32(ValueId::NWK_FRAME_COUNTER)?),
        };
        let aps = known(self.value_u32(ValueId::APS_FRAME_COUNTER)?);
        Ok(Live { nwk, aps })
    }

    fn ieee_plan(
        &mut self,
        target: [u8; 8],
        o: RestoreOptions,
    ) -> Result<Result<IeeePlan, CoordError>, HostError> {
        let current = self.eui64.0;
        if target == current {
            return Ok(Ok(IeeePlan::Same));
        }
        // No usable IEEE in the backup (zigpy's `EUI64.UNKNOWN` is all ones).
        if !o.allow_ieee_change || target == ERASED || target == [0; 8] {
            return Ok(Ok(IeeePlan::KeepOwn));
        }
        if let Some(key) = self.restored_eui64_token()? {
            return Ok(Ok(IeeePlan::Rewrite(key)));
        }
        let custom = match self.try_call(&Command::GetMfgToken {
            token: MfgTokenId::CUSTOM_EUI_64,
        })? {
            Some(Response::GetMfgToken { data }) => <[u8; 8]>::try_from(data.as_slice()).ok(),
            _ => None,
        };
        let (want, have) = (ieee_hex(&target), ieee_hex(&current));
        Ok(match custom {
            Some(c) if c == ERASED && o.ieee_write_once => Ok(IeeePlan::WriteOnce),
            Some(c) if c == ERASED => Err(refused(format!(
                "the backup's coordinator IEEE {want} differs from the NCP's {have}; this NCP can store \
                 another IEEE only once (the custom EUI-64 token, never again): set confirm_ieee_write_once \
                 to write it, or allow_ieee_change false to keep {have}"
            ))),
            Some(c) if c == target => Ok(IeeePlan::Reapply),
            Some(c) => Err(refused(format!(
                "the NCP's one-time IEEE token already holds {}: it cannot take the backup's {want}; \
                 set allow_ieee_change false to restore with {have}",
                ieee_hex(&c)
            ))),
            None => Err(CoordError::described(
                CoordError::Unsupported,
                format!(
                    "the NCP cannot change its IEEE {have} to the backup's {want}; set allow_ieee_change \
                     false to restore with {have}"
                ),
            )),
        })
    }

    /// The key of the restored-EUI64 stack token, if the NCP has the token
    /// interface (bellows' `can_rewrite_custom_eui64`).
    fn restored_eui64_token(&mut self) -> Result<Option<u32>, HostError> {
        for key in [
            token::CREATOR_STACK_RESTORED_EUI64,
            token::NVM3KEY_STACK_RESTORED_EUI64,
        ] {
            match self.try_call(&Command::GetTokenData {
                token: key,
                index: 0,
            })? {
                // invalidCommand: no token interface.
                None => return Ok(None),
                Some(Response::GetTokenData { status, data })
                    if status.is_success() && data.len() == 8 =>
                {
                    return Ok(Some(key));
                }
                Some(_) => {}
            }
        }
        Ok(None)
    }

    /// The worker ends the session and starts a new one from its loop (an
    /// NCP reset: ASH RST), so that the stack reads its tokens again; the
    /// restore goes on in [`Self::restore_resume`]. Not from here: the
    /// session start would run on top of the job's stack.
    fn reset_ncp(&mut self, r: Box<Restoring>) {
        self.restoring = Some(r);
        self.lost = true;
    }

    /// Starts a restore (bellows' order, Gecko SDK 4.4.6 frames): leave the
    /// current network, clear the key table, the coordinator IEEE (an NCP
    /// reset applies it), the NWK and APS frame counters plus their margins
    /// on [`PERSIST_STEP`] (`setValue`, without a network), the initial
    /// security state with `NO_FRAME_COUNTER_RESET` (the backup's network
    /// key and sequence, the hashed link key root), the key table
    /// (`importLinkKey`),
    /// `formNetwork`, the NWK update ID (the network management token:
    /// `formNetwork` does not take it), the child table (`setChildData`),
    /// and two NCP resets that must bring the restored network back (the
    /// second moves the frame counters past those used before the first:
    /// [`PERSIST_STEP`]), its counters read back into the report. The
    /// answer goes to `slot` once done.
    pub(super) fn restore_start(&mut self, plan: Box<Plan>, o: RestoreOptions, slot: Arc<Slot>) {
        let r = self.restore_begin(&plan, o);
        let (report, step) = match r {
            Ok(x) => x,
            Err(e) => return slot.answer(Answer::Restore(Err(e))),
        };
        let r = Box::new(Restoring {
            plan,
            report,
            step,
            slot,
        });
        match step {
            Step::Network => self.restore_network(r),
            Step::Ieee { .. } => self.reset_ncp(r),
            Step::Verify { .. } | Step::Advance { .. } => unreachable!("set after the network"),
        }
    }

    /// The session after a reset of a restore is up.
    pub(super) fn restore_resume(&mut self, mut r: Box<Restoring>) {
        match r.step {
            Step::Ieee { note } => {
                if self.eui64.0 != r.plan.ieee {
                    let e = refused(format!(
                        "the NCP reports IEEE {} after its reset, not the backup's {}",
                        ieee_hex(&self.eui64.0),
                        ieee_hex(&r.plan.ieee)
                    ));
                    return r.slot.answer(Answer::Restore(Err(e)));
                }
                if let Some(n) = note {
                    r.report.notes.push((0, n));
                }
                self.restore_network(r);
            }
            Step::Verify {
                update_id_written,
                used,
            } => {
                if let Err(e) = self.check_restored(&r.plan) {
                    return r.slot.answer(Answer::Restore(Err(e)));
                }
                if !update_id_written
                    && self
                        .network
                        .is_some_and(|n| n.nwk_update_id != r.plan.nwk_update_id)
                {
                    r.report
                        .notes
                        .push((0, restore_note::UPDATE_ID_NOT_RESTORED));
                }
                r.step = Step::Advance { used };
                self.reset_ncp(r);
            }
            Step::Advance { used } => {
                let result = self.check_restored(&r.plan).map(|()| {
                    self.left = None;
                    self.read_back(&mut r.report, used);
                    info!(
                        "ezsp: network restored (PAN 0x{:04X}, channel {}, {} link keys, {} children, NWK frame counter {}, APS {})",
                        r.plan.pan_id,
                        r.plan.channel,
                        r.report.link_keys_on_ncp,
                        r.report.children,
                        r.report.nwk_fc_start,
                        r.report.aps_fc_start
                    );
                    std::mem::take(&mut r.report)
                });
                r.slot.answer(Answer::Restore(result));
            }
            Step::Network => self.restore_network(r),
        }
    }

    /// The NCP did not come back from a restore's reset.
    pub(super) fn restore_lost(&mut self) {
        if let Some(r) = self.restoring.take() {
            let e = CoordError::described(
                CoordError::LinkDown,
                "the NCP did not come back after its reset",
            );
            r.slot.answer(Answer::Restore(Err(e)));
        }
    }

    /// Checks, leaves the current network, clears the key table and writes
    /// the IEEE.
    fn restore_begin(
        &mut self,
        plan: &Plan,
        o: RestoreOptions,
    ) -> Result<(RestoreReport, Step), CoordError> {
        let (ieee, c) = self.restore_plan(plan, o)?;
        let mut report = RestoreReport {
            nwk_fc_start: c.nwk_start,
            aps_fc_start: c.aps_start,
            ..RestoreReport::default()
        };
        if c.raised {
            info!(
                "ezsp: the NCP's frame counters are newer than the backup's: the restore starts from them (NWK {}, APS {})",
                c.nwk_start, c.aps_start
            );
            report.notes.push((0, restore_note::FLOOR_RAISED));
        }
        if c.unknown {
            warn!(
                "ezsp: the NCP stores a network whose frame counters it does not report: the restore adds 2^20 (NWK {}, APS {})",
                c.nwk_start, c.aps_start
            );
            report.notes.push((0, restore_note::COUNTERS_UNKNOWN));
        }
        if self.net != NetState::None || self.ownership != NetworkOwnership::NoNetwork {
            self.leave_reset()?;
        }
        if let Err(e) = self.expect_ok(&Command::ClearKeyTable) {
            if is_link_error(&e) {
                return Err(self.admin_error(e));
            }
            warn!("ezsp: clearKeyTable: {e}");
        }
        let target = plan.ieee;
        let note = match ieee {
            IeeePlan::Same => return Ok((report, Step::Network)),
            IeeePlan::KeepOwn => {
                report.notes.push((0, restore_note::IEEE_KEPT));
                return Ok((report, Step::Network));
            }
            IeeePlan::Rewrite(key) => {
                let cmd = Command::SetTokenData {
                    token: key,
                    index: 0,
                    data: target.to_vec(),
                };
                let r = self.expect_ok(&cmd);
                r.map_err(|e| self.admin_error(e))?;
                Some(restore_note::IEEE_REWRITTEN)
            }
            IeeePlan::WriteOnce => {
                warn!(
                    "ezsp: writing IEEE {} to the NCP's one-time custom EUI-64 token",
                    ieee_hex(&target)
                );
                let cmd = Command::SetMfgToken {
                    token: MfgTokenId::CUSTOM_EUI_64,
                    data: target.to_vec(),
                };
                let r = self.expect_ok(&cmd);
                r.map_err(|e| self.admin_error(e))?;
                Some(restore_note::IEEE_WRITTEN_ONCE)
            }
            IeeePlan::Reapply => None,
        };
        Ok((report, Step::Ieee { note }))
    }

    /// Security, link keys, the network, its update ID and children; then
    /// the final reset.
    fn restore_network(&mut self, mut r: Box<Restoring>) {
        let result = (|| {
            let nwk_fc = r.report.nwk_fc_start;
            let aps_fc = r.report.aps_fc_start;
            let s = self.restore_security(&r.plan, nwk_fc, aps_fc, &mut r.report);
            s.map_err(|e| self.admin_error(e))?;
            self.restore_form(&r.plan)?;
            let written = self.restore_update_id(r.plan.nwk_update_id);
            let written = written.map_err(|e| self.admin_error(e))?;
            let c = self.restore_children(&r.plan, &mut r.report);
            c.map_err(|e| self.admin_error(e))?;
            Ok(written)
        })();
        match result {
            Ok(update_id_written) => {
                // The stack reads the child table and the tokens again.
                let used = self.read_live().ok().and_then(|l| l.nwk);
                r.step = Step::Verify {
                    update_id_written,
                    used,
                };
                self.reset_ncp(r);
            }
            Err(e) => r.slot.answer(Answer::Restore(Err(e))),
        }
    }

    fn restore_security(
        &mut self,
        plan: &Plan,
        nwk_fc: u32,
        aps_fc: u32,
        report: &mut RestoreReport,
    ) -> Result<(), HostError> {
        // Frame counters only without a network, before the security
        // state that keeps them (bellows, zigbee-herdsman).
        self.expect_ok(&Command::SetValue {
            id: ValueId::NWK_FRAME_COUNTER,
            value: nwk_fc.to_le_bytes().to_vec(),
        })?;
        self.expect_ok(&Command::SetValue {
            id: ValueId::APS_FRAME_COUNTER,
            value: aps_fc.to_le_bytes().to_vec(),
        })?;
        let root = match plan.seed {
            Some(s) => s,
            None => {
                // Another stack's backup: the devices' link keys are in the
                // key table, if any (bellows does the same).
                report.notes.push((0, restore_note::TCLK_ROOT_GENERATED));
                self.random16()?
            }
        };
        use security_bitmask as sb;
        let state = InitialSecurityState {
            bitmask: sb::TRUST_CENTER_GLOBAL_LINK_KEY
                | sb::HAVE_PRECONFIGURED_KEY
                | sb::HAVE_NETWORK_KEY
                | sb::NO_FRAME_COUNTER_RESET
                | sb::REQUIRE_ENCRYPTED_KEY
                | sb::TRUST_CENTER_USES_HASHED_LINK_KEY,
            preconfigured_key: root,
            network_key: plan.network_key,
            network_key_sequence_number: plan.key_seq,
            preconfigured_trust_center_eui64: Eui64::default(),
        };
        self.expect_ok(&Command::SetInitialSecurityState(state))?;
        let ext = extended_security_bitmask::JOINER_GLOBAL_LINK_KEY
            | extended_security_bitmask::NWK_LEAVE_REQUEST_NOT_ALLOWED;
        self.host.call(&Command::SetValue {
            id: ValueId::EXTENDED_SECURITY_BITMASK,
            value: ext.to_le_bytes().to_vec(),
        })?;
        let size = self.cfg(ConfigId::KEY_TABLE_SIZE).unwrap_or(0).min(255);
        let mut slot: u16 = 0;
        for (i, d) in &plan.devices {
            let Some(l) = d.link else { continue };
            if d.ieee == self.eui64.0 {
                continue;
            }
            if slot >= size {
                report.notes.push((*i, restore_note::IGNORED));
                continue;
            }
            let cmd = Command::ImportLinkKey {
                index: slot as u8,
                eui64: Eui64(d.ieee),
                key: l.key,
            };
            match self.expect_ok(&cmd) {
                Ok(()) => {
                    slot += 1;
                    report.link_keys_on_ncp += 1;
                }
                Err(e) if is_link_error(&e) => return Err(e),
                Err(e) => {
                    warn!("ezsp: link key of {} not restored: {e}", ieee_hex(&d.ieee));
                    report.notes.push((*i, restore_note::IGNORED));
                }
            }
        }
        Ok(())
    }

    fn restore_form(&mut self, plan: &Plan) -> Result<(), CoordError> {
        let mut params = NetworkParameters {
            extended_pan_id: plan.ext_pan_id,
            pan_id: plan.pan_id,
            radio_tx_power: self.tx_power.or(plan.tx_power).unwrap_or(RESTORE_TX_POWER),
            radio_channel: plan.channel,
            join_method: JoinMethod::USE_MAC_ASSOCIATION,
            nwk_manager_id: plan.nwk_manager,
            nwk_update_id: plan.nwk_update_id,
            channels: plan.channel_mask,
        };
        let mut status = self.form_network(params)?;
        // The backup's TX power is the radio's that made it: an NCP that
        // does not have it forms at the default (bellows does the same).
        let from_backup = self.tx_power.is_none() && plan.tx_power.is_some();
        if from_backup
            && params.radio_tx_power != RESTORE_TX_POWER
            && matches!(
                status,
                Status::Ember(EmberStatus::PHY_INVALID_POWER)
                    | Status::Sl(SlStatus::TRANSMIT_INVALID_POWER)
            )
        {
            warn!(
                "ezsp: the NCP refuses the backup's TX power {} dBm; forming at {RESTORE_TX_POWER} dBm",
                params.radio_tx_power
            );
            params.radio_tx_power = RESTORE_TX_POWER;
            status = self.form_network(params)?;
        }
        if !status.is_success() {
            return Err(ezsp_error(id::FORM_NETWORK, status));
        }
        self.emit(CoordEvent::NetworkState(NetChange::Forming));
        let deadline = self.now() + self.opts.network_up_timeout.as_millis() as u64;
        if !self.wait(deadline, |w| w.net == NetState::Up) {
            return Err(if self.lost {
                CoordError::LinkDown
            } else {
                CoordError::described(CoordError::Timeout, "formNetwork: no NETWORK_UP")
            });
        }
        self.refresh_network();
        Ok(())
    }

    fn form_network(&mut self, params: NetworkParameters) -> Result<Status, CoordError> {
        match self.host.call(&Command::FormNetwork(params)) {
            Ok(Response::FormNetwork { status }) => Ok(status),
            Ok(other) => Err(host_error(&HostError::UnexpectedResponse(other))),
            Err(e) => Err(self.admin_error(e)),
        }
    }

    /// The NWK update ID through the network management stack token
    /// (bellows' `write_nwk_update_id`: `formNetwork` ignores it); `false`
    /// when the NCP has no token interface or the token.
    fn restore_update_id(&mut self, update_id: u8) -> Result<bool, HostError> {
        if self.network.is_some_and(|n| n.nwk_update_id == update_id) {
            return Ok(true);
        }
        let at = token::NETWORK_MANAGEMENT_UPDATE_ID;
        for key in [
            token::CREATOR_STACK_NETWORK_MANAGEMENT,
            token::NVM3KEY_STACK_NETWORK_MANAGEMENT,
        ] {
            match self.try_call(&Command::GetTokenData {
                token: key,
                index: 0,
            })? {
                None => return Ok(false),
                Some(Response::GetTokenData { status, mut data })
                    if status.is_success() && data.len() > at =>
                {
                    data[at] = update_id;
                    let r = self.try_call(&Command::SetTokenData {
                        token: key,
                        index: 0,
                        data,
                    })?;
                    return Ok(r
                        .as_ref()
                        .and_then(Response::status)
                        .is_some_and(|s| s.is_success()));
                }
                Some(_) => {}
            }
        }
        Ok(false)
    }

    /// The backup's children with a short address, into the child table
    /// (`setChildData`, as bellows does) up to `MAX_END_DEVICE_CHILDREN`.
    fn restore_children(
        &mut self,
        plan: &Plan,
        report: &mut RestoreReport,
    ) -> Result<(), HostError> {
        let max = self
            .cfg(ConfigId::MAX_END_DEVICE_CHILDREN)
            .unwrap_or(0)
            .min(255);
        let mut index: u16 = 0;
        for (i, d) in &plan.devices {
            let (true, Some(nwk)) = (d.is_child, d.nwk) else {
                continue;
            };
            if index >= max {
                report.notes.push((*i, restore_note::CHILD_DEMOTED));
                continue;
            }
            let c = d.child.unwrap_or(BackupChild {
                capability: 0x80,
                timeout_exp: 8,
            });
            let data = ChildData {
                eui64: Eui64(d.ieee),
                // MAC capability bit 3: receiver on when idle.
                node_type: if c.capability & 0x08 != 0 {
                    NodeType::END_DEVICE
                } else {
                    NodeType::SLEEPY_END_DEVICE
                },
                id: nwk,
                phy: 0,
                power: 0,
                timeout: c.timeout_exp,
                remaining_timeout: 0,
            };
            match self.expect_ok(&Command::SetChildData {
                index: index as u8,
                data,
            }) {
                Ok(()) => {
                    self.fix_child_flags(index as u8, data.eui64, nwk)?;
                    index += 1;
                    report.children += 1;
                }
                Err(e) if is_link_error(&e) => return Err(e),
                Err(e) => {
                    warn!("ezsp: child {} not restored: {e}", ieee_hex(&d.ieee));
                    report.notes.push((*i, restore_note::CHILD_DEMOTED));
                }
            }
        }
        Ok(())
    }

    /// bellows' workaround (EZSP ≥ 10): `setChildData` leaves a flags byte
    /// of the NVM child table entry 0x00 where a working coordinator has
    /// 0x80. With the token interface, the entry is read back; if it is
    /// exactly this child (EUI64, node ID) with other flags, it is written
    /// with 0x80. Best effort: another layout or no token interface leaves
    /// it.
    fn fix_child_flags(&mut self, index: u8, eui64: Eui64, id: NodeId) -> Result<(), HostError> {
        let mut entry = eui64.0.to_vec();
        entry.extend_from_slice(&id.to_le_bytes());
        entry.push(token::CHILD_TABLE_FLAGS);
        for key in [
            token::NVM3KEY_STACK_CHILD_TABLE,
            token::CREATOR_STACK_CHILD_TABLE,
        ] {
            let data = match self.try_call(&Command::GetTokenData {
                token: key,
                index: u32::from(index),
            })? {
                None => return Ok(()),
                Some(Response::GetTokenData { status, data }) if status.is_success() => data,
                Some(_) => continue,
            };
            if data.len() != entry.len() || data[..10] != entry[..10] {
                warn!("ezsp: child table entry {index} not as expected: left as it is");
            } else if data[10] != token::CHILD_TABLE_FLAGS {
                let r = self.try_call(&Command::SetTokenData {
                    token: key,
                    index: u32::from(index),
                    data: entry,
                })?;
                if r.as_ref()
                    .and_then(Response::status)
                    .is_none_or(|s| !s.is_success())
                {
                    warn!("ezsp: child table entry {index}: flags not written");
                }
            }
            return Ok(());
        }
        Ok(())
    }

    /// After the reset, the NCP must run the backup's network.
    /// The frame counters the restored network resumed from after the
    /// restore's last reset, as the NCP reports them, into the report
    /// (those written stay where it does not report them); a warning if
    /// the NWK counter is not above `used` (before the first reset).
    fn read_back(&mut self, report: &mut RestoreReport, used: Option<u32>) {
        let Ok(live) = self.read_live() else { return };
        if let (Some(now), Some(used)) = (live.nwk, used)
            && now <= used
        {
            warn!(
                "ezsp: the NCP resumed the NWK frame counter at {now} after the restore, not above the {used} it used before: devices may drop its first frames"
            );
        }
        for (now, set) in [
            (live.nwk, &mut report.nwk_fc_start),
            (live.aps, &mut report.aps_fc_start),
        ] {
            if let Some(now) = now {
                *set = now;
            }
        }
    }

    fn check_restored(&mut self, plan: &Plan) -> Result<(), CoordError> {
        let NetworkOwnership::Observed(n) = self.ownership else {
            return Err(refused(
                "the restored network did not come up after the NCP's reset",
            ));
        };
        let key_ok = n
            .key_id
            .is_none_or(|k| k == NetworkIdentity::key_id_for(&plan.network_key));
        if n.ext_pan_id != plan.ext_pan_id || n.pan_id != plan.pan_id || !key_ok {
            return Err(refused(
                "after the NCP's reset it runs another network than the restored one",
            ));
        }
        Ok(())
    }
}
