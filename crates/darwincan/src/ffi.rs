// SPDX-License-Identifier: MIT OR Apache-2.0

//! The C API: the functions and types declared in `include/darwincan.h`.
//!
//! Frames, filters and flags are laid out exactly as in Linux's
//! `<linux/can.h>`, so frame handling ported from SocketCAN code stays as it
//! is; opening, sending and receiving go through the `dcan_*` functions
//! here, because macOS has no CAN sockets to bind.
//!
//! Every function returns 0 or a positive count on success and a negative
//! `DCAN_ERROR_*` code on failure (or a null handle from the `open`
//! functions); [`dcan_last_error`] then gives the message for the calling
//! thread. Keep this file and the header in step: the end-to-end tests
//! compile a C program against both.

#![allow(non_camel_case_types, clippy::missing_safety_doc)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::{c_char, c_int, c_long, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use darwincan_core::wire::PROTOCOL_VERSION;

use crate::bcm::{flag, MAX_FRAMES};
use crate::{
    default_socket_path, BcmMessage, BcmSocket, BusConfig, Control, Error, ErrorCode, Filter,
    Frame, IsoTpEvent, IsoTpFailure, IsoTpParams, IsoTpSocket, J1939Addr, J1939Event, J1939Filter,
    J1939Socket, RawSocket, RxFrame, CANFD_FDF,
};

pub const DCAN_ERROR_DAEMON_UNAVAILABLE: c_int = -1;
pub const DCAN_ERROR_IO: c_int = -2;
pub const DCAN_ERROR_INTERRUPTED: c_int = -3;
pub const DCAN_ERROR_INVALID_ARGUMENT: c_int = -4;
pub const DCAN_ERROR_VERSION_MISMATCH: c_int = -5;
pub const DCAN_ERROR_PROTOCOL: c_int = -6;
pub const DCAN_ERROR_ISOTP_TX: c_int = -7;
pub const DCAN_ERROR_ISOTP_RX: c_int = -8;
pub const DCAN_ERROR_INTERNAL: c_int = -9;
/// The daemon's own refusals are -100 minus its error code.
pub const DCAN_ERROR_NO_SUCH_INTERFACE: c_int = -101;
pub const DCAN_ERROR_INVALID_REQUEST: c_int = -102;
pub const DCAN_ERROR_ALREADY_EXISTS: c_int = -103;
pub const DCAN_ERROR_UNSUPPORTED: c_int = -104;
pub const DCAN_ERROR_TRANSMIT_FAILED: c_int = -105;
pub const DCAN_ERROR_INTERFACE_DOWN: c_int = -106;
pub const DCAN_ERROR_INTERFACE_REMOVED: c_int = -107;
pub const DCAN_ERROR_BUSY: c_int = -108;
pub const DCAN_ERROR_ADDRESS_NOT_AVAILABLE: c_int = -109;
pub const DCAN_ERROR_NOT_PERMITTED: c_int = -110;
pub const DCAN_ERROR_TOO_BIG: c_int = -111;
pub const DCAN_ERROR_DAEMON_OTHER: c_int = -199;

/// Size of a `struct can_frame`: what a receive call returns for a classic
/// frame.
pub const CAN_MTU: c_int = 16;
/// Size of a `struct canfd_frame`: what it returns for a CAN FD frame.
pub const CANFD_MTU: c_int = 72;

/// `struct can_frame` of `<linux/can.h>`.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct can_frame {
    pub can_id: u32,
    pub len: u8,
    pub pad: u8,
    pub res0: u8,
    pub len8_dlc: u8,
    pub data: [u8; 8],
}

/// `struct canfd_frame` of `<linux/can.h>`.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct canfd_frame {
    pub can_id: u32,
    pub len: u8,
    pub flags: u8,
    pub res0: u8,
    pub res1: u8,
    pub data: [u8; 64],
}

/// `struct can_filter` of `<linux/can.h>`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct can_filter {
    pub can_id: u32,
    pub can_mask: u32,
}

/// Set in [`dcan_rx_info::flags`] when the receiving socket sent the frame.
pub const DCAN_RX_OWN: u32 = 1;

/// What the daemon knows about a received frame besides its contents.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct dcan_rx_info {
    pub timestamp_ns: u64,
    pub flags: u32,
    /// Frames the socket has lost so far, as `SO_RXQ_OVFL` counts them.
    pub dropped: u32,
    /// The interface the frame was on.
    pub ifindex: u32,
    pub reserved: u32,
}

pub const DCAN_ISOTP_FD: u32 = 1;
pub const DCAN_ISOTP_BRS: u32 = 2;
pub const DCAN_ISOTP_PADDING: u32 = 4;

/// How an ISO-TP channel is set up; see [`IsoTpParams`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct dcan_isotp_options {
    pub tx_id: u32,
    pub rx_id: u32,
    pub flags: u32,
    pub tx_dl: u8,
    pub pad_byte: u8,
    pub block_size: u8,
    pub st_min: u8,
    pub n_bs_ms: u32,
    pub n_cr_ms: u32,
    pub max_rx_len: u32,
    pub reserved: [u32; 4],
}

/// How a hardware interface joins the bus; see [`BusConfig`]. A zero in
/// `data_bitrate` means classic CAN only, in a sample point the default.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct dcan_bus_config {
    pub bitrate: u32,
    pub data_bitrate: u32,
    pub sample_point: u16,
    pub data_sample_point: u16,
    pub listen_only: u8,
    pub reserved: [u8; 7],
}

/// One of the daemon's interfaces.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct dcan_interface {
    pub name: [c_char; 16],
    pub kind: [c_char; 16],
    pub description: [c_char; 128],
    pub up: u8,
    pub has_config: u8,
    pub reserved: [u8; 2],
    pub index: u32,
    pub config: dcan_bus_config,
}

/// `struct bcm_timeval` of `<linux/can/bcm.h>`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct bcm_timeval {
    pub tv_sec: c_long,
    pub tv_usec: c_long,
}

/// `struct bcm_msg_head` of `<linux/can/bcm.h>`, without the frames that
/// follow it: `nframes` of `can_frame`, or of `canfd_frame` when `flags`
/// has `CAN_FD_FRAME`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct bcm_msg_head {
    pub opcode: u32,
    pub flags: u32,
    pub count: u32,
    pub ival1: bcm_timeval,
    pub ival2: bcm_timeval,
    pub can_id: u32,
    pub nframes: u32,
}

/// `struct j1939_filter` of `<linux/can/j1939.h>`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct j1939_filter {
    pub name: u64,
    pub name_mask: u64,
    pub pgn: u32,
    pub pgn_mask: u32,
    pub addr: u8,
    pub addr_mask: u8,
}

/// One end of a J1939 message: the `can_addr.j1939` part of Linux's
/// `struct sockaddr_can`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct dcan_j1939_addr {
    pub name: u64,
    pub pgn: u32,
    pub addr: u8,
    pub reserved: [u8; 3],
}

/// What comes with a J1939 message or notice.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct dcan_j1939_info {
    pub timestamp_ns: u64,
    pub src: dcan_j1939_addr,
    pub dst_name: u64,
    pub dst_addr: u8,
    pub priority: u8,
    pub notice: u8,
    pub reason: u8,
    pub size: u32,
}

/// A J1939 socket on one interface.
pub struct dcan_j1939 {
    sock: J1939Socket,
    timeout: Mutex<Option<c_int>>,
}

/// A broadcast manager on one interface.
pub struct dcan_bcm {
    sock: BcmSocket,
    timeout: Mutex<Option<c_int>>,
}

/// A raw CAN socket on one interface.
pub struct dcan_raw {
    sock: RawSocket,
    timeout: Mutex<Option<c_int>>,
}

