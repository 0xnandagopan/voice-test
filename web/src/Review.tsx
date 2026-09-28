import { useEffect, useState } from "react";
import { Link, Navigate, useParams } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, ApiError, sessionKey, sessionQuery } from "./api";
import { TestimonialPreview } from "./Publication";
import {
  command,
  evidenceKey,
  interviewPath,
  recoveryKey,
  sameRevisions,
  workflowKey,
  type Content,
  type Evidence,
  type Recovery,
  type Revisions,
  type WorkflowAction,
  type WorkflowView,
} from "./workflow";

const emptyContent: Content = { text: "", attribution: "", clips: [] };
function Message({
  children,
  error = false,
}: {
  children: React.ReactNode;
  error?: boolean;
}) {
  return (
    <div
      className={`notice ${error ? "error" : ""}`}
      role={error ? "alert" : "status"}
    >
      {children}
    </div>
  );
}
function unavailable(error: unknown) {
  return (
    error instanceof ApiError && [401, 403, 404, 410].includes(error.status)
  );
}

export function Review() {
  const { interviewId } = useParams();
  const session = useQuery(sessionQuery);
  if (session.isPending)
    return <Message>Loading your private workspace…</Message>;
  if (session.isError || (interviewId && interviewId !== session.data.id)) {
    return (
      <section className="card">
        <h1>Review is unavailable.</h1>
        <Message error>
          Open the complete private invitation for this interview. Your current
          session cannot access this review.
        </Message>
      </section>
    );
  }
  // Keep the interview in the URL. A shared cookie changed in another tab must
  // never silently redirect an existing editor to another customer's interview.
  if (!interviewId)
    return <Navigate replace to={`/review/${session.data.id}`} />;
  return <ReviewWorkspace key={interviewId} id={interviewId} />;
}
function ReviewWorkspace({ id }: { id: string }) {
  const client = useQueryClient();
  const workflow = useQuery({
    queryKey: workflowKey(id),
    queryFn: () => api<WorkflowView>(`${interviewPath(id)}/workflow`),
    refetchInterval: 5000,
  });
  const evidence = useQuery({
    queryKey: evidenceKey(id),
    queryFn: () => api<Evidence>(`${interviewPath(id)}/evidence`),
    refetchInterval: 5000,
  });
  const recovery = useQuery({
    queryKey: recoveryKey(id),
    queryFn: () => api<Recovery>(`${interviewPath(id)}/recovery`),
    refetchInterval: 5000,
  });
  const [message, setMessage] = useState("");
  const [manualDraft, setManualDraft] = useState(false);
  const write = useMutation({
    mutationFn: ({
      expected,
      action,
    }: {
      expected: Revisions;
      action: WorkflowAction;
    }) => command(id, expected, action),
    onSuccess: (result) => {
      client.setQueryData(workflowKey(id), result.state);
      client.setQueryData<Evidence>(evidenceKey(id), (old) =>
        old ? { ...old, assessment: null } : old,
      );
      setMessage("Saved on the server.");
      void client.invalidateQueries({ queryKey: evidenceKey(id) });
    },
    onError: () => {
      void client.invalidateQueries({ queryKey: workflowKey(id) });
    },
  });
  const generate = useMutation({
    mutationFn: () =>
      api<{ job_id: string; state: WorkflowView }>(
        `${interviewPath(id)}/generate`,
        { request_id: crypto.randomUUID(), expected: workflow.data!.revisions },
      ),
    onSuccess: (result) => {
      client.setQueryData(workflowKey(id), result.state);
      setMessage(
        "Draft preparation requested. This does not approve or publish anything.",
      );
      void client.invalidateQueries({ queryKey: evidenceKey(id) });
    },
    onError: () => {
      void client.invalidateQueries({ queryKey: workflowKey(id) });
    },
  });
  const retry = useMutation({
    mutationFn: (jobId: string) =>
      api<{ job_id: string; state: WorkflowView }>(
        `${interviewPath(id)}/retry`,
        {
          request_id: crypto.randomUUID(),
          expected: workflow.data!.revisions,
          job_id: jobId,
        },
      ),
    onSuccess: (result) => {
      client.setQueryData(workflowKey(id), result.state);
      setMessage("Retry requested. Approval remains gated by the result.");
      void client.invalidateQueries({ queryKey: evidenceKey(id) });
      void client.invalidateQueries({ queryKey: recoveryKey(id) });
    },
    onError: () => {
      void client.invalidateQueries({ queryKey: workflowKey(id) });
    },
  });
  const confirmRecovery = useMutation({
    mutationFn: () =>
      api<{ revision: number; confirmed: boolean }>(
        `${interviewPath(id)}/recovery/confirm`,
        {
          request_id: crypto.randomUUID(),
          expected_revision: recovery.data!.interview_revision,
          evidence_revision: workflow.data!.revisions.evidence,
          acknowledge_incomplete: true,
        },
      ),
    onSuccess: () => {
      setMessage(
        "Recovery acknowledged. No recording has started. Choose Start interview when you are ready to continue.",
      );
      void client.invalidateQueries({ queryKey: recoveryKey(id) });
      void client.invalidateQueries({ queryKey: sessionKey });
    },
    onError: () => {
      void client.invalidateQueries({ queryKey: recoveryKey(id) });
      void client.invalidateQueries({ queryKey: workflowKey(id) });
    },
  });
  const errors = [
    workflow.error,
    evidence.error,
    recovery.error,
    write.error,
    generate.error,
    retry.error,
    confirmRecovery.error,
  ];
  if (errors.some(unavailable))
    return (
      <section className="card">
        <h1>Review is unavailable.</h1>
        <Message error>
          Your access may have expired or changed. Open your complete private
          invitation again.
        </Message>
      </section>
    );
  if (workflow.isPending) return <Message>Loading your review…</Message>;
  if (!workflow.data)
    return (
      <section className="card">
        <Message error>
          {workflow.error?.message ?? "Review could not be loaded."}
        </Message>
        <button onClick={() => void workflow.refetch()}>Retry review</button>
      </section>
    );
  const state = workflow.data;
  const busy =
    write.isPending ||
    generate.isPending ||
    retry.isPending ||
    confirmRecovery.isPending;
  const pendingJobs = evidence.data?.jobs.some((job) =>
    ["queued", "running"].includes(job.status),
  );
  const recordedSources = evidence.data?.sources.some(
    (source) => source.playback_available,
  );
  const insufficient =
    !state.content &&
    evidence.data?.jobs.some(
      (job) =>
        job.kind === "generate_draft" &&
        job.error_code === "insufficient_evidence",
    );
  const failedJobs = evidence.data?.jobs.some((job) =>
    ["failed", "cancelled"].includes(job.status),
  );
  return (
    <>
      <div className="page-heading">
        <div>
          <p className="eyebrow">PRIVATE REVIEW</p>
          <h1>Your story. Your decision.</h1>
          <p>
            Review the recording, edit your words and choose whether to approve.
          </p>
        </div>
        <Link className="text-link" to="/interview">
          Back to conversation
        </Link>
      </div>
      {errors.filter(Boolean).map((error, index) => (
        <Message error key={index}>
          {error instanceof ApiError && error.status === 409
            ? "The saved version changed. Refresh and reconcile your edits before trying again."
            : error!.message}
        </Message>
      ))}
      {message && <Message>{message}</Message>}
      {pendingJobs && (
        <Message>
          We are processing your recording or checking your text. Approval stays
          unavailable until all required checks pass.
        </Message>
      )}
      {failedJobs && (
        <Message error>
          A processing task did not finish. Available recordings remain below;
          missing evidence cannot support approval.
        </Message>
      )}
      {evidence.data?.jobs
        .filter((job) => job.status === "failed")
        .map((job, index) => (
          <button
            key={job.id}
            className="secondary"
            disabled={busy}
            onClick={() => retry.mutate(job.id)}
          >
            Retry{" "}
            {job.kind === "import_evidence"
              ? "recording recovery"
              : job.kind === "support_check"
                ? "support check"
                : "draft processing"}{" "}
            {index + 1}
          </button>
        ))}
      {insufficient && (
        <Message>
          There is not enough recorded detail to prepare a supported draft. No
          testimonial was generated. You can continue the interview or write a
          draft for later evidence checking.
        </Message>
      )}
      {!state.evidence_available && (
        <Message>
          Recorded evidence is not yet verified for approval. Available playback
          is private and may still need alignment review.
        </Message>
      )}
      {recovery.data && (
        <RecoveryStatus
          recovery={recovery.data}
          busy={busy}
          confirmed={confirmRecovery.isSuccess}
          onConfirm={() => confirmRecovery.mutate()}
        />
      )}
      <div className="review-grid">
        <section className="card">
          {state.content || manualDraft ? (
            <Editor
              state={state}
              busy={busy}
              write={async (expected, action) =>
                (await write.mutateAsync({ expected, action })).state
              }
            />
          ) : (
            <>
              <h2>Your draft is not ready yet.</h2>
              <p>
                We can prepare a provisional draft from available recorded
                sources. Recording alignment and support checks must pass before
                approval. Nothing has been approved.
              </p>
              <button
                disabled={!recordedSources || busy || Boolean(pendingJobs)}
                onClick={() => generate.mutate()}
              >
                {generate.isPending
                  ? "Requesting draft…"
                  : failedJobs
                    ? "Retry draft preparation"
                    : "Prepare provisional draft"}
              </button>
              <button
                className="secondary"
                disabled={busy}
                onClick={() => setManualDraft(true)}
              >
                Write my own draft
              </button>
              <p className="small">
                Your own draft is also checked against recorded evidence before
                approval.
              </p>
            </>
          )}
        </section>
        <section className="card evidence-panel">
          {evidence.data && (
            <ClaimReferences
              evidence={evidence.data}
              revisions={state.revisions}
            />
          )}
          <h2>Your recorded sources</h2>
          <p>
            Original transcript and recordings remain separate from your
            corrections and testimonial edits.
          </p>
          {evidence.isPending ? (
            <Message>Loading recorded sources…</Message>
          ) : !evidence.data?.sources.length ? (
            <Message>No recorded sources are available yet.</Message>
          ) : (
            evidence.data.sources.map((source, index) => (
              <Source
                key={source.source_id}
                id={id}
                index={index}
                source={source}
                correction={state.transcript_corrections[source.source_id]}
                busy={busy}
                revisions={state.revisions}
                onCorrect={async (text, expected) => {
                  const result = await write.mutateAsync({
                    expected,
                    action: {
                      type: "correct_transcript",
                      source_id: source.source_id,
                      text,
                    },
                  });
                  return result.state;
                }}
              />
            ))
          )}
          <button
            className="secondary"
            onClick={() => {
              void workflow.refetch();
              void evidence.refetch();
              void recovery.refetch();
            }}
          >
            Refresh saved status
          </button>
        </section>
      </div>
    </>
  );
}
function RecoveryStatus({
  recovery,
  busy,
  confirmed,
  onConfirm,
}: {
  recovery: Recovery;
  busy: boolean;
  confirmed: boolean;
  onConfirm: () => void;
}) {
  const [acknowledged, setAcknowledged] = useState(false);
  useEffect(() => setAcknowledged(false), [recovery.interview_revision]);
  if (!recovery.attempts.length) return null;
  const missing = recovery.attempts.some(
    (attempt) => attempt.status === "recording_artifacts_unavailable",
  );
  const waiting = recovery.attempts.some(
    (attempt) => attempt.status === "awaiting_recording_artifacts",
  );
  return (
    <section className="card recovery-panel">
      <h2>Recording recovery</h2>
      <p>
        {recovery.time_consumed_seconds} seconds already used. Returning does
        not reset your interview allowance.
      </p>
      {missing ? (
        <Message error>
          Some recording artifacts are unavailable. Automatic recovery has ended
          for those attempts.
        </Message>
      ) : waiting ? (
        <Message>
          Waiting for recording artifacts. Transcript text alone is not saved
          audio evidence.
        </Message>
      ) : (
        <Message>
          Recovered audio is available for review. Its alignment and
          completeness must be verified before approval.
        </Message>
      )}
      {recovery.unresolved_answers.length > 0 && (
        <>
          <h3>Answers that need review</h3>
          <ul>
            {recovery.unresolved_answers.map((answer) => (
              <li key={answer.source_id}>
                <p>{answer.text}</p>
                <span className="small">
                  This answer may be incomplete or unaligned.
                </span>
              </li>
            ))}
          </ul>
        </>
      )}
      {recovery.attempts.some(
        (attempt) => attempt.untranscribed_audio_ranges_ms.length,
      ) && (
        <Message>
          Some recorded speech has no transcript yet. It is not used to support
          a draft.
        </Message>
      )}
      {confirmed ? (
        <>
          <Message>
            Recovery acknowledged. Missing or incomplete answers remain
            ineligible evidence.
          </Message>
          <Link className="button" to="/interview">
            Continue to conversation
          </Link>
        </>
      ) : (
        recovery.requires_customer_confirmation && (
          <>
            <p>
              Listen to available sources below before continuing. Any missing
              or incomplete answer may need to be repeated; acknowledging
              recovery does not verify recording alignment.
            </p>
            <label className="checkbox">
              <input
                type="checkbox"
                checked={acknowledged}
                disabled={busy || waiting}
                onChange={(event) => setAcknowledged(event.target.checked)}
              />
              <span>
                I understand which answers may be missing or incomplete and want
                to continue.
              </span>
            </label>
            <button
              disabled={busy || waiting || !acknowledged}
              onClick={onConfirm}
            >
              Confirm recovery
            </button>
            <p className="small">
              Recording resumes only after you choose Start interview on the
              conversation screen.
            </p>
          </>
        )
      )}
    </section>
  );
}
function Editor({
  state,
  busy,
  write,
}: {
  state: WorkflowView;
  busy: boolean;
  write: (expected: Revisions, action: WorkflowAction) => Promise<WorkflowView>;
}) {
  const [base, setBase] = useState(state);
  const [draft, setDraft] = useState<Content>(state.content ?? emptyContent);
  const [confirmed, setConfirmed] = useState(false);
  const [saved, setSaved] = useState(false);
  const dirty =
    JSON.stringify(draft) !== JSON.stringify(base.content ?? emptyContent);
  const stale = !sameRevisions(base.revisions, state.revisions);
  useEffect(() => {
    if (!dirty) {
      setBase(state);
      setDraft(state.content ?? emptyContent);
    }
  }, [state, dirty]);
  useEffect(() => {
    setConfirmed(false);
  }, [
    draft,
    state.revisions.workflow,
    state.revisions.content,
    state.revisions.evidence,
    state.check,
  ]);
  const checkMessage = {
    pending:
      "Support checking is pending. Approval is unavailable until it passes.",
    supported:
      "The saved text passed its support check against the recorded evidence.",
    unsupported:
      "The saved text contains claims that are not supported. Edit it to match the recording and save for a fresh check.",
    ambiguous:
      "Some claims are unclear against the recording. Clarify your text or correct the transcript, then save for a fresh check.",
    failed:
      "The support check failed to complete. Your saved text is preserved; approval remains unavailable.",
  }[state.check];
  async function save() {
    try {
      const updated = await write(base.revisions, {
        type: "save",
        content: draft,
      });
      setBase(updated);
      setDraft(updated.content ?? emptyContent);
      setSaved(true);
    } catch {
      /* Error displayed by workspace; retain local text. */
    }
  }
  function update(next: Content) {
    setDraft(next);
    setSaved(false);
  }
  const ready =
    !busy &&
    !dirty &&
    !stale &&
    state.evidence_available &&
    state.check === "supported" &&
    Boolean(state.content?.text.trim()) &&
    Boolean(state.content?.attribution.trim()) &&
    !state.declined &&
    !state.approval;
  return (
    <>
      <h2>Review and edit</h2>
      {stale && (
        <div className="notice error" role="alert">
          <p>
            A newer version is saved. Your unsaved edits have been kept below.
            Compare them before continuing.
          </p>
          <h3>Latest saved text</h3>
          <p className="preserve-lines">
            {state.content?.text ?? "No saved draft"}
          </p>
          <p>Attribution: {state.content?.attribution ?? "None"}</p>
          <div className="actions">
            <button
              className="secondary"
              disabled={busy}
              onClick={() => {
                setBase(state);
                setDraft(state.content ?? emptyContent);
                setSaved(false);
              }}
            >
              Use latest saved version
            </button>
            <button
              className="secondary"
              disabled={busy}
              onClick={() => {
                setBase(state);
                setSaved(false);
              }}
            >
              Keep my edits against latest version
            </button>
          </div>
        </div>
      )}
      <form
        onSubmit={(event) => {
          event.preventDefault();
          void save();
        }}
      >
        <label>
          Testimonial text
          <textarea
            rows={8}
            value={draft.text}
            disabled={busy}
            onChange={(event) => update({ ...draft, text: event.target.value })}
            required
          />
        </label>
        <label>
          Attribution
          <input
            value={draft.attribution}
            disabled={busy}
            onChange={(event) =>
              update({ ...draft, attribution: event.target.value })
            }
            required
          />
        </label>
        <fieldset className="audio-choice">
          <legend>Audio in your testimonial</legend>
          {draft.clips.length ? (
            <>
              <p>
                {draft.clips.length} previously selected clip(s). This screen
                cannot preview those approved excerpts yet; remove them to
                approve text only.
              </p>
              <button
                className="secondary"
                type="button"
                disabled={busy}
                onClick={() => update({ ...draft, clips: [] })}
              >
                Exclude all audio clips
              </button>
            </>
          ) : (
            <>
              <label className="checkbox">
                <input type="checkbox" checked={false} disabled readOnly />
                <span>Include audio clips</span>
              </label>
              <p className="small">
                Audio is excluded. No verified selectable clips are available on
                this screen.
              </p>
            </>
          )}
        </fieldset>
        {dirty ? (
          <Message>
            You have unsaved changes. Save to request a fresh support check.
          </Message>
        ) : saved ? (
          <Message>Saved on the server.</Message>
        ) : (
          <p className="small">Showing the saved version.</p>
        )}
        <button
          type="submit"
          disabled={
            busy ||
            stale ||
            !dirty ||
            !draft.text.trim() ||
            !draft.attribution.trim()
          }
        >
          Save changes
        </button>
      </form>
      <Message
        error={["unsupported", "ambiguous", "failed"].includes(state.check)}
      >
        {checkMessage}
      </Message>
      {state.approval && (
        <Message>
          You approved this saved version. Publication is a separate operator
          action. Editing it will invalidate approval and withdraw any hosted
          version.
        </Message>
      )}
      {state.declined && (
        <Message>
          You declined this testimonial. It cannot be published.
        </Message>
      )}
      <TestimonialPreview content={draft} />
      <label className="checkbox">
        <input
          type="checkbox"
          checked={confirmed}
          disabled={!ready || draft.clips.length > 0}
          onChange={(event) => setConfirmed(event.target.checked)}
        />
        <span>I approve this exact text and attribution for publication.</span>
      </label>
      <div className="actions">
        <button
          disabled={!ready || !confirmed || draft.clips.length > 0}
          onClick={() =>
            void write(state.revisions, { type: "approve" }).catch(() => {})
          }
        >
          Approve exact testimonial
        </button>
        <button
          className="secondary"
          disabled={busy || stale || dirty || state.declined}
          onClick={() => {
            if (
              window.confirm(
                "Decline this testimonial? It will remain private and cannot be published.",
              )
            )
              void write(state.revisions, { type: "decline" }).catch(() => {});
          }}
        >
          Decline testimonial
        </button>
      </div>
      <p className="small muted">
        Saving or closing this page does not approve publication.
      </p>
    </>
  );
}
function ClaimReferences({
  evidence,
  revisions,
}: {
  evidence: Evidence;
  revisions: Revisions;
}) {
  const saved = evidence.assessment;
  if (
    !saved ||
    saved.content_revision !== revisions.content ||
    saved.evidence_revision !== revisions.evidence
  )
    return null;
  return (
    <section aria-label="Provisional claim suggestions">
      <h2>Provisional claim suggestions</h2>
      <p className="small">
        These model suggestions refer to the saved text. They do not verify
        recording alignment or grant approval. Listen to each source and check
        its meaning.
      </p>
      {saved.assessment.issues.length > 0 && (
        <ul>
          {saved.assessment.issues.map((issue, index) => (
            <li key={index}>{issue}</li>
          ))}
        </ul>
      )}
      {saved.assessment.claims.map((claim, index) => (
        <article className="source-record" key={index}>
          <h3>Claim {index + 1}</h3>
          <p className="preserve-lines">{claim.text}</p>
          {claim.verdict && (
            <p className="small">Suggested support: {claim.verdict}</p>
          )}
          {claim.issues?.length ? (
            <ul>
              {claim.issues.map((issue, issueIndex) => (
                <li key={issueIndex}>{issue}</li>
              ))}
            </ul>
          ) : null}
          {claim.sources.length ? (
            claim.sources.map((reference, referenceIndex) => {
              const sourceIndex = evidence.sources.findIndex(
                (source) => source.source_id === reference.source_id,
              );
              return (
                <div key={referenceIndex}>
                  <blockquote className="preserve-lines">
                    {reference.quote}
                  </blockquote>
                  {sourceIndex >= 0 ? (
                    <a
                      className="text-link"
                      href={`#recorded-source-${sourceIndex + 1}`}
                    >
                      Review source {sourceIndex + 1}
                      {evidence.sources[sourceIndex].playback_available
                        ? " and recording"
                        : " (playback unavailable)"}
                    </a>
                  ) : (
                    <p className="small">
                      This suggested source is unavailable.
                    </p>
                  )}
                </div>
              );
            })
          ) : (
            <p className="small">
              No recorded support was linked for this claim.
            </p>
          )}
        </article>
      ))}
    </section>
  );
}
function Source({
  id,
  index,
  source,
  correction,
  revisions,
  busy,
  onCorrect,
}: {
  id: string;
  index: number;
  source: Evidence["sources"][number];
  correction?: string;
  busy: boolean;
  revisions: Revisions;
  onCorrect: (text: string, expected: Revisions) => Promise<WorkflowView>;
}) {
  const current = correction ?? source.corrected_text ?? source.text;
  const [text, setText] = useState(current);
  const [editing, setEditing] = useState(false);
  const [baseText, setBaseText] = useState(current);
  const [baseRevisions, setBaseRevisions] = useState(revisions);
  const dirty = text !== baseText;
  const stale = !sameRevisions(revisions, baseRevisions);
  const [playbackError, setPlaybackError] = useState(false);
  useEffect(() => {
    if (!editing || !dirty) {
      setText(current);
      setBaseText(current);
      setBaseRevisions(revisions);
    }
  }, [current, editing, revisions, dirty]);
  return (
    <article className="source-record" id={`recorded-source-${index + 1}`}>
      <h3>
        Source {index + 1} ·{" "}
        {source.speaker === "customer" ? "Your answer" : "Interviewer"}
      </h3>
      <p className="small">Original transcript</p>
      <p className="preserve-lines">{source.text}</p>
      {(correction ?? source.corrected_text) != null && (
        <>
          <p className="small">Saved correction</p>
          <p className="preserve-lines">{current}</p>
        </>
      )}
      <p className="small">
        {source.alignment_verified
          ? "Recording alignment verified."
          : "Alignment is unverified. Playback may include surrounding speech or an incomplete answer."}
      </p>
      {source.playback_available ? (
        <audio
          aria-label={`Play source ${index + 1}`}
          controls
          preload="none"
          src={`/api${interviewPath(id)}/sources/${encodeURIComponent(source.source_id)}/audio`}
          onError={() => setPlaybackError(true)}
        />
      ) : (
        <p className="small">
          Recording playback is unavailable for this source.
        </p>
      )}
      {playbackError && (
        <Message error>
          Playback unavailable. Refresh your access and try again.
        </Message>
      )}
      <details onToggle={(event) => setEditing(event.currentTarget.open)}>
        <summary>Correct this transcript</summary>
        {stale && (
          <Message error>
            The saved evidence changed. Your correction is kept below. Compare
            it with the saved transcript, then choose how to proceed.
            <div className="actions">
              <button
                type="button"
                className="secondary"
                disabled={busy}
                onClick={() => {
                  setText(current);
                  setBaseText(current);
                  setBaseRevisions(revisions);
                }}
              >
                Use saved transcript
              </button>
              <button
                type="button"
                className="secondary"
                disabled={busy}
                onClick={() => {
                  setBaseText(current);
                  setBaseRevisions(revisions);
                }}
              >
                Keep my correction against latest version
              </button>
            </div>
          </Message>
        )}
        <p className="small">
          Corrections do not change the original recording. They invalidate
          existing checks and approval.
        </p>
        <form
          onSubmit={async (event) => {
            event.preventDefault();
            try {
              const updated = await onCorrect(text, baseRevisions);
              setBaseText(text);
              setBaseRevisions(updated.revisions);
            } catch {
              /* Preserve correction on error. */
            }
          }}
        >
          <label>
            Correction for source {index + 1}
            <textarea
              rows={3}
              value={text}
              onChange={(event) => setText(event.target.value)}
            />
          </label>
          <button
            className="secondary"
            disabled={busy || stale || text === current || !text.trim()}
          >
            Save transcript correction
          </button>
        </form>
      </details>
    </article>
  );
}
