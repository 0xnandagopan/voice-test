ALTER TABLE voice_hook_bindings ADD COLUMN delivered_permit_id uuid REFERENCES question_permits(id);
ALTER TABLE voice_answer_permits ADD COLUMN trusted_text text CHECK (octet_length(trusted_text)<=16000);
ALTER TABLE voice_answer_permits ADD COLUMN ordinal bigint GENERATED ALWAYS AS IDENTITY;
ALTER TABLE voice_answer_permits ADD COLUMN response_permit_id uuid REFERENCES question_permits(id);
ALTER TABLE voice_answer_permits DROP CONSTRAINT voice_answer_permits_attempt_id_question_permit_id_key;
CREATE INDEX voice_answer_order ON voice_answer_permits(attempt_id,ordinal);
-- Trusted incomplete markers are attempt-local, never copied from another attempt.
ALTER TABLE provider_attempts ADD COLUMN incomplete_turn_ids jsonb NOT NULL DEFAULT '[]'
    CHECK(jsonb_typeof(incomplete_turn_ids)='array');
ALTER TABLE provider_attempts ADD COLUMN provider_agent_id text;
ALTER TABLE provider_attempts ADD COLUMN provider_agent_name text;
CREATE TABLE recovery_confirmations (
 interview_id uuid NOT NULL REFERENCES interviews(id),
 request_id uuid NOT NULL,
 actor_hash text NOT NULL,
 request_hash text NOT NULL,
 evidence_revision bigint NOT NULL,
 confirmed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY(interview_id,request_id)
);
