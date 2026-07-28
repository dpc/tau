//! Protocol event types and payloads.
//!
//! Every [`Event`] variant is declared here; cohesive payload DTOs may live in
//! dedicated modules.
//!
//! Broadcast events include canonical facts plus peer-authored requests,
//! reports, and declarations that commit before downstream handling.
//! Point-to-point responses remain protocol messages rather than events.
//!
//! The event-name, routing, replay, and selected payload contracts are
//! specified by `SPEC-tau-proto-session-events`.
//! Provider streaming, retry, and recovery update payloads are specified by
//! `SPEC-tau-proto-provider-updates`.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    ActionInvocationId, AgentContextKey, AgentId, AgentInitializationContextSet,
    AgentInitializationId, AgentMessageId, AgentMetadataKey, AgentPromptId, CborValue, ContextItem,
    DiffSummary, EventCategory, EventName, ExtensionAgentDiscoverySnapshotDeclared,
    ExtensionInstanceId, ExtensionName, ExtensionSessionDiscoverySnapshotDeclared,
    HarnessAgentContextInitialized, HarnessProviderQuotaChanged, HarnessSessionSkillsAvailable,
    InternalPromptKind, MessageDeleted, MessageDelivered, MessageEdited, MessagePhase,
    MessageReactionAdded, MessageReactionRemoved, MessageSent, ModelId, ModelTag, ObservationId,
    PromptContext, PromptFragment, PromptSubmissionSource, ProviderQuotaClear, ProviderQuotaPatch,
    ProviderQuotaReplace, ProviderTokenUsage, ReasoningTextKind, SessionId, ToolCallId,
    ToolCallRef, ToolDefinition, ToolGroupName, ToolName, ToolTag,
};

fn default_true() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_affinity_neutral(value: &i32) -> bool {
    *value == 0
}

// ---------------------------------------------------------------------------
// Event names
// ---------------------------------------------------------------------------

/// Identifier of a node in one agent transcript tree. Lives on the wire
/// because tree-folding events stamp their `parent_node_id` so the
/// fold doesn't have to consult a shared write cursor.
///
/// Ids are valid only against the tree that produced them. The
/// in-memory agent tree uses the underlying `u64` as a positional
/// index into its node vector and assigns ids by insertion order, so
/// the same numeric id can refer to different nodes across different
/// trees. Replaying the same persisted agent event log yields the same ids
/// only because the fold is deterministic; an id that originated in
/// one agent is meaningless in another.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct NodeId(u64);

impl NodeId {
    #[must_use]
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Durable branch-head target for one agent transcript tree.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "node_id")]
pub enum AgentHead {
    /// Select the transcript root before any materialized node.
    Root,
    /// Select an existing transcript node as the branch head.
    Node(NodeId),
}

/// Maximum encoded length of a standalone-compaction transaction identifier.
pub const MAX_COMPACTION_TRANSACTION_ID_LEN: usize = 128;
/// Maximum encoded length of a manual-compaction request identifier.
pub const MAX_COMPACTION_REQUEST_ID_LEN: usize = 128;

/// Durable correlation identifier for one standalone-compaction transaction.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CompactionTransactionId(String);

impl CompactionTransactionId {
    /// Validates and constructs a bounded, non-empty transaction identifier.
    pub fn parse(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.is_empty() {
            return Err("compaction transaction id must not be empty");
        }
        if value.len() > MAX_COMPACTION_TRANSACTION_ID_LEN {
            return Err("compaction transaction id is too long");
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err("compaction transaction id contains invalid characters");
        }
        Ok(Self(value))
    }
}

impl TryFrom<String> for CompactionTransactionId {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<CompactionTransactionId> for String {
    fn from(value: CompactionTransactionId) -> Self {
        value.0
    }
}

impl fmt::Display for CompactionTransactionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Durable correlation identifier for one model-requested compaction.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CompactionRequestId(
    /// Validated path-safe request identifier.
    String,
);

impl CompactionRequestId {
    /// Validates and constructs a bounded, non-empty request identifier.
    pub fn parse(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.is_empty() {
            return Err("compaction request id must not be empty");
        }
        if value.len() > MAX_COMPACTION_REQUEST_ID_LEN {
            return Err("compaction request id is too long");
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err("compaction request id contains invalid characters");
        }
        Ok(Self(value))
    }
}

impl TryFrom<String> for CompactionRequestId {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<CompactionRequestId> for String {
    fn from(value: CompactionRequestId) -> Self {
        value.0
    }
}

impl fmt::Display for CompactionRequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl AgentHead {
    /// Converts this durable target to the in-memory optional head pointer.
    #[must_use]
    pub const fn as_option(self) -> Option<NodeId> {
        match self {
            Self::Root => None,
            Self::Node(node_id) => Some(node_id),
        }
    }
}

/// Target requested by a UI tree navigation command.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum UiTreeNavigationTarget {
    /// Rewind to the transcript root before any prompt.
    Root,
    /// Rewind to before the user-facing prompt anchor with this one-based
    /// ordinal. UIs should encode `0` as [`Self::Root`], not as this variant.
    PromptAnchor(u64),
    /// Expert/debug navigation to an existing raw transcript node.
    Node(NodeId),
}

// ---------------------------------------------------------------------------
// Harness notices
// ---------------------------------------------------------------------------

/// Severity or verbosity of a harness notice.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    /// Harness/control-plane failure or other must-see failure notice.
    Critical,
    /// Something is wrong, but the session is not necessarily terminating.
    Warning,
    /// Useful normal information.
    #[default]
    Info,
    /// Debugging information that is not normally needed.
    Debug,
    /// Developer-only noisy details that users should not normally see.
    Trace,
}

impl NoticeLevel {
    /// Returns the canonical config/protocol string for this level.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Warning => "warning",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    /// Parses a canonical notice level string.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "critical" => Some(Self::Critical),
            "warning" => Some(Self::Warning),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }

    /// Returns true when a notice at `self` should be shown for `threshold`.
    #[must_use]
    pub fn visible_at(self, threshold: Self) -> bool {
        self <= threshold
    }
}

/// A user-facing notice from the harness.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessNotice {
    /// Stable machine-readable notice kind used by UIs for special casing.
    pub kind: String,
    /// Human-readable notice text.
    pub message: String,
    /// Severity or verbosity level.
    #[serde(default)]
    pub level: NoticeLevel,
    /// Whether UI filters must show this non-critical notice.
    #[serde(default, skip_serializing_if = "is_false")]
    pub always_show: bool,
}

impl HarnessNotice {
    /// Creates a harness notice with filtering controlled only by its level.
    #[must_use]
    pub fn new(kind: impl Into<String>, message: impl Into<String>, level: NoticeLevel) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
            level,
            always_show: false,
        }
    }

    /// Returns true when this notice should be shown for `threshold`.
    #[must_use]
    pub fn visible_at(&self, threshold: NoticeLevel) -> bool {
        self.level == NoticeLevel::Critical || self.always_show || self.level.visible_at(threshold)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDirStatus {
    #[default]
    New,
    Resumed,
    Ephemeral,
}

impl SessionDirStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Resumed => "resumed",
            Self::Ephemeral => "ephemeral",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessSessionDir {
    pub session_id: SessionId,
    pub path: std::path::PathBuf,
    pub status: SessionDirStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessUiDir {
    pub path: std::path::PathBuf,
}

/// The harness announces all available models as `provider/model` strings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessModelsAvailable {
    /// Each entry is `"provider_name/model_id"`.
    pub models: Vec<ModelId>,
}

/// The harness announces role names with resolved descriptions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessRoleInfo {
    /// Stable role name accepted by `ui.role_select`.
    pub name: String,
    /// Human-readable summary of the role's resolved model and knobs.
    pub description: String,
    /// Optional free-form role summary from harness configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_description: Option<String>,
    /// Structured settings backing `description`; preferred by UIs over parsing
    /// the human-readable description string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<HarnessRoleDetails>,
}

/// Structured role settings for UI completions and status text.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessRoleDetails {
    /// Resolved model id for the role, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Resolved model parameters after provider/model defaults are applied.
    #[serde(default, skip_serializing_if = "ModelParams::is_default")]
    pub params: ModelParams,
    /// Explicit internal tool allow-list for this role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolName>>,
    /// Tool groups enabled in addition to the allow-list/default set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enable_tool_groups: Vec<ToolGroupName>,
    /// Tool groups disabled for this role.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disable_tool_groups: Vec<ToolGroupName>,
    /// Internal tools enabled in addition to the allow-list/default set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enable_tools: Vec<ToolName>,
    /// Internal tools disabled for this role.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disable_tools: Vec<ToolName>,
}

impl HarnessRoleDetails {
    /// Returns true when no structured role details are set.
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// One ordered role group used for keyboard navigation and grouped menus.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessRoleGroup {
    /// Stable group name from harness `agents.role_groups` configuration.
    pub name: String,
    /// Role names in navigation order. Names are accepted by `ui.role_select`.
    pub roles: Vec<String>,
}

/// One reusable prompt template configured by the running harness and offered
/// to UI clients as `:prompt <id>`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessCustomPrompt {
    /// Stable prompt id accepted by `:prompt <id>`.
    pub id: String,
    /// Prompt text inserted into the editable prompt buffer.
    pub text: String,
}

/// The harness announces all roles and reusable prompts available for
/// selection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessRolesAvailable {
    /// Role entries sorted by name for deterministic UI menus.
    pub roles: Vec<HarnessRoleInfo>,
    /// Ordered role groups for structured keyboard navigation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<HarnessRoleGroup>,
    /// Reusable prompt templates from the running harness config.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_prompts: Vec<HarnessCustomPrompt>,
}

/// The harness announces the selected role and its currently resolved model.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessRoleSelected {
    /// Selected agent role. Role selection is always the runtime source of
    /// truth; the model is derived from this role and provider availability.
    pub role: String,
    /// Model currently resolved for [`Self::role`], or `None` while the role's
    /// model is not provider-published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Total context window size, in tokens, if known for the resolved model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Effective role/provider baseline parameters, ignoring persisted state.
    /// The UI compares live parameters against this baseline so state overrides
    /// stay visible in the status bar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_params: Option<ModelParams>,
    /// Effective parameters derived from the selected role plus runtime role
    /// overrides for the currently resolved model.
    #[serde(default)]
    pub model_params: ModelParams,
}

/// Current context usage for the selected model.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessContextUsageChanged {
    /// Input tokens consumed by the most recent agent response, if the
    /// provider reported it. `None` means usage has never been
    /// reported for the current model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Cached input tokens consumed by the most recent agent response,
    /// if the provider reported them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// Percentage of the context window currently used. `None` when
    /// the selected provider model metadata is unavailable, so the UI
    /// can fall back to showing raw token count instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent_used: Option<u8>,
}

/// Current context usage for one agent transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessAgentContextUsageChanged {
    /// Agent whose context usage changed.
    pub agent_id: AgentId,
    /// Input tokens consumed by that agent's most recent response, if the
    /// provider reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Cached input tokens consumed by that agent's most recent response, if
    /// the provider reported them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// Total context window size for the model that produced the response, if
    /// known from provider metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Percentage of the context window currently used. `None` when either
    /// usage or provider model metadata is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent_used: Option<u8>,
}

/// Reasoning effort level. Maps to provider-specific reasoning
/// controls (OpenAI `reasoning.effort`, Anthropic
/// `thinking.budget_tokens`). `Off` disables it entirely.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Effort {
    #[default]
    Off = 0,
    Minimal = 1,
    Low = 2,
    Medium = 3,
    High = 4,
    /// `rename_all = "snake_case"` would emit `x_high` for this
    /// variant, but the canonical wire string is `xhigh` everywhere
    /// else (`:role engineer effort xhigh`, OpenAI's `reasoning_effort` field,
    /// `Display`, `FromStr`, `effort_wire`). Pin it explicitly so
    /// serde-driven config paths (`default_efforts`,
    /// `reasoningEfforts`) agree with the rest.
    #[serde(rename = "xhigh")]
    XHigh = 5,
    /// Maximum model reasoning effort.
    Max = 6,
}

impl Effort {
    const LEVEL_COUNT: usize = 7;

    /// Cycles to the next level (wraps `Max → Off`).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Off => Self::Minimal,
            Self::Minimal => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::XHigh,
            Self::XHigh => Self::Max,
            Self::Max => Self::Off,
        }
    }

    /// Cycle in the canonical order, but only through levels that are
    /// in `allowed` so callers don't land on a level the current model
    /// doesn't support (e.g. xhigh on `gpt-5.4-mini`). Falls back to
    /// [`Effort::next`] when `allowed` is empty.
    #[must_use]
    pub fn next_in(self, allowed: &[Self]) -> Self {
        if allowed.is_empty() {
            return self.next();
        }
        let mut candidate = self.next();
        // Bounded by `Effort` variant count — at most one full
        // wrap-around before we either land on an allowed level or
        // confirm none exist.
        for _ in 0..Self::LEVEL_COUNT {
            if allowed.contains(&candidate) {
                return candidate;
            }
            candidate = candidate.next();
        }
        self
    }

    /// Short label for status display (`off` / `low` / `high` / etc).
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Numeric tag suitable for storing in an `AtomicU8`. Round-trips
    /// through [`Effort::from_u8`].
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Effort::as_u8`]. Returns `None` for unknown tags so
    /// callers can decide how to recover; the common case (loading from
    /// an atomic mirror) maps `None` to [`Effort::Off`].
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Off),
            1 => Some(Self::Minimal),
            2 => Some(Self::Low),
            3 => Some(Self::Medium),
            4 => Some(Self::High),
            5 => Some(Self::XHigh),
            6 => Some(Self::Max),
            _ => None,
        }
    }

    /// True for the default level (`Off`). Used by `ModelParams`'
    /// `#[serde(skip_serializing_if)]` so untouched values stay out
    /// of the wire form.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Off)
    }
}

impl std::str::FromStr for Effort {
    type Err = ParseEffortError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Self::Off),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            other => Err(ParseEffortError {
                input: other.to_owned(),
            }),
        }
    }
}

/// Error returned when an effort string is not one of the well-known
/// levels (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseEffortError {
    input: String,
}

impl ParseEffortError {
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl fmt::Display for ParseEffortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown effort level `{}`; expected off/minimal/low/medium/high/xhigh/max",
            self.input
        )
    }
}

impl std::error::Error for ParseEffortError {}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Optional upstream service tier. `Fast` enables Fast mode on providers
/// that expose it; `Flex` is an explicit lower-priority service tier.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceTier {
    Fast,
    Flex,
}

impl ServiceTier {
    /// Config/event spelling used by Codex (`fast` / `flex`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Flex => "flex",
        }
    }

    /// OpenAI wire spelling used by Codex requests (`priority` / `flex`).
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Fast => "priority",
            Self::Flex => "flex",
        }
    }
}

/// Output verbosity hint sent to providers that support it (OpenAI
/// GPT-5 family: `verbosity` on Chat Completions, `text.verbosity` on
/// Responses). Providers that don't advertise `supportsVerbosity`
/// silently ignore the field.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Verbosity {
    #[default]
    Low = 0,
    Medium = 1,
    High = 2,
}

impl Verbosity {
    /// Cycles to the next level (wraps `High → Low`).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Low,
        }
    }

    /// Cycle in canonical order through levels that are in `allowed`.
    /// Falls back to plain [`Verbosity::next`] when `allowed` is empty.
    #[must_use]
    pub fn next_in(self, allowed: &[Self]) -> Self {
        if allowed.is_empty() {
            return self.next();
        }
        let mut candidate = self.next();
        for _ in 0..3 {
            if allowed.contains(&candidate) {
                return candidate;
            }
            candidate = candidate.next();
        }
        self
    }

    /// Short label for status display (`low` / `medium` / `high`).
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Low),
            1 => Some(Self::Medium),
            2 => Some(Self::High),
            _ => None,
        }
    }

    /// Wire string for OpenAI's `verbosity` / `text.verbosity` field.
    /// All variants map to a non-empty string — there is no "off"
    /// sentinel — so callers gate the field on a provider-level
    /// `supports_verbosity` flag, not on the value itself.
    #[must_use]
    pub const fn as_openai_wire(self) -> &'static str {
        self.as_str()
    }

    /// True for the default level. Used by `#[serde(skip_serializing_if)]`
    /// on `ModelParams` so untouched values stay out of the wire form.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Low)
    }
}

impl std::str::FromStr for Verbosity {
    type Err = ParseVerbosityError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            other => Err(ParseVerbosityError {
                input: other.to_owned(),
            }),
        }
    }
}

/// Error returned when a verbosity string is not one of the well-known
/// levels (`low`, `medium`, `high`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseVerbosityError {
    input: String,
}

impl ParseVerbosityError {
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl fmt::Display for ParseVerbosityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown verbosity level `{}`; expected low/medium/high",
            self.input
        )
    }
}

impl std::error::Error for ParseVerbosityError {}

impl fmt::Display for Verbosity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The harness announces which verbosity levels are valid for the
/// selected role's resolved model. Updated on startup and whenever the
/// resolved model changes. Empty list means the selected role has no
/// resolved model; a single-element `[Medium]` list means the provider
/// doesn't support the knob.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessVerbositiesAvailable {
    pub levels: Vec<Verbosity>,
}

/// Whether to ask the provider for a human-readable summary of its
/// reasoning, and at what verbosity. Currently only the OpenAI
/// Responses API exposes this surface (`reasoning.summary`). Auto by
/// default for providers that advertise `supportsReasoningSummary`;
/// `Off` everywhere else.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ThinkingSummary {
    #[default]
    Off = 0,
    Auto = 1,
    Concise = 2,
    Detailed = 3,
}

impl ThinkingSummary {
    /// Cycles to the next level (wraps `Detailed → Off`).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Off => Self::Auto,
            Self::Auto => Self::Concise,
            Self::Concise => Self::Detailed,
            Self::Detailed => Self::Off,
        }
    }

    /// Cycle in canonical order through levels that are in `allowed`.
    /// Falls back to plain [`ThinkingSummary::next`] when `allowed` is
    /// empty.
    #[must_use]
    pub fn next_in(self, allowed: &[Self]) -> Self {
        if allowed.is_empty() {
            return self.next();
        }
        let mut candidate = self.next();
        for _ in 0..4 {
            if allowed.contains(&candidate) {
                return candidate;
            }
            candidate = candidate.next();
        }
        self
    }

    /// Short label for status display.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
            Self::Concise => "concise",
            Self::Detailed => "detailed",
        }
    }

    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Off),
            1 => Some(Self::Auto),
            2 => Some(Self::Concise),
            3 => Some(Self::Detailed),
            _ => None,
        }
    }

    /// Wire string used by OpenAI's Responses `reasoning.summary`
    /// field, or `None` for the off mode where the field is omitted.
    #[must_use]
    pub const fn as_openai_wire(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Auto => Some("auto"),
            Self::Concise => Some("concise"),
            Self::Detailed => Some("detailed"),
        }
    }

    /// True for the default level.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Off)
    }
}

impl std::str::FromStr for ThinkingSummary {
    type Err = ParseThinkingSummaryError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Self::Off),
            "auto" => Ok(Self::Auto),
            "concise" => Ok(Self::Concise),
            "detailed" => Ok(Self::Detailed),
            other => Err(ParseThinkingSummaryError {
                input: other.to_owned(),
            }),
        }
    }
}

/// Error returned when a thinking-summary string is not one of the
/// well-known modes (`off`, `auto`, `concise`, `detailed`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseThinkingSummaryError {
    input: String,
}

impl ParseThinkingSummaryError {
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl fmt::Display for ParseThinkingSummaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown thinking summary `{}`; expected off/auto/concise/detailed",
            self.input
        )
    }
}

impl std::error::Error for ParseThinkingSummaryError {}

impl std::fmt::Display for ThinkingSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The harness announces which thinking-summary modes are valid for
/// the selected role's resolved model. Empty list means the selected role has
/// no resolved model; `[Off]` means the provider doesn't expose summaries.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessThinkingSummariesAvailable {
    pub levels: Vec<ThinkingSummary>,
}

/// Per-prompt model knobs the harness selects, persists, and stamps
/// onto every [`AgentPromptCreated`]. Bundling these together lets
/// providers and backends thread one struct through instead of a
/// growing list of fields. Each component independently falls back to
/// "omit the field" when its [`Verbosity::is_default`] / `is_default`
/// helper says it's still at the default.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelParams {
    #[serde(default, skip_serializing_if = "Effort::is_default")]
    pub effort: Effort,
    #[serde(default, skip_serializing_if = "Verbosity::is_default")]
    pub verbosity: Verbosity,
    #[serde(default, skip_serializing_if = "ThinkingSummary::is_default")]
    pub thinking_summary: ThinkingSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
}

impl ModelParams {
    #[must_use]
    /// Returns true when all model parameters are at their protocol defaults.
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}
/// The harness announces which efforts are valid for the selected role's
/// resolved model. Updated on startup and whenever the resolved model changes.
/// Empty list means no effort applies (the selected role has no resolved model,
/// or the provider doesn't support reasoning).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessEffortsAvailable {
    pub levels: Vec<Effort>,
}

// ---------------------------------------------------------------------------
// Tool events
// ---------------------------------------------------------------------------

/// Tool metadata used during registration and invocation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolType {
    #[default]
    Function,
    Custom,
}

impl ToolType {
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Function)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolGrammarSyntax {
    Lark,
    Regex,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolFormat {
    Text,
    Grammar {
        syntax: ToolGrammarSyntax,
        definition: String,
    },
}

/// Tool metadata used during registration and invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: ToolName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_visible_name: Option<ToolName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether this is a JSON-schema function tool or a freeform custom tool.
    pub tool_type: ToolType,
    /// JSON Schema describing the tool's input parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    /// Optional freeform/custom input format. `None` means provider-default
    /// unconstrained text for custom tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ToolFormat>,
    /// Neutral capability tags used by harness-owned tool policy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<ToolTag>,
    /// Whether this tool should be advertised to the agent when the role has
    /// no explicit `tools` allow-list and `disable_tools` does not remove it.
    #[serde(default = "tool_enabled_by_default", skip_serializing_if = "is_true")]
    pub enabled_by_default: bool,
    /// Whether the harness may close the model-visible foreground turn before
    /// the real tool process has returned. `None` means the harness applies its
    /// default policy, currently
    /// [`BackgroundSupport::MinForegroundSeconds`]`(2)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_support: Option<BackgroundSupport>,
    /// Optional compact examples owned by the tool provider. The harness keeps
    /// these out of provider-visible tool definitions and may surface one after
    /// a failed call to help the model repair mechanical argument mistakes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<ToolExample>,
}

/// Compact example arguments for a tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolExample {
    /// Stable provider-owned id for de-duplication and deterministic selection.
    pub id: String,
    /// Optional short human-readable label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Arguments that form a valid call for this tool.
    pub arguments: CborValue,
    /// Optional short note explaining when to use the example.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Optional declarative subcommand selector. The harness only compares this
    /// exact argument-path value; it never infers subcommands from prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subcommand: Option<ToolExampleSelector>,
}

/// Declarative selector for a tool example subcommand.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolExampleSelector {
    /// Path through the argument object, e.g. `["operation"]`.
    pub path: Vec<String>,
    /// Exact value at `path` for this subcommand.
    pub value: CborValue,
}

const fn tool_enabled_by_default() -> bool {
    true
}

const fn is_true(value: &bool) -> bool {
    *value
}

