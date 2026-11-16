# Status and roadmap

Where darwinCAN stands, what is planned, and what it does not do. Nothing here is a promise of a date. If something on the list matters to you, say so in an issue, or [help with it](../CONTRIBUTING.md).

## Status

Version 0.1: early, usable, and not yet stable. Interfaces may still change, and the daemon and its clients must come from the same build.

| Area | State |
|---|---|
| Daemon: shared interfaces, per-socket filters, loopback, error frames, timestamps | Works; tested on hardware |
| Virtual interfaces | Work |
| Hardware: PEAK PCAN-USB, PCAN-USB X6; Kvaser USBcan Light 4xHS | Tested; see [hardware.md](hardware.md). Other models of the same families are supported and untested |
| CAN FD | Tested between two channels of a PCAN-USB X6 |
| ISO-TP (normal addressing, classic CAN and CAN FD) | Works; STmin verified on the wire |
| Broadcast manager (`CAN_BCM`) | Works; tested on hardware |
| SAE J1939 (`CAN_J1939`), with BAM, RTS/CTS and the extended transport protocol | Works; tested only against itself |
| Tools: `candump`, `cansend`, `cangen`, `canplayer`, `isotpsend`, `isotprecv`, `canctl` | Work |
| Rust library, C library, Python package | Work; no stability promise yet |
| Launch agent (`darwincand install`) | Tested under a temporary label; little use so far |

Tested on macOS 26 on Apple silicon. Older macOS versions and Intel Macs are expected to work and are untested.

## Planned

Roughly in order of intent.

| Item | Notes |
|---|---|
| J1939 tools: `j1939spy`, `j1939cat`, `j1939sr`, `testj1939`, and `j1939acd` to claim and defend an address | As in can-utils |
| J1939 from Python | A socket class over the C API, as `IsoTpSocket` is for ISO-TP |
| J1939 checked against a reference | A real J1939 device, or Linux's `CAN_J1939`, on the same bus |
| Automatic bus-off recovery (`restart-ms`) | Today a bus-off interface is restarted by taking it down and up |
| Kvaser "leaf" backend | Leaf Light v2 and the other older Kvaser adapters |
| gs_usb / candleLight backend | The open firmware used by many low-cost adapters |
| SLCAN serial backend | |
| Packaging | A Homebrew formula; a Python wheel that carries the C library, so that `pip install` is all |
| ISO-TP extended and mixed addressing | |
| More tools | `cansniffer`, `isotpdump`, `candump -x` |
| Long soak tests | Hours, at high bus load, from several nodes |

Smaller pieces left out of what exists:

- J1939: the `RX_DPO` notice, a per-message priority from C, and retransmission requested by the receiver (a session with a missing packet is aborted instead).
- Python: the broadcast manager's receive jobs; a ready-made connection for `udsoncan`.
- Tools: the options listed as unavailable in [tools.md](tools.md#coming-from-linux).

## Known limitations

- **No independent reference yet.** Every hardware test has had darwinCAN at both ends of the cable. Two makers' controllers agree with each other, but nothing has been checked against another CAN stack.
- **Bus-off needs a manual restart**, and has not been provoked on every adapter.
- **A daemon that is killed outright** (`kill -9`, a crash) leaves its adapters on the bus, acknowledging frames, until the next daemon starts. A daemon stopped normally takes them off.
- **ISO-TP gaps are a little longer than STmin** on hardware: by what an adapter takes to report a frame sent (0.02 to 1.2 ms, depending on the adapter). This is the price of never being shorter.
- **Timestamps can read up to a millisecond late on a quiet bus** with some adapters; see [hardware.md](hardware.md#timing-on-real-adapters).
- **An adapter without a serial number** (the classic PCAN-USB) is named by the USB port it is in, and gets another interface name in another port.
- **No stable API or protocol.** Rebuild and reinstall everything together.

Open questions about specific adapters, and tests still to be run, are listed under "Open" in the [validation log](notes/hardware-validation-log.md#open).

## Not planned

- **An `AF_CAN` socket family, or a shim that lets Linux source compile unchanged.** macOS offers no way to add a socket family without a kernel extension, and a source-level shim would hide differences that matter. Porting means replacing the socket calls; the data structures stay as they are.
- **CAN XL**, until adapters for it are common.
- **Other operating systems.** Linux has SocketCAN; darwinCAN exists because macOS has not.
