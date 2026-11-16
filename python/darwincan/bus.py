# SPDX-License-Identifier: MIT OR Apache-2.0
"""A python-can bus on a darwinCAN interface."""

import ctypes
import logging
import threading
import time
from ctypes import byref
from typing import Optional

import can
from can import BusABC, BusState, CanProtocol, Message
from can.broadcastmanager import (
    LimitedDurationCyclicSendTaskABC,
    ModifiableCyclicTaskABC,
    RestartableCyclicTaskABC,
)

from . import _lib
from ._lib import (
    CAN_EFF_FLAG,
    CAN_EFF_MASK,
    CAN_ERR_FLAG,
    CAN_ERR_MASK,
    CAN_RTR_FLAG,
    CAN_SFF_MASK,
    CANFD_BRS,
    CANFD_ESI,
    CANFD_MTU,
    DATA_OFFSET,
    DCAN_ANY,
    lib,
)

log = logging.getLogger("can.darwincan")


def _can_id(msg: Message) -> int:
    """The message's identifier with its flag bits, as ``can_id`` holds it."""
    can_id = msg.arbitration_id
    if msg.is_extended_id:
        can_id = (can_id & CAN_EFF_MASK) | CAN_EFF_FLAG
    else:
        can_id &= CAN_SFF_MASK
    if msg.is_remote_frame:
        can_id |= CAN_RTR_FLAG
    return can_id


def _fill(frame, msg: Message) -> None:
    """Put a message into a ``can_frame`` or a ``canfd_frame``."""
    frame.can_id = _can_id(msg)
    data = bytes(msg.data) if msg.data is not None else b""
    if len(data) > len(frame.data):
        raise ValueError(
            f"{len(data)} data bytes do not fit a "
            f"{'CAN FD' if msg.is_fd else 'classic CAN'} frame"
        )
    if msg.is_fd:
        frame.len = len(data)
        frame.flags = (CANFD_BRS if msg.bitrate_switch else 0) | (
            CANFD_ESI if msg.error_state_indicator else 0
        )
    else:
        # A remote frame has no data; its length is the number asked for.
        frame.len = msg.dlc if msg.is_remote_frame else len(data)
        if frame.len > 8:
            raise ValueError(f"a classic CAN frame has at most 8 bytes, not {msg.dlc}")
    ctypes.memmove(frame.data, data, len(data))


class CyclicSendTask(
    LimitedDurationCyclicSendTaskABC,
    ModifiableCyclicTaskABC,
    RestartableCyclicTaskABC,
):
    """Frames sent every ``period`` by the daemon's broadcast manager, on a
    real-time thread: they leave on time whether or not Python is running.

    With several messages, one is sent each period, in turn.
    """

    def __init__(self, bus, messages, period, duration=None, autostart=True):
        super().__init__(messages, period, duration)
        if not period >= 0.000001:
            raise ValueError("the period must be at least a microsecond")
        if duration is not None and not duration > 0:
            raise ValueError("the duration must be positive")
        self._bus = bus
        self._fd = self._kind(self.messages)
        self._key = (_can_id(self.messages[0]), self._fd)
        self._started = False
        if autostart:
            self.start()

    @staticmethod
    def _kind(messages) -> bool:
        fd = messages[0].is_fd
        if any(m.is_fd != fd for m in messages):
            raise ValueError("classic and CAN FD frames cannot share a cyclic task")
        if any(m.is_error_frame for m in messages):
            raise ValueError("an error frame cannot be sent")
        return fd

    def _request(self, opcode, flags, timed, messages=()):
        frame_type = _lib.canfd_frame if self._fd else _lib.can_frame
        head_size = ctypes.sizeof(_lib.bcm_msg_head)
        size = head_size + len(messages) * ctypes.sizeof(frame_type)
        buffer = ctypes.create_string_buffer(size)
        head = _lib.bcm_msg_head.from_buffer(buffer)
        head.opcode = opcode
        head.flags = flags | (_lib.CAN_FD_FRAME if self._fd else 0)
        head.can_id = self._key[0]
        head.nframes = len(messages)
        if timed:
            seconds = int(self.period)
            interval = (seconds, round((self.period - seconds) * 1e6))
            if self.duration is None:
                head.ival2.tv_sec, head.ival2.tv_usec = interval
            else:
                # So many frames, and then none: the job stays, silent,
                # until the task is stopped.
                head.count = max(1, int(self.duration / self.period))
                head.ival1.tv_sec, head.ival1.tv_usec = interval
        frames = (frame_type * len(messages)).from_buffer(buffer, head_size)
        for frame, msg in zip(frames, messages):
            _fill(frame, msg)
        self._bus._bcm_request(buffer, size)

    def start(self) -> None:
        """Start sending, or start again: the first frame leaves at once."""
        self._bus._bcm_claim(self._key, self)
        try:
            self._request(
                _lib.TX_SETUP, _lib.SETTIMER | _lib.STARTTIMER, True, self.messages
            )
        except Exception:
            if not self._started:
                self._bus._bcm_release(self._key, self)
            raise
        self._started = True

    def stop(self) -> None:
        """Stop sending. ``start`` begins again."""
        if not self._started:
            return
        self._started = False
        try:
            self._request(_lib.TX_DELETE, 0, False)
        finally:
            self._bus._bcm_release(self._key, self)

    def modify_data(self, messages) -> None:
        """Change the data that is sent, without touching the timing."""
        messages = self._check_and_convert_messages(messages)
        self._check_modified_messages(messages)
        if self._kind(messages) != self._fd or _can_id(messages[0]) != self._key[0]:
            raise ValueError("the kind of frame cannot be changed")
        self.messages = messages
        if self._started:
            self._request(_lib.TX_SETUP, 0, False, messages)


