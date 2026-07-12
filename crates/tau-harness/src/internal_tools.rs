//! Injection point for harness-internal tools owned by higher crates.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use tau_proto::{
    CborValue, Event, StartAgentRequest, ToolCallId, ToolError, ToolName, ToolProgress, ToolResult,
    ToolSpec, ToolUseState,
};

use crate::discovery::DiscoveredSkillSource;
use crate::error::HarnessError;
use crate::harness::{HARNESS_CONNECTION_ID, Harness};
use crate::{AgentId, AgentToolCall};

/// A handler for tools implemented inside the harness process.
pub trait InternalToolHandler: Send + Sync {
    /// Tool specifications this handler registers as internal tools.
    fn tool_specs(&self) -> Vec<ToolSpec>;

    /// Optional independent policy group for one registered tool.
    fn tool_group(&self, _internal_tool_name: &ToolName) -> Option<tau_proto::ToolGroup> {
        None
    }

    /// Return true when this handler owns `internal_tool_name`.
    fn handles(&self, internal_tool_name: &ToolName) -> bool;

    /// React to a committed event.
    ///
    /// Internal tools observe the same durable lifecycle events as external
    /// extensions. A handler should filter for events it owns, such as
    /// `ToolStarted` for its registered tools or later correlation events like
    /// `StartAgentResult`.
    fn handle_event(
        &self,
        host: &mut InternalToolHost<'_>,
        event: &Event,
    ) -> Result<(), HarnessError> {
        let _ = host;
        let _ = event;
        Ok(())
    }
}

/// Shared reference-counted internal tool handler.
pub type InternalToolHandlers = Vec<Arc<dyn InternalToolHandler>>;

/// Public snapshot of one skill known to the harness.
#[derive(Clone)]
pub struct InternalSkill {
    /// Skill name used as the `skill` query exact match.
    pub name: String,
    /// Short human-facing description.
    pub description: String,
    /// Markdown source for loading or content search.
    pub source: InternalSkillSource,
}

/// Public snapshot of a skill Markdown source.
#[derive(Clone)]
pub enum InternalSkillSource {
    /// An extension-announced skill backed by an on-disk Markdown file.
    File(PathBuf),
    /// A Tau built-in skill embedded into the harness binary.
    BuiltIn { content: Cow<'static, str> },
}

impl InternalSkillSource {
    /// Human-readable source label for warnings.
    pub fn label(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::BuiltIn { .. } => "built-in skill".to_owned(),
        }
    }
}

/// Narrow facade exposed to internal tool handler crates.
///
/// Internal tools should behave like ordinary event-log-driven tools: they
/// register specs, observe committed lifecycle events, and publish normal
/// tool results/errors instead of being special-cased by the harness. This
/// facade exists only for the parts that cannot sensibly live outside the
/// harness, such as synchronized access to conversation, background-tool,
/// or side-agent state. Every method runs on the harness event-loop thread,
/// so handlers can consult or update that state without racing the event log
/// handler.
pub struct InternalToolHost<'a> {
    harness: &'a mut Harness,
}

impl<'a> InternalToolHost<'a> {
    pub(crate) fn new(harness: &'a mut Harness) -> Self {
        Self { harness }
    }

    /// Register a harness-process internal tool.
    pub fn register_internal_tool(&mut self, spec: ToolSpec, group: Option<tau_proto::ToolGroup>) {
        if let Some(group) = group {
            let _ = self.harness.registry.register_internal_with_group(
                HARNESS_CONNECTION_ID,
                spec,
                group,
            );
        } else {
            let _ = self
                .harness
                .registry
                .register_internal(HARNESS_CONNECTION_ID, spec);
        }
    }

    /// Return a cloned snapshot of skills discovered by the harness.
    pub fn discovered_skills(&self) -> Vec<InternalSkill> {
        self.harness
            .discovered_skills
            .iter()
            .filter(|(_, skill)| !skill.disable_model_invocation)
            .map(|(name, skill)| InternalSkill {
                name: name.as_str().to_owned(),
                description: skill.description.clone(),
                source: match &skill.source {
                    DiscoveredSkillSource::File(path) => InternalSkillSource::File(path.clone()),
                    DiscoveredSkillSource::BuiltIn { content } => InternalSkillSource::BuiltIn {
                        content: content.clone(),
                    },
                },
            })
            .collect()
    }

    /// Emit an important informational message to the user.
    pub fn emit_info_important(&mut self, message: &str) {
        self.harness.emit_info_important(message);
    }

