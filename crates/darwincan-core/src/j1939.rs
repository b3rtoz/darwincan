// SPDX-License-Identifier: MIT OR Apache-2.0

//! SAE J1939 addressing, as `<linux/can/j1939.h>` has it: addresses, NAMEs
//! and parameter group numbers, the filter of a J1939 socket, and what such
//! a socket receives.

/// The highest address one controller application can have.
pub const MAX_UNICAST_ADDR: u8 = 0xFD;
/// The address of a controller application that has none (it could not
/// claim one, or has not yet).
pub const IDLE_ADDR: u8 = 0xFE;
/// No address; as a destination, everyone.
pub const NO_ADDR: u8 = 0xFF;
/// No NAME: addressing is by address alone.
pub const NO_NAME: u64 = 0;
/// Request for a parameter group.
pub const PGN_REQUEST: u32 = 0x0EA00;
/// Address Claimed, and Cannot Claim Address.
pub const PGN_ADDRESS_CLAIMED: u32 = 0x0EE00;
/// Commanded Address.
pub const PGN_ADDRESS_COMMANDED: u32 = 0x0FED8;
/// The highest parameter group that is sent to one destination (PDU1).
pub const PGN_PDU1_MAX: u32 = 0x3FF00;
pub const PGN_MAX: u32 = 0x3FFFF;
/// No parameter group: any.
pub const NO_PGN: u32 = 0x40000;
/// The most filters a socket takes.
pub const FILTER_MAX: usize = 512;
/// The longest message the transport protocol carries (TP.CM/TP.DT), and
/// the longest that can be sent to everyone.
pub const MAX_TP_SIZE: usize = 1785;
/// The longest message the extended transport protocol carries.
pub const MAX_ETP_SIZE: usize = 0x00FF_FFFF * 7;
/// The priority of a message whose sender names none.
pub const DEFAULT_PRIORITY: u8 = 6;

/// True if `pgn` is sent to one destination: its low byte is then not part
/// of the group's number but the destination address.
pub fn pgn_is_pdu1(pgn: u32) -> bool {
    (pgn >> 8) & 0xFF < 240
}

/// True if `pgn` is a parameter group number as it is written: in range,
/// and without a destination in the low byte of a PDU1 group.
pub fn pgn_is_valid(pgn: u32) -> bool {
    pgn <= PGN_MAX && (!pgn_is_pdu1(pgn) || pgn & 0xFF == 0)
}

/// One end of a J1939 message, as the `can_addr.j1939` part of Linux's
/// `struct sockaddr_can`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct J1939Addr {
    /// The 64-bit NAME, when the address is the one that NAME has claimed;
    /// [`NO_NAME`] otherwise.
    pub name: u64,
    /// The parameter group, or [`NO_PGN`].
    pub pgn: u32,
    /// The address, or [`NO_ADDR`].
    pub addr: u8,
}

impl J1939Addr {
    /// No NAME, no address, no parameter group: a socket bound to this
    /// receives what is sent to everyone and cannot send.
    pub const NONE: J1939Addr = J1939Addr {
        name: NO_NAME,
        pgn: NO_PGN,
        addr: NO_ADDR,
    };

    /// A fixed address.
    pub fn address(addr: u8) -> J1939Addr {
        J1939Addr {
            addr,
            ..J1939Addr::NONE
        }
    }

    /// Everyone, for a parameter group.
    pub fn broadcast(pgn: u32) -> J1939Addr {
        J1939Addr {
            pgn,
            ..J1939Addr::NONE
        }
    }

    /// A fixed address and a parameter group.
    pub fn new(addr: u8, pgn: u32) -> J1939Addr {
        J1939Addr {
            name: NO_NAME,
            pgn,
            addr,
        }
    }
}

/// A receive filter, Linux's `struct j1939_filter`: a message passes if its
/// source's NAME, its parameter group and its source's address each equal
/// the filter's value under the filter's mask. A mask of zero ignores that
/// part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct J1939Filter {
    pub name: u64,
    pub name_mask: u64,
    pub pgn: u32,
    pub pgn_mask: u32,
    pub addr: u8,
    pub addr_mask: u8,
}

impl J1939Filter {
    /// Messages of one parameter group, from anyone.
    pub fn pgn(pgn: u32) -> J1939Filter {
        J1939Filter {
            pgn,
            pgn_mask: PGN_MAX,
            ..J1939Filter::default()
        }
    }

