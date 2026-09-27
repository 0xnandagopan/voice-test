use crate::{Error, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

/// Implementations must keep objects private and never return public URLs.
#[async_trait]
pub trait PrivateStorage: Send + Sync {
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<()>;
    async fn read(&self, key: &str) -> Result<Vec<u8>>;
    async fn delete(&self, key: &str) -> Result<()>;
}
#[derive(Clone)]
pub struct LocalPrivateStorage {
    root: PathBuf,
}
impl LocalPrivateStorage {
    pub async fn new(root: impl AsRef<Path>) -> Result<Self> {
        tokio::fs::create_dir_all(root.as_ref()).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(root.as_ref(), std::fs::Permissions::from_mode(0o700))
                .await?;
        }
        Ok(Self {
            root: tokio::fs::canonicalize(root).await?,
        })
    }
    fn path(&self, key: &str) -> Result<PathBuf> {
        // Flat opaque keys also prevent symlink traversal through nested directories.
        if key.is_empty()
            || key.len() > 240
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            || key.starts_with('.')
        {
            return Err(Error::Invalid("private object key"));
        }
        Ok(self.root.join(key))
    }
}
#[async_trait]
impl PrivateStorage for LocalPrivateStorage {
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let mut file = options.open(self.path(key)?).await?;
        file.write_all(bytes).await?;
        file.sync_all().await?;
        Ok(())
    }
    async fn read(&self, key: &str) -> Result<Vec<u8>> {
        let path = self.path(key)?;
        if tokio::fs::symlink_metadata(&path)
            .await?
            .file_type()
            .is_symlink()
        {
            return Err(Error::Invalid("private object symlink"));
        }
        Ok(tokio::fs::read(path).await?)
    }
    async fn delete(&self, key: &str) -> Result<()> {
        match tokio::fs::remove_file(self.path(key)?).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
