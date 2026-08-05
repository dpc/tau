//! Directional harness protocol messages.
//!
//! [`HarnessInputMessage`] is the set of messages the harness accepts from
//! peers. [`HarnessOutputMessage`] is the set of messages the harness sends to
//! peers. Events are never top-level protocol items: peer-authored events are
//! wrapped in [`HarnessInputMessage::Emit`], and harness deliveries are wrapped
//! in [`HarnessOutputMessage::Deliver`].
//!
//! Wire form: `{"message": "hello", "payload": {...}}` — flat, lower
//! snake_case names, distinct from [`crate::Event`]'s `{"event":
//! "tool.started", "payload": {...}}` shape.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::{fmt, time as path_std_time};

use serde::{Deserialize, Serialize};

use crate::{
    AgentId, AgentMessageId, AgentMessageKind, CborValue, ClientKind, Event, EventSelector,
    ExtensionName, InterceptionPriority, NoticeLevel, SessionId, ToolDefinition, ToolNamePrefix,
};

// ---------------------------------------------------------------------------
// Lifecycle messages
// ---------------------------------------------------------------------------

/// An authenticated peer's declared authority for optional protocol families.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerCapability {
    /// Publish external-message reports for downstream canonicalization.
    MessageBridge,
}

/// Announcement sent by a participant after connecting.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    /// Protocol version understood by the connecting peer.
    pub protocol_version: u32,
    /// Stable name used to identify the connecting peer.
    pub client_name: ExtensionName,
    /// Authority class requested by the connecting peer.
    pub client_kind: ClientKind,
    /// Session a UI expects this connection to enter.
    ///
    /// The harness rejects a UI connection when this value does not match its
    /// current session. Non-UI peers must omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_session_id: Option<SessionId>,
    /// Optional protocol authorities declared for this connection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<PeerCapability>,
}

/// Harness acknowledgement that binds an admitted UI connection to a session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiSessionAccepted {
    /// Session identity verified during UI connection admission.
    pub session_id: SessionId,
}

/// Subscription request describing which events a participant wants.
///
/// Historical selectors opt in to catch-up delivered with
/// [`EventDelivery::replay`] set to `true`, including durable facts and current
/// state snapshots. Live selectors opt in to future
/// committed deliveries after catch-up has completed. Keeping these sets
/// separate prevents restore-only state from widening live side-effect
/// exposure, and prevents live-only handlers from implicitly receiving
/// historical tool execution facts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Subscribe {
    /// Durable/restorable facts and current snapshots to replay before the live
    /// stream is released.
    #[serde(default)]
    pub historical_selectors: Vec<EventSelector>,
    /// Future committed events to deliver after catch-up.
    #[serde(default)]
    pub live_selectors: Vec<EventSelector>,
}

/// Interception request describing which event emissions a participant wants
/// to handle before they reach the event log and regular subscribers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Intercept {
    pub selectors: Vec<EventSelector>,
    pub priority: InterceptionPriority,
}

/// Readiness notification emitted after startup or handshake.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Ready {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Disconnect notification with an optional human-readable reason.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Disconnect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Configuration handed to an extension at startup. Sent
/// point-to-point from the harness to the extension immediately
/// after the harness sees the extension's
/// [`Hello`](crate::Hello). Carries whatever the
/// `config: { … }` value was for that extension in `harness.yaml`,
/// or [`CborValue::Null`] / an empty map when no config was
/// provided. `state_dir` is the harness-assigned persistent state
/// directory for this extension instance, when the harness can provide
/// one.
///
/// `Eq` is not derivable because the underlying CBOR value can
/// contain floats; `PartialEq` is enough for tests.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Configure {
    /// Free-form extension configuration from harness settings.
    pub config: CborValue,
    /// Stable configured extension instance name.
    pub instance_name: ExtensionName,
    /// Optional immutable prefix assigned to structural tool identifiers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_prefix: Option<ToolNamePrefix>,
    /// Persistent directory reserved for this extension's runtime state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_dir: Option<PathBuf>,
    /// Secret values explicitly authorized for this extension.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, SecretValue>,
    /// Bounded immutable startup snapshot of CLI-owned non-secret settings
    /// files.
    ///
    /// Keys are sanitized relative file names. The harness captures this map
    /// once before configuration; changes become visible after extension
    /// restart.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings_files: BTreeMap<String, Vec<u8>>,
}

/// Secret text passed from the harness to one authorized extension.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretValue(String);

impl SecretValue {
    /// Wrap a resolved secret value for protocol transport.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the underlying secret text. Avoid logging this value.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Reported by an extension when its
/// [`Configure`](Configure) value is malformed (or
/// otherwise unusable). The harness surfaces the message just like
/// a `harness.yaml` parse error so the user can see why their
/// per-extension config was rejected.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConfigError {
    pub message: String,
}

/// Request from a configured extension to show one routine user-facing notice.
///
/// The harness owns the resulting [`crate::HarnessNotice`] kind, visibility,
/// publication source, and live-only delivery policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionNoticeRequest {
    /// Human-readable notice text.
    pub message: String,
    /// Requested severity. The harness caps [`NoticeLevel::Critical`] to
    /// [`NoticeLevel::Warning`].
    pub level: NoticeLevel,
}

