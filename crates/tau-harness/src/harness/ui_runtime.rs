//! Owns attached-client transport lifecycles plus human-UI command admission,
//! routing, and replies.
//!
//! Human UI authority remains distinct from configured extensions and peers.

use super::start_coordinator::StartPhase;
use super::*;

/// Runtime-only state for attached clients and human-UI command routes.
///
/// Lifetimes intentionally differ within this owner: client writers follow
/// connection lifetime; pending commands follow their session or provider;
/// retry and action tombstones plus private shell-route identities survive for
/// the process lifetime. Final shutdown resolves or invalidates session-bound
/// work field by field instead of resetting the whole state. Normal connection
/// teardown removes its writer lifecycle, while the startup-failure path may
/// explicitly close a socket writer; the lifecycle owns no join handle or drop
/// side effect. This state does not own the generic event bus.
pub(crate) struct UiRuntimeState {
    /// Harness-authorized bootstrap request ids awaiting ordinary UI admission.
    pub(super) pending_bootstrap_creates: HashMap<String, String>,
    /// Cause carried by the command that stopped the central event loop.
    pub(crate) shutdown_cause: Option<crate::event::ShutdownCause>,
    /// Random stream for opaque provider-side shell route identities.
    ///
    /// This stream stays independent from agent-id generation so shell traffic
    /// cannot perturb later agent identities.
    pub(super) ui_shell_route_rng: StdRng,
    /// Provider routes and canonical requests awaiting shell terminals.
    pub(super) pending_ui_shell_commands: HashMap<UiShellRouteId, PendingUiShellCommand>,
    /// Process-lifetime shell routes whose target agent was ephemeral.
    ///
    /// Retention excludes late or interception-replaced reports from durable
    /// debug output, and non-reuse keeps a later command from inheriting that
    /// classification.
    pub(super) ephemeral_ui_shell_route_ids: HashSet<UiShellRouteId>,
    /// Public shell ids whose next canonical fact targets an ephemeral agent.
    pub(super) pending_ephemeral_ui_shell_canonical_events:
        HashMap<tau_proto::ShellCommandId, NonZeroUsize>,
    /// Public shell ids reserved from admission through terminal commit.
    pub(super) active_ui_shell_command_ids: HashSet<tau_proto::ShellCommandId>,
    /// Canonical shell completions that inject output after commit.
    pub(super) pending_ui_shell_output_injections: HashSet<tau_proto::ShellCommandId>,
    /// Action invocations awaiting their exact provider and requester route.
    pub(super) pending_action_invocations: HashMap<ActionInvocationId, PendingActionInvocation>,
    /// Terminal action invocation ids that can never be routed again.
    pub(super) completed_action_invocations: HashSet<ActionInvocationId>,
    /// Manual retry requests awaiting their exact provider owner.
    pub(super) pending_retry_prompts: HashMap<tau_proto::RetryPromptRequestId, PendingRetryPrompt>,
    /// Process-lifetime replay guard for UI-chosen retry correlations.
    pub(super) seen_retry_prompt_requests:
        HashSet<(tau_proto::ConnectionId, tau_proto::RetryPromptRequestId)>,
    /// FIFO order that bounds the retry replay guard.
    pub(super) seen_retry_prompt_request_order:
        VecDeque<(tau_proto::ConnectionId, tau_proto::RetryPromptRequestId)>,
    /// Live-log cursors and transport lifecycles for attached clients.
    pub(crate) client_writers: HashMap<tau_proto::ConnectionId, ClientWriterLifecycle>,
    /// Semantically quarantined exact-discovery probes.
    pub(super) runtime_probe_peers: HashSet<tau_proto::ConnectionId>,
    /// Socket connections that have not completed Hello admission.
    pub(crate) pending_socket_admission: HashSet<tau_proto::ConnectionId>,
    /// Authorized UI request for unconditional canonical harness shutdown.
    pub(super) shutdown_requested: bool,
    /// Initial-UI launches shut down after their last participating UI leaves.
    /// Explicit detach clears this for the daemon lifetime; it is never
    /// persisted.
    pub(crate) exit_on_disconnect: bool,
    /// Prevent an auto-shutdown before the first authenticated UI attaches.
    pub(super) ever_attached: bool,
    /// UIs that received a quit disposition no longer keep the daemon alive,
    /// even while their reply writer is draining.
    pub(super) quitting_uis: HashSet<tau_proto::ConnectionId>,
}

impl Default for UiRuntimeState {
    fn default() -> Self {
        Self {
            pending_bootstrap_creates: HashMap::new(),
            shutdown_cause: None,
            ui_shell_route_rng: StdRng::from_entropy(),
            pending_ui_shell_commands: HashMap::new(),
            ephemeral_ui_shell_route_ids: HashSet::new(),
            pending_ephemeral_ui_shell_canonical_events: HashMap::new(),
            active_ui_shell_command_ids: HashSet::new(),
            pending_ui_shell_output_injections: HashSet::new(),
            pending_action_invocations: HashMap::new(),
            completed_action_invocations: HashSet::new(),
            pending_retry_prompts: HashMap::new(),
            seen_retry_prompt_requests: HashSet::new(),
            seen_retry_prompt_request_order: VecDeque::new(),
            client_writers: HashMap::new(),
            runtime_probe_peers: HashSet::new(),
            pending_socket_admission: HashSet::new(),
            shutdown_requested: false,
            exit_on_disconnect: false,
            ever_attached: false,
            quitting_uis: HashSet::new(),
        }
    }
}

pub(super) fn shell_route_id(first: u64, second: u64) -> tau_proto::ShellCommandId {
    tau_proto::ShellCommandId::parse(format!("harness-shell-{first:016x}{second:016x}"))
        .expect("Tau-generated shell command id must be valid")
}

#[derive(Clone, Debug)]
pub(super) struct PendingActionInvocation {
    pub(super) owner_name: tau_proto::ExtensionName,
    pub(super) owner_instance_id: tau_proto::ExtensionInstanceId,
    pub(super) provider_connection_id: tau_proto::ConnectionId,
    pub(super) requester_client_id: tau_proto::ConnectionId,
    pub(super) session_id: tau_proto::SessionId,
    pub(super) action_id: String,
}

#[derive(Clone, Debug)]
pub(super) struct PendingRetryPrompt {
    /// UI correlation retained only for the requester-directed result.
    pub(super) ui_request_id: tau_proto::RetryPromptRequestId,
    pub(super) provider_connection_id: tau_proto::ConnectionId,
    pub(super) requester_client_id: tau_proto::ConnectionId,
    pub(super) agent_prompt_id: AgentPromptId,
    pub(super) target_agent_id: AgentId,
    pub(super) target_label: String,
}

/// Mint a process-unique provider-stage token that cannot collide with a UI's
/// replayable correlation namespace.
pub(super) fn next_provider_retry_token() -> tau_proto::RetryPromptRequestId {
    static NEXT: path_std_sync::atomic::AtomicU64 = path_std_sync_atomic::AtomicU64::new(0);
    let value = NEXT.fetch_add(1, path_std_sync_atomic::Ordering::Relaxed);
    tau_proto::RetryPromptRequestId::parse(format!("harness-retry-{value}"))
        .expect("harness retry token is valid")
}

/// Harness-owned identity and selected provider for an in-flight UI shell
/// command.
#[derive(Clone)]
pub(super) struct PendingUiShellCommand {
    /// Extension connection selected for point-to-point execution.
    pub(super) provider_id: tau_proto::ConnectionId,
    /// Canonical request after the harness resolves its target agent.
    pub(super) command: tau_proto::UiShellCommand,
    /// Whether this route's target requires ephemeral debug suppression.
    pub(super) targets_ephemeral: bool,
}

/// Harness-private provider execution identity for one UI shell route.
///
/// The wire protocol reuses `ShellCommandId`, so conversion is confined to the
/// provider boundary while harness state keeps UI and execution identities
/// distinct.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct UiShellRouteId(tau_proto::ShellCommandId);

impl UiShellRouteId {
    /// Wrap an opaque provider-side protocol id.
    pub(super) fn new(value: tau_proto::ShellCommandId) -> Self {
        Self(value)
    }

    /// Borrow the protocol id sent to and echoed by the selected provider.
    pub(super) fn as_protocol_id(&self) -> &tau_proto::ShellCommandId {
        &self.0
    }
}

impl Borrow<tau_proto::ShellCommandId> for UiShellRouteId {
    fn borrow(&self) -> &tau_proto::ShellCommandId {
        &self.0
    }
}

pub(super) struct CancelTarget {
    pub(super) call_id: ToolCallId,
    pub(super) tool_name: ToolName,
    pub(super) tool_type: ToolType,
    // True when the foreground tool round was already closed by a background
    // placeholder. Such calls receive a cancel request but must never receive
    // a transcript-terminal `ToolCancelled`; cancellation is reported through
    // `ToolBackgroundError` instead.
    pub(super) backgrounded: bool,
}

pub(super) fn ui_shell_provider_ids(
    registry: &tau_core::ToolRegistry,
) -> std::collections::HashSet<tau_proto::ConnectionId> {
    registry
        .all_tool_providers()
        .into_iter()
        .filter(|provider| {
            provider.kind == tau_core::ToolProviderKind::Extension
                && provider
                    .tool
                    .tags
                    .iter()
                    .any(|tag| tag.as_str() == "shell:exec:generic")
        })
        .map(|provider| provider.connection_id.clone())
        .collect()
}

impl Harness {
    pub(super) fn enqueue_attached_socket_ui_publish(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        event: Event,
        persist_override: Option<bool>,
    ) {
        if self.is_attached_socket_ui(client_id) {
            let persist = persist_override.unwrap_or_else(|| event.defaults_to_persist());
            self.enqueue_publish(Some(client_id), event, persist, false, None);
        }
    }

