//! Host against the simulated NCP: negotiation, the core-stage scenario,
//! failures, flow control; all on the virtual clock.

use std::time::Duration;

use crate::ash::{FailReason, ResetCode};
use crate::ezsp::command::id;
use crate::ezsp::version::VersionError;
use crate::ezsp::*;
use crate::host::{EventReceiver, EzspHost, HostConfig, HostError, HostEvent};
use crate::sim::{Delivery, Fault, Faults, Sim, SimClock, SimConfig, SimTransport};

type Host = EzspHost<SimTransport, SimClock>;

fn setup(cfg: SimConfig, faults: Faults, host_cfg: HostConfig) -> (Sim, Host, EventReceiver) {
    let (sim, transport, clock) = Sim::new(cfg, faults);
    let (host, events) = EzspHost::new(transport, clock, host_cfg).unwrap();
    (sim, host, events)
}

fn sim_version(n: u8) -> SimConfig {
    SimConfig {
        ezsp_version: n,
        ..SimConfig::default()
    }
}

fn callbacks(events: &EventReceiver) -> Vec<Callback> {
    events
        .drain()
        .into_iter()
        .filter_map(|e| match e {
            HostEvent::Callback(c) => Some(c),
            _ => None,
        })
        .collect()
}

/// Pumps until `pred` holds for a received callback, collecting the others.
fn wait_callback(
    host: &mut Host,
    events: &EventReceiver,
    pred: impl Fn(&Callback) -> bool,
) -> Callback {
    for _ in 0..200 {
        host.pump(Duration::from_millis(100)).unwrap();
        while let Some(ev) = events.try_recv() {
            if let HostEvent::Callback(c) = ev
                && pred(&c)
            {
                return c;
            }
        }
    }
    panic!("callback not received");
}

#[test]
fn negotiation_accepts_13_through_18_with_one_or_two_exchanges() {
    for n in [13, 14, 16, 17, 18] {
        let (sim, mut host, _) = setup(sim_version(n), Faults::none(), HostConfig::default());
        let info = host.start().unwrap();
        assert_eq!(info.ezsp_version, EzspVersion::new(n));
        assert_eq!(
            info.version_exchanges,
            if n == 18 { 1 } else { 2 },
            "EZSP {n}"
        );
        assert_eq!(info.reset_code, ResetCode::SOFTWARE);
        assert_eq!(info.ash_version, 2);
        assert_eq!(sim.ncp(|ncp| ncp.version()), Some(EzspVersion::new(n)));
        let versions = sim.ncp(|ncp| ncp.commands().iter().filter(|&&f| f == id::VERSION).count());
        assert_eq!(versions, usize::from(info.version_exchanges));
        assert_eq!(host.network_state().unwrap(), NetworkStatus::NO_NETWORK);
    }
}

