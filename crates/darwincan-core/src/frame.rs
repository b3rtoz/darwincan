// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;
use std::str::FromStr;

/// Set in `can_id` for a 29-bit (extended) identifier.
pub const CAN_EFF_FLAG: u32 = 0x8000_0000;
/// Set in `can_id` for a remote transmission request.
pub const CAN_RTR_FLAG: u32 = 0x4000_0000;
/// Set in `can_id` for an error message frame.
pub const CAN_ERR_FLAG: u32 = 0x2000_0000;
pub const CAN_SFF_MASK: u32 = 0x0000_07FF;
pub const CAN_EFF_MASK: u32 = 0x1FFF_FFFF;
/// Set in a filter's `can_id` to invert the match.
pub const CAN_INV_FILTER: u32 = 0x2000_0000;

/// CAN FD: bit rate switch (data phase at the second bitrate).
pub const CANFD_BRS: u8 = 0x01;
/// CAN FD: error state indicator of the transmitting node.
pub const CANFD_ESI: u8 = 0x02;
/// Marks the frame as CAN FD.
pub const CANFD_FDF: u8 = 0x04;

/// Error frame classes (bits of `can_id` when [`CAN_ERR_FLAG`] is set) and
/// detail bytes, as in `<linux/can/error.h>`.
pub mod err {
    /// Controller problems; details in `data[1]`.
    pub const CRTL: u32 = 0x0000_0004;
    /// Protocol violations; details in `data[2]` and `data[3]`.
    pub const PROT: u32 = 0x0000_0008;
    /// Bus off.
    pub const BUSOFF: u32 = 0x0000_0040;
    /// Controller restarted after bus off.
    pub const RESTARTED: u32 = 0x0000_0100;
    /// `data[6]` and `data[7]` hold the TX and RX error counters.
    pub const CNT: u32 = 0x0000_0200;
    /// All error classes, for use as an error filter.
    pub const MASK: u32 = 0x1FFF_FFFF;

    pub const CRTL_RX_OVERFLOW: u8 = 0x01;
    pub const CRTL_RX_WARNING: u8 = 0x04;
    pub const CRTL_TX_WARNING: u8 = 0x08;
    pub const CRTL_RX_PASSIVE: u8 = 0x10;
    pub const CRTL_TX_PASSIVE: u8 = 0x20;
    /// Recovered to the error active state.
    pub const CRTL_ACTIVE: u8 = 0x40;
}

pub const CAN_MAX_DLEN: usize = 8;
pub const CANFD_MAX_DLEN: usize = 64;

/// Smallest valid CAN FD payload length that holds `len` bytes.
pub fn fd_len_round_up(len: usize) -> Option<usize> {
    Some(match len {
        0..=8 => len,
        9..=12 => 12,
        13..=16 => 16,
        17..=20 => 20,
        21..=24 => 24,
        25..=32 => 32,
        33..=48 => 48,
        49..=64 => 64,
        _ => return None,
    })
}

pub fn is_valid_fd_len(len: usize) -> bool {
    fd_len_round_up(len) == Some(len)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    DataTooLong,
    InvalidFdLength,
    InvalidId,
    Parse,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FrameError::DataTooLong => "payload too long for this frame type",
            FrameError::InvalidFdLength => "payload length is not a valid CAN FD length",
            FrameError::InvalidId => "identifier out of range",
            FrameError::Parse => "malformed frame string",
        })
    }
}

impl std::error::Error for FrameError {}

/// A CAN identifier, 11-bit or 29-bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Id {
    Standard(u16),
    Extended(u32),
}

impl Id {
    pub fn standard(id: u16) -> Option<Id> {
        (u32::from(id) <= CAN_SFF_MASK).then_some(Id::Standard(id))
    }

    pub fn extended(id: u32) -> Option<Id> {
        (id <= CAN_EFF_MASK).then_some(Id::Extended(id))
    }

    /// Decode from a SocketCAN `can_id` (flag bits other than EFF ignored).
    pub fn from_can_id(can_id: u32) -> Id {
        if can_id & CAN_EFF_FLAG != 0 {
            Id::Extended(can_id & CAN_EFF_MASK)
        } else {
            Id::Standard((can_id & CAN_SFF_MASK) as u16)
        }
    }

