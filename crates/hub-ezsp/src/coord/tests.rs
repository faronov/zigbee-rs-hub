//! The EZSP backend against the simulated NCP on a paced virtual clock
//! (the worker runs on its own thread): session start, the request
//! contract (outcomes, ordering, barriers, busy, session loss), events and
//! administration. Contract checks are backend-neutral ([`super::contract`]).

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hub_coord::{
    ApsDst, ApsRequest, BackendConfig, BackendKind, CoordError, CoordEvent, Coordinator,
    EVENT_SLOTS, EndpointConfig, EventQueue, FormParams, Kind, NetChange, NetState, NetworkAdmin,
    NetworkIdentity, NetworkOwnership, OwnershipModel, Progress, Reply, Request, RequestId,
    SessionEvent, SessionState, Terminal, TxOptions,
};

use super::contract::{check, down, position, progress, terminal, up};
use super::*;
use crate::ash::ResetCode;
use crate::ezsp::command::{Callback, Command, id};
use crate::sim::{Delivery, Faults, LinkFault, Sim, SimConfig, SimNcp};

/// Real-time limit of every wait.
const T: Duration = Duration::from_secs(30);
/// Virtual milliseconds per real millisecond.
const PACE: u64 = 10;
const ZA09: [u8; 16] = *b"ZigBeeAlliance09";

fn opts() -> EzspOptions {
    EzspOptions {
        host: HostConfig {
            command_timeout: Duration::from_secs(2),
            ..HostConfig::default()
        },
        confirm_timeout: Duration::from_secs(20),
        network_up_timeout: Duration::from_secs(2),
        retry_min: Duration::from_millis(200),
        ..EzspOptions::default()
    }
}

fn sim(version: u8) -> SimConfig {
    SimConfig {
        ezsp_version: version,
        ..SimConfig::default()
    }
}

fn hub_endpoint() -> EndpointConfig {
    EndpointConfig {
        ep: 1,
        profile: 0x0104,
        device_id: 0x0005,
        version: 1,
        in_clusters: vec![0x0000, 0x0003, 0x0006],
        out_clusters: vec![0x0003, 0x0006, 0x0019],
    }
}

fn aps(dst: ApsDst, ack: bool) -> Request {
    Request::Aps(ApsRequest {
        dst,
        dst_ep: 1,
        src_ep: 1,
        profile: 0x0104,
        cluster: 6,
        payload: vec![0x01, 0x07, 0x01],
        options: TxOptions {
            ack,
            extended_timeout: false,
        },
    })
}

fn unicast(nwk: u16) -> Request {
    aps(ApsDst::Nwk(nwk), true)
}

struct Rig {
    sim: Sim,
    coord: Option<EzspCoordinator>,
    admin: EzspAdmin,
    q: Arc<EventQueue>,
    log: Vec<CoordEvent>,
    ids: Vec<RequestId>,
}

impl Rig {
    fn new(cfg: SimConfig) -> Rig {
        Rig::with(cfg, opts(), Faults::none(), EVENT_SLOTS)
    }

    fn with(cfg: SimConfig, opts: EzspOptions, faults: Faults, slots: usize) -> Rig {
        let (sim, transport, clock) = Sim::new(cfg, faults);
        sim.set_pace(Some(PACE));
        let q = Arc::new(EventQueue::new(slots, Box::new(|| {})));
        let backend = BackendConfig {
            endpoints: vec![hub_endpoint()],
        };
        let (coord, admin) =
            EzspCoordinator::start(transport, clock, opts, backend, q.clone()).unwrap();
        Rig {
            sim,
            coord: Some(coord),
            admin,
            q,
            log: Vec::new(),
            ids: Vec::new(),
        }
    }

    fn coord(&mut self) -> &mut EzspCoordinator {
        self.coord.as_mut().unwrap()
    }

    fn drain(&mut self) {
        let log = &mut self.log;
        self.q.drain(usize::MAX, |e| log.push(e));
    }

