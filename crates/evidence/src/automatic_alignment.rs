//! Recorded STT supplies a distinct recording-backed source for drafting.
//! The original live timeline stays immutable and is not an ASR correctness gate.
//! Legacy v1 proof verification remains supported for previously imported sources.
use crate::{
    Error, Result,
    alignment::OperatorAlignmentProof,
    manifest::{Manifest, RecordedSourceTranscript, Segment, digest},
    media::CandidateClip,
    stt::RecordedTranscript,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const METHOD: &str = "assemblyai_independent_customer_stt_v1";
pub const RECORDED_METHOD: &str = "assemblyai_recorded_customer_stt_v2";

/// Prepare an independently transcribed full customer track. The original
/// provider timeline remains private provenance, not the final support text.
pub fn prepare_recorded_sources(
    manifest: &mut Manifest,
    transcript: &RecordedTranscript,
) -> Result<()> {
    recorded_preflight(manifest)?;
    validate_recorded_transcript(manifest, transcript)?;
    if !manifest.operator_alignment.is_empty() {
        return Err(Error::Invalid("automatic alignment proof conflict"));
    }
    let duration = manifest.media.as_ref().unwrap().decoded_duration_ms;
    manifest.recorded_transcript = Some(RecordedSourceTranscript {
        recording_sha256: manifest.recording_sha256.clone(),
        transcript_id: transcript.id.clone(),
        model: "universal-2".into(),
        transcript_sha256: digest(transcript.text.as_bytes()),
        segments: vec![Segment {
            source_id: format!(
                "{}/recorded/{}/customer",
                manifest.provider_session_id, transcript.id
            ),
            turn_id: format!("recorded:{}", transcript.id),
            item_id: None,
            speaker: "customer".into(),
            channel: 0,
            text: transcript.text.clone(),
            turn_status: "completed".into(),
            source_range_ms: Some([0, duration]),
            alignment: "recorded_customer_track".into(),
        }],
    });
    manifest.automatic_alignment.clear();
    manifest.approval_eligible = false;
    manifest.media.as_mut().unwrap().alignment_proven = false;
    Ok(())
}

fn validate_recorded_transcript(
    manifest: &Manifest,
    transcript: &RecordedTranscript,
) -> Result<()> {
    let media = manifest
        .media
        .as_ref()
        .ok_or(Error::Invalid("decoded recording required"))?;
    if transcript.id.is_empty()
        || transcript.id.len() > 100
        || !transcript
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || transcript.text.trim().is_empty()
        || transcript.text.len() > 200_000
        || transcript.words.is_empty()
        || transcript.words.len() > 20_000
        || !transcript.audio_duration.is_finite()
        || transcript.audio_duration <= 0.0
        || ((transcript.audio_duration * 1000.0) - media.decoded_duration_ms as f64).abs() > 1500.0
    {
        return Err(Error::Invalid("transcription identity or duration"));
    }
    // Word times are diagnostics for a whole-track source. Overlapping estimates
    // are valid; impossible bounds and non-finite confidence values are not.
    if transcript.words.iter().any(|w| {
        w.start >= w.end
            || w.end > media.decoded_duration_ms
            || w.text.trim().is_empty()
            || !w.confidence.is_finite()
            || w.confidence <= 0.0
            || w.confidence > 1.0
    }) {
        return Err(Error::Invalid("transcription word timing"));
    }
    Ok(())
}

/// Validate all derived-source bindings again when consuming durable proofs.
pub(crate) fn valid_recorded_source(manifest: &Manifest) -> bool {
    let Some(recorded) = &manifest.recorded_transcript else {
        return false;
    };
    let Some(media) = &manifest.media else {
        return false;
    };
    if recorded.recording_sha256 != manifest.recording_sha256
        || recorded.segments.len() != 1
        || recorded.model != "universal-2"
        || recorded.transcript_id.is_empty()
        || recorded.transcript_id.len() > 100
        || !recorded
            .transcript_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return false;
    }
    let source = &recorded.segments[0];
    source.source_id
        == format!(
            "{}/recorded/{}/customer",
            manifest.provider_session_id, recorded.transcript_id
        )
        && source.source_range_ms == Some([0, media.decoded_duration_ms])
        && source.speaker == "customer"
        && source.channel == 0
        && !source.text.trim().is_empty()
        && recorded.transcript_sha256 == digest(source.text.as_bytes())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedRange {
    pub source_id: String,
    pub source_range_ms: [u64; 2],
}
/// Validate saved recording bytes before making billable calls. An interrupted
/// conversation can have a valid finalized recording; this does not establish
/// that its last answer finished or that speech after disconnection was saved.
pub fn recorded_preflight(manifest: &Manifest) -> Result<()> {
    let media = manifest
        .media
        .as_ref()
        .ok_or(Error::Invalid("decoded recording required"))?;
    if manifest.dropped_chunks != Some(0)
        || media.channels != 2
        || media.decoded_duration_ms == 0
        || media.decoded_duration_ms > 390_000
        || media.customer_activity_groups_ms.is_empty()
        || media
            .customer_activity_groups_ms
            .iter()
            .any(|[start, end]| start >= end || *end > media.decoded_duration_ms)
        || manifest.recording_sha256.len() != 64
        || !manifest
            .recording_sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(Error::Invalid("recording completeness unverified"));
    }
    // Session wall-clock timestamps are not a documented sample-clock contract:
    // https://www.assemblyai.com/docs/voice-agents/voice-agent-api/session-history
    // Keep metadata_duration_delta_ms as diagnostic rather than treating an
    // arbitrary tolerance as missing speech. Recorded STT uses the decoded WAV
    // clock directly; the original provider turn clock remains diagnostic.
    let sources: Vec<_> = manifest
        .segments
        .iter()
        .filter(|s| s.speaker == "customer")
        .collect();
    if sources.len() > 100 || sources.iter().any(|s| s.channel != 0) {
        return Err(Error::Invalid("customer source identity"));
    }
    Ok(())
}

