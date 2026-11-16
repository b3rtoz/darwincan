// SPDX-License-Identifier: MIT OR Apache-2.0

//! Client library for the darwinCAN daemon.
//!
//! ```no_run
//! use darwincan::{Frame, Id, RawSocket};
//!
//! let sock = RawSocket::open("vcan0")?;
//! sock.send(&Frame::new(Id::Standard(0x123), &[0xDE, 0xAD])?)?;
//! let rx = sock.recv()?;
//! println!("{} at {} ns", rx.frame, rx.timestamp_ns);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Sockets are `Sync`: one thread can block in `recv` while others `send`.
//!
//! The same sockets are available to C and anything that can call C through
//! the [`ffi`] module and `include/darwincan.h`.

pub mod ffi;

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use darwincan_core::wire::{self, LinkState, Message, MessageReader, SockOpt};

pub use darwincan_core::bcm;
pub use darwincan_core::j1939;
pub use darwincan_core::wire::{ErrorCode, IfaceInfo, ANY_INTERFACE};
pub use darwincan_core::{
    default_socket_path, err, BcmMessage, BusConfig, Filter, Frame, FrameError, Id, IsoTpFailure,
    IsoTpParams, J1939Addr, J1939Filter, J1939Message, J1939Notice, J1939NoticeKind, CANFD_BRS,
    CANFD_ESI, CANFD_FDF, CAN_EFF_FLAG, CAN_EFF_MASK, CAN_ERR_FLAG, CAN_INV_FILTER, CAN_RTR_FLAG,
    CAN_SFF_MASK,
};

