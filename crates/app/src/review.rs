//! Scoped review, evidence playback and publication HTTP boundaries.
use crate::{AppState, auth, error::ApiError, workflow};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;
use v0_domain::workflow::{CommandResult, WorkflowCommand, WorkflowView};
use v0_evidence::{
    manifest::Manifest,
    storage::{LocalPrivateStorage, PrivateStorage},
};

pub(crate) async fn token(
    state: &AppState,
    headers: &HeaderMap,
    id: Uuid,
    operator: bool,
) -> Result<String, ApiError> {
    if operator {
        auth::operator(state, headers).await?;
    } else if auth::customer(state, headers).await? != id {
        return Err(ApiError::unauthorized());
    }
    auth::cookie(
        headers,
        if operator {
            "operator_session"
        } else {
            "customer_session"
        },
    )
    .ok_or_else(ApiError::unauthorized)
}
pub async fn customer_view(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
) -> Result<Json<WorkflowView>, ApiError> {
    let t = token(&s, &h, id, false).await?;
    Ok(Json(workflow::inspect(&s.pool, &t, id).await?))
}
pub async fn operator_view(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
) -> Result<Json<WorkflowView>, ApiError> {
    let t = token(&s, &h, id, true).await?;
    Ok(Json(workflow::inspect(&s.pool, &t, id).await?))
}
async fn command(
    s: AppState,
    id: Uuid,
    h: HeaderMap,
    c: WorkflowCommand,
    op: bool,
) -> Result<Json<CommandResult>, ApiError> {
    let t = token(&s, &h, id, op).await?;
    if c.interview_id != id {
        return Err(ApiError::unauthorized());
    }
    Ok(Json(workflow::execute(&s.pool, &t, c).await?))
}
pub async fn customer_command(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
    Json(c): Json<WorkflowCommand>,
) -> Result<Json<CommandResult>, ApiError> {
    command(s, id, h, c, false).await
}
pub async fn operator_command(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
    Json(c): Json<WorkflowCommand>,
) -> Result<Json<CommandResult>, ApiError> {
    command(s, id, h, c, true).await
}

