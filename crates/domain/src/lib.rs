use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const CONSENT_POLICY_VERSION: &str = "recording-v1";
pub const INTERVIEW_BUDGET_SECONDS: i32 = 360;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    pub id: Uuid,
    pub customer_label: String,
    pub project_context: String,
    pub agency_name: String,
    pub state: String,
    pub revision: i64,
    pub consented_at: Option<DateTime<Utc>>,
    pub consent_policy_version: String,
    pub expires_at: DateTime<Utc>,
    pub remaining_seconds: i32,
    pub voice_available: bool,
}
