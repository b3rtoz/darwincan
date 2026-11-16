// SPDX-License-Identifier: GPL-2.0-only
//
// Driver for the classic PEAK-System PCAN-USB adapter. The device handling
// sequence follows the Linux kernel driver
// (drivers/net/can/usb/peak_usb/pcan_usb.c and pcan_usb_core.c),
// Copyright (C) 2003-2025 PEAK System-Technik GmbH,
// Author: Stéphane Grosjean.

//! PCAN-USB (classic CAN, SJA1000 controller).

pub mod proto;

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use darwincan_core::{BusConfig, Frame};
use nusb::transfer::{Buffer, Bulk, In, Out, TransferError};
use nusb::{DeviceInfo, Endpoint, MaybeFuture};

use self::proto::{Event, TickClock};
use super::log;
use super::peak_fd::proto::{overrun_error_frame, state_error_frame, BusState};
use super::timesync::TimeSync;
use super::Transmitter;
use crate::bittiming::{self, TimingLimits};
use crate::clock::now_ns;
use crate::{Backend, Daemon, InterfaceHandle};

const VENDOR_ID: u16 = 0x0C72;
const PRODUCT_ID: u16 = 0x000C;
const NAME: &str = "PCAN-USB";

const EP_CMD_OUT: u8 = 0x01;
const EP_CMD_IN: u8 = 0x81;
const EP_MSG_OUT: u8 = 0x02;
const EP_MSG_IN: u8 = 0x82;

/// The SJA1000 runs from a 16 MHz crystal divided by two.
const LIMITS: TimingLimits = TimingLimits {
    clock_hz: 8_000_000,
    tseg1_min: 1,
    tseg1_max: 16,
    tseg2_min: 1,
    tseg2_max: 8,
    sjw_max: 4,
    brp_min: 1,
    brp_max: 64,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
/// The adapter needs this long after going on the bus.
const STARTUP_DELAY: Duration = Duration::from_millis(10);
/// Receive transfers kept waiting in the kernel, so the adapter can go on
/// delivering while the daemon is not being run.
const RX_TRANSFERS: usize = 32;
/// From this hardware revision on the adapter can send a transmitted
/// frame back (the reference driver's condition for its loopback mode).
const REPORTS_FROM_REVISION: u8 = 41;

pub fn matches(info: &DeviceInfo) -> bool {
    info.vendor_id() == VENDOR_ID && info.product_id() == PRODUCT_ID
}

pub fn describe(info: &DeviceInfo) -> String {
    format!(
        "{NAME} (USB bus {} address {})",
        info.bus_id(),
        info.device_address()
    )
}

fn usb_error(what: &str, e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{what}: {e}"))
}

/// The command endpoints, used by one caller at a time.
struct Commands {
    out: Endpoint<Bulk, Out>,
    answers: Endpoint<Bulk, In>,
}

struct Adapter {
    /// The message IN endpoint, until the receive loop takes it.
    messages: Mutex<Option<Endpoint<Bulk, In>>>,
    /// Hardware revision: the high byte of the USB device release number.
    revision: u8,
    description: String,
    commands: Mutex<Commands>,
    /// The number of the next transmit packet.
    tx_lock: Mutex<u8>,
    /// For hardware that reports the frames it sent.
    transmitter: Transmitter,
    /// The frames handed over and not yet reported sent, oldest first.
    unreported: Mutex<Unreported>,
    listen_only: AtomicBool,
    /// Counts calls to `start`, so the receive loop notices a restart.
    starts: AtomicU64,
    gone: AtomicBool,
}

impl Adapter {
    fn check_present(&self) -> io::Result<()> {
        if self.gone.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::NotFound, "adapter unplugged"));
        }
        Ok(())
    }

    fn send_command(&self, command: [u8; proto::CMD_LEN]) -> io::Result<()> {
        self.check_present()?;
        Self::send_command_locked(&mut self.commands.lock().unwrap(), command)
    }

    fn send_command_locked(
        commands: &mut Commands,
        command: [u8; proto::CMD_LEN],
    ) -> io::Result<()> {
        let done = commands
            .out
            .transfer_blocking(command.to_vec().into(), COMMAND_TIMEOUT);
        match done.status {
            Ok(()) => Ok(()),
            // The library cancels a transfer that runs out of time.
            Err(TransferError::Cancelled) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "adapter did not accept a command",
            )),
            Err(e) => Err(usb_error("sending a command", e)),
        }
    }

    /// Send a command and read the adapter's answer to it.
    fn query(&self, command: [u8; proto::CMD_LEN]) -> io::Result<Vec<u8>> {
        self.check_present()?;
        let mut commands = self.commands.lock().unwrap();
        Self::send_command_locked(&mut commands, command)?;
        // A receive transfer has to be a whole number of USB packets.
        let room = commands.answers.max_packet_size().max(proto::CMD_LEN);
        let done = commands
            .answers
            .transfer_blocking(Buffer::new(room), COMMAND_TIMEOUT);
        match done.status {
            Ok(()) => Ok(done.buffer[..done.actual_len].to_vec()),
            Err(TransferError::Cancelled) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "adapter did not answer a command",
            )),
            Err(e) => Err(usb_error("reading a command's answer", e)),
        }
    }

    /// Go off the bus and hold the controller in reset.
    fn bus_off(&self) -> io::Result<()> {
        self.send_command(proto::set_bus(false))?;
        self.send_command(proto::set_controller_init())
    }
}