    pub fn matches(&self, name: u64, pgn: u32, addr: u8) -> bool {
        name & self.name_mask == self.name & self.name_mask
            && pgn & self.pgn_mask == self.pgn & self.pgn_mask
            && addr & self.addr_mask == self.addr & self.addr_mask
    }
}

/// A received message: one frame's worth, or one put together from a
/// transport protocol session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct J1939Message {
    /// When the message's last frame was on the bus.
    pub timestamp_ns: u64,
    /// Who sent it, and the parameter group. The NAME is [`NO_NAME`]
    /// unless the sender has claimed its address.
    pub src: J1939Addr,
    /// The address it was sent to; [`NO_ADDR`] if to everyone.
    pub dst_addr: u8,
    /// The NAME that has claimed that address, if one has.
    pub dst_name: u64,
    /// 0 (highest) to 7.
    pub priority: u8,
    pub data: Vec<u8>,
}

/// What a notice tells of, with the Linux error-queue message it stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum J1939NoticeKind {
    /// A message this socket sent is out: its frame was on the bus, its
    /// broadcast is complete, or the receiver has acknowledged all of it
    /// (`SCM_TSTAMP_ACK`).
    TxDone = 1,
    /// A message this socket sent was given up (`J1939_EE_INFO_TX_ABORT`).
    TxAborted = 2,
    /// A multi-frame message for this socket has begun to arrive
    /// (`J1939_EE_INFO_RX_RTS`).
    RxStarted = 3,
    /// A multi-frame message for this socket was given up part-way
    /// (`J1939_EE_INFO_RX_ABORT`).
    RxAborted = 4,
}

impl J1939NoticeKind {
    pub fn from_u8(v: u8) -> Option<J1939NoticeKind> {
        Some(match v {
            1 => J1939NoticeKind::TxDone,
            2 => J1939NoticeKind::TxAborted,
            3 => J1939NoticeKind::RxStarted,
            4 => J1939NoticeKind::RxAborted,
            _ => return None,
        })
    }
}

/// Why a transport protocol session was given up: the reason in the
/// connection abort message (SAE J1939-21), sent or received.
pub mod abort {
    /// Already in a session with this peer.
    pub const BUSY: u8 = 1;
    /// Resources are needed for something else.
    pub const RESOURCES: u8 = 2;
    /// A timeout.
    pub const TIMEOUT: u8 = 3;
    /// A clear-to-send arrived while data was being sent.
    pub const CTS_WHILE_SENDING: u8 = 4;
    /// The retransmit limit was reached.
    pub const RETRANSMIT_LIMIT: u8 = 5;
    /// A data packet nobody expected.
    pub const UNEXPECTED_DATA: u8 = 6;
    /// A data packet out of sequence.
    pub const BAD_SEQUENCE: u8 = 7;
    /// A data packet twice.
    pub const DUPLICATE_SEQUENCE: u8 = 8;
    /// The message is longer than the protocol carries.
    pub const TOO_BIG: u8 = 9;
    /// Anything else; also given when the interface could not send.
    pub const OTHER: u8 = 250;
}

/// The progress of a message, for a socket that asked to be told (Linux's
/// `SO_J1939_ERRQUEUE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct J1939Notice {
    pub kind: J1939NoticeKind,
    pub timestamp_ns: u64,
    pub pgn: u32,
    pub src_addr: u8,
    pub dst_addr: u8,
    pub src_name: u64,
    pub dst_name: u64,
    /// The length of the whole message.
    pub size: u32,
    /// For the aborted kinds, why: one of [`abort`].
    pub reason: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_groups_are_told_apart() {
        assert!(pgn_is_pdu1(PGN_REQUEST));
        assert!(!pgn_is_pdu1(PGN_ADDRESS_COMMANDED));
        assert!(pgn_is_valid(0xFEF1));
        assert!(pgn_is_valid(0xEA00));
        assert!(!pgn_is_valid(0xEA05));
        assert!(!pgn_is_valid(NO_PGN));
    }

    #[test]
    fn filters_compare_under_their_masks() {
        let f = J1939Filter::pgn(0xFEF1);
        assert!(f.matches(0, 0xFEF1, 0x80));
        assert!(!f.matches(0, 0xFEF2, 0x80));
        let f = J1939Filter {
            addr: 0x80,
            addr_mask: 0xFF,
            ..J1939Filter::default()
        };
        assert!(f.matches(7, 0x1234, 0x80));
        assert!(!f.matches(7, 0x1234, 0x81));
        assert!(J1939Filter::default().matches(1, 2, 3));
    }
}
