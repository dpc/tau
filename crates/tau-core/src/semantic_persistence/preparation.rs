//! Typed synchronous preparation results and worker commands.

use std::sync::Arc;
use std::sync::mpsc::SyncSender;

use super::identity::{LeaseIdentity, PersistenceLease};
use super::owner::PersistenceAdmissionError;

/// Canonical session launch authority used during preparation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPreparationMode {
    /// Establish a new canonical manifest and empty streams.
    New,
    /// Require and validate the existing canonical manifest and streams.
    Resume,
}

/// Strictly recovered existing durable agent stream.
pub struct PreparedAgentStream {
    /// Generation-bound admission authority.
    pub lease: PersistenceLease,
    /// Longest valid recovered durable prefix.
    pub events: Vec<crate::PersistedAgentEvent>,
}

/// Strictly prepared ordinary and restore streams for one session.
pub struct PreparedSessionStreams {
    /// Ordinary-session generation-bound admission authority.
    pub session_lease: PersistenceLease,
    /// Restore-journal generation-bound admission authority.
    pub restore_lease: PersistenceLease,
    /// Strictly recovered ordinary-session prefix.
    pub events: Vec<crate::PersistedSessionEvent>,
    /// Strictly recovered restore prefix.
    pub restore_events: Vec<crate::PersistedSessionEvent>,
    /// Canonical manifest loaded or created during preparation.
    pub meta: crate::SessionMeta,
}

/// Result returned across the worker preparation handoff.
pub(crate) enum PreparationResult {
    /// One prepared agent stream and its recovered records.
    Agent(Vec<crate::PersistedAgentEvent>),
    /// Both prepared session streams, records, and canonical manifest.
    Session {
        /// Ordinary-session records.
        events: Vec<crate::PersistedSessionEvent>,
        /// Restore-journal records.
        restore_events: Vec<crate::PersistedSessionEvent>,
        /// Canonical manifest authority.
        meta: crate::SessionMeta,
    },
}

/// Typed synchronous lifecycle work executed only by the filesystem worker.
pub(crate) enum WorkerCommand {
    /// Creates and synchronizes one canonical store root before registration.
    PrepareRoot {
        /// Canonical store root.
        path: std::path::PathBuf,
        /// Synchronous preparation result.
        reply: SyncSender<Result<(), PersistenceAdmissionError>>,
    },
    /// Strictly recover and acquire one existing durable agent.
    PrepareAgent {
        /// Exact generation being prepared.
        identity: Arc<LeaseIdentity>,
        /// Bounded reply channel.
        reply: SyncSender<Result<PreparationResult, PersistenceAdmissionError>>,
    },
    /// Create or strictly resume both canonical session streams.
    PrepareSession {
        /// Exact ordinary-session generation.
        session: Arc<LeaseIdentity>,
        /// Exact restore generation.
        restore: Arc<LeaseIdentity>,
        /// New-versus-resume manifest authority.
        mode: SessionPreparationMode,
        /// Bounded reply channel.
        reply: SyncSender<Result<PreparationResult, PersistenceAdmissionError>>,
    },
    /// Coalesces a lossy session activity hint for deadline-driven writeback.
    TouchSession {
        /// Exact ordinary-session generation.
        identity: Arc<LeaseIdentity>,
        /// Monotonic maximum wall-clock hint.
        last_touched: u64,
        /// Highest authoritative frame admitted before this hint.
        prerequisite: u64,
        /// Known disposition when the frame completed before touch admission.
        prerequisite_written: Option<bool>,
    },
    /// Releases exact generations after all earlier authoritative frames.
    Release {
        /// Exact capabilities to release together.
        identities: Vec<Arc<LeaseIdentity>>,
        /// Bounded reply channel.
        reply: SyncSender<Result<(), PersistenceAdmissionError>>,
    },
    /// Acknowledges only after every earlier frame and durability debt drains.
    #[cfg(any(test, feature = "test-legacy-writer"))]
    DurabilityBarrier {
        /// One-shot deterministic test reply.
        reply: SyncSender<Result<(), PersistenceAdmissionError>>,
    },
}