/// The frames the adapter has yet to report sent. It sends them in the
/// order given, and returns with each report the byte that went with the
/// frame, which is how a report finds its frame here.
#[derive(Default)]
struct Unreported {
    frames: VecDeque<(u8, u32)>,
    next_byte: u8,
    /// Frames given up as never to be reported, since the caller last
    /// asked: each still holds a place in the transmit window.
    given_up: usize,
}

impl Unreported {
    /// Remember a frame's token and return the byte to send with it.
    fn add(&mut self, token: u32) -> u8 {
        // The reference driver sends 0x80; keep that bit and count in
        // the rest.
        let byte = 0x80 | self.next_byte;
        self.next_byte = (self.next_byte + 1) & 0x7F;
        self.frames.push_back((byte, token));
        byte
    }

    /// The token of the frame a report belongs to. Frames ahead of it in
    /// the queue will not be reported any more. `None` if the frame is
    /// not known (it was handed over before the last restart).
    fn take(&mut self, byte: Option<u8>) -> Option<u32> {
        let at = match byte {
            Some(byte) => self.frames.iter().position(|&(b, _)| b == byte)?,
            // A remote frame comes back without its byte: the oldest.
            None => 0,
        };
        self.frames.drain(..at);
        self.given_up += at;
        self.frames.pop_front().map(|(_, token)| token)
    }

    /// How many frames were given up since this was last asked.
    fn given_up(&mut self) -> usize {
        std::mem::take(&mut self.given_up)
    }
}

struct Channel(Arc<Adapter>);

impl Adapter {
    fn reports(&self) -> bool {
        self.revision >= REPORTS_FROM_REVISION
    }

    /// Forget what was on its way out: the adapter is leaving the bus.
    fn reset_transmit(&self) {
        self.transmitter.reset();
        self.unreported.lock().unwrap().frames.clear();
    }
}

impl Backend for Channel {
    fn kind(&self) -> &str {
        "pcan_usb"
    }

    fn description(&self) -> String {
        self.0.description.clone()
    }

