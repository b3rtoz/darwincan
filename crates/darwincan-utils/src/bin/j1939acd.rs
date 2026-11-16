// SPDX-License-Identifier: MIT OR Apache-2.0

//! `j1939acd`: claims an SAE J1939 address for a NAME and defends it, as
//! can-utils' program of that name does. Other programs then open their
//! J1939 sockets bound to the NAME alone.

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use darwincan::j1939::{NO_ADDR, NO_PGN};
use darwincan::{J1939Addr, J1939Socket};
use darwincan_utils::acd::{self, Claimer, MAX_ADDR};
use darwincan_utils::{ctime, fail, getopt, long_options, usage, wants_help};

const TOOL: &str = "j1939acd";

const USAGE: &str = "\
Usage: j1939acd [options] NAME [INTF]

An SAE J1939 address claiming daemon: claims an address for NAME on the
interface INTF (default can0), answers requests for it, and takes another
one when a NAME with priority claims it. Programs that open a J1939 socket
bound to NAME then send from, and receive for, whatever address it has.

Options:
  -v, --verbose          Increase verbosity
  -r, --range=RANGE      Ranges of source addresses
                         e.g. 80,50-100,200-210 (defaults to 0-253)
  -c, --cache=FILE       Cache file to save/restore the source address
  -a, --address=ADDRESS  Start with Source Address ADDRESS
  -p, --prefix=STR       Prefix to use when logging

NAME is the 64bit nodename

Examples:
  j1939acd -r 100,80-120 -c /tmp/1122334455667788.jacd 1122334455667788
  j1939acd -r 100,80-120 -c /tmp/1122334455667788.jacd 1122334455667788 vcan0";

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: i32) {
    STOP.store(true, Ordering::Relaxed);
}

fn catch_signals() {
    extern "C" {
        fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
    }
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    // SAFETY: the handler only stores to an atomic, which is allowed in
    // a signal handler, and it stays valid for the life of the program.
    unsafe {
        signal(SIGINT, on_signal);
        signal(SIGTERM, on_signal);
    }
}

fn number(s: &str) -> Option<u64> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if wants_help(&args) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let long = [
        ("verbose", 'v'),
        ("range", 'r'),
        ("cache", 'c'),
        ("address", 'a'),
        ("prefix", 'p'),
    ];
    let opts = match long_options(args, &long).and_then(|args| getopt(args, "vr:c:a:p:")) {
        Ok(o) => o,
        Err(e) => return usage(TOOL, e, USAGE),
    };
    let (name, ifname) = match opts.positional.as_slice() {
        [name] => (name.as_str(), "can0"),
        [name, ifname] => (name.as_str(), ifname.as_str()),
        _ => return usage(TOOL, "expected a NAME and at most one interface", USAGE),
    };
    let digits = name.strip_prefix("0x").unwrap_or(name);
    let name = match u64::from_str_radix(digits, 16) {
        Ok(name) if name != 0 => name,
        _ => return usage(TOOL, format!("'{name}' is not a 64-bit NAME in hex"), USAGE),
    };
    let allowed = match opts.value('r').map(acd::parse_ranges) {
        None => [true; 254],
        Some(Ok(allowed)) => allowed,
        Some(Err(e)) => return usage(TOOL, e, USAGE),
    };
    let cache = opts.value('c');
    let mut preferred = None;
    if let Some(path) = cache {
        // A file that is not there yet is written when the program ends.
        if let Ok(text) = std::fs::read_to_string(path) {
            preferred = acd::parse_cache(&text);
        }
    }
    if let Some(a) = opts.value('a') {
        match number(a) {
            Some(a) if a <= u64::from(MAX_ADDR) => preferred = Some(a as u8),
            _ => return usage(TOOL, format!("'{a}' is not an address (0 to 253)"), USAGE),
        }
    }
    let verbose = opts.count('v');
    if verbose >= 1 {
        eprintln!("- ready for {ifname}:{name:016x}");
    }

    let unbound = J1939Addr {
        name,
        pgn: NO_PGN,
        addr: NO_ADDR,
    };
    let sock = match J1939Socket::open(ifname, unbound) {
        Ok(sock) => sock,
        Err(e) => return fail(TOOL, e),
    };
    catch_signals();
    let mut claimer = Claimer::new(name, allowed, preferred);
    let run = acd::run(
        &sock,
        name,
        &mut claimer,
        &STOP,
        verbose,
        &mut std::io::stderr(),
    );

    let (code, message) = match run {
        Ok(end) => end,
        Err(e) => (1, Some(e.to_string())),
    };
    if let (Some(path), true) = (cache, code == 0) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let text = acd::cache_text(claimer.address(), &ctime(now));
        if let Err(e) = std::fs::write(path, text) {
            return fail(TOOL, format!("{path}: {e}"));
        }
    }
    match message {
        Some(message) => fail(TOOL, message),
        None => ExitCode::from(code),
    }
}
