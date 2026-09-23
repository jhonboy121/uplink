//! The one connection to `uplink.db`, shared by contacts, the call log and settings.
//!
//! Three stores used to open the same file three times. WAL tolerates that, but they are one
//! process reading one file, and a single connection means one set of pragmas, one place that
//! knows where the database is, and no chance of two of them disagreeing about it.
//!
//! The lock is a mutex and could not be an `RwLock`: `rusqlite::Connection` is `Send` but not
//! `Sync`, because every statement needs exclusive use of the handle. Handing out two shared
//! references at once is exactly what SQLite does not allow, so there is no read side to share.

use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::Connection;

use crate::Error;

const DATABASE: &str = "uplink.db";

/// A handle to the database. Cloning shares the one connection.
#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    pub fn open(dir: &Path) -> Result<Self, Error> {
        let db = Connection::open(dir.join(DATABASE))?;
        // WAL so a reader never waits on the writer; foreign keys because SQLite leaves them off.
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self(Arc::new(Mutex::new(db))))
    }

    /// Runs `statements` against the connection. Every store's schema and every query goes
    /// through here, so the lock is never held across anything but SQLite's own work.
    pub fn with<T>(&self, statements: impl FnOnce(&Connection) -> Result<T, Error>) -> Result<T, Error> {
        statements(&self.0.lock())
    }

    #[cfg(test)]
    pub fn memory() -> Result<Self, Error> {
        Ok(Self(Arc::new(Mutex::new(Connection::open_in_memory()?))))
    }
}
