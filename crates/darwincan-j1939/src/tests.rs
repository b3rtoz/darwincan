// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;
use darwincan_core::j1939::{IDLE_ADDR, NO_PGN};

/// Two stacks on one bus, with a clock that jumps from one thing to do to
/// the next. Every frame takes a millisecond.
struct Net {
    nodes: [J1939; 2],
    now: u64,
    /// Every frame that was on the bus: when, from which node, the frame.
    log: Vec<(u64, usize, Frame)>,
    /// Drop the frames for which this says so, as a bus fault would.
    lose: Box<dyn FnMut(&Frame) -> bool>,
}

const FRAME_NS: u64 = MS;

impl Net {
    fn new() -> Net {
        Net {
            nodes: [J1939::new(), J1939::new()],
            now: 1_000 * MS,
            log: Vec::new(),
            lose: Box::new(|_| false),
        }
    }

    /// Let things happen until `until`, or until nothing is left to do.
    fn run(&mut self, until: u64) {
        loop {
            let mut sent = false;
            for n in 0..2 {
                if let Some((frame, socket)) = self.nodes[n].poll_transmit(self.now) {
                    self.now += FRAME_NS;
                    sent = true;
                    self.log.push((self.now, n, frame));
                    let lost = (self.lose)(&frame);
                    self.nodes[n].on_frame(&frame, self.now, self.now, Origin::Local(socket));
                    if !lost {
                        self.nodes[1 - n].on_frame(&frame, self.now, self.now, Origin::Bus);
                    }
                }
            }
            if sent {
                continue;
            }
            let next = self.nodes.iter().filter_map(|n| n.next_deadline()).min();
            match next {
                Some(at) if at <= until => {
                    self.now = self.now.max(at);
                    for n in &mut self.nodes {
                        n.poll(self.now);
                    }
                }
                _ => return,
            }
        }
    }

    fn settle(&mut self) {
        self.run(u64::MAX);
    }

    fn outputs(&mut self, node: usize) -> Vec<(SocketId, Output)> {
        std::iter::from_fn(|| self.nodes[node].poll_output()).collect()
    }

    fn messages(&mut self, node: usize) -> Vec<(SocketId, J1939Message)> {
        self.outputs(node)
            .into_iter()
            .filter_map(|(id, out)| match out {
                Output::Message(m) => Some((id, m)),
                Output::Notice(_) => None,
            })
            .collect()
    }

    fn notices(&mut self, node: usize) -> Vec<(SocketId, J1939Notice)> {
        self.outputs(node)
            .into_iter()
            .filter_map(|(id, out)| match out {
                Output::Notice(n) => Some((id, n)),
                Output::Message(_) => None,
            })
            .collect()
    }
}

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
}

/// Node 0 has socket 1 at address 0x80, node 1 socket 2 at 0x90.
fn pair() -> Net {
    let mut net = Net::new();
    net.nodes[0].open(1, J1939Addr::address(0x80)).unwrap();
    net.nodes[1].open(2, J1939Addr::address(0x90)).unwrap();
    net
}

#[test]
fn identifiers_are_built_and_read_as_j1939_has_them() {
    // Priority 6, PGN 0xFEF1 (PDU2), source 0x80.
    let f = frame(6, 0xFEF1, 0x12, 0x80, &[1]);
    assert_eq!(f.can_id() & CAN_EFF_MASK, 0x18FE_F180);
    let h = parse(&f).unwrap();
    assert_eq!((h.priority, h.pgn, h.da, h.sa), (6, 0xFEF1, NO_ADDR, 0x80));
    // Priority 3, PGN 0xEA00 (PDU1) to 0x90.
    let f = frame(3, 0xEA00, 0x90, 0x80, &[1]);
    assert_eq!(f.can_id() & CAN_EFF_MASK, 0x0CEA_9080);
    let h = parse(&f).unwrap();
    assert_eq!((h.priority, h.pgn, h.da, h.sa), (3, 0xEA00, 0x90, 0x80));
    // The data page bit is part of the group.
    let f = frame(7, 0x1FEF1, NO_ADDR, 1, &[]);
    assert_eq!(f.can_id() & CAN_EFF_MASK, 0x1DFE_F101);
    assert_eq!(parse(&f).unwrap().pgn, 0x1FEF1);
    // Not J1939: an 11-bit identifier.
    assert!(parse(&Frame::new(Id::Standard(0x123), &[]).unwrap()).is_none());
}