#[test]
fn negotiation_refuses_old_and_unverified_versions() {
    let (sim, mut host, _) = setup(sim_version(12), Faults::none(), HostConfig::default());
    match host.start() {
        Err(HostError::Version(VersionError::TooOld { ncp: 12 })) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(
        sim.ncp(|ncp| ncp.commands().to_vec()),
        vec![id::VERSION],
        "no second exchange"
    );
    assert!(matches!(host.network_state(), Err(HostError::NotStarted)));

    let (_, mut host, _) = setup(sim_version(15), Faults::none(), HostConfig::default());
    assert!(matches!(
        host.start(),
        Err(HostError::Version(VersionError::Unverified { ncp: 15 }))
    ));
    let cfg = HostConfig {
        policy: VersionPolicy {
            accept_unverified: true,
        },
        ..HostConfig::default()
    };
    let (_, mut host, _) = setup(sim_version(15), Faults::none(), cfg);
    assert_eq!(host.start().unwrap().ezsp_version, EzspVersion::new(15));
}

fn endpoint() -> Endpoint {
    Endpoint {
        endpoint: 1,
        profile_id: 0x0104,
        device_id: 0x0005,
        app_flags: 0,
        input_clusters: vec![0x0000, 0x0003, 0x0006, 0x0500],
        output_clusters: vec![0x0003, 0x0006, 0x0019],
    }
}

fn on_off_aps() -> ApsFrame {
    ApsFrame {
        profile_id: 0x0104,
        cluster_id: 0x0006,
        source_endpoint: 1,
        destination_endpoint: 1,
        options: ApsOptions::RETRY | ApsOptions::ENABLE_ROUTE_DISCOVERY,
        group_id: 0,
        sequence: 0,
    }
}

/// reset → version → config → addEndpoint → networkInit → stackStatus up
/// → networkState → parameters → permitJoining → sendUnicast →
/// messageSentHandler; injected incoming message and trust-center join.
fn core_scenario(version: u8, faults: Faults) {
    let ctx = format!("EZSP {version}, {faults:?}");
    let (sim, mut host, events) = setup(sim_version(version), faults, HostConfig::default());
    let info = host.start().unwrap_or_else(|e| panic!("{ctx}: {e}"));
    assert_eq!(info.ezsp_version.get(), version);
    assert_eq!(info.stack_version, StackVersion(0x8230));

    host.set_configuration_value(ConfigId::APS_UNICAST_MESSAGE_COUNT, 16)
        .unwrap();
    host.set_configuration_value(ConfigId::SOURCE_ROUTE_TABLE_SIZE, 32)
        .unwrap();
    assert_eq!(
        host.get_configuration_value(ConfigId::APS_UNICAST_MESSAGE_COUNT)
            .unwrap(),
        16
    );
    assert_eq!(
        sim.ncp(|n| n.configuration(ConfigId::SOURCE_ROUTE_TABLE_SIZE)),
        Some(32)
    );
    host.add_endpoint(endpoint()).unwrap();
    match host.add_endpoint(endpoint()) {
        Err(HostError::Status { status, .. }) => {
            let k = if version == 13 {
                StatusKind::InvalidValue
            } else {
                StatusKind::EzspError
            };
            assert_eq!(status.kind(), k, "{ctx}");
        }
        other => panic!("{ctx}: {other:?}"),
    }

    let status = host.network_init(NetworkInitBitmask::NO_OPTIONS).unwrap();
    assert!(status.is_success(), "{ctx}");
    let up = wait_callback(&mut host, &events, |c| {
        matches!(c, Callback::StackStatus { .. })
    });
    let Callback::StackStatus { status } = up else {
        unreachable!()
    };
    assert_eq!(status.kind(), StatusKind::NetworkUp, "{ctx}");
    assert!(
        matches!(status, Status::Sl(_)) == (version >= 14),
        "{ctx}: status width"
    );

    assert_eq!(host.network_state().unwrap(), NetworkStatus::JOINED_NETWORK);
    let (node_type, params) = host.get_network_parameters().unwrap();
    assert_eq!(
        (node_type, params),
        (NodeType::COORDINATOR, SimConfig::default().network)
    );
    assert_eq!(host.get_eui64().unwrap(), SimConfig::default().eui64);
    assert_eq!(host.get_node_id().unwrap(), 0x0000);
    assert_eq!(host.maximum_payload_length().unwrap(), 82);
    match host.set_configuration_value(ConfigId::MAX_HOPS, 10) {
        Err(HostError::Status { .. }) => {}
        other => {
            panic!("{ctx}: configuration must be refused once the network is up, got {other:?}")
        }
    }
    host.permit_joining(60).unwrap();

    let tag = if version == 13 { 0x5A } else { 0x5A5A };
    let zcl_toggle = [0x01, 0x10, 0x02];
    let seq = host
        .send_unicast(0x1234, on_off_aps(), tag, &zcl_toggle)
        .unwrap();
    let sent = wait_callback(&mut host, &events, |c| {
        matches!(c, Callback::MessageSent { .. })
    });
    let Callback::MessageSent {
        status,
        kind,
        index_or_destination,
        aps,
        tag: t,
        ..
    } = sent
    else {
        unreachable!()
    };
    assert_eq!(
        (status.kind(), kind, index_or_destination, t),
        (
            StatusKind::Success,
            OutgoingMessageType::DIRECT,
            0x1234,
            tag
        )
    );
    assert_eq!(aps.sequence, seq);
    let sent = sim.ncp(|n| n.sent().to_vec());
    assert_eq!(
        sent.len(),
        1,
        "{ctx}: sent exactly once despite retransmissions"
    );

    let incoming = sim.ncp(|n| {
        let cb = n.incoming_message(
            IncomingMessageType::UNICAST,
            on_off_aps(),
            0x1234,
            &[0x18, 0x01, 0x0B, 0x02, 0x00],
        );
        n.callback(cb.clone());
        n.callback(Callback::TrustCenterJoin {
            new_node_id: 0x9999,
            new_node_eui64: Eui64::from_u64(0x0015_8D00_0000_0001),
            status: DeviceUpdate::STANDARD_SECURITY_UNSECURED_JOIN,
            policy_decision: JoinDecision::USE_PRECONFIGURED_KEY,
            parent_of_new_node_id: 0x0000,
        });
        cb
    });
    let got = wait_callback(&mut host, &events, |c| {
        matches!(c, Callback::IncomingMessage { .. })
    });
    assert_eq!(got, incoming, "{ctx}");
    let Callback::IncomingMessage { info, .. } = got else {
        unreachable!()
    };
    assert_eq!(info.sender_eui64.is_some(), version >= 14);
    let tc = wait_callback(&mut host, &events, |c| {
        matches!(c, Callback::TrustCenterJoin { .. })
    });
    assert!(matches!(
        tc,
        Callback::TrustCenterJoin {
            new_node_id: 0x9999,
            ..
        }
    ));
    assert_eq!(host.stats().dropped_callbacks, 0);
    assert_eq!(host.stats().bad_frames, 0, "{ctx}");
    let (a, b) = sim.pipe_stats();
    if faults != Faults::none() {
        let injected = a.dropped
            + a.corrupted
            + a.duplicated
            + a.delayed
            + b.dropped
            + b.corrupted
            + b.duplicated
            + b.delayed;
        assert!(injected > 0, "{ctx}: no fault was injected");
    }
}

#[test]
fn core_scenario_without_faults() {
    for v in [13, 14, 16, 17, 18] {
        core_scenario(v, Faults::none());
    }
}

#[test]
fn core_scenario_under_fault_injection() {
    for (seed, v) in [
        (11, 13),
        (12, 18),
        (13, 14),
        (14, 16),
        (15, 17),
        (16, 13),
        (17, 18),
    ] {
        core_scenario(v, Faults::uniform(seed, 60, 1500));
    }
}

#[test]
fn faults_cause_retransmissions_not_duplicates() {
    let (sim, mut host, events) = setup(sim_version(18), Faults::none(), HostConfig::default());
    host.start().unwrap();
    host.network_init(NetworkInitBitmask::NO_OPTIONS).unwrap();
    callbacks(&events);
    // Lose the command, then the response, then corrupt and duplicate.
    for fault in [
        Fault::Drop,
        Fault::Corrupt,
        Fault::Duplicate,
        Fault::Delay(2500),
    ] {
        sim.inject_to_ncp(fault);
        host.send_unicast(0x1234, on_off_aps(), 1, &[1, 2, 3])
            .unwrap();
        sim.inject_to_host(fault);
        host.send_unicast(0x1234, on_off_aps(), 2, &[1, 2, 3])
            .unwrap();
    }
    host.pump(Duration::from_secs(5)).unwrap();
    assert_eq!(sim.ncp(|n| n.sent().len()), 8, "each command executed once");
    let sent: Vec<_> = callbacks(&events)
        .into_iter()
        .filter(|c| matches!(c, Callback::MessageSent { .. }))
        .collect();
    assert_eq!(sent.len(), 8);
    assert!(host.ash().stats().tx_retransmit > 0);
    assert!(host.ash().stats().rx_duplicate > 0 || host.ash().stats().rx_bad_crc > 0);
}

#[test]
fn delivery_failure_and_stored_network_absent() {
    let cfg = SimConfig {
        unicast_delivery: Delivery::Failed,
        ..sim_version(13)
    };
    let (_, mut host, events) = setup(cfg, Faults::none(), HostConfig::default());
    host.start().unwrap();
    host.network_init(NetworkInitBitmask::NO_OPTIONS).unwrap();
    host.send_unicast(0x2222, on_off_aps(), 7, &[0]).unwrap();
    let sent = wait_callback(&mut host, &events, |c| {
        matches!(c, Callback::MessageSent { .. })
    });
    let Callback::MessageSent { status, tag, .. } = sent else {
        unreachable!()
    };
    assert_eq!(
        (status, tag),
        (Status::Ember(EmberStatus::DELIVERY_FAILED), 7)
    );

    let cfg = SimConfig {
        network_formed: false,
        ..sim_version(18)
    };
    let (_, mut host, _) = setup(cfg, Faults::none(), HostConfig::default());
    host.start().unwrap();
    assert_eq!(
        host.network_init(NetworkInitBitmask::NO_OPTIONS).unwrap(),
        Status::Sl(SlStatus::NOT_JOINED)
    );
    match host.send_unicast(0x2222, on_off_aps(), 7, &[0]) {
        Err(HostError::Status { status, .. }) => assert_eq!(status.kind(), StatusKind::NetworkDown),
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(
            host.send_unicast(0x2222, on_off_aps(), 0x100, &[0]),
            Err(HostError::Status { .. })
        ),
        "16-bit tags encode in EZSP 18"
    );
}

#[test]
fn invalid_command_and_narrow_tag_are_typed_errors() {
    let (_, mut host, _) = setup(sim_version(13), Faults::none(), HostConfig::default());
    host.start().unwrap();
    match host.call(&Command::Raw {
        frame_id: 0x7777,
        params: vec![],
    }) {
        Err(HostError::InvalidCommand {
            frame_id: 0x7777,
            reason,
        }) => assert_eq!(reason, EzspStatus::ERROR_INVALID_FRAME_ID),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        host.send_unicast(0, on_off_aps(), 0x100, &[]),
        Err(HostError::Encode(EncodeError::TagTooWide(0x100)))
    ));
    assert_eq!(
        host.network_state().unwrap(),
        NetworkStatus::NO_NETWORK,
        "the session continues"
    );
}

