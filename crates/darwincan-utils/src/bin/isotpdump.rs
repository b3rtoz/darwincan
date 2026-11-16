// SPDX-License-Identifier: MIT OR Apache-2.0

//! `isotpdump`: shows the frames of an ISO-TP exchange taken apart, as
//! can-utils' program of that name does. It only listens.

use std::io::Write;
use std::process::ExitCode;

use darwincan::{Filter, RawSocket, CAN_EFF_FLAG, CAN_EFF_MASK, CAN_SFF_MASK};
use darwincan_utils::isotpdump::{line, Options, Side};
use darwincan_utils::{fail, getopt, local_time, parse_can_id, usage, wants_help};

const TOOL: &str = "isotpdump";

const USAGE: &str = "\
Usage: isotpdump [options] <CAN interface>
Options:
         -s <can_id>  (source can_id. Use 8 digits for extended IDs)
         -d <can_id>  (destination can_id. Use 8 digits for extended IDs)
         -x <addr>    (extended addressing mode. Use 'any' for all addresses)
         -X <addr>    (extended addressing mode (rx addr). Use 'any' for all)
         -c           (color mode)
         -a           (print data also in ASCII-chars)
         -t <type>    (timestamp: (a)bsolute/(d)elta/(z)ero/(A)bsolute w date)
         -u           (print uds messages)

CAN IDs and addresses are given and expected in hexadecimal values.

UDS output contains a flag which provides information about the type of the
message.

Flags:
       [SRQ]  = Service Request
       [PSR]  = Positive Service Response
       [NRC]  = Negative Response Code
       [???]  = Unknown (not specified)

Example:
  isotpdump -s 7E0 -d 7E8 -a -ta can0";

/// An address of extended addressing from the command line: one byte, or
/// `any`.
fn address(text: &str) -> Result<Option<u8>, String> {
    if text == "any" {
        return Ok(None);
    }
    u8::from_str_radix(text, 16)
        .map(Some)
        .map_err(|_| format!("'{text}' is not an address (one byte in hex, or 'any')"))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if wants_help(&args) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let opts = match getopt(args, "s:d:x:X:cat:u") {
        Ok(o) => o,
        Err(e) => return usage(TOOL, e, USAGE),
    };
    let id = |flag: char| -> Result<u32, String> {
        let text = opts
            .value(flag)
            .ok_or(format!("-{flag} <can_id> is needed"))?;
        parse_can_id(text)
    };
    let (source, destination) = match (id('s'), id('d')) {
        (Ok(s), Ok(d)) => (s, d),
        (Err(e), _) | (_, Err(e)) => return usage(TOOL, e, USAGE),
    };
    // With one address given, the other end's is any.
    let addresses = match (opts.value('x').map(address), opts.value('X').map(address)) {
        (None, None) => None,
        (Some(Err(e)), _) | (_, Some(Err(e))) => return usage(TOOL, e, USAGE),
        (tx, rx) => Some((
            tx.and_then(Result::unwrap_or_default),
            rx.and_then(Result::unwrap_or_default),
        )),
    };
    let stamp = match opts.value('t') {
        None | Some("a" | "d" | "z" | "A") => opts.value('t').unwrap_or(""),
        Some(other) => return usage(TOOL, format!("unknown timestamp type: {other}"), USAGE),
    };
    let [ifname] = opts.positional.as_slice() else {
        return usage(TOOL, "expected one interface", USAGE);
    };
    let options = Options {
        extended: addresses.is_some(),
        ascii: opts.has('a'),
        uds: opts.has('u'),
        colour: opts.has('c'),
    };

    let run = (|| -> darwincan::Result<()> {
        let sock = RawSocket::open(ifname)?;
        let exact = |id: u32| {
            let mask = if id & CAN_EFF_FLAG != 0 {
                CAN_EFF_MASK
            } else {
                CAN_SFF_MASK
            };
            Filter::new(id, mask | CAN_EFF_FLAG)
        };
        sock.set_filters(&[exact(source), exact(destination)])?;
        let mut out = std::io::stdout().lock();
        let (mut first, mut last) = (None, 0u64);
        loop {
            let rx = sock.recv()?;
            let frame = &rx.frame;
            if frame.is_remote() || frame.is_error() {
                continue;
            }
            let side = if frame.can_id() & !CAN_EFF_FLAG == source & !CAN_EFF_FLAG
                && frame.is_extended() == (source & CAN_EFF_FLAG != 0)
            {
                Side::Source
            } else {
                Side::Destination
            };
            if let Some((tx, rx_addr)) = addresses {
                let wanted = if side == Side::Source { tx } else { rx_addr };
                let got = frame.data().first().copied();
                if wanted.is_some() && wanted != got {
                    continue;
                }
            }
            let micros = rx.timestamp_ns / 1_000;
            let start = *first.get_or_insert(micros);
            let text = match stamp {
                "" => String::new(),
                "a" => format!("({}.{:06}) ", micros / 1_000_000, micros % 1_000_000),
                "A" => {
                    let t = local_time(micros / 1_000_000);
                    format!(
                        "({:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:06}) ",
                        t.year,
                        t.month,
                        t.day,
                        t.hour,
                        t.minute,
                        t.second,
                        micros % 1_000_000
                    )
                }
                kind => {
                    let from = if kind == "d" && last != 0 {
                        last
                    } else {
                        start
                    };
                    let diff = micros.saturating_sub(from);
                    format!("({}.{:06}) ", diff / 1_000_000, diff % 1_000_000)
                }
            };
            last = micros;
            let shown = line(&text, ifname, frame, side, &options);
            if writeln!(out, "{shown}").and_then(|()| out.flush()).is_err() {
                return Ok(());
            }
        }
    })();
    match run {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(TOOL, e),
    }
}
