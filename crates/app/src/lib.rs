pub mod auth;
pub mod config;
pub mod error;
pub mod handlers;
pub mod leases;
pub mod progress;
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
}
impl AppState {
    pub fn new(pool: PgPool, config: Config) -> Self {
        Self {
            pool,
            config: Arc::new(config),
            attempts: Arc::new(Mutex::new(Vec::new())),
        }
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
        .layer(DefaultBodyLimit::max(16 * 1024))
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
