//! Independent lifecycle owners of full accepted-record residency.

use super::history_runtime::HistoryOperation;
use super::start_coordinator::StartPhase;
use super::*;

/// Startup, synchronous consumers and asynchronous load/discovery hold separate
/// protection; finishing one must never release another.
#[derive(Default)]
pub(super) struct HistoryLifecycle {
    /// False until successful session initialization finishes all history
    /// users.
    startup_complete: bool,
    /// Nestable synchronous preparation or replay consumers.
    preparing: HashMap<AgentId, usize>,
    /// Exact load attempts whose committed reaction has not finished.
    loading: HashMap<AgentId, tau_proto::AgentInitializationId>,
    /// Load attempts not yet inside their committed reaction.
    awaiting_load: HashMap<AgentId, tau_proto::AgentInitializationId>,
    /// Exact initialization whose current-revision install has not finished.
    discovering: HashMap<AgentId, tau_proto::AgentInitializationId>,
}

/// Original load publication parameters retained across an off-loop cold read.
pub(super) struct HistoryLoad {
    /// Exact session, agent and initialization identity.
    pub(super) loaded: tau_proto::SessionAgentLoaded,
    /// Existing correlated start publication owner, when present.
    pub(super) start_id: Option<tau_proto::StartOperationId>,
    /// Already-loaded initialization restatements remain transient.
    pub(super) persist: bool,
}

impl Harness {
    /// Pins synchronous callers across nested publication and early returns.
    pub(super) fn begin_history_preparation(&mut self, agent: &AgentId) {
        *self
            .session_runtime
            .history
            .lifecycle
            .preparing
            .entry(agent.clone())
            .or_default() += 1;
    }

    /// Releases just this synchronous caller, then checks current frontiers.
    pub(super) fn finish_history_preparation(&mut self, agent: &AgentId) {
        let preparing = &mut self.session_runtime.history.lifecycle.preparing;
        let count = preparing
            .get_mut(agent)
            .expect("balanced history preparation");
        *count -= 1;
        if *count == 0 {
            preparing.remove(agent);
        }
        self.evict_eligible_history();
    }

    /// Starts independent load-reaction and discovery protection before any
    /// publication can reenter the runtime.
    pub(super) fn begin_history_load(&mut self, load: &tau_proto::SessionAgentLoaded) {
        let state = &mut self.session_runtime.history.lifecycle;
        state
            .loading
            .insert(load.agent_id.clone(), load.agent_initialization_id.clone());
        state
            .awaiting_load
            .insert(load.agent_id.clone(), load.agent_initialization_id.clone());
        state
            .discovering
            .insert(load.agent_id.clone(), load.agent_initialization_id.clone());
    }

    /// Prevents ready/disconnect from finalizing an unpublished cold load.
    pub(super) fn history_load_is_pending(&self, agent: &AgentId) -> bool {
        self.session_runtime
            .history
            .lifecycle
            .awaiting_load
            .contains_key(agent)
    }

    /// The load has committed; discovery may now finalize, while replay still
    /// owns its independent full-history protection.
    pub(super) fn enter_history_load_reaction(&mut self, loaded: &tau_proto::SessionAgentLoaded) {
        let awaiting = &mut self.session_runtime.history.lifecycle.awaiting_load;
        if awaiting.get(&loaded.agent_id) == Some(&loaded.agent_initialization_id) {
            awaiting.remove(&loaded.agent_id);
        }
    }

    /// Releases only the exact completed load reaction.
    pub(super) fn finish_history_load_reaction(&mut self, loaded: &tau_proto::SessionAgentLoaded) {
        let loading = &mut self.session_runtime.history.lifecycle.loading;
        if loading.get(&loaded.agent_id) == Some(&loaded.agent_initialization_id) {
            loading.remove(&loaded.agent_id);
        }
        self.evict_eligible_history();
    }

    /// A stale discovery result cannot release a newer initialization's pin.
    pub(super) fn finish_history_discovery(
        &mut self,
        context: &tau_proto::AgentInitializationContextSet,
    ) {
        let discovering = &mut self.session_runtime.history.lifecycle.discovering;
        if discovering.get(&context.agent_id) == Some(&context.agent_initialization_id) {
            discovering.remove(&context.agent_id);
        }
        self.evict_eligible_history();
    }

