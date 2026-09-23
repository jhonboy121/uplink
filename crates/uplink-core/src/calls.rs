//! The call log: what was attempted, which way it went, and how it ended.
//!
//! It shares the one connection with contacts and settings. A row is written when a call *ends*,
//! because until then its outcome is unknown.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::params;

use crate::db::Db;
use crate::{EndpointId, Error};
/// Enough to look back over, and small enough that the screen never pages.
const KEEP: i64 = 500;

/// How a call finished. `Missed` is ours to infer: the peer rang and nobody answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Answered,
    Missed,
    /// We declined theirs.
    Declined,
    /// They declined ours.
    Rejected,
    /// We hung up before they picked up.
    Cancelled,
    /// Rang out: we reached them and nobody answered.
    NoAnswer,
    /// Never reached them at all — nothing listening on that key, or no route to it.
    Unreachable,
    /// The network gave out, or the call failed for a reason worth reporting.
    Failed,
}

impl Outcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Missed => "missed",
            Self::Declined => "declined",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::NoAnswer => "no-answer",
            Self::Unreachable => "unreachable",
            Self::Failed => "failed",
        }
    }

    /// Anything unrecognised — a row from a newer build — reads as failed rather than being
    /// dropped, so the log never loses a call it cannot name.
    fn parse(text: &str) -> Self {
        match text {
            "answered" => Self::Answered,
            "missed" => Self::Missed,
            "declined" => Self::Declined,
            "rejected" => Self::Rejected,
            "cancelled" => Self::Cancelled,
            "no-answer" => Self::NoAnswer,
            "unreachable" => Self::Unreachable,
            _ => Self::Failed,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CallRecord {
    pub peer: EndpointId,
    pub incoming: bool,
    pub outcome: Outcome,
    pub at: SystemTime,
    /// Only for a call that was answered.
    pub duration: Option<Duration>,
}

pub struct CallLog {
    db: Db,
}

impl CallLog {
    pub fn open(db: Db) -> Result<Self, Error> {
        db.with(|db| {
            db.execute_batch(
                "CREATE TABLE IF NOT EXISTS calls (
                     id       INTEGER PRIMARY KEY AUTOINCREMENT,
                     peer     TEXT NOT NULL,
                     incoming INTEGER NOT NULL,
                     outcome  TEXT NOT NULL,
                     at       INTEGER NOT NULL,
                     seconds  INTEGER
                 );
                 CREATE INDEX IF NOT EXISTS calls_at ON calls (at DESC)",
            )?;
            Ok(())
        })?;
        Ok(Self { db })
    }

    pub fn record(&self, record: &CallRecord) -> Result<(), Error> {
        let at = i64::try_from(record.at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()).unwrap_or(i64::MAX);
        let seconds = record.duration.map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        self.db.with(|db| {
            db.execute(
                "INSERT INTO calls (peer, incoming, outcome, at, seconds) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![record.peer.to_string(), record.incoming, record.outcome.as_str(), at, seconds],
            )?;
            // Trimmed here rather than on a timer: the log only grows when a call ends.
            db.execute(
                "DELETE FROM calls WHERE id NOT IN (SELECT id FROM calls ORDER BY at DESC, id DESC LIMIT ?1)",
                params![KEEP],
            )?;
            Ok(())
        })
    }

    /// Most recent first.
    pub fn recent(&self, limit: i64) -> Result<Vec<CallRecord>, Error> {
        self.db.with(|db| {
            let mut statement = db.prepare(
                "SELECT peer, incoming, outcome, at, seconds FROM calls ORDER BY at DESC, id DESC LIMIT ?1",
            )?;
            let rows = statement.query_map(params![limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (peer, incoming, outcome, at, seconds) = row?;
                let Ok(peer) = peer.parse::<EndpointId>() else {
                    tracing::warn!(peer, "a logged key no longer parses; skipping");
                    continue;
                };
                out.push(CallRecord {
                    peer,
                    incoming,
                    outcome: Outcome::parse(&outcome),
                    at: UNIX_EPOCH + Duration::from_secs(u64::try_from(at).unwrap_or_default()),
                    duration: seconds.and_then(|s| u64::try_from(s).ok()).map(Duration::from_secs),
                });
            }
            Ok(out)
        })
    }

    /// How many calls came in and were never answered, which is what a tab badge would show.
    pub fn missed(&self) -> Result<i64, Error> {
        self.db
            .with(|db| Ok(db.query_row("SELECT COUNT(*) FROM calls WHERE outcome = 'missed'", [], |row| row.get(0))?))
    }

    pub fn clear(&self) -> Result<(), Error> {
        self.db.with(|db| {
            db.execute("DELETE FROM calls", [])?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use iroh::SecretKey;

    use super::*;

    fn record(incoming: bool, outcome: Outcome, at: SystemTime) -> CallRecord {
        CallRecord { peer: SecretKey::generate().public(), incoming, outcome, at, duration: None }
    }

    #[test]
    fn the_newest_call_is_first() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = CallLog::open(Db::open(dir.path())?)?;
        let old = UNIX_EPOCH + Duration::from_secs(1_000);
        let new = UNIX_EPOCH + Duration::from_secs(2_000);
        log.record(&record(true, Outcome::Missed, old))?;
        log.record(&record(false, Outcome::Answered, new))?;
        let recent = log.recent(10)?;
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].at, new);
        assert!(!recent[0].incoming);
        Ok(())
    }

    #[test]
    fn a_duration_survives_the_round_trip() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = CallLog::open(Db::open(dir.path())?)?;
        let mut call = record(false, Outcome::Answered, SystemTime::now());
        call.duration = Some(Duration::from_secs(252));
        log.record(&call)?;
        assert_eq!(log.recent(1)?[0].duration, Some(Duration::from_secs(252)));
        Ok(())
    }

    #[test]
    fn missed_calls_are_counted() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = CallLog::open(Db::open(dir.path())?)?;
        let now = SystemTime::now();
        log.record(&record(true, Outcome::Missed, now))?;
        log.record(&record(true, Outcome::Missed, now))?;
        log.record(&record(true, Outcome::Answered, now))?;
        assert_eq!(log.missed()?, 2);
        log.clear()?;
        assert_eq!(log.missed()?, 0);
        Ok(())
    }

    #[test]
    fn the_log_stops_growing() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = CallLog::open(Db::open(dir.path())?)?;
        for second in 0..KEEP + 20 {
            let at = UNIX_EPOCH + Duration::from_secs(u64::try_from(second).unwrap_or_default());
            log.record(&record(false, Outcome::Answered, at))?;
        }
        assert_eq!(i64::try_from(log.recent(KEEP * 2)?.len()).unwrap_or_default(), KEEP);
        Ok(())
    }

    #[test]
    fn an_unknown_outcome_reads_back_as_failed() {
        assert_eq!(Outcome::parse("something from a newer build"), Outcome::Failed);
        assert_eq!(Outcome::parse(Outcome::Declined.as_str()), Outcome::Declined);
    }
}
