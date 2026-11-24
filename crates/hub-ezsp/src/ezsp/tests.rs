use super::command::{MulticastExtra, NULL_NODE_ID, id};
use super::frame::{self, CallbackType, HeaderError, ResponseFlags};
use super::version::{MAX_VERIFIED_VERSION, StackVersion};
use super::*;

const VERIFIED: [u8; 5] = [13, 14, 16, 17, 18];

fn v(n: u8) -> EzspVersion {
    EzspVersion::new(n)
}

fn hex(s: &str) -> Vec<u8> {
    s.split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect()
}

fn aps() -> ApsFrame {
    ApsFrame {
        profile_id: 0x0104,
        cluster_id: 0x0006,
        source_endpoint: 1,
        destination_endpoint: 1,
        options: ApsOptions::RETRY | ApsOptions::ENABLE_ROUTE_DISCOVERY,
        group_id: 0,
        sequence: 0x42,
    }
}

fn tag_for(ver: EzspVersion) -> u16 {
    if ver.wide_message_tag() { 0xABCD } else { 0xAB }
}

fn ok(ver: EzspVersion, f: StatusFamily) -> Status {
    Status::for_version(ver, f, StatusKind::Success).unwrap()
}

fn commands(ver: EzspVersion) -> Vec<Command> {
    let tag = tag_for(ver);
    vec![
        Command::Version { desired: 13 },
        Command::GetConfigurationValue {
            id: ConfigId::STACK_PROFILE,
        },
        Command::SetConfigurationValue {
            id: ConfigId::APS_UNICAST_MESSAGE_COUNT,
            value: 0x1234,
        },
        Command::GetValue {
            id: ValueId::VERSION_INFO,
        },
        Command::SetValue {
            id: ValueId::MAXIMUM_INCOMING_TRANSFER_SIZE,
            value: vec![0x52, 0x00],
        },
        Command::AddEndpoint(Endpoint {
            endpoint: 1,
            profile_id: 0x0104,
            device_id: 0x0005,
            app_flags: 0,
            input_clusters: vec![0x0000, 0x0006],
            output_clusters: vec![0x0019],
        }),
        Command::NetworkInit {
            bitmask: NetworkInitBitmask::PARENT_INFO_IN_TOKEN,
        },
        Command::NetworkState,
        Command::GetNetworkParameters,
        Command::GetEui64,
        Command::GetNodeId,
        Command::PermitJoining { duration: 0xFE },
        Command::SendUnicast {
            kind: OutgoingMessageType::DIRECT,
            index_or_destination: 0x1234,
            aps: aps(),
            tag,
            message: vec![1, 2, 3],
        },
        Command::SendBroadcast {
            destination: 0xFFFD,
            extra: BroadcastExtra::none(ver),
            aps: aps(),
            radius: 0,
            tag,
            message: vec![9],
        },
        Command::SendMulticast {
            aps: aps(),
            hops: 0,
            extra: MulticastExtra::none(ver, 7),
            tag,
            message: vec![],
        },
        Command::SetMulticastTableEntry {
            index: 2,
            entry: MulticastTableEntry {
                multicast_id: 0x1001,
                endpoint: 1,
            },
        },
        Command::GetMulticastTableEntry { index: 2 },
        Command::MaximumPayloadLength,
        Command::Nop,
    ]
}

#[test]
fn commands_round_trip_for_every_verified_version() {
    for n in VERIFIED {
        for cmd in commands(v(n)) {
            let params = cmd.encode_params(v(n)).unwrap();
            assert_eq!(
                Command::decode(cmd.frame_id(), v(n), &params),
                Ok(Some(cmd.clone())),
                "EZSP {n} {cmd:?}"
            );
        }
    }
    assert_eq!(Command::decode(0x7777, v(18), &[]), Ok(None));
}

