//! Synthetic PostgreSQL/local HTTP checks for private context and fenced question preparation.
use argon2::{
    Argon2, PasswordHasher,
    password_hash::{SaltString, rand_core::OsRng},
};
use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode},
    routing::post,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Semaphore, mpsc};
use tower::ServiceExt;
use uuid::Uuid;
use v0_app::{AppState, config::Config, interview_context, leases, progress, router, voice_hook};
use v0_composition::GatewayClient;
use v0_domain::workflow::{VoiceAction, VoiceControlRequest};
use v0_evidence::jobs;
const PASSWORD: &str = "synthetic-test-password";
struct TestApp {
    app: Router,
    pool: PgPool,
    admin: PgPool,
    schema: String,
}
impl TestApp {
    async fn new() -> Self {
        let url = std::env::var("TEST_DATABASE_URL")
            .expect("Set TEST_DATABASE_URL to an isolated PostgreSQL database");
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("app_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let search = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .after_connect(move |conn, _| {
                let search = search.clone();
                Box::pin(async move {
                    sqlx::query(&format!("SET search_path TO {search}"))
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
        let config = Config {
            origin: "http://localhost:3000".into(),
            agency_name: "Test agency".into(),
            operator_username: "operator".into(),
            operator_password_hash: Argon2::default()
                .hash_password(PASSWORD.as_bytes(), &SaltString::generate(&mut OsRng))
                .unwrap()
                .to_string(),
            invitation_signing_key: "synthetic-signing-key-32-characters-min".into(),
            secure_cookie: false,
            voice_api_key: None,
        };
        Self {
            app: router(AppState::new(pool.clone(), config)),
            pool,
            admin,
            schema,
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
    async fn request(
        &self,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value, Option<String>) {
        self.request_origin(method, path, cookie, body, "http://localhost:3000")
            .await
    }
    async fn request_origin(
        &self,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        body: Value,
        origin: &str,
    ) -> (StatusCode, Value, Option<String>) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("Origin", origin)
            .header("Content-Type", "application/json");
        if let Some(cookie) = cookie {
            req = req.header("Cookie", cookie);
        }
        let response = self
            .app
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let cookie = response
            .headers()
            .get("set-cookie")
            .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_owned());
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            cookie,
        )
    }
    async fn login(&self) -> String {
        let (s, _, c) = self
            .request(
                "POST",
                "/api/operator/login",
                None,
                json!({"username":"operator","password":PASSWORD}),
            )
            .await;
        assert_eq!(s, StatusCode::OK);
        c.unwrap()
    }
    async fn invite(&self, op: &str) -> Value {
        let(s,v,_)=self.request("POST","/api/operator/invitations",Some(op),json!({"idempotency_key":Uuid::new_v4(),"customer_label":"Synthetic customer","project_context":"A fictional workflow project"})).await;
        assert_eq!(s, StatusCode::OK);
        v
    }
    async fn exchange(&self, invite: &Value) -> String {
        let token = invite["private_url"]
            .as_str()
            .unwrap()
            .split("#token=")
            .nth(1)
            .unwrap();
        let (s, _, c) = self
            .request(
                "POST",
                "/api/customer/exchange",
                None,
                json!({"invitation_id":invite["invitation"]["id"],"token":token}),
            )
            .await;
        assert_eq!(s, StatusCode::OK);
        c.unwrap()
    }
}

fn invite_body(files: Value) -> Value {
    json!({"idempotency_key":Uuid::new_v4(),"customer_label":"Synthetic customer", "project_context":"A fictional museum accessibility project", "context_attachments":files})
}
fn private_files() -> Value {
    json!([{"name":"profile.md","content":"PRIVATE_CONTEXT_SENTINEL: visitor access and inclusive signage"}])
}
fn interview_id(invite: &Value) -> Uuid {
    Uuid::parse_str(invite["invitation"]["id"].as_str().unwrap()).unwrap()
}
fn question_bank() -> Value {
    let keys = [
        "problem",
        "problem_detail",
        "problem_example",
        "change",
        "change_detail",
        "change_example",
        "result",
        "result_detail",
        "result_example",
        "uncertainty",
        "mixed_feedback",
    ];
    let questions: serde_json::Map<String, Value> = keys
        .into_iter()
        .map(|key| {
            (
                key.into(),
                json!(format!(
                    "What did you notice in the museum accessibility project about {}?",
                    key.replace('_', " ")
                )),
            )
        })
        .collect();
    json!({"questions":questions})
}
struct MockGateway {
    client: GatewayClient,
    requests: mpsc::UnboundedReceiver<Value>,
    release: Arc<Semaphore>,
    server: tokio::task::JoinHandle<()>,
}
impl MockGateway {
    async fn new(result: Value) -> Self {
        let (send, requests) = mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let gate = release.clone();
        let app=Router::new().route("/chat/completions",post(move |Json(body):Json<Value>| {
            let send=send.clone(); let gate=gate.clone(); let result=result.clone();
            async move {
                send.send(body).unwrap();
                gate.acquire().await.unwrap().forget();
                Json(json!({"choices":[{"finish_reason":"stop","message":{"content":result.to_string()}}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = GatewayClient::for_local_test_with_model(
            &format!("http://{}/chat/completions", listener.local_addr().unwrap()),
            Duration::from_secs(10),
            "gpt-6-luna",
        )
        .unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            client,
            requests,
            release,
            server,
        }
    }
    async fn request(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
}
impl Drop for MockGateway {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn attachment_validation_is_atomic_and_operator_only() {
    let t = TestApp::new().await;
    let op = t.login().await;
    let invitation = t.invite(&op).await;
    let customer = t.exchange(&invitation).await;
    for cookie in [None, Some(customer.as_str())] {
        assert_eq!(
            t.request(
                "POST",
                "/api/operator/invitations",
                cookie,
                invite_body(private_files())
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM interviews")
        .fetch_one(&t.pool)
        .await
        .unwrap();
    let invalid = vec![
        json!([{"name":"../contract.txt","content":"x"}]),
        json!([{"name":"folder\\contract.txt","content":"x"}]),
        json!([{"name":"contract.pdf","content":"x"}]),
        json!([{"name":".hidden.txt","content":"x"}]),
        json!([{"name":" notes.txt","content":"x"}]),
        json!([{"name":"a.txt","content":" "}]),
        json!([{"name":"a.json","content":"{broken"}]),
        json!([{"name":"a.txt","content":"NUL\u{0000}"}]),
        json!([{"name":"a.txt","content":"control\u{0008}"}]),
        json!([{"name":"a.txt","content":"x"},{"name":"A.TXT","content":"y"}]),
        json!([{"name":format!("{}.txt","a".repeat(157)),"content":"x"}]),
        json!([{"name":"a.txt","content":"x".repeat(32*1024+1)}]),
        Value::Array(
            (0..6)
                .map(|n| json!({"name":format!("{n}.txt"),"content":"x"}))
                .collect(),
        ),
        Value::Array(
            (0..4)
                .map(|n| json!({"name":format!("{n}.txt"),"content":"x".repeat(25*1024)}))
                .collect(),
        ),
    ];
    for files in invalid {
        assert_eq!(
            t.request(
                "POST",
                "/api/operator/invitations",
                Some(&op),
                invite_body(files)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM interviews")
        .fetch_one(&t.pool)
        .await
        .unwrap();
    assert_eq!(before, after);
    let boundary = Value::Array(
        (0..3)
            .map(|n| json!({"name":format!("{n}.TXT"),"content":"x".repeat(32*1024)}))
            .collect(),
    );
    assert_eq!(
        t.request(
            "POST",
            "/api/operator/invitations",
            Some(&op),
            invite_body(boundary)
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(t.request("POST","/api/operator/invitations",Some(&op),invite_body(json!([{"name":"profile.json","content":"{\"name\":\"Élodie\"}"},{"name":"notes.md","content":"Readable\r\ntext\twith Unicode café"}]))).await.0,StatusCode::OK);
    t.close().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn private_context_is_not_a_customer_or_public_payload_and_idempotency_binds_files() {
    let t = TestApp::new().await;
    let op = t.login().await;
    let request = invite_body(private_files());
    let (status, invite, _) = t
        .request(
            "POST",
            "/api/operator/invitations",
            Some(&op),
            request.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let id = interview_id(&invite);
    let (_, replay, _) = t
        .request(
            "POST",
            "/api/operator/invitations",
            Some(&op),
            request.clone(),
        )
        .await;
    assert_eq!(replay["private_url"], invite["private_url"]);
    let mut changed = request;
    changed["context_attachments"][0]["content"] = json!("Different confidential background");
    assert_eq!(
        t.request("POST", "/api/operator/invitations", Some(&op), changed)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE interview_id=$1 AND kind='prepare_interview'",
    )
    .bind(id)
    .fetch_one(&t.pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    let files: Value = sqlx::query_scalar("SELECT context_attachments FROM interviews WHERE id=$1")
        .bind(id)
        .fetch_one(&t.pool)
        .await
        .unwrap();
    assert_eq!(files, private_files());
    let customer = t.exchange(&invite).await;
    for (path, cookie) in [
        ("/api/operator/invitations".into(), Some(op.as_str())),
        ("/api/customer/session".into(), Some(customer.as_str())),
        (
            format!("/api/customer/interviews/{id}/workflow"),
            Some(customer.as_str()),
        ),
        (format!("/api/public/{id}"), None),
    ] {
        let (_, view, _) = t.request("GET", &path, cookie, json!({})).await;
        assert!(!view.to_string().contains("PRIVATE_CONTEXT_SENTINEL"));
        assert!(view.get("context_attachments").is_none());
    }
    assert!(!invite.to_string().contains("PRIVATE_CONTEXT_SENTINEL"));
    assert_eq!(
        t.request(
            "POST",
            &format!("/api/operator/invitations/{id}/prepare"),
            Some(&customer),
            json!({})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    t.close().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn preparation_uses_private_context_but_creates_no_testimonial_evidence_or_approval() {
    let t = TestApp::new().await;
    let op = t.login().await;
    let (_, invite, _) = t
        .request(
            "POST",
            "/api/operator/invitations",
            Some(&op),
            invite_body(private_files()),
        )
        .await;
    let id = interview_id(&invite);
    let customer = t.exchange(&invite).await;
    assert_eq!(
        t.request(
            "POST",
            "/api/customer/consent",
            Some(&customer),
            json!({"interview_id":id,"policy_version":"recording-v1"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(leases::acquire(&t.pool, id, 2).await.is_err());
    let job = jobs::claim(&t.pool).await.unwrap().unwrap();
    let mut mock = MockGateway::new(question_bank()).await;
    let client = mock.client.clone();
    let pool = t.pool.clone();
    let run = tokio::spawn(async move { interview_context::dispatch(&pool, &job, &client).await });
    let sent = mock.request().await;
    assert!(sent.to_string().contains("PRIVATE_CONTEXT_SENTINEL"));
    assert!(
        sent.to_string()
            .contains("fictional museum accessibility project")
    );
    assert_eq!(sent["messages"][0]["role"], "system");
    assert!(
        !sent["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("PRIVATE_CONTEXT_SENTINEL")
    );
    mock.release.add_permits(1);
    assert!(run.await.unwrap().is_ok());
    let row =
        sqlx::query("SELECT interview_preparation,interview_questions FROM interviews WHERE id=$1")
            .bind(id)
            .fetch_one(&t.pool)
            .await
            .unwrap();
    assert_eq!(row.get::<String, _>("interview_preparation"), "ready");
    assert_eq!(row.get::<Value, _>("interview_questions"), question_bank());
    let (status, session, _) = t
        .request("GET", "/api/customer/session", Some(&customer), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(session["interview_preparation"], "ready");
    assert!(session.get("interview_questions").is_none());
    assert!(session.get("context_attachments").is_none());
    assert!(!session.to_string().contains("PRIVATE_CONTEXT_SENTINEL"));
    let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM evidence_imports WHERE interview_id=$1),(SELECT count(*) FROM workflow_state WHERE interview_id=$1),(SELECT count(*) FROM provider_attempts WHERE interview_id=$1)").bind(id).fetch_one(&t.pool).await.unwrap();
    assert_eq!(counts, (0, 0, 0));
    let lease = leases::acquire(&t.pool, id, 2).await.unwrap();
    let attempt = Uuid::new_v4();
    progress::prepare_attempt(&t.pool, id, lease.lease_id, lease.generation, attempt)
        .await
        .unwrap();
    let secret = voice_hook::bind(&t.pool, id, attempt, lease.lease_id, lease.generation)
        .await
        .unwrap();
    let greeting = voice_hook::configured_greeting(&t.pool, attempt, &secret)
        .await
        .unwrap();
    assert_eq!(
        greeting,
        question_bank()["questions"]["problem"].as_str().unwrap()
    );
    progress::map_attempt(
        &t.pool,
        id,
        lease.lease_id,
        lease.generation,
        attempt,
        "synthetic-context-session",
    )
    .await
    .unwrap();
    let (first, _) = voice_hook::questions(&t.pool, attempt, lease.lease_id, lease.generation)
        .await
        .unwrap();
    voice_hook::mark_delivered(&t.pool, attempt, first, lease.lease_id, lease.generation)
        .await
        .unwrap();
    voice_hook::authorize_answer(
        &t.pool,
        attempt,
        first,
        "synthetic-answer",
        "We needed clearer signs.",
        lease.lease_id,
        lease.generation,
    )
    .await
    .unwrap();
    let reply = voice_hook::complete(
        &t.pool,
        attempt,
        &secret,
        voice_hook::CompletionRequest {
            messages: vec![voice_hook::Message {
                role: "user".into(),
                content: "We needed clearer signs.".into(),
            }],
            stream: false,
        },
    )
    .await
    .unwrap();
    let value: Value =
        serde_json::from_slice(&reply.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        value["choices"][0]["message"]["content"],
        question_bank()["questions"]["problem_detail"]
    );
    let token = customer.split_once('=').unwrap().1;
    for (action, key) in [
        (VoiceAction::Repeat, "problem_detail"),
        (VoiceAction::Skip, "change"),
        (VoiceAction::Repeat, "change"),
        (VoiceAction::Skip, "result"),
        (VoiceAction::Skip, "complete"),
    ] {
        let row = sqlx::query("SELECT revision,progress_revision FROM interviews WHERE id=$1")
            .bind(id)
            .fetch_one(&t.pool)
            .await
            .unwrap();
        let permit = voice_hook::control_question(
            &t.pool,
            token,
            VoiceControlRequest {
                interview_id: id,
                request_id: Uuid::new_v4(),
                expected_revision: row.get("revision"),
                expected_progress_revision: row.get("progress_revision"),
                lease_id: lease.lease_id,
                lease_generation: lease.generation,
                action,
            },
        )
        .await
        .unwrap();
        let reply = voice_hook::complete(
            &t.pool,
            attempt,
            &secret,
            voice_hook::CompletionRequest {
                messages: vec![voice_hook::Message {
                    role: "user".into(),
                    content: "We needed clearer signs.".into(),
                }],
                stream: false,
            },
        )
        .await
        .unwrap();
        let value: Value =
            serde_json::from_slice(&reply.into_body().collect().await.unwrap().to_bytes()).unwrap();
        if key == "complete" {
            assert!(
                value["choices"][0]["message"]["content"]
                    .as_str()
                    .unwrap()
                    .contains("Finish interview")
            );
        } else {
            assert_eq!(
                value["choices"][0]["message"]["content"],
                question_bank()["questions"][key]
            );
        }
        let actual: Uuid = sqlx::query_scalar(
            "SELECT last_permit_id FROM voice_hook_bindings WHERE attempt_id=$1",
        )
        .bind(attempt)
        .fetch_one(&t.pool)
        .await
        .unwrap();
        assert_eq!(actual, permit);
    }
    let progress:Value=sqlx::query_scalar("SELECT jsonb_build_object('topic',topic_index,'followups',followup_counts) FROM interviews WHERE id=$1").bind(id).fetch_one(&t.pool).await.unwrap();
    assert_eq!(progress, json!({"topic":3,"followups":[1,0,0]}));
    t.close().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn in_flight_context_work_is_fenced_by_lifecycle_hash_and_worker_lease() {
    let t = TestApp::new().await;
    let op = t.login().await;
    for mutation in [
        "revoked",
        "deleted",
        "expired",
        "hash",
        "lease",
        "lease_expired",
        "attempt",
    ] {
        let (_, invite, _) = t
            .request(
                "POST",
                "/api/operator/invitations",
                Some(&op),
                invite_body(private_files()),
            )
            .await;
        let id = interview_id(&invite);
        let job = jobs::claim(&t.pool).await.unwrap().unwrap();
        let job_id = job.id;
        let mut mock = MockGateway::new(question_bank()).await;
        let client = mock.client.clone();
        let pool = t.pool.clone();
        let run =
            tokio::spawn(async move { interview_context::dispatch(&pool, &job, &client).await });
        mock.request().await;
        match mutation {
            "revoked" => {
                sqlx::query("UPDATE interviews SET state='revoked' WHERE id=$1")
                    .bind(id)
                    .execute(&t.pool)
                    .await
                    .unwrap();
            }
            "deleted" => {
                sqlx::query("UPDATE interviews SET state='deleted',deleted_at=clock_timestamp() WHERE id=$1").bind(id).execute(&t.pool).await.unwrap();
            }
            "expired" => {
                sqlx::query("UPDATE interviews SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(id).execute(&t.pool).await.unwrap();
            }
            "hash" => {
                sqlx::query("UPDATE interviews SET context_hash='replaced' WHERE id=$1")
                    .bind(id)
                    .execute(&t.pool)
                    .await
                    .unwrap();
            }
            "lease" => {
                sqlx::query("UPDATE jobs SET lease_token=$2 WHERE id=$1")
                    .bind(job_id)
                    .bind(Uuid::new_v4())
                    .execute(&t.pool)
                    .await
                    .unwrap();
            }
            "lease_expired" => {
                sqlx::query(
                    "UPDATE jobs SET lease_until=clock_timestamp()-interval '1 second' WHERE id=$1",
                )
                .bind(job_id)
                .execute(&t.pool)
                .await
                .unwrap();
            }
            "attempt" => {
                sqlx::query("INSERT INTO provider_attempts(id,interview_id,lease_generation) VALUES($1,$2,1)").bind(Uuid::new_v4()).bind(id).execute(&t.pool).await.unwrap();
            }
            _ => unreachable!(),
        }
        mock.release.add_permits(1);
        assert!(
            run.await.unwrap().is_err(),
            "{mutation} must fence completion"
        );
        let ready:bool=sqlx::query_scalar("SELECT interview_preparation='ready' OR interview_questions IS NOT NULL FROM interviews WHERE id=$1").bind(id).fetch_one(&t.pool).await.unwrap();
        assert!(!ready);
        let succeeded: bool = sqlx::query_scalar("SELECT status='succeeded' FROM jobs WHERE id=$1")
            .bind(job_id)
            .fetch_one(&t.pool)
            .await
            .unwrap();
        assert!(!succeeded);
        // Do not let an intentionally expired test job be reclaimed by the next case.
        sqlx::query("UPDATE jobs SET status='cancelled' WHERE id=$1")
            .bind(job_id)
            .execute(&t.pool)
            .await
            .unwrap();
    }
    t.close().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn forged_job_scope_and_expired_lease_never_call_gateway() {
    let t = TestApp::new().await;
    let op = t.login().await;
    let (_, invite, _) = t
        .request(
            "POST",
            "/api/operator/invitations",
            Some(&op),
            invite_body(private_files()),
        )
        .await;
    let id = interview_id(&invite);
    let other = t.invite(&op).await;
    let mut job = jobs::claim(&t.pool).await.unwrap().unwrap();
    let original = job.interview_id;
    job.interview_id = if original == id {
        interview_id(&other)
    } else {
        id
    };
    let mut mock = MockGateway::new(question_bank()).await;
    assert!(
        interview_context::dispatch(&t.pool, &job, &mock.client)
            .await
            .is_err()
    );
    job.interview_id = original;
    sqlx::query("UPDATE jobs SET lease_until=clock_timestamp()-interval '1 second' WHERE id=$1")
        .bind(job.id)
        .execute(&t.pool)
        .await
        .unwrap();
    assert!(
        interview_context::dispatch(&t.pool, &job, &mock.client)
            .await
            .is_err()
    );
    assert!(mock.requests.try_recv().is_err());
    t.close().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn failed_preparation_is_retryable_only_before_an_attempt_and_deduplicates() {
    let t = TestApp::new().await;
    let op = t.login().await;
    let invite = t.invite(&op).await;
    let id = interview_id(&invite);
    sqlx::query("UPDATE jobs SET status='failed' WHERE interview_id=$1")
        .bind(id)
        .execute(&t.pool)
        .await
        .unwrap();
    interview_context::reconcile(&t.pool).await.unwrap();
    let path = format!("/api/operator/invitations/{id}/prepare");
    for _ in 0..2 {
        assert_eq!(
            t.request("POST", &path, Some(&op), json!({})).await.0,
            StatusCode::OK
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE interview_id=$1")
        .bind(id)
        .fetch_one(&t.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    sqlx::query("INSERT INTO provider_attempts(id,interview_id,lease_generation) VALUES($1,$2,1)")
        .bind(Uuid::new_v4())
        .bind(id)
        .execute(&t.pool)
        .await
        .unwrap();
    assert_eq!(
        t.request("POST", &path, Some(&op), json!({})).await.0,
        StatusCode::BAD_REQUEST
    );
    t.close().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn legacy_interviews_keep_default_questions_without_preparation_jobs() {
    let t = TestApp::new().await;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO interviews(id,customer_label,project_context,secret_hash,idempotency_key,request_hash,state,consented_at,expires_at) VALUES($1,'Legacy fixture','Synthetic legacy project','fixture',$2,'fixture','consented',clock_timestamp(),clock_timestamp()+interval '1 day')").bind(id).bind(Uuid::new_v4()).execute(&t.pool).await.unwrap();
    let prep: String =
        sqlx::query_scalar("SELECT interview_preparation FROM interviews WHERE id=$1")
            .bind(id)
            .fetch_one(&t.pool)
            .await
            .unwrap();
    assert_eq!(prep, "not_required");
    let lease = leases::acquire(&t.pool, id, 1).await.unwrap();
    let attempt = Uuid::new_v4();
    progress::prepare_attempt(&t.pool, id, lease.lease_id, lease.generation, attempt)
        .await
        .unwrap();
    let secret = voice_hook::bind(&t.pool, id, attempt, lease.lease_id, lease.generation)
        .await
        .unwrap();
    assert_eq!(
        voice_hook::configured_greeting(&t.pool, attempt, &secret)
            .await
            .unwrap(),
        "What problem were you trying to solve?"
    );
    assert!(jobs::claim(&t.pool).await.unwrap().is_none());
    t.close().await;
}
