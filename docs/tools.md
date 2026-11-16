# The daemon and the command-line tools

How to run `darwincand`, manage interfaces, and use the tools. If you know can-utils and `ip link` from Linux, most of this is already familiar: the tools take the same command lines and print the same output.

## The daemon

`darwincand` owns the CAN adapters and shares them between programs. Every tool and every program built on darwinCAN talks to it over a Unix socket; nothing works without it.

```sh
darwincand &          # run it by hand; creates the virtual interface vcan0
```

To have it running from login on, install it as a launch agent:

```sh
sudo make install     # the daemon, tools and C library into /usr/local
darwincand install    # add daemon options here, for example --vcan vcan1
```

This registers the agent `org.darwincan.darwincand` for your user. launchd creates the socket and hands it to the daemon, so the tools work from login on with nothing to start, a program that connects while the daemon is still starting waits instead of failing, and the daemon is started again if it ever exits. Its messages go to `~/Library/Logs/darwincand.log`.

Run `darwincand install` again after rebuilding into a different path. `darwincand uninstall` removes the agent, and `darwincand install --print` shows the property list without installing it.

| Option | Meaning |
|---|---|
| `--socket <path>` | Listen on this Unix socket. Default: `$DARWINCAN_SOCKET`, else `~/Library/Application Support/darwincan/darwincand.sock` |
| `--vcan <name>` | Create a virtual interface; may be repeated. Default: `vcan0` |
| `--no-vcan` | Do not create `vcan0` |
| `--no-hardware` | Do not look for USB adapters |
| `--names <file>` | Where the names of hardware interfaces are kept |
| `--timing-log <ms>` | Log whatever holds a frame up for longer than this, for finding timing faults |

The daemon and the tools run as you, without `sudo`. The daemon and everything that talks to it must come from the same build: they check each other's protocol version and say so when it differs.

## Interfaces

An interface is a name inside the daemon, not a macOS network interface: `ifconfig` does not show it. `canctl` manages them and takes the wording of `ip link`.

```sh
canctl link show                         # or: canctl list
canctl -details link show can0

canctl link add dev vcan1 type vcan      # a virtual interface
canctl link delete vcan1

canctl link set can0 up type can bitrate 500000
canctl link set can0 up type can bitrate 500000 dbitrate 2000000 fd on
canctl link set can0 up type can bitrate 500000 listen-only on
canctl link set can0 down
canctl link set can0 name chassis        # rename; the adapter keeps the name
```

| Setting | Meaning |
|---|---|
| `bitrate <bps>` | Nominal bitrate |
| `dbitrate <bps>` | CAN FD data bitrate; enables CAN FD |
| `sample-point <0.xxx>`, `dsample-point <0.xxx>` | Sample points; chosen by bitrate if not given |
| `listen-only on\|off` | Never transmit or acknowledge |
| `fd on\|off` | `on` needs `dbitrate`; `off` drops it |

Settings are remembered while an interface is down, so the two-step form (`link set can0 type can bitrate 500000`, then `link set can0 up`) works too. A hardware interface starts down when its adapter is attached. A virtual interface is up as soon as it exists and carries CAN FD frames without further settings.

`canctl` also has short forms of its own: `canctl up can0 bitrate 500000`, `canctl down can0`, `canctl add-vcan vcan1`.

Which adapters are supported, and how interface names are assigned and kept, is in [hardware.md](hardware.md).

## The tools

| Tool | What it does |
|---|---|
| `candump` | Prints or logs the frames on one or more interfaces, with filters and timestamps |
| `cansend` | Sends one frame: `123#DEADBEEF`, `123#R` (remote), `123##1...` (CAN FD) |
| `cangen` | Generates random or counting test traffic at a set interval |
| `canplayer` | Replays a `candump -l` log with its original timing |
| `isotpsend` | Sends one ISO-TP message, read as hex bytes from standard input |
| `isotprecv` | Receives ISO-TP messages and prints them as hex bytes |
| `canctl` | Manages interfaces, and measures delivery latency (`canctl latency vcan0`) |

