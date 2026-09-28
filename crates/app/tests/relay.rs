//! Opt-in provider integration only. Ordinary test runs never create agents.
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::{env, fs, path::Path};
use tokio::time::{Duration, interval, timeout};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use uuid::Uuid;
use v0_app::{
    AppState,
    auth::{hash_secret, random_secret},
    config::Config,
};

type Result<T> = std::result::Result<T, &'static str>;

async fn journey(pool: &PgPool, origin: &str, key: String, summary: &mut Value) -> Result<()> {
    let case = env::var("VOICE_RELAY_CASE").unwrap_or_default();
    let controls = case == "controls_loss";
    let mid_skip = case == "mid_skip_finish";
    let cap = case == "full_cap";
    summary["case"] = json!(if mid_skip {
        "mid_skip_repeat_finish"
    } else if cap {
        "full_elapsed_cap"
    } else if controls {
        "repeat_skip_loss"
    } else {
        "fragmented_stop"
    });
    let id = Uuid::new_v4();
    let secret = random_secret();
    sqlx::query("INSERT INTO interviews(id,customer_label,project_context,secret_hash,idempotency_key,request_hash,expires_at,state) VALUES($1,'Synthetic relay test','Synthetic consented audio only',$2,$3,'synthetic',clock_timestamp()+interval '1 day','invited')")
        .bind(id).bind(random_secret()).bind(Uuid::new_v4()).execute(pool).await.map_err(|_|"fixture_interview")?;
    sqlx::query("INSERT INTO sessions(token_hash,role,interview_id,expires_at) VALUES($1,'customer',$2,clock_timestamp()+interval '1 day')")
        .bind(hash_secret(&secret)).bind(id).execute(pool).await.map_err(|_|"fixture_session")?;
    let state = AppState::new(
        pool.clone(),
        Config {
            origin: origin.into(),
            agency_name: "Synthetic relay probe".into(),
            operator_username: random_secret(),
            operator_password_hash: String::new(),
            invitation_signing_key: random_secret(),
            secure_cookie: true,
            voice_api_key: Some(key),
        },
    )
    .with_controlled_voice(origin.to_string())
    .map_err(|_| "controlled_voice_config")?;
    let app = v0_app::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .map_err(|_| "port3000_in_use")?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "http_client")?;
    let cookie = format!("customer_session={secret}");
    let outcome=timeout(Duration::from_secs(if cap {400} else {100}),async {
        let denied=http.post("http://127.0.0.1:3000/api/customer/start").header("Origin",origin).header("Cookie",&cookie).json(&json!({"interview_id":id,"expected_revision":1})).send().await.map_err(|_|"start_before_consent_transport")?;
        summary["before_consent_status"]=json!(denied.status().as_u16());
        if denied.status()!=reqwest::StatusCode::BAD_REQUEST {return Err("start_before_consent_not_denied");}
        let consent=http.post("http://127.0.0.1:3000/api/customer/consent").header("Origin",origin).header("Cookie",&cookie).json(&json!({"interview_id":id,"policy_version":v0_domain::CONSENT_POLICY_VERSION})).send().await.map_err(|_|"consent_transport")?;
        if !consent.status().is_success(){return Err("consent_rejected");}
        let consent:Value=consent.json().await.map_err(|_|"consent_json")?;
        if controls {sqlx::query("UPDATE interviews SET time_consumed_seconds=320 WHERE id=$1").bind(id).execute(pool).await.map_err(|_|"near_cap_fixture")?;summary["seeded_consumed_seconds"]=json!(320);}
        let start=http.post("http://127.0.0.1:3000/api/customer/start").header("Origin",origin).header("Cookie",&cookie).json(&json!({"interview_id":id,"expected_revision":consent["revision"]})).send().await.map_err(|_|"start_transport")?;
        if !start.status().is_success(){return Err("start_rejected");}
        let start:Value=start.json().await.map_err(|_|"start_json")?;
        let path=start["ws_url"].as_str().filter(|p|p.starts_with("/api/customer/interviews/")).ok_or("start_ws_path")?;
        let make_request=||{
            let mut request=format!("ws://127.0.0.1:3000{path}").into_client_request().unwrap();
            request.headers_mut().insert("Origin",origin.parse().unwrap());
            request.headers_mut().insert("Cookie",cookie.parse().unwrap());
            request
        };
        let mut bad=make_request();bad.headers_mut().insert("Origin","https://untrusted.invalid".parse().unwrap());
        summary["cross_origin_upgrade_rejected"]=json!(tokio_tungstenite::connect_async(bad).await.is_err());
        let started=tokio::time::Instant::now();
        let (mut socket,_)=tokio_tungstenite::connect_async(make_request()).await.map_err(|_|"relay_connect")?;
        let pcm=fs::read(env::var("VOICE_PROBE_PCM").map_err(|_|"synthetic_pcm_path")?).map_err(|_|"synthetic_pcm_read")?;
        if pcm.is_empty()||pcm.len()>720000||pcm.len()%2!=0{return Err("synthetic_pcm_bounds");}
        let mut frames:Vec<_>=pcm.chunks(960).map(|b|STANDARD.encode(b)).collect();frames.extend((0..100).map(|_|STANDARD.encode([0u8;960])));
        let mut tick=interval(Duration::from_millis(20));tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut ready=false;let mut greeting=false;let mut feeding=false;let mut sent=0usize;let mut answer_finals=0usize;let mut reply_audio=0usize;let mut answer_reply=false;let mut stopped=false;
        let mut control_stage=0u8;let mut progress_revision=0i64;let mut revision=0i64;let mut abrupt=false;let mut first_input_frames=0usize;let mut repeat_text=None::<String>;let mut skip_text=None::<String>;
        let observed:Result<()>=async {
            loop {tokio::select! {
                _=tick.tick(),if feeding && sent<frames.len()=>{socket.send(Message::Text(json!({"type":"audio","audio":frames[sent]}).to_string().into())).await.map_err(|_|"audio_transport")?;sent+=1;}
                message=socket.next()=>{
                    let Some(Ok(Message::Text(text)))=message else{return Err("relay_closed_early");};
                    let event:Value=serde_json::from_str(&text).map_err(|_|"relay_json")?;
                    if matches!(event["type"].as_str(),Some("ready"|"state")) {revision=event["revision"].as_i64().ok_or("revision")?;progress_revision=event["progress_revision"].as_i64().ok_or("progress_revision")?;}
                    match event["type"].as_str() {
                        Some("ready")=>{
                            ready=true;
                            let (mut duplicate,_)=tokio_tungstenite::connect_async(make_request()).await.map_err(|_|"duplicate_connect")?;
                            let duplicate=timeout(Duration::from_secs(3),duplicate.next()).await.map_err(|_|"duplicate_timeout")?.ok_or("duplicate_closed")?.map_err(|_|"duplicate_error")?;
                            let duplicate:Value=serde_json::from_str(duplicate.to_text().map_err(|_|"duplicate_frame")?).map_err(|_|"duplicate_json")?;
                            summary["duplicate_socket_rejected"]=json!(duplicate["type"]=="error"&&duplicate["code"]=="conflict");
                        }
                        Some("caption") if event["speaker"]=="interviewer" && event["final"]==true=>{
                            if !greeting {greeting=true;feeding= !cap;} else if mid_skip {
                                match control_stage {
                                    1=>{skip_text=event["text"].as_str().map(str::to_owned);repeat_text=skip_text.clone();socket.send(Message::Text(json!({"type":"control","request_id":Uuid::new_v4(),"action":"repeat","expected_revision":revision,"expected_progress_revision":progress_revision}).to_string().into())).await.map_err(|_|"mid_repeat_send")?;control_stage=2;}
                                    2=>{summary["repeat_exact_text"]=json!(event["text"].as_str()==repeat_text.as_deref());socket.send(Message::Text(json!({"type":"control","request_id":Uuid::new_v4(),"action":"skip","expected_revision":revision,"expected_progress_revision":progress_revision}).to_string().into())).await.map_err(|_|"result_skip_send")?;control_stage=3;}
                                    3=>{socket.send(Message::Text(json!({"type":"control","request_id":Uuid::new_v4(),"action":"skip","expected_revision":revision,"expected_progress_revision":progress_revision}).to_string().into())).await.map_err(|_|"completion_skip_send")?;control_stage=4;}
                                    _=>{}
                                }
                            } else if answer_finals>0 {
                                answer_reply=true;
                                if controls {
                                    match control_stage {
                                        0=>{repeat_text=event["text"].as_str().map(str::to_owned);let request=json!({"type":"control","request_id":Uuid::new_v4(),"action":"repeat","expected_revision":revision,"expected_progress_revision":progress_revision});socket.send(Message::Text(request.to_string().into())).await.map_err(|_|"repeat_send")?;socket.send(Message::Text(request.to_string().into())).await.map_err(|_|"repeat_duplicate_send")?;control_stage=1;}
                                        1=>{summary["repeat_exact_text"]=json!(event["text"].as_str()==repeat_text.as_deref());socket.send(Message::Text(json!({"type":"control","request_id":Uuid::new_v4(),"action":"skip","expected_revision":revision,"expected_progress_revision":progress_revision}).to_string().into())).await.map_err(|_|"skip_send")?;control_stage=2;}
                                        2=>{skip_text=event["text"].as_str().map(str::to_owned);first_input_frames=sent;sent=0;feeding=true;control_stage=3;}
                                        _=>{}
                                    }
                                }
                            }
                        }
                        Some("caption") if event["speaker"]=="customer" && event["final"]==true=>{answer_finals+=1;}
                        Some("caption") if event["speaker"]=="customer" && event["final"]==false && mid_skip && control_stage==0=>{socket.send(Message::Text(json!({"type":"control","request_id":Uuid::new_v4(),"action":"skip","expected_revision":revision,"expected_progress_revision":progress_revision}).to_string().into())).await.map_err(|_|"mid_utterance_skip_send")?;control_stage=1;summary["skip_during_provisional_answer"]=json!(true);}
                        Some("state") if mid_skip && control_stage==4 && event["can_finish"]==true=>{socket.send(Message::Text(json!({"type":"control","request_id":Uuid::new_v4(),"action":"finish","expected_revision":revision,"expected_progress_revision":progress_revision}).to_string().into())).await.map_err(|_|"finish_send")?;control_stage=5;stopped=true;}
                        Some("caption") if event["speaker"]=="customer" && event["final"]==false && controls && control_stage==3=>{abrupt=true;break;}
                        Some("audio") if answer_finals>0=>{reply_audio+=1;}
                        Some("error")=>return Err("relay_error_event"),
                        Some("control_rejected")=>return Err("relay_control_rejected"),
                        Some("ended")=>{summary["ended_reason"]=event["reason"].clone();summary["recovery_pending"]=event["recovery_pending"].clone();break;}
                        _=>{}
                    }
                    if !controls && !mid_skip && !cap && answer_reply && sent==frames.len() && !stopped {
                        socket.send(Message::Text(json!({"type":"stop","request_id":Uuid::new_v4()}).to_string().into())).await.map_err(|_|"stop_transport")?;stopped=true;
                    }
                }
            }}
            Ok(())
        }.await;
        if abrupt {
            // Drop TCP without a WebSocket Close or product Stop, during a
            // provisional customer utterance. This is not a clean finish.
            drop(socket);
            tokio::time::sleep(Duration::from_secs(8)).await;
        } else {
            if !stopped {let _=socket.send(Message::Text(json!({"type":"stop","request_id":Uuid::new_v4()}).to_string().into())).await;}
            let _=timeout(Duration::from_secs(12),async {while socket.next().await.is_some(){}}).await;
            let _=socket.close(None).await;
        }
        summary["elapsed_seconds"]=json!(started.elapsed().as_secs_f64());
        summary["abrupt_disconnect"]=json!(abrupt);summary["control_stage"]=json!(control_stage);summary["skip_changes_question"]=json!(skip_text.is_some()&&skip_text!=repeat_text);summary["first_input_frames_sent"]=json!(first_input_frames);
        summary["ready"]=json!(ready);summary["greeting_delivered"]=json!(greeting);summary["input_frames_sent"]=json!(sent);summary["input_frames_total"]=json!(frames.len());summary["answer_final_events"]=json!(answer_finals);summary["reply_audio_frames"]=json!(reply_audio);summary["answer_reply_delivered"]=json!(answer_reply);summary["explicit_stop_sent"]=json!(stopped);
        observed?;
        let row=sqlx::query("SELECT i.state,i.lease_id,i.time_consumed_seconds,i.topic_index,i.completed_answers,i.followup_counts,p.incomplete_turn_ids,p.product_end_reason,p.provider_agent_id,p.provider_agent_name,p.ended_at FROM interviews i JOIN provider_attempts p ON p.interview_id=i.id WHERE i.id=$1").bind(id).fetch_one(pool).await.map_err(|_|"durable_state")?;
        summary["state"]=json!(row.get::<String,_>("state"));summary["lease_released"]=json!(row.get::<Option<Uuid>,_>("lease_id").is_none());summary["completed_answers"]=json!(row.get::<i32,_>("completed_answers"));summary["followups"]=row.get::<Value,_>("followup_counts");summary["product_end_reason"]=json!(row.get::<Option<String>,_>("product_end_reason"));summary["provider_agent_deleted"]=json!(row.get::<Option<String>,_>("provider_agent_id").is_none()&&row.get::<Option<String>,_>("provider_agent_name").is_none());
        summary["consumed_seconds"]=json!(row.get::<i32,_>("time_consumed_seconds"));summary["topic"]=json!(row.get::<i32,_>("topic_index"));summary["incomplete_turn_count"]=json!(row.get::<Value,_>("incomplete_turn_ids").as_array().map(Vec::len));
        let kinds:Vec<String>=sqlx::query_scalar("SELECT kind FROM jobs WHERE interview_id=$1 ORDER BY kind").bind(id).fetch_all(pool).await.map_err(|_|"durable_jobs")?;summary["durable_jobs"]=json!(kinds);
        if cap {
            if !ready||!greeting||sent!=0||summary["product_end_reason"]!="budget_exhausted"||summary["consumed_seconds"]!=360||summary["lease_released"]!=true||summary["provider_agent_deleted"]!=true||!(355.0..=385.0).contains(&started.elapsed().as_secs_f64()){return Err("full_cap_acceptance_failed");}
            return Ok(());
        }
        if mid_skip {
            if !ready||!greeting||control_stage!=5||answer_finals==0||summary["product_end_reason"]!="explicit_finish"||summary["state"]!="completed"||summary["topic"]!=3||summary["completed_answers"]!=0||summary["followups"]!=json!([0,0,0])||summary["repeat_exact_text"]!=true||summary["lease_released"]!=true||summary["provider_agent_deleted"]!=true {return Err("mid_skip_finish_acceptance_failed");}
            return Ok(());
        }
        if !ready||!greeting||!answer_reply||reply_audio==0||(!controls&&sent!=frames.len())||summary["completed_answers"]!=1||summary["lease_released"]!=true||summary["product_end_reason"]!=if controls {"transport_lost"} else {"explicit_stop"}||summary["provider_agent_deleted"]!=true||summary["cross_origin_upgrade_rejected"]!=true||summary["duplicate_socket_rejected"]!=true {return Err("relay_acceptance_failed");}
        if controls && (!abrupt||control_stage!=3||summary["repeat_exact_text"]!=true||summary["skip_changes_question"]!=true||summary["topic"]!=1||summary["followups"]!=json!([1,0,0])||summary["consumed_seconds"].as_i64().is_none_or(|n|!(320..=360).contains(&n))||summary["incomplete_turn_count"].as_u64().is_none_or(|n|n==0)){return Err("controls_loss_acceptance_failed");}
        Ok::<(),&'static str>(())
    }).await;
    // If the outer deadline interrupted the client, the task sees socket loss.
    tokio::time::sleep(Duration::from_secs(2)).await;
    server.abort();
    let _ = server.await;
    outcome.map_err(|_| "relay_probe_timeout")?
}

