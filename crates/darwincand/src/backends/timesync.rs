// SPDX-License-Identifier: GPL-2.0-only

//! Mapping an adapter's hardware clock onto the daemon's clock.

use std::collections::VecDeque;

/// How fast the adapter's crystal may run relative to ours, in parts per
/// million. Generous: typical crystals are within 50 ppm.
const MAX_DRIFT_PPM: f64 = 200.0;

/// A message that seems to have taken longer than this to arrive did not:
/// the device clock was set back.
const MAX_DELAY_NS: i128 = 100_000_000;

/// A message that seems to have arrived this much sooner than the fastest
/// before it did not: the device clock was set forward.
const MAX_EARLY_NS: i128 = 5_000_000;

/// The rate of the device clock is bounded from the fastest message in
/// each stretch of this length...
const WINDOW_NS: u64 = 8_000_000_000;
/// ...over this many of them (17 minutes).
const WINDOWS: usize = 128;

/// The fastest message of a window is taken to have needed no longer than
/// this to arrive. (A PCAN-USB X6 on a quiet bus takes up to 1.1 ms for
/// nearly every message.) A window whose fastest message is known to
/// have taken longer is left out.
const MAX_FASTEST_DELAY_NS: i128 = 2_000_000;
/// How far the rate may have moved since it was bounded, in parts per
/// million: a crystal's rate changes with its temperature.
const WANDER_PPM: f64 = 1.0;

/// The fastest message of one window: when it arrived and the difference
/// (host arrival time - device timestamp) it showed.
#[derive(Debug, Clone, Copy)]
struct Fastest {
    host_ns: u64,
    difference: i128,
    /// How far the difference was above the estimate. The estimate is
    /// never under the true offset, so the message took at least this
    /// long to arrive.
    delayed_ns: i128,
}

/// Converts device timestamps to daemon time.
///
/// A message is always seen by the host some time after the device stamped
/// it, never before. So of all the (host arrival time - device timestamp)
/// differences observed, the smallest is the best estimate of the offset
/// between the two clocks: it comes from the message that suffered the
/// least USB and scheduling delay.
///
/// The two clocks do not run at quite the same rate, so the offset moves,
/// and between two messages the estimate has to be allowed to creep
/// upward as fast as the offset may be growing: a timestamp must not be
/// early, because ISO-TP counts STmin from it. At first that is the
/// worst a crystal may drift. It is narrowed by what has been seen: if
/// the fastest message of some stretch showed a difference `a` and that
/// of a later one `b`, and neither took longer than `D` to arrive, the
/// offset grew by no more than `b - a + D` in between. The lowest such
/// bound over every two stretches of the last minutes is the creep.
///
/// It matters because the fastest messages can be rare. A PCAN-USB X6
/// receiving a frame every 5 ms delivers nearly all of them 0.7 to
/// 1.0 ms late and one every minute or more almost at once. Creeping at
/// the worst rate, the estimate is back among the slow ones in seconds;
/// the slower it creeps, the longer it stays with a fast one.
///
/// A rate measured from the same points (a line through them, the median
/// of their slopes) would creep less still, and was tried. It runs early
/// when the traffic changes: the fastest messages of a busy stretch are
/// quicker than those of a quiet one before it, which looks like a
/// device clock gaining on ours.
///
/// A device clock that jumps forward is followed at once, because the
/// difference shrinks. One that is set back makes every message look very
/// late; that is taken as a restart and the estimate begins again.
///
/// The result keeps the device's precise spacing between frames while
/// placing them on the daemon's time base, and is never in the future.
#[derive(Debug, Default)]
pub struct TimeSync {
    /// (offset in ns to add to device time, host time it was last updated)
    state: Option<(i128, u64)>,
    /// The fastest message of each recent window, oldest first.
    fastest: VecDeque<Fastest>,
    /// The most the offset may be growing by, in ns per ns of host time;
    /// `None` until something has been seen to bound it.
    creep: Option<f64>,
}

impl TimeSync {
    pub fn new() -> TimeSync {
        TimeSync::default()
    }

    /// Forget the clock relationship (the device clock was reset).
    pub fn reset(&mut self) {
        *self = TimeSync::default();
    }

