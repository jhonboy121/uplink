//! Contacts: iroh keys with local nicknames, kept in a human-editable TOML file.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{EndpointId, Error};

const CONTACTS_FILE: &str = "contacts.toml";
const TEMP_SUFFIX: &str = "tmp";

#[derive(Clone, Debug)]
pub struct Contact {
    pub name: String,
    pub id: EndpointId,
}

#[derive(Default, Deserialize, Serialize)]
struct Stored {
    #[serde(default, rename = "contact")]
    contacts: Vec<StoredContact>,
}

#[derive(Deserialize, Serialize)]
struct StoredContact {
    name: String,
    id: String,
}

pub struct Contacts {
    path: PathBuf,
    list: Vec<Contact>,
}

impl Contacts {
    pub async fn load(dir: &Path) -> Result<Self, Error> {
        let path = dir.join(CONTACTS_FILE);
        let stored: Stored = match tokio::fs::read_to_string(&path).await {
            Ok(text) => toml::from_str(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Stored::default(),
            Err(e) => return Err(e.into()),
        };
        let list = stored
            .contacts
            .into_iter()
            .map(|c| Ok(Contact { id: EndpointId::from_str(&c.id)?, name: c.name }))
            .collect::<Result<_, Error>>()?;
        Ok(Self { path, list })
    }

    /// Writes atomically (temp file + rename).
    pub async fn save(&self) -> Result<(), Error> {
        let stored = Stored {
            contacts: self.list.iter().map(|c| StoredContact { name: c.name.clone(), id: c.id.to_string() }).collect(),
        };
        let temp = self.path.with_extension(TEMP_SUFFIX);
        tokio::fs::write(&temp, toml::to_string(&stored)?).await?;
        tokio::fs::rename(&temp, &self.path).await?;
        Ok(())
    }

    pub fn add(&mut self, name: &str, id: EndpointId) -> Result<(), Error> {
        if let Some(existing) = self.list.iter().find(|c| c.name == name || c.id == id) {
            return Err(Error::DuplicateContact(existing.name.clone()));
        }
        self.list.push(Contact { name: name.to_owned(), id });
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<Contact, Error> {
        let index = self.list.iter().position(|c| c.name == name).ok_or_else(|| Error::UnknownContact(name.to_owned()))?;
        Ok(self.list.remove(index))
    }

    /// Resolves a nickname, or a key if `query` parses as one.
    pub fn resolve(&self, query: &str) -> Result<EndpointId, Error> {
        match self.list.iter().find(|c| c.name == query) {
            Some(contact) => Ok(contact.id),
            None => EndpointId::from_str(query).map_err(|_| Error::UnknownContact(query.to_owned())),
        }
    }

    pub fn name_of(&self, id: &EndpointId) -> Option<&str> {
        self.list.iter().find(|c| c.id == *id).map(|c| c.name.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = &Contact> {
        self.list.iter()
    }
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;

    fn key() -> EndpointId {
        SecretKey::generate().public()
    }

    async fn empty() -> anyhow::Result<(tempfile::TempDir, Contacts)> {
        let dir = tempfile::tempdir()?;
        let contacts = Contacts::load(dir.path()).await?;
        Ok((dir, contacts))
    }

    #[tokio::test]
    async fn missing_file_loads_empty() -> anyhow::Result<()> {
        let (_dir, contacts) = empty().await?;
        assert_eq!(contacts.iter().count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn resolves_names_and_raw_keys() -> anyhow::Result<()> {
        let (_dir, mut contacts) = empty().await?;
        let (alice, stranger) = (key(), key());
        contacts.add("alice", alice)?;
        assert_eq!(contacts.resolve("alice")?, alice);
        assert_eq!(contacts.resolve(&stranger.to_string())?, stranger);
        assert_eq!(contacts.name_of(&alice), Some("alice"));
        assert_eq!(contacts.name_of(&stranger), None);
        assert!(matches!(contacts.resolve("bob"), Err(Error::UnknownContact(_))));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_duplicate_names_and_keys() -> anyhow::Result<()> {
        let (_dir, mut contacts) = empty().await?;
        let alice = key();
        contacts.add("alice", alice)?;
        assert!(matches!(contacts.add("alice", key()), Err(Error::DuplicateContact(_))));
        assert!(matches!(contacts.add("alias", alice), Err(Error::DuplicateContact(_))));
        Ok(())
    }

    #[tokio::test]
    async fn remove_returns_the_contact() -> anyhow::Result<()> {
        let (_dir, mut contacts) = empty().await?;
        let alice = key();
        contacts.add("alice", alice)?;
        assert_eq!(contacts.remove("alice")?.id, alice);
        assert!(matches!(contacts.remove("alice"), Err(Error::UnknownContact(_))));
        Ok(())
    }

    #[tokio::test]
    async fn save_and_load_round_trip() -> anyhow::Result<()> {
        let (dir, mut contacts) = empty().await?;
        let (alice, bob) = (key(), key());
        contacts.add("alice", alice)?;
        contacts.add("bob", bob)?;
        contacts.save().await?;
        let loaded = Contacts::load(dir.path()).await?;
        let entries: Vec<_> = loaded.iter().map(|c| (c.name.as_str(), c.id)).collect();
        assert_eq!(entries, [("alice", alice), ("bob", bob)]);
        Ok(())
    }

    #[tokio::test]
    async fn invalid_key_in_file_is_an_error() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        tokio::fs::write(dir.path().join(CONTACTS_FILE), "[[contact]]\nname = \"x\"\nid = \"not-a-key\"\n").await?;
        assert!(matches!(Contacts::load(dir.path()).await, Err(Error::KeyParse(_))));
        Ok(())
    }
}
