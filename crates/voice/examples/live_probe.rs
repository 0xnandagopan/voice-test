//! Explicitly opt-in synthetic protocol probe; never run as an ordinary unit test.
//! VOICE_AGENT_API_KEY must be injected server-side. Results contain IDs/event counts
//! only, and are written outside git. No microphone or real customer is involved.
use serde_json::json;
use std::{collections::BTreeMap, env};
use tokio::time::{Duration, Instant, sleep, timeout};
use v0_voice::{
    client::VoiceClient,
    protocol::{ClientEvent, ProviderEvent, fixed_configuration},
};

async fn ready(client: &mut VoiceClient) -> Result<String, String> {
    timeout(Duration::from_secs(15), async {
        loop {
            match client.receive().await.map_err(|e| e.to_string())? {
                Some(ProviderEvent::Ready { session_id }) => return Ok(session_id),
                Some(ProviderEvent::Error { code }) => return Err(format!("provider_code:{code}")),
                None => return Err("closed_before_ready".into()),
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| "ready_timeout".to_string())?
}
async fn connect(key: &str, first: ClientEvent) -> Result<VoiceClient, String> {
    let mut client = VoiceClient::connect(key).await.map_err(|e| e.to_string())?;
    client.send(&first).await.map_err(|e| e.to_string())?;
    Ok(client)
}
async fn run() -> Result<serde_json::Value, String> {
    if env::var("VOICE_LIVE_PROBE").as_deref() != Ok("1") {
        return Err("set VOICE_LIVE_PROBE=1 to authorize this paid synthetic probe".into());
    }
    let key = env::var("VOICE_AGENT_API_KEY").map_err(|_| "missing VOICE_AGENT_API_KEY")?;
    let mut client = connect(
        &key,
        fixed_configuration(
            "Synthetic demonstration: a software agency improved an internal reporting workflow.",
        ),
    )
    .await?;
    let first_id = ready(&mut client).await?;
    // Drain greeting before injecting a synthetic text answer. No audio is uploaded.
    let greeting_deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let event = timeout(
            greeting_deadline.saturating_duration_since(Instant::now()),
            client.receive(),
        )
        .await;
        match event {
            Ok(Ok(Some(ProviderEvent::ReplyDone { .. }))) | Err(_) => break,
            Ok(Ok(None)) => break,
            _ => {}
        }
        if Instant::now() >= greeting_deadline {
            break;
        }
    }
    client.send(&ClientEvent::Conversation {role:"user".into(),content:"This is synthetic test input. Our reports took a while to prepare. I am unsure of the exact time and some reports still need manual checking.".into()}).await.map_err(|e|e.to_string())?;
    client.send(&ClientEvent::Reply {instructions:"Respond to the latest synthetic answer using the required next_step tool before speaking a new question.".into()}).await.map_err(|e|e.to_string())?;
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    let mut pending = None;
    let mut audio_before_tool_result = false;
    let mut sent_tool = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let event = match timeout(
            deadline.saturating_duration_since(Instant::now()),
            client.receive(),
        )
        .await
        {
            Ok(Ok(Some(e))) => e,
            _ => break,
        };
        match event {
            ProviderEvent::ToolCall { call_id, name, .. } => {
                *counts.entry("tool_call").or_default() += 1;
                if name == "next_step" {
                    pending = Some(call_id);
                }
            }
            ProviderEvent::Audio { .. } => {
                *counts.entry("audio_chunk").or_default() += 1;
                if !sent_tool {
                    audio_before_tool_result = true;
                }
            }
            ProviderEvent::ReplyDone { reply_id, .. } => {
                *counts.entry("reply_done").or_default() += 1;
                if let Some(call_id) = pending.take() {
                    if reply_id == format!("fc-{call_id}") {
                        client.send(&ClientEvent::ToolResult {call_id,result:json!({"step":"problem","followups":[1,0,0],"may_ask":true,"instruction":"Ask one neutral clarification; accept uncertainty."}).to_string(),is_error:false}).await.map_err(|e|e.to_string())?;
                        sent_tool = true;
                    }
                } else if sent_tool {
                    break;
                }
            }
            ProviderEvent::Error { code } => {
                let _ = client.end().await;
                return Err(format!("provider_code:{code}"));
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    drop(client);
    sleep(Duration::from_secs(2)).await;
    let mut resumed = connect(
        &key,
        ClientEvent::Resume {
            session_id: first_id.clone(),
        },
    )
    .await?;
    let resume_result = ready(&mut resumed).await;
    let short_resume_same_id = resume_result.as_ref().is_ok_and(|id| id == &first_id);
    let terminal_end_sent = resumed.send(&ClientEvent::End).await.is_ok();
    let terminal_end_observed = timeout(Duration::from_secs(5), async {
        loop {
            match resumed.receive().await {
                Ok(Some(ProviderEvent::Ended)) => return true,
                Ok(None) | Err(_) => return false,
                _ => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    drop(resumed);
    let mut second = connect(
        &key,
        fixed_configuration("Synthetic recovery probe. No customer data."),
    )
    .await?;
    let second_id = ready(&mut second).await?;
    drop(second);
    sleep(Duration::from_secs(32)).await;
    let mut expired = connect(
        &key,
        ClientEvent::Resume {
            session_id: second_id.clone(),
        },
    )
    .await?;
    let long_resume_result = ready(&mut expired).await;
    if long_resume_result.is_ok() {
        let _ = expired.end().await;
    }
    Ok(
        json!({"synthetic":true,"audio_uploaded":false,"session_ids":[first_id,second_id],"ready":true,"event_counts":counts,"tool_result_sent":sent_tool,"audio_before_tool_result":audio_before_tool_result,"short_resume_same_id":short_resume_same_id,"short_resume_error":resume_result.err(),"terminal_end_sent":terminal_end_sent,"terminal_end_observed":terminal_end_observed,"long_resume_error":long_resume_result.err(),"question_enforcement_proven":false,"microphone_quality_proven":false}),
    )
}
#[tokio::main]
async fn main() {
    // Dropping a socket after an outer timeout permits at most the provider's
    // documented 30s grace. Individual checks are budgeted under 90 seconds.
    let result = timeout(Duration::from_secs(110), run()).await;
    let report = match result {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => json!({"error":error}),
        Err(_) => json!({"error":"probe_deadline"}),
    };
    let path = env::var("VOICE_PROBE_OUTPUT").unwrap_or_else(|_| "/tmp/v0-voice-probe.json".into());
    tokio::fs::write(&path, serde_json::to_vec_pretty(&report).unwrap())
        .await
        .expect("write probe result");
    println!("Synthetic probe result saved to local file; no credentials or transcripts logged.");
}
