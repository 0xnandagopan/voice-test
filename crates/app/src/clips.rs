//! Operator listening verification and immutable, bounded customer-selected clips.
use crate::{AppState, auth, error::ApiError, review, workflow};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;
use v0_domain::workflow::{Revisions, WorkflowView};
use v0_evidence::{
    alignment::{OperatorAlignmentConfirmation, confirm_operator_alignment},
    manifest::{Manifest, digest},
    media::{CandidateClip, FfmpegValidator},
    storage::{PrivateStorage, storage_from_env},
};

fn invalid() -> ApiError {
    ApiError::invalid(
        "This answer cannot be verified with the selected range. Check that the complete customer answer is present and matches the transcript.",
    )
}
fn alignment_error(error: v0_evidence::Error) -> ApiError {
    match error {
        v0_evidence::Error::Invalid("explicit operator listening confirmation required") => {
            ApiError::invalid(
                "Listen to the entire preview and confirm both transcript accuracy and a complete answer.",
            )
        }
        v0_evidence::Error::Invalid("alignment confirmation evidence changed") => {
            ApiError::conflict()
        }
        v0_evidence::Error::Invalid("recording completeness or range unverified") => {
            ApiError::invalid(
                "The recording has missing or unverified audio, or this range extends beyond it. Listening confirmation cannot override missing evidence.",
            )
        }
        v0_evidence::Error::Invalid("whole audible answer range required") => ApiError::invalid(
            "The selected range cuts through audible speech or contains no complete answer. Adjust the range and listen again.",
        ),
        v0_evidence::Error::Invalid("verified answer ranges overlap") => ApiError::invalid(
            "This range overlaps another verified answer. Review both ranges before confirming.",
        ),
        v0_evidence::Error::Invalid("last answer completion unresolved") => ApiError::invalid(
            "This attempt stopped before a confirmed Finish. Its last answer may be incomplete and cannot be verified here. Use the customer's recording recovery flow to review and continue.",
        ),
        _ => invalid(),
    }
}
async fn storage() -> Result<Box<dyn PrivateStorage>, ApiError> {
    storage_from_env().await.map_err(|_| invalid())
}
fn validator() -> FfmpegValidator {
    FfmpegValidator::new(
        std::env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".into()),
        std::env::var("FFPROBE_PATH").unwrap_or_else(|_| "ffprobe".into()),
        std::env::var("EVIDENCE_WORK_DIR").unwrap_or_else(|_| ".local/evidence-jobs".into()),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RangeQuery {
    start_ms: u64,
    end_ms: u64,
    evidence_revision: i64,
}
struct Prepared {
    clip: CandidateClip,
    manifest: Manifest,
    original: Value,
    attempt: Uuid,
}
async fn prepare(
    s: &AppState,
    id: Uuid,
    source: &str,
    range: [u64; 2],
) -> Result<Prepared, ApiError> {
    if source.len() > 240 || range[0] >= range[1] || range[1] > 390_000 {
        return Err(invalid());
    }
    let rows=sqlx::query("SELECT provider_attempt_id,manifest,recording_key FROM evidence_imports WHERE interview_id=$1").bind(id).fetch_all(&s.pool).await?;
    for row in rows {
        let original: Value = row.get("manifest");
        let manifest: Manifest = serde_json::from_value(original.clone()).map_err(|_| invalid())?;
        if !manifest
            .segments
            .iter()
            .any(|s| s.source_id == source && s.speaker == "customer")
        {
            continue;
        }
        let audio = storage()
            .await?
            .read(&row.get::<String, _>("recording_key"))
            .await
            .map_err(|_| invalid())?;
        let clip = validator()
            .preview_range(&audio, &manifest, source, range)
            .await
            .map_err(|_| invalid())?;
        return Ok(Prepared {
            clip,
            manifest,
            original,
            attempt: row.get("provider_attempt_id"),
        });
    }
    Err(ApiError::unauthorized())
}
async fn preview_inner(
    s: &AppState,
    id: Uuid,
    source: &str,
    h: &HeaderMap,
    q: &RangeQuery,
) -> Result<Prepared, ApiError> {
    let token = review::token(s, h, id, true).await?;
    let before = workflow::inspect(&s.pool, &token, id).await?;
    if before.revisions.evidence != q.evidence_revision {
        return Err(ApiError::conflict());
    }
    let prepared = prepare(s, id, source, [q.start_ms, q.end_ms]).await?;
    let after = workflow::inspect(&s.pool, &token, id).await?;
    if before.revisions != after.revisions {
        return Err(ApiError::conflict());
    }
    Ok(prepared)
}
pub async fn preview(
    State(s): State<AppState>,
    Path((id, source)): Path<(Uuid, String)>,
    h: HeaderMap,
    Query(q): Query<RangeQuery>,
) -> Result<Json<Value>, ApiError> {
    let p = preview_inner(&s, id, &source, &h, &q).await?;
    let seg = p
        .manifest
        .segments
        .iter()
        .find(|s| s.source_id == source)
        .ok_or_else(invalid)?;
    Ok(Json(
        json!({"recording_sha256":p.manifest.recording_sha256,"timeline_sha256":p.manifest.timeline_sha256,"source_id":source,"source_text_sha256":digest(seg.text.as_bytes()),"source_range_ms":[q.start_ms,q.end_ms],"clip_sha256":digest(&p.clip.wav),"listened":false,"transcript_matches":false,"complete_answer":false}),
    ))
}
pub async fn preview_audio(
    State(s): State<AppState>,
    Path((id, source)): Path<(Uuid, String)>,
    h: HeaderMap,
    Query(q): Query<RangeQuery>,
) -> Result<Response, ApiError> {
    let p = preview_inner(&s, id, &source, &h, &q).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "audio/wav"),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        p.clip.wav,
    )
        .into_response())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Confirm {
    expected: Revisions,
    confirmation: OperatorAlignmentConfirmation,
}
pub async fn confirm(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
    Json(input): Json<Confirm>,
) -> Result<Json<WorkflowView>, ApiError> {
    let token = review::token(&s, &h, id, true).await?;
    let before = workflow::inspect(&s.pool, &token, id).await?;
    if before.revisions != input.expected {
        return Err(ApiError::conflict());
    }
    let mut p = prepare(
        &s,
        id,
        &input.confirmation.source_id,
        input.confirmation.source_range_ms,
    )
    .await?;
    confirm_operator_alignment(
        &mut p.manifest,
        &p.clip,
        &input.confirmation,
        &auth::hash_secret(&token),
        chrono::Utc::now(),
    )
    .map_err(alignment_error)?;
    let clip_id = Uuid::new_v4();
    let key = format!("clip-{clip_id}.wav");
    // Crash-safe orphan cleanup is scheduled before object I/O. Referenced clips
    // survive cleanup; failed/stale/deleted writes cannot leave untracked bytes.
    sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key,available_at) VALUES($1,$2,'cleanup_objects',$3,$4,clock_timestamp()+interval '10 minutes')")
        .bind(Uuid::new_v4()).bind(id).bind(json!({"keys":[key]})).bind(format!("clip-cleanup:{clip_id}")).execute(&s.pool).await?;
    storage()
        .await?
        .put(&key, &p.clip.wav)
        .await
        .map_err(|_| invalid())?;
    let mut updated = serde_json::to_value(&p.manifest).map_err(|_| invalid())?;
    for field in ["timeline_key", "metadata_key"] {
        if let Some(v) = p.original.get(field) {
            updated[field] = v.clone();
        }
    }
    let result = workflow::attach_verified_clip(
        &s.pool,
        &token,
        id,
        &input.expected,
        p.attempt,
        &p.original,
        updated,
        clip_id,
        &p.clip.source_id,
        &digest(&p.clip.wav),
        &key,
    )
    .await?;
    Ok(Json(result))
}
async fn read_clip(
    s: &AppState,
    id: Uuid,
    clip: Uuid,
    hash: &str,
    revision: i64,
) -> Result<Vec<u8>, ApiError> {
    let key:Option<String>=sqlx::query_scalar("SELECT object_key FROM workflow_clips WHERE id=$1 AND interview_id=$2 AND sha256=$3 AND evidence_revision=$4 AND ready")
        .bind(clip).bind(id).bind(hash).bind(revision).fetch_optional(&s.pool).await?;
    let bytes = storage()
        .await?
        .read(&key.ok_or_else(ApiError::unauthorized)?)
        .await
        .map_err(|_| invalid())?;
    if digest(&bytes) != hash {
        return Err(invalid());
    }
    Ok(bytes)
}
async fn private_audio(
    s: AppState,
    id: Uuid,
    clip: Uuid,
    h: HeaderMap,
    operator: bool,
) -> Result<Response, ApiError> {
    // This handler is used by both explicit role routes; never infer a role from
    // an arbitrary cookie when a valid customer session belongs to another interview.
    let token = review::token(&s, &h, id, operator).await?;
    let before = workflow::inspect(&s.pool, &token, id).await?;
    let hash:Option<String>=sqlx::query_scalar("SELECT sha256 FROM workflow_clips WHERE id=$1 AND interview_id=$2 AND evidence_revision=$3 AND ready")
        .bind(clip).bind(id).bind(before.revisions.evidence).fetch_optional(&s.pool).await?;
    let bytes = read_clip(
        &s,
        id,
        clip,
        &hash.ok_or_else(ApiError::unauthorized)?,
        before.revisions.evidence,
    )
    .await?;
    let after = workflow::inspect(&s.pool, &token, id).await?;
    if after.revisions != before.revisions {
        return Err(ApiError::conflict());
    }
    Ok((
        [
            (header::CONTENT_TYPE, "audio/wav"),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        bytes,
    )
        .into_response())
}
pub async fn public_audio(
    State(s): State<AppState>,
    Path((id, clip)): Path<(Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    let unavailable = || {
        ApiError(
            StatusCode::NOT_FOUND,
            "unavailable",
            "This audio is unavailable.",
        )
    };
    let before = workflow::published(&s.pool, id)
        .await
        .map_err(|_| unavailable())?;
    let selected = before
        .content
        .clips
        .iter()
        .find(|c| c.id == clip)
        .ok_or_else(unavailable)?;
    let bytes = read_clip(&s, id, clip, &selected.sha256, before.evidence_revision)
        .await
        .map_err(|_| unavailable())?;
    let after = workflow::published(&s.pool, id)
        .await
        .map_err(|_| unavailable())?;
    if before.id != after.id {
        return Err(unavailable());
    }
    // Range headers never grant source access: this is already a separate WAV
    // containing only the exact approved contiguous answer.
    Ok((
        [
            (header::CONTENT_TYPE, "audio/wav"),
            (header::CACHE_CONTROL, "no-store"),
            (
                header::HeaderName::from_static("x-robots-tag"),
                "noindex, nofollow",
            ),
        ],
        bytes,
    )
        .into_response())
}

pub async fn customer_audio(
    State(s): State<AppState>,
    Path((id, clip)): Path<(Uuid, Uuid)>,
    h: HeaderMap,
) -> Result<Response, ApiError> {
    private_audio(s, id, clip, h, false).await
}
pub async fn operator_audio(
    State(s): State<AppState>,
    Path((id, clip)): Path<(Uuid, Uuid)>,
    h: HeaderMap,
) -> Result<Response, ApiError> {
    private_audio(s, id, clip, h, true).await
}
