// SPDX-License-Identifier: GPL-2.0-only

//! Turning a bitrate into controller bit timing parameters.

/// Limits of a CAN controller's bit timing registers.
#[derive(Debug, Clone, Copy)]
pub struct TimingLimits {
    /// Controller clock in Hz.
    pub clock_hz: u32,
    pub tseg1_min: u32,
    pub tseg1_max: u32,
    pub tseg2_min: u32,
    pub tseg2_max: u32,
    pub sjw_max: u32,
    pub brp_min: u32,
    pub brp_max: u32,
}

/// One bit is `1 + tseg1 + tseg2` time quanta of `brp / clock` seconds each,
/// sampled after `1 + tseg1` quanta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitTiming {
    pub brp: u32,
    /// Propagation segment plus phase segment 1, in time quanta.
    pub tseg1: u32,
    /// Phase segment 2, in time quanta.
    pub tseg2: u32,
    pub sjw: u32,
    /// The bitrate these parameters actually produce.
    pub bitrate: u32,
    /// The resulting sample point in tenths of a percent.
    pub sample_point: u32,
}

/// CiA-recommended sample point for a bitrate, in tenths of a percent.
pub fn default_sample_point(bitrate: u32) -> u32 {
    if bitrate > 800_000 {
        750
    } else if bitrate > 500_000 {
        800
    } else {
        875
    }
}

