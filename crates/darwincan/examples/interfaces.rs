// SPDX-License-Identifier: MIT OR Apache-2.0

//! List the daemon's interfaces, and optionally put one on the bus.
//!
//!     cargo run --example interfaces
//!     cargo run --example interfaces -- can0 500000
//!
//! What `canctl link show` and `canctl link set can0 up type can bitrate
//! 500000` do, from a program.

use std::error::Error;

use darwincan::{BusConfig, Control, Error as CanError, ErrorCode};

fn main() -> Result<(), Box<dyn Error>> {
    let control = Control::connect()?;

    let mut args = std::env::args().skip(1);
    if let (Some(name), Some(bitrate)) = (args.next(), args.next()) {
        match control.up(&name, &BusConfig::new(bitrate.parse()?)) {
            Ok(()) => println!("{name} is up"),
            // The daemon says why in words; the code is for the program.
            Err(CanError::Daemon {
                code: ErrorCode::NoSuchInterface,
                message,
            }) => {
                return Err(format!("{message} (is the adapter plugged in?)").into());
            }
            Err(e) => return Err(e.into()),
        }
    }

    for interface in control.interfaces()? {
        let state = match (&interface.config, interface.up) {
            (Some(config), true) => format!("up at {} bit/s", config.bitrate),
            (_, true) => "up".to_string(),
            (_, false) => "down".to_string(),
        };
        println!(
            "{:<8} {:<12} {state}  {}",
            interface.name, interface.kind, interface.description
        );
    }
    Ok(())
}
