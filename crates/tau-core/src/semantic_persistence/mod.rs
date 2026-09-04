//! Lifecycle-owned asynchronous semantic journal persistence.
//!
//! This module is the sole mutable filesystem owner for live semantic streams.
//! Stores stage deterministic replacements in memory and use a generation-bound
//! lease to atomically install the replacement beside one FIFO frame.

mod backend;
mod capacity;
mod identity;
mod owner;
mod preparation;
mod worker;

pub use capacity::PersistenceCapacity;
pub use identity::{PersistenceGeneration, PersistenceLease, StreamIdentity};
#[cfg(any(test, feature = "test-legacy-writer"))]
pub use owner::DurabilityBarrierOutcome;
pub use owner::{
    PersistenceAdmissionError, PersistenceCapacityLimit, PersistenceCapacityPressure,
    PersistenceFailure, PersistenceFailureKind, PersistenceOperationalStatus, PersistenceUsage,
    SemanticPersistenceOwner,
};
pub(crate) use owner::{RetentionCharge, StagedFrame};
pub use preparation::{
    PreparedAgentStream, PreparedSessionStreams, SessionPreparationMode, SessionPreparationStatus,
};
pub(crate) use worker::AgentCheckpointCandidate;

#[cfg(test)]
mod tests;
