// SPDX-License-Identifier: GPL-2.0-only
//
// Wire format of the classic PEAK-System PCAN-USB adapter. Derived from the
// Linux kernel driver (drivers/net/can/usb/peak_usb/pcan_usb.c and
// pcan_usb_core.c),
// Copyright (C) 2003-2025 PEAK System-Technik GmbH,
// Author: Stéphane Grosjean.

//! Encoding and decoding of PCAN-USB commands and messages. Pure functions,
//! no I/O.

use darwincan_core::{Frame, CAN_EFF_FLAG, CAN_EFF_MASK, CAN_RTR_FLAG, CAN_SFF_MASK};

use crate::bittiming::BitTiming;

/// Every command, and every response to one, is this long.
pub const CMD_LEN: usize = 16;
/// Message packets in both directions are at most this long; transmit
/// packets are always sent at full length.
pub const MSG_LEN: usize = 64;

// Command functions.
const CMD_BITRATE: u8 = 1;
const CMD_SET_BUS: u8 = 3;
const CMD_SN: u8 = 6;
const CMD_REGISTER: u8 = 9;
const CMD_EXT_VCC: u8 = 10;
const CMD_ERR_FR: u8 = 11;

// Command numbers.
const GET: u8 = 1;
const SET: u8 = 2;
const BUS_XCVER: u8 = 2;
const BUS_SILENT_MODE: u8 = 3;

const SJA1000_MODE_INIT: u8 = 0x01;

/// Ask for the receive and transmit error counters whenever they change.
const ERR_MASK_COUNTERS: u8 = 0x02 | 0x04;

const MSG_HEADER_LEN: usize = 2;
const MSG_TX_CAN: u8 = 2;

// The status/length byte that starts every record.
const SL_TIMESTAMP: u8 = 1 << 7;
const SL_INTERNAL: u8 = 1 << 6;
const SL_EXT_ID: u8 = 1 << 5;
const SL_RTR: u8 = 1 << 4;
const SL_DLC: u8 = 0x0F;

/// Set in a received identifier word when a private byte follows the data.
const ID_SRR: u32 = 0x01;

// Internal record functions.
const REC_ERROR: u8 = 1;
const REC_ANALOG: u8 = 2;
const REC_BUSLOAD: u8 = 3;
const REC_TS: u8 = 4;
const REC_BUSEVT: u8 = 5;

// Flags of an error record.
pub const ERROR_RXQOVR: u8 = 0x02;
pub const ERROR_BUS_HEAVY: u8 = 0x08;
pub const ERROR_BUS_OFF: u8 = 0x10;

// Bus event records that carry the error counters.
const ERR_CNT_DEC: u8 = 0x00;
const ERR_CNT_INC: u8 = 0x80;

fn command(function: u8, number: u8, args: &[u8]) -> [u8; CMD_LEN] {
    let mut cmd = [0u8; CMD_LEN];
    cmd[0] = function;
    cmd[1] = number;
    cmd[2..2 + args.len()].copy_from_slice(args);
    cmd
}

/// Program the SJA1000 bus timing registers.
pub fn set_bittiming(t: &BitTiming) -> [u8; CMD_LEN] {
    let btr0 = ((t.brp - 1) as u8 & 0x3F) | (((t.sjw - 1) as u8 & 0x03) << 6);
    let btr1 = ((t.tseg1 - 1) as u8 & 0x0F) | (((t.tseg2 - 1) as u8 & 0x07) << 4);
    command(CMD_BITRATE, SET, &[btr1, btr0])
}

/// Switch the transceiver on (go on the bus) or off.
pub fn set_bus(on: bool) -> [u8; CMD_LEN] {
    command(CMD_SET_BUS, BUS_XCVER, &[u8::from(on)])
}

/// Listen-only mode. Only hardware revisions above 3 understand it.
pub fn set_silent(on: bool) -> [u8; CMD_LEN] {
    command(CMD_SET_BUS, BUS_SILENT_MODE, &[u8::from(on)])
}

