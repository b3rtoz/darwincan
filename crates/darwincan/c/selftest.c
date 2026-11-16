/* SPDX-License-Identifier: MIT OR Apache-2.0 */
/*
 * Exercises every part of darwincan.h against a running daemon, and so
 * doubles as an example of its use. The end-to-end tests compile and run
 * it.
 *
 *     cc selftest.c -ldarwincan -o selftest
 *     ./selftest [socket path]      (needs the interface vcan0)
 *     ./selftest <socket path or ""> can0 can1
 *                                   (two interfaces cabled together, both
 *                                   up with CAN FD)
 *     ./selftest --gone <socket path>   (against a daemon that hangs up)
 */
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>

#include <darwincan.h>
#include <darwincan_bcm.h>
#include <darwincan_j1939.h>

_Static_assert(sizeof(struct can_frame) == 16, "can_frame as on Linux");
_Static_assert(sizeof(struct canfd_frame) == 72, "canfd_frame as on Linux");
_Static_assert(offsetof(struct can_frame, data) == 8, "can_frame data");
_Static_assert(offsetof(struct canfd_frame, flags) == 5, "canfd_frame flags");
_Static_assert(sizeof(struct can_filter) == 8, "can_filter");
_Static_assert(sizeof(struct dcan_rx_info) == 24, "dcan_rx_info");
_Static_assert(sizeof(struct dcan_isotp_options) == 44, "dcan_isotp_options");
_Static_assert(sizeof(struct dcan_bus_config) == 20, "dcan_bus_config");
_Static_assert(sizeof(struct dcan_interface) == 188, "dcan_interface");
_Static_assert(sizeof(struct bcm_msg_head) == 56, "bcm_msg_head as on Linux");
_Static_assert(offsetof(struct bcm_msg_head, ival2) == 32, "bcm_msg_head ival2");
_Static_assert(offsetof(struct bcm_msg_head, frames) == 56, "bcm_msg_head frames");
_Static_assert(RX_CHANGED == 12, "the operations are numbered as on Linux");
_Static_assert(sizeof(struct j1939_filter) == 32, "j1939_filter as on Linux");
_Static_assert(offsetof(struct j1939_filter, addr) == 24, "j1939_filter addr");
_Static_assert(sizeof(struct dcan_j1939_addr) == 16, "dcan_j1939_addr");
_Static_assert(sizeof(struct dcan_j1939_info) == 48, "dcan_j1939_info");