/// Foreground/background policy for a tool call after dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundSupport {
    /// Close the foreground as soon as the tool is dispatched.
    Instant,
    /// Keep the call in the foreground for at least this many seconds.
    MinForegroundSeconds(u64),
    /// Never synthesize foreground completion before the real result arrives.
    Never,
}

impl BackgroundSupport {
    /// Effective background support when a tool registration omits the field.
    #[must_use]
    pub const fn default_effective() -> Self {
        Self::MinForegroundSeconds(2)
    }
}

// ---------------------------------------------------------------------------
// Action events
// ---------------------------------------------------------------------------

/// Harness-stamped action schema currently provided by one extension instance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActionSchemaPublished {
    /// Extension name owning this schema. Stamped by the harness.
    pub extension_name: ExtensionName,
    /// Extension instance id owning this schema. Stamped by the harness.
    pub instance_id: ExtensionInstanceId,
    /// Full action schema published by the extension.
    pub schema: tau_actions::ActionSchema,
}

/// UI request to invoke an extension-provided action.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionInvoke {
    /// Client-minted id used to route the matching result/error.
    pub invocation_id: ActionInvocationId,
    /// Active Tau session from which the action was invoked.
    pub session_id: SessionId,
    /// Extension name selected by the UI's schema snapshot.
    pub extension_name: ExtensionName,
    /// Extension instance id selected by the UI's schema snapshot.
    pub instance_id: ExtensionInstanceId,
    /// Stable action id selected by the parsed command line.
    pub action_id: String,
    /// Original command line submitted by the user.
    pub raw_line: String,
    /// Positional arguments in schema order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
    /// Typed/named argument map encoded as CBOR values.
    pub arguments: CborValue,
}

/// UI-visible successful action output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionResult {
    /// Invocation id copied from [`ActionInvoke`].
    pub invocation_id: ActionInvocationId,
    /// Stable action id that produced this result.
    pub action_id: String,
    /// Output the UI should render.
    pub output: ActionOutput,
}

/// UI-visible action failure.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionError {
    /// Invocation id copied from [`ActionInvoke`].
    pub invocation_id: ActionInvocationId,
    /// Stable action id that failed.
    pub action_id: String,
    /// Human-readable error message.
    pub message: String,
    /// Optional structured diagnostic details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<CborValue>,
}

/// Output shape for a successful extension action.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionOutput {
    /// Plain text output rendered by the UI.
    Text {
        /// Text to display.
        text: String,
    },
    /// Text buffer that a UI may open in an editor in a later phase.
    EditorBuffer {
        /// Short title for the buffer.
        title: String,
        /// Buffer contents.
        text: String,
        /// Whether the UI may let the user edit this buffer.
        editable: bool,
    },
}

/// Per-prompt knob telling the provider whether the model is allowed
/// to call tools on this turn. Stamped onto every
/// [`AgentPromptCreated`]; the harness sets [`Self::None`] for
/// non-tool extension-side queries (e.g. `std-notifications`' idle
/// summary) so the cache prefix (tools + system_prompt) stays
/// byte-identical to the parent conv's while still preventing the
/// summarizer from accidentally calling `edit` / `agent_start`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// The model decides whether to call tools (provider default).
    #[default]
    Auto,
    /// The model must produce a text answer this turn; tools are
    /// still declared in the request (so cache prefix matches), but
    /// the provider rejects tool-call output.
    None,
}

impl ToolChoice {
    /// True for the default value. Used by `#[serde(skip_serializing_if)]`
    /// on [`AgentPromptCreated`] so untouched values stay out of the
    /// wire form.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// Tool group metadata published by an extension or provider.
///
/// Groups let roles enable or disable related tools together and optionally add
/// shared provider-visible prompt context when any group member is available.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolGroup {
    /// Stable group name shared by related tools from the same provider.
    pub name: ToolGroupName,
    /// Optional system-prompt fragment template rendered once when any tool in
    /// this group is enabled for the current role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_fragment: Option<PromptFragment>,
}

/// Tool registration declaration emitted by a tool or core extension.
///
/// Proposes one tool definition and its optional grouping/prompt context. The
/// harness validates the committed declaration before publishing
/// [`ToolRegister`]. Declarations are transient and interceptable; commit is
/// not an acceptance acknowledgement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolRegistrationDeclared {
    /// Tool metadata made available to the agent and used for routing calls.
    pub tool: ToolSpec,
    /// Optional group containing this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_group: Option<ToolGroup>,
    /// Optional system-prompt fragment template to render whenever this tool is
    /// enabled for the current role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_fragment: Option<PromptFragment>,
}

/// Tool unregistration declaration emitted when an extension proposes
/// withdrawing one of its tools.
///
/// The transient, interceptable declaration commits before the harness checks
/// ownership. An accepted active withdrawal produces [`ToolUnregister`];
/// unknown or non-owner withdrawals produce a diagnostic.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolUnregistrationDeclared {
    /// Name of the extension-owned tool proposed for withdrawal.
    pub tool_name: ToolName,
}

/// Harness-authored canonical state for one accepted tool registration.
///
/// This transient runtime-only event is immutable, must-pass, and not replayed
/// after a cold restart.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolRegister {
    /// Stable configured extension identity that owns the tool.
    pub publisher_extension_id: ExtensionName,
    /// Harness-assigned logical configured-extension instance identity.
    ///
    /// This remains stable when the supervised process for that configured
    /// instance respawns; it is not a process-connection generation.
    pub publisher_instance_id: ExtensionInstanceId,
    /// Tool metadata made available to the agent and used for routing calls.
    pub tool: ToolSpec,
    /// Optional group containing this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_group: Option<ToolGroup>,
    /// Optional system-prompt fragment rendered whenever this tool is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_fragment: Option<PromptFragment>,
}

/// Harness-authored canonical state for one accepted tool withdrawal.
///
/// This transient runtime-only event is immutable, must-pass, and not replayed
/// after a cold restart.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolUnregister {
    /// Stable configured extension identity that owned the tool.
    pub publisher_extension_id: ExtensionName,
    /// Harness-assigned logical configured-extension instance identity.
    ///
    /// This remains stable when the supervised process for that configured
    /// instance respawns; it is not a process-connection generation.
    pub publisher_instance_id: ExtensionInstanceId,
    /// Name of the tool removed from harness routing metadata.
    pub tool_name: ToolName,
}

/// Request to run a tool call.
///
/// This is the pre-routing intent: it may come from an agent response
/// (`ContextItem::ToolCall`) or from another extension, and the harness may
/// still reject it before any provider receives a [`ToolStarted`].
///
/// A matching [`ToolStarted`] means routing succeeded and the selected
/// tool extension should start handling the call. A matching
/// [`ToolRejected`] means no tool extension was invoked.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolRequest {
    /// Stable id assigned by the agent/provider for this logical tool call.
    /// All later started, rejected, progress, result, error, or cancellation
    /// events for the same call use this id.
    pub call_id: ToolCallId,
    /// Tool name requested by the agent or extension. The harness resolves this
    /// name against the live tool registry before emitting [`ToolStarted`].
    pub tool_name: ToolName,
    /// Protocol-level kind of tool being requested. Function tools are the
    /// normal model-callable tools; the value is echoed in rejection/error
    /// paths.
    pub tool_type: ToolType,
    /// Raw CBOR arguments supplied by the requester. These are not trusted
    /// until the harness validates and routes the request.
    pub arguments: CborValue,
    /// Agent that owns this tool call.
    pub agent_id: AgentId,
    /// Who started the prompt that produced this tool call. The
    /// harness stamps this from the call's owning conversation so
    /// subscribers can tell main-agent tool activity from sub-agent
    /// (delegate / extension-query) tool activity without having to
    /// map `call_id` back to a conversation themselves.
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// Broadcast by the harness after accepting a tool request.
///
/// This is the post-routing counterpart to [`ToolRequest`]: if a tool
/// extension sees this event for a tool it owns, it should start handling the
/// call. The event is also durable UI-visible evidence that the invoke really
/// started.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolStarted {
    /// Stable id of the accepted tool call, copied from
    /// [`ToolRequest::call_id`].
    pub call_id: ToolCallId,
    /// Registry-resolved tool name. Subscribed extensions must ignore this
    /// event unless they own this tool.
    pub tool_name: ToolName,
    /// Arguments to pass to the selected tool provider. These are copied from
    /// the accepted request after harness validation/routing.
    pub arguments: CborValue,
    /// Agent that owns this tool call.
    pub agent_id: AgentId,
    /// Echo of [`ToolRequest::originator`]. Tools usually don't
    /// branch on it, but it's available for logging / progress
    /// tagging / policy decisions that depend on whether the call
    /// is for the main agent or a sub-agent.
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// Broadcast by the harness when a tool request is rejected before any
/// tool extension is asked to run it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolRejected {
    /// Stable id of the rejected tool call, copied from
    /// [`ToolRequest::call_id`].
    pub call_id: ToolCallId,
    /// Requested tool name that could not be routed or accepted.
    pub tool_name: ToolName,
    /// Requested tool type, echoed so UIs and logs can render the rejected call
    /// without consulting the original request.
    pub tool_type: ToolType,
    /// Human-readable rejection reason produced by the harness.
    pub message: String,
    /// Echo of [`ToolRequest::originator`], stamped by the harness so UIs can
    /// attribute the rejected call to the main agent or a sub-agent.
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// Provider-facing terminal result class for one logical tool call.
///
/// [`Self::Final`] is the normal terminal result: the real tool output is
/// available and can satisfy both UI rendering and provider tool-call state.
/// [`Self::BackgroundPlaceholder`] is synthetic foreground completion emitted
/// when the harness intentionally lets a long-running call continue in the
/// background; the real completion arrives later as [`ToolBackgroundResult`] or
/// [`ToolBackgroundError`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultKind {
    /// Real terminal tool result.
    #[default]
    Final,
    /// Synthetic foreground provider completion for a backgrounded call.
    BackgroundPlaceholder,
}

/// Successful logical tool completion.
///
/// For ordinary foreground calls this carries the final tool result. For
/// backgrounded calls, a result with [`ToolResultKind::BackgroundPlaceholder`]
/// closes only the provider-visible foreground turn; the later
/// [`ToolBackgroundResult`] carries the real output.
/// Tool/Core peers submit this payload as [`Event::ToolResultReported`]; the
/// harness uses [`Event::ToolResult`] and [`Event::ProviderToolResult`] for
/// canonical projections.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// Stable id of the completed tool call.
    pub call_id: ToolCallId,
    /// Tool name that produced this result.
    pub tool_name: ToolName,
    /// Protocol-level tool kind echoed from the request.
    pub tool_type: ToolType,
    /// Tool-owned successful result payload.
    pub result: CborValue,
    /// Typed provider-visible content that must not pass through text
    /// rendering.
    ///
    /// The harness strips this field from the generic UI completion event and
    /// preserves it only on the provider/transcript completion path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_content: Vec<crate::ToolResultContentPart>,
    /// Whether this is the real final result or a synthetic background
    /// placeholder.
    #[serde(default)]
    pub kind: ToolResultKind,
    /// Generic UI state for the completed tool use.
    ///
    /// Tool producers should populate this instead of relying on terminal UIs
    /// to parse `result`. This is operational display metadata, not
    /// transcript truth; the raw `result` remains the
    /// model-/extension-facing payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolUseState>,
    /// Echo of the originating [`ToolRequest::originator`]. Tool
    /// extensions usually pass [`PromptOriginator::User`] (the
    /// default); the harness re-stamps this with the call's owning
    /// conversation's originator before broadcasting, so subscribers
    /// see a faithful tag without every extension having to track
    /// it.
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// Failed logical tool completion.
///
/// This is terminal for a foreground call. Backgrounded calls that have already
/// emitted a [`ToolResultKind::BackgroundPlaceholder`] must report their later
/// failure as [`ToolBackgroundError`] instead, so provider state is not closed
/// twice.
/// Tool/Core peers submit this payload as [`Event::ToolErrorReported`]; the
/// harness uses [`Event::ToolError`] and [`Event::ProviderToolError`] for
/// canonical projections.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolError {
    /// Stable id of the failed tool call.
    pub call_id: ToolCallId,
    /// Tool name that produced this error.
    pub tool_name: ToolName,
    /// Protocol-level tool kind echoed from the request.
    pub tool_type: ToolType,
    /// Human-readable failure message.
    pub message: String,
    /// Optional structured error details for UIs or diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<CborValue>,
    /// See [`ToolResult::display`]. On error, the state `status` is typically
    /// [`ToolUseStatus::Error`] and
    /// `status_text` carries an optional error label. Renderers add the
    /// generic error prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolUseState>,
    /// Echo of the originating [`ToolRequest::originator`]; see
    /// [`ToolResult::originator`].
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// Real success result for a tool call whose foreground was already completed
/// with a synthetic background placeholder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolBackgroundResult {
    /// Stable id of the backgrounded tool call.
    pub call_id: ToolCallId,
    /// Tool name that produced this result.
    pub tool_name: ToolName,
    /// Protocol-level tool kind echoed from the request.
    pub tool_type: ToolType,
    /// Real successful output produced after foreground placeholder completion.
    pub result: CborValue,
    /// Generic UI state for the completed background tool use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolUseState>,
    /// Echo of the originating [`ToolRequest::originator`]; see
    /// [`ToolResult::originator`].
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// Real error result for a tool call whose foreground was already completed
/// with a synthetic background placeholder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolBackgroundError {
    /// Stable id of the backgrounded tool call.
    pub call_id: ToolCallId,
    /// Tool name that produced this error.
    pub tool_name: ToolName,
    /// Protocol-level tool kind echoed from the request.
    pub tool_type: ToolType,
    /// Human-readable failure message from the real background completion.
    pub message: String,
    /// Optional structured error details for UIs or diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<CborValue>,
    /// Generic UI state for the failed background tool use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolUseState>,
    /// Echo of the originating [`ToolRequest::originator`]; see
    /// [`ToolResult::originator`].
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// Generic UI state for one tool use at one point in its lifecycle.
///
/// This type exists to keep tool rendering semantic and uniform. A tool or the
/// harness should describe the current tool use here once, then every UI should
/// render this structure without parsing tool-specific arguments, CBOR result
/// shapes, error details, or ad-hoc strings. That separation is important: the
/// event log is the durable source of truth for replay, terminal rendering,
/// compact summaries, future graphical UIs, and alternate clients. If a
/// renderer has to know that `grep` uses `pattern`, `agent_start` has a role,
/// or `edit` carries a diff, the abstraction has failed and the special case
/// will spread.
///
/// Prefer extending this general-purpose structure when a new tool needs richer
/// presentation. Add optional fields, typed counters, typed chips, or a new
/// [`ToolUsePayload`] variant rather than teaching the CLI about that tool's
/// private input or output format. Free-form text fields are intentionally kept
/// small and display-oriented; model-visible transcript data still belongs in
/// the normal tool result payloads.
///
/// A `ToolUseState` may appear on `tool.progress`, `tool.result`, `tool.error`,
/// background result/error events, and delegated progress events. Each
/// occurrence is a complete replacement for the display
/// state at that lifecycle point, not a patch that renderers need to merge.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolUseState {
    /// Short label rendered alongside the tool name (e.g.
    /// `"src/main.rs"`, `"\"foo\" in src"`, `"git status"`). Empty
    /// when the tool has nothing useful to surface beyond its name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub args: String,
    /// Optional compact execution mode rendered between the tool name
    /// and `args` (e.g. shell `"ro"` / `"rw"`). This is intentionally
    /// separate from `args` so themes can style mode chips distinctly.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
    /// Optional display-oriented range rendered separately from `args`, for
    /// tools whose primary object and range should remain distinct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<ToolUseRange>,
    /// Compact `NM, NL, NkB`-style stats. Each field is optional
    /// so the renderer can omit a slot rather than emit `(0M, 1L)`.
    #[serde(default, skip_serializing_if = "ToolUseStats::is_empty")]
    pub stats: ToolUseStats,
    /// Labelled counter chips (current / optional total) rendered
    /// between stats and `info_chips`. Used for tools that surface
    /// progress data: `#12.3k/200k`, `%3`, `bytes: 12/200`,
    /// etc. The unit hint picks the rendering shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub progress_counters: Vec<ProgressCounter>,
    /// Free-form info chips beyond the stats slot (e.g. `"(2
    /// suggestions)"`). Rendered between counters and status.
    ///
    /// Keep these display-only and generic. If a chip starts requiring renderer
    /// code that knows which tool produced it, replace it with a typed optional
    /// field or typed counter instead.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub info_chips: Vec<String>,
    /// Severity of the trailing status chip. Picks its themed color.
    pub status: ToolUseStatus,
    /// Status word/message rendered as the last chip. Successfully completed
    /// tools use the shared short `"ok"` label unless a different label
    /// represents a documented non-success lifecycle state. For
    /// [`ToolUseStatus::Error`], this is the label without the
    /// generic `"err:"` prefix; renderers add that prefix and handle any
    /// width abbreviation needed for the current UI.
    pub status_text: String,
    /// Optional rich content rendered in a block below the chip row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<ToolUsePayload>,
}

/// Display-oriented range attached to a tool use.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ToolUseRange {
    /// Inclusive or lower range bound, already normalized for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// Exclusive/upper range bound, already normalized for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
}

impl ToolUseRange {
    /// Return true when both range bounds are absent.
    pub fn is_empty(&self) -> bool {
        self.start.is_none() && self.end.is_none()
    }
}

/// One labelled counter rendered as an info chip. Shape depends on
/// `unit` and which of `complete` / `total` are populated:
/// - `Count`: `N` (complete only) or `N/M` (both).
/// - `Percent`: `N%` (complete only) or `N%/M` (both — `M` is e.g. a context
///   window size, formatted like a token count).
/// - `Tokens`: `N` or `N/M` rendered with token-count suffixes.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ProgressCounter {
    /// Human-readable prefix shown before the value (e.g. `"ctx"`,
    /// `"tools"`). Renders as `"label: value"`. `None` for an
    /// unlabelled chip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// What `complete` and `total` represent. Picks the rendering.
    pub unit: ProgressUnit,
    /// Completed amount. `None` is rendered as `?`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complete: Option<u64>,
    /// Optional denominator. For `Count`, the cumulative total; for
    /// `Percent`, the underlying span (e.g. context window size).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// Unit used to interpret numeric progress counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressUnit {
    /// Raw integers. Renders as `N` or `N/M`. Default if the sender
    /// doesn't specify.
    #[default]
    Count,
    /// `complete` is a percent 0..=100. Renders as `N%` or
    /// `N%/format_token_count(total)`.
    Percent,
    /// `complete` and `total` are token counts, each formatted with
    /// token-count suffixes.
    Tokens,
}

/// Volume metrics. Each is optional because a given tool typically
/// reports only some of them — `read` and `ls` have lines/bytes but no
/// matches; `grep` has all three.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ToolUseStats {
    /// Number of matches produced by search-like tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<u64>,
    /// Number of text lines read, written, or returned by the tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u64>,
    /// Number of bytes read, written, or returned by the tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

impl ToolUseStats {
    /// Returns true when no statistic counters are present.
    pub fn is_empty(&self) -> bool {
        self.matches.is_none() && self.lines.is_none() && self.bytes.is_none()
    }

    /// Build line and byte statistics for non-empty text.
    #[must_use]
    pub fn for_text(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        Self {
            matches: None,
            lines: Some(text.lines().count() as u64),
            bytes: Some(text.len() as u64),
        }
    }
}

/// Status severity for one rendered tool-use or progress item.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolUseStatus {
    /// Tool execution completed successfully or progress is informational.
    #[default]
    Success,
    /// Tool execution completed with a non-fatal warning.
    Warning,
    /// Tool execution failed or progress describes an error state.
    Error,
    /// The tool provider has accepted the call and it is running. Used by
    /// progress events. The renderer trades an empty trailing status chip for
    /// [`crate::PROGRESS_INDICATOR_TEXT`].
    InProgress,
}

/// Rich content rendered below the chip row.
///
/// Extend this enum when a tool needs structured body content that a plain
/// stats/counter/chip row cannot express. That is preferred over adding
/// renderer-side checks for individual tool names.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolUsePayload {
    /// Structured file diff. The renderer derives the `+N -M` chip
    /// from the summary's `added`/`removed` and renders the hunks
    /// below the chip row.
    Diff(DiffSummary),
    /// Structured diffs for a multi-file mutation. Each entry carries its
    /// display path so UIs can keep file boundaries while rendering the
    /// same hunk/inline data as a single-file diff.
    Diffs { files: Vec<crate::FileDiffSummary> },
    /// Plain text rendered below the chip row. Used when the inline
    /// args label would be too noisy (e.g. multi-line shell commands).
    Text { text: String },
}

/// Simple current/total progress counter for tool progress events.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProgressUpdate {
    /// Completed units so far. Omitted when the tool only has a message or
    /// display update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<u64>,
    /// Optional total units for bounded progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// Progress payload shared by peer reports and harness-validated canonical
/// facts.
///
/// Tool/Core peers submit this payload as [`Event::ToolProgressReported`].
/// Subscribers consume it as the harness-authored [`Event::ToolProgress`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolProgress {
    /// Tool call id this progress update belongs to.
    pub call_id: ToolCallId,
    /// Name of the tool handling the call.
    pub tool_name: ToolName,
    /// Optional human-readable progress message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Optional numeric progress counter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<ProgressUpdate>,
    /// Optional complete replacement for the running tool-use UI state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolUseState>,
}

/// Legacy live snapshot of a sub-agent spawned by the old `agent_start` path.
///
/// First-party harness code no longer emits this event; generic
/// [`AgentWatchesUpdated`] and [`AgentStatsUpdated`] events carry current watch
/// relationships and per-agent operational stats instead. The type remains only
/// as legacy protocol surface for old logs/tests. Transient — not folded into
/// any durable semantic log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DelegateProgress {
    /// The original parent `agent_start` call — the tool block under
    /// which this update should appear.
    pub call_id: ToolCallId,
    /// Display name the parent agent provided for the sub-task.
    pub task_name: String,
    /// Agent id assigned to the delegated sub-agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    /// Role used by the delegated sub-agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Most recent percent-of-context-window the sub-agent reported,
    /// when its model's window size is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_percent: Option<u8>,
    /// Most recent input-token count the sub-agent reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_input_tokens: Option<u64>,
    /// Sub-agent's model context window size, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_window: Option<u64>,
    /// Number of tool calls currently in flight in the sub-agent.
    pub tools_in_flight: u32,
    /// Cumulative number of tool calls the sub-agent has started
    /// during this delegation (including completed and in-flight).
    pub tools_total: u32,
    /// Generic UI state for the running delegate block. The harness fills this
    /// in from the fields above so the renderer can paint the progress without
    /// delegate-specific parsing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolUseState>,
}

/// Broadcast intent to request cancellation of a running tool call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCancelRequest {
    /// Tool call id the requester wants canceled.
    pub target_call_id: ToolCallId,
}

/// Tool-provider observation or harness fact that one call was cancelled.
///
/// Tool/Core peers submit this payload as [`Event::ToolCancelledReported`].
/// The harness publishes [`Event::ToolCancelled`] only for accepted foreground
/// cancellation; backgrounded cancellation becomes [`ToolBackgroundError`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCancelled {
    /// Stable id of the cancelled tool call.
    pub call_id: ToolCallId,
    /// Tool name associated with the cancelled call.
    pub tool_name: ToolName,
    /// Protocol-level tool kind echoed from the request.
    pub tool_type: ToolType,
}

