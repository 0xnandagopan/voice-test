//! Private operator background and durable pre-interview question preparation.
//! Neither attachments nor generated questions enter recorded-testimony evidence.
use crate::{AppState, auth, composition_jobs::JobFailure, error::ApiError};
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow};
use uuid::Uuid;
use v0_composition::{ContextAttachment, GatewayClient, InterviewQuestions};
use v0_evidence::jobs::Job;
use v0_voice::pre_speech::{QuestionCode, QuestionPlan};

pub fn validate_attachments(files: &[ContextAttachment]) -> Result<(), ApiError> {
    if files.len() > 5 {
        return Err(ApiError::invalid("Attach at most five files."));
    }
    let mut total = 0;
    let mut names = std::collections::HashSet::new();
    for file in files {
        let lower = file.name.to_lowercase();
        let extension = lower.rsplit('.').next().unwrap_or("");
        if file.name.is_empty()
            || file.name.len() > 160
            || file.name.trim() != file.name
            || file.name.starts_with('.')
            || file.name.contains(['/', '\\'])
            || file.name.chars().any(char::is_control)
            || !matches!(extension, "txt" | "md" | "json")
            || !names.insert(lower.clone())
        {
            return Err(ApiError::invalid(
                "Use unique .txt, .json or .md filenames without directory paths.",
            ));
        }
        total += file.content.len();
        if file.content.trim().is_empty()
            || file.content.len() > 32 * 1024
            || total > 96 * 1024
            || file
                .content
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return Err(ApiError::invalid(
                "Files must contain readable text, up to 32 KB each and 96 KB total.",
            ));
        }
        if extension == "json" && serde_json::from_str::<Value>(&file.content).is_err() {
            return Err(ApiError::invalid(
                "A .json attachment contains invalid JSON.",
            ));
        }
    }
    Ok(())
}
pub fn context_hash(context: &str, files: &[ContextAttachment]) -> String {
    auth::hash_secret(&json!([context, files]).to_string())
}
pub async fn enqueue(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    hash: &str,
) -> Result<(), ApiError> {
    sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key,max_attempts) VALUES($1,$2,'prepare_interview',$3,$4,3) ON CONFLICT(dedupe_key) DO NOTHING")
        .bind(Uuid::new_v4()).bind(id).bind(json!({"context_hash":hash})).bind(format!("interview-context-v1:{id}:{hash}")).execute(&mut **tx).await?;
    Ok(())
}
fn available(row: &PgRow) -> Result<(), ApiError> {
    if row.get::<Option<DateTime<Utc>>, _>("deleted_at").is_some()
        || row.get::<DateTime<Utc>, _>("expires_at") <= Utc::now()
        || matches!(
            row.get::<String, _>("state").as_str(),
            "deleted" | "revoked"
        )
    {
        return Err(ApiError::expired());
    }
    Ok(())
}
pub async fn retry(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    h: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    auth::operator(&s, &h).await?;
    let mut tx = s.pool.begin().await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    available(&row)?;
    // Recheck on this transaction after the row-lock wait; do not reserve a
    // second pool connection while holding the interview lock.
    let token = auth::cookie(&h, "operator_session").ok_or_else(ApiError::unauthorized)?;
    let authorized: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE token_hash=$1 AND role='operator' AND expires_at>clock_timestamp())")
        .bind(auth::hash_secret(&token)).fetch_one(&mut *tx).await?;
    if !authorized {
        return Err(ApiError::unauthorized());
    }
    let started: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM provider_attempts WHERE interview_id=$1)")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if started {
        return Err(ApiError::invalid(
            "Interview questions are fixed once the interview has started.",
        ));
    }
    let status: String = row.get("interview_preparation");
    if matches!(status.as_str(), "ready" | "queued" | "running") {
        return Ok(Json(json!({"status":status})));
    }
    let files: Vec<ContextAttachment> =
        serde_json::from_value(row.get("context_attachments")).map_err(|_| ApiError::conflict())?;
    validate_attachments(&files)?;
    let hash = context_hash(&row.get::<String, _>("project_context"), &files);
    enqueue(&mut tx, id, &hash).await?;
    sqlx::query("UPDATE jobs SET status='queued',attempts=0,available_at=clock_timestamp(),last_error=NULL,lease_token=NULL,lease_until=NULL WHERE dedupe_key=$1 AND status='failed'").bind(format!("interview-context-v1:{id}:{hash}")).execute(&mut *tx).await?;
    sqlx::query("UPDATE interviews SET interview_preparation='queued',context_hash=$2 WHERE id=$1")
        .bind(id)
        .bind(hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"status":"queued"})))
}
pub async fn reconcile(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE interviews i SET interview_preparation='failed' WHERE i.interview_preparation IN ('queued','running') AND EXISTS(SELECT 1 FROM jobs j WHERE j.interview_id=i.id AND j.kind='prepare_interview' AND j.payload->>'context_hash'=i.context_hash AND j.status='failed')").execute(pool).await?;
    Ok(())
}
pub async fn dispatch(pool: &PgPool, job: &Job, client: &GatewayClient) -> Result<(), JobFailure> {
    let hash = job.payload["context_hash"]
        .as_str()
        .ok_or_else(JobFailure::stale)?;
    let row=sqlx::query("UPDATE interviews i SET interview_preparation='running' WHERE i.id=$1 AND i.context_hash=$2 AND i.interview_preparation IN ('queued','running') AND i.deleted_at IS NULL AND i.state NOT IN ('revoked','deleted') AND i.expires_at>clock_timestamp() AND NOT EXISTS(SELECT 1 FROM provider_attempts p WHERE p.interview_id=i.id) AND EXISTS(SELECT 1 FROM jobs j WHERE j.id=$3 AND j.interview_id=i.id AND j.kind='prepare_interview' AND j.status='running' AND j.lease_token=$4 AND j.lease_until>clock_timestamp()) RETURNING i.project_context,i.context_attachments")
        .bind(job.interview_id).bind(hash).bind(job.id).bind(job.token).fetch_optional(pool).await.map_err(|_|storage())?.ok_or_else(JobFailure::stale)?;
    let context: String = row.get("project_context");
    let files: Vec<ContextAttachment> =
        serde_json::from_value(row.get("context_attachments")).map_err(|_| JobFailure::stale())?;
    validate_attachments(&files).map_err(|_| JobFailure::stale())?;
    if context_hash(&context, &files) != hash {
        return Err(JobFailure::stale());
    }
    let questions = client
        .prepare_interview(&context, &files)
        .await
        .map_err(JobFailure::gateway)?;
    questions.validate().map_err(JobFailure::gateway)?;
    let mut tx = pool.begin().await.map_err(|_| storage())?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(job.interview_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| storage())?
        .ok_or_else(JobFailure::stale)?;
    available(&row).map_err(|_| JobFailure::stale())?;
    if row.get::<String, _>("context_hash") != hash
        || row.get::<String, _>("interview_preparation") != "running"
    {
        return Err(JobFailure::stale());
    }
    let changed=sqlx::query("UPDATE jobs SET status='succeeded',lease_token=NULL,lease_until=NULL,last_error=NULL WHERE id=$1 AND interview_id=$2 AND kind='prepare_interview' AND status='running' AND lease_token=$3 AND lease_until>clock_timestamp()")
        .bind(job.id).bind(job.interview_id).bind(job.token).execute(&mut *tx).await.map_err(|_|storage())?.rows_affected();
    if changed != 1 {
        return Err(JobFailure::stale());
    }
    let changed=sqlx::query("UPDATE interviews SET interview_questions=$2,interview_preparation='ready' WHERE id=$1 AND deleted_at IS NULL AND state NOT IN ('revoked','deleted') AND expires_at>clock_timestamp() AND NOT EXISTS(SELECT 1 FROM provider_attempts WHERE interview_id=$1)")
        .bind(job.interview_id).bind(json!(questions)).execute(&mut *tx).await.map_err(|_|storage())?.rows_affected();
    if changed != 1 {
        return Err(JobFailure::stale());
    }
    tx.commit().await.map_err(|_| storage())?;
    Ok(())
}
fn storage() -> JobFailure {
    JobFailure {
        code: "interview_preparation_storage",
        terminal: false,
        retry_after_secs: 15,
    }
}
/// Only stored validated questions can enter a permit; uploads/model messages
/// cannot change progression, counters, completion text or approval authority.
pub fn contextualize(row: &PgRow, plan: QuestionPlan) -> Result<QuestionPlan, ApiError> {
    if plan.code == QuestionCode::Complete {
        return Ok(plan);
    }
    match row.get::<String, _>("interview_preparation").as_str() {
        "not_required" => Ok(plan),
        "ready" => {
            let bank: InterviewQuestions =
                serde_json::from_value(row.get::<Value, _>("interview_questions"))
                    .map_err(|_| ApiError::conflict())?;
            bank.validate().map_err(|_| ApiError::conflict())?;
            let key = serde_json::to_value(plan.code).map_err(|_| ApiError::conflict())?;
            let text = bank
                .questions
                .get(key.as_str().ok_or_else(ApiError::conflict)?)
                .ok_or_else(ApiError::conflict)?
                .clone();
            plan.with_contextual_text(text)
                .map_err(|_| ApiError::conflict())
        }
        _ => Err(ApiError::invalid(
            "The project-specific interview is not ready yet.",
        )),
    }
}
