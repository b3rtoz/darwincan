// SPDX-License-Identifier: MIT OR Apache-2.0

//! Messages exchanged between `darwincand` and its clients over a Unix stream
//! socket.
//!
//! Each message is an 8-byte header (`type: u16`, `version: u16`,
//! `payload_len: u32`, little-endian) followed by the payload. One connection
//! plays the role of one socket: the client opens it as a raw or ISO-TP
//! endpoint with its first message and then exchanges frames or PDUs.
//! Option changes (`SetFilters`, `SetOption`) and `Sync` are acknowledged
//! with `Ok`, in order, in the same stream as the frames. Sends are not acknowledged;
//! their failures come back as asynchronous [`Message::Error`] messages.
//! A broadcast manager request (`Bcm`) is answered in the same order with
//! `Ok` or, if it is refused, with `Refused`. So is everything a J1939
//! socket asks, its sends included: a J1939 send can be refused for reasons
//! only the daemon knows (a NAME without an address).

use std::io::{self, Read, Write};

use crate::{
    BcmMessage, BusConfig, Filter, Frame, IsoTpFailure, IsoTpParams, J1939Addr, J1939Filter,
    J1939Message, J1939Notice, J1939NoticeKind,
};

/// The version of the protocol this build speaks, carried in every
/// message header. Daemon and client must agree on it exactly; it goes
/// up with every change to a message.
pub const PROTOCOL_VERSION: u16 = 5;
const HEADER_LEN: usize = 8;
/// Upper bound on a payload, to keep a corrupt header from allocating wildly.
/// The longest J1939 message (117 MB) fits.
pub const MAX_PAYLOAD: usize = 128 << 20;

/// Per-socket options, mirroring the `CAN_RAW_*` socket options and, for a
/// J1939 socket, the `SO_J1939_*` ones and `SO_BROADCAST`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum SockOpt {
    /// Deliver frames this socket sends to other local sockets (default on).
    Loopback = 1,
    /// Also deliver them back to this socket (default off).
    RecvOwnMsgs = 2,
    /// Mask of error classes whose error frames this socket receives
    /// (default 0: none).
    ErrFilter = 3,
    /// Deliver a frame only if every filter matches it, not if any does
    /// (default off), as `CAN_RAW_JOIN_FILTERS`.
    JoinFilters = 4,
    /// J1939: receive whatever is on the bus, whoever it is for (default
    /// off), as `SO_J1939_PROMISC`.
    J1939Promisc = 5,
    /// J1939: allow sending to everyone and receiving what is sent to
    /// everyone (default off), as `SO_BROADCAST`.
    J1939Broadcast = 6,
    /// J1939: the priority of messages sent without one, 0 (highest) to 7
    /// (default 6), as `SO_J1939_SEND_PRIO`.
    J1939SendPrio = 7,
    /// J1939: send notices of the progress of messages (default off), as
    /// `SO_J1939_ERRQUEUE`.
    J1939Notices = 8,
}

