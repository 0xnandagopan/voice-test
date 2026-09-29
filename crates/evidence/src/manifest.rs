use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

#[derive(Debug, Deserialize)]
pub struct Timeline {
    pub session_id: String,
    pub started_at_unix_ms: i64,
    #[serde(default)]
    pub turns: Vec<Turn>,
}
#[derive(Debug, Deserialize)]
pub struct Turn {
    pub turn_id: String,
    pub item_id: Option<String>,
    pub status: String,
    pub user_transcript: Option<String>,
    pub user_speech_started_at_ms: Option<i64>,
    pub user_speech_ended_at_ms: Option<i64>,
    pub agent_text: Option<String>,
    pub agent_reply_started_at_ms: Option<i64>,
    pub agent_reply_ended_at_ms: Option<i64>,
}
#[derive(Debug, Deserialize)]
pub struct Metadata {
    pub session_id: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: chrono::DateTime<chrono::Utc>,
    pub format: String,
    pub channels: u32,
    pub channel_layout: String,
    pub sample_rate: u32,
    pub file: String,
    pub dropped_chunks: Option<u64>,
    pub uploaded_chunks: Option<u64>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Segment {
    pub source_id: String,
    pub turn_id: String,
    pub item_id: Option<String>,
    pub speaker: String,
    pub channel: u32,
    pub text: String,
    pub turn_status: String,
    pub source_range_ms: Option<[u64; 2]>,
    pub alignment: String,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub provider_session_id: String,
    #[serde(default)]
    pub provider_close_reason: Option<String>,
    #[serde(default)]
    pub product_end_reason: Option<String>,
    pub artifact_identity: String,
    pub recording_sha256: String,
    pub timeline_sha256: String,
    pub metadata_sha256: String,
    pub duration_ms: u64,
    pub recording_started_at_unix_ms: i64,
    pub timeline_started_at_unix_ms: i64,
    pub dropped_chunks: Option<u64>,
    pub uploaded_chunks: Option<u64>,
    pub segments: Vec<Segment>,
    pub incomplete_turn_ids: Vec<String>,
    pub recording_validation: String,
    pub approval_eligible: bool,
    #[serde(default)]
    pub operator_alignment: Vec<crate::alignment::OperatorAlignmentProof>,
    #[serde(default)]
    pub automatic_alignment: Vec<crate::alignment::OperatorAlignmentProof>,
    #[serde(default)]
    pub media: Option<crate::media::MediaReport>,
}

pub fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn build(
    session: &str,
    audio: &[u8],
    timeline_bytes: &[u8],
    metadata_bytes: &[u8],
) -> Result<Manifest> {
    let timeline: Timeline = serde_json::from_slice(timeline_bytes)?;
    let meta: Metadata = serde_json::from_slice(metadata_bytes)?;
    if timeline.session_id != session || meta.session_id != session {
        return Err(Error::Invalid("session mismatch"));
    }
    if meta.format != "ogg/opus"
        || meta.channels != 2
        || meta.channel_layout != "stereo (left=user, right=agent)"
        || meta.sample_rate == 0
        || !audio.starts_with(b"OggS")
    {
        return Err(Error::Invalid("unsupported recording format"));
    }
    let duration = (meta.ended_at - meta.started_at).num_milliseconds();
    if !(1..=390_000).contains(&duration) || meta.file.is_empty() {
        return Err(Error::Invalid("recording clock or duration"));
    }
    let mut seen = HashSet::new();
    let mut segments = vec![];
    let mut incomplete = vec![];
    for turn in timeline.turns {
        if turn.turn_id.is_empty()
            || !seen.insert(turn.turn_id.clone())
            || !matches!(turn.status.as_str(), "completed" | "interrupted")
        {
            return Err(Error::Invalid("turn identity or status"));
        }
        if turn.status != "completed" {
            incomplete.push(turn.turn_id.clone());
        }
        // Provider times are Unix milliseconds. Recording and timeline clocks have
        // distinct starts in live artifacts; preserve both, never silently equate them.
        let range = |start: Option<i64>, end: Option<i64>| -> Result<Option<[u64; 2]>> {
            match (start, end) {
                (Some(start), Some(end)) => {
                    let start = start
                        .checked_sub(meta.started_at.timestamp_millis())
                        .ok_or(Error::Invalid("range overflow"))?;
                    let end = end
                        .checked_sub(meta.started_at.timestamp_millis())
                        .ok_or(Error::Invalid("range overflow"))?;
                    if start < 0 || start >= end || end > duration {
                        return Err(Error::Invalid("source range outside recording"));
                    }
                    Ok(Some([start as u64, end as u64]))
                }
                _ => Ok(None),
            }
        };
        let agent_range = range(turn.agent_reply_started_at_ms, turn.agent_reply_ended_at_ms)?;
        let customer_range = range(turn.user_speech_started_at_ms, turn.user_speech_ended_at_ms)?;
        for (speaker, channel, text, range) in [
            ("customer", 0, turn.user_transcript, customer_range),
            ("agent", 1, turn.agent_text, agent_range),
        ] {
            if let Some(text) = text.filter(|s| !s.trim().is_empty()) {
                segments.push(Segment {
                    source_id: format!("{session}/turn/{}/{speaker}", turn.turn_id),
                    turn_id: turn.turn_id.clone(),
                    item_id: turn.item_id.clone(),
                    speaker: speaker.into(),
                    channel,
                    text,
                    turn_status: turn.status.clone(),
                    source_range_ms: range,
                    alignment: if range.is_some() {
                        "provider_range_unverified"
                    } else {
                        "missing_alignment"
                    }
                    .into(),
                });
            }
        }
    }
    Ok(Manifest {
        schema_version: 1,
        provider_session_id: session.into(),
        provider_close_reason: None,
        product_end_reason: None,
        artifact_identity: format!("{session}:{}", meta.file),
        recording_sha256: digest(audio),
        timeline_sha256: digest(timeline_bytes),
        metadata_sha256: digest(metadata_bytes),
        duration_ms: duration as u64,
        recording_started_at_unix_ms: meta.started_at.timestamp_millis(),
        timeline_started_at_unix_ms: timeline.started_at_unix_ms,
        dropped_chunks: meta.dropped_chunks,
        uploaded_chunks: meta.uploaded_chunks,
        segments,
        incomplete_turn_ids: incomplete,
        // Header + metadata validation is deliberately not decoded-media validation.
        recording_validation: "header_and_metadata_only".into(),
        approval_eligible: false,
        operator_alignment: vec![],
        automatic_alignment: vec![],
        media: None,
    })
}
