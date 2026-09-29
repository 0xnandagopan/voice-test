//! Explicit operator listening verification, bound to immutable evidence bytes.
//! This records a human decision; decoding alone never establishes alignment.
use crate::{
    Error, Result,
    manifest::{Manifest, digest},
    media::CandidateClip,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorAlignmentConfirmation {
    pub recording_sha256: String,
    pub timeline_sha256: String,
    pub source_id: String,
    pub source_text_sha256: String,
    pub source_range_ms: [u64; 2],
    pub clip_sha256: String,
    pub listened: bool,
    pub transcript_matches: bool,
    pub complete_answer: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorAlignmentProof {
    pub recording_sha256: String,
    pub timeline_sha256: String,
    pub source_id: String,
    pub source_text_sha256: String,
    pub source_range_ms: [u64; 2],
    pub clip_sha256: String,
    pub method: String,
    pub verified_by: String,
    pub verified_at: DateTime<Utc>,
}

/// The caller must authenticate the operator, fence evidence revisions, and persist
/// proof, workflow invalidation and audit atomically. `clip` must have just been
/// extracted by FfmpegValidator from the current private source recording.
pub fn confirm_operator_alignment(
    manifest: &mut Manifest,
    clip: &CandidateClip,
    confirmation: &OperatorAlignmentConfirmation,
    actor: &str,
    verified_at: DateTime<Utc>,
) -> Result<()> {
    if actor.is_empty()
        || actor.len() > 200
        || !confirmation.listened
        || !confirmation.transcript_matches
        || !confirmation.complete_answer
    {
        return Err(Error::Invalid(
            "explicit operator listening confirmation required",
        ));
    }
    let source = manifest
        .segments
        .iter()
        .find(|s| {
            s.source_id == confirmation.source_id && s.speaker == "customer" && s.channel == 0
        })
        .ok_or(Error::Invalid("customer source identity"))?;
    if confirmation.recording_sha256 != manifest.recording_sha256
        || confirmation.timeline_sha256 != manifest.timeline_sha256
        || confirmation.source_text_sha256 != digest(source.text.as_bytes())
        || confirmation.source_id != clip.source_id
        || confirmation.recording_sha256 != clip.source_recording_sha256
        || confirmation.source_range_ms != clip.source_range_ms
        || confirmation.clip_sha256 != digest(&clip.wav)
    {
        return Err(Error::Invalid("alignment confirmation evidence changed"));
    }
    let media = manifest
        .media
        .as_ref()
        .ok_or(Error::Invalid("decoded recording required"))?;
    let [start, end] = confirmation.source_range_ms;
    if start >= end
        || end > media.decoded_duration_ms
        || end > 390_000
        || media.channels != 2
        || media.metadata_duration_delta_ms.unsigned_abs() > 1500
        || manifest.dropped_chunks != Some(0)
    {
        return Err(Error::Invalid("recording completeness or range unverified"));
    }
    // The operator may replace the activity detector's suggested bounds but must
    // include at least one entire audible group. A boundary cannot cut through
    // another group, and two answers cannot claim overlapping recorded bytes.
    let groups: Vec<_> = media
        .customer_activity_groups_ms
        .iter()
        .filter(|[a, b]| *a < end && *b > start)
        .collect();
    if groups.is_empty() || groups.iter().any(|[a, b]| *a < start || *b > end) {
        return Err(Error::Invalid("whole audible answer range required"));
    }
    if manifest.operator_alignment.iter().any(|p| {
        p.source_id != source.source_id
            && p.source_range_ms[0] < end
            && p.source_range_ms[1] > start
    }) {
        return Err(Error::Invalid("verified answer ranges overlap"));
    }
    // An interrupted agent reply does not prove an interrupted customer answer.
    // An intentional Pause may close after a completed customer turn. Its last
    // answer remains verifiable through explicit listening if the durable ledger
    // has no incomplete marker. Transport loss or a partial Pause tail is still
    // uncertain; a listening checkbox alone cannot override those signals.
    let completed_pause = manifest.product_end_reason.as_deref() == Some("explicit_stop")
        && source.turn_status == "completed"
        && !manifest
            .incomplete_turn_ids
            .iter()
            .any(|id| id == &source.turn_id);
    if manifest.product_end_reason.as_deref() != Some("explicit_finish")
        && !completed_pause
        && manifest
            .segments
            .iter()
            .rev()
            .find(|s| s.speaker == "customer")
            .is_some_and(|s| s.source_id == source.source_id)
    {
        return Err(Error::Invalid("last answer completion unresolved"));
    }
    let proof = OperatorAlignmentProof {
        recording_sha256: confirmation.recording_sha256.clone(),
        timeline_sha256: confirmation.timeline_sha256.clone(),
        source_id: confirmation.source_id.clone(),
        source_text_sha256: confirmation.source_text_sha256.clone(),
        source_range_ms: confirmation.source_range_ms,
        clip_sha256: confirmation.clip_sha256.clone(),
        method: "operator_whole_answer_listening_v1".into(),
        verified_by: actor.into(),
        verified_at,
    };
    manifest.automatic_alignment.clear();
    manifest
        .operator_alignment
        .retain(|p| p.source_id != proof.source_id);
    manifest.operator_alignment.push(proof);
    let all_verified = manifest
        .segments
        .iter()
        .filter(|s| s.speaker == "customer")
        .all(|s| source_proof(manifest, &s.source_id).is_some());
    let all_audio_covered = media.customer_activity_groups_ms.iter().all(|[a, b]| {
        manifest
            .operator_alignment
            .iter()
            .any(|p| p.source_range_ms[0] <= *a && p.source_range_ms[1] >= *b)
    });
    let eligible = all_verified && all_audio_covered && !manifest.operator_alignment.is_empty();
    manifest.approval_eligible = eligible;
    manifest.media.as_mut().unwrap().alignment_proven = eligible;
    manifest.recording_validation = if eligible {
        "operator_listened_alignment_verified"
    } else {
        "operator_alignment_partially_verified"
    }
    .into();
    Ok(())
}

/// Check every binding when consuming a proof; a transcript correction or replaced
/// recording makes an old proof unusable even if the caller forgot to clear it.
pub fn source_proof<'a>(
    manifest: &'a Manifest,
    source_id: &str,
) -> Option<&'a OperatorAlignmentProof> {
    let media = manifest.media.as_ref()?;
    if manifest.dropped_chunks != Some(0) || media.channels != 2 {
        return None;
    }
    let source = manifest
        .segments
        .iter()
        .find(|s| s.source_id == source_id && s.speaker == "customer" && s.channel == 0)?;
    manifest
        .operator_alignment
        .iter()
        .chain(manifest.automatic_alignment.iter())
        .find(|p| {
            p.source_id == source_id
                && p.recording_sha256 == manifest.recording_sha256
                && p.timeline_sha256 == manifest.timeline_sha256
                && p.source_text_sha256 == digest(source.text.as_bytes())
                && p.source_range_ms[0] < p.source_range_ms[1]
                && p.source_range_ms[1] <= media.decoded_duration_ms
                && ((p.method == "operator_whole_answer_listening_v1"
                    && media.metadata_duration_delta_ms.unsigned_abs() <= 1500)
                    || (p.method == crate::automatic_alignment::METHOD
                        && p.verified_by.starts_with("assemblyai:")
                        && crate::automatic_alignment::preflight(manifest).is_ok()))
                && !p.verified_by.is_empty()
        })
}
