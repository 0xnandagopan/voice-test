//! PostgreSQL and local-HTTP regressions; never calls a provider.
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;
use uuid::Uuid;
use v0_app::{auth::hash_secret, composition_jobs, workflow};
use v0_composition::GatewayClient;
use v0_domain::workflow::*;
use v0_evidence::{
    jobs::{self, Job},
    media::{MediaReport, RangeCheck},
};

struct Db {
    pool: PgPool,
    admin: PgPool,
    schema: String,
    id: Uuid,
    customer: String,
    source: String,
}
impl Db {
    async fn new() -> Self {
        let url = std::env::var("TEST_DATABASE_URL").expect("isolated TEST_DATABASE_URL required");
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("compose_{}", Uuid::new_v4().simple());
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
        let attempt = Uuid::new_v4();
        let session = attempt.to_string();
        sqlx::query("INSERT INTO interviews(id,customer_label,project_context,secret_hash,idempotency_key,request_hash,expires_at) VALUES($1,'Synthetic','Test composition',$2,$3,'hash',now()+interval '1 day')").bind(id).bind(id.to_string()).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO sessions(token_hash,role,interview_id,expires_at) VALUES($1,'customer',$2,now()+interval '1 day')").bind(hash_secret(&customer)).bind(id).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO provider_attempts(id,interview_id,provider_session_id,lease_generation) VALUES($1,$2,$3,1)").bind(attempt).bind(id).bind(&session).execute(&pool).await.unwrap();
        let timeline=serde_json::to_vec(&json!({"session_id":session,"started_at_unix_ms":0,"turns":[{"turn_id":"t1","status":"completed","user_transcript":"It helped, but setup was hard.","user_speech_started_at_ms":0,"user_speech_ended_at_ms":1000}]})).unwrap();
        let metadata=serde_json::to_vec(&json!({"session_id":session,"started_at":"1970-01-01T00:00:00Z","ended_at":"1970-01-01T00:00:05Z","format":"ogg/opus","channels":2,"channel_layout":"stereo (left=user, right=agent)","sample_rate":24000,"file":"fixture.ogg"})).unwrap();
        let mut manifest =
            v0_evidence::manifest::build(&session, b"OggSsynthetic-only", &timeline, &metadata)
                .unwrap();
        let source = manifest.segments[0].source_id.clone();
        // Explicit synthetic future-alignment fixture; production imports remain ineligible.
        manifest.approval_eligible = true;
        manifest.media = Some(MediaReport {
            codec: "opus".into(),
            channels: 2,
            sample_rate: 24000,
            decoded_duration_ms: 5000,
            metadata_duration_delta_ms: 0,
            alignment_proven: true,
            customer_activity_groups_ms: vec![[0, 1000]],
            ranges: vec![RangeCheck {
                source_id: source.clone(),
                channel: 0,
                within_recording: true,
                audible_samples: 24000,
                total_samples: 24000,
                status: "synthetic".into(),
                candidate_source_range_ms: Some([0, 1000]),
            }],
        });
        sqlx::query("INSERT INTO evidence_imports(id,interview_id,provider_attempt_id,manifest,recording_key) VALUES($1,$2,$3,$4,'private-synthetic')").bind(Uuid::new_v4()).bind(id).bind(attempt).bind(serde_json::to_value(manifest).unwrap()).execute(&pool).await.unwrap();
        Self {
            pool,
            admin,
            schema,
            id,
            customer,
            source,
        }
    }
    async fn state(&self) -> WorkflowView {
        workflow::inspect(&self.pool, &self.customer, self.id)
            .await
            .unwrap()
    }
    async fn save(&self, text: &str) -> WorkflowView {
        workflow::execute(
            &self.pool,
            &self.customer,
            WorkflowCommand {
                interview_id: self.id,
                request_id: Uuid::new_v4(),
                expected: self.state().await.revisions,
                action: WorkflowAction::Save {
                    content: Content {
                        text: text.into(),
                        attribution: "Synthetic customer".into(),
                        clips: vec![],
                    },
                },
            },
        )
        .await
        .unwrap()
        .state
    }
    async fn job(&self, kind: &str, state: &WorkflowView) -> Job {
        sqlx::query("UPDATE jobs SET status='cancelled' WHERE interview_id=$1 AND status='queued'")
            .bind(self.id)
            .execute(&self.pool)
            .await
            .unwrap();
        let id = Uuid::new_v4();
        let token = Uuid::new_v4();
        let payload = json!({"content_revision":state.revisions.content,"evidence_revision":state.revisions.evidence,"content_hash":hash_secret(&serde_json::to_value(&state.content).unwrap().to_string())});
        sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key,status,lease_token,lease_until,attempts,max_attempts) VALUES($1,$2,$3,$4,$5,'running',$6,clock_timestamp()+interval '60 seconds',1,3)").bind(id).bind(self.id).bind(kind).bind(&payload).bind(id.to_string()).bind(token).execute(&self.pool).await.unwrap();
        Job {
            id,
            interview_id: self.id,
            kind: kind.into(),
            payload,
            token,
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
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn exhausted_support_lease_changes_pending_to_failed_exactly_once() {
    let db = Db::new().await;
    let state = db.save("It helped, but setup was hard.").await;
    let job = db.job("support_check", &state).await;
    sqlx::query("UPDATE jobs SET attempts=max_attempts,lease_until=clock_timestamp()-interval '1 second' WHERE id=$1").bind(job.id).execute(&db.pool).await.unwrap();
    assert!(jobs::claim(&db.pool).await.unwrap().is_none());
    workflow::reconcile_failed_support(&db.pool).await.unwrap();
    let failed = db.state().await;
    assert_eq!(failed.check, CheckStatus::Failed);
    assert_eq!(failed.revisions.workflow, state.revisions.workflow + 1);
    assert_eq!(failed.revisions.content, state.revisions.content);
    workflow::reconcile_failed_support(&db.pool).await.unwrap();
    assert_eq!(db.state().await.revisions, failed.revisions);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn stale_failed_support_does_not_fail_new_customer_content() {
    let db = Db::new().await;
    let old = db.save("It helped, but setup was hard.").await;
    let job = db.job("support_check", &old).await;
    let current = db.save("Setup was hard.").await;
    sqlx::query("UPDATE jobs SET status='failed',lease_token=NULL,lease_until=NULL WHERE id=$1")
        .bind(job.id)
        .execute(&db.pool)
        .await
        .unwrap();
    workflow::reconcile_failed_support(&db.pool).await.unwrap();
    workflow::fail_support_job(&db.pool, db.id, job.id)
        .await
        .unwrap();
    let after = db.state().await;
    assert_eq!(after.revisions, current.revisions);
    assert_eq!(after.check, CheckStatus::Pending);
    assert_eq!(after.content.unwrap().text, "Setup was hard.");
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn generation_waiting_on_authority_lock_cannot_overwrite_edit() {
    let db = Db::new().await;
    let old = db.save("It helped, but setup was hard.").await;
    let job = db.job("generate_draft", &old).await;
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(db.id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let pool = db.pool.clone();
    let interview = db.id;
    let jobid = job.id;
    let token = job.token;
    let worker = tokio::spawn(async move {
        workflow::complete_generation(
            &pool,
            interview,
            jobid,
            token,
            Some("Old generated text.".into()),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mut edited = old;
    edited.content.as_mut().unwrap().text = "Customer edit while generation waits.".into();
    edited.revisions.content += 1;
    edited.revisions.workflow += 1;
    sqlx::query("UPDATE workflow_state SET value=$2 WHERE interview_id=$1")
        .bind(db.id)
        .bind(serde_json::to_value(&edited).unwrap())
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(worker.await.unwrap().is_err());
    let after = db.state().await;
    assert_eq!(after.revisions, edited.revisions);
    assert_eq!(
        after.content.unwrap().text,
        "Customer edit while generation waits."
    );
    let running: bool = sqlx::query_scalar("SELECT status='running' FROM jobs WHERE id=$1")
        .bind(job.id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(running);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL and local mock HTTP sockets"]
async fn unvalidated_model_cannot_support_approval_but_unsupported_is_preserved() {
    let db = Db::new().await;
    for supported in [true, false] {
        let candidate = if supported {
            "It helped, but setup was hard."
        } else {
            "It doubled revenue."
        };
        let state = db.save(candidate).await;
        let job = db.job("support_check", &state).await;
        let verdict = if supported {
            "supported"
        } else {
            "unsupported"
        };
        let refs = if supported {
            json!([{"source_id":db.source,"quote":"It helped, but setup was hard."}])
        } else {
            json!([])
        };
        let issues = if supported {
            json!([])
        } else {
            json!(["No recorded revenue evidence."])
        };
        let result = json!({"claims":[{"text":candidate,"verdict":verdict,"sources":refs,"issues":issues}],"issues":[]});
        let envelope =
            json!({"choices":[{"finish_reason":"stop","message":{"content":result.to_string()}}]});
        let app = Router::new().route(
            "/chat/completions",
            post(move || {
                let envelope = envelope.clone();
                async move { Json(envelope) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = GatewayClient::for_local_test(
            &format!("http://{addr}/chat/completions"),
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(
            composition_jobs::dispatch(&db.pool, &job, &client)
                .await
                .is_ok()
        );
        server.abort();
        let after = db.state().await;
        assert_eq!(
            after.check,
            if supported {
                CheckStatus::Ambiguous
            } else {
                CheckStatus::Unsupported
            }
        );
        assert!(after.approval.is_none());
        let persisted:Value = sqlx::query_scalar("SELECT result FROM workflow_support_results WHERE interview_id=$1 AND content_revision=$2 AND evidence_revision=$3")
            .bind(db.id).bind(after.revisions.content).bind(after.revisions.evidence).fetch_one(&db.pool).await.unwrap();
        assert_eq!(persisted["kind"], "support_check");
        assert_eq!(persisted["assessment"]["quality_gate_passed"], false);
        assert_eq!(persisted["assessment"]["claims"][0]["text"], candidate);
        assert!(
            persisted["assessment"]["claims"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
        );
        assert_eq!(after.content.as_ref().unwrap().text, candidate);
        let approval = workflow::execute(
            &db.pool,
            &db.customer,
            WorkflowCommand {
                interview_id: db.id,
                request_id: Uuid::new_v4(),
                expected: after.revisions,
                action: WorkflowAction::Approve,
            },
        )
        .await;
        assert!(approval.is_err());
    }
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn historical_duplicate_generation_cannot_read_inputs_or_overwrite_current_task() {
    let db = Db::new().await;
    let state = db.state().await;
    let old = db.job("generate_draft", &state).await;
    let latest = db.job("generate_draft", &state).await;
    assert!(
        workflow::composition_input(&db.pool, db.id, old.id, old.token)
            .await
            .is_err()
    );
    assert!(
        workflow::complete_generation(
            &db.pool,
            db.id,
            old.id,
            old.token,
            Some("Obsolete worker output.".into())
        )
        .await
        .is_err()
    );
    assert!(
        workflow::composition_input(&db.pool, db.id, latest.id, latest.token)
            .await
            .is_ok()
    );
    assert!(db.state().await.content.is_none());
    db.close().await;
}
