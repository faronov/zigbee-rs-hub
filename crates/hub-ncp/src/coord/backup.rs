//! Coordinator backups as ZRHP record streams (spec 9.3): the records of
//! `BKP_EXPORT_*` as [`BackupItem`]s, and items as the records of
//! `BKP_RESTORE_*`.

use hub_coord::{
    BackupChild, BackupDevice, BackupItem, BackupLinkKey, BackupSink, CoordError, RestoreBlob,
    RestoreEncoder,
};
use zigbee_zrhp::codec::{Reader, Wire, Writer};
use zigbee_zrhp::records::{
    ChildInfo, DeviceRecord, LinkBlock, NetworkKeyRecord, NetworkRecord, NodeRecord, RawRecords,
    Record, RsParamsRecord, TcLinkKeyRecord, device_flags, record_type,
};
use zigbee_zrhp::types::Ieee;

/// `NETWORK.tx_power` of an unknown TX power.
const TX_POWER_UNKNOWN: i8 = 0x7F;

/// The record of an item.
fn record(item: &BackupItem) -> Record<'static> {
    match *item {
        BackupItem::Node {
            ieee,
            ieee_override,
        } => Record::Node(NodeRecord {
            ieee: Ieee(ieee),
            nwk: 0,
            logical_type: 0,
            flags: u8::from(ieee_override),
        }),
        BackupItem::Network {
            pan_id,
            ext_pan_id,
            channel,
            channel_mask,
            nwk_update_id,
            nwk_manager,
            security_level,
            tx_power,
        } => Record::Network(NetworkRecord {
            pan_id,
            ext_pan_id,
            channel,
            channel_mask,
            nwk_update_id,
            nwk_manager,
            security_level,
            tx_power: tx_power.unwrap_or(TX_POWER_UNKNOWN),
        }),
        BackupItem::NetworkKey {
            key,
            seq,
            tx_counter,
            active,
        } => Record::NetworkKey(NetworkKeyRecord {
            key,
            seq,
            tx_counter,
            flags: u8::from(active),
        }),
        BackupItem::TcLinkKey {
            key,
            tc_ieee,
            tx_counter,
            hashed,
        } => Record::TcLinkKey(TcLinkKeyRecord {
            key,
            tc_ieee: Ieee(tc_ieee),
            tx_counter,
            flags: u8::from(hashed),
        }),
        BackupItem::HashedTclkSeed(seed) => Record::EzspHashedTclk(seed),
        BackupItem::RsParams {
            fc_stride,
            table_epoch,
            nwk_fc_limit,
            aps_fc_limit,
            host_backed_tables,
        } => Record::RsParams(RsParamsRecord {
            fc_stride,
            table_epoch,
            nwk_fc_limit,
            aps_fc_limit,
            flags: u8::from(host_backed_tables),
        }),
        BackupItem::Device(d) => {
            let mut flags = 0;
            if d.nwk.is_some() {
                flags |= device_flags::NWK_VALID;
            }
            if d.is_child {
                flags |= device_flags::IS_CHILD;
            }
            if d.child.is_some() {
                flags |= device_flags::CHILD_INFO;
            }
            if d.link.is_some() {
                flags |= device_flags::LINK;
            }
            Record::Device(DeviceRecord {
                ieee: Ieee(d.ieee),
                nwk: d.nwk.unwrap_or(0xFFFF),
                flags,
                child: d.child.map(|c| ChildInfo {
                    capability: c.capability,
                    timeout_exp: c.timeout_exp,
                }),
                link: d.link.map(|l| LinkBlock {
                    key: l.key,
                    key_seq: l.key_seq,
                    attrs: l.attrs,
                    origin: l.origin,
                    tx_counter: l.tx_counter,
                    rx_floor: l.rx_floor,
                }),
            })
        }
    }
}

