// SPDX-License-Identifier: GPL-2.0-only
//
// Wire format of Kvaser's "hydra" protocol, spoken by the firmware Kvaser
// labels mhydra. Derived from the Linux kernel driver
// (drivers/net/can/usb/kvaser_usb/kvaser_usb_hydra.c and kvaser_usb.h),
// Copyright (C) 2018 KVASER AB, Sweden,
// Copyright (C) 2010 Matthias Fuchs <matthias.fuchs@esd.eu>, esd gmbh.

//! Encoding and decoding of hydra commands. Pure functions, no I/O.
//!
//! Everything the adapter is told and everything it says is a command: 32
//! bytes, or for the "extended" ones that carry CAN FD frames a length of
//! their own. A command is addressed to one of the firmware's threads
//! ("hydra entities"), each with a 6-bit address that has to be asked for:
//! one per CAN channel, and one that answers questions about the device.

use darwincan_core::{
    err, Frame, CANFD_BRS, CANFD_ESI, CANFD_FDF, CAN_EFF_FLAG, CAN_EFF_MASK, CAN_RTR_FLAG,
    CAN_SFF_MASK,
};

use crate::bittiming::BitTiming;

/// The length of every command but the extended ones.
pub const CMD_LEN: usize = 32;
/// The longest extended command.
pub const MAX_CMD_LEN: usize = 128;

pub const CMD_SET_BUSPARAMS_REQ: u8 = 16;
pub const CMD_GET_BUSPARAMS_REQ: u8 = 17;
pub const CMD_GET_BUSPARAMS_RESP: u8 = 18;
pub const CMD_GET_CHIP_STATE_REQ: u8 = 19;
pub const CMD_CHIP_STATE_EVENT: u8 = 20;
pub const CMD_SET_DRIVERMODE_REQ: u8 = 21;
pub const CMD_START_CHIP_REQ: u8 = 26;
pub const CMD_START_CHIP_RESP: u8 = 27;
pub const CMD_STOP_CHIP_REQ: u8 = 28;
pub const CMD_STOP_CHIP_RESP: u8 = 29;
pub const CMD_TX_CAN_MESSAGE: u8 = 33;
pub const CMD_GET_CARD_INFO_REQ: u8 = 34;
pub const CMD_GET_CARD_INFO_RESP: u8 = 35;
pub const CMD_GET_SOFTWARE_INFO_REQ: u8 = 38;
pub const CMD_GET_SOFTWARE_INFO_RESP: u8 = 39;
pub const CMD_ERROR_EVENT: u8 = 45;
pub const CMD_FLUSH_QUEUE: u8 = 48;
pub const CMD_TX_ACKNOWLEDGE: u8 = 50;
pub const CMD_FLUSH_QUEUE_RESP: u8 = 66;
pub const CMD_SET_BUSPARAMS_FD_REQ: u8 = 69;
pub const CMD_SET_BUSPARAMS_FD_RESP: u8 = 70;
pub const CMD_SET_BUSPARAMS_RESP: u8 = 85;
pub const CMD_GET_CAPABILITIES_REQ: u8 = 95;
pub const CMD_GET_CAPABILITIES_RESP: u8 = 96;
pub const CMD_RX_MESSAGE: u8 = 106;
pub const CMD_MAP_CHANNEL_REQ: u8 = 200;
pub const CMD_MAP_CHANNEL_RESP: u8 = 201;
pub const CMD_GET_SOFTWARE_DETAILS_REQ: u8 = 202;
pub const CMD_GET_SOFTWARE_DETAILS_RESP: u8 = 203;
pub const CMD_EXTENDED: u8 = 255;

// Extended command numbers.
const CMD_TX_CAN_MESSAGE_FD: u8 = 224;
const CMD_TX_ACKNOWLEDGE_FD: u8 = 225;
const CMD_RX_MESSAGE_FD: u8 = 226;

/// The entity that hands out the addresses of the others.
pub const HE_ROUTER: u8 = 0x00;
/// No entity: questions about the device as a whole go here.
pub const HE_ILLEGAL: u8 = 0x3E;

/// The transaction ids that ask for a CAN channel's address (plus the
/// channel number) and for the address of the device's own entity. The
/// answer is told apart by them.
pub const TRANSID_CANHE: u16 = 0x40;
pub const TRANSID_SYSDBG: u16 = 0x61;
/// Channels a device is asked about, whether or not it has them.
pub const MAX_CHANNELS: usize = 5;

const TRANSID_MASK: u16 = 0x0FFF;

// Software detail flags.
pub const SW_FLAG_FW_BETA: u32 = 1 << 2;
pub const SW_FLAG_FW_BAD: u32 = 1 << 4;
pub const SW_FLAG_FREQ_80M: u32 = 1 << 5;
pub const SW_FLAG_EXT_CMD: u32 = 1 << 9;
pub const SW_FLAG_CANFD: u32 = 1 << 10;
pub const SW_FLAG_EXT_CAP: u32 = 1 << 12;
pub const SW_FLAG_CAN_FREQ_80M: u32 = 1 << 13;

// Capabilities that can be asked about.
pub const CAP_LISTEN_MODE: u16 = 0x02;
pub const CAP_ERR_REPORT: u16 = 0x05;
pub const CAP_STAT_OK: u16 = 0x00;

// Error event codes.
pub const ERROR_EVENT_CAN: u8 = 0x01;
pub const ERROR_EVENT_PARAM: u8 = 0x09;

// Chip state flags.
const BUS_ERR_PASS: u8 = 1 << 5;
const BUS_BUS_OFF: u8 = 1 << 6;

const BUS_MODE_CANFD_ISO: u8 = 0x01;
pub const BUSPARAM_TYPE_CAN: u8 = 0x00;
pub const BUSPARAM_TYPE_CANFD: u8 = 0x01;

const CTRLMODE_NORMAL: u8 = 0x01;
const CTRLMODE_LISTEN: u8 = 0x02;

/// In the identifier of the 32-byte frame commands.
const EXTENDED_FRAME_ID: u32 = 1 << 31;

// Frame flags.
const CF_FLAG_ERROR_FRAME: u32 = 1 << 0;
const CF_FLAG_OVERRUN: u32 = 1 << 1;
const CF_FLAG_REMOTE_FRAME: u32 = 1 << 4;
const CF_FLAG_EXTENDED_ID: u32 = 1 << 5;
// In extended commands only.
const CF_FLAG_OSM_NACK: u32 = 1 << 12;
const CF_FLAG_ABL: u32 = 1 << 13;
const CF_FLAG_FDF: u32 = 1 << 16;
const CF_FLAG_BRS: u32 = 1 << 17;
const CF_FLAG_ESI: u32 = 1 << 18;

