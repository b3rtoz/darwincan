// SPDX-License-Identifier: GPL-2.0-only
//
// Driver for the Kvaser adapters that run the "mhydra" firmware. The
// device handling sequence follows the Linux kernel driver
// (drivers/net/can/usb/kvaser_usb/kvaser_usb_core.c and
// kvaser_usb_hydra.c),
// Copyright (C) 2002-2018 KVASER AB, Sweden,
// Copyright (C) 2010 Matthias Fuchs <matthias.fuchs@esd.eu>, esd gmbh,
// Copyright (C) 2012 Olivier Sobrie <olivier@sobrie.be>,
// Copyright (C) 2015 Valeo S.A.

//! Kvaser's "hydra" adapters: USBcan Light 4xHS, USBcan Pro 2xHS v2 and
//! 5xHS, Leaf Pro HS v2, Memorator Pro, U100, Hybrid and their kin.
//!
//! Commands and frames for every channel share one pair of bulk
//! endpoints. Requests that are answered are answered on the same
//! endpoint the frames arrive on, so the receive loop is running before
//! the first question is asked, and hands the answers to whoever waits.

pub mod proto;

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use darwincan_core::{BusConfig, Frame};
use nusb::transfer::{Buffer, Bulk, In, Out, TransferError};
use nusb::{DeviceInfo, Endpoint, MaybeFuture};

use self::proto::{BusState, Busparams, Clock, Event, Header, Splitter, CMD_LEN};
use super::timesync::TimeSync;
use super::{log, transmit_failure, TRANSMIT_TIMEOUT, TRANSMIT_WINDOW};
use crate::bittiming::{self, TimingLimits};
use crate::clock::now_ns;
use crate::{Backend, Daemon, InterfaceHandle};

const VENDOR_ID: u16 = 0x0BFD;

/// The adapters Linux drives with this protocol.
const PRODUCT_IDS: [u16; 22] = [
    0x0102, // BlackBird v2
    0x0104, // Memorator Pro 5xHS
    0x0105, // USBcan Pro 5xHS
    0x0106, // USBcan Light 4xHS
    0x0107, // Leaf Pro HS v2
    0x0108, // USBcan Pro 2xHS v2
    0x0109, // Memorator 2xHS v2
    0x010A, // Memorator Pro 2xHS v2
    0x010B, // Hybrid 2xCAN/LIN
    0x010C, // ATI USBcan Pro 2xHS v2
    0x010D, // ATI Memorator Pro 2xHS v2
    0x010E, // Hybrid Pro 2xCAN/LIN
    0x0111, // U100
    0x0112, // U100P
    0x0113, // U100S
    0x0114, // USBcan Pro 4xHS
    0x0115, // Hybrid CAN/LIN
    0x0116, // Hybrid Pro CAN/LIN
    0x0117, // Leaf v3
    0x0119, // VINING 800
    0x011A, // USBcan Pro 5xCAN
    0x011B, // Mini PCIe 1xCAN
];

const BULK_IN: u8 = 0x82;
const BULK_OUT: u8 = 0x02;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
const RX_TRANSFER_SIZE: usize = 3072;
/// Receive transfers kept waiting in the kernel, so that the adapter can
/// go on delivering while the daemon is not being run.
const RX_TRANSFERS: usize = 32;
/// How often the receive loop looks up from an idle endpoint.
const RX_WAKE: Duration = Duration::from_millis(200);
/// How often a channel that is not error-active is asked for its state:
/// the firmware does not report the way back to error-active by itself.
const STATE_POLL: Duration = Duration::from_millis(500);
/// Answers kept for a request nobody has made yet.
const REPLIES_KEPT: usize = 16;

/// The controllers this firmware runs on, each with its clock and the
/// limits of its bit timing.
struct Controller {
    clock: Clock,
    nominal: TimingLimits,
    /// The data phase's limits, on a controller that does CAN FD.
    data: Option<TimingLimits>,
}