#[derive(Debug)]
pub enum Error {
    /// The daemon is not running (or not listening at the expected path).
    DaemonUnavailable(io::Error),
    Io(io::Error),
    /// The daemon rejected a request.
    Daemon {
        code: ErrorCode,
        message: String,
    },
    /// An ISO-TP transfer failed.
    IsoTp {
        tx: bool,
        reason: IsoTpFailure,
    },
    /// The daemon sent something unexpected.
    Protocol(&'static str),
    /// The daemon speaks another version of the protocol than this
    /// library: they come from different builds of darwinCAN.
    VersionMismatch {
        client: u16,
        daemon: u16,
    },
}

impl Error {
    /// True if a receive call gave up because the read timeout elapsed.
    pub fn is_timeout(&self) -> bool {
        matches!(self, Error::Io(e)
            if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::DaemonUnavailable(e) => {
                write!(f, "cannot reach darwincand ({e}); is the daemon running?")
            }
            Error::Io(e) => write!(f, "{e}"),
            Error::Daemon { message, .. } => write!(f, "{message}"),
            Error::IsoTp { tx: true, reason } => write!(f, "ISO-TP transmit failed: {reason}"),
            Error::IsoTp { tx: false, reason } => write!(f, "ISO-TP receive failed: {reason}"),
            Error::Protocol(what) => write!(f, "protocol error: {what}"),
            Error::VersionMismatch { client, daemon } => write!(
                f,
                "darwincand and this program come from different versions of darwinCAN \
                 (the daemon speaks protocol version {daemon}, this program version {client}); \
                 update or rebuild whichever is older"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::DaemonUnavailable(e) | Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        match wire::version_mismatch(&e) {
            Some(mismatch) => Error::VersionMismatch {
                client: mismatch.ours,
                daemon: mismatch.theirs,
            },
            None => Error::Io(e),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// How long to keep trying when the daemon's socket is missing or not yet
/// accepting connections, so a client launched right after the daemon (for
/// example from the same script) does not fail while it starts up.
const STARTUP_GRACE: Duration = Duration::from_millis(500);

fn connect_with_grace(path: &Path) -> io::Result<UnixStream> {
    let deadline = Instant::now() + STARTUP_GRACE;
    loop {
        match UnixStream::connect(path) {
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            other => return other,
        }
    }
}

/// What the reading side of a connection has seen but not yet handed out.
#[derive(Default)]
struct Inbox {
    /// Frames, PDUs and errors waiting for a `recv` call.
    events: VecDeque<Message>,
    /// Acknowledgements received so far, refusals among them.
    acks: u64,
    /// The acknowledged requests that were refused, by their number.
    refused: Vec<(u64, ErrorCode, String)>,
    /// A thread is currently blocked reading the socket.
    reading: bool,
    /// Whether the interface can send, when the daemon has said.
    link: Option<LinkState>,
    /// Whether the readiness descriptor has been told that events are
    /// held here.
    flagged: bool,
    /// Frames and messages thrown away here because the program does not
    /// receive: see [`Inbox::HELD_MAX`].
    overrun: u64,
}

impl Inbox {
    /// The most events kept for a program that is not receiving them. A
    /// program that only sends still has the daemon's messages taken in,
    /// to see acknowledgements and the interface's state, and what is on
    /// the bus comes with them; without a limit that would be kept for
    /// ever. Beyond it the oldest go, as they would in the daemon.
    const HELD_MAX: usize = 16 * 1024;

    /// Put a message from the daemon where its taker will look for it.
    fn file(&mut self, msg: Message) {
        if self.events.len() >= Self::HELD_MAX {
            let oldest_is_data = matches!(
                self.events.front(),
                Some(
                    Message::Frame { .. }
                        | Message::IsoTpData { .. }
                        | Message::J1939Data(_)
                        | Message::BcmNotify { .. }
                )
            );
            // Never an error or the end of a transfer: someone may wait
            // for exactly that.
            if oldest_is_data {
                self.events.pop_front();
                self.overrun += 1;
            }
        }
        match msg {
            Message::Ok => self.acks += 1,
            Message::Refused { code, message } => {
                self.acks += 1;
                self.refused.push((self.acks, code, message));
            }
            Message::LinkState(state) => self.link = Some(state),
            msg => self.events.push_back(msg),
        }
    }
}

enum Want {
    Event,
    /// The acknowledgement with this sequence number.
    Ack(u64),
}

/// A connection to the daemon, split so reads and writes do not contend.
///
/// Any number of threads may wait for events or acknowledgements. One of
/// them at a time reads the socket and files what arrives in the inbox;
/// the others wait on the condition variable.
struct Conn {
    stream: UnixStream,
    reader: Mutex<MessageReader<UnixStream>>,
    /// The write half, with the number of acknowledged requests sent.
    writer: Mutex<(UnixStream, u64)>,
    inbox: Mutex<Inbox>,
    inbox_changed: Condvar,
    /// Whether a wait for an event ends when a signal interrupts it, as a
    /// C program expects of a blocking call, or carries on.
    interruptible: AtomicBool,
    /// Made when a descriptor to wait on is first asked for; `None`
    /// inside if it could not be made.
    notifier: OnceLock<Option<Notifier>>,
    /// Whether something received is being kept outside the inbox, where
    /// a receive that does not wait would still find it (the C API's
    /// ISO-TP channel keeps messages that arrive during a send).
    held_elsewhere: AtomicBool,
    opened: Instant,
}

impl Conn {
    fn connect(path: &Path) -> Result<Conn> {
        let stream = connect_with_grace(path).map_err(Error::DaemonUnavailable)?;
        // No write here raises SIGPIPE when the daemon has gone: the
        // standard library sets SO_NOSIGPIPE on the sockets it makes on
        // macOS. A C program, unlike a Rust one, does not ignore that
        // signal, and the C API's tests check that it survives.
        Ok(Conn {
            interruptible: AtomicBool::new(false),
            notifier: OnceLock::new(),
            held_elsewhere: AtomicBool::new(false),
            opened: Instant::now(),
            reader: Mutex::new(MessageReader::new(stream.try_clone()?)),
            writer: Mutex::new((stream.try_clone()?, 0)),
            inbox: Mutex::new(Inbox::default()),
            inbox_changed: Condvar::new(),
            stream,
        })
    }

    fn send(&self, msg: &Message) -> Result<()> {
        let mut buf = Vec::with_capacity(96);
        msg.encode(&mut buf);
        self.writer.lock().unwrap().0.write_all(&buf)?;
        Ok(())
    }

    /// Send a request the daemon acknowledges and wait for the
    /// acknowledgement, so the request has taken effect on return. Events
    /// arriving meanwhile are kept for `recv`.
    fn send_acked(&self, msg: &Message) -> Result<()> {
        let mut buf = Vec::with_capacity(96);
        msg.encode(&mut buf);
        let seq = {
            let mut writer = self.writer.lock().unwrap();
            writer.0.write_all(&buf)?;
            writer.1 += 1;
            writer.1
        };
        self.wait(Want::Ack(seq))?;
        let mut inbox = self.inbox.lock().unwrap();
        match inbox.refused.iter().position(|(n, ..)| *n == seq) {
            Some(i) => {
                let (_, code, message) = inbox.refused.swap_remove(i);
                Err(Error::Daemon { code, message })
            }
            None => Ok(()),
        }
    }

    /// Next frame, PDU or other event; a daemon error becomes `Err`.
    fn recv(&self) -> Result<Message> {
        match self.wait(Want::Event)? {
            Some(Message::Error { code, message }) => Err(Error::Daemon { code, message }),
            Some(msg) => Ok(msg),
            None => Err(Error::Protocol("event expected")),
        }
    }

    /// An event that has arrived, if any, without waiting for one.
    fn try_recv(&self) -> Result<Option<Message>> {
        let taken = self.take_arrived(true);
        self.settle();
        match taken? {
            Some(Message::Error { code, message }) => Err(Error::Daemon { code, message }),
            other => Ok(other),
        }
    }

    /// Take in what the daemon has sent, without waiting and without
    /// handing anything out: link state and acknowledgements take effect,
    /// events are kept for `recv`.
    fn pump(&self) -> Result<()> {
        let taken = self.take_arrived(false);
        self.settle();
        taken.map(|_| ())
    }

    /// File everything that has arrived; with `pop`, return the first
    /// event there is.
    fn take_arrived(&self, pop: bool) -> Result<Option<Message>> {
        let mut inbox = self.inbox.lock().unwrap();
        loop {
            if pop {
                if let Some(msg) = inbox.events.pop_front() {
                    return Ok(Some(msg));
                }
            }
            // Another thread is blocked reading: what arrives goes to it.
            if inbox.reading {
                return Ok(None);
            }
            inbox.reading = true;
            drop(inbox);
            let arrived = self.read_arrived();
            inbox = self.inbox.lock().unwrap();
            inbox.reading = false;
            self.inbox_changed.notify_all();
            let arrived = arrived?;
            if arrived.is_empty() {
                return Ok(None);
            }
            for msg in arrived {
                inbox.file(msg);
            }
            if !pop {
                // Once more round: more may have come meanwhile.
                continue;
            }
        }
    }

    /// The messages the daemon has already sent, read without blocking.
    fn read_arrived(&self) -> Result<Vec<Message>> {
        let mut reader = self.reader.lock().unwrap();
        let mut arrived = Vec::new();
        loop {
            while let Some(msg) = reader.buffered()? {
                arrived.push(msg);
            }
            if !readable_now(self.stream.as_raw_fd()) {
                return Ok(arrived);
            }
            match reader.fill() {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // What came before the failure is delivered first; the
                // failure shows again at the next read.
                Err(_) if !arrived.is_empty() => return Ok(arrived),
                other => other?,
            }
        }
    }

    /// The next messages from the daemon, waiting until there is one. All
    /// that came with it are returned, so that nothing complete is left
    /// where only the reader can see it.
    fn read_some(&self, interruptible: bool) -> io::Result<Vec<Message>> {
        let mut reader = self.reader.lock().unwrap();
        let mut arrived = Vec::new();
        loop {
            loop {
                match reader.buffered() {
                    Ok(Some(msg)) => arrived.push(msg),
                    Ok(None) => break,
                    Err(_) if !arrived.is_empty() => break,
                    Err(e) => return Err(e),
                }
            }
            if !arrived.is_empty() {
                return Ok(arrived);
            }
            match reader.fill() {
                Err(e) if e.kind() == io::ErrorKind::Interrupted && !interruptible => {}
                other => other?,
            }
        }
    }

    fn wait(&self, want: Want) -> Result<Option<Message>> {
        let result = self.wait_unsettled(want);
        self.settle();
        result
    }

    fn wait_unsettled(&self, want: Want) -> Result<Option<Message>> {
        let mut inbox = self.inbox.lock().unwrap();
        loop {
            match want {
                Want::Event => {
                    if let Some(msg) = inbox.events.pop_front() {
                        return Ok(Some(msg));
                    }
                }
                Want::Ack(seq) => {
                    if inbox.acks >= seq {
                        return Ok(None);
                    }
                }
            }
            if inbox.reading {
                inbox = self.inbox_changed.wait(inbox).unwrap();
                continue;
            }
            // Become the reader. The inbox is unlocked while blocked on
            // the socket so other threads can still take queued events.
            inbox.reading = true;
            drop(inbox);
            // Only a wait for an event may be cut short: a request whose
            // acknowledgement is awaited has been sent and takes effect.
            let interruptible =
                matches!(want, Want::Event) && self.interruptible.load(Ordering::Relaxed);
            let result = match self.read_some(interruptible) {
                // The read timeout is for receiving. An acknowledgement
                // comes however long the daemon takes (a frame ahead of
                // the request may be waiting for room in an adapter), or
                // the connection fails.
                Err(e)
                    if matches!(want, Want::Ack(_))
                        && matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) =>
                {
                    Ok(Vec::new())
                }
                other => other,
            };
            inbox = self.inbox.lock().unwrap();
            inbox.reading = false;
            self.inbox_changed.notify_all();
            for msg in result? {
                inbox.file(msg);
            }
        }
    }

    /// Whether the socket's interface can send, as far as the daemon has
    /// said. `None` before it has said anything.
    fn link(&self) -> Option<LinkState> {
        self.inbox.lock().unwrap().link
    }

    /// A descriptor that is readable exactly when a receive that does not
    /// wait would return something: when the daemon has sent more, or
    /// events are already held here.
    fn readiness_fd(&self) -> RawFd {
        let notifier = self
            .notifier
            .get_or_init(|| Notifier::new(self.stream.as_raw_fd()));
        match notifier {
            Some(notifier) => {
                self.settle();
                notifier.fd()
            }
            None => self.stream.as_raw_fd(),
        }
    }

    /// Bring the readiness descriptor, if one has been asked for, in line
    /// with what is held here.
    fn settle(&self) {
        if let Some(Some(notifier)) = self.notifier.get() {
            let mut inbox = self.inbox.lock().unwrap();
            let holding = !inbox.events.is_empty() || self.held_elsewhere.load(Ordering::Relaxed);
            notifier.update((holding != inbox.flagged).then_some(holding));
            inbox.flagged = holding;
        }
    }

    /// Open the connection as a socket: send `msg` and wait for its `Ok`.
    /// What the daemon says before that (the interface's state) is kept.
    fn open(&self, msg: &Message) -> Result<()> {
        self.send(msg)?;
        loop {
            match self.reader.lock().unwrap().read() {
                Ok(Message::Ok) => return Ok(()),
                Ok(Message::Error { code, message }) => {
                    return Err(Error::Daemon { code, message })
                }
                Ok(other) => self.inbox.lock().unwrap().file(other),
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Err(Error::Protocol(
                        "the daemon closed the connection without answering; \
                         it may come from another version of darwinCAN",
                    ))
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Send a request and require `Ok` in response. Only for use before
    /// the connection is shared between threads.
    fn request_ok(&self, msg: &Message) -> Result<()> {
        match self.request(msg)? {
            Message::Ok => Ok(()),
            _ => Err(Error::Protocol("expected acknowledgement")),
        }
    }

    /// Send a request and return the single reply. Same restriction.
    fn request(&self, msg: &Message) -> Result<Message> {
        self.send(msg)?;
        match self.reader.lock().unwrap().read() {
            Ok(Message::Error { code, message }) => Err(Error::Daemon { code, message }),
            Ok(reply) => Ok(reply),
            // A daemon from before the protocol carried a version check
            // hangs up on a client it does not understand.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(Error::Protocol(
                "the daemon closed the connection without answering; \
                 it may come from another version of darwinCAN",
            )),
            Err(e) => Err(e.into()),
        }
    }
}

/// A descriptor to wait on with `poll`, `select` or `kqueue` that stands
/// for a connection: a kernel event queue that watches the connection's
/// socket and a flag raised while events are held in the library.
///
/// The queue is level-triggered, but it only looks at what it watches
/// when its events are collected. A condition that has passed (the socket
/// read empty, the flag lowered) would otherwise go on making it readable,
/// so every update collects.
#[cfg(target_vendor = "apple")]
struct Notifier {
    queue: std::mem::ManuallyDrop<std::os::fd::OwnedFd>,
    /// The process that made the queue. A child made by `fork` does not
    /// inherit a kernel event queue; the number is free there and may
    /// belong to something else by the time the child lets go of this.
    owner: u32,
}

#[cfg(target_vendor = "apple")]
impl Drop for Notifier {
    fn drop(&mut self) {
        if std::process::id() == self.owner {
            // SAFETY: dropped once, here, and not used again.
            unsafe { std::mem::ManuallyDrop::drop(&mut self.queue) };
        }
    }
}

#[cfg(target_vendor = "apple")]
mod kq {
    use std::ffi::{c_int, c_void};

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct KEvent {
        pub ident: usize,
        pub filter: i16,
        pub flags: u16,
        pub fflags: u32,
        pub data: isize,
        pub udata: *mut c_void,
    }

    #[repr(C)]
    pub struct TimeSpec {
        pub sec: i64,
        pub nsec: i64,
    }

    pub const EVFILT_READ: i16 = -1;
    pub const EVFILT_USER: i16 = -10;
    pub const EV_ADD: u16 = 0x0001;
    pub const EV_DELETE: u16 = 0x0002;
    pub const EV_ENABLE: u16 = 0x0004;
    pub const NOTE_TRIGGER: u32 = 0x0100_0000;

    extern "C" {
        pub fn kqueue() -> c_int;
        pub fn kevent(
            kq: c_int,
            changes: *const KEvent,
            nchanges: c_int,
            events: *mut KEvent,
            nevents: c_int,
            timeout: *const TimeSpec,
        ) -> c_int;
    }

    pub fn event(ident: usize, filter: i16, flags: u16, fflags: u32) -> KEvent {
        KEvent {
            ident,
            filter,
            flags,
            fflags,
            data: 0,
            udata: std::ptr::null_mut(),
        }
    }
}

#[cfg(target_vendor = "apple")]
impl Notifier {
    /// The identifier of the flag among the queue's events.
    const HOLDING: usize = 1;

    fn new(socket: RawFd) -> Option<Notifier> {
        use std::os::fd::FromRawFd;
        // SAFETY: plain system calls; a descriptor that comes back is
        // owned from here on.
        let queue = unsafe {
            let fd = kq::kqueue();
            if fd < 0 {
                return None;
            }
            std::os::fd::OwnedFd::from_raw_fd(fd)
        };
        let notifier = Notifier {
            queue: std::mem::ManuallyDrop::new(queue),
            owner: std::process::id(),
        };
        let watch = kq::event(socket as usize, kq::EVFILT_READ, kq::EV_ADD, 0);
        notifier.apply(Some(watch)).then_some(notifier)
    }

    fn fd(&self) -> RawFd {
        self.queue.as_raw_fd()
    }

    /// Raise or lower the flag (`Some`), or leave it, and collect.
    fn update(&self, holding: Option<bool>) {
        let change = holding.map(|holding| {
            if holding {
                kq::event(
                    Self::HOLDING,
                    kq::EVFILT_USER,
                    kq::EV_ADD | kq::EV_ENABLE,
                    kq::NOTE_TRIGGER,
                )
            } else {
                kq::event(Self::HOLDING, kq::EVFILT_USER, kq::EV_DELETE, 0)
            }
        });
        self.apply(change);
    }

    fn apply(&self, change: Option<kq::KEvent>) -> bool {
        let mut collected = [kq::event(0, 0, 0, 0); 4];
        let now = kq::TimeSpec { sec: 0, nsec: 0 };
        // SAFETY: the pointers are to live values of the lengths given.
        let n = unsafe {
            kq::kevent(
                self.queue.as_raw_fd(),
                change.as_ref().map_or(std::ptr::null(), |c| c),
                i32::from(change.is_some()),
                collected.as_mut_ptr(),
                collected.len() as i32,
                &now,
            )
        };
        n >= 0
    }
}

/// Elsewhere the socket itself has to do, with the rule that goes with
/// it: wait on it only when a receive that does not wait returned nothing.
#[cfg(not(target_vendor = "apple"))]
struct Notifier;

#[cfg(not(target_vendor = "apple"))]
impl Notifier {
    fn new(_socket: RawFd) -> Option<Notifier> {
        None
    }

    fn fd(&self) -> RawFd {
        -1
    }

    fn update(&self, _holding: Option<bool>) {}
}

extern "C" {
    fn recv(
        fd: RawFd,
        buffer: *mut std::ffi::c_void,
        length: usize,
        flags: std::ffi::c_int,
    ) -> isize;
}

/// Whether a read on `fd` would return at once (data, end of stream or an
/// error). Asked by looking at the first byte without taking it, which
/// costs far less than `poll` does on macOS.
fn readable_now(fd: RawFd) -> bool {
    const MSG_PEEK: std::ffi::c_int = 0x2;
    #[cfg(target_os = "linux")]
    const MSG_DONTWAIT: std::ffi::c_int = 0x40;
    #[cfg(not(target_os = "linux"))]
    const MSG_DONTWAIT: std::ffi::c_int = 0x80;
    let mut byte = 0u8;
    // SAFETY: `byte` is one writable byte and the length passed is 1.
    let n = unsafe {
        recv(
            fd,
            (&mut byte as *mut u8).cast(),
            1,
            MSG_PEEK | MSG_DONTWAIT,
        )
    };
    if n >= 0 {
        return true;
    }
    !matches!(
        io::Error::last_os_error().kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

/// Refuse a message longer than one request to the daemon can carry,
/// before the daemon hangs up on it.
fn sendable(data: &[u8]) -> Result<()> {
    if data.len() > darwincan_core::wire::MAX_PAYLOAD - 64 {
        return Err(Error::Daemon {
            code: ErrorCode::TooBig,
            message: "the message is longer than can be sent (128 MiB)".into(),
        });
    }
    Ok(())
}

/// A received frame with its metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxFrame {
    pub frame: Frame,
    /// When the daemon received or transmitted the frame: nanoseconds since
    /// the Unix epoch on a monotonic clock. Differences between timestamps
    /// are exact regardless of how late the client reads the frame.
    pub timestamp_ns: u64,
    /// True if this socket sent the frame (see
    /// [`RawSocket::set_recv_own_msgs`]).
    pub own: bool,
    /// The index of the interface the frame was on (see
    /// [`IfaceInfo::index`]): of use on a socket open on every interface.
    pub ifindex: u32,
    /// How many frames this socket has lost so far, up to this one,
    /// because the program read too slowly: the daemon drops frames for a
    /// socket whose queue is full and never holds up the bus.
    pub dropped: u64,
}

/// The equivalent of a SocketCAN `CAN_RAW` socket bound to one interface,
/// or, opened on [`ANY_INTERFACE`], to all of them.
pub struct RawSocket {
    conn: Conn,
    ifname: String,
    dropped: std::sync::atomic::AtomicU64,
    /// When a send last looked for news of the interface's state, in
    /// nanoseconds since the socket was opened.
    looked_at: std::sync::atomic::AtomicU64,
}

impl RawSocket {
    /// Open a raw socket on `ifname` via the default daemon socket.
    pub fn open(ifname: &str) -> Result<RawSocket> {
        RawSocket::open_at(&default_socket_path(), ifname)
    }

    /// Open via a daemon listening at `socket_path`.
    pub fn open_at(socket_path: &Path, ifname: &str) -> Result<RawSocket> {
        let conn = Conn::connect(socket_path)?;
        conn.open(&Message::OpenRaw {
            ifname: ifname.to_string(),
        })?;
        Ok(RawSocket {
            conn,
            ifname: ifname.to_string(),
            dropped: std::sync::atomic::AtomicU64::new(0),
            looked_at: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Queue a frame for transmission. Returns once the daemon has been
    /// handed the frame, like `write(2)` on a SocketCAN socket.
    ///
    /// Fails at once if the interface is down, bus-off or gone. A frame
    /// that the daemon took and then could not put on the bus (nothing
    /// acknowledges and the adapter's buffer is full) is reported by a
    /// later [`recv`](Self::recv).
    pub fn send(&self, frame: &Frame) -> Result<()> {
        if self.ifname == ANY_INTERFACE {
            return Err(Error::Daemon {
                code: ErrorCode::InvalidRequest,
                message: "a socket open on every interface must say which one to send on \
                          (send_to)"
                    .into(),
            });
        }
        self.check_link()?;
        self.conn.send(&Message::Send(*frame))
    }

    /// Queue a frame for transmission on the interface with this index
    /// (see [`IfaceInfo::index`]): the way to send on a socket open on
    /// every interface. Failures are reported by a later
    /// [`recv`](Self::recv).
    pub fn send_to(&self, ifindex: u32, frame: &Frame) -> Result<()> {
        self.conn.send(&Message::SendTo {
            ifindex,
            frame: *frame,
        })
    }

    /// Refuse to send on an interface that cannot. The daemon says when
    /// that changes; before believing that it cannot, ask, since the news
    /// of a change back is lost if this socket's queue was full then.
    fn check_link(&self) -> Result<()> {
        // Looking for news costs a system call, so an interface believed
        // able to send is looked at once a millisecond at the most. News
        // that comes in between is a moment late; the daemon still
        // refuses the frame, and says so to a later receive.
        let up = matches!(self.conn.link(), None | Some(LinkState::Up));
        let now = self.conn.opened.elapsed().as_nanos() as u64;
        if up && now.saturating_sub(self.looked_at.load(Ordering::Relaxed)) < 1_000_000 {
            return Ok(());
        }
        self.looked_at.store(now, Ordering::Relaxed);
        self.conn.pump()?;
        if matches!(self.conn.link(), None | Some(LinkState::Up)) {
            return Ok(());
        }
        self.conn.send_acked(&Message::QueryLink)?;
        let (code, message) = match self.conn.link() {
            None | Some(LinkState::Up) => return Ok(()),
            Some(LinkState::Down) => (ErrorCode::InterfaceDown, format!("{} is down", self.ifname)),
            Some(LinkState::BusOff) => (
                ErrorCode::InterfaceDown,
                format!("{} is bus-off; take it down and up again", self.ifname),
            ),
            Some(LinkState::Removed) => (
                ErrorCode::InterfaceRemoved,
                format!("{} was removed", self.ifname),
            ),
        };
        Err(Error::Daemon { code, message })
    }

    /// How many frames this socket has lost so far because the program
    /// read too slowly, as of the last frame received.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed) + self.conn.inbox.lock().unwrap().overrun
    }

    fn rx_frame(&self, msg: Message) -> Result<Option<RxFrame>> {
        match msg {
            Message::Frame {
                ifindex,
                timestamp_ns,
                own,
                frame,
            } => Ok(Some(RxFrame {
                frame,
                timestamp_ns,
                own,
                ifindex,
                dropped: self.dropped(),
            })),
            Message::Dropped { total } => {
                self.dropped.store(total, Ordering::Relaxed);
                Ok(None)
            }
            _ => Err(Error::Protocol("unexpected message on raw socket")),
        }
    }

    /// Block until the daemon has handled every frame sent on this socket
    /// so far: each has been passed to the adapter (or failed) and been
    /// delivered to the other local sockets' queues.
    pub fn flush(&self) -> Result<()> {
        self.conn.send_acked(&Message::Sync)
    }

    /// Block until a frame arrives (or the read timeout elapses; see
    /// [`Error::is_timeout`]).
    pub fn recv(&self) -> Result<RxFrame> {
        loop {
            if let Some(rx) = self.rx_frame(self.conn.recv()?)? {
                return Ok(rx);
            }
        }
    }

    /// Return a frame or error that has arrived, without waiting for one.
    ///
    /// After [`flush`](Self::flush) returns, everything the daemon sent
    /// this socket before acknowledging is available, including the
    /// failures of frames sent on it, so a sender can check its
    /// transmissions with `flush` followed by `try_recv` until it returns
    /// `None`. While another thread is blocked in [`recv`](Self::recv),
    /// arrivals go to that thread.
    pub fn try_recv(&self) -> Result<Option<RxFrame>> {
        while let Some(msg) = self.conn.try_recv()? {
            if let Some(rx) = self.rx_frame(msg)? {
                return Ok(Some(rx));
            }
        }
        Ok(None)
    }

    /// Replace the receive filters. A frame is delivered if any filter
    /// matches; an empty list receives nothing. New sockets receive
    /// everything.
    ///
    /// Like the other option setters, this returns once the daemon has
    /// applied the change: frames the daemon handles afterwards are
    /// filtered the new way. Frames it had already queued for this socket
    /// may still arrive.
    pub fn set_filters(&self, filters: &[Filter]) -> Result<()> {
        self.conn.send_acked(&Message::SetFilters(filters.to_vec()))
    }

    /// Whether frames sent on this socket reach other local sockets on the
    /// interface (default: true).
    pub fn set_loopback(&self, on: bool) -> Result<()> {
        self.set_option(SockOpt::Loopback, on)
    }

    /// Whether this socket receives its own frames, marked
    /// [`RxFrame::own`], stamped with their transmit time (default: false).
    pub fn set_recv_own_msgs(&self, on: bool) -> Result<()> {
        self.set_option(SockOpt::RecvOwnMsgs, on)
    }

    /// Whether a frame must match every filter to be delivered, not just
    /// one of them (default: false), as `CAN_RAW_JOIN_FILTERS` on Linux.
    pub fn set_join_filters(&self, on: bool) -> Result<()> {
        self.set_option(SockOpt::JoinFilters, on)
    }

    /// Receive error frames (bus state changes, overruns) for the given
    /// error classes; see [`err`]. Use [`err::MASK`] for all, 0 for none
    /// (the default). Error frames have [`Frame::is_error`] set and are
    /// not subject to the id filters.
    pub fn set_error_filter(&self, classes: u32) -> Result<()> {
        self.conn.send_acked(&Message::SetOption {
            opt: SockOpt::ErrFilter,
            value: classes,
        })
    }

    fn set_option(&self, opt: SockOpt, on: bool) -> Result<()> {
        self.conn.send_acked(&Message::SetOption {
            opt,
            value: u32::from(on),
        })
    }

    /// Limit how long [`recv`](Self::recv) blocks. `None` blocks forever.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        Ok(self.conn.stream.set_read_timeout(timeout)?)
    }

    /// Let a signal end a blocked [`recv`](Self::recv) with an
    /// `Interrupted` error, for the C API.
    pub(crate) fn set_interruptible(&self, on: bool) {
        self.conn.interruptible.store(on, Ordering::Relaxed);
    }
}

impl AsRawFd for RawSocket {
    /// A descriptor to wait on with `poll`, `select` or `kqueue`, and for
    /// nothing else: always read through [`RawSocket::recv`]. It is
    /// readable exactly when [`try_recv`](RawSocket::try_recv) would
    /// return something. It is not the socket itself, and lives as long
    /// as the socket does.
    fn as_raw_fd(&self) -> RawFd {
        self.conn.readiness_fd()
    }
}

/// Something that happened on an ISO-TP channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsoTpEvent {
    /// A complete message arrived from the peer.
    Data {
        data: Vec<u8>,
        timestamp_ns: u64,
    },
    /// The message passed to [`IsoTpSocket::send`] was fully transmitted.
    TxDone,
    TxFailed(IsoTpFailure),
    RxFailed(IsoTpFailure),
}

fn isotp_event(msg: Message) -> Result<IsoTpEvent> {
    match msg {
        Message::IsoTpData { timestamp_ns, data } => Ok(IsoTpEvent::Data { data, timestamp_ns }),
        Message::IsoTpTxDone => Ok(IsoTpEvent::TxDone),
        Message::IsoTpFailed { tx: true, reason } => Ok(IsoTpEvent::TxFailed(reason)),
        Message::IsoTpFailed { tx: false, reason } => Ok(IsoTpEvent::RxFailed(reason)),
        _ => Err(Error::Protocol("unexpected message on ISO-TP socket")),
    }
}

/// An ISO-TP (ISO 15765-2) channel, the equivalent of a `CAN_ISOTP` socket.
///
/// Segmentation, flow control and STmin pacing run inside the daemon, so
/// their timing does not depend on this process being scheduled promptly.
pub struct IsoTpSocket {
    conn: Conn,
}

impl IsoTpSocket {
    pub fn open(ifname: &str, params: IsoTpParams) -> Result<IsoTpSocket> {
        IsoTpSocket::open_at(&default_socket_path(), ifname, params)
    }

    pub fn open_at(socket_path: &Path, ifname: &str, params: IsoTpParams) -> Result<IsoTpSocket> {
        let conn = Conn::connect(socket_path)?;
        conn.open(&Message::OpenIsoTp {
            ifname: ifname.to_string(),
            params,
        })?;
        Ok(IsoTpSocket { conn })
    }

    /// Start transmitting a message. Returns immediately; the outcome
    /// arrives as [`IsoTpEvent::TxDone`] or [`IsoTpEvent::TxFailed`].
    pub fn send(&self, data: &[u8]) -> Result<()> {
        sendable(data)?;
        self.conn.send(&Message::IsoTpSend(data.to_vec()))
    }

    /// Say whether messages received on this channel are being kept
    /// somewhere a receive would find them, for the descriptor to wait on.
    pub(crate) fn hold(&self, held: bool) {
        self.conn.held_elsewhere.store(held, Ordering::Relaxed);
        self.conn.settle();
    }

    /// Block until the next event on the channel.
    pub fn recv_event(&self) -> Result<IsoTpEvent> {
        isotp_event(self.conn.recv()?)
    }

    /// The next event that has arrived, without waiting for one.
    pub fn try_recv_event(&self) -> Result<Option<IsoTpEvent>> {
        self.conn.try_recv()?.map(isotp_event).transpose()
    }

    /// Block until a complete message arrives. Transmit completions are
    /// skipped; a failed transfer in either direction is returned as an
    /// error.
    pub fn recv(&self) -> Result<Vec<u8>> {
        loop {
            match self.recv_event()? {
                IsoTpEvent::Data { data, .. } => return Ok(data),
                IsoTpEvent::TxDone => {}
                IsoTpEvent::TxFailed(reason) => return Err(Error::IsoTp { tx: true, reason }),
                IsoTpEvent::RxFailed(reason) => return Err(Error::IsoTp { tx: false, reason }),
            }
        }
    }

    /// Send a message and block until it has been fully transmitted.
    /// Messages received meanwhile are discarded; use
    /// [`send`](Self::send) and [`recv_event`](Self::recv_event) to
    /// interleave both directions.
    pub fn send_blocking(&self, data: &[u8]) -> Result<()> {
        self.send(data)?;
        loop {
            match self.recv_event()? {
                IsoTpEvent::TxDone => return Ok(()),
                IsoTpEvent::TxFailed(reason) => return Err(Error::IsoTp { tx: true, reason }),
                IsoTpEvent::Data { .. } | IsoTpEvent::RxFailed(_) => {}
            }
        }
    }

    /// Limit how long receive calls block. `None` blocks forever.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        Ok(self.conn.stream.set_read_timeout(timeout)?)
    }

    /// Let a signal end a blocked receive with an `Interrupted` error,
    /// for the C API.
    pub(crate) fn set_interruptible(&self, on: bool) {
        self.conn.interruptible.store(on, Ordering::Relaxed);
    }
}

impl AsRawFd for IsoTpSocket {
    /// A descriptor to wait on, as for [`RawSocket`].
    fn as_raw_fd(&self) -> RawFd {
        self.conn.readiness_fd()
    }
}

/// A report from the broadcast manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BcmReport {
    /// What happened (`opcode`: one of [`bcm::op`]'s `RX_CHANGED`,
    /// `RX_TIMEOUT`, `TX_EXPIRED`, `TX_STATUS`, `RX_STATUS`), for which
    /// job, and for `RX_CHANGED` the frame.
    pub msg: BcmMessage,
    /// When: for a frame, the time it was on the bus.
    pub timestamp_ns: u64,
}

/// A broadcast manager on one interface, the equivalent of a `CAN_BCM`
/// socket: jobs that send frames cyclically, and jobs that report when a
/// received frame changes or stays away.
///
/// The jobs and their timers run inside the daemon on a real-time thread,
/// so a cyclic frame leaves on time whether or not this process is
/// scheduled. They end when the socket is dropped.
pub struct BcmSocket {
    conn: Conn,
}

impl BcmSocket {
    pub fn open(ifname: &str) -> Result<BcmSocket> {
        BcmSocket::open_at(&default_socket_path(), ifname)
    }

    pub fn open_at(socket_path: &Path, ifname: &str) -> Result<BcmSocket> {
        let conn = Conn::connect(socket_path)?;
        conn.open(&Message::OpenBcm {
            ifname: ifname.to_string(),
        })?;
        Ok(BcmSocket { conn })
    }

    /// Make a request: set up, change, read or delete a job, or send one
    /// frame. Returns once the daemon has carried it out, or refused it.
    /// See [`BcmMessage`] for ready-made requests.
    pub fn send(&self, msg: &BcmMessage) -> Result<()> {
        self.conn.send_acked(&Message::Bcm(msg.clone()))
    }

    /// Block until the next report (or the read timeout elapses; see
    /// [`Error::is_timeout`]). A frame that a job could not hand to the
    /// adapter is reported here as an error, once when it begins.
    pub fn recv(&self) -> Result<BcmReport> {
        bcm_report(self.conn.recv()?)
    }

    /// A report that has arrived, without waiting for one.
    pub fn try_recv(&self) -> Result<Option<BcmReport>> {
        self.conn.try_recv()?.map(bcm_report).transpose()
    }

    /// Limit how long [`recv`](Self::recv) blocks. `None` blocks forever.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        Ok(self.conn.stream.set_read_timeout(timeout)?)
    }

    /// Let a signal end a blocked receive with an `Interrupted` error,
    /// for the C API.
    pub(crate) fn set_interruptible(&self, on: bool) {
        self.conn.interruptible.store(on, Ordering::Relaxed);
    }
}

fn bcm_report(msg: Message) -> Result<BcmReport> {
    match msg {
        Message::BcmNotify { timestamp_ns, msg } => Ok(BcmReport { msg, timestamp_ns }),
        _ => Err(Error::Protocol(
            "unexpected message on broadcast manager socket",
        )),
    }
}

impl AsRawFd for BcmSocket {
    /// A descriptor to wait on, as for [`RawSocket`].
    fn as_raw_fd(&self) -> RawFd {
        self.conn.readiness_fd()
    }
}

/// What a J1939 socket receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum J1939Event {
    Message(J1939Message),
    /// The progress of a message; see [`J1939Socket::set_notices`].
    Notice(J1939Notice),
}

fn j1939_event(msg: Message) -> Result<J1939Event> {
    match msg {
        Message::J1939Data(m) => Ok(J1939Event::Message(m)),
        Message::J1939Notice(n) => Ok(J1939Event::Notice(n)),
        _ => Err(Error::Protocol("unexpected message on J1939 socket")),
    }
}

/// An SAE J1939 socket on one interface, the equivalent of a `CAN_J1939`
/// socket.
///
/// The socket is bound to an address or to a NAME, sends messages of any
/// length up to [`j1939::MAX_ETP_SIZE`] to an address, a NAME or everyone,
/// and receives the messages meant for it. Messages of more than eight
/// bytes go through the transport protocols (BAM, RTS/CTS, and the
/// extended protocol above 1785 bytes); those run inside the daemon, which
/// answers for every address the interface's J1939 sockets have, so their
/// timing does not depend on this process being scheduled.
///
/// ```no_run
/// use darwincan::{J1939Addr, J1939Socket};
///
/// // Address 0x80 on can0; hear what is sent to it and to everyone.
/// let sock = J1939Socket::open("can0", J1939Addr::address(0x80))?;
/// sock.set_broadcast(true)?;
/// sock.send_to(&[0xF1, 0xFE, 0x00], &J1939Addr::new(0x00, 0xEA00))?;
/// let msg = sock.recv()?;
/// println!("PGN {:05X} from {:02X}: {:02X?}", msg.src.pgn, msg.src.addr, msg.data);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct J1939Socket {
    conn: Conn,
}

impl J1939Socket {
    /// Open a socket on `ifname` bound to `bind`: the address (or the
    /// NAME whose claimed address) it sends from and receives for, and,
    /// if `bind.pgn` is not [`j1939::NO_PGN`], the one parameter group it
    /// receives. [`J1939Addr::NONE`] gives a socket that only listens.
    pub fn open(ifname: &str, bind: J1939Addr) -> Result<J1939Socket> {
        J1939Socket::open_at(&default_socket_path(), ifname, bind)
    }

