use std::collections::HashSet;

use tau_proto::{AgentPromptId, ToolCallId};

/// Whether a deferred response interrupted an idle or awaiting turn phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DeferredPriorPhase {
    /// No visible submitted prompt was awaiting a response.
    Idle,
    /// A visible submitted prompt was awaiting a response.
    AwaitingFinal,
}

/// Exhaustive lifecycle phase for one agent's visible-turn notifications.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum AgentTurnPhase {
    /// No visible prompt is awaiting completion, with an optional started
    /// prompt id.
    Idle {
        /// Most recently started prompt id, when one arrived without a submit.
        current_prompt_id: Option<AgentPromptId>,
    },
    /// A visible prompt awaits its final response.
    AwaitingFinal {
        /// Current prompt id, when prompt-start has supplied it.
        current_prompt_id: Option<AgentPromptId>,
    },
    /// A final response waits for all background tool blockers to finish.
    DeferredFinal {
        /// Phase in effect before the final response arrived.
        prior: DeferredPriorPhase,
        /// Prompt id carried by the deferred final response.
        final_prompt_id: AgentPromptId,
        /// Independently overwriteable current prompt id.
        current_prompt_id: AgentPromptId,
        /// Exact deferred response text, including the literal empty string.
        response_text: String,
    },
    /// The current visible turn has emitted its completion hook.
    Completed {
        /// Prompt id retained for later termination mismatch checks.
        current_prompt_id: AgentPromptId,
    },
}

impl Default for AgentTurnPhase {
    fn default() -> Self {
        Self::Idle {
            current_prompt_id: None,
        }
    }
}

/// Per-agent visible-turn notification state.
#[derive(Default)]
pub(super) struct AgentTurnState {
    /// Exhaustive visible-turn lifecycle phase.
    phase: AgentTurnPhase,
    /// Last visible user prompt text supplied to turn-aware hook templates.
    last_user_prompt: String,
    /// User-originated background tool calls still blocking completion.
    active_background_tools: HashSet<ToolCallId>,
}

impl AgentTurnState {
    /// Begin a visible prompt unless one is already awaiting its final
    /// response.
    pub(super) fn begin_prompt(&mut self, text: String) -> bool {
        if self.is_waiting_for_final_response() {
            return false;
        }
        self.phase = AgentTurnPhase::AwaitingFinal {
            current_prompt_id: None,
        };
        self.last_user_prompt = text;
        true
    }

    /// Record a user-originated prompt-start id in every admitted lifecycle
    /// phase.
    pub(super) fn record_prompt_started(&mut self, prompt_id: AgentPromptId) {
        match &mut self.phase {
            AgentTurnPhase::Idle { current_prompt_id }
            | AgentTurnPhase::AwaitingFinal { current_prompt_id } => {
                *current_prompt_id = Some(prompt_id);
            }
            AgentTurnPhase::DeferredFinal {
                current_prompt_id, ..
            }
            | AgentTurnPhase::Completed { current_prompt_id } => *current_prompt_id = prompt_id,
        }
    }

    /// Defer an exact final response behind this agent's background blockers.
    pub(super) fn defer_final_response(&mut self, prompt_id: AgentPromptId, response_text: String) {
        let prior = if self.is_waiting_for_final_response() {
            DeferredPriorPhase::AwaitingFinal
        } else {
            DeferredPriorPhase::Idle
        };
        self.phase = AgentTurnPhase::DeferredFinal {
            prior,
            final_prompt_id: prompt_id.clone(),
            current_prompt_id: prompt_id,
            response_text,
        };
    }

    /// Clear notification lifecycle state while preserving prompt text and
    /// blockers.
    pub(super) fn terminate_prompt_preserving_backgrounds(&mut self) {
        self.phase = AgentTurnPhase::default();
    }

    /// Cancel a deferred response and return its final prompt id, if present.
    pub(super) fn cancel_deferred_final_response(&mut self) -> Option<AgentPromptId> {
        let AgentTurnPhase::DeferredFinal {
            final_prompt_id, ..
        } = &self.phase
        else {
            return None;
        };
        let final_prompt_id = final_prompt_id.clone();
        self.phase = AgentTurnPhase::default();
        Some(final_prompt_id)
    }

    /// Return whether a visible submit is still awaiting a final response.
    pub(super) fn is_waiting_for_final_response(&self) -> bool {
        matches!(
            self.phase,
            AgentTurnPhase::AwaitingFinal { .. }
                | AgentTurnPhase::DeferredFinal {
                    prior: DeferredPriorPhase::AwaitingFinal,
                    ..
                }
        )
    }

    /// Return whether a completion hook has already been emitted.
    pub(super) fn is_completed(&self) -> bool {
        matches!(self.phase, AgentTurnPhase::Completed { .. })
    }

    /// Return the current prompt id used by termination mismatch checks.
    pub(super) fn current_prompt_id(&self) -> Option<&AgentPromptId> {
        match &self.phase {
            AgentTurnPhase::Idle { current_prompt_id }
            | AgentTurnPhase::AwaitingFinal { current_prompt_id } => current_prompt_id.as_ref(),
            AgentTurnPhase::DeferredFinal {
                current_prompt_id, ..
            }
            | AgentTurnPhase::Completed { current_prompt_id } => Some(current_prompt_id),
        }
    }

    /// Return whether the deferred final belongs to the given prompt.
    pub(super) fn deferred_final_matches(&self, prompt_id: &AgentPromptId) -> bool {
        matches!(
            &self.phase,
            AgentTurnPhase::DeferredFinal {
                final_prompt_id,
                ..
            } if final_prompt_id == prompt_id
        )
    }

    /// Borrow the exact deferred final response when blockers have drained.
    pub(super) fn ready_deferred_final(&self) -> Option<(&AgentPromptId, &str)> {
        if !self.active_background_tools.is_empty() {
            return None;
        }
        match &self.phase {
            AgentTurnPhase::DeferredFinal {
                final_prompt_id,
                response_text,
                ..
            } => Some((final_prompt_id, response_text)),
            _ => None,
        }
    }

    /// Mark the current prompt completed after its end hook succeeds.
    pub(super) fn mark_completed(&mut self) {
        let current_prompt_id = self
            .current_prompt_id()
            .expect("completion always records a current prompt id")
            .clone();
        self.phase = AgentTurnPhase::Completed { current_prompt_id };
    }

    /// Borrow the last visible user prompt text.
    pub(super) fn last_user_prompt(&self) -> &str {
        &self.last_user_prompt
    }

    /// Return whether this agent has no active background blockers.
    pub(super) fn background_tools_are_empty(&self) -> bool {
        self.active_background_tools.is_empty()
    }

    /// Return the number of active background blockers.
    pub(super) fn background_tool_count(&self) -> usize {
        self.active_background_tools.len()
    }

    /// Add one active background blocker.
    pub(super) fn insert_background_tool(&mut self, call_id: ToolCallId) {
        self.active_background_tools.insert(call_id);
    }

    /// Remove one active background blocker.
    pub(super) fn remove_background_tool(&mut self, call_id: &ToolCallId) {
        self.active_background_tools.remove(call_id);
    }

    /// Borrow the exact lifecycle phase for direct transition tests.
    #[cfg(test)]
    pub(super) fn phase(&self) -> &AgentTurnPhase {
        &self.phase
    }
}
