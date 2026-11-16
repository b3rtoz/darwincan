// SPDX-License-Identifier: GPL-2.0-or-later

//! End-to-end tests: a real daemon on a temporary socket, real clients.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use darwincan::bcm::{flag, op};
use darwincan::{
    err, BcmMessage, BcmSocket, BusConfig, Control, Error, ErrorCode, Filter, Frame, Id,
    IsoTpEvent, IsoTpFailure, IsoTpParams, IsoTpSocket, RawSocket,
};
use darwincand::{Backend, Daemon};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Socket path that removes its file when the test ends.
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

/// Start a daemon with `vcan0` on a fresh socket and return its path.
fn start() -> (Daemon, SockPath) {
    // Unix socket paths are limited to ~100 bytes, so stay out of $TMPDIR.
    let path = PathBuf::from(format!(
        "/tmp/darwincan-test-{}-{}.sock",
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

fn raw(path: &Path) -> RawSocket {
    let s = RawSocket::open_at(path, "vcan0").unwrap();
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    s
}

fn frame(id: u16, data: &[u8]) -> Frame {
    Frame::new(Id::Standard(id), data).unwrap()
}

/// Assert nothing arrives within a short window.
fn assert_silent(s: &RawSocket) {
    s.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    match s.recv() {
        Err(e) if e.is_timeout() => {}
        other => panic!("expected silence, got {other:?}"),
    }
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
}

#[test]
fn frames_reach_other_clients_but_not_the_sender() {
    let (_d, path) = start();
    let a = raw(&path);
    let b = raw(&path);
    let c = raw(&path);

    let f = frame(0x123, &[1, 2, 3, 4]);
    let before = darwincand::now_ns();
    a.send(&f).unwrap();

    let rb = b.recv().unwrap();
    let rc = c.recv().unwrap();
    assert_eq!(rb.frame, f);
    assert_eq!(rc.frame, f);
    assert!(!rb.own);
    // Every receiver sees the same timestamp for the same frame.
    assert_eq!(rb.timestamp_ns, rc.timestamp_ns);
    assert!(rb.timestamp_ns >= before && rb.timestamp_ns <= darwincand::now_ns());
    assert_silent(&a);
}

#[test]
fn try_recv_returns_what_has_arrived_without_waiting() {
    let (_d, path) = start();
    let a = raw(&path);
    let b = raw(&path);
    assert_eq!(a.try_recv().unwrap(), None);
    b.send(&frame(0x101, &[1])).unwrap();
    b.send(&frame(0x102, &[2])).unwrap();
    b.flush().unwrap();
    // The frames are on their way to `a`; its own flush picks them up.
    a.flush().unwrap();
    assert_eq!(a.try_recv().unwrap().unwrap().frame, frame(0x101, &[1]));
    assert_eq!(a.try_recv().unwrap().unwrap().frame, frame(0x102, &[2]));
    assert_eq!(a.try_recv().unwrap(), None);
    assert_silent(&a);
}

#[test]
fn can_fd_and_remote_frames_pass_through() {
    let (_d, path) = start();
    let a = raw(&path);
    let b = raw(&path);

    let fd = Frame::new_fd(Id::Extended(0x18DA_F110), &[0x55; 64], true).unwrap();
    let rtr = Frame::new_remote(Id::Standard(0x321), 4).unwrap();
    a.send(&fd).unwrap();
    a.send(&rtr).unwrap();
    assert_eq!(b.recv().unwrap().frame, fd);
    assert_eq!(b.recv().unwrap().frame, rtr);
}

#[test]
fn filters_select_frames() {
    let (_d, path) = start();
    let a = raw(&path);
    let b = raw(&path);

    b.set_filters(&[
        Filter::new(0x100, 0x7F0),
        Filter::exact(Id::Standard(0x555)),
    ])
    .unwrap();
    for id in [0x0FF, 0x100, 0x10F, 0x110, 0x555, 0x556] {
        a.send(&frame(id, &[])).unwrap();
    }
    let got: Vec<u32> = (0..3).map(|_| b.recv().unwrap().frame.id().raw()).collect();
    assert_eq!(got, vec![0x100, 0x10F, 0x555]);
    assert_silent(&b);

    // An empty filter list receives nothing at all.
    b.set_filters(&[]).unwrap();
    a.send(&frame(0x100, &[])).unwrap();
    assert_silent(&b);
}

#[test]
fn joined_filters_must_all_match() {
    let (_d, path) = start();
    let a = raw(&path);
    let b = raw(&path);

    // 0x100 to 0x1FF, and odd: either alone passes more.
    b.set_filters(&[Filter::new(0x100, 0x700), Filter::new(0x001, 0x001)])
        .unwrap();
    b.set_join_filters(true).unwrap();
    for id in [0x0FF, 0x100, 0x101, 0x1FF, 0x201] {
        a.send(&frame(id, &[])).unwrap();
    }
    let got: Vec<u32> = (0..2).map(|_| b.recv().unwrap().frame.id().raw()).collect();
    assert_eq!(got, vec![0x101, 0x1FF]);
    assert_silent(&b);

    // Joined, no filters at all still pass nothing.
    b.set_filters(&[]).unwrap();
    a.send(&frame(0x101, &[])).unwrap();
    assert_silent(&b);

    // Back to any-of: all but the first pass one filter or the other.
    b.set_filters(&[Filter::new(0x100, 0x700), Filter::new(0x001, 0x001)])
        .unwrap();
    b.set_join_filters(false).unwrap();
    for id in [0x0FE, 0x100, 0x201] {
        a.send(&frame(id, &[])).unwrap();
    }
    let got: Vec<u32> = (0..2).map(|_| b.recv().unwrap().frame.id().raw()).collect();
    assert_eq!(got, vec![0x100, 0x201]);
    assert_silent(&b);
}

#[test]
fn recv_own_msgs_and_loopback_options() {
    let (_d, path) = start();
    let a = raw(&path);
    let b = raw(&path);

    a.set_recv_own_msgs(true).unwrap();
    let f = frame(0x200, &[9]);
    a.send(&f).unwrap();
    let own = a.recv().unwrap();
    assert!(own.own);
    assert_eq!(own.frame, f);
    assert_eq!(b.recv().unwrap().timestamp_ns, own.timestamp_ns);

    // With loopback off, other local sockets no longer see a's frames.
    a.set_loopback(false).unwrap();
    a.send(&frame(0x201, &[])).unwrap();
    assert_eq!(a.recv().unwrap().frame.id().raw(), 0x201);
    assert_silent(&b);
}

#[test]
fn many_frames_arrive_in_order() {
    let (_d, path) = start();
    let a = raw(&path);
    let b = raw(&path);

    const N: u32 = 5000;
    let sender = thread::spawn(move || {
        for i in 0..N {
            a.send(&frame(0x300, &i.to_le_bytes())).unwrap();
        }
        a
    });
    let mut last_ts = 0;
    for i in 0..N {
        let rx = b.recv().unwrap();
        assert_eq!(rx.frame.data(), &i.to_le_bytes());
        assert!(rx.timestamp_ns >= last_ts);
        last_ts = rx.timestamp_ns;
    }
    sender.join().unwrap();
}

#[test]
fn errors_are_reported() {
    let (_d, path) = start();
    match RawSocket::open_at(&path, "can9") {
        Err(Error::Daemon { code, .. }) => assert_eq!(code, ErrorCode::NoSuchInterface),
        other => panic!("unexpected: {:?}", other.err()),
    }
    let missing = PathBuf::from("/tmp/darwincan-test-no-such-daemon.sock");
    assert!(matches!(
        RawSocket::open_at(&missing, "vcan0"),
        Err(Error::DaemonUnavailable(_))
    ));
    let mut bad = IsoTpParams::new(0x7E0, 0x7E8);
    bad.tx_dl = 64; // needs fd
    match IsoTpSocket::open_at(&path, "vcan0", bad) {
        Err(Error::Daemon { code, .. }) => assert_eq!(code, ErrorCode::InvalidRequest),
        other => panic!("unexpected: {:?}", other.err()),
    }
    // A second daemon cannot take over a live socket.
    assert_eq!(
        darwincand::bind(&path).unwrap_err().kind(),
        io::ErrorKind::AddrInUse
    );
}

#[test]
fn control_lists_and_adds_interfaces() {
    let (_d, path) = start();
    let ctl = Control::connect_at(&path).unwrap();
    assert_eq!(ctl.interfaces().unwrap().len(), 1);
    ctl.add_vcan("vcan1").unwrap();
    let names: Vec<String> = ctl
        .interfaces()
        .unwrap()
        .into_iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(names, vec!["vcan0", "vcan1"]);
    assert!(matches!(
        ctl.add_vcan("vcan1"),
        Err(Error::Daemon {
            code: ErrorCode::AlreadyExists,
            ..
        })
    ));
    assert!(ctl.add_vcan("bad name").is_err());
    let vcan1 = RawSocket::open_at(&path, "vcan1").unwrap();
    assert!(ctl.interfaces().unwrap().iter().all(|i| i.up));

    // Deleting a virtual interface tells its sockets and frees the name.
    ctl.del_vcan("vcan1").unwrap();
    assert!(matches!(
        vcan1.recv(),
        Err(Error::Daemon {
            code: ErrorCode::InterfaceRemoved,
            ..
        })
    ));
    assert!(matches!(
        ctl.del_vcan("vcan1"),
        Err(Error::Daemon {
            code: ErrorCode::NoSuchInterface,
            ..
        })
    ));
    assert!(RawSocket::open_at(&path, "vcan1").is_err());
    ctl.add_vcan("vcan1").unwrap();
}

#[test]
fn disconnected_clients_are_forgotten() {
    let (_d, path) = start();
    let a = raw(&path);
    for _ in 0..50 {
        drop(raw(&path));
    }
    let b = raw(&path);
    a.send(&frame(0x111, &[1])).unwrap();
    assert_eq!(b.recv().unwrap().frame.id().raw(), 0x111);
}

fn isotp_pair(path: &Path, configure: impl Fn(&mut IsoTpParams)) -> (IsoTpSocket, IsoTpSocket) {
    let mut pa = IsoTpParams::new(0x7E0, 0x7E8);
    let mut pb = IsoTpParams::new(0x7E8, 0x7E0);
    configure(&mut pa);
    configure(&mut pb);
    let a = IsoTpSocket::open_at(path, "vcan0", pa).unwrap();
    let b = IsoTpSocket::open_at(path, "vcan0", pb).unwrap();
    for s in [&a, &b] {
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    }
    (a, b)
}

fn payload(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 + 7) as u8).collect()
}

#[test]
fn isotp_transfers_between_clients() {
    let (_d, path) = start();
    let (a, b) = isotp_pair(&path, |_| {});
    let sniffer = raw(&path);

    // Single frame.
    a.send(&[0x3E, 0x00]).unwrap();
    assert_eq!(a.recv_event().unwrap(), IsoTpEvent::TxDone);
    assert_eq!(b.recv().unwrap(), vec![0x3E, 0x00]);
    assert_eq!(
        sniffer.recv().unwrap().frame,
        frame(0x7E0, &[0x02, 0x3E, 0x00])
    );

    // Multi-frame, and the reply in the other direction.
    let request = payload(4000);
    a.send_blocking(&request).unwrap();
    assert_eq!(b.recv().unwrap(), request);
    let reply = payload(10_000);
    b.send_blocking(&reply).unwrap();
    assert_eq!(a.recv().unwrap(), reply);
}

#[test]
fn isotp_over_can_fd() {
    let (_d, path) = start();
    let (a, b) = isotp_pair(&path, |p| {
        p.fd = true;
        p.brs = true;
        p.tx_dl = 64;
    });
    let sniffer = raw(&path);
    let msg = payload(2000);
    a.send_blocking(&msg).unwrap();
    assert_eq!(b.recv().unwrap(), msg);
    let first = sniffer.recv().unwrap().frame;
    assert!(first.is_fd() && first.is_brs());
    assert_eq!(first.len(), 64);
}

#[test]
fn isotp_st_min_paces_consecutive_frames() {
    let (_d, path) = start();
    // b asks for 2 ms between consecutive frames.
    let (a, b) = isotp_pair(&path, |p| p.st_min = 2);
    let sniffer = raw(&path);

    let msg = payload(6 + 7 * 20);
    a.send_blocking(&msg).unwrap();
    assert_eq!(b.recv().unwrap(), msg);

    let mut cf_times = Vec::new();
    while cf_times.len() < 20 {
        let rx = sniffer.recv().unwrap();
        if rx.frame.id().raw() == 0x7E0 && rx.frame.data()[0] >> 4 == 2 {
            cf_times.push(rx.timestamp_ns);
        }
    }
    for w in cf_times.windows(2) {
        let gap = w[1] - w[0];
        assert!(gap >= 2_000_000, "gap {gap} ns is below STmin");
        // Loose upper bound: tests run in parallel on a loaded machine.
        assert!(gap < 100_000_000, "gap {gap} ns is far above STmin");
    }
}

#[test]
fn isotp_times_out_without_a_peer() {
    let (_d, path) = start();
    let mut p = IsoTpParams::new(0x700, 0x701);
    p.n_bs_ms = 150;
    let a = IsoTpSocket::open_at(&path, "vcan0", p).unwrap();
    a.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    let started = Instant::now();
    a.send(&payload(100)).unwrap();
    assert_eq!(
        a.recv_event().unwrap(),
        IsoTpEvent::TxFailed(IsoTpFailure::Timeout)
    );
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(150), "{waited:?}");
    assert!(waited < Duration::from_millis(400), "{waited:?}");

    // A second send while one is in flight is refused.
    a.send(&payload(100)).unwrap();
    a.send(&payload(100)).unwrap();
    assert_eq!(
        a.recv_event().unwrap(),
        IsoTpEvent::TxFailed(IsoTpFailure::Busy)
    );
}