    /// Ensure and return the agent id for a conversation.
    pub fn ensure_agent_id_for_agent(&mut self, conversation_id: &AgentId) -> Option<String> {
        self.harness.ensure_agent_id_for_agent(conversation_id)
    }

    /// Enqueue a start-agent request from an internal handler without draining.
    pub fn enqueue_start_agent_request_without_draining(
        &mut self,
        query: StartAgentRequest,
    ) -> Result<String, String> {
        self.harness
            .enqueue_internal_start_agent_request_without_draining(query)
    }

    /// Drain queued start-agent requests.
    pub fn drain_start_agent_requests(&mut self) -> Result<(), HarnessError> {
        self.harness.drain_pending_start_agent_requests()
    }

    /// Mark a call as backgrounded for instant-background tools.
    pub fn mark_tool_backgrounded(&mut self, call_id: &ToolCallId) -> bool {
        self.harness.tool_turn.mark_backgrounded(call_id)
    }

    /// Publish a background placeholder with custom text.
    pub fn publish_background_placeholder(&mut self, call_id: &ToolCallId, result: CborValue) {
        self.harness
            .publish_internal_background_placeholder(call_id, result);
    }

    /// Background an internal tool call with a custom placeholder and release
    /// the foreground turn so the caller can continue while the tool remains
    /// waitable.
    pub fn background_tool_call(&mut self, call_id: &ToolCallId, result: CborValue) {
        if self.harness.tool_turn.mark_backgrounded(call_id) {
            self.harness
                .publish_internal_background_placeholder(call_id, result);
        }
        self.harness
            .on_tool_call_foreground_complete(call_id.as_str());
    }

    /// Request standalone compaction on behalf of a committed built-in tool
    /// call.
    pub fn request_agent_tool_compaction(
        &mut self,
        conversation_id: &AgentId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
        target_agent_id: Option<&tau_proto::AgentId>,
    ) -> Result<(), HarnessError> {
        self.harness.request_agent_tool_compaction(
            conversation_id,
            call,
            visible_tool_name,
            target_agent_id,
        );
        Ok(())
    }

    /// Complete a prebuilt internal tool result, routing foreground/background.
    pub fn finish_prebuilt_tool_result(&mut self, result: ToolResult) {
        self.harness.finish_prebuilt_internal_tool_result(result);
    }

    /// Complete a prebuilt internal tool error, routing foreground/background.
    pub fn finish_prebuilt_tool_error(&mut self, error: ToolError) {
        self.harness.finish_prebuilt_internal_tool_error(error);
    }

    /// Handle the built-in `wait` tool.
    pub fn handle_wait_tool_call(
        &mut self,
        conversation_id: &AgentId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        self.harness
            .handle_wait_tool_call(conversation_id, call, visible_tool_name)
    }

    #[cfg(test)]
    pub(crate) fn handle_message_tool_call(
        &mut self,
        conversation_id: &AgentId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        self.harness
            .handle_message_tool_call(conversation_id, call, visible_tool_name)
    }

    /// Resolve a committed `ToolStarted` event for an internal tool.
    pub fn internal_started_call(
        &mut self,
        started: &tau_proto::ToolStarted,
    ) -> Option<(AgentId, AgentToolCall, ToolName)> {
        let cid = self.harness.tool_agents.get(&started.call_id)?.clone();
        let pending = self.harness.pending_tools.get(&started.call_id)?.clone();
        let call = AgentToolCall {
            id: started.call_id.clone(),
            name: pending.internal_name,
            tool_type: pending.tool_type,
            arguments: started.arguments.clone(),
        };
        Some((cid, call, pending.name))
    }

    /// Ensure the harness tracks an internal tool call before it completes.
    pub fn ensure_internal_tool_tracking(
        &mut self,
        conversation_id: &AgentId,
        call: &AgentToolCall,
        visible_tool_name: &ToolName,
    ) {
        self.harness
            .ensure_harness_owned_tool_tracking(conversation_id, call, visible_tool_name);
    }

    /// Complete an internal tool call with a final text result.
    pub fn finish_tool_with_result(
        &mut self,
        conversation_id: &AgentId,
        call_id: tau_proto::ToolCallId,
        tool_name: ToolName,
        tool_type: tau_proto::ToolType,
        result: String,
        details: Option<tau_proto::CborValue>,
    ) {
        self.harness.finish_harness_owned_tool_with_result(
            conversation_id,
            call_id,
            tool_name,
            tool_type,
            result,
            details,
        );
    }

