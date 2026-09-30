use crate::{AppState, auth, error::ApiError};
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Row, postgres::PgRow};
use uuid::Uuid;
use v0_domain::{CONSENT_POLICY_VERSION, SessionView};

pub async fn health() -> Json<Value> {
    Json(json!({"status":"ok"}))
}
pub async fn ready(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    sqlx::query("SELECT 1 FROM interviews LIMIT 1")
        .execute(&state.pool)
        .await?;
    Ok(Json(
        json!({"status":"ready","voice_available":state.controlled_voice_origin.is_some()}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    username: String,
    password: String,
}
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Login>,
) -> Result<Response, ApiError> {
    state.limit_auth()?;
    if input.username.len() > 120 || input.password.len() > 1024 {
        return Err(ApiError::unauthorized());
    }
    let hash = state.config.operator_password_hash.clone();
    let valid_user = input.username == state.config.operator_username;
    let valid = tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash).ok().is_some_and(|parsed| {
            Argon2::default()
                .verify_password(input.password.as_bytes(), &parsed)
                .is_ok()
        })
    })
    .await
    .map_err(|_| ApiError::unauthorized())?;
    if !valid || !valid_user {
        return Err(ApiError::unauthorized());
    }
    let token = auth::random_secret();
    let mut tx = state.pool.begin().await?;
    if let Some(old) = auth::cookie(&headers, "operator_session") {
        sqlx::query("DELETE FROM sessions WHERE token_hash=$1 AND role='operator'")
            .bind(auth::hash_secret(&old))
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("INSERT INTO sessions(token_hash, role, expires_at) VALUES ($1,'operator',now()+interval '8 hours')")
        .bind(auth::hash_secret(&token)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok((
        [(
            header::SET_COOKIE,
            auth::session_cookie(
                "operator_session",
                &token,
                state.config.secure_cookie,
                28800,
            ),
        )],
        Json(json!({"ok":true})),
    )
        .into_response())
}
pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(token) = auth::cookie(&headers, "operator_session") {
        sqlx::query("DELETE FROM sessions WHERE token_hash=$1 AND role='operator'")
            .bind(auth::hash_secret(&token))
            .execute(&state.pool)
            .await?;
    }
    Ok((
        [(
            header::SET_COOKIE,
            auth::session_cookie("operator_session", "", state.config.secure_cookie, 0),
        )],
        Json(json!({"ok":true})),
    )
        .into_response())
}
pub async fn me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    auth::operator(&state, &headers).await?;
    Ok(Json(json!({"username":state.config.operator_username})))
}
fn view(state: &AppState, row: &PgRow) -> SessionView {
    SessionView {
        id: row.get("id"),
        customer_label: row.get("customer_label"),
        project_context: row.get("project_context"),
        agency_name: state.config.agency_name.clone(),
        state: row.get("state"),
        revision: row.get("revision"),
        consented_at: row.get("consented_at"),
        consent_policy_version: CONSENT_POLICY_VERSION.into(),
        expires_at: row.get("expires_at"),
        remaining_seconds: {
            let elapsed = row
                .get::<Option<DateTime<Utc>>, _>("active_since")
                .map(|at| {
                    let until = row
                        .get::<Option<DateTime<Utc>>, _>("lease_expires_at")
                        .unwrap_or_else(Utc::now)
                        .min(Utc::now());
                    ((until - at).num_milliseconds().clamp(0, 360000) / 1000) as i32
                })
                .unwrap_or(0);
            (360 - row.get::<i32, _>("time_consumed_seconds") - elapsed).max(0)
        },
        voice_available: state.controlled_voice_origin.is_some()
            && matches!(
                row.get::<String, _>("interview_preparation").as_str(),
                "not_required" | "ready"
            ),
        interview_preparation: row.get("interview_preparation"),
    }
}
fn available(row: &PgRow) -> Result<(), ApiError> {
    let status: String = row.get("state");
    if row.get::<DateTime<Utc>, _>("expires_at") <= Utc::now()
        || row.get::<Option<DateTime<Utc>>, _>("deleted_at").is_some()
        || matches!(status.as_str(), "revoked" | "deleted")
    {
        return Err(ApiError::expired());
    }
    Ok(())
}
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    auth::operator(&state, &headers).await?;
    let rows = sqlx::query(
        "SELECT i.*, COALESCE(i.expires_at>clock_timestamp() AND i.state NOT IN ('revoked','deleted') AND w.value->>'published_approval_id'=w.value->'approval'->>'id' AND w.value->'approval'->'content_revision'=w.value->'revisions'->'content' AND w.value->'approval'->'evidence_revision'=w.value->'revisions'->'evidence',false) AS published FROM interviews i LEFT JOIN workflow_state w ON w.interview_id=i.id WHERE i.deleted_at IS NULL ORDER BY i.created_at DESC LIMIT 200",
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({"invitations":rows.iter().map(|r| {
            let mut item = json!(view(&state,r));
            item["published"] = json!(r.get::<bool,_>("published"));
            item
        }).collect::<Vec<_>>()})))
}
/// Return the original invitation only to the authenticated operator. Tokens
/// stay out of list responses and are never persisted in browser storage.
pub async fn invitation_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    auth::operator(&state, &headers).await?;
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 FOR SHARE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    available(&row)?;
    // Recheck the operator after waiting on a concurrent revocation/delete.
    let session = auth::cookie(&headers, "operator_session").ok_or_else(ApiError::unauthorized)?;
    let authorized: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE token_hash=$1 AND role='operator' AND expires_at>clock_timestamp())")
        .bind(auth::hash_secret(&session)).fetch_one(&mut *tx).await?;
    if !authorized {
        return Err(ApiError::unauthorized());
    }
    let token = auth::invitation_secret(&state.config.invitation_signing_key, id);
    if auth::hash_secret(&token) != row.get::<String, _>("secret_hash") {
        return Err(ApiError::conflict());
    }
    available(&row)?;
    let result = json!({"private_url":format!("{}/i/{}#token={}",state.config.origin,id,token)});
    tx.commit().await?;
    Ok(Json(result))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invite {
    idempotency_key: Uuid,
    customer_label: String,
    project_context: String,
    #[serde(default)]
    context_attachments: Vec<v0_composition::ContextAttachment>,
}
pub async fn invite(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Invite>,
) -> Result<Json<Value>, ApiError> {
    auth::operator(&state, &headers).await?;
    let label = input.customer_label.trim();
    let context = input.project_context.trim();
    if label.is_empty()
        || label.chars().count() > 120
        || context.is_empty()
        || context.chars().count() > 2000
        || context
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(ApiError::invalid(
            "Provide a customer label and project context within the stated limits.",
        ));
    }
    crate::interview_context::validate_attachments(&input.context_attachments)?;
    let context_hash = crate::interview_context::context_hash(context, &input.context_attachments);
    // Preserve legacy idempotency hashes for attachment-free invitations.
    let request_hash = auth::hash_secret(
        &if input.context_attachments.is_empty() {
            json!([label, context])
        } else {
            json!([label, context, input.context_attachments])
        }
        .to_string(),
    );
    let id = Uuid::new_v4();
    let secret = auth::invitation_secret(&state.config.invitation_signing_key, id);
    let mut tx = state.pool.begin().await?;
    sqlx::query("INSERT INTO interviews(id,customer_label,project_context,secret_hash,idempotency_key,request_hash,expires_at,context_attachments,context_hash,interview_preparation) VALUES($1,$2,$3,$4,$5,$6,now()+interval '14 days',$7,$8,'queued') ON CONFLICT (idempotency_key) DO NOTHING")
        .bind(id).bind(label).bind(context).bind(auth::hash_secret(&secret)).bind(input.idempotency_key).bind(&request_hash).bind(json!(input.context_attachments)).bind(&context_hash).execute(&mut *tx).await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE idempotency_key=$1 FOR UPDATE")
        .bind(input.idempotency_key)
        .fetch_one(&mut *tx)
        .await?;
    if row.get::<String, _>("request_hash") != request_hash {
        return Err(ApiError::conflict());
    }
    available(&row)?;
    let id: Uuid = row.get("id");
    if row.get::<String, _>("interview_preparation") == "queued" {
        crate::interview_context::enqueue(&mut tx, id, &row.get::<String, _>("context_hash"))
            .await?;
    }
    let token = auth::invitation_secret(&state.config.invitation_signing_key, id);
    // Key rotation invalidates old links; never return a newly derived but unusable token.
    if auth::hash_secret(&token) != row.get::<String, _>("secret_hash") {
        return Err(ApiError::conflict());
    }
    sqlx::query("INSERT INTO audit_events(interview_id,event) SELECT $1,'invitation_created' WHERE NOT EXISTS(SELECT 1 FROM audit_events WHERE interview_id=$1 AND event='invitation_created')").bind(id).execute(&mut *tx).await?;
    let response = json!({"invitation":view(&state,&row),"private_url":format!("{}/i/{}#token={}",state.config.origin,id,token)});
    tx.commit().await?;
    Ok(Json(response))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revision {
    expected_revision: i64,
}
pub async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<Revision>,
) -> Result<Json<Value>, ApiError> {
    auth::operator(&state, &headers).await?;
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    if row.get::<i64, _>("revision") != input.expected_revision {
        return Err(ApiError::conflict());
    }
    let status: String = row.get("state");
    if !matches!(status.as_str(), "invited" | "consented") {
        return Err(ApiError::invalid(
            "Only unused invitations can be revoked here.",
        ));
    }
    sqlx::query(
        "UPDATE interviews SET state='revoked',revision=revision+1,updated_at=now() WHERE id=$1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM sessions WHERE interview_id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO audit_events(interview_id,event) VALUES($1,'invitation_revoked')")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exchange {
    invitation_id: Uuid,
    token: String,
}
pub async fn exchange(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Exchange>,
) -> Result<Response, ApiError> {
    state.limit_auth()?;
    if input.token.len() != 64 {
        return Err(ApiError::unauthorized());
    }
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 AND secret_hash=$2 FOR UPDATE")
        .bind(input.invitation_id)
        .bind(auth::hash_secret(&input.token))
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    available(&row)?;
    let token = auth::random_secret();
    if let Some(old) = auth::cookie(&headers, "customer_session") {
        sqlx::query("DELETE FROM sessions WHERE token_hash=$1 AND role='customer'")
            .bind(auth::hash_secret(&old))
            .execute(&mut *tx)
            .await?;
    }
    let expiry: DateTime<Utc> = if row
        .get::<Option<DateTime<Utc>>, _>("completed_at")
        .is_none()
    {
        sqlx::query_scalar("UPDATE interviews SET expires_at=clock_timestamp()+interval '14 days',updated_at=clock_timestamp() WHERE id=$1 RETURNING expires_at").bind(input.invitation_id).fetch_one(&mut *tx).await?
    } else {
        row.get("expires_at")
    };
    let seconds = (expiry - Utc::now()).num_seconds().max(0);
    sqlx::query(
        "INSERT INTO sessions(token_hash,role,interview_id,expires_at) VALUES($1,'customer',$2,$3)",
    )
    .bind(auth::hash_secret(&token))
    .bind(input.invitation_id)
    .bind(expiry)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((
        [(
            header::SET_COOKIE,
            auth::session_cookie(
                "customer_session",
                &token,
                state.config.secure_cookie,
                seconds,
            ),
        )],
        Json(json!({"ok":true})),
    )
        .into_response())
}
pub async fn session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SessionView>, ApiError> {
    let id = auth::customer(&state, &headers).await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1")
        .bind(id)
        .fetch_one(&state.pool)
        .await?;
    available(&row)?;
    Ok(Json(view(&state, &row)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Consent {
    interview_id: Uuid,
    policy_version: String,
}
pub async fn consent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Consent>,
) -> Result<Response, ApiError> {
    let id = auth::customer(&state, &headers).await?;
    if id != input.interview_id {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "scope_changed",
            "This tab belongs to a different invitation. Reopen its private link.",
        ));
    }
    if input.policy_version != CONSENT_POLICY_VERSION {
        return Err(ApiError::invalid(
            "Review the current recording policy before consenting.",
        ));
    }
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    available(&row)?;
    if row
        .get::<Option<DateTime<Utc>>, _>("consented_at")
        .is_none()
    {
        if row.get::<String, _>("state") != "invited" {
            return Err(ApiError::conflict());
        }
        sqlx::query("UPDATE interviews SET consented_at=now(),consent_policy_version=$2,state='consented',revision=revision+1,updated_at=now(),expires_at=now()+interval '14 days' WHERE id=$1").bind(id).bind(CONSENT_POLICY_VERSION).execute(&mut *tx).await?;
        sqlx::query("UPDATE sessions SET expires_at=(SELECT expires_at FROM interviews WHERE id=$1) WHERE interview_id=$1").bind(id).execute(&mut *tx).await?;
        sqlx::query(
            "INSERT INTO audit_events(interview_id,event) VALUES($1,'recording_consented')",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let result = view(&state, &row);
    tx.commit().await?;
    let token = auth::cookie(&headers, "customer_session").ok_or_else(ApiError::unauthorized)?;
    let seconds = (result.expires_at - Utc::now()).num_seconds().max(0);
    Ok((
        [(
            header::SET_COOKIE,
            auth::session_cookie(
                "customer_session",
                &token,
                state.config.secure_cookie,
                seconds,
            ),
        )],
        Json(result),
    )
        .into_response())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    interview_id: Uuid,
    expected_revision: i64,
}
pub async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Start>,
) -> Result<Json<Value>, ApiError> {
    let id = auth::customer(&state, &headers).await?;
    if id != input.interview_id {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "scope_changed",
            "This tab belongs to a different invitation. Reopen its private link.",
        ));
    }
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1")
        .bind(id)
        .fetch_one(&state.pool)
        .await?;
    available(&row)?;
    if row.get::<i64, _>("revision") != input.expected_revision {
        return Err(ApiError::conflict());
    }
    if row
        .get::<Option<DateTime<Utc>>, _>("consented_at")
        .is_none()
    {
        return Err(ApiError::invalid("Recording consent is required."));
    }
    // This opt-in is for controlled tests; ordinary customer Start stays gated.
    if state.controlled_voice_origin.is_some() {
        if row.get::<String, _>("state") != "consented"
            || row.get::<i32, _>("time_consumed_seconds") >= 360
        {
            return Err(ApiError::conflict());
        }
        return Ok(Json(
            json!({"ws_url":format!("/api/customer/interviews/{id}/live?expected_revision={}", input.expected_revision)}),
        ));
    }
    if state.config.voice_api_key.is_none() {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "provider_unavailable",
            "The live interview is not configured yet. Your consent is saved.",
        ));
    }
    Err(ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "not_ready",
        "The live interview is awaiting voice and recovery verification. Your consent is saved.",
    ))
}