    pub fn open_at(socket_path: &Path, ifname: &str, bind: J1939Addr) -> Result<J1939Socket> {
        let conn = Conn::connect(socket_path)?;
        conn.open(&Message::OpenJ1939 {
            ifname: ifname.to_string(),
            addr: bind,
        })?;
        Ok(J1939Socket { conn })
    }

    /// Bind the socket anew, as an application does to move to another
    /// address after losing the one it had claimed.
    pub fn bind(&self, addr: J1939Addr) -> Result<()> {
        self.conn.send_acked(&Message::J1939Bind(addr))
    }

    /// Set where [`send`](Self::send) sends, and with which parameter
    /// group. The socket then receives from that NAME or address only.
    pub fn connect(&self, peer: J1939Addr) -> Result<()> {
        self.conn.send_acked(&Message::J1939Connect(peer))
    }

    /// Send a message to where the socket is connected.
    pub fn send(&self, data: &[u8]) -> Result<()> {
        sendable(data)?;
        self.conn.send_acked(&Message::J1939Send {
            dst: None,
            priority: None,
            data: data.to_vec(),
        })
    }

    /// Send a message of parameter group `dst.pgn` to `dst.name`'s
    /// address or, without a NAME, to `dst.addr`; [`j1939::NO_ADDR`] is
    /// everyone, which the socket must have been allowed
    /// ([`set_broadcast`](Self::set_broadcast)).
    ///
    /// Returns once the daemon has taken the message, or refused it: the
    /// interface is down ([`ErrorCode::InterfaceDown`]), a NAME has no
    /// claimed address ([`ErrorCode::AddressNotAvailable`]), a broadcast
    /// was not allowed ([`ErrorCode::NotPermitted`]), the message is too
    /// long ([`ErrorCode::TooBig`]: 1785 bytes to everyone). How a
    /// message of more than eight bytes fares after that is told in
    /// notices ([`set_notices`](Self::set_notices)).
    pub fn send_to(&self, data: &[u8], dst: &J1939Addr) -> Result<()> {
        sendable(data)?;
        self.conn.send_acked(&Message::J1939Send {
            dst: Some(*dst),
            priority: None,
            data: data.to_vec(),
        })
    }

