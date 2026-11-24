use super::codec::*;
use super::link::*;
use crate::sim::pipe::{Fault, Faults, Pipe};

fn hex(s: &str) -> Vec<u8> {
    s.split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect()
}

fn encode(f: &Frame, randomize_data: bool) -> Vec<u8> {
    let mut out = Vec::new();
    f.encode(randomize_data, &mut out);
    out
}

/// Control, data and CRC of the single frame in `wire` (stuffing removed).
fn unstuffed(wire: &[u8]) -> Vec<u8> {
    let mut d = Deframer::new();
    for &b in wire {
        if let Some(Deframed::Frame) = d.push(b) {
            let mut raw = d.frame().to_vec();
            raw.push(FLAG);
            return raw;
        }
    }
    panic!("no frame");
}

/// Deframes and decodes every frame in `wire`.
fn decode_all(wire: &[u8], randomize_data: bool) -> Vec<Result<Frame, FrameError>> {
    let mut d = Deframer::new();
    let mut out = Vec::new();
    for &b in wire {
        if let Some(Deframed::Frame) = d.push(b) {
            out.push(Frame::decode(d.frame(), randomize_data));
        }
    }
    out
}

fn data(frm_num: u8, ack_num: u8, re_tx: bool, payload: &[u8]) -> Frame {
    Frame::Data {
        frm_num,
        re_tx,
        ack_num,
        payload: payload.to_vec(),
    }
}

// UG101 §3 examples, verbatim.
const RST: &str = "C0 38 BC 7E";
const RSTACK: &str = "C1 02 02 9B 7B 7E";
const ACK1_READY: &str = "81 60 59 7E";
const ACK6_NOT_READY: &str = "8E 91 B6 7E";
const NAK6_READY: &str = "A6 34 DC 7E";
const NAK5_NOT_READY: &str = "AD 85 B7 7E";
// UG101 lists the non-randomized DATA examples without byte stuffing:
// 0x1A (Cancel, in the CRC) and 0x11 (XON, in the data) go on the wire as
// 7D 3A and 7D 31 (UG101 §4).
const DATA_2_5_PLAIN: &str = "25 00 00 00 02 1A AD 7E";
const DATA_2_5_PLAIN_STUFFED: &str = "25 00 00 00 02 7D 3A AD 7E";
const DATA_2_5_RANDOM: &str = "25 42 21 A8 56 A6 09 7E";
const DATA_5_3_PLAIN: &str = "53 00 80 00 02 02 11 30 63 16 7E";
const DATA_5_3_PLAIN_STUFFED: &str = "53 00 80 00 02 02 7D 31 30 63 16 7E";
const VERSION_CMD: &str = "00 00 00 02";
const VERSION_RSP: &str = "00 80 00 02 02 11 30";

#[test]
fn crc_and_randomization_match_ug101() {
    assert_eq!(crc16(&[0xC0]), 0x38BC);
    assert_eq!(crc16(&[0xC1, 0x02, 0x02]), 0x9B7B);
    let mut seq = [0u8; 5];
    randomize(&mut seq);
    assert_eq!(seq, [0x42, 0x21, 0xA8, 0x54, 0x2A]);
    let original: Vec<u8> = (0..=255).collect();
    let mut buf = original.clone();
    randomize(&mut buf);
    assert_ne!(buf, original);
    randomize(&mut buf);
    assert_eq!(buf, original);
}

#[test]
fn ug101_example_frames_encode_verbatim() {
    assert_eq!(encode(&Frame::Rst, true), hex(RST));
    assert_eq!(
        encode(
            &Frame::RstAck {
                version: 2,
                reset_code: 2
            },
            true
        ),
        hex(RSTACK)
    );
    assert_eq!(
        encode(
            &Frame::Ack {
                ack_num: 1,
                not_ready: false
            },
            true
        ),
        hex(ACK1_READY)
    );
    assert_eq!(
        encode(
            &Frame::Ack {
                ack_num: 6,
                not_ready: true
            },
            true
        ),
        hex(ACK6_NOT_READY)
    );
    assert_eq!(
        encode(
            &Frame::Nak {
                ack_num: 6,
                not_ready: false
            },
            true
        ),
        hex(NAK6_READY)
    );
    assert_eq!(
        encode(
            &Frame::Nak {
                ack_num: 5,
                not_ready: true
            },
            true
        ),
        hex(NAK5_NOT_READY)
    );
    let cmd = data(2, 5, false, &hex(VERSION_CMD));
    assert_eq!(unstuffed(&encode(&cmd, false)), hex(DATA_2_5_PLAIN));
    assert_eq!(encode(&cmd, false), hex(DATA_2_5_PLAIN_STUFFED));
    assert_eq!(encode(&cmd, true), hex(DATA_2_5_RANDOM));
    let rsp = data(5, 3, false, &hex(VERSION_RSP));
    assert_eq!(unstuffed(&encode(&rsp, false)), hex(DATA_5_3_PLAIN));
    assert_eq!(encode(&rsp, false), hex(DATA_5_3_PLAIN_STUFFED));
}