async fn evidence(s: AppState, id: Uuid, h: HeaderMap, op: bool) -> Result<Json<Value>, ApiError> {
    let t = token(&s, &h, id, op).await?;
    let before = workflow::inspect(&s.pool, &t, id).await?;
    let rows=sqlx::query("SELECT provider_attempt_id,manifest FROM evidence_imports WHERE interview_id=$1 ORDER BY created_at,id").bind(id).fetch_all(&s.pool).await?;
    let mut sources = Vec::new();
    for row in rows {
        let m: Manifest =
            serde_json::from_value(row.get("manifest")).map_err(|_| ApiError::conflict())?;
        for seg in m.segments.iter().filter(|s| s.speaker == "customer") {
            let check = m
                .media
                .as_ref()
                .and_then(|m| m.ranges.iter().find(|r| r.source_id == seg.source_id));
            sources.push(json!({"source_id":seg.source_id,"attempt_id":row.get::<Uuid,_>("provider_attempt_id"),"text":seg.text,"corrected_text":before.transcript_corrections.get(&seg.source_id),"speaker":seg.speaker,"start_ms":seg.source_range_ms.map(|r|r[0]),"end_ms":seg.source_range_ms.map(|r|r[1]),"playback_available":check.is_some_and(|c|c.within_recording && c.audible_samples>0 && c.candidate_source_range_ms.is_some()),"alignment_verified":m.approval_eligible && m.media.as_ref().is_some_and(|m|m.alignment_proven)}));
        }
    }
    let jobs=sqlx::query("SELECT id,kind,status,last_error FROM jobs WHERE interview_id=$1 AND kind IN ('import_evidence','generate_draft','support_check') ORDER BY created_at DESC LIMIT 30").bind(id).fetch_all(&s.pool).await?.into_iter().map(|r|json!({"id":r.get::<Uuid,_>("id"),"kind":r.get::<String,_>("kind"),"status":r.get::<String,_>("status"),"error_code":r.get::<Option<String>,_>("last_error")})).collect::<Vec<_>>();
    let assessment:Option<Value> = sqlx::query_scalar("SELECT result || jsonb_build_object('content_revision',content_revision,'evidence_revision',evidence_revision) FROM workflow_support_results WHERE interview_id=$1 AND content_revision=$2 AND evidence_revision=$3 AND result->'assessment' IS NOT NULL AND result->'assessment'<>'null'::jsonb")
        .bind(id).bind(before.revisions.content).bind(before.revisions.evidence).fetch_optional(&s.pool).await?;
    let after = workflow::inspect(&s.pool, &t, id).await?;
    if before.revisions != after.revisions {
        return Err(ApiError::conflict());
    }
    Ok(Json(
        json!({"sources":sources,"jobs":jobs,"evidence_revision":after.revisions.evidence,"assessment":assessment}),
    ))
}
pub async fn customer_evidence(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    evidence(s, id, h, false).await
}
pub async fn operator_evidence(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    evidence(s, id, h, true).await
}
pub async fn recovery(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
) -> Result<Json<v0_evidence::recovery::RecoveryContext>, ApiError> {
    let t = token(&s, &h, id, false).await?;
    let result = v0_evidence::recovery::recover_interview(&s.pool, id)
        .await
        .map_err(|_| ApiError::conflict())?;
    workflow::inspect(&s.pool, &t, id).await?;
    Ok(Json(result))
}
async fn audio(
    s: AppState,
    id: Uuid,
    source: String,
    h: HeaderMap,
    op: bool,
) -> Result<Response, ApiError> {
    let t = token(&s, &h, id, op).await?;
    let before = workflow::inspect(&s.pool, &t, id).await?;
    if source.len() > 240 {
        return Err(ApiError::invalid("Invalid source."));
    }
    let rows =
        sqlx::query("SELECT manifest,recording_key FROM evidence_imports WHERE interview_id=$1")
            .bind(id)
            .fetch_all(&s.pool)
            .await?;
    let mut selected = None;
    for row in rows {
        let m: Manifest =
            serde_json::from_value(row.get("manifest")).map_err(|_| ApiError::conflict())?;
        if m.segments
            .iter()
            .any(|s| s.source_id == source && s.speaker == "customer")
        {
            selected = Some((m, row.get::<String, _>("recording_key")));
            break;
        }
    }
    let (manifest, key) = selected.ok_or_else(ApiError::unauthorized)?;
    let storage = LocalPrivateStorage::new(
        std::env::var("EVIDENCE_STORAGE_DIR").unwrap_or_else(|_| ".local/private-evidence".into()),
    )
    .await
    .map_err(|_| unavailable_media())?;
    let recording = storage.read(&key).await.map_err(|_| unavailable_media())?;
    let validator = v0_evidence::media::FfmpegValidator::new(
        std::env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".into()),
        std::env::var("FFPROBE_PATH").unwrap_or_else(|_| "ffprobe".into()),
        std::env::var("EVIDENCE_WORK_DIR").unwrap_or_else(|_| ".local/evidence-jobs".into()),
    );
    let clip = validator
        .candidate_clip(&recording, &manifest, &source)
        .await
        .map_err(|_| unavailable_media())?;
    let after = workflow::inspect(&s.pool, &t, id).await?;
    if before.revisions != after.revisions {
        return Err(ApiError::conflict());
    }
    Ok((
        [
            (header::CONTENT_TYPE, "audio/wav"),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        clip.wav,
    )
        .into_response())
}
fn unavailable_media() -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "media_unavailable",
        "This recording is not available yet.",
    )
}
pub async fn customer_audio(
    State(s): State<AppState>,
    Path((id, source)): Path<(Uuid, String)>,
    h: HeaderMap,
) -> Result<Response, ApiError> {
    audio(s, id, source, h, false).await
}
pub async fn operator_audio(
    State(s): State<AppState>,
    Path((id, source)): Path<(Uuid, String)>,
    h: HeaderMap,
) -> Result<Response, ApiError> {
    audio(s, id, source, h, true).await
}
pub async fn public_snapshot(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let a = workflow::published(&s.pool, id).await.map_err(|_| {
        ApiError(
            StatusCode::NOT_FOUND,
            "unavailable",
            "This testimonial is unavailable.",
        )
    })?;
    // Clip serving is a separate gated representation; never expose source keys.
    if !a.content.clips.is_empty() {
        return Err(unavailable_media());
    }
    Ok(([("x-robots-tag","noindex, nofollow"),("cache-control","no-store")],Json(json!({"text":a.content.text,"attribution":a.content.attribution,"clips":[],"approved_at":a.approved_at}))).into_response())
}
pub async fn export(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
) -> Result<Response, ApiError> {
    token(&s, &h, id, true).await?;
    let a = workflow::published(&s.pool, id).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename= testimonial.txt",
            ),
        ],
        format!("{}\n\n— {}\n", a.content.text, a.content.attribution),
    )
        .into_response())
}