    /// As [`send`](Self::send), at a priority of this message's own: 0
    /// (highest) to 7.
    pub fn send_with_priority(&self, data: &[u8], priority: u8) -> Result<()> {
        sendable(data)?;
        self.conn.send_acked(&Message::J1939Send {
            dst: None,
            priority: Some(priority),
            data: data.to_vec(),
        })
    }

    /// As [`send_to`](Self::send_to), at a priority of this message's
    /// own: 0 (highest) to 7.
    pub fn send_to_with_priority(&self, data: &[u8], dst: &J1939Addr, priority: u8) -> Result<()> {
        sendable(data)?;
        self.conn.send_acked(&Message::J1939Send {
            dst: Some(*dst),
            priority: Some(priority),
            data: data.to_vec(),
        })
    }

    /// Block until a message or notice arrives (or the read timeout
    /// elapses; see [`Error::is_timeout`]).
    pub fn recv_event(&self) -> Result<J1939Event> {
        j1939_event(self.conn.recv()?)
    }

    /// A message or notice that has arrived, without waiting for one.
    pub fn try_recv_event(&self) -> Result<Option<J1939Event>> {
        self.conn.try_recv()?.map(j1939_event).transpose()
    }

    /// Block until a message arrives. Notices are passed over; use
    /// [`recv_event`](Self::recv_event) to see them.
    pub fn recv(&self) -> Result<J1939Message> {
        loop {
            if let J1939Event::Message(m) = self.recv_event()? {
                return Ok(m);
            }
        }
    }