#[test]
fn ug101_example_frames_decode() {
    let cases: Vec<(&str, bool, Frame)> = vec![
        (RST, true, Frame::Rst),
        (
            RSTACK,
            true,
            Frame::RstAck {
                version: 2,
                reset_code: 2,
            },
        ),
        (
            ACK1_READY,
            true,
            Frame::Ack {
                ack_num: 1,
                not_ready: false,
            },
        ),
        (
            ACK6_NOT_READY,
            true,
            Frame::Ack {
                ack_num: 6,
                not_ready: true,
            },
        ),
        (
            NAK6_READY,
            true,
            Frame::Nak {
                ack_num: 6,
                not_ready: false,
            },
        ),
        (
            NAK5_NOT_READY,
            true,
            Frame::Nak {
                ack_num: 5,
                not_ready: true,
            },
        ),
        (
            DATA_2_5_PLAIN_STUFFED,
            false,
            data(2, 5, false, &hex(VERSION_CMD)),
        ),
        (DATA_2_5_RANDOM, true, data(2, 5, false, &hex(VERSION_CMD))),
        (
            DATA_5_3_PLAIN_STUFFED,
            false,
            data(5, 3, false, &hex(VERSION_RSP)),
        ),
    ];
    for (wire, rnd, expected) in cases {
        assert_eq!(decode_all(&hex(wire), rnd), vec![Ok(expected)], "{wire}");
    }
    // Verbatim, the unstuffed listings break: the Cancel Byte cuts the
    // first, the XON byte is dropped from the second.
    assert_eq!(
        decode_all(&hex(DATA_2_5_PLAIN), false),
        vec![Err(FrameError::TooShort)]
    );
    assert_eq!(
        decode_all(&hex(DATA_5_3_PLAIN), false),
        vec![Err(FrameError::BadCrc)]
    );
}

#[test]
fn ug101_inconsistent_examples_fail_the_crc() {
    // UG101 §3 ERROR example: the CRC of C2 01 52 is 0xCD8D, not FA BD.
    assert_eq!(
        decode_all(&hex("C2 01 52 FA BD 7E"), true),
        vec![Err(FrameError::BadCrc)]
    );
    // UG101 §3 randomized version response: the data bytes are right, the
    // CRC is not.
    let wire = hex("53 42 A1 A8 56 28 04 82 96 23 7E");
    assert_eq!(decode_all(&wire, true), vec![Err(FrameError::BadCrc)]);
    let mut field = wire[1..8].to_vec();
    randomize(&mut field);
    assert_eq!(field, hex(VERSION_RSP));
    assert_eq!(
        encode(&data(5, 3, false, &hex(VERSION_RSP)), true),
        hex("53 42 A1 A8 56 28 04 82 03 2A 7E")
    );
}

#[test]
fn byte_stuffing_matches_ug101_and_round_trips() {
    for (b, esc) in [
        (0x7E, 0x5E),
        (0x11, 0x31),
        (0x13, 0x33),
        (0x18, 0x38),
        (0x1A, 0x3A),
        (0x7D, 0x5D),
    ] {
        let mut out = Vec::new();
        stuff_byte(b, &mut out);
        assert_eq!(out, vec![0x7D, esc]);
    }
    let all: Vec<u8> = (0..=255).collect();
    for chunk in all.chunks(MAX_DATA_FIELD) {
        for rnd in [false, true] {
            let f = data(3, 4, true, chunk);
            let wire = encode(&f, rnd);
            assert_eq!(wire.iter().filter(|&&b| b == FLAG).count(), 1);
            assert!(
                wire[..wire.len() - 1]
                    .iter()
                    .all(|&b| !matches!(b, FLAG | XON | XOFF | SUBSTITUTE | CANCEL))
            );
            assert_eq!(decode_all(&wire, rnd), vec![Ok(f)]);
        }
    }
    let max = data(0, 0, false, &[0x7E; MAX_DATA_FIELD]);
    assert!(encode(&max, false).len() < MAX_ENCODED_FRAME);
}

