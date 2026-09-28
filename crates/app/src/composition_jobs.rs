//! Durable worker orchestration; no model response has customer/operator authority.
use crate::{error::ApiError, workflow};
use sqlx::{PgPool, Row};
use v0_composition::{GatewayClient, GatewayError, GenerationStatus, Verdict};
use v0_domain::workflow::{CheckStatus, SupportResult};
use v0_evidence::jobs::Job;

/// Add entries only with a recorded passing G3 evaluation for this exact pair.
const VALIDATED_MODEL_PROMPTS: &[(&str, &str)] = &[];
pub async fn dispatch(pool: &PgPool, job: &Job, client: &GatewayClient) -> Result<(), JobFailure> {
    let (state, sources) = workflow::composition_input(pool, job.interview_id, job.id, job.token)
        .await
        .map_err(|_| JobFailure::stale())?;
    if job.kind == "generate_draft" {
        let generated = client
            .generate(&sources)
            .await
            .map_err(JobFailure::gateway)?;
        let details = serde_json::json!({"kind":"generate_draft","model":client.model(),"prompt_version":v0_composition::PROMPT_VERSION,"assessment":generated});
        let text = if generated.status == GenerationStatus::Draft {
            Some(generated.text)
        } else {
            None
        };
        workflow::complete_generation_assessed(
            pool,
            job.interview_id,
            job.id,
            job.token,
            text,
            Some(details),
        )
        .await
        .map_err(|_| JobFailure::stale())?;
    } else if job.kind == "support_check" {
        let content = state.content.as_ref().ok_or_else(JobFailure::stale)?;
        let checked = client
            .check(&content.text, &sources)
            .await
            .map_err(JobFailure::gateway)?;
        let quality =
            VALIDATED_MODEL_PROMPTS.contains(&(client.model(), v0_composition::PROMPT_VERSION));
        let status = match checked.verdict {
            Verdict::Unsupported => CheckStatus::Unsupported,
            Verdict::Uncertain => CheckStatus::Ambiguous,
            Verdict::Supported if quality && state.evidence_available => CheckStatus::Supported,
            _ => CheckStatus::Ambiguous,
        };
        let mut ids = checked
            .claims
            .iter()
            .flat_map(|c| c.sources.iter().map(|s| s.source_id.clone()))
            .collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        workflow::complete_support_assessed(
            pool,
            job.interview_id,
            job.id,
            job.token,
            SupportResult {
                content_revision: state.revisions.content,
                evidence_revision: state.revisions.evidence,
                status,
                all_substantive_claims_checked: true,
                source_ids: ids,
                model: client.model().into(),
                prompt_version: v0_composition::PROMPT_VERSION.into(),
            },
            Some(serde_json::json!(checked)),
        )
        .await
        .map_err(|_| JobFailure::stale())?;
    } else {
        return Err(JobFailure::stale());
    }
    Ok(())
}
pub struct JobFailure {
    pub code: &'static str,
    pub retry_after_secs: u64,
    pub terminal: bool,
}
impl JobFailure {
    fn stale() -> Self {
        Self {
            code: "stale_composition",
            retry_after_secs: 0,
            terminal: true,
        }
    }
    fn gateway(error: GatewayError) -> Self {
        match error {
            GatewayError::RateLimited { retry_after_secs } => Self {
                code: "gateway_rate_limited",
                retry_after_secs: retry_after_secs.max(60),
                terminal: false,
            },
            GatewayError::Transport | GatewayError::Deadline | GatewayError::RetryExhausted => {
                Self {
                    code: "gateway_unavailable",
                    retry_after_secs: 300,
                    terminal: false,
                }
            }
            GatewayError::Authentication | GatewayError::Configuration => Self {
                code: "gateway_configuration",
                retry_after_secs: 0,
                terminal: true,
            },
            _ => Self {
                code: "generation_validation_failed",
                retry_after_secs: 0,
                terminal: true,
            },
        }
    }
}
pub async fn fail(pool: &PgPool, job: &Job, failure: JobFailure) -> Result<(), ApiError> {
    // Release job lock before authority. Current worker ownership checked at each step.
    let failed=sqlx::query("UPDATE jobs SET status=CASE WHEN $3 OR attempts>=max_attempts THEN 'failed' ELSE 'queued' END,last_error=$4,available_at=clock_timestamp()+make_interval(secs=>$5),lease_token=NULL,lease_until=NULL WHERE id=$1 AND lease_token=$2 AND status='running' AND lease_until>clock_timestamp() RETURNING status").bind(job.id).bind(job.token).bind(failure.terminal).bind(failure.code).bind(failure.retry_after_secs as f64).fetch_optional(pool).await?;
    if job.kind == "support_check"
        && failed.is_some_and(|r| r.get::<String, _>("status") == "failed")
    {
        let _ = workflow::fail_support_job(pool, job.interview_id, job.id).await;
    }
    Ok(())
}
