//! Controlled integration route; normal application instances leave this disabled.
use crate::{AppState, auth, error::ApiError, relay};
use axum::{
    extract::{Path, Query, State, ws::WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use serde::Deserialize;
use uuid::Uuid;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connect {
    expected_revision: i64,
}
pub async fn connect(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(input): Query<Connect>,
    headers: HeaderMap,
    socket: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    if headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        != Some(state.config.origin.as_str())
    {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "invalid_origin",
            "Use the application to start the interview.",
        ));
    }
    let origin = state.controlled_voice_origin.clone().ok_or(ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "not_ready",
        "Live voice awaits verification.",
    ))?;
    let key = state.config.voice_api_key.clone().ok_or(ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "provider_unavailable",
        "Live voice is not configured.",
    ))?;
    if auth::customer(&state, &headers).await? != id {
        return Err(ApiError::unauthorized());
    }
    let token = auth::cookie(&headers, "customer_session").ok_or_else(ApiError::unauthorized)?;
    Ok(socket
        .max_message_size(96 * 1024)
        .max_frame_size(96 * 1024)
        .on_upgrade(move |ws| {
            relay::run(
                ws,
                state.pool,
                token,
                id,
                input.expected_revision,
                relay::RelayConfig {
                    api_key: key,
                    public_origin: origin,
                },
            )
        }))
}