    /// Publish provider-owned display state for an in-flight internal tool.
    pub fn publish_tool_progress(
        &mut self,
        conversation_id: &AgentId,
        call_id: ToolCallId,
        tool_name: ToolName,
        display: ToolUseState,
    ) {
        self.harness.publish_for_agent(
            conversation_id,
            Event::ToolProgress(ToolProgress {
                call_id,
                tool_name,
                message: None,
                progress: None,
                display: Some(display),
            }),
        );
    }

    /// Complete an internal tool call with a final structured result.
    pub fn finish_tool_with_cbor_result(
        &mut self,
        conversation_id: &AgentId,
        call_id: tau_proto::ToolCallId,
        tool_name: ToolName,
        tool_type: tau_proto::ToolType,
        result: tau_proto::CborValue,
        display: Option<ToolUseState>,
    ) {
        self.harness.finish_harness_owned_tool_with_cbor_result(
            conversation_id,
            call_id,
            tool_name,
            tool_type,
            result,
            display,
        );
    }

    /// Complete an internal tool call with a final error.
    pub fn finish_tool_with_error(
        &mut self,
        conversation_id: &AgentId,
        call_id: tau_proto::ToolCallId,
        tool_name: ToolName,
        tool_type: tau_proto::ToolType,
        message: String,
        details: Option<tau_proto::CborValue>,
    ) {
        self.harness.finish_harness_owned_tool_with_error(
            conversation_id,
            call_id,
            tool_name,
            tool_type,
            message,
            details,
        );
    }

    /// Complete an internal tool call with a final displayed error.
    #[allow(clippy::too_many_arguments)]
    pub fn finish_tool_with_display_error(
        &mut self,
        conversation_id: &AgentId,
        call_id: tau_proto::ToolCallId,
        tool_name: ToolName,
        tool_type: tau_proto::ToolType,
        message: String,
        details: Option<tau_proto::CborValue>,
        display: Option<ToolUseState>,
    ) {
        self.harness.finish_harness_owned_tool_with_display_error(
            conversation_id,
            call_id,
            tool_name,
            tool_type,
            message,
            details,
            display,
        );
    }

    /// Return true when a tool call is still tracked as running.
    pub fn is_running_tool_call(&self, target_call_id: &ToolCallId) -> bool {
        self.harness.is_running_tool_call(target_call_id)
    }

    /// Return true when a running tool call owned by `conversation_id` is known
    /// to accept the generic event-log cancellation request used by the
    /// `cancel` tool.
    pub fn is_running_cancellable_tool_call_for(
        &self,
        conversation_id: &AgentId,
        target_call_id: &ToolCallId,
    ) -> bool {
        self.harness
            .is_running_cancellable_tool_call_for(conversation_id, target_call_id)
    }

    /// Return true when this harness saw the caller's tool call reach a
    /// terminal state.
    pub fn is_completed_tool_call_for(
        &self,
        conversation_id: &AgentId,
        target_call_id: &ToolCallId,
    ) -> bool {
        self.harness
            .is_completed_tool_call_for(conversation_id, target_call_id)
    }

    /// Publish a durable broadcast tool cancellation request after checking
    /// that `conversation_id` owns the target call.
    pub fn publish_tool_cancel_request_for(
        &mut self,
        conversation_id: &AgentId,
        target_call_id: ToolCallId,
    ) -> Result<(), String> {
        self.harness
            .publish_tool_cancel_request_for(conversation_id, target_call_id)
    }

    /// Cancel a start-agent request owned by an internal tool handler.
    pub fn cancel_start_agent_request(
        &mut self,
        query_id: &str,
        target_call_id: &ToolCallId,
        suppress_background_completion_prompt: bool,
    ) -> Result<(), String> {
        self.harness.cancel_start_agent_request(
            query_id,
            target_call_id,
            suppress_background_completion_prompt,
        )
    }

    /// Publish an agent-to-agent or agent-to-user message from a conversation.
    pub fn publish_agent_message(
        &mut self,
        conversation_id: &AgentId,
        recipient_id: String,
        message: String,
    ) -> Result<(), String> {
        self.harness
            .publish_agent_message_from_agent(conversation_id, recipient_id, message)
    }

    /// Return the harness's active session id.
    pub fn current_session_id(&self) -> tau_proto::SessionId {
        self.harness.current_session_id.clone()
    }

