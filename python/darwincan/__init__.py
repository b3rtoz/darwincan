# SPDX-License-Identifier: MIT OR Apache-2.0
"""CAN on macOS through the darwincand daemon, for python-can.

    import can
    bus = can.Bus(interface="darwincan", channel="can0")

and, for ISO-TP with the daemon's timing,

    from darwincan import IsoTpSocket
    with IsoTpSocket("can0", tx_id=0x7E0, rx_id=0x7E8) as tester:
        tester.send(b"\\x22\\xF1\\x90")
        answer = tester.recv(timeout=1)

and for SAE J1939 messages of any length,

    from darwincan import J1939Socket
    with J1939Socket("can0", addr=0x80) as node:
        node.send_to(b"\xDA\xFE\x00", pgn=0xEA00, addr=0x00)
        message = node.recv(timeout=1)
"""

from .bus import CyclicSendTask, DarwincanBus
from .isotp import IsoTpError, IsoTpSocket
from .j1939 import J1939Error, J1939Message, J1939Notice, J1939Socket

__version__ = "0.1.0"

__all__ = [
    "CyclicSendTask",
    "DarwincanBus",
    "IsoTpError",
    "IsoTpSocket",
    "J1939Error",
    "J1939Message",
    "J1939Notice",
    "J1939Socket",
    "library_version",
]


def library_version() -> str:
    """The version of the C library in use."""
    from ._lib import lib

    return lib().dcan_version().decode()