// ---------------------------------------------------------------------------
// Extension supervision events
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionStarting {
    pub instance_id: crate::ExtensionInstanceId,
    pub extension_name: ExtensionName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionReady {
    pub instance_id: crate::ExtensionInstanceId,
    pub extension_name: ExtensionName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionExited {
    pub instance_id: crate::ExtensionInstanceId,
    pub extension_name: ExtensionName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionRestarting {
    pub instance_id: crate::ExtensionInstanceId,
    pub extension_name: ExtensionName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Declares that this extension provides prompt context after matching
/// `session.agent_loaded` events.
///
/// The transient declaration commits before provider membership changes.
/// Registration controls wait participation but does not gate value or
/// readiness publication.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionContextProviderRegister {}

/// A transient session-provider declaration committed before provider
/// membership changes.
///
/// Effective registered Tool subscribers publish session-wide prompt context
/// after matching `session.started` events and acknowledge with
/// `extension.session_context_ready`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionSessionContextProviderRegister {}

/// Acknowledges that this extension finished publishing context for one agent.
///
/// The transient acknowledgement commits before effects. It releases only the
/// source from the exact session/agent/initialization wait; session discovery
/// readiness is independent. Registration does not gate publication.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionContextReady {
    /// Session containing the loaded agent.
    pub session_id: SessionId,
    /// Agent whose context contributions are complete for now.
    pub agent_id: AgentId,
    /// Exact initialization attempt whose provider wait this settles.
    pub agent_initialization_id: AgentInitializationId,
}

/// A transient session-discovery acknowledgement committed before it may
/// release session initialization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionSessionContextReady {
    /// Session whose session-wide context is complete for now.
    pub session_id: SessionId,
}

/// Arbitrary JSON value published by an extension for one agent context key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentContextValue(pub serde_json::Value);

/// Publishes this extension's complete agent-scoped contribution for one key.
///
/// The transient value commits before slot replacement. Registration and
/// current loaded-agent membership do not gate publication.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExtAgentContextPublish {
    /// Session containing the target initialization.
    pub session_id: SessionId,
    /// Agent this context belongs to.
    pub agent_id: AgentId,
    /// Exact initialization attempt receiving this contribution.
    pub agent_initialization_id: AgentInitializationId,
    /// Top-level context key exposed to templates under
    /// `agent_context.<key>`.
    pub key: AgentContextKey,
    /// Complete JSON contribution from this extension for the key.
    pub value: AgentContextValue,
}

/// An extension publishes or replaces one extension-level prompt fragment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtPromptFragmentPublish {
    /// Fragment template to make available during prompt rendering.
    ///
    /// The harness keys replacement by `(source_connection_id, fragment.name)`;
    /// the same extension publishing the same name again replaces its previous
    /// fragment.
    pub fragment: PromptFragment,
}

/// Configured-extension claim for an internal prompt's activation source.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InternalPromptActivationKind {
    /// Ordinary extension-authored internal prompt.
    InternalPrompt,
    /// Wakeup produced by an elapsed timer.
    Timer,
}

/// Request from an extension to submit a hidden internal prompt to a loaded
/// agent.
///
/// Requests default to transient delivery and never enter semantic history. The
/// harness commits an admitted request before validating its target or
/// submitting the prompt. It owns transcript publication; extensions must not
/// publish `agent.prompt_submitted` directly.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtInternalPromptSubmitRequest {
    /// Loaded agent that should receive the prompt.
    pub agent_id: AgentId,
    /// Prompt text to submit.
    pub text: String,
    /// Optional submitter correlation id copied to the created prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_id: Option<String>,
    /// Typed activation provenance claimed by the configured extension.
    ///
    /// Absence has the same meaning as
    /// [`InternalPromptActivationKind::InternalPrompt`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_kind: Option<InternalPromptActivationKind>,
}

/// Recipient of a global agent-to-agent or agent-to-user message.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentMessageRecipient {
    /// Deliver the message to another durable agent transcript.
    Agent { agent_id: AgentId },
    /// Deliver the message to an agent owned by another active harness session.
    ExternalAgent {
        /// Active session id of the harness that should receive the message.
        session_id: SessionId,
        /// Agent id within the target harness.
        agent_id: AgentId,
    },
    /// Deliver the message to the human user.
    User,
}

/// Source semantics for an agent-to-agent delivery.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageKind {
    /// An explicit `message` tool invocation.
    #[default]
    Message,
    /// An automatic `agent_watch` response notification.
    WatchResponse,
    /// An automatic `agent_watch` user-prompt notification.
    WatchPrompt,
    /// An automatic structured `agent_watch` outer agent-turn state
    /// notification.
    WatchTurnState,
    /// An automatic structured, sanitized provider-work status notification.
    WatchProviderStatus,
}

impl AgentMessageKind {
    /// Returns true when this value is the default message kind.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// A harness-authored durable sender-side projection of a message sent by one
/// agent to another agent or to the user.
///
/// External clients and extensions must not forge this event. The harness-owned
/// `message` tool validates the sender and recipient, then publishes this
/// durable transcript fact into the sender's transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentMessageSent {
    /// Stable id for this logical message, shared by sender/recipient
    /// projections.
    pub message_id: AgentMessageId,
    /// Agent id of the sender.
    pub sender_id: AgentId,
    /// Recipient agent or the human user.
    pub recipient: AgentMessageRecipient,
    /// Delivery source semantics.
    #[serde(default, skip_serializing_if = "AgentMessageKind::is_default")]
    pub kind: AgentMessageKind,
    /// Message body.
    pub message: String,
}

/// A harness-authored durable recipient-side projection of a message received
/// from another agent.
///
/// External clients and extensions must not forge this event. The harness emits
/// it only for agent recipients so the recipient transcript can represent the
/// inbound side distinctly from the sender transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentMessageReceived {
    /// Stable id for this logical message, shared by sender/recipient
    /// projections.
    pub message_id: AgentMessageId,
    /// Agent id of the sender.
    pub sender_id: AgentId,
    /// Active session id of an external sender. Absent for same-session/local
    /// senders and for legacy records written before cross-harness messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_session_id: Option<SessionId>,
    /// Recipient agent id that received the message.
    pub recipient_id: AgentId,
    /// Delivery source semantics.
    #[serde(default, skip_serializing_if = "AgentMessageKind::is_default")]
    pub kind: AgentMessageKind,
    /// Structured outer agent-turn state carried by
    /// [`AgentMessageKind::WatchTurnState`].
    ///
    /// This must be present exactly when `kind` is
    /// [`AgentMessageKind::WatchTurnState`] and absent for every other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_turn_state: Option<AgentWatchTurnStateNotification>,
    /// Structured status carried by [`AgentMessageKind::WatchProviderStatus`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_provider_status: Option<AgentWatchProviderStatusNotification>,
    /// Message body.
    pub message: String,
}

/// Provider-owned reason that an unchanged logical request will be retried.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRetryCategory {
    /// Network or transport failure.
    Transport,
    /// Provider overload.
    Overload,
    /// Provider throttling.
    Throttle,
    /// Provider usage-window exhaustion.
    UsageWindow,
    /// Provider account state.
    Account,
    /// Authentication or authorization state.
    Auth,
    /// Unclassified retryable provider failure.
    Unknown,
}

/// A closed, sanitized provider-work category safe for cross-agent delivery.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentWatchProviderCategory {
    /// Network or transport failure.
    Transport,
    /// Provider overload.
    Overload,
    /// Provider throttling.
    Throttle,
    /// Provider usage-window exhaustion.
    UsageWindow,
    /// Provider account state.
    Account,
    /// Authentication or authorization state.
    Auth,
    /// Unclassified retryable provider failure.
    Unknown,
    /// Context-window exhaustion or recovery.
    ContextWindow,
    /// Standalone or inline compaction work.
    Compaction,
}

impl AgentWatchProviderCategory {
    /// Stable wire-compatible label used in safe presentation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Overload => "overload",
            Self::Throttle => "throttle",
            Self::UsageWindow => "usage_window",
            Self::Account => "account",
            Self::Auth => "auth",
            Self::Unknown => "unknown",
            Self::ContextWindow => "context_window",
            Self::Compaction => "compaction",
        }
    }
}

/// Provider-owned structured retry facts alongside human UI status.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderRetryStatus {
    /// Sanitized retry category.
    pub category: ProviderRetryCategory,
    /// Saturating logical attempt count.
    pub attempt: u32,
    /// Approximate delay until the next attempt, rounded to whole seconds.
    pub next_retry_delay_secs: u32,
}

/// Invariant-preserving current state of watched provider work.
///
/// The phase is the serde discriminator rather than an independent field, so
/// malformed phase/category/attempt combinations cannot be constructed through
/// this API or accepted from the wire.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentWatchProviderState {
    /// The provider will retry the unchanged logical request.
    Retrying {
        /// Sanitized reason for retrying.
        category: AgentWatchProviderCategory,
        /// Saturating logical attempt count.
        attempt: u32,
        /// Approximate delay until the next attempt, rounded to whole seconds.
        next_retry_delay_secs: u32,
    },
    /// The harness is recovering from a canonical context-window rejection.
    RecoveringContext {
        /// Saturating provider attempt that triggered recovery.
        attempt: u32,
    },
    /// Durable work is blocked pending manual recovery.
    Blocked {
        /// Sanitized reason the work is blocked.
        category: AgentWatchProviderCategory,
    },
    /// Dispatch may have happened and automatic replay is unsafe.
    DispatchUncertain {
        /// Sanitized class of the uncertain work.
        category: AgentWatchProviderCategory,
    },
    /// Provider work ended in a terminal error.
    TerminalError {
        /// Sanitized terminal failure category.
        failure_kind: ProviderFailureKind,
        /// Saturating provider attempt correlated with this prompt.
        attempt: u32,
    },
}

impl AgentWatchProviderState {
    /// Stable wire-compatible phase label used in safe presentation.
    pub const fn phase_str(&self) -> &'static str {
        match self {
            Self::Retrying { .. } => "retrying",
            Self::RecoveringContext { .. } => "recovering_context",
            Self::Blocked { .. } => "blocked",
            Self::DispatchUncertain { .. } => "dispatch_uncertain",
            Self::TerminalError { .. } => "terminal_error",
        }
    }
}

/// Harness-authored current provider-work snapshot for one watched agent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentWatchProviderStatusNotification {
    /// Session containing the watch relation.
    pub session_id: SessionId,
    /// Fresh identity for the directed watch relation.
    pub subscription_id: String,
    /// Watched-agent outer-turn generation.
    pub turn_generation: u64,
    /// Prompt whose provider work produced this status.
    pub agent_prompt_id: AgentPromptId,
    /// Tagged provider-work state whose variants enforce phase invariants.
    pub state: AgentWatchProviderState,
    /// Whether this snapshot was returned when enabling an existing/new watch.
    pub initial: bool,
}

/// Structured state delivered to one watcher for a watched agent's outer turn.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentWatchTurnStateNotification {
    /// Session in which the session-local watch relation exists.
    pub session_id: SessionId,
    /// Fresh identity minted when this directed watch relation is enabled.
    pub subscription_id: String,
    /// Current two-state outer agent-turn state.
    pub state: AgentRuntimeState,
    /// Whether this is the snapshot sent when the watch was enabled.
    pub initial: bool,
    /// Harness-runtime-scoped watched-agent generation, incremented only when
    /// an idle agent starts an outer turn.
    pub turn_generation: u64,
}

/// Durable agent branch-state fact: the selected head moved, so the next
/// append should branch from that root-or-node target.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentHeadMoved {
    /// Agent whose selected branch head changed.
    pub agent_id: AgentId,
    /// Root or existing transcript node now selected as the branch head.
    pub head: AgentHead,
}

/// Immutable agent creation fact recorded at the start of an agent log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentStarted {
    /// Agent this log belongs to.
    pub agent_id: AgentId,
    /// Authenticated path that initiated this agent's creation. `None` decodes
    /// only pre-accounting legacy journals; forward harness writes always set
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<AgentCreator>,
    /// Optional parent agent whose inheritable metadata was copied at creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent: Option<AgentId>,
    /// Agent role used to build prompts for this agent.
    pub role: String,
    /// Optional human-friendly name for presenting this agent in UIs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Metadata facts present at creation time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metadata: Vec<AgentInitialMetadata>,
    /// Whether this agent's semantic transcript is memory-only in the current
    /// daemon and should not be expected after restart/resume.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ephemeral: bool,
}

/// Authenticated provenance for an immutable agent creation fact.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentCreator {
    /// A user-facing harness path created the agent.
    #[default]
    User,
    /// Another agent initiated creation, possibly across a session boundary.
    Agent {
        /// Session containing the initiating agent.
        session_id: SessionId,
        /// Initiating agent identity.
        agent_id: AgentId,
    },
    /// An extension initiated creation without an owning user or agent.
    Extension {
        /// Stable configured extension identity.
        name: ExtensionName,
        /// Runtime extension instance that authenticated the request.
        instance_id: ExtensionInstanceId,
    },
}

/// Stable identity of one non-overlapping outer agent turn.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AgentOuterTurnId(
    /// Exact `ot-` prefix followed by a validated [`AgentPromptId`].
    String,
);

/// Error returned when parsing an outer-turn identifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentOuterTurnIdParseError {
    /// The identifier did not begin with the required `ot-` prefix.
    InvalidPrefix,
    /// The prompt-identifier suffix was invalid.
    InvalidPromptId(crate::AgentPromptIdParseError),
}

impl std::fmt::Display for AgentOuterTurnIdParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPrefix => formatter.write_str("outer turn id must start with `ot-`"),
            Self::InvalidPromptId(error) => {
                write!(formatter, "invalid outer turn prompt id: {error}")
            }
        }
    }
}

impl std::error::Error for AgentOuterTurnIdParseError {}

impl AgentOuterTurnId {
    /// Derive the turn identity from its unique durable inference prompt.
    #[must_use]
    pub fn for_prompt(prompt_id: &AgentPromptId) -> Self {
        Self(format!("ot-{prompt_id}"))
    }

    /// Parse an outer-turn identifier in its exact derived representation.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, AgentOuterTurnIdParseError> {
        let value = value.as_ref();
        let prompt_id = value
            .strip_prefix("ot-")
            .ok_or(AgentOuterTurnIdParseError::InvalidPrefix)?
            .parse()
            .map_err(AgentOuterTurnIdParseError::InvalidPromptId)?;
        Ok(Self::for_prompt(&prompt_id))
    }
}

impl std::str::FromStr for AgentOuterTurnId {
    type Err = AgentOuterTurnIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> serde::Deserialize<'de> for AgentOuterTurnId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for AgentOuterTurnId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::ops::Deref for AgentOuterTurnId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

crate::validated_string_newtype!(
    /// Opaque stable correlation captured when a non-journaled input is accepted.
    AgentActivationCorrelationId,
    AgentActivationCorrelationIdParseError,
    "agent activation correlation id",
    128
);

crate::validated_string_newtype!(
    /// Unique identity for one harness accounting runtime.
    AccountingRuntimeId,
    AccountingRuntimeIdParseError,
    "accounting runtime id",
    32
);

/// Stable initiating occurrence for an outer turn.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "details", rename_all = "snake_case")]
pub enum AgentOuterTurnActivation {
    /// An exact occurrence in the owning agent journal.
    Journal {
        /// Durable transcript node that initiated the turn.
        occurrence: AgentHead,
    },
    /// A stable correlation copied from accepted non-journaled input.
    External {
        /// Opaque harness-minted accepted-input correlation.
        correlation_id: AgentActivationCorrelationId,
    },
}

/// Harness-authored durable fact that an accepted activation started an outer
/// turn.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentOuterTurnStarted {
    /// Agent executing the turn.
    pub agent_id: AgentId,
    /// Session to which this occurrence is attributed.
    pub session_id: SessionId,
    /// Stable per-agent outer-turn identity.
    pub outer_turn_id: AgentOuterTurnId,
    /// Durable inference checkpoint/prompt that owns this turn.
    pub agent_prompt_id: AgentPromptId,
    /// Harness-runtime identity used to distinguish a valid post-crash start
    /// from overlapping starts in one runtime.
    pub runtime_id: AccountingRuntimeId,
    /// Exact durable transcript occurrence that initiated this activation.
    pub activation: AgentOuterTurnActivation,
}

/// Terminal disposition of a durably bounded outer turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOuterTurnDisposition {
    /// The harness settled all foreground work and returned the agent to idle.
    Settled,
}

/// Harness-authored durable fact that an outer agent turn returned to idle.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentOuterTurnFinished {
    /// Agent that executed the turn.
    pub agent_id: AgentId,
    /// Session to which this occurrence is attributed.
    pub session_id: SessionId,
    /// Identity copied from the matching start fact.
    pub outer_turn_id: AgentOuterTurnId,
    /// Terminal outcome selected at the actual running-to-idle transition.
    pub disposition: AgentOuterTurnDisposition,
}

/// Content-free durable fact recording one accepted visible user interaction.
///
/// The enclosing persisted record supplies the acceptance timestamp. Keeping
/// prompt content out of this fact lets derived summaries recover interaction
/// ordering even when a queued prompt is later recalled.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentUserInteractionRecorded {
    /// Agent that accepted the visible user interaction.
    pub agent_id: AgentId,
}

/// Payload shared by a metadata-set request and its durable canonical fact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentMetadataSet {
    /// Target agent for the requested or recorded metadata operation.
    pub agent_id: AgentId,
    /// Metadata key requested or recorded as replaced.
    pub key: AgentMetadataKey,
    /// Arbitrary CBOR metadata value requested or recorded.
    pub value: CborValue,
    /// Optional opaque mutation correlation echoed unchanged if a canonical
    /// fact is published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutation_id: Option<crate::AgentMetadataMutationId>,
    /// Requested or recorded child-agent inheritance behavior for this key.
    #[serde(default, skip_serializing_if = "is_false")]
    pub inheritable: bool,
}

/// Payload shared by a metadata-unset request and its durable canonical fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentMetadataUnset {
    /// Target agent for the requested or recorded metadata operation.
    pub agent_id: AgentId,
    /// Metadata key requested or recorded as deleted.
    pub key: AgentMetadataKey,
}

pub const MAX_AGENT_METADATA_VALUE_BYTES: usize = 64 * 1024;
pub const MAX_AGENT_METADATA_KEY_BYTES: usize = 256;
/// Maximum opaque metadata mutation-correlation identifier size.
pub const MAX_AGENT_METADATA_MUTATION_ID_BYTES: usize = crate::AGENT_METADATA_MUTATION_ID_MAX_BYTES;

/// Durable fact that updates an agent's human-friendly display name.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentDisplayNameSet {
    /// Agent whose display name changed.
    pub agent_id: AgentId,
    /// New human-friendly display name. Empty names are rejected by the
    /// harness.
    pub display_name: String,
}

/// Session membership fact: an agent is now loaded in a session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionAgentLoaded {
    /// Session membership container.
    pub session_id: SessionId,
    /// Agent now available in the session.
    pub agent_id: AgentId,
    /// Fresh correlation minted for this runtime load attempt.
    pub agent_initialization_id: AgentInitializationId,
    /// Whether this membership is live/memory-only because the agent is
    /// ephemeral in the current daemon.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ephemeral: bool,
}

/// Session membership fact: an agent is no longer loaded in a session.
///
/// Durable agents record this in the session store; ephemeral agents only fold
/// it into the live in-memory session view.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionAgentUnloaded {
    /// Session membership container.
    pub session_id: SessionId,
    /// Agent removed from the session.
    pub agent_id: AgentId,
}

/// Transient request to start a side-agent conversation.
///
/// The harness commits configured peer requests before processing them, but raw
/// requests never enter semantic history.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StartAgentRequest {
    /// Requester-assigned correlation id, echoed back on accepted/result
    /// events.
    pub query_id: String,
    /// User-style instruction text. Appended to the current
    /// conversation's history as a `User` message before dispatch.
    pub instruction: String,
    /// Requested agent role for this side conversation. Tool-backed
    /// delegate queries default to `engineer`; non-tool queries without
    /// a role keep using the currently selected interactive role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Input stats for the extension-provided instruction, excluding
    /// any private prefix the extension may have added.
    #[serde(default, skip_serializing_if = "ToolUseStats::is_empty")]
    pub input_stats: ToolUseStats,
    /// `ToolCallId` of the tool invocation that triggered this query,
    /// when the extension is implementing a tool-backed side query. The current
    /// first-party `agent_start` path uses this only for side-agent teardown
    /// and background ownership; progress is reported with generic agent
    /// stats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    /// Human-readable name for the delegated task. Optional for the same reason
    /// `tool_call_id` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_name: Option<String>,
    /// Optional parent agent whose inheritable metadata should be copied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent: Option<AgentId>,
}

/// A [`StartAgentRequest`] was accepted for side-agent startup.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StartAgentAccepted {
    /// Request correlation id copied from [`StartAgentRequest::query_id`].
    pub query_id: String,
    /// Harness-minted side-agent id for the accepted request.
    pub agent_id: AgentId,
}

/// Final reply to a [`StartAgentRequest`]. `text` is the agent's final answer
/// (empty when `error` is set).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StartAgentResult {
    /// Request correlation id copied from [`StartAgentRequest::query_id`].
    pub query_id: String,
    /// Final agent answer. Empty when [`Self::error`] is set.
    pub text: String,
    /// Failure message when the request could not be started or completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Transient runtime state of an agent as observed by the harness.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRuntimeState {
    /// The agent has no prompt currently running.
    #[default]
    Idle,
    /// The agent owns live work such as an in-flight provider prompt or tool
    /// execution.
    Running,
}

/// Current runtime state for an agent.
///
/// This is a transient agent-state snapshot: it describes live work owned by
/// the harness and is not part of the durable agent transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentStateChanged {
    /// Agent whose live runtime state changed.
    pub agent_id: AgentId,
    /// New transient runtime state for the agent.
    pub state: AgentRuntimeState,
}

/// Cause associated with an [`AgentWatchesUpdated`] snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentWatchUpdateCause {
    /// A successful `agent_start` tool call enabled the watch automatically.
    AgentStart,
    /// A successful `agent_watch` tool call enabled the watch.
    AgentWatchEnable,
    /// A successful `agent_watch` tool call disabled the watch.
    AgentWatchDisable,
    /// The harness pruned a stale watcher after delivery failed.
    WatcherPruned,
    /// Current in-memory state replayed to a late subscriber.
    SessionSnapshot,
}

/// Complete session-local watch set for one watcher agent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentWatchesUpdated {
    /// Session that owns these transient watch relationships.
    pub session_id: SessionId,
    /// Agent receiving watch notifications.
    pub watcher_id: AgentId,
    /// Complete replacement set of agents currently watched by `watcher_id`.
    pub watched_agent_ids: Vec<AgentId>,
    /// Agent whose relationship changed, when the snapshot follows one
    /// mutation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_agent_id: Option<AgentId>,
    /// Reason this snapshot was emitted.
    pub cause: AgentWatchUpdateCause,
}

/// Current tool counters for an agent.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentToolStats {
    /// Tool calls currently in flight for this agent.
    pub in_flight: u32,
    /// Cumulative tool calls started while the agent is loaded in this harness.
    pub started_total: u32,
}

/// Most recently known context usage for an agent.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentContextStats {
    /// Latest provider-reported input token count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Latest provider-reported cached input token count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// Model context window in tokens, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Percent of the context window used, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent_used: Option<u8>,
}

/// Harness-owned classification controlling whether a loaded agent appears in
/// UI navigation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentNavigationMode {
    /// Always include the loaded agent in navigation targets.
    #[default]
    Active,
    /// Include the loaded agent only while its outer turn is running.
    ActiveAuto,
    /// Exclude the loaded agent from navigation targets.
    Suspended,
}