#[test]
fn a_short_message_is_one_frame_to_its_destination() {
    let mut net = pair();
    net.nodes[0]
        .send(
            1,
            Some(J1939Addr::new(0x90, 0xEF00)),
            None,
            vec![1, 2, 3],
            net.now,
        )
        .unwrap();
    net.settle();
    assert_eq!(net.log.len(), 1);
    assert_eq!(net.log[0].2.can_id() & CAN_EFF_MASK, 0x18EF_9080);
    assert_eq!(net.log[0].2.data(), &[1, 2, 3]);
    let got = net.messages(1);
    assert_eq!(got.len(), 1);
    let (to, m) = &got[0];
    assert_eq!(*to, 2);
    assert_eq!(m.src, J1939Addr::new(0x80, 0xEF00));
    assert_eq!((m.dst_addr, m.priority), (0x90, 6));
    assert_eq!(m.data, [1, 2, 3]);
    assert_eq!(m.timestamp_ns, net.log[0].0);
    // The sender does not get its own message.
    assert!(net.messages(0).is_empty());
}

#[test]
fn only_the_socket_addressed_receives() {
    let mut net = pair();
    net.nodes[1].open(3, J1939Addr::address(0x91)).unwrap();
    net.nodes[1].open(4, J1939Addr::NONE).unwrap();
    net.nodes[1].open(5, J1939Addr::NONE).unwrap();
    net.nodes[1].set_promisc(5, true).unwrap();
    net.nodes[0]
        .send(
            1,
            Some(J1939Addr::new(0x90, 0xEF00)),
            None,
            vec![1],
            net.now,
        )
        .unwrap();
    net.settle();
    let to: Vec<SocketId> = net.messages(1).iter().map(|(id, _)| *id).collect();
    assert_eq!(to, [2, 5]);
}

#[test]
fn broadcasts_need_allowing_both_ways() {
    let mut net = pair();
    let everyone = J1939Addr::broadcast(0xFEF1);
    assert_eq!(
        net.nodes[0].send(1, Some(everyone), None, vec![1], net.now),
        Err(SendError::Broadcast)
    );
    net.nodes[0].set_broadcast(1, true).unwrap();
    net.nodes[0]
        .send(1, Some(everyone), None, vec![1], net.now)
        .unwrap();
    net.settle();
    assert!(net.messages(1).is_empty());
    net.nodes[1].set_broadcast(2, true).unwrap();
    net.nodes[0]
        .send(1, Some(everyone), Some(3), vec![2], net.now)
        .unwrap();
    net.settle();
    let got = net.messages(1);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1.dst_addr, NO_ADDR);
    assert_eq!(got[0].1.priority, 3);
    assert_eq!(got[0].1.src.pgn, 0xFEF1);
}

#[test]
fn bind_connect_and_filters_narrow_what_is_received() {
    let mut net = pair();
    net.nodes[0].open(10, J1939Addr::address(0x81)).unwrap();
    // Socket 2 takes one group only; 3 only from 0x81; 4 by filter.
    net.nodes[1].bind(2, J1939Addr::new(0x90, 0xEF00)).unwrap();
    net.nodes[1].open(3, J1939Addr::address(0x90)).unwrap();
    net.nodes[1]
        .connect(3, J1939Addr::new(0x81, 0xEF00))
        .unwrap();
    net.nodes[1].open(4, J1939Addr::address(0x90)).unwrap();
    net.nodes[1]
        .set_filters(4, vec![J1939Filter::pgn(0xAA00), J1939Filter::pgn(0xBB00)])
        .unwrap();
    for (socket, pgn) in [(1, 0xEF00), (1, 0xAA00), (10, 0xEF00), (10, 0xBB00)] {
        net.nodes[0]
            .send(
                socket,
                Some(J1939Addr::new(0x90, pgn)),
                None,
                vec![0],
                net.now,
            )
            .unwrap();
    }
    net.settle();
    let got: Vec<(SocketId, u8, u32)> = net
        .messages(1)
        .iter()
        .map(|(id, m)| (*id, m.src.addr, m.src.pgn))
        .collect();
    assert_eq!(
        got,
        [
            (2, 0x80, 0xEF00),
            (4, 0x80, 0xAA00),
            (2, 0x81, 0xEF00),
            (3, 0x81, 0xEF00),
            (3, 0x81, 0xBB00),
            (4, 0x81, 0xBB00),
        ]
    );
    // A connected socket sends to its peer without naming it.
    net.nodes[1].send(3, None, None, vec![9], net.now).unwrap();
    net.settle();
    assert_eq!(
        net.log.last().unwrap().2.can_id() & CAN_EFF_MASK,
        0x18EF_8190
    );
}

