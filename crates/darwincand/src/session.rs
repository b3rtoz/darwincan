// SPDX-License-Identifier: GPL-2.0-only

//! One client connection: a control session, a raw socket, an ISO-TP
//! channel, a broadcast manager or a J1939 socket, depending on what the
//! client opens.

use std::cell::Cell;
use std::io;
use std::io::{BufWriter, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{
    self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError,
};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use darwincan_core::wire::{
    self, ErrorCode, IfaceInfo, LinkState, Message, MessageReader, SockOpt, ANY_INTERFACE,
};
use darwincan_core::{BcmMessage, Filter, Frame, IsoTpParams};
use darwincan_isotp::{Event, IsoTp};

use crate::bcm::Bcm;
use crate::budget::{Account, COUNTED_FROM};
use crate::bus::{Bus, ConfigureError, Sink};
use crate::clock::now_ns;
use crate::j1939::NodeInput;
use crate::priority::make_current_thread_realtime;
use crate::{AddInterfaceError, RenameError, Shared};

/// Messages buffered per client before frames are dropped. A client that
/// cannot keep up loses frames instead of stalling the bus, as with a full
/// socket receive buffer on Linux.
const OUT_QUEUE: usize = 16 * 1024;
/// How long an answer may wait for room in a client's queue when it comes
/// from a thread that serves more than that client's requests.
const ANSWER_WAIT: Duration = Duration::from_millis(100);
/// The most filters a raw socket takes, as `CAN_RAW_FILTER_MAX` on Linux:
/// they are gone through for every frame on the interface.
const RAW_FILTER_MAX: usize = 512;

/// Queue an answer for a client from a thread that must not be held up by
/// it. A client that awaits an answer is reading, so its queue has room
/// at once or within moments; one whose queue stays full is not reading
/// what it asked for. Returns false for that client, which the caller
/// then drops (see [`hang_up`]).
pub(crate) fn give_answer(out: &SyncSender<Message>, mut msg: Message) -> bool {
    let give_up = Instant::now() + ANSWER_WAIT;
    loop {
        match out.try_send(msg) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(back)) => {
                if Instant::now() >= give_up {
                    return false;
                }
                msg = back;
                thread::sleep(Duration::from_micros(200));
            }
        }
    }
}

/// Queue an ISO-TP or J1939 message for a client to read, unless the
/// client has too much waiting already: it is then dropped, as a frame
/// is for a client that does not keep up. Never waits.
pub(crate) fn queue_data(out: &SyncSender<Message>, account: &Account, msg: Message) {
    let bytes = data_len(&msg);
    if !account.queue(bytes) {
        return;
    }
    if out.try_send(msg).is_err() {
        account.unqueue(bytes);
    }
}

/// The length of the data a message carries to a client, if it is one
/// that is counted.
fn data_len(msg: &Message) -> usize {
    match msg {
        Message::IsoTpData { data, .. } => data.len(),
        Message::J1939Data(m) => m.data.len(),
        _ => 0,
    }
}

/// End a client's connection from another thread than its session's: the
/// session sees the end of its input and tidies up as for any client that
/// has gone.
pub(crate) fn hang_up(stream: &UnixStream) {
    let _ = stream.shutdown(Shutdown::Both);
}

/// Timers sleep until this close to their deadline, then spin. The timer
/// thread is real-time (see `make_current_thread_realtime`) and is woken
/// some tens of microseconds late; the margin covers that with room to
/// spare, and stays within the processor time the thread declares.
const SPIN_NS: u64 = 200_000;

struct RawSink {
    id: u64,
    /// The index of the interface the socket is open on.
    ifindex: u32,
    out: SyncSender<Message>,
    filters: RwLock<Vec<Filter>>,
    loopback: AtomicBool,
    recv_own: AtomicBool,
    /// A frame must match every filter, not just one.
    join_filters: AtomicBool,
    /// Error classes this socket wants error frames for.
    err_filter: AtomicU32,
    /// Frames lost because the client's queue was full, and how many of
    /// them the client has been told of.
    dropped: AtomicU64,
    dropped_told: AtomicU64,
}

