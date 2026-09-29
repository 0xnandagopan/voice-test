use serde_json::json;
use v0_evidence::{
    alignment::{OperatorAlignmentConfirmation, confirm_operator_alignment, source_proof},
    manifest::{self, Manifest},
    media::{CandidateClip, MediaReport},
};
fn fixture() -> Manifest {
    let mut m = manifest::build("session", b"OggSfixture", &serde_json::to_vec(&json!({"session_id":"session","started_at_unix_ms":0,"turns":[
        {"turn_id":"a","status":"completed","user_transcript":"First complete answer.","user_speech_started_at_ms":100,"user_speech_ended_at_ms":800},
        {"turn_id":"b","status":"completed","user_transcript":"Second complete answer.","user_speech_started_at_ms":1100,"user_speech_ended_at_ms":1800}
    ]})).unwrap(), &serde_json::to_vec(&json!({"session_id":"session","started_at":"1970-01-01T00:00:00Z","ended_at":"1970-01-01T00:00:02Z","format":"ogg/opus","channels":2,"channel_layout":"stereo (left=user, right=agent)","sample_rate":24000,"file":"f.ogg","dropped_chunks":0,"uploaded_chunks":1})).unwrap()).unwrap();
    m.product_end_reason = Some("explicit_finish".into());
    m.media = Some(MediaReport {
        codec: "opus".into(),
        channels: 2,
        sample_rate: 24000,
        decoded_duration_ms: 2000,
        metadata_duration_delta_ms: 0,
        ranges: vec![],
        alignment_proven: false,
        customer_activity_groups_ms: vec![[0, 900], [1000, 1900]],
    });
    m
}
fn candidate(
    m: &Manifest,
    index: usize,
    range: [u64; 2],
) -> (CandidateClip, OperatorAlignmentConfirmation) {
    let s = &m.segments[index];
    let clip = CandidateClip {
        wav: b"test private bounded clip".to_vec(),
        source_id: s.source_id.clone(),
        source_range_ms: range,
        source_recording_sha256: m.recording_sha256.clone(),
    };
    let c = OperatorAlignmentConfirmation {
        recording_sha256: m.recording_sha256.clone(),
        timeline_sha256: m.timeline_sha256.clone(),
        source_id: s.source_id.clone(),
        source_text_sha256: manifest::digest(s.text.as_bytes()),
        source_range_ms: range,
        clip_sha256: manifest::digest(&clip.wav),
        listened: true,
        transcript_matches: true,
        complete_answer: true,
    };
    (clip, c)
}
fn confirm(m: &mut Manifest, clip: &CandidateClip, c: &OperatorAlignmentConfirmation) -> bool {
    confirm_operator_alignment(m, clip, c, "authenticated-operator", chrono::Utc::now()).is_ok()
}
#[test]
fn partial_verification_cannot_approve_and_original_provenance_is_immutable() {
    let mut m = fixture();
    let original = m.segments[0].source_range_ms;
    let (clip, c) = candidate(&m, 0, [0, 900]);
    assert!(confirm(&mut m, &clip, &c));
    assert!(!m.approval_eligible);
    assert!(!m.media.as_ref().unwrap().alignment_proven);
    assert_eq!(m.segments[0].source_range_ms, original);
    assert_eq!(m.segments[0].alignment, "provider_range_unverified");
    let (clip, c) = candidate(&m, 1, [1000, 1900]);
    assert!(confirm(&mut m, &clip, &c));
    assert!(m.approval_eligible);
    assert!(m.media.as_ref().unwrap().alignment_proven);
    m.segments[0].text.push_str(" changed");
    assert!(source_proof(&m, &m.segments[0].source_id).is_none());
}
#[test]
fn rejects_stale_hash_text_clip_and_missing_attestation() {
    for kind in 0..7 {
        let mut m = fixture();
        let (clip, mut c) = candidate(&m, 0, [0, 900]);
        match kind {
            0 => c.recording_sha256 = "changed".into(),
            1 => c.timeline_sha256 = "changed".into(),
            2 => c.source_text_sha256 = "changed".into(),
            3 => c.clip_sha256 = "changed".into(),
            4 => c.listened = false,
            5 => c.transcript_matches = false,
            _ => c.complete_answer = false,
        }
        assert!(!confirm(&mut m, &clip, &c));
        assert!(m.operator_alignment.is_empty());
    }
}
#[test]
fn rejects_drop_unknown_completeness_truncated_activity_and_overlap() {
    for kind in 0..4 {
        let mut m = fixture();
        if kind == 0 {
            m.dropped_chunks = Some(1)
        } else if kind == 1 {
            m.dropped_chunks = None
        } else if kind == 2 {
            m.media.as_mut().unwrap().metadata_duration_delta_ms = 2000
        }
        let range = if kind == 3 { [100, 800] } else { [0, 900] };
        let (clip, c) = candidate(&m, 0, range);
        assert!(!confirm(&mut m, &clip, &c));
    }
    let mut m = fixture();
    let (clip, c) = candidate(&m, 0, [0, 1900]);
    assert!(confirm(&mut m, &clip, &c));
    let (clip, c) = candidate(&m, 1, [1000, 1900]);
    assert!(!confirm(&mut m, &clip, &c));
}
#[test]
fn unresolved_loss_tail_and_untranscribed_audio_stay_blocked() {
    let mut m = fixture();
    m.product_end_reason = Some("transport_lost".into());
    let (clip, c) = candidate(&m, 1, [1000, 1900]);
    assert!(!confirm(&mut m, &clip, &c));
    let mut m = fixture();
    m.media
        .as_mut()
        .unwrap()
        .customer_activity_groups_ms
        .push([1950, 1990]);
    for i in 0..2 {
        let (clip, c) = candidate(&m, i, if i == 0 { [0, 900] } else { [1000, 1900] });
        assert!(confirm(&mut m, &clip, &c));
    }
    assert!(!m.approval_eligible);
}
#[test]
fn interrupted_agent_reply_does_not_prevent_explicit_whole_answer_verification() {
    let mut m = fixture();
    m.segments[1].turn_status = "interrupted".into();
    m.incomplete_turn_ids.push(m.segments[1].turn_id.clone());
    let (clip, c) = candidate(&m, 1, [1000, 1900]);
    assert!(confirm(&mut m, &clip, &c));
    assert!(source_proof(&m, &m.segments[1].source_id).is_some());
}
