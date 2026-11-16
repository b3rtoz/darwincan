use std::process::ExitCode;
use std::time::Duration;

use darwincan::{Frame, RawSocket};
use darwincan_utils::{check_sent, fail, usage};

const TOOL: &str = "cansend";
const USAGE: &str = "\
Usage: cansend <interface> <frame>

Send one CAN frame.

Frame formats (hex; 3-digit id = 11-bit, 8-digit id = 29-bit):
  <id>#<data>            classic CAN, up to 8 bytes; '.' may separate bytes
  <id>#R[<len>]          remote request
  <id>##<flags><data>    CAN FD, up to 64 bytes; flags 1 = bit rate switch

Examples:
  cansend vcan0 123#DEADBEEF
  cansend vcan0 18DAF110#02.10.03
  cansend vcan0 123##1112233445566778899";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let [ifname, frame] = args.as_slice() else {
        return usage(TOOL, "expected an interface and a frame", USAGE);
    };
    let frame: Frame = match frame.parse() {
        Ok(f) => f,
        Err(e) => return usage(TOOL, format!("{frame}: {e}"), USAGE),
    };
    match send(ifname, &frame) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(TOOL, e),
    }
}

/// Send the frame and wait until the daemon has passed it to the
/// interface, so a refusal (interface down, bus-off, no room) is reported.
/// Like Linux, this does not wait for the frame to be on the bus: with no
/// other node to acknowledge it, the adapter keeps trying after we exit.
fn send(ifname: &str, frame: &Frame) -> Result<(), String> {
    let run = || -> darwincan::Result<()> {
        let sock = RawSocket::open(ifname)?;
        sock.set_read_timeout(Some(Duration::from_secs(2)))?;
        // Receive nothing: only failures come back.
        sock.set_filters(&[])?;
        sock.send(frame)?;
        check_sent(&sock)
    };
    run().map_err(|e| {
        if e.is_timeout() {
            "timed out waiting for the daemon to take the frame".to_string()
        } else {
            e.to_string()
        }
    })
}
