# SPDX-License-Identifier: MIT OR Apache-2.0
"""The python-can bus against a real daemon.

Needs a daemon at $DARWINCAN_SOCKET with the virtual interface vcan0; the
Rust test `python_api` starts one and runs these.

Frames sent on one connection race requests on another, so a sender is
flushed before anything is asserted about what another bus received.
"""

import os
import select
import threading
import time
import unittest

import can

import darwincan
from darwincan import DarwincanBus

CHANNEL = "vcan0"


def std(arbitration_id, data=(), **more):
    return can.Message(
        arbitration_id=arbitration_id, data=data, is_extended_id=False, **more
    )


class BusTest(unittest.TestCase):
    def bus(self, **options):
        options.setdefault("channel", CHANNEL)
        bus = DarwincanBus(**options)
        self.addCleanup(bus.shutdown)
        return bus

    def send(self, bus, *messages):
        for message in messages:
            bus.send(message)
        bus.flush_tx_buffer()

    def test_python_can_finds_the_interface(self):
        with can.Bus(interface="darwincan", channel=CHANNEL) as bus:
            self.assertIsInstance(bus, DarwincanBus)
            self.assertEqual(bus.channel_info, "darwincan channel 'vcan0'")
            self.assertEqual(bus.protocol, can.CanProtocol.CAN_20)
            self.assertEqual(bus.state, can.BusState.ACTIVE)
        self.assertIn(
            {"interface": "darwincan", "channel": CHANNEL},
            can.detect_available_configs("darwincan"),
        )
        self.assertEqual(darwincan.library_version(), darwincan.__version__)

    def test_frames_arrive_as_they_were_sent(self):
        a, b = self.bus(), self.bus()
        before = time.time()
        self.send(
            a,
            std(0x123, [1, 2, 3]),
            can.Message(arbitration_id=0x18DAF110, data=b"\xaa" * 8, is_extended_id=True),
            std(0x1FF, is_remote_frame=True, dlc=4),
            std(0x7FF),
        )
        got = [b.recv(timeout=5) for _ in range(4)]
        self.assertTrue(all(got))
        self.assertEqual(got[0].arbitration_id, 0x123)
        self.assertEqual(bytes(got[0].data), b"\x01\x02\x03")
        self.assertEqual(got[0].dlc, 3)
        self.assertFalse(got[0].is_extended_id)
        self.assertEqual(got[0].channel, CHANNEL)
        # Sent from this machine, by another bus: not received from the
        # wire, as python-can's socketcan interface has it.
        self.assertFalse(got[0].is_rx)
        self.assertFalse(got[0].is_fd)
        self.assertEqual(got[1].arbitration_id, 0x18DAF110)
        self.assertTrue(got[1].is_extended_id)
        self.assertEqual(bytes(got[1].data), b"\xaa" * 8)
        self.assertTrue(got[2].is_remote_frame)
        self.assertEqual(got[2].dlc, 4)
        self.assertEqual(len(got[2].data), 0)
        self.assertEqual((got[3].arbitration_id, got[3].dlc), (0x7FF, 0))
        # The daemon's time: the Unix epoch, in order.
        times = [m.timestamp for m in got]
        self.assertEqual(times, sorted(times))
        self.assertGreater(times[0], before - 1)
        self.assertLess(times[-1], time.time() + 1)
        # Nothing comes back to the sender unless it asks.
        self.assertIsNone(a.recv(timeout=0))

    def test_a_receive_waits_no_longer_and_no_shorter_than_asked(self):
        bus = self.bus()
        started = time.monotonic()
        self.assertIsNone(bus.recv(timeout=0.05))
        waited = time.monotonic() - started
        self.assertGreaterEqual(waited, 0.05)
        self.assertLess(waited, 2)

    def test_own_messages_come_back_marked(self):
        a = self.bus(receive_own_messages=True)
        self.send(a, std(0x321, [9]))
        own = a.recv(timeout=5)
        self.assertEqual(own.arbitration_id, 0x321)
        self.assertFalse(own.is_rx)

    def test_without_loopback_other_programs_do_not_see_a_frame(self):
        a, b = self.bus(local_loopback=False), self.bus()
        self.send(a, std(0x100))
        a.shutdown()
        c = self.bus()
        self.send(c, std(0x101))
        self.assertEqual(b.recv(timeout=5).arbitration_id, 0x101)

    def test_fd_frames_need_an_fd_bus(self):
        a, plain, fd = self.bus(fd=True), self.bus(), self.bus(fd=True)
        self.assertEqual(a.protocol, can.CanProtocol.CAN_FD)
        self.send(
            a,
            std(0x200, bytes(range(12)), is_fd=True, bitrate_switch=True),
            std(0x201, [1]),
        )
        first = fd.recv(timeout=5)
        self.assertTrue(first.is_fd)
        self.assertTrue(first.bitrate_switch)
        self.assertFalse(first.error_state_indicator)
        self.assertEqual(first.dlc, 12)
        self.assertEqual(bytes(first.data), bytes(range(12)))
        self.assertFalse(fd.recv(timeout=5).is_fd)
        # The bus without fd is not given the CAN FD frame, as on Linux.
        self.assertEqual(plain.recv(timeout=5).arbitration_id, 0x201)
        with self.assertRaises(can.CanOperationError):
            plain.send(std(0x200, bytes(12), is_fd=True))
        # A length no CAN FD frame has.
        with self.assertRaises(can.CanOperationError):
            a.send(std(0x200, bytes(9), is_fd=True))

    def test_filters_are_applied_by_the_daemon(self):
        a = self.bus()
        b = self.bus(can_filters=[{"can_id": 0x100, "can_mask": 0x7F0}])
        self.send(a, std(0x200), std(0x105))
        self.assertEqual(b.recv(timeout=5).arbitration_id, 0x105)

        # The same number as an 11-bit and as a 29-bit identifier.
        b.set_filters([{"can_id": 0x105, "can_mask": 0x7FF, "extended": True}])
        self.send(
            a,
            std(0x105),
            can.Message(arbitration_id=0x105, is_extended_id=True, data=[1]),
        )
        got = b.recv(timeout=5)
        self.assertTrue(got.is_extended_id)

        # No filters is everything again.
        b.set_filters(None)
        self.send(a, std(0x7AB))
        self.assertEqual(b.recv(timeout=5).arbitration_id, 0x7AB)

    def test_a_bus_on_every_interface_names_the_interface(self):
        everything = self.bus(channel="any")
        a = self.bus()
        self.send(a, std(0x111))
        got = everything.recv(timeout=5)
        self.assertEqual((got.arbitration_id, got.channel), (0x111, CHANNEL))

        self.send(everything, std(0x112, channel=CHANNEL))
        self.assertEqual(a.recv(timeout=5).arbitration_id, 0x112)
        with self.assertRaises(can.CanOperationError):
            everything.send(std(0x113))
        with self.assertRaises(can.CanOperationError):
            everything.send(std(0x113, channel="nosuch0"))
        # A bus on one interface refuses a message meant for another.
        with self.assertRaises(can.CanOperationError):
            a.send(std(0x113, channel="can9"))

    def test_what_cannot_be_opened_or_sent_is_refused(self):
        with self.assertRaises(can.CanInitializationError):
            DarwincanBus(channel="nosuch0")
        with self.assertRaises(can.CanInitializationError):
            DarwincanBus(channel=None)
        with self.assertRaises(can.CanInitializationError):
            DarwincanBus(channel=CHANNEL, socket_path="/tmp/darwincan-nobody.sock")
        bus = self.bus()
        with self.assertRaises(can.CanOperationError):
            bus.send(std(0x1, is_error_frame=True))
        with self.assertRaises(can.CanOperationError):
            bus.send(std(0x1, bytes(9)))
        # A virtual interface has no bitrate: one given changes nothing.
        self.bus(bitrate=500000, listen_only=False).shutdown()
        with self.assertRaises(can.CanInitializationError):
            DarwincanBus(channel="any", bitrate=500000)
        with self.assertRaises(NotImplementedError):
            bus.state = can.BusState.PASSIVE

    def test_the_descriptor_is_readable_when_a_frame_waits(self):
        a, b = self.bus(), self.bus()
        self.assertEqual(select.select([b], [], [], 0)[0], [])
        self.send(a, std(0x150))
        self.assertEqual(select.select([b], [], [], 5)[0], [b])
        self.assertEqual(b.recv(timeout=0).arbitration_id, 0x150)
        self.assertEqual(select.select([b], [], [], 0)[0], [])

    def test_a_notifier_delivers_frames(self):
        a, b = self.bus(), self.bus()
        reader = can.BufferedReader()
        notifier = can.Notifier(b, [reader], timeout=0.05)
        try:
            self.send(a, std(0x160, [6]))
            got = reader.get_message(timeout=5)
        finally:
            notifier.stop()
        self.assertEqual((got.arbitration_id, bytes(got.data)), (0x160, b"\x06"))

    def test_closing_ends_a_receive_in_another_thread(self):
        bus = DarwincanBus(channel=CHANNEL)
        outcome = []

        def receive():
            try:
                outcome.append(bus.recv())
            except can.CanOperationError as e:
                outcome.append(e)

        reader = threading.Thread(target=receive)
        reader.start()
        bus.shutdown()
        reader.join(5)
        self.assertFalse(reader.is_alive())
        self.assertIsInstance(outcome[0], can.CanOperationError)
        with self.assertRaises(can.CanOperationError):
            bus.send(std(0x1))
        bus.shutdown()  # twice does no harm


