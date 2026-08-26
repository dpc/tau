//! UI agent-creation admission and initial-prompt lifecycle.

use tau_proto::{AgentId, AgentPromptQueued, Event, HarnessOutputMessage};

use super::{Harness, user_skill_invocation};
use crate::agent::{InitialPromptCorrelation, PendingPrompt};
use crate::error::HarnessError;

const CREATE_AGENT_DIAGNOSTIC_MAX_CHARS: usize = 512;

/// Admission data resolved before a create request mutates agent state.
struct ValidatedCreateAdmission {
    /// Loaded runtime parent, when the request names one.
    parent_cid: Option<AgentId>,
}

/// Initial-prompt data retained after the new agent commits.
struct CreatedInitialPrompt {
    /// Create request correlation returned to the requester.
    request_id: String,
    /// Session that owns the created agent.
    session_id: tau_proto::SessionId,
    /// Durable identity returned by successful creation.
    agent_id: tau_proto::AgentId,
    /// Exact initial-prompt correlation.
    ctx_id: String,
    /// Initial prompt text.
    text: String,
    /// User/internal classification for the prompt.
    message_class: tau_proto::PromptMessageClass,
    /// Authenticated originator projection for the prompt.
    originator: tau_proto::PromptOriginator,
    /// Whether skill expansion waits until dispatch.
    defer_skill_expansion: bool,
}

impl Harness {
    /// Validate and execute one attached-UI create-agent request.
    pub(crate) fn handle_ui_create_agent_from(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        req: tau_proto::UiCreateAgent,
    ) -> Result<bool, HarnessError> {
        let request_id = req.request_id.clone();
        let session_id = req.session_id.clone();
        let Ok(ValidatedCreateAdmission { parent_cid }) =
            self.validate_ui_create_agent_admission(client_id, &req)
        else {
            return Ok(true);
        };
        let is_user_initial_prompt = req.initial_prompt.is_some()
            && req.originator.is_user()
            && !req.message_class.is_internal();
        let defer_initial_skill_expansion = is_user_initial_prompt
            && !req.literal
            && req.initial_prompt.as_deref().is_some_and(|text| {
                user_skill_invocation::parse_user_skill_command(text).is_some()
            });
        let initial_prompt = req.initial_prompt;
        let prompt_ctx_id = req.ctx_id.clone();
        let parent_ephemeral = parent_cid
            .as_ref()
            .and_then(|cid| self.agent_registry.agents.get(cid))
            .is_some_and(|agent| agent.persistence.is_ephemeral());
        let persistence = if req.ephemeral || parent_ephemeral {
            tau_core::AgentPersistenceMode::Ephemeral
        } else {
            tau_core::AgentPersistenceMode::Durable
        };
        let cid = match self.try_create_user_agent_with_parent(
            req.session_id.clone(),
            &req.role,
            parent_cid,
            req.metadata,
            persistence,
        ) {
            Ok(cid) => cid,
            Err(error) => {
                self.emit_harness_failure(&format!("failed to create UI agent: {error}"));
                self.send_ui_create_agent_rejection(
                    client_id,
                    request_id,
                    session_id,
                    tau_proto::UiCreateAgentRejection::CreationFailed,
                    "failed to create agent".to_owned(),
                    None,
                );
                return Ok(true);
            }
        };
        let created_agent_id = self
            .target_agent_id_for_agent(&cid)
            .map(crate::parse_agent_id)
            .expect("new UI agent has a durable id");
        if is_user_initial_prompt
            && let Some(agent_id) = self.target_agent_id_for_agent(&cid)
            && let Err(error) = self.record_accepted_visible_user_interaction(&agent_id)
        {
            self.emit_harness_failure(&format!(
                "failed to record visible interaction for created UI agent: {error}"
            ));
            self.send_ui_create_agent_rejection(
                client_id,
                request_id,
                session_id,
                tau_proto::UiCreateAgentRejection::InitialPromptFailed,
                "failed to admit initial prompt".to_owned(),
                Some(created_agent_id),
            );
            return Ok(true);
        }
        if let Some(conv) = self.agent_registry.agents.get_mut(&cid) {
            conv.next_ctx_id = prompt_ctx_id.clone();
            conv.model_override = req.model_override;
        }
        if let Some(text) = initial_prompt {
            self.admit_created_initial_prompt(
                client_id,
                &cid,
                CreatedInitialPrompt {
                    request_id,
                    session_id,
                    agent_id: created_agent_id,
                    ctx_id: prompt_ctx_id.expect("validated initial prompt correlation id"),
                    text,
                    message_class: req.message_class,
                    originator: req.originator,
                    defer_skill_expansion: defer_initial_skill_expansion,
                },
            );
        } else {
            self.send_ui_create_agent_created(
                client_id,
                request_id,
                session_id,
                created_agent_id,
                tau_proto::UiCreateAgentInitialPrompt::Absent,
            );
        }
        Ok(true)
    }