const KCAN_LIMITS: TimingLimits = TimingLimits {
    clock_hz: 80_000_000,
    tseg1_min: 1,
    tseg1_max: 255,
    tseg2_min: 1,
    tseg2_max: 32,
    sjw_max: 16,
    brp_min: 1,
    brp_max: 8192,
};
const KCAN: Controller = Controller {
    clock: Clock { ticks_per_us: 80 },
    nominal: KCAN_LIMITS,
    data: Some(KCAN_LIMITS),
};
const FLEXCAN: Controller = Controller {
    clock: Clock { ticks_per_us: 1 },
    nominal: TimingLimits {
        clock_hz: 24_000_000,
        tseg1_min: 4,
        tseg1_max: 16,
        tseg2_min: 2,
        tseg2_max: 8,
        sjw_max: 4,
        brp_min: 1,
        brp_max: 256,
    },
    data: None,
};
const RT: Controller = Controller {
    clock: Clock { ticks_per_us: 24 },
    nominal: TimingLimits {
        clock_hz: 80_000_000,
        tseg1_min: 2,
        tseg1_max: 96,
        tseg2_min: 2,
        tseg2_max: 32,
        sjw_max: 32,
        brp_min: 1,
        brp_max: 1024,
    },
    data: Some(TimingLimits {
        clock_hz: 80_000_000,
        tseg1_min: 2,
        tseg1_max: 39,
        tseg2_min: 2,
        tseg2_max: 8,
        sjw_max: 8,
        brp_min: 1,
        brp_max: 1024,
    }),
};

pub fn matches(info: &DeviceInfo) -> bool {
    info.vendor_id() == VENDOR_ID && PRODUCT_IDS.contains(&info.product_id())
}

fn model(info: &DeviceInfo) -> String {
    // What the device calls itself, kept to plain text: it goes into the
    // log, the interface listing and the file of interface names.
    let name: String = info
        .product_string()
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || " -_./()".contains(*c))
        .take(48)
        .collect();
    match name.trim() {
        "" => format!("Kvaser adapter {:04x}", info.product_id()),
        name => name.to_string(),
    }
}

pub fn describe(info: &DeviceInfo) -> String {
    format!(
        "{} (USB bus {} address {})",
        model(info),
        info.bus_id(),
        info.device_address()
    )
}

fn usb_error(what: &str, e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{what}: {e}"))
}

fn unplugged() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "adapter unplugged")
}

/// A frame handed to the adapter and not yet reported sent: the daemon's
/// name for it, and the frame, which the report does not repeat.
type Slot = Option<(u32, Frame)>;

/// What the receive loop and a channel's callers share about a channel.
struct ChannelShared {
    /// The firmware entity that is this channel.
    he: u8,
    /// Frames with the adapter, by the transaction id they were sent
    /// with.
    slots: Mutex<Vec<Slot>>,
    room: Condvar,
    /// On the bus, as far as the daemon is concerned.
    open: AtomicBool,
    listen_only: AtomicBool,
    fd: AtomicBool,
    state: Mutex<ChannelState>,
}

#[derive(Clone, Copy)]
struct ChannelState {
    state: BusState,
    tx_errors: u8,
    rx_errors: u8,
}

const ERROR_ACTIVE: ChannelState = ChannelState {
    state: BusState::Active,
    tx_errors: 0,
    rx_errors: 0,
};

impl ChannelShared {
    fn new(he: u8, window: usize) -> ChannelShared {
        ChannelShared {
            he,
            slots: Mutex::new(vec![None; window]),
            room: Condvar::new(),
            open: AtomicBool::new(false),
            listen_only: AtomicBool::new(false),
            fd: AtomicBool::new(false),
            state: Mutex::new(ERROR_ACTIVE),
        }
    }

    /// Take a frame out of the window, if it is there.
    fn release(&self, transid: u16) -> Slot {
        let mut slots = self.slots.lock().unwrap();
        let taken = slots.get_mut(usize::from(transid))?.take();
        self.room.notify_one();
        taken
    }

    /// The channel left the bus: what was on its way is not going out.
    fn reset(&self) {
        let mut slots = self.slots.lock().unwrap();
        slots.iter_mut().for_each(|slot| *slot = None);
        self.room.notify_all();
    }
}

/// What the adapter said about itself when it was attached.
struct Info {
    controller: &'static Controller,
    /// The firmware takes frames as extended commands, which CAN FD
    /// needs.
    ext_cmd: bool,
    fd: bool,
    /// Channels that can be listen-only, one bit each.
    listen_channels: u32,
    channels: Vec<ChannelShared>,
}

/// An answer from the adapter, kept for the request it belongs to.
struct Reply {
    header: Header,
    cmd: [u8; CMD_LEN],
}

