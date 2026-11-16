// SPDX-License-Identifier: GPL-2.0-only

//! The Python package against a real daemon: the tests in `python/tests`,
//! run with the C library built beside this test.
//!
//! They need a Python that has python-can and the package installed:
//!
//! ```sh
//! python3 -m venv .venv && .venv/bin/pip install -e python
//! ```
//!
//! `DARWINCAN_PYTHON` names another interpreter. Without one that can
//! import both, the test says so and does nothing.

use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::thread;

use darwincan_core::{BusConfig, Frame};
use darwincand::{Backend, Daemon};

/// An adapter that takes every setting and is wired to nothing: an
/// interface that can be down, and brought up at a bitrate.
struct Idle;

impl Backend for Idle {
    fn kind(&self) -> &str {
        "mock"
    }
    fn start(&self, _config: &BusConfig) -> io::Result<()> {
        Ok(())
    }
    fn stop(&self) -> io::Result<()> {
        Ok(())
    }
    fn transmit(&self, _frame: &Frame, _token: u32) -> io::Result<()> {
        Ok(())
    }
}

/// Socket path that removes its file when the test ends.
struct SockPath(PathBuf);

impl Drop for SockPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The interpreter to use, if there is one with what the tests need.
fn python(root: &std::path::Path) -> Option<PathBuf> {
    let named = std::env::var_os("DARWINCAN_PYTHON").map(PathBuf::from);
    let python = named
        .clone()
        .unwrap_or_else(|| root.join(".venv/bin/python"));
    let usable = Command::new(&python)
        .args(["-c", "import can, darwincan"])
        .output()
        .map(|ran| ran.status.success())
        .unwrap_or(false);
    if usable {
        return Some(python);
    }
    // An interpreter asked for by name has to work.
    assert!(
        named.is_none(),
        "{} cannot import can and darwincan",
        python.display()
    );
    None
}

#[test]
fn the_python_package_passes_its_tests() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let Some(python) = python(&root) else {
        eprintln!(
            "skipped: no Python with python-can and darwincan \
             (python3 -m venv .venv && .venv/bin/pip install -e python)"
        );
        return;
    };
    // The C library is built beside this test.
    let library = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("libdarwincan.dylib");
    assert!(library.exists(), "{} is missing", library.display());

    let path = SockPath(PathBuf::from(format!(
        "/tmp/darwincan-py-{}.sock",
        std::process::id()
    )));
    let daemon = Daemon::new();
    daemon.add_vcan("vcan0").unwrap();
    let mock: Vec<String> = (0..3)
        .map(|_| {
            daemon
                .add_can_interface("a mock adapter", Box::new(Idle))
                .name()
        })
        .collect();
    let listener = darwincand::bind(&path.0).unwrap();
    thread::spawn(move || daemon.serve(listener));

    let ran = Command::new(&python)
        .args(["-W", "error", "-m", "unittest", "discover", "-s"])
        .arg(root.join("python/tests"))
        .env("DARWINCAN_SOCKET", &path.0)
        .env("DARWINCAN_LIBRARY", &library)
        // Interfaces that are down, for the tests that bring one up.
        .env("DARWINCAN_TEST_DOWN", mock.join(","))
        // The package as it is in the tree, whatever is installed.
        .env("PYTHONPATH", root.join("python"))
        .output()
        .unwrap();
    assert!(
        ran.status.success(),
        "the Python tests failed ({}):\n{}{}",
        ran.status,
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );
    // Everything they need is here, so none may have been left out.
    let summary = String::from_utf8_lossy(&ran.stderr);
    assert!(!summary.contains("skipped"), "{summary}");
}