impl SockOpt {
    fn from_u16(v: u16) -> Option<SockOpt> {
        match v {
            1 => Some(SockOpt::Loopback),
            2 => Some(SockOpt::RecvOwnMsgs),
            3 => Some(SockOpt::ErrFilter),
            4 => Some(SockOpt::JoinFilters),
            5 => Some(SockOpt::J1939Promisc),
            6 => Some(SockOpt::J1939Broadcast),
            7 => Some(SockOpt::J1939SendPrio),
            8 => Some(SockOpt::J1939Notices),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ErrorCode {
    NoSuchInterface = 1,
    InvalidRequest = 2,
    AlreadyExists = 3,
    Unsupported = 4,
    TransmitFailed = 5,
    InterfaceDown = 6,
    InterfaceRemoved = 7,
    /// The interface is on the bus and must be taken down first.
    Busy = 8,
    /// J1939: a NAME has no claimed address (`EADDRNOTAVAIL`).
    AddressNotAvailable = 9,
    /// J1939: to everyone, from a socket not allowed to (`EACCES`).
    NotPermitted = 10,
    /// The message is longer than can be carried (`EMSGSIZE`).
    TooBig = 11,
    Other = 0xFFFF,
}

impl ErrorCode {
    fn from_u16(v: u16) -> ErrorCode {
        match v {
            1 => ErrorCode::NoSuchInterface,
            2 => ErrorCode::InvalidRequest,
            3 => ErrorCode::AlreadyExists,
            4 => ErrorCode::Unsupported,
            5 => ErrorCode::TransmitFailed,
            6 => ErrorCode::InterfaceDown,
            7 => ErrorCode::InterfaceRemoved,
            8 => ErrorCode::Busy,
            9 => ErrorCode::AddressNotAvailable,
            10 => ErrorCode::NotPermitted,
            11 => ErrorCode::TooBig,
            _ => ErrorCode::Other,
        }
    }
}

/// Whether an interface can send, as the daemon tells a raw socket so
/// that a send on one that cannot fails at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LinkState {
    Up = 0,
    /// Not on the bus.
    Down = 1,
    /// The controller went bus-off and sends nothing until restarted.
    BusOff = 2,
    /// The adapter was unplugged.
    Removed = 3,
}

impl LinkState {
    fn from_u8(v: u8) -> Option<LinkState> {
        Some(match v {
            0 => LinkState::Up,
            1 => LinkState::Down,
            2 => LinkState::BusOff,
            3 => LinkState::Removed,
            _ => return None,
        })
    }
}

/// The interface name that opens a raw socket on every interface.
pub const ANY_INTERFACE: &str = "any";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfaceInfo {
    /// The interface's number, from 1: what a frame received on a socket
    /// open on every interface is marked with. It stays the same while
    /// the interface exists and is not given to another while the daemon
    /// runs.
    pub index: u32,
    pub name: String,
    /// `"virtual"` or the backend's name.
    pub kind: String,
    /// Human-readable device description (model, serial number); may be empty.
    pub description: String,
    /// Whether the interface is on the bus. Virtual interfaces always are.
    pub up: bool,
    /// The bus configuration: the active one while up, otherwise the one
    /// remembered for the next `IfaceUp` (`None` if it was never
    /// configured). Virtual interfaces report `None`.
    pub config: Option<BusConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    // Client to daemon.
    OpenRaw {
        ifname: String,
    },
    OpenIsoTp {
        ifname: String,
        params: IsoTpParams,
    },
    SetFilters(Vec<Filter>),
    SetOption {
        opt: SockOpt,
        value: u32,
    },
    Send(Frame),
    IsoTpSend(Vec<u8>),
    ListIfaces,
    AddVcan {
        name: String,
    },
    /// Configure a hardware interface and put it on the bus.
    IfaceUp {
        name: String,
        config: BusConfig,
    },
    /// Take a hardware interface off the bus. Its configuration is kept.
    IfaceDown {
        name: String,
    },
    /// Store the configuration of a hardware interface that is down, for a
    /// later `IfaceUp` (the client reads it back with `ListIfaces`).
    /// Answered with `Ok`, or `Error` (`Busy` if the interface is up).
    IfaceConfigure {
        name: String,
        config: BusConfig,
    },
    /// Remove a virtual CAN interface. Answered with `Ok` or `Error`.
    DelVcan {
        name: String,
    },
    /// Give an interface another name. Answered with `Ok` or `Error`
    /// (`Busy` if it is a hardware interface that is up).
    IfaceRename {
        name: String,
        new_name: String,
    },
    /// Ask for an `Ok` once everything sent before this has been handled.
    Sync,
    /// Open the connection as a broadcast manager on an interface.
    /// Answered with `Ok` or `Error`.
    OpenBcm {
        ifname: String,
    },
    /// A broadcast manager request. Answered with `Ok` or `Refused`, in
    /// order with the other acknowledgements.
    Bcm(BcmMessage),
    /// Send a frame on the interface with this index, on a raw socket open
    /// on every interface. Not acknowledged, like `Send`.
    SendTo {
        ifindex: u32,
        frame: Frame,
    },
    /// Ask a raw socket's interface whether it can send. Answered with
    /// `LinkState` and then `Ok`.
    QueryLink,
    /// Open the connection as a J1939 socket on an interface, bound to
    /// `addr`. Answered with `Ok` or `Error`.
    OpenJ1939 {
        ifname: String,
        addr: J1939Addr,
    },
    /// Bind a J1939 socket anew. Answered with `Ok` or `Refused`, as are
    /// the other J1939 requests.
    J1939Bind(J1939Addr),
    /// Set where a J1939 socket sends when it is not told, and whom it
    /// receives from.
    J1939Connect(J1939Addr),
    J1939Filters(Vec<J1939Filter>),
    /// Send a message: to `dst`, or to where the socket is connected; at
    /// `priority`, or at the socket's. `Ok` says the message is taken,
    /// not that it has been sent.
    J1939Send {
        dst: Option<J1939Addr>,
        priority: Option<u8>,
        data: Vec<u8>,
    },

    // Daemon to client.
    Ok,
    Error {
        code: ErrorCode,
        message: String,
    },
    Frame {
        /// The index of the interface the frame was on.
        ifindex: u32,
        /// Nanoseconds since the Unix epoch, from the daemon's monotonic clock.
        timestamp_ns: u64,
        /// True if this socket sent the frame itself.
        own: bool,
        frame: Frame,
    },
    IsoTpData {
        timestamp_ns: u64,
        data: Vec<u8>,
    },
    IsoTpTxDone,
    IsoTpFailed {
        /// True for a failed transmission, false for a failed reception.
        tx: bool,
        reason: IsoTpFailure,
    },
    Ifaces(Vec<IfaceInfo>),
    /// The answer to an acknowledged request that was not carried out,
    /// in the place its `Ok` would have had.
    Refused {
        code: ErrorCode,
        message: String,
    },
    /// How many frames a raw socket has lost so far because its client
    /// read too slowly, sent ahead of the next frame after a loss.
    Dropped {
        total: u64,
    },
    /// A raw socket's interface can or cannot send: sent when the socket
    /// is opened, before its `Ok`, and whenever that changes.
    LinkState(LinkState),
    /// The broadcast manager reports: a frame changed, a timeout, a job's
    /// settings.
    BcmNotify {
        timestamp_ns: u64,
        msg: BcmMessage,
    },
    /// A message for a J1939 socket.
    J1939Data(J1939Message),
    /// The progress of a message, for a J1939 socket that asked.
    J1939Notice(J1939Notice),
}

const T_OPEN_RAW: u16 = 1;
const T_OPEN_ISOTP: u16 = 2;
const T_SET_FILTERS: u16 = 3;
const T_SET_OPTION: u16 = 4;
const T_SEND: u16 = 5;
const T_ISOTP_SEND: u16 = 6;
const T_LIST_IFACES: u16 = 7;
const T_ADD_VCAN: u16 = 8;
const T_IFACE_UP: u16 = 9;
const T_IFACE_DOWN: u16 = 10;
const T_SYNC: u16 = 11;
const T_IFACE_CONFIGURE: u16 = 12;
const T_DEL_VCAN: u16 = 13;
const T_OPEN_BCM: u16 = 14;
const T_BCM: u16 = 15;
const T_SEND_TO: u16 = 16;
const T_QUERY_LINK: u16 = 17;
const T_IFACE_RENAME: u16 = 18;
const T_OPEN_J1939: u16 = 19;
const T_J1939_BIND: u16 = 20;
const T_J1939_CONNECT: u16 = 21;
const T_J1939_FILTERS: u16 = 22;
const T_J1939_SEND: u16 = 23;
const T_OK: u16 = 64;
const T_ERROR: u16 = 65;
const T_FRAME: u16 = 66;
const T_ISOTP_DATA: u16 = 67;
const T_ISOTP_TX_DONE: u16 = 68;
const T_ISOTP_FAILED: u16 = 69;
const T_IFACES: u16 = 70;
const T_REFUSED: u16 = 71;
const T_BCM_NOTIFY: u16 = 72;
const T_DROPPED: u16 = 73;
const T_LINK_STATE: u16 = 74;
const T_J1939_DATA: u16 = 75;
const T_J1939_NOTICE: u16 = 76;

/// The other end of a connection speaks another version of the protocol:
/// daemon and client come from different builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionMismatch {
    pub ours: u16,
    pub theirs: u16,
}

impl std::fmt::Display for VersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "protocol version mismatch between client and daemon \
             (this side speaks version {}, the other version {})",
            self.ours, self.theirs
        )
    }
}

