use crate::{Error, Result};
use async_trait::async_trait;
use reqwest::{Client, redirect::Policy};
use serde::Deserialize;
use std::time::Duration;
use url::Url;

pub struct Artifacts {
    pub close_reason: Option<String>,
    pub audio: Vec<u8>,
    pub timeline: Vec<u8>,
    pub metadata: Vec<u8>,
}
#[async_trait]
pub trait HistoryProvider: Send + Sync {
    async fn fetch(&self, session_id: &str) -> Result<Artifacts>;
}

pub struct AssemblyHistory {
    client: Client,
    api_key: String,
    base: Url,
    allowed_artifact_hosts: Vec<String>,
    allow_loopback_http: bool,
}
#[derive(Deserialize)]
struct Session {
    id: String,
    status: String,
    public_close_reason: Option<String>,
    #[serde(default)]
    artifacts: Vec<Artifact>,
}
#[derive(Deserialize)]
struct Artifact {
    #[serde(rename = "type")]
    kind: String,
    url: String,
}
impl AssemblyHistory {
    pub fn new(api_key: String, allowed_artifact_hosts: Vec<String>) -> Result<Self> {
        Self::with_endpoint(
            api_key,
            Url::parse("https://agents.assemblyai.com").unwrap(),
            allowed_artifact_hosts,
            false,
        )
    }
    /// Custom HTTP endpoints are limited to literal loopback addresses for deterministic tests.
    pub fn with_endpoint(
        api_key: String,
        base: Url,
        hosts: Vec<String>,
        allow_loopback_http: bool,
    ) -> Result<Self> {
        if api_key.trim().is_empty() {
            return Err(Error::Invalid("VOICE_AGENT_API_KEY is required"));
        }
        let value = Self {
            client: Client::builder()
                .redirect(Policy::none())
                .timeout(Duration::from_secs(25))
                .connect_timeout(Duration::from_secs(5))
                .build()?,
            api_key,
            base,
            allowed_artifact_hosts: hosts,
            allow_loopback_http,
        };
        value.validate_url(&value.base, false)?;
        Ok(value)
    }
    fn validate_url(&self, url: &Url, artifact: bool) -> Result<()> {
        let host = url.host_str().ok_or(Error::Invalid("download host"))?;
        let local = self.allow_loopback_http
            && (host == "127.0.0.1" || host == "[::1]")
            && url.scheme() == "http";
        if (!local && url.scheme() != "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || (!local && url.port_or_known_default() != Some(443))
        {
            return Err(Error::Invalid("unsafe download URL"));
        }
        if artifact
            && !self
                .allowed_artifact_hosts
                .iter()
                .any(|allowed| allowed == host)
        {
            return Err(Error::Invalid("artifact host is not explicitly allowed"));
        }
        if !local && host.parse::<std::net::IpAddr>().is_ok() {
            return Err(Error::Invalid("literal download address"));
        }
        Ok(())
    }
    async fn download(&self, url: Url, cap: usize, authenticated: bool) -> Result<Vec<u8>> {
        self.validate_url(&url, !authenticated)?;
        let mut request = self.client.get(url);
        if authenticated {
            request = request.header("Authorization", &self.api_key);
        }
        let mut response = request.send().await?.error_for_status()?;
        if !response.status().is_success() {
            return Err(Error::Invalid("artifact redirect rejected"));
        }
        if response.content_length().is_some_and(|n| n > cap as u64) {
            return Err(Error::Invalid("artifact size limit"));
        }
        let mut data = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if data.len().saturating_add(chunk.len()) > cap {
                return Err(Error::Invalid("artifact size limit"));
            }
            data.extend_from_slice(&chunk);
        }
        Ok(data)
    }
}
#[async_trait]
impl HistoryProvider for AssemblyHistory {
    async fn fetch(&self, session_id: &str) -> Result<Artifacts> {
        if session_id.is_empty()
            || session_id.len() > 160
            || !session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(Error::Invalid("session identifier"));
        }
        let url = self
            .base
            .join(&format!("/v1/sessions/{session_id}"))
            .map_err(|_| Error::Invalid("session URL"))?;
        let session: Session = serde_json::from_slice(&self.download(url, 1_048_576, true).await?)?;
        if session.id != session_id {
            return Err(Error::Invalid("session response mismatch"));
        }
        if session.status != "completed" {
            return Err(Error::NotReady);
        }
        let get = |kind: &str| -> Result<Url> {
            let matching: Vec<_> = session
                .artifacts
                .iter()
                .filter(|a| a.kind == kind)
                .collect();
            match matching.as_slice() {
                [a] => Url::parse(&a.url).map_err(|_| Error::Invalid("artifact URL")),
                [] => Err(Error::NotReady),
                _ => Err(Error::Invalid("duplicate artifacts")),
            }
        };
        Ok(Artifacts {
            close_reason: session.public_close_reason.clone(),
            audio: self
                .download(get("audio")?, 32 * 1024 * 1024, false)
                .await?,
            timeline: self
                .download(get("timeline")?, 2 * 1024 * 1024, false)
                .await?,
            metadata: self.download(get("metadata")?, 64 * 1024, false).await?,
        })
    }
}
