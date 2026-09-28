use crate::*;
use reqwest::{
    Client, StatusCode, Url,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::Mutex,
    time::{Instant, sleep_until, timeout},
};

const ENDPOINT: &str = "https://llm-gateway.assemblyai.com/v1/chat/completions";
const MAX_RESPONSE_BYTES: usize = 128 * 1024;

/// Deliberately excludes URLs, provider bodies, credentials and testimonial text.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("invalid gateway configuration")]
    Configuration,
    #[error("invalid composition input")]
    InvalidInput,
    #[error("gateway transport failed")]
    Transport,
    #[error("gateway operation deadline exceeded")]
    Deadline,
    #[error("gateway authentication failed")]
    Authentication,
    #[error("gateway retry budget exhausted")]
    RetryExhausted,
    #[error("gateway rate limit requires delayed retry")]
    RateLimited { retry_after_secs: u64 },
    #[error("gateway account cannot access the selected model")]
    ModelAccessDenied,
    #[error("gateway rejected request")]
    Rejected,
    #[error("gateway response exceeded size limit")]
    ResponseTooLarge,
    #[error("gateway response envelope is invalid")]
    InvalidEnvelope,
    #[error("gateway response did not finish normally")]
    IncompleteResponse,
    #[error("gateway response is not plain JSON")]
    InvalidJson,
    #[error("gateway output does not match the required schema")]
    InvalidSchema,
    #[error("gateway output failed validation")]
    InvalidOutput,
    #[error("explicit request budget exhausted")]
    RequestBudget,
}

#[derive(Clone)]
pub struct GatewayClient {
    client: Client,
    endpoint: Url,
    key: HeaderValue,
    model: String,
    next_request: Arc<Mutex<Instant>>,
    spacing: Duration,
    deadline: Duration,
    request_budget: Option<Arc<AtomicUsize>>,
}

impl GatewayClient {
    /// Reuse clones of ONE client across worker jobs: they share rate scheduling.
    /// Multi-process workers additionally need a shared database rate limiter.
    pub fn new(api_key: String, model: String) -> Result<Self, GatewayError> {
        Self::build(
            api_key,
            model,
            ENDPOINT,
            Duration::from_secs(31),
            Duration::from_secs(240),
            Duration::from_secs(40),
        )
    }

    /// Explicit local mock transport; never accepts remote hosts or redirects.
    /// This constructor has zero pacing so tests do not consume production slots.
    pub fn for_local_test(endpoint: &str, deadline: Duration) -> Result<Self, GatewayError> {
        let url = Url::parse(endpoint).map_err(|_| GatewayError::Configuration)?;
        if url.scheme() != "http" || !matches!(url.host_str(), Some("127.0.0.1") | Some("[::1]")) {
            return Err(GatewayError::Configuration);
        }
        Self::build(
            "test-key".into(),
            "test-model".into(),
            endpoint,
            Duration::ZERO,
            deadline,
            deadline,
        )
    }

