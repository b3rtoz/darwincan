/* SPDX-License-Identifier: MIT OR Apache-2.0 */
/*
 * darwincan_j1939.h: SAE J1939 sockets. A socket is bound to an address
 * or a NAME, sends messages of any length to an address, a NAME or
 * everyone, and receives the messages meant for it. The transport
 * protocols for messages of more than eight bytes (BAM, RTS/CTS and the
 * extended protocol above 1785 bytes) run in the darwincand daemon on a
 * real-time thread, which answers for every address the J1939 sockets on
 * an interface have.
 *
 * The constants, types and struct j1939_filter have the names, values and
 * layout they have in Linux's <linux/can/j1939.h>, and the sockets behave
 * as CAN_J1939 sockets do. Where Linux code calls socket, bind, connect,
 * setsockopt, sendto and recvmsg, call the dcan_j1939_* functions below.
 *
 * Results, threads, signals and time are as darwincan.h describes them
 * for a raw socket.
 */
#ifndef DARWINCAN_J1939_H
#define DARWINCAN_J1939_H

#include <darwincan.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---- As in <linux/can/j1939.h> ----------------------------------------- */

#define J1939_MAX_UNICAST_ADDR 0xfd
#define J1939_IDLE_ADDR 0xfe
#define J1939_NO_ADDR 0xff /* == broadcast or no addr */
#define J1939_NO_NAME 0
#define J1939_PGN_REQUEST 0x0ea00 /* Request PG */
#define J1939_PGN_ADDRESS_CLAIMED 0x0ee00 /* Address Claimed */
#define J1939_PGN_ADDRESS_COMMANDED 0x0fed8 /* Commanded Address */
#define J1939_PGN_PDU1_MAX 0x3ff00
#define J1939_PGN_MAX 0x3ffff
#define J1939_NO_PGN 0x40000

/* A parameter group number: PDU specific in bits 0-7 (zero for a group
 * that is sent to one destination), PDU format in bits 8-15, the data
 * page in bit 16, the reserved bit in bit 17. */
typedef uint32_t pgn_t;

/* A priority: 0 (highest) to 7. */
typedef uint8_t priority_t;

/* A 64-bit NAME. */
typedef uint64_t name_t;

/* A receive filter: a message passes if its source's NAME, its parameter
 * group and its source's address each equal the filter's value under the
 * filter's mask. */
struct j1939_filter {
	name_t name;
	name_t name_mask;
	pgn_t pgn;
	pgn_t pgn_mask;
	uint8_t addr;
	uint8_t addr_mask;
};

#define J1939_FILTER_MAX 512 /* the most filters a socket takes */

/* ---- Addresses --------------------------------------------------------- */

/* One end of a message: what the can_addr.j1939 part of struct
 * sockaddr_can holds on Linux. Clear it before filling it in. */
struct dcan_j1939_addr {
	name_t name;  /* the NAME whose claimed address this is, or J1939_NO_NAME */
	pgn_t pgn;    /* the parameter group, or J1939_NO_PGN */
	uint8_t addr; /* the address, or J1939_NO_ADDR */
	uint8_t reserved[3];
};

/* ---- What dcan_j1939_recv reports -------------------------------------- */

/* The kinds of notice, in dcan_j1939_info.notice. On Linux the same news
 * comes through the socket's error queue. */
#define DCAN_J1939_MESSAGE  0 /* not a notice: a message */
#define DCAN_J1939_TX_DONE  1 /* a message sent is out, or acknowledged (SCM_TSTAMP_ACK) */
#define DCAN_J1939_TX_ABORT 2 /* a message sent was given up (J1939_EE_INFO_TX_ABORT) */
#define DCAN_J1939_RX_RTS   3 /* a long message begins to arrive (J1939_EE_INFO_RX_RTS) */
#define DCAN_J1939_RX_ABORT 4 /* a long message was given up part-way (J1939_EE_INFO_RX_ABORT) */
#define DCAN_J1939_RX_DPO   5 /* a message over 1785 bytes is still arriving: its sender has
                               * begun another block of packets, and info.acked bytes are
                               * here (J1939_EE_INFO_RX_DPO) */

/* Why, in dcan_j1939_info.reason: the reason of the connection abort
 * message of SAE J1939-21, sent or received. */
#define DCAN_J1939_ABORT_BUSY          1
#define DCAN_J1939_ABORT_RESOURCES     2
#define DCAN_J1939_ABORT_TIMEOUT       3
#define DCAN_J1939_ABORT_CTS_WHILE_DT  4
#define DCAN_J1939_ABORT_RETRANSMIT    5
#define DCAN_J1939_ABORT_UNEXPECTED_DT 6
#define DCAN_J1939_ABORT_BAD_SEQ       7
#define DCAN_J1939_ABORT_DUP_SEQ       8
#define DCAN_J1939_ABORT_TOO_BIG       9
#define DCAN_J1939_ABORT_OTHER         250 /* also: the interface could not send */

/* What comes with a message: on Linux the source address of recvfrom and
 * the SCM_J1939_DEST_ADDR, SCM_J1939_DEST_NAME and SCM_J1939_PRIO control
 * messages of recvmsg. For a notice, the message it tells of. */
