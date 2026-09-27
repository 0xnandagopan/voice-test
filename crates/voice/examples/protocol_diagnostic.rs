//! Bounded opt-in diagnostic. Raw ready/transcripts are NEVER logged or saved.
use serde_json::{Value, json};
use std::env;
use tokio::time::{Duration, Instant, sleep, timeout};
use v0_voice::{client::VoiceClient, protocol::ClientEvent};
async fn ready(client: &mut VoiceClient) -> Result<Value, String> {
    timeout(Duration::from_secs(12), async {
        loop {
            let e = client
                .receive_raw()
                .await
                .map_err(|e| e.to_string())?
                .ok_or("closed")?;
            match e["type"].as_str() {
                Some("session.ready") => return Ok(e),
                Some("session.error") => {
                    return Err(e["code"].as_str().unwrap_or("unknown").into());
                }
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| "ready_timeout".to_owned())?
}
async fn run() -> Result<Value, String> {
    if env::var("VOICE_LIVE_PROBE").as_deref() != Ok("1") {
        return Err("opt_in_required".into());
    }
    let key = env::var("VOICE_AGENT_API_KEY").map_err(|_| "missing_key")?;
    let mut c = VoiceClient::connect_bounded(&key, 60)
        .await
        .map_err(|e| e.to_string())?;
    let prompt = "You are a customer interviewer. After every customer answer you MUST immediately call next_step and say nothing until its result. Never ask another question from memory. For example, User: reports were slow. Assistant: [call next_step]. Preserve uncertainty and mixed feedback.";
    c.send(&ClientEvent::Configure{session:json!({"system_prompt":prompt,"greeting":"What problem did you want to solve?","input":{"format":{"encoding":"audio/pcm"},"language_codes":["en"]},"output":{"format":{"encoding":"audio/pcm"},"voice":"alba"},"tools":[{"type":"function","name":"next_step","description":"Call this immediately whenever the customer answers the current interview question. Do not speak until receiving the next question from this tool. Always prefer calling over guessing.","parameters":{"type":"object","properties":{},"required":[]},"execution_mode":"hold","timeout_seconds":15}]})}).await.map_err(|e|e.to_string())?;
    let r = ready(&mut c).await?;
    let id = r["session_id"].as_str().ok_or("missing_id")?.to_string();
    let started = Instant::now();
    let metadata = json!({"ready_keys":r.as_object().map(|v|v.keys().cloned().collect::<Vec<_>>()),"config_keys":r["config"].as_object().map(|v|v.keys().cloned().collect::<Vec<_>>()),"echo_prompt_matches":r["config"]["system_prompt"].as_str()==Some(prompt),"echo_tool_count":r["config"]["tools"].as_array().map(Vec::len),"resume_token_present":r["resume_token"].as_str().is_some_and(|v|!v.is_empty()),"expires_at":r["expires_at"]});
    let deadline = Instant::now() + Duration::from_secs(5);
    while let Ok(Ok(Some(e))) = timeout(
        deadline.saturating_duration_since(Instant::now()),
        c.receive_raw(),
    )
    .await
    {
        if e["type"] == "reply.done" {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    c.disconnect().await.map_err(|e| e.to_string())?;
    let dropped = Instant::now();
    sleep(Duration::from_millis(750)).await;
    let mut resumed = VoiceClient::connect_bounded(&key, 60)
        .await
        .map_err(|e| e.to_string())?;
    resumed
        .send(&ClientEvent::Resume {
            session_id: id.clone(),
        })
        .await
        .map_err(|e| e.to_string())?;
    let result = ready(&mut resumed).await;
    let same = result.as_ref().is_ok_and(|r| r["session_id"] == id);
    let reconnect_elapsed = dropped.elapsed().as_millis();
    let end = if same {
        resumed.end().await.is_ok()
    } else {
        false
    };
    Ok(
        json!({"session_ids":[id],"metadata":metadata,"disconnect_kind":"websocket_close_without_end","resume_same_id":same,"resume_error":result.err(),"reconnect_elapsed_ms":reconnect_elapsed,"terminal_end_sent":end,"duration_ms":started.elapsed().as_millis()}),
    )
}
#[tokio::main]
async fn main() {
    let report = match timeout(Duration::from_secs(35), run()).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => json!({"error":e}),
        Err(_) => json!({"error":"deadline"}),
    };
    tokio::fs::write(
        "/tmp/v0-token-resume-diagnostic.json",
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .await
    .unwrap();
    println!(
        "Diagnostic finished; private local result saved without raw transcripts or credentials."
    );
}