#[test]
fn control_bytes_round_trip() {
    for b in 0..=255u8 {
        match Control::decode(b) {
            Some(c @ (Control::Ack { .. } | Control::Nak { .. })) => {
                assert_eq!(c.encode(), b & !0x10, "{b:02X}")
            }
            Some(c) => assert_eq!(c.encode(), b),
            None => assert!(b >= 0xC3, "{b:02X}"),
        }
    }
}

/// Control + data with a correct CRC, without frame-type checks.
fn encode_raw(body: &[u8]) -> Vec<u8> {
    let crc = crc16(body);
    let mut out = Vec::new();
    for &b in body.iter().chain(&crc.to_be_bytes()) {
        stuff_byte(b, &mut out);
    }
    out.push(FLAG);
    out
}

#[test]
fn deframer_handles_cancel_substitute_flow_control_and_noise() {
    let ack = hex(ACK1_READY);
    let ack1 = Ok(Frame::Ack {
        ack_num: 1,
        not_ready: false,
    });
    let mut wire = vec![0x12, 0x34, CANCEL];
    wire.extend(&ack);
    assert_eq!(decode_all(&wire, true), vec![ack1.clone()]);

    let mut d = Deframer::new();
    let mut wire = vec![0x81, SUBSTITUTE, 0x60, 0x59, FLAG];
    wire.extend(&ack);
    let seen: Vec<_> = wire.into_iter().filter_map(|b| d.push(b)).collect();
    assert_eq!(
        seen,
        vec![Deframed::Substituted, Deframed::Frame],
        "Substitute drops up to the next flag only"
    );

    let wire = vec![WAKE, FLAG, FLAG, 0x81, XON, 0x60, XOFF, 0x59, FLAG, FLAG];
    assert_eq!(
        decode_all(&wire, true),
        vec![ack1.clone()],
        "XON/XOFF, wake and repeated flags ignored"
    );

    let wire = vec![0x81, 0x60, 0x59, ESCAPE, FLAG];
    assert_eq!(
        decode_all(&wire, true),
        vec![ack1],
        "Escape before a reserved byte has no effect"
    );

    let mut d = Deframer::new();
    for _ in 0..=MAX_FRAME {
        assert_eq!(d.push(0x01), None);
    }
    assert_eq!(d.push(FLAG), Some(Deframed::TooLong));

    assert_eq!(
        decode_all(&[0x81, 0x60, FLAG], true),
        vec![Err(FrameError::TooShort)]
    );
    assert_eq!(
        decode_all(&encode_raw(&[0xC3]), true),
        vec![Err(FrameError::BadControl(0xC3))]
    );
    assert_eq!(
        decode_all(&encode_raw(&[0x81, 0x00]), true),
        vec![Err(FrameError::BadLength {
            control: 0x81,
            len: 1
        })]
    );
    assert_eq!(
        decode_all(&encode_raw(&[0x00, 0x00, 0x00]), true),
        vec![Err(FrameError::BadLength { control: 0, len: 2 })]
    );
}

// ---------------------------------------------------------------- link

fn transmit(ash: &mut Ash, now: u64) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    loop {
        let mut out = Vec::new();
        if !ash.poll_transmit(now, &mut out) {
            return frames;
        }
        frames.push(out);
    }
}

fn transmit_frames(ash: &mut Ash, now: u64) -> Vec<Frame> {
    transmit(ash, now)
        .iter()
        .flat_map(|w| decode_all(w, true))
        .map(Result::unwrap)
        .collect()
}

fn events(ash: &mut Ash) -> Vec<AshEvent> {
    std::iter::from_fn(|| ash.poll_event()).collect()
}

fn connected_host(cfg: AshConfig) -> Ash {
    let mut h = Ash::new(cfg).unwrap();
    h.reset(0);
    assert_eq!(
        transmit(&mut h, 0),
        vec![hex("1A C0 38 BC 7E")],
        "Cancel + RST (UG101 §4)"
    );
    feed(&mut h, &SOFTWARE_RSTACK, 5);
    assert_eq!(
        events(&mut h),
        vec![AshEvent::Connected {
            version: 2,
            reset_code: ResetCode::SOFTWARE
        }]
    );
    assert_eq!(h.state(), LinkState::Connected);
    h
}

