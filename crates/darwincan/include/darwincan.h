/* SPDX-License-Identifier: MIT OR Apache-2.0 */
/*
 * darwincan.h: CAN bus access on macOS through the darwincand daemon.
 *
 * Frames, filters and flag bits have the names and the layout they have in
 * Linux's <linux/can.h> and <linux/can/error.h>, so code that builds and
 * takes apart frames for SocketCAN keeps working. macOS has no CAN sockets,
 * so where Linux code calls socket, bind, read, write and setsockopt, call
 * the dcan_* functions below instead.
 *
 * Results. A function returns 0 or a positive count when it worked and a
 * negative DCAN_ERROR_* value when it did not; an open function returns
 * NULL. dcan_last_error() then gives the message, for the calling thread.
 *
 * Threads. A raw socket and an ISO-TP channel may each be used by one
 * thread that receives and any number that send, at once. A control
 * connection takes one call at a time. Nothing may still be using a
 * handle when it is closed.
 *
 * Signals. A receive that is waiting returns DCAN_ERROR_INTERRUPTED when a
 * signal handler installed without SA_RESTART has run, like read on a
 * socket. No call raises SIGPIPE; when the daemon has gone, calls fail
 * with DCAN_ERROR_IO and the handle can only be closed.
 *
 * Time. Timestamps are nanoseconds since the Unix epoch, taken by the
 * daemon (from the adapter's own clock where it has one) on a clock that
 * never steps: differences between them are exact however late a program
 * reads its frames.
 *
 * Link with -ldarwincan.
 */
#ifndef DARWINCAN_H
#define DARWINCAN_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define DCAN_VERSION "0.1.0"

/* ---- Frames, as in <linux/can.h> -------------------------------------- */

typedef uint32_t canid_t;
typedef uint32_t can_err_mask_t;

/* Bits of can_id above the identifier. */
#define CAN_EFF_FLAG 0x80000000U /* 29-bit identifier */
#define CAN_RTR_FLAG 0x40000000U /* remote frame */
#define CAN_ERR_FLAG 0x20000000U /* error frame, see CAN_ERR_* below */

#define CAN_SFF_MASK 0x000007FFU /* an 11-bit identifier */
#define CAN_EFF_MASK 0x1FFFFFFFU /* a 29-bit identifier */
#define CAN_ERR_MASK 0x1FFFFFFFU /* the error classes of an error frame */

#define CAN_MAX_DLC 8
#define CAN_MAX_DLEN 8
#define CANFD_MAX_DLC 15
#define CANFD_MAX_DLEN 64

/* A classic CAN frame. len is the number of data bytes, 0 to 8; for a
 * remote frame, the number requested. can_dlc is its older name. */
struct can_frame {
	canid_t can_id;
	union {
		uint8_t len;
		uint8_t can_dlc;
	};
	uint8_t __pad;
	uint8_t __res0;
	uint8_t len8_dlc; /* not used by darwinCAN */
	uint8_t data[CAN_MAX_DLEN] __attribute__((aligned(8)));
};

/* Bits of canfd_frame.flags. */
#define CANFD_BRS 0x01 /* data sent at the data bitrate */
#define CANFD_ESI 0x02 /* the sender is error-passive */
#define CANFD_FDF 0x04 /* set in every CAN FD frame received */

/* A CAN FD frame. len is 0 to 8, 12, 16, 20, 24, 32, 48 or 64. */
struct canfd_frame {
	canid_t can_id;
	uint8_t len;
	uint8_t flags;
	uint8_t __res0;
	uint8_t __res1;
	uint8_t data[CANFD_MAX_DLEN] __attribute__((aligned(8)));
};

#define CAN_MTU (sizeof(struct can_frame))
#define CANFD_MTU (sizeof(struct canfd_frame))

/* A receive filter: a frame passes if
 *     (received can_id & can_mask) == (can_id & can_mask).
 * With CAN_INV_FILTER set in can_id, it passes if they differ. */
struct can_filter {
	canid_t can_id;
	canid_t can_mask;
};

