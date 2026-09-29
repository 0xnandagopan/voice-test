//! Background recording alignment. Customer approval never requires an editorial
//! decision from the operator; uncertain evidence remains a technical exception.
use crate::{composition_jobs::JobFailure, workflow};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;
use v0_evidence::{
    automatic_alignment,
    jobs::Job,
    manifest::{Manifest, digest},
    media::FfmpegValidator,
    storage::PrivateStorage,
    stt::{AssemblyAiTranscriber, TranscriptStatus},
};

fn failure(code: &'static str, terminal: bool) -> JobFailure {
    JobFailure {
        code,
        terminal,
        retry_after_secs: 15,
    }
}
fn evidence_error(error: v0_evidence::Error) -> JobFailure {
    let code = match error {
        v0_evidence::Error::Invalid("recorded transcript differs from original answers") => {
            "alignment_transcript_mismatch"
        }
        v0_evidence::Error::Invalid(
            "recording completion unresolved" | "recording completeness unverified",
        ) => "alignment_recording_incomplete",
        v0_evidence::Error::Invalid(
            "ambiguous overlapping whole answers"
            | "ambiguous answer boundary"
            | "untranscribed customer audio remains",
        ) => "alignment_ranges_uncertain",
        v0_evidence::Error::Io(_) => return failure("alignment_storage_unavailable", false),
        _ => "alignment_unverified",
    };
    failure(code, true)
}

