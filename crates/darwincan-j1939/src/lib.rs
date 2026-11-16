// SPDX-License-Identifier: MIT OR Apache-2.0

//! An SAE J1939 stack for one CAN interface, without I/O and without a
//! clock: the caller passes in the frames seen on the bus and the time, and
//! takes out the frames to send and what each socket is to be given.
//!
//! It does what Linux's `CAN_J1939` sockets do. A socket is bound to an
//! address or to a NAME, sends messages of any length to an address, a NAME
//! or everyone, and receives the messages meant for it. Messages of more
//! than eight bytes go through the transport protocols of SAE J1939-21:
//! the broadcast announce message (BAM) to everyone, a connection with
//! request-to-send and clear-to-send to one destination, and the extended
//! protocol of ISO 11783-3 above 1785 bytes. The stack follows the address
//! claims on the bus (SAE J1939-81), so that a NAME can stand for the
//! address it has claimed; claiming itself is left to the application, as
//! on Linux.
//!
//! The stack hears its own frames: the caller passes each frame back in
//! once it has been on the bus, marked as local. That is how a socket's
//! message reaches the other sockets on the same interface, and how two of
//! them hold a transport session with each other.

use std::collections::VecDeque;
use std::fmt;

use darwincan_core::j1939::{
    abort, pgn_is_pdu1, pgn_is_valid, DEFAULT_PRIORITY, FILTER_MAX, IDLE_ADDR, MAX_ETP_SIZE,
    MAX_TP_SIZE, MAX_UNICAST_ADDR, NO_ADDR, NO_NAME, PGN_ADDRESS_CLAIMED, PGN_MAX, PGN_REQUEST,
};
use darwincan_core::{
    Frame, Id, J1939Addr, J1939Filter, J1939Message, J1939Notice, J1939NoticeKind, CAN_EFF_MASK,
};

/// Names a socket to the stack. The caller chooses it.
pub type SocketId = u64;

const PGN_TP_CM: u32 = 0xEC00;
const PGN_TP_DT: u32 = 0xEB00;
const PGN_ETP_CM: u32 = 0xC800;
const PGN_ETP_DT: u32 = 0xC700;

const CM_RTS: u8 = 16;
const CM_CTS: u8 = 17;
const CM_EOMA: u8 = 19;
const CM_BAM: u8 = 32;
const CM_ABORT: u8 = 255;
const ETP_RTS: u8 = 20;
const ETP_CTS: u8 = 21;
const ETP_DPO: u8 = 22;
const ETP_EOMA: u8 = 23;

const MS: u64 = 1_000_000;
/// How long a receiver waits for the next data packet.
pub const T1_NS: u64 = 750 * MS;
/// How long a receiver waits for data after its clear-to-send.
pub const T2_NS: u64 = 1250 * MS;
/// How long a sender waits for a clear-to-send or the acknowledgement.
pub const T3_NS: u64 = 1250 * MS;
/// How long a sender waits after a clear-to-send that told it to hold.
pub const T4_NS: u64 = 1050 * MS;
/// The least time between two packets of a broadcast.
pub const BAM_GAP_NS: u64 = 50 * MS;
/// How long after an address claim the address counts as the NAME's.
pub const CLAIM_NS: u64 = 250 * MS;

/// The most packets asked for with one clear-to-send.
const MAX_BLOCK: u32 = 255;
/// How much may wait to be sent before a send is refused.
const MAX_QUEUED_FRAMES: usize = 1024;
const MAX_SESSIONS: usize = 256;
/// How often the receiver of one message asks for packets again before it
/// gives the message up with [`abort::RETRANSMIT_LIMIT`]. SAE J1939-21
/// (5.10.3.2 and 5.12.3) recommends no more than two such requests and
/// has the connection aborted with reason 5 after them.
pub const MAX_RETRANSMIT_REQUESTS: u8 = 2;

/// Where a frame passed to [`J1939::on_frame`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Another node on the bus, or another program on this machine.
    Bus,
    /// This stack sent it, for this socket if it was a socket's message.
    Local(Option<SocketId>),
}

/// Why a message was not taken for sending. Each stands for the error a
/// Linux J1939 socket gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// The socket is bound to no address and no NAME (`EBADFD`).
    NoSource,
    /// No destination was given and the socket is not connected
    /// (`EDESTADDRREQ`).
    NoDestination,
    /// Not a parameter group number (`EINVAL`).
    BadPgn,
    /// To everyone, from a socket that has not been allowed to
    /// (`EACCES`).
    Broadcast,
    /// A NAME, the socket's own or the destination's, has no claimed
    /// address (`EADDRNOTAVAIL`).
    AddressNotAvailable,
    /// Longer than the transport protocol for that destination carries
    /// (`EMSGSIZE`).
    TooBig,
    /// An address claim that does not carry the socket's NAME (`EPROTO`).
    NameMismatch,
    /// Too much is waiting to be sent already (`ENOBUFS`).
    Busy,
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SendError::NoSource => "the socket is bound to no address and cannot send",
            SendError::NoDestination => "no destination: the socket is not connected",
            SendError::BadPgn => "not a parameter group number",
            SendError::Broadcast => {
                "sending to everyone has to be allowed on the socket first (broadcast option)"
            }
            SendError::AddressNotAvailable => "the NAME has not claimed an address",
            SendError::TooBig => "the message is too long",
            SendError::NameMismatch => {
                "an address claim must carry the NAME the socket is bound to"
            }
            SendError::Busy => "too many messages are waiting to be sent",
        })
    }
}

impl std::error::Error for SendError {}

/// What the stack hands a socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Message(J1939Message),
    Notice(J1939Notice),
}

struct Socket {
    id: SocketId,
    bound: J1939Addr,
    connected: Option<J1939Addr>,
    filters: Vec<J1939Filter>,
    promisc: bool,
    broadcast: bool,
    priority: u8,
    notices: bool,
}

/// A NAME and the address it has claimed.
struct Claim {
    name: u64,
    addr: u8,
    /// From when the address counts as the NAME's.
    valid_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bam,
    Tp,
    Etp,
}

/// Which end of a connection a node is: the abort message says which of
/// the two its sender was (SAE J1939-21, figure 14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// It sent the request to send.
    Originator = 0,
    /// It sent the clear-to-send.
    Responder = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxState {
    /// Behind another message to the same destination.
    Queued,
    /// For a clear-to-send or the acknowledgement, until `deadline`.
    Waiting,
    /// Data packets `next..=last` are to go; a broadcast's next one not
    /// before `deadline`.
    Sending,
    /// A broadcast whose last packet has been handed over and not yet
    /// been seen on the bus.
    Finishing,
}