impl RawSink {
    fn new(id: u64, ifindex: u32, out: SyncSender<Message>) -> RawSink {
        RawSink {
            id,
            ifindex,
            out,
            filters: RwLock::new(vec![Filter::ANY]),
            loopback: AtomicBool::new(true),
            recv_own: AtomicBool::new(false),
            join_filters: AtomicBool::new(false),
            err_filter: AtomicU32::new(0),
            dropped: AtomicU64::new(0),
            dropped_told: AtomicU64::new(0),
        }
    }

    /// A frame from the interface with index `ifindex`.
    fn deliver_from(&self, ifindex: u32, frame: &Frame, timestamp_ns: u64, own: bool, local: bool) {
        if own && !self.recv_own.load(Ordering::Relaxed) {
            return;
        }
        let id = frame.can_id();
        if frame.is_error() {
            // Error frames bypass the id filters and have their own mask.
            if id & self.err_filter.load(Ordering::Relaxed) & darwincan_core::err::MASK == 0 {
                return;
            }
        } else {
            let filters = self.filters.read().unwrap();
            let passes = if self.join_filters.load(Ordering::Relaxed) {
                !filters.is_empty() && filters.iter().all(|f| f.matches(id))
            } else {
                filters.iter().any(|f| f.matches(id))
            };
            if !passes {
                return;
            }
        }
        // After a loss, the count goes ahead of the next frame that fits.
        let dropped = self.dropped.load(Ordering::Relaxed);
        if dropped != self.dropped_told.load(Ordering::Relaxed)
            && self
                .out
                .try_send(Message::Dropped { total: dropped })
                .is_ok()
        {
            self.dropped_told.store(dropped, Ordering::Relaxed);
        }
        let msg = Message::Frame {
            ifindex,
            timestamp_ns,
            own,
            local,
            frame: *frame,
        };
        if self.out.try_send(msg).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Sink for RawSink {
    fn id(&self) -> u64 {
        self.id
    }

    fn deliver(&self, frame: &Frame, timestamp_ns: u64, own: bool) {
        self.deliver_from(self.ifindex, frame, timestamp_ns, own, own);
    }

    fn deliver_from_origin(&self, frame: &Frame, timestamp_ns: u64, own: bool, local: bool) {
        self.deliver_from(self.ifindex, frame, timestamp_ns, own, local);
    }

    fn link_state(&self, state: LinkState) {
        // If this is lost to a full queue the client finds out when it
        // asks, which it does before it refuses to send.
        let _ = self.out.try_send(Message::LinkState(state));
    }

    fn bus_removed(&self) {
        let _ = self.out.try_send(error(
            ErrorCode::InterfaceRemoved,
            "the interface was removed (adapter unplugged)",
        ));
    }
}

/// One interface's feed into a socket that is open on every interface.
struct Tap {
    socket: Arc<RawSink>,
    ifindex: u32,
}

impl Sink for Tap {
    fn id(&self) -> u64 {
        self.socket.id
    }

    fn deliver(&self, frame: &Frame, timestamp_ns: u64, own: bool) {
        self.socket
            .deliver_from(self.ifindex, frame, timestamp_ns, own, own);
    }

    fn deliver_from_origin(&self, frame: &Frame, timestamp_ns: u64, own: bool, local: bool) {
        self.socket
            .deliver_from(self.ifindex, frame, timestamp_ns, own, local);
    }
}

enum IsoInput {
    Frame(Frame),
    /// A frame of ours was on the bus at this time.
    Sent(Frame, u64),
    /// A frame of ours will not be sent.
    Dropped,
    /// A message to send, and what was counted for it when it was read.
    Send(Vec<u8>, usize),
    Close,
}

struct IsoSink {
    id: u64,
    rx_id: u32,
    /// Whether the bus reports our frames once they have been sent.
    confirms: bool,
    input: Sender<IsoInput>,
}

impl Sink for IsoSink {
    fn id(&self) -> u64 {
        self.id
    }

    fn deliver(&self, frame: &Frame, timestamp_ns: u64, own: bool) {
        if own {
            if self.confirms {
                let _ = self.input.send(IsoInput::Sent(*frame, timestamp_ns));
            }
        } else if frame.can_id() == self.rx_id {
            // Comparing the whole can_id also excludes remote and error
            // frames.
            let _ = self.input.send(IsoInput::Frame(*frame));
        }
    }

    fn transmit_dropped(&self) {
        let _ = self.input.send(IsoInput::Dropped);
    }
}

enum BcmInput {
    Request(BcmMessage),
    /// A frame a receive job watches was on the bus at this time.
    Frame(Frame, u64),
    Sync,
    Close,
}

struct BcmSink {
    id: u64,
    /// The identifiers the receive jobs watch, with whether each is for
    /// CAN FD frames. Kept by the broadcast manager's thread, so that only
    /// frames a job wants cross over to it.
    watched: Arc<RwLock<Vec<(u32, bool)>>>,
    input: Sender<BcmInput>,
    out: SyncSender<Message>,
}

impl Sink for BcmSink {
    fn id(&self) -> u64 {
        self.id
    }

    fn deliver(&self, frame: &Frame, timestamp_ns: u64, _own: bool) {
        // A job sees the frames of its own client's transmit jobs too, as
        // on Linux.
        let key = (frame.can_id(), frame.is_fd());
        if self.watched.read().unwrap().contains(&key) {
            let _ = self.input.send(BcmInput::Frame(*frame, timestamp_ns));
        }
    }

    fn bus_removed(&self) {
        let _ = self.out.try_send(error(
            ErrorCode::InterfaceRemoved,
            "the interface was removed (adapter unplugged)",
        ));
    }
}

enum Mode {
    Control,
    Raw {
        bus: Arc<Bus>,
        sink: Arc<RawSink>,
    },
    /// A raw socket on every interface.
    RawEverywhere {
        sink: Arc<RawSink>,
    },
    IsoTp {
        bus: Arc<Bus>,
        id: u64,
        input: Sender<IsoInput>,
    },
    Bcm {
        bus: Arc<Bus>,
        id: u64,
        input: Sender<BcmInput>,
    },
    /// A J1939 socket: its interface's stack does the work.
    J1939 {
        id: u64,
        node: Sender<NodeInput>,
    },
}

fn error(code: ErrorCode, message: impl Into<String>) -> Message {
    Message::Error {
        code,
        message: message.into(),
    }
}

pub(crate) fn run(shared: Arc<Shared>, stream: UnixStream) {
    make_current_thread_realtime();
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let (out, out_rx) = mpsc::sync_channel::<Message>(OUT_QUEUE);
    let account = Account::new(&shared.budget);
    let _ = {
        let account = account.clone();
        thread::Builder::new()
            .name("darwincand-writer".into())
            .spawn(move || writer_loop(write_half, out_rx, account))
    };

    let mut reader = MessageReader::new(read_half);
    // A long message is counted before it is read, and waited with while
    // there is no room for it: the client's send then waits, as it does
    // on a socket whose buffer is full.
    {
        let account = account.clone();
        reader.set_gate(COUNTED_FROM, move |bytes| account.reserve(bytes));
    }
    let mut mode = Mode::Control;
    let opened = Cell::new(true);

    loop {
        let msg = match reader.read() {
            Ok(msg) => msg,
            Err(e) => {
                // A client from another build: say so before hanging up.
                // It reads the version in the answer's header and reports
                // the mismatch; without an answer it would only see the
                // connection close. Written here and not through the
                // writer, which the hanging up below would overtake.
                if let Some(mismatch) = wire::version_mismatch(&e) {
                    let _ = error(ErrorCode::Unsupported, mismatch.to_string()).write_to(&stream);
                }
                break;
            }
        };
        // What was counted for this message when it was read. Whoever
        // takes the message takes this too; if nobody does, it is given
        // back below.
        let mut reserved = reader.last_gated();
        let reply = match (&mode, msg) {
            (Mode::Control, Message::ListIfaces) => Some(Message::Ifaces(
                shared
                    .buses
                    .read()
                    .unwrap()
                    .values()
                    .map(|b| IfaceInfo {
                        index: b.index,
                        name: b.name(),
                        kind: b.kind().to_string(),
                        description: b.description(),
                        up: b.is_up(),
                        config: b.config(),
                    })
                    .collect(),
            )),
            (Mode::Control, Message::AddVcan { name }) => Some(match shared.add_bus(&name, None) {
                Ok(_) => Message::Ok,
                Err(e @ AddInterfaceError::InvalidName) => {
                    error(ErrorCode::InvalidRequest, e.to_string())
                }
                Err(e @ (AddInterfaceError::AlreadyExists | AddInterfaceError::Reserved)) => {
                    error(ErrorCode::AlreadyExists, e.to_string())
                }
            }),
            (Mode::Control, Message::IfaceUp { name, config }) => Some(match shared.bus(&name) {
                None => no_such_interface(&name),
                Some(bus) => match bus.start(&config) {
                    Ok(()) => Message::Ok,
                    Err(e) => transmit_error(&e),
                },
            }),
            (Mode::Control, Message::IfaceDown { name }) => Some(match shared.bus(&name) {
                None => no_such_interface(&name),
                Some(bus) => match bus.stop() {
                    Ok(()) => Message::Ok,
                    Err(e) => transmit_error(&e),
                },
            }),
            (Mode::Control, Message::IfaceConfigure { name, config }) => {
                Some(match shared.bus(&name).map(|bus| bus.configure(&config)) {
                    None => no_such_interface(&name),
                    Some(Ok(())) => Message::Ok,
                    Some(Err(ConfigureError::Virtual)) => error(
                        ErrorCode::Unsupported,
                        format!("{name} is virtual and has no bus settings"),
                    ),
                    Some(Err(ConfigureError::Up)) => error(
                        ErrorCode::Busy,
                        format!("{name} is up; take it down before changing its settings"),
                    ),
                })
            }
            (Mode::Control, Message::IfaceRename { name, new_name }) => {
                Some(match shared.rename(&name, &new_name) {
                    Ok(()) => Message::Ok,
                    Err(RenameError::NoSuchInterface) => no_such_interface(&name),
                    Err(RenameError::InvalidName) => error(
                        ErrorCode::InvalidRequest,
                        AddInterfaceError::InvalidName.to_string(),
                    ),
                    Err(RenameError::AlreadyExists) => error(
                        ErrorCode::AlreadyExists,
                        format!("there is an interface called {new_name} already"),
                    ),
                    Err(RenameError::Reserved) => error(
                        ErrorCode::AlreadyExists,
                        format!("{new_name} is kept for an adapter that is not plugged in"),
                    ),
                    Err(RenameError::Up) => error(
                        ErrorCode::Busy,
                        format!("{name} is up; take it down before changing its name"),
                    ),
                })
            }
            (Mode::Control, Message::DelVcan { name }) => Some({
                let mut buses = shared.buses.write().unwrap();
                match buses.get(&name).map(|bus| bus.is_virtual()) {
                    None => no_such_interface(&name),
                    Some(false) => error(
                        ErrorCode::Unsupported,
                        format!("{name} is a hardware interface; it goes away with its adapter"),
                    ),
                    Some(true) => {
                        let bus = buses.remove(&name).expect("present under the lock");
                        drop(buses);
                        bus.mark_removed();
                        Message::Ok
                    }
                }
            }),
            (Mode::Control, Message::OpenRaw { ifname }) if ifname == ANY_INTERFACE => {
                let sink = Arc::new(RawSink::new(shared.next_id(), 0, out.clone()));
                let socket = sink.clone();
                shared.attach_everywhere(
                    sink.id,
                    Box::new(move |bus| {
                        let tap = Arc::new(Tap {
                            socket: socket.clone(),
                            ifindex: bus.index,
                        });
                        bus.add_sink(tap, || {});
                    }),
                    || opened.set(out.try_send(Message::Ok).is_ok()),
                );
                mode = Mode::RawEverywhere { sink };
                None
            }
            (Mode::Control, Message::OpenRaw { ifname }) => match shared.bus(&ifname) {
                None => Some(no_such_interface(&ifname)),
                Some(bus) => {
                    let sink = Arc::new(RawSink::new(shared.next_id(), bus.index, out.clone()));
                    bus.add_sink(sink.clone(), || {
                        // The state first, so that the client has it by
                        // the time it knows the socket is open.
                        opened.set(
                            out.try_send(Message::LinkState(bus.link_state())).is_ok()
                                && out.try_send(Message::Ok).is_ok(),
                        );
                    });
                    mode = Mode::Raw { bus, sink };
                    None
                }
            },
            (Mode::Control, Message::OpenIsoTp { ifname, params }) => {
                match (shared.bus(&ifname), params.validate()) {
                    (None, _) => Some(no_such_interface(&ifname)),
                    (_, Err(why)) => Some(error(ErrorCode::InvalidRequest, why)),
                    (Some(bus), Ok(())) => {
                        let id = shared.next_id();
                        let (input, input_rx) = mpsc::channel();
                        let sink = Arc::new(IsoSink {
                            id,
                            rx_id: params.rx_id,
                            confirms: bus.confirms_transmission(),
                            input: input.clone(),
                        });
                        let spawned = {
                            let bus = bus.clone();
                            let out = out.clone();
                            let account = account.clone();
                            thread::Builder::new()
                                .name("darwincand-isotp".into())
                                .spawn(move || isotp_loop(bus, id, params, input_rx, out, account))
                        };
                        if spawned.is_err() {
                            Some(error(ErrorCode::Other, "could not start ISO-TP thread"))
                        } else {
                            bus.add_sink(sink, || opened.set(out.try_send(Message::Ok).is_ok()));
                            mode = Mode::IsoTp { bus, id, input };
                            None
                        }
                    }
                }
            }

            (Mode::Raw { bus, .. }, Message::QueryLink) => {
                if out.send(Message::LinkState(bus.link_state())).is_err() {
                    break;
                }
                Some(Message::Ok)
            }
            (Mode::RawEverywhere { .. }, Message::QueryLink) => Some(Message::Ok),
            (Mode::Raw { bus, sink }, Message::SendTo { ifindex, frame }) => {
                if ifindex != bus.index {
                    Some(error(
                        ErrorCode::InvalidRequest,
                        format!("this socket is open on {} only", bus.name()),
                    ))
                } else {
                    let loopback = sink.loopback.load(Ordering::Relaxed);
                    match bus.transmit(sink.id, loopback, &frame) {
                        Ok(()) => None,
                        Err(e) => Some(transmit_error(&e)),
                    }
                }
            }
            (Mode::RawEverywhere { sink }, Message::SendTo { ifindex, frame }) => {
                match shared.bus_by_index(ifindex) {
                    None => Some(error(
                        ErrorCode::NoSuchInterface,
                        format!("no interface has index {ifindex}"),
                    )),
                    Some(bus) => {
                        let loopback = sink.loopback.load(Ordering::Relaxed);
                        match bus.transmit(sink.id, loopback, &frame) {
                            Ok(()) => None,
                            Err(e) => Some(transmit_error(&e)),
                        }
                    }
                }
            }
            (Mode::RawEverywhere { .. }, Message::Send(_)) => Some(error(
                ErrorCode::InvalidRequest,
                "a socket open on every interface must say which one to send on",
            )),
            (
                Mode::Raw { sink, .. } | Mode::RawEverywhere { sink },
                Message::SetFilters(filters),
            ) => {
                if filters.len() > RAW_FILTER_MAX {
                    // In the place of the `Ok` the client waits for.
                    Some(Message::Refused {
                        code: ErrorCode::InvalidRequest,
                        message: format!("a socket takes at most {RAW_FILTER_MAX} filters"),
                    })
                } else {
                    *sink.filters.write().unwrap() = filters;
                    Some(Message::Ok)
                }
            }
            (
                Mode::Raw { sink, .. } | Mode::RawEverywhere { sink },
                Message::SetOption { opt, value },
            ) => {
                let mut answer = Message::Ok;
                match opt {
                    SockOpt::Loopback => sink.loopback.store(value != 0, Ordering::Relaxed),
                    SockOpt::RecvOwnMsgs => sink.recv_own.store(value != 0, Ordering::Relaxed),
                    SockOpt::ErrFilter => sink.err_filter.store(value, Ordering::Relaxed),
                    SockOpt::JoinFilters => sink.join_filters.store(value != 0, Ordering::Relaxed),
                    SockOpt::J1939Promisc
                    | SockOpt::J1939Broadcast
                    | SockOpt::J1939SendPrio
                    | SockOpt::J1939Notices => {
                        answer = Message::Refused {
                            code: ErrorCode::InvalidRequest,
                            message: "not an option of a raw socket".into(),
                        }
                    }
                }
                Some(answer)
            }
            (Mode::Raw { bus, sink }, Message::Send(frame)) => {
                let loopback = sink.loopback.load(Ordering::Relaxed);
                match bus.transmit(sink.id, loopback, &frame) {
                    Ok(()) => None,
                    Err(e) => Some(transmit_error(&e)),
                }
            }

            (Mode::Control, Message::OpenBcm { ifname }) => match shared.bus(&ifname) {
                None => Some(no_such_interface(&ifname)),
                Some(bus) => {
                    let id = shared.next_id();
                    let (input, input_rx) = mpsc::channel();
                    let watched = Arc::new(RwLock::new(Vec::new()));
                    let sink = Arc::new(BcmSink {
                        id,
                        watched: watched.clone(),
                        input: input.clone(),
                        out: out.clone(),
                    });
                    let spawned = {
                        let bus = bus.clone();
                        let out = out.clone();
                        stream.try_clone().and_then(|client| {
                            thread::Builder::new()
                                .name("darwincand-bcm".into())
                                .spawn(move || bcm_loop(bus, id, watched, input_rx, out, client))
                        })
                    };
                    if spawned.is_err() {
                        Some(error(
                            ErrorCode::Other,
                            "could not start broadcast manager thread",
                        ))
                    } else {
                        bus.add_sink(sink, || opened.set(out.try_send(Message::Ok).is_ok()));
                        mode = Mode::Bcm { bus, id, input };
                        None
                    }
                }
            },
            // Answered by the broadcast manager's thread, so that the
            // answers stay in step with what that thread reports.
            (Mode::Bcm { input, .. }, Message::Bcm(msg)) => {
                let _ = input.send(BcmInput::Request(msg));
                None
            }
            (Mode::Bcm { input, .. }, Message::Sync) => {
                let _ = input.send(BcmInput::Sync);
                None
            }

            (Mode::Control, Message::OpenJ1939 { ifname, addr }) => {
                match (
                    shared.bus(&ifname),
                    darwincan_j1939::J1939::check_bind(&addr),
                ) {
                    (None, _) => Some(no_such_interface(&ifname)),
                    (_, Err(why)) => Some(error(ErrorCode::InvalidRequest, why)),
                    (Some(bus), Ok(())) => {
                        let id = shared.next_id();
                        // The stack answers, once the socket is open.
                        let node = stream.try_clone().ok().and_then(|client| {
                            let open = NodeInput::Open {
                                id,
                                addr,
                                out: out.clone(),
                                client,
                                account: account.clone(),
                            };
                            shared.j1939.open(&shared, &bus, open)
                        });
                        match node {
                            Some(node) => {
                                mode = Mode::J1939 { id, node };
                                None
                            }
                            None => Some(error(ErrorCode::Other, "could not start J1939 thread")),
                        }
                    }
                }
            }
            // Answered by the stack's thread, in step with what it sends.
            (
                Mode::J1939 { id, node },
                msg @ (Message::J1939Bind(_)
                | Message::J1939Connect(_)
                | Message::J1939Filters(_)
                | Message::J1939Send { .. }
                | Message::SetOption { .. }
                | Message::Sync),
            ) => {
                let request = NodeInput::Request {
                    id: *id,
                    msg,
                    reserved: std::mem::take(&mut reserved),
                };
                if node.send(request).is_err() {
                    break;
                }
                None
            }

            (_, Message::Sync) => Some(Message::Ok),

            (Mode::IsoTp { input, .. }, Message::IsoTpSend(data)) => {
                let _ = input.send(IsoInput::Send(data, std::mem::take(&mut reserved)));
                None
            }

            _ => Some(error(
                ErrorCode::InvalidRequest,
                "request not valid in this state",
            )),
        };
        account.release(reserved);
        if let Some(reply) = reply {
            if out.send(reply).is_err() {
                break;
            }
        }
        // An open is acknowledged while the interface's list of sockets
        // is held, where nothing may wait. The queue of a client that has
        // only just opened is full only if it has been filling it without
        // reading, and such a client is dropped.
        if !opened.get() {
            break;
        }
    }

    match mode {
        Mode::Control => {}
        Mode::Raw { bus, sink } => bus.remove_sink(sink.id),
        Mode::RawEverywhere { sink } => shared.detach_everywhere(sink.id),
        Mode::IsoTp { bus, id, input } => {
            bus.remove_sink(id);
            let _ = input.send(IsoInput::Close);
        }
        Mode::Bcm { bus, id, input } => {
            bus.remove_sink(id);
            let _ = input.send(BcmInput::Close);
        }
        Mode::J1939 { id, node } => {
            let _ = node.send(NodeInput::Close { id });
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

/// Report a failed bus operation with the most specific code available.
fn transmit_error(e: &io::Error) -> Message {
    let code = match e.kind() {
        io::ErrorKind::NotConnected => ErrorCode::InterfaceDown,
        io::ErrorKind::NotFound => ErrorCode::InterfaceRemoved,
        io::ErrorKind::InvalidInput => ErrorCode::InvalidRequest,
        io::ErrorKind::Unsupported => ErrorCode::Unsupported,
        _ => ErrorCode::TransmitFailed,
    };
    error(code, e.to_string())
}

fn no_such_interface(ifname: &str) -> Message {
    error(
        ErrorCode::NoSuchInterface,
        format!("no such interface: {ifname}"),
    )
}

/// Drains the client's queue to its socket, flushing whenever the queue
/// runs empty so a lone frame is never held back.
fn writer_loop(stream: UnixStream, rx: Receiver<Message>, account: Arc<Account>) {
    make_current_thread_realtime();
    let mut w = BufWriter::with_capacity(64 * 1024, stream);
    let mut buf = Vec::with_capacity(256);
    while let Ok(first) = rx.recv() {
        let mut next = Some(first);
        while let Some(msg) = next {
            buf.clear();
            msg.encode(&mut buf);
            let counted = data_len(&msg);
            drop(msg);
            let written = w.write_all(&buf);
            if counted > 0 {
                account.unqueue(counted);
                // The room a long message took is not kept for the next.
                if buf.capacity() > COUNTED_FROM {
                    buf = Vec::with_capacity(256);
                }
            }
            if written.is_err() {
                return;
            }
            next = rx.try_recv().ok();
        }
        if w.flush().is_err() {
            return;
        }
    }
}

/// Wait for input until `deadline`. `Ok(None)` means the deadline passed.
pub(crate) fn wait_until<T>(rx: &Receiver<T>, deadline: u64) -> Result<Option<T>, ()> {
    loop {
        let now = now_ns();
        if now >= deadline {
            return Ok(None);
        }
        let remaining = deadline - now;
        if remaining > SPIN_NS {
            match rx.recv_timeout(Duration::from_nanos(remaining - SPIN_NS)) {
                Ok(input) => return Ok(Some(input)),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(()),
            }
        } else {
            match rx.try_recv() {
                Ok(input) => return Ok(Some(input)),
                Err(TryRecvError::Empty) => std::hint::spin_loop(),
                Err(TryRecvError::Disconnected) => return Err(()),
            }
        }
    }
}

/// Runs one ISO-TP channel: owns the protocol state machine and its timers.
fn isotp_loop(
    bus: Arc<Bus>,
    id: u64,
    params: IsoTpParams,
    input: Receiver<IsoInput>,
    out: SyncSender<Message>,
    account: Arc<Account>,
) {
    make_current_thread_realtime();
    let confirms = bus.confirms_transmission();
    let mut tp = IsoTp::new(params);
    // What is counted for the message being sent.
    let mut sending = 0usize;
    loop {
        let event = match tp.next_deadline() {
            None => match input.recv() {
                Ok(input) => Some(input),
                Err(_) => return,
            },
            Some(deadline) => match wait_until(&input, deadline) {
                Ok(input) => input,
                Err(()) => return,
            },
        };
        match event {
            Some(IsoInput::Frame(frame)) => tp.on_frame(&frame, now_ns()),
            // The adapter stamps a frame at its end, which is where STmin
            // counts from. On the daemon's clock that time is never early
            // (see `TimeSync`), and the next frame cannot start before it
            // is handed over, so the gap on the bus is never under STmin.
            // It is longer by what the report takes to arrive and the
            // frame takes to start: 0.2 to 1.2 ms on a PCAN-USB X6, which
            // appears to send in steps of 1 ms. Handing over early by a measured amount was tried and undercut STmin
            // once in 35,000 frames, so it is not done.
            Some(IsoInput::Sent(frame, at)) => tp.frame_transmitted(&frame, at),
            Some(IsoInput::Dropped) => tp.abort_send(),
            Some(IsoInput::Send(data, reserved)) => {
                match tp.send(data, now_ns()) {
                    // Counted until the message is out or given up.
                    Ok(()) => sending = reserved,
                    Err(reason) => {
                        account.release(reserved);
                        let _ = give_answer(&out, Message::IsoTpFailed { tx: true, reason });
                    }
                }
            }
            Some(IsoInput::Close) => return,
            None => {}
        }
        tp.poll(now_ns());

        while let Some(frame) = tp.poll_transmit() {
            match bus.transmit(id, true, &frame) {
                // The bus will say when the frame was sent.
                Ok(()) if confirms => tp.frame_pending(&frame, now_ns()),
                // STmin counts from the end of the frame. This adapter
                // does not say when that is, so take the earliest it can
                // be: now, plus the frame's time on the wire.
                Ok(()) => tp.frame_transmitted(&frame, now_ns() + bus.frame_time_ns(&frame)),
                Err(_) => {
                    // The rest of the message would fare no better, and
                    // each frame could wait for room in the adapter.
                    tp.abort_send();
                    while tp.poll_transmit().is_some() {}
                }
            }
        }
        while let Some(event) = tp.poll_event() {
            let msg = match event {
                Event::Received(data) => Message::IsoTpData {
                    timestamp_ns: now_ns(),
                    data,
                },
                Event::TxDone => Message::IsoTpTxDone,
                Event::TxFailed(reason) => Message::IsoTpFailed { tx: true, reason },
                Event::RxFailed(reason) => Message::IsoTpFailed { tx: false, reason },
            };
            // The end of a send is waited for: it is not dropped for a
            // queue that is full at this moment, as received data is.
            if matches!(
                msg,
                Message::IsoTpTxDone | Message::IsoTpFailed { tx: true, .. }
            ) {
                account.release(std::mem::take(&mut sending));
                let _ = give_answer(&out, msg);
            } else if matches!(msg, Message::IsoTpData { .. }) {
                queue_data(&out, &account, msg);
            } else {
                let _ = out.try_send(msg);
            }
        }
    }
}

/// Runs one client's broadcast manager: its jobs and their timers.
fn bcm_loop(
    bus: Arc<Bus>,
    id: u64,
    watched: Arc<RwLock<Vec<(u32, bool)>>>,
    input: Receiver<BcmInput>,
    out: SyncSender<Message>,
    client: UnixStream,
) {
    make_current_thread_realtime();
    let mut bcm = Bcm::new();
    // Whether the last frame a timer sent could not be handed over: the
    // client is told when that begins, not for every frame of a job.
    let mut failing = false;
    loop {
        let event = match bcm.next_deadline() {
            None => match input.recv() {
                Ok(input) => Some(input),
                Err(_) => return,
            },
            Some(deadline) => match wait_until(&input, deadline) {
                Ok(input) => input,
                Err(()) => return,
            },
        };
        // What the request at hand is answered with, once the frames it
        // made have been handed over.
        let mut answer = None;
        let mut single_send = false;
        match event {
            Some(BcmInput::Request(msg)) => {
                single_send = msg.opcode == darwincan_core::bcm::op::TX_SEND;
                answer = Some(match bcm.request(&msg, now_ns()) {
                    Ok(()) => Message::Ok,
                    Err(why) => Message::Refused {
                        code: ErrorCode::InvalidRequest,
                        message: why.into(),
                    },
                });
                // Before the answer: a frame that arrives after it is
                // seen by the job the request set up.
                *watched.write().unwrap() = bcm.watched();
            }
            Some(BcmInput::Frame(frame, at)) => bcm.on_frame(&frame, at, now_ns()),
            Some(BcmInput::Sync) => answer = Some(Message::Ok),
            Some(BcmInput::Close) => return,
            None => {}
        }
        bcm.poll(now_ns());

        while let Some(frame) = bcm.poll_transmit() {
            match bus.transmit(id, true, &frame) {
                Ok(()) => failing = false,
                // A single send fails as a send on a raw socket does.
                Err(e) if single_send => {
                    if let Message::Error { code, message } = transmit_error(&e) {
                        answer = Some(Message::Refused { code, message });
                    }
                }
                // A job carries on, as on Linux: the interface may come
                // up, or the bus find someone to acknowledge.
                Err(e) => {
                    if !failing {
                        let _ = out.try_send(transmit_error(&e));
                    }
                    failing = true;
                }
            }
        }
        if let Some(msg) = answer {
            // Not without limit: the jobs' timers run on this thread, and
            // requests and watched frames queue up behind it.
            if !give_answer(&out, msg) {
                hang_up(&client);
                return;
            }
        }
        while let Some((timestamp_ns, msg)) = bcm.poll_report() {
            let _ = out.try_send(Message::BcmNotify { timestamp_ns, msg });
        }
    }
}
