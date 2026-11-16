# Security

## What darwinCAN protects, and what it does not

`darwincand` runs as the logged-in user, without root. It listens on a Unix socket that only that user can open: the socket is created with mode 0600, and by default it lives in `~/Library/Application Support/darwincan`, a directory other users cannot enter.

Within that boundary there is no further access control, by design:

- **Any program running as you can use every CAN interface.** It can bring an interface up, transmit on it, take it down while other programs are using it, or rename it. This is the same trust that SocketCAN places in a user who has been given access to a CAN device on Linux.
- **A CAN bus has no authentication of its own.** A program that can transmit can send any frame. If the adapter is connected to a vehicle or a machine, treat every program you run on the Mac as able to command it. Use an adapter's listen-only mode when you only need to observe, and note that not every adapter has one (see [docs/hardware.md](docs/hardware.md)).
- **A socket in a shared directory is not protected.** If you point `DARWINCAN_SOCKET` or `--socket` at a place such as `/tmp`, another user of the Mac can create a file there first and keep the daemon from starting, or run something else at that path for your programs to connect to. Use such paths only for throwaway tests.

The daemon is written so that a faulty or hostile client cannot crash it or stall the bus for other clients, and so that malformed data from a USB adapter cannot crash it. The limits that back this up (on filters, jobs, message sizes, queue lengths and the amount of message data held) are described in [docs/design.md](docs/design.md). The gaps that are known and not yet closed are listed under "Robustness work still to do" in [docs/roadmap.md](docs/roadmap.md).

## Reporting a vulnerability

Please report security problems privately, through GitHub's "Report a vulnerability" button on the repository's Security tab, rather than in a public issue. Include what you did, what happened, and the versions of macOS and darwinCAN involved.

darwinCAN is at version 0.1 and is maintained on a best-effort basis. Only the latest version receives fixes.