#[test]
fn sends_that_cannot_be_made_are_refused() {
    let mut net = pair();
    net.nodes[0].open(7, J1939Addr::NONE).unwrap();
    let to = Some(J1939Addr::new(0x90, 0xEF00));
    assert_eq!(
        net.nodes[0].send(7, to, None, vec![1], net.now),
        Err(SendError::NoSource)
    );
    assert_eq!(
        net.nodes[0].send(1, None, None, vec![1], net.now),
        Err(SendError::NoDestination)
    );
    assert_eq!(
        net.nodes[0].send(
            1,
            Some(J1939Addr::new(0x90, 0xEF12)),
            None,
            vec![1],
            net.now
        ),
        Err(SendError::BadPgn)
    );
    assert_eq!(
        net.nodes[0].send(
            1,
            Some(J1939Addr::new(0x90, NO_PGN)),
            None,
            vec![1],
            net.now
        ),
        Err(SendError::BadPgn)
    );
    net.nodes[0].set_broadcast(1, true).unwrap();
    assert_eq!(
        net.nodes[0].send(
            1,
            Some(J1939Addr::broadcast(0xFEF1)),
            None,
            vec![0; MAX_TP_SIZE + 1],
            net.now
        ),
        Err(SendError::TooBig)
    );
    let named = J1939Addr {
        name: 0x1234,
        ..J1939Addr::new(NO_ADDR, 0xEF00)
    };
    assert_eq!(
        net.nodes[0].send(1, Some(named), None, vec![1], net.now),
        Err(SendError::AddressNotAvailable)
    );
    assert!(net.nodes[0].bind(1, J1939Addr::new(0x80, 0xEF12)).is_err());
    assert!(net.nodes[0].set_priority(1, 8).is_err());
    net.settle();
    assert!(net.log.is_empty());
}

#[test]
fn a_broadcast_of_many_bytes_is_announced_and_paced() {
    let mut net = pair();
    net.nodes[0].set_broadcast(1, true).unwrap();
    net.nodes[0].set_notices(1, true).unwrap();
    net.nodes[1].set_broadcast(2, true).unwrap();
    let data = payload(20);
    net.nodes[0]
        .send(
            1,
            Some(J1939Addr::broadcast(0xFEB0)),
            None,
            data.clone(),
            net.now,
        )
        .unwrap();
    net.settle();
    assert_eq!(net.log.len(), 4);
    // BAM: 20 bytes, 3 packets, PGN 0xFEB0, to everyone at priority 7.
    assert_eq!(net.log[0].2.can_id() & CAN_EFF_MASK, 0x1CEC_FF80);
    assert_eq!(net.log[0].2.data(), &[32, 20, 0, 3, 0xFF, 0xB0, 0xFE, 0]);
    assert_eq!(net.log[1].2.can_id() & CAN_EFF_MASK, 0x1CEB_FF80);
    assert_eq!(net.log[1].2.data()[0], 1);
    // The last packet is filled up with FF.
    assert_eq!(
        net.log[3].2.data(),
        &[3, data[14], data[15], data[16], data[17], data[18], data[19], 0xFF]
    );
    for pair in net.log.windows(2) {
        let gap = pair[1].0 - pair[0].0;
        assert!(gap >= BAM_GAP_NS, "packets {gap} ns apart");
        assert!(gap <= BAM_GAP_NS + 2 * FRAME_NS, "packets {gap} ns apart");
    }
    let got = net.messages(1);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1.data, data);
    assert_eq!(got[0].1.src, J1939Addr::new(0x80, 0xFEB0));
    assert_eq!(got[0].1.dst_addr, NO_ADDR);
    assert_eq!(got[0].1.timestamp_ns, net.log[3].0);
    let notices = net.notices(0);
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].1.kind, J1939NoticeKind::TxDone);
    assert_eq!(notices[0].1.size, 20);
}

