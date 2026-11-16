// SPDX-License-Identifier: GPL-2.0-only
//
// Wire format of PEAK-System's "uCAN" protocol as spoken by the PCAN-USB FD
// family. Derived from the Linux kernel driver
// (drivers/net/can/usb/peak_usb/pcan_usb_fd.c and
// include/linux/can/dev/peak_canfd.h),
// Copyright (C) 2003-2025 PEAK System-Technik GmbH,
// Author: Stéphane Grosjean.

//! Encoding and decoding of uCAN commands and messages. Pure functions, no
//! I/O.

use darwincan_core::{
    err, Frame, CANFD_BRS, CANFD_ESI, CANFD_FDF, CAN_EFF_FLAG, CAN_EFF_MASK, CAN_RTR_FLAG,
    CAN_SFF_MASK,
};

use crate::bittiming::BitTiming;

// Command opcodes (low 10 bits of opcode_channel).
const CMD_RESET_MODE: u16 = 0x001;
const CMD_NORMAL_MODE: u16 = 0x002;
const CMD_LISTEN_ONLY_MODE: u16 = 0x003;
const CMD_TIMING_SLOW: u16 = 0x004;
const CMD_TIMING_FAST: u16 = 0x005;
const CMD_FILTER_STD: u16 = 0x008;
const CMD_WR_ERR_CNT: u16 = 0x00A;
const CMD_TX_ABORT: u16 = 0x009;
const CMD_SET_EN_OPTION: u16 = 0x00B;
const CMD_CLR_DIS_OPTION: u16 = 0x00C;
const CMD_CLK_SET: u16 = 0x080;
const CMD_LED_SET: u16 = 0x086;

const OPTION_ERROR: u16 = 0x0001;
const OPTION_CANFD_ISO: u16 = 0x0004;
/// USB-level option: periodic time calibration messages.
const USB_OPTION_CALIBRATION: u16 = 0x8000;

const WR_ERR_CNT_TX: u16 = 0x4000;
const WR_ERR_CNT_RX: u16 = 0x8000;

pub const CLOCK_80MHZ: u8 = 0;
pub const LED_DEVICE_DEFAULT: u8 = 0;
pub const LED_OFF: u8 = 4;

// Message types.
const MSG_CAN_RX: u16 = 0x0001;
const MSG_ERROR: u16 = 0x0002;
const MSG_STATUS: u16 = 0x0003;
const MSG_CALIBRATION: u16 = 0x0100;
const MSG_OVERRUN: u16 = 0x0101;
const MSG_CAN_TX: u16 = 0x1000;

// Flags of CAN messages.
const FLAG_ERROR_STATE_IND: u16 = 0x40;
const FLAG_BITRATE_SWITCH: u16 = 0x20;
const FLAG_EXT_DATA_LEN: u16 = 0x10;
/// In a transmit record: report the frame back once it has been sent.
/// In a receive record: this is such a report, not a frame from another
/// node. The record's client byte and tag come back unchanged. The USB
/// driver in the kernel does not use this; the PCIe one for the same
/// protocol does. Seen to work on a PCAN-USB X6, firmware 3.2.0.
const FLAG_LOOPED_BACK: u16 = 0x04;
const FLAG_EXT_ID: u16 = 0x02;
const FLAG_RTR: u16 = 0x01;

const STATUS_PASSIVE: u8 = 0x20;
const STATUS_WARNING: u8 = 0x40;
const STATUS_BUSOFF: u8 = 0x80;

/// Size of one command record.
pub const CMD_LEN: usize = 8;
/// A command packet holds at most this many bytes.
pub const CMD_BUFFER_SIZE: usize = 512;
const RX_MSG_HEADER: usize = 28;
const TX_MSG_HEADER: usize = 20;

const FD_LENGTHS: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 24, 32, 48, 64];

fn fd_len_to_dlc(len: usize) -> u8 {
    FD_LENGTHS
        .iter()
        .position(|&l| usize::from(l) == len)
        .expect("Frame guarantees a valid CAN FD length") as u8
}

/// A list of commands for one channel, sent to the device as one packet.
pub struct Commands {
    channel: u8,
    buf: Vec<u8>,
}

impl Commands {
    pub fn new(channel: u8) -> Commands {
        Commands {
            channel,
            buf: Vec::with_capacity(CMD_BUFFER_SIZE),
        }
    }

