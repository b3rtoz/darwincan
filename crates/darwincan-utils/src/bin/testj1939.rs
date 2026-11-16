// SPDX-License-Identifier: MIT OR Apache-2.0

//! `testj1939`: sends and receives J1939 messages to try the stack out,
//! as can-utils' program of that name does.

use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use darwincan::j1939::MAX_ETP_SIZE;
use darwincan::{J1939Addr, J1939Event, J1939NoticeKind, J1939Socket};
use darwincan_utils::fail;
use darwincan_utils::j1939::{parse_spec, testj1939_data, testj1939_dump, Spec};

const TOOL: &str = "testj1939";

const USAGE: &str = "\
Usage: testj1939 [OPTIONS] FROM TO
 FROM / TO\t- or [IFACE][:[SA][,[PGN][,NAME]]]
Options:
 -v\t\tPrint relevant API calls
 -s[=LEN]\tInitial send of LEN bytes dummy data
 -r\t\tReceive (and print) data
 -e\t\tEcho incoming packets back
\t\tThis actually receives packets
 -c\t\tIssue connect()
 -p=PRIO\tSet priority to PRIO
 -P\t\tPromiscuous mode. Allow to receive all packets
 -b\t\tDo normal bind with SA+1 and rebind with actual SA
 -B\t\tAllow to send and receive broadcast packets.
 -o\t\tOmit bind
 -n\t\tEmit 64bit NAMEs in output
 -w[TIME]\tReturn after TIME (default 1) seconds

The calls -v prints are those of darwinCAN's C API.

Examples:
testj1939 can1 20

testj1939 -r vcan0:0x90
testj1939 -s20 vcan0:0x80 :0x90,0x12300";

#[derive(Default)]
struct Options {
    verbose: bool,
    send: Option<usize>,
    receive: bool,
    echo: bool,
    connect: bool,
    priority: Option<u8>,
    promisc: bool,
    rebind: bool,
    broadcast: bool,
    omit_bind: bool,
    names: bool,
    wait: Option<f64>,
    positional: Vec<String>,
}

/// The original's options: single letters that may be joined, three of
/// which take a value written right after them, with or without `=`.
fn parse(args: &[String]) -> Result<Options, String> {
    let mut o = Options::default();
    for arg in args {
        let Some(cluster) = arg.strip_prefix('-').filter(|c| !c.is_empty()) else {
            o.positional.push(arg.clone());
            continue;
        };
        let mut chars = cluster.chars();
        while let Some(flag) = chars.next() {
            let rest = chars.as_str();
            let value = rest.strip_prefix('=').unwrap_or(rest);
            match flag {
                'v' => o.verbose = true,
                'r' => o.receive = true,
                'e' => o.echo = true,
                'c' => o.connect = true,
                'P' => o.promisc = true,
                'b' => o.rebind = true,
                'B' => o.broadcast = true,
                'o' => o.omit_bind = true,
                'n' => o.names = true,
                's' => {
                    // Checked here, before that many bytes are made up: the
                    // daemon would refuse a longer message anyway.
                    o.send = Some(match value {
                        "" => 8,
                        v => match v.parse() {
                            Ok(len) if len <= MAX_ETP_SIZE => len,
                            Ok(_) => {
                                return Err(format!(
                                    "-s: a message is at most {MAX_ETP_SIZE} bytes long"
                                ))
                            }
                            Err(_) => return Err(format!("-s: '{v}' is not a length")),
                        },
                    });
                    break;
                }
                'p' => {
                    o.priority = Some(match value.parse() {
                        Ok(p) if p <= 7 => p,
                        _ => return Err(format!("-p: '{value}' is not a priority (0 to 7)")),
                    });
                    break;
                }
                'w' => {
                    o.wait = Some(match value {
                        "" => 1.0,
                        v => v.parse().map_err(|_| format!("-w: '{v}' is not a time"))?,
                    });
                    break;
                }
                other => return Err(format!("invalid option -- '{other}'")),
            }
        }
    }
    Ok(o)
}