#[tokio::test]
#[ignore = "requires explicit VOICE_RELAY_LIVE=1, active named tunnel and synthetic PCM"]
async fn controlled_live_relay_consent_fragmented_answer_and_stop() {
    assert_eq!(
        env::var("VOICE_RELAY_LIVE").as_deref(),
        Ok("1"),
        "explicit live opt-in required"
    );
    let key = env::var("VOICE_AGENT_API_KEY").expect("voice key must be supplied privately");
    let url = env::var("TEST_DATABASE_URL").expect("isolated loopback test database required");
    assert!(matches!(
        url::Url::parse(&url).unwrap().host_str(),
        Some("127.0.0.1" | "localhost")
    ));
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let schema = format!("relay_probe_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let search = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(6)
        .after_connect(move |c, _| {
            let s = search.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO {s}"))
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
    let mut summary = json!({"synthetic":true,"global_g1_passed":false,"global_g2_passed":false});
    let result = journey(
        &pool,
        "https://slug-rev-tests.trypreview.online",
        key,
        &mut summary,
    )
    .await;
    if let Err(error) = result {
        summary["error"] = json!(error);
    }
    let remaining:i64=sqlx::query_scalar("SELECT COUNT(*) FROM provider_attempts WHERE provider_agent_name IS NOT NULL OR provider_agent_id IS NOT NULL").fetch_one(&pool).await.unwrap();
    summary["provider_cleanup_handles_remaining"] = json!(remaining);
    pool.close().await;
    if remaining == 0 {
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&admin)
            .await
            .unwrap();
        summary["isolated_schema_removed"] = json!(true);
    } else {
        summary["isolated_schema_removed"] = json!(false);
        let handle = Path::new(".local/verification/relay-cleanup-schema.txt");
        if let Some(parent) = handle.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(handle, schema).unwrap();
    }
    admin.close().await;
    let path = env::var("VOICE_RELAY_SUMMARY")
        .unwrap_or_else(|_| ".local/verification/day2-relay.json".into());
    if let Some(parent) = Path::new(&path).parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, serde_json::to_vec_pretty(&summary).unwrap()).unwrap();
    assert!(
        result.is_ok(),
        "Controlled relay probe failed; inspect the redacted summary and retained cleanup handles."
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL via TEST_DATABASE_URL; no provider calls"]
async fn stop_during_startup_releases_lease_without_creating_provider_session() {
    use axum::{Router, extract::ws::WebSocketUpgrade, routing::get};
    let url = env::var("TEST_DATABASE_URL").expect("isolated database required");
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let schema = format!("relay_stop_{}", Uuid::new_v4().simple());
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
    let secret = random_secret();
    sqlx::query("INSERT INTO interviews(id,customer_label,project_context,secret_hash,idempotency_key,request_hash,expires_at,consented_at,state) VALUES($1,'Synthetic','Startup stop',$2,$3,'synthetic',clock_timestamp()+interval '1 day',clock_timestamp(),'consented')").bind(id).bind(random_secret()).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO sessions(token_hash,role,interview_id,expires_at) VALUES($1,'customer',$2,clock_timestamp()+interval '1 day')").bind(hash_secret(&secret)).bind(id).execute(&pool).await.unwrap();
    let relay_pool = pool.clone();
    let app = Router::new().route(
        "/socket",
        get(move |ws: WebSocketUpgrade| {
            let pool = relay_pool.clone();
            let secret = secret.clone();
            async move {
                ws.on_upgrade(move |socket| {
                    v0_app::relay::run(
                        socket,
                        pool,
                        secret,
                        id,
                        1,
                        v0_app::relay::RelayConfig {
                            api_key: String::new(),
                            public_origin: "http://invalid.local".into(),
                        },
                    )
                })
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/socket"))
        .await
        .unwrap();
    socket
        .send(Message::Text(
            json!({"type":"stop","request_id":Uuid::new_v4()})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        while socket.next().await.is_some() {}
    })
    .await
    .unwrap();
    let row=sqlx::query("SELECT i.lease_id,i.state,p.provider_session_id,p.product_end_reason FROM interviews i JOIN provider_attempts p ON p.interview_id=i.id WHERE i.id=$1").bind(id).fetch_one(&pool).await.unwrap();
    assert!(row.get::<Option<Uuid>, _>("lease_id").is_none());
    assert_eq!(row.get::<String, _>("state"), "recovering");
    assert!(
        row.get::<Option<String>, _>("provider_session_id")
            .is_none()
    );
    assert_eq!(
        row.get::<Option<String>, _>("product_end_reason")
            .as_deref(),
        Some("explicit_stop")
    );
    // A later lease is fenced from delayed cleanup belonging to an old socket.
    let revision: i64 = sqlx::query_scalar("SELECT revision FROM interviews WHERE id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let next = v0_app::leases::acquire(&pool, id, revision).await.unwrap();
    let next_attempt = Uuid::new_v4();
    v0_app::progress::map_attempt(
        &pool,
        id,
        next.lease_id,
        next.generation,
        next_attempt,
        "synthetic-finalization-only",
    )
    .await
    .unwrap();
    use v0_domain::workflow::AttemptEndReason;
    assert!(
        v0_app::progress::finalize_relay(
            &pool,
            id,
            Uuid::new_v4(),
            next.generation - 1,
            next_attempt,
            AttemptEndReason::ExplicitStop,
            vec![]
        )
        .await
        .is_err()
    );
    let active: Option<Uuid> = sqlx::query_scalar("SELECT lease_id FROM interviews WHERE id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(active, Some(next.lease_id));
    v0_app::progress::finalize_relay(
        &pool,
        id,
        next.lease_id,
        next.generation,
        next_attempt,
        AttemptEndReason::TransportLost,
        vec!["synthetic-incomplete".into()],
    )
    .await
    .unwrap();
    let before:Value=sqlx::query_scalar("SELECT jsonb_build_object('consumed',time_consumed_seconds,'revision',revision) FROM interviews WHERE id=$1").bind(id).fetch_one(&pool).await.unwrap();
    assert!(
        v0_app::progress::finalize_relay(
            &pool,
            id,
            next.lease_id,
            next.generation,
            next_attempt,
            AttemptEndReason::ExplicitFinish,
            vec![]
        )
        .await
        .is_err()
    );
    let after:Value=sqlx::query_scalar("SELECT jsonb_build_object('consumed',time_consumed_seconds,'revision',revision) FROM interviews WHERE id=$1").bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(before, after);
    let markers: Value =
        sqlx::query_scalar("SELECT incomplete_turn_ids FROM provider_attempts WHERE id=$1")
            .bind(next_attempt)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(markers, json!(["synthetic-incomplete"]));
    let imports: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE dedupe_key=$1")
        .bind(format!("import:{next_attempt}"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(imports, 1);
    server.abort();
    let _ = server.await;
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
