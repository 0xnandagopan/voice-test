use crate::{Error, Result, manifest, provider::HistoryProvider, storage::PrivateStorage};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Debug)]
pub struct Job {
    pub id: Uuid,
    pub interview_id: Uuid,
    pub kind: String,
    pub payload: Value,
    pub token: Uuid,
}

pub async fn enqueue_import(pool: &PgPool, interview: Uuid, attempt: Uuid) -> Result<Uuid> {
    let row = sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key) SELECT $1,$2,'import_evidence',$3,$4 FROM provider_attempts p JOIN interviews i ON i.id=p.interview_id WHERE p.id=$5 AND p.interview_id=$2 AND p.provider_session_id IS NOT NULL AND i.deleted_at IS NULL AND i.state NOT IN ('revoked','deleted') AND i.expires_at>now() ON CONFLICT(dedupe_key) DO UPDATE SET dedupe_key=EXCLUDED.dedupe_key RETURNING id")
        .bind(Uuid::new_v4()).bind(interview).bind(json!({"provider_attempt_id": attempt})).bind(format!("import:{attempt}")).bind(attempt).fetch_optional(pool).await?.ok_or(Error::Stale)?;
    Ok(row.get("id"))
}

pub async fn claim(pool: &PgPool) -> Result<Option<Job>> {
    // A worker dying on its final attempt must become visibly failed, never stick running.
    let exhausted = sqlx::query("UPDATE jobs SET status='failed',last_error='lease_expired',lease_token=NULL,lease_until=NULL WHERE status='running' AND lease_until<=now() AND attempts>=max_attempts RETURNING id,interview_id,kind")
        .fetch_all(pool).await?;
    // Autocommit releases job locks before acquiring interview locks. Failure
    // propagation is fenced by the still-failed job and current workflow state.
    for row in exhausted {
        if row.get::<String, _>("kind") == "import_evidence" {
            propagate_failure(pool, row.get("id"), row.get("interview_id")).await?;
        }
    }
    let token = Uuid::new_v4();
    let row = sqlx::query("WITH candidate AS (SELECT id FROM jobs WHERE ((status='queued' AND available_at<=now()) OR (status='running' AND lease_until<=now())) AND attempts<max_attempts ORDER BY available_at,id FOR UPDATE SKIP LOCKED LIMIT 1) UPDATE jobs SET status='running',attempts=attempts+1,lease_token=$1,lease_until=now()+interval '120 seconds' WHERE id=(SELECT id FROM candidate) RETURNING id,interview_id,kind,payload")
        .bind(token).fetch_optional(pool).await?;
    Ok(row.map(|r| Job {
        id: r.get("id"),
        interview_id: r.get("interview_id"),
        kind: r.get("kind"),
        payload: r.get("payload"),
        token,
    }))
}

pub async fn fail(pool: &PgPool, job: &Job, error_code: &str) -> Result<()> {
    // Callers supply fixed codes only; provider errors can contain signed URLs or text.
    let code = match error_code {
        "artifacts_not_ready"
        | "invalid_artifacts"
        | "import_timeout"
        | "provider_unavailable"
        | "storage_unavailable"
        | "unsupported_job" => error_code,
        _ => "import_failed",
    };
    let changed=sqlx::query("UPDATE jobs SET status=CASE WHEN attempts>=max_attempts THEN 'failed' ELSE 'queued' END,available_at=now()+make_interval(secs=>LEAST(300,attempts*15)),last_error=$3,lease_token=NULL,lease_until=NULL WHERE id=$1 AND lease_token=$2 AND status='running' AND lease_until>now() RETURNING status,kind")
        .bind(job.id).bind(job.token).bind(code).fetch_optional(pool).await?;
    if changed.is_some_and(|row| {
        row.get::<String, _>("status") == "failed"
            && row.get::<String, _>("kind") == "import_evidence"
    }) {
        propagate_failure(pool, job.id, job.interview_id).await?;
    }
    Ok(())
}

async fn propagate_failure(pool: &PgPool, job: Uuid, interview: Uuid) -> Result<()> {
    sqlx::query("UPDATE interviews SET state='recovering',updated_at=now() WHERE id=$1 AND deleted_at IS NULL AND expires_at>now() AND active_since IS NULL AND state IN ('invited','consented','recovering') AND EXISTS(SELECT 1 FROM jobs WHERE id=$2 AND interview_id=$1 AND status='failed')")
        .bind(interview).bind(job).execute(pool).await?;
    Ok(())
}

