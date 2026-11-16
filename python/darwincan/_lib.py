# SPDX-License-Identifier: MIT OR Apache-2.0
"""The darwinCAN C library, loaded with ctypes: the structs and functions of
``darwincan.h``, ``darwincan_bcm.h`` and ``darwincan_j1939.h`` that this
package uses."""

import ctypes
import ctypes.util
import os
import threading
from ctypes import (
    POINTER,
    Structure,
    c_char,
    c_char_p,
    c_int,
    c_long,
    c_size_t,
    c_uint,
    c_uint8,
    c_uint16,
    c_uint32,
    c_uint64,
    c_void_p,
)

import can

CAN_EFF_FLAG = 0x80000000
CAN_RTR_FLAG = 0x40000000
CAN_ERR_FLAG = 0x20000000
CAN_SFF_MASK = 0x000007FF
CAN_EFF_MASK = 0x1FFFFFFF
CAN_ERR_MASK = 0x1FFFFFFF

CANFD_BRS = 0x01
CANFD_ESI = 0x02

CAN_MTU = 16
CANFD_MTU = 72

DCAN_RX_OWN = 1
DCAN_ANY = "any"

DCAN_ERROR_INTERRUPTED = -3
DCAN_ERROR_ISOTP_TX = -7
DCAN_ERROR_ISOTP_RX = -8
DCAN_ERROR_TRANSMIT_FAILED = -105

DCAN_ISOTP_FD = 1
DCAN_ISOTP_BRS = 2
DCAN_ISOTP_PADDING = 4

# <linux/can/bcm.h>, as darwincan_bcm.h has it.
TX_SETUP = 1
TX_DELETE = 2
SETTIMER = 0x0001
STARTTIMER = 0x0002
CAN_FD_FRAME = 0x0800


class can_frame(Structure):
    _fields_ = [
        ("can_id", c_uint32),
        ("len", c_uint8),
        ("_pad", c_uint8),
        ("_res0", c_uint8),
        ("len8_dlc", c_uint8),
        ("data", c_uint8 * 8),
    ]


class canfd_frame(Structure):
    _fields_ = [
        ("can_id", c_uint32),
        ("len", c_uint8),
        ("flags", c_uint8),
        ("_res0", c_uint8),
        ("_res1", c_uint8),
        ("data", c_uint8 * 64),
    ]


#: Where the data bytes begin in both frame structs.
DATA_OFFSET = 8


class can_filter(Structure):
    _fields_ = [("can_id", c_uint32), ("can_mask", c_uint32)]


class dcan_rx_info(Structure):
    _fields_ = [
        ("timestamp_ns", c_uint64),
        ("flags", c_uint32),
        ("dropped", c_uint32),
        ("ifindex", c_uint32),
        ("reserved", c_uint32),
    ]


class dcan_isotp_options(Structure):
    _fields_ = [
        ("tx_id", c_uint32),
        ("rx_id", c_uint32),
        ("flags", c_uint32),
        ("tx_dl", c_uint8),
        ("pad_byte", c_uint8),
        ("block_size", c_uint8),
        ("st_min", c_uint8),
        ("n_bs_ms", c_uint32),
        ("n_cr_ms", c_uint32),
        ("max_rx_len", c_uint32),
        ("reserved", c_uint32 * 4),
    ]


class dcan_bus_config(Structure):
    _fields_ = [
        ("bitrate", c_uint32),
        ("data_bitrate", c_uint32),
        ("sample_point", c_uint16),
        ("data_sample_point", c_uint16),
        ("listen_only", c_uint8),
        ("reserved", c_uint8 * 7),
    ]


class dcan_interface(Structure):
    _fields_ = [
        ("name", c_char * 16),
        ("kind", c_char * 16),
        ("description", c_char * 128),
        ("up", c_uint8),
        ("has_config", c_uint8),
        ("reserved", c_uint8 * 2),
        ("index", c_uint32),
        ("config", dcan_bus_config),
    ]


class bcm_timeval(Structure):
    _fields_ = [("tv_sec", c_long), ("tv_usec", c_long)]


class bcm_msg_head(Structure):
    _fields_ = [
        ("opcode", c_uint32),
        ("flags", c_uint32),
        ("count", c_uint32),
        ("ival1", bcm_timeval),
        ("ival2", bcm_timeval),
        ("can_id", c_uint32),
        ("nframes", c_uint32),
    ]


class dcan_j1939_addr(Structure):
    _fields_ = [
        ("name", c_uint64),
        ("pgn", c_uint32),
        ("addr", c_uint8),
        ("_reserved", c_uint8 * 3),
    ]