    /// Encode as a SocketCAN `can_id` (EFF flag set for extended ids).
    pub fn to_can_id(self) -> u32 {
        match self {
            Id::Standard(id) => u32::from(id),
            Id::Extended(id) => id | CAN_EFF_FLAG,
        }
    }

    pub fn raw(self) -> u32 {
        match self {
            Id::Standard(id) => u32::from(id),
            Id::Extended(id) => id,
        }
    }
}

/// A classic CAN or CAN FD frame.
///
/// `can_id` and `flags` use the SocketCAN encoding, so a frame maps directly
/// onto `struct can_frame` / `struct canfd_frame`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    can_id: u32,
    len: u8,
    flags: u8,
    // Bytes past `len` are always zero, so derived equality is exact.
    data: [u8; CANFD_MAX_DLEN],
}

impl Frame {
    /// Classic CAN data frame, up to 8 bytes.
    pub fn new(id: Id, data: &[u8]) -> Result<Frame, FrameError> {
        Frame::from_raw(id.to_can_id(), 0, data)
    }

    /// CAN FD data frame. `data.len()` must be a valid FD length
    /// (0-8, 12, 16, 20, 24, 32, 48 or 64).
    pub fn new_fd(id: Id, data: &[u8], brs: bool) -> Result<Frame, FrameError> {
        let flags = CANFD_FDF | if brs { CANFD_BRS } else { 0 };
        Frame::from_raw(id.to_can_id(), flags, data)
    }

    /// Classic CAN remote frame requesting `dlc` bytes.
    pub fn new_remote(id: Id, dlc: u8) -> Result<Frame, FrameError> {
        if usize::from(dlc) > CAN_MAX_DLEN {
            return Err(FrameError::DataTooLong);
        }
        Ok(Frame {
            can_id: id.to_can_id() | CAN_RTR_FLAG,
            len: dlc,
            flags: 0,
            data: [0; CANFD_MAX_DLEN],
        })
    }

    /// Build from SocketCAN-encoded `can_id` and FD `flags`.
    ///
    /// For a remote frame, `data.len()` is the requested length and the
    /// contents are ignored.
    pub fn from_raw(can_id: u32, flags: u8, data: &[u8]) -> Result<Frame, FrameError> {
        let fd = flags & CANFD_FDF != 0;
        let rtr = can_id & CAN_RTR_FLAG != 0;
        if fd {
            if rtr {
                return Err(FrameError::InvalidId);
            }
            if data.len() > CANFD_MAX_DLEN {
                return Err(FrameError::DataTooLong);
            }
            if !is_valid_fd_len(data.len()) {
                return Err(FrameError::InvalidFdLength);
            }
        } else if data.len() > CAN_MAX_DLEN {
            return Err(FrameError::DataTooLong);
        }
        let is_err = can_id & CAN_ERR_FLAG != 0;
        if can_id & CAN_EFF_FLAG == 0 && !is_err && can_id & CAN_EFF_MASK > CAN_SFF_MASK {
            return Err(FrameError::InvalidId);
        }
        let mut buf = [0u8; CANFD_MAX_DLEN];
        if !rtr {
            buf[..data.len()].copy_from_slice(data);
        }
        Ok(Frame {
            can_id,
            len: data.len() as u8,
            flags: if fd { flags } else { 0 },
            data: buf,
        })
    }

    /// An error message frame of the given class(es) (see [`err`]) with its
    /// eight detail bytes.
    pub fn new_error(class: u32, detail: [u8; 8]) -> Frame {
        let mut data = [0u8; CANFD_MAX_DLEN];
        data[..8].copy_from_slice(&detail);
        Frame {
            can_id: CAN_ERR_FLAG | (class & err::MASK),
            len: 8,
            flags: 0,
            data,
        }
    }

    /// SocketCAN-encoded identifier including EFF/RTR/ERR flag bits.
    pub fn can_id(&self) -> u32 {
        self.can_id
    }

    pub fn id(&self) -> Id {
        Id::from_can_id(self.can_id)
    }