/// The item of a record; `None` for the record types a backup file does
/// not carry.
fn item(r: &Record<'_>) -> Option<BackupItem> {
    Some(match r {
        Record::Node(n) => BackupItem::Node {
            ieee: n.ieee.0,
            ieee_override: n.flags & 1 != 0,
        },
        Record::Network(n) => BackupItem::Network {
            pan_id: n.pan_id,
            ext_pan_id: n.ext_pan_id,
            channel: n.channel,
            channel_mask: n.channel_mask,
            nwk_update_id: n.nwk_update_id,
            nwk_manager: n.nwk_manager,
            security_level: n.security_level,
            tx_power: (n.tx_power != TX_POWER_UNKNOWN).then_some(n.tx_power),
        },
        Record::NetworkKey(k) => BackupItem::NetworkKey {
            key: k.key,
            seq: k.seq,
            tx_counter: k.tx_counter,
            active: k.flags & 1 != 0,
        },
        Record::TcLinkKey(k) => BackupItem::TcLinkKey {
            key: k.key,
            tc_ieee: k.tc_ieee.0,
            tx_counter: k.tx_counter,
            hashed: k.flags & 1 != 0,
        },
        Record::EzspHashedTclk(s) => BackupItem::HashedTclkSeed(*s),
        Record::RsParams(p) => BackupItem::RsParams {
            fc_stride: p.fc_stride,
            table_epoch: p.table_epoch,
            nwk_fc_limit: p.nwk_fc_limit,
            aps_fc_limit: p.aps_fc_limit,
            host_backed_tables: p.flags & 1 != 0,
        },
        Record::Device(d) => BackupItem::Device(BackupDevice {
            ieee: d.ieee.0,
            nwk: (d.flags & device_flags::NWK_VALID != 0).then_some(d.nwk),
            is_child: d.flags & device_flags::IS_CHILD != 0,
            child: d.child.as_ref().map(|c| BackupChild {
                capability: c.capability,
                timeout_exp: c.timeout_exp,
            }),
            link: d.link.as_ref().map(|l| BackupLinkKey {
                key: l.key,
                key_seq: l.key_seq,
                attrs: l.attrs,
                origin: l.origin,
                tx_counter: l.tx_counter,
                rx_floor: l.rx_floor,
            }),
        }),
        _ => return None,
    })
}

/// Feeds the items of a record stream from `BKP_EXPORT_*` to `sink`, up to
/// its `END`.
pub fn export_items(stream: &[u8], sink: &mut dyn BackupSink) -> Result<(), CoordError> {
    for raw in RawRecords::new(stream) {
        let (rtype, tlv) =
            raw.map_err(|e| CoordError::Other(format!("record stream: {e:?}").into()))?;
        if rtype == record_type::END {
            break;
        }
        match Reader::new(tlv).read::<Record>() {
            Ok(r) => {
                if let Some(i) = item(&r) {
                    sink.item(&i)?;
                }
            }
            Err(e) => return Err(CoordError::Other(format!("record: {e:?}").into())),
        }
    }
    Ok(())
}

/// Encodes restore items as ZRHP records, appended to one stream (no
/// `END`: the host core adds it).
#[derive(Debug, Default)]
pub struct RecordEncoder {
    stream: Vec<u8>,
}

impl RestoreEncoder for RecordEncoder {
    fn push(&mut self, item: &BackupItem) -> Result<(), CoordError> {
        let mut buf = [0u8; 64];
        let mut w = Writer::new(&mut buf);
        record(item)
            .write(&mut w)
            .map_err(|e| CoordError::Other(format!("encoding: {e:?}").into()))?;
        let n = w.len();
        self.stream
            .try_reserve(n)
            .map_err(|_| CoordError::Other("out of memory".into()))?;
        self.stream.extend_from_slice(&buf[..n]);
        Ok(())
    }

