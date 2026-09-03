//! User settings loaded from `~/.config/tau/` with `.d/` directory
//! overrides. Primary config files:
//!
//! - `cli.yaml` — CLI display preferences
//! - `harness.yaml` — harness settings, extensions, and roles
//!
//! Uses the `config` crate for layered YAML loading.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::num::{NonZeroU8, NonZeroU16, NonZeroU64};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};
use std::{fmt, io as path_std_io};

use indexmap::IndexMap;
use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use tau_proto::{
    ModelId, ModelName, ModelTag, PromptContent, PromptPriority, ProviderName, ToolName, ToolTag,
};

use crate::web_tools::*;

// ---------------------------------------------------------------------------
// Built-in configuration resources
//
// Tau ships its baseline `cli.yaml`, `cli-bindings.yaml` and
// `harness.yaml` as ordinary source files under
// `crates/tau-config/config/`, embedded via `include_str!`. They are layered
// underneath user files, with a small role-merge pass for role metadata whose
// semantics differ from generic YAML array replacement.
// ---------------------------------------------------------------------------

const BUILT_IN_CLI_YAML: &str = include_str!("../config/built-in.cli.yaml");
const BUILT_IN_CLI_BINDINGS_YAML: &str = include_str!("../config/built-in.cli-bindings.yaml");

/// Built-in lower effective bound for activating-input `wait` tool calls.
pub const DEFAULT_WAIT_TIMEOUT_MINIMUM_MINUTES: u64 = 1;
/// Built-in upper effective bound for activating-input `wait` tool calls.
pub const DEFAULT_WAIT_TIMEOUT_MAXIMUM_MINUTES: u64 = 1_440;
const BUILT_IN_HARNESS_YAML: &str = include_str!("../config/built-in.harness.yaml");
/// Default model-visible instruction for a context-size alert.
const DEFAULT_CONTEXT_SIZE_ALERT_MESSAGE: &str =
    "Use the `compact` tool after finishing your current task.";

/// One positive whole-minute activating-input wait timeout.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WaitTimeoutMinutes(NonZeroU16);

impl WaitTimeoutMinutes {
    /// Creates a positive timeout representable by persisted wait metadata.
    #[must_use]
    pub const fn new(minutes: u16) -> Option<Self> {
        match NonZeroU16::new(minutes) {
            Some(minutes) => Some(Self(minutes)),
            None => None,
        }
    }

    /// Returns the timeout as whole minutes.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get() as u64
    }

    /// Returns the timeout as a scheduling duration.
    #[must_use]
    pub const fn duration(self) -> Duration {
        Duration::from_secs(self.get() * 60)
    }
}

/// Validated inclusive bounds for activating-input wait timeouts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaitTimeoutBounds {
    /// Smallest effective activating-input wait timeout.
    minimum: WaitTimeoutMinutes,
    /// Largest effective activating-input wait timeout.
    maximum: WaitTimeoutMinutes,
}

impl WaitTimeoutBounds {
    /// Creates ordered inclusive timeout bounds.
    #[must_use]
    pub const fn new(minimum: WaitTimeoutMinutes, maximum: WaitTimeoutMinutes) -> Option<Self> {
        if minimum.get() <= maximum.get() {
            Some(Self { minimum, maximum })
        } else {
            None
        }
    }

    /// Returns the configured minimum timeout.
    #[must_use]
    pub const fn minimum(self) -> WaitTimeoutMinutes {
        self.minimum
    }

    /// Returns the configured maximum timeout.
    #[must_use]
    pub const fn maximum(self) -> WaitTimeoutMinutes {
        self.maximum
    }

    /// Returns the configured minimum as a scheduling duration.
    #[must_use]
    pub const fn minimum_duration(self) -> Duration {
        self.minimum.duration()
    }

    /// Returns the configured maximum as a scheduling duration.
    #[must_use]
    pub const fn maximum_duration(self) -> Duration {
        self.maximum.duration()
    }

    /// Clamps a positive requested minute count to this effective policy.
    #[must_use]
    pub fn clamp(self, requested_minutes: u64) -> WaitTimeoutMinutes {
        let minutes = requested_minutes.clamp(self.minimum.get(), self.maximum.get());
        WaitTimeoutMinutes(NonZeroU16::new(minutes as u16).expect("validated wait bounds"))
    }

    /// Clamps a positive wide integer from decoded tool input to this policy.
    #[must_use]
    pub fn clamp_integer(self, requested_minutes: i128) -> WaitTimeoutMinutes {
        let minutes = requested_minutes.clamp(
            i128::from(self.minimum.0.get()),
            i128::from(self.maximum.0.get()),
        );
        WaitTimeoutMinutes(
            NonZeroU16::new(minutes as u16).expect("validated positive decoded wait timeout"),
        )
    }

    fn from_raw(minimum: u64, maximum: u64) -> Result<Self, String> {
        if minimum < 1 {
            return Err("wait_timeout_minimum_minutes must be at least 1".to_owned());
        }
        if maximum < 1 {
            return Err("wait_timeout_maximum_minutes must be at least 1".to_owned());
        }
        if u64::from(u16::MAX) < maximum {
            return Err(format!(
                "wait_timeout_maximum_minutes must not exceed {}",
                u16::MAX
            ));
        }
        if maximum < minimum {
            return Err(
                "wait_timeout_minimum_minutes must not exceed wait_timeout_maximum_minutes"
                    .to_owned(),
            );
        }
        let minimum = WaitTimeoutMinutes(
            NonZeroU16::new(minimum as u16).expect("validated positive bounded wait minimum"),
        );
        let maximum = WaitTimeoutMinutes(
            NonZeroU16::new(maximum as u16).expect("validated positive bounded wait maximum"),
        );
        Ok(Self { minimum, maximum })
    }

    /// Returns the built-in effective timeout policy.
    #[must_use]
    pub fn built_in() -> Self {
        Self::from_raw(
            DEFAULT_WAIT_TIMEOUT_MINIMUM_MINUTES,
            DEFAULT_WAIT_TIMEOUT_MAXIMUM_MINUTES,
        )
        .expect("built-in wait timeout bounds are valid")
    }
}

fn parse_built_in_yaml<T: for<'de> Deserialize<'de>>(name: &str, text: &str) -> T {
    serde_yaml_ng::from_str(text).unwrap_or_else(|err| {
        panic!("tau ships with malformed {name}: {err}\nthis is a bug; please report it")
    })
}

// ---------------------------------------------------------------------------
// Extension environment input
// ---------------------------------------------------------------------------

/// Supported environment variable for additively enabling configured
/// extensions.
pub const TAU_ENABLE_EXTENSIONS_ENV: &str = "TAU_ENABLE_EXTENSIONS";
/// Emergency process-wide override for supervised extension Tau-state access.
pub const TAU_EXTENSION_TAU_STATE_ACCESS_ENV: &str = "TAU_EXTENSION_TAU_STATE_ACCESS";

/// Visibility of Tau's state root in a supervised extension mount namespace.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TauStateAccess {
    /// Present an empty Tau-state tree, restoring only approved owned paths.
    Hidden,
    /// Present Tau state read-only, restoring only approved owned writable
    /// paths.
    #[default]
    ReadOnly,
    /// Retain the historical ambient Tau-state view except for secrets.
    Legacy,
}

/// Visibility of Tau harness runtime sockets in a supervised extension mount
/// namespace.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TauRuntimeSocketAccess {
    /// Present an empty read-only harness runtime directory.
    #[default]
    Hidden,
    /// Retain the historical ambient harness runtime directory view.
    Legacy,
}

impl std::fmt::Display for TauStateAccess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Hidden => "hidden",
            Self::ReadOnly => "read_only",
            Self::Legacy => "legacy",
        })
    }
}

/// Failure while parsing [`TAU_EXTENSION_TAU_STATE_ACCESS_ENV`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TauStateAccessEnvError(
    /// Human-readable exact-token validation failure.
    String,
);

impl fmt::Display for TauStateAccessEnvError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for TauStateAccessEnvError {}

/// Parses the exact emergency Tau-state access override token.
pub fn parse_tau_state_access_env(
    value: Option<OsString>,
) -> Result<Option<TauStateAccess>, TauStateAccessEnvError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.into_string().map_err(|_| {
        TauStateAccessEnvError(format!(
            "{TAU_EXTENSION_TAU_STATE_ACCESS_ENV} must be valid UTF-8 and exactly hidden, read_only, or legacy"
        ))
    })?;
    match value.as_str() {
        "hidden" => Ok(Some(TauStateAccess::Hidden)),
        "read_only" => Ok(Some(TauStateAccess::ReadOnly)),
        "legacy" => Ok(Some(TauStateAccess::Legacy)),
        _ => Err(TauStateAccessEnvError(format!(
            "{TAU_EXTENSION_TAU_STATE_ACCESS_ENV} must be exactly hidden, read_only, or legacy"
        ))),
    }
}

/// Failure while parsing [`TAU_ENABLE_EXTENSIONS_ENV`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnableExtensionsEnvError(String);

impl fmt::Display for EnableExtensionsEnvError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for EnableExtensionsEnvError {}

/// Parses `TAU_ENABLE_EXTENSIONS` as a strict comma-separated list of exact
/// names.
///
/// ASCII space and tab around names are ignored. Empty values are a no-op and
/// duplicate names are retained only at their first occurrence.
///
/// # Errors
///
/// Returns a source-specific diagnostic for non-UTF-8 input, empty items, or
/// names outside Tau's exact extension-name grammar.
pub fn parse_enable_extensions_env(
    value: Option<OsString>,
) -> Result<Vec<String>, EnableExtensionsEnvError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let value = value.into_string().map_err(|_| {
        EnableExtensionsEnvError(format!(
            "{TAU_ENABLE_EXTENSIONS_ENV} must be valid UTF-8 and contain NAME[,NAME...]"
        ))
    })?;
    let value = value.trim_matches([' ', '\t']);
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for (index, item) in value.split(',').enumerate() {
        let name = item.trim_matches([' ', '\t']);
        if name.is_empty() {
            return Err(EnableExtensionsEnvError(format!(
                "{TAU_ENABLE_EXTENSIONS_ENV} item {} is empty; expected NAME[,NAME...]",
                index + 1
            )));
        }
        validate_extension_name(name).map_err(|_| {
            EnableExtensionsEnvError(format!(
                "{TAU_ENABLE_EXTENSIONS_ENV} item {} is invalid; names may contain only ASCII letters, digits, '_' and '-'",
                index + 1
            ))
        })?;
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

// ---------------------------------------------------------------------------
// CLI settings
// ---------------------------------------------------------------------------

/// CLI display settings loaded from `cli.yaml`.
///
/// Has no `Default` impl on purpose — the baseline lives in
/// `config/built-in.cli.yaml` and is layered in by the loader. Use
/// [`CliSettings::built_in`] when you need a fresh, populated value
/// in a test or fallback.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliSettings {
    /// Show a greeting message on startup.
    pub greeting: bool,
    /// Show the tau ASCII logo on startup.
    pub show_logo: bool,
    /// Use a bar-shaped cursor in the CLI. When false, use a steady
    /// block cursor instead.
    pub bar_cursor: bool,
    /// Whether the CLI UI responds to mouse activity.
    ///
    /// This static `cli.yaml` setting defaults to true. When false, the CLI
    /// disables terminal mouse reporting while it owns the terminal, so the
    /// terminal handles wheel scrolling, selection, and links natively. It is
    /// not a runtime `:set` option because terminal feature ownership is
    /// acquired when the CLI starts.
    pub mouse: bool,
    /// Whether prompt-draft liveness events include the current prompt buffer.
    ///
    /// This static `cli.yaml` setting defaults to false. It is intentionally
    /// not a runtime `:set` option because prompt drafts can reach live
    /// subscribers and best-effort diagnostic logs.
    pub send_prompt_draft_content: bool,
    /// Symbol shown before the input prompt and queued prompts.
    pub prompt_symbol: String,
    /// Symbol shown before submitted prompts in the transcript.
    pub submitted_prompt_symbol: String,
    /// Whether to render file-mutation diffs in their full expanded
    /// form by default.
    pub show_diff: bool,
    /// Whether to render the agent's reasoning summary by default.
    pub show_thinking: bool,
    /// Whether to render per-turn token usage stats by default.
    pub show_turn_stats: bool,
    /// Whether to render the full-redraw debug counter in the model
    /// status bar by default.
    pub redraw_counter: bool,
    /// Maximum number of rendered history lines to replay to the terminal on a
    /// full redraw by default.
    pub redraw_history_size: usize,
    /// Whether Markdown links use clickable OSC 8 terminal hyperlinks.
    pub osc8_links: bool,
    /// Whether to render UI↔harness socket throughput in the model
    /// status bar by default.
    pub show_ui_io: bool,
    /// How tool calls are rendered in the transcript by default.
    pub show_tools: ShowTools,
    /// How inter-agent and user-agent messages are rendered in the transcript.
    pub show_messages: ShowMessages,
    /// Whether to render harness-internal prompt facts in the transcript.
    pub show_internal_prompts: bool,
    /// Default verbose-mode visibility threshold for diagnostic notices.
    pub notice_level: tau_proto::NoticeLevel,
    /// Deprecated compatibility setting for old routine status visibility.
    pub show_status: ShowStatus,
    /// Whether to show a compact indicator when the prompt input is locally
    /// scrolled.
    pub show_prompt_scroll_indicator: bool,
    /// Which terminal color theme selector to use.
    pub theme: CliTheme,
    /// Prompt-text completion rules keyed by word prefix. Values name
    /// the completer to run, optionally followed by completer arguments.
    /// Intrinsic first-non-whitespace colon command mode takes precedence.
    #[serde(default)]
    pub completions: HashMap<String, String>,
    /// Key bindings for prompt-local actions. Defaults to an
    /// empty map at the serde layer; the loader merges
    /// `built-in.cli-bindings.yaml` underneath the user's bindings.
    #[serde(default)]
    pub bind: HashMap<String, CliBindingAction>,
}

impl CliSettings {
    /// The fully-populated baseline that ships with tau, parsed from
    /// the embedded `built-in.cli.yaml` plus `built-in.cli-bindings.yaml`.
    pub fn built_in() -> Self {
        let mut s: Self = parse_built_in_yaml("built-in.cli.yaml", BUILT_IN_CLI_YAML);
        s.bind = default_cli_bindings();
        s
    }

    /// Return the default runtime UI state derived from static CLI config.
    #[must_use]
    pub fn default_state(&self) -> CliState {
        CliState {
            show_diff: self.show_diff,
            show_thinking: self.show_thinking,
            show_turn_stats: self.show_turn_stats,
            redraw_counter: self.redraw_counter,
            redraw_history_size: self.redraw_history_size,
            show_ui_io: self.show_ui_io,
            show_tools: self.show_tools,
            show_messages: self.show_messages,
            show_internal_prompts: self.show_internal_prompts,
            notice_level: self.notice_level,
            show_status: self.show_status,
            show_prompt_scroll_indicator: self.show_prompt_scroll_indicator,
        }
    }
}

/// CLI key binding action.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CliBindingAction {
    /// Action name, e.g. `submit-prompt`, `insert-newline`,
    /// `shell-prompt-insert`, `shell-prompt-edit`, `fast-toggle`,
    /// `cycle-role`, `cycle-role-group`, `agent-previous`, or `agent-next`.
    pub action: String,
    /// Shell command to execute. `None` for actions that don't shell
    /// out (e.g. `submit-prompt`, `insert-newline`,
    /// `prompt-previous`, `prompt-next`, `fast-toggle`, `cycle-role`,
    /// `cycle-role-group`, `agent-previous`, or `agent-next`).
    pub command: Option<String>,
    /// Whether to trim command stdout before insertion.
    pub trim: bool,
}

impl Default for CliBindingAction {
    fn default() -> Self {
        Self {
            action: "shell-prompt-insert".to_owned(),
            command: None,
            trim: false,
        }
    }
}

/// Parse the embedded `built-in.cli-bindings.yaml`. Called from
/// [`CliSettings::built_in`] and from [`load_cli_settings_in`] (the
/// latter overlays user bindings on top of this baseline so users
/// don't lose unmentioned keys when they customize a single chord).
pub(crate) fn default_cli_bindings() -> HashMap<String, CliBindingAction> {
    parse_built_in_yaml("built-in.cli-bindings.yaml", BUILT_IN_CLI_BINDINGS_YAML)
}

// ---------------------------------------------------------------------------
// CLI runtime state
// ---------------------------------------------------------------------------

/// Mutable CLI state persisted across runs at
/// `<state_dir>/cli.json`. Distinct from `CliSettings` (config) —
/// this file is written by the CLI itself in response to
/// `:set <name> <value>` commands.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct CliState {
    /// Whether to render file-mutation diffs in their full expanded
    /// form (vs the compact `+N/-M` chip). Controlled by
    /// `:set show-diff <true|false>`.
    pub show_diff: bool,
    /// Whether to render the agent's reasoning summary (the
    /// `agent.thinking` block). Controlled by
    /// `:set show-thinking <true|false>`.
    pub show_thinking: bool,
    /// Whether to render per-turn token usage stats below agent
    /// responses. Controlled by `:set show-turn-stats <true|false>`.
    pub show_turn_stats: bool,
    /// Whether to render the full-redraw debug counter in the model
    /// status bar. Controlled by `:set redraw-counter <true|false>`.
    pub redraw_counter: bool,
    /// Maximum number of rendered history lines replayed to the terminal on a
    /// full redraw. Controlled by `:set redraw-history-size <lines>`.
    pub redraw_history_size: usize,
    /// Whether to render UI↔harness socket throughput in the model
    /// status bar. Controlled by `:set show-ui-io <true|false>`.
    pub show_ui_io: bool,
    /// How tool calls are rendered in the transcript. Controlled by
    /// `:set show-tools <off|summarize-turn|summarize-prompt|compact|full>`.
    pub show_tools: ShowTools,
    /// How messages between agents are rendered in the transcript. Controlled
    /// by `:set show-messages <mode>`.
    pub show_messages: ShowMessages,
    /// Whether to render typed harness-internal prompt facts. Controlled by
    /// `:set show-internal-prompts <on|off>`.
    pub show_internal_prompts: bool,
    /// Verbose-mode diagnostic visibility threshold, controlled by
    /// `:set notice-level <critical|warning|info|debug|trace>`.
    pub notice_level: tau_proto::NoticeLevel,
    /// Deprecated compatibility setting for old routine status visibility.
    pub show_status: ShowStatus,
    /// Whether to show a compact indicator when the prompt input has hidden
    /// rows. Controlled by `:set show-prompt-scroll-indicator <true|false>`.
    pub show_prompt_scroll_indicator: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct CliStatePatch {
    show_diff: Option<bool>,
    show_thinking: Option<bool>,
    show_turn_stats: Option<bool>,
    redraw_counter: Option<bool>,
    redraw_history_size: Option<usize>,
    show_ui_io: Option<bool>,
    show_tools: Option<ShowTools>,
    show_messages: Option<ShowMessages>,
    /// Optional persisted diagnostic visibility override for harness prompts.
    show_internal_prompts: Option<bool>,
    notice_level: Option<tau_proto::NoticeLevel>,
    show_status: Option<ShowStatus>,
    show_prompt_scroll_indicator: Option<bool>,
}

impl CliStatePatch {
    fn apply_to(self, mut state: CliState) -> CliState {
        if let Some(value) = self.show_diff {
            state.show_diff = value;
        }
        if let Some(value) = self.show_thinking {
            state.show_thinking = value;
        }
        if let Some(value) = self.show_turn_stats {
            state.show_turn_stats = value;
        }
        if let Some(value) = self.redraw_counter {
            state.redraw_counter = value;
        }
        if let Some(value) = self.redraw_history_size {
            state.redraw_history_size = value;
        }
        if let Some(value) = self.show_ui_io {
            state.show_ui_io = value;
        }
        if let Some(value) = self.show_tools {
            state.show_tools = value;
        }
        if let Some(value) = self.show_messages {
            state.show_messages = value;
        }
        if let Some(value) = self.show_internal_prompts {
            state.show_internal_prompts = value;
        }
        if let Some(value) = self.notice_level {
            state.notice_level = value;
        }
        if let Some(value) = self.show_status {
            state.show_status = value;
        }
        if let Some(value) = self.show_prompt_scroll_indicator {
            state.show_prompt_scroll_indicator = value;
        }
        state
    }
}
/// CLI color theme selection.
///
/// Serialized names are non-empty named built-in or external themes resolved by
/// the CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CliTheme {
    /// Load a named built-in theme such as `tau-plain-dark`, `tau-plain-light`,
    /// or `tau-dpc`, or an external theme from `themes/<name>.json5`.
    Named(String),
}

impl Default for CliTheme {
    /// Returns the conservative terminal-palette-safe built-in.
    fn default() -> Self {
        Self::Named("tau-plain-dark".to_owned())
    }
}

impl CliTheme {
    /// Parses a user-authored theme name from `cli.yaml` or `TAU_THEME`.
    /// Leading and trailing whitespace is ignored, arbitrary non-empty names
    /// become [`CliTheme::Named`], and empty or whitespace-only input returns
    /// `None`.
    #[must_use]
    pub fn parse_name(value: &str) -> Option<Self> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(Self::Named(trimmed.to_owned()))
    }

    /// Returns the selector name used for serialization and diagnostics.
    #[must_use]
    pub fn as_name(&self) -> &str {
        match self {
            Self::Named(name) => name,
        }
    }
}

impl<'de> Deserialize<'de> for CliTheme {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse_name(&value).ok_or_else(|| D::Error::custom("theme name must not be empty"))
    }
}

