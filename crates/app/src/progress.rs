//! Durable server-owned checkpoints. Invoke before allowing input or output at
//! the relay; browser/model data cannot replace counters or elapsed time.
use crate::error::ApiError;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow};
use uuid::Uuid;
use v0_domain::workflow::{InterviewProgress, ProgressAction, ProgressCommand};

async fn fenced<'a>(
    pool: &'a PgPool,
    id: Uuid,
    lease: Uuid,
    generation: i64,
) -> Result<(Transaction<'a, Postgres>, PgRow), ApiError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    if row.get::<Option<Uuid>, _>("lease_id") != Some(lease)
        || row.get::<i64, _>("lease_generation") != generation
        || row
            .get::<Option<DateTime<Utc>>, _>("lease_expires_at")
            .is_none_or(|v| v <= Utc::now())
        || row.get::<DateTime<Utc>, _>("expires_at") <= Utc::now()
        || row.get::<Option<DateTime<Utc>>, _>("deleted_at").is_some()
        || row.get::<String, _>("state") != "interviewing"
        || row.get::<i32, _>("time_consumed_seconds") >= 360
        || row
            .get::<Option<DateTime<Utc>>, _>("consented_at")
            .is_none()
    {
        return Err(ApiError::conflict());
    }
    Ok((tx, row))
}
fn view(row: &PgRow) -> Result<InterviewProgress, ApiError> {
    let counts: serde_json::Value = row.get("followup_counts");
    Ok(InterviewProgress {
        revision: row.get("progress_revision"),
        topic: row.get::<i32, _>("topic_index") as u8,
        followups: serde_json::from_value(counts)
            .map_err(|_| ApiError::invalid("Invalid persisted progress."))?,
        completed_answers: row.get::<i32, _>("completed_answers") as u32,
        incomplete_turn: row.get("incomplete_turn"),
    })
}
/// Acknowledge this transaction before forwarding the first input.audio frame.
/// The provider session identity is immutable and globally unique.
pub async fn map_attempt(
    pool: &PgPool,
    id: Uuid,
    lease: Uuid,
    generation: i64,
    attempt: Uuid,
    provider_session: &str,
) -> Result<(), ApiError> {
    if provider_session.is_empty() || provider_session.len() > 200 {
        return Err(ApiError::invalid("Invalid provider identity."));
    }
    let (mut tx, _) = fenced(pool, id, lease, generation).await?;
    let old=sqlx::query("SELECT interview_id,provider_session_id,lease_generation,state,ended_at FROM provider_attempts WHERE id=$1").bind(attempt).fetch_optional(&mut *tx).await?;
    if let Some(old) = old {
        if old.get::<Uuid, _>("interview_id") != id
            || old.get::<i64, _>("lease_generation") != generation
            || old.get::<Option<DateTime<Utc>>, _>("ended_at").is_some()
        {
            return Err(ApiError::conflict());
        }
        let old_id = old.get::<Option<String>, _>("provider_session_id");
        let state = old.get::<String, _>("state");
        if state == "connecting" && old_id.is_none() {
            sqlx::query(
                "UPDATE provider_attempts SET provider_session_id=$2,state='active' WHERE id=$1",
            )
            .bind(attempt)
            .bind(provider_session)
            .execute(&mut *tx)
            .await?;
        } else if state != "active" || old_id.as_deref() != Some(provider_session) {
            return Err(ApiError::conflict());
        }
    } else {
        let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM provider_attempts WHERE interview_id=$1 AND state IN ('connecting','active'))")
            .bind(id).fetch_one(&mut *tx).await?;
        if active {
            return Err(ApiError::conflict());
        }
        let inserted=sqlx::query("INSERT INTO provider_attempts(id,interview_id,provider_session_id,lease_generation,state) VALUES($1,$2,$3,$4,'active') ON CONFLICT DO NOTHING")
            .bind(attempt).bind(id).bind(provider_session).bind(generation).execute(&mut *tx).await?;
        if inserted.rows_affected() != 1 {
            return Err(ApiError::conflict());
        }
    }
    tx.commit().await?;
    Ok(())
}
/// Reserve durable identity before configuring a provider/custom-LLM greeting.
/// Input remains forbidden until map_attempt commits the matching session.ready.
pub async fn prepare_attempt(
    pool: &PgPool,
    id: Uuid,
    lease: Uuid,
    generation: i64,
    attempt: Uuid,
) -> Result<(), ApiError> {
    let (mut tx, _) = fenced(pool, id, lease, generation).await?;
    let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM provider_attempts WHERE interview_id=$1 AND state IN ('connecting','active'))").bind(id).fetch_one(&mut *tx).await?;
    if active {
        return Err(ApiError::conflict());
    }
    sqlx::query("INSERT INTO provider_attempts(id,interview_id,lease_generation,state) VALUES($1,$2,$3,'connecting')").bind(attempt).bind(id).bind(generation).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
