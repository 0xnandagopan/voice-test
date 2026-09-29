//! Bounded local decoding establishes file integrity and available audio ranges.
//! It does not establish transcript meaning or automatically authorize approval.
use crate::{Error, Result, manifest::Manifest};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};
use uuid::Uuid;

const SAMPLE_RATE: usize = 24_000;
const MAX_PCM_BYTES: u64 = 390 * SAMPLE_RATE as u64 * 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeCheck {
    pub source_id: String,
    pub channel: u32,
    pub within_recording: bool,
    pub audible_samples: u64,
    pub total_samples: u64,
    pub status: String,
    pub candidate_source_range_ms: Option<[u64; 2]>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaReport {
    pub codec: String,
    pub channels: u32,
    pub sample_rate: u32,
    pub decoded_duration_ms: u64,
    pub metadata_duration_delta_ms: i64,
    pub ranges: Vec<RangeCheck>,
    pub alignment_proven: bool,
    pub customer_activity_groups_ms: Vec<[u64; 2]>,
}

pub struct CandidateClip {
    pub wav: Vec<u8>,
    pub source_id: String,
    pub source_range_ms: [u64; 2],
    pub source_recording_sha256: String,
}

pub struct FfmpegValidator {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    work_root: PathBuf,
}
struct JobFiles(PathBuf);
impl Drop for JobFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
impl FfmpegValidator {
    pub fn new(
        ffmpeg: impl Into<PathBuf>,
        ffprobe: impl Into<PathBuf>,
        work_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            ffprobe: ffprobe.into(),
            work_root: work_root.into(),
        }
    }
    pub async fn validate(&self, audio: &[u8], manifest: &Manifest) -> Result<MediaReport> {
        let pcm = self.decode(audio).await?;
        Ok(check_pcm(&pcm, manifest))
    }
    /// Full customer-only recording for independent STT, with the original clock.
    /// Never concatenate activity groups: doing so would invalidate word timestamps.
    pub async fn customer_wav(&self, audio: &[u8], manifest: &Manifest) -> Result<Vec<u8>> {
        if crate::manifest::digest(audio) != manifest.recording_sha256 {
            return Err(Error::Invalid("source recording hash mismatch"));
        }
        let pcm = self.decode(audio).await?;
        Ok(left_channel_wav(&pcm))
    }
    /// A physically bounded PRIVATE review clip. This does not authorize publication.
    /// The caller must authorize the current interview before and after awaiting it.
    pub async fn candidate_clip(
        &self,
        audio: &[u8],
        manifest: &Manifest,
        source_id: &str,
    ) -> Result<CandidateClip> {
        if crate::manifest::digest(audio) != manifest.recording_sha256 {
            return Err(Error::Invalid("source recording hash mismatch"));
        }
        let segment = manifest
            .segments
            .iter()
            .find(|s| s.source_id == source_id && s.speaker == "customer")
            .ok_or(Error::Invalid("customer source identity"))?;
        let [start, end] = manifest
            .media
            .as_ref()
            .and_then(|m| m.ranges.iter().find(|r| r.source_id == segment.source_id))
            .and_then(|r| r.candidate_source_range_ms)
            .ok_or(Error::Invalid("source group unavailable"))?;
        self.preview_range(audio, manifest, source_id, [start, end])
            .await
    }

    /// Private operator preview. The caller authorizes access before and after I/O.
    /// The range is contiguous, bounded and always uses the customer (left) channel.
    pub async fn preview_range(
        &self,
        audio: &[u8],
        manifest: &Manifest,
        source_id: &str,
        [start, end]: [u64; 2],
    ) -> Result<CandidateClip> {
        if crate::manifest::digest(audio) != manifest.recording_sha256 {
            return Err(Error::Invalid("source recording hash mismatch"));
        }
        if !manifest
            .segments
            .iter()
            .any(|s| s.source_id == source_id && s.speaker == "customer" && s.channel == 0)
        {
            return Err(Error::Invalid("customer source identity"));
        }
        if start >= end || end > 390_000 {
            return Err(Error::Invalid("clip range outside decoded recording"));
        }
        let pcm = self.decode(audio).await?;
        let from = (start * SAMPLE_RATE as u64 / 1000) as usize;
        let to = (end * SAMPLE_RATE as u64 / 1000) as usize;
        if from >= to || to > pcm.len() / 4 {
            return Err(Error::Invalid("clip range outside decoded recording"));
        }
        let wav = left_channel_wav(&pcm[from * 4..to * 4]);
        Ok(CandidateClip {
            wav,
            source_id: source_id.into(),
            source_range_ms: [start, end],
            source_recording_sha256: manifest.recording_sha256.clone(),
        })
    }
    /// A verified whole-answer clip; this is still not publication authorization.
    pub async fn verified_clip(
        &self,
        audio: &[u8],
        manifest: &Manifest,
        source_id: &str,
    ) -> Result<CandidateClip> {
        let proof = crate::alignment::source_proof(manifest, source_id)
            .ok_or(Error::Invalid("source alignment is unverified"))?;
        let clip = self
            .preview_range(audio, manifest, source_id, proof.source_range_ms)
            .await?;
        if crate::manifest::digest(&clip.wav) != proof.clip_sha256 {
            return Err(Error::Invalid("verified clip hash mismatch"));
        }
        Ok(clip)
    }
    async fn decode(&self, audio: &[u8]) -> Result<Vec<u8>> {
        if audio.is_empty() || audio.len() > 32 * 1024 * 1024 {
            return Err(Error::Invalid("recording size limit"));
        }
        tokio::fs::create_dir_all(&self.work_root).await?;
        let dir = self.work_root.join(Uuid::new_v4().to_string());
        tokio::fs::create_dir(&dir).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let files = JobFiles(dir);
        let input = files.0.join("source.ogg");
        let output = files.0.join("decoded.pcm");
        tokio::fs::write(&input, audio).await?;
        let probe = self.probe(&input).await?;
        let streams = probe
            .get("streams")
            .and_then(|v| v.as_array())
            .ok_or(Error::Invalid("media stream metadata"))?;
        let stream = streams
            .first()
            .filter(|_| streams.len() == 1)
            .ok_or(Error::Invalid("recording audio stream"))?;
        if stream["codec_name"] != "opus" || stream["channels"] != 2 {
            return Err(Error::Invalid("decoded recording format"));
        }
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-xerror",
            "-max_alloc",
            "67108864",
            "-protocol_whitelist",
            "file,pipe",
            "-threads",
            "1",
            "-i",
        ])
        .arg(&input)
        .args([
            "-map", "0:a:0", "-vn", "-sn", "-dn", "-ac", "2", "-ar", "24000", "-t", "391", "-fs",
        ])
        .arg((MAX_PCM_BYTES + 4).to_string())
        .args(["-f", "s16le"])
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
        let status = tokio::time::timeout(Duration::from_secs(20), cmd.status())
            .await
            .map_err(|_| Error::Invalid("media decode timeout"))??;
        if !status.success() {
            return Err(Error::Invalid("recording decode failed"));
        }
        let size = tokio::fs::metadata(&output).await?.len();
        if size == 0 || size > MAX_PCM_BYTES || size % 4 != 0 {
            return Err(Error::Invalid("decoded recording size limit"));
        }
        Ok(tokio::fs::read(&output).await?)
    }
    async fn probe(&self, input: &Path) -> Result<serde_json::Value> {
        let mut cmd = Command::new(&self.ffprobe);
        cmd.args([
            "-v",
            "error",
            "-max_alloc",
            "67108864",
            "-protocol_whitelist",
            "file,pipe",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_name,channels,sample_rate",
            "-of",
            "json",
        ])
        .arg(input)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let mut reader = child
            .stdout
            .take()
            .ok_or(Error::Invalid("media probe pipe"))?
            .take(65_537);
        let operation = async {
            let mut data = Vec::new();
            reader.read_to_end(&mut data).await?;
            if data.len() > 65_536 {
                return Err(Error::Invalid("media probe output limit"));
            }
            if !child.wait().await?.success() {
                return Err(Error::Invalid("media probe failed"));
            }
            Ok(serde_json::from_slice(&data)?)
        };
        tokio::time::timeout(Duration::from_secs(10), operation)
            .await
            .map_err(|_| Error::Invalid("media probe timeout"))?
    }
}