/// An ISO-TP channel on one interface.
///
/// Threads may receive and send on it at once. The daemon reports
/// messages received and the end of a send in one stream, so whichever
/// thread is reading files what arrives for the thread it belongs to.
pub struct dcan_isotp {
    sock: IsoTpSocket,
    state: Mutex<IsoTpState>,
    changed: Condvar,
    /// Held for the length of a send: the channel carries one at a time.
    sending: Mutex<()>,
}

/// The most messages kept for a program that sends on a channel and
/// never receives; beyond it the oldest go.
const ISOTP_HELD_MAX: usize = 1024;

/// The most filters a socket takes, raw (`CAN_RAW_FILTER_MAX`) or J1939
/// (`J1939_FILTER_MAX`). Checked before the caller's array is read.
const FILTER_MAX: usize = 512;

impl IsoTpState {
    fn keep(&mut self, received: Result<(Vec<u8>, u64), IsoTpFailure>) {
        if self.received.len() >= ISOTP_HELD_MAX {
            self.received.pop_front();
        }
        self.received.push_back(received);
    }
}

#[derive(Default)]
struct IsoTpState {
    /// Messages and receive failures not yet handed to a receive call.
    received: VecDeque<Result<(Vec<u8>, u64), IsoTpFailure>>,
    /// How the send in progress ended, until its caller takes it.
    sent: Option<Result<(), IsoTpFailure>>,
    /// A thread is blocked reading the socket.
    reading: bool,
    timeout: Option<c_int>,
}

enum Wanted {
    Message,
    Sent,
}

enum Taken {
    Message(Result<(Vec<u8>, u64), IsoTpFailure>),
    Sent(Result<(), IsoTpFailure>),
}

impl dcan_isotp {
    /// Wait for a received message or for the end of the send, at most
    /// `timeout_ms` (negative: without limit; 0: only what has arrived).
    fn wait(&self, wanted: Wanted, timeout_ms: c_int) -> crate::Result<Option<Taken>> {
        let deadline =
            (timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(timeout_ms as u64));
        let mut state = self.state.lock().unwrap();
        loop {
            match wanted {
                Wanted::Message => {
                    if let Some(message) = state.received.pop_front() {
                        if state.received.is_empty() {
                            self.sock.hold(false);
                        }
                        return Ok(Some(Taken::Message(message)));
                    }
                }
                Wanted::Sent => {
                    if let Some(result) = state.sent.take() {
                        return Ok(Some(Taken::Sent(result)));
                    }
                }
            }
            // Milliseconds left to wait: 0 for none, -1 for no limit.
            let left = match deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(None);
                    }
                    (left.as_millis() as c_int).max(1)
                }
                None => timeout_ms.clamp(-1, 0),
            };
            if state.reading {
                // Another thread is reading; it files what arrives.
                state = match left {
                    0 => return Ok(None),
                    -1 => self.changed.wait(state).unwrap(),
                    ms => {
                        self.changed
                            .wait_timeout(state, Duration::from_millis(ms as u64))
                            .unwrap()
                            .0
                    }
                };
                continue;
            }
            state.reading = true;
            let event = if left == 0 {
                drop(state);
                self.sock.try_recv_event()
            } else {
                let set =
                    apply_timeout(&mut state.timeout, left, |t| self.sock.set_read_timeout(t));
                drop(state);
                match set.and_then(|()| self.sock.recv_event()) {
                    Ok(event) => Ok(Some(event)),
                    Err(e) if e.is_timeout() => Ok(None),
                    Err(e) => Err(e),
                }
            };
            state = self.state.lock().unwrap();
            state.reading = false;
            self.changed.notify_all();
            match event? {
                Some(IsoTpEvent::Data { data, timestamp_ns }) => {
                    state.keep(Ok((data, timestamp_ns)));
                    self.sock.hold(true);
                }
                Some(IsoTpEvent::RxFailed(reason)) => {
                    state.keep(Err(reason));
                    self.sock.hold(true);
                }
                Some(IsoTpEvent::TxDone) => state.sent = Some(Ok(())),
                Some(IsoTpEvent::TxFailed(reason)) => state.sent = Some(Err(reason)),
                None if left == 0 => return Ok(None),
                None => {}
            }
        }
    }
}

/// A management connection to the daemon.
pub struct dcan_control {
    control: Control,
}

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
    static LAST_ISOTP_FAILURE: Cell<c_int> = const { Cell::new(0) };
}

fn set_error(message: &str) {
    let text = CString::new(message.replace('\0', " ")).unwrap_or_default();
    LAST_ERROR.with(|last| *last.borrow_mut() = text);
}

fn fail(code: c_int, message: &str) -> c_int {
    set_error(message);
    code
}

fn invalid(message: &str) -> c_int {
    fail(DCAN_ERROR_INVALID_ARGUMENT, message)
}

fn error_code(e: &Error) -> c_int {
    match e {
        Error::DaemonUnavailable(_) => DCAN_ERROR_DAEMON_UNAVAILABLE,
        Error::Io(e) if e.kind() == std::io::ErrorKind::Interrupted => DCAN_ERROR_INTERRUPTED,
        Error::Io(_) => DCAN_ERROR_IO,
        Error::Daemon { code, .. } => match code {
            ErrorCode::Other => DCAN_ERROR_DAEMON_OTHER,
            code => -100 - (*code as c_int),
        },
        Error::IsoTp { tx: true, .. } => DCAN_ERROR_ISOTP_TX,
        Error::IsoTp { tx: false, .. } => DCAN_ERROR_ISOTP_RX,
        Error::Protocol(_) => DCAN_ERROR_PROTOCOL,
        Error::VersionMismatch { .. } => DCAN_ERROR_VERSION_MISMATCH,
    }
}

fn failed(e: &Error) -> c_int {
    if let Error::IsoTp { reason, .. } = e {
        LAST_ISOTP_FAILURE.with(|last| last.set(*reason as c_int));
    }
    fail(error_code(e), &e.to_string())
}

fn done(result: crate::Result<()>) -> c_int {
    match result {
        Ok(()) => 0,
        Err(e) => failed(&e),
    }
}

/// Run a function's body so that a panic becomes an error code instead of
/// unwinding into C.
fn guard(body: impl FnOnce() -> c_int) -> c_int {
    catch_unwind(AssertUnwindSafe(body))
        .unwrap_or_else(|_| fail(DCAN_ERROR_INTERNAL, "internal error in libdarwincan"))
}

fn guard_ptr<T>(body: impl FnOnce() -> Result<T, c_int>) -> *mut T {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(value)) => Box::into_raw(Box::new(value)),
        Ok(Err(_)) => ptr::null_mut(),
        Err(_) => {
            fail(DCAN_ERROR_INTERNAL, "internal error in libdarwincan");
            ptr::null_mut()
        }
    }
}

unsafe fn text<'a>(s: *const c_char, what: &str) -> Result<&'a str, c_int> {
    if s.is_null() {
        return Err(invalid(&format!("{what} is a null pointer")));
    }
    CStr::from_ptr(s)
        .to_str()
        .map_err(|_| invalid(&format!("{what} is not valid UTF-8")))
}

/// The daemon socket to use: `path`, or the default when it is null.
unsafe fn socket_path(path: *const c_char) -> Result<PathBuf, c_int> {
    if path.is_null() {
        return Ok(default_socket_path());
    }
    text(path, "socket path").map(|p| Path::new(p).to_path_buf())
}

fn timeout_of(ms: c_int) -> Option<Duration> {
    (ms >= 0).then(|| Duration::from_millis(ms as u64))
}

/// Give the socket the read timeout a call asked for, if it differs from
/// the one it has.
fn apply_timeout(
    current: &mut Option<c_int>,
    ms: c_int,
    set: impl FnOnce(Option<Duration>) -> crate::Result<()>,
) -> crate::Result<()> {
    let ms = ms.max(-1);
    if *current != Some(ms) {
        set(timeout_of(ms))?;
        *current = Some(ms);
    }
    Ok(())
}

