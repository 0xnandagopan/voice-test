//! Opt-in native-resume check: abort an opaque TCP proxy, sending neither a
//! WebSocket Close frame nor session.end. TLS validates the real provider host.
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::env;
use tokio::{
    net::{TcpListener, TcpStream},
    time::{Duration, Instant, sleep, timeout},
};
use tokio_tungstenite::{
    client_async_tls,
    tungstenite::{Message, client::IntoClientRequest},
};
use v0_voice::{client::VoiceClient, protocol::ClientEvent};
async fn run() -> Result<Value, String> {
    if env::var("VOICE_LIVE_PROBE").as_deref() != Ok("1") {
        return Err("opt_in_required".into());
    }
    let key = env::var("VOICE_AGENT_API_KEY").map_err(|_| "missing_key")?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| "local_proxy_bind")?;
    let address = listener.local_addr().map_err(|_| "local_proxy_address")?;
    let tunnel = tokio::spawn(async move {
        let (mut input, _) = listener.accept().await?;
        let mut output = TcpStream::connect(("agents.assemblyai.com", 443)).await?;
        tokio::io::copy_bidirectional(&mut input, &mut output).await
    });
    let tcp = TcpStream::connect(address)
        .await
        .map_err(|_| "proxy_connect")?;
    let mut request = "wss://agents.assemblyai.com/v1/ws"
        .into_client_request()
        .map_err(|_| "request")?;
    let mut auth = format!("Bearer {key}")
        .parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()
        .map_err(|_| "invalid_key")?;
    auth.set_sensitive(true);
    request.headers_mut().insert("Authorization", auth);
    let (mut ws, _) = client_async_tls(request, tcp)
        .await
        .map_err(|_| "tls_connect")?;
    ws.send(Message::Text(json!({"type":"session.update","session":{"system_prompt":"This is a synthetic connectivity check. Be concise.","greeting":"Hello.","output":{"voice":"alba","format":{"encoding":"audio/pcm"}}}}).to_string().into())).await.map_err(|_|"configure")?;
    let ready = timeout(Duration::from_secs(12), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(raw))) => {
                    let v: Value = serde_json::from_str(&raw).map_err(|_| "invalid_json")?;
                    if v["type"] == "session.ready" {
                        return Ok::<Value, String>(v);
                    }
                    if v["type"] == "session.error" {
                        return Err(v["code"].as_str().unwrap_or("unknown").to_owned());
                    }
                }
                Some(Ok(_)) => {}
                _ => return Err("closed_before_ready".into()),
            }
        }
    })
    .await
    .map_err(|_| "ready_timeout")??;
    let id = ready["session_id"].as_str().ok_or("missing_id")?.to_owned();
    let started = Instant::now();
    sleep(Duration::from_millis(500)).await;
    let mut upload = serde_json::Value::Null;
    if let Ok(path) = env::var("VOICE_PROBE_PARTIAL_PCM") {
        let source = tokio::fs::read(&path)
            .await
            .map_err(|_| "partial_fixture")?;
        let partial = &source[..source.len().min(24_000 * 2 * 3)];
        tokio::fs::write("/tmp/v0-synthetic-partial.pcm", partial)
            .await
            .map_err(|_| "partial_fixture_write")?;
        let upload_start_ms = started.elapsed().as_millis();
        for chunk in partial.chunks(960) {
            ws.send(Message::Text(
                json!({"type":"input.audio","audio":STANDARD.encode(chunk)})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|_| "partial_upload")?;
            sleep(Duration::from_millis(20)).await;
        }
        upload = json!({"source":"/tmp/v0-synthetic-partial.pcm","source_bytes":partial.len(),"upload_start_ms":upload_start_ms,"upload_end_ms":started.elapsed().as_millis()});
    }
    // Cancelling the TCP pump drops the remote transport without touching TLS/WS.
    tunnel.abort();
    let _ = tunnel.await;
    let cut = Instant::now();
    drop(ws);
    sleep(Duration::from_millis(500)).await;
    let mut resumed = VoiceClient::connect(&key)
        .await
        .map_err(|e| e.to_string())?;
    resumed
        .send(&ClientEvent::Resume {
            session_id: id.clone(),
        })
        .await
        .map_err(|e| e.to_string())?;
    let outcome = timeout(Duration::from_secs(8), async {
        loop {
            match resumed.receive_raw().await {
                Ok(Some(v)) if v["type"] == "session.ready" => return Ok(v["session_id"] == id),
                Ok(Some(v)) if v["type"] == "session.error" => {
                    return Err(v["code"].as_str().unwrap_or("unknown").to_owned());
                }
                Ok(Some(_)) => {}
                _ => return Err("closed".into()),
            }
        }
    })
    .await
    .map_err(|_| "resume_timeout")?;
    let elapsed = cut.elapsed().as_millis();
    let same = outcome.as_ref().is_ok_and(|v| *v);
    let ended = if same {
        resumed.end().await.is_ok()
    } else {
        false
    };
    Ok(
        json!({"session_ids":[id],"loss_kind":"aborted_opaque_tcp_proxy_no_websocket_close","resume_same_id":same,"resume_error":outcome.err(),"reconnect_elapsed_ms":elapsed,"terminal_end_sent":ended,"upload":upload}),
    )
}
#[tokio::main]
async fn main() {
    let r = match timeout(Duration::from_secs(30), run()).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => json!({"error":e}),
        Err(_) => json!({"error":"deadline"}),
    };
    tokio::fs::write(
        env::var("VOICE_PROBE_OUTPUT")
            .unwrap_or_else(|_| "/tmp/v0-abrupt-resume-probe.json".into()),
        serde_json::to_vec_pretty(&r).unwrap(),
    )
    .await
    .unwrap();
    println!("Abrupt-loss probe finished; private metadata saved.");
}
