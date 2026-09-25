//! Canonical workdir mutations and source-local discovery replacement.

use super::*;

/// A missing or disconnected scanner must not permanently block model recovery.
const DISCOVERY_REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

impl Harness {
    /// Follow the supervisor's configured-instance replacement mapping, never a
    /// peer-claimed key or name, while keeping retained discovery source-local.
    pub(super) fn rebind_workdir_discovery_source(
        &mut self,
        previous: &tau_proto::ConnectionId,
        replacement: &tau_proto::ConnectionId,
    ) {
        let state = &mut self.prompt_coordination.context_discovery;
        for pending in state.pending_agents.values_mut() {
            pending.rebind_source(previous, replacement);
        }
        for frozen in state.frozen_agents.values_mut() {
            frozen.inputs.rebind_source(previous, replacement);
            for skill in frozen.skills.values_mut() {
                if &skill.source_id == previous {
                    skill.source_id = replacement.clone();
                }
            }
        }
    }

    /// Establish the readiness barrier at the actual canonical metadata commit.
    pub(super) fn begin_workdir_discovery_refresh(
        &mut self,
        agent_id: &tau_proto::AgentId,
        key: &tau_proto::AgentMetadataKey,
        value: Option<tau_proto::CborValue>,
        mutation_id: Option<tau_proto::AgentMetadataMutationId>,
    ) {
        let state = &mut self.prompt_coordination.context_discovery;
        if !state.pending_agents.contains_key(agent_id) {
            let Some(frozen) = state.frozen_agents.get(agent_id) else {
                return;
            };
            if !frozen
                .inputs
                .workdir_sources
                .values()
                .any(|source| &source.metadata_key == key)
            {
                return;
            }
            let mut pending = frozen.inputs.clone();
            pending.waiting_on.clear();
            pending.publishing_revision = None;
            pending.superseded_refreshes.clear();
            for source in pending.workdir_sources.values_mut() {
                source.refresh = None;
            }
            state.pending_agents.insert(agent_id.clone(), pending);
        }
        let pending = state
            .pending_agents
            .get_mut(agent_id)
            .expect("pending discovery exists");
        let sources = pending
            .workdir_sources
            .iter()
            .filter(|(_, source)| &source.metadata_key == key)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if sources.is_empty() {
            return;
        }
        pending.retained_install = None;
        pending.revision = pending
            .revision
            .checked_add(1)
            .expect("discovery revision exhausted");
        let refresh_id = pending.revision;
        let initialization_id = pending.initialization_id.clone();
        let mut requests = Vec::new();
        for source_id in sources {
            let source = pending
                .workdir_sources
                .get_mut(&source_id)
                .expect("selected source");
            if let Some(mut superseded) = source.refresh.take()
                && superseded.mutation_id.is_some()
            {
                superseded.error =
                    Some("cwd changed again before discovery was installed".to_owned());
                pending.superseded_refreshes.push(superseded);
            }
            source.refresh = Some(tau_proto::DiscoveryRefreshOutcome {
                metadata_key: key.clone(),
                refresh_id,
                mutation_id: mutation_id.clone(),
                error: None,
            });
            source.deadline = Some(Instant::now() + DISCOVERY_REFRESH_TIMEOUT);
            pending.waiting_on.insert(source_id);
            requests.push(tau_proto::HarnessAgentDiscoveryRefreshRequested {
                session_id: self.session_runtime.current_session_id.clone(),
                agent_id: agent_id.clone(),
                agent_initialization_id: initialization_id.clone(),
                metadata_key: key.clone(),
                metadata_value: value.clone(),
                refresh_id,
                retained_user_state: source.retained_user_state.clone(),
            });
        }
        for request in requests {
            self.publish_event(
                Some(harness_connection_id()),
                Event::HarnessAgentDiscoveryRefreshRequested(request),
            );
        }
    }