/// State shared by the channels of one adapter.
struct Device {
    model: String,
    /// Where commands and frames go, in the order they are handed over.
    out: Mutex<Endpoint<Bulk, Out>>,
    /// The bulk IN endpoint, until the receive loop takes it.
    messages: Mutex<Option<Endpoint<Bulk, In>>>,
    replies: Mutex<Vec<Reply>>,
    replied: Condvar,
    /// One request that waits for an answer at a time.
    asking: Mutex<()>,
    transid: AtomicU16,
    info: OnceLock<Info>,
    gone: AtomicBool,
}

impl Device {
    fn info(&self) -> &Info {
        self.info
            .get()
            .expect("channels exist only once the adapter has been asked about itself")
    }

    /// A transaction id for a command: 1 to 255, in turn.
    fn next_transid(&self) -> u16 {
        self.transid.fetch_add(1, Ordering::Relaxed) % 0xFF + 1
    }

    /// Hand a command to the adapter without waiting for it to be taken.
    ///
    /// An earlier command that turns out not to have reached the adapter
    /// is logged, and this one is handed over all the same: it may be the
    /// one that takes a channel off the bus.
    fn write(&self, command: &[u8]) -> io::Result<()> {
        if self.gone.load(Ordering::Acquire) {
            return Err(unplugged());
        }
        let mut out = self.out.lock().unwrap();
        while out.pending() > 0 {
            let Some(done) = out.wait_next_complete(Duration::ZERO) else {
                break;
            };
            match done.status {
                Ok(()) | Err(TransferError::Cancelled) => {}
                Err(e) => {
                    // A frame that never reached the adapter will not be
                    // reported sent: give its place in the window back.
                    self.lost(&done.buffer[..]);
                    log(format!("{}: {}", self.model, transmit_failure(e)));
                }
            }
        }
        out.submit(command.to_vec().into());
        Ok(())
    }

    /// A command did not reach the adapter. If it was a frame, it no
    /// longer counts as being on its way.
    fn lost(&self, command: &[u8]) {
        let Some(info) = self.info.get() else {
            return;
        };
        if command.len() < 4
            || !matches!(command[0], proto::CMD_TX_CAN_MESSAGE | proto::CMD_EXTENDED)
        {
            return;
        }
        let he = command[1] & 0x3F;
        let transid = u16::from_le_bytes([command[2], command[3]]) & 0x0FFF;
        if let Some(channel) = info.channels.iter().find(|c| c.he == he) {
            channel.release(transid);
        }
    }

    /// Send a command and wait for its answer: the command numbered
    /// `answer`, from the entity `from` or to the transaction `transid`
    /// if either is given.
    fn ask(
        &self,
        command: &[u8; CMD_LEN],
        answer: u8,
        from: Option<u8>,
        transid: Option<u16>,
    ) -> io::Result<[u8; CMD_LEN]> {
        let is_it = |r: &Reply| {
            r.header.cmd_no == answer
                && from.is_none_or(|he| r.header.src_he == he)
                && transid.is_none_or(|t| r.header.transid == t)
        };
        let _one_at_a_time = self.asking.lock().unwrap();
        // An answer already there is to an earlier question.
        self.replies.lock().unwrap().retain(|r| !is_it(r));
        self.write(command)?;
        let give_up = Instant::now() + COMMAND_TIMEOUT;
        let mut replies = self.replies.lock().unwrap();
        loop {
            if let Some(at) = replies.iter().position(is_it) {
                return Ok(replies.remove(at).cmd);
            }
            if self.gone.load(Ordering::Acquire) {
                return Err(unplugged());
            }
            let left = give_up.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "the adapter did not answer command {} (expected {answer})",
                        command[0]
                    ),
                ));
            }
            replies = self.replied.wait_timeout(replies, left).unwrap().0;
        }
    }

    /// A command to a channel that is answered by that channel.
    fn ask_channel(&self, he: u8, request: u8, answer: u8) -> io::Result<[u8; CMD_LEN]> {
        self.ask(
            &proto::simple(request, he, self.next_transid()),
            answer,
            Some(he),
            None,
        )
    }

    /// Keep an answer for whoever asked.
    fn answered(&self, header: Header, cmd: [u8; CMD_LEN]) {
        let mut replies = self.replies.lock().unwrap();
        if replies.len() >= REPLIES_KEPT {
            replies.remove(0);
        }
        replies.push(Reply { header, cmd });
        self.replied.notify_all();
    }
}

struct Channel {
    device: Arc<Device>,
    index: usize,
    description: String,
}