@unittest.skipUnless(
    os.environ.get("DARWINCAN_TEST_DOWN"),
    "needs three interfaces that are down, named in DARWINCAN_TEST_DOWN",
)
class BringUpTest(unittest.TestCase):
    """A bus with a bitrate brings its interface up, and never changes one
    that is up. Each test has an interface of its own, since they stay up."""

    def setUp(self):
        self.names = os.environ["DARWINCAN_TEST_DOWN"].split(",")

    def bus(self, channel, **options):
        bus = DarwincanBus(channel=channel, **options)
        self.addCleanup(bus.shutdown)
        return bus

    def refused(self, channel, **options):
        with self.assertRaises(can.CanInitializationError) as raised:
            DarwincanBus(channel=channel, **options)
        return str(raised.exception)

    def test_a_bitrate_brings_a_down_interface_up_and_it_stays_up(self):
        name = self.names[0]
        # Without a bitrate the interface is left as it is: down.
        idle = self.bus(name)
        self.assertEqual(idle.state, can.BusState.ERROR)
        with self.assertRaises(can.CanOperationError):
            idle.send(std(0x1))
        self.assertIn("needs a bitrate", self.refused(name, listen_only=True))
        self.assertEqual(idle.state, can.BusState.ERROR)

        first = DarwincanBus(channel=name, bitrate=250000)
        self.assertEqual(first.state, can.BusState.ACTIVE)
        idle.send(std(0x1))
        first.shutdown()
        self.assertEqual(idle.state, can.BusState.ACTIVE)

        # Up is shared: the same settings open, others are refused.
        self.bus(name, bitrate=250000, listen_only=False)
        self.assertIn("250000, not 500000", self.refused(name, bitrate=500000))
        self.assertIn(
            "data bitrate", self.refused(name, bitrate=250000, data_bitrate=2000000)
        )
        self.assertIn(
            "not listen-only", self.refused(name, bitrate=250000, listen_only=True)
        )
        self.assertEqual(idle.state, can.BusState.ACTIVE)

    def test_listen_only_is_asked_for_and_kept(self):
        name = self.names[1]
        bus = self.bus(name, bitrate=500000, listen_only=True)
        self.assertEqual(bus.state, can.BusState.PASSIVE)
        self.bus(name, listen_only=True)
        self.assertIn("is up and listen-only", self.refused(name, listen_only=False))

    def test_fd_needs_a_data_bitrate(self):
        name = self.names[2]
        self.assertIn("data_bitrate", self.refused(name, fd=True, bitrate=500000))
        bus = self.bus(name, fd=True, bitrate=500000, data_bitrate=2000000)
        self.assertEqual(bus.state, can.BusState.ACTIVE)
        self.bus(name, fd=True, bitrate=500000, data_bitrate=2000000)


