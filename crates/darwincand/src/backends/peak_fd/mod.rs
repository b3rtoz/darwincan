// SPDX-License-Identifier: GPL-2.0-only
//
// Driver for the PEAK-System PCAN-USB FD family. The device handling
// sequence follows the Linux kernel driver
// (drivers/net/can/usb/peak_usb/pcan_usb_fd.c),
// Copyright (C) 2013-2025 PEAK System-Technik GmbH,
// Author: Stéphane Grosjean.

//! PCAN-USB FD, PCAN-USB Pro FD, PCAN-USB X6 and PCAN-Chip USB.

pub mod proto;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use darwincan_core::{BusConfig, Frame};
use nusb::transfer::{
    Buffer, Bulk, ControlIn, ControlOut, ControlType, In, Out, Recipient, TransferError,
};
use nusb::{DeviceInfo, Endpoint, Interface, MaybeFuture, Speed};

use self::proto::{BusState, Commands, Event, FwInfo};
use super::log;
use super::timesync::TimeSync;
use super::Transmitter;
use crate::bittiming::{self, TimingLimits};
use crate::clock::now_ns;
use crate::{Backend, Daemon, InterfaceHandle};

const VENDOR_ID: u16 = 0x0C72;

pub struct Model {
    product_id: u16,
    name: &'static str,
    channels: u8,
    /// USB devices one adapter of this model presents itself as, side by
    /// side on a hub of its own.
    devices: u8,
}

impl Model {
    pub fn devices(&self) -> usize {
        usize::from(self.devices)
    }
}

const MODELS: [Model; 4] = [
    Model {
        product_id: 0x0012,
        name: "PCAN-USB FD",
        channels: 1,
        devices: 1,
    },
    Model {
        product_id: 0x0011,
        name: "PCAN-USB Pro FD",
        channels: 2,
        devices: 1,
    },
    Model {
        product_id: 0x0013,
        name: "PCAN-Chip USB",
        channels: 1,
        devices: 1,
    },
    // The X6 presents itself as three two-channel USB devices.
    Model {
        product_id: 0x0014,
        name: "PCAN-USB X6",
        channels: 2,
        devices: 3,
    },
];

/// The controller runs from an 80 MHz clock.
const NOMINAL_LIMITS: TimingLimits = TimingLimits {
    clock_hz: 80_000_000,
    tseg1_min: 1,
    tseg1_max: 256,
    tseg2_min: 1,
    tseg2_max: 128,
    sjw_max: 128,
    brp_min: 1,
    brp_max: 1024,
};
const DATA_LIMITS: TimingLimits = TimingLimits {
    tseg1_max: 32,
    tseg2_max: 16,
    sjw_max: 16,
    ..NOMINAL_LIMITS
};

// Vendor control requests.
const REQ_INFO: u8 = 0;
const REQ_FCT: u8 = 2;
const INFO_FIRMWARE: u16 = 1;
const FCT_DRIVER_LOADED: u16 = 5;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
const RX_TRANSFER_SIZE: usize = 2048;
/// Receive transfers kept waiting in the kernel. They are what lets the
/// adapter go on delivering while the daemon is not being run: with four,
/// a pause of 46 ms in a soak ended in the adapter dropping four frames
/// (receive overrun).
const RX_TRANSFERS: usize = 32;
/// Full-speed USB links carry commands in packets of this size.
const FULL_SPEED_PACKET: usize = 64;

pub fn model_for(info: &DeviceInfo) -> Option<&'static Model> {
    if info.vendor_id() != VENDOR_ID {
        return None;
    }
    MODELS.iter().find(|m| m.product_id == info.product_id())
}

pub fn describe(info: &DeviceInfo, model: &Model) -> String {
    format!(
        "{} (USB bus {} address {})",
        model.name,
        info.bus_id(),
        info.device_address()
    )
}

fn usb_error(what: &str, e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{what}: {e}"))
}

/// State shared by the channels of one adapter.
struct Device {
    interface: Interface,
    fw: FwInfo,
    model: &'static Model,
    high_speed: bool,
    /// The command endpoint, used by one caller at a time.
    commands: Mutex<Endpoint<Bulk, Out>>,
    /// The message IN endpoint, until the receive loop takes it.
    messages: Mutex<Option<Endpoint<Bulk, In>>>,
    /// One per channel.
    transmitters: Vec<Transmitter>,
    /// Which channels are on the bus. Held while one starts or stops.
    open: Mutex<[bool; 2]>,
    /// Set when the adapter's clock was restarted, for the receive loop.
    clock_restarted: AtomicBool,
    gone: AtomicBool,
}

