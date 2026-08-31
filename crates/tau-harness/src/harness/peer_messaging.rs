//! Owns inter-agent message activation and selected-branch wake bookkeeping.
//!
//! Durable fact publication and external receive commit continuations remain
//! with the publication coordinator.

use super::selected_branch_wake_view::{SelectedBranchWakeProbe, SelectedBranchWakeView};
use super::*;

/// Runtime-only authentication, delivery, and fair routing state for
/// cooperative inter-harness peers.
///
/// Socket admission follows connection lifetime. Callback capabilities,
/// receive acknowledgements, rate admission, auto-start tracking, I/O
/// cancellation, and fairness belong to the active session generation. Session
/// rollover first invalidates callback authority, then resolves parked receive
/// acknowledgements, signals and drains I/O handles, and resets rate and
/// fairness state. Connection teardown removes socket admission and inbound
/// handles. Durable receive publication remains with the publication
/// coordinator, and this state has no independent drop side effects.
#[derive(Default)]
pub(super) struct PeerMessagingState {
    /// Socket clients authenticated for the external-message RPC.
    pub(super) external_message_peers: HashSet<tau_proto::ConnectionId>,
    /// Outbound messages awaiting callback authentication by logical message
    /// id.
    pub(super) pending_external_message_auth:
        HashMap<tau_proto::AgentMessageId, PendingExternalAgentMessageAuth>,
    /// Remote acknowledgements retained until their durable receive commits.
    pub(super) pending_external_receive_acks:
        HashMap<tau_proto::AgentMessageId, PendingExternalReceiveAck>,
    /// Rolling accepted-input timestamps for each concrete peer endpoint.
    pub(super) peer_input_rate: HashMap<tau_proto::AgentId, VecDeque<std::time::Instant>>,
    /// Auto-started endpoints awaiting their first committed peer input.
    pub(super) uncommitted_peer_auto_starts: HashSet<tau_proto::AgentId>,
    /// Weak cancellation handles for outbound peer I/O in the active session.
    pub(super) peer_io_cancellations: Vec<std::sync::Weak<path_std_sync::atomic::AtomicBool>>,
    /// Inbound callback cancellation handles grouped by owning socket.
    pub(super) inbound_peer_io_cancellations:
        HashMap<tau_proto::ConnectionId, Vec<std::sync::Weak<path_std_sync::atomic::AtomicBool>>>,
    /// Monotonic event-loop clock used for fair peer selection.
    pub(super) peer_route_clock: u64,
    /// Most recent selection clock for each concrete peer agent.
    pub(super) peer_last_routed: HashMap<String, u64>,
}

pub(crate) const EXTERNAL_AGENT_MESSAGE_CLIENT_NAME: &str = "tau-external-agent-message";

#[derive(Clone, Debug)]
pub(crate) struct PendingExternalAgentMessageAuth {
    /// Sender-minted bearer capability authorizing one outbound message.
    pub(crate) capability: String,
    /// Sender session active when the outbound message was created.
    pub(crate) sender_session_id: tau_proto::SessionId,
    /// Agent id claimed as sender by the outbound message.
    pub(crate) sender_id: tau_proto::AgentId,
    /// Recipient session targeted by the outbound message.
    pub(crate) recipient_session_id: tau_proto::SessionId,
    /// Typed recipient authority targeted by the outbound message.
    pub(crate) recipient: tau_proto::ExternalAgentMessageRecipient,
    /// Delivery kind authorized by the harness-owned source path.
    pub(crate) kind: tau_proto::AgentMessageKind,
    /// Message body authorized by the harness-owned source path.
    pub(crate) message: String,
}

/// Remote acknowledgement held until the exact receive projection commits.
pub(crate) struct PendingExternalReceiveAck {
    /// Target session generation in which validation and selection occurred.
    pub(crate) session_generation: SessionGeneration,
    /// Concrete recipient selected before the projection was enqueued.
    pub(crate) recipient_id: tau_proto::AgentId,
    /// Typed authority whose semantics must still hold at commit.
    pub(crate) recipient: tau_proto::ExternalAgentMessageRecipient,
    /// Exact immutable projection expected to emerge from interception.
    pub(crate) expected_receive: tau_proto::AgentMessageReceived,
    /// Whether disconnect/rollover canceled this parked continuation.
    pub(crate) canceled: bool,
    /// Whether resolving this delivery created its recipient.
    pub(crate) started: bool,
    /// Whether commit-time bare-route invalidation already consumed its one
    /// permitted reselection.
    pub(crate) reselect_attempted: bool,
    /// Rolling-rate admission to release if this receive never commits.
    pub(crate) rate_admitted_at: std::time::Instant,
    /// Completion released by the receive projection's commit hook.
    pub(crate) completion: PendingPeerReceiveCompletion,
}

