//! Whether a call that still works is struggling, and on whose side: the weak pill on the call
//! screen. Judged from this phone's own counters only; nothing is asked of the other one.
//!
//! Ours: what we send backs up, so frames are dropped before sending or reset after missing their
//! deadline, or audio does not fit the send buffer. Theirs: what arrives is late or has gaps while
//! our own sending is fine. A call that has stopped altogether is the reconnecting overlay's, not
//! this.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::media::MediaStats;

/// Bad samples in a row before the pill shows: one late frame is not a weak connection.
const SHOW_AFTER: u32 = 2;
/// Good samples in a row before it goes, so it does not blink while a link hovers at the edge.
const CLEAR_AFTER: u32 = 5;
/// Audio packets per sample that may arrive late or be concealed before it counts: a few in fifty
/// are inaudible.
const AUDIO_TOLERANCE: u64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weak {
    None,
    Ours,
    Theirs,
}

/// The counters that say something about health, at one moment.
#[derive(Clone, Copy, Default)]
struct Counts {
    congested: u64,
    late: u64,
    audio_send_dropped: u64,
    received_dropped: u64,
    audio_late: u64,
    audio_concealed: u64,
}

impl Counts {
    fn read(stats: &MediaStats) -> Self {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Self {
            congested: get(&stats.frames_dropped_congested),
            late: get(&stats.frames_late),
            audio_send_dropped: get(&stats.audio_send_dropped),
            received_dropped: get(&stats.frames_dropped_received),
            audio_late: get(&stats.audio_late),
            audio_concealed: get(&stats.audio_concealed),
        }
    }

    const fn ours_bad(&self, before: &Self) -> bool {
        self.congested - before.congested + self.late - before.late + self.audio_send_dropped
            - before.audio_send_dropped
            > 0
    }

    const fn theirs_bad(&self, before: &Self) -> bool {
        self.received_dropped > before.received_dropped
            || self.audio_late - before.audio_late + self.audio_concealed - before.audio_concealed
                > AUDIO_TOLERANCE
    }
}

/// One side's pill, shown and cleared with some patience.
#[derive(Default)]
struct Streak {
    shown: bool,
    run: u32,
}

impl Streak {
    const fn sample(&mut self, bad: bool) {
        if bad == self.shown {
            self.run = 0;
            return;
        }
        self.run += 1;
        if self.run >= if bad { SHOW_AFTER } else { CLEAR_AFTER } {
            self.shown = bad;
            self.run = 0;
        }
    }
}

#[derive(Default)]
pub struct Health {
    last: Option<Counts>,
    ours: Streak,
    theirs: Streak,
}

impl Health {
    /// Takes the call's counters once a period and says which pill, if any, to show.
    pub fn sample(&mut self, stats: &MediaStats) -> Weak {
        self.next(Counts::read(stats))
    }

    const fn next(&mut self, now: Counts) -> Weak {
        if let Some(before) = self.last.replace(now) {
            let ours = now.ours_bad(&before);
            self.ours.sample(ours);
            // Theirs only while ours is healthy: a phone that cannot send cannot judge the other.
            self.theirs.sample(!ours && !self.ours.shown && now.theirs_bad(&before));
        }
        if self.ours.shown {
            Weak::Ours
        } else if self.theirs.shown {
            Weak::Theirs
        } else {
            Weak::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(health: &mut Health, counts: &mut Counts, seconds: u32, step: impl Fn(&mut Counts)) -> Weak {
        let mut weak = Weak::None;
        for _ in 0..seconds {
            step(counts);
            weak = health.next(*counts);
        }
        weak
    }

    #[test]
    fn one_bad_second_shows_nothing() {
        let (mut health, mut counts) = (Health::default(), Counts::default());
        assert_eq!(run(&mut health, &mut counts, 1, |_| {}), Weak::None);
        assert_eq!(run(&mut health, &mut counts, 1, |c| c.late += 1), Weak::None);
        assert_eq!(run(&mut health, &mut counts, 1, |_| {}), Weak::None);
    }

    #[test]
    fn ours_shows_after_two_and_clears_after_five() {
        let (mut health, mut counts) = (Health::default(), Counts::default());
        run(&mut health, &mut counts, 1, |_| {});
        assert_eq!(run(&mut health, &mut counts, SHOW_AFTER, |c| c.congested += 3), Weak::Ours);
        assert_eq!(run(&mut health, &mut counts, CLEAR_AFTER - 1, |_| {}), Weak::Ours);
        assert_eq!(run(&mut health, &mut counts, 1, |_| {}), Weak::None);
    }

    #[test]
    fn theirs_needs_our_side_healthy() {
        let (mut health, mut counts) = (Health::default(), Counts::default());
        run(&mut health, &mut counts, 1, |_| {});
        let both = |c: &mut Counts| {
            c.received_dropped += 1;
            c.late += 1;
        };
        assert_eq!(run(&mut health, &mut counts, SHOW_AFTER, both), Weak::Ours);
        let mut health = Health::default();
        run(&mut health, &mut counts, 1, |_| {});
        assert_eq!(run(&mut health, &mut counts, SHOW_AFTER, |c| c.received_dropped += 1), Weak::Theirs);
    }

    #[test]
    fn a_few_concealed_packets_are_fine() {
        let (mut health, mut counts) = (Health::default(), Counts::default());
        run(&mut health, &mut counts, 1, |_| {});
        assert_eq!(run(&mut health, &mut counts, 10, |c| c.audio_concealed += AUDIO_TOLERANCE), Weak::None);
        assert_eq!(run(&mut health, &mut counts, SHOW_AFTER, |c| c.audio_late += AUDIO_TOLERANCE + 1), Weak::Theirs);
    }
}
