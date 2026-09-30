mod agent_cleanup;
use std::{env, time::Duration};
use v0_app::alignment_jobs;
use v0_app::composition_jobs::{self, JobFailure};
use v0_composition::GatewayClient;
use v0_evidence::{Error, jobs, provider::AssemblyHistory, storage::LocalPrivateStorage};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    v0_app::config::load_dotenv()?;
    tracing_subscriber::fmt()
        .with_env_filter("v0_worker=info")
        .init();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is required in .env")?)
        .await?;
    let recovery_pool = pool.clone();
    let _recovery = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            if v0_app::recovery_maintenance::reap_expired(&recovery_pool)
                .await
                .is_err()
            {
                tracing::warn!(
                    error_code = "relay_recovery_failed",
                    "Recovery maintenance will retry"
                );
            }
        }
    });
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
    let provider = AssemblyHistory::new(key.clone(), hosts)?;
    let storage = LocalPrivateStorage::new(
        env::var("EVIDENCE_STORAGE_DIR").unwrap_or_else(|_| ".local/private-evidence".into()),
    )
    .await?;
    let validator = v0_evidence::media::FfmpegValidator::new(
        env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".into()),
        env::var("FFPROBE_PATH").unwrap_or_else(|_| "ffprobe".into()),
        env::var("EVIDENCE_WORK_DIR").unwrap_or_else(|_| ".local/evidence-jobs".into()),
    );
    let transcriber = v0_evidence::stt::AssemblyAiTranscriber::new(key.clone())?;
    let gateway = env::var("GATEWAY_MODEL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .and_then(|model| GatewayClient::new(key.clone(), model).ok());
    loop {
        alignment_jobs::enqueue_missing(&pool).await?;
        v0_app::interview_context::reconcile(&pool).await?;
        let claimed = jobs::claim(&pool).await?;
        v0_app::workflow::reconcile_failed_support(&pool)
            .await
            .map_err(|_| "Could not reconcile job failures")?;
        if let Some(job) = claimed {
            let run = async {
                match job.kind.as_str() {
                    "align_evidence" | "delete_alignment_transcript" => {
                        alignment_jobs::dispatch(&pool, &job, &transcriber, &storage, &validator)
                            .await
                    }
                    "prepare_interview" => match gateway.as_ref() {
                        Some(client) => {
                            v0_app::interview_context::dispatch(&pool, &job, client).await
                        }
                        None => Err(JobFailure {
                            code: "gateway_configuration",
                            retry_after_secs: 0,
                            terminal: true,
                        }),
                    },
                    "generate_draft" | "support_check" => match gateway.as_ref() {
                        Some(client) => composition_jobs::dispatch(&pool, &job, client).await,
                        None => Err(JobFailure {
                            code: "gateway_configuration",
                            retry_after_secs: 0,
                            terminal: true,
                        }),
                    },
                    "delete_provider_agent" => agent_cleanup::dispatch(&pool, &job, &key)
                        .await
                        .map_err(|_| JobFailure {
                            code: "provider_agent_cleanup_pending",
                            retry_after_secs: 60,
                            terminal: false,
                        }),
                    "import_evidence" | "cleanup_objects" => jobs::dispatch_with_media(
                        &pool,
                        &job,
                        &provider,
                        &storage,
                        Some(&validator),
                    )
                    .await
                    .map_err(|error| JobFailure {
                        code: match error {
                            Error::NotReady => "artifacts_not_ready",
                            Error::Invalid(_) | Error::Json(_) => "invalid_artifacts",
                            Error::Http(_) => "provider_unavailable",
                            Error::Io(_) => "storage_unavailable",
                            _ => "import_failed",
                        },
                        retry_after_secs: 30,
                        terminal: false,
                    }),
                    _ => Err(JobFailure {
                        code: "unsupported_job",
                        retry_after_secs: 0,
                        terminal: true,
                    }),
                }
            };
            let outcome = {
                tokio::pin!(run);
                let deadline = tokio::time::sleep(Duration::from_secs(300));
                tokio::pin!(deadline);
                let renew = async {
                    let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
                    loop {
                        heartbeat.tick().await;
                        let result = sqlx::query("UPDATE jobs SET lease_until=clock_timestamp()+interval '120 seconds' WHERE id=$1 AND status='running' AND lease_token=$2 AND lease_until>clock_timestamp()")
                            .bind(job.id).bind(job.token).execute(&pool).await;
                        if !result.is_ok_and(|r| r.rows_affected() == 1) {
                            break;
                        }
                    }
                };
                tokio::pin!(renew);
                tokio::select! {
                    result = &mut run => Some(result),
                    _ = tokio::signal::ctrl_c() => return Ok(()),
                    _ = &mut deadline => Some(Err(JobFailure {code:"job_timeout",retry_after_secs:60,terminal:false})),
                    _ = &mut renew => None,
                }
            };
            match outcome {
                Some(Ok(())) => tracing::info!(job_id=%job.id,"job completed"),
                Some(Err(failure)) => {
                    tracing::warn!(job_id=%job.id,error_code=failure.code,"job did not complete");
                    if job.kind == "import_evidence" {
                        jobs::fail(&pool, &job, failure.code).await?;
                    } else {
                        composition_jobs::fail(&pool, &job, failure)
                            .await
                            .map_err(|_| "Could not persist job failure")?;
                    }
                }
                None => tracing::warn!(job_id=%job.id,"job lease lost"),
            }
        } else {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
    }
    Ok(())
}
