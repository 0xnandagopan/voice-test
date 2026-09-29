import { useEffect, useState } from "react";
import { Link, Navigate, useLocation, useParams } from "react-router-dom";
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
  const location = useLocation();
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
    onMutate: () => clearActionErrors(),
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
        old
          ? {
              ...old,
              assessment: null,
              jobs: old.jobs.filter((job) => isRecordingJob(job)),
            }
          : old,
      );
      setMessage("");
      void client.invalidateQueries({ queryKey: evidenceKey(id) });
    },
    onError: () => {
      void client.invalidateQueries({ queryKey: workflowKey(id) });
    },
  });
  const generate = useMutation({
    onMutate: () => clearActionErrors(),
    mutationFn: () =>
      api<{ job_id: string; state: WorkflowView }>(
        `${interviewPath(id)}/generate`,
        { request_id: crypto.randomUUID(), expected: workflow.data!.revisions },
      ),
    onSuccess: (result) => {
      client.setQueryData(workflowKey(id), result.state);
      setMessage("");
      void client.invalidateQueries({ queryKey: evidenceKey(id) });
    },
    onError: () => {
      void client.invalidateQueries({ queryKey: workflowKey(id) });
    },
  });
  const retry = useMutation({
    onMutate: () => clearActionErrors(),
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
      setMessage("");
      client.setQueryData<Evidence>(evidenceKey(id), (old) =>
        old
          ? {
              ...old,
              jobs: old.jobs.map((job) =>
                job.id === result.job_id
                  ? {
                      ...job,
                      status: "queued",
                      error_code: null,
                      can_retry: false,
                    }
                  : job,
              ),
            }
          : old,
      );
      void client.invalidateQueries({ queryKey: evidenceKey(id) });
      void client.invalidateQueries({ queryKey: recoveryKey(id) });
    },
    onError: () => {
      void client.invalidateQueries({ queryKey: workflowKey(id) });
    },
  });
  const confirmRecovery = useMutation({
    onMutate: () => clearActionErrors(),
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
  function clearActionErrors(): void {
    setMessage("");
    for (const mutation of [write, generate, retry, confirmRecovery]) {
      if (mutation.isError) mutation.reset();
    }
  }
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
  // Independent polls may return old evidence after a cross-tab edit. Keep
  // recording recovery visible, but never attach an old task to new text.
  const currentJobs = (evidence.data?.jobs ?? []).filter(
    (job) =>
      isRecordingJob(job) ||
      (evidence.data?.content_revision === state.revisions.content &&
        evidence.data?.evidence_revision === state.revisions.evidence),
  );
  const pendingJobs = currentJobs.some((job) =>
    ["queued", "running"].includes(job.status),
  );
  const recordedSources = evidence.data?.sources.some(
    (source) => source.playback_available,
  );
  const insufficient =
    !state.content &&
    currentJobs.some(
      (job) =>
        job.kind === "generate_draft" &&
        job.error_code === "insufficient_evidence",
    );
  const draftJob = currentJobs.find((job) => job.kind === "generate_draft");
  const supportTaskVisible =
    currentJobs.some(
      (job) =>
        job.kind === "support_check" &&
        ["queued", "running", "failed"].includes(job.status),
    ) ?? false;
  return (
    <>
      <div className="page-heading">
        <div>
          <p className="eyebrow">PRIVATE REVIEW</p>
          <h1>Your story. Your decision.</h1>
          <p>
            Review your draft, make any edits, and approve the version you want
            to share. The operator can publish it after your approval.
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
      <ProcessingTasks
        jobs={currentJobs}
        busy={busy}
        onRetry={(jobId) => retry.mutate(jobId)}
      />
      {insufficient && (
        <Message>
          There is not enough recorded detail to prepare a supported draft. No
          testimonial was generated. You can continue the interview or write a
          draft for later evidence checking.
        </Message>
      )}
      {!state.evidence_available &&
        !currentJobs.some(
          (job) =>
            job.kind === "align_evidence" &&
            ["queued", "running", "failed"].includes(job.status),
        ) && (
          <Message>
            Your recordings are not ready yet. Audio from before and after any
            reconnect must finish processing. Your draft is saved, and you can
            keep editing.
          </Message>
        )}
      {recovery.data?.attempts.some(
        (attempt) => attempt.status === "recording_artifacts_unavailable",
      ) && (
        <Message error>
          Some recordings could not be recovered. Your saved draft is preserved.
          More detail is available under the optional recording and transcript
          section.
        </Message>
      )}
      <div
        className="review-grid"
        style={{ gridTemplateColumns: "minmax(0, 1fr)" }}
      >
        <section className="card">
          {state.content || manualDraft ? (
            <Editor
              state={state}
              id={id}
              evidence={evidence.data}
              busy={busy}
              supportTaskVisible={supportTaskVisible}
              write={async (expected, action) =>
                (await write.mutateAsync({ expected, action })).state
              }
            />
          ) : (
            <>
              <h2>Your draft is not ready yet.</h2>
              <p>
                We can prepare a draft from your interview. You can edit it
                before deciding whether to approve it.
              </p>
              {!draftJob || draftJob.status === "succeeded" ? (
                <button
                  disabled={
                    !recordedSources ||
                    busy ||
                    Boolean(pendingJobs) ||
                    Boolean(insufficient)
                  }
                  onClick={() => generate.mutate()}
                >
                  {generate.isPending
                    ? "Requesting draft…"
                    : "Prepare provisional draft"}
                </button>
              ) : (
                <p>
                  See draft preparation status above, or write your own draft
                  below.
                </p>
              )}
              <button
                className="secondary"
                disabled={busy}
                onClick={() => setManualDraft(true)}
              >
                Write my own draft
              </button>
              <p className="small">
                Write the testimonial you want to share about your interview.
              </p>
            </>
          )}
        </section>
        <details
          id="recordings"
          className="card evidence-panel"
          open={location.hash === "#recordings" ? true : undefined}
        >
          <summary>View recording and transcript (optional)</summary>
          <p>
            You do not need to replay or confirm each answer to approve your
            testimonial.
          </p>
          {recovery.data && (
            <RecoveryStatus
              recovery={recovery.data}
              busy={busy}
              confirmed={confirmRecovery.isSuccess}
              onConfirm={() => confirmRecovery.mutate()}
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
        </details>
      </div>
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
    </>
  );
}
type ProcessingJob = Evidence["jobs"][number];
function isRecordingJob(job: ProcessingJob) {
  return job.kind === "import_evidence" || job.kind === "align_evidence";
}
function taskCopy(job: ProcessingJob) {
  if (job.kind === "align_evidence") {
    return {
      title: "Recording check",
      retryLabel: "Retry recording check",
      text:
        job.status === "queued"
          ? "Preparing your draft for approval. You can keep editing and saving."
          : job.status === "running"
            ? "Preparing your draft for approval. You can keep editing and saving."
            : job.error_code === "alignment_transcript_mismatch"
              ? "We could not finish preparing your recording. Your draft is saved. This needs a technical repair; you do not need to verify each answer."
              : job.error_code === "alignment_recording_incomplete"
                ? "We could not confirm a complete recording. Your text is saved, but approval is unavailable until recording recovery succeeds."
                : job.error_code === "alignment_ranges_uncertain"
                  ? "We could not reliably match the recorded answers to playable clips. Your text is saved; approval is unavailable until recording recovery succeeds."
                  : "We could not finish preparing your recording. Your draft is saved. A technical repair is needed before approval becomes available.",
    };
  }
  const support = job.kind === "support_check";
  const recovery = job.kind === "import_evidence";
  const title = recovery
    ? "Recording recovery"
    : support
      ? "Evidence check"
      : "Draft preparation";
  const retryLabel = recovery
    ? "Retry recording recovery"
    : support
      ? "Retry evidence check"
      : "Retry draft preparation";
  if (["queued", "running"].includes(job.status)) {
    const waiting = job.status === "queued";
    return {
      title,
      retryLabel,
      text: recovery
        ? "Waiting for your recording to become available. You can leave this page and return later."
        : support
          ? "Preparing your draft for approval. Your text is saved, and you can keep editing."
          : `${waiting ? "Draft preparation is queued" : "We are preparing a draft from your recordings"}. You can leave this page and return later.`,
    };
  }
  if (recovery)
    return {
      title,
      retryLabel,
      text: "One recording could not be recovered. Other available recordings remain below. Retry recovery to check again.",
    };
  const preserved = support
    ? "Your text is saved. Approval remains unavailable."
    : "Your recordings are unchanged. No generated draft was saved.";
  let detail: string;
  switch (job.error_code) {
    case "gateway_model_access":
      return {
        title,
        retryLabel: "Retry service check",
        text: `${preserved} The automatic drafting service has an account-access problem. Technical support needs to restore the service. This does not require anyone to approve your draft; retrying before the repair may fail again.`,
      };
    case "gateway_configuration":
    case "gateway_request_rejected":
      return {
        title,
        retryLabel: "Retry service check",
        text: `${preserved} The automatic drafting service could not accept this request because of a technical configuration problem. This does not require anyone to approve your draft. You can keep editing; retry after the service is repaired.`,
      };
    case "gateway_rate_limited":
      detail = "The drafting service is busy. Please try again later.";
      break;
    case "gateway_unavailable":
      detail =
        "The drafting service could not be reached. Please try again later.";
      break;
    case "gateway_response_incomplete":
      detail =
        "The drafting service returned an incomplete response. You can retry.";
      break;
    case "generation_validation_failed":
    case "gateway_response_invalid":
    case "gateway_output_schema_invalid":
    case "gateway_output_json_invalid":
    case "gateway_response_too_large":
      detail =
        "The drafting service returned a response we could not verify. You can retry; if it repeats, contact the operator.";
      break;
    case "gateway_request_budget_exhausted":
    case "composition_input_invalid":
      detail =
        "The available recorded text could not be processed. Please contact the operator.";
      break;
    default:
      detail = support
        ? "The automated evidence check did not finish. You can retry."
        : "The draft could not be prepared. You can retry or write your own draft.";
  }
  return { title, retryLabel, text: `${preserved} ${detail}` };
}
function ProcessingTasks({
  jobs,
  busy,
  onRetry,
}: {
  jobs: ProcessingJob[];
  busy: boolean;
  onRetry: (id: string) => void;
}) {
  return (
    <div className="processing-tasks">
      {jobs
        .filter((job) => ["queued", "running", "failed"].includes(job.status))
        .map((job) => {
          const copy = taskCopy(job);
          return (
            <section
              className={`notice processing-task ${job.status === "failed" ? "error" : ""}`}
              key={job.id}
              aria-label={copy.title}
            >
              <div role={job.status === "failed" ? "alert" : "status"}>
                <strong>{copy.title}</strong>
                <p>{copy.text}</p>
              </div>
              {job.status === "failed" && job.can_retry && (
                <button
                  className="secondary"
                  disabled={busy}
                  onClick={() => onRetry(job.id)}
                >
                  {copy.retryLabel}
                </button>
              )}
            </section>
          );
        })}
    </div>
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
          Your recording is available here if you want to listen.
        </Message>
      )}
      {recovery.unresolved_answers.length > 0 && (
        <>
          <h3>Interrupted answers</h3>
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
              The connection was interrupted. You can return to the conversation
              to finish any incomplete answers. Listening here is optional.
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
  id,
  evidence,
  state,
  busy,
  supportTaskVisible,
  write,
}: {
  id: string;
  evidence?: Evidence;
  state: WorkflowView;
  busy: boolean;
  supportTaskVisible: boolean;
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
    pending: "Preparing your draft for approval. You can keep editing.",
    supported: "Your saved draft is ready for your approval.",
    unsupported:
      "Some wording differs from your interview. You can edit it or approve this version as written.",
    ambiguous:
      "Some wording could not be confidently matched to your interview. You can edit it or approve this version as written.",
    failed:
      "Your draft is saved, but its automatic check did not finish. Retry the check above.",
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
  const availableClips =
    evidence?.evidence_revision === state.revisions.evidence
      ? (evidence.clips ?? [])
      : [];
  const unavailableSelection = draft.clips.some(
    (selected) =>
      !availableClips.some(
        (clip) => clip.id === selected.id && clip.sha256 === selected.sha256,
      ),
  );
  const qualityValidationFailed =
    evidence?.assessment?.content_revision === state.revisions.content &&
    evidence.assessment.evidence_revision === state.revisions.evidence &&
    evidence.assessment.assessment.quality_gate_passed === false;
  const ready =
    !qualityValidationFailed &&
    !unavailableSelection &&
    !busy &&
    !dirty &&
    !stale &&
    state.evidence_available &&
    ["supported", "unsupported", "ambiguous"].includes(state.check) &&
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
          <p className="small">
            Audio is optional. Choose any clips you want to include. You can
            preview them if you wish; your saved selection is part of your
            approval.
          </p>
          {availableClips.length ? (
            availableClips.map((clip, index) => (
              <div key={clip.id}>
                <label className="checkbox">
                  <input
                    type="checkbox"
                    disabled={busy}
                    checked={draft.clips.some(
                      (selected) =>
                        selected.id === clip.id &&
                        selected.sha256 === clip.sha256,
                    )}
                    onChange={(event) =>
                      update({
                        ...draft,
                        clips: event.target.checked
                          ? [
                              ...draft.clips.filter(
                                (selected) => selected.id !== clip.id,
                              ),
                              { id: clip.id, sha256: clip.sha256 },
                            ]
                          : draft.clips.filter(
                              (selected) => selected.id !== clip.id,
                            ),
                      })
                    }
                  />
                  <span>Include recording {index + 1}</span>
                </label>
                <p>
                  {
                    evidence?.sources.find(
                      (source) => source.source_id === clip.source_id,
                    )?.text
                  }
                </p>
                {evidence?.sources.find(
                  (source) => source.source_id === clip.source_id,
                )?.recording_interrupted && (
                  <p className="small">
                    Saved audio from before the interruption. Its ending may be
                    incomplete. Listening is optional.
                  </p>
                )}
                <audio
                  controls
                  preload="none"
                  aria-label={`Recording ${index + 1}`}
                  src={`/api${interviewPath(id)}/clips/${encodeURIComponent(clip.id)}/audio`}
                />
              </div>
            ))
          ) : (
            <Message>
              Audio clips are not ready. You can leave audio out.
            </Message>
          )}
          {unavailableSelection && (
            <Message error>
              A previously selected clip is no longer verified for this evidence
              version. Exclude it or wait for the recording check to complete
              before selecting it again.
            </Message>
          )}
          {draft.clips.length > 0 && (
            <button
              className="secondary"
              type="button"
              disabled={busy}
              onClick={() => update({ ...draft, clips: [] })}
            >
              Exclude all audio clips
            </button>
          )}
        </fieldset>
        {dirty ? (
          <Message>
            You have unsaved changes. Save your draft before approving it.
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
      {!dirty &&
        !qualityValidationFailed &&
        state.evidence_available &&
        !(
          supportTaskVisible && ["pending", "failed"].includes(state.check)
        ) && <Message error={state.check === "failed"}>{checkMessage}</Message>}
      {qualityValidationFailed && (
        <Message>
          The evidence-checking service has not passed its quality checks yet.
          Your text is saved, but approval is unavailable until technical
          service validation is complete. This is not a review of your draft by
          the operator.
        </Message>
      )}
      {evidence && ["unsupported", "ambiguous"].includes(state.check) && (
        <ClaimReferences evidence={evidence} revisions={state.revisions} />
      )}
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
      <TestimonialPreview
        content={draft}
        clipBasePath={`${interviewPath(id)}/clips`}
      />
      {!ready && !state.approval && !state.declined && (
        <p className="small" role="status">
          {stale
            ? "Resolve the saved-version change above before approving."
            : dirty
              ? "Save your changes before approving this version."
              : !draft.text.trim() || !draft.attribution.trim()
                ? "Add your testimonial and attribution, then save."
                : unavailableSelection
                  ? "Remove unavailable audio clips or choose another clip, then save."
                  : !state.evidence_available
                    ? "Your draft is saved. Approval will be available once all saved recordings, including audio from before and after any reconnect, finish processing. You do not need to replay or confirm your answers."
                    : state.check === "pending"
                      ? "Preparing your draft for approval…"
                      : state.check === "failed"
                        ? "The automatic check did not finish. Your draft is saved."
                        : qualityValidationFailed
                          ? "The checking service needs a technical update before approval is available."
                          : "Saving your changes…"}
        </p>
      )}
      <label className="checkbox">
        <input
          type="checkbox"
          checked={confirmed}
          disabled={!ready}
          onChange={(event) => setConfirmed(event.target.checked)}
        />
        <span>
          {draft.clips.length
            ? "I approve this exact text, attribution and selected audio clips for publication."
            : "I approve this exact text and attribution for publication."}
        </span>
      </label>
      <div className="actions">
        <button
          disabled={!ready || !confirmed}
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
    <section aria-label="Draft notes">
      <h2>Draft notes</h2>
      <p className="small">
        These notes highlight wording that may differ from your interview. You
        can edit it or approve your saved version as written. You do not need to
        replay or confirm each answer.
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
          <h3>
            {saved.assessment.claims.length === 1
              ? "About this draft"
              : `Draft passage ${index + 1}`}
          </h3>
          <p className="preserve-lines">{claim.text}</p>
          {claim.verdict && (
            <p className="small">
              {claim.verdict === "supported"
                ? "Matches the interview."
                : claim.verdict === "unsupported"
                  ? "Some wording was not found in the interview."
                  : "The comparison with the interview is uncertain."}
            </p>
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
                      onClick={() => {
                        const details =
                          document.querySelector<HTMLDetailsElement>(
                            "details.evidence-panel",
                          );
                        if (details) details.open = true;
                      }}
                    >
                      View source {sourceIndex + 1}
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
              No matching recording passage was linked for this wording.
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
  const [baseText, setBaseText] = useState(current);
  const [baseRevisions, setBaseRevisions] = useState(revisions);
  const dirty = text !== baseText;
  const stale = !sameRevisions(revisions, baseRevisions);
  const [playbackError, setPlaybackError] = useState(false);
  useEffect(() => {
    if (!dirty) {
      setText(current);
      setBaseText(current);
      setBaseRevisions(revisions);
    }
  }, [current, revisions, dirty]);
  return (
    <article className="source-record" id={`recorded-source-${index + 1}`}>
      <h3>
        Source {index + 1} ·{" "}
        {source.speaker === "customer" ? "Your recording" : "Interviewer"}
      </h3>
      <p className="small">Original transcript</p>
      <p className="preserve-lines">{source.text}</p>
      {(correction ?? source.corrected_text) != null && (
        <>
          <p className="small">Saved correction</p>
          <p className="preserve-lines">{current}</p>
        </>
      )}
      {source.recording_interrupted && (
        <p className="small">
          Saved audio from before the interruption. Its ending may be
          incomplete. Listening is optional.
        </p>
      )}
      <p className="small">
        {source.alignment_verified
          ? source.recording_interrupted
            ? "Saved recording verified; this does not mean the spoken answer was finished."
            : "Recording alignment verified."
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
      <details>
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
