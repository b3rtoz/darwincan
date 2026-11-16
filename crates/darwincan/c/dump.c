/* SPDX-License-Identifier: MIT OR Apache-2.0 */
/*
 * dump.c: print the frames on an interface until interrupted. An example
 * of a receive loop with filters, error frames, timestamps and a clean
 * stop on Ctrl-C.
 *
 *     cc dump.c -ldarwincan -o dump
 *     ./dump vcan0             every frame
 *     ./dump can0 5 100:700    five frames with identifiers 0x100-0x1FF
 */
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>

#include <darwincan.h>

static volatile sig_atomic_t stop;

static void on_signal(int number)
{
	(void)number;
	stop = 1;
}

int main(int argc, char **argv)
{
	/* No SA_RESTART: the signal ends a receive that is waiting. */
	struct sigaction action = { .sa_handler = on_signal };
	struct canfd_frame frame;
	struct dcan_rx_info info;
	long wanted = argc > 2 ? strtol(argv[2], NULL, 10) : 0;
	long seen = 0;
	dcan_raw *sock;
	int n, i;

	if (argc < 2) {
		fprintf(stderr, "usage: dump <interface> [count [id:mask]]\n");
		return 2;
	}
	sock = dcan_raw_open(argv[1]);
	if (sock == NULL) {
		fprintf(stderr, "dump: %s\n", dcan_last_error());
		return 1;
	}
	if (argc > 3) {
		struct can_filter filter;

		if (sscanf(argv[3], "%x:%x", &filter.can_id, &filter.can_mask) != 2) {
			fprintf(stderr, "dump: filter is <id>:<mask> in hex\n");
			return 2;
		}
		if (dcan_raw_set_filters(sock, &filter, 1) < 0) {
			fprintf(stderr, "dump: %s\n", dcan_last_error());
			return 1;
		}
	}
	/* Bus state changes and adapter errors, too. */
	if (dcan_raw_set_err_filter(sock, CAN_ERR_MASK) < 0) {
		fprintf(stderr, "dump: %s\n", dcan_last_error());
		return 1;
	}
	sigemptyset(&action.sa_mask);
	sigaction(SIGINT, &action, NULL);
	sigaction(SIGTERM, &action, NULL);
	fprintf(stderr, "listening on %s\n", argv[1]);

	while (!stop && (wanted == 0 || seen < wanted)) {
		/* Not without limit: a signal that arrives between the test
		 * of the flag above and the wait here would otherwise go
		 * unnoticed until the next frame. */
		n = dcan_raw_recv(sock, &frame, &info, 250);
		if (n == 0 || n == DCAN_ERROR_INTERRUPTED)
			continue;
		if (n < 0) {
			fprintf(stderr, "dump: %s\n", dcan_last_error());
			/* What the daemon refuses (-100 and below) is about
			 * one frame, such as one sent on this socket that
			 * could not go out, and the socket carries on; except
			 * that the adapter was unplugged. Anything else is the
			 * connection or the library, and will not get better. */
			if (n > -100 || n == DCAN_ERROR_INTERFACE_REMOVED)
				break;
			continue;
		}
		seen++;
		printf("(%llu.%06llu) %s ",
		       (unsigned long long)(info.timestamp_ns / 1000000000),
		       (unsigned long long)(info.timestamp_ns % 1000000000 / 1000),
		       argv[1]);
		if (frame.can_id & CAN_ERR_FLAG) {
			printf("error frame, classes %08X\n", frame.can_id & CAN_ERR_MASK);
			continue;
		}
		if (frame.can_id & CAN_EFF_FLAG)
			printf("%08X", frame.can_id & CAN_EFF_MASK);
		else
			printf("%03X", frame.can_id & CAN_SFF_MASK);
		if (frame.can_id & CAN_RTR_FLAG) {
			printf(" remote, %d bytes asked for\n", frame.len);
			continue;
		}
		printf(n == (int)CANFD_MTU ? " [%02d]" : " [%d]", frame.len);
		for (i = 0; i < frame.len; i++)
			printf(" %02X", frame.data[i]);
		if (n == (int)CANFD_MTU && (frame.flags & CANFD_BRS))
			printf("  (bit rate switched)");
		printf("\n");
	}
	fflush(stdout);
	dcan_raw_close(sock);
	return 0;
}
