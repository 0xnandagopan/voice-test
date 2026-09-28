//! Delete only the provider agent recorded for this attempt. This is permitted
//! after interview deletion; unlike imports, cleanup must not depend on availability.
use reqwest::{
    Client, StatusCode,
    header::{AUTHORIZATION, HeaderValue},
};
use serde_json::Value;
use sqlx::{PgPool, Row};
use std::time::Duration;
use uuid::Uuid;
use v0_evidence::{Error, Result, jobs::Job};

const ENDPOINT: &str = "https://agents.assemblyai.com/v1/agents";
const MAX_LIST_BYTES: usize = 512 * 1024;

pub async fn dispatch(pool: &PgPool, job: &Job, api_key: &str) -> Result<()> {
    dispatch_at(pool, job, api_key, ENDPOINT).await
}

async fn dispatch_at(pool: &PgPool, job: &Job, api_key: &str, endpoint: &str) -> Result<()> {
    if job.kind != "delete_provider_agent" {
        return Err(Error::Invalid("unexpected agent cleanup job"));
    }
    let attempt: Uuid = serde_json::from_value(
        job.payload
            .get("attempt_id")
            .cloned()
            .ok_or(Error::Invalid("agent cleanup payload"))?,
    )?;
    // This payload is written only by the trusted relay after a late create;
    // never expose job insertion or this provider identity to browser commands.
    let direct_id = job
        .payload
        .get("provider_agent_id")
        .map(|value| {
            value
                .as_str()
                .filter(|id| valid_id(id))
                .map(str::to_owned)
                .ok_or(Error::Invalid("late agent identity"))
        })
        .transpose()?;
    let mut key =
        HeaderValue::from_str(api_key).map_err(|_| Error::Invalid("provider key configuration"))?;
    if api_key.trim().is_empty() {
        return Err(Error::Invalid("provider key configuration"));
    }
    key.set_sensitive(true);
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Error::Invalid("agent cleanup client"))?;
    // The same interview lock guards relay ownership changes. Keep it through
    // bounded deletion so an expired attempt cannot resume between check/delete.
    let mut tx = pool.begin().await?;
    let interview = sqlx::query(
        "SELECT lease_generation,lease_id,lease_expires_at FROM interviews WHERE id=$1 FOR UPDATE",
    )
    .bind(job.interview_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::Stale)?;
    let lease=sqlx::query("SELECT lease_until FROM jobs WHERE id=$1 AND interview_id=$2 AND kind='delete_provider_agent' AND status='running' AND lease_token=$3 FOR UPDATE")
        .bind(job.id).bind(job.interview_id).bind(job.token).fetch_optional(&mut *tx).await?.ok_or(Error::Stale)?;
    let lease_until: chrono::DateTime<chrono::Utc> = lease.get("lease_until");
    let row=sqlx::query("SELECT provider_agent_id,provider_agent_name,state,ended_at,lease_generation FROM provider_attempts WHERE id=$1 AND interview_id=$2 FOR UPDATE")
        .bind(attempt).bind(job.interview_id).fetch_optional(&mut *tx).await?.ok_or(Error::Stale)?;
    let current: bool = sqlx::query_scalar("SELECT clock_timestamp()<$1")
        .bind(lease_until)
        .fetch_one(&mut *tx)
        .await?;
    if !current {
        return Err(Error::Stale);
    }
    let active_until: Option<chrono::DateTime<chrono::Utc>> = interview.get("lease_expires_at");
    let active: bool = sqlx::query_scalar("SELECT COALESCE(clock_timestamp()<$1,false)")
        .bind(active_until)
        .fetch_one(&mut *tx)
        .await?;
    let id: Option<String> = row.get("provider_agent_id");
    let name: Option<String> = row.get("provider_agent_name");
    let different_late_id = direct_id
        .as_ref()
        .zip(id.as_ref())
        .is_some_and(|(late, current)| late != current);
    if active
        && !different_late_id
        && interview.get::<Option<Uuid>, _>("lease_id").is_some()
        && interview.get::<i64, _>("lease_generation") == row.get::<i64, _>("lease_generation")
        && row
            .get::<Option<chrono::DateTime<chrono::Utc>>, _>("ended_at")
            .is_none()
        && matches!(
            row.get::<String, _>("state").as_str(),
            "connecting" | "active" | "recovering"
        )
    {
        return Err(Error::NotReady);
    }
    if let Some(late_id) = &direct_id {
        delete(&client, endpoint, &key, late_id).await?;
    } else {
        match (&id, &name) {
            (Some(id), _) => delete(&client, endpoint, &key, id).await?,
            (None, Some(name)) => {
                if name.trim().is_empty() || name.len() > 256 {
                    return Err(Error::Invalid("agent recovery name"));
                }
                let response = client
                    .get(endpoint)
                    .header(AUTHORIZATION, key.clone())
                    .send()
                    .await
                    .map_err(|_| Error::Invalid("agent list request"))?;
                if response.status() != StatusCode::OK {
                    return Err(Error::Invalid("agent list unavailable"));
                }
                let value = bounded_json(response).await?;
                let id = exact_match(&value, name)?.ok_or(Error::NotReady)?;
                delete(&client, endpoint, &key, &id).await?;
            }
            (None, None) => {} // Already cleared, or creation never began.
        }
    }
    // Database identities stayed locked during I/O. Keep explicit compare guards
    // and fence by actual wall-clock time after all potential lock waits.
    if direct_id.is_none() || direct_id == id {
        let changed=sqlx::query("UPDATE provider_attempts SET provider_agent_id=NULL,provider_agent_name=NULL WHERE id=$1 AND interview_id=$2 AND provider_agent_id IS NOT DISTINCT FROM $3 AND provider_agent_name IS NOT DISTINCT FROM $4")
        .bind(attempt).bind(job.interview_id).bind(id).bind(name).execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(Error::Stale);
        }
    }
    let complete=sqlx::query("UPDATE jobs SET status='succeeded',lease_token=NULL,lease_until=NULL,last_error=NULL WHERE id=$1 AND interview_id=$2 AND status='running' AND lease_token=$3 AND lease_until>clock_timestamp()")
        .bind(job.id).bind(job.interview_id).bind(job.token).execute(&mut *tx).await?;
    if complete.rows_affected() != 1 {
        return Err(Error::Stale);
    }
    // SQL completion could wait on a trigger/lock: do not commit a stale clear.
    let current: bool = sqlx::query_scalar("SELECT clock_timestamp()<$1")
        .bind(lease_until)
        .fetch_one(&mut *tx)
        .await?;
    if !current {
        return Err(Error::Stale);
    }
    tx.commit().await?;
    Ok(())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn exact_match(value: &Value, name: &str) -> Result<Option<String>> {
    let agents = value
        .as_array()
        .filter(|v| v.len() <= 2000)
        .ok_or(Error::Invalid("agent list shape"))?;
    let mut found = None;
    for agent in agents {
        // An incomplete record could hide the target; do not infer absence.
        let candidate = agent
            .get("name")
            .and_then(Value::as_str)
            .ok_or(Error::Invalid("agent list record"))?;
        if candidate == name {
            let id = agent
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| valid_id(id))
                .ok_or(Error::Invalid("agent identity"))?;
            if found.is_some() {
                return Err(Error::Invalid("ambiguous agent recovery name"));
            }
            found = Some(id.to_owned());
        }
    }
    Ok(found)
}