    fn push(&mut self, opcode: u16, args: [u8; 6]) -> &mut Self {
        let opcode_channel = (u16::from(self.channel) << 12) | (opcode & 0x3FF);
        self.buf.extend_from_slice(&opcode_channel.to_le_bytes());
        self.buf.extend_from_slice(&args);
        self
    }

    pub fn clock(&mut self, mode: u8) -> &mut Self {
        self.push(CMD_CLK_SET, [mode, 0, 0, 0, 0, 0])
    }

    pub fn led(&mut self, mode: u8) -> &mut Self {
        self.push(CMD_LED_SET, [mode, 0, 0, 0, 0, 0])
    }

    /// Nominal (arbitration phase) bit timing.
    pub fn timing_slow(&mut self, t: &BitTiming) -> &mut Self {
        let brp = ((t.brp - 1) as u16 & 0x3FF).to_le_bytes();
        self.push(
            CMD_TIMING_SLOW,
            [
                96, // error warning limit (device default)
                (t.sjw - 1) as u8 & 0x7F,
                (t.tseg2 - 1) as u8 & 0x7F,
                (t.tseg1 - 1) as u8,
                brp[0],
                brp[1],
            ],
        )
    }

    /// CAN FD data phase bit timing.
    pub fn timing_fast(&mut self, t: &BitTiming) -> &mut Self {
        let brp = ((t.brp - 1) as u16 & 0x3FF).to_le_bytes();
        self.push(
            CMD_TIMING_FAST,
            [
                0,
                (t.sjw - 1) as u8 & 0x0F,
                (t.tseg2 - 1) as u8 & 0x0F,
                (t.tseg1 - 1) as u8 & 0x1F,
                brp[0],
                brp[1],
            ],
        )
    }

    /// Accept every 11-bit identifier: the device filters standard ids
    /// through a 64 x 32 bit table, one bit per identifier.
    pub fn accept_all_standard_ids(&mut self) -> &mut Self {
        for row in 0u16..64 {
            let idx = row.to_le_bytes();
            self.push(CMD_FILTER_STD, [idx[0], idx[1], 0xFF, 0xFF, 0xFF, 0xFF]);
        }
        self
    }

    /// Enable or disable error reporting and time calibration messages.
    pub fn error_and_calibration_messages(&mut self, on: bool) -> &mut Self {
        let ucan = OPTION_ERROR.to_le_bytes();
        let usb = USB_OPTION_CALIBRATION.to_le_bytes();
        let opcode = if on {
            CMD_SET_EN_OPTION
        } else {
            CMD_CLR_DIS_OPTION
        };
        self.push(opcode, [ucan[0], ucan[1], 0, 0, usb[0], usb[1]])
    }

    /// Enable or disable error reporting for this channel only.
    pub fn error_messages(&mut self, on: bool) -> &mut Self {
        let ucan = OPTION_ERROR.to_le_bytes();
        let opcode = if on {
            CMD_SET_EN_OPTION
        } else {
            CMD_CLR_DIS_OPTION
        };
        self.push(opcode, [ucan[0], ucan[1], 0, 0, 0, 0])
    }

    /// Select ISO CAN FD framing (firmware 2.x and later can switch).
    pub fn iso_fd(&mut self) -> &mut Self {
        let opt = OPTION_CANFD_ISO.to_le_bytes();
        self.push(CMD_SET_EN_OPTION, [opt[0], opt[1], 0, 0, 0, 0])
    }

    pub fn reset_error_counters(&mut self) -> &mut Self {
        let sel = (WR_ERR_CNT_TX | WR_ERR_CNT_RX).to_le_bytes();
        self.push(CMD_WR_ERR_CNT, [sel[0], sel[1], 0, 0, 0, 0])
    }

    /// Go on the bus.
    pub fn operational(&mut self, listen_only: bool) -> &mut Self {
        let opcode = if listen_only {
            CMD_LISTEN_ONLY_MODE
        } else {
            CMD_NORMAL_MODE
        };
        self.push(opcode, [0; 6])
    }

    /// Go off the bus.
    pub fn reset_mode(&mut self) -> &mut Self {
        self.push(CMD_RESET_MODE, [0; 6])
    }

