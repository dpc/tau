//! Bounded runtime ownership for independently committed side-agent startup
//! phases.

use super::agent_registry::PendingStartAgentRequest;
use super::interception::{
    PeerPublicationContext, PublicationOutcomeOwners, RetainedStartTerminal,
};
use super::*;

/// Maximum nonterminal side-agent starts retained by one harness runtime.
pub(super) const MAX_START_OPERATIONS: usize = 64;
/// Maximum aggregate encoded startup payload retained by one harness runtime.
pub(super) const MAX_START_RETAINED_BYTES: usize = 4 * 1024 * 1024;
/// Maximum configured-extension request correlation length.
pub(super) const MAX_START_QUERY_ID_BYTES: usize = 128;

/// One runtime phase between canonical startup facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StartPhase {
    /// Acceptance has not committed and no startup obligation is externally
    /// visible.
    AwaitAcceptedCommit,
    /// Acceptance committed; the immutable creation fact is pending.
    AwaitStartedCommit,
    /// Creation committed; session membership is pending.
    AwaitLoadedCommit,
    /// Membership committed; the initial prompt is pending.
    AwaitPromptCommit,
    /// The prompt committed; the inference checkpoint is pending.
    AwaitDispatchCommit,
    /// One compact terminal owns all remaining startup cleanup.
    ClosingFailure,
}

impl StartPhase {
    /// Return the compact public phase represented by this runtime state.
    pub(super) const fn public(self) -> tau_proto::AgentStartPhase {
        match self {
            Self::AwaitAcceptedCommit | Self::AwaitStartedCommit => {
                tau_proto::AgentStartPhase::AgentStarted
            }
            Self::AwaitLoadedCommit => tau_proto::AgentStartPhase::SessionAgentLoaded,
            Self::AwaitPromptCommit => tau_proto::AgentStartPhase::AgentPromptSubmitted,
            Self::AwaitDispatchCommit | Self::ClosingFailure => {
                tau_proto::AgentStartPhase::AgentInferenceDispatchStarted
            }
        }
    }
}

/// Immutable one-event publication ownership captured at enqueue time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StartPhaseOwner {
    /// Startup operation whose phase this publication resolves.
    pub(super) start_id: tau_proto::StartOperationId,
    /// Runtime phase that must still own the outcome.
    pub(super) expected_phase: StartPhase,
    /// Exact event family allowed to consume this owner.
    pub(super) expected_event: tau_proto::EventName,
}

/// Runtime snapshot retained only until startup succeeds or terminalizes.
#[derive(Clone, Debug)]
pub(super) struct StartOperation {
    /// Stable duplicate/rebind identity.
    pub(super) request_key: (tau_proto::ExtensionName, String),
    /// Current requester route for directed projections.
    pub(super) source_id: tau_proto::ConnectionId,
    /// Session generation captured before acceptance.
    pub(super) session_generation: SessionGeneration,
    /// Complete validated startup snapshot.
    pub(super) pending: PendingStartAgentRequest,
    /// Exact bytes charged against the aggregate runtime bound.
    pub(super) retained_bytes: usize,
    /// Current nonterminal phase.
    pub(super) phase: StartPhase,
    /// Canonical acceptance cached after commit for duplicate replay.
    pub(super) accepted: Option<tau_proto::StartAgentAccepted>,
    /// Whether session membership committed and needs unload on failure.
    pub(super) membership_committed: bool,
    /// Whether this start uses the shared semantic-persistence owner.
    pub(super) uses_persistence_owner: bool,
    /// Exact shared persistence-owner epoch captured at reservation.
    pub(super) persistence_owner_epoch: Option<u64>,
    /// Exact prepared agent-stream generation, once creation commits.
    pub(super) persistence_generation: Option<u64>,
    /// Exact committed initial-prompt head that the startup checkpoint must
    /// cover.
    pub(super) startup_through: Option<tau_proto::AgentHead>,
}

