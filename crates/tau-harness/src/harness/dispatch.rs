//! Agent prompt-queue dispatch.
//!
//! Each live agent owns a `pending_prompts` queue. The harness has no global
//! agent slot — the agent extension serializes its own consumption of
//! `AgentPromptCreated` from the event log — so the dispatch logic here just
//! drains one prompt per *runnable* agent (Idle turn state, non-empty queue)
//! and lets the agent interleave them on its side.
//!
//! [`Harness::dispatch_user_prompt`] is the direct entry point for interactive
//! submissions and creates/reuses the session's durable user agent;
//! [`Harness::dispatch_prompt_for_agent`] is the shared per-agent primitive
//! (also used by side queries spawned via `StartAgentRequest`).
//! [`Harness::try_advance_queue`] is the react-to-state- change drain that
//! picks the next runnable agent and dispatches one prompt from its queue.
//!
//! [`Harness::dispatch_blocked_for`] is the predicate the rest of the harness
//! uses to decide whether to dispatch immediately or queue.

use tau_proto::{AgentId, Event, SessionId};

use crate::agent::{AgentTurnState, PendingPrompt};
use crate::error::HarnessError;
use crate::harness::Harness;

impl Harness {
    pub(crate) fn dispatch_user_prompt(
        &mut self,
        session_id: SessionId,
        text: String,
    ) -> Result<(), HarnessError> {
        let agent_id = self
            .agents
            .iter()
            .find_map(|(cid, conv)| {
                (conv.session_id == session_id
                    && conv.originator.is_user()
                    && conv.agent_id.is_some())
                .then_some(cid.clone())
            })
            .unwrap_or_else(|| {
                let role = self.selected_role.clone();
                self.create_durable_user_agent(session_id, &role)
            });
        self.dispatch_prompt_for_agent(&agent_id, PendingPrompt::user(text))
    }