// The controller's own frame header, carried in extended commands.
const KCAN_DATA_DLC_SHIFT: u32 = 8;
const KCAN_DATA_DLC_MASK: u32 = 0xF << KCAN_DATA_DLC_SHIFT;
const KCAN_DATA_BRS: u32 = 1 << 14;
const KCAN_DATA_FDF: u32 = 1 << 15;
/// Ask for the frame to be reported when it has been sent.
const KCAN_DATA_AREQ: u32 = 1 << 31;
const KCAN_DATA_SRR: u32 = 1 << 31;
const KCAN_DATA_RTR: u32 = 1 << 29;
const KCAN_DATA_IDE: u32 = 1 << 30;

/// Where an extended command's own fields begin: after the header, its
/// length and its command number.
const EXT_BODY: usize = 8;
/// Where the data bytes of an extended frame command begin.
const EXT_PAYLOAD: usize = 32;

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// A 32-byte command with its header filled in and the rest zero.
fn command(cmd_no: u8, dest_he: u8, transid: u16) -> [u8; CMD_LEN] {
    let mut cmd = [0u8; CMD_LEN];
    cmd[0] = cmd_no;
    // The upper two bits are part of the sender's address, which is
    // nobody's when the host sends.
    cmd[1] = dest_he & 0x3F;
    cmd[2..4].copy_from_slice(&(transid & TRANSID_MASK).to_le_bytes());
    cmd
}

/// A command that is nothing but its number.
pub fn simple(cmd_no: u8, dest_he: u8, transid: u16) -> [u8; CMD_LEN] {
    command(cmd_no, dest_he, transid)
}

/// Ask the router for the address of the entity called `name`
/// (`"CAN"`, `"SYSDBG"`) for `channel`.
pub fn map_channel_req(transid: u16, channel: u8, name: &str) -> [u8; CMD_LEN] {
    let mut cmd = command(CMD_MAP_CHANNEL_REQ, HE_ROUTER, transid);
    cmd[4..4 + name.len()].copy_from_slice(name.as_bytes());
    cmd[20] = channel;
    cmd
}

/// Ask for the firmware's version and abilities, and say that this host
/// understands extended commands.
pub fn software_details_req(transid: u16) -> [u8; CMD_LEN] {
    let mut cmd = command(CMD_GET_SOFTWARE_DETAILS_REQ, HE_ILLEGAL, transid);
    cmd[4] = 1;
    cmd
}

pub fn capability_req(sysdbg_he: u8, transid: u16, capability: u16) -> [u8; CMD_LEN] {
    let mut cmd = command(CMD_GET_CAPABILITIES_REQ, sysdbg_he, transid);
    cmd[4..6].copy_from_slice(&capability.to_le_bytes());
    cmd
}

/// Bit timing as the adapter takes it: the bitrate and the lengths of the
/// two segments in time quanta. It works out the prescaler itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busparams {
    pub bitrate: u32,
    pub tseg1: u8,
    pub tseg2: u8,
    pub sjw: u8,
    pub nsamples: u8,
}

impl Busparams {
    pub fn from_timing(timing: &BitTiming) -> Busparams {
        Busparams {
            bitrate: timing.bitrate,
            tseg1: timing.tseg1 as u8,
            tseg2: timing.tseg2 as u8,
            sjw: timing.sjw as u8,
            nsamples: 1,
        }
    }

    fn write(&self, to: &mut [u8]) {
        to[0..4].copy_from_slice(&self.bitrate.to_le_bytes());
        to[4] = self.tseg1;
        to[5] = self.tseg2;
        to[6] = self.sjw;
        to[7] = self.nsamples;
    }

    fn read(from: &[u8]) -> Busparams {
        Busparams {
            bitrate: le32(from, 0),
            tseg1: from[4],
            tseg2: from[5],
            sjw: from[6],
            nsamples: from[7],
        }
    }
}

pub fn set_busparams(he: u8, transid: u16, nominal: &Busparams) -> [u8; CMD_LEN] {
    let mut cmd = command(CMD_SET_BUSPARAMS_REQ, he, transid);
    nominal.write(&mut cmd[4..12]);
    cmd
}

/// The data phase's bit timing, for ISO CAN FD.
pub fn set_busparams_fd(he: u8, transid: u16, data: &Busparams) -> [u8; CMD_LEN] {
    let mut cmd = command(CMD_SET_BUSPARAMS_FD_REQ, he, transid);
    data.write(&mut cmd[16..24]);
    cmd[24] = BUS_MODE_CANFD_ISO;
    cmd
}

/// Ask what bit timing is in effect; `kind` is one of `BUSPARAM_TYPE_*`.
pub fn get_busparams_req(he: u8, transid: u16, kind: u8) -> [u8; CMD_LEN] {
    let mut cmd = command(CMD_GET_BUSPARAMS_REQ, he, transid);
    cmd[4] = kind;
    cmd
}

pub fn set_driver_mode(he: u8, transid: u16, listen_only: bool) -> [u8; CMD_LEN] {
    let mut cmd = command(CMD_SET_DRIVERMODE_REQ, he, transid);
    cmd[4] = if listen_only {
        CTRLMODE_LISTEN
    } else {
        CTRLMODE_NORMAL
    };
    cmd
}

/// The DLC of a CAN FD frame of `len` bytes.
fn fd_len_to_dlc(len: usize) -> u8 {
    match len {
        0..=8 => len as u8,
        12 => 9,
        16 => 10,
        20 => 11,
        24 => 12,
        32 => 13,
        48 => 14,
        _ => 15,
    }
}

fn fd_dlc_to_len(dlc: u8) -> usize {
    match dlc & 0xF {
        dlc @ 0..=8 => usize::from(dlc),
        9 => 12,
        10 => 16,
        11 => 20,
        12 => 24,
        13 => 32,
        14 => 48,
        _ => 64,
    }
}

/// A classic frame to send, for firmware without extended commands.
/// `transid` comes back in the report that it was sent.
pub fn encode_tx_std(he: u8, transid: u16, frame: &Frame) -> [u8; CMD_LEN] {
    let mut cmd = command(CMD_TX_CAN_MESSAGE, he, transid);
    let mut flags = 0;
    let id = if frame.is_extended() {
        flags |= CF_FLAG_EXTENDED_ID;
        (frame.can_id() & CAN_EFF_MASK) | EXTENDED_FRAME_ID
    } else {
        frame.can_id() & CAN_SFF_MASK
    };
    if frame.is_remote() {
        flags |= CF_FLAG_REMOTE_FRAME;
    }
    cmd[4..8].copy_from_slice(&id.to_le_bytes());
    cmd[8..8 + frame.data().len()].copy_from_slice(frame.data());
    cmd[16] = frame.len() as u8;
    cmd[17] = flags as u8;
    cmd
}

