# SPDX-License-Identifier: MIT OR Apache-2.0
"""J1939 sockets against a real daemon; see test_bus.py for what it needs."""

import select
import threading
import time
import unittest

import can

from darwincan import DarwincanBus, J1939Error, J1939Message, J1939Notice, J1939Socket

CHANNEL = "vcan0"


def payload(length):
    return bytes((n * 7 + n // 251) % 256 for n in range(length))


class J1939Test(unittest.TestCase):
    def socket(self, addr=None, **options):
        sock = J1939Socket(CHANNEL, addr, **options)
        self.addCleanup(sock.close)
        return sock

    def test_a_message_reaches_the_address_it_is_sent_to(self):
        a = self.socket(0x10)
        b = self.socket(0x11)
        other = self.socket(0x12)
        before = time.time()
        a.send_to(b"\x01\x02\x03", pgn=0xEF00, addr=0x11)
        m = b.recv(timeout=5)
        self.assertIsInstance(m, J1939Message)
        self.assertEqual(m.data, b"\x01\x02\x03")
        self.assertEqual((m.pgn, m.src_addr, m.dst_addr), (0xEF00, 0x10, 0x11))
        self.assertEqual((m.priority, m.size, m.src_name, m.dst_name), (6, 3, 0, 0))
        self.assertGreater(m.timestamp, before - 1)
        self.assertLess(m.timestamp, time.time() + 1)
        self.assertIsNone(other.recv(timeout=0.05))
        self.assertIsNone(a.recv(timeout=0))

    def test_long_messages_cross_whole_and_notices_tell_of_them(self):
        a = self.socket(0x14, notices=True)
        b = self.socket(0x15, notices=True)
        # One frame, the transport protocol, and the extended one.
        for length in (8, 9, 1785, 4000):
            data = payload(length)
            a.send_to(data, pgn=0xEF00, addr=0x15)
            kinds = []
            while True:
                got = b.recv(timeout=10)
                if isinstance(got, J1939Message):
                    break
                self.assertIsInstance(got, J1939Notice, length)
                self.assertEqual((got.src_addr, got.dst_addr, got.size), (0x14, 0x15, length))
                kinds.append((got.kind, got.acked))
            self.assertEqual(got.data, data, length)
            self.assertEqual(got.size, length)
            expected = []
            if length > 8:
                expected.append((J1939Socket.NOTICE_RX_RTS, 0))
            if length > 1785:
                # 572 packets, in blocks of 255.
                expected += [(J1939Socket.NOTICE_RX_DPO, n * 7) for n in (0, 255, 510)]
            self.assertEqual(kinds, expected, length)
            done = a.recv(timeout=10)
            self.assertIsInstance(done, J1939Notice)
            self.assertEqual(done.kind, J1939Socket.NOTICE_TX_DONE)
            self.assertEqual((done.size, done.acked, done.pgn), (length, length, 0xEF00))

    def test_a_message_longer_than_the_buffer_is_cut(self):
        a = self.socket(0x16)
        b = self.socket(0x17, max_rx_len=100)
        a.send_to(payload(300), pgn=0xEF00, addr=0x17)
        m = b.recv(timeout=5)
        self.assertEqual(m.data, payload(300)[:100])
        self.assertEqual(m.size, 300)

    def test_broadcasts_need_allowing_both_ways(self):
        a = self.socket(0x18)
        quiet = self.socket(0x19)
        listener = self.socket(0x1A, broadcast=True)
        with self.assertRaises(J1939Error) as refused:
            a.send_to(b"\x01", pgn=0xFEF1)
        self.assertEqual(refused.exception.error_code, J1939Socket.ERROR_NOT_PERMITTED)
        a.set_broadcast(True)
        with self.assertRaises(J1939Error) as refused:
            a.send_to(payload(1786), pgn=0xFEB0)
        self.assertEqual(refused.exception.error_code, J1939Socket.ERROR_TOO_BIG)
        a.send_to(payload(20), pgn=0xFEB0)
        m = listener.recv(timeout=5)
        self.assertEqual(m.data, payload(20))
        self.assertEqual((m.pgn, m.src_addr, m.dst_addr), (0xFEB0, 0x18, J1939Socket.NO_ADDR))
        self.assertIsNone(quiet.recv(timeout=0.05))

    def test_priorities_for_the_socket_and_for_one_message(self):
        a = self.socket(0x1C, priority=4)
        b = self.socket(0x1D)
        a.connect(0x1D, pgn=0xEF00)
        a.send(b"\x01")
        a.send(b"\x02", priority=1)
        a.send_to(b"\x03", pgn=0xEF00, addr=0x1D, priority=0)
        # A long message keeps its priority too.
        a.send_to(payload(50), pgn=0xEF00, addr=0x1D, priority=2)
        a.set_send_priority(7)
        a.send(b"\x05")
        got = [b.recv(timeout=5).priority for _ in range(5)]
        self.assertEqual(got, [4, 1, 0, 2, 7])
        for bad in (-1, 8):
            with self.assertRaises(ValueError):
                a.send(b"\x01", priority=bad)
            with self.assertRaises(ValueError):
                a.set_send_priority(bad)

    def test_what_is_received_can_be_narrowed(self):
        a = self.socket(0x20)
        other = self.socket(0x21)
        # By the parameter group the socket is bound to.
        bound = self.socket(0x22, pgn=0xAA00)
        a.send_to(b"\x01", pgn=0xEF00, addr=0x22)
        a.send_to(b"\x02", pgn=0xAA00, addr=0x22)
        self.assertEqual(bound.recv(timeout=5).data, b"\x02")
        # By filters, on the sender and on the parameter group.
        filtered = self.socket(0x23, filters=[{"addr": 0x21}, {"pgn": 0xBB00}])
        a.send_to(b"\x03", pgn=0xEF00, addr=0x23)
        other.send_to(b"\x04", pgn=0xEF00, addr=0x23)
        a.send_to(b"\x05", pgn=0xBB00, addr=0x23)
        self.assertEqual([filtered.recv(timeout=5).data for _ in range(2)], [b"\x04", b"\x05"])
        filtered.set_filters(None)
        a.send_to(b"\x06", pgn=0xEF00, addr=0x23)
        self.assertEqual(filtered.recv(timeout=5).data, b"\x06")
        with self.assertRaises(ValueError):
            filtered.set_filters([{"sa": 1}])
        with self.assertRaises(ValueError):
            filtered.set_filters([{"addr": 0x100}])
        # By connecting, to one sender.
        connected = self.socket(0x24)
        connected.connect(0x21)
        a.send_to(b"\x07", pgn=0xEF00, addr=0x24)
        other.send_to(b"\x08", pgn=0xEF00, addr=0x24)
        self.assertEqual(connected.recv(timeout=5).data, b"\x08")
        # And widened: a promiscuous socket hears what is for others.
        spy = self.socket(promisc=True, filters=[{"addr": 0x20, "pgn": 0xCC00}])
        a.send_to(b"\x09", pgn=0xCC00, addr=0x21)
        m = spy.recv(timeout=5)
        self.assertEqual((m.data, m.dst_addr), (b"\x09", 0x21))

    def test_a_name_stands_for_the_address_it_claimed(self):
        engine, brakes = 0x8000_0000_0000_1111, 0x8000_0000_0000_2222
        a = self.socket(0x28, name=engine, broadcast=True)
        b = self.socket(0x29, name=brakes, broadcast=True)
        with self.assertRaises(J1939Error) as refused:
            a.send_to(b"\x01", pgn=0xEF00, name=brakes)
        self.assertEqual(
            refused.exception.error_code, J1939Socket.ERROR_ADDRESS_NOT_AVAILABLE
        )
        # Claiming is the program's part: the NAME, low byte first, to everyone.
        a.send_to(engine.to_bytes(8, "little"), pgn=J1939Socket.PGN_ADDRESS_CLAIMED)
        b.send_to(brakes.to_bytes(8, "little"), pgn=J1939Socket.PGN_ADDRESS_CLAIMED)
        claim = b.recv(timeout=5)
        self.assertEqual((claim.pgn, claim.src_addr, claim.src_name), (0xEE00, 0x28, engine))
        a.recv(timeout=5)
        # A claim has to stand for a quarter of a second.
        deadline = time.monotonic() + 5
        while True:
            try:
                a.send_to(b"\x02", pgn=0xEF00, name=brakes)
                break
            except J1939Error as e:
                self.assertEqual(e.error_code, J1939Socket.ERROR_ADDRESS_NOT_AVAILABLE)
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.02)
        m = b.recv(timeout=5)
        self.assertEqual((m.data, m.src_addr, m.dst_addr), (b"\x02", 0x28, 0x29))
        self.assertEqual((m.src_name, m.dst_name), (engine, brakes))
        # Moving to another address: bind again and claim it.
        a.bind(0x2A, name=engine)
        a.send_to(engine.to_bytes(8, "little"), pgn=J1939Socket.PGN_ADDRESS_CLAIMED)
        self.assertEqual(b.recv(timeout=5).src_addr, 0x2A)

    def test_a_long_message_nobody_answers_is_given_up(self):
        a = self.socket(0x2C, notices=True)
        begun = time.monotonic()
        a.send_to(payload(100), pgn=0xEF00, addr=0x2D)
        notice = a.recv(timeout=10)
        self.assertEqual(notice.kind, J1939Socket.NOTICE_TX_ABORT)
        self.assertEqual(notice.reason, J1939Socket.ABORT_TIMEOUT)
        self.assertGreaterEqual(time.monotonic() - begun, 1.25)

    def test_sends_that_cannot_be_made_and_bad_arguments(self):
        listener = self.socket()
        with self.assertRaises(J1939Error) as refused:
            listener.send_to(b"\x01", pgn=0xEF00, addr=0x30)
        self.assertEqual(refused.exception.error_code, J1939Socket.ERROR_INVALID_REQUEST)
        self.assertIsInstance(refused.exception, can.CanOperationError)
        a = self.socket(0x31)
        with self.assertRaises(J1939Error):
            a.send(b"\x01")  # not connected
        with self.assertRaises(J1939Error):
            a.send_to(b"\x01", pgn=0xEF07, addr=0x30)  # not a parameter group
        for bad in ({"addr": 0x100}, {"addr": -1}, {"name": 1 << 64}, {"pgn": 0x40001}):
            with self.assertRaises(ValueError):
                J1939Socket(CHANNEL, **bad)
        with self.assertRaises(ValueError):
            a.send_to(b"\x01", pgn=0xEF00, addr=0x100)
        with self.assertRaises(ValueError):
            J1939Socket(CHANNEL, 0x31, max_rx_len=0)
        with self.assertRaises(can.CanInitializationError):
            J1939Socket("nosuch0", 0x31)

    def test_a_raw_bus_sees_the_frames(self):
        bus = DarwincanBus(
            channel=CHANNEL,
            can_filters=[{"can_id": 0x00EF3534, "can_mask": 0x00FFFFFF, "extended": True}],
        )
        self.addCleanup(bus.shutdown)
        a = self.socket(0x34)
        a.send_to(b"\xAA\xBB", pgn=0xEF00, addr=0x35, priority=3)
        frame = bus.recv(timeout=5)
        self.assertEqual(frame.arbitration_id, 0x0CEF3534)
        self.assertEqual(bytes(frame.data), b"\xAA\xBB")

    def test_waiting_closing_and_the_descriptor(self):
        a = self.socket(0x38)
        b = self.socket(0x39)
        begun = time.monotonic()
        self.assertIsNone(b.recv(timeout=0.2))
        self.assertGreaterEqual(time.monotonic() - begun, 0.2)
        self.assertEqual(select.select([b.fileno()], [], [], 0)[0], [])
        a.send_to(b"\x01", pgn=0xEF00, addr=0x39)
        self.assertEqual(select.select([b.fileno()], [], [], 5)[0], [b.fileno()])
        self.assertEqual(b.recv(timeout=0).data, b"\x01")
        # A receive in another thread ends when the socket is closed.
        failed = []

        def wait():
            try:
                b.recv()
            except can.CanOperationError as e:
                failed.append(e)

        thread = threading.Thread(target=wait)
        thread.start()
        time.sleep(0.1)
        b.close()
        thread.join(timeout=5)
        self.assertFalse(thread.is_alive())
        self.assertEqual(len(failed), 1)
        with self.assertRaises(can.CanOperationError):
            b.send_to(b"\x01", pgn=0xEF00, addr=0x38)
        with J1939Socket(CHANNEL, 0x3A) as c:
            c.send_to(b"\x01", pgn=0xEF00, addr=0x38)
        with self.assertRaises(can.CanOperationError):
            c.recv(timeout=0)


if __name__ == "__main__":
    unittest.main()
