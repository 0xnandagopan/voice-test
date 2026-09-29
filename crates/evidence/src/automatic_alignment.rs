//! Independent customer-channel transcription verifies the original timeline.
//! This is exact token agreement, not fuzzy semantic matching or model approval.
//! Original source text, ranges and turn identities are never rewritten.
use crate::{
    Error, Result,
    alignment::OperatorAlignmentProof,
    manifest::{Manifest, digest},
    media::CandidateClip,
    stt::RecordedTranscript,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const METHOD: &str = "assemblyai_independent_customer_stt_v1";
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedRange {
    pub source_id: String,
    pub source_range_ms: [u64; 2],
}
/// Reject known missing or uncertain recording tails before making billable calls.
pub fn preflight(manifest: &Manifest) -> Result<()> {
    let media = manifest
        .media
        .as_ref()
        .ok_or(Error::Invalid("decoded recording required"))?;
    if manifest.dropped_chunks != Some(0)
        || media.channels != 2
        || media.metadata_duration_delta_ms.unsigned_abs() > 1500
        || media.decoded_duration_ms == 0
        || media.decoded_duration_ms > 390_000
        || media.customer_activity_groups_ms.is_empty()
    {
        return Err(Error::Invalid("recording completeness unverified"));
    }
    let sources: Vec<_> = manifest
        .segments
        .iter()
        .filter(|s| s.speaker == "customer")
        .collect();
    if sources.is_empty()
        || sources.len() > 100
        || sources
            .iter()
            .any(|s| s.channel != 0 || tokens(&s.text).is_empty())
    {
        return Err(Error::Invalid("customer source identity"));
    }
    match manifest.product_end_reason.as_deref() {
        // Voice-agent turn status includes the agent reply. An explicit customer
        // Finish can interrupt that reply after a complete customer answer.
        Some("explicit_finish") => (),
        Some("explicit_stop")
            if sources.iter().all(|s| {
                s.turn_status == "completed" && !manifest.incomplete_turn_ids.contains(&s.turn_id)
            }) =>
        {
            ()
        }
        _ => return Err(Error::Invalid("recording completion unresolved")),
    }
    Ok(())
}

pub fn plan(manifest: &Manifest, transcript: &RecordedTranscript) -> Result<Vec<VerifiedRange>> {
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
    let mut previous_end = 0;
    for (index, word) in transcript.words.iter().enumerate() {
        if word.start < previous_end
            || word.start >= word.end
            || word.end > media.decoded_duration_ms
            || !word.confidence.is_finite()
            || word.confidence <= 0.0
            || word.confidence > 1.0
        {
            return Err(Error::Invalid("transcription word timing"));
        }
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
            .segments
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
            method: METHOD.into(),
            verified_by: format!("assemblyai:{}", transcript.id),
            verified_at: at,
        });
    }
    manifest.automatic_alignment = proofs;
    manifest.approval_eligible = true;
    manifest.media.as_mut().unwrap().alignment_proven = true;
    manifest.recording_validation = "independent_customer_stt_alignment_verified".into();
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
