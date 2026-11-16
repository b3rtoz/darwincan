# Using darwinCAN from Python

darwinCAN provides a [python-can](https://python-can.readthedocs.io) interface. A program written for python-can runs on darwinCAN by naming the interface `darwincan` where on Linux it would name `socketcan`. The package also provides ISO-TP channels whose timing is kept by the daemon.

The package is in [`python/`](../python). It is pure Python on top of the C library ([c-api.md](c-api.md)), which it loads at run time. The behaviour that is the same in every language (timestamps, what a send means, what happens when an adapter is unplugged) is described in [application-guide.md](application-guide.md).

## Install

Build and install the daemon and the C library first (see [c-api.md](c-api.md#install)). Then install the package into the Python environment that your program uses:

```sh
sudo make install            # or: make install PREFIX=$HOME/.local
pip install ./python
```

The package needs Python 3.9 or later and python-can 4.5 or later; `pip` installs python-can with it.

The package looks for `libdarwincan.dylib` in `/usr/local/lib`, `/opt/homebrew/lib` and `~/.local/lib`. If your library is elsewhere, or if you want to use the one in the build directory without installing it, name it in `DARWINCAN_LIBRARY`:

```sh
export DARWINCAN_LIBRARY=$PWD/target/release/libdarwincan.dylib
```

The daemon has to be running: start it with `darwincand`, or run `darwincand install` once to have it started at login. The library and the daemon have to come from the same build. If they do not, they refuse to work with each other and say so.

## A first program

```python
import can

with can.Bus(interface="darwincan", channel="vcan0") as bus:
    bus.send(can.Message(arbitration_id=0x123, data=[1, 2, 3], is_extended_id=False))
    for message in bus:
        print(message)
```

`vcan0` is the virtual interface that every daemon has. `candump vcan0` and `cansend vcan0 123#0102`, run in other terminals, are on the same bus.

python-can's own tools work too, for example its logger:

```sh
python -m can.logger -i darwincan -c can0
```

python-can's configuration works as well, so a program that calls `can.Bus()` with no arguments needs no change at all:

```sh
export CAN_INTERFACE=darwincan CAN_CHANNEL=can0
```

## Coming from `socketcan`

| On Linux | Here |
|---|---|
| `can.Bus(interface="socketcan", channel="can0")` | `can.Bus(interface="darwincan", channel="can0")` |
| `ip link set can0 up type can bitrate 500000` | `canctl link set can0 up type can bitrate 500000`, which takes the same words, or pass `bitrate=500000` to `can.Bus` |
| `channel=""` for every interface | `channel="any"` |
| `receive_own_messages`, `local_loopback`, `fd`, `can_filters`, `ignore_rx_error_frames` | The same options exist, with the same defaults |
| `bus.send_periodic(...)`, which uses the kernel's broadcast manager | The same call, which uses the daemon's broadcast manager; see below |
| `isotp.socket` of the `can-isotp` package | `darwincan.IsoTpSocket`; see below |

A bus that is given a `bitrate` brings its interface up if the interface is down, as python-can's interfaces for other adapters do:

```python
bus = can.Bus(interface="darwincan", channel="can0", bitrate=500000)
bus = can.Bus(interface="darwincan", channel="can1", fd=True,
              bitrate=500000, data_bitrate=2000000)
bus = can.Bus(interface="darwincan", channel="can2", bitrate=250000,
              listen_only=True)      # never transmits or acknowledges
```

An interface that is already up is shared with any other programs that have it open, so a bus never reconfigures it. Its bitrate, data bitrate and listen-only mode must then equal the ones asked for. If they do not, opening the bus fails with `can.CanInitializationError`, which names both the current setting and the one asked for, so that the program never runs at a bitrate it did not ask for. To change these settings, first take the interface down with `canctl link set can0 down`.

Bringing an interface up for CAN FD (`fd=True`) needs a `data_bitrate` as well, and `listen_only` needs a `bitrate` when the interface is down.

Closing the bus leaves the interface up, since another program may have begun to use it. Without a `bitrate`, a bus leaves its interface as it finds it, as `socketcan` does. The bus can be opened on an interface that is down, and sending then fails until `canctl` or another bus brings the interface up.

As on Linux, a bus opened without `fd=True` does not receive the CAN FD frames on the bus and cannot send any.

The following points differ from `socketcan`:

- **Timestamps** come from the daemon, which takes them from the adapter's own clock where the adapter has one. They are in seconds since the Unix epoch, on a clock that never steps. The difference between two timestamps is exact, however late Python reads the messages. A float holds a timestamp to about a quarter of a microsecond.
- **`send` never waits**, so its `timeout` is not used. It fails at once with `can.CanOperationError` on an interface that is down, bus-off or unplugged.
- **A frame that the adapter could not put on the bus** after the daemon accepted it (because nothing on the bus acknowledges and the adapter is full) cannot be reported by the `send` call, which returned long before. Instead, the next `recv` logs the failure as a warning on the `can.darwincan` logger and counts it in `bus.transmit_failures`. It does not raise an exception, so a `can.Notifier` keeps running.
- **`bus.state`** reads `ACTIVE` for an interface that is on the bus, `PASSIVE` for a listen-only interface and `ERROR` for one that is down or gone. It cannot be set; use `canctl` to change the state of an interface.
- **`bus.flush_tx_buffer()`** waits until the daemon has handled every frame sent on the bus, which is what a test needs before it looks at what another bus received.
- **Closing is safe from another thread.** `bus.shutdown()` makes a `recv` that is waiting in another thread end with `can.CanOperationError`.

## Waiting

`recv(timeout)` waits in the library. Ctrl-C ends the wait.

`bus.fileno()` returns a descriptor for use with `select` or an event loop. The descriptor is readable exactly when `recv(timeout=0)` would return a message. It is not the socket itself, so never read from it.

```python
import select

readable, _, _ = select.select([bus_a, bus_b], [], [], 1.0)
for bus in readable:
    message = bus.recv(timeout=0)
```

One thread may receive on a bus while others send. If more than one thread receives, wrap the bus in `can.ThreadSafeBus`, as with any python-can interface.

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

The frames are sent by the daemon's broadcast manager on a real-time thread, so they leave on time whether or not Python is running at that moment: a busy interpreter, the garbage collector or another thread holding the interpreter lock does not delay them. `modify_data` changes what is sent without disturbing the rhythm, `stop` ends the sending, `start` begins it again, and a `duration` ends it after that length of time. If the task is given a list of messages, it sends one of them each period, in turn.

Two kinds of task are sent from a Python thread instead, and so have only the timing that a Python thread can give: a task with a `modifier_callback`, because the daemon cannot call Python for each frame, and any task on a bus opened on `"any"`.

A bus can have only one task per identifier: a second `send_periodic` for an identifier that is already being sent raises `ValueError`, as on Linux. To change the running task, use `modify_data`.

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

The daemon does the segmentation and the flow control, and keeps the gaps between frames (STmin). The standard's timing requires this, and a Python loop over a bus cannot promise it. `send` returns when the peer has the whole message, and raises `IsoTpError` if the transfer fails. The error's `failure` is one of `IsoTpSocket.FAILURE_TIMEOUT`, `FAILURE_OVERFLOW` and so on. `IsoTpError` is a `can.CanOperationError`.

| Option | Meaning |
|---|---|
| `extended_id=True` | Both identifiers are 29-bit |
| `fd=True`, `bitrate_switch=True`, `tx_dl=64` | Use CAN FD frames, send them at the data bitrate, and give them this length |
| `padding=0xCC` | Pad frames with this byte |
| `block_size`, `st_min` | The block size and STmin that this end asks of the peer when this end receives. `st_min` is the byte as it appears on the wire: 0 to 0x7F are milliseconds, 0xF1 to 0xF9 are 100 to 900 microseconds |
| `n_bs`, `n_cr` | Seconds to wait for the peer's flow control and for its next frame (1 s each by default) |
| `max_rx_len` | The longest message accepted (1 MiB by default) |

`recv_timestamped` also returns the time of the message's last frame, and `fileno()` returns a descriptor to wait on, as it does for a bus. One thread may receive while others send; a channel carries one message at a time in each direction.

## Not there

- **J1939.** The daemon and the C library support it ([c-api.md](c-api.md#j1939)), but the Python package does not yet.
- **The broadcast manager's receive jobs**, which report when a frame changes or stops arriving, are not available. Use the C library or a `can.Notifier` instead.
- **A wheel that includes the C library** does not exist yet. The library is installed separately, with `make install`.

## When something does not work

| What you see | What it means |
|---|---|
| `CanInterfaceNotImplementedError: the darwinCAN C library could not be loaded` | `make install` has not been run, or the library is somewhere the package does not look. The message lists where it looked. Set `DARWINCAN_LIBRARY` to the library's path. |
| `Unknown interface type "darwincan"` | The package is not installed in the Python that is running. Run `pip install ./python` with that Python's `pip`. |
| `CanInitializationError: could not open can0: cannot reach darwincand ...` | `darwincand` is not running, or `DARWINCAN_SOCKET` points somewhere other than the daemon's socket. |
| `... no such interface: can0` | No interface has that name. `canctl link show` lists the names that exist; an adapter that is not plugged in has no interface. |
| `... version ...` when opening | The library and the daemon are from different builds. Install both from the same build and restart the daemon. |
| `CanOperationError: can0 is down` on `send` | The interface is down. Give `can.Bus` a `bitrate`, or run `canctl link set can0 up type can bitrate ...`. |
| Nothing is received | Check with `canctl link show` that the interface is up and at the bus's bitrate, and check that something else is on the bus to acknowledge. `candump can0` shows what the daemon sees. |

## Tests

The tests in `python/tests` run against a real daemon. `cargo test` starts one and runs them when `.venv/bin/python` (or the interpreter named in `DARWINCAN_PYTHON`) has python-can and the package installed:

```sh
python3 -m venv .venv
.venv/bin/pip install -e python
cargo test -p darwincand --test python_api
```

Without such an interpreter, that test reports that it was skipped, and passes.
