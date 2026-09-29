import { api } from "./api";

export type Content = {
  text: string;
  attribution: string;
  clips: { id: string; sha256: string }[];
};
export type Revisions = { workflow: number; content: number; evidence: number };
export type WorkflowView = {
  revisions: Revisions;
  evidence_available: boolean;
  content: Content | null;
  check: "pending" | "supported" | "unsupported" | "ambiguous" | "failed";
  approval: {
    id: string;
    content_revision: number;
    evidence_revision: number;
    content: Content;
    approved_at: string;
  } | null;
  published_approval_id: string | null;
  declined: boolean;
  transcript_corrections: Record<string, string>;
};
export type WorkflowAction =
  | { type: "save"; content: Content }
  | { type: "correct_transcript"; source_id: string; text: string }
  | { type: "approve" }
  | { type: "decline" };
export type Evidence = {
  sources: {
    source_id: string;
    attempt_id: string;
    text: string;
    corrected_text: string | null;
    speaker: string;
    start_ms: number | null;
    end_ms: number | null;
    playback_available: boolean;
    alignment_verified: boolean;
    recording_interrupted?: boolean;
    candidate_range_ms?: [number, number] | null;
    verified_range_ms?: [number, number] | null;
  }[];
  clips?: { id: string; sha256: string; source_id: string }[];
  jobs: {
    id: string;
    kind: string;
    status: string;
    error_code: string | null;
    can_retry: boolean;
  }[];
  evidence_revision: number;
  content_revision: number;
  assessment?: {
    kind: "generate_draft" | "support_check";
    model: string;
    prompt_version: string;
    content_revision: number;
    evidence_revision: number;
    assessment: {
      quality_gate_passed?: boolean;
      claims: {
        text: string;
        sources: { source_id: string; quote: string }[];
        verdict?: "supported" | "unsupported" | "uncertain";
        issues?: string[];
      }[];
      issues: string[];
    };
  } | null;
};
export type Recovery = {
  interview_id: string;
  interview_revision: number;
  topic_index: number;
  followup_counts: number[];
  time_consumed_seconds: number;
  attempts: {
    provider_attempt_id: string;
    status: string;
    recommended_action: string;
    untranscribed_audio_ranges_ms: [number, number][];
  }[];
  recorded_utterances: RecoveredAnswer[];
  unresolved_answers: RecoveredAnswer[];
  requires_customer_confirmation: boolean;
  may_advance_progress: boolean;
  recommended_action: string;
};
type RecoveredAnswer = {
  source_id: string;
  text: string;
  status: string;
  needs_alignment_review: boolean;
};
export function interviewPath(id: string) {
  return `/customer/interviews/${encodeURIComponent(id)}`;
}
export const workflowKey = (id: string) => ["workflow", id];
export const evidenceKey = (id: string) => ["evidence", id];
export const recoveryKey = (id: string) => ["recovery", id];
export function command(
  id: string,
  expected: Revisions,
  action: WorkflowAction,
) {
  return api<{ request_id: string; replayed: boolean; state: WorkflowView }>(
    `${interviewPath(id)}/workflow`,
    { interview_id: id, request_id: crypto.randomUUID(), expected, action },
  );
}
export function sameRevisions(a: Revisions, b: Revisions) {
  return (
    a.workflow === b.workflow &&
    a.content === b.content &&
    a.evidence === b.evidence
  );
}
