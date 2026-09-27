//! What the app remembers about how you like it: a key/value table beside contacts and calls.
//!
//! Marker files were the alternative and the reason not to: one zero-byte file per remembered
//! fact, no types, no atomicity, and nothing to list. A row is a row.

use std::sync::Arc;

use parking_lot::RwLock;
use rustc_hash::FxHashMap;

use crate::Error;
use crate::db::{Db, rows};

/// Whether the battery explainer has been answered. Shown once: the exemption is not required,
/// and a "no" asked again every launch is nagging. Settings keeps the way back.
pub const BATTERY_OFFERED: &str = "battery-explained";
/// `system`, `light` or `dark` — whatever the UI puts there; this store does not interpret it.
pub const APPEARANCE: &str = "appearance";
/// `system`, or the language the UI was set to (`en`, `ar`). The layout's direction follows it.
pub const LANGUAGE: &str = "language";
/// Set when the relays are chosen by hand; absent is automatic. See [`crate::relays`].
pub const RELAYS_MANUAL: &str = "relays-manual";
/// The relays that may be used, one URL per line. Absent is all of them.
pub const RELAYS_TICKED: &str = "relays-ticked";
/// The relays you added, one `<url> <name>` per line.
pub const RELAYS_CUSTOM: &str = "relays-custom";
/// The last survey: its unix time, then `<url> <rtt ms>` best first.
pub const RELAYS_RANKING: &str = "relays-ranking";
/// Set when this phone's screen is kept from screenshots and recordings, always.
pub const BLOCK_CAPTURE: &str = "block-capture";
/// Set when every call asks the other phone to keep its screen from them, for that call.
pub const ASK_BLOCK_CAPTURE: &str = "ask-block-capture";
/// Set when a key not in contacts never rings: turned away at signalling and logged.
pub const REJECT_UNKNOWN: &str = "reject-unknown";

const TRUE: &str = "1";
const FALSE: &str = "0";

/// Cloning shares the store and its copy, because the callbacks that write a setting are
/// scattered across the UI and none of them owns it.
///
/// Every setting is read into memory when the store opens: they are a few dozen short strings,
/// read far more often than written, and often from code that cannot wait (a flag checked as a
/// call comes in, the network's quality step). Writes go to the database first and change the
/// copy only once they land, so the two never disagree about what was saved.
#[derive(Clone)]
pub struct Settings {
    db: Db,
    values: Arc<RwLock<FxHashMap<String, String>>>,
}

impl Settings {
    /// The store these settings are in, for what reads another table next to a setting.
    pub(crate) const fn db(&self) -> &Db {
        &self.db
    }

    pub async fn open(db: Db) -> Result<Self, Error> {
        let values = db
            .run(async |db| {
                db.execute_batch(
                    "CREATE TABLE IF NOT EXISTS settings (
                         key   TEXT PRIMARY KEY NOT NULL,
                         value TEXT NOT NULL
                     )",
                )
                .await?;
                let mut values = FxHashMap::default();
                for row in rows(db, "SELECT key, value FROM settings", ()).await? {
                    values.insert(row.get::<String>(0)?, row.get::<String>(1)?);
                }
                Ok(values)
            })
            .await?;
        Ok(Self { db, values: Arc::new(RwLock::new(values)) })
    }

    /// The saved value, or `None` if it has never been set.
    pub fn get(&self, key: &str) -> Option<String> {
        self.values.read().get(key).cloned()
    }

    pub async fn set(&self, key: &str, value: &str) -> Result<(), Error> {
        self.db
            .run(async |db| {
                db.execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    (key, value),
                )
                .await?;
                Ok(())
            })
            .await?;
        self.values.write().insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    /// A setting that is only ever on or off. Absent is off.
    pub fn flag(&self, key: &str) -> bool {
        self.get(key).as_deref() == Some(TRUE)
    }

    pub async fn set_flag(&self, key: &str, on: bool) -> Result<(), Error> {
        self.set(key, if on { TRUE } else { FALSE }).await
    }

    /// A setting holding a list, one entry per line. Blank lines are not entries: they are what
    /// an emptied list leaves behind.
    pub fn lines(&self, key: &str) -> Vec<String> {
        let Some(stored) = self.get(key) else {
            return Vec::new();
        };
        stored.lines().filter(|line| !line.is_empty()).map(str::to_owned).collect()
    }

    pub async fn set_lines(&self, key: &str, lines: &[String]) -> Result<(), Error> {
        self.set(key, &lines.join("\n")).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn settings() -> Result<Settings, Error> {
        Settings::open(Db::memory().await?).await
    }

    #[tokio::test]
    async fn a_setting_survives_being_written_twice() -> Result<(), Error> {
        let settings = settings().await?;
        assert_eq!(settings.get(APPEARANCE), None);
        settings.set(APPEARANCE, "dark").await?;
        settings.set(APPEARANCE, "light").await?;
        assert_eq!(settings.get(APPEARANCE).as_deref(), Some("light"));
        Ok(())
    }

    /// Absent reads as off, which is what every flag here means before it is ever set.
    #[tokio::test]
    async fn a_flag_is_off_until_it_is_set() -> Result<(), Error> {
        let settings = settings().await?;
        assert!(!settings.flag(BATTERY_OFFERED));
        settings.set_flag(BATTERY_OFFERED, true).await?;
        assert!(settings.flag(BATTERY_OFFERED));
        settings.set_flag(BATTERY_OFFERED, false).await?;
        assert!(!settings.flag(BATTERY_OFFERED));
        Ok(())
    }

    /// Reopened, the store reads back what was written, and a clone sees a write made through
    /// another: the copy is shared, not per handle.
    #[tokio::test]
    async fn the_copy_is_shared_and_the_store_is_the_truth() -> Result<(), Error> {
        let dir = tempfile::tempdir()?;
        let settings = Settings::open(Db::open(dir.path()).await?).await?;
        let other = settings.clone();
        settings.set(LANGUAGE, "ar").await?;
        assert_eq!(other.get(LANGUAGE).as_deref(), Some("ar"));
        drop((settings, other));
        let reopened = Settings::open(Db::open(dir.path()).await?).await?;
        assert_eq!(reopened.get(LANGUAGE).as_deref(), Some("ar"));
        Ok(())
    }
}