class DarwincanBus(BusABC):
    """A CAN interface of the darwincand daemon.

    :param channel:
        The interface's name (``"can0"``, ``"vcan0"``), or ``"any"`` for
        every interface: each received message then names its interface in
        ``channel``, and a message to send has to.
    :param can_filters:
        As for every python-can bus. The daemon does the filtering.
    :param receive_own_messages:
        Also receive the frames this bus sent, once they are on the bus
        and with the time they were; their ``is_rx`` is False.
    :param local_loopback:
        Whether frames sent here reach the other programs on the interface.
    :param fd:
        Receive and send CAN FD frames. Without it, as on Linux, CAN FD
        frames on the bus are not delivered and sending one fails.
    :param ignore_rx_error_frames:
        Do not deliver error frames.
    :param bitrate, data_bitrate:
        With a ``bitrate``, an interface that is down is brought up at it
        (and, for CAN FD, at ``data_bitrate``), and stays up when the bus
        is closed. One that is up is shared with other programs and is
        never reconfigured: its settings must then equal these, or the
        open fails. Without a ``bitrate`` the interface is left as it is,
        as on Linux.
    :param listen_only:
        With a ``bitrate``: bring the interface up so that it never
        transmits or acknowledges. An interface that is already up has to
        be in the mode asked for.
    :param socket_path:
        The daemon's socket; by default ``$DARWINCAN_SOCKET`` or the usual
        place.
    """

    def __init__(
        self,
        channel=None,
        can_filters=None,
        receive_own_messages: bool = False,
        local_loopback: bool = True,
        fd: bool = False,
        ignore_rx_error_frames: bool = False,
        bitrate: Optional[int] = None,
        data_bitrate: Optional[int] = None,
        listen_only: Optional[bool] = None,
        socket_path=None,
        **kwargs,
    ):
        if not channel or not isinstance(channel, str):
            raise can.CanInitializationError(
                "channel must be the name of an interface, such as 'can0'"
            )
        if kwargs:
            log.debug("options not used by darwincan: %s", ", ".join(sorted(kwargs)))
        self.channel = channel
        self.channel_info = f"darwincan channel '{channel}'"
        self._any = channel == DCAN_ANY
        self._fd = bool(fd)
        self._can_protocol = CanProtocol.CAN_FD if fd else CanProtocol.CAN_20
        self._path = _lib.path_arg(socket_path)
        self._lib = lib()
        #: Frames this bus sent that the adapter could not put on the bus.
        self.transmit_failures = 0

        self._control = None
        self._control_lock = threading.Lock()
        self._bcm = None
        self._bcm_lock = threading.Lock()
        self._bcm_jobs = {}
        self._names = {}
        self._indexes = {}
        # One thread receives at a time, so one buffer serves.
        self._rx_frame = _lib.canfd_frame()
        self._rx_info = _lib.dcan_rx_info()

        pointer = self._lib.dcan_raw_open_at(self._path, channel.encode())
        if not pointer:
            raise can.CanInitializationError(
                f"could not open {channel}: {_lib.last_error()}"
            )
        self._sock = _lib.Handle(pointer, self._lib.dcan_raw_close, "the bus")
        try:
            self._configure(bitrate, data_bitrate, listen_only)
            self._set(self._lib.dcan_raw_set_loopback, int(bool(local_loopback)))
            self._set(
                self._lib.dcan_raw_set_recv_own_msgs, int(bool(receive_own_messages))
            )
            self._set(
                self._lib.dcan_raw_set_err_filter,
                0 if ignore_rx_error_frames else CAN_ERR_MASK,
            )
            super().__init__(channel=channel, can_filters=can_filters, **kwargs)
        except BaseException:
            self._close_handles()
            raise

    # ---- Set-up ----------------------------------------------------------

    def _set(self, function, *args) -> None:
        with self._sock as sock:
            result = function(sock, *args)
        if result < 0:
            raise can.CanInitializationError(_lib.last_error(), error_code=result)

    def _interfaces(self):
        """The daemon's interfaces, as ``dcan_interface`` structs."""
        with self._control_lock:
            if self._control is None:
                pointer = self._lib.dcan_control_open_at(self._path)
                if not pointer:
                    raise can.CanOperationError(_lib.last_error())
                self._control = _lib.Handle(
                    pointer, self._lib.dcan_control_close, "the bus"
                )
            with self._control as control:
                capacity = 16
                while True:
                    found = (_lib.dcan_interface * capacity)()
                    count = self._lib.dcan_interfaces(control, found, capacity)
                    if count < 0:
                        raise can.CanOperationError(_lib.last_error(), error_code=count)
                    if count <= capacity:
                        return list(found[:count])
                    capacity = count
        return []

    def _interface(self):
        """This bus's interface, or None (also for ``"any"``)."""
        name = self.channel.encode()
        return next((i for i in self._interfaces() if i.name == name), None)

    def _configure(self, bitrate, data_bitrate, listen_only) -> None:
        """Bring the interface up as asked if it is down; if it is up, see
        that it is as asked."""
        if bitrate is None and data_bitrate is None and listen_only is None:
            return
        if self._any:
            raise can.CanInitializationError(
                "a bus on every interface cannot configure them; "
                "open the interface by name, or use canctl"
            )
        interface = self._interface()
        if interface is None or interface.kind == b"virtual":
            return  # a virtual interface has no bitrate
        if not interface.up:
            if bitrate is None:
                raise can.CanInitializationError(
                    f"{self.channel} is down, and bringing it up needs a bitrate"
                )
            if self._fd and not data_bitrate:
                raise can.CanInitializationError(
                    f"{self.channel} is down, and bringing it up for CAN FD "
                    "needs a data_bitrate"
                )
            config = _lib.dcan_bus_config(
                bitrate=int(bitrate),
                data_bitrate=int(data_bitrate or 0),
                listen_only=int(bool(listen_only)),
            )
            with self._control_lock, self._control as control:
                result = self._lib.dcan_interface_up(
                    control, self.channel.encode(), byref(config)
                )
            if result >= 0:
                return
            # Another program may have brought it up meanwhile: then it
            # is compared like any interface found up.
            message = _lib.last_error()
            interface = self._interface()
            if interface is None or not interface.up:
                raise can.CanInitializationError(
                    f"could not bring {self.channel} up: {message}", error_code=result
                )
        for wanted, actual, what in (
            (bitrate, interface.config.bitrate, "bitrate"),
            (data_bitrate, interface.config.data_bitrate, "data bitrate"),
        ):
            if wanted is not None and int(wanted) != actual:
                raise can.CanInitializationError(
                    f"{self.channel} is up with a {what} of {actual}, not {wanted}, "
                    "and other programs may be using it; take it down first "
                    f"(`canctl link set {self.channel} down`) or open it as it is"
                )
        if listen_only is not None and bool(listen_only) != bool(
            interface.config.listen_only
        ):
            raise can.CanInitializationError(
                f"{self.channel} is up and "
                + ("not listen-only" if listen_only else "listen-only")
                + "; take it down first to change that"
            )

    def _apply_filters(self, filters) -> None:
        if not filters:
            # python-can's "no filters" is everything; for the library an
            # empty list is nothing.
            filters = [{"can_id": 0, "can_mask": 0}]
        array = (_lib.can_filter * len(filters))()
        for entry, wanted in zip(array, filters):
            can_id = wanted["can_id"]
            can_mask = wanted["can_mask"]
            if "extended" in wanted:
                # Tell 11-bit and 29-bit identifiers apart as well.
                can_mask |= CAN_EFF_FLAG
                if wanted["extended"]:
                    can_id |= CAN_EFF_FLAG
                else:
                    can_id &= ~CAN_EFF_FLAG
            entry.can_id = can_id & 0xFFFFFFFF
            entry.can_mask = can_mask & 0xFFFFFFFF
        with self._sock as sock:
            result = self._lib.dcan_raw_set_filters(sock, array, len(array))
        if result < 0:
            raise can.CanOperationError(_lib.last_error(), error_code=result)

    # ---- Frames ----------------------------------------------------------

    def _recv_internal(self, timeout):
        deadline = None if timeout is None else time.monotonic() + timeout
        frame = self._rx_frame
        info = self._rx_info
        while True:
            wait = _lib.wait_ms(deadline, time.monotonic())
            with self._sock as sock:
                size = self._lib.dcan_raw_recv(sock, byref(frame), byref(info), wait)
            if size > 0:
                if size == CANFD_MTU and not self._fd:
                    continue
                return self._message(frame, info, size == CANFD_MTU), True
            if size == 0:
                if deadline is not None and time.monotonic() >= deadline:
                    return None, True
            elif size == _lib.DCAN_ERROR_TRANSMIT_FAILED:
                # About a frame sent earlier, not about receiving: a reader
                # (a Notifier, say) must not die of it.
                self.transmit_failures += 1
                log.warning("%s: %s", self.channel, _lib.last_error())
            elif size != _lib.DCAN_ERROR_INTERRUPTED:
                raise can.CanOperationError(_lib.last_error(), error_code=size)

    def _message(self, frame, info, is_fd: bool) -> Message:
        can_id = frame.can_id
        extended = bool(can_id & CAN_EFF_FLAG)
        error = bool(can_id & CAN_ERR_FLAG)
        remote = bool(can_id & CAN_RTR_FLAG)
        length = frame.len
        data = (
            None
            if remote
            else ctypes.string_at(ctypes.addressof(frame) + DATA_OFFSET, length)
        )
        return Message(
            timestamp=info.timestamp_ns / 1e9,
            arbitration_id=can_id & (CAN_EFF_MASK if extended or error else CAN_SFF_MASK),
            is_extended_id=extended,
            is_remote_frame=remote,
            is_error_frame=error,
            channel=self._name(info.ifindex) if self._any else self.channel,
            dlc=length,
            data=data,
            is_fd=is_fd,
            is_rx=not info.flags & _lib.DCAN_RX_OWN,
            bitrate_switch=is_fd and bool(frame.flags & CANFD_BRS),
            error_state_indicator=is_fd and bool(frame.flags & CANFD_ESI),
            check=False,
        )

    def _name(self, ifindex: int) -> str:
        """The name of an interface, for a bus on all of them."""
        name = self._names.get(ifindex)
        if name is None:
            self._learn()
            name = self._names.get(ifindex, str(ifindex))
        return name

    def _learn(self) -> None:
        interfaces = self._interfaces()
        self._names = {i.index: i.name.decode() for i in interfaces}
        self._indexes = {name: index for index, name in self._names.items()}

    def send(self, msg: Message, timeout: Optional[float] = None) -> None:
        """Send a message. Returns once the daemon has it, so ``timeout`` is
        never needed and not used.

        :raises can.CanOperationError:
            The interface is down, bus-off or unplugged, or the message
            cannot be sent on it. A frame the daemon took and the adapter
            could then not put on the bus is counted in
            :attr:`transmit_failures` and logged, by a later ``recv``.
        """
        if msg.is_error_frame:
            raise can.CanOperationError("an error frame cannot be sent")
        if msg.is_fd and not self._fd:
            raise can.CanOperationError(
                "a CAN FD frame needs a bus opened with fd=True"
            )
        frame = _lib.canfd_frame() if msg.is_fd else _lib.can_frame()
        try:
            _fill(frame, msg)
        except ValueError as e:
            raise can.CanOperationError(str(e)) from None
        if self._any:
            ifindex = self._index(msg.channel)
            function = (
                self._lib.dcan_raw_sendto_fd if msg.is_fd else self._lib.dcan_raw_sendto
            )
            with self._sock as sock:
                result = function(sock, byref(frame), ifindex)
        else:
            if msg.channel is not None and msg.channel != self.channel:
                raise can.CanOperationError(
                    f"the message is for {msg.channel!r}, the bus is on {self.channel!r}"
                )
            function = (
                self._lib.dcan_raw_send_fd if msg.is_fd else self._lib.dcan_raw_send
            )
            with self._sock as sock:
                result = function(sock, byref(frame))
        if result < 0:
            raise can.CanOperationError(_lib.last_error(), error_code=result)

    def _index(self, channel) -> int:
        if not isinstance(channel, str):
            raise can.CanOperationError(
                "on a bus opened on every interface a message has to name "
                "its interface in `channel`"
            )
        if channel not in self._indexes:
            self._learn()
        try:
            return self._indexes[channel]
        except KeyError:
            raise can.CanOperationError(f"no interface named {channel!r}") from None

    def flush_tx_buffer(self) -> None:
        """Wait until the daemon has handled every frame sent on this bus."""
        with self._sock as sock:
            result = self._lib.dcan_raw_flush(sock)
        if result < 0:
            raise can.CanOperationError(_lib.last_error(), error_code=result)

    def fileno(self) -> int:
        """A descriptor to wait on with ``select``: readable exactly when
        ``recv(timeout=0)`` would return something. Never read from it."""
        with self._sock as sock:
            return self._lib.dcan_raw_fileno(sock)

    # ---- Cyclic sending --------------------------------------------------

    def _send_periodic_internal(
        self, msgs, period, duration=None, autostart=True, modifier_callback=None
    ):
        if modifier_callback is not None or self._any:
            # The daemon cannot call back into Python for each frame, and a
            # broadcast manager serves one interface: let python-can send
            # these from a thread, with a thread's timing.
            return super()._send_periodic_internal(
                msgs, period, duration, autostart, modifier_callback
            )
        return CyclicSendTask(self, msgs, period, duration, autostart)

    def _bcm_claim(self, key, task) -> None:
        with self._bcm_lock:
            owner = self._bcm_jobs.setdefault(key, task)
        if owner is not task:
            raise ValueError(
                f"a cyclic task already sends identifier {key[0] & CAN_EFF_MASK:X} "
                "on this bus; stop it or change it with modify_data"
            )

    def _bcm_release(self, key, task) -> None:
        with self._bcm_lock:
            if self._bcm_jobs.get(key) is task:
                del self._bcm_jobs[key]

    def _bcm_request(self, buffer, size) -> None:
        with self._bcm_lock:
            if self._sock.closed:
                raise can.CanOperationError("the bus is closed")
            if self._bcm is None:
                pointer = self._lib.dcan_bcm_open_at(self._path, self.channel.encode())
                if not pointer:
                    raise can.CanOperationError(_lib.last_error())
                self._bcm = _lib.Handle(pointer, self._lib.dcan_bcm_close, "the bus")
            with self._bcm as bcm:
                self._bcm_reports(bcm)
                result = self._lib.dcan_bcm_send(bcm, buffer, size)
            if result < 0:
                raise can.CanOperationError(_lib.last_error(), error_code=result)

    def _bcm_reports(self, bcm) -> None:
        """Log what the broadcast manager had to say since the last request:
        a job whose frames cannot be sent says so once."""
        report = ctypes.create_string_buffer(
            ctypes.sizeof(_lib.bcm_msg_head) + CANFD_MTU
        )
        while True:
            size = self._lib.dcan_bcm_recv(bcm, report, len(report), None, 0)
            if size == 0:
                return
            if size < 0:
                if size == _lib.DCAN_ERROR_INTERRUPTED:
                    continue
                log.warning("%s: cyclic sending: %s", self.channel, _lib.last_error())
                if size > -100:
                    return  # the connection itself; nothing more will come

    # ---- State -----------------------------------------------------------

    @property
    def state(self) -> BusState:
        """``ACTIVE`` on the bus, ``PASSIVE`` listen-only, ``ERROR`` down or
        gone. Set it with ``canctl``."""
        if self._any:
            return BusState.ACTIVE
        interface = self._interface()
        if interface is None or not interface.up:
            return BusState.ERROR
        if interface.has_config and interface.config.listen_only:
            return BusState.PASSIVE
        return BusState.ACTIVE

    @state.setter
    def state(self, new_state: BusState) -> None:
        raise NotImplementedError(
            "an interface's mode is not changed while it is up; use canctl"
        )

    def _close_handles(self) -> None:
        with self._bcm_lock:
            bcm, self._bcm = self._bcm, None
            self._bcm_jobs.clear()
        if bcm is not None:
            bcm.close()
        self._sock.close()
        with self._control_lock:
            control, self._control = self._control, None
        if control is not None:
            control.close()

    def shutdown(self) -> None:
        """Stop the cyclic tasks and close the bus. A ``recv`` waiting in
        another thread ends with ``can.CanOperationError``."""
        super().shutdown()
        self._close_handles()

    @staticmethod
    def _detect_available_configs():
        try:
            library = lib()
        except can.CanInterfaceNotImplementedError:
            return []
        pointer = library.dcan_control_open_at(None)
        if not pointer:
            return []  # no daemon
        try:
            capacity = 64
            found = (_lib.dcan_interface * capacity)()
            count = library.dcan_interfaces(pointer, found, capacity)
        finally:
            library.dcan_control_close(pointer)
        return [
            {"interface": "darwincan", "channel": i.name.decode()}
            for i in found[: max(0, min(count, capacity))]
        ]
