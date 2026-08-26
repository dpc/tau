//! Typed stream identities and generation-bound admission leases.

use std::fmt;
use std::sync::{Arc, Weak};

use tau_proto::{AgentId, SessionId};

use super::owner::Shared;

/// One authoritative semantic journal stream.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum StreamIdentity {
    /// One durable agent transcript.
    Agent(AgentId),
    /// One ordinary durable session journal.
    Session(SessionId),
    /// One session execution/restore journal.
    SessionRestore(SessionId),
}

/// Monotonic generation of one stream identity within an owner epoch.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PersistenceGeneration(pub(crate) u64);

impl PersistenceGeneration {
    /// Returns the process-local numeric generation for diagnostics and tests.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Capability permitting admission only to one exact owner, stream, and
/// generation.
#[derive(Clone)]
pub struct PersistenceLease {
    /// Shared immutable identity makes capability cloning allocation-free.
    pub(crate) identity: Arc<LeaseIdentity>,
}

/// Immutable exact authority shared by every clone of one capability.
pub(crate) struct LeaseIdentity {
    /// Owner state invalidated atomically if its worker exits.
    pub(crate) shared: Weak<Shared>,
    /// Epoch distinguishes owners even when stream and generation numbers
    /// match.
    pub(crate) owner_epoch: u64,
    /// Exact journal selected by this capability.
    pub(crate) stream: StreamIdentity,
    /// Exact registered generation selected by this capability.
    pub(crate) generation: PersistenceGeneration,
}

impl PersistenceLease {
    /// Returns the stream identity selected by this lease.
    #[must_use]
    pub fn stream(&self) -> &StreamIdentity {
        &self.identity.stream
    }

    /// Returns the registered stream generation.
    #[must_use]
    pub fn generation(&self) -> PersistenceGeneration {
        self.identity.generation
    }
}

impl fmt::Debug for PersistenceLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PersistenceLease")
            .field("owner_epoch", &self.identity.owner_epoch)
            .field("stream", &self.identity.stream)
            .field("generation", &self.identity.generation)
            .finish()
    }
}
