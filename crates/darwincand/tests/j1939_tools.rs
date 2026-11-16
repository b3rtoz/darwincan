// SPDX-License-Identifier: GPL-2.0-only

//! End-to-end tests of the J1939 tools' workings: a real daemon on a
//! temporary socket, the tools' code from `darwincan-utils`, real clients.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use darwincan::j1939::{NO_ADDR, NO_PGN};
use darwincan::{ErrorCode, Frame, Id, J1939Addr, J1939Socket, RawSocket, CAN_EFF_MASK};
use darwincan_utils::acd::{self, Claimer};
use darwincand::Daemon;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct SockPath(PathBuf);

impl Drop for SockPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn start() -> (Daemon, SockPath) {
    let path = PathBuf::from(format!(
        "/tmp/darwincan-jtools-{}-{}.sock",
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

fn named(name: u64) -> J1939Addr {
    J1939Addr {
        name,
        pgn: NO_PGN,
        addr: NO_ADDR,
    }
}

/// How an address claiming daemon ended: its exit status, its failure
/// message, the address it had last, and what it said.
type Ended = (u8, Option<String>, u8, String);

/// An address claiming daemon for `name`, as `j1939acd` runs it.
struct Acd {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<Ended>>,
}

impl Acd {
    fn start(path: &Path, name: u64, ranges: &str) -> Acd {
        let stop = Arc::new(AtomicBool::new(false));
        let sock = J1939Socket::open_at(path, "vcan0", named(name)).unwrap();
        let allowed = acd::parse_ranges(ranges).unwrap();
        let flag = stop.clone();
        let thread = thread::spawn(move || {
            let mut claimer = Claimer::new(name, allowed, None);
            let mut said = Vec::new();
            let (code, message) = acd::run(&sock, name, &mut claimer, &flag, 2, &mut said).unwrap();
            let said = String::from_utf8(said).unwrap();
            (code, message, claimer.address(), said)
        });
        Acd {
            stop,
            thread: Some(thread),
        }
    }

    /// Tell it to end, as a signal does.
    fn end(mut self) -> Ended {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap()
    }

    /// Wait for it to end by itself.
    fn finished(mut self) -> Ended {
        self.thread.take().unwrap().join().unwrap()
    }
}

fn raw(path: &Path) -> RawSocket {
    let s = RawSocket::open_at(path, "vcan0").unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s
}

fn ext(id: u32, data: &[u8]) -> Frame {
    Frame::new(Id::Extended(id), data).unwrap()
}

fn next(wire: &RawSocket) -> (u32, Vec<u8>) {
    let f = wire.recv().unwrap().frame;
    (f.can_id() & CAN_EFF_MASK, f.data().to_vec())
}

const NAME: u64 = 0x1122_3344_5566_7788;
const NAME_BYTES: [u8; 8] = [0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11];

#[test]
fn an_address_is_claimed_defended_and_given_up() {
    let (_d, path) = start();
    let wire = raw(&path.0);
    let begun = Instant::now();
    let daemon = Acd::start(&path.0, NAME, "0x80-0x82");
    // It asks who has which address, from the null address, and claims
    // the first of its own a while later.
    assert_eq!(next(&wire), (0x18EA_FFFE, vec![0x00, 0xEE, 0x00]));
    assert_eq!(next(&wire), (0x18EE_FF80, NAME_BYTES.to_vec()));
    let waited = begun.elapsed();
    assert!(
        waited >= Duration::from_millis(1250),
        "claimed after {waited:?}"
    );
    // A higher NAME is told whose address it is.
    wire.send(&ext(0x18EE_FF80, &[0xFF; 8])).unwrap();
    assert_eq!(next(&wire), (0x18EE_FF80, NAME_BYTES.to_vec()));
    // A request is answered, to everyone, wherever it was sent.
    wire.send(&ext(0x18EA_80FE, &[0x00, 0xEE, 0x00])).unwrap();
    assert_eq!(next(&wire), (0x18EE_FF80, NAME_BYTES.to_vec()));
    wire.send(&ext(0x18EA_FFFE, &[0x00, 0xEE, 0x00])).unwrap();
    assert_eq!(next(&wire), (0x18EE_FF80, NAME_BYTES.to_vec()));
    // A lower NAME gets the address; the next one is claimed.
    wire.send(&ext(0x18EE_FF80, &[1, 0, 0, 0, 0, 0, 0, 0]))
        .unwrap();
    assert_eq!(next(&wire), (0x18EE_FF81, NAME_BYTES.to_vec()));
    // Told to end, it gives the address up.
    let (code, message, addr, said) = daemon.end();
    assert_eq!(next(&wire), (0x18EE_FFFE, NAME_BYTES.to_vec()));
    assert_eq!((code, message, addr), (0, None, 0x81));
    assert_eq!(
        said,
        "- request sent, pending for 1250 ms\n\
         - claimed 0x80\n\
         - claimed 0x80\n\
         - claimed 0x80\n\
         - claimed 0x80\n\
         - address collision for 0x80\n\
         - claimed 0x81\n\
         - shutdown\n"
    );
}

#[test]
fn a_program_bound_to_the_name_uses_the_address_claimed_for_it() {
    let (_d, path) = start();
    let wire = raw(&path.0);
    // The program is there first, and has no address.
    let program = J1939Socket::open_at(&path.0, "vcan0", named(NAME)).unwrap();
    let to = J1939Addr::new(0x33, 0xEF00);
    match program.send_to(&[1], &to) {
        Err(darwincan::Error::Daemon { code, .. }) => {
            assert_eq!(code, ErrorCode::AddressNotAvailable)
        }
        other => panic!("{other:?}"),
    }
    let daemon = Acd::start(&path.0, NAME, "0x80-0x82");
    assert_eq!(next(&wire).0, 0x18EA_FFFE);
    assert_eq!(next(&wire).0, 0x18EE_FF80);
    // The claim has to stand for 250 ms; then the program sends from it.
    loop {
        match program.send_to(&[2], &to) {
            Ok(()) => break,
            Err(darwincan::Error::Daemon { .. }) => thread::sleep(Duration::from_millis(10)),
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(next(&wire), (0x18EF_3380, vec![2]));
    // And it is reached at it.
    program
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    wire.send(&ext(0x18EF_8033, &[9, 9])).unwrap();
    assert_eq!(program.recv().unwrap().data, [9, 9]);
    // The address moves when a lower NAME takes it; the program follows.
    wire.send(&ext(0x18EE_FF80, &[1, 0, 0, 0, 0, 0, 0, 0]))
        .unwrap();
    assert_eq!(next(&wire).0, 0x18EE_FF81);
    loop {
        match program.send_to(&[3], &to) {
            Ok(()) => break,
            Err(darwincan::Error::Daemon { .. }) => thread::sleep(Duration::from_millis(10)),
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(next(&wire), (0x18EF_3381, vec![3]));
    daemon.end();
}

#[test]
fn two_names_that_want_one_address_settle_it() {
    let (_d, path) = start();
    let wire = raw(&path.0);
    wire.set_filters(&[darwincan::Filter::new(
        0x00EE_0000 | darwincan::CAN_EFF_FLAG,
        0x00FF_0000 | darwincan::CAN_EFF_FLAG,
    )])
    .unwrap();
    // The lower NAME is there first, with one address; the higher one
    // may use two and prefers the same.
    let low = Acd::start(&path.0, 0x1000, "0x80");
    assert_eq!(next(&wire), (0x18EE_FF80, 0x1000u64.to_le_bytes().to_vec()));
    let high = Acd::start(&path.0, 0x2000, "0x80-0x81");
    // The higher one hears the answer to its request and takes the
    // address that is free.
    assert_eq!(next(&wire), (0x18EE_FF80, 0x1000u64.to_le_bytes().to_vec()));
    assert_eq!(next(&wire), (0x18EE_FF81, 0x2000u64.to_le_bytes().to_vec()));
    assert_eq!(high.end().2, 0x81);
    assert_eq!(low.end().2, 0x80);
}

#[test]
fn a_name_with_nowhere_to_go_says_so_and_ends() {
    let (_d, path) = start();
    let wire = raw(&path.0);
    let daemon = Acd::start(&path.0, NAME, "0x80");
    assert_eq!(next(&wire).0, 0x18EA_FFFE);
    assert_eq!(next(&wire).0, 0x18EE_FF80);
    wire.send(&ext(0x18EE_FF80, &[1, 0, 0, 0, 0, 0, 0, 0]))
        .unwrap();
    assert_eq!(next(&wire), (0x18EE_FFFE, NAME_BYTES.to_vec()));
    let (code, message, addr, said) = daemon.finished();
    assert_eq!((code, message, addr), (0, None, 0xFE));
    assert!(said.ends_with("- address collision for 0x80\n- shutdown\nno address left\n"));
    // One whose only address is a lower NAME's from the start fails.
    wire.send(&ext(0x18EE_FF80, &[1, 0, 0, 0, 0, 0, 0, 0]))
        .unwrap();
    let daemon = Acd::start(&path.0, NAME, "0x80");
    assert_eq!(next(&wire).0, 0x18EA_FFFE);
    thread::sleep(Duration::from_millis(300));
    wire.send(&ext(0x18EE_FF80, &[1, 0, 0, 0, 0, 0, 0, 0]))
        .unwrap();
    let (code, message, ..) = daemon.finished();
    assert_eq!(
        (code, message.as_deref()),
        (1, Some("no free address to use"))
    );
}
