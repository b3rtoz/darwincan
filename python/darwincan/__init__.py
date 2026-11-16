# SPDX-License-Identifier: MIT OR Apache-2.0
"""CAN on macOS through the darwincand daemon, for python-can.

    import can
    bus = can.Bus(interface="darwincan", channel="can0")

and, for ISO-TP with the daemon's timing,

    from darwincan import IsoTpSocket
    with IsoTpSocket("can0", tx_id=0x7E0, rx_id=0x7E8) as tester:
        tester.send(b"\\x22\\xF1\\x90")
        answer = tester.recv(timeout=1)
"""

from .bus import CyclicSendTask, DarwincanBus
from .isotp import IsoTpError, IsoTpSocket

__version__ = "0.1.0"

__all__ = [
    "CyclicSendTask",
    "DarwincanBus",
    "IsoTpError",
    "IsoTpSocket",
    "library_version",
]


def library_version() -> str:
    """The version of the C library in use."""
    from ._lib import lib

    return lib().dcan_version().decode()
