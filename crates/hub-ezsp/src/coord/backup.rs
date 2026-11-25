//! The EZSP backend's restore data: [`BackupItem`]s in a compact binary
//! encoding ([`BlobEncoder`], the [`RestoreBlob`] of
//! [`NetworkAdmin::backup_restore`](hub_coord::NetworkAdmin::backup_restore)),
//! and the checked [`Plan`] the worker restores from it. The blob holds
//! keys: it is never logged, and [`Plan`]'s `Debug` leaves them out.

use hub_coord::{
    BackendKind, BackupChild, BackupDevice, BackupItem, BackupLinkKey, Code, CoordError, Kind,
    RestoreBlob, RestoreEncoder,
};

use crate::ezsp::types::{EmberStatus, KeyData};
use crate::ezsp::wire::{DecodeError, Reader, Writer};

const NODE: u8 = 1;
const NETWORK: u8 = 2;
const NETWORK_KEY: u8 = 3;
const TC_LINK_KEY: u8 = 4;
const HASHED_TCLK_SEED: u8 = 5;
const RS_PARAMS: u8 = 6;
const DEVICE: u8 = 7;

fn opt_u16(w: &mut Writer, v: Option<u16>) {
    w.bool(v.is_some());
    w.u16(v.unwrap_or(0));
}

/// Appends `item` to `w`.
fn write_item(w: &mut Writer, item: &BackupItem) {
    match *item {
        BackupItem::Node {
            ieee,
            ieee_override,
        } => {
            w.u8(NODE);
            w.bytes(&ieee);
            w.bool(ieee_override);
        }
        BackupItem::Network {
            pan_id,
            ext_pan_id,
            channel,
            channel_mask,
            nwk_update_id,
            nwk_manager,
            security_level,
            tx_power,
        } => {
            w.u8(NETWORK);
            w.u16(pan_id);
            w.bytes(&ext_pan_id);
            w.u8(channel);
            w.u32(channel_mask);
            w.u8(nwk_update_id);
            w.u16(nwk_manager);
            w.u8(security_level);
            w.bool(tx_power.is_some());
            w.u8(tx_power.unwrap_or(0) as u8);
        }
        BackupItem::NetworkKey {
            key,
            seq,
            tx_counter,
            active,
        } => {
            w.u8(NETWORK_KEY);
            w.bytes(&key);
            w.u8(seq);
            w.u32(tx_counter);
            w.bool(active);
        }
        BackupItem::TcLinkKey {
            key,
            tc_ieee,
            tx_counter,
            hashed,
        } => {
            w.u8(TC_LINK_KEY);
            w.bytes(&key);
            w.bytes(&tc_ieee);
            w.u32(tx_counter);
            w.bool(hashed);
        }
        BackupItem::HashedTclkSeed(seed) => {
            w.u8(HASHED_TCLK_SEED);
            w.bytes(&seed);
        }
        BackupItem::RsParams {
            fc_stride,
            table_epoch,
            nwk_fc_limit,
            aps_fc_limit,
            host_backed_tables,
        } => {
            w.u8(RS_PARAMS);
            w.u32(fc_stride);
            w.u32(table_epoch);
            w.u32(nwk_fc_limit);
            w.u32(aps_fc_limit);
            w.bool(host_backed_tables);
        }
        BackupItem::Device(d) => {
            w.u8(DEVICE);
            w.bytes(&d.ieee);
            opt_u16(w, d.nwk);
            w.bool(d.is_child);
            w.bool(d.child.is_some());
            let c = d.child.unwrap_or(BackupChild {
                capability: 0,
                timeout_exp: 0,
            });
            w.u8(c.capability);
            w.u8(c.timeout_exp);
            w.bool(d.link.is_some());
            if let Some(l) = d.link {
                w.bytes(&l.key);
                w.u8(l.key_seq);
                w.u8(l.attrs);
                w.u8(l.origin);
                w.u32(l.tx_counter);
                w.u32(l.rx_floor);
            }
        }
    }
}

