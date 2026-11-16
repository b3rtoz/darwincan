// SPDX-License-Identifier: MIT OR Apache-2.0

//! Messages of the broadcast manager: jobs that send frames cyclically and
//! jobs that watch received frames for changes and silence, run by the
//! daemon with its own timers.
//!
//! Operations, flags and their meaning follow `<linux/can/bcm.h>`: a
//! [`BcmMessage`] carries what a `struct bcm_msg_head` and the frames
//! after it carry on a `CAN_BCM` socket.

use crate::Frame;

/// What a [`BcmMessage`] asks for or reports (`opcode`).
pub mod op {
    /// Create or change a cyclic transmit job.
    pub const TX_SETUP: u32 = 1;
    /// Remove a transmit job.
    pub const TX_DELETE: u32 = 2;
    /// Ask for a transmit job's settings; answered with `TX_STATUS`.
    pub const TX_READ: u32 = 3;
    /// Send one frame once.
    pub const TX_SEND: u32 = 4;
    /// Create or change a receive job.
    pub const RX_SETUP: u32 = 5;
    /// Remove a receive job.
    pub const RX_DELETE: u32 = 6;
    /// Ask for a receive job's settings; answered with `RX_STATUS`.
    pub const RX_READ: u32 = 7;
    /// The answer to `TX_READ`.
    pub const TX_STATUS: u32 = 8;
    /// A transmit job has sent its `count` frames at `ival1`.
    pub const TX_EXPIRED: u32 = 9;
    /// The answer to `RX_READ`.
    pub const RX_STATUS: u32 = 10;
    /// A watched frame did not arrive within `ival1`.
    pub const RX_TIMEOUT: u32 = 11;
    /// A watched frame arrived for the first time or with changed content.
    pub const RX_CHANGED: u32 = 12;
}

/// Bits of [`BcmMessage::flags`].
pub mod flag {
    /// Take `count`, `ival1` and `ival2` from this message.
    pub const SETTIMER: u32 = 0x0001;
    /// Start the job's timer.
    pub const STARTTIMER: u32 = 0x0002;
    /// Report `TX_EXPIRED` when `count` frames have been sent.
    pub const TX_COUNTEVT: u32 = 0x0004;
    /// Send a frame at once, besides what the timer sends.
    pub const TX_ANNOUNCE: u32 = 0x0008;
    /// Give every frame of the message the message's `can_id`.
    pub const TX_CP_CAN_ID: u32 = 0x0010;
    /// Watch by identifier only: every such frame is reported.
    pub const RX_FILTER_ID: u32 = 0x0020;
    /// A change of length counts as a change of content.
    pub const RX_CHECK_DLC: u32 = 0x0040;
    /// A received frame does not restart the timeout.
    pub const RX_NO_AUTOTIMER: u32 = 0x0080;
    /// After a timeout, report the next frame even if it is unchanged.
    pub const RX_ANNOUNCE_RESUME: u32 = 0x0100;
    /// Start a sequence of several frames again from its first.
    pub const TX_RESET_MULTI_IDX: u32 = 0x0200;
    /// Answer a remote frame with the job's frame.
    pub const RX_RTR_FRAME: u32 = 0x0400;
    /// The job's frames are CAN FD frames.
    pub const CAN_FD_FRAME: u32 = 0x0800;
}

/// The most frames one job may hold.
pub const MAX_FRAMES: usize = 256;

/// One broadcast manager message, to the daemon or from it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BcmMessage {
    /// One of [`op`].
    pub opcode: u32,
    /// Bits of [`flag`].
    pub flags: u32,
    /// How many frames a transmit job sends at `ival1` before it goes
    /// over to `ival2`.
    pub count: u32,
    /// Transmit: the first interval. Receive: the timeout. Microseconds;
    /// 0 for none.
    pub ival1_us: u64,
    /// Transmit: the interval after `count` frames. Receive: the least
    /// time between two reports of a change. Microseconds; 0 for none.
    pub ival2_us: u64,
    /// The job's identifier, with the SocketCAN flag bits.
    pub can_id: u32,
    /// Transmit: the frames to send, in turn. Receive: masks (see the
    /// daemon's `bcm` module), or the frame reported.
    pub frames: Vec<Frame>,
}

impl BcmMessage {
    /// Send `frame` every `interval_us` until the job is deleted.
    pub fn tx_cyclic(frame: Frame, interval_us: u64) -> BcmMessage {
        BcmMessage {
            opcode: op::TX_SETUP,
            flags: flag::SETTIMER | flag::STARTTIMER | fd_flag(&frame),
            count: 0,
            ival1_us: 0,
            ival2_us: interval_us,
            can_id: frame.can_id(),
            frames: vec![frame],
        }
    }

    /// Replace the data a cyclic job sends, without disturbing its timer.
    pub fn tx_update(frame: Frame) -> BcmMessage {
        BcmMessage {
            opcode: op::TX_SETUP,
            flags: fd_flag(&frame),
            can_id: frame.can_id(),
            frames: vec![frame],
            ..BcmMessage::default()
        }
    }

    /// Remove the transmit job for `can_id` (`fd`: its CAN FD job).
    pub fn tx_delete(can_id: u32, fd: bool) -> BcmMessage {
        BcmMessage {
            opcode: op::TX_DELETE,
            flags: if fd { flag::CAN_FD_FRAME } else { 0 },
            can_id,
            ..BcmMessage::default()
        }
    }

    /// Report every frame with `can_id`, and its absence for longer than
    /// `timeout_us` (0: never).
    pub fn rx_watch(can_id: u32, timeout_us: u64) -> BcmMessage {
        BcmMessage {
            opcode: op::RX_SETUP,
            flags: flag::RX_FILTER_ID | flag::SETTIMER | flag::STARTTIMER,
            ival1_us: timeout_us,
            can_id,
            ..BcmMessage::default()
        }
    }
}

fn fd_flag(frame: &Frame) -> u32 {
    if frame.is_fd() {
        flag::CAN_FD_FRAME
    } else {
        0
    }
}
