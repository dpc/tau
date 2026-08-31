//! Injection point for harness-internal tools owned by higher crates.

use std::borrow::Cow;
#[cfg(test)]
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::Arc;

use tau_proto::{
    CborValue, Effort, Event, ModelId, SessionAgentWorkStatus, SessionId, StartAgentRequest,
    ToolCallId, ToolError, ToolName, ToolProgress, ToolResult, ToolSpec, ToolUseState,
};

#[cfg(test)]
use crate::agent as path_crate_agent;
use crate::discovery::DiscoveredSkillSource;
use crate::error::HarnessError;
use crate::harness::Harness;
use crate::{
    AgentId, AgentToolCall, event as path_crate_event, runtime_dir as path_crate_runtime_dir,
};

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
    /// extensions. The harness sends `ToolStarted` only to handlers whose
    /// [`Self::handles`] predicate accepts the resolved internal name. Later
    /// correlation events such as `StartAgentResult` remain broadcast in
    /// handler registration order, so each handler must filter those events
    /// itself.
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

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct InternalToolDispatchWork {
    /// Ownership predicates evaluated while selecting a started tool's handler.
    pub(crate) ownership_predicate_visits: usize,
    /// Handlers invoked after event-specific selection.
    pub(crate) handler_invocations: usize,
    /// Deep argument clones made while materializing an internal call.
    pub(crate) argument_clones: usize,
}

#[cfg(test)]
thread_local! {
    /// Production-path work retained for deterministic dispatch tests.
    static INTERNAL_TOOL_DISPATCH_WORK: Cell<InternalToolDispatchWork> =
        const { Cell::new(InternalToolDispatchWork {
            ownership_predicate_visits: 0,
            handler_invocations: 0,
            argument_clones: 0,
        }) };
}

#[cfg(test)]
fn record_internal_tool_dispatch_work(update: impl FnOnce(&mut InternalToolDispatchWork)) {
    INTERNAL_TOOL_DISPATCH_WORK.with(|work| {
        let mut current = work.get();
        update(&mut current);
        work.set(current);
    });
}

#[cfg(test)]
pub(crate) fn reset_internal_tool_dispatch_work() {
    INTERNAL_TOOL_DISPATCH_WORK.with(|work| work.set(InternalToolDispatchWork::default()));
}

#[cfg(test)]
pub(crate) fn internal_tool_dispatch_work() -> InternalToolDispatchWork {
    INTERNAL_TOOL_DISPATCH_WORK.with(Cell::get)
}

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

/// Redacted current-session agent summary exposed to the opt-in `agent_list`
/// built-in tool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InternalAgentSummary {
    /// Stable public agent id.
    pub agent_id: String,
    /// Immutable creation role.
    pub role: String,
    /// Role group containing the creation role.
    pub group: String,
    /// Current-session lifecycle state.
    pub state: InternalAgentState,
}

/// Lifecycle states exposed by the built-in `agent_list` tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InternalAgentState {
    /// Accepted creation has not dispatched yet.
    Pending,
    /// Resumable endpoint is idle.
    Idle,
    /// Resumable endpoint is running.
    Running,
    /// Cold restore retained the transcript but could not reconstruct its
    /// route.
    RestoredUnavailable,
    /// Known endpoint has unloaded.
    Stopped,
}

impl InternalAgentState {
    /// Stable tool-facing spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Idle => "idle",
            Self::Running => "running",
            Self::RestoredUnavailable => "restored_unavailable",
            Self::Stopped => "stopped",
        }
    }

    /// Parse one tool-facing spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "idle" => Some(Self::Idle),
            "running" => Some(Self::Running),
            "restored_unavailable" => Some(Self::RestoredUnavailable),
            "stopped" => Some(Self::Stopped),
            _ => None,
        }
    }
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

/// Opaque proof that one committed internal-tool start belongs to its calling
/// model agent rather than a configured extension.
pub struct AgentOwnedInternalToolCall {
    /// Calling conversation selected from harness-owned routing.
    conversation_id: AgentId,
    /// Materialized internal call.
    call: AgentToolCall,
    /// Model-visible tool name.
    visible_tool_name: ToolName,
}

/// Authoritative runtime metadata for the model agent invoking an internal
/// tool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InternalSelfInfo {
    /// Stable public identity of the calling agent.
    pub agent_id: tau_proto::AgentId,
    /// Session that owns the calling agent.
    pub session_id: SessionId,
    /// Durable session directory, absent when the session has no persistent
    /// directory.
    pub session_dir: Option<PathBuf>,
    /// Exact provider-qualified model captured for the prompt that made the
    /// call.
    pub model: ModelId,
    /// Exact reasoning effort captured for the prompt that made the call.
    pub effort: Effort,
    /// Current canonical harness-owned semantic work status.
    pub work_status: SessionAgentWorkStatus,
}

