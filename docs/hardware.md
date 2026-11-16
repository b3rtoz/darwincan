# Hardware support

This page lists the USB CAN adapters that darwinCAN drives, how far each has been tested, and what to expect from them.

An adapter counts as **tested** only for what has actually been run against the real device. Everything else is marked untested, however likely it is to work. The dated record behind every statement on this page is [notes/hardware-validation-log.md](notes/hardware-validation-log.md).

## Supported adapters

| Driver | Adapters | Tested on | Status |
|---|---|---|---|
| `peak_fd` | PEAK PCAN-USB FD, PCAN-USB Pro FD, PCAN-USB X6, PCAN-Chip USB | PCAN-USB X6, firmware 3.2.0 | Classic CAN and CAN FD are tested. The other three models are untested. |
| `pcan_usb` | PEAK PCAN-USB (classic CAN, IPEH-002021 and IPEH-002022) | IPEH-002021, hardware revision 86, firmware 8.6.2 | Tested. |
| `kvaser_hydra` | Kvaser adapters with the "hydra" protocol: USBcan Light 4xHS, USBcan Pro 2xHS v2, 4xHS, 5xHS and 5xCAN, Leaf Pro HS v2, Leaf v3, U100, U100P, U100S, Memorator Pro 2xHS v2 and 5xHS, Memorator 2xHS v2, Hybrid CAN/LIN and Hybrid Pro CAN/LIN, BlackBird v2, Mini PCIe 1xCAN, VINING 800 | USBcan Light 4xHS, firmware 3.18.288 | Classic CAN is tested. The other models are untested, and so is CAN FD on those that have it. |

The following are not supported yet: Kvaser's older adapters (Leaf Light, Leaf Light v2 and USBcan II, which speak the "leaf" protocol), gs_usb / candleLight devices, and SLCAN serial adapters. See the [roadmap](roadmap.md).

No driver, kernel extension or root access is needed. macOS lets only one program claim a USB device, so quit other CAN software (PCAN-View, MacCAN-based tools, Kvaser CANlib) before starting the daemon.

## What each adapter can do

| | PCAN-USB X6 (`peak_fd`) | PCAN-USB (`pcan_usb`) | USBcan Light 4xHS (`kvaser_hydra`) |
|---|---|---|---|
| Classic CAN | Yes | Yes | Yes |
| CAN FD | Yes | No | No |
| Listen-only | Yes | Yes | No |
| Reports each frame sent (transmit confirmation) | Yes | Yes, from hardware revision 41 | Yes |
| Timestamp resolution | 1 µs | 42.7 µs | 102.4 µs |
| Error counters in error frames | Yes | Yes | No (always 0) |
| Unsent frames the daemon lets it hold | 32 | 32 | 32 |
| Channels | 6 | 1 | 4 |

A setting an adapter cannot honour is refused with a message, and the interface stays down. In particular, an adapter without a listen-only mode is never started in a mode where it would acknowledge frames instead.

## What "tested" covers

Each tested adapter has been through the following checks, on a real bus with a second adapter:

| Area | Checks |
|---|---|
| Frames | Standard, extended, remote and empty frames in both directions, compared at both ends |
| Load | Back-to-back floods and soaks with every frame counted (30 minutes at half bus load on the PEAK adapters, over a minute at 80 % on the Kvaser): no frame was lost and none arrived out of order |
| Timing | Hardware timestamps compared between adapters; cyclic sending; ISO-TP with STmin measured on the wire |
| Transmit confirmation | A frame is shown as sent only once it has been acknowledged on the bus; frames that are waiting when an interface goes down do not go out later |
| Nobody acknowledging | Error-warning and error-passive are reported; sends are refused once the adapter's buffer is full; traffic resumes when a partner appears |
| Unplugging | The cable is pulled while sending and receiving: programs are told at once, the daemon carries on, and the adapter returns under the same interface names |
| Sleep and wake | Interfaces that were up are restored after the Mac wakes, and programs carry on without a restart |
| Daemon stopped or killed | A daemon that is stopped takes its interfaces off the bus; after a daemon is killed, the next one takes them off |

What was run differs between the adapters:

| Adapter | Bitrates with traffic | Also tested | Not yet tested |
|---|---|---|---|
| PCAN-USB X6 | 125 kbit/s to 1 Mbit/s; CAN FD data at 1 to 8 Mbit/s | Bus-off caused by a short circuit, and recovery from it; CAN FD frames of every length with and without bit rate switching; ISO-TP over CAN FD; J1939 transport; all six connectors | CAN FD against another maker's node; long cables and non-default sample points |
| PCAN-USB | 125 kbit/s to 1 Mbit/s | Bus-off caused by a short circuit, and recovery from it | Hardware older than revision 41 |
| USBcan Light 4xHS | 250 kbit/s | Two of its own channels on one bus | Bus-off; other bitrates under traffic (10 kbit/s and 1 Mbit/s are accepted); a daemon stopped by a signal (a killed one was tested) |

