//! Print the frames on an interface, with the daemon's timestamps.
//!
//!     cargo run --example dump -- vcan0 [count]
//!
//! Shows a receive loop, a filter, error frames and the count of frames
//! the program was too slow to read.

use std::error::Error;

use darwincan::{err, Filter, RawSocket};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let ifname = args.next().unwrap_or_else(|| "vcan0".into());
    let count: Option<usize> = args.next().map(|n| n.parse()).transpose()?;

    let can = RawSocket::open(&ifname)?;
    // Everything; Filter::new(0x100, 0x700) would pass identifiers 100 to 1FF.
    can.set_filters(&[Filter::ANY])?;
    // Error frames too: bus-off, error-passive, a restart after sleep.
    can.set_error_filter(err::MASK)?;

    let mut seen = 0;
    while count.map_or(true, |count| seen < count) {
        let rx = can.recv()?;
        let seconds = rx.timestamp_ns as f64 / 1e9;
        if rx.frame.is_error() {
            println!("({seconds:.6}) {ifname} error frame {}", rx.frame);
        } else {
            println!("({seconds:.6}) {ifname} {}", rx.frame);
        }
        if rx.dropped > 0 {
            eprintln!("{} frames lost so far: reading too slowly", rx.dropped);
        }
        seen += 1;
    }
    Ok(())
}
