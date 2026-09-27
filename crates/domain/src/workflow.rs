//! Server command contracts. Deserializing a command never authorizes its caller.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Content {
    pub text: String,
    pub attribution: String,
    /// Empty by default: text approval never implicitly opts into audio.
    pub clips: Vec<ClipSelection>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipSelection {
    pub id: Uuid,
    pub sha256: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    #[default]
    Pending,
    Supported,
    Unsupported,
    Ambiguous,
    Failed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revisions {
    pub workflow: i64,
    pub content: i64,
    pub evidence: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCommand {
    pub interview_id: Uuid,
    pub request_id: Uuid,
    pub expected: Revisions,
    pub action: WorkflowAction,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowAction {
    Save { content: Content },
    CorrectTranscript { source_id: String, text: String },
    Approve,
    Decline,
    Publish { approval_id: Uuid },
    Unpublish,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalSnapshot {
    pub id: Uuid,
    pub content_revision: i64,
    pub evidence_revision: i64,
    pub content: Content,
    pub approved_at: chrono::DateTime<chrono::Utc>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowView {
    pub revisions: Revisions,
    pub evidence_available: bool,
    pub content: Option<Content>,
    pub check: CheckStatus,
    pub approval: Option<ApprovalSnapshot>,
    pub published_approval_id: Option<Uuid>,
    pub declined: bool,
    pub transcript_corrections: std::collections::BTreeMap<String, String>,
}
impl Default for WorkflowView {
    fn default() -> Self {
        Self {
            revisions: Revisions {
                workflow: 0,
                content: 0,
                evidence: 0,
            },
            evidence_available: false,
            content: None,
            check: CheckStatus::Pending,
            approval: None,
            published_approval_id: None,
            declined: false,
            transcript_corrections: Default::default(),
        }
    }
}
/// A response records command receipt separately from the current authoritative state.
/// Replaying a receipt after an edit cannot reapply an old approval/publication.
#[derive(Debug, Serialize, Deserialize)]
pub struct CommandResult {
    pub request_id: Uuid,
    pub replayed: bool,
    pub state: WorkflowView,
}
/// Trusted support worker result, never accepted from a browser or voice model tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupportResult {
    pub content_revision: i64,
    pub evidence_revision: i64,
    pub status: CheckStatus,
    pub all_substantive_claims_checked: bool,
    pub source_ids: Vec<String>,
    pub model: String,
    pub prompt_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupportJob {
    pub content_revision: i64,
    pub evidence_revision: i64,
    pub content_hash: String,
}

/// Reserved request contract; not an enabled route until the voice gates pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceControlRequest {
    pub interview_id: Uuid,
    pub request_id: Uuid,
    pub expected_revision: i64,
    pub expected_progress_revision: i64,
    pub lease_id: Uuid,
    pub lease_generation: i64,
    pub action: VoiceAction,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceAction {
    Resume,
    Skip,
    Repeat,
    Stop,
    Finish,
    Discard,
}

/// References original provider-attempt coordinates, never a synthesized clock.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLocator {
    pub interview_id: Uuid,
    pub attempt_id: Uuid,
    pub source_id: String,
    pub evidence_revision: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryRequest {
    pub interview_id: Uuid,
    pub request_id: Uuid,
    pub expected: Revisions,
    pub job_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct InterviewProgress {
    pub revision: i64,
    pub topic: u8,
    pub followups: [u8; 3],
    pub completed_answers: u32,
    pub incomplete_turn: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProgressAction {
    Answer { follow_up: bool },
    Skip,
    Repeat,
    Incomplete { turn_id: String },
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptEndReason {
    ExplicitFinish,
    ExplicitStop,
    TransportLost,
    BudgetExhausted,
    Discard,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressCommand {
    pub interview_id: Uuid,
    pub lease_id: Uuid,
    pub lease_generation: i64,
    pub attempt_id: Uuid,
    pub event_id: String,
    pub expected_revision: i64,
    pub action: ProgressAction,
}