/// A classic or CAN FD frame to send, as an extended command.
pub fn encode_tx_ext(he: u8, transid: u16, frame: &Frame) -> Vec<u8> {
    let data = frame.data();
    let len = (EXT_PAYLOAD + data.len()).next_multiple_of(8);
    let mut cmd = vec![0u8; len];
    cmd[..4].copy_from_slice(&command(CMD_EXTENDED, he, transid)[..4]);
    cmd[4..6].copy_from_slice(&(len as u16).to_le_bytes());
    cmd[6] = CMD_TX_CAN_MESSAGE_FD;

    let dlc = if frame.is_fd() {
        fd_len_to_dlc(frame.len())
    } else {
        frame.len() as u8
    };
    let (id, mut flags, mut kcan_id) = if frame.is_extended() {
        let id = frame.can_id() & CAN_EFF_MASK;
        (id, CF_FLAG_EXTENDED_ID, id | KCAN_DATA_IDE | KCAN_DATA_SRR)
    } else {
        let id = frame.can_id() & CAN_SFF_MASK;
        (id, 0, id)
    };
    let mut kcan_header =
        ((u32::from(dlc) << KCAN_DATA_DLC_SHIFT) & KCAN_DATA_DLC_MASK) | KCAN_DATA_AREQ;
    if frame.is_fd() {
        kcan_header |= KCAN_DATA_FDF;
        if frame.is_brs() {
            kcan_header |= KCAN_DATA_BRS;
        }
    } else if frame.is_remote() {
        kcan_id |= KCAN_DATA_RTR;
        flags |= CF_FLAG_REMOTE_FRAME;
    }
    let body = &mut cmd[EXT_BODY..];
    body[0..4].copy_from_slice(&flags.to_le_bytes());
    body[4..8].copy_from_slice(&id.to_le_bytes());
    body[8..12].copy_from_slice(&kcan_id.to_le_bytes());
    body[12..16].copy_from_slice(&kcan_header.to_le_bytes());
    // A remote frame carries no bytes, whatever length it asks for.
    body[16] = data.len() as u8;
    body[17] = dlc;
    cmd[EXT_PAYLOAD..EXT_PAYLOAD + data.len()].copy_from_slice(data);
    cmd
}

/// Whose command a 32-byte or extended command is, and for which request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub cmd_no: u8,
    /// The entity that sent it.
    pub src_he: u8,
    pub transid: u16,
}

impl Header {
    /// `cmd` has at least four bytes.
    pub fn parse(cmd: &[u8]) -> Header {
        let transid = le16(cmd, 2);
        Header {
            cmd_no: cmd[0],
            // Two bits of the sender's address travel beside the
            // destination, four above the transaction id.
            src_he: ((cmd[1] & 0xC0) >> 2) | (transid >> 12) as u8,
            transid: transid & TRANSID_MASK,
        }
    }
}

/// The answer to [`map_channel_req`]: the entity's address.
pub fn map_channel_resp(cmd: &[u8; CMD_LEN]) -> u8 {
    cmd[4]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardInfo {
    pub serial: u32,
    /// The article number, 13 decimal digits packed four bits each.
    pub ean: [u32; 2],
    pub channels: u8,
}

pub fn card_info(cmd: &[u8; CMD_LEN]) -> CardInfo {
    CardInfo {
        serial: le32(cmd, 4),
        ean: [le32(cmd, 16), le32(cmd, 20)],
        channels: cmd[28],
    }
}

/// From the software info: how many frames one channel may have with the
/// adapter, sent and not yet reported.
pub fn max_outstanding_tx(cmd: &[u8; CMD_LEN]) -> u16 {
    le16(cmd, 12)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoftwareDetails {
    /// `SW_FLAG_*`.
    pub flags: u32,
    /// Major, minor, build.
    pub version: (u8, u8, u16),
    pub max_bitrate: u32,
}

pub fn software_details(cmd: &[u8; CMD_LEN]) -> SoftwareDetails {
    let version = le32(cmd, 8);
    SoftwareDetails {
        flags: le32(cmd, 4),
        version: ((version >> 24) as u8, (version >> 16) as u8, version as u16),
        max_bitrate: le32(cmd, 24),
    }
}

/// The answer to [`capability_req`]: the channels (one bit each) that have
/// the capability, or `None` if the firmware could not say.
pub fn capability_resp(cmd: &[u8; CMD_LEN]) -> Option<(u16, u32)> {
    let status = le16(cmd, 6);
    (status == CAP_STAT_OK).then(|| (le16(cmd, 4), le32(cmd, 8) & le32(cmd, 12)))
}

/// The answer to [`get_busparams_req`].
pub fn busparams_resp(cmd: &[u8; CMD_LEN]) -> Busparams {
    Busparams::read(&cmd[4..12])
}

/// How the adapter's clock ticks, by the kind of controller it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    /// Ticks in a microsecond.
    pub ticks_per_us: u32,
}

impl Clock {
    pub fn ns(&self, ticks: u64) -> u64 {
        (u128::from(ticks) * 1000 / u128::from(self.ticks_per_us)) as u64
    }
}

fn timestamp48(cmd: &[u8], at: usize) -> u64 {
    u64::from(le16(cmd, at))
        | u64::from(le16(cmd, at + 2)) << 16
        | u64::from(le16(cmd, at + 4)) << 32
}

fn timestamp64(cmd: &[u8], at: usize) -> u64 {
    u64::from(le32(cmd, at)) | u64::from(le32(cmd, at + 4)) << 32
}

/// In order of severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BusState {
    Active,
    Warning,
    Passive,
    BusOff,
}

/// The controller's state from the status the adapter reports and its
/// error counters. The firmware does not tell error-warning from
/// error-active; the counters do.
pub fn bus_state(bus_status: u8, tx_errors: u8, rx_errors: u8) -> BusState {
    if bus_status & BUS_BUS_OFF != 0 {
        BusState::BusOff
    } else if bus_status & BUS_ERR_PASS != 0 || tx_errors >= 128 || rx_errors >= 128 {
        BusState::Passive
    } else if tx_errors >= 96 || rx_errors >= 96 {
        BusState::Warning
    } else {
        BusState::Active
    }
}

/// What a report of the controller's state makes of the state a channel
/// is known to be in, if anything new.
///
/// Bus-off ends only when the interface is taken down and up. And a
/// report that comes with an error on the bus may only make things worse:
/// the USBcan Light 4xHS sends zeros for status and counters with every
/// bus error, which would otherwise read as a return to error-active. The
/// way back is told by a chip state event.
pub fn next_state(now: BusState, reported: BusState, with_bus_error: bool) -> Option<BusState> {
    if now == BusState::BusOff || reported == now || (with_bus_error && reported < now) {
        None
    } else {
        Some(reported)
    }
}

/// The controller's status and counters, as they come with a chip state
/// event and with an error frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChipState {
    pub bus_status: u8,
    pub tx_errors: u8,
    pub rx_errors: u8,
}

