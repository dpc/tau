//! Session binding, persistence, and lifecycle runtime ownership.

use std::fmt::{Display as FmtDisplay, LowerHex as FmtLowerHex};

use super::*;

/// Process-local authority generation for the active harness session binding.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SessionGeneration(u64);

impl SessionGeneration {
    /// Reconstructs a generation in focused state-machine tests.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn from_raw(value: u64) -> Self {
        Self(value)
    }

    /// Advances the session generation while preserving scalar saturation.
    #[must_use]
    pub(crate) const fn saturating_next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Moves back one generation in focused rollover tests.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn saturating_previous(self) -> Self {
        Self(self.0.saturating_sub(1))
    }
}

impl std::fmt::Display for SessionGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        FmtDisplay::fmt(&self.0, formatter)
    }
}

impl std::fmt::LowerHex for SessionGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        FmtLowerHex::fmt(&self.0, formatter)
    }
}

/// Harness storage plus the active session's binding and lifecycle state.
pub(crate) struct SessionRuntimeState {
    /// Unique lifecycle owner for every durable semantic stream.
    pub(crate) persistence_owner: Option<std::sync::Arc<tau_core::SemanticPersistenceOwner>>,
    /// Runtime state directory for this harness.
    pub(crate) state_dir: PathBuf,
    /// Session membership store.
    pub(crate) store: SessionStore,
    /// Per-agent transcript store.
    pub(crate) agent_store: AgentStore,
    /// Harness-wide immutable storage policy.
    pub(crate) storage_mode: crate::HarnessStorageMode,
    /// Runtime daemon path stem used for discovery metadata updates.
    pub(crate) runtime_harness_path: Option<PathBuf>,
    /// Absolute canonical startup project root.
    pub(crate) project_root: PathBuf,
    /// Active session binding.
    pub(crate) current_session_id: SessionId,
    /// Monotonic generation of the active session binding.
    pub(crate) current_session_generation: SessionGeneration,
    /// Reason associated with the active session binding.
    pub(crate) current_session_start_reason: tau_proto::SessionStartReason,
    /// Buffered lifecycle messages for the next interaction outcome.
    pub(crate) lifecycle_messages: Vec<String>,
    /// Acceptance order for visible user interactions.
    pub(crate) user_interaction_order: HashMap<String, u64>,
    /// Next process-local visible interaction ordinal.
    pub(crate) next_user_interaction_order: u64,
    /// Interaction facts journal-appended before central delivery.
    pub(crate) precommitted_user_interactions: HashMap<String, u64>,
    /// Current session initialization turn state.
    pub(crate) turn_state: TurnState,
    /// Current-generation session initialization progress.
    pub(crate) session_init_progress_generation: SessionInitProgressGeneration,
    /// State reset as one unit when the active session changes.
    pub(crate) current_session_state: CurrentSessionState,
}
