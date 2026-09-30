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

Independent live audio persistence, multi-agency accounts, additional languages and additional supported browsers are outside this baseline. Deployment adaptations add private S3-compatible storage and separate frontend/API hosting without expanding product features. Real-device acceptance and deployment checks below remain separate from the scope freeze.

## Architecture

| Component | Implementation |
| --- | --- |
| Browser | React, TypeScript, Vite, React Router, TanStack Query |
| Application API | Rust, Axum, Tokio; serves the built browser app |
| Authority and jobs | PostgreSQL with SQLx migrations and durable jobs |
| Worker | Separate Rust process for provider recovery, transcription, drafting, checks and media processing |
| Voice and models | AssemblyAI Voice Agent API, recorded transcription and LLM Gateway |
| Audio processing | FFmpeg and FFprobe |
| Private storage | Local filesystem for development; private S3-compatible bucket for separate hosted services |

The live agent uses an authenticated custom-LLM endpoint to obtain questions from the server's bounded interview controller. The Gateway model prepares contextual questions and handles post-interview drafting/checking. Live captions are provisional; saved recordings and transcripts remain separate evidence.

Local development defaults to a shared private evidence directory. Hosted API and worker use the same private S3-compatible bucket. Audio is served through application permission checks; the bucket and raw source files remain private.

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
| `BIND_ADDR`, `PORT` | Explicit listener takes precedence; otherwise `PORT` binds `0.0.0.0`, with loopback port 3000 as the local default |
| `APP_ORIGIN` | Exact browser origin, including scheme and optional port; configure only once |
| `COOKIE_SECURE` | `false` only for loopback HTTP development; `true` for HTTPS |
| `AGENCY_NAME` | Customer-facing agency name |
| `OPERATOR_USERNAME`, `OPERATOR_PASSWORD_HASH` | Operator login and generated Argon2 hash |
| `INVITATION_SIGNING_KEY` | Stable, random invitation signing secret |
| `VOICE_AGENT_API_KEY` | AssemblyAI API key, used only on the server |
| `GATEWAY_MODEL` | Exact model identifier accessible to your AssemblyAI account |
| `VOICE_TEST_ENABLED` | Defaults to `false`; the current live voice path requires `true` on a controlled instance |
| `VOICE_PUBLIC_ORIGIN` | Reachable HTTPS origin for the authenticated voice-agent custom-LLM callback |
| `EVIDENCE_STORAGE_BACKEND` | `local` for development; `s3` for the hosted services |
| `EVIDENCE_STORAGE_DIR` | Shared persistent private evidence directory when using local storage |
| `S3_ENDPOINT`, `S3_BUCKET`, `S3_REGION` | Private bucket connection settings from Railway |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | Bucket credentials; server-side only |
| `S3_FORCE_PATH_STYLE` | Use the URL style specified by your bucket; defaults to virtual-hosted style |
| `VOICE_ARTIFACT_HOSTS` | Exact trusted provider artifact hostnames; do not replace with an unrestricted allowlist |
| `FFMPEG_PATH`, `FFPROBE_PATH` | Media executable names or absolute paths |
| `EVIDENCE_WORK_DIR` | Worker media-processing scratch directory |
| `SERVE_WEB`, `WEB_DIST` | Serve built React locally by default; set `SERVE_WEB=false` for a Vercel frontend |
| `VITE_API_ORIGIN` | Frontend build-time API origin; empty uses the browser origin |
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
export FFMPEG_PATH=ffmpeg FFPROBE_PATH=ffprobe
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

## Railway + Vercel deployment

Selected domains:

- Frontend: `https://voice-test-app.trypreview.online` on Vercel.
- API and voice callback: `https://voice-test-api.trypreview.online` on Railway.

These are deployment targets, not evidence that DNS or services are already live. Both use HTTPS under the same parent domain. Session cookies remain host-only, HttpOnly and SameSite=Strict; only the configured frontend origin receives credentialed CORS access. Unrelated preview domains require separate configuration and verification.

### 1. Create data resources

In one Railway project/environment, create PostgreSQL (prefer version 17 to match development) and a private Storage Bucket. Choose regions near the API/worker; a bucket's region cannot be changed after creation. Keep PostgreSQL private and configure backups. Start with empty resources unless you separately plan to migrate the existing test database and its corresponding audio files.

The bucket Credentials tab supplies its actual S3 bucket name, endpoint, region and credentials. Use the actual bucket identifier rather than its display name. Use Railway variable references to supply values to both services. Never expose these settings through `VITE_` variables or commit a production `.env`.

### 2. Deploy API and worker from one commit

The root [Dockerfile](Dockerfile) builds both Rust executables and includes FFmpeg/FFprobe. [.dockerignore](.dockerignore) restricts the build context to compiler inputs. The runtime uses a non-root user and temporary scratch space; durable evidence goes to the bucket. The image defaults to the S3 backend and fails startup if its required storage configuration is missing.

Create two Railway services from this repository, with the repository root as the source directory and the root Dockerfile as the build definition. Configure the following in each service's dashboard:

