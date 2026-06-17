//! Protocol event types and payloads.
//!
//! All event definitions live here so `grep events.rs` finds them.
//!
//! Events are facts — each component broadcasts what happened.
//! There are no requests or responses, only announcements.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    ActionInvocationId, AgentContextKey, AgentId, AgentMessageId, AgentMetadataKey, AgentPromptId,
    CborValue, ContextItem, DiffSummary, EventCategory, EventName, ExtensionInstanceId,
    ExtensionName, ModelId, PromptContext, PromptFragment, ProviderResponseItem,
    ProviderTokenUsage, SessionId, SkillName, ToolCallId, ToolDefinition, ToolGroupName, ToolName,
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
}

impl SessionDirStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Resumed => "resumed",
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
    /// Stable group name from harness `role_groups` configuration.
    pub name: String,
    /// Role names in navigation order. Names are accepted by `ui.role_select`.
    pub roles: Vec<String>,
}

/// One reusable prompt template configured by the running harness and offered
/// to UI clients as `/prompt <id>`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessCustomPrompt {
    /// Stable prompt id accepted by `/prompt <id>`.
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

/// Current context usage for one durable agent transcript.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessAgentContextUsageChanged {
    /// Durable agent whose context usage changed.
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
    /// else (`/role engineer effort xhigh`, OpenAI's `reasoning_effort` field,
    /// `Display`, `FromStr`, `effort_wire`). Pin it explicitly so
    /// serde-driven config paths (`default_efforts`,
    /// `reasoningEfforts`) agree with the rest.
    #[serde(rename = "xhigh")]
    XHigh = 5,
}

impl Effort {
    /// Cycles to the next level (wraps `XHigh → Off`).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Off => Self::Minimal,
            Self::Minimal => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::XHigh,
            Self::XHigh => Self::Off,
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
        for _ in 0..6 {
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
            other => Err(ParseEffortError {
                input: other.to_owned(),
            }),
        }
    }
}

/// Error returned when an effort string is not one of the well-known
/// levels (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`).
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
            "unknown effort level `{}`; expected off/minimal/low/medium/high/xhigh",
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
    /// Whether this tool should be advertised to the agent when the role has
    /// no explicit `tools` allow-list and `disable_tools` does not remove it.
    #[serde(default = "tool_enabled_by_default", skip_serializing_if = "is_true")]
    pub enabled_by_default: bool,
    /// Whether the harness may close the model-visible foreground turn before
    /// the real tool process has returned. `None` means the harness applies its
    /// default policy, currently
    /// [`BackgroundSupport::MinForegroundSeconds`]`(5)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_support: Option<BackgroundSupport>,
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
        Self::MinForegroundSeconds(5)
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
    /// Full slash-action schema published by the extension.
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
    /// Original slash command line submitted by the user.
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

/// Tool registration event emitted by an extension or provider.
///
/// Registers or refreshes one tool definition and its optional grouping/prompt
/// context. The harness uses this as routing metadata for later tool requests.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolRegister {
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

/// Tool unregistration event emitted when a provider withdraws one tool.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolUnregister {
    /// Name of the tool to remove from harness routing metadata.
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
    /// Durable agent that owns this tool call.
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
    /// Durable agent that owns this tool call.
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultKind {
    #[default]
    Final,
    BackgroundPlaceholder,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub tool_type: ToolType,
    pub result: CborValue,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolError {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub tool_type: ToolType,
    pub message: String,
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
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub tool_type: ToolType,
    pub result: CborValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolUseState>,
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// Real error result for a tool call whose foreground was already completed
/// with a synthetic background placeholder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolBackgroundError {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub tool_type: ToolType,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<CborValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolUseState>,
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
    /// Status word/message rendered as the last chip (e.g. `"ok"`,
    /// `"regex parse error"`). For
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

/// Live progress update emitted by a tool provider for one in-flight call.
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

/// Live snapshot of a sub-agent spawned by the `agent_start` tool.
///
/// Emitted by the harness whenever the side conversation backing a
/// `agent_start` invocation makes observable progress: a tool call starts
/// or finishes, or the sub-agent reports new context-token usage. The
/// CLI re-renders the running `agent_start` tool block to surface this
/// to the user without persisting per-update history. Transient — not
/// folded into any durable semantic log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DelegateProgress {
    /// The original parent `agent_start` call — the tool block under
    /// which this update should appear.
    pub call_id: ToolCallId,
    /// Display name the parent agent provided for the sub-task.
    pub task_name: String,
    /// Agent id assigned to the delegated sub-agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCancelled {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
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

/// An extension discovered a skill and is advertising it to the harness.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtSkillAvailable {
    pub name: SkillName,
    pub description: String,
    /// Absolute path to the skill file so the harness can read it.
    pub file_path: std::path::PathBuf,
    /// When true the harness should include this skill in the system prompt.
    pub add_to_prompt: bool,
    /// Whether users may explicitly invoke this skill with `/skill`.
    #[serde(default = "default_true")]
    pub user_invocable: bool,
    /// Whether model-side skill discovery/loading should hide this skill.
    #[serde(default)]
    pub disable_model_invocation: bool,
    /// Optional UI hint for arguments accepted by this skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
}

/// An extension discovered one AGENTS.md file and is advertising it to the
/// harness.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtAgentsMdAvailable {
    /// Absolute path to the AGENTS.md file.
    pub file_path: std::path::PathBuf,
    /// Full file contents, sent eagerly so the harness can inject them
    /// without an extra tool round trip.
    pub content: String,
}