#[test]
fn version_gated_layouts_match_the_sdk_byte_order() {
    let aps_bytes = "04 01 06 00 01 01 40 01 00 00 42";
    let unicast = |ver| {
        commands(v(ver))
            .into_iter()
            .find(|c| c.frame_id() == id::SEND_UNICAST)
            .unwrap()
    };
    assert_eq!(
        unicast(13).encode_params(v(13)).unwrap(),
        hex(&format!("00 34 12 {aps_bytes} AB 03 01 02 03"))
    );
    assert_eq!(
        unicast(14).encode_params(v(14)).unwrap(),
        hex(&format!("00 34 12 {aps_bytes} CD AB 03 01 02 03"))
    );

    let bcast = |ver| {
        commands(v(ver))
            .into_iter()
            .find(|c| c.frame_id() == id::SEND_BROADCAST)
            .unwrap()
    };
    assert_eq!(
        bcast(13).encode_params(v(13)).unwrap(),
        hex(&format!("FD FF {aps_bytes} 00 AB 01 09"))
    );
    assert_eq!(
        bcast(18).encode_params(v(18)).unwrap(),
        hex(&format!("FF FF FD FF 00 {aps_bytes} 00 CD AB 01 09"))
    );

    let mcast = |ver| {
        commands(v(ver))
            .into_iter()
            .find(|c| c.frame_id() == id::SEND_MULTICAST)
            .unwrap()
    };
    assert_eq!(
        mcast(13).encode_params(v(13)).unwrap(),
        hex(&format!("{aps_bytes} 00 07 AB 00"))
    );
    assert_eq!(
        mcast(16).encode_params(v(16)).unwrap(),
        hex(&format!("{aps_bytes} 00 FD FF FF FF 00 CD AB 00"))
    );

    let sent = |ver: u8, status| Callback::MessageSent {
        status,
        kind: OutgoingMessageType::DIRECT,
        index_or_destination: 0x1234,
        aps: aps(),
        tag: tag_for(v(ver)),
        message: vec![],
    };
    let s13 = Status::Ember(EmberStatus::DELIVERY_FAILED);
    let s18 = Status::Sl(SlStatus::ZIGBEE_DELIVERY_FAILED);
    assert_eq!(
        sent(13, s13).encode_params(v(13)).unwrap(),
        hex(&format!("00 34 12 {aps_bytes} AB 66 00"))
    );
    assert_eq!(
        sent(18, s18).encode_params(v(18)).unwrap(),
        hex(&format!("02 0C 00 00 00 34 12 {aps_bytes} CD AB 00"))
    );

    let info13 = RxPacketInfo {
        sender: 0x5678,
        binding_index: 0xFF,
        address_index: 0xFE,
        last_hop_lqi: 200,
        last_hop_rssi: -50,
        ..Default::default()
    };
    let info18 = RxPacketInfo {
        sender_eui64: Some(Eui64::from_u64(0x0011_2233_4455_6677)),
        last_hop_timestamp: Some(0x0102_0304),
        ..info13
    };
    let incoming = |info| Callback::IncomingMessage {
        kind: IncomingMessageType::UNICAST,
        aps: aps(),
        info,
        message: vec![0xAA],
    };
    assert_eq!(
        incoming(info13).encode_params(v(13)).unwrap(),
        hex(&format!("00 {aps_bytes} C8 CE 78 56 FF FE 01 AA"))
    );
    assert_eq!(
        incoming(info18).encode_params(v(18)).unwrap(),
        hex(&format!(
            "00 {aps_bytes} 78 56 77 66 55 44 33 22 11 00 FF FE C8 CE 04 03 02 01 01 AA"
        ))
    );
    assert_eq!(
        Callback::decode(
            id::INCOMING_MESSAGE_HANDLER,
            v(13),
            &incoming(info13).encode_params(v(13)).unwrap()
        ),
        Ok(incoming(info13))
    );

    // Statuses: u8 in 13, u32 from 14.
    let r = Response::NetworkInit {
        status: Status::Ember(EmberStatus::NOT_JOINED),
    };
    assert_eq!(r.encode_params(v(13)).unwrap(), vec![0x93]);
    let r = Response::NetworkInit {
        status: Status::Sl(SlStatus::NOT_JOINED),
    };
    assert_eq!(r.encode_params(v(14)).unwrap(), vec![0x17, 0, 0, 0]);
}

#[test]
fn responses_round_trip_for_every_verified_version() {
    use StatusFamily::{Ember, Ezsp};
    for n in VERIFIED {
        let ver = v(n);
        let params = NetworkParameters {
            extended_pan_id: [1, 2, 3, 4, 5, 6, 7, 8],
            pan_id: 0x1A62,
            radio_tx_power: -3,
            radio_channel: 25,
            join_method: JoinMethod::USE_MAC_ASSOCIATION,
            nwk_manager_id: 0,
            nwk_update_id: 4,
            channels: 0x07FF_F800,
        };
        let responses = vec![
            Response::Version {
                protocol_version: n,
                stack_type: 2,
                stack_version: StackVersion(0x7450),
            },
            Response::GetConfigurationValue {
                status: ok(ver, Ezsp),
                value: 7,
            },
            Response::SetConfigurationValue {
                status: Status::for_version(ver, Ezsp, StatusKind::InvalidCall).unwrap(),
            },
            Response::GetValue {
                status: ok(ver, Ezsp),
                value: vec![1, 2, 3],
            },
            Response::SetValue {
                status: ok(ver, Ezsp),
            },
            Response::AddEndpoint {
                status: ok(ver, Ezsp),
            },
            Response::NetworkInit {
                status: Status::for_version(ver, Ember, StatusKind::NotJoined).unwrap(),
            },
            Response::NetworkState {
                state: NetworkStatus::JOINED_NETWORK,
            },
            Response::GetNetworkParameters {
                status: ok(ver, Ember),
                node_type: NodeType::COORDINATOR,
                parameters: params,
            },
            Response::GetEui64 {
                eui64: Eui64::from_u64(0x000B_57FF_FE12_3456),
            },
            Response::GetNodeId { node_id: 0 },
            Response::PermitJoining {
                status: ok(ver, Ember),
            },
            Response::SendUnicast {
                status: ok(ver, Ember),
                sequence: 9,
            },
            Response::SendBroadcast {
                status: Status::for_version(ver, Ember, StatusKind::MaxMessageLimitReached)
                    .unwrap(),
                sequence: 0,
            },
            Response::SendMulticast {
                status: ok(ver, Ember),
                sequence: 10,
            },
            Response::SetMulticastTableEntry {
                status: ok(ver, Ember),
            },
            Response::GetMulticastTableEntry {
                status: ok(ver, Ember),
                entry: MulticastTableEntry {
                    multicast_id: 5,
                    endpoint: 1,
                },
            },
            Response::MaximumPayloadLength { aps_length: 82 },
            Response::Nop,
            Response::InvalidCommand {
                reason: EzspStatus::ERROR_INVALID_FRAME_ID,
            },
        ];
        for r in responses {
            let bytes = r.encode_params(ver).unwrap();
            assert_eq!(
                Response::decode(r.frame_id(), ver, &bytes),
                Ok(r.clone()),
                "EZSP {n} {r:?}"
            );
        }
    }
}

