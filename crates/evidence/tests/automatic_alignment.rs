use serde_json::json;
use v0_evidence::{
    alignment::source_proof,
    automatic_alignment::{self, confirm, plan, preflight},
    manifest::{self, Manifest},
    media::{CandidateClip, MediaReport},
    stt::{RecordedTranscript, RecordedWord},
};
fn fixture() -> (Manifest, RecordedTranscript) {
    let mut m = manifest::build("session", b"OggSfixture", &serde_json::to_vec(&json!({"session_id":"session","started_at_unix_ms":0,"turns":[
        {"turn_id":"a","status":"completed","user_transcript":"It helped, but not always."},
        {"turn_id":"b","status":"completed","user_transcript":"Around 60+ registrations."}
    ]})).unwrap(), &serde_json::to_vec(&json!({"session_id":"session","started_at":"1970-01-01T00:00:00Z","ended_at":"1970-01-01T00:00:04Z","format":"ogg/opus","channels":2,"channel_layout":"stereo (left=user, right=agent)","sample_rate":24000,"file":"f.ogg","dropped_chunks":0,"uploaded_chunks":1})).unwrap()).unwrap();
    m.product_end_reason = Some("explicit_finish".into());
    m.media = Some(MediaReport {
        codec: "opus".into(),
        channels: 2,
        sample_rate: 24000,
        decoded_duration_ms: 4000,
        metadata_duration_delta_ms: 0,
        ranges: vec![],
        alignment_proven: false,
        customer_activity_groups_ms: vec![[100, 1500], [2200, 3300]],
    });
    let words = [
        ("It", 200, 300),
        ("helped,", 350, 600),
        ("but", 650, 800),
        ("not", 850, 1000),
        ("always.", 1100, 1400),
        ("Around", 2300, 2500),
        ("60+", 2600, 2800),
        ("registrations.", 2900, 3200),
    ]
    .into_iter()
    .map(|(text, start, end)| RecordedWord {
        text: text.into(),
        start,
        end,
        confidence: 0.9,
    })
    .collect();
    (
        m,
        RecordedTranscript {
            id: "test-transcript".into(),
            text: "It helped, but not always. Around 60+ registrations.".into(),
            words,
            audio_duration: 4.0,
        },
    )
}
fn clips(m: &Manifest, t: &RecordedTranscript) -> Vec<CandidateClip> {
    plan(m, t)
        .unwrap()
        .into_iter()
        .map(|r| {
            let mut wav = vec![0; 44];
            wav[..4].copy_from_slice(b"RIFF");
            wav[8..12].copy_from_slice(b"WAVE");
            CandidateClip {
                wav,
                source_id: r.source_id,
                source_range_ms: r.source_range_ms,
                source_recording_sha256: m.recording_sha256.clone(),
            }
        })
        .collect()
}
#[test]
fn exact_independent_recording_agreement_verifies_without_operator_and_preserves_provenance() {
    let (mut m, t) = fixture();
    let original = serde_json::to_value(&m.segments).unwrap();
    let clips = clips(&m, &t);
    confirm(&mut m, &t, &clips, chrono::Utc::now()).unwrap();
    assert!(m.approval_eligible);
    assert!(m.operator_alignment.is_empty());
    assert_eq!(m.automatic_alignment.len(), 2);
    assert_eq!(serde_json::to_value(&m.segments).unwrap(), original);
    assert_eq!(
        source_proof(&m, &m.segments[0].source_id).unwrap().method,
        automatic_alignment::METHOD
    );
    m.segments[0].text.push_str(" However");
    assert!(source_proof(&m, &m.segments[0].source_id).is_none());
}
#[test]
fn negations_numbers_qualifiers_symbols_and_extra_audio_words_cannot_disappear() {
    for altered in [
        "It helped, always.",
        "It helped, but always.",
        "Around 60 registrations.",
        "Around 16+ registrations.",
        "60+ registrations.",
        "Around -60+ registrations.",
        "Around $60+ registrations.",
        "Around 60.5+ registrations.",
        "Around 60% registrations.",
        "Around <60+ registrations.",
        "Around >60+ registrations.",
    ] {
        let (mut m, t) = fixture();
        let i = usize::from(altered.contains("registrations"));
        m.segments[i].text = altered.into();
        assert!(plan(&m, &t).is_err(), "must reject semantic drift");
    }
    let (m, mut t) = fixture();
    t.words.push(RecordedWord {
        text: "except".into(),
        start: 3500,
        end: 3800,
        confidence: 0.8,
    });
    t.text.push_str(" except");
    assert!(plan(&m, &t).is_err());
}
#[test]
fn case_and_punctuation_are_normalized_but_low_confidence_is_not_discarded() {
    let (mut m, mut t) = fixture();
    m.segments[0].text = "IT HELPED; but not always!".into();
    t.words[0].confidence = 0.6;
    assert!(plan(&m, &t).is_ok());
}
#[test]
fn rejects_untranscribed_audio_overlap_timing_and_false_duration() {
    for bad in 0..8 {
        let (mut m, mut t) = fixture();
        match bad {
            0 => m
                .media
                .as_mut()
                .unwrap()
                .customer_activity_groups_ms
                .push([3500, 3900]),
            1 => m.media.as_mut().unwrap().customer_activity_groups_ms = vec![[100, 3300]],
            2 => t.words[1].start = 0,
            3 => t.words[7].end = 4500,
            4 => t.audio_duration = 10.0,
            5 => t.words[0].confidence = f64::NAN,
            6 => t.words[0].confidence = 0.0,
            _ => t.text = "Unrelated transcript".into(),
        };
        assert!(plan(&m, &t).is_err());
    }
}
#[test]
fn preflight_rejects_loss_drops_and_partial_pause_but_allows_completed_pause() {
    let (mut m, t) = fixture();
    m.product_end_reason = Some("explicit_stop".into());
    assert!(plan(&m, &t).is_ok());
    for bad in 0..5 {
        let (mut m, _) = fixture();
        match bad {
            0 => m.dropped_chunks = None,
            1 => m.dropped_chunks = Some(1),
            2 => m.product_end_reason = Some("transport_lost".into()),
            3 => {
                m.product_end_reason = Some("explicit_stop".into());
                m.incomplete_turn_ids.push("b".into())
            }
            _ => m.media.as_mut().unwrap().decoded_duration_ms = 0,
        };
        assert!(preflight(&m).is_err());
    }
}
#[test]
fn confirmation_is_atomic_and_rejects_wrong_clips_or_replaced_recordings() {
    for bad in 0..4 {
        let (mut m, t) = fixture();
        let mut c = clips(&m, &t);
        match bad {
            0 => c[0].source_range_ms = [0, 4000],
            1 => c[0].source_recording_sha256 = "wrong".into(),
            2 => {
                c.pop();
            }
            _ => c[1].source_id = c[0].source_id.clone(),
        };
        assert!(confirm(&mut m, &t, &c, chrono::Utc::now()).is_err());
        assert!(m.automatic_alignment.is_empty());
        assert!(!m.approval_eligible);
    }
}