    fn build(
        api_key: String,
        model: String,
        endpoint: &str,
        spacing: Duration,
        deadline: Duration,
        request_timeout: Duration,
    ) -> Result<Self, GatewayError> {
        if api_key.trim().is_empty()
            || model.trim().is_empty()
            || model.len() > 200
            || !model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.:/".contains(c))
        {
            return Err(GatewayError::Configuration);
        }
        let mut key = HeaderValue::from_str(&api_key).map_err(|_| GatewayError::Configuration)?;
        key.set_sensitive(true);
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(request_timeout)
            .build()
            .map_err(|_| GatewayError::Configuration)?;
        Ok(Self {
            client,
            endpoint: Url::parse(endpoint).map_err(|_| GatewayError::Configuration)?,
            key,
            model,
            next_request: Arc::new(Mutex::new(Instant::now())),
            spacing,
            deadline,
            request_budget: None,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Cap total dispatched HTTP attempts, including retries, across all clones.
    /// Intended for explicitly bounded live evaluations.
    pub fn with_request_budget(mut self, attempts: usize) -> Self {
        self.request_budget = Some(Arc::new(AtomicUsize::new(attempts)));
        self
    }

    pub async fn generate(&self, sources: &[EvidenceSource]) -> Result<Generation, GatewayError> {
        validation::validate_input(sources)?;
        if sources.is_empty() {
            return Ok(Generation {
                status: GenerationStatus::NoDraft,
                text: String::new(),
                claims: vec![],
                issues: vec!["No recorded evidence is available.".into()],
            });
        }
        let value = self
            .request(GENERATION_PROMPT, json!({"sources": sources}))
            .await?;
        validate_generation(&value, sources)?;
        Ok(value)
    }

    pub async fn check(
        &self,
        candidate: &str,
        sources: &[EvidenceSource],
    ) -> Result<CheckResult, GatewayError> {
        validation::validate_input(sources)?;
        if candidate.trim().is_empty() || candidate.len() > 16 * 1024 {
            return Err(GatewayError::InvalidInput);
        }
        if sources.is_empty() {
            return Ok(CheckResult {
                verdict: Verdict::Unsupported,
                claims: vec![CheckedClaim {
                    text: candidate.to_owned(),
                    verdict: Verdict::Unsupported,
                    sources: vec![],
                    issues: vec!["No recorded evidence supports this text.".into()],
                }],
                issues: vec![],
            });
        }
        let value = self
            .request(
                CHECK_PROMPT,
                json!({"candidate":candidate,"sources":sources}),
            )
            .await?;
        validate_check(&value, candidate, sources)?;
        Ok(value)
    }

    async fn request<T: DeserializeOwned>(
        &self,
        task_prompt: &str,
        input: Value,
    ) -> Result<T, GatewayError> {
        timeout(self.deadline, self.request_inner(task_prompt, input))
            .await
            .map_err(|_| GatewayError::Deadline)?
    }

    async fn request_inner<T: DeserializeOwned>(
        &self,
        task_prompt: &str,
        input: Value,
    ) -> Result<T, GatewayError> {
        // Use prompt-produced JSON across configured models; native schema support
        // must be checked for each selected model before enabling response_format.
        // No fallback list or JSON repair: model changes and malformed output fail closed.
        let body = json!({"model":self.model,"max_tokens":3000,"temperature":0,
            "messages":[{"role":"system","content":format!("{COMMON_PROMPT}\n{task_prompt}")},
            {"role":"user","content":input.to_string()}]});
        for attempt in 0..3 {
            // Hold the queue lock through dispatch scheduling, not the network operation.
            // Cancellation may waste a slot but cannot exceed the configured rate.
            {
                let mut next = self.next_request.lock().await;
                sleep_until(*next).await;
                *next = Instant::now() + self.spacing;
            }
            if let Some(budget) = &self.request_budget {
                budget
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .map_err(|_| GatewayError::RequestBudget)?;
            }
            let sent = self
                .client
                .post(self.endpoint.clone())
                .header(AUTHORIZATION, self.key.clone())
                .json(&body)
                .send()
                .await;
            let mut response = match sent {
                Ok(response) => response,
                Err(_) if attempt < 2 => {
                    self.defer(Duration::from_secs(2_u64.pow(attempt + 1)))
                        .await;
                    continue;
                }
                Err(_) => return Err(GatewayError::Transport),
            };
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                return Err(GatewayError::Authentication);
            }
            if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                let delay = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| {
                        v.parse::<u64>().ok().or_else(|| {
                            httpdate::parse_http_date(v).ok().map(|time| {
                                time.duration_since(std::time::SystemTime::now())
                                    .map(|d| d.as_secs().saturating_add(1))
                                    .unwrap_or(0)
                            })
                        })
                    })
                    .unwrap_or(if status == StatusCode::TOO_MANY_REQUESTS {
                        60
                    } else {
                        2_u64.pow(attempt + 1)
                    });
                // Never retry earlier than a long server-requested delay; return instead.
                if status == StatusCode::TOO_MANY_REQUESTS && (attempt == 2 || delay > 120) {
                    return Err(GatewayError::RateLimited {
                        retry_after_secs: delay,
                    });
                }
                if attempt == 2 || delay > 120 {
                    return Err(GatewayError::RetryExhausted);
                }
                self.defer(Duration::from_secs(delay)).await;
                continue;
            }
            if !status.is_success() {
                // Inspect only bounded provider metadata for a known actionable
                // rejection; never retain or expose the provider's error body.
                if status == StatusCode::BAD_REQUEST {
                    let mut rejection = Vec::new();
                    while let Some(chunk) =
                        response.chunk().await.map_err(|_| GatewayError::Rejected)?
                    {
                        if rejection.len() + chunk.len() > 16 * 1024 {
                            return Err(GatewayError::Rejected);
                        }
                        rejection.extend_from_slice(&chunk);
                    }
                    if let Ok(value) = serde_json::from_slice::<Value>(&rejection)
                        && value["metadata"]["errors"].as_array().is_some_and(|errors| errors.iter().any(|error| {
                            error.as_str().is_some_and(|text| text.eq_ignore_ascii_case("Your account does not have access to this LLM Gateway model"))
                        })) {
                            return Err(GatewayError::ModelAccessDenied);
                    }
                }
                return Err(GatewayError::Rejected);
            }
            if response
                .content_length()
                .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
            {
                return Err(GatewayError::ResponseTooLarge);
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| GatewayError::Transport)?
            {
                if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                    return Err(GatewayError::ResponseTooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            let envelope: Value =
                serde_json::from_slice(&bytes).map_err(|_| GatewayError::InvalidEnvelope)?;
            let choices = envelope
                .get("choices")
                .and_then(Value::as_array)
                .filter(|c| c.len() == 1)
                .ok_or(GatewayError::InvalidEnvelope)?;
            if choices[0].get("finish_reason").and_then(Value::as_str) != Some("stop") {
                return Err(GatewayError::IncompleteResponse);
            }
            let message = &choices[0]["message"];
            if message.get("tool_calls").is_some_and(|v| !v.is_null())
                || message.get("refusal").is_some_and(|v| !v.is_null())
            {
                return Err(GatewayError::InvalidEnvelope);
            }
            let content = message
                .get("content")
                .and_then(Value::as_str)
                .ok_or(GatewayError::InvalidEnvelope)?;
            // Deserialize directly: going through Value would collapse duplicate keys
            // and could hide malformed/ambiguous model output from strict validation.
            return serde_json::from_str(content).map_err(|error| {
                if error.is_data() {
                    GatewayError::InvalidSchema
                } else {
                    GatewayError::InvalidJson
                }
            });
        }
        Err(GatewayError::RetryExhausted)
    }

    async fn defer(&self, delay: Duration) {
        let mut next = self.next_request.lock().await;
        *next = (*next).max(Instant::now() + delay);
    }
}