impl Device {
    fn send_commands(&self, commands: &Commands) -> io::Result<()> {
        if self.gone.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::NotFound, "adapter unplugged"));
        }
        let packet = commands.finish();
        let mut endpoint = self.commands.lock().unwrap();
        // The firmware cannot reassemble a long command list over a
        // full-speed link, so feed it one packet at a time there.
        let chunk = if self.high_speed {
            packet.len()
        } else {
            FULL_SPEED_PACKET
        };
        for part in packet.chunks(chunk) {
            let done = endpoint.transfer_blocking(part.to_vec().into(), COMMAND_TIMEOUT);
            match done.status {
                Ok(()) => {}
                // The library cancels a transfer that runs out of time.
                Err(TransferError::Cancelled) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "adapter did not accept a command",
                    ))
                }
                Err(e) => return Err(usb_error("sending a command", e)),
            }
        }
        Ok(())
    }

    /// Tell the adapter whether a driver is in charge of it.
    fn set_driver_loaded(&self, loaded: bool) -> io::Result<()> {
        let mut data = [0u8; 16];
        data[1] = u8::from(loaded);
        self.interface
            .control_out(
                ControlOut {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Other,
                    request: REQ_FCT,
                    value: FCT_DRIVER_LOADED,
                    index: 0,
                    data: &data,
                },
                CONTROL_TIMEOUT,
            )
            .wait()
            .map_err(|e| usb_error("announcing the driver", e))
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if !self.gone.load(Ordering::Acquire) {
            for channel in 0..self.model.channels {
                let mut c = Commands::new(channel);
                c.reset_mode().led(proto::LED_OFF);
                let _ = self.send_commands(&c);
            }
            let _ = self.set_driver_loaded(false);
        }
    }
}

struct Channel {
    device: Arc<Device>,
    index: u8,
    description: String,
    listen_only: AtomicBool,
    fd: AtomicBool,
}

impl Channel {
    fn transmitter(&self) -> &Transmitter {
        &self.device.transmitters[usize::from(self.index)]
    }

    /// Configure the channel and put it on the bus. `restart_clock`
    /// switches the time calibration messages on even if the adapter's
    /// other channel is open.
    fn go_on_bus(&self, config: &BusConfig, restart_clock: bool) -> io::Result<()> {
        let invalid = |e: String| io::Error::new(io::ErrorKind::InvalidInput, e);
        let nominal = bittiming::calculate(
            &NOMINAL_LIMITS,
            config.bitrate,
            config.sample_point.map(u32::from),
        )
        .map_err(invalid)?;
        let data = config
            .data_bitrate
            .map(|bitrate| {
                bittiming::calculate(
                    &DATA_LIMITS,
                    bitrate,
                    config.data_sample_point.map(u32::from),
                )
            })
            .transpose()
            .map_err(invalid)?;
        if let Some(data) = &data {
            if data.bitrate < nominal.bitrate {
                return Err(invalid(
                    "the data bitrate must not be below the nominal bitrate".into(),
                ));
            }
        }

        let mut timing = Commands::new(self.index);
        timing.timing_slow(&nominal);
        if let Some(data) = &data {
            timing.timing_fast(data);
        }
        self.device.send_commands(&timing)?;
        self.device
            .send_commands(Commands::new(self.index).accept_all_standard_ids())?;

        // Time calibration messages belong to the adapter, not the channel:
        // they are switched on with the first channel to open and off with
        // the last to close, as the reference driver does. Switching them
        // on restarts the adapter's clock, so make sure it is a real
        // switch (a daemon that was killed leaves them on). After a
        // restore they are off whatever the other channel's state.
        let mut open = self.device.open.lock().unwrap();
        if !restart_clock && open.iter().any(|&o| o) {
            self.device
                .send_commands(Commands::new(self.index).error_messages(true))?;
        } else {
            self.device
                .send_commands(Commands::new(self.index).error_and_calibration_messages(false))?;
            self.device.clock_restarted.store(true, Ordering::Release);
            self.device
                .send_commands(Commands::new(self.index).error_and_calibration_messages(true))?;
        }

        self.transmitter().reset();
        let mut go = Commands::new(self.index);
        go.reset_error_counters();
        if self.device.fw.fw_version[0] >= 2 {
            // Older firmware is fixed to the pre-ISO CAN FD frame format.
            go.iso_fd();
        }
        go.operational(config.listen_only);
        self.device.send_commands(&go)?;

        self.listen_only
            .store(config.listen_only, Ordering::Release);
        self.fd.store(data.is_some(), Ordering::Release);
        open[usize::from(self.index)] = true;
        Ok(())
    }
}