/// Live-only completion waiting for one exact peer receive commit.
pub(crate) enum PendingPeerReceiveCompletion {
    /// Remote socket acknowledgement.
    Remote {
        /// Socket connection awaiting the result.
        client_id: tau_proto::ConnectionId,
        /// RPC request id echoed in the result.
        request_id: String,
    },
    /// Current-session sender projection and tool completion.
    Local {
        /// Conversation owning the message tool call.
        conversation_id: AgentId,
        /// Tool call to complete after receive commit.
        call_id: ToolCallId,
        /// Visible tool name.
        tool_name: ToolName,
        /// Declared tool type.
        tool_type: tau_proto::ToolType,
        /// Harness-authored sender id.
        sender_id: tau_proto::AgentId,
        /// Original message body for the sent projection.
        message: String,
    },
}

/// Message recipient state used to report precise tool errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentMessageRecipientStatus {
    /// The recipient is known and can receive messages now.
    Live,
    /// The restored transcript remains inspectable, but its interrupted request
    /// cannot be resumed safely in this harness runtime.
    RestoredUnavailable,
    /// The recipient id was known earlier but its agent has stopped.
    Stopped,
    /// The recipient id has never been observed by this harness.
    Unknown,
}

/// Classifies live activation without carrying or re-rendering message payload.
pub(crate) fn agent_message_activation_class(
    message: &tau_proto::AgentMessageReceived,
) -> Option<crate::agent::AgentMessageActivationClass> {
    use crate::agent::AgentMessageActivationClass::{
        IsolatedWatchNotification, OrdinaryAgentInput,
    };
    match message.kind {
        tau_proto::AgentMessageKind::Message
        | tau_proto::AgentMessageKind::WatchResponse
        | tau_proto::AgentMessageKind::WatchPrompt => Some(OrdinaryAgentInput),
        tau_proto::AgentMessageKind::WatchProviderStatus => message
            .watch_provider_status
            .as_ref()
            .is_some_and(|status| !status.initial)
            .then_some(IsolatedWatchNotification),
        tau_proto::AgentMessageKind::WatchWorkStatus => message
            .watch_work_status
            .as_ref()
            .is_some_and(|status| !status.initial)
            .then_some(IsolatedWatchNotification),
        tau_proto::AgentMessageKind::WatchLongWait => message
            .watch_long_wait
            .is_some()
            .then_some(IsolatedWatchNotification),
        tau_proto::AgentMessageKind::WatchLifecycle => message
            .watch_lifecycle
            .is_some()
            .then_some(IsolatedWatchNotification),
    }
}

impl Harness {
    /// Classify whether a message recipient can receive a hidden
    /// prompt.
    ///
    /// Historical membership restored on cold resume distinguishes an unloaded
    /// stopped recipient from an id that was never known to the session.
    pub(crate) fn agent_message_recipient_status(
        &self,
        recipient_id: &str,
    ) -> AgentMessageRecipientStatus {
        if self
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(recipient_id)
            .is_some_and(|cid| {
                self.agent_runtime
                    .agent_registry
                    .agents
                    .get(cid)
                    .is_some_and(|agent| !agent.dispatch.terminating)
            })
            || self
                .agent_runtime
                .agent_registry
                .pending_start_requests
                .iter()
                .any(|pending| pending.agent_id == recipient_id)
        {
            AgentMessageRecipientStatus::Live
        } else if self
            .agent_runtime
            .agent_registry
            .restored_unavailable
            .contains_key(recipient_id)
        {
            AgentMessageRecipientStatus::RestoredUnavailable
        } else if self
            .agent_runtime
            .agent_registry
            .stopped_ids
            .contains(recipient_id)
            || self
                .agent_runtime
                .agent_registry
                .session_ever_loaded
                .contains(recipient_id)
        {
            AgentMessageRecipientStatus::Stopped
        } else {
            AgentMessageRecipientStatus::Unknown
        }
    }