#define CAN_INV_FILTER 0x20000000U

/* ---- Error frames, as in <linux/can/error.h> -------------------------- */

#define CAN_ERR_DLC 8 /* an error frame has eight data bytes */

/* Error classes, in can_id beside CAN_ERR_FLAG. */
#define CAN_ERR_TX_TIMEOUT 0x00000001U
#define CAN_ERR_LOSTARB    0x00000002U
#define CAN_ERR_CRTL       0x00000004U /* controller state, in data[1] */
#define CAN_ERR_PROT       0x00000008U /* protocol violation, in data[2] */
#define CAN_ERR_TRX        0x00000010U
#define CAN_ERR_ACK        0x00000020U /* no acknowledgement */
#define CAN_ERR_BUSOFF     0x00000040U
#define CAN_ERR_BUSERROR   0x00000080U
#define CAN_ERR_RESTARTED  0x00000100U
#define CAN_ERR_CNT        0x00000200U /* counters: tx in data[6], rx in data[7] */

/* data[1] with CAN_ERR_CRTL. */
#define CAN_ERR_CRTL_UNSPEC      0x00
#define CAN_ERR_CRTL_RX_OVERFLOW 0x01
#define CAN_ERR_CRTL_TX_OVERFLOW 0x02
#define CAN_ERR_CRTL_RX_WARNING  0x04
#define CAN_ERR_CRTL_TX_WARNING  0x08
#define CAN_ERR_CRTL_RX_PASSIVE  0x10
#define CAN_ERR_CRTL_TX_PASSIVE  0x20
#define CAN_ERR_CRTL_ACTIVE      0x40

/* data[2] with CAN_ERR_PROT. */
#define CAN_ERR_PROT_UNSPEC   0x00
#define CAN_ERR_PROT_BIT      0x01
#define CAN_ERR_PROT_FORM     0x02
#define CAN_ERR_PROT_STUFF    0x04
#define CAN_ERR_PROT_BIT0     0x08
#define CAN_ERR_PROT_BIT1     0x10
#define CAN_ERR_PROT_OVERLOAD 0x20
#define CAN_ERR_PROT_ACTIVE   0x40
#define CAN_ERR_PROT_TX       0x80

/* ---- Results ----------------------------------------------------------- */

#define DCAN_ERROR_DAEMON_UNAVAILABLE (-1) /* darwincand is not running */
#define DCAN_ERROR_IO                 (-2) /* the connection to it failed */
#define DCAN_ERROR_INTERRUPTED        (-3) /* a signal ended the wait */
#define DCAN_ERROR_INVALID_ARGUMENT   (-4)
#define DCAN_ERROR_VERSION_MISMATCH   (-5) /* daemon and library differ */
#define DCAN_ERROR_PROTOCOL           (-6)
#define DCAN_ERROR_ISOTP_TX           (-7) /* see dcan_isotp_last_failure */
#define DCAN_ERROR_ISOTP_RX           (-8)
#define DCAN_ERROR_INTERNAL           (-9)
/* Refused by the daemon. */
#define DCAN_ERROR_NO_SUCH_INTERFACE  (-101)
#define DCAN_ERROR_INVALID_REQUEST    (-102)
#define DCAN_ERROR_ALREADY_EXISTS     (-103)
#define DCAN_ERROR_UNSUPPORTED        (-104)
#define DCAN_ERROR_TRANSMIT_FAILED    (-105) /* a frame sent earlier was not put on the bus */
#define DCAN_ERROR_INTERFACE_DOWN     (-106)
#define DCAN_ERROR_INTERFACE_REMOVED  (-107) /* the adapter was unplugged */
#define DCAN_ERROR_BUSY               (-108) /* the interface is up */
#define DCAN_ERROR_ADDRESS_NOT_AVAILABLE (-109) /* J1939: a NAME without an address */
#define DCAN_ERROR_NOT_PERMITTED      (-110) /* J1939: a broadcast not allowed */
#define DCAN_ERROR_TOO_BIG            (-111) /* a message longer than can be carried */
#define DCAN_ERROR_DAEMON_OTHER       (-199)