fn copy_text(out: &mut [c_char], text: &str) {
    let room = out.len() - 1;
    let mut end = text.len().min(room);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    for (o, b) in out.iter_mut().zip(text.as_bytes()[..end].iter()) {
        *o = *b as c_char;
    }
    out[end] = 0;
}

fn frame_to_c(frame: &Frame, out: &mut canfd_frame) -> c_int {
    *out = canfd_frame {
        can_id: frame.can_id(),
        len: frame.len() as u8,
        flags: frame.flags(),
        res0: 0,
        res1: 0,
        data: [0; 64],
    };
    out.data[..frame.data().len()].copy_from_slice(frame.data());
    if frame.is_fd() {
        CANFD_MTU
    } else {
        CAN_MTU
    }
}

fn config_from_c(c: &dcan_bus_config) -> BusConfig {
    BusConfig {
        bitrate: c.bitrate,
        data_bitrate: (c.data_bitrate != 0).then_some(c.data_bitrate),
        sample_point: (c.sample_point != 0).then_some(c.sample_point),
        data_sample_point: (c.data_sample_point != 0).then_some(c.data_sample_point),
        listen_only: c.listen_only != 0,
    }
}

fn config_to_c(c: &BusConfig) -> dcan_bus_config {
    dcan_bus_config {
        bitrate: c.bitrate,
        data_bitrate: c.data_bitrate.unwrap_or(0),
        sample_point: c.sample_point.unwrap_or(0),
        data_sample_point: c.data_sample_point.unwrap_or(0),
        listen_only: u8::from(c.listen_only),
        reserved: [0; 7],
    }
}

/// The message of the last failed call on this thread. Valid until the
/// next failed call on the same thread.
#[no_mangle]
pub extern "C" fn dcan_last_error() -> *const c_char {
    LAST_ERROR.with(|last| last.borrow().as_ptr())
}

/// Why the last ISO-TP transfer failed on this thread: one of the
/// `DCAN_ISOTP_FAILURE_*` values, after a call returned
/// `DCAN_ERROR_ISOTP_TX` or `DCAN_ERROR_ISOTP_RX`.
#[no_mangle]
pub extern "C" fn dcan_isotp_last_failure() -> c_int {
    LAST_ISOTP_FAILURE.with(Cell::get)
}

/// The library's version, as in `DCAN_VERSION`.
#[no_mangle]
pub extern "C" fn dcan_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// The version of the daemon protocol the library speaks. A daemon that
/// speaks another is refused with `DCAN_ERROR_VERSION_MISMATCH`.
#[no_mangle]
pub extern "C" fn dcan_protocol_version() -> c_int {
    c_int::from(PROTOCOL_VERSION)
}

/// Open a raw socket on `ifname` through the daemon at its default place.
#[no_mangle]
pub unsafe extern "C" fn dcan_raw_open(ifname: *const c_char) -> *mut dcan_raw {
    dcan_raw_open_at(ptr::null(), ifname)
}

/// Open a raw socket through the daemon listening at `socket_path` (null
/// for the default).
#[no_mangle]
pub unsafe extern "C" fn dcan_raw_open_at(
    socket_path_: *const c_char,
    ifname: *const c_char,
) -> *mut dcan_raw {
    guard_ptr(|| {
        let path = socket_path(socket_path_)?;
        let ifname = text(ifname, "interface name")?;
        let sock = RawSocket::open_at(&path, ifname).map_err(|e| failed(&e))?;
        sock.set_interruptible(true);
        Ok(dcan_raw {
            sock,
            timeout: Mutex::new(None),
        })
    })
}

/// Close a raw socket. No other thread may be using it.
#[no_mangle]
pub unsafe extern "C" fn dcan_raw_close(sock: *mut dcan_raw) {
    if !sock.is_null() {
        drop(Box::from_raw(sock));
    }
}

unsafe fn raw<'a>(sock: *const dcan_raw) -> Result<&'a dcan_raw, c_int> {
    sock.as_ref()
        .ok_or_else(|| invalid("socket is a null pointer"))
}

/// Send a classic CAN frame (data or remote).
#[no_mangle]
pub unsafe extern "C" fn dcan_raw_send(sock: *mut dcan_raw, frame: *const can_frame) -> c_int {
    guard(|| match (raw(sock), classic(frame)) {
        (Ok(sock), Ok(frame)) => done(sock.sock.send(&frame)),
        (Err(code), _) | (_, Err(code)) => code,
    })
}

/// Send a CAN FD frame; `flags` may carry `CANFD_BRS` and `CANFD_ESI`.
#[no_mangle]
pub unsafe extern "C" fn dcan_raw_send_fd(sock: *mut dcan_raw, frame: *const canfd_frame) -> c_int {
    guard(|| match (raw(sock), fd(frame)) {
        (Ok(sock), Ok(frame)) => done(sock.sock.send(&frame)),
        (Err(code), _) | (_, Err(code)) => code,
    })
}

unsafe fn classic(frame: *const can_frame) -> Result<Frame, c_int> {
    let frame = frame
        .as_ref()
        .ok_or_else(|| invalid("frame is a null pointer"))?;
    let data = frame
        .data
        .get(..usize::from(frame.len))
        .ok_or_else(|| invalid("a classic CAN frame holds at most 8 bytes"))?;
    Frame::from_raw(frame.can_id, 0, data).map_err(|e| invalid(&e.to_string()))
}

unsafe fn fd(frame: *const canfd_frame) -> Result<Frame, c_int> {
    let frame = frame
        .as_ref()
        .ok_or_else(|| invalid("frame is a null pointer"))?;
    let data = frame
        .data
        .get(..usize::from(frame.len))
        .ok_or_else(|| invalid("a CAN FD frame holds at most 64 bytes"))?;
    Frame::from_raw(frame.can_id, frame.flags | CANFD_FDF, data)
        .map_err(|e| invalid(&e.to_string()))
}

/// Send a classic CAN frame on the interface with index `ifindex`: the
/// way to send on a socket open on every interface.
#[no_mangle]
pub unsafe extern "C" fn dcan_raw_sendto(
    sock: *mut dcan_raw,
    frame: *const can_frame,
    ifindex: u32,
) -> c_int {
    guard(|| match (raw(sock), classic(frame)) {
        (Ok(sock), Ok(frame)) => done(sock.sock.send_to(ifindex, &frame)),
        (Err(code), _) | (_, Err(code)) => code,
    })
}

/// Send a CAN FD frame on the interface with index `ifindex`.
#[no_mangle]
pub unsafe extern "C" fn dcan_raw_sendto_fd(
    sock: *mut dcan_raw,
    frame: *const canfd_frame,
    ifindex: u32,
) -> c_int {
    guard(|| match (raw(sock), fd(frame)) {
        (Ok(sock), Ok(frame)) => done(sock.sock.send_to(ifindex, &frame)),
        (Err(code), _) | (_, Err(code)) => code,
    })
}

/// The index of the interface called `name`, or 0 if there is none (or
/// the daemon cannot be asked), as `if_nametoindex`.
#[no_mangle]
pub unsafe extern "C" fn dcan_if_nametoindex(name: *const c_char) -> u32 {
    let found = catch_unwind(AssertUnwindSafe(|| {
        let name = text(name, "interface name").ok()?;
        let list = Control::connect()
            .and_then(|c| c.interfaces())
            .map_err(|e| failed(&e))
            .ok()?;
        let index = list.iter().find(|i| i.name == name).map(|i| i.index);
        if index.is_none() {
            fail(
                DCAN_ERROR_NO_SUCH_INTERFACE,
                &format!("no such interface: {name}"),
            );
        }
        index
    }));
    found.ok().flatten().unwrap_or(0)
}

