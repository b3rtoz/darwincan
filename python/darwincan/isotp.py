# SPDX-License-Identifier: MIT OR Apache-2.0
"""ISO-TP (ISO 15765-2) channels, run by the daemon."""

import ctypes
import time
from ctypes import byref
from typing import Optional, Tuple

import can

from . import _lib
from ._lib import CAN_EFF_FLAG, lib


class IsoTpError(can.CanOperationError):
    """A transfer failed. ``failure`` says why: one of the ``FAILURE_*``
    values of :class:`IsoTpSocket`, or 0 if the library gave no reason."""

    def __init__(self, message: str, failure: int, error_code: int):
        super().__init__(message, error_code=error_code)
        self.failure = failure


class IsoTpSocket:
    """An ISO-TP channel with normal addressing: what a ``CAN_ISOTP`` socket
    is on Linux. Segmentation, flow control and STmin run in the daemon, so
    their timing does not depend on Python.

    One thread may receive while others send.

    :param channel: The interface's name.
    :param tx_id: The identifier this end sends with.
    :param rx_id: The identifier it receives with.
    :param extended_id: Both identifiers are 29-bit.
    :param fd: Send CAN FD frames.
    :param bitrate_switch: With the data bitrate.
    :param tx_dl: Frame length: 8, or with ``fd`` 12 to 64.
    :param padding: A byte to fill frames up with, or None for no padding.
    :param block_size: Announced to the peer; 0 for no limit.
    :param st_min:
        Announced to the peer, as on the wire: 0x00 to 0x7F are
        milliseconds, 0xF1 to 0xF9 are 100 to 900 microseconds.
    :param n_bs: Seconds to wait for the peer's flow control.
    :param n_cr: Seconds to wait for the peer's next frame.
    :param max_rx_len: The longest message accepted, 1 MiB by default.
    :param socket_path: The daemon's socket, as for the bus.
    """

    FAILURE_TIMEOUT = 1  #: the peer did not answer
    FAILURE_SEQUENCE = 2  #: a frame out of sequence
    FAILURE_OVERFLOW = 3  #: the peer has no room
    FAILURE_INVALID_LENGTH = 4  #: empty or too long
    FAILURE_INVALID_FLOW_CONTROL = 5
    FAILURE_BUSY = 6  #: already sending
    FAILURE_ABORTED = 7  #: cut off by a new message
    FAILURE_WAIT_LIMIT = 8  #: too many wait frames
    FAILURE_NOT_SENT = 9  #: a frame did not reach the bus

    def __init__(
        self,
        channel: str,
        tx_id: int,
        rx_id: int,
        *,
        extended_id: bool = False,
        fd: bool = False,
        bitrate_switch: bool = False,
        tx_dl: Optional[int] = None,
        padding: Optional[int] = None,
        block_size: int = 0,
        st_min: int = 0,
        n_bs: Optional[float] = None,
        n_cr: Optional[float] = None,
        max_rx_len: Optional[int] = None,
        socket_path=None,
    ):
        # ctypes would cut a value that does not fit its field without a
        # word, and the channel would open with other settings than asked.
        for name, value, most in (
            ("tx_id", tx_id, 0x1FFFFFFF if extended_id else 0x7FF),
            ("rx_id", rx_id, 0x1FFFFFFF if extended_id else 0x7FF),
            ("tx_dl", tx_dl, 64),
            ("padding", padding, 0xFF),
            ("block_size", block_size, 0xFF),
            ("st_min", st_min, 0xFF),
            ("max_rx_len", max_rx_len, 0xFFFFFFFF),
        ):
            if value is not None and not 0 <= value <= most:
                raise ValueError(f"{name} must be 0 to {most:#x}, not {value!r}")
        for name, seconds in (("n_bs", n_bs), ("n_cr", n_cr)):
            if seconds is not None and not 0 <= seconds * 1000 <= 0xFFFFFFFF:
                raise ValueError(f"{name} is out of range: {seconds!r}")
        self._lib = lib()
        self.channel = channel
        flag = CAN_EFF_FLAG if extended_id else 0
        options = _lib.dcan_isotp_options()
        self._lib.dcan_isotp_options_init(byref(options), tx_id | flag, rx_id | flag)
        if fd:
            options.flags |= _lib.DCAN_ISOTP_FD
        if bitrate_switch:
            options.flags |= _lib.DCAN_ISOTP_BRS
        if padding is not None:
            options.flags |= _lib.DCAN_ISOTP_PADDING
            options.pad_byte = padding
        if tx_dl is not None:
            options.tx_dl = tx_dl
        options.block_size = block_size
        options.st_min = st_min
        if n_bs is not None:
            options.n_bs_ms = round(n_bs * 1000)
        if n_cr is not None:
            options.n_cr_ms = round(n_cr * 1000)
        if max_rx_len is not None:
            options.max_rx_len = max_rx_len
        pointer = self._lib.dcan_isotp_open_at(
            _lib.path_arg(socket_path), channel.encode(), byref(options)
        )
        if not pointer:
            raise can.CanInitializationError(
                f"could not open an ISO-TP channel on {channel}: {_lib.last_error()}"
            )
        self._sock = _lib.Handle(
            pointer, self._lib.dcan_isotp_close, "the ISO-TP channel"
        )
        # One thread receives at a time, so one buffer serves. It holds the
        # longest message the daemon will hand over.
        self._buffer = ctypes.create_string_buffer(options.max_rx_len)
        self._timestamp = ctypes.c_uint64()

    def _fail(self, result: int):
        failure = 0
        if result in (_lib.DCAN_ERROR_ISOTP_TX, _lib.DCAN_ERROR_ISOTP_RX):
            failure = self._lib.dcan_isotp_last_failure()
        raise IsoTpError(_lib.last_error(), failure, result)

    def send(self, data) -> None:
        """Send one message and wait until the peer has all of it.

        :raises IsoTpError: The transfer failed; see its ``failure``.
        """
        data = bytes(data)
        with self._sock as sock:
            result = self._lib.dcan_isotp_send(sock, data, len(data))
            if result < 0:
                self._fail(result)

    def recv_timestamped(
        self, timeout: Optional[float] = None
    ) -> Optional[Tuple[bytes, float]]:
        """Receive one message and the time of its last frame, in seconds
        since the epoch as ``can.Message.timestamp`` is. None if no message
        came within ``timeout`` seconds; without a timeout, waits.

        :raises IsoTpError: A message was given up part-way.
        """
        deadline = None if timeout is None else time.monotonic() + timeout
        while True:
            wait = _lib.wait_ms(deadline, time.monotonic())
            with self._sock as sock:
                size = self._lib.dcan_isotp_recv(
                    sock, self._buffer, len(self._buffer), byref(self._timestamp), wait
                )
                if size < 0 and size != _lib.DCAN_ERROR_INTERRUPTED:
                    self._fail(size)
            if size > 0:
                # A message longer than the buffer cannot come: the daemon
                # accepts none longer than max_rx_len.
                size = min(size, len(self._buffer))
                return (
                    ctypes.string_at(self._buffer, size),
                    self._timestamp.value / 1e9,
                )
            if size == 0 and deadline is not None and time.monotonic() >= deadline:
                return None

    def recv(self, timeout: Optional[float] = None) -> Optional[bytes]:
        """Receive one message, or None if none came within ``timeout``
        seconds; without a timeout, waits."""
        received = self.recv_timestamped(timeout)
        return None if received is None else received[0]

    def fileno(self) -> int:
        """A descriptor to wait on with ``select``: readable exactly when
        ``recv(timeout=0)`` would return something. Never read from it."""
        with self._sock as sock:
            return self._lib.dcan_isotp_fileno(sock)

    def close(self) -> None:
        """Close the channel. A ``recv`` waiting in another thread ends with
        ``can.CanOperationError``."""
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