    /// Discard the frames waiting to be sent. Only valid in reset mode.
    pub fn flush_transmit_queue(&mut self) -> &mut Self {
        const TX_ABORT_FLUSH: u16 = 0x0001;
        let flags = TX_ABORT_FLUSH.to_le_bytes();
        self.push(CMD_TX_ABORT, [flags[0], flags[1], 0, 0, 0, 0])
    }

    /// The packet to send. A list that does not fill the command buffer is
    /// terminated by an end-of-collection record of all ones.
    pub fn finish(&self) -> Vec<u8> {
        assert!(self.buf.len() <= CMD_BUFFER_SIZE, "command list too long");
        let mut packet = self.buf.clone();
        if packet.len() <= CMD_BUFFER_SIZE - CMD_LEN {
            packet.extend_from_slice(&[0xFF; CMD_LEN]);
        }
        packet
    }
}

/// Encode a frame for the channel's bulk OUT endpoint. The adapter is
/// asked to report the frame back once it has been sent, with `token`.
pub fn encode_tx(channel: u8, frame: &Frame, token: u32) -> Vec<u8> {
    let len = frame.data().len();
    let size = (TX_MSG_HEADER + len + 3) & !3;
    let mut flags = FLAG_LOOPED_BACK;
    let id = if frame.is_extended() {
        flags |= FLAG_EXT_ID;
        frame.can_id() & CAN_EFF_MASK
    } else {
        frame.can_id() & CAN_SFF_MASK
    };
    let dlc = if frame.is_fd() {
        flags |= FLAG_EXT_DATA_LEN;
        if frame.flags() & CANFD_BRS != 0 {
            flags |= FLAG_BITRATE_SWITCH;
        }
        if frame.flags() & CANFD_ESI != 0 {
            flags |= FLAG_ERROR_STATE_IND;
        }
        fd_len_to_dlc(len)
    } else {
        if frame.is_remote() {
            flags |= FLAG_RTR;
        }
        // For a remote frame this is the requested length.
        frame.len() as u8
    };

    // The record, padded to 32 bits, then a zero-size record as terminator.
    let mut out = vec![0u8; size + 4];
    out[0..2].copy_from_slice(&(size as u16).to_le_bytes());
    out[2..4].copy_from_slice(&MSG_CAN_TX.to_le_bytes());
    // Bytes 4..12: tag, returned with the report. The high half is unused.
    out[4..8].copy_from_slice(&token.to_le_bytes());
    out[12] = (channel & 0x0F) | (dlc << 4);
    // Byte 13: client, unused.
    out[14..16].copy_from_slice(&flags.to_le_bytes());
    out[16..20].copy_from_slice(&id.to_le_bytes());
    out[TX_MSG_HEADER..TX_MSG_HEADER + len].copy_from_slice(frame.data());
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusState {
    Active,
    Warning,
    Passive,
    BusOff,
}

/// One record from the bulk IN endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Frame {
        channel: u8,
        frame: Frame,
        /// Device clock, microseconds.
        timestamp_us: u64,
    },
    /// The adapter sent this frame of ours and it was acknowledged.
    Sent {
        channel: u8,
        frame: Frame,
        /// The token given to [`encode_tx`].
        token: u32,
        timestamp_us: u64,
    },
    Status {
        channel: u8,
        state: BusState,
        timestamp_us: u64,
    },
    ErrorCounters {
        channel: u8,
        tx: u8,
        rx: u8,
    },
    Overrun {
        channel: u8,
        timestamp_us: u64,
    },
    /// Periodic time calibration message. Its counter starts when the
    /// messages are switched on, so it is not on the clock that stamps
    /// frames (seen on a PCAN-USB X6, firmware 3.2.0: 4 s here against
    /// 5667 s in the frames of the same transfer).
    Calibration {
        timestamp_us: u64,
    },
}