/// The name of the interface with index `ifindex`, written to the 16
/// bytes at `name` and returned; null if there is none, as
/// `if_indextoname`.
#[no_mangle]
pub unsafe extern "C" fn dcan_if_indextoname(ifindex: u32, name: *mut c_char) -> *mut c_char {
    let found = catch_unwind(AssertUnwindSafe(|| {
        if name.is_null() {
            invalid("name is a null pointer");
            return None;
        }
        let list = Control::connect()
            .and_then(|c| c.interfaces())
            .map_err(|e| failed(&e))
            .ok()?;
        let Some(iface) = list.iter().find(|i| i.index == ifindex) else {
            fail(
                DCAN_ERROR_NO_SUCH_INTERFACE,
                &format!("no interface has index {ifindex}"),
            );
            return None;
        };
        copy_text(std::slice::from_raw_parts_mut(name, 16), &iface.name);
        Some(())
    }));
    match found {
        Ok(Some(())) => name,
        _ => ptr::null_mut(),
    }
}

/// Receive a frame, waiting at most `timeout_ms` (negative: without
/// limit; 0: only what has arrived).
///
/// Returns `CAN_MTU` for a classic frame, whose first 16 bytes in `frame`
/// are a `struct can_frame`; `CANFD_MTU` for a CAN FD frame; 0 if none
/// came in time. `info` may be null.
#[no_mangle]
pub unsafe extern "C" fn dcan_raw_recv(
    sock: *mut dcan_raw,
    frame: *mut canfd_frame,
    info: *mut dcan_rx_info,
    timeout_ms: c_int,
) -> c_int {
    guard(|| {
        let sock = match raw(sock) {
            Ok(sock) => sock,
            Err(code) => return code,
        };
        let Some(out) = frame.as_mut() else {
            return invalid("frame is a null pointer");
        };
        let received = if timeout_ms == 0 {
            sock.sock.try_recv()
        } else {
            let mut current = sock.timeout.lock().unwrap();
            let set = apply_timeout(&mut current, timeout_ms, |t| sock.sock.set_read_timeout(t));
            drop(current);
            match set.and_then(|()| sock.sock.recv()) {
                Ok(rx) => Ok(Some(rx)),
                Err(e) if e.is_timeout() => Ok(None),
                Err(e) => Err(e),
            }
        };
        match received {
            Ok(Some(RxFrame {
                frame,
                timestamp_ns,
                own,
                ifindex,
                dropped,
            })) => {
                if let Some(info) = info.as_mut() {
                    *info = dcan_rx_info {
                        timestamp_ns,
                        flags: if own { DCAN_RX_OWN } else { 0 },
                        // The count wraps, as on Linux.
                        dropped: dropped as u32,
                        ifindex,
                        reserved: 0,
                    };
                }
                frame_to_c(&frame, out)
            }
            Ok(None) => 0,
            Err(e) => failed(&e),
        }
    })
}

/// Replace the receive filters; with `count` 0 nothing is received.
#[no_mangle]
pub unsafe extern "C" fn dcan_raw_set_filters(
    sock: *mut dcan_raw,
    filters: *const can_filter,
    count: usize,
) -> c_int {
    guard(|| {
        let sock = match raw(sock) {
            Ok(sock) => sock,
            Err(code) => return code,
        };
        if filters.is_null() && count != 0 {
            return invalid("filters is a null pointer");
        }
        if count > FILTER_MAX {
            return invalid("a socket takes at most 512 filters");
        }
        let filters: Vec<Filter> = (0..count)
            .map(|i| {
                let f = filters.add(i).read_unaligned();
                Filter::new(f.can_id, f.can_mask)
            })
            .collect();
        done(sock.sock.set_filters(&filters))
    })
}

/// Which classes of error frames to receive (`CAN_ERR_*`; 0 for none).
#[no_mangle]
pub unsafe extern "C" fn dcan_raw_set_err_filter(sock: *mut dcan_raw, mask: u32) -> c_int {
    guard(|| match raw(sock) {
        Ok(sock) => done(sock.sock.set_error_filter(mask)),
        Err(code) => code,
    })
}

/// Whether a frame must pass every filter, not just one (default: no).
#[no_mangle]
pub unsafe extern "C" fn dcan_raw_set_join_filters(sock: *mut dcan_raw, on: c_int) -> c_int {
    guard(|| match raw(sock) {
        Ok(sock) => done(sock.sock.set_join_filters(on != 0)),
        Err(code) => code,
    })
}

/// Whether frames sent here reach other local sockets (default: yes).
#[no_mangle]
pub unsafe extern "C" fn dcan_raw_set_loopback(sock: *mut dcan_raw, on: c_int) -> c_int {
    guard(|| match raw(sock) {
        Ok(sock) => done(sock.sock.set_loopback(on != 0)),
        Err(code) => code,
    })
}

/// Whether this socket receives the frames it sent (default: no).
#[no_mangle]
pub unsafe extern "C" fn dcan_raw_set_recv_own_msgs(sock: *mut dcan_raw, on: c_int) -> c_int {
    guard(|| match raw(sock) {
        Ok(sock) => done(sock.sock.set_recv_own_msgs(on != 0)),
        Err(code) => code,
    })
}

/// Wait until the daemon has handled every frame sent on this socket.
#[no_mangle]
pub unsafe extern "C" fn dcan_raw_flush(sock: *mut dcan_raw) -> c_int {
    guard(|| match raw(sock) {
        Ok(sock) => done(sock.sock.flush()),
        Err(code) => code,
    })
}

/// A file descriptor to wait on with `poll`, `select` or `kqueue`, and
/// for nothing else: readable exactly when a receive with a timeout of 0
/// would return something.
#[no_mangle]
pub unsafe extern "C" fn dcan_raw_fileno(sock: *const dcan_raw) -> c_int {
    use std::os::fd::AsRawFd;
    guard(|| match raw(sock) {
        Ok(sock) => sock.sock.as_raw_fd(),
        Err(code) => code,
    })
}

/// Fill `options` with the defaults for a classic CAN channel between
/// `tx_id` and `rx_id`.
#[no_mangle]
pub unsafe extern "C" fn dcan_isotp_options_init(
    options: *mut dcan_isotp_options,
    tx_id: u32,
    rx_id: u32,
) {
    let Some(options) = options.as_mut() else {
        return;
    };
    let p = IsoTpParams::new(tx_id, rx_id);
    *options = dcan_isotp_options {
        tx_id,
        rx_id,
        flags: 0,
        tx_dl: p.tx_dl,
        pad_byte: 0,
        block_size: p.block_size,
        st_min: p.st_min,
        n_bs_ms: p.n_bs_ms,
        n_cr_ms: p.n_cr_ms,
        max_rx_len: p.max_rx_len,
        reserved: [0; 4],
    };
}

/// Open an ISO-TP channel on `ifname` through the daemon at its default
/// place.
#[no_mangle]
pub unsafe extern "C" fn dcan_isotp_open(
    ifname: *const c_char,
    options: *const dcan_isotp_options,
) -> *mut dcan_isotp {
    dcan_isotp_open_at(ptr::null(), ifname, options)
}

/// Open an ISO-TP channel through the daemon listening at `socket_path`
/// (null for the default).
#[no_mangle]
pub unsafe extern "C" fn dcan_isotp_open_at(
    socket_path_: *const c_char,
    ifname: *const c_char,
    options: *const dcan_isotp_options,
) -> *mut dcan_isotp {
    guard_ptr(|| {
        let path = socket_path(socket_path_)?;
        let ifname = text(ifname, "interface name")?;
        let o = options
            .as_ref()
            .ok_or_else(|| invalid("options is a null pointer"))?;
        let known = DCAN_ISOTP_FD | DCAN_ISOTP_BRS | DCAN_ISOTP_PADDING;
        if o.flags & !known != 0 {
            return Err(invalid(
                "options.flags has a bit this library does not know",
            ));
        }
        let params = IsoTpParams {
            tx_id: o.tx_id,
            rx_id: o.rx_id,
            fd: o.flags & DCAN_ISOTP_FD != 0,
            brs: o.flags & DCAN_ISOTP_BRS != 0,
            tx_dl: o.tx_dl,
            padding: (o.flags & DCAN_ISOTP_PADDING != 0).then_some(o.pad_byte),
            block_size: o.block_size,
            st_min: o.st_min,
            n_bs_ms: o.n_bs_ms,
            n_cr_ms: o.n_cr_ms,
            max_rx_len: o.max_rx_len.min(c_int::MAX as u32),
        };
        params.validate().map_err(invalid)?;
        let sock = IsoTpSocket::open_at(&path, ifname, params).map_err(|e| failed(&e))?;
        sock.set_interruptible(true);
        Ok(dcan_isotp {
            sock,
            state: Mutex::new(IsoTpState::default()),
            changed: Condvar::new(),
            sending: Mutex::new(()),
        })
    })
}

