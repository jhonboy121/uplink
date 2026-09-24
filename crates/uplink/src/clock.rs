//! Times as the phone's own clock shows them.

use std::cell::RefCell;
use std::time::{SystemTime, UNIX_EPOCH};

use rustc_hash::FxHashMap;
use uplink_android::platform::AppContext;

const SECONDS_PER_HOUR: i64 = 3_600;
const SECONDS_PER_DAY: i64 = 86_400;
const MINUTES_PER_HOUR: i64 = 60;

/// The log keeps UTC, and the time of day and the day boundaries both depend on where the phone
/// is — its zone asked of Android for each moment, so a call from before a daylight-saving change
/// still reads the way the clock read then.
pub struct LocalClock {
    context: AppContext,
    /// Offsets already asked for, by UTC hour: a list of a hundred calls would otherwise cross
    /// into Java three times a row. Daylight saving needs nothing more — each hour keeps the
    /// offset that was right for it — so only a change of zone empties this ([`Self::forget`]).
    /// Near enough: nearly every zone changes offset on the hour, and the few that do not could
    /// show a time half an hour off in that one hour of the year.
    offsets: RefCell<FxHashMap<i64, i64>>,
}

impl LocalClock {
    pub fn new(context: AppContext) -> Self {
        Self { context, offsets: RefCell::default() }
    }

    /// The phone moved to another zone, or its clock was set: every cached offset may be wrong.
    pub fn forget(&self) {
        self.offsets.borrow_mut().clear();
    }

    /// Seconds since the epoch as a wall clock here reads them. UTC if the zone cannot be read,
    /// which is wrong by the offset but never by more.
    fn local_seconds(&self, at: SystemTime) -> i64 {
        let epoch = match at.duration_since(UNIX_EPOCH) {
            Ok(since) => i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
            Err(before) => -i64::try_from(before.duration().as_secs()).unwrap_or(i64::MAX),
        };
        let hour = epoch.div_euclid(SECONDS_PER_HOUR);
        let cached = self.offsets.borrow().get(&hour).copied();
        let offset = cached.unwrap_or_else(|| {
            let offset = self.context.utc_offset(at).unwrap_or_else(|e| {
                tracing::warn!("reading the timezone: {e}");
                0
            });
            self.offsets.borrow_mut().insert(hour, offset);
            offset
        });
        epoch.saturating_add(offset)
    }

    /// Calendar days here between `at` and now, so a call at 23:00 is yesterday by the next
    /// morning, not today for another fourteen hours. Zero for anything today or later.
    pub fn days_ago(&self, at: SystemTime) -> i64 {
        let day = |moment| self.local_seconds(moment).div_euclid(SECONDS_PER_DAY);
        (day(SystemTime::now()) - day(at)).max(0)
    }

    /// "21:04". Time of day is what a log row wants; its group's heading carries the date.
    pub fn clock_of(&self, at: SystemTime) -> String {
        let minutes = self.local_seconds(at).rem_euclid(SECONDS_PER_DAY) / (SECONDS_PER_HOUR / MINUTES_PER_HOUR);
        format!("{:02}:{:02}", minutes / MINUTES_PER_HOUR, minutes % MINUTES_PER_HOUR)
    }
}
