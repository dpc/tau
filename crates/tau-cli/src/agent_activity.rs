//! Pure lifecycle state for deciding whether agent work is still in flight.

use std::collections::HashSet;

use tau_proto::{AgentPromptId, ContextItem, ToolCallId};

/// Session/agent lifecycle state used to decide whether Ctrl-D is safe.
#[derive(Clone, Default)]
pub(crate) struct AgentActivity {
    /// User submissions seen before the harness assigns a prompt id.
    optimistic_submissions: usize,
    /// Prompt ids currently being processed by any provider conversation.
    active_prompts: HashSet<AgentPromptId>,
    /// Tool call ids emitted by any agent and not finished yet.
    active_tools: HashSet<ToolCallId>,
    /// Tool call ids whose foreground provider protocol has completed with a
    /// synthetic placeholder, but whose real tool process is still running.
    backgrounded_tools: HashSet<ToolCallId>,
}

impl AgentActivity {
    /// Returns true while any prompt submission, provider turn, or requested
    /// tool is still active.
    pub(crate) fn is_in_progress(&self) -> bool {
        self.optimistic_submissions != 0
            || !self.active_prompts.is_empty()
            || !self.active_tools.is_empty()
    }

    /// Returns true while at least one provider prompt is active.
    pub(crate) fn has_active_prompts(&self) -> bool {
        !self.active_prompts.is_empty()
    }

    /// Records a local user submission before the harness assigns a prompt id.
    pub(crate) fn mark_optimistic_submission(&mut self) {
        self.optimistic_submissions = self.optimistic_submissions.saturating_add(1);
    }

    /// Moves one optimistic submission, if present, into an active prompt id.
    ///
    /// Repeated canonical progress updates retain the same active prompt
    /// without consuming another optimistic submission.
    pub(crate) fn start_prompt(&mut self, agent_prompt_id: &AgentPromptId) {
        if self.active_prompts.contains(agent_prompt_id) {
            return;
        }
        self.optimistic_submissions = self.optimistic_submissions.saturating_sub(1);
        self.active_prompts.insert(agent_prompt_id.clone());
    }

    /// Finishes a provider prompt and keeps the session busy if the response
    /// requested follow-up tools.
    pub(crate) fn finish_prompt(
        &mut self,
        agent_prompt_id: &AgentPromptId,
        output_items: &[ContextItem],
    ) {
        if self.finish_active_prompt(
            agent_prompt_id,
            tool_call_ids_from_output_items(output_items),
        ) {
            return;
        }
        self.optimistic_submissions = self.optimistic_submissions.saturating_sub(1);
    }

    /// Finishes a provider prompt using call ids from an existing terminal
    /// projection.
    pub(crate) fn finish_prompt_with_tool_call_ids<'a>(
        &mut self,
        agent_prompt_id: &AgentPromptId,
        call_ids: impl IntoIterator<Item = &'a ToolCallId>,
    ) {
        if self.finish_active_prompt(agent_prompt_id, call_ids) {
            return;
        }
        self.optimistic_submissions = self.optimistic_submissions.saturating_sub(1);
    }

    /// Finishes a provider prompt only if this snapshot already tracks it.
    #[cfg(test)]
    pub(crate) fn finish_prompt_if_active(
        &mut self,
        agent_prompt_id: &AgentPromptId,
        output_items: &[ContextItem],
    ) {
        let _ = self.finish_active_prompt(
            agent_prompt_id,
            tool_call_ids_from_output_items(output_items),
        );
    }

    /// Finishes a tracked provider prompt using an existing terminal
    /// projection.
    pub(crate) fn finish_prompt_if_active_with_tool_call_ids<'a>(
        &mut self,
        agent_prompt_id: &AgentPromptId,
        call_ids: impl IntoIterator<Item = &'a ToolCallId>,
    ) {
        let _ = self.finish_active_prompt(agent_prompt_id, call_ids);
    }

    fn finish_active_prompt<'a>(
        &mut self,
        agent_prompt_id: &AgentPromptId,
        call_ids: impl IntoIterator<Item = &'a ToolCallId>,
    ) -> bool {
        if self.active_prompts.remove(agent_prompt_id) {
            for call_id in call_ids {
                self.active_tools.insert(call_id.clone());
            }
            true
        } else {
            false
        }
    }

    /// Records a tool call that started independently of prompt-finish output.
    pub(crate) fn start_tool(&mut self, call_id: &ToolCallId) {
        self.active_tools.insert(call_id.clone());
    }

    /// Marks a tool as backgrounded while keeping it active for Ctrl-D guard
    /// purposes until its background result arrives.
    pub(crate) fn background_tool(&mut self, call_id: &ToolCallId) {
        self.backgrounded_tools.insert(call_id.clone());
        self.active_tools.insert(call_id.clone());
    }

    /// Finishes a foreground tool, leaving backgrounded tools active until the
    /// background completion path explicitly clears them.
    pub(crate) fn finish_tool(&mut self, call_id: &ToolCallId) {
        if !self.backgrounded_tools.contains(call_id) {
            self.active_tools.remove(call_id);
        }
    }

    /// Finishes a backgrounded tool and clears its Ctrl-D guard state.
    pub(crate) fn finish_background_tool(&mut self, call_id: &ToolCallId) {
        self.backgrounded_tools.remove(call_id);
        self.active_tools.remove(call_id);
    }

    /// Clears local optimistic submissions that can no longer receive prompt
    /// ids.
    pub(crate) fn clear_optimistic_submissions(&mut self) {
        self.optimistic_submissions = 0;
    }

    /// Clears all tracked prompt and tool activity.
    pub(crate) fn clear(&mut self) {
        self.optimistic_submissions = 0;
        self.active_prompts.clear();
        self.active_tools.clear();
        self.backgrounded_tools.clear();
    }
}

fn tool_call_ids_from_output_items(
    output_items: &[ContextItem],
) -> impl Iterator<Item = &ToolCallId> {
    output_items.iter().filter_map(|item| match item {
        ContextItem::ToolCall(call) => Some(&call.call_id),
        _ => None,
    })
}
