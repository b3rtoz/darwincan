// SPDX-License-Identifier: GPL-2.0-or-later

//! The C API against a real daemon: a C program built from the header
//! and the library as a C user would, and the version check both ways.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use darwincan::{Control, Error, Frame, Id, IsoTpParams, IsoTpSocket, RawSocket};
use darwincan_core::wire::{Message, MessageReader, PROTOCOL_VERSION};
use darwincand::Daemon;

/// Socket path that removes its file when the test ends.
struct SockPath(PathBuf);

impl Drop for SockPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn start(tag: &str) -> (Daemon, SockPath) {
    let path = PathBuf::from(format!(
        "/tmp/darwincan-c-{}-{tag}.sock",
        std::process::id()
    ));
    let daemon = Daemon::new();
    daemon.add_vcan("vcan0").unwrap();
    let listener = darwincand::bind(&path).unwrap();
    let d = daemon.clone();
    thread::spawn(move || d.serve(listener));
    (daemon, SockPath(path))
}

/// A C program from `crates/darwincan/c`, built as a user of the library
/// would build it and removed again when the test ends.
struct CProgram(PathBuf);

impl Drop for CProgram {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn build(source: &str, tag: &str) -> CProgram {
    let client = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../darwincan");
    // The static library is built beside this test.
    let library = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("libdarwincan.a");
    assert!(library.exists(), "{} is missing", library.display());
    let program = CProgram(PathBuf::from(format!(
        "/tmp/darwincan-c-{}-{tag}",
        std::process::id()
    )));
    let built = Command::new("cc")
        // -isystem and not -I: the examples include <darwincan.h>, as a
        // program does once the header is installed.
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-isystem"])
        .arg(client.join("include"))
        .arg(client.join("c").join(source))
        .arg(&library)
        .arg("-o")
        .arg(&program.0)
        .output()
        .expect("a C compiler is needed for this test");
    assert!(
        built.status.success(),
        "{source} did not build:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
    program
}

/// Run a program and return what it printed, having checked it succeeded.
fn run(command: &mut Command) -> String {
    let ran = command.output().unwrap();
    assert!(
        ran.status.success(),
        "the C program failed ({}):\n{}{}",
        ran.status,
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );
    String::from_utf8_lossy(&ran.stdout).into_owned()
}

#[test]
fn a_c_program_uses_the_header_and_the_library() {
    let program = build("selftest.c", "selftest");
    let (_daemon, path) = start("selftest");
    assert_eq!(run(Command::new(&program.0).arg(&path.0)), "ok\n");
}

#[test]
fn a_c_program_outlives_its_daemon() {
    let program = build("selftest.c", "gone");
    // A stand-in daemon that opens the socket asked for and hangs up.
    let path = SockPath(PathBuf::from(format!(
        "/tmp/darwincan-c-{}-gone.sock",
        std::process::id()
    )));
    let listener = std::os::unix::net::UnixListener::bind(&path.0).unwrap();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = MessageReader::new(stream.try_clone().unwrap());
        assert!(matches!(reader.read().unwrap(), Message::OpenRaw { .. }));
        Message::Ok.write_to(&stream).unwrap();
    });
    assert_eq!(
        run(Command::new(&program.0).arg("--gone").arg(&path.0)),
        "gone ok\n"
    );
}

#[test]
fn a_client_of_another_version_is_told_so() {
    let (_daemon, path) = start("oldclient");
    let mut stream = UnixStream::connect(&path.0).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = Vec::new();
    Message::ListIfaces.encode(&mut request);
    request[2..4].copy_from_slice(&(PROTOCOL_VERSION + 1).to_le_bytes());
    stream.write_all(&request).unwrap();

    // The daemon answers in its own version, which is how the client
    // learns of the mismatch, and then hangs up.
    let mut reader = MessageReader::new(stream.try_clone().unwrap());
    match reader.read().unwrap() {
        Message::Error { message, .. } => {
            assert!(message.contains("version"), "{message}");
        }
        other => panic!("unexpected answer {other:?}"),
    }
    let mut rest = Vec::new();
    assert_eq!(stream.read_to_end(&mut rest).unwrap(), 0);
}

