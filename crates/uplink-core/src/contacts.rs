//! Contacts: iroh keys with a local nickname each, in SQLite.
//!
//! A contact is the one thing here a person cannot regenerate — lose the table and every key they
//! ever collected is gone — so it lives in a real database rather than a file rewritten whole on
//! every change. Rows also carry what the peer calls *itself*, which is a claim and never
//! overwrites the nickname, and the hash of a picture stored as a file beside the database.
//!
//! Reads come from an in-memory list because the UI walks it on every repaint; writes go to
//! SQLite first and update the list only once they land.

use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};

use crate::db::Db;
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
    pub fn open(db: Db) -> Result<Self, Error> {
        db.with(|db| {
            db.execute_batch(
                "CREATE TABLE IF NOT EXISTS contacts (
                     id          TEXT PRIMARY KEY NOT NULL,
                     name        TEXT NOT NULL UNIQUE,
                     advertised  TEXT,
                     picture     TEXT,
                     favourite   INTEGER NOT NULL DEFAULT 0,
                     last_called INTEGER
                 )",
            )?;
            Ok(())
        })?;
        let mut contacts = Self { db, list: Vec::new() };
        contacts.reload()?;
        Ok(contacts)
    }

    /// Favourites first, then whoever was called most recently, then by name — which is the order
    /// the list is read in. Public for a copy whose table another writer has changed under it.
    pub fn reload(&mut self) -> Result<(), Error> {
        self.list = self.db.with(|db| {
            let mut statement = db.prepare(
                "SELECT id, name, advertised, picture, favourite, last_called FROM contacts
                 ORDER BY favourite DESC, last_called IS NULL, last_called DESC, name COLLATE NOCASE",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, bool>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            })?;
            let mut list = Vec::new();
            for row in rows {
                let (id, name, advertised, picture, favourite, last_called) = row?;
                let Ok(id) = EndpointId::from_str(&id) else {
                    tracing::warn!(name, "a stored key no longer parses; skipping");
                    continue;
                };
                list.push(Contact {
                    name,
                    id,
                    advertised,
                    picture,
                    favourite,
                    last_called: last_called
                        .and_then(|s| u64::try_from(s).ok())
                        .map(|s| UNIX_EPOCH + std::time::Duration::from_secs(s)),
                });
            }
            Ok(list)
        })?;
        Ok(())
    }

    /// Every write goes through here: one statement, then the cache is rebuilt from the table
    /// rather than patched, so the list and the rows cannot drift.
    fn write(&mut self, sql: &str, values: &[&dyn rusqlite::ToSql]) -> Result<usize, Error> {
        let changed = self.db.with(|db| Ok(db.execute(sql, values)?))?;
        self.reload()?;
        Ok(changed)
    }

    pub fn add(&mut self, name: &str, id: EndpointId) -> Result<(), Error> {
        if let Some(existing) = self.list.iter().find(|c| c.name == name || c.id == id) {
            return Err(Error::DuplicateContact(existing.name.clone()));
        }
        self.write("INSERT INTO contacts (id, name) VALUES (?1, ?2)", params![id.to_string(), name])?;
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<Contact, Error> {
        let contact = self
            .list
            .iter()
            .find(|c| c.name == name)
            .cloned()
            .ok_or_else(|| Error::UnknownContact(name.to_owned()))?;
        self.remove_id(contact.id)
    }

    pub fn remove_id(&mut self, id: EndpointId) -> Result<Contact, Error> {
        let contact = self
            .list
            .iter()
            .find(|c| c.id == id)
            .cloned()
            .ok_or_else(|| Error::UnknownContact(id.fmt_short().to_string()))?;
        self.write("DELETE FROM contacts WHERE id = ?1", params![id.to_string()])?;
        Ok(contact)
    }

    pub fn rename(&mut self, id: EndpointId, name: &str) -> Result<(), Error> {
        if self.list.iter().any(|c| c.name == name && c.id != id) {
            return Err(Error::DuplicateContact(name.to_owned()));
        }
        let changed = self.write("UPDATE contacts SET name = ?2 WHERE id = ?1", params![id.to_string(), name])?;
        if changed == 0 {
            return Err(Error::UnknownContact(id.fmt_short().to_string()));
        }
        Ok(())
    }

    pub fn set_favourite(&mut self, id: EndpointId, favourite: bool) -> Result<(), Error> {
        self.write("UPDATE contacts SET favourite = ?2 WHERE id = ?1", params![id.to_string(), favourite])?;
        Ok(())
    }

    /// Stamps a call, which is what the second line of a contact row shows.
    pub fn called(&mut self, id: EndpointId) -> Result<(), Error> {
        let now = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
            .unwrap_or(i64::MAX);
        self.write("UPDATE contacts SET last_called = ?2 WHERE id = ?1", params![id.to_string(), now])?;
        Ok(())
    }

    /// What a peer calls itself, kept beside the nickname and never replacing it.
    pub fn set_advertised(&mut self, id: EndpointId, name: Option<&str>) -> Result<(), Error> {
        self.write("UPDATE contacts SET advertised = ?2 WHERE id = ?1", params![id.to_string(), name])?;
        Ok(())
    }

    pub fn set_picture(&mut self, id: EndpointId, hash: Option<&str>) -> Result<(), Error> {
        self.write("UPDATE contacts SET picture = ?2 WHERE id = ?1", params![id.to_string(), hash])?;
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
    pub fn count(&self) -> Result<i64, Error> {
        self.db
            .with(|db| Ok(db.query_row("SELECT COUNT(*) FROM contacts", [], |row| row.get(0)).optional()?.unwrap_or(0)))
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

    #[test]
    fn survives_being_reopened() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (noor, ammar) = (key(), key());
        {
            let mut contacts = Contacts::open(Db::open(dir.path())?)?;
            contacts.add("Noor", noor)?;
            contacts.add("Ammar", ammar)?;
        }
        let contacts = Contacts::open(Db::open(dir.path())?)?;
        assert_eq!(contacts.count()?, 2);
        assert_eq!(contacts.name_of(&noor), Some("Noor"));
        Ok(())
    }

    #[test]
    fn a_key_and_a_name_are_each_unique() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut contacts = Contacts::open(Db::open(dir.path())?)?;
        let noor = key();
        contacts.add("Noor", noor)?;
        assert!(contacts.add("Noor", key()).is_err(), "the same name twice");
        assert!(contacts.add("Someone else", noor).is_err(), "the same key twice");
        // A rename onto a name already taken is the same clash by another route.
        contacts.add("Ammar", key())?;
        assert!(contacts.rename(noor, "Ammar").is_err());
        assert_eq!(contacts.count()?, 2);
        Ok(())
    }

    #[test]
    fn resolves_by_nickname_or_by_key() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut contacts = Contacts::open(Db::open(dir.path())?)?;
        let noor = key();
        contacts.add("Noor", noor)?;
        assert_eq!(contacts.resolve("Noor")?, noor);
        assert_eq!(contacts.resolve(&noor.to_string())?, noor);
        // An unsaved key still resolves: the CLI lets you call someone you have not added.
        let stranger = key();
        assert_eq!(contacts.resolve(&stranger.to_string())?, stranger);
        assert!(contacts.resolve("nobody").is_err());
        Ok(())
    }

    #[test]
    fn favourites_come_first_then_the_most_recently_called() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut contacts = Contacts::open(Db::open(dir.path())?)?;
        let (old, recent, star) = (key(), key(), key());
        contacts.add("Old", old)?;
        contacts.add("Recent", recent)?;
        contacts.add("Star", star)?;
        contacts.called(old)?;
        contacts.called(recent)?;
        contacts.set_favourite(star, true)?;

        let order: Vec<&str> = contacts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(order.first(), Some(&"Star"), "a favourite outranks a recent call");
        // Both were called within the same second, so only their being ahead of nobody is certain.
        assert!(contacts.get(&recent).is_some_and(|c| c.last_called.is_some()));
        assert!(contacts.get(&star).is_some_and(|c| c.favourite));
        Ok(())
    }

    #[test]
    fn a_claimed_name_never_replaces_the_nickname() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut contacts = Contacts::open(Db::open(dir.path())?)?;
        let noor = key();
        contacts.add("Noor", noor)?;
        contacts.set_advertised(noor, Some("Someone Else Entirely"))?;
        assert_eq!(contacts.name_of(&noor), Some("Noor"));
        assert_eq!(contacts.get(&noor).and_then(|c| c.advertised.as_deref()), Some("Someone Else Entirely"));
        Ok(())
    }

    #[test]
    fn removing_reports_what_went() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut contacts = Contacts::open(Db::open(dir.path())?)?;
        let noor = key();
        contacts.add("Noor", noor)?;
        assert_eq!(contacts.remove_id(noor)?.name, "Noor");
        assert!(!contacts.contains(&noor));
        assert!(contacts.remove_id(noor).is_err(), "removing twice");
        Ok(())
    }
}
