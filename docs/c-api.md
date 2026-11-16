# Using darwinCAN from C

How to build a C or C++ program on darwinCAN, and how to bring a Linux SocketCAN program over to it. The reference for each function is the header itself, [`darwincan.h`](../crates/darwincan/include/darwincan.h); this guide covers what the header cannot: installing, porting, and the places where behaviour differs from Linux. The broadcast manager has a header of its own, [`darwincan_bcm.h`](../crates/darwincan/include/darwincan_bcm.h).

What is the same in every language (how a program reaches the bus, what a timestamp and a send mean, what happens when an adapter is unplugged) is in [application-guide.md](application-guide.md). For Python, see [python.md](python.md).

Four programs in [`crates/darwincan/c`](../crates/darwincan/c) are worth reading beside it. The tests build and run all four.

| File | Shows |
|---|---|
| [`dump.c`](../crates/darwincan/c/dump.c) | A receive loop with a filter, error frames, timestamps and a clean stop on Ctrl-C |
| [`uds_read.c`](../crates/darwincan/c/uds_read.c) | A diagnostic request and response over ISO-TP |
| [`cyclic.c`](../crates/darwincan/c/cyclic.c) | A frame sent every few milliseconds by the broadcast manager, with its data changed as it goes |
| [`selftest.c`](../crates/darwincan/c/selftest.c) | Every function at least once, including threads, signals and `poll` |

## Install

You need Rust 1.85 or later (`brew install rustup`, then put `/opt/homebrew/opt/rustup/bin` on your `PATH`) and the Xcode command line tools.

```sh
make                 # cargo build --release
sudo make install    # into /usr/local
```

That installs the daemon and the tools into `/usr/local/bin`, `darwincan.h`, `darwincan_bcm.h` and `darwincan_j1939.h` into `/usr/local/include`, and `libdarwincan.dylib`, `libdarwincan.a` and `pkgconfig/darwincan.pc` into `/usr/local/lib`. The compiler looks in `/usr/local` by itself, so a program needs only:

```sh
cc prog.c -ldarwincan -o prog
```

To install somewhere that needs no `sudo`, give a prefix and let pkg-config supply the flags:

```sh
make install PREFIX=$HOME/.local
export PKG_CONFIG_PATH=$HOME/.local/lib/pkgconfig
cc prog.c $(pkg-config --cflags --libs darwincan) -o prog
```

`sudo make uninstall` (with the same `PREFIX`) removes it all again.

Without installing, point the compiler at the build directory. Naming the static library links it into the program, which then needs nothing but the daemon at run time:

```sh
cc -I crates/darwincan/include prog.c target/release/libdarwincan.a -o prog
```

The library has no dependencies beyond the system's. It is licensed MIT or Apache-2.0, so a program that links it may carry any licence; the daemon is GPL-2.0, and programs only talk to it over a socket.

### The daemon

Every call goes to `darwincand`, which owns the adapters. Start it by hand (`darwincand &`) or once and for all with `darwincand install`, which makes it start at login. It listens on a socket under `~/Library/Application Support/darwincan`, or wherever `DARWINCAN_SOCKET` points; the library looks in the same place, and the `_at` functions take a path for the rare program that talks to a particular daemon.

Daemon and library must speak the same version of the protocol between them. After updating, run `make install` and restart the daemon (`darwincand install` again if it starts at login). A mismatch is not silent: the open call fails with `DCAN_ERROR_VERSION_MISMATCH` and a message that says which side is older.

A hardware interface has to be on the bus before frames flow: `canctl link set can0 up type can bitrate 500000`, exactly as with `ip link` on Linux, or `dcan_interface_up` from a program. `vcan0` exists from the start and needs nothing.

## A first program

```c
#include <stdio.h>
#include <darwincan.h>

int main(void)
{
	struct can_frame tx = { .can_id = 0x123, .len = 2, .data = { 0xDE, 0xAD } };
	struct canfd_frame rx;
	struct dcan_rx_info info;
	dcan_raw *can = dcan_raw_open("vcan0");

	if (can == NULL) {
		fprintf(stderr, "%s\n", dcan_last_error());
		return 1;
	}
	dcan_raw_send(can, &tx);
	for (;;) {
		int n = dcan_raw_recv(can, &rx, &info, 1000);
		if (n < 0) {
			fprintf(stderr, "%s\n", dcan_last_error());
			break;
		}
		if (n == 0)
			continue;       /* nothing for a second */
		printf("%X [%d] at %llu ns\n", rx.can_id, rx.len,
		       (unsigned long long)info.timestamp_ns);
	}
	dcan_raw_close(can);
	return 0;
}
```

