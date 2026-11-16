// SPDX-License-Identifier: GPL-2.0-only

//! J1939 sockets: one stack per interface, shared by the sockets open on
//! it, as on Linux. The stack answers for every address its sockets have,
//! so a transport session to one of them is held once however many sockets
//! listen, and it runs on its own real-time thread, so the protocol's
//! timing does not depend on any client.

use std::collections::{HashMap, VecDeque};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;

use darwincan_core::wire::{ErrorCode, LinkState, Message, SockOpt};
use darwincan_core::{Frame, J1939Addr};
use darwincan_j1939::{Origin, Output, SendError, J1939};

use crate::bus::{Bus, Sink};
use crate::clock::now_ns;
use crate::priority::make_current_thread_realtime;
use crate::session;
use crate::Shared;

/// How long a frame handed to the adapter may take to be reported sent
/// before everything going out is given up: nothing on the bus is
/// acknowledging.
const ECHO_NS: u64 = 1_000_000_000;

/// How many frames the adapter may hold that it has not reported sent.
/// The stack makes a session's packets as they are handed over, so this
/// is what keeps a long message from flooding the adapter; a few at once
/// let it send them back to back.
const IN_FLIGHT: usize = 4;

pub(crate) enum NodeInput {
    /// A socket opens. Answered with `Ok` on `out`.
    Open {
        id: u64,
        addr: J1939Addr,
        out: SyncSender<Message>,
        /// The client's connection, to end it with if the client does not
        /// take its answers.
        client: UnixStream,
    },
    /// Something a socket asks. Answered with `Ok` or `Refused`.
    Request {
        id: u64,
        msg: Message,
    },
    Close {
        id: u64,
    },
    /// A frame from the bus.
    Frame(Frame, u64),
    /// A frame of the stack's own was on the bus.
    Sent(Frame, u64),
    /// A frame of the stack's own will not be sent.
    Dropped,
    /// The interface is gone.
    Removed,
}

struct NodeSink {
    id: u64,
    input: Sender<NodeInput>,
}

impl Sink for NodeSink {
    fn id(&self) -> u64 {
        self.id
    }

    fn deliver(&self, frame: &Frame, timestamp_ns: u64, own: bool) {
        // J1939 is 29-bit identifiers on classic CAN.
        if !frame.is_extended() || frame.is_fd() || frame.is_error() || frame.is_remote() {
            return;
        }
        let _ = self.input.send(if own {
            NodeInput::Sent(*frame, timestamp_ns)
        } else {
            NodeInput::Frame(*frame, timestamp_ns)
        });
    }

    fn bus_removed(&self) {
        let _ = self.input.send(NodeInput::Removed);
    }

    fn transmit_dropped(&self) {
        let _ = self.input.send(NodeInput::Dropped);
    }
}

/// The stacks that are running, by their interface's index. A stack runs
/// while a socket is open on its interface.
#[derive(Default)]
pub(crate) struct Nodes {
    running: Mutex<HashMap<u32, Sender<NodeInput>>>,
}

impl Nodes {
    /// Open socket `id` on `bus`, starting the interface's stack if this
    /// is its first socket. Returns where the socket's requests go.
    pub(crate) fn open(
        &self,
        shared: &Arc<Shared>,
        bus: &Arc<Bus>,
        id: u64,
        addr: J1939Addr,
        out: SyncSender<Message>,
        client: UnixStream,
    ) -> Option<Sender<NodeInput>> {
        // Held until the socket is with a stack: a stack that finds
        // itself without sockets looks for one on its way under this
        // lock before it ends.
        let mut running = self.running.lock().unwrap();
        let mut open = NodeInput::Open {
            id,
            addr,
            out,
            client,
        };
        if let Some(node) = running.get(&bus.index) {
            match node.send(open) {
                Ok(()) => return Some(node.clone()),
                // The stack has ended; start another.
                Err(mpsc::SendError(back)) => open = back,
            }
        }
        let (input, input_rx) = mpsc::channel();
        let node_id = shared.next_id();
        input.send(open).ok()?;
        let spawned = {
            let shared = shared.clone();
            let bus = bus.clone();
            thread::Builder::new()
                .name("darwincand-j1939".into())
                .spawn(move || node_loop(shared, bus, node_id, input_rx))
        };
        spawned.ok()?;
        bus.add_sink(
            Arc::new(NodeSink {
                id: node_id,
                input: input.clone(),
            }),
            || {},
        );
        running.insert(bus.index, input.clone());
        Some(input)
    }
}