struct MockBackend {
    sent: Arc<Mutex<Vec<Frame>>>,
    log: Arc<Mutex<Vec<String>>>,
}

impl Backend for MockBackend {
    fn kind(&self) -> &str {
        "mock"
    }
    fn description(&self) -> String {
        "Mock adapter".into()
    }
    fn start(&self, config: &BusConfig) -> io::Result<()> {
        if config.bitrate == 1 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad bitrate"));
        }
        self.log
            .lock()
            .unwrap()
            .push(format!("start {}", config.bitrate));
        Ok(())
    }
    fn stop(&self) -> io::Result<()> {
        self.log.lock().unwrap().push("stop".into());
        Ok(())
    }
    fn transmit(&self, frame: &Frame, _token: u32) -> io::Result<()> {
        if frame.id().raw() == 0x666 {
            return Err(io::Error::other("bus off"));
        }
        self.sent.lock().unwrap().push(*frame);
        Ok(())
    }
}

#[test]
fn hardware_backend_lifecycle() {
    let (daemon, path) = start();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::new(Mutex::new(Vec::new()));
    let handle = daemon.add_can_interface(
        "a mock adapter",
        Box::new(MockBackend {
            sent: sent.clone(),
            log: log.clone(),
        }),
    );
    assert_eq!(handle.name(), "can0");

    let ctl = Control::connect_at(&path).unwrap();
    let info = |ctl: &Control| {
        ctl.interfaces()
            .unwrap()
            .into_iter()
            .find(|i| i.name == "can0")
            .unwrap()
    };
    let can0 = info(&ctl);
    assert_eq!(can0.kind, "mock");
    assert_eq!(can0.description, "Mock adapter");
    assert_eq!(can0.config, None);
    assert!(!can0.up);

    let a = RawSocket::open_at(&path, "can0").unwrap();
    let b = RawSocket::open_at(&path, "can0").unwrap();
    for s in [&a, &b] {
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    }

    // Transmitting on an interface that is down fails, at once, and
    // leaves nothing behind to be received.
    let f = frame(0x123, &[1]);
    match a.send(&f) {
        Err(Error::Daemon { code, message }) => {
            assert_eq!(code, ErrorCode::InterfaceDown);
            assert_eq!(message, "can0 is down");
        }
        other => panic!("unexpected result {other:?}"),
    }
    a.flush().unwrap();
    assert_eq!(a.try_recv().unwrap(), None);

    // A configuration the backend rejects is reported and leaves it down.
    assert!(matches!(
        ctl.up("can0", &BusConfig::new(1)),
        Err(Error::Daemon {
            code: ErrorCode::InvalidRequest,
            ..
        })
    ));
    assert_eq!(info(&ctl).config, None);

    // Up, then reconfigure: the backend is stopped before restarting.
    let cfg = BusConfig::fd(500_000, 2_000_000);
    ctl.up("can0", &BusConfig::new(250_000)).unwrap();
    ctl.up("can0", &cfg).unwrap();
    assert_eq!(info(&ctl).config, Some(cfg));
    assert!(info(&ctl).up);
    assert_eq!(
        *log.lock().unwrap(),
        vec!["start 250000", "stop", "start 500000"]
    );
    // The settings of an interface that is up cannot be changed in place.
    assert!(matches!(
        ctl.configure("can0", &BusConfig::new(125_000)),
        Err(Error::Daemon {
            code: ErrorCode::Busy,
            ..
        })
    ));
    assert_eq!(info(&ctl).config, Some(cfg));
    assert!(matches!(
        ctl.up("can9", &cfg),
        Err(Error::Daemon {
            code: ErrorCode::NoSuchInterface,
            ..
        })
    ));

    // Client transmit goes to the wire and is echoed to other clients.
    a.send(&f).unwrap();
    assert_eq!(b.recv().unwrap().frame, f);
    assert_eq!(*sent.lock().unwrap(), vec![f]);

    // Frames from the wire reach every client with the hardware timestamp.
    let from_wire = frame(0x456, &[2]);
    handle.receive(&from_wire, Some(1234));
    for s in [&a, &b] {
        let rx = s.recv().unwrap();
        assert_eq!(rx.frame, from_wire);
        assert_eq!(rx.timestamp_ns, 1234);
        assert!(!rx.own);
    }

    // A transmit failure is reported to the sender and not echoed.
    a.send(&frame(0x666, &[])).unwrap();
    assert!(matches!(
        a.recv(),
        Err(Error::Daemon {
            code: ErrorCode::TransmitFailed,
            ..
        })
    ));
    assert_silent(&b);

    // Error frames reach only sockets that asked for that error class.
    a.set_error_filter(err::BUSOFF).unwrap();
    let overrun = Frame::new_error(err::CRTL, [0, err::CRTL_RX_OVERFLOW, 0, 0, 0, 0, 0, 0]);
    let bus_off = Frame::new_error(err::BUSOFF, [0; 8]);
    handle.receive(&overrun, None);
    handle.receive(&bus_off, None);
    let rx = a.recv().unwrap();
    assert!(rx.frame.is_error());
    assert_eq!(rx.frame, bus_off);
    assert_silent(&a);
    assert_silent(&b);

    // Down: off the bus, transmits fail again.
    ctl.down("can0").unwrap();
    assert_eq!(log.lock().unwrap().last().unwrap(), "stop");
    assert!(matches!(
        a.send(&f),
        Err(Error::Daemon {
            code: ErrorCode::InterfaceDown,
            ..
        })
    ));

    // The configuration survives going down and can be replaced while
    // down, without touching the adapter.
    assert_eq!(info(&ctl).config, Some(cfg));
    assert!(!info(&ctl).up);
    let stored = BusConfig {
        listen_only: true,
        ..BusConfig::new(125_000)
    };
    ctl.configure("can0", &stored).unwrap();
    assert_eq!(info(&ctl).config, Some(stored));
    assert!(!info(&ctl).up);
    assert_eq!(log.lock().unwrap().last().unwrap(), "stop");
    assert!(matches!(
        ctl.configure("can9", &stored),
        Err(Error::Daemon {
            code: ErrorCode::NoSuchInterface,
            ..
        })
    ));
    assert!(matches!(
        ctl.configure("vcan0", &stored),
        Err(Error::Daemon {
            code: ErrorCode::Unsupported,
            ..
        })
    ));
    // Only virtual interfaces can be deleted.
    assert!(matches!(
        ctl.del_vcan("can0"),
        Err(Error::Daemon {
            code: ErrorCode::Unsupported,
            ..
        })
    ));

    // Unplugging tells attached clients and frees the name.
    handle.remove();
    for s in [&a, &b] {
        assert!(matches!(
            s.recv(),
            Err(Error::Daemon {
                code: ErrorCode::InterfaceRemoved,
                ..
            })
        ));
    }
    assert!(ctl.interfaces().unwrap().iter().all(|i| i.name != "can0"));
    assert!(RawSocket::open_at(&path, "can0").is_err());
}