| Setting | API | Worker |
| --- | --- | --- |
| Start command | `/usr/local/bin/v0-app` | `/usr/local/bin/v0-worker` |
| Pre-deploy command | `/usr/local/bin/v0-app migrate` | None |
| Healthcheck | `/api/ready` | No HTTP healthcheck |
| Public domain | `voice-test-api.trypreview.online` | None |
| Replicas | 1 initially | 1 initially |
| Restart policy | Always | Always |
| Serverless/sleep | Off | Off |

Configure a migration timeout with room for the database size. On the first deployment, run the API migrations successfully before starting the worker. Database schema changes must remain compatible with both services during subsequent rollouts. `/api/ready` checks PostgreSQL, not provider access or worker health; monitor job completion separately.

Use the same database connection and bucket settings in both services:

```text
DATABASE_URL=<reference to PostgreSQL private DATABASE_URL>
EVIDENCE_STORAGE_BACKEND=s3
S3_ENDPOINT=<bucket endpoint>
S3_BUCKET=<actual bucket name>
S3_REGION=<bucket region>
AWS_ACCESS_KEY_ID=<reference to bucket access key>
AWS_SECRET_ACCESS_KEY=<reference to bucket secret key>
S3_FORCE_PATH_STYLE=false
VOICE_AGENT_API_KEY=<AssemblyAI key>
FFMPEG_PATH=ffmpeg
FFPROBE_PATH=ffprobe
EVIDENCE_WORK_DIR=/tmp/voice-test-jobs
```

Follow the URL style in the bucket Credentials tab; older buckets may need `S3_FORCE_PATH_STYLE=true`. Provide the exact trusted provider artifact hostnames in `VOICE_ARTIFACT_HOSTS`. Add `GATEWAY_MODEL` to the worker using the accessible model already validated for drafting and question preparation.

Add these API settings:

```text
APP_ORIGIN=https://voice-test-app.trypreview.online
VOICE_PUBLIC_ORIGIN=https://voice-test-api.trypreview.online
COOKIE_SECURE=true
SERVE_WEB=false
VOICE_TEST_ENABLED=true
AGENCY_NAME=<your agency>
OPERATOR_USERNAME=<operator login>
OPERATOR_PASSWORD_HASH=<generated Argon2 hash>
INVITATION_SIGNING_KEY=<stable random signing secret>
```

Do not copy the local loopback `BIND_ADDR` into Railway. Let the API use Railway's `PORT`. Live voice retains its explicit enable switch; enabling it is not proof that all device acceptance checks have passed.

### 3. Deploy the browser on Vercel

Connect the same repository and select:

```text
Root directory: web
Install command: npm ci
Build command: npm run build
Output directory: dist
VITE_API_ORIGIN=https://voice-test-api.trypreview.online
```

[web/vercel.json](web/vercel.json) provides SPA routing. Set the API address before building; changing it requires a new frontend deployment. Backend credentials do not belong in Vercel. Configure the custom domain `voice-test-app.trypreview.online`.

### 4. Add Cloudflare DNS records

Add the custom domains in Railway and Vercel and copy the exact DNS targets they show. Create CNAME records for `voice-test-api` and `voice-test-app` in Cloudflare, plus any verification records requested by the host. Start with DNS-only records while each host verifies its domain and issues TLS certificates. Do not guess CNAME destinations or point them at the former local testing tunnel.

Confirm HTTPS works on both names before testing login. The API's authenticated AssemblyAI callback and browser voice WebSocket must remain reachable; do not place an interactive login challenge in front of those connections.

### 5. Verify and release

Before connecting customer traffic, verify operator login, invitation exchange, consent, live voice, tab-close recovery, draft processing, private audio, exact approval, publication and withdrawal on the deployed domains. Restart the worker and API and confirm recorded evidence persists. Check object deletion and database backup/restore procedures.

Local deployment checks include:

```bash
cargo test -p v0-app --test deployment --locked
cargo test -p v0-evidence --locked storage
(cd web && npx playwright test --config playwright.split.config.ts)
TEST_SPLIT_ORIGIN=true python3 scripts/test-headless-api.py
```

The split-origin harness requires the same disposable `TEST_DATABASE_URL` as the ordinary API tests, built Rust binaries and installed frontend dependencies. It builds a separate temporary React bundle and runs the real API against another local origin. It does not contact AssemblyAI or the hosted bucket.

A local image can be built with `docker build -t voice-test:deployment .`. A completed build or mocked S3 test does not prove Railway bucket compatibility or live voice quality. Validate the real bucket with synthetic files first, then run an authorized interview. With bucket variables explicitly set in the process environment (the test does not load `.env`), run:

```bash
cargo test -p v0-evidence --lib storage::s3::tests::live_s3_private_storage_round_trip --locked -- --ignored --exact
```

This opt-in probe writes one uniquely named synthetic object, checks readback and overwrite rejection, then removes it. It sends no customer recordings. It must pass before relying on Railway's conditional-write behavior for customer evidence.

Deployments should use a known commit for both services. Back up before migrations; application rollback is safe only while the schema remains compatible. Preserve deletion/expiry enforcement after restore. Do not enable shared caching that continues serving withdrawn testimonials or audio.

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