#[test]
fn ncp_error_and_spontaneous_reset_are_reported_and_recoverable() {
    let (sim, mut host, events) = setup(sim_version(18), Faults::none(), HostConfig::default());
    host.start().unwrap();
    sim.ncp(|n| n.fail(ResetCode::ASSERT));
    // The NCP announces the FAILED state with an ERROR frame (UG101 §6).
    match host.network_state() {
        Err(HostError::Link(FailReason::NcpError(ResetCode::ASSERT))) => {}
        other => panic!("{other:?}"),
    }
    assert!(
        events
            .drain()
            .contains(&HostEvent::LinkLost(FailReason::NcpError(
                ResetCode::ASSERT
            )))
    );
    assert!(matches!(host.network_state(), Err(HostError::NotStarted)));
    host.start().unwrap();
    assert_eq!(host.network_state().unwrap(), NetworkStatus::NO_NETWORK);

    sim.ncp(|n| n.spontaneous_reset(ResetCode::WATCHDOG));
    match host.pump(Duration::from_millis(100)) {
        Err(HostError::Link(FailReason::NcpReset(ResetCode::WATCHDOG))) => {}
        other => panic!("{other:?}"),
    }
    let info = host.start().unwrap();
    assert_eq!(info.reset_code, ResetCode::SOFTWARE);
    assert_eq!(
        host.network_init(NetworkInitBitmask::NO_OPTIONS)
            .unwrap()
            .kind(),
        StatusKind::Success
    );
}