    pub(super) fn handle_ui_shell_command(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        mut command: tau_proto::UiShellCommand,
    ) {
        if self
            .ui_runtime
            .active_ui_shell_command_ids
            .contains(&command.command_id)
        {
            self.emit_info("discarding duplicate in-flight shell command id");
            return;
        }
        self.ui_runtime
            .active_ui_shell_command_ids
            .insert(command.command_id.clone());
        if command.session_id != self.session_runtime.current_session_id {
            self.project_ui_shell_command_start(client_id, &command);
            self.finish_unroutable_ui_shell(
                command,
                "the shell command targets a stale session",
                false,
            );
            return;
        }
        let providers = ui_shell_provider_ids(&self.tool_routing.registry);
        if providers.len() != 1 {
            let reason = if providers.is_empty() {
                "no shell extension instance is available"
            } else {
                "multiple shell extension instances are available; select one explicitly before using ! or !!"
            };
            self.project_ui_shell_command_start(client_id, &command);
            self.finish_unroutable_ui_shell(command, reason, false);
            return;
        }
        let target_agent_id = if let Some(agent_id) = command.target_agent_id.as_ref() {
            self.agent_runtime
                .agent_registry
                .agent_routes
                .get(agent_id.as_str())
                .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
                .filter(|conversation| {
                    conversation.identity.session_id == command.session_id
                        && !conversation.dispatch.terminating
                })
                .and_then(|conversation| conversation.identity.agent_id.clone())
        } else {
            self.default_shell_output_target_agent()
                .map(|(_, agent_id)| agent_id)
        };
        let Some(target_agent_id) = target_agent_id else {
            self.project_ui_shell_command_start(client_id, &command);
            self.finish_unroutable_ui_shell(
                command,
                "no unambiguous target agent is available",
                false,
            );
            return;
        };
        command.target_agent_id = Some(target_agent_id);
        let provider = providers.into_iter().next().expect("one provider");
        let route_id = self.next_ui_shell_route_id();
        let targets_ephemeral = command
            .target_agent_id
            .as_ref()
            .is_some_and(|agent_id| self.agent_is_ephemeral(agent_id));
        let mut provider_command = command.clone();
        provider_command.command_id = route_id.as_protocol_id().clone();
        self.project_ui_shell_command_start(client_id, &command);
        let delivered = self.runtime_io.bus.send_to(
            &provider,
            Some(client_id),
            HarnessOutputMessage::deliver(Event::UiShellCommand(provider_command)),
        );
        if delivered.is_ok_and(|report| !report.delivered_to.is_empty()) {
            if targets_ephemeral {
                self.ui_runtime
                    .ephemeral_ui_shell_route_ids
                    .insert(route_id.clone());
            }
            self.ui_runtime.pending_ui_shell_commands.insert(
                route_id,
                PendingUiShellCommand {
                    provider_id: provider,
                    command,
                    targets_ephemeral,
                },
            );
            return;
        }
        self.finish_unroutable_ui_shell(
            command,
            "the selected shell extension instance became unavailable",
            targets_ephemeral,
        );
    }

    /// Project the canonical public command identity to every attached UI.
    pub(super) fn project_ui_shell_command_start(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        command: &tau_proto::UiShellCommand,
    ) {
        for ui in self
            .runtime_io
            .bus
            .connections()
            .into_iter()
            .filter(|connection| connection.kind == ClientKind::Ui)
        {
            let _ = self.runtime_io.bus.send_to(
                &ui.id,
                Some(client_id),
                HarnessOutputMessage::deliver(Event::UiShellCommand(command.clone())),
            );
        }
    }

    pub(super) fn fail_pending_ui_shell_commands_for_provider(
        &mut self,
        provider_id: &tau_proto::ConnectionId,
        reason: &str,
    ) {
        let failed = self
            .ui_runtime
            .pending_ui_shell_commands
            .iter()
            .filter(|(_, pending)| &pending.provider_id == provider_id)
            .map(|(command_id, _)| command_id.clone())
            .collect::<Vec<_>>();
        for command_id in failed {
            if let Some(pending) = self
                .ui_runtime
                .pending_ui_shell_commands
                .remove(&command_id)
            {
                self.finish_unroutable_ui_shell(pending.command, reason, pending.targets_ephemeral);
            }
        }
    }

    pub(super) fn fail_all_pending_ui_shell_commands(&mut self, reason: &str) {
        let pending = std::mem::take(&mut self.ui_runtime.pending_ui_shell_commands);
        for (_, pending) in pending {
            self.finish_unroutable_ui_shell(pending.command, reason, pending.targets_ephemeral);
        }
    }

