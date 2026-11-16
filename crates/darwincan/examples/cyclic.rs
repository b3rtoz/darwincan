//! Send a frame at a fixed interval and change its data while it runs.
//!
//!     cargo run --example cyclic -- vcan0 123 20 2
//!
//! (interface, identifier in hex, interval in milliseconds, seconds to
//! run). The daemon's broadcast manager sends the frames from a real-time
//! timer, so they leave on time whether or not this program is scheduled.

use std::error::Error;
use std::thread;
use std::time::Duration;

use darwincan::{BcmMessage, BcmSocket, Frame, Id};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [ifname, id, interval_ms, seconds] = args.as_slice() else {
        return Err("usage: cyclic <interface> <id> <interval ms> <seconds>".into());
    };
    let id = Id::standard(u16::from_str_radix(id, 16)?).ok_or("identifier out of range")?;
    let interval_us = interval_ms.parse::<u64>()? * 1000;
    let seconds: u8 = seconds.parse()?;

    let bcm = BcmSocket::open(ifname)?;
    // The first frame leaves at once, the rest every interval.
    bcm.send(&BcmMessage::tx_cyclic(
        Frame::new(id, &[0, 0])?,
        interval_us,
    ))?;
    for second in 1..=seconds {
        thread::sleep(Duration::from_secs(1));
        // New data from the next frame on; the rhythm is not disturbed.
        bcm.send(&BcmMessage::tx_update(Frame::new(id, &[0, second])?))?;
    }
    // Dropping the socket ends its jobs; deleting one says so.
    bcm.send(&BcmMessage::tx_delete(id.to_can_id(), false))?;
    Ok(())
}