pub async fn generate(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
    Json(r): Json<workflow::GenerationRequest>,
) -> Result<Json<Value>, ApiError> {
    let t = token(&s, &h, id, false).await?;
    let (job_id, state) = workflow::request_generation(&s.pool, &t, id, r).await?;
    Ok(Json(json!({"job_id":job_id,"state":state})))
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryBody {
    request_id: Uuid,
    expected: v0_domain::workflow::Revisions,
    job_id: Uuid,
}
pub async fn retry(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
    Json(r): Json<RetryBody>,
) -> Result<Json<Value>, ApiError> {
    let t = token(&s, &h, id, false).await?;
    let (job_id, state) = workflow::retry_job(
        &s.pool,
        &t,
        v0_domain::workflow::RetryRequest {
            interview_id: id,
            request_id: r.request_id,
            expected: r.expected,
            job_id: r.job_id,
        },
    )
    .await?;
    Ok(Json(json!({"job_id":job_id,"state":state})))
}
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmRecovery {
    request_id: Uuid,
    expected_revision: i64,
    evidence_revision: i64,
    acknowledge_incomplete: bool,
}
pub async fn confirm_recovery(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
    Json(r): Json<ConfirmRecovery>,
) -> Result<Json<Value>, ApiError> {
    let t = token(&s, &h, id, false).await?;
    let current = workflow::inspect(&s.pool, &t, id).await?;
    if current.revisions.evidence != r.evidence_revision {
        return Err(ApiError::conflict());
    }
    let context = v0_evidence::recovery::recover_interview(&s.pool, id)
        .await
        .map_err(|_| ApiError::conflict())?;
    if context
        .attempts
        .iter()
        .any(|a| a.status == "awaiting_recording_artifacts")
    {
        return Err(ApiError::invalid("Recording recovery is still pending."));
    }
    if !r.acknowledge_incomplete {
        return Err(ApiError::invalid(
            "Confirm recovered and incomplete answers before restarting.",
        ));
    }
    let mut tx = s.pool.begin().await?;
    let row=sqlx::query("SELECT revision,state,expires_at,deleted_at,lease_expires_at FROM interviews WHERE id=$1 FOR UPDATE").bind(id).fetch_one(&mut *tx).await?;
    let authorized:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE token_hash=$1 AND role='customer' AND interview_id=$2 AND expires_at>clock_timestamp())").bind(auth::hash_secret(&t)).bind(id).fetch_one(&mut *tx).await?;
    let fingerprint = auth::hash_secret(
        &serde_json::to_value(&r)
            .map_err(|_| ApiError::conflict())?
            .to_string(),
    );
    let old:Option<String>=sqlx::query_scalar("SELECT request_hash FROM recovery_confirmations WHERE interview_id=$1 AND request_id=$2 AND actor_hash=$3").bind(id).bind(r.request_id).bind(auth::hash_secret(&t)).fetch_optional(&mut *tx).await?;
    if !authorized
        || matches!(
            row.get::<String, _>("state").as_str(),
            "revoked" | "deleted"
        )
        || row
            .get::<Option<chrono::DateTime<chrono::Utc>>, _>("deleted_at")
            .is_some()
        || row.get::<chrono::DateTime<chrono::Utc>, _>("expires_at") <= chrono::Utc::now()
    {
        return Err(ApiError::expired());
    }
    if let Some(old) = old {
        if old != fingerprint {
            return Err(ApiError::conflict());
        }
        return Ok(Json(
            json!({"revision":row.get::<i64,_>("revision"),"confirmed":true}),
        ));
    }
    if row.get::<String, _>("state") != "recovering"
        || row.get::<i64, _>("revision") != r.expected_revision
        || row
            .get::<Option<chrono::DateTime<chrono::Utc>>, _>("lease_expires_at")
            .is_some_and(|v| v > chrono::Utc::now())
    {
        return Err(ApiError::conflict());
    }
    // Artifact imports lock the interview; compare their immutable manifests with
    // the workflow fingerprint while holding that same lock.
    let manifests: Vec<Value> = sqlx::query_scalar(
        "SELECT manifest FROM evidence_imports WHERE interview_id=$1 ORDER BY provider_attempt_id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    let fingerprint_now = auth::hash_secret(
        &serde_json::to_value(&manifests)
            .map_err(|_| ApiError::conflict())?
            .to_string(),
    );
    let workflow_row =
        sqlx::query("SELECT evidence_fingerprint,value FROM workflow_state WHERE interview_id=$1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if workflow_row.get::<String, _>("evidence_fingerprint") != fingerprint_now
        || workflow_row.get::<Value, _>("value")["revisions"]["evidence"] != r.evidence_revision
    {
        return Err(ApiError::conflict());
    }
    let pending: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM provider_attempts p LEFT JOIN evidence_imports e ON e.provider_attempt_id=p.id AND e.interview_id=p.interview_id LEFT JOIN jobs j ON j.dedupe_key='import:'||p.id::text AND j.interview_id=p.interview_id AND j.kind='import_evidence' AND j.payload->>'provider_attempt_id'=p.id::text WHERE p.interview_id=$1 AND e.id IS NULL AND NOT COALESCE((p.provider_session_id IS NULL AND p.state='ended') OR j.status IN ('failed','cancelled','succeeded') OR (j.attempts>=j.max_attempts AND (j.status='queued' OR (j.status='running' AND (j.lease_until IS NULL OR j.lease_until<=clock_timestamp())))),false))")
        .bind(id).fetch_one(&mut *tx).await?;
    if pending {
        return Err(ApiError::conflict());
    }
    let rev:i64=sqlx::query_scalar("UPDATE interviews SET state='consented',revision=revision+1,updated_at=clock_timestamp() WHERE id=$1 RETURNING revision").bind(id).fetch_one(&mut *tx).await?;
    sqlx::query("INSERT INTO recovery_confirmations(interview_id,request_id,actor_hash,request_hash,evidence_revision) VALUES($1,$2,$3,$4,$5)").bind(id).bind(r.request_id).bind(auth::hash_secret(&t)).bind(fingerprint).bind(r.evidence_revision).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"revision":rev,"confirmed":true})))
}