    /// Record that a message stamped `device_ns` arrived at `host_now_ns`.
    ///
    /// When one transfer carries several messages, observe them all before
    /// stamping any: the last one waited the shortest and so tells the most
    /// about the offset, which the earlier ones should benefit from.
    pub fn observe(&mut self, device_ns: u64, host_now_ns: u64) {
        let observed = i128::from(host_now_ns) - i128::from(device_ns);
        let offset = match self.state {
            None => observed,
            Some((offset, updated)) => {
                let elapsed = host_now_ns.saturating_sub(updated) as f64;
                let creep = self.creep.unwrap_or(MAX_DRIFT_PPM / 1e6);
                let ceiling = offset + (elapsed * creep) as i128;
                let floor = offset - (elapsed * MAX_DRIFT_PPM / 1e6) as i128;
                if observed > ceiling + MAX_DELAY_NS || observed < floor - MAX_EARLY_NS {
                    // The device clock was set: start over from here.
                    self.fastest.clear();
                    self.creep = None;
                    observed
                } else {
                    ceiling.min(observed)
                }
            }
        };
        self.state = Some((offset, host_now_ns));
        self.note_fastest(Fastest {
            host_ns: host_now_ns,
            difference: observed,
            delayed_ns: observed - offset,
        });
    }

    /// Keep the fastest message of each window, and bound the rate again
    /// whenever a window is complete.
    fn note_fastest(&mut self, new: Fastest) {
        match self.fastest.back_mut() {
            Some(last) if last.host_ns / WINDOW_NS == new.host_ns / WINDOW_NS => {
                if new.difference < last.difference {
                    *last = new;
                }
            }
            _ => {
                self.window_complete();
                if self.fastest.len() == WINDOWS {
                    self.fastest.pop_front();
                }
                self.fastest.push_back(new);
            }
        }
    }

    /// The newest window is complete: bound the rate with it.
    fn window_complete(&mut self) {
        let Some(&last) = self.fastest.back() else {
            return;
        };
        if last.delayed_ns > MAX_FASTEST_DELAY_NS {
            // Every message of the window was held up longer than is
            // allowed for, so it bounds nothing. Or the estimate has
            // fallen behind, which must not last: bring it up.
            self.fastest.pop_back();
            if let Some((offset, _)) = self.state.as_mut() {
                *offset = (*offset).max(last.difference - MAX_FASTEST_DELAY_NS);
            }
            return;
        }
        let mut bound = f64::INFINITY;
        for (i, earlier) in self.fastest.iter().enumerate() {
            for later in self.fastest.iter().skip(i + 1) {
                let apart = later.host_ns.saturating_sub(earlier.host_ns) as f64;
                let grew = (later.difference - earlier.difference + MAX_FASTEST_DELAY_NS) as f64;
                bound = bound.min(grew / apart);
            }
        }
        if bound < -MAX_DRIFT_PPM / 1e6 {
            // No crystal is that far out: the points are not all from one
            // run of the device clock.
            self.fastest.clear();
            self.creep = None;
        } else if bound.is_finite() {
            self.creep = Some((bound + WANDER_PPM / 1e6).min(MAX_DRIFT_PPM / 1e6));
        }
    }

    /// The daemon-clock time of a message stamped `device_ns`, by the
    /// current estimate. Never later than the arrival of any message
    /// observed since that was stamped at or after `device_ns`.
    pub fn stamp(&self, device_ns: u64) -> u64 {
        let offset = self.state.map_or(0, |(offset, _)| offset);
        (i128::from(device_ns) + offset).max(0) as u64
    }

