// SPDX-License-Identifier: MIT OR Apache-2.0

//! `j1939sr`: sends what it reads from standard input as J1939 messages
//! and writes the data of the messages it receives to standard output,
//! as can-utils' program of that name is described.

use std::io::{Read, Write};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use darwincan::j1939::{NO_ADDR, NO_NAME, NO_PGN};
use darwincan::{J1939Event, J1939NoticeKind, J1939Socket};
use darwincan_utils::j1939::parse_hex_spec;
use darwincan_utils::{fail, getopt, long_options, usage, wants_help};

const TOOL: &str = "j1939sr";

const USAGE: &str = "\
Usage: j1939sr [OPTION...] SOURCE [DEST]

An SAE J1939 send/recv utility: each piece read from standard input is
sent to DEST as one message, and the data of each message received is
written to standard output.

Options:
  -v, --verbose       Increase verbosity
  -p, --priority=VAL  J1939 priority (0..7, default 6)
  -S, --serialize     Strictly serialize outgoing packets
  -s, --size          Packet size, default 1024

  SOURCE  [IFACE:][NAME|SA][,PGN]
  DEST    [IFACE:][NAME|SA][,PGN]

Numbers are in hex. Messages are sent with DEST's parameter group or,
if it names none, with SOURCE's. With a DEST, only what it sends is
received; with a PGN in SOURCE, only that parameter group.

Example:
  j1939sr can0:90,EF00 can0:80";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if wants_help(&args) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let long = [
        ("verbose", 'v'),
        ("priority", 'p'),
        ("serialize", 'S'),
        ("size", 's'),
    ];
    let opts = match long_options(args, &long).and_then(|args| getopt(args, "vp:Ss:")) {
        Ok(o) => o,
        Err(e) => return usage(TOOL, e, USAGE),
    };
    let priority = match opts.value('p').map(str::parse::<u8>) {
        None => None,
        Some(Ok(p)) if p <= 7 => Some(p),
        Some(_) => return usage(TOOL, "a priority is 0 to 7", USAGE),
    };
    let size = match opts.value('s').map(str::parse::<usize>) {
        None => 1024,
        Some(Ok(size)) if size > 0 => size,
        Some(_) => return usage(TOOL, "-s needs a size in bytes", USAGE),
    };
    let specs: Result<Vec<_>, String> = opts.positional.iter().map(|s| parse_hex_spec(s)).collect();
    let (source, dest) = match specs.as_deref() {
        Ok([source]) => (source.clone(), None),
        Ok([source, dest]) => (source.clone(), Some(dest.clone())),
        Ok(_) => return usage(TOOL, "expected SOURCE and at most one DEST", USAGE),
        Err(e) => return usage(TOOL, e, USAGE),
    };
    let Some(ifname) = source.ifname.clone() else {
        return fail(
            TOOL,
            "an interface has to be named, as in: j1939sr can0:90,EF00 can0:80",
        );
    };
    let verbose = opts.has('v');
    let serialize = opts.has('S');

    let run = (|| -> darwincan::Result<()> {
        let sock = J1939Socket::open(&ifname, source.addr)?;
        if let Some(priority) = priority {
            sock.set_send_priority(priority)?;
        }
        let mut can_send = false;
        if let Some(dest) = &dest {
            let mut peer = dest.addr;
            if peer.pgn == NO_PGN {
                peer.pgn = source.addr.pgn;
            }
            sock.connect(peer)?;
            can_send = peer.pgn != NO_PGN && (peer.addr != NO_ADDR || peer.name != NO_NAME);
        }
        sock.set_notices(serialize)?;
        // Standard input is read on a thread of its own; the socket is
        // used on this one only.
        let (pieces, incoming) = mpsc::sync_channel::<Vec<u8>>(1);
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buffer = vec![0u8; size];
            while let Ok(n) = stdin.read(&mut buffer) {
                if n == 0 || pieces.send(buffer[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        sock.set_read_timeout(Some(Duration::from_millis(5)))?;
        let mut out = std::io::stdout().lock();
        let mut pending = false;
        loop {
            if !pending {
                if let Ok(piece) = incoming.try_recv() {
                    if !can_send {
                        return Err(darwincan::Error::Io(std::io::Error::other(
                            "nothing to send to: give a DEST, and a PGN in DEST or SOURCE",
                        )));
                    }
                    if verbose {
                        eprintln!("{TOOL}: sending {} bytes", piece.len());
                    }
                    sock.send(&piece)?;
                    pending = serialize;
                }
            }
            match sock.recv_event() {
                Ok(J1939Event::Message(m)) => {
                    if out.write_all(&m.data).and_then(|()| out.flush()).is_err() {
                        return Ok(());
                    }
                }
                Ok(J1939Event::Notice(n)) => match n.kind {
                    J1939NoticeKind::TxDone => pending = false,
                    J1939NoticeKind::TxAborted => {
                        return Err(darwincan::Error::Io(std::io::Error::other(format!(
                            "a message was given up (abort reason {})",
                            n.reason
                        ))))
                    }
                    _ => {}
                },
                Err(e) if e.is_timeout() => {}
                Err(e) => return Err(e),
            }
        }
    })();
    match run {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(TOOL, e),
    }
}
