//! Explicitly gated browser/provider relay. All credentials and question permits
//! stay server-side; provider callbacks cannot authenticate a customer command.
use crate::{auth::hash_secret, error::ApiError, leases, progress, voice_hook};
use axum::extract::ws::{Message, WebSocket};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::SinkExt;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;
use tokio::time::{Duration, Instant, interval, timeout};
use uuid::Uuid;
use v0_domain::workflow::{AttemptEndReason, VoiceAction, VoiceControlRequest};
use v0_voice::{
    client::VoiceClient,
    protocol::{ClientEvent, ProviderEvent},
};

pub struct RelayConfig {
    pub api_key: String,
    pub public_origin: String,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum BrowserEvent {
    Audio {
        audio: String,
    },
    Control {
        request_id: Uuid,
        action: VoiceAction,
        expected_revision: i64,
        expected_progress_revision: i64,
    },
    Stop {
        request_id: Uuid,
    },
}

/// Tracks provider reply lifecycle independently of browser playback. A control
/// waits for the superseded reply to drain before requesting its replacement.
struct ReplyGate {
    identity: Option<String>,
    active: bool,
    requested: bool,
    suppressed: bool,
    pending_control: bool,
}
impl ReplyGate {
    fn greeting() -> Self {
        Self {
            identity: None,
            active: false,
            requested: true,
            suppressed: false,
            pending_control: false,
        }
    }
    fn control(&mut self) {
        self.suppressed = true;
        self.pending_control = true;
    }
    fn requested_replacement(&mut self) {
        self.pending_control = false;
        self.requested = true;
    }
    fn final_answer(&mut self) {
        // Providers can finalize another fragment of the same answer after
        // reply.started. That does not promise a second reply.started event.
        // Keep waiting only when no reply is currently serving that answer.
        if !self.active {
            self.requested = true;
        }
    }
    fn started(&mut self, id: String) {
        self.identity = Some(id);
        self.active = true;
        self.requested = false;
        self.suppressed = self.pending_control;
    }
    fn done(&mut self, id: &str, interrupted: bool) -> bool {
        if self.identity.as_deref() != Some(id) {
            return false;
        }
        self.active = false;
        if interrupted {
            self.suppressed = true;
        }
        true
    }
}

async fn emit(socket: &mut WebSocket, value: Value) -> Result<(), ApiError> {
    timeout(
        Duration::from_secs(2),
        socket.send(Message::Text(value.to_string().into())),
    )
    .await
    .map_err(|_| ApiError::conflict())?
    .map_err(|_| ApiError::conflict())
}
fn unavailable() -> ApiError {
    ApiError(
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "voice_unavailable",
        "The interview connection is unavailable. Recording recovery may still be pending.",
    )
}
async fn current_access(
    pool: &PgPool,
    token: &str,
    id: Uuid,
    lease: &leases::InterviewLease,
) -> Result<(), ApiError> {
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions s JOIN interviews i ON i.id=s.interview_id WHERE s.token_hash=$1 AND s.role='customer' AND s.interview_id=$2 AND s.expires_at>clock_timestamp() AND i.expires_at>clock_timestamp() AND i.deleted_at IS NULL AND i.consented_at IS NOT NULL AND i.state='interviewing' AND i.lease_id=$3 AND i.lease_generation=$4 AND i.lease_expires_at>clock_timestamp() AND i.time_consumed_seconds + EXTRACT(EPOCH FROM (clock_timestamp()-i.active_since)) < 360)")
        .bind(hash_secret(token)).bind(id).bind(lease.lease_id).bind(lease.generation).fetch_one(pool).await?;
    if !valid {
        return Err(ApiError::unauthorized());
    }
    Ok(())
}
async fn state(
    pool: &PgPool,
    id: Uuid,
    kind: &str,
    remaining: i32,
    quiet: bool,
) -> Result<Value, ApiError> {
    let row=sqlx::query("SELECT revision,progress_revision,topic_index,followup_counts,completed_answers,incomplete_turn FROM interviews WHERE id=$1").bind(id).fetch_one(pool).await?;
    Ok(
        json!({"type":kind,"revision":row.get::<i64,_>("revision"),"progress_revision":row.get::<i64,_>("progress_revision"),"remaining_seconds":remaining,"topic":row.get::<i32,_>("topic_index"),"followups":row.get::<Value,_>("followup_counts"),"completed_answers":row.get::<i32,_>("completed_answers"),"can_finish":quiet && row.get::<i32,_>("topic_index")>=3 && row.get::<Option<String>,_>("incomplete_turn").is_none()}),
    )
}
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}
fn http() -> Result<reqwest::Client, ApiError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| unavailable())
}
async fn create_agent(
    pool: &PgPool,
    attempt: Uuid,
    config: &RelayConfig,
    greeting: &str,
    secret: &str,
) -> Result<String, ApiError> {
    let origin = url::Url::parse(&config.public_origin).map_err(|_| unavailable())?;
    if origin.scheme() != "https"
        || origin.host_str().is_none()
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.query().is_some()
        || origin.fragment().is_some()
        || origin.path() != "/"
    {
        return Err(unavailable());
    }
    let name = format!("v0-controlled-{}", attempt);
    // Retain a lookup handle before the external mutation; an ambiguous timeout
    // must not erase the only way to find a provider-side agent for cleanup.
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE provider_attempts SET provider_agent_name=$2 WHERE id=$1")
        .bind(attempt)
        .bind(&name)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key,available_at) SELECT $1,interview_id,'delete_provider_agent',$3,$4,clock_timestamp()+interval '7 minutes' FROM provider_attempts WHERE id=$2 ON CONFLICT(dedupe_key) DO NOTHING")
        .bind(Uuid::new_v4()).bind(attempt).bind(json!({"attempt_id":attempt})).bind(format!("agent-cleanup:{attempt}")).execute(&mut *tx).await?;
    tx.commit().await?;
    let response=http()?.post("https://agents.assemblyai.com/v1/agents").header("Authorization",&config.api_key)
        .json(&json!({"name":name,"system_prompt":"Ask only the exact question supplied by the custom completion service. Never grant approval or publication.","voice":{"voice_id":"alba"},"input":{"format":{"encoding":"audio/pcm"},"language_codes":["en"]},"output":{"format":{"encoding":"audio/pcm"},"voice":"alba"},"greeting":greeting,"llm":[{"base_url":format!("{}/voice-hook/{attempt}",config.public_origin.trim_end_matches('/')),"model":"v0-bounded","api_key":secret}]}))
        .send().await.map_err(|_|unavailable())?;
    if !response.status().is_success() {
        return Err(unavailable());
    }
    let body: Value = response.json().await.map_err(|_| unavailable())?;
    let id = body["id"]
        .as_str()
        .filter(|id| safe_id(id))
        .ok_or_else(unavailable)?
        .to_string();
    let mapped=sqlx::query("UPDATE provider_attempts p SET provider_agent_id=$2 FROM interviews i WHERE p.id=$1 AND i.id=p.interview_id AND p.provider_agent_name=$3 AND p.state='connecting' AND p.ended_at IS NULL AND i.lease_generation=p.lease_generation AND i.lease_expires_at>clock_timestamp() AND i.expires_at>clock_timestamp() AND i.deleted_at IS NULL AND i.state='interviewing'")
        .bind(attempt).bind(&id).bind(&name).execute(pool).await?;
    if mapped.rows_affected() != 1 {
        // A delayed create response is cleanup work, never authority to reopen
        // an ended/revoked attempt or recreate its cleared active-agent handle.
        sqlx::query("INSERT INTO jobs(id,interview_id,kind,payload,dedupe_key) SELECT $1,interview_id,'delete_provider_agent',$3,$4 FROM provider_attempts WHERE id=$2 ON CONFLICT(dedupe_key) DO NOTHING")
            .bind(Uuid::new_v4()).bind(attempt).bind(json!({"attempt_id":attempt,"provider_agent_id":id})).bind(format!("agent-cleanup-late:{attempt}:{id}")).execute(pool).await?;
        let _ = http()?
            .delete(format!("https://agents.assemblyai.com/v1/agents/{id}"))
            .header("Authorization", &config.api_key)
            .send()
            .await;
        return Err(unavailable());
    }
    Ok(id)
}
async fn cleanup_agent(pool: &PgPool, attempt: Uuid, config: &RelayConfig) {
    let id: Option<String> =
        sqlx::query_scalar("SELECT provider_agent_id FROM provider_attempts WHERE id=$1")
            .bind(attempt)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .flatten();
    if let Some(id) = id.filter(|id| safe_id(id)) {
        let deleted = match http() {
            Ok(client) => client
                .delete(format!("https://agents.assemblyai.com/v1/agents/{id}"))
                .header("Authorization", &config.api_key)
                .send()
                .await
                .ok()
                .is_some_and(|r| {
                    r.status().is_success() || r.status() == reqwest::StatusCode::NOT_FOUND
                }),
            Err(_) => false,
        };
        if deleted {
            let _=sqlx::query("UPDATE provider_attempts SET provider_agent_id=NULL,provider_agent_name=NULL WHERE id=$1").bind(attempt).execute(pool).await;
        }
    }
}