/// An extension declares that it will publish per-agent prompt context after
/// each matching `session.agent_loaded` event and acknowledge completion with
/// `extension.context_ready`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionContextProviderRegister {}

/// An extension finished broadcasting refreshed prompt context for one agent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionContextReady {
    /// Session containing the loaded agent.
    pub session_id: SessionId,
    /// Durable agent whose context contributions are complete for now.
    pub agent_id: AgentId,
}

/// Arbitrary JSON value published by an extension for one agent context key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentContextValue(pub serde_json::Value);

/// An extension publishes its complete agent-scoped contribution for one key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExtAgentContextPublish {
    /// Durable agent this context belongs to.
    pub agent_id: AgentId,
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

/// Request from an extension to submit a normal user prompt to a loaded agent.
///
/// The harness owns validation and transcript publication for this request;
/// extensions must not publish `agent.prompt_submitted` directly.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtPromptSubmitRequest {
    /// Loaded durable agent that should receive the prompt.
    pub agent_id: AgentId,
    /// User-style prompt text to submit.
    pub text: String,
    /// Optional submitter correlation id copied to the created prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_id: Option<String>,
}

/// Recipient of a global agent-to-agent or agent-to-user message.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentMessageRecipient {
    /// Deliver the message to another durable agent transcript.
    Agent { agent_id: AgentId },
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
}

impl AgentMessageKind {
    fn is_default(&self) -> bool {
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
    /// Recipient agent id that received the message.
    pub recipient_id: AgentId,
    /// Delivery source semantics.
    #[serde(default, skip_serializing_if = "AgentMessageKind::is_default")]
    pub kind: AgentMessageKind,
    /// Message body.
    pub message: String,
}

/// Durable agent branch-state fact: the selected head moved to an existing
/// transcript node, so the next append should branch from there.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentHeadMoved {
    /// Agent whose selected branch head changed.
    pub agent_id: AgentId,
    /// Existing transcript node that is now the selected branch head.
    pub node_id: NodeId,
}

/// Immutable agent creation fact recorded at the start of an agent log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentStarted {
    /// Durable agent this log belongs to.
    pub agent_id: AgentId,
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
}

/// Durable per-agent metadata value update.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentMetadataSet {
    /// Agent whose metadata map is updated.
    pub agent_id: AgentId,
    /// Metadata key to replace.
    pub key: AgentMetadataKey,
    /// Arbitrary CBOR metadata value.
    pub value: CborValue,
    /// Whether this key is copied to newly-created child agents.
    #[serde(default, skip_serializing_if = "is_false")]
    pub inheritable: bool,
}

/// Durable per-agent metadata deletion.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentMetadataUnset {
    /// Agent whose metadata map is updated.
    pub agent_id: AgentId,
    /// Metadata key to delete.
    pub key: AgentMetadataKey,
}

pub const MAX_AGENT_METADATA_VALUE_BYTES: usize = 64 * 1024;
pub const MAX_AGENT_METADATA_KEY_BYTES: usize = 256;

/// Durable fact that updates an agent's human-friendly display name.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentDisplayNameSet {
    /// Agent whose display name changed.
    pub agent_id: AgentId,
    /// New human-friendly display name. Empty names are rejected by the
    /// harness.
    pub display_name: String,
}

