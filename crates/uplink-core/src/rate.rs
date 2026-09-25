//! How much video a call sends: at most its quality step's cap, and less while the path cannot
//! carry that. Sampled once a second from the call's own counters; nothing is asked of the peer,
//! since QUIC's round trip and losses already say what happens on the way there.
//!
//! Frames that could not go, a round trip climbing over the path's best (a queue filling
//! somewhere), or heavy loss cut the bitrate to a little under what got through. It then holds,
//! so the queue can drain before it is judged again, and grows back while the path stays clean.
//! When the bitrate stays under what the step below needs, the picture steps down to that
//! step's size and frame rate: a smaller picture at the same bitrate looks better than a starved
//! big one. It steps back up once the bitrate has had room for a while, and waits longer after
//! a step up that did not hold.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::media::{MediaStats, Route};
use crate::preset::Preset;

/// Below this a picture is not worth sending, and a path that cannot carry it is the stall
/// overlay's business, not this one's.
const FLOOR_KBPS: u32 = 150;
const PERCENT: u32 = 100;
/// A cut goes to what got through, less this much, so the queue it built can drain.
const CUT_TO_PERCENT: u32 = 85;
/// Never more than half at once: one second's count is noisy, a keyframe's burst most of all.
const MOST_CUT_PERCENT: u32 = 50;
/// Growth per clean sample: back from half in about nine seconds.
const GROWTH_PERCENT: u32 = 108;
/// Samples after a cut, or a new step, before the path is judged again: QUIC's smoothed round
/// trip lags, and a queue takes a moment to drain.
const HOLD_SAMPLES: u32 = 2;
/// A round trip this far over the path's best is a queue, not the distance.
const QUEUE_MS: u64 = 150;
/// The path's best is the least of this many samples, so a longer path after a change of
/// network, relay or family becomes the new best within a minute.
const BEST_OF: usize = 60;
/// Lost packets, as a share of those sent, that count as the path failing.
const LOSS_PERCENT: u64 = 5;
/// Samples under the step below's bitrate before the picture steps down.
const DOWN_AFTER: u32 = 5;
/// Samples with room for the step above before the picture steps up; doubled after a step up
/// that did not hold, up to the most.
const UP_AFTER: u32 = 10;
const UP_AFTER_MOST: u32 = 80;
/// Room for the step above: this much over the current step's own bitrate.
const UP_MARGIN_PERCENT: u32 = 125;
/// A step up that falls back within this many samples did not hold.
const PROBATION: u32 = DOWN_AFTER + UP_AFTER;

/// The call's counters at one moment.
#[derive(Clone, Copy, Debug)]
pub struct Reading {
    at: Instant,
    /// Frames of ours that could not go: dropped before sending, or reset late.
    stuck: u64,
    bytes_sent: u64,
    lost_packets: u64,
    datagrams_sent: u64,
    /// Gauges, not counters: read as they are.
    rtt_ms: u64,
    route: Route,
}

impl Reading {
    pub fn read(stats: &MediaStats, at: Instant) -> Self {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Self {
            at,
            stuck: get(&stats.frames_dropped_congested) + get(&stats.frames_late),
            bytes_sent: get(&stats.bytes_sent),
            lost_packets: get(&stats.lost_packets),
            datagrams_sent: get(&stats.datagrams_sent),
            rtt_ms: get(&stats.rtt_ms),
            route: stats.route(),
        }
    }
}

/// What the encoder has to change, if anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    None,
    /// The same picture at this many kbps.
    Bitrate(u32),
    /// Another step's picture, starting at this many kbps.
    Step(Preset, u32),
}

/// The best round trip lately on the path the call is on.
#[derive(Default)]
struct Best {
    route: Route,
    recent: VecDeque<u64>,
}

impl Best {
    /// Whether `rtt_ms` is a queue over the best of the recent ones, this one included.
    fn queued(&mut self, route: Route, rtt_ms: u64) -> bool {
        // Nothing measured yet.
        if rtt_ms == 0 {
            return false;
        }
        // 600 ms relayed is not a queue over 110 ms direct.
        if route != self.route {
            self.route = route;
            self.recent.clear();
        }
        if self.recent.len() == BEST_OF {
            self.recent.pop_front();
        }
        self.recent.push_back(rtt_ms);
        self.recent.iter().min().is_some_and(|best| rtt_ms >= best + QUEUE_MS)
    }
}