impl Serialize for CliTheme {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_name())
    }
}

/// How tool calls are rendered in the CLI transcript.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum ShowTools {
    /// Do not render tool calls.
    #[serde(rename = "off")]
    Off,
    /// Summarize tool calls once per agent turn.
    #[serde(rename = "summarize-turn")]
    SummarizeTurn,
    /// Summarize tool calls once per submitted prompt.
    #[serde(rename = "summarize-prompt")]
    SummarizePrompt,
    /// Render compact per-call chips.
    #[serde(rename = "compact")]
    Compact,
    /// Render full tool call input/output. Also accepts legacy `on`.
    #[serde(rename = "full", alias = "on")]
    #[default]
    Full,
}

impl ShowTools {
    /// Returns the canonical config/state string for this mode.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::SummarizeTurn => "summarize-turn",
            Self::SummarizePrompt => "summarize-prompt",
            Self::Compact => "compact",
            Self::Full => "full",
        }
    }

    /// Parses a config/state string. Accepts legacy `on` as
    /// [`ShowTools::Full`].
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "summarize-turn" => Some(Self::SummarizeTurn),
            "summarize-prompt" => Some(Self::SummarizePrompt),
            "compact" => Some(Self::Compact),
            "full" | "on" => Some(Self::Full),
            _ => None,
        }
    }
}

/// Which inter-agent messages are shown in the CLI transcript.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum ShowMessages {
    /// Hide these messages.
    #[serde(rename = "none")]
    None,
    /// Show summaries for messages involving this agent.
    #[serde(rename = "self-summary")]
    SelfSummary,
    /// Show full messages involving this agent.
    #[serde(rename = "self-full")]
    SelfFull,
    /// Show summaries for all such messages.
    #[serde(rename = "all-summary")]
    AllSummary,
    /// Show full text for all such messages.
    #[serde(rename = "all-full")]
    #[default]
    AllFull,
}

impl ShowMessages {
    /// Returns the canonical config/state string for this mode.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::SelfSummary => "self-summary",
            Self::SelfFull => "self-full",
            Self::AllSummary => "all-summary",
            Self::AllFull => "all-full",
        }
    }

    /// Parses a config/state string into a message display mode.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "self-summary" => Some(Self::SelfSummary),
            "self-full" => Some(Self::SelfFull),
            "all-summary" => Some(Self::AllSummary),
            "all-full" => Some(Self::AllFull),
            _ => None,
        }
    }
}

/// Visibility mode for routine CLI lifecycle and status messages.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum ShowStatus {
    /// Show all routine startup lifecycle and status messages.
    #[serde(rename = "all")]
    #[default]
    All,
    /// Hide routine startup lifecycle/status messages while preserving
    /// important messages such as extension configuration errors.
    #[serde(rename = "minimal")]
    Minimal,
}

impl ShowStatus {
    /// Returns the canonical config/state string for this mode.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Minimal => "minimal",
        }
    }

    /// Parses a config/state string into a status display mode.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "all" => Some(Self::All),
            "minimal" => Some(Self::Minimal),
            _ => None,
        }
    }
}

impl Default for CliState {
    fn default() -> Self {
        Self {
            show_diff: false,
            show_thinking: true,
            show_turn_stats: false,
            redraw_counter: false,
            redraw_history_size: 2000,
            show_ui_io: false,
            show_tools: ShowTools::Full,
            show_messages: ShowMessages::AllFull,
            show_internal_prompts: false,
            notice_level: tau_proto::NoticeLevel::Info,
            show_status: ShowStatus::All,
            show_prompt_scroll_indicator: true,
        }
    }
}

impl CliState {
    /// Load the persisted CLI state. Missing / malformed file → defaults.
    #[must_use]
    pub fn load(dirs: &TauDirs) -> Self {
        Self::load_with_default(dirs, Self::default())
    }

    /// Load the persisted CLI state, using `default` when state is missing or
    /// malformed. This lets static CLI config provide the initial values while
    /// `:set` changes still persist as runtime state.
    #[must_use]
    pub fn load_with_default(dirs: &TauDirs, default: Self) -> Self {
        let Some(dir) = dirs.state_dir.as_ref() else {
            return default;
        };
        let path = dir.join("cli.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return default;
        };
        match serde_json::from_str::<CliStatePatch>(&text) {
            Ok(patch) => patch.apply_to(default),
            Err(_) => default,
        }
    }

    /// Persist current state. Best-effort: a command never fails
    /// because the user's state dir is read-only, but failures are
    /// logged on stderr so a silently-resetting state dir is visible
    /// to the user.
    pub fn save(&self, dirs: &TauDirs) {
        let Some(dir) = dirs.state_dir.as_ref() else {
            return;
        };
        if let Err(error) = self.save_inner(dir) {
            eprintln!(
                "tau: failed to persist CLI state to {}: {error}",
                dir.join("cli.json").display()
            );
        }
    }

    fn save_inner(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("cli.json");
        let text = serde_json::to_string_pretty(self).map_err(path_std_io::Error::other)?;
        std::fs::write(path, text)
    }
}

// ---------------------------------------------------------------------------
// Harness settings
// ---------------------------------------------------------------------------

/// Global disabled-by-default policy for bounded Provider cache refreshes.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderCacheRefresh {
    /// Whether this harness may issue cache refresh work.
    pub enabled: bool,
    /// Maximum idle lifetime retained after a qualifying real response.
    pub max_idle_seconds: ProviderCacheMaxIdle,
}

/// Validated economic-idle horizon for automatic Provider cache refreshes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProviderCacheMaxIdle(u64);

impl ProviderCacheMaxIdle {
    /// Construct a horizon in the inclusive range `1..=86_400`.
    pub fn new(seconds: u64) -> Result<Self, &'static str> {
        if !(1..=86_400).contains(&seconds) {
            return Err(
                "provider_cache_refresh.max_idle_seconds must be between 1 and 86400 seconds inclusive",
            );
        }
        Ok(Self(seconds))
    }

    /// Return the represented duration.
    #[must_use]
    pub fn duration(self) -> Duration {
        Duration::from_secs(self.0)
    }

    /// Return the validated integer value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for ProviderCacheMaxIdle {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let seconds = u64::deserialize(deserializer)?;
        Self::new(seconds).map_err(D::Error::custom)
    }
}

impl Default for ProviderCacheRefresh {
    fn default() -> Self {
        Self {
            enabled: false,
            max_idle_seconds: ProviderCacheMaxIdle(300),
        }
    }
}

/// Inclusive threshold policy for live agent-watch retry notifications.
///
/// The raw YAML threshold remains an unrestricted `u32`: zero disables
/// threshold suppression, while a nonzero value suppresses retries through that
/// attempt inclusively.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentWatchRetryNotificationPolicy {
    /// Largest live retry attempt included by the suppression threshold.
    threshold: u32,
}

impl AgentWatchRetryNotificationPolicy {
    /// Preserves a raw configuration threshold without changing its validity.
    #[must_use]
    pub const fn from_raw(threshold: u32) -> Self {
        Self { threshold }
    }

    /// Returns whether a live retry attempt is hidden from a watching agent.
    #[must_use]
    pub const fn suppresses(self, attempt: u32) -> bool {
        self.threshold != 0 && attempt <= self.threshold
    }
}

/// Validated millisecond delays for one harness-owned notification class.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(try_from = "NotificationDeliveryPolicyWire")]
pub struct NotificationDeliveryPolicy {
    /// Delay while the target is dispatchable and idle.
    idle: Duration,
    /// Delay while the target waits for any input or reports `Waiting`.
    wait_any: Duration,
    /// Delay while the target waits for one or more exact tool calls.
    wait_tool: Duration,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotificationDeliveryPolicyWire {
    /// Idle delay in integer milliseconds.
    #[serde(alias = "idleMs")]
    idle_ms: u64,
    /// Wait-any delay in integer milliseconds.
    #[serde(alias = "waitAnyMs")]
    wait_any_ms: u64,
    /// Exact-tool-wait delay in integer milliseconds.
    #[serde(alias = "waitToolMs")]
    wait_tool_ms: u64,
}

impl TryFrom<NotificationDeliveryPolicyWire> for NotificationDeliveryPolicy {
    type Error = String;

    fn try_from(raw: NotificationDeliveryPolicyWire) -> Result<Self, Self::Error> {
        if raw.wait_any_ms < raw.idle_ms || raw.wait_tool_ms < raw.wait_any_ms {
            return Err(
                "notification delivery delays must satisfy idle_ms <= wait_any_ms <= wait_tool_ms"
                    .to_owned(),
            );
        }
        let now = Instant::now();
        for (name, millis) in [
            ("idle_ms", raw.idle_ms),
            ("wait_any_ms", raw.wait_any_ms),
            ("wait_tool_ms", raw.wait_tool_ms),
        ] {
            if now.checked_add(Duration::from_millis(millis)).is_none() {
                return Err(format!(
                    "notification delivery {name} exceeds the monotonic clock range"
                ));
            }
        }
        Ok(Self {
            idle: Duration::from_millis(raw.idle_ms),
            wait_any: Duration::from_millis(raw.wait_any_ms),
            wait_tool: Duration::from_millis(raw.wait_tool_ms),
        })
    }
}

impl NotificationDeliveryPolicy {
    /// Validate and construct one integer-millisecond delivery policy.
    pub fn from_millis(idle_ms: u64, wait_any_ms: u64, wait_tool_ms: u64) -> Result<Self, String> {
        Self::try_from(NotificationDeliveryPolicyWire {
            idle_ms,
            wait_any_ms,
            wait_tool_ms,
        })
    }

    /// Returns the idle delay.
    #[must_use]
    pub const fn idle(self) -> Duration {
        self.idle
    }

    /// Returns the wait-any delay.
    #[must_use]
    pub const fn wait_any(self) -> Duration {
        self.wait_any
    }

    /// Returns the exact-tool-wait delay.
    #[must_use]
    pub const fn wait_tool(self) -> Duration {
        self.wait_tool
    }

    /// Return whether every runtime state selects immediate delivery.
    #[must_use]
    pub const fn is_immediate(self) -> bool {
        self.idle.is_zero() && self.wait_any.is_zero() && self.wait_tool.is_zero()
    }
}

/// Harness-owned delivery policy for every approved notification class.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NotificationDeliveryPolicies {
    /// Authenticated visible user prompts.
    pub user_prompt: NotificationDeliveryPolicy,
    /// Noninitial provider, work, long-wait, and lifecycle watch notifications.
    pub status: NotificationDeliveryPolicy,
    /// Message, WatchPrompt, and WatchResponse agent messages.
    pub agent_message: NotificationDeliveryPolicy,
    /// Accepted live canonical external-message facts.
    pub external_message: NotificationDeliveryPolicy,
}

/// Harness/agent settings loaded from `harness.yaml`.
///
/// Has no `Default` impl on purpose — the baseline lives in
/// `config/built-in.harness.yaml` and is layered in by the loader. Use
/// [`HarnessSettings::built_in`] when you need a fresh, populated value in a
/// test or fallback.
#[derive(Clone, Debug)]
pub struct HarnessSettings {
    /// Number of days to keep inactive session state directories.
    /// Set to `0` to disable session cleanup.
    pub session_retention_days: u64,

    /// Number of days to keep non-authoritative session diagnostic files.
    /// Set to `0` to disable diagnostic cleanup.
    pub diagnostic_retention_days: u64,
    /// Whether a newly spawned interactive harness greets its initial UI with
    /// the Tau onboarding notice.
    pub show_introduction_notice: bool,
    /// Validated inclusive policy for activating-input wait timeouts.
    pub wait_timeout_bounds: WaitTimeoutBounds,
    /// Inclusive policy for suppressing live agent-watch retry notifications.
    pub agent_watch_retry_notification_threshold: AgentWatchRetryNotificationPolicy,
    /// Bounded runtime-only prompt-injected notification delivery delays.
    pub notification_delivery: NotificationDeliveryPolicies,
    /// Disabled-by-default bounded Provider cache refresh policy.
    pub provider_cache_refresh: ProviderCacheRefresh,
    /// Default Tau-state presentation for supervised extension instances.
    pub tau_state_access: TauStateAccess,

    /// Extension table, keyed by name. Built-in entries (`provider-builtin`,
    /// `core-shell`) come pre-baked at the harness level; anything the
    /// user writes here overrides those per-field, or adds a new
    /// extension.
    ///
    /// Example `harness.yaml`:
    /// ```yaml
    /// extensions:
    ///   core-shell:
    ///     enable: false
    ///   provider-builtin:
    ///     prefix: ["ssh", "user@host"]
    ///     cwd: "/srv/tau-provider"
    ///   mything:
    ///     command: ["/usr/local/bin/my-tau-ext"]
    /// ```
    pub extensions: HashMap<String, ExtensionEntry>,

    /// Role selected on startup when no explicit runtime selection has been
    /// made. If the configured role is missing, Tau warns and falls back to
    /// the first role from the first non-empty `agents.role_groups` entry after
    /// roles inside that group are sorted by role `order` and then role name.
    pub default_role: Option<String>,

    /// Harness-owned role defaults. Each role is a partial set of model
    /// settings; missing fields use provider/model fallbacks for the selected
    /// provider-published model.
    pub roles: HashMap<String, AgentRole>,

    /// Ordered role groups used by the CLI for structured role navigation.
    /// Role names remain globally unique; groups provide shared defaults for
    /// their `roles` entries and affect presentation and keyboard cycling.
    /// The group list preserves config order; role presentation inside a group
    /// is sorted later by each role's `order` and then role name.
    pub role_groups: Vec<RoleGroup>,

    /// Agent-global prompt fragments from harness config. Loaded settings also
    /// fold these into every role's prompt fragments; this field preserves the
    /// global source list for inspection and future config tooling.
    pub prompt_fragments: Vec<RolePromptFragment>,

    /// Agent-global required skill names applied to every role.
    pub required_skills: Vec<tau_proto::SkillName>,

    /// Agent-global named context-size alerts applied to every role.
    pub context_size_alerts: BTreeMap<String, ContextSizeAlert>,

    /// User-configured prompt templates exposed in the CLI as `:prompt <id>`.
    /// Map keys are non-empty ids with no whitespace so they can be addressed
    /// unambiguously from the command.
    pub custom_prompts: Vec<CustomPrompt>,

    /// Harness-owned declarative policy applied to tool tags before role
    /// overrides.
    pub tool_policy: ToolPolicy,

    /// Handlebars template used to mint new durable agent identifiers.
    pub agent_id_template: String,

    /// Optional Handlebars template used to name newly created agents.
    pub agent_display_name_template: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessSettingsWire {
    /// Whole-session directory retention in days.
    session_retention_days: u64,
    /// Non-authoritative session diagnostic retention in days.
    diagnostic_retention_days: u64,
    /// Whether to show Tau's onboarding notice to the initial UI.
    #[serde(alias = "showIntroductionNotice")]
    show_introduction_notice: bool,
    /// Lowest effective activating-input wait timeout in whole minutes.
    #[serde(alias = "waitTimeoutMinimumMinutes")]
    wait_timeout_minimum_minutes: u64,
    /// Highest effective activating-input wait timeout in whole minutes.
    #[serde(alias = "waitTimeoutMaximumMinutes")]
    wait_timeout_maximum_minutes: u64,
    /// Largest provider retry attempt hidden from watching agents.
    #[serde(alias = "agentWatchRetryNotificationThreshold")]
    agent_watch_retry_notification_threshold: u32,
    /// Bounded runtime-only prompt-injected notification delivery delays.
    #[serde(alias = "notificationDelivery")]
    notification_delivery: NotificationDeliveryPolicies,
    /// Disabled-by-default bounded Provider cache refresh policy.
    #[serde(default)]
    provider_cache_refresh: ProviderCacheRefresh,
    /// Default extension Tau-state presentation.
    #[serde(default)]
    tau_state_access: TauStateAccess,
    /// Configured extension entries.
    extensions: HashMap<String, ExtensionEntry>,
    #[serde(default, alias = "customPrompts")]
    /// User-defined prompt text keyed by prompt identifier.
    custom_prompts: BTreeMap<String, String>,
    #[serde(default)]
    /// Harness-wide declarative tool policy.
    tool_policy: ToolPolicy,
    /// Agent defaults and role groups.
    agents: AgentsSettings,
    /// Startup-only provider/model reference aliases. The loader resolves these
    /// after role replay and does not retain them in [`HarnessSettings`].
    #[serde(default, rename = "aliases")]
    _aliases: ModelReferenceAliases,
    /// Named, opt-in patches. The loader consumes these before returning the
    /// effective settings, so they never enter [`HarnessSettings`].
    #[serde(default, rename = "profiles")]
    _profiles: BTreeMap<String, HarnessProfile>,
    /// Base-layer fallback profile selection. The loader consumes this before
    /// applying a profile, so it never enters [`HarnessSettings`].
    #[serde(default, rename = "default_profile")]
    _default_profile: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentsSettings {
    /// Whether roles default to enabled before group and role overrides apply.
    ///
    /// Omission keeps the built-in enabled baseline. Explicit `null` clears
    /// this patch and leaves the role's default behavior in effect.
    #[serde(
        default = "agent_enable_default",
        alias = "enabled",
        deserialize_with = "present_option"
    )]
    enable: Option<Option<bool>>,
    /// Whether roles default to appearing in the built-in delegate-role
    /// catalog.
    ///
    /// Omission retains the visible baseline. Explicit `null` clears this patch
    /// and leaves the role's default presentation behavior in effect.
    #[serde(default = "agent_visible_default", deserialize_with = "present_option")]
    visible: Option<Option<bool>>,
    #[serde(default, alias = "defaultRole")]
    default_role: Option<String>,
    #[serde(alias = "idTemplate")]
    id_template: String,
    #[serde(default, alias = "displayNameTemplate")]
    display_name_template: Option<String>,
    #[serde(default, alias = "promptFragments")]
    prompt_fragments: Vec<RolePromptFragment>,
    #[serde(default, alias = "requiredSkills")]
    required_skills: Vec<tau_proto::SkillName>,
    /// Provider settings that default every role before group and role patches.
    #[serde(default, deserialize_with = "present_option")]
    model: Option<Option<ModelId>>,
    #[serde(default, deserialize_with = "present_option")]
    effort: Option<Option<ConfiguredRoleSetting<tau_proto::Effort>>>,
    #[serde(default, deserialize_with = "present_option")]
    verbosity: Option<Option<ConfiguredRoleSetting<tau_proto::Verbosity>>>,
    #[serde(
        default,
        alias = "thinkingSummary",
        deserialize_with = "present_option"
    )]
    thinking_summary: Option<Option<ConfiguredRoleSetting<tau_proto::ThinkingSummary>>>,
    #[serde(default, alias = "serviceTier", deserialize_with = "present_option")]
    service_tier: Option<Option<tau_proto::ServiceTier>>,
    #[serde(default, deserialize_with = "present_option")]
    compaction: Option<Option<RoleCompaction>>,
    #[serde(
        default,
        alias = "inferenceCompaction",
        deserialize_with = "present_option"
    )]
    inference_compaction: Option<Option<RoleCompaction>>,
    #[serde(default)]
    compactions: BTreeMap<String, CompactionPolicyPatch>,
    /// Agent-global alert patches applied before group and role settings.
    #[serde(default, alias = "contextSizeAlerts")]
    context_size_alerts: BTreeMap<String, ContextSizeAlertPatch>,
    #[serde(default, alias = "roleGroups")]
    role_groups: RawRoleGroups,
    /// Logical web capability defaults applied to every role.
    #[serde(default, alias = "webTools")]
    web_tools: RawWebToolsPolicy,
}

impl<'de> Deserialize<'de> for HarnessSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = HarnessSettingsWire::deserialize(deserializer)?;
        for extension_name in wire.extensions.keys() {
            validate_extension_name(extension_name).map_err(D::Error::custom)?;
        }
        let agent_defaults = wire.agents.role_defaults();
        let wait_timeout_bounds = WaitTimeoutBounds::from_raw(
            wire.wait_timeout_minimum_minutes,
            wire.wait_timeout_maximum_minutes,
        )
        .map_err(D::Error::custom)?;
        let mut settings = Self {
            session_retention_days: wire.session_retention_days,
            diagnostic_retention_days: wire.diagnostic_retention_days,
            show_introduction_notice: wire.show_introduction_notice,
            wait_timeout_bounds,
            agent_watch_retry_notification_threshold: AgentWatchRetryNotificationPolicy::from_raw(
                wire.agent_watch_retry_notification_threshold,
            ),
            notification_delivery: wire.notification_delivery,
            provider_cache_refresh: wire.provider_cache_refresh,
            tau_state_access: wire.tau_state_access,
            extensions: wire.extensions,
            default_role: wire.agents.default_role,
            roles: HashMap::new(),
            role_groups: Vec::new(),
            prompt_fragments: wire.agents.prompt_fragments,
            required_skills: wire.agents.required_skills,
            context_size_alerts: BTreeMap::new(),
            custom_prompts: custom_prompt_map_to_vec(wire.custom_prompts),
            tool_policy: wire.tool_policy,
            agent_id_template: wire.agents.id_template,
            agent_display_name_template: wire.agents.display_name_template,
        };
        let mut effective_agent_defaults = AgentRole::default();
        effective_agent_defaults.apply_patch(&agent_defaults);
        settings.apply_agent_defaults_to_roles(&agent_defaults);
        settings.apply_context_size_alert_overrides(wire.agents.context_size_alerts);
        settings
            .apply_role_group_overrides(wire.agents.role_groups, &effective_agent_defaults)
            .map_err(D::Error::custom)?;
        validate_custom_prompts(&settings.custom_prompts).map_err(D::Error::custom)?;
        settings.remove_disabled_roles();
        settings
            .validate_inter_session_roles()
            .map_err(D::Error::custom)?;
        settings
            .validate_context_size_alerts()
            .map_err(D::Error::custom)?;
        settings.validate_web_tools().map_err(D::Error::custom)?;
        Ok(settings)
    }
}

