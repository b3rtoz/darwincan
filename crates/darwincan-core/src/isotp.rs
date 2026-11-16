// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;

/// Configuration of one ISO-TP (ISO 15765-2) channel, normal addressing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoTpParams {
    /// Identifier we transmit on (SocketCAN `can_id`, EFF flag for 29-bit).
    pub tx_id: u32,
    /// Identifier we receive on.
    pub rx_id: u32,
    /// Send CAN FD frames.
    pub fd: bool,
    /// Set the bit rate switch flag on transmitted FD frames.
    pub brs: bool,
    /// Transmit data link length: 8, or with `fd` 12/16/20/24/32/48/64.
    pub tx_dl: u8,
    /// Pad transmitted frames to full length with this byte.
    pub padding: Option<u8>,
    /// Block size we announce in flow control frames (0 = no further FC).
    pub block_size: u8,
    /// STmin we announce in flow control frames, in its raw wire encoding
    /// (0x00-0x7F milliseconds, 0xF1-0xF9 hundreds of microseconds).
    pub st_min: u8,
    /// N_Bs: how long to wait for the peer's flow control frame.
    pub n_bs_ms: u32,
    /// N_Cr: how long to wait for the peer's next consecutive frame.
    pub n_cr_ms: u32,
    /// Largest message we accept; longer ones are refused with FC overflow.
    pub max_rx_len: u32,
}

impl IsoTpParams {
    /// Classic CAN channel with ISO 15765-2 default timeouts.
    pub fn new(tx_id: u32, rx_id: u32) -> IsoTpParams {
        IsoTpParams {
            tx_id,
            rx_id,
            fd: false,
            brs: false,
            tx_dl: 8,
            padding: None,
            block_size: 0,
            st_min: 0,
            n_bs_ms: 1000,
            n_cr_ms: 1000,
            max_rx_len: 1 << 20,
        }
    }

    /// Check the combination of fields is usable.
    pub fn validate(&self) -> Result<(), &'static str> {
        let dl = usize::from(self.tx_dl);
        if !(8..=64).contains(&dl) || !crate::is_valid_fd_len(dl) {
            return Err("tx_dl must be 8, 12, 16, 20, 24, 32, 48 or 64");
        }
        if dl > 8 && !self.fd {
            return Err("tx_dl above 8 requires fd");
        }
        if self.brs && !self.fd {
            return Err("brs requires fd");
        }
        if self.tx_id == self.rx_id {
            return Err("tx_id and rx_id must differ");
        }
        for id in [self.tx_id, self.rx_id] {
            if id & (crate::CAN_RTR_FLAG | crate::CAN_ERR_FLAG) != 0 {
                return Err("ids must not carry RTR or ERR flags");
            }
            if id & crate::CAN_EFF_FLAG == 0 && id > crate::CAN_SFF_MASK {
                return Err("11-bit id out of range (set the EFF flag for 29-bit ids)");
            }
        }
        if self.max_rx_len == 0 {
            return Err("max_rx_len must be non-zero");
        }
        // A message is handed to the client in one piece.
        if self.max_rx_len as usize > crate::wire::MAX_PAYLOAD - 64 {
            return Err("max_rx_len is more than can be delivered (128 MiB)");
        }
        Ok(())
    }
}

/// Why an ISO-TP transfer did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IsoTpFailure {
    /// The peer did not answer in time (N_Bs or N_Cr expired).
    Timeout = 1,
    /// A consecutive frame arrived with the wrong sequence number.
    Sequence = 2,
    /// The peer refused the message with a flow control overflow.
    Overflow = 3,
    /// Message empty or too long to send.
    InvalidLength = 4,
    /// The peer sent a malformed flow control frame.
    InvalidFlowControl = 5,
    /// A transmission is already in progress on this channel.
    Busy = 6,
    /// Reception was interrupted by a new message from the peer.
    Aborted = 7,
    /// The peer sent too many flow control WAIT frames.
    WaitLimit = 8,
    /// A frame of the message could not be put on the bus: the interface
    /// went down or bus-off, or nothing acknowledged the frame in time.
    NotSent = 9,
}

impl IsoTpFailure {
    pub fn from_u8(v: u8) -> Option<IsoTpFailure> {
        Some(match v {
            1 => IsoTpFailure::Timeout,
            2 => IsoTpFailure::Sequence,
            3 => IsoTpFailure::Overflow,
            4 => IsoTpFailure::InvalidLength,
            5 => IsoTpFailure::InvalidFlowControl,
            6 => IsoTpFailure::Busy,
            7 => IsoTpFailure::Aborted,
            8 => IsoTpFailure::WaitLimit,
            9 => IsoTpFailure::NotSent,
            _ => return None,
        })
    }
}

impl fmt::Display for IsoTpFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            IsoTpFailure::Timeout => "timed out waiting for the peer",
            IsoTpFailure::Sequence => "consecutive frame out of sequence",
            IsoTpFailure::Overflow => "peer reported buffer overflow",
            IsoTpFailure::InvalidLength => "message empty or too long",
            IsoTpFailure::InvalidFlowControl => "malformed flow control frame",
            IsoTpFailure::Busy => "a transmission is already in progress",
            IsoTpFailure::Aborted => "interrupted by a new message",
            IsoTpFailure::WaitLimit => "too many flow control wait frames",
            IsoTpFailure::NotSent => "a frame could not be put on the bus",
        })
    }
}
