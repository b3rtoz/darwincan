// SPDX-License-Identifier: MIT OR Apache-2.0

//! `cansniffer`: shows the latest data of each CAN identifier and marks
//! what changes, as can-utils' program of that name does.

use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use darwincan::RawSocket;
use darwincan_utils::sniffer::{Reply, Sniffer, View, HIDE_CURSOR, SHOW_CURSOR};
use darwincan_utils::{fail, getopt, usage};

const TOOL: &str = "cansniffer";

const USAGE: &str = "\
cansniffer - volatile CAN content visualizer.

Usage: cansniffer [can-interface]
Options:
         -q          (quiet - all IDs deactivated)
         -r <name>   (read sniffset.name from file)
         -e          (fix extended frame format output - no auto detect)
         -b          (start with binary mode)
         -8          (start with binary mode - for EFF on 80 chars)
         -B          (start with binary mode with gap - exceeds 80 chars!)
         -c          (color changes)
         -f <mode>   (CAN FD mode: 0 = OFF, 1 = ON, 2 = auto detect, default: 2)
         -t <time>   (timeout for ID display [x10ms] default: 500, 0 = OFF)
         -h <time>   (hold marker on changes [x10ms] default: 100)
         -l <time>   (loop time (display) [x10ms] default: 20)
         -?          (print this help text)
Use interface name 'any' to receive from all can-interfaces.

commands that can be entered at runtime:
 q<ENTER>        - quit
 b<ENTER>        - toggle binary / HEX-ASCII output
 8<ENTER>        - toggle binary / HEX-ASCII output (small for EFF on 80 chars)
 B<ENTER>        - toggle binary with gap / HEX-ASCII output (exceeds 80 chars!)
 c<ENTER>        - toggle color mode
 @<ENTER>        - toggle ASCII output (disabled for CAN FD by default)
 <SPACE><ENTER>  - force a clear screen
 #<ENTER>        - notch currently marked/changed bits (can be used repeatedly)
 *<ENTER>        - clear notched marked
 rMYNAME<ENTER>  - read settings file (filter/notch)
 wMYNAME<ENTER>  - write settings file (filter/notch)
 a<ENTER>        - enable 'a'll SFF CAN-IDs to sniff
 n<ENTER>        - enable 'n'one SFF CAN-IDs to sniff
 A<ENTER>        - enable 'A'll EFF CAN-IDs to sniff
 N<ENTER>        - enable 'N'one EFF CAN-IDs to sniff
 +FILTER<ENTER>  - add CAN-IDs to sniff
 -FILTER<ENTER>  - remove CAN-IDs to sniff

FILTER can be a single CAN-ID or a CAN-ID/Bitmask:

 single SFF 11 bit IDs:
  +1F5<ENTER>               - add SFF CAN-ID 0x1F5
  -42E<ENTER>               - remove SFF CAN-ID 0x42E

 single EFF 29 bit IDs:
  +18FEDF55<ENTER>          - add EFF CAN-ID 0x18FEDF55
  -00000090<ENTER>          - remove EFF CAN-ID 0x00000090

 CAN-ID/Bitmask SFF:
  -42E7FF<ENTER>            - remove SFF CAN-ID 0x42E (using Bitmask)
  -500700<ENTER>            - remove SFF CAN-IDs 0x500 - 0x5FF
  +400600<ENTER>            - add SFF CAN-IDs 0x400 - 0x5FF
  +000000<ENTER>            - add all SFF CAN-IDs
  -000000<ENTER>            - remove all SFF CAN-IDs

 CAN-ID/Bitmask EFF:
  -0000000000000000<ENTER>  - remove all EFF CAN-IDs
  +12345678000000FF<ENTER>  - add EFF CAN IDs xxxxxx78
  +0000000000000000<ENTER>  - add all EFF CAN-IDs

if (id & filter) == (sniff-id & filter) the action (+/-) is performed,
which is quite easy when the filter is 000.

Settings are kept in the file sniffset.<name> in the current directory.";

