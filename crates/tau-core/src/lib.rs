//! Core event bus, routing, and connection abstractions.
//!
//! This crate keeps transport details outside the routing layer. Stdio, Unix
//! socket, and in-memory test clients can all plug into the same bus through a
//! small [`ConnectionSink`] interface.

mod action_registry;
mod agent_checkpoint;
mod agent_store;
mod bus;
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
    AgentSummary, list_agent_entries,
};
pub use agent_store::{
    AgentAppendOutcome, AgentCreationFacts, AgentCreationFactsBudget,
    AgentCreationFactsBudgetExceeded, AgentJournalLocks, AgentJournalReader, AgentJournalSnapshot,
    AgentPersistenceMode, AgentStore, AgentStoreError, agent_is_locked, list_agent_metas,
    read_agent_creation_record,
};
pub use bus::EventBus;
pub use connection::{
    AllowAll, Connection, ConnectionMetadata, ConnectionOrigin, ConnectionSendError,
    ConnectionSink, DeliveryFailure, PendingConnectionMetadata, RouteError, RouteReport,
    RoutedFrame, SharedConsumerId, SharedDeliveryGroup, SharedDeliveryTarget, VisibilityFilter,
};
pub use memory::{MemoryInbox, memory_connection};
pub use semantic_persistence::{
    PersistenceAdmissionError, PersistenceCapacity, PersistenceFailure, PersistenceFailureKind,
    PersistenceGeneration, PersistenceLease, PreparedAgentStream, PreparedSessionStreams,
    SemanticPersistenceOwner, SessionPreparationMode, StreamIdentity,
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
    SessionPersistenceMode, SessionStore, SessionStoreError, list_session_metas, session_is_locked,
};
pub use tool_registry::{
    RegisterToolReport, ToolArgumentRepair, ToolArgumentValidationError, ToolProvider,
    ToolProviderKind, ToolRegistration, ToolRegistrationError, ToolRegistry, ToolRouteError,
    ToolRouteReport, ToolRouteTarget, repair_tool_arguments, tool_example_hint,
    validate_tool_arguments, validate_tool_examples,
};