fn text(addr: &J1939Addr) -> String {
    format!(
        "{{ name {:#x}, pgn {:#x}, addr {:#x} }}",
        addr.name, addr.pgn, addr.addr
    )
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help" || a == "-?") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let o = match parse(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{TOOL}: {e}\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let spec = |i: usize| -> Result<Spec, String> {
        o.positional
            .get(i)
            .map_or(Ok(Spec::none()), |s| parse_spec(s))
    };
    let (from, to) = match (spec(0), spec(1)) {
        (Ok(from), Ok(to)) if o.positional.len() <= 2 => (from, to),
        (Err(e), _) | (_, Err(e)) => return fail(TOOL, e),
        _ => return fail(TOOL, "expected FROM and TO"),
    };
    let Some(ifname) = from.ifname.clone().or(to.ifname.clone()) else {
        return fail(
            TOOL,
            "an interface has to be named, as in: testj1939 -r can0:0x90",
        );
    };
    let say = |call: String| {
        if o.verbose {
            eprintln!("- {call}");
        }
    };

    let run = (|| -> darwincan::Result<()> {
        let bound = if o.omit_bind {
            J1939Addr::NONE
        } else {
            from.addr
        };
        let first = if o.rebind {
            J1939Addr {
                addr: bound.addr.wrapping_add(1),
                ..bound
            }
        } else {
            bound
        };
        say(format!("dcan_j1939_open({ifname}, {});", text(&first)));
        let sock = J1939Socket::open(&ifname, first)?;
        if o.promisc {
            say("dcan_j1939_set_promisc(, 1);".into());
            sock.set_promisc(true)?;
        }
        if o.broadcast {
            say("dcan_j1939_set_broadcast(, 1);".into());
            sock.set_broadcast(true)?;
        }
        if let Some(priority) = o.priority {
            say(format!("dcan_j1939_set_send_prio(, {priority});"));
            sock.set_send_priority(priority)?;
        }
        if o.rebind {
            say(format!("dcan_j1939_bind(, {});", text(&bound)));
            sock.bind(bound)?;
        }
        if o.connect {
            say(format!("dcan_j1939_connect(, {});", text(&to.addr)));
            sock.connect(to.addr)?;
        }
        if let Some(len) = o.send {
            let data = testj1939_data(len);
            // A message of several frames is seen through before the
            // socket closes, which would give it up.
            sock.set_notices(len > 8)?;
            if o.connect {
                say(format!("dcan_j1939_send(, <dat>, {len});"));
                sock.send(&data)?;
            } else {
                say(format!(
                    "dcan_j1939_sendto(, <dat>, {len}, {});",
                    text(&to.addr)
                ));
                sock.send_to(&data, &to.addr)?;
            }
            // Nothing is told of a single frame.
            let mut pending = len > 8;
            while pending {
                if let J1939Event::Notice(n) = sock.recv_event()? {
                    match n.kind {
                        J1939NoticeKind::TxDone => pending = false,
                        J1939NoticeKind::TxAborted => {
                            return Err(darwincan::Error::Io(std::io::Error::other(format!(
                                "the message was given up (abort reason {})",
                                n.reason
                            ))))
                        }
                        _ => {}
                    }
                }
            }
            sock.set_notices(false)?;
        }
        if !(o.receive || o.echo || o.wait.is_some()) {
            return sock.flush();
        }
        let end = o
            .wait
            .map(|secs| Instant::now() + Duration::from_secs_f64(secs.max(0.0)));
        say("while (1)".into());
        let mut out = std::io::stdout().lock();
        loop {
            if let Some(end) = end {
                let left = end.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    eprintln!("{TOOL}: exit as requested");
                    return Ok(());
                }
                sock.set_read_timeout(Some(left))?;
            }
            if !(o.receive || o.echo) {
                // Only waiting.
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            say("dcan_j1939_recv(, <dat>, , &info, );".into());
            let m = match sock.recv() {
                Ok(m) => m,
                Err(e) if e.is_timeout() => continue,
                Err(e) => return Err(e),
            };
            if o.echo {
                let back = J1939Addr::new(m.src.addr, m.src.pgn);
                say(format!(
                    "dcan_j1939_sendto(, <dat>, {}, {});",
                    m.data.len(),
                    text(&back)
                ));
                sock.send_to(&m.data, &back)?;
            }
            if o.receive {
                let dump = testj1939_dump(&m, o.names);
                if out
                    .write_all(dump.as_bytes())
                    .and_then(|()| out.flush())
                    .is_err()
                {
                    return Ok(());
                }
            }
        }
    })();
    match run {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(TOOL, e),
    }
}