impl ChipState {
    pub fn bus_state(&self) -> BusState {
        bus_state(self.bus_status, self.tx_errors, self.rx_errors)
    }
}

/// One command from the adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A frame from the bus.
    Frame {
        src_he: u8,
        frame: Frame,
        ticks: u64,
        /// Frames before this one were lost in the adapter.
        overrun: bool,
    },
    /// The frame sent with `transid` was on the bus, or with `failed`
    /// (one-shot mode only) was given up.
    Sent {
        src_he: u8,
        transid: u16,
        ticks: u64,
        failed: bool,
    },
    /// The controller saw an error on the bus.
    BusError {
        src_he: u8,
        state: ChipState,
        ticks: u64,
    },
    /// The controller's state, when it changes or when asked for.
    State { src_he: u8, state: ChipState },
    /// The firmware refused a command: its code, and the command number.
    Refused { code: u8, cmd_no: u16 },
    /// An answer to a request, for whoever is waiting for it.
    Reply { header: Header, cmd: [u8; CMD_LEN] },
    /// Nothing to act on.
    Ignored,
    /// A command this driver does not know.
    Unknown { cmd_no: u8, ext: Option<u8> },
}

fn err_frame_state(data: &[u8]) -> ChipState {
    ChipState {
        bus_status: data[0],
        tx_errors: data[2],
        rx_errors: data[3],
    }
}

/// Decode one whole command, as [`Splitter`] hands them out.
pub fn decode(cmd: &[u8]) -> Event {
    let header = Header::parse(cmd);
    let src_he = header.src_he;
    if header.cmd_no == CMD_EXTENDED {
        return decode_ext(cmd, header);
    }
    let Ok(cmd) = <&[u8; CMD_LEN]>::try_from(cmd) else {
        return Event::Ignored;
    };
    match header.cmd_no {
        CMD_RX_MESSAGE => {
            let flags = u32::from(cmd[7]);
            let ticks = timestamp48(cmd, 8);
            if flags & CF_FLAG_ERROR_FRAME != 0 {
                return Event::BusError {
                    src_he,
                    state: err_frame_state(&cmd[20..28]),
                    ticks,
                };
            }
            let raw = le32(cmd, 16);
            let mut can_id = if raw & EXTENDED_FRAME_ID != 0 {
                (raw & CAN_EFF_MASK) | CAN_EFF_FLAG
            } else {
                raw & CAN_SFF_MASK
            };
            let len = usize::from(cmd[14]).min(8);
            if flags & CF_FLAG_REMOTE_FRAME != 0 {
                can_id |= CAN_RTR_FLAG;
            }
            match Frame::from_raw(can_id, 0, &cmd[20..20 + len]) {
                Ok(frame) => Event::Frame {
                    src_he,
                    frame,
                    ticks,
                    overrun: flags & CF_FLAG_OVERRUN != 0,
                },
                Err(_) => Event::Ignored,
            }
        }
        CMD_TX_ACKNOWLEDGE => Event::Sent {
            src_he,
            transid: header.transid,
            ticks: timestamp48(cmd, 18),
            failed: false,
        },
        CMD_CHIP_STATE_EVENT => Event::State {
            src_he,
            state: ChipState {
                tx_errors: cmd[10],
                rx_errors: cmd[11],
                bus_status: cmd[12],
            },
        },
        CMD_ERROR_EVENT => Event::Refused {
            code: cmd[11],
            cmd_no: le16(cmd, 12),
        },
        CMD_START_CHIP_RESP
        | CMD_STOP_CHIP_RESP
        | CMD_FLUSH_QUEUE_RESP
        | CMD_GET_BUSPARAMS_RESP
        | CMD_MAP_CHANNEL_RESP
        | CMD_GET_CARD_INFO_RESP
        | CMD_GET_SOFTWARE_INFO_RESP
        | CMD_GET_SOFTWARE_DETAILS_RESP
        | CMD_GET_CAPABILITIES_RESP => Event::Reply { header, cmd: *cmd },
        // Zero is the padding of a transfer.
        0 | CMD_SET_BUSPARAMS_RESP | CMD_SET_BUSPARAMS_FD_RESP => Event::Ignored,
        cmd_no => Event::Unknown { cmd_no, ext: None },
    }
}

fn decode_ext(cmd: &[u8], header: Header) -> Event {
    let src_he = header.src_he;
    if cmd.len() < EXT_PAYLOAD {
        return Event::Ignored;
    }
    let body = &cmd[EXT_BODY..];
    let flags = le32(body, 0);
    match cmd[6] {
        CMD_RX_MESSAGE_FD => {
            let ticks = timestamp64(body, 16);
            let payload = &cmd[EXT_PAYLOAD..];
            if flags & CF_FLAG_ERROR_FRAME != 0 {
                if payload.len() < 4 {
                    return Event::Ignored;
                }
                return Event::BusError {
                    src_he,
                    state: err_frame_state(payload),
                    ticks,
                };
            }
            let dlc = ((le32(body, 12) & KCAN_DATA_DLC_MASK) >> KCAN_DATA_DLC_SHIFT) as u8;
            let raw = le32(body, 4);
            let mut can_id = if flags & CF_FLAG_EXTENDED_ID != 0 {
                (raw & CAN_EFF_MASK) | CAN_EFF_FLAG
            } else {
                raw & CAN_SFF_MASK
            };
            let fd = flags & CF_FLAG_FDF != 0;
            let mut fd_flags = 0;
            let len = if fd {
                fd_flags = CANFD_FDF;
                if flags & CF_FLAG_BRS != 0 {
                    fd_flags |= CANFD_BRS;
                }
                if flags & CF_FLAG_ESI != 0 {
                    fd_flags |= CANFD_ESI;
                }
                fd_dlc_to_len(dlc)
            } else {
                usize::from(dlc).min(8)
            };
            let remote = !fd && flags & CF_FLAG_REMOTE_FRAME != 0;
            if remote {
                can_id |= CAN_RTR_FLAG;
            }
            // A remote frame's length is what it asks for, not what it
            // carries.
            let data = if remote {
                &[0u8; 8][..len]
            } else if payload.len() >= len {
                &payload[..len]
            } else {
                return Event::Ignored;
            };
            match Frame::from_raw(can_id, fd_flags, data) {
                Ok(frame) => Event::Frame {
                    src_he,
                    frame,
                    ticks,
                    overrun: flags & CF_FLAG_OVERRUN != 0,
                },
                Err(_) => Event::Ignored,
            }
        }
        CMD_TX_ACKNOWLEDGE_FD => Event::Sent {
            src_he,
            transid: header.transid,
            ticks: timestamp64(body, 8),
            failed: flags & (CF_FLAG_OSM_NACK | CF_FLAG_ABL) != 0,
        },
        ext => Event::Unknown {
            cmd_no: CMD_EXTENDED,
            ext: Some(ext),
        },
    }
}