    /// Publish one pending prompt as an `AgentPromptSubmitted` event on one
    /// agent's branch without dispatching an agent prompt yet.
    ///
    /// Callers that publish additional prompt-bearing events in the same batch
    /// can use this helper and then call
    /// [`Self::dispatch_prompt_after_publish_idle`] once the full batch has
    /// been queued. That keeps interception from sending the agent a prompt
    /// that only contains the first committed user-message event.
    pub(crate) fn publish_pending_prompt_for_agent(
        &mut self,
        agent_id: &AgentId,
        prompt: impl Into<PendingPrompt>,
    ) -> Result<(), HarnessError> {
        let prompt = prompt.into();
        let target_agent_id: tau_proto::AgentId =
            crate::parse_agent_id(self.ensure_agent_id_for_agent(agent_id).ok_or_else(|| {
                HarnessError::Participant(format!(
                    "publish_pending_prompt_for_agent: unknown agent `{agent_id}`"
                ))
            })?);
        let originator = self
            .agents
            .get(agent_id)
            .map(|c| c.originator.clone())
            .ok_or_else(|| {
                HarnessError::Participant(format!(
                    "publish_pending_prompt_for_agent: unknown agent `{agent_id}`"
                ))
            })?;
        if prompt.ctx_id.is_some()
            && let Some(agent) = self.agents.get_mut(agent_id)
        {
            agent.next_ctx_id = prompt.ctx_id.clone();
        }
        self.publish_for_agent(
            agent_id,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                agent_id: target_agent_id,
                text: prompt.text,
                message_class: prompt.message_class,
                originator,
                display_name: self.agent_display_name_for_cid(agent_id),
                ctx_id: prompt.ctx_id,
            }),
        );
        Ok(())
    }

    /// Dispatches one prompt for one agent: publishes the
    /// `AgentPromptSubmitted` event (head-bounced via
    /// `publish_for_agent` so it lands on the agent's
    /// branch), enters `AgentThinking`, and asks the agent for a
    /// completion.
    ///
    /// Used for both interactive user prompts on user agents and side-query
    /// prompts spawned by extensions.
    pub(crate) fn dispatch_prompt_for_agent(
        &mut self,
        agent_id: &AgentId,
        prompt: impl Into<PendingPrompt>,
    ) -> Result<(), HarnessError> {
        let prompt = prompt.into();
        if !prompt.is_internal() {
            let restore_prompts = self.take_pending_restore_prompts_for_user_prompt(agent_id);
            if !restore_prompts.is_empty() {
                for restore_prompt in restore_prompts {
                    self.publish_pending_prompt_for_agent(agent_id, restore_prompt)?;
                }
                self.publish_pending_prompt_for_agent(agent_id, prompt)?;
                self.dispatch_prompt_after_publish_idle(agent_id);
                return Ok(());
            }
        }

        self.publish_pending_prompt_for_agent(agent_id, prompt)?;
        // If the publish parked in interception (or queued behind one
        // that is), defer the agent dispatch until this user-prompt
        // event actually commits. If it committed inline, the helper
        // dispatches now: the AgentTree already reflects the new
        // user message, so the message list assembled inside
        // `send_prompt_to_agent_for` will include it.
        self.dispatch_prompt_after_user_message_publish(agent_id);
        Ok(())
    }

    /// Drains every runnable agent's pending prompt queue.
    ///
    /// There is no global agent slot — the agent extension serializes
    /// its own consumption of `AgentPromptCreated`. The harness emits
    /// one prompt per runnable agent (Idle turn state, non-empty
    /// queue) and routes responses back via `prompt_agents`.
    ///
    /// Session initialization still happens before prompt dispatch, so
    /// a fresh `chat-*` session can discover AGENTS.md and skills before
    /// the agent sees the first user message.
    pub(crate) fn try_advance_queue(&mut self) {
        if !self.turn_state.is_idle()
            || !self.extensions_all_ready()
            || (self.selected_model.is_none() && self.provider_model_info.is_empty())
        {
            return;
        }

        while let Some(agent_id) = self.next_runnable_agent() {
            let session_id = self
                .agents
                .get(&agent_id)
                .map(|c| c.session_id.clone())
                .expect("runnable agent exists");

            if !self.session_initialized(&session_id) {
                // Reachable only if the bound session somehow lost its
                // `initialized_sessions` entry; treat as a re-init.
                // Init is global, so stop draining until it completes.
                self.start_session_init(session_id, tau_proto::SessionStartReason::Initial);
                return;
            }

            let prompt = self
                .agents
                .get_mut(&agent_id)
                .and_then(|c| c.pending_prompts.pop_front())
                .expect("runnable agent has a prompt");
            if let Err(error) = self.dispatch_prompt_for_agent(&agent_id, prompt) {
                self.emit_harness_failure(&format!("failed to dispatch queued prompt: {error}"));
                // Reset the agent so it doesn't wedge as
                // AgentThinking with no in-flight prompt.
                if let Some(conv) = self.agents.get_mut(&agent_id) {
                    conv.in_flight_prompt = None;
                }
                self.set_agent_turn_state(&agent_id, AgentTurnState::Idle);
            }
        }
    }

    pub(crate) fn next_runnable_agent(&self) -> Option<AgentId> {
        self.agents
            .iter()
            .find(|(agent_id, conv)| {
                !conv.pending_prompts.is_empty()
                    && matches!(conv.turn_state, AgentTurnState::Idle)
                    && !self.has_deferred_prompt_dispatch_for(agent_id)
            })
            .map(|(agent_id, _)| agent_id.clone())
    }

    /// True when a fresh prompt for one agent should *not* be sent
    /// immediately. Two layers of gating:
    /// - global: selected role has no resolved model, harness mid-init,
    ///   extensions not yet `Ready`;
    /// - per-agent: that agent already has a prompt in flight, is waiting on
    ///   tool results, or has a latent dispatch parked behind interception.
    pub(crate) fn dispatch_blocked_for(&self, agent_id: &AgentId) -> bool {
        if self.selected_model.is_none()
            || !self.turn_state.is_idle()
            || !self.extensions_all_ready()
            || !self.agent_context_ready_for(agent_id)
        {
            return true;
        }
        match self.agents.get(agent_id) {
            Some(conv) => {
                !matches!(conv.turn_state, AgentTurnState::Idle)
                    || self.has_deferred_prompt_dispatch_for(agent_id)
            }
            None => true,
        }
    }
}
