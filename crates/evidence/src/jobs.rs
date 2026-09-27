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
    let row = sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key) SELECT $1,$2,'import_evidence',$3,$4 FROM provider_attempts p JOIN interviews i ON i.id=p.interview_id WHERE p.id=$5 AND p.interview_id=$2 AND p.provider_session_id IS NOT NULL AND i.deleted_at IS NULL AND i.expires_at>now() ON CONFLICT(dedupe_key) DO UPDATE SET dedupe_key=EXCLUDED.dedupe_key RETURNING id")
        .bind(Uuid::new_v4()).bind(interview).bind(json!({"provider_attempt_id": attempt})).bind(format!("import:{attempt}")).bind(attempt).fetch_optional(pool).await?.ok_or(Error::Stale)?;
    Ok(row.get("id"))
}

pub async fn claim(pool: &PgPool) -> Result<Option<Job>> {
    // A worker dying on its final attempt must become visibly failed, never stick running.
    sqlx::query("WITH exhausted AS (UPDATE jobs SET status='failed',last_error='lease_expired',lease_token=NULL,lease_until=NULL WHERE status='running' AND lease_until<=now() AND attempts>=max_attempts RETURNING interview_id,kind) UPDATE interviews SET state='recovering',updated_at=now() WHERE id IN (SELECT interview_id FROM exhausted WHERE kind='import_evidence') AND deleted_at IS NULL AND expires_at>now()")
        .execute(pool).await?;
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
    sqlx::query("WITH changed AS (UPDATE jobs SET status=CASE WHEN attempts>=max_attempts THEN 'failed' ELSE 'queued' END,available_at=now()+make_interval(secs=>LEAST(300,attempts*15)),last_error=$3,lease_token=NULL,lease_until=NULL WHERE id=$1 AND lease_token=$2 AND status='running' AND lease_until>now() RETURNING interview_id,status,kind) UPDATE interviews SET state='recovering',updated_at=now() WHERE id IN(SELECT interview_id FROM changed WHERE status='failed' AND kind='import_evidence') AND deleted_at IS NULL AND expires_at>now()")
        .bind(job.id).bind(job.token).bind(code).execute(pool).await?;
    Ok(())
}

pub async fn dispatch(
    pool: &PgPool,
    job: &Job,
    provider: &dyn HistoryProvider,
    storage: &dyn PrivateStorage,
) -> Result<()> {
    match job.kind.as_str() {
        "import_evidence" => import(pool, job, provider, storage).await,
        "cleanup_objects" => cleanup(pool, job, storage).await,
        _ => Err(Error::Invalid("unsupported job")),
    }
}

async fn import(
    pool: &PgPool,
    job: &Job,
    provider: &dyn HistoryProvider,
    storage: &dyn PrivateStorage,
) -> Result<()> {
    let attempt: Uuid = serde_json::from_value(
        job.payload
            .get("provider_attempt_id")
            .cloned()
            .ok_or(Error::Invalid("attempt payload"))?,
    )?;
    let mapping = sqlx::query("SELECT p.provider_session_id,i.revision FROM provider_attempts p JOIN interviews i ON i.id=p.interview_id WHERE p.id=$1 AND p.interview_id=$2 AND i.deleted_at IS NULL AND i.expires_at>now()")
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
    let manifest = manifest::build(
        &session,
        &artifacts.audio,
        &artifacts.timeline,
        &artifacts.metadata,
    )?;
    let mut value = serde_json::to_value(&manifest)?;
    value["timeline_key"] = json!(timeline_key);
    value["metadata_key"] = json!(metadata_key);
    storage.put(&audio_key, &artifacts.audio).await?;
    storage.put(&timeline_key, &artifacts.timeline).await?;
    storage.put(&metadata_key, &artifacts.metadata).await?;
    let mut tx = pool.begin().await?;
    let eligible = sqlx::query("SELECT id FROM interviews WHERE id=$1 AND deleted_at IS NULL AND expires_at>now() AND revision=$2 FOR UPDATE")
        .bind(job.interview_id).bind(revision).fetch_optional(&mut *tx).await?;
    if eligible.is_none() {
        return Err(Error::Stale);
    }
    let lease = sqlx::query("SELECT id FROM jobs WHERE id=$1 AND status='running' AND lease_token=$2 AND lease_until>now() FOR UPDATE")
        .bind(job.id).bind(job.token).fetch_optional(&mut *tx).await?;
    if lease.is_none() {
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
    sqlx::query("UPDATE interviews SET state='recovering',updated_at=now() WHERE id=$1")
        .bind(job.interview_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
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
        let referenced: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM evidence_imports WHERE recording_key=$1 OR manifest->>'timeline_key'=$1 OR manifest->>'metadata_key'=$1)").bind(&key).fetch_one(pool).await?;
        if !referenced {
            storage.delete(&key).await?;
        }
    }
    sqlx::query("UPDATE jobs SET status='succeeded',lease_token=NULL,lease_until=NULL,last_error=NULL WHERE id=$1 AND lease_token=$2 AND lease_until>now() AND status='running'").bind(job.id).bind(job.token).execute(pool).await?;
    Ok(())
}