impl Channel {
    fn shared(&self) -> &ChannelShared {
        &self.device.info().channels[self.index]
    }

    /// Set the bit timing and see that the adapter took it.
    fn set_timing(&self, kind: u8, wanted: &Busparams) -> io::Result<()> {
        let device = &self.device;
        let he = self.shared().he;
        let set = if kind == proto::BUSPARAM_TYPE_CAN {
            proto::set_busparams(he, device.next_transid(), wanted)
        } else {
            proto::set_busparams_fd(he, device.next_transid(), wanted)
        };
        device.write(&set)?;
        let answer = device.ask(
            &proto::get_busparams_req(he, device.next_transid(), kind),
            proto::CMD_GET_BUSPARAMS_RESP,
            Some(he),
            None,
        )?;
        let has = proto::busparams_resp(&answer);
        if has != *wanted {
            return Err(io::Error::other(format!(
                "the adapter did not take the bit timing: asked for {} bit/s with segments \
                 of {} and {} quanta, it has {} bit/s with {} and {}",
                wanted.bitrate, wanted.tseg1, wanted.tseg2, has.bitrate, has.tseg1, has.tseg2
            )));
        }
        Ok(())
    }
}

impl Backend for Channel {
    fn kind(&self) -> &str {
        "kvaser_hydra"
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn start(&self, config: &BusConfig) -> io::Result<()> {
        let invalid = |e: String| io::Error::new(io::ErrorKind::InvalidInput, e);
        let device = &self.device;
        let info = device.info();
        let shared = self.shared();

        if config.listen_only && info.listen_channels & (1 << self.index) == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this adapter cannot be listen-only",
            ));
        }
        let nominal = bittiming::calculate(
            &info.controller.nominal,
            config.bitrate,
            config.sample_point.map(u32::from),
        )
        .map_err(invalid)?;
        let data = match (config.data_bitrate, &info.controller.data) {
            (None, _) => None,
            (Some(bitrate), Some(limits)) if info.fd && info.ext_cmd => Some(
                bittiming::calculate(limits, bitrate, config.data_sample_point.map(u32::from))
                    .map_err(invalid)?,
            ),
            (Some(_), _) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "this adapter does not do CAN FD",
                ))
            }
        };
        if let Some(data) = &data {
            if data.bitrate < nominal.bitrate {
                return Err(invalid(
                    "the data bitrate must not be below the nominal bitrate".into(),
                ));
            }
        }

        self.set_timing(proto::BUSPARAM_TYPE_CAN, &Busparams::from_timing(&nominal))?;
        if let Some(data) = &data {
            self.set_timing(proto::BUSPARAM_TYPE_CANFD, &Busparams::from_timing(data))?;
        }
        device.write(&proto::set_driver_mode(
            shared.he,
            device.next_transid(),
            config.listen_only,
        ))?;

        shared.reset();
        *shared.state.lock().unwrap() = ERROR_ACTIVE;
        shared
            .listen_only
            .store(config.listen_only, Ordering::Release);
        shared.fd.store(data.is_some(), Ordering::Release);
        let started = device.ask_channel(
            shared.he,
            proto::CMD_START_CHIP_REQ,
            proto::CMD_START_CHIP_RESP,
        );
        if let Err(e) = started {
            // The request may have been carried out and only its answer
            // lost: an interface reported as down must not be on the bus.
            let _ = device.write(&proto::simple(
                proto::CMD_STOP_CHIP_REQ,
                shared.he,
                device.next_transid(),
            ));
            return Err(e);
        }
        shared.open.store(true, Ordering::Release);
        Ok(())
    }

    fn stop(&self) -> io::Result<()> {
        let device = &self.device;
        let shared = self.shared();
        shared.open.store(false, Ordering::Release);
        // Drop what was waiting to be sent, which would otherwise go out
        // when the channel is next started; then off the bus.
        let flushed = device.ask_channel(
            shared.he,
            proto::CMD_FLUSH_QUEUE,
            proto::CMD_FLUSH_QUEUE_RESP,
        );
        let stopped = device.ask_channel(
            shared.he,
            proto::CMD_STOP_CHIP_REQ,
            proto::CMD_STOP_CHIP_RESP,
        );
        shared.reset();
        stopped.and(flushed).map(|_| ())
    }

    fn confirms_transmission(&self) -> bool {
        true
    }

    fn transmit(&self, frame: &Frame, token: u32) -> io::Result<()> {
        let device = &self.device;
        let info = device.info();
        let shared = self.shared();
        if device.gone.load(Ordering::Acquire) {
            return Err(unplugged());
        }
        if shared.listen_only.load(Ordering::Acquire) {
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
        if frame.is_fd() && !shared.fd.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "interface is not configured for CAN FD (no data bitrate set)",
            ));
        }

        // A place in the window; its number comes back in the adapter's
        // report that the frame was sent.
        let give_up = Instant::now() + TRANSMIT_TIMEOUT;
        let mut slots = shared.slots.lock().unwrap();
        let place = loop {
            if let Some(place) = slots.iter().position(Option::is_none) {
                break place;
            }
            let left = give_up.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "adapter transmit buffer is full (is anything acknowledging on the bus, or is other traffic keeping it busy?)",
                ));
            }
            slots = shared.room.wait_timeout(slots, left).unwrap().0;
        };
        slots[place] = Some((token, *frame));
        drop(slots);

        let transid = place as u16;
        let sent = if info.ext_cmd {
            device.write(&proto::encode_tx_ext(shared.he, transid, frame))
        } else {
            device.write(&proto::encode_tx_std(shared.he, transid, frame))
        };
        if sent.is_err() {
            shared.release(transid);
        }
        sent
    }
}

