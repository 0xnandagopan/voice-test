import { useState, type SyntheticEvent } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "./api";
import type { WorkflowView } from "./workflow";

type Source = {
  source_id: string;
  text: string;
  speaker?: string;
  start_ms: number | null;
  end_ms: number | null;
  candidate_range_ms: [number, number] | null;
  verified_range_ms: [number, number] | null;
  alignment_verified: boolean;
};
type Confirmation = {
  recording_sha256: string;
  timeline_sha256: string;
  source_id: string;
  source_text_sha256: string;
  source_range_ms: [number, number];
  clip_sha256: string;
  listened: boolean;
  transcript_matches: boolean;
  complete_answer: boolean;
};
type Evidence = {
  assessment?: { assessment: { quality_gate_passed?: boolean } } | null;
  sources: Source[];
  evidence_revision: number;
  content_revision: number;
};

/** Manual listening establishes a scoped operator attestation, never customer approval. */
export function AlignmentReview({
  interviewId,
  state,
}: {
  interviewId: string;
  state: WorkflowView;
}) {
  const client = useQueryClient();
  const path = `/operator/interviews/${encodeURIComponent(interviewId)}`;
  const evidence = useQuery({
    queryKey: ["operator-evidence", interviewId],
    queryFn: () => api<Evidence>(`${path}/evidence`),
    refetchInterval: 5000,
  });
  const sources =
    evidence.data?.sources.filter(
      (source) => !source.speaker || source.speaker === "customer",
    ) ?? [];
  const current =
    !evidence.isError &&
    evidence.data?.evidence_revision === state.revisions.evidence &&
    evidence.data?.content_revision === state.revisions.content;
  return (
    <section className="card" aria-label="Recording verification">
      <h2>Verify recorded answers</h2>
      <p>
        Listen to each exact audio preview. Confirm that the original transcript
        matches the customer's complete answer, including qualifications and
        mixed feedback. Adjust its time range if needed.
      </p>
      <p className="small">
        This verifies recorded evidence only. A support check and the customer's
        exact approval are still required before publication.
      </p>
      {evidence.isPending && <p role="status">Loading recorded answers…</p>}
      {evidence.error && (
        <div className="notice error" role="alert">
          {evidence.error.message}
        </div>
      )}
      {evidence.data && !current && (
        <div className="notice" role="status">
          Recording or text revisions changed. Waiting for the current saved
          version before verification.
        </div>
      )}
      {evidence.data && sources.length === 0 && (
        <p>No recorded customer answers are available for verification yet.</p>
      )}
      {current &&
        sources.map((source) => (
          <SourceReview
            key={`${source.source_id}:${source.text}:${state.revisions.workflow}:${state.revisions.evidence}:${state.revisions.content}`}
            source={source}
            path={path}
            state={state}
            onRefresh={() => {
              void client.invalidateQueries({
                queryKey: ["operator-workflow", interviewId],
              });
              void client.invalidateQueries({
                queryKey: ["operator-evidence", interviewId],
              });
            }}
            onVerified={(result) => {
              client.setQueryData(["operator-workflow", interviewId], result);
              void client.invalidateQueries({
                queryKey: ["operator-evidence", interviewId],
              });
              void client.invalidateQueries({
                queryKey: ["evidence", interviewId],
              });
              void client.invalidateQueries({
                queryKey: ["workflow", interviewId],
              });
            }}
          />
        ))}
      {current &&
        sources.length > 0 &&
        sources.every((source) => source.alignment_verified) &&
        !state.evidence_available && (
          <div className="notice" role="status">
            The listed answers are verified, but recording completeness or
            coverage is still unresolved. Review the verified time ranges for
            missing parts of an answer. Only confirm a range that contains that
            complete answer and matches its original transcript; do not include
            unrelated audio just to clear this check.
          </div>
        )}
      {evidence.data?.assessment?.assessment.quality_gate_passed === false && (
        <div className="notice error" role="alert">
          The configured drafting model has not passed the required quality
          checks. Changing the model in local configuration and validating it is
          required before customer approval can be enabled. Listening
          verification alone does not resolve this service requirement.
        </div>
      )}
      {state.evidence_available ? (
        <p role="status">
          Recorded evidence is verified. Publication also requires current
          support checks and exact customer approval.
        </p>
      ) : (
        <p role="status">
          Approval remains unavailable until all required recorded evidence and
          support checks pass.
        </p>
      )}
    </section>
  );
}

