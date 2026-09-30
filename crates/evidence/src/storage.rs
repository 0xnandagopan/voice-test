use crate::{Error, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

mod s3;
pub use s3::S3PrivateStorage;

/// Covers the bounded source recordings and decoded clips, not arbitrary uploads.
pub const MAX_PRIVATE_OBJECT_BYTES: usize = 64 * 1024 * 1024;

/// Explicit credentials are required for S3; no implicit instance metadata lookup.
pub async fn storage_from_env() -> Result<Box<dyn PrivateStorage>> {
    storage_from_config(|name| std::env::var(name).ok()).await
}

async fn storage_from_config(
    get: impl Fn(&str) -> Option<String>,
) -> Result<Box<dyn PrivateStorage>> {
    match get("EVIDENCE_STORAGE_BACKEND")
        .as_deref()
        .unwrap_or("local")
    {
        "local" => Ok(Box::new(
            LocalPrivateStorage::new(
                get("EVIDENCE_STORAGE_DIR").unwrap_or_else(|| ".local/private-evidence".into()),
            )
            .await?,
        )),
        "s3" => Ok(Box::new(S3PrivateStorage::from_config(get)?)),
        _ => Err(Error::Invalid(
            "EVIDENCE_STORAGE_BACKEND must be local or s3",
        )),
    }
}

fn validate_key(key: &str) -> Result<()> {
    // Flat opaque keys prevent traversal and ambiguous URL encoding on both backends.
    if key.is_empty()
        || key.len() > 240
        || key.starts_with('.')
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(Error::Invalid("private object key"));
    }
    Ok(())
}

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
        validate_key(key)?;
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