fn read_item(r: &mut Reader<'_>) -> Result<BackupItem, DecodeError> {
    let tag = r.u8()?;
    Ok(match tag {
        NODE => BackupItem::Node {
            ieee: r.array()?,
            ieee_override: r.bool()?,
        },
        NETWORK => BackupItem::Network {
            pan_id: r.u16()?,
            ext_pan_id: r.array()?,
            channel: r.u8()?,
            channel_mask: r.u32()?,
            nwk_update_id: r.u8()?,
            nwk_manager: r.u16()?,
            security_level: r.u8()?,
            tx_power: {
                let some = r.bool()?;
                let p = r.i8()?;
                some.then_some(p)
            },
        },
        NETWORK_KEY => BackupItem::NetworkKey {
            key: r.array()?,
            seq: r.u8()?,
            tx_counter: r.u32()?,
            active: r.bool()?,
        },
        TC_LINK_KEY => BackupItem::TcLinkKey {
            key: r.array()?,
            tc_ieee: r.array()?,
            tx_counter: r.u32()?,
            hashed: r.bool()?,
        },
        HASHED_TCLK_SEED => BackupItem::HashedTclkSeed(r.array()?),
        RS_PARAMS => BackupItem::RsParams {
            fc_stride: r.u32()?,
            table_epoch: r.u32()?,
            nwk_fc_limit: r.u32()?,
            aps_fc_limit: r.u32()?,
            host_backed_tables: r.bool()?,
        },
        DEVICE => {
            let ieee = r.array()?;
            let has_nwk = r.bool()?;
            let nwk = r.u16()?;
            let is_child = r.bool()?;
            let has_child = r.bool()?;
            let child = BackupChild {
                capability: r.u8()?,
                timeout_exp: r.u8()?,
            };
            let link = if r.bool()? {
                Some(BackupLinkKey {
                    key: r.array()?,
                    key_seq: r.u8()?,
                    attrs: r.u8()?,
                    origin: r.u8()?,
                    tx_counter: r.u32()?,
                    rx_floor: r.u32()?,
                })
            } else {
                None
            };
            BackupItem::Device(BackupDevice {
                ieee,
                nwk: has_nwk.then_some(nwk),
                is_child,
                child: has_child.then_some(child),
                link,
            })
        }
        _ => return Err(DecodeError::Invalid("backup item type")),
    })
}

/// Builds the EZSP backend's [`RestoreBlob`].
#[derive(Default)]
pub struct BlobEncoder {
    blob: Vec<u8>,
}

impl RestoreEncoder for BlobEncoder {
    fn push(&mut self, item: &BackupItem) -> Result<(), CoordError> {
        let mut w = Writer::new();
        write_item(&mut w, item);
        let b = w.into_inner();
        self.blob
            .try_reserve(b.len())
            .map_err(|_| CoordError::Other("out of memory".into()))?;
        self.blob.extend_from_slice(&b);
        Ok(())
    }

    fn finish(self: Box<Self>) -> RestoreBlob {
        RestoreBlob(self.blob)
    }
}

fn invalid(why: impl Into<std::borrow::Cow<'static, str>>) -> CoordError {
    CoordError::described(
        CoordError::Backend {
            code: Code {
                backend: BackendKind::Ezsp,
                value: u16::from(EmberStatus::BAD_ARGUMENT.0),
                text: None,
            },
            kind: Kind::InvalidParameter,
        },
        why,
    )
}

/// The trust center link key of a backup.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct TcLink {
    pub(crate) key: KeyData,
    pub(crate) tx_counter: u32,
}

/// A checked restore: the network, its keys and devices.
#[derive(Clone)]
pub(crate) struct Plan {
    /// Coordinator IEEE (wire order).
    pub(crate) ieee: [u8; 8],
    pub(crate) pan_id: u16,
    pub(crate) ext_pan_id: [u8; 8],
    pub(crate) channel: u8,
    pub(crate) channel_mask: u32,
    pub(crate) nwk_update_id: u8,
    pub(crate) nwk_manager: u16,
    pub(crate) tx_power: Option<i8>,
    pub(crate) network_key: KeyData,
    pub(crate) key_seq: u8,
    pub(crate) nwk_counter: u32,
    pub(crate) tc_link: Option<TcLink>,
    /// The hashed link key root (`stack_specific.ezsp.hashed_tclk`).
    pub(crate) seed: Option<KeyData>,
    /// Devices with their item index.
    pub(crate) devices: Vec<(u16, BackupDevice)>,
}

impl std::fmt::Debug for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plan")
            .field("pan_id", &format_args!("0x{:04X}", self.pan_id))
            .field("ext_pan_id", &self.ext_pan_id)
            .field("channel", &self.channel)
            .field("devices", &self.devices.len())
            .finish_non_exhaustive()
    }
}