    /// Publish an external message and arrange asynchronous tool completion.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_external_agent_message(
        &mut self,
        conversation_id: &AgentId,
        recipient_session_id: tau_proto::SessionId,
        recipient_id: tau_proto::AgentId,
        message: String,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: tau_proto::ToolType,
        details: CborValue,
    ) -> Result<(), String> {
        self.harness.publish_external_agent_message_from_agent(
            conversation_id,
            recipient_session_id,
            recipient_id,
            message,
            tau_proto::AgentMessageKind::Message,
            Some(crate::harness::ExternalMessageToolCompletion {
                conversation_id: conversation_id.clone(),
                session_generation: self.harness.current_session_generation,
                call_id,
                tool_name,
                tool_type,
                details,
            }),
        )
    }

    /// Publish an agent-to-agent message by public sender and recipient ids.
    pub fn publish_agent_message_from_agent_ids(
        &mut self,
        sender_agent_id: &str,
        recipient_id: String,
        message: String,
    ) -> Result<(), String> {
        let sender_cid = self.sender_conversation_id(sender_agent_id)?;
        self.harness
            .publish_agent_message_from_agent(&sender_cid, recipient_id, message)
    }

    /// Publish an `agent_watch` response notification by public sender and
    /// recipient ids.
    pub fn publish_agent_watch_response_from_agent_ids(
        &mut self,
        sender_agent_id: &str,
        recipient_id: String,
        message: String,
    ) -> Result<(), String> {
        let sender_cid = self.sender_conversation_id(sender_agent_id)?;
        self.harness
            .publish_agent_watch_response_from_agent(&sender_cid, recipient_id, message)
    }

    /// Atomically validate the target lifecycle and mutate a watch relation.
    ///
    /// Enabling requires a live target. Disabling is idempotent, including for
    /// stopped or unknown target ids.
    pub fn try_set_agent_watch(
        &mut self,
        watcher_id: &str,
        watched_agent_id: &str,
        enable: bool,
        cause: tau_proto::AgentWatchUpdateCause,
    ) -> Result<(), String> {
        self.harness
            .try_set_agent_watch(watcher_id, watched_agent_id, enable, cause)
    }

    /// Return public watcher ids currently watching `watched_agent_id`.
    pub fn watchers_for_agent(&self, watched_agent_id: &str) -> Vec<String> {
        self.harness.watchers_for_agent(watched_agent_id)
    }

    /// Return the sanitized current provider status for a watched agent.
    pub fn agent_watch_provider_status_summary(&self, watched_agent_id: &str) -> Option<String> {
        self.harness
            .agent_watch_provider_status_summary(watched_agent_id)
    }

    /// Return whether the current generation may fan out a final watch
    /// response.
    ///
    /// Generations caused only by watch notifications are suppressed to prevent
    /// mutual watches from creating response feedback. Ordinary input promotes
    /// the generation before completion and restores normal response fanout.
    /// Missing or terminating watched endpoints are denied so a parked final
    /// response cannot cross the unload boundary.
    pub fn agent_watch_response_allowed(&self, watched_agent_id: &str) -> bool {
        self.harness
            .agent_routes
            .get(watched_agent_id)
            .and_then(|cid| self.harness.agents.get(cid))
            .is_some_and(|agent| !agent.terminating && !agent.lifecycle_notification_only_turn)
    }

    /// Prune a stale watch relationship after notification delivery failed.
    pub fn prune_agent_watch(&mut self, watcher_id: &str, watched_agent_id: &str) {
        self.harness.prune_agent_watch(watcher_id, watched_agent_id);
    }

    fn sender_conversation_id(&self, sender_agent_id: &str) -> Result<AgentId, String> {
        self.harness
            .agent_routes
            .get(sender_agent_id)
            .cloned()
            .ok_or_else(|| format!("unknown message sender: `{sender_agent_id}`"))
    }

    /// Return true when a public agent id is known to this harness, even if
    /// stopped.
    pub fn is_known_agent_id(&self, agent_id: &str) -> bool {
        !matches!(
            self.harness.agent_message_recipient_status(agent_id),
            crate::harness::AgentMessageRecipientStatus::Unknown
        )
    }
}

impl Harness {
    /// Install handlers and register their internal tool specs.
    pub fn install_internal_tool_handlers(&mut self, handlers: InternalToolHandlers) {
        self.internal_tool_handlers = handlers;
        let handlers = self.internal_tool_handlers.clone();
        let mut host = InternalToolHost::new(self);
        for handler in handlers {
            for spec in handler.tool_specs() {
                let group = handler.tool_group(&spec.name);
                host.register_internal_tool(spec, group);
            }
        }
    }

    pub(crate) fn dispatch_internal_tool_event(
        &mut self,
        event: &Event,
    ) -> Result<(), HarnessError> {
        let handlers = self.internal_tool_handlers.clone();
        for handler in handlers {
            let mut host = InternalToolHost::new(self);
            handler.handle_event(&mut host, event)?;
        }
        Ok(())
    }
}
