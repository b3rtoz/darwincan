/* SPDX-License-Identifier: MIT OR Apache-2.0 */
/*
 * darwincan_bcm.h: the broadcast manager. Jobs that send frames
 * cyclically, and jobs that report when a received frame changes or stays
 * away, run by the darwincand daemon on a real-time thread: a cyclic
 * frame leaves on time whether or not the program is scheduled.
 *
 * Messages, operations and flags have the names, values and layout they
 * have in Linux's <linux/can/bcm.h>, and mean the same. Where Linux code
 * opens a CAN_BCM socket, connects it and calls write and read, call the
 * dcan_bcm_* functions below. The names are as short and general here as
 * there (TX_SEND, SETTIMER), which is why they have a header of their own.
 *
 * Results, threads, signals and time are as darwincan.h describes them
 * for a raw socket.
 */
#ifndef DARWINCAN_BCM_H
#define DARWINCAN_BCM_H

#include <darwincan.h>

#ifdef __cplusplus
extern "C" {
#endif

/* An interval: seconds and microseconds. */
struct bcm_timeval {
	long tv_sec;
	long tv_usec;
};

/* The head of every message to and from the broadcast manager. nframes
 * frames follow it: struct can_frame, or struct canfd_frame when flags
 * has CAN_FD_FRAME. The usual way to build a message is a struct of your
 * own with the head first and the frames after it:
 *
 *     struct {
 *             struct bcm_msg_head head;
 *             struct can_frame frame;
 *     } msg;
 *
 * frames is declared with no elements so that this compiles without a
 * warning; it is only the place where the frames begin. */
#if defined(__clang__)
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wzero-length-array"
#elif defined(__GNUC__)
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wpedantic"
#endif
struct bcm_msg_head {
	uint32_t opcode;           /* what to do, or what happened: below */
	uint32_t flags;            /* below */
	uint32_t count;            /* frames to send at ival1 */
	struct bcm_timeval ival1;  /* see the operations */
	struct bcm_timeval ival2;
	canid_t can_id;            /* the job's identifier */
	uint32_t nframes;
	struct can_frame frames[0];
};
#if defined(__clang__)
#pragma clang diagnostic pop
#elif defined(__GNUC__)
#pragma GCC diagnostic pop
#endif

enum {
	/* Requests. */
	TX_SETUP = 1, /* create or change a cyclic transmit job */
	TX_DELETE,    /* remove it */
	TX_READ,      /* ask for its settings: answered with TX_STATUS */
	TX_SEND,      /* send one frame once */
	RX_SETUP,     /* create or change a receive job */
	RX_DELETE,    /* remove it */
	RX_READ,      /* ask for its settings: answered with RX_STATUS */
	/* Reports. */
	TX_STATUS,    /* the answer to TX_READ */
	TX_EXPIRED,   /* count frames have been sent at ival1 (TX_COUNTEVT) */
	RX_STATUS,    /* the answer to RX_READ */
	RX_TIMEOUT,   /* the frame did not come within ival1 */
	RX_CHANGED    /* the frame came for the first time or changed */
};

/*
 * A transmit job (TX_SETUP) sends its frames in turn: count times every
 * ival1, then every ival2 for as long as it exists. A job is known by
 * its can_id (and CAN_FD_FRAME); TX_SETUP for one that exists changes it.
 *
 * A receive job (RX_SETUP) watches frames with its can_id. ival1 is how
 * long the frame may stay away before RX_TIMEOUT, ival2 the least time
 * between two RX_CHANGED reports. What counts as a change:
 *   - no frames given, or RX_FILTER_ID: every frame;
 *   - one frame given: its data is a mask, and a frame counts when the
 *     bits under the mask differ from the last frame's;
 *   - several frames given: the first is a mask that picks out a
 *     multiplexer within the first eight bytes, each of the others a mask
 *     that holds one multiplexer value, and a frame is compared with the
 *     last one of the same multiplexer value.
 */
#define SETTIMER           0x0001 /* take count, ival1 and ival2 from this message */
#define STARTTIMER         0x0002 /* start the timer; a transmit job sends its first frame at once */
#define TX_COUNTEVT        0x0004 /* report TX_EXPIRED when count is used up */
#define TX_ANNOUNCE        0x0008 /* send a frame now, besides the timer's */
#define TX_CP_CAN_ID       0x0010 /* give every frame the head's can_id */
#define RX_FILTER_ID       0x0020 /* watch by identifier only */
#define RX_CHECK_DLC       0x0040 /* a change of length is a change */
#define RX_NO_AUTOTIMER    0x0080 /* a frame does not restart the timeout */
#define RX_ANNOUNCE_RESUME 0x0100 /* after a timeout, report the next frame even if unchanged */
#define TX_RESET_MULTI_IDX 0x0200 /* start a sequence of frames from its first again */
#define RX_RTR_FRAME       0x0400 /* answer a remote frame with the frame given */
#define CAN_FD_FRAME       0x0800 /* the frames are struct canfd_frame */

/* A broadcast manager on one interface: what a CAN_BCM socket connected
 * to the interface is on Linux. Closing it ends its jobs. */
typedef struct dcan_bcm dcan_bcm;

dcan_bcm *dcan_bcm_open(const char *ifname);
dcan_bcm *dcan_bcm_open_at(const char *socket_path, const char *ifname);
void dcan_bcm_close(dcan_bcm *sock);

/* Make a request: size bytes at msg, a head and its nframes frames, as
 * written to a CAN_BCM socket. Returns when the daemon has carried it
 * out, or refused it with DCAN_ERROR_INVALID_REQUEST (no such job, a job
 * growing beyond the frames it was created with, an operation or a
 * combination of flags that does not exist). TX_SEND on an interface
 * that is down fails with DCAN_ERROR_INTERFACE_DOWN. */
int dcan_bcm_send(dcan_bcm *sock, const struct bcm_msg_head *msg, size_t size);

/* Receive a report into the capacity bytes at msg, waiting at most
 * timeout_ms as for dcan_raw_recv. Returns its size in bytes, which is
 * more than capacity if frames had to be left out; 0 if none came in
 * time. *timestamp_ns, if not NULL, gets the time of the frame reported,
 * or of the timeout.
 *
 * A transmit job whose frames cannot be handed to the adapter keeps
 * trying, as on Linux; here the trouble is also reported, once when it
 * begins, as DCAN_ERROR_INTERFACE_DOWN or DCAN_ERROR_TRANSMIT_FAILED. */
int dcan_bcm_recv(dcan_bcm *sock, struct bcm_msg_head *msg, size_t capacity,
		  uint64_t *timestamp_ns, int timeout_ms);

/* A file descriptor to wait on, as dcan_raw_fileno. */
int dcan_bcm_fileno(const dcan_bcm *sock);

#ifdef __cplusplus
}
#endif

#endif /* DARWINCAN_BCM_H */