    pub(super) fn finish_unroutable_ui_shell(
        &mut self,
        command: tau_proto::UiShellCommand,
        reason: &str,
        targets_ephemeral: bool,
    ) {
        if targets_ephemeral
            || command
                .target_agent_id
                .as_ref()
                .is_some_and(|agent_id| self.agent_is_ephemeral(agent_id))
        {
            self.mark_pending_ephemeral_shell_canonical(command.command_id.clone());
        }
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::ShellCommandFinished(tau_proto::ShellCommandFinished {
                command_id: command.command_id,
                session_id: command.session_id,
                command: command.command,
                include_in_context: command.include_in_context,
                target_agent_id: command.target_agent_id,
                output: reason.to_owned(),
                exit_code: None,
                cancelled: false,
            }),
        );
    }

    pub(super) fn handle_ui_debug_event_stats_request(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        request: tau_proto::UiDebugEventStatsRequest,
    ) {
        if !self.is_attached_socket_ui(client_id) {
            self.send_direct_harness_notice(
                client_id,
                tau_proto::notice_kind::UI_COMMAND_ERROR,
                tau_proto::NoticeLevel::Info,
                tau_proto::NoticePurpose::Response,
                "extension event stats are only available to attached local UIs".to_owned(),
            );
            return;
        }
        let live_matches = self
            .extensions
            .entries
            .values()
            .filter(|entry| {
                entry.name == request.extension_name.as_str()
                    && entry.state != ExtensionState::Disconnected
            })
            .collect::<Vec<_>>();
        let (kind, message) = match live_matches.as_slice() {
            [] => (
                tau_proto::notice_kind::UI_COMMAND_ERROR,
                format!("no live extension named `{}`", request.extension_name),
            ),
            [entry] => {
                let stats = entry.protocol_io.cumulative_stats();
                (
                    tau_proto::notice_kind::HARNESS_NOTICE,
                    tau_client::format_protocol_io_cumulative_stats(
                        &format!(
                            "Extension `{}` protocol I/O cumulative stats",
                            request.extension_name
                        ),
                        "extension -> harness",
                        "harness -> extension",
                        "no extension frames recorded yet",
                        &stats,
                    ),
                )
            }
            _ => (
                tau_proto::notice_kind::UI_COMMAND_ERROR,
                format!(
                    "extension name `{}` matched {} live connections",
                    request.extension_name,
                    live_matches.len()
                ),
            ),
        };
        self.send_direct_harness_notice(
            client_id,
            kind,
            tau_proto::NoticeLevel::Info,
            tau_proto::NoticePurpose::Response,
            message,
        );
    }

    pub(super) fn send_direct_harness_notice(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        kind: &str,
        level: tau_proto::NoticeLevel,
        purpose: tau_proto::NoticePurpose,
        message: String,
    ) {
        let event = Event::HarnessNotice(match purpose {
            tau_proto::NoticePurpose::Response => {
                tau_proto::HarnessNotice::response(kind, message, level)
            }
            tau_proto::NoticePurpose::Alert => {
                tau_proto::HarnessNotice::alert(kind, message, level)
            }
            tau_proto::NoticePurpose::Diagnostic => {
                tau_proto::HarnessNotice::diagnostic(kind, message, level)
            }
        });
        let frame = HarnessOutputMessage::deliver_live(tau_proto::UnixMicros::now(), event);
        let _ = self.runtime_io.bus.send_to(client_id, None, frame);
    }

    /// Sends exact live-only feedback to the UI that initiated an action.
    pub(super) fn send_ui_response(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        message: impl Into<String>,
    ) {
        self.send_direct_harness_notice(
            client_id,
            tau_proto::notice_kind::HARNESS_NOTICE,
            tau_proto::NoticeLevel::Info,
            tau_proto::NoticePurpose::Response,
            message.into(),
        );
    }

    /// Sends a live-only command rejection to the UI that initiated an action.
    pub(super) fn send_ui_error_response(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        message: impl Into<String>,
    ) {
        self.send_direct_harness_notice(
            client_id,
            tau_proto::notice_kind::UI_COMMAND_ERROR,
            tau_proto::NoticeLevel::Warning,
            tau_proto::NoticePurpose::Response,
            message.into(),
        );
    }

    pub(super) fn handle_ui_tree_request(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        tau_proto::UiTreeRequest {
            session_id,
            target_agent_id,
        }: tau_proto::UiTreeRequest,
    ) {
        if !self.is_attached_socket_ui(client_id) {
            return;
        }
        let message = self.tree_request_result(&session_id, target_agent_id.as_deref());
        self.send_direct_harness_notice(
            client_id,
            tau_proto::notice_kind::HARNESS_NOTICE,
            tau_proto::NoticeLevel::Info,
            tau_proto::NoticePurpose::Response,
            message,
        );
    }

    pub(super) fn handle_recall_queued_prompt(&mut self, req: &tau_proto::UiRecallQueuedPrompt) {
        if req.session_id != self.session_runtime.current_session_id {
            return;
        }
        let Some(cid) = self.runtime_agent_id_for_target_agent(req.target_agent_id.as_deref())
        else {
            return;
        };
        let Some(prompt) = self
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .and_then(|conv| {
                let index = conv
                    .dispatch
                    .pending_prompts
                    .iter()
                    .rposition(|prompt| !prompt.is_internal())?;
                conv.dispatch.pending_prompts.remove(index)
            })
        else {
            return;
        };
        if let Some(correlation) = prompt.initial_prompt_correlation.clone() {
            self.publish_initial_prompt_failed(
                correlation,
                tau_proto::AgentPromptFailureStage::Canceled,
                "initial prompt was recalled",
            );
        }
        self.publish_event(
            None,
            Event::AgentPromptRecalled(AgentPromptRecalled {
                agent_id: self
                    .target_agent_id_for_agent(&cid)
                    .expect("agent has durable id"),
                text: prompt.text,
            }),
        );
    }

    pub(super) fn handle_cancel_prompt(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        req: &UiCancelPrompt,
    ) {
        if req.session_id != self.session_runtime.current_session_id {
            self.send_ui_error_response(client_id, "cancel request is for a stale session");
            return;
        }
        let Some(cid) = self.runtime_agent_id_for_target_agent(req.target_agent_id.as_deref())
        else {
            self.send_ui_error_response(client_id, "cancel request targets an unknown agent");
            return;
        };
        let retained_terminal_prompt_id = self.pending_provider_terminal_prompt_id(&cid);
        let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(&cid) else {
            self.send_ui_error_response(client_id, "cancel request targets an unloaded agent");
            return;
        };
        if conv.dispatch.pending_cancel.is_some() {
            self.send_ui_error_response(client_id, "cancellation is already pending");
            return;
        }
        self.cancel_pending_context_claim(&cid);
        let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(&cid) else {
            self.send_ui_error_response(client_id, "cancel request targets an unloaded agent");
            return;
        };
        let output_length_pending = matches!(
            conv.turn.output_length_continuation,
            path_crate_agent::OutputLengthContinuationState::Planned(_)
                | path_crate_agent::OutputLengthContinuationState::OwnerReady(_)
                | path_crate_agent::OutputLengthContinuationState::OwnerPending(_)
                | path_crate_agent::OutputLengthContinuationState::Active(_)
        );
        if matches!(conv.turn.turn_state, AgentTurnState::Idle) && !output_length_pending {
            self.send_ui_error_response(client_id, "no active turn to cancel");
            return;
        }
        let prompt_id = conv
            .dispatch
            .in_flight_prompt
            .clone()
            .or_else(|| match &conv.turn.output_length_continuation {
                path_crate_agent::OutputLengthContinuationState::Active(continuation) => {
                    Some(continuation.plan.agent_prompt_id.clone())
                }
                _ => None,
            })
            .or(retained_terminal_prompt_id);
        conv.dispatch.pending_cancel = Some(PendingCancel {
            requester_client_id: client_id.clone(),
            agent_prompt_id: prompt_id.clone(),
            reason: "cancelled by user".to_owned(),
        });
        let _ = conv;
        self.send_ui_response(client_id, "cancelling current prompt");
        self.fail_pending_initial_prompts(
            &cid,
            tau_proto::AgentPromptFailureStage::Canceled,
            "initial prompt was canceled",
        );
        let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(&cid) else {
            return;
        };
        conv.dispatch
            .pending_prompts
            .retain(PendingPrompt::is_output_length_continuation);

        if let Some(prompt_id) = prompt_id {
            self.publish_event(
                None,
                Event::UiCancelPrompt(UiCancelPrompt {
                    session_id: req.session_id.clone(),
                    target_agent_id: self.target_agent_id_for_agent(&cid),
                    agent_prompt_id: Some(prompt_id),
                }),
            );
        }
        self.apply_pending_cancel_for_agent(&cid);
    }

    pub(super) fn handle_retry_prompt(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        req: tau_proto::UiRetryPrompt,
    ) {
        const RETRY_TOMBSTONE_LIMIT: usize = 1024;
        const PENDING_RETRY_LIMIT: usize = 1024;
        let reject = |this: &mut Self, target_agent_id, label: String, message: &str| {
            let _ = this.runtime_io.bus.send_to(
                client_id,
                None,
                HarnessOutputMessage::deliver(Event::UiRetryPromptResult(
                    tau_proto::UiRetryPromptResult {
                        request_id: req.request_id.clone(),
                        target_agent_id,
                        target_label: label,
                        status: None,
                        message: message.to_owned(),
                    },
                )),
            );
        };
        let request_key = (client_id.to_owned(), req.request_id.clone());
        if !self
            .ui_runtime
            .seen_retry_prompt_requests
            .insert(request_key.clone())
        {
            reject(
                self,
                req.target_agent_id,
                "selected agent".into(),
                "Cannot retry: duplicate retry request.",
            );
            return;
        }
        self.ui_runtime
            .seen_retry_prompt_request_order
            .push_back(request_key);
        while self.ui_runtime.seen_retry_prompt_request_order.len() > RETRY_TOMBSTONE_LIMIT {
            if let Some(expired) = self.ui_runtime.seen_retry_prompt_request_order.pop_front() {
                self.ui_runtime.seen_retry_prompt_requests.remove(&expired);
            }
        }
        if req.session_id != self.session_runtime.current_session_id {
            reject(
                self,
                req.target_agent_id,
                "selected agent".into(),
                "Cannot retry: the session is stale.",
            );
            return;
        }
        let Some(cid) = self.runtime_agent_id_for_target_agent(req.target_agent_id.as_deref())
        else {
            reject(
                self,
                req.target_agent_id,
                "selected agent".into(),
                "Cannot retry: no agent is selected.",
            );
            return;
        };
        let target_agent_id = self
            .target_agent_id_for_agent(&cid)
            .expect("runtime agent has durable id");
        let target_label = self
            .agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .and_then(|agent| normalize_display_name(agent.identity.display_name.as_deref()))
            .unwrap_or_else(|| target_agent_id.as_str().to_owned());
        let Some(agent_prompt_id) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .and_then(|agent| agent.dispatch.in_flight_prompt.clone())
        else {
            reject(
                self,
                Some(target_agent_id),
                target_label,
                "Cannot retry: the selected agent has no active prompt.",
            );
            return;
        };
        let Some(provider_connection_id) = self
            .provider_runtime
            .pending_prompts
            .get(&agent_prompt_id)
            .cloned()
        else {
            reject(
                self,
                Some(target_agent_id),
                target_label,
                "Cannot retry: the prompt's provider route is unavailable.",
            );
            return;
        };
        if self.ui_runtime.pending_retry_prompts.len() >= PENDING_RETRY_LIMIT
            || self
                .ui_runtime
                .pending_retry_prompts
                .values()
                .any(|pending| {
                    pending.requester_client_id == **client_id
                        && pending.agent_prompt_id == agent_prompt_id
                })
        {
            reject(
                self,
                Some(target_agent_id),
                target_label,
                "Cannot retry: a retry request for this prompt is already pending.",
            );
            return;
        }
        let provider_request_id = next_provider_retry_token();
        let targeted = Event::UiRetryPrompt(tau_proto::UiRetryPrompt {
            request_id: provider_request_id.clone(),
            session_id: req.session_id,
            target_agent_id: Some(target_agent_id.clone()),
            agent_prompt_id: Some(agent_prompt_id.clone()),
        });
        let delivered = self
            .runtime_io
            .bus
            .send_to(
                &provider_connection_id,
                Some(client_id),
                HarnessOutputMessage::deliver(targeted),
            )
            .is_ok_and(|report| !report.delivered_to.is_empty());
        if !delivered {
            reject(
                self,
                Some(target_agent_id),
                target_label,
                "Cannot retry: the prompt's provider route is unavailable.",
            );
            return;
        }
        if self.agent_is_ephemeral(&target_agent_id) {
            self.prompt_coordination
                .prompt_runtime
                .ephemeral_provider_retry_requests
                .insert(provider_request_id.clone());
        }
        self.ui_runtime.pending_retry_prompts.insert(
            provider_request_id,
            PendingRetryPrompt {
                ui_request_id: req.request_id,
                provider_connection_id,
                requester_client_id: client_id.clone(),
                agent_prompt_id,
                target_agent_id,
                target_label,
            },
        );
    }

    /// Marks an intercepted reactive claim for durable cancellation when its
    /// start eventually commits, without allowing remote dispatch.
    pub(super) fn cancel_pending_context_claim(&mut self, cid: &AgentId) {
        let restored_pending =
            self.agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|agent| match &agent.dispatch.activation_dispatch {
                    path_crate_agent::ActivationDispatchState::ContextRecoveryPending {
                        checkpoint,
                    } => Some(checkpoint.clone()),
                    _ => None,
                });
        if let Some(checkpoint) = restored_pending {
            self.terminalize_replay_blocked_context_recovery(
                cid,
                &checkpoint,
                tau_proto::StandaloneCompactionFailureReason::Cancelled,
            );
            return;
        }
        let pending = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| match &agent.dispatch.activation_dispatch {
                path_crate_agent::ActivationDispatchState::ContextRecoveryClaimPending {
                    checkpoint: _,
                    transaction_id,
                } => Some((agent.identity.agent_id.clone()?, transaction_id.clone())),
                _ => None,
            });
        if let Some((agent_id, transaction_id)) = pending {
            self.prompt_coordination
                .compaction_runtime
                .suppress_start_for_cancellation(agent_id, transaction_id);
        }
    }

    pub(super) fn apply_pending_cancel_for_agent(&mut self, cid: &AgentId) {
        if !self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|conv| conv.dispatch.pending_cancel.is_some())
        {
            return;
        }
        let dormant_repair_exists = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
            .and_then(tau_core::AgentTree::output_length_dormant_repair)
            .is_some();
        if dormant_repair_exists {
            if matches!(
                self.prompt_coordination
                    .prompt_runtime
                    .pending_publish_completions
                    .get(cid),
                Some(AgentPublishCompletion::OutputLengthDormantRepair { .. })
            ) {
                self.retry_pending_agent_publish_completion(cid);
            } else if self.publish_chain_is_idle() {
                self.repair_dormant_output_length_lineage(cid);
            }
            return;
        }
        let Some(turn_state) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .map(|conv| conv.turn.turn_state.clone())
        else {
            return;
        };
        match turn_state {
            AgentTurnState::Idle => {
                let continuation_state = self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(cid)
                    .map(|agent| agent.turn.output_length_continuation.clone());
                match continuation_state {
                    Some(path_crate_agent::OutputLengthContinuationState::OwnerReady(_)) => {
                        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                            agent.dispatch.pending_replay_activation = true;
                        }
                        self.dispatch_activation_after_publish_idle(cid);
                        return;
                    }
                    Some(path_crate_agent::OutputLengthContinuationState::Planned(_)) => {
                        if self.publish_chain_is_idle() {
                            let has_marker = self.agent_runtime.agent_registry.agents.get(cid).is_some_and(|agent| {
                                agent
                                    .dispatch.pending_prompts
                                    .iter()
                                    .any(PendingPrompt::is_output_length_continuation)
                            });
                            if !has_marker && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                                agent
                                    .dispatch.pending_prompts
                                    .push_back(PendingPrompt::output_length_continuation());
                            }
                            self.fold_pending_prompts_as_steered(cid);
                            self.dispatch_activation_after_publish_idle(cid);
                        }
                        return;
                    }
                    Some(path_crate_agent::OutputLengthContinuationState::Active(_))
                        if self.agent_runtime.agent_registry.agents.get(cid).is_some_and(|agent| {
                            matches!(
                                agent.dispatch.activation_dispatch,
                                path_crate_agent::ActivationDispatchState::AwaitingCheckpoint {
                                    owner:
                                        path_crate_agent::InferenceCheckpointOwner::Standalone {
                                            ..
                                        },
                                    ..
                                }
                            )
                        }) =>
                    {
                        // A cold restart between successful compaction and its
                        // exact descendant checkpoint restores the reserved
                        // output-length owner before any provider delivery.
                        // Commit that owner first; its write-complete handler
                        // observes the pending cancellation and authors the
                        // sole canonical Cancelled terminal.
                        self.retry_standalone_inference_checkpoint(cid);
                        return;
                    }
                    Some(path_crate_agent::OutputLengthContinuationState::Active(_))
                        if self.agent_runtime.agent_registry.agents.get(cid).is_some_and(|agent| {
                            matches!(
                                agent.dispatch.activation_dispatch,
                                path_crate_agent::ActivationDispatchState::ContextRecoveryClaimPending {
                                    ..
                                }
                            )
                        }) =>
                    {
                        // The staged reactive Start owns the sole following
                        // failure. Preserve cancellation until Start
                        // write-complete can arbitrate StaleBranch to Cancelled.
                        return;
                    }
                    _ => {}
                }
                if self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(cid)
                    .is_some_and(|agent| {
                        matches!(
                            agent.turn.output_length_continuation,
                            path_crate_agent::OutputLengthContinuationState::OwnerPending(_)
                        )
                    })
                {
                    return;
                }
                self.fail_pending_initial_prompts(
                    cid,
                    tau_proto::AgentPromptFailureStage::Canceled,
                    "initial prompt was canceled",
                );
                if let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                    conv.dispatch.pending_cancel = None;
                    conv.dispatch.pending_prompts.clear();
                }
            }
            AgentTurnState::AgentThinking { .. } => {
                self.finalize_canceled_in_flight_prompt(cid);
                if self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(cid)
                    .is_some_and(|agent| agent.dispatch.pending_cancel.is_some())
                {
                    return;
                }
                self.try_advance_queue();
            }
            AgentTurnState::ToolsRunning { remaining_calls } => {
                self.reject_pending_ui_compaction(cid);
                let mut cancelled_calls = remaining_calls.ordered_remaining();
                cancelled_calls.extend(
                    self.tool_routing
                        .tool_runtime
                        .tool_turn
                        .backgrounded_calls_for(cid),
                );
                cancelled_calls.sort();
                cancelled_calls.dedup();
                let foreground_pending = self.cancel_remaining_tool_calls(
                    cid,
                    cancelled_calls,
                    BackgroundCompletionPromptMode::QueuePassive,
                );
                if !foreground_pending {
                    self.finalize_cancelled_tool_turn(cid);
                }
            }
        }
    }

    pub(super) fn finalize_cancelled_tool_turn(&mut self, cid: &AgentId) {
        let requester = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| {
                agent
                    .dispatch
                    .pending_cancel
                    .as_ref()
                    .map(|pending| pending.requester_client_id.clone())
            });
        self.fail_pending_initial_prompts(
            cid,
            tau_proto::AgentPromptFailureStage::Canceled,
            "initial prompt was canceled",
        );
        if let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            conv.dispatch.pending_cancel = None;
            conv.turn.work_status.clear_working_reminder();
            // User cancellation discards stale queued work, but keeps passive
            // background notices so the next user prompt can observe terminal
            // background cancellation events.
            conv.dispatch.pending_prompts.retain(|prompt| {
                prompt.is_passive_background_completion() || prompt.is_self_compaction_terminal()
            });
            conv.dispatch.in_flight_prompt = None;
        }
        self.set_agent_turn_state(cid, AgentTurnState::Idle);
        if let Some(requester) = requester {
            self.send_ui_response(&requester, "cancelled current turn");
        }
        if self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|agent| {
                agent
                    .dispatch
                    .pending_prompts
                    .iter()
                    .any(PendingPrompt::is_self_compaction_terminal)
            })
        {
            self.fold_pending_prompts_as_steered(cid);
            self.dispatch_prompt_after_publish_idle(cid);
        }
        self.try_advance_queue();
    }

    /// Terminalizes one live cancellation and rejects its eventual late
    /// provider response.
    ///
    /// Only an exact ordinary-inference `DispatchUncertain` owner is released.
    /// Mismatched prompt ids and standalone-compaction continuations retain
    /// their distinct durable recovery obligations.
    pub(super) fn finalize_canceled_in_flight_prompt(&mut self, cid: &AgentId) {
        let Some((session_id, canceled_prompt_id, originator)) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|conv| {
                conv.dispatch
                    .in_flight_prompt
                    .clone()
                    .or_else(|| match &conv.turn.output_length_continuation {
                        path_crate_agent::OutputLengthContinuationState::Active(continuation) => {
                            Some(continuation.plan.agent_prompt_id.clone())
                        }
                        _ => None,
                    })
                    .or_else(|| {
                        conv.dispatch
                            .pending_cancel
                            .as_ref()
                            .and_then(|pending| pending.agent_prompt_id.clone())
                    })
                    .map(|agent_prompt_id| {
                        (
                            conv.identity.session_id.clone(),
                            agent_prompt_id,
                            conv.identity.originator.clone(),
                        )
                    })
            })
        else {
            return;
        };
        self.prompt_coordination
            .prompt_runtime
            .pending_materialization_timings
            .remove(&canceled_prompt_id);
        let marked_owner = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
            .and_then(|tree| tree.marked_inference_through(&canceled_prompt_id))
            .is_some();
        self.cancel_running_compaction(cid, &canceled_prompt_id);
        let output_length_owner =
            self.agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|agent| match &agent.turn.output_length_continuation {
                    path_crate_agent::OutputLengthContinuationState::Active(continuation)
                        if continuation.plan.agent_prompt_id == canceled_prompt_id =>
                    {
                        Some(continuation.plan.owner.clone())
                    }
                    _ => None,
                });
        if let Some(owner) = output_length_owner {
            let terminal_write_pending = self
                .runtime_io
                .publication
                .pending_intercept
                .as_ref()
                .is_some_and(|pending| {
                    matches!(
                            &pending.event,
                        Event::ProviderResponseFinished(response)
                            if response.agent_prompt_id == canceled_prompt_id
                                && matches!(
                                    response.output_length_disposition,
                                    tau_proto::OutputLengthDisposition::ContinuationTerminal { .. }
                                )
                    )
                })
                || self.runtime_io.publication.deferred.iter().any(|pending| {
                    matches!(
                        pending.event(),
                        Event::ProviderResponseFinished(response)
                            if response.agent_prompt_id == canceled_prompt_id
                                && matches!(
                                    response.output_length_disposition,
                                    tau_proto::OutputLengthDisposition::ContinuationTerminal { .. }
                                )
                    )
                })
                || self
                    .prompt_coordination
                    .prompt_runtime
                    .pending_publish_completions
                    .get(cid)
                    .is_some_and(|completion| {
                        completion.owns_output_length_terminal(&canceled_prompt_id)
                    });
            if self
                .prompt_coordination
                .prompt_runtime
                .local_route_failures
                .contains(&canceled_prompt_id)
                || terminal_write_pending
            {
                if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                    agent.dispatch.pending_prompts.clear();
                }
                return;
            }
            let status_was_available = self
                .prompt_coordination
                .prompt_runtime
                .tool_specs
                .get(&canceled_prompt_id)
                .is_some_and(|specs| {
                    specs
                        .iter()
                        .any(|spec| self.tool_model_visible_name(spec).as_str() == "status")
                });
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                agent.turn.terminal_status_was_available = status_was_available;
                agent.turn.terminal_notice_eligible = false;
            }
            let automatic_compaction_decision = self
                .prompt_coordination
                .prompt_runtime
                .models
                .get(&canceled_prompt_id)
                .cloned()
                .and_then(|model| {
                    let reported_input =
                        self.automatic_compaction_reported_input_tokens(cid, &model);
                    let policies = self
                        .prompt_coordination
                        .prompt_runtime
                        .compaction_policies
                        .get(&canceled_prompt_id)
                        .cloned()
                        .unwrap_or_default();
                    self.eager_automatic_compaction_decision(
                        cid,
                        model,
                        reported_input,
                        None,
                        &policies,
                    )
                });
            let response = ProviderResponseFinished {
                automatic_compaction_decision,
                agent_prompt_id: canceled_prompt_id.clone(),
                agent_id: self
                    .target_agent_id_for_agent(cid)
                    .expect("loaded agent has durable identity"),
                output_items: Vec::new(),
                stop_reason: ProviderStopReason::Error,
                error: Some("cancelled".to_owned()),
                failure_kind: None,
                context_limit_telemetry: None,
                recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
                output_length_disposition:
                    tau_proto::OutputLengthDisposition::ContinuationTerminal {
                        outer_turn_id: owner.outer_turn_id.clone(),
                        source_agent_prompt_id: owner.source_agent_prompt_id,
                        ordinal: owner.ordinal,
                        outcome: tau_proto::OutputLengthContinuationOutcome::Cancelled,
                        outer_turn_finish_owed: true,
                    },
                originator,
                usage: None,
                estimated_api_cost_rates: None,
                estimated_api_cost_increment: None,
                compaction_original_input_tokens: None,
                compaction_output_tokens: None,
                backend: None,
                provider_attempt: Default::default(),
                provider_response_id: None,
                ws_pool_delta: None,
            };
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                agent
                    .dispatch
                    .pending_prompts
                    .retain(PendingPrompt::is_output_length_continuation);
            }
            self.prompt_coordination
                .canceled_prompts
                .insert(canceled_prompt_id.clone());
            let completion = Some(AgentPublishCompletion::OutputLengthContinuation {
                batch_parent: self
                    .selected_head_for_agent(cid)
                    .unwrap_or(tau_proto::AgentHead::Root),
                reducer: CommittedOutputLengthContinuation {
                    response: Box::new(response.clone()),
                    assistant_text: None,
                },
                owned_publication: None,
            });
            self.publish_finished_response_for_agent(cid, None, &response, completion, false);
            return;
        }
        if self.provider_terminal_publication_pending(cid, &canceled_prompt_id) {
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                agent.dispatch.pending_prompts.clear();
            }
            return;
        }
        let status_was_available = self
            .prompt_coordination
            .prompt_runtime
            .tool_specs
            .get(&canceled_prompt_id)
            .is_some_and(|specs| {
                specs
                    .iter()
                    .any(|spec| self.tool_model_visible_name(spec).as_str() == "status")
            });
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            agent.turn.terminal_status_was_available = status_was_available;
            agent.turn.terminal_notice_eligible = false;
        }
        let cancellation_decision = self
            .prompt_coordination
            .prompt_runtime
            .models
            .get(&canceled_prompt_id)
            .cloned()
            .and_then(|model| {
                let reported_input = self.automatic_compaction_reported_input_tokens(cid, &model);
                let policies = self
                    .prompt_coordination
                    .prompt_runtime
                    .compaction_policies
                    .get(&canceled_prompt_id)
                    .cloned()
                    .unwrap_or_default();
                self.eager_automatic_compaction_decision(
                    cid,
                    model,
                    reported_input,
                    None,
                    &policies,
                )
            });
        self.publish_prompt_terminated_with_decision(
            session_id,
            canceled_prompt_id.clone(),
            AgentPromptTerminationReason::Canceled,
            originator,
            cancellation_decision,
        );
        if marked_owner {
            return;
        }
        self.prompt_coordination
            .canceled_prompts
            .insert(canceled_prompt_id.clone());
        self.prompt_coordination
            .prompt_runtime
            .operations
            .remove(&canceled_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .context_limits
            .remove(&canceled_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .context_size_alerts
            .remove(&canceled_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .compaction_policies
            .remove(&canceled_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .semantic_output
            .remove(&canceled_prompt_id);
        self.fail_pending_initial_prompts(
            cid,
            tau_proto::AgentPromptFailureStage::Canceled,
            "initial prompt was canceled",
        );
        if let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            conv.dispatch.pending_cancel = None;
            conv.turn.work_status.clear_working_reminder();
            conv.dispatch.pending_prompts.clear();
            conv.dispatch.in_flight_prompt = None;
            if matches!(
                &conv.dispatch.activation_dispatch,
                crate::agent::ActivationDispatchState::DispatchUncertain {
                    owner: crate::agent::InferenceCheckpointOwner::Inference,
                    agent_prompt_id,
                    ..
                } if agent_prompt_id == &canceled_prompt_id
            ) {
                conv.dispatch.activation_dispatch = path_crate_agent::ActivationDispatchState::None;
            }
        }
        self.set_agent_turn_state(cid, AgentTurnState::Idle);
    }

    /// Records a durable terminal outcome when cancellation targets an active
    /// standalone compaction prompt.
    pub(super) fn cancel_running_compaction(&mut self, cid: &AgentId, prompt_id: &AgentPromptId) {
        let transaction = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|conv| match &conv.dispatch.activation_dispatch {
                path_crate_agent::ActivationDispatchState::Running {
                    id,
                    cut,
                    resume_through,
                    compact_prompt_id,
                    ..
                } if compact_prompt_id == prompt_id => Some((
                    conv.identity.agent_id.clone()?,
                    id.clone(),
                    *cut,
                    *resume_through,
                )),
                _ => None,
            });
        if let Some((agent_id, transaction_id, cut, resume_through)) = transaction {
            self.publish_awaiting_cancelled_standalone_accounting(prompt_id);
            self.publish_event_for_agent_with_completion(
                cid,
                None,
                Event::AgentStandaloneCompactionFailed(
                    tau_proto::AgentStandaloneCompactionFailed {
                        agent_id,
                        transaction_id,
                        cut,
                        reason: tau_proto::StandaloneCompactionFailureReason::Cancelled,
                        resume_through,
                        context_retreat: None,
                        incomplete_response: None,
                    },
                ),
                Some(AgentPublishCompletion::OwedCompactionFact {
                    batch_parent: self
                        .selected_head_for_agent(cid)
                        .unwrap_or(tau_proto::AgentHead::Root),
                    owned_publication: None,
                }),
                false,
            );
        }
    }

    pub(super) fn finish_backgrounded_tool_cancelled_by_harness(
        &mut self,
        target: CancelTarget,
        completion_prompt_mode: BackgroundCompletionPromptMode,
    ) {
        if !self
            .tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&target.call_id)
        {
            return;
        }
        if !self
            .tool_routing
            .tool_runtime
            .tool_agents
            .contains_key(&target.call_id)
        {
            return;
        }
        if let Some(accepted) = self
            .prompt_coordination
            .compaction_runtime
            .accepted_manual_tools
            .values()
            .find(|accepted| {
                accepted
                    .request
                    .tool_source()
                    .is_some_and(|source| source.initiating_tool_call_id == target.call_id)
            })
            .cloned()
            && let Some(target_cid) = self
                .runtime_agent_id_for_target_agent(Some(accepted.request.target_agent_id.as_str()))
        {
            self.fail_accepted_manual_compaction(
                &target_cid,
                &accepted.request,
                tau_proto::ManualCompactionRequestFailureReason::Cancelled,
            );
            return;
        }
        if let Some((transaction_id, pending)) = self
            .prompt_coordination
            .compaction_runtime
            .model_tool_start_by_call(&target.call_id)
            && let Some(target_cid) =
                self.runtime_agent_id_for_target_agent(Some(pending.target_agent_id.as_str()))
        {
            let compact_prompt_id = self
                .agent_runtime
                .agent_registry
                .agents
                .get(&target_cid)
                .and_then(|agent| match &agent.dispatch.activation_dispatch {
                    path_crate_agent::ActivationDispatchState::Running {
                        id,
                        compact_prompt_id,
                        ..
                    } if *id == transaction_id => Some(compact_prompt_id.clone()),
                    _ => None,
                });
            if let Some(compact_prompt_id) = compact_prompt_id {
                self.cancel_running_compaction(&target_cid, &compact_prompt_id);
                return;
            }
        }
        // Complete the actual-running background call. Delegate teardown skips
        // model-visible completion prompts for work from a removed branch,
        // while live-branch turn cancellation keeps a queued internal notice so
        // the placeholder has an observable completion on that branch without
        // immediately auto-advancing the model.
        if let Some(cid) = self
            .tool_routing
            .tool_runtime
            .tool_agents
            .get(&target.call_id)
            .cloned()
        {
            self.observe_tool_terminal(
                &cid,
                &target.call_id,
                tau_proto::ToolTerminalCause::LifecycleTeardown,
            );
        }
        let error = ToolError {
            presentation: Default::default(),
            call_id: target.call_id,
            tool_name: target.tool_name,
            tool_type: target.tool_type,
            message: "Tool call canceled".to_owned(),
            details: None,
            display: None,
            originator: PromptOriginator::User,
        };
        self.handle_background_tool_error_inner(
            Some(crate::harness::harness_connection_id()),
            error,
            completion_prompt_mode,
            tau_proto::ToolTerminalCause::LifecycleTeardown,
        );
    }

    pub(super) fn cancel_delegate_side_conversation(&mut self, target_call_id: &ToolCallId) {
        let Some((cid, turn_state)) =
            self.agent_runtime
                .agent_registry
                .agents
                .iter()
                .find_map(|(cid, conv)| {
                    if conv.identity.parent_tool_call_id.as_ref() != Some(target_call_id) {
                        return None;
                    }
                    Some((cid.clone(), conv.turn.turn_state.clone()))
                })
        else {
            return;
        };
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
            agent.dispatch.terminating = true;
        }

        let mut cancelled_calls = match turn_state {
            AgentTurnState::ToolsRunning { remaining_calls } => remaining_calls.ordered_remaining(),
            _ => Vec::new(),
        };
        cancelled_calls.extend(
            self.tool_routing
                .tool_runtime
                .tool_turn
                .backgrounded_calls_for(&cid),
        );
        cancelled_calls.extend(self.background_completion_call_ids_for_teardown(&cid));
        cancelled_calls.sort();
        cancelled_calls.dedup();
        let foreground_pending = self.cancel_remaining_tool_calls(
            &cid,
            cancelled_calls,
            BackgroundCompletionPromptMode::DoNotQueue,
        );
        if foreground_pending {
            return;
        }
        self.finish_cancel_delegate_side_conversation(&cid);
    }

    pub(super) fn finish_cancel_delegate_side_conversation(&mut self, cid: &AgentId) {
        let Some((session_id, spid, originator)) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .map(|conv| {
                (
                    conv.identity.session_id.clone(),
                    conv.dispatch.in_flight_prompt.clone(),
                    conv.identity.originator.clone(),
                )
            })
        else {
            return;
        };
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            agent.dispatch.terminating = false;
        }
        let marked_owner = spid.as_ref().is_some_and(|prompt_id| {
            self.agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|agent| agent.identity.agent_id.as_deref())
                .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
                .and_then(|tree| tree.marked_inference_through(prompt_id))
                .is_some()
        });
        if marked_owner {
            self.remove_agent_expected(cid);
            return;
        }
        self.cancel_pending_context_claim(cid);
        if let Some(spid) = spid {
            self.cancel_running_compaction(cid, &spid);
            self.prompt_coordination
                .prompt_runtime
                .semantic_output
                .remove(&spid);
            self.prompt_coordination
                .canceled_prompts
                .insert(spid.clone());
            self.publish_prompt_terminated(
                session_id.clone(),
                spid.clone(),
                AgentPromptTerminationReason::Canceled,
                originator,
            );
            self.remember_ephemeral_provider_prompt(&spid);
            self.prompt_coordination.prompt_runtime.agents.remove(&spid);
            self.prompt_coordination
                .prompt_runtime
                .operations
                .remove(&spid);
            self.prompt_coordination
                .prompt_runtime
                .context_limits
                .remove(&spid);
            self.prompt_coordination
                .prompt_runtime
                .context_size_alerts
                .remove(&spid);
            self.prompt_coordination
                .prompt_runtime
                .compaction_policies
                .remove(&spid);
            self.publish_event(
                None,
                Event::UiCancelPrompt(UiCancelPrompt {
                    session_id,
                    target_agent_id: self.target_agent_id_for_agent(cid),
                    agent_prompt_id: Some(spid),
                }),
            );
        }
        self.release_start_agent_request(cid);
        self.remove_agent_expected(cid);
        self.try_advance_queue();
    }
    pub(super) fn is_ui_client(&self, client_id: &tau_proto::ConnectionId) -> bool {
        self.runtime_io
            .bus
            .connection(client_id)
            .is_some_and(|connection| connection.kind == ClientKind::Ui)
    }

    pub(super) fn is_attached_socket_ui(&self, client_id: &tau_proto::ConnectionId) -> bool {
        self.runtime_io
            .bus
            .connection(client_id)
            .is_some_and(|connection| {
                connection.kind == ClientKind::Ui && connection.origin == ConnectionOrigin::Socket
            })
            && !self.ui_runtime.runtime_probe_peers.contains(client_id)
            && !self
                .peer_messaging
                .external_message_peers
                .contains(client_id)
    }

    pub(super) fn handle_client_ui_event(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        event: Event,
    ) -> Result<(bool, Option<Event>), HarnessError> {
        match event {
            Event::UiRoleSelect(select) => self
                .handle_ui_role_select(client_id, select)
                .map(|keep_going| (keep_going, None)),
            Event::UiAgentModelSelect(select) => self
                .handle_ui_agent_model_select(client_id, select)
                .map(|keep_going| (keep_going, None)),
            Event::UiRoleUpdate(req) => self
                .handle_ui_role_update(client_id, req)
                .map(|keep_going| (keep_going, None)),
            Event::UiPromptSubmitted(prompt) => {
                if self.is_attached_socket_ui(client_id) {
                    self.handle_authenticated_ui_prompt_submitted(client_id, prompt)
                        .map(|keep_going| (keep_going, None))
                } else {
                    Ok((true, None))
                }
            }
            Event::ActionInvoke(invoke) => self
                .handle_action_invoke(client_id, invoke)
                .map(|keep_going| (keep_going, None)),
            Event::UiShellCommand(command) => {
                self.handle_ui_shell_command(client_id, command);
                Ok((true, None))
            }
            Event::ActionSchemaDeclared(_)
            | Event::ActionSchemaPublished(_)
            | Event::ActionResultReported(_)
            | Event::ActionResult(_)
            | Event::ActionErrorReported(_)
            | Event::ActionError(_) => Ok((true, None)),
            Event::UiCreateAgent(req) => {
                if self.is_attached_socket_ui(client_id) {
                    self.handle_ui_create_agent_from(client_id, req)
                        .map(|keep_going| (keep_going, None))
                } else {
                    Ok((true, None))
                }
            }
            Event::UiSetAgentDisplayName(req) => self
                .handle_ui_set_agent_display_name(client_id, req)
                .map(|keep_going| (keep_going, None)),
            Event::UiNavigateTree(req) => self
                .handle_ui_navigate_tree(client_id, req)
                .map(|keep_going| (keep_going, None)),
            Event::UiCompactRequest(req) => self
                .handle_ui_compact_request(client_id, req)
                .map(|keep_going| (keep_going, None)),
            Event::UiCancelPrompt(req) => {
                self.handle_cancel_prompt(client_id, &req);
                Ok((true, None))
            }
            Event::UiRetryPrompt(req) => {
                self.handle_retry_prompt(client_id, req);
                Ok((true, None))
            }
            Event::UiSetAgentNavigationMode(req) => {
                let is_ui = self.runtime_io.bus.connections().iter().any(|connection| {
                    connection.id == **client_id && connection.kind == tau_proto::ClientKind::Ui
                }) && !self
                    .peer_messaging
                    .external_message_peers
                    .iter()
                    .any(|connection_id| connection_id == client_id);
                if is_ui {
                    self.handle_set_agent_navigation_mode(client_id, req);
                }
                Ok((true, None))
            }
            Event::UiRecallQueuedPrompt(req) => {
                self.handle_recall_queued_prompt(&req);
                Ok((true, None))
            }
            other => Ok((true, Some(other))),
        }
    }

    pub(super) fn next_ui_shell_route_id(&mut self) -> UiShellRouteId {
        loop {
            let route_id = UiShellRouteId::new(shell_route_id(
                self.ui_runtime.ui_shell_route_rng.next_u64(),
                self.ui_runtime.ui_shell_route_rng.next_u64(),
            ));
            if !self
                .ui_runtime
                .pending_ui_shell_commands
                .contains_key(&route_id)
                && !self
                    .ui_runtime
                    .ephemeral_ui_shell_route_ids
                    .contains(&route_id)
            {
                return route_id;
            }
        }
    }

    /// Sends the process-local onboarding hint only to the spawning interactive
    /// UI, directing setup when no accepted provider model routes are
    /// available.
    pub(crate) fn send_introduction_notice_to_initial_client(
        &mut self,
        initial_client_id: Option<&tau_proto::ConnectionId>,
    ) {
        let Some(client_id) = initial_client_id.filter(|_| {
            self.config
                .accepted_harness_settings
                .show_introduction_notice
        }) else {
            return;
        };
        let message = if self.provider_runtime.model_info.is_empty() {
            "Welcome to Tau! No usable LLM provider is available. Run `tau provider add`, then restart Tau."
        } else {
            "Welcome to Tau! Ask your model to introduce you to Tau."
        };
        self.send_direct_harness_notice(
            client_id,
            tau_proto::notice_kind::HARNESS_INTRODUCTION,
            tau_proto::NoticeLevel::Info,
            tau_proto::NoticePurpose::Diagnostic,
            message.to_owned(),
        );
    }

    pub(super) fn handle_ui_role_select(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        select: tau_proto::UiRoleSelect,
    ) -> Result<bool, HarnessError> {
        self.clear_cache_refreshes(tau_proto::ProviderCacheRefreshCancelReason::PolicyChanged);
        if !self.config.available_roles.contains_key(&select.role) {
            let message = self
                .config
                .disabled_role_reasons
                .get(&select.role)
                .map(|reason| reason.message.clone())
                .unwrap_or_else(|| format!("unknown role: {}", select.role));
            self.send_ui_error_response(client_id, message);
            return Ok(true);
        }

        let was_empty = self.config.selected_model.is_none();
        self.config.selected_role = select.role.clone();
        self.reconcile_selected_model_with_available();
        self.reconcile_agent_context_usage_models();
        if self.config.selected_model.is_none() {
            self.send_ui_error_response(
                client_id,
                format!("role `{}` has no available model", select.role),
            );
        }
        self.publish_current_model_state();
        if was_empty
            && self.config.selected_model.is_some()
            && self.session_runtime.turn_state.is_idle()
        {
            self.try_advance_queue();
        }
        Ok(true)
    }

    pub(super) fn handle_ui_agent_model_select(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        select: tau_proto::UiAgentModelSelect,
    ) -> Result<bool, HarnessError> {
        self.clear_cache_refreshes(tau_proto::ProviderCacheRefreshCancelReason::ModelChanged);
        if !self
            .provider_runtime
            .available_models
            .contains(&select.model)
        {
            self.send_ui_error_response(client_id, format!("unknown model: {}", select.model));
            return Ok(true);
        }
        let cid = if let Some(target_agent_id) = select.target_agent_id.as_deref() {
            self.runtime_agent_id_for_target_agent(Some(target_agent_id))
        } else {
            let mut matches =
                self.agent_runtime
                    .agent_registry
                    .agents
                    .iter()
                    .filter_map(|(cid, conv)| {
                        (conv.identity.session_id == select.session_id
                            && conv.identity.originator.is_user()
                            && conv.identity.agent_id.is_some())
                        .then_some(cid.clone())
                    });
            let first = matches.next();
            if matches.next().is_some() {
                None
            } else {
                first
            }
        };
        let Some(cid) = cid else {
            self.send_ui_error_response(client_id, ":model: no selected agent to update");
            return Ok(true);
        };
        let previous_usage_model = self
            .agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .and_then(|conv| conv.execution.context_usage_model.clone());
        let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(&cid) else {
            self.send_ui_error_response(client_id, ":model: selected agent is not loaded");
            return Ok(true);
        };
        if conv.identity.session_id != select.session_id {
            self.send_ui_error_response(client_id, ":model: selected agent is not in this session");
            return Ok(true);
        }
        conv.identity.model_override = Some(select.model.clone());
        let agent_name = conv
            .identity
            .display_name
            .clone()
            .or_else(|| conv.identity.agent_id.as_ref().map(ToString::to_string))
            .unwrap_or_else(|| cid.to_string());
        if previous_usage_model.as_ref() != Some(&select.model) {
            self.clear_agent_context_usage(&cid);
        }
        self.send_ui_response(
            client_id,
            format!("agent `{agent_name}` model set to {}", select.model),
        );
        Ok(true)
    }

    pub(super) fn handle_ui_role_update(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        req: tau_proto::UiRoleUpdate,
    ) -> Result<bool, HarnessError> {
        if matches!(
            &req.action,
            tau_proto::UiRoleUpdateAction::SetEffort {
                effort: Some(intent)
            } if !intent.is_nominal()
        ) {
            self.send_ui_error_response(
                client_id,
                ":role: absolute reasoning intensity must be between 0.0 and 1.0",
            );
            return Ok(true);
        }
        self.clear_cache_refreshes(tau_proto::ProviderCacheRefreshCancelReason::PolicyChanged);
        if let Some(reason) = self.config.disabled_role_reasons.get(&req.role) {
            self.send_ui_error_response(
                client_id,
                format!(
                    ":role: role `{}` is disabled by configuration: {}",
                    req.role, reason.message
                ),
            );
            return Ok(true);
        }
        let mut selected_role_changed = false;
        let selected_was_empty = self.config.selected_model.is_none();
        match req.action {
            tau_proto::UiRoleUpdateAction::Delete => {
                selected_role_changed = self.handle_ui_role_delete(client_id, req.role)?;
            }
            action => {
                if let Some(next_role) = self.role_after_update(&req.role, action) {
                    self.config
                        .available_roles
                        .insert(req.role.clone(), next_role.clone());
                    self.config
                        .role_overrides
                        .insert(req.role.clone(), next_role);
                    selected_role_changed = self.config.selected_role == req.role;
                }
            }
        }
        self.reconcile_agent_context_usage_models();
        if selected_role_changed {
            self.reconcile_selected_model_with_available();
            self.publish_current_model_state();
            if selected_was_empty
                && self.config.selected_model.is_some()
                && self.session_runtime.turn_state.is_idle()
            {
                self.try_advance_queue();
            }
        }
        self.publish_event(
            None,
            Event::HarnessRolesAvailable(tau_proto::HarnessRolesAvailable {
                roles: role_infos(
                    &self.provider_runtime.model_info,
                    &self.config.available_roles,
                    &self.provider_runtime.available_models,
                ),
                groups: self.current_role_groups(),
                custom_prompts: self.config.custom_prompts.clone(),
            }),
        );
        self.publish_delegate_roles_context();
        Ok(true)
    }

    pub(super) fn handle_ui_role_delete(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        role_name: String,
    ) -> Result<bool, HarnessError> {
        let was_selected = self.config.selected_role == role_name;
        let previous_override = self.config.role_overrides.remove(&role_name);
        let configured_role = self
            .config
            .accepted_harness_settings
            .roles
            .get(&role_name)
            .cloned();

        if let Some(role) = configured_role {
            self.config.available_roles.insert(role_name, role);
            return Ok(was_selected);
        }

        let removed_role = self.config.available_roles.remove(&role_name);
        if self.config.available_roles.is_empty() {
            if let Some(role) = removed_role {
                self.config.available_roles.insert(role_name.clone(), role);
            }
            if let Some(role) = previous_override {
                self.config.role_overrides.insert(role_name.clone(), role);
            }
            self.send_ui_error_response(client_id, ":role: cannot delete the last role");
            return Ok(false);
        }
        if was_selected {
            self.config.selected_role = fallback_role(&self.config.available_roles);
            return Ok(true);
        }
        Ok(false)
    }

    pub(super) fn handle_authenticated_ui_prompt_submitted(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        prompt: tau_proto::UiPromptSubmitted,
    ) -> Result<bool, HarnessError> {
        let agent_id = &prompt.agent_id;
        let is_user_interaction =
            prompt.originator.is_user() && !prompt.message_class.is_internal();
        let text = if is_user_interaction && !prompt.literal {
            match self.expand_user_skill_command(&prompt.agent_id, &prompt.text) {
                Ok(text) => text,
                Err(message) => {
                    self.send_ui_error_response(client_id, message);
                    return Ok(true);
                }
            }
        } else {
            prompt.text.clone()
        };
        let pending = Self::pending_authenticated_ui_prompt(
            text,
            prompt.message_class,
            is_user_interaction,
            prompt.ctx_id.clone(),
        );
        let will_accept = prompt.session_id == self.session_runtime.current_session_id
            && self
                .agent_runtime
                .agent_registry
                .agent_routes
                .get(agent_id)
                .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
                .is_some_and(|agent| !agent.dispatch.terminating);
        if will_accept
            && is_user_interaction
            && (!self
                .agent_runtime
                .agent_registry
                .session_loaded
                .contains(&prompt.agent_id)
                || !self
                    .agent_runtime
                    .agent_registry
                    .navigation_modes
                    .contains_key(&prompt.agent_id))
        {
            tracing::error!(
                target: "tau_harness",
                agent_id = agent_id.as_str(),
                "routable prompt target is missing loaded membership or navigation mode"
            );
            return Ok(true);
        }
        if will_accept && is_user_interaction {
            self.record_accepted_visible_user_interaction(agent_id.as_str())?;
            if self
                .write_loaded_agent_navigation_mode(
                    &prompt.agent_id,
                    tau_proto::AgentNavigationMode::Active,
                )
                .is_err()
            {
                return Err(HarnessError::Participant(format!(
                    "accepted UI prompt target `{agent_id}` lost its navigation mode"
                )));
            }
        }
        let submission =
            self.submit_prompt_to_agent(prompt.session_id, agent_id.as_str(), pending)?;
        if let PromptSubmission::Rejected { reason } = &submission {
            self.send_ui_error_response(client_id, reason.clone());
        }
        debug_assert_eq!(
            matches!(submission, PromptSubmission::Rejected { .. }),
            !will_accept
        );
        Ok(true)
    }

    /// Classify an authenticated UI prompt and move its already-expanded text
    /// into the selected pending prompt.
    pub(super) fn pending_authenticated_ui_prompt(
        text: String,
        message_class: tau_proto::PromptMessageClass,
        is_user_interaction: bool,
        ctx_id: Option<String>,
    ) -> PendingPrompt {
        let pending = if message_class.is_internal() {
            PendingPrompt::untrusted_internal(text)
        } else if is_user_interaction {
            PendingPrompt::human_ui_watch_notified(text)
        } else {
            PendingPrompt::human_ui(text)
        };
        pending.with_ctx_id(ctx_id)
    }

    /// Record one accepted visible UI interaction without retaining its
    /// content.
    pub(super) fn record_accepted_visible_user_interaction(
        &mut self,
        agent_id: &str,
    ) -> Result<(), HarnessError> {
        let event = Event::AgentUserInteractionRecorded(tau_proto::AgentUserInteractionRecorded {
            agent_id: crate::parse_agent_id(agent_id),
        });
        let parent = self
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(agent_id)
            .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
            .and_then(|agent| agent.identity.head)
            .map_or(
                tau_core::AgentEventParent::Root,
                tau_core::AgentEventParent::Under,
            );
        self.append_direct_agent_semantic_event(agent_id, parent, event.clone())?;
        *self
            .session_runtime
            .precommitted_user_interactions
            .entry(agent_id.to_owned())
            .or_default() += 1;
        self.session_runtime.user_interaction_order.insert(
            agent_id.to_owned(),
            self.session_runtime.next_user_interaction_order,
        );
        self.session_runtime.next_user_interaction_order = self
            .session_runtime
            .next_user_interaction_order
            .saturating_add(1);
        self.enqueue_publish(
            None,
            event,
            true,
            true,
            self.agent_runtime
                .agent_registry
                .agent_routes
                .get(agent_id)
                .cloned()
                .map(|cid| ConversationHeadSync {
                    cid,
                    agent_id: Some(crate::parse_agent_id(agent_id)),
                    session_generation: self.session_runtime.current_session_generation,
                    fold_parent: None,
                    suppress_activation_dispatch: false,
                    continuation: None,
                    notify_watchers: false,
                }),
        );
        Ok(())
    }

    pub(super) fn expand_user_skill_command(
        &mut self,
        agent_id: &tau_proto::AgentId,
        text: &str,
    ) -> Result<String, String> {
        let Some((name, args)) = user_skill_invocation::parse_user_skill_command(text) else {
            return Ok(text.to_owned());
        };
        if let Some(message) = tau_skills::skill_name_validation_message(name) {
            let message = format!(":skill: invalid skill name `{name}`: {message}");
            self.emit_info(&message);
            return Err(message);
        }
        let skill_name = tau_proto::SkillName::from(name.to_owned());
        let Some(skills) = self
            .prompt_coordination
            .context_discovery
            .frozen_agents
            .get(agent_id)
            .map(|snapshot| &snapshot.skills)
        else {
            let message = format!(":skill: agent `{agent_id}` has no finalized discovery snapshot");
            self.emit_info(&message);
            return Err(message);
        };
        let Some(skill) = skills.get(&skill_name).cloned() else {
            let message = format!(":skill: unknown skill `{name}`");
            self.emit_info(&message);
            return Err(message);
        };
        if !skill.user_invocable {
            let message = format!(":skill: skill `{name}` is not user-invocable");
            self.emit_info(&message);
            return Err(message);
        }
        match user_skill_invocation::read_user_invoked_skill_body(&skill.source) {
            Ok(loaded) => {
                if loaded.truncated {
                    self.emit_info_important(&format!(
                        "skill too long: {} truncated to {} bytes while invoking {name}",
                        skill.source.label(),
                        user_skill_invocation::MAX_USER_INVOKED_SKILL_BYTES
                    ));
                }
                Ok(user_skill_invocation::format_user_invoked_skill_prompt(
                    name,
                    &skill.source,
                    &loaded.body,
                    loaded.truncated.then_some(loaded.total_bytes),
                    args,
                ))
            }
            Err(message) => {
                let message = format!(":skill: failed to load `{name}`: {message}");
                self.emit_info(&message);
                Err(message)
            }
        }
    }

    pub(super) fn resolve_pending_user_skill_for_agent(
        &mut self,
        cid: &AgentId,
        mut prompt: PendingPrompt,
    ) -> Result<PendingPrompt, String> {
        if !prompt.expand_user_skill_on_dispatch {
            return Ok(prompt);
        }
        let Some(agent_id) = self.target_agent_id_for_agent(cid) else {
            return Err("created agent route is unavailable".to_owned());
        };
        prompt.text = self.expand_user_skill_command(&agent_id, &prompt.text)?;
        prompt.expand_user_skill_on_dispatch = false;
        Ok(prompt)
    }

    pub(super) fn handle_ui_set_agent_display_name(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        req: tau_proto::UiSetAgentDisplayName,
    ) -> Result<bool, HarnessError> {
        if req.session_id != self.session_runtime.current_session_id {
            self.send_ui_error_response(
                client_id,
                format!(
                    "harness is bound to session `{}`; agent-name request for `{}` rejected",
                    self.session_runtime.current_session_id.as_str(),
                    req.session_id.as_str()
                ),
            );
            return Ok(true);
        }
        let display_name = normalize_display_name(Some(&req.display_name));
        let Some(display_name) = display_name else {
            self.send_ui_error_response(client_id, "agent display name must not be empty");
            return Ok(true);
        };
        let agent_id = &req.agent_id;
        let Some(cid) = self
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(agent_id)
            .cloned()
        else {
            self.send_ui_error_response(client_id, format!("unknown agent: {agent_id}"));
            return Ok(true);
        };
        if let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
            conv.identity.display_name = Some(display_name.clone());
        }
        self.publish_for_agent(
            &cid,
            Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
                agent_id: req.agent_id,
                display_name,
            }),
        );
        Ok(true)
    }

    pub(super) fn handle_ui_navigate_tree(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        req: tau_proto::UiNavigateTree,
    ) -> Result<bool, HarnessError> {
        // Validate the requested target against *this* harness's bound
        // session before publishing. The durable branch-state fact is
        // agent-owned (`agent.head_moved`), not the UI-scoped request.
        if let Some((cid, agent_id, head)) = self.validate_navigate_tree_target(
            client_id,
            &req.session_id,
            req.target_agent_id.as_deref(),
            req.target,
        ) {
            self.publish_event_for_agent(
                &cid,
                None,
                Event::AgentHeadMoved(tau_proto::AgentHeadMoved { agent_id, head }),
            );
            self.send_ui_response(
                client_id,
                format!("navigated to {}", format_agent_head(head)),
            );
        }
        Ok(true)
    }

    pub(super) fn handle_ui_compact_request(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        req: tau_proto::UiCompactRequest,
    ) -> Result<bool, HarnessError> {
        self.publish_event(Some(client_id), Event::UiCompactRequest(req.clone()));
        self.handle_compact_request(client_id, req.session_id, req.target_agent_id.as_deref());
        Ok(true)
    }

    pub(super) fn runtime_agent_id_for_target_agent(
        &self,
        target_agent_id: Option<&str>,
    ) -> Option<AgentId> {
        self.agent_runtime
            .agent_registry
            .agent_routes
            .get(target_agent_id?)
            .cloned()
    }

    pub(super) fn target_agent_id_for_agent(&self, cid: &AgentId) -> Option<AgentId> {
        self.agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|conv| conv.identity.agent_id.clone())
    }

    pub(super) fn resolve_shell_output_target_agent(
        &mut self,
        finished: &tau_proto::ShellCommandFinished,
    ) -> Option<(AgentId, tau_proto::AgentId)> {
        if finished.session_id != self.session_runtime.current_session_id {
            self.emit_info(&format!(
                "shell output ignored: harness is bound to session `{}` but command finished for `{}`",
                self.session_runtime.current_session_id.as_str(),
                finished.session_id.as_str(),
            ));
            return None;
        }

        if let Some(target_agent_id) = finished.target_agent_id.as_ref() {
            let Some(cid) = self
                .agent_runtime
                .agent_registry
                .agent_routes
                .get(target_agent_id)
                .cloned()
            else {
                self.emit_info(&format!(
                    "shell output ignored: unknown target agent `{target_agent_id}`"
                ));
                return None;
            };
            let Some(conv) = self.agent_runtime.agent_registry.agents.get(&cid) else {
                self.emit_info(&format!(
                    "shell output ignored: target agent `{target_agent_id}` is not loaded"
                ));
                return None;
            };
            if conv.identity.session_id != finished.session_id {
                self.emit_info(&format!(
                    "shell output ignored: target agent `{target_agent_id}` is not in session `{}`",
                    finished.session_id.as_str(),
                ));
                return None;
            }
            if conv.dispatch.terminating {
                self.emit_info(&format!(
                    "shell output ignored: target agent `{target_agent_id}` is terminating"
                ));
                return None;
            }
            let Some(agent_id) = conv.identity.agent_id.clone() else {
                self.emit_info(&format!(
                    "shell output ignored: target agent `{target_agent_id}` has no durable id"
                ));
                return None;
            };
            return Some((cid, agent_id));
        }

        self.default_shell_output_target_agent()
    }

    pub(super) fn default_shell_output_target_agent(
        &mut self,
    ) -> Option<(AgentId, tau_proto::AgentId)> {
        let mut candidates: Vec<_> = self
            .agent_runtime
            .agent_registry
            .agents
            .iter()
            .filter_map(|(cid, conv)| {
                if conv.identity.session_id != self.session_runtime.current_session_id
                    || !conv.identity.originator.is_user()
                    || conv.dispatch.terminating
                {
                    return None;
                }
                let agent_id = conv.identity.agent_id.clone()?;
                let interaction_order = self
                    .session_runtime
                    .user_interaction_order
                    .get(agent_id.as_str())
                    .copied()
                    .unwrap_or_default();
                Some((interaction_order, cid.clone(), agent_id))
            })
            .collect();

        match candidates.len() {
            0 => {
                if self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .values()
                    .any(|conv| {
                        conv.identity.session_id == self.session_runtime.current_session_id
                            && conv.identity.originator.is_user()
                            && conv.dispatch.terminating
                    })
                {
                    self.emit_info("shell output ignored: user agent is terminating");
                    return None;
                }
                let role = self.config.selected_role.clone();
                let cid = match self.try_create_durable_user_agent(
                    self.session_runtime.current_session_id.clone(),
                    &role,
                ) {
                    Ok(cid) => cid,
                    Err(error) => {
                        self.emit_harness_failure(&format!(
                            "failed to create shell-output target agent: {error}"
                        ));
                        return None;
                    }
                };
                let agent_id = self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(&cid)
                    .and_then(|conv| conv.identity.agent_id.clone())
                    .expect("new user agent has durable id");
                Some((cid, agent_id))
            }
            1 => {
                let (_, cid, agent_id) = candidates.pop().expect("one candidate");
                Some((cid, agent_id))
            }
            _ => {
                candidates
                    .sort_by_key(|(last_user_interaction_time, _, _)| *last_user_interaction_time);
                let (selected_time, cid, agent_id) = candidates.pop().expect("last candidate");
                let Some((previous_time, _, _)) = candidates.last() else {
                    return Some((cid, agent_id));
                };
                if *previous_time < selected_time {
                    return Some((cid, agent_id));
                }
                self.emit_info(
                    "shell output ignored: multiple user agents exist and no explicit target was provided",
                );
                None
            }
        }
    }

    pub(super) fn cancel_remaining_tool_calls(
        &mut self,
        cid: &AgentId,
        remaining_calls: Vec<ToolCallId>,
        background_completion_prompt_mode: BackgroundCompletionPromptMode,
    ) -> bool {
        let remaining: std::collections::HashSet<ToolCallId> =
            remaining_calls.iter().cloned().collect();
        let mut to_cancel: Vec<CancelTarget> = self
            .tool_routing
            .tool_runtime
            .tool_turn
            .cancel_queued_for(cid, &remaining)
            .into_iter()
            .map(|(call_id, tool_name, tool_type)| CancelTarget {
                call_id,
                tool_name,
                tool_type,
                backgrounded: false,
            })
            .collect();
        let retained_terminal_call = self
            .prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .get(cid)
            .and_then(|completion| match completion {
                AgentPublishCompletion::ToolTerminal { call_id, .. } => Some(call_id.clone()),
                _ => None,
            });
        to_cancel.retain(|target| Some(&target.call_id) != retained_terminal_call.as_ref());
        for call_id in remaining_calls {
            if retained_terminal_call.as_ref() == Some(&call_id) {
                continue;
            }
            if to_cancel.iter().any(|target| target.call_id == call_id) {
                continue;
            }
            let Some(tool) = self
                .tool_routing
                .tool_runtime
                .pending_tools
                .get(&call_id)
                .cloned()
            else {
                continue;
            };
            let backgrounded = self
                .tool_routing
                .tool_runtime
                .tool_turn
                .is_backgrounded(&call_id);
            to_cancel.push(CancelTarget {
                call_id,
                tool_name: tool.name,
                tool_type: tool.tool_type,
                backgrounded,
            });
        }

        let mut foreground_call_ids = Vec::new();
        for target in to_cancel {
            self.publish_event(
                Some(crate::harness::harness_connection_id()),
                Event::ToolCancelRequest(tau_proto::ToolCancelRequest {
                    target_call_id: target.call_id.clone(),
                }),
            );
            if self.cancel_target_should_finish_as_background_error(&target) {
                // A background placeholder is already the foreground terminal
                // result. After broadcasting cancellation, synthesize a
                // background error only if no synchronous handler completed the
                // call while processing the request.
                let call_id = target.call_id.clone();
                if matches!(
                    background_completion_prompt_mode,
                    BackgroundCompletionPromptMode::QueuePassive
                ) {
                    self.tool_routing
                        .tool_runtime
                        .suppressed_background_completion_prompts
                        .remove(&call_id);
                }
                self.finish_backgrounded_tool_cancelled_by_harness(
                    target,
                    background_completion_prompt_mode,
                );
                if matches!(
                    background_completion_prompt_mode,
                    BackgroundCompletionPromptMode::QueuePassive
                ) {
                    self.queue_existing_passive_background_completion_prompt(&call_id);
                }
                continue;
            }
            if !self
                .tool_routing
                .tool_runtime
                .pending_tools
                .contains_key(&target.call_id)
            {
                continue;
            }
            self.tool_routing
                .tool_runtime
                .tool_agents
                .entry(target.call_id.clone())
                .or_insert_with(|| cid.clone());
            let call_id = target.call_id;
            let cancelled = ToolCancelled {
                presentation: Default::default(),
                call_id: call_id.clone(),
                tool_name: target.tool_name,
                tool_type: target.tool_type,
                display: None,
            };
            if self.tool_terminal_has_open_durable_owner(cid, &call_id) {
                foreground_call_ids.push(call_id);
                self.observe_tool_terminal(
                    cid,
                    &cancelled.call_id,
                    tau_proto::ToolTerminalCause::LifecycleTeardown,
                );
                self.publish_for_agent(cid, Event::ToolCancelled(cancelled));
            } else {
                self.publish_event(
                    Some(crate::harness::harness_connection_id()),
                    Event::ToolCancelled(cancelled),
                );
                self.record_wait_tool_cancelled(&HashSet::from([call_id.clone()]), None);
                self.finish_tool_call_runtime_state(call_id.as_str());
                self.clear_tool_call_tracking(call_id.as_str());
            }
        }
        retained_terminal_call.is_some()
            || foreground_call_ids
                .iter()
                .any(|call_id| self.tool_routing.tool_runtime.tool_agents.get(call_id) == Some(cid))
    }

    pub(super) fn cancel_target_should_finish_as_background_error(
        &self,
        target: &CancelTarget,
    ) -> bool {
        target.backgrounded
            || self
                .tool_routing
                .tool_runtime
                .tool_turn
                .is_backgrounded(&target.call_id)
    }

    pub(crate) fn is_running_tool_call(&self, target_call_id: &ToolCallId) -> bool {
        self.tool_routing
            .tool_runtime
            .pending_tools
            .contains_key(target_call_id)
    }

    pub(crate) fn is_running_cancellable_tool_call_for(
        &self,
        conversation_id: &AgentId,
        target_call_id: &ToolCallId,
    ) -> bool {
        self.tool_routing
            .tool_runtime
            .pending_tools
            .contains_key(target_call_id)
            && self
                .tool_routing
                .tool_runtime
                .tool_agents
                .get(target_call_id)
                == Some(conversation_id)
    }

    pub(crate) fn is_completed_tool_call_for(
        &self,
        conversation_id: &AgentId,
        target_call_id: &ToolCallId,
    ) -> bool {
        self.tool_routing
            .tool_runtime
            .completed_tool_agents
            .get(target_call_id)
            == Some(conversation_id)
    }

    pub(crate) fn publish_tool_cancel_request_for(
        &mut self,
        conversation_id: &AgentId,
        cancel_call: Option<tau_proto::ToolCallRef>,
        target_call_id: ToolCallId,
    ) -> Result<(), String> {
        if !self.is_running_cancellable_tool_call_for(conversation_id, &target_call_id) {
            return Err("Unknown tool call id".to_owned());
        }
        if let (Some(cancel_call), Some(target_call)) =
            (cancel_call, self.wait_tool_call_ref(&target_call_id))
        {
            let request = tau_proto::ObservationId::random();
            self.tool_routing
                .tool_runtime
                .pending_cancellation_observations
                .entry(target_call_id.clone())
                .or_insert(request);
            self.append_best_effort_observation(
                conversation_id,
                request,
                Event::AgentToolCancellationRequested(tau_proto::AgentToolCancellationRequested {
                    cancel_call,
                    target_call,
                }),
            );
        }
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::ToolCancelRequest(tau_proto::ToolCancelRequest { target_call_id }),
        );
        Ok(())
    }

    pub(crate) fn cancel_start_agent_request(
        &mut self,
        query_id: &str,
        target_call_id: &ToolCallId,
        suppress_background_completion_prompt: bool,
    ) -> Result<(), String> {
        let coordinated_start = self
            .agent_runtime
            .agent_registry
            .start_coordinator
            .operations
            .iter()
            .find_map(|(start_id, operation)| {
                (operation.pending.query.query_id == query_id
                    || operation.pending.query.tool_call_id.as_ref() == Some(target_call_id))
                .then_some((*start_id, operation.phase))
            });
        if let Some((start_id, phase)) = coordinated_start {
            self.emit_info("tool call cancellation request");
            if suppress_background_completion_prompt {
                self.suppress_background_completion_prompt(target_call_id.clone());
            }
            if phase == StartPhase::AwaitAcceptedCommit {
                self.abort_preaccept_start(start_id, tau_proto::AgentStartFailure::Canceled);
            } else {
                self.begin_start_failure(start_id, tau_proto::AgentStartFailure::Canceled);
            }
            return Ok(());
        }
        let mut source_id = None;
        let mut stopped_pending_agent_ids = Vec::new();
        self.agent_runtime
            .agent_registry
            .pending_start_requests
            .retain(|pending| {
                let is_canceled = pending.query.query_id == query_id
                    || pending.query.tool_call_id.as_ref() == Some(target_call_id);
                if is_canceled {
                    source_id = Some(pending.source_id.clone());
                    stopped_pending_agent_ids.push(crate::parse_agent_id(&pending.agent_id));
                }
                !is_canceled
            });
        self.agent_runtime
            .agent_registry
            .stopped_ids
            .extend(stopped_pending_agent_ids);

        if source_id.is_none() {
            source_id = self
                .agent_runtime
                .agent_registry
                .agents
                .iter()
                .find_map(|(_, conv)| {
                    if conv.identity.parent_tool_call_id.as_ref() != Some(target_call_id) {
                        return None;
                    }
                    conv.identity.source_connection.clone()
                });
        }
        let Some(source_id) = source_id else {
            return Err("Tool call is not a running cancellable tool call".to_owned());
        };

        self.emit_info("tool call cancellation request");
        if suppress_background_completion_prompt {
            self.suppress_background_completion_prompt(target_call_id.clone());
        }
        self.cancel_delegate_side_conversation(target_call_id);
        let result = tau_proto::StartAgentResult {
            query_id: query_id.to_owned(),
            text: String::new(),
            error: Some("Tool call canceled".to_owned()),
        };
        if &source_id == harness_connection_id() {
            self.publish_event(
                Some(crate::harness::harness_connection_id()),
                Event::StartAgentResult(result),
            );
        } else {
            let _ = self.runtime_io.bus.send_to(
                &source_id,
                None,
                HarnessOutputMessage::deliver(Event::StartAgentResult(result)),
            );
        }
        Ok(())
    }
}