/// The answer to RST: version 2, software reset (0x0B).
const SOFTWARE_RSTACK: Frame = Frame::RstAck {
    version: 2,
    reset_code: 0x0B,
};

fn feed(ash: &mut Ash, f: &Frame, now: u64) {
    ash.handle_rx(&encode(f, true), now);
}

const EZSP: [u8; 5] = [0x01, 0x80, 0x01, 0x05, 0x00];

#[test]
fn reset_discards_frames_before_rstack() {
    let mut h = Ash::new(AshConfig::default()).unwrap();
    h.reset(0);
    transmit(&mut h, 0);
    feed(&mut h, &data(0, 0, false, &EZSP), 1);
    feed(
        &mut h,
        &Frame::Ack {
            ack_num: 0,
            not_ready: false,
        },
        1,
    );
    h.handle_rx(&hex("12 34 56 7E"), 1);
    assert!(events(&mut h).is_empty());
    assert!(transmit(&mut h, 2).is_empty(), "no NAK while resetting");
    assert_eq!(h.stats().rx_discarded, 2);
    feed(&mut h, &SOFTWARE_RSTACK, 3);
    assert_eq!(
        events(&mut h),
        vec![AshEvent::Connected {
            version: 2,
            reset_code: ResetCode::SOFTWARE
        }]
    );
}

/// After nRESET or a power cycle the NCP sends RSTACK(external/power-on)
/// unprompted. Such an RSTACK, or any RSTACK before our RST went out, is
/// not the answer to our RST and must not connect the link.
#[test]
fn stale_rstack_is_ignored_while_resetting() {
    let mut h = Ash::new(AshConfig::default()).unwrap();
    h.reset(0);
    feed(&mut h, &SOFTWARE_RSTACK, 0);
    assert_eq!(h.state(), LinkState::Resetting, "RST not sent yet");
    transmit(&mut h, 0);
    h.handle_rx(&hex(RSTACK), 1);
    feed(
        &mut h,
        &Frame::RstAck {
            version: 2,
            reset_code: ResetCode::EXTERNAL.0,
        },
        2,
    );
    assert!(events(&mut h).is_empty());
    assert_eq!(h.state(), LinkState::Resetting);
    assert_eq!(h.stats().rx_stale_rstack, 3);
    assert_eq!(h.stats().rx_discarded, 3);
    feed(&mut h, &SOFTWARE_RSTACK, 3);
    assert_eq!(h.state(), LinkState::Connected);
    assert_eq!(
        events(&mut h),
        vec![AshEvent::Connected {
            version: 2,
            reset_code: ResetCode::SOFTWARE
        }]
    );
}

#[test]
fn reset_retries_five_times_then_fails() {
    let mut h = Ash::new(AshConfig::default()).unwrap();
    h.reset(0);
    let mut now = 0;
    let mut rsts = 0;
    while h.state() == LinkState::Resetting {
        rsts += transmit(&mut h, now).len();
        now = h.next_timeout().unwrap();
        h.handle_timeout(now);
    }
    assert_eq!(rsts, 6, "one RST + five retries (UG101 §5)");
    assert_eq!(now, 6 * 3200);
    assert_eq!(h.state(), LinkState::Failed(FailReason::ResetTimeout));
    assert_eq!(
        events(&mut h),
        vec![AshEvent::Failed(FailReason::ResetTimeout)]
    );
}

#[test]
fn rstack_with_wrong_version_fails() {
    let mut h = Ash::new(AshConfig::default()).unwrap();
    h.reset(0);
    transmit(&mut h, 0);
    feed(
        &mut h,
        &Frame::RstAck {
            version: 1,
            reset_code: 0x0B,
        },
        1,
    );
    assert_eq!(h.state(), LinkState::Failed(FailReason::AshVersion(1)));
}

#[test]
fn data_is_acknowledged_and_ack_time_adapts() {
    let mut h = connected_host(AshConfig::default());
    assert_eq!(h.send(&[0, 0, 0]), Ok(()));
    assert_eq!(
        transmit_frames(&mut h, 100),
        vec![data(0, 0, false, &[0, 0, 0])]
    );
    assert_eq!(h.next_timeout(), Some(100 + 1600));
    feed(
        &mut h,
        &Frame::Ack {
            ack_num: 1,
            not_ready: false,
        },
        200,
    );
    assert_eq!(h.tx_pending(), 0);
    assert_eq!(h.ack_timeout_ms(), 1450, "7/8 * 1600 + 100 / 2 (UG101 §5)");
    assert_eq!(h.next_timeout(), None);

    feed(&mut h, &data(0, 1, false, &EZSP), 300);
    assert_eq!(events(&mut h), vec![AshEvent::Data(EZSP.to_vec())]);
    assert_eq!(
        transmit_frames(&mut h, 300),
        vec![Frame::Ack {
            ack_num: 1,
            not_ready: false
        }]
    );
    assert_eq!(h.send(&[1, 2]), Err(SendError::InvalidLength(2)));
}

