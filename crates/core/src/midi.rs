//! MIDI logic (ADR-0009): a parser that splits raw MIDI 1.0 bytes into whole messages,
//! the receiver's jitter buffer and the tracker that silences held notes when a peer
//! goes away. Pure; time is passed in as microseconds.

use std::collections::{BTreeSet, VecDeque};

use bridge_protocol::midi::data_len;
pub use bridge_protocol::midi::{MAX_SYSEX, is_valid_message};

/// Splits a byte stream into complete messages; keeps running status.
#[derive(Default, Debug)]
pub struct MidiParser {
    running: Option<u8>,
    pending: Vec<u8>,
    sysex: Option<Vec<u8>>,
}

impl MidiParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds bytes; returns the complete messages (with status bytes restored).
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for &b in bytes {
            if b >= 0xF8 {
                // Real-time bytes may appear anywhere, even inside SysEx.
                out.push(vec![b]);
                continue;
            }
            if let Some(sx) = &mut self.sysex {
                if b == 0xF7 {
                    sx.push(b);
                    out.push(std::mem::take(sx));
                    self.sysex = None;
                } else if b & 0x80 != 0 {
                    // A status byte ends an unterminated SysEx: drop it.
                    self.sysex = None;
                    self.start(b, &mut out);
                } else if sx.len() < MAX_SYSEX {
                    sx.push(b);
                } else {
                    self.sysex = None;
                }
                continue;
            }
            if b & 0x80 != 0 {
                self.start(b, &mut out);
                continue;
            }
            let status = match self.pending.first() {
                Some(&s) => s,
                None => {
                    let Some(s) = self.running else { continue };
                    self.pending.push(s);
                    s
                }
            };
            self.pending.push(b);
            if Some(self.pending.len() - 1) == data_len(status) {
                out.push(std::mem::take(&mut self.pending));
            }
        }
        out
    }

    fn start(&mut self, status: u8, out: &mut Vec<Vec<u8>>) {
        self.pending.clear();
        match data_len(status) {
            None => {
                self.sysex = Some(vec![status]);
                self.running = None;
            }
            Some(0) => {
                out.push(vec![status]);
                if status >= 0xF0 {
                    self.running = None;
                }
            }
            Some(_) => {
                self.pending.push(status);
                // System common messages cancel running status.
                self.running = (status < 0xF0).then_some(status);
            }
        }
    }
}

/// Held notes and pedals of one stream, so a lost peer never leaves a note hanging.
#[derive(Default, Debug, Clone)]
pub struct NoteTracker {
    notes: BTreeSet<(u8, u8)>,
    pedals: BTreeSet<(u8, u8)>,
}

const PEDALS: [u8; 3] = [64, 66, 67];

impl NoteTracker {
    pub fn observe(&mut self, msg: &[u8]) {
        let [status, a, b] = msg else {
            return;
        };
        let ch = status & 0x0F;
        match status & 0xF0 {
            0x90 if *b > 0 => {
                self.notes.insert((ch, *a));
            }
            0x80 | 0x90 => {
                self.notes.remove(&(ch, *a));
            }
            0xB0 if PEDALS.contains(a) => {
                if *b >= 64 {
                    self.pedals.insert((ch, *a));
                } else {
                    self.pedals.remove(&(ch, *a));
                }
            }
            0xB0 if *a == 123 || *a == 120 => {
                self.notes.retain(|(c, _)| *c != ch);
            }
            _ => {}
        }
    }

    pub fn is_silent(&self) -> bool {
        self.notes.is_empty() && self.pedals.is_empty()
    }