/// Declarative harness-owned tool policy loaded from `tool_policy` config.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ToolPolicy {
    /// Optional global shell edit implementation override. `None` uses the
    /// selected model's declared preference, or exact-text except for ChatGPT.
    #[serde(default, deserialize_with = "deserialize_shell_tool_style")]
    pub default_shell_tool_style: Option<ShellToolStyle>,
    /// Rules keyed by stable names so higher-precedence config can override or
    /// disable built-in behavior.
    pub rules: IndexMap<String, ToolPolicyRule>,
}

/// One provider-visible shell editing surface selected before role controls.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ShellToolStyle {
    /// Use the Custom/Text Codex patch surface.
    Codex,
    /// Use Tau's line-coordinate editor.
    Edit,
    /// Use exact snapshot-based text replacement.
    Replace,
}

/// Deserializes an optional style, treating whitespace-only text as a reset.
fn deserialize_shell_tool_style<'de, D>(deserializer: D) -> Result<Option<ShellToolStyle>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    match value.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some("codex") => Ok(Some(ShellToolStyle::Codex)),
        Some("edit") => Ok(Some(ShellToolStyle::Edit)),
        Some("replace") => Ok(Some(ShellToolStyle::Replace)),
        Some(_) => Err(D::Error::custom(
            "tool_policy.default_shell_tool_style must be codex, edit, or replace",
        )),
    }
}

/// One declarative tool policy rule evaluated against model/tool tags.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ToolPolicyRule {
    /// Whether this rule participates in evaluation. Defaults to true;
    /// `enabled` is accepted as an alias for config ergonomics.
    #[serde(alias = "enabled")]
    pub enable: bool,
    /// Priority used before rule name for deterministic evaluation order.
    pub priority: i32,
    /// Optional model-side match conditions. Missing or empty means always.
    pub when: ToolPolicyWhen,
    /// Tool tag patterns disabled first when the rule matches.
    pub disable_tool_tags: Vec<ToolTagPattern>,
    /// Tool tag patterns enabled after disables when the rule matches.
    pub enable_tool_tags: Vec<ToolTagPattern>,
}

impl Default for ToolPolicyRule {
    fn default() -> Self {
        Self {
            enable: true,
            priority: 0,
            when: ToolPolicyWhen::default(),
            disable_tool_tags: Vec::new(),
            enable_tool_tags: Vec::new(),
        }
    }
}

/// Model tag predicates controlling when a tool policy rule applies.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ToolPolicyWhen {
    /// Required model tag patterns; all listed patterns must match at least one
    /// selected model tag.
    pub model_tags: Vec<ModelTagPattern>,
}

/// Exact or terminal-prefix pattern over provider-published model tags.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ModelTagPattern(TagPattern);

impl ModelTagPattern {
    /// Returns true when this pattern matches one selected model tag.
    pub fn matches(&self, tag: &ModelTag) -> bool {
        self.0.matches(tag.as_str())
    }
}

impl<'de> Deserialize<'de> for ModelTagPattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        TagPattern::parse(&text, |candidate| ModelTag::try_new(candidate).is_some())
            .map(Self)
            .map_err(D::Error::custom)
    }
}

/// Exact or terminal-prefix pattern over extension-published tool tags.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ToolTagPattern(TagPattern);

impl ToolTagPattern {
    /// Returns true when this pattern matches one extension-published tool tag.
    pub fn matches(&self, tag: &ToolTag) -> bool {
        self.0.matches(tag.as_str())
    }
}

impl<'de> Deserialize<'de> for ToolTagPattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        TagPattern::parse(&text, |candidate| ToolTag::try_new(candidate).is_some())
            .map(Self)
            .map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TagPattern {
    text: String,
    prefix_len: Option<usize>,
}

impl TagPattern {
    fn parse(text: &str, valid_exact: impl Fn(&str) -> bool) -> Result<Self, String> {
        if let Some(star) = text.find('*') {
            if star != text.len() - 1 || text.matches('*').count() != 1 {
                return Err(format!(
                    "tag pattern `{text}` may only use `*` as a terminal prefix wildcard"
                ));
            }
            let prefix = &text[..star];
            if prefix.is_empty() || !prefix.ends_with(':') {
                return Err(format!(
                    "tag pattern `{text}` wildcard must follow a non-empty colon prefix"
                ));
            }
            let sentinel = format!("{prefix}x");
            if !valid_exact(&sentinel) {
                return Err(format!("tag pattern `{text}` has an invalid tag prefix"));
            }
            Ok(Self {
                text: text.to_owned(),
                prefix_len: Some(prefix.len()),
            })
        } else if valid_exact(text) {
            Ok(Self {
                text: text.to_owned(),
                prefix_len: None,
            })
        } else {
            Err(format!("tag pattern `{text}` is not a valid tag"))
        }
    }

    fn matches(&self, tag: &str) -> bool {
        match self.prefix_len {
            Some(prefix_len) => tag.starts_with(&self.text[..prefix_len]),
            None => tag == self.text,
        }
    }
}

impl Serialize for TagPattern {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.text)
    }
}

#[derive(Deserialize)]
struct HarnessRoleOverrides {
    // This narrower pass extracts only agent role metadata after the main
    // harness settings layer has already validated the full schema. Leave
    // unrelated fields permissive here so `agents.id_template`, future
    // non-role agent settings, and unrelated top-level fields do not need
    // duplicate ignore entries in this replay-only wire type.
    #[serde(default)]
    agents: HarnessAgentRoleOverrides,
}

/// Startup-only aliases for the two components of configured model references.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct ModelReferenceAliases {
    /// Exact provider-name aliases.
    providers: BTreeMap<ProviderName, ProviderName>,
    /// Exact model-name suffix aliases.
    models: BTreeMap<ModelName, ModelName>,
}

/// Narrow extraction view used to discard aliases before runtime settings.
#[derive(Default, Deserialize)]
#[serde(default)]
struct HarnessAliasesWire {
    /// Effective startup-only aliases.
    aliases: ModelReferenceAliases,
}

/// Raw, selected-profile patches kept separate from effective harness settings.
///
/// Profiles deliberately expose only the startup default role, role metadata,
/// the global extension Tau-state access default, extension enablement, and
/// arbitrary extension-owned config patches. This avoids making a profile a
/// second copy of the complete harness schema while allowing extension settings
/// to compose recursively.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HarnessProfile {
    /// Default Tau-state presentation for supervised extension instances.
    tau_state_access: Option<TauStateAccess>,
    /// Agent defaults, role groups, and per-role patches.
    agents: HarnessProfileAgentOverrides,
    /// Startup-only provider/model aliases changed by this profile.
    aliases: ModelReferenceAliases,
    /// Enablement and extension-owned config patches for normally resolved
    /// extensions, including built-ins.
    extensions: BTreeMap<String, HarnessProfileExtension>,
}

/// The supported agent portion of one configuration profile.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HarnessProfileAgentOverrides {
    /// Startup role patch applied after base files and before CLI overrides.
    #[serde(default, alias = "defaultRole", deserialize_with = "present_option")]
    default_role: Option<Option<String>>,
    /// Default enablement patch applied to every role.
    #[serde(alias = "enabled", deserialize_with = "present_option")]
    enable: Option<Option<bool>>,
    /// Default built-in delegate-role catalog visibility applied to every role.
    #[serde(deserialize_with = "present_option")]
    visible: Option<Option<bool>>,
    /// Role groups and their member role patches.
    #[serde(alias = "roleGroups")]
    role_groups: RawRoleGroups,
    /// Global prompt fragments applied to every role.
    #[serde(alias = "promptFragments")]
    prompt_fragments: Vec<RolePromptFragment>,
    /// Global required skills applied to every role.
    #[serde(alias = "requiredSkills")]
    required_skills: Vec<tau_proto::SkillName>,
    /// Default model patch.
    #[serde(default, deserialize_with = "present_option")]
    model: Option<Option<ModelId>>,
    /// Default effort patch.
    #[serde(default, deserialize_with = "present_option")]
    effort: Option<Option<ConfiguredRoleSetting<tau_proto::Effort>>>,
    /// Default verbosity patch.
    #[serde(default, deserialize_with = "present_option")]
    verbosity: Option<Option<ConfiguredRoleSetting<tau_proto::Verbosity>>>,
    /// Default thinking-summary patch.
    #[serde(
        default,
        alias = "thinkingSummary",
        deserialize_with = "present_option"
    )]
    thinking_summary: Option<Option<ConfiguredRoleSetting<tau_proto::ThinkingSummary>>>,
    /// Default service-tier patch.
    #[serde(default, alias = "serviceTier", deserialize_with = "present_option")]
    service_tier: Option<Option<tau_proto::ServiceTier>>,
    /// Default compaction patch.
    #[serde(default, deserialize_with = "present_option")]
    compaction: Option<Option<RoleCompaction>>,
    /// Default provider-inline compaction patch.
    #[serde(
        default,
        alias = "inferenceCompaction",
        deserialize_with = "present_option"
    )]
    inference_compaction: Option<Option<RoleCompaction>>,
    /// Default named standalone compaction patches.
    #[serde(default)]
    compactions: BTreeMap<String, CompactionPolicyPatch>,
    /// Global named context-size alert patches.
    #[serde(alias = "contextSizeAlerts")]
    context_size_alerts: BTreeMap<String, ContextSizeAlertPatch>,
    /// Logical web capability policy patch.
    #[serde(default, alias = "webTools")]
    web_tools: Option<RawWebToolsPolicy>,
}

impl From<HarnessProfileAgentOverrides> for HarnessAgentRoleOverrides {
    fn from(profile: HarnessProfileAgentOverrides) -> Self {
        Self {
            enable: profile.enable,
            visible: profile.visible,
            role_groups: profile.role_groups,
            prompt_fragments: profile.prompt_fragments,
            required_skills: profile.required_skills,
            model: profile.model,
            effort: profile.effort,
            verbosity: profile.verbosity,
            thinking_summary: profile.thinking_summary,
            service_tier: profile.service_tier,
            compaction: profile.compaction,
            inference_compaction: profile.inference_compaction,
            compactions: profile.compactions,
            context_size_alerts: profile.context_size_alerts,
            web_tools: profile.web_tools,
        }
    }
}

/// One profile's extension enablement and extension-owned config patch.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HarnessProfileExtension {
    /// Whether the named base extension should run.
    #[serde(alias = "enabled")]
    enable: Option<bool>,
    /// Arbitrary extension-owned configuration patch.
    config: Option<serde_json::Value>,
}

/// Named role and extension patches discovered from built-in and user
/// configuration files.
#[derive(Default, Deserialize)]
#[serde(default)]
struct HarnessProfiles {
    /// Raw profile patches keyed by the selectable profile name.
    profiles: BTreeMap<String, HarnessProfile>,
}

/// Top-level base configuration used only to select a fallback profile.
///
/// This intentionally does not deserialize the effective harness schema:
/// selection must finish before any selected-profile patch is considered.
#[derive(Default, Deserialize)]
#[serde(default)]
struct HarnessDefaultProfile {
    /// Named profile selected when no CLI or environment selector is present.
    default_profile: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct HarnessAgentRoleOverrides {
    /// Agent-global role enablement patch replayed before group and role
    /// patches.
    #[serde(alias = "enabled", deserialize_with = "present_option")]
    enable: Option<Option<bool>>,
    /// Agent-global built-in delegate-role catalog visibility patch replayed
    /// before group and role patches.
    #[serde(deserialize_with = "present_option")]
    visible: Option<Option<bool>>,
    #[serde(alias = "roleGroups")]
    role_groups: RawRoleGroups,
    #[serde(alias = "promptFragments")]
    prompt_fragments: Vec<RolePromptFragment>,
    #[serde(alias = "requiredSkills")]
    required_skills: Vec<tau_proto::SkillName>,
    #[serde(default, deserialize_with = "present_option")]
    model: Option<Option<ModelId>>,
    #[serde(default, deserialize_with = "present_option")]
    effort: Option<Option<ConfiguredRoleSetting<tau_proto::Effort>>>,
    #[serde(default, deserialize_with = "present_option")]
    verbosity: Option<Option<ConfiguredRoleSetting<tau_proto::Verbosity>>>,
    #[serde(
        default,
        alias = "thinkingSummary",
        deserialize_with = "present_option"
    )]
    thinking_summary: Option<Option<ConfiguredRoleSetting<tau_proto::ThinkingSummary>>>,
    #[serde(default, alias = "serviceTier", deserialize_with = "present_option")]
    service_tier: Option<Option<tau_proto::ServiceTier>>,
    #[serde(default, deserialize_with = "present_option")]
    compaction: Option<Option<RoleCompaction>>,
    #[serde(
        default,
        alias = "inferenceCompaction",
        deserialize_with = "present_option"
    )]
    inference_compaction: Option<Option<RoleCompaction>>,
    #[serde(default)]
    compactions: BTreeMap<String, CompactionPolicyPatch>,
    /// Agent-global alert patches replayed through domain-specific role
    /// merging.
    #[serde(alias = "contextSizeAlerts")]
    context_size_alerts: BTreeMap<String, ContextSizeAlertPatch>,
    /// Agent-global logical web capability policy patch.
    #[serde(default, alias = "webTools")]
    web_tools: Option<RawWebToolsPolicy>,
}

impl AgentsSettings {
    /// Returns agent defaults that apply before group and role patches.
    fn role_defaults(&self) -> AgentRolePatch {
        AgentRolePatch {
            enable: self.enable,
            visible: self.visible,
            model: self.model.clone(),
            effort: self.effort,
            verbosity: self.verbosity,
            thinking_summary: self.thinking_summary,
            service_tier: self.service_tier,
            compaction: self.compaction,
            inference_compaction: self.inference_compaction,
            compactions: self.compactions.clone(),
            web_tools: Some(self.web_tools.clone()),
            ..AgentRolePatch::default()
        }
    }
}

impl HarnessAgentRoleOverrides {
    /// Returns agent defaults that apply before group and role patches.
    fn role_defaults(&self) -> AgentRolePatch {
        AgentRolePatch {
            enable: self.enable,
            visible: self.visible,
            model: self.model.clone(),
            effort: self.effort,
            verbosity: self.verbosity,
            thinking_summary: self.thinking_summary,
            service_tier: self.service_tier,
            compaction: self.compaction,
            inference_compaction: self.inference_compaction,
            compactions: self.compactions.clone(),
            web_tools: self.web_tools.clone(),
            ..AgentRolePatch::default()
        }
    }
}

/// One saved prompt template exposed through the CLI `:prompt <id>` command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustomPrompt {
    /// Stable command argument used to select the prompt. Must be non-empty
    /// and contain no whitespace.
    pub id: String,
    /// Prompt text inserted into the editable CLI prompt buffer. Must be
    /// non-empty; users can still edit it before submission.
    pub text: String,
}

fn custom_prompt_map_to_vec(prompts: BTreeMap<String, String>) -> Vec<CustomPrompt> {
    prompts
        .into_iter()
        .map(|(id, text)| CustomPrompt { id, text })
        .collect()
}

fn validate_custom_prompts(prompts: &[CustomPrompt]) -> Result<(), String> {
    for prompt in prompts {
        if prompt.id.is_empty() {
            return Err("custom prompt id must not be empty".to_owned());
        }
        if prompt.id.split_whitespace().count() != 1 || prompt.id.trim() != prompt.id {
            return Err(format!(
                "custom prompt id `{}` must not contain whitespace",
                prompt.id
            ));
        }
        if prompt.text.is_empty() {
            return Err(format!(
                "custom prompt `{}` text must not be empty",
                prompt.id
            ));
        }
    }
    Ok(())
}

/// One ordered group in the role navigation palette.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoleGroup {
    /// Stable group name from `agents.role_groups.<name>`.
    pub name: String,
    /// Globally unique role names in this group, in config declaration order.
    pub roles: Vec<String>,
}

type RawRoleGroups = IndexMap<String, RawRoleGroup>;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawRoleGroup {
    // `enabled` was a mistaken old spelling. Keep it as a little bandaid for
    // reading old config during migration.
    #[serde(alias = "enabled", deserialize_with = "present_option")]
    enable: Option<Option<bool>>,
    #[serde(deserialize_with = "present_option")]
    visible: Option<Option<bool>>,
    #[serde(deserialize_with = "present_option")]
    order: Option<Option<i64>>,
    #[serde(alias = "interSessionReceiver", deserialize_with = "present_option")]
    inter_session_receiver: Option<Option<bool>>,
    #[serde(alias = "interSessionAutoStart", deserialize_with = "present_option")]
    inter_session_auto_start: Option<Option<bool>>,
    #[serde(deserialize_with = "present_option")]
    description: Option<Option<String>>,
    #[serde(deserialize_with = "present_option")]
    model: Option<Option<ModelId>>,
    #[serde(deserialize_with = "present_option")]
    effort: Option<Option<ConfiguredRoleSetting<tau_proto::Effort>>>,
    #[serde(deserialize_with = "present_option")]
    verbosity: Option<Option<ConfiguredRoleSetting<tau_proto::Verbosity>>>,
    #[serde(alias = "thinkingSummary", deserialize_with = "present_option")]
    thinking_summary: Option<Option<ConfiguredRoleSetting<tau_proto::ThinkingSummary>>>,
    #[serde(alias = "serviceTier", deserialize_with = "present_option")]
    service_tier: Option<Option<tau_proto::ServiceTier>>,
    #[serde(deserialize_with = "present_option")]
    compaction: Option<Option<RoleCompaction>>,
    #[serde(alias = "inferenceCompaction", deserialize_with = "present_option")]
    inference_compaction: Option<Option<RoleCompaction>>,
    compactions: BTreeMap<String, CompactionPolicyPatch>,
    /// Group-default alert patches applied to every member role.
    #[serde(alias = "contextSizeAlerts")]
    context_size_alerts: BTreeMap<String, ContextSizeAlertPatch>,
    #[serde(alias = "promptFragments")]
    prompt_fragments: Option<Vec<RolePromptFragment>>,
    #[serde(alias = "promptOverride", deserialize_with = "present_option")]
    prompt_override: Option<Option<String>>,
    #[serde(deserialize_with = "present_option")]
    tools: Option<Option<Vec<ToolName>>>,
    #[serde(alias = "disableToolTags")]
    disable_tool_tags: Option<Vec<ToolTagPattern>>,
    #[serde(alias = "enableToolTags")]
    enable_tool_tags: Option<Vec<ToolTagPattern>>,
    #[serde(alias = "disableToolGroups")]
    disable_tool_groups: Option<Vec<tau_proto::ToolGroupName>>,
    #[serde(alias = "enableToolGroups")]
    enable_tool_groups: Option<Vec<tau_proto::ToolGroupName>>,
    #[serde(alias = "disableTools")]
    disable_tools: Option<Vec<ToolName>>,
    #[serde(alias = "enableTools")]
    enable_tools: Option<Vec<ToolName>>,
    #[serde(alias = "requiredSkills")]
    required_skills: Option<Vec<tau_proto::SkillName>>,
    /// Group-default logical web capability policy patch.
    #[serde(alias = "webTools")]
    web_tools: Option<RawWebToolsPolicy>,
    roles: IndexMap<String, AgentRolePatch>,
}

// Role patches must distinguish three scalar states during layered merges:
// absent means inherit the lower-precedence value, `null` means clear it, and a
// concrete value replaces it. Replacement lists use `Option<Vec<_>>` so an
// absent field inherits while an explicit `[]` clears the list. `tools` is a
// nullable replacement list: `tools: null` clears an inherited allow-list back
// to default tool behavior, while `tools: []` sets an explicit empty
// allow-list. Prompt fragments and required skills are the exceptions and
// remain additive when present.
///
/// Model-facing scalar settings additionally accept a saturating relative
/// adjustment. Resolution always produces the existing absolute role value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfiguredRoleSetting<T> {
    /// A concrete setting replaces the inherited value.
    Absolute(T),
    /// Adjusts the inherited value by validated saturating levels.
    Relative(tau_proto::UiRoleSettingAdjustment),
}

impl<'de, T> Deserialize<'de> for ConfiguredRoleSetting<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire<T> {
            Absolute(T),
            Relative(String),
        }

        match Wire::deserialize(deserializer)? {
            Wire::Absolute(value) => Ok(Self::Absolute(value)),
            Wire::Relative(value) => {
                let (direction, amount) = value
                    .split_once(':')
                    .map_or((value.as_str(), 1), |(direction, amount)| {
                        (direction, amount.parse::<u8>().unwrap_or(0))
                    });
                if amount == 0 {
                    return Err(D::Error::custom(
                        "relative role setting amount must be a positive integer",
                    ));
                }
                match direction {
                    "increase" => NonZeroU8::new(amount)
                        .map(tau_proto::UiRoleSettingAdjustment::Increase)
                        .map(Self::Relative)
                        .ok_or_else(|| {
                            D::Error::custom(
                                "relative role setting amount must be a positive integer",
                            )
                        }),
                    "decrease" => NonZeroU8::new(amount)
                        .map(tau_proto::UiRoleSettingAdjustment::Decrease)
                        .map(Self::Relative)
                        .ok_or_else(|| {
                            D::Error::custom(
                                "relative role setting amount must be a positive integer",
                            )
                        }),
                    _ => Err(D::Error::custom(
                        "relative role setting must be increase, decrease, increase:<amount>, or decrease:<amount>",
                    )),
                }
            }
        }
    }
}

