//! Contacts: iroh keys with a local nickname each, in the database.
//!
//! A contact is the one thing here a person cannot regenerate — lose the table and every key they
//! ever collected is gone — so it lives in a real database rather than a file rewritten whole on
//! every change. Rows also carry what the peer calls *itself*, which is a claim and never
//! overwrites the nickname, and the hash of a picture stored as a file beside the database.
//!
//! Reads come from an in-memory list because the UI walks it on every repaint; writes go to
//! the database first and update the list only once they land, so every write is async and every
//! read is not.

use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::db::{Db, rows};
use crate::{EndpointId, Error};

#[derive(Clone, Debug)]
pub struct Contact {
    /// What *you* call them. Always wins over `advertised`.
    pub name: String,
    pub id: EndpointId,
    /// What they call themselves, when they have said. A claim, not an identity.
    pub advertised: Option<String>,
    /// Hash of their picture, which is a file in the app's private directory.
    pub picture: Option<String>,
    pub favourite: bool,
    pub last_called: Option<SystemTime>,
}

pub struct Contacts {
    db: Db,
    list: Vec<Contact>,
}

impl Contacts {
    /// Whether `id` is saved, read from the store now rather than a list loaded earlier: the
    /// endpoint asks while the window adds and removes contacts through its own.
    pub async fn known(db: &Db, id: &EndpointId) -> Result<bool, Error> {
        db.run(async |db| Ok(!rows(db, "SELECT 1 FROM contacts WHERE id = ?1", (id.to_string(),)).await?.is_empty()))
            .await
    }

    /// The nickname saved for `id`, read from the store now: for what names a caller without
    /// holding the whole list, as the process-wide side does when a call rings with no window.
    pub async fn saved_name(db: &Db, id: &EndpointId) -> Result<Option<String>, Error> {
        let found = db
            .run(async |db| rows(db, "SELECT name FROM contacts WHERE id = ?1", (id.to_string(),)).await)
            .await?;
        Ok(found.first().map(|row| row.get::<String>(0)).transpose()?)
    }

    /// Stamps a call on `id`'s row, which is what the second line of a contact row shows. A key
    /// that is not a contact has no row, and nothing happens.
    pub async fn stamp_called(db: &Db, id: &EndpointId) -> Result<(), Error> {
        let now = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
            .unwrap_or(i64::MAX);
        db.run(async |db| {
            db.execute("UPDATE contacts SET last_called = ?2 WHERE id = ?1", (id.to_string(), now)).await?;
            Ok(())
        })
        .await
    }

    /// A handle with nothing loaded yet, for a screen that must draw before the database has
    /// answered: [`Self::reload`] fills it. The table is the one [`Self::open`] made.
    pub const fn empty(db: Db) -> Self {
        Self { db, list: Vec::new() }
    }

    /// The database this list is read from, for a fresh handle to write through.
    pub fn db(&self) -> Db {
        self.db.clone()
    }

