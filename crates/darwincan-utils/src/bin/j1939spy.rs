// SPDX-License-Identifier: MIT OR Apache-2.0

//! `j1939spy`: prints the J1939 messages on an interface, one to a line,
//! as can-utils' program of that name does.

use std::io::Write;
use std::process::ExitCode;

use darwincan::{J1939Addr, J1939Socket};
use darwincan_utils::j1939::{parse_hex_spec, source_filter, spy_line, SpyClock, SpyTime};
use darwincan_utils::{fail, getopt, long_options, usage, wants_help};

const TOOL: &str = "j1939spy";

const USAGE: &str = "\
Usage: j1939spy [OPTION...] [[IFACE:][NAME|SA][,PGN]]

An SAE J1939 spy utility: prints each message with its sender, its
parameter group, whom it was sent to, its priority, its length and its
data. NAME, SA and PGN are in hex and pick out one sender and one
parameter group. An interface has to be named.

Options:
  -v, --verbose         Increase verbosity
  -P, --promisc         Run in promiscuous mode
                        (= receive traffic not for this ECU)
  -b, --block=SIZE      Use a receive buffer of SIZE (default 1024)
  -t, --time[=a|d|z|A]  Show time: (a)bsolute, (d)elta, (z)ero, (A)bsolute w date

Without -P nothing is shown: the socket has no address of its own.

Example:
  j1939spy -P -ta can0
  j1939spy -P can0:80,FEF1";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if wants_help(&args) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    // The time option's letter is optional and, if given, attached.
    let args = args.into_iter().map(|a| match a.as_str() {
        "-t" | "--time" => "-tz".to_string(),
        _ => match a.strip_prefix("--time=") {
            Some(letter) => format!("-t{letter}"),
            None => a,
        },
    });
    let long = [("verbose", 'v'), ("promisc", 'P'), ("block", 'b')];
    let opts = match long_options(args, &long).and_then(|args| getopt(args, "vPb:t:")) {
        Ok(o) => o,
        Err(e) => return usage(TOOL, e, USAGE),
    };
    let mode = match opts.value('t').map(SpyTime::from_letter) {
        None => SpyTime::None,
        Some(Some(mode)) => mode,
        Some(None) => return usage(TOOL, "the time is shown as a, d, z or A", USAGE),
    };
    let block = match opts.value('b').map(str::parse::<usize>) {
        None => 1024,
        Some(Ok(size)) if size > 0 => size,
        Some(_) => return usage(TOOL, "-b needs a size in bytes", USAGE),
    };
    let spec = match opts.positional.as_slice() {
        [] => {
            return fail(
                TOOL,
                "an interface has to be named, as in: j1939spy -P can0",
            )
        }
        [spec] => match parse_hex_spec(spec) {
            Ok(spec) => spec,
            Err(e) => return usage(TOOL, e, USAGE),
        },
        _ => return usage(TOOL, "expected one [IFACE:][NAME|SA][,PGN]", USAGE),
    };
    let Some(ifname) = spec.ifname else {
        return fail(
            TOOL,
            "an interface has to be named, as in: j1939spy -P can0:80",
        );
    };

    let run = (|| -> darwincan::Result<()> {
        let sock = J1939Socket::open(&ifname, J1939Addr::NONE)?;
        if opts.has('P') {
            sock.set_promisc(true)?;
        }
        if let Some(filter) = source_filter(&spec.addr) {
            sock.set_filters(&[filter])?;
        }
        if opts.has('v') {
            eprintln!("{TOOL}: listening");
        }
        let mut clock = SpyClock::new(mode);
        let mut out = std::io::stdout().lock();
        loop {
            let m = sock.recv()?;
            let line = spy_line(&clock.stamp(m.timestamp_ns), &ifname, &m, block);
            if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
                // Nobody is reading any more.
                return Ok(());
            }
        }
    })();
    match run {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(TOOL, e),
    }
}
