//! Whether a call that still works is struggling, and on whose side: the weak pill on the call
//! screen. Judged from this phone's own counters only; nothing is asked of the other one.
//!
//! Ours: what we send backs up, so frames are dropped before sending or reset after missing their
//! deadline, or audio does not fit the send buffer; or the round trip climbs, which is a queue
//! filling somewhere before any of that shows. Theirs: what arrives is late or has gaps. Both at
//! once is a path struggling both ways, which one phone cannot place: a bottleneck at either end
//! slows both directions, so it blames nobody. A call that has stopped altogether is the
//! reconnecting overlay's, not this.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::media::{MediaStats, Route};

/// Bad samples in a row before the pill shows: one late frame is not a weak connection.
const SHOW_AFTER: u32 = 2;
/// Good samples in a row before it goes, so it does not blink while a link hovers at the edge.
const CLEAR_AFTER: u32 = 5;
/// Audio packets per sample that may arrive late or be concealed before it counts: a few in fifty
/// are inaudible.
const AUDIO_TOLERANCE: u64 = 2;
/// Samples their side goes unjudged after our own audio restarts. A switch of output reopens the
/// voice streams about 0.7 s later, and the gap it leaves lands in the sample after that.
const EXCUSED_SAMPLES: u32 = 3;
/// How far over the path's best the round trip may go: twice it, and at least this much more, so a
/// 30 ms direct path wobbling to 70 ms does not count.
const RTT_CLIMB: u64 = 2;
const RTT_MARGIN_MS: u64 = 150;
/// A round trip past this is weak however the call started: talk turns into taking turns.
const RTT_CEILING_MS: u64 = 1500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weak {
    None,
    Ours,
    Theirs,
    /// Both ways: weak, but whose cannot be told from here.
    Both,
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
    /// Gauges, not counters: read as they are.
    rtt_ms: u64,
    route: Route,
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
            rtt_ms: get(&stats.rtt_ms),
            route: stats.route(),
        }
    }

    const fn ours_bad(&self, before: &Self) -> bool {
        self.congested - before.congested + self.late - before.late + self.audio_send_dropped
            - before.audio_send_dropped
            > 0
    }

    const fn theirs_bad(&self, before: &Self) -> bool {
        self.received_dropped > before.received_dropped
            || self.audio_late - before.audio_late + self.audio_concealed - before.audio_concealed > AUDIO_TOLERANCE
    }
}

/// The best round trip seen on the path the call is on. A new path, relayed after direct or the
/// other way round, starts over: 600 ms relayed is not a climb from 110 ms direct.
#[derive(Default)]
struct Baseline {
    route: Route,
    lowest: u64,
}

impl Baseline {
    fn climbed(&mut self, route: Route, rtt_ms: u64) -> bool {
        // Nothing measured yet.
        if rtt_ms == 0 {
            return false;
        }
        if self.lowest == 0 || self.route != route {
            self.route = route;
            self.lowest = rtt_ms;
        } else if rtt_ms < self.lowest {
            self.lowest = rtt_ms;
        }
        rtt_ms >= RTT_CEILING_MS || (rtt_ms >= self.lowest * RTT_CLIMB && rtt_ms - self.lowest >= RTT_MARGIN_MS)
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
    baseline: Baseline,
    ours: Streak,
    theirs: Streak,
    /// Samples left in which their side is not judged, after our own playout restarted.
    excused: u32,
}

impl Health {
    /// Takes the call's counters once a period and says which pill, if any, to show.
    pub fn sample(&mut self, stats: &MediaStats) -> Weak {
        self.next(Counts::read(stats))
    }

    /// Our own audio restarted: a new output, a hold, a reopened stream. Playout pauses while it
    /// does, and what arrives meanwhile reads as late and concealed — our gap, not theirs — so
    /// their side goes unjudged for a few samples, and a streak building against them starts over.
    pub const fn ours_restarted(&mut self) {
        self.excused = EXCUSED_SAMPLES;
        if !self.theirs.shown {
            self.theirs.run = 0;
        }
    }

