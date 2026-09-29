use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use v0_evidence::stt::{AssemblyAiTranscriber, TranscriptStatus};
async fn server(
    replies: Vec<(u16, String)>,
) -> (AssemblyAiTranscriber, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let mut requests = vec![];
        for (status, body) in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let n = socket.read(&mut buffer).await.unwrap();
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
                assert!(n > 0);
            }
            requests.push(String::from_utf8_lossy(&bytes).into_owned());
            let response = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
        requests
    });
    (
        AssemblyAiTranscriber::with_endpoint(
            "fixture-key".into(),
            format!("http://{addr}/").parse().unwrap(),
        )
        .unwrap(),
        handle,
    )
}
#[tokio::test]
async fn resumable_upload_submit_poll_delete_use_raw_auth_without_automatic_resubmission() {
    let (client,server)=server(vec![(200,json!({"upload_url":"https://cdn.assemblyai.com/upload/test"}).to_string()),(200,json!({"id":"transcript-1","status":"queued"}).to_string()),(200,json!({"id":"transcript-1","status":"processing"}).to_string()),(200,json!({"id":"transcript-1","status":"completed","text":"Synthetic","audio_duration":1.0,"words":[{"text":"Synthetic","start":0,"end":700,"confidence":0.9}]}).to_string()),(200,"{}".into())]).await;
    let mut wav = vec![0; 44];
    wav[..4].copy_from_slice(b"RIFF");
    wav[8..12].copy_from_slice(b"WAVE");
    wav[22..24].copy_from_slice(&1u16.to_le_bytes());
    let upload = client.upload(&wav).await.unwrap();
    let id = client.submit(&upload).await.unwrap();
    assert!(matches!(
        client.poll(&id).await.unwrap(),
        TranscriptStatus::Pending
    ));
    assert!(matches!(
        client.poll(&id).await.unwrap(),
        TranscriptStatus::Completed(_)
    ));
    client.delete(&id).await.unwrap();
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 5);
    for r in &requests {
        assert!(r.to_lowercase().contains("authorization: fixture-key"));
        assert!(!r.contains("Bearer"));
    }
    assert!(requests[1].contains("\"speech_models\":[\"universal-2\"]"));
    assert!(requests[1].contains("\"disfluencies\":true"));
    assert!(requests[4].starts_with("DELETE /v2/transcript/transcript-1"));
}
#[tokio::test]
async fn rejects_redirects_untrusted_upload_urls_identity_mismatch_and_oversized_responses() {
    for (status, body, upload) in [
        (302, "{}".into(), false),
        (
            200,
            json!({"upload_url":"https://evil.example/upload"}).to_string(),
            true,
        ),
        (
            200,
            json!({"id":"another","status":"processing"}).to_string(),
            false,
        ),
        (200, "x".repeat(2 * 1024 * 1024 + 1), false),
    ] {
        let (client, task) = server(vec![(status, body)]).await;
        if upload {
            let mut wav = vec![0; 44];
            wav[..4].copy_from_slice(b"RIFF");
            wav[8..12].copy_from_slice(b"WAVE");
            wav[22..24].copy_from_slice(&1u16.to_le_bytes());
            assert!(client.upload(&wav).await.is_err());
        } else {
            assert!(client.poll("transcript-1").await.is_err());
        }
        let _ = task.await;
    }
}
#[tokio::test]
async fn rejects_path_injection_and_untrusted_endpoints_without_network() {
    let client = AssemblyAiTranscriber::new("test".into()).unwrap();
    assert!(client.poll("../bad").await.is_err());
    assert!(client.submit("http://169.254.169.254/audio").await.is_err());
    assert!(
        AssemblyAiTranscriber::with_endpoint(
            "test".into(),
            "https://evil.example/".parse().unwrap()
        )
        .is_err()
    );
}
