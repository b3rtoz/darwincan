# Design decisions

The decisions that shaped darwinCAN, in the order they were made, each with its reason where the reason is not obvious. This is a record, not a specification: [../design.md](../design.md) describes the result, and [../roadmap.md](../roadmap.md) what is planned.

Add a row when a decision is made that someone might later want to revisit: what was chosen, and why not the alternative.

| Date | Decision |
|---|---|
| 2026-11-11 | Clients retry for 500 ms when the daemon is still starting |
| 2026-11-11 | Tools follow can-utils and `ip link` command lines and output; unsupported options are refused, never ignored |
| 2026-11-12 | Real-time scheduling for the daemon's protocol timer threads and for tools that pace frames |
| 2026-11-12 | Start hardware testing with the classic PCAN-USB rather than wait for a CAN FD adapter |
| 2026-11-13 | Order of work: bench close-out, merge, transmit confirmation, CAN FD on the wire; further backends and programming interfaces after that |
| 2026-11-13 | The public programming interface (C API, python-can) is not planned until the bench tests and transmit confirmation are done, because transmit confirmation changes what a client sees of its own frames |
| 2026-11-13 | Transmit confirmation: a backend reports each frame the adapter says was sent; clients see their frames then, with the adapter's time. Frames still with the adapter when an interface goes down are dropped without an error per frame, as on Linux; an ISO-TP transfer in flight fails with "a frame could not be put on the bus" |
| 2026-11-13 | STmin is a strict lower bound on the bus. ISO-TP waits for the adapter's report of each frame; a correction that made gaps shorter undercut STmin once in 35,000 frames and was dropped |
| 2026-11-13 | The daemon's frame-handling threads run in the real-time class, like its protocol timer threads |
| 2026-11-13 | `cansend` returns once the daemon has taken the frame, as on Linux, and no longer waits for its own copy |
| 2026-11-13 | A stop signal takes hardware interfaces off the bus (at most 2 s), then ends the daemon with the same exit status as before |
| 2026-11-14 | So does the USB library's delivery thread: the daemon makes real-time the thread that appears when it begins to watch for adapters |
| 2026-11-14 | The daemon uses the USB library's version 0.2 (`nusb` 0.2.9) and finds that thread by its name. The daemon needs Rust 1.85; the permissive crates stay at 1.75 |
| 2026-11-14 | The programming interface is versioned, with a protocol version check, but carries no stability promise or packaging yet. Four uses count equally: test automation with timing, diagnostics over ISO-TP, porting Linux tools, logging and replay |
| 2026-11-14 | A C API first, a python-can plugin on top of it afterwards, then the Kvaser backend |
| 2026-11-14 | The C API has the structs and constants of `<linux/can.h>` exactly, and its own `dcan_*` functions in place of the socket calls; no shim that lets Linux source compile unchanged |
| 2026-11-14 | The C API is the `darwincan` crate built as a C library, not a crate of its own, with a hand-written header checked by a C program in the tests. Receive calls take a timeout; an ISO-TP send waits until the message is out |
| 2026-11-14 | Made ready for use the same day: joined filters (`CAN_RAW_JOIN_FILTERS`, and `candump`'s `j`), a receive that a signal ends, an ISO-TP channel one thread can receive on while another sends, `make install` with pkg-config, two example programs and docs/c-api.md. Left out on purpose: `CAN_BCM`, DLC above 8, a socket on all interfaces |
| 2026-11-14 | The protocol version goes up with every change to a message. A daemon answers a client of another version before hanging up, so the client can say what is wrong |
| 2026-11-15 | The broadcast manager before Python, to finish the API. It follows `CAN_BCM` operation by operation and flag by flag, with `struct bcm_msg_head` laid out as on Linux in a header of its own. Its jobs run in the daemon, one real-time thread per client. Where it departs from Linux: a job that cannot send says so once, and one broadcast manager serves one interface |
| 2026-11-15 | Order of work from here: remove the differences from Linux that can be removed, then J1939 (wanted for commercial vehicle networks), then python-can, then Kvaser |
| 2026-11-15 | J1939 as Linux has it, in full (the transport protocols and NAMEs, not only listening and single frames), because a port from Linux needs all of it. The protocol is written from the standards, not from the kernel's GPL implementation, so it can live in a permissive crate. One stack per interface, shared by its sockets, as on Linux: a session to a local address is answered once. A J1939 send is acknowledged by the daemon, unlike a raw send, because only the daemon knows whether a NAME has an address. Address claiming is left to the program, as on Linux |
| 2026-11-15 | python-can next, without waiting for a J1939 reference device; the J1939 tools are planned for after it |
| 2026-11-15 | The Python package loads the installed C library with ctypes and has no compiled part, so it needs no build per Python version. A bus given a `bitrate` brings a down interface up at it (with `listen_only` if asked) and leaves it up when closed; an interface that is already up is shared and never reconfigured, so a different `bitrate` fails the open. Without a `bitrate` the interface is left alone, as on Linux. (First built to only compare, as `socketcan` ignores the option; but that is for want of root on Linux, which does not apply here.) A transmit failure reported after the send is logged and counted by `recv`, not raised, so that a reader thread survives it. ISO-TP is a class of the package's own (`IsoTpSocket`), since python-can has no such thing |
| 2026-11-15 | The Kvaser backend is for the hydra protocol first, since the adapter available for testing (a USBcan Light 4xHS) speaks it. Requests and frames share one pair of endpoints there, so its receive loop runs before the adapter is identified and hands answers to whoever asked. A report that comes with a bus error may only worsen a channel's state: that adapter sends zeros with them. An adapter that cannot be listen-only refuses to be started so, rather than start able to acknowledge |
| 2026-11-15 | An acknowledged request (flush, an option, a J1939 or broadcast manager send) waits for its acknowledgement however short the socket's read timeout: the timeout is for receiving. Found when a flush failed behind a frame that was waiting for room in an adapter |
| 2026-11-15 | A raw socket is told its interface's state by the daemon and refuses to send on one that is down, bus-off or gone, without a round trip; before refusing it asks, in case the news of a change was lost. A send looks for such news at most once a millisecond |
| 2026-11-15 | The descriptor a program waits on is a kernel event queue the library keeps in step with what it holds, not the socket, so that it is readable exactly when a receive would return something |
| 2026-11-15 | Interfaces have an index (not kept across daemon restarts) and a name kept per adapter channel in `~/Library/Application Support/darwincan/interfaces`; `any` is not a name an interface can have |
| 2026-11-15 | An acknowledged request can be refused: the daemon answers `Refused` in the place of `Ok`, and the client matches the answer to its request by order |
