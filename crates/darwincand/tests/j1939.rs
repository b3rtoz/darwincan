// SPDX-License-Identifier: GPL-2.0-or-later

//! End-to-end tests of J1939 sockets: a real daemon on a temporary socket,
//! real clients.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use darwincan::j1939::{abort, NO_ADDR, NO_PGN, PGN_ADDRESS_CLAIMED};
use darwincan::{
    BusConfig, Error, ErrorCode, Filter, Frame, Id, J1939Addr, J1939Event, J1939Filter,
    J1939NoticeKind, J1939Socket, RawSocket, CAN_EFF_MASK,
};
use darwincand::{Backend, Daemon};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct SockPath(PathBuf);

impl std::ops::Deref for SockPath {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for SockPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn start() -> (Daemon, SockPath) {
    let path = PathBuf::from(format!(
        "/tmp/darwincan-j1939-{}-{}.sock",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let daemon = Daemon::new();
    daemon.add_vcan("vcan0").unwrap();
    let listener = darwincand::bind(&path).unwrap();
    let d = daemon.clone();
    thread::spawn(move || d.serve(listener));
    (daemon, SockPath(path))
}

fn open(path: &Path, addr: u8) -> J1939Socket {
    let s = J1939Socket::open_at(path, "vcan0", J1939Addr::address(addr)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s
}

/// A raw socket that sees J1939's frames only.
fn raw(path: &Path) -> RawSocket {
    let s = RawSocket::open_at(path, "vcan0").unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s
}

fn payload(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 13 + i / 255) as u8).collect()
}

fn ext(id: u32, data: &[u8]) -> Frame {
    Frame::new(Id::Extended(id), data).unwrap()
}

fn refusal(result: darwincan::Result<()>) -> ErrorCode {
    match result {
        Err(Error::Daemon { code, .. }) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_message_reaches_the_address_it_is_sent_to() {
    let (_d, path) = start();
    let a = open(&path, 0x80);
    let b = open(&path, 0x90);
    let other = open(&path, 0x91);
    let wire = raw(&path);
    a.send_to(&[1, 2, 3], &J1939Addr::new(0x90, 0xEF00))
        .unwrap();
    let m = b.recv().unwrap();
    assert_eq!(m.data, [1, 2, 3]);
    assert_eq!(m.src, J1939Addr::new(0x80, 0xEF00));
    assert_eq!((m.dst_addr, m.priority), (0x90, 6));
    // On the wire it is one frame with a J1939 identifier.
    let rx = wire.recv().unwrap();
    assert_eq!(rx.frame.can_id() & CAN_EFF_MASK, 0x18EF_9080);
    assert_eq!(rx.timestamp_ns, m.timestamp_ns);
    // A frame from elsewhere on the bus is a message too.
    wire.send(&ext(0x0CEF_9033, &[9, 8])).unwrap();
    let m = b.recv().unwrap();
    assert_eq!(m.data, [9, 8]);
    assert_eq!((m.src.addr, m.priority), (0x33, 3));
    // Neither was for the third socket, nor comes back to the sender.
    a.flush().unwrap();
    wire.flush().unwrap();
    assert_eq!(other.try_recv().unwrap(), None);
    assert_eq!(a.try_recv().unwrap(), None);
}

#[test]
fn long_messages_cross_whole() {
    let (_d, path) = start();
    let a = open(&path, 0x80);
    let b = open(&path, 0x90);
    a.set_notices(true).unwrap();
    // The transport protocol, its longest message, and the extended one.
    for len in [9, 100, 1785, 1786, 20_000] {
        let data = payload(len);
        a.send_to(&data, &J1939Addr::new(0x90, 0xEF00)).unwrap();
        let m = b.recv().unwrap();
        assert_eq!(m.data, data, "{len} bytes");
        assert_eq!(m.src, J1939Addr::new(0x80, 0xEF00));
        match a.recv_event().unwrap() {
            J1939Event::Notice(n) => {
                assert_eq!(n.kind, J1939NoticeKind::TxDone);
                assert_eq!(n.size as usize, len);
                assert_eq!((n.src_addr, n.dst_addr, n.pgn), (0x80, 0x90, 0xEF00));
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn a_node_on_the_bus_is_answered_as_the_standard_says() {
    let (_d, path) = start();
    let ecu = open(&path, 0x80);
    let wire = raw(&path);
    wire.set_filters(&[Filter::new(
        0x00EC_0000 | darwincan::CAN_EFF_FLAG,
        0x00FF_0000 | darwincan::CAN_EFF_FLAG,
    )])
    .unwrap();
    // Request to send: 20 bytes in 3 packets of PGN 0xEF00, from 0x33.
    wire.send(&ext(0x1CEC_8033, &[16, 20, 0, 3, 0xFF, 0, 0xEF, 0]))
        .unwrap();
    let cts = wire.recv().unwrap().frame;
    assert_eq!(cts.can_id() & CAN_EFF_MASK, 0x1CEC_3380);
    assert_eq!(cts.data(), &[17, 3, 1, 0xFF, 0xFF, 0, 0xEF, 0]);
    let data = payload(20);
    for n in 0..3usize {
        let mut d = [0xFFu8; 8];
        d[0] = n as u8 + 1;
        let chunk = &data[n * 7..(n * 7 + 7).min(20)];
        d[1..1 + chunk.len()].copy_from_slice(chunk);
        wire.send(&ext(0x1CEB_8033, &d)).unwrap();
    }
    let eoma = wire.recv().unwrap().frame;
    assert_eq!(eoma.can_id() & CAN_EFF_MASK, 0x1CEC_3380);
    assert_eq!(eoma.data(), &[19, 20, 0, 3, 0xFF, 0, 0xEF, 0]);
    let m = ecu.recv().unwrap();
    assert_eq!(m.data, data);
    assert_eq!(m.src, J1939Addr::new(0x33, 0xEF00));
}

#[test]
fn a_broadcast_is_paced_and_needs_allowing() {
    let (_d, path) = start();
    let a = open(&path, 0x80);
    let b = open(&path, 0x90);
    let deaf = open(&path, 0x91);
    let wire = raw(&path);
    let everyone = J1939Addr::broadcast(0xFEB0);
    assert_eq!(
        refusal(a.send_to(&payload(30), &everyone)),
        ErrorCode::NotPermitted
    );
    a.set_broadcast(true).unwrap();
    b.set_broadcast(true).unwrap();
    assert_eq!(
        refusal(a.send_to(&payload(1786), &everyone)),
        ErrorCode::TooBig
    );
    let data = payload(30);
    a.send_to(&data, &everyone).unwrap();
    let m = b.recv().unwrap();
    assert_eq!(m.data, data);
    assert_eq!(m.dst_addr, NO_ADDR);
    // The announcement and five packets, each at least 50 ms after the
    // one before.
    let mut times = Vec::new();
    for n in 0..6 {
        let rx = wire.recv().unwrap();
        let pf = (rx.frame.can_id() >> 16) & 0xFF;
        assert_eq!(pf, if n == 0 { 0xEC } else { 0xEB });
        times.push(rx.timestamp_ns);
    }
    for pair in times.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(gap >= 50_000_000, "packets {gap} ns apart");
        assert!(gap < 500_000_000, "packets {gap} ns apart");
    }
    assert_eq!(m.timestamp_ns, times[5]);
    a.flush().unwrap();
    assert_eq!(deaf.try_recv().unwrap(), None);
}

#[test]
fn filters_and_the_bound_group_select_messages() {
    let (_d, path) = start();
    let a = open(&path, 0x80);
    let b = J1939Socket::open_at(&path, "vcan0", J1939Addr::new(0x90, 0xAA00)).unwrap();
    let c = open(&path, 0x90);
    let all = J1939Socket::open_at(&path, "vcan0", J1939Addr::NONE).unwrap();
    c.set_filters(&[J1939Filter::pgn(0xBB00)]).unwrap();
    all.set_promisc(true).unwrap();
    for pgn in [0xAA00, 0xBB00, 0xCC00] {
        a.send_to(&[pgn as u8, (pgn >> 8) as u8], &J1939Addr::new(0x90, pgn))
            .unwrap();
    }
    a.flush().unwrap();
    let pgns = |s: &J1939Socket| -> Vec<u32> {
        std::iter::from_fn(|| s.try_recv().unwrap())
            .map(|m| m.src.pgn)
            .collect()
    };
    // The messages are in the sockets' queues by the time the sender's
    // last request is answered: one thread handles them all.
    assert_eq!(pgns(&b), [0xAA00]);
    assert_eq!(pgns(&c), [0xBB00]);
    assert_eq!(pgns(&all), [0xAA00, 0xBB00, 0xCC00]);
    // A connected socket sends without naming the destination and hears
    // only its peer.
    c.set_filters(&[]).unwrap();
    c.connect(J1939Addr::new(0x80, 0xDD00)).unwrap();
    c.send(&[5]).unwrap();
    let m = a.recv().unwrap();
    assert_eq!((m.src.addr, m.src.pgn, m.data), (0x90, 0xDD00, vec![5]));
    let stranger = open(&path, 0x70);
    stranger
        .send_to(&[1], &J1939Addr::new(0x90, 0xDD00))
        .unwrap();
    a.send_to(&[2], &J1939Addr::new(0x90, 0xDD00)).unwrap();
    a.flush().unwrap();
    assert_eq!(c.recv().unwrap().data, [2]);
}

#[test]
fn sends_that_cannot_be_made_fail_at_once() {
    let (_d, path) = start();
    let a = open(&path, 0x80);
    let listener = J1939Socket::open_at(&path, "vcan0", J1939Addr::NONE).unwrap();
    let to = J1939Addr::new(0x90, 0xEF00);
    assert_eq!(
        refusal(listener.send_to(&[1], &to)),
        ErrorCode::InvalidRequest
    );
    assert_eq!(refusal(a.send(&[1])), ErrorCode::InvalidRequest);
    assert_eq!(
        refusal(a.send_to(&[1], &J1939Addr::new(0x90, 0xEF07))),
        ErrorCode::InvalidRequest
    );
    let named = J1939Addr {
        name: 42,
        pgn: 0xEF00,
        addr: NO_ADDR,
    };
    assert_eq!(
        refusal(a.send_to(&[1], &named)),
        ErrorCode::AddressNotAvailable
    );
    assert_eq!(refusal(a.set_send_priority(8)), ErrorCode::InvalidRequest);
    assert!(matches!(
        J1939Socket::open_at(&path, "vcan0", J1939Addr::new(0x80, 0xEF07)),
        Err(Error::Daemon {
            code: ErrorCode::InvalidRequest,
            ..
        })
    ));
    assert!(matches!(
        J1939Socket::open_at(&path, "nope", J1939Addr::NONE),
        Err(Error::Daemon {
            code: ErrorCode::NoSuchInterface,
            ..
        })
    ));
    // The socket still works.
    a.set_send_priority(2).unwrap();
    let b = open(&path, 0x90);
    a.send_to(&[1], &to).unwrap();
    assert_eq!(b.recv().unwrap().priority, 2);
    a.send_to_with_priority(&[1], &to, 5).unwrap();
    assert_eq!(b.recv().unwrap().priority, 5);
}

struct Idle;

impl Backend for Idle {
    fn kind(&self) -> &str {
        "mock"
    }
    fn start(&self, _config: &BusConfig) -> io::Result<()> {
        Ok(())
    }
    fn stop(&self) -> io::Result<()> {
        Ok(())
    }
    fn transmit(&self, _frame: &Frame, _token: u32) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_send_on_an_interface_that_is_down_fails() {
    let (daemon, path) = start();
    let handle = daemon.add_can_interface("a mock adapter", Box::new(Idle));
    let s = J1939Socket::open_at(&path, &handle.name(), J1939Addr::address(0x80)).unwrap();
    assert_eq!(
        refusal(s.send_to(&[1], &J1939Addr::new(0x90, 0xEF00))),
        ErrorCode::InterfaceDown
    );
}

#[test]
fn a_connection_nobody_answers_is_given_up() {
    let (_d, path) = start();
    let a = open(&path, 0x80);
    a.set_notices(true).unwrap();
    let begun = Instant::now();
    a.send_to(&payload(100), &J1939Addr::new(0x55, 0xEF00))
        .unwrap();
    match a.recv_event().unwrap() {
        J1939Event::Notice(n) => {
            assert_eq!(n.kind, J1939NoticeKind::TxAborted);
            assert_eq!(n.reason, abort::TIMEOUT);
        }
        other => panic!("{other:?}"),
    }
    let waited = begun.elapsed();
    assert!(
        waited >= Duration::from_millis(1250),
        "gave up after {waited:?}"
    );
    assert!(waited < Duration::from_secs(4), "gave up after {waited:?}");
}

#[test]
fn a_name_is_reached_at_the_address_it_claimed() {
    const ENGINE: u64 = 0x8000_0000_0000_1111;
    const BRAKES: u64 = 0x8000_0000_0000_2222;
    let (_d, path) = start();
    let bind = |name: u64, addr: u8| J1939Addr {
        name,
        pgn: NO_PGN,
        addr,
    };
    let engine = J1939Socket::open_at(&path, "vcan0", bind(ENGINE, 0x80)).unwrap();
    let brakes = J1939Socket::open_at(&path, "vcan0", bind(BRAKES, 0x90)).unwrap();
    for s in [&engine, &brakes] {
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.set_broadcast(true).unwrap();
        s.set_filters(&[J1939Filter::pgn(0xEF00)]).unwrap();
    }
    let to_brakes = J1939Addr {
        name: BRAKES,
        pgn: 0xEF00,
        addr: NO_ADDR,
    };
    let claim = J1939Addr::broadcast(PGN_ADDRESS_CLAIMED);
    assert_eq!(
        refusal(engine.send_to(&[1], &to_brakes)),
        ErrorCode::AddressNotAvailable
    );
    // A claim must carry the socket's own NAME.
    assert_eq!(
        refusal(engine.send_to(&BRAKES.to_le_bytes(), &claim)),
        ErrorCode::InvalidRequest
    );
    engine.send_to(&ENGINE.to_le_bytes(), &claim).unwrap();
    brakes.send_to(&BRAKES.to_le_bytes(), &claim).unwrap();
    let claimed = Instant::now();
    // A claim has to stand for 250 ms before the NAME has the address;
    // until then the send is refused. Nothing tells a program when the
    // time is up, so ask until it is.
    let sent = loop {
        match engine.send_to(&[7], &to_brakes) {
            Ok(()) => break claimed.elapsed(),
            Err(Error::Daemon {
                code: ErrorCode::AddressNotAvailable,
                ..
            }) => thread::sleep(Duration::from_millis(5)),
            Err(e) => panic!("{e}"),
        }
        assert!(claimed.elapsed() < Duration::from_secs(5));
    };
    assert!(sent >= Duration::from_millis(249), "sent after {sent:?}");
    let m = brakes.recv().unwrap();
    assert_eq!(m.data, [7]);
    assert_eq!((m.src.name, m.src.addr), (ENGINE, 0x80));
    assert_eq!((m.dst_name, m.dst_addr), (BRAKES, 0x90));
    // A long message goes to the NAME as well.
    engine.send_to(&payload(300), &to_brakes).unwrap();
    assert_eq!(brakes.recv().unwrap().data, payload(300));
}

#[test]
fn an_interface_stack_ends_with_its_last_socket_and_starts_again() {
    let (_d, path) = start();
    for round in 0..3u8 {
        let a = open(&path, 0x80);
        let b = open(&path, 0x90);
        a.send_to(&[round], &J1939Addr::new(0x90, 0xEF00)).unwrap();
        assert_eq!(b.recv().unwrap().data, [round]);
        a.send_to(&payload(50), &J1939Addr::new(0x90, 0xEF00))
            .unwrap();
        assert_eq!(b.recv().unwrap().data, payload(50));
    }
    // Opening and closing from many threads at once.
    let threads: Vec<_> = (0..8u8)
        .map(|n| {
            let path = path.to_path_buf();
            thread::spawn(move || {
                for _ in 0..50 {
                    let s = J1939Socket::open_at(&path, "vcan0", J1939Addr::address(n)).unwrap();
                    s.set_broadcast(true).unwrap();
                    s.send_to(&[n], &J1939Addr::broadcast(0xFEF1)).unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}
