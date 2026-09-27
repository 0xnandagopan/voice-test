use serde_json::json;
use std::{path::PathBuf, process::Stdio};
use uuid::Uuid;
use v0_evidence::{manifest, media::FfmpegValidator};

fn tools() -> (PathBuf, PathBuf) {
    (
        std::env::var("FFMPEG_PATH")
            .expect("FFMPEG_PATH required")
            .into(),
        std::env::var("FFPROBE_PATH")
            .expect("FFPROBE_PATH required")
            .into(),
    )
}
fn fixture(audio: &[u8]) -> manifest::Manifest {
    manifest::build("sess_media",audio,
        &serde_json::to_vec(&json!({"session_id":"sess_media","started_at_unix_ms":0,"turns":[{"turn_id":"user_turn","status":"completed","user_transcript":"Synthetic answer","user_speech_started_at_ms":200,"user_speech_ended_at_ms":800,"agent_text":"Silent channel fixture","agent_reply_started_at_ms":200,"agent_reply_ended_at_ms":800},{"turn_id":"out_of_file","status":"interrupted","user_transcript":"Truncated answer","user_speech_started_at_ms":2200,"user_speech_ended_at_ms":2800}]})).unwrap(),
        &serde_json::to_vec(&json!({"session_id":"sess_media","started_at":"1970-01-01T00:00:00Z","ended_at":"1970-01-01T00:00:03Z","format":"ogg/opus","channels":2,"channel_layout":"stereo (left=user, right=agent)","sample_rate":24000,"file":"media.ogg","dropped_chunks":0,"uploaded_chunks":1})).unwrap()).unwrap()
}
#[tokio::test]
#[ignore = "requires FFMPEG_PATH and FFPROBE_PATH"]
async fn real_opus_decode_validates_channels_bounds_and_reconstruction_without_approval() {
    let (ffmpeg, ffprobe) = tools();
    let dir = std::env::temp_dir().join(format!("v0-media-test-{}", Uuid::new_v4()));
    tokio::fs::create_dir(&dir).await.unwrap();
    let source = dir.join("fixture.ogg");
    let status = tokio::process::Command::new(&ffmpeg)
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "aevalsrc=0.25*sin(2*PI*440*t)|0:s=24000:d=1",
            "-c:a",
            "libopus",
        ])
        .arg(&source)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let audio = tokio::fs::read(&source).await.unwrap();
    let mut manifest = fixture(&audio);
    let work = dir.join("jobs");
    let validator = FfmpegValidator::new(ffmpeg, ffprobe, &work);
    let report = validator.validate(&audio, &manifest).await.unwrap();
    assert!((990..=1010).contains(&report.decoded_duration_ms));
    assert!(report.ranges[0].within_recording);
    assert!(report.ranges[0].audible_samples > 0);
    assert_eq!(report.ranges[1].audible_samples, 0);
    assert_eq!(report.ranges[1].status, "no_audible_samples");
    assert!(!report.ranges[2].within_recording);
    assert_eq!(report.ranges[2].status, "outside_decoded_recording");
    assert!(!report.alignment_proven);
    manifest.media = Some(report);
    let (recorded, unresolved) = v0_evidence::recovery::reconstruct(Uuid::new_v4(), &manifest);
    assert_eq!(recorded.len(), 1);
    assert_eq!(unresolved.len(), 1);
    assert!(!manifest.approval_eligible);
    let clip = validator
        .candidate_clip(&audio, &manifest, &manifest.segments[0].source_id)
        .await
        .unwrap();
    assert_eq!(clip.source_range_ms, [0, 1000]);
    assert_eq!(&clip.wav[..4], b"RIFF");
    assert_eq!(clip.wav.len(), 48044);
    assert!(
        validator
            .candidate_clip(b"OggSchanged", &manifest, &manifest.segments[0].source_id)
            .await
            .is_err()
    );
    // Provider client_end and completed cannot certify the last answer after loss.
    let mut last_answer: manifest::Manifest =
        serde_json::from_value(serde_json::to_value(&manifest).unwrap()).unwrap();
    last_answer.segments.truncate(2);
    last_answer.provider_close_reason = Some("client_end".into());
    last_answer.product_end_reason = Some("transport_lost".into());
    let (recorded, unresolved) = v0_evidence::recovery::reconstruct(Uuid::new_v4(), &last_answer);
    assert!(recorded.is_empty());
    assert_eq!(unresolved.len(), 1);
    last_answer.product_end_reason = Some("explicit_finish".into());
    let (recorded, unresolved) = v0_evidence::recovery::reconstruct(Uuid::new_v4(), &last_answer);
    assert_eq!(recorded.len(), 1);
    assert!(unresolved.is_empty());
    assert!(
        tokio::fs::read_dir(&work)
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        validator
            .validate(b"OggSnot-real-media", &manifest)
            .await
            .is_err()
    );
    assert!(
        tokio::fs::read_dir(&work)
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none()
    );
    tokio::fs::remove_dir_all(dir).await.unwrap();
}