    /// [`observe`](Self::observe) one message and [`stamp`](Self::stamp) it.
    pub fn convert(&mut self, device_ns: u64, host_now_ns: u64) -> u64 {
        self.observe(device_ns, host_now_ns);
        self.stamp(device_ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;
    const S: u64 = 1_000 * MS;

    #[test]
    fn preserves_device_spacing_despite_usb_jitter() {
        let mut sync = TimeSync::new();
        let host_base = 1_700_000_000 * S;
        // Frames exactly 1 ms apart on the device; USB delivery adds
        // between 0.1 and 2 ms.
        let delays = [900_000, 100_000, 2_000_000, 400_000, 100_000, 1_500_000];
        let mut out = Vec::new();
        for (i, delay) in delays.iter().enumerate() {
            let dev = 5 * S + i as u64 * MS;
            let host = host_base + i as u64 * MS + delay;
            let ts = sync.convert(dev, host);
            assert!(ts <= host, "timestamp in the future");
            out.push(ts);
        }
        // Once the low-delay sample has been seen, spacing is exact.
        for w in out[1..].windows(2) {
            assert!((w[1] - w[0]).abs_diff(MS) < 1_000, "{:?}", out);
        }
        // And the absolute error is the smallest delay observed.
        assert_eq!(out[1], host_base + MS + 100_000);
    }

    #[test]
    fn follows_a_slow_device_clock() {
        let mut sync = TimeSync::new();
        let mut worst = 0;
        // Device runs 100 ppm slow for 100 s; one message per 100 ms with
        // a constant 200 us delivery delay.
        for i in 0..1000u64 {
            let true_time = i * 100 * MS;
            let dev = true_time - true_time / 10_000;
            let host = 50 * S + true_time + 200_000;
            let ts = sync.convert(dev, host);
            let error = (50 * S + true_time).abs_diff(ts);
            worst = worst.max(error);
        }
        // Without drift tracking the error would reach 10 ms.
        assert!(worst <= 250_000, "worst error {worst} ns");
    }

    #[test]
    fn follows_a_fast_device_clock() {
        let mut sync = TimeSync::new();
        for i in 0..1000u64 {
            let true_time = i * 100 * MS;
            let dev = true_time + true_time / 10_000;
            let host = 50 * S + true_time + 200_000;
            let ts = sync.convert(dev, host);
            assert!(ts <= host);
            assert!((50 * S + true_time).abs_diff(ts) <= 250_000);
        }
    }

    #[test]
    fn recovers_when_the_device_clock_is_set_back() {
        let mut sync = TimeSync::new();
        let host = 5000 * S;
        assert_eq!(sync.convert(219 * S, host), host);
        // The device clock restarted from zero; delivery takes 300 us.
        let ts = sync.convert(MS, host + S + 300_000);
        assert_eq!(ts, host + S + 300_000);
        // Later messages are placed relative to the new clock.
        let ts = sync.convert(11 * MS, host + S + 10 * MS + 100_000);
        assert_eq!(ts, host + S + 10 * MS + 100_000);
        // An ordinary late delivery is not a restart.
        let ts = sync.convert(21 * MS, host + S + 20 * MS + 50 * MS);
        assert!(ts.abs_diff(host + S + 20 * MS + 100_000) < 20_000, "{ts}");
    }

    #[test]
    fn a_transfer_observed_whole_keeps_its_spacing() {
        // Five frames 250 us apart arrive together after the estimate has
        // gone stale by a millisecond.
        let host = 9000 * S;
        let frames: Vec<u64> = (0..5).map(|i| 40 * S + i * 250_000).collect();
        let arrival = host + frames[4] - frames[0] + 100_000;

        let mut one_by_one = TimeSync::new();
        one_by_one.convert(frames[0] - S, host - S + MS);
        let late: Vec<u64> = frames
            .iter()
            .map(|&f| one_by_one.convert(f, arrival))
            .collect();
        assert_eq!(late[0], late[3], "stamped singly, they pile up");

        let mut sync = TimeSync::new();
        sync.convert(frames[0] - S, host - S + MS);
        for &f in &frames {
            sync.observe(f, arrival);
        }
        let stamps: Vec<u64> = frames.iter().map(|&f| sync.stamp(f)).collect();
        for w in stamps.windows(2) {
            assert_eq!(w[1] - w[0], 250_000);
        }
        assert_eq!(stamps[4], arrival);
    }

    /// The error of each timestamp, in ns, for messages `spacing_ms`
    /// apart with delivery delays in ns drawn from `delay(i, random)`,
    /// from a device whose clock is `ppm` slow.
    fn errors(
        sync: &mut TimeSync,
        start_ms: u64,
        count: u64,
        spacing_ms: u64,
        ppm: i64,
        delay: impl Fn(u64, u64) -> u64,
    ) -> Vec<i64> {
        let mut random = 12345u64 + start_ms;
        (0..count)
            .map(|i| {
                let true_time = (start_ms + i * spacing_ms) * MS;
                let dev = (1000 * S + true_time) as i64 - true_time as i64 * ppm / 1_000_000;
                random = random
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let host = 50_000 * S + true_time + delay(i, random >> 33);
                let ts = sync.convert(dev as u64, host);
                assert!(ts <= host, "timestamp in the future");
                ts as i64 - (50_000 * S + true_time) as i64
            })
            .collect()
    }

    #[test]
    fn the_estimate_stays_longer_with_a_fast_message_as_the_rate_is_bounded() {
        for ppm in [-60, 0, 25, 100] {
            // A frame every 5 ms for ten minutes. Delivery takes 0.8 to
            // 1.0 ms, but 0.1 ms for one message every 50 s (the PCAN-USB
            // X6 as measured).
            let mut sync = TimeSync::new();
            let all = errors(&mut sync, 0, 120_000, 5, ppm, |i, r| {
                if i % 10_000 == 2_000 {
                    100_000
                } else {
                    800_000 + r % 200_000
                }
            });
            let earliest = all.iter().min().unwrap();
            assert!(*earliest >= -5_000, "{ppm} ppm: {earliest} ns early");
            // Twenty seconds after the first fast message the estimate is
            // back among the slow ones...
            assert!(all[2_000] <= 100_000);
            assert!(all[6_000] >= 700_000, "{ppm} ppm: {}", all[6_000]);
            // ...and after the one nine minutes in, still near it.
            assert!(all[112_000] <= 100_000);
            assert!(all[116_000] <= 300_000, "{ppm} ppm: {}", all[116_000]);
        }
    }

    #[test]
    fn a_timestamp_is_never_early_when_the_traffic_changes() {
        // A quiet bus, where the adapter delivers every message late;
        // then a busy one, where it delivers them almost at once; then
        // quiet again. (This sequence set an estimate that measured the
        // rate from the fastest messages half a millisecond early, and
        // ISO-TP frames then followed each other in under STmin.)
        for ppm in [-60, 0, 25, 100] {
            let mut sync = TimeSync::new();
            let slow = |_, r: u64| 800_000 + r % 200_000;
            let fast = |_, r: u64| 30_000 + r % 100_000;
            let mut all = errors(&mut sync, 0, 6_000, 5, ppm, slow);
            all.extend(errors(&mut sync, 30_000, 20_000, 1, ppm, fast));
            all.extend(errors(&mut sync, 50_000, 60_000, 5, ppm, slow));
            let earliest = all.iter().min().unwrap();
            assert!(*earliest >= -5_000, "{ppm} ppm: {earliest} ns early");
        }
    }

    #[test]
    fn an_estimate_that_fell_behind_is_brought_up() {
        let mut sync = TimeSync::new();
        // Five minutes at one rate, then the device clock slows by
        // 150 ppm at a stroke (no crystal does; a wrong measurement
        // would look the same from here on).
        let mut dev = 1000 * S;
        let mut worst = 0;
        for i in 0..90_000u64 {
            let true_time = i * 5 * MS;
            dev += if i < 60_000 { 5 * MS } else { 5 * MS - 750 };
            let host = 50_000 * S + true_time + 300_000;
            let ts = sync.convert(dev, host);
            assert!(ts <= host);
            worst = worst.max(host - ts);
        }
        // Checked once a window, against a message that may be a window
        // old, and the clock loses 1.2 ms in a window.
        assert!(worst <= 5_000_000, "fell {worst} ns behind");
        assert!(worst > 300_000);
    }

    #[test]
    fn a_device_clock_set_forward_is_followed_at_once() {
        let mut sync = TimeSync::new();
        let mut jump = 0;
        for i in 0..6_000u64 {
            let true_time = i * 10 * MS;
            if i == 3_000 {
                jump = 30 * S;
            }
            let host = 50_000 * S + true_time + 200_000;
            let ts = sync.convert(1000 * S + true_time + jump, host);
            assert!(ts <= host);
            assert!(host - ts <= 250_000, "error {} ns at {i}", host - ts);
        }
    }

    #[test]
    fn reset_starts_over() {
        let mut sync = TimeSync::new();
        sync.convert(1000 * S, 2000 * S);
        sync.reset();
        // The device clock restarted from zero.
        assert_eq!(sync.convert(S, 2001 * S), 2001 * S);
    }
}