/// Ask the adapter what it is, as the reference driver does when it
/// finds one, and leave its channels off the bus.
fn identify(device: &Device) -> io::Result<(Info, proto::CardInfo, proto::SoftwareDetails)> {
    // The addresses of the firmware's entities: one for each channel
    // there could be, and the one that knows about the device.
    let mut channel_he = [proto::HE_ILLEGAL; proto::MAX_CHANNELS];
    for (channel, he) in channel_he.iter_mut().enumerate() {
        let transid = proto::TRANSID_CANHE | channel as u16;
        let answer = device.ask(
            &proto::map_channel_req(transid, channel as u8, "CAN"),
            proto::CMD_MAP_CHANNEL_RESP,
            None,
            Some(transid),
        )?;
        *he = proto::map_channel_resp(&answer);
    }
    let answer = device.ask(
        &proto::map_channel_req(proto::TRANSID_SYSDBG, 0, "SYSDBG"),
        proto::CMD_MAP_CHANNEL_RESP,
        None,
        Some(proto::TRANSID_SYSDBG),
    )?;
    let sysdbg_he = proto::map_channel_resp(&answer);

    let answer = device.ask(
        &proto::simple(
            proto::CMD_GET_SOFTWARE_INFO_REQ,
            proto::HE_ILLEGAL,
            device.next_transid(),
        ),
        proto::CMD_GET_SOFTWARE_INFO_RESP,
        None,
        None,
    )?;
    let window = usize::from(proto::max_outstanding_tx(&answer)).clamp(1, TRANSMIT_WINDOW);

    let answer = device.ask(
        &proto::software_details_req(device.next_transid()),
        proto::CMD_GET_SOFTWARE_DETAILS_RESP,
        None,
        None,
    )?;
    let software = proto::software_details(&answer);
    if software.flags & proto::SW_FLAG_FW_BAD != 0 {
        return Err(io::Error::other(
            "the adapter says its firmware is bad and refuses to run",
        ));
    }
    let controller = if software.flags & proto::SW_FLAG_FREQ_80M != 0 {
        &KCAN
    } else if software.flags & proto::SW_FLAG_CAN_FREQ_80M != 0 {
        &RT
    } else {
        &FLEXCAN
    };

    let answer = device.ask(
        &proto::simple(
            proto::CMD_GET_CARD_INFO_REQ,
            proto::HE_ILLEGAL,
            device.next_transid(),
        ),
        proto::CMD_GET_CARD_INFO_RESP,
        None,
        None,
    )?;
    let card = proto::card_info(&answer);
    let count = usize::from(card.channels);
    if count == 0 || count > proto::MAX_CHANNELS {
        return Err(io::Error::other(format!(
            "the adapter says it has {count} channels"
        )));
    }

    let mut listen_channels = 0;
    if software.flags & proto::SW_FLAG_EXT_CAP != 0 {
        let answer = device.ask(
            &proto::capability_req(sysdbg_he, device.next_transid(), proto::CAP_LISTEN_MODE),
            proto::CMD_GET_CAPABILITIES_RESP,
            None,
            None,
        )?;
        if let Some((proto::CAP_LISTEN_MODE, channels)) = proto::capability_resp(&answer) {
            listen_channels = channels;
        }
    }

    let info = Info {
        controller,
        ext_cmd: software.flags & proto::SW_FLAG_EXT_CMD != 0,
        fd: software.flags & proto::SW_FLAG_CANFD != 0,
        listen_channels,
        channels: channel_he[..count]
            .iter()
            .map(|&he| ChannelShared::new(he, window))
            .collect(),
    };
    // Whatever state a previous user left it in (a daemon that was killed
    // leaves its channels on the bus, perhaps with frames waiting), start
    // off the bus with nothing to send.
    for channel in &info.channels {
        device.ask_channel(
            channel.he,
            proto::CMD_FLUSH_QUEUE,
            proto::CMD_FLUSH_QUEUE_RESP,
        )?;
        device.ask_channel(
            channel.he,
            proto::CMD_STOP_CHIP_REQ,
            proto::CMD_STOP_CHIP_RESP,
        )?;
    }
    Ok((info, card, software))
}

