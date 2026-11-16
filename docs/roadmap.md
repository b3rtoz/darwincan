# Status and roadmap

This page describes where darwinCAN stands today, what is planned next, and what it deliberately does not do. Nothing here is a commitment to a date. If an item matters to you, please say so in an issue, or [help with it](../CONTRIBUTING.md).

## Status

darwinCAN is at version 0.1. It is usable and has been tested on real buses, but it is not yet stable: programming interfaces may still change between releases, and the daemon and the programs that talk to it must come from the same build.

| Area | State |
|---|---|
| Daemon (shared interfaces, per-socket filters, loopback, error frames, timestamps) | Working, and tested on hardware. |
| Virtual interfaces | Working. |
| Hardware support | The PEAK PCAN-USB, the PEAK PCAN-USB X6 and the Kvaser USBcan Light 4xHS are tested. Other models in the same families are supported by the same drivers but have not been tried. See [hardware.md](hardware.md). |
| CAN FD | Tested between two channels of a PCAN-USB X6. |
| ISO-TP (normal addressing, over classic CAN and CAN FD) | Working. The STmin gap has been verified on the wire. |
| Broadcast manager (`CAN_BCM`) | Working, and tested on hardware. |
| SAE J1939 (`CAN_J1939`), including BAM, RTS/CTS and the extended transport protocol | Working. Tested against itself and against Linux's J1939 stack on one bus, not yet against a J1939 device. |
| Command-line tools (`candump`, `cansend`, `cangen`, `canplayer`, `cansniffer`, `isotpsend`, `isotprecv`, `isotpdump`, `canctl`) | Working. |
| J1939 tools (`j1939acd`, `j1939spy`, `j1939cat`, `j1939sr`, `testj1939`) | Working. Compared with can-utils 2023.03 on one bus, except `j1939sr`, which was compared for one case only. |
| Rust library, C library and Python package | Working. There is no stability promise yet. |
| Launch agent (`darwincand install`) | The mechanism is covered by tests, but it has seen little everyday use so far. |

Everything has been developed and tested on macOS 26 on Apple silicon. Older macOS versions and Intel Macs are expected to work, but they have not been tested.

## Planned

The items below are listed roughly in the order they are likely to be done.

| Item | Notes |
|---|---|
| J1939 checked against a device | Messages of every kind have been exchanged with Linux's `CAN_J1939` on one bus, and addresses have been claimed and contended for against can-utils' `j1939acd`. A real J1939 device and several messages at once are still to do. |
| J1939 extended transport protocol checked against ISO 11783-3 | The transport protocol for up to 1785 bytes follows SAE J1939-21. The extended protocol has not been compared with its standard, and it does not ask again for a missing packet. |
| Automatic bus-off recovery (`restart-ms`) | Today an interface that has gone bus-off has to be taken down and brought up again by hand. |
| Kvaser "leaf" backend | Support for the Leaf Light v2 and Kvaser's other older adapters. |
| gs_usb / candleLight backend | Support for the open firmware used by many low-cost adapters. |
| SLCAN backend | Support for adapters that present themselves as a serial port. |
| Packaging | A Homebrew formula, and a Python wheel that includes the C library so that `pip install` is the only step. |
| ISO-TP extended and mixed addressing | Only normal addressing is supported today. |
| Long soak tests | Runs lasting hours, at high bus load, with several nodes on the bus. |

Some smaller pieces were left out of features that otherwise exist:

- **Python:** the broadcast manager's receive jobs, and a ready-made connection class for `udsoncan`.
- **Tools:** the options listed as unavailable in [tools.md](tools.md#coming-from-linux).

## Known limitations

- **There is little independent reference yet.** Almost every hardware test so far has had darwinCAN at both ends of the cable. The exception is J1939, which has exchanged messages with Linux's stack; nothing else has been checked against another CAN stack.
- **Bus-off needs a manual restart.** It has also not been provoked on every supported adapter.
- **A daemon that is killed outright leaves its adapters on the bus.** After `kill -9` or a crash, the adapters keep acknowledging frames until the next daemon starts and takes them off. A daemon that is stopped normally takes them off the bus itself.
- **ISO-TP gaps are slightly longer than STmin on hardware.** The excess is the time an adapter takes to report that a frame was sent, which is between 0.02 and 1.2 ms depending on the adapter. This is the price of never sending earlier than STmin allows.
- **Timestamps can read up to a millisecond late on a quiet bus** with some adapters. See [hardware.md](hardware.md#timing-on-real-adapters).
- **An adapter without a serial number is named by its USB port.** This applies to the classic PCAN-USB, which gets a different interface name if it is moved to another port.
- **A receive timeout applies to each read, not to the whole call.** A receive with a timeout can therefore return slightly later than asked, and noticeably later if another thread on the same socket is waiting for an acknowledgement from a stalled adapter.
- **There is no stable API or protocol.** Rebuild and reinstall the daemon, the libraries and your programs together.

### Robustness work still to do

A review of the code before the first public release found no memory-safety bug and no way for a client's input or an adapter's data to crash the daemon. It did find robustness gaps. The clear ones were fixed at once; the ones below remain, mostly because fixing them changes hardware paths that need to be re-tested on every adapter.

- **A transmit report that never arrives costs a place in the transmit window.** On the PEAK FD adapters, if the adapter loses its own report of a sent frame, the daemon counts that frame as outstanding until the interface is restarted. After 32 such losses, sends are refused until the interface is taken down and up.
- **A frame lost to a USB error is not reported to its sender.** The failure is logged, and on the PEAK adapters it is attributed to the next frame instead.
- **A frame sent at the same moment an interface is taken down can stay in the adapter** and be transmitted when the interface next comes up.
- **Shutdown takes interfaces off the bus one after another within a single two-second limit.** One unresponsive adapter can use up that time and leave the others on the bus.
- **The Kvaser backend does not confirm that listen-only mode was accepted** before starting a channel. It only requests listen-only on channels that report the capability, so this matters only if the firmware misreports.
- **The timestamp mapping trusts the adapter's clock.** A single badly wrong timestamp from an adapter can make later timestamps read a few milliseconds early until the mapping corrects itself.
- **The daemon holds its list of interfaces locked while it renames one or reads their state.** A program that renames interfaces in a tight loop can slow down the others.

The daemon trusts every program that can open its socket; see [SECURITY.md](../SECURITY.md).

Open questions about specific adapters, and the tests that still need to be run, are listed under "Open" in the [validation log](notes/hardware-validation-log.md#open).

## Not planned

- **An `AF_CAN` socket family, or a shim that lets Linux source code compile unchanged.** macOS offers no way to add a socket family without a kernel extension, and a source-level shim would hide differences that matter. Porting a program means replacing its socket calls; the data structures stay as they are.
- **CAN XL.** This will wait until adapters for it are common.
- **Other operating systems.** Linux already has SocketCAN. darwinCAN exists because macOS has no equivalent.
