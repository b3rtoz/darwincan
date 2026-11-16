// SPDX-License-Identifier: MIT OR Apache-2.0

//! Send and receive SAE J1939 messages.
//!
//!     cargo run --example j1939 -- vcan0
//!
//! Two sockets on one interface, as two controller applications: one at
//! address 0x80 and one at 0x90. The daemon's J1939 stack takes messages
//! of any length apart and puts them together (BAM, RTS/CTS, the extended
//! transport protocol), with the protocol's timing.

use std::error::Error;
use std::time::Duration;

use darwincan::{J1939Addr, J1939Socket};

/// A proprietary parameter group that is sent to one destination (PDU1).
const PGN_PROPRIETARY_A: u32 = 0x0EF00;
/// A parameter group that is sent to everyone (PDU2): vehicle speed.
const PGN_CCVS: u32 = 0x0FEF1;

fn main() -> Result<(), Box<dyn Error>> {
    let ifname = std::env::args().nth(1).unwrap_or_else(|| "vcan0".into());

    let engine = J1939Socket::open(&ifname, J1939Addr::address(0x80))?;
    let display = J1939Socket::open(&ifname, J1939Addr::address(0x90))?;
    display.set_read_timeout(Some(Duration::from_secs(2)))?;

    // To one address: 100 bytes do not fit a frame, so the stack opens a
    // transport session (RTS/CTS) with the receiver.
    let report = [0x5A; 100];
    engine.send_to(&report, &J1939Addr::new(0x90, PGN_PROPRIETARY_A))?;
    let got = display.recv()?;
    println!(
        "from {:02X}: parameter group {:05X}, {} bytes",
        got.src.addr,
        got.src.pgn,
        got.data.len()
    );

    // To everyone. Both sending and receiving broadcasts have to be
    // allowed, as SO_BROADCAST does on Linux.
    engine.set_broadcast(true)?;
    display.set_broadcast(true)?;
    engine.send_to(&[0xFF; 8], &J1939Addr::broadcast(PGN_CCVS))?;
    let got = display.recv()?;
    println!(
        "from {:02X}: parameter group {:05X}, sent to everyone",
        got.src.addr, got.src.pgn
    );
    Ok(())
}