impl Backend for Channel {
    fn kind(&self) -> &str {
        "peak_fd"
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn start(&self, config: &BusConfig) -> io::Result<()> {
        self.go_on_bus(config, false)
    }

    fn restore(&self, config: &BusConfig) -> io::Result<()> {
        let _ = self.stop();
        // The adapter has forgotten that calibration messages were on, and
        // without them its timestamps stand still. The other channel being
        // open no longer means they are on.
        self.go_on_bus(config, true)
    }

    fn stop(&self) -> io::Result<()> {
        let mut open = self.device.open.lock().unwrap();
        open[usize::from(self.index)] = false;
        let mut c = Commands::new(self.index);
        // Off the bus, then drop what was waiting to be sent: it would
        // otherwise go out when the channel is next started.
        c.reset_mode().flush_transmit_queue();
        if open.iter().any(|&o| o) {
            c.error_messages(false);
        } else {
            c.error_and_calibration_messages(false);
        }
        let stopped = self.device.send_commands(&c);
        self.transmitter().reset();
        stopped
    }

    fn confirms_transmission(&self) -> bool {
        true
    }

    fn transmit(&self, frame: &Frame, token: u32) -> io::Result<()> {
        if self.device.gone.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::NotFound, "adapter unplugged"));
        }
        if self.listen_only.load(Ordering::Acquire) {
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
        if frame.is_fd() && !self.fd.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "interface is not configured for CAN FD (no data bitrate set)",
            ));
        }
        self.transmitter()
            .send(|| proto::encode_tx(self.index, frame, token))
    }
}

/// Take over an adapter and register its channels as interfaces. Returns
/// the interface names.
pub fn attach(
    daemon: &Daemon,
    info: &DeviceInfo,
    model: &'static Model,
) -> io::Result<Vec<String>> {
    let device = info
        .open()
        .wait()
        .map_err(|e| usb_error("opening the device", e))?;
    // The CAN function is always USB interface 0.
    let interface = device.claim_interface(0).wait().map_err(|e| {
        usb_error(
            "claiming the USB interface (is another application using the adapter?)",
            e,
        )
    })?;

    let raw = interface
        .control_in(
            ControlIn {
                control_type: ControlType::Vendor,
                recipient: Recipient::Other,
                request: REQ_INFO,
                value: INFO_FIRMWARE,
                index: 0,
                length: proto::FW_INFO_LEN as u16,
            },
            CONTROL_TIMEOUT,
        )
        .wait()
        .map_err(|e| usb_error("reading firmware information", e))?;
    let fw =
        FwInfo::parse(&raw).ok_or_else(|| io::Error::other("firmware information is too short"))?;

    let endpoint = |e| usb_error("opening an endpoint", e);
    let mut transmitters = Vec::new();
    for channel in 0..usize::from(model.channels) {
        let out = interface
            .endpoint(fw.data_out_ep[channel])
            .map_err(endpoint)?;
        transmitters.push(Transmitter::new(out));
    }
    let device = Arc::new(Device {
        transmitters,
        commands: Mutex::new(interface.endpoint(fw.cmd_out_ep).map_err(endpoint)?),
        messages: Mutex::new(Some(interface.endpoint(fw.data_in_ep).map_err(endpoint)?)),
        interface,
        high_speed: matches!(
            info.speed(),
            Some(Speed::High | Speed::Super | Speed::SuperPlus)
        ),
        open: Mutex::new([false; 2]),
        clock_restarted: AtomicBool::new(false),
        gone: AtomicBool::new(false),
        model,
        fw,
    });
    device.set_driver_loaded(true)?;

    let serial = format!("{:08X}", device.fw.serial);
    let [major, minor, patch] = device.fw.fw_version;
    // An adapter that is several USB devices numbers its channels across
    // them, in the order of their ports on its hub: the order of the
    // connectors on its housing.
    let first = match info.port_chain().last() {
        Some(&port) if model.devices > 1 => {
            usize::from(port.saturating_sub(1)) * usize::from(model.channels)
        }
        _ => 0,
    };
    let numbered = model.channels > 1 || model.devices > 1;
    let mut handles = Vec::new();
    for index in 0..model.channels {
        let number = first + usize::from(index) + 1;
        // Whatever state a previous user left it in (a daemon that was
        // killed leaves its channels on the bus, perhaps with frames
        // waiting), start off the bus with nothing to send. Then
        // select the 80 MHz clock the bit timing is calculated for, and
        // leave the LED under device control.
        let mut init = Commands::new(index);
        init.reset_mode()
            .flush_transmit_queue()
            .clock(proto::CLOCK_80MHZ)
            .led(proto::LED_DEVICE_DEFAULT);
        device.send_commands(&init)?;

        let description = if numbered {
            format!(
                "{} channel {number} (serial {serial}, firmware {major}.{minor}.{patch})",
                model.name
            )
        } else {
            format!(
                "{} (serial {serial}, firmware {major}.{minor}.{patch})",
                model.name
            )
        };
        let identity = super::identity(
            model.name,
            device.fw.serial,
            info,
            numbered.then_some(number),
        );
        handles.push(daemon.add_can_interface(
            &identity,
            Box::new(Channel {
                device: device.clone(),
                index,
                description,
                listen_only: AtomicBool::new(false),
                fd: AtomicBool::new(false),
            }),
        ));
    }

    let names = handles.iter().map(|h| h.name().to_string()).collect();
    thread::Builder::new()
        .name("darwincand-peak-rx".into())
        .spawn(move || receive_loop(device, handles))?;
    Ok(names)
}

