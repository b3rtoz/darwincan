# Building applications on darwinCAN

For developers who want to interface with CAN on a Mac:

* A test bench
* A diagnostic tool
* A logger
* A port of something originally written for SocketCAN

This guide covers what is the same in every language as well as the Rust library in detail. C and Python each have a guide of their own.

| Language | Start here            | What you get                                                                                                                                    |
| -------- | --------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| Rust     | This guide            | The `darwincan` crate: `RawSocket`, `IsoTpSocket`, `BcmSocket`, `J1939Socket`, `Control`                                            |
| C, C++   | [c-api.md](c-api.md)   | `libdarwincan` and `darwincan.h`, with the structs and constants of `<linux/can.h>`; a call-by-call table for porting a SocketCAN program |
| Python   | [python.md](python.md) | A [python-can](https://python-can.readthedocs.io) interface (`can.Bus(interface="darwincan")`) and ISO-TP channels                             |

All three are the same library underneath and behave the same way.

## How a program reaches the bus

Your program does not open the adapter. The daemon, `darwincand`, owns the adapters and your program connects to it; so can any number of other programs at the same time, each with its own filters, on the same interface. That is the point of darwinCAN: a logger beside your application, or `candump` beside your test, all on one adapter.

What this means for an application:

- **The daemon must be running.** Opening a socket fails with "cannot reach darwincand" otherwise. Users normally install it once as a launch agent ([tools.md](tools.md#the-daemon)).
- **Interfaces have names**: `can0` for an adapter channel, `vcan0` for the virtual interface every daemon has. `canctl list` shows them.
- **A hardware interface has to be put on the bus** with a bitrate before frames flow, by the user with `canctl` or by your program.
- **Library and daemon must come from the same build.** They check each other's protocol version and report a mismatch in words.
- **`DARWINCAN_SOCKET`** names the daemon's socket if it is not in the default place; every open call also has a form that takes the path.

There is no API stability promise yet: the project is at version 0.1 and interfaces may still change between releases.

## What every application should know

These hold in Rust, C and Python alike.

### Frames

Identifiers, flags and filters use the SocketCAN encoding: bit 31 of `can_id` marks a 29-bit identifier, bit 30 a remote frame, bit 29 an error frame; a filter passes a frame when `can_id & mask` equals the filter's `id & mask`. Constants and matching logic from Linux code carry over unchanged.

### Receiving

- A new socket receives every data frame on its interface. Set filters to receive less; the daemon does the filtering.
- **Every frame has a timestamp** taken once in the daemon (from the adapter's own clock on hardware): nanoseconds since the Unix epoch on a clock that never steps. The difference between two timestamps is exact however late your program reads the frames. Use them, not the time of reading, for anything about timing.
- **A slow reader loses frames.** The daemon drops frames for a socket whose queue is full rather than delay the bus or another program. Each received frame carries the count of frames lost so far.
- **Error frames are off by default.** Ask for them to hear about bus-off, error-passive, receive overruns and a restart after sleep.
- **Your own frames come back only if you ask**, and on hardware only once they were really on the bus and acknowledged, with the adapter's timestamp. That makes them the right thing to measure send timing with.

### Sending

- **A send returns as soon as the daemon has the frame**, like `write` on a Linux socket. It does not wait for the bus.
- It fails at once if the interface is down, bus-off or unplugged.
- A frame the daemon took but the adapter could not put on the bus (nothing acknowledges, and the adapter is full) is reported later, in the receive stream, as a transmit failure.
- **Flush** waits until the daemon has handled everything sent so far. After it, another socket's receive queue holds the frames you sent. This is what makes tests deterministic without sleeps.

### Things that happen to an interface

| Event                            | What your program sees                                                                                                               |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| The adapter is unplugged         | Receives and sends fail with "the interface was removed". Open the interface again once it is back; it comes back down               |
| The Mac sleeps and wakes         | The daemon restores the interface; with error frames on, a "restarted" error frame arrives. Frames in flight at that moment are lost |
| The bus goes bus-off             | A bus-off error frame, then sends are refused until the interface is taken down and up                                               |
| Someone takes the interface down | Sends fail with "is down"                                                                                                            |

### What the daemon times for you

Protocol timing runs in the daemon on real-time threads, so it does not depend on when your process is scheduled:

- **ISO-TP**: segmentation, flow control, and the gap between consecutive frames (STmin), which is never undercut.
- **Cyclic messages**: a broadcast manager job sends a frame every interval for as long as it exists.
- **J1939 transport**: BAM, RTS/CTS and the extended protocol.

If your application needs a frame out at a fixed rate, give it to the broadcast manager rather than sending from a timer of your own.

## Rust

The crate is not on crates.io yet. Depend on it from the repository, at the same revision as the daemon you run:

```toml
[dependencies]
darwincan = { git = "https://github.com/b3rtoz/darwincan" }
```

It has no dependencies of its own and builds with Rust 1.75 or later. Every snippet below is taken from a runnable program in [`crates/darwincan/examples`](../crates/darwincan/examples):

| Example        | Shows                                                                                     |
| -------------- | ----------------------------------------------------------------------------------------- |
| `dump`       | A receive loop with a filter, error frames, timestamps and the count of lost frames       |
| `uds_read`   | A diagnostic request and response over ISO-TP                                             |
| `cyclic`     | A frame sent every few milliseconds by the broadcast manager, its data changed as it goes |
| `j1939`      | J1939 messages to one address and to everyone                                             |
| `interfaces` | Listing interfaces and putting one on the bus                                             |

```sh
darwincand &
cargo run --example dump -- vcan0 &
cansend vcan0 123#DEADBEEF
```

### Raw frames

```rust
use darwincan::{err, Filter, Frame, Id, RawSocket};

let can = RawSocket::open("vcan0")?;
can.set_filters(&[Filter::new(0x100, 0x700)])?;   // identifiers 100 to 1FF
can.set_error_filter(err::MASK)?;                 // and error frames

can.send(&Frame::new(Id::Standard(0x123), &[0xDE, 0xAD])?)?;
can.send(&Frame::new_fd(Id::Extended(0x18DAF110), &[0; 12], true)?)?;   // CAN FD, bit rate switching

let rx = can.recv()?;
println!("{} at {} ns", rx.frame, rx.timestamp_ns);
if rx.dropped > 0 {
    eprintln!("{} frames lost so far", rx.dropped);
}
```

`RawSocket` is what a `CAN_RAW` socket is on Linux.

| Linux                      | `RawSocket`                                                                          |
| -------------------------- | -------------------------------------------------------------------------------------- |
| `socket`, `bind`       | `RawSocket::open("can0")`, or `open(darwincan::ANY_INTERFACE)` for every interface |
| `write`                  | `send`; `send_to(ifindex, ...)` on a socket open on every interface                |
| `read`                   | `recv`, `try_recv`; `set_read_timeout` bounds the wait                           |
| `CAN_RAW_FILTER`         | `set_filters`                                                                        |
| `CAN_RAW_ERR_FILTER`     | `set_error_filter`                                                                   |
| `CAN_RAW_LOOPBACK`       | `set_loopback`                                                                       |
| `CAN_RAW_RECV_OWN_MSGS`  | `set_recv_own_msgs`; `RxFrame::own` marks them                                     |
| `CAN_RAW_JOIN_FILTERS`   | `set_join_filters`                                                                   |
| `SO_TIMESTAMP`           | Always there: `RxFrame::timestamp_ns`                                                |
| `SO_RXQ_OVFL`            | Always there: `RxFrame::dropped`, `RawSocket::dropped()`                           |
| `poll` on the descriptor | `AsRawFd`: readable exactly when `try_recv` would return something                 |

One thread may receive on a socket while others send on it.

`can.flush()` waits until the daemon has handled every frame sent on the socket. In a test, flush the sender before asserting what a receiver got:

```rust
sender.send(&frame)?;
sender.flush()?;
assert_eq!(receiver.try_recv()?.map(|rx| rx.frame), Some(frame));
```

### ISO-TP

```rust
use std::time::Duration;
use darwincan::{IsoTpParams, IsoTpSocket};

let ecu = IsoTpSocket::open("can0", IsoTpParams::new(0x7E0, 0x7E8))?;   // we send 7E0, hear 7E8
ecu.set_read_timeout(Some(Duration::from_secs(2)))?;

ecu.send_blocking(&[0x22, 0xF1, 0x90])?;   // returns once the ECU has all of it
let response = ecu.recv()?;
```

`IsoTpParams` holds what a `CAN_ISOTP` socket's options do: CAN FD and bit rate switching, frame length, padding, the block size and STmin this end asks of the peer, the timeouts, and the longest message accepted. For a 29-bit identifier, set `CAN_EFF_FLAG` in `tx_id` and `rx_id`.

`send_blocking` waits until the transfer is complete and fails with `Error::IsoTp` and its reason (`IsoTpFailure::Timeout` when the peer does not answer, and so on). `send` and `recv_event` are the non-blocking form: `IsoTpEvent::TxDone`, `TxFailed`, `RxFailed` and `Data` arrive as they happen. A channel carries one message at a time in each direction.

Addressing is normal addressing only; extended and mixed addressing are not there yet.

### Cyclic messages and watching for changes

```rust
use darwincan::{BcmMessage, BcmSocket, Frame, Id};

let bcm = BcmSocket::open("can0")?;
let id = Id::Standard(0x123);

bcm.send(&BcmMessage::tx_cyclic(Frame::new(id, &[0, 0])?, 10_000))?;   // every 10 ms
// ...
bcm.send(&BcmMessage::tx_update(Frame::new(id, &[0, 1])?))?;           // new data, same rhythm
// ...
bcm.send(&BcmMessage::tx_delete(id.to_can_id(), false))?;

bcm.send(&BcmMessage::rx_watch(0x321, 500_000))?;   // tell me of each 321, and if none comes for 0.5 s
let report = bcm.recv()?;                           // opcode RX_CHANGED or RX_TIMEOUT
```

`BcmSocket` is what a `CAN_BCM` socket is on Linux, operation by operation and flag by flag: `BcmMessage` has the fields of `struct bcm_msg_head`, and `darwincan::bcm::{op, flag}` the constants. The helpers above build the common messages; fill in a `BcmMessage` yourself for the rest (a burst of `count` frames at one interval and then another, a sequence of frames sent in turn, masks that pick out the bits whose change is reported, a multiplexed message, an answer to a remote frame). The jobs end when the socket is dropped.

### J1939

```rust
use darwincan::{J1939Addr, J1939Socket};

let engine = J1939Socket::open("can0", J1939Addr::address(0x80))?;

// To one address. More than eight bytes go by the transport protocol.
engine.send_to(&[0x5A; 100], &J1939Addr::new(0x90, 0x0EF00))?;

// To everyone, which has to be allowed first.
engine.set_broadcast(true)?;
engine.send_to(&[0xFF; 8], &J1939Addr::broadcast(0x0FEF1))?;

let message = engine.recv()?;   // message.src, message.data, message.priority
```

`J1939Socket` is what a `CAN_J1939` socket is on Linux: bound to an address or to a NAME, optionally connected to a peer, with filters, promiscuous mode and priorities. Messages can be of any length: one frame up to 8 bytes, a broadcast (BAM) up to 1785 bytes, RTS/CTS to one destination, and the extended transport protocol beyond that.

- **NAMEs.** The daemon follows the address claims on the bus, so a socket bound or sending to a NAME uses the address that NAME holds, 250 ms after its claim. Claiming and defending an address is the program's job, as on Linux.
- **Notices** (`set_notices`) report how a long message fared: sent, given up and why, a long message arriving. They stand where Linux has the socket's error queue.
- A send is refused at once if the interface is down, a NAME has no address, a broadcast was not allowed, or the message is too long.

J1939 has so far been tested only against itself, not against another implementation.

### Interfaces

```rust
use darwincan::{BusConfig, Control};

let control = Control::connect()?;
control.up("can0", &BusConfig::new(500_000))?;                 // classic CAN
control.up("can1", &BusConfig::fd(500_000, 2_000_000))?;       // CAN FD
for interface in control.interfaces()? {
    println!("{} {} up={}", interface.name, interface.kind, interface.up);
}
control.down("can0")?;
```

`Control` does what `canctl` does: list, configure, bring up and down, rename, and add and remove virtual interfaces. `BusConfig` also has sample points and `listen_only`.

An interface is shared. Before reconfiguring one that is up, consider that another program may be using it at its current bitrate.

### Errors

Every call returns `darwincan::Result`. The cases worth telling apart:

| `Error`                    | Meaning                                                                                                                                                                                                    |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `DaemonUnavailable`        | The daemon is not running, or not at this socket path                                                                                                                                                      |
| `VersionMismatch`          | Library and daemon are from different builds                                                                                                                                                               |
| `Daemon { code, message }` | The daemon refused something. `message` is for the user; `code` (`ErrorCode::InterfaceDown`, `NoSuchInterface`, `InterfaceRemoved`, `TransmitFailed`, `Unsupported`, ...) is for the program |
| `IsoTp { tx, reason }`     | An ISO-TP transfer failed                                                                                                                                                                                  |
| `Io`                       | The connection failed, or a read timed out (`Error::is_timeout()`)                                                                                                                                       |

## Developing and testing without hardware

`vcan0` behaves like a bus with every program on it as a node, so most of an application can be written and tested with no adapter attached:

```sh
darwincand &
candump vcan0 &                 # watch what your program sends
cangen vcan0 -g 10              # give it traffic to receive
```

For automated tests, start a daemon of your own on a throwaway socket so that tests do not depend on, or disturb, an installed one:

```sh
darwincand --socket /tmp/myapp-test.sock --no-hardware &
DARWINCAN_SOCKET=/tmp/myapp-test.sock cargo test
```

Keep the socket path short: macOS limits Unix socket paths to about 100 bytes. On the virtual bus a frame is delivered as it is sent, sender and receiver see one timestamp, and nothing is ever unacknowledged; test what depends on a real bus (bitrates, error states, transmit confirmation, adapter timing) on hardware.

## Shipping an application

- **Your users need the daemon.** They build and install it with `make install` and `darwincand install` (see [tools.md](tools.md#the-daemon)); there is no packaged installer yet.
- **Ship or pin matching versions.** The library your program was built with and the daemon it meets must be the same build.
- **Licensing.** The library you link (Rust crate, C library, Python package) is MIT OR Apache-2.0, so your application may use any licence. The daemon is GPL-2.0-only; your program talks to it over a socket and does not link it. If you distribute the daemon itself, the GPL applies to that.
- **Supported adapters** and their limits are in [hardware.md](hardware.md). Check there before promising a feature on a particular adapter: not all can be listen-only or do CAN FD.