    pub(super) fn activate_received_agent_message(
        &mut self,
        message: &tau_proto::AgentMessageReceived,
        append_outcome: Option<&tau_core::AgentAppendOutcome>,
    ) {
        let Some(outcome) = append_outcome else {
            return;
        };
        let peer_admission_bytes = self
            .peer_messaging
            .pending_external_receive_acks
            .contains_key(&message.message_id)
            .then_some(message.message.len());
        if let Some(cid) = self
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(message.recipient_id.as_str())
            .cloned()
        {
            if self
                .agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .is_none_or(|agent| agent.dispatch.terminating)
            {
                return;
            }
            if outcome.folded_node_id.is_some()
                && let Some(node_id) = outcome.selected_head_id
                && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
            {
                agent.identity.head = Some(node_id);
                agent.execution.result_dedup.note_head_advanced_to(node_id);
            }
            let Some(activation_class) = agent_message_activation_class(message) else {
                return;
            };
            if activation_class == path_crate_agent::AgentMessageActivationClass::OrdinaryAgentInput
            {
                self.promote_lifecycle_notification_turn(&cid);
            }
            let activation = tau_proto::ObservationId::random();
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
                && !agent.dispatch.pending_message_wakes.iter().any(|wake| {
                    matches!(
                        wake.source,
                        crate::agent::PendingMessageWakeSource::AgentMessageReceived {
                            durable_event_seq,
                            ..
                        } if durable_event_seq == outcome.seq
                    )
                })
            {
                agent
                    .dispatch
                    .pending_message_wakes
                    .push_back(crate::agent::PendingMessageWake {
                        source: path_crate_agent::PendingMessageWakeSource::AgentMessageReceived {
                            durable_event_seq: outcome.seq,
                            activation_class,
                            peer_admission_bytes,
                        },
                        node_id: outcome.folded_node_id,
                        activation_observation: Some(activation),
                        source_observation: Some(outcome.observation_id),
                    });
            }
            let kind = match activation_class {
                path_crate_agent::AgentMessageActivationClass::OrdinaryAgentInput => {
                    tau_proto::ActivationKind::AgentMessage
                }
                path_crate_agent::AgentMessageActivationClass::IsolatedWatchNotification => {
                    tau_proto::ActivationKind::WatchNotification
                }
            };
            self.append_activation_queued(
                &cid,
                activation,
                kind,
                Some(outcome.observation_id),
                None,
            );
            self.activate_waits_for(&cid, activation);
            self.preempt_queued_tool_calls_for_message_received(&cid);
            if self.terminalize_uncertain_marked_owner_for_live_activation(&cid) {
                return;
            }
            self.try_advance_queue();
            return;
        }
        let Some(activation_class) = agent_message_activation_class(message) else {
            return;
        };
        if let Some(pending) = self
            .agent_runtime
            .agent_registry
            .pending_start_requests
            .iter_mut()
            .find(|pending| pending.agent_id == message.recipient_id.as_str())
            && !pending.pending_agent_message_wakes.iter().any(|wake| {
                matches!(
                    wake.source,
                    crate::agent::PendingMessageWakeSource::AgentMessageReceived {
                        durable_event_seq,
                        ..
                    } if durable_event_seq == outcome.seq
                )
            })
        {
            pending
                .pending_agent_message_wakes
                .push_back(crate::agent::PendingMessageWake {
                    source: path_crate_agent::PendingMessageWakeSource::AgentMessageReceived {
                        durable_event_seq: outcome.seq,
                        activation_class,
                        peer_admission_bytes,
                    },
                    node_id: outcome.folded_node_id,
                    activation_observation: Some(tau_proto::ObservationId::random()),
                    source_observation: Some(outcome.observation_id),
                });
        }
    }

