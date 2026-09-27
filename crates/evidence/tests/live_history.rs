//! Explicit opt-in, read-only provider gate. Supply a completed synthetic session only.
use v0_evidence::{
    manifest,
    provider::{AssemblyHistory, HistoryProvider},
};

#[tokio::test]
#[ignore = "requires provider credentials and a completed synthetic TEST_PROVIDER_SESSION_ID"]
async fn inspect_real_history_without_logging_customer_content() {
    let key = std::env::var("VOICE_AGENT_API_KEY").expect("populate VOICE_AGENT_API_KEY locally");
    let session =
        std::env::var("TEST_PROVIDER_SESSION_ID").expect("completed synthetic session required");
    let hosts = std::env::var("VOICE_ARTIFACT_HOSTS")
        .unwrap_or_else(|_| "s3.amazonaws.com".into())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let provider = AssemblyHistory::new(key, hosts).unwrap();
    let artifacts = provider
        .fetch(&session)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let manifest = manifest::build(
        &session,
        &artifacts.audio,
        &artifacts.timeline,
        &artifacts.metadata,
    )
    .unwrap();
    println!(
        "recording_bytes={} duration_ms={} customer_segments={} missing_alignment={} incomplete_turns={} approval_eligible={}",
        artifacts.audio.len(),
        manifest.duration_ms,
        manifest
            .segments
            .iter()
            .filter(|s| s.speaker == "customer")
            .count(),
        manifest
            .segments
            .iter()
            .filter(|s| s.alignment == "missing_alignment")
            .count(),
        manifest.incomplete_turn_ids.len(),
        manifest.approval_eligible
    );
    assert!(
        !manifest.approval_eligible,
        "this foundation must not claim validated alignment"
    );
}