    /// Collects events until `stop` holds for the log.
    fn until(&mut self, what: &str, stop: impl Fn(&[CoordEvent]) -> bool) {
        let t = Instant::now();
        loop {
            self.drain();
            if stop(&self.log) {
                return;
            }
            assert!(t.elapsed() < T, "{what}: {:#?}", self.log);
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// The next `Up` after the events seen so far; its epoch.
    fn next_up(&mut self) -> u32 {
        let from = self.log.len();
        self.until("session up", |l| l[from..].iter().any(|e| up(e).is_some()));
        self.log[from..].iter().find_map(up).unwrap()
    }

    fn submit(&mut self, req: Request) -> RequestId {
        let id = self.coord().submit(req, None).unwrap();
        self.ids.push(id);
        id
    }

    fn outcome(&mut self, id: RequestId) -> Terminal {
        self.until("terminal", |l| terminal(l, id).is_some());
        terminal(&self.log, id).unwrap().1.clone()
    }

    /// Waits until `pred` holds for the simulated NCP.
    fn sim_until(&self, what: &str, pred: impl Fn(&SimNcp) -> bool) {
        let t = Instant::now();
        while !self.sim.ncp(|n| pred(n)) {
            assert!(t.elapsed() < T, "{what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn commands(&self) -> Vec<u16> {
        self.sim.ncp(|n| n.commands().to_vec())
    }

    fn sent(&self) -> Vec<Command> {
        self.sim
            .ncp(|n| n.sent().iter().map(|m| m.command.clone()).collect())
    }

    fn inject(&self, f: impl FnOnce(&SimNcp) -> Callback) {
        self.sim.ncp(|n| {
            let cb = f(n);
            n.callback(cb)
        });
    }

    /// Stops the backend; the log must satisfy the request contract.
    fn finish(mut self) -> Vec<CoordEvent> {
        self.coord.take().unwrap().shutdown();
        self.drain();
        check(&self.log, &self.ids).unwrap_or_else(|e| panic!("{e}: {:#?}", self.log));
        self.log
    }
}

fn index_of(commands: &[u16], frame: u16) -> usize {
    commands.iter().position(|c| *c == frame).unwrap()
}

fn last_index_of(commands: &[u16], frame: u16) -> usize {
    commands.iter().rposition(|c| *c == frame).unwrap()
}

fn incoming(n: &SimNcp, from: u16, profile: u16, cluster: u16, payload: &[u8]) -> Callback {
    let aps = ApsFrame {
        profile_id: profile,
        cluster_id: cluster,
        source_endpoint: if profile == 0 { 0 } else { 1 },
        destination_endpoint: if profile == 0 { 0 } else { 1 },
        options: ApsOptions::NONE,
        group_id: 0,
        sequence: 1,
    };
    n.incoming_message(IncomingMessageType::UNICAST, aps, from, payload)
}

fn tc_join(nwk: u16, ieee: u64, status: DeviceUpdate, decision: JoinDecision) -> Callback {
    Callback::TrustCenterJoin {
        new_node_id: nwk,
        new_node_eui64: Eui64::from_u64(ieee),
        status,
        policy_decision: decision,
        parent_of_new_node_id: 0,
    }
}

// ------------------------------------------------------------ session

/// EZSP 13 (EmberZNet 7.4, the SONOFF Dongle Max image): reset, version
/// (twice: the host asks for 18 first), configuration before policies,
/// endpoints after both, networkInit last; then ownership with the key
/// ID, `Up` and the network state.
#[test]
fn ezsp_13_session_starts_in_order_and_reports_identity_and_capabilities() {
    let cfg = sim(13);
    let (epid, key, eui) = (cfg.network.extended_pan_id, cfg.network_key, cfg.eui64);
    let mut rig = Rig::new(cfg);
    assert_eq!(rig.next_up(), 1);
    rig.until("network up", |l| {
        l.contains(&CoordEvent::NetworkState(NetChange::Up))
    });
    let at = |f: &dyn Fn(&CoordEvent) -> bool| position(&rig.log, f).unwrap();
    let owned = at(&|e| matches!(e, CoordEvent::Ownership(_)));
    assert!(owned < at(&|e| up(e).is_some()), "{:#?}", rig.log);
    assert!(at(&|e| up(e).is_some()) < at(&|e| matches!(e, CoordEvent::NetworkState(_))));
    let CoordEvent::Ownership(o) = &rig.log[owned] else {
        unreachable!()
    };
    let NetworkOwnership::Observed(identity) = **o else {
        panic!("{o:?}")
    };
    assert_eq!(identity.ext_pan_id, epid);
    assert_eq!(identity.key_id, Some(NetworkIdentity::key_id_for(&key)));
    assert_eq!((identity.pan_id, identity.channel), (0x1A62, 15));
    assert_eq!(identity.coordinator_ieee, eui.0);

    let c = rig.commands();
    assert_eq!(&c[..2], &[id::VERSION, id::VERSION]);
    let last_config = last_index_of(&c, id::SET_CONFIGURATION_VALUE);
    assert!(index_of(&c, id::SET_CONFIGURATION_VALUE) < index_of(&c, id::SET_POLICY));
    assert!(last_config < index_of(&c, id::SET_POLICY));
    assert!(last_index_of(&c, id::SET_POLICY) < index_of(&c, id::ADD_ENDPOINT));
    assert!(index_of(&c, id::ADD_ENDPOINT) < index_of(&c, id::NETWORK_INIT));
    rig.sim.ncp(|n| {
        assert_eq!(n.configuration(ConfigId::STACK_PROFILE), Some(2));
        assert_eq!(n.configuration(ConfigId::SECURITY_LEVEL), Some(5));
        assert_eq!(
            n.configuration(ConfigId::SOURCE_ROUTE_TABLE_SIZE),
            Some(200)
        );
        assert_eq!(n.configuration(ConfigId::APPLICATION_ZDO_FLAGS), Some(1));
        assert_eq!(n.policy(PolicyId::TRUST_CENTER_POLICY), Some(0x02));
        assert_eq!(n.policy(PolicyId::TC_KEY_REQUEST_POLICY), Some(0x51));
        assert_eq!(n.policy(PolicyId::APP_KEY_REQUEST_POLICY), Some(0x60));
        assert_eq!(
            n.policy(PolicyId::MESSAGE_CONTENTS_IN_CALLBACK_POLICY),
            Some(0x40)
        );
        assert_eq!(n.endpoints().len(), 1);
        assert_eq!(
            n.endpoints()[0].input_clusters,
            vec![0x0000, 0x0003, 0x0006]
        );
        assert!(matches!(
            n.concentrator(),
            Some(Command::SetConcentrator {
                on: true,
                concentrator_type: concentrator::HIGH_RAM,
                ..
            })
        ));
    });

    let st = rig.coord().status();
    assert_eq!((st.session, st.epoch), (SessionState::Ready, Some(1)));
    assert_eq!(st.network_state, NetState::Up);
    assert_eq!(st.ownership, NetworkOwnership::Observed(identity));
    let ncp = st.ncp.unwrap();
    assert_eq!(ncp.fw, "EmberZNet 8.2.3.0 build 397");
    assert_eq!(ncp.protocol, (13, 0));
    assert_eq!(
        (ncp.board.as_str(), ncp.product.as_str()),
        ("SIM-MG24", "hub-ezsp sim")
    );
    assert_eq!((ncp.baud, ncp.flow_control), (115_200, false));
    let caps = st.caps.unwrap();
    assert_eq!(caps.backend, BackendKind::Ezsp);
    assert_eq!(caps.ownership, OwnershipModel::NcpNv);
    assert_eq!((caps.max_in_flight, caps.max_asdu), (32, 82));
    assert_eq!(caps.tables.multicast, Some(16));
    assert_eq!(caps.tables.source_routes, Some(200));
    assert!(!caps.gp.sink && !caps.backup.export && caps.fw_update.is_none());
    rig.finish();
}

#[test]
fn every_verified_version_comes_up_and_delivers() {
    for v in [13, 14, 16, 17, 18] {
        let mut rig = Rig::new(sim(v));
        rig.next_up();
        let id = rig.submit(unicast(0x1234));
        assert_eq!(rig.outcome(id), Terminal::Delivered, "EZSP {v}");
        rig.finish();
    }
}

/// Secure Vault may keep the network key from being exported: the
/// identity is then EPID-only (design §8.5).
#[test]
fn a_key_the_ncp_does_not_export_leaves_the_identity_epid_only() {
    let mut rig = Rig::new(SimConfig {
        key_exportable: false,
        ..sim(13)
    });
    rig.next_up();
    match rig.coord().status().ownership {
        NetworkOwnership::Observed(id) => assert_eq!(id.key_id, None),
        o => panic!("{o:?}"),
    }
    rig.finish();
}

#[test]
fn an_ncp_without_a_network_reports_none_and_forming_reports_it() {
    let mut rig = Rig::new(SimConfig {
        network_formed: false,
        energy: vec![(20, -50), (25, -85)],
        ..sim(13)
    });
    rig.next_up();
    let st = rig.coord().status();
    assert_eq!(
        (st.network_state, st.ownership),
        (NetState::None, NetworkOwnership::NoNetwork)
    );
    assert!(rig.log.contains(&CoordEvent::Ownership(Box::new(
        NetworkOwnership::NoNetwork
    ))));
    let key = [0x11; 16];
    let p = rig
        .admin
        .form(FormParams {
            channel_mask: (1 << 20) | (1 << 25),
            pan_id: Some(0x4242),
            ext_pan_id: None,
            network_key: Some(key),
            tx_power: 4,
        })
        .unwrap();
    assert_eq!((p.channel, p.pan_id), (25, 0x4242), "the quieter channel");
    rig.until("ownership after forming", |l| {
        l.iter().any(|e| {
            matches!(e, CoordEvent::Ownership(o) if matches!(**o, NetworkOwnership::Observed(_)))
        })
    });
    let forming = position(&rig.log, |e| {
        *e == CoordEvent::NetworkState(NetChange::Forming)
    });
    let up_at = position(&rig.log, |e| *e == CoordEvent::NetworkState(NetChange::Up));
    assert!(forming.unwrap() < up_at.unwrap());
    match rig.coord().status().ownership {
        NetworkOwnership::Observed(id) => {
            assert_eq!(id.key_id, Some(NetworkIdentity::key_id_for(&key)));
            assert_eq!(id.ext_pan_id, p.ext_pan_id);
            assert_ne!(id.ext_pan_id, [0; 8]);
        }
        o => panic!("{o:?}"),
    }
    let sec = rig.sim.ncp(|n| n.security_state()).unwrap();
    // Global TC link key, preconfigured and network key, no frame
    // counter reset, encrypted key required, hashed link keys.
    assert_eq!(sec.bitmask, 0x1B84);
    assert_eq!(sec.network_key, key);
    assert_ne!(sec.preconfigured_key, ZA09, "a random root key");
    assert_eq!(rig.sim.ncp(|n| n.config().network.radio_tx_power), 4);
    assert!(rig.log.iter().any(|e| matches!(
        e,
        CoordEvent::ScanProgress {
            scan: 0,
            total: 2,
            ..
        }
    )));
    // A second form is refused while the network is up.
    let e = rig.admin.form(FormParams::default()).unwrap_err();
    assert!(
        matches!(
            e.meaning(),
            CoordError::Backend {
                kind: Kind::InvalidState,
                ..
            }
        ),
        "{e:?}"
    );
    rig.finish();
}

/// Reported once, and the state stays `Incompatible` while the worker
/// tries again (every `retry_max`).
#[test]
fn an_incompatible_ncp_is_reported_once_and_aps_refused() {
    let mut o = opts();
    o.retry_max = Duration::from_secs(1);
    let mut rig = Rig::with(sim(12), o, Faults::none(), EVENT_SLOTS);
    let incompatible =
        |e: &CoordEvent| matches!(e, CoordEvent::Session(s) if **s == SessionEvent::Incompatible);
    rig.until("incompatible", |l| l.iter().any(incompatible));
    let versions = |r: &Rig| r.commands().iter().filter(|c| **c == id::VERSION).count();
    let tried = versions(&rig);
    let t = Instant::now();
    while versions(&rig) < tried + 2 {
        assert_eq!(rig.coord().status().session, SessionState::Incompatible);
        assert_eq!(
            rig.coord().submit(unicast(1), None),
            Err(CoordError::LinkDown)
        );
        assert!(t.elapsed() < T);
        thread::sleep(Duration::from_millis(1));
    }
    rig.drain();
    assert_eq!(rig.log.iter().filter(|e| incompatible(e)).count(), 1);
    rig.finish();
}

// ------------------------------------------------------------ requests

#[test]
fn aps_outcomes_follow_the_message_sent_handler() {
    let mut rig = Rig::new(sim(13));
    rig.next_up();
    let acked = rig.submit(unicast(0x1234));
    let unacked = rig.submit(aps(ApsDst::Nwk(0x1234), false));
    let group = rig.submit(aps(ApsDst::Group(0x1001), false));
    let bcast = rig.submit(aps(ApsDst::Nwk(0xFFFD), false));
    assert_eq!(rig.outcome(acked), Terminal::Delivered);
    assert_eq!(rig.outcome(unacked), Terminal::Sent);
    assert_eq!(rig.outcome(group), Terminal::Sent);
    assert_eq!(rig.outcome(bcast), Terminal::Sent);
    for id in [acked, unacked, group, bcast] {
        assert_eq!(
            progress(&rig.log, id).map(|(_, p)| p),
            Some(Progress::Accepted { zcl_tsn: None })
        );
    }
    let sent = rig.sent();
    assert!(
        matches!(&sent[0], Command::SendUnicast { index_or_destination: 0x1234, aps, .. }
        if aps.options.contains(ApsOptions::RETRY) && aps.options.contains(ApsOptions::ENABLE_ROUTE_DISCOVERY))
    );
    assert!(
        matches!(&sent[1], Command::SendUnicast { aps, .. } if !aps.options.contains(ApsOptions::RETRY))
    );
    assert!(matches!(&sent[2], Command::SendMulticast { aps, .. } if aps.group_id == 0x1001));
    assert!(matches!(
        &sent[3],
        Command::SendBroadcast {
            destination: 0xFFFD,
            ..
        }
    ));

    rig.sim
        .ncp(|n| n.config_mut().unicast_delivery = Delivery::Failed);
    let lost = rig.submit(unicast(0x1234));
    match rig.outcome(lost) {
        Terminal::NotDelivered(CoordError::NoAck(code)) => {
            assert_eq!((code.backend, code.value), (BackendKind::Ezsp, 0x66))
        }
        t => panic!("{t:?}"),
    }
    // Network down: the NCP refuses.
    rig.sim
        .ncp(|n| n.config_mut().unicast_delivery = Delivery::Delivered);
    let unknown = rig.submit(aps(ApsDst::Ieee(0x00AB_CDEF_0000_0001), true));
    match rig.outcome(unknown) {
        Terminal::Refused(e) => {
            assert!(matches!(
                e.meaning(),
                CoordError::Backend {
                    kind: Kind::NoRoute,
                    ..
                }
            ))
        }
        t => panic!("{t:?}"),
    }
    rig.finish();
}

/// Design §2.2: a terminal never precedes an indication that the NCP
/// sent before it, and `Accepted` precedes the terminal.
#[test]
fn a_terminal_never_overtakes_an_indication_sent_before_it() {
    let mut rig = Rig::new(sim(13));
    rig.next_up();
    rig.sim.ncp(|n| n.hold_message_sent(true));
    let id = rig.submit(unicast(0x1234));
    rig.sim_until("send", |n| n.held_message_sent() == 1);
    rig.inject(|n| incoming(n, 0x1234, 0x0104, 6, &[0x18, 0x01, 0x0B, 0x01, 0x00]));
    rig.sim.ncp(|n| {
        n.hold_message_sent(false);
        n.release_message_sent(1);
    });
    assert_eq!(rig.outcome(id), Terminal::Delivered);
    let ind = position(
        &rig.log,
        |e| matches!(e, CoordEvent::Indication(i) if i.src_nwk == 0x1234),
    );
    let (accepted, _) = progress(&rig.log, id).unwrap();
    let (done, _) = terminal(&rig.log, id).unwrap();
    assert!(
        accepted < ind.unwrap() && ind.unwrap() < done,
        "{:#?}",
        rig.log
    );
    rig.finish();
}

/// An outcome the response decides (here a refusal) follows the
/// callbacks the NCP sent before that response.
#[test]
fn a_refusal_follows_the_callbacks_sent_before_the_response() {
    let mut rig = Rig::new(sim(13));
    rig.next_up();
    rig.sim.ncp(|n| {
        let cb = incoming(n, 0x0BAD, 0x0104, 6, &[0x01]);
        n.callback_before_next_response(cb);
        n.busy_sends(1);
    });
    let id = rig.submit(unicast(0x1234));
    assert_eq!(rig.outcome(id), Terminal::Refused(CoordError::Busy));
    let ind = position(
        &rig.log,
        |e| matches!(e, CoordEvent::Indication(i) if i.src_nwk == 0x0BAD),
    );
    assert!(
        ind.unwrap() < terminal(&rig.log, id).unwrap().0,
        "{:#?}",
        rig.log
    );
    rig.finish();
}

/// A request to an IEEE address the NCP does not know is refused after
/// the callbacks that came before the lookup's response.
#[test]
fn an_unknown_ieee_is_refused_after_the_callbacks_sent_before_the_lookup() {
    let mut rig = Rig::new(sim(14));
    rig.next_up();
    rig.sim.ncp(|n| {
        let cb = incoming(n, 0x0BAD, 0x0104, 6, &[0x01]);
        n.callback_before_next_response(cb);
    });
    let id = rig.submit(aps(ApsDst::Ieee(0x00AB_CDEF_0000_0002), true));
    assert!(matches!(rig.outcome(id), Terminal::Refused(_)));
    let ind = position(
        &rig.log,
        |e| matches!(e, CoordEvent::Indication(i) if i.src_nwk == 0x0BAD),
    );
    assert!(
        ind.unwrap() < terminal(&rig.log, id).unwrap().0,
        "{:#?}",
        rig.log
    );
    rig.finish();
}

/// A `messageSentHandler` that came before the deadline, while the
/// application was behind, decides the outcome: the deadline is checked
/// only after the callbacks that came before it.
#[test]
fn a_deadline_does_not_overtake_a_message_sent_handler_already_received() {
    let mut rig = Rig::with(sim(13), opts(), Faults::none(), 2);
    rig.next_up();
    rig.sim.ncp(|n| n.hold_message_sent(true));
    let id = rig
        .coord()
        .submit(unicast(0x1234), Some(Duration::from_secs(3)))
        .unwrap();
    rig.ids.push(id);
    rig.sim_until("sent", |n| n.held_message_sent() == 1);
    rig.drain();
    // The application falls behind: its queue (2) fills up.
    for i in 0..8 {
        rig.inject(|n| incoming(n, 0x1234, 0x0104, 6, &[i]));
    }
    rig.sim.ncp(|n| {
        n.hold_message_sent(false);
        n.release_message_sent(1);
    });
    // 6 s of virtual time with nobody draining: past the deadline.
    thread::sleep(Duration::from_millis(600));
    assert_eq!(rig.outcome(id), Terminal::Delivered);
    rig.finish();
}

/// Control requests are barriers; `submit` never waits, even while the
/// worker waits for the NCP; a request cancelled before it was taken ends
/// `Cancelled`.
#[test]
fn control_requests_are_barriers_and_submit_never_waits() {
    let mut rig = Rig::new(sim(13));
    rig.next_up();
    rig.sim
        .ncp(|n| n.stall(Some(id::SET_MULTICAST_TABLE_ENTRY)));
    let t = Instant::now();
    let group = rig.submit(Request::Group {
        endpoint: 1,
        group: 0x1001,
        add: true,
    });
    let first = rig.submit(unicast(0x1234));
    let cancelled = rig.submit(unicast(0x1234));
    rig.coord().cancel(cancelled);
    assert!(t.elapsed() < Duration::from_millis(50), "{:?}", t.elapsed());
    rig.sim_until("group sent", |n| {
        n.commands().contains(&id::SET_MULTICAST_TABLE_ENTRY)
    });
    thread::sleep(Duration::from_millis(50));
    assert!(
        !rig.commands().contains(&id::SEND_UNICAST),
        "nothing passes the barrier"
    );
    // The NCP never answers: the command times out (2 s).
    assert_eq!(rig.outcome(group), Terminal::Unknown(CoordError::Timeout));
    rig.sim.ncp(|n| n.stall(None));
    assert_eq!(rig.outcome(first), Terminal::Delivered);
    assert_eq!(rig.outcome(cancelled), Terminal::Cancelled);
    let c = rig.commands();
    assert!(index_of(&c, id::SET_MULTICAST_TABLE_ENTRY) < index_of(&c, id::SEND_UNICAST));
    let order: Vec<RequestId> = rig
        .log
        .iter()
        .filter_map(|e| match e {
            CoordEvent::Done { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(order, [group, cancelled, first]);
    assert_eq!(rig.coord().queued().0, 0);
    rig.finish();
}

/// Acknowledged unicasts beyond the NCP's APS unicast slots wait in the
/// queue, in order (design §3.6), instead of being refused by the NCP.
#[test]
fn unicasts_beyond_the_ncp_slots_wait_in_order() {
    let mut o = opts();
    o.config.push((ConfigId::APS_UNICAST_MESSAGE_COUNT, 2));
    let mut rig = Rig::with(sim(13), o, Faults::none(), EVENT_SLOTS);
    rig.next_up();
    assert_eq!(rig.coord().status().caps.unwrap().max_in_flight, 2);
    rig.sim.ncp(|n| n.hold_message_sent(true));
    let ids: Vec<RequestId> = (0..4).map(|_| rig.submit(unicast(0x1234))).collect();
    rig.sim_until("two sent", |n| n.held_message_sent() == 2);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(rig.sent().len(), 2, "the others wait for a slot");
    rig.sim.ncp(|n| n.release_message_sent(1));
    rig.sim_until("third sent", |n| n.sent().len() == 3);
    rig.sim.ncp(|n| {
        n.hold_message_sent(false);
        n.release_message_sent(usize::MAX);
    });
    for id in &ids {
        assert_eq!(rig.outcome(*id), Terminal::Delivered);
    }
    let done: Vec<usize> = ids
        .iter()
        .map(|id| terminal(&rig.log, *id).unwrap().0)
        .collect();
    assert!(done.windows(2).all(|w| w[0] < w[1]), "in order");
    rig.finish();
}

/// NCP buffer exhaustion (MAX_MESSAGE_LIMIT_REACHED) is the plain
/// `Refused(Busy)` the application counts as busy.
#[test]
fn ncp_buffer_exhaustion_is_refused_busy() {
    let mut rig = Rig::new(sim(14));
    rig.next_up();
    rig.sim.ncp(|n| n.busy_sends(1));
    let busy = rig.submit(unicast(0x1234));
    let next = rig.submit(unicast(0x1234));
    assert_eq!(rig.outcome(busy), Terminal::Refused(CoordError::Busy));
    assert_eq!(rig.outcome(next), Terminal::Delivered);
    rig.finish();
}

#[test]
fn requests_beyond_the_command_cap_are_refused() {
    let mut o = opts();
    o.host.command_timeout = Duration::from_secs(60);
    let mut rig = Rig::with(sim(13), o, Faults::none(), EVENT_SLOTS);
    rig.next_up();
    rig.sim
        .ncp(|n| n.stall(Some(id::SET_MULTICAST_TABLE_ENTRY)));
    rig.submit(Request::Group {
        endpoint: 1,
        group: 1,
        add: true,
    });
    rig.sim_until("group sent", |n| {
        n.commands().contains(&id::SET_MULTICAST_TABLE_ENTRY)
    });
    for _ in 0..COMMAND_CAP {
        rig.submit(unicast(0x1234));
    }
    assert_eq!(
        rig.coord().submit(unicast(0x1234), None),
        Err(CoordError::Busy)
    );
    assert_eq!(rig.coord().queued(), (COMMAND_CAP, COMMAND_CAP));
    rig.coord.take().unwrap().shutdown();
}

#[test]
fn gp_requests_are_refused_and_backup_is_not_offered_yet() {
    let mut rig = Rig::new(sim(13));
    rig.next_up();
    let gp = Request::Gp(hub_coord::GpRequest::SinkConfig {
        mode: 1,
        communication_mode: 2,
        group: 0x0B84,
        exit_mode: 2,
        window_s: 180,
        security_level: 0,
        groupcast_radius: 0,
    });
    let e = rig.coord().submit(gp, None).unwrap_err();
    assert_eq!(*e.meaning(), CoordError::Unsupported);
    struct Sink;
    impl hub_coord::BackupSink for Sink {
        fn item(&mut self, _: &hub_coord::BackupItem) -> Result<(), CoordError> {
            Ok(())
        }
    }
    assert_eq!(
        rig.admin.backup_export(true, &mut Sink),
        Err(CoordError::Unsupported)
    );
    assert!(rig.admin.restore_encoder().is_err());
    assert!(rig.admin.firmware().is_none());
    // A GP frame from the NCP goes to the GP path (counted, no event).
    rig.inject(|_| Callback::Unknown {
        frame_id: id::GPEP_INCOMING_MESSAGE_HANDLER,
        params: vec![0; 24],
    });
    let id = rig.submit(unicast(0x1234));
    rig.outcome(id);
    let d = rig.admin.diagnostics(0xFF, false);
    let gp = d
        .host_link
        .iter()
        .find(|c| c.name == Some("gp_frames_ignored"));
    assert_eq!(gp.map(|c| c.value), Some(1));
    assert!(!rig.log.iter().any(|e| matches!(e, CoordEvent::Gp(_))));
    rig.finish();
}

// ------------------------------------------------------------ session loss

/// With a traffic gate (an application that decides network ownership,
/// `NcpNv`): the request queued behind the one on the NCP is not sent in
/// the next session, which may run another network; it ends
/// `Refused(LinkDown)`. The worker closes the gate for each new session's
/// epoch; requests go out once the application opens that epoch, and an
/// opening for an earlier session does not count.
#[test]
fn with_a_traffic_gate_queued_requests_do_not_outlive_the_session() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let gate = Arc::new(AtomicU32::new(0));
    let open = |epoch: u32| {
        gate.compare_exchange(
            traffic_closed(epoch),
            traffic_closed(epoch) | 1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_ok()
    };
    let mut o = opts();
    o.config.push((ConfigId::APS_UNICAST_MESSAGE_COUNT, 1));
    o.traffic = Some(gate.clone());
    let mut rig = Rig::with(sim(13), o, Faults::none(), EVENT_SLOTS);
    let first = rig.next_up();
    assert_eq!(
        gate.load(Ordering::SeqCst),
        traffic_closed(first),
        "closed until the application decides"
    );
    assert!(open(first));
    rig.sim.ncp(|n| n.hold_message_sent(true));
    let a = rig.submit(unicast(0x1234));
    let b = rig.submit(unicast(0x5678));
    rig.sim_until("a sent", |n| n.held_message_sent() == 1);
    rig.sim.ncp(|n| {
        n.hold_message_sent(false);
        n.spontaneous_reset(ResetCode::WATCHDOG);
    });
    let b_t = rig.outcome(b);
    assert!(
        matches!(&b_t, Terminal::Refused(e) if *e.meaning() == CoordError::LinkDown),
        "{b_t:?}"
    );
    assert!(matches!(rig.outcome(a), Terminal::Unknown(_)));
    let second = rig.next_up();
    assert_ne!(second, first);
    let to = |dst: u16| move |c: &Command| matches!(c, Command::SendUnicast { index_or_destination, .. } if *index_or_destination == dst);
    // A late decision about the first session opens nothing.
    assert!(!open(first));
    gate.store(traffic_closed(first) | 1, Ordering::SeqCst);
    let c = rig.submit(unicast(0x5678));
    assert!(matches!(rig.outcome(c), Terminal::Refused(_)));
    assert!(!rig.sent().iter().any(to(0x5678)), "{:?}", rig.sent());
    // The decision about this session opens it.
    gate.store(traffic_closed(second), Ordering::SeqCst);
    assert!(open(second));
    let d = rig.submit(unicast(0x5678));
    assert_eq!(rig.outcome(d), Terminal::Delivered);
    rig.finish();
}

/// With `traffic_hold`: a request queued before the application decides
/// about a new session waits for the decision and goes out once the flag
/// opens for that session, for at most the hold; those queued when a
/// session ends still end at once.
#[test]
fn with_a_traffic_hold_requests_wait_for_the_decision_about_a_new_session() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let gate = Arc::new(AtomicU32::new(0));
    let open = |epoch: u32| {
        gate.compare_exchange(
            traffic_closed(epoch),
            traffic_closed(epoch) | 1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_ok()
    };
    let to = |dst: u16| move |c: &Command| matches!(c, Command::SendUnicast { index_or_destination, .. } if *index_or_destination == dst);
    let refused =
        |t: &Terminal| matches!(t, Terminal::Refused(e) if *e.meaning() == CoordError::LinkDown);
    let mut o = opts();
    o.config.push((ConfigId::APS_UNICAST_MESSAGE_COUNT, 1));
    o.traffic = Some(gate.clone());
    o.traffic_hold = Duration::from_secs(5);
    let mut rig = Rig::with(sim(13), o, Faults::none(), EVENT_SLOTS);
    // Queued before the first session: held until the decision.
    let a = rig.submit(unicast(0x1234));
    let first = rig.next_up();
    thread::sleep(Duration::from_millis(50));
    rig.drain();
    assert!(terminal(&rig.log, a).is_none(), "held: {:#?}", rig.log);
    assert!(
        !rig.sent().iter().any(to(0x1234)),
        "nothing sent before the decision"
    );
    assert!(open(first));
    assert_eq!(rig.outcome(a), Terminal::Delivered);
    // Queued when the session ends: ended at once.
    rig.sim.ncp(|n| n.hold_message_sent(true));
    let b = rig.submit(unicast(0x1234));
    let c = rig.submit(unicast(0x5678));
    rig.sim_until("b sent", |n| n.held_message_sent() == 1);
    rig.sim.ncp(|n| {
        n.hold_message_sent(false);
        n.spontaneous_reset(ResetCode::WATCHDOG);
    });
    let c_t = rig.outcome(c);
    assert!(refused(&c_t), "{c_t:?}");
    assert!(matches!(rig.outcome(b), Terminal::Unknown(_)));
    rig.next_up();
    // No decision within the hold: refused, never sent.
    let d = rig.submit(unicast(0x5678));
    let d_t = rig.outcome(d);
    assert!(refused(&d_t), "{d_t:?}");
    assert!(!rig.sent().iter().any(to(0x5678)), "{:?}", rig.sent());
    rig.finish();
}

/// An NCP reset: `Down`, then the request on the NCP ends
/// `Unknown(DeliveryUnknown)` (EZSP loses it, design §2.2); the queued one
/// goes out after the next `Up`, in a new epoch.
#[test]
fn an_ncp_reset_ends_requests_on_the_ncp_at_down_and_sends_the_queued_after_up() {
    let mut o = opts();
    o.config.push((ConfigId::APS_UNICAST_MESSAGE_COUNT, 1));
    let mut rig = Rig::with(sim(13), o, Faults::none(), EVENT_SLOTS);
    let epoch = rig.next_up();
    rig.sim.ncp(|n| n.hold_message_sent(true));
    let a = rig.submit(unicast(0x1234));
    let b = rig.submit(unicast(0x5678));
    rig.sim_until("a sent", |n| n.held_message_sent() == 1);
    rig.sim.ncp(|n| {
        n.hold_message_sent(false);
        n.spontaneous_reset(ResetCode::WATCHDOG);
    });
    assert_eq!(rig.outcome(b), Terminal::Delivered);
    let down_at = position(&rig.log, down).expect("down");
    let (a_at, a_t) = terminal(&rig.log, a).unwrap();
    assert!(
        matches!(a_t, Terminal::Unknown(e) if *e.meaning() == CoordError::DeliveryUnknown),
        "{a_t:?}"
    );
    assert!(down_at < a_at, "{:#?}", rig.log);
    let up_again = rig
        .log
        .iter()
        .enumerate()
        .skip(down_at)
        .find_map(|(i, e)| up(e).map(|ep| (i, ep)))
        .unwrap();
    assert_ne!(up_again.1, epoch);
    assert!(terminal(&rig.log, b).unwrap().0 > up_again.0);
    // The second session did the whole start again.
    let c = rig.commands();
    assert_eq!(c.iter().filter(|f| **f == id::NETWORK_INIT).count(), 2);
    rig.finish();
}

/// A reset right behind the response: `Accepted`, `Down`, then `Unknown`.
#[test]
fn a_reset_right_after_the_response_ends_the_request_unknown() {
    let mut rig = Rig::new(sim(18));
    rig.next_up();
    rig.sim
        .ncp(|n| n.fail_after_next_response(LinkFault::Reset(ResetCode::POWER_ON)));
    let id = rig.submit(unicast(0x1234));
    let t = rig.outcome(id);
    assert!(
        matches!(&t, Terminal::Unknown(e) if *e.meaning() == CoordError::DeliveryUnknown),
        "{t:?}"
    );
    rig.next_up();
    let (accepted, _) = progress(&rig.log, id).unwrap();
    let down_at = position(&rig.log, down).unwrap();
    assert!(accepted < down_at && down_at < terminal(&rig.log, id).unwrap().0);
    rig.finish();
}

/// The operator's TX power of an earlier run (the hub keeps it) is applied
/// in the first session already.
#[test]
fn a_kept_tx_power_is_applied_from_the_first_session() {
    let o = EzspOptions {
        tx_power: Some(-3),
        ..opts()
    };
    let mut rig = Rig::with(sim(13), o, Faults::none(), EVENT_SLOTS);
    rig.next_up();
    assert_eq!(rig.admin.tx_power_setting(), Some(-3));
    assert_eq!(rig.sim.ncp(|n| n.config().network.radio_tx_power), -3);
    rig.finish();
}

/// The multicast table and the endpoints are RAM on the NCP: written again
/// in the next session; so is the operator's TX power.
#[test]
fn groups_endpoints_and_tx_power_are_restored_after_a_reset() {
    let mut rig = Rig::new(sim(13));
    rig.next_up();
    let add = rig.submit(Request::Group {
        endpoint: 1,
        group: 0x1001,
        add: true,
    });
    let again = rig.submit(Request::Group {
        endpoint: 1,
        group: 0x1001,
        add: true,
    });
    assert_eq!(rig.outcome(add), Terminal::Done(Reply::None));
    assert_eq!(rig.outcome(again), Terminal::Done(Reply::None));
    assert_eq!(rig.admin.set_tx_power(5), Ok(5));
    assert_eq!(rig.admin.tx_power_setting(), Some(5));
    let entry = |n: &SimNcp| n.multicast_table().first().copied();
    assert_eq!(
        rig.sim.ncp(|n| entry(n)),
        Some(MulticastTableEntry {
            multicast_id: 0x1001,
            endpoint: 1
        })
    );
    rig.sim.ncp(|n| {
        n.config_mut().network.radio_tx_power = 8;
        n.spontaneous_reset(ResetCode::WATCHDOG);
    });
    rig.next_up();
    rig.sim.ncp(|n| {
        assert_eq!(
            entry(n),
            Some(MulticastTableEntry {
                multicast_id: 0x1001,
                endpoint: 1
            })
        );
        assert_eq!(n.endpoints().len(), 1);
        assert_eq!(n.config().network.radio_tx_power, 5);
    });
    let c = rig.commands();
    assert_eq!(c.iter().filter(|f| **f == id::ADD_ENDPOINT).count(), 2);
    // Removal clears the entry.
    let rm = rig.submit(Request::Group {
        endpoint: 1,
        group: 0x1001,
        add: false,
    });
    assert_eq!(rig.outcome(rm), Terminal::Done(Reply::None));
    assert_eq!(rig.sim.ncp(|n| entry(n)).map(|e| e.endpoint), Some(0));
    rig.finish();
}

/// A lossy UART (drops, corruption, duplicates, delays): ASH recovers and
/// every request still ends exactly once.
#[test]
fn a_lossy_uart_still_ends_every_request_once() {
    let mut rig = Rig::with(sim(13), opts(), Faults::uniform(7, 30, 300), EVENT_SLOTS);
    rig.next_up();
    let ids: Vec<RequestId> = (0..20u16)
        .map(|i| rig.submit(unicast(0x1000 + i)))
        .collect();
    for id in &ids {
        rig.outcome(*id);
    }
    let lost = rig.sim.pipe_stats();
    assert!(lost.0.dropped + lost.1.dropped + lost.0.corrupted + lost.1.corrupted > 0);
    rig.finish();
}

// ------------------------------------------------------------ events

#[test]
fn joins_rejoins_and_leaves_are_reported_denials_are_not() {
    let mut rig = Rig::new(sim(13));
    rig.next_up();
    let dev = 0x0015_8D00_0012_3456;
    rig.inject(|_| {
        tc_join(
            0x4321,
            dev,
            DeviceUpdate::STANDARD_SECURITY_UNSECURED_JOIN,
            JoinDecision::USE_PRECONFIGURED_KEY,
        )
    });
    rig.inject(|_| {
        tc_join(
            0x4322,
            dev,
            DeviceUpdate::STANDARD_SECURITY_SECURED_REJOIN,
            JoinDecision::NO_ACTION,
        )
    });
    rig.inject(|_| {
        tc_join(
            0x4323,
            0x99,
            DeviceUpdate::STANDARD_SECURITY_UNSECURED_JOIN,
            JoinDecision::DENY_JOIN,
        )
    });
    rig.inject(|_| {
        tc_join(
            0x4324,
            0x98,
            DeviceUpdate::DEVICE_LEFT,
            JoinDecision::NO_ACTION,
        )
    });
    // The NCP's events and the application's requests are not ordered
    // with each other: an indication behind the joins marks them handled.
    rig.inject(|n| incoming(n, 0x0001, 0x0104, 6, &[0xEE]));
    rig.until("joins handled", |l| {
        l.iter().any(|e| matches!(e, CoordEvent::Indication(_)))
    });
    // An IEEE-addressed request goes to the address of the last join.
    let id = rig.submit(aps(ApsDst::Ieee(dev), true));
    assert_eq!(rig.outcome(id), Terminal::Delivered);
    let joined: Vec<(u64, u16)> = rig
        .log
        .iter()
        .filter_map(|e| match e {
            CoordEvent::DeviceJoined { ieee, nwk } => Some((*ieee, *nwk)),
            _ => None,
        })
        .collect();
    assert_eq!(joined, [(dev, 0x4321), (dev, 0x4322)]);
    let left: Vec<&CoordEvent> = rig
        .log
        .iter()
        .filter(|e| matches!(e, CoordEvent::DeviceLeft { .. }))
        .collect();
    assert_eq!(
        left,
        [&CoordEvent::DeviceLeft {
            ieee: 0x98,
            nwk: 0x4324,
            rejoin: None
        }]
    );
    assert!(matches!(
        rig.sent().last(),
        Some(Command::SendUnicast {
            index_or_destination: 0x4322,
            ..
        })
    ));
    rig.finish();
}

/// Permit join (design §5): the well-known key as a transient key, joins
/// in the trust center policy, the coordinator's permitJoining and a
/// Mgmt_Permit_Joining_req broadcast; closing undoes it, and so does the
/// end of the window.
#[test]
fn permit_join_opens_the_trust_center_and_closes_it_again() {
    let mut rig = Rig::new(sim(13));
    rig.next_up();
    let open = rig.submit(Request::PermitJoin {
        duration_s: 60,
        target: None,
    });
    assert_eq!(rig.outcome(open), Terminal::Done(Reply::None));
    rig.sim.ncp(|n| {
        assert_eq!(n.transient_keys(), &[(Eui64([0xFF; 8]), ZA09)]);
        assert_eq!(n.policy(PolicyId::TRUST_CENTER_POLICY), Some(0x03));
    });
    assert!(matches!(
        rig.sent().last(),
        Some(Command::SendBroadcast { destination: 0xFFFC, aps, message, .. })
            if aps.cluster_id == 0x0036 && aps.profile_id == 0 && message[1..] == [60, 1]
    ));
    assert!(
        rig.log
            .contains(&CoordEvent::PermitJoin { remaining_s: 60 })
    );
    assert!((58..=60).contains(&rig.coord().status().permit_join_s));
    let close = rig.submit(Request::PermitJoin {
        duration_s: 0,
        target: None,
    });
    assert_eq!(rig.outcome(close), Terminal::Done(Reply::None));
    rig.sim.ncp(|n| {
        assert!(n.transient_keys().is_empty());
        assert_eq!(n.policy(PolicyId::TRUST_CENTER_POLICY), Some(0x02));
    });
    assert_eq!(rig.coord().status().permit_join_s, 0);

    // One router only: no local permitJoining, the request goes to it.
    let permits = |r: &Rig| {
        r.commands()
            .iter()
            .filter(|c| **c == id::PERMIT_JOINING)
            .count()
    };
    let before = permits(&rig);
    let router = rig.submit(Request::PermitJoin {
        duration_s: 2,
        target: Some(0x1234),
    });
    assert_eq!(rig.outcome(router), Terminal::Done(Reply::None));
    assert_eq!(permits(&rig), before);
    assert!(matches!(
        rig.sent().last(),
        Some(Command::SendUnicast { index_or_destination: 0x1234, aps, .. }) if aps.cluster_id == 0x0036
    ));
    // The window ends by itself.
    let from = rig.log.len();
    rig.until("window closed", |l| {
        l[from..].contains(&CoordEvent::PermitJoin { remaining_s: 0 })
    });
    rig.sim.ncp(|n| {
        assert!(n.transient_keys().is_empty());
        assert_eq!(n.policy(PolicyId::TRUST_CENTER_POLICY), Some(0x02));
    });
    rig.finish();
}

/// A permit join that fails half way (here the Mgmt_Permit_Joining_req
/// broadcast: no buffer) leaves nothing open: neither the coordinator,
/// nor the trust center policy, nor the transient key.
#[test]
fn a_failed_permit_join_leaves_nothing_open() {
    let mut rig = Rig::new(sim(13));
    rig.next_up();
    rig.sim.ncp(|n| n.busy_sends(1));
    let open = rig.submit(Request::PermitJoin {
        duration_s: 60,
        target: None,
    });
    assert_eq!(rig.outcome(open), Terminal::Refused(CoordError::Busy));
    rig.sim.ncp(|n| {
        assert_eq!(n.permit_joining(), 0);
        assert!(n.transient_keys().is_empty());
        assert_eq!(n.policy(PolicyId::TRUST_CENTER_POLICY), Some(0x02));
    });
    assert_eq!(rig.coord().status().permit_join_s, 0);
    assert!(
        !rig.log
            .iter()
            .any(|e| matches!(e, CoordEvent::PermitJoin { .. }))
    );
    rig.finish();
}

/// A permit join through a router waits for its delivery; the previous
/// window ending meanwhile does not close the new one.
#[test]
fn a_new_permit_window_survives_the_end_of_the_previous_one() {
    let mut rig = Rig::new(sim(13));
    rig.next_up();
    let first = rig.submit(Request::PermitJoin {
        duration_s: 1,
        target: None,
    });
    assert_eq!(rig.outcome(first), Terminal::Done(Reply::None));
    rig.sim.ncp(|n| n.hold_message_sent(true));
    let router = rig.submit(Request::PermitJoin {
        duration_s: 60,
        target: Some(0x1234),
    });
    rig.sim_until("router request", |n| n.held_message_sent() == 1);
    // 3 s of virtual time: the first window has ended meanwhile.
    thread::sleep(Duration::from_millis(300));
    rig.sim.ncp(|n| {
        n.hold_message_sent(false);
        n.release_message_sent(usize::MAX);
    });
    assert_eq!(rig.outcome(router), Terminal::Done(Reply::None));
    rig.sim.ncp(|n| {
        assert_eq!(n.policy(PolicyId::TRUST_CENTER_POLICY), Some(0x03));
        assert_eq!(n.transient_keys().len(), 1);
    });
    assert!(rig.coord().status().permit_join_s > 50);
    rig.finish();
}

/// Software flow control on the UART: an XOFF from the NCP holds the
/// requests (no frame lost, no session lost) until XON.
#[test]
fn xoff_from_the_ncp_holds_requests_until_xon() {
    let mut rig = Rig::new(sim(13));
    rig.next_up();
    rig.sim.xoff();
    let ids: Vec<RequestId> = (0..3).map(|_| rig.submit(unicast(0x1234))).collect();
    thread::sleep(Duration::from_millis(20));
    rig.sim.xon();
    for id in &ids {
        assert_eq!(rig.outcome(*id), Terminal::Delivered);
    }
    let d = rig.admin.diagnostics(0xFF, false);
    let xoff = d
        .host_link
        .iter()
        .find(|c| c.name == Some("rx_xoff"))
        .unwrap();
    assert_eq!(xoff.value, 1);
    assert!(!rig.log.iter().any(down));
    rig.finish();
}

/// The application behind (a small event queue nobody drains): the worker
/// keeps the link (ASH ACKs, no session loss) and stops reading callbacks;
/// the host's not-ready holds the NCP's. Once drained, every indication
/// arrives, in order, none lost.
#[test]
fn a_slow_application_holds_callbacks_not_the_link() {
    let mut rig = Rig::with(sim(13), opts(), Faults::none(), 4);
    rig.next_up();
    let mut n = 0u8;
    for _ in 0..3 {
        for _ in 0..25 {
            rig.inject(|s| incoming(s, 0x1234, 0x0104, 6, &[n]));
            n += 1;
        }
        // 2 s of virtual time with nobody draining.
        thread::sleep(Duration::from_millis(200));
    }
    let id = rig.submit(unicast(0x1234));
    rig.until("all indications", |l| {
        l.iter()
            .filter(|e| matches!(e, CoordEvent::Indication(_)))
            .count()
            == 75
    });
    rig.outcome(id);
    let payloads: Vec<u8> = rig
        .log
        .iter()
        .filter_map(|e| match e {
            CoordEvent::Indication(i) => Some(i.payload[0]),
            _ => None,
        })
        .collect();
    assert_eq!(payloads, (0..75).collect::<Vec<u8>>());
    assert_eq!(rig.sim.ncp(|s| s.dropped_callbacks()), 0);
    assert!(
        !rig.log
            .iter()
            .any(|e| down(e) || matches!(e, CoordEvent::Dropped { .. }))
    );
    rig.finish();
}

// ------------------------------------------------------------ admin

#[test]
fn admin_scans_tables_channel_change_zdo_and_diagnostics() {
    let beacon = ZigbeeNetwork {
        channel: 20,
        pan_id: 0x7777,
        extended_pan_id: [7; 8],
        allowing_join: true,
        stack_profile: 2,
        nwk_update_id: 3,
    };
    let cfg = SimConfig {
        energy: vec![(11, -40), (15, -100)],
        beacons: vec![(beacon, 180, -60)],
        neighbors: vec![NeighborTableEntry {
            short_id: 0x1111,
            average_lqi: 200,
            in_cost: 1,
            out_cost: 3,
            age: 2,
            long_id: Eui64::from_u64(0x11),
        }],
        children: vec![ChildData {
            eui64: Eui64::from_u64(0x22),
            node_type: NodeType::SLEEPY_END_DEVICE,
            id: 0x2222,
            timeout: 8,
            ..ChildData::default()
        }],
        routes: vec![RouteTableEntry {
            destination: 0x3333,
            next_hop: 0x1111,
            ..RouteTableEntry::default()
        }],
        ..sim(13)
    };
    let mut rig = Rig::new(cfg);
    rig.next_up();
    let energy = rig.admin.energy_scan((1 << 11) | (1 << 15), 2).unwrap();
    assert_eq!(energy.len(), 2);
    assert_eq!(
        (energy[0].channel, energy[0].max_rssi_dbm, energy[0].energy),
        (11, -40, 255)
    );
    assert_eq!((energy[1].channel, energy[1].energy), (15, 0));
    let beacons = rig.admin.active_scan(0x07FF_F800, 2).unwrap();
    assert_eq!(beacons.len(), 1);
    assert_eq!(
        (
            beacons[0].pan_id,
            beacons[0].permit_join,
            beacons[0].update_id
        ),
        (0x7777, true, 3)
    );
    assert!(rig.admin.energy_scan(0, 2).is_err());

    let t = rig.admin.coordinator_tables(true);
    let n = t.neighbors.unwrap();
    assert_eq!((n[0].nwk, n[0].lqi, n[0].device_type), (0x1111, 200, 1));
    let c = t.children.unwrap();
    assert_eq!(
        (c[0].nwk, c[0].capability, c[0].timeout_exp),
        (0x2222, 0x80, 8)
    );
    assert_eq!(t.routes.unwrap().unwrap()[0].dst, 0x3333);

    let inv = rig.submit(Request::DeviceInventory);
    match rig.outcome(inv) {
        Terminal::Done(Reply::Inventory(i)) => {
            let found: Vec<(u64, Option<u8>)> =
                i.devices.iter().map(|d| (d.ieee, d.device_type)).collect();
            assert_eq!(found, [(0x22, Some(2)), (0x11, Some(1))]);
        }
        t => panic!("{t:?}"),
    }

    let change = rig.admin.change_channel(20).unwrap();
    assert_eq!((change.channel, change.nwk_update_id), (20, 1));
    assert_eq!(rig.coord().status().network.unwrap().channel, 20);
    assert!(rig.admin.change_channel(27).is_err());

    // ZDO: the request goes out; the answer (matched by TSN, cluster and
    // sender) is returned and passed on as an indication too.
    let admin = rig.admin.clone();
    let zdo = thread::spawn(move || {
        admin.zdo_request(0x1111, 0x0031, &[0x42, 0x00], Duration::from_secs(5))
    });
    rig.sim_until("ZDO sent", |n| {
        n.sent().iter().any(
            |m| matches!(&m.command, Command::SendUnicast { aps, .. } if aps.cluster_id == 0x0031),
        )
    });
    rig.inject(|n| incoming(n, 0x1111, 0, 0x8031, &[0x41, 0x00, 0x01]));
    rig.inject(|n| incoming(n, 0x1111, 0, 0x8031, &[0x42, 0x00, 0x01]));
    assert_eq!(zdo.join().unwrap(), Ok(vec![0x00, 0x01]));
    rig.sim
        .ncp(|n| n.config_mut().unicast_delivery = Delivery::Failed);
    let e = rig
        .admin
        .zdo_request(0x1111, 0x0031, &[0x43, 0x00], Duration::from_secs(5))
        .unwrap_err();
    assert!(matches!(e, CoordError::NoAck(_)), "{e:?}");

    let d = rig.admin.diagnostics(0xFF, true);
    assert_eq!(d.counters.iter().filter(|c| c.id < 0x100).count(), 41);
    assert_eq!(d.counters[23].name, Some("ash_xoff"));
    assert!(
        d.counters
            .iter()
            .any(|c| c.name == Some("nwk_frame_counter"))
    );
    assert_eq!(d.status.unwrap().network_state, 2);
    assert!(d.errors.is_empty(), "{:?}", d.errors);
    assert!(rig.commands().contains(&id::READ_AND_CLEAR_COUNTERS));
    rig.finish();
}

/// The tag of an admin ZDO request is free once its messageSentHandler
/// came; the response that ends the wait later must not touch the request
/// that took the tag meanwhile (EZSP 13: 8-bit tags wrap after 255 sends).
#[test]
fn a_zdo_wait_does_not_touch_a_request_that_reused_its_tag() {
    let mut rig = Rig::new(sim(13));
    rig.next_up();
    let admin = rig.admin.clone();
    let zdo = thread::spawn(move || {
        admin.zdo_request(0x1111, 0x0031, &[0x42, 0x00], Duration::from_secs(600))
    });
    let is_zdo =
        |c: &Command| matches!(c, Command::SendUnicast { aps, .. } if aps.cluster_id == 0x0031);
    rig.sim_until("ZDO sent", |n| n.sent().iter().any(|m| is_zdo(&m.command)));
    // Through the other 254 tags while the response is awaited.
    let done: Vec<RequestId> = (0..250).map(|_| rig.submit(unicast(0x1234))).collect();
    for id in &done {
        assert_eq!(rig.outcome(*id), Terminal::Delivered);
    }
    rig.sim.ncp(|n| n.hold_message_sent(true));
    let held: Vec<RequestId> = (0..20).map(|_| rig.submit(unicast(0x1234))).collect();
    rig.sim_until("held", |n| n.held_message_sent() == 20);
    let tags: Vec<u16> = rig
        .sent()
        .iter()
        .filter_map(|c| match c {
            Command::SendUnicast { tag, .. } => Some(*tag),
            _ => None,
        })
        .collect();
    let zdo_tag = tags[0];
    assert!(
        tags[tags.len() - 20..].contains(&zdo_tag),
        "the tag was taken again"
    );
    rig.inject(|n| incoming(n, 0x1111, 0, 0x8031, &[0x42, 0x00]));
    assert_eq!(zdo.join().unwrap(), Ok(vec![0x00]));
    rig.sim.ncp(|n| {
        n.hold_message_sent(false);
        n.release_message_sent(usize::MAX);
    });
    for id in &held {
        assert_eq!(rig.outcome(*id), Terminal::Delivered);
    }
    rig.finish();
}

/// Only NOT_JOINED says the NCP stores no network. Another networkInit
/// failure leaves the network unknown, and forming (which would overwrite
/// a network still in the NCP's NVM) is refused.
#[test]
fn a_failed_network_init_reports_unknown_and_refuses_to_form() {
    let mut rig = Rig::new(SimConfig {
        network_init_fails: true,
        ..sim(13)
    });
    rig.next_up();
    let st = rig.coord().status();
    assert_eq!(st.network_state, NetState::FormedDown);
    assert_eq!(st.ownership, NetworkOwnership::Unknown);
    assert!(
        rig.log
            .contains(&CoordEvent::Ownership(Box::new(NetworkOwnership::Unknown)))
    );
    let e = rig
        .admin
        .form(FormParams {
            channel_mask: 1 << 11,
            ..FormParams::default()
        })
        .unwrap_err();
    assert!(
        matches!(
            e.meaning(),
            CoordError::Backend {
                kind: Kind::InvalidState,
                ..
            }
        ),
        "{e:?}"
    );
    let c = rig.commands();
    assert!(!c.contains(&id::FORM_NETWORK) && !c.contains(&id::SET_INITIAL_SECURITY_STATE));
    rig.finish();
}

#[test]
fn leave_reset_clears_the_network_and_a_new_one_can_be_formed() {
    let mut rig = Rig::new(sim(14));
    rig.next_up();
    let add = rig.submit(Request::Group {
        endpoint: 1,
        group: 0x1001,
        add: true,
    });
    rig.outcome(add);
    rig.admin.leave_reset().unwrap();
    let st = rig.coord().status();
    assert_eq!(
        (st.network_state, st.network, st.ownership),
        (NetState::None, None, NetworkOwnership::NoNetwork)
    );
    rig.until("left", |l| {
        l.contains(&CoordEvent::Ownership(Box::new(
            NetworkOwnership::NoNetwork,
        )))
    });
    let leaving = position(&rig.log, |e| {
        *e == CoordEvent::NetworkState(NetChange::Leaving)
    })
    .unwrap();
    let net_down = position(&rig.log, |e| {
        *e == CoordEvent::NetworkState(NetChange::Down)
    })
    .unwrap();
    assert!(leaving < net_down);
    assert!(!rig.sim.ncp(|n| n.network_up()));
    assert_eq!(rig.sim.ncp(|n| n.multicast_table()[0].endpoint), 0);
    let p = rig.admin.form(FormParams {
        channel_mask: 1 << 11,
        ..FormParams::default()
    });
    assert_eq!(p.map(|p| p.channel), Ok(11));
    rig.finish();
}