impl std::error::Error for VersionMismatch {}

/// The version mismatch behind a read error, if that is what it was.
pub fn version_mismatch(e: &io::Error) -> Option<VersionMismatch> {
    e.get_ref()?.downcast_ref::<VersionMismatch>().copied()
}

fn bad(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    let bytes = &s.as_bytes()[..s.len().min(u16::MAX as usize)];
    buf.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(bytes);
}

fn put_frame(buf: &mut Vec<u8>, f: &Frame) {
    buf.extend_from_slice(&f.can_id().to_le_bytes());
    buf.push(f.len() as u8);
    buf.push(f.flags());
    // Remote frames carry a length but no data.
    buf.extend_from_slice(f.data());
}

fn put_bcm(buf: &mut Vec<u8>, m: &BcmMessage) {
    buf.extend_from_slice(&m.opcode.to_le_bytes());
    buf.extend_from_slice(&m.flags.to_le_bytes());
    buf.extend_from_slice(&m.count.to_le_bytes());
    buf.extend_from_slice(&m.ival1_us.to_le_bytes());
    buf.extend_from_slice(&m.ival2_us.to_le_bytes());
    buf.extend_from_slice(&m.can_id.to_le_bytes());
    buf.extend_from_slice(&(m.frames.len() as u32).to_le_bytes());
    for frame in &m.frames {
        put_frame(buf, frame);
    }
}

fn put_j1939_addr(buf: &mut Vec<u8>, a: &J1939Addr) {
    buf.extend_from_slice(&a.name.to_le_bytes());
    buf.extend_from_slice(&a.pgn.to_le_bytes());
    buf.push(a.addr);
}

fn put_config(buf: &mut Vec<u8>, c: &BusConfig) {
    buf.extend_from_slice(&c.bitrate.to_le_bytes());
    buf.extend_from_slice(&c.data_bitrate.unwrap_or(0).to_le_bytes());
    buf.extend_from_slice(&c.sample_point.unwrap_or(0).to_le_bytes());
    buf.extend_from_slice(&c.data_sample_point.unwrap_or(0).to_le_bytes());
    buf.push(u8::from(c.listen_only));
}

struct Rd<'a>(&'a [u8]);

impl<'a> Rd<'a> {
    fn bytes(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(bad("message payload truncated"));
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    fn string(&mut self) -> io::Result<String> {
        let n = usize::from(self.u16()?);
        String::from_utf8(self.bytes(n)?.to_vec()).map_err(|_| bad("string is not UTF-8"))
    }
    fn config(&mut self) -> io::Result<BusConfig> {
        let bitrate = self.u32()?;
        let data_bitrate = self.u32()?;
        let sample_point = self.u16()?;
        let data_sample_point = self.u16()?;
        Ok(BusConfig {
            bitrate,
            data_bitrate: (data_bitrate != 0).then_some(data_bitrate),
            sample_point: (sample_point != 0).then_some(sample_point),
            data_sample_point: (data_sample_point != 0).then_some(data_sample_point),
            listen_only: self.u8()? != 0,
        })
    }
    fn bcm(&mut self) -> io::Result<BcmMessage> {
        let opcode = self.u32()?;
        let flags = self.u32()?;
        let count = self.u32()?;
        let ival1_us = self.u64()?;
        let ival2_us = self.u64()?;
        let can_id = self.u32()?;
        let n = self.u32()? as usize;
        // One more than a job may hold: a receive job's first frame can
        // be a mask in front of the full number.
        if n > crate::bcm::MAX_FRAMES + 1 {
            return Err(bad("too many frames in a broadcast manager message"));
        }
        let mut frames = Vec::with_capacity(n);
        for _ in 0..n {
            frames.push(self.frame()?);
        }
        Ok(BcmMessage {
            opcode,
            flags,
            count,
            ival1_us,
            ival2_us,
            can_id,
            frames,
        })
    }
    fn j1939_addr(&mut self) -> io::Result<J1939Addr> {
        Ok(J1939Addr {
            name: self.u64()?,
            pgn: self.u32()?,
            addr: self.u8()?,
        })
    }
    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }
    fn frame(&mut self) -> io::Result<Frame> {
        let can_id = self.u32()?;
        let len = usize::from(self.u8()?);
        let flags = self.u8()?;
        if can_id & crate::CAN_RTR_FLAG != 0 {
            if len > crate::CAN_MAX_DLEN {
                return Err(bad("invalid frame"));
            }
            return Frame::from_raw(can_id, flags, &[0u8; 8][..len])
                .map_err(|_| bad("invalid frame"));
        }
        let data = self.bytes(len)?;
        Frame::from_raw(can_id, flags, data).map_err(|_| bad("invalid frame"))
    }
}