/// Durable session membership fact: an agent is now loaded in a session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionAgentLoaded {
    /// Session membership container.
    pub session_id: SessionId,
    /// Durable agent now available in the session.
    pub agent_id: AgentId,
}

/// Durable session membership fact: an agent is no longer loaded in a session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionAgentUnloaded {
    /// Session membership container.
    pub session_id: SessionId,
    /// Durable agent removed from the session.
    pub agent_id: AgentId,
}

/// Request to start a side-agent conversation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StartAgentRequest {
    /// Requester-assigned correlation id, echoed back on accepted/result
    /// events.
    pub query_id: String,
    /// User-style instruction text. Appended to the current
    /// conversation's history as a `User` message before dispatch.
    pub instruction: String,
    /// Requested agent role for this side conversation. Tool-backed
    /// delegate queries default to `senior-engineer`; non-tool queries without
    /// a role keep using the currently selected interactive role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Input stats for the extension-provided instruction, excluding
    /// any private prefix the extension may have added.
    #[serde(default, skip_serializing_if = "ToolUseStats::is_empty")]
    pub input_stats: ToolUseStats,
    /// `ToolCallId` of the tool invocation that triggered this query,
    /// when the extension is implementing a tool whose live progress
    /// the harness should attribute back to that call. Used by the
    /// `agent_start` tool: the harness emits [`DelegateProgress`] under
    /// this id as the side conversation runs. Optional — non-tool
    /// extensions issuing queries leave it `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    /// Human-readable name for the delegated task, surfaced in the
    /// UI alongside [`DelegateProgress`]. Optional for the same reason
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
}

/// Provider extension snapshot of its currently available models.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelsUpdated {
    /// Complete replacement snapshot for the sending extension. Publishing an
    /// empty list means the extension currently serves no models.
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
    /// Hidden control text that still belongs in model context.
    Internal,
}

impl PromptMessageClass {
    /// Returns true for prompt text that should be hidden from user-facing UI
    /// and latest-user-prompt metadata.
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
/// the prompt is accepted; UIs and other extensions filter on
/// `originator.is_user()` to avoid rendering side conversations as
/// real user turns.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiPromptSubmitted {
    pub session_id: SessionId,
    pub text: String,
    /// Agent that should receive this prompt. Agent creation is explicit via
    /// [`UiCreateAgent`]; prompt submissions only target existing agents.
    pub agent_id: AgentId,
    /// Whether this prompt text is user-authored or hidden internal control
    /// text.
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
/// Always transient — never persisted to a semantic event log and never folded
/// into an agent transcript. Subscribers use it to detect
/// "user is alive" without polling: e.g. std-notifications resets
/// its idle deadline on every draft event so the desktop notification
/// doesn't fire while the user is mid-sentence.
///
/// Future consumers might use the text for autocomplete, draft
/// restoration on UI reconnect, or in-progress prompt sync across
/// multiple attached UIs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiPromptDraft {
    pub session_id: SessionId,
    pub text: String,
}

/// The UI terminal focus state changed. Emitted when the terminal supports
/// focus-in/focus-out reporting and the user moves focus into or away from the
/// Tau terminal window.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiFocusChanged {
    /// Session whose attached UI observed the focus change.
    pub session_id: SessionId,
    /// Whether the terminal reported focus gained (`true`) or lost (`false`).
    pub focused: bool,
}

/// The UI is detaching and wants the daemon to stay alive after it
/// leaves, so a later `tau --attach` can pick up the same
/// session. The harness flips its `exit_on_disconnect` flag to
/// `false` on receipt.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiDetachRequest {}

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

/// The UI requests creation of a durable agent and may include the first prompt
/// that should be submitted to it. This is the explicit boundary between
/// pre-agent UI state (role/cwd can still change freely) and durable agent
/// state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiCreateAgent {
    /// Session in which the agent should be loaded.
    pub session_id: SessionId,
    /// Role to bind to the new durable agent.
    pub role: String,
    /// Initial metadata facts to publish for the new agent.
    ///
    /// The harness fills in the newly-created agent id when publishing these
    /// as durable `agent.metadata_set` events.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metadata: Vec<AgentInitialMetadata>,
    /// Optional first prompt to append after agent context has been loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<String>,
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

/// UI request to set a durable agent display name.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiSetAgentDisplayName {
    /// Session in which the target agent must be loaded or known.
    pub session_id: SessionId,
    /// Agent whose display name should be changed.
    pub agent_id: AgentId,
    /// New human-friendly display name.
    pub display_name: String,
}