#[test]
fn session_clock_offset_is_diagnostic_but_missing_words_or_audio_still_fail() {
    let (mut m, transcript) = fixture();
    m.duration_ms = 6200;
    m.media.as_mut().unwrap().metadata_duration_delta_ms = -2200;
    preflight(&m).unwrap();
    let candidate_clips = clips(&m, &transcript);
    confirm(&mut m, &transcript, &candidate_clips, chrono::Utc::now()).unwrap();
    assert!(m.approval_eligible);
    assert_eq!(m.media.as_ref().unwrap().metadata_duration_delta_ms, -2200);
    assert!(source_proof(&m, &m.segments[0].source_id).is_some());
    // A wall-clock offset cannot excuse missing original answer content.
    let mut truncated = transcript.clone();
    truncated.words.truncate(5);
    truncated.text = "It helped, but not always.".into();
    assert!(plan(&m, &truncated).is_err());
    let mut wrong_file_duration = transcript.clone();
    wrong_file_duration.audio_duration = 6.2;
    assert!(plan(&m, &wrong_file_duration).is_err());
    m.media
        .as_mut()
        .unwrap()
        .customer_activity_groups_ms
        .push([3500, 3900]);
    assert!(plan(&m, &transcript).is_err());
    m.dropped_chunks = Some(1);
    assert!(preflight(&m).is_err());
    assert!(source_proof(&m, &m.segments[0].source_id).is_none());
}

#[test]
fn ordered_word_overlaps_inside_one_answer_preserve_whole_answer_verification() {
    let (mut m, mut transcript) = fixture();
    // The provider may estimate adjacent words over a shared acoustic boundary.
    transcript.words[1].start = 280;
    assert!(transcript.words[1].start < transcript.words[0].end);
    let ranges = plan(&m, &transcript).unwrap();
    assert_eq!(ranges[0].source_range_ms, [100, 1500]);
    let candidate_clips = clips(&m, &transcript);
    confirm(&mut m, &transcript, &candidate_clips, chrono::Utc::now()).unwrap();
    assert!(m.approval_eligible);
}
#[test]
fn decreasing_word_starts_or_ends_and_cross_answer_overlap_stay_rejected() {
    for issue in 0..3 {
        let (m, mut transcript) = fixture();
        match issue {
            0 => transcript.words[1].start = 100,
            1 => {
                transcript.words[1].start = 210;
                transcript.words[1].end = 290;
            }
            _ => {
                transcript.words[4].end = 2350;
            }
        }
        // Last case preserves nondecreasing starts and ends but crosses the
        // original answer boundary; overlapping answer clips cannot be approved.
        assert!(plan(&m, &transcript).is_err());
    }
}

