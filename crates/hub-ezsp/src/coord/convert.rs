//! EZSP ⇄ `hub-coord`: statuses and host errors as [`CoordError`]s, the
//! terminal of a `messageSentHandler`, incoming messages as
//! [`ApsIndication`]s, the [`Capabilities`] of a session and the
//! EmberZNet counter names.

use std::collections::BTreeMap;

use hub_coord::{
    ApsIndication, BackendKind, BackupSupport, Capabilities, Code, CoordError, GpSupport, Kind,
    OwnershipModel, RadioOps, Tables, Terminal,
};

use crate::ezsp::types::*;
use crate::host::HostError;

/// The wire value of a status, as a [`Code`] value (EZSP 13: 8 bits;
/// `sl_status_t` values in use fit 16 bits).
fn code_value(s: Status) -> u16 {
    match s {
        Status::Ezsp(s) => u16::from(s.0),
        Status::Ember(s) => u16::from(s.0),
        Status::Sl(s) => u16::try_from(s.0).unwrap_or(u16::MAX),
    }
}

fn code(s: Status, text: Option<&'static str>) -> Code {
    Code {
        backend: BackendKind::Ezsp,
        value: code_value(s),
        text,
    }
}

/// Statuses that mean "no buffer or slot now, try later": the APS unicast
/// limit, packet buffers, the broadcast table.
fn is_busy(s: Status) -> bool {
    match s {
        Status::Ember(e) => matches!(
            e,
            EmberStatus::MAX_MESSAGE_LIMIT_REACHED
                | EmberStatus::NO_BUFFERS
                | EmberStatus::NETWORK_BUSY
        ),
        Status::Sl(e) => matches!(
            e,
            SlStatus::ZIGBEE_MAX_MESSAGE_LIMIT_REACHED
                | SlStatus::BUSY
                | SlStatus::ALLOCATION_FAILED
        ),
        Status::Ezsp(e) => matches!(e, EzspStatus::ERROR_OUT_OF_MEMORY | EzspStatus::NO_TX_SPACE),
    }
}

fn kind(s: Status) -> Kind {
    use StatusKind as K;
    match s.kind() {
        K::MessageTooLong => return Kind::TooLarge,
        K::NetworkDown | K::NotJoined | K::InvalidCall => return Kind::InvalidState,
        K::InvalidValue | K::InvalidId => return Kind::InvalidParameter,
        _ => {}
    }
    match s {
        Status::Ember(e) => match e {
            EmberStatus::BAD_ARGUMENT | EmberStatus::INDEX_OUT_OF_RANGE => Kind::InvalidParameter,
            EmberStatus::TABLE_FULL => Kind::TableFull,
            EmberStatus::NOT_FOUND => Kind::NotFound,
            EmberStatus::KEY_INVALID | EmberStatus::SECURITY_STATE_NOT_SET => Kind::Security,
            _ => Kind::Other,
        },
        Status::Sl(e) => match e {
            SlStatus::INVALID_STATE | SlStatus::NOT_READY => Kind::InvalidState,
            SlStatus::INVALID_PARAMETER | SlStatus::INVALID_INDEX => Kind::InvalidParameter,
            SlStatus::FULL => Kind::TableFull,
            SlStatus::NOT_FOUND => Kind::NotFound,
            SlStatus::INVALID_KEY => Kind::Security,
            _ => Kind::Other,
        },
        Status::Ezsp(_) => Kind::Other,
    }
}

/// The [`CoordError`] of a non-success status of frame `frame_id`. Buffer
/// and slot exhaustion is the plain [`CoordError::Busy`] (counted as busy
/// by the application); everything else is a [`CoordError::Backend`]
/// with its [`Kind`], worded with the status name.
pub fn ezsp_error(frame_id: u16, s: Status) -> CoordError {
    if is_busy(s) {
        return CoordError::Busy;
    }
    let unsupported = matches!(
        s,
        Status::Sl(SlStatus::NOT_SUPPORTED | SlStatus::NOT_AVAILABLE)
    );
    let error = if unsupported {
        CoordError::Unsupported
    } else {
        CoordError::Backend {
            code: code(s, None),
            kind: kind(s),
        }
    };
    CoordError::described(error, format!("EZSP frame 0x{frame_id:04X}: {s:?}"))
}

