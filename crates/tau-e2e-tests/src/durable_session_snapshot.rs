//! Multi-agent CBOR persistence snapshots for deterministic restore acceptance.

use std::collections::BTreeMap;
use std::path::Path;

use tau_core::{AgentStore, PersistedAgentEvent, PersistedSessionEvent, SessionStore};
use tau_proto::{AgentId, SessionId};

/// Authoritative multi-agent membership, restore, and transcript snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct DurableSessionSnapshot {
    /// Exact durable session identifier.
    pub session_id: SessionId,
    /// Typed membership records from the session journal.
    pub session_events: Vec<PersistedSessionEvent>,
    /// Typed execution-restore records from the session restore journal.
    pub restore_events: Vec<PersistedSessionEvent>,
    /// Current composed durable membership and each agent's typed journal.
    pub agent_events: BTreeMap<AgentId, Vec<PersistedAgentEvent>>,
}

impl DurableSessionSnapshot {
    /// Loads all currently composed durable session agents from authoritative
    /// stores.
    ///
    /// # Errors
    ///
    /// Returns an error when the session, membership, restore stream, or any
    /// current agent journal cannot be decoded.
    pub fn load(
        state_root: &Path,
        session_id: &SessionId,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut sessions = SessionStore::open(state_root.join("sessions"))?;
        let membership = sessions
            .load_session(session_id.as_str())?
            .ok_or_else(|| format!("missing durable session `{session_id}`"))?;
        let loaded_agents = membership
            .loaded_agents()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let session_events = sessions.session_events(session_id.as_str())?;
        let restore_events = sessions.session_restore_events(session_id.as_str())?;
        let agents = AgentStore::open(state_root.join("agents"))?;
        let mut agent_events = BTreeMap::new();
        for agent_id in loaded_agents {
            let events = agents.agent_events(agent_id.as_str())?;
            agent_events.insert(agent_id, events);
        }
        Ok(Self {
            session_id: session_id.clone(),
            session_events,
            restore_events,
            agent_events,
        })
    }

    /// Requires every Boot-A record to remain an exact prefix after cold
    /// resume.
    ///
    /// # Errors
    ///
    /// Returns an error when session identity, membership, restore state,
    /// current agent set, or any per-agent journal prefix changed.
    pub fn require_prefix(&self, prefix: &Self) -> Result<(), Box<dyn std::error::Error>> {
        if self.session_id != prefix.session_id {
            return Err("durable session identity changed across cold resume".into());
        }
        if !self.session_events.starts_with(&prefix.session_events) {
            return Err("durable membership prefix changed across cold resume".into());
        }
        if !self.restore_events.starts_with(&prefix.restore_events) {
            return Err(format!(
                "durable restore prefix changed across cold resume: before={}, after={}",
                prefix.restore_events.len(),
                self.restore_events.len()
            )
            .into());
        }
        if self.agent_events.keys().ne(prefix.agent_events.keys()) {
            return Err(format!(
                "durable current agent set changed across cold resume: before={:?}, after={:?}",
                prefix.agent_events.keys().collect::<Vec<_>>(),
                self.agent_events.keys().collect::<Vec<_>>()
            )
            .into());
        }
        if let Some(agent_id) = self.agent_events.iter().find_map(|(agent_id, events)| {
            prefix
                .agent_events
                .get(agent_id)
                .is_none_or(|prefix| !events.starts_with(prefix))
                .then_some(agent_id)
        }) {
            return Err(
                format!("durable agent prefix changed across cold resume: {agent_id}").into(),
            );
        }
        Ok(())
    }
}