#[test]
fn recorded_sources_replace_live_caption_dependency_without_rewriting_original() {
    let (mut m, t) = fixture();
    m.segments[0].text = "A different live recognition of the answer".into();
    let original = serde_json::to_value(&m.segments).unwrap();
    let timeline_sha = m.timeline_sha256.clone();
    automatic_alignment::prepare_recorded_sources(&mut m, &t).unwrap();
    assert_eq!(serde_json::to_value(&m.segments).unwrap(), original);
    assert_eq!(m.timeline_sha256, timeline_sha);
    assert_eq!(m.support_segments().len(), 1);
    assert_eq!(m.support_segments()[0].text, t.text);
    assert_eq!(m.support_segments()[0].source_range_ms, Some([0, 4000]));
    assert!(m.support_segments()[0].text.contains("but not always"));
    assert!(m.support_segments()[0].text.contains("Around 60+"));
    let c = clips(&m, &t);
    confirm(&mut m, &t, &c, chrono::Utc::now()).unwrap();
    assert!(m.approval_eligible);
    let source_id = m.support_segments()[0].source_id.clone();
    assert_eq!(
        source_proof(&m, &source_id).unwrap().method,
        automatic_alignment::RECORDED_METHOD
    );
    // The live captions are no longer proof text. The recorded source is.
    m.segments[0].text.push_str(" live caption correction");
    assert!(source_proof(&m, &source_id).is_some());
    m.recorded_transcript.as_mut().unwrap().segments[0].text = "60 registrations guaranteed".into();
    assert!(source_proof(&m, &source_id).is_none());
}

#[test]
fn whole_track_sources_allow_asr_word_estimate_overlap_but_reject_impossible_media() {
    let (mut m, mut t) = fixture();
    t.words[1].start = 100; // harmless overlapping word estimates in one full track
    t.words[1].end = 250;
    automatic_alignment::prepare_recorded_sources(&mut m, &t).unwrap();
    assert_eq!(plan(&m, &t).unwrap()[0].source_range_ms, [0, 4000]);
    for fault in 0..7 {
        let (mut m, mut t) = fixture();
        match fault {
            0 => m.media = None,
            1 => m.dropped_chunks = Some(1),
            2 => m.media.as_mut().unwrap().channels = 1,
            3 => t.words[0].end = 4001,
            4 => t.audio_duration = 9.0,
            5 => t.words[0].confidence = f64::NAN,
            _ => m.media.as_mut().unwrap().customer_activity_groups_ms = vec![[0, 4500]],
        }
        assert!(automatic_alignment::prepare_recorded_sources(&mut m, &t).is_err());
        assert!(m.recorded_transcript.is_none());
        assert!(!m.approval_eligible);
    }
}

#[test]
fn recorded_source_proofs_bind_recording_identity_transcript_and_clip() {
    for fault in 0..5 {
        let (mut m, t) = fixture();
        automatic_alignment::prepare_recorded_sources(&mut m, &t).unwrap();
        let c = clips(&m, &t);
        confirm(&mut m, &t, &c, chrono::Utc::now()).unwrap();
        let id = m.support_segments()[0].source_id.clone();
        match fault {
            0 => m.recording_sha256 = "0".repeat(64),
            1 => m.recorded_transcript.as_mut().unwrap().recording_sha256 = "0".repeat(64),
            2 => m.recorded_transcript.as_mut().unwrap().transcript_id = "other".into(),
            3 => {
                m.recorded_transcript.as_mut().unwrap().segments[0].source_range_ms =
                    Some([100, 3900])
            }
            _ => m.automatic_alignment[0].verified_by = "assemblyai:other".into(),
        }
        assert!(source_proof(&m, &id).is_none());
    }
}

#[test]
fn whole_recorded_source_does_not_need_missing_live_captions_and_recovery_keeps_progress_external()
{
    let (mut m, t) = fixture();
    m.segments.clear();
    automatic_alignment::prepare_recorded_sources(&mut m, &t).unwrap();
    let c = clips(&m, &t);
    confirm(&mut m, &t, &c, chrono::Utc::now()).unwrap();
    let (recorded, unresolved) = v0_evidence::recovery::reconstruct(uuid::Uuid::new_v4(), &m);
    assert!(unresolved.is_empty());
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].text, t.text);
    assert_eq!(
        recorded[0].status,
        "recorded_utterance_automatically_verified"
    );
    assert!(m.segments.is_empty());
}

#[test]
fn interrupted_saved_track_is_usable_without_declaring_original_answer_complete() {
    for end in [None, Some("transport_lost"), Some("explicit_stop")] {
        let (mut m, t) = fixture();
        m.product_end_reason = end.map(str::to_owned);
        m.segments[1].turn_status = "incomplete".into();
        m.incomplete_turn_ids.push("b".into());
        let original = serde_json::to_value(&m.segments).unwrap();
        assert!(preflight(&m).is_err()); // old turn-to-caption proof stays strict
        automatic_alignment::prepare_recorded_sources(&mut m, &t).unwrap();
        let c = clips(&m, &t);
        confirm(&mut m, &t, &c, chrono::Utc::now()).unwrap();
        assert!(m.approval_eligible);
        assert!(source_proof(&m, &m.support_segments()[0].source_id).is_some());
        assert_eq!(m.support_segments()[0].source_range_ms, Some([0, 4000]));
        assert_eq!(serde_json::to_value(&m.segments).unwrap(), original);
        assert_eq!(m.incomplete_turn_ids, vec!["b"]);
        assert_eq!(m.product_end_reason.as_deref(), end);
    }
}
