//! Runtime-only conservative loop guard state for loaded agents.

use std::collections::VecDeque;

use tau_proto::ToolCallId;

/// Runtime-only conservative loop detector state for one loaded agent branch.
#[derive(Debug, Default)]
pub(crate) struct LoopGuardState {
    /// Bounded FIFO of recent turn signatures for suffix/repetition detection.
    recent: VecDeque<LoopTurnSignature>,
    /// Bounded FIFO of loop cycles that already received a breaker or block.
    cycles: VecDeque<LoopCycleRecord>,
    /// Consecutive terminal tool failures since the last successful tool result
    /// or user input.
    consecutive_tool_failures: u8,
    /// Bounded FIFO of recent terminal tool-failure signatures.
    recent_tool_failures: VecDeque<String>,
    /// Bounded FIFO mapping in-flight call ids to normalized tool+argument
    /// signatures for repeated-failing-call detection.
    tool_call_signatures: VecDeque<LoopToolCallSignature>,
    /// Set after a breaker was dispatched and the same cycle continued. The
    /// next automatic continuation is suppressed until clear user/tool
    /// progress.
    stop_automatic_continuation: bool,
}

/// Bounded record of a detected loop cycle and breaker lifecycle state.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LoopCycleRecord {
    /// Stable key for one detected cycle. Kept bounded by `LoopGuardState`.
    key: String,
    /// Where this cycle is in the breaker lifecycle.
    state: LoopCycleState,
}

/// Breaker lifecycle for one loop cycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LoopCycleState {
    /// Breaker prompt has been queued but not folded into the transcript yet.
    BreakerPending,
    /// Breaker prompt was folded/dispatched; the next same-cycle trigger
    /// blocks.
    BreakerDispatched,
    /// Same cycle continued after the breaker; automatic continuation is
    /// stopped.
    Blocked,
}

/// Compact signature for one in-flight tool call, used when a later error
/// arrives so identical-failure detection can include normalized arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LoopToolCallSignature {
    /// Tool call id this signature belongs to.
    call_id: ToolCallId,
    /// Bounded `tool-name:normalized-arguments` signature captured at call
    /// intake.
    signature: String,
}

/// Detected loop guard trigger.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LoopGuardTrigger {
    /// Stable key identifying the repeated cycle.
    pub(crate) cycle_key: String,
    /// Short human-readable reason inserted into the pivot prompt and notice.
    pub(crate) reason: String,
}

/// Compact recent turn signature used by the loop guard.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LoopTurnSignature {
    /// Normalized assistant text when a response contains no concrete action.
    AssistantText(String),
    /// Normalized terminal tool failure signature.
    ToolFailure(String),
}

impl LoopGuardState {
    /// Clear detector and breaker history after clear progress.
    ///
    /// In-flight tool-call argument signatures are intentionally preserved:
    /// a successful result in a multi-tool turn must not erase argument
    /// metadata for sibling calls that can still fail later.
    pub(crate) fn reset_for_progress(&mut self) {
        self.recent.clear();
        self.cycles.clear();
        self.consecutive_tool_failures = 0;
        self.recent_tool_failures.clear();
        self.stop_automatic_continuation = false;
    }

    /// Clear all branch-local loop-guard state after a non-linear head move.
    ///
    /// Unlike progress reset, branch invalidation also drops in-flight tool
    /// signatures because they were captured against the old branch cursor.
    pub(crate) fn invalidate_branch(&mut self) {
        self.reset_for_progress();
        self.tool_call_signatures.clear();
    }

    /// Append one recent turn signature and enforce the bounded FIFO limit.
    pub(crate) fn push_recent(&mut self, signature: LoopTurnSignature, limit: usize) {
        self.recent.push_back(signature);
        while self.recent.len() > limit {
            self.recent.pop_front();
        }
    }

    /// Return whether the most recent `threshold` signatures all equal
    /// `signature`.
    pub(crate) fn recent_repeats(&self, signature: &LoopTurnSignature, threshold: usize) -> bool {
        self.recent.len() >= threshold
            && self
                .recent
                .iter()
                .rev()
                .take(threshold)
                .all(|sig| sig == signature)
    }