    /// Return the first source scan deadline without coupling it to tool
    /// lifetime.
    pub(super) fn next_discovery_refresh_deadline(&self) -> Option<Instant> {
        self.prompt_coordination
            .context_discovery
            .pending_agents
            .values()
            .flat_map(|pending| pending.workdir_sources.values())
            .filter_map(|source| source.deadline)
            .min()
    }

    /// Install retained user inputs when a scan cannot complete.
    pub(super) fn fail_discovery_refresh(
        &mut self,
        agent_id: &tau_proto::AgentId,
        source_id: &tau_proto::ConnectionId,
        error: &str,
    ) {
        let Some(pending) = self
            .prompt_coordination
            .context_discovery
            .pending_agents
            .get_mut(agent_id)
        else {
            return;
        };
        let Some(source) = pending.workdir_sources.get_mut(source_id) else {
            return;
        };
        if source.deadline.take().is_none() && !pending.waiting_on.contains(source_id) {
            return;
        }
        source.error = Some(error.to_owned());
        if let Some(refresh) = &mut source.refresh {
            refresh.error = Some(error.to_owned());
        }
        replace_discovery_source(
            &mut pending.skill_candidates,
            &mut pending.skills,
            &mut pending.agents_files,
            source_id,
            source.user_skills.clone(),
            source.user_agents_files.clone(),
        );
        pending.waiting_on.remove(source_id);
        if let Err(error) = self.finalize_agent_discovery(agent_id) {
            self.fail_agent_initialization(agent_id, &error.to_string());
        }
    }

    /// Convert expired scans into ordinary durable degraded replacements.
    pub(super) fn process_discovery_refresh_deadlines(&mut self, now: Instant) {
        let expired = self
            .prompt_coordination
            .context_discovery
            .pending_agents
            .iter()
            .flat_map(|(agent, pending)| {
                pending
                    .workdir_sources
                    .iter()
                    .filter(|(_, source)| source.deadline.is_some_and(|deadline| deadline <= now))
                    .map(move |(source, _)| (agent.clone(), source.clone()))
            })
            .collect::<Vec<_>>();
        for (agent, source) in expired {
            self.fail_discovery_refresh(
                &agent,
                &source,
                "project discovery timed out after cwd commit; retained user context only",
            );
        }
    }

    /// Retain only definite nonacceptance; accepted or ambiguous persistence
    /// outcomes never authorize another append.
    pub(super) fn retain_capacity_rejected_discovery_install(&mut self, event: &Event) {
        let Event::AgentInitializationContextSet(context) = event else {
            return;
        };
        if let Some(pending) = self
            .prompt_coordination
            .context_discovery
            .pending_agents
            .get_mut(&context.agent_id)
            && pending.initialization_id == context.agent_initialization_id
            && pending.revision == context.discovery_revision
        {
            pending.retained_install = Some(context.clone());
        }
    }

    /// Retry the exact approved side-state fact, not its scan or interceptor
    /// chain. New revisions and load lifecycles invalidate the retained owner.
    pub(super) fn retry_capacity_rejected_discovery_installs(&mut self) {
        let contexts = self
            .prompt_coordination
            .context_discovery
            .pending_agents
            .values_mut()
            .filter_map(|pending| {
                let context = pending.retained_install.take()?;
                (pending.initialization_id == context.agent_initialization_id
                    && pending.revision == context.discovery_revision
                    && pending.waiting_on.is_empty())
                .then_some(context)
            })
            .collect::<Vec<_>>();
        for context in contexts {
            if context.session_id != self.session_runtime.current_session_id
                || self
                    .runtime_agent_id_for_target_agent(Some(context.agent_id.as_str()))
                    .is_none()
            {
                continue;
            }
            self.commit_event(
                None,
                &interception::PeerPublicationContext::default(),
                Event::AgentInitializationContextSet(context),
                true,
                None,
                interception::PublicationOutcomeOwners {
                    ui_interaction: None,
                    prompt_acceptance: None,
                    start: None,
                },
            );
        }
        self.drain_deferred_publishes();
    }
}