    fn finish(self: Box<Self>) -> RestoreBlob {
        RestoreBlob(self.stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IEEE: [u8; 8] = [0x78, 0x56, 0x34, 0x12, 0x00, 0x4b, 0x12, 0x00];

    fn items() -> Vec<BackupItem> {
        vec![
            BackupItem::Node {
                ieee: IEEE,
                ieee_override: true,
            },
            BackupItem::Network {
                pan_id: 0x1A62,
                ext_pan_id: [0xDD; 8],
                channel: 15,
                channel_mask: 1 << 15 | 1 << 20 | 1 << 25,
                nwk_update_id: 2,
                nwk_manager: 0,
                security_level: 5,
                tx_power: Some(4),
            },
            BackupItem::NetworkKey {
                key: [0x37; 16],
                seq: 1,
                tx_counter: 112_233,
                active: true,
            },
            BackupItem::TcLinkKey {
                key: *b"ZigBeeAlliance09",
                tc_ieee: IEEE,
                tx_counter: 4096,
                hashed: false,
            },
            BackupItem::HashedTclkSeed([0x11; 16]),
            BackupItem::RsParams {
                fc_stride: 1024,
                table_epoch: 7,
                nwk_fc_limit: 5000,
                aps_fc_limit: 6000,
                host_backed_tables: true,
            },
            BackupItem::Device(BackupDevice {
                ieee: [0x5b, 0x1a, 0x7c, 0x01, 0x00, 0x8d, 0x15, 0x00],
                nwk: Some(0x4D32),
                is_child: true,
                child: Some(BackupChild {
                    capability: 0x80,
                    timeout_exp: 8,
                }),
                link: Some(BackupLinkKey {
                    key: [0x0f; 16],
                    key_seq: 0,
                    attrs: 2,
                    origin: 3,
                    tx_counter: 120,
                    rx_floor: 6,
                }),
            }),
            BackupItem::Device(BackupDevice {
                ieee: [9; 8],
                nwk: None,
                is_child: false,
                child: None,
                link: None,
            }),
        ]
    }

    fn encode(items: &[BackupItem]) -> Vec<u8> {
        let mut enc: Box<dyn RestoreEncoder> = Box::<RecordEncoder>::default();
        for i in items {
            enc.push(i).unwrap();
        }
        enc.finish().0
    }

    fn records(stream: &[u8]) -> Vec<Record<'_>> {
        RawRecords::new(stream)
            .flatten()
            .map(|(_, t)| Reader::new(t).read::<Record>().unwrap())
            .collect()
    }

    #[test]
    fn items_become_records_with_their_flags() {
        let stream = encode(&items());
        let recs = records(&stream);
        assert_eq!(recs.len(), 8);
        assert!(
            matches!(&recs[0], Record::Node(n) if n.ieee.0 == IEEE && n.nwk == 0 && n.flags == 1)
        );
        assert!(
            matches!(&recs[1], Record::Network(n) if n.pan_id == 0x1A62 && n.channel_mask == (1 << 15 | 1 << 20 | 1 << 25) && n.nwk_update_id == 2 && n.tx_power == 4)
        );
        assert!(
            matches!(&recs[2], Record::NetworkKey(k) if k.seq == 1 && k.tx_counter == 112_233 && k.flags == 1)
        );
        assert!(matches!(&recs[3], Record::TcLinkKey(k) if k.tx_counter == 4096 && k.flags == 0));
        assert!(matches!(&recs[5], Record::RsParams(p) if p.fc_stride == 1024 && p.flags == 1));
        let Record::Device(d) = &recs[6] else {
            panic!()
        };
        assert_eq!((d.ieee.0[0], d.nwk), (0x5b, 0x4D32));
        assert_eq!(
            d.flags,
            device_flags::NWK_VALID
                | device_flags::IS_CHILD
                | device_flags::CHILD_INFO
                | device_flags::LINK
        );
        assert_eq!(d.link.as_ref().unwrap().rx_floor, 6);
        let Record::Device(d) = &recs[7] else {
            panic!()
        };
        assert_eq!(
            (d.flags, d.nwk),
            (0, 0xFFFF),
            "nwk unknown, not a child, no key"
        );
        let unknown = encode(&[BackupItem::Network {
            pan_id: 1,
            ext_pan_id: [1; 8],
            channel: 11,
            channel_mask: 1 << 11,
            nwk_update_id: 0,
            nwk_manager: 0,
            security_level: 5,
            tx_power: None,
        }]);
        assert!(
            matches!(&records(&unknown)[0], Record::Network(n) if n.tx_power == TX_POWER_UNKNOWN)
        );
    }

    #[derive(Default)]
    struct Sink(Vec<BackupItem>);

    impl BackupSink for Sink {
        fn item(&mut self, item: &BackupItem) -> Result<(), CoordError> {
            self.0.push(*item);
            Ok(())
        }
    }

    #[test]
    fn exported_records_become_the_same_items() {
        let mut stream = encode(&items());
        // Records a backup file does not carry are skipped; `END` ends it.
        let mut buf = [0u8; 64];
        let mut w = Writer::new(&mut buf);
        Record::Unknown {
            rtype: 0x30,
            value: &[1, 2],
        }
        .write(&mut w)
        .unwrap();
        stream.extend_from_slice(w.written());
        stream.extend_from_slice(&encode(&items()[..1]));
        let mut w = Writer::new(&mut buf);
        Record::End(crate::lifecycle::end_record(&stream))
            .write(&mut w)
            .unwrap();
        stream.extend_from_slice(w.written());
        stream.extend_from_slice(&encode(&items()[..1]));
        let mut sink = Sink::default();
        export_items(&stream, &mut sink).unwrap();
        let mut want = items();
        want.push(want[0]);
        assert_eq!(sink.0, want);
        let cut = encode(&items());
        assert!(
            export_items(&cut[..cut.len() - 3], &mut Sink::default()).is_err(),
            "truncated"
        );
    }
}
