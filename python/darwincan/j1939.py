# SPDX-License-Identifier: MIT OR Apache-2.0
"""SAE J1939 sockets, run by the daemon."""

import ctypes
import time
from ctypes import byref
from typing import Iterable, Mapping, NamedTuple, Optional, Union

import can

from . import _lib
from ._lib import lib


class J1939Error(can.CanOperationError):
    """The daemon refused a request, or the connection to it failed.
    ``error_code`` is the C library's: one of the ``ERROR_*`` values of
    :class:`J1939Socket` for a send that cannot be made."""


class J1939Message(NamedTuple):
    """A received message: one frame's worth, or one put together from a
    transport protocol session."""

    data: bytes
    pgn: int  #: the parameter group number
    src_addr: int
    dst_addr: int  #: ``J1939Socket.NO_ADDR`` if it was sent to everyone
    priority: int  #: 0 (highest) to 7
    timestamp: float  #: of its last frame, in seconds since the epoch
    src_name: int  #: the sender's NAME once it has claimed its address, else 0
    dst_name: int
    size: int  #: its whole length, which ``data`` is cut to ``max_rx_len`` of


class J1939Notice(NamedTuple):
    """News of a long message, for a socket opened with ``notices=True``:
    what Linux puts in a J1939 socket's error queue."""

    kind: int  #: one of the ``NOTICE_*`` values of :class:`J1939Socket`
    pgn: int
    src_addr: int
    dst_addr: int
    size: int  #: the whole length of the message
    acked: int  #: how many of its bytes have arrived, or been acknowledged
    reason: int  #: for the two aborts, one of the ``ABORT_*`` values
    timestamp: float
    src_name: int
    dst_name: int