#[test]
fn reset_without_rstack_times_out() {
    let (sim, mut host, _) = setup(sim_version(18), Faults::none(), HostConfig::default());
    sim.set_faults(Faults {
        seed: 1,
        drop_permille: 1000,
        ..Faults::none()
    });
    assert!(matches!(host.start(), Err(HostError::ResetTimeout)));
    assert_eq!(sim.pipe_stats().0.dropped, 6, "one RST and five retries");
    sim.set_faults(Faults::none());
    host.start().unwrap();
}

#[test]
fn command_timeout_abandons_and_ignores_the_late_response() {
    let cfg = HostConfig {
        command_timeout: Duration::from_millis(300),
        ..HostConfig::default()
    };
    let (sim, mut host, _) = setup(sim_version(18), Faults::none(), cfg);
    host.start().unwrap();
    sim.inject_to_host(Fault::Delay(1000));
    assert!(matches!(
        host.network_state(),
        Err(HostError::Timeout {
            frame_id: id::NETWORK_STATE
        })
    ));
    host.pump(Duration::from_secs(2)).unwrap();
    assert_eq!(host.stats().unexpected_responses, 1);
    assert_eq!(host.network_state().unwrap(), NetworkStatus::NO_NETWORK);
}

#[test]
fn callback_flow_control_keeps_the_queue_bounded() {
    let cfg = HostConfig {
        event_queue: 12,
        event_not_ready_at: 4,
        ..HostConfig::default()
    };
    let (sim, mut host, events) = setup(sim_version(18), Faults::none(), cfg);
    host.start().unwrap();
    host.network_init(NetworkInitBitmask::NO_OPTIONS).unwrap();
    wait_callback(&mut host, &events, |c| {
        matches!(c, Callback::StackStatus { .. })
    });
    events.drain();
    const N: usize = 24;
    sim.ncp(|n| {
        for i in 0..N {
            let cb = n.incoming_message(
                IncomingMessageType::BROADCAST,
                on_off_aps(),
                i as u16,
                &[i as u8],
            );
            n.callback(cb);
        }
    });
    let mut max_len = 0;
    for _ in 0..20 {
        host.pump(Duration::from_millis(500)).unwrap();
        max_len = max_len.max(events.len());
        // Responses still flow while callbacks are held back.
        assert_eq!(host.network_state().unwrap(), NetworkStatus::JOINED_NETWORK);
    }
    assert!(max_len <= events.capacity(), "bounded");
    assert!(events.len() < N, "the NCP paused callbacks (nRdy)");
    let mut received = 0;
    for _ in 0..100 {
        received += callbacks(&events).len();
        if received >= N {
            break;
        }
        host.pump(Duration::from_millis(200)).unwrap();
    }
    assert_eq!(received, N);
    assert_eq!(host.stats().dropped_callbacks, 0);
    assert_eq!(sim.ncp(|n| n.dropped_callbacks()), 0);
}

