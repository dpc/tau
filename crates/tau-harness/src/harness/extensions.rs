//! Extension lifecycle and activation state owned by the harness.
//!
//! The harness event loop still coordinates activation because staged extension
//! announcements interact with prompt assembly, routing, and replay. This
//! module names the extension-specific state machine separately from the rest
//! of [`Harness`](super::Harness).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use tau_proto::{Event, HarnessInputMessage, PromptFragment, ToolRegistrationDeclared};

use crate::event::SupervisedWriterHandle;
use crate::extension::ExtensionEntry;

/// Event payload staged while an extension is still handshaking.
#[derive(Clone, Debug)]
pub(super) struct StagedExtensionPublish {
    /// Event payload withheld until the source extension reaches `Ready`.
    pub(super) event: Event,
    /// Whether the staged event should skip durable session history.
    pub(super) transient: bool,
}

/// Operational message withheld behind activation with its global arrival
/// order.
#[derive(Clone, Debug)]
pub(super) struct DeferredExtensionMessage {
    /// Monotonic harness-local arrival order.
    pub(super) order: u64,
    /// Owned protocol message replayed after activation.
    pub(super) message: HarnessInputMessage,
}

/// Extension-originated announcements accumulated until the extension reaches
/// `Ready` and can be activated atomically.
#[derive(Clone, Debug, Default)]
pub(super) struct ExtensionActivationStage {
    /// Tool registrations received before the extension finished its handshake.
    pub(super) tool_registrations: Vec<ToolRegistrationDeclared>,
    /// Canonical provider model updates derived from committed declarations and
    /// awaiting activation, in declaration commit order.
    pub(super) provider_model_updates: Vec<tau_proto::ProviderModelsUpdated>,
    /// Action schema received before `Ready`. Schema publishing is a
    /// replacement, so only the latest staged schema matters.
    pub(super) action_schema: Option<tau_actions::ActionSchema>,
    /// Skill announcements received before `Ready`, in wire order.
    pub(super) skill_announcements: Vec<tau_proto::ExtSkillAvailable>,
    /// AGENTS.md announcements received before `Ready`, in wire order.
    pub(super) agents_files: Vec<tau_proto::ExtAgentsMdAvailable>,
    /// Whether the extension registered as an agent context provider before
    /// `Ready`.
    pub(super) agent_context_provider_registered: bool,
    /// Whether the extension registered as a session context provider before
    /// `Ready`.
    pub(super) session_context_provider_registered: bool,
    /// Agent context publishes received before `Ready`, in wire order.
    pub(super) agent_context_publishes: Vec<tau_proto::ExtAgentContextPublish>,
    /// Extension-level prompt fragments received before `Ready`, keyed by name
    /// so repeated publishes replace earlier staged content.
    pub(super) prompt_fragments: BTreeMap<String, PromptFragment>,
    /// Interceptor registration received before `Ready`. Registration is a
    /// replacement, so only the latest staged message matters.
    pub(super) intercept: Option<tau_proto::Intercept>,
    /// Per-agent context acknowledgements received before `Ready`, in wire
    /// order.
    pub(super) context_ready_events: Vec<tau_proto::ExtensionContextReady>,
    /// Session-wide context acknowledgements received before `Ready`, in wire
    /// order.
    pub(super) session_context_ready_events: Vec<tau_proto::ExtensionSessionContextReady>,
    /// Extension-started agent queries received before `Ready`, in wire order.
    pub(super) agent_queries: Vec<tau_proto::StartAgentRequest>,
    /// Generic extension emits/events withheld until `Ready`.
    pub(super) emitted_events: Vec<StagedExtensionPublish>,
    /// Operational protocol messages received after Ready but before the global
    /// activation barrier closed.
    pub(super) deferred_messages: Vec<DeferredExtensionMessage>,
    /// Number of retained declaration/operational frames charged to this stage.
    pub(super) retained_message_count: usize,
    /// Encoded bytes charged to this stage.
    pub(super) retained_message_bytes: usize,
}

/// Runtime state for extension process lifecycle and pre-`Ready` activation.
#[derive(Default)]
pub(crate) struct ExtensionRuntimeState {
    /// Every spawned or in-process extension, keyed by current `ConnectionId`.
    /// Supervises restart and shutdown. Lookups by connection id (the hot
    /// per-event path — every `Hello`, `Ready`, `Disconnected`) are O(1).
    pub(crate) entries: HashMap<tau_proto::ConnectionId, ExtensionEntry>,
    /// Join/watchdog ownership for each supervised extension writer.
    pub(super) supervised_writers: HashMap<tau_proto::ConnectionId, SupervisedWriterHandle>,
    /// Absolute watchdog deadlines for disconnected supervised writers.
    pub(super) cleanup_deadlines: HashMap<tau_proto::ConnectionId, Instant>,
    /// Absolute delayed-restart deadlines for disconnected tool extensions.
    pub(super) restart_deadlines: HashMap<tau_proto::ConnectionId, Instant>,
    /// Connections disabled only because the current session exhausted restart
    /// budget.
    pub(super) restart_budget_disabled: HashSet<tau_proto::ConnectionId>,
    /// Extension-originated state announced during handshake and withheld until
    /// the extension sends `Ready`. Activation happens in the main harness loop
    /// so prompt assembly, routing, and subscribers see the full batch at once.
    pub(super) activation_staging: HashMap<tau_proto::ConnectionId, ExtensionActivationStage>,
    /// Pre-`Ready` provider declarations admitted but not yet committed or
    /// dropped by interception.
    pub(super) pending_provider_model_declarations: HashMap<tau_proto::ConnectionId, usize>,
    /// Pre-`Ready` tool declarations admitted but not yet committed or dropped
    /// by interception.
    pub(super) pending_tool_lifecycle_declarations: HashMap<tau_proto::ConnectionId, usize>,
    /// Pre-`Ready` prompt-fragment declarations admitted but not yet committed
    /// or dropped by interception.
    pub(super) pending_prompt_fragment_declarations: HashMap<tau_proto::ConnectionId, usize>,
    /// Connections that sent `Ready` but are still waiting for the global
    /// initial collision preflight or their atomic stage activation.
    pub(super) ready_received: HashSet<tau_proto::ConnectionId>,
    /// Spawn-order list of connection ids into `entries`. Drives deterministic
    /// startup and shutdown loops that a `HashMap` alone cannot supply, and is
    /// updated in place whenever a supervised extension respawns with a fresh
    /// id.
    pub(crate) order: Vec<tau_proto::ConnectionId>,
    /// Number of queued extension connect commands not yet applied by the
    /// harness loop. Startup waits on this before treating an empty extension
    /// map as ready.
    pub(super) pending_connects: usize,
}