#[test]
fn a_broadcast_with_a_packet_missing_is_not_delivered() {
    let mut net = pair();
    net.nodes[0].set_broadcast(1, true).unwrap();
    net.nodes[1].set_broadcast(2, true).unwrap();
    net.nodes[1].set_notices(2, true).unwrap();
    net.lose = Box::new(|f| f.can_id() & CAN_EFF_MASK == 0x1CEB_FF80 && f.data()[0] == 2);
    net.nodes[0]
        .send(
            1,
            Some(J1939Addr::broadcast(0xFEB0)),
            None,
            payload(30),
            net.now,
        )
        .unwrap();
    net.settle();
    assert!(net.nodes[1].rx.is_empty());
    let out = net.outputs(1);
    let kinds: Vec<J1939NoticeKind> = out
        .iter()
        .map(|(_, o)| match o {
            Output::Notice(n) => n.kind,
            Output::Message(_) => panic!("a message with a hole in it"),
        })
        .collect();
    assert_eq!(
        kinds,
        [J1939NoticeKind::RxStarted, J1939NoticeKind::RxAborted]
    );
}

#[test]
fn a_long_message_to_one_address_is_a_connection() {
    for len in [9, 14, 15, 1785] {
        let mut net = pair();
        net.nodes[0].set_notices(1, true).unwrap();
        net.nodes[1].set_notices(2, true).unwrap();
        let data = payload(len);
        net.nodes[0]
            .send(
                1,
                Some(J1939Addr::new(0x90, 0xEF00)),
                None,
                data.clone(),
                net.now,
            )
            .unwrap();
        net.settle();
        let packets = len.div_ceil(7);
        // Request, clear-to-send, the packets, the acknowledgement.
        assert_eq!(net.log.len(), packets + 3, "{len} bytes");
        let rts = net.log[0].2;
        assert_eq!(rts.can_id() & CAN_EFF_MASK, 0x1CEC_9080);
        assert_eq!(
            rts.data(),
            &[
                16,
                len as u8,
                (len >> 8) as u8,
                packets as u8,
                0xFF,
                0,
                0xEF,
                0
            ]
        );
        let cts = net.log[1].2;
        assert_eq!(cts.can_id() & CAN_EFF_MASK, 0x1CEC_8090);
        assert_eq!(cts.data(), &[17, packets as u8, 1, 0xFF, 0xFF, 0, 0xEF, 0]);
        let eoma = net.log.last().unwrap().2;
        assert_eq!(eoma.can_id() & CAN_EFF_MASK, 0x1CEC_8090);
        assert_eq!(eoma.data()[0], 19);
        let out = net.outputs(1);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0].1, Output::Notice(n)
            if n.kind == J1939NoticeKind::RxStarted && n.size as usize == len));
        match &out[1].1 {
            Output::Message(m) => {
                assert_eq!(m.data, data);
                assert_eq!(m.src, J1939Addr::new(0x80, 0xEF00));
                assert_eq!(m.dst_addr, 0x90);
            }
            other => panic!("{other:?}"),
        }
        let notices = net.notices(0);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].1.kind, J1939NoticeKind::TxDone);
        assert!(net.nodes[0].tx.is_empty() && net.nodes[1].rx.is_empty());
    }
}

#[test]
fn a_sender_that_limits_the_block_is_asked_in_blocks() {
    let mut net = pair();
    // A request to send from the bus: 30 bytes in 5 packets, 2 at a time.
    let rts = frame(7, PGN_TP_CM, 0x80, 0x33, &[16, 30, 0, 5, 2, 0, 0xEF, 0]);
    net.nodes[0].on_frame(&rts, net.now, net.now, Origin::Bus);
    let (cts, _) = net.nodes[0].poll_transmit(net.now).unwrap();
    assert_eq!(cts.data(), &[17, 2, 1, 0xFF, 0xFF, 0, 0xEF, 0]);
    let data = payload(30);
    let packet = |net: &mut Net, n: u8| {
        let mut d = [0xFF; 8];
        d[0] = n;
        let start = (usize::from(n) - 1) * 7;
        let end = (start + 7).min(30);
        d[1..1 + end - start].copy_from_slice(&data[start..end]);
        net.nodes[0].on_frame(
            &frame(7, PGN_TP_DT, 0x80, 0x33, &d),
            net.now,
            net.now,
            Origin::Bus,
        );
    };
    packet(&mut net, 1);
    assert!(net.nodes[0].poll_transmit(net.now).is_none());
    packet(&mut net, 2);
    let (cts, _) = net.nodes[0].poll_transmit(net.now).unwrap();
    assert_eq!(cts.data(), &[17, 2, 3, 0xFF, 0xFF, 0, 0xEF, 0]);
    packet(&mut net, 3);
    packet(&mut net, 4);
    let (cts, _) = net.nodes[0].poll_transmit(net.now).unwrap();
    assert_eq!(cts.data(), &[17, 1, 5, 0xFF, 0xFF, 0, 0xEF, 0]);
    packet(&mut net, 5);
    let (eoma, _) = net.nodes[0].poll_transmit(net.now).unwrap();
    assert_eq!(eoma.data(), &[19, 30, 0, 5, 0xFF, 0, 0xEF, 0]);
    let got = net.messages(0);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1.data, payload(30));
}

