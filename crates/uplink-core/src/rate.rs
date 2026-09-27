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
//!
//! Three things keep it from climbing into the same wall again, after WebRTC's congestion
//! control: growth never runs past half again what actually went out, so a step that holds the
//! encoder back cannot grow a target nothing tested; the rate each cut came at is remembered,
//! and approached slowly; and a step whose encoder puts out more than it is set to (a HiSilicon
//! one would not go under 2.5 Mbps at 720p) is judged on what it really sends.

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
/// Growth per clean sample: back from half in about four seconds, and from the floor to the
/// highest cap in under half a minute. It stops at the first queue or loss, and a cut then goes
/// to under what got through, so a faster climb overshoots a bottleneck by a second at most.
const GROWTH_PERCENT: u32 = 120;
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
const UP_AFTER: u32 = 5;
const UP_AFTER_MOST: u32 = 40;
/// Room for the step above: this much over the current step's own bitrate.
const UP_MARGIN_PERCENT: u32 = 125;
/// A step up that falls back within this many samples did not hold: a home router's queue took
/// twenty to fifty seconds to show one that did not.
const PROBATION: u32 = 60;
/// Growth stops at this share of what went out, plus a little so a small rate can still grow.
const CEILING_PERCENT: u32 = 150;
const CEILING_KBPS: u32 = 10;
/// Within this band around the rate the last cuts came at, growth is this share of it a second
/// instead of [`GROWTH_PERCENT`]; past the top of it the path has more room than it had, and
/// the memory goes.
const NEAR_BELOW_PERCENT: u32 = 90;
const NEAR_ABOVE_PERCENT: u32 = 110;
const NEAR_GROWTH_PERCENT: u32 = 3;
/// The encoder putting out this much over what it is set to, on this many clean samples in a
/// row, is sending what it will, not what it is told: that step's real cost. One sample is not
/// enough: a keyframe (after every step, and whenever the peer asks under loss) is a burst that
/// read as 700 kbps at a 187 kbps setting under netem.
const OVERSHOOT_PERCENT: u32 = 130;
const OVERSHOOT_SAMPLES: u32 = 5;
/// Samples a step's learned floor is trusted before it is tried again: the path may carry it
/// now, and nothing but trying can say. A try that fails is a step up that did not hold.
const FLOOR_KEPT: u64 = 120;