impl Message {
    /// Append the encoded message (header and payload) to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&[0u8; HEADER_LEN]);
        let ty = match self {
            Message::OpenRaw { ifname } => {
                put_str(out, ifname);
                T_OPEN_RAW
            }
            Message::OpenIsoTp { ifname, params } => {
                put_str(out, ifname);
                out.extend_from_slice(&params.tx_id.to_le_bytes());
                out.extend_from_slice(&params.rx_id.to_le_bytes());
                out.push(params.tx_dl);
                let mut flags = 0u8;
                if params.fd {
                    flags |= 1;
                }
                if params.brs {
                    flags |= 2;
                }
                if params.padding.is_some() {
                    flags |= 4;
                }
                out.push(flags);
                out.push(params.padding.unwrap_or(0));
                out.push(params.block_size);
                out.push(params.st_min);
                out.extend_from_slice(&params.n_bs_ms.to_le_bytes());
                out.extend_from_slice(&params.n_cr_ms.to_le_bytes());
                out.extend_from_slice(&params.max_rx_len.to_le_bytes());
                T_OPEN_ISOTP
            }
            Message::SetFilters(filters) => {
                for f in filters {
                    out.extend_from_slice(&f.can_id.to_le_bytes());
                    out.extend_from_slice(&f.can_mask.to_le_bytes());
                }
                T_SET_FILTERS
            }
            Message::SetOption { opt, value } => {
                out.extend_from_slice(&(*opt as u16).to_le_bytes());
                out.extend_from_slice(&value.to_le_bytes());
                T_SET_OPTION
            }
            Message::Send(frame) => {
                put_frame(out, frame);
                T_SEND
            }
            Message::IsoTpSend(data) => {
                out.extend_from_slice(data);
                T_ISOTP_SEND
            }
            Message::ListIfaces => T_LIST_IFACES,
            Message::AddVcan { name } => {
                put_str(out, name);
                T_ADD_VCAN
            }
            Message::IfaceUp { name, config } => {
                put_str(out, name);
                put_config(out, config);
                T_IFACE_UP
            }
            Message::IfaceDown { name } => {
                put_str(out, name);
                T_IFACE_DOWN
            }
            Message::IfaceConfigure { name, config } => {
                put_str(out, name);
                put_config(out, config);
                T_IFACE_CONFIGURE
            }
            Message::DelVcan { name } => {
                put_str(out, name);
                T_DEL_VCAN
            }
            Message::IfaceRename { name, new_name } => {
                put_str(out, name);
                put_str(out, new_name);
                T_IFACE_RENAME
            }
            Message::Sync => T_SYNC,
            Message::OpenBcm { ifname } => {
                put_str(out, ifname);
                T_OPEN_BCM
            }
            Message::Bcm(msg) => {
                put_bcm(out, msg);
                T_BCM
            }
            Message::SendTo { ifindex, frame } => {
                out.extend_from_slice(&ifindex.to_le_bytes());
                put_frame(out, frame);
                T_SEND_TO
            }
            Message::QueryLink => T_QUERY_LINK,
            Message::OpenJ1939 { ifname, addr } => {
                put_str(out, ifname);
                put_j1939_addr(out, addr);
                T_OPEN_J1939
            }
            Message::J1939Bind(addr) => {
                put_j1939_addr(out, addr);
                T_J1939_BIND
            }
            Message::J1939Connect(addr) => {
                put_j1939_addr(out, addr);
                T_J1939_CONNECT
            }
            Message::J1939Filters(filters) => {
                for f in filters {
                    out.extend_from_slice(&f.name.to_le_bytes());
                    out.extend_from_slice(&f.name_mask.to_le_bytes());
                    out.extend_from_slice(&f.pgn.to_le_bytes());
                    out.extend_from_slice(&f.pgn_mask.to_le_bytes());
                    out.push(f.addr);
                    out.push(f.addr_mask);
                }
                T_J1939_FILTERS
            }
            Message::J1939Send {
                dst,
                priority,
                data,
            } => {
                out.push(u8::from(dst.is_some()));
                put_j1939_addr(out, &dst.unwrap_or(J1939Addr::NONE));
                // 0xFF: the socket's own.
                out.push(priority.unwrap_or(0xFF));
                out.extend_from_slice(data);
                T_J1939_SEND
            }
            Message::J1939Data(m) => {
                out.extend_from_slice(&m.timestamp_ns.to_le_bytes());
                put_j1939_addr(out, &m.src);
                out.push(m.dst_addr);
                out.extend_from_slice(&m.dst_name.to_le_bytes());
                out.push(m.priority);
                out.extend_from_slice(&m.data);
                T_J1939_DATA
            }
            Message::J1939Notice(n) => {
                out.push(n.kind as u8);
                out.extend_from_slice(&n.timestamp_ns.to_le_bytes());
                out.extend_from_slice(&n.pgn.to_le_bytes());
                out.push(n.src_addr);
                out.push(n.dst_addr);
                out.extend_from_slice(&n.src_name.to_le_bytes());
                out.extend_from_slice(&n.dst_name.to_le_bytes());
                out.extend_from_slice(&n.size.to_le_bytes());
                out.push(n.reason);
                T_J1939_NOTICE
            }
            Message::Dropped { total } => {
                out.extend_from_slice(&total.to_le_bytes());
                T_DROPPED
            }
            Message::LinkState(state) => {
                out.push(*state as u8);
                T_LINK_STATE
            }
            Message::Refused { code, message } => {
                out.extend_from_slice(&(*code as u16).to_le_bytes());
                put_str(out, message);
                T_REFUSED
            }
            Message::BcmNotify { timestamp_ns, msg } => {
                out.extend_from_slice(&timestamp_ns.to_le_bytes());
                put_bcm(out, msg);
                T_BCM_NOTIFY
            }
            Message::Ok => T_OK,
            Message::Error { code, message } => {
                out.extend_from_slice(&(*code as u16).to_le_bytes());
                put_str(out, message);
                T_ERROR
            }
            Message::Frame {
                ifindex,
                timestamp_ns,
                own,
                frame,
            } => {
                out.extend_from_slice(&ifindex.to_le_bytes());
                out.extend_from_slice(&timestamp_ns.to_le_bytes());
                out.push(u8::from(*own));
                put_frame(out, frame);
                T_FRAME
            }
            Message::IsoTpData { timestamp_ns, data } => {
                out.extend_from_slice(&timestamp_ns.to_le_bytes());
                out.extend_from_slice(data);
                T_ISOTP_DATA
            }
            Message::IsoTpTxDone => T_ISOTP_TX_DONE,
            Message::IsoTpFailed { tx, reason } => {
                out.push(u8::from(*tx));
                out.push(*reason as u8);
                T_ISOTP_FAILED
            }
            Message::Ifaces(list) => {
                out.extend_from_slice(&(list.len() as u16).to_le_bytes());
                for i in list {
                    out.extend_from_slice(&i.index.to_le_bytes());
                    put_str(out, &i.name);
                    put_str(out, &i.kind);
                    put_str(out, &i.description);
                    out.push(u8::from(i.up));
                    out.push(u8::from(i.config.is_some()));
                    if let Some(c) = &i.config {
                        put_config(out, c);
                    }
                }
                T_IFACES
            }
        };
        let payload_len = (out.len() - start - HEADER_LEN) as u32;
        out[start..start + 2].copy_from_slice(&ty.to_le_bytes());
        out[start + 2..start + 4].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        out[start + 4..start + 8].copy_from_slice(&payload_len.to_le_bytes());
    }

    /// Encode and write the message with a single `write_all`.
    pub fn write_to<W: Write>(&self, mut w: W) -> io::Result<()> {
        let mut buf = Vec::with_capacity(96);
        self.encode(&mut buf);
        w.write_all(&buf)
    }

    fn decode(ty: u16, payload: &[u8]) -> io::Result<Message> {
        let mut r = Rd(payload);
        let msg = match ty {
            T_OPEN_RAW => Message::OpenRaw {
                ifname: r.string()?,
            },
            T_OPEN_ISOTP => {
                let ifname = r.string()?;
                let tx_id = r.u32()?;
                let rx_id = r.u32()?;
                let tx_dl = r.u8()?;
                let flags = r.u8()?;
                let pad = r.u8()?;
                let block_size = r.u8()?;
                let st_min = r.u8()?;
                Message::OpenIsoTp {
                    ifname,
                    params: IsoTpParams {
                        tx_id,
                        rx_id,
                        fd: flags & 1 != 0,
                        brs: flags & 2 != 0,
                        tx_dl,
                        padding: (flags & 4 != 0).then_some(pad),
                        block_size,
                        st_min,
                        n_bs_ms: r.u32()?,
                        n_cr_ms: r.u32()?,
                        max_rx_len: r.u32()?,
                    },
                }
            }
            T_SET_FILTERS => {
                if payload.len() % 8 != 0 {
                    return Err(bad("filter list has a partial entry"));
                }
                let mut filters = Vec::with_capacity(payload.len() / 8);
                while !r.0.is_empty() {
                    filters.push(Filter {
                        can_id: r.u32()?,
                        can_mask: r.u32()?,
                    });
                }
                Message::SetFilters(filters)
            }
            T_SET_OPTION => {
                let opt =
                    SockOpt::from_u16(r.u16()?).ok_or_else(|| bad("unknown socket option"))?;
                Message::SetOption {
                    opt,
                    value: r.u32()?,
                }
            }
            T_SEND => Message::Send(r.frame()?),
            T_ISOTP_SEND => Message::IsoTpSend(r.rest().to_vec()),
            T_LIST_IFACES => Message::ListIfaces,
            T_ADD_VCAN => Message::AddVcan { name: r.string()? },
            T_IFACE_UP => Message::IfaceUp {
                name: r.string()?,
                config: r.config()?,
            },
            T_IFACE_DOWN => Message::IfaceDown { name: r.string()? },
            T_IFACE_CONFIGURE => Message::IfaceConfigure {
                name: r.string()?,
                config: r.config()?,
            },
            T_DEL_VCAN => Message::DelVcan { name: r.string()? },
            T_IFACE_RENAME => Message::IfaceRename {
                name: r.string()?,
                new_name: r.string()?,
            },
            T_SYNC => Message::Sync,
            T_OPEN_BCM => Message::OpenBcm {
                ifname: r.string()?,
            },
            T_BCM => Message::Bcm(r.bcm()?),
            T_SEND_TO => Message::SendTo {
                ifindex: r.u32()?,
                frame: r.frame()?,
            },
            T_QUERY_LINK => Message::QueryLink,
            T_OPEN_J1939 => Message::OpenJ1939 {
                ifname: r.string()?,
                addr: r.j1939_addr()?,
            },
            T_J1939_BIND => Message::J1939Bind(r.j1939_addr()?),
            T_J1939_CONNECT => Message::J1939Connect(r.j1939_addr()?),
            T_J1939_FILTERS => {
                if payload.len() % 26 != 0 {
                    return Err(bad("filter list has a partial entry"));
                }
                let mut filters = Vec::with_capacity(payload.len() / 26);
                while !r.0.is_empty() {
                    filters.push(J1939Filter {
                        name: r.u64()?,
                        name_mask: r.u64()?,
                        pgn: r.u32()?,
                        pgn_mask: r.u32()?,
                        addr: r.u8()?,
                        addr_mask: r.u8()?,
                    });
                }
                Message::J1939Filters(filters)
            }
            T_J1939_SEND => {
                let has_dst = r.u8()? != 0;
                let dst = r.j1939_addr()?;
                let priority = r.u8()?;
                Message::J1939Send {
                    dst: has_dst.then_some(dst),
                    priority: (priority != 0xFF).then_some(priority),
                    data: r.rest().to_vec(),
                }
            }
            T_J1939_DATA => Message::J1939Data(J1939Message {
                timestamp_ns: r.u64()?,
                src: r.j1939_addr()?,
                dst_addr: r.u8()?,
                dst_name: r.u64()?,
                priority: r.u8()?,
                data: r.rest().to_vec(),
            }),
            T_J1939_NOTICE => Message::J1939Notice(J1939Notice {
                kind: J1939NoticeKind::from_u8(r.u8()?)
                    .ok_or_else(|| bad("unknown J1939 notice"))?,
                timestamp_ns: r.u64()?,
                pgn: r.u32()?,
                src_addr: r.u8()?,
                dst_addr: r.u8()?,
                src_name: r.u64()?,
                dst_name: r.u64()?,
                size: r.u32()?,
                reason: r.u8()?,
            }),
            T_DROPPED => Message::Dropped { total: r.u64()? },
            T_LINK_STATE => Message::LinkState(
                LinkState::from_u8(r.u8()?).ok_or_else(|| bad("unknown link state"))?,
            ),
            T_REFUSED => Message::Refused {
                code: ErrorCode::from_u16(r.u16()?),
                message: r.string()?,
            },
            T_BCM_NOTIFY => Message::BcmNotify {
                timestamp_ns: r.u64()?,
                msg: r.bcm()?,
            },
            T_OK => Message::Ok,
            T_ERROR => Message::Error {
                code: ErrorCode::from_u16(r.u16()?),
                message: r.string()?,
            },
            T_FRAME => Message::Frame {
                ifindex: r.u32()?,
                timestamp_ns: r.u64()?,
                own: r.u8()? != 0,
                frame: r.frame()?,
            },
            T_ISOTP_DATA => Message::IsoTpData {
                timestamp_ns: r.u64()?,
                data: r.rest().to_vec(),
            },
            T_ISOTP_TX_DONE => Message::IsoTpTxDone,
            T_ISOTP_FAILED => {
                let tx = r.u8()? != 0;
                let reason =
                    IsoTpFailure::from_u8(r.u8()?).ok_or_else(|| bad("unknown ISO-TP failure"))?;
                Message::IsoTpFailed { tx, reason }
            }
            T_IFACES => {
                let n = usize::from(r.u16()?);
                let mut list = Vec::with_capacity(n.min(256));
                for _ in 0..n {
                    let index = r.u32()?;
                    let name = r.string()?;
                    let kind = r.string()?;
                    let description = r.string()?;
                    let up = r.u8()? != 0;
                    let config = if r.u8()? != 0 {
                        Some(r.config()?)
                    } else {
                        None
                    };
                    list.push(IfaceInfo {
                        index,
                        name,
                        kind,
                        description,
                        up,
                        config,
                    });
                }
                Message::Ifaces(list)
            }
            _ => return Err(bad("unknown message type")),
        };
        Ok(msg)
    }
}

