import { apiUrl } from "./api-origin";
import { Link, useParams } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlignmentReview } from "./AlignmentReview";
import { api } from "./api";
import type { Content, WorkflowView } from "./workflow";

/** Shared approved/preview presentation accepts no private source objects. */
export function TestimonialPreview({
  content,
  clipBasePath,
}: {
  content: Content;
  clipBasePath?: string;
}) {
  return (
    <section
      className="testimonial-preview"
      aria-label="Exact testimonial preview"
    >
      <h3>Your exact preview</h3>
      <blockquote className="preserve-lines">{content.text}</blockquote>
      <p>{content.attribution}</p>
      <p className="small">
        {content.clips.length
          ? `${content.clips.length} audio clip(s) selected.`
          : "Text only · no audio included"}
      </p>
      {content.clips.map((clip, index) =>
        clipBasePath ? (
          <audio
            crossOrigin="use-credentials"
            key={`${clip.id}:${clip.sha256}`}
            aria-label={`Selected audio clip ${index + 1}`}
            controls
            preload="none"
            style={{ width: "100%" }}
            src={apiUrl(`${clipBasePath}/${encodeURIComponent(clip.id)}/audio`)}
          />
        ) : (
          <p key={clip.id}>Clip playback is unavailable on this screen.</p>
        ),
      )}
    </section>
  );
}
export function PublicTestimonial() {
  const { slug = "" } = useParams();
  const snapshot = useQuery({
    queryKey: ["public-snapshot", slug],
    queryFn: () =>
      api<Content & { approved_at: string }>(
        `/public/${encodeURIComponent(slug)}`,
      ),
    refetchInterval: 5000,
    gcTime: 0,
  });
  if (snapshot.isPending)
    return (
      <div className="notice" role="status">
        Loading testimonial…
      </div>
    );
  if (snapshot.isError || !snapshot.data)
    return (
      <section className="card placeholder">
        <p className="eyebrow">PUBLIC TESTIMONIAL</p>
        <h1>Nothing is published here.</h1>
        <p>
          This testimonial is unavailable. It may have been withdrawn or
          expired.
        </p>
      </section>
    );
  return (
    <section className="card">
      <p className="eyebrow">CUSTOMER-APPROVED TESTIMONIAL</p>
      <h1>In their own words.</h1>
      <TestimonialPreview
        content={snapshot.data}
        clipBasePath={`/public/${encodeURIComponent(slug)}/clips`}
      />
    </section>
  );
}
export function OperatorReview() {
  const { interviewId = "" } = useParams();
  const client = useQueryClient();
  const key = ["operator-workflow", interviewId];
  const path = `/operator/interviews/${encodeURIComponent(interviewId)}`;
  const query = useQuery({
    queryKey: key,
    queryFn: () => api<WorkflowView>(`${path}/workflow`),
    refetchInterval: 5000,
  });
  const write = useMutation({
    mutationFn: (
      action: { type: "publish"; approval_id: string } | { type: "unpublish" },
    ) =>
      api<{ state: WorkflowView }>(`${path}/workflow`, {
        interview_id: interviewId,
        request_id: crypto.randomUUID(),
        expected: query.data!.revisions,
        action,
      }),
    onSuccess: (result) => client.setQueryData(key, result.state),
    onError: () => {
      void query.refetch();
    },
  });
  if (query.isPending)
    return (
      <div className="notice" role="status">
        Loading saved testimonial…
      </div>
    );
  if (query.isError || !query.data)
    return (
      <section className="card">
        <h1>Review is unavailable.</h1>
        <p>Sign in with operator access and reopen this invitation.</p>
        <Link to="/operator">Operator workspace</Link>
      </section>
    );
  const state = query.data;
  const approved = state.approval;
  const eligible = Boolean(
    approved &&
    state.content &&
    approved.content_revision === state.revisions.content &&
    approved.evidence_revision === state.revisions.evidence &&
    state.evidence_available &&
    ["supported", "unsupported", "ambiguous"].includes(state.check) &&
    !state.declined,
  );
  return (
    <section className="card">
      <p className="eyebrow">OPERATOR REVIEW</p>
      <h1>Publish an approved story.</h1>
      <Link className="text-link" to="/operator">
        Back to invitations
      </Link>
      {write.error && (
        <div className="notice error" role="alert">
          {write.error.message} The latest saved state is shown; review it
          before trying again.
        </div>
      )}
      {state.content ? (
        <TestimonialPreview
          content={state.content}
          clipBasePath={`${path}/clips`}
        />
      ) : (
        <div className="notice" role="status">
          No draft is available yet.
        </div>
      )}
      {!eligible && (
        <div className="notice" role="status">
          Publication requires current customer approval, an available recording
          and completed automatic checks.
        </div>
      )}
      {["unsupported", "ambiguous"].includes(state.check) && (
        <div className="notice" role="status">
          Some wording may differ from the interview. The customer can approve
          their testimonial as written; the recording does not verify every
          statement in their edited text.
        </div>
      )}
      {state.declined && <p>The customer declined this testimonial.</p>}
      {state.published_approval_id ? (
        <>
          <div className="notice" role="status">
            Published with customer approval.
          </div>
          <div className="actions">
            <Link className="button" to={`/t/${interviewId}`}>
              View public testimonial
            </Link>
            <a className="button secondary" href={apiUrl(`${path}/export`)}>
              Download exact text
            </a>
            <button
              className="secondary"
              disabled={write.isPending}
              onClick={() => {
                if (
                  window.confirm(
                    "Unpublish this testimonial? Hosted access will stop; private evidence is retained.",
                  )
                )
                  write.mutate({ type: "unpublish" });
              }}
            >
              Unpublish
            </button>
          </div>
        </>
      ) : (
        <button
          disabled={!eligible || write.isPending}
          onClick={() => {
            if (approved)
              write.mutate({ type: "publish", approval_id: approved.id });
          }}
        >
          Publish approved testimonial
        </button>
      )}
      <details>
        <summary>Recording troubleshooting</summary>
        <p>
          Recording and text checks normally run automatically. Use this repair
          tool only when a recording cannot be verified. The customer reviews
          and approves their own testimonial; this is not draft approval.
        </p>
        <AlignmentReview interviewId={interviewId} state={state} />
      </details>
      <p className="small muted">
        Customer approval and operator publication are separate actions. Later
        content edits withdraw the hosted testimonial and require fresh
        approval.
      </p>
    </section>
  );
}
