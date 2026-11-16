# Using darwinCAN from Python

darwinCAN has a [python-can](https://python-can.readthedocs.io) interface: a program written for python-can runs on it by naming the interface `darwincan` where on Linux it names `socketcan`. The package also has ISO-TP channels whose timing is kept by the daemon.

The package is in [`python/`](../python). It is plain Python over the C library ([c-api.md](c-api.md)), which it loads at run time. What holds in every language (timestamps, what a send means, what happens when an adapter is unplugged) is in [application-guide.md](application-guide.md).

## Install

Build and install the daemon and the C library first (see [c-api.md](c-api.md#install)), then the package, into the environment your program uses:

```sh
sudo make install            # or: make install PREFIX=$HOME/.local
pip install ./python
```

It needs Python 3.9 or later and python-can 4.5 or later, which `pip` installs with it.

The package looks for `libdarwincan.dylib` in `/usr/local/lib`, `/opt/homebrew/lib` and `~/.local/lib`. If yours is elsewhere, or to use one from the build directory without installing, name it:

```sh
export DARWINCAN_LIBRARY=$PWD/target/release/libdarwincan.dylib
```

The daemon has to be running (`darwincand`, or `darwincand install` once to start it at login), and library and daemon have to come from the same build: they refuse each other otherwise, and say so.

## A first program

```python
import can

with can.Bus(interface="darwincan", channel="vcan0") as bus:
    bus.send(can.Message(arbitration_id=0x123, data=[1, 2, 3], is_extended_id=False))
    for message in bus:
        print(message)
```

`vcan0` is the virtual interface every daemon has; `candump vcan0` and `cansend vcan0 123#0102` in other terminals are on the same bus.

python-can's own tools work too, for example its logger:

```sh
python -m can.logger -i darwincan -c can0
```

and so does its configuration, so that a program which calls `can.Bus()` with no arguments needs no change at all:

```sh
export CAN_INTERFACE=darwincan CAN_CHANNEL=can0
```

## Coming from `socketcan`

| On Linux | Here |
|---|---|
| `can.Bus(interface="socketcan", channel="can0")` | `can.Bus(interface="darwincan", channel="can0")` |
| `ip link set can0 up type can bitrate 500000` | `canctl link set can0 up type can bitrate 500000`, with the same words , or `bitrate=500000` to `can.Bus` |
| `channel=""` for every interface | `channel="any"` |
| `receive_own_messages`, `local_loopback`, `fd`, `can_filters`, `ignore_rx_error_frames` | The same, with the same defaults |
| `bus.send_periodic(...)` through the kernel's broadcast manager | Through the daemon's; see below |
| `isotp.socket` of the `can-isotp` package | `darwincan.IsoTpSocket`; see below |

A bus given a `bitrate` brings its interface up if it is down, as python-can's interfaces for other adapters do:

```python
bus = can.Bus(interface="darwincan", channel="can0", bitrate=500000)
bus = can.Bus(interface="darwincan", channel="can1", fd=True,
              bitrate=500000, data_bitrate=2000000)
bus = can.Bus(interface="darwincan", channel="can2", bitrate=250000,
              listen_only=True)      # never transmits or acknowledges
```

An interface that is already up is shared with whatever other programs have it open, so a bus never reconfigures it. Its bitrate, data bitrate and listen-only mode then have to equal those asked for, or the open fails with `can.CanInitializationError` naming both, rather than let the program run at a bitrate it did not ask for. Take it down with `canctl link set can0 down` to change them.

Bringing an interface up for CAN FD (`fd=True`) needs a `data_bitrate` as well, and `listen_only` needs a `bitrate` when the interface is down.

Closing the bus leaves the interface up, since another program may have begun to use it. Without a `bitrate` a bus leaves its interface as it finds it, as `socketcan` does: it opens on one that is down, and sending fails until `canctl`, or another bus, brings it up.

Also as on Linux, a bus opened without `fd=True` is not given the CAN FD frames on the bus and cannot send one.

What differs:

- **Timestamps** are the daemon's, taken from the adapter's own clock where it has one: seconds since the Unix epoch, on a clock that never steps. Differences between them are exact however late Python reads the messages. A float holds them to about a quarter of a microsecond.
- **`send` never waits**, so its `timeout` is not used. It fails at once with `can.CanOperationError` on an interface that is down, bus-off or unplugged.
- **A frame the adapter could not put on the bus** after the daemon took it (nothing on the bus acknowledges and the adapter is full) cannot be reported by the `send` that has long returned. The next `recv` logs it as a warning on the `can.darwincan` logger and counts it in `bus.transmit_failures`; it does not raise, so that a `can.Notifier` survives it.
- **`bus.state`** reads `ACTIVE` on the bus, `PASSIVE` for a listen-only interface and `ERROR` for one that is down or gone. It cannot be set; use `canctl`.
- **`bus.flush_tx_buffer()`** waits until the daemon has handled every frame sent on the bus, which is what a test needs before it looks at what another bus received.
- **Closing is safe from another thread.** `bus.shutdown()` ends a `recv` that is waiting elsewhere with `can.CanOperationError`.

## Waiting

`recv(timeout)` waits in the library. Ctrl-C ends it.

`bus.fileno()` is a descriptor for `select` and for an event loop, readable exactly when `recv(timeout=0)` would return a message. It is not the socket: never read from it.

```python
import select

readable, _, _ = select.select([bus_a, bus_b], [], [], 1.0)
for bus in readable:
    message = bus.recv(timeout=0)
```

One thread may receive on a bus while others send. For more than one receiving thread, wrap it in `can.ThreadSafeBus` as with any interface.

## Sending cyclically

```python
task = bus.send_periodic(
    can.Message(arbitration_id=0x100, data=[0, 0], is_extended_id=False), 0.010
)
...
task.modify_data(can.Message(arbitration_id=0x100, data=[0, 1], is_extended_id=False))
...
task.stop()
```

The frames are sent by the daemon's broadcast manager on a real-time thread, so they leave on time whether or not Python is running: a busy interpreter, the garbage collector or another thread holding the interpreter lock do not move them. `modify_data` changes what is sent without disturbing the rhythm, `stop` ends it, `start` begins again, and a `duration` ends it after so long. With a list of messages, one is sent each period, in turn.

Two things send from a Python thread instead, with a thread's timing: a task with a `modifier_callback`, since the daemon cannot call Python for each frame, and any task on a bus opened on `"any"`.

One task per identifier on a bus: a second `send_periodic` for an identifier that is being sent raises `ValueError`, as on Linux. Change the running one with `modify_data`.

## ISO-TP

```python
from darwincan import IsoTpError, IsoTpSocket

with IsoTpSocket("can0", tx_id=0x7E0, rx_id=0x7E8) as tester:
    tester.send(bytes([0x22, 0xF1, 0x90]))       # read data by identifier
    try:
        answer = tester.recv(timeout=1.0)        # bytes, or None
    except IsoTpError as error:
        print("the answer was cut short:", error)
```

Segmentation, flow control and the gaps between frames (STmin) are kept by the daemon, which is what the standard's timing needs and what a Python loop over a bus cannot promise. `send` returns when the peer has the whole message and raises `IsoTpError` if the transfer fails; its `failure` is one of `IsoTpSocket.FAILURE_TIMEOUT`, `FAILURE_OVERFLOW` and so on. `IsoTpError` is a `can.CanOperationError`.

| Option | Meaning |
|---|---|
| `extended_id=True` | Both identifiers are 29-bit |
| `fd=True`, `bitrate_switch=True`, `tx_dl=64` | CAN FD frames, at the data bitrate, of this length |
| `padding=0xCC` | Fill frames up with this byte |
| `block_size`, `st_min` | What this end asks of the peer when it receives. `st_min` is the byte on the wire: 0 to 0x7F are milliseconds, 0xF1 to 0xF9 are 100 to 900 microseconds |
| `n_bs`, `n_cr` | Seconds to wait for the peer's flow control and for its next frame (1 s each) |
| `max_rx_len` | The longest message accepted (1 MiB) |

`recv_timestamped` also gives the time of the message's last frame, and `fileno()` a descriptor to wait on, as for a bus. One thread may receive while others send; a channel carries one message at a time in each direction.

## Not there

- **J1939.** The daemon and the C library have it ([c-api.md](c-api.md#j1939)); the Python package does not yet.
- **The broadcast manager's receive jobs** (reports when a frame changes or stays away). Use the C library, or a `can.Notifier`.
- **A wheel that carries the C library.** The library is installed separately, with `make install`.

## When something does not work

| What you see | What it means |
|---|---|
| `CanInterfaceNotImplementedError: the darwinCAN C library could not be loaded` | `make install` has not been run, or the library is somewhere the package does not look; the message lists where it looked. Set `DARWINCAN_LIBRARY` |
| `Unknown interface type "darwincan"` | The package is not installed in the Python that is running: `pip install ./python` with that Python's `pip` |
| `CanInitializationError: could not open can0: cannot reach darwincand ...` | `darwincand` is not running, or `DARWINCAN_SOCKET` points somewhere else than the daemon's socket |
| `... no such interface: can0` | `canctl link show` lists the names there are; an adapter that is not plugged in has no interface |
| `... version ...` when opening | Library and daemon are from different builds; install both from the same one and restart the daemon |
| `CanOperationError: can0 is down` on `send` | Give `can.Bus` a `bitrate`, or `canctl link set can0 up type can bitrate ...` |
| Nothing is received | `canctl link show`: is the interface up, at the bus's bitrate? Is anything else on the bus to acknowledge? `candump can0` shows what the daemon sees |

## Tests

`python/tests` runs against a real daemon. `cargo test` starts one and runs them when `.venv/bin/python` (or the interpreter named in `DARWINCAN_PYTHON`) has python-can and the package:

```sh
python3 -m venv .venv
.venv/bin/pip install -e python
cargo test -p darwincand --test python_api
```

Without such an interpreter that test says it was skipped and passes.
