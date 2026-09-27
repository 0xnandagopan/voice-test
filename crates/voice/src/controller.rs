use crate::protocol::{ClientEvent, ProviderEvent};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

pub const MAX_MILLIS: u64 = 360_000;
pub const NATIVE_RESUME_MILLIS: u64 = 30_000;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum VoiceError {
    #[error("recording consent is required")]
    Consent,
    #[error("interview has ended or its allowance is exhausted")]
    Ended,
    #[error("provider identity must be durably recorded before audio")]
    MappingRequired,
    #[error("provider identity changed unexpectedly")]
    IdentityMismatch,
    #[error("recovery of completed evidence is required")]
    RecoveryRequired,
    #[error("invalid persisted interview progress")]
    InvalidProgress,
}

/// Persist inside the same lease/fencing transaction as each authorized command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Progress {
    pub topic: u8,
    pub followups: [u8; 3],
    pub consumed_millis: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Problem,
    Change,
    Result,
    Complete,
}
impl Progress {
    pub fn validate(&self) -> Result<(), VoiceError> {
        if self.topic > 3
            || self.followups.iter().any(|n| *n > 2)
            || self.consumed_millis > MAX_MILLIS
        {
            return Err(VoiceError::InvalidProgress);
        }
        Ok(())
    }
    pub fn current(&self) -> Step {
        if self.consumed_millis >= MAX_MILLIS {
            return Step::Complete;
        }
        match self.topic {
            0 => Step::Problem,
            1 => Step::Change,
            2 => Step::Result,
            _ => Step::Complete,
        }
    }
    /// Called only for a completed nonempty answer; the model cannot reset counts.
    pub fn after_answer(&mut self, follow_up_requested: bool) -> Step {
        if self.current() == Step::Complete {
            return Step::Complete;
        }
        let count = &mut self.followups[self.topic as usize];
        if follow_up_requested && *count < 2 {
            *count += 1;
        } else {
            self.topic += 1;
        }
        self.current()
    }
    pub fn skip(&mut self) -> Step {
        self.topic = (self.topic + 1).min(3);
        self.current()
    }
    pub fn repeat(&self) -> Step {
        self.current()
    }
    /// Charge monotonic elapsed time, including speaking/playback/silence, not PCM length.
    pub fn charge(&mut self, elapsed_millis: u64) -> bool {
        self.consumed_millis = self
            .consumed_millis
            .saturating_add(elapsed_millis)
            .min(MAX_MILLIS);
        self.current() != Step::Complete
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    AwaitingPersistence(String),
    Active(String),
    Disconnected { session_id: String, at_millis: u64 },
    Recovering,
    Ended,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    PersistMapping(String),
    ClearPlayback,
    RecoverEvidence,
    RetryLater,
    FatalProviderError,
    Ended,
    None,
}

/// Captions are transient UI state, never recording-backed evidence. Final agent
/// transcript replaces provisional words, including after an interruption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caption {
    pub text: String,
    pub final_: bool,
    pub interrupted: bool,
}
pub struct SessionController {
    pub progress: Progress,
    pub state: ConnectionState,
    pub captions: BTreeMap<String, Caption>,
    expected_session: Option<String>,
}
impl SessionController {
    pub fn new(consented: bool, progress: Progress) -> Result<Self, VoiceError> {
        if !consented {
            return Err(VoiceError::Consent);
        }
        progress.validate()?;
        if progress.current() == Step::Complete {
            return Err(VoiceError::Ended);
        }
        Ok(Self {
            progress,
            state: ConnectionState::Connecting,
            captions: BTreeMap::new(),
            expected_session: None,
        })
    }
    pub fn accept(&mut self, event: ProviderEvent) -> Result<Effect, VoiceError> {
        if matches!(self.state, ConnectionState::Ended) {
            return Err(VoiceError::Ended);
        }
        match event {
            ProviderEvent::Ready { session_id } => {
                if session_id.is_empty()
                    || self
                        .expected_session
                        .as_ref()
                        .is_some_and(|id| id != &session_id)
                {
                    self.state = ConnectionState::Recovering;
                    return Err(VoiceError::IdentityMismatch);
                }
                self.expected_session = Some(session_id.clone());
                self.state = ConnectionState::AwaitingPersistence(session_id.clone());
                Ok(Effect::PersistMapping(session_id))
            }
            ProviderEvent::Ended => {
                self.state = ConnectionState::Ended;
                Ok(Effect::Ended)
            }
            ProviderEvent::Error { code } => {
                self.state = ConnectionState::Recovering;
                Ok(match code.as_str() {
                    "session_not_found" | "session_forbidden" | "session_expired" => {
                        Effect::RecoverEvidence
                    }
                    "at_capacity" | "concurrency_exceeded" | "internal_error" => Effect::RetryLater,
                    _ => Effect::FatalProviderError,
                })
            }
            ProviderEvent::ReplyDone { status, .. } if status == "interrupted" => {
                Ok(Effect::ClearPlayback)
            }
            ProviderEvent::UserDelta { item_id, text } => {
                self.caption(item_id, text, false, false);
                Ok(Effect::None)
            }
            ProviderEvent::UserFinal { item_id, text } => {
                self.caption(item_id, text, true, false);
                Ok(Effect::None)
            }
            ProviderEvent::AgentDelta { item_id, delta, .. } => {
                let c = self.captions.entry(item_id).or_insert(Caption {
                    text: String::new(),
                    final_: false,
                    interrupted: false,
                });
                if !c.final_ {
                    if !c.text.is_empty() {
                        c.text.push(' ');
                    }
                    c.text.push_str(&delta);
                }
                Ok(Effect::None)
            }
            ProviderEvent::AgentFinal {
                item_id,
                text,
                interrupted,
                ..
            } => {
                self.caption(item_id, text, true, interrupted);
                Ok(if interrupted {
                    Effect::ClearPlayback
                } else {
                    Effect::None
                })
            }
            // SpeechStarted is deliberately not an interruption: back-channels keep playback.
            _ => Ok(Effect::None),
        }
    }
    fn caption(&mut self, id: String, text: String, final_: bool, interrupted: bool) {
        if !final_ && self.captions.get(&id).is_some_and(|c| c.final_) {
            return;
        }
        self.captions.insert(
            id,
            Caption {
                text,
                final_,
                interrupted,
            },
        );
    }
    pub fn mapping_persisted(&mut self, session_id: &str) -> Result<(), VoiceError> {
        if self.state != ConnectionState::AwaitingPersistence(session_id.into()) {
            return Err(VoiceError::IdentityMismatch);
        }
        self.state = ConnectionState::Active(session_id.into());
        Ok(())
    }
    pub fn input_audio(&self, base64_pcm: String) -> Result<ClientEvent, VoiceError> {
        if self.progress.current() == Step::Complete {
            return Err(VoiceError::Ended);
        }
        if !matches!(self.state, ConnectionState::Active(_)) {
            return Err(VoiceError::MappingRequired);
        }
        Ok(ClientEvent::Audio { audio: base64_pcm })
    }
    pub fn disconnected(&mut self, now_millis: u64) {
        if let ConnectionState::Active(id) | ConnectionState::AwaitingPersistence(id) = &self.state
        {
            self.state = ConnectionState::Disconnected {
                session_id: id.clone(),
                at_millis: now_millis,
            };
        } else if self.state != ConnectionState::Ended {
            self.state = ConnectionState::Recovering;
        }
    }
    pub fn resume(&mut self, now_millis: u64) -> Result<ClientEvent, VoiceError> {
        if let ConnectionState::Disconnected {
            session_id,
            at_millis,
        } = &self.state
        {
            let elapsed = now_millis.saturating_sub(*at_millis);
            let id = session_id.clone();
            let allowance_remains = self.progress.charge(elapsed);
            if elapsed < NATIVE_RESUME_MILLIS && allowance_remains {
                self.state = ConnectionState::Connecting;
                return Ok(ClientEvent::Resume { session_id: id });
            }
        }
        if self.state == ConnectionState::Ended {
            return Err(VoiceError::Ended);
        }
        self.state = ConnectionState::Recovering;
        Err(VoiceError::RecoveryRequired)
    }
    pub fn stop(&mut self) -> ClientEvent {
        self.state = ConnectionState::Ended;
        ClientEvent::End
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counts_cannot_be_reset_by_repeat_or_reconnect() {
        let mut p = Progress::default();
        for topic in 0..3 {
            assert_eq!(p.topic, topic);
            p.after_answer(true);
            p.repeat();
            p.after_answer(true);
            p.after_answer(true);
        }
        assert_eq!(p.followups, [2, 2, 2]);
        assert_eq!(p.current(), Step::Complete);
        let restored: Progress = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(p, restored);
    }
    #[test]
    fn cap_includes_silence_and_never_overflows() {
        let mut p = Progress::default();
        assert!(p.charge(359_999));
        assert!(!p.charge(u64::MAX));
        assert_eq!(p.consumed_millis, MAX_MILLIS);
    }
    #[test]
    fn no_capture_without_consent_or_durable_mapping() {
        assert!(matches!(
            SessionController::new(false, Progress::default()),
            Err(VoiceError::Consent)
        ));
        let mut c = SessionController::new(true, Progress::default()).unwrap();
        assert!(c.input_audio("AA==".into()).is_err());
        c.accept(ProviderEvent::Ready {
            session_id: "one".into(),
        })
        .unwrap();
        assert!(c.input_audio("AA==".into()).is_err());
        c.mapping_persisted("one").unwrap();
        assert!(c.input_audio("AA==".into()).is_ok());
        c.disconnected(100);
        assert!(matches!(c.resume(300), Ok(ClientEvent::Resume { .. })));
        assert_eq!(c.progress.consumed_millis, 200);
        c.accept(ProviderEvent::Ready {
            session_id: "one".into(),
        })
        .unwrap();
        c.mapping_persisted("one").unwrap();
        c.stop();
        assert!(matches!(c.resume(400), Err(VoiceError::Ended)));
    }
    #[test]
    fn expired_resume_does_not_create_empty_attempt() {
        let mut c = SessionController::new(true, Progress::default()).unwrap();
        c.accept(ProviderEvent::Ready {
            session_id: "s".into(),
        })
        .unwrap();
        c.disconnected(0);
        assert!(matches!(
            c.resume(30_000),
            Err(VoiceError::RecoveryRequired)
        ));
    }
    #[test]
    fn mismatched_identity_blocks_audio() {
        let mut c = SessionController::new(true, Progress::default()).unwrap();
        c.accept(ProviderEvent::Ready {
            session_id: "a".into(),
        })
        .unwrap();
        assert_eq!(
            c.accept(ProviderEvent::Ready {
                session_id: "b".into()
            }),
            Err(VoiceError::IdentityMismatch)
        );
        assert!(c.input_audio("x".into()).is_err());
    }
    #[test]
    fn semantic_interruption_and_final_spoken_text() {
        let mut c = SessionController::new(true, Progress::default()).unwrap();
        assert_eq!(
            c.accept(ProviderEvent::SpeechStarted).unwrap(),
            Effect::None
        );
        c.accept(ProviderEvent::AgentDelta {
            item_id: "a".into(),
            reply_id: "r".into(),
            delta: "too much".into(),
        })
        .unwrap();
        assert_eq!(
            c.accept(ProviderEvent::AgentFinal {
                item_id: "a".into(),
                reply_id: "r".into(),
                text: "too".into(),
                interrupted: true
            })
            .unwrap(),
            Effect::ClearPlayback
        );
        assert_eq!(c.captions["a"].text, "too");
    }
}