/// Three-index bounded runtime coordinator for side-agent startup.
pub(super) struct StartCoordinator {
    /// Operations keyed by run-unique correlation.
    pub(super) operations: HashMap<tau_proto::StartOperationId, StartOperation>,
    /// Stable extension/query duplicate index.
    pub(super) requests: HashMap<(tau_proto::ExtensionName, String), tau_proto::StartOperationId>,
    /// Reserved agent identity to startup correlation.
    pub(super) agents: HashMap<tau_proto::AgentId, tau_proto::StartOperationId>,
    /// Aggregate encoded startup payload retained by live operations.
    pub(super) retained_bytes: usize,
    /// Checked monotonic operation identity cursor for this harness runtime.
    next_id: u64,
}

impl StartCoordinator {
    /// Create an empty coordinator with the first positive runtime identity.
    pub(super) fn new() -> Self {
        Self {
            operations: HashMap::new(),
            requests: HashMap::new(),
            agents: HashMap::new(),
            retained_bytes: 0,
            next_id: 1,
        }
    }

    /// Mint one operation identity that cannot repeat in this runtime.
    fn mint_id(&mut self) -> tau_proto::StartOperationId {
        let id = tau_proto::StartOperationId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("start operation identity space exhausted");
        id
    }
    /// Measure only contentful startup data retained across event boundaries.
    pub(super) fn retained_payload_bytes(
        pending: &PendingStartAgentRequest,
    ) -> Result<usize, String> {
        let query = &pending.query;
        let retained = (
            &query.instruction,
            &query.trusted_internal_spans,
            &query.input_stats,
            &query.tool_call_id,
            &query.task_name,
        );
        let mut encoded = Vec::new();
        ciborium::into_writer(&retained, &mut encoded)
            .map_err(|error| format!("failed to measure retained startup payload: {error}"))?;
        Ok(encoded.len())
    }

    /// Return whether one operation and its retained payload fit both runtime
    /// bounds.
    pub(super) fn can_insert(&self, retained_bytes: usize) -> bool {
        self.operations.len() < MAX_START_OPERATIONS
            && self
                .retained_bytes
                .checked_add(retained_bytes)
                .is_some_and(|bytes| bytes <= MAX_START_RETAINED_BYTES)
    }

    /// Remove one terminal operation and all indexes, returning its snapshot.
    pub(super) fn remove(
        &mut self,
        start_id: tau_proto::StartOperationId,
    ) -> Option<StartOperation> {
        let operation = self.operations.remove(&start_id)?;
        self.requests.remove(&operation.request_key);
        self.agents.remove(&operation.pending.cid);
        self.retained_bytes = self
            .retained_bytes
            .checked_sub(operation.retained_bytes)
            .expect("start retained-byte accounting underflow");
        Some(operation)
    }
}

impl Harness {
    /// Install one synthetic operation without publication for deterministic
    /// persistence-targeting and phase-cut oracles.
    #[cfg(test)]
    pub(super) fn insert_start_operation_for_test(
        &mut self,
        pending: PendingStartAgentRequest,
        phase: StartPhase,
        uses_persistence_owner: bool,
        persistence_owner_epoch: Option<u64>,
        persistence_generation: Option<u64>,
    ) -> tau_proto::StartOperationId {
        let retained_bytes =
            StartCoordinator::retained_payload_bytes(&pending).expect("test payload encodes");
        assert!(
            self.agent_runtime
                .agent_registry
                .start_coordinator
                .can_insert(retained_bytes)
        );
        let start_id = self
            .agent_runtime
            .agent_registry
            .start_coordinator
            .mint_id();
        let request_key = (
            pending.extension_name.clone(),
            pending.query.query_id.clone(),
        );
        let agent_id = pending.cid.clone();
        let operation = StartOperation {
            request_key: request_key.clone(),
            source_id: pending.source_id.clone(),
            session_generation: self.session_runtime.current_session_generation,
            pending,
            retained_bytes,
            phase,
            accepted: None,
            membership_committed: matches!(
                phase,
                StartPhase::AwaitPromptCommit | StartPhase::AwaitDispatchCommit
            ),
            uses_persistence_owner,
            persistence_owner_epoch,
            persistence_generation,
            startup_through: None,
        };
        let coordinator = &mut self.agent_runtime.agent_registry.start_coordinator;
        coordinator.retained_bytes += retained_bytes;
        coordinator.requests.insert(request_key, start_id);
        coordinator.agents.insert(agent_id, start_id);
        coordinator.operations.insert(start_id, operation);
        start_id
    }

