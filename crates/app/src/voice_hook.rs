//! Fenced custom-LLM callback. Provider history is checked against trusted final
//! events; message boundaries never determine answer or follow-up counts.
use crate::{
    AppState,
    auth::{hash_secret, random_secret},
    error::ApiError,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, header},
    response::Response,
    routing::post,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::{PgPool, Row};
use uuid::Uuid;
use v0_voice::{
    controller::Progress,
    pre_speech::{CommittedQuestion, FollowupKind, QuestionPlan},
};

#[derive(Clone, Deserialize)]
pub struct CompletionRequest {
    pub messages: Vec<Message>,
    #[serde(default)]
    pub stream: bool,
}
#[derive(Clone, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

/// Called by trusted relay setup only. Never return this secret to a browser.
/// Rotating a credential does not reset the processed-history cursor.
pub async fn bind(
    pool: &PgPool,
    interview: Uuid,
    attempt: Uuid,
    lease: Uuid,
    generation: i64,
) -> Result<String, ApiError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(interview)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    let consumed = validate(&row, generation)?;
    if row.get::<Option<Uuid>, _>("lease_id") != Some(lease) {
        return Err(ApiError::conflict());
    }
    let bound=sqlx::query("UPDATE provider_attempts SET control_mode='custom' WHERE id=$1 AND interview_id=$2 AND lease_generation=$3 AND state IN ('connecting','active') AND ended_at IS NULL")
  .bind(attempt).bind(interview).bind(generation).execute(&mut *tx).await?;
    if bound.rows_affected() != 1 {
        return Err(ApiError::conflict());
    }
    let secret = random_secret();
    let existing: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM voice_hook_bindings WHERE attempt_id=$1)")
            .bind(attempt)
            .fetch_one(&mut *tx)
            .await?;
    if existing {
        sqlx::query("UPDATE voice_hook_bindings SET secret_hash=$2,binding_generation=$3 WHERE attempt_id=$1").bind(attempt).bind(hash_secret(&secret)).bind(generation).execute(&mut *tx).await?;
    } else {
        let progress = Progress {
            topic: row.get::<i32, _>("topic_index") as u8,
            followups: serde_json::from_value(row.get("followup_counts"))
                .map_err(|_| ApiError::conflict())?,
            consumed_millis: consumed,
        };
        // Reconstruct using the last exact authorized question. Recovered history
        // belongs in system context, never new user input/evidence.
        let previous:Option<serde_json::Value>=sqlx::query_scalar("SELECT q.plan FROM question_permits q JOIN provider_attempts p ON p.id=q.attempt_id WHERE p.interview_id=$1 ORDER BY q.progress_revision DESC LIMIT 1")
            .bind(interview).fetch_optional(&mut *tx).await?;
        let plan = if let Some(previous) = previous {
            let mut previous: QuestionPlan =
                serde_json::from_value(previous).map_err(|_| ApiError::conflict())?;
            if previous.progress.topic != progress.topic
                || previous.progress.followups != progress.followups
            {
                return Err(ApiError::conflict());
            }
            previous.progress = progress;
            previous
        } else {
            QuestionPlan::initial(progress).map_err(|_| ApiError::conflict())?
        };
        let permit = Uuid::new_v4();
        CommittedQuestion::after_commit(permit.to_string(), plan.clone())
            .map_err(|_| ApiError::conflict())?;
        let hash = hash_secret("[]");
        let revision = row.get::<i64, _>("progress_revision") + 1;
        sqlx::query("INSERT INTO question_permits(id,attempt_id,request_hash,progress_revision,plan) VALUES($1,$2,$3,$4,$5)").bind(permit).bind(attempt).bind(&hash).bind(revision).bind(serde_json::to_value(plan).map_err(|_|ApiError::conflict())?).execute(&mut *tx).await?;
        sqlx::query("UPDATE interviews SET progress_revision=$2 WHERE id=$1")
            .bind(interview)
            .bind(revision)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO voice_hook_bindings(attempt_id,secret_hash,binding_generation,processed_user_count,last_request_hash,last_permit_id) VALUES($1,$2,$3,0,$4,$5)")
            .bind(attempt).bind(hash_secret(&secret)).bind(generation).bind(hash).bind(permit).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(secret)
}
fn validate(row: &sqlx::postgres::PgRow, generation: i64) -> Result<u64, ApiError> {
    let now = Utc::now();
    if row.get::<i64, _>("lease_generation") != generation
        || row.get::<String, _>("state") != "interviewing"
        || row.get::<Option<DateTime<Utc>>, _>("deleted_at").is_some()
        || row.get::<DateTime<Utc>, _>("expires_at") <= now
        || row
            .get::<Option<DateTime<Utc>>, _>("lease_expires_at")
            .is_none_or(|t| t <= now)
        || row
            .get::<Option<DateTime<Utc>>, _>("consented_at")
            .is_none()
    {
        return Err(ApiError::conflict());
    }
    let elapsed = row
        .get::<Option<DateTime<Utc>>, _>("active_since")
        .map(|t| (now - t).num_milliseconds().max(0) as u64)
        .unwrap_or(0);
    let consumed = row.get::<i32, _>("time_consumed_seconds") as u64 * 1000 + elapsed;
    if consumed >= 360000 {
        return Err(ApiError::conflict());
    }
    Ok(consumed)
}
/// Only classification, never instructions or freeform generated output. This
/// intentionally simple candidate awaits live conversational-quality evaluation.
fn classify(answer: &str) -> (bool, FollowupKind) {
    let answer = answer.to_lowercase();
    if ["don't know", "do not know", "can't say", "cannot say"]
        .iter()
        .any(|s| answer.contains(s))
    {
        return (false, FollowupKind::Uncertainty);
    }
    if ["unsure", "not sure", "approximately", "maybe", "exact"]
        .iter()
        .any(|s| answer.contains(s))
    {
        return (true, FollowupKind::Uncertainty);
    }
    if [" but ", "however", "still difficult", "still hard"]
        .iter()
        .any(|s| answer.contains(s))
    {
        return (true, FollowupKind::MixedFeedback);
    }
    (answer.split_whitespace().count() < 35, FollowupKind::Detail)
}
async fn completion(
    State(state): State<AppState>,
    Path(attempt): Path<Uuid>,
    headers: HeaderMap,
    Json(input): Json<CompletionRequest>,
) -> Result<Response, ApiError> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|s| s.len() == 64)
        .ok_or_else(ApiError::unauthorized)?;
    // The provider's HTTP callback and WebSocket final event are independent
    // transports. Briefly allow the trusted relay to commit an already-arriving
    // final without treating callback text as authority or holding a DB lock.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(250);
    loop {
        match complete(&state.pool, attempt, token, input.clone()).await {
            Err(error)
                if error.0 == axum::http::StatusCode::CONFLICT
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            result => return result,
        }
    }
}
/// Validate auth/history, commit permit and counters, then emit only permit text.
/// Duplicate user history returns the same completion ID, including after restart.
pub async fn complete(
    pool: &PgPool,
    attempt: Uuid,
    token: &str,
    input: CompletionRequest,
) -> Result<Response, ApiError> {
    if input.messages.len() > 100
        || input.messages.iter().any(|m| {
            m.content.len() > 16000 || !matches!(m.role.as_str(), "system" | "user" | "assistant")
        })
    {
        return Err(ApiError::invalid("Unsupported conversation history."));
    }
    let users: Vec<&str> = input
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.trim())
        .collect();
    if users.iter().any(|s| s.is_empty()) {
        return Err(ApiError::invalid("Empty input is not an answer."));
    }
    let request_hash = hash_secret(
        &serde_json::to_string(&users).map_err(|_| ApiError::invalid("Invalid messages."))?,
    );
    let mut tx = pool.begin().await?;
    // Lookup identity without locking; all mutation paths subsequently take the
    // interview lock first, then recheck the binding and current generation.
    let id:Uuid=sqlx::query_scalar("SELECT p.interview_id FROM provider_attempts p JOIN voice_hook_bindings b ON b.attempt_id=p.id WHERE p.id=$1 AND b.secret_hash=$2")
  .bind(attempt).bind(hash_secret(token)).fetch_optional(&mut *tx).await?.ok_or_else(ApiError::unauthorized)?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let binding=sqlx::query("SELECT b.*,p.lease_generation FROM voice_hook_bindings b JOIN provider_attempts p ON p.id=b.attempt_id WHERE b.attempt_id=$1 AND b.secret_hash=$2 AND p.state='active' AND p.control_mode='custom' AND b.binding_generation=p.lease_generation")
  .bind(attempt).bind(hash_secret(token)).fetch_optional(&mut *tx).await?.ok_or_else(ApiError::unauthorized)?;
    let consumed = validate(&row, binding.get("lease_generation"))?;
    let finals = sqlx::query("SELECT event_id,question_permit_id,trusted_text,response_permit_id FROM voice_answer_permits WHERE attempt_id=$1 ORDER BY ordinal")
        .bind(attempt).fetch_all(&mut *tx).await?;
    let trusted: Vec<String> = finals
        .iter()
        .map(|f| {
            f.get::<Option<String>, _>("trusted_text")
                .ok_or_else(ApiError::conflict)
        })
        .collect::<Result<_, _>>()?;
    let trusted_refs: Vec<&str> = trusted.iter().map(String::as_str).collect();
    let matched =
        v0_voice::history::matched_prefix(&users, &trusted_refs).ok_or_else(ApiError::conflict)?;
    let prior_question: Uuid = binding
        .get::<Option<Uuid>, _>("last_permit_id")
        .ok_or_else(ApiError::conflict)?;
    let delivered: Option<Uuid> = binding.get("delivered_permit_id");
    let mut next = prior_question;
    if matched > 0 {
        let question: Uuid = finals[matched - 1].get("question_permit_id");
        let response: Option<Uuid> = finals
            .iter()
            .filter(|f| f.get::<Uuid, _>("question_permit_id") == question)
            .find_map(|f| f.get("response_permit_id"));
        if let Some(response) = response {
            // Retried or extended speech answering the same delivered question
            // reuses its successor, even when provider history was merged.
            if response != prior_question {
                let control: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM question_permits old JOIN question_permits control ON control.attempt_id=old.attempt_id AND control.progress_revision>old.progress_revision AND control.request_hash LIKE 'control:%' WHERE old.id=$1 AND old.attempt_id=$2)")
                    .bind(question).bind(attempt).fetch_one(&mut *tx).await?;
                // A committed customer Skip/Repeat can request a new rendering
                // of unchanged, already consumed history. Unconsumed delayed
                // speech still follows the delivered-question checks below.
                if !control {
                    return Err(ApiError::conflict());
                }
            }
        } else {
            // A control committed after this speech began supersedes its answer.
            // Retain the trusted words for provider history, without counting them
            // as an answer to the newly selected topic.
            let superseded: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM question_permits old JOIN question_permits control ON control.attempt_id=old.attempt_id AND control.progress_revision>old.progress_revision AND control.request_hash LIKE 'control:%' WHERE old.id=$1 AND old.attempt_id=$2)")
                .bind(question).bind(attempt).fetch_one(&mut *tx).await?;
            if !superseded {
                if delivered != Some(question) || question != prior_question {
                    return Err(ApiError::conflict());
                }
                let progress = Progress {
                    topic: row.get::<i32, _>("topic_index") as u8,
                    followups: serde_json::from_value(row.get("followup_counts"))
                        .map_err(|_| ApiError::conflict())?,
                    consumed_millis: consumed,
                };
                if progress.topic >= 3 {
                    return Err(ApiError::conflict());
                }
                // Classify trusted speech only. Callback system/assistant text and
                // presentation changes cannot influence which bounded prompt is used.
                let answer = finals
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| f.get::<Uuid, _>("question_permit_id") == question)
                    .map(|(i, _)| trusted[i].as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                let (follow, kind) = classify(&answer);
                let plan = QuestionPlan::after_answer(progress, follow, kind)
                    .map_err(|_| ApiError::conflict())?;
                next = Uuid::new_v4();
                let revision = row.get::<i64, _>("progress_revision") + 1;
                sqlx::query("INSERT INTO question_permits(id,attempt_id,request_hash,progress_revision,plan) VALUES($1,$2,$3,$4,$5)")
                .bind(next).bind(attempt).bind(&request_hash).bind(revision).bind(serde_json::to_value(&plan).map_err(|_|ApiError::conflict())?).execute(&mut *tx).await?;
                sqlx::query("UPDATE interviews SET progress_revision=$2,topic_index=$3,followup_counts=$4,completed_answers=completed_answers+1,incomplete_turn=NULL WHERE id=$1")
                .bind(id).bind(revision).bind(plan.progress.topic as i32).bind(serde_json::json!(plan.progress.followups)).execute(&mut *tx).await?;
                sqlx::query("UPDATE voice_hook_bindings SET last_permit_id=$2 WHERE attempt_id=$1")
                    .bind(attempt)
                    .bind(next)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        sqlx::query("UPDATE voice_answer_permits SET consumed=true,response_permit_id=COALESCE(response_permit_id,$3) WHERE attempt_id=$1 AND question_permit_id=$2")
            .bind(attempt).bind(question).bind(next).execute(&mut *tx).await?;
    } else if binding.get::<i32, _>("processed_user_count") > 0 {
        return Err(ApiError::conflict());
    }
    sqlx::query("UPDATE voice_hook_bindings SET processed_user_count=GREATEST(processed_user_count,$2),last_request_hash=$3 WHERE attempt_id=$1")
        .bind(attempt).bind(matched as i32).bind(request_hash).execute(&mut *tx).await?;
    let value: serde_json::Value =
        sqlx::query_scalar("SELECT plan FROM question_permits WHERE id=$1 AND attempt_id=$2")
            .bind(next)
            .bind(attempt)
            .fetch_one(&mut *tx)
            .await?;
    let mut plan: QuestionPlan = serde_json::from_value(value).map_err(|_| ApiError::conflict())?;
    plan.progress.consumed_millis = consumed;
    let permit = next;
    // Validate output before committing, but never emit it until commit succeeds.
    let output = CommittedQuestion::after_commit(permit.to_string(), plan)
        .map_err(|_| ApiError::conflict())?;
    validate(&row, binding.get("lease_generation"))?;
    tx.commit().await?;
    Ok(output.response(input.stream))
}
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/voice-hook/{attempt}/chat/completions", post(completion))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(state)
}