    /// A message that has arrived, without waiting for one.
    pub fn try_recv(&self) -> Result<Option<J1939Message>> {
        while let Some(event) = self.try_recv_event()? {
            if let J1939Event::Message(m) = event {
                return Ok(Some(m));
            }
        }
        Ok(None)
    }

    /// Replace the receive filters: with any set, a message is received
    /// only if one of them matches its source and parameter group. They
    /// apply on top of what the socket is bound and connected to, and to
    /// a promiscuous socket too.
    pub fn set_filters(&self, filters: &[J1939Filter]) -> Result<()> {
        self.conn
            .send_acked(&Message::J1939Filters(filters.to_vec()))
    }

    /// Receive whatever is on the bus, whoever it is for (default:
    /// false), as `SO_J1939_PROMISC`. Multi-frame messages between two
    /// other nodes are not put together: only a session's receiver can
    /// do that.
    pub fn set_promisc(&self, on: bool) -> Result<()> {
        self.set_option(SockOpt::J1939Promisc, u32::from(on))
    }

    /// Allow sending to everyone and receiving what is sent to everyone
    /// (default: false), as `SO_BROADCAST`.
    pub fn set_broadcast(&self, on: bool) -> Result<()> {
        self.set_option(SockOpt::J1939Broadcast, u32::from(on))
    }

