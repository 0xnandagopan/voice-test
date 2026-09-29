//! Reconstruction is read-only: provider text cannot advance interview progress.
use crate::{Error, Result, manifest::Manifest};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
pub struct RecoveredAnswer {
    pub source_id: String,
    pub provider_attempt_id: Uuid,
    pub provider_session_id: String,
    pub turn_id: String,
    pub text: String,
    pub source_range_ms: Option<[u64; 2]>,
    pub candidate_source_range_ms: Option<[u64; 2]>,
    pub needs_alignment_review: bool,
    pub status: String,
    pub completion_basis: String,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct AttemptRecovery {
    pub provider_attempt_id: Uuid,
    pub status: String,
    pub evidence_revision: Option<i64>,
    pub incomplete_turn_ids: Vec<String>,
    pub artifact_sha256: Option<String>,
    pub recommended_action: String,
    pub untranscribed_audio_ranges_ms: Vec<[u64; 2]>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct RecoveryContext {
    pub interview_id: Uuid,
    pub interview_revision: i64,
    pub topic_index: i32,
    pub followup_counts: serde_json::Value,
    pub time_consumed_seconds: i32,
    pub attempts: Vec<AttemptRecovery>,
    /// Bounded, decoded utterances, still subject to alignment and meaning checks.
    pub recorded_utterances: Vec<RecoveredAnswer>,
    /// Preserve partial/unverified text, but never use it as approved evidence.
    pub unresolved_answers: Vec<RecoveredAnswer>,
    pub requires_customer_confirmation: bool,
    pub may_advance_progress: bool,
    pub recommended_action: String,
}

pub fn reconstruct(
    attempt: Uuid,
    manifest: &Manifest,
) -> (Vec<RecoveredAnswer>, Vec<RecoveredAnswer>) {
    let mut recorded = vec![];
    let mut unresolved = vec![];
    let last_customer = manifest
        .segments
        .iter()
        .rev()
        .find(|s| s.speaker == "customer")
        .map(|s| s.source_id.as_str());
    for segment in manifest.segments.iter().filter(|s| s.speaker == "customer") {
        let check = manifest.media.as_ref().and_then(|media| {
            media
                .ranges
                .iter()
                .find(|r| r.source_id == segment.source_id)
        });
        let available = check.is_some_and(|r| r.within_recording && r.audible_samples > 0);
        let clean = manifest.dropped_chunks == Some(0);
        let bounded = segment.source_range_ms.is_some();
        let uncertain_tail = last_customer == Some(segment.source_id.as_str())
            && manifest.product_end_reason.as_deref() != Some("explicit_finish");
        let interrupted = segment.turn_status != "completed"
            || manifest
                .incomplete_turn_ids
                .iter()
                .any(|id| id == &segment.turn_id);
        let verified = crate::alignment::source_proof(manifest, &segment.source_id);
        let complete = verified.is_some()
            || (available && clean && bounded && !uncertain_tail && !interrupted);
        let item = RecoveredAnswer {
            source_id: segment.source_id.clone(),
            provider_attempt_id: attempt,
            provider_session_id: manifest.provider_session_id.clone(),
            turn_id: segment.turn_id.clone(),
            text: segment.text.clone(),
            source_range_ms: segment.source_range_ms,
            candidate_source_range_ms: verified
                .map(|p| p.source_range_ms)
                .or_else(|| check.and_then(|c| c.candidate_source_range_ms)),
            needs_alignment_review: verified.is_none(),
            status: if verified.is_some_and(|p| p.method == crate::automatic_alignment::METHOD) {
                "recorded_utterance_automatically_verified"
            } else if verified.is_some() {
                "recorded_utterance_operator_verified"
            } else if complete {
                "recorded_utterance_alignment_pending"
            } else if interrupted {
                "interrupted_turn_requires_confirmation"
            } else if uncertain_tail {
                "last_answer_completion_unconfirmed"
            } else if !bounded {
                "incomplete_or_unaligned"
            } else if !clean {
                "recording_completeness_unknown"
            } else {
                "recording_unverified"
            }
            .into(),
            completion_basis: if verified
                .is_some_and(|p| p.method == crate::automatic_alignment::METHOD)
            {
                "independent_customer_stt"
            } else if verified.is_some() {
                "operator_whole_answer_listening"
            } else if bounded {
                "provider_speech_bounds"
            } else {
                "no_complete_provider_speech_bounds"
            }
            .into(),
        };
        if complete {
            recorded.push(item)
        } else {
            unresolved.push(item)
        }
    }
    (recorded, unresolved)
}

pub async fn recover_interview(pool: &PgPool, interview: Uuid) -> Result<RecoveryContext> {
    let mut tx = pool.begin().await?;
    let state=sqlx::query("SELECT revision,topic_index,followup_counts,time_consumed_seconds,expires_at,deleted_at,state FROM interviews WHERE id=$1 FOR SHARE")
        .bind(interview).fetch_optional(&mut *tx).await?.ok_or(Error::Stale)?;
    // now() freezes at transaction start, before a potential row-lock wait.
    // Read the database wall clock only after acquiring the protected row.
    let current: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let expires: chrono::DateTime<chrono::Utc> = state.get("expires_at");
    if expires <= current
        || state
            .get::<Option<chrono::DateTime<chrono::Utc>>, _>("deleted_at")
            .is_some()
        || matches!(
            state.get::<String, _>("state").as_str(),
            "deleted" | "revoked"
        )
    {
        return Err(Error::Stale);
    }
    // Read only the canonical import job, scoped to both attempt and interview.
    // An exhausted worker can remain running until the next claim sweep; its
    // expired lease already means no automatic recovery attempt remains.
    let rows=sqlx::query("SELECT p.id,p.state,e.evidence_revision,e.manifest,COALESCE((p.provider_session_id IS NULL AND p.state='ended') OR j.status IN ('failed','cancelled','succeeded') OR (j.attempts>=j.max_attempts AND (j.status='queued' OR (j.status='running' AND (j.lease_until IS NULL OR j.lease_until<=clock_timestamp())))),false) AS artifacts_unavailable FROM provider_attempts p LEFT JOIN evidence_imports e ON e.provider_attempt_id=p.id AND e.interview_id=p.interview_id LEFT JOIN jobs j ON j.dedupe_key='import:'||p.id::text AND j.interview_id=p.interview_id AND j.kind='import_evidence' AND j.payload->>'provider_attempt_id'=p.id::text WHERE p.interview_id=$1 ORDER BY p.started_at,p.id")
        .bind(interview).fetch_all(&mut *tx).await?;
    let mut result = RecoveryContext {
        interview_id: interview,
        interview_revision: state.get("revision"),
        topic_index: state.get("topic_index"),
        followup_counts: state.get("followup_counts"),
        time_consumed_seconds: state.get("time_consumed_seconds"),
        attempts: vec![],
        recorded_utterances: vec![],
        unresolved_answers: vec![],
        requires_customer_confirmation: state.get::<String, _>("state") != "completed"
            && state.get::<i32, _>("time_consumed_seconds") < 360,
        may_advance_progress: false,
        recommended_action: "confirm_recovered_answers_before_resume".into(),
    };
    for row in rows {
        let attempt = row.get("id");
        let value: Option<serde_json::Value> = row.get("manifest");
        if let Some(value) = value {
            let manifest: Manifest = serde_json::from_value(value)?;
            let (recorded, unresolved) = reconstruct(attempt, &manifest);
            result.recorded_utterances.extend(recorded);
            result.unresolved_answers.extend(unresolved);
            result.attempts.push(AttemptRecovery {
                provider_attempt_id: attempt,
                status: if manifest.approval_eligible && !manifest.automatic_alignment.is_empty() {
                    "imported_automatic_alignment_verified"
                } else if manifest.approval_eligible {
                    "imported_operator_alignment_verified"
                } else if manifest.media.is_some() {
                    "imported_decoded_alignment_pending"
                } else {
                    "imported_unverified"
                }
                .into(),
                evidence_revision: row.get("evidence_revision"),
                incomplete_turn_ids: manifest.incomplete_turn_ids.clone(),
                recommended_action: if manifest.product_end_reason.as_deref()
                    != Some("explicit_finish")
                {
                    "confirm_last_answer_or_repeat"
                } else {
                    "review_recovered_answer_ranges"
                }
                .into(),
                untranscribed_audio_ranges_ms: manifest
                    .media
                    .as_ref()
                    .map(|media| {
                        media
                            .customer_activity_groups_ms
                            .iter()
                            .filter(|group| {
                                !manifest
                                    .segments
                                    .iter()
                                    .filter(|s| s.speaker == "customer")
                                    .any(|segment| {
                                        crate::alignment::source_proof(
                                            &manifest,
                                            &segment.source_id,
                                        )
                                        .map(|p| p.source_range_ms)
                                        .or(segment.source_range_ms)
                                        .is_some_and(
                                            |[start, end]| start < group[1] && end > group[0],
                                        )
                                    })
                            })
                            .copied()
                            .collect()
                    })
                    .unwrap_or_default(),
                artifact_sha256: Some(manifest.recording_sha256),
            });
        } else {
            let unavailable: bool = row.get("artifacts_unavailable");
            result.attempts.push(AttemptRecovery {
                provider_attempt_id: attempt,
                status: if unavailable {
                    "recording_artifacts_unavailable"
                } else {
                    "awaiting_recording_artifacts"
                }
                .into(),
                evidence_revision: None,
                incomplete_turn_ids: vec![],
                artifact_sha256: None,
                recommended_action: if unavailable {
                    "retry_recovery_or_discard"
                } else {
                    "wait_for_recording_recovery"
                }
                .into(),
                untranscribed_audio_ranges_ms: vec![],
            });
        }
    }
    if result
        .attempts
        .iter()
        .any(|a| a.status == "recording_artifacts_unavailable")
    {
        // One terminal gap must not be hidden by another attempt still polling.
        result.recommended_action = "retry_recovery_or_discard".into();
    } else if result
        .attempts
        .iter()
        .any(|a| a.status == "awaiting_recording_artifacts")
    {
        result.recommended_action = "wait_for_recording_recovery".into();
    } else if !result.unresolved_answers.is_empty()
        || result
            .attempts
            .iter()
            .any(|a| a.recommended_action == "confirm_last_answer_or_repeat")
    {
        result.recommended_action = "confirm_recovered_answers_and_repeat_unresolved".into();
    }
    if !result.requires_customer_confirmation {
        result.recommended_action = "review_available_recordings".into();
    }
    let still_current: bool = sqlx::query_scalar("SELECT clock_timestamp()<$1")
        .bind(expires)
        .fetch_one(&mut *tx)
        .await?;
    if !still_current {
        return Err(Error::Stale);
    }
    tx.commit().await?;
    Ok(result)
}
