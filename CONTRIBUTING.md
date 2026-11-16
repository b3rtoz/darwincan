# Contributing to darwinCAN

Bug reports, reports of how an adapter behaves, and pull requests are all welcome.

## Reporting a bug

Open an issue and include:

- what you ran and what happened, with the exact message;
- the macOS version, and whether the Mac is Apple silicon or Intel;
- the adapter's model and firmware version, if hardware is involved (`canctl list` shows both);
- the daemon's messages, which are in `~/Library/Logs/darwincand.log` when the daemon runs under the launch agent, and in the terminal otherwise. Running `darwincand --timing-log 3` also logs what held frames up, which helps with any problem that involves timing.

## Reporting on an adapter

Most supported adapters are listed as untested, because only a few models have been on a test bench. If you have one of them, a report that it works, or of how it fails, is a real contribution. [docs/hardware.md](docs/hardware.md#reporting-how-an-adapter-behaves) says what to include.

## Sending a pull request

For anything larger than a fix, open an issue first so that the approach can be agreed before you write it.

A pull request should:

- pass `cargo test`, `cargo clippy --all-targets` without warnings, and `cargo fmt --check`;
- come with tests: a unit test for protocol or encoding work, and an end-to-end test in `crates/darwincand/tests` for behaviour that a client can see;
- update the documentation it makes out of date (the table at the end of [docs/development.md](docs/development.md#documentation) says which);
- say what was tested and how and, for hardware work, on which adapter. "Untested on hardware" is an acceptable and useful thing to say.

[docs/development.md](docs/development.md) covers building, the layout of the source, how to add a hardware backend, and the rules that are easy to break. Please read "Rules that are easy to break" before you make your first change.

## Licensing of contributions

The project has two licences, and which one applies depends on where a change goes:

| Where | Licence |
|---|---|
| `crates/darwincand` (the daemon and its hardware backends) | GPL-2.0-only |
| Everything else: `darwincan`, `darwincan-core`, `darwincan-isotp`, `darwincan-j1939`, `darwincan-utils`, `python/` | MIT OR Apache-2.0 |

By contributing you agree that your contribution is licensed under the licence of the part it changes. Every source file says which licence applies to it in an `SPDX-License-Identifier` line at its top. Every file of the daemon is marked `GPL-2.0-only`, and every other source file `MIT OR Apache-2.0`.

Two rules follow from this split, and a pull request that breaks either cannot be merged:

- **Code derived from GPL sources goes only into `darwincand`.** A hardware backend written from a Linux kernel driver carries `SPDX-License-Identifier: GPL-2.0-only` and names the original file and its copyright holders.
- **Nothing in the permissively licensed parts may be copied or derived from GPL or LGPL code.** That includes the Linux kernel's CAN protocols, can-utils and python-can. Write from standards, header definitions and documented behaviour.
