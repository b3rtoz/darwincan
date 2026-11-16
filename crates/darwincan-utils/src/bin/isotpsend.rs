// SPDX-License-Identifier: MIT OR Apache-2.0

use std::io::Read;
use std::process::ExitCode;

use darwincan::IsoTpSocket;
use darwincan_utils::{fail, parse_hex_bytes, parse_isotp_args, usage, ISOTP_OPTIONS};

const TOOL: &str = "isotpsend";

fn usage_text() -> String {
    format!(
        "\
Usage: isotpsend -s <id> -d <id> [options] <interface>

Send one ISO-TP message. The payload is read from standard input as
whitespace-separated hex bytes.

Options:
{ISOTP_OPTIONS}

Example:
  echo 22 F1 90 | isotpsend -s 7E0 -d 7E8 vcan0"
    )
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{}", usage_text());
        return ExitCode::SUCCESS;
    }
    let parsed = match parse_isotp_args(args.into_iter(), &[]) {
        Ok(p) => p,
        Err(e) => return usage(TOOL, e, &usage_text()),
    };

    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        return fail(TOOL, e);
    }
    let data = match parse_hex_bytes(&input) {
        Ok(d) if d.is_empty() => return fail(TOOL, "no payload on standard input"),
        Ok(d) => d,
        Err(e) => return fail(TOOL, e),
    };

    let result =
        IsoTpSocket::open(&parsed.ifname, parsed.params).and_then(|sock| sock.send_blocking(&data));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(TOOL, e),
    }
}