/// The call's counters at one moment.
#[derive(Clone, Copy, Debug)]
pub struct Reading {
    at: Instant,
    /// Frames of ours that could not go: dropped before sending, or reset late.
    stuck: u64,
    bytes_sent: u64,
    /// What our encoder put out, sent or not.
    bytes_encoded: u64,
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
            bytes_encoded: get(&stats.bytes_encoded),
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

/// The bitrate that has room for the step above `step`: a margin over `step`'s own.
const fn room_above(step: Preset) -> u32 {
    step.video().kbps * UP_MARGIN_PERCENT / PERCENT
}

/// kbit/s between two byte counts `millis` apart.
fn kbps_between(before: u64, now: u64, millis: u64) -> u32 {
    // Bits per millisecond is kbit/s.
    let kbps = now.saturating_sub(before).saturating_mul(u64::from(u8::BITS)) / millis;
    u32::try_from(kbps).unwrap_or(u32::MAX)
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
    /// The rate the path's recent cuts came at, while it still describes the path.
    capacity: Option<u32>,
    /// What each of `steps` really sends at the least, once its encoder has shown it will not
    /// go under, and the sample that last showed it.
    floors: Vec<Option<(u32, u64)>>,
    /// Samples taken, as a clock for `floors`.
    samples: u64,
    /// Clean samples in a row with the encoder well over its setting, and the least it sent.
    over: u32,
    over_least: u32,
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
        let floors = vec![None; steps.len()];
        Self {
            steps,
            step: cap,
            target_kbps: most_kbps,
            most_kbps,
            last: None,
            best: Best::default(),
            capacity: None,
            floors,
            samples: 0,
            over: 0,
            over_least: 0,
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
        self.capacity = None;
    }

    /// Takes the call's counters once a second and says what the encoder should change.
    pub fn sample(&mut self, now: Reading) -> Change {
        let Some(before) = self.last.replace(now) else { return Change::None };
        self.samples += 1;
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
        let millis = u64::try_from(now.at.duration_since(before.at).as_millis()).unwrap_or(u64::MAX).max(1);
        let through = kbps_between(before.bytes_sent, now.bytes_sent, millis);
        let encoded = kbps_between(before.bytes_encoded, now.bytes_encoded, millis);
        // A cut on one path says nothing about the next.
        if now.route != before.route {
            self.capacity = None;
        }
        let queued = self.best.queued(now.route, now.rtt_ms);
        if self.hold > 0 {
            self.hold -= 1;
            self.over = 0;
            return;
        }
        let stuck = now.stuck > before.stuck;
        let lost = now.lost_packets.saturating_sub(before.lost_packets);
        let sent = now.datagrams_sent.saturating_sub(before.datagrams_sent);
        let lossy = sent > 0 && lost * u64::from(PERCENT) >= sent * LOSS_PERCENT;
        let failing = stuck || queued || lossy;
        self.learn_floor(encoded, !failing);
        if failing {
            let (least, most) =
                (self.target_kbps * MOST_CUT_PERCENT / PERCENT, self.target_kbps * CUT_TO_PERCENT / PERCENT);
            let cut = (through / PERCENT * CUT_TO_PERCENT).clamp(least, most);
            self.capacity = Some(self.capacity.map_or(through, |capacity| capacity.midpoint(through)));
            self.target_kbps = cut.max(FLOOR_KBPS);
            tracing::info!(
                stuck,
                queued,
                lossy,
                rtt_ms = now.rtt_ms,
                through,
                kbps = self.target_kbps,
                capacity = self.capacity,
                "video bitrate cut"
            );
            self.hold = HOLD_SAMPLES;
        } else {
            self.target_kbps = self.grown(through);
        }
    }

    /// The target after a clean sample: fast far from the rate the path last failed at, slow
    /// near it, and never past half again what actually went out.
    fn grown(&mut self, through: u32) -> u32 {
        let target = self.target_kbps;
        let ceiling = through.saturating_mul(CEILING_PERCENT) / PERCENT + CEILING_KBPS;
        let fast = target.saturating_mul(GROWTH_PERCENT) / PERCENT;
        let next = match self.capacity {
            Some(capacity) if target.saturating_mul(PERCENT) > capacity.saturating_mul(NEAR_ABOVE_PERCENT) => {
                // Clean well past it: the path has more room than it had.
                self.capacity = None;
                fast
            }
            Some(capacity) if target.saturating_mul(PERCENT) >= capacity.saturating_mul(NEAR_BELOW_PERCENT) => {
                target + (capacity * NEAR_GROWTH_PERCENT / PERCENT).max(1)
            }
            _ => fast,
        };
        next.min(ceiling.max(target)).min(self.most_kbps)
    }

    /// Notes what this step's encoder really sends, once it has sent well over what it is set to
    /// for [`OVERSHOOT_SAMPLES`] clean samples in a row.
    fn learn_floor(&mut self, encoded: u32, clean: bool) {
        let set = self.kbps();
        if !clean || encoded.saturating_mul(PERCENT) <= set.saturating_mul(OVERSHOOT_PERCENT) {
            self.over = 0;
            return;
        }
        self.over_least = if self.over == 0 { encoded } else { self.over_least.min(encoded) };
        self.over += 1;
        if self.over < OVERSHOOT_SAMPLES {
            return;
        }
        let at = self.steps.iter().position(|step| *step == self.step).unwrap_or_default();
        let known = self.floor(at);
        let learned = if known == 0 { self.over_least } else { known.min(self.over_least) };
        if learned != known {
            tracing::info!(step = ?self.step, set, sends = learned, "encoder sends over its bitrate");
        }
        if let Some(floor) = self.floors.get_mut(at) {
            *floor = Some((learned, self.samples));
        }
    }

    /// What the step at position `at` has lately shown it sends at the least; zero otherwise.
    fn floor(&self, at: usize) -> u32 {
        match self.floors.get(at).copied().flatten() {
            Some((kbps, seen)) if self.samples.saturating_sub(seen) <= FLOOR_KEPT => kbps,
            _ => 0,
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
        // Starved: under what the step below needs, or under what this one really sends.
        let starved =
            lower.is_some_and(|lower| self.target_kbps < lower.video().kbps || self.target_kbps < self.floor(at));
        let roomy =
            higher.is_some() && self.target_kbps >= room_above(self.step) && self.target_kbps >= self.floor(at + 1);
        (self.below, self.above) = (if starved { self.below + 1 } else { 0 }, if roomy { self.above + 1 } else { 0 });
        let next = if self.below >= DOWN_AFTER {
            if self.since_up.is_some() {
                self.up_after = (self.up_after * 2).min(UP_AFTER_MOST);
                self.since_up = None;
            }
            lower
        } else if self.above >= self.up_after {
            self.since_up = Some(0);
            // As high as the bitrate has room for, not one step a wait: back from the floor, a
            // step at a time was a minute and more of a small picture on a path that was fine.
            self.steps.get(at + 1..).and_then(|above| {
                let mut below = self.step;
                above
                    .iter()
                    .copied()
                    .zip(at + 1..)
                    .take_while(|&(step, index)| {
                        let room = self.target_kbps >= room_above(below) && self.target_kbps >= self.floor(index);
                        below = step;
                        room
                    })
                    .last()
                    .map(|(step, _)| step)
            })
        } else {
            None
        };
        if let Some(next) = next {
            self.step = next;
            (self.below, self.above, self.over) = (0, 0, 0);
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
                bytes_encoded: 0,
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
            // The encoder puts out what it is set to; a test that says otherwise changes this.
            self.reading.bytes_encoded += u64::from(rate.kbps()) * 1000 / u64::from(u8::BITS);
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

    /// The path from the netem squeeze test: starved to the floor, then clean again. Growth never
    /// runs past half again what went out, so no step is skipped on a rate nothing tested; each
    /// waits its [`UP_AFTER`] and the new encoder's hold. Under half a minute when it could skip.
    #[test]
    fn back_from_the_floor_a_step_at_a_time_within_three_quarters_of_a_minute() {
        const BACK_WITHIN: u32 = 45;
        let (mut rate, mut path) = started(Preset::Highest, 30);
        path.narrow(&mut rate, 0, 60);
        assert_eq!(rate.step(), Preset::Lowest);
        let mut steps = Vec::new();
        let mut waited = 0;
        while rate.step() != Preset::Highest {
            if let Some(Change::Step(step, _)) = path.clean(&mut rate, 1).pop() {
                steps.push(step);
            }
            waited += 1;
            assert!(waited <= BACK_WITHIN, "still at {:?} after {waited} s ({steps:?})", rate.step());
        }
        assert_eq!(steps, [Preset::Low, Preset::Balanced, Preset::High, Preset::Highest]);
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

    /// The field log's cycle: a lower step holds the encoder back, the target must not grow past
    /// half again what went out, so it cannot step up on a rate nothing tested.
    #[test]
    fn a_held_back_step_does_not_grow_a_target_nothing_tested() {
        let (mut rate, mut path) = started(Preset::High, 30);
        path.narrow(&mut rate, 1200, 30);
        assert_eq!(rate.step(), Preset::Balanced);
        // Clean again, but the step holds the encoder at its own bitrate.
        path.clean(&mut rate, 3);
        let sent = rate.kbps();
        assert!(
            rate.target_kbps <= sent * CEILING_PERCENT / PERCENT + CEILING_KBPS,
            "target {} over {sent}",
            rate.target_kbps
        );
    }

    /// After a cut the path's rate is remembered: growth near it is a few percent a second, not a
    /// fifth, so it creeps back to where it failed rather than leaping past it.
    #[test]
    fn growth_is_slow_near_where_the_path_last_failed() {
        let (mut rate, mut path) = started(Preset::High, 30);
        let _ = path.second(&mut rate, 3000, |r| r.stuck += 1);
        path.clean(&mut rate, HOLD_SAMPLES + 1);
        let capacity = rate.capacity.unwrap_or_default();
        assert_eq!(capacity, 3000);
        // Into the band around it, then a second of growth inside it.
        while rate.target_kbps * PERCENT < capacity * NEAR_BELOW_PERCENT {
            path.clean(&mut rate, 1);
        }
        let before = rate.target_kbps;
        path.clean(&mut rate, 1);
        assert!(
            rate.target_kbps - before <= capacity * NEAR_GROWTH_PERCENT / PERCENT,
            "{before} → {}",
            rate.target_kbps
        );
    }

    /// A HiSilicon encoder at 720p would not go under about 2.5 Mbps whatever it was set to. Once
    /// seen, 720p costs that: a target under it steps down, and cannot step back up until the
    /// floor is old enough to be worth trying again.
    #[test]
    fn a_step_is_judged_on_what_its_encoder_really_sends() {
        const SENDS: u32 = 2500;
        let balanced = Preset::ALL.iter().position(|step| *step == Preset::Balanced).unwrap_or_default();
        let (mut rate, mut path) = started(Preset::Balanced, 30);
        let _ = path.second(&mut rate, 1500, |r| r.stuck += 1);
        // The encoder puts out SENDS a second, whatever it is set to.
        let changes: Vec<Change> = (0..HOLD_SAMPLES + OVERSHOOT_SAMPLES + DOWN_AFTER + 1)
            .map(|_| {
                let set = rate.kbps();
                let extra = u64::from(SENDS.saturating_sub(set)) * 1000 / u64::from(u8::BITS);
                path.second(&mut rate, set, |r| r.bytes_encoded += extra)
            })
            .collect();
        assert_eq!(rate.floor(balanced), SENDS);
        assert!(changes.iter().any(|change| matches!(change, Change::Step(Preset::Low, _))), "{changes:?}");
        // Clean from here, with an honest encoder at 540p: not back to 720p while the floor holds.
        path.clean(&mut rate, 60);
        assert_eq!(rate.step(), Preset::Low);
        // Old enough to try again.
        let mut waited = 0;
        while rate.step() == Preset::Low {
            path.clean(&mut rate, 1);
            waited += 1;
            assert!(waited <= u32::try_from(FLOOR_KEPT).unwrap_or(u32::MAX), "never tried 720p again");
        }
    }

    /// Netem, 2026-09-27: under a squeeze, the keyframes after each step and the peer's asks
    /// read as the encoder sending 700 kbps at a 187 kbps setting. Learned as Low's floor, that
    /// kept the picture at the lowest step for the rest of the call once the path was clean.
    #[test]
    fn keyframe_bursts_on_a_failing_path_teach_no_floor() {
        let (mut rate, mut path) = started(Preset::High, 30);
        for _ in 0..30 {
            let set = rate.kbps();
            let burst = u64::from(set * 3) * 1000 / u64::from(u8::BITS);
            let _ = path
                .second(&mut rate, set / 4, |r| (r.stuck, r.bytes_encoded) = (r.stuck + 1, r.bytes_encoded + burst));
        }
        assert_eq!(rate.step(), Preset::Lowest);
        assert!(rate.floors.iter().all(Option::is_none), "{:?}", rate.floors);
        path.clean(&mut rate, 45);
        assert_eq!(rate.step(), Preset::High, "back once clean");
    }
}