/// Per-channel state the receive loop keeps between records.
#[derive(Clone, Copy)]
struct ChannelState {
    state: BusState,
    tx_errors: u8,
    rx_errors: u8,
}

/// Reads the adapter's message stream until it is unplugged.
fn receive_loop(device: Arc<Device>, handles: Vec<InterfaceHandle>) {
    crate::priority::make_current_thread_realtime();
    let Some(mut messages) = device.messages.lock().unwrap().take() else {
        return;
    };
    let mut sync = TimeSync::new();
    let mut events: Vec<Event> = Vec::new();
    let mut channels = vec![
        ChannelState {
            state: BusState::Active,
            tx_errors: 0,
            rx_errors: 0,
        };
        handles.len()
    ];
    let mut consecutive_errors = 0;

    loop {
        while messages.pending() < RX_TRANSFERS {
            messages.submit(Buffer::new(RX_TRANSFER_SIZE));
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
                    log(format!(
                        "{}: giving up after USB errors: {e}",
                        device.model.name
                    ));
                    break;
                }
                if matches!(e, TransferError::Stall) {
                    let _ = messages.clear_halt().wait();
                }
                thread::sleep(Duration::from_millis(10));
                continue;
            }
        }

        if device.clock_restarted.swap(false, Ordering::AcqRel) {
            sync.reset();
        }

        // Learn what the whole transfer says about the adapter's clock
        // before stamping anything in it.
        events.clear();
        let data = &completion.buffer[..completion.actual_len];
        let result = proto::decode(data, |event| events.push(event));
        for event in &events {
            if let Some(timestamp_us) = event.frame_clock_us() {
                sync.observe(timestamp_us.saturating_mul(1000), arrived);
            }
        }
        // How long after the adapter stamped its latest message the
        // transfer got here, by the clock estimate.
        if let Some(latest) = events.iter().filter_map(Event::frame_clock_us).max() {
            let late = arrived.saturating_sub(sync.stamp(latest.saturating_mul(1000)));
            crate::timing::late(late, || {
                format!(
                    "{}: its messages reached the daemon late by",
                    device.model.name
                )
            });
        }
        for event in events.drain(..) {
            match event {
                Event::Frame {
                    channel,
                    frame,
                    timestamp_us,
                } => {
                    let ts = sync.stamp(timestamp_us.saturating_mul(1000));
                    if let Some(handle) = handles.get(usize::from(channel)) {
                        handle.receive(&frame, Some(ts));
                    }
                }
                Event::Sent {
                    channel,
                    frame,
                    token,
                    timestamp_us,
                } => {
                    let ts = sync.stamp(timestamp_us.saturating_mul(1000));
                    let index = usize::from(channel);
                    if let Some(transmitter) = device.transmitters.get(index) {
                        transmitter.confirmed();
                    }
                    if let Some(handle) = handles.get(index) {
                        handle.transmitted(token, &frame, Some(ts));
                    }
                }
                Event::Calibration { .. } => {}
                Event::ErrorCounters { channel, tx, rx } => {
                    if let Some(ch) = channels.get_mut(usize::from(channel)) {
                        ch.tx_errors = tx;
                        ch.rx_errors = rx;
                    }
                }
                Event::Status {
                    channel,
                    state,
                    timestamp_us,
                } => {
                    let ts = sync.stamp(timestamp_us.saturating_mul(1000));
                    let index = usize::from(channel);
                    if let (Some(ch), Some(handle)) = (channels.get_mut(index), handles.get(index))
                    {
                        if ch.state != state {
                            ch.state = state;
                            if state == BusState::Active {
                                ch.tx_errors = 0;
                                ch.rx_errors = 0;
                            }
                            let frame = proto::state_error_frame(state, ch.tx_errors, ch.rx_errors);
                            handle.receive(&frame, Some(ts));
                        }
                    }
                }
                Event::Overrun {
                    channel,
                    timestamp_us,
                } => {
                    let ts = sync.stamp(timestamp_us.saturating_mul(1000));
                    if let Some(handle) = handles.get(usize::from(channel)) {
                        handle.receive(&proto::overrun_error_frame(), Some(ts));
                    }
                }
            }
        }
        if result.is_err() {
            log(format!(
                "{}: malformed message from the adapter ignored",
                device.model.name
            ));
        }
    }

    device.gone.store(true, Ordering::Release);
    for handle in &handles {
        log(format!("{} removed ({})", handle.name(), device.model.name));
        handle.remove();
    }
}
