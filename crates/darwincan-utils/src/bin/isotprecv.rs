// SPDX-License-Identifier: MIT OR Apache-2.0

use std::io::{self, Write};
use std::process::ExitCode;

use darwincan::{IsoTpEvent, IsoTpSocket};
use darwincan_utils::{fail, hex_bytes, parse_isotp_args, usage, ISOTP_OPTIONS};

const TOOL: &str = "isotprecv";

fn usage_text() -> String {
    format!(
        "\
Usage: isotprecv -s <id> -d <id> [options] <interface>

Receive ISO-TP messages and print each as hex bytes on one line.

Options:
{ISOTP_OPTIONS}
  -l            Keep receiving instead of exiting after the first message

Example:
  isotprecv -s 7E8 -d 7E0 -l vcan0"
    )
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{}", usage_text());
        return ExitCode::SUCCESS;
    }
    let parsed = match parse_isotp_args(args.into_iter(), &['l']) {
        Ok(p) => p,
        Err(e) => return usage(TOOL, e, &usage_text()),
    };
    let keep_going = parsed.switches.contains(&'l');

    let sock = match IsoTpSocket::open(&parsed.ifname, parsed.params) {
        Ok(s) => s,
        Err(e) => return fail(TOOL, e),
    };
    loop {
        match sock.recv_event() {
            Ok(IsoTpEvent::Data { data, .. }) => {
                let mut out = io::stdout().lock();
                if writeln!(out, "{}", hex_bytes(&data))
                    .and_then(|()| out.flush())
                    .is_err()
                {
                    return ExitCode::SUCCESS;
                }
                if !keep_going {
                    return ExitCode::SUCCESS;
                }
            }
            Ok(IsoTpEvent::RxFailed(reason)) => {
                eprintln!("{TOOL}: receive failed: {reason}");
                if !keep_going {
                    return ExitCode::FAILURE;
                }
            }
            Ok(IsoTpEvent::TxDone | IsoTpEvent::TxFailed(_)) => {}
            Err(e) => return fail(TOOL, e),
        }
    }
}
