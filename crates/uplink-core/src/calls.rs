//! The call log: what was attempted, which way it went, and how it ended.
//!
//! It shares the one connection with contacts and settings. A row is written when a call *ends*,
//! because until then its outcome is unknown.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};

use crate::db::Db;
use crate::node::{EndReason, Mode};
use crate::quality::Quality;
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
    /// One side needs an update before the two can call.
    Incompatible,
    /// Answered, and then the network went and did not come back in time.
    Lost,
    /// The network gave out, or the call failed for a reason worth reporting.
    Failed,
}

impl Outcome {
    /// How a call that ended for `reason` goes in the log. A hang-up means different things
    /// depending on which way the call went and whether anyone picked up; nothing else does.
    pub const fn of(reason: &EndReason, incoming: bool, answered: bool) -> Self {
        // A call someone picked up was answered, however it ended: a connection dropped a minute
        // in is not a call that "did not connect". Losing it is still worth saying.
        if answered {
            return match reason {
                EndReason::ConnectionLost => Self::Lost,
                _ => Self::Answered,
            };
        }
        match reason {
            EndReason::Busy | EndReason::Failed(_) | EndReason::ConnectionLost | EndReason::Refused => Self::Failed,
            EndReason::Incompatible { .. } => Self::Incompatible,
            EndReason::DialTimeout => Self::Unreachable,
            EndReason::NoAnswer => Self::NoAnswer,
            EndReason::Declined => Self::Declined,
            EndReason::Rejected => Self::Rejected,
            EndReason::LocalHangup | EndReason::RemoteHangup => {
                if answered {
                    Self::Answered
                } else if incoming {
                    // They rang off, or their ring timed out: either way nobody spoke.
                    Self::Missed
                } else {
                    Self::Cancelled
                }
            }
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Missed => "missed",
            Self::Declined => "declined",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::NoAnswer => "no-answer",
            Self::Unreachable => "unreachable",
            Self::Incompatible => "incompatible",
            Self::Lost => "lost",
            Self::Failed => "failed",
        }
    }

    /// Answered, however it ended.
    pub const fn answered(self) -> bool {
        matches!(self, Self::Answered | Self::Lost)
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
            "incompatible" => Self::Incompatible,
            "lost" => Self::Lost,
            _ => Self::Failed,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CallRecord {
    pub peer: EndpointId,
    pub incoming: bool,
    pub outcome: Outcome,
    /// How it was placed. Calls logged before voice calls read as video, which they were.
    pub mode: Mode,
    pub at: SystemTime,
    /// Only for a call that was answered.
    pub duration: Option<Duration>,
    /// How far into a voice call both sides agreed to switch to video.
    pub video_from: Option<Duration>,
    /// Only for a call that was answered, and only on calls logged since this was recorded.
    pub traffic: Option<Traffic>,
    /// How it went, for an answered call; see [`crate::quality`].
    pub quality: Option<Quality>,
}

/// What a call carried each way: media payload, video and audio, without QUIC's own overhead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Traffic {
    pub sent: u64,
    pub received: u64,
}

/// One row of the log, so a screen can point back at it: selecting calls to remove, for one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CallId(i64);

impl std::fmt::Display for CallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for CallId {
    type Err = std::num::ParseIntError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.parse().map(Self)
    }
}

/// A call as the log holds it: the record, and which row it is.
#[derive(Clone, Debug)]
pub struct Logged {
    pub id: CallId,
    pub call: CallRecord,
}

pub struct CallLog {
    db: Db,
}

/// What every read selects, in the order [`read_row`] takes it.
const COLUMNS: &str = "id, peer, incoming, outcome, at, seconds, sent, received, quality, voice, video_at";

/// Columns added after the table first shipped, with their types. `CREATE TABLE IF NOT EXISTS`
/// leaves an existing table as it was, so these are added to it on open when missing.
const ADDED: [(&str, &str); 5] =
    [("sent", "INTEGER"), ("received", "INTEGER"), ("quality", "BLOB"), ("voice", "INTEGER"), ("video_at", "INTEGER")];