/// Put the CAN controller into its reset (initialisation) mode.
pub fn set_controller_init() -> [u8; CMD_LEN] {
    command(CMD_REGISTER, SET, &[0, SJA1000_MODE_INIT])
}

/// Switch the 5 V supply on the CAN connector on or off.
pub fn set_ext_vcc(on: bool) -> [u8; CMD_LEN] {
    command(CMD_EXT_VCC, SET, &[u8::from(on)])
}

/// Have the adapter report its error counters when they change.
pub fn report_error_counters() -> [u8; CMD_LEN] {
    command(CMD_ERR_FR, SET, &[ERR_MASK_COUNTERS])
}

/// Ask for the serial number; the answer comes on the command IN endpoint.
pub fn get_serial() -> [u8; CMD_LEN] {
    command(CMD_SN, GET, &[])
}

/// The serial number in the response to [`get_serial`].
pub fn parse_serial(response: &[u8]) -> Option<u32> {
    let b = response.get(2..6)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Encode a classic frame as one transmit packet. `counter` goes in the
/// last byte; the reference driver numbers the packets it sends there.
///
/// With `report` the adapter is asked to send the frame back once it has
/// been transmitted (the controller's self-reception request, which the
/// reference driver uses for its loopback mode, on hardware revision 41
/// and later). The byte given follows the data and comes back with it.
pub fn encode_tx(frame: &Frame, counter: u8, report: Option<u8>) -> [u8; MSG_LEN] {
    let mut out = [0u8; MSG_LEN];
    out[0] = MSG_TX_CAN;
    out[1] = 1; // one frame per packet

    // For a remote frame the length is the requested length.
    let mut status_len = frame.len() as u8 & SL_DLC;
    if frame.is_remote() {
        status_len |= SL_RTR;
    }
    let mut at = MSG_HEADER_LEN + 1;
    let srr = if report.is_some() { ID_SRR } else { 0 };
    if frame.is_extended() {
        status_len |= SL_EXT_ID;
        let id = (frame.can_id() & CAN_EFF_MASK) << 3 | srr;
        out[at..at + 4].copy_from_slice(&id.to_le_bytes());
        at += 4;
    } else {
        let id = ((frame.can_id() & CAN_SFF_MASK) << 5 | srr) as u16;
        out[at..at + 2].copy_from_slice(&id.to_le_bytes());
        at += 2;
    }
    out[MSG_HEADER_LEN] = status_len;
    if !frame.is_remote() {
        let data = frame.data();
        out[at..at + data.len()].copy_from_slice(data);
        at += data.len();
    }
    if let Some(byte) = report {
        out[at] = byte;
    }
    out[MSG_LEN - 1] = counter;
    out
}

/// One record from the message IN endpoint. Timestamps are the low 16 bits
/// of the adapter's tick counter (see [`TickClock`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Frame {
        frame: Frame,
        ticks: u16,
    },
    /// The adapter sent this frame of ours and it was acknowledged. `id`
    /// is the byte given to [`encode_tx`]; a remote frame comes back
    /// without it.
    Sent {
        frame: Frame,
        ticks: u16,
        id: Option<u8>,
    },
    /// Controller status: a combination of the `ERROR_*` flags.
    Error {
        flags: u8,
        ticks: Option<u16>,
    },
    /// The controller's error counters changed.
    ErrorCounters {
        rx: u8,
        tx: u8,
    },
    /// Periodic announcement of the adapter's clock.
    Sync {
        ticks: u16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

/// Reads a packet front to back.
struct Reader<'a> {
    buf: &'a [u8],
    /// Timestamp of the last record that had one. The first such record in
    /// a packet carries 16 bits, the following ones only the low 8.
    ticks: Option<u16>,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        if self.buf.len() < n {
            return Err(Malformed);
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    fn timestamp(&mut self) -> Result<u16, Malformed> {
        let ticks = match self.ticks {
            None => {
                let b = self.take(2)?;
                u16::from_le_bytes([b[0], b[1]])
            }
            Some(prev) => {
                let low = self.take(1)?[0];
                // The low byte went round since the previous record.
                let high = if low < prev as u8 {
                    prev.wrapping_add(0x100)
                } else {
                    prev
                };
                (high & 0xFF00) | u16::from(low)
            }
        };
        self.ticks = Some(ticks);
        Ok(ticks)
    }
}

/// Decode every record in one packet from the message IN endpoint. Records
/// decoded before a malformed one are still delivered.
pub fn decode(buf: &[u8], mut on_event: impl FnMut(Event)) -> Result<(), Malformed> {
    if buf.is_empty() {
        return Ok(());
    }
    if buf.len() <= MSG_HEADER_LEN {
        return Err(Malformed);
    }
    let records = buf[1];
    let mut r = Reader {
        buf: &buf[MSG_HEADER_LEN..],
        ticks: None,
    };
    for _ in 0..records {
        let status_len = r.take(1)?[0];
        let rec_len = usize::from(status_len & SL_DLC);
        if status_len & SL_INTERNAL != 0 {
            let head = r.take(2)?;
            let (function, number) = (head[0], head[1]);
            let ticks = if status_len & SL_TIMESTAMP != 0 {
                Some(r.timestamp()?)
            } else {
                None
            };
            let body_len = match function {
                REC_ANALOG => 2,
                REC_BUSLOAD => 1,
                _ => rec_len,
            };
            let body = r.take(body_len)?;
            match function {
                REC_ERROR => on_event(Event::Error {
                    flags: number,
                    ticks,
                }),
                REC_TS => {
                    if body.len() < 2 {
                        return Err(Malformed);
                    }
                    on_event(Event::Sync {
                        ticks: u16::from_le_bytes([body[0], body[1]]),
                    });
                }
                REC_BUSEVT if number == ERR_CNT_DEC || number == ERR_CNT_INC => {
                    if body.len() < 3 {
                        return Err(Malformed);
                    }
                    on_event(Event::ErrorCounters {
                        rx: body[1],
                        tx: body[2],
                    });
                }
                _ => {} // Analog values, bus load and unknown records.
            }
        } else {
            let (can_id, id_word) = if status_len & SL_EXT_ID != 0 {
                let b = r.take(4)?;
                let word = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                (((word >> 3) & CAN_EFF_MASK) | CAN_EFF_FLAG, word)
            } else {
                let b = r.take(2)?;
                let word = u32::from(u16::from_le_bytes([b[0], b[1]]));
                ((word >> 5) & CAN_SFF_MASK, word)
            };
            let ticks = r.timestamp()?;
            // Classic CAN allows length codes 9 to 15; they mean 8 bytes.
            let len = rec_len.min(8);
            let sent = id_word & ID_SRR != 0;
            let mut id = None;
            let frame = if status_len & SL_RTR != 0 {
                Frame::from_raw(can_id | CAN_RTR_FLAG, 0, &[0u8; 8][..len])
            } else {
                let data = r.take(rec_len)?;
                if sent {
                    id = Some(r.take(1)?[0]);
                }
                Frame::from_raw(can_id, 0, &data[..len])
            };
            let frame = frame.map_err(|_| Malformed)?;
            on_event(if sent {
                Event::Sent { frame, ticks, id }
            } else {
                Event::Frame { frame, ticks }
            });
        }
    }
    Ok(())
}

/// One tick of the adapter's clock is 128/3 microseconds.
const TICK_NS_NUM: u64 = 128_000;
const TICK_NS_DEN: u64 = 3;
const WRAP: i64 = 1 << 16;

/// Extends the adapter's 16-bit tick counter, which goes round every 2.8
/// seconds, into a clock that does not.
///
/// The step from one timestamp to the next is known modulo 2^16 ticks; the
/// host time between their arrivals says how many whole turns to add when
/// the adapter was silent for longer than one.
#[derive(Debug, Default)]
pub struct TickClock {
    /// (16-bit ticks, extended ticks, host arrival time) of the last
    /// timestamp seen.
    last: Option<(u16, i64, u64)>,
}

impl TickClock {
    pub fn new() -> TickClock {
        TickClock::default()
    }

    /// The adapter's time in nanoseconds for a 16-bit timestamp that
    /// arrived at `host_now_ns`. Timestamps may be slightly out of order.
    pub fn extend(&mut self, ticks: u16, host_now_ns: u64) -> u64 {
        let extended = match self.last {
            // Start one turn in so an earlier timestamp stays positive.
            None => WRAP + i64::from(ticks),
            Some((last_ticks, last_extended, last_host)) => {
                let step = i64::from(ticks.wrapping_sub(last_ticks) as i16);
                let host_ns = host_now_ns.saturating_sub(last_host);
                let expected = (u128::from(host_ns) * u128::from(TICK_NS_DEN)
                    / u128::from(TICK_NS_NUM)) as i64;
                let turns = (expected - step + WRAP / 2).div_euclid(WRAP);
                (last_extended + step + turns * WRAP).max(0)
            }
        };
        self.last = Some((ticks, extended, host_now_ns));
        (extended as u128 * u128::from(TICK_NS_NUM) / u128::from(TICK_NS_DEN)) as u64
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

    fn args(cmd: &[u8; CMD_LEN]) -> (u8, u8, &[u8]) {
        (cmd[0], cmd[1], &cmd[2..])
    }

    #[test]
    fn commands_are_function_number_and_fourteen_argument_bytes() {
        let mut zero = [0u8; 14];
        assert_eq!(args(&set_bus(false)), (3, 2, &zero[..]));
        assert_eq!(args(&get_serial()), (6, 1, &zero[..]));
        assert_eq!(args(&set_ext_vcc(false)), (10, 2, &zero[..]));
        zero[0] = 1;
        assert_eq!(args(&set_bus(true)), (3, 2, &zero[..]));
        assert_eq!(args(&set_silent(true)), (3, 3, &zero[..]));
        zero[0] = 0x06;
        assert_eq!(args(&report_error_counters()), (11, 2, &zero[..]));
        // The controller mode is the second argument.
        let init = set_controller_init();
        assert_eq!(&init[..4], &[9, 2, 0, 1]);
        assert!(init[4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn bit_timing_is_sent_as_sja1000_registers() {
        // 500 kbit/s from 8 MHz: 16 quanta, sample after 14. These are the
        // register values PEAK documents for 500 kbit/s (BTR0 00, BTR1 1C).
        let cmd = set_bittiming(&timing(1, 13, 2, 1));
        assert_eq!(&cmd[..4], &[1, 2, 0x1C, 0x00]);
        // Every field at its maximum.
        let cmd = set_bittiming(&timing(64, 16, 8, 4));
        assert_eq!(&cmd[..4], &[1, 2, 0x7F, 0xFF]);
        // 125 kbit/s: prescaler 4, BTR0 03.
        let cmd = set_bittiming(&timing(4, 13, 2, 1));
        assert_eq!(&cmd[..4], &[1, 2, 0x1C, 0x03]);
    }

    #[test]
    fn serial_number_is_little_endian_after_the_header() {
        let mut response = [0u8; CMD_LEN];
        response[2..6].copy_from_slice(&[0x78, 0x56, 0x34, 0x12]);
        assert_eq!(parse_serial(&response), Some(0x1234_5678));
        assert_eq!(parse_serial(&response[..5]), None);
    }

    #[test]
    fn encodes_standard_extended_and_remote_frames() {
        let f = Frame::new(Id::Standard(0x123), &[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
        let p = encode_tx(&f, 7, None);
        // 0x123 << 5 = 0x2460.
        assert_eq!(&p[..9], &[2, 1, 0x04, 0x60, 0x24, 0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(p[9..63].iter().all(|&b| b == 0));
        assert_eq!(p[63], 7);

        let f = Frame::new(Id::Extended(0x18DA_F110), &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        let p = encode_tx(&f, 0, None);
        // 0x18DAF110 << 3 = 0xC6D78880.
        assert_eq!(&p[..7], &[2, 1, 0x28, 0x80, 0x88, 0xD7, 0xC6]);
        assert_eq!(&p[7..15], &[1, 2, 3, 4, 5, 6, 7, 8]);

        let f = Frame::new_remote(Id::Standard(0x7FF), 3).unwrap();
        let p = encode_tx(&f, 0, None);
        assert_eq!(&p[..5], &[2, 1, 0x13, 0xE0, 0xFF]);
        assert!(p[5..].iter().all(|&b| b == 0));
    }

    fn decode_all(buf: &[u8]) -> (Vec<Event>, Result<(), Malformed>) {
        let mut events = Vec::new();
        let result = decode(buf, |e| events.push(e));
        (events, result)
    }

    #[test]
    fn decodes_frames_with_word_then_byte_timestamps() {
        let mut p = vec![2, 4];
        // Standard data frame, 2 bytes, 16-bit timestamp 0x12F0.
        p.extend_from_slice(&[0x82, 0x60, 0x24, 0xF0, 0x12, 0xAA, 0xBB]);
        // Extended data frame, 1 byte, 8-bit timestamp 0xF8.
        p.extend_from_slice(&[0xA1, 0x80, 0x88, 0xD7, 0xC6, 0xF8, 0x55]);
        // Standard remote frame, length 3; the low byte went round.
        p.extend_from_slice(&[0x93, 0xE0, 0xFF, 0x05]);
        // Standard data frame echoed with a private byte after the data.
        p.extend_from_slice(&[0x81, 0x61, 0x24, 0x06, 0x99, 0x80]);
        let (events, result) = decode_all(&p);
        assert_eq!(result, Ok(()));
        assert_eq!(
            events,
            vec![
                Event::Frame {
                    frame: Frame::new(Id::Standard(0x123), &[0xAA, 0xBB]).unwrap(),
                    ticks: 0x12F0,
                },
                Event::Frame {
                    frame: Frame::new(Id::Extended(0x18DA_F110), &[0x55]).unwrap(),
                    ticks: 0x12F8,
                },
                Event::Frame {
                    frame: Frame::new_remote(Id::Standard(0x7FF), 3).unwrap(),
                    ticks: 0x1305,
                },
                Event::Sent {
                    frame: Frame::new(Id::Standard(0x123), &[0x99]).unwrap(),
                    ticks: 0x1306,
                    id: Some(0x80),
                },
            ]
        );
    }

    #[test]
    fn a_report_is_asked_for_with_the_self_reception_bit_and_a_byte() {
        let f = Frame::new(Id::Standard(0x123), &[0xDE, 0xAD]).unwrap();
        let p = encode_tx(&f, 7, Some(0x85));
        // 0x123 << 5 = 0x2460, plus the bit; the byte follows the data.
        assert_eq!(&p[..8], &[2, 1, 0x02, 0x61, 0x24, 0xDE, 0xAD, 0x85]);
        assert_eq!(p[63], 7);

        let f = Frame::new(Id::Extended(0x18DA_F110), &[1]).unwrap();
        let p = encode_tx(&f, 0, Some(0x80));
        assert_eq!(&p[..9], &[2, 1, 0x21, 0x81, 0x88, 0xD7, 0xC6, 1, 0x80]);

        // A remote frame has no data; the byte follows the identifier.
        let f = Frame::new_remote(Id::Standard(0x7FF), 3).unwrap();
        let p = encode_tx(&f, 0, Some(0x80));
        assert_eq!(&p[..6], &[2, 1, 0x13, 0xE1, 0xFF, 0x80]);
    }

    #[test]
    fn decodes_internal_records() {
        let mut p = vec![2, 6];
        // Clock announcement: no record timestamp, two bytes of clock.
        p.extend_from_slice(&[0x42, 4, 0, 0x34, 0x12]);
        // Error counters: rx 5, tx 136.
        p.extend_from_slice(&[0x43, 5, 0x80, 0x00, 5, 136]);
        // Error status with a timestamp, then one without.
        p.extend_from_slice(&[0xC0, 1, 0x08, 0x40, 0x12]);
        p.extend_from_slice(&[0x40, 1, 0x12]);
        // Analog values and bus load are skipped whatever their length says.
        p.extend_from_slice(&[0x40, 2, 0, 0xEE, 0xEE]);
        p.extend_from_slice(&[0x40, 3, 0, 0xEE]);
        let (events, result) = decode_all(&p);
        assert_eq!(result, Ok(()));
        assert_eq!(
            events,
            vec![
                Event::Sync { ticks: 0x1234 },
                Event::ErrorCounters { rx: 5, tx: 136 },
                Event::Error {
                    flags: ERROR_BUS_HEAVY,
                    ticks: Some(0x1240),
                },
                Event::Error {
                    flags: ERROR_BUS_OFF | ERROR_RXQOVR,
                    ticks: None,
                },
            ]
        );
    }

    #[test]
    fn long_length_codes_carry_eight_bytes() {
        // Length code 9: nine bytes follow, eight are data.
        let p = [2, 1, 0x89, 0x60, 0x24, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        let (events, result) = decode_all(&p);
        assert_eq!(result, Ok(()));
        assert_eq!(
            events,
            vec![Event::Frame {
                frame: Frame::new(Id::Standard(0x123), &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap(),
                ticks: 0,
            }]
        );
    }

    #[test]
    fn malformed_packets_are_rejected_after_the_good_part() {
        assert_eq!(decode_all(&[]), (vec![], Ok(())));
        assert_eq!(decode_all(&[2]), (vec![], Err(Malformed)));
        assert_eq!(decode_all(&[2, 1]), (vec![], Err(Malformed)));
        // An idle packet: header and no records.
        assert_eq!(decode_all(&[2, 0, 0]), (vec![], Ok(())));
        // Two records announced, the second cut short.
        let p = [
            2, 2, 0x81, 0x60, 0x24, 0x00, 0x10, 0x77, 0x82, 0x60, 0x24, 0x01,
        ];
        let (events, result) = decode_all(&p);
        assert_eq!(result, Err(Malformed));
        assert_eq!(
            events,
            vec![Event::Frame {
                frame: Frame::new(Id::Standard(0x123), &[0x77]).unwrap(),
                ticks: 0x1000,
            }]
        );
        // A clock announcement too short to hold a clock.
        assert_eq!(decode_all(&[2, 1, 0x41, 4, 0, 0x34]).1, Err(Malformed));
    }

    const MS: u64 = 1_000_000;

    /// Whole ticks from one adapter time to a later one.
    fn ticks_between(from_ns: u64, to_ns: u64) -> u64 {
        ((to_ns - from_ns) * 3 + 64_000) / 128_000
    }

    #[test]
    fn tick_clock_follows_the_counter_round() {
        let mut clock = TickClock::new();
        let start = clock.extend(0xFFF0, 1000 * MS);
        // 0x20 ticks later the counter has gone round.
        let next = clock.extend(0x0010, 1002 * MS);
        assert_eq!(ticks_between(start, next), 0x20);
        // A timestamp from just before the previous one.
        let earlier = clock.extend(0x000C, 1002 * MS);
        assert_eq!(ticks_between(earlier, next), 4);
    }

    #[test]
    fn tick_clock_counts_turns_missed_while_silent() {
        let mut clock = TickClock::new();
        let start = clock.extend(100, 0);
        // Ten seconds of silence: 234375 ticks, three and a half turns.
        let ticks = 234_375u64;
        let later = clock.extend((100 + ticks) as u16, 10_000 * MS + 300_000);
        assert_eq!(ticks_between(start, later), ticks);
        // Exactly one tick is 128/3 microseconds.
        assert_eq!(later - start, ticks * 128_000 / 3);
    }

    #[test]
    fn tick_clock_never_goes_below_zero() {
        let mut clock = TickClock::new();
        let first = clock.extend(5, 0);
        let earlier = clock.extend(0xFFF0, 0);
        assert!(earlier < first);
        assert_eq!(ticks_between(earlier, first), 21);
    }
}
