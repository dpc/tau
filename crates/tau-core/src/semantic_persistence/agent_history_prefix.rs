//! Generation-bound, complete-written agent history for off-loop readers.

use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Weak};

use super::identity::PersistenceLease;
use super::owner::{PersistenceAdmissionError, validate_generation};
use crate::{
    AgentJournalReader, AgentStoreError, AgentTree, PersistedAgentEvent, PersistedAgentEventSeq,
};

/// Immutable read capability for a complete-written prefix of one exact agent
/// generation. Capturing it performs no filesystem I/O; reading it belongs on a
/// filesystem worker, never in a live semantic reaction.
#[derive(Clone, Debug)]
pub struct AgentHistoryPrefix {
    /// Exact owner/stream/generation whose inode was captured.
    lease: PersistenceLease,
    /// Diagnostic path only; reads never reopen this path.
    path: Arc<PathBuf>,
    /// Exact worker-selected inode, upgraded only inside an off-loop read.
    /// Capturing or dropping this capability never closes a file.
    file: Weak<File>,
    /// First sequence not covered by the captured prefix.
    next_seq: PersistedAgentEventSeq,
    /// Byte offset immediately after the last complete frame.
    end_offset: u64,
}

/// Worker-owned readable frontier, coalesced in the bounded stream registry.
pub(super) struct ReadableAgentJournal {
    /// Non-owning reference: registry cleanup under the admission mutex must
    /// never close a file. The worker and captured readers own the descriptor.
    pub(super) file: Weak<File>,
    /// Last complete frame boundary.
    pub(super) end_offset: u64,
    /// First sequence not covered by that boundary.
    pub(super) next_seq: PersistedAgentEventSeq,
}

impl PersistenceLease {
    /// Captures the latest complete-written prefix without waiting for writes
    /// or synchronization. `None` means no readable agent prefix is
    /// available yet.
    pub fn agent_history_prefix(
        &self,
    ) -> Result<Option<AgentHistoryPrefix>, PersistenceAdmissionError> {
        let shared = self
            .identity
            .shared
            .upgrade()
            .ok_or(PersistenceAdmissionError::Unavailable)?;
        let state = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        validate_generation(
            &state,
            shared.owner_epoch,
            self.identity.owner_epoch,
            &self.identity.stream,
            self.identity.generation,
        )?;
        let registered = state
            .streams
            .get(&self.identity.stream)
            .expect("validated stream");
        let Some(readable) = &registered.readable_agent else {
            return Ok(None);
        };
        // A frontier whose worker-owned handle disappeared is unavailable, not
        // an empty/new stream. This also covers the worker-exit invalidation
        // cut.
        if readable.file.strong_count() == 0 {
            return Err(PersistenceAdmissionError::Unavailable);
        }
        Ok(Some(AgentHistoryPrefix {
            lease: self.clone(),
            path: Arc::new(registered.journal_path.clone()),
            file: readable.file.clone(),
            next_seq: readable.next_seq,
            end_offset: readable.end_offset,
        }))
    }
}

impl AgentHistoryPrefix {
    /// Returns the first sequence outside this immutable prefix.
    #[must_use]
    pub fn next_seq(&self) -> PersistedAgentEventSeq {
        self.next_seq
    }

    /// Returns the exact lease that authorized this prefix.
    #[must_use]
    pub fn lease(&self) -> &PersistenceLease {
        &self.lease
    }

    /// Checks that the owning generation still permits reads.
    pub fn validate_authority(&self) -> Result<(), PersistenceAdmissionError> {
        let shared = self
            .lease
            .identity
            .shared
            .upgrade()
            .ok_or(PersistenceAdmissionError::Unavailable)?;
        let state = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        validate_generation(
            &state,
            shared.owner_epoch,
            self.lease.identity.owner_epoch,
            &self.lease.identity.stream,
            self.lease.identity.generation,
        )
    }

    /// Reads and strictly validates the selected finite prefix. Must run off
    /// the runtime loop. Revocation wins over a racing filesystem error so
    /// ordinary lifecycle cancellation is not misclassified as corruption.
    pub fn read(&self) -> Result<Vec<PersistedAgentEvent>, AgentStoreError> {
        self.read_with_after_io(|| {})
    }

    /// Exercises revocation between actual I/O and its final authority check.
    #[cfg(test)]
    pub(crate) fn read_with_after_io_for_test(
        &self,
        after_io: impl FnOnce(),
    ) -> Result<Vec<PersistedAgentEvent>, AgentStoreError> {
        self.read_with_after_io(after_io)
    }

    fn read_with_after_io(
        &self,
        after_io: impl FnOnce(),
    ) -> Result<Vec<PersistedAgentEvent>, AgentStoreError> {
        self.validate_authority()
            .map_err(AgentStoreError::Persistence)?;
        let result = self
            .file
            .upgrade()
            .ok_or(AgentStoreError::Persistence(
                PersistenceAdmissionError::Unavailable,
            ))
            .and_then(|file| self.read_inner(&file));
        after_io();
        self.validate_authority()
            .map_err(AgentStoreError::Persistence)?;
        result
    }

    fn read_inner(&self, file: &File) -> Result<Vec<PersistedAgentEvent>, AgentStoreError> {
        let events =
            AgentJournalReader::from_complete_prefix(self.path.as_path(), file, self.end_offset)
                .collect::<Result<Vec<_>, _>>()?;
        let actual = PersistedAgentEventSeq::new(events.len() as u64);
        if actual != self.next_seq {
            return Err(AgentStoreError::InvalidSequence {
                path: self.path.as_ref().clone(),
                expected: self.next_seq,
                actual,
            });
        }
        let super::StreamIdentity::Agent(agent_id) = self.lease.stream() else {
            return Err(AgentStoreError::Persistence(
                PersistenceAdmissionError::StaleLease,
            ));
        };
        AgentTree::try_from_events(agent_id.clone(), &events)
            .map_err(|source| AgentStoreError::InvalidEvent { source })?;
        Ok(events)
    }
}