/// Close an ISO-TP channel. No other thread may be using it.
#[no_mangle]
pub unsafe extern "C" fn dcan_isotp_close(sock: *mut dcan_isotp) {
    if !sock.is_null() {
        drop(Box::from_raw(sock));
    }
}

unsafe fn isotp<'a>(sock: *const dcan_isotp) -> Result<&'a dcan_isotp, c_int> {
    sock.as_ref()
        .ok_or_else(|| invalid("socket is a null pointer"))
}

/// Send one message and wait until the peer has all of it, or the
/// transfer has failed. Another thread may be receiving meanwhile; a
/// second send waits for the first.
#[no_mangle]
pub unsafe extern "C" fn dcan_isotp_send(
    sock: *mut dcan_isotp,
    data: *const u8,
    len: usize,
) -> c_int {
    guard(|| {
        let sock = match isotp(sock) {
            Ok(sock) => sock,
            Err(code) => return code,
        };
        if data.is_null() || len == 0 {
            return invalid("an ISO-TP message has at least one byte");
        }
        let data = std::slice::from_raw_parts(data, len);
        let _sending = sock.sending.lock().unwrap();
        sock.state.lock().unwrap().sent = None;
        if let Err(e) = sock.sock.send(data) {
            return failed(&e);
        }
        loop {
            match sock.wait(Wanted::Sent, -1) {
                Ok(Some(Taken::Sent(Ok(())))) => return 0,
                Ok(Some(Taken::Sent(Err(reason)))) => {
                    return failed(&Error::IsoTp { tx: true, reason })
                }
                // The transfer goes on in the daemon whatever happens
                // here, so a signal does not end the wait for it.
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Ok(_) => {}
                Err(e) => return failed(&e),
            }
        }
    })
}

/// Receive one message into `buffer`, waiting at most `timeout_ms`
/// (negative: without limit; 0: only what has arrived).
///
/// Returns the message's length, which is more than `capacity` if it did
/// not fit and was cut short, or 0 if none came in time. `timestamp_ns`
/// may be null.
#[no_mangle]
pub unsafe extern "C" fn dcan_isotp_recv(
    sock: *mut dcan_isotp,
    buffer: *mut u8,
    capacity: usize,
    timestamp_ns: *mut u64,
    timeout_ms: c_int,
) -> c_int {
    guard(|| {
        let sock = match isotp(sock) {
            Ok(sock) => sock,
            Err(code) => return code,
        };
        if buffer.is_null() && capacity != 0 {
            return invalid("buffer is a null pointer");
        }
        match sock.wait(Wanted::Message, timeout_ms) {
            Ok(Some(Taken::Message(Ok((data, at))))) => {
                let n = data.len().min(capacity);
                if n > 0 {
                    ptr::copy_nonoverlapping(data.as_ptr(), buffer, n);
                }
                if let Some(out) = timestamp_ns.as_mut() {
                    *out = at;
                }
                data.len() as c_int
            }
            Ok(Some(Taken::Message(Err(reason)))) => failed(&Error::IsoTp { tx: false, reason }),
            Ok(_) => 0,
            Err(e) => failed(&e),
        }
    })
}

/// A file descriptor to wait on with `poll`, `select` or `kqueue`, and
/// for nothing else: readable exactly when a receive with a timeout of 0
/// would return something.
#[no_mangle]
pub unsafe extern "C" fn dcan_isotp_fileno(sock: *const dcan_isotp) -> c_int {
    use std::os::fd::AsRawFd;
    guard(|| match isotp(sock) {
        Ok(sock) => sock.sock.as_raw_fd(),
        Err(code) => code,
    })
}

fn interval_us(t: &bcm_timeval) -> Option<u64> {
    let (sec, usec) = (
        u64::try_from(t.tv_sec).ok()?,
        u64::try_from(t.tv_usec).ok()?,
    );
    sec.checked_mul(1_000_000)?.checked_add(usec)
}

fn interval_to_c(us: u64) -> bcm_timeval {
    bcm_timeval {
        tv_sec: (us / 1_000_000) as c_long,
        tv_usec: (us % 1_000_000) as c_long,
    }
}

/// The bytes one frame takes after a `bcm_msg_head` with these flags.
fn bcm_frame_size(flags: u32) -> usize {
    if flags & flag::CAN_FD_FRAME != 0 {
        CANFD_MTU as usize
    } else {
        CAN_MTU as usize
    }
}

/// Open a broadcast manager on `ifname` through the daemon at its default
/// place.
#[no_mangle]
pub unsafe extern "C" fn dcan_bcm_open(ifname: *const c_char) -> *mut dcan_bcm {
    dcan_bcm_open_at(ptr::null(), ifname)
}

/// Open a broadcast manager through the daemon listening at `socket_path`
/// (null for the default).
#[no_mangle]
pub unsafe extern "C" fn dcan_bcm_open_at(
    socket_path_: *const c_char,
    ifname: *const c_char,
) -> *mut dcan_bcm {
    guard_ptr(|| {
        let path = socket_path(socket_path_)?;
        let ifname = text(ifname, "interface name")?;
        let sock = BcmSocket::open_at(&path, ifname).map_err(|e| failed(&e))?;
        sock.set_interruptible(true);
        Ok(dcan_bcm {
            sock,
            timeout: Mutex::new(None),
        })
    })
}

/// Close a broadcast manager; its jobs end. No other thread may be using
/// it.
#[no_mangle]
pub unsafe extern "C" fn dcan_bcm_close(sock: *mut dcan_bcm) {
    if !sock.is_null() {
        drop(Box::from_raw(sock));
    }
}

/// Make a request: `size` bytes at `msg`, a `bcm_msg_head` followed by
/// its `nframes` frames, as written to a `CAN_BCM` socket. Returns once
/// the daemon has carried it out or refused it.
#[no_mangle]
pub unsafe extern "C" fn dcan_bcm_send(
    sock: *mut dcan_bcm,
    msg: *const bcm_msg_head,
    size: usize,
) -> c_int {
    guard(|| {
        let Some(sock) = sock.as_ref() else {
            return invalid("socket is a null pointer");
        };
        if msg.is_null() || size < std::mem::size_of::<bcm_msg_head>() {
            return invalid("the message is shorter than a struct bcm_msg_head");
        }
        let head = msg.read_unaligned();
        let count = head.nframes as usize;
        let each = bcm_frame_size(head.flags);
        if count > MAX_FRAMES + 1 || size != std::mem::size_of::<bcm_msg_head>() + count * each {
            return invalid("the message's size does not fit its nframes and flags");
        }
        let (Some(ival1_us), Some(ival2_us)) = (interval_us(&head.ival1), interval_us(&head.ival2))
        else {
            return invalid("an interval is negative or too long");
        };
        let first = msg.add(1).cast::<u8>();
        let mut frames = Vec::with_capacity(count);
        for i in 0..count {
            let at = first.add(i * each);
            let frame = if each == CANFD_MTU as usize {
                let f = at.cast::<canfd_frame>().read_unaligned();
                f.data
                    .get(..usize::from(f.len))
                    .ok_or(crate::FrameError::DataTooLong)
                    .and_then(|data| Frame::from_raw(f.can_id, f.flags | CANFD_FDF, data))
            } else {
                let f = at.cast::<can_frame>().read_unaligned();
                f.data
                    .get(..usize::from(f.len))
                    .ok_or(crate::FrameError::DataTooLong)
                    .and_then(|data| Frame::from_raw(f.can_id, 0, data))
            };
            match frame {
                Ok(frame) => frames.push(frame),
                Err(e) => return invalid(&format!("frame {i} of the message: {e}")),
            }
        }
        done(sock.sock.send(&BcmMessage {
            opcode: head.opcode,
            flags: head.flags,
            count: head.count,
            ival1_us,
            ival2_us,
            can_id: head.can_id,
            frames,
        }))
    })
}

