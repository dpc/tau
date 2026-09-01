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
use crate::harness::SessionGeneration;

/// One extension's initial readiness deadline and availability policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StartupDeadline {
    /// Absolute time at which this extension must already have sent `Ready`.
    pub(super) deadline: Instant,
    /// Stable configured identity used for a timeout diagnostic before connect.
    pub(super) name: tau_proto::ExtensionName,
    /// Whether deadline expiry fails startup rather than disabling this peer.
    pub(super) require: bool,
}

/// Event payload staged while an extension is still handshaking.
#[derive(Clone, Debug)]
pub(super) struct StagedExtensionPublish {
    /// Event payload withheld until the source extension reaches `Ready`.
    pub(super) event: Event,
    /// Whether eligible staged semantic facts should enter durable history.
    pub(super) persist: bool,
}

/// Operational message withheld behind activation with its global arrival
/// order.
#[derive(Clone, Debug)]
pub(super) struct DeferredExtensionMessage {
    /// Monotonic harness-local arrival order.
    pub(super) order: u64,
    /// Session binding current when this operational frame arrived.
    pub(super) admission: ExtensionFrameAdmission,
    /// Owned protocol message replayed after activation.
    pub(super) message: HarnessInputMessage,
}

/// Immutable session binding captured when an extension frame arrives.
#[derive(Clone, Debug)]
pub(super) struct ExtensionFrameAdmission {
    /// Session id active at frame admission.
    pub(super) session_id: tau_proto::SessionId,
    /// In-process generation of that session binding.
    pub(super) session_generation: SessionGeneration,
}

/// One session-bound capability projection retained until extension activation.
#[derive(Clone, Debug)]
pub(super) struct StagedSessionBound<T> {
    /// Session binding captured from the declaration's committed publication.
    pub(super) admission: ExtensionFrameAdmission,
    /// Derived capability value to apply only while that binding remains
    /// current.
    pub(super) value: T,
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
    /// Latest complete session discovery replacement.
    pub(super) session_discovery_snapshot:
        Option<StagedSessionBound<tau_proto::ExtensionSessionDiscoverySnapshotDeclared>>,
    /// Latest complete discovery replacement per agent initialization.
    pub(super) agent_discovery_snapshots: BTreeMap<
        (tau_proto::AgentId, tau_proto::AgentInitializationId),
        StagedSessionBound<tau_proto::ExtensionAgentDiscoverySnapshotDeclared>,
    >,
    /// Session binding of the latest staged agent-context provider
    /// registration.
    pub(super) agent_context_provider_admission: Option<ExtensionFrameAdmission>,
    /// Session binding of the latest staged session-context provider
    /// registration.
    pub(super) session_context_provider_admission: Option<ExtensionFrameAdmission>,
    /// Session-bound agent context publishes received before `Ready`, in wire
    /// order.
    pub(super) agent_context_publishes: Vec<StagedSessionBound<tau_proto::ExtAgentContextPublish>>,
    /// Extension-level prompt fragments received before `Ready`, keyed by name
    /// so repeated publishes replace earlier staged content.
    pub(super) prompt_fragments: BTreeMap<String, PromptFragment>,
    /// Interceptor registration received before `Ready`. Registration is a
    /// replacement, so only the latest staged message matters.
    pub(super) intercept: Option<tau_proto::Intercept>,
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
    /// Immutable process-local sink for opt-in supervised-child stderr
    /// mirroring.
    pub(crate) stderr_mirror: Option<crate::extension_stderr_mirror::ExtensionStderrMirror>,
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
    /// Initial readiness deadlines, recorded at supervised spawn or, for
    /// externally managed peers, from the one startup-wait instant. Entries
    /// disappear at first `Ready` or disconnect.
    pub(super) startup_deadlines: HashMap<tau_proto::ConnectionId, StartupDeadline>,
    /// One general deadline established when initial startup begins, used for
    /// queued and externally managed peers that lack a supervised spawn record.
    pub(super) startup_wait_deadline: Option<Instant>,
    /// Optional peers that expired before their queued connect command reached
    /// the harness. They are disabled as soon as that command installs them.
    pub(super) expired_startup_connects: HashMap<tau_proto::ConnectionId, StartupDeadline>,
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
    /// Pre-`Ready` Action snapshots admitted but not yet committed or dropped.
    pub(super) pending_action_schema_declarations: HashMap<tau_proto::ConnectionId, usize>,
    /// Pre-`Ready` prompt-fragment declarations admitted but not yet committed
    /// or dropped by interception.
    pub(super) pending_prompt_fragment_declarations: HashMap<tau_proto::ConnectionId, usize>,
    /// Pre-`Ready` session-discovery declarations admitted but not yet
    /// committed or dropped by interception.
    pub(super) pending_session_discovery_declarations: HashMap<tau_proto::ConnectionId, usize>,
    /// Pre-`Ready` per-agent context declarations admitted but not yet
    /// committed or dropped by interception.
    pub(super) pending_agent_context_declarations: HashMap<tau_proto::ConnectionId, usize>,
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
    /// Whether initial deterministic tool-collision preflight has completed.
    pub(super) initial_tool_preflight_complete: bool,
    /// Whether startup collision losers are being disconnected.
    pub(super) resolving_initial_collisions: bool,
    /// Next arrival order for operational frames deferred behind activation.
    pub(super) next_deferred_message_order: u64,
    /// Names enabled by final startup resolution, including skipped optionals.
    pub(crate) enabled_names: std::collections::BTreeSet<String>,
}