/// Trusted customer command seam. A relay issues reply.create only after this
/// commits. HTTP adapters must authenticate the customer cookie and check Origin.
/// Stop/finish use end_attempt; resume/reconstruction require artifact recovery.
pub async fn control_question(
    pool: &PgPool,
    token: &str,
    request: v0_domain::workflow::VoiceControlRequest,
) -> Result<Uuid, ApiError> {
    use v0_domain::workflow::VoiceAction;
    if !matches!(request.action, VoiceAction::Skip | VoiceAction::Repeat) {
        return Err(ApiError::invalid(
            "This command requires the session lifecycle controller.",
        ));
    }
    let mut tx = pool.begin().await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(request.interview_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    let consumed = validate(&row, request.lease_generation)?;
    if row.get::<Option<Uuid>, _>("lease_id") != Some(request.lease_id) {
        return Err(ApiError::conflict());
    }
    let authorized:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE token_hash=$1 AND role='customer' AND interview_id=$2 AND expires_at>clock_timestamp())").bind(hash_secret(token)).bind(request.interview_id).fetch_one(&mut *tx).await?;
    if !authorized {
        return Err(ApiError::unauthorized());
    }
    let binding=sqlx::query("SELECT b.* FROM voice_hook_bindings b JOIN provider_attempts p ON p.id=b.attempt_id WHERE p.interview_id=$1 AND p.lease_generation=$2 AND b.binding_generation=p.lease_generation AND p.state='active' AND p.control_mode='custom'")
   .bind(request.interview_id).bind(request.lease_generation).fetch_optional(&mut *tx).await?.ok_or_else(ApiError::conflict)?;
    let attempt: Uuid = binding.get("attempt_id");
    let hash = hash_secret(&serde_json::to_string(&request).map_err(|_| ApiError::conflict())?);
    let prior=sqlx::query("SELECT request_hash,permit_id FROM question_control_receipts WHERE attempt_id=$1 AND request_id=$2").bind(attempt).bind(request.request_id).fetch_optional(&mut *tx).await?;
    if let Some(prior) = prior {
        if prior.get::<String, _>("request_hash") != hash {
            return Err(ApiError::conflict());
        }
        // Return the receipt, not an instruction to play the historical question.
        return Ok(prior.get("permit_id"));
    }
    if row.get::<i64, _>("revision") != request.expected_revision
        || row.get::<i64, _>("progress_revision") != request.expected_progress_revision
    {
        return Err(ApiError::conflict());
    }
    let previous: serde_json::Value =
        sqlx::query_scalar("SELECT plan FROM question_permits WHERE id=$1 AND attempt_id=$2")
            .bind(binding.get::<Option<Uuid>, _>("last_permit_id"))
            .bind(attempt)
            .fetch_one(&mut *tx)
            .await?;
    let mut previous: QuestionPlan =
        serde_json::from_value(previous).map_err(|_| ApiError::conflict())?;
    previous.progress.consumed_millis = consumed;
    if previous.progress.topic >= 3 {
        return Err(ApiError::conflict());
    }
    let plan = match request.action {
        VoiceAction::Skip => {
            QuestionPlan::skip(previous.progress).map_err(|_| ApiError::conflict())?
        }
        VoiceAction::Repeat => previous,
        _ => unreachable!(),
    };
    let permit = Uuid::new_v4();
    let revision = row.get::<i64, _>("progress_revision") + 1;
    sqlx::query("INSERT INTO question_permits(id,attempt_id,request_hash,progress_revision,plan) VALUES($1,$2,$3,$4,$5)").bind(permit).bind(attempt).bind(format!("control:{}",request.request_id)).bind(revision).bind(serde_json::to_value(&plan).map_err(|_|ApiError::conflict())?).execute(&mut *tx).await?;
    sqlx::query("UPDATE interviews SET progress_revision=$2,topic_index=$3,followup_counts=$4,incomplete_turn=NULL WHERE id=$1").bind(request.interview_id).bind(revision).bind(plan.progress.topic as i32).bind(serde_json::json!(plan.progress.followups)).execute(&mut *tx).await?;
    sqlx::query("UPDATE voice_hook_bindings SET last_permit_id=$2,delivered_permit_id=NULL WHERE attempt_id=$1")
        .bind(attempt)
        .bind(permit)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO question_control_receipts(attempt_id,request_id,request_hash,permit_id) VALUES($1,$2,$3,$4)").bind(attempt).bind(request.request_id).bind(hash).bind(permit).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(permit)
}