async fn bounded_json(mut response: reqwest::Response) -> Result<Value> {
    if response
        .content_length()
        .is_some_and(|n| n > MAX_LIST_BYTES as u64)
    {
        return Err(Error::Invalid("agent list bound"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Error::Invalid("agent list transport"))?
    {
        if bytes.len() + chunk.len() > MAX_LIST_BYTES {
            return Err(Error::Invalid("agent list bound"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::Invalid("agent list JSON"))
}

async fn delete(client: &Client, endpoint: &str, key: &HeaderValue, id: &str) -> Result<()> {
    if !valid_id(id) {
        return Err(Error::Invalid("agent identity"));
    }
    let response = client
        .delete(format!("{endpoint}/{id}"))
        .header(AUTHORIZATION, key.clone())
        .send()
        .await
        .map_err(|_| Error::Invalid("agent delete request"))?;
    if matches!(
        response.status(),
        StatusCode::NO_CONTENT | StatusCode::NOT_FOUND
    ) {
        Ok(())
    } else {
        Err(Error::Invalid("agent deletion unconfirmed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Db {
        pool: PgPool,
        admin: PgPool,
        schema: String,
    }
    impl Db {
        async fn new() -> Self {
            let url =
                std::env::var("TEST_DATABASE_URL").expect("isolated TEST_DATABASE_URL required");
            let admin = sqlx::postgres::PgPoolOptions::new()
                .max_connections(2)
                .connect(&url)
                .await
                .unwrap();
            let schema = format!("cleanup_{}", Uuid::new_v4().simple());
            sqlx::query(&format!("CREATE SCHEMA {schema}"))
                .execute(&admin)
                .await
                .unwrap();
            let search = schema.clone();
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(3)
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
            Self {
                pool,
                admin,
                schema,
            }
        }
        async fn job(&self) -> (Job, Uuid) {
            let interview = Uuid::new_v4();
            let attempt = Uuid::new_v4();
            let token = Uuid::new_v4();
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO interviews(id,customer_label,project_context,secret_hash,idempotency_key,request_hash,expires_at) VALUES($1,'Synthetic','Cleanup test',$2,$3,'hash',now()+interval '1 day')").bind(interview).bind(interview.to_string()).bind(Uuid::new_v4()).execute(&self.pool).await.unwrap();
            sqlx::query("INSERT INTO provider_attempts(id,interview_id,state,lease_generation,ended_at,provider_agent_id,provider_agent_name) VALUES($1,$2,'ended',1,now(),'agent_test','v0-controlled-test')").bind(attempt).bind(interview).execute(&self.pool).await.unwrap();
            let payload = json!({"attempt_id":attempt});
            sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key,status,lease_token,lease_until) VALUES($1,$2,'delete_provider_agent',$3,$4,'running',$5,now()+interval '60 seconds')").bind(id).bind(interview).bind(&payload).bind(id.to_string()).bind(token).execute(&self.pool).await.unwrap();
            (
                Job {
                    id,
                    interview_id: interview,
                    kind: "delete_provider_agent".into(),
                    payload,
                    token,
                },
                attempt,
            )
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

    async fn response_server(
        status: &'static str,
        delay: Duration,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let n = socket.read(&mut request).await.unwrap();
            assert!(n > 0);
            tokio::time::sleep(delay).await;
            socket
                .write_all(
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
        });
        (format!("http://{address}/v1/agents"), server)
    }

    #[tokio::test]
    #[ignore = "requires isolated local TEST_DATABASE_URL"]
    async fn cleanup_fences_tombstones_active_attempts_and_deadline_races() {
        let db = Db::new().await;
        let (job, attempt) = db.job().await;
        sqlx::query("UPDATE interviews SET deleted_at=now(),state='deleted' WHERE id=$1")
            .bind(job.interview_id)
            .execute(&db.pool)
            .await
            .unwrap();
        let (endpoint, server) = response_server("204 No Content", Duration::ZERO).await;
        dispatch_at(&db.pool, &job, "test-key", &endpoint)
            .await
            .unwrap();
        server.await.unwrap();
        let cleared:bool=sqlx::query_scalar("SELECT provider_agent_id IS NULL AND provider_agent_name IS NULL FROM provider_attempts WHERE id=$1").bind(attempt).fetch_one(&db.pool).await.unwrap();
        assert!(cleared);
        let succeeded: bool = sqlx::query_scalar(
            "SELECT status='succeeded' AND lease_token IS NULL FROM jobs WHERE id=$1",
        )
        .bind(job.id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(succeeded);

        let (job, attempt) = db.job().await;
        sqlx::query("UPDATE interviews SET lease_generation=1,lease_id=$2,lease_expires_at=now()+interval '1 minute' WHERE id=$1").bind(job.interview_id).bind(Uuid::new_v4()).execute(&db.pool).await.unwrap();
        sqlx::query("UPDATE provider_attempts SET state='active',ended_at=NULL WHERE id=$1")
            .bind(attempt)
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(matches!(
            dispatch_at(&db.pool, &job, "test-key", "http://127.0.0.1:9").await,
            Err(Error::NotReady)
        ));

        // A late create can be cleaned without touching the active current ID.
        let mut late_job = job;
        late_job.payload["provider_agent_id"] = json!("agent_test");
        assert!(matches!(
            dispatch_at(&db.pool, &late_job, "test-key", "http://127.0.0.1:9").await,
            Err(Error::NotReady)
        ));
        late_job.payload["provider_agent_id"] = json!("agent_late");
        sqlx::query("UPDATE jobs SET payload=$2 WHERE id=$1")
            .bind(late_job.id)
            .bind(&late_job.payload)
            .execute(&db.pool)
            .await
            .unwrap();
        let (endpoint, server) = response_server("204 No Content", Duration::ZERO).await;
        dispatch_at(&db.pool, &late_job, "test-key", &endpoint)
            .await
            .unwrap();
        server.await.unwrap();
        let retained:bool=sqlx::query_scalar("SELECT provider_agent_id='agent_test' AND provider_agent_name='v0-controlled-test' FROM provider_attempts WHERE id=$1").bind(attempt).fetch_one(&db.pool).await.unwrap();
        assert!(retained);

        let (job, _) = db.job().await;
        sqlx::query("UPDATE jobs SET lease_token=$2 WHERE id=$1")
            .bind(job.id)
            .bind(Uuid::new_v4())
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(matches!(
            dispatch_at(&db.pool, &job, "test-key", "http://127.0.0.1:9").await,
            Err(Error::Stale)
        ));

        for (status, delay, expire) in [
            ("500 Error", Duration::ZERO, false),
            ("204 No Content", Duration::from_millis(200), true),
        ] {
            let (job, attempt) = db.job().await;
            if expire {
                sqlx::query("UPDATE jobs SET lease_until=clock_timestamp()+interval '100 milliseconds' WHERE id=$1").bind(job.id).execute(&db.pool).await.unwrap();
            }
            let (endpoint, server) = response_server(status, delay).await;
            let outcome = dispatch_at(&db.pool, &job, "test-key", &endpoint).await;
            assert!(outcome.is_err());
            if expire {
                assert!(matches!(outcome, Err(Error::Stale)));
            }
            server.await.unwrap();
            let retained:bool=sqlx::query_scalar("SELECT provider_agent_id='agent_test' AND provider_agent_name='v0-controlled-test' FROM provider_attempts WHERE id=$1").bind(attempt).fetch_one(&db.pool).await.unwrap();
            assert!(retained);
            let running: bool = sqlx::query_scalar(
                "SELECT status='running' AND lease_token=$2 FROM jobs WHERE id=$1",
            )
            .bind(job.id)
            .bind(job.token)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert!(running);
        }
        db.close().await;
    }
    #[test]
    fn exact_unique_name_accepts_opaque_id_only() {
        let list = json!([{"id":"unrelated","name":"v0-attempt-extra"},{"id":"agent_ABC-123","name":"v0-attempt"}]);
        assert_eq!(
            exact_match(&list, "v0-attempt").unwrap(),
            Some("agent_ABC-123".into())
        );
        assert!(exact_match(&list, "absent").unwrap().is_none());
        assert!(exact_match(&json!([{"id":"a","name":"n"},{"id":"b","name":"n"}]), "n").is_err());
        assert!(exact_match(&json!({"agents":[]}), "n").is_err());
        assert!(exact_match(&json!([{"id":"../victim","name":"n"}]), "n").is_err());
    }
    #[tokio::test]
    async fn deletion_accepts_only_confirmed_status_and_does_not_follow_redirects() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (status, success) in [
            ("204 No Content", true),
            ("404 Not Found", true),
            ("202 Accepted", false),
            ("302 Found", false),
            ("500 Error", false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let read = socket.read(&mut request).await.unwrap();
                assert!(
                    std::str::from_utf8(&request[..read])
                        .unwrap()
                        .starts_with("DELETE /v1/agents/agent_123 ")
                );
                socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nLocation: http://127.0.0.1:9/private\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            });
            let client = Client::builder()
                .timeout(Duration::from_secs(2))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            let result = delete(
                &client,
                &format!("http://{addr}/v1/agents"),
                &HeaderValue::from_static("test-key"),
                "agent_123",
            )
            .await;
            assert_eq!(result.is_ok(), success);
            server.await.unwrap();
        }
    }
}