/// The user typed `/tree`: render an agent's branching tree (one
/// `harness.notice` line per node) to the chat output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiTreeRequest {
    pub session_id: SessionId,
    /// Target agent tree to render. `None` leaves selection to the harness's
    /// current/default conversation state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent_id: Option<AgentId>,
}

/// The user typed `/tree <id>`: move an agent's head pointer to the given node,
/// so the next prompt branches off there.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiNavigateTree {
    pub session_id: SessionId,
    /// Target agent tree to navigate. `None` leaves selection to the harness's
    /// current/default conversation state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent_id: Option<AgentId>,
    pub node_id: u64,
}

/// The user typed `/compact`: force a provider-side compaction pass on
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
/// Originally tied to the user typing `/cancel`, now also published
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
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellCommandProgress {
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
/// The extension echoes `command`, `session_id`, and
/// `include_in_context` back from the originating `UiShellCommand`
/// so the harness can act on the finished event without bookkeeping
/// a per-command_id map.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellCommandFinished {
    pub command_id: crate::ShellCommandId,
    pub session_id: SessionId,
    pub command: String,
    pub include_in_context: bool,
    /// Target agent for this user-authored shell command. `None` means no
    /// explicit target; the harness uses its default conversation state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent_id: Option<AgentId>,
    /// Interleaved stdout + stderr (truncated), the same shape the
    /// `shell` tool returns.
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
/// The UI base64-encodes `value` and emits the appropriate escape
/// sequence form (plain, or `\x1bPtmux;...\x1b\\` wrapped when running
/// inside `tmux`). Components without access to a terminal — or
/// running through a UI that ignores the event — are no-ops.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Osc1337SetUserVar {
    /// User-variable name. Must be printable ASCII without `=` or
    /// control characters; the UI does not validate this and passes
    /// it through verbatim.
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
    /// Prompt text.
    pub text: String,
    /// Whether this prompt text is user-authored or hidden internal control
    /// text.
    #[serde(default)]
    pub message_class: PromptMessageClass,
    /// Who initiated the prompt.
    #[serde(default)]
    pub originator: PromptOriginator,
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

/// A durable provider-visible manual compaction trigger was inserted into
/// an agent transcript.
///
/// This records the user-facing fact that compaction was requested. It is not
/// a lifecycle/status event: providers translate the folded
/// [`ContextItem::CompactionTrigger`] into their server-side compaction
/// mechanism during normal prompt handling.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentCompactionTriggered {
    /// Agent transcript receiving the compaction trigger.
    pub agent_id: AgentId,
    /// Who requested the trigger.
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// A previously queued user prompt that the harness folded into the
/// in-flight turn as a steering message — appended to the next
/// `AgentPromptCreated` for this agent alongside tool results, rather
/// than waiting for the agent to return to `Idle` and kicking off a fresh turn.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentPromptSteered {
    /// Agent whose in-flight turn received the prompt.
    pub agent_id: AgentId,
    pub text: String,
    /// Whether this prompt text is user-authored or hidden internal control
    /// text.
    #[serde(default)]
    pub message_class: PromptMessageClass,
}

/// A synthetic user message injected into an agent transcript by the harness
/// (not authored by the human user directly). Sources include shell command
/// output and eager AGENTS.md context preambles.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentUserMessageInjected {
    /// Agent transcript receiving the injected message.
    pub agent_id: AgentId,
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
    /// The user requested a fresh session via `/session new`.
    New,
    /// The user resumed an existing session by id.
    Resume,
}

/// The harness created or switched to a session. Extensions that
/// subscribe react by performing per-session setup (e.g. discovering
/// AGENTS.md) and signal completion with `ExtensionContextReady`.
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

/// The harness persisted a normal assistant-generation prompt and
/// assigned it an ID.
///
/// Carries the assembled conversation context for the provider's normal
/// response path.
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
// Provider execution events — facts from the provider backend
// ---------------------------------------------------------------------------

/// The provider accepted a prompt and began processing it.
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

