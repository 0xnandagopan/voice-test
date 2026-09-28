//! Local-only G1 authority tests; these do not prove real-device audio quality.
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use uuid::Uuid;
use v0_app::{auth, leases, progress, recovery_maintenance};
use v0_domain::workflow::AttemptEndReason;

struct Db {
    pool: PgPool,
    admin: PgPool,
    schema: String,
    id: Uuid,
    token: String,
}
impl Db {
    async fn new() -> Self {
        let url = std::env::var("TEST_DATABASE_URL").expect("isolated TEST_DATABASE_URL required");
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("g1_{}", Uuid::new_v4().simple());
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
        let token = auth::random_secret();
        sqlx::query("INSERT INTO interviews(id,customer_label,project_context,secret_hash,idempotency_key,request_hash,state,consented_at,expires_at) VALUES($1,'Fixture','Synthetic',$2,$3,'fixture','consented',clock_timestamp(),clock_timestamp()+interval '14 days')").bind(id).bind(auth::random_secret()).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO sessions(token_hash,role,interview_id,expires_at) VALUES($1,'customer',$2,clock_timestamp()+interval '14 days')").bind(auth::hash_secret(&token)).bind(id).execute(&pool).await.unwrap();
        Self {
            pool,
            admin,
            schema,
            id,
            token,
        }
    }
    async fn begin(&self) -> (leases::InterviewLease, Uuid) {
        let rev: i64 = sqlx::query_scalar("SELECT revision FROM interviews WHERE id=$1")
            .bind(self.id)
            .fetch_one(&self.pool)
            .await
            .unwrap();
        let lease = leases::acquire_customer(&self.pool, &self.token, self.id, rev)
            .await
            .unwrap();
        let attempt = Uuid::new_v4();
        progress::prepare_attempt(
            &self.pool,
            self.id,
            lease.lease_id,
            lease.generation,
            attempt,
        )
        .await
        .unwrap();
        (lease, attempt)
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
async fn heartbeats_do_not_round_each_tick_or_move_the_six_minute_deadline() {
    let db = Db::new().await;
    let (lease, attempt) = db.begin().await;
    let start: DateTime<Utc> =
        sqlx::query_scalar("SELECT active_since FROM interviews WHERE id=$1")
            .bind(db.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    for _ in 0..40 {
        assert!(
            leases::heartbeat(&db.pool, db.id, lease.lease_id, lease.generation)
                .await
                .unwrap()
                > 350
        );
    }
    let row = sqlx::query("SELECT active_since,time_consumed_seconds FROM interviews WHERE id=$1")
        .bind(db.id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(row.get::<DateTime<Utc>, _>("active_since"), start);
    assert_eq!(row.get::<i32, _>("time_consumed_seconds"), 0);
    sqlx::query(
        "UPDATE interviews SET active_since=clock_timestamp()-interval '359.2 seconds' WHERE id=$1",
    )
    .bind(db.id)
    .execute(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        leases::heartbeat(&db.pool, db.id, lease.lease_id, lease.generation)
            .await
            .unwrap(),
        1
    );
    let bounded: bool = sqlx::query_scalar(
        "SELECT lease_expires_at<=active_since+interval '360 seconds' FROM interviews WHERE id=$1",
    )
    .bind(db.id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(bounded);
    sqlx::query("UPDATE interviews SET active_since=clock_timestamp()-interval '361 seconds',lease_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(db.id).execute(&db.pool).await.unwrap();
    assert_eq!(
        recovery_maintenance::reap_expired(&db.pool).await.unwrap(),
        1
    );
    let value:Value=sqlx::query_scalar("SELECT jsonb_build_object('consumed',i.time_consumed_seconds,'reason',p.product_end_reason) FROM interviews i JOIN provider_attempts p ON p.interview_id=i.id WHERE p.id=$1").bind(attempt).fetch_one(&db.pool).await.unwrap();
    assert_eq!(value, json!({"consumed":360,"reason":"budget_exhausted"}));
    let context = v0_evidence::recovery::recover_interview(&db.pool, db.id)
        .await
        .unwrap();
    assert!(!context.requires_customer_confirmation);
    db.close().await;
}
#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn crash_recovery_preserves_progress_markers_and_requires_acknowledgement() {
    let db = Db::new().await;
    let (lease, attempt) = db.begin().await;
    progress::map_attempt(
        &db.pool,
        db.id,
        lease.lease_id,
        lease.generation,
        attempt,
        "synthetic-session",
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE provider_attempts SET incomplete_turn_ids='[\"partial-item\"]' WHERE id=$1",
    )
    .bind(attempt)
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE interviews SET topic_index=1,followup_counts='[2,1,0]',time_consumed_seconds=100,active_since=clock_timestamp()-interval '11 seconds',lease_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(db.id).execute(&db.pool).await.unwrap();
    assert_eq!(
        recovery_maintenance::reap_expired(&db.pool).await.unwrap(),
        1
    );
    assert_eq!(
        recovery_maintenance::reap_expired(&db.pool).await.unwrap(),
        0
    );
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1")
        .bind(db.id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("state"), "recovering");
    assert_eq!(row.get::<i32, _>("topic_index"), 1);
    assert_eq!(row.get::<Value, _>("followup_counts"), json!([2, 1, 0]));
    assert_eq!(row.get::<i32, _>("time_consumed_seconds"), 110);
    assert!(row.get::<Option<Uuid>, _>("lease_id").is_none());
    assert!(
        leases::acquire_customer(&db.pool, &db.token, db.id, row.get("revision"))
            .await
            .is_err()
    );
    let marks: Value =
        sqlx::query_scalar("SELECT incomplete_turn_ids FROM provider_attempts WHERE id=$1")
            .bind(attempt)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(marks, json!(["partial-item"]));
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE interview_id=$1")
        .bind(db.id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(jobs, 2);
    assert!(
        progress::finalize_relay(
            &db.pool,
            db.id,
            lease.lease_id,
            lease.generation,
            attempt,
            AttemptEndReason::ExplicitStop,
            vec![]
        )
        .await
        .is_err()
    );
    db.close().await;
}
#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn expired_cleanup_never_resurrects_deleted_content_or_imports_evidence() {
    let db = Db::new().await;
    let (lease, attempt) = db.begin().await;
    progress::map_attempt(
        &db.pool,
        db.id,
        lease.lease_id,
        lease.generation,
        attempt,
        "synthetic-session",
    )
    .await
    .unwrap();
    sqlx::query("UPDATE interviews SET state='deleted',deleted_at=clock_timestamp(),lease_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(db.id).execute(&db.pool).await.unwrap();
    recovery_maintenance::reap_expired(&db.pool).await.unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM interviews WHERE id=$1")
        .bind(db.id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(state, "deleted");
    let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM jobs WHERE interview_id=$1")
        .bind(db.id)
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert_eq!(kinds, vec!["delete_provider_agent"]);
    db.close().await;
}
#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn maintenance_skips_busy_and_live_leases_without_waiting() {
    let db = Db::new().await;
    let (_lease, _attempt) = db.begin().await;
    assert_eq!(
        recovery_maintenance::reap_expired(&db.pool).await.unwrap(),
        0
    );
    sqlx::query(
        "UPDATE interviews SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",
    )
    .bind(db.id)
    .execute(&db.pool)
    .await
    .unwrap();
    let mut lock = db.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM interviews WHERE id=$1 FOR UPDATE")
        .bind(db.id)
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            recovery_maintenance::reap_expired(&db.pool)
        )
        .await
        .unwrap()
        .unwrap(),
        0
    );
    lock.commit().await.unwrap();
    assert_eq!(
        recovery_maintenance::reap_expired(&db.pool).await.unwrap(),
        1
    );
    db.close().await;
}
#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn finish_requires_complete_progress_and_no_persisted_partial_answer() {
    let db = Db::new().await;
    let (lease, attempt) = db.begin().await;
    assert!(
        progress::finalize_relay(
            &db.pool,
            db.id,
            lease.lease_id,
            lease.generation,
            attempt,
            AttemptEndReason::ExplicitFinish,
            vec![]
        )
        .await
        .is_err()
    );
    sqlx::query("UPDATE interviews SET topic_index=3 WHERE id=$1")
        .bind(db.id)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE provider_attempts SET incomplete_turn_ids='[\"partial\"]' WHERE id=$1")
        .bind(attempt)
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        progress::finalize_relay(
            &db.pool,
            db.id,
            lease.lease_id,
            lease.generation,
            attempt,
            AttemptEndReason::ExplicitFinish,
            vec![]
        )
        .await
        .is_err()
    );
    sqlx::query("UPDATE provider_attempts SET incomplete_turn_ids='[]' WHERE id=$1")
        .bind(attempt)
        .execute(&db.pool)
        .await
        .unwrap();
    progress::finalize_relay(
        &db.pool,
        db.id,
        lease.lease_id,
        lease.generation,
        attempt,
        AttemptEndReason::ExplicitFinish,
        vec![],
    )
    .await
    .unwrap();
    let row = sqlx::query("SELECT * FROM interviews WHERE id=$1")
        .bind(db.id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("state"), "completed");
    let duration =
        row.get::<DateTime<Utc>, _>("expires_at") - row.get::<DateTime<Utc>, _>("completed_at");
    assert_eq!(duration.num_days(), 30);
    assert!(
        leases::acquire_customer(&db.pool, &db.token, db.id, row.get("revision"))
            .await
            .is_err()
    );
    assert!(
        !v0_evidence::recovery::recover_interview(&db.pool, db.id)
            .await
            .unwrap()
            .requires_customer_confirmation
    );
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn terminal_loss_preserves_durable_partial_items_missing_from_memory() {
    let db = Db::new().await;
    let (lease, attempt) = db.begin().await;
    sqlx::query(
        "UPDATE provider_attempts SET incomplete_turn_ids='[\"durable-partial\"]' WHERE id=$1",
    )
    .bind(attempt)
    .execute(&db.pool)
    .await
    .unwrap();
    progress::finalize_relay(
        &db.pool,
        db.id,
        lease.lease_id,
        lease.generation,
        attempt,
        AttemptEndReason::TransportLost,
        vec!["memory-partial".into(), "durable-partial".into()],
    )
    .await
    .unwrap();
    let items: Value =
        sqlx::query_scalar("SELECT incomplete_turn_ids FROM provider_attempts WHERE id=$1")
            .bind(attempt)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(items.as_array().unwrap().len(), 2);
    assert!(
        items
            .as_array()
            .unwrap()
            .contains(&json!("durable-partial"))
    );
    assert!(items.as_array().unwrap().contains(&json!("memory-partial")));
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL"]
async fn finish_cannot_acknowledge_completion_after_access_is_withdrawn() {
    let db = Db::new().await;
    let (lease, attempt) = db.begin().await;
    sqlx::query("UPDATE interviews SET topic_index=3,state='deleted',deleted_at=clock_timestamp() WHERE id=$1")
        .bind(db.id).execute(&db.pool).await.unwrap();
    assert!(
        progress::finalize_relay(
            &db.pool,
            db.id,
            lease.lease_id,
            lease.generation,
            attempt,
            AttemptEndReason::ExplicitFinish,
            vec![]
        )
        .await
        .is_err()
    );
    let row = sqlx::query("SELECT state,completed_at FROM interviews WHERE id=$1")
        .bind(db.id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("state"), "deleted");
    assert!(
        row.get::<Option<DateTime<Utc>>, _>("completed_at")
            .is_none()
    );
    db.close().await;
}