#define CHECK(what)                                                          \
	do {                                                                 \
		if (!(what)) {                                                \
			fprintf(stderr, "%s:%d: %s failed (last error: %s)\n", \
				__FILE__, __LINE__, #what, dcan_last_error()); \
			exit(1);                                              \
		}                                                             \
	} while (0)

static const char *path;
/* The two ends: the same interface on a virtual bus, two interfaces
 * cabled to each other on real adapters. */
static const char *if_a = "vcan0", *if_b = "vcan0";
static int one_interface = 1;

static void interfaces(void)
{
	struct dcan_interface list[32];
	char name[16];
	struct dcan_bus_config config = { .bitrate = 500000 };
	dcan_control *control = dcan_control_open_at(path);
	int n, i, found = 0;

	CHECK(control != NULL);
	CHECK(dcan_vcan_add(control, "vcan7") == 0);
	CHECK(dcan_vcan_add(control, "vcan7") == DCAN_ERROR_ALREADY_EXISTS);
	CHECK(strstr(dcan_last_error(), "exists") != NULL);
	n = dcan_interfaces(control, list, 32);
	CHECK(n >= 2);
	for (i = 0; i < n && i < 32; i++) {
		if (strcmp(list[i].name, "vcan7") == 0) {
			found = 1;
			CHECK(strcmp(list[i].kind, "virtual") == 0);
			CHECK(list[i].up == 1 && list[i].has_config == 0);
			/* Its index and its name lead to each other. */
			CHECK(list[i].index != 0);
			CHECK(dcan_if_nametoindex("vcan7") == list[i].index);
			CHECK(dcan_if_indextoname(list[i].index, name) == name);
			CHECK(strcmp(name, "vcan7") == 0);
		}
	}
	CHECK(found);
	CHECK(dcan_if_nametoindex("nosuch0") == 0);
	CHECK(dcan_if_indextoname(999999, name) == NULL);
	/* Asking with no room still says how many there are. */
	CHECK(dcan_interfaces(control, NULL, 0) == n);
	/* A virtual interface has no bus settings to change. */
	CHECK(dcan_interface_configure(control, "vcan7", &config) ==
	      DCAN_ERROR_UNSUPPORTED);
	CHECK(dcan_interface_up(control, "nosuch0", &config) ==
	      DCAN_ERROR_NO_SUCH_INTERFACE);
	CHECK(dcan_interface_down(control, "nosuch0") ==
	      DCAN_ERROR_NO_SUCH_INTERFACE);
	/* Another name, and back. */
	CHECK(dcan_interface_rename(control, "vcan7", "vcan8") == 0);
	CHECK(dcan_if_nametoindex("vcan7") == 0);
	CHECK(dcan_if_nametoindex("vcan8") != 0);
	CHECK(dcan_interface_rename(control, "vcan8", "vcan0") ==
	      DCAN_ERROR_ALREADY_EXISTS);
	CHECK(dcan_interface_rename(control, "vcan8", "vcan7") == 0);
	CHECK(dcan_vcan_delete(control, "vcan7") == 0);
	CHECK(dcan_interfaces(control, NULL, 0) == n - 1);
	dcan_control_close(control);
}

static void raw_frames(void)
{
	dcan_raw *a = dcan_raw_open_at(path, if_a);
	dcan_raw *b = dcan_raw_open_at(path, if_b);
	struct can_frame classic = { .can_id = 0x123, .len = 3,
				     .data = { 0xDE, 0xAD, 0xBE } };
	struct can_frame remote = { .can_id = 0x124 | CAN_RTR_FLAG, .len = 4 };
	struct canfd_frame fd = { .can_id = 0x1ABCDEF0 | CAN_EFF_FLAG,
				  .len = 12, .flags = CANFD_BRS };
	struct canfd_frame got;
	struct can_frame *got_classic = (struct can_frame *)&got;
	struct dcan_rx_info info, own;
	struct can_filter only_fd = { .can_id = 0x1ABCDEF0 | CAN_EFF_FLAG,
				      .can_mask = CAN_EFF_MASK | CAN_EFF_FLAG };
	struct pollfd wait;
	int i, n;

	CHECK(a != NULL && b != NULL);
	CHECK(dcan_raw_open_at(path, "nosuch0") == NULL);
	CHECK(strstr(dcan_last_error(), "nosuch0") != NULL);
	for (i = 0; i < 12; i++)
		fd.data[i] = (uint8_t)(i + 1);

	/* Nothing has been sent: nothing arrives, with or without waiting. */
	CHECK(dcan_raw_recv(b, &got, NULL, 0) == 0);
	CHECK(dcan_raw_recv(b, &got, NULL, 20) == 0);

	CHECK(dcan_raw_send(a, &classic) == 0);
	CHECK(dcan_raw_send(a, &remote) == 0);
	CHECK(dcan_raw_send_fd(a, &fd) == 0);

	CHECK(dcan_raw_recv(b, &got, &info, 2000) == (int)CAN_MTU);
	CHECK(got_classic->can_id == 0x123 && got_classic->len == 3);
	CHECK(memcmp(got_classic->data, classic.data, 3) == 0);
	CHECK(info.timestamp_ns != 0 && (info.flags & DCAN_RX_OWN) == 0);
	/* Sent by another socket on this machine. */
	CHECK((info.flags & DCAN_RX_LOCAL) != 0);

	CHECK(dcan_raw_recv(b, &got, NULL, 2000) == (int)CAN_MTU);
	CHECK(got.can_id == (0x124 | CAN_RTR_FLAG) && got.len == 4);

	/* The third with poll, as on a socket: wait, then receive. The
	 * frame may be in the library already, having come with the two
	 * before it, and the descriptor says so all the same. */
	wait.fd = dcan_raw_fileno(b);
	wait.events = POLLIN;
	CHECK(wait.fd >= 0);
	CHECK(poll(&wait, 1, 2000) == 1);
	n = dcan_raw_recv(b, &got, &info, 0);
	CHECK(n == (int)CANFD_MTU);
	CHECK(info.ifindex == dcan_if_nametoindex(if_b) && info.dropped == 0);
	CHECK(got.can_id == fd.can_id && got.len == 12);
	CHECK(got.flags == (CANFD_FDF | CANFD_BRS));
	CHECK(memcmp(got.data, fd.data, 12) == 0);
	CHECK(dcan_raw_recv(b, &got, NULL, 0) == 0);
	/* Nothing left: the descriptor is quiet again. */
	CHECK(poll(&wait, 1, 0) == 0);

	/* The sender does not hear itself unless it asks to. */
	CHECK(dcan_raw_flush(a) == 0);
	CHECK(dcan_raw_recv(a, &got, NULL, 0) == 0);
	CHECK(dcan_raw_set_recv_own_msgs(a, 1) == 0);
	CHECK(dcan_raw_send(a, &classic) == 0);
	CHECK(dcan_raw_recv(a, &got, &own, 2000) == (int)CAN_MTU);
	CHECK(own.flags & DCAN_RX_OWN);
	CHECK(dcan_raw_recv(b, &got, &info, 2000) == (int)CAN_MTU);
	/* On one interface both see the frame with the one time it was
	 * stamped; two adapters each stamp it with their own clock. */
	if (one_interface)
		CHECK(info.timestamp_ns == own.timestamp_ns);

	/* Filters: only the CAN FD frame's identifier, then nothing at all. */
	CHECK(dcan_raw_set_filters(b, &only_fd, 1) == 0);
	CHECK(dcan_raw_send(a, &classic) == 0);
	CHECK(dcan_raw_send_fd(a, &fd) == 0);
	CHECK(dcan_raw_recv(b, &got, NULL, 2000) == (int)CANFD_MTU);
	CHECK(dcan_raw_set_filters(b, NULL, 0) == 0);
	CHECK(dcan_raw_send(a, &classic) == 0);
	CHECK(dcan_raw_flush(a) == 0);
	CHECK(dcan_raw_recv(b, &got, NULL, one_interface ? 0 : 100) == 0);

	/* Joined filters: 0x100 to 0x1FF and odd, so 0x123 but not 0x124. */
	{
		struct can_filter both[2] = { { 0x100, 0x700 }, { 0x001, 0x001 } };
		struct can_frame even = { .can_id = 0x124 };

		CHECK(dcan_raw_set_filters(b, both, 2) == 0);
		CHECK(dcan_raw_set_join_filters(b, 1) == 0);
		CHECK(dcan_raw_send(a, &even) == 0);
		CHECK(dcan_raw_send(a, &classic) == 0);
		CHECK(dcan_raw_recv(b, &got, NULL, 2000) == (int)CAN_MTU);
		CHECK(got.can_id == 0x123);
		CHECK(dcan_raw_set_join_filters(b, 0) == 0);
	}

	/* With loopback off, other local sockets on the interface do not
	 * see the frame. (Across a real bus it arrives all the same.) */
	CHECK(dcan_raw_set_filters(b, &(struct can_filter){ 0, 0 }, 1) == 0);
	CHECK(dcan_raw_set_loopback(a, 0) == 0);
	CHECK(dcan_raw_send(a, &classic) == 0);
	CHECK(dcan_raw_flush(a) == 0);
	if (one_interface)
		CHECK(dcan_raw_recv(b, &got, NULL, 0) == 0);
	else
		CHECK(dcan_raw_recv(b, &got, NULL, 2000) == (int)CAN_MTU);
	CHECK(dcan_raw_set_err_filter(b, CAN_ERR_MASK) == 0);

	/* Frames that cannot exist are refused before they are sent. */
	classic.len = 9;
	CHECK(dcan_raw_send(a, &classic) == DCAN_ERROR_INVALID_ARGUMENT);
	fd.len = 13;
	CHECK(dcan_raw_send_fd(a, &fd) == DCAN_ERROR_INVALID_ARGUMENT);
	CHECK(dcan_raw_send(NULL, &classic) == DCAN_ERROR_INVALID_ARGUMENT);

	dcan_raw_close(a);
	dcan_raw_close(b);
}

static void isotp(int with_fd)
{
	struct dcan_isotp_options tester, ecu;
	dcan_isotp *t, *e;
	uint8_t request[300], reply[5000], buffer[5000];
	uint64_t at = 0;
	size_t i;

	for (i = 0; i < sizeof(request); i++)
		request[i] = (uint8_t)(i * 7);
	for (i = 0; i < sizeof(reply); i++)
		reply[i] = (uint8_t)(i * 13 + 1);

	dcan_isotp_options_init(&tester, 0x7E0, 0x7E8);
	dcan_isotp_options_init(&ecu, 0x7E8, 0x7E0);
	CHECK(tester.tx_dl == 8 && tester.n_bs_ms == 1000);
	if (with_fd) {
		tester.flags = ecu.flags =
			DCAN_ISOTP_FD | DCAN_ISOTP_BRS | DCAN_ISOTP_PADDING;
		tester.tx_dl = ecu.tx_dl = 64;
		tester.pad_byte = ecu.pad_byte = 0xAA;
	}
	ecu.st_min = 0xF1;
	t = dcan_isotp_open_at(path, if_a, &tester);
	e = dcan_isotp_open_at(path, if_b, &ecu);
	CHECK(t != NULL && e != NULL);

	CHECK(dcan_isotp_recv(e, buffer, sizeof(buffer), NULL, 0) == 0);
	CHECK(dcan_isotp_recv(e, buffer, sizeof(buffer), NULL, 20) == 0);

	/* The daemon runs both ends, so one thread can send and then read. */
	CHECK(dcan_isotp_send(t, request, sizeof(request)) == 0);
	CHECK(dcan_isotp_recv(e, buffer, sizeof(buffer), &at, 2000) ==
	      (int)sizeof(request));
	CHECK(memcmp(buffer, request, sizeof(request)) == 0);
	CHECK(at != 0);

	CHECK(dcan_isotp_send(e, reply, sizeof(reply)) == 0);
	/* Too small a buffer gets the start and is told the whole length. */
	memset(buffer, 0, sizeof(buffer));
	CHECK(dcan_isotp_recv(t, buffer, 100, NULL, 2000) == (int)sizeof(reply));
	CHECK(memcmp(buffer, reply, 100) == 0 && buffer[100] == 0);
	CHECK(dcan_isotp_fileno(t) >= 0);

	/* A single frame both ways, the usual diagnostic exchange. */
	CHECK(dcan_isotp_send(t, (const uint8_t *)"\x22\xF1\x90", 3) == 0);
	CHECK(dcan_isotp_recv(e, buffer, sizeof(buffer), NULL, 2000) == 3);
	CHECK(memcmp(buffer, "\x22\xF1\x90", 3) == 0);

	CHECK(dcan_isotp_send(t, request, 0) == DCAN_ERROR_INVALID_ARGUMENT);
	dcan_isotp_close(e);

	/* With nobody to answer, a long message fails after N_Bs. */
	dcan_isotp_close(t);
	tester.n_bs_ms = 50;
	t = dcan_isotp_open_at(path, if_a, &tester);
	CHECK(t != NULL);
	CHECK(dcan_isotp_send(t, request, sizeof(request)) == DCAN_ERROR_ISOTP_TX);
	CHECK(dcan_isotp_last_failure() == DCAN_ISOTP_FAILURE_TIMEOUT);
	dcan_isotp_close(t);

	/* Settings that cannot work are refused when opening. */
	tester.tx_dl = 13;
	CHECK(dcan_isotp_open_at(path, if_a, &tester) == NULL);
	CHECK(strstr(dcan_last_error(), "tx_dl") != NULL);
}

/* A broadcast manager message with one classic frame, as Linux code
 * declares it. */
struct bcm_one {
	struct bcm_msg_head head;
	struct can_frame frame;
};

/* Cyclic sending and watching received frames. The jobs are on one end;
 * a raw socket on the other end sees and feeds them. */
static void broadcast_manager(void)
{
	dcan_bcm *jobs = dcan_bcm_open_at(path, if_a);
	dcan_raw *other = dcan_raw_open_at(path, if_b);
	struct bcm_one tx = {
		.head = { .opcode = TX_SETUP, .flags = SETTIMER | STARTTIMER,
			  .ival2 = { 0, 5000 }, .can_id = 0x2A0, .nframes = 1 },
		.frame = { .can_id = 0x2A0, .len = 2, .data = { 1, 2 } },
	};
	struct bcm_one rx = {
		.head = { .opcode = RX_SETUP, .flags = SETTIMER,
			  .ival1 = { 0, 50000 }, .can_id = 0x2B0, .nframes = 1 },
		/* Only the first byte matters. */
		.frame = { .can_id = 0x2B0, .len = 2, .data = { 0xFF, 0x00 } },
	};
	struct bcm_one report;
	struct bcm_msg_head request = { .can_id = 0x2A0 };
	struct can_frame feed = { .can_id = 0x2B0, .len = 2 };
	struct canfd_frame got;
	struct dcan_rx_info info;
	uint64_t first = 0, at = 0, changed_at;
	int i;

	CHECK(jobs != NULL && other != NULL);
	CHECK(dcan_bcm_open_at(path, "nosuch0") == NULL);

	/* A frame every 5 ms, the first at once. */
	CHECK(dcan_bcm_send(jobs, &tx.head, sizeof(tx)) == 0);
	for (i = 0; i < 6; i++) {
		CHECK(dcan_raw_recv(other, &got, &info, 2000) == (int)CAN_MTU);
		CHECK(got.can_id == 0x2A0 && got.len == 2 && got.data[1] == 2);
		if (i == 0)
			first = info.timestamp_ns;
	}
	/* Five intervals, give or take what starting and the bus add. */
	CHECK(info.timestamp_ns - first > 24000000);
	CHECK(info.timestamp_ns - first < 500000000);

	/* New data without touching the timer: no flags, same identifier. */
	tx.head.flags = 0;
	tx.frame.data[1] = 9;
	CHECK(dcan_bcm_send(jobs, &tx.head, sizeof(tx)) == 0);
	for (i = 0; i < 100 && got.data[1] != 9; i++)
		CHECK(dcan_raw_recv(other, &got, NULL, 2000) == (int)CAN_MTU);
	CHECK(got.data[1] == 9);

	/* What the job is now. */
	request.opcode = TX_READ;
	CHECK(dcan_bcm_send(jobs, &request, sizeof(request)) == 0);
	CHECK(dcan_bcm_recv(jobs, &report.head, sizeof(report), NULL, 2000) ==
	      (int)sizeof(report));
	CHECK(report.head.opcode == TX_STATUS && report.head.nframes == 1);
	CHECK(report.head.ival2.tv_sec == 0 && report.head.ival2.tv_usec == 5000);
	CHECK(report.frame.can_id == 0x2A0 && report.frame.data[1] == 9);

	/* Gone, and gone only once. */
	request.opcode = TX_DELETE;
	CHECK(dcan_bcm_send(jobs, &request, sizeof(request)) == 0);
	CHECK(dcan_bcm_send(jobs, &request, sizeof(request)) ==
	      DCAN_ERROR_INVALID_REQUEST);
	CHECK(strstr(dcan_last_error(), "no such transmit job") != NULL);
	while (dcan_raw_recv(other, &got, NULL, 100) > 0)
		;

	/* A size that does not fit the number of frames is refused here. */
	CHECK(dcan_bcm_send(jobs, &tx.head, sizeof(tx.head)) ==
	      DCAN_ERROR_INVALID_ARGUMENT);

	/* Watch 0x2B0: reported the first time and when its first byte
	 * changes, and its absence for 50 ms. */
	CHECK(dcan_bcm_send(jobs, &rx.head, sizeof(rx)) == 0);
	CHECK(dcan_bcm_recv(jobs, &report.head, sizeof(report), NULL, 0) == 0);
	feed.data[0] = 1;
	CHECK(dcan_raw_send(other, &feed) == 0);
	feed.data[1] = 5;       /* not under the mask */
	CHECK(dcan_raw_send(other, &feed) == 0);
	feed.data[0] = 2;
	CHECK(dcan_raw_send(other, &feed) == 0);

	CHECK(dcan_bcm_recv(jobs, &report.head, sizeof(report), &at, 2000) ==
	      (int)sizeof(report));
	CHECK(report.head.opcode == RX_CHANGED && report.head.can_id == 0x2B0);
	CHECK(report.frame.data[0] == 1 && report.frame.data[1] == 0 && at != 0);
	CHECK(dcan_bcm_recv(jobs, &report.head, sizeof(report), &changed_at, 2000) ==
	      (int)sizeof(report));
	CHECK(report.head.opcode == RX_CHANGED);
	CHECK(report.frame.data[0] == 2 && report.frame.data[1] == 5);
	CHECK(changed_at > at);

	/* A report with no frame is just the head. */
	CHECK(dcan_bcm_recv(jobs, &report.head, sizeof(report), &at, 2000) ==
	      (int)sizeof(struct bcm_msg_head));
	CHECK(report.head.opcode == RX_TIMEOUT && report.head.nframes == 0);
	CHECK(at - changed_at >= 50000000);
	CHECK(dcan_bcm_fileno(jobs) >= 0);

	/* A single frame, without a job. */
	tx.head.opcode = TX_SEND;
	tx.frame.can_id = 0x2C0;
	CHECK(dcan_bcm_send(jobs, &tx.head, sizeof(tx)) == 0);
	CHECK(dcan_raw_recv(other, &got, NULL, 2000) == (int)CAN_MTU);
	CHECK(got.can_id == 0x2C0);

	dcan_bcm_close(jobs);
	dcan_raw_close(other);
}

static void on_alarm(int signal_number)
{
	(void)signal_number;
}

static void alarm_in(int milliseconds)
{
	struct itimerval timer = { .it_value = { 0, milliseconds * 1000 } };

	CHECK(setitimer(ITIMER_REAL, &timer, NULL) == 0);
}

/* A signal ends a receive that is waiting, as it ends a read, and the
 * socket is as good as before. */
static void signals(void)
{
	struct sigaction action = { .sa_handler = on_alarm };
	struct dcan_isotp_options options;
	struct can_frame frame = { .can_id = 0x321, .len = 1, .data = { 7 } };
	struct canfd_frame got;
	uint8_t buffer[8];
	dcan_raw *a = dcan_raw_open_at(path, if_a);
	dcan_raw *b = dcan_raw_open_at(path, if_b);
	dcan_isotp *t;

	CHECK(a != NULL && b != NULL);
	sigemptyset(&action.sa_mask);
	CHECK(sigaction(SIGALRM, &action, NULL) == 0);

	alarm_in(50);
	CHECK(dcan_raw_recv(b, &got, NULL, -1) == DCAN_ERROR_INTERRUPTED);
	alarm_in(50);
	CHECK(dcan_raw_recv(b, &got, NULL, 5000) == DCAN_ERROR_INTERRUPTED);
	CHECK(dcan_raw_send(a, &frame) == 0);
	CHECK(dcan_raw_recv(b, &got, NULL, 2000) == (int)CAN_MTU);
	CHECK(got.can_id == 0x321 && got.data[0] == 7);

	dcan_isotp_options_init(&options, 0x7E0, 0x7E8);
	t = dcan_isotp_open_at(path, if_a, &options);
	CHECK(t != NULL);
	alarm_in(50);
	CHECK(dcan_isotp_recv(t, buffer, sizeof(buffer), NULL, -1) ==
	      DCAN_ERROR_INTERRUPTED);
	CHECK(dcan_isotp_recv(t, buffer, sizeof(buffer), NULL, 0) == 0);

	dcan_isotp_close(t);
	dcan_raw_close(a);
	dcan_raw_close(b);
}

struct listener {
	dcan_isotp *channel;
	int lengths[2];
};

/* Receives two messages, waiting without limit for each. */
static void *listen_twice(void *argument)
{
	struct listener *l = argument;
	uint8_t buffer[512];
	int i;

	for (i = 0; i < 2; i++)
		l->lengths[i] = dcan_isotp_recv(l->channel, buffer,
						sizeof(buffer), NULL, -1);
	return NULL;
}

/* One thread waits in a receive on a channel while another sends a long
 * message on the same channel. */
static void threads(void)
{
	struct dcan_isotp_options tester, ecu;
	struct listener l;
	pthread_t thread;
	uint8_t request[300], reply[5000], buffer[5000];
	size_t i;

	for (i = 0; i < sizeof(request); i++)
		request[i] = (uint8_t)i;
	for (i = 0; i < sizeof(reply); i++)
		reply[i] = (uint8_t)(i * 3);
	dcan_isotp_options_init(&tester, 0x7E0, 0x7E8);
	dcan_isotp_options_init(&ecu, 0x7E8, 0x7E0);
	dcan_isotp *t = dcan_isotp_open_at(path, if_a, &tester);
	l.channel = dcan_isotp_open_at(path, if_b, &ecu);
	CHECK(t != NULL && l.channel != NULL);
	CHECK(pthread_create(&thread, NULL, listen_twice, &l) == 0);

	CHECK(dcan_isotp_send(t, request, sizeof(request)) == 0);
	/* The listener is in its second receive, or about to be, while this
	 * thread sends on its channel. */
	CHECK(dcan_isotp_send(l.channel, reply, sizeof(reply)) == 0);
	CHECK(dcan_isotp_recv(t, buffer, sizeof(buffer), NULL, 2000) ==
	      (int)sizeof(reply));
	CHECK(memcmp(buffer, reply, sizeof(reply)) == 0);
	CHECK(dcan_isotp_send(t, request, 100) == 0);
	CHECK(pthread_join(thread, NULL) == 0);
	CHECK(l.lengths[0] == (int)sizeof(request) && l.lengths[1] == 100);

	dcan_isotp_close(t);
	dcan_isotp_close(l.channel);
}

/* The daemon hangs up: calls fail with an error, and nothing kills the
 * program (a write to a closed socket would, by SIGPIPE, if the library
 * did not prevent it). */
static int daemon_gone(void)
{
	struct can_frame frame = { .can_id = 0x123, .len = 8 };
	struct canfd_frame got;
	dcan_raw *sock = dcan_raw_open_at(path, "vcan0");
	int i, result = 0;

	CHECK(sock != NULL);
	/* As when started from a shell: the tests' runner ignores the
	 * signal, and a program it starts would inherit that. */
	signal(SIGPIPE, SIG_DFL);
	for (i = 0; i < 100000 && result == 0; i++)
		result = dcan_raw_send(sock, &frame);
	CHECK(result == DCAN_ERROR_IO);
	CHECK(dcan_raw_recv(sock, &got, NULL, 100) == DCAN_ERROR_IO);
	dcan_raw_close(sock);
	printf("gone ok\n");
	return 0;
}

static void j1939(void)
{
	struct dcan_j1939_addr a_addr = { .pgn = J1939_NO_PGN, .addr = 0x80 };
	struct dcan_j1939_addr b_addr = { .pgn = J1939_NO_PGN, .addr = 0x90 };
	struct dcan_j1939_addr to_b = { .pgn = 0xEF00, .addr = 0x90 };
	struct dcan_j1939_addr other_pgn = { .pgn = 0xAA00, .addr = 0x90 };
	struct dcan_j1939_addr everyone = { .pgn = 0xFEB0, .addr = J1939_NO_ADDR };
	struct dcan_j1939_addr named = { .name = 0x1234, .pgn = 0xEF00,
					 .addr = J1939_NO_ADDR };
	struct dcan_j1939_addr bad_pgn = { .pgn = 0xEF07, .addr = 0x90 };
	struct j1939_filter only = { .pgn = 0xAA00, .pgn_mask = J1939_PGN_MAX };
	dcan_j1939 *a = dcan_j1939_open_at(path, if_a, &a_addr);
	dcan_j1939 *b = dcan_j1939_open_at(path, if_b, &b_addr);
	dcan_j1939 *listener = dcan_j1939_open_at(path, if_b, NULL);
	struct dcan_j1939_info info;
	static uint8_t out[3000], in[3000];
	struct pollfd wait;
	size_t i;

	CHECK(a != NULL && b != NULL && listener != NULL);
	CHECK(dcan_j1939_open_at(path, "nosuch0", &a_addr) == NULL);
	CHECK(dcan_j1939_open_at(path, if_a, &bad_pgn) == NULL);
	for (i = 0; i < sizeof out; i++)
		out[i] = (uint8_t)(i * 11 + i / 256);

	/* One frame. */
	CHECK(dcan_j1939_recv(b, in, sizeof in, &info, 0) == 0);
	CHECK(dcan_j1939_sendto(a, out, 3, &to_b) == 0);
	wait.fd = dcan_j1939_fileno(b);
	wait.events = POLLIN;
	CHECK(wait.fd >= 0);
	CHECK(poll(&wait, 1, 2000) == 1);
	CHECK(dcan_j1939_recv(b, in, sizeof in, &info, 0) == 1);
	CHECK(info.notice == DCAN_J1939_MESSAGE && info.size == 3);
	CHECK(memcmp(in, out, 3) == 0);
	CHECK(info.src.addr == 0x80 && info.src.pgn == 0xEF00);
	CHECK(info.src.name == J1939_NO_NAME);
	CHECK(info.dst_addr == 0x90 && info.priority == 6);
	CHECK(info.timestamp_ns != 0);

	/* The transport protocol and the extended one, with notices. */
	CHECK(dcan_j1939_set_notices(a, 1) == 0);
	CHECK(dcan_j1939_set_notices(b, 1) == 0);
	CHECK(dcan_j1939_sendto(a, out, 1000, &to_b) == 0);
	CHECK(dcan_j1939_recv(b, in, sizeof in, &info, 5000) == 1);
	CHECK(info.notice == DCAN_J1939_RX_RTS && info.size == 1000);
	CHECK(dcan_j1939_recv(b, in, sizeof in, &info, 5000) == 1);
	CHECK(info.notice == DCAN_J1939_MESSAGE && info.size == 1000);
	CHECK(memcmp(in, out, 1000) == 0);
	CHECK(dcan_j1939_recv(a, in, sizeof in, &info, 5000) == 1);
	CHECK(info.notice == DCAN_J1939_TX_DONE && info.size == 1000);
	CHECK(info.acked == 1000);
	CHECK(info.src.addr == 0x80 && info.dst_addr == 0x90);
	CHECK(dcan_j1939_set_notices(a, 0) == 0);
	CHECK(dcan_j1939_sendto(a, out, sizeof out, &to_b) == 0);
	/* The extended protocol's sender says where each block of packets
	 * belongs: 3000 bytes are 429 packets, in blocks of 255. */
	CHECK(dcan_j1939_recv(b, in, sizeof in, &info, 5000) == 1);
	CHECK(info.notice == DCAN_J1939_RX_RTS && info.size == sizeof out);
	CHECK(dcan_j1939_recv(b, in, sizeof in, &info, 5000) == 1);
	CHECK(info.notice == DCAN_J1939_RX_DPO && info.size == sizeof out);
	CHECK(info.src.addr == 0x80 && info.src.pgn == 0xEF00 && info.acked == 0);
	CHECK(dcan_j1939_recv(b, in, sizeof in, &info, 5000) == 1);
	CHECK(info.notice == DCAN_J1939_RX_DPO && info.acked == 255 * 7);
	CHECK(dcan_j1939_set_notices(b, 0) == 0);
	/* A buffer too small gets the start; the size is the whole. */
	memset(in, 0, sizeof in);
	CHECK(dcan_j1939_recv(b, in, 4, &info, 10000) == 1);
	CHECK(info.notice == DCAN_J1939_MESSAGE);
	CHECK(info.size == sizeof out);
	CHECK(memcmp(in, out, 4) == 0 && in[4] == 0);

	/* To everyone, once both ends allow it. */
	CHECK(dcan_j1939_sendto(a, out, 20, &everyone) == DCAN_ERROR_NOT_PERMITTED);
	CHECK(dcan_j1939_set_broadcast(a, 1) == 0);
	CHECK(dcan_j1939_set_broadcast(b, 1) == 0);
	CHECK(dcan_j1939_sendto(a, out, 1786, &everyone) == DCAN_ERROR_TOO_BIG);
	CHECK(dcan_j1939_sendto(a, out, 20, &everyone) == 0);
	CHECK(dcan_j1939_recv(b, in, sizeof in, &info, 5000) == 1);
	CHECK(info.size == 20 && memcmp(in, out, 20) == 0);
	CHECK(info.dst_addr == J1939_NO_ADDR && info.src.pgn == 0xFEB0);
	/* Not for a socket that has not asked for broadcasts. */
	CHECK(dcan_j1939_recv(listener, in, sizeof in, &info, 0) == 0);

	/* Filters, priority and a connected socket. */
	CHECK(dcan_j1939_set_filters(b, &only, 1) == 0);
	CHECK(dcan_j1939_set_send_prio(a, 3) == 0);
	CHECK(dcan_j1939_set_send_prio(a, 8) == DCAN_ERROR_INVALID_ARGUMENT);
	CHECK(dcan_j1939_sendto(a, out, 2, &to_b) == 0);
	CHECK(dcan_j1939_connect(a, &other_pgn) == 0);
	CHECK(dcan_j1939_send(a, out, 5) == 0);
	CHECK(dcan_j1939_recv(b, in, sizeof in, &info, 5000) == 1);
	CHECK(info.src.pgn == 0xAA00 && info.size == 5 && info.priority == 3);
	/* A priority for one message, to a destination and connected. */
	CHECK(dcan_j1939_sendto_prio(a, out, 2, &other_pgn, 1) == 0);
	CHECK(dcan_j1939_recv(b, in, sizeof in, &info, 5000) == 1);
	CHECK(info.size == 2 && info.priority == 1);
	CHECK(dcan_j1939_sendto_prio(a, out, 4, NULL, 0) == 0);
	CHECK(dcan_j1939_recv(b, in, sizeof in, &info, 5000) == 1);
	CHECK(info.src.pgn == 0xAA00 && info.size == 4 && info.priority == 0);
	CHECK(dcan_j1939_send(a, out, 5) == 0);
	CHECK(dcan_j1939_recv(b, in, sizeof in, &info, 5000) == 1);
	CHECK(info.priority == 3);
	CHECK(dcan_j1939_sendto_prio(a, out, 2, &other_pgn, 8) ==
	      DCAN_ERROR_INVALID_ARGUMENT);
	CHECK(dcan_j1939_sendto_prio(a, out, 2, &other_pgn, -1) ==
	      DCAN_ERROR_INVALID_ARGUMENT);
	CHECK(dcan_j1939_set_filters(b, NULL, 0) == 0);
	CHECK(dcan_j1939_set_promisc(listener, 1) == 0);
	CHECK(dcan_j1939_send(a, out, 1) == 0);
	CHECK(dcan_j1939_recv(listener, in, sizeof in, &info, 5000) == 1);
	CHECK(info.src.pgn == 0xAA00 && info.dst_addr == 0x90);
	CHECK(dcan_j1939_recv(b, in, sizeof in, &info, 5000) == 1);
	CHECK(info.size == 1);

	/* What cannot be sent is refused at once. */
	CHECK(dcan_j1939_sendto(a, out, 1, &named) ==
	      DCAN_ERROR_ADDRESS_NOT_AVAILABLE);
	CHECK(dcan_j1939_sendto(a, out, 1, &bad_pgn) == DCAN_ERROR_INVALID_REQUEST);
	CHECK(dcan_j1939_sendto(listener, out, 1, &to_b) ==
	      DCAN_ERROR_INVALID_REQUEST);
	CHECK(dcan_j1939_sendto(a, out, 1, NULL) == DCAN_ERROR_INVALID_ARGUMENT);
	CHECK(dcan_j1939_sendto(a, NULL, 1, &to_b) == DCAN_ERROR_INVALID_ARGUMENT);
	CHECK(dcan_j1939_recv(b, in, sizeof in, NULL, 0) ==
	      DCAN_ERROR_INVALID_ARGUMENT);
	CHECK(dcan_j1939_send(NULL, out, 1) == DCAN_ERROR_INVALID_ARGUMENT);
	/* Moving to another address. */
	b_addr.addr = 0x91;
	CHECK(dcan_j1939_bind(b, &b_addr) == 0);
	to_b.addr = 0x91;
	CHECK(dcan_j1939_sendto(a, out, 4, &to_b) == 0);
	CHECK(dcan_j1939_recv(b, in, sizeof in, &info, 5000) == 1);
	CHECK(info.dst_addr == 0x91 && info.size == 4);

	dcan_j1939_close(a);
	dcan_j1939_close(b);
	dcan_j1939_close(listener);
	dcan_j1939_close(NULL);
}

int main(int argc, char **argv)
{
	if (argc == 3 && strcmp(argv[1], "--gone") == 0) {
		path = argv[2];
		return daemon_gone();
	}
	path = argc > 1 && argv[1][0] != '\0' ? argv[1] : NULL;
	if (argc == 4) {
		if_a = argv[2];
		if_b = argv[3];
		one_interface = strcmp(if_a, if_b) == 0;
	}
	/* The functions that take no socket path look here. */
	if (path != NULL)
		setenv("DARWINCAN_SOCKET", path, 1);
	CHECK(strcmp(dcan_version(), DCAN_VERSION) == 0);
	CHECK(dcan_protocol_version() > 0);
	interfaces();
	raw_frames();
	isotp(0);
	isotp(1);
	broadcast_manager();
	j1939();
	signals();
	threads();
	printf("ok\n");
	return 0;
}