// ---------------------------------------------------------------------------
// Wire transport — event delivery
// ---------------------------------------------------------------------------

/// Wall-clock timestamp as microseconds since the UNIX epoch.
///
/// Stamped onto persisted session events and the JSONL debug log so
/// offline inspection can compute inter-event gaps, RPM bursts, and
/// correlations with provider-side cache misses. `u64` µs covers
/// ~584,000 years past 1970, so saturation is not a concern in
/// practice — callers still saturate on bogus clocks rather than
/// panic, keeping the persistence path infallible. A zero value
/// marks records written before this field existed
/// (`#[serde(default)]` on the carrying struct).
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct UnixMicros(u64);

impl UnixMicros {
    #[must_use]
    pub fn new(v: u64) -> Self {
        Self(v)
    }

    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }

    /// Reads the current wall clock and returns a `UnixMicros`.
    /// Saturates on bogus clocks (pre-1970 or post-2554) instead of
    /// panicking, so callers on the durable-write path can stay
    /// infallible.
    #[must_use]
    pub fn now() -> Self {
        let micros = path_std_time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        Self(micros)
    }
}

impl std::fmt::Display for UnixMicros {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A bus event delivered by the harness to one peer.
///
/// The protocol no longer has a bare top-level event lane. Every event the
/// harness sends to a peer is wrapped in [`HarnessOutputMessage::Deliver`] with
/// this payload so delivery metadata is explicitly harness-owned and
/// direction-specific.
///
/// `replay` distinguishes catch-up input from live occurrence: subscribe-time
/// catch-up sends durable facts and current snapshots with `replay: true`.
/// Consumers that render state (UI transcripts) fold replay frames like live
/// events; consumers that perform side effects (sounds, tool execution, idle
/// timers) must skip them.
///
/// `recorded_at` is present for committed runtime deliveries and for replay
/// entries when a timestamp is meaningful. Synthetic catch-up snapshots use a
/// harness-generated catch-up timestamp.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventDelivery {
    /// Inner bus fact delivered to the peer.
    pub event: Box<Event>,
    /// True when this delivery re-sends a durable historical fact to a late
    /// subscriber instead of announcing a live occurrence.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replay: bool,
    /// Runtime or historical append timestamp associated with the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at: Option<UnixMicros>,
}

impl EventDelivery {
    /// Creates a direct delivery for a synthesized current-state
    /// announcement (no meaningful append timestamp).
    #[must_use]
    pub fn direct(event: Event) -> Self {
        Self {
            event: Box::new(event),
            replay: false,
            recorded_at: None,
        }
    }

    /// Creates a delivery for a live committed runtime event.
    #[must_use]
    pub fn live(recorded_at: UnixMicros, event: Event) -> Self {
        Self {
            event: Box::new(event),
            replay: false,
            recorded_at: Some(recorded_at),
        }
    }

    /// Creates a replay delivery for a historical fact or catch-up snapshot
    /// with its timestamp.
    #[must_use]
    pub fn replay(recorded_at: UnixMicros, event: Event) -> Self {
        Self {
            event: Box::new(event),
            replay: true,
            recorded_at: Some(recorded_at),
        }
    }

    /// Returns the inner delivered event.
    #[must_use]
    pub fn event(&self) -> &Event {
        &self.event
    }

    /// Returns true when this delivery re-sends a durable historical fact to
    /// a late subscriber instead of announcing a live occurrence.
    ///
    /// Replay frames describe the past. Consumers that render state (UI
    /// transcripts) fold them like live events; consumers that perform side
    /// effects (sounds, tool execution, idle timers) must skip them.
    #[must_use]
    pub fn is_replay(&self) -> bool {
        self.replay
    }

    /// Consumes this delivery and returns the inner event.
    #[must_use]
    pub fn into_event(self) -> Event {
        *self.event
    }

    /// Consumes this delivery and returns the event, the replay marker, and
    /// the append timestamp.
    #[must_use]
    pub fn into_parts(self) -> (Event, bool, Option<UnixMicros>) {
        (*self.event, self.replay, self.recorded_at)
    }
}

/// Extension/client request to emit one event with harness-owned delivery
/// metadata.
///
/// The inner `event` is the fact that subscribers see. `persist` requests that
/// the harness write eligible semantic facts to durable session or agent event
/// history; it is not part of the emitted fact itself.
///
/// `Emit` is strictly for peer → harness event emission. Harness → peer event
/// delivery uses [`HarnessOutputMessage::Deliver`] instead.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Emit {
    /// Event the peer asks the harness to publish.
    pub event: Box<Event>,
    /// Whether eligible semantic facts should enter durable logs.
    pub persist: bool,
}

/// Typed recipient authority for one cross-harness agent message.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalAgentMessageRecipient {
    /// Resolve exactly one endpoint through the target's configured entrypoint.
    BareEntrypoint,
    /// Deliver to a caller-known exact agent id, independent of entrypoint
    /// policy.
    Exact(AgentId),
}

