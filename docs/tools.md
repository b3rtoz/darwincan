# The daemon and the command-line tools

This page explains how to run `darwincand`, manage interfaces and use the tools. If you know can-utils and `ip link` from Linux, most of this is already familiar: the tools take the same command lines and print the same output.

## The daemon

`darwincand` owns the CAN adapters and shares them between programs. Every tool and every program built on darwinCAN talks to it over a Unix socket; nothing works without it.

```sh
darwincand &          # run it by hand; creates the virtual interface vcan0
```

To have the daemon running from the moment you log in, install it as a launch agent:

```sh
sudo make install     # the daemon, tools and C library into /usr/local
darwincand install    # add daemon options here, for example --vcan vcan1
```

This registers the agent `org.darwincan.darwincand` for your user account. launchd creates the socket and hands it to the daemon. As a result, the tools work from login onwards with nothing to start by hand, a program that connects while the daemon is still starting waits instead of failing, and the daemon is started again if it ever exits. The daemon's messages go to `~/Library/Logs/darwincand.log`.

Run `darwincand install` again if the `darwincand` executable moves to a different path, for example after building or installing somewhere else. `darwincand uninstall` removes the agent, and `darwincand install --print` shows the property list without installing it.

| Option | Meaning |
|---|---|
| `--socket <path>` | Listen on this Unix socket. The default is `$DARWINCAN_SOCKET` if it is set, and otherwise `~/Library/Application Support/darwincan/darwincand.sock` |
| `--vcan <name>` | Create a virtual interface. The option may be repeated. The default is `vcan0` |
| `--no-vcan` | Do not create `vcan0` |
| `--no-hardware` | Do not look for USB adapters |
| `--names <file>` | The file in which the names of hardware interfaces are kept |
| `--timing-log <ms>` | Log anything that holds a frame up for longer than this, to help find timing faults |

The daemon and the tools run under your own user account, without `sudo`. The daemon and everything that talks to it must come from the same build: they check each other's protocol version and report it when the versions differ.

## Interfaces

An interface is a name inside the daemon, not a macOS network interface, so `ifconfig` does not show it. `canctl` manages interfaces and takes the same wording as `ip link`.

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
| `sample-point <0.xxx>`, `dsample-point <0.xxx>` | Sample points. If they are not given, they are chosen according to the bitrate |
| `listen-only on\|off` | Never transmit or acknowledge |
| `fd on\|off` | `on` needs `dbitrate` to be given as well; `off` removes the data bitrate |

Settings are remembered while an interface is down, so the two-step form (`link set can0 type can bitrate 500000`, then `link set can0 up`) works too. A hardware interface starts down when its adapter is attached. A virtual interface is up as soon as it exists and carries CAN FD frames without further settings.

`canctl` also has short forms of its own: `canctl up can0 bitrate 500000`, `canctl down can0`, `canctl add-vcan vcan1`.

[hardware.md](hardware.md) describes which adapters are supported and how interface names are assigned and kept.

## The tools

| Tool | What it does |
|---|---|
| `candump` | Prints or logs the frames on one or more interfaces, with filters and timestamps |
| `cansend` | Sends one frame: `123#DEADBEEF`, `123#R` (remote), `123##1...` (CAN FD) |
| `cangen` | Generates random or counting test traffic at a set interval |
| `canplayer` | Replays a `candump -l` log with its original timing |
| `isotpsend` | Sends one ISO-TP message, read as hex bytes from standard input |
| `isotprecv` | Receives ISO-TP messages and prints them as hex bytes |
| `j1939acd` | Claims a J1939 address for a NAME and defends it, so that programs can use the NAME |
| `j1939spy` | Prints the J1939 messages on an interface, one to a line |
| `j1939cat` | Sends a file as J1939 messages, or receives one message and writes it out |
| `j1939sr` | Sends what it reads from standard input as J1939 messages, and writes out what it receives |
| `testj1939` | Sends and receives J1939 messages to try things out |
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

The ISO-TP tools take their addresses as `-s <transmit id> -d <receive id>`, and they take `-p` (padding), `-b` (block size), `-m` (STmin) and `-L` (CAN FD link layer) as on Linux.

## The J1939 tools

```sh
j1939spy -P can0                                # every J1939 message on can0
j1939spy -P -ta can0:80,FEF1                    # with time; only PGN FEF1 from address 80

testj1939 -B -r can0:0x90 &                     # receive as address 0x90, and print
testj1939 -s20 can0:0x80 :0x90,0x12300          # send 20 bytes from 0x80 to 0x90

j1939cat can0:0x90 -r > received.bin &          # receive one message into a file
j1939cat -i some.bin can0:0x80 :0x90,0x12300    # send a file, of any length up to 117 MB

j1939acd -r 0x80-0x8F -c /tmp/1122334455667788.jacd 1122334455667788 can0 &
```

An address is written in one of two ways, as in can-utils. `testj1939` and `j1939cat` take `[IFACE][:[SA][,[PGN][,NAME]]]` with numbers as C writes them (`0x80`, `128`). `j1939spy` and `j1939sr` take `[IFACE:][NAME|SA][,PGN]` in hex without a prefix, where a number of up to two digits is an address and a longer one is a NAME.

`j1939acd` is the address claiming daemon. It asks which addresses are taken, waits 1.25 s for the answers, claims the first free address in its range (or the one kept in its cache file, or the one given with `-a`), answers requests for it, and moves to another address when a NAME with priority claims the one it has. A program that opens its J1939 socket bound to the NAME alone then sends from, and receives for, whatever address `j1939acd` holds. When it is told to end (Ctrl-C or `kill`), it gives the address up on the bus and writes it to the cache file for the next run. If no address in its range is left, it says so on the bus and ends.