/// Complete operational snapshot for one loaded agent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentStatsUpdated {
    /// Session that owns this transient agent runtime state.
    pub session_id: SessionId,
    /// Agent described by this snapshot.
    pub agent_id: AgentId,
    /// Current harness-owned navigation classification.
    pub navigation_mode: AgentNavigationMode,
    /// Current harness runtime state for the agent.
    pub runtime_state: AgentRuntimeState,
    /// Current and cumulative tool counters.
    pub tools: AgentToolStats,
    /// Latest context usage known to the harness.
    pub context: AgentContextStats,
    /// Runtime-lifetime estimated equivalent API cost for this agent.
    #[serde(default)]
    pub estimated_api_cost: crate::EstimatedApiCost,
}
/// A modality that a provider route can accept as prompt input.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputModality {
    /// UTF-8 text input.
    Text,
    /// Validated raster image input.
    Image,
}

/// Metadata for one model currently served by a provider extension.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelInfo {
    /// Fully-qualified model id. The provider segment is part of user-visible
    /// selection and harness routing.
    pub id: ModelId,
    /// Optional human-friendly label. UIs may fall back to [`Self::id`] when it
    /// is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Provider-published model capability tags used by harness-owned policy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<ModelTag>,
    /// Tool definition kinds this model's provider can deliver upstream.
    ///
    /// An empty list preserves compatibility with older providers and means
    /// function tools only; custom tools require explicit publication.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_tool_types: Vec<ToolType>,
    /// Input modalities accepted by the exact provider/model route.
    ///
    /// Omitted legacy metadata means text-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<InputModality>,
    /// Modalities accepted specifically inside native tool-result output.
    ///
    /// Image-reading tools require image support in both this field and
    /// [`Self::input_modalities`]. Omitted legacy metadata means text-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_result_modalities: Vec<InputModality>,
    /// Whether the exact published route supports multiple tool calls in one
    /// model response. This is an effective route capability, not merely an
    /// abstract model capability.
    #[serde(default = "default_true")]
    pub supports_parallel_tool_calls: bool,
    /// Provider-published preference for becoming the implicit default model
    /// when the selected role does not name one. Higher values win; ties are
    /// broken by model id for deterministic behavior. Zero means neutral.
    #[serde(default, skip_serializing_if = "is_default_affinity_neutral")]
    pub default_affinity: i32,
    /// Total model context window in tokens. Required so harness/UI state does
    /// not have to fall back to provider-specific config.
    pub context_window: u64,
    /// Reasoning-effort levels accepted by this model, in UI cycling order.
    /// Empty means the model does not support reasoning-effort selection.
    pub efforts: Vec<Effort>,
    /// Output-verbosity levels accepted by this model, in UI cycling order.
    /// Providers that do not expose verbosity selection should publish
    /// [`Verbosity::Medium`] rather than an empty list.
    pub verbosities: Vec<Verbosity>,
    /// Thinking-summary modes accepted by this model, in UI cycling order.
    /// Providers that do not support summaries should publish
    /// [`ThinkingSummary::Off`] rather than an empty list.
    pub thinking_summaries: Vec<ThinkingSummary>,
    /// Whether this model can use provider/server-side context compaction.
    #[serde(default)]
    pub supports_compaction: bool,
    /// Whether this model supports a standalone replacement-window compaction
    /// operation, independently of inline context management.
    #[serde(default)]
    pub supports_standalone_compaction: bool,
    /// Provider-recommended token threshold for harness-scheduled standalone
    /// compaction. `None` means no provider default is published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standalone_compaction_threshold: Option<u64>,
    /// Estimated USD price per million uncached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_uncached_input_cost_1m_usd: Option<crate::EstimatedUsdPerMillion>,
    /// Estimated USD price per million provider-reported cached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_cached_input_cost_1m_usd: Option<crate::EstimatedUsdPerMillion>,
    /// Estimated USD price per million output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_output_cost_1m_usd: Option<crate::EstimatedUsdPerMillion>,
}

impl ProviderModelInfo {
    /// Resolve explicit basic pricing, using the central GPT-5.6 equivalent for
    /// each omitted price.
    #[must_use]
    pub fn estimated_api_cost_rates(&self) -> crate::EstimatedApiCostRates {
        crate::EstimatedApiCostRates {
            uncached_input: self
                .est_uncached_input_cost_1m_usd
                .unwrap_or(crate::ESTIMATED_API_COST_FALLBACK.uncached_input),
            cached_input: self
                .est_cached_input_cost_1m_usd
                .unwrap_or(crate::ESTIMATED_API_COST_FALLBACK.cached_input),
            output: self
                .est_output_cost_1m_usd
                .unwrap_or(crate::ESTIMATED_API_COST_FALLBACK.output),
        }
    }
}

/// Provider extension declaration of its currently available models.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelsDeclared {
    /// Complete replacement declaration. An empty list means the provider
    /// declares no currently servable models.
    pub models: Vec<ProviderModelInfo>,
}

/// Harness-authored canonical current state for one stable provider publisher.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelsUpdated {
    /// Stable configured provider extension that owns this replacement
    /// snapshot.
    pub publisher_extension_id: ExtensionName,
    /// Complete accepted replacement snapshot. An empty list means the provider
    /// currently serves no models.
    pub models: Vec<ProviderModelInfo>,
}

/// Extension-defined event payload.
///
/// `name` is the dotted event name used for routing and subscription
/// matching. `payload` carries extension-owned CBOR data. `session_id`, when
/// set, is runtime routing/context metadata; custom events are not folded into
/// durable semantic logs unless a typed durable event is added for that use
/// case. The name must use an extension-owned category, not one of Tau's
/// reserved first-party categories such as `tool`, `harness`, `agent`, or
/// `extension`; this prevents custom payloads from spoofing typed protocol
/// events in routing code keyed by [`Event::name`].
#[derive(Clone, Debug, PartialEq)]
pub struct CustomEvent {
    name: EventName,
    session_id: Option<SessionId>,
    payload: CborValue,
}

/// Error returned when a custom event uses a reserved first-party event
/// category.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidCustomEventName {
    name: EventName,
}
impl InvalidCustomEventName {
    /// Rejected event name.
    #[must_use]
    pub fn name(&self) -> &EventName {
        &self.name
    }

    /// Consume the error and return the rejected event name.
    #[must_use]
    pub fn into_name(self) -> EventName {
        self.name
    }
}

impl fmt::Display for InvalidCustomEventName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "custom event name must use an extension-owned category, got {}",
            self.name
        )
    }
}

impl std::error::Error for InvalidCustomEventName {}

impl CustomEvent {
    /// Create a custom event with a validated extension-owned event name.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidCustomEventName`] when `name` uses a reserved
    /// first-party event category.
    pub fn try_new(
        name: EventName,
        session_id: Option<SessionId>,
        payload: CborValue,
    ) -> Result<Self, InvalidCustomEventName> {
        if !Self::name_is_allowed(&name) {
            return Err(InvalidCustomEventName { name });
        }
        Ok(Self {
            name,
            session_id,
            payload,
        })
    }

    /// Event name used for routing and subscription matching.
    #[must_use]
    pub fn name(&self) -> &EventName {
        &self.name
    }

    /// Optional session metadata associated with this custom event.
    #[must_use]
    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    /// Extension-owned CBOR payload.
    #[must_use]
    pub fn payload(&self) -> &CborValue {
        &self.payload
    }

    /// Returns `true` when `name` has valid segments and uses an
    /// extension-owned event category.
    #[must_use]
    pub fn name_is_allowed(name: &EventName) -> bool {
        matches!(
            EventCategory::from_wire(name.category().as_str()),
            EventCategory::Other(_)
        )
    }
}

impl Serialize for CustomEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if !Self::name_is_allowed(&self.name) {
            return Err(serde::ser::Error::custom(InvalidCustomEventName {
                name: self.name.clone(),
            }));
        }

        #[derive(Serialize)]
        struct WireCustomEvent<'a> {
            name: &'a EventName,
            #[serde(skip_serializing_if = "Option::is_none")]
            session_id: Option<&'a SessionId>,
            payload: &'a CborValue,
        }

        WireCustomEvent {
            name: &self.name,
            session_id: self.session_id.as_ref(),
            payload: &self.payload,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CustomEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireCustomEvent {
            name: EventName,
            #[serde(default)]
            session_id: Option<SessionId>,
            payload: CborValue,
        }

        let wire = WireCustomEvent::deserialize(deserializer)?;
        Self::try_new(wire.name, wire.session_id, wire.payload).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// UI events — facts from the user interface
// ---------------------------------------------------------------------------

/// Classifies whether a prompt-like message came from the human user or from
/// harness internals.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptMessageClass {
    /// Default — visible user-authored prompt text.
    #[default]
    User,
    /// Internal control text that still belongs in model context.
    ///
    /// UIs hide this by default; a durable event may carry a typed presentation
    /// exception such as [`InternalPromptKind::ContextSizeAlert`].
    Internal,
}

impl PromptMessageClass {
    /// Returns true for internal prompt text excluded from user-prompt
    /// metadata.
    ///
    /// UIs may still render a separately typed internal presentation.
    #[must_use]
    pub fn is_internal(self) -> bool {
        matches!(self, Self::Internal)
    }
}

/// The user submitted a prompt in the UI.
///
/// `originator` is normally [`PromptOriginator::User`] — the field
/// exists so the harness can re-use this event type when dispatching
/// side queries spawned by extensions. The harness routes this UI request to a
/// concrete agent and publishes an `AgentPromptSubmitted` transcript fact when
/// the prompt is accepted. For an authenticated visible-user request to an
/// existing target, acceptance also makes that target's harness-owned
/// navigation mode `Active` before queue or dispatch and publishes a fresh
/// complete stats snapshot. The required `agent_id` is the routing target;
/// agent-like text mentions do not retarget the request. UIs and other
/// extensions filter on `originator.is_user()` to avoid rendering side
/// conversations as real user turns.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiPromptSubmitted {
    pub session_id: SessionId,
    pub text: String,
    /// Whether the canonical text came from a doubled-colon literal escape and
    /// must bypass harness-owned prompt command processing.
    #[serde(default, skip_serializing_if = "is_false")]
    pub literal: bool,
    /// Agent that should receive this prompt. Agent creation is explicit via
    /// [`UiCreateAgent`]; prompt submissions only target existing agents.
    pub agent_id: AgentId,
    /// Whether this prompt text is user-authored or internal control text.
    #[serde(default)]
    pub message_class: PromptMessageClass,
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Free-form correlation tag chosen by the submitter and copied
    /// forward onto the first [`AgentPromptCreated`] the harness
    /// emits for this prompt. Lets a client (notably the test helper
    /// in `tau-harness::daemon`) match the response chain to the
    /// submission it made, without relying on event ordering or
    /// re-using a long-lived connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_id: Option<String>,
}

/// A trailing-edge debounced snapshot of the in-progress prompt the
/// user is composing in the UI. Emitted at most once per second
/// while the user is typing; carries the full current contents of
/// the prompt buffer.
///
/// Defaults to transient, but caller-selected Emit metadata remains
/// independent: the interactive CLI currently sends `persist=true`. The
/// harness preserves either bit while excluding drafts from semantic stores and
/// replay. Subscribers use it to detect "user is alive" without polling: e.g.
/// std-notifications resets its idle deadline on every draft event so the
/// desktop notification doesn't fire while the user is mid-sentence.
///
/// The target agent scopes the currently viewed transcript at the moment the
/// snapshot was captured. This keeps future draft restore, autocomplete, or
/// cross-UI synchronization consumers from guessing which visible agent a draft
/// belonged to. Modern producers must set it to `Some(agent_id)` when the draft
/// belongs to an existing loaded agent transcript. `None` means the draft is
/// session-level/unscoped, normally because the UI is composing the
/// start-new-agent prompt; legacy peers whose payloads predate this field also
/// decode as `None`, so stateful consumers must not reinterpret absence as the
/// current agent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiPromptDraft {
    /// Session whose attached UI owns the prompt buffer.
    pub session_id: SessionId,
    /// Existing agent transcript viewed by the user while editing this draft.
    ///
    /// `None` means the draft is session-level/unscoped, normally the
    /// start-new-agent prompt. It is also the compatibility value for legacy
    /// payloads that did not carry target scoping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent_id: Option<AgentId>,
    /// Full current prompt-buffer contents.
    pub text: String,
}

/// Synthetic boundary emitted after one loaded agent's historical replay.
///
/// The harness creates this transient event as a non-replay delivery. It is not
/// accepted from peers and is not persisted; ordered delivery lets restore
/// handlers treat it as "all earlier replay frames for this agent have run".
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentReplayComplete {
    /// Agent whose replay batch has reached its boundary.
    pub agent_id: AgentId,
    /// Session for which the agent was replayed, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// Replay error for this agent, if catch-up could not complete normally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Synthetic boundary emitted after session-scoped historical replay.
///
/// The harness creates this transient event as a non-replay delivery. It is not
/// accepted from peers and is not persisted; live deliveries for the connection
/// are released only after this boundary is sent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionReplayComplete {
    /// Session whose catch-up phase has reached its boundary.
    pub session_id: SessionId,
    /// Replay error for this session, if catch-up could not complete normally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The UI terminal focus state changed. Emitted when the terminal supports
/// focus-in/focus-out reporting and the user moves focus into or away from the
/// Tau terminal window.
///
/// Like [`UiPromptDraft`], this event defaults to transient while the
/// interactive CLI currently sends `persist=true`; the harness preserves
/// either bit and excludes the observation from semantic stores and replay.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiFocusChanged {
    /// Session whose attached UI observed the focus change.
    pub session_id: SessionId,
    /// Whether the terminal reported focus gained (`true`) or lost (`false`).
    pub focused: bool,
}

/// The user requests switching to an agent role.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiRoleSelect {
    /// Role name to make the runtime source of truth for model resolution.
    pub role: String,
}

/// The user requests switching the model used by one loaded agent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiAgentModelSelect {
    /// Session whose agent should be updated.
    pub session_id: SessionId,
    /// Agent to update. `None` asks the harness to use the session's only
    /// unambiguous loaded user agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent_id: Option<AgentId>,
    /// Model to use for future prompts sent to the target agent.
    pub model: ModelId,
}

/// The user changes or deletes an agent role.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiRoleUpdate {
    /// Role name whose runtime override should change.
    pub role: String,
    /// Typed mutation to apply to the role override.
    pub action: UiRoleUpdateAction,
}

/// Typed role mutation requested by a UI. `None` fields clear the explicit
/// role value so normal model-specific fallback resolution applies.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UiRoleUpdateAction {
    /// Remove this role's runtime override, or delete the runtime-only role.
    Delete,
    /// Set or clear the role's preferred model.
    SetModel {
        /// Model to pin this role to, or `None` to use the first available
        /// model.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelId>,
    },
    /// Set or clear the role's reasoning effort.
    SetEffort {
        /// Reasoning effort to store, or `None` to use the model fallback.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<Effort>,
    },
    /// Set or clear the role's output verbosity.
    SetVerbosity {
        /// Output verbosity to store, or `None` to use the model fallback.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verbosity: Option<Verbosity>,
    },
    /// Set or clear the role's thinking-summary mode.
    SetThinkingSummary {
        /// Thinking-summary mode to store, or `None` to use the model fallback.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_summary: Option<ThinkingSummary>,
    },
    /// Set or clear the role's provider service tier.
    SetServiceTier {
        /// Service tier to store, or `None` to use the provider default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service_tier: Option<ServiceTier>,
    },
    /// Set or clear the role's automatic compaction token threshold.
    SetCompactionThreshold {
        /// Token threshold at which automatic server-side compaction should
        /// start, or `None` to use the provider/server default behavior.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compaction_threshold: Option<u64>,
    },
    /// Set or clear the role's explicit tool allow-list.
    SetTools {
        /// Internal tool names to allow. `None` clears back to default tool
        /// enablement; `Some(Vec::new())` is an explicit empty allow-list.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tools: Option<Vec<ToolName>>,
    },
    /// Set the role's additive tool-group allow-list.
    SetEnableToolGroups {
        /// Tool group names to enable before individual tool overrides are
        /// applied.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        enable_tool_groups: Vec<ToolGroupName>,
    },
    /// Set the role's explicit tool-group block-list.
    SetDisableToolGroups {
        /// Tool group names to disable before individual tool overrides are
        /// applied.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        disable_tool_groups: Vec<ToolGroupName>,
    },
    /// Set the role's additive tool allow-list.
    SetEnableTools {
        /// Internal tool names to enable in addition to defaults or the
        /// explicit `tools` allow-list.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        enable_tools: Vec<ToolName>,
    },
    /// Set the role's explicit tool block-list.
    SetDisableTools {
        /// Internal tool names to disable even when enabled by default or
        /// explicitly allowed.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        disable_tools: Vec<ToolName>,
    },
}

/// The user requests switching to a different session within the same
/// daemon. Harness emits `SessionShutdown` for the current session,
/// then `SessionStarted { reason: New | Resume }` for the new one,
/// and waits for extensions to acknowledge re-init.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiSwitchSession {
    pub new_session_id: SessionId,
    /// `New` if the id was just minted, `Resume` if it points at an
    /// existing session on disk.
    pub reason: SessionStartReason,
}

/// The UI requests creation of an agent and may include the first prompt
/// that should be submitted to it. This is the explicit boundary between
/// pre-agent UI state (role/cwd can still change freely) and agent state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiCreateAgent {
    /// Session in which the agent should be loaded.
    pub session_id: SessionId,
    /// Role to bind to the new agent.
    pub role: String,
    /// Model override to apply to the new agent before its first prompt is
    /// dispatched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<ModelId>,
    /// Initial metadata facts to publish for the new agent.
    ///
    /// The harness fills in the newly-created agent id when publishing these
    /// as durable `agent.metadata_set` events.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metadata: Vec<AgentInitialMetadata>,
    /// Optional first prompt to append after agent context has been loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<String>,
    /// Whether `initial_prompt` came from a doubled-colon literal escape and
    /// must bypass harness-owned prompt command processing.
    #[serde(default, skip_serializing_if = "is_false")]
    pub literal: bool,
    /// Whether the initial prompt is user-authored or hidden internal control
    /// text.
    #[serde(default)]
    pub message_class: PromptMessageClass,
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Correlation tag copied forward onto the first `AgentPromptCreated` for
    /// `initial_prompt`, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_id: Option<String>,
    /// Optional parent agent whose inheritable metadata should be copied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent: Option<AgentId>,
    /// Whether the new agent should keep its semantic transcript and session
    /// membership in memory only for the lifetime of the current daemon.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ephemeral: bool,
}

/// Initial metadata value requested while creating a new UI-owned agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentInitialMetadata {
    /// Metadata key to publish for the new agent.
    pub key: AgentMetadataKey,
    /// Metadata value to publish for the new agent.
    pub value: CborValue,
    /// Whether the metadata should be inherited by child agents.
    #[serde(default, skip_serializing_if = "is_false")]
    pub inheritable: bool,
}

/// UI request to set an agent display name.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiSetAgentDisplayName {
    /// Session in which the target agent must be loaded or known.
    pub session_id: SessionId,
    /// Agent whose display name should be changed.
    pub agent_id: AgentId,
    /// New human-friendly display name.
    pub display_name: String,
}

/// The user typed `:tree <target>`: move an agent's head pointer to a
/// user-facing prompt anchor, root, or explicit raw node.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiNavigateTree {
    pub session_id: SessionId,
    /// Target agent tree to navigate. `None` leaves selection to the harness's
    /// current/default conversation state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent_id: Option<AgentId>,
    /// User-facing or explicit expert navigation target.
    pub target: UiTreeNavigationTarget,
}

/// The user typed `:compact`: force a provider-side compaction pass on
/// the target agent history before the next prompt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiCompactRequest {
    pub session_id: SessionId,
    /// Target agent conversation to compact. `None` leaves selection to the
    /// harness's current/default conversation state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent_id: Option<AgentId>,
}

/// Stop advancing an in-flight prompt at the next harness boundary.
///
/// Originally tied to the user typing `:cancel`, now also published
/// by the harness itself to preempt non-tool extension side
/// conversations when a user prompt arrives. The optional
/// [`Self::agent_prompt_id`] disambiguates the two cases:
///
/// - `None` — broadcast cancel for the selected target conversation. The
///   harness uses the current/default conversation when `target_agent_id` is
///   absent; the agent aborts whatever prompt it's currently retry-sleeping on.
/// - `Some(spid)` — targeted cancel. The agent only aborts if the in-flight
///   prompt's spid matches; otherwise the frame is left in the retry-loop's
///   deferred buffer so the wrong prompt isn't collateral damage. The agent
///   serializes prompt processing internally, so a cancel published while the
///   spid in question is still queued (not yet dequeued from the agent's frame
///   channel) is harmless — it just falls through with no in-flight match.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiCancelPrompt {
    /// Session whose active or queued prompt should be cancelled.
    pub session_id: SessionId,
    /// Target agent conversation to cancel. `None` leaves selection to the
    /// harness's current/default conversation state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent_id: Option<AgentId>,
    /// Optional target. See struct doc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_prompt_id: Option<AgentPromptId>,
}

/// Maximum encoded length of a manual provider-retry correlation identifier.
pub const MAX_RETRY_PROMPT_REQUEST_ID_LEN: usize = 64;

/// Correlation identifier for one manual provider-retry request.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RetryPromptRequestId(String);

impl RetryPromptRequestId {
    /// Validates and constructs a bounded, path-safe correlation identifier.
    pub fn parse(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.is_empty() {
            return Err("retry prompt request id must not be empty");
        }
        if value.len() > MAX_RETRY_PROMPT_REQUEST_ID_LEN {
            return Err("retry prompt request id is too long");
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err("retry prompt request id contains invalid characters");
        }
        Ok(Self(value))
    }

    /// Returns the validated wire representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RetryPromptRequestId {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<RetryPromptRequestId> for String {
    fn from(value: RetryPromptRequestId) -> Self {
        value.0
    }
}

/// Request that a selected prompt's provider-owned delayed retry run now.
///
/// UIs leave [`Self::agent_prompt_id`] empty. The harness resolves and fills it
/// before directing the request to the provider that owns the prompt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiRetryPrompt {
    /// Correlates the provider result with the invoking UI.
    pub request_id: RetryPromptRequestId,
    /// Session captured when the UI submitted the command.
    pub session_id: SessionId,
    /// Agent captured when the UI submitted the command.
    pub target_agent_id: Option<AgentId>,
    /// Exact logical prompt, filled only by the harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_prompt_id: Option<AgentPromptId>,
}

/// Authoritative outcome of a provider scheduler's atomic ownership check.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryPromptStatus {
    /// The parked job was transferred to the normal runnable queue.
    Accepted,
    /// The scheduler did not own a parked job with this prompt id.
    NotParked,
}

/// Provider-authored `provider.retry_prompt_result_reported` payload for a
/// targeted [`UiRetryPrompt`]. The harness validates its private correlation
/// and sends the requester a [`UiRetryPromptResult`]; there is no canonical
/// provider event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderRetryPromptResult {
    /// Correlation identifier copied from the request.
    pub request_id: RetryPromptRequestId,
    /// Exact logical prompt checked by the provider scheduler.
    pub agent_prompt_id: AgentPromptId,
    /// Result of the atomic ownership check.
    pub status: RetryPromptStatus,
}