/// Called only after the HTTP adapter validates scoped customer access, Origin,
/// and explicit VOICE_TEST_ENABLED. A second socket cannot obtain the same lease.
pub async fn run(
    mut socket: WebSocket,
    pool: PgPool,
    customer_token: String,
    interview: Uuid,
    expected_revision: i64,
    config: RelayConfig,
) {
    let lease = match leases::acquire_customer(&pool, &customer_token, interview, expected_revision)
        .await
    {
        Ok(lease) => lease,
        Err(error) => {
            let _ = emit(&mut socket, json!({"type":"error","code":error.1})).await;
            return;
        }
    };
    let attempt = Uuid::new_v4();
    let mut provider = None;
    let mut incomplete = BTreeMap::<String, Uuid>::new();
    let mut reason = AttemptEndReason::TransportLost;
    let deadline = Instant::now() + Duration::from_secs(lease.remaining_seconds as u64);
    let outcome:Result<(),ApiError>=async {
        // Reserve before startup becomes cancellable: even an immediate Stop
        // must have a durable attempt identity to finalize.
        progress::prepare_attempt(&pool,interview,lease.lease_id,lease.generation,attempt).await?;
        let (ready,mut remaining)=tokio::select! {
            result=async {
        current_access(&pool,&customer_token,interview,&lease).await?;
        let secret=voice_hook::bind(&pool,interview,attempt,lease.lease_id,lease.generation).await?;
        let greeting=voice_hook::configured_greeting(&pool,attempt,&secret).await?;
        let agent=create_agent(&pool,attempt,&config,&greeting,&secret).await?;
        // Setup counts against the same cumulative allowance and lease.
        let remaining=leases::heartbeat(&pool,interview,lease.lease_id,lease.generation).await?;
        if remaining<=0 {return Err(unavailable());}
        let client=VoiceClient::connect_bounded(&config.api_key,remaining.clamp(60,360) as u32).await.map_err(|_|unavailable())?;
        provider=Some(client);
        let client=provider.as_mut().unwrap();
        client.send(&ClientEvent::Configure{session:json!({"agent_id":agent})}).await.map_err(|_|unavailable())?;
        let session=timeout(Duration::from_secs(12),async {
            loop {
                match client.receive().await.map_err(|_|unavailable())? {
                    Some(ProviderEvent::Ready{session_id})=>break Ok(session_id),
                    Some(ProviderEvent::Error{..})|Some(ProviderEvent::Ended)|None=>break Err(unavailable()),
                    _=>{}
                }
            }
        }).await.map_err(|_|unavailable())??;
        progress::map_attempt(&pool,interview,lease.lease_id,lease.generation,attempt,&session).await?;
        let remaining=leases::heartbeat(&pool,interview,lease.lease_id,lease.generation).await?;
        let mut ready=state(&pool,interview,"ready",remaining,false).await?;
        ready["attempt_id"]=json!(attempt);ready["lease_id"]=json!(lease.lease_id);ready["lease_generation"]=json!(lease.generation);
        Ok::<_,ApiError>((ready,remaining))
            }=>result?,
            incoming=socket.recv()=>{
                if let Some(Ok(Message::Text(text)))=incoming
                    && matches!(serde_json::from_str::<BrowserEvent>(&text),Ok(BrowserEvent::Stop{..})) {reason=AttemptEndReason::ExplicitStop;}
                return Ok(());
            }
            _=tokio::time::sleep_until(deadline)=>{reason=AttemptEndReason::BudgetExhausted;return Ok(());}
        };
        let client=provider.as_mut().ok_or_else(unavailable)?;
        emit(&mut socket,ready).await?;
        let mut heartbeat=interval(Duration::from_secs(10));
        let mut guard=interval(Duration::from_secs(1));
        let mut speech_question=None;
        let mut playback=ReplyGate::greeting();
        let mut speech_active=false;
        let mut awaiting_final=false;
        let mut reply_delivered=false;
        let mut offered_at_audio=None;
        let mut agent_captions=BTreeMap::<String,String>::new();
        let mut control_receipts=BTreeMap::<Uuid,String>::new();
        let mut received_bytes=0usize;
        let mut rate_window=Instant::now();
        loop {
            tokio::select! {
                _=tokio::time::sleep_until(deadline)=>{reason=AttemptEndReason::BudgetExhausted;break;}
                _=guard.tick()=>{current_access(&pool,&customer_token,interview,&lease).await?;}
                _=heartbeat.tick()=>{
                    remaining=leases::heartbeat(&pool,interview,lease.lease_id,lease.generation).await?;
                    if remaining<=0 {reason=AttemptEndReason::BudgetExhausted;break;}
                    emit(&mut socket,state(&pool,interview,"state",remaining,!speech_active && !awaiting_final && incomplete.is_empty() && !playback.active && !playback.requested && !playback.pending_control).await?).await?;
                }
                browser=socket.recv()=>{
                    let Some(Ok(message))=browser else {break;};
                    let text=match message {Message::Text(text)=>text,Message::Close(_)=>break,Message::Ping(_)|Message::Pong(_)=>continue,_=>return Err(ApiError::invalid("Unsupported audio transport."))};
                    if text.len()>65536 {return Err(ApiError::invalid("Audio frame is too large."));}
                    let event:BrowserEvent=serde_json::from_str(&text).map_err(|_|ApiError::invalid("Invalid voice command."))?;
                    current_access(&pool,&customer_token,interview,&lease).await?;
                    match event {
                        BrowserEvent::Stop{request_id}=>{let _=request_id;reason=AttemptEndReason::ExplicitStop;break;}
                        BrowserEvent::Audio{audio}=>{
                            let bytes=STANDARD.decode(&audio).map_err(|_|ApiError::invalid("Invalid audio frame."))?;
                            if bytes.is_empty()||bytes.len()>48000||bytes.len()%2!=0 {return Err(ApiError::invalid("Invalid PCM frame."));}
                            if rate_window.elapsed()>=Duration::from_secs(1){received_bytes=0;rate_window=Instant::now();}
                            received_bytes+=bytes.len();
                            if received_bytes>192000 {return Err(ApiError::invalid("Audio input exceeded its rate bound."));}
                            client.send(&ClientEvent::Audio{audio}).await.map_err(|_|unavailable())?;
                        }
                        BrowserEvent::Control{request_id,action,expected_revision,expected_progress_revision}=>{
                            if matches!(action,VoiceAction::Finish) {
                                let request=VoiceControlRequest{interview_id:interview,request_id,expected_revision,expected_progress_revision,lease_id:lease.lease_id,lease_generation:lease.generation,action};
                                let result=voice_hook::validate_finish(&pool,&customer_token,&request).await;
                                if result.is_ok() && !speech_active && !awaiting_final && incomplete.is_empty() && !playback.active && !playback.requested && !playback.pending_control {reason=AttemptEndReason::ExplicitFinish;break;}
                                let code=result.err().map(|e|e.1).unwrap_or("finish_not_ready");
                                emit(&mut socket,json!({"type":"control_rejected","code":code})).await?;
                                emit(&mut socket,state(&pool,interview,"state",remaining,!speech_active && !awaiting_final && incomplete.is_empty() && !playback.active && !playback.requested && !playback.pending_control).await?).await?;continue;
                            }
                            if !matches!(action,VoiceAction::Skip|VoiceAction::Repeat){emit(&mut socket,json!({"type":"control_rejected","code":"unsupported_control"})).await?;emit(&mut socket,state(&pool,interview,"state",remaining,false).await?).await?;continue;}
                            let meaning=serde_json::to_string(&(expected_revision,expected_progress_revision,&action)).map_err(|_|ApiError::conflict())?;
                            if let Some(prior)=control_receipts.get(&request_id) {
                                if prior!=&meaning {emit(&mut socket,json!({"type":"control_rejected","code":"conflict"})).await?;}
                                {emit(&mut socket,state(&pool,interview,"state",remaining,!speech_active && !awaiting_final && incomplete.is_empty() && !playback.active && !playback.requested && !playback.pending_control).await?).await?;}
                                continue;
                            }
                            if control_receipts.len()>=100 {return Err(ApiError::conflict());}
                            match voice_hook::control_question(&pool,&customer_token,VoiceControlRequest{interview_id:interview,request_id,expected_revision,expected_progress_revision,lease_id:lease.lease_id,lease_generation:lease.generation,action}).await {
                                Ok(_)=>{control_receipts.insert(request_id,meaning);playback.control();
                                    // Keep speech_question pinned across Skip/Repeat. Its final
                                    // remains history of the old question, never a new answer.
                                    emit(&mut socket,json!({"type":"clear_playback"})).await?;
                                    if !playback.active && !playback.requested && !speech_active && !awaiting_final && incomplete.is_empty() {
                                        client.send(&ClientEvent::Reply{instructions:"Speak only the exact current permitted question.".into()}).await.map_err(|_|unavailable())?;
                                        playback.requested_replacement();
                                    }
                                    emit(&mut socket,state(&pool,interview,"state",remaining,!speech_active && !awaiting_final && incomplete.is_empty() && !playback.active && !playback.requested && !playback.pending_control).await?).await?;}
                                Err(error)=>{emit(&mut socket,json!({"type":"control_rejected","code":error.1})).await?;emit(&mut socket,state(&pool,interview,"state",remaining,!speech_active && !awaiting_final && incomplete.is_empty() && !playback.active && !playback.requested && !playback.pending_control).await?).await?;}
                            }
                        }
                    }
                }
                event=client.receive()=>{
                    match event.map_err(|_|unavailable())? {
                        Some(ProviderEvent::SpeechStarted)=>{speech_active=true;awaiting_final=true;speech_question=voice_hook::questions(&pool,attempt,lease.lease_id,lease.generation).await?.1;emit(&mut socket,state(&pool,interview,"state",remaining,false).await?).await?;}
                        Some(ProviderEvent::SpeechStopped)=>{speech_active=false;}
                        Some(ProviderEvent::UserDelta{item_id,text})=>{
                            awaiting_final=true;
                            if !incomplete.contains_key(&item_id) {voice_hook::mark_incomplete(&pool,attempt,&item_id,lease.lease_id,lease.generation).await?;}
                            if let Some(question)=speech_question {incomplete.insert(item_id.clone(),question);}
                            emit(&mut socket,json!({"type":"caption","speaker":"customer","item_id":item_id,"text":text,"final":false})).await?;
                        }
                        Some(ProviderEvent::UserFinal{item_id,text})=>{
                            let question=incomplete.get(&item_id).copied().or(speech_question).ok_or_else(ApiError::conflict)?;
                            voice_hook::authorize_answer(&pool,attempt,question,&item_id,&text,lease.lease_id,lease.generation).await?;
                            incomplete.remove(&item_id);awaiting_final= !incomplete.is_empty();playback.final_answer();
                            emit(&mut socket,json!({"type":"caption","speaker":"customer","item_id":item_id,"text":text,"final":true})).await?;
                        }
                        Some(ProviderEvent::ReplyStarted{reply_id,..})=>{playback.started(reply_id);reply_delivered=false;offered_at_audio=None;if !playback.suppressed {emit(&mut socket,json!({"type":"reply_started"})).await?;}emit(&mut socket,state(&pool,interview,"state",remaining,false).await?).await?;}
                        Some(ProviderEvent::Audio{data})=>{
                            if playback.suppressed {continue;}
                            if !playback.active{return Err(ApiError::conflict());}
                            if !reply_delivered {
                                let permit=voice_hook::questions(&pool,attempt,lease.lease_id,lease.generation).await?.0;
                                voice_hook::mark_delivered(&pool,attempt,permit,lease.lease_id,lease.generation).await?;
                                offered_at_audio=Some(permit);reply_delivered=true;
                                emit(&mut socket,state(&pool,interview,"state",remaining,!speech_active && !awaiting_final && incomplete.is_empty() && !playback.active && !playback.requested && !playback.pending_control).await?).await?;
                            }
                            emit(&mut socket,json!({"type":"audio","audio":data})).await?;
                        }
                        Some(ProviderEvent::AgentDelta{item_id,reply_id,delta})=>{
                            if playback.suppressed || playback.identity.as_deref()!=Some(reply_id.as_str()){continue;}
                            let text=agent_captions.entry(item_id.clone()).or_default();if !text.is_empty(){text.push(' ');}text.push_str(&delta);
                            emit(&mut socket,json!({"type":"caption","speaker":"interviewer","item_id":item_id,"text":text,"final":false})).await?;
                        }
                        Some(ProviderEvent::AgentFinal{item_id,text,interrupted,reply_id})=>{
                            if playback.identity.as_deref()!=Some(reply_id.as_str()){continue;}
                            if !interrupted && !playback.suppressed {
                                let permit=offered_at_audio.ok_or_else(ApiError::conflict)?;
                                let plan:Value=sqlx::query_scalar("SELECT plan FROM question_permits WHERE id=$1 AND attempt_id=$2").bind(permit).bind(attempt).fetch_one(&pool).await?;
                                let plan:v0_voice::pre_speech::QuestionPlan=serde_json::from_value(plan).map_err(|_|ApiError::conflict())?;
                                let committed=v0_voice::pre_speech::CommittedQuestion::after_commit(permit.to_string(),plan).map_err(|_|ApiError::conflict())?;
                                if v0_voice::history::words(&text)!=v0_voice::history::words(committed.text()){return Err(ApiError::conflict());}
                            }
                            if interrupted {playback.suppressed=true;emit(&mut socket,json!({"type":"clear_playback"})).await?;}
                            agent_captions.remove(&item_id);
                            if !playback.suppressed {emit(&mut socket,json!({"type":"caption","speaker":"interviewer","item_id":item_id,"text":text,"final":true})).await?;}
                        }
                        Some(ProviderEvent::ReplyDone{status,reply_id})=>{
                            if !playback.done(&reply_id,status=="interrupted"){continue;}
                            if status=="interrupted"{playback.suppressed=true;emit(&mut socket,json!({"type":"clear_playback"})).await?;}
                            // The documented reply lifecycle must drain before a
                            // replacement is requested. Never unsuppress a late
                            // reply.started while a control is still waiting.
                            if playback.pending_control && !speech_active && !awaiting_final && incomplete.is_empty() {
                                client.send(&ClientEvent::Reply{instructions:"Speak only the exact current permitted question.".into()}).await.map_err(|_|unavailable())?;
                                playback.requested_replacement();
                            }
                            emit(&mut socket,state(&pool,interview,"state",remaining,!speech_active && !awaiting_final && incomplete.is_empty() && !playback.active && !playback.requested && !playback.pending_control).await?).await?;
                        }
                        Some(ProviderEvent::Ended)|None=>break,
                        Some(ProviderEvent::Error{..})=>return Err(unavailable()),
                        _=>{}
                    }
                }
            }
        }
        Ok(())
    }.await;
    // The database clock can reach the allowance a few milliseconds before
    // the socket's monotonic timer. Preserve the truthful reason when a guard
    // or heartbeat wins that race, without touching a later lease generation.
    if matches!(reason, AttemptEndReason::TransportLost) {
        let exhausted:Result<bool,_>=sqlx::query_scalar("SELECT time_consumed_seconds + COALESCE(EXTRACT(EPOCH FROM(clock_timestamp()-active_since)),0)>=360 FROM interviews WHERE id=$1 AND lease_id=$2 AND lease_generation=$3")
            .bind(interview).bind(lease.lease_id).bind(lease.generation).fetch_one(&pool).await;
        if matches!(exhausted, Ok(true)) {
            reason = AttemptEndReason::BudgetExhausted;
        }
    }
    // Explicit terminal cleanup even after local/provider failures; this relay
    // uses recording-backed recovery, not an unproven native-resume assumption.
    let _ = emit(&mut socket, json!({"type":"clear_playback"})).await;
    if let Some(client) = provider {
        let _ = timeout(Duration::from_secs(3), client.end()).await;
    }
    let end = progress::finalize_relay(
        &pool,
        interview,
        lease.lease_id,
        lease.generation,
        attempt,
        reason,
        incomplete.into_keys().collect(),
    )
    .await;
    if end.is_err() {
        // If preparation failed before the attempt existed, release only this
        // exact lease. Never mutate a later connection's ownership.
        let exists: Result<bool, _> = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM provider_attempts WHERE id=$1 AND interview_id=$2)",
        )
        .bind(attempt)
        .bind(interview)
        .fetch_one(&pool)
        .await;
        if matches!(exists, Ok(false)) {
            let _ = leases::release(&pool, interview, lease.lease_id, lease.generation).await;
        }
    }
    // A requested Finish is not a durable completion acknowledgement.
    let reported_reason = if end.is_ok() {
        reason
    } else {
        AttemptEndReason::TransportLost
    };
    let _=emit(&mut socket,json!({"type":"ended","reason":reported_reason,"recovery_required":true,"recovery_pending":end.is_ok()})).await;
    if let Err(error) = outcome {
        let _ = emit(&mut socket, json!({"type":"error","code":error.1})).await;
    }
    cleanup_agent(&pool, attempt, &config).await;
    let _ = socket.close().await;
}

