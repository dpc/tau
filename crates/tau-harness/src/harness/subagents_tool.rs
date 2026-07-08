//! Harness-owned `agent_start`, `wait`, `cancel`, and `message` tools.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tau_proto::{
    AgentContextKey, AgentContextValue, AgentId, AgentMessageReceived, AgentMessageSent,
    AgentWatchUpdateCause, AgentWatchesUpdated, CborValue, Event, ToolBackgroundError,
    ToolBackgroundResult, ToolCallId, ToolError, ToolName, ToolResult, ToolResultKind, ToolType,
    ToolUseState, ToolUseStatus,
};

use crate::error::HarnessError;
use crate::event::{ExternalMessageToolCompletedCommand, HarnessCommand, HarnessEvent};
use crate::harness::{
    AgentMessageRecipientStatus, AgentToolCall, HARNESS_CONNECTION_ID, Harness,
    PendingExternalAgentMessageAuth,
};

/// Model-visible name of the harness-owned wait tool.
pub(crate) const WAIT_TOOL_NAME: &str = "wait";
#[cfg(test)]
pub(crate) const MESSAGE_TOOL_NAME: &str = "message";
#[derive(Default)]
pub(crate) struct SubagentToolState {
    /// State used by the wait tool to track background completions.
    wait_tracker: WaitTracker,
}

static NEXT_AGENT_MESSAGE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
pub(crate) const EXTERNAL_AGENT_MESSAGE_AUTH_TIMEOUT: Duration = Duration::from_secs(25);
pub(crate) const EXTERNAL_AGENT_MESSAGE_RESULT_TIMEOUT: Duration = Duration::from_secs(30);

fn build_agent_message_id(
    sender_id: &AgentId,
    timestamp: tau_proto::UnixMicros,
    sequence: u64,
) -> tau_proto::AgentMessageId {
    tau_proto::AgentMessageId::from(format!(
        "msg-{}-{}-{}",
        sender_id.as_str(),
        timestamp.get(),
        sequence
    ))
}

fn next_agent_message_id(sender_id: &AgentId) -> tau_proto::AgentMessageId {
    build_agent_message_id(
        sender_id,
        tau_proto::UnixMicros::now(),
        NEXT_AGENT_MESSAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    )
}

impl Harness {
    #[cfg(test)]
    pub(crate) fn register_harness_tools(&mut self) {
        self.install_internal_tool_handlers(vec![std::sync::Arc::new(TestBuiltinTools)]);
    }

    pub(crate) fn publish_delegate_roles_context(&mut self) {
        let mut roles: Vec<_> = crate::model::role_infos(
            &self.provider_model_info,
            &self.available_roles,
            &self.available_models,
        )
        .into_iter()
        .filter(|info| {
            crate::model::model_for_role(
                &self.provider_model_info,
                &self.available_roles,
                &info.name,
            )
            .is_some()
        })
        .map(|info| {
            let description = info
                .role_description
                .as_deref()
                .filter(|description| !description.is_empty())
                .unwrap_or(&info.description);
            serde_json::json!({
                "name": info.name,
                "description": description,
            })
        })
        .collect();
        roles.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        let agent_ids: Vec<_> = self
            .agents
            .values()
            .filter_map(|agent| agent.agent_id.clone())
            .collect();
        for agent_id in agent_ids {
            self.agent_context.publish(
                tau_proto::AgentId::parse(agent_id).expect("agent id"),
                AgentContextKey::new("delegate_roles"),
                tau_proto::ConnectionId::from(HARNESS_CONNECTION_ID),
                "harness".to_owned(),
                AgentContextValue(serde_json::Value::Array(roles.clone())),
            );
        }
    }

    pub(crate) fn record_wait_tool_request(&mut self, call_id: &ToolCallId) {
        if let Some(tool) = self.pending_tools.get(call_id) {
            let Some(owner) = self.wait_owner_for_call(call_id) else {
                return;
            };
            self.subagents.wait_tracker.record_tool_invoke(
                call_id.clone(),
                tool.name.clone(),
                owner,
            );
        }
    }

    pub(crate) fn record_wait_tool_result(&mut self, result: ToolResult) {
        let Some(owner) = self.wait_owner_for_call(&result.call_id) else {
            return;
        };
        let replies = self
            .subagents
            .wait_tracker
            .record_tool_result(result, owner);
        self.publish_wait_replies(replies);
    }

    pub(crate) fn record_wait_tool_error(&mut self, error: ToolError) {
        let Some(owner) = self.wait_owner_for_call(&error.call_id) else {
            return;
        };
        let replies = self.subagents.wait_tracker.record_tool_error(error, owner);
        self.publish_wait_replies(replies);
    }

    pub(crate) fn record_wait_background_result(&mut self, result: ToolBackgroundResult) {
        let Some(owner) = self.wait_owner_for_call(&result.call_id) else {
            return;
        };
        let replies = self
            .subagents
            .wait_tracker
            .record_background_result(result, owner);
        self.publish_wait_replies(replies);
    }

    pub(crate) fn record_wait_background_error(&mut self, error: ToolBackgroundError) {
        let Some(owner) = self.wait_owner_for_call(&error.call_id) else {
            return;
        };
        let replies = self
            .subagents
            .wait_tracker
            .record_background_error(error, owner);
        self.publish_wait_replies(replies);
    }

    /// Move the wait tracker's background-call ownership during
    /// side-conversation teardown.
    pub(crate) fn transfer_wait_background_owner_before_teardown(
        &mut self,
        call_id: &ToolCallId,
        source: &AgentId,
        target: &AgentId,
    ) {
        self.subagents
            .wait_tracker
            .transfer_call_owner(call_id, source, target);
    }

    /// Drop wait ownership for a background call that belongs to a canceled
    /// side agent so it cannot become waitable from its parent.
    pub(crate) fn discard_wait_background_owner_before_teardown(
        &mut self,
        call_id: &ToolCallId,
        source: &AgentId,
    ) {
        self.subagents
            .wait_tracker
            .discard_call_owner(call_id, source);
    }

    fn wait_owner_for_call(&self, call_id: &ToolCallId) -> Option<AgentId> {
        self.tool_agents
            .get(call_id)
            .or_else(|| self.background_completion_targets.get(call_id))
            .cloned()
    }

    pub(crate) fn interrupt_active_waits(&mut self) {
        let replies = self.subagents.wait_tracker.interrupt_active_waits();
        self.publish_wait_replies(replies);
    }

    pub(crate) fn interrupt_active_waits_for(&mut self, owner: &AgentId) {
        let replies = self
            .subagents
            .wait_tracker
            .interrupt_active_waits_for(owner);
        self.publish_wait_replies(replies);
    }

    pub(crate) fn record_wait_tool_cancelled(&mut self, call_ids: &HashSet<ToolCallId>) {
        let cancelled = self.subagents.wait_tracker.record_tool_cancelled(call_ids);
        for call_id in cancelled.unsuppress_call_ids {
            self.unsuppress_background_completion_prompt(call_id);
        }
        for call_id in cancelled.suppress_call_ids {
            self.suppress_background_completion_prompt(call_id);
        }
        self.publish_wait_replies(cancelled.replies);
    }

    /// Handle the harness-owned `message` tool call inline.
    /// Publish an agent message after validating sender and recipient state.
    pub(crate) fn publish_agent_message_from_agent(
        &mut self,
        agent_id: &AgentId,
        recipient_id: String,
        message: String,
    ) -> Result<(), String> {
        self.publish_agent_delivery_from_agent(
            agent_id,
            recipient_id,
            message,
            tau_proto::AgentMessageKind::Message,
        )
    }

    pub(crate) fn publish_agent_watch_response_from_agent(
        &mut self,
        agent_id: &AgentId,
        recipient_id: String,
        message: String,
    ) -> Result<(), String> {
        self.publish_agent_delivery_from_agent(
            agent_id,
            recipient_id,
            message,
            tau_proto::AgentMessageKind::WatchResponse,
        )
    }