pub struct Rate {
    /// The steps this call may send at, lowest first: what the phone can send, up to the cap.
    steps: Vec<Preset>,
    step: Preset,
    target_kbps: u32,
    /// The cap's own bitrate: the most the target grows to, whichever step is showing.
    most_kbps: u32,
    last: Option<Reading>,
    best: Best,
    hold: u32,
    below: u32,
    above: u32,
    up_after: u32,
    /// Samples since the picture last stepped up.
    since_up: Option<u32>,
}

impl Rate {
    /// Starts at `cap`, all of it: the path has not said otherwise yet.
    pub fn new(cap: Preset, sendable: &[Preset]) -> Self {
        let mut steps: Vec<Preset> = sendable.iter().copied().filter(|step| *step <= cap).collect();
        if !steps.contains(&cap) {
            steps.push(cap);
        }
        steps.sort_unstable();
        let most_kbps = cap.video().kbps;
        Self {
            steps,
            step: cap,
            target_kbps: most_kbps,
            most_kbps,
            last: None,
            best: Best::default(),
            hold: 0,
            below: 0,
            above: 0,
            up_after: UP_AFTER,
            since_up: None,
        }
    }

    pub const fn step(&self) -> Preset {
        self.step
    }

    /// What the encoder should run at: the target, within the step's own cap.
    pub fn kbps(&self) -> u32 {
        self.target_kbps.min(self.step.video().kbps)
    }

    /// The call stopped for a while (reconnecting, or nothing arriving): what piled up meanwhile
    /// says nothing about the path it comes back on, so the next sample starts afresh.
    pub fn pause(&mut self) {
        self.last = None;
        self.best = Best::default();
    }

    /// Takes the call's counters once a second and says what the encoder should change.
    pub fn sample(&mut self, now: Reading) -> Change {
        let Some(before) = self.last.replace(now) else { return Change::None };
        let (kbps_before, step_before) = (self.kbps(), self.step);
        self.judge(&before, &now);
        self.move_step();
        let kbps = self.kbps();
        if self.step != step_before {
            Change::Step(self.step, kbps)
        } else if kbps != kbps_before {
            Change::Bitrate(kbps)
        } else {
            Change::None
        }
    }

    /// Cuts, holds or grows the target.
    fn judge(&mut self, before: &Reading, now: &Reading) {
        let queued = self.best.queued(now.route, now.rtt_ms);
        if self.hold > 0 {
            self.hold -= 1;
            return;
        }
        let stuck = now.stuck > before.stuck;
        let lost = now.lost_packets.saturating_sub(before.lost_packets);
        let sent = now.datagrams_sent.saturating_sub(before.datagrams_sent);
        let lossy = sent > 0 && lost * u64::from(PERCENT) >= sent * LOSS_PERCENT;
        if stuck || queued || lossy {
            let millis = u64::try_from(now.at.duration_since(before.at).as_millis()).unwrap_or(u64::MAX).max(1);
            // Bits per millisecond is kbit/s.
            let through = now.bytes_sent.saturating_sub(before.bytes_sent).saturating_mul(u64::from(u8::BITS)) / millis;
            let through = u32::try_from(through).unwrap_or(u32::MAX);
            let (least, most) =
                (self.target_kbps * MOST_CUT_PERCENT / PERCENT, self.target_kbps * CUT_TO_PERCENT / PERCENT);
            let cut = (through / PERCENT * CUT_TO_PERCENT).clamp(least, most);
            tracing::info!(stuck, queued, lossy, rtt_ms = now.rtt_ms, through, kbps = cut, "video bitrate cut");
            self.target_kbps = cut.max(FLOOR_KBPS);
            self.hold = HOLD_SAMPLES;
        } else {
            self.target_kbps = (self.target_kbps * GROWTH_PERCENT / PERCENT).min(self.most_kbps);
        }
    }