pub fn check_pcm(pcm: &[u8], manifest: &Manifest) -> MediaReport {
    let frames = pcm.len() / 4;
    let duration = (frames * 1000 / SAMPLE_RATE) as u64;
    let activity = customer_activity_groups(pcm);
    let ranges = manifest
        .segments
        .iter()
        .map(|segment| {
            let Some([start, end]) = segment.source_range_ms else {
                return RangeCheck {
                    source_id: segment.source_id.clone(),
                    channel: segment.channel,
                    within_recording: false,
                    audible_samples: 0,
                    total_samples: 0,
                    status: "missing_alignment".into(),
                    candidate_source_range_ms: None,
                };
            };
            let from = (start * SAMPLE_RATE as u64 / 1000) as usize;
            let to = (end * SAMPLE_RATE as u64 / 1000) as usize;
            let within = from < to && to <= frames && segment.channel < 2;
            let audible = if within {
                pcm[from * 4..to * 4]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|frame| {
                        let offset = segment.channel as usize * 2;
                        i16::from_le_bytes([frame[offset], frame[offset + 1]]).unsigned_abs() > 128
                    })
                    .count() as u64
            } else {
                0
            };
            RangeCheck {
                source_id: segment.source_id.clone(),
                channel: segment.channel,
                within_recording: within,
                audible_samples: audible,
                total_samples: to.saturating_sub(from) as u64,
                candidate_source_range_ms: if segment.channel == 0 {
                    let overlapping: Vec<_> = activity
                        .iter()
                        .filter(|[a, b]| *a < end && *b > start)
                        .collect();
                    if overlapping.len() == 1 {
                        Some(*overlapping[0])
                    } else {
                        None
                    }
                } else {
                    None
                },
                status: if !within {
                    "outside_decoded_recording"
                } else if audible == 0 {
                    "no_audible_samples"
                } else {
                    "decoded_range_unverified"
                }
                .into(),
            }
        })
        .collect();
    MediaReport {
        codec: "opus".into(),
        channels: 2,
        sample_rate: SAMPLE_RATE as u32,
        decoded_duration_ms: duration,
        metadata_duration_delta_ms: duration as i64 - manifest.duration_ms as i64,
        ranges,
        alignment_proven: false,
        customer_activity_groups_ms: activity,
    }
}