Every test so far has had darwinCAN at both ends of the cable, with two different makers' controllers. Nothing has yet been checked against an independent CAN stack such as a Linux machine or a production ECU.

## Using an adapter

The daemon attaches a supported adapter when it is plugged in and removes it when it is unplugged. An interface starts in the down state: configure it and put it on the bus with `canctl`, as with `ip link` on Linux.

```sh
canctl link show
canctl link set can0 up type can bitrate 500000                         # classic CAN
canctl link set can0 up type can bitrate 500000 dbitrate 2000000 fd on  # CAN FD
canctl link set can0 up type can bitrate 500000 listen-only on          # never transmit or acknowledge
candump -e can0                                                         # -e also shows error frames
canctl link set can0 down
```

**Names.** Each adapter channel keeps the interface name it was first given, whatever else is plugged in and in whatever order. An adapter is recognised by its serial number; one without a serial number of its own (the classic PCAN-USB) is recognised by the USB port it is in, and it gets a different name if it is moved to another port. `canctl link set can0 name chassis` gives an interface a name of your choosing. `canctl list` shows which adapter and channel is behind each name. A multi-channel adapter's interfaces follow the order of the connectors on its housing.

**Sleep.** Adapters lose their bus settings when the Mac sleeps. About a quarter of a second after waking, the daemon restores every interface that was up and sends its clients a "restarted" error frame. Frames that were with the adapter at that moment are lost. Use `caffeinate -i` to keep the Mac awake while a bus must not be interrupted.

**Bus-off.** A controller driven off the bus by a fault stays off: the interface refuses frames with "is bus-off; take it down and up again" until that is done. There is no automatic restart yet.

**An unacknowledged frame stays with the adapter** and is retried until something acknowledges it. Once the adapter holds 32 such frames, further sends wait up to 500 ms and are then refused with "adapter transmit buffer is full". The same happens to a sender whose frames keep losing arbitration to a flood of higher-priority traffic. Taking the interface down discards what was waiting.

## Timing on real adapters

Frames are timestamped by the adapter and placed on the Mac's clock by the daemon (see [design.md](design.md#timestamps)). The following figures were measured:

| Measure | PCAN-USB X6 | PCAN-USB | USBcan Light 4xHS |
|---|---|---|---|
| Scatter of timestamps on back-to-back frames | 3–6 µs | 17–22 µs | One step of 102.4 µs |
| The same frame stamped by two adapters | Within about 0.1 ms on a busy bus | Within about 0.1 ms on a busy bus | Within 0.1 ms of the X6 |
| ISO-TP gap in excess of STmin (never under it) | 0.2 to 1.2 ms | About 0.4 ms | 0.02 to 0.26 ms |
| Cyclic 10 ms message, spread of intervals | 9.83 to 10.15 ms | Not measured | 9.83 to 10.14 ms |

The following points are worth knowing:

- **STmin is a lower bound that is never undercut.** The next consecutive frame is timed from the adapter's report that the previous one was on the bus, so the gap is longer than STmin by the time that report takes to arrive. An STmin of less than a millisecond gives a gap of about 1.2 ms on the X6.
- **On a quiet bus the X6's timestamps can read up to 1 ms late.** The X6 delivers most messages the greater part of a millisecond after the event, and delivers one quickly only now and then; the daemon places timestamps by the quickest delivery it has seen recently. On a busy bus the timestamps are within about 0.1 ms.
- **The USBcan Light 4xHS stamps in steps of 102.4 µs**, each rounded up to the end of its step. Intervals read from its timestamps are exact only to that step.
- **The USBcan Light 4xHS reports no error counters.** Its error frames say "tx 0 rx 0", so they cannot show whether sending or receiving caused an error state. Linux shows the same for this adapter.
- **A classic PCAN-USB older than hardware revision 41 cannot report frames sent.** With such an adapter, a frame is shown as sent when it is handed to the adapter.

## Known gaps

- Bus-off has not been provoked on the Kvaser adapter, nor on any adapter since the daemon moved to version 0.2 of its USB library.
- The longest run is 30 minutes at half bus load; nothing has run for hours, at full load from several nodes, or with more than two nodes.
- CAN FD has been run only over a short cable between two channels of one adapter.
- J1939 has been run only against itself.

The full list, with the details, is under "Open" in the [validation log](notes/hardware-validation-log.md#open).

## Reporting how an adapter behaves

A report on an adapter that is listed as untested, whether it works or fails, is one of the most useful contributions to the project. Open an issue with:

- the adapter's model, hardware revision and firmware version (the daemon prints them when it attaches the adapter, and `canctl list` shows them);
- the macOS version and whether the Mac is Apple silicon or Intel;
- what the adapter was connected to, and the bitrate;
- what you ran, and the daemon's messages (they are in `~/Library/Logs/darwincand.log` when the daemon runs under the launch agent, and in the terminal otherwise).

To add support for an adapter, see [development.md](development.md#adding-a-hardware-backend).