struct dcan_j1939_info {
	uint64_t timestamp_ns;      /* when the message's last frame was on the bus */
	struct dcan_j1939_addr src; /* who sent it, and the parameter group */
	name_t dst_name;            /* the NAME that has claimed dst_addr, if one has */
	uint8_t dst_addr;           /* J1939_NO_ADDR: to everyone */
	priority_t priority;
	uint8_t notice;             /* DCAN_J1939_MESSAGE, or the kind of notice */
	uint8_t reason;             /* for the two aborts: DCAN_J1939_ABORT_* */
	uint32_t size;              /* the length of the whole message */
	uint32_t acked;             /* for a notice: how many of its bytes have arrived so
	                             * far, or have been acknowledged by the receiver
	                             * (J1939_NLA_BYTES_ACKED); for a message, size */
};

/* ---- Sockets ----------------------------------------------------------- */

typedef struct dcan_j1939 dcan_j1939;

/* Open a J1939 socket on an interface and bind it, as socket and bind.
 *
 * bind->addr, or the address bind->name has claimed, is what the socket
 * sends from and receives for; with bind->pgn other than J1939_NO_PGN the
 * socket receives that parameter group only. A socket bound to a NAME
 * sends once the NAME's address claim has stood for 250 ms; claiming is
 * the program's part, as on Linux: send J1939_PGN_ADDRESS_CLAIMED to
 * J1939_NO_ADDR with the NAME as its eight bytes, from a socket bound to
 * the NAME and the address wanted. bind may be NULL for a socket that
 * only listens. */
dcan_j1939 *dcan_j1939_open(const char *ifname, const struct dcan_j1939_addr *bind);
dcan_j1939 *dcan_j1939_open_at(const char *socket_path, const char *ifname,
			       const struct dcan_j1939_addr *bind);
void dcan_j1939_close(dcan_j1939 *sock);

/* Bind the socket anew: to move to another address. */
int dcan_j1939_bind(dcan_j1939 *sock, const struct dcan_j1939_addr *addr);

/* Set where dcan_j1939_send sends and with which parameter group, as
 * connect. The socket then receives from that NAME or address only. */
int dcan_j1939_connect(dcan_j1939 *sock, const struct dcan_j1939_addr *addr);

/* Send a message of len bytes: dcan_j1939_send to where the socket is
 * connected, dcan_j1939_sendto to dst->name's address or, without a NAME,
 * to dst->addr (J1939_NO_ADDR: everyone), as parameter group dst->pgn.
 * Up to 8 bytes go as one frame, more through the transport protocols:
 * up to 1785 bytes to everyone, up to 117440505 to one destination.
 *
 * Returns 0 once the daemon has taken the message. It fails at once with
 * DCAN_ERROR_INTERFACE_DOWN on an interface that is down or bus-off,
 * DCAN_ERROR_ADDRESS_NOT_AVAILABLE when a NAME, the socket's or the
 * destination's, has no claimed address, DCAN_ERROR_NOT_PERMITTED for a
 * broadcast from a socket without dcan_j1939_set_broadcast,
 * DCAN_ERROR_TOO_BIG, and DCAN_ERROR_INVALID_REQUEST for a socket bound
 * to nothing, no destination, or a pgn that is not one. How a long
 * message fares afterwards is told in notices. */
int dcan_j1939_send(dcan_j1939 *sock, const uint8_t *data, size_t len);
int dcan_j1939_sendto(dcan_j1939 *sock, const uint8_t *data, size_t len,
		      const struct dcan_j1939_addr *dst);

/* As dcan_j1939_sendto, at a priority of this message's own, 0 (highest)
 * to 7, in place of the socket's. With dst NULL the message goes to
 * where the socket is connected. Linux's documentation describes a
 * priority per socket only (SO_J1939_SEND_PRIO); this has no documented
 * counterpart there. */
int dcan_j1939_sendto_prio(dcan_j1939 *sock, const uint8_t *data, size_t len,
			   const struct dcan_j1939_addr *dst, int priority);

/* Receive a message, waiting at most timeout_ms as for dcan_raw_recv.
 * Returns 1 when one was received and 0 if none came in time.
 * info->size is its length; its first capacity bytes are in buffer.
 *
 * On a socket with dcan_j1939_set_notices, notices are returned too, in
 * order with the messages: info->notice says which, and nothing is put
 * in buffer. */
int dcan_j1939_recv(dcan_j1939 *sock, uint8_t *buffer, size_t capacity,
		    struct dcan_j1939_info *info, int timeout_ms);

/* Replace the receive filters, as SO_J1939_FILTER: with any set, a
 * message is received only if one of them matches. count 0 removes them. */
int dcan_j1939_set_filters(dcan_j1939 *sock, const struct j1939_filter *filters,
			   size_t count);

/* Receive whatever is on the bus, whoever it is for (default off), as
 * SO_J1939_PROMISC. */
int dcan_j1939_set_promisc(dcan_j1939 *sock, int on);

/* Allow sending to everyone and receiving what is sent to everyone
 * (default off), as SO_BROADCAST. */
int dcan_j1939_set_broadcast(dcan_j1939 *sock, int on);

/* The priority of the messages sent, 0 (highest) to 7 (default 6), as
 * SO_J1939_SEND_PRIO. */
int dcan_j1939_set_send_prio(dcan_j1939 *sock, int priority);

/* Have dcan_j1939_recv return notices (default off), in place of
 * SO_J1939_ERRQUEUE. */
int dcan_j1939_set_notices(dcan_j1939 *sock, int on);

/* A file descriptor to wait on, as dcan_raw_fileno. */
int dcan_j1939_fileno(const dcan_j1939 *sock);

#ifdef __cplusplus
}
#endif

#endif /* DARWINCAN_J1939_H */