trait RelativeRoleSettingValue: Copy {
    /// Applies Tau's shared, saturating positive adjustment.
    fn adjust(self, adjustment: tau_proto::UiRoleSettingAdjustment) -> Self;
}

impl RelativeRoleSettingValue for tau_proto::Effort {
    fn adjust(self, adjustment: tau_proto::UiRoleSettingAdjustment) -> Self {
        tau_proto::Effort::adjust(self, adjustment)
    }
}

impl RelativeRoleSettingValue for tau_proto::Verbosity {
    fn adjust(self, adjustment: tau_proto::UiRoleSettingAdjustment) -> Self {
        tau_proto::Verbosity::adjust(self, adjustment)
    }
}

impl RelativeRoleSettingValue for tau_proto::ThinkingSummary {
    fn adjust(self, adjustment: tau_proto::UiRoleSettingAdjustment) -> Self {
        tau_proto::ThinkingSummary::adjust(self, adjustment)
    }
}

impl<T: RelativeRoleSettingValue> ConfiguredRoleSetting<T> {
    /// Resolves this setting against an absolute inherited value.
    fn resolve(self, inherited: T) -> T {
        match self {
            Self::Absolute(value) => value,
            Self::Relative(adjustment) => inherited.adjust(adjustment),
        }
    }
}

pub(super) fn present_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

fn agent_enable_default() -> Option<Option<bool>> {
    Some(Some(true))
}

fn agent_visible_default() -> Option<Option<bool>> {
    Some(Some(true))
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AgentRolePatch {
    #[serde(alias = "enabled", deserialize_with = "present_option")]
    enable: Option<Option<bool>>,
    #[serde(deserialize_with = "present_option")]
    visible: Option<Option<bool>>,
    #[serde(deserialize_with = "present_option")]
    order: Option<Option<i64>>,
    #[serde(alias = "interSessionReceiver", deserialize_with = "present_option")]
    inter_session_receiver: Option<Option<bool>>,
    #[serde(alias = "interSessionAutoStart", deserialize_with = "present_option")]
    inter_session_auto_start: Option<Option<bool>>,
    #[serde(deserialize_with = "present_option")]
    description: Option<Option<String>>,
    #[serde(deserialize_with = "present_option")]
    model: Option<Option<ModelId>>,
    #[serde(deserialize_with = "present_option")]
    effort: Option<Option<ConfiguredRoleSetting<tau_proto::Effort>>>,
    #[serde(deserialize_with = "present_option")]
    verbosity: Option<Option<ConfiguredRoleSetting<tau_proto::Verbosity>>>,
    #[serde(alias = "thinkingSummary", deserialize_with = "present_option")]
    thinking_summary: Option<Option<ConfiguredRoleSetting<tau_proto::ThinkingSummary>>>,
    #[serde(alias = "serviceTier", deserialize_with = "present_option")]
    service_tier: Option<Option<tau_proto::ServiceTier>>,
    #[serde(deserialize_with = "present_option")]
    compaction: Option<Option<RoleCompaction>>,
    #[serde(alias = "inferenceCompaction", deserialize_with = "present_option")]
    inference_compaction: Option<Option<RoleCompaction>>,
    compactions: BTreeMap<String, CompactionPolicyPatch>,
    /// Role-specific alert patches applied after group defaults.
    #[serde(alias = "contextSizeAlerts")]
    context_size_alerts: BTreeMap<String, ContextSizeAlertPatch>,
    #[serde(alias = "promptFragments")]
    prompt_fragments: Option<Vec<RolePromptFragment>>,
    #[serde(alias = "promptOverride", deserialize_with = "present_option")]
    prompt_override: Option<Option<String>>,
    #[serde(deserialize_with = "present_option")]
    tools: Option<Option<Vec<ToolName>>>,
    #[serde(alias = "disableToolTags")]
    disable_tool_tags: Option<Vec<ToolTagPattern>>,
    #[serde(alias = "enableToolTags")]
    enable_tool_tags: Option<Vec<ToolTagPattern>>,
    #[serde(alias = "disableToolGroups")]
    disable_tool_groups: Option<Vec<tau_proto::ToolGroupName>>,
    #[serde(alias = "enableToolGroups")]
    enable_tool_groups: Option<Vec<tau_proto::ToolGroupName>>,
    #[serde(alias = "disableTools")]
    disable_tools: Option<Vec<ToolName>>,
    #[serde(alias = "enableTools")]
    enable_tools: Option<Vec<ToolName>>,
    #[serde(alias = "requiredSkills")]
    required_skills: Option<Vec<tau_proto::SkillName>>,
    /// Logical web capability policy patch.
    #[serde(alias = "webTools")]
    web_tools: Option<RawWebToolsPolicy>,
}

impl AgentRolePatch {
    /// Rejects a source that mixes the legacy compound setting with either
    /// successor setting, whose meaning would otherwise be order-dependent.
    fn validate_compaction_input(&self, path: &str) -> Result<(), SettingsError> {
        if self.compaction.is_some()
            && (self.inference_compaction.is_some() || !self.compactions.is_empty())
        {
            return Err(SettingsError::Config(config::ConfigError::Message(
                format!(
                    "{path}: legacy `compaction` cannot be combined with `inference_compaction` or `compactions` in one source layer"
                ),
            )));
        }
        Ok(())
    }
}

impl RawRoleGroup {
    fn defaults(&self) -> AgentRolePatch {
        AgentRolePatch {
            enable: self.enable,
            visible: self.visible,
            order: self.order,
            inter_session_receiver: self.inter_session_receiver,
            inter_session_auto_start: self.inter_session_auto_start,
            description: self.description.clone(),
            model: self.model.clone(),
            effort: self.effort,
            verbosity: self.verbosity,
            thinking_summary: self.thinking_summary,
            service_tier: self.service_tier,
            compaction: self.compaction,
            inference_compaction: self.inference_compaction,
            compactions: self.compactions.clone(),
            context_size_alerts: self.context_size_alerts.clone(),
            prompt_fragments: self.prompt_fragments.clone(),
            prompt_override: self.prompt_override.clone(),
            tools: self.tools.clone(),
            disable_tool_tags: self.disable_tool_tags.clone(),
            enable_tool_tags: self.enable_tool_tags.clone(),
            disable_tool_groups: self.disable_tool_groups.clone(),
            enable_tool_groups: self.enable_tool_groups.clone(),
            disable_tools: self.disable_tools.clone(),
            enable_tools: self.enable_tools.clone(),
            required_skills: self.required_skills.clone(),
            web_tools: self.web_tools.clone(),
        }
    }
}

/// One command-line role availability override, applied after all config files.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum RoleCliOverride {
    /// Enable a named role in the effective role set.
    Enable(String),
    /// Disable a named role in the effective role set.
    Disable(String),
    /// Disable all roles before later command-line role overrides are applied.
    DisableAll,
}

/// One command-line extension availability override, applied after all config
/// files and built-in extension defaults are merged.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum ExtensionCliOverride {
    /// Enable a named extension in the effective extension set.
    Enable(String),
    /// Disable a named extension in the effective extension set.
    Disable(String),
    /// Enable all configured extensions before later command-line extension
    /// overrides are applied.
    EnableAll,
    /// Disable all configured extensions before later command-line extension
    /// overrides are applied.
    DisableAll,
}

impl HarnessSettings {
    /// The fully-populated baseline that ships with tau, parsed from
    /// the embedded `built-in.harness.yaml`.
    pub fn built_in() -> Self {
        let mut s: Self = parse_built_in_yaml("built-in.harness.yaml", BUILT_IN_HARNESS_YAML);
        s.remove_disabled_roles();
        s.apply_agent_globals_to_roles();
        s
    }

    /// Returns the validated inclusive policy for activating-input `wait`
    /// calls.
    #[must_use]
    pub const fn wait_timeout_bounds(&self) -> WaitTimeoutBounds {
        self.wait_timeout_bounds
    }

    fn apply_role_group_overrides(
        &mut self,
        groups: RawRoleGroups,
        agent_defaults: &AgentRole,
    ) -> Result<(), SettingsError> {
        self.apply_role_group_members(&groups, agent_defaults)?;
        self.apply_role_group_defaults(&groups);
        self.apply_role_overrides(&groups);
        Ok(())
    }

    /// Registers role-group membership without applying group or role patches.
    ///
    /// The layered loader first gathers all members, so defaults from an
    /// earlier source also apply to roles introduced by a later source.
    fn apply_role_group_members(
        &mut self,
        groups: &RawRoleGroups,
        agent_defaults: &AgentRole,
    ) -> Result<(), SettingsError> {
        for (group_name, group) in groups {
            let group_exists = self
                .role_groups
                .iter()
                .any(|existing_group| existing_group.name == group_name.as_str());
            if group.roles.is_empty() {
                if !group_exists {
                    self.role_groups.push(RoleGroup {
                        name: group_name.clone(),
                        roles: Vec::new(),
                    });
                }
                continue;
            }
            for role_name in group.roles.keys() {
                let override_role = AgentRole {
                    context_size_alerts: self.context_size_alerts.clone(),
                    ..agent_defaults.clone()
                };
                self.ensure_role_group_member(group_name, role_name)?;
                self.roles.entry(role_name.clone()).or_insert(override_role);
            }
        }
        Ok(())
    }

    /// Applies each source's group defaults after every role has joined its
    /// group, and before any source's per-role overrides.
    fn apply_role_group_defaults(&mut self, groups: &RawRoleGroups) {
        for (group_name, group) in groups {
            let Some(role_names) = self
                .role_groups
                .iter()
                .find(|existing_group| existing_group.name == group_name.as_str())
                .map(|existing_group| existing_group.roles.clone())
            else {
                continue;
            };
            let group_defaults = group.defaults();
            for role_name in role_names {
                if let Some(role) = self.roles.get_mut(&role_name) {
                    role.apply_patch(&group_defaults);
                }
            }
        }
    }

    /// Applies every per-role patch in one source after all group defaults have
    /// established the inheritance base.
    fn apply_role_overrides(&mut self, groups: &RawRoleGroups) {
        for group in groups.values() {
            for (role_name, role_overrides) in &group.roles {
                if let Some(role) = self.roles.get_mut(role_name) {
                    role.apply_patch(role_overrides);
                }
            }
        }
    }

    fn apply_agent_defaults_to_roles(&mut self, defaults: &AgentRolePatch) {
        for role in self.roles.values_mut() {
            role.apply_patch(defaults);
        }
    }

    fn apply_role_cli_overrides(
        &mut self,
        overrides: &[RoleCliOverride],
    ) -> Result<(), SettingsError> {
        for override_ in overrides {
            match override_ {
                RoleCliOverride::Enable(role_name) => {
                    let role = self
                        .roles
                        .get_mut(role_name)
                        .ok_or_else(|| SettingsError::UnknownRoleCliOverride(role_name.clone()))?;
                    role.enable = Some(true);
                }
                RoleCliOverride::Disable(role_name) => {
                    let role = self
                        .roles
                        .get_mut(role_name)
                        .ok_or_else(|| SettingsError::UnknownRoleCliOverride(role_name.clone()))?;
                    role.enable = Some(false);
                }
                RoleCliOverride::DisableAll => {
                    for role in self.roles.values_mut() {
                        role.enable = Some(false);
                    }
                }
            }
        }
        Ok(())
    }

    fn remove_disabled_roles(&mut self) {
        self.roles
            .retain(|_role_name, role| role.enable.unwrap_or(true));
        for group in &mut self.role_groups {
            group
                .roles
                .retain(|role_name| self.roles.contains_key(role_name));
        }
        self.role_groups.retain(|group| !group.roles.is_empty());
    }

    fn ensure_role_group_member(
        &mut self,
        group_name: &str,
        role_name: &str,
    ) -> Result<(), SettingsError> {
        for group in &mut self.role_groups {
            if group.roles.iter().any(|existing| existing == role_name) {
                if group.name == group_name {
                    return Ok(());
                }
                return Err(SettingsError::DuplicateGroupedRole {
                    role: role_name.to_owned(),
                    first_group: group.name.clone(),
                    second_group: group_name.to_owned(),
                });
            }
        }

        if let Some(group) = self
            .role_groups
            .iter_mut()
            .find(|group| group.name == group_name)
        {
            group.roles.push(role_name.to_owned());
        } else {
            self.role_groups.push(RoleGroup {
                name: group_name.to_owned(),
                roles: vec![role_name.to_owned()],
            });
        }
        Ok(())
    }

    fn validate_inter_session_roles(&self) -> Result<(), SettingsError> {
        let mut role_names = self.roles.keys().collect::<Vec<_>>();
        role_names.sort();
        for role_name in role_names {
            let role = &self.roles[role_name];
            if role.inter_session_auto_start.unwrap_or(false)
                && !role.inter_session_receiver.unwrap_or(false)
            {
                return Err(SettingsError::InvalidInterSessionAutoStart {
                    role: role_name.to_owned(),
                });
            }
        }
        Ok(())
    }

    fn validate_context_size_alerts(&self) -> Result<(), SettingsError> {
        for (role_name, role) in &self.roles {
            for (alert_name, alert) in &role.context_size_alerts {
                if !alert.threshold.is_valid() {
                    return Err(SettingsError::Config(config::ConfigError::Message(
                        format!(
                            "role `{role_name}` context-size alert `{alert_name}` requires a positive threshold"
                        ),
                    )));
                }
                if alert.message.is_empty() {
                    return Err(SettingsError::Config(config::ConfigError::Message(
                        format!(
                            "role `{role_name}` context-size alert `{alert_name}` message must not be empty"
                        ),
                    )));
                }
                if !matches!(
                    alert.when.at,
                    ContextPolicyPoint::AfterResponse | ContextPolicyPoint::OuterTurnFinished
                ) {
                    return Err(SettingsError::Config(config::ConfigError::Message(
                        format!(
                            "role `{role_name}` context-size alert `{alert_name}` only supports after_response or outer_turn_finished"
                        ),
                    )));
                }
            }
            for (policy_name, policy) in &role.compactions {
                if matches!(policy.threshold, CompactionPolicyThreshold::Tokens(0)) {
                    return Err(SettingsError::Config(config::ConfigError::Message(
                        format!(
                            "role `{role_name}` compaction policy `{policy_name}` requires a threshold"
                        ),
                    )));
                }
                if !matches!(
                    policy.when.at,
                    ContextPolicyPoint::BeforeInference | ContextPolicyPoint::OuterTurnFinished
                ) {
                    return Err(SettingsError::Config(config::ConfigError::Message(
                        format!(
                            "role `{role_name}` compaction policy `{policy_name}` only supports before_inference or outer_turn_finished"
                        ),
                    )));
                }
            }
        }
        // The global map is independently public effective state. Validate it
        // after roles to retain established role-specific diagnostic priority.
        for (alert_name, alert) in &self.context_size_alerts {
            if !alert.threshold.is_valid() {
                return Err(SettingsError::Config(config::ConfigError::Message(
                    format!(
                        "agent-global context-size alert `{alert_name}` requires a positive threshold"
                    ),
                )));
            }
        }
        Ok(())
    }

    /// Validate every effective role's logical web policy after inheritance.
    fn validate_web_tools(&mut self) -> Result<(), SettingsError> {
        for (role_name, role) in &mut self.roles {
            role.web_tools
                .finalize(&format!("agents.roles.{role_name}.web_tools"))
                .map_err(|message| SettingsError::Config(config::ConfigError::Message(message)))?;
        }
        Ok(())
    }

    fn apply_prompt_fragment_overrides(&mut self, fragments: Vec<RolePromptFragment>) {
        for prompt_fragment in fragments {
            if !self.prompt_fragments.contains(&prompt_fragment) {
                self.prompt_fragments.push(prompt_fragment);
            }
        }
    }

    fn apply_required_skill_overrides(&mut self, skills: Vec<tau_proto::SkillName>) {
        for skill in skills {
            if !self.required_skills.contains(&skill) {
                self.required_skills.push(skill);
            }
        }
    }

    fn apply_context_size_alert_overrides(
        &mut self,
        alerts: BTreeMap<String, ContextSizeAlertPatch>,
    ) {
        apply_context_size_alert_patches(&mut self.context_size_alerts, &alerts);
        for role in self.roles.values_mut() {
            apply_context_size_alert_patches(&mut role.context_size_alerts, &alerts);
        }
    }

    fn apply_global_prompt_fragments_to_roles(&mut self) {
        for role in self.roles.values_mut() {
            for prompt_fragment in &self.prompt_fragments {
                if !role.prompt_fragments.contains(prompt_fragment) {
                    role.prompt_fragments.push(prompt_fragment.clone());
                }
            }
        }
    }

    fn apply_global_required_skills_to_roles(&mut self) {
        for role in self.roles.values_mut() {
            for skill in &self.required_skills {
                if !role.required_skills.contains(skill) {
                    role.required_skills.push(skill.clone());
                }
            }
        }
    }

    fn apply_agent_globals_to_roles(&mut self) {
        self.apply_global_prompt_fragments_to_roles();
        self.apply_global_required_skills_to_roles();
        for role in self.roles.values_mut() {
            for (name, alert) in &self.context_size_alerts {
                role.context_size_alerts
                    .entry(name.clone())
                    .or_insert_with(|| alert.clone());
            }
        }
    }

    /// Returns the configured session retention duration.
    ///
    /// A value of `0` disables time-based cleanup and returns `None`; otherwise
    /// the configured day count is converted to a saturating [`Duration`].
    #[must_use]
    pub fn session_retention(&self) -> Option<Duration> {
        if self.session_retention_days == 0 {
            return None;
        }
        Some(Duration::from_secs(
            self.session_retention_days.saturating_mul(24 * 60 * 60),
        ))
    }

    /// Returns the configured non-authoritative diagnostic retention duration.
    ///
    /// A value of `0` disables time-based cleanup and returns `None`; otherwise
    /// the configured day count is converted to a saturating [`Duration`].
    #[must_use]
    pub fn diagnostic_retention(&self) -> Option<Duration> {
        if self.diagnostic_retention_days == 0 {
            return None;
        }
        Some(Duration::from_secs(
            self.diagnostic_retention_days.saturating_mul(24 * 60 * 60),
        ))
    }
}

/// Stable identity for a Tau-owned component command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltinComponentIdentity {
    /// Tau's built-in provider component.
    Provider,
}

impl BuiltinComponentIdentity {
    /// Resolve identity from suffix tokens only after the caller has
    /// established that the command slot is Tau-owned or inherited as the
    /// current Tau executable. Arbitrary explicit commands must not call
    /// this to claim component authority.
    #[must_use]
    pub fn from_tau_owned_suffix(suffix: &[String]) -> Option<Self> {
        (suffix == ["component", "ext-provider-builtin"]).then_some(Self::Provider)
    }
}

/// One entry in the harness's `extensions` map.
///
/// All fields are optional on the wire so users can override just the
/// fields they care about for built-in extensions; the harness merges
/// these with built-in defaults at startup. `None` on any field means
/// "the user did not say anything" — distinct from an empty value the
/// user set on purpose.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionEntry {
    /// argv prefix prepended before `command`. Useful for wrappers
    /// that don't change the inner command, e.g.
    /// `["ssh", "user@host"]` to run remotely or
    /// `["bwrap", "--ro-bind", "/", "/", "--"]` to sandbox.
    pub prefix: Option<Vec<String>>,
    /// Optional instance-specific Tau-state presentation.
    pub tau_state_access: Option<TauStateAccess>,
    /// Optional restoration of the historical ambient Tau runtime socket view.
    pub tau_runtime_socket_access: Option<TauRuntimeSocketAccess>,

    /// Optional immutable prefix for this instance's structural tool names.
    ///
    /// The outer option preserves layering presence: an absent value inherits,
    /// explicit `null` clears, and a string sets the prefix.
    #[serde(default, alias = "toolPrefix", deserialize_with = "present_option")]
    pub tool_prefix: Option<Option<tau_proto::ToolNamePrefix>>,

    /// argv of the extension itself. `command[0]` is the executable;
    /// the rest are arguments. For built-in extensions this defaults
    /// to `[<current-exe>]`. A new entry may provide a nonempty command
    /// explicitly, or omit `command` and provide a nonempty `suffix` to run
    /// that suffix as a subcommand of the current Tau executable. An explicit
    /// empty command is invalid and does not enable piggybacking.
    pub command: Option<Vec<String>>,

    /// Current working directory used when starting the extension process.
    ///
    /// The outer option tracks layered config presence: `None` means this layer
    /// did not mention `cwd`. `Some(Some(path))` sets or overrides the working
    /// directory, while `Some(None)` comes from explicit `cwd: null` and clears
    /// a lower-precedence cwd so the child inherits the harness process working
    /// directory.
    #[serde(default, deserialize_with = "present_option")]
    pub cwd: Option<Option<PathBuf>>,

    /// argv suffix appended after `command`. Symmetric to `prefix`.
    /// Built-in extensions use this to spell their subcommand (e.g.
    /// `["component", "ext-provider-builtin"]`) so the `command` slot stays
    /// as the tau binary path.
    pub suffix: Option<Vec<String>>,

    /// Whether to run this extension. Defaults to the built-in's
    /// `enable` (or `true` for user-added entries). Set to `false`
    /// to keep the entry in config but skip spawning.
    pub enable: Option<bool>,

    /// Whether harness startup requires this enabled extension to initialize.
    /// Defaults to the built-in's `require` value, or `true` for user-added
    /// entries and built-ins that do not specify it. Disabled extensions ignore
    /// this field because they are not desired at all.
    pub require: Option<bool>,

    /// Maximum number of seconds the harness waits for this extension to send
    /// its initial `Ready` after a successful supervised spawn.
    ///
    /// An absent value inherits the built-in value, or Tau's two-second general
    /// default for user-added extensions. Values must be within one through
    /// 3,600 seconds.
    pub startup_timeout_seconds: Option<u64>,

    /// Role tag. Built-in providers use `role: "provider"`; entries
    /// without that role are treated as tool extensions.
    pub role: Option<String>,

    /// Free-form configuration object handed to the extension at
    /// startup via `LifecycleConfigure`. The harness does not
    /// interpret it — the extension defines and validates its own
    /// schema. Absent on the wire means "merge nothing in", so the
    /// built-in's default config object is used unchanged.
    pub config: Option<serde_json::Value>,

    /// Secret names this extension is allowed to receive, keyed by secret name.
    pub secrets: Option<BTreeMap<String, ExtensionSecretEntry>>,
}

