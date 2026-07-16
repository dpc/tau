//! Typed CBOR-backed persistence snapshots for deterministic acceptance tests.

use std::path::Path;

use tau_core::{AgentStore, PersistedAgentEvent, PersistedSessionEvent, SessionStore};
use tau_proto::{AgentId, Event, SessionId};

/// One authoritative session-membership and agent-transcript snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct DurableSnapshot {
    /// Exact durable session identifier.
    pub session_id: SessionId,
    /// Exact sole loaded durable agent identifier.
    pub agent_id: AgentId,
    /// Typed records read from the session `events.cbor`.
    pub session_events: Vec<PersistedSessionEvent>,
    /// Typed records read from the agent `events.cbor`.
    pub agent_events: Vec<PersistedAgentEvent>,
}

impl DurableSnapshot {
    /// Loads one session and its sole loaded agent from authoritative CBOR
    /// stores.
    pub fn load(
        state_root: &Path,
        session_id: &SessionId,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut sessions = SessionStore::open(state_root.join("sessions"))?;
        let membership = sessions
            .load_session(session_id.as_str())?
            .ok_or_else(|| format!("missing durable session `{session_id}`"))?;
        let loaded = membership.loaded_agents();
        if loaded.len() != 1 {
            return Err(format!(
                "expected exactly one loaded durable agent, found {}",
                loaded.len()
            )
            .into());
        }
        let agent_id = loaded[0].clone();
        let session_events = sessions.session_events(session_id.as_str())?;
        let matching_loads = session_events
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::SessionAgentLoaded(loaded)
                        if loaded.session_id == *session_id
                            && loaded.agent_id == agent_id
                            && !loaded.ephemeral
                )
            })
            .count();
        let unloads = session_events
            .iter()
            .filter(|record| matches!(&record.event, Event::SessionAgentUnloaded(_)))
            .count();
        if matching_loads != 1 || unloads != 0 || session_events.len() != 1 {
            return Err(format!(
                "expected one durable load and zero unloads for `{agent_id}`; \
                 loads={matching_loads}, unloads={unloads}, records={}",
                session_events.len()
            )
            .into());
        }
        let agents = AgentStore::open(state_root.join("agents"))?;
        let agent_events = agents.agent_events(agent_id.as_str())?;
        Ok(Self {
            session_id: session_id.clone(),
            agent_id,
            session_events,
            agent_events,
        })
    }

    /// Requires this snapshot to preserve every record from `prefix`
    /// record-for-record in typed decode order.
    pub fn require_prefix(&self, prefix: &Self) -> Result<(), Box<dyn std::error::Error>> {
        if self.session_id != prefix.session_id
            || self.agent_id != prefix.agent_id
            || !self.session_events.starts_with(&prefix.session_events)
            || !self.agent_events.starts_with(&prefix.agent_events)
        {
            return Err("durable CBOR prefix changed across cold resume".into());
        }
        Ok(())
    }
}
