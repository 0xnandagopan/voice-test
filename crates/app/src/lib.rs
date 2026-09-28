pub mod auth;
pub mod composition_jobs;
pub mod config;
pub mod error;
pub mod handlers;
pub mod leases;
pub mod live;
pub mod progress;
pub mod relay;
pub mod review;
pub mod voice_hook;
pub mod workflow;
use axum::{
    Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use config::Config;
use error::ApiError;
use sqlx::PgPool;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tower_http::services::{ServeDir, ServeFile};
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    attempts: Arc<Mutex<Vec<Instant>>>,
    pub controlled_voice_origin: Option<String>,
}
impl AppState {
    pub fn new(pool: PgPool, config: Config) -> Self {
        Self {
            pool,
            config: Arc::new(config),
            attempts: Arc::new(Mutex::new(Vec::new())),
            controlled_voice_origin: None,
        }
    }
    pub fn with_controlled_voice(mut self, origin: String) -> Result<Self, &'static str> {
        let url =
            url::Url::parse(&origin).map_err(|_| "VOICE_PUBLIC_ORIGIN must be an HTTPS origin")?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || self.config.voice_api_key.is_none()
        {
            return Err(
                "Controlled voice requires VOICE_AGENT_API_KEY and an HTTPS VOICE_PUBLIC_ORIGIN",
            );
        }
        self.controlled_voice_origin = Some(origin.trim_end_matches('/').into());
        Ok(self)
    }
    pub fn limit_auth(&self) -> Result<(), ApiError> {
        let mut attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        attempts.retain(|at| at.elapsed() < Duration::from_secs(60));
        if attempts.len() >= 30 {
            return Err(ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "Too many access attempts. Try again shortly.",
            ));
        }
        attempts.push(Instant::now());
        Ok(())
    }
}
async fn policy(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) && request
        .headers()
        .get(header::ORIGIN)
        .and_then(|h| h.to_str().ok())
        != Some(state.config.origin.as_str())
    {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "invalid_origin",
            "This request must come from the application.",
        ));
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    Ok(response)
}
pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/health", get(handlers::health))
        .route("/api/ready", get(handlers::ready))
        .route("/api/operator/login", post(handlers::login))
        .route("/api/operator/logout", post(handlers::logout))
        .route("/api/operator/me", get(handlers::me))
        .route(
            "/api/operator/invitations",
            get(handlers::list).post(handlers::invite),
        )
        .route(
            "/api/operator/invitations/{id}/revoke",
            post(handlers::revoke),
        )
        .route("/api/customer/exchange", post(handlers::exchange))
        .route("/api/customer/session", get(handlers::session))
        .route("/api/customer/consent", post(handlers::consent))
        .route("/api/customer/start", post(handlers::start))
        .route("/api/customer/interviews/{id}/live", get(live::connect))
        .route(
            "/api/customer/interviews/{id}/workflow",
            get(review::customer_view).post(review::customer_command),
        )
        .route(
            "/api/operator/interviews/{id}/workflow",
            get(review::operator_view).post(review::operator_command),
        )
        .route(
            "/api/customer/interviews/{id}/evidence",
            get(review::customer_evidence),
        )
        .route(
            "/api/operator/interviews/{id}/evidence",
            get(review::operator_evidence),
        )
        .route(
            "/api/customer/interviews/{id}/sources/{source}/audio",
            get(review::customer_audio),
        )
        .route(
            "/api/operator/interviews/{id}/sources/{source}/audio",
            get(review::operator_audio),
        )
        .route(
            "/api/customer/interviews/{id}/recovery",
            get(review::recovery),
        )
        .route(
            "/api/customer/interviews/{id}/recovery/confirm",
            post(review::confirm_recovery),
        )
        .route(
            "/api/customer/interviews/{id}/generate",
            post(review::generate),
        )
        .route("/api/customer/interviews/{id}/retry", post(review::retry))
        .route("/api/public/{id}", get(review::public_snapshot))
        .route("/api/operator/interviews/{id}/export", get(review::export))
        .route(
            "/api/{*path}",
            get(|| async {
                ApiError(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "This endpoint is unavailable.",
                )
            })
            .post(|| async {
                ApiError(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "This endpoint is unavailable.",
                )
            }),
        )
        .layer(DefaultBodyLimit::max(128 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), policy))
        .with_state(state.clone());
    // Machine-authenticated provider callback uses a scoped Bearer token, not
    // browser cookies. Keep it outside the browser Origin policy.
    api.merge(voice_hook::router(state))
}
pub fn with_static(app: Router, dir: &str) -> Router {
    app.fallback_service(
        ServeDir::new(dir).not_found_service(ServeFile::new(format!("{dir}/index.html"))),
    )
}
