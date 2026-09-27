use async_trait::async_trait;
use serde_json::json;
use sqlx::{Connection, Executor, PgConnection, PgPool};
use std::str::FromStr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;
use v0_evidence::{
    Error, Result, jobs, manifest,
    provider::{Artifacts, AssemblyHistory, HistoryProvider},
    storage::{LocalPrivateStorage, PrivateStorage},
};

fn artifacts() -> Artifacts {
    Artifacts {
        close_reason: Some("client_end".into()),
        audio: b"OggSsynthetic-header-not-real-audio".to_vec(),
        timeline: serde_json::to_vec(&json!({"session_id":"sess_test","started_at_unix_ms":0,"turns":[{"turn_id":"turn1","item_id":"item1","status":"completed","user_transcript":"It helped, but setup was hard.","agent_text":"What was hard?","agent_reply_started_at_ms":5000,"agent_reply_ended_at_ms":6000},{"turn_id":"turn2","status":"interrupted","user_transcript":"I started to"}]})).unwrap(),
        metadata: serde_json::to_vec(&json!({"session_id":"sess_test","started_at":"1970-01-01T00:00:00Z","ended_at":"1970-01-01T00:00:10Z","format":"ogg/opus","channels":2,"channel_layout":"stereo (left=user, right=agent)","sample_rate":24000,"file":"sess_test/recording/audio.ogg"})).unwrap(),
    }
}
struct Fixture;
#[async_trait]
impl HistoryProvider for Fixture {
    async fn fetch(&self, _: &str) -> Result<Artifacts> {
        Ok(artifacts())
    }
}

#[test]
fn provenance_preserves_partial_turns_and_never_invents_customer_ranges() {
    let a = artifacts();
    let m = manifest::build("sess_test", &a.audio, &a.timeline, &a.metadata).unwrap();
    assert_eq!(m.segments[0].source_range_ms, None);
    assert_eq!(m.segments[0].alignment, "missing_alignment");
    assert_eq!(m.segments[0].channel, 0);
    assert_eq!(m.segments[1].source_range_ms, Some([5000, 6000]));
    assert_eq!(m.incomplete_turn_ids, ["turn2"]);
    assert!(!m.approval_eligible);
    assert_eq!(m.recording_validation, "header_and_metadata_only");
}
#[test]
fn rejects_mismatched_session_and_out_of_recording_ranges() {
    let mut a = artifacts();
    assert!(manifest::build("another_session", &a.audio, &a.timeline, &a.metadata).is_err());
    let mut t: serde_json::Value = serde_json::from_slice(&a.timeline).unwrap();
    t["turns"][0]["agent_reply_ended_at_ms"] = json!(11000);
    a.timeline = serde_json::to_vec(&t).unwrap();
    assert!(manifest::build("sess_test", &a.audio, &a.timeline, &a.metadata).is_err());
}
#[tokio::test]
async fn local_objects_are_private_immutable_and_paths_bounded() {
    let dir = std::env::temp_dir().join(format!("v0-evidence-{}", Uuid::new_v4()));
    let s = LocalPrivateStorage::new(&dir).await.unwrap();
    assert!(s.put("../bad", b"no").await.is_err());
    s.put("a.ogg", b"original").await.unwrap();
    assert!(s.put("a.ogg", b"replacement").await.is_err());
    assert_eq!(s.read("a.ogg").await.unwrap(), b"original");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.join("a.ogg"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    s.delete("a.ogg").await.unwrap();
    s.delete("a.ogg").await.unwrap();
    tokio::fs::remove_dir_all(dir).await.unwrap();
}

#[tokio::test]
async fn mock_http_downloads_never_forward_provider_credentials_to_artifacts() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let a = artifacts();
        for index in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 4096];
            let n = socket.read(&mut bytes).await.unwrap();
            let request = String::from_utf8_lossy(&bytes[..n]).to_lowercase();
            if index == 0 {
                assert!(request.contains("authorization: fixture-secret"));
            } else {
                assert!(!request.contains("authorization:"));
            }
            let body=match index {
                0=>serde_json::to_vec(&json!({"id":"sess_test","status":"completed","artifacts":[{"type":"audio","url":format!("http://{address}/audio")},{"type":"timeline","url":format!("http://{address}/timeline")},{"type":"metadata","url":format!("http://{address}/metadata")}]})).unwrap(),
                1=>a.audio.clone(),2=>a.timeline.clone(),_=>a.metadata.clone()
            };
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(&body).await.unwrap();
        }
    });
    let p = AssemblyHistory::with_endpoint(
        "fixture-secret".into(),
        format!("http://{address}").parse().unwrap(),
        vec!["127.0.0.1".into()],
        true,
    )
    .unwrap();
    let a = p.fetch("sess_test").await.unwrap();
    assert!(a.audio.starts_with(b"OggS"));
    server.await.unwrap();
}