/// Requester-directed outcome of a manual retry command.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiRetryPromptResult {
    /// Correlation identifier copied from the request.
    pub request_id: RetryPromptRequestId,
    /// Agent captured by the request, when one was resolvable.
    pub target_agent_id: Option<AgentId>,
    /// Stable display label captured before asynchronous provider work.
    pub target_label: String,
    /// Authoritative scheduler result, or `None` for harness rejection.
    pub status: Option<RetryPromptStatus>,
    /// User-facing result text.
    pub message: String,
}

/// Absolute navigation-mode mutation requested by a UI.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiAgentNavigationModeAction {
    /// Set the mode to [`AgentNavigationMode::Active`].
    SetActive,
    /// Set the mode to [`AgentNavigationMode::ActiveAuto`].
    SetActiveAuto,
    /// Set the mode to [`AgentNavigationMode::Suspended`].
    SetSuspended,
}

/// Request to change one loaded agent's shared navigation mode.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiSetAgentNavigationMode {
    /// Correlates the requester-directed result with this request.
    pub request_id: String,
    /// Session captured when the UI submitted the request.
    pub session_id: SessionId,
    /// Loaded agent whose mode should change.
    pub agent_id: AgentId,
    /// Absolute mode to apply.
    pub action: UiAgentNavigationModeAction,
}

/// Stable reason why a navigation-mode mutation was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiSetAgentNavigationModeRejection {
    /// The request names a session other than the harness binding.
    StaleSession,
    /// The target is not currently loaded.
    AgentNotLoaded,
}

/// Outcome of a navigation-mode mutation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UiSetAgentNavigationModeOutcome {
    /// The absolute write was accepted at one event-loop position.
    Applied,
    /// The request did not mutate shared state.
    Rejected {
        /// Stable rejection reason.
        reason: UiSetAgentNavigationModeRejection,
    },
}

/// Requester-directed acknowledgement of a navigation-mode mutation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiSetAgentNavigationModeResult {
    /// Correlation identifier copied from the request.
    pub request_id: String,
    /// Session copied from the request.
    pub session_id: SessionId,
    /// Agent copied from the request.
    pub agent_id: AgentId,
    /// Authoritative acceptance or rejection at processing time.
    pub outcome: UiSetAgentNavigationModeOutcome,
}

/// Request that the harness remove and return the most recently queued user
/// prompt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiRecallQueuedPrompt {
    /// Session whose conversation queue should be recalled from.
    pub session_id: SessionId,
    /// Target agent conversation to recall from. `None` leaves selection to the
    /// harness's current/default conversation state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent_id: Option<AgentId>,
}

/// Which stream a [`ShellCommandProgress`] chunk came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellStream {
    Stdout,
    Stderr,
}

/// The user submitted a `!`/`!!` shell command.
///
/// `include_in_context`: when `true` (from `!<cmd>`), the harness
/// injects a tagged user message containing the command and its
/// output into the target agent's transcript on completion, so the
/// agent sees it on its next turn. When `false` (from `!!<cmd>`),
/// the result is UI-only and never reaches the model.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiShellCommand {
    pub session_id: SessionId,
    /// UI lifecycle correlation id. The harness accepts 1..=256 UTF-8 bytes
    /// and requires uniqueness until the terminal event commits.
    pub command_id: crate::ShellCommandId,
    pub command: String,
    pub include_in_context: bool,
    /// Target agent for this user-authored shell command. `None` means no
    /// explicit target; the harness uses its default conversation state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent_id: Option<AgentId>,
}

/// A chunk of output from a running user-initiated shell command.
/// Correlated to the request by `command_id`.
///
/// The selected provider submits this payload as
/// [`Event::ShellCommandProgressReported`] with the opaque command id received
/// from the harness. After commit and routed-owner validation, the harness maps
/// that provider route id back to the UI lifecycle id and publishes
/// [`Event::ShellCommandProgress`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellCommandProgress {
    /// Private provider route id in reports; public UI lifecycle id in
    /// canonical facts.
    pub command_id: crate::ShellCommandId,
    pub stream: ShellStream,
    pub chunk: String,
    /// Target agent for this user-authored shell command. `None` means no
    /// explicit target; the harness uses its default conversation state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent_id: Option<AgentId>,
}

/// A user-initiated shell command completed (exited or was cancelled).
///
/// The extension echoes `command`, `session_id`, `include_in_context`, and the
/// resolved target from the originating `UiShellCommand`. The harness validates
/// those immutable fields and the event source against its pending-command
/// route after the extension's [`Event::ShellCommandFinishedReported`] commits.
/// The provider also echoes the opaque harness route id it received; the
/// harness maps that id back to the UI lifecycle id before publishing the
/// immutable, must-pass [`Event::ShellCommandFinished`] fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellCommandFinished {
    /// Private provider route id in reports; public UI lifecycle id in
    /// canonical facts.
    pub command_id: crate::ShellCommandId,
    pub session_id: SessionId,
    pub command: String,
    pub include_in_context: bool,
    /// Target agent for this user-authored shell command. `None` means no
    /// explicit target; the harness uses its default conversation state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent_id: Option<AgentId>,
    /// Native user-shell stdout followed by an optional `[stderr]` section.
    /// When the 10 KiB visible bound truncates it, the final suffix is a
    /// `[tau-output-metadata]` block containing `truncated`, complete
    /// `total_lines`/`total_bytes`, a warning, and exactly one full, partial,
    /// or unavailable saved-output field set.
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub cancelled: bool,
}

// ---------------------------------------------------------------------------
// Term events — terminal-output side effects directed at the UI
// ---------------------------------------------------------------------------

/// Ask the UI to write an iTerm2 OSC 1337 `SetUserVar` escape sequence
/// to its terminal. The terminal emulator interprets it as setting
/// the named user variable (visible from terminal multiplexers and
/// scripts watching status); the visible terminal output does not
/// change. Useful for surfacing notifications, build status, or any
/// other state to terminal-side tooling.
///
/// Producers should validate `name` before emitting the event. The terminal UI
/// validates again and skips invalid names instead of writing malformed escape
/// sequences. The UI base64-encodes `value` and emits the appropriate escape
/// sequence form (plain, or `\x1bPtmux;...\x1b\\` wrapped when running inside
/// `tmux`). Components without access to a terminal — or running through a UI
/// that ignores the event — are no-ops.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Osc1337SetUserVar {
    /// User-variable name. Must be printable ASCII without `=` or
    /// control characters. Terminal UIs validate and skip invalid names as
    /// defense in depth.
    pub name: String,
    /// Value to associate with `name`. Arbitrary bytes are fine — the
    /// UI base64-encodes before transmission.
    pub value: String,
}

/// Ask the UI to write an ASCII BEL (`\x07`) to its terminal. Terminal
/// behavior depends on the user's terminal settings: it may play a sound,
/// flash, raise a desktop notification, or do nothing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TermBell {}

// ---------------------------------------------------------------------------
// Agent transcript/runtime events
// ---------------------------------------------------------------------------

/// A prompt was accepted into a concrete agent transcript.
///
/// This is the durable agent-owned counterpart to the transient
/// [`UiPromptSubmitted`] request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentPromptSubmitted {
    /// Agent transcript receiving the prompt.
    pub agent_id: AgentId,
    /// Harness-owned immutable activation marker. True creates
    /// checkpoint-governed inference work; false is passive or legacy context
    /// and cannot independently wake inference during replay.
    #[serde(default, skip_serializing_if = "is_false")]
    pub inference_activation: bool,
    /// Prompt text.
    pub text: String,
    /// Whether this prompt text is user-authored or internal control text.
    #[serde(default)]
    pub message_class: PromptMessageClass,
    /// Harness-owned subtype for internal prompts with visible presentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal_kind: Option<InternalPromptKind>,
    /// Who initiated the prompt.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Harness-stamped provenance of this accepted prompt.
    #[serde(default)]
    pub submission_source: PromptSubmissionSource,
    /// Human-friendly display name known when the prompt was submitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Echo of [`UiPromptSubmitted::ctx_id`] when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_id: Option<String>,
}

/// The harness queued a user prompt because the agent is busy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentPromptQueued {
    /// Agent whose queue owns the prompt.
    pub agent_id: AgentId,
    /// Queued prompt text.
    pub text: String,
    /// Whether this prompt text is user-authored or hidden internal control
    /// text.
    #[serde(default)]
    pub message_class: PromptMessageClass,
}

/// The harness recalled a previously queued user prompt for editing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentPromptRecalled {
    /// Agent whose queue the prompt was recalled from.
    pub agent_id: AgentId,
    /// Recalled prompt text.
    pub text: String,
}

/// A durable provider-visible manual or automatic compaction request was
/// inserted into an agent transcript.
///
/// This records the user-facing fact that compaction was requested. It is not
/// a lifecycle/status event: providers translate the folded
/// [`ContextItem::CompactionTrigger`] into inline context management, while
/// standalone-capable providers receive an explicit compact operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentCompactionTriggered {
    /// Agent transcript receiving the compaction trigger.
    pub agent_id: AgentId,
    /// Who requested the trigger.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Whether the harness scheduled this boundary before an already-published
    /// user turn and must resume inference after successful compaction.
    #[serde(default, skip_serializing_if = "is_false")]
    pub resume_inference: bool,
}

/// Why a standalone compaction transaction ended without an accepted window.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StandaloneCompactionFailureReason {
    /// The provider returned a terminal error.
    ProviderError,
    /// The provider returned a malformed replacement window.
    InvalidWindow,
    /// No matching provider route accepted the request.
    RouteFailed,
    /// The operation was explicitly cancelled.
    Cancelled,
    /// The selected transcript branch changed while compaction was running.
    StaleBranch,
    /// Replay found a started transaction with no durable outcome.
    Interrupted,
}

/// Durable start record for one standalone-compaction transaction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentStandaloneCompactionStarted {
    /// Agent whose transcript is being compacted.
    pub agent_id: AgentId,
    /// Unique transaction correlation identifier.
    pub transaction_id: CompactionTransactionId,
    /// Provider prompt id pre-minted before the durable start commits.
    pub compact_prompt_id: AgentPromptId,
    /// Immutable last node included in the compact request.
    pub cut: AgentHead,
    /// Last already-committed activation owed an inference, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_through: Option<AgentHead>,
    /// Model semantics captured when the transaction started.
    pub model: ModelId,
    /// Provider operation captured for this transaction.
    pub operation: PromptOperation,
    /// Originator semantics captured when the transaction started.
    pub originator: PromptOriginator,
    /// Earlier failed transaction explicitly replaced by this attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<CompactionTransactionId>,
    /// Cause that created this transaction and, for reactive recovery, the
    /// rejected inference prompt it uniquely claims.
    #[serde(default)]
    pub trigger: StandaloneCompactionTrigger,
}

/// Model-callable tool that initiated a durable manual compaction request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualCompactionTool {
    /// The caller requested compaction of its own complete tool round.
    Compact,
    /// The caller requested compaction of another loaded agent.
    AgentCompact,
}

/// Harness-owned durable acceptance fact for a model-requested compaction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentManualCompactionRequested {
    /// Stable identifier for this accepted request.
    pub request_id: CompactionRequestId,
    /// Public id of the tool-call owner.
    pub caller_agent_id: AgentId,
    /// Public id of the agent whose transcript will be compacted.
    pub target_agent_id: AgentId,
    /// Prompt snapshot that authorized the tool call.
    pub initiating_agent_prompt_id: AgentPromptId,
    /// Original tool call completed when compaction terminates.
    pub initiating_tool_call_id: ToolCallId,
    /// Exact separately authorized tool used by the caller.
    pub initiating_tool_name: ManualCompactionTool,
    /// Prompt-visible name retained for terminal background correlation.
    pub visible_tool_name: ToolName,
    /// Target branch head observed at acceptance.
    pub requested_target_head: AgentHead,
    /// Target-owned materialized prompt generation observed at acceptance.
    pub target_generation: u64,
    /// Provider-qualified model observed at acceptance.
    pub model: ModelId,
    /// Whether successful compaction owes continuation inference.
    pub resume_inference: bool,
}

/// Safe categorical reason an accepted request could not start.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualCompactionRequestFailureReason {
    /// The original background tool call was cancelled.
    Cancelled,
    /// The target left the loaded session before dispatch.
    TargetUnloaded,
    /// The target's selected model changed after acceptance.
    ModelChanged,
    /// The selected model no longer supports standalone compaction.
    Unsupported,
    /// The captured provider route disappeared before dispatch.
    RouteFailed,
    /// The captured branch is no longer a safe compaction boundary.
    StaleBranch,
}

/// Durable terminal failure before a manual request became a transaction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentManualCompactionRequestFailed {
    /// Request that reached this one terminal pre-start outcome.
    pub request_id: CompactionRequestId,
    /// Target agent owning the durable request.
    pub target_agent_id: AgentId,
    /// Safe bounded failure category.
    pub reason: ManualCompactionRequestFailureReason,
}

/// Durable cause of a standalone compaction transaction.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum StandaloneCompactionTrigger {
    /// Explicit manual compaction, and the legacy default when the trigger
    /// field is absent.
    #[default]
    Manual,
    /// Automatic compaction after the local context projection reached the
    /// configured role/model threshold.
    AutomaticThreshold,
    /// Explicit compaction requested by a model-callable harness tool.
    ManualAgentTool {
        /// Durable request uniquely claimed by this transaction.
        request_id: CompactionRequestId,
        /// Public id of the agent that owned the tool call.
        caller_agent_id: AgentId,
        /// Tool call completed when this transaction reaches a terminal state.
        initiating_tool_call_id: ToolCallId,
    },
    /// Automatic recovery of one canonically rejected ordinary inference.
    ReactiveContextOverflow {
        /// Failed inference prompt uniquely claimed by this transaction.
        failed_agent_prompt_id: AgentPromptId,
    },
}

/// Durable terminal failure of one standalone-compaction transaction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentStandaloneCompactionFailed {
    /// Agent that owned the transaction.
    pub agent_id: AgentId,
    /// Transaction that failed.
    pub transaction_id: CompactionTransactionId,
    /// Immutable compact request cut.
    pub cut: AgentHead,
    /// Safe categorical failure reason.
    pub reason: StandaloneCompactionFailureReason,
    /// Last activation still owed an inference, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_through: Option<AgentHead>,
}

/// Durable checkpoint committed before an inference leaves the harness.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentInferenceDispatchStarted {
    /// Agent whose activation snapshot is acknowledged.
    pub agent_id: AgentId,
    /// Compaction transaction that enabled this inference, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<CompactionTransactionId>,
    /// Provider prompt correlation identifier.
    pub agent_prompt_id: AgentPromptId,
    /// Immutable transcript head represented by the provider prompt.
    pub through: AgentHead,
    /// Provider-qualified model captured before dispatch. Absent on legacy
    /// checkpoints, which are deliberately recovery-ineligible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Provider operation captured before dispatch. Absent on legacy records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<PromptOperation>,
    /// Immutable transcript head immediately before the owed activation.
    /// Absent on legacy records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_cut: Option<AgentHead>,
}

/// The harness accepted one standalone provider compaction result.
///
/// This is the sole transcript boundary for replacement-window compaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentCompacted {
    /// Agent transcript receiving the replacement window.
    pub agent_id: AgentId,
    /// New-format standalone transaction correlation; absent with all five
    /// other correlation fields on legacy records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<CompactionTransactionId>,
    /// Immutable compact-input cut; absent on legacy hard boundaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut: Option<AgentHead>,
    /// Last suffix node immediately before this boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix_end: Option<AgentHead>,
    /// Compact provider prompt correlation; absent on legacy boundaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_prompt_id: Option<AgentPromptId>,
    /// Captured provider-qualified model; absent on legacy boundaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Captured provider operation; absent on legacy boundaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<PromptOperation>,
    /// Provider-validated ordered context that replaces all older model-visible
    /// history.
    pub replacement_window: Vec<ContextItem>,
}

/// A previously queued user prompt that the harness folded into the
/// in-flight turn as a steering message — appended to the next
/// `AgentPromptCreated` for this agent alongside tool results, rather
/// than waiting for the agent to return to `Idle` and kicking off a fresh turn.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentPromptSteered {
    /// Agent whose in-flight turn received the prompt.
    pub agent_id: AgentId,
    /// Harness-owned immutable activation marker. True creates
    /// checkpoint-governed inference work; false is passive or legacy context
    /// and cannot independently wake inference during replay.
    #[serde(default, skip_serializing_if = "is_false")]
    pub inference_activation: bool,
    /// Harness-stamped provenance of this accepted prompt.
    pub submission_source: PromptSubmissionSource,
    /// Prompt text appended to the in-flight turn.
    pub text: String,
    /// Whether this prompt text is user-authored or internal control text.
    #[serde(default)]
    pub message_class: PromptMessageClass,
    /// Harness-owned subtype for internal prompts with visible presentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal_kind: Option<InternalPromptKind>,
    /// Echo of the original queued prompt correlation id, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_id: Option<String>,
}

/// A synthetic user message injected into an agent transcript by the harness
/// (not authored by the human user directly). Sources include shell command
/// output and eager AGENTS.md context preambles.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentUserMessageInjected {
    /// Agent transcript receiving the injected message.
    pub agent_id: AgentId,
    /// Harness-owned immutable activation marker. True creates
    /// checkpoint-governed inference work; false is passive or legacy context
    /// and cannot independently wake inference during replay.
    #[serde(default, skip_serializing_if = "is_false")]
    pub inference_activation: bool,
    pub text: String,
    /// Whether this prompt text is user-authored or hidden internal control
    /// text.
    #[serde(default)]
    pub message_class: PromptMessageClass,
}

// ---------------------------------------------------------------------------
// Session lifecycle/membership events
// ---------------------------------------------------------------------------

/// Why a `SessionStarted` was published. Lets extensions distinguish
/// "first session of this harness's life" from "user switched to a new
/// session" (e.g. so they can clear caches).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStartReason {
    /// The harness eagerly initialized this session at startup.
    Initial,
    /// The user requested a fresh session via `:session new`.
    New,
    /// The user resumed an existing session by id.
    Resume,
}

/// The harness created or switched to a session. Extensions that subscribe
/// react by performing per-session setup (e.g. discovering AGENTS.md) and
/// signal completion with `ExtensionSessionContextReady`. Per-agent providers
/// instead react to `SessionAgentLoaded` and use `ExtensionContextReady`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionStarted {
    pub session_id: SessionId,
    #[serde(default = "default_session_start_reason")]
    pub reason: SessionStartReason,
}

fn default_session_start_reason() -> SessionStartReason {
    SessionStartReason::Initial
}

/// The harness is leaving the current session. Fired before
/// `SessionStarted` for the next one when the user switches sessions.
/// Extensions that hold per-session state subscribe to flush or drop it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionShutdown {
    pub session_id: SessionId,
}

/// Who initiated the prompt — the human user via the UI, or a side query from
/// an extension or harness-owned tool via [`StartAgentRequest`].
///
/// The provider's only obligation is to copy the originator from the
/// incoming [`AgentPromptCreated`] onto its outgoing
/// [`ProviderResponseFinished`]. The harness reads it on the way back
/// to decide whether the response is a normal turn (route to UI,
/// keep `default_conversation` advancing) or a side-query reply
/// (route an [`StartAgentResult`] to the requester and tear the conversation
/// down).
///
/// UIs filter on `originator.is_user()` to ignore side conversations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PromptOriginator {
    /// Default — interactive prompt submitted through the UI.
    #[default]
    User,
    /// Side prompt requested by an extension or harness-owned tool via
    /// [`StartAgentRequest`]. The harness uses `__harness__` here for its own
    /// tools.
    Extension {
        name: ExtensionName,
        query_id: String,
    },
}

impl PromptOriginator {
    /// True iff this prompt is the user's interactive turn.
    #[must_use]
    pub const fn is_user(&self) -> bool {
        matches!(self, Self::User)
    }
}

/// Reference to tool definitions carried by an earlier prompt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PromptToolsRef {
    /// Prompt whose materialized tools contain the full tool list.
    pub base_agent_prompt_id: AgentPromptId,
}

/// Transient provider work request materialized by the harness.
///
/// Carries the assembled conversation context for the provider's normal
/// response path. Semantic journals must never persist this payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentPromptCreated {
    pub agent_prompt_id: AgentPromptId,
    /// Agent transcript this prompt belongs to.
    pub agent_id: AgentId,
    /// Session where this request was first made.
    pub session_id: SessionId,
    /// System prompt sent alongside the item timeline.
    pub system_prompt: String,
    /// Fully materialized prompt context for this turn.
    pub context: PromptContext,
    /// Tool definitions, or empty when [`Self::tools_ref`] is set.
    pub tools: Vec<ToolDefinition>,
    /// Optional reference to full tool definitions from an earlier prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_ref: Option<PromptToolsRef>,
    /// Currently selected model as `"provider/model_id"`.
    pub model: ModelId,
    /// Per-prompt model knobs (reasoning effort, output verbosity,
    /// thinking-summary mode). The harness stamps in its current
    /// selection on every prompt; backends pass each field through
    /// only when the provider advertises support for it.
    #[serde(default)]
    pub model_params: ModelParams,
    /// Whether tool calls are allowed on this turn. Defaults to
    /// `Auto`; the harness flips to `None` for non-tool extension
    /// side queries (e.g. idle-summary) so they cannot trigger
    /// destructive tools. Backends emit this as `tool_choice: "none"`
    /// on the upstream request body.
    #[serde(default, skip_serializing_if = "ToolChoice::is_default")]
    pub tool_choice: ToolChoice,
    /// Who asked for this prompt. Defaults to [`PromptOriginator::User`]
    /// for backward compatibility with old persisted events.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Legacy cache-sharing hint kept for compatibility with persisted events
    /// and older provider implementations. First-party ChatGPT/Codex cache
    /// routing is now stable per target agent and ignores this flag; prompt
    /// originator/provenance must not split provider cache buckets.
    #[serde(default, skip_serializing_if = "is_false")]
    pub share_user_cache_key: bool,
    /// Echo of [`UiPromptSubmitted::ctx_id`] when this prompt was
    /// initiated by a UI submission. Tool-result follow-up
    /// `AgentPromptCreated` events for the same chain do not
    /// inherit it — only the first one does — so a correlator should
    /// capture the resulting [`Self::agent_prompt_id`] and track
    /// the rest of the chain by spid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_id: Option<String>,
    /// Server-side context-management compaction request metadata for providers
    /// that support compaction. When present without a threshold, the provider
    /// should opt in to its server default behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<PromptCompactionContext>,
    /// Provider operation requested for this prompt.
    #[serde(default, skip_serializing_if = "PromptOperation::is_inference")]
    pub operation: PromptOperation,
}

/// Provider operation represented by an [`AgentPromptCreated`] work request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptOperation {
    /// Produce a normal assistant response.
    #[default]
    Inference,
    /// Replace the model-visible transcript through a unary compact endpoint.
    StandaloneCompaction,
}

impl PromptOperation {
    /// Returns whether this is the default inference operation.
    #[must_use]
    pub const fn is_inference(&self) -> bool {
        matches!(self, Self::Inference)
    }
}

/// How one provider-declared wait call blocks.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolWaitMode {
    /// Wait for one exact declared call.
    Exact {
        /// Exact target call.
        target: ToolCallRef,
    },
    /// Exact target syntax was valid but no declared target call resolved.
    ExactUnresolved,
    /// Wait for the next retained background completion.
    NextBackground,
    /// Wait for activating input up to the effective runtime timeout.
    ActivatingInput {
        /// Clamped timeout used by the runtime.
        effective_timeout_minutes: u16,
    },
    /// Wait arguments were malformed and could not produce a runtime mode.
    InvalidArguments,
}