/// Receive a report into the `capacity` bytes at `msg`, waiting at most
/// `timeout_ms` (negative: without limit; 0: only what has arrived).
///
/// Returns the report's size in bytes, a `bcm_msg_head` and its frames,
/// which is more than `capacity` if it did not fit and frames were left
/// out; 0 if none came in time. `timestamp_ns` may be null.
#[no_mangle]
pub unsafe extern "C" fn dcan_bcm_recv(
    sock: *mut dcan_bcm,
    msg: *mut bcm_msg_head,
    capacity: usize,
    timestamp_ns: *mut u64,
    timeout_ms: c_int,
) -> c_int {
    guard(|| {
        let Some(sock) = sock.as_ref() else {
            return invalid("socket is a null pointer");
        };
        let head_size = std::mem::size_of::<bcm_msg_head>();
        if msg.is_null() || capacity < head_size {
            return invalid("there must be room for a struct bcm_msg_head");
        }
        let received = if timeout_ms == 0 {
            sock.sock.try_recv()
        } else {
            let mut current = sock.timeout.lock().unwrap();
            let set = apply_timeout(&mut current, timeout_ms, |t| sock.sock.set_read_timeout(t));
            drop(current);
            match set.and_then(|()| sock.sock.recv()) {
                Ok(report) => Ok(Some(report)),
                Err(e) if e.is_timeout() => Ok(None),
                Err(e) => Err(e),
            }
        };
        let report = match received {
            Ok(Some(report)) => report,
            Ok(None) => return 0,
            Err(e) => return failed(&e),
        };
        let m = &report.msg;
        msg.write_unaligned(bcm_msg_head {
            opcode: m.opcode,
            flags: m.flags,
            count: m.count,
            ival1: interval_to_c(m.ival1_us),
            ival2: interval_to_c(m.ival2_us),
            can_id: m.can_id,
            nframes: m.frames.len() as u32,
        });
        let each = bcm_frame_size(m.flags);
        let first = msg.add(1).cast::<u8>();
        for (i, frame) in m.frames.iter().enumerate() {
            if head_size + (i + 1) * each > capacity {
                break;
            }
            let mut whole = canfd_frame {
                can_id: 0,
                len: 0,
                flags: 0,
                res0: 0,
                res1: 0,
                data: [0; 64],
            };
            frame_to_c(frame, &mut whole);
            // A classic frame is the start of the CAN FD layout.
            ptr::copy_nonoverlapping(
                (&whole as *const canfd_frame).cast::<u8>(),
                first.add(i * each),
                each,
            );
        }
        if let Some(out) = timestamp_ns.as_mut() {
            *out = report.timestamp_ns;
        }
        (head_size + m.frames.len() * each) as c_int
    })
}

/// A file descriptor to wait on with `poll`, `select` or `kqueue`, and
/// for nothing else: readable exactly when a receive with a timeout of 0
/// would return something.
#[no_mangle]
pub unsafe extern "C" fn dcan_bcm_fileno(sock: *const dcan_bcm) -> c_int {
    use std::os::fd::AsRawFd;
    guard(|| match sock.as_ref() {
        Some(sock) => sock.sock.as_raw_fd(),
        None => invalid("socket is a null pointer"),
    })
}

unsafe fn j1939_addr(addr: *const dcan_j1939_addr) -> J1939Addr {
    match addr.as_ref() {
        Some(a) => J1939Addr {
            name: a.name,
            pgn: a.pgn,
            addr: a.addr,
        },
        None => J1939Addr::NONE,
    }
}

fn j1939_addr_to_c(a: &J1939Addr) -> dcan_j1939_addr {
    dcan_j1939_addr {
        name: a.name,
        pgn: a.pgn,
        addr: a.addr,
        reserved: [0; 3],
    }
}

/// Open a J1939 socket on `ifname`, bound to `bind` (null: to nothing,
/// to listen only), through the daemon at its default place.
#[no_mangle]
pub unsafe extern "C" fn dcan_j1939_open(
    ifname: *const c_char,
    bind: *const dcan_j1939_addr,
) -> *mut dcan_j1939 {
    dcan_j1939_open_at(ptr::null(), ifname, bind)
}

/// Open a J1939 socket through the daemon listening at `socket_path`
/// (null for the default).
#[no_mangle]
pub unsafe extern "C" fn dcan_j1939_open_at(
    socket_path_: *const c_char,
    ifname: *const c_char,
    bind: *const dcan_j1939_addr,
) -> *mut dcan_j1939 {
    guard_ptr(|| {
        let path = socket_path(socket_path_)?;
        let ifname = text(ifname, "interface name")?;
        let sock = J1939Socket::open_at(&path, ifname, j1939_addr(bind)).map_err(|e| failed(&e))?;
        sock.set_interruptible(true);
        Ok(dcan_j1939 {
            sock,
            timeout: Mutex::new(None),
        })
    })
}

/// Close a J1939 socket; what it was sending is given up. No other thread
/// may be using it.
#[no_mangle]
pub unsafe extern "C" fn dcan_j1939_close(sock: *mut dcan_j1939) {
    if !sock.is_null() {
        drop(Box::from_raw(sock));
    }
}

unsafe fn j1939<'a>(sock: *const dcan_j1939) -> Result<&'a dcan_j1939, c_int> {
    sock.as_ref()
        .ok_or_else(|| invalid("socket is a null pointer"))
}

/// Bind the socket anew, as `bind` on a `CAN_J1939` socket.
#[no_mangle]
pub unsafe extern "C" fn dcan_j1939_bind(
    sock: *mut dcan_j1939,
    addr: *const dcan_j1939_addr,
) -> c_int {
    guard(|| match j1939(sock) {
        Ok(sock) => done(sock.sock.bind(j1939_addr(addr))),
        Err(code) => code,
    })
}

/// Set the destination of `dcan_j1939_send`, as `connect`.
#[no_mangle]
pub unsafe extern "C" fn dcan_j1939_connect(
    sock: *mut dcan_j1939,
    addr: *const dcan_j1939_addr,
) -> c_int {
    guard(|| match j1939(sock) {
        Ok(_) if addr.is_null() => invalid("address is a null pointer"),
        Ok(sock) => done(sock.sock.connect(j1939_addr(addr))),
        Err(code) => code,
    })
}

unsafe fn j1939_data<'a>(data: *const u8, len: usize) -> Result<&'a [u8], c_int> {
    if len == 0 {
        Ok(&[])
    } else if data.is_null() {
        Err(invalid("data is a null pointer"))
    } else {
        Ok(std::slice::from_raw_parts(data, len))
    }
}

/// Send a message to where the socket is connected, as `send`.
#[no_mangle]
pub unsafe extern "C" fn dcan_j1939_send(
    sock: *mut dcan_j1939,
    data: *const u8,
    len: usize,
) -> c_int {
    guard(|| match (j1939(sock), j1939_data(data, len)) {
        (Ok(sock), Ok(data)) => done(sock.sock.send(data)),
        (Err(code), _) | (_, Err(code)) => code,
    })
}