#[test]
fn a_daemon_of_another_version_is_reported_as_such() {
    // A stand-in daemon that answers everything in a later version.
    let path = SockPath(PathBuf::from(format!(
        "/tmp/darwincan-c-{}-newdaemon.sock",
        std::process::id()
    )));
    let listener = std::os::unix::net::UnixListener::bind(&path.0).unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut header = [0u8; 8];
        stream.read_exact(&mut header).unwrap();
        let mut answer = Vec::new();
        Message::Ifaces(Vec::new()).encode(&mut answer);
        answer[2..4].copy_from_slice(&(PROTOCOL_VERSION + 1).to_le_bytes());
        stream.write_all(&answer).unwrap();
    });
    let control = Control::connect_at(&path.0).unwrap();
    match control.interfaces() {
        Err(Error::VersionMismatch { client, daemon }) => {
            assert_eq!((client, daemon), (PROTOCOL_VERSION, PROTOCOL_VERSION + 1));
        }
        other => panic!("unexpected result {other:?}"),
    }
}

#[test]
fn the_dump_example_prints_what_is_on_the_bus() {
    let program = build("dump.c", "dump");
    let (_daemon, path) = start("dump");
    let mut child = Command::new(&program.0)
        .args(["vcan0", "3", "100:700"])
        .env("DARWINCAN_SOCKET", &path.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // It says when it is listening; frames sent before that are not its.
    let mut ready = String::new();
    BufReader::new(child.stderr.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready, "listening on vcan0\n");

    let sender = RawSocket::open_at(&path.0, "vcan0").unwrap();
    let send = |frame: Frame| sender.send(&frame).unwrap();
    send(Frame::new(Id::Standard(0x200), &[1]).unwrap());
    send(Frame::new(Id::Standard(0x123), &[0xDE, 0xAD]).unwrap());
    send(Frame::new_remote(Id::Standard(0x1FF), 4).unwrap());
    send(Frame::new_fd(Id::Standard(0x100), &[7; 12], true).unwrap());

    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    // Each line starts with the time; compare what follows it.
    let lines: Vec<&str> = text
        .lines()
        .map(|line| line.split_once(") ").unwrap().1)
        .collect();
    assert_eq!(
        lines,
        [
            "vcan0 123 [2] DE AD",
            "vcan0 1FF remote, 4 bytes asked for",
            "vcan0 100 [12] 07 07 07 07 07 07 07 07 07 07 07 07  (bit rate switched)",
        ]
    );
}

#[test]
fn the_uds_example_reads_a_data_identifier() {
    let program = build("uds_read.c", "uds");
    let (_daemon, path) = start("uds");
    let ecu = IsoTpSocket::open_at(&path.0, "vcan0", IsoTpParams::new(0x7E8, 0x7E0)).unwrap();
    ecu.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let answering = thread::spawn(move || {
        assert_eq!(ecu.recv().unwrap(), [0x22, 0xF1, 0x90]);
        // Ask for patience once, then answer in several frames.
        ecu.send_blocking(&[0x7F, 0x22, 0x78]).unwrap();
        let mut response = vec![0x62, 0xF1, 0x90];
        response.extend_from_slice(b"WDB1234567890ABCD");
        ecu.send_blocking(&response).unwrap();
    });
    let printed = run(Command::new(&program.0)
        .args(["vcan0", "7E0", "7E8", "F190"])
        .env("DARWINCAN_SOCKET", &path.0));
    answering.join().unwrap();
    assert_eq!(
        printed,
        "62 F1 90 57 44 42 31 32 33 34 35 36 37 38 39 30 41 42 43 44\n"
    );
}

#[test]
fn the_cyclic_example_sends_on_time_and_changes_its_data() {
    let program = build("cyclic.c", "cyclic");
    let (_daemon, path) = start("cyclic");
    let listener = RawSocket::open_at(&path.0, "vcan0").unwrap();
    listener
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut child = Command::new(&program.0)
        .args(["vcan0", "123", "20", "1"])
        .env("DARWINCAN_SOCKET", &path.0)
        .spawn()
        .unwrap();
    // It runs for a second: some fifty frames 20 ms apart, the last few
    // with the data it changes to after that second.
    let mut times = Vec::new();
    let mut last = Vec::new();
    loop {
        match listener.recv() {
            Ok(rx) => {
                assert_eq!(rx.frame.id(), Id::Standard(0x123));
                times.push(rx.timestamp_ns);
                last = rx.frame.data().to_vec();
            }
            Err(e) if e.is_timeout() => break,
            Err(e) => panic!("{e}"),
        }
        if child.try_wait().unwrap().is_some() && listener.try_recv().unwrap().is_none() {
            break;
        }
    }
    assert!(child.wait().unwrap().success());
    assert!((45..=60).contains(&times.len()), "{} frames", times.len());
    // Frames 1 and later lie on a 20 ms grid; none of them is early.
    for pair in times[1..].windows(2) {
        assert!(
            pair[1] - pair[0] > 19_000_000,
            "gap {} ns",
            pair[1] - pair[0]
        );
    }
    assert!(last == [0, 0] || last == [0, 1], "{last:?}");
}
