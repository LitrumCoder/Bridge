//! "Shake the mouse to find the cursor": several fast direction reversals in a short
//! window.

use std::collections::VecDeque;
use std::time::Duration;

use crate::Micros;

const WINDOW: Duration = Duration::from_millis(600);
const MIN_SEGMENT_PX: f64 = 40.0;
const REVERSALS: usize = 4;
const COOLDOWN: Duration = Duration::from_secs(1);

#[derive(Default, Debug)]
pub(crate) struct ShakeDetector {
    reversals: VecDeque<Micros>,
    run: f64,
    direction: i8,
    cooldown_until: Micros,
}

impl ShakeDetector {
    /// Feeds a horizontal movement; true when a shake is recognised.
    pub(crate) fn feed(&mut self, now: Micros, dx: f64) -> bool {
        if dx == 0.0 {
            return false;
        }
        let dir = if dx > 0.0 { 1 } else { -1 };
        if dir == self.direction {
            self.run += dx.abs();
        } else {
            if self.run >= MIN_SEGMENT_PX && self.direction != 0 {
                self.reversals.push_back(now);
            }
            self.direction = dir;
            self.run = dx.abs();
        }
        while self
            .reversals
            .front()
            .is_some_and(|t| now.saturating_sub(*t) > WINDOW)
        {
            self.reversals.pop_front();
        }
        if self.reversals.len() >= REVERSALS && now >= self.cooldown_until {
            self.reversals.clear();
            self.cooldown_until = now + COOLDOWN;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_back_and_forth_is_a_shake_slow_drift_is_not() {
        let mut s = ShakeDetector::default();
        let mut t = 1_000_000u64;
        let mut fired = false;
        for i in 0..10 {
            for _ in 0..5 {
                t += 10_000;
                fired |= s.feed(Micros(t), if i % 2 == 0 { 15.0 } else { -15.0 });
            }
        }
        assert!(fired);

        let mut s = ShakeDetector::default();
        let mut t = 1_000_000u64;
        for i in 0..10 {
            t += 400_000;
            assert!(
                !s.feed(Micros(t), if i % 2 == 0 { 60.0 } else { -60.0 }),
                "too slow"
            );
        }
        let mut s = ShakeDetector::default();
        for i in 0..100 {
            assert!(!s.feed(Micros(1_000_000 + i * 1000), 5.0), "one direction");
        }
        assert!(!s.feed(Micros(5), 0.0));
    }
}