    /// CAN FD flags (`CANFD_BRS`, `CANFD_ESI`, `CANFD_FDF`); zero for classic CAN.
    pub fn flags(&self) -> u8 {
        self.flags
    }

    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Payload bytes (empty for remote frames).
    pub fn data(&self) -> &[u8] {
        if self.is_remote() {
            &[]
        } else {
            &self.data[..usize::from(self.len)]
        }
    }

    pub fn is_fd(&self) -> bool {
        self.flags & CANFD_FDF != 0
    }

    pub fn is_brs(&self) -> bool {
        self.flags & CANFD_BRS != 0
    }

    pub fn is_extended(&self) -> bool {
        self.can_id & CAN_EFF_FLAG != 0
    }

    pub fn is_remote(&self) -> bool {
        self.can_id & CAN_RTR_FLAG != 0
    }

    pub fn is_error(&self) -> bool {
        self.can_id & CAN_ERR_FLAG != 0
    }
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Frame({self})")
    }
}

/// Formats in can-utils log notation: `123#DEADBEEF`, `123#R2`, `123##1AABB`.
impl fmt::Display for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_error() {
            // As can-utils logs error frames: the class with the error flag.
            write!(f, "{:08X}", self.can_id & (CAN_ERR_FLAG | err::MASK))?;
        } else if self.is_extended() {
            write!(f, "{:08X}", self.can_id & CAN_EFF_MASK)?;
        } else {
            write!(f, "{:03X}", self.can_id & CAN_SFF_MASK)?;
        }
        if self.is_fd() {
            write!(f, "##{:X}", self.flags & (CANFD_BRS | CANFD_ESI))?;
        } else if self.is_remote() {
            f.write_str("#R")?;
            if self.len > 0 {
                write!(f, "{}", self.len)?;
            }
            return Ok(());
        } else {
            f.write_str("#")?;
        }
        for b in self.data() {
            write!(f, "{b:02X}")?;
        }
        Ok(())
    }
}

/// Parses can-utils `cansend` notation:
/// `<id>#<data>`, `<id>#R[len]`, `<id>##<flags><data>`.
/// The id is 3 hex digits (standard) or 8 (extended); data bytes may be
/// separated by `.`.
impl FromStr for Frame {
    type Err = FrameError;

    fn from_str(s: &str) -> Result<Frame, FrameError> {
        let (id_str, rest) = s.split_once('#').ok_or(FrameError::Parse)?;
        let id_val = u32::from_str_radix(id_str, 16).map_err(|_| FrameError::Parse)?;
        let can_id = match id_str.len() {
            3 => {
                if id_val > CAN_SFF_MASK {
                    return Err(FrameError::InvalidId);
                }
                id_val
            }
            8 => {
                if id_val > CAN_EFF_MASK {
                    return Err(FrameError::InvalidId);
                }
                id_val | CAN_EFF_FLAG
            }
            _ => return Err(FrameError::Parse),
        };

        if let Some(fd_rest) = rest.strip_prefix('#') {
            let mut chars = fd_rest.chars();
            let flag_ch = chars.next().ok_or(FrameError::Parse)?;
            let flags = flag_ch.to_digit(16).ok_or(FrameError::Parse)? as u8;
            let data = parse_hex_bytes(chars.as_str())?;
            let len = fd_len_round_up(data.len()).ok_or(FrameError::DataTooLong)?;
            let mut buf = [0u8; CANFD_MAX_DLEN];
            buf[..data.len()].copy_from_slice(&data);
            let flags = (flags & (CANFD_BRS | CANFD_ESI)) | CANFD_FDF;
            return Frame::from_raw(can_id, flags, &buf[..len]);
        }

        if let Some(rtr_rest) = rest.strip_prefix(['R', 'r']) {
            let dlc = if rtr_rest.is_empty() {
                0
            } else {
                rtr_rest.parse::<u8>().map_err(|_| FrameError::Parse)?
            };
            return Frame::new_remote(Id::from_can_id(can_id), dlc);
        }

        let data = parse_hex_bytes(rest)?;
        Frame::from_raw(can_id, 0, &data)
    }
}

fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, FrameError> {
    let digits: Vec<u8> = s.bytes().filter(|&b| b != b'.').collect();
    if digits.len() % 2 != 0 {
        return Err(FrameError::Parse);
    }
    digits
        .chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16).ok_or(FrameError::Parse)?;
            let lo = (pair[1] as char).to_digit(16).ok_or(FrameError::Parse)?;
            Ok((hi * 16 + lo) as u8)
        })
        .collect()
}