#[test]
fn options_take_effect_before_the_setter_returns() {
    let (_d, path) = start();
    let a = raw(&path);
    let b = Arc::new(raw(&path));

    // One thread sits in recv the whole time; it must not swallow the
    // acknowledgements another thread is waiting for.
    let receiver = {
        let b = b.clone();
        thread::spawn(move || {
            let mut seen = Vec::new();
            loop {
                let id = b.recv().unwrap().frame.id().raw();
                if id == 0x7FF {
                    return seen;
                }
                seen.push(id);
            }
        })
    };

    for round in 0..200u16 {
        let wanted = 0x100 + round;
        b.set_filters(&[
            Filter::exact(Id::Standard(wanted)),
            Filter::exact(Id::Standard(0x7FF)),
        ])
        .unwrap();
        // The filter is in force now: only `wanted` gets through.
        a.send(&frame(wanted - 1, &[])).unwrap();
        a.send(&frame(wanted, &[])).unwrap();
        a.send(&frame(wanted + 1, &[])).unwrap();
        // Have the daemon finish with these before the next filter change.
        a.flush().unwrap();
    }
    a.send(&frame(0x7FF, &[])).unwrap();

    let seen = receiver.join().unwrap();
    let expected: Vec<u32> = (0..200).map(|r| 0x100 + r).collect();
    assert_eq!(seen, expected);
}