#[test]
fn full_queue_drops_are_counted_and_reported() {
    let cfg = HostConfig {
        event_queue: 2,
        event_not_ready_at: 2,
        ..HostConfig::default()
    };
    let (sim, mut host, events) = setup(sim_version(18), Faults::none(), cfg);
    host.start().unwrap();
    host.network_init(NetworkInitBitmask::NO_OPTIONS).unwrap();
    sim.ncp(|n| {
        for i in 0..6 {
            let cb = n.incoming_message(IncomingMessageType::UNICAST, on_off_aps(), i, &[]);
            n.callback(cb);
        }
    });
    host.pump(Duration::from_millis(500)).unwrap();
    assert!(host.stats().dropped_callbacks > 0);
    assert_eq!(events.len(), 2);
    events.drain();
    host.pump(Duration::from_millis(10)).unwrap();
    assert!(
        matches!(events.try_recv(), Some(HostEvent::Dropped(n)) if n == host.stats().dropped_callbacks)
    );
}

#[test]
fn ncp_overflow_flag_is_surfaced() {
    let mut cfg = sim_version(16);
    cfg.ash.tx_queue_frames = 2;
    let (sim, mut host, events) = setup(cfg, Faults::none(), HostConfig::default());
    host.start().unwrap();
    host.network_init(NetworkInitBitmask::NO_OPTIONS).unwrap();
    sim.ncp(|n| {
        for i in 0..8 {
            let cb = n.incoming_message(IncomingMessageType::UNICAST, on_off_aps(), i, &[]);
            n.callback(cb);
        }
    });
    host.pump(Duration::from_millis(500)).unwrap();
    assert!(sim.ncp(|n| n.dropped_callbacks()) > 0);
    host.network_state().unwrap();
    assert!(events.drain().contains(&HostEvent::NcpOverflow));
    assert_eq!(host.stats().ncp_overflows, 1);
}