#[test]
fn callbacks_round_trip_for_every_verified_version() {
    for n in VERIFIED {
        let ver = v(n);
        let wide = ver.alias_fields();
        let callbacks = vec![
            Callback::StackStatus {
                status: Status::for_version(ver, StatusFamily::Ember, StatusKind::NetworkUp)
                    .unwrap(),
            },
            Callback::MessageSent {
                status: ok(ver, StatusFamily::Ember),
                kind: OutgoingMessageType::BROADCAST,
                index_or_destination: 0xFFFC,
                aps: aps(),
                tag: tag_for(ver),
                message: vec![1, 2],
            },
            Callback::IncomingMessage {
                kind: IncomingMessageType::MULTICAST,
                aps: ApsFrame {
                    group_id: 0x1001,
                    ..aps()
                },
                info: RxPacketInfo {
                    sender: 0x4321,
                    sender_eui64: wide.then_some(Eui64::from_u64(1)),
                    binding_index: 0xFF,
                    address_index: 0xFF,
                    last_hop_lqi: 255,
                    last_hop_rssi: -90,
                    last_hop_timestamp: wide.then_some(77),
                },
                message: (0..82).collect(),
            },
            Callback::TrustCenterJoin {
                new_node_id: 0x4321,
                new_node_eui64: Eui64::from_u64(0x0015_8D00_0012_3456),
                status: DeviceUpdate::STANDARD_SECURITY_UNSECURED_JOIN,
                policy_decision: JoinDecision::USE_PRECONFIGURED_KEY,
                parent_of_new_node_id: 0,
            },
            Callback::ChildJoin {
                index: 0,
                joining: true,
                child_id: 0x4321,
                child_eui64: Eui64::from_u64(2),
                child_type: NodeType::SLEEPY_END_DEVICE,
            },
        ];
        for cb in callbacks {
            let bytes = cb.encode_params(ver).unwrap();
            assert_eq!(
                Callback::decode(cb.frame_id(), ver, &bytes),
                Ok(cb.clone()),
                "EZSP {n} {cb:?}"
            );
        }
    }
    assert_eq!(
        Callback::decode(0x00F0, v(18), &[1]),
        Ok(Callback::Unknown {
            frame_id: 0x00F0,
            params: vec![1]
        })
    );
}

fn coordinator_commands() -> Vec<Command> {
    vec![
        Command::SetPolicy {
            policy: PolicyId::TRUST_CENTER_POLICY,
            decision: tc_policy::ALLOW_JOINS | tc_policy::ALLOW_UNSECURED_REJOINS,
        },
        Command::SetInitialSecurityState(InitialSecurityState {
            bitmask: security_bitmask::HAVE_NETWORK_KEY,
            preconfigured_key: [1; 16],
            network_key: [2; 16],
            network_key_sequence_number: 3,
            preconfigured_trust_center_eui64: Eui64::from_u64(4),
        }),
        Command::FormNetwork(NetworkParameters {
            extended_pan_id: [9; 8],
            pan_id: 0x1A62,
            radio_tx_power: 8,
            radio_channel: 15,
            ..NetworkParameters::default()
        }),
        Command::LeaveNetwork { options: 0 },
        Command::StartScan {
            scan_type: ScanType::ACTIVE_SCAN,
            channel_mask: 0x07FF_F800,
            duration: 3,
        },
        Command::StopScan,
        Command::SetRadioPower { power: -4 },
        Command::GetNeighbor { index: 1 },
        Command::NeighborCount,
        Command::GetRouteTableEntry { index: 2 },
        Command::GetChildData { index: 3 },
        Command::ReadCounters,
        Command::ReadAndClearCounters,
        Command::ExportKey(SecManContext::network_key()),
        Command::GetNetworkKeyInfo,
        Command::ImportTransientKey {
            eui64: Eui64([0xFF; 8]),
            key: *b"ZigBeeAlliance09",
            flags: 0,
        },
        Command::ClearTransientLinkKeys,
        Command::SetConcentrator {
            on: true,
            concentrator_type: concentrator::HIGH_RAM,
            min_time: 10,
            max_time: 60,
            route_error_threshold: 3,
            delivery_failure_threshold: 1,
            max_hops: 0,
        },
        Command::EnergyScanRequest {
            target: 0xFFFF,
            channels: 1 << 20,
            duration: 0xFE,
            count: 0,
        },
        Command::GetRandomNumber,
        Command::GetMfgToken {
            token: MfgTokenId::BOARD_NAME,
        },
        Command::LookupNodeIdByEui64 {
            eui64: Eui64::from_u64(5),
        },
    ]
}