impl AgentOwnedInternalToolCall {
    /// Return the calling conversation.
    pub fn conversation_id(&self) -> &AgentId {
        &self.conversation_id
    }

    /// Return the validated agent-owned call.
    pub fn call(&self) -> &AgentToolCall {
        &self.call
    }

    /// Return the model-visible tool name.
    pub fn visible_tool_name(&self) -> &ToolName {
        &self.visible_tool_name
    }
}

impl<'a> InternalToolHost<'a> {
    pub(crate) fn new(harness: &'a mut Harness) -> Self {
        Self { harness }
    }

    /// Register a harness-process internal tool.
    pub fn register_internal_tool(&mut self, spec: ToolSpec, group: Option<tau_proto::ToolGroup>) {
        if let Some(group) = group {
            let _ = self
                .harness
                .tool_routing
                .registry
                .register_internal_with_group(crate::harness::harness_connection_id(), spec, group);
        } else {
            let _ = self
                .harness
                .tool_routing
                .registry
                .register_internal(crate::harness::harness_connection_id(), spec);
        }
    }

    /// Normalize an activating-input wait timeout against this harness's
    /// validated configuration.
    pub fn normalized_wait_timeout_minutes(
        &self,
        arguments: &CborValue,
    ) -> Result<Option<u64>, String> {
        self.harness
            .normalized_input_wait_timeout_minutes(arguments)
    }

