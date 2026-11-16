// SPDX-License-Identifier: MIT OR Apache-2.0

//! Read a data identifier from an ECU over ISO-TP (UDS service 0x22).
//!
//!     cargo run --example uds_read -- vcan0 7E0 7E8 F190
//!
//! Segmentation, flow control and STmin are handled by the daemon; the
//! program sends a request and receives a response.

use std::error::Error;
use std::time::Duration;

use darwincan::{IsoTpParams, IsoTpSocket};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [ifname, tx_id, rx_id, did] = args.as_slice() else {
        return Err("usage: uds_read <interface> <tx id> <rx id> <data identifier>".into());
    };
    let tx_id = u32::from_str_radix(tx_id, 16)?;
    let rx_id = u32::from_str_radix(rx_id, 16)?;
    let did = u16::from_str_radix(did, 16)?;

    let ecu = IsoTpSocket::open(ifname, IsoTpParams::new(tx_id, rx_id))?;
    ecu.set_read_timeout(Some(Duration::from_secs(2)))?;

    let [high, low] = did.to_be_bytes();
    // Returns once the ECU has the whole request.
    ecu.send_blocking(&[0x22, high, low])?;
    loop {
        let response = ecu.recv()?;
        // 7F 22 78: "response pending", the real answer follows.
        if response == [0x7F, 0x22, 0x78] {
            continue;
        }
        let hex: Vec<String> = response.iter().map(|b| format!("{b:02X}")).collect();
        println!("{}", hex.join(" "));
        return Ok(());
    }
}