/// The provider has new accumulated response output for a prompt.
///
/// Each update is a replace-style ordered item snapshot, not a delta. Consumers
/// should render `items` in order; completed entries are still transient until
/// [`ProviderResponseFinished`] commits final `output_items`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderResponseUpdated {
    /// Prompt id whose accumulated response changed.
    pub agent_prompt_id: AgentPromptId,
    /// Ordered live provider output items. Completed entries are stable within
    /// the live snapshot but are not durable transcript facts until the
    /// matching [`ProviderResponseFinished`] commits final `output_items`.
    pub items: Vec<ProviderResponseItem>,
    /// Input-token count of the conversation before provider-side compaction,
    /// if this update is reporting a compaction lifecycle item and the harness
    /// knows the previous context size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_original_input_tokens: Option<u64>,
    /// Prompt/input-token count of the compacted provider item in this live
    /// update, if the provider has already completed the compaction item and
    /// the harness can estimate its replay size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_compacted_input_tokens: Option<u64>,
    /// Echo of [`AgentPromptCreated::originator`]. UIs filter on
    /// `originator.is_user()` so the streaming text from a side
    /// conversation doesn't paint into the user's chat window.
    #[serde(default)]
    pub originator: PromptOriginator,
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
}

impl ProviderStopReason {
    #[must_use]
    pub const fn requests_tool_calls(self) -> bool {
        matches!(self, Self::ToolCalls)
    }
}

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
    /// Echo of [`AgentPromptCreated::originator`]. The provider must
    /// copy this from the prompt; the harness routes the response
    /// based on it.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Provider-reported usage for this response, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ProviderTokenUsage>,
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
    /// only for Responses-backend turns where the WS path was
    /// attempted (i.e. `cfg.supports_websocket` and the routing-key
    /// sticky-disable flag was off). `None` for Chat Completions and
    /// for Responses routing keys that have been permanently flipped to
    /// HTTP+SSE. Lets offline analysis attribute a low
    /// `cached_tokens` to a chain-strip event (the Codex chain cache
    /// is connection-local; a fresh socket or a silent reconnect
    /// drops the in-request `previous_response_id`, collapsing
    /// `cached_tokens` to the static system+tools baseline).
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
    /// Default — covers Chat Completions and the HTTP+SSE Responses
    /// fallback.
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
    #[serde(rename = "tool.result")]
    ToolResult(ToolResult),
    #[serde(rename = "tool.error")]
    ToolError(ToolError),
    #[serde(rename = "tool.background_result")]
    ToolBackgroundResult(ToolBackgroundResult),
    #[serde(rename = "tool.background_error")]
    ToolBackgroundError(ToolBackgroundError),
    #[serde(rename = "tool.progress")]
    ToolProgress(ToolProgress),
    #[serde(rename = "tool.cancel_request")]
    ToolCancelRequest(ToolCancelRequest),
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

    // Extension supervision
    #[serde(rename = "extension.starting")]
    ExtensionStarting(ExtensionStarting),
    #[serde(rename = "extension.ready")]
    ExtensionReady(ExtensionReady),
    #[serde(rename = "extension.exited")]
    ExtensionExited(ExtensionExited),
    #[serde(rename = "extension.restarting")]
    ExtensionRestarting(ExtensionRestarting),
    #[serde(rename = "extension.skill_available")]
    ExtSkillAvailable(ExtSkillAvailable),
    #[serde(rename = "extension.agents_md_available")]
    ExtAgentsMdAvailable(ExtAgentsMdAvailable),
    #[serde(rename = "extension.context_provider_register")]
    ExtensionContextProviderRegister(ExtensionContextProviderRegister),
    #[serde(rename = "extension.context_ready")]
    ExtensionContextReady(ExtensionContextReady),
    #[serde(rename = "extension.agent_context_publish")]
    ExtAgentContextPublish(ExtAgentContextPublish),
    #[serde(rename = "extension.prompt_fragment_publish")]
    ExtPromptFragmentPublish(ExtPromptFragmentPublish),
    #[serde(rename = "extension.prompt_submit_request")]
    ExtPromptSubmitRequest(ExtPromptSubmitRequest),
    #[serde(rename = "agent.start_request")]
    StartAgentRequest(StartAgentRequest),
    #[serde(rename = "agent.start_accepted")]
    StartAgentAccepted(StartAgentAccepted),
    #[serde(rename = "agent.start_result")]
    StartAgentResult(StartAgentResult),
    #[serde(rename = "agent.message_sent")]
    AgentMessageSent(AgentMessageSent),
    #[serde(rename = "agent.message_received")]
    AgentMessageReceived(AgentMessageReceived),
    #[serde(rename = "extension.event")]
    ExtensionEvent(CustomEvent),
    #[serde(rename = "provider.models_updated")]
    ProviderModelsUpdated(ProviderModelsUpdated),
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
    #[serde(rename = "harness.role_selected")]
    HarnessRoleSelected(HarnessRoleSelected),
    #[serde(rename = "harness.context_usage_changed")]
    HarnessContextUsageChanged(HarnessContextUsageChanged),
    #[serde(rename = "harness.agent_context_usage_changed")]
    HarnessAgentContextUsageChanged(HarnessAgentContextUsageChanged),
    #[serde(rename = "agent.state")]
    AgentState(AgentStateChanged),
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
    #[serde(rename = "ui.detach_request")]
    UiDetachRequest(UiDetachRequest),
    #[serde(rename = "ui.shell_command")]
    UiShellCommand(UiShellCommand),
    #[serde(rename = "ui.switch_session")]
    UiSwitchSession(UiSwitchSession),
    #[serde(rename = "ui.create_agent")]
    UiCreateAgent(UiCreateAgent),
    #[serde(rename = "ui.tree_request")]
    UiTreeRequest(UiTreeRequest),
    #[serde(rename = "ui.navigate_tree")]
    UiNavigateTree(UiNavigateTree),
    #[serde(rename = "ui.compact_request")]
    UiCompactRequest(UiCompactRequest),
    #[serde(rename = "ui.cancel_prompt")]
    UiCancelPrompt(UiCancelPrompt),
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
    #[serde(rename = "shell.command_progress")]
    ShellCommandProgress(ShellCommandProgress),
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
    #[serde(rename = "agent.prompt_created")]
    AgentPromptCreated(AgentPromptCreated),
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
    #[serde(rename = "agent.display_name_set")]
    AgentDisplayNameSet(AgentDisplayNameSet),
    #[serde(rename = "agent.metadata_set")]
    AgentMetadataSet(AgentMetadataSet),
    #[serde(rename = "agent.metadata_unset")]
    AgentMetadataUnset(AgentMetadataUnset),

    // Session lifecycle/membership
    #[serde(rename = "session.started")]
    SessionStarted(SessionStarted),
    #[serde(rename = "session.shutdown")]
    SessionShutdown(SessionShutdown),
    #[serde(rename = "session.agent_loaded")]
    SessionAgentLoaded(SessionAgentLoaded),
    #[serde(rename = "session.agent_unloaded")]
    SessionAgentUnloaded(SessionAgentUnloaded),

    // Provider execution
    #[serde(rename = "provider.prompt_submitted")]
    ProviderPromptSubmitted(ProviderPromptSubmitted),
    #[serde(rename = "provider.response_updated")]
    ProviderResponseUpdated(ProviderResponseUpdated),
    #[serde(rename = "provider.response_finished")]
    ProviderResponseFinished(ProviderResponseFinished),
    #[serde(rename = "provider.cache_miss_diagnostic")]
    ProviderCacheMissDiagnostic(ProviderCacheMissDiagnostic),
}