#[test]
fn a_hold_is_waited_out_and_a_block_can_be_asked_again() {
    let mut net = pair();
    net.nodes[0].set_notices(1, true).unwrap();
    net.nodes[0]
        .send(
            1,
            Some(J1939Addr::new(0x55, 0xEF00)),
            None,
            payload(21),
            net.now,
        )
        .unwrap();
    let (rts, socket) = net.nodes[0].poll_transmit(net.now).unwrap();
    assert_eq!(rts.data()[0], 16);
    assert_eq!(socket, Some(1));
    let cts = |n: u8, first: u8| {
        frame(
            7,
            PGN_TP_CM,
            0x80,
            0x55,
            &[17, n, first, 0xFF, 0xFF, 0, 0xEF, 0],
        )
    };
    // Hold: nothing goes, and the wait is the longer one.
    let t0 = net.now;
    net.nodes[0].on_frame(&cts(0, 0xFF), t0, t0, Origin::Bus);
    assert!(net.nodes[0].poll_transmit(t0).is_none());
    assert_eq!(net.nodes[0].next_deadline(), Some(t0 + T4_NS));
    // Two packets, then the first two again, then the last.
    let seqs = |net: &mut Net| -> Vec<u8> {
        std::iter::from_fn(|| net.nodes[0].poll_transmit(net.now))
            .map(|(f, _)| f.data()[0])
            .collect()
    };
    net.nodes[0].on_frame(&cts(2, 1), t0, t0, Origin::Bus);
    assert_eq!(seqs(&mut net), [1, 2]);
    net.nodes[0].on_frame(&cts(2, 1), t0, t0, Origin::Bus);
    assert_eq!(seqs(&mut net), [1, 2]);
    net.nodes[0].on_frame(&cts(1, 3), t0, t0, Origin::Bus);
    assert_eq!(seqs(&mut net), [3]);
    let eoma = frame(7, PGN_TP_CM, 0x80, 0x55, &[19, 21, 0, 3, 0xFF, 0, 0xEF, 0]);
    net.nodes[0].on_frame(&eoma, t0, t0, Origin::Bus);
    let notices = net.notices(0);
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].1.kind, J1939NoticeKind::TxDone);
}

#[test]
fn a_connection_nobody_answers_times_out() {
    let mut net = pair();
    net.nodes[0].set_notices(1, true).unwrap();
    let start = net.now;
    net.nodes[0]
        .send(
            1,
            Some(J1939Addr::new(0x55, 0xEF00)),
            None,
            payload(100),
            net.now,
        )
        .unwrap();
    net.settle();
    assert_eq!(net.log.len(), 2);
    assert_eq!(
        net.log[1].2.data(),
        &[255, abort::TIMEOUT, 0xFF, 0xFF, 0xFF, 0, 0xEF, 0]
    );
    assert!(net.log[1].0 >= start + T3_NS);
    let notices = net.notices(0);
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].1.kind, J1939NoticeKind::TxAborted);
    assert_eq!(notices[0].1.reason, abort::TIMEOUT);
}

#[test]
fn a_receiver_gives_up_when_the_data_stops() {
    let mut net = pair();
    net.nodes[0].set_notices(1, true).unwrap();
    net.nodes[1].set_notices(2, true).unwrap();
    // The fourth packet never arrives.
    net.lose = Box::new(|f| f.can_id() & CAN_EFF_MASK == 0x1CEB_9080 && f.data()[0] == 4);
    net.nodes[0]
        .send(
            1,
            Some(J1939Addr::new(0x90, 0xEF00)),
            None,
            payload(100),
            net.now,
        )
        .unwrap();
    net.settle();
    let aborts: Vec<&(u64, usize, Frame)> = net
        .log
        .iter()
        .filter(|(_, _, f)| f.data()[0] == 255 && (f.can_id() >> 16) & 0xFF == 0xEC)
        .collect();
    // The receiver says the sequence is wrong when the fifth comes.
    assert_eq!(aborts.len(), 1);
    assert_eq!(aborts[0].1, 1);
    assert_eq!(aborts[0].2.data()[1], abort::BAD_SEQUENCE);
    assert!(net.messages(1).is_empty());
    let notices = net.notices(0);
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].1.kind, J1939NoticeKind::TxAborted);
    assert_eq!(notices[0].1.reason, abort::BAD_SEQUENCE);
    assert!(net.nodes[0].tx.is_empty() && net.nodes[1].rx.is_empty());
}