/// How often, and how long apart, taking the adapter over is tried again
/// while another program has it: five seconds in all.
const BUSY_ATTEMPTS: u32 = 20;
const BUSY_RETRY: Duration = Duration::from_millis(250);

/// Do `step`, again for as long as the system says another program has the
/// device. A web browser opens every USB device for a moment as it is
/// plugged in, to read what it is; selecting a configuration needs the
/// device to ourselves, and one attempt at that moment is refused.
fn while_busy<T>(mut step: impl FnMut() -> Result<T, nusb::Error>) -> Result<T, nusb::Error> {
    let mut attempt = 1;
    loop {
        match step() {
            Err(e) if e.kind() == nusb::ErrorKind::Busy && attempt < BUSY_ATTEMPTS => {
                attempt += 1;
                thread::sleep(BUSY_RETRY);
            }
            done => {
                if attempt > 1 && done.is_ok() {
                    log(format!(
                        "another program had the adapter; it was free after {} ms",
                        (attempt - 1) * BUSY_RETRY.as_millis() as u32
                    ));
                }
                return done;
            }
        }
    }
}

/// Take over an adapter and register its channels as interfaces. Returns
/// the interface names.
pub fn attach(daemon: &Daemon, info: &DeviceInfo) -> io::Result<Vec<String>> {
    let usb = info
        .open()
        .wait()
        .map_err(|e| usb_error("opening the device", e))?;
    // macOS leaves a device of a vendor's own class without a configuration
    // (the PEAK adapters, whose class is per interface, get one), and a
    // device without one has no interfaces to claim.
    if usb.active_configuration().is_err() {
        let configuration = usb
            .configurations()
            .next()
            .map_or(1, |c| c.configuration_value());
        while_busy(|| usb.set_configuration(configuration).wait()).map_err(|e| {
            usb_error(
                "selecting the USB configuration (is another application using the adapter?)",
                e,
            )
        })?;
    }
    let interface = while_busy(|| usb.claim_interface(0).wait()).map_err(|e| {
        usb_error(
            "claiming the USB interface (is another application using the adapter?)",
            e,
        )
    })?;
    let endpoint = |e| usb_error("opening an endpoint", e);
    let model = model(info);
    let device = Arc::new(Device {
        model: model.clone(),
        out: Mutex::new(interface.endpoint(BULK_OUT).map_err(endpoint)?),
        messages: Mutex::new(Some(interface.endpoint(BULK_IN).map_err(endpoint)?)),
        replies: Mutex::new(Vec::new()),
        replied: Condvar::new(),
        asking: Mutex::new(()),
        transid: AtomicU16::new(0),
        info: OnceLock::new(),
        gone: AtomicBool::new(false),
    });

    // The answers to what is asked next arrive where the frames do.
    let (give_handles, handles) = mpsc::channel();
    let reader = device.clone();
    thread::Builder::new()
        .name("darwincand-kvaser-rx".into())
        .spawn(move || receive_loop(reader, handles))?;

    let (card, software) = match identify(&device) {
        Ok((found, card, software)) => {
            let _ = device.info.set(found);
            (card, software)
        }
        Err(e) => {
            // Ends the receive loop.
            device.gone.store(true, Ordering::Release);
            return Err(e);
        }
    };
    let found = device.info();
    let (major, minor, build) = software.version;
    log(format!(
        "{model}: serial {}, EAN {:08x}{:08x}, firmware {major}.{minor}.{build} (flags {:#x}), \
         {} channels, {} MHz controller{}{}, {}",
        card.serial,
        card.ean[1],
        card.ean[0],
        software.flags,
        found.channels.len(),
        found.controller.nominal.clock_hz / 1_000_000,
        if found.ext_cmd {
            ", extended commands"
        } else {
            ""
        },
        if found.fd { ", CAN FD" } else { "" },
        if software.flags & proto::SW_FLAG_EXT_CAP == 0 {
            "does not say what it can do, so no listen-only".to_string()
        } else if found.listen_channels == 0 {
            "cannot be listen-only".to_string()
        } else {
            format!("listen-only on channels {:#b}", found.listen_channels)
        },
    ));

    let numbered = found.channels.len() > 1;
    let mut interfaces = Vec::new();
    for index in 0..found.channels.len() {
        let number = index + 1;
        let description = if numbered {
            format!(
                "{model} channel {number} (serial {}, firmware {major}.{minor}.{build})",
                card.serial
            )
        } else {
            format!(
                "{model} (serial {}, firmware {major}.{minor}.{build})",
                card.serial
            )
        };
        let identity = super::identity(&model, card.serial, info, numbered.then_some(number));
        interfaces.push(daemon.add_can_interface(
            &identity,
            Box::new(Channel {
                device: device.clone(),
                index,
                description,
            }),
        ));
    }
    let names = interfaces.iter().map(|h| h.name().to_string()).collect();
    let _ = give_handles.send(interfaces);
    Ok(names)
}

