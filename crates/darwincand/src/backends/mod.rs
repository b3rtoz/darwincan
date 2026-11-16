// SPDX-License-Identifier: GPL-2.0-or-later

//! Hardware backends and the hotplug manager that attaches them.

pub mod kvaser_hydra;
pub mod pcan_usb;
pub mod peak_fd;
pub mod timesync;

use std::collections::HashSet;
use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use futures_core::Stream;
use nusb::hotplug::HotplugEvent;
use nusb::transfer::{Bulk, Out, TransferError};
use nusb::{DeviceId, DeviceInfo, Endpoint, MaybeFuture};

use crate::block_on::block_on;
use crate::Daemon;

pub(crate) fn log(message: impl std::fmt::Display) {
    eprintln!("darwincand: {message}");
}

/// What the USB library calls the thread that delivers transfers.
const USB_LIBRARY_THREAD: &str = "nusb-events";

/// How long a frame may wait for room in an adapter's transmit buffer
/// (it fills up when nothing on the bus acknowledges our frames).
const TRANSMIT_TIMEOUT: Duration = Duration::from_millis(500);
/// Frames a channel may have handed to its adapter and not seen reported
/// sent. The PEAK adapters themselves hold about 35 (measured on a
/// PCAN-USB X6 and a classic PCAN-USB).
const TRANSMIT_WINDOW: usize = 32;

/// A channel's way out: frames go to the adapter without waiting for the
/// one before, and the adapter's reports of frames sent make room again.
pub(crate) struct Transmitter {
    state: Mutex<TransmitState>,
    room: Condvar,
}

struct TransmitState {
    endpoint: Endpoint<Bulk, Out>,
    /// Frames handed over and not yet reported sent.
    unconfirmed: usize,
}

impl Transmitter {
    pub(crate) fn new(endpoint: Endpoint<Bulk, Out>) -> Transmitter {
        Transmitter {
            state: Mutex::new(TransmitState {
                endpoint,
                unconfirmed: 0,
            }),
            room: Condvar::new(),
        }
    }

    /// Hand a packet to the adapter, waiting for room if the window is
    /// full. `packet` builds it once there is room, with the transmitter
    /// held, so packets are built in the order they go out.
    pub(crate) fn send(&self, packet: impl FnOnce() -> Vec<u8>) -> io::Result<()> {
        let give_up = Instant::now() + TRANSMIT_TIMEOUT;
        let mut state = self.state.lock().unwrap();
        loop {
            // Collect the transfers that have finished. One that failed
            // never reached the adapter, so no report will come for it.
            while state.endpoint.pending() > 0 {
                let Some(done) = state.endpoint.wait_next_complete(Duration::ZERO) else {
                    break;
                };
                match done.status {
                    Ok(()) | Err(TransferError::Cancelled) => {}
                    Err(e) => {
                        state.unconfirmed = state.unconfirmed.saturating_sub(1);
                        return Err(transmit_failure(e));
                    }
                }
            }
            if state.unconfirmed < TRANSMIT_WINDOW {
                break;
            }
            let left = give_up.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "adapter transmit buffer is full (is anything acknowledging on the bus, or is other traffic keeping it busy?)",
                ));
            }
            state = self.room.wait_timeout(state, left).unwrap().0;
        }
        state.endpoint.submit(packet().into());
        state.unconfirmed += 1;
        Ok(())
    }

    /// The adapter reported one frame sent.
    /// Hand a packet to an adapter that does not report what it sent, and
    /// wait until it has taken it.
    pub(crate) fn send_and_wait(&self, packet: Vec<u8>) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let done = state
            .endpoint
            .transfer_blocking(packet.into(), TRANSMIT_TIMEOUT);
        match done.status {
            Ok(()) => Ok(()),
            Err(TransferError::Cancelled) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "adapter transmit buffer is full (is anything acknowledging on the bus, or is other traffic keeping it busy?)",
            )),
            Err(e) => Err(transmit_failure(e)),
        }
    }

    pub(crate) fn confirmed(&self) {
        let mut state = self.state.lock().unwrap();
        state.unconfirmed = state.unconfirmed.saturating_sub(1);
        self.room.notify_one();
    }

    /// The channel left the bus: what was on its way is not going out.
    pub(crate) fn reset(&self) {
        let mut state = self.state.lock().unwrap();
        state.endpoint.cancel_all();
        state.unconfirmed = 0;
        self.room.notify_all();
    }
}