/// Per-secret declaration for one extension.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionSecretEntry {
    /// Whether startup may continue when this secret is unavailable. Required
    /// by default.
    pub optional: bool,
}

/// One command-line harness config override in `key=value` form.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessConfigCliOverride {
    /// Dotted config path to override, e.g.
    /// `extensions.core-shell.config.working_directory`.
    pub key: String,
    /// Raw right-hand side parsed as YAML when applied.
    pub raw_value: String,
}

/// Public environment variable containing a JSON object of provider aliases.
pub const TAU_PROVIDER_ALIASES_ENV: &str = "TAU_PROVIDER_ALIASES";
/// Public environment variable containing a JSON object of model-name aliases.
pub const TAU_MODEL_ALIASES_ENV: &str = "TAU_MODEL_ALIASES";

/// One typed `--provider-alias FROM=TO` startup override.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderAliasCliOverride {
    /// Alias name used in configured model references.
    pub from: ProviderName,
    /// Provider name substituted at startup.
    pub to: ProviderName,
}

impl FromStr for ProviderAliasCliOverride {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (from, to) = parse_alias_assignment(value)?;
        Ok(Self {
            from: from
                .parse()
                .map_err(|error| format!("invalid provider alias name: {error}"))?,
            to: to
                .parse()
                .map_err(|error| format!("invalid provider alias target: {error}"))?,
        })
    }
}

/// One typed `--model-alias FROM=TO` startup override.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelAliasCliOverride {
    /// Exact model-name suffix used in configured model references.
    pub from: ModelName,
    /// Exact model-name suffix substituted at startup.
    pub to: ModelName,
}

impl FromStr for ModelAliasCliOverride {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (from, to) = parse_alias_assignment(value)?;
        Ok(Self {
            from: from
                .parse()
                .map_err(|error| format!("invalid model alias name: {error}"))?,
            to: to
                .parse()
                .map_err(|error| format!("invalid model alias target: {error}"))?,
        })
    }
}

/// Environment and dedicated CLI sources for startup-only model references.
#[derive(Default)]
pub struct ModelReferenceAliasSources<'a> {
    /// Public provider-alias JSON environment value.
    pub provider_environment: Option<OsString>,
    /// Public model-alias JSON environment value.
    pub model_environment: Option<OsString>,
    /// Ordered dedicated provider CLI operations.
    pub provider_cli: &'a [ProviderAliasCliOverride],
    /// Ordered dedicated model CLI operations.
    pub model_cli: &'a [ModelAliasCliOverride],
}

/// Splits a dedicated alias operation at its first equals sign without
/// trimming provider-meaningful or model-meaningful bytes.
fn parse_alias_assignment(value: &str) -> Result<(&str, &str), String> {
    let Some((from, to)) = value.split_once('=') else {
        return Err("expected FROM=TO".to_owned());
    };
    if from.is_empty() {
        return Err("alias name must not be empty".to_owned());
    }
    if to.is_empty() {
        return Err("alias target must not be empty".to_owned());
    }
    Ok((from, to))
}

/// Converts public environment alias maps and dedicated CLI operations into
/// final generic config layers. Environment maps apply first; repeated CLI
/// operations then replace matching keys in command-line order.
pub fn model_reference_alias_config_overrides(
    sources: ModelReferenceAliasSources<'_>,
) -> Result<Vec<HarnessConfigCliOverride>, SettingsError> {
    let mut providers = parse_alias_environment::<ProviderName>(
        TAU_PROVIDER_ALIASES_ENV,
        sources.provider_environment,
    )?;
    let mut models =
        parse_alias_environment::<ModelName>(TAU_MODEL_ALIASES_ENV, sources.model_environment)?;
    for override_ in sources.provider_cli {
        providers.insert(override_.from.clone(), override_.to.clone());
    }
    for override_ in sources.model_cli {
        models.insert(override_.from.clone(), override_.to.clone());
    }
    let mut overrides = Vec::new();
    if !providers.is_empty() {
        overrides.push(HarnessConfigCliOverride {
            key: "aliases.providers".to_owned(),
            raw_value: serde_json::to_string(&providers)
                .expect("provider alias maps always serialize"),
        });
    }
    if !models.is_empty() {
        overrides.push(HarnessConfigCliOverride {
            key: "aliases.models".to_owned(),
            raw_value: serde_json::to_string(&models).expect("model alias maps always serialize"),
        });
    }
    Ok(overrides)
}

/// Parses one public alias environment variable as a strict JSON object.
fn parse_alias_environment<T>(
    variable: &'static str,
    value: Option<OsString>,
) -> Result<BTreeMap<T, T>, SettingsError>
where
    T: Ord + for<'de> Deserialize<'de>,
{
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let value = value.into_string().map_err(|_| {
        SettingsError::Config(config::ConfigError::Message(format!(
            "{variable} must contain valid UTF-8 JSON"
        )))
    })?;
    serde_json::from_str(&value).map_err(|error| {
        SettingsError::Config(config::ConfigError::Message(format!(
            "{variable} must be a JSON object of alias names to targets: {error}"
        )))
    })
}

impl FromStr for HarnessConfigCliOverride {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((key, raw_value)) = value.split_once('=') else {
            return Err("expected KEY=VALUE".to_owned());
        };
        if key.is_empty() {
            return Err("harness config override key must not be empty".to_owned());
        }
        Ok(Self {
            key: key.to_owned(),
            raw_value: raw_value.to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// Harness roles
// ---------------------------------------------------------------------------

/// Partial harness role settings loaded from `harness.yaml` and persisted
/// to state. `None` means "use the selected model's fallback" for every field.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AgentRole {
    /// Whether this role is part of the effective runtime role set. Defaults to
    /// enabled; set to `false` in a higher-precedence config layer to hide a
    /// built-in or lower-layer role without deleting the rest of its settings.
    ///
    /// `enabled` was a mistaken old spelling. Keep it as a little bandaid for
    /// reading old config during migration.
    #[serde(alias = "enabled", skip_serializing_if = "Option::is_none")]
    pub enable: Option<bool>,
    /// Whether this role appears in the built-in delegate-role prompt catalog.
    ///
    /// Visibility only controls that catalog. It does not affect role
    /// availability, authorization, or explicit `agent_start` requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    /// Optional role ordering key within a role group. Lower values come first;
    /// roles with the same order, or without an order, are sorted by role name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
    /// Whether agents created with this role may receive bare inter-session
    /// messages. Unset effective values are disabled.
    #[serde(
        skip_serializing_if = "Option::is_none",
        alias = "interSessionReceiver"
    )]
    pub inter_session_receiver: Option<bool>,
    /// Whether this role may be started when a bare inter-session message has
    /// no live receiver. This requires [`Self::inter_session_receiver`].
    #[serde(
        skip_serializing_if = "Option::is_none",
        alias = "interSessionAutoStart"
    )]
    pub inter_session_auto_start: Option<bool>,
    /// Short free-form summary shown in role-selection completion menus.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Model id preferred by this role.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Reasoning effort preferred by this role.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<tau_proto::Effort>,
    /// Output verbosity preferred by this role.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<tau_proto::Verbosity>,
    /// Thinking-summary mode preferred by this role.
    #[serde(skip_serializing_if = "Option::is_none", alias = "thinkingSummary")]
    pub thinking_summary: Option<tau_proto::ThinkingSummary>,
    /// Provider service tier preferred by this role.
    #[serde(skip_serializing_if = "Option::is_none", alias = "serviceTier")]
    pub service_tier: Option<tau_proto::ServiceTier>,
    /// Automatic provider-side compaction policy for this role. Missing values
    /// inherit from lower-precedence role settings; effective roles default to
    /// [`RoleCompaction::ProviderDefault`].
    #[serde(skip_serializing, default)]
    pub compaction: Option<RoleCompaction>,
    /// Singular provider-inline and reactive-overflow compaction policy.
    #[serde(skip_serializing_if = "Option::is_none", alias = "inferenceCompaction")]
    pub inference_compaction: Option<RoleCompaction>,
    /// Named harness-scheduled standalone compaction policies.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub compactions: BTreeMap<String, CompactionPolicy>,
    /// Named internal prompts injected after provider-reported context usage
    /// exceeds each enabled alert's token threshold.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        alias = "contextSizeAlerts"
    )]
    pub context_size_alerts: BTreeMap<String, ContextSizeAlert>,
    /// Prompt fragments contributed by this role. Fragments are rendered as
    /// Handlebars templates and ordered together with tool/extension fragments.
    #[serde(skip_serializing_if = "Vec::is_empty", alias = "promptFragments")]
    pub prompt_fragments: Vec<RolePromptFragment>,
    /// Optional system prompt template name for this role. "built-in" selects
    /// Tau's embedded default template. Other names resolve to
    /// `<config_dir>/prompts/<name>.hbs`.
    #[serde(skip_serializing_if = "Option::is_none", alias = "promptOverride")]
    pub prompt_override: Option<String>,
    /// Explicit internal tool names enabled for this role. When unset, tools
    /// use their own default enablement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolName>>,
    /// Logical web capability policy compiled at prompt materialization.
    #[serde(default, alias = "webTools")]
    pub web_tools: WebToolsPolicy,
    /// Tool tag patterns disabled after global policy and before role-level tag
    /// enables.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        alias = "disableToolTags"
    )]
    pub disable_tool_tags: Vec<ToolTagPattern>,
    /// Tool tag patterns enabled after role-level tag disables and global
    /// policy.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        alias = "enableToolTags"
    )]
    pub enable_tool_tags: Vec<ToolTagPattern>,
    /// Tool group names disabled after tag changes and before group enables.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        alias = "disableToolGroups"
    )]
    pub disable_tool_groups: Vec<tau_proto::ToolGroupName>,
    /// Tool group names enabled after group disables and before individual tool
    /// changes.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        alias = "enableToolGroups"
    )]
    pub enable_tool_groups: Vec<tau_proto::ToolGroupName>,
    /// Internal tool names disabled before final individual tool enables.
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "disableTools")]
    pub disable_tools: Vec<ToolName>,
    /// Internal tool names enabled last for this role.
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "enableTools")]
    pub enable_tools: Vec<ToolName>,
    /// Exact skill names that must be model-loadable before this role is
    /// available. Agent-global, group, and role requirements are additive and
    /// de-duplicated.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        alias = "requiredSkills"
    )]
    pub required_skills: Vec<tau_proto::SkillName>,
}

/// Automatic provider-side compaction policy for a harness role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoleCompaction {
    /// Ask the provider to use its model-specific default threshold.
    ProviderDefault,
    /// Do not request provider-side automatic compaction.
    Disabled,
    /// Ask the provider to compact at an explicit token threshold.
    Threshold(u64),
    /// Ask the provider to compact when this many tokens remain in the selected
    /// model's context window.
    Reserve(u64),
}

impl Serialize for RoleCompaction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        match self {
            Self::ProviderDefault => serializer.serialize_str("provider_default"),
            Self::Disabled => serializer.serialize_str("disabled"),
            Self::Threshold(tokens) | Self::Reserve(tokens) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(
                    if matches!(self, Self::Threshold(_)) {
                        "threshold"
                    } else {
                        "reserve"
                    },
                    tokens,
                )?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for RoleCompaction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Name(String),
            Boundary(Boundary),
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Boundary {
            threshold: Option<u64>,
            reserve: Option<u64>,
        }

        match Wire::deserialize(deserializer)? {
            Wire::Name(name) if matches!(name.as_str(), "provider_default" | "providerDefault") => {
                Ok(Self::ProviderDefault)
            }
            Wire::Name(name) if name == "disabled" => Ok(Self::Disabled),
            Wire::Name(name) => Err(D::Error::custom(format!(
                "unknown compaction policy `{name}`"
            ))),
            Wire::Boundary(Boundary { threshold, reserve }) => match (threshold, reserve) {
                (Some(_), Some(_)) => Err(D::Error::custom(
                    "compaction policy cannot set both `threshold` and `reserve`",
                )),
                (Some(tokens), None) => Ok(Self::Threshold(tokens)),
                (None, Some(tokens)) => Ok(Self::Reserve(tokens)),
                (None, None) => Err(D::Error::custom(
                    "compaction policy requires `threshold` or `reserve`",
                )),
            },
        }
    }
}

/// Lifecycle point at which a context policy is evaluated.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextPolicyPoint {
    /// Immediately after a successful ordinary provider response.
    AfterResponse,
    /// At the existing safe checkpoint immediately before ordinary inference.
    #[default]
    BeforeInference,
    /// After the durable outer-turn finish has committed.
    #[serde(alias = "outerTurnFinished")]
    OuterTurnFinished,
}

/// Shared lifecycle and logical-status selector for context policies.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextPolicyWhen {
    /// Lifecycle point at which this policy is evaluated.
    pub at: ContextPolicyPoint,
    /// Logical work phases accepted by the policy; `None` accepts every phase.
    #[serde(deserialize_with = "deserialize_optional_nonempty_statuses")]
    pub statuses: Option<Vec<tau_proto::AgentWorkStatusPhase>>,
}

impl Default for ContextPolicyWhen {
    fn default() -> Self {
        Self {
            at: ContextPolicyPoint::BeforeInference,
            statuses: None,
        }
    }
}

fn deserialize_optional_nonempty_statuses<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<tau_proto::AgentWorkStatusPhase>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let statuses = Option::<Vec<tau_proto::AgentWorkStatusPhase>>::deserialize(deserializer)?;
    if statuses.as_ref().is_some_and(Vec::is_empty) {
        return Err(D::Error::custom(
            "context policy statuses must be null or a nonempty list",
        ));
    }
    Ok(statuses)
}

/// Threshold used by one harness-scheduled standalone compaction policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionPolicyThreshold {
    /// Resolve the provider-published standalone threshold for the selected
    /// model.
    ProviderDefault,
    /// Compact at this explicit positive token count.
    Tokens(u64),
    /// Compact when this many tokens remain in the selected model's context
    /// window.
    Reserve(u64),
}

/// Error returned when a remaining-context reserve cannot fit the selected
/// model's context window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionReserveError {
    /// Context window published for the selected model.
    pub context_window: u64,
    /// Configured number of tokens to keep in reserve.
    pub reserve: u64,
}

impl fmt::Display for CompactionReserveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "compaction reserve {} exceeds selected model context window {}",
            self.reserve, self.context_window
        )
    }
}

impl std::error::Error for CompactionReserveError {}

/// Converts a remaining-context reserve into the equivalent used-context
/// threshold for the selected model.
pub fn compaction_threshold_from_reserve(
    context_window: u64,
    reserve: u64,
) -> Result<u64, CompactionReserveError> {
    context_window
        .checked_sub(reserve)
        .ok_or(CompactionReserveError {
            context_window,
            reserve,
        })
}

impl Serialize for CompactionPolicyThreshold {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::ProviderDefault => serializer.serialize_str("provider_default"),
            Self::Tokens(tokens) => serializer.serialize_u64(*tokens),
            Self::Reserve(tokens) => {
                use serde::ser::SerializeMap;

                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("reserve", tokens)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for CompactionPolicyThreshold {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Scalar(CompactionPolicyThresholdScalarWire),
            Reserve(CompactionReserveWire),
        }
        match Wire::deserialize(deserializer)? {
            Wire::Scalar(wire) => wire.into_threshold::<D::Error>(),
            Wire::Reserve(CompactionReserveWire { reserve }) => Ok(Self::Reserve(reserve)),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CompactionPolicyThresholdScalarWire {
    Tokens(u64),
    Name(String),
}

impl CompactionPolicyThresholdScalarWire {
    fn into_threshold<E>(self) -> Result<CompactionPolicyThreshold, E>
    where
        E: serde::de::Error,
    {
        match self {
            Self::Tokens(0) => Err(E::custom("compaction policy threshold must be positive")),
            Self::Tokens(tokens) => Ok(CompactionPolicyThreshold::Tokens(tokens)),
            Self::Name(name)
                if matches!(
                    name.as_str(),
                    "context_limit_safe"
                        | "contextLimitSafe"
                        | "provider_default"
                        | "providerDefault"
                ) =>
            {
                Ok(CompactionPolicyThreshold::ProviderDefault)
            }
            Self::Name(name) => Err(E::custom(format!(
                "unknown compaction policy threshold `{name}`"
            ))),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactionReserveWire {
    reserve: u64,
}

fn deserialize_optional_compaction_policy_threshold<'de, D>(
    deserializer: D,
) -> Result<Option<CompactionPolicyThreshold>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<CompactionPolicyThresholdScalarWire>::deserialize(deserializer)?
        .map(CompactionPolicyThresholdScalarWire::into_threshold::<D::Error>)
        .transpose()
}

/// One effective named harness-scheduled standalone compaction policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionPolicy {
    /// Token boundary for this policy. `Reserve` is serialized through the
    /// sibling `reserve` key rather than `threshold`.
    pub threshold: CompactionPolicyThreshold,
    /// Whether this policy participates in evaluation.
    pub enable: bool,
    /// Lifecycle and logical-status selector.
    pub when: ContextPolicyWhen,
}

impl Serialize for CompactionPolicy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        match self.threshold {
            CompactionPolicyThreshold::Reserve(tokens) => {
                map.serialize_entry("reserve", &tokens)?;
            }
            threshold => map.serialize_entry("threshold", &threshold)?,
        }
        map.serialize_entry("enable", &self.enable)?;
        map.serialize_entry("when", &self.when)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for CompactionPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(
                default,
                deserialize_with = "deserialize_optional_compaction_policy_threshold"
            )]
            threshold: Option<CompactionPolicyThreshold>,
            reserve: Option<u64>,
            #[serde(default = "context_size_alert_enabled_default")]
            enable: bool,
            #[serde(default)]
            when: ContextPolicyWhen,
        }

        let wire = Wire::deserialize(deserializer)?;
        let threshold = compaction_policy_boundary(wire.threshold, wire.reserve)
            .map_err(D::Error::custom)?
            .ok_or_else(|| D::Error::missing_field("threshold or reserve"))?;
        Ok(Self {
            threshold,
            enable: wire.enable,
            when: wire.when,
        })
    }
}

impl CompactionPolicy {
    /// Creates an incomplete merge value that must acquire a threshold before
    /// it can become effective configuration.
    fn merge_seed() -> Self {
        Self {
            threshold: CompactionPolicyThreshold::Tokens(0),
            enable: true,
            when: ContextPolicyWhen::default(),
        }
    }
}

/// Presence-aware patch for one named standalone compaction policy.
#[derive(Clone, Debug, Default)]
struct CompactionPolicyPatch {
    /// Replacement boundary when this layer specifies one.
    threshold: Option<CompactionPolicyThreshold>,
    /// Replacement enablement when this layer specifies one.
    enable: Option<bool>,
    /// Nested condition patch; null resets the complete condition.
    when: Option<Option<ContextPolicyWhenPatch>>,
}

impl<'de> Deserialize<'de> for CompactionPolicyPatch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Default, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        struct Wire {
            #[serde(
                default,
                deserialize_with = "deserialize_optional_compaction_policy_threshold"
            )]
            threshold: Option<CompactionPolicyThreshold>,
            reserve: Option<u64>,
            enable: Option<bool>,
            #[serde(deserialize_with = "present_option")]
            when: Option<Option<ContextPolicyWhenPatch>>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            threshold: compaction_policy_boundary(wire.threshold, wire.reserve)
                .map_err(D::Error::custom)?,
            enable: wire.enable,
            when: wire.when,
        })
    }
}

fn compaction_policy_boundary(
    threshold: Option<CompactionPolicyThreshold>,
    reserve: Option<u64>,
) -> Result<Option<CompactionPolicyThreshold>, &'static str> {
    match (threshold, reserve) {
        (Some(_), Some(_)) => Err("compaction policy cannot set both `threshold` and `reserve`"),
        (Some(threshold), None) => Ok(Some(threshold)),
        (None, Some(reserve)) => Ok(Some(CompactionPolicyThreshold::Reserve(reserve))),
        (None, None) => Ok(None),
    }
}

impl CompactionPolicyPatch {
    /// Applies all fields present in this layer to an effective merge value.
    fn apply_to(&self, policy: &mut CompactionPolicy) {
        if let Some(threshold) = self.threshold {
            policy.threshold = threshold;
        }
        if let Some(enable) = self.enable {
            policy.enable = enable;
        }
        if let Some(when) = &self.when {
            match when {
                Some(patch) => patch.apply_to(&mut policy.when, ContextPolicyWhen::default()),
                None => policy.when = ContextPolicyWhen::default(),
            }
        }
    }
}

