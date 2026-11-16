# SPDX-License-Identifier: MIT OR Apache-2.0
"""ISO-TP channels against a real daemon; see test_bus.py for what it needs."""

import threading
import time
import unittest

import can

from darwincan import DarwincanBus, IsoTpError, IsoTpSocket

CHANNEL = "vcan0"


class IsoTpTest(unittest.TestCase):
    def channel(self, tx_id, rx_id, **options):
        sock = IsoTpSocket(CHANNEL, tx_id, rx_id, **options)
        self.addCleanup(sock.close)
        return sock

    def test_short_and_long_messages_arrive(self):
        tester = self.channel(0x7E0, 0x7E8)
        ecu = self.channel(0x7E8, 0x7E0)
        before = time.time()
        tester.send(b"\x22\xf1\x90")
        self.assertEqual(ecu.recv(timeout=5), b"\x22\xf1\x90")

        long = bytes(n % 251 for n in range(4000))
        ecu.send(long)
        data, timestamp = tester.recv_timestamped(timeout=5)
        self.assertEqual(data, long)
        self.assertGreater(timestamp, before - 1)
        self.assertLess(timestamp, time.time() + 1)
        self.assertIsNone(tester.recv(timeout=0))

    def test_extended_identifiers_fd_and_padding(self):
        a = self.channel(
            0x18DA10F1, 0x18DAF110, extended_id=True, fd=True, tx_dl=64, padding=0xCC
        )
        b = self.channel(
            0x18DAF110, 0x18DA10F1, extended_id=True, fd=True, tx_dl=64, padding=0xCC
        )
        bus = DarwincanBus(
            channel=CHANNEL,
            fd=True,
            can_filters=[{"can_id": 0x18DA10F1, "can_mask": 0x1FFFFFFF, "extended": True}],
        )
        self.addCleanup(bus.shutdown)
        message = bytes(range(200))
        a.send(message)
        self.assertEqual(b.recv(timeout=5), message)
        first = bus.recv(timeout=5)
        self.assertTrue(first.is_fd and first.is_extended_id)
        self.assertEqual(first.dlc, 64)

    def test_stmin_is_kept_by_the_daemon(self):
        sender = self.channel(0x701, 0x702)
        self.channel(0x702, 0x701, st_min=5)
        bus = DarwincanBus(
            channel=CHANNEL, can_filters=[{"can_id": 0x701, "can_mask": 0x7FF}]
        )
        self.addCleanup(bus.shutdown)
        sender.send(bytes(48))  # a first frame and six consecutive frames
        frames = [bus.recv(timeout=5) for _ in range(7)]
        self.assertTrue(all(frames))
        consecutive = [m.timestamp for m in frames[1:]]
        for earlier, later in zip(consecutive, consecutive[1:]):
            self.assertGreaterEqual(later - earlier, 0.005)
            self.assertLess(later - earlier, 1)

    def test_a_peer_that_does_not_answer_is_a_timeout(self):
        alone = self.channel(0x711, 0x712, n_bs=0.1)
        started = time.monotonic()
        with self.assertRaises(IsoTpError) as raised:
            alone.send(bytes(100))
        waited = time.monotonic() - started
        self.assertEqual(raised.exception.failure, IsoTpSocket.FAILURE_TIMEOUT)
        self.assertIsInstance(raised.exception, can.CanOperationError)
        self.assertGreaterEqual(waited, 0.1)
        self.assertLess(waited, 3)
        # A message that fits one frame needs no answer.
        alone.send(b"\x3e\x00")

    def test_receive_waits_as_long_as_asked_and_ends_when_closed(self):
        sock = IsoTpSocket(CHANNEL, 0x721, 0x722)
        started = time.monotonic()
        self.assertIsNone(sock.recv(timeout=0.05))
        self.assertGreaterEqual(time.monotonic() - started, 0.05)

        outcome = []

        def receive():
            try:
                outcome.append(sock.recv())
            except can.CanOperationError as e:
                outcome.append(e)

        reader = threading.Thread(target=receive)
        reader.start()
        sock.close()
        reader.join(5)
        self.assertFalse(reader.is_alive())
        self.assertIsInstance(outcome[0], can.CanOperationError)

    def test_what_cannot_be_opened_is_refused(self):
        with self.assertRaises(can.CanInitializationError):
            IsoTpSocket("nosuch0", 0x1, 0x2)
        with self.assertRaises(can.CanInitializationError):
            IsoTpSocket(CHANNEL, 0x1, 0x2, tx_dl=9)
        with IsoTpSocket(CHANNEL, 0x731, 0x732) as sock:
            self.assertGreater(sock.fileno(), 0)
        with self.assertRaises(can.CanOperationError):
            sock.recv(timeout=0)


if __name__ == "__main__":
    unittest.main()