struct TxSession {
    socket: SocketId,
    sa: u8,
    da: u8,
    pgn: u32,
    kind: Kind,
    data: Vec<u8>,
    state: TxState,
    packets: u32,
    next: u32,
    last: u32,
    /// Extended protocol: the packet number the sequence numbers count
    /// from.
    offset: u32,
    /// The first packet of the block asked for last. The receiver may ask
    /// again for packets from there on, not for earlier ones.
    block: u32,
    /// The message's priority, which its transport frames carry too, so
    /// that it is seen on the bus and by the receiver. SAE J1939-21 gives
    /// 7 as the default for those frames; Linux sends them as here.
    priority: u8,
    deadline: u64,
}

impl TxSession {
    /// How many bytes the receiver has acknowledged: all of them once the
    /// message is `done`, otherwise what came before the block it asked
    /// for last.
    fn acked(&self, done: bool) -> u32 {
        if done {
            self.data.len() as u32
        } else {
            (((self.block - 1) * 7) as usize).min(self.data.len()) as u32
        }
    }
}

struct RxSession {
    sa: u8,
    da: u8,
    pgn: u32,
    kind: Kind,
    size: usize,
    packets: u32,
    data: Vec<u8>,
    /// The packet expected next, and the last one asked for so far.
    next: u32,
    last: u32,
    offset: u32,
    /// Extended protocol: a clear-to-send is out and the sender's offset
    /// message has not come.
    awaiting_offset: bool,
    /// The most packets the sender wants to be asked for at once.
    max_block: u32,
    /// A packet of the block being sent went missing: what follows it in
    /// the block is not kept, and at the block's end the packets are asked
    /// for again from the missing one.
    gap: bool,
    /// A clear-to-send is out and nothing has come of it yet.
    granted: bool,
    /// How often packets have been asked for again.
    retries: u8,
    /// The priority the sender's frames came with: the answers go out
    /// with the same.
    priority: u8,
    deadline: u64,
    /// The local socket whose message this is, which is not given it.
    exclude: Option<SocketId>,
}

struct Header {
    priority: u8,
    pgn: u32,
    da: u8,
    sa: u8,
}

fn parse(frame: &Frame) -> Option<Header> {
    if !frame.is_extended() || frame.is_remote() || frame.is_error() || frame.is_fd() {
        return None;
    }
    let id = frame.can_id() & CAN_EFF_MASK;
    let raw = (id >> 8) & PGN_MAX;
    let (pgn, da) = if pgn_is_pdu1(raw) {
        (raw & 0x3FF00, raw as u8)
    } else {
        (raw, NO_ADDR)
    };
    Some(Header {
        priority: (id >> 26) as u8 & 7,
        pgn,
        da,
        sa: id as u8,
    })
}

fn frame(priority: u8, pgn: u32, da: u8, sa: u8, data: &[u8]) -> Frame {
    let ps = if pgn_is_pdu1(pgn) {
        u32::from(da)
    } else {
        pgn & 0xFF
    };
    let id = u32::from(priority & 7) << 26 | (pgn & 0x3FF00) << 8 | ps << 8 | u32::from(sa);
    Frame::new(Id::Extended(id), data).expect("a 29-bit identifier and at most eight bytes")
}

fn pgn_bytes(pgn: u32) -> [u8; 3] {
    [pgn as u8, (pgn >> 8) as u8, (pgn >> 16) as u8]
}

fn pgn_from(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | u32::from(bytes[1]) << 8 | u32::from(bytes[2]) << 16
}

fn u24(bytes: &[u8]) -> u32 {
    pgn_from(bytes)
}

fn is_unicast(addr: u8) -> bool {
    addr <= MAX_UNICAST_ADDR
}

fn packets_for(size: usize) -> u32 {
    size.div_ceil(7) as u32
}

/// The stack for one interface. See the crate's description.
#[derive(Default)]
pub struct J1939 {
    sockets: Vec<Socket>,
    claims: Vec<Claim>,
    /// Frames ready to go, each with the socket whose message it is.
    control: VecDeque<(Frame, Option<SocketId>)>,
    tx: Vec<TxSession>,
    rx: Vec<RxSession>,
    out: VecDeque<(SocketId, Output)>,
    /// Which sending session is served next.
    turn: usize,
}

impl J1939 {
    pub fn new() -> J1939 {
        J1939::default()
    }

    /// True if no socket is open.
    pub fn is_empty(&self) -> bool {
        self.sockets.is_empty()
    }