/// Presence-aware patch for a context-policy condition.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ContextPolicyWhenPatch {
    /// Replacement point; null restores the action-specific default point.
    #[serde(deserialize_with = "present_option")]
    at: Option<Option<ContextPolicyPoint>>,
    /// Replacement status matcher; null accepts every phase.
    #[serde(deserialize_with = "present_optional_nonempty_statuses")]
    statuses: Option<Option<Vec<tau_proto::AgentWorkStatusPhase>>>,
}

impl ContextPolicyWhenPatch {
    /// Applies fields from this patch, resolving reset values from `default`.
    fn apply_to(&self, when: &mut ContextPolicyWhen, default: ContextPolicyWhen) {
        if let Some(at) = self.at {
            when.at = at.unwrap_or(default.at);
        }
        if let Some(statuses) = &self.statuses {
            when.statuses.clone_from(statuses);
        }
    }
}

fn present_optional_nonempty_statuses<'de, D>(
    deserializer: D,
) -> Result<Option<Option<Vec<tau_proto::AgentWorkStatusPhase>>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let statuses = Option::<Vec<tau_proto::AgentWorkStatusPhase>>::deserialize(deserializer)?;
    if statuses.as_ref().is_some_and(Vec::is_empty) {
        return Err(D::Error::custom(
            "context policy statuses must be null or a nonempty list",
        ));
    }
    Ok(Some(statuses))
}

/// Positive provider-input-token policy threshold for a context-size alert.
///
/// Layer merging privately retains `None` for a missing or authored-zero
/// threshold. Final settings validation must reject that marker before any
/// effective alert reaches serialization or runtime policy evaluation. Delaying
/// rejection preserves the role-and-alert-specific diagnostics produced after
/// all config layers have been applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextSizeAlertThreshold(Option<NonZeroU64>);

impl ContextSizeAlertThreshold {
    /// Creates a positive alert threshold.
    #[must_use]
    pub const fn new(tokens: u64) -> Option<Self> {
        match NonZeroU64::new(tokens) {
            Some(tokens) => Some(Self(Some(tokens))),
            None => None,
        }
    }

    /// Returns the positive threshold as a raw token count for config display.
    #[must_use]
    pub const fn get(self) -> u64 {
        match self.0 {
            Some(tokens) => tokens.get(),
            None => panic!("incomplete context-size alert escaped validation"),
        }
    }

    /// Returns whether provider-reported input usage strictly exceeds this
    /// policy.
    #[must_use]
    pub fn is_exceeded_by(self, input_tokens: tau_proto::TokenCount) -> bool {
        input_tokens.get() > self.get()
    }

    /// Creates the private invalid marker used only while merging raw patches.
    const fn merge_seed() -> Self {
        Self(None)
    }

    /// Returns whether layered configuration supplied a positive threshold.
    ///
    /// Every public effective alert map must check this before it can leave the
    /// settings loader.
    const fn is_valid(self) -> bool {
        self.0.is_some()
    }
}

impl Serialize for ContextSizeAlertThreshold {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.get())
    }
}

impl<'de> Deserialize<'de> for ContextSizeAlertThreshold {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let threshold = u64::deserialize(deserializer)?;
        Self::new(threshold)
            .ok_or_else(|| D::Error::custom("context-size alert threshold must be positive"))
    }
}

/// Effective configuration for one named context-size alert.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSizeAlert {
    /// Provider-reported input-token count above which the alert fires.
    pub threshold: ContextSizeAlertThreshold,
    /// Whether this alert is active. Defaults to `true`.
    #[serde(default = "context_size_alert_enabled_default")]
    pub enable: bool,
    /// Internal prompt injected when the threshold is crossed.
    #[serde(
        default = "context_size_alert_message_default",
        deserialize_with = "deserialize_nonempty_context_size_alert_message"
    )]
    pub message: String,
    /// Lifecycle and logical-status selector.
    #[serde(default = "context_size_alert_when_default")]
    pub when: ContextPolicyWhen,
}

impl ContextSizeAlert {
    /// Creates an incomplete private merge seed. Effective configuration is
    /// validated before this value can leave the settings loader.
    fn merge_seed() -> Self {
        Self {
            threshold: ContextSizeAlertThreshold::merge_seed(),
            enable: true,
            message: context_size_alert_message_default(),
            when: context_size_alert_when_default(),
        }
    }
}

fn context_size_alert_when_default() -> ContextPolicyWhen {
    ContextPolicyWhen {
        at: ContextPolicyPoint::AfterResponse,
        statuses: None,
    }
}

fn context_size_alert_enabled_default() -> bool {
    true
}

fn context_size_alert_message_default() -> String {
    DEFAULT_CONTEXT_SIZE_ALERT_MESSAGE.to_owned()
}

fn deserialize_nonempty_context_size_alert_message<'de, D>(
    deserializer: D,
) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let message = String::deserialize(deserializer)?;
    if message.is_empty() {
        return Err(D::Error::custom(
            "context-size alert message must not be empty",
        ));
    }
    Ok(message)
}

/// Partial field update for one named alert during layered config merging.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ContextSizeAlertPatch {
    /// Replacement threshold when the current layer specifies one.
    threshold: Option<u64>,
    /// Replacement enablement when the current layer specifies one.
    enable: Option<bool>,
    /// Replacement prompt message when the current layer specifies one.
    message: Option<String>,
    /// Nested condition patch; null resets this alert to after-response/any.
    #[serde(deserialize_with = "present_option")]
    when: Option<Option<ContextPolicyWhenPatch>>,
}

impl ContextSizeAlertPatch {
    /// Applies every field present in this layer to an alert merge value.
    fn apply_to(&self, alert: &mut ContextSizeAlert) {
        if let Some(threshold) = self.threshold {
            // Zero deliberately becomes the same private invalid marker as an
            // omitted threshold. Post-merge validation then reports the fully
            // resolved role/global path instead of a layer-local serde error.
            alert.threshold = ContextSizeAlertThreshold::new(threshold)
                .unwrap_or_else(ContextSizeAlertThreshold::merge_seed);
        }
        if let Some(enable) = self.enable {
            alert.enable = enable;
        }
        if let Some(message) = &self.message {
            alert.message.clone_from(message);
        }
        if let Some(when) = &self.when {
            match when {
                Some(patch) => patch.apply_to(&mut alert.when, context_size_alert_when_default()),
                None => alert.when = context_size_alert_when_default(),
            }
        }
    }
}

fn apply_compaction_policy_patches(
    policies: &mut BTreeMap<String, CompactionPolicy>,
    patches: &BTreeMap<String, CompactionPolicyPatch>,
) {
    for (name, patch) in patches {
        let policy = policies
            .entry(name.clone())
            .or_insert_with(CompactionPolicy::merge_seed);
        patch.apply_to(policy);
    }
}

fn apply_context_size_alert_patches(
    alerts: &mut BTreeMap<String, ContextSizeAlert>,
    patches: &BTreeMap<String, ContextSizeAlertPatch>,
) {
    for (name, patch) in patches {
        let alert = alerts
            .entry(name.clone())
            .or_insert_with(ContextSizeAlert::merge_seed);
        patch.apply_to(alert);
    }
}

impl AgentRole {
    /// Normalizes one legacy singular policy into its standalone and inference
    /// successors while retaining the legacy field for compatibility callers.
    fn apply_legacy_compaction(&mut self, legacy: Option<RoleCompaction>) {
        self.compaction = legacy;
        let normalized = legacy.unwrap_or(RoleCompaction::ProviderDefault);
        self.inference_compaction = Some(normalized);
        self.compactions.clear();
        if normalized != RoleCompaction::Disabled {
            self.compactions.insert(
                "default".to_owned(),
                CompactionPolicy {
                    threshold: match normalized {
                        RoleCompaction::ProviderDefault => {
                            CompactionPolicyThreshold::ProviderDefault
                        }
                        RoleCompaction::Disabled => unreachable!("disabled policy is not inserted"),
                        RoleCompaction::Threshold(tokens) => {
                            CompactionPolicyThreshold::Tokens(tokens)
                        }
                        RoleCompaction::Reserve(tokens) => {
                            CompactionPolicyThreshold::Reserve(tokens)
                        }
                    },
                    enable: true,
                    when: ContextPolicyWhen::default(),
                },
            );
        }
    }

    fn apply_patch(&mut self, patch: &AgentRolePatch) {
        if let Some(enable) = patch.enable {
            self.enable = enable;
        }
        if let Some(visible) = patch.visible {
            self.visible = visible;
        }
        if let Some(order) = patch.order {
            self.order = order;
        }
        if let Some(inter_session_receiver) = patch.inter_session_receiver {
            self.inter_session_receiver = inter_session_receiver;
        }
        if let Some(inter_session_auto_start) = patch.inter_session_auto_start {
            self.inter_session_auto_start = inter_session_auto_start;
        }
        if let Some(description) = &patch.description {
            self.description = description.clone();
        }
        if let Some(model) = &patch.model {
            self.model = model.clone();
        }
        if let Some(effort) = patch.effort {
            self.effort = effort
                .map(|setting| setting.resolve(self.effort.unwrap_or(tau_proto::Effort::Medium)));
        }
        if let Some(verbosity) = patch.verbosity {
            self.verbosity = verbosity.map(|setting| {
                setting.resolve(self.verbosity.unwrap_or(tau_proto::Verbosity::Medium))
            });
        }
        if let Some(thinking_summary) = patch.thinking_summary {
            self.thinking_summary = thinking_summary.map(|setting| {
                setting.resolve(
                    self.thinking_summary
                        .unwrap_or(tau_proto::ThinkingSummary::Auto),
                )
            });
        }
        if let Some(service_tier) = patch.service_tier {
            self.service_tier = service_tier;
        }
        if let Some(legacy_compaction) = patch.compaction {
            self.apply_legacy_compaction(legacy_compaction);
        }
        if let Some(compaction) = patch.inference_compaction {
            self.inference_compaction = Some(compaction.unwrap_or(RoleCompaction::ProviderDefault));
        }
        apply_compaction_policy_patches(&mut self.compactions, &patch.compactions);
        apply_context_size_alert_patches(&mut self.context_size_alerts, &patch.context_size_alerts);
        if let Some(prompt_fragments) = &patch.prompt_fragments {
            for prompt_fragment in prompt_fragments {
                if !self.prompt_fragments.contains(prompt_fragment) {
                    self.prompt_fragments.push(prompt_fragment.clone());
                }
            }
        }
        if let Some(prompt_override) = &patch.prompt_override {
            self.prompt_override = prompt_override.clone();
        }
        if let Some(tools) = &patch.tools {
            self.tools = tools.clone();
        }
        if let Some(disable_tool_tags) = &patch.disable_tool_tags {
            self.disable_tool_tags = disable_tool_tags.clone();
        }
        if let Some(enable_tool_tags) = &patch.enable_tool_tags {
            self.enable_tool_tags = enable_tool_tags.clone();
        }
        if let Some(disable_tool_groups) = &patch.disable_tool_groups {
            self.disable_tool_groups = disable_tool_groups.clone();
        }
        if let Some(enable_tool_groups) = &patch.enable_tool_groups {
            self.enable_tool_groups = enable_tool_groups.clone();
        }
        if let Some(disable_tools) = &patch.disable_tools {
            self.disable_tools = disable_tools.clone();
        }
        if let Some(enable_tools) = &patch.enable_tools {
            self.enable_tools = enable_tools.clone();
        }
        if let Some(web_tools) = &patch.web_tools {
            self.web_tools.apply_patch(web_tools);
        }
        if let Some(required_skills) = &patch.required_skills {
            for skill in required_skills {
                if !self.required_skills.contains(skill) {
                    self.required_skills.push(skill.clone());
                }
            }
        }
    }
}

/// One prompt fragment configured on a harness role.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RolePromptFragment {
    /// Stable fragment name, preferably namespaced by role or purpose.
    pub name: String,
    /// Priority controlling placement among all prompt fragments. Lower values
    /// render earlier. Values below 100 are intended for role/persona
    /// instructions that should precede generated context; high values are for
    /// epilogue-style context such as the current working directory.
    pub priority: PromptPriority,
    /// Handlebars template text rendered into the system prompt.
    pub text: PromptContent,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Errors from settings loading.
#[derive(Debug)]
pub enum SettingsError {
    /// Error reported by the layered `config` crate or serde conversion.
    Config(config::ConfigError),
    /// A role name appeared in more than one role group.
    DuplicateGroupedRole {
        /// Duplicated role name.
        role: String,
        /// First group that contained the role.
        first_group: String,
        /// Later group that attempted to contain the same role.
        second_group: String,
    },
    /// An inter-session auto-start role lacks receiver authority.
    InvalidInterSessionAutoStart {
        /// Incoherently configured role name.
        role: String,
    },
    /// A command-line role override named a role absent from effective config.
    UnknownRoleCliOverride(String),
    /// The requested configuration profile is not present in effective files.
    UnknownProfile(ProfileName),
    /// A profile names an extension absent from base configuration and
    /// built-ins.
    UnknownProfileExtension {
        /// The selected profile containing the bad target.
        profile: ProfileName,
        /// The extension name that no base or built-in entry defines.
        extension: String,
    },
    /// A `--harness-config KEY=VALUE` override had invalid syntax, YAML, or
    /// conflicting legacy/canonical key spellings.
    InvalidHarnessConfigCliOverride(String),
    /// A startup-only provider alias graph contains a closed cycle.
    ProviderAliasCycle {
        /// Closed cycle path, with the first provider repeated at the end.
        path: Vec<ProviderName>,
    },
    /// A startup-only model alias graph contains a closed cycle.
    ModelAliasCycle {
        /// Closed cycle path, with the first model name repeated at the end.
        path: Vec<ModelName>,
    },
}
impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(source) => write!(f, "settings error: {source}"),
            Self::DuplicateGroupedRole {
                role,
                first_group,
                second_group,
            } => write!(
                f,
                "role `{role}` appears in multiple role_groups (`{first_group}` and `{second_group}`)"
            ),
            Self::UnknownRoleCliOverride(role) => {
                write!(f, "unknown role in CLI override: `{role}`")
            }
            Self::UnknownProfile(profile) => {
                write!(f, "unknown configuration profile: `{profile}`")
            }
            Self::UnknownProfileExtension { profile, extension } => write!(
                f,
                "configuration profile `{profile}` changes unknown extension `{extension}`"
            ),
            Self::InvalidInterSessionAutoStart { role } => write!(
                f,
                "role `{role}` enables `inter_session_auto_start` without `inter_session_receiver`"
            ),
            Self::InvalidHarnessConfigCliOverride(message) => {
                write!(f, "invalid harness config CLI override: {message}")
            }
            Self::ProviderAliasCycle { path } => write!(
                f,
                "provider alias cycle: {}",
                path.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" -> ")
            ),
            Self::ModelAliasCycle { path } => write!(
                f,
                "model alias cycle: {}",
                path.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" -> ")
            ),
        }
    }
}

impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(source) => Some(source),
            Self::DuplicateGroupedRole { .. }
            | Self::InvalidInterSessionAutoStart { .. }
            | Self::UnknownRoleCliOverride(_)
            | Self::UnknownProfile(_)
            | Self::UnknownProfileExtension { .. }
            | Self::InvalidHarnessConfigCliOverride(_)
            | Self::ProviderAliasCycle { .. }
            | Self::ModelAliasCycle { .. } => None,
        }
    }
}

/// Environment variable selecting an ordered harness configuration profile
/// stack.
pub const TAU_PROFILE_ENV: &str = "TAU_PROFILE";

/// One validated profile name inside an ordered configuration selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileName(String);

impl ProfileName {
    /// Parses a non-empty profile name so loader callers cannot silently bypass
    /// selection validation.
    pub fn parse(value: impl Into<String>) -> Result<Self, SettingsError> {
        let value = value.into();
        if value.is_empty() {
            return Err(SettingsError::Config(config::ConfigError::Message(
                "configuration profile must not be empty".to_owned(),
            )));
        }
        Ok(Self(value))
    }

    /// Returns the profile name as its configuration-map key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProfileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// An ordered, nonempty selection of named configuration profiles.
///
/// Each entry is applied after the preceding one. Selection syntax ignores
/// surrounding ASCII spaces and tabs, matching comma-separated extension
/// environment settings, but retains duplicate names and their order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileSelection {
    /// Nonempty profile names in their exact application order, including
    /// deliberate duplicates.
    profiles: Vec<ProfileName>,
}

impl ProfileSelection {
    /// Parses comma-separated nonempty profile names.
    ///
    /// It trims only ASCII spaces and tabs around every item. Empty segments
    /// and items containing only any Unicode whitespace are configuration
    /// errors.
    pub fn parse(value: impl Into<String>) -> Result<Self, SettingsError> {
        let value = value.into();
        let mut profiles = Vec::new();
        for (index, item) in value.split(',').enumerate() {
            let name = item.trim_matches([' ', '\t']);
            if name.is_empty() || name.chars().all(char::is_whitespace) {
                return Err(SettingsError::Config(config::ConfigError::Message(
                    format!(
                        "configuration profile item {} is empty; expected NAME[,NAME...]",
                        index + 1
                    ),
                )));
            }
            profiles.push(ProfileName::parse(name)?);
        }
        Ok(Self { profiles })
    }

    /// Returns the selected profile names in application order.
    #[must_use]
    pub fn names(&self) -> &[ProfileName] {
        &self.profiles
    }
}

impl TryFrom<ProfileName> for ProfileSelection {
    type Error = SettingsError;

    /// Converts one canonical profile name into a one-item selection.
    ///
    /// This rejects a name that list parsing would split or trim, so a
    /// selection constructed in-process has the same identity when it crosses
    /// the daemon's [`TAU_PROFILE_ENV`] transport.
    fn try_from(profile: ProfileName) -> Result<Self, Self::Error> {
        let profile_name = profile.to_string();
        let selection = Self::parse(profile_name.clone())?;
        if selection.names().len() != 1 || selection.names()[0].as_str() != profile_name {
            return Err(SettingsError::Config(config::ConfigError::Message(
                "configuration profile must be one canonical selection item".to_owned(),
            )));
        }
        Ok(selection)
    }
}

impl fmt::Display for ProfileSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, profile) in self.profiles.iter().enumerate() {
            if index != 0 {
                f.write_str(",")?;
            }
            profile.fmt(f)?;
        }
        Ok(())
    }
}

/// Resolves the profile selection from a CLI flag or [`TAU_PROFILE_ENV`].
///
/// An explicit CLI value wins over the environment, which both win over the
/// top-level `default_profile` from layered base configuration. Empty
/// selections fail so typoed shell expansions do not silently select no stack.
pub fn selected_profile(
    cli_profile: Option<&str>,
) -> Result<Option<ProfileSelection>, SettingsError> {
    selected_profile_in(&TauDirs::default(), cli_profile)
}

/// Resolves an ordered profile selection for one explicit directory layout.
///
/// This reads only base configuration while finding `default_profile`, so a
/// profile cannot influence which selection is applied.
pub fn selected_profile_in(
    dirs: &TauDirs,
    cli_profile: Option<&str>,
) -> Result<Option<ProfileSelection>, SettingsError> {
    selected_profile_in_from_sources(dirs, cli_profile, std::env::var_os(TAU_PROFILE_ENV))
}

/// Resolves an ordered profile selection from an explicit directory layout and
/// already-read
/// command-line and environment sources.
fn selected_profile_in_from_sources(
    dirs: &TauDirs,
    cli_profile: Option<&str>,
    environment_profile: Option<OsString>,
) -> Result<Option<ProfileSelection>, SettingsError> {
    match selected_profile_from_sources(cli_profile, environment_profile)? {
        Some(profile) => Ok(Some(profile)),
        None => default_profile_in(dirs)
            .and_then(|profile| profile.map(ProfileSelection::try_from).transpose()),
    }
}

/// Resolves a profile from already-read CLI and environment sources.
fn selected_profile_from_sources(
    cli_profile: Option<&str>,
    environment_profile: Option<OsString>,
) -> Result<Option<ProfileSelection>, SettingsError> {
    let profile = match cli_profile {
        Some(profile) => Some(profile.to_owned()),
        None => environment_profile
            .map(|profile| {
                profile.into_string().map_err(|_| {
                    SettingsError::Config(config::ConfigError::Message(format!(
                        "{TAU_PROFILE_ENV} must contain valid UTF-8"
                    )))
                })
            })
            .transpose()?,
    };
    match profile {
        Some(profile) => ProfileSelection::parse(profile).map(Some),
        None => Ok(None),
    }
}

impl From<config::ConfigError> for SettingsError {
    fn from(source: config::ConfigError) -> Self {
        Self::Config(source)
    }
}

/// Returns the default tau config directory (`~/.config/tau`).
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("tau"))
}

/// Returns the default tau state directory (`~/.local/state/tau`).
#[must_use]
pub fn state_dir() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|d| d.join("tau"))
}

/// Returns the per-session storage root inside `state_dir`. Each
/// session lives in its own directory at
/// `<state_dir>/sessions/<session_id>/`; grouping them under a
/// dedicated subdirectory keeps the state dir's top level reserved
/// for tau-wide scalar state such as `cli.json`.
#[must_use]
pub fn sessions_dir_of(state_dir: &Path) -> PathBuf {
    state_dir.join("sessions")
}

/// Returns the persistent state directory reserved for one extension.
///
/// The harness passes this path to the extension in
/// [`tau_proto::Configure::state_dir`]. Extension names come from the resolved
/// harness configuration, including user-authored `harness.yaml` keys, so only
/// conservative names are accepted before joining under `state/ext/`: names
/// must be safe as one path component and unambiguous in dotted harness config
/// override paths.
pub fn extension_state_dir_of(
    state_dir: &Path,
    extension_name: &str,
) -> Result<PathBuf, InvalidExtensionName> {
    validate_extension_name(extension_name)?;
    Ok(state_dir.join("ext").join(extension_name))
}