#[test]
fn clients_wait_for_a_daemon_that_is_still_starting() {
    let path = SockPath(PathBuf::from(format!(
        "/tmp/darwincan-test-{}-late.sock",
        std::process::id()
    )));
    let daemon_path = path.clone();
    thread::spawn(move || {
        // The client is already trying to connect when the daemon appears.
        thread::sleep(Duration::from_millis(150));
        let daemon = Daemon::new();
        daemon.add_vcan("vcan0").unwrap();
        let listener = darwincand::bind(&daemon_path).unwrap();
        daemon.serve(listener)
    });
    let started = Instant::now();
    RawSocket::open_at(&path, "vcan0").unwrap();
    assert!(started.elapsed() >= Duration::from_millis(100));
}

#[test]
fn interfaces_that_are_up_are_restored() {
    let (daemon, path) = start();
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut names = Vec::new();
    for _ in 0..3 {
        let handle = daemon.add_can_interface(
            "a mock adapter",
            Box::new(MockBackend {
                sent: Arc::new(Mutex::new(Vec::new())),
                log: log.clone(),
            }),
        );
        names.push(handle.name().to_string());
    }
    assert_eq!(names, ["can0", "can1", "can2"]);

    let ctl = Control::connect_at(&path).unwrap();
    let cfg = BusConfig {
        listen_only: true,
        ..BusConfig::new(250_000)
    };
    ctl.up("can0", &cfg).unwrap();
    // can1 was up and went down again; can2 was never started.
    ctl.up("can1", &BusConfig::new(125_000)).unwrap();
    ctl.down("can1").unwrap();

    let listener = RawSocket::open_at(&path, "can0").unwrap();
    listener
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    listener.set_error_filter(err::RESTARTED).unwrap();
    let deaf = RawSocket::open_at(&path, "can0").unwrap();
    deaf.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    log.lock().unwrap().clear();

    // Only the interface that is up is stopped and started again, with the
    // settings it had; its clients are told.
    assert!(daemon.restore_interfaces().is_empty());
    assert_eq!(*log.lock().unwrap(), vec!["stop", "start 250000"]);
    let told = listener.recv().unwrap().frame;
    assert!(told.is_error());
    assert_eq!(told.can_id() & err::MASK, err::RESTARTED);
    assert_silent(&deaf);
    let can0 = ctl
        .interfaces()
        .unwrap()
        .into_iter()
        .find(|i| i.name == "can0")
        .unwrap();
    assert!(can0.up);
    assert_eq!(can0.config, Some(cfg));
}

/// A backend whose adapter reports each frame once it has been sent: the
/// test plays the adapter and makes the reports.
struct ConfirmingBackend {
    handed_over: Mutex<std::sync::mpsc::Sender<(u32, Frame)>>,
}

impl Backend for ConfirmingBackend {
    fn kind(&self) -> &str {
        "mock"
    }
    fn start(&self, _config: &BusConfig) -> io::Result<()> {
        Ok(())
    }
    fn stop(&self) -> io::Result<()> {
        Ok(())
    }
    fn confirms_transmission(&self) -> bool {
        true
    }
    fn transmit(&self, frame: &Frame, token: u32) -> io::Result<()> {
        let _ = self.handed_over.lock().unwrap().send((token, *frame));
        Ok(())
    }
}

type HandedOver = std::sync::mpsc::Receiver<(u32, Frame)>;

/// A daemon with a confirming `can0` that is up.
fn start_confirming() -> (Daemon, SockPath, darwincand::InterfaceHandle, HandedOver) {
    let (daemon, path) = start();
    let (tx, handed_over) = std::sync::mpsc::channel();
    let handle = daemon.add_can_interface(
        "a mock adapter",
        Box::new(ConfirmingBackend {
            handed_over: Mutex::new(tx),
        }),
    );
    let ctl = Control::connect_at(&path).unwrap();
    ctl.up("can0", &BusConfig::new(500_000)).unwrap();
    (daemon, path, handle, handed_over)
}

