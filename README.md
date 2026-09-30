# Voice Testimonial Interviewer

A voice interview and review workflow for turning customer experiences into customer-approved testimonials. Built for one agency, one operator, English interviews, desktop Chrome and Android Chrome.

The operator invites a customer, the customer consents to an interview and approves their exact testimonial, and the operator separately chooses whether to publish it.

## Release scope

**Scope frozen on 30 September 2026** for the v0 deployment candidate. This is a feature baseline, not a claim that production deployment or every acceptance gate is complete.

Included in this baseline:

- Private invitations with copy/revoke controls, expiry and interview/publication status.
- Project context and private operator attachments (`.txt`, `.md`, `.json`) used to prepare relevant interview questions. Attachments are limited to five files, 32 KiB each and 96 KiB total.
- Explicit recording consent, sound check and a live question card with Pause/Resume, Repeat, Skip and Finish controls alongside the conversation transcript.
- Three interview topics, at most two follow-ups per topic and a six-minute cumulative allowance preserved across reconnects.
- Provider-backed recording recovery across interrupted attempts, original transcripts and private source playback.
- Generated drafts, manual editing, optional recorded clips and automatic comparison notes. Completed comparisons that identify unsupported or ambiguous wording are advisory; customers can approve their exact saved wording. Pending/failed checks and unavailable evidence remain technical blocks.
- Exact customer approval followed by separate operator publication and export. Content edits invalidate approval and withdraw the hosted version; fresh approval is required before republication.
- Unpublish, withdrawal, expiry and deletion workflows.

During the deployment phase, changes are limited to deployment blockers, regressions, security fixes, configuration, packaging and documentation. New features and further visual redesigns are deferred to a subsequent release scope.

Independent live audio persistence, a production S3 adapter, multi-agency accounts, additional languages and additional supported browsers are outside this baseline. Real-device acceptance and deployment checks below remain separate from the scope freeze.

## Architecture

| Component | Implementation |
| --- | --- |
| Browser | React, TypeScript, Vite, React Router, TanStack Query |
| Application API | Rust, Axum, Tokio; serves the built browser app |
| Authority and jobs | PostgreSQL with SQLx migrations and durable jobs |
| Worker | Separate Rust process for provider recovery, transcription, drafting, checks and media processing |
| Voice and models | AssemblyAI Voice Agent API, recorded transcription and LLM Gateway |
| Audio processing | FFmpeg and FFprobe |
| Current private storage | Local filesystem shared by the API and worker |

The live agent uses an authenticated custom-LLM endpoint to obtain questions from the server's bounded interview controller. The Gateway model prepares contextual questions and handles post-interview drafting/checking. Live captions are provisional; saved recordings and transcripts remain separate evidence.

**The current storage implementation is local, not S3.** API and worker must share the same persistent private evidence directory. Never serve that directory as public static files.

## Local setup

Run commands from the repository root unless stated otherwise. Prerequisites:

- Rust as pinned in [rust-toolchain.toml](rust-toolchain.toml).
- Node.js as pinned in [.node-version](.node-version), with npm.
- PostgreSQL 17 (the provided Compose service is for local development).
- FFmpeg and FFprobe on the worker's path.
- Python 3 and headless Chromium for the API/browser checks.

Install dependencies and build:

```bash
npm --prefix web ci
cargo build --locked -p v0-app -p v0-worker
npm --prefix web run build
docker compose up -d postgres
test -f .env || cp .env.example .env
chmod 600 .env
```

The Compose database binds to loopback and uses development credentials. Keep an existing `.env`; do not replace it with the example.

Generate an operator password hash using a password of at least 12 characters. This Bash sequence keeps the password out of shell history:

```bash
read -r -s -p 'Operator password: ' operator_password
printf '%s' "$operator_password" | ./target/debug/v0-app hash-password
unset operator_password
```

Copy the resulting hash into `OPERATOR_PASSWORD_HASH` in `.env`, enclosed in single quotes to preserve its dollar signs. Generate a separate invitation signing key with `openssl rand -hex 32` and save it as `INVITATION_SIGNING_KEY`. Keep that key stable across restarts.

### Configuration

Both processes load `.env` automatically; process environment variables take precedence. See [.env.example](.env.example) for the complete template. Never commit real credentials or customer data.

| Variable | Purpose |
| --- | --- |
| `DATABASE_URL` | PostgreSQL connection for API and worker |
| `BIND_ADDR` | API listener; defaults to `127.0.0.1:3000` |
| `APP_ORIGIN` | Exact browser origin, including scheme and optional port; configure only once |
| `COOKIE_SECURE` | `false` only for loopback HTTP development; `true` for HTTPS |
| `AGENCY_NAME` | Customer-facing agency name |
| `OPERATOR_USERNAME`, `OPERATOR_PASSWORD_HASH` | Operator login and generated Argon2 hash |
| `INVITATION_SIGNING_KEY` | Stable, random invitation signing secret |
| `VOICE_AGENT_API_KEY` | AssemblyAI API key, used only on the server |
| `GATEWAY_MODEL` | Exact model identifier accessible to your AssemblyAI account |
| `VOICE_TEST_ENABLED` | Defaults to `false`; the current live voice path requires `true` on a controlled instance |
| `VOICE_PUBLIC_ORIGIN` | Reachable HTTPS origin for the authenticated voice-agent custom-LLM callback |
| `EVIDENCE_STORAGE_DIR` | Shared persistent private evidence directory |
| `VOICE_ARTIFACT_HOSTS` | Exact trusted provider artifact hostnames; do not replace with an unrestricted allowlist |
| `FFMPEG_PATH`, `FFPROBE_PATH` | Media executable names or absolute paths |
| `EVIDENCE_WORK_DIR` | Worker media-processing scratch directory |
| `WEB_DIST` | Built frontend directory; defaults to `web/dist` |
| `RUST_LOG` | Runtime logging filter |

