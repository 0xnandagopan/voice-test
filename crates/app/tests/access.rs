use argon2::{
    Argon2, PasswordHasher,
    password_hash::{SaltString, rand_core::OsRng},
};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;
use v0_app::{AppState, config::Config, leases, router};
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
#[tokio::test]
#[ignore = "requires isolated PostgreSQL via TEST_DATABASE_URL"]
async fn invitation_consent_survive_reload_and_never_fake_live_success() {
    let t = TestApp::new().await;
    let op = t.login().await;
    let invite = t.invite(&op).await;
    let customer = t.exchange(&invite).await;
    let (s, v, _) = t
        .request("GET", "/api/customer/session", Some(&customer), json!({}))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(v["consented_at"].is_null());
    assert_eq!(v["voice_available"], false);
    let (s, _, _) = t
        .request(
            "POST",
            "/api/customer/start",
            Some(&customer),
            json!({"expected_revision":1}),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, v, _) = t
        .request(
            "POST",
            "/api/customer/consent",
            Some(&customer),
            json!({"policy_version":"old-policy"}),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["code"], "invalid_request");
    let (s, v, _) = t
        .request(
            "POST",
            "/api/customer/consent",
            Some(&customer),
            json!({"policy_version":"recording-v1"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["revision"], 2);
    assert_eq!(v["state"], "consented");
    let (_, again, _) = t
        .request(
            "POST",
            "/api/customer/consent",
            Some(&customer),
            json!({"policy_version":"recording-v1"}),
        )
        .await;
    assert_eq!(again["revision"], 2);
    let (_, reopened, _) = t
        .request("GET", "/api/customer/session", Some(&customer), json!({}))
        .await;
    assert_eq!(v, reopened);
    let (s, v, _) = t
        .request(
            "POST",
            "/api/customer/start",
            Some(&customer),
            json!({"expected_revision":2}),
        )
        .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v["error"]["code"], "provider_unavailable");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_attempts")
        .fetch_one(&t.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    t.close().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL via TEST_DATABASE_URL"]
async fn scope_origin_and_revocation_are_enforced() {
    let t = TestApp::new().await;
    assert_eq!(
        t.request("GET", "/api/operator/invitations", None, json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        t.request_origin(
            "POST",
            "/api/operator/login",
            None,
            json!({"username":"operator","password":PASSWORD}),
            "https://evil.example"
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let op = t.login().await;
    let invite = t.invite(&op).await;
    let customer = t.exchange(&invite).await;
    assert_eq!(
        t.request(
            "GET",
            "/api/operator/invitations",
            Some(&customer),
            json!({})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let other = t.invite(&op).await;
    let token = invite["private_url"]
        .as_str()
        .unwrap()
        .split("#token=")
        .nth(1)
        .unwrap();
    assert_eq!(
        t.request(
            "POST",
            "/api/customer/exchange",
            None,
            json!({"invitation_id":other["invitation"]["id"],"token":token})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let path = format!(
        "/api/operator/invitations/{}/revoke",
        invite["invitation"]["id"].as_str().unwrap()
    );
    assert_eq!(
        t.request("POST", &path, Some(&op), json!({"expected_revision":99}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        t.request("POST", &path, Some(&op), json!({"expected_revision":1}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        t.request("GET", "/api/customer/session", Some(&customer), json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        t.request(
            "POST",
            "/api/customer/exchange",
            None,
            json!({"invitation_id":invite["invitation"]["id"],"token":token})
        )
        .await
        .0,
        StatusCode::GONE
    );
    assert_eq!(
        t.request("POST", "/api/operator/logout", Some(&op), json!({}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        t.request("GET", "/api/operator/me", Some(&op), json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    t.close().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL via TEST_DATABASE_URL"]
async fn repeated_and_concurrent_invites_are_one_record_and_payload_bound() {
    let t = TestApp::new().await;
    let op = t.login().await;
    let body = json!({"idempotency_key":Uuid::new_v4(),"customer_label":"Synthetic","project_context":"Fictional project"});
    let (a, b) = tokio::join!(
        t.request("POST", "/api/operator/invitations", Some(&op), body.clone()),
        t.request("POST", "/api/operator/invitations", Some(&op), body.clone())
    );
    assert_eq!(a.0, StatusCode::OK);
    assert_eq!(b.0, StatusCode::OK);
    assert_eq!(a.1, b.1);
    let mut changed = body;
    changed["project_context"] = json!("Different meaning");
    assert_eq!(
        t.request("POST", "/api/operator/invitations", Some(&op), changed)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM interviews")
        .fetch_one(&t.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let raw: String = sqlx::query_scalar("SELECT secret_hash FROM interviews")
        .fetch_one(&t.pool)
        .await
        .unwrap();
    assert_ne!(
        raw,
        a.1["private_url"]
            .as_str()
            .unwrap()
            .split("#token=")
            .nth(1)
            .unwrap()
    );
    t.close().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL via TEST_DATABASE_URL"]
async fn expiry_does_not_depend_on_worker_and_reads_do_not_extend_it() {
    let t = TestApp::new().await;
    let op = t.login().await;
    let invite = t.invite(&op).await;
    let customer = t.exchange(&invite).await;
    let id = Uuid::parse_str(invite["invitation"]["id"].as_str().unwrap()).unwrap();
    let before: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT expires_at FROM interviews WHERE id=$1")
            .bind(id)
            .fetch_one(&t.pool)
            .await
            .unwrap();
    t.request("GET", "/api/customer/session", Some(&customer), json!({}))
        .await;
    let after: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT expires_at FROM interviews WHERE id=$1")
            .bind(id)
            .fetch_one(&t.pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    sqlx::query("UPDATE interviews SET expires_at=now()-interval '1 second' WHERE id=$1")
        .bind(id)
        .execute(&t.pool)
        .await
        .unwrap();
    assert_eq!(
        t.request("GET", "/api/customer/session", Some(&customer), json!({}))
            .await
            .0,
        StatusCode::GONE
    );
    t.close().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL via TEST_DATABASE_URL"]
async fn interview_lease_fences_tabs_and_preserves_reconnect_allowance() {
    let t = TestApp::new().await;
    let op = t.login().await;
    let invite = t.invite(&op).await;
    let customer = t.exchange(&invite).await;
    let id = Uuid::parse_str(invite["invitation"]["id"].as_str().unwrap()).unwrap();
    assert!(leases::acquire(&t.pool, id, 1).await.is_err());
    t.request(
        "POST",
        "/api/customer/consent",
        Some(&customer),
        json!({"policy_version":"recording-v1"}),
    )
    .await;
    let (a, b) = tokio::join!(
        leases::acquire(&t.pool, id, 2),
        leases::acquire(&t.pool, id, 2)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let first = a.or(b).unwrap();
    assert_eq!(first.remaining_seconds, 360);
    sqlx::query("UPDATE interviews SET lease_expires_at=now()-interval '1 second',active_since=now()-interval '11 seconds',time_consumed_seconds=100 WHERE id=$1").bind(id).execute(&t.pool).await.unwrap();
    let second = leases::acquire(&t.pool, id, 3).await.unwrap();
    assert!(second.remaining_seconds <= 250);
    assert_eq!(second.generation, first.generation + 1);
    assert_ne!(second.lease_id, first.lease_id);
    sqlx::query("UPDATE interviews SET lease_expires_at=now()-interval '1 second',time_consumed_seconds=360 WHERE id=$1").bind(id).execute(&t.pool).await.unwrap();
    assert!(leases::acquire(&t.pool, id, 4).await.is_err());
    t.close().await;
}