/// How long the command that begins with these bytes is, if that can be
/// told yet (an extended command says so in its fifth and sixth byte).
fn command_len(start: &[u8]) -> Option<usize> {
    match start.first() {
        Some(&CMD_EXTENDED) => (start.len() >= 6).then(|| usize::from(le16(start, 4))),
        Some(_) => Some(CMD_LEN),
        None => None,
    }
}

/// Cuts what arrives on the bulk IN endpoint into commands. A command may
/// begin in one transfer and end in the next.
#[derive(Default)]
pub struct Splitter {
    leftover: Vec<u8>,
}

/// The adapter sent something that cannot be a command; what was held
/// over has been dropped.
#[derive(Debug, PartialEq, Eq)]
pub struct Malformed;

impl Splitter {
    pub fn new() -> Splitter {
        Splitter::default()
    }

    /// Forget a command that was begun (the stream starts afresh).
    pub fn reset(&mut self) {
        self.leftover.clear();
    }

    pub fn feed(&mut self, mut data: &[u8], mut each: impl FnMut(&[u8])) -> Result<(), Malformed> {
        // Finish the command the last transfer began.
        while !self.leftover.is_empty() {
            let Some(len) = command_len(&self.leftover) else {
                // Its length is not known yet: take a byte more.
                let Some((&next, rest)) = data.split_first() else {
                    return Ok(());
                };
                self.leftover.push(next);
                data = rest;
                continue;
            };
            if !(EXT_BODY..=MAX_CMD_LEN).contains(&len) {
                self.leftover.clear();
                return Err(Malformed);
            }
            let take = (len - self.leftover.len()).min(data.len());
            self.leftover.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.leftover.len() < len {
                return Ok(());
            }
            each(&self.leftover);
            self.leftover.clear();
        }
        while !data.is_empty() {
            match command_len(data) {
                Some(len) if !(EXT_BODY..=MAX_CMD_LEN).contains(&len) => return Err(Malformed),
                Some(len) if len <= data.len() => {
                    each(&data[..len]);
                    data = &data[len..];
                }
                _ => {
                    self.leftover.extend_from_slice(data);
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

/// The error frame announcing a bus state change, in SocketCAN's format.
/// The larger counter tells which direction caused the state.
pub fn state_error_frame(state: BusState, tx_errors: u8, rx_errors: u8) -> Frame {
    let mut detail = [0u8; 8];
    let tx = tx_errors >= rx_errors;
    let rx = rx_errors >= tx_errors;
    let pick =
        |tx_bit: u8, rx_bit: u8| (if tx { tx_bit } else { 0 }) | (if rx { rx_bit } else { 0 });
    let class = match state {
        // The counters mean nothing once the controller is off the bus.
        BusState::BusOff => return Frame::new_error(err::BUSOFF, detail),
        BusState::Passive => {
            detail[1] = pick(err::CRTL_TX_PASSIVE, err::CRTL_RX_PASSIVE);
            err::CRTL
        }
        BusState::Warning => {
            detail[1] = pick(err::CRTL_TX_WARNING, err::CRTL_RX_WARNING);
            err::CRTL
        }
        BusState::Active => {
            detail[1] = err::CRTL_ACTIVE;
            err::CRTL
        }
    };
    detail[6] = tx_errors;
    detail[7] = rx_errors;
    Frame::new_error(class | err::CNT, detail)
}

pub fn overrun_error_frame() -> Frame {
    Frame::new_error(err::CRTL, [0, err::CRTL_RX_OVERFLOW, 0, 0, 0, 0, 0, 0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use darwincan_core::Id;

    /// A command as the adapter sends it: from `src_he`, answering
    /// `transid`.
    fn from_adapter(cmd_no: u8, src_he: u8, transid: u16) -> [u8; CMD_LEN] {
        let mut cmd = [0u8; CMD_LEN];
        cmd[0] = cmd_no;
        cmd[1] = (src_he << 2) & 0xC0;
        cmd[2..4].copy_from_slice(&(transid | u16::from(src_he & 0xF) << 12).to_le_bytes());
        cmd
    }

    #[test]
    fn a_header_carries_the_sender_in_two_places() {
        // struct kvaser_cmd_header: cmd_no, he_addr, transid.
        let cmd = from_adapter(CMD_START_CHIP_RESP, 0x2B, 0x0123);
        assert_eq!(cmd[1], 0x80);
        assert_eq!(le16(&cmd, 2), 0xB123);
        assert_eq!(
            Header::parse(&cmd),
            Header {
                cmd_no: CMD_START_CHIP_RESP,
                src_he: 0x2B,
                transid: 0x123
            }
        );
        // To the adapter: the destination in the low six bits.
        let cmd = simple(CMD_START_CHIP_REQ, 0x2B, 0x1FFF);
        assert_eq!(&cmd[..4], [26, 0x2B, 0xFF, 0x0F]);
        assert!(cmd[4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn requests_are_laid_out_as_the_driver_structs() {
        // struct kvaser_cmd_map_ch_req: name[16], channel.
        let cmd = map_channel_req(TRANSID_CANHE | 3, 3, "CAN");
        assert_eq!(&cmd[..4], [200, HE_ROUTER, 0x43, 0x00]);
        assert_eq!(&cmd[4..8], b"CAN\0");
        assert_eq!(cmd[20], 3);
        let cmd = map_channel_req(TRANSID_SYSDBG, 0, "SYSDBG");
        assert_eq!(&cmd[4..11], b"SYSDBG\0");

        let cmd = software_details_req(7);
        assert_eq!(&cmd[..5], [202, HE_ILLEGAL, 7, 0, 1]);

        // struct kvaser_cmd_cap_req: cap_cmd.
        let cmd = capability_req(0x11, 8, CAP_LISTEN_MODE);
        assert_eq!(&cmd[..6], [95, 0x11, 8, 0, 2, 0]);

        // struct kvaser_cmd_set_busparams: nominal, reserved[4], data,
        // canfd_mode; struct kvaser_usb_busparams: bitrate, tseg1, tseg2,
        // sjw, nsamples.
        let params = Busparams {
            bitrate: 500_000,
            tseg1: 139,
            tseg2: 20,
            sjw: 16,
            nsamples: 1,
        };
        let cmd = set_busparams(0x22, 9, &params);
        assert_eq!(cmd[0], 16);
        assert_eq!(&cmd[4..12], [0x20, 0xA1, 0x07, 0x00, 139, 20, 16, 1]);
        assert!(cmd[12..].iter().all(|&b| b == 0));
        let cmd = set_busparams_fd(0x22, 9, &params);
        assert_eq!(cmd[0], 69);
        assert!(cmd[4..16].iter().all(|&b| b == 0));
        assert_eq!(&cmd[16..24], [0x20, 0xA1, 0x07, 0x00, 139, 20, 16, 1]);
        assert_eq!(cmd[24], 1);

        assert_eq!(get_busparams_req(0x22, 1, BUSPARAM_TYPE_CANFD)[4], 1);
        assert_eq!(set_driver_mode(0x22, 1, true)[4], 2);
        assert_eq!(set_driver_mode(0x22, 1, false)[4], 1);

        // What comes back is compared with what was sent.
        let mut resp = from_adapter(CMD_GET_BUSPARAMS_RESP, 0x22, 1);
        resp[4..12].copy_from_slice(&[0x20, 0xA1, 0x07, 0x00, 139, 20, 16, 1]);
        assert_eq!(busparams_resp(&resp), params);
    }

    #[test]
    fn answers_about_the_device_are_read() {
        // struct kvaser_cmd_card_info.
        let mut cmd = from_adapter(CMD_GET_CARD_INFO_RESP, 1, 2);
        cmd[4..8].copy_from_slice(&12345u32.to_le_bytes());
        cmd[16..20].copy_from_slice(&0x3000_6312u32.to_le_bytes());
        cmd[20..24].copy_from_slice(&0x0007_3301u32.to_le_bytes());
        cmd[28] = 4;
        assert_eq!(
            card_info(&cmd),
            CardInfo {
                serial: 12345,
                ean: [0x3000_6312, 0x0007_3301],
                channels: 4
            }
        );

        // struct kvaser_cmd_sw_info: reserved0[8], max_outstanding_tx.
        let mut cmd = from_adapter(CMD_GET_SOFTWARE_INFO_RESP, 1, 2);
        cmd[12..14].copy_from_slice(&200u16.to_le_bytes());
        assert_eq!(max_outstanding_tx(&cmd), 200);

        // struct kvaser_cmd_sw_detail_res.
        let mut cmd = from_adapter(CMD_GET_SOFTWARE_DETAILS_RESP, 1, 2);
        cmd[4..8].copy_from_slice(&(SW_FLAG_EXT_CMD | SW_FLAG_FREQ_80M).to_le_bytes());
        cmd[8..12].copy_from_slice(&0x0304_0123u32.to_le_bytes());
        cmd[24..28].copy_from_slice(&1_000_000u32.to_le_bytes());
        let details = software_details(&cmd);
        assert_eq!(details.version, (3, 4, 0x123));
        assert_eq!(details.max_bitrate, 1_000_000);
        assert_ne!(details.flags & SW_FLAG_EXT_CMD, 0);

        // struct kvaser_cmd_cap_res: cap_cmd, status, mask, value.
        let mut cmd = from_adapter(CMD_GET_CAPABILITIES_RESP, 1, 2);
        cmd[4] = CAP_LISTEN_MODE as u8;
        cmd[8..12].copy_from_slice(&0x0Fu32.to_le_bytes());
        cmd[12..16].copy_from_slice(&0x05u32.to_le_bytes());
        assert_eq!(capability_resp(&cmd), Some((CAP_LISTEN_MODE, 0x05)));
        cmd[6] = 1; // not implemented
        assert_eq!(capability_resp(&cmd), None);
    }

    #[test]
    fn classic_frames_are_encoded_for_sending() {
        // struct kvaser_cmd_tx_can: id, data[8], dlc, flags.
        let frame = Frame::new(Id::Standard(0x123), &[1, 2, 3]).unwrap();
        let cmd = encode_tx_std(0x05, 17, &frame);
        assert_eq!(&cmd[..4], [33, 0x05, 17, 0]);
        assert_eq!(le32(&cmd, 4), 0x123);
        assert_eq!(&cmd[8..16], [1, 2, 3, 0, 0, 0, 0, 0]);
        assert_eq!((cmd[16], cmd[17]), (3, 0));

        let frame = Frame::new(Id::Extended(0x18DA_F110), &[0xAA; 8]).unwrap();
        let cmd = encode_tx_std(0x05, 0, &frame);
        assert_eq!(le32(&cmd, 4), 0x98DA_F110);
        assert_eq!((cmd[16], cmd[17]), (8, 0x20));

        let frame = Frame::new_remote(Id::Standard(0x7FF), 4).unwrap();
        let cmd = encode_tx_std(0x05, 0, &frame);
        assert_eq!((cmd[16], cmd[17]), (4, 0x10));
        assert!(cmd[8..16].iter().all(|&b| b == 0));
    }

    #[test]
    fn extended_commands_carry_frames_of_any_kind() {
        // struct kvaser_cmd_ext: header, len, cmd_no_ext, reserved; then
        // struct kvaser_cmd_ext_tx_can: flags, id, kcan_id, kcan_header,
        // databytes, dlc, reserved[6], kcan_payload.
        let frame = Frame::new(Id::Standard(0x123), &[1, 2, 3]).unwrap();
        let cmd = encode_tx_ext(0x05, 17, &frame);
        assert_eq!(cmd.len(), 40);
        assert_eq!(&cmd[..8], [255, 0x05, 17, 0, 40, 0, 224, 0]);
        assert_eq!(le32(&cmd, 8), 0);
        assert_eq!(le32(&cmd, 12), 0x123);
        assert_eq!(le32(&cmd, 16), 0x123);
        assert_eq!(le32(&cmd, 20), KCAN_DATA_AREQ | 3 << 8);
        assert_eq!((cmd[24], cmd[25]), (3, 3));
        assert_eq!(&cmd[32..35], [1, 2, 3]);

        let frame = Frame::new(Id::Extended(0x18DA_F110), &[]).unwrap();
        let cmd = encode_tx_ext(0x05, 1, &frame);
        assert_eq!(cmd.len(), 32);
        assert_eq!(le32(&cmd, 8), CF_FLAG_EXTENDED_ID);
        assert_eq!(le32(&cmd, 12), 0x18DA_F110);
        assert_eq!(le32(&cmd, 16), 0x18DA_F110 | KCAN_DATA_IDE | KCAN_DATA_SRR);

        let frame = Frame::new_remote(Id::Standard(0x100), 8).unwrap();
        let cmd = encode_tx_ext(0x05, 1, &frame);
        assert_eq!(cmd.len(), 32);
        assert_eq!(le32(&cmd, 8), CF_FLAG_REMOTE_FRAME);
        assert_eq!(le32(&cmd, 16), 0x100 | KCAN_DATA_RTR);
        assert_eq!(le32(&cmd, 20), KCAN_DATA_AREQ | 8 << 8);
        assert_eq!((cmd[24], cmd[25]), (0, 8));

        let data: Vec<u8> = (0..12).collect();
        let frame = Frame::new_fd(Id::Standard(0x200), &data, true).unwrap();
        let cmd = encode_tx_ext(0x05, 1, &frame);
        assert_eq!(cmd.len(), 48);
        assert_eq!(le16(&cmd, 4), 48);
        assert_eq!(
            le32(&cmd, 20),
            KCAN_DATA_AREQ | KCAN_DATA_FDF | KCAN_DATA_BRS | 9 << 8
        );
        assert_eq!((cmd[24], cmd[25]), (12, 9));
        assert_eq!(&cmd[32..44], &data[..]);

        let frame = Frame::new_fd(Id::Standard(0x200), &[7; 64], false).unwrap();
        let cmd = encode_tx_ext(0x05, 1, &frame);
        assert_eq!(cmd.len(), 96);
        assert_eq!(le32(&cmd, 20), KCAN_DATA_AREQ | KCAN_DATA_FDF | 15 << 8);
    }

    #[test]
    fn received_classic_frames_are_decoded() {
        // struct kvaser_cmd_rx_can: cmd_len, cmd_no, channel, flags,
        // timestamp[3], dlc, padding, id, data[8].
        let mut cmd = from_adapter(CMD_RX_MESSAGE, 0x07, 0);
        cmd[8..14].copy_from_slice(&[0x01, 0x00, 0x02, 0x00, 0x03, 0x00]);
        cmd[14] = 2;
        cmd[16..20].copy_from_slice(&0x123u32.to_le_bytes());
        cmd[20..22].copy_from_slice(&[0xDE, 0xAD]);
        assert_eq!(
            decode(&cmd),
            Event::Frame {
                src_he: 0x07,
                frame: Frame::new(Id::Standard(0x123), &[0xDE, 0xAD]).unwrap(),
                ticks: 0x0003_0002_0001,
                overrun: false,
            }
        );

        cmd[16..20].copy_from_slice(&0x98DA_F110u32.to_le_bytes());
        cmd[7] = (CF_FLAG_REMOTE_FRAME | CF_FLAG_OVERRUN) as u8;
        cmd[14] = 12; // a DLC above 8 is 8 bytes
        match decode(&cmd) {
            Event::Frame { frame, overrun, .. } => {
                assert_eq!(
                    frame,
                    Frame::new_remote(Id::Extended(0x18DA_F110), 8).unwrap()
                );
                assert!(overrun);
            }
            other => panic!("{other:?}"),
        }

        // struct kvaser_err_frame_data in place of the data: bus_status,
        // reserved0, tx_err_counter, rx_err_counter.
        cmd[7] = CF_FLAG_ERROR_FRAME as u8;
        cmd[20..24].copy_from_slice(&[BUS_ERR_PASS, 0, 130, 3]);
        match decode(&cmd) {
            Event::BusError { src_he, state, .. } => {
                assert_eq!(src_he, 0x07);
                assert_eq!((state.tx_errors, state.rx_errors), (130, 3));
                assert_eq!(state.bus_state(), BusState::Passive);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn received_extended_commands_are_decoded() {
        // struct kvaser_cmd_ext_rx_can: flags, id, kcan_id, kcan_header,
        // timestamp, kcan_payload.
        let mut cmd = vec![0u8; 48];
        cmd[..4].copy_from_slice(&from_adapter(CMD_EXTENDED, 0x15, 0)[..4]);
        cmd[4..6].copy_from_slice(&48u16.to_le_bytes());
        cmd[6] = CMD_RX_MESSAGE_FD;
        cmd[8..12].copy_from_slice(&(CF_FLAG_FDF | CF_FLAG_BRS | CF_FLAG_ESI).to_le_bytes());
        cmd[12..16].copy_from_slice(&0x200u32.to_le_bytes());
        cmd[20..24].copy_from_slice(&(9u32 << 8).to_le_bytes());
        cmd[24..32].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        for (n, byte) in cmd[32..44].iter_mut().enumerate() {
            *byte = n as u8;
        }
        let data: Vec<u8> = (0..12).collect();
        match decode(&cmd) {
            Event::Frame {
                src_he,
                frame,
                ticks,
                overrun,
            } => {
                assert_eq!(src_he, 0x15);
                assert_eq!(frame.can_id(), 0x200);
                assert_eq!(frame.data(), &data[..]);
                assert_eq!(frame.flags(), CANFD_FDF | CANFD_BRS | CANFD_ESI);
                assert_eq!(ticks, 0x0102_0304_0506_0708);
                assert!(!overrun);
            }
            other => panic!("{other:?}"),
        }

        // A classic frame with a 29-bit identifier.
        cmd[8..12].copy_from_slice(&CF_FLAG_EXTENDED_ID.to_le_bytes());
        cmd[12..16].copy_from_slice(&0x18DA_F110u32.to_le_bytes());
        cmd[20..24].copy_from_slice(&(3u32 << 8).to_le_bytes());
        match decode(&cmd) {
            Event::Frame { frame, .. } => {
                assert_eq!(
                    frame,
                    Frame::new(Id::Extended(0x18DA_F110), &[0, 1, 2]).unwrap()
                );
            }
            other => panic!("{other:?}"),
        }

        // A command cut short of the bytes its DLC promises is dropped.
        cmd[8..12].copy_from_slice(&CF_FLAG_FDF.to_le_bytes());
        cmd[20..24].copy_from_slice(&(15u32 << 8).to_le_bytes());
        assert_eq!(decode(&cmd), Event::Ignored);

        // struct kvaser_cmd_ext_tx_ack: flags, reserved0[4], timestamp.
        let mut ack = vec![0u8; 32];
        ack[..4].copy_from_slice(&from_adapter(CMD_EXTENDED, 0x15, 31)[..4]);
        ack[4..6].copy_from_slice(&32u16.to_le_bytes());
        ack[6] = CMD_TX_ACKNOWLEDGE_FD;
        ack[16..24].copy_from_slice(&99u64.to_le_bytes());
        assert_eq!(
            decode(&ack),
            Event::Sent {
                src_he: 0x15,
                transid: 31,
                ticks: 99,
                failed: false
            }
        );
        ack[8..12].copy_from_slice(&CF_FLAG_OSM_NACK.to_le_bytes());
        assert!(matches!(decode(&ack), Event::Sent { failed: true, .. }));
    }

    #[test]
    fn reports_and_replies_are_told_apart() {
        // struct kvaser_cmd_tx_ack: id, data[8], dlc, flags, timestamp[3].
        let mut cmd = from_adapter(CMD_TX_ACKNOWLEDGE, 0x07, 5);
        cmd[18..24].copy_from_slice(&[0x10, 0, 0, 0, 0, 0]);
        assert_eq!(
            decode(&cmd),
            Event::Sent {
                src_he: 0x07,
                transid: 5,
                ticks: 0x10,
                failed: false
            }
        );

        // struct kvaser_cmd_chip_state_event: timestamp[3], tx_err_counter,
        // rx_err_counter, bus_status.
        let mut cmd = from_adapter(CMD_CHIP_STATE_EVENT, 0x07, 0);
        cmd[10..13].copy_from_slice(&[100, 2, 0]);
        match decode(&cmd) {
            Event::State { state, .. } => assert_eq!(state.bus_state(), BusState::Warning),
            other => panic!("{other:?}"),
        }
        cmd[12] = BUS_BUS_OFF;
        match decode(&cmd) {
            Event::State { state, .. } => assert_eq!(state.bus_state(), BusState::BusOff),
            other => panic!("{other:?}"),
        }

        // struct kvaser_cmd_error_event: timestamp[3], reserved,
        // error_code, info1, info2.
        let mut cmd = from_adapter(CMD_ERROR_EVENT, 0x07, 0);
        cmd[11] = ERROR_EVENT_PARAM;
        cmd[12] = CMD_SET_BUSPARAMS_REQ;
        assert_eq!(
            decode(&cmd),
            Event::Refused {
                code: ERROR_EVENT_PARAM,
                cmd_no: 16
            }
        );

        let cmd = from_adapter(CMD_STOP_CHIP_RESP, 0x07, 9);
        assert!(matches!(decode(&cmd), Event::Reply { header, .. } if header.transid == 9));
        assert_eq!(decode(&[0u8; CMD_LEN]), Event::Ignored);
        assert_eq!(
            decode(&from_adapter(CMD_SET_BUSPARAMS_RESP, 0x07, 0)),
            Event::Ignored
        );
        assert_eq!(
            decode(&from_adapter(123, 0x07, 0)),
            Event::Unknown {
                cmd_no: 123,
                ext: None
            }
        );
    }

    #[test]
    fn bus_state_follows_status_and_counters() {
        assert_eq!(bus_state(0, 0, 0), BusState::Active);
        assert_eq!(bus_state(0, 95, 95), BusState::Active);
        assert_eq!(bus_state(0, 96, 0), BusState::Warning);
        assert_eq!(bus_state(0, 0, 127), BusState::Warning);
        assert_eq!(bus_state(0, 128, 0), BusState::Passive);
        assert_eq!(bus_state(BUS_ERR_PASS, 0, 0), BusState::Passive);
        assert_eq!(
            bus_state(BUS_BUS_OFF | BUS_ERR_PASS, 0, 0),
            BusState::BusOff
        );

        use BusState::{Active, BusOff, Passive, Warning};
        assert_eq!(next_state(Active, Passive, false), Some(Passive));
        assert_eq!(next_state(Active, Passive, true), Some(Passive));
        assert_eq!(next_state(Passive, Passive, false), None);
        // Only a chip state event brings a channel back.
        assert_eq!(next_state(Passive, Active, true), None);
        assert_eq!(next_state(Passive, Warning, true), None);
        assert_eq!(next_state(Passive, Active, false), Some(Active));
        assert_eq!(next_state(Warning, BusOff, true), Some(BusOff));
        assert_eq!(next_state(BusOff, Active, false), None);

        let frame = state_error_frame(BusState::Passive, 130, 3);
        assert_eq!(frame.can_id() & err::MASK, err::CRTL | err::CNT);
        assert_eq!(frame.data()[1], err::CRTL_TX_PASSIVE);
        assert_eq!(&frame.data()[6..8], [130, 3]);
        let frame = state_error_frame(BusState::BusOff, 255, 0);
        assert_eq!(frame.can_id() & err::MASK, err::BUSOFF);
        let frame = state_error_frame(BusState::Active, 0, 0);
        assert_eq!(frame.data()[1], err::CRTL_ACTIVE);
    }

    #[test]
    fn timestamps_are_ticks_of_the_adapters_clock() {
        // kvaser_usb_ticks_to_ktime: ticks * 1000 / timestamp_freq.
        assert_eq!(Clock { ticks_per_us: 80 }.ns(80), 1000);
        assert_eq!(Clock { ticks_per_us: 80 }.ns(1), 12);
        assert_eq!(Clock { ticks_per_us: 1 }.ns(5), 5000);
        assert_eq!(Clock { ticks_per_us: 24 }.ns(24_000_000), 1_000_000_000);
        // 48 bits of ticks at 80 MHz do not overflow.
        assert_eq!(
            Clock { ticks_per_us: 80 }.ns(0xFFFF_FFFF_FFFF),
            0xFFFF_FFFF_FFFF_u64 / 80 * 1000 + 0xFFFF_FFFF_FFFF_u64 % 80 * 1000 / 80
        );
    }

    #[test]
    fn commands_are_cut_out_of_transfers() {
        let a = from_adapter(CMD_START_CHIP_RESP, 1, 1);
        let b = from_adapter(CMD_STOP_CHIP_RESP, 2, 2);
        let mut ext = vec![0u8; 48];
        ext[0] = CMD_EXTENDED;
        ext[4..6].copy_from_slice(&48u16.to_le_bytes());
        ext[6] = CMD_RX_MESSAGE_FD;
        ext[47] = 0xEE;

        let mut stream = Vec::new();
        stream.extend_from_slice(&a);
        stream.extend_from_slice(&ext);
        stream.extend_from_slice(&b);

        // Whole, and cut at every possible place.
        for cut in 0..=stream.len() {
            let mut splitter = Splitter::new();
            let mut got: Vec<Vec<u8>> = Vec::new();
            splitter
                .feed(&stream[..cut], |cmd| got.push(cmd.to_vec()))
                .unwrap();
            splitter
                .feed(&stream[cut..], |cmd| got.push(cmd.to_vec()))
                .unwrap();
            assert_eq!(got, [a.to_vec(), ext.clone(), b.to_vec()], "cut at {cut}");
        }
        // A byte at a time.
        let mut splitter = Splitter::new();
        let mut got = 0;
        for byte in &stream {
            splitter.feed(&[*byte], |_| got += 1).unwrap();
        }
        assert_eq!(got, 3);

        // An extended command of a length there cannot be.
        let mut bad = vec![0u8; 16];
        bad[0] = CMD_EXTENDED;
        bad[4..6].copy_from_slice(&4u16.to_le_bytes());
        assert_eq!(Splitter::new().feed(&bad, |_| {}), Err(Malformed));
        bad[4..6].copy_from_slice(&500u16.to_le_bytes());
        let mut splitter = Splitter::new();
        assert_eq!(splitter.feed(&bad[..3], |_| {}), Ok(()));
        assert_eq!(splitter.feed(&bad[3..], |_| {}), Err(Malformed));
        // After which the stream is taken up afresh.
        let mut got = 0;
        splitter.feed(&a, |_| got += 1).unwrap();
        assert_eq!(got, 1);
    }
}