async fn database() -> (PgPool, String, String) {
    let url = std::env::var("TEST_DATABASE_URL")
        .expect("real PostgreSQL required: set TEST_DATABASE_URL");
    let schema = format!("evidence_{}", Uuid::new_v4().simple());
    let mut admin = PgConnection::connect(&url).await.unwrap();
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .unwrap();
    let options = sqlx::postgres::PgConnectOptions::from_str(&url)
        .unwrap()
        .options([("search_path", schema.as_str())]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .unwrap();
    pool.execute(include_str!("../../../migrations/0001_foundation.sql"))
        .await
        .unwrap();
    pool.execute(include_str!(
        "../../../migrations/0002_workflow_authority.sql"
    ))
    .await
    .unwrap();
    (pool, schema, url)
}
async fn seed(pool: &PgPool) -> (Uuid, Uuid) {
    let interview = Uuid::new_v4();
    let attempt = Uuid::new_v4();
    sqlx::query("INSERT INTO interviews(id,customer_label,project_context,secret_hash,idempotency_key,request_hash,expires_at) VALUES($1,'Fixture','Synthetic interview',$2,$3,'fixture',now()+interval '1 day')").bind(interview).bind(Uuid::new_v4().to_string()).bind(Uuid::new_v4()).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO provider_attempts(id,interview_id,provider_session_id,lease_generation) VALUES($1,$2,'sess_test',1)").bind(attempt).bind(interview).execute(pool).await.unwrap();
    (interview, attempt)
}
async fn drop_database(pool: PgPool, schema: String, url: String) {
    pool.close().await;
    let mut admin = PgConnection::connect(&url).await.unwrap();
    admin
        .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn postgres_dedupe_lease_reclaim_stale_worker_and_idempotent_import() {
    let (pool, schema, url) = database().await;
    let (i, a) = seed(&pool).await;
    let id = jobs::enqueue_import(&pool, i, a).await.unwrap();
    assert_eq!(id, jobs::enqueue_import(&pool, i, a).await.unwrap());
    let first = jobs::claim(&pool).await.unwrap().unwrap();
    assert!(jobs::claim(&pool).await.unwrap().is_none());
    sqlx::query("UPDATE jobs SET lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let second = jobs::claim(&pool).await.unwrap().unwrap();
    assert_ne!(first.token, second.token);
    let dir = std::env::temp_dir().join(format!("v0-evidence-{}", Uuid::new_v4()));
    let store = LocalPrivateStorage::new(&dir).await.unwrap();
    assert!(matches!(
        jobs::dispatch(&pool, &first, &Fixture, &store).await,
        Err(Error::Stale)
    ));
    jobs::dispatch(&pool, &second, &Fixture, &store)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET status='queued',available_at=now() WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let replay = jobs::claim(&pool).await.unwrap().unwrap();
    jobs::dispatch(&pool, &replay, &Fixture, &store)
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM evidence_imports")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let manifest: serde_json::Value = sqlx::query_scalar("SELECT manifest FROM evidence_imports")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(manifest["approval_eligible"], false);
    sqlx::query("UPDATE jobs SET available_at=now() WHERE kind='cleanup_objects'")
        .execute(&pool)
        .await
        .unwrap();
    while let Some(job) = jobs::claim(&pool).await.unwrap() {
        jobs::dispatch(&pool, &job, &Fixture, &store).await.unwrap();
    }
    let mut entries = tokio::fs::read_dir(&dir).await.unwrap();
    let mut files = 0;
    while entries.next_entry().await.unwrap().is_some() {
        files += 1;
    }
    assert_eq!(files, 3);
    tokio::fs::remove_dir_all(dir).await.unwrap();
    drop_database(pool, schema, url).await;
}

struct TombstoneDuringFetch {
    pool: PgPool,
    interview: Uuid,
}
#[async_trait]
impl HistoryProvider for TombstoneDuringFetch {
    async fn fetch(&self, _: &str) -> Result<Artifacts> {
        sqlx::query("UPDATE interviews SET deleted_at=now(),revision=revision+1 WHERE id=$1")
            .bind(self.interview)
            .execute(&self.pool)
            .await?;
        Ok(artifacts())
    }
}
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn postgres_delete_during_import_never_attaches_and_orphans_are_cleaned() {
    let (pool, schema, url) = database().await;
    let (i, a) = seed(&pool).await;
    jobs::enqueue_import(&pool, i, a).await.unwrap();
    let job = jobs::claim(&pool).await.unwrap().unwrap();
    let dir = std::env::temp_dir().join(format!("v0-evidence-{}", Uuid::new_v4()));
    let store = LocalPrivateStorage::new(&dir).await.unwrap();
    assert!(matches!(
        jobs::dispatch(
            &pool,
            &job,
            &TombstoneDuringFetch {
                pool: pool.clone(),
                interview: i
            },
            &store
        )
        .await,
        Err(Error::Stale)
    ));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM evidence_imports")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("UPDATE jobs SET available_at=now() WHERE kind='cleanup_objects'")
        .execute(&pool)
        .await
        .unwrap();
    let cleanup = jobs::claim(&pool).await.unwrap().unwrap();
    jobs::dispatch(&pool, &cleanup, &Fixture, &store)
        .await
        .unwrap();
    assert!(
        tokio::fs::read_dir(&dir)
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none()
    );
    tokio::fs::remove_dir_all(dir).await.unwrap();
    drop_database(pool, schema, url).await;
}
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn postgres_expiry_and_final_attempt_crash_are_visible() {
    let (pool, schema, url) = database().await;
    let (i, a) = seed(&pool).await;
    let id = jobs::enqueue_import(&pool, i, a).await.unwrap();
    sqlx::query("UPDATE jobs SET max_attempts=1 WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    jobs::claim(&pool).await.unwrap().unwrap();
    sqlx::query("UPDATE jobs SET lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(jobs::claim(&pool).await.unwrap().is_none());
    let state: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "failed");
    sqlx::query("UPDATE interviews SET expires_at=now()-interval '1 second' WHERE id=$1")
        .bind(i)
        .execute(&pool)
        .await
        .unwrap();
    assert!(jobs::enqueue_import(&pool, i, a).await.is_err());
    drop_database(pool, schema, url).await;
}

async fn one_response(response: String) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let received = socket.read(&mut request).await.unwrap();
        assert!(received > 0);
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    (base, task)
}
#[tokio::test]
async fn provider_pending_missing_redirect_and_oversized_responses_fail_closed() {
    for (body, expected) in [
        (
            json!({"id":"sess_test","status":"active","artifacts":[]}),
            "pending",
        ),
        (
            json!({"id":"sess_test","status":"completed","artifacts":[]}),
            "pending",
        ),
        (
            json!({"id":"sess_test","status":"completed","artifacts":[{"type":"audio","url":"http://169.254.169.254/secret"},{"type":"timeline","url":"https://s3.amazonaws.com/t"},{"type":"metadata","url":"https://s3.amazonaws.com/m"}]}),
            "invalid",
        ),
    ] {
        let body = body.to_string();
        let (base, task) = one_response(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ))
        .await;
        let p = AssemblyHistory::with_endpoint(
            "fixture".into(),
            base.parse().unwrap(),
            vec!["s3.amazonaws.com".into()],
            true,
        )
        .unwrap();
        let result = p.fetch("sess_test").await;
        if expected == "pending" {
            assert!(matches!(result, Err(Error::NotReady)));
        } else {
            assert!(matches!(result, Err(Error::Invalid(_))));
        }
        task.await.unwrap();
    }
    for response in [
        "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 2000000\r\nConnection: close\r\n\r\n",
    ] {
        let (base, task) = one_response(response.into()).await;
        let p =
            AssemblyHistory::with_endpoint("fixture".into(), base.parse().unwrap(), vec![], true)
                .unwrap();
        assert!(matches!(p.fetch("sess_test").await, Err(Error::Invalid(_))));
        task.await.unwrap();
    }
}

struct ChangeDuringFetch {
    pool: PgPool,
    interview: Uuid,
    expire: bool,
}
#[async_trait]
impl HistoryProvider for ChangeDuringFetch {
    async fn fetch(&self, _: &str) -> Result<Artifacts> {
        let query = if self.expire {
            "UPDATE interviews SET expires_at=now()-interval '1 second' WHERE id=$1"
        } else {
            "UPDATE interviews SET revision=revision+1 WHERE id=$1"
        };
        sqlx::query(query)
            .bind(self.interview)
            .execute(&self.pool)
            .await?;
        Ok(artifacts())
    }
}
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn postgres_expiry_and_revision_races_reject_artifacts() {
    for expire in [true, false] {
        let (pool, schema, url) = database().await;
        let (i, a) = seed(&pool).await;
        jobs::enqueue_import(&pool, i, a).await.unwrap();
        let job = jobs::claim(&pool).await.unwrap().unwrap();
        let dir = std::env::temp_dir().join(format!("v0-evidence-{}", Uuid::new_v4()));
        let store = LocalPrivateStorage::new(&dir).await.unwrap();
        assert!(matches!(
            jobs::dispatch(
                &pool,
                &job,
                &ChangeDuringFetch {
                    pool: pool.clone(),
                    interview: i,
                    expire
                },
                &store
            )
            .await,
            Err(Error::Stale)
        ));
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM evidence_imports")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        tokio::fs::remove_dir_all(dir).await.unwrap();
        drop_database(pool, schema, url).await;
    }
}

struct RevokeDuringFetch {
    pool: PgPool,
    interview: Uuid,
}
#[async_trait]
impl HistoryProvider for RevokeDuringFetch {
    async fn fetch(&self, _: &str) -> Result<Artifacts> {
        sqlx::query("UPDATE interviews SET state='revoked' WHERE id=$1")
            .bind(self.interview)
            .execute(&self.pool)
            .await?;
        Ok(artifacts())
    }
}
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn postgres_revocation_is_never_resurrected_by_import_or_failure() {
    let (pool, schema, url) = database().await;
    let (i, a) = seed(&pool).await;
    let id = jobs::enqueue_import(&pool, i, a).await.unwrap();
    sqlx::query("UPDATE jobs SET max_attempts=1 WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let job = jobs::claim(&pool).await.unwrap().unwrap();
    let dir = std::env::temp_dir().join(format!("v0-evidence-{}", Uuid::new_v4()));
    let store = LocalPrivateStorage::new(&dir).await.unwrap();
    assert!(matches!(
        jobs::dispatch(
            &pool,
            &job,
            &RevokeDuringFetch {
                pool: pool.clone(),
                interview: i
            },
            &store
        )
        .await,
        Err(Error::Stale)
    ));
    assert!(jobs::enqueue_import(&pool, i, a).await.is_err());
    jobs::fail(&pool, &job, "invalid_artifacts").await.unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM interviews WHERE id=$1")
        .bind(i)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "revoked");
    sqlx::query(
        "UPDATE jobs SET status='running',lease_until=now()-interval '1 second' WHERE id=$1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    jobs::claim(&pool).await.unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM interviews WHERE id=$1")
        .bind(i)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "revoked");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM evidence_imports")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("UPDATE jobs SET available_at=now() WHERE kind='cleanup_objects'")
        .execute(&pool)
        .await
        .unwrap();
    let cleanup = jobs::claim(&pool).await.unwrap().unwrap();
    sqlx::query("UPDATE jobs SET lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(cleanup.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        jobs::dispatch(&pool, &cleanup, &Fixture, &store).await,
        Err(Error::Stale)
    ));
    assert!(
        tokio::fs::read_dir(&dir)
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_some()
    );
    let new_cleanup = jobs::claim(&pool).await.unwrap().unwrap();
    jobs::dispatch(&pool, &new_cleanup, &Fixture, &store)
        .await
        .unwrap();
    assert!(
        tokio::fs::read_dir(&dir)
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none()
    );
    tokio::fs::remove_dir_all(dir).await.unwrap();
    drop_database(pool, schema, url).await;
}