    fn start(&self, config: &BusConfig) -> io::Result<()> {
        let adapter = &*self.0;
        let invalid = |e: String| io::Error::new(io::ErrorKind::InvalidInput, e);
        if config.data_bitrate.is_some() {
            return Err(invalid(format!("the {NAME} does not support CAN FD")));
        }
        // Older hardware has no listen-only mode. Refuse rather than go on
        // the bus acknowledging frames.
        if config.listen_only && adapter.revision <= 3 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "this {NAME} (hardware revision {}) has no listen-only mode",
                    adapter.revision
                ),
            ));
        }
        let timing =
            bittiming::calculate(&LIMITS, config.bitrate, config.sample_point.map(u32::from))
                .map_err(invalid)?;

        adapter.send_command(proto::set_bittiming(&timing))?;
        adapter.reset_transmit();
        adapter.starts.fetch_add(1, Ordering::AcqRel);
        adapter.send_command(proto::report_error_counters())?;
        if adapter.revision > 3 {
            adapter.send_command(proto::set_silent(config.listen_only))?;
        }
        adapter.send_command(proto::set_ext_vcc(false))?;
        adapter.send_command(proto::set_bus(true))?;
        thread::sleep(STARTUP_DELAY);

        adapter
            .listen_only
            .store(config.listen_only, Ordering::Release);
        Ok(())
    }

    fn stop(&self) -> io::Result<()> {
        let stopped = self.0.bus_off();
        self.0.reset_transmit();
        stopped
    }

    fn confirms_transmission(&self) -> bool {
        self.0.reports()
    }

    fn transmit(&self, frame: &Frame, token: u32) -> io::Result<()> {
        let adapter = &*self.0;
        adapter.check_present()?;
        if adapter.listen_only.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "interface is listen-only",
            ));
        }
        if frame.is_error() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "error frames cannot be transmitted",
            ));
        }
        if frame.is_fd() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the {NAME} does not support CAN FD"),
            ));
        }
        if adapter.reports() {
            return adapter.transmitter.send(|| {
                let mut counter = adapter.tx_lock.lock().unwrap();
                let byte = adapter.unreported.lock().unwrap().add(token);
                let packet = proto::encode_tx(frame, *counter, Some(byte));
                *counter = counter.wrapping_add(1);
                packet.to_vec()
            });
        }
        // Older hardware: wait for the adapter to take each frame.
        let mut counter = adapter.tx_lock.lock().unwrap();
        let packet = proto::encode_tx(frame, *counter, None);
        adapter.transmitter.send_and_wait(packet.to_vec())?;
        *counter = counter.wrapping_add(1);
        Ok(())
    }
}

/// Take over an adapter and register it as an interface. Returns the
/// interface name.
pub fn attach(daemon: &Daemon, info: &DeviceInfo) -> io::Result<String> {
    let device = info
        .open()
        .wait()
        .map_err(|e| usb_error("opening the device", e))?;
    let interface = device.claim_interface(0).wait().map_err(|e| {
        usb_error(
            "claiming the USB interface (is another application using the adapter?)",
            e,
        )
    })?;

    let endpoint = |e| usb_error("opening an endpoint", e);
    let mut adapter = Adapter {
        transmitter: Transmitter::new(interface.endpoint(EP_MSG_OUT).map_err(endpoint)?),
        unreported: Mutex::new(Unreported::default()),
        messages: Mutex::new(Some(interface.endpoint(EP_MSG_IN).map_err(endpoint)?)),
        commands: Mutex::new(Commands {
            out: interface.endpoint(EP_CMD_OUT).map_err(endpoint)?,
            answers: interface.endpoint(EP_CMD_IN).map_err(endpoint)?,
        }),
        revision: (info.device_version() >> 8) as u8,
        description: String::new(),
        tx_lock: Mutex::new(0),
        listen_only: AtomicBool::new(false),
        starts: AtomicU64::new(0),
        gone: AtomicBool::new(false),
    };
    let serial = proto::parse_serial(&adapter.query(proto::get_serial())?)
        .ok_or_else(|| io::Error::other("the serial number answer is too short"))?;
    adapter.description = format!(
        "{NAME} (serial {serial:08X}, hardware revision {})",
        adapter.revision
    );
    // Whatever state a previous user left it in, start off the bus.
    adapter.bus_off()?;

    let adapter = Arc::new(adapter);
    let handle = daemon.add_can_interface(
        &super::identity(NAME, serial, info, None),
        Box::new(Channel(adapter.clone())),
    );
    let name = handle.name().to_string();
    thread::Builder::new()
        .name("darwincand-pcan-rx".into())
        .spawn(move || receive_loop(adapter, handle))?;
    Ok(name)
}