pub async fn dispatch(
    pool: &PgPool,
    job: &Job,
    provider: &dyn HistoryProvider,
    storage: &dyn PrivateStorage,
) -> Result<()> {
    dispatch_with_media(pool, job, provider, storage, None).await
}

pub async fn dispatch_with_media(
    pool: &PgPool,
    job: &Job,
    provider: &dyn HistoryProvider,
    storage: &dyn PrivateStorage,
    validator: Option<&crate::media::FfmpegValidator>,
) -> Result<()> {
    match job.kind.as_str() {
        "import_evidence" => import(pool, job, provider, storage, validator).await,
        "cleanup_objects" => cleanup(pool, job, storage).await,
        _ => Err(Error::Invalid("unsupported job")),
    }
}

async fn import(
    pool: &PgPool,
    job: &Job,
    provider: &dyn HistoryProvider,
    storage: &dyn PrivateStorage,
    validator: Option<&crate::media::FfmpegValidator>,
) -> Result<()> {
    let attempt: Uuid = serde_json::from_value(
        job.payload
            .get("provider_attempt_id")
            .cloned()
            .ok_or(Error::Invalid("attempt payload"))?,
    )?;
    let mapping = sqlx::query("SELECT p.provider_session_id,p.product_end_reason,i.revision FROM provider_attempts p JOIN interviews i ON i.id=p.interview_id WHERE p.id=$1 AND p.interview_id=$2 AND i.deleted_at IS NULL AND i.state NOT IN ('revoked','deleted') AND i.expires_at>now()")
        .bind(attempt).bind(job.interview_id).fetch_optional(pool).await?.ok_or(Error::Stale)?;
    let session: Option<String> = mapping.get("provider_session_id");
    let session = session.ok_or(Error::NotReady)?;
    let revision: i64 = mapping.get("revision");
    let audio_key = format!("{}-{}.ogg", attempt, job.token);
    let timeline_key = format!("{}-{}.timeline.json", attempt, job.token);
    let metadata_key = format!("{}-{}.metadata.json", attempt, job.token);
    // Schedule orphan cleanup before any provider/object I/O, covering process crashes.
    sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key,available_at) VALUES($1,$2,'cleanup_objects',$3,$4,now()+interval '10 minutes') ON CONFLICT(dedupe_key) DO NOTHING")
        .bind(Uuid::new_v4()).bind(job.interview_id).bind(json!({"keys":[audio_key,timeline_key,metadata_key]})).bind(format!("cleanup:{}", job.token)).execute(pool).await?;
    let artifacts = provider.fetch(&session).await?;
    let mut manifest = manifest::build(
        &session,
        &artifacts.audio,
        &artifacts.timeline,
        &artifacts.metadata,
    )?;
    manifest.provider_close_reason = artifacts.close_reason.clone();
    manifest.product_end_reason = mapping.get("product_end_reason");
    if let Some(validator) = validator {
        manifest.media = Some(validator.validate(&artifacts.audio, &manifest).await?);
        manifest.recording_validation = "decoded_bounded_alignment_unverified".into();
        // Successful decoding proves availability, never transcript alignment or meaning.
        manifest.approval_eligible = false;
    }
    let mut value = serde_json::to_value(&manifest)?;
    value["timeline_key"] = json!(timeline_key);
    value["metadata_key"] = json!(metadata_key);
    storage.put(&audio_key, &artifacts.audio).await?;
    storage.put(&timeline_key, &artifacts.timeline).await?;
    storage.put(&metadata_key, &artifacts.metadata).await?;
    let mut tx = pool.begin().await?;
    let eligible = sqlx::query("SELECT expires_at FROM interviews WHERE id=$1 AND deleted_at IS NULL AND state NOT IN ('revoked','deleted') AND revision=$2 FOR UPDATE")
        .bind(job.interview_id).bind(revision).fetch_optional(&mut *tx).await?;
    let eligible = eligible.ok_or(Error::Stale)?;
    let expires_at: chrono::DateTime<chrono::Utc> = eligible.get("expires_at");
    let lease = sqlx::query("SELECT lease_until FROM jobs WHERE id=$1 AND status='running' AND lease_token=$2 FOR UPDATE")
        .bind(job.id).bind(job.token).fetch_optional(&mut *tx).await?;
    let lease = lease.ok_or(Error::Stale)?;
    let lease_until: chrono::DateTime<chrono::Utc> = lease.get("lease_until");
    // Both rows are protected now. Transaction-start now() could predate an
    // arbitrarily long lock wait, so evaluate the actual wall clock here.
    ensure_import_time(&mut tx, expires_at, lease_until).await?;
    let mapping_matches: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM provider_attempts WHERE id=$1 AND interview_id=$2 AND provider_session_id=$3)")
        .bind(attempt).bind(job.interview_id).bind(&session).fetch_one(&mut *tx).await?;
    if !mapping_matches {
        return Err(Error::Stale);
    }
    // The attempt's immutable provider identity plus its unique import prevents duplicate answers.
    let old: Option<Value> =
        sqlx::query_scalar("SELECT manifest FROM evidence_imports WHERE provider_attempt_id=$1")
            .bind(attempt)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(old) = old {
        if old["artifact_identity"] != value["artifact_identity"]
            || old["recording_sha256"] != value["recording_sha256"]
            || old["timeline_sha256"] != value["timeline_sha256"]
            || old["metadata_sha256"] != value["metadata_sha256"]
        {
            return Err(Error::Invalid("provider artifact changed after import"));
        }
    } else {
        sqlx::query("INSERT INTO evidence_imports(id,interview_id,provider_attempt_id,manifest,recording_key) VALUES($1,$2,$3,$4,$5)")
            .bind(Uuid::new_v4()).bind(job.interview_id).bind(attempt).bind(value).bind(audio_key).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE jobs SET status='succeeded',lease_token=NULL,lease_until=NULL,last_error=NULL WHERE id=$1").bind(job.id).execute(&mut *tx).await?;
    sqlx::query("UPDATE provider_attempts SET state='imported' WHERE id=$1")
        .bind(attempt)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE interviews SET state='recovering',updated_at=clock_timestamp() WHERE id=$1 AND active_since IS NULL AND state IN ('invited','consented','recovering')")
        .bind(job.interview_id)
        .execute(&mut *tx)
        .await?;
    // Later writes can also wait (for example on an attempt row). Roll back
    // every attachment if either deadline elapsed before completion.
    ensure_import_time(&mut tx, expires_at, lease_until).await?;
    tx.commit().await?;
    Ok(())
}