fn refused(code: ErrorCode, message: impl Into<String>) -> Message {
    Message::Refused {
        code,
        message: message.into(),
    }
}

/// Carry out what a socket asks and say how it went.
fn request(stack: &mut J1939, bus: &Bus, id: u64, msg: Message) -> Message {
    let done = match msg {
        Message::Sync => Ok(()),
        Message::J1939Bind(addr) => stack.bind(id, addr),
        Message::J1939Connect(addr) => stack.connect(id, addr),
        Message::J1939Filters(filters) => stack.set_filters(id, filters),
        Message::SetOption { opt, value } => match opt {
            SockOpt::J1939Promisc => stack.set_promisc(id, value != 0),
            SockOpt::J1939Broadcast => stack.set_broadcast(id, value != 0),
            SockOpt::J1939SendPrio => stack.set_priority(id, value),
            SockOpt::J1939Notices => stack.set_notices(id, value != 0),
            _ => Err("not an option of a J1939 socket"),
        },
        Message::J1939Send {
            dst,
            priority,
            data,
        } => {
            // As a raw socket's send: at once, if the interface cannot.
            let name = bus.name();
            match bus.link_state() {
                LinkState::Up => {}
                LinkState::Down => {
                    return refused(ErrorCode::InterfaceDown, format!("{name} is down"))
                }
                LinkState::BusOff => {
                    return refused(
                        ErrorCode::InterfaceDown,
                        format!("{name} is bus-off; take it down and up again"),
                    )
                }
                LinkState::Removed => {
                    return refused(ErrorCode::InterfaceRemoved, format!("{name} was removed"))
                }
            }
            return match stack.send(id, dst, priority, data, now_ns()) {
                Ok(()) => Message::Ok,
                Err(e) => {
                    let code = match e {
                        SendError::Broadcast => ErrorCode::NotPermitted,
                        SendError::AddressNotAvailable => ErrorCode::AddressNotAvailable,
                        SendError::TooBig => ErrorCode::TooBig,
                        SendError::Busy => ErrorCode::TransmitFailed,
                        SendError::NoSource
                        | SendError::NoDestination
                        | SendError::BadPgn
                        | SendError::NameMismatch => ErrorCode::InvalidRequest,
                    };
                    refused(code, e.to_string())
                }
            };
        }
        _ => Err("request not valid on a J1939 socket"),
    };
    match done {
        Ok(()) => Message::Ok,
        Err(why) => refused(ErrorCode::InvalidRequest, why),
    }
}