/// Peer-to-harness RPC that asks the active target harness to publish a
/// harness-owned external agent-message delivery.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExternalAgentMessageRequest {
    /// Sender-generated request correlation id.
    pub request_id: String,
    /// Stable logical message id shared by sender and recipient projections.
    pub message_id: AgentMessageId,
    /// Sender-minted bearer capability authorizing this exact outbound message.
    ///
    /// The recipient harness validates this capability by calling back to the
    /// claimed sender harness before publishing the inbound projection.
    pub capability: String,
    /// Active session id of the sending harness.
    pub sender_session_id: SessionId,
    /// Agent id of the sender in the sending harness.
    pub sender_id: AgentId,
    /// Active session id expected on the receiving harness.
    pub recipient_session_id: SessionId,
    /// Typed bare-entrypoint or exact recipient authority.
    pub recipient: ExternalAgentMessageRecipient,
    /// Delivery source semantics.
    #[serde(default, skip_serializing_if = "AgentMessageKind::is_default")]
    pub kind: AgentMessageKind,
    /// Message body.
    pub message: String,
}

/// Peer-to-harness RPC that asks a claimed sender harness to authenticate an
/// external agent-message request before the recipient records it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExternalAgentMessageAuthRequest {
    /// Recipient-generated request correlation id.
    pub request_id: String,
    /// Stable logical message id being authenticated.
    pub message_id: AgentMessageId,
    /// Bearer capability copied from the external message request.
    pub capability: String,
    /// Active session id of the claimed sending harness.
    pub sender_session_id: SessionId,
    /// Claimed sender agent id.
    pub sender_id: AgentId,
    /// Active session id expected on the receiving harness.
    pub recipient_session_id: SessionId,
    /// Claimed typed bare-entrypoint or exact recipient authority.
    pub recipient: ExternalAgentMessageRecipient,
    /// Claimed delivery source semantics.
    #[serde(default, skip_serializing_if = "AgentMessageKind::is_default")]
    pub kind: AgentMessageKind,
    /// Claimed message body. The sender harness compares this with its pending
    /// outbound message so the capability cannot be replayed with altered text.
    pub message: String,
}

/// Harness-to-peer response for an external agent-message authentication RPC.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExternalAgentMessageAuthResult {
    /// Request correlation id copied from the request.
    pub request_id: String,
    /// True when the claimed sender harness has a matching pending outbound
    /// message capability.
    pub authorized: bool,
    /// Optional bounded diagnostic explaining why authorization failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Harness-to-peer response for an external agent-message RPC.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExternalAgentMessageResult {
    /// Request correlation id copied from the request.
    pub request_id: String,
    /// Empty on success; otherwise a bounded user-facing delivery error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Canonical resolved recipient on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_id: Option<AgentId>,
    /// Whether resolving this request started the recipient.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub started: bool,
}

/// Narrow live-harness probe used by bounded session discovery.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PeerSessionProbe {
    /// Caller-generated correlation id.
    pub request_id: String,
    /// Active session id advertised by runtime metadata.
    pub session_id: SessionId,
}

/// Harness-authored answer to a peer-session discovery probe.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PeerSessionProbeResult {
    /// Correlation id copied from the request.
    pub request_id: String,
    /// True only when this live harness is on the requested session and accepts
    /// bare inter-session messages.
    pub available: bool,
}

impl Emit {
    /// Creates a durable-by-default emit request.
    #[must_use]
    pub fn new(event: Event) -> Self {
        Self {
            event: Box::new(event),
            persist: true,
        }
    }

    /// Creates an emit request with explicit persistence metadata.
    #[must_use]
    pub fn with_persist(event: Event, persist: bool) -> Self {
        Self {
            event: Box::new(event),
            persist,
        }
    }

    /// Consumes this request and returns the inner event plus persistence flag.
    #[must_use]
    pub fn into_parts(self) -> (Event, bool) {
        (*self.event, self.persist)
    }
}

/// Directed harness → interceptor message carrying an event emission that has
/// not reached the event log yet. The interceptor must reply with an
/// [`InterceptReply`]; until it does, the harness suspends draining of any
/// further publishes that would themselves be subject to interception.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InterceptRequest {
    /// Event being offered to the interceptor.
    pub event: Box<Event>,
    /// Original persistence metadata from the publish request.
    pub persist: bool,
}

/// What an interceptor wants the harness to do with the event it was given.
///
/// `Pass(None)` republishes the original event unchanged (the common no-op
/// case). `Pass(Some(event))` substitutes a possibly-mutated version that flows
/// on through any remaining interceptors and then to subscribers. `Drop`
/// discards the event entirely — but the harness may override `Drop` for events
/// the publisher marked `must_pass`, `tracing::warn!`-ing and falling back to
/// the original.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum InterceptAction {
    Pass(Option<Box<Event>>),
    Drop,
}

/// Interceptor → harness response to an [`InterceptRequest`]. Exactly one reply
/// per request; out-of-order or duplicate replies are a programming error and
/// the harness logs + falls back to the original event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InterceptReply {
    pub action: InterceptAction,
}