#[cfg(test)]
mod lifecycle_tests {
    use super::ReplyGate;
    #[test]
    fn fragmented_final_after_reply_started_does_not_wait_for_a_second_reply() {
        let mut gate = ReplyGate::greeting();
        gate.started("greeting".into());
        assert!(gate.done("greeting", false));
        gate.final_answer(); // Recorded first final at 2362 ms.
        assert!(gate.requested);
        gate.started("answer-reply".into()); // Recorded at 2369 ms.
        gate.final_answer(); // Another final at 7962 ms, same answer.
        assert!(gate.active && !gate.requested);
        assert!(gate.done("answer-reply", false)); // 11983 ms, no second start.
        assert!(!gate.active && !gate.requested);
        // The next distinct answer after reply completion must still wait.
        gate.final_answer();
        assert!(gate.requested);
    }
    #[test]
    fn skip_during_pending_old_reply_suppresses_until_it_drains() {
        let mut gate = ReplyGate::greeting();
        gate.control(); // Skip arrives before old greeting's reply.started.
        gate.started("old-greeting".into());
        assert!(gate.active && gate.suppressed && gate.pending_control);
        assert!(gate.done("old-greeting", false));
        assert!(gate.suppressed && gate.pending_control && !gate.active);
        gate.requested_replacement();
        assert!(gate.suppressed && gate.requested); // Late old audio stays muted.
        gate.started("replacement".into());
        assert!(gate.active && !gate.suppressed && !gate.pending_control);
        assert!(!gate.done("old-greeting", true)); // Delayed old terminal event.
        assert!(gate.active && !gate.suppressed);
        assert!(gate.done("replacement", false));
        assert!(!gate.active && !gate.requested);
    }
    #[test]
    fn repeat_and_confirmed_interruption_clear_only_the_active_reply() {
        let mut gate = ReplyGate::greeting();
        gate.started("question".into());
        gate.control();
        assert!(gate.suppressed);
        assert!(gate.done("question", true));
        gate.requested_replacement();
        gate.started("repeat".into());
        assert!(!gate.suppressed);
        assert!(gate.done("repeat", true));
        assert!(gate.suppressed && !gate.active);
        // A natural successor after semantic interruption is audible.
        gate.started("after-answer".into());
        assert!(!gate.suppressed && gate.active);
    }
}