#[test]
fn provider_range_keeps_distinct_recording_origin_and_remains_unverified() {
    let mut a = artifacts();
    let mut timeline: serde_json::Value = serde_json::from_slice(&a.timeline).unwrap();
    timeline["turns"][0]["user_speech_started_at_ms"] = json!(1000);
    timeline["turns"][0]["user_speech_ended_at_ms"] = json!(4000);
    a.timeline = serde_json::to_vec(&timeline).unwrap();
    let mut metadata: serde_json::Value = serde_json::from_slice(&a.metadata).unwrap();
    metadata["started_at"] = json!("1970-01-01T00:00:00.023Z");
    metadata["dropped_chunks"] = json!(1);
    a.metadata = serde_json::to_vec(&metadata).unwrap();
    let m = manifest::build("sess_test", &a.audio, &a.timeline, &a.metadata).unwrap();
    assert_eq!(m.timeline_started_at_unix_ms, 0);
    assert_eq!(m.recording_started_at_unix_ms, 23);
    assert_eq!(m.segments[0].source_range_ms, Some([977, 3977]));
    assert_eq!(m.segments[0].alignment, "provider_range_unverified");
    assert_eq!(m.dropped_chunks, Some(1));
    assert!(!m.approval_eligible);
}

struct PendingArtifacts;
#[async_trait]
impl HistoryProvider for PendingArtifacts {
    async fn fetch(&self, _: &str) -> Result<Artifacts> {
        Err(Error::NotReady)
    }
}
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn postgres_delayed_artifacts_survive_worker_restart_and_preserve_reconstruction_progress() {
    let (pool, schema, url) = database().await;
    let (i, a) = seed(&pool).await;
    sqlx::query("UPDATE interviews SET topic_index=1,time_consumed_seconds=73,followup_counts='[1,0,0]' WHERE id=$1").bind(i).execute(&pool).await.unwrap();
    let id = jobs::enqueue_import(&pool, i, a).await.unwrap();
    let first = jobs::claim(&pool).await.unwrap().unwrap();
    let dir = std::env::temp_dir().join(format!("v0-restart-test-{}", Uuid::new_v4()));
    let storage = LocalPrivateStorage::new(&dir).await.unwrap();
    assert!(matches!(
        jobs::dispatch(&pool, &first, &PendingArtifacts, &storage).await,
        Err(Error::NotReady)
    ));
    jobs::fail(&pool, &first, "artifacts_not_ready")
        .await
        .unwrap();
    let options = (*pool.connect_options()).clone();
    pool.close().await;
    let restarted = PgPool::connect_with(options).await.unwrap();
    sqlx::query("UPDATE jobs SET available_at=now() WHERE id=$1")
        .bind(id)
        .execute(&restarted)
        .await
        .unwrap();
    let next = jobs::claim(&restarted).await.unwrap().unwrap();
    assert_ne!(first.token, next.token);
    jobs::dispatch(&restarted, &next, &Fixture, &storage)
        .await
        .unwrap();
    let restored = v0_evidence::recovery::recover_interview(&restarted, i)
        .await
        .unwrap();
    assert_eq!(restored.topic_index, 1);
    assert_eq!(restored.time_consumed_seconds, 73);
    assert_eq!(restored.followup_counts, json!([1, 0, 0]));
    assert!(restored.requires_customer_confirmation);
    assert!(!restored.may_advance_progress);
    assert_eq!(restored.unresolved_answers.len(), 2);
    assert!(restored.recorded_utterances.is_empty());
    assert_eq!(
        restored.unresolved_answers[0].source_id,
        "sess_test/turn/turn1/customer"
    );
    assert_eq!(restored.attempts[0].incomplete_turn_ids, vec!["turn2"]);
    sqlx::query("UPDATE interviews SET state='revoked' WHERE id=$1")
        .bind(i)
        .execute(&restarted)
        .await
        .unwrap();
    assert!(matches!(
        v0_evidence::recovery::recover_interview(&restarted, i).await,
        Err(Error::Stale)
    ));
    tokio::fs::remove_dir_all(dir).await.unwrap();
    drop_database(restarted, schema, url).await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn postgres_failure_releases_job_before_waiting_for_interview_lock() {
    let (pool, schema, url) = database().await;
    let (i, a) = seed(&pool).await;
    let id = jobs::enqueue_import(&pool, i, a).await.unwrap();
    sqlx::query("UPDATE jobs SET max_attempts=1 WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let job = jobs::claim(&pool).await.unwrap().unwrap();
    let mut authority = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(i)
        .fetch_one(&mut *authority)
        .await
        .unwrap();
    let cloned = pool.clone();
    let fail = tokio::spawn(async move { jobs::fail(&cloned, &job, "provider_unavailable").await });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let state: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
            if state == "failed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("job failure must commit before waiting on authority");
    sqlx::query("SET LOCAL lock_timeout='500ms'")
        .execute(&mut *authority)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM jobs WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *authority)
        .await
        .unwrap();
    sqlx::query("UPDATE interviews SET state='revoked' WHERE id=$1")
        .bind(i)
        .execute(&mut *authority)
        .await
        .unwrap();
    authority.commit().await.unwrap();
    fail.await.unwrap().unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM interviews WHERE id=$1")
        .bind(i)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "revoked");
    drop_database(pool, schema, url).await;
}