#[test]
fn a_receiver_times_out_on_silence() {
    let mut net = pair();
    net.nodes[1].set_notices(2, true).unwrap();
    let rts = frame(7, PGN_TP_CM, 0x90, 0x33, &[16, 30, 0, 5, 0xFF, 0, 0xEF, 0]);
    let t0 = net.now;
    net.nodes[1].on_frame(&rts, t0, t0, Origin::Bus);
    net.settle();
    let last = net.log.last().unwrap();
    assert_eq!(
        last.2.data(),
        &[255, abort::TIMEOUT, 0xFF, 0xFF, 0xFF, 0, 0xEF, 0]
    );
    assert_eq!(last.2.can_id() & CAN_EFF_MASK, 0x1CEC_3390);
    assert!(last.0 >= t0 + T2_NS);
    let kinds: Vec<J1939NoticeKind> = net.notices(1).iter().map(|(_, n)| n.kind).collect();
    assert_eq!(
        kinds,
        [J1939NoticeKind::RxStarted, J1939NoticeKind::RxAborted]
    );
}

#[test]
fn messages_to_one_destination_go_one_after_another() {
    let mut net = pair();
    let to = Some(J1939Addr::new(0x90, 0xEF00));
    net.nodes[0]
        .send(1, to, None, payload(50), net.now)
        .unwrap();
    net.nodes[0]
        .send(1, to, None, payload(60), net.now)
        .unwrap();
    net.nodes[0].send(1, to, None, vec![7], net.now).unwrap();
    net.settle();
    let lens: Vec<usize> = net.messages(1).iter().map(|(_, m)| m.data.len()).collect();
    // The single frame does not wait for the connections.
    assert_eq!(lens, [1, 50, 60]);
    // The second request to send comes after the first's acknowledgement.
    let cm: Vec<u8> = net
        .log
        .iter()
        .filter(|(_, _, f)| (f.can_id() >> 16) & 0xFF == 0xEC)
        .map(|(_, _, f)| f.data()[0])
        .collect();
    assert_eq!(cm, [16, 17, 19, 16, 17, 19]);
}

#[test]
fn the_extended_protocol_carries_what_is_longer() {
    for len in [1786, 5000, 1785 * 3 + 1] {
        let mut net = pair();
        net.nodes[0].set_notices(1, true).unwrap();
        let data = payload(len);
        net.nodes[0]
            .send(
                1,
                Some(J1939Addr::new(0x90, 0xEF00)),
                None,
                data.clone(),
                net.now,
            )
            .unwrap();
        net.settle();
        let rts = net.log[0].2;
        assert_eq!(rts.can_id() & CAN_EFF_MASK, 0x1CC8_9080);
        let size = (len as u32).to_le_bytes();
        assert_eq!(
            rts.data(),
            &[20, size[0], size[1], size[2], size[3], 0, 0xEF, 0]
        );
        // Clear-to-send for 255 packets from the first, then the offset.
        assert_eq!(net.log[1].2.data(), &[21, 255, 1, 0, 0, 0, 0xEF, 0]);
        assert_eq!(net.log[2].2.data(), &[22, 255, 0, 0, 0, 0, 0xEF, 0]);
        assert_eq!(net.log[3].2.can_id() & CAN_EFF_MASK, 0x1CC7_9080);
        assert_eq!(net.log[3].2.data()[0], 1);
        // The second block counts from where the first ended.
        let second: Vec<&Frame> = net
            .log
            .iter()
            .map(|(_, _, f)| f)
            .filter(|f| (f.can_id() >> 16) & 0xFF == 0xC8 && f.data()[0] == 22)
            .collect();
        assert_eq!(second[1].data()[2..5], [255, 0, 0]);
        assert_eq!(net.log.last().unwrap().2.data()[0], 23);
        let got = net.messages(1);
        assert_eq!(got.len(), 1, "{len} bytes");
        assert_eq!(got[0].1.data, data);
        let notices = net.notices(0);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].1.kind, J1939NoticeKind::TxDone);
    }
}