    /// The priority of the messages sent: 0 (highest) to 7 (default: 6),
    /// as `SO_J1939_SEND_PRIO`.
    pub fn set_send_priority(&self, priority: u8) -> Result<()> {
        self.set_option(SockOpt::J1939SendPrio, u32::from(priority))
    }

    /// Be told how messages fare (default: false), in place of
    /// `SO_J1939_ERRQUEUE`: when one this socket sent is out or was given
    /// up, and when a multi-frame message for it begins to arrive or is
    /// given up part-way. The notices come through
    /// [`recv_event`](Self::recv_event), in order with the messages.
    pub fn set_notices(&self, on: bool) -> Result<()> {
        self.set_option(SockOpt::J1939Notices, u32::from(on))
    }

    fn set_option(&self, opt: SockOpt, value: u32) -> Result<()> {
        self.conn.send_acked(&Message::SetOption { opt, value })
    }

    /// Block until the daemon has handled everything asked on this socket
    /// so far.
    pub fn flush(&self) -> Result<()> {
        self.conn.send_acked(&Message::Sync)
    }

    /// Limit how long receive calls block. `None` blocks forever.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        Ok(self.conn.stream.set_read_timeout(timeout)?)
    }

    /// Let a signal end a blocked receive with an `Interrupted` error,
    /// for the C API.
    pub(crate) fn set_interruptible(&self, on: bool) {
        self.conn.interruptible.store(on, Ordering::Relaxed);
    }
}

impl AsRawFd for J1939Socket {
    /// A descriptor to wait on, as for [`RawSocket`].
    fn as_raw_fd(&self) -> RawFd {
        self.conn.readiness_fd()
    }
}

/// Management connection to the daemon.
pub struct Control {
    conn: Conn,
}

impl Control {
    pub fn connect() -> Result<Control> {
        Control::connect_at(&default_socket_path())
    }