/// Returns the harness-mediated durable secret root reserved for one configured
/// extension instance.
///
/// The harness never sends this path to the extension. The configured instance
/// name is validated before it becomes one path component.
pub fn extension_secret_dir_of(
    state_dir: &Path,
    extension_name: &str,
) -> Result<PathBuf, InvalidExtensionName> {
    validate_extension_name(extension_name)?;
    Ok(state_dir.join("secrets").join("ext").join(extension_name))
}

/// Returns the CLI-owned provider settings root for one configured extension
/// instance.
pub fn extension_provider_settings_dir_of(
    state_dir: &Path,
    extension_name: &str,
) -> Result<PathBuf, InvalidExtensionName> {
    validate_extension_name(extension_name)?;
    Ok(state_dir.join("providers").join(extension_name))
}

/// Returns the read-only portable provider profile root for one configured
/// extension instance.
pub fn extension_provider_config_dir_of(
    config_dir: &Path,
    extension_name: &str,
) -> Result<PathBuf, InvalidExtensionName> {
    validate_extension_name(extension_name)?;
    Ok(config_dir.join("providers").join(extension_name))
}

/// Validates that an extension name is safe to use as a single path component
/// in harness-owned per-extension paths and as an unambiguous segment in
/// dotted harness config override paths. Valid names contain only ASCII
/// letters, digits, `_`, and `-`.
pub fn validate_extension_name(extension_name: &str) -> Result<(), InvalidExtensionName> {
    if extension_name.len() > tau_proto::EXTENSION_NAME_MAX_BYTES {
        return Err(InvalidExtensionName {
            name: extension_name.to_owned(),
            reason: "extension name must be at most 128 ASCII bytes",
        });
    }
    if tau_proto::ExtensionName::parse(extension_name.to_owned()).is_err() {
        return Err(InvalidExtensionName {
            name: extension_name.to_owned(),
            reason: "extension name must contain 1-128 ASCII letters, digits, '_' or '-'",
        });
    }
    Ok(())
}

/// Error returned when a configured extension name is unsafe as a state
/// directory path component or ambiguous in dotted config override paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidExtensionName {
    name: String,
    reason: &'static str,
}

impl fmt::Display for InvalidExtensionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid extension name `{}` for harness path/config key segment: {}",
            self.name, self.reason
        )
    }
}

impl std::error::Error for InvalidExtensionName {}

/// Returns the default tau per-session storage root
/// (`~/.local/state/tau/sessions`).
#[must_use]
pub fn sessions_dir() -> Option<PathBuf> {
    state_dir().map(|d| sessions_dir_of(&d))
}

/// Overridable directory layout for tau. Use the defaults (`Self::default()`)
/// for normal user runs or construct explicit paths for tests and custom
/// installations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TauDirs {
    /// Where to look for `cli.yaml`, `harness.yaml`, etc.
    pub config_dir: Option<PathBuf>,
    /// Where to read/write runtime state like persisted role settings.
    pub state_dir: Option<PathBuf>,
}

impl Default for TauDirs {
    fn default() -> Self {
        Self {
            config_dir: config_dir(),
            state_dir: state_dir(),
        }
    }
}

/// Testing-only settings loaded from `testing.yaml`.
///
/// This file is intentionally separate from normal `harness.yaml` settings
/// because it controls whether local development helpers may copy provider
/// credentials into scratch environments used by E2E tests.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TestingSettings {
    /// Exact extension-instance and provider pairs that `tau dev tmux start`
    /// may copy into its scratch Tau state directory.
    #[serde(default)]
    pub testing_providers: Vec<TestingProvider>,
}

/// One exact provider registration allowed in a development scratch harness.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(deny_unknown_fields)]
pub struct TestingProvider {
    /// Stable configured provider-extension instance name.
    pub extension: tau_proto::ExtensionName,
    /// Provider namespace inside that instance.
    pub provider: ProviderName,
}

/// Loads CLI settings from `cli.yaml` with `cli.d/*.yaml` overrides.
pub fn load_cli_settings() -> Result<CliSettings, SettingsError> {
    load_cli_settings_in(&TauDirs::default())
}

/// Like [`load_cli_settings`] but reads from an explicit directory layout.
///
/// The embedded `built-in.cli.yaml` is layered underneath the user's
/// own `cli.yaml` (and any `cli.d/*.yaml` drop-ins), so the user
/// can write a partial file and unmentioned fields fall back to the
/// shipped defaults. The `completions` and `bind` maps are merged
/// per-key on top so customizing one prefix or chord does not remove
/// the built-ins.
pub fn load_cli_settings_in(dirs: &TauDirs) -> Result<CliSettings, SettingsError> {
    let mut settings: CliSettings =
        load_yaml_layered_with_builtin(BUILT_IN_CLI_YAML, dirs.config_dir.as_deref(), "cli")?;
    let mut completions = CliSettings::built_in().completions;
    completions.extend(settings.completions);
    settings.completions = completions;
    let mut bindings = default_cli_bindings();
    bindings.extend(settings.bind);
    settings.bind = bindings;
    Ok(settings)
}

/// Loads optional testing settings from `testing.yaml`.
///
/// Missing files return `Ok(None)` so callers can keep safe defaults while
/// warning users that provider access has not been explicitly configured.
///
/// # Errors
///
/// Returns an error when `testing.yaml` exists but cannot be read, is not a
/// regular file, or does not parse as valid testing settings.
pub fn load_testing_settings(dirs: &TauDirs) -> Result<Option<TestingSettings>, SettingsError> {
    let Some(dir) = dirs.config_dir.as_deref() else {
        return Ok(None);
    };
    let path = dir.join("testing.yaml");
    let Some(metadata) = std::fs::metadata(&path).map(Some).or_else(|err| {
        if err.kind() == path_std_io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(SettingsError::Config(config::ConfigError::Message(
                format!("failed to inspect {}: {err}", path.display()),
            )))
        }
    })?
    else {
        return Ok(None);
    };
    if !metadata.is_file() {
        return Err(SettingsError::Config(config::ConfigError::Message(
            format!("{} exists but is not a regular file", path.display()),
        )));
    }
    let settings = config::Config::builder()
        .add_source(config::File::from(path).required(true))
        .build()?
        .try_deserialize::<TestingSettings>()?;
    Ok(Some(settings))
}

/// Loads a configured fallback profile, if any, over `harness.yaml` and
/// `harness.d/*.yaml` overrides.
pub fn load_harness_settings() -> Result<HarnessSettings, SettingsError> {
    load_harness_settings_in(&TauDirs::default())
}

/// Like [`load_harness_settings`] but reads from an explicit directory layout.
pub fn load_harness_settings_in(dirs: &TauDirs) -> Result<HarnessSettings, SettingsError> {
    load_harness_settings_with_cli_overrides_in(dirs, &[], &[])
}

/// Like [`load_harness_settings_in`], then applies role CLI overrides in order.
pub fn load_harness_settings_with_role_overrides_in(
    dirs: &TauDirs,
    role_overrides: &[RoleCliOverride],
) -> Result<HarnessSettings, SettingsError> {
    load_harness_settings_with_cli_overrides_in(dirs, role_overrides, &[])
}

/// Like [`load_harness_settings_in`], then applies role and harness config CLI
/// overrides in order. Harness config overrides are layered last and use normal
/// dotted config paths such as
/// `extensions.core-shell.config.working_directory`.
pub fn load_harness_settings_with_cli_overrides_in(
    dirs: &TauDirs,
    role_overrides: &[RoleCliOverride],
    harness_config_overrides: &[HarnessConfigCliOverride],
) -> Result<HarnessSettings, SettingsError> {
    let profile = default_profile_in(dirs)
        .and_then(|profile| profile.map(ProfileSelection::try_from).transpose())?;
    load_harness_settings_with_profile_and_cli_overrides_in(
        dirs,
        profile.as_ref(),
        role_overrides,
        harness_config_overrides,
    )
}

/// Reads the top-level profile fallback from built-in, user, and drop-in base
/// layers without applying any profile or command-line override.
///
/// An absent or explicit-null value selects no profile. A nonempty value names
/// one profile after trimming surrounding ASCII spaces/tabs. It rejects commas
/// so its exact one-name selection round-trips through [`TAU_PROFILE_ENV`].
pub fn default_profile_in(dirs: &TauDirs) -> Result<Option<ProfileName>, SettingsError> {
    let fallback: HarnessDefaultProfile = load_yaml_layered_with_builtin_and_harness_overrides(
        BUILT_IN_HARNESS_YAML,
        dirs.config_dir.as_deref(),
        "harness",
        &[],
        &[],
    )?;
    fallback
        .default_profile
        .map(parse_default_profile_name)
        .transpose()
}

/// Parses one base-configured fallback profile without accepting list syntax.
fn parse_default_profile_name(value: String) -> Result<ProfileName, SettingsError> {
    let name = value.trim_matches([' ', '\t']);
    if name.contains(',') || name.chars().all(char::is_whitespace) {
        return Err(SettingsError::Config(config::ConfigError::Message(
            "default_profile must name exactly one profile".to_owned(),
        )));
    }
    ProfileName::parse(name)
}

/// Loads settings with an optional explicitly supplied profile selection after
/// file layers and before command-line configuration and role overrides.
///
/// Callers that select profiles through process environment should resolve that
/// selection with [`selected_profile_in`] for the same directory layout and
/// pass the result here. This explicit boundary keeps deterministic callers
/// independent from ambient environment; `None` deliberately loads only base
/// layers for profile validation.
pub fn load_harness_settings_with_profile_and_cli_overrides_in(
    dirs: &TauDirs,
    profile_selection: Option<&ProfileSelection>,
    role_overrides: &[RoleCliOverride],
    harness_config_overrides: &[HarnessConfigCliOverride],
) -> Result<HarnessSettings, SettingsError> {
    let profiles = match profile_selection {
        Some(selection) => load_harness_profile_layers(dirs, selection)?,
        None => Vec::new(),
    };
    let aliases: HarnessAliasesWire = load_yaml_layered_with_builtin_and_harness_overrides(
        BUILT_IN_HARNESS_YAML,
        dirs.config_dir.as_deref(),
        "harness",
        &profiles,
        harness_config_overrides,
    )?;
    let mut settings: HarnessSettings = load_yaml_layered_with_builtin_and_harness_overrides(
        BUILT_IN_HARNESS_YAML,
        dirs.config_dir.as_deref(),
        "harness",
        &profiles,
        harness_config_overrides,
    )?;

    // Generic YAML layering replaces arrays, but role metadata is additive.
    // Recompute it from raw source patches. Source order still controls patches
    // at one scope, while scope precedence controls the result across sources:
    // all agent defaults, then all group defaults, then all role overrides.
    let mut role_settings = HarnessSettings::built_in();
    role_settings.roles.clear();
    role_settings.role_groups.clear();
    role_settings.prompt_fragments.clear();
    role_settings.required_skills.clear();
    role_settings.context_size_alerts.clear();

    let mut role_layers = vec![parse_built_in_yaml::<HarnessRoleOverrides>(
        "built-in.harness.yaml",
        BUILT_IN_HARNESS_YAML,
    )];
    role_layers.extend(load_yaml_layer_files::<HarnessRoleOverrides>(
        dirs.config_dir.as_deref(),
        "harness",
    )?);
    role_layers.extend(profiles.into_iter().map(|profile| HarnessRoleOverrides {
        agents: profile.agents.into(),
    }));
    role_layers.extend(harness_role_cli_override_layers(harness_config_overrides)?);
    for layer in &role_layers {
        validate_role_layer_compaction_inputs(layer)?;
    }

    let mut effective_agent_defaults = AgentRole {
        enable: Some(true),
        visible: Some(true),
        ..AgentRole::default()
    };
    for overrides in &role_layers {
        let agent_defaults = overrides.agents.role_defaults();
        effective_agent_defaults.apply_patch(&agent_defaults);
        role_settings.apply_prompt_fragment_overrides(overrides.agents.prompt_fragments.clone());
        role_settings.apply_required_skill_overrides(overrides.agents.required_skills.clone());
        role_settings
            .apply_context_size_alert_overrides(overrides.agents.context_size_alerts.clone());
    }
    for overrides in &role_layers {
        role_settings
            .apply_role_group_members(&overrides.agents.role_groups, &effective_agent_defaults)?;
    }
    for overrides in &role_layers {
        role_settings.apply_role_group_defaults(&overrides.agents.role_groups);
    }
    for overrides in &role_layers {
        role_settings.apply_role_overrides(&overrides.agents.role_groups);
    }
    role_settings.apply_role_cli_overrides(role_overrides)?;
    role_settings.remove_disabled_roles();
    role_settings.validate_inter_session_roles()?;
    role_settings.apply_agent_globals_to_roles();
    role_settings.validate_context_size_alerts()?;
    role_settings.validate_web_tools()?;
    settings.prompt_fragments = role_settings.prompt_fragments;
    settings.required_skills = role_settings.required_skills;
    settings.context_size_alerts = role_settings.context_size_alerts;
    settings.roles = role_settings.roles;
    settings.role_groups = role_settings.role_groups;
    resolve_model_references(&mut settings, &aliases.aliases)?;
    Ok(settings)
}

/// Resolves every effective static role model and validates the complete alias
/// graphs, including entries that no role currently uses.
fn resolve_model_references(
    settings: &mut HarnessSettings,
    aliases: &ModelReferenceAliases,
) -> Result<(), SettingsError> {
    validate_alias_graph(&aliases.providers)
        .map_err(|path| SettingsError::ProviderAliasCycle { path })?;
    validate_alias_graph(&aliases.models)
        .map_err(|path| SettingsError::ModelAliasCycle { path })?;
    for role in settings.roles.values_mut() {
        let Some(model) = role.model.take() else {
            continue;
        };
        role.model = Some(ModelId::new(
            resolve_alias(&aliases.providers, &model.provider),
            resolve_alias(&aliases.models, &model.model),
        ));
    }
    Ok(())
}

/// Validates one complete alias graph so unused cycles cannot remain latent.
fn validate_alias_graph<T>(aliases: &BTreeMap<T, T>) -> Result<(), Vec<T>>
where
    T: Clone + Ord,
{
    for start in aliases.keys() {
        let mut path = Vec::new();
        let mut current = start;
        while let Some(next) = aliases.get(current) {
            if next == current {
                break;
            }
            if let Some(position) = path.iter().position(|seen| seen == current) {
                let mut cycle = path[position..].to_vec();
                cycle.push(current.clone());
                return Err(cycle);
            }
            path.push(current.clone());
            current = next;
        }
    }
    Ok(())
}

/// Follows one already-validated alias chain, treating an identity edge as an
/// explicit terminal reset.
fn resolve_alias<T>(aliases: &BTreeMap<T, T>, start: &T) -> T
where
    T: Clone + Ord,
{
    let mut current = start;
    while let Some(next) = aliases.get(current) {
        if next == current {
            break;
        }
        current = next;
    }
    current.clone()
}

fn validate_role_layer_compaction_inputs(
    layer: &HarnessRoleOverrides,
) -> Result<(), SettingsError> {
    layer
        .agents
        .role_defaults()
        .validate_compaction_input("agents")?;
    for (group_name, group) in &layer.agents.role_groups {
        group
            .defaults()
            .validate_compaction_input(&format!("agents.role_groups.{group_name}"))?;
        for (role_name, patch) in &group.roles {
            patch.validate_compaction_input(&format!(
                "agents.role_groups.{group_name}.roles.{role_name}"
            ))?;
        }
    }
    Ok(())
}

// Legacy harness aliases are accepted for user compatibility, but every alias
// must be handled in all three places that can see user-authored keys:
// serde aliases on patch structs, JSON layer normalization below, and dotted
// `--harness-config` key canonicalization. Keep the regression tests for the
// file-layer and CLI alias tables in sync when adding or renaming fields.
fn normalize_alias_key(
    map: &mut serde_json::Map<String, serde_json::Value>,
    alias: &str,
    canonical: &str,
    source: &str,
    path: &str,
) -> Result<(), SettingsError> {
    if map.contains_key(alias) && map.contains_key(canonical) {
        return Err(SettingsError::Config(config::ConfigError::Message(
            format!(
                "{source}: both legacy key `{path}.{alias}` and canonical key `{path}.{canonical}` are set"
            ),
        )));
    }
    if let Some(value) = map.remove(alias) {
        map.entry(canonical.to_owned()).or_insert(value);
    }
    Ok(())
}

fn normalize_role_config_keys(
    value: &mut serde_json::Value,
    source: &str,
    path: &str,
) -> Result<(), SettingsError> {
    let serde_json::Value::Object(map) = value else {
        return Ok(());
    };
    normalize_alias_key(map, "enabled", "enable", source, path)?;
    normalize_alias_key(
        map,
        "interSessionReceiver",
        "inter_session_receiver",
        source,
        path,
    )?;
    normalize_alias_key(
        map,
        "interSessionAutoStart",
        "inter_session_auto_start",
        source,
        path,
    )?;
    normalize_alias_key(map, "thinkingSummary", "thinking_summary", source, path)?;
    normalize_alias_key(map, "serviceTier", "service_tier", source, path)?;
    normalize_alias_key(
        map,
        "inferenceCompaction",
        "inference_compaction",
        source,
        path,
    )?;
    normalize_alias_key(map, "promptFragments", "prompt_fragments", source, path)?;
    normalize_alias_key(map, "promptOverride", "prompt_override", source, path)?;
    normalize_alias_key(map, "disableToolTags", "disable_tool_tags", source, path)?;
    normalize_alias_key(map, "enableToolTags", "enable_tool_tags", source, path)?;
    normalize_alias_key(
        map,
        "disableToolGroups",
        "disable_tool_groups",
        source,
        path,
    )?;
    normalize_alias_key(map, "enableToolGroups", "enable_tool_groups", source, path)?;
    normalize_alias_key(map, "disableTools", "disable_tools", source, path)?;
    normalize_alias_key(map, "enableTools", "enable_tools", source, path)?;
    normalize_alias_key(map, "requiredSkills", "required_skills", source, path)?;
    normalize_alias_key(map, "webTools", "web_tools", source, path)?;
    if let Some(web_tools) = map.get_mut("web_tools") {
        normalize_web_tools_keys(web_tools, source, &format!("{path}.web_tools"))?;
    }
    normalize_alias_key(
        map,
        "contextSizeAlerts",
        "context_size_alerts",
        source,
        path,
    )?;
    normalize_context_policy_value(map.get_mut("when"));
    if let Some(serde_json::Value::Object(policies)) = map.get_mut("compactions") {
        for policy in policies.values_mut() {
            if let serde_json::Value::Object(policy) = policy {
                normalize_context_policy_value(policy.get_mut("when"));
            }
        }
    }
    if let Some(serde_json::Value::Object(alerts)) = map.get_mut("context_size_alerts") {
        for alert in alerts.values_mut() {
            if let serde_json::Value::Object(alert) = alert {
                normalize_context_policy_value(alert.get_mut("when"));
            }
        }
    }
    Ok(())
}

/// Normalize nested logical-web aliases while rejecting duplicate spellings.
fn normalize_web_tools_keys(
    value: &mut serde_json::Value,
    source: &str,
    path: &str,
) -> Result<(), SettingsError> {
    let serde_json::Value::Object(map) = value else {
        return Ok(());
    };
    normalize_alias_key(map, "allowedDomains", "allowed_domains", source, path)?;
    for logical in ["search", "fetch"] {
        let Some(serde_json::Value::Object(logical_map)) = map.get_mut(logical) else {
            continue;
        };
        let Some(serde_json::Value::Object(candidates)) = logical_map.get_mut("candidates") else {
            continue;
        };
        for (candidate_name, candidate) in candidates {
            if let serde_json::Value::Object(candidate_map) = candidate {
                normalize_alias_key(
                    candidate_map,
                    "contextSize",
                    "context_size",
                    source,
                    &format!("{path}.{logical}.candidates.{candidate_name}"),
                )?;
            }
        }
    }
    Ok(())
}

/// Canonicalizes the one supported camel-case lifecycle value before layering.
fn normalize_context_policy_value(value: Option<&mut serde_json::Value>) {
    let Some(serde_json::Value::Object(when)) = value else {
        return;
    };
    if when.get("at") == Some(&serde_json::Value::String("outerTurnFinished".to_owned())) {
        when.insert(
            "at".to_owned(),
            serde_json::Value::String("outer_turn_finished".to_owned()),
        );
    }
}

fn normalize_tool_policy_config_keys(
    value: &mut serde_json::Value,
    source: &str,
    path: &str,
) -> Result<(), SettingsError> {
    let serde_json::Value::Object(policy) = value else {
        return Ok(());
    };
    let Some(serde_json::Value::Object(rules)) = policy.get_mut("rules") else {
        return Ok(());
    };
    for (rule_name, rule) in rules {
        let serde_json::Value::Object(rule_map) = rule else {
            continue;
        };
        normalize_alias_key(
            rule_map,
            "enabled",
            "enable",
            source,
            &format!("{path}.rules.{rule_name}"),
        )?;
    }
    Ok(())
}