/// Reads the adapter's commands until it is unplugged.
fn receive_loop(device: Arc<Device>, interfaces: mpsc::Receiver<Vec<InterfaceHandle>>) {
    crate::priority::make_current_thread_realtime();
    let Some(mut messages) = device.messages.lock().unwrap().take() else {
        return;
    };
    let mut handles: Vec<InterfaceHandle> = Vec::new();
    let mut splitter = Splitter::new();
    let mut sync = TimeSync::new();
    let mut events: Vec<Event> = Vec::new();
    let mut consecutive_errors = 0;
    let mut polled = Instant::now();

    'usb: loop {
        while messages.pending() < RX_TRANSFERS {
            messages.submit(Buffer::new(RX_TRANSFER_SIZE));
        }
        let completion = messages.wait_next_complete(RX_WAKE);
        if device.gone.load(Ordering::Acquire) {
            // The adapter could not be identified; nobody is using it.
            messages.cancel_all();
            return;
        }
        if handles.is_empty() {
            if let Ok(given) = interfaces.try_recv() {
                handles = given;
            }
        }
        if polled.elapsed() >= STATE_POLL {
            polled = Instant::now();
            poll_states(&device);
        }
        let Some(completion) = completion else {
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
                    log(format!("{}: giving up after USB errors: {e}", device.model));
                    break 'usb;
                }
                if matches!(e, TransferError::Stall) {
                    let _ = messages.clear_halt().wait();
                }
                splitter.reset();
                thread::sleep(Duration::from_millis(10));
                continue;
            }
        }

        events.clear();
        let data = &completion.buffer[..completion.actual_len];
        let result = splitter.feed(data, |cmd| events.push(proto::decode(cmd)));
        if result.is_err() {
            log(format!(
                "{}: malformed message from the adapter ignored",
                device.model
            ));
        }
        // Answers first, and they need nothing known about the adapter.
        events.retain(|event| match event {
            Event::Reply { header, cmd } => {
                device.answered(*header, *cmd);
                false
            }
            _ => true,
        });
        // Frames from an adapter still being identified are a previous
        // user's; it is taken off the bus before anyone listens.
        let Some(info) = device.info.get() else {
            continue;
        };
        let clock = info.controller.clock;
        let channel_of = |he: u8| info.channels.iter().position(|c| c.he == he);

        // Learn what the whole transfer says about the adapter's clock
        // before stamping anything in it. (The USBcan Light 4xHS counts
        // 80 MHz ticks but reports them in steps of 8192, 102.4 us, each
        // time rounded up to the end of its step: never early, which is
        // what timing STmin from these stamps relies on.)
        let ticks_of = |event: &Event| match event {
            Event::Frame { ticks, .. } | Event::Sent { ticks, .. } => Some(*ticks),
            _ => None,
        };
        for ticks in events.iter().filter_map(ticks_of) {
            sync.observe(clock.ns(ticks), arrived);
        }
        if let Some(latest) = events.iter().filter_map(ticks_of).max() {
            let late = arrived.saturating_sub(sync.stamp(clock.ns(latest)));
            crate::timing::late(late, || {
                format!("{}: its messages reached the daemon late by", device.model)
            });
        }

        for event in events.drain(..) {
            match event {
                Event::Frame {
                    src_he,
                    frame,
                    ticks,
                    overrun,
                } => {
                    let Some(index) = channel_of(src_he) else {
                        continue;
                    };
                    let ts = sync.stamp(clock.ns(ticks));
                    if let Some(handle) = handles.get(index) {
                        if overrun {
                            handle.receive(&proto::overrun_error_frame(), Some(ts));
                        }
                        handle.receive(&frame, Some(ts));
                    }
                }
                Event::Sent {
                    src_he,
                    transid,
                    ticks,
                    failed,
                } => {
                    let Some(index) = channel_of(src_he) else {
                        continue;
                    };
                    let Some((token, frame)) = info.channels[index].release(transid) else {
                        continue;
                    };
                    if failed {
                        continue;
                    }
                    let ts = sync.stamp(clock.ns(ticks));
                    if let Some(handle) = handles.get(index) {
                        handle.transmitted(token, &frame, Some(ts));
                    }
                }
                Event::BusError { src_he, state, .. } => {
                    if let Some(index) = channel_of(src_he) {
                        let channel = &info.channels[index];
                        state_changed(&device, channel, handles.get(index), state, true);
                    }
                }
                Event::State { src_he, state } => {
                    if let Some(index) = channel_of(src_he) {
                        let channel = &info.channels[index];
                        state_changed(&device, channel, handles.get(index), state, false);
                    }
                }
                Event::Refused { code, cmd_no } => log(format!(
                    "{}: the adapter refused command {cmd_no} (error {code})",
                    device.model
                )),
                Event::Unknown { cmd_no, ext } => log(format!(
                    "{}: unknown message {cmd_no}{} from the adapter ignored",
                    device.model,
                    ext.map(|e| format!("/{e}")).unwrap_or_default()
                )),
                Event::Reply { .. } | Event::Ignored => {}
            }
        }
    }

    device.gone.store(true, Ordering::Release);
    device.replied.notify_all();
    if let Some(info) = device.info.get() {
        info.channels.iter().for_each(ChannelShared::reset);
    }
    if handles.is_empty() {
        if let Ok(given) = interfaces.try_recv() {
            handles = given;
        }
    }
    for handle in &handles {
        log(format!("{} removed ({})", handle.name(), device.model));
        handle.remove();
    }
}

