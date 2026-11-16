# Developing darwinCAN

This page is for people working on darwinCAN itself. It covers building it, testing it, finding your way around the source, and the rules that keep it correct. To build an application *on* darwinCAN, read [application-guide.md](application-guide.md) instead.

[CONTRIBUTING.md](../CONTRIBUTING.md) says how to send a change, and [design.md](design.md) explains why things are built the way they are.

## Building

You need macOS, the Xcode command line tools, and Rust 1.85 or later (`brew install rustup`; Homebrew's rustup puts its tools in `/opt/homebrew/opt/rustup/bin`, which you may need to add to your `PATH`). The client library and the tools keep to Rust 1.75; only the daemon needs 1.85.

```sh
cargo build --release
cargo test                      # every crate, including end-to-end daemon tests
cargo clippy --all-targets      # must be free of warnings
cargo fmt
make install PREFIX=/some/scratch/dir   # daemon, tools, C library, headers, pkg-config
```

To try a build by hand, run the daemon on a throwaway socket so that it does not touch one you have installed. macOS limits Unix socket paths to about 100 bytes, so keep the path short:

```sh
export DARWINCAN_SOCKET=/tmp/dc.sock
target/release/darwincand --no-hardware &
target/release/candump vcan0
```

The Python package's tests need a Python with python-can:

```sh
python3 -m venv .venv
.venv/bin/pip install -e python
cargo test -p darwincand --test python_api
```

`cargo test` runs them when `.venv/bin/python` (or the interpreter named in `DARWINCAN_PYTHON`) can import `can` and `darwincan`. Otherwise it reports that test as skipped.

## Where things are

| Path | Contents |
|---|---|
| `crates/darwincan-core` | `Frame`, `Filter`, `BusConfig`, `IsoTpParams`, the J1939 and broadcast manager types, and the wire protocol (`wire.rs`) |
| `crates/darwincan-isotp` | The ISO 15765-2 state machine, with no I/O and no clock |
| `crates/darwincan-j1939` | The SAE J1939 stack for one interface, with no I/O and no clock |
| `crates/darwincan` | The client library: `RawSocket`, `IsoTpSocket`, `BcmSocket`, `J1939Socket`, `Control`. `ffi.rs` is the C API, which is declared in `include/*.h`. `c/` holds the C examples and the self-test, and `examples/` holds the Rust examples |
| `crates/darwincan-utils` | `candump`, `cansend`, `cangen`, `canplayer`, `cansniffer`, `isotpsend`, `isotprecv`, `isotpdump`, `canctl`, and the J1939 tools `j1939acd`, `j1939spy`, `j1939cat`, `j1939sr`, `testj1939`. The address claiming of `j1939acd` (`acd.rs`) does no I/O and reads no clock |
| `crates/darwincand` | The daemon: `bus.rs` (an interface and its fan-out), `session.rs` (one client connection), `bcm.rs` (broadcast manager jobs), `j1939.rs` (one J1939 stack per interface), `backends/` (hardware drivers, `timesync.rs`), `bittiming.rs`, `names.rs`, `launchd.rs`, `priority.rs` |
| `python/` | The Python package: `_lib.py` (the C declarations, written with ctypes), `bus.py` (the python-can interface and cyclic tasks), `isotp.py`, `j1939.py`, `tests/` |
| `docs/` | The guides. `docs/notes/` holds the dated hardware log and the decision log |

The tests are in these places:

| Where | What |
|---|---|
| Unit tests beside the code | Protocol engines, encodings, bit timing, clock mapping |
| `crates/darwincand/tests/daemon.rs`, `j1939.rs` | End-to-end tests: a real daemon on a temporary socket, driven through the client library |
| `crates/darwincand/tests/c_api.rs` | Compiles the C programs in `crates/darwincan/c` against the header and the library, and runs them against a daemon |
| `crates/darwincand/tests/python_api.rs` | Runs `python/tests` against a daemon |

## Rules that are easy to break

**Licence boundary.** Hardware backends are derived from GPL-2.0-only Linux kernel drivers, so they live only in `darwincand`. Never move or copy backend code into the permissively licensed crates, and never make those crates depend on `darwincand` (a dev-dependency in the other direction is fine). A file derived from kernel sources carries `SPDX-License-Identifier: GPL-2.0-only` and names the original file and its copyright holders. Code for the permissive crates must not be taken from GPL sources: the J1939 stack is written from the standards, never from Linux's `net/can/j1939`; the tools are written from can-utils' behaviour, never from its source; and the Python bus follows the behaviour of python-can's `socketcan` interface without copying it.

**Dependencies.** The permissive crates have no third-party dependencies. Keep it that way unless there is a strong reason. The daemon uses `nusb` for USB.

**SocketCAN compatibility.** `can_id` flag bits, CAN FD flags, filter matching and error frame classes follow `<linux/can.h>` and `<linux/can/error.h>` exactly. Check the Linux definition before adding or changing one.

**can-utils compatibility.** The tools take the same command lines as can-utils (and `canctl link ...` the same as `ip link ...`) and print the same output. An option that cannot be supported is refused with a message, never silently ignored.

**Timing.** Frames are timestamped once, in the daemon. Protocol timing runs in the daemon, not in clients. Nothing on the frame path may block: a sink drops frames for a slow client rather than waiting. After touching `bus.rs` or `session.rs`, measure again with `canctl latency vcan0` and an ISO-TP transfer with STmin, and compare the results with the figures from before your change. Tools that pace frames wait with `darwincan_utils::Pacer`, not with a bare sleep, which can wake milliseconds late.

**Wire protocol.** Sends are fire-and-forget; option changes and `Sync` are acknowledged in the frame stream. A new request must say which of the two kinds it is, fire-and-forget or acknowledged, and it gets a round-trip case in the tests of `wire.rs`. Raise `PROTOCOL_VERSION` with every change to a message.

**C API.** `ffi.rs`, the headers in `crates/darwincan/include`, `c/selftest.c` and [c-api.md](c-api.md) change together. Structs and constants taken from the Linux headers keep their Linux layout and names.

**Python.** `python/darwincan/_lib.py` repeats structs and constants of the C headers. Change them together, and change [python.md](python.md) with them.

**Deterministic tests.** Frames sent on one connection race requests on another. Call `flush()` on the sender before asserting on what another socket received; do not add sleeps. A timing assertion gets a strict lower bound and a loose upper bound, because tests run in parallel on a machine doing other things.

**Say what was tested.** A backend is "untested on hardware" until it has run against the real adapter, and the documentation says so. Do not describe something as working on the strength of unit tests alone.

## Adding a hardware backend

1. Put it in `crates/darwincand/src/backends/<name>/`. Keep the protocol's encoding and decoding as pure functions in `proto.rs`, with unit tests built from the reference driver's structures.
2. Implement `darwincand::Backend` for a channel: `start`, `stop`, `transmit`, and `confirms_transmission` if the adapter reports each frame it sent. Deliver received frames and those reports through the `InterfaceHandle`, converting the adapter's timestamps with `backends::timesync::TimeSync`.
3. Give each channel an identity with `backends::identity`, so that it keeps its interface name.
4. Register the USB ids in `backends/mod.rs` (`try_attach`).
5. Refuse what the adapter cannot do (listen-only, CAN FD, a bitrate) with a message.
6. Test it on the adapter (see below), record the run, and add the adapter to [hardware.md](hardware.md) with its real status.

`backends/peak_fd` is the model to follow. `backends/kvaser_hydra` is the one to follow for an adapter that answers requests on the same endpoint its frames arrive on.

The following points were learnt the hard way:

- macOS leaves a device whose USB *device* class is vendor-specific without a configuration. Select one before claiming the interface.
- Take the adapter off the bus when attaching it, because a daemon that was killed leaves it running.
- Find out where the adapter takes its timestamp (it should be the end of the frame) and whether the timestamp can read early. STmin is timed from it.
- Flush the adapter's transmit queue when an interface goes down; otherwise stale frames go out when it next comes up.

## Testing on hardware

Hardware tests are run by hand and recorded. Add an entry to [notes/hardware-validation-log.md](notes/hardware-validation-log.md) for every run (what was wired, what was run, what was measured and what went wrong), and bring [hardware.md](hardware.md) up to date.

Before putting anything on a bus:

- **Know what the adapter is wired to.** A test bus of your own is one thing; a vehicle or a machine is another.
- **Start listen-only** on a bus that is not yours alone. An adapter without a listen-only mode acknowledges frames as soon as it is up.
- **Terminate the bus**, so that there are 60 Ω between CAN-H and CAN-L. Without termination you will measure errors, not the software.

A minimal bench is two adapters, or two channels of one, cabled together and terminated.

```sh
export DARWINCAN_SOCKET=/tmp/dc-hw.sock
target/release/darwincand --timing-log 3 > /tmp/dc-hw.log 2>&1 &
target/release/canctl -details link show

# A counted stream, checked for loss and order on the other side
candump -L -e -n 5000 -T 9000 can1 > rx.log &
cangen can0 -g 0 -n 5000 -I 100 -L 8 -D i

# ISO-TP, 4095 bytes, the receiver asking for an STmin of 5 ms
isotprecv -s 7E8 -d 7E0 -m 05 can1 > rx.txt &
isotpsend -s 7E0 -d 7E8 can0 < payload.txt
```

The checks a backend should be put through are listed under ["What tested covers"](hardware.md#what-tested-covers) in hardware.md.

Each of the following pitfalls has cost time:

- **The Mac falls asleep.** It goes to sleep after a few idle minutes, and a test that is waiting stops counting while it sleeps. Run `caffeinate -i` during a long test. Sleep is a test case of its own, but it should not happen by accident.
- **Making the Mac sleep on demand is hard.** With "Wake for network access" turned on, a network packet wakes the Mac within a second; after any wake, macOS will not sleep again for 45 seconds; and `caffeinate -s` turns a sleep that is asked for into a dimmed display with everything still running. Check `pmset -g log` to see whether the Mac really slept.
- **A wrong bitrate or a missing node does not cause bus-off** on a two-node bus; the sender goes no further than error-passive. Bus-off needs a real fault: CAN-H bridged to CAN-L for a second or two while both ends send.
- **A sender that never wins arbitration looks like a dead bus.** One end flooding a lower identifier without pause starves the other, whose sends are then refused as "transmit buffer is full". Space the frames when testing both directions at once.
- **Settings persist across down and up**, as on Linux. After `listen-only on`, you must give `listen-only off` before the interface transmits again.
- **An error state is reported once, when it changes.** A `candump -e` that is started afterwards shows nothing.
- **A listen-only receiver sees the sender's retries** as an endless stream of frames, so give `candump` a frame count (`-n`).
- **`candump` on several interfaces ends when one of them is removed.**
- **macOS has no `timeout` command.** Use `perl -e 'alarm 40; exec @ARGV' <command>` to bound a tool that waits.

`darwincand --timing-log <ms>` logs whatever held a frame up for longer than the given number of milliseconds, and `DARWINCAN_PACER_LOG=<ms>` does the same for `cangen` and `canplayer`. Use them to tell a late frame from a late measurement.

## Documentation

| File | Keep it current when |
|---|---|
| `README.md` | What the project does or supports changes |
| `docs/hardware.md` | An adapter is added or tested |
| `docs/notes/hardware-validation-log.md` | Any hardware test is run |
| `docs/application-guide.md`, `docs/c-api.md`, `docs/python.md` | A programming interface changes |
| `docs/tools.md` | A tool or a daemon option changes |
| `docs/design.md`, `docs/notes/decisions.md` | A design decision is made or revisited |
| `docs/roadmap.md` | Something planned is done, or something new is planned |