An accessible `GATEWAY_MODEL` is needed for question preparation, drafting and comparison jobs. The worker must be running for these jobs to finish. Provider access is account-specific; a successful browser build does not verify model access.

For local HTTP use `APP_ORIGIN=http://localhost:3000` and `COOKIE_SECURE=false`. For external voice testing, route an HTTPS origin to the API, set both `APP_ORIGIN` and `VOICE_PUBLIC_ORIGIN` to that origin, and set `COOKIE_SECURE=true`. The proxy must support WebSockets and allow AssemblyAI to reach the authenticated callback. Browser/operator access controls must not inadvertently block that callback.

### Run

Apply migrations once, then start the API:

```bash
./target/debug/v0-app migrate
./target/debug/v0-app
```

In a second terminal, from the same repository root:

```bash
./target/debug/v0-worker
```

Open `http://localhost:3000/operator` for loopback development. The API serves the built browser app; a separate Vite server is not required for this setup. Provider-enabled actions can incur usage charges.

## Verification

The project supports headless development. Install the browser used by the tests:

```bash
cd web
npx playwright install chromium
cd ..
```

On a minimal Linux host, Playwright may also require system browser dependencies. Use its `install --with-deps chromium` option where installing system packages is permitted.

Run static checks, ordinary Rust tests, the frontend build, audio-engine tests and mocked browser journeys:

```bash
make check
make test-browser
make test-media
```

For database/API checks, create a **separate disposable** PostgreSQL database, for example `v0_e2e`, and set `TEST_DATABASE_URL` to its connection string. The API harness requires a loopback database whose name ends in `_e2e`. Do not point tests at the application or production database.

```bash
export TEST_DATABASE_URL=postgres://v0:v0_local_only@127.0.0.1:55432/v0_e2e
make test-db
cargo test -p v0-app --test automatic_alignment_jobs --test alignment_http --locked -- --ignored
make test-api
```

`make test-api` builds and runs a temporary local API for real HTTP/browser walkthroughs. The alignment suites supplement `make test-db`; they are not included in that target. Provider probes are separate, opt-in operations.

Headless tests and synthetic microphone fixtures do not establish actual desktop/Android microphone quality, speaker interruption behavior or live provider recovery. Retain physical-device walkthroughs as separate acceptance evidence. Elapsed-time retention verification was deferred; automated lifecycle checks are not proof of that elapsed-time observation.

## Deployment handoff

This repository does not yet contain production container images, process supervision or a deployment pipeline. [compose.yaml](compose.yaml) starts only the development database.

Before exposing a release instance:

1. Select the host and final HTTPS origin. Run the API and worker as supervised processes from the same build, with private persistent storage accessible to both.
2. Build locked assets and release binaries:

   ```bash
   npm --prefix web ci
   npm --prefix web run build
   cargo build --release --locked -p v0-app -p v0-worker
   ```

3. Configure secrets outside Git and the build artifact. Set the final origins, secure cookies, provider/model access, storage paths and proxy WebSocket support.
4. Back up the database and private evidence together. Rehearse restore and schema-compatible rollback; apply `target/release/v0-app migrate` once before starting the compatible API and worker.
5. Resolve the SPA deep-link response issue: the current static fallback serves the app shell with HTTP 404 for paths such as `/operator`. Direct navigation and refresh must return the intended success response in the deployment configuration or API implementation.
6. Check API health/readiness, operator login, a complete authorized live interview, asynchronous processing, exact approval/publication and withdrawal of hosted text/audio. `/api/ready` checks the database; it does not certify worker or provider readiness.
7. Confirm backups, durable storage, worker restarts, retention/deletion behavior and the remaining real-device observations before widening access beyond a controlled release.

Do not configure shared caching that continues serving withdrawn testimonials or audio. Keep raw evidence private; public audio is limited to the explicitly approved bounded clips. Logs and release artifacts must not contain credentials, invitation secrets or customer transcripts.

## Repository map

- [`web/`](web/) — browser UI, audio engine and Playwright journeys.
- [`crates/app/`](crates/app/) — API, authentication, authority and workflow transactions.
- [`crates/domain/`](crates/domain/) — shared domain types and rules.
- [`crates/voice/`](crates/voice/) — voice provider integration and bounded interview logic.
- [`crates/evidence/`](crates/evidence/) — recording recovery and media handling.
- [`crates/composition/`](crates/composition/) — Gateway drafting and comparison integration.
- [`crates/worker/`](crates/worker/) — durable background processing and lifecycle work.
- [`migrations/`](migrations/) — PostgreSQL schema migrations.
- [`contracts/`](contracts/) — shared workflow contracts and fixtures.
- [`scripts/`](scripts/) — verification harnesses and opt-in provider probes.