/// Best-effort request for a materialized full `agent.prompt_created` payload
/// by id.
///
/// Prompt-created payloads are transient delivery objects; harnesses are not
/// required to retain them after live delivery. A missing prompt is reported as
/// `None` in [`AgentPromptCreatedResult::prompt`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GetAgentPromptCreated {
    /// Request correlation id echoed by [`AgentPromptCreatedResult`].
    pub request_id: String,
    /// Session containing the requested prompt.
    pub session_id: crate::SessionId,
    /// Prompt to materialize.
    pub agent_prompt_id: crate::AgentPromptId,
}

/// Response to [`GetAgentPromptCreated`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentPromptCreatedResult {
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<crate::AgentPromptCreated>,
}

/// Request that the harness render the effective system prompt for one role.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GetRenderedSystemPrompt {
    /// Request correlation id echoed by [`RenderedSystemPromptResult`].
    pub request_id: String,
    /// Role name whose resolved prompt should be rendered.
    pub role: String,
}

/// Response to [`GetRenderedSystemPrompt`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RenderedSystemPromptResult {
    /// Request correlation id copied from the request.
    pub request_id: String,
    /// Rendered prompt when the role exists and template rendering succeeds.
    /// Exactly one of `prompt` and `error` should be present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Human-readable failure when the role is unknown or rendering fails.
    /// Exactly one of `prompt` and `error` should be present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Request that the harness render the effective provider-visible prompt
/// context for a role.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GetRenderedPrompt {
    /// Request correlation id echoed by [`RenderedPromptResult`].
    pub request_id: String,
    /// Explicit role whose resolved prompt should be rendered.
    ///
    /// When absent, the harness renders for its currently selected role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Include harness-injected AGENTS.md context.
    #[serde(default = "default_true")]
    pub enable_agents_md: bool,
}

/// Response to [`GetRenderedPrompt`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RenderedPromptResult {
    /// Request correlation id copied from the request.
    pub request_id: String,
    /// Rendered prompt context when the role exists and template rendering
    /// succeeds. Exactly one of `prompt` and `error` should be present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Human-readable failure when the role is unknown or rendering fails.
    /// Exactly one of `prompt` and `error` should be present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Request that the harness report the effective tools for one role.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GetRenderedToolDefinitions {
    /// Request correlation id echoed by [`RenderedToolDefinitionsResult`].
    pub request_id: String,
    /// Explicit role whose resolved tool list should be reported.
    ///
    /// When absent, the harness reports tools for its currently selected role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// Response to [`GetRenderedToolDefinitions`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenderedToolDefinitionsResult {
    /// Request correlation id copied from the request.
    pub request_id: String,
    /// Effective provider-facing tool definitions for the requested role.
    /// Exactly one of `tools` and `error` should be present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    /// Human-readable failure when the role is unknown.
    /// Exactly one of `tools` and `error` should be present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Agent-roster scope requested from the currently bound harness session.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAgentListScope {
    /// Return current members, including members whose agent is unavailable.
    Current,
    /// Return current members plus previously loaded and now-unloaded members.
    History,
}

/// Current-session lifecycle of one listed agent.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SessionAgentLifecycle {
    /// The current member has a live harness agent and navigation snapshot.
    Live {
        /// Current outer-turn runtime state.
        runtime_state: crate::AgentRuntimeState,
        /// Current harness-owned navigation mode.
        navigation_mode: crate::AgentNavigationMode,
    },
    /// The session currently contains the agent, but no live harness agent
    /// exists.
    Unavailable,
    /// The agent was previously loaded and its latest membership state is
    /// unloaded.
    Unloaded,
}

/// Persistence policy recorded for one session agent.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAgentPersistence {
    /// Membership and transcript facts are durable.
    Durable,
    /// Membership and transcript facts exist only in the current daemon.
    Ephemeral,
}

/// Read-only creation-fact enrichment for one listed agent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SessionAgentFacts {
    /// A valid matching first `agent.started` record was read.
    Available {
        /// Creation timestamp from the persisted first record.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_at: Option<crate::UnixMicros>,
        /// Parent recorded by the immutable creation fact.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_agent: Option<AgentId>,
        /// Role recorded by the immutable creation fact.
        role: String,
        /// Display name from current memory or a journal-bound checkpoint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
    },
    /// No creation journal or first record exists.
    Missing,
    /// The first record exists but is not a valid matching creation fact.
    Invalid,
    /// Bounded read, decoding, or I/O prevented classification.
    Unreadable,
}

/// One content-minimized agent-roster entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionAgentListEntry {
    /// Stable agent id.
    pub agent_id: AgentId,
    /// Current-session lifecycle.
    pub lifecycle: SessionAgentLifecycle,
    /// Agent transcript and membership persistence.
    pub persistence: SessionAgentPersistence,
    /// Bounded creation-fact enrichment.
    pub facts: SessionAgentFacts,
    /// Current runtime-only canonical work status, absent without a live agent.
    pub work_status: Option<SessionAgentWorkStatus>,
}

/// Current canonical self-reported work status for one live roster agent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    try_from = "SessionAgentWorkStatusWire",
    into = "SessionAgentWorkStatusWire"
)]
pub struct SessionAgentWorkStatus {
    /// Closed self-reported task phase.
    phase: crate::AgentWorkStatusPhase,
    /// Canonical model-authored title: absent for `Unreported`; required for
    /// `Working`, `Done`, and `Blocked`; optional valid last title for
    /// `Unknown`. Every present title is nonempty, trimmed, single-line,
    /// control-free, and at most 160 UTF-8 bytes.
    ///
    /// Consumers must treat the title as untrusted display metadata.
    title: Option<String>,
}