/// Find the parameters that hit `bitrate` most closely, then the requested
/// sample point most closely, preferring the finest time quantum. Fails if
/// the controller cannot get within 0.5 % of the bitrate.
pub fn calculate(
    limits: &TimingLimits,
    bitrate: u32,
    sample_point: Option<u32>,
) -> Result<BitTiming, String> {
    if bitrate == 0 {
        return Err("bitrate must be non-zero".into());
    }
    let want_sp = sample_point.unwrap_or_else(|| default_sample_point(bitrate));
    if !(500..=950).contains(&want_sp) {
        return Err(format!(
            "sample point {}.{} % is outside 50-95 %",
            want_sp / 10,
            want_sp % 10
        ));
    }
    let total_min = 1 + limits.tseg1_min + limits.tseg2_min;
    let total_max = 1 + limits.tseg1_max + limits.tseg2_max;

    // (bitrate error, sample point error, brp) - smaller is better.
    let mut best: Option<((u64, u32, u32), BitTiming)> = None;
    for brp in limits.brp_min..=limits.brp_max {
        let tq_rate = u64::from(limits.clock_hz) / u64::from(brp);
        // Nearest whole number of quanta per bit.
        let total = ((tq_rate + u64::from(bitrate) / 2) / u64::from(bitrate)) as u32;
        if total < total_min {
            // Larger prescalers only make it worse.
            break;
        }
        if total > total_max {
            continue;
        }
        let actual = (u64::from(limits.clock_hz) / (u64::from(brp) * u64::from(total))) as u32;
        let rate_err = u64::from(actual.abs_diff(bitrate));

        let mut tseg2 =
            ((total * (1000 - want_sp) + 500) / 1000).clamp(limits.tseg2_min, limits.tseg2_max);
        let mut tseg1 = total - 1 - tseg2;
        if tseg1 > limits.tseg1_max {
            tseg1 = limits.tseg1_max;
            tseg2 = total - 1 - tseg1;
        } else if tseg1 < limits.tseg1_min {
            tseg1 = limits.tseg1_min;
            tseg2 = total - 1 - tseg1;
        }
        if !(limits.tseg2_min..=limits.tseg2_max).contains(&tseg2) {
            continue;
        }
        let sp = (1 + tseg1) * 1000 / total;
        let score = (rate_err, sp.abs_diff(want_sp), brp);
        if best.as_ref().is_none_or(|(s, _)| score < *s) {
            let sjw = tseg1.min(tseg2 / 2).clamp(1, limits.sjw_max);
            best = Some((
                score,
                BitTiming {
                    brp,
                    tseg1,
                    tseg2,
                    sjw,
                    bitrate: actual,
                    sample_point: sp,
                },
            ));
        }
    }

    match best {
        Some(((rate_err, _, _), timing)) if rate_err * 200 <= u64::from(bitrate) => Ok(timing),
        _ => Err(format!(
            "this adapter cannot produce {bitrate} bit/s from its {} Hz clock",
            limits.clock_hz
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PCAN-USB FD nominal phase limits.
    const NOMINAL: TimingLimits = TimingLimits {
        clock_hz: 80_000_000,
        tseg1_min: 1,
        tseg1_max: 256,
        tseg2_min: 1,
        tseg2_max: 128,
        sjw_max: 128,
        brp_min: 1,
        brp_max: 1024,
    };
    /// PCAN-USB FD data phase limits.
    const DATA: TimingLimits = TimingLimits {
        tseg1_max: 32,
        tseg2_max: 16,
        sjw_max: 16,
        ..NOMINAL
    };

    fn check(limits: &TimingLimits, t: &BitTiming) {
        assert!(
            (limits.tseg1_min..=limits.tseg1_max).contains(&t.tseg1),
            "{t:?}"
        );
        assert!(
            (limits.tseg2_min..=limits.tseg2_max).contains(&t.tseg2),
            "{t:?}"
        );
        assert!((1..=limits.sjw_max).contains(&t.sjw), "{t:?}");
        assert!(t.sjw <= t.tseg2, "{t:?}");
        assert!((limits.brp_min..=limits.brp_max).contains(&t.brp), "{t:?}");
        assert_eq!(
            limits.clock_hz / (t.brp * (1 + t.tseg1 + t.tseg2)),
            t.bitrate
        );
    }

    #[test]
    fn common_nominal_bitrates_are_exact() {
        for (bitrate, sp) in [
            (10_000, 875),
            (20_000, 875),
            (50_000, 875),
            (100_000, 875),
            (125_000, 875),
            (250_000, 875),
            (500_000, 875),
            (800_000, 800),
            (1_000_000, 750),
        ] {
            let t = calculate(&NOMINAL, bitrate, None).unwrap();
            check(&NOMINAL, &t);
            assert_eq!(t.bitrate, bitrate);
            assert_eq!(t.sample_point, sp, "{bitrate}: {t:?}");
        }
    }

    #[test]
    fn common_data_bitrates_are_exact() {
        for bitrate in [1_000_000, 2_000_000, 4_000_000, 5_000_000, 8_000_000] {
            let t = calculate(&DATA, bitrate, None).unwrap();
            check(&DATA, &t);
            assert_eq!(t.bitrate, bitrate);
            assert!(t.sample_point.abs_diff(750) <= 50, "{bitrate}: {t:?}");
        }
        // 2 Mbit/s at 80 %: 40 quanta, sample after 32.
        let t = calculate(&DATA, 2_000_000, Some(800)).unwrap();
        assert_eq!((t.brp, t.tseg1, t.tseg2), (1, 31, 8));
    }

    #[test]
    fn finest_quantum_is_preferred() {
        let t = calculate(&NOMINAL, 500_000, None).unwrap();
        // 160 quanta per bit at brp 1: sample after 140.
        assert_eq!((t.brp, t.tseg1, t.tseg2), (1, 139, 20));
        assert_eq!(t.sjw, 10);
    }

    #[test]
    fn unreachable_requests_fail() {
        assert!(calculate(&NOMINAL, 0, None).is_err());
        assert!(calculate(&NOMINAL, 500_000, Some(990)).is_err());
        // 3 quanta minimum at 80 MHz tops out near 26.7 Mbit/s.
        assert!(calculate(&DATA, 50_000_000, None).is_err());
        // 7 Mbit/s does not divide 80 MHz within tolerance.
        assert!(calculate(&DATA, 7_000_000, None).is_err());
        // Slower than the prescaler and segment registers can stretch a bit.
        assert!(calculate(&NOMINAL, 100, None).is_err());
    }
}
