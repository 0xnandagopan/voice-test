use crate::{AppState, error::ApiError};
use axum::http::{HeaderMap, HeaderValue, header};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;
pub fn hash_secret(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}
pub fn random_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}
pub fn invitation_secret(key: &str, id: Uuid) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key size");
    mac.update(b"v0-invitation:");
    mac.update(id.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|h| h.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .find(|(key, value)| *key == name && value.len() == 64)
        .map(|(_, value)| value.to_owned())
}
pub fn session_cookie(name: &str, token: &str, secure: bool, max_age: i64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{}",
        if secure { "; Secure" } else { "" }
    ))
    .expect("generated cookie is ASCII")
}
pub async fn operator(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let token = cookie(headers, "operator_session").ok_or_else(ApiError::unauthorized)?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE token_hash=$1 AND role='operator' AND expires_at>now())").bind(hash_secret(&token)).fetch_one(&state.pool).await?;
    if exists {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
    }
}
pub async fn customer(state: &AppState, headers: &HeaderMap) -> Result<Uuid, ApiError> {
    let token = cookie(headers, "customer_session").ok_or_else(ApiError::unauthorized)?;
    let row = sqlx::query("SELECT s.interview_id, i.expires_at, i.deleted_at, i.state FROM sessions s JOIN interviews i ON i.id=s.interview_id WHERE s.token_hash=$1 AND s.role='customer' AND s.expires_at>now()").bind(hash_secret(&token)).fetch_optional(&state.pool).await?.ok_or_else(ApiError::unauthorized)?;
    let expires: DateTime<Utc> = row.get("expires_at");
    let deleted: Option<DateTime<Utc>> = row.get("deleted_at");
    let status: String = row.get("state");
    if expires <= Utc::now()
        || deleted.is_some()
        || matches!(status.as_str(), "revoked" | "deleted")
    {
        return Err(ApiError::expired());
    }
    Ok(row.get("interview_id"))
}
