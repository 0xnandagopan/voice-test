use std::{env, time::Duration};
use v0_evidence::{Error, jobs, provider::AssemblyHistory, storage::LocalPrivateStorage};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter("v0_worker=info")
        .init();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is required in .env")?)
        .await?;
    let key = env::var("VOICE_AGENT_API_KEY").map_err(
        |_| "Populate VOICE_AGENT_API_KEY in the local .env before running live recovery",
    )?;
    let hosts = env::var("VOICE_ARTIFACT_HOSTS")
        .unwrap_or_else(|_| "s3.amazonaws.com".into())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let provider = AssemblyHistory::new(key, hosts)?;
    let storage = LocalPrivateStorage::new(
        env::var("EVIDENCE_STORAGE_DIR").unwrap_or_else(|_| ".local/private-evidence".into()),
    )
    .await?;
    let validator = v0_evidence::media::FfmpegValidator::new(
        env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".into()),
        env::var("FFPROBE_PATH").unwrap_or_else(|_| "ffprobe".into()),
        env::var("EVIDENCE_WORK_DIR").unwrap_or_else(|_| ".local/evidence-jobs".into()),
    );
    loop {
        if let Some(job) = jobs::claim(&pool).await? {
            let outcome = tokio::time::timeout(
                Duration::from_secs(100),
                jobs::dispatch_with_media(&pool, &job, &provider, &storage, Some(&validator)),
            )
            .await;
            let code = match outcome {
                Ok(Ok(())) => {
                    tracing::info!(job_id = %job.id, "job completed");
                    continue;
                }
                Err(_) => "import_timeout",
                Ok(Err(Error::NotReady)) => "artifacts_not_ready",
                Ok(Err(Error::Invalid(_))) | Ok(Err(Error::Json(_))) => "invalid_artifacts",
                Ok(Err(Error::Http(_))) => "provider_unavailable",
                Ok(Err(Error::Io(_))) => "storage_unavailable",
                Ok(Err(_)) => "import_failed",
            };
            jobs::fail(&pool, &job, code).await?;
            tracing::warn!(job_id = %job.id, error_code = code, "job did not complete");
        } else {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
    }
    Ok(())
}