    /// Note-offs for every held note and pedal releases; the tracker is empty after.
    pub fn release_all(&mut self) -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = self
            .notes
            .iter()
            .map(|(ch, n)| vec![0x80 | ch, *n, 0])
            .collect();
        out.extend(self.pedals.iter().map(|(ch, cc)| vec![0xB0 | ch, *cc, 0]));
        self.notes.clear();
        self.pedals.clear();
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JitterConfig {
    /// Extra delay on top of the fastest observed transit, microseconds.
    pub buffer_us: u64,
    /// Size the buffer from the recent transit variation instead (p99, capped).
    pub adaptive: bool,
    pub max_buffer_us: u64,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self {
            buffer_us: 3_000,
            adaptive: false,
            max_buffer_us: 20_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitterStats {
    pub events: u64,
    pub late: u64,
    /// Fastest transit seen (sender clock converted to ours), microseconds.
    pub min_transit_us: u64,
    /// Current buffer on top of it.
    pub buffer_us: u64,
}

const WINDOW: usize = 256;

/// Schedules events from one sender at `sender_ts + transit_min + buffer` in local
/// time. Play times never decrease, so the order is kept; events that arrive past
/// their slot play at once and count as late.
#[derive(Debug)]
pub struct JitterBuffer {
    config: JitterConfig,
    min_transit: Option<i64>,
    recent: VecDeque<i64>,
    last_play: u64,
    queue: VecDeque<(u64, Vec<u8>)>,
    stats: JitterStats,
}

impl JitterBuffer {
    pub fn new(config: JitterConfig) -> Self {
        Self {
            config,
            min_transit: None,
            recent: VecDeque::new(),
            last_play: 0,
            queue: VecDeque::new(),
            stats: JitterStats::default(),
        }
    }

    fn buffer(&self) -> u64 {
        if !self.config.adaptive || self.recent.len() < 16 {
            return self.config.buffer_us.min(self.config.max_buffer_us);
        }
        let min = self.min_transit.unwrap_or(0);
        let mut v: Vec<i64> = self.recent.iter().map(|t| t - min).collect();
        v.sort_unstable();
        let p99 = v[(v.len() - 1) * 99 / 100].max(0) as u64;
        p99.min(self.config.max_buffer_us)
    }

    /// An event the sender stamped `sent_local` (already converted to our clock)
    /// arrived at `now`. Returns its play time.
    pub fn push(&mut self, now: u64, sent_local: u64, msg: Vec<u8>) -> u64 {
        let transit = now as i64 - sent_local as i64;
        self.min_transit = Some(self.min_transit.map_or(transit, |m| m.min(transit)));
        if self.recent.len() == WINDOW {
            self.recent.pop_front();
        }
        self.recent.push_back(transit);
        let min = self.min_transit.unwrap_or(transit);
        let target = (sent_local as i64 + min + self.buffer() as i64).max(0) as u64;
        let mut play = target.max(self.last_play);
        if play < now {
            play = now.max(self.last_play);
            self.stats.late += 1;
        }
        self.last_play = play;
        self.stats.events += 1;
        self.stats.min_transit_us = min.max(0) as u64;
        self.stats.buffer_us = self.buffer();
        self.queue.push_back((play, msg));
        play
    }

    /// The next event due at or before `now`.
    pub fn pop_due(&mut self, now: u64) -> Option<Vec<u8>> {
        if self.queue.front().is_some_and(|(t, _)| *t <= now) {
            return self.queue.pop_front().map(|(_, m)| m);
        }
        None
    }

    /// When the next event is due.
    pub fn next_due(&self) -> Option<u64> {
        self.queue.front().map(|(t, _)| *t)
    }

    /// Everything still queued, in order (a peer left: play out, then silence).
    pub fn drain(&mut self) -> Vec<Vec<u8>> {
        self.queue.drain(..).map(|(_, m)| m).collect()
    }

    pub fn stats(&self) -> JitterStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_handles_running_status_realtime_and_sysex() {
        let mut p = MidiParser::new();
        let msgs = p.feed(&[0x90, 60, 100, 62, 90, 0xF8, 0x80, 60, 0]);
        assert_eq!(
            msgs,
            vec![
                vec![0x90, 60, 100],
                vec![0x90, 62, 90],
                vec![0xF8],
                vec![0x80, 60, 0]
            ]
        );
        assert_eq!(p.feed(&[0xC0, 5, 6]), vec![vec![0xC0, 5], vec![0xC0, 6]]);
        assert_eq!(p.feed(&[0xF0, 1, 2]), Vec::<Vec<u8>>::new());
        assert_eq!(
            p.feed(&[0xFE, 3, 0xF7]),
            vec![vec![0xFE], vec![0xF0, 1, 2, 3, 0xF7]]
        );
        assert_eq!(
            p.feed(&[0xF0, 1, 0x90, 1, 2]),
            vec![vec![0x90, 1, 2]],
            "broken SysEx is dropped"
        );
        assert_eq!(
            p.feed(&[0xF2, 1, 2, 3]),
            vec![vec![0xF2, 1, 2]],
            "system common ends running status"
        );
        assert!(p.feed(&[0x7F]).is_empty());
    }

    #[test]
    fn validation() {
        assert!(is_valid_message(&[0x90, 60, 1]));
        assert!(!is_valid_message(&[0x90, 60]));
        assert!(!is_valid_message(&[0x90, 60, 0x80]));
        assert!(is_valid_message(&[0xF8]));
        assert!(is_valid_message(&[0xF0, 0x7E, 0xF7]));
        assert!(!is_valid_message(&[0xF0, 0x7E]));
        assert!(!is_valid_message(&[0xF7]));
        assert!(!is_valid_message(&[]));
        let mut big = vec![0xF0];
        big.extend(std::iter::repeat_n(1, MAX_SYSEX));
        big.push(0xF7);
        assert!(!is_valid_message(&big));
    }

    #[test]
    fn tracker_releases_notes_and_pedals() {
        let mut t = NoteTracker::default();
        t.observe(&[0x90, 60, 100]);
        t.observe(&[0x91, 64, 80]);
        t.observe(&[0x90, 62, 0]);
        t.observe(&[0xB0, 64, 127]);
        t.observe(&[0x91, 64, 0]);
        assert!(!t.is_silent());
        assert_eq!(t.release_all(), vec![vec![0x80, 60, 0], vec![0xB0, 64, 0]]);
        assert!(t.is_silent());
        t.observe(&[0x92, 1, 1]);
        t.observe(&[0xB2, 123, 0]);
        assert!(t.is_silent(), "all notes off");
    }

    #[test]
    fn jitter_buffer_smooths_and_keeps_order() {
        let mut j = JitterBuffer::new(JitterConfig::default());
        // Transit 1 ms, then 4 ms: the second still plays 3 ms after its send + 1 ms.
        assert_eq!(j.push(1_000, 0, vec![1]), 4_000);
        assert_eq!(j.push(14_000, 10_000, vec![2]), 14_000);
        assert_eq!(j.stats().late, 0);
        // A very late event plays at once and never before an earlier one.
        assert_eq!(j.push(30_000, 11_000, vec![3]), 30_000);
        assert_eq!(j.stats().late, 1);
        assert_eq!(j.pop_due(3_999), None);
        assert_eq!(j.pop_due(4_000), Some(vec![1]));
        assert_eq!(j.next_due(), Some(14_000));
        assert_eq!(j.drain(), vec![vec![2], vec![3]]);
    }

    #[test]
    fn adaptive_buffer_follows_the_spread() {
        let mut j = JitterBuffer::new(JitterConfig {
            adaptive: true,
            ..JitterConfig::default()
        });
        for i in 0..100u64 {
            let jitter = (i % 10) * 500;
            j.push(i * 10_000 + 1_000 + jitter, i * 10_000, vec![0xF8]);
        }
        let b = j.stats().buffer_us;
        assert!((4_000..=5_000).contains(&b), "p99 of 0..4.5 ms spread: {b}");
    }
}
