CREATE TABLE interviews (
    id uuid PRIMARY KEY,
    customer_label text NOT NULL CHECK (char_length(customer_label) BETWEEN 1 AND 120),
    project_context text NOT NULL CHECK (char_length(project_context) BETWEEN 1 AND 2000),
    secret_hash text NOT NULL UNIQUE,
    idempotency_key uuid NOT NULL UNIQUE,
    request_hash text NOT NULL,
    state text NOT NULL DEFAULT 'invited',
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    consented_at timestamptz,
    consent_policy_version text,
    expires_at timestamptz NOT NULL,
    completed_at timestamptz,
    deleted_at timestamptz,
    time_consumed_seconds integer NOT NULL DEFAULT 0 CHECK (time_consumed_seconds BETWEEN 0 AND 360),
    topic_index integer NOT NULL DEFAULT 0 CHECK (topic_index BETWEEN 0 AND 3),
    followup_counts jsonb NOT NULL DEFAULT '[0,0,0]',
    lease_id uuid,
    lease_generation bigint NOT NULL DEFAULT 0,
    lease_expires_at timestamptz,
    active_since timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE sessions (
    token_hash text PRIMARY KEY,
    role text NOT NULL CHECK (role IN ('operator','customer')),
    interview_id uuid REFERENCES interviews(id),
    expires_at timestamptz NOT NULL,
    CHECK ((role = 'operator' AND interview_id IS NULL) OR (role = 'customer' AND interview_id IS NOT NULL))
);

CREATE TABLE provider_attempts (
    id uuid PRIMARY KEY,
    interview_id uuid NOT NULL REFERENCES interviews(id),
    provider_session_id text UNIQUE,
    state text NOT NULL DEFAULT 'connecting',
    lease_generation bigint NOT NULL,
    started_at timestamptz NOT NULL DEFAULT now(),
    ended_at timestamptz
);

CREATE TABLE jobs (
    id uuid PRIMARY KEY,
    interview_id uuid NOT NULL REFERENCES interviews(id),
    kind text NOT NULL,
    payload jsonb NOT NULL,
    dedupe_key text NOT NULL UNIQUE,
    status text NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','running','succeeded','failed','cancelled')),
    attempts integer NOT NULL DEFAULT 0,
    max_attempts integer NOT NULL DEFAULT 5 CHECK (max_attempts > 0),
    available_at timestamptz NOT NULL DEFAULT now(),
    lease_token uuid,
    lease_until timestamptz,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX jobs_ready ON jobs (available_at) WHERE status IN ('queued','running');

CREATE TABLE evidence_imports (
    id uuid PRIMARY KEY,
    interview_id uuid NOT NULL REFERENCES interviews(id),
    provider_attempt_id uuid NOT NULL UNIQUE REFERENCES provider_attempts(id),
    evidence_revision bigint NOT NULL DEFAULT 1,
    manifest jsonb NOT NULL,
    recording_key text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE audit_events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    interview_id uuid REFERENCES interviews(id),
    event text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