#[test]
fn coordinator_frames_round_trip_for_every_verified_version() {
    use StatusFamily::{Ember, Ezsp};
    for n in VERIFIED {
        let ver = v(n);
        for cmd in coordinator_commands() {
            let params = cmd.encode_params(ver).unwrap();
            assert_eq!(
                Command::decode(cmd.frame_id(), ver, &params),
                Ok(Some(cmd.clone())),
                "EZSP {n} {cmd:?}"
            );
        }
        let sl_ok = Status::Sl(SlStatus::OK);
        let wide = ver.simplicity_layout();
        let responses = vec![
            Response::SetPolicy {
                status: ok(ver, Ezsp),
            },
            Response::SetInitialSecurityState {
                status: ok(ver, Ember),
            },
            Response::FormNetwork {
                status: ok(ver, Ember),
            },
            Response::LeaveNetwork {
                status: ok(ver, Ember),
            },
            Response::StartScan { status: sl_ok },
            Response::StopScan {
                status: ok(ver, Ember),
            },
            Response::SetRadioPower {
                status: ok(ver, Ember),
            },
            Response::GetNeighbor {
                status: ok(ver, Ember),
                entry: NeighborTableEntry {
                    short_id: 0x1234,
                    average_lqi: 200,
                    in_cost: 1,
                    out_cost: 3,
                    age: 2,
                    long_id: Eui64::from_u64(7),
                },
            },
            Response::NeighborCount { count: 4 },
            Response::GetRouteTableEntry {
                status: ok(ver, Ember),
                entry: RouteTableEntry {
                    destination: 0x1234,
                    next_hop: 0x5678,
                    status: 0,
                    age: 1,
                    concentrator_type: 0,
                    route_record_state: 0,
                },
            },
            Response::GetChildData {
                status: ok(ver, Ember),
                data: ChildData {
                    eui64: Eui64::from_u64(8),
                    node_type: NodeType::SLEEPY_END_DEVICE,
                    id: 0x4321,
                    phy: 0,
                    power: 0,
                    timeout: 8,
                    remaining_timeout: 1000,
                },
            },
            Response::ReadCounters {
                values: (0..41).collect(),
            },
            Response::ReadAndClearCounters {
                values: vec![7; 42],
            },
            Response::ExportKey {
                status: sl_ok,
                key: [0x5A; 16],
                context: wide.then_some(SecManContext::network_key()),
            },
            Response::GetNetworkKeyInfo {
                status: sl_ok,
                info: NetworkKeyInfo {
                    network_key_set: true,
                    alternate_network_key_set: false,
                    network_key_sequence_number: 1,
                    alt_network_key_sequence_number: 0,
                    network_key_frame_counter: 0x0102_0304,
                },
            },
            Response::ImportTransientKey { status: sl_ok },
            Response::ClearTransientLinkKeys,
            Response::SetConcentrator {
                status: ok(ver, Ember),
            },
            Response::EnergyScanRequest {
                status: ok(ver, Ember),
            },
            Response::GetRandomNumber {
                status: ok(ver, Ember),
                value: 0xBEEF,
            },
            Response::GetMfgToken {
                data: b"SONOFF Dongle-M\0".to_vec(),
            },
            Response::LookupNodeIdByEui64 {
                status: wide.then_some(sl_ok),
                node_id: 0xFFFE,
            },
        ];
        for r in responses {
            let bytes = r.encode_params(ver).unwrap();
            assert_eq!(
                Response::decode(r.frame_id(), ver, &bytes),
                Ok(r.clone()),
                "EZSP {n} {r:?}"
            );
        }
        let callbacks = vec![
            Callback::EnergyScanResult {
                channel: 15,
                max_rssi: -71,
            },
            Callback::NetworkFound {
                network: ZigbeeNetwork {
                    channel: 20,
                    pan_id: 0x1234,
                    extended_pan_id: [1, 2, 3, 4, 5, 6, 7, 8],
                    allowing_join: true,
                    stack_profile: 2,
                    nwk_update_id: 0,
                },
                last_hop_lqi: 180,
                last_hop_rssi: -60,
            },
            Callback::ScanComplete {
                channel: 0,
                status: ok(ver, Ember),
            },
            Callback::IncomingRouteRecord {
                source: 0x1234,
                source_eui64: Eui64::from_u64(9),
                last_hop_lqi: 200,
                last_hop_rssi: -50,
                relays: vec![0x1111, 0x2222],
            },
        ];
        for cb in callbacks {
            let bytes = cb.encode_params(ver).unwrap();
            assert_eq!(
                Callback::decode(cb.frame_id(), ver, &bytes),
                Ok(cb.clone()),
                "EZSP {n} {cb:?}"
            );
        }
    }
}

