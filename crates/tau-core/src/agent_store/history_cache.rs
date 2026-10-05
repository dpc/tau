//! Managed accepted-record residency; filesystem reads belong to the caller's
//! worker.

use std::sync::Arc;

use tau_proto::AgentId;

use super::{AgentStore, AgentStoreError, managed_agent_encoded_event_bytes};
use crate::{
    AgentEventValidationError, AgentHistoryPrefix, PersistedAgentEvent, PersistenceAdmissionError,
    StreamIdentity,
};

/// Validated, sized history materialized off-loop for an exact captured prefix.
///
/// Private fields prevent callers from pairing unrelated records with a lease.
pub struct PrefetchedAgentHistory {
    /// Metadata-only authority validated again at runtime handoff.
    prefix: AgentHistoryPrefix,
    /// Strictly decoded complete prefix in sequence order.
    records: Vec<PersistedAgentEvent>,
    /// Encoded record sizes computed on the reader worker, not runtime handoff.
    record_bytes: Vec<usize>,
}

impl PrefetchedAgentHistory {
    /// Returns the exact generation and written cut used by the reader.
    #[must_use]
    pub fn prefix(&self) -> &AgentHistoryPrefix {
        &self.prefix
    }
}

impl AgentHistoryPrefix {
    /// Reads, validates and sizes this prefix for cache installation.
    ///
    /// All filesystem and full-prefix encoding work must execute on a read
    /// worker.
    pub fn prefetch(&self) -> Result<PrefetchedAgentHistory, AgentStoreError> {
        let records = self.read()?;
        let record_bytes = records
            .iter()
            .map(|record| managed_agent_encoded_event_bytes(std::slice::from_ref(record)))
            .collect();
        self.validate_authority()
            .map_err(AgentStoreError::Persistence)?;
        Ok(PrefetchedAgentHistory {
            prefix: self.clone(),
            records,
            record_bytes,
        })
    }
}

impl AgentStore {
    /// Reports whether an explicitly prepared agent needs history prefetch.
    ///
    /// Unprepared and memory-only agents do not have an evicted managed cache.
    #[must_use]
    pub fn agent_history_is_evicted(&self, agent_id: &AgentId) -> bool {
        self.managed_projections
            .get(agent_id)
            .is_some_and(|projection| projection.history.first_resident_seq != 0)
    }

    /// Captures a prepared agent's complete-written prefix without filesystem
    /// I/O.
    pub fn agent_history_prefix(
        &self,
        agent_id: &AgentId,
    ) -> Result<Option<AgentHistoryPrefix>, AgentStoreError> {
        self.persistence_leases
            .get(agent_id)
            .map(|lease| {
                lease
                    .agent_history_prefix()
                    .map_err(AgentStoreError::Persistence)
            })
            .transpose()
            .map(Option::flatten)
    }

    /// Returns encoded bytes and records retained from a request's protected
    /// cut.
    ///
    /// The caller must keep that cut protected until the read is consumed or
    /// canceled. A missing suffix is a lifecycle invariant failure, never empty
    /// history.
    pub fn agent_history_pin_usage(
        &self,
        agent_id: &AgentId,
        from_seq: u64,
    ) -> Result<(usize, u64), AgentStoreError> {
        let Some(projection) = self.managed_projections.get(agent_id) else {
            return Err(AgentStoreError::Persistence(
                PersistenceAdmissionError::StaleLease,
            ));
        };
        let history = &projection.history;
        if from_seq < history.first_resident_seq || from_seq > history.accepted_count {
            return Err(history_invariant(
                "protected accepted history suffix is unavailable",
            ));
        }
        let offset = (from_seq - history.first_resident_seq) as usize;
        let bytes = history.record_bytes[offset..]
            .iter()
            .fold(0_usize, |sum, size| sum.saturating_add(*size));
        Ok((bytes, history.accepted_count - from_seq))
    }

    /// Evicts only a complete-written prefix, stopping at an optional request
    /// pin.
    ///
    /// Lifecycle owners must not call this while startup, load replay or
    /// discovery still needs complete records. No file is opened, read,
    /// synchronized or closed.
    pub fn evict_agent_history(
        &mut self,
        agent_id: &AgentId,
        protected_from: Option<u64>,
    ) -> Result<(), AgentStoreError> {
        let Some(prefix) = self.agent_history_prefix(agent_id)? else {
            return Ok(());
        };
        let end = protected_from.map_or(prefix.next_seq().get(), |pin| {
            pin.min(prefix.next_seq().get())
        });
        if let Some(projection) = self.managed_projections.get_mut(agent_id) {
            projection.history.evict_before(end);
        }
        Ok(())
    }

    /// Installs a validated off-loop prefix followed by the current accepted
    /// suffix.
    ///
    /// This changes only cache residency, not live facts, tree authority or
    /// sequence. The caller must keep the captured prefix end pinned until
    /// this handoff.
    pub fn install_agent_history_prefix(
        &mut self,
        prefetched: PrefetchedAgentHistory,
    ) -> Result<(), AgentStoreError> {
        let PrefetchedAgentHistory {
            prefix,
            mut records,
            mut record_bytes,
        } = prefetched;
        prefix
            .validate_authority()
            .map_err(AgentStoreError::Persistence)?;
        let StreamIdentity::Agent(agent_id) = prefix.lease().stream() else {
            return Err(history_invariant("history prefix does not name an agent"));
        };
        let lease = self
            .persistence_leases
            .get(agent_id)
            .ok_or(AgentStoreError::Persistence(
                PersistenceAdmissionError::StaleLease,
            ))?;
        if !Arc::ptr_eq(&lease.identity, &prefix.lease().identity) {
            return Err(AgentStoreError::Persistence(
                PersistenceAdmissionError::StaleLease,
            ));
        }
        let end = prefix.next_seq().get();
        self.agent_history_pin_usage(agent_id, end)?;
        let history = &mut self
            .managed_projections
            .get_mut(agent_id)
            .expect("validated history projection")
            .history;
        let offset = (end - history.first_resident_seq) as usize;
        record_bytes.extend_from_slice(&history.record_bytes[offset..]);
        records.extend_from_slice(&history.records[offset..]);
        history.records = records;
        history.record_bytes = record_bytes;
        history.first_resident_seq = 0;
        Ok(())
    }
}

/// Reports a local cache consistency failure without returning a partial
/// history.
fn history_invariant(message: &str) -> AgentStoreError {
    AgentStoreError::InvalidEvent {
        source: AgentEventValidationError::new(message),
    }
}