#[test]
fn a_request_for_too_much_is_refused() {
    let mut net = pair();
    // The extended protocol for a size the plain one carries.
    let size = 1000u32.to_le_bytes();
    let rts = frame(
        7,
        PGN_ETP_CM,
        0x80,
        0x33,
        &[20, size[0], size[1], size[2], size[3], 0, 0xEF, 0],
    );
    net.nodes[0].on_frame(&rts, net.now, net.now, Origin::Bus);
    let (abort_frame, _) = net.nodes[0].poll_transmit(net.now).unwrap();
    assert_eq!(abort_frame.data()[..2], [255, abort::TOO_BIG]);
    assert!(net.nodes[0].rx.is_empty());
    // A request to an address nobody here has is not answered at all.
    let rts = frame(7, PGN_TP_CM, 0x44, 0x33, &[16, 30, 0, 5, 0xFF, 0, 0xEF, 0]);
    net.nodes[0].on_frame(&rts, net.now, net.now, Origin::Bus);
    assert!(net.nodes[0].poll_transmit(net.now).is_none());
}

#[test]
fn two_sockets_on_one_interface_reach_each_other() {
    let mut net = Net::new();
    net.nodes[0].open(1, J1939Addr::address(0x80)).unwrap();
    net.nodes[0].open(2, J1939Addr::address(0x90)).unwrap();
    net.nodes[0].open(3, J1939Addr::NONE).unwrap();
    net.nodes[0].set_broadcast(1, true).unwrap();
    net.nodes[0].set_broadcast(3, true).unwrap();
    net.nodes[0].set_notices(1, true).unwrap();
    net.nodes[0]
        .send(
            1,
            Some(J1939Addr::new(0x90, 0xEF00)),
            None,
            vec![1],
            net.now,
        )
        .unwrap();
    net.nodes[0]
        .send(
            1,
            Some(J1939Addr::new(0x90, 0xEF00)),
            None,
            payload(500),
            net.now,
        )
        .unwrap();
    net.nodes[0]
        .send(
            1,
            Some(J1939Addr::broadcast(0xFEB0)),
            None,
            payload(40),
            net.now,
        )
        .unwrap();
    net.settle();
    let out = net.outputs(0);
    let messages: Vec<(SocketId, usize)> = out
        .iter()
        .filter_map(|(id, o)| match o {
            Output::Message(m) => Some((*id, m.data.len())),
            Output::Notice(_) => None,
        })
        .collect();
    // The broadcast reaches the socket that listens for them and not the
    // one that sent it.
    assert_eq!(messages, [(2, 1), (2, 500), (3, 40)]);
    let done = out
        .iter()
        .filter(|(id, o)| {
            *id == 1 && matches!(o, Output::Notice(n) if n.kind == J1939NoticeKind::TxDone)
        })
        .count();
    assert_eq!(done, 3);
}