function SourceReview({
  source,
  path,
  state,
  onVerified,
  onRefresh,
}: {
  source: Source;
  path: string;
  state: WorkflowView;
  onVerified: (state: WorkflowView) => void;
  onRefresh: () => void;
}) {
  const range = source.verified_range_ms ??
    source.candidate_range_ms ?? [source.start_ms, source.end_ms];
  const [start, setStart] = useState(
    range[0] == null ? "" : String(range[0] / 1000),
  );
  const [end, setEnd] = useState(
    range[1] == null ? "" : String(range[1] / 1000),
  );
  const [editingVerified, setEditingVerified] = useState(false);
  const [preview, setPreview] = useState<Confirmation | null>(null);
  const [listened, setListened] = useState(false);
  const [matches, setMatches] = useState(false);
  const [complete, setComplete] = useState(false);
  const startMs = Math.round(Number(start) * 1000);
  const endMs = Math.round(Number(end) * 1000);
  const validRange =
    start.trim() !== "" &&
    end.trim() !== "" &&
    Number.isSafeInteger(startMs) &&
    Number.isSafeInteger(endMs) &&
    startMs >= 0 &&
    endMs > startMs;
  const endpoint = `${path}/alignment/${encodeURIComponent(source.source_id)}`;
  const params = (a: number, b: number) =>
    `?start_ms=${a}&end_ms=${b}&evidence_revision=${state.revisions.evidence}`;
  const prepare = useMutation({
    mutationFn: () => api<Confirmation>(`${endpoint}${params(startMs, endMs)}`),
    onSuccess: setPreview,
  });
  const verify = useMutation({
    mutationFn: () =>
      api<WorkflowView>(`${path}/alignment`, {
        expected: state.revisions,
        confirmation: {
          ...preview!,
          listened: true,
          transcript_matches: true,
          complete_answer: true,
        },
      }),
    onSuccess: onVerified,
    onError: () => {
      setListened(false);
      setMatches(false);
      setComplete(false);
      onRefresh();
    },
  });
  function clear() {
    setPreview(null);
    setListened(false);
    setMatches(false);
    setComplete(false);
    prepare.reset();
    verify.reset();
  }
  function playbackEnded(event: SyntheticEvent<HTMLAudioElement>) {
    const audio = event.currentTarget;
    let covered = 0;
    const tolerance = Math.min(0.05, audio.duration * 0.01);
    for (let i = 0; i < audio.played.length; i++) {
      if (audio.played.start(i) > covered + tolerance) break;
      covered = Math.max(covered, audio.played.end(i));
    }
    setListened(
      audio.played.length > 0 &&
        Number.isFinite(audio.duration) &&
        audio.duration > 0 &&
        covered >= audio.duration - tolerance,
    );
  }
  if (source.alignment_verified && !editingVerified)
    return (
      <article className="source-record">
        <p className="preserve-lines">{source.text}</p>
        <p role="status">Recorded answer verified.</p>
        {source.verified_range_ms && (
          <p className="small">
            Verified range: {source.verified_range_ms[0] / 1000}–
            {source.verified_range_ms[1] / 1000} seconds.
          </p>
        )}
        <button className="secondary" onClick={() => setEditingVerified(true)}>
          Review or change verified range
        </button>
      </article>
    );
  return (
    <article className="source-record">
      <h3>Original recorded answer</h3>
      {editingVerified && (
        <>
          <p>
            Changing this verified range requires listening and confirming
            again. Saving a new verification invalidates existing checks and
            customer approval.
          </p>
          <button
            className="secondary"
            disabled={prepare.isPending || verify.isPending}
            onClick={() => {
              clear();
              setStart(range[0] == null ? "" : String(range[0] / 1000));
              setEnd(range[1] == null ? "" : String(range[1] / 1000));
              setEditingVerified(false);
            }}
          >
            Keep verified range
          </button>
        </>
      )}
      <p className="preserve-lines">{source.text}</p>
      <div className="actions">
        <label>
          Start time (seconds)
          <input
            type="number"
            min="0"
            step="0.001"
            value={start}
            disabled={prepare.isPending || verify.isPending}
            onChange={(e) => {
              clear();
              setStart(e.target.value);
            }}
          />
        </label>
        <label>
          End time (seconds)
          <input
            type="number"
            min="0"
            step="0.001"
            value={end}
            disabled={prepare.isPending || verify.isPending}
            onChange={(e) => {
              clear();
              setEnd(e.target.value);
            }}
          />
        </label>
      </div>
      <button
        className="secondary"
        disabled={!validRange || prepare.isPending || verify.isPending}
        onClick={() => {
          clear();
          prepare.mutate();
        }}
      >
        {prepare.isPending ? "Preparing preview…" : "Prepare audio preview"}
      </button>
      {prepare.error && (
        <div className="notice error" role="alert">
          {prepare.error.message}
        </div>
      )}
      {preview && (
        <>
          <p>
            Listen to the entire preview before confirming. Seeking past audio
            does not count as listening.
          </p>
          <audio
            aria-label="Exact recorded answer preview"
            controls
            preload="none"
            src={`/api${endpoint}/audio${params(...preview.source_range_ms)}`}
            onEnded={playbackEnded}
            onError={() => setListened(false)}
          />
          <p role="status">
            {listened
              ? "Full preview played. Confirm what you heard below."
              : "Play the full preview to enable verification."}
          </p>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={matches}
              disabled={!listened || verify.isPending}
              onChange={(e) => setMatches(e.target.checked)}
            />
            The original transcript matches the words in this audio.
          </label>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={complete}
              disabled={!listened || verify.isPending}
              onChange={(e) => setComplete(e.target.checked)}
            />
            This is the complete answer, including its qualifications and mixed
            feedback.
          </label>
          <button
            disabled={!listened || !matches || !complete || verify.isPending}
            onClick={() => verify.mutate()}
          >
            {verify.isPending
              ? "Saving verification…"
              : "Verify recorded answer"}
          </button>
          {verify.error && (
            <div className="notice error" role="alert">
              {verify.error.message} Reload the current recording before trying
              again.
            </div>
          )}
        </>
      )}
    </article>
  );
}