impl Plan {
    /// Decodes and checks a blob of [`BlobEncoder`]: the coordinator, the
    /// network (security level 5, channel 11–26) and its network key are
    /// required.
    pub(crate) fn decode(blob: &[u8]) -> Result<Plan, CoordError> {
        let mut r = Reader::new(blob);
        let (mut node, mut network, mut nkey, mut tc_link, mut seed) =
            (None, None, None, None, None);
        let mut devices = Vec::new();
        let mut index: u16 = 0;
        while r.remaining() > 0 {
            let item = read_item(&mut r).map_err(|e| invalid(format!("restore data: {e}")))?;
            match item {
                BackupItem::Node { ieee, .. } => node = Some(ieee),
                n @ BackupItem::Network { .. } => network = Some(n),
                BackupItem::NetworkKey {
                    key,
                    seq,
                    tx_counter,
                    ..
                } => nkey = Some((key, seq, tx_counter)),
                BackupItem::TcLinkKey {
                    key, tx_counter, ..
                } => tc_link = Some(TcLink { key, tx_counter }),
                BackupItem::HashedTclkSeed(s) => seed = Some(s),
                BackupItem::RsParams { .. } => {}
                BackupItem::Device(d) => {
                    devices
                        .try_reserve(1)
                        .map_err(|_| CoordError::Other("out of memory".into()))?;
                    devices.push((index, d));
                }
            }
            index = index.saturating_add(1);
        }
        let ieee = node.ok_or_else(|| invalid("the backup names no coordinator"))?;
        let Some(BackupItem::Network {
            pan_id,
            ext_pan_id,
            channel,
            channel_mask,
            nwk_update_id,
            nwk_manager,
            security_level,
            tx_power,
        }) = network
        else {
            return Err(invalid("the backup has no network"));
        };
        let (network_key, key_seq, nwk_counter) =
            nkey.ok_or_else(|| invalid("the backup has no network key (incomplete backup)"))?;
        if security_level != 5 {
            return Err(invalid(format!(
                "security level {security_level} (only 5 is supported)"
            )));
        }
        if !(11..=26).contains(&channel) {
            return Err(invalid(format!("channel {channel} outside 11-26")));
        }
        if network_key == [0; 16] {
            return Err(invalid("all-zero network key"));
        }
        let mask = channel_mask & 0x07FF_F800;
        Ok(Plan {
            ieee,
            pan_id,
            ext_pan_id,
            channel,
            channel_mask: if mask & (1 << channel) != 0 {
                mask
            } else {
                1 << channel
            },
            nwk_update_id,
            nwk_manager,
            tx_power,
            network_key,
            key_seq,
            nwk_counter,
            tc_link,
            seed: seed.filter(|s| *s != [0; 16]),
            devices,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<BackupItem> {
        vec![
            BackupItem::Node {
                ieee: [1, 2, 3, 4, 5, 6, 7, 8],
                ieee_override: true,
            },
            BackupItem::Network {
                pan_id: 0x1A62,
                ext_pan_id: [0xDD; 8],
                channel: 15,
                channel_mask: 1 << 15 | 1 << 20,
                nwk_update_id: 3,
                nwk_manager: 0,
                security_level: 5,
                tx_power: Some(-4),
            },
            BackupItem::NetworkKey {
                key: [0x37; 16],
                seq: 1,
                tx_counter: 112_233,
                active: true,
            },
            BackupItem::TcLinkKey {
                key: *b"ZigBeeAlliance09",
                tc_ieee: [1, 2, 3, 4, 5, 6, 7, 8],
                tx_counter: 4096,
                hashed: true,
            },
            BackupItem::HashedTclkSeed([0x5C; 16]),
            BackupItem::RsParams {
                fc_stride: 1,
                table_epoch: 2,
                nwk_fc_limit: 3,
                aps_fc_limit: 4,
                host_backed_tables: true,
            },
            BackupItem::Device(BackupDevice {
                ieee: [9; 8],
                nwk: Some(0x4D32),
                is_child: true,
                child: Some(BackupChild {
                    capability: 0x80,
                    timeout_exp: 8,
                }),
                link: Some(BackupLinkKey {
                    key: [0x42; 16],
                    key_seq: 0,
                    attrs: 2,
                    origin: 3,
                    tx_counter: 120,
                    rx_floor: 6,
                }),
            }),
            BackupItem::Device(BackupDevice {
                ieee: [10; 8],
                nwk: None,
                is_child: false,
                child: None,
                link: None,
            }),
        ]
    }

    fn blob(items: &[BackupItem]) -> RestoreBlob {
        let mut e = Box::<BlobEncoder>::default();
        for i in items {
            e.push(i).unwrap();
        }
        e.finish()
    }

    #[test]
    fn every_item_round_trips() {
        let items = items();
        let b = blob(&items);
        let mut r = Reader::new(&b.0);
        let mut back = Vec::new();
        while r.remaining() > 0 {
            back.push(read_item(&mut r).unwrap());
        }
        assert_eq!(back, items);
    }

    #[test]
    fn a_plan_needs_the_network_and_its_key_and_hides_them_in_debug() {
        let items = items();
        let p = Plan::decode(&blob(&items).0).unwrap();
        assert_eq!(
            (p.pan_id, p.channel, p.nwk_update_id, p.nwk_counter),
            (0x1A62, 15, 3, 112_233)
        );
        assert_eq!(p.seed, Some([0x5C; 16]));
        assert_eq!(p.tc_link.map(|t| t.tx_counter), Some(4096));
        assert_eq!(
            p.devices.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            [6, 7]
        );
        let dbg = format!("{p:?}");
        assert!(
            !dbg.contains("55, 55") && !dbg.contains("92, 92"),
            "no key bytes: {dbg}"
        );
        for skip in [0, 1, 2] {
            let some: Vec<_> = items
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != skip)
                .map(|(_, x)| *x)
                .collect();
            assert!(Plan::decode(&blob(&some).0).is_err(), "without item {skip}");
        }
        assert!(Plan::decode(&[0xEE]).is_err(), "unknown item");
    }
}
