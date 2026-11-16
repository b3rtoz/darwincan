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

Your program does not open the adapter itself. The daemon, `darwincand`, owns the adapters, and your program connects to the daemon. Any number of other programs can be connected at the same time and use the same interface, each with its own filters. This sharing is the point of darwinCAN: a logger can run beside your application, or `candump` beside your test, all on one adapter.

For an application, this has the following consequences:

- **The daemon must be running.** If it is not, opening a socket fails with "cannot reach darwincand". Users normally install the daemon once as a launch agent (see [tools.md](tools.md#the-daemon)).
- **Interfaces have names.** An adapter channel has a name such as `can0`, and `vcan0` is the virtual interface that every daemon has. `canctl list` shows the names.
- **A hardware interface has to be put on the bus** with a bitrate before frames flow. Either the user does this with `canctl`, or your program does it.
- **The library and the daemon must come from the same build.** They check each other's protocol version and report a mismatch in a plain message.
- **`DARWINCAN_SOCKET`** names the daemon's socket if it is not in the default place. Every call that opens a socket also has a form that takes the path.

There is no API stability promise yet: the project is at version 0.1 and interfaces may still change between releases.

## What every application should know

The points in this section hold in Rust, C and Python alike.

### Frames

Identifiers, flags and filters use the SocketCAN encoding. Bit 31 of `can_id` marks a 29-bit identifier, bit 30 marks a remote frame and bit 29 marks an error frame. A filter passes a frame when `can_id & mask` equals the filter's `id & mask`. Constants and matching logic from Linux code carry over unchanged.

### Receiving

- A new socket receives every data frame on its interface. Set filters to receive fewer; the daemon does the filtering.
- **Every frame has a timestamp.** It is taken once, in the daemon, and on hardware it comes from the adapter's own clock. It counts nanoseconds since the Unix epoch on a clock that never steps. The difference between two timestamps is exact, however late your program reads the frames. For anything that concerns timing, use the timestamps and not the time at which your program read the frames.
- **A slow reader loses frames.** The daemon drops frames for a socket whose queue is full rather than delay the bus or another program. Each received frame carries the count of frames lost so far.
- **Error frames are off by default.** Turn them on to be told about bus-off, the error-passive state, receive overruns and a restart after the Mac has slept.
- **Your own frames come back only if you ask for them.** On hardware they come back only once they have really been on the bus and been acknowledged, and they carry the adapter's timestamp. That makes them the right thing to measure send timing with.

### Sending

- **Long messages are limited in bytes, not only in number.** The daemon holds at most 128 MiB of ISO-TP and J1939 messages that one connection has given it to send; a program that sends more waits, as on a Linux socket whose buffer is full. At most 128 MiB of received messages wait for a program that does not read them, and further ones are dropped. One message always fits, whatever its length. See [design.md](design.md) for the limits.
- **A send returns as soon as the daemon has the frame**, like `write` on a Linux socket. It does not wait for the bus.
- A send fails at once if the interface is down, bus-off or unplugged.
- A frame that the daemon accepted but the adapter could not put on the bus (because nothing acknowledges and the adapter is full) is reported later, in the receive stream, as a transmit failure.
- **Flush** waits until the daemon has handled everything sent so far. Once it returns, another socket's receive queue holds the frames you sent. This is what makes tests deterministic without sleeps.

### Things that happen to an interface

| Event                            | What your program sees                                                                                                               |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| The adapter is unplugged         | Receives and sends fail with "the interface was removed". Open the interface again once the adapter is back. The interface is down when it returns.               |
| The Mac sleeps and wakes         | The daemon restores the interface. If error frames are turned on, a "restarted" error frame arrives. Frames in flight at that moment are lost. |
| The bus goes bus-off             | A bus-off error frame arrives. Sends are then refused until the interface is taken down and brought up again.                                               |
| Someone takes the interface down | Sends fail with "is down".                                                                                                            |

### What the daemon times for you

Protocol timing runs in the daemon on real-time threads, so it does not depend on when your process is scheduled:

- **ISO-TP**: segmentation, flow control, and the gap between consecutive frames, which is never shorter than the STmin the receiver asked for.
- **Cyclic messages**: a broadcast manager job sends a frame at every interval for as long as the job exists.
- **J1939 transport**: BAM, RTS/CTS and the extended protocol.

If your application needs a frame out at a fixed rate, give it to the broadcast manager rather than sending from a timer of your own.

## Rust

The crate is not on crates.io yet. Depend on it from the repository, at the same revision as the daemon you run:

```toml
[dependencies]
darwincan = { git = "https://github.com/b3rtoz/darwincan" }
```

The crate has no dependencies of its own and builds with Rust 1.75 or later. Every snippet below is taken from a runnable program in [`crates/darwincan/examples`](../crates/darwincan/examples):

| Example        | Shows                                                                                     |
| -------------- | ----------------------------------------------------------------------------------------- |
| `dump`       | A receive loop with a filter, error frames, timestamps and the count of lost frames       |
| `uds_read`   | A diagnostic request and response over ISO-TP                                             |
| `cyclic`     | A frame sent every few milliseconds by the broadcast manager, with its data changed while it runs |
| `j1939`      | J1939 messages sent to one address and to every node                                             |
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

`RawSocket` is the equivalent of a `CAN_RAW` socket on Linux. The table shows what replaces each Linux call and option.

| Linux                      | `RawSocket`                                                                          |
| -------------------------- | -------------------------------------------------------------------------------------- |
| `socket`, `bind`       | `RawSocket::open("can0")`, or `open(darwincan::ANY_INTERFACE)` for every interface |
| `write`                  | `send`; `send_to(ifindex, ...)` on a socket that is open on every interface                |
| `read`                   | `recv`, `try_recv`; `set_read_timeout` limits the wait                           |
| `CAN_RAW_FILTER`         | `set_filters`                                                                        |
| `CAN_RAW_ERR_FILTER`     | `set_error_filter`                                                                   |
| `CAN_RAW_LOOPBACK`       | `set_loopback`                                                                       |
| `CAN_RAW_RECV_OWN_MSGS`  | `set_recv_own_msgs`; `RxFrame::own` marks them                                     |
| `MSG_DONTROUTE` on a received frame | `RxFrame::local`: a program on this machine sent it |
| `CAN_RAW_JOIN_FILTERS`   | `set_join_filters`                                                                   |
| `SO_TIMESTAMP`           | Always provided, as `RxFrame::timestamp_ns`                                                |
| `SO_RXQ_OVFL`            | Always provided, as `RxFrame::dropped` and `RawSocket::dropped()`                           |
| `poll` on the descriptor | `AsRawFd`: the descriptor is readable exactly when `try_recv` would return something                 |

One thread may receive on a socket while others send on it.

`can.flush()` waits until the daemon has handled every frame sent on the socket. In a test, flush the sender before asserting on what a receiver got:

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

`IsoTpParams` holds the settings that the options of a `CAN_ISOTP` socket hold on Linux: CAN FD and bit rate switching, the frame length, padding, the block size and STmin that this end asks of the peer, the timeouts, and the length of the longest message accepted. For a 29-bit identifier, set `CAN_EFF_FLAG` in `tx_id` and `rx_id`.

`send_blocking` waits until the transfer is complete. If the transfer fails, it returns `Error::IsoTp` with the reason, for example `IsoTpFailure::Timeout` when the peer does not answer. `send` and `recv_event` are the non-blocking form: the events `IsoTpEvent::TxDone`, `TxFailed`, `RxFailed` and `Data` arrive as they happen. A channel carries one message at a time in each direction.

Only normal addressing is supported; extended and mixed addressing are not available yet.

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

`BcmSocket` is the equivalent of a `CAN_BCM` socket on Linux, operation by operation and flag by flag: `BcmMessage` has the fields of `struct bcm_msg_head`, and `darwincan::bcm::{op, flag}` holds the constants. The helpers above build the common messages. For the rest, fill in a `BcmMessage` yourself: a burst of `count` frames at one interval followed by sending at another interval, a sequence of frames sent in turn, masks that pick out the bits whose change is reported, a multiplexed message, or an answer to a remote frame. A socket's jobs end when the socket is dropped.

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

`J1939Socket` is the equivalent of a `CAN_J1939` socket on Linux. It is bound to an address or to a NAME, can be connected to a peer, and has filters, promiscuous mode and priorities. Messages can be of any length: a message of up to 8 bytes goes in one frame, a longer one of up to 1785 bytes goes as a broadcast (BAM) or by RTS/CTS to one destination, and the extended transport protocol carries anything longer than that.

- **NAMEs.** The daemon follows the address claims on the bus, so a socket that is bound to a NAME, or sends to one, uses the address that the NAME holds. An address counts as held from 250 ms after the NAME claimed it. Claiming and defending an address is the program's job, as on Linux, or that of `j1939acd` ([tools.md](tools.md#the-j1939-tools)): with it running for a NAME, a program binds to the NAME alone.
- **Notices** (`set_notices`) report what happened to a long message: that it was sent, that it was given up and why, that a long message is arriving, or that the sender of a message of more than 1785 bytes has begun another block of it. They take the place of the socket's error queue on Linux.
- **Priority.** `set_send_priority` sets the priority of everything the socket sends; `send_with_priority` and `send_to_with_priority` give one message a priority of its own.
- **A packet that goes missing** in a message of up to 1785 bytes to one destination is asked for again, at most twice for one message; after that the message is given up. A broadcast with a packet missing is not delivered, because a broadcast cannot be asked for again, and a message of more than 1785 bytes with a packet missing is given up.
- A send is refused at once if the interface is down, if a NAME has no address, if a broadcast was not allowed, or if the message is too long.

J1939 has been tested against itself and, on one bus, against Linux's J1939 stack. It has not been tested against a real J1939 device.

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

`Control` does what `canctl` does: it lists interfaces, configures them, brings them up and takes them down, renames them, and adds and removes virtual interfaces. `BusConfig` also has the sample points and `listen_only`.

An interface is shared. Before reconfiguring one that is up, consider that another program may be using it at its current bitrate.

### Errors

Every call returns `darwincan::Result`. The table lists the errors that are worth telling apart.

| `Error`                    | Meaning                                                                                                                                                                                                    |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `DaemonUnavailable`        | The daemon is not running, or is not listening at this socket path                                                                                                                                                      |
| `VersionMismatch`          | The library and the daemon are from different builds                                                                                                                                                               |
| `Daemon { code, message }` | The daemon refused something. `message` is for the user; `code` (`ErrorCode::InterfaceDown`, `NoSuchInterface`, `InterfaceRemoved`, `TransmitFailed`, `Unsupported`, ...) is for the program |
| `IsoTp { tx, reason }`     | An ISO-TP transfer failed                                                                                                                                                                                  |
| `Io`                       | The connection failed, or a read timed out (`Error::is_timeout()`)                                                                                                                                       |

## Developing and testing without hardware

`vcan0` behaves like a bus on which every program is a node, so most of an application can be written and tested with no adapter attached:

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

Keep the socket path short: macOS limits Unix socket paths to about 100 bytes. On the virtual bus a frame is delivered as soon as it is sent, the sender and the receiver see the same timestamp, and no frame ever goes unacknowledged. Anything that depends on a real bus (bitrates, error states, transmit confirmation, adapter timing) has to be tested on hardware.

## Shipping an application

- **Your users need the daemon.** They build and install it with `make install` and `darwincand install` (see [tools.md](tools.md#the-daemon)); there is no packaged installer yet.
- **Ship or pin matching versions.** The library your program was built with and the daemon it connects to must come from the same build.
- **Licensing.** The library you link (Rust crate, C library, Python package) is MIT OR Apache-2.0, so your application may use any licence. The daemon is GPL-2.0-only; your program talks to it over a socket and does not link against it. If you distribute the daemon itself, the GPL applies to that.
- **Supported adapters** and their limits are listed in [hardware.md](hardware.md). Check there before promising a feature on a particular adapter: not every adapter has a listen-only mode or supports CAN FD.