/// Why a frame could not be handed to an adapter. A transfer that fails
/// as an adapter is pulled out does so before the receive loop has marked
/// the interface removed, and macOS does not always say "disconnected".
pub(crate) fn transmit_failure(e: TransferError) -> io::Error {
    match e {
        TransferError::Disconnected => io::Error::new(io::ErrorKind::NotFound, "adapter unplugged"),
        e => io::Error::other(format!(
            "USB transfer failed while sending a frame ({e}); was the adapter unplugged?"
        )),
    }
}

/// Which adapter channel an interface is, in words that are the same
/// every time the adapter is attached: what its name is kept under.
///
/// An adapter is known by its serial number. One that has none of its
/// own (the classic PCAN-USB leaves the factory with all ones) is known
/// by where it is plugged in instead, and has another name in another
/// port.
pub(crate) fn identity(
    model: &str,
    serial: u32,
    info: &DeviceInfo,
    channel: Option<usize>,
) -> String {
    let adapter = if serial == 0 || serial == u32::MAX {
        let ports: Vec<String> = info.port_chain().iter().map(u8::to_string).collect();
        format!("{model} at USB {} port {}", info.bus_id(), ports.join("."))
    } else {
        format!("{model} serial {serial:08X}")
    };
    match channel {
        Some(n) => format!("{adapter} channel {n}"),
        None => adapter,
    }
}

/// Attach the right driver to a USB device, if we have one for it.
fn try_attach(daemon: &Daemon, info: &DeviceInfo) {
    if pcan_usb::matches(info) {
        match pcan_usb::attach(daemon, info) {
            Ok(name) => log(format!("{} attached as {name}", pcan_usb::describe(info))),
            Err(e) => log(format!("cannot use {}: {e}", pcan_usb::describe(info))),
        }
        return;
    }
    if kvaser_hydra::matches(info) {
        match kvaser_hydra::attach(daemon, info) {
            Ok(names) => log(format!(
                "{} attached as {}",
                kvaser_hydra::describe(info),
                names.join(", ")
            )),
            Err(e) => log(format!("cannot use {}: {e}", kvaser_hydra::describe(info))),
        }
        return;
    }
    if let Some(model) = peak_fd::model_for(info) {
        match peak_fd::attach(daemon, info, model) {
            Ok(names) => log(format!(
                "{} attached as {}",
                peak_fd::describe(info, model),
                names.join(", ")
            )),
            Err(e) => log(format!(
                "cannot use {}: {e}",
                peak_fd::describe(info, model)
            )),
        }
    }
}

/// How long to wait for the rest of an adapter that is several USB
/// devices once the first of them has turned up.
const ADAPTER_GATHER: Duration = Duration::from_secs(2);

/// The hub ports leading to the hub a device is plugged into.
fn hub(port_chain: &[u8]) -> &[u8] {
    &port_chain[..port_chain.len().saturating_sub(1)]
}

/// Whether two devices are the same model on the same hub, as the parts
/// of one adapter are.
fn parts_of_one_adapter(a: &DeviceInfo, b: &DeviceInfo) -> bool {
    a.vendor_id() == b.vendor_id()
        && a.product_id() == b.product_id()
        && a.bus_id() == b.bus_id()
        && hub(a.port_chain()) == hub(b.port_chain())
}