/// Relay-only admission: pin question_id at input.speech.started, then use that
/// same identity with the final user item. Multiple finals may belong to the same
/// actually delivered question. Skip/repeat preserve late final speech in the
/// ledger, but supersede its ability to advance topic/follow-up counters.
/// The callback itself never authorizes an answer merely by appending history.
pub async fn authorize_answer(
    pool: &PgPool,
    attempt: Uuid,
    question_id: Uuid,
    event_id: &str,
    text: &str,
    lease_id: Uuid,
    generation: i64,
) -> Result<(), ApiError> {
    if event_id.is_empty() || event_id.len() > 200 || text.trim().is_empty() || text.len() > 16000 {
        return Err(ApiError::invalid("Invalid final answer identity."));
    }
    let mut tx = pool.begin().await?;
    let id: Uuid = sqlx::query_scalar("SELECT interview_id FROM provider_attempts WHERE id=$1")
        .bind(attempt)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::conflict)?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let binding=sqlx::query("SELECT b.last_permit_id,b.delivered_permit_id,p.lease_generation FROM voice_hook_bindings b JOIN provider_attempts p ON p.id=b.attempt_id WHERE p.id=$1 AND p.state='active' AND p.control_mode='custom' AND b.binding_generation=p.lease_generation").bind(attempt).fetch_optional(&mut *tx).await?.ok_or_else(ApiError::conflict)?;
    if row.get::<Option<Uuid>, _>("lease_id") != Some(lease_id)
        || binding.get::<i64, _>("lease_generation") != generation
    {
        return Err(ApiError::conflict());
    }
    validate(&row, generation)?;
    let delivered: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM question_permits old WHERE old.id=$1 AND old.attempt_id=$2 AND old.delivered_at IS NOT NULL AND ($3::uuid=old.id OR EXISTS(SELECT 1 FROM question_permits control WHERE control.attempt_id=old.attempt_id AND control.progress_revision>old.progress_revision AND control.request_hash LIKE 'control:%')))")
        .bind(question_id).bind(attempt).bind(binding.get::<Option<Uuid>,_>("delivered_permit_id")).fetch_one(&mut *tx).await?;
    if !delivered {
        return Err(ApiError::conflict());
    }
    let hash = hash_secret(text.trim());
    let existing=sqlx::query("SELECT question_permit_id,answer_hash FROM voice_answer_permits WHERE attempt_id=$1 AND event_id=$2").bind(attempt).bind(event_id).fetch_optional(&mut *tx).await?;
    if let Some(existing) = existing {
        if existing.get::<Uuid, _>("question_permit_id") != question_id
            || existing.get::<String, _>("answer_hash") != hash
        {
            return Err(ApiError::conflict());
        }
    } else {
        let total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM voice_answer_permits WHERE attempt_id=$1")
                .bind(attempt)
                .fetch_one(&mut *tx)
                .await?;
        if total >= 100 {
            return Err(ApiError::conflict());
        }
        let inserted=sqlx::query("INSERT INTO voice_answer_permits(attempt_id,event_id,question_permit_id,answer_hash,trusted_text,response_permit_id,consumed) VALUES($1,$2,$3,$4,$5,(SELECT response_permit_id FROM voice_answer_permits WHERE attempt_id=$1 AND question_permit_id=$3 AND response_permit_id IS NOT NULL LIMIT 1),EXISTS(SELECT 1 FROM voice_answer_permits WHERE attempt_id=$1 AND question_permit_id=$3 AND consumed)) ON CONFLICT DO NOTHING").bind(attempt).bind(event_id).bind(question_id).bind(hash).bind(text.trim()).execute(&mut *tx).await?;
        if inserted.rows_affected() != 1 {
            return Err(ApiError::conflict());
        }
    }
    // A final clears only its own provisional marker in the same transaction.
    sqlx::query(
        "UPDATE provider_attempts SET incomplete_turn_ids=incomplete_turn_ids-$2::text WHERE id=$1",
    )
    .bind(attempt)
    .bind(event_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
/// Trusted setup retrieves this exact text for the provider greeting. Recovered
/// history belongs in system context; it must not become fresh user evidence.
pub async fn configured_greeting(
    pool: &PgPool,
    attempt: Uuid,
    secret: &str,
) -> Result<String, ApiError> {
    let row=sqlx::query("SELECT q.plan,i.* FROM voice_hook_bindings b JOIN provider_attempts p ON p.id=b.attempt_id JOIN interviews i ON i.id=p.interview_id JOIN question_permits q ON q.id=b.last_permit_id WHERE p.id=$1 AND b.secret_hash=$2 AND b.binding_generation=p.lease_generation AND p.state IN ('connecting','active') AND p.lease_generation=i.lease_generation")
    .bind(attempt).bind(hash_secret(secret)).fetch_optional(pool).await?.ok_or_else(ApiError::unauthorized)?;
    validate(&row, row.get("lease_generation"))?;
    let plan: QuestionPlan =
        serde_json::from_value(row.get("plan")).map_err(|_| ApiError::conflict())?;
    Ok(plan.code.text().to_owned())
}

/// Server-only delivery checkpoint, invoked for the first audio frame of the
/// reply pinned at reply.started, before forwarding that frame to the browser.
/// Offered completion text alone never establishes a question was delivered.
pub async fn mark_delivered(
    pool: &PgPool,
    attempt: Uuid,
    question: Uuid,
    lease: Uuid,
    generation: i64,
) -> Result<(), ApiError> {
    let mut tx = pool.begin().await?;
    let id: Uuid = sqlx::query_scalar("SELECT interview_id FROM provider_attempts WHERE id=$1")
        .bind(attempt)
        .fetch_one(&mut *tx)
        .await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    validate(&row, generation)?;
    if row.get::<Option<Uuid>, _>("lease_id") != Some(lease) {
        return Err(ApiError::conflict());
    }
    let updated=sqlx::query("UPDATE voice_hook_bindings b SET delivered_permit_id=$2 FROM provider_attempts p WHERE b.attempt_id=$1 AND p.id=b.attempt_id AND b.last_permit_id=$2 AND b.binding_generation=$3 AND p.lease_generation=$3 AND p.state='active'")
        .bind(attempt).bind(question).bind(generation).execute(&mut *tx).await?;
    if updated.rows_affected() != 1 {
        return Err(ApiError::conflict());
    }
    sqlx::query("UPDATE question_permits SET delivered_at=COALESCE(delivered_at,clock_timestamp()) WHERE id=$1 AND attempt_id=$2")
        .bind(question).bind(attempt).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Retrieve server-owned offered/delivered identities for the trusted relay.
/// Never infer delivery from callback history or client acknowledgement.
pub async fn questions(
    pool: &PgPool,
    attempt: Uuid,
    lease: Uuid,
    generation: i64,
) -> Result<(Uuid, Option<Uuid>), ApiError> {
    let row=sqlx::query("SELECT i.*,b.last_permit_id,b.delivered_permit_id FROM voice_hook_bindings b JOIN provider_attempts p ON p.id=b.attempt_id JOIN interviews i ON i.id=p.interview_id WHERE p.id=$1 AND p.state='active' AND b.binding_generation=$2 AND p.lease_generation=$2")
        .bind(attempt).bind(generation).fetch_optional(pool).await?.ok_or_else(ApiError::conflict)?;
    validate(&row, generation)?;
    if row.get::<Option<Uuid>, _>("lease_id") != Some(lease) {
        return Err(ApiError::conflict());
    }
    Ok((
        row.get::<Option<Uuid>, _>("last_permit_id")
            .ok_or_else(ApiError::conflict)?,
        row.get("delivered_permit_id"),
    ))
}

/// Persist an item identity as soon as the provider emits provisional speech.
/// Only final speech enters the answer ledger; provisional caption text stays
/// transient. A crashed relay leaves this marker for recording recovery.
pub async fn mark_incomplete(
    pool: &PgPool,
    attempt: Uuid,
    item: &str,
    lease: Uuid,
    generation: i64,
) -> Result<(), ApiError> {
    if item.is_empty() || item.len() > 200 {
        return Err(ApiError::invalid("Invalid speech item identity."));
    }
    let mut tx = pool.begin().await?;
    let id: Uuid = sqlx::query_scalar("SELECT interview_id FROM provider_attempts WHERE id=$1")
        .bind(attempt)
        .fetch_one(&mut *tx)
        .await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    validate(&row, generation)?;
    if row.get::<Option<Uuid>, _>("lease_id") != Some(lease) {
        return Err(ApiError::conflict());
    }
    let updated=sqlx::query("UPDATE provider_attempts SET incomplete_turn_ids=CASE WHEN incomplete_turn_ids ? $2 THEN incomplete_turn_ids ELSE incomplete_turn_ids || jsonb_build_array($2::text) END WHERE id=$1 AND lease_generation=$3 AND state='active' AND jsonb_array_length(incomplete_turn_ids)<100")
        .bind(attempt).bind(item).bind(generation).execute(&mut *tx).await?;
    if updated.rows_affected() != 1 {
        return Err(ApiError::conflict());
    }
    tx.commit().await?;
    Ok(())
}

/// A Finish click carries the same scoped lease and optimistic revisions as all
/// other voice controls. Runtime speech/reply state is checked by the relay too.
pub async fn validate_finish(
    pool: &PgPool,
    token: &str,
    request: &v0_domain::workflow::VoiceControlRequest,
) -> Result<(), ApiError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(request.interview_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    validate(&row, request.lease_generation)?;
    let authorized:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE token_hash=$1 AND role='customer' AND interview_id=$2 AND expires_at>clock_timestamp())").bind(hash_secret(token)).bind(request.interview_id).fetch_one(&mut *tx).await?;
    if !authorized {
        return Err(ApiError::unauthorized());
    }
    if !matches!(request.action, v0_domain::workflow::VoiceAction::Finish)
        || row.get::<Option<Uuid>, _>("lease_id") != Some(request.lease_id)
        || row.get::<i64, _>("revision") != request.expected_revision
        || row.get::<i64, _>("progress_revision") != request.expected_progress_revision
    {
        return Err(ApiError::conflict());
    }
    let pending:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM provider_attempts WHERE interview_id=$1 AND lease_generation=$2 AND (state!='active' OR jsonb_array_length(incomplete_turn_ids)>0))").bind(request.interview_id).bind(request.lease_generation).fetch_one(&mut *tx).await?;
    if row.get::<i32, _>("topic_index") < 3
        || row.get::<Option<String>, _>("incomplete_turn").is_some()
        || pending
    {
        return Err(ApiError(
            axum::http::StatusCode::CONFLICT,
            "finish_not_ready",
            "Wait for the current answer and interviewer reply to finish.",
        ));
    }
    tx.commit().await?;
    Ok(())
}