    /// Reserve one bounded operation and enqueue its sole acceptance
    /// occurrence.
    pub(super) fn begin_start_operation(&mut self, pending: PendingStartAgentRequest) {
        let query_id = pending.query.query_id.clone();
        let source_id = pending.source_id.clone();
        if query_id.len() > MAX_START_QUERY_ID_BYTES {
            self.fail_start_agent_request(
                &source_id,
                query_id,
                format!("start query id exceeds the {MAX_START_QUERY_ID_BYTES}-byte limit"),
            );
            return;
        }
        let retained_bytes = match StartCoordinator::retained_payload_bytes(&pending) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.fail_start_agent_request(&source_id, query_id, error);
                return;
            }
        };
        if !self
            .agent_runtime
            .agent_registry
            .start_coordinator
            .can_insert(retained_bytes)
        {
            self.fail_start_agent_request(
                &source_id,
                query_id,
                "too many or too much retained side-agent startup work".to_owned(),
            );
            return;
        }
        let start_id = self
            .agent_runtime
            .agent_registry
            .start_coordinator
            .mint_id();
        let agent_id = pending.cid.clone();
        let uses_persistence_owner = !self.session_runtime.storage_mode.is_memory_only()
            && !pending.persistence.is_ephemeral();
        let persistence_owner_epoch = uses_persistence_owner
            .then(|| {
                self.session_runtime
                    .persistence_owner
                    .as_ref()
                    .map(|owner| owner.owner_epoch())
            })
            .flatten();
        let request_key = (
            pending.extension_name.clone(),
            pending.query.query_id.clone(),
        );
        let operation = StartOperation {
            request_key: request_key.clone(),
            source_id,
            session_generation: self.session_runtime.current_session_generation,
            pending,
            retained_bytes,
            phase: StartPhase::AwaitAcceptedCommit,
            accepted: None,
            membership_committed: false,
            uses_persistence_owner,
            persistence_owner_epoch,
            persistence_generation: None,
            startup_through: None,
        };
        let coordinator = &mut self.agent_runtime.agent_registry.start_coordinator;
        coordinator.retained_bytes += retained_bytes;
        coordinator.requests.insert(request_key, start_id);
        coordinator.agents.insert(agent_id.clone(), start_id);
        coordinator.operations.insert(start_id, operation);
        let accepted = Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
            start_id,
            query_id: self
                .agent_runtime
                .agent_registry
                .start_coordinator
                .operations[&start_id]
                .pending
                .query
                .query_id
                .clone(),
            agent_id,
        });
        self.enqueue_start_phase(
            accepted,
            false,
            false,
            StartPhaseOwner {
                start_id,
                expected_phase: StartPhase::AwaitAcceptedCommit,
                expected_event: tau_proto::EventName::AGENT_START_ACCEPTED,
            },
        );
    }

    /// Correlate an enqueued startup event with the operation that currently
    /// owns it.
    pub(super) fn start_phase_owner_for_event(&self, event: &Event) -> Option<StartPhaseOwner> {
        let coordinator = &self.agent_runtime.agent_registry.start_coordinator;
        let (start_id, expected_phase) = match event {
            Event::StartAgentAccepted(accepted) => {
                (accepted.start_id, StartPhase::AwaitAcceptedCommit)
            }
            Event::AgentInferenceDispatchStarted(started) => (
                *coordinator.agents.get(&started.agent_id)?,
                StartPhase::AwaitDispatchCommit,
            ),
            Event::AgentStartFailed(failed) => (failed.start_id, StartPhase::ClosingFailure),
            _ => return None,
        };
        coordinator
            .operations
            .get(&start_id)
            .filter(|operation| {
                operation.phase == expected_phase
                    && operation.session_generation
                        == self.session_runtime.current_session_generation
                    && (!matches!(event, Event::AgentInferenceDispatchStarted(_))
                        || matches!(
                            event,
                            Event::AgentInferenceDispatchStarted(started)
                                if operation.startup_through.is_some_and(|startup_through| {
                                    self.session_runtime
                                        .agent_store
                                        .agent(started.agent_id.as_str())
                                        .is_some_and(|tree| {
                                            tree.contains_head_ancestry(
                                                startup_through,
                                                started.through,
                                            )
                                        })
                                })
                        ))
            })
            .map(|_| StartPhaseOwner {
                start_id,
                expected_phase,
                expected_event: event.name(),
            })
    }

    /// Close an accepted startup whose initial dispatch cannot be admitted.
    pub(super) fn fail_start_dispatch_for_agent(
        &mut self,
        cid: &AgentId,
        reason: tau_proto::AgentStartFailure,
    ) -> bool {
        let start_id = self
            .agent_runtime
            .agent_registry
            .start_coordinator
            .agents
            .get(cid)
            .copied()
            .filter(|start_id| {
                self.agent_runtime
                    .agent_registry
                    .start_coordinator
                    .operations
                    .get(start_id)
                    .is_some_and(|operation| operation.phase == StartPhase::AwaitDispatchCommit)
            });
        let Some(start_id) = start_id else {
            return false;
        };
        self.begin_start_failure(start_id, reason);
        true
    }

    /// Consume one canonical committed outcome and emit only its next phase.
    pub(super) fn commit_start_phase(
        &mut self,
        owner: StartPhaseOwner,
        event: &Event,
        append_outcome: Option<&tau_core::AgentAppendOutcome>,
    ) -> Option<(Event, StartPhaseOwner)> {
        if event.name() != owner.expected_event
            || !self
                .agent_runtime
                .agent_registry
                .start_coordinator
                .operations
                .get(&owner.start_id)
                .is_some_and(|operation| operation.phase == owner.expected_phase)
        {
            return None;
        }
        match event {
            Event::StartAgentAccepted(accepted) => {
                let source = {
                    let operation = self
                        .agent_runtime
                        .agent_registry
                        .start_coordinator
                        .operations
                        .get_mut(&owner.start_id)
                        .expect("matched start operation");
                    operation.phase = StartPhase::AwaitStartedCommit;
                    operation.accepted = Some(accepted.clone());
                    operation.source_id.clone()
                };
                let _ = self.runtime_io.bus.send_to(
                    &source,
                    None,
                    HarnessOutputMessage::deliver(Event::StartAgentAccepted(accepted.clone())),
                );
                let pending = self
                    .agent_runtime
                    .agent_registry
                    .start_coordinator
                    .operations[&owner.start_id]
                    .pending
                    .clone();
                if pending.query.tool_call_id.is_some()
                    && &pending.source_id == crate::harness::harness_connection_id()
                {
                    self.agent_runtime
                        .agent_registry
                        .pending_builtin_delegates
                        .insert(pending.query.query_id.clone(), pending.cid.clone());
                }
                if pending.persistence.is_ephemeral()
                    && let Err(error) = self
                        .session_runtime
                        .agent_store
                        .mark_agent_ephemeral(&pending.agent_id)
                {
                    self.emit_harness_failure(&format!(
                        "failed to mark accepted child agent `{}` ephemeral: {error}",
                        pending.agent_id
                    ));
                    self.begin_start_failure(
                        owner.start_id,
                        tau_proto::AgentStartFailure::CreationWorker,
                    );
                    return None;
                }
                let parent_agent = pending.parent_cid.as_ref().and_then(|parent| {
                    self.agent_runtime
                        .agent_registry
                        .agents
                        .get(parent)
                        .and_then(|agent| agent.identity.agent_id.clone())
                });
                let creator = if pending.query.tool_call_id.is_some() {
                    pending
                        .parent_cid
                        .as_ref()
                        .and_then(|parent| self.agent_runtime.agent_registry.agents.get(parent))
                        .and_then(|parent| {
                            Some(tau_proto::AgentCreator::Agent {
                                session_id: parent.identity.session_id.clone(),
                                agent_id: parent.identity.agent_id.clone()?,
                            })
                        })
                } else {
                    let (name, instance_id) = self.extension_action_owner(&pending.source_id);
                    Some(tau_proto::AgentCreator::Extension { name, instance_id })
                };
                let display_name = if pending.query.tool_call_id.is_some() {
                    normalize_display_name(pending.query.task_name.as_deref()).or_else(|| {
                        self.display_name_for_new_agent(
                            &pending.agent_id,
                            &pending.role,
                            pending.query.task_name.as_deref(),
                        )
                    })
                } else {
                    self.display_name_for_new_agent(
                        &pending.agent_id,
                        &pending.role,
                        pending.query.task_name.as_deref(),
                    )
                };
                let started = Event::AgentStarted(tau_proto::AgentStarted {
                    agent_id: pending.cid.clone(),
                    creator,
                    parent_agent,
                    role: pending.role.clone(),
                    display_name,
                    metadata: Vec::new(),
                    ephemeral: self.session_runtime.storage_mode.is_memory_only()
                        || pending.persistence.is_ephemeral(),
                });
                return Some((
                    started,
                    StartPhaseOwner {
                        start_id: owner.start_id,
                        expected_phase: StartPhase::AwaitStartedCommit,
                        expected_event: tau_proto::EventName::AGENT_STARTED,
                    },
                ));
            }
            Event::AgentStarted(_) => {
                let persistence_generation = self
                    .session_runtime
                    .agent_store
                    .managed_persistence_leases()
                    .into_iter()
                    .find(|lease| {
                        matches!(
                            lease.stream(),
                            tau_core::StreamIdentity::Agent(agent_id)
                                if agent_id == &self
                                    .agent_runtime
                                    .agent_registry
                                    .start_coordinator
                                    .operations[&owner.start_id]
                                    .pending
                                    .cid
                        )
                    })
                    .map(|lease| lease.generation().get());
                let pending = {
                    let operation = self
                        .agent_runtime
                        .agent_registry
                        .start_coordinator
                        .operations
                        .get_mut(&owner.start_id)
                        .expect("matched start operation");
                    operation.phase = StartPhase::AwaitLoadedCommit;
                    operation.persistence_generation = persistence_generation;
                    let buffered_wakes =
                        std::mem::take(&mut operation.pending.pending_agent_message_wakes);
                    let mut pending = operation.pending.clone();
                    pending.pending_agent_message_wakes = buffered_wakes;
                    pending
                };
                if let Err(error) = self.start_agent_request_inner(
                    pending,
                    false,
                    false,
                    true,
                    Some(owner.start_id),
                ) {
                    self.emit_harness_failure(&format!(
                        "failed to install committed side agent: {error}"
                    ));
                    self.begin_start_failure(
                        owner.start_id,
                        tau_proto::AgentStartFailure::CreationWorker,
                    );
                }
            }
            Event::SessionAgentLoaded(_) => {
                let (cid, instruction, spans) = {
                    let operation = self
                        .agent_runtime
                        .agent_registry
                        .start_coordinator
                        .operations
                        .get_mut(&owner.start_id)
                        .expect("matched start operation");
                    operation.phase = StartPhase::AwaitPromptCommit;
                    operation.membership_committed = true;
                    (
                        operation.pending.cid.clone(),
                        operation.pending.query.instruction.clone(),
                        operation.pending.query.trusted_internal_spans.clone(),
                    )
                };
                let mut prompt = PendingPrompt::user(instruction);
                prompt.trusted_internal_spans = spans;
                prompt.start_operation_id = Some(owner.start_id);
                if let Err(error) = self.publish_pending_prompt_for_agent(&cid, prompt) {
                    self.emit_harness_failure(&format!(
                        "failed to publish accepted side-agent prompt: {error}"
                    ));
                    self.begin_start_failure(
                        owner.start_id,
                        tau_proto::AgentStartFailure::StorageAdmission,
                    );
                }
                self.drain_publish_idle_dispatches();
            }
            Event::AgentPromptSubmitted(_) => {
                let coordinator = &mut self.agent_runtime.agent_registry.start_coordinator;
                let operation = coordinator
                    .operations
                    .get_mut(&owner.start_id)
                    .expect("matched start operation");
                operation.phase = StartPhase::AwaitDispatchCommit;
                operation.startup_through = append_outcome
                    .and_then(|outcome| outcome.folded_node_id)
                    .map(tau_proto::AgentHead::Node);
                let released = operation.retained_bytes;
                operation.retained_bytes = 0;
                operation.pending.query.instruction = String::new();
                operation.pending.query.trusted_internal_spans = Vec::new();
                operation.pending.query.task_name = None;
                operation.pending.query.tool_call_id = None;
                operation.pending.query.input_stats = Default::default();
                coordinator.retained_bytes = coordinator
                    .retained_bytes
                    .checked_sub(released)
                    .expect("start retained-byte accounting underflow");
            }
            Event::AgentInferenceDispatchStarted(_) => {
                self.agent_runtime
                    .agent_registry
                    .start_coordinator
                    .remove(owner.start_id);
            }
            Event::AgentStartFailed(failed) => {
                let operation = self
                    .agent_runtime
                    .agent_registry
                    .start_coordinator
                    .remove(owner.start_id)?;
                self.fail_start_agent_request(
                    &operation.source_id,
                    operation.pending.query.query_id,
                    format!(
                        "failed to start agent `{}` ({:?})",
                        failed.agent_id, failed.reason
                    ),
                );
                self.retire_agent_watch_endpoint(
                    &failed.agent_id,
                    Some(tau_proto::AgentWatchLifecycleReason::UnexpectedUnload),
                );
                self.cleanup_failed_start_runtime(
                    &operation.pending.cid,
                    &failed.agent_id,
                    operation.membership_committed,
                );
                if operation.membership_committed {
                    self.publish_event(
                        None,
                        Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
                            session_id: self.session_runtime.current_session_id.clone(),
                            agent_id: failed.agent_id.clone(),
                        }),
                    );
                }
            }
            _ => {}
        }
        None
    }

    /// Remove runtime-only installation state when creation committed but
    /// session membership did not.
    fn cleanup_failed_start_runtime(
        &mut self,
        cid: &AgentId,
        agent_id: &AgentId,
        membership_committed: bool,
    ) {
        self.agent_runtime
            .agent_registry
            .session_loaded
            .remove(agent_id);
        if !membership_committed {
            self.agent_runtime
                .agent_registry
                .session_ever_loaded
                .remove(agent_id);
        }
        if !membership_committed {
            self.agent_runtime
                .agent_registry
                .roster_loaded
                .remove(agent_id);
        }
        self.agent_runtime
            .agent_registry
            .navigation_modes
            .remove(agent_id);
        self.prompt_coordination
            .context_discovery
            .pending_agents
            .remove(agent_id);
        self.prompt_coordination
            .context_discovery
            .frozen_agents
            .remove(agent_id);
        self.prompt_coordination
            .context_discovery
            .initialized_agent_context
            .remove(agent_id);
        self.agent_runtime
            .agent_registry
            .agent_routes
            .remove(agent_id);
        self.agent_runtime
            .agent_registry
            .stopped_ids
            .insert(agent_id.clone());
        self.runtime_io
            .publication
            .idle_dispatches
            .retain(|dispatch| &dispatch.cid != cid);
        self.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .remove(cid);
        self.runtime_io
            .publication
            .capacity_rejected_activations
            .remove(cid);
        self.agent_runtime.agent_registry.agents.remove(cid);
        self.cancel_agent_synchronized_publications(cid);
    }

    /// Consume one rejected phase outcome without creating a second owner.
    pub(super) fn reject_start_phase(
        &mut self,
        owner: StartPhaseOwner,
        reason: tau_proto::AgentStartFailure,
    ) {
        let phase_matches = self
            .agent_runtime
            .agent_registry
            .start_coordinator
            .operations
            .get(&owner.start_id)
            .is_some_and(|operation| operation.phase == owner.expected_phase);
        if !phase_matches {
            return;
        }
        if owner.expected_phase == StartPhase::AwaitAcceptedCommit {
            if let Some(operation) = self
                .agent_runtime
                .agent_registry
                .start_coordinator
                .remove(owner.start_id)
            {
                self.fail_start_agent_request(
                    &operation.source_id,
                    operation.pending.query.query_id,
                    "start acceptance was rejected".to_owned(),
                );
            }
        } else {
            self.begin_start_failure(owner.start_id, reason);
        }
    }

    /// Publish at most one compact terminal for an accepted startup.
    pub(super) fn begin_start_failure(
        &mut self,
        start_id: tau_proto::StartOperationId,
        reason: tau_proto::AgentStartFailure,
    ) {
        let (agent_id, phase) = {
            let Some(operation) = self
                .agent_runtime
                .agent_registry
                .start_coordinator
                .operations
                .get_mut(&start_id)
            else {
                return;
            };
            if matches!(
                operation.phase,
                StartPhase::AwaitAcceptedCommit | StartPhase::ClosingFailure
            ) {
                return;
            }
            let phase = operation.phase.public();
            operation.phase = StartPhase::ClosingFailure;
            (operation.pending.cid.clone(), phase)
        };
        let _ = self.cancel_start_phase_publication(start_id);
        let event = Event::AgentStartFailed(tau_proto::AgentStartFailed {
            start_id,
            agent_id,
            phase,
            reason,
        });
        let owner = StartPhaseOwner {
            start_id,
            expected_phase: StartPhase::ClosingFailure,
            expected_event: tau_proto::EventName::AGENT_START_FAILED,
        };
        self.enqueue_start_phase(event, false, true, owner);
    }

    /// Retain one already-interceptor-approved compact failure after its
    /// live-log admission rejects. The operation remains in
    /// `ClosingFailure`.
    pub(super) fn retain_start_terminal(&mut self, event: Event, owner: StartPhaseOwner) {
        let previous = self
            .runtime_io
            .publication
            .retained_start_terminals
            .insert(owner.start_id, RetainedStartTerminal { event, owner });
        if previous.is_some() {
            // ast-grep-ignore: debug-assert-expression-must-not-mutate
            debug_assert!(false, "startup terminal retained twice");
        }
    }

    /// Deterministic live-log admission control at the real post-interception
    /// publication boundary.
    pub(super) fn start_terminal_live_admission_available(&mut self) -> bool {
        #[cfg(test)]
        if std::mem::take(
            &mut self
                .runtime_io
                .publication
                .reject_next_start_terminal_live_admission_for_test,
        ) {
            return false;
        }
        true
    }

    /// Retry compact startup terminals retained by temporary live publication
    /// pressure without re-running interception or creating another owner.
    pub(super) fn retry_retained_start_terminals(&mut self) {
        let retained = std::mem::take(&mut self.runtime_io.publication.retained_start_terminals);
        for (start_id, terminal) in retained {
            if self
                .agent_runtime
                .agent_registry
                .start_coordinator
                .operations
                .get(&start_id)
                .is_some_and(|operation| operation.phase == StartPhase::ClosingFailure)
            {
                self.commit_event(
                    Some(crate::harness::harness_connection_id()),
                    &PeerPublicationContext::default(),
                    terminal.event,
                    false,
                    None,
                    PublicationOutcomeOwners {
                        prompt_acceptance: None,
                        start: Some(terminal.owner),
                    },
                );
            }
        }
    }

    /// Cancel the still-uncommitted acceptance owner before removing its
    /// private reservation. If acceptance already committed, close the newly
    /// visible obligation instead.
    pub(super) fn abort_preaccept_start(
        &mut self,
        start_id: tau_proto::StartOperationId,
        reason: tau_proto::AgentStartFailure,
    ) {
        let owner = self
            .cancel_start_phase_publication(start_id)
            .unwrap_or(StartPhaseOwner {
                start_id,
                expected_phase: StartPhase::AwaitAcceptedCommit,
                expected_event: tau_proto::EventName::AGENT_START_ACCEPTED,
            });
        if self
            .agent_runtime
            .agent_registry
            .start_coordinator
            .operations
            .get(&start_id)
            .is_some_and(|operation| operation.phase == StartPhase::AwaitAcceptedCommit)
        {
            self.reject_start_phase(owner, reason);
        } else {
            self.begin_start_failure(start_id, reason);
        }
    }

    /// Best-effort terminalize every operation owned by the current session.
    pub(super) fn fail_start_operations_for_session_shutdown(&mut self) {
        let starts = self
            .agent_runtime
            .agent_registry
            .start_coordinator
            .operations
            .iter()
            .map(|(start_id, operation)| (*start_id, operation.phase))
            .collect::<Vec<_>>();
        for (start_id, phase) in starts {
            if phase == StartPhase::AwaitAcceptedCommit {
                self.abort_preaccept_start(start_id, tau_proto::AgentStartFailure::SessionStopped);
            } else {
                self.begin_start_failure(start_id, tau_proto::AgentStartFailure::SessionStopped);
            }
        }
    }
}