/// The order adapters are attached in when several are there at once:
/// by where they are plugged in, not by which the system found first.
fn by_place(a: &DeviceInfo, b: &DeviceInfo) -> std::cmp::Ordering {
    (a.bus_id(), a.port_chain()).cmp(&(b.bus_id(), b.port_chain()))
}

/// A device has turned up: attach it, unless that was done already.
///
/// An adapter that is several USB devices (the PCAN-USB X6 is three) has
/// them found in any order, so the first to turn up waits for the others
/// and they are attached in the order of their ports on the adapter's
/// hub. Its connectors then keep their places among its interfaces.
fn arrived(daemon: &Daemon, info: &DeviceInfo, seen: &mut HashSet<DeviceId>) {
    if seen.contains(&info.id()) {
        return;
    }
    let devices = peak_fd::model_for(info).map_or(1, peak_fd::Model::devices);
    if devices == 1 {
        seen.insert(info.id());
        try_attach(daemon, info);
        return;
    }
    let give_up = Instant::now() + ADAPTER_GATHER;
    let mut parts = loop {
        let parts: Vec<DeviceInfo> = match nusb::list_devices().wait() {
            Ok(all) => all.filter(|d| parts_of_one_adapter(d, info)).collect(),
            Err(_) => vec![info.clone()],
        };
        if parts.len() >= devices || Instant::now() >= give_up {
            break parts;
        }
        thread::sleep(Duration::from_millis(20));
    };
    if parts.len() < devices {
        log(format!(
            "only {} of the {devices} USB devices of an adapter turned up;              its interfaces may not be in the order of its connectors",
            parts.len()
        ));
    }
    parts.sort_by(by_place);
    for part in &parts {
        if seen.insert(part.id()) {
            try_attach(daemon, part);
        }
    }
}

/// Attach every supported adapter that is plugged in, then keep watching
/// for adapters being plugged in later. Unplugging is noticed by each
/// driver's own receive loop.
pub fn start_hotplug(daemon: Daemon) -> io::Result<()> {
    // Subscribe before listing so a device arriving in between is not missed
    // (it may be seen twice; the second time it is passed over).
    //
    // Watching makes the USB library start its one thread, which from
    // then on hands the daemon every adapter's transfers. It has to be
    // real-time like the threads it feeds. The library gives no hold on
    // it but its name.
    let mut watch = nusb::watch_devices().map_err(io::Error::other)?;
    if !crate::priority::make_named_thread_realtime(USB_LIBRARY_THREAD) {
        log(
            "could not give the USB library's thread real-time priority; \
             adapters' messages may reach the daemon some milliseconds late",
        );
    }
    let mut seen = HashSet::new();
    let mut present: Vec<DeviceInfo> = nusb::list_devices()
        .wait()
        .map_err(io::Error::other)?
        .collect();
    present.sort_by(by_place);
    for info in &present {
        arrived(&daemon, info, &mut seen);
    }
    thread::Builder::new()
        .name("darwincand-hotplug".into())
        .spawn(move || loop {
            match block_on(poll_fn(|cx| Pin::new(&mut watch).poll_next(cx))) {
                Some(HotplugEvent::Connected(info)) => arrived(&daemon, &info, &mut seen),
                Some(HotplugEvent::Disconnected(id)) => {
                    seen.remove(&id);
                }
                None => return,
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::hub;

    #[test]
    fn devices_side_by_side_share_a_hub() {
        // The three devices of a PCAN-USB X6 behind a dock, as macOS
        // reports them.
        assert_eq!(hub(&[1, 2, 1, 2]), hub(&[1, 2, 1, 1]));
        assert_eq!(hub(&[1, 2, 1, 3]), [1, 2, 1]);
        // A second adapter on the dock's next port is another hub.
        assert_ne!(hub(&[1, 2, 2, 1]), hub(&[1, 2, 1, 1]));
        // A device on a root port has no hub above it.
        assert_eq!(hub(&[4]), [] as [u8; 0]);
        assert_eq!(hub(&[]), [] as [u8; 0]);
    }
}