Run it and type `cansend vcan0 321#11223344` in another terminal.

## Porting a SocketCAN program

What stays as it is: `struct can_frame`, `struct canfd_frame`, `struct can_filter`, `canid_t`, and every `CAN_*`, `CANFD_*` and `CAN_ERR_*` constant the header defines. They have the Linux names, values and layout, so the code that fills in frames, decodes them, builds filter lists and reads error frames does not change.

What changes is every line that touches the socket, because macOS has no `PF_CAN`. In order:

1. **Headers.** Replace `<linux/can.h>`, `<linux/can/raw.h>`, `<linux/can/error.h>`, `<linux/can/isotp.h>`, `<net/if.h>` and `<sys/ioctl.h>` (as far as they were there for CAN) with `<darwincan.h>`, `<linux/can/bcm.h>` with `<darwincan_bcm.h>`, and `<linux/can/j1939.h>` with `<darwincan_j1939.h>`.
2. **Opening.** `socket`, the `SIOCGIFINDEX` ioctl and `bind` become one call. The socket is a `dcan_raw *`, not an `int`.
3. **Options.** Each `setsockopt` becomes the matching `dcan_raw_set_*` call. Drop `CAN_RAW_FD_FRAMES`: a socket receives classic and CAN FD frames alike.
4. **Sending.** `write(s, &frame, sizeof frame)` becomes `dcan_raw_send` for a `can_frame` and `dcan_raw_send_fd` for a `canfd_frame`.
5. **Receiving.** `read` becomes `dcan_raw_recv`, which always takes a `struct canfd_frame` to fill and returns `CAN_MTU` or `CANFD_MTU` as `read` does. Code that tells the two kinds apart by the size read keeps working.
6. **Waiting.** Where the program used `select` or `poll` only to give `read` a timeout, pass the timeout to `dcan_raw_recv` and delete the rest. Where it waits on several things at once, see [Waiting with poll](#waiting-with-poll).
7. **Errors.** `perror` and `errno` become `dcan_last_error()`.
8. **Closing.** `close(s)` becomes `dcan_raw_close(sock)`.

Before:

```c
int s = socket(PF_CAN, SOCK_RAW, CAN_RAW);
struct ifreq ifr;
struct sockaddr_can addr = { .can_family = AF_CAN };
struct can_filter filter = { .can_id = 0x100, .can_mask = 0x700 };
struct can_frame frame;
int on = 1;

strcpy(ifr.ifr_name, "can0");
ioctl(s, SIOCGIFINDEX, &ifr);
addr.can_ifindex = ifr.ifr_ifindex;
bind(s, (struct sockaddr *)&addr, sizeof(addr));
setsockopt(s, SOL_CAN_RAW, CAN_RAW_FILTER, &filter, sizeof(filter));
setsockopt(s, SOL_CAN_RAW, CAN_RAW_RECV_OWN_MSGS, &on, sizeof(on));

while (read(s, &frame, sizeof(frame)) == sizeof(frame))
	handle(&frame);
perror("read");
close(s);
```

After:

```c
dcan_raw *s = dcan_raw_open("can0");
struct can_filter filter = { .can_id = 0x100, .can_mask = 0x700 };
struct canfd_frame frame;

dcan_raw_set_filters(s, &filter, 1);
dcan_raw_set_recv_own_msgs(s, 1);

while (dcan_raw_recv(s, &frame, NULL, -1) == CAN_MTU)
	handle((struct can_frame *)&frame);
fprintf(stderr, "recv: %s\n", dcan_last_error());
dcan_raw_close(s);
```

### Call by call

| On Linux | Here |
|---|---|
| `socket(PF_CAN, SOCK_RAW, CAN_RAW)`, `SIOCGIFINDEX`, `bind` | `dcan_raw_open("can0")` |
| `write` of a `can_frame` | `dcan_raw_send` |
| `write` of a `canfd_frame` | `dcan_raw_send_fd` |
| `read` | `dcan_raw_recv(sock, &frame, &info, -1)` |
| `read` with `SO_RCVTIMEO`, or after `select` with a timeout | `dcan_raw_recv` with the timeout in milliseconds |
| `MSG_DONTWAIT`, `O_NONBLOCK` | `dcan_raw_recv` with a timeout of 0 |
| `CAN_RAW_FILTER` | `dcan_raw_set_filters` |
| `CAN_RAW_FILTER` with length 0 (receive nothing) | `dcan_raw_set_filters(sock, NULL, 0)` |
| `CAN_RAW_JOIN_FILTERS` | `dcan_raw_set_join_filters` |
| `CAN_RAW_ERR_FILTER` | `dcan_raw_set_err_filter` |
| `CAN_RAW_LOOPBACK` | `dcan_raw_set_loopback` |
| `CAN_RAW_RECV_OWN_MSGS` | `dcan_raw_set_recv_own_msgs` |
| `CAN_RAW_FD_FRAMES` | Delete it |
| `SO_TIMESTAMP`, `SO_TIMESTAMPING`, `SIOCGSTAMP` | `info.timestamp_ns` from `dcan_raw_recv` |
| `MSG_CONFIRM` in `msg_flags` (own frame) | `info.flags & DCAN_RX_OWN` |
| `poll` or `select` on the socket | `poll` or `select` on `dcan_raw_fileno(sock)` |
| A `CAN_ISOTP` socket, `CAN_ISOTP_OPTS`, `CAN_ISOTP_RECV_FC`, `CAN_ISOTP_LL_OPTS` | `dcan_isotp_open` with a `struct dcan_isotp_options` |
| `write` and `read` on it | `dcan_isotp_send`, `dcan_isotp_recv` |
| `socket(PF_CAN, SOCK_DGRAM, CAN_BCM)` and `connect` | `dcan_bcm_open("can0")` |
| `write` of a `bcm_msg_head` and its frames | `dcan_bcm_send(sock, &msg.msg_head, sizeof(msg))` |
| `read` on the BCM socket | `dcan_bcm_recv` |
| `socket(PF_CAN, SOCK_DGRAM, CAN_J1939)` and `bind` with `can_addr.j1939` | `dcan_j1939_open("can0", &addr)` |
| `bind` again, `connect` | `dcan_j1939_bind`, `dcan_j1939_connect` |
| `send`, `sendto` on it | `dcan_j1939_send`, `dcan_j1939_sendto` |
| `recvfrom`, `recvmsg` with `SCM_J1939_DEST_ADDR`, `SCM_J1939_DEST_NAME`, `SCM_J1939_PRIO` | `dcan_j1939_recv`, which fills a `struct dcan_j1939_info` |
| `SO_J1939_FILTER`, `SO_J1939_PROMISC`, `SO_J1939_SEND_PRIO`, `SO_BROADCAST` | `dcan_j1939_set_filters`, `_set_promisc`, `_set_send_prio`, `_set_broadcast` |
| `SO_J1939_ERRQUEUE` and `recvmsg(MSG_ERRQUEUE)` | `dcan_j1939_set_notices`; the notices come from `dcan_j1939_recv` |
| `ip link set can0 up type can bitrate 500000` (or netlink) | `dcan_interface_up`, or `canctl link set ...` |
| `ip link add vcan1 type vcan` | `dcan_vcan_add` |
| `if_nametoindex`, `if_indextoname` | `dcan_if_nametoindex`, `dcan_if_indextoname` |
| `if_nameindex` | `dcan_interfaces` |
| Binding to interface index 0 (all interfaces) | `dcan_raw_open(DCAN_ANY)` |
| `recvfrom` and `can_ifindex` in the address | `info.ifindex` from `dcan_raw_recv` |
| `sendto` with a `sockaddr_can` | `dcan_raw_sendto`, `dcan_raw_sendto_fd` |
| `SO_RXQ_OVFL` | `info.dropped` from `dcan_raw_recv` |
| `ip link set can0 name chassis` | `dcan_interface_rename`, or `canctl link set can0 name chassis` |

### Where it behaves differently

- **The handle is not a file descriptor.** macOS has no CAN sockets and no way to add them, so frames cannot travel through `read` and `write` on a descriptor, and a `dcan_raw *` cannot be passed to `dup`, kept across `fork` or handed to another process. `dcan_raw_fileno` gives a descriptor for waiting only (see [Waiting with poll](#waiting-with-poll)). This is the one difference that cannot be removed.
- **Timestamps are always there** and always the adapter's where it has a clock, placed on the Mac's clock by the daemon, with no socket option to ask for them. They are nanoseconds since 1970 in a `uint64_t`, not a `struct timeval`. They never step with the wall clock, so differences are exact.
- **An interface's index does not survive a restart of the daemon;** its name does. Look the index up when the program starts (`dcan_if_nametoindex`), as a Linux program does after a reboot.

What works as on Linux, and is worth knowing:

- **A send on an interface that is down fails at once,** with `DCAN_ERROR_INTERFACE_DOWN`, as `write` fails with `ENETDOWN`; so does a send on one that is bus-off, or whose adapter was unplugged (`DCAN_ERROR_INTERFACE_REMOVED`). A frame that the daemon took and the adapter then could not send, because nothing on the bus acknowledges, is not reported by the send on either system. Here a later `dcan_raw_recv` on the same socket returns `DCAN_ERROR_TRANSMIT_FAILED` once the adapter's buffer is full.
- **A slow reader loses frames; it never slows the bus.** The daemon keeps some 16,000 frames for each socket and drops what does not fit. `dcan_rx_info.dropped` counts the frames a socket has lost so far, as `SO_RXQ_OVFL` does.
- **One socket can cover every interface:** `dcan_raw_open(DCAN_ANY)`, which is what binding to interface index 0 is on Linux. It hears interfaces that appear later too. Each frame says in `dcan_rx_info.ifindex` where it was, and sending takes `dcan_raw_sendto` with an index.
- **An adapter has the same name every time.** The daemon keeps each adapter channel's name, by serial number and channel, so `can0` is the same connector whatever else is plugged in and in whatever order. `canctl link set can0 name chassis` (or `dcan_interface_rename`) gives it a name of your choosing, which it keeps as well. An adapter with no serial number of its own (the classic PCAN-USB) is known by the USB port it is in, and gets another name in another port.
- **Your own frames come back when they are on the bus,** not when they are sent: with `dcan_raw_set_recv_own_msgs`, a frame is delivered once the adapter reports it sent and acknowledged, with the time it was on the bus. A frame nobody acknowledges does not come back.

### Not there

| Missing | What to do |
|---|---|
| A `CAN_BCM` socket connected to no interface (jobs on all of them), `sendto` with an interface index per message | One `dcan_bcm` per interface |
| Classic frames with a DLC of 9 to 15 (`len8_dlc`, `CAN_CTRLMODE_CC_LEN8_DLC`) | Not supported; `len8_dlc` is ignored and received as 0 |
| ISO-TP extended and mixed addressing, `CAN_ISOTP_LISTEN_MODE`, functional addressing without flow control (`CAN_ISOTP_SF_BROADCAST`) | Not supported yet. A single frame to a functional address can be sent on a raw socket |
| J1939: the `J1939_EE_INFO_RX_DPO` notice, `SO_TIMESTAMPING`'s `SCM_TSTAMP_SCHED`, a priority for one message | Not supported; the notices there are say when a message is out or given up |
| `CAN_GW`, CAN XL | Not supported |
| Bus-off recovery (`restart-ms`) | Take the interface down and up |

## Results and errors

A function returns 0 or a positive count when it worked and a negative `DCAN_ERROR_*` value when it did not. The open functions return `NULL`. `dcan_last_error()` gives the message, kept per thread until that thread's next failing call. The codes a program is most likely to act on:

| Code | Meaning | What a program can do |
|---|---|---|
| `DCAN_ERROR_DAEMON_UNAVAILABLE` | Nothing is listening on the daemon's socket | Start `darwincand` |
| `DCAN_ERROR_VERSION_MISMATCH` | Daemon and library are from different versions | Reinstall, restart the daemon |
| `DCAN_ERROR_NO_SUCH_INTERFACE` | No interface of that name | Check `canctl list` |
| `DCAN_ERROR_INTERFACE_DOWN` | A send on an interface that is not on the bus, or is bus-off | Bring it up; after bus-off, down and up |
| `DCAN_ERROR_TRANSMIT_FAILED` | A frame could not be handed to the adapter, for example because nothing acknowledges and its buffer is full | Check wiring, bitrate, termination |
| `DCAN_ERROR_INTERFACE_REMOVED` | The adapter was unplugged | Close the socket; open again when it is back |
| `DCAN_ERROR_IO` | The connection to the daemon failed: it stopped or was restarted | Close the handle and open a new one |
| `DCAN_ERROR_INTERRUPTED` | A signal handler ran while a receive was waiting | Check your flag, call again |
| `DCAN_ERROR_ISOTP_TX`, `DCAN_ERROR_ISOTP_RX` | An ISO-TP transfer failed | `dcan_isotp_last_failure()` says why |
| `DCAN_ERROR_INVALID_ARGUMENT` | A null pointer, a frame that cannot exist, an option out of range | Fix the call |

After `DCAN_ERROR_IO` a handle is finished: every further call on it fails, and nothing reconnects by itself.

## Waiting

Every receive takes a timeout in milliseconds: a negative value waits without limit, 0 returns only what has already arrived, and a positive value waits that long. A receive returns 0 when nothing came in time.

### Waiting with poll

To wait on a CAN socket together with other descriptors, use `dcan_raw_fileno` (or `dcan_isotp_fileno`, `dcan_bcm_fileno`) with `poll`, `select` or `kqueue`, as a socket's descriptor is used on Linux: wait, then receive.

```c
struct pollfd fds[2] = {
	{ .fd = dcan_raw_fileno(can), .events = POLLIN },
	{ .fd = other,                .events = POLLIN },
};

for (;;) {
	poll(fds, 2, -1);
	if (fds[0].revents & POLLIN) {
		n = dcan_raw_recv(can, &frame, &info, 0);
		if (n < 0)
			break;
		if (n > 0)
			handle(&frame);
	}
	if (fds[1].revents & POLLIN)
		handle_other();
}
```

The descriptor is readable exactly when a receive with a timeout of 0 would return something, whether that is still on its way from the daemon or already inside the library. Receive with a timeout of 0 after waiting, not with one that blocks: now and then the descriptor is readable for something that is not a frame for you (an acknowledgement, news of the interface), and the receive then returns 0.

It is a descriptor for waiting and nothing else. Never `read`, `write` or `close` it; it lasts until the handle is closed.

### Signals

A receive that is waiting returns `DCAN_ERROR_INTERRUPTED` when a signal handler has run, provided the handler was installed with `sigaction` without `SA_RESTART` (see `dump.c`). A handler installed with `signal()` restarts the wait instead, as it does for `read`, and the receive does not return; give the receive a timeout and check your flag when it returns 0 if you cannot change how the handler is installed.

Sends, option changes and `dcan_isotp_send` are not cut short by a signal. No call raises `SIGPIPE`, even when the daemon has gone.

## Threads

- A raw socket, an ISO-TP channel and a broadcast manager may each be used by one thread that receives and any number that send, all at once. The usual arrangement, a reader thread and a sender, needs no locking.
- Two threads receiving on one handle is not supported: which gets a frame is arbitrary, and their timeouts interfere. Open a second socket on the interface instead; they are cheap, and each has its own filters.
- A control connection takes one call at a time.
- `dcan_last_error()` and `dcan_isotp_last_failure()` are per thread.
- Close a handle only when no thread is in a call on it. To stop a reader thread, give its receive a timeout and have it check a flag.

## ISO-TP

A channel is one pair of identifiers on one interface, with normal addressing. Segmentation, flow control and the timing of consecutive frames run in the daemon on a real-time thread, so they do not depend on when your process is scheduled.

```c
struct dcan_isotp_options options;
uint8_t response[4095];

dcan_isotp_options_init(&options, 0x7E0, 0x7E8);   /* we send 7E0, hear 7E8 */
dcan_isotp *ecu = dcan_isotp_open("can0", &options);

if (dcan_isotp_send(ecu, (const uint8_t[]){ 0x22, 0xF1, 0x90 }, 3) < 0)
	fprintf(stderr, "%s\n", dcan_last_error());
int len = dcan_isotp_recv(ecu, response, sizeof response, NULL, 1000);
```

- **Always start from `dcan_isotp_options_init`,** then change fields. It sets the defaults (8-byte frames, no padding, timeouts of a second) and clears the fields reserved for later versions.
- **29-bit identifiers:** add `CAN_EFF_FLAG` to `tx_id` and `rx_id`.
- **CAN FD:** set `flags` to `DCAN_ISOTP_FD` (and `DCAN_ISOTP_BRS` for the data bitrate) and `tx_dl` to the frame length, up to 64.
- **Padding:** set `DCAN_ISOTP_PADDING` and `pad_byte`. Most ECUs expect it.
- **`st_min` and `block_size`** are what this end tells the peer to respect when the peer sends. When this end sends, it respects what the peer asks for, and that is not configurable. The gap between consecutive frames is never shorter than the peer's STmin; with the PEAK adapters it is 0.2 to 1.2 ms longer, and with an STmin under a millisecond a PCAN-USB X6 still leaves about 1.2 ms.
- **`dcan_isotp_send` waits** until the peer has the whole message, or the transfer has failed. A thread that must not wait that long sends from another thread; a second send on the same channel waits for the first.
- **`dcan_isotp_recv` returns the message's length.** If that is more than the buffer's capacity the message was cut short to fit. Messages longer than `max_rx_len` (1 MiB unless changed) are refused on the bus with a flow control overflow.
- **A failed receive is reported once,** by the next `dcan_isotp_recv`, as `DCAN_ERROR_ISOTP_RX`; the channel carries on with the next message.

| `dcan_isotp_last_failure()` | Usual cause |
|---|---|
| `DCAN_ISOTP_FAILURE_TIMEOUT` | The peer did not send flow control or its next frame within `n_bs_ms` / `n_cr_ms`: wrong identifiers, ECU not listening, bus down |
| `DCAN_ISOTP_FAILURE_NOT_SENT` | A frame of the message could not be put on the bus: the interface went down or bus-off, or nothing acknowledged |
| `DCAN_ISOTP_FAILURE_OVERFLOW` | The peer refused the message as too long |
| `DCAN_ISOTP_FAILURE_SEQUENCE` | A consecutive frame was lost or out of order |
| `DCAN_ISOTP_FAILURE_ABORTED` | The peer began a new message before finishing the last |
| `DCAN_ISOTP_FAILURE_INVALID_LENGTH` | The message is empty or longer than the protocol allows |
| `DCAN_ISOTP_FAILURE_INVALID_FLOW_CONTROL`, `DCAN_ISOTP_FAILURE_WAIT_LIMIT` | The peer's flow control was malformed, or it asked to wait too often |

## The broadcast manager

The broadcast manager sends frames cyclically and watches received frames for you. Its jobs run in the daemon on a real-time thread, so a frame due every 10 ms leaves every 10 ms whether or not your process is scheduled, and a program that only wants to hear when a signal changes is not woken for every frame.

Everything in `<darwincan_bcm.h>` has the name, value and layout it has in `<linux/can/bcm.h>`: `struct bcm_msg_head`, `struct bcm_timeval`, the operations (`TX_SETUP`, `RX_SETUP`, ...) and the flags (`SETTIMER`, `STARTTIMER`, ...), and they mean the same. Code that builds and reads BCM messages stays as it is; `socket` and `connect` become `dcan_bcm_open`, `write` becomes `dcan_bcm_send` and `read` becomes `dcan_bcm_recv`.

Send a frame every 10 ms, then change its data without disturbing the rhythm:

```c
#include <darwincan_bcm.h>

struct {
	struct bcm_msg_head head;
	struct can_frame frame;
} msg = {
	.head = { .opcode = TX_SETUP, .flags = SETTIMER | STARTTIMER,
		  .ival2 = { 0, 10000 }, .can_id = 0x123, .nframes = 1 },
	.frame = { .can_id = 0x123, .len = 2, .data = { 0x11, 0x22 } },
};
dcan_bcm *jobs = dcan_bcm_open("can0");

dcan_bcm_send(jobs, &msg.head, sizeof(msg));

msg.head.flags = 0;                 /* leave the timer alone */
msg.frame.data[0] = 0x33;
dcan_bcm_send(jobs, &msg.head, sizeof(msg));
```

Be told when the first byte of 0x2B0 changes, and when the frame stays away for 50 ms:

```c
struct {
	struct bcm_msg_head head;
	struct can_frame frame;
} watch = {
	.head = { .opcode = RX_SETUP, .flags = SETTIMER,
		  .ival1 = { 0, 50000 }, .can_id = 0x2B0, .nframes = 1 },
	.frame = { .can_id = 0x2B0, .len = 8, .data = { 0xFF } },   /* the mask */
}, report;
uint64_t at;

dcan_bcm_send(jobs, &watch.head, sizeof(watch));
for (;;) {
	int n = dcan_bcm_recv(jobs, &report.head, sizeof(report), &at, -1);
	if (n < 0)
		break;
	if (report.head.opcode == RX_CHANGED)
		printf("now %02X\n", report.frame.data[0]);
	else if (report.head.opcode == RX_TIMEOUT)
		printf("gone\n");
}
```

What to know beyond the header:

- **A job is known by its identifier** (and by `CAN_FD_FRAME`). `TX_SETUP` or `RX_SETUP` for a job that exists changes it; it may hold fewer frames than it was created with, not more.
- **Starting a transmit job sends its first frame at once.** The others follow on a grid from that moment. A timer that is woken late sends one frame and returns to the grid; it never sends a burst to catch up.
- **For an endless cycle** set `ival2` and leave `count` and `ival1` 0. For a fast start, `count` frames go every `ival1` and the rest every `ival2`; with `TX_COUNTEVT` a `TX_EXPIRED` report says when the change happens.
- **CAN FD jobs** set `CAN_FD_FRAME` in the flags and follow the head with `struct canfd_frame`.
- **Closing the broadcast manager ends its jobs,** as closing the socket does on Linux. So does the program ending for any reason.
- **A job hears its own program's frames:** a receive job reports a frame that a transmit job or `TX_SEND` of the same broadcast manager sent.
- **A request is answered before `dcan_bcm_send` returns.** A request the daemon refuses fails with `DCAN_ERROR_INVALID_REQUEST` and a message that says why.
- **A job whose frames cannot be sent keeps trying,** as on Linux. Unlike Linux, the trouble is reported: the next `dcan_bcm_recv` returns the error once when it begins, for example `DCAN_ERROR_INTERFACE_DOWN`.
- **How exact it is.** Measured over 20 seconds with the data changed every second: on a virtual interface every interval of a 1 ms job was within 0.03 ms of 1 ms; from one PCAN-USB X6 channel to another it was within 0.13 ms, 99 in 100 within 0.03 ms.
- **The one difference in the declarations:** `frames` at the end of `struct bcm_msg_head` is declared `frames[0]`, as older Linux headers had it, not `frames[]`. Clang warns about the newer form whenever the head is put in front of frames in a struct, which is how every program uses it.

## J1939

A J1939 socket sends and receives whole messages of SAE J1939: a parameter group number (PGN), a source and a destination address, a priority and any number of data bytes. The daemon builds the 29-bit identifiers and runs the transport protocols for messages of more than eight bytes: the broadcast announce message (BAM) to everyone, a connection with request-to-send and clear-to-send (RTS/CTS) to one destination, and the extended protocol above 1785 bytes. It answers for every address the J1939 sockets on an interface have, on a real-time thread, so the protocol's timeouts and the 50 ms between the packets of a broadcast do not depend on your process.

`<darwincan_j1939.h>` has the constants (`J1939_NO_ADDR`, `J1939_NO_PGN`, `J1939_PGN_ADDRESS_CLAIMED`, ...), the types (`pgn_t`, `name_t`, `priority_t`) and `struct j1939_filter` of `<linux/can/j1939.h>`, and the sockets behave as `CAN_J1939` sockets do. An address is a `struct dcan_j1939_addr`, with the three fields `can_addr.j1939` has in `struct sockaddr_can`.

Listen to what everyone sends, and ask the engine (address 0) for its software identification:

```c
#include <darwincan_j1939.h>

struct dcan_j1939_addr me = { .pgn = J1939_NO_PGN, .addr = 0x80 };
struct dcan_j1939_addr engine = { .pgn = J1939_PGN_REQUEST, .addr = 0x00 };
uint8_t wanted[3] = { 0xDA, 0xFE, 0x00 }; /* PGN 0xFEDA, low byte first */
uint8_t data[1785];
struct dcan_j1939_info info;

dcan_j1939 *s = dcan_j1939_open("can0", &me);
dcan_j1939_set_broadcast(s, 1);
dcan_j1939_sendto(s, wanted, 3, &engine);
while (dcan_j1939_recv(s, data, sizeof data, &info, -1) == 1)
	printf("PGN %05X from %02X to %02X, %u bytes\n", info.src.pgn,
	       info.src.addr, info.dst_addr, info.size);
```

The answer to that request is longer than eight bytes, so the engine sends it through the transport protocol; it arrives as one message.

What to know:

- **What a socket receives.** Messages sent to its address, and, once `dcan_j1939_set_broadcast` is on, messages sent to everyone, which is most of the traffic on a vehicle. A PGN in the address the socket is bound to narrows that to one parameter group, `dcan_j1939_connect` to one sender, and filters to the sources and groups they name. `dcan_j1939_set_promisc` receives everything whoever it is for; filters still apply. A socket does not receive its own messages; other sockets on the same interface do.
- **Sending needs an address.** A socket opened with `NULL`, or bound to `J1939_NO_ADDR` without a NAME, only listens. Sending to everyone needs `dcan_j1939_set_broadcast`, as it needs `SO_BROADCAST` on Linux.
- **A send returns when the daemon has taken the message,** and fails at once if it cannot be sent at all (see the header for the codes). A message of up to eight bytes is then on its way. A longer one to one destination needs the destination to answer; if it does not, the session is given up after 1.25 s. To hear of that, turn on `dcan_j1939_set_notices`: `dcan_j1939_recv` then also returns notices, with `info.notice` saying `DCAN_J1939_TX_DONE` or `DCAN_J1939_TX_ABORT` (and `info.reason` why), in order with the messages. On Linux the same news comes through the error queue.
- **Messages to one destination go one after another;** a second long message to the same address waits for the first to finish. Single frames do not wait.
- **NAMEs.** A socket bound with a NAME sends from the address that NAME has claimed, and a message sent to a NAME goes to the address it has claimed; the daemon follows the address claims on the bus to know. As on Linux, claiming is the program's part: bind to the NAME and the address wanted, allow broadcasts, and send `J1939_PGN_ADDRESS_CLAIMED` to `J1939_NO_ADDR` with the NAME as its eight bytes, low byte first. 250 ms after the claim the NAME has the address; until then, and after another NAME takes the address away, sends fail with `DCAN_ERROR_ADDRESS_NOT_AVAILABLE`. To move, bind again with the new address and claim it. Contending for an address (answering a claim for yours, giving way to a lower NAME) is also the program's part; `j1939acd` does it on Linux and has no counterpart here yet.
- **Other programs on the same address.** A program that speaks J1939 on a raw socket must not use an address a J1939 socket has: both would answer a request to send.
- **A promiscuous socket does not see long messages between two other nodes put together,** on Linux either: only the receiver of a session takes part in it. Broadcasts are put together for everyone.
- **A session with a packet missing is given up,** with an abort to the sender, not repaired by asking for the packet again.
- **How it was tested.** Against itself: between sockets on a virtual interface, and between two channels of a PCAN-USB X6 at 250 kbit/s, up to 200,000 bytes in one message. Not yet against a real J1939 device or Linux's stack.

## Interfaces

```c
struct dcan_interface list[16];
struct dcan_bus_config config = { .bitrate = 500000, .data_bitrate = 2000000 };
dcan_control *control = dcan_control_open();

int n = dcan_interfaces(control, list, 16);
for (int i = 0; i < n && i < 16; i++)
	printf("%s %s %s %s\n", list[i].name, list[i].kind,
	       list[i].up ? "up" : "down", list[i].description);

dcan_interface_up(control, "can0", &config);
dcan_control_close(control);
```

Initialise a `struct dcan_bus_config` with designated initialisers, as above, so that the fields not named are zero: a zero `data_bitrate` means classic CAN only, a zero sample point the usual one for the bitrate. The settings are the daemon's and outlive the program; a second program that brings an interface up with other settings changes them for everyone using it.

## Versions

`DCAN_VERSION` in the header and `dcan_version()` in the library should be equal; if they are not, the program was compiled against one install and runs against another. `dcan_protocol_version()` is what the library and the daemon have to agree on.

The API is at version 0: it may still change between releases. Structures have reserved fields so that most additions will not change their size.

## When something does not work

| What you see | Likely cause |
|---|---|
| The open call fails with "cannot reach darwincand" | The daemon is not running, or `DARWINCAN_SOCKET` differs between it and the program |
| "different versions of darwinCAN", or "the daemon closed the connection without answering" | The daemon running is older or newer than the library; reinstall and restart it |
| `dcan_raw_send` succeeds and nothing is on the bus | Nothing acknowledges the frame (wiring, bitrate, termination, a lone node); a later `dcan_raw_recv` on that socket reports it once the adapter is full |
| Frames are missing and `info.dropped` goes up | The program reads too slowly; see the last row |
| "adapter transmit buffer is full" | Nothing acknowledges the frames (wiring, bitrate, termination, a lone node), or other traffic keeps the bus busy |
| A cyclic job stops when the program exits | It is meant to: the jobs belong to the `dcan_bcm`. Keep the program, or a small one like `cyclic.c`, running |
| The program hangs on Ctrl-C | The handler was installed with `signal()`; see [Signals](#signals) |
| `dyld: Library not loaded: .../libdarwincan.dylib` | The program was linked against the library in the build directory, which has since moved; run `make install` and link again, or link `libdarwincan.a` |
| Frames are missing under load | The program reads too slowly and the daemon dropped them; read in a dedicated thread, or filter |