/// Byte order of the coordinator frames whose layout changes between EZSP
/// 13 (Gecko SDK 4.4.6) and 14+ (Simplicity SDK 2024.6.0 / 2025.6.3).
#[test]
fn coordinator_frames_match_the_sdk_byte_order() {
    let leave = Command::LeaveNetwork { options: 0 };
    assert_eq!(leave.encode_params(v(13)).unwrap(), Vec::<u8>::new());
    assert_eq!(leave.encode_params(v(14)).unwrap(), vec![0x00]);
    assert!(matches!(
        Command::LeaveNetwork { options: 0x20 }.encode_params(v(13)),
        Err(EncodeError::WrongLayout(_))
    ));

    let transient = Command::ImportTransientKey {
        eui64: Eui64([0xFF; 8]),
        key: *b"ZigBeeAlliance09",
        flags: 0,
    };
    let zb09 = "5A 69 67 42 65 65 41 6C 6C 69 61 6E 63 65 30 39";
    assert_eq!(
        transient.encode_params(v(13)).unwrap(),
        hex(&format!("FF FF FF FF FF FF FF FF {zb09} 00"))
    );
    assert_eq!(
        transient.encode_params(v(18)).unwrap(),
        hex(&format!("FF FF FF FF FF FF FF FF {zb09}"))
    );

    // exportKey: the context of the network key (type 1), 18 bytes.
    let ctx = "01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00";
    assert_eq!(
        Command::ExportKey(SecManContext::network_key())
            .encode_params(v(13))
            .unwrap(),
        hex(ctx)
    );
    let key = "5A 5A 5A 5A 5A 5A 5A 5A 5A 5A 5A 5A 5A 5A 5A 5A";
    let export = |ver: u8| {
        Response::decode(
            id::EXPORT_KEY,
            v(ver),
            &hex(&if ver == 13 {
                format!("{key} 00 00 00 00")
            } else {
                format!("00 00 00 00 {key} {ctx}")
            }),
        )
        .unwrap()
    };
    for ver in [13, 14] {
        assert!(
            matches!(export(ver), Response::ExportKey { status, key, .. } if status.is_success() && key == [0x5A; 16]),
            "EZSP {ver}"
        );
    }

    // startScan answers sl_status_t already in EZSP 13; scanComplete's
    // status is an EmberStatus there.
    assert_eq!(
        Response::decode(id::START_SCAN, v(13), &hex("00 00 00 00")),
        Ok(Response::StartScan {
            status: Status::Sl(SlStatus::OK)
        })
    );
    assert_eq!(
        Callback::decode(id::SCAN_COMPLETE_HANDLER, v(13), &hex("0F 00")),
        Ok(Callback::ScanComplete {
            channel: 15,
            status: Status::Ember(EmberStatus::SUCCESS)
        })
    );
    assert_eq!(
        Callback::decode(id::SCAN_COMPLETE_HANDLER, v(14), &hex("0F 00 00 00 00")),
        Ok(Callback::ScanComplete {
            channel: 15,
            status: Status::Sl(SlStatus::OK)
        })
    );
    // lookupNodeIdByEui64: node ID only in 13, status first from 14.
    assert_eq!(
        Response::decode(id::LOOKUP_NODE_ID_BY_EUI64, v(13), &hex("34 12")),
        Ok(Response::LookupNodeIdByEui64 {
            status: None,
            node_id: 0x1234
        })
    );
    assert_eq!(
        Response::decode(
            id::LOOKUP_NODE_ID_BY_EUI64,
            v(16),
            &hex("00 00 00 00 34 12")
        ),
        Ok(Response::LookupNodeIdByEui64 {
            status: Some(Status::Sl(SlStatus::OK)),
            node_id: 0x1234
        })
    );
    // Initial security state: 43 bytes, bitmask first.
    let state = InitialSecurityState {
        bitmask: 0x1B84,
        ..InitialSecurityState::default()
    };
    let bytes = Command::SetInitialSecurityState(state)
        .encode_params(v(13))
        .unwrap();
    assert_eq!((bytes.len(), &bytes[..2]), (43, &[0x84, 0x1B][..]));
    // Counters: as many as the NCP sends (41 in 13, 42 in 18).
    match Response::decode(id::READ_COUNTERS, v(18), &[1; 84]).unwrap() {
        Response::ReadCounters { values } => assert_eq!(values.len(), 42),
        other => panic!("{other:?}"),
    }
}

