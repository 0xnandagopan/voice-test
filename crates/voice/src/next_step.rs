//! Candidate tool controller. This enforces returned allowances, not the provider's
//! decision to call it before speaking. Keep live routing disabled until G1 proves
//! that stronger property. Drain ONLY in the matching reply.done handler.
use crate::{
    controller::{Progress, Step},
    pre_speech::{FollowupKind, QuestionPlan},
    protocol::ClientEvent,
};
use serde_json::json;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct NextStepGate {
    pending: HashMap<String, (bool, FollowupKind)>,
    answered: HashSet<String>,
    latest_answer: Option<String>,
    completed: HashMap<String, ClientEvent>,
}
impl NextStepGate {
    /// Final user text, never a provisional caption. Empty/silence does not advance.
    pub fn record_answer(&mut self, item_id: &str, text: &str) {
        if !item_id.is_empty() && !text.trim().is_empty() && !self.answered.contains(item_id) {
            self.latest_answer = Some(item_id.into());
        }
    }
    pub fn queue(&mut self, call_id: &str, follow_up: bool) {
        self.queue_classified(call_id, follow_up, FollowupKind::Detail);
    }
    pub fn queue_classified(&mut self, call_id: &str, follow_up: bool, kind: FollowupKind) {
        if !call_id.is_empty() && !self.completed.contains_key(call_id) {
            self.pending
                .entry(call_id.into())
                .or_insert((follow_up, kind));
        }
    }
    /// Persist the returned progress/question decision before sending this result.
    pub fn reply_done(
        &mut self,
        reply_id: &str,
        interrupted: bool,
        progress: &mut Progress,
    ) -> Option<ClientEvent> {
        let call_id = reply_id.strip_prefix("fc-")?;
        if interrupted {
            self.pending.remove(call_id);
            return None;
        }
        if let Some(result) = self.completed.get(call_id) {
            return Some(result.clone());
        }
        let (follow_up, kind) = self.pending.remove(call_id)?;
        let has_answer = self
            .latest_answer
            .take()
            .is_some_and(|id| self.answered.insert(id));
        let question = if has_answer {
            let plan = QuestionPlan::after_answer(progress.clone(), follow_up, kind).ok()?;
            *progress = plan.progress;
            Some(plan.code.text())
        } else {
            None
        };
        let step = progress.current();
        let event=ClientEvent::ToolResult {call_id:call_id.into(), result:json!({"step":step,"question":question,"must_speak_exactly":true,"followups":progress.followups,"remaining_millis":360_000u64.saturating_sub(progress.consumed_millis),"may_ask":has_answer && step != Step::Complete,"reason":if has_answer {"bounded_next_step"} else {"no_new_complete_answer"}}).to_string(), is_error:!has_answer};
        self.completed.insert(call_id.into(), event.clone());
        Some(event)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn waits_for_matching_reply_done_and_counts_each_answer_once() {
        let mut gate = NextStepGate::default();
        let mut progress = Progress::default();
        gate.record_answer("u1", "uncertain result");
        gate.queue("c1", true);
        assert!(gate.reply_done("other", false, &mut progress).is_none());
        assert_eq!(progress.followups, [0, 0, 0]);
        assert!(gate.reply_done("fc-c1", false, &mut progress).is_some());
        assert_eq!(progress.followups, [1, 0, 0]);
        gate.reply_done("fc-c1", false, &mut progress);
        assert_eq!(progress.followups, [1, 0, 0]);
        gate.record_answer("u1", "uncertain result");
        gate.queue("c2", true);
        gate.reply_done("fc-c2", false, &mut progress);
        assert_eq!(progress.followups, [1, 0, 0]);
    }
    #[test]
    fn silence_and_interrupted_tool_reply_do_not_advance() {
        let mut gate = NextStepGate::default();
        let mut progress = Progress::default();
        gate.record_answer("u1", "  ");
        gate.queue("c", true);
        gate.reply_done("fc-c", false, &mut progress);
        assert_eq!(progress.followups, [0, 0, 0]);
        gate.record_answer("u2", "answer");
        gate.queue("d", true);
        assert!(gate.reply_done("fc-d", true, &mut progress).is_none());
        assert_eq!(progress.followups, [0, 0, 0]);
    }
}