/// Runs one interface's J1939 stack: its sessions and their timers.
fn node_loop(shared: Arc<Shared>, bus: Arc<Bus>, node_id: u64, input: Receiver<NodeInput>) {
    make_current_thread_realtime();
    let mut stack = J1939::new();
    let mut outs: HashMap<u64, SyncSender<Message>> = HashMap::new();
    let mut clients: HashMap<u64, UnixStream> = HashMap::new();
    // The frames the adapter has and has not reported sent, in the order
    // they were handed over: each with the socket whose message it
    // belongs to and how long to wait for the report.
    let mut awaiting: VecDeque<(Frame, Option<u64>, u64)> = VecDeque::new();
    // Read ahead of its turn while the stack was about to end.
    let mut ahead: Option<NodeInput> = None;
    // The last socket has closed: the stack ends once what it still has
    // to send (an abort for a message that socket had not finished, or
    // frames it had just sent) is on the bus.
    let mut ending = false;
    loop {
        let now = now_ns();
        while awaiting.len() < IN_FLIGHT {
            let Some((frame, socket)) = stack.poll_transmit(now) else {
                break;
            };
            match bus.transmit(node_id, true, &frame) {
                Ok(()) => awaiting.push_back((frame, socket, now + ECHO_NS)),
                Err(_) => {
                    stack.link_down(now);
                    break;
                }
            }
        }
        if ending && ahead.is_none() && awaiting.is_empty() && !stack.wants_transmit(now) {
            let mut running = shared.j1939.running.lock().unwrap();
            match input.try_recv() {
                // A socket opened meanwhile, or a frame came: carry on.
                Ok(event) => ahead = Some(event),
                Err(_) => {
                    running.remove(&bus.index);
                    drop(running);
                    bus.remove_sink(node_id);
                    return;
                }
            }
        }
        while let Some((id, output)) = stack.poll_output() {
            if let Some(out) = outs.get(&id) {
                // A socket that does not read loses messages; nothing
                // here waits for it.
                let _ = out.try_send(match output {
                    Output::Message(m) => Message::J1939Data(m),
                    Output::Notice(n) => Message::J1939Notice(n),
                });
            }
        }
        let event = match ahead.take() {
            Some(event) => Some(event),
            None => {
                let deadline = match (awaiting.front(), stack.next_deadline()) {
                    (Some((.., echo)), Some(timer)) => Some(timer.min(*echo)),
                    (Some((.., echo)), None) => Some(*echo),
                    (None, timer) => timer,
                };
                if awaiting.len() < IN_FLIGHT && stack.wants_transmit(now_ns()) {
                    // Something became due meanwhile.
                    continue;
                }
                match deadline {
                    None => match input.recv() {
                        Ok(event) => Some(event),
                        Err(_) => return,
                    },
                    Some(deadline) => match crate::session::wait_until(&input, deadline) {
                        Ok(event) => event,
                        Err(()) => return,
                    },
                }
            }
        };
        let now = now_ns();
        match event {
            Some(NodeInput::Frame(frame, at)) => stack.on_frame(&frame, at, now, Origin::Bus),
            Some(NodeInput::Sent(frame, at)) => {
                // Reports come in the order the frames were handed over;
                // one that is missing took its frame with it.
                let socket = match awaiting.iter().position(|(sent, ..)| *sent == frame) {
                    Some(i) => awaiting.drain(..=i).next_back().and_then(|(_, s, _)| s),
                    None => None,
                };
                stack.on_frame(&frame, at, now, Origin::Local(socket));
            }
            Some(NodeInput::Dropped) => {
                awaiting.clear();
                stack.link_down(now);
            }
            Some(NodeInput::Open {
                id,
                addr,
                out,
                client,
            }) => {
                // The session has checked the address.
                let answer = match stack.open(id, addr) {
                    Ok(()) => Message::Ok,
                    Err(why) => Message::Error {
                        code: ErrorCode::InvalidRequest,
                        message: why.into(),
                    },
                };
                // The stack's thread serves every socket on the
                // interface and keeps the protocol's time, so it does not
                // wait long for one client to make room for an answer.
                ending = false;
                if session::give_answer(&out, answer) {
                    outs.insert(id, out);
                    clients.insert(id, client);
                } else {
                    session::hang_up(&client);
                }
            }
            Some(NodeInput::Request { id, msg }) => {
                let answer = request(&mut stack, &bus, id, msg);
                let taken = outs.get(&id).map(|out| session::give_answer(out, answer));
                if taken == Some(false) {
                    // It is not reading what it asks for. Its session
                    // closes the socket here when its connection ends.
                    outs.remove(&id);
                    if let Some(client) = clients.remove(&id) {
                        session::hang_up(&client);
                    }
                }
            }
            Some(NodeInput::Close { id }) => {
                stack.close(id, now);
                outs.remove(&id);
                clients.remove(&id);
                ending = stack.is_empty();
            }
            Some(NodeInput::Removed) => {
                for out in outs.values() {
                    let _ = out.try_send(Message::Error {
                        code: ErrorCode::InterfaceRemoved,
                        message: "the interface was removed (adapter unplugged)".into(),
                    });
                }
            }
            None => {
                if matches!(awaiting.front(), Some((.., echo)) if now >= *echo) {
                    awaiting.clear();
                    stack.link_down(now);
                }
            }
        }
        stack.poll(now);
    }
}