/* The message of the last call that failed on this thread. It stays valid
 * until the next call that fails on the same thread. */
const char *dcan_last_error(void);

/* The library's version (DCAN_VERSION when header and library match) and
 * the version of the daemon protocol it speaks. A daemon that speaks
 * another is refused with DCAN_ERROR_VERSION_MISMATCH. */
const char *dcan_version(void);
int dcan_protocol_version(void);

/* ---- Raw sockets ------------------------------------------------------- */

/* A raw CAN socket on one interface: what a CAN_RAW socket bound to the
 * interface is on Linux. It receives classic and CAN FD frames alike. */
typedef struct dcan_raw dcan_raw;

/* What the daemon knows about a received frame besides its contents. */
struct dcan_rx_info {
	uint64_t timestamp_ns;
	uint32_t flags;   /* DCAN_RX_* */
	uint32_t dropped; /* frames this socket has lost so far because the
			     program read too slowly, as SO_RXQ_OVFL counts */
	uint32_t ifindex; /* the interface the frame was on */
	uint32_t reserved;
};

#define DCAN_RX_OWN 1U /* this socket sent the frame itself */

/* The interface name that opens a socket on every interface, those that
 * appear later included: what binding to interface index 0 is on Linux.
 * Each frame received says in dcan_rx_info.ifindex where it was, and
 * sending takes dcan_raw_sendto. */
#define DCAN_ANY "any"

/* Open a socket on an interface ("can0", "vcan0", DCAN_ANY). socket_path
 * names the daemon's socket; NULL, like dcan_raw_open, means
 * $DARWINCAN_SOCKET or the usual place. */
dcan_raw *dcan_raw_open(const char *ifname);
dcan_raw *dcan_raw_open_at(const char *socket_path, const char *ifname);
void dcan_raw_close(dcan_raw *sock);

/* Send a frame. Returns once the daemon has it, like write on Linux.
 *
 * On an interface that is down or bus-off it fails at once with
 * DCAN_ERROR_INTERFACE_DOWN, and on one whose adapter was unplugged with
 * DCAN_ERROR_INTERFACE_REMOVED. A frame the daemon took and then could
 * not hand to the adapter, because nothing on the bus acknowledges and
 * the adapter is full, is reported by a later dcan_raw_recv on this
 * socket as DCAN_ERROR_TRANSMIT_FAILED. */
int dcan_raw_send(dcan_raw *sock, const struct can_frame *frame);
int dcan_raw_send_fd(dcan_raw *sock, const struct canfd_frame *frame);

/* Send on the interface with index ifindex, as sendto with a
 * sockaddr_can does: for a socket open on every interface. A failure is
 * reported by a later dcan_raw_recv. */
int dcan_raw_sendto(dcan_raw *sock, const struct can_frame *frame,
		    unsigned int ifindex);
int dcan_raw_sendto_fd(dcan_raw *sock, const struct canfd_frame *frame,
		       unsigned int ifindex);

/* Receive a frame, waiting at most timeout_ms: negative waits without
 * limit, 0 takes only what has arrived.
 *
 * Returns CAN_MTU for a classic frame, and the start of *frame is then a
 * struct can_frame; CANFD_MTU for a CAN FD frame; 0 if none came in time.
 * info may be NULL. */
int dcan_raw_recv(dcan_raw *sock, struct canfd_frame *frame,
		  struct dcan_rx_info *info, int timeout_ms);

/* Replace the receive filters. A frame is received if any filter passes
 * it; with count 0, none is. A new socket receives everything. */
int dcan_raw_set_filters(dcan_raw *sock, const struct can_filter *filters,
			 size_t count);

/* Whether a frame must pass every filter to be received, not just one of
 * them (default: 0), as CAN_RAW_JOIN_FILTERS. */
int dcan_raw_set_join_filters(dcan_raw *sock, int on);