async fn ensure_import_time(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    expires_at: chrono::DateTime<chrono::Utc>,
    lease_until: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let current: bool =
        sqlx::query_scalar("SELECT clock_timestamp() < $1 AND clock_timestamp() < $2")
            .bind(expires_at)
            .bind(lease_until)
            .fetch_one(&mut **tx)
            .await?;
    if current { Ok(()) } else { Err(Error::Stale) }
}

async fn cleanup(pool: &PgPool, job: &Job, storage: &dyn PrivateStorage) -> Result<()> {
    let keys: Vec<String> = serde_json::from_value(
        job.payload
            .get("keys")
            .cloned()
            .ok_or(Error::Invalid("cleanup payload"))?,
    )?;
    if keys.len() > 3 {
        return Err(Error::Invalid("cleanup key bound"));
    }
    for key in keys {
        let owns_lease: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM jobs WHERE id=$1 AND lease_token=$2 AND status='running' AND lease_until>now())")
            .bind(job.id).bind(job.token).fetch_one(pool).await?;
        if !owns_lease {
            return Err(Error::Stale);
        }
        let referenced: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM evidence_imports WHERE recording_key=$1 OR manifest->>'timeline_key'=$1 OR manifest->>'metadata_key'=$1)").bind(&key).fetch_one(pool).await?;
        if !referenced {
            storage.delete(&key).await?;
        }
    }
    sqlx::query("UPDATE jobs SET status='succeeded',lease_token=NULL,lease_until=NULL,last_error=NULL WHERE id=$1 AND lease_token=$2 AND lease_until>now() AND status='running'").bind(job.id).bind(job.token).execute(pool).await?;
    Ok(())
}
