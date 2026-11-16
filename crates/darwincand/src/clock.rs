// SPDX-License-Identifier: GPL-2.0-only

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
mod raw {
    use std::sync::OnceLock;

    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }

    extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_continuous_time() -> u64;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }

    fn to_ns(ticks: u64) -> u64 {
        static TIMEBASE: OnceLock<(u64, u64)> = OnceLock::new();
        let (numer, denom) = *TIMEBASE.get_or_init(|| {
            let mut info = Timebase { numer: 0, denom: 0 };
            // SAFETY: the pointer refers to a live local of the layout
            // mach_timebase_info fills in.
            let ok = unsafe { mach_timebase_info(&mut info) } == 0 && info.denom != 0;
            if ok {
                (u64::from(info.numer), u64::from(info.denom))
            } else {
                (1, 1)
            }
        });
        (u128::from(ticks) * u128::from(numer) / u128::from(denom)) as u64
    }

    /// Time since boot, counting time spent asleep.
    pub fn continuous_ns() -> u64 {
        // SAFETY: takes no arguments and cannot fail.
        to_ns(unsafe { mach_continuous_time() })
    }

    /// Time since boot, not counting time spent asleep.
    pub fn awake_ns() -> u64 {
        // SAFETY: takes no arguments and cannot fail.
        to_ns(unsafe { mach_absolute_time() })
    }
}

#[cfg(not(target_os = "macos"))]
mod raw {
    use std::sync::OnceLock;
    use std::time::Instant;

    pub fn continuous_ns() -> u64 {
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_nanos() as u64
    }

    pub fn awake_ns() -> u64 {
        continuous_ns()
    }
}

struct Anchor {
    continuous_ns: u64,
    wall_ns: u64,
}

static ANCHOR: OnceLock<Anchor> = OnceLock::new();

/// Nanoseconds since the Unix epoch on a monotonic clock.
///
/// The wall clock is sampled once at startup and advanced with a monotonic
/// clock from then on, so timestamps read as real time but never jump or run
/// backwards when the system clock is adjusted. The monotonic clock keeps
/// counting while the computer sleeps, so they still read as real time
/// afterwards.
pub fn now_ns() -> u64 {
    let a = ANCHOR.get_or_init(|| Anchor {
        continuous_ns: raw::continuous_ns(),
        wall_ns: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
    });
    a.wall_ns + raw::continuous_ns().saturating_sub(a.continuous_ns)
}

/// How long the computer has spent asleep since it booted. Grows only
/// across a sleep, which is how a wake-up is noticed.
pub fn asleep_ns() -> u64 {
    raw::continuous_ns().saturating_sub(raw::awake_ns())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_advances_and_reads_as_wall_time() {
        let before = now_ns();
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        std::thread::sleep(std::time::Duration::from_millis(2));
        let after = now_ns();
        assert!(after >= before + 2_000_000);
        // Within a minute of the wall clock (it was sampled at first use).
        assert!(before.abs_diff(wall) < 60_000_000_000);
        // No sleep happens during a test run. The two clocks behind the
        // figure are read one after the other, so it can also come out a
        // little smaller the second time.
        let asleep = asleep_ns();
        assert!(asleep_ns().saturating_sub(asleep) < 1_000_000_000);
    }
}
