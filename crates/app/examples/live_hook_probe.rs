//! Opt-in synthetic live probe. This is a gate diagnostic, not the customer relay.
//! Exposes only the authenticated hook and health on loopback for the named tunnel.
//! Raw transcripts, callback secrets and provider IDs never enter the summary/logs.
use axum::{
    extract::Request,
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::{
    collections::BTreeMap,
    env,
    sync::{Arc, Mutex},
};
use tokio::time::{Duration, Instant, interval, timeout};
use uuid::Uuid;
use v0_app::{
    AppState,
    auth::{hash_secret, random_secret},
    config::Config,
    leases, progress, voice_hook,
};
use v0_voice::{client::VoiceClient, pre_speech::QuestionPlan, protocol::ClientEvent};
type Result<T> = std::result::Result<T, String>;

struct Fixture {
    id: Uuid,
    attempt: Uuid,
    lease: leases::InterviewLease,
    secret: String,
}
async fn fixture(pool: &PgPool) -> Result<Fixture> {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO interviews(id,customer_label,project_context,secret_hash,idempotency_key,request_hash,expires_at,consented_at,consent_policy_version,state) VALUES($1,'Synthetic test','Synthetic controlled test only',$2,$3,'probe',now()+interval '1 hour',now(),'synthetic-test','consented')")
        .bind(id).bind(hash_secret(&random_secret())).bind(Uuid::new_v4()).execute(pool).await.map_err(|_|"fixture_insert")?;
    sqlx::query("INSERT INTO sessions(token_hash,role,interview_id,expires_at) VALUES($1,'customer',$2,now()+interval '1 hour')")
        .bind(hash_secret(&random_secret())).bind(id).execute(pool).await.map_err(|_|"fixture_session")?;
    let lease = leases::acquire(pool, id, 1)
        .await
        .map_err(|_| "lease_acquire")?;
    let attempt = Uuid::new_v4();
    progress::prepare_attempt(pool, id, lease.lease_id, lease.generation, attempt)
        .await
        .map_err(|_| "prepare_attempt")?;
    let secret = voice_hook::bind(pool, id, attempt, lease.lease_id, lease.generation)
        .await
        .map_err(|_| "bind_hook")?;
    Ok(Fixture {
        id,
        attempt,
        lease,
        secret,
    })
}
async fn current_permit(pool: &PgPool, attempt: Uuid) -> Result<(Uuid, String)> {
    let row=sqlx::query("SELECT q.id,q.plan FROM voice_hook_bindings b JOIN question_permits q ON q.id=b.last_permit_id WHERE b.attempt_id=$1")
        .bind(attempt).fetch_one(pool).await.map_err(|_|"read_permit")?;
    let plan: QuestionPlan =
        serde_json::from_value(row.get("plan")).map_err(|_| "decode_permit")?;
    Ok((row.get("id"), plan.code.text().to_string()))
}
async fn preflight(pool: &PgPool, origin: &str, summary: &mut Value) -> Result<()> {
    let f = fixture(pool).await?;
    progress::map_attempt(
        pool,
        f.id,
        f.lease.lease_id,
        f.lease.generation,
        f.attempt,
        &format!("synthetic-preflight-{}", Uuid::new_v4()),
    )
    .await
    .map_err(|_| "preflight_map")?;
    let url = format!("{origin}/voice-hook/{}/chat/completions", f.attempt);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "http_client")?;
    let body = json!({"model":"v0-bounded","messages":[],"stream":true});
    let denied = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|_| "public_preflight_network")?;
    summary["unauthenticated_status"] = json!(denied.status().as_u16());
    if denied.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Err("public_hook_did_not_reject_missing_auth".into());
    }
    let reply = client
        .post(&url)
        .bearer_auth(&f.secret)
        .json(&body)
        .send()
        .await
        .map_err(|_| "public_sse_network")?;
    let status = reply.status();
    let sse = reply
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    let text = reply.text().await.map_err(|_| "public_sse_body")?;
    let expected = current_permit(pool, f.attempt).await?.1;
    let done = text.contains("data: [DONE]");
    let content: String = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|s| serde_json::from_str::<Value>(s).ok())
        .filter_map(|v| {
            v["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    let matched = content == expected;
    summary["public_sse_preflight"] = json!({"status":status.as_u16(),"content_type":sse,"done":done,"matches_permit":matched,"incremental_latency_proven":false});
    if !status.is_success() || !sse || !done || !matched {
        return Err("public_sse_preflight_failed".into());
    }
    Ok(())
}
// Only safe enumerated provider codes enter the report, never error text/config.
fn provider_code(event: &Value) -> &'static str {
    match event["code"].as_str().unwrap_or("") {
        "invalid_config" => "invalid_config",
        "invalid_format" => "invalid_format",
        "agent_init_failed" => "agent_init_failed",
        "agent_timeout" => "agent_timeout",
        "agent_not_found" => "agent_not_found",
        "agent_id_not_first" => "agent_id_not_first",
        "session_expired" => "session_expired",
        "server_error" => "server_error",
        "at_capacity" => "at_capacity",
        "concurrency_exceeded" => "concurrency_exceeded",
        "UNAUTHORIZED" => "UNAUTHORIZED",
        "FORBIDDEN" => "FORBIDDEN",
        _ => "other",
    }
}
struct StoredAgent {
    id: String,
    recovery_path: std::path::PathBuf,
}
async fn delete_agent(agent: &StoredAgent, key: &str, summary: &mut Value) -> bool {
    let result = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client
            .delete(format!(
                "https://agents.assemblyai.com/v1/agents/{}",
                agent.id
            ))
            .header("Authorization", key)
            .send()
            .await
            .ok(),
        Err(_) => None,
    };
    let deleted = result
        .as_ref()
        .is_some_and(|r| r.status().is_success() || r.status() == reqwest::StatusCode::NOT_FOUND);
    summary["stored_agent_delete_status"] = json!(result.as_ref().map(|r| r.status().as_u16()));
    summary["stored_agent_deleted"] = json!(deleted);
    if deleted {
        let _ = std::fs::remove_file(&agent.recovery_path);
    }
    deleted
}
async fn create_agent(
    f: &Fixture,
    origin: &str,
    greeting: &str,
    key: &str,
    summary: &mut Value,
) -> Result<StoredAgent> {
    use std::io::{Seek, Write};
    use std::os::unix::fs::OpenOptionsExt;
    let recovery_path = std::path::PathBuf::from(
        env::var("VOICE_PROBE_AGENT_MAP")
            .unwrap_or_else(|_| ".local/verification/custom-hook-agent-cleanup.json".into()),
    );
    if let Some(parent) = recovery_path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| "agent_map_directory")?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&recovery_path)
        .map_err(|_| "agent_map_must_be_new")?;
    let name = format!("synthetic-hook-probe-{}", Uuid::new_v4());
    // Persist lookup name before POST: even an ambiguous create timeout is recoverable.
    file.write_all(
        serde_json::to_string(&json!({"agent_name":name,"creation_pending":true,"synthetic":true}))
            .unwrap()
            .as_bytes(),
    )
    .map_err(|_| "agent_map_write")?;
    file.sync_all().map_err(|_| "agent_map_sync")?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "agent_http_client")?;
    let response=http.post("https://agents.assemblyai.com/v1/agents").header("Authorization",key).json(&json!({"name":name,"system_prompt":"Ask only the exact question supplied by the custom completion service. This is a synthetic test.","voice":{"voice_id":"alba"},"input":{"format":{"encoding":"audio/pcm"},"language_codes":["en"]},"output":{"format":{"encoding":"audio/pcm"},"voice":"alba"},"greeting":greeting,"llm":[{"base_url":format!("{origin}/voice-hook/{}",f.attempt),"model":"v0-bounded","api_key":f.secret}]})).send().await.map_err(|_|"agent_create_network_recovery_map_retained")?;
    summary["stored_agent_create_status"] = json!(response.status().as_u16());
    if !response.status().is_success() {
        if response.status().is_client_error() {
            let _ = std::fs::remove_file(&recovery_path);
        }
        return Err("agent_create_rejected".into());
    }
    let value: Value = response
        .json()
        .await
        .map_err(|_| "agent_create_response_recovery_map_retained")?;
    let id = value["id"]
        .as_str()
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 200
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .ok_or("agent_id_invalid_recovery_map_retained")?
        .to_owned();
    let agent = StoredAgent {
        id: id.clone(),
        recovery_path,
    };
    let saved = (|| -> std::io::Result<()> {
        file.rewind()?;
        file.set_len(0)?;
        file.write_all(
            serde_json::to_string(&json!({"agent_id":id,"agent_name":name,"synthetic":true}))
                .unwrap()
                .as_bytes(),
        )?;
        file.sync_all()
    })();
    if saved.is_err() {
        delete_agent(&agent, key, summary).await;
        return Err("agent_map_write".into());
    }
    summary["stored_agent_created"] = json!(true);
    Ok(agent)
}
async fn live(pool: &PgPool, origin: &str, summary: &mut Value) -> Result<()> {
    let key = env::var("VOICE_AGENT_API_KEY").map_err(|_| "missing_voice_key")?;
    let pcm =
        tokio::fs::read(env::var("VOICE_PROBE_PCM").map_err(|_| "missing_synthetic_pcm_path")?)
            .await
            .map_err(|_| "pcm_read")?;
    if pcm.is_empty() || pcm.len() > 24_000 * 2 * 15 || pcm.len() % 2 != 0 {
        return Err("pcm_must_be_synthetic_mono16_24k_under15sec".into());
    }
    let f = fixture(pool).await?;
    let greeting = voice_hook::configured_greeting(pool, f.attempt, &f.secret)
        .await
        .map_err(|_| "greeting")?;
    let stored = if env::var("VOICE_PROBE_STORED_AGENT").as_deref() == Ok("1") {
        Some(create_agent(&f, origin, &greeting, &key, summary).await?)
    } else {
        None
    };
    summary["stored_agent_mode"] = json!(stored.is_some());
    let connected = timeout(
        Duration::from_secs(12),
        VoiceClient::connect_bounded(&key, 60),
    )
    .await;
    let mut client = match connected {
        Ok(Ok(client)) => client,
        other => {
            if let Some(agent) = &stored {
                delete_agent(agent, &key, summary).await;
            }
            return Err(if other.is_err() {
                "bounded_provider_connect_timeout"
            } else {
                "bounded_provider_connect"
            }
            .into());
        }
    };
    let session = if let Some(agent) = &stored {
        json!({"agent_id":agent.id})
    } else {
        json!({"system_prompt":"Ask only the exact question supplied by the custom completion service. This is a synthetic test.","greeting":greeting,"input":{"format":{"encoding":"audio/pcm"},"language_codes":["en"]},"output":{"format":{"encoding":"audio/pcm"},"voice":"alba"},"llm":[{"base_url":format!("{origin}/voice-hook/{}",f.attempt),"model":"v0-bounded","api_key":f.secret}]})
    };
    let outcome=timeout(Duration::from_secs(35), async {
        client.send(&ClientEvent::Configure{session}).await.map_err(|_|"provider_configure")?;
        let ready=timeout(Duration::from_secs(12),async {
            loop {let e=client.receive_raw().await.map_err(|_|"provider_ready_transport")?.ok_or("provider_closed_before_ready")?;
                match e["type"].as_str(){Some("session.ready")=>return Ok::<Value,String>(e),Some("session.error")=>{summary["provider_error_code"]=json!(provider_code(&e));return Err("provider_configuration_rejected".into());},_=>{}}
            }
        }).await.map_err(|_|"provider_ready_timeout")??;
        let provider_id=ready["session_id"].as_str().ok_or("missing_provider_identity")?;
        progress::map_attempt(pool,f.id,f.lease.lease_id,f.lease.generation,f.attempt,provider_id).await.map_err(|_|"provider_mapping")?;
        summary["provider_ready_mapped"]=json!(true);
        leases::heartbeat(pool,f.id,f.lease.lease_id,f.lease.generation).await.map_err(|_|"mapped_heartbeat")?;
        if let Ok(path)=env::var("VOICE_PROBE_ARTIFACT_MAP") {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).map_err(|_|"private_map_create")?;
            file.write_all(serde_json::to_string(&json!({"session_ids":[provider_id],"synthetic":true})).unwrap().as_bytes()).map_err(|_|"private_map_write")?;
        }
        // Wait for completed configured greeting before injecting the synthetic answer.
        timeout(Duration::from_secs(12),async {
            loop {let e=client.receive_raw().await.map_err(|_|"greeting_transport")?.ok_or("greeting_closed")?;
                if e["type"]=="reply.audio" {let permit=current_permit(pool,f.attempt).await?.0;voice_hook::mark_delivered(pool,f.attempt,permit,f.lease.lease_id,f.lease.generation).await.map_err(|_|"greeting_delivery")?;}
                if e["type"]=="reply.done" {return Ok::<(),String>(());}
                if e["type"]=="session.error" {summary["provider_error_code"]=json!(provider_code(&e));return Err("greeting_provider_error".into());}
            }
        }).await.map_err(|_|"greeting_timeout")??;
        leases::heartbeat(pool,f.id,f.lease.lease_id,f.lease.generation).await.map_err(|_|"initial_heartbeat")?;
        let initial=current_permit(pool,f.attempt).await?.0;
        let mut chunks:Vec<String>=pcm.chunks(960).map(|c|STANDARD.encode(c)).collect();
        chunks.extend((0..100).map(|_|STANDARD.encode([0u8;960])));
        let mut tick=interval(Duration::from_millis(20));tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut heartbeat=interval(Duration::from_secs(10));
        let start=Instant::now();
        let deadline=tokio::time::sleep(Duration::from_secs(30));tokio::pin!(deadline);
        let mut pos=0;let mut audio_chunks=0u32;let mut pinned=None;let mut admitted=false;let mut counts=BTreeMap::<String,u32>::new();
        let mut events=Vec::new();let mut audio_before_permit=false;let mut agent_matched=false;let mut final_text_seen=false;
        let mut terminal_error=None;
        loop {tokio::select!{
            _=tick.tick(),if pos<chunks.len()=>{client.send(&ClientEvent::Audio{audio:chunks[pos].clone()}).await.map_err(|_|"audio_send")?;pos+=1;}
            _=heartbeat.tick()=>{leases::heartbeat(pool,f.id,f.lease.lease_id,f.lease.generation).await.map_err(|_|"heartbeat")?;}
            _=&mut deadline=>{break;}
            event=client.receive_raw()=>{
                let e=event.map_err(|_|"provider_receive")?.ok_or("provider_closed")?;
                let kind=e["type"].as_str().unwrap_or("unknown");*counts.entry(kind.to_string()).or_default()+=1;
                if !matches!(kind,"reply.audio"|"transcript.user.delta"|"transcript.agent.delta"){events.push(json!({"type":kind,"at_ms":start.elapsed().as_millis()}));}
                match kind {
                    "input.speech.started"=>{pinned=voice_hook::questions(pool,f.attempt,f.lease.lease_id,f.lease.generation).await.map_err(|_|"delivered_question")?.1;}
                    "transcript.user"=>{
                        let permit=pinned.ok_or("final_without_speech_start")?;
                        voice_hook::authorize_answer(pool,f.attempt,permit,e["item_id"].as_str().ok_or("missing_final_item_id")?,e["text"].as_str().ok_or("missing_final_text")?,f.lease.lease_id,f.lease.generation).await.map_err(|_|"answer_admission_failed")?;
                        admitted=true;events.push(json!({"type":"answer_admission_committed","at_ms":start.elapsed().as_millis()}));
                    }
                    "reply.audio"=>{audio_chunks+=1;let permit=current_permit(pool,f.attempt).await?.0;if permit==initial {audio_before_permit=true;}voice_hook::mark_delivered(pool,f.attempt,permit,f.lease.lease_id,f.lease.generation).await.map_err(|_|"reply_delivery")?;}
                    "transcript.agent"=>{let current=current_permit(pool,f.attempt).await?;final_text_seen=true;agent_matched=current.0!=initial&&e["text"].as_str().is_some_and(|s|s.trim()==current.1);}
                    "reply.done"=>{if admitted&&final_text_seen {break;}}
                    "session.error"=>{summary["provider_error_code"]=json!(provider_code(&e));terminal_error=Some("provider_session_error");break;}
                    "session.ended"=>{terminal_error=Some("provider_session_ended");break;}
                    _=>{}
                }
            }
        }}
        let persisted=sqlx::query("SELECT topic_index,followup_counts,completed_answers FROM interviews WHERE id=$1").bind(f.id).fetch_one(pool).await.map_err(|_|"final_progress")?;
        summary["live"]=json!({"event_counts":counts,"events":events,"input_frames_sent":pos,"input_frames_total":chunks.len(),"answer_admitted":admitted,"reply_audio_chunks":audio_chunks,"audio_before_new_permit":audio_before_permit,"final_agent_text_seen":final_text_seen,"final_agent_text_matches_permit":agent_matched,"topic":persisted.get::<i32,_>("topic_index"),"followups":persisted.get::<Value,_>("followup_counts"),"completed_answers":persisted.get::<i32,_>("completed_answers"),"terminal_error":terminal_error});
        if !admitted || !agent_matched || audio_chunks == 0 || audio_before_permit || terminal_error.is_some() {
            return Err("live_diagnostic_failed".into());
        }
        Ok::<(),String>(())
    }).await;
    // Explicit Stop even when the probe fails or reaches its deadline.
    summary["provider_stop_sent"] = json!(client.send(&ClientEvent::End).await.is_ok());
    summary["provider_socket_closed"] = json!(
        timeout(Duration::from_secs(2), client.disconnect())
            .await
            .is_ok_and(|r| r.is_ok())
    );
    let _ = progress::end_attempt(
        pool,
        f.id,
        f.lease.lease_id,
        f.lease.generation,
        f.attempt,
        v0_domain::workflow::AttemptEndReason::ExplicitStop,
    )
    .await;
    if let Some(agent) = &stored
        && !delete_agent(agent, &key, summary).await
    {
        return Err("stored_agent_cleanup_failed_recovery_map_retained".into());
    }
    match outcome {
        Ok(result) => result,
        Err(_) => Err("live_probe_deadline".into()),
    }
}