`j1939spy` shows, for each message, the interface and sender, the parameter group, the destination (`-` for everyone), the priority, the length and the data:

```
 can0:80,0fef1 - !6 [8] 01020304 05060708
 can0:1122334455667788,0ef00 90 !6 [2] aabb
 80,0feb0 - !7 [20] 01234567 89abcdef 01234567 89abcdef 01234567
```

A sender is shown by its NAME once it has claimed its address. A message that came in several frames is shown without the interface, as on Linux.

## Coming from Linux

| Linux | Here |
|---|---|
| `candump`, `cansend`, `cangen`, `canplayer`, `isotpsend`, `isotprecv` | The same names and options, and the same syntax for frames and filters |
| `j1939acd`, `j1939spy`, `j1939cat`, `j1939sr`, `testj1939` | The same names, options, address syntax and output |
| `ip link ...` | `canctl link ...`: replace `ip` with `canctl` |
| `sudo modprobe vcan` | This step is not needed |
| The kernel is always there | Run `darwincand install` once, so that the daemon is always there too |

The following differences are worth knowing:

- **No `sudo` is needed.** The daemon and the tools run under your own user account.
- **`link set vcan0 up` is accepted and does nothing**: a virtual interface is always up.
- **`link set can0 up type can bitrate ...` on an interface that is already up reconfigures it.** Changing settings without `up` requires the interface to be down first, as on Linux.
- **`candump -e` receives all error frames by itself.** On Linux it also needs a `#<mask>` filter.
- **`canplayer` sends each frame at its logged time** instead of polling every `-g` milliseconds, so `-g` has no effect.
- **A frame you send on a hardware interface is shown once it has been on the bus**, with the adapter's timestamp. A frame that nobody acknowledges is not shown.
- **The J1939 tools need an interface to be named.** On Linux some of them fall back to `can0` or to every interface; here only `j1939acd` falls back to `can0`.
- **`j1939spy` without `-P` shows nothing,** as on Linux: its socket has no address of its own. A parameter group given without a sender (`can0:,FEF1`) filters here; on Linux it was seen not to.
- **`-v` on the J1939 tools does not print Linux's system calls.** `testj1939 -v` prints the calls of darwinCAN's C API in their place; `j1939acd -v` prints its other lines (`claimed 0x80`, `address collision for 0x80`) and leaves the calls out. `j1939acd -p` is accepted and, as in can-utils 2023.03, changes nothing that is printed.
- **`testj1939 -p=3` sets priority 3.** In can-utils 2023.03 the form with `=` sets priority 0; `-p3` works in both.
- **`j1939cat` reports less of a message's progress.** It prints `TX ACK` when a message is out and `TX ABT` when it was given up, with Linux's error number and text, and exits with that number. The `TX ENQ` lines Linux prints while a long message is under way are not printed. With `-s`, the input is sent in pieces of that size, each as one message.
- **`j1939sr` sends with a parameter group.** It takes the one in DEST or, failing that, the one in SOURCE, and refuses to send without one. In can-utils 2023.03 it sends with no valid parameter group.
- **Address claims that collide on the bus are not spaced out.** SAE J1939-81 asks for a random delay before a claim is sent again after a bus error. The adapters repeat a frame by themselves, so `j1939acd` cannot do that.
- **The following are not available:** `candump -x`, `-c`, `-i`, `-S`, `-d`, `-D` and `-r`; `cangen -a`, `-t`, `-X` and `-8`; `canplayer -i`; bit timing by segments (`tq`, `prop-seg` and so on), `restart-ms`, `loopback on`, `one-shot on` and `triple-sampling on`. Each is refused with a message and is never silently ignored.

## Troubleshooting

| What you see | What to do |
|---|---|
| `cannot reach darwincand (...); is the daemon running?` | Start it with `darwincand &`, or install the launch agent. If you set `DARWINCAN_SOCKET`, the daemon and the tools need the same value |
| The daemon fails to bind its socket | macOS limits Unix socket paths to about 100 bytes; use a shorter `DARWINCAN_SOCKET` |
| An adapter is plugged in but no interface appears | Another program is probably holding the adapter (PCAN-View, a MacCAN-based tool, Kvaser CANlib). Quit that program and plug the adapter in again. The daemon's messages say what it saw |
| A tool says the daemon and the program come from different versions | Rebuild both from the same source and restart the daemon. If the daemon runs as a launch agent, run `darwincand install` again |
| `can0 is down` | Bring the interface up with `canctl link set can0 up type can bitrate ...` |
| `can0 is bus-off; take it down and up again` | A fault on the bus (usually a short) drove the controller off the bus. Run `canctl link set can0 down`, then `up`. `candump -e` shows the error states that led there |
| `adapter transmit buffer is full` | Nothing on the bus acknowledges your frames (wrong bitrate, no other node, no termination), or other traffic keeps winning the bus |
| An interface is silent after the Mac slept | The daemon restores interfaces after the Mac wakes, and logs that it has done so. If an interface stays silent, take it down and up, or keep the Mac awake with `caffeinate -i` |
| `candump` misses frames under heavy load | A program that reads too slowly loses frames rather than delay the bus. Log with `candump -l` instead of printing to a terminal |
| Which adapter is `can0`? | `canctl list` names the adapter and channel behind each interface |
