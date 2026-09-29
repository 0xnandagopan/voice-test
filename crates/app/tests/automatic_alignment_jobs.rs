//! Real FFmpeg + PostgreSQL job fences; synthetic tone/ASR fixtures are not G2 evidence.
use axum::{
    Json, Router,
    routing::{get, post},
};
use serde_json::{Value, json};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use uuid::Uuid;
use v0_app::{alignment_jobs, auth::hash_secret, workflow};
use v0_domain::workflow::{CheckStatus, Content, WorkflowAction, WorkflowCommand};
use v0_evidence::{
    jobs::Job,
    manifest,
    media::FfmpegValidator,
    storage::{LocalPrivateStorage, PrivateStorage},
    stt::AssemblyAiTranscriber,
};

#[derive(Default)]
struct Calls {
    uploads: AtomicUsize,
    submits: AtomicUsize,
    polls: AtomicUsize,
    deletes: AtomicUsize,
    completed: AtomicBool,
    mismatch: AtomicBool,
}
struct Mock {
    calls: Arc<Calls>,
    client: AssemblyAiTranscriber,
    task: tokio::task::JoinHandle<()>,
}
impl Mock {
    async fn new() -> Self {
        let calls = Arc::new(Calls::default());
        let u = calls.clone();
        let s = calls.clone();
        let p = calls.clone();
        let d = calls.clone();
        let app=Router::new()
            .route("/v2/upload",post(move |body: axum::body::Bytes| async move {
                assert_eq!(&body[..4],b"RIFF"); assert_eq!(u16::from_le_bytes(body[22..24].try_into().unwrap()),1);
                u.uploads.fetch_add(1,Ordering::SeqCst); Json(json!({"upload_url":"https://cdn.assemblyai.com/upload/synthetic"}))
            }))
            .route("/v2/transcript",post(move |Json(body):Json<Value>| async move {
                assert_eq!(body["speech_models"],json!(["universal-2"]));
                s.submits.fetch_add(1,Ordering::SeqCst); Json(json!({"id":"synthetic-transcript","status":"queued"}))
            }))
            .route("/v2/transcript/synthetic-transcript",get(move || async move {
                p.polls.fetch_add(1,Ordering::SeqCst);
                if !p.completed.load(Ordering::SeqCst) { return Json(json!({"id":"synthetic-transcript","status":"processing"})); }
                let word=if p.mismatch.load(Ordering::SeqCst) {"Different"} else {"Synthetic"};
                Json(json!({"id":"synthetic-transcript","status":"completed","text":format!("{word} answer only."),"audio_duration":2.0,"words":[{"text":word,"start":0,"end":150,"confidence":0.99},{"text":"answer","start":150,"end":300,"confidence":0.99},{"text":"only.","start":300,"end":500,"confidence":0.99}]}))
            }).delete(move || async move { d.deletes.fetch_add(1,Ordering::SeqCst); Json(json!({"id":"synthetic-transcript","status":"deleted"})) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = AssemblyAiTranscriber::with_endpoint(
            "synthetic-key".into(),
            format!("http://{addr}/").parse().unwrap(),
        )
        .unwrap();
        Self {
            calls,
            client,
            task,
        }
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Fixture {
    pool: PgPool,
    admin: PgPool,
    schema: String,
    id: Uuid,
    customer: String,
    dir: PathBuf,
    storage: LocalPrivateStorage,
    validator: FfmpegValidator,
}
impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("TEST_DATABASE_URL").expect("isolated PostgreSQL required");
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("automatic_jobs_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let search = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .after_connect(move |c, _| {
                let search = search.clone();
                Box::pin(async move {
                    sqlx::query(&format!("SET search_path TO {search}"))
                        .execute(c)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
        let id = Uuid::new_v4();
        let customer = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO interviews(id,customer_label,project_context,secret_hash,idempotency_key,request_hash,expires_at,consented_at,state) VALUES($1,'Synthetic participant','Fixture context',$2,$3,'fixture',clock_timestamp()+interval '14 days',clock_timestamp(),'consented')").bind(id).bind(id.to_string()).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO sessions(token_hash,role,interview_id,expires_at) VALUES($1,'customer',$2,clock_timestamp()+interval '1 day')").bind(hash_secret(&customer)).bind(id).execute(&pool).await.unwrap();
        let dir = std::env::temp_dir().join(format!("automatic-jobs-{}", Uuid::new_v4()));
        tokio::fs::create_dir(&dir).await.unwrap();
        let ffmpeg = std::env::var("FFMPEG_PATH").expect("FFMPEG_PATH required");
        let validator = FfmpegValidator::new(
            ffmpeg.clone(),
            std::env::var("FFPROBE_PATH").unwrap(),
            dir.join("work"),
        );
        let source = dir.join("fixture.ogg");
        let status=tokio::process::Command::new(ffmpeg).args(["-nostdin","-hide_banner","-loglevel","error","-f","lavfi","-i","aevalsrc=if(lt(t\\,0.5)\\,0.25*sin(2*PI*440*t)\\,0)|0.25*sin(2*PI*880*t):s=24000:d=2","-c:a","libopus"]).arg(&source).status().await.unwrap();
        assert!(status.success());
        let audio = tokio::fs::read(source).await.unwrap();
        let attempt = Uuid::new_v4();
        let session = format!("synthetic_{}", attempt.simple());
        let timeline=serde_json::to_vec(&json!({"session_id":session,"started_at_unix_ms":0,"turns":[{"turn_id":"answer","status":"completed","user_transcript":"Synthetic answer only.","user_speech_started_at_ms":0,"user_speech_ended_at_ms":1000}]})).unwrap();
        let metadata=serde_json::to_vec(&json!({"session_id":session,"started_at":"1970-01-01T00:00:00Z","ended_at":"1970-01-01T00:00:02Z","format":"ogg/opus","channels":2,"channel_layout":"stereo (left=user, right=agent)","sample_rate":24000,"file":"fixture.ogg","dropped_chunks":0,"uploaded_chunks":1})).unwrap();
        let mut manifest = manifest::build(&session, &audio, &timeline, &metadata).unwrap();
        manifest.product_end_reason = Some("explicit_finish".into());
        manifest.media = Some(validator.validate(&audio, &manifest).await.unwrap());
        let storage = LocalPrivateStorage::new(dir.join("private")).await.unwrap();
        let key = format!("source-{attempt}.ogg");
        storage.put(&key, &audio).await.unwrap();
        sqlx::query("INSERT INTO provider_attempts(id,interview_id,provider_session_id,lease_generation,state,product_end_reason) VALUES($1,$2,$3,1,'ended','explicit_finish')").bind(attempt).bind(id).bind(session).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO evidence_imports(id,interview_id,provider_attempt_id,manifest,recording_key) VALUES($1,$2,$3,$4,$5)").bind(Uuid::new_v4()).bind(id).bind(attempt).bind(serde_json::to_value(manifest).unwrap()).bind(key).execute(&pool).await.unwrap();
        alignment_jobs::enqueue_missing(&pool).await.unwrap();
        Self {
            pool,
            admin,
            schema,
            id,
            customer,
            dir,
            storage,
            validator,
        }
    }
    async fn lease(&self, kind: &str) -> Job {
        let token = Uuid::new_v4();
        let row=sqlx::query("UPDATE jobs SET status='running',attempts=attempts+1,lease_token=$2,lease_until=clock_timestamp()+interval '120 seconds' WHERE interview_id=$1 AND kind=$3 RETURNING id,kind,payload").bind(self.id).bind(token).bind(kind).fetch_one(&self.pool).await.unwrap();
        Job {
            id: row.get("id"),
            interview_id: self.id,
            kind: row.get("kind"),
            payload: row.get("payload"),
            token,
        }
    }
    async fn dispatch(&self, job: &Job, mock: &Mock) -> Result<(), &'static str> {
        alignment_jobs::dispatch(
            &self.pool,
            job,
            &mock.client,
            &self.storage,
            &self.validator,
        )
        .await
        .map_err(|failure| failure.code)
    }
    async fn save(&self, text: &str) {
        let state = workflow::inspect(&self.pool, &self.customer, self.id)
            .await
            .unwrap();
        workflow::execute(
            &self.pool,
            &self.customer,
            WorkflowCommand {
                interview_id: self.id,
                request_id: Uuid::new_v4(),
                expected: state.revisions,
                action: WorkflowAction::Save {
                    content: Content {
                        text: text.into(),
                        attribution: "Synthetic participant".into(),
                        clips: vec![],
                    },
                },
            },
        )
        .await
        .unwrap();
    }
    async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
    }
    async fn close(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
        tokio::fs::remove_dir_all(self.dir).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL and FFMPEG_PATH/FFPROBE_PATH"]
async fn pending_poll_resumes_once_and_checks_current_customer_edit_then_cleans_provider_copy() {
    let f = Fixture::new().await;
    let mock = Mock::new().await;
    alignment_jobs::enqueue_missing(&f.pool).await.unwrap();
    assert_eq!(
        f.count("SELECT count(*) FROM jobs WHERE kind='align_evidence'")
            .await,
        1
    );
    f.save("Initial customer text.").await;
    let job = f.lease("align_evidence").await;
    f.dispatch(&job, &mock).await.unwrap();
    let payload: Value =
        sqlx::query_scalar("SELECT payload FROM jobs WHERE id=$1 AND status='queued'")
            .bind(job.id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(payload["transcript_id"], "synthetic-transcript");
    assert!(payload.get("upload_url").is_none());
    f.save("Current edited customer text.").await;
    let before = workflow::inspect(&f.pool, &f.customer, f.id).await.unwrap();
    mock.calls.completed.store(true, Ordering::SeqCst);
    f.dispatch(&f.lease("align_evidence").await, &mock)
        .await
        .unwrap();
    assert_eq!(mock.calls.uploads.load(Ordering::SeqCst), 1);
    assert_eq!(mock.calls.submits.load(Ordering::SeqCst), 1);
    assert_eq!(mock.calls.polls.load(Ordering::SeqCst), 2);
    let state = workflow::inspect(&f.pool, &f.customer, f.id).await.unwrap();
    assert!(state.evidence_available);
    assert_eq!(
        state.content.as_ref().unwrap().text,
        "Current edited customer text."
    );
    assert_eq!(state.revisions.content, before.revisions.content);
    assert!(state.revisions.evidence > before.revisions.evidence);
    assert_eq!(state.check, CheckStatus::Pending);
    assert!(state.approval.is_none());
    assert!(state.content.unwrap().clips.is_empty());
    assert_eq!(
        f.count("SELECT count(*) FROM workflow_clips WHERE ready")
            .await,
        1
    );
    let pending:i64=sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind='support_check' AND status='queued' AND (payload->>'content_revision')::bigint=$1 AND (payload->>'evidence_revision')::bigint=$2").bind(state.revisions.content).bind(state.revisions.evidence).fetch_one(&f.pool).await.unwrap();
    assert_eq!(pending, 1);
    let manifest: Value = sqlx::query_scalar("SELECT manifest FROM evidence_imports")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(manifest["automatic_alignment"].as_array().unwrap().len(), 1);
    assert!(
        manifest["operator_alignment"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    f.dispatch(&f.lease("delete_alignment_transcript").await, &mock)
        .await
        .unwrap();
    assert_eq!(mock.calls.deletes.load(Ordering::SeqCst), 1);
    f.close().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL and FFMPEG_PATH/FFPROBE_PATH"]
async fn mismatched_transcript_never_attaches_clips_and_retains_cleanup() {
    let f = Fixture::new().await;
    let mock = Mock::new().await;
    mock.calls.completed.store(true, Ordering::SeqCst);
    mock.calls.mismatch.store(true, Ordering::SeqCst);
    let failure = f
        .dispatch(&f.lease("align_evidence").await, &mock)
        .await
        .unwrap_err();
    assert_eq!(failure, "alignment_transcript_mismatch");
    assert_eq!(f.count("SELECT count(*) FROM workflow_clips").await, 0);
    assert_eq!(
        f.count("SELECT count(*) FROM jobs WHERE kind='delete_alignment_transcript'")
            .await,
        1
    );
    assert!(
        !workflow::inspect(&f.pool, &f.customer, f.id)
            .await
            .unwrap()
            .evidence_available
    );
    sqlx::query("UPDATE jobs SET status='failed',lease_token=NULL,lease_until=NULL WHERE kind='align_evidence'").execute(&f.pool).await.unwrap();
    f.dispatch(&f.lease("delete_alignment_transcript").await, &mock)
        .await
        .unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL and FFMPEG_PATH/FFPROBE_PATH"]
async fn deleted_interview_and_lost_lease_cannot_attach_automatic_evidence() {
    for deleted in [false, true] {
        let f = Fixture::new().await;
        let mock = Mock::new().await;
        f.dispatch(&f.lease("align_evidence").await, &mock)
            .await
            .unwrap();
        let job = f.lease("align_evidence").await;
        if deleted {
            sqlx::query(
                "UPDATE interviews SET deleted_at=clock_timestamp(),state='deleted' WHERE id=$1",
            )
            .bind(f.id)
            .execute(&f.pool)
            .await
            .unwrap();
        } else {
            sqlx::query("UPDATE jobs SET lease_token=$2 WHERE id=$1")
                .bind(job.id)
                .bind(Uuid::new_v4())
                .execute(&f.pool)
                .await
                .unwrap();
        }
        mock.calls.completed.store(true, Ordering::SeqCst);
        assert!(f.dispatch(&job, &mock).await.is_err());
        assert_eq!(f.count("SELECT count(*) FROM workflow_clips").await, 0);
        let eligible: bool = sqlx::query_scalar(
            "SELECT (manifest->>'approval_eligible')::bool FROM evidence_imports",
        )
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert!(!eligible);
        // A lost worker lease must not erase a transcript while its replacement is active.
        if !deleted {
            f.dispatch(&f.lease("delete_alignment_transcript").await, &mock)
                .await
                .unwrap();
            assert_eq!(mock.calls.deletes.load(Ordering::SeqCst), 0);
            sqlx::query("UPDATE jobs SET created_at=clock_timestamp()-interval '2 hours' WHERE kind='align_evidence'").execute(&f.pool).await.unwrap();
        }
        // Provider cleanup is allowed even after a tombstone or exhausted processing window.
        f.dispatch(&f.lease("delete_alignment_transcript").await, &mock)
            .await
            .unwrap();
        assert_eq!(mock.calls.deletes.load(Ordering::SeqCst), 1);
        f.close().await;
    }
}