#[test]
fn lost_ack_retransmits_with_backoff_then_fails() {
    let mut h = connected_host(AshConfig::default());
    h.send(&EZSP).unwrap();
    assert_eq!(transmit_frames(&mut h, 0), vec![data(0, 0, false, &EZSP)]);
    let mut now = 0;
    let mut timeouts = Vec::new();
    for _ in 0..4 {
        now = h.next_timeout().unwrap();
        h.handle_timeout(now);
        assert_eq!(
            transmit_frames(&mut h, now),
            vec![data(0, 0, true, &EZSP)],
            "same frmNum, reTx set"
        );
        timeouts.push(h.ack_timeout_ms());
    }
    assert_eq!(timeouts, vec![3200; 4], "doubled, clamped to T_RX_ACK_MAX");
    feed(
        &mut h,
        &Frame::Ack {
            ack_num: 1,
            not_ready: false,
        },
        now + 10,
    );
    assert_eq!(
        h.ack_timeout_ms(),
        3200,
        "a retransmitted frame is not measured"
    );
    assert_eq!(h.state(), LinkState::Connected);

    h.send(&EZSP).unwrap();
    transmit(&mut h, now);
    for _ in 0..5 {
        now = h.next_timeout().unwrap();
        h.handle_timeout(now);
        transmit(&mut h, now);
    }
    assert_eq!(
        h.state(),
        LinkState::Failed(FailReason::AckTimeouts),
        "more than ACK_TIMEOUTS = 4"
    );
    assert_eq!(h.stats().ack_timeouts, 9);
}

#[test]
fn corrupt_frame_sets_reject_and_sends_one_nak() {
    let mut h = connected_host(AshConfig::default());
    let mut wire = encode(&data(0, 0, false, &EZSP), true);
    wire[2] ^= 0x04;
    h.handle_rx(&wire, 10);
    assert_eq!(
        transmit_frames(&mut h, 10),
        vec![Frame::Nak {
            ack_num: 0,
            not_ready: false
        }]
    );
    h.handle_rx(&wire, 11);
    assert!(
        transmit(&mut h, 11).is_empty(),
        "Reject Condition already set"
    );
    feed(&mut h, &data(0, 0, true, &EZSP), 12);
    assert_eq!(events(&mut h), vec![AshEvent::Data(EZSP.to_vec())]);
    assert_eq!(
        transmit_frames(&mut h, 12),
        vec![Frame::Ack {
            ack_num: 1,
            not_ready: false
        }]
    );
    assert_eq!(h.stats().rx_bad_crc, 2);
    h.handle_rx(&[0x10, SUBSTITUTE, 0x20, FLAG], 13);
    assert_eq!(
        transmit_frames(&mut h, 13),
        vec![Frame::Nak {
            ack_num: 1,
            not_ready: false
        }],
        "Substitute"
    );
}

#[test]
fn out_of_sequence_frame_is_naked_and_invalid_ack_num_rejected() {
    let mut h = connected_host(AshConfig::default());
    feed(&mut h, &data(1, 0, false, &EZSP), 1);
    assert!(events(&mut h).is_empty());
    assert_eq!(
        transmit_frames(&mut h, 1),
        vec![Frame::Nak {
            ack_num: 0,
            not_ready: false
        }]
    );
    assert_eq!(h.stats().rx_out_of_sequence, 1);

    let mut h = connected_host(AshConfig::default());
    feed(
        &mut h,
        &Frame::Ack {
            ack_num: 3,
            not_ready: false,
        },
        1,
    );
    assert_eq!(
        transmit_frames(&mut h, 1),
        vec![Frame::Nak {
            ack_num: 0,
            not_ready: false
        }]
    );
    assert_eq!(h.stats().rx_bad_ack_num, 1);
}