#[test]
fn a_name_stands_for_the_address_it_claimed() {
    const ENGINE: u64 = 0x8000_0000_0000_1111;
    const BRAKES: u64 = 0x8000_0000_0000_2222;
    let mut net = Net::new();
    let bound = J1939Addr {
        name: ENGINE,
        pgn: NO_PGN,
        addr: 0x80,
    };
    net.nodes[0].open(1, bound).unwrap();
    net.nodes[0].set_broadcast(1, true).unwrap();
    net.nodes[1]
        .open(
            2,
            J1939Addr {
                name: BRAKES,
                pgn: NO_PGN,
                addr: 0x90,
            },
        )
        .unwrap();
    net.nodes[1].set_broadcast(2, true).unwrap();
    let to_brakes = Some(J1939Addr {
        name: BRAKES,
        pgn: 0xEF00,
        addr: NO_ADDR,
    });
    // Before any claim, neither NAME has an address.
    assert_eq!(
        net.nodes[0].send(1, to_brakes, None, vec![1], net.now),
        Err(SendError::AddressNotAvailable)
    );
    let claim = Some(J1939Addr::broadcast(PGN_ADDRESS_CLAIMED));
    // A claim has to carry the socket's own NAME.
    assert_eq!(
        net.nodes[0].send(1, claim, None, BRAKES.to_le_bytes().to_vec(), net.now),
        Err(SendError::NameMismatch)
    );
    net.nodes[0]
        .send(1, claim, None, ENGINE.to_le_bytes().to_vec(), net.now)
        .unwrap();
    net.nodes[1]
        .send(2, claim, None, BRAKES.to_le_bytes().to_vec(), net.now)
        .unwrap();
    net.settle();
    assert_eq!(net.log[0].2.can_id() & CAN_EFF_MASK, 0x18EE_FF80);
    // Each sees the other's claim.
    assert_eq!(net.messages(0)[0].1.src.name, BRAKES);
    net.messages(1);
    // Not yet: a claim has to stand for a quarter of a second.
    assert_eq!(
        net.nodes[0].send(1, to_brakes, None, vec![1], net.now),
        Err(SendError::AddressNotAvailable)
    );
    net.now += CLAIM_NS;
    net.nodes[0]
        .send(1, to_brakes, None, vec![1], net.now)
        .unwrap();
    net.settle();
    assert_eq!(
        net.log.last().unwrap().2.can_id() & CAN_EFF_MASK,
        0x18EF_9080
    );
    let got = net.messages(1);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1.src.name, ENGINE);
    assert_eq!(got[0].1.dst_name, BRAKES);

    // The brakes move to another address: the NAME follows.
    net.nodes[1]
        .bind(
            2,
            J1939Addr {
                name: BRAKES,
                pgn: NO_PGN,
                addr: 0x95,
            },
        )
        .unwrap();
    net.nodes[1]
        .send(2, claim, None, BRAKES.to_le_bytes().to_vec(), net.now)
        .unwrap();
    net.settle();
    net.now += CLAIM_NS;
    net.nodes[0]
        .send(1, to_brakes, None, payload(30), net.now)
        .unwrap();
    net.settle();
    let got = net.messages(1);
    assert_eq!(got.last().unwrap().1.data, payload(30));
    assert_eq!(got.last().unwrap().1.dst_addr, 0x95);

    // A lower NAME takes the engine's address from it; the engine says it
    // cannot claim one and is then without.
    let rival = 0x0000_0000_0000_0001u64;
    let f = frame(6, PGN_ADDRESS_CLAIMED, NO_ADDR, 0x80, &rival.to_le_bytes());
    for n in &mut net.nodes {
        n.on_frame(&f, net.now, net.now, Origin::Bus);
    }
    net.now += CLAIM_NS;
    assert_eq!(
        net.nodes[0].send(1, to_brakes, None, vec![1], net.now),
        Err(SendError::AddressNotAvailable)
    );
    net.nodes[0]
        .bind(
            1,
            J1939Addr {
                name: ENGINE,
                pgn: NO_PGN,
                addr: IDLE_ADDR,
            },
        )
        .unwrap();
    net.nodes[0]
        .send(1, claim, None, ENGINE.to_le_bytes().to_vec(), net.now)
        .unwrap();
    net.settle();
    assert_eq!(
        net.log.last().unwrap().2.can_id() & CAN_EFF_MASK,
        0x18EE_FFFE
    );
    // A higher NAME claiming a held address does not get it.
    let late = 0xFFFF_0000_0000_0000u64;
    let f = frame(6, PGN_ADDRESS_CLAIMED, NO_ADDR, 0x80, &late.to_le_bytes());
    net.nodes[0].on_frame(&f, net.now, net.now, Origin::Bus);
    assert_eq!(net.nodes[0].name_of(0x80), rival);
}

#[test]
fn closing_a_socket_gives_up_what_it_was_sending() {
    let mut net = pair();
    net.nodes[0]
        .send(
            1,
            Some(J1939Addr::new(0x90, 0xEF00)),
            None,
            payload(100),
            net.now,
        )
        .unwrap();
    let (rts, _) = net.nodes[0].poll_transmit(net.now).unwrap();
    net.nodes[1].on_frame(&rts, net.now, net.now, Origin::Bus);
    net.nodes[0].close(1, net.now);
    assert!(net.nodes[0].is_empty());
    net.settle();
    assert!(net
        .log
        .iter()
        .any(|(_, n, f)| *n == 0 && f.data()[0] == 255));
    assert!(net.nodes[1].rx.is_empty());
    assert!(net.messages(1).is_empty());
}

#[test]
fn a_dead_interface_ends_what_was_going_out() {
    let mut net = pair();
    net.nodes[0].set_notices(1, true).unwrap();
    net.nodes[0]
        .send(
            1,
            Some(J1939Addr::new(0x90, 0xEF00)),
            None,
            payload(100),
            net.now,
        )
        .unwrap();
    net.nodes[0].link_down(net.now);
    assert!(net.nodes[0].poll_transmit(net.now).is_none());
    assert_eq!(net.nodes[0].next_deadline(), None);
    let notices = net.notices(0);
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].1.kind, J1939NoticeKind::TxAborted);
    assert_eq!(notices[0].1.reason, abort::OTHER);
}
