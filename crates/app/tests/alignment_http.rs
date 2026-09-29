//! Actual FFmpeg extraction plus synthetic operator/support authority fixtures.
//! Synthetic tones and attestations verify transport/fencing, not semantic alignment or G3.
use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;
use v0_app::{AppState, auth::hash_secret, config::Config, router, workflow};
use v0_domain::workflow::{CheckStatus, SupportResult, WorkflowView};

struct Fixture {
    pool: PgPool,
    admin: PgPool,
    schema: String,
    app: Router,
    id: Uuid,
    customer: String,
    operator: String,
}
impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("TEST_DATABASE_URL").expect("isolated PostgreSQL required");
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("alignment_http_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let search = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .after_connect(move |connection, _| {
                let search = search.clone();
                Box::pin(async move {
                    sqlx::query(&format!("SET search_path TO {search}"))
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query(&format!("SET application_name TO '{search}'"))
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO interviews(id,customer_label,project_context,secret_hash,idempotency_key,request_hash,expires_at,consented_at,state) VALUES($1,'Synthetic participant','Fixture context',$2,$3,'fixture',clock_timestamp()+interval '14 days',clock_timestamp(),'consented')")
            .bind(id).bind(id.to_string()).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
        let customer = v0_app::auth::random_secret();
        let operator = v0_app::auth::random_secret();
        sqlx::query("INSERT INTO sessions(token_hash,role,interview_id,expires_at) VALUES($1,'customer',$2,clock_timestamp()+interval '1 day'),($3,'operator',NULL,clock_timestamp()+interval '1 day')")
            .bind(hash_secret(&customer)).bind(id).bind(hash_secret(&operator)).execute(&pool).await.unwrap();
        let config = Config {
            origin: "http://localhost:3000".into(),
            agency_name: "Fixture agency".into(),
            operator_username: "fixture".into(),
            operator_password_hash: String::new(),
            invitation_signing_key: "x".repeat(32),
            secure_cookie: false,
            voice_api_key: None,
        };
        let app = router(AppState::new(pool.clone(), config));
        Self {
            pool,
            admin,
            schema,
            app,
            id,
            customer,
            operator,
        }
    }
    async fn close(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
    fn path(&self, suffix: &str) -> String {
        format!("/api/customer/interviews/{}/{suffix}", self.id)
    }
    fn cookie(&self, operator: bool) -> String {
        if operator {
            format!("operator_session={}", self.operator)
        } else {
            format!("customer_session={}", self.customer)
        }
    }
    async fn request(
        &self,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value, HeaderMap) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("Origin", "http://localhost:3000")
            .header("Content-Type", "application/json");
        if let Some(cookie) = cookie {
            request = request.header("Cookie", cookie);
        }
        let response = self
            .app
            .clone()
            .oneshot(
                request
                    .body(if method == "GET" {
                        Body::empty()
                    } else {
                        Body::from(body.to_string())
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, value, headers)
    }
    async fn state(&self) -> WorkflowView {
        workflow::inspect(&self.pool, &self.customer, self.id)
            .await
            .unwrap()
    }
    async fn action(&self, operator: bool, action: Value) -> (StatusCode, Value, HeaderMap) {
        let role = if operator { "operator" } else { "customer" };
        self.request("POST", &format!("/api/{role}/interviews/{}/workflow", self.id), Some(&self.cookie(operator)), json!({"interview_id":self.id,"request_id":Uuid::new_v4(),"expected":self.state().await.revisions,"action":action})).await
    }
    async fn trusted_fixture_support(&self) {
        let state = self.state().await;
        let job: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE interview_id=$1 AND kind='support_check' AND status='queued' ORDER BY created_at DESC LIMIT 1").bind(self.id).fetch_one(&self.pool).await.unwrap();
        let lease = self.lease(job).await;
        let sources: Value =
            sqlx::query_scalar("SELECT source_ids FROM workflow_state WHERE interview_id=$1")
                .bind(self.id)
                .fetch_one(&self.pool)
                .await
                .unwrap();
        workflow::complete_support(
            &self.pool,
            self.id,
            job,
            lease,
            SupportResult {
                content_revision: state.revisions.content,
                evidence_revision: state.revisions.evidence,
                status: CheckStatus::Supported,
                all_substantive_claims_checked: true,
                source_ids: serde_json::from_value(sources).unwrap(),
                model: "trusted-fixture-not-G3".into(),
                prompt_version: "fixture".into(),
            },
        )
        .await
        .unwrap();
    }
    async fn lease(&self, job: Uuid) -> Uuid {
        let lease = Uuid::new_v4();
        sqlx::query("UPDATE jobs SET status='running',lease_token=$2,lease_until=clock_timestamp()+interval '60 seconds' WHERE id=$1").bind(job).bind(lease).execute(&self.pool).await.unwrap();
        lease
    }
}
impl Fixture {
    async fn audio(
        &self,
        path: &str,
        cookie: Option<&str>,
        range: bool,
    ) -> (StatusCode, Vec<u8>, HeaderMap) {
        let mut r = Request::builder().uri(path);
        if let Some(cookie) = cookie {
            r = r.header("Cookie", cookie);
        }
        if range {
            r = r.header("Range", "bytes=0-");
        }
        let response = self
            .app
            .clone()
            .oneshot(r.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let h = response.headers().clone();
        (
            status,
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
            h,
        )
    }
    async fn synthetic_recording(&self, dir: &std::path::Path) -> String {
        use v0_evidence::{
            manifest,
            media::FfmpegValidator,
            storage::{LocalPrivateStorage, PrivateStorage},
        };
        let ffmpeg = std::env::var("FFMPEG_PATH").expect("FFMPEG_PATH required");
        let ffprobe = std::env::var("FFPROBE_PATH").expect("FFPROBE_PATH required");
        let source = dir.join("fixture.ogg");
        let status=tokio::process::Command::new(&ffmpeg).args(["-nostdin","-hide_banner","-loglevel","error","-f","lavfi","-i","aevalsrc=if(lt(t\\,0.5)\\,0.25*sin(2*PI*440*t)\\,0)|0.25*sin(2*PI*880*t):s=24000:d=2","-c:a","libopus"]).arg(&source).status().await.unwrap();
        assert!(status.success());
        let audio = tokio::fs::read(source).await.unwrap();
        let attempt = Uuid::new_v4();
        let session = format!("synthetic_{}", attempt.simple());
        let timeline=serde_json::to_vec(&json!({"session_id":session,"started_at_unix_ms":0,"turns":[{"turn_id":"answer","status":"completed","user_transcript":"Synthetic answer only.","user_speech_started_at_ms":0,"user_speech_ended_at_ms":1000}]})).unwrap();
        let metadata=serde_json::to_vec(&json!({"session_id":session,"started_at":"1970-01-01T00:00:00Z","ended_at":"1970-01-01T00:00:02Z","format":"ogg/opus","channels":2,"channel_layout":"stereo (left=user, right=agent)","sample_rate":24000,"file":"fixture.ogg","dropped_chunks":0,"uploaded_chunks":1})).unwrap();
        let mut manifest = manifest::build(&session, &audio, &timeline, &metadata).unwrap();
        manifest.product_end_reason = Some("explicit_finish".into());
        manifest.media = Some(
            FfmpegValidator::new(ffmpeg, ffprobe, dir.join("decode"))
                .validate(&audio, &manifest)
                .await
                .unwrap(),
        );
        let source_id = manifest.segments[0].source_id.clone();
        let storage = LocalPrivateStorage::new(std::env::var("EVIDENCE_STORAGE_DIR").unwrap())
            .await
            .unwrap();
        let key = format!("source-{attempt}.ogg");
        storage.put(&key, &audio).await.unwrap();
        sqlx::query("INSERT INTO provider_attempts(id,interview_id,provider_session_id,lease_generation,state,product_end_reason) VALUES($1,$2,$3,1,'ended','explicit_finish')").bind(attempt).bind(self.id).bind(session).execute(&self.pool).await.unwrap();
        sqlx::query("INSERT INTO evidence_imports(id,interview_id,provider_attempt_id,manifest,recording_key) VALUES($1,$2,$3,$4,$5)").bind(Uuid::new_v4()).bind(self.id).bind(attempt).bind(serde_json::to_value(manifest).unwrap()).bind(key).execute(&self.pool).await.unwrap();
        source_id
    }
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL and local FFMPEG_PATH/FFPROBE_PATH"]
async fn listened_clip_http_fences_roles_revisions_and_publication_lifecycle() {
    // One test in this integration binary owns these process-local environment paths.
    let dir = std::env::temp_dir().join(format!("v0-alignment-http-{}", Uuid::new_v4()));
    tokio::fs::create_dir(&dir).await.unwrap();
    unsafe {
        std::env::set_var("EVIDENCE_STORAGE_DIR", dir.join("private"));
        std::env::set_var("EVIDENCE_WORK_DIR", dir.join("work"));
    }
    let f = Fixture::new().await;
    let source = f.synthetic_recording(&dir).await;
    let operator = f.cookie(true);
    let customer = f.cookie(false);
    let initial = f.state().await;
    assert!(!initial.evidence_available);
    let base = format!("/api/operator/interviews/{}/alignment", f.id);
    let encoded = source.replace('/', "%2F");
    let query = format!(
        "start_ms=0&end_ms=1000&evidence_revision={}",
        initial.revisions.evidence
    );
    let preview = format!("{base}/{encoded}?{query}");
    let preview_audio = format!("{base}/{encoded}/audio?{query}");
    for cookie in [None, Some(customer.as_str())] {
        assert_eq!(
            f.request("GET", &preview, cookie, json!({})).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            f.audio(&preview_audio, cookie, true).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (status, mut confirmation, _) =
        f.request("GET", &preview, Some(&operator), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{confirmation}");
    assert_eq!(confirmation["listened"], false);
    let (status, clip, headers) = f.audio(&preview_audio, Some(&operator), true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&clip[..4], b"RIFF");
    assert_eq!(clip.len(), 48044);
    assert_eq!(u16::from_le_bytes(clip[22..24].try_into().unwrap()), 1);
    assert!(
        headers["cache-control"]
            .to_str()
            .unwrap()
            .contains("no-store")
    );
    let not_listened = json!({"expected":initial.revisions,"confirmation":confirmation});
    assert_eq!(
        f.request("POST", &base, Some(&operator), not_listened)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    for key in ["listened", "transcript_matches", "complete_answer"] {
        confirmation[key] = json!(true);
    }
    let confirm = json!({"expected":initial.revisions,"confirmation":confirmation});
    assert_eq!(
        f.request("POST", &base, Some(&customer), confirm.clone())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let mut stale = confirm.clone();
    stale["confirmation"]["clip_sha256"] = json!("changed");
    assert_eq!(
        f.request("POST", &base, Some(&operator), stale).await.0,
        StatusCode::CONFLICT
    );
    let (status, result, _) = f
        .request("POST", &base, Some(&operator), confirm.clone())
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["evidence_available"], true);
    assert_eq!(
        f.request("POST", &base, Some(&operator), confirm).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.request("GET", &preview, Some(&operator), json!({}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    // Concurrent operator confirmations share the same expected revisions. At
    // most one may attach a clip; the loser must not publish stale staged bytes.
    let competing = json!({"expected":f.state().await.revisions,"confirmation":confirmation});
    let (first, second) = tokio::join!(
        f.request("POST", &base, Some(&operator), competing.clone()),
        f.request("POST", &base, Some(&operator), competing)
    );
    let outcomes = [first.0, second.0];
    assert_eq!(outcomes.iter().filter(|s| **s == StatusCode::OK).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|s| **s == StatusCode::CONFLICT)
            .count(),
        1
    );
    let evidence = f
        .request("GET", &f.path("evidence"), Some(&customer), json!({}))
        .await
        .1;
    assert_eq!(evidence["clips"].as_array().unwrap().len(), 1);
    assert_eq!(evidence["sources"][0]["alignment_verified"], true);
    let selected = json!({"id":evidence["clips"][0]["id"],"sha256":evidence["clips"][0]["sha256"]});
    let clip_id = selected["id"].as_str().unwrap();
    let private = f.path(&format!("clips/{clip_id}/audio"));
    assert_eq!(
        f.audio(&private, Some(&operator), true).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.audio(&private, None, true).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(f.audio(&private, Some(&customer), true).await.1, clip);
    let public = format!("/api/public/{}/clips/{clip_id}/audio", f.id);
    assert_eq!(f.audio(&public, None, true).await.0, StatusCode::NOT_FOUND);
    // A valid customer cookie for another interview cannot read this clip.
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO interviews(id,customer_label,project_context,secret_hash,idempotency_key,request_hash,expires_at) VALUES($1,'Other synthetic','Fixture',$2,$3,'fixture',clock_timestamp()+interval '1 day')").bind(other).bind(other.to_string()).bind(Uuid::new_v4()).execute(&f.pool).await.unwrap();
    let other_token = v0_app::auth::random_secret();
    sqlx::query("INSERT INTO sessions(token_hash,role,interview_id,expires_at) VALUES($1,'customer',$2,clock_timestamp()+interval '1 day')").bind(hash_secret(&other_token)).bind(other).execute(&f.pool).await.unwrap();
    assert_eq!(
        f.audio(
            &private,
            Some(&format!("customer_session={other_token}")),
            true
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    // Customer can select only the exact verified ID/hash, never an arbitrary recording.
    let text = json!({"text":"Synthetic answer only.","attribution":"Synthetic participant","clips":[selected]});
    let mut wrong = text.clone();
    wrong["clips"][0]["sha256"] = json!("changed");
    assert_ne!(
        f.action(false, json!({"type":"save","content":wrong}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        f.action(false, json!({"type":"save","content":text.clone()}))
            .await
            .0,
        StatusCode::OK
    );
    f.trusted_fixture_support().await;
    assert_eq!(
        f.action(false, json!({"type":"approve"})).await.0,
        StatusCode::OK
    );
    let approval = f.state().await.approval.unwrap().id;
    assert_eq!(
        f.action(true, json!({"type":"publish","approval_id":approval}))
            .await
            .0,
        StatusCode::OK
    );
    let (status, published, h) = f.audio(&public, None, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(published, clip);
    assert_eq!(h["cache-control"], "no-store");
    assert_eq!(
        f.audio(
            &format!("/api/public/{}/clips/{}/audio", f.id, Uuid::new_v4()),
            None,
            true
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.action(true, json!({"type":"unpublish"})).await.0,
        StatusCode::OK
    );
    assert_eq!(f.audio(&public, None, true).await.0, StatusCode::NOT_FOUND);
    // Editing exact approved content withdraws the same direct audio URL.
    assert_eq!(
        f.action(true, json!({"type":"publish","approval_id":approval}))
            .await
            .0,
        StatusCode::OK
    );
    let mut edited = text.clone();
    edited["attribution"] = json!("Edited synthetic attribution");
    assert_eq!(
        f.action(false, json!({"type":"save","content":edited}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(f.audio(&public, None, true).await.0, StatusCode::NOT_FOUND);
    assert!(f.state().await.approval.is_none());
    // A later provider attempt without imported artifacts is missing evidence,
    // even when an earlier recording has verified clips and a published approval.
    f.trusted_fixture_support().await;
    assert_eq!(
        f.action(false, json!({"type":"approve"})).await.0,
        StatusCode::OK
    );
    let latest_approval = f.state().await.approval.unwrap().id;
    assert_eq!(
        f.action(
            true,
            json!({"type":"publish","approval_id":latest_approval})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(f.audio(&public, None, true).await.0, StatusCode::OK);
    let missing = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_attempts(id,interview_id,provider_session_id,lease_generation,state) VALUES($1,$2,$3,2,'ended')").bind(missing).bind(f.id).bind(format!("synthetic-missing-{missing}")).execute(&f.pool).await.unwrap();
    assert_eq!(f.audio(&public, None, true).await.0, StatusCode::NOT_FOUND);
    let unavailable = f.state().await;
    assert!(!unavailable.evidence_available);
    assert!(unavailable.approval.is_none());
    // A private read already waiting for the authority row must observe expiry
    // after the lock is released, not its earlier transaction start time.
    let mut held = f.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(f.id)
        .fetch_one(&mut *held)
        .await
        .unwrap();
    let request = Request::builder()
        .uri(&private)
        .header("Cookie", &customer)
        .header("Range", "bytes=0-")
        .body(Body::empty())
        .unwrap();
    let app = f.app.clone();
    let waiting = tokio::spawn(async move { app.oneshot(request).await.unwrap().status() });
    for _ in 0..100 {
        let blocked: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event_type='Lock' AND application_name=$1)").bind(&f.schema).fetch_one(&f.pool).await.unwrap();
        if blocked {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let blocked: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event_type='Lock' AND application_name=$1)").bind(&f.schema).fetch_one(&f.pool).await.unwrap();
    assert!(
        blocked,
        "private clip read should wait for the held authority row"
    );
    sqlx::query(
        "UPDATE interviews SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",
    )
    .bind(f.id)
    .execute(&mut *held)
    .await
    .unwrap();
    held.commit().await.unwrap();
    assert!(!waiting.await.unwrap().is_success());
    // Expiry and deletion deny even a previously authorized exact private URL.
    sqlx::query(
        "UPDATE interviews SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",
    )
    .bind(f.id)
    .execute(&f.pool)
    .await
    .unwrap();
    assert!(
        !f.audio(&private, Some(&customer), true)
            .await
            .0
            .is_success()
    );
    assert!(
        !f.request("GET", &preview_audio, Some(&operator), json!({}))
            .await
            .0
            .is_success()
    );
    sqlx::query("UPDATE interviews SET expires_at=clock_timestamp()+interval '1 day',deleted_at=clock_timestamp(),state='deleted' WHERE id=$1").bind(f.id).execute(&f.pool).await.unwrap();
    assert!(
        !f.audio(&private, Some(&customer), true)
            .await
            .0
            .is_success()
    );
    assert_eq!(f.audio(&public, None, true).await.0, StatusCode::NOT_FOUND);
    f.close().await;
    tokio::fs::remove_dir_all(dir).await.unwrap();
}
