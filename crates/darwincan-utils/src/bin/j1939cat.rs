// SPDX-License-Identifier: MIT OR Apache-2.0

//! `j1939cat`: sends a file as J1939 messages, or receives one message
//! and writes it out, as can-utils' program of that name does.

use std::io::{Read, Write};
use std::process::ExitCode;

use darwincan::j1939::{MAX_ETP_SIZE, NO_PGN};
use darwincan::{J1939Event, J1939NoticeKind, J1939Socket};
use darwincan_utils::j1939::{abort_errno, cat_event, cat_rts_details, parse_spec};
use darwincan_utils::{fail, getopt, usage, wants_help};

const TOOL: &str = "j1939cat";

const USAGE: &str = "\
j1939cat: netcat-like tool for j1939
Usage: j1939cat [options] FROM TO
 FROM / TO\t- or [IFACE][:[SA][,[PGN][,NAME]]]
Options:
 -i <infile>\t(default stdin)
 -s <size>\tSet maximal transfer size. Default: 117440505 byte
 -r\t\tReceive data
 -P <timeout>  poll timeout in milliseconds before sending data.
\t\tAccepted; a send here never waits, so it changes nothing.
 -R <count>\tSet send repeat count. Default: 1
 -B\t\tAllow to send and receive broadcast packets.

Example:
j1939cat -i some_file_to_send  can0:0x80 :0x90,0x12300
j1939cat can0:0x90 -r > /tmp/some_file_to_receive

What is sent is the input in pieces of at most <size> bytes, each as one
message. Progress goes to standard error: TX ACK when a message is out,
TX ABT when it was given up, and RX RTS, RX DPO and RX ABT on the
receiving side.";

/// Ends the program with the number Linux gives the error.
struct Failed(u8, String);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if wants_help(&args) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let opts = match getopt(args, "i:s:rP:R:B") {
        Ok(o) => o,
        Err(e) => return usage(TOOL, e, USAGE),
    };
    let number = |flag: char, default: usize| -> Result<usize, String> {
        match opts.value(flag) {
            None => Ok(default),
            Some(v) => v
                .parse()
                .map_err(|_| format!("-{flag}: '{v}' is not a number")),
        }
    };
    let (max, repeat) = match (number('s', MAX_ETP_SIZE), number('R', 1), number('P', 0)) {
        (Ok(max), Ok(repeat), Ok(_)) if max > 0 => (max.min(MAX_ETP_SIZE), repeat),
        (Err(e), ..) | (_, Err(e), _) | (.., Err(e)) => return usage(TOOL, e, USAGE),
        _ => return usage(TOOL, "-s needs a size of at least one byte", USAGE),
    };
    let specs: Result<Vec<_>, String> = opts.positional.iter().map(|s| parse_spec(s)).collect();
    let (from, to) = match specs.as_deref() {
        Ok([from]) => (from.clone(), None),
        Ok([from, to]) => (from.clone(), Some(to.clone())),
        Ok(_) => return usage(TOOL, "expected FROM and TO", USAGE),
        Err(e) => return usage(TOOL, e, USAGE),
    };
    let ifname = from
        .ifname
        .clone()
        .or(to.as_ref().and_then(|t| t.ifname.clone()));
    let Some(ifname) = ifname else {
        return fail(
            TOOL,
            "an interface has to be named, as in: j1939cat can0:0x80 :0x90,0x12300",
        );
    };
    let receive = opts.has('r');

    let run = (|| -> Result<Result<(), Failed>, darwincan::Error> {
        let sock = J1939Socket::open(&ifname, from.addr)?;
        if opts.has('B') {
            sock.set_broadcast(true)?;
        }
        sock.set_notices(true)?;
        if receive {
            if let Some(to) = &to {
                sock.connect(to.addr)?;
            }
            loop {
                match sock.recv_event()? {
                    J1939Event::Message(m) => {
                        let mut out = std::io::stdout().lock();
                        let data = &m.data[..m.data.len().min(max)];
                        out.write_all(data).and_then(|()| out.flush())?;
                        return Ok(Ok(()));
                    }
                    J1939Event::Notice(n) => match n.kind {
                        J1939NoticeKind::RxStarted => {
                            eprintln!("{}", cat_event("RX RTS", n.timestamp_ns, 0, 0));
                            eprintln!("{}", cat_rts_details(&n));
                        }
                        J1939NoticeKind::RxOffset => {
                            eprintln!("{}", cat_event("RX DPO", n.timestamp_ns, 0, n.acked));
                        }
                        J1939NoticeKind::RxAborted => {
                            eprintln!("{}", cat_event("RX ABT", n.timestamp_ns, 0, 0));
                            let (errno, text) = abort_errno(n.reason);
                            return Ok(Err(Failed(
                                errno,
                                format!("serr: rx error: {errno}, {text}"),
                            )));
                        }
                        _ => {}
                    },
                }
            }
        }
        let Some(to) = to.filter(|t| t.addr.pgn != NO_PGN) else {
            return Ok(Err(Failed(
                1,
                "sending needs a TO with a parameter group, as in: :0x90,0x12300".into(),
            )));
        };
        let mut data = Vec::new();
        match opts.value('i') {
            Some(path) => std::fs::File::open(path)?.read_to_end(&mut data)?,
            None => std::io::stdin().lock().read_to_end(&mut data)?,
        };
        let mut seq = 0;
        for _ in 0..repeat {
            for piece in data.chunks(max) {
                sock.send_to(piece, &to.addr)?;
                loop {
                    let J1939Event::Notice(n) = sock.recv_event()? else {
                        continue;
                    };
                    match n.kind {
                        J1939NoticeKind::TxDone => {
                            if piece.len() <= 8 {
                                // One frame: it was handed over and it
                                // was on the bus, both known only now.
                                eprintln!("{}", cat_event("TX ENQ", n.timestamp_ns, seq, n.acked));
                            }
                            eprintln!("{}", cat_event("TX ACK", n.timestamp_ns, seq, n.acked));
                            break;
                        }
                        J1939NoticeKind::TxAborted => {
                            eprintln!("{}", cat_event("TX ABT", n.timestamp_ns, seq, n.acked));
                            let (errno, text) = abort_errno(n.reason);
                            return Ok(Err(Failed(
                                errno,
                                format!("serr: tx error: {errno}, {text}"),
                            )));
                        }
                        _ => {}
                    }
                }
                seq += 1;
            }
        }
        Ok(Ok(()))
    })();
    match run {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(Failed(code, message))) => {
            eprintln!("{TOOL}: {message}");
            ExitCode::from(code)
        }
        Err(e) => fail(TOOL, e),
    }
}
