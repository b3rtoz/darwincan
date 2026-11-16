# darwinCAN for Python

A [python-can](https://python-can.readthedocs.io) interface for the darwincand daemon, and ISO-TP channels with the daemon's timing.

```sh
make install                 # in the repository: the daemon and the C library
pip install ./python
```

```python
import can

with can.Bus(interface="darwincan", channel="can0") as bus:
    bus.send(can.Message(arbitration_id=0x123, data=[1, 2, 3], is_extended_id=False))
    print(bus.recv(timeout=1))
```

The guide is [docs/python.md](../docs/python.md).