    /// Resolve wakes buffered behind an open provider tool round.
    pub(super) fn resolve_materialized_message_wakes(&mut self, cid: &AgentId) {
        let Some(agent_id) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
        else {
            return;
        };
        let Some(tree) = self.session_runtime.agent_store.agent(agent_id) else {
            return;
        };
        let resolved_message_inputs: HashMap<_, _> = tree
            .nodes()
            .iter()
            .filter_map(|node| match &node.entry {
                tau_core::AgentEntry::MessageFact {
                    durable_event_seq, ..
                } => Some((*durable_event_seq, node.id)),
                tau_core::AgentEntry::AgentMessage {
                    durable_event_seq, ..
                } => Some((*durable_event_seq, node.id)),
                _ => None,
            })
            .collect();
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            for wake in &mut agent.dispatch.pending_message_wakes {
                if wake.node_id.is_none() {
                    wake.node_id = resolved_message_inputs
                        .get(&wake.source.durable_event_seq())
                        .copied();
                }
            }
        }
    }

    /// Acknowledge only materialized wakes on the checkpointed branch through
    /// its head.
    pub(super) fn acknowledge_message_wakes_through(
        &mut self,
        cid: &AgentId,
        through: tau_proto::AgentHead,
    ) {
        self.resolve_materialized_message_wakes(cid);
        let branch: HashSet<_> = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
            .map(|tree| {
                tree.branch_node_ids_from(through.as_option())
                    .into_iter()
                    .collect()
            })
            .unwrap_or_default();
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            agent.dispatch.pending_replay_activation = false;
            agent.dispatch.pending_message_wakes.retain(|wake| {
                wake.node_id
                    .is_none_or(|node_id| !branch.contains(&node_id))
            });
        }
    }

    /// Returns whether at least one materialized wake belongs to the selected
    /// branch. Off-branch wakes remain dormant until navigation reselects them.
    pub(crate) fn has_ready_message_wake_on_selected_branch(&self, cid: &AgentId) -> bool {
        self.selected_branch_wake_view(cid)
            .is_some_and(|view| view.has_ready_wake())
    }

    /// Probes readiness and reports exact transient branch/wake work.
    pub(crate) fn selected_branch_wake_probe(
        &self,
        cid: &AgentId,
    ) -> Option<SelectedBranchWakeProbe> {
        let agent = self.agent_runtime.agent_registry.agents.get(cid)?;
        let tree = agent
            .identity
            .agent_id
            .as_deref()
            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))?;
        Some(SelectedBranchWakeView::probe_ready(
            tree,
            agent.identity.head,
            &agent.dispatch.pending_message_wakes,
        ))
    }

    /// Returns whether accepted message input should interrupt a newly
    /// registered wait.
    ///
    /// Unmaterialized wakes remain globally actionable while tool adjacency is
    /// open. Once materialized, only wakes on the selected branch interrupt;
    /// sibling-branch wakes stay dormant until navigation reselects them.
    pub(crate) fn has_wait_preempting_message_wake(&self, cid: &AgentId) -> bool {
        self.agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|agent| {
                agent
                    .dispatch
                    .pending_message_wakes
                    .iter()
                    .any(|wake| wake.node_id.is_none())
            })
            || self.has_ready_message_wake_on_selected_branch(cid)
    }

    /// Builds one immutable projection of pending wakes onto the selected
    /// branch.
    pub(crate) fn selected_branch_wake_view(
        &self,
        cid: &AgentId,
    ) -> Option<SelectedBranchWakeView> {
        let agent = self.agent_runtime.agent_registry.agents.get(cid)?;
        let tree = self
            .session_runtime
            .agent_store
            .agent(agent.identity.agent_id.as_deref()?)?;
        Some(SelectedBranchWakeView::new(
            tree,
            agent.identity.head,
            &agent.dispatch.pending_message_wakes,
        ))
    }

    pub(super) fn preempt_queued_tool_calls_for_message_received(&mut self, cid: &AgentId) {
        let Some(remaining_calls) =
            self.agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|conv| match &conv.turn.turn_state {
                    AgentTurnState::ToolsRunning { remaining_calls } => {
                        Some(remaining_calls.ordered_remaining())
                    }
                    _ => None,
                })
        else {
            return;
        };
        if self
            .tool_routing
            .tool_runtime
            .tool_turn
            .any_in_flight_for(cid)
            || self
                .tool_routing
                .tool_runtime
                .tool_turn
                .backgrounded_calls_for(cid)
                .is_empty()
        {
            return;
        }

        let remaining: std::collections::HashSet<ToolCallId> =
            remaining_calls.iter().cloned().collect();
        let cancelled = self
            .tool_routing
            .tool_runtime
            .tool_turn
            .cancel_queued_for(cid, &remaining);
        if cancelled.len() != remaining_calls.len() {
            return;
        }
        for (call_id, tool_name, tool_type) in cancelled {
            self.tool_routing
                .tool_runtime
                .tool_agents
                .entry(call_id.clone())
                .or_insert_with(|| cid.clone());
            self.publish_for_agent(
                cid,
                Event::ToolCancelled(ToolCancelled {
                    presentation: Default::default(),
                    call_id,
                    tool_name,
                    tool_type,
                    display: None,
                }),
            );
        }
    }

    pub(super) fn has_pending_agent_message_wake(&self, cid: &AgentId) -> bool {
        self.agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|conv| {
                conv.dispatch.pending_message_wakes.iter().any(|wake| {
                    matches!(
                        wake.source,
                        crate::agent::PendingMessageWakeSource::AgentMessageReceived { .. }
                    )
                })
            })
    }
}