class j1939_filter(Structure):
    _fields_ = [
        ("name", c_uint64),
        ("name_mask", c_uint64),
        ("pgn", c_uint32),
        ("pgn_mask", c_uint32),
        ("addr", c_uint8),
        ("addr_mask", c_uint8),
    ]


class dcan_j1939_info(Structure):
    _fields_ = [
        ("timestamp_ns", c_uint64),
        ("src", dcan_j1939_addr),
        ("dst_name", c_uint64),
        ("dst_addr", c_uint8),
        ("priority", c_uint8),
        ("notice", c_uint8),
        ("reason", c_uint8),
        ("size", c_uint32),
        ("acked", c_uint32),
    ]


assert ctypes.sizeof(can_frame) == CAN_MTU
assert ctypes.sizeof(canfd_frame) == CANFD_MTU
assert ctypes.sizeof(dcan_rx_info) == 24
assert ctypes.sizeof(dcan_isotp_options) == 44
assert ctypes.sizeof(dcan_bus_config) == 20
assert ctypes.sizeof(dcan_interface) == 188
assert ctypes.sizeof(bcm_msg_head) == 56
assert ctypes.sizeof(dcan_j1939_addr) == 16
assert ctypes.sizeof(j1939_filter) == 32
assert ctypes.sizeof(dcan_j1939_info) == 48

_FUNCTIONS = {
    "dcan_last_error": (c_char_p, []),
    "dcan_version": (c_char_p, []),
    "dcan_protocol_version": (c_int, []),
    "dcan_raw_open_at": (c_void_p, [c_char_p, c_char_p]),
    "dcan_raw_close": (None, [c_void_p]),
    "dcan_raw_send": (c_int, [c_void_p, POINTER(can_frame)]),
    "dcan_raw_send_fd": (c_int, [c_void_p, POINTER(canfd_frame)]),
    "dcan_raw_sendto": (c_int, [c_void_p, POINTER(can_frame), c_uint]),
    "dcan_raw_sendto_fd": (c_int, [c_void_p, POINTER(canfd_frame), c_uint]),
    "dcan_raw_recv": (
        c_int,
        [c_void_p, POINTER(canfd_frame), POINTER(dcan_rx_info), c_int],
    ),
    "dcan_raw_set_filters": (c_int, [c_void_p, POINTER(can_filter), c_size_t]),
    "dcan_raw_set_err_filter": (c_int, [c_void_p, c_uint32]),
    "dcan_raw_set_loopback": (c_int, [c_void_p, c_int]),
    "dcan_raw_set_recv_own_msgs": (c_int, [c_void_p, c_int]),
    "dcan_raw_flush": (c_int, [c_void_p]),
    "dcan_raw_fileno": (c_int, [c_void_p]),
    "dcan_isotp_options_init": (None, [POINTER(dcan_isotp_options), c_uint32, c_uint32]),
    "dcan_isotp_open_at": (c_void_p, [c_char_p, c_char_p, POINTER(dcan_isotp_options)]),
    "dcan_isotp_close": (None, [c_void_p]),
    "dcan_isotp_send": (c_int, [c_void_p, c_char_p, c_size_t]),
    "dcan_isotp_recv": (c_int, [c_void_p, c_void_p, c_size_t, POINTER(c_uint64), c_int]),
    "dcan_isotp_fileno": (c_int, [c_void_p]),
    "dcan_isotp_last_failure": (c_int, []),
    "dcan_control_open_at": (c_void_p, [c_char_p]),
    "dcan_control_close": (None, [c_void_p]),
    "dcan_interfaces": (c_int, [c_void_p, POINTER(dcan_interface), c_size_t]),
    "dcan_interface_up": (c_int, [c_void_p, c_char_p, POINTER(dcan_bus_config)]),
    "dcan_bcm_open_at": (c_void_p, [c_char_p, c_char_p]),
    "dcan_bcm_close": (None, [c_void_p]),
    "dcan_bcm_send": (c_int, [c_void_p, c_void_p, c_size_t]),
    "dcan_bcm_recv": (c_int, [c_void_p, c_void_p, c_size_t, POINTER(c_uint64), c_int]),
    "dcan_j1939_open_at": (c_void_p, [c_char_p, c_char_p, POINTER(dcan_j1939_addr)]),
    "dcan_j1939_close": (None, [c_void_p]),
    "dcan_j1939_bind": (c_int, [c_void_p, POINTER(dcan_j1939_addr)]),
    "dcan_j1939_connect": (c_int, [c_void_p, POINTER(dcan_j1939_addr)]),
    "dcan_j1939_send": (c_int, [c_void_p, c_char_p, c_size_t]),
    "dcan_j1939_sendto": (c_int, [c_void_p, c_char_p, c_size_t, POINTER(dcan_j1939_addr)]),
    "dcan_j1939_sendto_prio": (
        c_int,
        [c_void_p, c_char_p, c_size_t, POINTER(dcan_j1939_addr), c_int],
    ),
    "dcan_j1939_recv": (
        c_int,
        [c_void_p, c_void_p, c_size_t, POINTER(dcan_j1939_info), c_int],
    ),
    "dcan_j1939_set_filters": (c_int, [c_void_p, POINTER(j1939_filter), c_size_t]),
    "dcan_j1939_set_promisc": (c_int, [c_void_p, c_int]),
    "dcan_j1939_set_broadcast": (c_int, [c_void_p, c_int]),
    "dcan_j1939_set_send_prio": (c_int, [c_void_p, c_int]),
    "dcan_j1939_set_notices": (c_int, [c_void_p, c_int]),
    "dcan_j1939_fileno": (c_int, [c_void_p]),
}

