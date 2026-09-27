//! The one connection to `uplink.db`, shared by contacts, the call log and settings.
//!
//! Three stores used to open the same file three times. WAL tolerates that, but they are one
//! process reading one file, and a single connection means one set of pragmas, one place that
//! knows where the database is, and no chance of two of them disagreeing about it.
//!
//! The database is Turso, a pure-Rust rewrite of SQLite reading and writing the same file format,
//! and it is async all the way down. Its connection refuses an operation that overlaps another
//! exclusive one rather than queueing it, so every unit of work runs under an async lock: a
//! caller awaits its turn instead of failing, and a unit (a write and its trim, a transaction)
//! is never interleaved with another's.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;
use turso::{Builder, Connection, Database};

use crate::Error;

const DATABASE: &str = "uplink.db";
#[cfg(test)]
const IN_MEMORY: &str = ":memory:";

/// A handle to the database. Cloning shares the one connection.
#[derive(Clone)]
pub struct Db {
    connection: Arc<Mutex<Connection>>,
    /// Kept alive for as long as any handle is: the connection belongs to it.
    _database: Arc<Database>,
}

impl Db {
    pub async fn open(dir: &Path) -> Result<Self, Error> {
        tokio::fs::create_dir_all(dir).await?;
        let path = dir.join(DATABASE);
        Self::at(&path.to_string_lossy()).await
    }

    async fn at(path: &str) -> Result<Self, Error> {
        let database = Builder::new_local(path).build().await?;
        let connection = database.connect()?;
        // Foreign keys, because SQLite leaves them off; Turso journals with WAL by default.
        connection.execute("PRAGMA foreign_keys = ON", ()).await?;
        Ok(Self { connection: Arc::new(Mutex::new(connection)), _database: Arc::new(database) })
    }

    /// Runs `work` against the connection, alone: every store's schema and every query goes
    /// through here, so the lock is held across one unit of the database's work and nothing else.
    pub async fn run<T>(&self, work: impl AsyncFnOnce(&Connection) -> Result<T, Error>) -> Result<T, Error> {
        let connection = self.connection.lock().await;
        work(&connection).await
    }

    #[cfg(test)]
    pub async fn memory() -> Result<Self, Error> {
        Self::at(IN_MEMORY).await
    }
}

/// Every row of `sql`, read to the end before the lock is let go.
pub async fn rows(
    connection: &Connection,
    sql: &str,
    params: impl turso::params::IntoParams,
) -> Result<Vec<turso::Row>, Error> {
    let mut rows = connection.query(sql, params).await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        out.push(row);
    }
    Ok(out)
}