/// Content-free observation that the harness dispatched a declared call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentToolDispatchObserved {
    /// Dispatched call.
    pub call: ToolCallRef,
}

/// Content-free observation that the harness decided a declared call continues
/// in the background.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentToolBackgroundedObserved {
    /// Call moved to background execution.
    pub call: ToolCallRef,
}

/// Content-free observation that the runtime recognized a successfully parsed
/// wait invocation before resolving it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentToolWaitObserved {
    /// Declared wait call.
    pub wait_call: ToolCallRef,
    /// Parsed wait mode.
    pub mode: ToolWaitMode,
}

/// Content-free observation that the runtime installed a waiter.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentToolWaitRegistered {
    /// Observation of the parsed wait invocation.
    pub wait_observation: ObservationId,
    /// Declared wait call.
    pub wait_call: ToolCallRef,
    /// Installed wait mode.
    pub mode: ToolWaitMode,
}

/// Classification of one queued activating input.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationKind {
    /// Visible user input.
    VisibleUser,
    /// Inter-agent message.
    AgentMessage,
    /// Canonical external message.
    ExternalMessage,
    /// Harness-internal prompt.
    InternalPrompt,
    /// Timer wakeup.
    Timer,
    /// Watched-agent notification.
    WatchNotification,
    /// Loop guard wakeup.
    LoopGuard,
    /// Background completion notice.
    BackgroundCompletion,
    /// Other activating input.
    Other,
}

/// Content-free observation that activating input entered an agent queue.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentActivationQueued {
    /// Activating input class.
    pub kind: ActivationKind,
    /// Durable source observation, when one already exists.
    pub source_observation: Option<ObservationId>,
    /// Source call, when the activation came from tool work.
    pub source_call: Option<ToolCallRef>,
}

/// Phase of a source completion delivered by a wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSourcePhase {
    /// Source completed before background transition.
    Foreground,
    /// Source completed after background transition.
    Background,
}

/// Envelope applied by a wait around source-owned output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputEnvelope {
    /// Wait delivered source output unchanged.
    Identity,
    /// Wait added the original-tool-call-id header.
    OriginalToolCallIdHeader,
}

/// Structured reason that a wait invocation was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitRejectionReason {
    /// Arguments were malformed.
    InvalidArguments,
    /// Exact target was unknown.
    UnknownTarget,
    /// An exact wait was already active.
    DuplicateExactWait,
    /// A next-background wait was already active.
    DuplicateAnyWait,
    /// An activating-input wait was already active.
    DuplicateInputWait,
    /// Target had already returned in the foreground.
    TargetReturnedForegroundBeforeWait,
    /// Retained completion was already consumed.
    ResultAlreadyConsumed,
    /// No background candidate exists.
    NoBackgroundCandidate,
}

/// Explicit runtime outcome of one wait invocation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ToolWaitOutcome {
    /// A source completion was delivered.
    CompletionDelivered {
        /// Source call.
        source_call: ToolCallRef,
        /// Canonical source terminal observation.
        source_terminal: ObservationId,
        /// Source terminal phase.
        source_phase: ToolSourcePhase,
        /// Wait envelope applied around source-owned output.
        envelope: ToolOutputEnvelope,
    },
    /// Unrelated activating input interrupted the wait.
    InterruptedByActivation {
        /// Selected activation observation.
        activation: ObservationId,
    },
    /// Activating input satisfied an input wait.
    InputAvailable {
        /// Selected activation observation.
        activation: ObservationId,
    },
    /// Runtime deadline elapsed.
    TimedOut,
    /// Runtime rejected the invocation.
    Rejected {
        /// Structured rejection reason.
        reason: WaitRejectionReason,
    },
    /// Wait call was cancelled.
    Cancelled,
    /// Agent lifecycle ended before settlement.
    LifecycleAborted,
}

/// Content-free observation linking a wait terminal to runtime settlement.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentToolWaitSettled {
    /// Observation of the parsed wait invocation.
    pub wait_observation: ObservationId,
    /// Declared wait call.
    pub wait_call: ToolCallRef,
    /// Registration observation, absent for immediate settlement.
    pub registration: Option<ObservationId>,
    /// Canonical wait terminal observation.
    pub wait_terminal: ObservationId,
    /// Explicit settlement outcome.
    pub outcome: ToolWaitOutcome,
}

/// Content-free observation that an accepted cancel call targeted another call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentToolCancellationRequested {
    /// Accepted cancel call.
    pub cancel_call: ToolCallRef,
    /// Target call.
    pub target_call: ToolCallRef,
}

/// Explicit cause assigned to a canonical terminal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ToolTerminalCause {
    /// Normal completion.
    Completed,
    /// Tool-reported error.
    ToolError,
    /// Accepted cancellation caused the terminal.
    Cancellation {
        /// Cancellation-request observation.
        request: ObservationId,
    },
    /// Provider disconnected.
    ProviderDisconnected,
    /// Agent lifecycle teardown.
    LifecycleTeardown,
    /// Cold-restart repair.
    RestartRepair,
    /// Cause is unavailable.
    Unknown,
}

/// Content-free classification of one canonical call terminal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentToolTerminalClassified {
    /// Terminal call.
    pub call: ToolCallRef,
    /// Canonical terminal observation.
    pub terminal: ObservationId,
    /// Explicit terminal cause.
    pub cause: ToolTerminalCause,
}

/// Durable, content-free fact that one provider prompt was materialized.
///
/// This mirrors the routing and provenance metadata from
/// [`AgentPromptCreated`] without carrying the materialized provider prompt
/// (`system_prompt`, context, or tool definitions). Consumers that only need to
/// track in-flight prompt state should subscribe to this event instead of
/// `agent.prompt_created`.
///
/// The harness commits this fact after the durable dispatch owner and before
/// the matching transient [`AgentPromptCreated`] provider work request.
/// Historical subscriber catch-up excludes it even though agent-journal replay
/// folds it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentPromptStarted {
    /// Prompt that was materialized; this does not assert provider receipt.
    pub agent_prompt_id: AgentPromptId,
    /// Agent transcript this prompt belongs to.
    pub agent_id: AgentId,
    /// Session where this request was first made.
    pub session_id: SessionId,
    /// Currently selected model as `"provider/model_id"`.
    pub model: ModelId,
    /// Captured model dispatch parameters, authoritative for historical
    /// accounting. `None` identifies a pre-accounting legacy fact.
    #[serde(default)]
    pub model_params: Option<ModelParams>,
    /// Owning ordinary outer turn; absent for standalone work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outer_turn_id: Option<AgentOuterTurnId>,
    /// Provider operation materialized for this dispatch.
    pub operation: PromptOperation,
    /// Who asked for this prompt.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Echo of [`UiPromptSubmitted::ctx_id`] when this prompt was initiated by
    /// a UI submission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_id: Option<String>,
}

impl From<&AgentPromptCreated> for AgentPromptStarted {
    fn from(prompt: &AgentPromptCreated) -> Self {
        Self {
            agent_prompt_id: prompt.agent_prompt_id.clone(),
            agent_id: prompt.agent_id.clone(),
            session_id: prompt.session_id.clone(),
            model: prompt.model.clone(),
            model_params: Some(prompt.model_params),
            outer_turn_id: None,
            operation: prompt.operation,
            originator: prompt.originator.clone(),
            ctx_id: prompt.ctx_id.clone(),
        }
    }
}

/// Request metadata for provider/server-side context compaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PromptCompactionContext {
    /// Token threshold at which automatic server-side compaction should run.
    /// `None` means use the provider/server default behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_threshold: Option<u64>,
}

/// Why a prompt ended without a provider response being accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPromptTerminationReason {
    /// A later prompt superseded this response before the harness accepted it.
    Stale,
    /// The harness cancelled or preempted the prompt.
    Canceled,
}

/// The harness ended a prompt without publishing `provider.response_finished`.
///
/// This is a transient lifecycle fact for UIs and other observers that track
/// in-flight prompts. It does not add assistant content to the agent
/// transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentPromptTerminated {
    /// Agent whose prompt is no longer in flight.
    pub agent_id: AgentId,
    /// Prompt that is no longer in flight.
    pub agent_prompt_id: AgentPromptId,
    /// Why no provider response will be published for this prompt.
    pub reason: AgentPromptTerminationReason,
    /// Who asked for this prompt.
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// Best-effort provider-side prompt-cache prewarm request.
///
/// Carries the same stable prefix fields as the first real
/// [`AgentPromptCreated`] but intentionally has no
/// [`AgentPromptId`], no user task prompt, and no
/// `previous_response_id`. Providers that support a non-generating
/// upstream call may send it; all others no-op.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentPromptPrewarmRequested {
    /// Agent whose prompt prefix should be warmed.
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub system_prompt: String,
    pub context: PromptContext,
    pub tools: Vec<ToolDefinition>,
    /// Currently selected model as `"provider/model_id"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Per-prompt model knobs, matching the first real prompt.
    #[serde(default)]
    pub model_params: ModelParams,
    /// Whether tool calls are allowed on the warmed prefix.
    #[serde(default, skip_serializing_if = "ToolChoice::is_default")]
    pub tool_choice: ToolChoice,
    /// Prompt provenance mirrored from the prompt being warmed. First-party
    /// ChatGPT/Codex cache routing is stable per target agent and ignores this
    /// field for cache-bucket selection.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Legacy cache-sharing hint mirrored from the first real prompt. Kept for
    /// compatibility; first-party ChatGPT/Codex ignores it for cache routing.
    #[serde(default, skip_serializing_if = "is_false")]
    pub share_user_cache_key: bool,
}

// ---------------------------------------------------------------------------
// Provider execution payloads — reports and canonical harness facts
// ---------------------------------------------------------------------------

/// The provider accepted a prompt and began processing it.
///
/// Provider extensions submit this payload as
/// `provider.prompt_submitted_reported`; the harness publishes the validated
/// canonical `provider.prompt_submitted` fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderPromptSubmitted {
    /// Prompt id the provider accepted.
    pub agent_prompt_id: AgentPromptId,
    /// Echo of [`AgentPromptCreated::originator`]. UIs and other
    /// extensions filter on `originator.is_user()` so provider work for a side
    /// conversation doesn't trigger user-facing
    /// effects like clearing an idle deadline.
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// The provider has newly appended displayable response output for a prompt.
///
/// Each update is a transient append-delta event. Providers send only newly
/// appended assistant/reasoning text in `deltas`; the complete durable response
/// remains [`ProviderResponseFinished::output_items`]. Some updates are
/// status- or compaction-only and therefore have empty `deltas`.
/// Provider extensions submit this payload as
/// `provider.response_updated_reported`; the harness publishes the validated
/// canonical `provider.response_updated` fact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderResponseUpdated {
    /// Prompt id whose response changed.
    pub agent_prompt_id: AgentPromptId,
    /// Agent transcript this in-flight response belongs to.
    pub agent_id: AgentId,
    /// Newly appended displayable assistant/reasoning text chunks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deltas: Vec<ProviderResponseTextDelta>,
    /// Small provider-side compaction lifecycle/status snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<ProviderResponseCompactionUpdate>,
    /// Provider-authored transient status text, such as retry diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ProviderResponseStatusUpdate>,
    /// Public content-free response throughput sample for this prompt.
    ///
    /// Providers own the response byte counter because they observe upstream
    /// response bytes at the backend transport boundary. The harness validates
    /// prompt ownership and broadcasts this sample unchanged; UI clients render
    /// it directly from `provider.response_updated`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_stats: Option<ProviderResponseStats>,
    /// Echo of [`AgentPromptCreated::originator`]. UIs filter on
    /// `originator.is_user()` so the streaming text from a side
    /// conversation doesn't paint into the user's chat window.
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// One prompt-local public throughput update, whose provider ownership is
/// specified by `SPEC-provider-response-streaming`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderResponseStats {
    /// Latest cumulative provider-response statistics sample for this prompt.
    pub current: ProviderResponseStatsSample,
    /// Previously emitted provider-response sample for this prompt.
    pub previous: ProviderResponseStatsSample,
}

/// One provider-owned, prompt-local response throughput sample.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderResponseStatsSample {
    /// Monotonic content-free byte count of backend response bytes received for
    /// the current provider prompt before semantic parsing.
    pub response_bytes_received: u64,
    /// Monotonic elapsed time since backend request dispatch for this provider
    /// prompt, in microseconds.
    pub elapsed_micros: u64,
}

/// Newly appended displayable text in a provider response update.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderResponseTextDelta {
    /// Assistant-authored message text appended to one provider output item.
    Message {
        /// Provider output index when available. Backends without native
        /// indices use their local live-output item order.
        output_index: u32,
        /// Newly appended assistant text.
        text: String,
        /// Optional assistant-message phase metadata.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<MessagePhase>,
    },
    /// Displayable reasoning text appended to one provider output item.
    ReasoningText {
        /// Provider output index when available.
        output_index: u32,
        /// Whether this is summary or full reasoning text.
        kind: ReasoningTextKind,
        /// Newly appended reasoning text.
        text: String,
    },
}

/// Provider-side compaction lifecycle/status carried by a transient update.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderResponseCompactionUpdate {
    /// Current compaction status.
    pub status: ProviderResponseCompactionStatus,
    /// Input-token count before compaction, filled by the harness when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_input_tokens: Option<u64>,
    /// Prompt/input-token count of the compacted provider item, when available.
    /// Live updates may omit this and rely on the final response metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_input_tokens: Option<u64>,
}

/// Status of provider-side compaction in a transient response update.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResponseCompactionStatus {
    /// The provider has started compaction.
    Started,
    /// The provider finished compaction successfully.
    Completed,
}

/// Provider-authored transient status text for an in-flight response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderResponseStatusUpdate {
    /// Human-readable status text to display while the provider continues work.
    pub text: String,
    /// Whether prior live assistant/reasoning deltas for this prompt should be
    /// hidden because the provider is retrying or otherwise replacing work.
    #[serde(default, skip_serializing_if = "is_false")]
    pub clear_response: bool,
    /// Structured retry facts. Raw provider diagnostics must never be placed
    /// here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<ProviderRetryStatus>,
}

/// The provider finished processing a prompt.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStopReason {
    /// The model completed the turn without requesting any tool work.
    #[default]
    EndTurn,
    /// The model stopped because it emitted tool calls that Tau should run.
    ToolCalls,
    /// The model stopped because the provider output-token cap was reached.
    Length,
    /// The turn ended with a provider/runtime error.
    Error,
    /// The provider aborted a tight exact streaming repetition.
    RepetitionDetected,
}

impl ProviderStopReason {
    #[must_use]
    pub const fn requests_tool_calls(self) -> bool {
        matches!(self, Self::ToolCalls)
    }
}

/// Machine-readable category for a terminal provider request failure.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailureKind {
    /// The provider rejected the unchanged request because its context was too
    /// large.
    ContextWindowExceeded,
    /// The provider deterministically rejected the unchanged request for
    /// another reason.
    RequestRejected,
    /// A terminal provider failure without a more specific safe classification.
    Unknown,
}

/// Content-free evidence captured when a provider rejects a prompt for context
/// length. This is durable diagnostic telemetry, not an automatic calibration
/// command: advertised limits and policy thresholds remain configuration-owned.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextLimitTelemetry {
    /// Provider-qualified model selected for the rejected prompt.
    pub model: ModelId,
    /// Operation rejected by the provider.
    pub operation: PromptOperation,
    /// Conservative harness estimate immediately before dispatch, when a
    /// same-model usage baseline was available. Transcript growth counts
    /// byte-free JSON structure plus canonical image bytes and rounded-up
    /// 32-by-32 patches. This estimate is not provider-token evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projected_input_tokens: Option<u64>,
    /// Serialized transcript growth since the usage baseline. This byte count
    /// is intentionally not labeled or interpreted as provider tokens. It is
    /// absent when any suffix entry is not JSON-representable or the exact
    /// total cannot be represented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_delta_bytes: Option<u64>,
    /// Provider-published context window observed for this exact model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertised_context_window: Option<u64>,
    /// Provider-reported input usage attached to the rejection, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_input_tokens: Option<u64>,
    /// Conservative reserve included in the harness projection.
    pub projection_reserve_tokens: u64,
    /// Explicit role/model compaction threshold active at dispatch, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_threshold: Option<u64>,
    /// Explicit compaction policy active for the role at dispatch.
    pub compaction_policy: ContextLimitCompactionPolicy,
    /// Whether all reactive-recovery gates were satisfied.
    pub recovery_eligible: bool,
    /// Harness recovery action chosen after validating operation, capability,
    /// policy, branch, and output-safety gates.
    pub action: ContextLimitAction,
    /// Bounded interpretation of the evidence.
    pub observation: ContextLimitObservation,
}

/// Bounded harness action associated with context-limit evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextLimitAction {
    /// No automatic recovery was authorized.
    Terminal,
    /// Exactly one reactive standalone compaction was durably planned.
    ReactiveCompactionPlanned,
}

/// Sanitized role compaction policy active at dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextLimitCompactionPolicy {
    /// Provider/model default threshold policy.
    ProviderDefault,
    /// Explicit configured threshold.
    Threshold,
    /// Automatic compaction disabled.
    Disabled,
}

/// Sanitized interpretation of one context-limit rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextLimitObservation {
    /// Nonzero provider input usage was below the advertised window and any
    /// available conservative projection agreed, indicating hidden overhead or
    /// provider/model limit drift.
    RejectedBelowAdvertisedLimit,
    /// Nonzero provider input usage reached or exceeded the advertised limit
    /// and any available conservative projection agreed.
    RejectedAtOrAboveAdvertisedLimit,
    /// Provider usage or model-limit evidence was absent, zero, or
    /// contradictory. A transcript projection alone always has this category.
    InsufficientEvidence,
}

/// Harness-authored durable disposition of a terminal provider response.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextRecoveryDisposition {
    /// No automatic context recovery was authorized.
    #[default]
    None,
    /// The matching rejected inference must be claimed by one standalone
    /// compaction transaction.
    ReactiveCompactionPlanned,
}

impl ContextRecoveryDisposition {
    fn is_none(value: &Self) -> bool {
        matches!(value, Self::None)
    }
}

impl ProviderFailureKind {
    /// Stable wire-compatible label used in safe presentation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContextWindowExceeded => "context_window_exceeded",
            Self::RequestRejected => "request_rejected",
            Self::Unknown => "unknown",
        }
    }
}

/// Terminal provider-response payload shared by a Provider-authored
/// `provider.response_finished_reported` observation and the harness-canonical
/// `provider.response_finished` fact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderResponseFinished {
    /// Prompt id the provider finished.
    pub agent_prompt_id: AgentPromptId,
    /// Agent transcript this response belongs to.
    pub agent_id: AgentId,
    /// Final provider output, including assistant messages, reasoning,
    /// compaction payloads, and/or requested tool calls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_items: Vec<ContextItem>,
    /// Why the provider stopped this turn.
    pub stop_reason: ProviderStopReason,
    /// Human-readable provider/runtime error detail for clients to display.
    /// This is not assistant output and must not be replayed into future
    /// provider prompts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Typed terminal failure category, independent of display-oriented error
    /// prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<ProviderFailureKind>,
    /// Sanitized harness-authored context-limit diagnostic on canonical
    /// responses. Reports may carry an untrusted value, which the terminal
    /// pipeline discards and rederives before canonical publication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_limit_telemetry: Option<ContextLimitTelemetry>,
    /// Harness-authored recovery decision on canonical responses. Provider
    /// reports may carry an untrusted value, which the terminal pipeline
    /// discards and rederives before canonical publication.
    #[serde(default, skip_serializing_if = "ContextRecoveryDisposition::is_none")]
    pub recovery_disposition: ContextRecoveryDisposition,
    /// Echo of [`AgentPromptCreated::originator`]. The provider must
    /// copy this from the prompt; the harness routes the response
    /// based on it.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Provider-reported usage for this response, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ProviderTokenUsage>,
    /// Harness-captured effective price rates for this accepted response.
    /// Missing when accepted provider usage is unavailable; providers never
    /// author this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_api_cost_rates: Option<crate::EstimatedApiCostRates>,
    /// Harness-calculated cost from this response's local usage counters.
    /// Missing when accepted provider usage is unavailable. A calculated zero
    /// remains present as zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_api_cost_increment: Option<crate::EstimatedApiCost>,
    /// Input-token count of the conversation before provider-side compaction,
    /// if this finished response contains a durable compaction item and the
    /// harness knows the previous context size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_original_input_tokens: Option<u64>,
    /// Prompt/input-token count of the compacted provider item, if this
    /// finished response contains a durable compaction item and the harness can
    /// derive or estimate the replay size. This is UI context-size metadata,
    /// not a billing counter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_compacted_input_tokens: Option<u64>,
    /// Which LLM backend handled this turn. Recorded once per turn
    /// (instead of in a trace line) so offline inspection of the
    /// event log can correlate cache-miss / retry patterns with the
    /// backend that produced them — e.g. distinguishing OpenAI
    /// public-API behavior from the ChatGPT Codex Responses backend.
    /// `None` for turns that never reached a backend (e.g. an
    /// provider-side resolution failure or the in-process echo provider).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<ProviderBackend>,
    /// Provider-supplied response id for this turn, when the backend
    /// exposed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_response_id: Option<String>,
    /// Per-turn delta of the provider's Codex WS pool counters. `Some(_)`
    /// is used when a Responses backend can report WebSocket pool stats for
    /// the turn. `None` for Chat Completions, Responses configs that do not
    /// use WebSocket, and turns that fail before a WebSocket pool delta can be
    /// computed. Transport selection is recorded separately in
    /// `ProviderBackend::transport`; WebSocket-capable configs must not be
    /// silently or permanently flipped to HTTP/SSE after WebSocket failures.
    /// Lets offline analysis attribute a low `cached_tokens` to a chain-strip
    /// event (the Codex chain cache is connection-local; a fresh socket or a
    /// silent reconnect drops the in-request `previous_response_id`,
    /// collapsing `cached_tokens` to the static system+tools baseline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws_pool_delta: Option<WsPoolDelta>,
}

/// Per-turn delta of the provider's Codex WebSocket pool counters. The
/// counters are monotonic-since-process-start in the provider; the harness
/// records the *delta* incurred by a single turn so offline analysis can
/// attribute cache misses to WS-layer events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WsPoolDelta {
    /// Fresh sockets opened this turn. Counts every reason: cold
    /// pool, server-age purge, bearer rotation, silent-reconnect
    /// recovery.
    pub upgrades: u32,
    /// Cached sockets that died mid-turn and triggered the silent
    /// reopen-and-replay-without-chain-id recovery this turn.
    pub silent_reconnects: u32,
}