impl Default for SessionAgentWorkStatus {
    fn default() -> Self {
        Self {
            phase: crate::AgentWorkStatusPhase::Unreported,
            title: None,
        }
    }
}

impl SessionAgentWorkStatus {
    /// Validates one canonical work-status phase and optional task title.
    ///
    /// # Errors
    ///
    /// Returns an error when the phase/title combination is invalid or a title
    /// is not a canonical task name.
    pub fn new(phase: crate::AgentWorkStatusPhase, title: Option<String>) -> Result<Self, String> {
        let title_shape_valid = match phase {
            crate::AgentWorkStatusPhase::Unreported => title.is_none(),
            crate::AgentWorkStatusPhase::Working
            | crate::AgentWorkStatusPhase::Done
            | crate::AgentWorkStatusPhase::Blocked => title.is_some(),
            crate::AgentWorkStatusPhase::Unknown => true,
        };
        if !title_shape_valid {
            return Err(
                "work-status title must be absent for unreported and present for working, done, and blocked"
                    .to_owned(),
            );
        }
        if title.as_ref().is_some_and(|title| {
            title.is_empty()
                || 160 < title.len()
                || title.trim() != title
                || title.chars().any(|character| {
                    character.is_control() || matches!(character, '\u{2028}' | '\u{2029}')
                })
        }) {
            return Err(
                "work-status title must be nonempty, trimmed, one line, control-free, and at most 160 UTF-8 bytes"
                    .to_owned(),
            );
        }
        Ok(Self { phase, title })
    }

    /// Returns the closed canonical work-status phase.
    #[must_use]
    pub fn phase(&self) -> crate::AgentWorkStatusPhase {
        self.phase
    }

    /// Returns the canonical task title, if this phase retains one.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }
}

/// Wire form used to validate a work-status snapshot before exposing it.
#[derive(Serialize, Deserialize)]
struct SessionAgentWorkStatusWire {
    /// Closed self-reported task phase.
    phase: crate::AgentWorkStatusPhase,
    /// Potential canonical model-authored task title.
    title: Option<String>,
}

impl From<SessionAgentWorkStatus> for SessionAgentWorkStatusWire {
    fn from(status: SessionAgentWorkStatus) -> Self {
        Self {
            phase: status.phase,
            title: status.title,
        }
    }
}

impl TryFrom<SessionAgentWorkStatusWire> for SessionAgentWorkStatus {
    type Error = String;

    fn try_from(status: SessionAgentWorkStatusWire) -> Result<Self, Self::Error> {
        Self::new(status.phase, status.title)
    }
}

/// Stable whole-request error category for an agent-roster snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAgentListErrorKind {
    /// The request did not name the harness's currently bound session.
    StaleSession,
    /// The harness's maintained membership projection is inconsistent.
    SessionRead,
    /// The distinct member count exceeded the fixed response bound.
    TooManyAgents,
    /// Bounded per-agent enrichment exceeded its aggregate read budget.
    EnrichmentTooLarge,
    /// The encoded successful response exceeded the protocol message bound.
    ResponseTooLarge,
}

/// Whole-request agent-roster error.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionAgentListError {
    /// Stable machine-readable category.
    pub kind: SessionAgentListErrorKind,
    /// Bounded user-facing detail.
    pub message: String,
}

/// Request for the harness's authoritative current session identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GetCurrentSession {
    /// Caller-generated correlation id.
    pub request_id: String,
}

/// Directed response to [`GetCurrentSession`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CurrentSessionResult {
    /// Correlation id copied from the request.
    pub request_id: String,
    /// Harness-owned current session id at request handling time.
    pub session_id: SessionId,
    /// Absolute canonical project root captured when the harness started.
    pub project_root: PathBuf,
}

/// Request for a content-minimized roster of the currently bound session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GetSessionAgentList {
    /// Caller-generated correlation id.
    pub request_id: String,
    /// Exact currently bound session expected by the caller.
    pub session_id: SessionId,
    /// Current-only or complete membership-history scope.
    pub scope: SessionAgentListScope,
}

/// Directed response to [`GetSessionAgentList`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionAgentListResult {
    /// Correlation id copied from the request.
    pub request_id: String,
    /// Session id copied from the request.
    pub session_id: SessionId,
    /// Complete success rows or one whole-request error.
    pub result: SessionAgentListResultPayload,
}

/// Success or whole-request failure for one agent-roster snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SessionAgentListResultPayload {
    /// The complete row set.
    Ok {
        /// Every row in the requested scope.
        agents: Vec<SessionAgentListEntry>,
    },
    /// The snapshot failed atomically.
    Error {
        /// Stable typed failure.
        error: SessionAgentListError,
    },
}

// ---------------------------------------------------------------------------
// Extension data RPC
// ---------------------------------------------------------------------------

