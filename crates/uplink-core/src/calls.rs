//! The call log: what was attempted, which way it went, and how it ended.
//!
//! It shares the one connection with contacts and settings. A row is written when a call *ends*,
//! because until then its outcome is unknown.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use turso::{Connection, Row, Value};

use crate::db::{Db, rows};
use crate::node::{EndReason, Mode};
use crate::quality::Quality;
use crate::{EndpointId, Error};
/// Enough to look back over, and small enough that the screen never pages.
pub const KEEP: i64 = 500;

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
    /// Theirs, turned away without ringing: not in contacts, with reject unknown callers on.
    Screened,
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
            EndReason::Screened { .. } => Self::Screened,
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
            Self::Screened => "screened",
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
            "screened" => Self::Screened,
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

/// Cloning shares the connection, as every handle to the database does.
#[derive(Clone)]
pub struct CallLog {
    db: Db,
}

/// What every read selects, in the order [`read_row`] takes it.
const COLUMNS: &str = "id, peer, incoming, outcome, at, seconds, sent, received, quality, voice, video_at";

/// Columns added after the table first shipped, with their types. `CREATE TABLE IF NOT EXISTS`
/// leaves an existing table as it was, so these are added to it on open when missing.
const ADDED: [(&str, &str); 5] =
    [("sent", "INTEGER"), ("received", "INTEGER"), ("quality", "BLOB"), ("voice", "INTEGER"), ("video_at", "INTEGER")];

/// The table as it is created now: `id` is the rowid, which a new row takes as one past the
/// highest. Tables made before had `AUTOINCREMENT`, which Turso keeps a counter for of its own
/// beside SQLite's, so a row written by plain SQLite (the dev tools) could have its id handed
/// out again; nothing here needs ids never to be reused, so the table is rebuilt without it.
const SCHEMA: &str = "(
     id       INTEGER PRIMARY KEY,
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
 )";
const INDEX: &str = "CREATE INDEX IF NOT EXISTS calls_at ON calls (at DESC)";
const OLD_KEY: &str = "AUTOINCREMENT";

/// A row as [`COLUMNS`] reads it, or `None` for one whose key no longer parses.
fn read_row(row: &Row) -> Result<Option<Logged>, Error> {
    let peer = row.get::<String>(1)?;
    let Ok(peer_id) = peer.parse::<EndpointId>() else {
        tracing::warn!(peer, "a logged key no longer parses; skipping");
        return Ok(None);
    };
    let unsigned = |value: Option<i64>| value.and_then(|v| u64::try_from(v).ok());
    let (sent, received) = (unsigned(row.get(6)?), unsigned(row.get(7)?));
    let quality = match row.get_value(8)? {
        Value::Blob(bytes) => Quality::from_bytes(&bytes)
            .inspect_err(|e| tracing::debug!("a call's quality summary no longer reads: {e}"))
            .ok(),
        _ => None,
    };
    let call = CallRecord {
        peer: peer_id,
        incoming: row.get(2)?,
        outcome: Outcome::parse(&row.get::<String>(3)?),
        mode: if row.get::<Option<bool>>(9)?.unwrap_or_default() { Mode::Voice } else { Mode::Video },
        at: UNIX_EPOCH + Duration::from_secs(unsigned(Some(row.get(4)?)).unwrap_or_default()),
        duration: unsigned(row.get(5)?).map(Duration::from_secs),
        video_from: unsigned(row.get(10)?).map(Duration::from_secs),
        traffic: sent.zip(received).map(|(sent, received)| Traffic { sent, received }),
        quality,
    };
    Ok(Some(Logged { id: CallId(row.get(0)?), call }))
}

/// Brings a table from any earlier build up to [`SCHEMA`]: the columns added since, then the
/// rebuild without `AUTOINCREMENT`, in one transaction so a failure leaves the old table whole.
async fn upgrade(db: &Connection) -> Result<(), Error> {
    let existing: Vec<String> = rows(db, "SELECT name FROM pragma_table_info('calls')", ())
        .await?
        .iter()
        .map(|row| row.get::<String>(0))
        .collect::<Result<_, _>>()?;
    for (column, kind) in ADDED {
        if !existing.iter().any(|name| name == column) {
            db.execute_batch(&format!("ALTER TABLE calls ADD COLUMN {column} {kind}")).await?;
        }
    }
    let table = rows(db, "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'calls'", ()).await?;
    let old = table.first().map(|row| row.get::<String>(0)).transpose()?.is_some_and(|sql| sql.contains(OLD_KEY));
    if old {
        tracing::info!("rebuilding the call log without AUTOINCREMENT");
        db.execute_batch(&format!(
            "BEGIN;
             CREATE TABLE calls_rebuilt {SCHEMA};
             INSERT INTO calls_rebuilt ({COLUMNS}) SELECT {COLUMNS} FROM calls;
             DROP TABLE calls;
             ALTER TABLE calls_rebuilt RENAME TO calls;
             {INDEX};
             COMMIT"
        ))
        .await?;
    }
    Ok(())
}