    /// Whether a socket can be bound or connected to `addr`: its parameter
    /// group has to be one, or none.
    pub fn check_bind(addr: &J1939Addr) -> Result<(), &'static str> {
        if addr.pgn > PGN_MAX + 1 || (addr.pgn <= PGN_MAX && !pgn_is_valid(addr.pgn)) {
            return Err("not a parameter group number");
        }
        Ok(())
    }

    /// Open a socket bound to `bound`: its NAME or address is what the
    /// socket sends from and receives for, and its parameter group, if it
    /// names one, the only one the socket receives.
    pub fn open(&mut self, id: SocketId, bound: J1939Addr) -> Result<(), &'static str> {
        J1939::check_bind(&bound)?;
        self.sockets.push(Socket {
            id,
            bound,
            connected: None,
            filters: Vec::new(),
            promisc: false,
            broadcast: false,
            priority: DEFAULT_PRIORITY,
            notices: false,
        });
        Ok(())
    }

    fn socket_mut(&mut self, id: SocketId) -> Result<&mut Socket, &'static str> {
        self.sockets
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or("no such socket")
    }

    /// Bind an open socket anew, as an application does to move to
    /// another address.
    pub fn bind(&mut self, id: SocketId, bound: J1939Addr) -> Result<(), &'static str> {
        J1939::check_bind(&bound)?;
        self.socket_mut(id)?.bound = bound;
        Ok(())
    }

    /// Set the destination and parameter group of messages sent without
    /// one. The socket then receives from that NAME or address only.
    pub fn connect(&mut self, id: SocketId, peer: J1939Addr) -> Result<(), &'static str> {
        J1939::check_bind(&peer)?;
        self.socket_mut(id)?.connected = Some(peer);
        Ok(())
    }

    pub fn set_filters(
        &mut self,
        id: SocketId,
        filters: Vec<J1939Filter>,
    ) -> Result<(), &'static str> {
        if filters.len() > FILTER_MAX {
            return Err("too many filters");
        }
        self.socket_mut(id)?.filters = filters;
        Ok(())
    }

    /// Receive whatever is on the bus, whoever it is for.
    pub fn set_promisc(&mut self, id: SocketId, on: bool) -> Result<(), &'static str> {
        self.socket_mut(id)?.promisc = on;
        Ok(())
    }

    /// Allow sending to everyone and receiving what is sent to everyone.
    pub fn set_broadcast(&mut self, id: SocketId, on: bool) -> Result<(), &'static str> {
        self.socket_mut(id)?.broadcast = on;
        Ok(())
    }

    /// The priority of messages sent without one: 0 (highest) to 7.
    pub fn set_priority(&mut self, id: SocketId, priority: u32) -> Result<(), &'static str> {
        if priority > 7 {
            return Err("a priority is 0 to 7");
        }
        self.socket_mut(id)?.priority = priority as u8;
        Ok(())
    }

    /// Tell the socket of the progress of its messages.
    pub fn set_notices(&mut self, id: SocketId, on: bool) -> Result<(), &'static str> {
        self.socket_mut(id)?.notices = on;
        Ok(())
    }

    /// Close a socket. What it was sending is given up.
    pub fn close(&mut self, id: SocketId, now: u64) {
        let mut i = 0;
        while i < self.tx.len() {
            if self.tx[i].socket == id {
                let s = self.tx.remove(i);
                if s.kind != Kind::Bam && s.state != TxState::Queued {
                    self.push_abort(
                        s.kind,
                        Role::Originator,
                        s.priority,
                        s.sa,
                        s.da,
                        s.pgn,
                        abort::OTHER,
                    );
                }
            } else {
                i += 1;
            }
        }
        self.sockets.retain(|s| s.id != id);
        self.out.retain(|(to, _)| *to != id);
        // Sessions to an address nobody here has any more end quietly.
        let mut i = 0;
        while i < self.rx.len() {
            if self.rx[i].kind != Kind::Bam && !self.is_local(self.rx[i].da) {
                self.rx.remove(i);
            } else {
                i += 1;
            }
        }
        self.start_queued(now);
    }

    // Addresses and NAMEs.

    fn name_of(&self, addr: u8) -> u64 {
        self.claims
            .iter()
            .find(|c| c.addr == addr)
            .map_or(NO_NAME, |c| c.name)
    }

    /// The NAME that has `addr`, once its claim has stood long enough.
    fn settled_name_of(&self, addr: u8, now: u64) -> u64 {
        self.claims
            .iter()
            .find(|c| c.addr == addr && now >= c.valid_at)
            .map_or(NO_NAME, |c| c.name)
    }

    /// The address `name` has claimed, once the claim has stood long
    /// enough.
    fn addr_of(&self, name: u64, now: u64) -> Option<u8> {
        self.claims
            .iter()
            .find(|c| c.name == name && now >= c.valid_at)
            .map(|c| c.addr)
    }

    fn claimed(&mut self, sa: u8, name: u64, now: u64) {
        // The addresses that change hands: the one this NAME had, and
        // the one it takes from another NAME.
        let had = self.claims.iter().find(|c| c.name == name).map(|c| c.addr);
        let mut lost: Vec<(u64, u8)> = Vec::new();
        if let Some(addr) = had.filter(|addr| *addr != sa) {
            lost.push((name, addr));
        }
        self.claims.retain(|c| c.name != name);
        if is_unicast(sa) && name != NO_NAME {
            let holder = self.claims.iter().position(|c| c.addr == sa);
            // The lower NAME keeps a contested address.
            if holder.map_or(true, |i| self.claims[i].name > name) {
                if let Some(i) = holder {
                    lost.push((self.claims[i].name, sa));
                    self.claims.remove(i);
                }
                self.claims.push(Claim {
                    name,
                    addr: sa,
                    valid_at: now + CLAIM_NS,
                });
            }
        }
        // Otherwise it could not claim an address: it has none now.
        for (name, addr) in lost {
            if self.sockets.iter().any(|s| s.bound.name == name) {
                self.lost_address(name, addr, now);
            }
        }
    }

    /// A NAME that a socket here is bound to no longer has `addr`: what
    /// was being sent from it and received for it ends, without an abort
    /// message; the other end finds out by its timeout (SAE J1939-81,
    /// 5.9.10). A broadcast being received goes on.
    fn lost_address(&mut self, name: u64, addr: u8, now: u64) {
        self.control.retain(|(f, _)| {
            let h = parse(f);
            !h.is_some_and(|h| h.sa == addr && h.pgn != PGN_ADDRESS_CLAIMED)
        });
        let mut i = 0;
        while i < self.tx.len() {
            if self.tx[i].sa == addr {
                let s = self.tx.remove(i);
                let mut notice = self.make_notice(
                    J1939NoticeKind::TxAborted,
                    s.sa,
                    s.da,
                    s.pgn,
                    s.data.len(),
                    abort::OTHER,
                    now,
                );
                notice.acked = s.acked(false);
                self.notice_tx(s.socket, notice);
            } else {
                i += 1;
            }
        }
        let mut i = 0;
        while i < self.rx.len() {
            if self.rx[i].kind != Kind::Bam && self.rx[i].da == addr {
                let s = self.rx.remove(i);
                let mut notice = self.make_notice(
                    J1939NoticeKind::RxAborted,
                    s.sa,
                    s.da,
                    s.pgn,
                    s.size,
                    abort::OTHER,
                    now,
                );
                notice.acked = s.data.len() as u32;
                // The address is not theirs any more, so the sockets it
                // was for are found by the NAME.
                let to: Vec<SocketId> = self
                    .sockets
                    .iter()
                    .filter(|t| t.bound.name == name && t.notices && Some(t.id) != s.exclude)
                    .map(|t| t.id)
                    .collect();
                for id in to {
                    self.out.push_back((id, Output::Notice(notice)));
                }
            } else {
                i += 1;
            }
        }
        self.start_queued(now);
    }

    /// The address a socket has: the one its NAME has claimed, or the one
    /// it is bound to.
    fn socket_addr(&self, s: &Socket) -> u8 {
        if s.bound.name != NO_NAME {
            self.claims
                .iter()
                .find(|c| c.name == s.bound.name)
                .map_or(NO_ADDR, |c| c.addr)
        } else {
            s.bound.addr
        }
    }

    fn is_local(&self, addr: u8) -> bool {
        is_unicast(addr) && self.sockets.iter().any(|s| self.socket_addr(s) == addr)
    }

    fn wants(&self, s: &Socket, sa: u8, da: u8, pgn: u32, src_name: u64, dst_name: u64) -> bool {
        if !s.promisc {
            if s.bound.name != NO_NAME && dst_name != NO_NAME {
                if s.bound.name != dst_name {
                    return false;
                }
            } else if is_unicast(da) {
                if self.socket_addr(s) != da {
                    return false;
                }
            } else if !s.broadcast {
                return false;
            }
            if s.bound.pgn <= PGN_MAX && s.bound.pgn != pgn {
                return false;
            }
            if let Some(peer) = &s.connected {
                if peer.name != NO_NAME && src_name != NO_NAME {
                    if peer.name != src_name {
                        return false;
                    }
                } else if is_unicast(peer.addr) && peer.addr != sa {
                    return false;
                }
            }
        }
        s.filters.is_empty() || s.filters.iter().any(|f| f.matches(src_name, pgn, sa))
    }

    fn receivers(&self, sa: u8, da: u8, pgn: u32, exclude: Option<SocketId>) -> Vec<&Socket> {
        let src_name = self.name_of(sa);
        let dst_name = if is_unicast(da) {
            self.name_of(da)
        } else {
            NO_NAME
        };
        self.sockets
            .iter()
            .filter(|s| Some(s.id) != exclude && self.wants(s, sa, da, pgn, src_name, dst_name))
            .collect()
    }

    fn deliver(&mut self, h: &Header, data: &[u8], timestamp_ns: u64, exclude: Option<SocketId>) {
        let to: Vec<SocketId> = self
            .receivers(h.sa, h.da, h.pgn, exclude)
            .iter()
            .map(|s| s.id)
            .collect();
        if to.is_empty() {
            return;
        }
        // A NAME is given once its claim has stood long enough, as it is
        // on Linux; the claim itself says whose it is.
        let src_name = if h.pgn == PGN_ADDRESS_CLAIMED && data.len() == 8 {
            u64::from_le_bytes(data.try_into().expect("eight bytes"))
        } else {
            self.settled_name_of(h.sa, timestamp_ns)
        };
        let msg = J1939Message {
            timestamp_ns,
            src: J1939Addr {
                name: src_name,
                pgn: h.pgn,
                addr: h.sa,
            },
            dst_addr: h.da,
            dst_name: self.settled_name_of(h.da, timestamp_ns),
            priority: h.priority,
            data: data.to_vec(),
        };
        for id in to {
            self.out.push_back((id, Output::Message(msg.clone())));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn make_notice(
        &self,
        kind: J1939NoticeKind,
        sa: u8,
        da: u8,
        pgn: u32,
        size: usize,
        reason: u8,
        timestamp_ns: u64,
    ) -> J1939Notice {
        J1939Notice {
            kind,
            timestamp_ns,
            pgn,
            src_addr: sa,
            dst_addr: da,
            src_name: self.name_of(sa),
            dst_name: if is_unicast(da) {
                self.name_of(da)
            } else {
                NO_NAME
            },
            size: size as u32,
            acked: 0,
            reason,
        }
    }

    /// Tell the socket that sent a message how it went.
    fn notice_tx(&mut self, socket: SocketId, notice: J1939Notice) {
        if self.sockets.iter().any(|s| s.id == socket && s.notices) {
            self.out.push_back((socket, Output::Notice(notice)));
        }
    }

    /// Tell the sockets a message is for.
    fn notice_rx(&mut self, notice: J1939Notice, exclude: Option<SocketId>) {
        let to: Vec<SocketId> = self
            .receivers(notice.src_addr, notice.dst_addr, notice.pgn, exclude)
            .iter()
            .filter(|s| s.notices)
            .map(|s| s.id)
            .collect();
        for id in to {
            self.out.push_back((id, Output::Notice(notice)));
        }
    }

    // Sending.

    /// Take a message for sending from `socket`, to `dst` or, without
    /// one, to where the socket is connected. Returns once it is queued:
    /// how it goes is told in notices to a socket that asked for them.
    pub fn send(
        &mut self,
        socket: SocketId,
        dst: Option<J1939Addr>,
        priority: Option<u8>,
        data: Vec<u8>,
        now: u64,
    ) -> Result<(), SendError> {
        let s = self
            .sockets
            .iter()
            .find(|s| s.id == socket)
            .ok_or(SendError::NoSource)?;
        let dst = dst.or(s.connected).ok_or(SendError::NoDestination)?;
        let pgn = dst.pgn;
        if !pgn_is_valid(pgn) {
            return Err(SendError::BadPgn);
        }
        let sa = if s.bound.name != NO_NAME {
            if pgn == PGN_ADDRESS_CLAIMED {
                // The one message a NAME sends before it has an address:
                // from the address it is bound to, saying who it is.
                if data.len() != 8 || data[..] != s.bound.name.to_le_bytes() {
                    return Err(SendError::NameMismatch);
                }
                s.bound.addr
            } else if pgn == PGN_REQUEST
                && data[..] == pgn_bytes(PGN_ADDRESS_CLAIMED)
                && self.claims.iter().all(|c| c.name != s.bound.name)
            {
                // The other message a NAME may send without an address:
                // a request for the address claims of others, from the
                // null address (SAE J1939-81, 5.9.3).
                IDLE_ADDR
            } else {
                self.addr_of(s.bound.name, now)
                    .ok_or(SendError::AddressNotAvailable)?
            }
        } else if s.bound.addr == NO_ADDR {
            return Err(SendError::NoSource);
        } else {
            s.bound.addr
        };
        let da = if dst.name != NO_NAME {
            self.addr_of(dst.name, now)
                .ok_or(SendError::AddressNotAvailable)?
        } else {
            dst.addr
        };
        if !is_unicast(da) && !s.broadcast {
            return Err(SendError::Broadcast);
        }
        let priority = priority.unwrap_or(s.priority) & 7;
        if self.control.len() >= MAX_QUEUED_FRAMES || self.tx.len() >= MAX_SESSIONS {
            return Err(SendError::Busy);
        }
        if data.len() <= 8 {
            self.control
                .push_back((frame(priority, pgn, da, sa, &data), Some(socket)));
            return Ok(());
        }
        let kind = if !is_unicast(da) {
            if data.len() > MAX_TP_SIZE {
                return Err(SendError::TooBig);
            }
            Kind::Bam
        } else if data.len() <= MAX_TP_SIZE {
            Kind::Tp
        } else if data.len() <= MAX_ETP_SIZE {
            Kind::Etp
        } else {
            return Err(SendError::TooBig);
        };
        self.tx.push(TxSession {
            socket,
            sa,
            da: if kind == Kind::Bam { NO_ADDR } else { da },
            pgn,
            kind,
            packets: packets_for(data.len()),
            data,
            state: TxState::Queued,
            next: 1,
            last: 0,
            offset: 0,
            block: 1,
            priority,
            deadline: 0,
        });
        self.start_queued(now);
        Ok(())
    }

    /// Begin the queued messages whose destination has become free.
    fn start_queued(&mut self, now: u64) {
        for i in 0..self.tx.len() {
            if self.tx[i].state != TxState::Queued {
                continue;
            }
            let (sa, da) = (self.tx[i].sa, self.tx[i].da);
            let busy = self.tx.iter().enumerate().any(|(j, o)| {
                j != i && o.sa == sa && o.da == da && (o.state != TxState::Queued || j < i)
            });
            if busy {
                continue;
            }
            let s = &mut self.tx[i];
            let size = s.data.len();
            let pgn = pgn_bytes(s.pgn);
            let announce = match s.kind {
                Kind::Bam => {
                    s.state = TxState::Sending;
                    s.last = s.packets;
                    s.deadline = now + BAM_GAP_NS;
                    [
                        CM_BAM,
                        size as u8,
                        (size >> 8) as u8,
                        s.packets as u8,
                        0xFF,
                        pgn[0],
                        pgn[1],
                        pgn[2],
                    ]
                }
                Kind::Tp => {
                    s.state = TxState::Waiting;
                    s.deadline = now + T3_NS;
                    [
                        CM_RTS,
                        size as u8,
                        (size >> 8) as u8,
                        s.packets as u8,
                        0xFF,
                        pgn[0],
                        pgn[1],
                        pgn[2],
                    ]
                }
                Kind::Etp => {
                    s.state = TxState::Waiting;
                    s.deadline = now + T3_NS;
                    let size = (size as u32).to_le_bytes();
                    [
                        ETP_RTS, size[0], size[1], size[2], size[3], pgn[0], pgn[1], pgn[2],
                    ]
                }
            };
            let cm = if s.kind == Kind::Etp {
                PGN_ETP_CM
            } else {
                PGN_TP_CM
            };
            let (priority, da, sa, socket) = (s.priority, s.da, s.sa, s.socket);
            self.control
                .push_back((frame(priority, cm, da, sa, &announce), Some(socket)));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_abort(
        &mut self,
        kind: Kind,
        role: Role,
        priority: u8,
        from: u8,
        to: u8,
        pgn: u32,
        reason: u8,
    ) {
        let pgn = pgn_bytes(pgn);
        // The transport protocol's abort says in the low two bits of its
        // third byte which end sent it; the extended protocol's does not.
        let (cm, who) = if kind == Kind::Etp {
            (PGN_ETP_CM, 0xFF)
        } else {
            (PGN_TP_CM, 0xFC | role as u8)
        };
        let data = [CM_ABORT, reason, who, 0xFF, 0xFF, pgn[0], pgn[1], pgn[2]];
        self.control
            .push_back((frame(priority, cm, to, from, &data), None));
    }

    /// End a sending session and tell its socket.
    fn finish_tx(&mut self, i: usize, kind: J1939NoticeKind, reason: u8, now: u64) {
        let s = self.tx.remove(i);
        let mut notice = self.make_notice(kind, s.sa, s.da, s.pgn, s.data.len(), reason, now);
        notice.acked = s.acked(kind == J1939NoticeKind::TxDone);
        self.notice_tx(s.socket, notice);
        self.start_queued(now);
    }

    /// The next frame to put on the bus, if one is due, with the socket
    /// whose message it belongs to. Pass the frame back through
    /// [`on_frame`](Self::on_frame) as [`Origin::Local`] with that socket
    /// once it has been on the bus. A transport session's packets are
    /// made one at a time as they are asked for, so ask only for as many
    /// as the adapter may hold at once.
    pub fn poll_transmit(&mut self, now: u64) -> Option<(Frame, Option<SocketId>)> {
        if let Some(out) = self.control.pop_front() {
            return Some(out);
        }
        let n = self.tx.len();
        for k in 0..n {
            let i = (self.turn + k) % n;
            let s = &mut self.tx[i];
            if s.state != TxState::Sending || (s.kind == Kind::Bam && now < s.deadline) {
                continue;
            }
            let start = (s.next as usize - 1) * 7;
            let end = (start + 7).min(s.data.len());
            let mut data = [0xFFu8; 8];
            data[0] = (s.next - s.offset) as u8;
            data[1..1 + end - start].copy_from_slice(&s.data[start..end]);
            let dt = if s.kind == Kind::Etp {
                PGN_ETP_DT
            } else {
                PGN_TP_DT
            };
            let out = (frame(s.priority, dt, s.da, s.sa, &data), Some(s.socket));
            s.next += 1;
            self.turn = i + 1;
            if s.next > s.last {
                if s.kind == Kind::Bam {
                    s.state = TxState::Finishing;
                } else {
                    s.state = TxState::Waiting;
                    s.deadline = now + T3_NS;
                }
            } else if s.kind == Kind::Bam {
                // Until the frame is seen on the bus, from when the gap
                // really counts.
                s.deadline = now + BAM_GAP_NS;
            }
            return Some(out);
        }
        None
    }

    /// How many bytes of messages `socket` has given to be sent are
    /// still held: those of its messages that go in several frames and
    /// are waiting or under way.
    pub fn queued_bytes(&self, socket: SocketId) -> usize {
        self.tx
            .iter()
            .filter(|s| s.socket == socket)
            .map(|s| s.data.len())
            .sum()
    }

    /// The interface cannot send: give up everything that was to go out.
    pub fn link_down(&mut self, now: u64) {
        self.control.clear();
        while !self.tx.is_empty() {
            let s = self.tx.remove(0);
            let mut notice = self.make_notice(
                J1939NoticeKind::TxAborted,
                s.sa,
                s.da,
                s.pgn,
                s.data.len(),
                abort::OTHER,
                now,
            );
            notice.acked = s.acked(false);
            self.notice_tx(s.socket, notice);
        }
    }

    // Receiving.

    fn find_tx(&self, sa: u8, da: u8, etp: bool) -> Option<usize> {
        self.tx.iter().position(|s| {
            s.sa == sa
                && s.da == da
                && s.state != TxState::Queued
                && s.kind != Kind::Bam
                && (s.kind == Kind::Etp) == etp
        })
    }

    fn find_rx(&self, sa: u8, da: u8, kind: Kind) -> Option<usize> {
        self.rx
            .iter()
            .position(|s| s.sa == sa && s.da == da && s.kind == kind)
    }

    /// End a receiving session that failed, and tell who it was for.
    fn fail_rx(&mut self, i: usize, reason: u8, now: u64) {
        let s = self.rx.remove(i);
        if s.kind != Kind::Bam {
            self.push_abort(
                s.kind,
                Role::Responder,
                s.priority,
                s.da,
                s.sa,
                s.pgn,
                reason,
            );
        }
        let mut notice = self.make_notice(
            J1939NoticeKind::RxAborted,
            s.sa,
            s.da,
            s.pgn,
            s.size,
            reason,
            now,
        );
        notice.acked = s.data.len() as u32;
        self.notice_rx(notice, s.exclude);
    }

    /// Ask the sender for the next packets.
    fn grant(&mut self, i: usize, now: u64) {
        let s = &mut self.rx[i];
        let n = (s.packets - s.next + 1).min(s.max_block);
        s.last = s.next + n - 1;
        s.gap = false;
        s.granted = true;
        s.deadline = now + T2_NS;
        let pgn = pgn_bytes(s.pgn);
        let (cm, data) = if s.kind == Kind::Etp {
            s.awaiting_offset = true;
            let next = s.next.to_le_bytes();
            (
                PGN_ETP_CM,
                [
                    ETP_CTS, n as u8, next[0], next[1], next[2], pgn[0], pgn[1], pgn[2],
                ],
            )
        } else {
            (
                PGN_TP_CM,
                [
                    CM_CTS,
                    n as u8,
                    s.next as u8,
                    0xFF,
                    0xFF,
                    pgn[0],
                    pgn[1],
                    pgn[2],
                ],
            )
        };
        let (priority, to, from) = (s.priority, s.sa, s.da);
        self.control
            .push_back((frame(priority, cm, to, from, &data), None));
    }

    /// Packets of a connection went missing: ask for them again from the
    /// first one that is missing, unless that has been done too often.
    /// The sender takes a clear-to-send only once it has sent the block it
    /// was asked for, so this is called at the end of a block or when the
    /// data stops (SAE J1939-21, 5.10.3.2; AUTOSAR's specification of a
    /// J1939 transport layer says the same in SWS_J1939Tp_00222 and
    /// 00232).
    fn request_again(&mut self, i: usize, now: u64) {
        if self.rx[i].retries >= MAX_RETRANSMIT_REQUESTS {
            self.fail_rx(i, abort::RETRANSMIT_LIMIT, now);
            return;
        }
        self.rx[i].retries += 1;
        self.grant(i, now);
    }

    #[allow(clippy::too_many_arguments)]
    fn begin_rx(
        &mut self,
        h: &Header,
        kind: Kind,
        pgn: u32,
        size: usize,
        max_block: u32,
        exclude: Option<SocketId>,
        now: u64,
    ) {
        let packets = packets_for(size);
        self.rx.push(RxSession {
            sa: h.sa,
            da: h.da,
            pgn,
            kind,
            size,
            packets,
            // Not all at once: the size is only what the sender says.
            data: Vec::with_capacity(size.min(64 * 1024)),
            next: 1,
            last: packets,
            offset: 0,
            awaiting_offset: false,
            max_block: max_block.clamp(1, MAX_BLOCK),
            gap: false,
            granted: false,
            retries: 0,
            priority: h.priority,
            deadline: now + T1_NS,
            exclude,
        });
        let notice = self.make_notice(J1939NoticeKind::RxStarted, h.sa, h.da, pgn, size, 0, now);
        self.notice_rx(notice, exclude);
        if kind != Kind::Bam {
            self.grant(self.rx.len() - 1, now);
        }
    }

    /// A request to send arrived. `size` has been read from it.
    #[allow(clippy::too_many_arguments)]
    fn on_rts(
        &mut self,
        h: &Header,
        kind: Kind,
        pgn: u32,
        size: usize,
        max_block: u32,
        exclude: Option<SocketId>,
        now: u64,
    ) {
        if !self.is_local(h.da) {
            return;
        }
        if let Some(i) = self.find_rx(h.sa, h.da, kind) {
            if self.rx[i].pgn != pgn {
                self.push_abort(
                    kind,
                    Role::Responder,
                    h.priority,
                    h.da,
                    h.sa,
                    pgn,
                    abort::BUSY,
                );
                return;
            }
            // The sender starts over.
            self.rx.remove(i);
        }
        let fits = match kind {
            Kind::Etp => size > MAX_TP_SIZE && size <= MAX_ETP_SIZE,
            _ => size > 8 && size <= MAX_TP_SIZE,
        };
        if !fits {
            self.push_abort(
                kind,
                Role::Responder,
                h.priority,
                h.da,
                h.sa,
                pgn,
                abort::TOO_BIG,
            );
            return;
        }
        self.begin_rx(h, kind, pgn, size, max_block, exclude, now);
    }

    /// Give up a message being sent, and tell the receiver and the socket.
    fn abort_tx(&mut self, i: usize, reason: u8, now: u64) {
        let s = &self.tx[i];
        let (kind, priority, sa, da, pgn) = (s.kind, s.priority, s.sa, s.da, s.pgn);
        self.push_abort(kind, Role::Originator, priority, sa, da, pgn, reason);
        self.finish_tx(i, J1939NoticeKind::TxAborted, reason, now);
    }

    /// A clear-to-send for a message being sent: `count` packets from
    /// number `first`.
    fn on_cts(&mut self, i: usize, count: u32, first: u32, now: u64) {
        let s = &mut self.tx[i];
        let tp = s.kind == Kind::Tp;
        if s.state != TxState::Waiting {
            // While data is going out. The transport protocol has the
            // session given up (SAE J1939-21, 5.10.3.2); for the extended
            // protocol, whose standard was not at hand, it is passed over.
            if tp {
                self.abort_tx(i, abort::CTS_WHILE_SENDING, now);
            }
            return;
        }
        if count == 0 {
            // Hold: the receiver will say when.
            s.deadline = now + T4_NS;
            return;
        }
        // The receiver asks for what follows what has been sent or, to
        // have packets again, from somewhere in the block asked for last
        // (SAE J1939-21, 5.10.3.2: a clear-to-send confirms every packet
        // before the one it names, and packets can be had again within
        // the last set cleared to send). Not for packets beyond those,
        // and not for packets before that block (AUTOSAR
        // SWS_J1939Tp_00223; SAE J1939-82 tests for the same).
        let outside = tp && (first > s.next || first < s.block);
        if first == 0 || first > s.packets || outside {
            self.abort_tx(i, abort::OTHER, now);
            return;
        }
        let count = count.min(s.packets - first + 1).min(MAX_BLOCK);
        s.block = first;
        s.next = first;
        s.last = first + count - 1;
        s.state = TxState::Sending;
        if s.kind == Kind::Etp {
            s.offset = first - 1;
            let offset = s.offset.to_le_bytes();
            let pgn = pgn_bytes(s.pgn);
            let data = [
                ETP_DPO,
                count as u8,
                offset[0],
                offset[1],
                offset[2],
                pgn[0],
                pgn[1],
                pgn[2],
            ];
            let (priority, da, sa, socket) = (s.priority, s.da, s.sa, s.socket);
            self.control
                .push_back((frame(priority, PGN_ETP_CM, da, sa, &data), Some(socket)));
        }
    }

    fn on_cm(&mut self, h: &Header, d: &[u8], etp: bool, origin: Origin, now: u64) {
        let pgn = pgn_from(&d[5..8]);
        let exclude = match origin {
            Origin::Local(socket) => socket,
            Origin::Bus => None,
        };
        match (etp, d[0]) {
            (false, CM_BAM) => {
                let size = usize::from(d[1]) | usize::from(d[2]) << 8;
                if is_unicast(h.da) || size <= 8 || size > MAX_TP_SIZE {
                    return;
                }
                if u32::from(d[3]) != packets_for(size) {
                    return;
                }
                // A new announcement ends an unfinished one.
                if let Some(i) = self.find_rx(h.sa, h.da, Kind::Bam) {
                    self.rx.remove(i);
                }
                self.begin_rx(h, Kind::Bam, pgn, size, MAX_BLOCK, exclude, now);
            }
            (false, CM_RTS) => {
                let size = usize::from(d[1]) | usize::from(d[2]) << 8;
                self.on_rts(h, Kind::Tp, pgn, size, u32::from(d[4]), exclude, now);
            }
            (true, ETP_RTS) => {
                let size = u32::from_le_bytes([d[1], d[2], d[3], d[4]]) as usize;
                self.on_rts(h, Kind::Etp, pgn, size, MAX_BLOCK, exclude, now);
            }
            (false, CM_CTS) | (true, ETP_CTS) => {
                let Some(i) = self.find_tx(h.da, h.sa, etp) else {
                    return;
                };
                if self.tx[i].pgn != pgn {
                    return;
                }
                let first = if etp { u24(&d[2..5]) } else { u32::from(d[2]) };
                self.on_cts(i, u32::from(d[1]), first, now);
            }
            (false, CM_EOMA) | (true, ETP_EOMA) => {
                if let Some(i) = self.find_tx(h.da, h.sa, etp) {
                    let s = &self.tx[i];
                    // Before the last packet has gone it is passed over
                    // (SAE J1939-21, 5.10.3.3).
                    let early = !etp && (s.state != TxState::Waiting || s.next <= s.packets);
                    if s.pgn == pgn && !early {
                        self.finish_tx(i, J1939NoticeKind::TxDone, 0, now);
                    }
                }
            }
            (true, ETP_DPO) => {
                let Some(i) = self.find_rx(h.sa, h.da, Kind::Etp) else {
                    return;
                };
                let s = &mut self.rx[i];
                let (count, offset) = (u32::from(d[1]), u24(&d[2..5]));
                if !s.awaiting_offset
                    || s.pgn != pgn
                    || offset != s.next - 1
                    || count == 0
                    || s.next + count - 1 > s.last
                {
                    self.fail_rx(i, abort::OTHER, now);
                    return;
                }
                s.awaiting_offset = false;
                s.granted = false;
                s.offset = offset;
                s.last = s.next + count - 1;
                s.deadline = now + T1_NS;
                let (sa, da, pgn, size, exclude) = (s.sa, s.da, s.pgn, s.size, s.exclude);
                let have = s.data.len() as u32;
                let mut notice =
                    self.make_notice(J1939NoticeKind::RxOffset, sa, da, pgn, size, 0, now);
                notice.acked = have;
                self.notice_rx(notice, exclude);
            }
            (_, CM_ABORT) => {
                let kind = if etp { Kind::Etp } else { Kind::Tp };
                // Which of the two sessions there can be between two
                // nodes it ends: the one its sender is the receiver of,
                // the one it is the sender of, or, said by an older
                // implementation that does not tell, both.
                let (from_originator, from_responder) = match (etp, d[2] & 3) {
                    (false, 0) => (true, false),
                    (false, 1) => (false, true),
                    _ => (true, true),
                };
                if let Some(i) = self.find_tx(h.da, h.sa, etp).filter(|_| from_responder) {
                    if self.tx[i].pgn == pgn {
                        self.finish_tx(i, J1939NoticeKind::TxAborted, d[1], now);
                    }
                }
                if let Some(i) = self.find_rx(h.sa, h.da, kind).filter(|_| from_originator) {
                    if self.rx[i].pgn == pgn {
                        let s = self.rx.remove(i);
                        let mut notice = self.make_notice(
                            J1939NoticeKind::RxAborted,
                            s.sa,
                            s.da,
                            s.pgn,
                            s.size,
                            d[1],
                            now,
                        );
                        notice.acked = s.data.len() as u32;
                        self.notice_rx(notice, s.exclude);
                    }
                }
            }
            _ => {}
        }
    }

    fn on_dt(&mut self, h: &Header, d: &[u8], etp: bool, timestamp_ns: u64, now: u64) {
        let kind = if etp {
            Kind::Etp
        } else if is_unicast(h.da) {
            Kind::Tp
        } else {
            Kind::Bam
        };
        let Some(i) = self.find_rx(h.sa, h.da, kind) else {
            return;
        };
        let s = &mut self.rx[i];
        let packet = s.offset + u32::from(d[0]);
        if s.awaiting_offset {
            self.fail_rx(i, abort::UNEXPECTED_DATA, now);
            return;
        }
        if packet > s.last {
            self.fail_rx(i, abort::BAD_SEQUENCE, now);
            return;
        }
        if !s.gap && packet < s.next {
            self.fail_rx(i, abort::DUPLICATE_SEQUENCE, now);
            return;
        }
        if s.gap || packet != s.next {
            // A packet is missing. A broadcast cannot be repaired; a
            // connection is, by asking again once the sender has sent the
            // rest of the block, which is not kept (SAE J1939-21,
            // 5.10.3.2). The extended protocol's standard was not at
            // hand, so its sessions are given up as they were before.
            if s.kind != Kind::Tp {
                self.fail_rx(i, abort::BAD_SEQUENCE, now);
                return;
            }
            s.gap = true;
            s.granted = false;
            s.deadline = now + T1_NS;
            if packet == s.last {
                self.request_again(i, now);
            }
            return;
        }
        s.granted = false;
        let take = (s.size - s.data.len()).min(7);
        s.data.extend_from_slice(&d[1..1 + take]);
        s.next += 1;
        if s.next > s.packets {
            let s = self.rx.remove(i);
            if s.kind != Kind::Bam {
                let pgn = pgn_bytes(s.pgn);
                let (cm, data) = if s.kind == Kind::Etp {
                    let size = (s.size as u32).to_le_bytes();
                    (
                        PGN_ETP_CM,
                        [
                            ETP_EOMA, size[0], size[1], size[2], size[3], pgn[0], pgn[1], pgn[2],
                        ],
                    )
                } else {
                    (
                        PGN_TP_CM,
                        [
                            CM_EOMA,
                            s.size as u8,
                            (s.size >> 8) as u8,
                            s.packets as u8,
                            0xFF,
                            pgn[0],
                            pgn[1],
                            pgn[2],
                        ],
                    )
                };
                self.control
                    .push_back((frame(s.priority, cm, s.sa, s.da, &data), None));
            }
            let whole = Header {
                priority: h.priority,
                pgn: s.pgn,
                da: s.da,
                sa: s.sa,
            };
            self.deliver(&whole, &s.data, timestamp_ns, s.exclude);
        } else if s.next > s.last {
            self.grant(i, now);
        } else {
            s.deadline = now + T1_NS;
        }
    }

    /// A frame was on the bus at `timestamp_ns`.
    pub fn on_frame(&mut self, frame: &Frame, timestamp_ns: u64, now: u64, origin: Origin) {
        let Some(h) = parse(frame) else {
            return;
        };
        let d = frame.data();
        match h.pgn {
            PGN_TP_CM | PGN_ETP_CM if d.len() == 8 => {
                self.on_cm(&h, d, h.pgn == PGN_ETP_CM, origin, now);
            }
            PGN_TP_DT | PGN_ETP_DT if d.len() == 8 => {
                if let Origin::Local(_) = origin {
                    // One of our broadcast's packets: the next is due a
                    // gap after this one was on the bus, and with the
                    // last one the broadcast is out.
                    if let Some(i) = self.tx.iter().position(|s| {
                        s.kind == Kind::Bam && s.sa == h.sa && s.state != TxState::Queued
                    }) {
                        let s = &mut self.tx[i];
                        if s.state == TxState::Finishing && u32::from(d[0]) == s.packets {
                            self.finish_tx(i, J1939NoticeKind::TxDone, 0, timestamp_ns);
                        } else {
                            s.deadline = timestamp_ns + BAM_GAP_NS;
                        }
                    }
                }
                self.on_dt(&h, d, h.pgn == PGN_ETP_DT, timestamp_ns, now);
            }
            PGN_TP_CM | PGN_ETP_CM | PGN_TP_DT | PGN_ETP_DT => {}
            _ => {
                if h.pgn == PGN_ADDRESS_CLAIMED && d.len() == 8 {
                    let name = u64::from_le_bytes(d.try_into().expect("eight bytes"));
                    self.claimed(h.sa, name, now);
                }
                let exclude = match origin {
                    Origin::Local(Some(socket)) => {
                        let mut notice = self.make_notice(
                            J1939NoticeKind::TxDone,
                            h.sa,
                            h.da,
                            h.pgn,
                            d.len(),
                            0,
                            timestamp_ns,
                        );
                        notice.acked = d.len() as u32;
                        self.notice_tx(socket, notice);
                        Some(socket)
                    }
                    _ => None,
                };
                self.deliver(&h, d, timestamp_ns, exclude);
            }
        }
        if let (Origin::Local(_), PGN_TP_CM, true) = (origin, h.pgn, d.len() == 8) {
            // The announcement of our broadcast: its first packet is due
            // a gap after it.
            if d[0] == CM_BAM {
                if let Some(s) = self
                    .tx
                    .iter_mut()
                    .find(|s| s.kind == Kind::Bam && s.sa == h.sa && s.state == TxState::Sending)
                {
                    s.deadline = timestamp_ns + BAM_GAP_NS;
                }
            }
        }
    }

    /// Act on the timers that have run out.
    pub fn poll(&mut self, now: u64) {
        let mut i = 0;
        while i < self.tx.len() {
            let s = &self.tx[i];
            if s.state == TxState::Waiting && now >= s.deadline {
                let (kind, priority, sa, da, pgn) = (s.kind, s.priority, s.sa, s.da, s.pgn);
                self.push_abort(
                    kind,
                    Role::Originator,
                    priority,
                    sa,
                    da,
                    pgn,
                    abort::TIMEOUT,
                );
                self.finish_tx(i, J1939NoticeKind::TxAborted, abort::TIMEOUT, now);
                // Starting the next in line may have changed the list.
                i = 0;
            } else {
                i += 1;
            }
        }
        let mut i = 0;
        while i < self.rx.len() {
            let s = &self.rx[i];
            if now < s.deadline {
                i += 1;
                continue;
            }
            let before = self.rx.len();
            if s.kind == Kind::Tp && !s.granted {
                // The data stopped within a block: its end was lost. The
                // receiver may ask again or give up (SAE J1939-21,
                // 5.10.3.2 and figure C5).
                self.request_again(i, now);
            } else {
                self.fail_rx(i, abort::TIMEOUT, now);
            }
            if self.rx.len() == before {
                i += 1;
            }
        }
    }

    /// When [`poll`](Self::poll) or [`poll_transmit`](Self::poll_transmit)
    /// next has something to do that it does not have now.
    pub fn next_deadline(&self) -> Option<u64> {
        let tx = self
            .tx
            .iter()
            .filter(|s| {
                s.state == TxState::Waiting || (s.state == TxState::Sending && s.kind == Kind::Bam)
            })
            .map(|s| s.deadline);
        let rx = self.rx.iter().map(|s| s.deadline);
        tx.chain(rx).min()
    }

    /// True if [`poll_transmit`](Self::poll_transmit) has a frame at `now`.
    pub fn wants_transmit(&self, now: u64) -> bool {
        !self.control.is_empty()
            || self
                .tx
                .iter()
                .any(|s| s.state == TxState::Sending && (s.kind != Kind::Bam || now >= s.deadline))
    }

    /// The next message or notice for a socket.
    pub fn poll_output(&mut self) -> Option<(SocketId, Output)> {
        self.out.pop_front()
    }
}

#[cfg(test)]
mod tests;