/// Send a message to `dst`, as `sendto`.
#[no_mangle]
pub unsafe extern "C" fn dcan_j1939_sendto(
    sock: *mut dcan_j1939,
    data: *const u8,
    len: usize,
    dst: *const dcan_j1939_addr,
) -> c_int {
    guard(|| match (j1939(sock), j1939_data(data, len)) {
        (Ok(_), Ok(_)) if dst.is_null() => invalid("destination is a null pointer"),
        (Ok(sock), Ok(data)) => done(sock.sock.send_to(data, &j1939_addr(dst))),
        (Err(code), _) | (_, Err(code)) => code,
    })
}

/// Receive a message or, on a socket that asked for them, a notice,
/// waiting at most `timeout_ms` (negative: without limit; 0: only what
/// has arrived).
///
/// Returns 1 when something was received and 0 if nothing came in time.
/// `info->size` is the message's length; the first `capacity` bytes of
/// it are in `buffer`.
#[no_mangle]
pub unsafe extern "C" fn dcan_j1939_recv(
    sock: *mut dcan_j1939,
    buffer: *mut u8,
    capacity: usize,
    info: *mut dcan_j1939_info,
    timeout_ms: c_int,
) -> c_int {
    guard(|| {
        let sock = match j1939(sock) {
            Ok(sock) => sock,
            Err(code) => return code,
        };
        if buffer.is_null() && capacity != 0 {
            return invalid("buffer is a null pointer");
        }
        let Some(info) = info.as_mut() else {
            return invalid("info is a null pointer");
        };
        let received = if timeout_ms == 0 {
            sock.sock.try_recv_event()
        } else {
            let mut current = sock.timeout.lock().unwrap();
            let set = apply_timeout(&mut current, timeout_ms, |t| sock.sock.set_read_timeout(t));
            drop(current);
            match set.and_then(|()| sock.sock.recv_event()) {
                Ok(event) => Ok(Some(event)),
                Err(e) if e.is_timeout() => Ok(None),
                Err(e) => Err(e),
            }
        };
        match received {
            Ok(Some(J1939Event::Message(m))) => {
                let n = m.data.len().min(capacity);
                if n != 0 {
                    ptr::copy_nonoverlapping(m.data.as_ptr(), buffer, n);
                }
                *info = dcan_j1939_info {
                    timestamp_ns: m.timestamp_ns,
                    src: j1939_addr_to_c(&m.src),
                    dst_name: m.dst_name,
                    dst_addr: m.dst_addr,
                    priority: m.priority,
                    notice: 0,
                    reason: 0,
                    size: m.data.len() as u32,
                };
                1
            }
            Ok(Some(J1939Event::Notice(n))) => {
                *info = dcan_j1939_info {
                    timestamp_ns: n.timestamp_ns,
                    src: j1939_addr_to_c(&J1939Addr {
                        name: n.src_name,
                        pgn: n.pgn,
                        addr: n.src_addr,
                    }),
                    dst_name: n.dst_name,
                    dst_addr: n.dst_addr,
                    priority: 0,
                    notice: n.kind as u8,
                    reason: n.reason,
                    size: n.size,
                };
                1
            }
            Ok(None) => 0,
            Err(e) => failed(&e),
        }
    })
}

/// Replace the receive filters, as `SO_J1939_FILTER`; with `count` 0
/// there are none and everything for the socket is received.
#[no_mangle]
pub unsafe extern "C" fn dcan_j1939_set_filters(
    sock: *mut dcan_j1939,
    filters: *const j1939_filter,
    count: usize,
) -> c_int {
    guard(|| {
        let sock = match j1939(sock) {
            Ok(sock) => sock,
            Err(code) => return code,
        };
        if filters.is_null() && count != 0 {
            return invalid("filters is a null pointer");
        }
        if count > FILTER_MAX {
            return invalid("a socket takes at most 512 filters");
        }
        let filters: Vec<J1939Filter> = (0..count)
            .map(|i| {
                let f = filters.add(i).read_unaligned();
                J1939Filter {
                    name: f.name,
                    name_mask: f.name_mask,
                    pgn: f.pgn,
                    pgn_mask: f.pgn_mask,
                    addr: f.addr,
                    addr_mask: f.addr_mask,
                }
            })
            .collect();
        done(sock.sock.set_filters(&filters))
    })
}

/// As `SO_J1939_PROMISC`.
#[no_mangle]
pub unsafe extern "C" fn dcan_j1939_set_promisc(sock: *mut dcan_j1939, on: c_int) -> c_int {
    guard(|| match j1939(sock) {
        Ok(sock) => done(sock.sock.set_promisc(on != 0)),
        Err(code) => code,
    })
}

/// As `SO_BROADCAST`.
#[no_mangle]
pub unsafe extern "C" fn dcan_j1939_set_broadcast(sock: *mut dcan_j1939, on: c_int) -> c_int {
    guard(|| match j1939(sock) {
        Ok(sock) => done(sock.sock.set_broadcast(on != 0)),
        Err(code) => code,
    })
}

/// As `SO_J1939_SEND_PRIO`: 0 (highest) to 7.
#[no_mangle]
pub unsafe extern "C" fn dcan_j1939_set_send_prio(sock: *mut dcan_j1939, priority: c_int) -> c_int {
    guard(|| match j1939(sock) {
        Ok(_) if !(0..=7).contains(&priority) => invalid("a priority is 0 to 7"),
        Ok(sock) => done(sock.sock.set_send_priority(priority as u8)),
        Err(code) => code,
    })
}

/// In place of `SO_J1939_ERRQUEUE`: have `dcan_j1939_recv` return notices.
#[no_mangle]
pub unsafe extern "C" fn dcan_j1939_set_notices(sock: *mut dcan_j1939, on: c_int) -> c_int {
    guard(|| match j1939(sock) {
        Ok(sock) => done(sock.sock.set_notices(on != 0)),
        Err(code) => code,
    })
}

/// A file descriptor to wait on, as `dcan_raw_fileno`.
#[no_mangle]
pub unsafe extern "C" fn dcan_j1939_fileno(sock: *const dcan_j1939) -> c_int {
    use std::os::fd::AsRawFd;
    guard(|| match sock.as_ref() {
        Some(sock) => sock.sock.as_raw_fd(),
        None => invalid("socket is a null pointer"),
    })
}

/// Connect to the daemon at its default place to manage its interfaces.
#[no_mangle]
pub unsafe extern "C" fn dcan_control_open() -> *mut dcan_control {
    dcan_control_open_at(ptr::null())
}

/// Connect to the daemon listening at `socket_path` (null for the
/// default).
#[no_mangle]
pub unsafe extern "C" fn dcan_control_open_at(socket_path_: *const c_char) -> *mut dcan_control {
    guard_ptr(|| {
        let path = socket_path(socket_path_)?;
        let control = Control::connect_at(&path).map_err(|e| failed(&e))?;
        Ok(dcan_control { control })
    })
}

/// Close a management connection.
#[no_mangle]
pub unsafe extern "C" fn dcan_control_close(control: *mut dcan_control) {
    if !control.is_null() {
        drop(Box::from_raw(control));
    }
}

unsafe fn managed<'a>(
    control: *const dcan_control,
    name: *const c_char,
) -> Result<(&'a Control, &'a str), c_int> {
    let control = control
        .as_ref()
        .ok_or_else(|| invalid("control is a null pointer"))?;
    Ok((&control.control, text(name, "interface name")?))
}