/// The [`CoordError`] of a host failure, worded as the host words it.
pub fn host_error(e: &HostError) -> CoordError {
    let text = e.to_string();
    match e {
        HostError::Timeout { .. } => CoordError::Timeout,
        HostError::Status { frame_id, status } => ezsp_error(*frame_id, *status),
        HostError::Link(_)
        | HostError::ResetTimeout
        | HostError::NotStarted
        | HostError::Io(_)
        | HostError::Version(_) => CoordError::described(CoordError::LinkDown, text),
        HostError::InvalidCommand { reason, .. } => CoordError::described(
            CoordError::Backend {
                code: Code {
                    backend: BackendKind::Ezsp,
                    value: u16::from(reason.0),
                    text: None,
                },
                kind: Kind::InvalidParameter,
            },
            text,
        ),
        _ => CoordError::Other(text.into()),
    }
}

/// Whether `e` means the link to the NCP is gone (the session ends).
pub(crate) fn is_link_error(e: &HostError) -> bool {
    matches!(
        e,
        HostError::Link(_) | HostError::NotStarted | HostError::Io(_) | HostError::ResetTimeout
    )
}

/// The terminal of a `messageSentHandler` (design §5): success is
/// `Delivered` only for a unicast that asked for an APS ACK (`acked`), and
/// `Sent` otherwise (broadcast, multicast, unacknowledged unicast: the
/// stack only reports the first hop or the submission, UG100 §8). A failed
/// unicast is `NotDelivered(NoAck)`: EmberZNet reports `DELIVERY_FAILED`
/// for a missing APS ACK and for a route failure alike.
pub(crate) fn sent_terminal(s: Status, unicast: bool, acked: bool) -> Terminal {
    if s.is_success() {
        return if unicast && acked {
            Terminal::Delivered
        } else {
            Terminal::Sent
        };
    }
    let no_ack = s.kind() == StatusKind::DeliveryFailed
        || matches!(s, Status::Sl(SlStatus::ZIGBEE_NO_APS_ACK));
    if unicast && no_ack {
        Terminal::NotDelivered(CoordError::NoAck(code(
            s,
            Some("no APS ACK or no route (DELIVERY_FAILED)"),
        )))
    } else {
        Terminal::NotDelivered(ezsp_error(
            crate::ezsp::command::id::MESSAGE_SENT_HANDLER,
            s,
        ))
    }
}

/// An `incomingMessageHandler` as an [`ApsIndication`]; loopbacks of the
/// coordinator's own broadcasts and multicasts are dropped. The sender's
/// IEEE is known when the frame carried it (APS option `SOURCE_EUI64`,
/// EZSP ≥ 14 packet info).
pub(crate) fn indication(
    kind: IncomingMessageType,
    aps: &ApsFrame,
    info: &RxPacketInfo,
    message: &[u8],
) -> Option<ApsIndication> {
    if matches!(
        kind,
        IncomingMessageType::BROADCAST_LOOPBACK | IncomingMessageType::MULTICAST_LOOPBACK
    ) {
        return None;
    }
    Some(ApsIndication {
        src_nwk: info.sender,
        src_ieee: info
            .sender_eui64
            .filter(|_| aps.options.contains(ApsOptions::SOURCE_EUI64))
            .map(Eui64::to_u64),
        src_ep: aps.source_endpoint,
        dst_ep: aps.destination_endpoint,
        group: (kind == IncomingMessageType::MULTICAST).then_some(aps.group_id),
        profile: aps.profile_id,
        cluster: aps.cluster_id,
        payload: message.to_vec(),
        lqi: info.last_hop_lqi,
        rssi: info.last_hop_rssi,
    })
}

/// Energy (0–255, 802.15.4 ED scale) of a channel's maximum RSSI: linear
/// from −100 dBm (0) to −40 dBm (255). EmberZNet reports the RSSI only.
pub fn energy_of_rssi(rssi: i8) -> u8 {
    let above = (i32::from(rssi) + 100).clamp(0, 60);
    (above * 255 / 60) as u8
}

/// MAC capability of a child of the coordinator: address allocated, and
/// receiver on when idle unless sleepy.
pub(crate) fn child_capability(t: NodeType) -> u8 {
    if t == NodeType::SLEEPY_END_DEVICE {
        0x80
    } else {
        0x88
    }
}

/// The configuration values the capabilities are read from.
pub(crate) const CAPS_CONFIG: &[ConfigId] = &[
    ConfigId::APS_UNICAST_MESSAGE_COUNT,
    ConfigId::MAX_END_DEVICE_CHILDREN,
    ConfigId::ADDRESS_TABLE_SIZE,
    ConfigId::SOURCE_ROUTE_TABLE_SIZE,
    ConfigId::MULTICAST_TABLE_SIZE,
    ConfigId::NEIGHBOR_TABLE_SIZE,
    ConfigId::ROUTE_TABLE_SIZE,
    ConfigId::BINDING_TABLE_SIZE,
    ConfigId::KEY_TABLE_SIZE,
];