/// Ask the channels that are on the bus and not error-active how they
/// are. The answer is a state event like any other.
fn poll_states(device: &Device) {
    let Some(info) = device.info.get() else {
        return;
    };
    for channel in &info.channels {
        let state = channel.state.lock().unwrap().state;
        if channel.open.load(Ordering::Acquire)
            && matches!(state, BusState::Warning | BusState::Passive)
        {
            let _ = device.write(&proto::simple(
                proto::CMD_GET_CHIP_STATE_REQ,
                channel.he,
                device.next_transid(),
            ));
        }
    }
}

/// The adapter reported a channel's state: tell the interface's clients
/// if it is news.
fn state_changed(
    device: &Device,
    channel: &ChannelShared,
    handle: Option<&InterfaceHandle>,
    reported: proto::ChipState,
    with_bus_error: bool,
) {
    // The firmware reports a channel that is off the bus as bus-off.
    if !channel.open.load(Ordering::Acquire) {
        return;
    }
    let mut now = channel.state.lock().unwrap();
    let Some(state) = proto::next_state(now.state, reported.bus_state(), with_bus_error) else {
        return;
    };
    now.tx_errors = reported.tx_errors;
    now.rx_errors = reported.rx_errors;
    now.state = state;
    let frame = proto::state_error_frame(state, reported.tx_errors, reported.rx_errors);
    drop(now);
    if state == BusState::BusOff {
        // The controller would come back by itself after a while and send
        // what it still holds. Keep it off the bus instead, as the
        // reference driver does when no restart is asked for.
        let _ = device.write(&proto::simple(
            proto::CMD_STOP_CHIP_REQ,
            channel.he,
            device.next_transid(),
        ));
        channel.reset();
    }
    if let Some(handle) = handle {
        handle.receive(&frame, None);
    }
}
