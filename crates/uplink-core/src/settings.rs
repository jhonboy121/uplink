//! What the app remembers about how you like it: a key/value table beside contacts and calls.
//!
//! Marker files were the alternative and the reason not to: one zero-byte file per remembered
//! fact, no types, no atomicity, and nothing to list. A row is a row.

use rusqlite::{OptionalExtension, params};

use crate::Error;
use crate::db::Db;

/// Whether the permission explainer has been shown. A one-time grant lapses when the process
/// dies, and reading the same screen every launch is a wall between the user and the prompt.
pub const GATE_EXPLAINED: &str = "gate-explained";
/// `system`, `light` or `dark` — whatever the UI puts there; this store does not interpret it.
pub const APPEARANCE: &str = "appearance";
/// Whether every screen is mirrored, for Arabic.
pub const LAYOUT_RTL: &str = "layout-rtl";

const TRUE: &str = "1";

/// Cloning shares the store, because the callbacks that write a setting are scattered across the
/// UI and none of them owns it.
#[derive(Clone)]
pub struct Settings {
    db: Db,
}

impl Settings {
    pub fn open(db: Db) -> Result<Self, Error> {
        db.with(|db| {
            db.execute_batch(
                "CREATE TABLE IF NOT EXISTS settings (
                     key   TEXT PRIMARY KEY NOT NULL,
                     value TEXT NOT NULL
                 )",
            )?;
            Ok(())
        })?;
        Ok(Self { db })
    }

    /// The stored value, or `None` if it has never been set. An unreadable row reads as unset:
    /// a setting is a preference, and failing to start over one would be absurd.
    pub fn get(&self, key: &str) -> Option<String> {
        let read = self.db.with(|db| {
            Ok(db
                .query_one("SELECT value FROM settings WHERE key = ?1", params![key], |row| row.get(0))
                .optional()?)
        });
        read.unwrap_or_else(|e| {
            tracing::warn!(key, "reading a setting: {e}");
            None
        })
    }

    pub fn set(&self, key: &str, value: &str) -> Result<(), Error> {
        self.db.with(|db| {
            db.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
            Ok(())
        })
    }

    /// A setting that is only ever on or off. Absent is off.
    pub fn flag(&self, key: &str) -> bool {
        self.get(key).as_deref() == Some(TRUE)
    }

    pub fn set_flag(&self, key: &str, on: bool) -> Result<(), Error> {
        self.set(key, if on { TRUE } else { "0" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Result<Settings, Error> {
        Settings::open(Db::memory()?)
    }

    #[test]
    fn a_setting_survives_being_written_twice() -> Result<(), Error> {
        let settings = settings()?;
        assert_eq!(settings.get(APPEARANCE), None);
        settings.set(APPEARANCE, "dark")?;
        settings.set(APPEARANCE, "light")?;
        assert_eq!(settings.get(APPEARANCE).as_deref(), Some("light"));
        Ok(())
    }

    /// Absent reads as off, which is what every flag here means before it is ever set.
    #[test]
    fn a_flag_is_off_until_it_is_set() -> Result<(), Error> {
        let settings = settings()?;
        assert!(!settings.flag(GATE_EXPLAINED));
        settings.set_flag(GATE_EXPLAINED, true)?;
        assert!(settings.flag(GATE_EXPLAINED));
        settings.set_flag(GATE_EXPLAINED, false)?;
        assert!(!settings.flag(GATE_EXPLAINED));
        Ok(())
    }
}