/// Conservative activity grouping, not speech recognition or transcript alignment.
/// Ranges derive from decoded left-channel samples; provider receipt timestamps
/// never establish their boundaries. Gaps <=800ms remain one contiguous group.
fn customer_activity_groups(pcm: &[u8]) -> Vec<[u64; 2]> {
    let frames = pcm.len() / 4;
    let mut groups: Vec<[u64; 2]> = vec![];
    for (index, window) in pcm.chunks(480 * 4).enumerate() {
        let samples = window.as_chunks::<4>().0;
        if samples.is_empty() {
            continue;
        }
        let energy: u64 = samples
            .iter()
            .map(|f| {
                let s = i16::from_le_bytes([f[0], f[1]]) as i64;
                (s * s) as u64
            })
            .sum();
        if energy / (samples.len() as u64) < 128 * 128 {
            continue;
        }
        let start = index as u64 * 20;
        let end = ((index * 480 + samples.len()) * 1000 / SAMPLE_RATE) as u64;
        if let Some(last) = groups.last_mut().filter(|last| start <= last[1] + 800) {
            last[1] = end;
        } else {
            groups.push([start, end]);
        }
    }
    for group in &mut groups {
        group[0] = group[0].saturating_sub(120);
        group[1] = (group[1] + 120).min((frames * 1000 / SAMPLE_RATE) as u64);
    }
    groups
}

fn left_channel_wav(pcm: &[u8]) -> Vec<u8> {
    let data_len = (pcm.len() / 2) as u32;
    let mut wav = Vec::with_capacity(data_len as usize + 44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(data_len + 36).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE as u32).to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE as u32 * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for frame in pcm.as_chunks::<4>().0 {
        wav.extend_from_slice(&frame[..2]);
    }
    wav
}
