use std::fmt;
use std::ops::{Add, Sub};
use std::time::Duration;

/// Monotonic microseconds since an arbitrary per-process epoch. The core never reads
/// clocks itself; drivers pass the current value in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Micros(pub u64);

impl Micros {
    pub const ZERO: Self = Self(0);

    pub fn from_duration(d: Duration) -> Self {
        Self(u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
    }

    pub fn as_duration(self) -> Duration {
        Duration::from_micros(self.0)
    }

    pub fn saturating_sub(self, other: Self) -> Duration {
        Duration::from_micros(self.0.saturating_sub(other.0))
    }
}

impl Add<Duration> for Micros {
    type Output = Micros;
    fn add(self, d: Duration) -> Micros {
        Micros(
            self.0
                .saturating_add(u64::try_from(d.as_micros()).unwrap_or(u64::MAX)),
        )
    }
}

impl Sub<Duration> for Micros {
    type Output = Micros;
    fn sub(self, d: Duration) -> Micros {
        Micros(
            self.0
                .saturating_sub(u64::try_from(d.as_micros()).unwrap_or(u64::MAX)),
        )
    }
}

impl fmt::Debug for Micros {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}µs", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_saturates() {
        let t = Micros(10);
        assert_eq!(t + Duration::from_micros(5), Micros(15));
        assert_eq!(t - Duration::from_secs(1), Micros::ZERO);
        assert_eq!(Micros(u64::MAX) + Duration::from_secs(1), Micros(u64::MAX));
        assert_eq!(
            Micros(30).saturating_sub(Micros(10)),
            Duration::from_micros(20)
        );
        assert_eq!(Micros(10).saturating_sub(Micros(30)), Duration::ZERO);
        assert_eq!(
            Micros::from_duration(Duration::from_millis(2)).as_duration(),
            Duration::from_millis(2)
        );
        assert_eq!(format!("{:?}", Micros(7)), "7µs");
    }
}
