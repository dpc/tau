//! Core event bus, routing, and connection abstractions.
//!
//! This crate keeps transport details outside the routing layer. Stdio, Unix
//! socket, and in-memory test clients can all plug into the same bus through a
//! small [`ConnectionSink`] interface.

mod action_registry;
mod agent_checkpoint;
mod agent_store;
mod bus;
mod compaction_chain_view;
mod connection;
mod journal_sync;
mod memory;
mod record_log;
mod semantic_persistence;
mod session;
mod session_store;
mod tool_registry;

#[cfg(test)]
mod tests;

pub use action_registry::{
    ActionProviderSchema, ActionRegistry, ActionRegistryError, ActionRouteError,
};
pub use agent_checkpoint::{
    AgentCheckpoint, AgentJournalCheckpoint, AgentListEntry, AgentListIdentity, AgentListStatus,
    AgentRetentionEvidence, AgentSummary, inspect_agent_retention_evidence, list_agent_entries,
};
pub use agent_store::{
    AgentAppendOutcome, AgentCreationFacts, AgentCreationFactsBudget,
    AgentCreationFactsBudgetExceeded, AgentJournalLocks, AgentJournalReader, AgentJournalSnapshot,
    AgentPersistenceMode, AgentStore, AgentStoreError, agent_is_locked, list_agent_metas,
    read_agent_creation_record, retired_agent_tombstone, retired_agents_dir,
};
pub use bus::{DeliveryOutcomeCount, EventBus};
pub use compaction_chain_view::{
    CompactionChainCompletion, CompactionChainElapsed, CompactionChainEstimatedCost,
    CompactionChainView,
};
pub use connection::{
    AllowAll, Connection, ConnectionMetadata, ConnectionOrigin, ConnectionSendError,
    ConnectionSink, DeliveryFailure, PendingConnectionMetadata, RouteError, RouteReport,
    RoutedFrame, SharedConsumerId, SharedDeliveryGroup, SharedDeliveryTarget, VisibilityFilter,
};
pub use memory::{MemoryInbox, memory_connection};
#[cfg(any(test, feature = "test-legacy-writer"))]
pub use semantic_persistence::DurabilityBarrierOutcome;
pub use semantic_persistence::{
    PersistenceAdmissionError, PersistenceCapacity, PersistenceCapacityLimit,
    PersistenceCapacityPressure, PersistenceFailure, PersistenceFailureKind, PersistenceGeneration,
    PersistenceLease, PersistenceOperationalStatus, PersistenceUsage, PreparedAgentStream,
    PreparedSessionStreams, SemanticPersistenceOwner, SessionPreparationMode,
    SessionPreparationStatus, StreamIdentity,
};
pub use session::{
    AgentEntry, AgentEventParent, AgentEventValidationError, AgentJournalFoldSemantics,
    AgentMessageDirection, AgentMeta, AgentMetadataEntry, AgentNode, AgentTree,
    BackgroundToolCallState, BackgroundToolCompletion, BackgroundToolPlaceholder,
    InferenceDispatchRecovery, ManualCompactionOutcome, ManualCompactionRecovery, NodeId,
    OutputLengthContinuationRecovery, OutputLengthDormantRepair, OutputLengthTerminalIncomplete,
    PersistedAgentEvent, PersistedAgentEventSeq, PersistedEventSource, ReactiveCompactionProgress,
    SessionMeta, StandaloneCompactionRecovery,
};
pub use session_store::{
    AppendOutcome, PersistedSessionEvent, PersistedSessionEventSeq, SessionMembership,
    SessionPersistenceMode, SessionRetentionReferences, SessionStore, SessionStoreError,
    list_session_metas, read_session_ever_loaded_agents, session_is_locked,
};
pub use tool_registry::{
    RegisterToolReport, ToolArgumentRepair, ToolArgumentValidationError, ToolProvider,
    ToolProviderKind, ToolRegistration, ToolRegistrationError, ToolRegistry, ToolRouteError,
    ToolRouteReport, ToolRouteTarget, repair_tool_arguments, tool_example_hint,
    validate_tool_arguments, validate_tool_examples,
};