#[test]
fn a_sent_frame_is_seen_when_the_adapter_reports_it() {
    let (_daemon, path, handle, handed_over) = start_confirming();
    let wait = Duration::from_secs(2);
    let a = RawSocket::open_at(&path, "can0").unwrap();
    let b = RawSocket::open_at(&path, "can0").unwrap();
    for s in [&a, &b] {
        s.set_read_timeout(Some(wait)).unwrap();
    }
    a.set_recv_own_msgs(true).unwrap();

    // Handed to the adapter, but not on the bus: nobody sees it yet.
    let f = frame(0x123, &[1]);
    a.send(&f).unwrap();
    a.flush().unwrap();
    let (token, handed) = handed_over.recv_timeout(wait).unwrap();
    assert_eq!(handed, f);
    assert_eq!(a.try_recv().unwrap(), None);
    assert_eq!(b.try_recv().unwrap(), None);

    // The adapter reports it sent: both see it, with the adapter's time.
    handle.transmitted(token, &f, Some(5555));
    let own = a.recv().unwrap();
    assert!(own.own);
    assert_eq!((own.frame, own.timestamp_ns), (f, 5555));
    let other = b.recv().unwrap();
    assert!(!other.own);
    assert_eq!((other.frame, other.timestamp_ns), (f, 5555));

    // With loopback off only the sender sees its frame.
    a.set_loopback(false).unwrap();
    a.send(&f).unwrap();
    let (token, _) = handed_over.recv_timeout(wait).unwrap();
    handle.transmitted(token, &f, Some(6666));
    assert_eq!(a.recv().unwrap().timestamp_ns, 6666);
    assert_silent(&b);
    a.set_loopback(true).unwrap();

    // A frame still with the adapter when the interface goes down is
    // forgotten. If the adapter sends it after all, it was on the bus and
    // is shown, but it is nobody's own any more.
    let ctl = Control::connect_at(&path).unwrap();
    a.send(&f).unwrap();
    let (token, _) = handed_over.recv_timeout(wait).unwrap();
    ctl.down("can0").unwrap();
    ctl.up("can0", &BusConfig::new(500_000)).unwrap();
    handle.transmitted(token, &f, Some(7777));
    for s in [&a, &b] {
        let late = s.recv().unwrap();
        assert!(!late.own);
        assert_eq!(late.timestamp_ns, 7777);
    }
}

#[test]
fn isotp_waits_for_each_frame_to_be_reported_sent() {
    let (_daemon, path, handle, handed_over) = start_confirming();
    let wait = Duration::from_secs(2);
    let a = IsoTpSocket::open_at(&path, "can0", IsoTpParams::new(0x7E0, 0x7E8)).unwrap();
    a.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    // First frame, then the peer's flow control asking for 20 ms.
    a.send(&payload(6 + 7 * 3)).unwrap();
    let (token, ff) = handed_over.recv_timeout(wait).unwrap();
    assert_eq!(ff.data()[0] >> 4, 1);
    handle.transmitted(token, &ff, None);
    handle.receive(&frame(0x7E8, &[0x30, 0, 20]), None);

    // The first consecutive frame is handed over. Until the adapter
    // reports it sent, the second is held back, however long STmin has
    // passed.
    let (token, cf1) = handed_over.recv_timeout(wait).unwrap();
    assert_eq!(cf1.data()[0], 0x21);
    assert!(handed_over
        .recv_timeout(Duration::from_millis(100))
        .is_err());

    // Reported: the second follows STmin after the time of the report.
    let reported = Instant::now();
    handle.transmitted(token, &cf1, Some(darwincand::now_ns()));
    let (_, cf2) = handed_over.recv_timeout(wait).unwrap();
    let gap = reported.elapsed();
    assert_eq!(cf2.data()[0], 0x22);
    assert!(gap >= Duration::from_millis(20), "{gap:?}");
    assert!(gap < Duration::from_millis(500), "{gap:?}");

    // The interface goes down with the second frame unsent: the message
    // fails instead of waiting for a report that will not come.
    Control::connect_at(&path).unwrap().down("can0").unwrap();
    assert_eq!(
        a.recv_event().unwrap(),
        IsoTpEvent::TxFailed(IsoTpFailure::NotSent)
    );
}