    /// Admit and dispatch the initial prompt after its agent commits.
    fn admit_created_initial_prompt(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        cid: &AgentId,
        admission: CreatedInitialPrompt,
    ) {
        let CreatedInitialPrompt {
            request_id,
            session_id,
            agent_id,
            ctx_id,
            text,
            message_class,
            originator,
            defer_skill_expansion,
        } = admission;
        if !message_class.is_internal() {
            self.preempt_blocking_ext_side_agents(&session_id);
        }
        let mut prompt = if message_class.is_internal() {
            PendingPrompt::untrusted_internal(text)
        } else if originator.is_user() {
            PendingPrompt::human_ui(text)
        } else {
            PendingPrompt::user(text)
        }
        .with_ctx_id(Some(ctx_id.clone()));
        prompt.expand_user_skill_on_dispatch = defer_skill_expansion;
        prompt.initial_prompt_correlation = Some(InitialPromptCorrelation {
            request_id: request_id.clone(),
            agent_id: agent_id.clone(),
            ctx_id: ctx_id.clone(),
            activation_through: None,
        });
        self.ensure_prompt_activation_observed(cid, &mut prompt);
        self.send_ui_create_agent_created(
            client_id,
            request_id.clone(),
            session_id.clone(),
            agent_id.clone(),
            tau_proto::UiCreateAgentInitialPrompt::Queued,
        );
        if self.dispatch_blocked_for(cid) || !self.session_initialized(&session_id) {
            if let Some(conv) = self.agent_registry.agents.get_mut(cid) {
                conv.pending_prompts.push_back(prompt.clone());
            }
            self.publish_event(
                None,
                Event::AgentPromptQueued(AgentPromptQueued {
                    agent_id: agent_id.clone(),
                    text: prompt.text,
                    message_class: prompt.message_class,
                }),
            );
            self.try_advance_queue();
            return;
        }
        let prompt = match self.resolve_pending_user_skill_for_agent(cid, prompt) {
            Ok(prompt) => prompt,
            Err(message) => {
                self.publish_initial_prompt_failed(
                    InitialPromptCorrelation {
                        request_id,
                        agent_id,
                        ctx_id,
                        activation_through: None,
                    },
                    tau_proto::AgentPromptFailureStage::Preprocessing,
                    &bound_create_agent_diagnostic(message),
                );
                return;
            }
        };
        if let Err(error) = self.dispatch_prompt_for_agent(cid, prompt) {
            self.emit_harness_failure(&format!(
                "failed to dispatch created UI agent's initial prompt: {error}"
            ));
            self.publish_initial_prompt_failed(
                InitialPromptCorrelation {
                    request_id,
                    agent_id,
                    ctx_id,
                    activation_through: None,
                },
                tau_proto::AgentPromptFailureStage::Submission,
                "failed to submit initial prompt",
            );
        }
    }

    /// Validate immutable pre-creation fields and resolve the optional parent.
    fn validate_ui_create_agent_admission(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        req: &tau_proto::UiCreateAgent,
    ) -> Result<ValidatedCreateAdmission, ()> {
        let reject =
            |harness: &mut Self, reason: tau_proto::UiCreateAgentRejection, message: String| {
                harness.send_ui_create_agent_rejection(
                    client_id,
                    req.request_id.clone(),
                    req.session_id.clone(),
                    reason,
                    message,
                    None,
                );
                Err(())
            };
        if req.request_id.is_empty()
            || tau_proto::MAX_UI_CREATE_AGENT_REQUEST_ID_BYTES < req.request_id.len()
        {
            return reject(
                self,
                tau_proto::UiCreateAgentRejection::InvalidRequestId,
                "create-agent request id must contain 1 through 128 bytes".to_owned(),
            );
        }
        if req.initial_prompt.is_some()
            && !req.ctx_id.as_ref().is_some_and(|ctx_id| {
                !ctx_id.is_empty()
                    && ctx_id.len() <= tau_proto::MAX_UI_CREATE_AGENT_PROMPT_CTX_ID_BYTES
            })
        {
            return reject(
                self,
                tau_proto::UiCreateAgentRejection::InvalidRequestId,
                "initial prompt correlation id must contain 1 through 128 bytes".to_owned(),
            );
        }
        if req.session_id != self.current_session_id {
            let message = format!(
                "harness is bound to session `{}`; create-agent for `{}` rejected",
                self.current_session_id.as_str(),
                req.session_id.as_str()
            );
            return reject(
                self,
                tau_proto::UiCreateAgentRejection::StaleSession,
                message,
            );
        }
        if !self.available_roles.contains_key(&req.role) {
            let message = self
                .disabled_role_reasons
                .get(&req.role)
                .map(|reason| reason.message.clone())
                .unwrap_or_else(|| format!("unknown role `{}`", req.role));
            return reject(
                self,
                tau_proto::UiCreateAgentRejection::RoleUnavailable,
                message,
            );
        }
        if let Err(error) = self.validate_initial_agent_metadata(&req.metadata) {
            let message = format!("create-agent metadata rejected: {error}");
            return reject(
                self,
                tau_proto::UiCreateAgentRejection::InvalidMetadata,
                message,
            );
        }
        let parent_cid = if let Some(agent_id) = req.parent_agent.as_ref() {
            let Some(cid) = self
                .agent_registry
                .agent_routes
                .get(agent_id.as_str())
                .cloned()
            else {
                let message =
                    format!("parent_agent `{agent_id}` is not loaded in the current session");
                return reject(
                    self,
                    tau_proto::UiCreateAgentRejection::ParentNotLoaded,
                    message,
                );
            };
            Some(cid)
        } else {
            None
        };
        Ok(ValidatedCreateAdmission { parent_cid })
    }