/// A receive filter with SocketCAN semantics: a frame matches when
/// `received_id & can_mask == can_id & can_mask`. Set [`CAN_INV_FILTER`] in
/// `can_id` to invert the result. Include [`CAN_EFF_FLAG`] / [`CAN_RTR_FLAG`]
/// in the mask to distinguish frame formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Filter {
    pub can_id: u32,
    pub can_mask: u32,
}

impl Filter {
    /// Matches every frame.
    pub const ANY: Filter = Filter {
        can_id: 0,
        can_mask: 0,
    };

    pub fn new(can_id: u32, can_mask: u32) -> Filter {
        Filter { can_id, can_mask }
    }

    /// Matches exactly this identifier (and identifier format).
    pub fn exact(id: Id) -> Filter {
        let mask = match id {
            Id::Standard(_) => CAN_SFF_MASK,
            Id::Extended(_) => CAN_EFF_MASK,
        };
        Filter {
            can_id: id.to_can_id(),
            can_mask: mask | CAN_EFF_FLAG | CAN_RTR_FLAG,
        }
    }

    pub fn matches(&self, can_id: u32) -> bool {
        let inverted = self.can_id & CAN_INV_FILTER != 0;
        let want = self.can_id & !CAN_INV_FILTER;
        let mask = self.can_mask & !CAN_ERR_FLAG;
        ((can_id & mask) == (want & mask)) != inverted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format_round_trip() {
        for s in [
            "123#DEADBEEF",
            "7FF#",
            "12345678#0011223344556677",
            "123#R",
            "123#R4",
            "123##1AABBCCDD",
            "1FFFFFFF##0",
        ] {
            let f: Frame = s.parse().unwrap();
            assert_eq!(f.to_string(), s);
        }
    }

    #[test]
    fn error_frames_format_with_their_flag() {
        let f = Frame::new_error(err::BUSOFF, [0; 8]);
        assert_eq!(f.to_string(), "20000040#0000000000000000");
    }

    #[test]
    fn parse_details() {
        let f: Frame = "123#DE.AD.BE.EF".parse().unwrap();
        assert_eq!(f.data(), &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(f.id(), Id::Standard(0x123));
        assert!(!f.is_fd());

        let f: Frame = "00000123#01".parse().unwrap();
        assert_eq!(f.id(), Id::Extended(0x123));

        // FD payloads are padded up to the next valid length.
        let f: Frame = "123##1000102030405060708090A".parse().unwrap();
        assert!(f.is_fd() && f.is_brs());
        assert_eq!(f.len(), 12);

        assert!("123".parse::<Frame>().is_err());
        assert!("1234#00".parse::<Frame>().is_err());
        assert!("123#0".parse::<Frame>().is_err());
        assert!("123#001122334455667788".parse::<Frame>().is_err());
        assert!("FFFFFFFF#00".parse::<Frame>().is_err());
    }

    #[test]
    fn constructors_validate() {
        assert!(Frame::new(Id::Standard(0x100), &[0; 9]).is_err());
        assert!(Frame::new_fd(Id::Standard(0x100), &[0; 9], false).is_err());
        assert!(Frame::new_fd(Id::Standard(0x100), &[0; 12], false).is_ok());
        assert!(Frame::from_raw(0x800, 0, &[]).is_err());
        assert!(Id::standard(0x800).is_none());
        assert!(Id::extended(0x2000_0000).is_none());
    }

    #[test]
    fn filter_semantics() {
        assert!(Filter::ANY.matches(0x123));
        let f = Filter::new(0x120, 0x7F0);
        assert!(f.matches(0x123));
        assert!(!f.matches(0x133));
        let inv = Filter::new(0x120 | CAN_INV_FILTER, 0x7F0);
        assert!(!inv.matches(0x123));
        assert!(inv.matches(0x133));
        let exact = Filter::exact(Id::Standard(0x123));
        assert!(exact.matches(0x123));
        assert!(!exact.matches(0x123 | CAN_EFF_FLAG));
        assert!(!exact.matches(0x123 | CAN_RTR_FLAG));
    }
}
