-- Keep evidence/content/check/approval/publication independent of interview access.
-- These are authority interfaces, not enabled product routes.
CREATE TABLE workflow_state (
    interview_id uuid PRIMARY KEY REFERENCES interviews(id),
    value jsonb NOT NULL,
    evidence_fingerprint text NOT NULL DEFAULT '',
    source_ids jsonb NOT NULL DEFAULT '[]'
);
CREATE TABLE workflow_receipts (
    interview_id uuid NOT NULL REFERENCES interviews(id),
    actor_hash text NOT NULL,
    request_id uuid NOT NULL,
    request_hash text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(interview_id, actor_hash, request_id)
);
CREATE TABLE workflow_support_results (
    interview_id uuid NOT NULL REFERENCES interviews(id),
    content_revision bigint NOT NULL,
    evidence_revision bigint NOT NULL,
    result jsonb NOT NULL,
    PRIMARY KEY(interview_id, content_revision, evidence_revision)
);
CREATE TABLE workflow_clips (
    id uuid PRIMARY KEY,
    interview_id uuid NOT NULL REFERENCES interviews(id),
    evidence_revision bigint NOT NULL,
    sha256 text NOT NULL CHECK(length(sha256)=64),
    source_id text NOT NULL,
    object_key text NOT NULL,
    ready boolean NOT NULL DEFAULT false
);
ALTER TABLE interviews ADD COLUMN progress_revision bigint NOT NULL DEFAULT 0;
ALTER TABLE interviews ADD COLUMN completed_answers integer NOT NULL DEFAULT 0 CHECK(completed_answers>=0);
ALTER TABLE interviews ADD COLUMN incomplete_turn text;
ALTER TABLE interviews ADD CONSTRAINT bounded_followups CHECK (
    COALESCE(jsonb_typeof(followup_counts)='array' AND jsonb_array_length(followup_counts)=3
    AND followup_counts->0 IN ('0'::jsonb,'1'::jsonb,'2'::jsonb)
    AND followup_counts->1 IN ('0'::jsonb,'1'::jsonb,'2'::jsonb)
    AND followup_counts->2 IN ('0'::jsonb,'1'::jsonb,'2'::jsonb),false)
);
CREATE TABLE progress_events (
    interview_id uuid NOT NULL REFERENCES interviews(id),
    attempt_id uuid NOT NULL REFERENCES provider_attempts(id),
    event_id text NOT NULL,
    request_hash text NOT NULL,
    PRIMARY KEY(attempt_id, event_id)
);
ALTER TABLE provider_attempts ADD COLUMN control_mode text NOT NULL DEFAULT 'managed'
    CHECK(control_mode IN ('managed','custom'));
ALTER TABLE provider_attempts ADD COLUMN product_end_reason text
    CHECK(product_end_reason IN ('explicit_finish','explicit_stop','transport_lost','budget_exhausted','discard'));
CREATE TABLE voice_hook_bindings (
    attempt_id uuid PRIMARY KEY REFERENCES provider_attempts(id),
    secret_hash text NOT NULL UNIQUE,
    binding_generation bigint NOT NULL,
    processed_user_count integer NOT NULL DEFAULT -1,
    last_request_hash text,
    last_permit_id uuid
);
CREATE TABLE question_permits (
    id uuid PRIMARY KEY,
    attempt_id uuid NOT NULL REFERENCES provider_attempts(id),
    request_hash text NOT NULL,
    progress_revision bigint NOT NULL,
    plan jsonb NOT NULL,
    UNIQUE(attempt_id,request_hash)
);
CREATE TABLE question_control_receipts (
    attempt_id uuid NOT NULL REFERENCES provider_attempts(id),
    request_id uuid NOT NULL,
    request_hash text NOT NULL,
    permit_id uuid NOT NULL REFERENCES question_permits(id),
    PRIMARY KEY(attempt_id,request_id)
);
CREATE TABLE voice_answer_permits (
    attempt_id uuid NOT NULL REFERENCES provider_attempts(id),
    event_id text NOT NULL,
    question_permit_id uuid NOT NULL REFERENCES question_permits(id),
    answer_hash text NOT NULL,
    consumed boolean NOT NULL DEFAULT false,
    PRIMARY KEY(attempt_id,event_id),
    UNIQUE(attempt_id,question_permit_id)
);