/// Harness-owned storage scope for extension data RPC requests.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionDataScope {
    /// Session-local data under `<session_data_dir>/ext/data/<ext-name>`.
    ///
    /// The harness rejects this scope in session-ephemeral mode because there
    /// is no durable session data directory.
    Session,
    /// User-persistent data under `~/.local/state/tau/ext/<ext-name>`.
    User,
    /// User cache data under `~/.cache/tau/ext/<ext-name>`.
    Cache,
    /// Durable credential data under the harness-only
    /// `~/.local/state/tau/secrets/ext/<ext-name>` root.
    ///
    /// The harness never exposes this root directly to the extension and denies
    /// this scope to memory-only and in-process extensions.
    Secret,
}

/// Extension request for harness-mediated file access inside its data roots.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionDataRequest {
    /// Request correlation id echoed by [`ExtensionDataResult`].
    pub request_id: String,
    /// Storage scope to access.
    pub scope: ExtensionDataScope,
    /// File operation to perform.
    pub op: ExtensionDataRequestOp,
}

impl std::fmt::Debug for ExtensionDataRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExtensionDataRequest")
            .field("request_id", &self.request_id)
            .field("scope", &self.scope)
            .field("op", &self.op)
            .finish()
    }
}

/// Extension-provided path text inside an extension data scope.
///
/// The wire format is a plain string for compatibility. This newtype marks
/// fields that must be interpreted as extension-data paths. Constructors and
/// deserialization do not validate or sanitize the text; the harness must
/// validate and sanitize it before filesystem access.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExtensionDataPath(String);

impl ExtensionDataPath {
    /// Creates a raw path value from extension-provided path text.
    ///
    /// This performs no validation or sanitization.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    /// Borrows the raw path text as carried on the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the wrapper and returns the raw path string for harness
    /// validation or storage-specific path handling.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl From<String> for ExtensionDataPath {
    fn from(path: String) -> Self {
        Self::new(path)
    }
}

impl From<&str> for ExtensionDataPath {
    fn from(path: &str) -> Self {
        Self::new(path)
    }
}

impl AsRef<str> for ExtensionDataPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// File operation requested by an extension data RPC.
///
/// The harness may reject operations with
/// [`ExtensionDataErrorKind::QuotaExceeded`] when file contents or directory
/// listings exceed harness-owned resource limits.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ExtensionDataRequestOp {
    /// Read one whole file at an extension-provided path, subject to harness
    /// path validation and file-size quota.
    ReadFile { path: ExtensionDataPath },
    /// Write one whole file at an extension-provided path, atomically replacing
    /// any old content and subject to harness path validation and file-size
    /// quota.
    WriteFile {
        /// Relative file path inside the selected extension data scope.
        path: ExtensionDataPath,
        /// Complete replacement file contents.
        contents: Vec<u8>,
    },
    /// Replace one complete file only when its current BLAKE3 generation
    /// matches.
    CompareAndSwapFile {
        /// Relative file path inside the selected extension data scope.
        path: ExtensionDataPath,
        /// Lowercase BLAKE3 digest of the complete expected current contents.
        expected_generation: String,
        /// Complete replacement file contents.
        contents: Vec<u8>,
    },
    /// Create one whole file at an extension-provided path, failing when the
    /// file already exists and subject to harness path validation and file-size
    /// quota.
    CreateFile {
        /// Relative file path inside the selected extension data scope.
        path: ExtensionDataPath,
        /// Initial file contents.
        contents: Vec<u8>,
    },
    /// Append bytes to one file at an extension-provided path, creating it when
    /// missing and subject to harness path validation and file-size quota.
    AppendFile {
        /// Relative file path inside the selected extension data scope.
        path: ExtensionDataPath,
        /// Bytes to append.
        contents: Vec<u8>,
    },
    /// Delete one file at an extension-provided path after harness validation.
    /// Missing files succeed.
    DeleteFile { path: ExtensionDataPath },
    /// Rename one file between extension-provided paths after harness
    /// validation. The destination must not already exist.
    RenameFile {
        /// Source relative file path inside the selected extension data scope.
        from: ExtensionDataPath,
        /// Destination relative file path inside the selected extension data
        /// scope.
        to: ExtensionDataPath,
    },
    /// List direct children of an extension-provided directory path after
    /// harness validation, subject to the harness directory-entry quota.
    ListFiles { path: ExtensionDataPath },
}

impl std::fmt::Debug for ExtensionDataRequestOp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadFile { path } => formatter
                .debug_struct("ReadFile")
                .field("path", path)
                .finish(),
            Self::WriteFile { path, contents } => formatter
                .debug_struct("WriteFile")
                .field("path", path)
                .field("contents_len", &contents.len())
                .finish(),
            Self::CompareAndSwapFile {
                path,
                expected_generation,
                contents,
            } => formatter
                .debug_struct("CompareAndSwapFile")
                .field("path", path)
                .field("expected_generation", expected_generation)
                .field("contents_len", &contents.len())
                .finish(),
            Self::CreateFile { path, contents } => formatter
                .debug_struct("CreateFile")
                .field("path", path)
                .field("contents_len", &contents.len())
                .finish(),
            Self::AppendFile { path, contents } => formatter
                .debug_struct("AppendFile")
                .field("path", path)
                .field("contents_len", &contents.len())
                .finish(),
            Self::DeleteFile { path } => formatter
                .debug_struct("DeleteFile")
                .field("path", path)
                .finish(),
            Self::RenameFile { from, to } => formatter
                .debug_struct("RenameFile")
                .field("from", from)
                .field("to", to)
                .finish(),
            Self::ListFiles { path } => formatter
                .debug_struct("ListFiles")
                .field("path", path)
                .finish(),
        }
    }
}
/// Harness response to an [`ExtensionDataRequest`].
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionDataResult {
    /// Request correlation id copied from the request.
    pub request_id: String,
    /// Operation result or human-readable error.
    pub result: ExtensionDataResultPayload,
}