    /// Enable or disable one session-local watch relation and publish the
    /// authoritative watcher snapshot.
    pub(crate) fn set_agent_watch(
        &mut self,
        watcher_id: &str,
        watched_agent_id: &str,
        enable: bool,
        cause: AgentWatchUpdateCause,
    ) {
        if watcher_id == watched_agent_id {
            return;
        }
        if enable {
            self.agent_watches
                .entry(watcher_id.to_owned())
                .or_default()
                .insert(watched_agent_id.to_owned());
            self.agent_watchers
                .entry(watched_agent_id.to_owned())
                .or_default()
                .insert(watcher_id.to_owned());
        } else {
            if let Some(watched) = self.agent_watches.get_mut(watcher_id) {
                watched.remove(watched_agent_id);
                if watched.is_empty() {
                    self.agent_watches.remove(watcher_id);
                }
            }
            if let Some(watchers) = self.agent_watchers.get_mut(watched_agent_id) {
                watchers.remove(watcher_id);
                if watchers.is_empty() {
                    self.agent_watchers.remove(watched_agent_id);
                }
            }
        }
        self.publish_agent_watches_snapshot(watcher_id, Some(watched_agent_id), cause);
    }

    /// Return a sorted snapshot of current watcher ids for a watched agent.
    pub(crate) fn watchers_for_agent(&self, watched_agent_id: &str) -> Vec<String> {
        self.agent_watchers
            .get(watched_agent_id)
            .map(|watchers| watchers.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Remove a stale watch relation and publish the updated watcher snapshot.
    pub(crate) fn prune_agent_watch(&mut self, watcher_id: &str, watched_agent_id: &str) {
        self.set_agent_watch(
            watcher_id,
            watched_agent_id,
            false,
            AgentWatchUpdateCause::WatcherPruned,
        );
    }

    fn publish_agent_watches_snapshot(
        &mut self,
        watcher_id: &str,
        changed_agent_id: Option<&str>,
        cause: AgentWatchUpdateCause,
    ) {
        let watched_agent_ids = self
            .agent_watches
            .get(watcher_id)
            .map(|watched| {
                watched
                    .iter()
                    .map(crate::parse_agent_id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.publish_event(
            Some(super::HARNESS_CONNECTION_ID),
            Event::AgentWatchesUpdated(AgentWatchesUpdated {
                session_id: self.current_session_id.clone(),
                watcher_id: crate::parse_agent_id(watcher_id),
                watched_agent_ids,
                changed_agent_id: changed_agent_id.map(crate::parse_agent_id),
                cause,
            }),
        );
    }

    pub(crate) fn publish_agent_delivery_from_agent(
        &mut self,
        agent_id: &AgentId,
        recipient_id: String,
        message: String,
        kind: tau_proto::AgentMessageKind,
    ) -> Result<(), String> {
        let sender_id = self
            .ensure_agent_id_for_agent(agent_id)
            .ok_or_else(|| "sender agent no longer exists".to_owned())?;
        if recipient_id != "user" {
            match self.agent_message_recipient_status(&recipient_id) {
                AgentMessageRecipientStatus::Live => {}
                AgentMessageRecipientStatus::Stopped => {
                    return Err(format!("stopped message recipient: `{recipient_id}`"));
                }
                AgentMessageRecipientStatus::Unknown => {
                    return Err(format!("unknown message recipient: `{recipient_id}`"));
                }
            }
        }
        let recipient = if recipient_id == "user" {
            tau_proto::AgentMessageRecipient::User
        } else {
            tau_proto::AgentMessageRecipient::Agent {
                agent_id: crate::parse_agent_id(&recipient_id),
            }
        };
        let sender_id: tau_proto::AgentId = crate::parse_agent_id(&sender_id);
        let message_id = next_agent_message_id(&sender_id);
        if kind == tau_proto::AgentMessageKind::Message {
            self.publish_for_agent_from(
                agent_id,
                Some(HARNESS_CONNECTION_ID),
                Event::AgentMessageSent(AgentMessageSent {
                    message_id: message_id.clone(),
                    sender_id: sender_id.clone(),
                    recipient: recipient.clone(),
                    kind,
                    message: message.clone(),
                }),
            );
        }
        if let tau_proto::AgentMessageRecipient::Agent { agent_id } = recipient {
            self.publish_event(
                Some(HARNESS_CONNECTION_ID),
                Event::AgentMessageReceived(AgentMessageReceived {
                    message_id,
                    sender_id,
                    sender_session_id: None,
                    recipient_id: agent_id,
                    kind,
                    message,
                }),
            );
        }
        Ok(())
    }

    /// Prepare the sender-side projection for an external message and start a
    /// worker that performs runtime-dir lookup plus socket RPC off the harness
    /// event loop. The projection is published only after confirmed delivery.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn publish_external_agent_message_from_agent(
        &mut self,
        conversation_id: &AgentId,
        recipient_session_id: tau_proto::SessionId,
        recipient_id: AgentId,
        message: String,
        kind: tau_proto::AgentMessageKind,
        completion: Option<ExternalMessageToolCompletion>,
    ) -> Result<(), String> {
        let sender_id = self
            .ensure_agent_id_for_agent(conversation_id)
            .ok_or_else(|| "sender agent no longer exists".to_owned())?;
        let sender_id: tau_proto::AgentId = crate::parse_agent_id(&sender_id);
        let message_id = next_agent_message_id(&sender_id);
        let request_id = format!("external-{message_id}");
        let capability = random_external_message_capability(&mut self.agent_id_rng);
        let sent_event = (kind == tau_proto::AgentMessageKind::Message).then(|| AgentMessageSent {
            message_id: message_id.clone(),
            sender_id: sender_id.clone(),
            recipient: tau_proto::AgentMessageRecipient::ExternalAgent {
                session_id: recipient_session_id.clone(),
                agent_id: recipient_id.clone(),
            },
            kind,
            message: message.clone(),
        });
        self.pending_external_message_auth.insert(
            message_id.clone(),
            PendingExternalAgentMessageAuth {
                capability: capability.clone(),
                sender_session_id: self.current_session_id.clone(),
                sender_id: sender_id.clone(),
                recipient_session_id: recipient_session_id.clone(),
                recipient_id: recipient_id.clone(),
                kind,
                message: message.clone(),
            },
        );
        let request = tau_proto::ExternalAgentMessageRequest {
            request_id,
            message_id,
            capability,
            sender_session_id: self.current_session_id.clone(),
            sender_id,
            recipient_session_id,
            recipient_id,
            kind,
            message,
        };
        let tx = self.tx.clone();
        thread::spawn(move || {
            let auth_message_id = request.message_id.clone();
            let result = send_external_agent_message_request(request);
            if let Some(completion) = completion {
                let _ = tx.send(HarnessEvent::Command(
                    HarnessCommand::ExternalMessageToolCompleted(Box::new(
                        ExternalMessageToolCompletedCommand {
                            conversation_id: completion.conversation_id,
                            session_generation: completion.session_generation,
                            call_id: completion.call_id,
                            tool_name: completion.tool_name,
                            tool_type: completion.tool_type,
                            result,
                            details: completion.details,
                            auth_message_id,
                            sent_event,
                        },
                    )),
                ));
            }
        });
        Ok(())
    }

    /// Authenticate a target-harness callback for a pending outbound external
    /// message.
    pub(crate) fn handle_external_agent_message_auth_request(
        &mut self,
        request: tau_proto::ExternalAgentMessageAuthRequest,
    ) -> tau_proto::ExternalAgentMessageAuthResult {
        let request_id = request.request_id.clone();
        let result = self.authorize_external_agent_message(request);
        tau_proto::ExternalAgentMessageAuthResult {
            request_id,
            authorized: result.is_ok(),
            error: result.err(),
        }
    }

    fn authorize_external_agent_message(
        &self,
        request: tau_proto::ExternalAgentMessageAuthRequest,
    ) -> Result<(), String> {
        let Some(pending) = self.pending_external_message_auth.get(&request.message_id) else {
            return Err("unknown external message capability".to_owned());
        };
        if pending.capability != request.capability
            || pending.sender_session_id != request.sender_session_id
            || pending.sender_id != request.sender_id
            || pending.recipient_session_id != request.recipient_session_id
            || pending.recipient_id != request.recipient_id
            || pending.kind != request.kind
            || pending.message != request.message
        {
            return Err("external message capability does not match request".to_owned());
        }
        if request.sender_session_id != self.current_session_id {
            return Err(format!(
                "sender harness is on active session `{}`, not `{}`",
                self.current_session_id, request.sender_session_id
            ));
        }
        Ok(())
    }

    /// Handle an external agent message RPC accepted from a socket client.
    pub(crate) fn start_external_agent_message_auth(
        &mut self,
        client_id: tau_proto::ConnectionId,
        request: tau_proto::ExternalAgentMessageRequest,
    ) -> Option<tau_proto::ExternalAgentMessageResult> {
        let request_id = request.request_id.clone();
        if let Err(error) = self.validate_external_agent_message_target(&request) {
            return Some(tau_proto::ExternalAgentMessageResult {
                request_id,
                error: Some(error),
            });
        }
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = authenticate_external_agent_message_sender(&request);
            let _ = tx.send(HarnessEvent::Command(
                HarnessCommand::ExternalMessageAuthCompleted(Box::new(
                    crate::event::ExternalMessageAuthCompletedCommand {
                        client_id,
                        request,
                        result,
                    },
                )),
            ));
        });
        None
    }

    pub(crate) fn complete_external_agent_message_auth(
        &mut self,
        request: tau_proto::ExternalAgentMessageRequest,
        result: Result<(), String>,
    ) -> tau_proto::ExternalAgentMessageResult {
        let request_id = request.request_id.clone();
        let result = result.and_then(|()| self.receive_external_agent_message(request));
        tau_proto::ExternalAgentMessageResult {
            request_id,
            error: result.err(),
        }
    }

    #[cfg(test)]
    pub(crate) fn handle_external_agent_message_request_without_auth_for_test(
        &mut self,
        request: tau_proto::ExternalAgentMessageRequest,
    ) -> tau_proto::ExternalAgentMessageResult {
        let request_id = request.request_id.clone();
        let result = self.receive_external_agent_message(request);
        tau_proto::ExternalAgentMessageResult {
            request_id,
            error: result.err(),
        }
    }

    fn receive_external_agent_message(
        &mut self,
        request: tau_proto::ExternalAgentMessageRequest,
    ) -> Result<(), String> {
        self.validate_external_agent_message_target(&request)?;
        self.publish_event(
            Some(HARNESS_CONNECTION_ID),
            Event::AgentMessageReceived(AgentMessageReceived {
                message_id: request.message_id,
                sender_id: request.sender_id,
                sender_session_id: Some(request.sender_session_id),
                recipient_id: request.recipient_id,
                kind: request.kind,
                message: request.message,
            }),
        );
        Ok(())
    }

    fn validate_external_agent_message_target(
        &self,
        request: &tau_proto::ExternalAgentMessageRequest,
    ) -> Result<(), String> {
        if request.recipient_session_id != self.current_session_id {
            return Err(format!(
                "target harness is on active session `{}`, not `{}`",
                self.current_session_id, request.recipient_session_id
            ));
        }
        if request.message.trim().is_empty() {
            return Err("`message` must not be empty".to_owned());
        }
        match self.agent_message_recipient_status(request.recipient_id.as_str()) {
            AgentMessageRecipientStatus::Live => {}
            AgentMessageRecipientStatus::Stopped => {
                return Err(format!(
                    "stopped message recipient: `{}`",
                    request.recipient_id
                ));
            }
            AgentMessageRecipientStatus::Unknown => {
                return Err(format!(
                    "unknown message recipient: `{}`",
                    request.recipient_id
                ));
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn handle_message_tool_call(
        &mut self,
        agent_id: &AgentId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        let call_id: ToolCallId = call.id.clone();
        self.ensure_harness_owned_tool_tracking(agent_id, call, &visible_tool_name);
        let result = parse_message_args(&call.arguments).and_then(|parsed| {
            self.publish_agent_message_from_agent(agent_id, parsed.recipient_id, parsed.message)
        });
        match result {
            Ok(()) => self.finish_harness_owned_tool_with_result(
                agent_id,
                call_id,
                visible_tool_name,
                call.tool_type,
                "Message sent".to_owned(),
                None,
            ),
            Err(message) => self.finish_harness_owned_tool_with_error(
                agent_id,
                call_id,
                visible_tool_name,
                call.tool_type,
                message,
                Some(call.arguments.clone()),
            ),
        }
        Ok(())
    }

    /// Handle the harness-owned `wait` tool call inline.
    pub(crate) fn handle_wait_tool_call(
        &mut self,
        agent_id: &AgentId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        let call_id: ToolCallId = call.id.clone();
        self.ensure_harness_owned_tool_tracking(agent_id, call, &visible_tool_name);
        if self.has_pending_wait_preempting_prompt(agent_id) {
            let reply = match parse_wait_args(&call.arguments) {
                Ok(WaitTarget::Exact(target)) => {
                    if !self
                        .subagents
                        .wait_tracker
                        .call_is_owned_by(&target, agent_id)
                    {
                        wait_error_reply(
                            call_id,
                            visible_tool_name,
                            format!("unknown tool call: `{target}`"),
                            None,
                        )
                    } else {
                        let source_tool_name = self
                            .subagents
                            .wait_tracker
                            .call_tool_names
                            .get(&target)
                            .cloned();
                        wait_interrupted_reply(
                            call_id,
                            visible_tool_name,
                            source_tool_name,
                            &target,
                        )
                    }
                }
                Ok(WaitTarget::AnyBackground) => {
                    wait_interrupted_any_reply(call_id, visible_tool_name)
                }
                Err(message) => wait_error_reply(call_id, visible_tool_name, message, None),
            };
            self.publish_wait_replies(vec![reply]);
            return Ok(());
        }
        let start = self.subagents.wait_tracker.handle_wait_invoke(
            agent_id,
            call_id,
            visible_tool_name,
            &call.arguments,
        );
        if let Some(target) = start.suppress_call_id {
            self.suppress_background_completion_prompt(target);
        }
        self.publish_wait_replies(start.reply.into_iter().collect());
        Ok(())
    }

    fn has_pending_wait_preempting_prompt(&self, agent_id: &AgentId) -> bool {
        self.agents.get(agent_id).is_some_and(|agent| {
            // `submit_prompt_to_agent` and `handle_agent_message_received` both
            // interrupt waits that are active at queue time. This start-time
            // guard closes the converse race where the prompt/message queued
            // first and a later provider tool call tries to start a passive
            // wait behind that already-pending input.
            agent
                .pending_prompts
                .iter()
                .any(|prompt| !prompt.is_internal() || prompt.is_agent_message_received())
        })
    }

    pub(crate) fn ensure_harness_owned_tool_tracking(
        &mut self,
        cid: &AgentId,
        call: &AgentToolCall,
        visible_tool_name: &ToolName,
    ) {
        if self.tool_agents.contains_key(&call.id) {
            return;
        }
        self.tool_agents.insert(call.id.clone(), cid.clone());
        self.pending_tools.insert(
            call.id.clone(),
            crate::harness::PendingTool {
                name: visible_tool_name.clone(),
                internal_name: call.name.clone(),
                tool_type: call.tool_type,
            },
        );
        self.bump_tools_started_for(cid);
    }

    pub(crate) fn finish_harness_owned_tool_with_result(
        &mut self,
        cid: &AgentId,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: ToolType,
        result: String,
        details: Option<CborValue>,
    ) {
        self.finish_harness_owned_tool_with_cbor_result(
            cid,
            call_id,
            tool_name,
            tool_type,
            details.unwrap_or(CborValue::Text(result)),
            None,
        );
    }

    pub(crate) fn finish_harness_owned_tool_with_cbor_result(
        &mut self,
        cid: &AgentId,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: ToolType,
        result: CborValue,
        display: Option<tau_proto::ToolUseState>,
    ) {
        let result = ToolResult {
            call_id: call_id.clone(),
            tool_name,
            tool_type,
            result,
            kind: ToolResultKind::Final,
            display,
            originator: tau_proto::PromptOriginator::User,
        };
        self.publish_terminal_tool_result(Some(cid), None, result);
        self.on_tool_call_complete(call_id.as_str());
        self.clear_tool_call_tracking(call_id.as_str());
    }

    pub(crate) fn finish_harness_owned_tool_with_error(
        &mut self,
        cid: &AgentId,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: ToolType,
        message: String,
        details: Option<CborValue>,
    ) {
        self.finish_harness_owned_tool_with_display_error(
            cid, call_id, tool_name, tool_type, message, details, None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_harness_owned_tool_with_display_error(
        &mut self,
        cid: &AgentId,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: ToolType,
        message: String,
        details: Option<CborValue>,
        display: Option<tau_proto::ToolUseState>,
    ) {
        let error = ToolError {
            call_id: call_id.clone(),
            tool_name,
            tool_type,
            message,
            details,
            display,
            originator: tau_proto::PromptOriginator::User,
        };
        self.publish_terminal_tool_error(Some(cid), None, error);
        self.on_tool_call_complete(call_id.as_str());
        self.clear_tool_call_tracking(call_id.as_str());
    }

    pub(crate) fn finish_prebuilt_internal_tool_result(&mut self, result: ToolResult) {
        let call_id = result.call_id.clone();
        let Some(owner_cid) = self.tool_agents.get(&call_id).cloned() else {
            return;
        };
        if self.tool_turn.is_backgrounded(&call_id) {
            self.handle_background_tool_result(HARNESS_CONNECTION_ID, result);
        } else {
            self.publish_terminal_tool_result(Some(&owner_cid), None, result);
            self.on_tool_call_complete(call_id.as_str());
            self.clear_tool_call_tracking(call_id.as_str());
        }
    }

    pub(crate) fn finish_prebuilt_internal_tool_error(&mut self, error: ToolError) {
        let call_id = error.call_id.clone();
        let Some(owner_cid) = self.tool_agents.get(&call_id).cloned() else {
            return;
        };
        if self.tool_turn.is_backgrounded(&call_id) {
            self.handle_background_tool_error(Some(HARNESS_CONNECTION_ID), error);
        } else {
            self.publish_terminal_tool_error(Some(&owner_cid), None, error);
            self.on_tool_call_complete(call_id.as_str());
            self.clear_tool_call_tracking(call_id.as_str());
        }
    }

    fn publish_wait_replies(&mut self, replies: Vec<WaitReply>) {
        for reply in replies {
            if let Some(call_id) = reply.unsuppress_call_id.clone() {
                self.unsuppress_background_completion_prompt(call_id);
            }
            if let Some(call_id) = reply.suppress_call_id.clone() {
                self.suppress_background_completion_prompt(call_id);
            }
            let wait_call_id = reply.wait_call_id.clone();
            let Some(cid) = self.tool_agents.get(&wait_call_id).cloned() else {
                continue;
            };
            match reply.kind {
                WaitReplyKind::Result { result, display } => {
                    self.publish_terminal_tool_result(
                        Some(&cid),
                        None,
                        ToolResult {
                            call_id: reply.wait_call_id,
                            tool_name: reply.wait_tool_name,
                            tool_type: ToolType::Function,
                            result,
                            kind: ToolResultKind::Final,
                            display,
                            originator: tau_proto::PromptOriginator::User,
                        },
                    );
                }
                WaitReplyKind::Error {
                    message,
                    details,
                    display,
                } => {
                    self.publish_terminal_tool_error(
                        Some(&cid),
                        None,
                        ToolError {
                            call_id: reply.wait_call_id,
                            tool_name: reply.wait_tool_name,
                            tool_type: ToolType::Function,
                            message,
                            details,
                            display,
                            originator: tau_proto::PromptOriginator::User,
                        },
                    );
                }
            }
            self.on_tool_call_complete(wait_call_id.as_str());
            self.clear_tool_call_tracking(wait_call_id.as_str());
        }
    }
}

/// State needed to complete an external message tool call after async delivery.
pub(crate) struct ExternalMessageToolCompletion {
    /// Conversation that owns the tool call.
    pub(crate) conversation_id: AgentId,
    /// Session generation active when the tool call was dispatched.
    pub(crate) session_generation: u64,
    /// Tool call id to complete.
    pub(crate) call_id: ToolCallId,
    /// Visible tool name for the result/error.
    pub(crate) tool_name: ToolName,
    /// Tool type declared on the call.
    pub(crate) tool_type: ToolType,
    /// Original arguments used as error details.
    pub(crate) details: CborValue,
}

fn random_external_message_capability(rng: &mut rand::rngs::StdRng) -> String {
    use rand::Rng as _;
    use rand::distributions::Alphanumeric;

    rng.sample_iter(Alphanumeric)
        .take(48)
        .map(char::from)
        .collect()
}

fn authenticate_external_agent_message_sender(
    request: &tau_proto::ExternalAgentMessageRequest,
) -> Result<(), String> {
    let harness_path =
        crate::runtime_dir::find_harness_for_session(request.sender_session_id.as_str())
            .map_err(|err| format!("failed to find sender harness: {err}"))?
            .ok_or_else(|| {
                format!(
                    "no running daemon for sender session `{}`",
                    request.sender_session_id
                )
            })?;
    let socket = crate::runtime_dir::socket_path(&harness_path);
    let mut peer = tau_socket::SocketPeer::connect(&socket)
        .map_err(|err| format!("failed to connect to sender harness: {err}"))?;
    peer.send(&tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME.into(),
        client_kind: tau_proto::ClientKind::External,
    }))
    .map_err(|err| format!("failed to send external auth hello: {err}"))?;
    let auth_request_id = format!("auth-{}", request.request_id);
    peer.send(&tau_proto::HarnessInputMessage::ExternalAgentMessageAuth(
        tau_proto::ExternalAgentMessageAuthRequest {
            request_id: auth_request_id.clone(),
            message_id: request.message_id.clone(),
            capability: request.capability.clone(),
            sender_session_id: request.sender_session_id.clone(),
            sender_id: request.sender_id.clone(),
            recipient_session_id: request.recipient_session_id.clone(),
            recipient_id: request.recipient_id.clone(),
            kind: request.kind,
            message: request.message.clone(),
        },
    ))
    .map_err(|err| format!("failed to send external auth request: {err}"))?;
    let deadline = Instant::now() + EXTERNAL_AGENT_MESSAGE_AUTH_TIMEOUT;
    loop {
        let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
            return Err(format!(
                "timed out after {}s waiting for external message authentication",
                EXTERNAL_AGENT_MESSAGE_AUTH_TIMEOUT.as_secs()
            ));
        };
        match peer
            .recv_timeout(timeout)
            .map_err(|err| format!("failed to receive external auth result: {err}"))?
        {
            tau_socket::SocketReceive::Message {
                message: tau_proto::HarnessOutputMessage::ExternalAgentMessageAuthResult(result),
            } if result.request_id == auth_request_id => {
                if result.authorized {
                    return Ok(());
                }
                return Err(result
                    .error
                    .unwrap_or_else(|| "sender rejected external message capability".to_owned()));
            }
            tau_socket::SocketReceive::Message { .. } => continue,
            tau_socket::SocketReceive::Timeout => {
                return Err(format!(
                    "timed out after {}s waiting for external message authentication",
                    EXTERNAL_AGENT_MESSAGE_AUTH_TIMEOUT.as_secs()
                ));
            }
            tau_socket::SocketReceive::Closed => {
                return Err("sender harness closed before external auth result".to_owned());
            }
        }
    }
}

fn send_external_agent_message_request(
    request: tau_proto::ExternalAgentMessageRequest,
) -> Result<(), String> {
    let harness_path =
        crate::runtime_dir::find_harness_for_session(request.recipient_session_id.as_str())
            .map_err(|err| err.to_string())?
            .ok_or_else(|| {
                format!(
                    "no running daemon for session `{}`",
                    request.recipient_session_id
                )
            })?;
    let socket = crate::runtime_dir::socket_path(&harness_path);
    let mut peer = tau_socket::SocketPeer::connect(&socket)
        .map_err(|err| format!("failed to connect to target harness: {err}"))?;
    peer.send(&tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME.into(),
        client_kind: tau_proto::ClientKind::External,
    }))
    .map_err(|err| format!("failed to send external message hello: {err}"))?;
    peer.send(&tau_proto::HarnessInputMessage::ExternalAgentMessage(
        request.clone(),
    ))
    .map_err(|err| format!("failed to send external message request: {err}"))?;
    let deadline = Instant::now() + EXTERNAL_AGENT_MESSAGE_RESULT_TIMEOUT;
    loop {
        let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
            return Err(format!(
                "timed out after {}s waiting for external message result",
                EXTERNAL_AGENT_MESSAGE_RESULT_TIMEOUT.as_secs()
            ));
        };
        match peer
            .recv_timeout(timeout)
            .map_err(|err| format!("failed to receive external message result: {err}"))?
        {
            tau_socket::SocketReceive::Message {
                message: tau_proto::HarnessOutputMessage::ExternalAgentMessageResult(result),
            } if result.request_id == request.request_id => {
                return result.error.map_or(Ok(()), Err);
            }
            tau_socket::SocketReceive::Message { .. } => continue,
            tau_socket::SocketReceive::Timeout => {
                return Err(format!(
                    "timed out after {}s waiting for external message result",
                    EXTERNAL_AGENT_MESSAGE_RESULT_TIMEOUT.as_secs()
                ));
            }
            tau_socket::SocketReceive::Closed => {
                return Err("target harness closed before external message result".to_owned());
            }
        }
    }
}

#[cfg(test)]
struct TestBuiltinTools;

#[cfg(test)]
impl crate::InternalToolHandler for TestBuiltinTools {
    fn tool_specs(&self) -> Vec<tau_proto::ToolSpec> {
        vec![
            test_tool_spec("skill", None),
            wait_tool_spec(),
            message_tool_spec(),
        ]
    }