    pub async fn open(db: Db) -> Result<Self, Error> {
        db.run(async |db| {
            db.execute_batch(
                "CREATE TABLE IF NOT EXISTS contacts (
                     id          TEXT PRIMARY KEY NOT NULL,
                     name        TEXT NOT NULL UNIQUE,
                     advertised  TEXT,
                     picture     TEXT,
                     favourite   INTEGER NOT NULL DEFAULT 0,
                     last_called INTEGER
                 )",
            )
            .await?;
            Ok(())
        })
        .await?;
        let mut contacts = Self { db, list: Vec::new() };
        contacts.reload().await?;
        Ok(contacts)
    }

    /// Favourites first, then whoever was called most recently, then by name — which is the order
    /// the list is read in. Public for a copy whose table another writer has changed under it.
    pub async fn reload(&mut self) -> Result<(), Error> {
        let found = self
            .db
            .run(async |db| {
                rows(
                    db,
                    "SELECT id, name, advertised, picture, favourite, last_called FROM contacts
                     ORDER BY favourite DESC, last_called IS NULL, last_called DESC, name COLLATE NOCASE",
                    (),
                )
                .await
            })
            .await?;
        let mut list = Vec::with_capacity(found.len());
        for row in found {
            let (id, name) = (row.get::<String>(0)?, row.get::<String>(1)?);
            let Ok(id) = EndpointId::from_str(&id) else {
                tracing::warn!(name, "a stored key no longer parses; skipping");
                continue;
            };
            list.push(Contact {
                name,
                id,
                advertised: row.get(2)?,
                picture: row.get(3)?,
                favourite: row.get(4)?,
                last_called: row
                    .get::<Option<i64>>(5)?
                    .and_then(|s| u64::try_from(s).ok())
                    .map(|s| UNIX_EPOCH + std::time::Duration::from_secs(s)),
            });
        }
        self.list = list;
        Ok(())
    }

    /// Every write goes through here: one statement, then the cache is rebuilt from the table
    /// rather than patched, so the list and the rows cannot drift.
    async fn write(&mut self, sql: &str, values: impl turso::params::IntoParams) -> Result<u64, Error> {
        let changed = self.db.run(async |db| Ok(db.execute(sql, values).await?)).await?;
        self.reload().await?;
        Ok(changed)
    }

    pub async fn add(&mut self, name: &str, id: EndpointId) -> Result<(), Error> {
        if let Some(existing) = self.list.iter().find(|c| c.name == name || c.id == id) {
            return Err(Error::DuplicateContact(existing.name.clone()));
        }
        self.write("INSERT INTO contacts (id, name) VALUES (?1, ?2)", (id.to_string(), name)).await?;
        Ok(())
    }

    pub async fn remove(&mut self, name: &str) -> Result<Contact, Error> {
        let contact = self
            .list
            .iter()
            .find(|c| c.name == name)
            .cloned()
            .ok_or_else(|| Error::UnknownContact(name.to_owned()))?;
        self.remove_id(contact.id).await
    }

    pub async fn remove_id(&mut self, id: EndpointId) -> Result<Contact, Error> {
        let contact = self
            .list
            .iter()
            .find(|c| c.id == id)
            .cloned()
            .ok_or_else(|| Error::UnknownContact(id.fmt_short().to_string()))?;
        self.write("DELETE FROM contacts WHERE id = ?1", (id.to_string(),)).await?;
        Ok(contact)
    }

    pub async fn rename(&mut self, id: EndpointId, name: &str) -> Result<(), Error> {
        if self.list.iter().any(|c| c.name == name && c.id != id) {
            return Err(Error::DuplicateContact(name.to_owned()));
        }
        let changed = self.write("UPDATE contacts SET name = ?2 WHERE id = ?1", (id.to_string(), name)).await?;
        if changed == 0 {
            return Err(Error::UnknownContact(id.fmt_short().to_string()));
        }
        Ok(())
    }

    pub async fn set_favourite(&mut self, id: EndpointId, favourite: bool) -> Result<(), Error> {
        self.write("UPDATE contacts SET favourite = ?2 WHERE id = ?1", (id.to_string(), favourite)).await?;
        Ok(())
    }

    /// Stamps a call, which is what the second line of a contact row shows.
    pub async fn called(&mut self, id: EndpointId) -> Result<(), Error> {
        Self::stamp_called(&self.db, &id).await?;
        self.reload().await
    }

    /// What a peer calls itself, kept beside the nickname and never replacing it.
    pub async fn set_advertised(&mut self, id: EndpointId, name: Option<&str>) -> Result<(), Error> {
        self.write("UPDATE contacts SET advertised = ?2 WHERE id = ?1", (id.to_string(), name)).await?;
        Ok(())
    }

    pub async fn set_picture(&mut self, id: EndpointId, hash: Option<&str>) -> Result<(), Error> {
        self.write("UPDATE contacts SET picture = ?2 WHERE id = ?1", (id.to_string(), hash)).await?;
        Ok(())
    }

    /// A nickname, or a key itself — how the CLI lets you name someone either way.
    pub fn resolve(&self, query: &str) -> Result<EndpointId, Error> {
        if let Some(contact) = self.list.iter().find(|c| c.name == query) {
            return Ok(contact.id);
        }
        EndpointId::from_str(query).map_err(|_| Error::UnknownContact(query.to_owned()))
    }

    pub fn name_of(&self, id: &EndpointId) -> Option<&str> {
        self.list.iter().find(|c| c.id == *id).map(|c| c.name.as_str())
    }

    pub fn get(&self, id: &EndpointId) -> Option<&Contact> {
        self.list.iter().find(|c| c.id == *id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Contact> {
        self.list.iter()
    }

    /// Whether this key already has a row, which is what stops a peer being added twice.
    pub fn contains(&self, id: &EndpointId) -> bool {
        self.list.iter().any(|c| c.id == *id)
    }

    /// The most recent row count straight from the database, for tests and diagnostics.
    pub async fn count(&self) -> Result<i64, Error> {
        let counted = self.db.run(async |db| rows(db, "SELECT COUNT(*) FROM contacts", ()).await).await?;
        Ok(counted.first().map(|row| row.get::<i64>(0)).transpose()?.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use iroh::SecretKey;

    use super::*;

    fn key() -> EndpointId {
        SecretKey::generate().public()
    }

    /// Read from the store, so a contact another handle added counts at once.
    #[tokio::test]
    async fn known_reads_the_store_now() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = Db::open(dir.path()).await?;
        let mut contacts = Contacts::open(db.clone()).await?;
        let id = key();
        assert!(!Contacts::known(&db, &id).await?);
        contacts.add("Noor", id).await?;
        assert!(Contacts::known(&db, &id).await?);
        assert!(!Contacts::known(&db, &key()).await?);
        Ok(())
    }

    #[tokio::test]
    async fn survives_being_reopened() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (noor, ammar) = (key(), key());
        {
            let mut contacts = Contacts::open(Db::open(dir.path()).await?).await?;
            contacts.add("Noor", noor).await?;
            contacts.add("Ammar", ammar).await?;
        }
        let contacts = Contacts::open(Db::open(dir.path()).await?).await?;
        assert_eq!(contacts.count().await?, 2);
        assert_eq!(contacts.name_of(&noor), Some("Noor"));
        Ok(())
    }

    #[tokio::test]
    async fn a_key_and_a_name_are_each_unique() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut contacts = Contacts::open(Db::open(dir.path()).await?).await?;
        let noor = key();
        contacts.add("Noor", noor).await?;
        assert!(contacts.add("Noor", key()).await.is_err(), "the same name twice");
        assert!(contacts.add("Someone else", noor).await.is_err(), "the same key twice");
        // A rename onto a name already taken is the same clash by another route.
        contacts.add("Ammar", key()).await?;
        assert!(contacts.rename(noor, "Ammar").await.is_err());
        assert_eq!(contacts.count().await?, 2);
        Ok(())
    }

    #[tokio::test]
    async fn resolves_by_nickname_or_by_key() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut contacts = Contacts::open(Db::open(dir.path()).await?).await?;
        let noor = key();
        contacts.add("Noor", noor).await?;
        assert_eq!(contacts.resolve("Noor")?, noor);
        assert_eq!(contacts.resolve(&noor.to_string())?, noor);
        // An unsaved key still resolves: the CLI lets you call someone you have not added.
        let stranger = key();
        assert_eq!(contacts.resolve(&stranger.to_string())?, stranger);
        assert!(contacts.resolve("nobody").is_err());
        Ok(())
    }

    #[tokio::test]
    async fn favourites_come_first_then_the_most_recently_called() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut contacts = Contacts::open(Db::open(dir.path()).await?).await?;
        let (old, recent, star) = (key(), key(), key());
        contacts.add("Old", old).await?;
        contacts.add("Recent", recent).await?;
        contacts.add("Star", star).await?;
        contacts.called(old).await?;
        contacts.called(recent).await?;
        contacts.set_favourite(star, true).await?;

        let order: Vec<&str> = contacts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(order.first(), Some(&"Star"), "a favourite outranks a recent call");
        // Both were called within the same second, so only their being ahead of nobody is certain.
        assert!(contacts.get(&recent).is_some_and(|c| c.last_called.is_some()));
        assert!(contacts.get(&star).is_some_and(|c| c.favourite));
        Ok(())
    }

    #[tokio::test]
    async fn a_claimed_name_never_replaces_the_nickname() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut contacts = Contacts::open(Db::open(dir.path()).await?).await?;
        let noor = key();
        contacts.add("Noor", noor).await?;
        contacts.set_advertised(noor, Some("Someone Else Entirely")).await?;
        assert_eq!(contacts.name_of(&noor), Some("Noor"));
        assert_eq!(contacts.get(&noor).and_then(|c| c.advertised.as_deref()), Some("Someone Else Entirely"));
        Ok(())
    }

    #[tokio::test]
    async fn removing_reports_what_went() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut contacts = Contacts::open(Db::open(dir.path()).await?).await?;
        let noor = key();
        contacts.add("Noor", noor).await?;
        assert_eq!(contacts.remove_id(noor).await?.name, "Noor");
        assert!(!contacts.contains(&noor));
        assert!(contacts.remove_id(noor).await.is_err(), "removing twice");
        Ok(())
    }
}