    pub fn connect_at(socket_path: &Path) -> Result<Control> {
        Ok(Control {
            conn: Conn::connect(socket_path)?,
        })
    }

    pub fn interfaces(&self) -> Result<Vec<IfaceInfo>> {
        match self.conn.request(&Message::ListIfaces)? {
            Message::Ifaces(list) => Ok(list),
            _ => Err(Error::Protocol("expected interface list")),
        }
    }

    /// Configure a hardware interface and put it on the bus. If it is
    /// already up it is reconfigured.
    pub fn up(&self, name: &str, config: &BusConfig) -> Result<()> {
        self.conn.request_ok(&Message::IfaceUp {
            name: name.to_string(),
            config: *config,
        })
    }

    /// Take a hardware interface off the bus. Its configuration is kept
    /// and reported by [`interfaces`](Self::interfaces).
    pub fn down(&self, name: &str) -> Result<()> {
        self.conn.request_ok(&Message::IfaceDown {
            name: name.to_string(),
        })
    }

    /// Store the configuration of a hardware interface that is down
    /// without putting it on the bus, like `ip link set can0 type can ...`
    /// on Linux. Fails with [`ErrorCode::Busy`] if the interface is up.
    pub fn configure(&self, name: &str, config: &BusConfig) -> Result<()> {
        self.conn.request_ok(&Message::IfaceConfigure {
            name: name.to_string(),
            config: *config,
        })
    }

    /// Give an interface another name. A hardware interface has to be
    /// down ([`ErrorCode::Busy`] otherwise), as on Linux; its adapter
    /// then has the new name every time it is plugged in. Sockets open on
    /// the interface stay open.
    pub fn rename(&self, name: &str, new_name: &str) -> Result<()> {
        self.conn.request_ok(&Message::IfaceRename {
            name: name.to_string(),
            new_name: new_name.to_string(),
        })
    }

    /// Remove a virtual CAN interface. Sockets open on it are told it is
    /// gone.
    pub fn del_vcan(&self, name: &str) -> Result<()> {
        self.conn.request_ok(&Message::DelVcan {
            name: name.to_string(),
        })
    }

    /// Create a virtual CAN interface.
    pub fn add_vcan(&self, name: &str) -> Result<()> {
        self.conn.request_ok(&Message::AddVcan {
            name: name.to_string(),
        })
    }
}
