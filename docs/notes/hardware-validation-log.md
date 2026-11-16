# Hardware validation log

The dated record of every run against real adapters: what was tested, what was measured, what went wrong and how it was fixed. It is an engineering notebook, written as the work was done and kept as it was written, so later entries correct earlier ones and "this build" means the build of that day.

For the current state of hardware support read [../hardware.md](../hardware.md); for how to run hardware tests yourself, [../development.md](../development.md#testing-on-hardware). Add an entry here whenever a hardware test is run, and bring the summary in hardware.md up to date with it.

## Bench

| Item | Detail |
|---|---|
| Computer | Apple silicon Mac, macOS 26 |
| Adapter A | PEAK PCAN-USB, IPEH-002021, hardware revision 86, firmware 8.6.2. Classic CAN only. Reports its serial number as `FFFFFFFF` |
| Adapter B | PEAK PCAN-USB X6, firmware 3.2.0. Six CAN FD channels; appears as a USB hub with three two-channel devices |
| Wiring | X6 connector CAN1 to adapter A, 60 Ω between CAN-H and CAN-L, no other nodes |
| USB | Both adapters straight into the computer's USB-C ports up to 2026-11-13; through a USB dock on 2026-11-14. Say which with every result: the dock made late delivery several times worse before it was fixed |
| Adapter C | Kvaser USBcan Light 4xHS, EAN 73-30130-00831-1, firmware 3.18.288. Four classic CAN channels on an 80 MHz controller. No listen-only mode, no CAN FD. On the bench since 2026-11-15, channel 1 cabled to the X6's bus |
| Not yet on the bench | Kvaser Leaf v3 and U100 (the `kvaser_hydra` backend should drive them); Kvaser Leaf Light v2 (no backend: the older "leaf" protocol) |

## Running the tests

Use a daemon on a throwaway socket so nothing touches an installed one:

```sh
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
cargo build --release
export DARWINCAN_SOCKET=/tmp/dc-hw.sock
target/release/darwincand > /tmp/dc-hw.log 2>&1 &
target/release/canctl -details link show
```

Things that have already cost time:

- **Keep the Mac awake.** It idle-sleeps after a few minutes, and waits in a test stop counting while it sleeps. Run `caffeinate -i -t 900 &` first. Sleep itself is a test case (see below), but not by accident.
- **A bitrate mismatch or a missing node does not give bus-off** on a two-node bus. The sender stops at error-passive: once the other node is error-passive too it no longer destroys the frame, and a missing acknowledgement does not raise the counter further. The unterminated X6 channel that went bus-off on 2026-11-12 stopped at error-passive (receive) on 2026-11-13. Bus-off needs a real fault: CAN-H bridged to CAN-L for a second or two while both adapters send every 50 ms. Check the recording for error frames before going on; one of three attempts made no contact and left no trace.
- **The X6 keeps frames it was handed while bus-off** and sends them in a burst when it is restarted (39 frames seen). The classic PCAN-USB discards them. Since 2026-11-13 the daemon refuses frames for a bus-off interface, so only those already in the adapter are affected.
- **`candump` on several interfaces ends when one of them is removed.** Start another before going on with a test.
- **Check which interface is which on every daemon start.** Interfaces are numbered in the order the adapters are found. The same bench has come up as classic = `can0`, X6 = `can1`–`can6`, and as X6 = `can0`–`can5`, classic = `can6`. Read `canctl -details link show` before every run. Adapters there when the daemon starts are taken in the order of the ports they are plugged into; one plugged in later takes the first free names. Since 2026-11-14 the X6's six interfaces keep their order among themselves (connector CAN1 is its first), but which six names they are still depends on the above.
- **Only X6 connector CAN1 is identified.** It is the X6's first interface. The other five should follow in the order printed on the housing; that is unconfirmed until something is wired to them.
- **Settings persist across down and up**, as on Linux. After `listen-only on`, say `listen-only off` to transmit again.
- **Frames queued in an adapter survive down and up** and are sent when the interface comes back. After a test that fills the transmit buffer, expect stale frames and error states that were reported before a new `candump` attached.
- **An error state is reported once, when it changes.** A `candump -e` started after the adapter went error-passive shows nothing.
- **A listen-only receiver sees the sender's retries** as an endless stream of frames, so give `candump` a frame count (`-n`), not only a timeout.
- **No termination means errors.** An unterminated X6 channel at 500 kbit/s counts receive errors when it transmits, and goes bus-off on a CAN FD frame with bit rate switching. At 20 and 125 kbit/s the transmit counter climbs as expected.
- **macOS has no `timeout` command.** Use `perl -e 'alarm 40; exec @ARGV' <command>` to bound a tool that waits.
- **Start listen-only** on a bus that is not yours alone, and transmit only once you know what the adapters are wired to.

Useful commands:

```sh
# Counted stream, checked for loss and order on the other side
candump -L -e -n 5000 -T 9000 can1 > rx.log &
cangen can0 -g 0 -n 5000 -I 100 -L 8 -D i

# ISO-TP, 4095 bytes, receiver asks for STmin 5 ms
isotprecv -s 7E8 -d 7E0 -m 05 can1 > rx.txt &
isotpsend -s 7E0 -d 7E8 can0 < payload.txt

# Daemon timing on the virtual bus (required after touching bus.rs or session.rs)
canctl latency vcan0

# Finding what holds frames up: each part reports what took over 2 ms
darwincand --timing-log 2
DARWINCAN_PACER_LOG=2 cangen can0 -g 1 -n 720000 -I 200 -L 8 -D i
```

## Results

On 2026-11-12 unless a date is given, classic CAN unless stated. "A" is the classic PCAN-USB, "X6" is X6 connector CAN1.

### Each adapter alone, nothing wired

| Test | A (`pcan_usb`) | X6 (`peak_fd`) |
|---|---|---|
| Attach at daemon start | Pass | Pass, all six channels |
| Unplug and replug while down | Pass | Not run |
| Bring up at several bitrates | Pass: 33.333, 250, 500 kbit/s | Pass: 250 kbit/s; 500 kbit/s nominal with 2 Mbit/s data on all six |
| Listen-only: sends refused | Pass | Pass |
| CAN FD settings | Refused with a message, as intended | Accepted |
| Transmit with no acknowledging node | Error-warning (tx 96), then error-passive (tx 128) | Same at 20 and 125 kbit/s |
| Transmit a CAN FD frame with bit rate switching | n/a | Error-warning, error-passive, then bus-off reported |
| Fill the transmit buffer | "adapter transmit buffer is full" after 35 frames | Same message |
| Down, then up | Controller returns error-active | Reports errors afresh |

### The two adapters wired together

| Test | Result |
|---|---|
| Standard, extended and remote frames, both directions | Pass, no error frames |
| Back-to-back floods at 125 kbit/s, 500 kbit/s and 1 Mbit/s, both directions | Pass: every frame received, in order, over 40,000 frames in total |
| Streams at 1 ms and 10 ms spacing, both directions | Pass, no loss |
| ISO-TP, 4095 bytes, both directions, STmin 5 ms, 1 ms, 0.5 ms and 0 | Pass, data identical |
| Listen-only on the X6 while A sends | Pass: A gets no acknowledgement and goes error-passive; the X6 sees the frame |
| Listen-only on A while the X6 sends | Pass, likewise |
| Sleep and wake with three interfaces up (A, X6 CAN1 and the second channel of the same X6 device), 2026-11-13 | Pass after a fix. The daemon restored all three and sent "restarted" error frames; 500 of 500 frames at 1 ms spacing arrived in each direction after an 85 s sleep. The X6's receive timestamps were frozen after the first wake (see the bugs table); after the fix and a second sleep every gap was 0.83–1.15 ms |
| Daemon stopped by SIGTERM with three interfaces up, 2026-11-13 | Pass: the log says "taken off the bus" for each; after a restart a frame from A alone got no acknowledgement and A went error-passive |
| Daemon killed by SIGKILL with A and X6 CAN1 up, 2026-11-13 | Pass after a fix. The X6 stayed on the bus and acknowledged A's frame while the restarted daemon listed it as down. With channels taken off the bus at attach, the same frame gets no acknowledgement |
| Unplug A while up, sending and receiving at 20 ms spacing, with a `candump` attached, 2026-11-13 | Pass: "can0 removed" logged, `candump` told and ended, the sender's frame refused (with an unhelpful "unknown error", since reworded), the X6 went error-passive for want of an acknowledgement. Replugged, it attached as `can0`, down; 500 of 500 frames each way once up |
| Unplug the X6 likewise, 2026-11-13 | Pass: all six interfaces removed, `candump` and sender ended the same way. Replugged, the three devices attached under the same names, down and without their earlier settings; 500 of 500 frames each way once up, gaps 0.79–1.21 ms |
| Bus-off by a short between CAN-H and CAN-L, both adapters sending every 50 ms, 2026-11-13 | Pass after a fix. Both reported error-warning, error-passive and bus-off within a millisecond. Neither came back by itself in 30 s. Down and up recovered each; frames crossed both ways again. Before the fix `cansend` went on succeeding and its frames were echoed to clients though none reached the bus; after it, both senders were refused with "is bus-off; take it down and up again" until the restart |
| ISO-TP regression after the day's changes, 4095 bytes, both directions, STmin 5 ms, 1 ms and 0, 2026-11-13 | Pass, data identical in all six. Pacing as on 2026-11-12. One gap of 12.4 ms at STmin 5 ms from the X6 to A, not explained |
| Soak: 30 minutes, 1.8 million counted frames in each direction at once, 1 ms spacing (about half bus load), 2026-11-13 | Pass for delivery: every frame received in both directions, in order, no error frames. Timing: 99.94 % of the gaps at the X6 and 99.98 % at A were within 0.5–1.5 ms, with two unexplained effects listed under Open |
| Soak after the calibration fix: 12 minutes, 720,000 frames each way at 1 ms spacing, with `canctl latency vcan0` looping beside it, 2026-11-13 | Pass for delivery: nothing lost, nothing out of order, no error frames, no timestamp running backwards. Gaps under 0.5 ms at the X6 fell from 470 in 30 minutes to 20 in 12, all next to a stall. Stalls of 5–11 ms were still there: see Open |
| Down and up from error-passive, 2026-11-13 | Pass on an unwired X6 channel: "error active" reported, then errors counted afresh. Bus-off itself was not reached |

### Transmit echo from the adapters (probe, 2026-11-13)

Done with temporary code, to settle the design of transmit confirmation before changing the `Backend` contract. The Linux driver for the X6 family never asks for an echo, so this was not known.

| Question | X6 (`peak_fd`) | A (`pcan_usb`) |
|---|---|---|
| How an echo is asked for | Flag `0x04` ("looped back") in the transmit record | Bit 0 of the identifier word (SRR) and one extra byte after the data, as the Linux driver does for its loopback mode |
| What comes back | A receive record with flag `0x04`, the adapter's timestamp, and the client byte and 32-bit tag of the transmit record unchanged | A receive record with the SRR bit, the adapter's timestamp and the extra byte (`0x80` sent, `0x80` returned; other values not tried) |
| When it comes back | Only once the frame is acknowledged on the bus: with A down nothing came for 2 s, then the echo arrived when A came up | The same, with the X6 down and up |
| Flag `0x80` ("self receive") in the transmit record | No effect seen | n/a |

Not tried in the probe: CAN FD frames, and the echo of a frame that is pending when the channel goes bus-off.

### Transmit confirmation on the X6 (2026-11-13)

The X6 backend asks for an echo of every frame and passes it to the daemon as the report that the frame was sent. The classic PCAN-USB still works the old way (echo at hand-over) until its turn.

| Test | Result |
|---|---|
| Data, remote and extended frames from the X6 | Pass: each is shown on `can1` when the adapter reports it, with the adapter's timestamp |
| A down, the X6 sends 5 frames | Pass: nothing shown on `can1` for 2 s but the error states; `cansend` returns at once without error, as on Linux |
| A comes up | Pass: the 5 frames go out and are shown, in order |
| A down, 5 frames, X6 down and up, A up | Pass: none of the 5 goes out. The adapter's queue is emptied on down with the "transmit abort, flush" command, which the Linux USB driver does not use; it works on firmware 3.2.0 |
| 40 frames with nobody to acknowledge | Pass: 32 are taken, 8 refused with "adapter transmit buffer is full" after 500 ms each |
| Flood of 20,000 frames back to back to A | Pass: all received, in order |
| Soak, 12 minutes, 720,000 frames each way at 1 ms spacing | Pass for delivery: nothing lost or out of order, no error frames. Six stalls of 5–8 ms, in both directions at once: see Open |
| ISO-TP, 4095 bytes to A, STmin 5 ms, 1 ms, 0.5 ms, 0 | Pass: 75 transfers, all identical. Pacing below |

Where the adapters take their timestamp: a flood of frames of random length showed the gap between two timestamps following the length of the later frame (104 µs before a frame without data, 236 µs before one of 8 bytes, at 500 kbit/s) and not that of the earlier one, on both adapters. So both stamp a frame at its end, which is where STmin counts from.

ISO-TP from the X6, idle time on the bus before each consecutive frame: the gap between the X6's own timestamps of two frames, less the exact length of the later frame (stuff bits counted). STmin is the least allowed.

| STmin | Frames | Shortest | Mean | Under STmin |
|---|---|---|---|---|
| 5 ms | 5,840 | 5.29 ms | 6.06 ms | 0 |
| 1 ms | 23,360 | 1.17 ms | 1.88 ms | 0 |
| 0.5 ms | 11,680 | 0.78 ms | 1.32 ms | 0 |
| 0 | 2,920 | back to back | back to back | n/a |

The idle time is never under STmin, and about 1.1 ms over it on average. The next frame is timed from the adapter's report of the last, which on the daemon's clock is about 0.7 ms later than the frame really ended (the adapter's fastest message takes that long to arrive), and a frame handed over takes 0.2 to 0.6 ms to start. Before, at STmin 1 ms the mean was 1.25 ms and the shortest 1.03 ms; at 0.5 ms the shortest was 0.52 ms.

Tried and dropped: handing the next frame over early by the least delay seen between hand-over and report. With a margin of 0.1 ms one gap in 2,920 was 0.81 ms at STmin 1 ms; with 0.25 ms, one in 35,040 was 0.93 ms. The adapter now and then gets a frame out faster than any seen before.

The classic PCAN-USB on the old path, STmin 5 ms, measured at the X6: shortest 4.37 ms, mean 5.29 ms. It undercut; see the next section.

### Transmit confirmation on the classic PCAN-USB (2026-11-13)

The backend asks for a report of every frame (hardware revision 41 and later; this one is 86) and matches each report to its frame by the byte that goes out and comes back with it. The adapter returns the byte unchanged, any value with the top bit set.

| Test | Result |
|---|---|
| Data, remote (with and without a length), extended and empty frames | Pass: each shown on `can0` when reported. A remote frame's report carries no byte, as the Linux driver's decoding implies |
| X6 down, A sends 5 frames | Pass: nothing shown for 2 s; shown in order when the X6 came up |
| X6 down, 5 frames, A down and up, X6 up | Pass: none goes out. This adapter discards its queue on down by itself |
| 40 frames with nobody to acknowledge | Pass: 32 taken, 8 refused |
| Flood of 20,000 frames back to back to the X6 | Pass: all received, in order |
| ISO-TP, 4095 bytes to the X6, STmin 5 ms, 1 ms, 0.5 ms, 0 | Pass: 55 transfers, all identical |

ISO-TP from the classic PCAN-USB, idle time before each consecutive frame by its own timestamps:

| STmin | Frames | Shortest | Mean | Under STmin |
|---|---|---|---|---|
| 5 ms | 5,840 | 5.19 ms | 5.46 ms | 0 |
| 1 ms | 17,520 | 1.18 ms | 1.38 ms | 0 |
| 0.5 ms | 5,840 | 0.72 ms | 1.32 ms | 0 |

About 0.4 ms over STmin, less than the X6's 1.1 ms: this adapter's reports arrive sooner.

Both adapters together, on the build with the daemon's frame-handling threads in the real-time class:

| Test | Result |
|---|---|
| Both adapters flooding each other at once, 20,000 frames each (the first test at full bus load from two nodes) | Pass: all received in both directions, in order, no error frames |
| Soak, 12 minutes, 720,000 frames each way at 1 ms spacing, real-time threads behind a switch | Pass: nothing lost; every gap in both directions within 0.5–1.5 ms; no timing report from the daemon |
| The same soak with the real-time threads made permanent | Partial: one pause of 46 ms at 625 s, in both directions at once. The X6 reported a receive overrun and four frames from A were lost, the first frames lost in any test. Otherwise every gap within 0.5–1.5 ms but five. See "A pause of tens of milliseconds" under Open |
| Soak, 30 minutes, 1.8 million frames each way at 1 ms spacing, after raising the receive transfers kept waiting from 4 to 32 | Pass: nothing lost or out of order, no error frames, no gap over 5 ms; all but 13 of 3.6 million gaps within 0.5–1.5 ms. The daemon logged adapters' messages reaching it 3 to 18 ms late in 45 separate seconds, both adapters together each time; the bus was not affected |

### Late delivery through a USB dock (2026-11-14)

Soaks of 720,000 frames each way at 1 ms spacing, both adapters through a USB dock, `darwincand --timing-log 3`.

| Build | Seconds with late delivery | Worst | Gaps over 5 ms on the bus | Lost |
|---|---|---|---|---|
| As of 2026-11-13 | 31 in 12 minutes | 82 ms | 3 per direction, longest 53 ms | 0 |
| Every thread of the daemon real-time (experiment) | 0 in 12 minutes | none | 0 | 0 |
| Threads started while an adapter is attached made real-time (a first fix, which missed) | 58 in 30 minutes | 11.5 ms | 0 | 0 |
| The thread started when the daemon begins to watch for adapters made real-time (the fix) | 1 in 12 minutes | 3.2 ms | 0 | 0 |

The first row also shows the 32 receive transfers holding through a 53 ms pause without loss; with four, a 46 ms pause lost four frames.

### The USB library's version 0.2 (2026-11-14)

The daemon moved from `nusb` 0.1.14 to 0.2.9, which changed every USB call in both backends. Through the dock, on that build:

| Test | Result |
|---|---|
| Attach, serial number and firmware information | Pass, both adapters; the library's delivery thread found by name |
| Data, remote and extended frames, both directions | Pass |
| Unacknowledged frames shown only once acknowledged, both adapters | Pass |
| X6 taken down with frames waiting | Pass: none goes out afterwards |
| 40 unacknowledged frames, each adapter | Pass: 32 taken, 8 refused |
| Floods of 20,000 frames, each direction | Pass: all received, in order |
| ISO-TP, 4095 bytes: X6 at STmin 5 ms and 1 ms, A at 5 ms and 0 | Pass: 11 transfers identical; shortest idle time 5.91 ms (X6, 5 ms), 5.23 ms (A, 5 ms), 1.34 ms (X6, 1 ms) |
| Soak, 12 minutes, 720,000 frames each way at 1 ms spacing | Pass: nothing lost or out of order, no error frames, no gap over 5 ms, all but 12 of 1.44 million gaps within 0.5–1.5 ms; no late delivery reported, nor anything else, by `--timing-log 3` |
| Daemon stopped by SIGTERM with both adapters up | Pass: both taken off the bus; after a restart a frame from A alone got no acknowledgement |

| Each adapter unplugged from the dock, and the dock from the Mac, idle bus | Pass: the daemon kept running, every interface was removed and came back (down) each time, frames passed again once they were brought up |
| The same three unplugs with each adapter sending every 20 ms | Pass, with the naming fault below. The sender and `candump` on a pulled adapter end with "the interface was removed (adapter unplugged)"; the sender on the other ends with "adapter transmit buffer is full" once nothing acknowledges. Pulling the dock ended one sender with "USB transfer failed while sending a frame (unknown (error 0xe00002ed)); was the adapter unplugged?", the reworded error seen on hardware for the first time. After the PCAN-USB and the X6 were each plugged back in and brought up, frames passed again in both directions |
| Sleep for 18 s and wake, each adapter sending every 20 ms | Pass: "woke from sleep, restoring interfaces", both interfaces "restarted", the senders and `candump` carried on without being restarted; frames in both directions again 0.25 s after the daemon noticed the wake, timestamps continuous |

**The X6's interfaces changed places when the dock was plugged back in.** The X6 is three USB devices behind its own hub, all with the same serial number, and interfaces are numbered in the order devices are found. After the dock came back the three were found in another order, at the same USB addresses as before: connector CAN1 was `can1` before and `can3` after. The senders restarted on `can0` and `can1`, got no acknowledgement, and it looked like a dead bus. Listening on all six channels found the cable on `can3`, and `can0` and `can3` then exchanged frames. So neither the order of arrival nor the USB address tells the X6's devices apart; the port on the X6's hub does, and the daemon now goes by it: see the bugs table.

During that, `can1`, by then a connector with nothing on it, reported "error passive (receive)" with rx 135 each time it was brought up for sending, while the PCAN-USB was resending an unacknowledged frame on another connector. In listen-only it reported nothing. This is the "500 kbit/s receive-counter observation" under Open again.

Also learnt: `listen-only on` stays set through down and up, as on Linux, until `listen-only off` is given.

Not run again on this build: bus-off (the short could not be made that day). The bus-off results above are from the build before the move.

By the X6's receive timestamps the same transfers show 16 gaps under STmin (the shortest 0.78 ms at 1 ms). They are not real: each is one gap at the moment the X6's clock estimate steps down by the same amount, and the PCAN-USB's own timestamps show the gap before and after unchanged. See "The adapter clock's rate" below.

### Measurements

Hardware timestamps of frames received back to back at 500 kbit/s:

| Receiver | Spacing | Scatter (standard deviation) |
|---|---|---|
| X6 | 252–256 µs | 3–6 µs |
| A | about 248 µs | 17–22 µs (its clock ticks every 42.7 µs) |

ISO-TP, idle time on the wire between consecutive frames (STmin is the minimum allowed), 500 kbit/s, measured from the receiver's hardware timestamps. "Before" is the build that timed STmin from handing a frame to the adapter; "after" is the build, later the same day, that adds the frame's time on the wire.

| Sender | STmin | Before: mean / shortest | After: mean / shortest |
|---|---|---|---|
| X6 | 5 ms | 5.03 / 4.66 ms | 5.29 / 4.94 ms |
| X6 | 1 ms | 0.91 / 0.65 ms | 1.25 / 1.03 ms |
| X6 | 0.5 ms | not measured | 0.68 / 0.52 ms |
| A | 5 ms | 5.06 / 4.68 ms | 5.36 / 4.58 ms |
| A | 1 ms | 1.02 / 0.89 ms | 1.30 / 0.92 ms |
| vcan0 | 5 ms | 5.01 ms mean | 5.01 ms mean |
| vcan0 | 1 ms | 1.01 ms mean | 1.01 ms mean |

`canctl latency vcan0` median on the bench machine: 47 µs on 2026-11-11, 75–85 µs on 2026-11-12 with identical results from the old and the new code, so the difference is the machine's state, not the change.

### The adapter clock's rate (2026-11-14)

`TimeSync` keeps the smallest difference seen between a message's arrival and its timestamp, and used to let that estimate creep later at 200 ppm, the worst a crystal may be out, to follow a slow adapter clock.

Measured as the X6's receive timestamp minus the classic PCAN-USB's timestamp of the same frame, which should be constant since both stamp the end of the frame. With A sending a frame every 5 ms and the X6 only receiving, that build read 0.7 to 1.0 ms nearly all the time: the X6 delivers nearly every message that late on a quiet bus and a fast one only every one to five minutes, and within a minute of a fast one the estimate was back among the slow ones. With a frame every 1 ms each way fast messages are frequent and it read −0.11 to +0.04 ms. So the fault recorded on 2026-11-13 as "steps back every few seconds" is really "about 0.9 ms late on a quiet bus, stepping back at each fast message".

**First attempt, the build of 2026-11-14: wrong, and on `main` for about an hour.** It measured the clock's rate as the median slope between the fastest messages of 4 s windows and crept at only 0.5 ppm more. On steady traffic that looked very good (within −0.06 to +0.17 ms for 450 s after the first fast message). But when traffic changes from quiet to busy the fastest messages get quicker, which looks like an adapter clock gaining on ours; the estimate then ran early. After 20 s of dense traffic, ISO-TP from the X6 at STmin 5 ms left 4.77 ms on average between frames (shortest 4.64 ms), by both adapters' clocks, and 0.72 ms at STmin 1 ms. The runs that were meant to check ISO-TP on that build started from a fresh "up" and did not show it.

**Second attempt (this build).** The creep is the lowest bound on the rate that the fastest messages of 8 s windows over the last 17 minutes allow, taking none of them to have needed more than 2 ms, plus 1 ppm. A timestamp cannot run early that way, and a unit test now runs the quiet, busy, quiet sequence. The gain is smaller and grows with the time an interface has been up:

| Traffic | 200 ppm (before) | This build |
|---|---|---|
| A frame every 5 ms; time for the estimate to be back among the slow messages after a fast one | under a minute | about 40 s after the fast one at 10 s; about 2.5 minutes after the one at 270 s |
| The same; size of the step back at a fast message | 0.9 ms | 0.9 ms (at 270 s and at 530 s) |
| A frame every 1 ms each way, 30 s | −0.11 to +0.04 ms | −0.12 to +0.02 ms |

ISO-TP, 4095 bytes, on this build, idle time between frames at the sender's adapter (mean / shortest) and the shortest by the receiver's:

| Sender | STmin | Before or after 20 s of dense traffic | Transfers | Sender: mean / shortest | Receiver: shortest |
|---|---|---|---|---|---|
| X6 | 5 ms | before | 2, identical | 5.90 / 5.21 ms | 5.03 ms |
| X6 | 5 ms | after | 2, identical | 5.73 / 5.24 ms | 5.23 ms |
| X6 | 1 ms | after | 3, identical | 2.21 / 2.14 ms | 2.07 ms |
| A | 5 ms | after | 2, identical | 5.41 / 5.23 ms | 5.22 ms |
| A | 1 ms | after | 3, identical | 1.39 / 1.30 ms | 0.99 ms |

The 0.99 ms is by the X6's receive timestamps, where the sender's own adapter reads 1.30 ms for its shortest: a step in the X6's estimate, not a short gap.

**The X6 seems to work in 1 ms steps.** On a quiet bus its messages arrive 0.7 to 1.0 ms after the event. At STmin 1 ms its gaps come out at either about 1.3 ms or about 2.2 ms: all at 2.2 ms in the run above, 1 in 100 at 2.2 ms when the estimate was late and the next frame was handed over as soon as the report came. A frame handed over just after a step waits for the next. If so, the X6's surplus over STmin (0.2 to 1.2 ms at STmin 5 ms) is the adapter's and cannot be narrowed without handing frames over early, which is what undercuts STmin.

During the first run the Mac fell asleep for under a second; see "A sleep of under half a second" under Open.

### The X6's interfaces in the order of its connectors (2026-11-14)

The daemon now waits up to 2 s for all three of an X6's USB devices and attaches them in the order of their ports on the X6's hub.

| Test | Result |
|---|---|
| Daemon start, the devices at USB addresses 11, 10, 12 on hub ports 1, 2, 3 | Pass: attached in port order; connector CAN1 is the X6's first interface and exchanged frames with A |
| X6 unplugged and plugged in again three times | Pass: the devices came back at addresses 10/12/11, 10/11/12 and 12/11/10 in the order of their names, which is port order each time (checked against the system's list after the third: 12, 11, 10 on ports 1, 2, 3); connector CAN1 still the first interface, frames exchanged with A in both directions |

All six connectors have been tied to their interfaces by a cable (2026-11-14): with connector CAN1 (`can0`) sending ten frames a second and the other five channels up, the cable's far end was moved from CAN2 to CAN3, CAN4, CAN5 and CAN6 in turn, and the frames were heard and acknowledged on `can1`, `can2`, `can3`, `can4` and `can5` in that order, each only while the cable was on its connector. The interfaces follow the order printed on the housing.

After the run the LED of connector CAN5 was reported flashing red. Its channel (`can4`) had received and acknowledged all 83 frames sent while the cable was on it, and the daemon logged no error state for it; the cause is not known. Hot-plugging the cable in the middle of a frame is a likely one. With the channels down the LED was solid green again, and a check with the cable back on CAN1 and CAN5 found nothing wrong: 3,000 classic frames and 5,000 CAN FD frames with bit rate switching from each end at once (500 kbit/s, data 2 Mbit/s), all 16,000 seen alike at both ends, no error frames.


### CAN FD between two X6 channels (2026-11-14)

Bench for this section only: connectors CAN1 and CAN2 of the X6 cabled together and terminated, the classic PCAN-USB taken off the bus (it cannot read CAN FD frames). The second connector was found by listening only on the five other interfaces while `can0` sent: `can1` heard it, as the port order predicts for connector CAN2. `can0` and `can1` are the two channels of one of the X6's three USB devices, so both ends are the same firmware and the same driver; there is still no independent reference. Sample points were the defaults, the cable is short.

Each bitrate pair ran the same three checks:

- **Every kind of frame**, 168 in all, half from each end, compared byte for byte at both ends: classic frames of 0 to 8 bytes and a remote frame, CAN FD frames of every length (0 to 8, 12, 16, 20, 24, 32, 48, 64) without and with bit rate switching, each with standard and extended identifiers.
- **A flood one way** of 5,000 frames of 64 bytes, without and with bit rate switching. The frame rate shows whether the data phase really ran at the data bitrate.
- **Both ways at once**, 20,000 frames from each end with random lengths, one end switching bit rate and the other not, 0.4 ms apart so that neither starves. The two ends must show the same 40,000 frames in the same order: what one sent, as its adapter reported it, against what the other received.

| Nominal / data bitrate | Every kind of frame | 64-byte flood, frames/s without / with switching | Both ways, 40,000 frames | Error frames |
|---|---|---|---|---|
| 250 kbit/s / 2 Mbit/s | Pass | 367 / 2,225 | Pass, none lost, same order | 0 |
| 500 kbit/s / 1 Mbit/s | Pass | 734 / 1,404 | Pass | 0 |
| 500 kbit/s / 2 Mbit/s | Pass | 734 / 2,582 | Pass | 0 |
| 500 kbit/s / 4 Mbit/s | Pass | 734 / 4,450 | Pass | 0 |
| 500 kbit/s / 5 Mbit/s | Pass | 734 / 5,203 | Pass | 0 |
| 500 kbit/s / 8 Mbit/s | Pass | 734 / 6,973 | Pass | 0 |
| 1 Mbit/s / 8 Mbit/s | Pass | 1,467 / 8,896 | Pass | 0 |

Transmit confirmation works for CAN FD frames as for classic ones: the sender's own view of each frame came from the adapter's report, and matched the receiver's in every run. A frame sent with the error state indicator set arrives with it set, as with the Linux driver.

ISO-TP over CAN FD, at 500 kbit/s / 2 Mbit/s, `7E0` to `7E8`. Every message was received identical to what was sent, and no transfer failed:

| Test | Result |
|---|---|
| 4,095 bytes in 64-byte frames with bit rate switching, STmin 0, 100 µs, 500 µs, 1 ms and 5 ms, each direction, three transfers each | Pass |
| 4,095 bytes with frames of 8, 12, 16, 20, 24, 32 and 48 bytes | Pass |
| 4,095 bytes in 64-byte frames without bit rate switching, STmin 0 and 1 ms | Pass |
| Messages of 1, 7, 8, 61, 62, 63, 64, 4,095, 4,096 and 20,000 bytes in 64-byte frames; 7, 8, 10, 11 and 12 bytes in 12-byte frames (the single-frame limits, and the long first frame above 4,095 bytes) | Pass; a frame over 8 bytes is filled up to the next CAN FD length with `CC` |
| 1,000 bytes with block size 4, STmin 1 ms, padding `55` one way and `AA` the other | Pass: a flow control after every four frames, all frames padded |

Time from the end of one full consecutive frame to the end of the next, by the adapter's timestamps (the same at both ends); a 64-byte frame with bit rate switching lasts at most 0.35 ms, without it 1.21 ms:

| STmin | Frames | Shortest | Mean | Idle time at the shortest |
|---|---|---|---|---|
| 0 | 64 bytes, switching | 0.29 ms | 0.35 ms | back to back |
| 100 µs | 64 bytes, switching | 1.52 ms | 1.59 ms | 1.17 ms |
| 500 µs | 64 bytes, switching | 1.08 ms | 1.59 ms | 0.73 ms |
| 1 ms | 64 bytes, switching | 1.57 ms | 1.60 ms | 1.22 ms |
| 5 ms | 64 bytes, switching | 5.61 ms | 5.66 ms | 5.26 ms |
| 1 ms | 64 bytes, no switching | 2.39 ms | 2.44 ms | 1.18 ms |

No gap is under STmin. With an STmin under a millisecond the X6 still leaves about 1.2 ms: the next frame waits for the adapter's report of the one before, and that takes about a millisecond to arrive. Only STmin 0, where frames are handed over without waiting, is faster.

Two things seen on the way:

- **A timestamp before its predecessor's.** In one transfer with frames 0.11 ms apart, one frame of 1,172 was stamped 15 µs before the frame ahead of it, at both ends alike. This is the estimate of the adapter's clock stepping back at a fast message (see "The adapter clock's rate"); on closely spaced frames the step shows as timestamps out of order. None in the 40,000-frame runs checked.
- **A sender that never wins the bus is told the wrong thing.** With one end flooding a lower identifier without pause, the other end's frames lost every arbitration and `cangen` stopped after 500 ms with "adapter transmit buffer is full (is anything acknowledging on the bus?)". That is how CAN works, but the message guessed the wrong cause; it now also names a bus kept busy by other traffic.

Not covered: a long cable or one with stubs, sample points other than the defaults, a transceiver delay that needs compensating differently, the X6's other four connectors, error states under CAN FD with a partner present, and a node from another maker.

### The C API between two X6 channels (2026-11-14)

Connectors CAN1 and CAN5 of the X6 cabled together (`can0`, `can4`), 500 kbit/s with 2 Mbit/s data. The C self-test (`crates/darwincan/c/selftest.c`), built against the static library, ran three times with one end on each interface: classic, remote and CAN FD frames, filters and joined filters, a socket's own frames coming back, `poll`, ISO-TP in classic and CAN FD frames up to 5,000 bytes in both directions, a transfer that times out with no peer, a receive ended by a signal, and a thread receiving on an ISO-TP channel while another sends on it. All three runs passed; a capture on `can4` showed 4,848 frames and no error frames.

The same program passes on the virtual bus in the test suite. On two interfaces it skips the two checks that only hold on one: that sender and receiver see one timestamp, and that a frame sent with loopback off stays unseen by other sockets.

### The broadcast manager between two X6 channels (2026-11-15)

Connectors CAN1 and CAN5 (`can0`, `can4`), 500 kbit/s with 2 Mbit/s data. The C self-test, which now includes the broadcast manager (a cyclic job, its data changed, read back and deleted; a receive job with a mask and a timeout; a single send), passed three times with one end on each interface.

Cyclic sending from `can0`, 20 seconds each, with the job's data changed once a second, by the example `crates/darwincan/c/cyclic.c`. Time between consecutive frames, by the adapters' timestamps:

| Job | Measured at | Intervals | Shortest | 1 % | Median | 99 % | Longest | Off by more than 0.1 ms |
|---|---|---|---|---|---|---|---|---|
| Every 1 ms | `can4`, receiving | 20,059 | 0.898 ms | 0.971 | 1.001 | 1.028 | 1.130 | 13 |
| Every 1 ms | `can0`, its report of sending | 20,059 | 0.898 | 0.971 | 1.001 | 1.027 | 1.131 | 13 |
| Every 10 ms | `can4`, receiving | 2,007 | 9.827 | 9.901 | 10.002 | 10.105 | 10.145 | 41 |
| Every 10 ms | `can0`, its report of sending | 2,007 | 9.858 | 9.904 | 10.000 | 10.102 | 10.146 | 35 |

No interval was off by more than 0.5 ms, none was lost, there were no error frames and the daemon's timing log (threshold 3 ms) stayed empty. The mean is the interval asked for, so the job does not drift. The two ends are different USB devices of the X6 with their own clocks, and agree.

The 10 ms job looks less even than the 1 ms one. That is probably the measurement, not the sending: at 10 ms the bus is quiet between frames, where the X6's timestamps are known to wander by some tenths of a millisecond (see "The adapter clock's rate").

For comparison, the same on the virtual bus, where the timestamp is the daemon's own: every interval of the 1 ms job within 0.023 ms of 1 ms (20,080 intervals), of the 10 ms job within 0.017 ms.

### Names, indices and the rest of the Linux behaviour (2026-11-15)

Both adapters attached, nothing transmitted for the naming checks.

| Check | Result |
|---|---|
| First start with no names kept | X6 `can0`–`can5`, now described as channels 1 to 6 across its three USB devices; classic PCAN-USB `can6`. Written to `~/Library/Application Support/darwincan/interfaces`, the X6 by serial number and channel, the classic adapter (serial `FFFFFFFF`, that is none) by USB port |
| Two more starts | The same names each time |
| `canctl link set can4 name rear`, then a restart | `rear` is X6 channel 5 after the restart; `can4` was free for a virtual interface meanwhile. Renamed back afterwards |
| Renaming to a name in use | Refused |
| `cansend can0` with `can0` down | "can0 is down" at once, exit status 1 |
| C self-test between `can0` and `can4` (connectors CAN1 and CAN5, 500 kbit/s / 2 Mbit/s), three runs | Pass. It now waits with `poll` before receiving, checks the interface index of received frames, and renames an interface |
| `candump any` with frames sent on `can0`, `can4` and `vcan0` | Each frame shown under the interface it was on, both ends of the cable included |

Not done: the adapters plugged in in another order, or the X6 alone, to see the names hold; that needs hands at the bench.

### J1939 between two X6 channels (2026-11-15)

`can0` and `can4` (connectors CAN1 and CAN5, cabled together) at 250 kbit/s, a J1939 socket at address 0x80 on one and at 0x90 on the other, so every message crossed the cable and each interface's stack talked to the other's. `candump -L can4` logged the bus.

| Check | Result |
|---|---|
| C self-test, which now includes J1939 (single frame, 1000 bytes by RTS/CTS, 3000 by the extended protocol, a broadcast, filters, priority, connect, refusals, rebinding), three runs and once more on the final build | Pass |
| Broadcast (BAM) of 700 bytes, 100 packets | Intact. Packets 51.5 to 51.9 ms apart, never under the 50 ms the standard requires |
| 1785 bytes by RTS/CTS | Intact. Data packets 0.53 to 0.57 ms apart, which is back to back at this bitrate |
| 20,000 and 200,000 bytes by the extended protocol | Intact. 31,429 data packets, median 0.55 ms apart; 200,000 bytes took 16 s |
| 200 messages of 9 to 1601 bytes by RTS/CTS, one after another | All intact |

The first build handed the adapter one frame at a time and waited for each to be reported sent. That was correct but slow: 1.85 ms a packet, 53 s for the 200,000 bytes. Four frames may now be with the adapter at once.

Not done: a real J1939 device or Linux's stack at the other end. Both ends here are this implementation, so a misreading of the standard that is the same at both ends would not show. Address claiming was run on the virtual bus only.

### Python between two X6 channels (2026-11-15)

`can0` and `can4` (connectors CAN1 and CAN5, cabled together) at 250 kbit/s, data 2 Mbit/s, one python-can bus on each, Python 3.14 with python-can 4.6.1.

| Check | Result |
|---|---|
| A bus without a `bitrate` on an interface that is down | Opens; `bus.state` reads `ERROR` and a send is refused at once |
| A bus with `bitrate=250000, listen_only=True` on `can0` while down | Brings it up listen-only (`canctl link show` agrees, `bus.state` reads `PASSIVE`); it stays up when the bus is closed; a bus asking for it not listen-only is refused |
| Buses with `fd=True, bitrate=250000, data_bitrate=2000000` on `can0` and `can4` while down | Bring both up (`ACTIVE`); a classic frame and a 64-byte CAN FD frame cross intact; both stay up when the buses are closed |
| On `can0` while up: a second bus with the same bitrate; with another bitrate, another data bitrate, or `listen_only=True` | The first opens; the others are refused, naming what the interface has and what was asked |
| Classic, 29-bit and 64-byte CAN FD frames with bit rate switching, `can0` to `can4`, and a frame back | Intact. The sender's own frames (`receive_own_messages`) come back marked, 20 µs after the receiver's on the two channels' clocks |
| `send_periodic` every 10 ms for 3 s, data changed half-way with `modify_data` | 301 frames, 9.87 to 10.14 ms apart, median 10.002; the new data from the change on; silent after `stop` |
| `IsoTpSocket`, 4000 bytes, STmin 1 ms asked by the receiver, and a short answer back | Intact in 1.41 s. 571 consecutive frames 2.30 to 2.64 ms apart, none under STmin |
| `bus.transmit_failures` on both | 0 |

Interfaces taken down afterwards.

### Kvaser USBcan Light 4xHS against the X6 (2026-11-15)

The first run of the `kvaser_hydra` backend. Kvaser channel 1 (`can7`) and X6 connector CAN1 (`can0`) on one bus at 250 kbit/s, on the same USB bus of the computer. The adapter cannot be listen-only, so it was started able to acknowledge. Driven from Python (`python/`), which also made this the Python package's first run against a second maker's adapter.

What the adapter says of itself: 4 channels, an 80 MHz controller, takes frames as extended commands, no CAN FD, no listen-only on any channel. macOS leaves it without a USB configuration (its device class is the vendor's own), so the backend selects one before claiming the interface.

| Check | Result |
|---|---|
| Listen-only, and CAN FD, asked of it | Refused in words; the interface stays down |
| Standard, 29-bit, remote, empty and all-ones-identifier frames, each way | Intact, with the kind and length they were sent with |
| The Kvaser's report of a frame sent against the X6's reception of it, and the reverse | Within 100 µs of each other in every case seen (−98 to +62 µs): two makers' clocks, each brought to the daemon's |
| 2000 frames back to back, each way | All received, in order; a report for each, in order; 1.00 s on the bus |
| Both directions at once at 40 % bus load each, 48,000 frames each way, 74 s | All received, in order; no send refused; nothing in the timing log |
| `send_periodic` every 10 ms from the Kvaser for 3 s | 301 frames 9.88 to 10.14 ms apart, median 10.00 |
| ISO-TP, 4000 bytes, STmin 1 ms: Kvaser to X6, and X6 to Kvaser | Intact both ways. Consecutive frames 1.52 to 1.76 ms apart from the Kvaser, 1.90 to 2.15 ms from the X6: a frame takes 0.5 ms, so the Kvaser's gaps are 0.02 to 0.26 ms over STmin and never under |
| One frame with nobody to acknowledge it | Error-passive reported once; the frame stays with the adapter |
| 100 frames with nobody to acknowledge | 31 more taken, then each refused after 500 ms with "adapter transmit buffer is full"; the program is told of each |
| Down and up after that, then the X6 joins | Nothing of what was waiting goes out; a new frame does, and is reported sent |
| Three frames waiting when the X6 joins | All three go out and are reported; error-active reported within the next state poll |
| Daemon killed (`kill -9`) with the channel up, a new daemon started | The new daemon takes the channel off the bus as it attaches: the X6, sending alone afterwards, goes error-passive unacknowledged |
| Channels 2 to 4, wired to nothing: up and down; channel 1 at 10 kbit/s and 1 Mbit/s | The adapter takes each bit timing and reads it back the same |

What was learnt about the adapter:

- **Timestamps come in steps of 102.4 µs.** It counts 80 MHz ticks, as the reference driver says, but the low 13 bits of every stamp are ones: each is rounded up to the end of its step. Gaps between received frames therefore read as 409.6 or 512 µs where the X6 reads 417 to 453. Rounded up means never early, which timing STmin from the stamps depends on.
- **It reports no error counters.** Its state events and its bus-error reports carry zeros for both, so an error frame from this adapter says "error counters tx 0 rx 0" and cannot say whether sending or receiving caused the state. Linux shows the same. (Each bus-error report has a field that rose by 8 with every unacknowledged attempt, up to 128, as the transmit counter does; the reference driver does not read it and neither does this one.)
- **Its bus-error reports say "error-active"** whatever the state. They are therefore allowed to worsen a channel's state and never to mend it; the way back comes from the state the daemon asks for twice a second while a channel is not error-active.
- **A flat-out sender with a higher-priority identifier starves it**, as it would any node: in a first soak the X6 sent identifier 100 without pause and the Kvaser's 101 waited until the X6 was done, 59 of its frames being refused after 500 ms each. That is arbitration, not a fault, and the refusals are the right answer.

Unplugged while up, the same day: `can7` and `can0` up with a frame every 10 ms each way, the Kvaser's USB cable pulled and put back into the same port after 12 s.

| Check | Result |
|---|---|
| The program sending and receiving on `can7` when the cable is pulled | Told at once: "the interface was removed (adapter unplugged)" |
| The daemon | Removes `can7` to `can10` and carries on; the X6, alone on the bus, has its frames refused after 500 ms each, as it should |
| Opening `can7` meanwhile | "no such interface" |
| Plugged in again | Attached under the same four names, down; the program brings `can7` up and traffic resumes both ways |

Sleep and wake, the same day, after the computer had been restarted: `can7` and `can0` up with a frame every 10 ms each way, and the Mac left to fall asleep by itself, twice (16.5 minutes and 7 minutes asleep).

| Check | Result |
|---|---|
| The daemon on waking | "woke from sleep, restoring interfaces", then `can0` and `can7` restored, both times |
| The program sending and receiving on both | Carried on without being restarted: a "restarted" error frame on each interface 0.24 s after the wake, and frames both ways from then on |
| What the Kvaser said first on waking | Error-passive, 0.2 s after the wake and 40 ms before it was restored: it was sending again before the X6 was back to acknowledge |
| Frames lost over the two sleeps | 20 of 46,000 from the Kvaser, those with the adapters when the Mac went to sleep and while they were being restored |

Sleeps asked for from the menu earlier that day lasted under a second each and tested nothing: with "Wake for network access" on, a packet on the wireless network woke the Mac at once, and after any wake macOS will not sleep again for 45 seconds. `caffeinate -s` turns a sleep that is asked for into a dimmed display with everything running. For a sleep to order, stop `caffeinate`, turn the wireless network off and wait a minute since the last wake.

Two of its channels on one bus, the same day: the X6 taken off, Kvaser channels 1 and 2 (`can7`, `can8`) cabled together at 250 kbit/s, and channels 3 and 4 up as well, wired to nothing.

| Check | Result |
|---|---|
| Standard, 29-bit, remote, empty and all-ones-identifier frames, each way | Intact; each arrives on the other channel only, marked with its interface, and is reported sent on its own |
| Channels 3 and 4 | Heard nothing throughout: the four channels are told apart |
| The sender's report of a frame against the receiver's stamp | The same to the microsecond in all but a few of 4000 frames, and one step (102.4 µs) apart in those: one clock for all channels |
| 2000 frames back to back, each way | All received and all reported, in order; none refused |
| `send_periodic` every 10 ms for 3 s | 301 frames, 9.83 to 10.14 ms apart |
| ISO-TP, 4000 bytes, STmin 1 ms, each way | Intact in 0.88 s. Consecutive frames read 1.43, 1.54 or 1.64 ms apart, which is as close as stamps in steps of 0.1 ms can say; against the X6's finer stamps the same transfer never read under 1.52 ms |
| Both directions at once at 40 % bus load each, 24,000 frames each way, 38 s | All received, in order; none refused; no error frames; nothing in the timing log |

Not done: bus-off (needs a short between CAN-H and CAN-L; the backend stops the channel on it, as the reference driver does, untested), channels 3 and 4 with traffic, bitrates other than 250 kbit/s with traffic.

### After the pre-release review (2026-11-15)

A review of the code changed the Kvaser backend (a command is now sent even when an earlier one failed, a failed start takes the channel off the bus again, the adapter's name is reduced to plain text before it is logged or stored) and the daemon's handling of clients. Kvaser channels 1 and 2 cabled together at 250 kbit/s, the X6 off the bus.

| Check | Result |
|---|---|
| The adapter after the daemon was restarted on the new build | Attached under the same four names |
| The two-channel run above, repeated in full | Pass: frames of every kind each way, 2000-frame bursts each way, cyclic sending, ISO-TP 4000 bytes each way (gaps 1.54 to 1.84 ms at STmin 1 ms), 24,000 frames each way at once, nothing refused, channels 3 and 4 silent |
| `canctl latency vcan0` | Median 39 µs |
| ISO-TP on the virtual bus at STmin 5 ms | Shortest gap 5.004 ms |

The X6 on the same build, later that day: connectors CAN1 and CAN2 (`can0`, `can1`) cabled together, the Kvaser not on that bus, 500 kbit/s with 2 Mbit/s data. The wiring was found by sending one frame from each connector in turn with the other five listen-only.

| Check | Result |
|---|---|
| Classic, 29-bit, remote, empty and CAN FD frames (12, 48 and 64 bytes, with and without bit rate switching), each way | Intact; each reported sent on its own interface, 2 µs after the other end's stamp |
| 5000 frames back to back each way, classic and 64-byte CAN FD frames alternating | All received and all reported, in order; none refused |
| `send_periodic` every 10 ms for 3 s | 301 frames, 9.91 to 10.13 ms apart |
| ISO-TP, 4000 bytes, STmin 1 ms: classic each way, and 64-byte CAN FD frames | Intact. Frame ends 2.11 to 2.59 ms apart in classic frames and 2.59 to 2.72 ms apart in CAN FD frames, none under STmin |
| Both directions at once, 30,000 frames each way, 40 s | All received, in order; none refused; no error frames; nothing in the timing log |

So CAN FD is still accepted on firmware 3.2.0, which the new refusal for firmware older than 2.0 must not affect. Not re-tested: the classic PCAN-USB, which was not attached. Its change (a place in the transmit window is given back for a frame it will never report) has a unit test but has not run on the adapter, and the refusal itself has not been seen on an adapter with old firmware.

### J1939 against Linux's stack (2026-11-16)

The first run with something other than darwinCAN at the far end. The Kvaser USBcan Light 4xHS was handed to a Linux virtual machine on the same Mac (Ubuntu 24.04 for ARM, kernel 6.8, can-utils 2023.03, the `kvaser_usb` driver), its channel 1 on the bench bus with X6 connector CAN1 (`can0`), terminated, nothing else on the bus, 250 kbit/s. Linux had address 0x90 and used `j1939cat`; darwinCAN had address 0x80 and a test program on the Rust library. Data was random and compared by checksum. The adapters went straight into the Mac. Timing through a virtual machine's USB is not representative, so only the broadcast gap was measured, on the X6's timestamps.

| Check | Result |
|---|---|
| darwinCAN to Linux: 8, 9, 100 and 1785 bytes (one frame and RTS/CTS), 1786 and 20,000 bytes (extended protocol) | All intact. Linux asked for 255 packets at a time; 20,000 bytes took 1.6 s. Linux reported one `RX DPO` for each of the 2 and 12 blocks |
| Linux to darwinCAN: the same six lengths | All intact. darwinCAN gave one notice at the start of each long message, and one for each offset of the extended ones (2 and 12) |
| Broadcast of 700 bytes, each way | Intact both ways. darwinCAN's packets were 51.8 to 52.0 ms apart, Linux's 49.8 to 56.0 ms |
| darwinCAN's socket closed 80 ms into sending 1785 bytes | darwinCAN sent an abort with reason 250 and the sender's role in the third byte (`FF FA FC`). Linux ended its receive session at once (`RX ABT`, protocol error), not 750 ms later by timeout: it accepts an abort that says who sent it |
| A packet lost on the way from Linux to darwinCAN: a forwarder between `can0` and a virtual interface left out packet 4 of 15 once, and the darwinCAN socket was on the virtual interface | darwinCAN asked again from packet 4 (`CTS` for 12 packets) 1.8 ms after packet 15. Linux sent packets 4 to 15 again, then at once an abort with reason 5. darwinCAN had the whole message by then, delivered it intact and acknowledged it. Linux's sender reported the message as failed (`TX ABT`, "State not recoverable") |

What this shows: the two stacks carry each other's messages by every transport there is, and an abort from darwinCAN is understood. It also shows one way they do not agree. Linux 6.8 sends an abort after it has answered a request for packets again, so a message that darwinCAN repaired arrives while its Linux sender is told that it failed. Watched on a virtual interface inside the virtual machine, with the other node played frame by frame, Linux's stack also did not ask again for a missing packet (it aborted with a timeout), sent packets again from wherever it was asked, ended a session on an acknowledgement that came early, and put FFh in the third byte of its aborts. darwinCAN follows SAE J1939-21 (May 2022) in each of these; see the decisions record.

Two faults were found. The daemon did not put on the bus what a J1939 stack still had to send when its last socket closed, so the abort for an unfinished message was never sent; in the first run Linux waited for its timeout. The stack now ends once those frames are out. And a first version of the forwarder passed frames round in a circle for about eight seconds (some 30,000 frames on the bench bus); both controllers stayed error-active. The forwarder now uses one socket for each interface, which does not receive what it sent.

Not done: more than one message at a time, address claiming between the two (NAMEs), Linux's J1939 tools against darwinCAN's (there are none yet), a clear-to-send during a transfer, and anything through the Kvaser backend, which was not in use because Linux had the adapter.

### The J1939 tools against can-utils (2026-11-16)

The same bench as the run before: X6 connector CAN1 (`can0` on the Mac) and the Kvaser's channel 1 in the Linux virtual machine (`can0` there, kernel 6.8, can-utils 2023.03), 250 kbit/s, nothing else on the bus. Each tool ran against the original of the same name or beside it.

| Check | Result |
|---|---|
| `j1939spy -P can0` on both machines, watching one frame, 5 and 20 bytes to an address and a 20-byte broadcast sent by `testj1939` from each side | The lines are the same, character for character, for every message both saw. Each side showed the other's 20-byte message to its own address and not its own to the other, as a promiscuous socket does on Linux |
| `testj1939 -B -r` on both machines for the same traffic | The same output on both, with the 20-byte messages in three lines |
| `j1939acd` on Linux with the lower NAME and one address, then darwinCAN's with a higher NAME and two | darwinCAN's asked, heard Linux's claim, and took the other address |
| The same with the roles exchanged | Linux's took the other address. Frames and output were the mirror image of the case above |
| darwinCAN's `j1939acd` holding 0x80 with the higher NAME; Linux's, with the lower NAME and 0x80 as its only address, claims it | darwinCAN's said `address collision for 0x80` and claimed 0x81 2.6 ms after Linux's claim |
| The same with the roles exchanged | Linux's moved to 0x81 2.3 ms after darwinCAN's claim, with the same output |
| `j1939cat` to `j1939cat -r`, 5, 100 and 2000 bytes, each way | All intact. The lines printed were the same, apart from what is listed below |

Differences seen: Linux's `j1939cat -r` gives the bytes received so far in its second `RX DPO` line (`send=0001785`) where darwinCAN's printed 0, which was corrected after the run and not run again on the bench; and the transport frames of darwinCAN carried priority 7 where Linux's carry the message's, which `j1939spy` showed as `!7` and `!6`. That was changed after the run as well: they now carry the message's priority (see the decisions record). A second run on the same bench confirmed it: 20 bytes to an address at priority 3 and a 20-byte broadcast at priority 2, sent with `testj1939` from each side, were shown as `!3` and `!2` by `j1939spy` on both machines, both sides' frames had the identifiers `0CEC`, `0CEB`, `08EC` and `08EB`, and each side answered the other at the priority its frames came with.

Before this run, the originals were watched on a virtual interface inside the virtual machine to learn what they print and send, and darwinCAN's tools were run through the same cases on a virtual interface of their own. `j1939acd` sent the same frames in the same order in a case of fifteen frames (claim, three contentions, two requests, no address left).

Not done: `j1939sr` against the original, which could be made to run in one case only on Linux; a program bound to a NAME exchanging long messages while `j1939acd` moves the address; and `j1939acd` on a bus where two claims collide.

### J1939 from Python against Linux (2026-11-16)

The same bench and settings as the two runs before. A Python program with a `J1939Socket` at address 0x80 on the Mac; `testj1939` and `j1939spy` of can-utils at 0x90 on Linux.

| Check | Result |
|---|---|
| Python sends 5 bytes, 100 bytes at priority 3, 2000 bytes, and a 20-byte broadcast at priority 2 | Linux's `testj1939 -r` printed all four, and `j1939spy` showed them with the lengths and the priorities given (`!6`, `!3`, `!6`, `!2`). Python got a notice that each was out, with all its bytes acknowledged |
| Linux sends 5 bytes, 100 bytes at priority 3 and a 20-byte broadcast at priority 2 | Python received all three with the data as sent and the priorities 6, 3 and 2 |

Not done: 2000 bytes from Linux, because can-utils' `testj1939` sends at most 128; that length crossed in both directions with `j1939cat` in the run before.

### `candump -x` and `isotpdump` against can-utils (2026-11-16)

The same bench and settings as the runs before, with `candump -x can0` and `isotpdump -s 7E0 -d 7E8 -a can0` running on both machines at once.

| Check | Result |
|---|---|
| Two frames sent from the Mac and two from Linux (one of them a remote frame with a 29-bit identifier) | Each `candump -x` showed its own machine's frames as `TX` and the other's as `RX`; apart from that the lines were the same |
| A 20-byte ISO-TP message from Linux's `isotpsend` to darwinCAN's `isotprecv` | Received. Both `isotpdump` printed the same four lines: the first frame, darwinCAN's flow control and two consecutive frames |

That ISO-TP message is the first to cross between Linux's ISO-TP and darwinCAN's; nothing else of ISO-TP has been run against Linux.

### The limit on message data, and the daemon's allocator (2026-11-16)

Measured on virtual interfaces with the release build, and then run on the bench against Linux (X6 connector CAN1, the Kvaser's channel 1 in the virtual machine, 250 kbit/s) to see that nothing else had changed.

| Check | Result |
|---|---|
| Daemon memory after three J1939 messages of 117,440,505 bytes that nobody received | 7 MB after each, and about 120 MB while one was held. Before the change: 234, 346 and 458 MB, which stayed |
| A J1939 message of 20 MB delivered on a virtual interface | Intact, in 0.64 s; 7 MB afterwards |
| 500 ISO-TP messages of 4095 bytes one after another, as a flash sends its blocks | All received, in 1.4 s |
| ISO-TP, 4000 bytes, receiver asking for STmin 5 ms | Gaps of 5.003 to 5.037 ms, none under |
| Delivery latency on a virtual interface, six runs before and six after, twice | Medians 84 to 132 µs before and 21 to 92 µs after; 99th percentiles 156 to 217 µs before and 62 to 167 µs after. One run after the change had a single sample of 13 ms; single samples of 1 to 2 ms occurred before the change too |
| On the bench: 5000 counted frames from the Mac to Linux and 5000 back | All received each way; none repeated or missing among those Linux received |
| On the bench: 200,000 bytes by J1939 each way | Intact both ways; 16.1 s from the Mac |
| On the bench: a 4095-byte ISO-TP message from the Mac to Linux's `isotprecv` | Intact |
| Daemon memory over the bench run | 8 MB at the start, 9 MB at the end |

Not done: the limits themselves on real adapters, which behave no differently there; a message near the 128 MiB limits on a bus, which would take hours; and the classic PCAN-USB and the Kvaser backend, which were not in use.

## Bugs found on hardware and fixed

| Symptom | Cause | Fixed |
|---|---|---|
| A reported error-passive up to a second late | The adapter sends no status record when the counters cross 128; the state is now also derived from its counter reports | 2026-11-12 |
| X6 frames stamped minutes off | The X6 restarts its clock when calibration messages are enabled, and they were enabled per channel. Now enabled with the first channel opened and disabled with the last, as the Linux driver does | 2026-11-12 |
| First frames after an idle period stamped late and identical | Frames sharing a USB transfer each refined the clock estimate in turn. Both backends now examine a whole transfer before stamping it | 2026-11-12 |
| Interfaces dead after the Mac slept, still shown as up | Adapters lose their bus settings in sleep. The daemon now restores them on wake | 2026-11-12 |
| X6 receive timestamps frozen after a wake when two channels of one X6 device were up: frames stamped in clumps 100 ms apart | The adapter forgets that calibration messages were on, and the restore switched them on only if the device's other channel was closed. `Backend::restore` now lets the backend set up such state again | 2026-11-13 |
| X6 still acknowledging after the daemon was killed, shown as down by the next daemon | Nothing took the channels off the bus at attach. They are now reset first, as the classic PCAN-USB backend already did | 2026-11-13 |
| Frames accepted and echoed as sent on an interface that was bus-off | Nothing recorded the bus-off state. The bus now refuses frames from the bus-off error frame until the interface is restarted or the controller reports error-active | 2026-11-13 |
| One frame a second stamped up to a millisecond late by the X6, sometimes later than the frame after it | The calibration message's counter starts when the messages are switched on; frames are stamped by a clock that runs from power-up (4 s against 5667 s in one transfer). Both were fed to the clock estimate, so every calibration message threw away what had been learnt and the frames sharing its transfer were stamped by their arrival. Calibration messages are no longer used for the estimate. This also corrects the 2026-11-12 entry above: enabling them does not restart the clock that stamps frames | 2026-11-13 |
| Sending stalled for 5–20 ms a few times in ten minutes, once for 143 ms, in both directions at once | The daemon's threads that take frames from clients and adapters were of ordinary (user-interactive) priority, and macOS holds such threads up now and then; its real-time threads were never late. First put down to waiting for USB transfers to be reported, wrongly: the stalls remained when that wait was gone. Those threads are now in the real-time class, which ended the stalls of 5 to 20 ms. A rarer pause of tens of milliseconds remains, see Open | 2026-11-13 |
| Frames shown as sent, and STmin timed, from the hand-over to the adapter; the classic PCAN-USB's ISO-TP gaps as short as 4.37 ms at STmin 5 ms | Transmit confirmation: both backends ask the adapter to report each frame sent | 2026-11-13 (X6), 2026-11-13 (classic) |
| Adapters' messages reached the daemon 3 to 18 ms late about every 40 seconds (up to 82 ms through a dock), both adapters together; now and then the bus paused with it (46 ms, 53 ms), and once the X6 overran and lost four frames | The USB library hands every adapter's transfers to the daemon from one thread of its own, unnamed and of ordinary priority, which macOS holds up now and then. The daemon puts that thread in the real-time class, finding it at first as the thread that appears when it begins to watch for adapters (2026-11-14) and, since the library's version 0.2 names it, by name; it says so at start-up if it finds none. Shown through the dock: 31 affected seconds in 12 minutes before, one after | 2026-11-14 |
| ISO-TP frames closer together than STmin | STmin was timed from handing a frame to the adapter. The frame's time on the wire is now added | 2026-11-12 |
| A flush, or any other request the daemon acknowledges, failed with "Resource temporarily unavailable" when the program had last received with a short timeout and a frame ahead of the request was waiting for room in an adapter (seen from Python with the Kvaser alone on the bus) | The socket's read timeout, set by the last receive, also cut the wait for an acknowledgement short. An acknowledgement is now waited for until it comes | With the Kvaser backend |
| The X6's connectors changed interface names when it was plugged in again | Its three USB devices were attached in the order the system found them, which varies. They are now gathered and attached in the order of their ports on the X6's hub | 2026-11-14 |
| A J1939 abort for a message whose socket closed never reached the bus when that socket was the last on its interface; the receiver waited for its timeout | The interface's stack ended with its last socket, before the frames it still had to send were handed over. It now ends once they are out | 2026-11-16 |

## Open

Needs the current wiring:

- **Bus-off on the `nusb` 0.2 build.** Short CAN-H to CAN-L with both adapters sending, check the refusals and the recovery by down and up.

- **A second "bus off" from the classic PCAN-USB when it is restarted** after a bus-off, just before "error active". Seen in one of two restarts. Harmless, since "error active" follows, but wrong.
- **Bus-off counters on the X6** read tx 127 in its bus-off report, where the classic PCAN-USB reads 248.
- **Automatic bus-off recovery** (`restart-ms`) does not exist; recovery is down and up by hand.
- **J1939 against a real ECU.** It has run against Linux's stack and tools (2026-11-16) but not against a device. Several messages at once have not been run against Linux either.
- **Kept names with the adapters plugged in in another order.** The names are kept since 2026-11-15 and held over three daemon starts (see "Names, indices and the rest of the Linux behaviour"), but the adapters have not been unplugged and plugged in the other way round since, nor the classic PCAN-USB moved to another port.
- **Unplug during sleep, and sleep with an ISO-TP transfer in flight.**
- **Mid-flood timestamp glitch on the X6.** Seen once on 2026-11-12: five frames in a back-to-back flood got the same timestamp. Probably the calibration message fault fixed on 2026-11-13 (see the bugs table); not seen again, but not reproduced on purpose either.
- **Longer and harder runs.** The longest is 30 minutes at half bus load. Nothing has run for hours or at full load from both sides.

Needs two X6 channels cabled together, with termination:

- **CAN FD beyond the short bench cable**: sample points, long cables, error states with a partner present.
- **The 500 kbit/s receive-counter observation**: confirm that a terminated channel raises the transmit counter instead.

Needs other hardware or work first:

- **A known-good reference node** (the Kvaser on a Linux or Windows machine, or any CAN device): so far each backend has only been checked against another written here. Since 2026-11-15 that includes a second maker's controller (the Kvaser), which rules out a fault shared by the PEAK adapters but not one in how this project reads a bus.
- **Timestamps about 0.9 ms late on a quiet bus**, stepping back at each of the X6's rare fast messages. The estimate of an adapter's clock is only as good as the fastest message seen lately; see "The adapter clock's rate". It now holds on to a fast message for minutes once an interface has been up for minutes, not for the first minute. Doing better needs a way to know the rate of the adapter's clock that traffic cannot fool, or to provoke a quick delivery.
- **A sleep of under half a second asleep went unnoticed** (2026-11-14, an idle sleep ended by a network packet at once): the X6 lost its bus settings and stopped acknowledging, and the daemon restored nothing. The threshold is now 10 ms. Not seen again, and not reproduced on purpose: such a sleep cannot be made to order.
- **ISO-TP gaps 0.2 to 1.2 ms longer than STmin on the X6.** The price of never undercutting, and probably the adapter's own 1 ms steps; see "The adapter clock's rate".
- **STmin under a millisecond gives about 1.2 ms on the X6**, because the next frame waits for the adapter's report of the one before.
- **Bus-off with frames pending** on the X6: the daemon forgets them (unit test); not tried on the bench since the change.
- **Other `peak_fd` models** (PCAN-USB FD, Pro FD, Chip): untested.
- **Kvaser, on the adapter tested**: bus-off; channels 3 and 4 with traffic; bitrates other than 250 kbit/s with traffic.
- **Other Kvaser adapters**: the Leaf v3 and U100 (hydra, untested); the Leaf Light v2 (no backend).

Never tested at all: bus load near 100 % from several nodes, error frames caused by a real fault (short, wrong bitrate on one node), more than two nodes, remote adapters on a hub under USB contention.
