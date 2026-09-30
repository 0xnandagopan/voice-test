use crate::error::ApiError;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgPool, Row};
use uuid::Uuid;
#[derive(Debug, Serialize)]
pub struct InterviewLease {
    pub lease_id: Uuid,
    pub generation: i64,
    pub remaining_seconds: i32,
}
pub async fn acquire(
    pool: &PgPool,
    interview: Uuid,
    expected_revision: i64,
) -> Result<InterviewLease, ApiError> {
    acquire_inner(pool, interview, expected_revision, None).await
}
/// Browser acquisition requires explicit consent/recovery acknowledgement.
/// Native resume remains a separate trusted path and cannot bypass this check.
pub async fn acquire_customer(
    pool: &PgPool,
    token: &str,
    interview: Uuid,
    expected_revision: i64,
) -> Result<InterviewLease, ApiError> {
    acquire_inner(pool, interview, expected_revision, Some(token)).await
}
async fn acquire_inner(
    pool: &PgPool,
    interview: Uuid,
    expected_revision: i64,
    customer: Option<&str>,
) -> Result<InterviewLease, ApiError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(interview)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    let now = Utc::now();
    let state: String = row.get("state");
    if row.get::<Option<DateTime<Utc>>, _>("deleted_at").is_some()
        || row.get::<DateTime<Utc>, _>("expires_at") <= now
        || matches!(state.as_str(), "revoked" | "deleted")
    {
        return Err(ApiError::expired());
    }
    if !matches!(
        row.get::<String, _>("interview_preparation").as_str(),
        "not_required" | "ready"
    ) {
        return Err(ApiError::invalid(
            "The project-specific interview is still being prepared. Please try again shortly.",
        ));
    }
    if !matches!(state.as_str(), "consented" | "recovering" | "interviewing")
        || row
            .get::<Option<DateTime<Utc>>, _>("consented_at")
            .is_none()
    {
        return Err(ApiError::invalid("Recording consent is required."));
    }
    if let Some(token) = customer {
        let valid:bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE token_hash=$1 AND interview_id=$2 AND role='customer' AND expires_at>clock_timestamp())")
            .bind(crate::auth::hash_secret(token)).bind(interview).fetch_one(&mut *tx).await?;
        if !valid {
            return Err(ApiError::unauthorized());
        }
        if state != "consented"
            || row
                .get::<Option<DateTime<Utc>>, _>("completed_at")
                .is_some()
        {
            return Err(ApiError::conflict());
        }
    }
    if row.get::<i64, _>("revision") != expected_revision {
        return Err(ApiError::conflict());
    }
    if row
        .get::<Option<DateTime<Utc>>, _>("lease_expires_at")
        .is_some_and(|until| until > now)
    {
        return Err(ApiError::conflict());
    }
    let mut consumed: i32 = row.get("time_consumed_seconds");
    if let Some(since) = row.get::<Option<DateTime<Utc>>, _>("active_since") {
        let until = row
            .get::<Option<DateTime<Utc>>, _>("lease_expires_at")
            .unwrap_or(now)
            .min(now);
        consumed = (consumed
            + (((until - since).num_milliseconds().clamp(0, 360_000) + 999) / 1000) as i32)
            .min(360);
    }
    if consumed >= 360 {
        return Err(ApiError::invalid("The interview time allowance has ended."));
    }
    let lease = InterviewLease {
        lease_id: Uuid::new_v4(),
        generation: row.get::<i64, _>("lease_generation") + 1,
        remaining_seconds: 360 - consumed,
    };
    sqlx::query("UPDATE interviews SET lease_id=$2, lease_generation=$3, lease_expires_at=clock_timestamp()+make_interval(secs => $4), active_since=clock_timestamp(), time_consumed_seconds=$5, state='interviewing', revision=revision+1, updated_at=clock_timestamp() WHERE id=$1")
        .bind(interview).bind(lease.lease_id).bind(lease.generation).bind(lease.remaining_seconds.min(30) as f64).bind(consumed).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(lease)
}
pub async fn heartbeat(
    pool: &PgPool,
    interview: Uuid,
    lease_id: Uuid,
    generation: i64,
) -> Result<i32, ApiError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 AND lease_id=$2 AND lease_generation=$3 AND lease_expires_at>clock_timestamp() AND deleted_at IS NULL AND expires_at>clock_timestamp() AND state='interviewing' FOR UPDATE")
        .bind(interview).bind(lease_id).bind(generation).fetch_optional(&mut *tx).await?.ok_or_else(ApiError::conflict)?;
    let now = Utc::now();
    if row.get::<DateTime<Utc>, _>("expires_at") <= now
        || row
            .get::<Option<DateTime<Utc>>, _>("lease_expires_at")
            .is_none_or(|deadline| deadline <= now)
    {
        return Err(ApiError::expired());
    }
    let since: DateTime<Utc> = row.get("active_since");
    let baseline: i32 = row.get("time_consumed_seconds");
    let deadline = since + chrono::Duration::seconds(i64::from(360 - baseline));
    let remaining_ms = (deadline - now).num_milliseconds().max(0);
    let remaining = ((remaining_ms + 999) / 1000).min(360) as i32;
    // Keep the original attempt clock. Rounding each heartbeat to whole seconds
    // would repeatedly charge fractional seconds and shorten a six-minute call.
    sqlx::query("UPDATE interviews SET lease_expires_at=LEAST($4,clock_timestamp()+interval '30 seconds') WHERE id=$1 AND lease_id=$2 AND lease_generation=$3")
        .bind(interview).bind(lease_id).bind(generation).bind(deadline).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(remaining)
}

pub async fn release(
    pool: &PgPool,
    interview: Uuid,
    lease_id: Uuid,
    generation: i64,
) -> Result<(), ApiError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 AND lease_id=$2 AND lease_generation=$3 AND state='interviewing' AND deleted_at IS NULL FOR UPDATE")
        .bind(interview).bind(lease_id).bind(generation).fetch_optional(&mut *tx).await?.ok_or_else(ApiError::conflict)?;
    let until = row
        .get::<Option<DateTime<Utc>>, _>("lease_expires_at")
        .unwrap_or_else(Utc::now)
        .min(Utc::now());
    let since: DateTime<Utc> = row.get("active_since");
    let elapsed = (((until - since).num_milliseconds().clamp(0, 360_000) + 999) / 1000) as i32;
    let consumed = (row.get::<i32, _>("time_consumed_seconds") + elapsed).min(360);
    sqlx::query("UPDATE interviews SET time_consumed_seconds=$2, active_since=NULL, lease_id=NULL,lease_expires_at=NULL,state='recovering',revision=revision+1,updated_at=clock_timestamp() WHERE id=$1")
        .bind(interview).bind(consumed).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
