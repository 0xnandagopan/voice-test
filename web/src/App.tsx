import {
  useEffect,
  useRef,
  useState,
  type FormEvent,
  type ReactNode,
} from "react";
import { Link, Route, Routes, useNavigate, useParams } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  api,
  ApiError,
  sessionKey,
  sessionQuery,
  type SessionView,
} from "./api";
import { AudioController, browserAudioDependencies } from "./audio";
import { Review } from "./Review";
import { OperatorReview, PublicTestimonial } from "./Publication";
import {
  connectRelay,
  type RelayEvent,
  type RelayTransport,
} from "./voice-transport";

function useAudioController() {
  const ref = useRef<AudioController | null>(null);
  useEffect(() => {
    const controller = new AudioController(browserAudioDependencies());
    ref.current = controller;
    return () => {
      controller.dispose();
      ref.current = null;
    };
  }, []);
  return ref;
}
function Notice({
  children,
  error = false,
}: {
  children: ReactNode;
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
function ErrorNotice({ error }: { error: Error | null }) {
  return error ? <Notice error>{error.message}</Notice> : null;
}
function Shell({
  children,
  operator = false,
}: {
  children: ReactNode;
  operator?: boolean;
}) {
  return (
    <>
      <header>
        <Link className="wordmark" to={operator ? "/operator" : "/"}>
          <span className="brand-icon">v</span> voice
          <span className="brand-description">TESTIMONIAL STUDIO</span>
        </Link>
        <span className="header-note">
          {operator ? "Operator workspace" : "Your experience. Your words."}
        </span>
      </header>
      <main>{children}</main>
      <footer>
        <span>Thoughtful conversations. Stories you control.</span>
        <span>Private by default</span>
      </footer>
    </>
  );
}
function Steps({ current = 1 }: { current?: number }) {
  return (
    <ol className="steps" aria-label="Interview progress">
      {["Welcome", "Sound check", "Conversation", "Your review"].map(
        (label, index) => (
          <li
            key={label}
            className={current === index + 1 ? "current" : ""}
            aria-current={current === index + 1 ? "step" : undefined}
          >
            <span>{index + 1}</span>
            {label}
          </li>
        ),
      )}
    </ol>
  );
}
function Pending() {
  return <Notice>Loading your private workspace…</Notice>;
}
function AccessError({ error }: { error: Error }) {
  return (
    <section className="card">
      <p className="eyebrow">PRIVATE INVITATION</p>
      <h1>We couldn’t open this invitation.</h1>
      <p>
        It may have expired or been withdrawn. Ask the person who invited you
        for a new link.
      </p>
      <ErrorNotice error={error} />
    </section>
  );
}

function Operator() {
  const client = useQueryClient();
  const auth = useQuery({
    queryKey: ["operator"],
    queryFn: () => api<{ username: string }>("/operator/me"),
  });
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const login = useMutation({
    mutationFn: () => api("/operator/login", { username, password }),
    onSuccess: () => {
      setPassword("");
      void client.invalidateQueries({ queryKey: ["operator"] });
    },
  });
  if (auth.isPending)
    return (
      <Shell operator>
        <Pending />
      </Shell>
    );
  if (auth.isError)
    return (
      <Shell operator>
        <section className="login card">
          <p className="eyebrow">OPERATOR ACCESS</p>
          <h1>Welcome back.</h1>
          <p>Invite a customer to share their experience in their own words.</p>
          {!(auth.error instanceof ApiError && auth.error.status === 401) && (
            <ErrorNotice error={auth.error} />
          )}
          <form
            onSubmit={(e) => {
              e.preventDefault();
              login.mutate();
            }}
          >
            <label>
              Username
              <input
                autoComplete="username"
                value={username}
                onChange={(e) => setUsername(e.target.value)}
                required
              />
            </label>
            <label>
              Password
              <input
                type="password"
                autoComplete="current-password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                required
              />
            </label>
            <ErrorNotice error={login.error} />
            <button disabled={login.isPending}>
              {login.isPending ? "Signing in…" : "Sign in"}
            </button>
          </form>
        </section>
      </Shell>
    );
  return <Dashboard username={auth.data.username} />;
}
function InvitationCopy({ item }: { item: SessionView }) {
  const [copied, setCopied] = useState(false);
  const [fallbackUrl, setFallbackUrl] = useState("");
  const copy = useMutation({
    mutationFn: async () => {
      setCopied(false);
      setFallbackUrl("");
      const { private_url } = await api<{ private_url: string }>(
        `/operator/invitations/${item.id}/link`,
      );
      try {
        await navigator.clipboard.writeText(private_url);
        setCopied(true);
      } catch {
        // Keep a selectable fallback in this mounted row, never browser storage.
        setFallbackUrl(private_url);
      }
    },
  });
  return (
    <div className="invitation-copy">
      <button
        className="secondary"
        aria-label={`Copy invitation link for ${item.customer_label}`}
        disabled={copy.isPending}
        onClick={() => copy.mutate()}
      >
        {copy.isPending
          ? "Getting link…"
          : copied
            ? "Link copied"
            : "Copy link"}
      </button>
      {copied && (
        <span role="status" className="small">
          Private link copied.
        </span>
      )}
      <ErrorNotice error={copy.error} />
      {fallbackUrl && (
        <>
          <label>
            Private invitation link for {item.customer_label}
            <input
              readOnly
              value={fallbackUrl}
              onFocus={(e) => e.currentTarget.select()}
            />
          </label>
          <p role="status" className="small">
            Clipboard is unavailable. Select and copy this private link, then
            share it directly with your customer.
          </p>
        </>
      )}
    </div>
  );
}
function Dashboard({ username }: { username: string }) {
  const client = useQueryClient();
  const invitations = useQuery({
    queryKey: ["invitations"],
    queryFn: () => api<{ invitations: SessionView[] }>("/operator/invitations"),
  });
  const [label, setLabel] = useState("");
  const [context, setContext] = useState("");
  const [privateUrl, setPrivateUrl] = useState("");
  const [copied, setCopied] = useState(false);
  const [copyError, setCopyError] = useState(false);
  const idempotency = useRef<string | null>(null);
  const create = useMutation({
    mutationFn: () =>
      api<{ invitation: SessionView; private_url: string }>(
        "/operator/invitations",
        {
          customer_label: label,
          project_context: context,
          idempotency_key: (idempotency.current ??= crypto.randomUUID()),
        },
      ),
    onSuccess: (data) => {
      setPrivateUrl(data.private_url);
      setCopied(false);
      setCopyError(false);
      setLabel("");
      setContext("");
      idempotency.current = null;
      void client.invalidateQueries({ queryKey: ["invitations"] });
    },
  });
  const revoke = useMutation({
    mutationFn: (item: SessionView) =>
      api(`/operator/invitations/${item.id}/revoke`, {
        expected_revision: item.revision,
      }),
    onSuccess: () =>
      void client.invalidateQueries({ queryKey: ["invitations"] }),
    onError: () => void client.invalidateQueries({ queryKey: ["invitations"] }),
  });
  const logout = useMutation({
    mutationFn: () => api("/operator/logout", {}),
    onSuccess: () => {
      client.clear();
      window.location.assign("/operator");
    },
  });
  function submit(e: FormEvent) {
    e.preventDefault();
    setPrivateUrl("");
    create.mutate();
  }
  return (
    <Shell operator>
      <div className="page-heading">
        <div>
          <p className="eyebrow">WORKSPACE · {username}</p>
          <h1>Make room for their story.</h1>
          <p>A short conversation. A testimonial they get to approve.</p>
        </div>
        <button
          className="quiet"
          onClick={() => logout.mutate()}
          disabled={logout.isPending}
        >
          Sign out
        </button>
      </div>
      <ErrorNotice error={logout.error} />
      <div className="dashboard-grid">
        <section className="card">
          <p className="eyebrow">01 / START A CONVERSATION</p>
          <h2>Invite a customer</h2>
          <form onSubmit={submit}>
            <label>
              Customer name
              <input
                value={label}
                maxLength={120}
                placeholder="e.g. Alex at Studio North"
                onChange={(e) => {
                  setLabel(e.target.value);
                  idempotency.current = null;
                }}
                required
              />
            </label>
            <label>
              Project context
              <textarea
                value={context}
                maxLength={2000}
                rows={4}
                placeholder="What did you work on together?"
                onChange={(e) => {
                  setContext(e.target.value);
                  idempotency.current = null;
                }}
                required
              />
            </label>
            <p className="small muted">
              This context is shared with your customer and helps guide the
              conversation.
            </p>
            <ErrorNotice error={create.error} />
            <button disabled={create.isPending}>
              {create.isPending ? "Creating…" : "Create private invitation"}
              <span aria-hidden>↗</span>
            </button>
          </form>
          {privateUrl && (
            <div className="link-result">
              <label>
                Private invitation link
                <input
                  readOnly
                  value={privateUrl}
                  onFocus={(e) => e.currentTarget.select()}
                />
              </label>
              <button
                className="secondary"
                onClick={async () => {
                  try {
                    await navigator.clipboard.writeText(privateUrl);
                    setCopied(true);
                    setCopyError(false);
                  } catch {
                    setCopyError(true);
                  }
                }}
              >
                {copied ? "Link copied" : "Copy link"}
              </button>
              <p className="small">
                Anyone with this link can access this customer’s session. Share
                it directly with them.
              </p>
              {copyError && (
                <Notice error>
                  Clipboard is unavailable. Select and copy the link above.
                </Notice>
              )}
            </div>
          )}
        </section>
        <section className="card">
          <p className="eyebrow">02 / FOLLOW THE CONVERSATION</p>
          <h2>Invitations</h2>
          <ErrorNotice error={invitations.error} />
          <ErrorNotice error={revoke.error} />
          {invitations.isPending ? (
            <Pending />
          ) : invitations.data?.invitations.length ? (
            <ul className="invitation-list">
              {invitations.data.invitations.map((item) => (
                <li key={item.id}>
                  <div className="invitation-details">
                    <strong>{item.customer_label}</strong>
                    <p>{item.project_context}</p>
                    <span className="badge">{item.state}</span>
                    <Link
                      className="text-link"
                      to={`/operator/interviews/${item.id}`}
                    >
                      Review testimonial
                    </Link>
                    <span className="small muted">
                      {" "}
                      Expires {new Date(item.expires_at).toLocaleDateString()}
                    </span>
                    {!["revoked", "deleted"].includes(item.state) &&
                      Date.parse(item.expires_at) > Date.now() && (
                        <InvitationCopy item={item} />
                      )}
                  </div>
                  {!["revoked", "deleted"].includes(item.state) && (
                    <button
                      className="quiet danger"
                      disabled={revoke.isPending}
                      onClick={() => {
                        if (
                          window.confirm(
                            `Revoke ${item.customer_label}’s invitation? Their link will stop working.`,
                          )
                        )
                          revoke.mutate(item);
                      }}
                    >
                      Revoke
                    </button>
                  )}
                </li>
              ))}
            </ul>
          ) : (
            <div className="empty">
              <span className="empty-mark" aria-hidden>
                ↗
              </span>
              <h3>Your first story starts here.</h3>
              <p>
                Create an invitation, then share the private link with your
                customer.
              </p>
            </div>
          )}
        </section>
      </div>
    </Shell>
  );
}

function Invitation() {
  const { invitationId } = useParams();
  const client = useQueryClient();
  const [exchange, setExchange] = useState<"pending" | "done" | "failed">(
    "pending",
  );
  const [error, setError] = useState<Error | null>(null);
  const started = useRef(false);
  useEffect(() => {
    if (started.current) return;
    started.current = true;
    const token = new URLSearchParams(window.location.hash.slice(1)).get(
      "token",
    );
    window.history.replaceState(
      null,
      "",
      window.location.pathname + window.location.search,
    );
    if (!token) {
      setExchange("done");
      return;
    }
    api("/customer/exchange", { invitation_id: invitationId, token })
      .then(() => {
        client.removeQueries({ queryKey: sessionKey });
        setExchange("done");
      })
      .catch((e: Error) => {
        setError(e);
        setExchange("failed");
      });
  }, [invitationId, client]);
  return (
    <Shell>
      {exchange === "pending" ? (
        <Pending />
      ) : exchange === "failed" ? (
        <AccessError error={error!} />
      ) : (
        <Welcome invitationId={invitationId} />
      )}
    </Shell>
  );
}
function Welcome({ invitationId }: { invitationId?: string }) {
  const session = useQuery(sessionQuery);
  const client = useQueryClient();
  const [accepted, setAccepted] = useState(false);
  const consent = useMutation({
    mutationFn: () =>
      api<SessionView>("/customer/consent", {
        interview_id: session.data?.id,
        policy_version: "recording-v1",
      }),
    onSuccess: (data) => client.setQueryData(sessionKey, data),
  });
  if (session.isPending) return <Pending />;
  if (session.isError) return <AccessError error={session.error} />;
  if (invitationId && session.data.id !== invitationId)
    return (
      <AccessError
        error={
          new Error(
            "This invitation does not match your active session. Open your complete private link again.",
          )
        }
      />
    );
  if (["revoked", "deleted"].includes(session.data.state))
    return (
      <AccessError
        error={new Error("This invitation is no longer available.")}
      />
    );
  const hasConsent = Boolean(session.data.consented_at);
  return (
    <>
      <Steps current={hasConsent ? 2 : 1} />
      <div className="customer-grid">
        <section>
          <p className="eyebrow">
            AN INVITATION FROM {session.data.agency_name}
          </p>
          <h1>
            A little conversation.
            <br />
            <em>A story only you can tell.</em>
          </h1>
          <p className="lead">
            Hi {session.data.customer_label}. We’d love to hear about your
            experience. Take a moment, settle in, and share what stood out.
          </p>
          <div className="context">
            <span className="eyebrow">WHAT WE’LL TALK ABOUT</span>
            <p>{session.data.project_context}</p>
          </div>
          <div className="facts">
            <span>◷ Usually 3–5 minutes</span>
            <span>◎ 3 simple topics</span>
            <span>✓ You approve every word</span>
          </div>
          <p className="muted small">
            Six-minute maximum · English · Desktop or Android Chrome · A quiet
            spot helps
          </p>
        </section>
        <section className="card consent-card">
          {hasConsent ? (
            <>
              <Readiness session={session.data} />
              <Link className="text-link" to={`/review/${session.data.id}`}>
                View saved recording and review
              </Link>
            </>
          ) : (
            <>
              <span className="card-icon" aria-hidden>
                ◉
              </span>
              <h2>Your voice, your choice.</h2>
              <p>
                You’ll speak with an AI interviewer. With your permission, your
                voice will be recorded and transcribed to prepare a testimonial.
              </p>
              <ul className="disclosures">
                <li>
                  Recording starts only after you consent and start the
                  interview. You can stop at any time.
                </li>
                <li>
                  You review and approve the exact text before it can be
                  published. Audio clips are excluded unless you choose them.
                </li>
                <li>
                  Nothing is published automatically. Your invitation and source
                  recording remain private.
                </li>
                <li>
                  Unfinished access expires after 14 inactive days. Completed
                  content expires after 30 days unless extended before expiry.
                </li>
                <li>
                  For removal, contact the person who invited you. Pilot removal
                  requests are handled within 24 hours.
                </li>
              </ul>
              <form
                onSubmit={(e) => {
                  e.preventDefault();
                  if (accepted) consent.mutate();
                }}
              >
                <label className="checkbox">
                  <input
                    type="checkbox"
                    checked={accepted}
                    onChange={(e) => setAccepted(e.target.checked)}
                  />
                  <span>
                    I consent to recording and transcription of this AI
                    interview for a testimonial I will review.
                  </span>
                </label>
                <ErrorNotice error={consent.error} />
                <button disabled={!accepted || consent.isPending}>
                  {consent.isPending
                    ? "Saving your consent…"
                    : "I agree — check my sound"}
                  <span aria-hidden>→</span>
                </button>
              </form>
              <p className="small muted">
                Microphone access is requested only after consent is saved.
              </p>
            </>
          )}
        </section>
      </div>
    </>
  );
}
function Readiness({ session }: { session: SessionView }) {
  const navigate = useNavigate();
  const controller = useAudioController();
  const [mic, setMic] = useState<"idle" | "checking" | "ready" | "error">(
    "idle",
  );
  const [error, setError] = useState<Error | null>(null);
  const [tone, setTone] = useState(false);
  const [heard, setHeard] = useState(false);
  const disposed = useRef(false);
  useEffect(() => {
    disposed.current = false;
    return () => {
      disposed.current = true;
    };
  }, []);
  async function check() {
    setMic("checking");
    setError(null);
    try {
      await controller.current!.checkMicrophone(Boolean(session.consented_at));
      if (!disposed.current) setMic("ready");
    } catch (e) {
      if (!disposed.current) {
        setMic("error");
        setError(
          e instanceof Error ? e : new Error("Microphone access failed."),
        );
      }
    }
  }
  return (
    <>
      <span className="card-icon" aria-hidden>
        ♫
      </span>
      <h2>Let’s check your sound.</h2>
      <p>
        Your recording consent is saved. These local checks do not send audio to
        the interviewer.
      </p>
      <div className="check-row">
        <div>
          <h3>1. Check your microphone</h3>
          <p className="small">
            Allow access when your browser asks. We release the microphone as
            soon as the check finishes.
          </p>
        </div>
        <button
          className="secondary"
          onClick={() => void check()}
          disabled={mic === "checking"}
        >
          {mic === "checking"
            ? "Checking…"
            : mic === "ready"
              ? "Check again"
              : mic === "error"
                ? "Retry microphone"
                : "Check microphone"}
        </button>
      </div>
      {mic === "ready" && (
        <Notice>
          Microphone is accessible. You can continue after the speaker check.
        </Notice>
      )}
      <ErrorNotice error={error} />
      {mic === "error" && (
        <p className="small">
          Allow microphone access in your browser’s site settings, connect a
          microphone, then retry. Use HTTPS or localhost.
        </p>
      )}
      <div className="check-row">
        <div>
          <h3>2. Check your speaker</h3>
          <p className="small">
            Turn up the volume or put on headphones, then play a short tone.
          </p>
        </div>
        <button
          className="secondary"
          onClick={async () => {
            try {
              await controller.current!.playTestTone();
              setTone(true);
              setError(null);
            } catch (e) {
              setError(
                e instanceof Error ? e : new Error("Speaker check failed."),
              );
            }
          }}
        >
          Play test tone
        </button>
      </div>
      <label className="checkbox">
        <input
          type="checkbox"
          checked={heard}
          disabled={!tone}
          onChange={(e) => setHeard(e.target.checked)}
        />
        <span>I heard the test tone.</span>
      </label>
      <button
        disabled={mic !== "ready" || !heard}
        onClick={() => navigate("/interview")}
      >
        Continue to conversation <span aria-hidden>→</span>
      </button>
      <p className="small muted">
        The conversation begins only when you choose Start interview.
      </p>
    </>
  );
}
function Interview() {
  const session = useQuery({
    ...sessionQuery,
    // Local Stop releases audio before the server finishes recording recovery.
    // Keep refreshing an occupied lease until the authoritative state settles.
    refetchInterval: (query) =>
      query.state.data?.state === "interviewing" ? 2000 : false,
  });
  const controller = useAudioController();
  const [stopped, setStopped] = useState(false);
  const [active, setActive] = useState(false);
  const [ended, setEnded] = useState(false);
  const [completed, setCompleted] = useState(false);
  const [budgetExhausted, setBudgetExhausted] = useState(false);
  const [canFinish, setCanFinish] = useState(false);
  const [finishing, setFinishing] = useState(false);
  const [error, setError] = useState<Error | null>(null);
  const [remaining, setRemaining] = useState<number | null>(null);
  const [captions, setCaptions] = useState<
    Record<string, Extract<RelayEvent, { type: "caption" }>>
  >({});
  const transport = useRef<RelayTransport | null>(null);
  const connection = useRef<AbortController | null>(null);
  const pinnedInterview = useRef<string | null>(null);
  if (!pinnedInterview.current && session.data)
    pinnedInterview.current = session.data.id;
  useEffect(
    () => () => {
      connection.current?.abort();
    },
    [],
  );
  function stop() {
    controller.current?.stop();
    connection.current?.abort();
    transport.current = null;
    setActive(false);
    setStopped(true);
    setCanFinish(false);
    setFinishing(false);
    void session.refetch();
  }
  const start = useMutation({
    mutationFn: async () => {
      if (
        !session.data?.consented_at ||
        session.data.id !== pinnedInterview.current
      )
        throw new Error(
          "Open the correct private invitation and confirm recording consent first.",
        );
      const attempt = new AbortController();
      connection.current = attempt;
      const result = await api<{ ws_url: string }>("/customer/start", {
        interview_id: pinnedInterview.current,
        expected_revision: session.data.revision,
      });
      if (attempt.signal.aborted) return;
      if (!result.ws_url)
        throw new Error(
          "The live voice connection is not available. No recording has started.",
        );
      await controller.current!.start({
        consented: Boolean(session.data.consented_at),
        connect: async (onAudio) => {
          const relay = await connectRelay(
            result.ws_url,
            onAudio,
            (event) => {
              if (attempt.signal.aborted) return;
              if (event.type === "ready" || event.type === "state") {
                setRemaining(event.remaining_seconds);
                setCanFinish(event.can_finish === true);
              }
              if (event.type === "control_rejected") {
                setFinishing(false);
                setError(
                  new Error(
                    "That control was not applied because the conversation changed. Check the current question and try again. You can still pause at any time.",
                  ),
                );
              }
              if (event.type === "caption")
                setCaptions((previous) => ({
                  ...previous,
                  [event.item_id]: event,
                }));
              if (event.type === "ended") {
                setActive(false);
                setEnded(true);
                setCompleted(event.reason === "explicit_finish");
                setBudgetExhausted(event.reason === "budget_exhausted");
                if (event.reason === "budget_exhausted") setRemaining(0);
                setCanFinish(false);
                setFinishing(false);
                void session.refetch();
              }
              if (event.type === "error") {
                setActive(false);
                setEnded(true);
                setCanFinish(false);
                setFinishing(false);
                void session.refetch();
                setError(
                  new Error(
                    "The connection stopped. Your microphone has been released. Check recording recovery before continuing.",
                  ),
                );
              }
            },
            attempt.signal,
          );
          transport.current = relay;
          return relay;
        },
      });
      if (!attempt.signal.aborted && controller.current?.state === "active")
        setActive(true);
    },
  });
  const scopeMismatch =
    session.data && pinnedInterview.current !== session.data.id;
  const requiresRecovery = session.data?.state === "recovering";
  const existingSession = session.data?.state === "interviewing" && !active;
  const processing =
    session.data?.state === "completed" ||
    session.data?.state === "processing" ||
    session.data?.state === "draft";
  const connecting = start.isPending && !stopped && !ended;
  const showStop = active || connecting || finishing;
  const reviewRequired =
    stopped ||
    ended ||
    requiresRecovery ||
    existingSession ||
    processing ||
    session.data?.remaining_seconds === 0;
  const canOfferStart = !active && !reviewRequired;
  const continuing = (session.data?.remaining_seconds ?? 360) < 360;
  return (
    <Shell>
      <Steps current={3} />
      <section className="interview card">
        {session.isPending ? (
          <Pending />
        ) : session.isError ? (
          <AccessError error={session.error} />
        ) : scopeMismatch ? (
          <AccessError
            error={
              new Error(
                "Your active invitation changed. Reopen the correct private link.",
              )
            }
          />
        ) : !session.data.consented_at ? (
          <>
            <h1>Consent comes first.</h1>
            <p>Return to your invitation to review the recording disclosure.</p>
            <Link className="button" to={`/i/${session.data.id}`}>
              Review consent
            </Link>
          </>
        ) : (
          <>
            <p className="eyebrow">YOUR CONVERSATION</p>
            <div className="voice-orb" aria-hidden>
              <span />
              <span />
              <span />
              <span />
              <span />
            </div>
            <h1>
              {active
                ? "Tell us your story."
                : completed || session.data.state === "completed"
                  ? "Your interview is complete."
                  : processing
                    ? "Your interview is ready for review."
                    : reviewRequired
                      ? "Review recording recovery."
                      : "Ready when you are."}
            </h1>
            <p>
              {completed || processing ? (
                "Review your recordings and prepare the testimonial you want to share."
              ) : reviewRequired ? (
                "Review the available recordings before deciding what to do next."
              ) : (
                <>
                  Three topics. Up to{" "}
                  {Math.ceil(
                    (remaining ?? session.data.remaining_seconds) / 60,
                  )}{" "}
                  minutes remaining. Tell us what worked, what changed, and what
                  could have been better.
                </>
              )}
            </p>
            {!session.data.voice_available && (
              <Notice>
                Live interviews are not ready in this build. No recording is in
                progress.
              </Notice>
            )}
            {active && (
              <Notice>
                Recording is active. Pause stops your microphone and playback;
                Finish ends the interview.
              </Notice>
            )}
            <ErrorNotice error={start.error} />
            <ErrorNotice error={error} />
            {stopped && (
              <Notice>
                Paused locally. No microphone or playback is active. Review
                recording recovery before resuming; your used time and question
                counts are preserved.
              </Notice>
            )}
            {ended && (
              <Notice>
                {completed
                  ? "The server confirmed the interview is complete. Recording recovery may still be in progress; completion does not approve or publish a testimonial."
                  : budgetExhausted
                    ? "Your six-minute interview allowance is used. Recording has stopped. Review the available recording; reconnecting cannot reset the allowance."
                    : "The connection has ended. Review recording recovery before continuing. Available recordings may still be recovering."}
              </Notice>
            )}
            {requiresRecovery && !ended && (
              <Notice>
                Review and acknowledge recording recovery before resuming. Your
                used time and question counts are preserved.
              </Notice>
            )}
            {existingSession && (
              <Notice>
                A conversation may still be active or awaiting recovery. No
                microphone is active in this tab. Review recording recovery
                before trying again.
              </Notice>
            )}
            {processing && !ended && (
              <Notice>
                Your interview is ready for recording review. No recording
                starts automatically.
              </Notice>
            )}
            <div
              className={`conversation-controls ${showStop || reviewRequired ? "persistent-controls" : ""}`}
              role="group"
              aria-label="Conversation controls"
            >
              {canOfferStart && (
                <button
                  disabled={
                    start.isPending ||
                    session.data.state !== "consented" ||
                    !session.data.voice_available ||
                    session.data.remaining_seconds <= 0
                  }
                  onClick={() => {
                    setError(null);
                    setStopped(false);
                    start.mutate();
                  }}
                >
                  {connecting
                    ? "Connecting…"
                    : continuing
                      ? "Resume interview"
                      : "Start interview"}
                </button>
              )}
              {showStop && (
                <button className="secondary" onClick={stop}>
                  Pause interview
                </button>
              )}
              {reviewRequired && !showStop && (
                <Link className="button" to={`/review/${session.data.id}`}>
                  {completed || processing
                    ? "Review testimonial"
                    : "Review recording recovery"}
                </Link>
              )}
              {active && (
                <>
                  <button
                    className="secondary"
                    disabled={finishing}
                    onClick={() => {
                      try {
                        transport.current?.control("repeat");
                      } catch {
                        stop();
                      }
                    }}
                  >
                    Repeat question
                  </button>
                  <button
                    className="secondary"
                    disabled={finishing}
                    onClick={() => {
                      try {
                        transport.current?.control("skip");
                      } catch {
                        stop();
                      }
                    }}
                  >
                    Skip topic
                  </button>
                  <button
                    disabled={!canFinish || finishing}
                    onClick={() => {
                      try {
                        if (!transport.current)
                          throw new Error(
                            "The voice connection is unavailable.",
                          );
                        transport.current.control("finish");
                        setFinishing(true);
                        setError(null);
                      } catch (reason) {
                        setFinishing(false);
                        setError(
                          reason instanceof Error
                            ? reason
                            : new Error(
                                "Finish could not be requested. You can still pause at any time.",
                              ),
                        );
                      }
                    }}
                  >
                    {finishing ? "Finishing…" : "Finish interview"}
                  </button>
                </>
              )}
            </div>
            {Object.keys(captions).length > 0 && (
              <section
                className="live-captions"
                aria-label="Provisional conversation captions"
              >
                <h2>Live captions</h2>
                <p className="small">
                  Provisional captions are not saved recording evidence.
                </p>
                {Object.values(captions).map((caption) => (
                  <p key={caption.item_id}>
                    <strong>
                      {caption.speaker === "customer" ? "You" : "Interviewer"}
                      :{" "}
                    </strong>
                    {caption.text}
                    {!caption.final && " …"}
                  </p>
                ))}
              </section>
            )}
            {!showStop && !reviewRequired && (
              <nav className="actions" aria-label="Conversation navigation">
                <Link className="text-link" to={`/review/${session.data.id}`}>
                  View saved recording and review
                </Link>
                <Link className="text-link" to={`/i/${session.data.id}`}>
                  Back to sound check
                </Link>
              </nav>
            )}
            <p className="small muted">
              A saved recording or recoverable evidence will only be shown after
              server confirmation.
            </p>
          </>
        )}
      </section>
    </Shell>
  );
}
function Landing() {
  return (
    <Shell>
      <section className="card placeholder">
        <p className="eyebrow">VOICE TESTIMONIAL STUDIO</p>
        <h1>Every experience has a story.</h1>
        <p>Open your private invitation link to share yours.</p>
        <Link className="text-link" to="/operator">
          Operator sign in →
        </Link>
      </section>
    </Shell>
  );
}
export function App() {
  return (
    <Routes>
      <Route path="/" element={<Landing />} />
      <Route path="/operator" element={<Operator />} />
      <Route
        path="/operator/interviews/:interviewId"
        element={
          <Shell operator>
            <OperatorReview />
          </Shell>
        }
      />
      <Route path="/i/:invitationId" element={<Invitation />} />
      <Route path="/interview" element={<Interview />} />
      <Route
        path="/review"
        element={
          <Shell>
            <Review />
          </Shell>
        }
      />
      <Route
        path="/review/:interviewId"
        element={
          <Shell>
            <Review />
          </Shell>
        }
      />
      <Route
        path="/t/:slug"
        element={
          <Shell>
            <PublicTestimonial />
          </Shell>
        }
      />
      <Route path="*" element={<Landing />} />
    </Routes>
  );
}