#[test]
fn retransmitted_duplicate_is_acked_not_delivered() {
    let mut h = connected_host(AshConfig::default());
    feed(&mut h, &data(0, 0, false, &EZSP), 1);
    assert_eq!(
        transmit_frames(&mut h, 1),
        vec![Frame::Ack {
            ack_num: 1,
            not_ready: false
        }]
    );
    feed(&mut h, &data(0, 0, true, &EZSP), 2);
    assert_eq!(
        events(&mut h),
        vec![AshEvent::Data(EZSP.to_vec())],
        "delivered once"
    );
    assert_eq!(
        transmit_frames(&mut h, 2),
        vec![Frame::Ack {
            ack_num: 1,
            not_ready: false
        }],
        "immediate ACK, no NAK"
    );
    assert_eq!(h.stats().rx_duplicate, 1);
    assert_eq!(h.stats().tx_nak, 0);
}

#[test]
fn nak_restarts_from_oldest_and_window_limits_flight() {
    let mut h = connected_host(AshConfig {
        tx_window: 3,
        tx_queue_frames: 4,
        ..AshConfig::default()
    });
    for i in 0..4u8 {
        h.send(&[i, 0, 0]).unwrap();
    }
    assert_eq!(h.send(&[9, 0, 0]), Err(SendError::QueueFull));
    let sent = transmit_frames(&mut h, 0);
    assert_eq!(
        sent,
        (0..3)
            .map(|i| data(i, 0, false, &[i, 0, 0]))
            .collect::<Vec<_>>(),
        "window of 3"
    );
    feed(
        &mut h,
        &Frame::Nak {
            ack_num: 1,
            not_ready: false,
        },
        5,
    );
    let resent = transmit_frames(&mut h, 5);
    assert_eq!(
        resent,
        vec![
            data(1, 0, true, &[1, 0, 0]),
            data(2, 0, true, &[2, 0, 0]),
            data(3, 0, false, &[3, 0, 0])
        ]
    );
    feed(
        &mut h,
        &Frame::Ack {
            ack_num: 4,
            not_ready: false,
        },
        6,
    );
    assert_eq!(h.tx_pending(), 0);
    h.send(&[4, 0, 0]).unwrap();
    transmit(&mut h, 7);
    feed(&mut h, &data(0, 5, false, &EZSP), 8);
    assert_eq!(
        h.tx_pending(),
        0,
        "an ackNum carried by DATA acknowledges too"
    );
}

#[test]
fn error_frame_and_unexpected_rstack_fail_the_link() {
    let mut h = connected_host(AshConfig::default());
    feed(
        &mut h,
        &Frame::Error {
            version: 2,
            code: 0x51,
        },
        1,
    );
    assert_eq!(
        h.state(),
        LinkState::Failed(FailReason::NcpError(ResetCode::EXCEEDED_MAX_ACK_TIMEOUTS))
    );
    assert_eq!(
        events(&mut h),
        vec![AshEvent::Failed(FailReason::NcpError(ResetCode(0x51)))]
    );

    let mut h = connected_host(AshConfig::default());
    feed(&mut h, &data(0, 0, false, &EZSP), 1);
    feed(
        &mut h,
        &Frame::RstAck {
            version: 2,
            reset_code: 0x03,
        },
        2,
    );
    assert_eq!(
        events(&mut h),
        vec![
            AshEvent::Data(EZSP.to_vec()),
            AshEvent::Failed(FailReason::NcpReset(ResetCode::WATCHDOG))
        ],
        "data received before the failure stays readable"
    );
    assert_eq!(h.send(&EZSP), Err(SendError::NotConnected));
}

#[test]
fn not_ready_is_advertised_refreshed_and_cleared() {
    let cfg = AshConfig {
        rx_queue_frames: 3,
        rx_not_ready_at: 2,
        ..AshConfig::default()
    };
    let mut h = connected_host(cfg);
    feed(&mut h, &data(0, 0, false, &EZSP), 0);
    assert_eq!(
        transmit_frames(&mut h, 0),
        vec![Frame::Ack {
            ack_num: 1,
            not_ready: false
        }]
    );
    feed(&mut h, &data(1, 0, false, &EZSP), 10);
    assert_eq!(
        transmit_frames(&mut h, 10),
        vec![Frame::Ack {
            ack_num: 2,
            not_ready: true
        }]
    );
    assert_eq!(h.next_timeout(), Some(510));
    assert!(transmit(&mut h, 400).is_empty());
    assert_eq!(
        transmit_frames(&mut h, 510),
        vec![Frame::Ack {
            ack_num: 2,
            not_ready: true
        }],
        "T_LOCAL_NOTRDY refresh"
    );
    feed(&mut h, &data(2, 0, false, &EZSP), 600);
    feed(&mut h, &data(3, 0, false, &EZSP), 601);
    assert_eq!(
        h.stats().rx_no_space,
        1,
        "fourth frame discarded for lack of memory"
    );
    assert_eq!(
        transmit_frames(&mut h, 601),
        vec![Frame::Nak {
            ack_num: 3,
            not_ready: true
        }]
    );
    assert_eq!(events(&mut h).len(), 3);
    assert_eq!(
        transmit_frames(&mut h, 602),
        vec![Frame::Ack {
            ack_num: 3,
            not_ready: false
        }],
        "ready again"
    );
    h.set_not_ready(true);
    assert_eq!(
        transmit_frames(&mut h, 603),
        vec![Frame::Ack {
            ack_num: 3,
            not_ready: true
        }]
    );
}