/// The [`Capabilities`] of a session from the configuration values the
/// NCP reported (`config`) and its `maximumPayloadLength`. Green Power,
/// backup and NCP firmware update stay off until their follow-ups.
pub fn capabilities(config: &BTreeMap<u8, u16>, max_payload: Option<u8>) -> Capabilities {
    let get = |id: ConfigId| config.get(&id.0).copied();
    Capabilities {
        backend: BackendKind::Ezsp,
        ownership: OwnershipModel::NcpNv,
        max_in_flight: get(ConfigId::APS_UNICAST_MESSAGE_COUNT)
            .unwrap_or(10)
            .max(1),
        // UG100 §8 maximumPayloadLength: the APS payload that fits one
        // frame now (security and source routing included).
        max_asdu: max_payload.map_or(82, u16::from),
        fragmentation: false,
        group_membership: true,
        tables: Tables {
            children: get(ConfigId::MAX_END_DEVICE_CHILDREN),
            address_map: get(ConfigId::ADDRESS_TABLE_SIZE),
            source_routes: get(ConfigId::SOURCE_ROUTE_TABLE_SIZE),
            multicast: get(ConfigId::MULTICAST_TABLE_SIZE),
            neighbors: get(ConfigId::NEIGHBOR_TABLE_SIZE),
            routes: get(ConfigId::ROUTE_TABLE_SIZE),
            bindings: get(ConfigId::BINDING_TABLE_SIZE),
            link_keys: get(ConfigId::KEY_TABLE_SIZE),
        },
        gp: GpSupport::default(),
        coex: false,
        radio: RadioOps {
            energy_scan: true,
            active_scan: true,
            channel: true,
            tx_power: true,
        },
        backup: BackupSupport::default(),
        fw_update: None,
        diagnostics: true,
    }
}