#[test]
fn encode_and_decode_errors() {
    let unicast = Command::SendUnicast {
        kind: OutgoingMessageType::DIRECT,
        index_or_destination: 0,
        aps: aps(),
        tag: 0x100,
        message: vec![],
    };
    assert_eq!(
        unicast.encode_params(v(13)),
        Err(EncodeError::TagTooWide(0x100))
    );
    assert!(unicast.encode_params(v(14)).is_ok());
    let bcast = Command::SendBroadcast {
        destination: 0xFFFC,
        extra: BroadcastExtra::V14 {
            alias: NULL_NODE_ID,
            nwk_sequence: 0,
        },
        aps: aps(),
        radius: 0,
        tag: 1,
        message: vec![],
    };
    assert_eq!(
        bcast.encode_params(v(13)),
        Err(EncodeError::WrongLayout("sendBroadcast"))
    );
    let long = Command::SetValue {
        id: ValueId::VERSION_INFO,
        value: vec![0; 256],
    };
    assert_eq!(long.encode_params(v(18)), Err(EncodeError::TooLong(256)));
    let r = Response::NetworkInit {
        status: Status::Sl(SlStatus::OK),
    };
    assert_eq!(r.encode_params(v(13)), Err(EncodeError::StatusWidth));
    let r = Response::AddEndpoint {
        status: Status::Ember(EmberStatus::SUCCESS),
    };
    assert_eq!(
        r.encode_params(v(13)),
        Err(EncodeError::StatusWidth),
        "addEndpoint uses EzspStatus in EZSP 13"
    );

    assert_eq!(
        Response::decode(id::GET_NODE_ID, v(18), &[0x00]),
        Err(DecodeError::Truncated { at: 0 })
    );
    assert_eq!(
        Response::decode(id::NETWORK_INIT, v(14), &[0x00]),
        Err(DecodeError::Truncated { at: 0 }),
        "u32 status"
    );
    assert_eq!(
        Response::decode(id::NETWORK_INIT, v(13), &[0x00, 0x00]),
        Err(DecodeError::Trailing { extra: 1 })
    );
}

#[test]
fn status_kinds_are_version_independent() {
    use StatusKind as K;
    let kinds = [
        K::Success,
        K::NetworkUp,
        K::NetworkDown,
        K::NotJoined,
        K::NetworkOpened,
        K::NetworkClosed,
        K::DeliveryFailed,
        K::MaxMessageLimitReached,
        K::MessageTooLong,
        K::InvalidCall,
    ];
    for n in VERIFIED {
        for k in kinds {
            let s = Status::for_version(v(n), StatusFamily::Ember, k).unwrap();
            assert!(s.matches(v(n), StatusFamily::Ember));
            let expected = if n >= 14 && k == K::InvalidCall {
                K::EzspError
            } else {
                k
            };
            assert_eq!(s.kind(), expected, "EZSP {n} {k:?}");
        }
    }
    assert_eq!(Status::Ember(EmberStatus::NETWORK_UP).kind(), K::NetworkUp);
    assert_eq!(Status::Sl(SlStatus(0x0015)).kind(), K::NetworkUp);
    assert_eq!(Status::Sl(SlStatus(0x0C1E)).kind(), K::EzspError);
    assert_eq!(
        Status::Ezsp(EzspStatus::ERROR_INVALID_ID).kind(),
        K::InvalidId
    );
    assert_eq!(Status::Sl(SlStatus(0x1234)).kind(), K::Other);
    assert_eq!(
        format!("{:?}", Status::Sl(SlStatus::ZIGBEE_DELIVERY_FAILED)),
        "SlStatus::ZIGBEE_DELIVERY_FAILED"
    );
    assert_eq!(format!("{:?}", EmberStatus(0x42)), "EmberStatus(0x42)");
}