impl std::fmt::Debug for ExtensionDataResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExtensionDataResult")
            .field("request_id", &self.request_id)
            .field("result", &self.result)
            .finish()
    }
}

/// Result payload for an extension data RPC.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ExtensionDataResultPayload {
    /// Operation succeeded.
    Ok { value: ExtensionDataValue },
    /// Operation failed.
    Error {
        /// Machine-readable error kind.
        #[serde(default = "default_extension_data_error_kind")]
        kind: ExtensionDataErrorKind,
        /// Human-readable error details.
        message: String,
    },
}

impl std::fmt::Debug for ExtensionDataResultPayload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok { value } => formatter.debug_tuple("Ok").field(value).finish(),
            Self::Error { kind, message } => formatter
                .debug_struct("Error")
                .field("kind", kind)
                .field("message", message)
                .finish(),
        }
    }
}

/// Successful value returned by an extension data RPC.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ExtensionDataValue {
    /// Whole file contents from a read request.
    ReadFile { contents: Vec<u8> },
    /// Empty success marker for a write request.
    WriteFile,
    /// Empty success marker for a compare-and-swap request.
    CompareAndSwapFile,
    /// Empty success marker for a create request.
    CreateFile,
    /// Empty success marker for an append request.
    AppendFile,
    /// Empty success marker for a delete request.
    DeleteFile,
    /// Empty success marker for a rename request.
    RenameFile,
    /// Direct child entries from a list request.
    ListFiles { entries: Vec<ExtensionDataEntry> },
}

impl std::fmt::Debug for ExtensionDataValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadFile { contents } => formatter
                .debug_struct("ReadFile")
                .field("contents_len", &contents.len())
                .finish(),
            Self::WriteFile => formatter.write_str("WriteFile"),
            Self::CompareAndSwapFile => formatter.write_str("CompareAndSwapFile"),
            Self::CreateFile => formatter.write_str("CreateFile"),
            Self::AppendFile => formatter.write_str("AppendFile"),
            Self::DeleteFile => formatter.write_str("DeleteFile"),
            Self::RenameFile => formatter.write_str("RenameFile"),
            Self::ListFiles { entries } => formatter
                .debug_struct("ListFiles")
                .field("entries", entries)
                .finish(),
        }
    }
}

/// Machine-readable extension data RPC error kind.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionDataErrorKind {
    /// Requested path or ancestor does not exist.
    NotFound,
    /// Exclusive create requested for an existing file.
    AlreadyExists,
    /// Path failed extension data path validation.
    InvalidPath,
    /// Requested file operation targeted a directory or non-file.
    NotFile,
    /// Requested directory operation targeted a file or non-directory.
    NotDir,
    /// Permission denied by the operating system or harness policy.
    Permission,
    /// Operation exceeded a harness-enforced resource quota.
    QuotaExceeded,
    /// Compare-and-swap expected generation did not match the current file.
    GenerationMismatch,
    /// Any other I/O or harness-side error.
    Io,
}

fn default_extension_data_error_kind() -> ExtensionDataErrorKind {
    ExtensionDataErrorKind::Io
}
/// One direct child returned by an extension data list request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionDataEntry {
    /// Path relative to the requested scope root, validated by the harness and
    /// carried as the same string wire shape as request paths.
    pub path: ExtensionDataPath,
    /// True when this entry is a directory.
    pub is_dir: bool,
    /// File size in bytes for files. Directories use `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub len: Option<u64>,
}

// ---------------------------------------------------------------------------
// Directional protocol envelopes
// ---------------------------------------------------------------------------

/// Dedicated UI input requesting current protocol frame I/O stats for one
/// configured extension.
///
/// The harness consumes this debug request directly and replies only to the
/// requesting UI connection. It is not a durable session fact and must not be
/// broadcast to extensions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiDebugEventStatsRequest {
    /// Configured extension name to inspect.
    pub extension_name: ExtensionName,
}

/// Dedicated UI input requesting that the daemon outlive this UI connection.
///
/// The harness consumes this connection-control request directly. It is not a
/// session fact and must not be broadcast to extensions.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiDetachRequest {}

/// Dedicated UI input requesting an agent's user-facing prompt rewind anchors.
///
/// The harness consumes this request directly and replies only to the
/// requesting UI connection. It is not a session fact and must not be
/// broadcast.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiTreeRequest {
    /// Session whose agent tree should be rendered.
    pub session_id: SessionId,
    /// Target agent tree to render. `None` leaves selection to the harness's
    /// current/default conversation state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent_id: Option<AgentId>,
}