impl CallLog {
    pub async fn open(db: Db) -> Result<Self, Error> {
        db.run(async |db| {
            db.execute_batch(&format!("CREATE TABLE IF NOT EXISTS calls {SCHEMA}; {INDEX}")).await?;
            upgrade(db).await
        })
        .await?;
        Ok(Self { db })
    }

    pub async fn record(&self, record: &CallRecord) -> Result<(), Error> {
        let at = i64::try_from(record.at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()).unwrap_or(i64::MAX);
        let seconds = record.duration.map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        let signed = |bytes: u64| i64::try_from(bytes).unwrap_or(i64::MAX);
        let (sent, received) = record.traffic.map(|t| (signed(t.sent), signed(t.received))).unzip();
        let quality = record.quality.as_ref().map(Quality::to_bytes);
        let video_at = record.video_from.map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        let values: [Value; 10] = [
            record.peer.to_string().into(),
            record.incoming.into(),
            record.outcome.as_str().into(),
            at.into(),
            seconds.into(),
            sent.into(),
            received.into(),
            quality.into(),
            (record.mode == Mode::Voice).into(),
            video_at.into(),
        ];
        self.db
            .run(async |db| {
                db.execute(
                    "INSERT INTO calls (peer, incoming, outcome, at, seconds, sent, received, quality, voice, video_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    values,
                )
                .await?;
                // Trimmed here rather than on a timer: the log only grows when a call ends. Only
                // what lies past the newest `KEEP` is looked at, through the index on `at`:
                // `NOT IN` the newest sorted the whole table on every call, which Turso does slowly.
                db.execute(
                    "DELETE FROM calls WHERE id IN
                         (SELECT id FROM calls ORDER BY at DESC, id DESC LIMIT -1 OFFSET ?1)",
                    (KEEP,),
                )
                .await?;
                Ok(())
            })
            .await
    }

    /// Most recent first.
    pub async fn recent(&self, limit: i64) -> Result<Vec<Logged>, Error> {
        let found = self
            .db
            .run(async |db| {
                rows(db, &format!("SELECT {COLUMNS} FROM calls ORDER BY at DESC, id DESC LIMIT ?1"), (limit,)).await
            })
            .await?;
        let mut out = Vec::with_capacity(found.len());
        for row in &found {
            out.extend(read_row(row)?);
        }
        Ok(out)
    }

    /// One call, for its details page; `None` once it has been removed or trimmed.
    pub async fn get(&self, id: CallId) -> Result<Option<Logged>, Error> {
        let found = self
            .db
            .run(async |db| rows(db, &format!("SELECT {COLUMNS} FROM calls WHERE id = ?1"), (id.0,)).await)
            .await?;
        Ok(found.first().map(read_row).transpose()?.flatten())
    }

    /// Forgets the given calls, in one transaction, so a selection goes all at once or not at all.
    pub async fn remove(&self, ids: &[CallId]) -> Result<(), Error> {
        self.db
            .run(async |db| {
                db.execute_batch("BEGIN").await?;
                for id in ids {
                    if let Err(e) = db.execute("DELETE FROM calls WHERE id = ?1", (id.0,)).await {
                        if let Err(rollback) = db.execute_batch("ROLLBACK").await {
                            tracing::warn!("rolling back a removal: {rollback}");
                        }
                        return Err(e.into());
                    }
                }
                db.execute_batch("COMMIT").await?;
                Ok(())
            })
            .await
    }

    /// How many calls came in and were never answered, which is what a tab badge would show.
    pub async fn missed(&self) -> Result<i64, Error> {
        let counted = self
            .db
            .run(async |db| rows(db, "SELECT COUNT(*) FROM calls WHERE outcome = 'missed'", ()).await)
            .await?;
        Ok(counted.first().map(|row| row.get::<i64>(0)).transpose()?.unwrap_or_default())
    }

    pub async fn clear(&self) -> Result<(), Error> {
        self.db
            .run(async |db| {
                db.execute("DELETE FROM calls", ()).await?;
                Ok(())
            })
            .await
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

    #[tokio::test]
    async fn a_voice_call_that_became_video_survives_the_round_trip() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = CallLog::open(Db::open(dir.path()).await?).await?;
        let mut call = record(false, Outcome::Lost, SystemTime::now());
        call.mode = Mode::Voice;
        call.video_from = Some(Duration::from_secs(192));
        log.record(&call).await?;
        let back = &log.recent(1).await?[0].call;
        assert_eq!((back.mode, back.video_from, back.outcome), (Mode::Voice, call.video_from, Outcome::Lost));
        Ok(())
    }