    fn handles(&self, internal_tool_name: &ToolName) -> bool {
        matches!(
            internal_tool_name.as_str(),
            "skill" | WAIT_TOOL_NAME | MESSAGE_TOOL_NAME
        )
    }

    fn handle_event(
        &self,
        host: &mut crate::InternalToolHost<'_>,
        event: &tau_proto::Event,
    ) -> Result<(), HarnessError> {
        let tau_proto::Event::ToolStarted(started) = event else {
            return Ok(());
        };
        let Some((conversation_id, call, visible_tool_name)) = host.internal_started_call(started)
        else {
            return Ok(());
        };
        match call.name.as_str() {
            "skill" => Ok(()),
            WAIT_TOOL_NAME => {
                host.handle_wait_tool_call(&conversation_id, &call, visible_tool_name)
            }
            MESSAGE_TOOL_NAME => {
                host.handle_message_tool_call(&conversation_id, &call, visible_tool_name)
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
fn message_tool_spec() -> tau_proto::ToolSpec {
    let mut spec = test_tool_spec(MESSAGE_TOOL_NAME, Some(tau_proto::BackgroundSupport::Never));
    spec.parameters = Some(serde_json::json!({
        "type":"object",
        "required":["recipient_id","message"]
    }));
    spec
}

#[cfg(test)]
fn wait_tool_spec() -> tau_proto::ToolSpec {
    test_tool_spec(WAIT_TOOL_NAME, Some(tau_proto::BackgroundSupport::Never))
}

#[cfg(test)]
fn test_tool_spec(
    name: &str,
    background_support: Option<tau_proto::BackgroundSupport>,
) -> tau_proto::ToolSpec {
    tau_proto::ToolSpec {
        name: ToolName::new(name),
        model_visible_name: None,
        description: Some(name.to_owned()),
        tool_type: ToolType::Function,
        parameters: Some(serde_json::json!({"type":"object"})),
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support,
        examples: Vec::new(),
    }
}

#[cfg(test)]
#[derive(Debug, PartialEq)]
struct MessageArgs {
    recipient_id: String,
    message: String,
}

#[cfg(test)]
fn parse_message_args(arguments: &CborValue) -> Result<MessageArgs, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    let mut recipient_id = None;
    let mut message = None;
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        match name.as_str() {
            "recipient_id" => match v {
                CborValue::Text(text) => recipient_id = Some(text.clone()),
                _ => return Err("`recipient_id` must be a string".to_owned()),
            },
            "message" => match v {
                CborValue::Text(text) => message = Some(text.clone()),
                _ => return Err("`message` must be a string".to_owned()),
            },
            _ => {}
        }
    }
    let recipient_id = recipient_id.ok_or_else(|| "`recipient_id` is required".to_owned())?;
    if recipient_id.trim().is_empty() {
        return Err("`recipient_id` must not be empty".to_owned());
    }
    let message = message.ok_or_else(|| "`message` is required".to_owned())?;
    if message.trim().is_empty() {
        return Err("`message` must not be empty".to_owned());
    }
    Ok(MessageArgs {
        recipient_id,
        message,
    })
}

const ORIGINAL_TOOL_CALL_ID_HEADER: &str = "original_tool_call_id";

#[derive(Clone, Debug, PartialEq)]
enum WaitTarget {
    Exact(ToolCallId),
    AnyBackground,
}

#[derive(Clone, Debug, PartialEq)]
enum WaitCallState {
    Pending,
    Backgrounded,
    NormalReturned,
    BackgroundResult(ToolBackgroundResult),
    BackgroundError(ToolBackgroundError),
    Consumed,
}

#[derive(Clone, Debug, PartialEq)]
struct WaitRequest {
    call_id: ToolCallId,
    tool_name: ToolName,
    owner: AgentId,
}

#[derive(Clone, Debug, PartialEq)]
enum WaitReplyKind {
    Result {
        result: CborValue,
        display: Option<ToolUseState>,
    },
    Error {
        message: String,
        details: Option<CborValue>,
        display: Option<ToolUseState>,
    },
}

#[derive(Clone, Debug, PartialEq)]
struct WaitReply {
    wait_call_id: ToolCallId,
    wait_tool_name: ToolName,
    kind: WaitReplyKind,
    suppress_call_id: Option<ToolCallId>,
    unsuppress_call_id: Option<ToolCallId>,
}

#[derive(Clone, Debug, PartialEq, Default)]
struct WaitStart {
    reply: Option<WaitReply>,
    suppress_call_id: Option<ToolCallId>,
}

#[derive(Clone, Debug, PartialEq, Default)]
struct WaitCancel {
    replies: Vec<WaitReply>,
    unsuppress_call_ids: Vec<ToolCallId>,
    suppress_call_ids: Vec<ToolCallId>,
}

#[derive(Default)]
struct WaitTracker {
    calls: HashMap<ToolCallId, WaitCallState>,
    waiters: HashMap<ToolCallId, WaitRequest>,
    any_waiters: HashMap<AgentId, WaitRequest>,
    call_owners: HashMap<ToolCallId, AgentId>,
    call_tool_names: HashMap<ToolCallId, ToolName>,
    completion_order: VecDeque<ToolCallId>,
}

impl WaitTracker {
    fn record_tool_invoke(&mut self, call_id: ToolCallId, tool_name: ToolName, owner: AgentId) {
        if tool_name.as_str() != WAIT_TOOL_NAME {
            self.call_tool_names
                .insert(call_id.clone(), tool_name.clone());
            self.call_owners.insert(call_id.clone(), owner);
            self.calls.entry(call_id).or_insert(WaitCallState::Pending);
        }
    }

    fn handle_wait_invoke(
        &mut self,
        owner: &AgentId,
        call_id: ToolCallId,
        tool_name: ToolName,
        arguments: &CborValue,
    ) -> WaitStart {
        let target = match parse_wait_args(arguments) {
            Ok(target) => target,
            Err(message) => {
                return WaitStart::reply(wait_error_reply(
                    call_id,
                    tool_name,
                    message,
                    Some(arguments.clone()),
                ));
            }
        };
        let wait = WaitRequest {
            call_id,
            tool_name,
            owner: owner.clone(),
        };
        match target {
            WaitTarget::Exact(target) => self.start_exact_wait(target, wait),
            WaitTarget::AnyBackground => self.start_any_wait(owner.clone(), wait),
        }
    }

    fn start_exact_wait(&mut self, target: ToolCallId, wait: WaitRequest) -> WaitStart {
        if !self.call_is_owned_by(&target, &wait.owner) {
            return WaitStart::reply(wait_error_reply(
                wait.call_id,
                wait.tool_name,
                format!("unknown tool call: `{target}`"),
                None,
            ));
        }
        if self.waiters.contains_key(&target) {
            return WaitStart::reply(wait_error_reply(
                wait.call_id,
                wait.tool_name,
                "existing wait for this tool already in progress".to_owned(),
                None,
            ));
        }
        let state = self.calls.remove(&target);
        match state {
            Some(WaitCallState::Pending) => {
                self.calls.insert(target.clone(), WaitCallState::Pending);
                self.waiters.insert(target, wait);
                WaitStart::default()
            }
            Some(WaitCallState::Backgrounded) => {
                self.calls
                    .insert(target.clone(), WaitCallState::Backgrounded);
                self.waiters.insert(target.clone(), wait);
                WaitStart::suppress(target)
            }
            Some(WaitCallState::NormalReturned) => {
                self.calls.insert(target.clone(), WaitCallState::Consumed);
                let source_tool_name = self.call_tool_names.get(&target).cloned();
                WaitStart::reply(
                    wait_error_reply(
                        wait.call_id,
                        wait.tool_name,
                        format!("Tool call {target} returned normally, not backgrounded"),
                        None,
                    )
                    .with_source_display(source_tool_name, None),
                )
            }
            Some(WaitCallState::BackgroundResult(result)) => {
                self.calls.insert(target.clone(), WaitCallState::Consumed);
                self.remove_completed(&target);
                let source_tool_name = Some(result.tool_name.clone());
                WaitStart::reply_with_suppress(
                    wait_result_reply(
                        wait.call_id,
                        wait.tool_name,
                        source_tool_name,
                        result.result,
                        result.display,
                    ),
                    target,
                )
            }
            Some(WaitCallState::BackgroundError(error)) => {
                self.calls.insert(target.clone(), WaitCallState::Consumed);
                self.remove_completed(&target);
                let source_tool_name = Some(error.tool_name.clone());
                WaitStart::reply_with_suppress(
                    wait_error_reply(wait.call_id, wait.tool_name, error.message, error.details)
                        .with_source_display(source_tool_name, error.display),
                    target,
                )
            }
            Some(WaitCallState::Consumed) => {
                let source_tool_name = self.call_tool_names.get(&target).cloned();
                WaitStart::reply(
                    wait_error_reply(
                        wait.call_id,
                        wait.tool_name,
                        format!("result for tool call `{target}` already consumed"),
                        None,
                    )
                    .with_source_display(source_tool_name, None),
                )
            }
            None => WaitStart::reply(wait_error_reply(
                wait.call_id,
                wait.tool_name,
                format!("unknown tool call: `{target}`"),
                None,
            )),
        }
    }

    fn call_is_owned_by(&self, call_id: &ToolCallId, owner: &AgentId) -> bool {
        self.call_owners.get(call_id) == Some(owner)
    }

    fn start_any_wait(&mut self, owner: AgentId, wait: WaitRequest) -> WaitStart {
        if self.any_waiters.contains_key(&owner) {
            return WaitStart::reply(wait_error_reply(
                wait.call_id,
                wait.tool_name,
                "existing wait for a background tool call in this conversation already in progress"
                    .to_owned(),
                None,
            ));
        }
        if let Some(target) = self.oldest_completed_for_owner(&owner) {
            return self.consume_completed_for_any(target, wait);
        }
        if self.has_running_background_for_owner(&owner) {
            self.any_waiters.insert(owner, wait);
            return WaitStart::default();
        }
        WaitStart::reply(wait_error_reply(
            wait.call_id,
            wait.tool_name,
            "no background tool calls are running or completed in this conversation".to_owned(),
            None,
        ))
    }

    fn consume_completed_for_any(&mut self, target: ToolCallId, wait: WaitRequest) -> WaitStart {
        let Some(state) = self.calls.remove(&target) else {
            return WaitStart::reply(wait_error_reply(
                wait.call_id,
                wait.tool_name,
                format!("unknown tool call: `{target}`"),
                None,
            ));
        };
        self.calls.insert(target.clone(), WaitCallState::Consumed);
        self.remove_completed(&target);
        match state {
            WaitCallState::BackgroundResult(result) => {
                let source_tool_name = Some(result.tool_name.clone());
                WaitStart::reply_with_suppress(
                    wait_result_reply(
                        wait.call_id,
                        wait.tool_name,
                        source_tool_name,
                        result_with_original_tool_call_id(&target, result.result),
                        result.display,
                    ),
                    target,
                )
            }
            WaitCallState::BackgroundError(error) => {
                let source_tool_name = Some(error.tool_name.clone());
                WaitStart::reply_with_suppress(
                    wait_error_reply(
                        wait.call_id,
                        wait.tool_name,
                        error.message,
                        details_with_original_tool_call_id(&target, error.details),
                    )
                    .with_source_display(source_tool_name, error.display),
                    target,
                )
            }
            other => {
                self.calls.insert(target.clone(), other);
                let source_tool_name = self.call_tool_names.get(&target).cloned();
                WaitStart::reply(
                    wait_error_reply(
                        wait.call_id,
                        wait.tool_name,
                        format!("tool call `{target}` has no completed background result"),
                        None,
                    )
                    .with_source_display(source_tool_name, None),
                )
            }
        }
    }

    fn record_tool_result(&mut self, result: ToolResult, owner: AgentId) -> Vec<WaitReply> {
        if result.tool_name.as_str() == WAIT_TOOL_NAME {
            return Vec::new();
        }
        let call_id = result.call_id.clone();
        self.call_tool_names
            .insert(call_id.clone(), result.tool_name.clone());
        self.call_owners.insert(call_id.clone(), owner);
        if self.is_consumed(&call_id) || self.is_backgrounded(&call_id) {
            return Vec::new();
        }
        if result.kind == ToolResultKind::BackgroundPlaceholder {
            self.calls.insert(call_id, WaitCallState::Backgrounded);
            return Vec::new();
        }
        if let Some(wait) = self.waiters.remove(&call_id) {
            self.calls.insert(call_id, WaitCallState::Consumed);
            let source_tool_name = Some(result.tool_name.clone());
            return vec![wait_result_reply(
                wait.call_id,
                wait.tool_name,
                source_tool_name,
                result.result,
                result.display,
            )];
        }
        self.calls.insert(call_id, WaitCallState::NormalReturned);
        Vec::new()
    }

    fn record_tool_error(&mut self, error: ToolError, owner: AgentId) -> Vec<WaitReply> {
        if error.tool_name.as_str() == WAIT_TOOL_NAME {
            return Vec::new();
        }
        let call_id = error.call_id.clone();
        self.call_tool_names
            .insert(call_id.clone(), error.tool_name.clone());
        self.call_owners.insert(call_id.clone(), owner);
        if self.is_consumed(&call_id) {
            return Vec::new();
        }
        if let Some(wait) = self.waiters.remove(&call_id) {
            self.calls.insert(call_id, WaitCallState::Consumed);
            let source_tool_name = Some(error.tool_name.clone());
            return vec![
                wait_error_reply(wait.call_id, wait.tool_name, error.message, error.details)
                    .with_source_display(source_tool_name, error.display),
            ];
        }
        self.calls.insert(call_id, WaitCallState::NormalReturned);
        Vec::new()
    }

    fn record_background_result(
        &mut self,
        result: ToolBackgroundResult,
        owner: AgentId,
    ) -> Vec<WaitReply> {
        if result.tool_name.as_str() == WAIT_TOOL_NAME {
            return Vec::new();
        }
        let call_id = result.call_id.clone();
        self.call_tool_names
            .insert(call_id.clone(), result.tool_name.clone());
        self.call_owners.insert(call_id.clone(), owner.clone());
        if self.is_consumed(&call_id) {
            return Vec::new();
        }
        if let Some(wait) = self.waiters.remove(&call_id) {
            self.calls.insert(call_id.clone(), WaitCallState::Consumed);
            self.remove_completed(&call_id);
            let source_tool_name = Some(result.tool_name.clone());
            let mut replies = vec![
                wait_result_reply(
                    wait.call_id,
                    wait.tool_name,
                    source_tool_name,
                    result.result,
                    result.display,
                )
                .with_suppress(call_id.clone()),
            ];
            replies.extend(self.finish_any_waiter_if_no_candidates(&owner));
            return replies;
        }
        if let Some(wait) = self.any_waiters.remove(&owner) {
            self.calls.insert(call_id.clone(), WaitCallState::Consumed);
            self.remove_completed(&call_id);
            return vec![
                wait_result_reply(
                    wait.call_id,
                    wait.tool_name,
                    Some(result.tool_name.clone()),
                    result_with_original_tool_call_id(&call_id, result.result),
                    result.display,
                )
                .with_suppress(call_id),
            ];
        }
        self.calls
            .insert(call_id.clone(), WaitCallState::BackgroundResult(result));
        self.push_completed(call_id);
        Vec::new()
    }

    fn record_background_error(
        &mut self,
        error: ToolBackgroundError,
        owner: AgentId,
    ) -> Vec<WaitReply> {
        if error.tool_name.as_str() == WAIT_TOOL_NAME {
            return Vec::new();
        }
        let call_id = error.call_id.clone();
        self.call_tool_names
            .insert(call_id.clone(), error.tool_name.clone());
        self.call_owners.insert(call_id.clone(), owner.clone());
        if self.is_consumed(&call_id) {
            return Vec::new();
        }
        if let Some(wait) = self.waiters.remove(&call_id) {
            self.calls.insert(call_id.clone(), WaitCallState::Consumed);
            self.remove_completed(&call_id);
            let source_tool_name = Some(error.tool_name.clone());
            let mut replies = vec![
                wait_error_reply(wait.call_id, wait.tool_name, error.message, error.details)
                    .with_source_display(source_tool_name, error.display)
                    .with_suppress(call_id.clone()),
            ];
            replies.extend(self.finish_any_waiter_if_no_candidates(&owner));
            return replies;
        }
        if let Some(wait) = self.any_waiters.remove(&owner) {
            self.calls.insert(call_id.clone(), WaitCallState::Consumed);
            self.remove_completed(&call_id);
            let source_tool_name = Some(error.tool_name.clone());
            return vec![
                wait_error_reply(
                    wait.call_id,
                    wait.tool_name,
                    error.message,
                    details_with_original_tool_call_id(&call_id, error.details),
                )
                .with_source_display(source_tool_name, error.display)
                .with_suppress(call_id),
            ];
        }
        self.calls
            .insert(call_id.clone(), WaitCallState::BackgroundError(error));
        self.push_completed(call_id);
        Vec::new()
    }

    fn record_tool_cancelled(&mut self, call_ids: &HashSet<ToolCallId>) -> WaitCancel {
        if call_ids.is_empty() {
            return WaitCancel::default();
        }

        let cancelled_owners: HashSet<AgentId> = call_ids
            .iter()
            .filter_map(|call_id| self.call_owners.get(call_id).cloned())
            .collect();
        let mut exact_consumed_cancelled = HashSet::new();
        let mut cancelled = WaitCancel::default();
        let waiters = std::mem::take(&mut self.waiters);
        for (target, wait) in waiters {
            let target_cancelled = call_ids.contains(&target);
            let wait_cancelled = call_ids.contains(&wait.call_id);
            let target_was_backgrounded = self.is_backgrounded(&target);

            if wait_cancelled {
                if target_was_backgrounded {
                    cancelled.unsuppress_call_ids.push(target.clone());
                }
                continue;
            }
            if target_cancelled {
                let source_tool_name = self.call_tool_names.get(&target).cloned();
                let mut reply = wait_error_reply(
                    wait.call_id,
                    wait.tool_name,
                    format!("Tool call `{target}` was cancelled"),
                    None,
                )
                .with_source_display(source_tool_name, None);
                if target_was_backgrounded {
                    reply = reply.with_unsuppress(target.clone());
                }
                exact_consumed_cancelled.insert(target.clone());
                cancelled.replies.push(reply);
            } else {
                self.waiters.insert(target, wait);
            }
        }

        for call_id in call_ids {
            if exact_consumed_cancelled.contains(call_id) {
                self.calls.insert(call_id.clone(), WaitCallState::Consumed);
                self.remove_completed(call_id);
            } else if self.is_backgrounded(call_id) {
                self.calls.insert(
                    call_id.clone(),
                    WaitCallState::BackgroundError(ToolBackgroundError {
                        call_id: call_id.clone(),
                        tool_name: self
                            .call_tool_names
                            .get(call_id)
                            .cloned()
                            .unwrap_or_else(|| ToolName::new("cancelled")),
                        tool_type: ToolType::Function,
                        message: "Tool call canceled".to_owned(),
                        details: None,
                        originator: tau_proto::PromptOriginator::User,

                        display: None,
                    }),
                );
                self.push_completed(call_id.clone());
            } else {
                self.calls.insert(call_id.clone(), WaitCallState::Consumed);
                self.remove_completed(call_id);
            }
        }

        let any_waiters = std::mem::take(&mut self.any_waiters);
        for (owner, wait) in any_waiters {
            if call_ids.contains(&wait.call_id) {
                continue;
            }
            if let Some(target) = self.oldest_completed_for_owner(&owner) {
                let start = self.consume_completed_for_any(target, wait);
                if let Some(call_id) = start.suppress_call_id {
                    cancelled.suppress_call_ids.push(call_id);
                }
                cancelled.replies.extend(start.reply);
            } else if self.has_running_background_for_owner(&owner) {
                self.any_waiters.insert(owner, wait);
            } else if cancelled_owners.contains(&owner) {
                let source_tool_name = call_ids.iter().find_map(|call_id| {
                    if self.call_owners.get(call_id) == Some(&owner) {
                        self.call_tool_names.get(call_id).cloned()
                    } else {
                        None
                    }
                });
                cancelled.replies.push(
                    wait_error_reply(
                        wait.call_id,
                        wait.tool_name,
                        "background tool call in this conversation was cancelled".to_owned(),
                        None,
                    )
                    .with_source_display(source_tool_name, None),
                );
            } else {
                self.any_waiters.insert(owner, wait);
            }
        }

        cancelled
    }

    fn interrupt_active_waits(&mut self) -> Vec<WaitReply> {
        let waiters = std::mem::take(&mut self.waiters);
        let mut replies: Vec<WaitReply> = waiters
            .into_iter()
            .map(|(target, wait)| self.interrupted_exact_wait_reply(target, wait))
            .collect();
        replies.extend(
            std::mem::take(&mut self.any_waiters)
                .into_values()
                .map(|wait| wait_interrupted_any_reply(wait.call_id, wait.tool_name)),
        );
        replies
    }

    fn interrupt_active_waits_for(&mut self, owner: &AgentId) -> Vec<WaitReply> {
        let targets: Vec<ToolCallId> = self
            .waiters
            .keys()
            .filter(|target| {
                self.waiters
                    .get(*target)
                    .is_some_and(|wait| &wait.owner == owner)
            })
            .cloned()
            .collect();
        let mut replies: Vec<WaitReply> = targets
            .into_iter()
            .filter_map(|target| {
                self.waiters
                    .remove(&target)
                    .map(|wait| self.interrupted_exact_wait_reply(target, wait))
            })
            .collect();
        if let Some(wait) = self.any_waiters.remove(owner) {
            replies.push(wait_interrupted_any_reply(wait.call_id, wait.tool_name));
        }
        replies
    }

    fn interrupted_exact_wait_reply(&self, target: ToolCallId, wait: WaitRequest) -> WaitReply {
        let source_tool_name = self.call_tool_names.get(&target).cloned();
        let mut reply =
            wait_interrupted_reply(wait.call_id, wait.tool_name, source_tool_name, &target);
        if self.is_backgrounded(&target) {
            reply = reply.with_unsuppress(target);
        }
        reply
    }

    fn transfer_call_owner(&mut self, call_id: &ToolCallId, source: &AgentId, target: &AgentId) {
        if !self.calls.contains_key(call_id) {
            return;
        }
        match self.call_owners.get(call_id) {
            Some(owner) if owner != source => {}
            _ => {
                self.call_owners.insert(call_id.clone(), target.clone());
            }
        }
    }

    fn discard_call_owner(&mut self, call_id: &ToolCallId, source: &AgentId) {
        if self.call_owners.get(call_id) == Some(source) {
            self.call_owners.remove(call_id);
            self.call_tool_names.remove(call_id);
        }
        if self.calls.get(call_id) == Some(&WaitCallState::Backgrounded) {
            self.calls.remove(call_id);
        }
        self.completion_order
            .retain(|completed| completed != call_id);
    }

    fn finish_any_waiter_if_no_candidates(&mut self, owner: &AgentId) -> Vec<WaitReply> {
        if self.oldest_completed_for_owner(owner).is_some()
            || self.has_running_background_for_owner(owner)
        {
            return Vec::new();
        }
        let Some(wait) = self.any_waiters.remove(owner) else {
            return Vec::new();
        };
        vec![wait_error_reply(
            wait.call_id,
            wait.tool_name,
            "no background tool calls are running or completed in this conversation".to_owned(),
            None,
        )]
    }

    fn oldest_completed_for_owner(&self, owner: &AgentId) -> Option<ToolCallId> {
        self.completion_order.iter().find_map(|call_id| {
            (self.call_owners.get(call_id) == Some(owner) && self.is_completed(call_id))
                .then_some(call_id.clone())
        })
    }

    fn has_running_background_for_owner(&self, owner: &AgentId) -> bool {
        self.calls.iter().any(|(call_id, state)| {
            matches!(state, WaitCallState::Backgrounded)
                && self.call_owners.get(call_id) == Some(owner)
        })
    }

    fn push_completed(&mut self, call_id: ToolCallId) {
        if self
            .completion_order
            .iter()
            .all(|existing| existing != &call_id)
        {
            self.completion_order.push_back(call_id);
        }
    }

    fn remove_completed(&mut self, call_id: &ToolCallId) {
        self.completion_order.retain(|existing| existing != call_id);
    }

    fn is_backgrounded(&self, call_id: &ToolCallId) -> bool {
        self.calls
            .get(call_id)
            .is_some_and(|state| matches!(state, WaitCallState::Backgrounded))
    }

    fn is_completed(&self, call_id: &ToolCallId) -> bool {
        self.calls.get(call_id).is_some_and(|state| {
            matches!(
                state,
                WaitCallState::BackgroundResult(_) | WaitCallState::BackgroundError(_)
            )
        })
    }

    fn is_consumed(&self, call_id: &ToolCallId) -> bool {
        self.calls
            .get(call_id)
            .is_some_and(|state| matches!(state, WaitCallState::Consumed))
    }
}

impl WaitReply {
    fn with_source_display(
        mut self,
        source_tool_name: Option<ToolName>,
        display: Option<ToolUseState>,
    ) -> Self {
        if let WaitReplyKind::Error {
            message,
            display: dst,
            ..
        } = &mut self.kind
        {
            *dst = Some(wait_display_from_source(
                source_tool_name,
                display,
                ToolUseStatus::Error,
                wait_error_status_text(message),
            ));
        }
        self
    }

    fn with_suppress(mut self, call_id: ToolCallId) -> Self {
        self.suppress_call_id = Some(call_id);
        self
    }

    fn with_unsuppress(mut self, call_id: ToolCallId) -> Self {
        self.unsuppress_call_id = Some(call_id);
        self
    }
}

impl WaitStart {
    fn reply(reply: WaitReply) -> Self {
        Self {
            reply: Some(reply),
            suppress_call_id: None,
        }
    }

    fn suppress(call_id: ToolCallId) -> Self {
        Self {
            reply: None,
            suppress_call_id: Some(call_id),
        }
    }

    fn reply_with_suppress(reply: WaitReply, call_id: ToolCallId) -> Self {
        Self {
            reply: Some(reply),
            suppress_call_id: Some(call_id),
        }
    }
}

fn wait_result_reply(
    wait_call_id: ToolCallId,
    wait_tool_name: ToolName,
    source_tool_name: Option<ToolName>,
    result: CborValue,
    display: Option<ToolUseState>,
) -> WaitReply {
    WaitReply {
        wait_call_id,
        wait_tool_name,
        kind: WaitReplyKind::Result {
            result,
            display: Some(wait_display_from_source(
                source_tool_name,
                display,
                ToolUseStatus::Success,
                "ok".to_owned(),
            )),
        },
        suppress_call_id: None,
        unsuppress_call_id: None,
    }
}

fn wait_display_from_source(
    source_tool_name: Option<ToolName>,
    display: Option<ToolUseState>,
    default_status: ToolUseStatus,
    default_status_text: String,
) -> ToolUseState {
    // The waited tool's descriptor describes the payload returned to the model.
    // Rendering that descriptor under the `wait` tool makes the UI surface
    // arbitrary command/path labels when the source tool happened to provide
    // them. Keep the source tool name plus completion severity for the `wait`
    // call itself.
    let (status, status_text) = display
        .map(|display| (display.status, display.status_text))
        .unwrap_or((default_status, default_status_text));
    ToolUseState {
        args: source_tool_name
            .map(|tool_name| tool_name.to_string())
            .unwrap_or_default(),
        status,
        status_text: wait_display_status_text(status, status_text),
        ..Default::default()
    }
}

fn wait_display_status_text(status: ToolUseStatus, status_text: String) -> String {
    if !status_text.trim().is_empty() {
        return status_text;
    }
    match status {
        ToolUseStatus::Success => "ok".to_owned(),
        ToolUseStatus::Warning => "warning".to_owned(),
        ToolUseStatus::Error => "err".to_owned(),
        ToolUseStatus::InProgress => tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
    }
}

fn wait_error_status_text(message: &str) -> String {
    message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("err")
        .to_owned()
}

fn wait_error_reply(
    wait_call_id: ToolCallId,
    wait_tool_name: ToolName,
    message: String,
    details: Option<CborValue>,
) -> WaitReply {
    WaitReply {
        wait_call_id,
        wait_tool_name,
        kind: WaitReplyKind::Error {
            message,
            details,
            display: None,
        },
        suppress_call_id: None,
        unsuppress_call_id: None,
    }
}

fn wait_interrupted_reply(
    wait_call_id: ToolCallId,
    wait_tool_name: ToolName,
    source_tool_name: Option<ToolName>,
    target_call_id: &ToolCallId,
) -> WaitReply {
    wait_result_reply(
        wait_call_id,
        wait_tool_name,
        source_tool_name,
        CborValue::Text(format!(
            "{}: true\n\nWaiting for tool call `{target_call_id}` was interrupted because new input is queued. Try again later.",
            tau_proto::TAU_INTERNAL_HEADER_NAME
        )),
        None,
    )
}

fn wait_interrupted_any_reply(wait_call_id: ToolCallId, wait_tool_name: ToolName) -> WaitReply {
    wait_result_reply(
        wait_call_id,
        wait_tool_name,
        None,
        CborValue::Text(format!(
            "{}: true\n\nWaiting for a background tool call in this conversation was interrupted because new input is queued. Try again later.",
            tau_proto::TAU_INTERNAL_HEADER_NAME
        )),
        None,
    )
}

fn result_with_original_tool_call_id(
    original_call_id: &ToolCallId,
    result: CborValue,
) -> CborValue {
    let header = original_tool_call_id_entry(original_call_id);
    match result {
        CborValue::Map(mut entries) => {
            entries.insert(0, header);
            CborValue::Map(entries)
        }
        other => CborValue::Map(vec![header, (CborValue::Text("output".to_owned()), other)]),
    }
}

fn details_with_original_tool_call_id(
    original_call_id: &ToolCallId,
    details: Option<CborValue>,
) -> Option<CborValue> {
    let header = original_tool_call_id_entry(original_call_id);
    Some(match details {
        Some(CborValue::Map(mut entries)) => {
            entries.insert(0, header);
            CborValue::Map(entries)
        }
        Some(other) => CborValue::Map(vec![header, (CborValue::Text("details".to_owned()), other)]),
        None => CborValue::Map(vec![header]),
    })
}

fn original_tool_call_id_entry(original_call_id: &ToolCallId) -> (CborValue, CborValue) {
    (
        CborValue::Text(ORIGINAL_TOOL_CALL_ID_HEADER.to_owned()),
        CborValue::Text(original_call_id.to_string()),
    )
}

fn parse_wait_args(arguments: &CborValue) -> Result<WaitTarget, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        if name == "tool_call_id" {
            return match v {
                CborValue::Text(text) => {
                    let text = text.trim();
                    if text.is_empty() {
                        Err("`tool_call_id` must not be empty".to_owned())
                    } else {
                        Ok(WaitTarget::Exact(text.to_owned().into()))
                    }
                }
                _ => Err("`tool_call_id` must be a string".to_owned()),
            };
        }
    }
    Ok(WaitTarget::AnyBackground)
}

#[cfg(test)]
mod tests;