/// Reads messages from a byte stream.
///
/// Bytes of a partially received message are kept across calls, so a read
/// timeout on the underlying stream never desynchronises the framing.
/// The longest payload of everything but the messages that carry a
/// transport protocol's data or the list of interfaces: the largest of the
/// rest, a broadcast manager message of 257 CAN FD frames, is a third of
/// this.
const MAX_SMALL_PAYLOAD: usize = 64 * 1024;

/// The longest payload a message of type `ty` can have. Checked on the
/// header alone, so that a peer cannot make the reader hold megabytes
/// for a message that never has them.
fn max_payload(ty: u16) -> usize {
    match ty {
        T_ISOTP_SEND | T_ISOTP_DATA | T_J1939_SEND | T_J1939_DATA | T_IFACES => MAX_PAYLOAD,
        _ => MAX_SMALL_PAYLOAD,
    }
}

/// What the reader's buffer is cut back to once a long message has gone
/// through it.
const READER_KEEPS: usize = 64 * 1024;

pub struct MessageReader<R> {
    inner: R,
    buf: Vec<u8>,
    pos: usize,
}

impl<R: Read> MessageReader<R> {
    pub fn new(inner: R) -> Self {
        MessageReader {
            inner,
            buf: Vec::with_capacity(4096),
            pos: 0,
        }
    }

    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Return the next message, reading from the stream as needed. An
    /// orderly close by the peer is reported as `UnexpectedEof`.
    pub fn read(&mut self) -> io::Result<Message> {
        loop {
            if let Some(msg) = self.try_parse()? {
                return Ok(msg);
            }
            self.compact();
            let mut chunk = [0u8; 16 * 1024];
            let n = match self.inner.read(&mut chunk) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// A message that is already complete in what has been read, without
    /// touching the stream.
    pub fn buffered(&mut self) -> io::Result<Option<Message>> {
        self.try_parse()
    }

    /// Read from the stream once and keep what arrives for
    /// [`buffered`](Self::buffered). Blocks like one `read` on the stream,
    /// and like it fails with `Interrupted` when a signal cuts it short.
    pub fn fill(&mut self) -> io::Result<()> {
        self.compact();
        let mut chunk = [0u8; 16 * 1024];
        match self.inner.read(&mut chunk)? {
            0 => Err(io::ErrorKind::UnexpectedEof.into()),
            n => {
                self.buf.extend_from_slice(&chunk[..n]);
                Ok(())
            }
        }
    }

    /// Drop what has been parsed, and give back the room a long message
    /// took once it is no longer needed.
    fn compact(&mut self) {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        if self.buf.capacity() > 4 * READER_KEEPS && self.buf.len() <= READER_KEEPS {
            self.buf.shrink_to(READER_KEEPS);
        }
    }

    fn try_parse(&mut self) -> io::Result<Option<Message>> {
        let avail = &self.buf[self.pos..];
        if avail.len() < HEADER_LEN {
            return Ok(None);
        }
        let ty = u16::from_le_bytes([avail[0], avail[1]]);
        let version = u16::from_le_bytes([avail[2], avail[3]]);
        let len = u32::from_le_bytes([avail[4], avail[5], avail[6], avail[7]]) as usize;
        if version != PROTOCOL_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                VersionMismatch {
                    ours: PROTOCOL_VERSION,
                    theirs: version,
                },
            ));
        }
        if len > max_payload(ty) {
            return Err(bad("message payload too large"));
        }
        if avail.len() < HEADER_LEN + len {
            return Ok(None);
        }
        let msg = Message::decode(ty, &avail[HEADER_LEN..HEADER_LEN + len])?;
        self.pos += HEADER_LEN + len;
        Ok(Some(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Id;

    /// Hands out the stream one byte per read call.
    struct Trickle(Vec<u8>, usize);
    impl Read for Trickle {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.1 >= self.0.len() {
                return Ok(0);
            }
            out[0] = self.0[self.1];
            self.1 += 1;
            Ok(1)
        }
    }

    #[test]
    fn every_message_round_trips() {
        let mut params = IsoTpParams::new(0x7E0, 0x7E8);
        params.fd = true;
        params.tx_dl = 64;
        params.padding = Some(0xAA);
        params.st_min = 0xF5;
        let msgs = vec![
            Message::OpenRaw {
                ifname: "vcan0".into(),
            },
            Message::OpenIsoTp {
                ifname: "can0".into(),
                params,
            },
            Message::SetFilters(vec![Filter::new(0x123, 0x7FF), Filter::ANY]),
            Message::SetFilters(vec![]),
            Message::SetOption {
                opt: SockOpt::RecvOwnMsgs,
                value: 1,
            },
            Message::SetOption {
                opt: SockOpt::JoinFilters,
                value: 1,
            },
            Message::Send(Frame::new(Id::Standard(0x123), &[1, 2, 3]).unwrap()),
            Message::Send(Frame::new_remote(Id::Extended(0x1234567), 4).unwrap()),
            Message::IsoTpSend(vec![9; 300]),
            Message::OpenBcm {
                ifname: "can0".into(),
            },
            Message::Bcm(BcmMessage {
                opcode: crate::bcm::op::TX_SETUP,
                flags: crate::bcm::flag::SETTIMER | crate::bcm::flag::STARTTIMER,
                count: 3,
                ival1_us: 1_000,
                ival2_us: 2_500_000,
                can_id: 0x123,
                frames: vec![
                    Frame::new(Id::Standard(0x123), &[1, 2, 3]).unwrap(),
                    Frame::new(Id::Standard(0x123), &[4]).unwrap(),
                ],
            }),
            Message::Bcm(BcmMessage::tx_delete(0x123, true)),
            Message::Refused {
                code: ErrorCode::InvalidRequest,
                message: "no such job".into(),
            },
            Message::BcmNotify {
                timestamp_ns: 1_700_000_000_123_456_789,
                msg: BcmMessage {
                    opcode: crate::bcm::op::RX_CHANGED,
                    can_id: 0x1ABCDEF0 | crate::CAN_EFF_FLAG,
                    frames: vec![Frame::new_fd(Id::Extended(0x1ABCDEF0), &[7; 12], true).unwrap()],
                    ..BcmMessage::default()
                },
            },
            Message::ListIfaces,
            Message::AddVcan {
                name: "vcan1".into(),
            },
            Message::Ok,
            Message::Error {
                code: ErrorCode::NoSuchInterface,
                message: "no such interface".into(),
            },
            Message::SendTo {
                ifindex: 3,
                frame: Frame::new(Id::Standard(0x123), &[1, 2, 3]).unwrap(),
            },
            Message::QueryLink,
            Message::IfaceRename {
                name: "can0".into(),
                new_name: "chassis".into(),
            },
            Message::Dropped { total: 1 << 40 },
            Message::OpenJ1939 {
                ifname: "can0".into(),
                addr: J1939Addr {
                    name: 0x8000_1234_5678_9ABC,
                    pgn: crate::j1939::NO_PGN,
                    addr: 0x80,
                },
            },
            Message::J1939Bind(J1939Addr::address(0x81)),
            Message::J1939Connect(J1939Addr::new(0x90, 0xEF00)),
            Message::J1939Filters(vec![
                J1939Filter::pgn(0xFEF1),
                J1939Filter {
                    name: 1,
                    name_mask: u64::MAX,
                    addr: 0x80,
                    addr_mask: 0xFF,
                    ..J1939Filter::default()
                },
            ]),
            Message::J1939Filters(vec![]),
            Message::J1939Send {
                dst: Some(J1939Addr::new(0x90, 0xEF00)),
                priority: Some(3),
                data: vec![5; 2000],
            },
            Message::J1939Send {
                dst: None,
                priority: None,
                data: vec![],
            },
            Message::SetOption {
                opt: SockOpt::J1939SendPrio,
                value: 2,
            },
            Message::J1939Data(J1939Message {
                timestamp_ns: 1_700_000_000_123_456_789,
                src: J1939Addr {
                    name: 7,
                    pgn: 0xFEF1,
                    addr: 0x80,
                },
                dst_addr: 0xFF,
                dst_name: 0,
                priority: 6,
                data: vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
            }),
            Message::J1939Notice(J1939Notice {
                kind: J1939NoticeKind::TxAborted,
                timestamp_ns: 99,
                pgn: 0xEF00,
                src_addr: 0x80,
                dst_addr: 0x90,
                src_name: 1,
                dst_name: 2,
                size: 1785,
                reason: crate::j1939::abort::TIMEOUT,
            }),
            Message::LinkState(LinkState::Up),
            Message::LinkState(LinkState::BusOff),
            Message::Frame {
                ifindex: 2,
                timestamp_ns: 1_700_000_000_123_456_789,
                own: true,
                frame: Frame::new_fd(Id::Extended(0x18DAF110), &[7; 48], true).unwrap(),
            },
            Message::IsoTpData {
                timestamp_ns: 42,
                data: vec![1, 2, 3],
            },
            Message::IsoTpTxDone,
            Message::IsoTpFailed {
                tx: true,
                reason: IsoTpFailure::Timeout,
            },
            Message::Ifaces(vec![
                IfaceInfo {
                    index: 1,
                    name: "vcan0".into(),
                    kind: "virtual".into(),
                    description: String::new(),
                    up: true,
                    config: None,
                },
                IfaceInfo {
                    index: 7,
                    name: "can0".into(),
                    kind: "peak_fd".into(),
                    description: "PCAN-USB FD".into(),
                    up: false,
                    config: Some(BusConfig {
                        sample_point: Some(800),
                        listen_only: true,
                        ..BusConfig::fd(500_000, 2_000_000)
                    }),
                },
            ]),
            Message::IfaceUp {
                name: "can0".into(),
                config: BusConfig::new(250_000),
            },
            Message::IfaceDown {
                name: "can0".into(),
            },
            Message::IfaceConfigure {
                name: "can0".into(),
                config: BusConfig::fd(500_000, 2_000_000),
            },
            Message::DelVcan {
                name: "vcan1".into(),
            },
            Message::Sync,
            Message::Frame {
                ifindex: 1,
                timestamp_ns: 7,
                own: false,
                frame: Frame::new_error(crate::err::BUSOFF, [0; 8]),
            },
        ];
        let mut bytes = Vec::new();
        for m in &msgs {
            m.encode(&mut bytes);
        }
        let mut reader = MessageReader::new(Trickle(bytes, 0));
        for m in &msgs {
            assert_eq!(&reader.read().unwrap(), m);
        }
        assert_eq!(
            reader.read().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn rejects_other_protocol_versions() {
        let mut bytes = Vec::new();
        Message::Ok.encode(&mut bytes);
        bytes[2] = 99;
        let mut reader = MessageReader::new(&bytes[..]);
        let e = reader.read().unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            version_mismatch(&e),
            Some(VersionMismatch {
                ours: PROTOCOL_VERSION,
                theirs: 99
            })
        );
        // Any other read error is not one.
        let mut reader = MessageReader::new(&bytes[..3]);
        assert_eq!(version_mismatch(&reader.read().unwrap_err()), None);
    }

    #[test]
    fn buffered_messages_come_out_without_reading() {
        let mut bytes = Vec::new();
        Message::Ok.encode(&mut bytes);
        Message::Sync.encode(&mut bytes);
        let half = bytes.len() - 3;
        let mut reader = MessageReader::new(&bytes[..half]);
        assert_eq!(reader.buffered().unwrap(), None);
        reader.fill().unwrap();
        assert_eq!(reader.buffered().unwrap(), Some(Message::Ok));
        // The second message is not all there yet.
        assert_eq!(reader.buffered().unwrap(), None);
        assert_eq!(
            reader.fill().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
