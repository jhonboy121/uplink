//! What the app remembers about how you like it: a key/value table beside contacts and calls.
//!
//! Marker files were the alternative and the reason not to: one zero-byte file per remembered
//! fact, no types, no atomicity, and nothing to list. A row is a row.

use rusqlite::{OptionalExtension, params};

use crate::Error;
use crate::db::Db;

/// Whether the battery explainer has been answered. Shown once: the exemption is not required,
/// and a "no" asked again every launch is nagging. Settings keeps the way back.
pub const BATTERY_OFFERED: &str = "battery-explained";
/// `system`, `light` or `dark` — whatever the UI puts there; this store does not interpret it.
pub const APPEARANCE: &str = "appearance";
/// Whether every screen is mirrored, for Arabic.
pub const LAYOUT_RTL: &str = "layout-rtl";
/// `custom`, or anything else for n0's own relays. See [`crate::relays`].
pub const RELAY_SOURCE: &str = "relay-source";
/// Which of n0's relays are switched off, one host per line.
pub const RELAYS_OFF: &str = "relays-off";
/// The relays to use instead of n0's, one `<url> <name>` per line.
pub const RELAYS_CUSTOM: &str = "relays-custom";

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

    /// A setting holding a list, one entry per line. Blank lines are not entries: they are what
    /// an emptied list leaves behind.
    pub fn lines(&self, key: &str) -> Vec<String> {
        let Some(stored) = self.get(key) else {
            return Vec::new();
        };
        stored.lines().filter(|line| !line.is_empty()).map(str::to_owned).collect()
    }

    pub fn set_lines(&self, key: &str, lines: &[String]) -> Result<(), Error> {
        self.set(key, &lines.join("\n"))
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
        assert!(!settings.flag(LAYOUT_RTL));
        settings.set_flag(LAYOUT_RTL, true)?;
        assert!(settings.flag(LAYOUT_RTL));
        settings.set_flag(LAYOUT_RTL, false)?;
        assert!(!settings.flag(LAYOUT_RTL));
        Ok(())
    }
}
