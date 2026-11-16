// SPDX-License-Identifier: GPL-2.0-or-later

//! The daemon measuring its own timing, to tell which part was slow when
//! frames leave late: the adapter, the daemon's threads or the client.
//! Off unless enabled; then anything slower than a threshold is logged.

use std::fmt::Display;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use crate::backends::log;
use crate::clock::now_ns;
use crate::priority::make_current_thread_realtime;

/// Zero while timing is not being watched.
static THRESHOLD_NS: AtomicU64 = AtomicU64::new(0);

/// How often the timer check wakes.
const PERIOD: Duration = Duration::from_millis(1);

/// The time now if timing is being watched, for [`finish`].
pub(crate) fn start() -> Option<u64> {
    (THRESHOLD_NS.load(Ordering::Relaxed) != 0).then(now_ns)
}

/// Log `what` if it took longer than the threshold since [`start`].
pub(crate) fn finish<D: Display>(started: Option<u64>, what: impl FnOnce() -> D) {
    if let Some(started) = started {
        let now = now_ns();
        report(now, now.saturating_sub(started), what);
    }
}

/// Log `what` if `late_ns`, worked out by the caller, is over the
/// threshold.
pub(crate) fn late<D: Display>(late_ns: u64, what: impl FnOnce() -> D) {
    if THRESHOLD_NS.load(Ordering::Relaxed) != 0 {
        report(now_ns(), late_ns, what);
    }
}

fn report<D: Display>(at_ns: u64, took_ns: u64, what: impl FnOnce() -> D) {
    let threshold = THRESHOLD_NS.load(Ordering::Relaxed);
    if threshold != 0 && took_ns > threshold {
        log(format!(
            "timing {}.{:03}: {} {:.1} ms",
            at_ns / 1_000_000_000,
            at_ns / 1_000_000 % 1000,
            what(),
            took_ns as f64 / 1e6
        ));
    }
}

/// Log whatever takes longer than `threshold` from now on: handing a frame
/// to an adapter, an adapter's messages reaching the daemon, and a
/// real-time thread of the daemon's being woken from a one-millisecond
/// wait. The second says whether the computer as a whole
/// held the daemon up, since such a thread is otherwise woken within tens
/// of microseconds.
pub fn watch(threshold: Duration) -> io::Result<()> {
    let threshold_ns = (threshold.as_nanos() as u64).max(1);
    THRESHOLD_NS.store(threshold_ns, Ordering::Relaxed);
    thread::Builder::new()
        .name("darwincand-timing".into())
        .spawn(|| {
            make_current_thread_realtime();
            loop {
                let due = now_ns() + PERIOD.as_nanos() as u64;
                thread::sleep(PERIOD);
                let now = now_ns();
                report(now, now.saturating_sub(due), || {
                    "a real-time thread was woken late by"
                });
            }
        })?;
    Ok(())
}
