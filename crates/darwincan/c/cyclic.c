/* SPDX-License-Identifier: MIT OR Apache-2.0 */
/*
 * cyclic.c: send a frame every few milliseconds with the broadcast
 * manager, and change its data as it goes. The daemon keeps the rhythm;
 * this program only says what to send.
 *
 *     cc cyclic.c -ldarwincan -o cyclic
 *     ./cyclic vcan0 123 10        identifier 0x123 every 10 ms, until Ctrl-C
 *     ./cyclic vcan0 123 10 3      for three seconds
 */
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#include <darwincan_bcm.h>

static volatile sig_atomic_t stop;

static void on_signal(int number)
{
	(void)number;
	stop = 1;
}

int main(int argc, char **argv)
{
	struct sigaction action = { .sa_handler = on_signal };
	struct {
		struct bcm_msg_head head;
		struct can_frame frame;
	} msg = { 0 };
	long interval_ms, seconds, second;
	dcan_bcm *jobs;

	if (argc < 4) {
		fprintf(stderr, "usage: cyclic <interface> <id> <ms> [seconds]\n");
		return 2;
	}
	interval_ms = strtol(argv[3], NULL, 10);
	seconds = argc > 4 ? strtol(argv[4], NULL, 10) : 0;
	jobs = dcan_bcm_open(argv[1]);
	if (jobs == NULL) {
		fprintf(stderr, "cyclic: %s\n", dcan_last_error());
		return 1;
	}
	sigemptyset(&action.sa_mask);
	sigaction(SIGINT, &action, NULL);
	sigaction(SIGTERM, &action, NULL);

	/* Send the frame at once and then every interval, without end:
	 * count and ival1 stay 0, ival2 is the interval. */
	msg.head.opcode = TX_SETUP;
	msg.head.flags = SETTIMER | STARTTIMER;
	msg.head.ival2.tv_sec = interval_ms / 1000;
	msg.head.ival2.tv_usec = interval_ms % 1000 * 1000;
	msg.head.can_id = (canid_t)strtoul(argv[2], NULL, 16);
	msg.head.nframes = 1;
	msg.frame.can_id = msg.head.can_id;
	msg.frame.len = 2;
	if (dcan_bcm_send(jobs, &msg.head, sizeof(msg)) < 0) {
		fprintf(stderr, "cyclic: %s\n", dcan_last_error());
		return 1;
	}

	/* Once a second, new data. With no flags the timer is left alone
	 * and the next frame simply carries the new bytes. */
	msg.head.flags = 0;
	for (second = 1; !stop && (seconds == 0 || second <= seconds); second++) {
		sleep(1);
		msg.frame.data[0] = (uint8_t)(second >> 8);
		msg.frame.data[1] = (uint8_t)second;
		if (dcan_bcm_send(jobs, &msg.head, sizeof(msg)) < 0) {
			fprintf(stderr, "cyclic: %s\n", dcan_last_error());
			break;
		}
	}
	/* Closing ends the job; TX_DELETE would end it and keep the socket. */
	dcan_bcm_close(jobs);
	return 0;
}