    fn send_ui_create_agent_created(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        request_id: String,
        session_id: tau_proto::SessionId,
        agent_id: tau_proto::AgentId,
        initial_prompt: tau_proto::UiCreateAgentInitialPrompt,
    ) {
        let _ = self.bus.send_to(
            client_id,
            None,
            HarnessOutputMessage::deliver(Event::UiCreateAgentResult(
                tau_proto::UiCreateAgentResult {
                    request_id,
                    session_id,
                    outcome: tau_proto::UiCreateAgentOutcome::Created {
                        agent_id,
                        initial_prompt,
                    },
                },
            )),
        );
    }

    /// Publish one immutable pre-materialization prompt terminal.
    pub(super) fn publish_initial_prompt_failed(
        &mut self,
        correlation: InitialPromptCorrelation,
        stage: tau_proto::AgentPromptFailureStage,
        message: &str,
    ) {
        if let Some(cid) = self
            .agent_registry
            .agent_routes
            .get(correlation.agent_id.as_str())
            .cloned()
            && let Some(agent) = self.agent_registry.agents.get_mut(&cid)
            && agent.next_ctx_id.as_deref() == Some(correlation.ctx_id.as_str())
        {
            agent.next_ctx_id = None;
        }
        self.publish_event(
            None,
            Event::AgentPromptFailed(tau_proto::AgentPromptFailed {
                request_id: correlation.request_id,
                agent_id: correlation.agent_id,
                ctx_id: correlation.ctx_id,
                stage,
                message: bound_create_agent_diagnostic(message.to_owned()),
            }),
        );
    }

    /// Terminate every accepted initial prompt still owned by one agent.
    pub(super) fn fail_pending_initial_prompts(
        &mut self,
        cid: &AgentId,
        stage: tau_proto::AgentPromptFailureStage,
        message: &str,
    ) {
        let mut correlations = self
            .agent_registry
            .agents
            .get_mut(cid)
            .map(|agent| {
                agent
                    .pending_prompts
                    .iter_mut()
                    .filter_map(|prompt| prompt.initial_prompt_correlation.take())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        correlations.extend(self.prompt_runtime.pending_initial_correlations.remove(cid));
        for correlation in correlations {
            self.publish_initial_prompt_failed(correlation, stage, message);
        }
    }

    /// Terminate one submitted initial prompt before provider materialization.
    pub(super) fn fail_initial_prompt_materialization(&mut self, cid: &AgentId, message: &str) {
        if let Some(correlation) = self.prompt_runtime.pending_initial_correlations.remove(cid) {
            self.retire_deferred_activation(cid, correlation.activation_through);
            self.publish_initial_prompt_failed(
                correlation,
                tau_proto::AgentPromptFailureStage::Submission,
                message,
            );
        }
    }

    /// Terminate an initial prompt whose render preflight failed before its
    /// activating submission entered durable history.
    pub(super) fn fail_initial_prompt_preflight(&mut self, correlation: InitialPromptCorrelation) {
        self.publish_initial_prompt_failed(
            correlation,
            tau_proto::AgentPromptFailureStage::Submission,
            "failed to validate initial prompt before submission",
        );
    }

    fn send_ui_create_agent_rejection(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        request_id: String,
        session_id: tau_proto::SessionId,
        reason: tau_proto::UiCreateAgentRejection,
        message: String,
        agent_id: Option<tau_proto::AgentId>,
    ) {
        let _ = self.bus.send_to(
            client_id,
            None,
            HarnessOutputMessage::deliver(Event::UiCreateAgentResult(
                tau_proto::UiCreateAgentResult {
                    request_id,
                    session_id,
                    outcome: tau_proto::UiCreateAgentOutcome::Rejected {
                        reason,
                        message: bound_create_agent_diagnostic(message),
                        agent_id,
                    },
                },
            )),
        );
    }
}

/// Bound a create-admission diagnostic before exposing it across the UI wire.
pub(super) fn bound_create_agent_diagnostic(message: String) -> String {
    let mut chars = message.chars();
    let mut bounded = chars
        .by_ref()
        .take(CREATE_AGENT_DIAGNOSTIC_MAX_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}