/// Cache diagnostic payload submitted as
/// `provider.cache_miss_diagnostic_reported` and republished canonically by the
/// harness after prompt-owner validation.
///
/// Diagnostic emitted when a prompt with a previous provider response reports
/// unexpectedly low provider cache reuse. Provider extensions derive it from
/// the original [`AgentPromptCreated`] plus final [`ProviderResponseFinished`]
/// token usage and the harness accepts it only from the provider that owns the
/// prompt. Offline analysis can use it to spot suspicious cache misses and then
/// inspect the dumped provider request JSON for exact wire details.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderCacheMissDiagnostic {
    /// Prompt id whose cache behavior looked unexpectedly low.
    pub agent_prompt_id: AgentPromptId,
    /// Currently selected model as `"provider/model_id"`.
    pub model: ModelId,
    /// Prompt originator copied from the finished provider response.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Tool-choice mode used by the request that produced this diagnostic.
    #[serde(default, skip_serializing_if = "ToolChoice::is_default")]
    pub tool_choice: ToolChoice,
    /// WebSocket-pool turn delta, when the backend can report one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws_pool_delta: Option<WsPoolDelta>,
    /// Input tokens reported by the current response.
    pub input_tokens: u64,
    /// Provider-cache-hit input tokens reported by the current response.
    pub cached_tokens: u64,
    /// Input tokens reported by the previous chained response.
    pub previous_input_tokens: u64,
    /// Estimated cacheable prefix tokens after correcting for request growth.
    pub cacheable_input_tokens: u64,
    /// Corrected cache-hit ratio for the cacheable prefix.
    pub corrected_cache_efficiency: f32,
}

/// Identifies the LLM backend that handled an
/// [`ProviderResponseFinished`].
///
/// Kind discriminates the provider API shape (Chat Completions vs.
/// Responses), and `base_url` pins down the specific endpoint —
/// `https://api.openai.com/v1` and `https://chatgpt.com/backend-api`
/// share the Responses kind but have very different cache /
/// rate-limit behavior, so the base URL is what an offline analysis
/// needs to tell them apart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderBackend {
    /// Provider API family used for the turn.
    pub kind: ProviderBackendKind,
    /// Base URL or origin of the upstream provider endpoint.
    pub base_url: String,
    /// Wire transport the turn was sent over. Defaults to
    /// `HttpSse` for backwards compatibility with sessions recorded
    /// before this field existed.
    #[serde(default)]
    pub transport: ProviderBackendTransport,
    /// The backend retried a rejected `previous_response_id` as a full replay.
    /// Surfaced here so the harness and offline tools can tell a successful
    /// response still paid the stale-chain recovery cost.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stale_chain_fallback: bool,
}

/// The provider API shape an [`ProviderBackend`] talks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderBackendKind {
    ChatCompletions,
    Responses,
}

/// Transport the provider used to deliver one turn. `HttpSse` covers
/// both the Chat Completions path and the HTTP+SSE Responses path
/// (kind discriminates which API); `Websocket` is the Codex
/// Responses persistent-WS path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderBackendTransport {
    /// One-shot HTTP request with Server-Sent Events streaming.
    /// Default — covers Chat Completions and Responses endpoints that do not
    /// use WebSocket.
    #[default]
    HttpSse,
    /// Persistent WebSocket. Only Codex Responses today.
    Websocket,
}

// ---------------------------------------------------------------------------
// Top-level event envelope
// ---------------------------------------------------------------------------

/// Top-level event envelope used on the wire.
///
/// When adding, renaming, or changing default durability for an event, update
/// `docs/events.md` in the same change so extension authors do not learn a
/// stale wire contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "payload")]
pub enum Event {
    // Tools
    #[serde(rename = "tool.registration_declared")]
    ToolRegistrationDeclared(ToolRegistrationDeclared),
    #[serde(rename = "tool.unregistration_declared")]
    ToolUnregistrationDeclared(ToolUnregistrationDeclared),
    #[serde(rename = "tool.register")]
    ToolRegister(ToolRegister),
    #[serde(rename = "tool.unregister")]
    ToolUnregister(ToolUnregister),
    #[serde(rename = "tool.request")]
    ToolRequest(ToolRequest),
    #[serde(rename = "tool.started")]
    ToolStarted(ToolStarted),
    #[serde(rename = "tool.rejected")]
    ToolRejected(ToolRejected),
    /// Peer-authored successful completion awaiting routed-call validation.
    #[serde(rename = "tool.result_reported")]
    ToolResultReported(ToolResult),
    /// Harness-authored canonical successful completion.
    #[serde(rename = "tool.result")]
    ToolResult(ToolResult),
    /// Peer-authored failed completion awaiting routed-call validation.
    #[serde(rename = "tool.error_reported")]
    ToolErrorReported(ToolError),
    /// Harness-authored canonical failed completion.
    #[serde(rename = "tool.error")]
    ToolError(ToolError),
    #[serde(rename = "tool.background_result")]
    ToolBackgroundResult(ToolBackgroundResult),
    #[serde(rename = "tool.background_error")]
    ToolBackgroundError(ToolBackgroundError),
    /// Peer-authored observation awaiting routed-call validation.
    #[serde(rename = "tool.progress_reported")]
    ToolProgressReported(ToolProgress),
    /// Harness-authored canonical progress fact.
    #[serde(rename = "tool.progress")]
    ToolProgress(ToolProgress),
    #[serde(rename = "tool.cancel_request")]
    ToolCancelRequest(ToolCancelRequest),
    /// Peer-authored cancellation awaiting routed-call validation.
    #[serde(rename = "tool.cancelled_reported")]
    ToolCancelledReported(ToolCancelled),
    /// Harness-authored canonical foreground cancellation.
    #[serde(rename = "tool.cancelled")]
    ToolCancelled(ToolCancelled),
    #[serde(rename = "tool.delegate_progress")]
    ToolDelegateProgress(DelegateProgress),

    // Extension-provided UI actions
    #[serde(rename = "action.schema_published")]
    ActionSchemaPublished(ActionSchemaPublished),
    #[serde(rename = "action.invoke")]
    ActionInvoke(ActionInvoke),
    #[serde(rename = "action.result")]
    ActionResult(ActionResult),
    #[serde(rename = "action.error")]
    ActionError(ActionError),

    // Harness-authored canonical external-message facts
    #[serde(rename = "message.delivered")]
    MessageDelivered(MessageDelivered),
    #[serde(rename = "message.edited")]
    MessageEdited(MessageEdited),
    #[serde(rename = "message.deleted")]
    MessageDeleted(MessageDeleted),
    #[serde(rename = "message.reaction_added")]
    MessageReactionAdded(MessageReactionAdded),
    #[serde(rename = "message.reaction_removed")]
    MessageReactionRemoved(MessageReactionRemoved),
    #[serde(rename = "message.sent")]
    MessageSent(MessageSent),

    // Extension-published external-message reports
    #[serde(rename = "message.delivered_reported")]
    MessageDeliveredReported(MessageDelivered<crate::RawMessagePublisherId>),
    #[serde(rename = "message.edited_reported")]
    MessageEditedReported(MessageEdited<crate::RawMessagePublisherId>),
    #[serde(rename = "message.deleted_reported")]
    MessageDeletedReported(MessageDeleted<crate::RawMessagePublisherId>),
    #[serde(rename = "message.reaction_added_reported")]
    MessageReactionAddedReported(MessageReactionAdded<crate::RawMessagePublisherId>),
    #[serde(rename = "message.reaction_removed_reported")]
    MessageReactionRemovedReported(MessageReactionRemoved<crate::RawMessagePublisherId>),
    #[serde(rename = "message.sent_reported")]
    MessageSentReported(MessageSent<crate::RawMessagePublisherId>),

    // Extension supervision
    #[serde(rename = "extension.starting")]
    ExtensionStarting(ExtensionStarting),
    #[serde(rename = "extension.ready")]
    ExtensionReady(ExtensionReady),
    #[serde(rename = "extension.exited")]
    ExtensionExited(ExtensionExited),
    #[serde(rename = "extension.restarting")]
    ExtensionRestarting(ExtensionRestarting),
    #[serde(rename = "extension.session_discovery_snapshot_declared")]
    ExtensionSessionDiscoverySnapshotDeclared(ExtensionSessionDiscoverySnapshotDeclared),
    #[serde(rename = "extension.agent_discovery_snapshot_declared")]
    ExtensionAgentDiscoverySnapshotDeclared(ExtensionAgentDiscoverySnapshotDeclared),
    #[serde(rename = "extension.context_provider_register")]
    ExtensionContextProviderRegister(ExtensionContextProviderRegister),
    #[serde(rename = "extension.session_context_provider_register")]
    ExtensionSessionContextProviderRegister(ExtensionSessionContextProviderRegister),
    #[serde(rename = "extension.context_ready")]
    ExtensionContextReady(ExtensionContextReady),
    #[serde(rename = "extension.session_context_ready")]
    ExtensionSessionContextReady(ExtensionSessionContextReady),
    #[serde(rename = "extension.agent_context_publish")]
    ExtAgentContextPublish(ExtAgentContextPublish),
    #[serde(rename = "extension.prompt_fragment_publish")]
    ExtPromptFragmentPublish(ExtPromptFragmentPublish),
    #[serde(rename = "extension.internal_prompt_submit_request")]
    ExtInternalPromptSubmitRequest(ExtInternalPromptSubmitRequest),
    #[serde(rename = "agent.start_request")]
    StartAgentRequest(StartAgentRequest),
    #[serde(rename = "agent.start_accepted")]
    StartAgentAccepted(StartAgentAccepted),
    #[serde(rename = "agent.start_result")]
    StartAgentResult(StartAgentResult),
    #[serde(rename = "agent.initialization_context_set")]
    AgentInitializationContextSet(AgentInitializationContextSet),
    #[serde(rename = "agent.message_sent")]
    AgentMessageSent(AgentMessageSent),
    #[serde(rename = "agent.message_received")]
    AgentMessageReceived(AgentMessageReceived),
    #[serde(rename = "extension.event")]
    ExtensionEvent(CustomEvent),
    #[serde(rename = "provider.models_declared")]
    ProviderModelsDeclared(ProviderModelsDeclared),
    #[serde(rename = "provider.models_updated")]
    ProviderModelsUpdated(ProviderModelsUpdated),
    /// Provider-authored full quota observation awaiting harness validation.
    #[serde(rename = "provider.quota_replace_reported")]
    ProviderQuotaReplaceReported(ProviderQuotaReplace),
    /// Provider-authored sparse quota observation awaiting harness validation.
    #[serde(rename = "provider.quota_patch_reported")]
    ProviderQuotaPatchReported(ProviderQuotaPatch),
    /// Provider-authored quota-clear observation awaiting harness validation.
    #[serde(rename = "provider.quota_clear_reported")]
    ProviderQuotaClearReported(ProviderQuotaClear),
    #[serde(rename = "provider.tool_result")]
    ProviderToolResult(ToolResult),
    #[serde(rename = "provider.tool_error")]
    ProviderToolError(ToolError),

    // Harness notices
    #[serde(rename = "harness.notice")]
    HarnessNotice(HarnessNotice),
    #[serde(rename = "harness.session_dir")]
    HarnessSessionDir(HarnessSessionDir),
    #[serde(rename = "harness.ui_dir")]
    HarnessUiDir(HarnessUiDir),
    #[serde(rename = "harness.models_available")]
    HarnessModelsAvailable(HarnessModelsAvailable),
    #[serde(rename = "harness.roles_available")]
    HarnessRolesAvailable(HarnessRolesAvailable),
    #[serde(rename = "harness.agent_context_initialized")]
    HarnessAgentContextInitialized(HarnessAgentContextInitialized),
    #[serde(rename = "harness.session_skills_available")]
    HarnessSessionSkillsAvailable(HarnessSessionSkillsAvailable),
    #[serde(rename = "harness.role_selected")]
    HarnessRoleSelected(HarnessRoleSelected),
    #[serde(rename = "harness.context_usage_changed")]
    HarnessContextUsageChanged(HarnessContextUsageChanged),
    #[serde(rename = "harness.provider_quota_changed")]
    HarnessProviderQuotaChanged(HarnessProviderQuotaChanged),
    #[serde(rename = "harness.agent_context_usage_changed")]
    HarnessAgentContextUsageChanged(HarnessAgentContextUsageChanged),
    #[serde(rename = "agent.state")]
    AgentState(AgentStateChanged),
    #[serde(rename = "agent.watches_updated")]
    AgentWatchesUpdated(AgentWatchesUpdated),
    #[serde(rename = "agent.stats_updated")]
    AgentStatsUpdated(AgentStatsUpdated),
    #[serde(rename = "harness.efforts_available")]
    HarnessEffortsAvailable(HarnessEffortsAvailable),
    #[serde(rename = "harness.verbosities_available")]
    HarnessVerbositiesAvailable(HarnessVerbositiesAvailable),
    #[serde(rename = "harness.thinking_summaries_available")]
    HarnessThinkingSummariesAvailable(HarnessThinkingSummariesAvailable),

    // UI
    #[serde(rename = "ui.prompt_submitted")]
    UiPromptSubmitted(UiPromptSubmitted),
    #[serde(rename = "ui.prompt_draft")]
    UiPromptDraft(UiPromptDraft),
    #[serde(rename = "ui.focus_changed")]
    UiFocusChanged(UiFocusChanged),
    #[serde(rename = "ui.role_select")]
    UiRoleSelect(UiRoleSelect),
    #[serde(rename = "ui.agent_model_select")]
    UiAgentModelSelect(UiAgentModelSelect),
    #[serde(rename = "ui.role_update")]
    UiRoleUpdate(UiRoleUpdate),
    #[serde(rename = "ui.shell_command")]
    UiShellCommand(UiShellCommand),
    #[serde(rename = "ui.switch_session")]
    UiSwitchSession(UiSwitchSession),
    #[serde(rename = "ui.create_agent")]
    UiCreateAgent(UiCreateAgent),
    #[serde(rename = "ui.navigate_tree")]
    UiNavigateTree(UiNavigateTree),
    #[serde(rename = "ui.compact_request")]
    UiCompactRequest(UiCompactRequest),
    #[serde(rename = "ui.cancel_prompt")]
    UiCancelPrompt(UiCancelPrompt),
    #[serde(rename = "ui.retry_prompt")]
    UiRetryPrompt(UiRetryPrompt),
    #[serde(rename = "ui.retry_prompt_result")]
    UiRetryPromptResult(UiRetryPromptResult),
    #[serde(rename = "ui.set_agent_navigation_mode")]
    UiSetAgentNavigationMode(UiSetAgentNavigationMode),
    #[serde(rename = "ui.set_agent_navigation_mode_result")]
    UiSetAgentNavigationModeResult(UiSetAgentNavigationModeResult),
    #[serde(rename = "ui.recall_queued_prompt")]
    UiRecallQueuedPrompt(UiRecallQueuedPrompt),
    #[serde(rename = "ui.set_agent_display_name")]
    UiSetAgentDisplayName(UiSetAgentDisplayName),

    // Term (terminal-output side effects)
    #[serde(rename = "term.osc1337_set_user_var")]
    Osc1337SetUserVar(Osc1337SetUserVar),
    #[serde(rename = "term.bell")]
    TermBell(TermBell),

    // Shell (user-initiated)
    /// Peer-authored progress awaiting routed-command validation.
    #[serde(rename = "shell.command_progress_reported")]
    ShellCommandProgressReported(ShellCommandProgress),
    /// Harness-authored canonical progress fact.
    #[serde(rename = "shell.command_progress")]
    ShellCommandProgress(ShellCommandProgress),
    /// Peer-authored completion awaiting routed-command validation.
    #[serde(rename = "shell.command_finished_reported")]
    ShellCommandFinishedReported(ShellCommandFinished),
    /// Harness-authored canonical completion fact.
    #[serde(rename = "shell.command_finished")]
    ShellCommandFinished(ShellCommandFinished),

    // Agent transcript/runtime
    #[serde(rename = "agent.prompt_submitted")]
    AgentPromptSubmitted(AgentPromptSubmitted),
    #[serde(rename = "agent.prompt_queued")]
    AgentPromptQueued(AgentPromptQueued),
    #[serde(rename = "agent.prompt_recalled")]
    AgentPromptRecalled(AgentPromptRecalled),
    #[serde(rename = "agent.prompt_steered")]
    AgentPromptSteered(AgentPromptSteered),
    #[serde(rename = "agent.compaction_triggered")]
    AgentCompactionTriggered(AgentCompactionTriggered),
    #[serde(rename = "agent.compacted")]
    AgentCompacted(AgentCompacted),
    #[serde(rename = "agent.standalone_compaction_started")]
    AgentStandaloneCompactionStarted(AgentStandaloneCompactionStarted),
    #[serde(rename = "agent.manual_compaction_requested")]
    AgentManualCompactionRequested(AgentManualCompactionRequested),
    #[serde(rename = "agent.manual_compaction_request_failed")]
    AgentManualCompactionRequestFailed(AgentManualCompactionRequestFailed),
    #[serde(rename = "agent.standalone_compaction_failed")]
    AgentStandaloneCompactionFailed(AgentStandaloneCompactionFailed),
    #[serde(rename = "agent.inference_dispatch_started")]
    AgentInferenceDispatchStarted(AgentInferenceDispatchStarted),
    #[serde(rename = "agent.tool_dispatch_observed")]
    AgentToolDispatchObserved(AgentToolDispatchObserved),
    #[serde(rename = "agent.tool_backgrounded_observed")]
    AgentToolBackgroundedObserved(AgentToolBackgroundedObserved),
    #[serde(rename = "agent.tool_wait_observed")]
    AgentToolWaitObserved(AgentToolWaitObserved),
    #[serde(rename = "agent.tool_wait_registered")]
    AgentToolWaitRegistered(AgentToolWaitRegistered),
    #[serde(rename = "agent.activation_queued")]
    AgentActivationQueued(AgentActivationQueued),
    #[serde(rename = "agent.tool_wait_settled")]
    AgentToolWaitSettled(AgentToolWaitSettled),
    #[serde(rename = "agent.tool_cancellation_requested")]
    AgentToolCancellationRequested(AgentToolCancellationRequested),
    #[serde(rename = "agent.tool_terminal_classified")]
    AgentToolTerminalClassified(AgentToolTerminalClassified),
    #[serde(rename = "agent.prompt_created")]
    AgentPromptCreated(AgentPromptCreated),
    #[serde(rename = "agent.prompt_started")]
    AgentPromptStarted(AgentPromptStarted),
    #[serde(rename = "agent.prompt_terminated")]
    AgentPromptTerminated(AgentPromptTerminated),
    #[serde(rename = "agent.prompt_prewarm_requested")]
    AgentPromptPrewarmRequested(AgentPromptPrewarmRequested),
    #[serde(rename = "agent.user_message_injected")]
    AgentUserMessageInjected(AgentUserMessageInjected),
    #[serde(rename = "agent.head_moved")]
    AgentHeadMoved(AgentHeadMoved),
    #[serde(rename = "agent.started")]
    AgentStarted(AgentStarted),
    #[serde(rename = "agent.outer_turn_started")]
    AgentOuterTurnStarted(AgentOuterTurnStarted),
    #[serde(rename = "agent.outer_turn_finished")]
    AgentOuterTurnFinished(AgentOuterTurnFinished),
    #[serde(rename = "agent.user_interaction_recorded")]
    AgentUserInteractionRecorded(AgentUserInteractionRecorded),
    #[serde(rename = "agent.display_name_set")]
    AgentDisplayNameSet(AgentDisplayNameSet),
    /// Durable harness-authored canonical metadata-set fact.
    #[serde(rename = "agent.metadata_set")]
    AgentMetadataSet(AgentMetadataSet),
    /// Durable harness-authored canonical metadata-unset fact.
    #[serde(rename = "agent.metadata_unset")]
    AgentMetadataUnset(AgentMetadataUnset),
    /// Transient-by-default peer request whose commit does not imply
    /// acceptance.
    ///
    /// A valid request may produce [`Event::AgentMetadataSet`].
    #[serde(rename = "agent.metadata_set_request")]
    AgentMetadataSetRequest(AgentMetadataSet),
    /// Transient-by-default peer request whose commit does not imply
    /// acceptance.
    ///
    /// A valid request may produce [`Event::AgentMetadataUnset`].
    #[serde(rename = "agent.metadata_unset_request")]
    AgentMetadataUnsetRequest(AgentMetadataUnset),
    #[serde(rename = "agent.replay_complete")]
    AgentReplayComplete(AgentReplayComplete),

    // Session lifecycle/membership
    #[serde(rename = "session.started")]
    SessionStarted(SessionStarted),
    #[serde(rename = "session.shutdown")]
    SessionShutdown(SessionShutdown),
    #[serde(rename = "session.agent_loaded")]
    SessionAgentLoaded(SessionAgentLoaded),
    #[serde(rename = "session.agent_unloaded")]
    SessionAgentUnloaded(SessionAgentUnloaded),
    #[serde(rename = "session.replay_complete")]
    SessionReplayComplete(SessionReplayComplete),

    // Provider execution
    /// Provider-authored prompt-acceptance observation awaiting harness
    /// validation.
    #[serde(rename = "provider.prompt_submitted_reported")]
    ProviderPromptSubmittedReported(ProviderPromptSubmitted),
    #[serde(rename = "provider.prompt_submitted")]
    ProviderPromptSubmitted(ProviderPromptSubmitted),
    /// Provider-authored streaming observation awaiting harness validation.
    #[serde(rename = "provider.response_updated_reported")]
    ProviderResponseUpdatedReported(ProviderResponseUpdated),
    #[serde(rename = "provider.response_updated")]
    ProviderResponseUpdated(ProviderResponseUpdated),
    /// Provider-authored terminal observation awaiting harness validation.
    #[serde(rename = "provider.response_finished_reported")]
    ProviderResponseFinishedReported(ProviderResponseFinished),
    #[serde(rename = "provider.response_finished")]
    ProviderResponseFinished(ProviderResponseFinished),
    /// Provider-authored manual-retry outcome awaiting harness correlation.
    #[serde(rename = "provider.retry_prompt_result_reported")]
    ProviderRetryPromptResultReported(ProviderRetryPromptResult),
    /// Provider-authored cache observation awaiting harness validation.
    #[serde(rename = "provider.cache_miss_diagnostic_reported")]
    ProviderCacheMissDiagnosticReported(ProviderCacheMissDiagnostic),
    #[serde(rename = "provider.cache_miss_diagnostic")]
    ProviderCacheMissDiagnostic(ProviderCacheMissDiagnostic),
}

impl Event {
    /// Convert an exactly authenticated extension message report into its
    /// canonical fact shape.
    ///
    /// Conversion requires a byte-exact match between the raw top-level claim
    /// and `publisher`, then replaces that claim with the validated canonical
    /// identity. Mismatched claims and non-report events return `None`.
    #[must_use]
    pub fn into_stamped_canonical_message_fact(
        self,
        publisher: crate::MessagePublisherId,
    ) -> Option<Self> {
        match self {
            Self::MessageDeliveredReported(report)
                if report.publisher_extension_id.as_str() == publisher.as_str() =>
            {
                Some(Self::MessageDelivered(report.with_publisher(publisher)))
            }
            Self::MessageEditedReported(report)
                if report.publisher_extension_id.as_str() == publisher.as_str() =>
            {
                Some(Self::MessageEdited(report.with_publisher(publisher)))
            }
            Self::MessageDeletedReported(report)
                if report.publisher_extension_id.as_str() == publisher.as_str() =>
            {
                Some(Self::MessageDeleted(report.with_publisher(publisher)))
            }
            Self::MessageReactionAddedReported(report)
                if report.publisher_extension_id.as_str() == publisher.as_str() =>
            {
                Some(Self::MessageReactionAdded(report.with_publisher(publisher)))
            }
            Self::MessageReactionRemovedReported(report)
                if report.publisher_extension_id.as_str() == publisher.as_str() =>
            {
                Some(Self::MessageReactionRemoved(
                    report.with_publisher(publisher),
                ))
            }
            Self::MessageSentReported(report)
                if report.publisher_extension_id.as_str() == publisher.as_str() =>
            {
                Some(Self::MessageSent(report.with_publisher(publisher)))
            }
            _ => None,
        }
    }