    fn next(&mut self, now: Counts) -> Weak {
        let climbed = self.baseline.climbed(now.route, now.rtt_ms);
        if let Some(before) = self.last.replace(now) {
            let ours = now.ours_bad(&before) || climbed;
            self.ours.sample(ours);
            if self.excused > 0 {
                self.excused -= 1;
            } else {
                self.theirs.sample(now.theirs_bad(&before));
            }
        }
        match (self.ours.shown, self.theirs.shown) {
            (true, true) => Weak::Both,
            (true, false) => Weak::Ours,
            (false, true) => Weak::Theirs,
            (false, false) => Weak::None,
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
    fn both_ways_blames_nobody() {
        let (mut health, mut counts) = (Health::default(), Counts::default());
        run(&mut health, &mut counts, 1, |_| {});
        let both = |c: &mut Counts| {
            c.received_dropped += 1;
            c.late += 1;
        };
        // The netem squeeze on the other phone's end: every frame late both ways. Blaming this
        // phone's connection for it was wrong.
        assert_eq!(run(&mut health, &mut counts, SHOW_AFTER, both), Weak::Both);
        let mut health = Health::default();
        run(&mut health, &mut counts, 1, |_| {});
        assert_eq!(run(&mut health, &mut counts, SHOW_AFTER, |c| c.received_dropped += 1), Weak::Theirs);
    }

    const DIRECT_MS: u64 = 113;
    const RELAYED_MS: u64 = 618;

    fn on(route: Route, rtt_ms: u64) -> impl Fn(&mut Counts) {
        move |c: &mut Counts| {
            c.route = route;
            c.rtt_ms = rtt_ms;
        }
    }

    #[test]
    fn a_climbing_round_trip_is_ours() {
        let (mut health, mut counts) = (Health::default(), Counts::default());
        assert_eq!(run(&mut health, &mut counts, 3, on(Route::Direct, DIRECT_MS)), Weak::None);
        let climbed = DIRECT_MS * RTT_CLIMB + RTT_MARGIN_MS;
        assert_eq!(run(&mut health, &mut counts, SHOW_AFTER, on(Route::Direct, climbed)), Weak::Ours);
        assert_eq!(run(&mut health, &mut counts, CLEAR_AFTER, on(Route::Direct, DIRECT_MS)), Weak::None);
    }

    #[test]
    fn a_new_route_is_a_new_baseline() {
        let (mut health, mut counts) = (Health::default(), Counts::default());
        run(&mut health, &mut counts, 3, on(Route::Direct, DIRECT_MS));
        assert_eq!(run(&mut health, &mut counts, 10, on(Route::Relay, RELAYED_MS)), Weak::None);
    }

    #[test]
    fn small_wobbles_and_the_ceiling() {
        let (mut health, mut counts) = (Health::default(), Counts::default());
        let fast = 30;
        run(&mut health, &mut counts, 3, on(Route::Direct, fast));
        // Over twice as long, but not by the margin.
        assert_eq!(run(&mut health, &mut counts, 10, on(Route::Direct, fast * 3)), Weak::None);
        let mut health = Health::default();
        assert_eq!(run(&mut health, &mut counts, SHOW_AFTER + 1, on(Route::Relay, RTT_CEILING_MS)), Weak::Ours);
    }

    #[test]
    fn a_few_concealed_packets_are_fine() {
        let (mut health, mut counts) = (Health::default(), Counts::default());
        run(&mut health, &mut counts, 1, |_| {});
        assert_eq!(run(&mut health, &mut counts, 10, |c| c.audio_concealed += AUDIO_TOLERANCE), Weak::None);
        assert_eq!(run(&mut health, &mut counts, SHOW_AFTER, |c| c.audio_late += AUDIO_TOLERANCE + 1), Weak::Theirs);
    }

    #[test]
    fn our_own_audio_restarting_is_not_their_weak_link() {
        let (mut health, mut counts) = (Health::default(), Counts::default());
        run(&mut health, &mut counts, 1, |_| {});
        let gap = |c: &mut Counts| {
            c.audio_late += AUDIO_TOLERANCE * 4;
            c.audio_concealed += AUDIO_TOLERANCE * 4;
        };
        health.ours_restarted();
        assert_eq!(run(&mut health, &mut counts, EXCUSED_SAMPLES, gap), Weak::None);
        // Past the excuse, the same gaps are theirs again.
        assert_eq!(run(&mut health, &mut counts, SHOW_AFTER, gap), Weak::Theirs);
    }
}