/// One durable task per immutable recording, including recordings imported before
/// automatic alignment existed. Polling is maintenance, never customer activity.
pub async fn enqueue_missing(pool: &PgPool) -> Result<(), sqlx::Error> {
    let rows=sqlx::query("SELECT e.interview_id,e.provider_attempt_id,e.manifest->>'recording_sha256' AS hash FROM evidence_imports e JOIN interviews i ON i.id=e.interview_id WHERE i.deleted_at IS NULL AND i.state NOT IN ('deleted','revoked') AND i.expires_at>clock_timestamp() AND e.manifest->'media' IS NOT NULL AND e.manifest->'media'<>'null'::jsonb AND e.manifest->>'approval_eligible'='false' AND COALESCE(jsonb_array_length(e.manifest->'operator_alignment'),0)=0 AND NOT EXISTS(SELECT 1 FROM jobs j WHERE j.dedupe_key='align-v2:'||e.provider_attempt_id::text||':'||(e.manifest->>'recording_sha256')) LIMIT 32").fetch_all(pool).await?;
    for row in rows {
        let attempt: Uuid = row.get("provider_attempt_id");
        let hash: String = row.get("hash");
        sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key,max_attempts) VALUES($1,$2,'align_evidence',$3,$4,40) ON CONFLICT(dedupe_key) DO NOTHING")
            .bind(Uuid::new_v4()).bind(row.get::<Uuid,_>("interview_id")).bind(json!({"provider_attempt_id":attempt,"recording_sha256":hash,"alignment_version":2}))
            .bind(format!("align-v2:{attempt}:{hash}")).execute(pool).await?;
    }
    Ok(())
}
async fn checkpoint(pool: &PgPool, job: &Job, payload: &Value) -> Result<(), JobFailure> {
    let changed=sqlx::query("UPDATE jobs j SET payload=$3 WHERE j.id=$1 AND j.lease_token=$2 AND j.status='running' AND j.lease_until>clock_timestamp() AND EXISTS(SELECT 1 FROM interviews i JOIN evidence_imports e ON e.interview_id=i.id WHERE i.id=j.interview_id AND i.deleted_at IS NULL AND i.state NOT IN ('deleted','revoked') AND i.expires_at>clock_timestamp() AND e.provider_attempt_id::text=$3->>'provider_attempt_id' AND e.manifest->>'recording_sha256'=$3->>'recording_sha256' AND COALESCE(jsonb_array_length(e.manifest->'operator_alignment'),0)=0)")
        .bind(job.id).bind(job.token).bind(payload).execute(pool).await.map_err(|_|failure("alignment_storage_unavailable",false))?.rows_affected();
    if changed != 1 {
        return Err(failure("alignment_stale", true));
    }
    Ok(())
}
async fn defer(pool: &PgPool, job: &Job) -> Result<(), JobFailure> {
    let changed=sqlx::query("UPDATE jobs SET status=CASE WHEN attempts>=max_attempts THEN 'failed' ELSE 'queued' END,last_error=CASE WHEN attempts>=max_attempts THEN 'alignment_timeout' ELSE NULL END,available_at=clock_timestamp()+interval '10 seconds',lease_token=NULL,lease_until=NULL WHERE id=$1 AND lease_token=$2 AND status='running' AND lease_until>clock_timestamp()")
        .bind(job.id).bind(job.token).execute(pool).await.map_err(|_|failure("alignment_storage_unavailable",false))?.rows_affected();
    if changed != 1 {
        return Err(failure("alignment_stale", true));
    }
    Ok(())
}
async fn persist_submission(
    pool: &PgPool,
    job: &Job,
    transcript: &str,
    payload: &Value,
) -> Result<(), JobFailure> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| failure("alignment_storage_unavailable", false))?;
    let changed=sqlx::query("UPDATE jobs SET payload=$3 WHERE id=$1 AND lease_token=$2 AND status='running' AND lease_until>clock_timestamp()")
        .bind(job.id).bind(job.token).bind(payload).execute(&mut *tx).await.map_err(|_|failure("alignment_storage_unavailable",false))?.rows_affected();
    if changed != 1 {
        return Err(failure("alignment_stale", true));
    }
    sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key,available_at,max_attempts) VALUES($1,$2,'delete_alignment_transcript',$3,$4,clock_timestamp()+interval '15 minutes',10) ON CONFLICT(dedupe_key) DO NOTHING")
        .bind(Uuid::new_v4()).bind(job.interview_id).bind(json!({"transcript_id":transcript,"alignment_job_id":job.id})).bind(format!("stt-cleanup:{transcript}")).execute(&mut *tx).await.map_err(|_|failure("alignment_storage_unavailable",false))?;
    tx.commit()
        .await
        .map_err(|_| failure("alignment_storage_unavailable", false))?;
    Ok(())
}
pub async fn dispatch(
    pool: &PgPool,
    job: &Job,
    client: &AssemblyAiTranscriber,
    storage: &dyn PrivateStorage,
    validator: &FfmpegValidator,
) -> Result<(), JobFailure> {
    if job.kind == "delete_alignment_transcript" {
        let id = job.payload["transcript_id"]
            .as_str()
            .ok_or_else(|| failure("alignment_payload_invalid", true))?;
        // Keep an in-progress transcript for at most one hour. Deletion/expiry
        // releases it immediately; this maintenance never extends customer access.
        let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM jobs a JOIN interviews i ON i.id=a.interview_id WHERE a.id::text=$1 AND a.status IN ('queued','running') AND a.created_at>clock_timestamp()-interval '1 hour' AND i.deleted_at IS NULL AND i.state NOT IN ('deleted','revoked') AND i.expires_at>clock_timestamp())")
            .bind(job.payload["alignment_job_id"].as_str().unwrap_or("")).fetch_one(pool).await.map_err(|_|failure("alignment_storage_unavailable",false))?;
        if active {
            sqlx::query("UPDATE jobs SET status='queued',attempts=GREATEST(0,attempts-1),available_at=clock_timestamp()+interval '1 minute',lease_token=NULL,lease_until=NULL WHERE id=$1 AND lease_token=$2 AND status='running' AND lease_until>clock_timestamp()")
                .bind(job.id).bind(job.token).execute(pool).await.map_err(|_|failure("alignment_storage_unavailable",false))?;
            return Ok(());
        }
        client
            .delete(id)
            .await
            .map_err(|_| failure("alignment_cleanup_pending", false))?;
        sqlx::query("UPDATE jobs SET status='succeeded',lease_token=NULL,lease_until=NULL,last_error=NULL WHERE id=$1 AND lease_token=$2 AND status='running' AND lease_until>clock_timestamp()")
            .bind(job.id).bind(job.token).execute(pool).await.map_err(|_|failure("alignment_storage_unavailable",false))?;
        return Ok(());
    }
    let attempt: Uuid = serde_json::from_value(job.payload["provider_attempt_id"].clone())
        .map_err(|_| failure("alignment_payload_invalid", true))?;
    let row=sqlx::query("SELECT e.manifest,e.recording_key FROM evidence_imports e JOIN interviews i ON i.id=e.interview_id WHERE e.interview_id=$1 AND e.provider_attempt_id=$2 AND i.deleted_at IS NULL AND i.state NOT IN ('deleted','revoked') AND i.expires_at>clock_timestamp()")
        .bind(job.interview_id).bind(attempt).fetch_optional(pool).await.map_err(|_|failure("alignment_storage_unavailable",false))?.ok_or_else(||failure("alignment_stale",true))?;
    let original: Value = row.get("manifest");
    let mut manifest: Manifest = serde_json::from_value(original.clone())
        .map_err(|_| failure("alignment_payload_invalid", true))?;
    if job.payload["recording_sha256"] != manifest.recording_sha256
        || !manifest.operator_alignment.is_empty()
    {
        return Err(failure("alignment_stale", true));
    }
    automatic_alignment::preflight(&manifest).map_err(evidence_error)?;
    let mut payload = job.payload.clone();
    let transcript_id = if let Some(id) = payload["transcript_id"].as_str() {
        id.to_owned()
    } else {
        if payload["submit_started"] == true {
            return Err(failure("alignment_submission_uncertain", true));
        }
        let upload_url = if let Some(url) = payload["upload_url"].as_str() {
            url.to_owned()
        } else {
            let audio = storage
                .read(&row.get::<String, _>("recording_key"))
                .await
                .map_err(evidence_error)?;
            let wav = validator
                .customer_wav(&audio, &manifest)
                .await
                .map_err(evidence_error)?;
            checkpoint(pool, job, &payload).await?;
            let url = client
                .upload(&wav)
                .await
                .map_err(|_| failure("alignment_provider_unavailable", false))?;
            payload["upload_url"] = json!(url);
            checkpoint(pool, job, &payload).await?;
            url
        };
        // An uncertain submission is not automatically repeated, preventing an
        // expired/reclaimed lease from issuing duplicate billable transcription.
        payload["submit_started"] = json!(true);
        checkpoint(pool, job, &payload).await?;
        let id = client
            .submit(&upload_url)
            .await
            .map_err(|_| failure("alignment_submission_failed", true))?;
        payload["transcript_id"] = json!(id);
        payload.as_object_mut().unwrap().remove("upload_url");
        if let Err(error) = persist_submission(pool, job, &id, &payload).await {
            // A successful POST must not lose its deletion obligation on a DB
            // failure. If commit was ambiguous, a later poll fails closed.
            let _ = client.delete(&id).await;
            return Err(error);
        }
        id
    };
    checkpoint(pool, job, &payload).await?;
    let transcript = match client
        .poll(&transcript_id)
        .await
        .map_err(|_| failure("alignment_provider_unavailable", false))?
    {
        TranscriptStatus::Pending => return defer(pool, job).await,
        TranscriptStatus::Failed => return Err(failure("alignment_transcription_failed", true)),
        TranscriptStatus::Completed(value) => value,
    };
    automatic_alignment::prepare_recorded_sources(&mut manifest, &transcript)
        .map_err(evidence_error)?;
    let ranges = automatic_alignment::plan(&manifest, &transcript).map_err(evidence_error)?;
    let audio = storage
        .read(&row.get::<String, _>("recording_key"))
        .await
        .map_err(evidence_error)?;
    let mut clips = vec![];
    for range in ranges {
        clips.push(
            validator
                .preview_range(&audio, &manifest, &range.source_id, range.source_range_ms)
                .await
                .map_err(evidence_error)?,
        );
    }
    automatic_alignment::confirm(&mut manifest, &transcript, &clips, chrono::Utc::now())
        .map_err(evidence_error)?;
    let mut staged = vec![];
    for clip in &clips {
        let id = Uuid::new_v4();
        let key = format!("clip-{id}.wav");
        sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key,available_at) VALUES($1,$2,'cleanup_objects',$3,$4,clock_timestamp()+interval '10 minutes')")
            .bind(Uuid::new_v4()).bind(job.interview_id).bind(json!({"keys":[key]})).bind(format!("clip-cleanup:{id}")).execute(pool).await.map_err(|_|failure("alignment_storage_unavailable",false))?;
        storage.put(&key, &clip.wav).await.map_err(evidence_error)?;
        staged.push(workflow::AlignedClip {
            id,
            source_id: clip.source_id.clone(),
            sha256: digest(&clip.wav),
            object_key: key,
        });
    }
    let mut updated =
        serde_json::to_value(manifest).map_err(|_| failure("alignment_payload_invalid", true))?;
    for field in ["timeline_key", "metadata_key"] {
        if let Some(value) = original.get(field) {
            updated[field] = value.clone();
        }
    }
    workflow::complete_alignment(pool, job, attempt, &original, updated, &staged)
        .await
        .map_err(|_| failure("alignment_stale", true))?;
    // Cleanup remains durable if this best-effort scheduling update fails.
    let _ = sqlx::query(
        "UPDATE jobs SET available_at=clock_timestamp() WHERE dedupe_key=$1 AND status='queued'",
    )
    .bind(format!("stt-cleanup:{transcript_id}"))
    .execute(pool)
    .await;
    Ok(())
}