    #[tokio::test]
    async fn the_newest_call_is_first() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = CallLog::open(Db::open(dir.path()).await?).await?;
        let old = UNIX_EPOCH + Duration::from_secs(1_000);
        let new = UNIX_EPOCH + Duration::from_secs(2_000);
        log.record(&record(true, Outcome::Missed, old)).await?;
        log.record(&record(false, Outcome::Answered, new)).await?;
        let recent = log.recent(10).await?;
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].call.at, new);
        assert!(!recent[0].call.incoming);
        Ok(())
    }

    #[tokio::test]
    async fn traffic_survives_the_round_trip() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = CallLog::open(Db::open(dir.path()).await?).await?;
        let mut call = record(true, Outcome::Answered, SystemTime::now());
        call.traffic = Some(Traffic { sent: 12_345_678, received: 9_876 });
        let mut quality = Quality::default();
        quality.fps_in.add(24.0);
        call.quality = Some(quality);
        log.record(&call).await?;
        let logged = log.recent(1).await?;
        assert_eq!(logged[0].call.traffic, call.traffic);
        assert_eq!(logged[0].call.quality, call.quality);
        let one = log.get(logged[0].id).await?.map(|found| found.call.traffic);
        assert_eq!(one, Some(call.traffic));
        Ok(())
    }

    /// A log from an earlier build gains the columns added since, keeps its rows, and loses
    /// `AUTOINCREMENT` (see [`SCHEMA`]).
    #[tokio::test]
    async fn an_older_log_is_brought_up_to_date() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = Db::open(dir.path()).await?;
        let peer = SecretKey::generate().public().to_string();
        db.run(async |db| {
            db.execute_batch(
                "CREATE TABLE calls (id INTEGER PRIMARY KEY AUTOINCREMENT, peer TEXT NOT NULL,
                     incoming INTEGER NOT NULL, outcome TEXT NOT NULL, at INTEGER NOT NULL, seconds INTEGER);",
            )
            .await?;
            db.execute("INSERT INTO calls (peer, incoming, outcome, at) VALUES (?1, 1, 'missed', 1)", (peer,))
                .await?;
            Ok(())
        })
        .await?;
        let log = CallLog::open(db.clone()).await?;
        let old = log.recent(10).await?;
        assert_eq!(old.len(), 1);
        assert_eq!(old[0].call.traffic, None);
        let sql = db
            .run(async |db| rows(db, "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'calls'", ()).await)
            .await?;
        assert!(sql.first().map(|row| row.get::<String>(0)).transpose()?.is_some_and(|sql| !sql.contains(OLD_KEY)));
        log.record(&record(false, Outcome::Answered, SystemTime::now())).await?;
        assert_eq!(log.recent(10).await?.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn removing_takes_only_the_chosen_calls() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = CallLog::open(Db::open(dir.path()).await?).await?;
        for second in 1..=3 {
            log.record(&record(false, Outcome::Answered, UNIX_EPOCH + Duration::from_secs(second))).await?;
        }
        let before = log.recent(10).await?;
        log.remove(&[before[0].id, before[2].id]).await?;
        let after = log.recent(10).await?;
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, before[1].id);
        // An id survives the trip through the screen as text.
        assert_eq!(before[1].id.to_string().parse::<CallId>()?, before[1].id);
        Ok(())
    }

    #[tokio::test]
    async fn a_duration_survives_the_round_trip() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = CallLog::open(Db::open(dir.path()).await?).await?;
        let mut call = record(false, Outcome::Answered, SystemTime::now());
        call.duration = Some(Duration::from_secs(252));
        log.record(&call).await?;
        assert_eq!(log.recent(1).await?[0].call.duration, Some(Duration::from_secs(252)));
        Ok(())
    }

    #[tokio::test]
    async fn missed_calls_are_counted() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = CallLog::open(Db::open(dir.path()).await?).await?;
        let now = SystemTime::now();
        log.record(&record(true, Outcome::Missed, now)).await?;
        log.record(&record(true, Outcome::Missed, now)).await?;
        log.record(&record(true, Outcome::Answered, now)).await?;
        assert_eq!(log.missed().await?, 2);
        log.clear().await?;
        assert_eq!(log.missed().await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn the_log_stops_growing() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = CallLog::open(Db::open(dir.path()).await?).await?;
        for second in 0..KEEP + 20 {
            let at = UNIX_EPOCH + Duration::from_secs(u64::try_from(second).unwrap_or_default());
            log.record(&record(false, Outcome::Answered, at)).await?;
        }
        assert_eq!(i64::try_from(log.recent(KEEP * 2).await?.len()).unwrap_or_default(), KEEP);
        Ok(())
    }

    #[test]
    fn an_unknown_outcome_reads_back_as_failed() {
        assert_eq!(Outcome::parse("something from a newer build"), Outcome::Failed);
        assert_eq!(Outcome::parse(Outcome::Declined.as_str()), Outcome::Declined);
    }
}