impl Event {
    /// Returns the dotted event name carried by this envelope.
    #[must_use]
    pub fn name(&self) -> EventName {
        match self {
            Self::ToolRegister(_) => EventName::TOOL_REGISTER,
            Self::ToolUnregister(_) => EventName::TOOL_UNREGISTER,
            Self::ToolRequest(_) => EventName::TOOL_REQUEST,
            Self::ToolStarted(_) => EventName::TOOL_STARTED,
            Self::ToolRejected(_) => EventName::TOOL_REJECTED,
            Self::ToolResult(_) => EventName::TOOL_RESULT,
            Self::ToolError(_) => EventName::TOOL_ERROR,
            Self::ToolBackgroundResult(_) => EventName::TOOL_BACKGROUND_RESULT,
            Self::ToolBackgroundError(_) => EventName::TOOL_BACKGROUND_ERROR,
            Self::ToolProgress(_) => EventName::TOOL_PROGRESS,
            Self::ToolCancelRequest(_) => EventName::TOOL_CANCEL_REQUEST,
            Self::ToolCancelled(_) => EventName::TOOL_CANCELLED,
            Self::ToolDelegateProgress(_) => EventName::TOOL_DELEGATE_PROGRESS,
            Self::ActionSchemaPublished(_) => EventName::ACTION_SCHEMA_PUBLISHED,
            Self::ActionInvoke(_) => EventName::ACTION_INVOKE,
            Self::ActionResult(_) => EventName::ACTION_RESULT,
            Self::ActionError(_) => EventName::ACTION_ERROR,
            Self::ExtensionStarting(_) => EventName::EXTENSION_STARTING,
            Self::ExtensionReady(_) => EventName::EXTENSION_READY,
            Self::ExtensionExited(_) => EventName::EXTENSION_EXITED,
            Self::ExtensionRestarting(_) => EventName::EXTENSION_RESTARTING,
            Self::ExtSkillAvailable(_) => EventName::EXTENSION_SKILL_AVAILABLE,
            Self::ExtAgentsMdAvailable(_) => EventName::EXTENSION_AGENTS_MD_AVAILABLE,
            Self::ExtensionContextProviderRegister(_) => {
                EventName::EXTENSION_CONTEXT_PROVIDER_REGISTER
            }
            Self::ExtensionContextReady(_) => EventName::EXTENSION_CONTEXT_READY,
            Self::ExtAgentContextPublish(_) => EventName::EXTENSION_AGENT_CONTEXT_PUBLISH,
            Self::ExtPromptFragmentPublish(_) => EventName::EXTENSION_PROMPT_FRAGMENT_PUBLISH,
            Self::ExtPromptSubmitRequest(_) => EventName::EXTENSION_PROMPT_SUBMIT_REQUEST,
            Self::StartAgentRequest(_) => EventName::AGENT_START_REQUEST,
            Self::StartAgentAccepted(_) => EventName::AGENT_START_ACCEPTED,
            Self::StartAgentResult(_) => EventName::AGENT_START_RESULT,
            Self::AgentMessageSent(_) => EventName::AGENT_MESSAGE_SENT,
            Self::AgentMessageReceived(_) => EventName::AGENT_MESSAGE_RECEIVED,
            Self::ExtensionEvent(event) => event.name().clone(),
            Self::ProviderModelsUpdated(_) => EventName::PROVIDER_MODELS_UPDATED,
            Self::ProviderToolResult(_) => EventName::PROVIDER_TOOL_RESULT,
            Self::ProviderToolError(_) => EventName::PROVIDER_TOOL_ERROR,
            Self::HarnessNotice(_) => EventName::HARNESS_NOTICE,
            Self::HarnessSessionDir(_) => EventName::HARNESS_SESSION_DIR,
            Self::HarnessUiDir(_) => EventName::HARNESS_UI_DIR,
            Self::HarnessModelsAvailable(_) => EventName::HARNESS_MODELS_AVAILABLE,
            Self::HarnessRolesAvailable(_) => EventName::HARNESS_ROLES_AVAILABLE,
            Self::HarnessRoleSelected(_) => EventName::HARNESS_ROLE_SELECTED,
            Self::HarnessContextUsageChanged(_) => EventName::HARNESS_CONTEXT_USAGE_CHANGED,
            Self::HarnessAgentContextUsageChanged(_) => {
                EventName::HARNESS_AGENT_CONTEXT_USAGE_CHANGED
            }
            Self::AgentState(_) => EventName::AGENT_STATE,
            Self::HarnessEffortsAvailable(_) => EventName::HARNESS_EFFORTS_AVAILABLE,
            Self::HarnessVerbositiesAvailable(_) => EventName::HARNESS_VERBOSITIES_AVAILABLE,
            Self::HarnessThinkingSummariesAvailable(_) => {
                EventName::HARNESS_THINKING_SUMMARIES_AVAILABLE
            }
            Self::UiPromptSubmitted(_) => EventName::UI_PROMPT_SUBMITTED,
            Self::UiPromptDraft(_) => EventName::UI_PROMPT_DRAFT,
            Self::UiFocusChanged(_) => EventName::UI_FOCUS_CHANGED,
            Self::UiRoleSelect(_) => EventName::UI_ROLE_SELECT,
            Self::UiAgentModelSelect(_) => EventName::UI_AGENT_MODEL_SELECT,
            Self::UiRoleUpdate(_) => EventName::UI_ROLE_UPDATE,
            Self::UiDetachRequest(_) => EventName::UI_DETACH_REQUEST,
            Self::UiShellCommand(_) => EventName::UI_SHELL_COMMAND,
            Self::UiSwitchSession(_) => EventName::UI_SWITCH_SESSION,
            Self::UiCreateAgent(_) => EventName::UI_CREATE_AGENT,
            Self::UiTreeRequest(_) => EventName::UI_TREE_REQUEST,
            Self::UiNavigateTree(_) => EventName::UI_NAVIGATE_TREE,
            Self::UiCompactRequest(_) => EventName::UI_COMPACT_REQUEST,
            Self::UiCancelPrompt(_) => EventName::UI_CANCEL_PROMPT,
            Self::UiRecallQueuedPrompt(_) => EventName::UI_RECALL_QUEUED_PROMPT,
            Self::UiSetAgentDisplayName(_) => EventName::UI_SET_AGENT_DISPLAY_NAME,
            Self::Osc1337SetUserVar(_) => EventName::TERM_OSC1337_SET_USER_VAR,
            Self::TermBell(_) => EventName::TERM_BELL,
            Self::ShellCommandProgress(_) => EventName::SHELL_COMMAND_PROGRESS,
            Self::ShellCommandFinished(_) => EventName::SHELL_COMMAND_FINISHED,
            Self::AgentPromptSubmitted(_) => EventName::AGENT_PROMPT_SUBMITTED,
            Self::AgentPromptQueued(_) => EventName::AGENT_PROMPT_QUEUED,
            Self::AgentPromptRecalled(_) => EventName::AGENT_PROMPT_RECALLED,
            Self::AgentPromptSteered(_) => EventName::AGENT_PROMPT_STEERED,
            Self::AgentCompactionTriggered(_) => EventName::AGENT_COMPACTION_TRIGGERED,
            Self::AgentStarted(_) => EventName::AGENT_STARTED,
            Self::AgentDisplayNameSet(_) => EventName::AGENT_DISPLAY_NAME_SET,
            Self::AgentMetadataSet(_) => EventName::AGENT_METADATA_SET,
            Self::AgentMetadataUnset(_) => EventName::AGENT_METADATA_UNSET,
            Self::SessionStarted(_) => EventName::SESSION_STARTED,
            Self::SessionShutdown(_) => EventName::SESSION_SHUTDOWN,
            Self::SessionAgentLoaded(_) => EventName::SESSION_AGENT_LOADED,
            Self::SessionAgentUnloaded(_) => EventName::SESSION_AGENT_UNLOADED,
            Self::AgentPromptCreated(_) => EventName::AGENT_PROMPT_CREATED,
            Self::AgentPromptTerminated(_) => EventName::AGENT_PROMPT_TERMINATED,
            Self::AgentPromptPrewarmRequested(_) => EventName::AGENT_PROMPT_PREWARM_REQUESTED,
            Self::AgentUserMessageInjected(_) => EventName::AGENT_USER_MESSAGE_INJECTED,
            Self::AgentHeadMoved(_) => EventName::AGENT_HEAD_MOVED,
            Self::ProviderPromptSubmitted(_) => EventName::PROVIDER_PROMPT_SUBMITTED,
            Self::ProviderResponseUpdated(_) => EventName::PROVIDER_RESPONSE_UPDATED,
            Self::ProviderResponseFinished(_) => EventName::PROVIDER_RESPONSE_FINISHED,
            Self::ProviderCacheMissDiagnostic(_) => EventName::PROVIDER_CACHE_MISS_DIAGNOSTIC,
        }
    }

    /// Returns true for protocol events that are runtime-only by default when
    /// sent directly without an explicit [`crate::Emit`] durability override.
    #[must_use]
    pub const fn defaults_to_transient(&self) -> bool {
        matches!(
            self,
            Self::ToolCancelled(_)
                | Self::ProviderResponseUpdated(_)
                | Self::ProviderPromptSubmitted(_)
                | Self::ToolProgress(_)
                | Self::ToolDelegateProgress(_)
                | Self::ActionSchemaPublished(_)
                | Self::ActionInvoke(_)
                | Self::ActionResult(_)
                | Self::ActionError(_)
                | Self::ShellCommandProgress(_)
                | Self::UiPromptSubmitted(_)
                | Self::AgentPromptQueued(_)
                | Self::AgentPromptRecalled(_)
                | Self::AgentPromptCreated(_)
                | Self::AgentPromptTerminated(_)
                | Self::AgentPromptPrewarmRequested(_)
                | Self::AgentState(_)
                | Self::UiCompactRequest(_)
                | Self::UiCreateAgent(_)
                | Self::UiPromptDraft(_)
                | Self::UiFocusChanged(_)
                | Self::UiSetAgentDisplayName(_)
        )
    }
}
