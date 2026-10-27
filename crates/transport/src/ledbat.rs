//! Delay-based congestion control for the bulk connection (ADR-0002), after LEDBAT
//! (RFC 6817): the window grows while the measured queuing delay stays under a small
//! target and shrinks as soon as it rises, so file transfers never build the queue that
//! would delay input and MIDI on the interactive connection.

use std::any::Any;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quinn::congestion::{Controller, ControllerFactory};
use quinn_proto::RttEstimator;

#[derive(Clone, Copy, Debug)]
pub struct LedbatConfig {
    /// Queuing delay we allow ourselves to add.
    pub target: Duration,
    /// Window change per RTT at the extremes, in packets.
    pub gain: f64,
    pub initial_packets: u64,
    pub min_packets: u64,
    /// How long the base (empty-queue) delay is remembered.
    pub base_history: Duration,
}

impl Default for LedbatConfig {
    fn default() -> Self {
        Self {
            target: Duration::from_millis(5),
            gain: 1.0,
            initial_packets: 10,
            min_packets: 2,
            base_history: Duration::from_secs(60),
        }
    }
}

impl ControllerFactory for LedbatConfig {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Ledbat::new(*self, now, current_mtu))
    }
}

/// Minimum one-way-ish delay per bucket of `base_history / BUCKETS`.
const BUCKETS: usize = 6;
/// Recent samples whose minimum is the current delay. More than RFC 6817 suggests (4):
/// delayed acks and coarse OS timers add noise that must not read as queuing.
const CURRENT: usize = 16;

#[derive(Clone, Debug)]
pub struct Ledbat {
    config: LedbatConfig,
    mtu: u64,
    window: u64,
    slow_start: bool,
    base: VecDeque<(Instant, Duration)>,
    current: VecDeque<Duration>,
    last_loss: Option<Instant>,
    last_decrease: Option<Instant>,
}

impl Ledbat {
    pub fn new(config: LedbatConfig, _now: Instant, mtu: u16) -> Self {
        let mtu = u64::from(mtu);
        Self {
            config,
            mtu,
            window: config.initial_packets * mtu,
            slow_start: true,
            base: VecDeque::new(),
            current: VecDeque::new(),
            last_loss: None,
            last_decrease: None,
        }
    }

    fn min_window(&self) -> u64 {
        self.config.min_packets * self.mtu
    }

    fn base_delay(&self) -> Option<Duration> {
        self.base.iter().map(|(_, d)| *d).min()
    }

    fn record(&mut self, now: Instant, sample: Duration) {
        let bucket = self.config.base_history / BUCKETS as u32;
        match self.base.back_mut() {
            Some((start, min)) if now.duration_since(*start) < bucket => *min = (*min).min(sample),
            _ => {
                self.base.push_back((now, sample));
                if self.base.len() > BUCKETS {
                    self.base.pop_front();
                }
            }
        }
        self.current.push_back(sample);
        if self.current.len() > CURRENT {
            self.current.pop_front();
        }
    }

    /// Acknowledged `bytes` sent at `sent`.
    pub fn ack(&mut self, now: Instant, sent: Instant, bytes: u64, app_limited: bool) {
        self.record(now, now.saturating_duration_since(sent));
        if app_limited {
            return;
        }
        let Some(queuing) = self.queuing_delay() else {
            return;
        };
        let target = self.config.target.as_secs_f64();
        let off_target = (target - queuing.as_secs_f64()) / target;
        if self.slow_start {
            if off_target > 0.5 {
                self.window += bytes;
                return;
            }
            self.slow_start = false;
        }
        if off_target >= 0.0 {
            let delta =
                self.config.gain * off_target * bytes as f64 * self.mtu as f64 / self.window as f64;
            self.window += delta as u64;
            return;
        }
        // Over the target: back off multiplicatively, at most once per round trip
        // (LEDBAT++), or a large window would take minutes to drain the queue.
        let rtt = self.current.iter().min().copied().unwrap_or_default();
        if self
            .last_decrease
            .is_some_and(|t| now.duration_since(t) < rtt)
        {
            return;
        }
        self.last_decrease = Some(now);
        let factor = 1.0 - 0.5 * (-off_target).min(1.0);
        self.window = ((self.window as f64 * factor) as u64).max(self.min_window());
    }

    /// Loss of a packet sent at `sent`: one reduction per window of losses, as in Reno.
    pub fn loss(&mut self, now: Instant, sent: Instant) {
        if self.last_loss.is_some_and(|t| sent <= t) {
            return;
        }
        self.last_loss = Some(now);
        self.slow_start = false;
        self.window = (self.window / 2).max(self.min_window());
    }

    /// Queuing delay estimate: current minimum over the base.
    pub fn queuing_delay(&self) -> Option<Duration> {
        let current = self.current.iter().min()?;
        Some(current.saturating_sub(self.base_delay()?))
    }
}

impl Controller for Ledbat {
    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        _rtt: &RttEstimator,
    ) {
        self.ack(now, sent, bytes, app_limited);
    }

    fn on_congestion_event(&mut self, now: Instant, sent: Instant, _persistent: bool, _lost: u64) {
        self.loss(now, sent);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.mtu = u64::from(new_mtu);
        self.window = self.window.max(self.min_window());
    }

    fn window(&self) -> u64 {
        self.window
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.config.initial_packets * self.mtu
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds one window's worth of acks with the given round-trip time.
    fn round(c: &mut Ledbat, now: &mut Instant, rtt_ms: u64) {
        let packets = (c.window() / c.mtu).max(1);
        for _ in 0..packets {
            *now += Duration::from_micros(100);
            let sent = *now - Duration::from_millis(rtt_ms);
            c.ack(*now, sent, c.mtu, false);
        }
    }

    #[test]
    fn grows_on_an_empty_path_and_backs_off_when_the_queue_builds() {
        let mut now = Instant::now();
        let mut c = Ledbat::new(LedbatConfig::default(), now, 1200);
        let start = c.window();
        for _ in 0..10 {
            round(&mut c, &mut now, 10);
        }
        let grown = c.window();
        assert!(
            grown > start * 4,
            "slow start on an idle path: {start} → {grown}"
        );
        // The queue adds 20 ms over the 10 ms base: four times the target.
        for _ in 0..10 {
            round(&mut c, &mut now, 30);
        }
        assert!(
            c.window() < grown / 2,
            "shrinks under queuing delay: {}",
            c.window()
        );
        assert!(c.window() >= c.min_window());
        let low = c.window();
        for _ in 0..10 {
            round(&mut c, &mut now, 11);
        }
        assert!(c.window() > low, "recovers below the target");
    }

    #[test]
    fn loss_halves_once_per_window() {
        let now = Instant::now();
        let mut c = Ledbat::new(LedbatConfig::default(), now, 1200);
        let w = c.window();
        c.loss(now, now);
        assert_eq!(c.window(), w / 2);
        c.loss(now, now);
        assert_eq!(c.window(), w / 2, "same window of losses");
        for _ in 0..10 {
            c.loss(now + Duration::from_secs(1), now + Duration::from_secs(1));
        }
        assert_eq!(c.window(), c.min_window().max(w / 4));
        c.on_mtu_update(1400);
        assert!(c.window() >= 2 * 1400);
        assert_eq!(c.clone_box().window(), c.window());
    }
}