class J1939Socket:
    """A J1939 socket: what a ``CAN_J1939`` socket is on Linux. It sends and
    receives whole messages of any length; the daemon builds the identifiers
    and runs the transport protocols (BAM, RTS/CTS and the extended one), so
    their timing does not depend on Python.

    One thread may receive while others send.

    :param channel: The interface's name.
    :param addr:
        The address to send from and receive for. Without one, and without
        a ``name``, the socket only listens.
    :param name:
        A 64-bit NAME: the socket then uses whatever address that NAME has
        claimed. Claiming is the program's part, or ``j1939acd``'s.
    :param pgn: Receive this parameter group only.
    :param broadcast:
        Allow sending to everyone, and receive what is sent to everyone.
    :param promisc: Receive whatever is on the bus, whoever it is for.
    :param priority: The priority of what is sent: 0 (highest) to 7; 6
        unless given.
    :param notices: Have ``recv`` return :class:`J1939Notice` objects too.
    :param filters: As for :meth:`set_filters`.
    :param max_rx_len:
        The most bytes of one message that are handed over, 1 MiB by
        default. A longer message is cut; its ``size`` says how long it was.
    :param socket_path: The daemon's socket, as for the bus.
    """

    NO_ADDR = 0xFF  #: no address; as a destination, everyone
    IDLE_ADDR = 0xFE  #: the address of a node that could not claim one
    NO_NAME = 0
    NO_PGN = 0x40000
    PGN_REQUEST = 0x0EA00
    PGN_ADDRESS_CLAIMED = 0x0EE00
    PGN_ADDRESS_COMMANDED = 0x0FED8
    PGN_MAX = 0x3FFFF

    NOTICE_TX_DONE = 1  #: a message sent is out, or acknowledged
    NOTICE_TX_ABORT = 2  #: a message sent was given up
    NOTICE_RX_RTS = 3  #: a long message begins to arrive
    NOTICE_RX_ABORT = 4  #: a long message was given up part-way
    NOTICE_RX_DPO = 5  #: a message over 1785 bytes is still arriving

    ABORT_BUSY = 1
    ABORT_RESOURCES = 2
    ABORT_TIMEOUT = 3
    ABORT_CTS_WHILE_DT = 4
    ABORT_RETRANSMIT = 5
    ABORT_UNEXPECTED_DT = 6
    ABORT_BAD_SEQ = 7
    ABORT_DUP_SEQ = 8
    ABORT_TOO_BIG = 9
    ABORT_OTHER = 250

    ERROR_INVALID_REQUEST = -102  #: nothing to send from or to, or not a PGN
    ERROR_INTERFACE_DOWN = -106
    ERROR_ADDRESS_NOT_AVAILABLE = -109  #: a NAME has no claimed address
    ERROR_NOT_PERMITTED = -110  #: to everyone, without ``broadcast``
    ERROR_TOO_BIG = -111

    def __init__(
        self,
        channel: str,
        addr: Optional[int] = None,
        *,
        name: Optional[int] = None,
        pgn: Optional[int] = None,
        broadcast: bool = False,
        promisc: bool = False,
        priority: Optional[int] = None,
        notices: bool = False,
        filters: Optional[Iterable[Mapping[str, int]]] = None,
        max_rx_len: int = 1 << 20,
        socket_path=None,
    ):
        if not 0 < max_rx_len <= 0xFFFFFFFF:
            raise ValueError(f"max_rx_len is out of range: {max_rx_len!r}")
        bound = self._address(addr, name, pgn)
        self._lib = lib()
        self.channel = channel
        pointer = self._lib.dcan_j1939_open_at(
            _lib.path_arg(socket_path), channel.encode(), byref(bound)
        )
        if not pointer:
            raise can.CanInitializationError(
                f"could not open a J1939 socket on {channel}: {_lib.last_error()}"
            )
        self._sock = _lib.Handle(pointer, self._lib.dcan_j1939_close, "the J1939 socket")
        # One thread receives at a time, so one buffer serves.
        self._buffer = ctypes.create_string_buffer(max_rx_len)
        self._info = _lib.dcan_j1939_info()
        try:
            if broadcast:
                self.set_broadcast(True)
            if promisc:
                self.set_promisc(True)
            if priority is not None:
                self.set_send_priority(priority)
            if notices:
                self.set_notices(True)
            if filters is not None:
                self.set_filters(filters)
        except Exception:
            self._sock.close()
            raise

    @classmethod
    def _address(cls, addr, name, pgn) -> "_lib.dcan_j1939_addr":
        # ctypes would cut a value that does not fit its field without a
        # word, and the message would go somewhere else than asked.
        for what, value, most in (
            ("addr", addr, 0xFF),
            ("name", name, 0xFFFFFFFFFFFFFFFF),
            ("pgn", pgn, cls.NO_PGN),
        ):
            if value is not None and not 0 <= value <= most:
                raise ValueError(f"{what} must be 0 to {most:#x}, not {value!r}")
        return _lib.dcan_j1939_addr(
            name=cls.NO_NAME if name is None else name,
            pgn=cls.NO_PGN if pgn is None else pgn,
            addr=cls.NO_ADDR if addr is None else addr,
        )

    @staticmethod
    def _priority(priority: int) -> int:
        if not 0 <= priority <= 7:
            raise ValueError(f"a priority is 0 to 7, not {priority!r}")
        return priority

    def _check(self, result: int) -> None:
        if result < 0:
            raise J1939Error(_lib.last_error(), error_code=result)

    def bind(
        self,
        addr: Optional[int] = None,
        *,
        name: Optional[int] = None,
        pgn: Optional[int] = None,
    ) -> None:
        """Bind the socket anew, as at opening: to move to another address."""
        bound = self._address(addr, name, pgn)
        with self._sock as sock:
            self._check(self._lib.dcan_j1939_bind(sock, byref(bound)))

    def connect(
        self,
        addr: Optional[int] = None,
        *,
        name: Optional[int] = None,
        pgn: Optional[int] = None,
    ) -> None:
        """Set where :meth:`send` sends and with which parameter group. The
        socket then receives from that address or NAME only."""
        peer = self._address(addr, name, pgn)
        with self._sock as sock:
            self._check(self._lib.dcan_j1939_connect(sock, byref(peer)))

    def send(self, data, *, priority: Optional[int] = None) -> None:
        """Send a message to where the socket is connected.

        Returns once the daemon has taken it. How a message of more than
        eight bytes fares after that is told in notices.

        :raises J1939Error: It cannot be sent; see its ``error_code``.
        """
        data = bytes(data)
        with self._sock as sock:
            if priority is None:
                result = self._lib.dcan_j1939_send(sock, data, len(data))
            else:
                result = self._lib.dcan_j1939_sendto_prio(
                    sock, data, len(data), None, self._priority(priority)
                )
            self._check(result)

    def send_to(
        self,
        data,
        pgn: int,
        addr: Optional[int] = None,
        *,
        name: Optional[int] = None,
        priority: Optional[int] = None,
    ) -> None:
        """Send a message of parameter group ``pgn`` to ``addr``, to the
        address ``name`` has claimed, or, given neither, to everyone (which
        needs ``broadcast``). Up to 8 bytes go as one frame, more through
        the transport protocols: up to 1785 bytes to everyone, up to 117 MB
        to one destination.

        :param priority: For this message only, in place of the socket's.
        :raises J1939Error: It cannot be sent; see its ``error_code``.
        """
        data = bytes(data)
        to = self._address(addr, name, pgn)
        with self._sock as sock:
            if priority is None:
                result = self._lib.dcan_j1939_sendto(sock, data, len(data), byref(to))
            else:
                result = self._lib.dcan_j1939_sendto_prio(
                    sock, data, len(data), byref(to), self._priority(priority)
                )
            self._check(result)

    def recv(
        self, timeout: Optional[float] = None
    ) -> Union[J1939Message, J1939Notice, None]:
        """Receive one message, or None if none came within ``timeout``
        seconds; without a timeout, waits. On a socket with notices, a
        :class:`J1939Notice` may come in its place, in order with the
        messages."""
        deadline = None if timeout is None else time.monotonic() + timeout
        info = self._info
        while True:
            wait = _lib.wait_ms(deadline, time.monotonic())
            with self._sock as sock:
                result = self._lib.dcan_j1939_recv(
                    sock, self._buffer, len(self._buffer), byref(info), wait
                )
                if result < 0 and result != _lib.DCAN_ERROR_INTERRUPTED:
                    self._check(result)
            if result > 0:
                timestamp = info.timestamp_ns / 1e9
                if info.notice:
                    return J1939Notice(
                        kind=info.notice,
                        pgn=info.src.pgn,
                        src_addr=info.src.addr,
                        dst_addr=info.dst_addr,
                        size=info.size,
                        acked=info.acked,
                        reason=info.reason,
                        timestamp=timestamp,
                        src_name=info.src.name,
                        dst_name=info.dst_name,
                    )
                return J1939Message(
                    data=ctypes.string_at(self._buffer, min(info.size, len(self._buffer))),
                    pgn=info.src.pgn,
                    src_addr=info.src.addr,
                    dst_addr=info.dst_addr,
                    priority=info.priority,
                    timestamp=timestamp,
                    src_name=info.src.name,
                    dst_name=info.dst_name,
                    size=info.size,
                )
            if result == 0 and deadline is not None and time.monotonic() >= deadline:
                return None

    def set_filters(self, filters: Optional[Iterable[Mapping[str, int]]]) -> None:
        """Replace the receive filters: with any set, a message is received
        only if one of them matches its sender and parameter group. Each is
        a mapping with any of ``pgn``, ``addr`` and ``name``, compared in
        full, or under ``pgn_mask``, ``addr_mask`` and ``name_mask`` if
        those are given. None or an empty list removes the filters."""
        fields = {"pgn": 0x3FFFF, "addr": 0xFF, "name": 0xFFFFFFFFFFFFFFFF}
        filters = list(filters or [])
        array = (_lib.j1939_filter * max(len(filters), 1))()
        for entry, given in zip(array, filters):
            unknown = set(given) - set(fields) - {f"{f}_mask" for f in fields}
            if unknown:
                raise ValueError(f"not part of a J1939 filter: {sorted(unknown)}")
            for field, full in fields.items():
                value = given.get(field)
                mask = given.get(f"{field}_mask", 0 if value is None else full)
                for what, number in ((field, value or 0), (f"{field}_mask", mask)):
                    if not 0 <= number <= full:
                        raise ValueError(f"{what} must be 0 to {full:#x}, not {number!r}")
                    setattr(entry, what, number)
        with self._sock as sock:
            self._check(self._lib.dcan_j1939_set_filters(sock, array, len(filters)))

    def set_broadcast(self, on: bool) -> None:
        """Allow sending to everyone and receiving what is sent to everyone."""
        with self._sock as sock:
            self._check(self._lib.dcan_j1939_set_broadcast(sock, int(bool(on))))

    def set_promisc(self, on: bool) -> None:
        """Receive whatever is on the bus, whoever it is for."""
        with self._sock as sock:
            self._check(self._lib.dcan_j1939_set_promisc(sock, int(bool(on))))

    def set_send_priority(self, priority: int) -> None:
        """The priority of the messages sent: 0 (highest) to 7."""
        with self._sock as sock:
            self._check(
                self._lib.dcan_j1939_set_send_prio(sock, self._priority(priority))
            )

    def set_notices(self, on: bool) -> None:
        """Have :meth:`recv` return notices of the progress of long
        messages."""
        with self._sock as sock:
            self._check(self._lib.dcan_j1939_set_notices(sock, int(bool(on))))

    def fileno(self) -> int:
        """A descriptor to wait on with ``select``: readable exactly when
        ``recv(timeout=0)`` would return something. Never read from it."""
        with self._sock as sock:
            return self._lib.dcan_j1939_fileno(sock)

    def close(self) -> None:
        """Close the socket. What it was sending is given up. A ``recv``
        waiting in another thread ends with ``can.CanOperationError``."""
        self._sock.close()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False

    def __del__(self):
        sock = getattr(self, "_sock", None)
        if sock is not None:
            sock.close()