/* Receive error frames of these classes (CAN_ERR_*; CAN_ERR_MASK for all,
 * 0 for none, the default). */
int dcan_raw_set_err_filter(dcan_raw *sock, can_err_mask_t mask);

/* Whether frames sent here reach the other local sockets on the interface
 * (default: 1). */
int dcan_raw_set_loopback(dcan_raw *sock, int on);

/* Whether this socket also receives the frames it sent, once they are on
 * the bus and with the time they were, marked DCAN_RX_OWN (default: 0). */
int dcan_raw_set_recv_own_msgs(dcan_raw *sock, int on);

/* Wait until the daemon has handled every frame sent on this socket. */
int dcan_raw_flush(dcan_raw *sock);

/* A file descriptor to wait on with poll, select or kqueue. It is readable
 * exactly when dcan_raw_recv with a timeout of 0 would return something,
 * so it can be used as a socket's descriptor is used on Linux: wait, then
 * receive.
 *
 * It is not the socket: never read, write or close it, and do not take
 * it into a child process. It stays valid until dcan_raw_close. */
int dcan_raw_fileno(const dcan_raw *sock);

/* ---- ISO-TP (ISO 15765-2) ---------------------------------------------- */

/* An ISO-TP channel with normal addressing: what a CAN_ISOTP socket is on
 * Linux. Segmentation, flow control and STmin run in the daemon. */
typedef struct dcan_isotp dcan_isotp;

#define DCAN_ISOTP_FD      1U /* send CAN FD frames */
#define DCAN_ISOTP_BRS     2U /* with bit rate switching */
#define DCAN_ISOTP_PADDING 4U /* fill frames up with pad_byte */

struct dcan_isotp_options {
	canid_t tx_id;       /* sent with; CAN_EFF_FLAG for 29 bits */
	canid_t rx_id;       /* received with */
	uint32_t flags;      /* DCAN_ISOTP_* */
	uint8_t tx_dl;       /* frame length: 8, or with FD 12 to 64 */
	uint8_t pad_byte;
	uint8_t block_size;  /* announced to the peer; 0 for no limit */
	uint8_t st_min;      /* announced to the peer, as on the wire:
				0x00-0x7F ms, 0xF1-0xF9 for 100-900 us */
	uint32_t n_bs_ms;    /* wait for the peer's flow control */
	uint32_t n_cr_ms;    /* wait for the peer's next frame */
	uint32_t max_rx_len; /* longest message accepted */
	uint32_t reserved[4];
};

/* Fill *options with the defaults for classic CAN: 8-byte frames, no
 * padding, no block size or STmin, timeouts of one second. Always start
 * from this and then change fields. */
void dcan_isotp_options_init(struct dcan_isotp_options *options,
			     canid_t tx_id, canid_t rx_id);

dcan_isotp *dcan_isotp_open(const char *ifname,
			    const struct dcan_isotp_options *options);
dcan_isotp *dcan_isotp_open_at(const char *socket_path, const char *ifname,
			       const struct dcan_isotp_options *options);
void dcan_isotp_close(dcan_isotp *sock);

/* Send one message and wait until the peer has all of it. On
 * DCAN_ERROR_ISOTP_TX, dcan_isotp_last_failure says why. Messages that
 * arrive meanwhile are kept for dcan_isotp_recv, and another thread may be
 * in dcan_isotp_recv all the while. A channel carries one message at a
 * time: a second thread's send waits for the first to finish. */
int dcan_isotp_send(dcan_isotp *sock, const uint8_t *data, size_t len);

/* Receive one message into buffer, waiting at most timeout_ms as for
 * dcan_raw_recv. Returns its length, which is more than capacity if it
 * did not fit and was cut short; 0 if none came in time. *timestamp_ns,
 * if not NULL, gets the time of the message's last frame. */
int dcan_isotp_recv(dcan_isotp *sock, uint8_t *buffer, size_t capacity,
		    uint64_t *timestamp_ns, int timeout_ms);