_NAME = "libdarwincan.dylib"
_lib = None
_lib_lock = threading.Lock()


def _candidates():
    """Where the library is looked for, in order."""
    named = os.environ.get("DARWINCAN_LIBRARY")
    if named:
        # Named outright: nothing else is tried, so that a wrong name is an
        # error and not a silent use of some other copy.
        return [named]
    places = [
        os.path.join(os.path.dirname(os.path.abspath(__file__)), _NAME),
        os.path.join("/usr/local/lib", _NAME),
        os.path.join("/opt/homebrew/lib", _NAME),
        os.path.join(os.path.expanduser("~/.local/lib"), _NAME),
    ]
    found = ctypes.util.find_library("darwincan")
    if found:
        places.append(found)
    return places


def lib():
    """The loaded library. Raises ``can.CanInterfaceNotImplementedError`` if
    it is not installed."""
    global _lib
    if _lib is not None:
        return _lib
    with _lib_lock:
        if _lib is not None:
            return _lib
        tried = []
        for place in _candidates():
            if not os.path.exists(place):
                tried.append(f"{place}: not there")
                continue
            try:
                loaded = ctypes.CDLL(place)
                for name, (restype, argtypes) in _FUNCTIONS.items():
                    function = getattr(loaded, name)
                    function.restype = restype
                    function.argtypes = argtypes
            except (OSError, AttributeError) as e:
                tried.append(f"{place}: {e}")
                continue
            _lib = loaded
            return _lib
        raise can.CanInterfaceNotImplementedError(
            "the darwinCAN C library could not be loaded ("
            + "; ".join(tried)
            + "). Install it with `make install`, or name it in DARWINCAN_LIBRARY."
        )


def last_error():
    """The message of the call that just failed on this thread."""
    message = lib().dcan_last_error()
    return message.decode("utf-8", "replace") if message else "unknown error"


def path_arg(socket_path):
    """A daemon socket path as the ``*_open_at`` functions take it."""
    return None if socket_path is None else os.fsencode(socket_path)


def wait_ms(deadline, now):
    """How long one receive call may wait, in milliseconds.

    A wait is cut into pieces of a quarter of a second so that Ctrl-C and
    ``close`` from another thread are noticed; the timestamps come from the
    daemon and do not suffer from it. ``deadline`` is None for no limit.
    """
    if deadline is None:
        return 250
    left = deadline - now
    if left <= 0:
        return 0
    # Rounded up: a receive must not return early.
    return min(250, int(left * 1000) + 1)


class Handle:
    """A handle of the C library that can be closed while other threads use
    it: ``close`` waits for the calls in progress, and calls after it fail
    with ``can.CanOperationError`` instead of touching freed memory.

    Use as ``with handle as pointer: lib().dcan_...(pointer, ...)``.
    """

    def __init__(self, pointer, close, what):
        self._pointer = pointer
        self._close = close
        self._what = what
        self._users = 0
        self._idle = threading.Condition()

    def __enter__(self):
        with self._idle:
            if self._pointer is None:
                raise can.CanOperationError(f"{self._what} is closed")
            self._users += 1
            return self._pointer

    def __exit__(self, *exc):
        with self._idle:
            self._users -= 1
            if self._users == 0:
                self._idle.notify_all()
        return False

    @property
    def closed(self):
        return self._pointer is None

    def close(self):
        with self._idle:
            pointer, self._pointer = self._pointer, None
            if pointer is None:
                return
            while self._users:
                self._idle.wait()
        self._close(pointer)