#[test]
fn recovery_never_completes_interrupted_turns_even_with_explicit_finish() {
    let a = artifacts();
    let mut manifest = manifest::build("sess_test", &a.audio, &a.timeline, &a.metadata).unwrap();
    manifest.product_end_reason = Some("explicit_finish".into());
    manifest.dropped_chunks = Some(0);
    for segment in manifest
        .segments
        .iter_mut()
        .filter(|s| s.speaker == "customer")
    {
        segment.source_range_ms = Some([100, 500]);
    }
    let pcm = [1000i16.to_le_bytes(), 1000i16.to_le_bytes()]
        .concat()
        .repeat(24_000);
    manifest.media = Some(v0_evidence::media::check_pcm(&pcm, &manifest));
    // An earlier interrupted turn stays unresolved, independently of final finish intent.
    manifest.segments[0].turn_status = "interrupted".into();
    let (recorded, unresolved) = v0_evidence::recovery::reconstruct(Uuid::new_v4(), &manifest);
    assert!(recorded.is_empty());
    assert_eq!(unresolved.len(), 2);
    assert!(
        unresolved
            .iter()
            .all(|answer| answer.status == "interrupted_turn_requires_confirmation")
    );
    // A manifest's explicit incomplete identity also wins over a completed segment status.
    manifest.segments[0].turn_status = "completed".into();
    manifest.incomplete_turn_ids.push("turn1".into());
    let (recorded, unresolved) = v0_evidence::recovery::reconstruct(Uuid::new_v4(), &manifest);
    assert!(recorded.is_empty());
    assert_eq!(unresolved.len(), 2);
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn postgres_recovery_denies_expiry_crossed_while_waiting_for_row_lock() {
    let (pool, schema, url) = database().await;
    let (i, _) = seed(&pool).await;
    sqlx::query("UPDATE interviews SET expires_at=clock_timestamp()+interval '500 milliseconds' WHERE id=$1").bind(i).execute(&pool).await.unwrap();
    let mut authority = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(i)
        .fetch_one(&mut *authority)
        .await
        .unwrap();
    let app_name = format!("expiry_recovery_{}", Uuid::new_v4().simple());
    let recovery_pool = PgPool::connect_with(
        (*pool.connect_options())
            .clone()
            .application_name(&app_name),
    )
    .await
    .unwrap();
    let recovering = tokio::spawn(async move {
        let result = v0_evidence::recovery::recover_interview(&recovery_pool, i).await;
        recovery_pool.close().await;
        result
    });
    tokio::time::timeout(std::time::Duration::from_secs(3),async {
        loop {
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE application_name=$1 AND wait_event_type='Lock')").bind(&app_name).fetch_one(&pool).await.unwrap();
            if waiting {break;}
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }).await.expect("recovery must reach the held row lock");
    sqlx::query("SELECT pg_sleep(GREATEST(0,EXTRACT(EPOCH FROM expires_at-clock_timestamp())::double precision)+0.01) FROM interviews WHERE id=$1").bind(i).execute(&mut *authority).await.unwrap();
    authority.commit().await.unwrap();
    assert!(matches!(recovering.await.unwrap(), Err(Error::Stale)));
    drop_database(pool, schema, url).await;
}