/* A file descriptor to wait on, as dcan_raw_fileno: readable exactly when
 * dcan_isotp_recv with a timeout of 0 would return something. */
int dcan_isotp_fileno(const dcan_isotp *sock);

/* Why the last transfer failed on this thread. */
int dcan_isotp_last_failure(void);

#define DCAN_ISOTP_FAILURE_TIMEOUT              1 /* the peer did not answer */
#define DCAN_ISOTP_FAILURE_SEQUENCE             2 /* frame out of sequence */
#define DCAN_ISOTP_FAILURE_OVERFLOW             3 /* the peer has no room */
#define DCAN_ISOTP_FAILURE_INVALID_LENGTH       4 /* empty or too long */
#define DCAN_ISOTP_FAILURE_INVALID_FLOW_CONTROL 5
#define DCAN_ISOTP_FAILURE_BUSY                 6 /* already sending */
#define DCAN_ISOTP_FAILURE_ABORTED              7 /* cut off by a new message */
#define DCAN_ISOTP_FAILURE_WAIT_LIMIT           8 /* too many wait frames */
#define DCAN_ISOTP_FAILURE_NOT_SENT             9 /* a frame did not reach the bus */

/* ---- Interfaces -------------------------------------------------------- */

/* A connection for managing the daemon's interfaces, as ip link does on
 * Linux. */
typedef struct dcan_control dcan_control;

/* How a hardware interface joins the bus. */
struct dcan_bus_config {
	uint32_t bitrate;           /* bit/s */
	uint32_t data_bitrate;      /* CAN FD data phase; 0 for classic only */
	uint16_t sample_point;      /* tenths of a percent; 0 for the default */
	uint16_t data_sample_point;
	uint8_t listen_only;        /* never transmit or acknowledge */
	uint8_t reserved[7];        /* set to 0 */
};

struct dcan_interface {
	char name[16];
	char kind[16];         /* "virtual", or the adapter's driver */
	char description[128]; /* model, serial number */
	uint8_t up;            /* on the bus */
	uint8_t has_config;    /* config below is meaningful */
	uint8_t reserved[2];
	uint32_t index;        /* its number, from 1; see below */
	struct dcan_bus_config config; /* in use, or kept for the next up */
};

dcan_control *dcan_control_open(void);
dcan_control *dcan_control_open_at(const char *socket_path);
void dcan_control_close(dcan_control *control);

/* List the interfaces into out, which has room for capacity entries.
 * Returns how many there are, which may be more than capacity. */
int dcan_interfaces(dcan_control *control, struct dcan_interface *out,
		    size_t capacity);

/* Put a hardware interface on the bus with these settings; store the
 * settings of one that is down; take one off the bus. */
int dcan_interface_up(dcan_control *control, const char *name,
		      const struct dcan_bus_config *config);
int dcan_interface_configure(dcan_control *control, const char *name,
			     const struct dcan_bus_config *config);
int dcan_interface_down(dcan_control *control, const char *name);

/* Give an interface another name, as ip link set <name> name <new name>.
 * A hardware interface has to be down (DCAN_ERROR_BUSY otherwise); its
 * adapter then has the new name every time it is plugged in. Sockets
 * open on the interface stay open. */
int dcan_interface_rename(dcan_control *control, const char *name,
			  const char *new_name);

/* Create and remove virtual interfaces. */
int dcan_vcan_add(dcan_control *control, const char *name);
int dcan_vcan_delete(dcan_control *control, const char *name);

/* An interface's index and name, as if_nametoindex and if_indextoname
 * give them on Linux: 0 or NULL if there is no such interface. name has
 * room for 16 bytes. An index belongs to one interface for as long as it
 * exists, and is not given to another while the daemon runs; it is not
 * kept across a restart of the daemon, as the name is. These ask the
 * daemon at $DARWINCAN_SOCKET or the usual place. */
unsigned int dcan_if_nametoindex(const char *name);
char *dcan_if_indextoname(unsigned int ifindex, char *name);

#ifdef __cplusplus
}
#endif

#endif /* DARWINCAN_H */
