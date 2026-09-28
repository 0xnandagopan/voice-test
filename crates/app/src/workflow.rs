//! Transactional authority seam for later review/publication routes.
//! No HTTP routes expose worker results or manufacture approval eligibility.
use crate::{auth::hash_secret, error::ApiError};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow};
use uuid::Uuid;
use v0_domain::workflow::*;

type Tx<'a> = Transaction<'a, Postgres>;
fn encode<T: serde::Serialize>(value: &T) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|_| ApiError::invalid("Invalid workflow data."))
}
async fn lock<'a>(pool: &'a PgPool, id: Uuid) -> Result<Tx<'a>, ApiError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    available(&row)?;
    sqlx::query(
        "INSERT INTO workflow_state(interview_id,value) VALUES($1,$2) ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(encode(&WorkflowView::default())?)
    .execute(&mut *tx)
    .await?;
    Ok(tx)
}
fn available(row: &PgRow) -> Result<(), ApiError> {
    if row.get::<DateTime<Utc>, _>("expires_at") <= Utc::now()
        || row.get::<Option<DateTime<Utc>>, _>("deleted_at").is_some()
        || matches!(
            row.get::<String, _>("state").as_str(),
            "revoked" | "deleted"
        )
    {
        return Err(ApiError::expired());
    }
    Ok(())
}
async fn load(tx: &mut Tx<'_>, id: Uuid) -> Result<WorkflowView, ApiError> {
    let v: Value = sqlx::query_scalar("SELECT value FROM workflow_state WHERE interview_id=$1")
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
    serde_json::from_value(v).map_err(|_| ApiError::invalid("Invalid stored workflow."))
}
async fn store(tx: &mut Tx<'_>, id: Uuid, state: &WorkflowView) -> Result<(), ApiError> {
    current_access(tx, id).await?;
    sqlx::query("UPDATE workflow_state SET value=$2 WHERE interview_id=$1")
        .bind(id)
        .bind(encode(state)?)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn current_access(tx: &mut Tx<'_>, id: Uuid) -> Result<(), ApiError> {
    let row = sqlx::query("SELECT expires_at,deleted_at,state FROM interviews WHERE id=$1")
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
    available(&row)
}
fn invalidate(state: &mut WorkflowView) {
    state.check = CheckStatus::Pending;
    state.approval = None;
    state.published_approval_id = None;
}
/// Refresh the interview-wide evidence version under the same lock as approval.
/// A caller cannot supply source IDs or an availability flag.
async fn evidence(tx: &mut Tx<'_>, id: Uuid, state: &mut WorkflowView) -> Result<(), ApiError> {
    let manifests: Vec<Value> = sqlx::query_scalar(
        "SELECT manifest FROM evidence_imports WHERE interview_id=$1 ORDER BY provider_attempt_id",
    )
    .bind(id)
    .fetch_all(&mut **tx)
    .await?;
    let fingerprint = hash_secret(&encode(&manifests)?.to_string());
    let old: String =
        sqlx::query_scalar("SELECT evidence_fingerprint FROM workflow_state WHERE interview_id=$1")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
    if old == fingerprint {
        return Ok(());
    }
    let mut source_ids = vec![];
    let mut ready = !manifests.is_empty();
    for manifest in &manifests {
        ready &= manifest["approval_eligible"] == true;
        let Some(segments) = manifest["segments"].as_array() else {
            ready = false;
            continue;
        };
        for source in segments.iter().filter(|s| s["speaker"] == "customer") {
            if let Some(id) = source["source_id"].as_str().filter(|s| !s.is_empty()) {
                source_ids.push(id.to_owned());
            }
            if let (Some(id), Some(range)) = (
                source["source_id"].as_str(),
                source["source_range_ms"].as_array(),
            ) {
                if !id.is_empty()
                    && range.len() == 2
                    && range[0]
                        .as_u64()
                        .zip(range[1].as_u64())
                        .is_some_and(|(a, b)| {
                            a < b && b <= manifest["duration_ms"].as_u64().unwrap_or(0)
                        })
                {
                    // Availability additionally requires a validated range.
                } else {
                    ready = false;
                }
            } else {
                ready = false;
            }
        }
    }
    ready &= !source_ids.is_empty();
    if !old.is_empty() || !manifests.is_empty() {
        state.revisions.evidence += 1;
        state.revisions.workflow += 1;
    }
    state.evidence_available = ready;
    invalidate(state);
    sqlx::query(
        "UPDATE workflow_state SET evidence_fingerprint=$2,source_ids=$3 WHERE interview_id=$1",
    )
    .bind(id)
    .bind(fingerprint)
    .bind(json!(source_ids))
    .execute(&mut **tx)
    .await?;
    store(tx, id, state).await?;
    if state.content.is_some() {
        enqueue_check(tx, id, state).await?;
    }
    Ok(())
}
async fn actor(tx: &mut Tx<'_>, id: Uuid, token: &str) -> Result<String, ApiError> {
    let row = sqlx::query("SELECT role,interview_id,expires_at FROM sessions WHERE token_hash=$1 AND expires_at>clock_timestamp() FOR SHARE")
        .bind(hash_secret(token)).fetch_optional(&mut **tx).await?.ok_or_else(ApiError::unauthorized)?;
    let role: String = row.get("role");
    if row.get::<DateTime<Utc>, _>("expires_at") <= Utc::now() {
        return Err(ApiError::unauthorized());
    }
    if role != "operator" && row.get::<Option<Uuid>, _>("interview_id") != Some(id) {
        return Err(ApiError::unauthorized());
    }
    Ok(role)
}
async fn eligible(tx: &mut Tx<'_>, id: Uuid, state: &WorkflowView) -> Result<(), ApiError> {
    if !state.evidence_available || state.check != CheckStatus::Supported || state.declined {
        return Err(ApiError::invalid(
            "Current evidence and complete support checks are required.",
        ));
    }
    let content = state
        .content
        .as_ref()
        .ok_or_else(|| ApiError::invalid("No content to approve."))?;
    for clip in &content.clips {
        let ok: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_clips WHERE id=$1 AND interview_id=$2 AND evidence_revision=$3 AND sha256=$4 AND ready AND source_id IN (SELECT jsonb_array_elements_text(source_ids) FROM workflow_state WHERE interview_id=$2))")
            .bind(clip.id).bind(id).bind(state.revisions.evidence).bind(&clip.sha256).fetch_one(&mut **tx).await?;
        if !ok {
            return Err(ApiError::invalid(
                "Selected clips must match ready evidence.",
            ));
        }
    }
    Ok(())
}
fn validate_content(content: &Content) -> Result<(), ApiError> {
    if content.text.trim().is_empty()
        || content.text.len() > 20000
        || content.attribution.trim().is_empty()
        || content.attribution.len() > 1000
        || content.clips.len() > 12
    {
        return Err(ApiError::invalid(
            "Content, attribution or clip selection is invalid.",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    if content.clips.iter().any(|c| {
        !seen.insert(c.id)
            || c.sha256.len() != 64
            || !c.sha256.bytes().all(|b| b.is_ascii_hexdigit())
    }) {
        return Err(ApiError::invalid(
            "Clip selections must be unique immutable identities.",
        ));
    }
    Ok(())
}
/// Authenticates the current session again after acquiring the interview lock.
/// HTTP adapters must also apply origin/CSRF checks and never accept the actor role.
pub async fn execute(
    pool: &PgPool,
    token: &str,
    command: WorkflowCommand,
) -> Result<CommandResult, ApiError> {
    let id = command.interview_id;
    let mut tx = lock(pool, id).await?;
    let role = actor(&mut tx, id, token).await?;
    current_access(&mut tx, id).await?;
    match &command.action {
        WorkflowAction::Approve | WorkflowAction::Decline if role != "customer" => {
            return Err(ApiError::unauthorized());
        }
        WorkflowAction::Publish { .. } | WorkflowAction::Unpublish if role != "operator" => {
            return Err(ApiError::unauthorized());
        }
        _ => {}
    }
    let mut state = load(&mut tx, id).await?;
    evidence(&mut tx, id, &mut state).await?;
    let request_hash = hash_secret(&encode(&command)?.to_string());
    let actor_hash = hash_secret(token);
    let receipt: Option<String>=sqlx::query_scalar("SELECT request_hash FROM workflow_receipts WHERE interview_id=$1 AND actor_hash=$2 AND request_id=$3")
        .bind(id).bind(&actor_hash).bind(command.request_id).fetch_optional(&mut *tx).await?;
    if let Some(old) = receipt {
        if old != request_hash {
            return Err(ApiError::conflict());
        }
        tx.commit().await?;
        return Ok(CommandResult {
            request_id: command.request_id,
            replayed: true,
            state,
        });
    }
    if state.revisions != command.expected {
        return Err(ApiError::conflict());
    }
    let mut changed = true;
    let recheck = matches!(
        &command.action,
        WorkflowAction::Save { .. } | WorkflowAction::CorrectTranscript { .. }
    );
    match command.action {
        WorkflowAction::Save { content } => {
            validate_content(&content)?;
            if state.content.as_ref() == Some(&content) {
                changed = false;
            } else {
                state.content = Some(content);
                state.revisions.content += 1;
                state.declined = false;
                invalidate(&mut state);
            }
        }
        WorkflowAction::Approve => {
            eligible(&mut tx, id, &state).await?;
            if state.approval.is_some() {
                changed = false;
            } else {
                state.approval = Some(ApprovalSnapshot {
                    id: Uuid::new_v4(),
                    content_revision: state.revisions.content,
                    evidence_revision: state.revisions.evidence,
                    content: state.content.clone().unwrap(),
                    approved_at: Utc::now(),
                });
            }
        }
        WorkflowAction::CorrectTranscript { source_id, text } => {
            let sources: Value =
                sqlx::query_scalar("SELECT source_ids FROM workflow_state WHERE interview_id=$1")
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await?;
            if text.trim().is_empty()
                || text.len() > 20000
                || !sources
                    .as_array()
                    .is_some_and(|s| s.contains(&json!(source_id)))
            {
                return Err(ApiError::invalid(
                    "Correction requires an existing customer source and text.",
                ));
            }
            if state.transcript_corrections.get(&source_id) == Some(&text) {
                changed = false;
            } else {
                state.transcript_corrections.insert(source_id, text);
                state.revisions.evidence += 1;
                invalidate(&mut state);
            }
        }
        WorkflowAction::Decline => {
            state.declined = true;
            state.approval = None;
            state.published_approval_id = None;
        }
        WorkflowAction::Publish { approval_id } => {
            eligible(&mut tx, id, &state).await?;
            let a = state.approval.as_ref().ok_or_else(ApiError::conflict)?;
            if a.id != approval_id
                || a.content_revision != state.revisions.content
                || a.evidence_revision != state.revisions.evidence
            {
                return Err(ApiError::conflict());
            }
            changed = state.published_approval_id != Some(approval_id);
            state.published_approval_id = Some(approval_id);
        }
        WorkflowAction::Unpublish => {
            changed = state.published_approval_id.take().is_some();
        }
    }
    if changed {
        state.revisions.workflow += 1;
    }
    store(&mut tx, id, &state).await?;
    if changed && recheck && state.content.is_some() {
        enqueue_check(&mut tx, id, &state).await?;
    }
    sqlx::query("INSERT INTO workflow_receipts(interview_id,actor_hash,request_id,request_hash) VALUES($1,$2,$3,$4)")
        .bind(id).bind(actor_hash).bind(command.request_id).bind(request_hash).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO audit_events(interview_id,event) VALUES($1,'workflow_command')")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(CommandResult {
        request_id: command.request_id,
        replayed: false,
        state,
    })
}
/// Current scoped state. Polling never extends inactivity.
pub async fn inspect(pool: &PgPool, token: &str, id: Uuid) -> Result<WorkflowView, ApiError> {
    let mut tx = lock(pool, id).await?;
    actor(&mut tx, id, token).await?;
    let mut state = load(&mut tx, id).await?;
    evidence(&mut tx, id, &mut state).await?;
    current_access(&mut tx, id).await?;
    tx.commit().await?;
    Ok(state)
}
/// Only a current leased support job can supply a semantic result. There is no
/// client-facing endpoint for this. Job payload binds the exact content/evidence.
pub async fn complete_support(
    pool: &PgPool,
    id: Uuid,
    job_id: Uuid,
    lease_token: Uuid,
    result: SupportResult,
) -> Result<(), ApiError> {
    complete_support_assessed(pool, id, job_id, lease_token, result, None).await
}
pub async fn complete_support_assessed(
    pool: &PgPool,
    id: Uuid,
    job_id: Uuid,
    lease_token: Uuid,
    result: SupportResult,
    assessment: Option<Value>,
) -> Result<(), ApiError> {
    let mut tx = lock(pool, id).await?;
    let mut state = load(&mut tx, id).await?;
    evidence(&mut tx, id, &mut state).await?;
    let job=sqlx::query("SELECT payload FROM jobs WHERE id=$1 AND interview_id=$2 AND kind='support_check' AND status='running' AND lease_token=$3 AND lease_until>clock_timestamp() FOR UPDATE")
        .bind(job_id).bind(id).bind(lease_token).fetch_optional(&mut *tx).await?.ok_or_else(ApiError::conflict)?;
    let payload: Value = job.get("payload");
    if !is_latest_composition_job(&mut tx, id, job_id, "support_check", &payload).await? {
        return Err(ApiError::conflict());
    }
    if result.content_revision != state.revisions.content
        || result.evidence_revision != state.revisions.evidence
        || payload["content_revision"] != result.content_revision
        || payload["evidence_revision"] != result.evidence_revision
        || payload["content_hash"] != hash_secret(&encode(&state.content)?.to_string())
    {
        return Err(ApiError::conflict());
    }
    if result.model.trim().is_empty() || result.prompt_version.trim().is_empty() {
        return Err(ApiError::invalid("Support provenance is required."));
    }
    let sources: Value =
        sqlx::query_scalar("SELECT source_ids FROM workflow_state WHERE interview_id=$1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if result.status == CheckStatus::Supported
        && (!state.evidence_available
            || !result.all_substantive_claims_checked
            || result.source_ids.is_empty()
            || result
                .source_ids
                .iter()
                .any(|id| !sources.as_array().is_some_and(|a| a.contains(&json!(id)))))
    {
        return Err(ApiError::invalid(
            "Support checks require complete coverage and existing source IDs.",
        ));
    }
    invalidate(&mut state);
    state.check = result.status;
    state.revisions.workflow += 1;
    store(&mut tx, id, &state).await?;
    let mut details = encode(&result)?;
    details["kind"] = json!("support_check");
    details["assessment"] = assessment.unwrap_or(Value::Null);
    sqlx::query("INSERT INTO workflow_support_results(interview_id,content_revision,evidence_revision,result) VALUES($1,$2,$3,$4) ON CONFLICT (interview_id,content_revision,evidence_revision) DO UPDATE SET result=EXCLUDED.result")
        .bind(id).bind(result.content_revision).bind(result.evidence_revision).bind(details).execute(&mut *tx).await?;
    let done=sqlx::query("UPDATE jobs SET status='succeeded',lease_token=NULL,lease_until=NULL WHERE id=$1 AND lease_token=$2 AND lease_until>clock_timestamp()")
        .bind(job_id).bind(lease_token).execute(&mut *tx).await?;
    if done.rows_affected() != 1 {
        return Err(ApiError::conflict());
    }
    current_access(&mut tx, id).await?;
    tx.commit().await?;
    Ok(())
}
/// Recheck every hosted read. The returned value deliberately contains no raw
/// manifest, storage key, transcript or provider identity.
pub async fn published(pool: &PgPool, id: Uuid) -> Result<ApprovalSnapshot, ApiError> {
    let mut tx = lock(pool, id).await?;
    let mut state = load(&mut tx, id).await?;
    evidence(&mut tx, id, &mut state).await?;
    eligible(&mut tx, id, &state).await?;
    let approval = state.approval.ok_or_else(ApiError::unauthorized)?;
    if state.published_approval_id != Some(approval.id)
        || approval.content_revision != state.revisions.content
        || approval.evidence_revision != state.revisions.evidence
    {
        return Err(ApiError::unauthorized());
    }
    current_access(&mut tx, id).await?;
    tx.commit().await?;
    Ok(approval)
}

async fn is_latest_composition_job(
    tx: &mut Tx<'_>,
    id: Uuid,
    job: Uuid,
    kind: &str,
    payload: &Value,
) -> Result<bool, ApiError> {
    let latest: Option<Uuid> = sqlx::query_scalar("SELECT id FROM jobs WHERE interview_id=$1 AND kind=$2 AND payload->'content_revision'=$3 AND payload->'evidence_revision'=$4 AND payload->'content_hash'=$5 AND status<>'cancelled' ORDER BY created_at DESC,id DESC LIMIT 1")
        .bind(id).bind(kind).bind(&payload["content_revision"]).bind(&payload["evidence_revision"]).bind(&payload["content_hash"]).fetch_optional(&mut **tx).await?;
    Ok(latest == Some(job))
}

async fn enqueue_check(tx: &mut Tx<'_>, id: Uuid, state: &WorkflowView) -> Result<Uuid, ApiError> {
    let key = format!(
        "support:{id}:{}:{}",
        state.revisions.content, state.revisions.evidence
    );
    let payload = json!({"content_revision":state.revisions.content,"evidence_revision":state.revisions.evidence,"content_hash":hash_secret(&encode(&state.content)?.to_string())});
    let job=sqlx::query_scalar("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key,max_attempts) VALUES($1,$2,'support_check',$3,$4,3) ON CONFLICT(dedupe_key) DO UPDATE SET dedupe_key=EXCLUDED.dedupe_key RETURNING id").bind(Uuid::new_v4()).bind(id).bind(payload).bind(key).fetch_one(&mut **tx).await?;
    Ok(job)
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationRequest {
    pub request_id: Uuid,
    pub expected: Revisions,
}
// Generation receipts retain the exact task ID even after its inputs become stale.
// Other commands compare the full receipt string, so request-ID reuse fails closed.
fn generation_receipt(expected: &Revisions, job: Uuid) -> Result<String, ApiError> {
    Ok(format!(
        "generation:v1:{}:{job}",
        hash_secret(&encode(expected)?.to_string())
    ))
}
fn generation_receipt_job(receipt: &str, expected: &Revisions) -> Result<Uuid, ApiError> {
    let prefix = format!(
        "generation:v1:{}:",
        hash_secret(&encode(expected)?.to_string())
    );
    receipt
        .strip_prefix(&prefix)
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(ApiError::conflict)
}
pub async fn request_generation(
    pool: &PgPool,
    token: &str,
    id: Uuid,
    request: GenerationRequest,
) -> Result<(Uuid, WorkflowView), ApiError> {
    let mut tx = lock(pool, id).await?;
    actor(&mut tx, id, token).await?;
    let mut state = load(&mut tx, id).await?;
    evidence(&mut tx, id, &mut state).await?;
    let existing_receipt: Option<String> = sqlx::query_scalar("SELECT request_hash FROM workflow_receipts WHERE interview_id=$1 AND actor_hash=$2 AND request_id=$3")
        .bind(id).bind(hash_secret(token)).bind(request.request_id).fetch_optional(&mut *tx).await?;
    if let Some(receipt) = existing_receipt {
        let job = generation_receipt_job(&receipt, &request.expected)?;
        tx.commit().await?;
        return Ok((job, state));
    }
    let key = format!(
        "generate:{id}:{}:{}",
        hash_secret(token),
        request.request_id
    );
    let existing = sqlx::query("SELECT id,payload FROM jobs WHERE dedupe_key=$1")
        .bind(&key)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some(row) = existing {
        let payload: Value = row.get("payload");
        if payload["expected"] != encode(&request.expected)? {
            return Err(ApiError::conflict());
        }
        tx.commit().await?;
        return Ok((row.get("id"), state));
    }
    if state.revisions != request.expected || state.approval.is_some() || state.declined {
        return Err(ApiError::conflict());
    }
    let imports: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM evidence_imports WHERE interview_id=$1)")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if !imports {
        return Err(ApiError::invalid(
            "A saved recording is required before drafting.",
        ));
    }
    // The interview lock serializes requests from different tabs and request IDs.
    // A failed current job is retried explicitly, never multiplied by Preview.
    let content_hash = hash_secret(&encode(&state.content)?.to_string());
    let existing_job: Option<Uuid> = sqlx::query_scalar("SELECT id FROM jobs WHERE interview_id=$1 AND kind='generate_draft' AND payload->'content_revision'=$2 AND payload->'evidence_revision'=$3 AND payload->>'content_hash'=$4 AND status<>'cancelled' ORDER BY created_at DESC,id DESC LIMIT 1")
        .bind(id).bind(json!(state.revisions.content)).bind(json!(state.revisions.evidence)).bind(&content_hash).fetch_optional(&mut *tx).await?;
    let job = if let Some(job) = existing_job {
        job
    } else {
        let job = Uuid::new_v4();
        let payload = json!({"expected":request.expected,"content_revision":state.revisions.content,"evidence_revision":state.revisions.evidence,"content_hash":content_hash});
        sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key,max_attempts) VALUES($1,$2,'generate_draft',$3,$4,3)").bind(job).bind(id).bind(payload).bind(key).execute(&mut *tx).await?;
        job
    };
    sqlx::query("INSERT INTO workflow_receipts(interview_id,actor_hash,request_id,request_hash) VALUES($1,$2,$3,$4)")
        .bind(id).bind(hash_secret(token)).bind(request.request_id).bind(generation_receipt(&request.expected, job)?).execute(&mut *tx).await?;
    current_access(&mut tx, id).await?;
    tx.commit().await?;
    Ok((job, state))
}
/// Trusted generation completion. A failed/stale worker cannot overwrite an edit.
pub async fn complete_generation(
    pool: &PgPool,
    id: Uuid,
    job: Uuid,
    lease: Uuid,
    text: Option<String>,
) -> Result<(), ApiError> {
    complete_generation_assessed(pool, id, job, lease, text, None).await
}
pub async fn complete_generation_assessed(
    pool: &PgPool,
    id: Uuid,
    job: Uuid,
    lease: Uuid,
    text: Option<String>,
    assessment: Option<Value>,
) -> Result<(), ApiError> {
    let mut tx = lock(pool, id).await?;
    let mut state = load(&mut tx, id).await?;
    evidence(&mut tx, id, &mut state).await?;
    let row=sqlx::query("SELECT payload FROM jobs WHERE id=$1 AND interview_id=$2 AND kind='generate_draft' AND status='running' AND lease_token=$3 FOR UPDATE").bind(job).bind(id).bind(lease).fetch_optional(&mut *tx).await?.ok_or_else(ApiError::conflict)?;
    let payload: Value = row.get("payload");
    if !is_latest_composition_job(&mut tx, id, job, "generate_draft", &payload).await? {
        return Err(ApiError::conflict());
    }
    if payload["content_revision"] != state.revisions.content
        || payload["evidence_revision"] != state.revisions.evidence
        || payload["content_hash"] != hash_secret(&encode(&state.content)?.to_string())
        || state.approval.is_some()
        || state.declined
    {
        return Err(ApiError::conflict());
    }
    let generated = text.is_some();
    if let Some(text) = text {
        let attribution: String =
            sqlx::query_scalar("SELECT customer_label FROM interviews WHERE id=$1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        let content = Content {
            text,
            attribution,
            clips: vec![],
        };
        validate_content(&content)?;
        state.content = Some(content);
        state.revisions.content += 1;
        state.revisions.workflow += 1;
        invalidate(&mut state);
        store(&mut tx, id, &state).await?;
        enqueue_check(&mut tx, id, &state).await?;
    }
    if let Some(details) = assessment {
        sqlx::query("INSERT INTO workflow_support_results(interview_id,content_revision,evidence_revision,result) VALUES($1,$2,$3,$4) ON CONFLICT(interview_id,content_revision,evidence_revision) DO UPDATE SET result=EXCLUDED.result")
            .bind(id).bind(state.revisions.content).bind(state.revisions.evidence).bind(details).execute(&mut *tx).await?;
    }
    let done=sqlx::query("UPDATE jobs SET status='succeeded',last_error=CASE WHEN $3 THEN NULL ELSE 'insufficient_evidence' END,lease_token=NULL,lease_until=NULL WHERE id=$1 AND lease_token=$2 AND lease_until>clock_timestamp()").bind(job).bind(lease).bind(generated).execute(&mut *tx).await?;
    if done.rows_affected() != 1 {
        return Err(ApiError::conflict());
    }
    current_access(&mut tx, id).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn retry_job(
    pool: &PgPool,
    token: &str,
    request: RetryRequest,
) -> Result<(Uuid, WorkflowView), ApiError> {
    let id = request.interview_id;
    let mut tx = lock(pool, id).await?;
    actor(&mut tx, id, token).await?;
    let mut state = load(&mut tx, id).await?;
    evidence(&mut tx, id, &mut state).await?;
    let payload_hash = hash_secret(&encode(&request)?.to_string());
    let existing:Option<String>=sqlx::query_scalar("SELECT request_hash FROM workflow_receipts WHERE interview_id=$1 AND actor_hash=$2 AND request_id=$3").bind(id).bind(hash_secret(token)).bind(request.request_id).fetch_optional(&mut *tx).await?;
    if let Some(h) = existing {
        if h != payload_hash {
            return Err(ApiError::conflict());
        }
        tx.commit().await?;
        return Ok((request.job_id, state));
    }
    if state.revisions != request.expected {
        return Err(ApiError::conflict());
    }
    let row = sqlx::query(
        "SELECT kind,payload,status FROM jobs WHERE id=$1 AND interview_id=$2 FOR UPDATE",
    )
    .bind(request.job_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(ApiError::unauthorized)?;
    let kind: String = row.get("kind");
    let payload: Value = row.get("payload");
    if row.get::<String, _>("status") != "failed"
        || !matches!(
            kind.as_str(),
            "import_evidence" | "support_check" | "generate_draft"
        )
    {
        return Err(ApiError::conflict());
    }
    if kind == "import_evidence" {
        let unresolved: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM provider_attempts p WHERE p.interview_id=$1 AND p.id::text=$2 AND NOT EXISTS(SELECT 1 FROM evidence_imports e WHERE e.interview_id=p.interview_id AND e.provider_attempt_id=p.id))")
            .bind(id).bind(payload["provider_attempt_id"].as_str()).fetch_one(&mut *tx).await?;
        if !unresolved {
            return Err(ApiError::conflict());
        }
    } else {
        if payload["content_revision"] != state.revisions.content
            || payload["evidence_revision"] != state.revisions.evidence
            || payload["content_hash"] != hash_secret(&encode(&state.content)?.to_string())
            || state.approval.is_some()
            || state.declined
        {
            return Err(ApiError::conflict());
        }
        if !is_latest_composition_job(&mut tx, id, request.job_id, &kind, &payload).await? {
            return Err(ApiError::conflict());
        }
    }
    sqlx::query("UPDATE jobs SET status='queued',max_attempts=attempts+3,available_at=clock_timestamp(),last_error=NULL,lease_token=NULL,lease_until=NULL WHERE id=$1").bind(request.job_id).execute(&mut *tx).await?;
    if kind == "support_check" {
        state.check = CheckStatus::Pending;
        state.revisions.workflow += 1;
        store(&mut tx, id, &state).await?;
    }
    sqlx::query("INSERT INTO workflow_receipts(interview_id,actor_hash,request_id,request_hash) VALUES($1,$2,$3,$4)").bind(id).bind(hash_secret(token)).bind(request.request_id).bind(payload_hash).execute(&mut *tx).await?;
    current_access(&mut tx, id).await?;
    tx.commit().await?;
    Ok((request.job_id, state))
}

/// Snapshot only current authorized job inputs. Original recording transcripts
/// remain the support source; edited transcript annotations never rewrite them.
pub async fn composition_input(
    pool: &PgPool,
    id: Uuid,
    job_id: Uuid,
    lease: Uuid,
) -> Result<(WorkflowView, Vec<v0_composition::EvidenceSource>), ApiError> {
    let mut tx = lock(pool, id).await?;
    let mut state = load(&mut tx, id).await?;
    evidence(&mut tx, id, &mut state).await?;
    let job=sqlx::query("SELECT payload,kind FROM jobs WHERE id=$1 AND interview_id=$2 AND kind IN ('support_check','generate_draft') AND status='running' AND lease_token=$3 AND lease_until>clock_timestamp() FOR UPDATE").bind(job_id).bind(id).bind(lease).fetch_optional(&mut *tx).await?.ok_or_else(ApiError::conflict)?;
    let payload: Value = job.get("payload");
    if !is_latest_composition_job(&mut tx, id, job_id, &job.get::<String, _>("kind"), &payload)
        .await?
    {
        return Err(ApiError::conflict());
    }
    if payload["content_revision"] != state.revisions.content
        || payload["evidence_revision"] != state.revisions.evidence
        || payload["content_hash"] != hash_secret(&encode(&state.content)?.to_string())
    {
        return Err(ApiError::conflict());
    }
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT manifest FROM evidence_imports WHERE interview_id=$1 ORDER BY provider_attempt_id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    let mut sources = vec![];
    for value in rows {
        let m: v0_evidence::manifest::Manifest =
            serde_json::from_value(value).map_err(|_| ApiError::conflict())?;
        for segment in &m.segments {
            let decoded = m.media.as_ref().is_some_and(|m| {
                m.ranges.iter().any(|r| {
                    r.source_id == segment.source_id && r.within_recording && r.audible_samples > 0
                })
            });
            if segment.speaker == "customer"
                && decoded
                && segment.turn_status == "completed"
                && !m.incomplete_turn_ids.contains(&segment.turn_id)
            {
                sources.push(v0_composition::EvidenceSource {
                    id: segment.source_id.clone(),
                    text: segment.text.clone(),
                });
            }
        }
    }
    current_access(&mut tx, id).await?;
    tx.commit().await?;
    Ok((state, sources))
}

/// Failure propagation shares the same authority lock and revision protocol as edits.
pub async fn fail_support_job(pool: &PgPool, id: Uuid, job_id: Uuid) -> Result<(), ApiError> {
    let mut tx = lock(pool, id).await?;
    let mut state = load(&mut tx, id).await?;
    evidence(&mut tx, id, &mut state).await?;
    let payload: Option<Value> = sqlx::query_scalar("SELECT payload FROM jobs WHERE id=$1 AND interview_id=$2 AND kind='support_check' AND status='failed' FOR UPDATE")
        .bind(job_id).bind(id).fetch_optional(&mut *tx).await?;
    let current = if let Some(p) = payload {
        p["content_revision"] == state.revisions.content
            && p["evidence_revision"] == state.revisions.evidence
            && p["content_hash"] == hash_secret(&encode(&state.content)?.to_string())
            && is_latest_composition_job(&mut tx, id, job_id, "support_check", &p).await?
    } else {
        false
    };
    if current && state.check == CheckStatus::Pending {
        state.check = CheckStatus::Failed;
        state.revisions.workflow += 1;
        store(&mut tx, id, &state).await?;
    }
    current_access(&mut tx, id).await?;
    tx.commit().await?;
    Ok(())
}

/// Reconcile final-attempt worker crashes through normal authority fencing.
pub async fn reconcile_failed_support(pool: &PgPool) -> Result<(), ApiError> {
    let rows = sqlx::query("SELECT j.id,j.interview_id FROM jobs j JOIN workflow_state w ON w.interview_id=j.interview_id JOIN interviews i ON i.id=j.interview_id WHERE j.kind='support_check' AND j.status='failed' AND w.value->>'check'='pending' AND j.payload->'content_revision'=w.value->'revisions'->'content' AND j.payload->'evidence_revision'=w.value->'revisions'->'evidence' AND i.deleted_at IS NULL AND i.state NOT IN ('revoked','deleted') AND i.expires_at>clock_timestamp() ORDER BY j.id LIMIT 30")
        .fetch_all(pool).await?;
    for row in rows {
        let _ = fail_support_job(pool, row.get("interview_id"), row.get("id")).await;
    }
    Ok(())
}