/// What the receive loop keeps between packets.
struct RxState {
    clock: TickClock,
    sync: TimeSync,
    state: BusState,
    /// The flags of the last status record.
    flags: u8,
    tx_errors: u8,
    rx_errors: u8,
    /// The records of the packet being handled, with the adapter's time
    /// for those that carry one. Kept to reuse its storage.
    events: Vec<(Event, Option<u64>)>,
}

impl RxState {
    fn new() -> RxState {
        RxState {
            clock: TickClock::new(),
            sync: TimeSync::new(),
            state: BusState::Active,
            flags: 0,
            tx_errors: 0,
            rx_errors: 0,
            events: Vec::new(),
        }
    }

    /// Note what a record stamped `ticks`, in a packet that arrived at
    /// `arrived`, says about the adapter's clock. Returns the adapter's
    /// time for it.
    fn observe(&mut self, ticks: u16, arrived: u64) -> u64 {
        let device_ns = self.clock.extend(ticks, arrived);
        self.sync.observe(device_ns, arrived);
        device_ns
    }

    /// Handle one packet's records. The whole packet is examined for what
    /// it says about the adapter's clock before anything in it is stamped.
    fn handle_packet(&mut self, arrived: u64, adapter: &Adapter, handle: &InterfaceHandle) {
        let mut events = std::mem::take(&mut self.events);
        for (event, device_ns) in &mut events {
            *device_ns = match *event {
                Event::Frame { ticks, .. } | Event::Sent { ticks, .. } | Event::Sync { ticks } => {
                    Some(self.observe(ticks, arrived))
                }
                Event::Error { ticks, .. } => ticks.map(|t| self.observe(t, arrived)),
                Event::ErrorCounters { .. } => None,
            };
        }
        if let Some(latest) = events.iter().filter_map(|(_, ns)| *ns).max() {
            let late = arrived.saturating_sub(self.sync.stamp(latest));
            crate::timing::late(late, || {
                format!("{NAME}: its messages reached the daemon late by")
            });
        }
        for (event, device_ns) in events.drain(..) {
            let ts = device_ns.map(|ns| self.sync.stamp(ns));
            match event {
                Event::Frame { frame, .. } => handle.receive(&frame, ts),
                Event::Sent { frame, id, .. } => {
                    adapter.transmitter.confirmed();
                    let (token, given_up) = {
                        let mut unreported = adapter.unreported.lock().unwrap();
                        (unreported.take(id), unreported.given_up())
                    };
                    // A frame passed over will not be reported: its place
                    // in the window would otherwise be lost for good.
                    for _ in 0..given_up {
                        adapter.transmitter.confirmed();
                    }
                    match token {
                        Some(token) => handle.transmitted(token, &frame, ts),
                        // Handed over before the last restart: it was on
                        // the bus all the same.
                        None => handle.receive(&frame, ts),
                    }
                }
                Event::Sync { .. } => {}
                Event::ErrorCounters { rx, tx } => {
                    self.rx_errors = rx;
                    self.tx_errors = tx;
                    // The adapter does not send another status record when
                    // the counters cross from warning to passive or back.
                    self.update_state(None, handle);
                }
                Event::Error { flags, .. } => {
                    if flags & proto::ERROR_RXQOVR != 0 {
                        handle.receive(&overrun_error_frame(), ts);
                    }
                    self.flags = flags;
                    self.update_state(ts, handle);
                }
            }
        }
        self.events = events;
    }