/// `EmberCounterType` names and diagnostic groups, by index (Gecko SDK
/// 4.4.6 `ember-types.h`: 41 counters; Simplicity SDK 2025.6.3 adds
/// `CSL_RX_SCHEDULE_FAILED` as the 42nd).
pub const COUNTERS: &[(&str, &str)] = &[
    ("mac_rx_broadcast", "mac"),
    ("mac_tx_broadcast", "mac"),
    ("mac_rx_unicast", "mac"),
    ("mac_tx_unicast_success", "mac"),
    ("mac_tx_unicast_retry", "mac"),
    ("mac_tx_unicast_failed", "mac"),
    ("aps_data_rx_broadcast", "aps"),
    ("aps_data_tx_broadcast", "aps"),
    ("aps_data_rx_unicast", "aps"),
    ("aps_data_tx_unicast_success", "aps"),
    ("aps_data_tx_unicast_retry", "aps"),
    ("aps_data_tx_unicast_failed", "aps"),
    ("route_discovery_initiated", "nwk"),
    ("neighbor_added", "tables"),
    ("neighbor_removed", "tables"),
    ("neighbor_stale", "tables"),
    ("join_indication", "nwk"),
    ("child_removed", "tables"),
    ("ash_overflow_error", "link"),
    ("ash_framing_error", "link"),
    ("ash_overrun_error", "link"),
    ("nwk_frame_counter_failure", "security"),
    ("aps_frame_counter_failure", "security"),
    ("ash_xoff", "link"),
    ("aps_link_key_not_authorized", "security"),
    ("nwk_decryption_failure", "security"),
    ("aps_decryption_failure", "security"),
    ("allocate_packet_buffer_failure", "queues"),
    ("relayed_unicast", "nwk"),
    ("phy_to_mac_queue_limit_reached", "queues"),
    ("packet_validate_library_dropped_count", "nwk"),
    ("nwk_retry_overflow", "nwk"),
    ("phy_cca_fail_count", "mac"),
    ("broadcast_table_full", "queues"),
    ("pta_lo_pri_requested", "mac"),
    ("pta_hi_pri_requested", "mac"),
    ("pta_lo_pri_denied", "mac"),
    ("pta_hi_pri_denied", "mac"),
    ("pta_lo_pri_tx_aborted", "mac"),
    ("pta_hi_pri_tx_aborted", "mac"),
    ("address_conflict_sent", "nwk"),
    ("csl_rx_schedule_failed", "mac"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_neutral_errors() {
        let busy = [
            Status::Ember(EmberStatus::MAX_MESSAGE_LIMIT_REACHED),
            Status::Ember(EmberStatus::NO_BUFFERS),
            Status::Ember(EmberStatus::NETWORK_BUSY),
            Status::Sl(SlStatus::ZIGBEE_MAX_MESSAGE_LIMIT_REACHED),
            Status::Sl(SlStatus::ALLOCATION_FAILED),
        ];
        for s in busy {
            assert_eq!(ezsp_error(0x34, s), CoordError::Busy, "{s:?}");
        }
        let kind_of = |s| match ezsp_error(0x34, s).meaning() {
            CoordError::Backend { kind, code } => {
                assert_eq!(code.backend, BackendKind::Ezsp);
                *kind
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(
            kind_of(Status::Ember(EmberStatus::MESSAGE_TOO_LONG)),
            Kind::TooLarge
        );
        assert_eq!(
            kind_of(Status::Sl(SlStatus::MESSAGE_TOO_LONG)),
            Kind::TooLarge
        );
        assert_eq!(
            kind_of(Status::Ember(EmberStatus::NETWORK_DOWN)),
            Kind::InvalidState
        );
        assert_eq!(
            kind_of(Status::Ember(EmberStatus::TABLE_FULL)),
            Kind::TableFull
        );
        assert_eq!(
            kind_of(Status::Sl(SlStatus::INVALID_INDEX)),
            Kind::InvalidParameter
        );
        assert_eq!(
            *ezsp_error(0x114, Status::Sl(SlStatus::NOT_SUPPORTED)).meaning(),
            CoordError::Unsupported
        );
        let e = ezsp_error(0x34, Status::Ember(EmberStatus::NETWORK_DOWN));
        assert_eq!(
            e.to_string(),
            "EZSP frame 0x0034: EmberStatus::NETWORK_DOWN"
        );
    }

    #[test]
    fn message_sent_outcomes() {
        let ok13 = Status::Ember(EmberStatus::SUCCESS);
        let ok18 = Status::Sl(SlStatus::OK);
        assert_eq!(sent_terminal(ok13, true, true), Terminal::Delivered);
        assert_eq!(sent_terminal(ok18, true, true), Terminal::Delivered);
        assert_eq!(
            sent_terminal(ok13, true, false),
            Terminal::Sent,
            "no APS ACK asked"
        );
        assert_eq!(
            sent_terminal(ok18, false, false),
            Terminal::Sent,
            "broadcast"
        );
        for failed in [
            Status::Ember(EmberStatus::DELIVERY_FAILED),
            Status::Sl(SlStatus::ZIGBEE_DELIVERY_FAILED),
            Status::Sl(SlStatus::ZIGBEE_NO_APS_ACK),
        ] {
            assert!(
                matches!(
                    sent_terminal(failed, true, true),
                    Terminal::NotDelivered(CoordError::NoAck(_))
                ),
                "{failed:?}"
            );
        }
        assert!(matches!(
            sent_terminal(Status::Ember(EmberStatus::DELIVERY_FAILED), false, false),
            Terminal::NotDelivered(CoordError::Described(_))
        ));
    }

    #[test]
    fn indications_drop_loopbacks_and_take_the_group() {
        let aps = ApsFrame {
            profile_id: 0x0104,
            cluster_id: 6,
            source_endpoint: 1,
            destination_endpoint: 1,
            options: ApsOptions::NONE,
            group_id: 0x1001,
            sequence: 0,
        };
        let info = RxPacketInfo {
            sender: 0x1234,
            sender_eui64: Some(Eui64::from_u64(7)),
            last_hop_lqi: 200,
            last_hop_rssi: -40,
            ..RxPacketInfo::default()
        };
        let m = indication(IncomingMessageType::MULTICAST, &aps, &info, &[1]).unwrap();
        assert_eq!(
            (m.group, m.src_ieee, m.lqi, m.rssi),
            (Some(0x1001), None, 200, -40)
        );
        let with_eui = ApsFrame {
            options: ApsOptions::SOURCE_EUI64,
            ..aps
        };
        let u = indication(IncomingMessageType::UNICAST, &with_eui, &info, &[1]).unwrap();
        assert_eq!((u.group, u.src_ieee), (None, Some(7)));
        assert!(indication(IncomingMessageType::BROADCAST_LOOPBACK, &aps, &info, &[]).is_none());
        assert!(indication(IncomingMessageType::MULTICAST_LOOPBACK, &aps, &info, &[]).is_none());
    }

    #[test]
    fn energy_scale_and_counter_names() {
        assert_eq!(energy_of_rssi(-100), 0);
        assert_eq!(energy_of_rssi(-128), 0);
        assert_eq!(energy_of_rssi(-70), 127);
        assert_eq!(energy_of_rssi(-40), 255);
        assert_eq!(energy_of_rssi(10), 255);
        assert_eq!(COUNTERS.len(), 42);
        assert_eq!(COUNTERS[23].0, "ash_xoff");
        assert_eq!(COUNTERS[40].0, "address_conflict_sent");
    }
}
