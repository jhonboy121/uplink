//! Device identity: an iroh secret key persisted in the data dir.
//!
//! Stored as raw bytes with owner-only permissions. On Android this file will be wrapped with a
//! Keystore key (security to-do in docs/plan.md).

use std::io::ErrorKind;
use std::path::Path;

use iroh::SecretKey;
use tokio::io::AsyncWriteExt;

use crate::Error;

const SECRET_KEY_FILE: &str = "secret.key";
const OWNER_READ_WRITE: u32 = 0o600;

/// Loads the device key, generating and saving one on first use.
pub async fn load_or_create(dir: &Path) -> Result<SecretKey, Error> {
    tokio::fs::create_dir_all(dir).await?;
    let path = dir.join(SECRET_KEY_FILE);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let bytes = bytes.try_into().map_err(|_| Error::CorruptKey(path))?;
            Ok(SecretKey::from_bytes(&bytes))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let key = SecretKey::generate();
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(OWNER_READ_WRITE)
                .open(&path)
                .await?;
            file.write_all(&key.to_bytes()).await?;
            file.sync_all().await?;
            tracing::info!(id = %key.public(), "generated device identity");
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

    #[tokio::test]
    async fn key_is_created_once_and_reloaded() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let first = load_or_create(dir.path()).await?;
        let second = load_or_create(dir.path()).await?;
        assert_eq!(first.public(), second.public());
        Ok(())
    }

    #[tokio::test]
    async fn key_file_is_owner_only() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        load_or_create(dir.path()).await?;
        let mode = tokio::fs::metadata(dir.path().join(SECRET_KEY_FILE)).await?.permissions().mode();
        assert_eq!(mode & PERMISSION_BITS, OWNER_READ_WRITE);
        Ok(())
    }

    #[tokio::test]
    async fn corrupt_key_file_is_an_error() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        tokio::fs::write(dir.path().join(SECRET_KEY_FILE), b"short").await?;
        assert!(matches!(load_or_create(dir.path()).await, Err(Error::CorruptKey(_))));
        Ok(())
    }
}