/// Messages the harness accepts from connected peers (UI clients and
/// extensions).
///
/// Wire form is `{"message": "<flat_name>", "payload": {...}}`. Event
/// emission is represented by [`HarnessInputMessage::Emit`]; a bare serialized
/// [`Event`] is not a valid harness input message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "message", content = "payload", rename_all = "snake_case")]
pub enum HarnessInputMessage {
    Hello(Hello),
    Subscribe(Subscribe),
    Intercept(Intercept),
    Ready(Ready),
    Disconnect(Disconnect),
    ConfigError(ConfigError),
    ExtensionNoticeRequest(ExtensionNoticeRequest),
    Emit(Emit),
    InterceptReply(InterceptReply),
    GetAgentPromptCreated(GetAgentPromptCreated),
    GetRenderedSystemPrompt(GetRenderedSystemPrompt),
    GetRenderedPrompt(GetRenderedPrompt),
    GetRenderedToolDefinitions(GetRenderedToolDefinitions),
    GetCurrentSession(GetCurrentSession),
    GetSessionAgentList(GetSessionAgentList),
    UiDebugEventStatsRequest(UiDebugEventStatsRequest),
    UiDetachRequest(UiDetachRequest),
    UiTreeRequest(UiTreeRequest),
    ExtensionDataRequest(ExtensionDataRequest),
    ExternalAgentMessage(ExternalAgentMessageRequest),
    ExternalAgentMessageAuth(ExternalAgentMessageAuthRequest),
    PeerSessionProbe(PeerSessionProbe),
}

impl HarnessInputMessage {
    /// Wraps an event emission request with durable-by-default metadata.
    #[must_use]
    pub fn emit(event: Event) -> Self {
        Self::Emit(Emit::new(event))
    }

    /// Wraps an event emission request for live-only publication.
    #[must_use]
    pub fn emit_transient(event: Event) -> Self {
        Self::Emit(Emit::with_persist(event, false))
    }

    /// Wraps an event emission request with caller-selected persistence
    /// metadata.
    #[must_use]
    pub fn emit_with_persist(event: Event, persist: bool) -> Self {
        Self::Emit(Emit::with_persist(event, persist))
    }
}

/// Messages the harness sends to connected peers (UI clients and extensions).
///
/// Event delivery is represented by [`HarnessOutputMessage::Deliver`]. The
/// output direction intentionally has no `Emit` variant: peers emit events to
/// the harness, while the harness delivers events to peers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "message", content = "payload", rename_all = "snake_case")]
pub enum HarnessOutputMessage {
    Configure(Configure),
    UiSessionAccepted(UiSessionAccepted),
    Disconnect(Disconnect),
    Deliver(EventDelivery),
    InterceptRequest(InterceptRequest),
    AgentPromptCreatedResult(Box<AgentPromptCreatedResult>),
    RenderedSystemPromptResult(Box<RenderedSystemPromptResult>),
    RenderedPromptResult(Box<RenderedPromptResult>),
    RenderedToolDefinitionsResult(Box<RenderedToolDefinitionsResult>),
    CurrentSessionResult(CurrentSessionResult),
    SessionAgentListResult(Box<SessionAgentListResult>),
    ExtensionDataResult(Box<ExtensionDataResult>),
    ExternalAgentMessageResult(ExternalAgentMessageResult),
    ExternalAgentMessageAuthResult(ExternalAgentMessageAuthResult),
    PeerSessionProbeResult(PeerSessionProbeResult),
}

impl HarnessOutputMessage {
    /// Wraps an event for direct delivery of a synthesized current-state
    /// announcement.
    #[must_use]
    pub fn deliver(event: Event) -> Self {
        Self::Deliver(EventDelivery::direct(event))
    }

    /// Wraps a live committed runtime event for delivery.
    #[must_use]
    pub fn deliver_live(recorded_at: UnixMicros, event: Event) -> Self {
        Self::Deliver(EventDelivery::live(recorded_at, event))
    }

    /// Wraps a historical event for replay-marked delivery.
    #[must_use]
    pub fn deliver_replay(recorded_at: UnixMicros, event: Event) -> Self {
        Self::Deliver(EventDelivery::replay(recorded_at, event))
    }

    /// Returns delivery metadata when this output message carries an event.
    #[must_use]
    pub fn as_delivery(&self) -> Option<&EventDelivery> {
        match self {
            Self::Deliver(delivery) => Some(delivery),
            _ => None,
        }
    }

    /// Returns the delivered event when this output message carries one.
    #[must_use]
    pub fn delivered_event(&self) -> Option<&Event> {
        self.as_delivery().map(EventDelivery::event)
    }

    /// Consumes this output message and returns its delivery payload, if any.
    #[must_use]
    pub fn into_delivery(self) -> Option<EventDelivery> {
        match self {
            Self::Deliver(delivery) => Some(delivery),
            _ => None,
        }
    }

    /// Consumes this output message and returns its delivered event, if any.
    #[must_use]
    pub fn into_delivered_event(self) -> Option<Event> {
        self.into_delivery().map(EventDelivery::into_event)
    }
}