#[test]
fn version_policy() {
    let p = VersionPolicy::default();
    assert_eq!(p.check(12), Err(VersionError::TooOld { ncp: 12 }));
    assert_eq!(p.check(8), Err(VersionError::TooOld { ncp: 8 }));
    for n in VERIFIED {
        assert_eq!(p.check(n), Ok(v(n)));
    }
    assert_eq!(p.check(15), Err(VersionError::Unverified { ncp: 15 }));
    assert_eq!(
        p.check(MAX_VERIFIED_VERSION + 1),
        Err(VersionError::Unverified { ncp: 19 })
    );
    let open = VersionPolicy {
        accept_unverified: true,
    };
    assert_eq!(open.check(19), Ok(v(19)));
    assert_eq!(open.check(12), Err(VersionError::TooOld { ncp: 12 }));
    assert!(!v(13).uses_sl_status() && !v(13).wide_message_tag() && !v(13).alias_fields());
    assert!(v(14).uses_sl_status() && v(14).wide_message_tag() && v(14).alias_fields());
    assert_eq!(StackVersion(0x7450).to_string(), "7.4.5.0");
    assert_eq!(StackVersion(0x8231).to_string(), "8.2.3.1");
}

#[test]
fn frame_header_formats() {
    // UG101 §3 example payloads are legacy-format version frames.
    let legacy = hex("00 80 00 02 02 11 30");
    let (h, p) = frame::parse(&legacy).unwrap();
    assert!(h.is_legacy() && h.control.is_response());
    assert_eq!(
        (h.sequence, h.frame_id, p),
        (0, id::VERSION, &hex("02 02 11 30")[..])
    );
    assert_eq!(
        Response::decode(h.frame_id, v(13), p),
        Ok(Response::Version {
            protocol_version: 2,
            stack_type: 2,
            stack_version: StackVersion(0x3011),
        })
    );
    assert_eq!(frame::legacy_version_command(0, 2), hex("00 00 00 02"));
    assert_eq!(
        frame::command(7, id::GET_NETWORK_PARAMETERS, &[]),
        hex("07 00 01 28 00")
    );

    let callback = hex("07 B5 01 45 00 AA");
    let (h, p) = frame::parse(&callback).unwrap();
    assert_eq!((h.frame_id, p), (id::INCOMING_MESSAGE_HANDLER, &[0xAA][..]));
    let c = h.control;
    assert!(c.overflow() && !c.truncated() && c.callback_pending());
    assert_eq!(c.callback_type(), CallbackType::Async);
    assert_eq!(c.network_index(), 1);
    assert_eq!(
        frame::parse(&hex("07 01 01 05 00"))
            .unwrap()
            .0
            .control
            .sleep_mode(),
        1
    );

    assert_eq!(frame::parse(&hex("00 80")), Err(HeaderError::TooShort(2)));
    assert_eq!(
        frame::parse(&hex("00 80 01 05")),
        Err(HeaderError::TooShort(4))
    );
    assert_eq!(
        frame::parse(&hex("00 80 81 05 00")),
        Err(HeaderError::SecurityOrPadding { fc_high: 0x81 })
    );
    assert_eq!(
        frame::parse(&hex("00 80 41 05 00")),
        Err(HeaderError::SecurityOrPadding { fc_high: 0x41 })
    );
    assert_eq!(
        frame::parse(&hex("00 80 05 05 00")),
        Err(HeaderError::ReservedBits { fc_high: 0x05 })
    );
    assert_eq!(
        frame::parse(&hex("00 80 18 00")),
        Err(HeaderError::UnsupportedFormat { byte2: 0x18 })
    );
}

fn ncp_response(seq: u8, flags: ResponseFlags, r: &Response, ver: EzspVersion) -> Vec<u8> {
    frame::response(seq, flags, r.frame_id(), &r.encode_params(ver).unwrap())
}

#[test]
fn session_negotiates_with_a_legacy_first_frame() {
    let mut s = Session::new();
    assert_eq!(
        s.encode(&Command::NetworkState),
        Err(SessionError::VersionNotSet)
    );
    let (seq, bytes) = s.encode(&Command::Version { desired: 18 }).unwrap();
    assert_eq!(bytes, vec![0, 0x00, 0x00, 18], "legacy format");
    assert_eq!(s.encode(&Command::Nop), Err(SessionError::Busy));
    let legacy =
        frame::legacy_version_response(seq, ResponseFlags::default(), &[13, 2, 0x50, 0x74]);
    let Inbound::Response { response, .. } = s.on_frame(&legacy).unwrap() else {
        panic!()
    };
    assert_eq!(
        response,
        Response::Version {
            protocol_version: 13,
            stack_type: 2,
            stack_version: StackVersion(0x7450)
        }
    );

    let (seq, bytes) = s.encode(&Command::Version { desired: 13 }).unwrap();
    assert_eq!(
        bytes,
        vec![1, 0x00, 0x01, 0x00, 0x00, 13],
        "extended format afterwards"
    );
    let r = Response::Version {
        protocol_version: 13,
        stack_type: 2,
        stack_version: StackVersion(0x7450),
    };
    assert!(matches!(
        s.on_frame(&ncp_response(seq, ResponseFlags::default(), &r, v(13))),
        Ok(Inbound::Response { .. })
    ));
    s.set_version(v(13));

    let (seq, bytes) = s.encode(&Command::NetworkState).unwrap();
    assert_eq!(bytes, vec![2, 0x00, 0x01, 0x18, 0x00]);
    let resp = Response::NetworkState {
        state: NetworkStatus::JOINED_NETWORK,
    };
    assert_eq!(
        s.on_frame(&ncp_response(seq, ResponseFlags::default(), &resp, v(13))),
        Ok(Inbound::Response {
            sequence: 2,
            response: resp,
            flags: Flags::default()
        })
    );
    s.reset();
    let (_, bytes) = s.encode(&Command::Version { desired: 18 }).unwrap();
    assert_eq!(bytes.len(), 4, "legacy again after a reset");
}