#[tokio::test]
#[ignore = "requires TEST_ARTIFACT_DIR plus local FFmpeg tools; reads only existing private synthetic fixtures"]
async fn inspect_downloaded_synthetic_recording_ranges() {
    let (ffmpeg, ffprobe) = tools();
    let dir = PathBuf::from(
        std::env::var("TEST_ARTIFACT_DIR").expect("private synthetic artifacts required"),
    );
    let audio = tokio::fs::read(dir.join("audio.ogg")).await.unwrap();
    let timeline = tokio::fs::read(dir.join("timeline.json")).await.unwrap();
    let metadata = tokio::fs::read(dir.join("metadata.json")).await.unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&metadata).unwrap();
    let session = meta["session_id"].as_str().unwrap();
    let mut manifest = manifest::build(session, &audio, &timeline, &metadata).unwrap();
    if let Ok(closure) = tokio::fs::read(dir.join("closure.json")).await {
        let closure: serde_json::Value = serde_json::from_slice(&closure).unwrap();
        manifest.provider_close_reason = closure["public_close_reason"].as_str().map(String::from);
    }
    let validator = FfmpegValidator::new(
        ffmpeg,
        ffprobe,
        std::env::temp_dir().join("v0-private-fixture-validation"),
    );
    let report = validator.validate(&audio, &manifest).await.unwrap();
    println!(
        "decoded_ms={} metadata_delta_ms={} customer_activity_groups_ms={:?}",
        report.decoded_duration_ms,
        report.metadata_duration_delta_ms,
        report.customer_activity_groups_ms
    );
    for (segment, check) in manifest
        .segments
        .iter()
        .zip(&report.ranges)
        .filter(|(s, _)| s.speaker == "customer")
    {
        println!(
            "original_provider_range={:?} candidate_range={:?} decoded_range_status={}",
            segment.source_range_ms, check.candidate_source_range_ms, check.status
        );
    }
    manifest.media = Some(report);
    if let Ok(clip_dir) = std::env::var("TEST_CLIP_DIR") {
        use v0_evidence::storage::PrivateStorage;
        let storage = v0_evidence::storage::LocalPrivateStorage::new(clip_dir)
            .await
            .unwrap();
        for segment in manifest.segments.iter().filter(|s| s.speaker == "customer") {
            let clip = validator
                .candidate_clip(&audio, &manifest, &segment.source_id)
                .await
                .unwrap();
            let key = format!(
                "{}.wav",
                v0_evidence::manifest::digest(segment.source_id.as_bytes())
            );
            storage.put(&key, &clip.wav).await.unwrap();
            println!(
                "private_candidate_clip_bytes={} source_range={:?}",
                clip.wav.len(),
                clip.source_range_ms
            );
        }
    }
    let (recorded, unresolved) = v0_evidence::recovery::reconstruct(Uuid::new_v4(), &manifest);
    println!(
        "recorded_utterance_candidates={} unresolved_answers={} approval_eligible={}",
        recorded.len(),
        unresolved.len(),
        manifest.approval_eligible
    );
    assert!(!manifest.approval_eligible);
}