/// Attach only the known provider attempt after a verified native resume. The
/// relay must validate the provider's ready identity before forwarding audio.
pub async fn resume_attempt(
    pool: &PgPool,
    id: Uuid,
    lease: Uuid,
    generation: i64,
    attempt: Uuid,
    provider_session: &str,
) -> Result<(), ApiError> {
    let (mut tx, _) = fenced(pool, id, lease, generation).await?;
    let result=sqlx::query("UPDATE provider_attempts SET lease_generation=$3,state='active' WHERE id=$1 AND interview_id=$2 AND provider_session_id=$4 AND state IN ('active','recovering') AND ended_at IS NULL AND NOT EXISTS(SELECT 1 FROM provider_attempts p WHERE p.interview_id=$2 AND p.id<>$1 AND p.state IN ('connecting','active'))")
        .bind(attempt).bind(id).bind(generation).bind(provider_session).execute(&mut *tx).await?;
    if result.rows_affected() != 1 {
        return Err(ApiError::conflict());
    }
    tx.commit().await?;
    Ok(())
}
/// event_id is the final user item ID for Answer, not a fresh tool-call ID.
/// Skip/repeat use a client command UUID. Same event + changed meaning conflicts.
pub async fn checkpoint(
    pool: &PgPool,
    command: ProgressCommand,
) -> Result<InterviewProgress, ApiError> {
    if command.event_id.is_empty() || command.event_id.len() > 200 {
        return Err(ApiError::invalid("Stable event identity is required."));
    }
    let (mut tx, row) = fenced(
        pool,
        command.interview_id,
        command.lease_id,
        command.lease_generation,
    )
    .await?;
    let mapped:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM provider_attempts WHERE id=$1 AND interview_id=$2 AND lease_generation=$3 AND provider_session_id IS NOT NULL AND state='active' AND (control_mode='managed' OR $4))")
        .bind(command.attempt_id).bind(command.interview_id).bind(command.lease_generation).bind(matches!(command.action, ProgressAction::Incomplete{..})).fetch_one(&mut *tx).await?;
    if !mapped {
        return Err(ApiError::conflict());
    }
    let mut state = view(&row)?;
    // Fence/expected revision intentionally omitted from event meaning so a valid
    // repeated delivery cannot count the same answer again after reconnection.
    let hash = crate::auth::hash_secret(
        &serde_json::to_string(&command.action)
            .map_err(|_| ApiError::invalid("Invalid progress event."))?,
    );
    let prior: Option<String> = sqlx::query_scalar(
        "SELECT request_hash FROM progress_events WHERE attempt_id=$1 AND event_id=$2",
    )
    .bind(command.attempt_id)
    .bind(&command.event_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(prior) = prior {
        if prior != hash {
            return Err(ApiError::conflict());
        }
        tx.commit().await?;
        return Ok(state);
    }
    if command.expected_revision != state.revision || state.topic >= 3 {
        return Err(ApiError::conflict());
    }
    match command.action {
        ProgressAction::Answer { follow_up } => {
            if follow_up && state.followups[state.topic as usize] >= 2 {
                return Err(ApiError::invalid(
                    "Follow-up allowance exhausted; advance topic.",
                ));
            }
            state.completed_answers += 1;
            state.incomplete_turn = None;
            if follow_up {
                state.followups[state.topic as usize] += 1;
            } else {
                state.topic += 1;
            }
        }
        ProgressAction::Skip => {
            state.topic += 1;
            state.incomplete_turn = None;
        }
        ProgressAction::Repeat => {}
        ProgressAction::Incomplete { turn_id } => {
            if turn_id.is_empty() || turn_id.len() > 200 {
                return Err(ApiError::invalid("Invalid incomplete turn identity."));
            }
            state.incomplete_turn = Some(turn_id);
        }
    }
    state.revision += 1;
    sqlx::query("UPDATE interviews SET progress_revision=$2,topic_index=$3,followup_counts=$4,completed_answers=$5,incomplete_turn=$6 WHERE id=$1")
        .bind(command.interview_id).bind(state.revision).bind(state.topic as i32).bind(serde_json::json!(state.followups)).bind(state.completed_answers as i32).bind(&state.incomplete_turn).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO progress_events(interview_id,attempt_id,event_id,request_hash) VALUES($1,$2,$3,$4)")
        .bind(command.interview_id).bind(command.attempt_id).bind(command.event_id).bind(hash).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(state)
}

/// Persist trusted product termination independently of provider close/status.
/// ExplicitFinish is only supplied after a completed final turn is acknowledged.
/// TransportLost is terminal here: first exhaust native resume, then finalize.
/// A lost transport may be recorded after lease expiry, but never by an old generation.
pub async fn end_attempt(
    pool: &PgPool,
    id: Uuid,
    lease: Uuid,
    generation: i64,
    attempt: Uuid,
    reason: v0_domain::workflow::AttemptEndReason,
) -> Result<(), ApiError> {
    let mut tx = pool.begin().await?;
    let row =
        sqlx::query("SELECT lease_id,lease_generation FROM interviews WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(ApiError::unauthorized)?;
    if row.get::<Option<Uuid>, _>("lease_id") != Some(lease)
        || row.get::<i64, _>("lease_generation") != generation
    {
        return Err(ApiError::conflict());
    }
    let reason = serde_json::to_value(reason).map_err(|_| ApiError::conflict())?;
    let reason = reason.as_str().ok_or_else(ApiError::conflict)?;
    let result=sqlx::query("UPDATE provider_attempts SET product_end_reason=$4,ended_at=COALESCE(ended_at,clock_timestamp()),state=CASE WHEN state IN ('active','connecting','recovering') THEN 'ended' ELSE state END WHERE id=$1 AND interview_id=$2 AND lease_generation=$3 AND (product_end_reason IS NULL OR product_end_reason=$4)")
  .bind(attempt).bind(id).bind(generation).bind(reason).execute(&mut *tx).await?;
    if result.rows_affected() != 1 {
        return Err(ApiError::conflict());
    }
    tx.commit().await?;
    Ok(())
}
