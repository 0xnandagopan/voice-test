//! HTTP authority tests use explicitly trusted synthetic evidence/support fixtures.
//! They do not establish recording alignment, semantic support, or G3 quality.
use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
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
        let schema = format!("review_{}", Uuid::new_v4().simple());
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
    async fn trusted_fixture_evidence(&self) {
        let attempt = Uuid::new_v4();
        let session = attempt.to_string();
        sqlx::query("INSERT INTO provider_attempts(id,interview_id,provider_session_id,lease_generation) VALUES($1,$2,$3,1)").bind(attempt).bind(self.id).bind(&session).execute(&self.pool).await.unwrap();
        let timeline = serde_json::to_vec(&json!({"session_id":session,"started_at_unix_ms":0,"turns":[{"turn_id":"turn1","status":"completed","user_transcript":"It helped, but setup was hard.","user_speech_started_at_ms":0,"user_speech_ended_at_ms":1000}]})).unwrap();
        let metadata = serde_json::to_vec(&json!({"session_id":session,"started_at":"1970-01-01T00:00:00Z","ended_at":"1970-01-01T00:00:05Z","format":"ogg/opus","channels":2,"channel_layout":"stereo (left=user, right=agent)","sample_rate":24000,"file":"fixture.ogg"})).unwrap();
        let mut manifest =
            v0_evidence::manifest::build(&session, b"OggSfixture-only", &timeline, &metadata)
                .unwrap();
        // Authority fixture simulates an already passed alignment gate.
        manifest.approval_eligible = true;
        sqlx::query("INSERT INTO evidence_imports(id,interview_id,provider_attempt_id,manifest,recording_key) VALUES($1,$2,$3,$4,'private-fixture-never-public')").bind(Uuid::new_v4()).bind(self.id).bind(attempt).bind(serde_json::to_value(manifest).unwrap()).execute(&self.pool).await.unwrap();
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
fn content(text: &str) -> Value {
    json!({"text":text,"attribution":"Synthetic participant","clips":[]})
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL via TEST_DATABASE_URL"]
async fn http_customer_operator_public_scopes_and_body_ids_are_separate() {
    let f = Fixture::new().await;
    let customer = f.cookie(false);
    let operator = f.cookie(true);
    for suffix in [
        "workflow",
        "evidence",
        "recovery",
        "sources/arbitrary/audio",
    ] {
        assert_eq!(
            f.request("GET", &f.path(suffix), None, json!({})).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            f.request("GET", &f.path(suffix), Some(&operator), json!({}))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        f.request("GET", &f.path("workflow"), Some(&customer), json!({}))
            .await
            .0,
        StatusCode::OK
    );
    let operator_path = format!("/api/operator/interviews/{}/workflow", f.id);
    assert_eq!(
        f.request("GET", &operator_path, Some(&customer), json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.request("GET", &operator_path, Some(&operator), json!({}))
            .await
            .0,
        StatusCode::OK
    );
    let altered = format!("/api/customer/interviews/{}/workflow", Uuid::new_v4());
    assert_eq!(
        f.request("GET", &altered, Some(&customer), json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let body = json!({"interview_id":Uuid::new_v4(),"request_id":Uuid::new_v4(),"expected":f.state().await.revisions,"action":{"type":"decline"}});
    assert_eq!(
        f.request("POST", &f.path("workflow"), Some(&customer), body)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.request("GET", &format!("/api/public/{}", f.id), None, json!({}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    f.close().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL via TEST_DATABASE_URL"]
async fn trusted_fixture_exact_public_snapshot_and_export_withdraw_after_customer_edit() {
    let f = Fixture::new().await;
    f.trusted_fixture_evidence().await;
    assert_eq!(
        f.action(
            false,
            json!({"type":"save","content":content("It helped, but setup was hard.")})
        )
        .await
        .0,
        StatusCode::OK
    );
    f.trusted_fixture_support().await;
    assert_ne!(
        f.action(true, json!({"type":"approve"})).await.0,
        StatusCode::OK
    );
    assert_eq!(
        f.action(false, json!({"type":"approve"})).await.0,
        StatusCode::OK
    );
    let approval = f.state().await.approval.unwrap().id;
    assert_ne!(
        f.action(false, json!({"type":"publish","approval_id":approval}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        f.action(true, json!({"type":"publish","approval_id":approval}))
            .await
            .0,
        StatusCode::OK
    );
    let public = format!("/api/public/{}", f.id);
    let (status, snapshot, headers) = f.request("GET", &public, None, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snapshot["text"], "It helped, but setup was hard.");
    assert_eq!(snapshot["clips"], json!([]));
    assert_eq!(snapshot.as_object().unwrap().len(), 4);
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(headers["x-robots-tag"], "noindex, nofollow");
    let export = format!("/api/operator/interviews/{}/export", f.id);
    let (status, text, _) = f
        .request("GET", &export, Some(&f.cookie(true)), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        text,
        "It helped, but setup was hard.\n\n— Synthetic participant\n"
    );
    assert_eq!(
        f.action(
            false,
            json!({"type":"save","content":content("A customer edit pending recheck.")})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        f.request("GET", &public, None, json!({})).await.0,
        StatusCode::NOT_FOUND
    );
    assert_ne!(
        f.request("GET", &export, Some(&f.cookie(true)), json!({}))
            .await
            .0,
        StatusCode::OK
    );
    assert!(f.state().await.approval.is_none());
    f.close().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL via TEST_DATABASE_URL"]
async fn generation_receipt_is_idempotent_but_late_completion_cannot_overwrite_edit() {
    let f = Fixture::new().await;
    f.trusted_fixture_evidence().await;
    let request = json!({"request_id":Uuid::new_v4(),"expected":f.state().await.revisions});
    let (status, result, _) = f
        .request(
            "POST",
            &f.path("generate"),
            Some(&f.cookie(false)),
            request.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let repeated = f
        .request("POST", &f.path("generate"), Some(&f.cookie(false)), request)
        .await
        .1;
    assert_eq!(result["job_id"], repeated["job_id"]);
    let job = Uuid::parse_str(result["job_id"].as_str().unwrap()).unwrap();
    let lease = f.lease(job).await;
    assert_eq!(
        f.action(
            false,
            json!({"type":"save","content":content("My newer deliberate edit.")})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(
        workflow::complete_generation(
            &f.pool,
            f.id,
            job,
            lease,
            Some("Stale generated draft".into())
        )
        .await
        .is_err()
    );
    assert_eq!(
        f.state().await.content.unwrap().text,
        "My newer deliberate edit."
    );
    f.close().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL via TEST_DATABASE_URL"]
async fn retry_receipt_does_not_reset_running_job_and_stale_requests_fail() {
    let f = Fixture::new().await;
    f.trusted_fixture_evidence().await;
    let (_, result, _) = f
        .request(
            "POST",
            &f.path("generate"),
            Some(&f.cookie(false)),
            json!({"request_id":Uuid::new_v4(),"expected":f.state().await.revisions}),
        )
        .await;
    let job = Uuid::parse_str(result["job_id"].as_str().unwrap()).unwrap();
    sqlx::query("UPDATE jobs SET status='failed',attempts=3 WHERE id=$1")
        .bind(job)
        .execute(&f.pool)
        .await
        .unwrap();
    let request =
        json!({"request_id":Uuid::new_v4(),"job_id":job,"expected":f.state().await.revisions});
    assert_eq!(
        f.request(
            "POST",
            &f.path("retry"),
            Some(&f.cookie(false)),
            request.clone()
        )
        .await
        .0,
        StatusCode::OK
    );
    let lease = f.lease(job).await;
    assert_eq!(
        f.request(
            "POST",
            &f.path("retry"),
            Some(&f.cookie(false)),
            request.clone()
        )
        .await
        .0,
        StatusCode::OK
    );
    let row = sqlx::query("SELECT status,lease_token,max_attempts FROM jobs WHERE id=$1")
        .bind(job)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("status"), "running");
    assert_eq!(row.get::<Uuid, _>("lease_token"), lease);
    assert_eq!(row.get::<i32, _>("max_attempts"), 6);
    assert_eq!(
        f.action(
            false,
            json!({"type":"save","content":content("New content revision.")})
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut stale = request;
    stale["request_id"] = json!(Uuid::new_v4());
    assert_eq!(
        f.request("POST", &f.path("retry"), Some(&f.cookie(false)), stale)
            .await
            .0,
        StatusCode::CONFLICT
    );
    f.close().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL via TEST_DATABASE_URL"]
async fn recovery_requires_explicit_acknowledgement_after_pending_artifacts_and_preserves_progress()
{
    let f = Fixture::new().await;
    sqlx::query("UPDATE interviews SET state='recovering',topic_index=1,followup_counts='[2,1,0]',time_consumed_seconds=123 WHERE id=$1").bind(f.id).execute(&f.pool).await.unwrap();
    let attempt = Uuid::new_v4();
    let job = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_attempts(id,interview_id,provider_session_id,lease_generation) VALUES($1,$2,$3,1)").bind(attempt).bind(f.id).bind(attempt.to_string()).execute(&f.pool).await.unwrap();
    sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key) VALUES($1,$2,'import_evidence',$3,$4)").bind(job).bind(f.id).bind(json!({"provider_attempt_id":attempt})).bind(format!("import:{attempt}")).execute(&f.pool).await.unwrap();
    let state = f.state().await;
    let (_, recovery, _) = f
        .request(
            "GET",
            &f.path("recovery"),
            Some(&f.cookie(false)),
            json!({}),
        )
        .await;
    let mut request = json!({"request_id":Uuid::new_v4(),"expected_revision":recovery["interview_revision"],"evidence_revision":state.revisions.evidence,"acknowledge_incomplete":true});
    assert_eq!(
        f.request(
            "POST",
            &f.path("recovery/confirm"),
            Some(&f.cookie(false)),
            request.clone()
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    sqlx::query("UPDATE jobs SET status='failed' WHERE id=$1")
        .bind(job)
        .execute(&f.pool)
        .await
        .unwrap();
    request["acknowledge_incomplete"] = json!(false);
    assert_eq!(
        f.request(
            "POST",
            &f.path("recovery/confirm"),
            Some(&f.cookie(false)),
            request.clone()
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    request["acknowledge_incomplete"] = json!(true);
    let (status, result, _) = f
        .request(
            "POST",
            &f.path("recovery/confirm"),
            Some(&f.cookie(false)),
            request.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["confirmed"], true);
    assert_eq!(
        f.request(
            "POST",
            &f.path("recovery/confirm"),
            Some(&f.cookie(false)),
            request
        )
        .await
        .0,
        StatusCode::OK
    );
    let row=sqlx::query("SELECT state,topic_index,followup_counts,time_consumed_seconds FROM interviews WHERE id=$1").bind(f.id).fetch_one(&f.pool).await.unwrap();
    assert_eq!(row.get::<String, _>("state"), "consented");
    assert_eq!(row.get::<i32, _>("topic_index"), 1);
    assert_eq!(row.get::<Value, _>("followup_counts"), json!([2, 1, 0]));
    assert_eq!(row.get::<i32, _>("time_consumed_seconds"), 123);
    assert!(!f.state().await.evidence_available);
    f.close().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL via TEST_DATABASE_URL"]
async fn support_failure_advances_revision_and_retry_cannot_use_pre_failure_state() {
    let f = Fixture::new().await;
    f.trusted_fixture_evidence().await;
    assert_eq!(
        f.action(
            false,
            json!({"type":"save","content":content("It helped, but setup was hard.")})
        )
        .await
        .0,
        StatusCode::OK
    );
    let before = f.state().await;
    let row=sqlx::query("SELECT id,payload FROM jobs WHERE interview_id=$1 AND kind='support_check' ORDER BY created_at DESC LIMIT 1").bind(f.id).fetch_one(&f.pool).await.unwrap();
    let id: Uuid = row.get("id");
    let lease = f.lease(id).await;
    let job = v0_evidence::jobs::Job {
        id,
        interview_id: f.id,
        kind: "support_check".into(),
        payload: row.get("payload"),
        token: lease,
    };
    v0_app::composition_jobs::fail(
        &f.pool,
        &job,
        v0_app::composition_jobs::JobFailure {
            code: "generation_validation_failed",
            retry_after_secs: 0,
            terminal: true,
        },
    )
    .await
    .unwrap();
    let failed = f.state().await;
    assert_eq!(failed.check, CheckStatus::Failed);
    assert_eq!(failed.revisions.workflow, before.revisions.workflow + 1);
    assert_eq!(failed.revisions.content, before.revisions.content);
    let stale = json!({"request_id":Uuid::new_v4(),"job_id":id,"expected":before.revisions});
    assert_eq!(
        f.request("POST", &f.path("retry"), Some(&f.cookie(false)), stale)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let fresh = json!({"request_id":Uuid::new_v4(),"job_id":id,"expected":failed.revisions});
    assert_eq!(
        f.request("POST", &f.path("retry"), Some(&f.cookie(false)), fresh)
            .await
            .0,
        StatusCode::OK
    );
    let status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id=$1")
        .bind(id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(status, "queued");
    assert!(f.state().await.approval.is_none());
    f.close().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL via TEST_DATABASE_URL"]
async fn generation_requests_share_current_task_and_keep_receipts_after_an_edit() {
    let f = Fixture::new().await;
    f.trusted_fixture_evidence().await;
    let expected = f.state().await.revisions;
    let first = json!({"request_id":Uuid::new_v4(),"expected":expected});
    let second = json!({"request_id":Uuid::new_v4(),"expected":expected});
    let path = f.path("generate");
    let cookie = f.cookie(false);
    let (a, b) = tokio::join!(
        f.request("POST", &path, Some(&cookie), first.clone()),
        f.request("POST", &path, Some(&cookie), second.clone())
    );
    assert_eq!(a.0, StatusCode::OK);
    assert_eq!(b.0, StatusCode::OK);
    assert_eq!(a.1["job_id"], b.1["job_id"]);
    let job = Uuid::parse_str(a.1["job_id"].as_str().unwrap()).unwrap();
    sqlx::query(
        "UPDATE jobs SET status='failed',last_error='generation_validation_failed' WHERE id=$1",
    )
    .bind(job)
    .execute(&f.pool)
    .await
    .unwrap();
    let (_, again, _) = f
        .request(
            "POST",
            &f.path("generate"),
            Some(&f.cookie(false)),
            json!({"request_id":Uuid::new_v4(),"expected":expected}),
        )
        .await;
    assert_eq!(again["job_id"], a.1["job_id"]);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind='generate_draft'")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let (_, evidence, _) = f
        .request(
            "GET",
            &f.path("evidence"),
            Some(&f.cookie(false)),
            Value::Null,
        )
        .await;
    assert_eq!(evidence["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(evidence["jobs"][0]["can_retry"], true);

    assert_eq!(
        f.action(
            false,
            json!({"type":"save","content":content("My own edited draft.")})
        )
        .await
        .0,
        StatusCode::OK
    );
    for request in [first.clone(), second] {
        let replay = f
            .request("POST", &f.path("generate"), Some(&f.cookie(false)), request)
            .await;
        assert_eq!(replay.0, StatusCode::OK);
        assert_eq!(replay.1["job_id"], a.1["job_id"]);
    }
    let current = f.state().await.revisions;
    let mut changed = first;
    changed["expected"] = json!(current);
    assert_eq!(
        f.request("POST", &f.path("generate"), Some(&f.cookie(false)), changed)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.request(
            "POST",
            &f.path("retry"),
            Some(&f.cookie(false)),
            json!({"request_id":Uuid::new_v4(),"job_id":job,"expected":current})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (_, evidence, _) = f
        .request(
            "GET",
            &f.path("evidence"),
            Some(&f.cookie(false)),
            Value::Null,
        )
        .await;
    assert_eq!(evidence["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(evidence["jobs"][0]["kind"], "support_check");
    assert_eq!(evidence["jobs"][0]["can_retry"], false);
    let unchanged: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id=$1")
        .bind(job)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(unchanged, "failed");
    f.close().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL via TEST_DATABASE_URL"]
async fn evidence_shows_latest_current_task_and_each_missing_attempt_not_historical_failures() {
    let f = Fixture::new().await;
    f.trusted_fixture_evidence().await;
    let state = f.state().await;
    let payload = json!({"content_revision":state.revisions.content,"evidence_revision":state.revisions.evidence,"content_hash":hash_secret(&serde_json::to_value(&state.content).unwrap().to_string())});
    let mut generation = vec![];
    for _ in 0..3 {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key,status,last_error) VALUES($1,$2,'generate_draft',$3,$4,'failed','generation_validation_failed')")
            .bind(id).bind(f.id).bind(&payload).bind(id.to_string()).execute(&f.pool).await.unwrap();
        generation.push(id);
    }
    let imported: Uuid = sqlx::query_scalar(
        "SELECT provider_attempt_id FROM evidence_imports WHERE interview_id=$1",
    )
    .bind(f.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    let mut missing_jobs = vec![];
    for attempt in [imported, Uuid::new_v4(), Uuid::new_v4()] {
        if attempt != imported {
            sqlx::query(
                "INSERT INTO provider_attempts(id,interview_id,lease_generation) VALUES($1,$2,2)",
            )
            .bind(attempt)
            .bind(f.id)
            .execute(&f.pool)
            .await
            .unwrap();
        }
        let job = Uuid::new_v4();
        sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key,status,last_error) VALUES($1,$2,'import_evidence',$3,$4,'failed','artifact_unavailable')")
            .bind(job).bind(f.id).bind(json!({"provider_attempt_id":attempt})).bind(format!("import:{attempt}")).execute(&f.pool).await.unwrap();
        if attempt != imported {
            missing_jobs.push(job);
        } else {
            assert_eq!(
                f.request(
                    "POST",
                    &f.path("retry"),
                    Some(&f.cookie(false)),
                    json!({"request_id":Uuid::new_v4(),"job_id":job,"expected":state.revisions})
                )
                .await
                .0,
                StatusCode::CONFLICT
            );
        }
    }
    let (_, evidence, _) = f
        .request(
            "GET",
            &f.path("evidence"),
            Some(&f.cookie(false)),
            Value::Null,
        )
        .await;
    let jobs = evidence["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 3);
    assert!(jobs.iter().all(|j| j["can_retry"] == true));
    assert!(jobs.iter().any(|j| j["id"] == json!(generation[2])));
    for job in missing_jobs {
        assert!(jobs.iter().any(|j| j["id"] == json!(job)));
    }
    assert_eq!(
        f.request(
            "POST",
            &f.path("retry"),
            Some(&f.cookie(false)),
            json!({"request_id":Uuid::new_v4(),"job_id":generation[0],"expected":state.revisions})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.request(
            "POST",
            &f.path("retry"),
            Some(&f.cookie(false)),
            json!({"request_id":Uuid::new_v4(),"job_id":generation[2],"expected":state.revisions})
        )
        .await
        .0,
        StatusCode::OK
    );
    f.close().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL via TEST_DATABASE_URL"]
async fn generation_receipts_fail_closed_for_malformed_or_other_command_request_ids() {
    let f = Fixture::new().await;
    f.trusted_fixture_evidence().await;
    let expected = f.state().await.revisions;
    for receipt in ["ordinary-command-hash", "generation:v1:invalid:not-a-uuid"] {
        let request_id = Uuid::new_v4();
        sqlx::query("INSERT INTO workflow_receipts(interview_id,actor_hash,request_id,request_hash) VALUES($1,$2,$3,$4)")
            .bind(f.id).bind(hash_secret(&f.customer)).bind(request_id).bind(receipt).execute(&f.pool).await.unwrap();
        assert_eq!(
            f.request(
                "POST",
                &f.path("generate"),
                Some(&f.cookie(false)),
                json!({"request_id":request_id,"expected":expected})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    f.close().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL via TEST_DATABASE_URL"]
async fn recovered_sources_keep_attempt_order_and_disclose_interrupted_endings() {
    let f = Fixture::new().await;
    f.trusted_fixture_evidence().await;
    f.trusted_fixture_evidence().await;
    let attempts: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM provider_attempts WHERE interview_id=$1 ORDER BY id DESC",
    )
    .bind(f.id)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    // Earlier attempt arrives later: neither import arrival nor random UUIDs
    // determine the order in which the customer told their story.
    for (index, attempt) in attempts.iter().enumerate() {
        sqlx::query("UPDATE provider_attempts SET started_at=clock_timestamp()+$2*interval '1 minute' WHERE id=$1")
            .bind(attempt).bind(index as i32).execute(&f.pool).await.unwrap();
        sqlx::query("UPDATE evidence_imports SET created_at=clock_timestamp()-$2*interval '1 minute',manifest=jsonb_set(manifest,'{product_end_reason}',$3) WHERE provider_attempt_id=$1")
            .bind(attempt).bind(index as i32)
            .bind(json!(if index == 0 { "transport_lost" } else { "explicit_finish" }))
            .execute(&f.pool).await.unwrap();
    }
    let (status, evidence, _) = f
        .request(
            "GET",
            &f.path("evidence"),
            Some(&f.cookie(false)),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(evidence["sources"].as_array().unwrap().len(), 2);
    assert_eq!(evidence["sources"][0]["attempt_id"], json!(attempts[0]));
    assert_eq!(evidence["sources"][1]["attempt_id"], json!(attempts[1]));
    assert_eq!(evidence["sources"][0]["recording_interrupted"], true);
    assert_eq!(evidence["sources"][1]["recording_interrupted"], false);
    f.close().await;
}