    /// Enables written-frontier wakes only after startup's final history user,
    /// and explicitly sweeps edges drained while wakes were disabled.
    pub(super) fn finish_history_startup(&mut self) {
        self.session_runtime.history.lifecycle.startup_complete = true;
        if let Some(owner) = self.session_runtime.persistence_owner.as_ref() {
            owner.enable_agent_history_wakes();
        }
        self.evict_eligible_history();
    }

    /// Teardown removes per-agent reasons, never the session startup reason.
    pub(super) fn retire_history_load(&mut self, agent: &AgentId) {
        self.cancel_agent_history_load(agent);
        let state = &mut self.session_runtime.history.lifecycle;
        state.loading.remove(agent);
        state.awaiting_load.remove(agent);
        state.discovering.remove(agent);
        self.evict_eligible_history();
    }

    /// Metadata-only sweep of managed projections; trees and live facts stay.
    pub(super) fn evict_eligible_history(&mut self) {
        if !self.session_runtime.history.lifecycle.startup_complete {
            return;
        }
        let agents = self
            .session_runtime
            .agent_store
            .agents()
            .into_iter()
            .map(|tree| crate::parse_agent_id(tree.agent_id()))
            .collect::<Vec<_>>();
        for agent in agents {
            let state = &self.session_runtime.history.lifecycle;
            if state.preparing.contains_key(&agent)
                || state.loading.contains_key(&agent)
                || state.discovering.contains_key(&agent)
                || self
                    .prompt_coordination
                    .context_discovery
                    .pending_agents
                    .contains_key(&agent)
            {
                continue;
            }
            let pin = self.history_request_pin(&agent);
            if let Err(error) = self
                .session_runtime
                .agent_store
                .evict_agent_history(&agent, pin)
            {
                self.record_history_error(error);
            }
        }
    }

    /// Submits a cold load after its discovery barrier exists; resident loads
    /// retain the original synchronous publication behavior.
    pub(super) fn publish_or_prefetch_history_load(&mut self, load: HistoryLoad) {
        if self
            .session_runtime
            .agent_store
            .agent_history_is_evicted(&load.loaded.agent_id)
        {
            self.defer_owned_history_operation(
                None,
                vec![load.loaded.agent_id.clone()],
                HistoryOperation::Load(load),
            );
        } else {
            self.publish_history_ready_load(load);
        }
    }

    /// Revalidates the exact pending initialization before publishing
    /// membership.
    pub(super) fn publish_history_ready_load(&mut self, load: HistoryLoad) {
        if load.loaded.session_id != self.session_runtime.current_session_id
            || !self
                .prompt_coordination
                .context_discovery
                .pending_agents
                .get(&load.loaded.agent_id)
                .is_some_and(|pending| {
                    pending.initialization_id == load.loaded.agent_initialization_id
                })
        {
            return;
        }
        let event = Event::SessionAgentLoaded(load.loaded);
        if let Some(start_id) = load.start_id {
            self.enqueue_start_phase(
                event,
                load.persist,
                false,
                StartPhaseOwner {
                    start_id,
                    expected_phase: StartPhase::AwaitLoadedCommit,
                    expected_event: tau_proto::EventName::SESSION_AGENT_LOADED,
                },
            );
        } else if load.persist {
            self.publish_event(None, event);
        } else {
            self.enqueue_publish(None, event, false, false, None);
        }
    }

    /// Read admission failures use the existing initialization/start failure
    /// lifecycle rather than allowing ungated dispatch or inventing a retry.
    pub(super) fn fail_history_load(&mut self, load: &HistoryLoad, message: &str) {
        if self
            .prompt_coordination
            .context_discovery
            .pending_agents
            .get(&load.loaded.agent_id)
            .is_some_and(|pending| pending.initialization_id == load.loaded.agent_initialization_id)
        {
            self.fail_agent_initialization(&load.loaded.agent_id, message);
        }
    }
}