fn settings_file(name: &str) -> String {
    format!("sniffset.{name}")
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // -h takes a value here, as in the original: help is -?.
    if args.iter().any(|a| a == "-?" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let opts = match getopt(args, "qr:eb8Bcf:t:h:l:") {
        Ok(o) => o,
        Err(e) => return usage(TOOL, e, USAGE),
    };
    let [ifname] = opts.positional.as_slice() else {
        return usage(TOOL, "expected one interface", USAGE);
    };
    let mut sniffer = Sniffer::new(ifname);
    let time = |flag: char, default: u64| -> Result<u64, String> {
        match opts.value(flag) {
            None => Ok(default),
            Some(v) => v
                .parse()
                .map_err(|_| format!("-{flag}: '{v}' is not a time in hundredths of a second")),
        }
    };
    match (time('t', 500), time('h', 100), time('l', 20)) {
        (Ok(timeout), Ok(hold), Ok(interval)) if interval > 0 => {
            sniffer.timeout = timeout;
            sniffer.hold = hold;
            sniffer.interval = interval;
        }
        (Err(e), ..) | (_, Err(e), _) | (.., Err(e)) => return usage(TOOL, e, USAGE),
        _ => return usage(TOOL, "-l needs a time of at least 1", USAGE),
    }
    match opts.value('f') {
        None | Some("1" | "2") => {}
        Some("0") => sniffer.fd = false,
        Some(other) => return usage(TOOL, format!("-f: unknown CAN FD mode {other}"), USAGE),
    }
    if opts.has('q') {
        sniffer.quiet();
    }
    sniffer.wide = opts.has('e');
    sniffer.colour = opts.has('c');
    if opts.has('b') {
        sniffer.view = View::Binary;
    }
    if opts.has('8') {
        sniffer.view = View::BinaryNarrow;
    }
    if opts.has('B') {
        sniffer.view = View::BinaryGap;
    }
    if let Some(name) = opts.value('r') {
        match std::fs::read_to_string(settings_file(name)) {
            Ok(text) => sniffer.load(&text),
            Err(e) => return fail(TOOL, format!("{}: {e}", settings_file(name))),
        }
    }

    let sock = match RawSocket::open(ifname) {
        Ok(sock) => sock,
        Err(e) => return fail(TOOL, e),
    };
    // The keyboard is read on a thread of its own, a line at a time.
    let (typed, lines) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if typed.send(line).is_err() {
                break;
            }
        }
    });

    let mut out = std::io::stdout().lock();
    let started = Instant::now();
    let now_ms = || started.elapsed().as_millis() as u64;
    let mut write = |text: &str| -> bool {
        out.write_all(text.as_bytes())
            .and_then(|()| out.flush())
            .is_ok()
    };
    write(HIDE_CURSOR);
    let run = (|| -> darwincan::Result<()> {
        let mut next = Instant::now();
        loop {
            if !write(&sniffer.draw(now_ms())) {
                return Ok(());
            }
            next += Duration::from_millis(sniffer.interval * 10);
            loop {
                let left = next.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                sock.set_read_timeout(Some(left))?;
                match sock.recv() {
                    Ok(rx) => sniffer.on_frame(&rx.frame, now_ms()),
                    Err(e) if e.is_timeout() => {}
                    Err(e) => return Err(e),
                }
            }
            while let Ok(line) = lines.try_recv() {
                match sniffer.command(&line) {
                    Reply::Carry => {}
                    Reply::Quit => return Ok(()),
                    Reply::Write(name, text) => {
                        // A file that cannot be written is told of on the
                        // next line of the screen and does not end the
                        // program.
                        if let Err(e) = std::fs::write(settings_file(&name), text) {
                            write(&format!("\n{TOOL}: {}: {e}\n", settings_file(&name)));
                        }
                    }
                    Reply::Read(name) => match std::fs::read_to_string(settings_file(&name)) {
                        Ok(text) => sniffer.load(&text),
                        Err(e) => {
                            write(&format!("\n{TOOL}: {}: {e}\n", settings_file(&name)));
                        }
                    },
                }
            }
        }
    })();
    write(SHOW_CURSOR);
    match run {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(TOOL, e),
    }
}
