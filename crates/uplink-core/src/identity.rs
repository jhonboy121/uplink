//! Device identity: an iroh secret key persisted in the data dir, through a [`Vault`].
//!
//! On Android the vault is the Keystore: the file holds the key sealed with a Keystore key that
//! never leaves the phone, so the file alone is not the identity. The host CLI, a test harness,
//! keeps it as raw bytes with owner-only permissions ([`Plain`]).

use std::io::ErrorKind;
use std::path::Path;

use iroh::SecretKey;
use tokio::io::AsyncWriteExt;

use crate::Error;

/// Where [`Plain`] keeps the key, and where every build kept it before the Keystore.
const PLAIN_FILE: &str = "secret.key";
const OWNER_READ_WRITE: u32 = 0o600;
const SECRET_BYTES: usize = 32;

/// How the key is kept at rest.
pub trait Vault {
    /// The file in the data dir that holds it.
    fn file(&self) -> &'static str;
    fn seal(&self, secret: &[u8]) -> Result<Vec<u8>, Error>;
    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, Error>;
}

/// The key as it is: for the host CLI, whose data dir is the machine's own.
pub struct Plain;

impl Vault for Plain {
    fn file(&self) -> &'static str {
        PLAIN_FILE
    }

    fn seal(&self, secret: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(secret.to_vec())
    }

    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(sealed.to_vec())
    }
}

/// Loads the device key, generating and saving one on first use. A file the vault cannot open is
/// an error, never a new identity: that would silently turn this device into someone its
/// contacts do not know.
pub async fn load_or_create(dir: &Path, vault: &impl Vault) -> Result<SecretKey, Error> {
    tokio::fs::create_dir_all(dir).await?;
    let path = dir.join(vault.file());
    match tokio::fs::read(&path).await {
        Ok(sealed) => {
            let secret = vault.open(&sealed)?;
            let bytes: [u8; SECRET_BYTES] = secret.try_into().map_err(|_| Error::CorruptKey(path))?;
            Ok(SecretKey::from_bytes(&bytes))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let key = SecretKey::generate();
            let sealed = vault.seal(&key.to_bytes())?;
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(OWNER_READ_WRITE)
                .open(&path)
                .await?;
            file.write_all(&sealed).await?;
            file.sync_all().await?;
            tracing::info!(id = %key.public(), file = vault.file(), "generated device identity");
            // A plaintext key left from before the vault is not carried over (the new identity
            // is the point) and not left lying about either.
            if vault.file() != PLAIN_FILE {
                match tokio::fs::remove_file(dir.join(PLAIN_FILE)).await {
                    Ok(()) => tracing::info!("removed the old plaintext identity"),
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(key)
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const PERMISSION_BITS: u32 = 0o777;

    /// Stands in for the Keystore: flips every bit, and refuses what it did not seal.
    struct Flip;

    const FLIP_FILE: &str = "secret.key.flipped";
    const FLIP_MARK: u8 = 0xA5;

    impl Vault for Flip {
        fn file(&self) -> &'static str {
            FLIP_FILE
        }

        fn seal(&self, secret: &[u8]) -> Result<Vec<u8>, Error> {
            Ok(std::iter::once(FLIP_MARK).chain(secret.iter().map(|b| !b)).collect())
        }

        fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, Error> {
            match sealed.split_first() {
                Some((&FLIP_MARK, rest)) => Ok(rest.iter().map(|b| !b).collect()),
                _ => Err(Error::Vault("not sealed here".to_owned())),
            }
        }
    }

    #[tokio::test]
    async fn key_is_created_once_and_reloaded() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let first = load_or_create(dir.path(), &Plain).await?;
        let second = load_or_create(dir.path(), &Plain).await?;
        assert_eq!(first.public(), second.public());
        Ok(())
    }

    #[tokio::test]
    async fn key_file_is_owner_only() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        load_or_create(dir.path(), &Plain).await?;
        let mode = tokio::fs::metadata(dir.path().join(PLAIN_FILE)).await?.permissions().mode();
        assert_eq!(mode & PERMISSION_BITS, OWNER_READ_WRITE);
        Ok(())
    }

    #[tokio::test]
    async fn corrupt_key_file_is_an_error() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        tokio::fs::write(dir.path().join(PLAIN_FILE), b"short").await?;
        assert!(matches!(load_or_create(dir.path(), &Plain).await, Err(Error::CorruptKey(_))));
        Ok(())
    }

    #[tokio::test]
    async fn a_vault_keeps_it_sealed_and_opens_it_again() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let key = load_or_create(dir.path(), &Flip).await?;
        let on_disk = tokio::fs::read(dir.path().join(FLIP_FILE)).await?;
        assert!(!on_disk.windows(SECRET_BYTES).any(|window| window == key.to_bytes()));
        assert_eq!(load_or_create(dir.path(), &Flip).await?.public(), key.public());
        Ok(())
    }

    /// A sealed file this phone cannot open (a backup restored elsewhere) stops the start; it
    /// is not replaced by a new identity.
    #[tokio::test]
    async fn an_unopenable_identity_is_an_error_not_a_new_one() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        tokio::fs::write(dir.path().join(FLIP_FILE), [0; SECRET_BYTES + 1]).await?;
        assert!(matches!(load_or_create(dir.path(), &Flip).await, Err(Error::Vault(_))));
        assert_eq!(tokio::fs::read(dir.path().join(FLIP_FILE)).await?, [0; SECRET_BYTES + 1]);
        Ok(())
    }

    /// No migration: a vault starts a new identity and removes the old plaintext one.
    #[tokio::test]
    async fn a_plaintext_key_is_not_carried_over_but_removed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let old = load_or_create(dir.path(), &Plain).await?;
        let new = load_or_create(dir.path(), &Flip).await?;
        assert_ne!(old.public(), new.public());
        assert!(!tokio::fs::try_exists(dir.path().join(PLAIN_FILE)).await?);
        Ok(())
    }
}