    /// Steps the picture down after the bitrate has stayed under what the step below needs, or up
    /// after it has had room for the step above.
    fn move_step(&mut self) {
        let at = self.steps.iter().position(|step| *step == self.step).unwrap_or_default();
        let lower = at.checked_sub(1).and_then(|i| self.steps.get(i)).copied();
        let higher = self.steps.get(at + 1).copied();
        self.since_up = self.since_up.map(|samples| samples.saturating_add(1));
        if self.since_up.is_some_and(|samples| samples > PROBATION) {
            // It held: the next step up waits no longer than the first.
            (self.since_up, self.up_after) = (None, UP_AFTER);
        }
        let starved = lower.is_some_and(|lower| self.target_kbps < lower.video().kbps);
        let roomy = higher.is_some() && self.target_kbps >= self.step.video().kbps * UP_MARGIN_PERCENT / PERCENT;
        (self.below, self.above) = (if starved { self.below + 1 } else { 0 }, if roomy { self.above + 1 } else { 0 });
        let next = if self.below >= DOWN_AFTER {
            if self.since_up.is_some() {
                self.up_after = (self.up_after * 2).min(UP_AFTER_MOST);
                self.since_up = None;
            }
            lower
        } else if self.above >= self.up_after {
            self.since_up = Some(0);
            higher
        } else {
            None
        };
        if let Some(next) = next {
            self.step = next;
            (self.below, self.above) = (0, 0);
            // A new encoder and the camera reopened onto it: frames stall a moment either way.
            self.hold = HOLD_SAMPLES;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    /// A path that carries `kbps` of it a second and answers in `rtt_ms`.
    struct Path {
        at: Instant,
        reading: Reading,
    }

    impl Path {
        fn new(rtt_ms: u64) -> Self {
            let at = Instant::now();
            let reading = Reading {
                at,
                stuck: 0,
                bytes_sent: 0,
                lost_packets: 0,
                datagrams_sent: 0,
                rtt_ms,
                route: Route::Direct,
            };
            Self { at, reading }
        }

        /// One second: `kbps` through, and whatever `change` does to the rest.
        fn second(&mut self, rate: &mut Rate, kbps: u32, change: impl Fn(&mut Reading)) -> Change {
            self.at += SECOND;
            self.reading.at = self.at;
            self.reading.bytes_sent += u64::from(kbps) * 1000 / u64::from(u8::BITS);
            self.reading.datagrams_sent += 100;
            change(&mut self.reading);
            rate.sample(self.reading)
        }

        /// `seconds` of a clean path carrying all the rate sends.
        fn clean(&mut self, rate: &mut Rate, seconds: u32) -> Vec<Change> {
            (0..seconds).map(|_| self.second(rate, rate.kbps(), |_| {})).collect()
        }

        /// `seconds` of a path that carries only `kbps`, dropping the rest.
        fn narrow(&mut self, rate: &mut Rate, kbps: u32, seconds: u32) -> Vec<Change> {
            (0..seconds)
                .map(|_| {
                    let over = rate.kbps() > kbps;
                    self.second(rate, kbps.min(rate.kbps()), |r| r.stuck += u64::from(over))
                })
                .collect()
        }
    }

    fn started(cap: Preset, rtt_ms: u64) -> (Rate, Path) {
        let (mut rate, path) = (Rate::new(cap, &Preset::ALL), Path::new(rtt_ms));
        assert_eq!(rate.sample(path.reading), Change::None, "the first reading only starts the count");
        (rate, path)
    }

    #[test]
    fn a_clean_path_keeps_the_cap() {
        let (mut rate, mut path) = started(Preset::High, 30);
        assert!(path.clean(&mut rate, 30).iter().all(|change| *change == Change::None));
        assert_eq!((rate.step(), rate.kbps()), (Preset::High, Preset::High.video().kbps));
    }

    #[test]
    fn frames_that_cannot_go_cut_to_under_what_got_through() {
        let (mut rate, mut path) = started(Preset::High, 30);
        let cap = Preset::High.video().kbps;
        let change = path.second(&mut rate, cap * 3 / 4, |r| r.stuck += 1);
        assert_eq!(change, Change::Bitrate(cap * 3 / 4 / PERCENT * CUT_TO_PERCENT));
        // Held while the queue drains, then back up while it stays clean.
        let kbps = rate.kbps();
        let held = path.second(&mut rate, kbps, |r| r.stuck += 1);
        assert_eq!(held, Change::None, "no second cut inside the hold");
        path.clean(&mut rate, 1);
        assert!(matches!(path.clean(&mut rate, 1)[..], [Change::Bitrate(kbps)] if kbps > cap * 3 / 4 * 85 / 100));
        path.clean(&mut rate, 20);
        assert_eq!(rate.kbps(), cap);
    }

    #[test]
    fn one_cut_is_at_most_half() {
        let (mut rate, mut path) = started(Preset::Balanced, 30);
        let change = path.second(&mut rate, 0, |r| r.stuck += 8);
        assert_eq!(change, Change::Bitrate(Preset::Balanced.video().kbps * MOST_CUT_PERCENT / PERCENT));
    }

    #[test]
    fn a_climbing_round_trip_is_a_queue_but_a_longer_path_is_not() {
        let (mut rate, mut path) = started(Preset::Low, 110);
        path.clean(&mut rate, 3);
        let kbps = rate.kbps();
        path.second(&mut rate, kbps, |r| r.rtt_ms = 110 + QUEUE_MS);
        assert!(rate.kbps() < kbps, "a queue building");
        let mut relayed = started(Preset::Low, 110);
        relayed.1.clean(&mut relayed.0, 3);
        relayed.1.second(&mut relayed.0, kbps, |r| (r.route, r.rtt_ms) = (Route::Relay, 618));
        assert_eq!(relayed.0.kbps(), kbps, "a new path, a new best");
    }

    #[test]
    fn heavy_loss_cuts() {
        let (mut rate, mut path) = started(Preset::Low, 30);
        let kbps = rate.kbps();
        // Of the hundred datagrams a second sends.
        path.second(&mut rate, kbps, |r| r.lost_packets += LOSS_PERCENT);
        assert!(rate.kbps() < Preset::Low.video().kbps);
    }

    #[test]
    fn the_picture_steps_down_when_starved_and_back_up_with_room() {
        let (mut rate, mut path) = started(Preset::High, 30);
        let changes = path.narrow(&mut rate, 1200, 30);
        assert!(changes.iter().any(|change| matches!(change, Change::Step(Preset::Balanced, _))));
        assert_eq!(rate.step(), Preset::Balanced, "1.2 Mbps is room for 720p, not 1080p");
        path.clean(&mut rate, 60);
        assert_eq!(rate.step(), Preset::High);
        assert_eq!(rate.kbps(), Preset::High.video().kbps);
    }

    #[test]
    fn a_step_up_that_does_not_hold_waits_longer_next_time() {
        let (mut rate, mut path) = started(Preset::Balanced, 30);
        path.narrow(&mut rate, 700, 30);
        assert_eq!(rate.step(), Preset::Low);
        let mut waited = 0;
        while rate.step() == Preset::Low {
            path.clean(&mut rate, 1);
            waited += 1;
            assert!(waited < 60, "never stepped back up");
        }
        path.narrow(&mut rate, 700, PROBATION);
        assert_eq!(rate.step(), Preset::Low);
        assert_eq!(rate.up_after, UP_AFTER * 2);
    }

    #[test]
    fn never_under_the_floor_nor_the_lowest_step() {
        let (mut rate, mut path) = started(Preset::Low, 30);
        path.narrow(&mut rate, 0, 60);
        assert_eq!((rate.step(), rate.kbps()), (Preset::Lowest, FLOOR_KBPS));
    }

    #[test]
    fn only_steps_the_phone_can_send() {
        let mut rate = Rate::new(Preset::High, &[Preset::Low, Preset::High]);
        let mut path = Path::new(30);
        rate.sample(path.reading);
        path.narrow(&mut rate, 700, 30);
        assert_eq!(rate.step(), Preset::Low, "no 720p on this phone: 1080p to 540p");
    }
}