/// A row as [`COLUMNS`] reads it, or `None` for one whose key no longer parses.
fn read_row(row: &rusqlite::Row) -> rusqlite::Result<Option<Logged>> {
    let peer = row.get::<_, String>(1)?;
    let Ok(peer_id) = peer.parse::<EndpointId>() else {
        tracing::warn!(peer, "a logged key no longer parses; skipping");
        return Ok(None);
    };
    let unsigned = |value: Option<i64>| value.and_then(|v| u64::try_from(v).ok());
    let (sent, received) = (unsigned(row.get(6)?), unsigned(row.get(7)?));
    let quality = row.get::<_, Option<Vec<u8>>>(8)?.and_then(|bytes| {
        Quality::from_bytes(&bytes)
            .inspect_err(|e| tracing::debug!("a call's quality summary no longer reads: {e}"))
            .ok()
    });
    let call = CallRecord {
        peer: peer_id,
        incoming: row.get(2)?,
        outcome: Outcome::parse(&row.get::<_, String>(3)?),
        mode: if row.get::<_, Option<bool>>(9)?.unwrap_or_default() { Mode::Voice } else { Mode::Video },
        at: UNIX_EPOCH + Duration::from_secs(unsigned(Some(row.get(4)?)).unwrap_or_default()),
        duration: unsigned(row.get(5)?).map(Duration::from_secs),
        video_from: unsigned(row.get(10)?).map(Duration::from_secs),
        traffic: sent.zip(received).map(|(sent, received)| Traffic { sent, received }),
        quality,
    };
    Ok(Some(Logged { id: CallId(row.get(0)?), call }))
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
                     seconds  INTEGER,
                     sent     INTEGER,
                     received INTEGER,
                     quality  BLOB,
                     voice    INTEGER,
                     video_at INTEGER
                 );
                 CREATE INDEX IF NOT EXISTS calls_at ON calls (at DESC)",
            )?;
            let existing = db
                .prepare("SELECT name FROM pragma_table_info('calls')")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            for (column, kind) in ADDED {
                if !existing.iter().any(|name| name == column) {
                    db.execute_batch(&format!("ALTER TABLE calls ADD COLUMN {column} {kind}"))?;
                }
            }
            Ok(())
        })?;
        Ok(Self { db })
    }

    pub fn record(&self, record: &CallRecord) -> Result<(), Error> {
        let at = i64::try_from(record.at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()).unwrap_or(i64::MAX);
        let seconds = record.duration.map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        let signed = |bytes: u64| i64::try_from(bytes).unwrap_or(i64::MAX);
        let (sent, received) = record.traffic.map(|t| (signed(t.sent), signed(t.received))).unzip();
        let quality = record.quality.as_ref().map(Quality::to_bytes);
        let video_at = record.video_from.map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        self.db.with(|db| {
            db.execute(
                "INSERT INTO calls (peer, incoming, outcome, at, seconds, sent, received, quality, voice, video_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    record.peer.to_string(),
                    record.incoming,
                    record.outcome.as_str(),
                    at,
                    seconds,
                    sent,
                    received,
                    quality,
                    record.mode == Mode::Voice,
                    video_at
                ],
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
    pub fn recent(&self, limit: i64) -> Result<Vec<Logged>, Error> {
        self.db.with(|db| {
            let mut statement =
                db.prepare(&format!("SELECT {COLUMNS} FROM calls ORDER BY at DESC, id DESC LIMIT ?1"))?;
            let rows = statement.query_map(params![limit], read_row)?;
            let mut out = Vec::new();
            for row in rows {
                out.extend(row?);
            }
            Ok(out)
        })
    }

    /// One call, for its details page; `None` once it has been removed or trimmed.
    pub fn get(&self, id: CallId) -> Result<Option<Logged>, Error> {
        self.db.with(|db| {
            let found = db
                .query_row(&format!("SELECT {COLUMNS} FROM calls WHERE id = ?1"), params![id.0], read_row)
                .optional()?;
            Ok(found.flatten())
        })
    }

    /// Forgets the given calls, in one transaction, so a selection goes all at once or not at all.
    pub fn remove(&self, ids: &[CallId]) -> Result<(), Error> {
        self.db.with(|db| {
            let transaction = db.unchecked_transaction()?;
            for id in ids {
                transaction.execute("DELETE FROM calls WHERE id = ?1", params![id.0])?;
            }
            transaction.commit()?;
            Ok(())
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
        CallRecord {
            peer: SecretKey::generate().public(),
            incoming,
            outcome,
            mode: Mode::Video,
            at,
            duration: None,
            video_from: None,
            traffic: None,
            quality: None,
        }
    }

    /// The caller giving up is a hang-up on our side of the wire, and it is still a missed call.
    #[test]
    fn a_hang_up_reads_by_direction_and_answer() {
        assert_eq!(Outcome::of(&EndReason::RemoteHangup, true, false), Outcome::Missed);
        assert_eq!(Outcome::of(&EndReason::LocalHangup, false, false), Outcome::Cancelled);
        assert_eq!(Outcome::of(&EndReason::RemoteHangup, true, true), Outcome::Answered);
        assert_eq!(Outcome::of(&EndReason::Declined, true, false), Outcome::Declined);
        assert_eq!(Outcome::of(&EndReason::Failed("connection lost".into()), false, true), Outcome::Answered);
        assert_eq!(Outcome::of(&EndReason::ConnectionLost, false, true), Outcome::Lost);
        assert!(Outcome::Lost.answered());
    }

    #[test]
    fn a_voice_call_that_became_video_survives_the_round_trip() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = CallLog::open(Db::open(dir.path())?)?;
        let mut call = record(false, Outcome::Lost, SystemTime::now());
        call.mode = Mode::Voice;
        call.video_from = Some(Duration::from_secs(192));
        log.record(&call)?;
        let back = &log.recent(1)?[0].call;
        assert_eq!((back.mode, back.video_from, back.outcome), (Mode::Voice, call.video_from, Outcome::Lost));
        Ok(())
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
        assert_eq!(recent[0].call.at, new);
        assert!(!recent[0].call.incoming);
        Ok(())
    }

    #[test]
    fn traffic_survives_the_round_trip() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = CallLog::open(Db::open(dir.path())?)?;
        let mut call = record(true, Outcome::Answered, SystemTime::now());
        call.traffic = Some(Traffic { sent: 12_345_678, received: 9_876 });
        let mut quality = Quality::default();
        quality.fps_in.add(24.0);
        call.quality = Some(quality);
        log.record(&call)?;
        let logged = log.recent(1)?;
        assert_eq!(logged[0].call.traffic, call.traffic);
        assert_eq!(logged[0].call.quality, call.quality);
        let one = log.get(logged[0].id)?.map(|found| found.call.traffic);
        assert_eq!(one, Some(call.traffic));
        Ok(())
    }

    /// A log from before traffic was recorded gains the columns on open and keeps its rows.
    #[test]
    fn an_older_log_is_brought_up_to_date() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = Db::open(dir.path())?;
        db.with(|db| {
            db.execute_batch(
                "CREATE TABLE calls (id INTEGER PRIMARY KEY AUTOINCREMENT, peer TEXT NOT NULL,
                     incoming INTEGER NOT NULL, outcome TEXT NOT NULL, at INTEGER NOT NULL, seconds INTEGER);",
            )?;
            db.execute(
                "INSERT INTO calls (peer, incoming, outcome, at) VALUES (?1, 1, 'missed', 1)",
                params![SecretKey::generate().public().to_string()],
            )?;
            Ok(())
        })?;
        let log = CallLog::open(db)?;
        let old = log.recent(10)?;
        assert_eq!(old.len(), 1);
        assert_eq!(old[0].call.traffic, None);
        Ok(())
    }

    #[test]
    fn removing_takes_only_the_chosen_calls() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = CallLog::open(Db::open(dir.path())?)?;
        for second in 1..=3 {
            log.record(&record(false, Outcome::Answered, UNIX_EPOCH + Duration::from_secs(second)))?;
        }
        let before = log.recent(10)?;
        log.remove(&[before[0].id, before[2].id])?;
        let after = log.recent(10)?;
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, before[1].id);
        // An id survives the trip through the screen as text.
        assert_eq!(before[1].id.to_string().parse::<CallId>()?, before[1].id);
        Ok(())
    }

    #[test]
    fn a_duration_survives_the_round_trip() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = CallLog::open(Db::open(dir.path())?)?;
        let mut call = record(false, Outcome::Answered, SystemTime::now());
        call.duration = Some(Duration::from_secs(252));
        log.record(&call)?;
        assert_eq!(log.recent(1)?[0].call.duration, Some(Duration::from_secs(252)));
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