/// Legacy turn matching requires complete answers, unlike saved-track STT.
pub fn preflight(manifest: &Manifest) -> Result<()> {
    recorded_preflight(manifest)?;
    let sources: Vec<_> = manifest
        .segments
        .iter()
        .filter(|s| s.speaker == "customer")
        .collect();
    match manifest.product_end_reason.as_deref() {
        // Voice-agent turn status includes the agent reply. An explicit customer
        // Finish can interrupt that reply after a complete customer answer.
        Some("explicit_finish") => (),
        Some("explicit_stop")
            if !sources.is_empty()
                && sources.iter().all(|s| {
                    s.turn_status == "completed"
                        && !manifest.incomplete_turn_ids.contains(&s.turn_id)
                }) => {}
        _ => return Err(Error::Invalid("recording completion unresolved")),
    }
    Ok(())
}

pub fn plan(manifest: &Manifest, transcript: &RecordedTranscript) -> Result<Vec<VerifiedRange>> {
    recorded_preflight(manifest)?;
    if let Some(recorded) = &manifest.recorded_transcript {
        validate_recorded_transcript(manifest, transcript)?;
        if !valid_recorded_source(manifest)
            || recorded.transcript_id != transcript.id
            || recorded.transcript_sha256 != digest(transcript.text.as_bytes())
        {
            return Err(Error::Invalid("recorded source binding changed"));
        }
        return Ok(vec![VerifiedRange {
            source_id: recorded.segments[0].source_id.clone(),
            source_range_ms: recorded.segments[0].source_range_ms.unwrap(),
        }]);
    }
    preflight(manifest)?;
    let media = manifest.media.as_ref().unwrap();
    if transcript.id.is_empty()
        || transcript.id.len() > 100
        || !transcript
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || transcript.words.is_empty()
        || transcript.words.len() > 20_000
        || !transcript.audio_duration.is_finite()
        || ((transcript.audio_duration * 1000.0) - media.decoded_duration_ms as f64).abs() > 1500.0
    {
        return Err(Error::Invalid("transcription identity or duration"));
    }
    let mut observed = Vec::new();
    // Word-level ASR estimates can overlap inside an answer. Only whole-answer
    // clips are extracted, so require ordered starts AND ends rather than invented
    // disjoint word intervals. Final answer ranges must still never overlap.
    let mut previous_start = 0;
    let mut previous_end = 0;
    for (index, word) in transcript.words.iter().enumerate() {
        if word.start < previous_start
            || word.end < previous_end
            || word.start >= word.end
            || word.end > media.decoded_duration_ms
            || !word.confidence.is_finite()
            || word.confidence <= 0.0
            || word.confidence > 1.0
        {
            return Err(Error::Invalid("transcription word timing"));
        }
        previous_start = word.start;
        previous_end = word.end;
        let word_tokens = tokens(&word.text);
        if word_tokens.is_empty() {
            return Err(Error::Invalid("transcription empty word"));
        }
        observed.extend(word_tokens.into_iter().map(|t| (t, index)));
    }
    let flat_observed: Vec<_> = observed.iter().map(|(t, _)| t.clone()).collect();
    if tokens(&transcript.text) != flat_observed {
        return Err(Error::Invalid("transcription word text mismatch"));
    }
    let sources: Vec<_> = manifest
        .segments
        .iter()
        .filter(|s| s.speaker == "customer")
        .collect();
    let expected: Vec<_> = sources.iter().flat_map(|s| tokens(&s.text)).collect();
    if expected != flat_observed {
        return Err(Error::Invalid(
            "recorded transcript differs from original answers",
        ));
    }
    let mut offset = 0;
    let mut ranges = Vec::new();
    for source in sources {
        let count = tokens(&source.text).len();
        let first = observed[offset].1;
        let last = observed[offset + count - 1].1;
        if (offset > 0 && observed[offset - 1].1 == first)
            || (offset + count < observed.len() && observed[offset + count].1 == last)
        {
            return Err(Error::Invalid("ambiguous answer boundary"));
        }
        let mut start = transcript.words[first].start;
        let mut end = transcript.words[last].end;
        let groups: Vec<_> = media
            .customer_activity_groups_ms
            .iter()
            .filter(|[a, b]| *a < end && *b > start)
            .collect();
        if groups.is_empty() {
            return Err(Error::Invalid("recorded speech has no audible source"));
        }
        for [a, b] in groups {
            start = start.min(*a);
            end = end.max(*b);
        }
        if ranges
            .iter()
            .any(|r: &VerifiedRange| r.source_range_ms[1] > start)
        {
            return Err(Error::Invalid("ambiguous overlapping whole answers"));
        }
        ranges.push(VerifiedRange {
            source_id: source.source_id.clone(),
            source_range_ms: [start, end],
        });
        offset += count;
    }
    if media.customer_activity_groups_ms.iter().any(|[a, b]| {
        *a >= *b
            || *b > media.decoded_duration_ms
            || !ranges
                .iter()
                .any(|r| r.source_range_ms[0] <= *a && r.source_range_ms[1] >= *b)
    }) {
        return Err(Error::Invalid("untranscribed customer audio remains"));
    }
    Ok(ranges)
}