Every tool prints its options with `-h`.

```sh
candump vcan0                                   # everything on vcan0
candump -ta can0,123:7FF vcan0                  # absolute timestamps, a filter, two interfaces
candump -e can0                                 # with error frames
candump -l any                                  # log every interface to candump-<date>.log

cansend vcan0 123#DEADBEEF
cansend vcan0 123##1112233445566778899AABBCC    # CAN FD with bit rate switching
cangen vcan0 -g 10 -n 100                       # 100 random frames, 10 ms apart
canplayer -I candump-2026-01-31_120000.log vcan0=can0

isotprecv -s 7E8 -d 7E0 vcan0 &
echo 22 F1 90 | isotpsend -s 7E0 -d 7E8 vcan0
```

The ISO-TP tools take the addresses as `-s <transmit id> -d <receive id>`, and `-p` (padding), `-b` (block size), `-m` (STmin) and `-L` (CAN FD link layer) as on Linux.

## Coming from Linux

| Linux | Here |
|---|---|
| `candump`, `cansend`, `cangen`, `canplayer`, `isotpsend`, `isotprecv` | The same names, options, and frame and filter syntax |
| `ip link ...` | `canctl link ...`: replace `ip` with `canctl` |
| `sudo modprobe vcan` | Not needed |
| The kernel is always there | `darwincand install`, once |

Differences worth knowing:

- **No `sudo`.** The daemon and the tools run as you.
- **`link set vcan0 up` is accepted and does nothing**: a virtual interface is always up.
- **`link set can0 up type can bitrate ...` on an interface that is already up reconfigures it.** Changing settings without `up` needs the interface down first, as on Linux.
- **`candump -e` receives all error frames by itself.** Linux also needs a `#<mask>` filter.
- **`canplayer` sends each frame at its logged time** instead of polling every `-g` milliseconds, so `-g` has no effect.
- **A frame you send on a hardware interface is shown once it was on the bus**, with the adapter's timestamp; a frame nobody acknowledges is not shown.
- **Not available:** `candump -x`, `-c`, `-i`, `-S`, `-d`, `-D` and `-r`; `cangen -a`, `-t`, `-X` and `-8`; `canplayer -i`; bit timing by segments (`tq`, `prop-seg` and so on), `restart-ms`, `loopback on`, `one-shot on` and `triple-sampling on`. Each is refused with a message, never silently ignored.

## Troubleshooting

| What you see | What to do |
|---|---|
| `cannot reach darwincand (...); is the daemon running?` | Start it with `darwincand &`, or install the launch agent. If you set `DARWINCAN_SOCKET`, the daemon and the tools need the same value |
| The daemon fails to bind its socket | macOS limits Unix socket paths to about 100 bytes; use a shorter `DARWINCAN_SOCKET` |
| An adapter is plugged in but no interface appears | Another program is probably holding it (PCAN-View, a MacCAN-based tool, Kvaser CANlib); quit it and plug the adapter in again. The daemon's messages say what it saw |
| A tool says the daemon and the program come from different versions | Rebuild both from the same source and restart the daemon; run `darwincand install` again if it is a launch agent |
| `can0 is down` | `canctl link set can0 up type can bitrate ...` |
| `can0 is bus-off; take it down and up again` | A fault on the bus (usually a short) drove the controller off it. `canctl link set can0 down`, then `up`. `candump -e` shows the error states that led there |
| `adapter transmit buffer is full` | Nothing on the bus acknowledges your frames (wrong bitrate, no other node, no termination), or other traffic keeps winning the bus |
| An interface is silent after the Mac slept | The daemon restores interfaces after a wake and logs it. If one stays silent, take it down and up, or keep the Mac awake with `caffeinate -i` |
| `candump` misses frames under heavy load | A program that reads too slowly loses frames rather than delay the bus. Log with `candump -l` instead of printing to a terminal |
| Which adapter is `can0`? | `canctl list` names the adapter and channel behind each interface |