#[test]
fn a_bus_off_interface_refuses_frames_until_restarted() {
    let (daemon, path) = start();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let handle = daemon.add_can_interface(
        "a mock adapter",
        Box::new(MockBackend {
            sent: sent.clone(),
            log: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let ctl = Control::connect_at(&path).unwrap();
    let cfg = BusConfig::new(500_000);
    ctl.up("can0", &cfg).unwrap();
    let a = RawSocket::open_at(&path, "can0").unwrap();
    let f = frame(0x123, &[1]);
    // A send is refused at once when the socket has heard of the state,
    // and by the daemon, as an error to be received, when it has not yet.
    let refused = |a: &RawSocket| {
        let down = |e: &Error| {
            matches!(
                e,
                Error::Daemon {
                    code: ErrorCode::InterfaceDown,
                    ..
                }
            )
        };
        match a.send(&f) {
            Err(e) => down(&e),
            Ok(()) => {
                a.flush().unwrap();
                a.try_recv().is_err_and(|e| down(&e))
            }
        }
    };

    assert!(!refused(&a));
    // The controller reports bus-off: nothing more goes to the adapter.
    handle.receive(&Frame::new_error(err::BUSOFF, [0; 8]), None);
    assert!(refused(&a));
    assert_eq!(sent.lock().unwrap().len(), 1);
    // A lesser error state does not lift it; down and up does.
    handle.receive(
        &Frame::new_error(err::CRTL, [0, err::CRTL_TX_PASSIVE, 0, 0, 0, 0, 0, 0]),
        None,
    );
    assert!(refused(&a));
    ctl.down("can0").unwrap();
    ctl.up("can0", &cfg).unwrap();
    assert!(!refused(&a));

    // So does the controller reporting that it is error-active again.
    handle.receive(&Frame::new_error(err::BUSOFF, [0; 8]), None);
    assert!(refused(&a));
    handle.receive(
        &Frame::new_error(err::CRTL, [0, err::CRTL_ACTIVE, 0, 0, 0, 0, 0, 0]),
        None,
    );
    assert!(!refused(&a));
    assert_eq!(sent.lock().unwrap().len(), 3);
}

#[test]
fn interfaces_that_are_up_are_taken_off_the_bus() {
    let (daemon, path) = start();
    let log = Arc::new(Mutex::new(Vec::new()));
    for _ in 0..3 {
        daemon.add_can_interface(
            "a mock adapter",
            Box::new(MockBackend {
                sent: Arc::new(Mutex::new(Vec::new())),
                log: log.clone(),
            }),
        );
    }
    let ctl = Control::connect_at(&path).unwrap();
    let cfg = BusConfig::new(250_000);
    ctl.up("can0", &cfg).unwrap();
    // can1 was up and went down again; can2 was never started.
    ctl.up("can1", &BusConfig::new(125_000)).unwrap();
    ctl.down("can1").unwrap();
    log.lock().unwrap().clear();

    // Only the interface that is up is stopped; it keeps its settings.
    daemon.stop_interfaces();
    assert_eq!(*log.lock().unwrap(), vec!["stop"]);
    let can0 = ctl
        .interfaces()
        .unwrap()
        .into_iter()
        .find(|i| i.name == "can0")
        .unwrap();
    assert!(!can0.up);
    assert_eq!(can0.config, Some(cfg));
}

#[test]
fn a_stop_signal_still_ends_the_daemon() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};

    for (name, number) in [("TERM", 15), ("INT", 2), ("HUP", 1)] {
        let path = SockPath(PathBuf::from(format!(
            "/tmp/darwincan-test-{}-{}.sock",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )));
        let mut child = Command::new(env!("CARGO_BIN_EXE_darwincand"))
            .arg("--no-hardware")
            .arg("--socket")
            .arg(&*path)
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Connecting waits until the daemon listens, and so until it has
        // set up its signal handling. A freshly built daemon can take
        // longer to start than a client is willing to wait, so keep
        // trying, with a loose limit.
        let give_up = Instant::now() + Duration::from_secs(20);
        let ctl = loop {
            match Control::connect_at(&path) {
                Ok(ctl) => break ctl,
                Err(e) if Instant::now() >= give_up => {
                    child.kill().ok();
                    panic!("the daemon did not start: {e}");
                }
                Err(_) => assert!(child.try_wait().unwrap().is_none(), "the daemon exited"),
            }
        };
        assert!(!ctl.interfaces().unwrap().is_empty());

        let killed = Command::new("kill")
            .arg(format!("-{name}"))
            .arg(child.id().to_string())
            .status()
            .unwrap();
        assert!(killed.success());
        // Ended by the signal itself, as it was before the daemon handled it.
        assert_eq!(child.wait().unwrap().signal(), Some(number), "SIG{name}");
    }
}

fn bcm(path: &Path) -> BcmSocket {
    let s = BcmSocket::open_at(path, "vcan0").unwrap();
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    s
}

const MS: u64 = 1_000_000;

#[test]
fn a_cyclic_job_sends_on_its_interval_until_deleted() {
    let (_d, path) = start();
    let listener = raw(&path);
    let jobs = bcm(&path);

    jobs.send(&BcmMessage::tx_cyclic(frame(0x123, &[1]), 5_000))
        .unwrap();
    let times: Vec<u64> = (0..6)
        .map(|_| {
            let rx = listener.recv().unwrap();
            assert_eq!(rx.frame, frame(0x123, &[1]));
            rx.timestamp_ns
        })
        .collect();
    // The first frame goes out with the request, the others on a grid of
    // 5 ms that starts just before it: none is early by more than the
    // moment between setting the timer and sending that first frame.
    for (k, t) in times.iter().enumerate().skip(1) {
        let since_first = t - times[0];
        assert!(
            since_first > k as u64 * 5 * MS - MS,
            "frame {k} early: {since_first} ns"
        );
        assert!(
            since_first < k as u64 * 5 * MS + 500 * MS,
            "frame {k} late: {since_first} ns"
        );
    }

    // New data from the next frame on, with the timer left alone.
    jobs.send(&BcmMessage::tx_update(frame(0x123, &[2, 2])))
        .unwrap();
    loop {
        let rx = listener.recv().unwrap();
        if rx.frame == frame(0x123, &[2, 2]) {
            break;
        }
        assert_eq!(rx.frame, frame(0x123, &[1]));
    }

    // Deleted: what was sent before the answer has been delivered by the
    // time the answer is here, and nothing follows.
    jobs.send(&BcmMessage::tx_delete(0x123, false)).unwrap();
    while listener.try_recv().unwrap().is_some() {}
    assert_silent(&listener);
}

#[test]
fn a_job_reports_when_its_count_is_used_up() {
    let (_d, path) = start();
    let listener = raw(&path);
    let jobs = bcm(&path);

    let mut msg = BcmMessage::tx_cyclic(frame(0x123, &[1]), 0);
    msg.count = 3;
    msg.ival1_us = 2_000;
    msg.flags |= flag::TX_COUNTEVT;
    jobs.send(&msg).unwrap();
    let report = jobs.recv().unwrap();
    assert_eq!(report.msg.opcode, op::TX_EXPIRED);
    assert_eq!((report.msg.can_id, report.msg.count), (0x123, 0));
    // Three frames in all, and with no second interval that is the end.
    for _ in 0..3 {
        assert_eq!(listener.recv().unwrap().frame, frame(0x123, &[1]));
    }
    jobs.send(&BcmMessage {
        opcode: op::TX_READ,
        can_id: 0x123,
        ..BcmMessage::default()
    })
    .unwrap();
    assert_eq!(jobs.recv().unwrap().msg.opcode, op::TX_STATUS);
    assert_silent(&listener);
}

#[test]
fn a_receive_job_reports_changes_and_silence() {
    let (_d, path) = start();
    let sender = raw(&path);
    let jobs = bcm(&path);

    // Watch the first byte of 0x123, and expect it every 50 ms.
    jobs.send(&BcmMessage {
        opcode: op::RX_SETUP,
        flags: flag::SETTIMER,
        ival1_us: 50_000,
        can_id: 0x123,
        frames: vec![frame(0x123, &[0xFF, 0])],
        ..BcmMessage::default()
    })
    .unwrap();
    for data in [[1, 1], [1, 2], [2, 2]] {
        sender.send(&frame(0x123, &data)).unwrap();
    }
    sender.send(&frame(0x124, &[9, 9])).unwrap();
    sender.flush().unwrap();
    let sent_at = Instant::now();

    let first = jobs.recv().unwrap();
    assert_eq!(first.msg.opcode, op::RX_CHANGED);
    assert_eq!(first.msg.frames, [frame(0x123, &[1, 1])]);
    let second = jobs.recv().unwrap();
    assert_eq!(second.msg.frames, [frame(0x123, &[2, 2])]);
    assert!(second.timestamp_ns > first.timestamp_ns);

    // Then nothing comes: the timeout is reported, once.
    let timeout = jobs.recv().unwrap();
    assert_eq!(timeout.msg.opcode, op::RX_TIMEOUT);
    assert_eq!(timeout.msg.can_id, 0x123);
    assert!(timeout.msg.frames.is_empty());
    // The 50 ms ran from the last frame, which the daemon had by the
    // time the flush returned.
    assert!(timeout.timestamp_ns - second.timestamp_ns >= 50 * MS);
    assert!(sent_at.elapsed() < Duration::from_secs(2));
    jobs.set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    assert!(jobs.recv().unwrap_err().is_timeout());
}

#[test]
fn a_job_hears_the_frames_of_its_own_socket() {
    let (_d, path) = start();
    let jobs = bcm(&path);
    jobs.send(&BcmMessage::rx_watch(0x123, 0)).unwrap();
    jobs.send(&BcmMessage {
        opcode: op::TX_SEND,
        frames: vec![frame(0x123, &[7])],
        ..BcmMessage::default()
    })
    .unwrap();
    let report = jobs.recv().unwrap();
    assert_eq!(report.msg.opcode, op::RX_CHANGED);
    assert_eq!(report.msg.frames, [frame(0x123, &[7])]);
}

#[test]
fn broadcast_manager_requests_can_be_refused() {
    let (_d, path) = start();
    let jobs = bcm(&path);
    let refused = |msg: &BcmMessage| match jobs.send(msg) {
        Err(Error::Daemon { code, message }) => (code, message),
        other => panic!("unexpected result {other:?}"),
    };
    let (code, message) = refused(&BcmMessage::tx_delete(0x123, false));
    assert_eq!(code, ErrorCode::InvalidRequest);
    assert!(message.contains("no such transmit job"), "{message}");
    let (code, _) = refused(&BcmMessage {
        opcode: 99,
        ..BcmMessage::default()
    });
    assert_eq!(code, ErrorCode::InvalidRequest);
    // A refusal does not unsettle what follows.
    jobs.send(&BcmMessage::rx_watch(0x123, 0)).unwrap();
    refused(&BcmMessage::tx_delete(0x555, true));
    jobs.send(&BcmMessage::tx_cyclic(frame(0x555, &[]), 1_000_000))
        .unwrap();

    match BcmSocket::open_at(&path, "nosuch0") {
        Err(Error::Daemon { code, .. }) => assert_eq!(code, ErrorCode::NoSuchInterface),
        other => panic!("unexpected result {:?}", other.map(|_| ())),
    }
}

#[test]
fn jobs_end_with_their_socket() {
    let (_d, path) = start();
    let listener = raw(&path);
    let jobs = bcm(&path);
    jobs.send(&BcmMessage::tx_cyclic(frame(0x123, &[1]), 1_000))
        .unwrap();
    listener.recv().unwrap();
    drop(jobs);
    // The daemon notices the socket closing a moment later; after that
    // nothing more comes.
    listener
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut after_close = 0;
    while listener.recv().is_ok() {
        after_close += 1;
        assert!(after_close < 1000, "the job outlived its socket");
    }
}

#[test]
fn a_send_fails_at_once_when_the_interface_cannot_send() {
    let (daemon, path) = start();
    let handle = daemon.add_can_interface(
        "a mock adapter",
        Box::new(MockBackend {
            sent: Arc::new(Mutex::new(Vec::new())),
            log: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let ctl = Control::connect_at(&path).unwrap();
    let a = RawSocket::open_at(&path, "can0").unwrap();
    let f = frame(0x123, &[1]);
    let refusal = |a: &RawSocket| match a.send(&f) {
        Err(Error::Daemon { code, message }) => Some((code, message)),
        Ok(()) => None,
        Err(e) => panic!("{e}"),
    };

    // Down from the start: the socket knows when it opens.
    assert_eq!(
        refusal(&a),
        Some((ErrorCode::InterfaceDown, "can0 is down".into()))
    );
    // Up: the socket believes it is down, asks, and sends.
    ctl.up("can0", &BusConfig::new(500_000)).unwrap();
    assert_eq!(refusal(&a), None);
    // Down again. The news is on its way to the socket; once it has had
    // an answer from the daemon it has the news too.
    ctl.down("can0").unwrap();
    a.flush().unwrap();
    assert_eq!(
        refusal(&a),
        Some((ErrorCode::InterfaceDown, "can0 is down".into()))
    );
    // Unplugged.
    handle.remove();
    a.flush().unwrap();
    assert_eq!(
        refusal(&a),
        Some((ErrorCode::InterfaceRemoved, "can0 was removed".into()))
    );
    // A virtual interface is always up.
    assert!(raw(&path).send(&f).is_ok());
}

#[test]
fn a_socket_on_every_interface_hears_them_all_and_new_ones() {
    let (daemon, path) = start();
    let ctl = Control::connect_at(&path).unwrap();
    ctl.add_vcan("vcan1").unwrap();
    let index = |name: &str| {
        ctl.interfaces()
            .unwrap()
            .iter()
            .find(|i| i.name == name)
            .unwrap()
            .index
    };
    let all = RawSocket::open_at(&path, darwincan::ANY_INTERFACE).unwrap();
    all.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let on0 = raw(&path);
    let on1 = RawSocket::open_at(&path, "vcan1").unwrap();
    on1.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

    on0.send(&frame(0x100, &[0])).unwrap();
    on0.flush().unwrap();
    on1.send(&frame(0x101, &[1])).unwrap();
    on1.flush().unwrap();
    let first = all.recv().unwrap();
    assert_eq!(
        (first.frame, first.ifindex),
        (frame(0x100, &[0]), index("vcan0"))
    );
    let second = all.recv().unwrap();
    assert_eq!(
        (second.frame, second.ifindex),
        (frame(0x101, &[1]), index("vcan1"))
    );
    assert_ne!(first.ifindex, second.ifindex);
    // A socket on one interface says which one, too.
    on0.send(&frame(0x102, &[2])).unwrap();
    on0.flush().unwrap();
    all.recv().unwrap();

    // An interface that appears later is heard as well.
    ctl.add_vcan("vcan2").unwrap();
    let on2 = RawSocket::open_at(&path, "vcan2").unwrap();
    on2.send(&frame(0x103, &[3])).unwrap();
    on2.flush().unwrap();
    let third = all.recv().unwrap();
    assert_eq!(
        (third.frame, third.ifindex),
        (frame(0x103, &[3]), index("vcan2"))
    );

    // Sending names the interface; without one it is refused.
    assert!(all.send(&frame(0x200, &[])).is_err());
    all.send_to(index("vcan1"), &frame(0x200, &[9])).unwrap();
    all.flush().unwrap();
    assert_eq!(on1.recv().unwrap().frame, frame(0x200, &[9]));
    assert_silent(&on0);
    all.send_to(9999, &frame(0x200, &[9])).unwrap();
    assert!(matches!(
        all.recv(),
        Err(Error::Daemon {
            code: ErrorCode::NoSuchInterface,
            ..
        })
    ));

    // Filters apply across all of them, and an interface going away
    // leaves the socket open.
    all.set_filters(&[Filter::new(0x300, 0x700)]).unwrap();
    on0.send(&frame(0x100, &[])).unwrap();
    on0.send(&frame(0x300, &[])).unwrap();
    on0.flush().unwrap();
    assert_eq!(all.recv().unwrap().frame, frame(0x300, &[]));
    ctl.del_vcan("vcan2").unwrap();
    on1.send(&frame(0x301, &[])).unwrap();
    on1.flush().unwrap();
    assert_eq!(all.recv().unwrap().frame, frame(0x301, &[]));

    // "any" is not a name an interface can have.
    assert!(ctl.add_vcan("any").is_err());
    drop(all);
    drop(daemon);
}

#[test]
fn a_slow_reader_is_told_how_many_frames_it_lost() {
    let (_d, path) = start();
    let sender = raw(&path);
    let slow = raw(&path);
    // More than the daemon and the socket between them can hold, while
    // `slow` reads nothing: the rest is dropped.
    const SENT: u64 = 400_000;
    for n in 0..SENT {
        sender
            .send(&frame(0x123, &(n as u32).to_le_bytes()))
            .unwrap();
    }
    sender.flush().unwrap();

    // Read what did get through, until there is no more.
    slow.set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let mut received = 0u64;
    let mut counted = 0u64;
    loop {
        match slow.recv() {
            Ok(rx) => {
                // The count never goes back.
                assert!(rx.dropped >= counted);
                counted = rx.dropped;
                received += 1;
            }
            Err(e) if e.is_timeout() => break,
            Err(e) => panic!("{e}"),
        }
    }
    // The count is sent ahead of the next frame that fits, so one more
    // frame brings the whole of it.
    sender.send(&frame(0x124, &[])).unwrap();
    let marker = slow.recv().unwrap();
    assert_eq!(marker.frame, frame(0x124, &[]));
    let dropped = marker.dropped;
    assert!(dropped > 0, "nothing was dropped; send more");
    assert_eq!(received + dropped, SENT);
    assert_eq!(slow.dropped(), dropped);
}

fn mock() -> Box<MockBackend> {
    Box::new(MockBackend {
        sent: Arc::new(Mutex::new(Vec::new())),
        log: Arc::new(Mutex::new(Vec::new())),
    })
}

/// A daemon that keeps its interface names in `names`, with a control
/// connection to it.
fn start_keeping_names(names: &Path) -> (Daemon, SockPath, Control) {
    let path = PathBuf::from(format!(
        "/tmp/darwincan-test-{}-{}.sock",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let daemon = Daemon::new();
    daemon.keep_names_in(names);
    daemon.add_vcan("vcan0").unwrap();
    let listener = darwincand::bind(&path).unwrap();
    let d = daemon.clone();
    thread::spawn(move || d.serve(listener));
    let ctl = Control::connect_at(&path).unwrap();
    (daemon, SockPath(path), ctl)
}

#[test]
fn an_adapter_keeps_its_name_from_one_daemon_to_the_next() {
    let dir = PathBuf::from(format!("/tmp/darwincan-test-{}-names", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let names = dir.join("interfaces");
    let named = |ctl: &Control| -> Vec<(String, String)> {
        let mut list: Vec<(String, String)> = ctl
            .interfaces()
            .unwrap()
            .into_iter()
            .filter(|i| i.kind == "mock")
            .map(|i| (i.name, i.description))
            .collect();
        list.sort();
        list
    };

    {
        let (daemon, _path, ctl) = start_keeping_names(&names);
        let a = daemon.add_can_interface("adapter A", mock());
        let b = daemon.add_can_interface("adapter B", mock());
        assert_eq!((a.name(), b.name()), ("can0".into(), "can1".into()));

        // Renamed, as with ip link: only while down, to a name that is
        // free, and with its sockets left open.
        let sock = RawSocket::open_at(&_path, "can1").unwrap();
        ctl.up("can1", &BusConfig::new(500_000)).unwrap();
        assert!(matches!(
            ctl.rename("can1", "chassis"),
            Err(Error::Daemon {
                code: ErrorCode::Busy,
                ..
            })
        ));
        ctl.down("can1").unwrap();
        for (name, code) in [
            ("can0", ErrorCode::AlreadyExists),
            ("vcan0", ErrorCode::AlreadyExists),
            ("any", ErrorCode::InvalidRequest),
            ("", ErrorCode::InvalidRequest),
        ] {
            match ctl.rename("can1", name) {
                Err(Error::Daemon { code: got, .. }) => assert_eq!(got, code, "{name}"),
                other => panic!("{name}: unexpected result {other:?}"),
            }
        }
        assert!(matches!(
            ctl.rename("nosuch0", "x"),
            Err(Error::Daemon {
                code: ErrorCode::NoSuchInterface,
                ..
            })
        ));
        ctl.rename("can1", "chassis").unwrap();
        assert_eq!(b.name(), "chassis");
        assert_eq!(named(&ctl).len(), 2);
        assert!(named(&ctl).iter().any(|(name, _)| name == "chassis"));
        ctl.up("chassis", &BusConfig::new(500_000)).unwrap();
        sock.send(&frame(0x123, &[1])).unwrap();
        sock.flush().unwrap();
        assert_eq!(sock.try_recv().unwrap(), None);
        // The name it had is free again, for a virtual interface too.
        ctl.add_vcan("can1").unwrap();
        ctl.del_vcan("can1").unwrap();
    }

    // Another run finds the adapters in the other order, and a new one
    // first of all.
    let (daemon, _path, ctl) = start_keeping_names(&names);
    let c = daemon.add_can_interface("adapter C", mock());
    // A name kept for an adapter that is away is not given to another
    // adapter, nor to a virtual interface, nor by renaming.
    assert_eq!(c.name(), "can1");
    assert!(matches!(
        ctl.add_vcan("chassis"),
        Err(Error::Daemon {
            code: ErrorCode::AlreadyExists,
            ..
        })
    ));
    assert!(ctl.rename("can1", "can0").is_err());
    let b = daemon.add_can_interface("adapter B", mock());
    let a = daemon.add_can_interface("adapter A", mock());
    assert_eq!((a.name(), b.name()), ("can0".into(), "chassis".into()));

    // A virtual interface can be renamed too; nothing is kept for it.
    ctl.rename("vcan0", "bench").unwrap();
    let on_bench = RawSocket::open_at(&_path, "bench").unwrap();
    on_bench.send(&frame(0x1, &[])).unwrap();
    assert!(RawSocket::open_at(&_path, "vcan0").is_err());

    let kept = std::fs::read_to_string(&names).unwrap();
    assert!(kept.contains("chassis = adapter B"), "{kept}");
    assert!(kept.contains("can0 = adapter A"), "{kept}");
    assert!(kept.contains("can1 = adapter C"), "{kept}");
    assert!(!kept.contains("bench"), "{kept}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// An adapter that takes its time over each frame, as a real one does
/// when its buffer is full.
struct Slow(Duration);

impl Backend for Slow {
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
        thread::sleep(self.0);
        Ok(())
    }
}

#[test]
fn a_read_timeout_does_not_cut_an_acknowledgement_short() {
    let (daemon, path) = start();
    let took = Duration::from_millis(150);
    let handle = daemon.add_can_interface("a slow adapter", Box::new(Slow(took)));
    let control = Control::connect_at(&path.0).unwrap();
    control
        .up(&handle.name(), &BusConfig::new(500_000))
        .unwrap();
    let s = RawSocket::open_at(&path.0, &handle.name()).unwrap();
    // A program that receives with a short timeout...
    s.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
    assert!(s.recv().unwrap_err().is_timeout());
    // ...and then asks for something the daemon is slow to acknowledge.
    let begun = Instant::now();
    s.send(&Frame::new(Id::Standard(0x123), &[1]).unwrap())
        .unwrap();
    s.flush().unwrap();
    s.set_filters(&[Filter::new(0x100, 0x700)]).unwrap();
    assert!(begun.elapsed() >= took, "{:?}", begun.elapsed());
    assert!(begun.elapsed() < Duration::from_secs(5));
    // Receiving still times out.
    assert!(s.recv().unwrap_err().is_timeout());
}