/// Clips must be generated by FfmpegValidator::preview_range against exactly the
/// original recording. The application fences lease, revisions and access before
/// atomically persisting this manifest, clip bytes, proof and workflow invalidation.
pub fn confirm(
    manifest: &mut Manifest,
    transcript: &RecordedTranscript,
    clips: &[CandidateClip],
    at: DateTime<Utc>,
) -> Result<()> {
    let ranges = plan(manifest, transcript)?;
    if !manifest.operator_alignment.is_empty() || clips.len() != ranges.len() {
        return Err(Error::Invalid("automatic alignment proof conflict"));
    }
    let mut proofs = Vec::new();
    for range in ranges {
        let candidates: Vec<_> = clips
            .iter()
            .filter(|c| c.source_id == range.source_id)
            .collect();
        let clip = candidates
            .first()
            .filter(|_| candidates.len() == 1)
            .ok_or(Error::Invalid("automatic alignment clip identity"))?;
        if clip.source_range_ms != range.source_range_ms
            || clip.source_recording_sha256 != manifest.recording_sha256
            || clip.wav.len() < 44
            || &clip.wav[..4] != b"RIFF"
            || &clip.wav[8..12] != b"WAVE"
        {
            return Err(Error::Invalid("automatic alignment clip binding"));
        }
        let source = manifest
            .support_segments()
            .iter()
            .find(|s| s.source_id == range.source_id)
            .unwrap();
        proofs.push(OperatorAlignmentProof {
            recording_sha256: manifest.recording_sha256.clone(),
            timeline_sha256: manifest.timeline_sha256.clone(),
            source_id: range.source_id,
            source_text_sha256: digest(source.text.as_bytes()),
            source_range_ms: range.source_range_ms,
            clip_sha256: digest(&clip.wav),
            method: if manifest.recorded_transcript.is_some() {
                RECORDED_METHOD
            } else {
                METHOD
            }
            .into(),
            verified_by: format!("assemblyai:{}", transcript.id),
            verified_at: at,
        });
    }
    manifest.automatic_alignment = proofs;
    manifest.approval_eligible = true;
    manifest.media.as_mut().unwrap().alignment_proven = true;
    manifest.recording_validation = if manifest.recorded_transcript.is_some() {
        "recorded_customer_stt_verified"
    } else {
        "independent_customer_stt_alignment_verified"
    }
    .into();
    Ok(())
}

// Case, apostrophe form and ordinary punctuation are immaterial. Preserve every
// word, filler, negation, numeral, decimal, sign, currency and percentage marker.
// No stemming, fuzzy comparison, dropped qualifiers or guessed number conversion.
fn tokens(text: &str) -> Vec<String> {
    let chars: Vec<_> = text.to_lowercase().replace('’', "'").chars().collect();
    let mut out = Vec::new();
    let mut word = String::new();
    for (i, c) in chars.iter().copied().enumerate() {
        let between_digits = i > 0
            && i + 1 < chars.len()
            && chars[i - 1].is_ascii_digit()
            && chars[i + 1].is_ascii_digit();
        let between_letters = i > 0
            && i + 1 < chars.len()
            && chars[i - 1].is_alphabetic()
            && chars[i + 1].is_alphabetic();
        if c.is_alphanumeric()
            || (c == '\'' && between_letters)
            || (matches!(c, '.' | ',') && between_digits)
        {
            word.push(c);
        } else {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            if matches!(
                c,
                '+' | '-'
                    | '%'
                    | '$'
                    | '€'
                    | '£'
                    | '¥'
                    | '−'
                    | '<'
                    | '>'
                    | '='
                    | '&'
                    | '@'
                    | '/'
                    | '×'
                    | '÷'
            ) {
                out.push(c.to_string());
            }
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}