/// Host and NCP links joined by two faulty pipes on a virtual clock.
struct Pair {
    host: Ash,
    ncp: Ash,
    to_ncp: Pipe,
    to_host: Pipe,
    now: u64,
    host_rx: Vec<Vec<u8>>,
    ncp_rx: Vec<Vec<u8>>,
    host_events: Vec<AshEvent>,
}

impl Pair {
    fn new(faults: Faults, host_cfg: AshConfig) -> Pair {
        let ncp_cfg = AshConfig {
            tx_window: 5,
            tx_queue_frames: 64,
            max_ack_timeouts: 0,
            ..AshConfig::default()
        };
        Pair {
            host: Ash::new(host_cfg).unwrap(),
            ncp: Ash::new_ncp(ncp_cfg).unwrap(),
            to_ncp: Pipe::new(faults),
            to_host: Pipe::new(Faults {
                seed: faults.seed.wrapping_mul(31).wrapping_add(7),
                ..faults
            }),
            now: 0,
            host_rx: Vec::new(),
            ncp_rx: Vec::new(),
            host_events: Vec::new(),
        }
    }

    fn step(&mut self) {
        let now = self.now;
        let mut buf = [0u8; 512];
        let n = self.to_host.read(now, &mut buf);
        self.host.handle_rx(&buf[..n], now);
        let n = self.to_ncp.read(now, &mut buf);
        self.ncp.handle_rx(&buf[..n], now);
        self.host.handle_timeout(now);
        self.ncp.handle_timeout(now);
        for w in transmit(&mut self.host, now) {
            self.to_ncp.push(&w, now);
        }
        for w in transmit(&mut self.ncp, now) {
            self.to_host.push(&w, now);
        }
        while let Some(ev) = self.host.poll_event() {
            match ev {
                AshEvent::Data(d) => self.host_rx.push(d),
                other => self.host_events.push(other),
            }
        }
        while let Some(ev) = self.ncp.poll_event() {
            if let AshEvent::Data(d) = ev {
                self.ncp_rx.push(d);
            }
        }
        let next = [
            self.to_host.next_due(),
            self.to_ncp.next_due(),
            self.host.next_timeout(),
            self.ncp.next_timeout(),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(now + 10);
        self.now = next.max(now + 1);
    }

    fn run_until(&mut self, limit: u64, mut done: impl FnMut(&Pair) -> bool) {
        while !done(self) {
            assert!(self.now < limit, "not done by {limit} ms");
            self.step();
        }
    }

    fn connect(&mut self) {
        self.host.reset(self.now);
        let start = self.host_events.len();
        let limit = self.now + 60_000;
        self.run_until(limit, |p| {
            p.host_events[start..]
                .iter()
                .any(|e| matches!(e, AshEvent::Connected { .. }))
        });
    }
}

fn payload(tag: u8, i: u16) -> Vec<u8> {
    let mut p = vec![tag, (i >> 8) as u8, i as u8];
    p.extend((0..(i % 40) as u8).map(|k| k.wrapping_mul(0x1B) ^ 0x7E));
    p
}

/// Traffic in both directions under drop/corrupt/duplicate/delay faults:
/// every frame arrives exactly once and in order (UG101 §5 recovery).
#[test]
fn lossy_link_delivers_everything_once_in_order() {
    const N: u16 = 60;
    for seed in 1..=4u64 {
        let faults = Faults::uniform(seed, 40, 2500);
        let mut p = Pair::new(
            faults,
            AshConfig {
                max_ack_timeouts: 0,
                ..AshConfig::default()
            },
        );
        p.connect();
        let (mut h_sent, mut n_sent) = (0u16, 0u16);
        while p.host_rx.len() < N as usize || p.ncp_rx.len() < N as usize {
            assert!(p.now < 3_000_000, "seed {seed}: stalled");
            while h_sent < N && p.host.send(&payload(0xA0, h_sent)).is_ok() {
                h_sent += 1;
            }
            while n_sent < N
                && p.ncp
                    .send_ncp(&payload(0xB0, n_sent), n_sent % 2 == 0)
                    .is_ok()
            {
                n_sent += 1;
            }
            p.step();
        }
        let expect = |tag| (0..N).map(|i| payload(tag, i)).collect::<Vec<_>>();
        assert_eq!(p.ncp_rx, expect(0xA0), "seed {seed}");
        assert_eq!(p.host_rx, expect(0xB0), "seed {seed}");
        let (a, b) = (p.to_ncp.stats(), p.to_host.stats());
        assert!(
            a.dropped + a.corrupted + b.dropped + b.corrupted > 0,
            "seed {seed}: faults were injected"
        );
        assert!(
            a.duplicated + b.duplicated > 0 && a.delayed + b.delayed > 0,
            "seed {seed}"
        );
        assert!(p.host.stats().tx_retransmit + p.ncp.stats().tx_retransmit > 0);
    }
}

#[test]
fn host_reset_mid_stream_restarts_frame_numbers() {
    let mut p = Pair::new(Faults::none(), AshConfig::default());
    p.connect();
    for i in 0..5 {
        p.host.send(&payload(1, i)).unwrap();
        p.ncp.send_ncp(&payload(2, i), true).unwrap();
        p.run_until(p.now + 10_000, |p| {
            p.host.tx_pending() == 0 && p.ncp.tx_pending() == 0
        });
    }
    assert_eq!(p.host_rx.len(), 5);
    p.host.send(&payload(1, 99)).unwrap();
    p.ncp.send_ncp(&payload(2, 99), false).unwrap();
    p.to_ncp.inject(Fault::Drop);
    p.step();
    p.connect();
    assert_eq!(
        p.host.tx_pending(),
        0,
        "frames in flight are dropped by the reset"
    );
    p.host.send(&payload(1, 100)).unwrap();
    let mut out = Vec::new();
    assert!(p.host.poll_transmit(p.now, &mut out));
    assert_eq!(
        decode_all(&out, true),
        vec![Ok(data(0, 0, false, &payload(1, 100)))],
        "numbering restarts at 0"
    );
    p.to_ncp.push(&out, p.now);
    p.run_until(p.now + 10_000, |p| {
        p.ncp_rx.last() == Some(&payload(1, 100))
    });
    assert!(!p.ncp_rx.contains(&payload(1, 99)));
    assert!(
        !p.host_rx.contains(&payload(2, 99)),
        "frames before RSTACK are discarded"
    );
}

#[test]
fn ncp_role_pauses_callbacks_while_host_not_ready() {
    let mut p = Pair::new(Faults::none(), AshConfig::default());
    p.connect();
    p.host.set_not_ready(true);
    p.host.send(&payload(1, 0)).unwrap();
    p.run_until(p.now + 5_000, |p| p.ncp_rx.len() == 1);
    p.ncp.send_ncp(&payload(3, 0), true).unwrap();
    p.ncp.send_ncp(&payload(4, 0), false).unwrap();
    let t = p.now + 3_000;
    p.run_until(t + 1, |p| p.now >= t);
    assert_eq!(
        p.host_rx,
        vec![payload(4, 0)],
        "responses pass, callbacks wait (UG101 §5)"
    );
    p.host.set_not_ready(false);
    p.run_until(p.now + 5_000, |p| p.host_rx.len() == 2);
    assert_eq!(p.host_rx[1], payload(3, 0));
}

#[test]
fn config_validation() {
    assert!(
        Ash::new(AshConfig {
            tx_window: 0,
            ..AshConfig::default()
        })
        .is_err()
    );
    assert!(
        Ash::new(AshConfig {
            tx_window: 8,
            ..AshConfig::default()
        })
        .is_err()
    );
    assert!(
        Ash::new(AshConfig {
            tx_window: 7,
            ..AshConfig::default()
        })
        .is_ok()
    );
    assert!(
        Ash::new(AshConfig {
            ack_timeout_init_ms: 100,
            ..AshConfig::default()
        })
        .is_err()
    );
    assert!(
        Ash::new(AshConfig {
            rx_not_ready_at: 9,
            ..AshConfig::default()
        })
        .is_err()
    );
}