async fn run(summary: &mut Value) -> Result<()> {
    if env::var("VOICE_LIVE_PROBE").as_deref() != Ok("1") {
        return Err("explicit_opt_in_required".into());
    }
    let origin = env::var("VOICE_HOOK_PUBLIC_ORIGIN")
        .unwrap_or_else(|_| "https://slug-rev-tests.trypreview.online".into());
    if origin != "https://slug-rev-tests.trypreview.online" {
        return Err("public_origin_not_allowlisted".into());
    }
    let url = env::var("TEST_DATABASE_URL").map_err(|_| "isolated_test_database_required")?;
    let database = url::Url::parse(&url).map_err(|_| "invalid_test_database_url")?;
    if !matches!(
        database.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]")
    ) {
        return Err("test_database_must_be_loopback".into());
    }
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(4))
        .connect(&url)
        .await
        .map_err(|_| "database_connect")?;
    let schema = format!("hook_probe_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .map_err(|_| "schema_create")?;
    let search = schema.clone();
    let pool_result = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(4))
        .after_connect(move |connection, _| {
            let search = search.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO {search}"))
                    .execute(&mut *connection)
                    .await?;
                sqlx::query("SET statement_timeout TO '5s'")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await;
    let outcome = if let Ok(pool) = pool_result {
        let result=async {
            sqlx::migrate!("../../migrations").run(&pool).await.map_err(|_|"migrate")?;
            let listener=tokio::net::TcpListener::bind("127.0.0.1:3000").await.map_err(|_|"port3000_unavailable")?;
            let config=Config{origin:origin.clone(),agency_name:"Synthetic probe".into(),operator_username:random_secret(),operator_password_hash:String::new(),invitation_signing_key:random_secret(),secure_cookie:true,voice_api_key:None};
            let callbacks=Arc::new(Mutex::new(Vec::<Value>::new()));let trace=callbacks.clone();let start=Instant::now();
            let app=voice_hook::router(AppState::new(pool.clone(),config)).layer(middleware::from_fn(move |request:Request,next:Next| {let trace=trace.clone();async move {let at=start.elapsed().as_millis();let response:Response=next.run(request).await;trace.lock().unwrap().push(json!({"started_ms":at,"response_ms":start.elapsed().as_millis(),"status":response.status().as_u16()}));response}})).route("/health",get(||async{"synthetic-hook-probe"}));
            let server=tokio::spawn(async move {axum::serve(listener,app).await});
            let probe=timeout(Duration::from_secs(80),async {preflight(&pool,&origin,summary).await?;live(&pool,&origin,summary).await}).await;
            server.abort();let _=server.await;
            summary["callback_requests"]=json!(*callbacks.lock().unwrap());
            if callbacks.lock().unwrap().iter().skip(2).any(|r| r["status"].as_u64().is_none_or(|s|s!=200)) {summary["callback_conflicts_observed"]=json!(true);}
            match probe {Ok(result)=>result,Err(_)=>Err("probe_deadline".into())}
        }.await;
        pool.close().await;
        result
    } else {
        Err("isolated_pool_connect".into())
    };
    summary["isolated_schema_removed"] = json!(
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&admin)
            .await
            .is_ok()
    );
    admin.close().await;
    outcome
}
// A returned transcript alone does not prove that a voice reply was delivered.
fn single_turn_passed(summary: &Value) -> bool {
    summary.get("error").is_none()
        && summary["live"]["answer_admitted"] == true
        && summary["live"]["final_agent_text_matches_permit"] == true
        && summary["live"]["audio_before_new_permit"] == false
        && summary["live"]["reply_audio_chunks"]
            .as_u64()
            .is_some_and(|n| n > 0)
        && summary["live"]["event_counts"]["transcript.user"]
            .as_u64()
            .is_some_and(|n| n >= 1)
        && summary["live"]["completed_answers"] == 1
        && summary["provider_stop_sent"] == true
        && summary["provider_socket_closed"] == true
        && summary["isolated_schema_removed"] == true
        && (summary["stored_agent_mode"] != true || summary["stored_agent_deleted"] == true)
        && summary["callback_requests"]
            .as_array()
            .is_some_and(|requests| {
                requests.len() >= 3 && requests.iter().skip(2).all(|r| r["status"] == 200)
            })
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let mut summary = json!({"synthetic":true,"scope":"single-answer custom hook integration probe","global_g1_passed":false,"global_g2_passed":false});
    if let Err(error) = run(&mut summary).await {
        summary["error"] = json!(error);
    }
    summary["single_turn_passed"] = json!(single_turn_passed(&summary));
    let path = env::var("VOICE_PROBE_SUMMARY")
        .unwrap_or_else(|_| ".local/verification/custom-hook-sept28.json".into());
    if let Some(parent) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(parent).expect("summary directory");
    }
    std::fs::write(path, serde_json::to_vec_pretty(&summary).unwrap())
        .expect("redacted summary write");
    println!("Controlled synthetic hook probe ended; redacted summary written.");
    if summary["single_turn_passed"] != true {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn successful() -> Value {
        json!({"live":{"answer_admitted":true,"final_agent_text_matches_permit":true,
            "audio_before_new_permit":false,"reply_audio_chunks":258,
            "event_counts":{"transcript.user":1},"completed_answers":1},
            "provider_stop_sent":true,"provider_socket_closed":true,"isolated_schema_removed":true,
            "stored_agent_mode":true,"stored_agent_deleted":true,
            "callback_requests":[{"status":401},{"status":200},{"status":200}]})
    }
    #[test]
    fn spoken_single_turn_requires_audio_consistent_callbacks_and_cleanup() {
        let good = successful();
        assert!(single_turn_passed(&good));
        let mut no_audio = good.clone();
        no_audio["live"]["reply_audio_chunks"] = json!(0);
        assert!(!single_turn_passed(&no_audio));
        let mut conflict = good.clone();
        conflict["callback_requests"][2]["status"] = json!(409);
        assert!(!single_turn_passed(&conflict));
        let mut split = good.clone();
        split["live"]["event_counts"]["transcript.user"] = json!(2);
        assert!(single_turn_passed(&split));
        split["live"]["completed_answers"] = json!(2);
        assert!(!single_turn_passed(&split));
        let mut leaked = good;
        leaked["stored_agent_deleted"] = json!(false);
        assert!(!single_turn_passed(&leaked));
    }
}