    /// Return whether this event is an extension-authored message report.
    #[must_use]
    pub const fn is_message_report(&self) -> bool {
        matches!(
            self,
            Self::MessageDeliveredReported(_)
                | Self::MessageEditedReported(_)
                | Self::MessageDeletedReported(_)
                | Self::MessageReactionAddedReported(_)
                | Self::MessageReactionRemovedReported(_)
                | Self::MessageSentReported(_)
        )
    }

    /// Return the raw claimed transcript target for a message fact.
    #[must_use]
    pub fn message_agent_target(&self) -> Option<&crate::MessageAgentTarget> {
        match self {
            Self::MessageDelivered(fact) => Some(&fact.agent_id),
            Self::MessageEdited(fact) => Some(&fact.agent_id),
            Self::MessageDeleted(fact) => Some(&fact.agent_id),
            Self::MessageReactionAdded(fact) => Some(&fact.agent_id),
            Self::MessageReactionRemoved(fact) => Some(&fact.agent_id),
            Self::MessageSent(fact) => Some(&fact.agent_id),
            _ => None,
        }
    }

    /// Replace a message fact's claimed publisher with authenticated
    /// provenance.
    ///
    /// Returns false when this event is not a message fact.
    pub fn stamp_message_publisher(&mut self, publisher: crate::MessagePublisherId) -> bool {
        let target = match self {
            Self::MessageDelivered(fact) => &mut fact.publisher_extension_id,
            Self::MessageEdited(fact) => &mut fact.publisher_extension_id,
            Self::MessageDeleted(fact) => &mut fact.publisher_extension_id,
            Self::MessageReactionAdded(fact) => &mut fact.publisher_extension_id,
            Self::MessageReactionRemoved(fact) => &mut fact.publisher_extension_id,
            Self::MessageSent(fact) => &mut fact.publisher_extension_id,
            _ => return false,
        };
        *target = publisher;
        true
    }

    /// Returns the dotted event name carried by this envelope.
    #[must_use]
    pub fn name(&self) -> EventName {
        if let Self::ExtensionEvent(event) = self {
            return event.name().clone();
        }
        if let Some(name) = self.tool_event_name() {
            return name;
        }
        if let Some(name) = self.action_event_name() {
            return name;
        }
        if let Some(name) = self.message_event_name() {
            return name;
        }
        if let Some(name) = self.extension_and_delegation_event_name() {
            return name;
        }
        if let Some(name) = self.provider_capability_event_name() {
            return name;
        }
        if let Some(name) = self.harness_event_name() {
            return name;
        }
        if let Some(name) = self.ui_event_name() {
            return name;
        }
        if let Some(name) = self.terminal_and_shell_event_name() {
            return name;
        }
        if let Some(name) = self.agent_event_name() {
            return name;
        }
        if let Some(name) = self.session_event_name() {
            return name;
        }
        if let Some(name) = self.provider_execution_event_name() {
            return name;
        }
        unreachable!("all Event variants must map to an EventName")
    }

    fn tool_event_name(&self) -> Option<EventName> {
        match self {
            Self::ToolRegistrationDeclared(_) => EventName::TOOL_REGISTRATION_DECLARED,
            Self::ToolUnregistrationDeclared(_) => EventName::TOOL_UNREGISTRATION_DECLARED,
            Self::ToolRegister(_) => EventName::TOOL_REGISTER,
            Self::ToolUnregister(_) => EventName::TOOL_UNREGISTER,
            Self::ToolRequest(_) => EventName::TOOL_REQUEST,
            Self::ToolStarted(_) => EventName::TOOL_STARTED,
            Self::ToolRejected(_) => EventName::TOOL_REJECTED,
            Self::ToolResultReported(_) => EventName::TOOL_RESULT_REPORTED,
            Self::ToolResult(_) => EventName::TOOL_RESULT,
            Self::ToolErrorReported(_) => EventName::TOOL_ERROR_REPORTED,
            Self::ToolError(_) => EventName::TOOL_ERROR,
            Self::ToolBackgroundResult(_) => EventName::TOOL_BACKGROUND_RESULT,
            Self::ToolBackgroundError(_) => EventName::TOOL_BACKGROUND_ERROR,
            Self::ToolProgressReported(_) => EventName::TOOL_PROGRESS_REPORTED,
            Self::ToolProgress(_) => EventName::TOOL_PROGRESS,
            Self::ToolCancelRequest(_) => EventName::TOOL_CANCEL_REQUEST,
            Self::ToolCancelledReported(_) => EventName::TOOL_CANCELLED_REPORTED,
            Self::ToolCancelled(_) => EventName::TOOL_CANCELLED,
            Self::ToolDelegateProgress(_) => EventName::TOOL_DELEGATE_PROGRESS,
            _ => return None,
        }
        .into()
    }

    fn action_event_name(&self) -> Option<EventName> {
        match self {
            Self::ActionSchemaPublished(_) => EventName::ACTION_SCHEMA_PUBLISHED,
            Self::ActionInvoke(_) => EventName::ACTION_INVOKE,
            Self::ActionResult(_) => EventName::ACTION_RESULT,
            Self::ActionError(_) => EventName::ACTION_ERROR,
            _ => return None,
        }
        .into()
    }

    fn message_event_name(&self) -> Option<EventName> {
        match self {
            Self::MessageDelivered(_) => EventName::MESSAGE_DELIVERED,
            Self::MessageDeliveredReported(_) => EventName::MESSAGE_DELIVERED_REPORTED,
            Self::MessageEdited(_) => EventName::MESSAGE_EDITED,
            Self::MessageEditedReported(_) => EventName::MESSAGE_EDITED_REPORTED,
            Self::MessageDeleted(_) => EventName::MESSAGE_DELETED,
            Self::MessageDeletedReported(_) => EventName::MESSAGE_DELETED_REPORTED,
            Self::MessageReactionAdded(_) => EventName::MESSAGE_REACTION_ADDED,
            Self::MessageReactionAddedReported(_) => EventName::MESSAGE_REACTION_ADDED_REPORTED,
            Self::MessageReactionRemoved(_) => EventName::MESSAGE_REACTION_REMOVED,
            Self::MessageReactionRemovedReported(_) => EventName::MESSAGE_REACTION_REMOVED_REPORTED,
            Self::MessageSent(_) => EventName::MESSAGE_SENT,
            Self::MessageSentReported(_) => EventName::MESSAGE_SENT_REPORTED,
            _ => return None,
        }
        .into()
    }

    fn extension_and_delegation_event_name(&self) -> Option<EventName> {
        match self {
            Self::ExtensionStarting(_) => EventName::EXTENSION_STARTING,
            Self::ExtensionReady(_) => EventName::EXTENSION_READY,
            Self::ExtensionExited(_) => EventName::EXTENSION_EXITED,
            Self::ExtensionRestarting(_) => EventName::EXTENSION_RESTARTING,
            Self::ExtensionSessionDiscoverySnapshotDeclared(_) => {
                EventName::EXTENSION_SESSION_DISCOVERY_SNAPSHOT_DECLARED
            }
            Self::ExtensionAgentDiscoverySnapshotDeclared(_) => {
                EventName::EXTENSION_AGENT_DISCOVERY_SNAPSHOT_DECLARED
            }
            Self::ExtensionContextProviderRegister(_) => {
                EventName::EXTENSION_CONTEXT_PROVIDER_REGISTER
            }
            Self::ExtensionSessionContextProviderRegister(_) => {
                EventName::EXTENSION_SESSION_CONTEXT_PROVIDER_REGISTER
            }
            Self::ExtensionContextReady(_) => EventName::EXTENSION_CONTEXT_READY,
            Self::ExtensionSessionContextReady(_) => EventName::EXTENSION_SESSION_CONTEXT_READY,
            Self::ExtAgentContextPublish(_) => EventName::EXTENSION_AGENT_CONTEXT_PUBLISH,
            Self::ExtPromptFragmentPublish(_) => EventName::EXTENSION_PROMPT_FRAGMENT_PUBLISH,
            Self::ExtInternalPromptSubmitRequest(_) => {
                EventName::EXTENSION_INTERNAL_PROMPT_SUBMIT_REQUEST
            }
            Self::StartAgentRequest(_) => EventName::AGENT_START_REQUEST,
            Self::StartAgentAccepted(_) => EventName::AGENT_START_ACCEPTED,
            Self::StartAgentResult(_) => EventName::AGENT_START_RESULT,
            Self::AgentMessageSent(_) => EventName::AGENT_MESSAGE_SENT,
            Self::AgentMessageReceived(_) => EventName::AGENT_MESSAGE_RECEIVED,
            _ => return None,
        }
        .into()
    }

    fn provider_capability_event_name(&self) -> Option<EventName> {
        match self {
            Self::ProviderModelsDeclared(_) => EventName::PROVIDER_MODELS_DECLARED,
            Self::ProviderModelsUpdated(_) => EventName::PROVIDER_MODELS_UPDATED,
            Self::ProviderQuotaReplaceReported(_) => EventName::PROVIDER_QUOTA_REPLACE_REPORTED,
            Self::ProviderQuotaPatchReported(_) => EventName::PROVIDER_QUOTA_PATCH_REPORTED,
            Self::ProviderQuotaClearReported(_) => EventName::PROVIDER_QUOTA_CLEAR_REPORTED,
            Self::ProviderToolResult(_) => EventName::PROVIDER_TOOL_RESULT,
            Self::ProviderToolError(_) => EventName::PROVIDER_TOOL_ERROR,
            _ => return None,
        }
        .into()
    }

    fn harness_event_name(&self) -> Option<EventName> {
        match self {
            Self::HarnessNotice(_) => EventName::HARNESS_NOTICE,
            Self::HarnessSessionDir(_) => EventName::HARNESS_SESSION_DIR,
            Self::HarnessUiDir(_) => EventName::HARNESS_UI_DIR,
            Self::HarnessModelsAvailable(_) => EventName::HARNESS_MODELS_AVAILABLE,
            Self::HarnessRolesAvailable(_) => EventName::HARNESS_ROLES_AVAILABLE,
            Self::HarnessAgentContextInitialized(_) => EventName::HARNESS_AGENT_CONTEXT_INITIALIZED,
            Self::HarnessSessionSkillsAvailable(_) => EventName::HARNESS_SESSION_SKILLS_AVAILABLE,
            Self::HarnessRoleSelected(_) => EventName::HARNESS_ROLE_SELECTED,
            Self::HarnessContextUsageChanged(_) => EventName::HARNESS_CONTEXT_USAGE_CHANGED,
            Self::HarnessProviderQuotaChanged(_) => EventName::HARNESS_PROVIDER_QUOTA_CHANGED,
            Self::HarnessAgentContextUsageChanged(_) => {
                EventName::HARNESS_AGENT_CONTEXT_USAGE_CHANGED
            }
            Self::AgentState(_) => EventName::AGENT_STATE,
            Self::AgentWatchesUpdated(_) => EventName::AGENT_WATCHES_UPDATED,
            Self::AgentStatsUpdated(_) => EventName::AGENT_STATS_UPDATED,
            Self::HarnessEffortsAvailable(_) => EventName::HARNESS_EFFORTS_AVAILABLE,
            Self::HarnessVerbositiesAvailable(_) => EventName::HARNESS_VERBOSITIES_AVAILABLE,
            Self::HarnessThinkingSummariesAvailable(_) => {
                EventName::HARNESS_THINKING_SUMMARIES_AVAILABLE
            }
            _ => return None,
        }
        .into()
    }

    fn ui_event_name(&self) -> Option<EventName> {
        match self {
            Self::UiPromptSubmitted(_) => EventName::UI_PROMPT_SUBMITTED,
            Self::UiPromptDraft(_) => EventName::UI_PROMPT_DRAFT,
            Self::UiFocusChanged(_) => EventName::UI_FOCUS_CHANGED,
            Self::UiRoleSelect(_) => EventName::UI_ROLE_SELECT,
            Self::UiAgentModelSelect(_) => EventName::UI_AGENT_MODEL_SELECT,
            Self::UiRoleUpdate(_) => EventName::UI_ROLE_UPDATE,
            Self::UiShellCommand(_) => EventName::UI_SHELL_COMMAND,
            Self::UiSwitchSession(_) => EventName::UI_SWITCH_SESSION,
            Self::UiCreateAgent(_) => EventName::UI_CREATE_AGENT,
            Self::UiNavigateTree(_) => EventName::UI_NAVIGATE_TREE,
            Self::UiCompactRequest(_) => EventName::UI_COMPACT_REQUEST,
            Self::UiCancelPrompt(_) => EventName::UI_CANCEL_PROMPT,
            Self::UiRetryPrompt(_) => EventName::UI_RETRY_PROMPT,
            Self::UiRetryPromptResult(_) => EventName::UI_RETRY_PROMPT_RESULT,
            Self::UiSetAgentNavigationMode(_) => EventName::UI_SET_AGENT_NAVIGATION_MODE,
            Self::UiSetAgentNavigationModeResult(_) => {
                EventName::UI_SET_AGENT_NAVIGATION_MODE_RESULT
            }
            Self::UiRecallQueuedPrompt(_) => EventName::UI_RECALL_QUEUED_PROMPT,
            Self::UiSetAgentDisplayName(_) => EventName::UI_SET_AGENT_DISPLAY_NAME,
            _ => return None,
        }
        .into()
    }

    fn terminal_and_shell_event_name(&self) -> Option<EventName> {
        match self {
            Self::Osc1337SetUserVar(_) => EventName::TERM_OSC1337_SET_USER_VAR,
            Self::TermBell(_) => EventName::TERM_BELL,
            Self::ShellCommandProgressReported(_) => EventName::SHELL_COMMAND_PROGRESS_REPORTED,
            Self::ShellCommandProgress(_) => EventName::SHELL_COMMAND_PROGRESS,
            Self::ShellCommandFinishedReported(_) => EventName::SHELL_COMMAND_FINISHED_REPORTED,
            Self::ShellCommandFinished(_) => EventName::SHELL_COMMAND_FINISHED,
            _ => return None,
        }
        .into()
    }

    fn agent_event_name(&self) -> Option<EventName> {
        match self {
            Self::AgentPromptSubmitted(_) => EventName::AGENT_PROMPT_SUBMITTED,
            Self::AgentPromptQueued(_) => EventName::AGENT_PROMPT_QUEUED,
            Self::AgentPromptRecalled(_) => EventName::AGENT_PROMPT_RECALLED,
            Self::AgentPromptSteered(_) => EventName::AGENT_PROMPT_STEERED,
            Self::AgentCompactionTriggered(_) => EventName::AGENT_COMPACTION_TRIGGERED,
            Self::AgentCompacted(_) => EventName::AGENT_COMPACTED,
            Self::AgentStandaloneCompactionStarted(_) => {
                EventName::AGENT_STANDALONE_COMPACTION_STARTED
            }
            Self::AgentManualCompactionRequested(_) => EventName::AGENT_MANUAL_COMPACTION_REQUESTED,
            Self::AgentManualCompactionRequestFailed(_) => {
                EventName::AGENT_MANUAL_COMPACTION_REQUEST_FAILED
            }
            Self::AgentStandaloneCompactionFailed(_) => {
                EventName::AGENT_STANDALONE_COMPACTION_FAILED
            }
            Self::AgentInferenceDispatchStarted(_) => EventName::AGENT_INFERENCE_DISPATCH_STARTED,
            Self::AgentToolDispatchObserved(_) => EventName::AGENT_TOOL_DISPATCH_OBSERVED,
            Self::AgentToolBackgroundedObserved(_) => EventName::AGENT_TOOL_BACKGROUNDED_OBSERVED,
            Self::AgentToolWaitObserved(_) => EventName::AGENT_TOOL_WAIT_OBSERVED,
            Self::AgentToolWaitRegistered(_) => EventName::AGENT_TOOL_WAIT_REGISTERED,
            Self::AgentActivationQueued(_) => EventName::AGENT_ACTIVATION_QUEUED,
            Self::AgentToolWaitSettled(_) => EventName::AGENT_TOOL_WAIT_SETTLED,
            Self::AgentToolCancellationRequested(_) => EventName::AGENT_TOOL_CANCELLATION_REQUESTED,
            Self::AgentToolTerminalClassified(_) => EventName::AGENT_TOOL_TERMINAL_CLASSIFIED,
            Self::AgentPromptCreated(_) => EventName::AGENT_PROMPT_CREATED,
            Self::AgentPromptStarted(_) => EventName::AGENT_PROMPT_STARTED,
            Self::AgentOuterTurnStarted(_) => EventName::AGENT_OUTER_TURN_STARTED,
            Self::AgentOuterTurnFinished(_) => EventName::AGENT_OUTER_TURN_FINISHED,
            Self::AgentPromptTerminated(_) => EventName::AGENT_PROMPT_TERMINATED,
            Self::AgentPromptPrewarmRequested(_) => EventName::AGENT_PROMPT_PREWARM_REQUESTED,
            Self::AgentUserMessageInjected(_) => EventName::AGENT_USER_MESSAGE_INJECTED,
            Self::AgentHeadMoved(_) => EventName::AGENT_HEAD_MOVED,
            Self::AgentStarted(_) => EventName::AGENT_STARTED,
            Self::AgentUserInteractionRecorded(_) => EventName::AGENT_USER_INTERACTION_RECORDED,
            Self::AgentDisplayNameSet(_) => EventName::AGENT_DISPLAY_NAME_SET,
            Self::AgentMetadataSet(_) => EventName::AGENT_METADATA_SET,
            Self::AgentMetadataUnset(_) => EventName::AGENT_METADATA_UNSET,
            Self::AgentMetadataSetRequest(_) => EventName::AGENT_METADATA_SET_REQUEST,
            Self::AgentMetadataUnsetRequest(_) => EventName::AGENT_METADATA_UNSET_REQUEST,
            Self::AgentInitializationContextSet(_) => EventName::AGENT_INITIALIZATION_CONTEXT_SET,
            Self::AgentReplayComplete(_) => EventName::AGENT_REPLAY_COMPLETE,
            _ => return None,
        }
        .into()
    }

    fn session_event_name(&self) -> Option<EventName> {
        match self {
            Self::SessionStarted(_) => EventName::SESSION_STARTED,
            Self::SessionShutdown(_) => EventName::SESSION_SHUTDOWN,
            Self::SessionAgentLoaded(_) => EventName::SESSION_AGENT_LOADED,
            Self::SessionAgentUnloaded(_) => EventName::SESSION_AGENT_UNLOADED,
            Self::SessionReplayComplete(_) => EventName::SESSION_REPLAY_COMPLETE,
            _ => return None,
        }
        .into()
    }

    fn provider_execution_event_name(&self) -> Option<EventName> {
        match self {
            Self::ProviderPromptSubmittedReported(_) => {
                EventName::PROVIDER_PROMPT_SUBMITTED_REPORTED
            }
            Self::ProviderPromptSubmitted(_) => EventName::PROVIDER_PROMPT_SUBMITTED,
            Self::ProviderResponseUpdatedReported(_) => {
                EventName::PROVIDER_RESPONSE_UPDATED_REPORTED
            }
            Self::ProviderResponseUpdated(_) => EventName::PROVIDER_RESPONSE_UPDATED,
            Self::ProviderResponseFinishedReported(_) => {
                EventName::PROVIDER_RESPONSE_FINISHED_REPORTED
            }
            Self::ProviderResponseFinished(_) => EventName::PROVIDER_RESPONSE_FINISHED,
            Self::ProviderRetryPromptResultReported(_) => {
                EventName::PROVIDER_RETRY_PROMPT_RESULT_REPORTED
            }
            Self::ProviderCacheMissDiagnosticReported(_) => {
                EventName::PROVIDER_CACHE_MISS_DIAGNOSTIC_REPORTED
            }
            Self::ProviderCacheMissDiagnostic(_) => EventName::PROVIDER_CACHE_MISS_DIAGNOSTIC,
            _ => return None,
        }
        .into()
    }

    /// Returns true for protocol events that request persistence by default
    /// when sent directly without an explicit [`crate::Emit`] durability
    /// override.
    #[must_use]
    pub const fn defaults_to_persist(&self) -> bool {
        !matches!(
            self,
            Self::ToolRegistrationDeclared(_)
                | Self::ToolUnregistrationDeclared(_)
                | Self::ToolRegister(_)
                | Self::ToolUnregister(_)
                | Self::ToolResultReported(_)
                | Self::ToolErrorReported(_)
                | Self::ToolCancelledReported(_)
                | Self::ToolCancelled(_)
                | Self::MessageDeliveredReported(_)
                | Self::MessageEditedReported(_)
                | Self::MessageDeletedReported(_)
                | Self::MessageReactionAddedReported(_)
                | Self::MessageReactionRemovedReported(_)
                | Self::MessageSentReported(_)
                | Self::ProviderModelsDeclared(_)
                | Self::ProviderModelsUpdated(_)
                | Self::ProviderPromptSubmittedReported(_)
                | Self::ProviderResponseUpdatedReported(_)
                | Self::ProviderResponseFinishedReported(_)
                | Self::ProviderRetryPromptResultReported(_)
                | Self::ProviderCacheMissDiagnosticReported(_)
                | Self::ProviderResponseUpdated(_)
                | Self::ProviderQuotaReplaceReported(_)
                | Self::ProviderQuotaPatchReported(_)
                | Self::ProviderQuotaClearReported(_)
                | Self::HarnessProviderQuotaChanged(_)
                | Self::ProviderPromptSubmitted(_)
                | Self::ToolProgressReported(_)
                | Self::ToolProgress(_)
                | Self::ToolDelegateProgress(_)
                | Self::ToolError(_)
                | Self::ActionSchemaPublished(_)
                | Self::ActionInvoke(_)
                | Self::ActionResult(_)
                | Self::ActionError(_)
                | Self::ExtPromptFragmentPublish(_)
                | Self::ExtensionSessionDiscoverySnapshotDeclared(_)
                | Self::ExtensionAgentDiscoverySnapshotDeclared(_)
                | Self::ExtensionSessionContextProviderRegister(_)
                | Self::ExtensionSessionContextReady(_)
                | Self::ExtensionContextProviderRegister(_)
                | Self::ExtensionContextReady(_)
                | Self::ExtAgentContextPublish(_)
                | Self::ExtInternalPromptSubmitRequest(_)
                | Self::StartAgentRequest(_)
                | Self::AgentMetadataSetRequest(_)
                | Self::AgentMetadataUnsetRequest(_)
                | Self::ShellCommandProgressReported(_)
                | Self::ShellCommandProgress(_)
                | Self::ShellCommandFinishedReported(_)
                | Self::UiPromptSubmitted(_)
                | Self::AgentPromptQueued(_)
                | Self::AgentPromptRecalled(_)
                | Self::AgentPromptCreated(_)
                | Self::AgentPromptStarted(_)
                | Self::AgentPromptTerminated(_)
                | Self::AgentPromptPrewarmRequested(_)
                | Self::AgentState(_)
                | Self::AgentWatchesUpdated(_)
                | Self::AgentStatsUpdated(_)
                | Self::HarnessAgentContextInitialized(_)
                | Self::HarnessSessionSkillsAvailable(_)
                | Self::AgentReplayComplete(_)
                | Self::SessionReplayComplete(_)
                | Self::UiCompactRequest(_)
                | Self::UiCreateAgent(_)
                | Self::UiPromptDraft(_)
                | Self::UiFocusChanged(_)
                | Self::UiSetAgentNavigationMode(_)
                | Self::UiSetAgentNavigationModeResult(_)
                | Self::UiSetAgentDisplayName(_)
        )
    }
}