    /// Return a cloned snapshot of skills discovered by the harness.
    pub fn discovered_skills(&self, conversation_id: &AgentId) -> Vec<InternalSkill> {
        let skills = self
            .harness
            .agent_runtime
            .agent_registry
            .agents
            .get(conversation_id)
            .and_then(|agent| agent.identity.agent_id.clone())
            .and_then(|agent_id| {
                self.harness
                    .prompt_coordination
                    .context_discovery
                    .frozen_agents
                    .get(&agent_id)
            })
            .map_or(
                &self.harness.prompt_coordination.context_discovery.skills,
                |snapshot| &snapshot.skills,
            );
        skills
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

    /// Apply a canonical work-status report to the calling agent only.
    ///
    /// Returns whether the runtime snapshot changed.
    pub fn report_work_status(
        &mut self,
        owner: &AgentOwnedInternalToolCall,
        report: crate::WorkStatusReport,
    ) -> Result<bool, String> {
        self.harness
            .report_agent_work_status(owner.conversation_id(), report)
    }

    /// Ensure and return the agent id for a conversation.
    pub fn ensure_agent_id_for_agent(&mut self, conversation_id: &AgentId) -> Option<String> {
        self.harness
            .ensure_agent_id_for_agent(conversation_id)
            .map(|agent_id| agent_id.to_string())
    }

    /// Return a redacted, deterministically ordered current-session agent
    /// snapshot.
    pub fn current_agent_summaries(&self) -> Vec<InternalAgentSummary> {
        let mut summaries = self
            .harness
            .agent_runtime
            .agent_registry
            .agents
            .values()
            .filter(|agent| !agent.dispatch.terminating)
            .filter_map(|agent| {
                let agent_id = agent.identity.agent_id.clone()?;
                let role = agent
                    .identity
                    .role
                    .clone()
                    .unwrap_or_else(|| self.harness.config.selected_role.clone());
                Some(InternalAgentSummary {
                    group: self.harness.role_group_name_for_role(&role),
                    role,
                    agent_id: agent_id.to_string(),
                    state: match agent.turn.published_runtime_state {
                        tau_proto::AgentRuntimeState::Idle => InternalAgentState::Idle,
                        tau_proto::AgentRuntimeState::Running => InternalAgentState::Running,
                    },
                })
            })
            .collect::<Vec<_>>();
        summaries.extend(self.harness.pending_agent_summary_data().into_iter().map(
            |(agent_id, role)| InternalAgentSummary {
                group: self.harness.role_group_name_for_role(&role),
                agent_id,
                role,
                state: InternalAgentState::Pending,
            },
        ));
        summaries.extend(
            self.harness
                .agent_runtime
                .agent_registry
                .restored_unavailable
                .iter()
                .map(|(agent_id, role)| InternalAgentSummary {
                    group: self.harness.role_group_name_for_role(role),
                    agent_id: agent_id.to_string(),
                    role: role.to_string(),
                    state: InternalAgentState::RestoredUnavailable,
                }),
        );
        let represented = summaries
            .iter()
            .map(|summary| summary.agent_id.clone())
            .collect::<std::collections::HashSet<_>>();
        summaries.extend(
            self.harness
                .agent_runtime
                .agent_registry
                .stopped_ids
                .iter()
                .filter(|agent_id| !represented.contains(agent_id.as_str()))
                .map(|agent_id| {
                    let role = self
                        .harness
                        .session_runtime
                        .agent_store
                        .agent_events(agent_id.as_str())
                        .ok()
                        .and_then(|events| {
                            events.into_iter().find_map(|record| match record.event {
                                tau_proto::Event::AgentStarted(started) => Some(started.role),
                                _ => None,
                            })
                        })
                        .unwrap_or_else(|| self.harness.config.selected_role.clone());
                    InternalAgentSummary {
                        group: self.harness.role_group_name_for_role(&role),
                        agent_id: agent_id.to_string(),
                        role,
                        state: InternalAgentState::Stopped,
                    }
                }),
        );
        summaries.sort_by(|left, right| left.agent_id.cmp(&right.agent_id));
        summaries
    }

    /// Derive the child endpoint for one still-outstanding built-in delegation.
    ///
    /// The durable extension originator supplies query correlation. Warm and
    /// cold completion therefore use the same source instead of a
    /// process-local map.
    pub fn agent_id_for_harness_start_query(&self, query_id: &str) -> Option<String> {
        self.harness
            .agent_runtime
            .agent_registry
            .pending_builtin_delegates
            .get(query_id)
            .map(ToString::to_string)
    }

    /// Start bounded runtime-dir peer-session discovery off the harness event
    /// loop.
    pub fn start_session_discovery(
        &mut self,
        conversation_id: &AgentId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
        query: Option<String>,
        limit: usize,
    ) {
        self.ensure_internal_tool_tracking(conversation_id, call, &visible_tool_name);
        let Some(permit) = path_crate_runtime_dir::DiscoveryCallPermit::try_acquire() else {
            self.harness.finish_harness_owned_tool_with_error(
                conversation_id,
                call.id.clone(),
                visible_tool_name,
                call.tool_type,
                "session discovery is busy; retry later".to_owned(),
                None,
            );
            return;
        };
        let tx = self.harness.runtime_io.tx.clone();
        let current_session_id = self.harness.session_runtime.current_session_id.clone();
        let command = crate::event::SessionDiscoveryCompletedCommand {
            conversation_id: conversation_id.clone(),
            session_generation: self.harness.session_runtime.current_session_generation,
            call_id: call.id.clone(),
            tool_name: visible_tool_name,
            tool_type: call.tool_type,
            result: CborValue::Null,
        };
        std::thread::spawn(move || {
            let snapshot = crate::runtime_dir::discover_peer_sessions(
                query.as_deref(),
                limit,
                current_session_id.as_str(),
                permit,
            );
            let sessions = snapshot
                .sessions
                .into_iter()
                .map(|session| {
                    let mut fields = vec![
                        (
                            CborValue::Text("session_id".to_owned()),
                            CborValue::Text(session.session_id),
                        ),
                        (
                            CborValue::Text("current".to_owned()),
                            CborValue::Bool(session.current),
                        ),
                    ];
                    if let Some(label) = session.project_label {
                        fields.push((
                            CborValue::Text("project".to_owned()),
                            CborValue::Text(label),
                        ));
                    }
                    CborValue::Map(fields)
                })
                .collect();
            let mut command = command;
            command.result = CborValue::Map(vec![
                (
                    CborValue::Text("sessions".to_owned()),
                    CborValue::Array(sessions),
                ),
                (
                    CborValue::Text("truncated".to_owned()),
                    CborValue::Bool(snapshot.truncated),
                ),
                (
                    CborValue::Text("scan_truncated".to_owned()),
                    CborValue::Bool(snapshot.scan_truncated),
                ),
            ]);
            let _ = tx.send(path_crate_event::HarnessEvent::Command(
                path_crate_event::HarnessCommand::SessionDiscoveryCompleted(Box::new(command)),
            ));
        });
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

    /// Background an internal tool call with a custom placeholder and release
    /// the foreground turn after that placeholder commits.
    pub fn background_tool_call(&mut self, call_id: &ToolCallId, result: CborValue) {
        if self
            .harness
            .tool_routing
            .tool_runtime
            .tool_turn
            .begin_backgrounding(call_id)
        {
            self.harness.observe_tool_backgrounded(call_id);
            self.harness
                .publish_internal_background_placeholder(call_id, result);
        }
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
        let cid = self
            .harness
            .tool_routing
            .tool_runtime
            .tool_agents
            .get(&started.call_id)
            .or_else(|| {
                self.harness
                    .tool_routing
                    .tool_runtime
                    .peer_internal_tool_agents
                    .get(&started.call_id)
            })?
            .clone();
        let pending = self
            .harness
            .tool_routing
            .tool_runtime
            .pending_tools
            .get(&started.call_id)?;
        let internal_name = pending.internal_name.clone();
        let tool_type = pending.tool_type;
        let visible_name = pending.name.clone();
        #[cfg(test)]
        record_internal_tool_dispatch_work(|work| work.argument_clones += 1);
        let call = AgentToolCall {
            call_ref: self.harness.wait_tool_call_ref(&started.call_id),
            id: started.call_id.clone(),
            name: internal_name,
            tool_type,
            arguments: started.arguments.clone(),
        };
        Some((cid, call, visible_name))
    }

    /// Resolve a committed internal-tool start only when its validated runtime
    /// owner is the calling model agent.
    pub fn agent_owned_internal_started_call(
        &mut self,
        started: &tau_proto::ToolStarted,
    ) -> Option<AgentOwnedInternalToolCall> {
        let cid = self
            .harness
            .tool_routing
            .tool_runtime
            .tool_agents
            .get(&started.call_id)?
            .clone();
        let pending = self
            .harness
            .tool_routing
            .tool_runtime
            .pending_tools
            .get(&started.call_id)?;
        let internal_name = pending.internal_name.clone();
        let tool_type = pending.tool_type;
        let visible_name = pending.name.clone();
        #[cfg(test)]
        record_internal_tool_dispatch_work(|work| work.argument_clones += 1);
        let call = AgentToolCall {
            call_ref: self.harness.wait_tool_call_ref(&started.call_id),
            id: started.call_id.clone(),
            name: internal_name,
            tool_type,
            arguments: started.arguments.clone(),
        };
        Some(AgentOwnedInternalToolCall {
            conversation_id: cid,
            call,
            visible_tool_name: visible_name,
        })
    }

    /// Return authoritative self metadata for an agent-owned internal tool
    /// call.
    ///
    /// Returns `None` unless the owner still correlates to its prompt-start
    /// fact and that fact has captured model parameters. Identity, session,
    /// model, and effort come from this frozen prompt ownership; work
    /// status is read at call time from the current loaded-agent reducer.
    pub(crate) fn self_info(&self, owner: &AgentOwnedInternalToolCall) -> Option<InternalSelfInfo> {
        let agent = self
            .harness
            .agent_runtime
            .agent_registry
            .agents
            .get(owner.conversation_id())?;
        let agent_id = agent.identity.agent_id.clone()?;
        let prompt_id = self
            .harness
            .prompt_coordination
            .prompt_runtime
            .tool_call_prompt(&owner.call().id)?;
        let started = self
            .harness
            .session_runtime
            .agent_store
            .agent(agent_id.as_str())?
            .prompt_started(prompt_id)?;
        let model_params = started.model_params?;
        let session_dir = self
            .harness
            .session_runtime
            .storage_mode
            .is_durable()
            .then(|| {
                self.harness
                    .session_runtime
                    .store
                    .sessions_dir()
                    .join(agent.identity.session_id.as_str())
            });
        Some(InternalSelfInfo {
            agent_id,
            session_id: agent.identity.session_id.clone(),
            session_dir,
            model: started.model.clone(),
            effort: model_params.effort,
            work_status: SessionAgentWorkStatus::new(
                agent.turn.work_status.phase(),
                agent.turn.work_status.title().map(ToOwned::to_owned),
            )
            .expect("harness work status is canonical"),
        })
    }

    /// Dispatch a hidden activating background-completion prompt synchronously.
    #[cfg(test)]
    pub(crate) fn dispatch_test_background_completion(
        &mut self,
        agent_id: &str,
        text: String,
    ) -> Result<(), HarnessError> {
        let cid = self
            .harness
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(agent_id)
            .cloned()
            .ok_or_else(|| HarnessError::Participant(format!("unknown test agent `{agent_id}`")))?;
        self.harness.dispatch_prompt_for_agent(
            &cid,
            path_crate_agent::PendingPrompt::activating_background_completion(text),
        )
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
        cancel_call: Option<tau_proto::ToolCallRef>,
        target_call_id: ToolCallId,
    ) -> Result<(), String> {
        self.harness
            .publish_tool_cancel_request_for(conversation_id, cancel_call, target_call_id)
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

    /// Publish an agent-to-agent message from a conversation.
    pub fn publish_agent_message(
        &mut self,
        conversation_id: &AgentId,
        recipient_id: String,
        message: String,
    ) -> Result<tau_proto::AgentMessageId, String> {
        self.harness
            .publish_agent_message_from_agent(conversation_id, recipient_id, message)
    }

    /// Return the harness's active session id.
    pub fn current_session_id(&self) -> tau_proto::SessionId {
        self.harness.session_runtime.current_session_id.clone()
    }

    /// Resolve and publish one bare message through this session's entrypoint.
    pub fn publish_local_peer_message(
        &mut self,
        conversation_id: &AgentId,
        message: String,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: tau_proto::ToolType,
    ) -> Result<(), String> {
        self.harness.publish_peer_entrypoint_message_from_agent(
            conversation_id,
            message,
            call_id,
            tool_name,
            tool_type,
        )
    }

    /// Publish an external message and arrange asynchronous tool completion.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_external_agent_message(
        &mut self,
        conversation_id: &AgentId,
        recipient_session_id: tau_proto::SessionId,
        recipient: tau_proto::ExternalAgentMessageRecipient,
        message: String,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: tau_proto::ToolType,
        details: CborValue,
    ) -> Result<(), String> {
        self.harness.publish_external_agent_message_from_agent(
            conversation_id,
            recipient_session_id,
            recipient,
            message,
            tau_proto::AgentMessageKind::Message,
            Some(crate::harness::ExternalMessageToolCompletion {
                conversation_id: conversation_id.clone(),
                session_generation: self.harness.session_runtime.current_session_generation,
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
    ) -> Result<tau_proto::AgentMessageId, String> {
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

    /// Atomically validate self-watch, target lifecycle, and acyclic topology,
    /// then mutate a watch relation.
    ///
    /// The topology mutation and endpoint lifecycle rules are specified by
    /// `SPEC-agent-watch`.
    ///
    /// Self-watch fails first. Enabling then requires a live target and rejects
    /// only a genuinely new edge that would close a directed cycle, before any
    /// watch mutation or event publication. Re-enabling retains established
    /// snapshot behavior. Disabling bypasses lifecycle and cycle analysis and
    /// is idempotent, including for stopped or unknown target ids. See
    /// `GATE-agent-watch-acyclic-topology`.
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

    /// Prune a stale watch relationship after notification delivery failed.
    pub fn prune_agent_watch(&mut self, watcher_id: &str, watched_agent_id: &str) {
        self.harness.prune_agent_watch(watcher_id, watched_agent_id);
    }

    fn sender_conversation_id(&self, sender_agent_id: &str) -> Result<AgentId, String> {
        self.harness
            .agent_runtime
            .agent_registry
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
    /// Install the reserved intrinsic `self_info` handler before all supplied
    /// handlers, then register every internal tool spec in that order.
    ///
    /// `self_info` is enabled by default but remains subject to ordinary
    /// effective role/tool policy. A supplied handler that claims the reserved
    /// name is excluded completely, so it cannot register other specs or
    /// observe committed events.
    pub fn install_internal_tool_handlers(&mut self, mut handlers: InternalToolHandlers) {
        let reserved_name = ToolName::new(crate::self_info_tool::SELF_INFO_TOOL_NAME);
        handlers.retain(|handler| !handler.handles(&reserved_name));
        handlers.insert(0, crate::self_info_tool::handler());
        self.tool_routing.internal_tool_handlers = handlers;
        let handlers = self.tool_routing.internal_tool_handlers.clone();
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
        let handlers = match event {
            Event::ToolStarted(started) => {
                let Some(internal_name) = self
                    .tool_routing
                    .tool_runtime
                    .pending_tools
                    .get(&started.call_id)
                    .map(|pending| &pending.internal_name)
                else {
                    return Ok(());
                };
                self.tool_routing
                    .internal_tool_handlers
                    .iter()
                    .filter(|handler| {
                        #[cfg(test)]
                        record_internal_tool_dispatch_work(|work| {
                            work.ownership_predicate_visits += 1;
                        });
                        handler.handles(internal_name)
                    })
                    .cloned()
                    .collect()
            }
            _ => self.tool_routing.internal_tool_handlers.clone(),
        };
        for handler in handlers {
            #[cfg(test)]
            record_internal_tool_dispatch_work(|work| work.handler_invocations += 1);
            let mut host = InternalToolHost::new(self);
            handler.handle_event(&mut host, event)?;
        }
        Ok(())
    }
}