/// List the daemon's interfaces into `out`, which has room for
/// `capacity`. Returns how many there are, which may be more.
#[no_mangle]
pub unsafe extern "C" fn dcan_interfaces(
    control: *mut dcan_control,
    out: *mut dcan_interface,
    capacity: usize,
) -> c_int {
    guard(|| {
        let Some(control) = control.as_ref() else {
            return invalid("control is a null pointer");
        };
        if out.is_null() && capacity != 0 {
            return invalid("out is a null pointer");
        }
        let list = match control.control.interfaces() {
            Ok(list) => list,
            Err(e) => return failed(&e),
        };
        for (i, iface) in list.iter().take(capacity).enumerate() {
            let mut entry = dcan_interface {
                name: [0; 16],
                kind: [0; 16],
                description: [0; 128],
                up: u8::from(iface.up),
                has_config: u8::from(iface.config.is_some()),
                reserved: [0; 2],
                index: iface.index,
                config: config_to_c(&iface.config.unwrap_or(BusConfig::new(0))),
            };
            copy_text(&mut entry.name, &iface.name);
            copy_text(&mut entry.kind, &iface.kind);
            copy_text(&mut entry.description, &iface.description);
            out.add(i).write(entry);
        }
        list.len() as c_int
    })
}

/// Configure a hardware interface and put it on the bus.
#[no_mangle]
pub unsafe extern "C" fn dcan_interface_up(
    control: *mut dcan_control,
    name: *const c_char,
    config: *const dcan_bus_config,
) -> c_int {
    guard(|| match (managed(control, name), config.as_ref()) {
        (Ok((control, name)), Some(config)) => done(control.up(name, &config_from_c(config))),
        (Ok(_), None) => invalid("config is a null pointer"),
        (Err(code), _) => code,
    })
}

/// Store the settings of a hardware interface that is down.
#[no_mangle]
pub unsafe extern "C" fn dcan_interface_configure(
    control: *mut dcan_control,
    name: *const c_char,
    config: *const dcan_bus_config,
) -> c_int {
    guard(|| match (managed(control, name), config.as_ref()) {
        (Ok((control, name)), Some(config)) => {
            done(control.configure(name, &config_from_c(config)))
        }
        (Ok(_), None) => invalid("config is a null pointer"),
        (Err(code), _) => code,
    })
}

/// Take a hardware interface off the bus.
#[no_mangle]
pub unsafe extern "C" fn dcan_interface_down(
    control: *mut dcan_control,
    name: *const c_char,
) -> c_int {
    guard(|| match managed(control, name) {
        Ok((control, name)) => done(control.down(name)),
        Err(code) => code,
    })
}

/// Give an interface another name.
#[no_mangle]
pub unsafe extern "C" fn dcan_interface_rename(
    control: *mut dcan_control,
    name: *const c_char,
    new_name: *const c_char,
) -> c_int {
    guard(
        || match (managed(control, name), text(new_name, "new name")) {
            (Ok((control, name)), Ok(new_name)) => done(control.rename(name, new_name)),
            (Err(code), _) | (_, Err(code)) => code,
        },
    )
}

/// Create a virtual CAN interface.
#[no_mangle]
pub unsafe extern "C" fn dcan_vcan_add(control: *mut dcan_control, name: *const c_char) -> c_int {
    guard(|| match managed(control, name) {
        Ok((control, name)) => done(control.add_vcan(name)),
        Err(code) => code,
    })
}

/// Remove a virtual CAN interface.
#[no_mangle]
pub unsafe extern "C" fn dcan_vcan_delete(
    control: *mut dcan_control,
    name: *const c_char,
) -> c_int {
    guard(|| match managed(control, name) {
        Ok((control, name)) => done(control.del_vcan(name)),
        Err(code) => code,
    })
}

#[cfg(test)]
mod tests {
    use std::mem::{align_of, size_of};

    use super::*;
    use crate::Id;

    #[test]
    fn frames_are_laid_out_as_on_linux() {
        /// How far into `whole` the field at `part` lies.
        fn offset<T, F>(whole: &T, part: &F) -> usize {
            part as *const F as usize - whole as *const T as usize
        }
        let classic = can_frame {
            can_id: 0,
            len: 0,
            pad: 0,
            res0: 0,
            len8_dlc: 0,
            data: [0; 8],
        };
        assert_eq!(size_of::<can_frame>(), CAN_MTU as usize);
        assert_eq!(align_of::<can_frame>(), 8);
        assert_eq!(offset(&classic, &classic.len), 4);
        assert_eq!(offset(&classic, &classic.len8_dlc), 7);
        assert_eq!(offset(&classic, &classic.data), 8);
        let fd = canfd_frame {
            can_id: 0,
            len: 0,
            flags: 0,
            res0: 0,
            res1: 0,
            data: [0; 64],
        };
        assert_eq!(size_of::<canfd_frame>(), CANFD_MTU as usize);
        assert_eq!(align_of::<canfd_frame>(), 8);
        assert_eq!(offset(&fd, &fd.len), 4);
        assert_eq!(offset(&fd, &fd.flags), 5);
        assert_eq!(offset(&fd, &fd.data), 8);
        assert_eq!(size_of::<can_filter>(), 8);
    }

    #[test]
    fn the_other_structures_have_the_sizes_the_header_states() {
        assert_eq!(size_of::<dcan_rx_info>(), 24);
        assert_eq!(size_of::<dcan_isotp_options>(), 44);
        assert_eq!(size_of::<dcan_bus_config>(), 20);
        assert_eq!(size_of::<dcan_interface>(), 188);
        // As on 64-bit Linux, where the frames start at byte 56.
        assert_eq!(size_of::<bcm_timeval>(), 16);
        assert_eq!(size_of::<bcm_msg_head>(), 56);
        // As on Linux.
        assert_eq!(size_of::<j1939_filter>(), 32);
        assert_eq!(size_of::<dcan_j1939_addr>(), 16);
        assert_eq!(size_of::<dcan_j1939_info>(), 40);
    }

    #[test]
    fn a_received_frame_says_which_kind_it_is() {
        let mut out = canfd_frame {
            can_id: 0,
            len: 0,
            flags: 0,
            res0: 0,
            res1: 0,
            data: [0xFF; 64],
        };
        let classic = Frame::new(Id::Standard(0x123), &[1, 2, 3]).unwrap();
        assert_eq!(frame_to_c(&classic, &mut out), CAN_MTU);
        assert_eq!((out.can_id, out.len, out.flags), (0x123, 3, 0));
        assert_eq!(out.data[..4], [1, 2, 3, 0]);
        let fd = Frame::new_fd(Id::Extended(0x1ABCDEF0), &[9; 12], true).unwrap();
        assert_eq!(frame_to_c(&fd, &mut out), CANFD_MTU);
        assert_eq!(out.can_id, 0x1ABCDEF0 | crate::CAN_EFF_FLAG);
        assert_eq!(out.len, 12);
        assert_eq!(out.flags, CANFD_FDF | crate::CANFD_BRS);
    }

    #[test]
    fn daemon_refusals_keep_their_numbers() {
        let code = |code| {
            error_code(&Error::Daemon {
                code,
                message: String::new(),
            })
        };
        assert_eq!(
            code(ErrorCode::NoSuchInterface),
            DCAN_ERROR_NO_SUCH_INTERFACE
        );
        assert_eq!(code(ErrorCode::Busy), DCAN_ERROR_BUSY);
        assert_eq!(
            code(ErrorCode::AddressNotAvailable),
            DCAN_ERROR_ADDRESS_NOT_AVAILABLE
        );
        assert_eq!(code(ErrorCode::NotPermitted), DCAN_ERROR_NOT_PERMITTED);
        assert_eq!(code(ErrorCode::TooBig), DCAN_ERROR_TOO_BIG);
        assert_eq!(code(ErrorCode::Other), DCAN_ERROR_DAEMON_OTHER);
    }

    #[test]
    fn text_is_cut_to_fit_at_a_character() {
        let mut out = [1 as c_char; 4];
        copy_text(&mut out, "aé€");
        // 'a' and the two bytes of 'é' fit before the terminator.
        assert_eq!(out[3], 0);
        let mut out = [1 as c_char; 3];
        copy_text(&mut out, "aé€");
        assert_eq!(out[1], 0);
    }
}