fn normalize_harness_config_value(
    value: &mut serde_json::Value,
    source: &str,
) -> Result<(), SettingsError> {
    let serde_json::Value::Object(map) = value else {
        return Ok(());
    };
    normalize_alias_key(map, "customPrompts", "custom_prompts", source, "root")?;
    normalize_alias_key(map, "toolPolicy", "tool_policy", source, "root")?;
    normalize_alias_key(
        map,
        "showIntroductionNotice",
        "show_introduction_notice",
        source,
        "root",
    )?;
    normalize_alias_key(
        map,
        "waitTimeoutMinimumMinutes",
        "wait_timeout_minimum_minutes",
        source,
        "root",
    )?;
    normalize_alias_key(
        map,
        "waitTimeoutMaximumMinutes",
        "wait_timeout_maximum_minutes",
        source,
        "root",
    )?;
    normalize_alias_key(
        map,
        "agentWatchRetryNotificationThreshold",
        "agent_watch_retry_notification_threshold",
        source,
        "root",
    )?;
    normalize_alias_key(
        map,
        "notificationDelivery",
        "notification_delivery",
        source,
        "root",
    )?;
    if let Some(serde_json::Value::Object(classes)) = map.get_mut("notification_delivery") {
        for (class, policy) in classes {
            if let serde_json::Value::Object(policy) = policy {
                let path = format!("notification_delivery.{class}");
                normalize_alias_key(policy, "idleMs", "idle_ms", source, &path)?;
                normalize_alias_key(policy, "waitAnyMs", "wait_any_ms", source, &path)?;
                normalize_alias_key(policy, "waitToolMs", "wait_tool_ms", source, &path)?;
            }
        }
    }
    if let Some(serde_json::Value::Object(extensions)) = map.get_mut("extensions") {
        for (extension_name, extension) in extensions {
            if let serde_json::Value::Object(extension) = extension {
                normalize_alias_key(
                    extension,
                    "toolPrefix",
                    "tool_prefix",
                    source,
                    &format!("extensions.{extension_name}"),
                )?;
            }
        }
    }
    if let Some(serde_json::Value::Object(profiles)) = map.get_mut("profiles") {
        for (profile_name, profile) in profiles {
            normalize_harness_config_value(
                profile,
                &format!("{source}, configuration profile `{profile_name}`"),
            )?;
        }
    }
    if let Some(tool_policy) = map.get_mut("tool_policy") {
        normalize_tool_policy_config_keys(tool_policy, source, "tool_policy")?;
    }
    if let Some(serde_json::Value::Object(agents)) = map.get_mut("agents") {
        normalize_alias_key(agents, "enabled", "enable", source, "agents")?;
        normalize_alias_key(agents, "defaultRole", "default_role", source, "agents")?;
        normalize_alias_key(agents, "idTemplate", "id_template", source, "agents")?;
        normalize_alias_key(
            agents,
            "displayNameTemplate",
            "display_name_template",
            source,
            "agents",
        )?;
        normalize_alias_key(
            agents,
            "promptFragments",
            "prompt_fragments",
            source,
            "agents",
        )?;
        normalize_alias_key(
            agents,
            "requiredSkills",
            "required_skills",
            source,
            "agents",
        )?;
        normalize_alias_key(
            agents,
            "contextSizeAlerts",
            "context_size_alerts",
            source,
            "agents",
        )?;
        normalize_alias_key(
            agents,
            "inferenceCompaction",
            "inference_compaction",
            source,
            "agents",
        )?;
        if let Some(serde_json::Value::Object(policies)) = agents.get_mut("compactions") {
            for policy in policies.values_mut() {
                if let serde_json::Value::Object(policy) = policy {
                    normalize_context_policy_value(policy.get_mut("when"));
                }
            }
        }
        if let Some(serde_json::Value::Object(alerts)) = agents.get_mut("context_size_alerts") {
            for alert in alerts.values_mut() {
                if let serde_json::Value::Object(alert) = alert {
                    normalize_context_policy_value(alert.get_mut("when"));
                }
            }
        }
        normalize_alias_key(
            agents,
            "thinkingSummary",
            "thinking_summary",
            source,
            "agents",
        )?;
        normalize_alias_key(agents, "serviceTier", "service_tier", source, "agents")?;
        normalize_alias_key(agents, "webTools", "web_tools", source, "agents")?;
        if let Some(web_tools) = agents.get_mut("web_tools") {
            normalize_web_tools_keys(web_tools, source, "agents.web_tools")?;
        }
        normalize_alias_key(agents, "roleGroups", "role_groups", source, "agents")?;
    }
    let Some(serde_json::Value::Object(agents)) = map.get_mut("agents") else {
        return Ok(());
    };
    if let Some(serde_json::Value::Object(role_groups)) = agents.get_mut("role_groups") {
        for (group_name, group) in role_groups {
            let group_path = format!("agents.role_groups.{group_name}");
            normalize_role_config_keys(group, source, &group_path)?;
            if let serde_json::Value::Object(group_map) = group
                && let Some(serde_json::Value::Object(roles)) = group_map.get_mut("roles")
            {
                for (role_name, role) in roles {
                    normalize_role_config_keys(
                        role,
                        source,
                        &format!("{group_path}.roles.{role_name}"),
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn load_yaml_layered_with_builtin_and_harness_overrides<T: for<'de> Deserialize<'de>>(
    built_in_text: &'static str,
    dir: Option<&Path>,
    name: &str,
    profiles: &[HarnessProfile],
    overrides: &[HarnessConfigCliOverride],
) -> Result<T, SettingsError> {
    let mut builder = config::Config::builder().add_source(normalized_harness_yaml_source(
        built_in_text,
        "built-in harness config",
    )?);
    for path in yaml_layer_paths(dir, name)? {
        let text = std::fs::read_to_string(&path).map_err(|err| {
            SettingsError::Config(config::ConfigError::Message(format!(
                "failed to read {}: {err}",
                path.display()
            )))
        })?;
        builder = builder.add_source(normalized_harness_yaml_source(
            &text,
            &format!("harness config {}", path.display()),
        )?);
    }
    for profile in profiles {
        builder = builder.add_source(profile_config_source(profile)?);
    }
    let normalized_overrides = normalized_harness_config_overrides(overrides)?;
    for override_ in &normalized_overrides {
        builder = builder.add_source(harness_config_override_source(override_)?);
    }
    let config = builder.build()?;
    let value: serde_json::Value = config.try_deserialize()?;
    serde_json::from_value(value)
        .map_err(|error| SettingsError::Config(config::ConfigError::Message(error.to_string())))
}

/// Serializes the intentionally small non-role-replay subset of a selected
/// profile.
fn profile_config_source(
    profile: &HarnessProfile,
) -> Result<config::File<config::FileSourceString, config::FileFormat>, SettingsError> {
    let extensions = profile
        .extensions
        .iter()
        .map(|(name, patch)| {
            let mut value = serde_json::Map::new();
            if let Some(enable) = patch.enable {
                value.insert("enable".to_owned(), serde_json::Value::Bool(enable));
            }
            if let Some(config) = &patch.config {
                value.insert("config".to_owned(), config.clone());
            }
            (name.clone(), serde_json::Value::Object(value))
        })
        .collect::<serde_json::Map<_, _>>();
    let mut values = serde_json::Map::new();
    if let Some(tau_state_access) = profile.tau_state_access {
        values.insert(
            "tau_state_access".to_owned(),
            serde_json::to_value(tau_state_access).map_err(|error| {
                SettingsError::Config(config::ConfigError::Message(format!(
                    "failed to serialize selected profile Tau-state access: {error}"
                )))
            })?,
        );
    }
    values.insert(
        "aliases".to_owned(),
        serde_json::to_value(&profile.aliases).map_err(|error| {
            SettingsError::Config(config::ConfigError::Message(format!(
                "failed to serialize selected profile aliases: {error}"
            )))
        })?,
    );
    values.insert(
        "extensions".to_owned(),
        serde_json::Value::Object(extensions),
    );
    if let Some(default_role) = &profile.agents.default_role {
        values.insert(
            "agents".to_owned(),
            serde_json::json!({ "default_role": default_role }),
        );
    }
    let value = serde_json::Value::Object(values);
    let yaml = serde_yaml_ng::to_string(&value).map_err(|error| {
        SettingsError::Config(config::ConfigError::Message(format!(
            "failed to serialize selected profile settings: {error}"
        )))
    })?;
    Ok(config::File::from_str(&yaml, config::FileFormat::Yaml).required(true))
}

/// Loads every selected profile from every built-in/user/drop-in source in
/// selection order.
///
/// Keeping these patches separate preserves additive role semantics and
/// relative provider-setting resolution across profile definitions.
fn load_harness_profile_layers(
    dirs: &TauDirs,
    selection: &ProfileSelection,
) -> Result<Vec<HarnessProfile>, SettingsError> {
    let mut profiles = Vec::new();
    for name in selection.names() {
        profiles.extend(load_harness_profile_layers_for_name(dirs, name)?);
    }
    Ok(profiles)
}

/// Loads one named profile from every built-in/user/drop-in source in order.
///
/// This deliberately replays a repeated name rather than sharing a parsed
/// patch: a duplicate selected profile is a later layer with normal merge
/// effects.
fn load_harness_profile_layers_for_name(
    dirs: &TauDirs,
    name: &ProfileName,
) -> Result<Vec<HarnessProfile>, SettingsError> {
    let mut profiles = Vec::new();
    let mut built_in =
        load_harness_profile_layer(BUILT_IN_HARNESS_YAML, "built-in harness config")?;
    if let Some(profile) = built_in.profiles.remove(name.as_str()) {
        profiles.push(profile);
    }
    for path in yaml_layer_paths(dirs.config_dir.as_deref(), "harness")? {
        let text = std::fs::read_to_string(&path).map_err(|err| {
            SettingsError::Config(config::ConfigError::Message(format!(
                "failed to read {}: {err}",
                path.display()
            )))
        })?;
        let mut layer =
            load_harness_profile_layer(&text, &format!("harness config {}", path.display()))?;
        if let Some(profile) = layer.profiles.remove(name.as_str()) {
            profiles.push(profile);
        }
    }
    if profiles.is_empty() {
        return Err(SettingsError::UnknownProfile(name.clone()));
    }
    Ok(profiles)
}

/// Returns every extension enablement target from the selected raw profile.
///
/// Harness startup validates these names against its built-in extension set and
/// the base configured extension table before it applies the profile.
pub fn profile_extension_names_in(
    dirs: &TauDirs,
    name: &ProfileName,
) -> Result<BTreeSet<String>, SettingsError> {
    Ok(load_harness_profile_layers_for_name(dirs, name)?
        .into_iter()
        .flat_map(|profile| profile.extensions.into_keys())
        .collect())
}

/// Parses the profile map from one normalized harness configuration source.
fn load_harness_profile_layer(
    text: &str,
    description: &str,
) -> Result<HarnessProfiles, SettingsError> {
    config::Config::builder()
        .add_source(normalized_harness_yaml_source(text, description)?)
        .build()?
        .try_deserialize()
        .map_err(SettingsError::from)
}

fn normalized_harness_yaml_source(
    text: &str,
    description: &str,
) -> Result<config::File<config::FileSourceString, config::FileFormat>, SettingsError> {
    let mut value: serde_json::Value = serde_yaml_ng::from_str(text).map_err(|err| {
        SettingsError::Config(config::ConfigError::Message(format!(
            "failed to parse {description}: {err}"
        )))
    })?;
    if value.is_null() {
        value = serde_json::Value::Object(serde_json::Map::new());
    }
    normalize_harness_config_value(&mut value, description)?;
    let normalized = serde_yaml_ng::to_string(&value).map_err(|err| {
        SettingsError::Config(config::ConfigError::Message(format!(
            "failed to normalize {description}: {err}"
        )))
    })?;
    Ok(config::File::from_str(&normalized, config::FileFormat::Yaml).required(true))
}

fn harness_role_cli_override_layers(
    overrides: &[HarnessConfigCliOverride],
) -> Result<Vec<HarnessRoleOverrides>, SettingsError> {
    let normalized_overrides = normalized_harness_config_overrides(overrides)?;
    let mut layers = Vec::new();
    for override_ in &normalized_overrides {
        let layer: HarnessRoleOverrides = config::Config::builder()
            .add_source(harness_config_override_source(override_)?)
            .build()?
            .try_deserialize()?;
        layers.push(layer);
    }
    Ok(layers)
}

fn harness_config_override_source(
    override_: &HarnessConfigCliOverride,
) -> Result<config::File<config::FileSourceString, config::FileFormat>, SettingsError> {
    let yaml: serde_json::Value = serde_yaml_ng::from_str(&override_.raw_value).map_err(|err| {
        SettingsError::InvalidHarnessConfigCliOverride(format!(
            "{}: failed to parse value as YAML: {err}",
            override_.key
        ))
    })?;
    let mut value = nested_harness_override_value(&override_.key, yaml);
    normalize_harness_config_value(&mut value, &format!("CLI override `{}`", override_.key))?;
    let normalized = serde_yaml_ng::to_string(&value).map_err(|err| {
        SettingsError::Config(config::ConfigError::Message(format!(
            "failed to normalize CLI override `{}`: {err}",
            override_.key
        )))
    })?;
    Ok(config::File::from_str(&normalized, config::FileFormat::Yaml).required(true))
}

fn nested_harness_override_value(key: &str, value: serde_json::Value) -> serde_json::Value {
    key.split('.').rev().fold(value, |value, key| {
        let mut map = serde_json::Map::new();
        map.insert(key.to_owned(), value);
        serde_json::Value::Object(map)
    })
}

fn normalized_harness_config_overrides(
    overrides: &[HarnessConfigCliOverride],
) -> Result<Vec<HarnessConfigCliOverride>, SettingsError> {
    let mut normalized = Vec::with_capacity(overrides.len());
    let mut seen = HashMap::<String, String>::new();
    for override_ in overrides {
        let key = normalize_harness_config_override_key(&override_.key);
        if let Some(previous) = seen.get(&key)
            && previous != &override_.key
        {
            return Err(SettingsError::InvalidHarnessConfigCliOverride(format!(
                "conflicting CLI override keys `{previous}` and `{}` both normalize to `{key}`",
                override_.key
            )));
        }
        seen.entry(key.clone())
            .or_insert_with(|| override_.key.clone());
        normalized.push(HarnessConfigCliOverride {
            key,
            raw_value: override_.raw_value.clone(),
        });
    }
    Ok(normalized)
}

fn normalize_harness_config_override_key(key: &str) -> String {
    let mut parts: Vec<&str> = key.split('.').collect();
    if parts.is_empty() {
        return key.to_owned();
    }

    parts[0] = canonical_top_level_key(parts[0]);
    if parts[0] == "extensions" && parts.len() > 2 && parts[2] == "toolPrefix" {
        parts[2] = "tool_prefix";
    }
    if parts[0] == "notification_delivery" && parts.len() > 2 {
        parts[2] = match parts[2] {
            "idleMs" => "idle_ms",
            "waitAnyMs" => "wait_any_ms",
            "waitToolMs" => "wait_tool_ms",
            key => key,
        };
    }
    if parts[0] == "agents" && parts.len() > 1 {
        parts[1] = canonical_agents_key(parts[1]);
        if parts[1] == "web_tools" {
            canonicalize_web_override_parts(&mut parts, 2);
        }
        if parts[1] == "role_groups" && parts.len() > 3 {
            if parts[3] == "roles" {
                if parts.len() > 5 {
                    parts[5] = canonical_role_key(parts[5]);
                    if parts[5] == "web_tools" {
                        canonicalize_web_override_parts(&mut parts, 6);
                    }
                }
            } else {
                parts[3] = canonical_role_key(parts[3]);
                if parts[3] == "web_tools" {
                    canonicalize_web_override_parts(&mut parts, 4);
                }
            }
        }
    }
    if parts[0] == "tool_policy" && parts.len() > 3 && parts[1] == "rules" {
        parts[3] = canonical_tool_policy_rule_key(parts[3]);
    }
    parts.join(".")
}

/// Canonicalize aliases below one `web_tools` CLI override segment.
fn canonicalize_web_override_parts(parts: &mut [&str], start: usize) {
    if parts.len() > start && parts[start] == "allowedDomains" {
        parts[start] = "allowed_domains";
    }
    if parts.len() > start + 3
        && matches!(parts[start], "search" | "fetch")
        && parts[start + 1] == "candidates"
        && parts[start + 3] == "contextSize"
    {
        parts[start + 3] = "context_size";
    }
}

fn canonical_top_level_key(key: &str) -> &str {
    match key {
        "customPrompts" => "custom_prompts",
        "toolPolicy" => "tool_policy",
        "showIntroductionNotice" => "show_introduction_notice",
        "waitTimeoutMinimumMinutes" => "wait_timeout_minimum_minutes",
        "waitTimeoutMaximumMinutes" => "wait_timeout_maximum_minutes",
        "agentWatchRetryNotificationThreshold" => "agent_watch_retry_notification_threshold",
        "notificationDelivery" => "notification_delivery",
        _ => key,
    }
}

fn canonical_agents_key(key: &str) -> &str {
    match key {
        "enabled" => "enable",
        "defaultRole" => "default_role",
        "idTemplate" => "id_template",
        "displayNameTemplate" => "display_name_template",
        "roleGroups" => "role_groups",
        "promptFragments" => "prompt_fragments",
        "requiredSkills" => "required_skills",
        "contextSizeAlerts" => "context_size_alerts",
        "webTools" => "web_tools",
        "thinkingSummary" => "thinking_summary",
        "serviceTier" => "service_tier",
        "inferenceCompaction" => "inference_compaction",
        _ => key,
    }
}

fn canonical_role_key(key: &str) -> &str {
    match key {
        "enabled" => "enable",
        "interSessionReceiver" => "inter_session_receiver",
        "interSessionAutoStart" => "inter_session_auto_start",
        "thinkingSummary" => "thinking_summary",
        "serviceTier" => "service_tier",
        "inferenceCompaction" => "inference_compaction",
        "promptFragments" => "prompt_fragments",
        "promptOverride" => "prompt_override",
        "enableToolGroups" => "enable_tool_groups",
        "disableToolGroups" => "disable_tool_groups",
        "enableToolTags" => "enable_tool_tags",
        "disableToolTags" => "disable_tool_tags",
        "enableTools" => "enable_tools",
        "disableTools" => "disable_tools",
        "requiredSkills" => "required_skills",
        "contextSizeAlerts" => "context_size_alerts",
        "webTools" => "web_tools",
        _ => key,
    }
}

fn canonical_tool_policy_rule_key(key: &str) -> &str {
    match key {
        "enabled" => "enable",
        _ => key,
    }
}

/// Stacks an embedded built-in YAML string underneath the user's files.
/// `T` therefore doesn't need a `Default` impl — the built-in layer always
/// supplies every required field.
fn load_yaml_layered_with_builtin<T: for<'de> Deserialize<'de>>(
    built_in_text: &'static str,
    dir: Option<&Path>,
    name: &str,
) -> Result<T, SettingsError> {
    let builder = config::Config::builder()
        .add_source(config::File::from_str(built_in_text, config::FileFormat::Yaml).required(true));
    let builder = add_yaml_file_sources(builder, dir, name)?;
    builder
        .build()?
        .try_deserialize()
        .map_err(SettingsError::from)
}

fn load_yaml_layer_files<T: for<'de> Deserialize<'de>>(
    dir: Option<&Path>,
    name: &str,
) -> Result<Vec<T>, SettingsError> {
    yaml_layer_paths(dir, name)?
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).map_err(|error| {
                SettingsError::Config(config::ConfigError::Message(format!(
                    "failed to read {}: {error}",
                    path.display()
                )))
            })?;
            config::Config::builder()
                .add_source(normalized_harness_yaml_source(
                    &text,
                    &path.display().to_string(),
                )?)
                .build()?
                .try_deserialize()
                .map_err(SettingsError::from)
        })
        .collect()
}

fn add_yaml_file_sources(
    mut builder: config::ConfigBuilder<config::builder::DefaultState>,
    dir: Option<&Path>,
    name: &str,
) -> Result<config::ConfigBuilder<config::builder::DefaultState>, SettingsError> {
    for path in yaml_layer_paths(dir, name)? {
        builder = builder.add_source(
            config::File::from(path)
                .format(config::FileFormat::Yaml)
                .required(true),
        );
    }
    Ok(builder)
}

fn yaml_layer_paths(dir: Option<&Path>, name: &str) -> Result<Vec<PathBuf>, SettingsError> {
    let Some(dir) = dir else {
        return Ok(Vec::new());
    };

    let mut paths = Vec::new();
    let base_path = dir.join(format!("{name}.yaml"));
    if base_path.try_exists().map_err(|err| {
        SettingsError::Config(config::ConfigError::Message(format!(
            "failed to check {}: {err}",
            base_path.display()
        )))
    })? {
        paths.push(base_path);
    }

    let drop_dir = dir.join(format!("{name}.d"));
    let Some(metadata) = std::fs::metadata(&drop_dir).map(Some).or_else(|err| {
        if err.kind() == path_std_io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(SettingsError::Config(config::ConfigError::Message(
                format!("failed to inspect {}: {err}", drop_dir.display()),
            )))
        }
    })?
    else {
        return Ok(paths);
    };
    if !metadata.is_dir() {
        return Err(SettingsError::Config(config::ConfigError::Message(
            format!("{} exists but is not a directory", drop_dir.display()),
        )));
    }

    let mut drop_in_paths = Vec::new();
    for entry in std::fs::read_dir(&drop_dir).map_err(|err| {
        SettingsError::Config(config::ConfigError::Message(format!(
            "failed to read {}: {err}",
            drop_dir.display()
        )))
    })? {
        let entry = entry.map_err(|err| {
            SettingsError::Config(config::ConfigError::Message(format!(
                "failed to read an entry in {}: {err}",
                drop_dir.display()
            )))
        })?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|ext| ext == "yaml" || ext == "yml")
        {
            drop_in_paths.push(path);
        }
    }
    drop_in_paths.sort();
    paths.extend(drop_in_paths);
    Ok(paths)
}

#[cfg(test)]
mod tests;
