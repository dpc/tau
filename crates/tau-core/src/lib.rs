//! Core event bus, routing, and connection abstractions.
//!
//! This crate keeps transport details outside the routing layer. Stdio, Unix
//! socket, and in-memory test clients can all plug into the same bus through a
//! small [`ConnectionSink`] interface.

mod action_registry;
mod agent_store;
mod bus;
mod connection;
mod memory;
mod policy;
mod session;
mod session_store;
mod tool_registry;

#[cfg(test)]
mod tests;

pub use action_registry::{
    ActionProviderSchema, ActionRegistry, ActionRegistryError, ActionRouteError,
};
pub use agent_store::{
    AgentAppendOutcome, AgentPersistenceMode, AgentStore, AgentStoreError, agent_is_locked,
    list_agent_metas,
};
pub use bus::EventBus;
pub use connection::{
    AllowAll, Connection, ConnectionMetadata, ConnectionOrigin, ConnectionSendError,
    ConnectionSink, DeliveryFailure, RouteError, RouteReport, RoutedFrame, VisibilityFilter,
};
pub use memory::{MemoryInbox, memory_connection};
pub use policy::{
    DefaultSubscriptionPolicy, PolicyStore, SubscriptionApproval, SubscriptionPolicy,
    SubscriptionPolicyError,
};
pub use session::{
    AgentEntry, AgentEventParent, AgentEventValidationError, AgentMessageDirection, AgentMeta,
    AgentMetadataEntry, AgentNode, AgentTree, BackgroundToolCallState, BackgroundToolCompletion,
    BackgroundToolPlaceholder, NodeId, PersistedAgentEvent, PersistedAgentEventSeq, SessionMeta,
};
pub use session_store::{
    AppendOutcome, PersistedSessionEvent, PersistedSessionEventSeq, SessionMembership,
    SessionPersistenceMode, SessionStore, SessionStoreError, list_session_metas, session_is_locked,
};
pub use tool_registry::{
    RegisterToolReport, ToolArgumentRepair, ToolArgumentValidationError, ToolProvider,
    ToolProviderKind, ToolRegistrationError, ToolRegistry, ToolRegistryWarning, ToolRouteError,
    ToolRouteReport, ToolRouteTarget, repair_tool_arguments, tool_example_hint,
    validate_tool_arguments, validate_tool_examples,
};
