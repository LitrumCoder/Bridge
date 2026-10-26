//! Clock offset estimation between two devices (ARCHITECTURE §4.6).
//!
//! Each exchange gives `t0` (our send), `t1`/`t2` (their receive/send, their clock) and
//! `t3` (our receive). Samples with the lowest round-trip time have the least queueing
//! asymmetry, so the estimate is the median offset of the fastest quarter of the window.

use std::collections::VecDeque;

const WINDOW: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClockSample {
    /// `their_clock - our_clock`, microseconds.
    pub offset: i64,
    pub rtt: u64,
    pub at: u64,
}

impl ClockSample {
    pub fn from_exchange(t0: u64, t1: u64, t2: u64, t3: u64) -> Option<Self> {
        if t3 < t0 || t2 < t1 {
            return None;
        }
        let total = t3 - t0;
        let remote = t2 - t1;
        let rtt = total.checked_sub(remote)?;
        let offset = (i128::from(t1) - i128::from(t0) + i128::from(t2) - i128::from(t3)) / 2;
        Some(Self {
            offset: i64::try_from(offset).ok()?,
            rtt,
            at: t3,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct ClockSync {
    samples: VecDeque<ClockSample>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClockEstimate {
    pub offset: i64,
    pub min_rtt: u64,
    /// Spread of round-trip times in the window (p90 − min), a jitter indicator.
    pub rtt_spread: u64,
    pub samples: usize,
}

impl ClockSync {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, sample: ClockSample) {
        if self.samples.len() == WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    pub fn reset(&mut self) {
        self.samples.clear();
    }

    pub fn estimate(&self) -> Option<ClockEstimate> {
        if self.samples.is_empty() {
            return None;
        }
        let mut by_rtt: Vec<ClockSample> = self.samples.iter().copied().collect();
        by_rtt.sort_by_key(|s| s.rtt);
        let best = &by_rtt[..by_rtt.len().div_ceil(4)];
        let mut offsets: Vec<i64> = best.iter().map(|s| s.offset).collect();
        offsets.sort_unstable();
        let offset = offsets[offsets.len() / 2];
        let min_rtt = by_rtt[0].rtt;
        let p90 = by_rtt[(by_rtt.len() * 9 / 10).min(by_rtt.len() - 1)].rtt;
        Some(ClockEstimate {
            offset,
            min_rtt,
            rtt_spread: p90 - min_rtt,
            samples: by_rtt.len(),
        })
    }

    /// Converts a timestamp of the remote clock into ours.
    pub fn to_local(&self, remote: u64) -> Option<u64> {
        let est = self.estimate()?;
        u64::try_from(i128::from(remote) - i128::from(est.offset)).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Remote clock = local + `offset`; one-way delays given explicitly.
    fn exchange(
        local_send: u64,
        offset: i64,
        out_delay: u64,
        back_delay: u64,
        hold: u64,
    ) -> ClockSample {
        let t0 = local_send;
        let t1 = (t0 as i64 + out_delay as i64 + offset) as u64;
        let t2 = t1 + hold;
        let t3 = (t2 as i64 - offset + back_delay as i64) as u64;
        ClockSample::from_exchange(t0, t1, t2, t3).unwrap()
    }

    #[test]
    fn symmetric_delay_gives_exact_offset() {
        let s = exchange(1_000_000, 12_345, 400, 400, 30);
        assert_eq!(s.offset, 12_345);
        assert_eq!(s.rtt, 800);
    }

    #[test]
    fn invalid_exchanges_are_rejected() {
        assert!(ClockSample::from_exchange(10, 0, 0, 5).is_none());
        assert!(ClockSample::from_exchange(0, 10, 5, 20).is_none());
        assert!(ClockSample::from_exchange(0, 0, 100, 20).is_none());
    }

    #[test]
    fn slow_asymmetric_samples_are_outvoted_by_fast_ones() {
        let mut sync = ClockSync::new();
        for i in 0..24 {
            sync.add(exchange(1_000_000 + i * 1000, -50_000, 300, 300, 10));
        }
        for i in 0..8 {
            sync.add(exchange(1_030_000 + i * 1000, -50_000, 300, 20_000, 10));
        }
        let est = sync.estimate().unwrap();
        assert_eq!(est.offset, -50_000);
        assert_eq!(est.min_rtt, 600);
        assert!(est.rtt_spread > 0);
        assert_eq!(sync.to_local(1_000_000), Some(1_050_000));
    }

    #[test]
    fn window_is_bounded_and_reset_clears() {
        let mut sync = ClockSync::new();
        assert!(sync.estimate().is_none());
        assert!(sync.to_local(5).is_none());
        for i in 0..100 {
            sync.add(exchange(i * 1000, 7, 100, 100, 0));
        }
        assert_eq!(sync.estimate().unwrap().samples, WINDOW);
        sync.reset();
        assert!(sync.estimate().is_none());
    }

    proptest! {
        #[test]
        fn error_is_bounded_by_half_the_asymmetry(
            offset in -1_000_000_000i64..1_000_000_000,
            delays in proptest::collection::vec((0u64..20_000, 0u64..20_000), 1..40),
        ) {
            let mut sync = ClockSync::new();
            let base = 2_000_000_000u64;
            for (i, (out, back)) in delays.iter().enumerate() {
                sync.add(exchange(base + i as u64 * 50_000, offset, *out, *back, 25));
            }
            let est = sync.estimate().unwrap();
            let worst = delays.iter().map(|(o, b)| o.abs_diff(*b)).max().unwrap();
            prop_assert!((est.offset - offset).unsigned_abs() <= worst / 2 + 1);
        }
    }
}
