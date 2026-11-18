# Changelog

This file records what changes in each release of darwinCAN. The newest release comes first.

darwinCAN has no stable interface before version 1.0: any release may change the Rust, C and Python interfaces and the protocol between the daemon and its clients. The daemon, the libraries and the programs that use them must always come from the same release.

## 0.1.0 (2026-11-18)

The first public release. It has been developed and tested on macOS 26 on Apple silicon. See [docs/roadmap.md](docs/roadmap.md) for the state of each part and for the known limitations.

### The daemon

- `darwincand` owns the USB CAN adapters and shares each channel, as a named interface, between any number of programs. Every program has its own filters, loopback settings and error frame mask, as a SocketCAN socket has on Linux.
- Virtual interfaces (`vcan0` by default, more with `--vcan`) work with no hardware attached.
- Frames are timestamped once, in the daemon, from the adapter's own clock. A frame that a program sends is reported as sent only when the adapter confirms that it was on the bus.
- The daemon restores interfaces after the Mac wakes from sleep, removes the interfaces of an adapter that is unplugged, and takes its adapters off the bus when it stops.
- Each adapter channel keeps its interface name, whatever else is plugged in and in whatever order. `canctl link set <name> name <new name>` renames one.
- `darwincand install` registers the daemon as a launch agent, so that it starts at login and the tools work without starting anything by hand. Installed with Homebrew, the agent names the path that Homebrew keeps across upgrades.
- The daemon limits the ISO-TP and J1939 message data it holds: 128 MiB waiting to be sent and 128 MiB waiting to be read for each connection, and 1 GiB for all connections together (`--memory-limit`). A program that sends more waits; it gets no error.
- The protocol between the daemon and its clients is at version 7. A client from another version is refused with a message that says so.

### Protocols

- ISO-TP (ISO 15765-2) with normal addressing, over classic CAN and CAN FD. Flow control and STmin are timed in the daemon, and the gap between consecutive frames is never shorter than STmin.
- The broadcast manager (`CAN_BCM`): cyclic transmission, and reception with change detection and timeouts.
- SAE J1939 (`CAN_J1939`): sockets bound to an address or to a NAME (the daemon follows the address claims on the bus, and `j1939acd` claims and defends an address), and the transport protocols (BAM, RTS/CTS and the extended transport protocol) for messages of up to 117,440,505 bytes.

### Hardware

- PEAK PCAN-USB FD, PCAN-USB Pro FD, PCAN-USB X6 and PCAN-Chip USB (`peak_fd`): classic CAN and CAN FD. Tested on the PCAN-USB X6.
- PEAK PCAN-USB (`pcan_usb`): classic CAN. Tested on the IPEH-002021.
- Kvaser adapters with the "hydra" protocol (`kvaser_hydra`): classic CAN. Tested on the USBcan Light 4xHS.
- A Kvaser adapter that a web browser has open for a moment as it is plugged in is waited for, for up to five seconds.
- Models other than those named as tested are driven by the same code but have not been tried. [docs/hardware.md](docs/hardware.md) says what was tested on each adapter and how each behaves.

### Command-line tools

- `candump`, `cansend`, `cangen`, `canplayer`, `cansniffer`, `isotpsend`, `isotprecv` and `isotpdump`, with the command lines and the output of can-utils.
- The J1939 tools `j1939acd`, `j1939spy`, `j1939cat`, `j1939sr` and `testj1939`, likewise.
- `canctl`, which takes the place of `ip link` (`canctl link set can0 up type can bitrate 500000`), lists adapters, and measures delivery latency.
- An option of the Linux tools that cannot be supported is refused with a message and is never silently ignored. [docs/tools.md](docs/tools.md#coming-from-linux) lists them.

### Libraries

- A Rust library (`darwincan`) with `RawSocket`, `IsoTpSocket`, `BcmSocket` and `J1939Socket`, and `Control` for managing interfaces. It has no third-party dependencies.
- A C library (`libdarwincan`, header `darwincan.h`) that uses the structs and constants of `<linux/can.h>`, installed by `make install` together with a pkg-config file.
- A Python package that is a [python-can](https://python-can.readthedocs.io) interface (`interface="darwincan"`), with cyclic tasks, ISO-TP sockets and a `J1939Socket`.

### Licensing

- The daemon (`crates/darwincand`) is licensed under GPL-2.0-only, because its hardware backends are derived from the Linux kernel's CAN drivers.
- The libraries, the protocol crates, the tools and the Python package are licensed under MIT or Apache-2.0, at your option. A program talks to the daemon over a socket and does not link it, so a program built on darwinCAN may carry any licence.

### Installing

- With Homebrew: `brew install b3rtoz/darwincan/darwincan`. The formula builds from source.
- From the source: `make install` installs the daemon, the tools, the C library, its headers and a pkg-config file.

### Requirements

- macOS and the Xcode command line tools.
- Rust 1.85 or later to build everything. The libraries and the tools alone build with Rust 1.75.
