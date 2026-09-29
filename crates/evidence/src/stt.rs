//! Resumable, bounded pre-recorded STT. The application persists each successful
//! upload URL and transcript ID before advancing. Never retry an ambiguous POST
//! automatically: a timeout may have created a billable transcript already.
use crate::{Error, Result};
use reqwest::{Client, Method, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use url::Url;

pub const SPEECH_MODEL: &str = "universal-2";
const RESPONSE_CAP: usize = 2 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordedWord {
    pub text: String,
    pub start: u64,
    pub end: u64,
    pub confidence: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordedTranscript {
    pub id: String,
    pub text: String,
    pub words: Vec<RecordedWord>,
    pub audio_duration: f64,
}
#[derive(Debug)]
pub enum TranscriptStatus {
    Pending,
    Completed(RecordedTranscript),
    Failed,
}
pub struct AssemblyAiTranscriber {
    client: Client,
    key: String,
    base: Url,
}
impl AssemblyAiTranscriber {
    pub fn new(key: String) -> Result<Self> {
        Self::with_endpoint(key, Url::parse("https://api.assemblyai.com/").unwrap())
    }
    /// Only the official origin or a literal HTTP loopback test server is accepted.
    pub fn with_endpoint(key: String, base: Url) -> Result<Self> {
        let official = base.scheme() == "https"
            && base.host_str() == Some("api.assemblyai.com")
            && base.port_or_known_default() == Some(443);
        let test =
            base.scheme() == "http" && matches!(base.host_str(), Some("127.0.0.1" | "[::1]"));
        if key.trim().is_empty()
            || !(official || test)
            || base.path() != "/"
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(Error::Invalid("transcription configuration"));
        }
        Ok(Self {
            client: Client::builder()
                .redirect(Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(40))
                .build()?,
            key,
            base,
        })
    }
    async fn response(&self, request: reqwest::RequestBuilder) -> Result<serde_json::Value> {
        let mut response = request.header("Authorization", &self.key).send().await?;
        if !response.status().is_success() {
            // Neither provider diagnostic text nor signed upload URLs enter logs.
            return Err(Error::Invalid("transcription provider request rejected"));
        }
        if response
            .content_length()
            .is_some_and(|n| n > RESPONSE_CAP as u64)
        {
            return Err(Error::Invalid("transcription response size"));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > RESPONSE_CAP {
                return Err(Error::Invalid("transcription response size"));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(serde_json::from_slice(&body)?)
    }
    pub async fn upload(&self, wav: &[u8]) -> Result<String> {
        if wav.len() < 44
            || wav.len() > 390 * 24_000 * 2 + 44
            || &wav[..4] != b"RIFF"
            || &wav[8..12] != b"WAVE"
            || wav[22..24] != 1u16.to_le_bytes()
        {
            return Err(Error::Invalid("customer WAV upload format or size"));
        }
        let value = self
            .response(
                self.client
                    .post(self.base.join("v2/upload").unwrap())
                    .header("Content-Type", "application/octet-stream")
                    .body(wav.to_vec()),
            )
            .await?;
        let upload = value["upload_url"]
            .as_str()
            .ok_or(Error::Invalid("transcription upload response"))?;
        validate_upload_url(upload)?;
        Ok(upload.into())
    }
    pub async fn submit(&self, upload_url: &str) -> Result<String> {
        validate_upload_url(upload_url)?;
        let value = self
            .response(
                self.client
                    .post(self.base.join("v2/transcript").unwrap())
                    .json(
                        &json!({"audio_url": upload_url, "speech_models": [SPEECH_MODEL],
                "language_code": "en", "punctuate": true, "format_text": true,
                "disfluencies": true, "filter_profanity": false}),
                    ),
            )
            .await?;
        let id = value["id"]
            .as_str()
            .ok_or(Error::Invalid("transcription submission identity"))?;
        validate_id(id)?;
        if !matches!(
            value["status"].as_str(),
            Some("queued" | "processing" | "completed")
        ) {
            return Err(Error::Invalid("transcription submission status"));
        }
        Ok(id.into())
    }
    pub async fn poll(&self, id: &str) -> Result<TranscriptStatus> {
        validate_id(id)?;
        let value = self
            .response(
                self.client
                    .get(self.base.join(&format!("v2/transcript/{id}")).unwrap()),
            )
            .await?;
        if value["id"] != id {
            return Err(Error::Invalid("transcription identity mismatch"));
        }
        match value["status"].as_str() {
            Some("queued" | "processing") => Ok(TranscriptStatus::Pending),
            Some("error") => Ok(TranscriptStatus::Failed),
            Some("completed") => {
                let transcript: RecordedTranscript = serde_json::from_value(value)?;
                if transcript.words.is_empty()
                    || transcript.words.len() > 20_000
                    || transcript.text.len() > 250_000
                    || !transcript.audio_duration.is_finite()
                    || !(0.0..=391.0).contains(&transcript.audio_duration)
                {
                    return Err(Error::Invalid("transcription completed response"));
                }
                Ok(TranscriptStatus::Completed(transcript))
            }
            _ => Err(Error::Invalid("transcription status")),
        }
    }
    pub async fn delete(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        let response = self
            .client
            .request(
                Method::DELETE,
                self.base.join(&format!("v2/transcript/{id}")).unwrap(),
            )
            .header("Authorization", &self.key)
            .send()
            .await?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(Error::Invalid("transcription cleanup failed"))
        }
    }
}
fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 100
        || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        Err(Error::Invalid("transcription identity"))
    } else {
        Ok(())
    }
}
fn validate_upload_url(value: &str) -> Result<()> {
    let url = Url::parse(value).map_err(|_| Error::Invalid("transcription upload URL"))?;
    if value.len() > 4096
        || url.scheme() != "https"
        || url.host_str() != Some("cdn.assemblyai.com")
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        Err(Error::Invalid("transcription upload URL"))
    } else {
        Ok(())
    }
}
