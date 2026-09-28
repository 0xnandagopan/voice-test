//! Reconcile abandoned relay leases after process loss. No media/provider I/O here.
use crate::error::ApiError;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;

pub async fn reap_expired(pool: &PgPool) -> Result<usize, ApiError> {
    let mut tx = pool.begin().await?;
    let rows = sqlx::query("SELECT * FROM interviews WHERE lease_id IS NOT NULL AND lease_expires_at<=clock_timestamp() ORDER BY lease_expires_at LIMIT 32 FOR UPDATE SKIP LOCKED")
        .fetch_all(&mut *tx).await?;
    let count = rows.len();
    for row in rows {
        let id: Uuid = row.get("id");
        let generation: i64 = row.get("lease_generation");
        let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        let until = row.get::<DateTime<Utc>, _>("lease_expires_at").min(now);
        let elapsed = row
            .get::<Option<DateTime<Utc>>, _>("active_since")
            .map(|at| ((until - at).num_milliseconds().clamp(0, 360000) + 999) / 1000)
            .unwrap_or(0) as i32;
        let consumed = (row.get::<i32, _>("time_consumed_seconds") + elapsed).min(360);
        let available = row.get::<DateTime<Utc>, _>("expires_at") > now
            && row.get::<Option<DateTime<Utc>>, _>("deleted_at").is_none()
            && matches!(
                row.get::<String, _>("state").as_str(),
                "interviewing" | "recovering"
            );
        let reason = if consumed >= 360 {
            "budget_exhausted"
        } else {
            "transport_lost"
        };
        let attempts=sqlx::query("UPDATE provider_attempts SET state='ended',ended_at=COALESCE(ended_at,clock_timestamp()),product_end_reason=COALESCE(product_end_reason,$3) WHERE interview_id=$1 AND lease_generation=$2 AND state IN ('connecting','active','recovering') RETURNING id,provider_session_id,incomplete_turn_ids")
            .bind(id).bind(generation).bind(reason).fetch_all(&mut *tx).await?;
        for attempt in attempts {
            let aid: Uuid = attempt.get("id");
            // Attempt-local incomplete IDs survive the crash unchanged.
            let _: Value = attempt.get("incomplete_turn_ids");
            if available
                && attempt
                    .get::<Option<String>, _>("provider_session_id")
                    .is_some()
            {
                sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key) VALUES($1,$2,'import_evidence',$3,$4) ON CONFLICT(dedupe_key) DO NOTHING")
                    .bind(Uuid::new_v4()).bind(id).bind(json!({"provider_attempt_id":aid})).bind(format!("import:{aid}")).execute(&mut *tx).await?;
            }
            sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key) VALUES($1,$2,'delete_provider_agent',$3,$4) ON CONFLICT(dedupe_key) DO UPDATE SET available_at=LEAST(jobs.available_at,clock_timestamp()) WHERE jobs.status='queued'")
                .bind(Uuid::new_v4()).bind(id).bind(json!({"attempt_id":aid})).bind(format!("agent-cleanup:{aid}")).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE interviews SET time_consumed_seconds=$2,lease_id=NULL,lease_expires_at=NULL,active_since=NULL,state=CASE WHEN $3 THEN 'recovering' ELSE state END,revision=revision+1,updated_at=clock_timestamp() WHERE id=$1")
            .bind(id).bind(consumed).bind(available).execute(&mut *tx).await?;
        sqlx::query(
            "INSERT INTO audit_events(interview_id,event) VALUES($1,'expired_relay_recovered')",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(count)
}