class CyclicTest(unittest.TestCase):
    def bus(self, **options):
        bus = DarwincanBus(channel=CHANNEL, **options)
        self.addCleanup(bus.shutdown)
        return bus

    def collect(self, bus, count):
        got = [bus.recv(timeout=5) for _ in range(count)]
        self.assertTrue(all(got))
        return got

    def assert_paced(self, messages, period):
        """Frames after the first lie on a grid; none of them is early."""
        times = [m.timestamp for m in messages]
        for earlier, later in zip(times, times[1:]):
            self.assertGreater(later - earlier, period * 0.95)
            self.assertLess(later - earlier, 1)

    def test_the_daemon_sends_on_time_and_takes_new_data(self):
        a = self.bus()
        b = self.bus(can_filters=[{"can_id": 0x300, "can_mask": 0x7FF}])
        task = a.send_periodic(std(0x300, [0, 0]), 0.02)
        self.assertIsInstance(task, darwincan.CyclicSendTask)
        first = self.collect(b, 6)
        self.assert_paced(first, 0.02)
        self.assertTrue(all(bytes(m.data) == b"\x00\x00" for m in first))

        # A second task for the same identifier would fight the first.
        with self.assertRaises(ValueError):
            a.send_periodic(std(0x300, [5]), 0.05)

        task.modify_data(std(0x300, [0, 1]))
        # Frames sent before the change may still be on their way.
        later = self.collect(b, 4)
        self.assertEqual(bytes(later[-1].data), b"\x00\x01")
        self.assert_paced(later, 0.02)

        task.stop()
        while b.recv(timeout=0):
            pass
        # Stopped is stopped: the request was acknowledged.
        self.assertIsNone(b.recv(timeout=0.1))

        task.start()
        again = self.collect(b, 3)
        self.assertEqual(bytes(again[0].data), b"\x00\x01")
        self.assert_paced(again, 0.02)

    def test_a_duration_ends_the_sending(self):
        a = self.bus()
        b = self.bus(can_filters=[{"can_id": 0x310, "can_mask": 0x7FF}])
        a.send_periodic(std(0x310, [7]), 0.01, duration=0.05)
        self.assert_paced(self.collect(b, 5), 0.01)
        self.assertIsNone(b.recv(timeout=0.1))

    def test_several_messages_take_turns_and_fd_frames_go_too(self):
        a = self.bus(fd=True)
        b = self.bus(fd=True, can_filters=[{"can_id": 0x320, "can_mask": 0x7FF}])
        frames = [std(0x320, bytes([n] * 12), is_fd=True) for n in range(3)]
        a.send_periodic(frames, 0.01)
        got = self.collect(b, 6)
        self.assertEqual([m.data[0] for m in got], [0, 1, 2, 0, 1, 2])
        self.assertTrue(all(m.is_fd and m.dlc == 12 for m in got))
        self.assert_paced(got, 0.01)
        with self.assertRaises(ValueError):
            a.send_periodic([std(0x321), std(0x321, is_fd=True)], 0.01)

    def test_closing_the_bus_stops_its_tasks(self):
        a = DarwincanBus(channel=CHANNEL)
        b = self.bus(can_filters=[{"can_id": 0x330, "can_mask": 0x7FF}])
        a.send_periodic(std(0x330), 0.01)
        self.collect(b, 2)
        a.shutdown()
        while b.recv(timeout=0):
            pass
        self.assertIsNone(b.recv(timeout=0.1))

    def test_a_callback_per_frame_runs_in_python(self):
        a = self.bus()
        b = self.bus(can_filters=[{"can_id": 0x340, "can_mask": 0x7FF}])

        def count_up(message):
            message.data[0] += 1

        task = a.send_periodic(std(0x340, [0]), 0.01, modifier_callback=count_up)
        self.assertNotIsInstance(task, darwincan.CyclicSendTask)
        got = self.collect(b, 3)
        task.stop()
        values = [m.data[0] for m in got]
        self.assertEqual(values, [values[0], values[0] + 1, values[0] + 2])


if __name__ == "__main__":
    unittest.main()