    /// Return the last two alternating signatures when the recent suffix is
    /// A/B/A/B.
    pub(crate) fn abab_suffix(&self) -> Option<(&LoopTurnSignature, &LoopTurnSignature)> {
        if self.recent.len() < 4 {
            return None;
        }
        let len = self.recent.len();
        let a = &self.recent[len - 4];
        let b = &self.recent[len - 3];
        (a != b && a == &self.recent[len - 2] && b == &self.recent[len - 1]).then_some((a, b))
    }

    /// Record a terminal tool failure and enforce bounded failure history.
    pub(crate) fn push_tool_failure(&mut self, failure: String, limit: usize) {
        self.consecutive_tool_failures = self.consecutive_tool_failures.saturating_add(1);
        self.recent_tool_failures.push_back(failure);
        while self.recent_tool_failures.len() > limit {
            self.recent_tool_failures.pop_front();
        }
    }

    /// Return whether the most recent tool failures repeat the same signature.
    pub(crate) fn repeated_tool_failure(&self, failure: &str, threshold: usize) -> bool {
        self.recent_tool_failures.len() >= threshold
            && self
                .recent_tool_failures
                .iter()
                .rev()
                .take(threshold)
                .all(|sig| sig == failure)
    }

    /// Consecutive terminal tool failures since the last clear progress reset.
    pub(crate) fn consecutive_tool_failures(&self) -> u8 {
        self.consecutive_tool_failures
    }

    /// Remember bounded normalized arguments for an in-flight tool call.
    pub(crate) fn push_tool_call_signature(
        &mut self,
        call_id: ToolCallId,
        signature: String,
        limit: usize,
    ) {
        self.tool_call_signatures
            .push_back(LoopToolCallSignature { call_id, signature });
        while self.tool_call_signatures.len() > limit {
            self.tool_call_signatures.pop_front();
        }
    }

    /// Take and remove the remembered argument signature for a completed tool
    /// call.
    pub(crate) fn take_tool_call_signature(&mut self, call_id: &ToolCallId) -> Option<String> {
        let pos = self
            .tool_call_signatures
            .iter()
            .position(|known| &known.call_id == call_id)?;
        self.tool_call_signatures
            .remove(pos)
            .map(|known| known.signature)
    }

    /// Return the breaker lifecycle state for a detected cycle key.
    pub(crate) fn cycle_state(&self, key: &str) -> Option<LoopCycleState> {
        self.cycles
            .iter()
            .find(|record| record.key == key)
            .map(|record| record.state)
    }

    /// Mark a newly detected cycle as having a queued, not-yet-dispatched
    /// breaker.
    pub(crate) fn remember_cycle_pending(&mut self, key: String, limit: usize) {
        self.cycles.push_back(LoopCycleRecord {
            key,
            state: LoopCycleState::BreakerPending,
        });
        while self.cycles.len() > limit {
            self.cycles.pop_front();
        }
    }

    /// Advance all queued breaker records after their prompt was
    /// folded/dispatched.
    pub(crate) fn mark_pending_breakers_dispatched(&mut self) {
        for record in &mut self.cycles {
            if record.state == LoopCycleState::BreakerPending {
                record.state = LoopCycleState::BreakerDispatched;
            }
        }
    }

    /// Mark an already-dispatched cycle as blocked and stop automatic
    /// continuation.
    pub(crate) fn mark_cycle_blocked(&mut self, key: &str) {
        if let Some(record) = self.cycles.iter_mut().find(|record| record.key == key) {
            record.state = LoopCycleState::Blocked;
        }
        self.stop_automatic_continuation = true;
    }

    /// Whether the harness should suppress automatic self-continuation for now.
    pub(crate) fn stop_automatic_continuation(&self) -> bool {
        self.stop_automatic_continuation
    }
}

#[cfg(test)]
mod tests;
