//! Completion-relative provenance for direct user prompts, without queue
//! persistence.

use tau_proto::{ContentPart, ContextItem, ContextRole, Event, PromptSubmissionSource};

use super::{ConversationHeadSync, Harness};

impl Harness {
    /// Freeze the causal decision immediately before the accepted prompt fact.
    pub(super) fn stamp_delayed_user_prompt(
        &self,
        event: &mut Event,
        sync: Option<&ConversationHeadSync>,
    ) {
        let Some(sync) = sync else {
            return;
        };
        let Some(baseline) = sync.user_response_baseline else {
            return;
        };
        let delayed = self
            .agent_runtime
            .agent_registry
            .agents
            .get(&sync.cid)
            .is_some_and(|agent| agent.dispatch.visible_response_generation != baseline);
        match event {
            Event::AgentPromptSubmitted(prompt)
                if prompt.submission_source == PromptSubmissionSource::HumanUi =>
            {
                prompt.delayed = delayed;
            }
            Event::AgentPromptSteered(prompt)
                if prompt.submission_source == PromptSubmissionSource::HumanUi =>
            {
                prompt.delayed = delayed;
            }
            _ => {}
        }
    }

    /// Count committed visible completions before releasing dependent prompts.
    ///
    /// Streaming deltas are not completion. Commentary and retained canceled
    /// text qualify; reasoning, tools, status, and private compaction do not.
    pub(super) fn observe_completed_visible_response(&mut self, event: &Event) {
        let Event::ProviderResponseFinished(response) = event else {
            return;
        };
        if !response.originator.is_user()
            || self
                .prompt_coordination
                .standalone_accounting
                .owners
                .contains_key(&response.agent_prompt_id)
            || self
                .prompt_coordination
                .prompt_runtime
                .operations
                .get(&response.agent_prompt_id)
                .is_some_and(|operation| {
                    operation.0 == tau_proto::PromptOperation::StandaloneCompaction
                })
            || !response.output_items.iter().any(|item| {
                matches!(item, ContextItem::Message(message)
                    if message.role == ContextRole::Assistant
                        && message.content.iter().any(|part|
                            matches!(part, ContentPart::Text { text } if !text.is_empty())))
            })
        {
            return;
        }
        if let Some(cid) = self
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(&response.agent_id)
            && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid)
        {
            agent.dispatch.visible_response_generation = agent
                .dispatch
                .visible_response_generation
                .checked_add(1)
                .expect("visible response generation exhausted");
        }
    }
}