#[test]
fn session_correlates_and_classifies() {
    let mut s = Session::new();
    s.set_version(v(18));
    let state = Response::NetworkState {
        state: NetworkStatus::NO_NETWORK,
    };

    // A callback between command and response does not complete it.
    let (seq, _) = s.encode(&Command::NetworkState).unwrap();
    let cb = Callback::StackStatus {
        status: Status::Sl(SlStatus::NETWORK_UP),
    };
    let cb_frame = frame::response(
        seq.wrapping_sub(1),
        ResponseFlags {
            async_callback: true,
            ..Default::default()
        },
        cb.frame_id(),
        &cb.encode_params(v(18)).unwrap(),
    );
    assert_eq!(
        s.on_frame(&cb_frame),
        Ok(Inbound::Callback {
            callback: cb,
            flags: Flags::default()
        })
    );
    assert_eq!(s.outstanding(), Some(seq));
    let wrong_seq = ncp_response(seq.wrapping_add(5), ResponseFlags::default(), &state, v(18));
    assert!(matches!(
        s.on_frame(&wrong_seq),
        Ok(Inbound::Unexpected { .. })
    ));
    let flags = ResponseFlags {
        overflow: true,
        ..Default::default()
    };
    let Ok(Inbound::Response { flags, .. }) = s.on_frame(&ncp_response(seq, flags, &state, v(18)))
    else {
        panic!()
    };
    assert!(flags.overflow, "overflow surfaced");

    // invalidCommand answers the outstanding command by sequence.
    let (seq, _) = s
        .encode(&Command::Raw {
            frame_id: 0x7777,
            params: vec![],
        })
        .unwrap();
    let invalid = Response::InvalidCommand {
        reason: EzspStatus::ERROR_INVALID_FRAME_ID,
    };
    assert_eq!(
        s.on_frame(&ncp_response(
            seq,
            ResponseFlags::default(),
            &invalid,
            v(18)
        )),
        Ok(Inbound::Response {
            sequence: seq,
            response: invalid,
            flags: Flags::default()
        })
    );

    // Raw commands get raw responses.
    let (seq, bytes) = s
        .encode(&Command::Raw {
            frame_id: 0x0081,
            params: vec![1, 0xAA],
        })
        .unwrap();
    assert_eq!(&bytes[3..], &[0x81, 0x00, 1, 0xAA]);
    let echo = frame::response(seq, ResponseFlags::default(), 0x0081, &[1, 0xAA]);
    assert!(matches!(
        s.on_frame(&echo),
        Ok(Inbound::Response {
            response: Response::Raw {
                frame_id: 0x0081,
                ..
            },
            ..
        })
    ));

    // Truncated responses are reported, not decoded.
    let (seq, _) = s.encode(&Command::NetworkState).unwrap();
    let truncated = ncp_response(
        seq,
        ResponseFlags {
            truncated: true,
            ..Default::default()
        },
        &state,
        v(18),
    );
    assert!(matches!(
        s.on_frame(&truncated),
        Ok(Inbound::Truncated {
            frame_id: id::NETWORK_STATE,
            ..
        })
    ));

    // A late response after a timeout is unexpected.
    let (seq, _) = s.encode(&Command::NetworkState).unwrap();
    assert_eq!(s.abandon(), Some(seq));
    assert!(matches!(
        s.on_frame(&ncp_response(seq, ResponseFlags::default(), &state, v(18))),
        Ok(Inbound::Unexpected { .. })
    ));

    // A response that does not decode for the version.
    let (seq, _) = s
        .encode(&Command::NetworkInit {
            bitmask: NetworkInitBitmask::NO_OPTIONS,
        })
        .unwrap();
    let narrow = frame::response(seq, ResponseFlags::default(), id::NETWORK_INIT, &[0x00]);
    assert!(matches!(
        s.on_frame(&narrow),
        Ok(Inbound::BadResponse { .. })
    ));

    assert_eq!(
        s.on_frame(&hex("00 00 01 05 00")),
        Err(SessionError::WrongDirection)
    );
    assert_eq!(
        s.on_frame(&hex("00 98 01 19 00 00 00 00 00")),
        Err(SessionError::ReservedCallbackType)
    );
}