    /// Work out the bus state from the last status flags and counters, and
    /// announce it if it changed.
    fn update_state(&mut self, ts: Option<u64>, handle: &InterfaceHandle) {
        // The adapter reports one "heavy" level; the counters say whether
        // it is the warning or the passive one.
        let state = if self.flags & proto::ERROR_BUS_OFF != 0 {
            BusState::BusOff
        } else if self.flags & proto::ERROR_BUS_HEAVY != 0 {
            if self.tx_errors >= 128 || self.rx_errors >= 128 {
                BusState::Passive
            } else {
                BusState::Warning
            }
        } else {
            BusState::Active
        };
        if state != self.state {
            self.state = state;
            let frame = state_error_frame(state, self.tx_errors, self.rx_errors);
            handle.receive(&frame, ts);
        }
    }
}

/// Reads the adapter's message stream until it is unplugged.
fn receive_loop(adapter: Arc<Adapter>, handle: InterfaceHandle) {
    crate::priority::make_current_thread_realtime();
    let Some(mut messages) = adapter.messages.lock().unwrap().take() else {
        return;
    };
    // A receive transfer has to be a whole number of USB packets.
    let transfer_size = messages.max_packet_size().max(proto::MSG_LEN);
    let mut rx = RxState::new();
    let mut starts = 0;
    let mut consecutive_errors = 0;

    loop {
        while messages.pending() < RX_TRANSFERS {
            messages.submit(Buffer::new(transfer_size));
        }
        let Some(completion) = messages.wait_next_complete(Duration::from_secs(3600)) else {
            continue;
        };
        let arrived = now_ns();
        match completion.status {
            Ok(()) => consecutive_errors = 0,
            Err(TransferError::Disconnected) => break,
            Err(TransferError::Cancelled) => break,
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors > 10 {
                    log(format!("{NAME}: giving up after USB errors: {e}"));
                    break;
                }
                if matches!(e, TransferError::Stall) {
                    let _ = messages.clear_halt().wait();
                }
                thread::sleep(Duration::from_millis(10));
                continue;
            }
        }

        // Going on the bus starts the controller afresh.
        let now_starts = adapter.starts.load(Ordering::Acquire);
        if now_starts != starts {
            starts = now_starts;
            rx = RxState::new();
        }

        let data = &completion.buffer[..completion.actual_len];
        let result = proto::decode(data, |event| rx.events.push((event, None)));
        rx.handle_packet(arrived, &adapter, &handle);
        if result.is_err() {
            log(format!(
                "{NAME}: malformed message from the adapter ignored"
            ));
        }
    }

    adapter.gone.store(true, Ordering::Release);
    log(format!("{} removed ({NAME})", handle.name()));
    handle.remove();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_finds_its_frame_by_the_byte_sent_with_it() {
        let mut u = Unreported::default();
        let (a, b, c) = (u.add(10), u.add(11), u.add(12));
        assert_eq!((a, b, c), (0x80, 0x81, 0x82));
        assert_eq!(u.take(Some(a)), Some(10));
        assert_eq!(u.given_up(), 0);
        // A report for a later frame: the one before it is given up.
        assert_eq!(u.take(Some(c)), Some(12));
        assert_eq!(u.given_up(), 1);
        assert_eq!(u.given_up(), 0);
        assert_eq!(u.take(Some(b)), None);
        // A remote frame's report carries no byte: it is the oldest.
        let d = u.add(13);
        u.add(14);
        assert_eq!(u.take(None), Some(13));
        assert_eq!(u.take(Some(d)), None);
        assert_eq!(u.take(None), Some(14));
        assert_eq!(u.take(None), None);
        // The count goes round within seven bits.
        u.next_byte = 0x7F;
        assert_eq!((u.add(1), u.add(2)), (0xFF, 0x80));
    }
}
