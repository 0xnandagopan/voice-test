//! Private recording recovery. Imported does not mean aligned or approval eligible.
pub mod jobs;
pub mod manifest;
pub mod media;
pub mod provider;
pub mod recovery;
pub mod storage;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("provider artifacts are not ready")]
    NotReady,
    #[error("invalid or unsupported evidence: {0}")]
    Invalid(&'static str),
    #[error("provider request failed")]
    Http(#[from] reqwest::Error),
    #[error("artifact JSON is invalid")]
    Json(#[from] serde_json::Error),
    #[error("private storage operation failed")]
    Io(#[from] std::io::Error),
    #[error("database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("job lease or interview eligibility changed")]
    Stale,
}

pub type Result<T> = std::result::Result<T, Error>;