impl Event {
    /// The record's time on the clock that stamps received frames, if it
    /// carries one. This is what the device clock is tracked from.
    pub fn frame_clock_us(&self) -> Option<u64> {
        match self {
            Event::Frame { timestamp_us, .. }
            | Event::Sent { timestamp_us, .. }
            | Event::Status { timestamp_us, .. }
            | Event::Overrun { timestamp_us, .. } => Some(*timestamp_us),
            Event::Calibration { .. } | Event::ErrorCounters { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Decode every record in one bulk IN transfer. Records decoded before a
/// malformed one are still delivered.
pub fn decode(buf: &[u8], mut on_event: impl FnMut(Event)) -> Result<(), Malformed> {
    let mut rest = buf;
    while rest.len() >= 2 {
        let size = usize::from(le16(rest, 0));
        if size == 0 {
            // Zero-size record: end of list.
            break;
        }
        if size < 12 || size > rest.len() {
            return Err(Malformed);
        }
        let rec = &rest[..size];
        let ty = le16(rec, 2);
        let timestamp_us = u64::from(le32(rec, 4)) | (u64::from(le32(rec, 8)) << 32);
        match ty {
            MSG_CAN_RX => {
                if size < RX_MSG_HEADER {
                    return Err(Malformed);
                }
                let channel = rec[20] & 0x0F;
                let dlc = rec[20] >> 4;
                let flags = le16(rec, 22);
                let mut can_id = le32(rec, 24);
                let mut fd_flags = 0;
                let len = if flags & FLAG_EXT_DATA_LEN != 0 {
                    fd_flags = CANFD_FDF;
                    if flags & FLAG_BITRATE_SWITCH != 0 {
                        fd_flags |= CANFD_BRS;
                    }
                    if flags & FLAG_ERROR_STATE_IND != 0 {
                        fd_flags |= CANFD_ESI;
                    }
                    usize::from(FD_LENGTHS[usize::from(dlc)])
                } else {
                    usize::from(dlc.min(8))
                };
                if flags & FLAG_EXT_ID != 0 {
                    can_id = (can_id & CAN_EFF_MASK) | CAN_EFF_FLAG;
                } else {
                    can_id &= CAN_SFF_MASK;
                }
                let frame = if flags & FLAG_RTR != 0 && fd_flags == 0 {
                    Frame::from_raw(can_id | CAN_RTR_FLAG, 0, &[0u8; 8][..len])
                } else {
                    if size - RX_MSG_HEADER < len {
                        return Err(Malformed);
                    }
                    Frame::from_raw(can_id, fd_flags, &rec[RX_MSG_HEADER..RX_MSG_HEADER + len])
                };
                let frame = frame.map_err(|_| Malformed)?;
                on_event(if flags & FLAG_LOOPED_BACK != 0 {
                    Event::Sent {
                        channel,
                        frame,
                        token: le32(rec, 12),
                        timestamp_us,
                    }
                } else {
                    Event::Frame {
                        channel,
                        frame,
                        timestamp_us,
                    }
                });
            }
            MSG_STATUS => {
                if size < 16 {
                    return Err(Malformed);
                }
                let s = rec[12];
                let state = if s & STATUS_BUSOFF != 0 {
                    BusState::BusOff
                } else if s & STATUS_PASSIVE != 0 {
                    BusState::Passive
                } else if s & STATUS_WARNING != 0 {
                    BusState::Warning
                } else {
                    BusState::Active
                };
                on_event(Event::Status {
                    channel: s & 0x0F,
                    state,
                    timestamp_us,
                });
            }
            MSG_ERROR => {
                if size < 16 {
                    return Err(Malformed);
                }
                on_event(Event::ErrorCounters {
                    channel: rec[12] & 0x0F,
                    tx: rec[14],
                    rx: rec[15],
                });
            }
            MSG_OVERRUN => {
                if size < 16 {
                    return Err(Malformed);
                }
                on_event(Event::Overrun {
                    channel: rec[12] & 0x0F,
                    timestamp_us,
                });
            }
            MSG_CALIBRATION => on_event(Event::Calibration { timestamp_us }),
            _ => {} // Unknown record types are skipped.
        }
        rest = &rest[size..];
    }
    Ok(())
}

/// The error frame announcing a bus state change, in SocketCAN's format.
/// `tx_errors` and `rx_errors` are the last counters the device reported;
/// the larger one tells which direction caused the state.
pub fn state_error_frame(state: BusState, tx_errors: u8, rx_errors: u8) -> Frame {
    let mut detail = [0u8; 8];
    detail[6] = tx_errors;
    detail[7] = rx_errors;
    let tx = tx_errors >= rx_errors;
    let rx = rx_errors >= tx_errors;
    let pick =
        |tx_bit: u8, rx_bit: u8| (if tx { tx_bit } else { 0 }) | (if rx { rx_bit } else { 0 });
    let class = match state {
        BusState::BusOff => err::BUSOFF,
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
    Frame::new_error(class | err::CNT, detail)
}

pub fn overrun_error_frame() -> Frame {
    Frame::new_error(err::CRTL, [0, err::CRTL_RX_OVERFLOW, 0, 0, 0, 0, 0, 0])
}

/// Firmware information returned by the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FwInfo {
    pub hw_version: u8,
    pub fw_version: [u8; 3],
    pub serial: u32,
    pub cmd_out_ep: u8,
    pub data_in_ep: u8,
    pub data_out_ep: [u8; 2],
}

pub const FW_INFO_LEN: usize = 36;

// Endpoints of devices whose firmware info does not list them.
const DEFAULT_CMD_OUT_EP: u8 = 0x01;
const DEFAULT_DATA_IN_EP: u8 = 0x82;
const DEFAULT_DATA_OUT_EP: [u8; 2] = [0x02, 0x03];

impl FwInfo {
    pub fn parse(b: &[u8]) -> Option<FwInfo> {
        // The fixed part is 28 bytes; endpoint numbers follow when the
        // structure type is 2 or higher.
        if b.len() < 28 {
            return None;
        }
        let ty = le16(b, 2);
        let extended = ty >= 2 && b.len() >= 33;
        Some(FwInfo {
            hw_version: b[8],
            fw_version: [b[9], b[10], b[11]],
            serial: le32(b, 20),
            cmd_out_ep: if extended { b[28] } else { DEFAULT_CMD_OUT_EP },
            data_out_ep: if extended {
                [b[30], b[31]]
            } else {
                DEFAULT_DATA_OUT_EP
            },
            data_in_ep: if extended { b[32] } else { DEFAULT_DATA_IN_EP },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use darwincan_core::Id;

    fn timing(brp: u32, tseg1: u32, tseg2: u32, sjw: u32) -> BitTiming {
        BitTiming {
            brp,
            tseg1,
            tseg2,
            sjw,
            bitrate: 0,
            sample_point: 0,
        }
    }

    #[test]
    fn commands_are_eight_byte_records_with_terminator() {
        let packet = Commands::new(1)
            .timing_slow(&timing(10, 13, 2, 1))
            .timing_fast(&timing(2, 15, 4, 4))
            .reset_error_counters()
            .iso_fd()
            .operational(false)
            .finish();
        assert_eq!(
            packet,
            [
                // TIMING_SLOW on channel 1: ewl 96, sjw-1, tseg2-1, tseg1-1, brp-1
                0x04, 0x10, 96, 0, 1, 12, 9, 0, //
                // TIMING_FAST
                0x05, 0x10, 0, 3, 3, 14, 1, 0, //
                // WR_ERR_CNT selecting both counters, values 0
                0x0A, 0x10, 0x00, 0xC0, 0, 0, 0, 0, //
                // SET_EN_OPTION: ISO CAN FD
                0x0B, 0x10, 0x04, 0x00, 0, 0, 0, 0, //
                // NORMAL_MODE
                0x02, 0x10, 0, 0, 0, 0, 0, 0, //
                // end of collection
                0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            ]
        );

        let p = Commands::new(0).operational(true).finish();
        assert_eq!(&p[..2], &[0x03, 0x00]);
        let p = Commands::new(0).reset_mode().finish();
        assert_eq!(&p[..2], &[0x01, 0x00]);
        let p = Commands::new(0).clock(CLOCK_80MHZ).led(LED_OFF).finish();
        assert_eq!(
            &p[..16],
            &[0x80, 0, 0, 0, 0, 0, 0, 0, 0x86, 0, 4, 0, 0, 0, 0, 0]
        );
        let p = Commands::new(0)
            .error_and_calibration_messages(true)
            .finish();
        assert_eq!(&p[..8], &[0x0B, 0, 0x01, 0x00, 0, 0, 0x00, 0x80]);
        let p = Commands::new(0)
            .error_and_calibration_messages(false)
            .finish();
        assert_eq!(&p[..2], &[0x0C, 0]);
    }

    #[test]
    fn filter_table_fills_the_buffer_without_terminator() {
        let packet = Commands::new(0).accept_all_standard_ids().finish();
        assert_eq!(packet.len(), 512);
        assert_eq!(&packet[..8], &[0x08, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(&packet[504..], &[0x08, 0, 63, 0, 0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn wide_timing_values_use_their_full_fields() {
        // 500 kbit/s at 80 MHz with the finest quantum: tseg1 = 139.
        let p = Commands::new(0)
            .timing_slow(&timing(1, 139, 20, 10))
            .finish();
        assert_eq!(&p[..8], &[0x04, 0, 96, 9, 19, 138, 0, 0]);
        let p = Commands::new(0)
            .timing_slow(&timing(1024, 256, 128, 128))
            .finish();
        assert_eq!(&p[..8], &[0x04, 0, 96, 127, 127, 255, 0xFF, 0x03]);
    }

    #[test]
    fn encodes_classic_frames() {
        let f = Frame::new(Id::Standard(0x123), &[0xDE, 0xAD, 0xBE]).unwrap();
        assert_eq!(
            encode_tx(0, &f, 0x1122_3344),
            [
                24, 0, // size: 20 + 3, padded to 24
                0x00, 0x10, // CAN_TX
                0x44, 0x33, 0x22, 0x11, 0, 0, 0, 0,    // tag: the token
                0x30, // channel 0, dlc 3
                0,    // client
                0x04, 0, // flags: report back when sent
                0x23, 0x01, 0, 0, // id
                0xDE, 0xAD, 0xBE, 0, // data + padding
                0, 0, 0, 0, // terminator
            ]
        );

        let f = Frame::new(Id::Extended(0x18DA_F110), &[]).unwrap();
        let out = encode_tx(1, &f, 0);
        assert_eq!(out.len(), 24);
        assert_eq!(out[0], 20);
        assert_eq!(out[12], 0x01);
        assert_eq!(&out[14..20], &[0x06, 0, 0x10, 0xF1, 0xDA, 0x18]);

        let f = Frame::new_remote(Id::Standard(0x7FF), 4).unwrap();
        let out = encode_tx(0, &f, 0);
        assert_eq!(out[0], 20);
        assert_eq!(out[12], 0x40);
        assert_eq!(&out[14..16], &[0x05, 0]);
    }

    #[test]
    fn encodes_fd_frames() {
        let f = Frame::new_fd(Id::Standard(0x100), &[0x55; 48], true).unwrap();
        let out = encode_tx(0, &f, 0);
        assert_eq!(out.len(), 68 + 4);
        assert_eq!(&out[0..2], &[68, 0]);
        assert_eq!(out[12], 0xE0); // dlc 14
        assert_eq!(&out[14..16], &[0x34, 0]); // FD + BRS + report back
        assert_eq!(&out[20..68], &[0x55; 48]);
        assert_eq!(&out[68..], &[0, 0, 0, 0]);

        let f = Frame::new_fd(Id::Standard(0x100), &[1, 2], false).unwrap();
        let out = encode_tx(0, &f, 0);
        assert_eq!(&out[14..16], &[0x14, 0]);
        assert_eq!(out[12], 0x20);
    }

    /// Build an RX record the way the device does.
    fn rx_record(channel: u8, dlc: u8, flags: u16, id: u32, data: &[u8], ts: u64) -> Vec<u8> {
        let size = (RX_MSG_HEADER + data.len() + 3) & !3;
        let mut r = vec![0u8; size];
        r[0..2].copy_from_slice(&(size as u16).to_le_bytes());
        r[2..4].copy_from_slice(&MSG_CAN_RX.to_le_bytes());
        r[4..8].copy_from_slice(&(ts as u32).to_le_bytes());
        r[8..12].copy_from_slice(&((ts >> 32) as u32).to_le_bytes());
        r[20] = channel | (dlc << 4);
        r[22..24].copy_from_slice(&flags.to_le_bytes());
        r[24..28].copy_from_slice(&id.to_le_bytes());
        r[28..28 + data.len()].copy_from_slice(data);
        r
    }

    fn short_record(ty: u16, ts: u64, tail: [u8; 4]) -> Vec<u8> {
        let mut r = vec![16, 0];
        r.extend_from_slice(&ty.to_le_bytes());
        r.extend_from_slice(&(ts as u32).to_le_bytes());
        r.extend_from_slice(&((ts >> 32) as u32).to_le_bytes());
        r.extend_from_slice(&tail);
        r
    }

    #[test]
    fn a_looped_back_record_is_a_report_of_our_own_frame() {
        // As captured from a PCAN-USB X6: the frame, the flag and the tag
        // of the transmit record come back with the adapter's timestamp.
        let mut record = rx_record(1, 2, 0x04, 0x4A1, &[0x11, 0x22], 7_191_896);
        record[12..16].copy_from_slice(&0x1122_3344u32.to_le_bytes());
        record[21] = 0x5A; // client byte, unused by us
        let (events, result) = decode_all(&record);
        assert_eq!(result, Ok(()));
        assert_eq!(
            events,
            vec![Event::Sent {
                channel: 1,
                frame: Frame::new(Id::Standard(0x4A1), &[0x11, 0x22]).unwrap(),
                token: 0x1122_3344,
                timestamp_us: 7_191_896,
            }]
        );
        assert_eq!(events[0].frame_clock_us(), Some(7_191_896));
    }

    #[test]
    fn flushing_the_transmit_queue_is_a_tx_abort_with_the_flush_flag() {
        let p = Commands::new(1).flush_transmit_queue().finish();
        assert_eq!(&p[..8], &[0x09, 0x10, 0x01, 0, 0, 0, 0, 0]);
    }

    fn decode_all(buf: &[u8]) -> (Vec<Event>, Result<(), Malformed>) {
        let mut events = Vec::new();
        let result = decode(buf, |e| events.push(e));
        (events, result)
    }

    #[test]
    fn decodes_a_mixed_transfer() {
        let ts = 0x1_2345_6789u64;
        let mut buf = Vec::new();
        buf.extend(rx_record(0, 3, 0, 0x123, &[1, 2, 3], ts));
        buf.extend(rx_record(1, 15, 0x32, 0x18DA_F110, &[7; 64], ts + 1));
        buf.extend(rx_record(0, 4, 0x01, 0x7FF, &[], ts + 2));
        buf.extend(short_record(MSG_STATUS, ts + 3, [0x41, 0, 0, 0]));
        buf.extend(short_record(MSG_ERROR, ts + 4, [0x00, 0, 130, 5]));
        buf.extend(short_record(MSG_OVERRUN, ts + 5, [0x01, 0, 0, 0]));
        buf.extend(short_record(MSG_CALIBRATION, ts + 6, [9, 0, 0, 0]));
        buf.extend(short_record(0x7777, ts, [0; 4])); // unknown: skipped
        buf.extend([0, 0, 0, 0]); // terminator
        buf.extend([0xEE; 12]); // stale bytes after the terminator

        let (events, result) = decode_all(&buf);
        assert_eq!(result, Ok(()));
        assert_eq!(
            events,
            vec![
                Event::Frame {
                    channel: 0,
                    frame: Frame::new(Id::Standard(0x123), &[1, 2, 3]).unwrap(),
                    timestamp_us: ts,
                },
                Event::Frame {
                    channel: 1,
                    frame: Frame::new_fd(Id::Extended(0x18DA_F110), &[7; 64], true).unwrap(),
                    timestamp_us: ts + 1,
                },
                Event::Frame {
                    channel: 0,
                    frame: Frame::new_remote(Id::Standard(0x7FF), 4).unwrap(),
                    timestamp_us: ts + 2,
                },
                Event::Status {
                    channel: 1,
                    state: BusState::Warning,
                    timestamp_us: ts + 3,
                },
                Event::ErrorCounters {
                    channel: 0,
                    tx: 130,
                    rx: 5,
                },
                Event::Overrun {
                    channel: 1,
                    timestamp_us: ts + 5,
                },
                Event::Calibration {
                    timestamp_us: ts + 6,
                },
            ]
        );
        // Everything stamped by the frame clock gives its time; the
        // calibration counter, which runs from a different start, does not.
        let clock: Vec<Option<u64>> = events.iter().map(Event::frame_clock_us).collect();
        assert_eq!(
            clock,
            [
                Some(ts),
                Some(ts + 1),
                Some(ts + 2),
                Some(ts + 3),
                None,
                Some(ts + 5),
                None
            ]
        );
    }

    #[test]
    fn status_priority_and_esi() {
        let (e, _) = decode_all(&short_record(MSG_STATUS, 0, [0xE0, 0, 0, 0]));
        assert!(matches!(
            e[0],
            Event::Status {
                state: BusState::BusOff,
                ..
            }
        ));
        let (e, _) = decode_all(&short_record(MSG_STATUS, 0, [0x60, 0, 0, 0]));
        assert!(matches!(
            e[0],
            Event::Status {
                state: BusState::Passive,
                ..
            }
        ));
        let (e, _) = decode_all(&short_record(MSG_STATUS, 0, [0x00, 0, 0, 0]));
        assert!(matches!(
            e[0],
            Event::Status {
                state: BusState::Active,
                ..
            }
        ));

        let (e, _) = decode_all(&rx_record(0, 1, 0x50, 0x1, &[9], 0));
        let Event::Frame { frame, .. } = &e[0] else {
            panic!()
        };
        assert_eq!(frame.flags(), CANFD_FDF | CANFD_ESI);
    }

    #[test]
    fn malformed_transfers_are_rejected_after_the_good_part() {
        let good = rx_record(0, 1, 0, 0x1, &[9], 0);

        // Record claims to extend past the transfer.
        let mut buf = good.clone();
        buf.extend([200, 0, 1, 0]);
        let (events, result) = decode_all(&buf);
        assert_eq!(events.len(), 1);
        assert_eq!(result, Err(Malformed));

        // Record shorter than any header.
        assert_eq!(decode_all(&[4, 0, 1, 0]).1, Err(Malformed));

        // CAN record whose data is cut short.
        let mut cut = rx_record(0, 8, 0, 0x1, &[0; 8], 0);
        cut.truncate(32);
        cut[0] = 32;
        assert_eq!(decode_all(&cut).1, Err(Malformed));

        // Empty and terminator-only transfers are fine.
        assert_eq!(decode_all(&[]), (vec![], Ok(())));
        assert_eq!(decode_all(&[0, 0, 0, 0]), (vec![], Ok(())));
    }

    #[test]
    fn state_changes_become_socketcan_error_frames() {
        let f = state_error_frame(BusState::Passive, 130, 5);
        assert!(f.is_error());
        assert_eq!(f.can_id() & err::MASK, err::CRTL | err::CNT);
        assert_eq!(f.data(), &[0, err::CRTL_TX_PASSIVE, 0, 0, 0, 0, 130, 5]);

        let f = state_error_frame(BusState::Warning, 3, 100);
        assert_eq!(f.data()[1], err::CRTL_RX_WARNING);

        let f = state_error_frame(BusState::Active, 0, 0);
        assert_eq!(f.data()[1], err::CRTL_ACTIVE);

        let f = state_error_frame(BusState::BusOff, 255, 0);
        assert_eq!(f.can_id() & err::MASK, err::BUSOFF | err::CNT);

        let f = overrun_error_frame();
        assert_eq!(f.can_id() & err::MASK, err::CRTL);
        assert_eq!(f.data()[1], err::CRTL_RX_OVERFLOW);
    }

    #[test]
    fn firmware_info() {
        let mut b = vec![0u8; FW_INFO_LEN];
        b[0] = 36;
        b[2] = 2; // extended: endpoints included
        b[8] = 4; // hw version
        b[9..12].copy_from_slice(&[3, 4, 1]);
        b[20..24].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        b[28] = 0x01;
        b[29] = 0x81;
        b[30] = 0x02;
        b[31] = 0x03;
        b[32] = 0x82;
        let info = FwInfo::parse(&b).unwrap();
        assert_eq!(info.fw_version, [3, 4, 1]);
        assert_eq!(info.hw_version, 4);
        assert_eq!(info.serial, 0xDEAD_BEEF);
        assert_eq!(
            (info.cmd_out_ep, info.data_in_ep, info.data_out_ep),
            (0x01, 0x82, [0x02, 0x03])
        );

        // Older structure without endpoints: defaults apply.
        b[2] = 1;
        b[28..33].fill(0x55);
        let info = FwInfo::parse(&b).unwrap();
        assert_eq!(
            (info.cmd_out_ep, info.data_in_ep, info.data_out_ep),
            (0x01, 0x82, [0x02, 0x03])
        );

        assert!(FwInfo::parse(&b[..20]).is_none());
    }
}
