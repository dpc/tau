//! User settings loaded from `~/.config/tau/` with `.d/` directory
//! overrides. Primary config files:
//!
//! - `cli.yaml` — CLI display preferences
//! - `harness.yaml` — harness settings, extensions, and roles
//!
//! Uses the `config` crate for layered YAML loading.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use indexmap::IndexMap;
use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use tau_proto::{
    ModelId, ModelTag, PromptContent, PromptPriority, ProviderName, ToolName, ToolTag,
};

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
const BUILT_IN_HARNESS_YAML: &str = include_str!("../config/built-in.harness.yaml");

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
    /// Symbol shown before the input prompt.
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
    /// Whether to render UI↔harness socket throughput in the model
    /// status bar by default.
    pub show_ui_io: bool,
    /// How tool calls are rendered in the transcript by default.
    pub show_tools: ShowTools,
    /// How inter-agent and user-agent messages are rendered in the transcript.
    pub show_messages: ShowMessages,
    /// Default notice visibility threshold for harness/UI notices.
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
/// `/set <name> <value>` commands.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct CliState {
    /// Whether to render file-mutation diffs in their full expanded
    /// form (vs the compact `+N/-M` chip). Controlled by
    /// `/set show-diff <true|false>`.
    pub show_diff: bool,
    /// Whether to render the agent's reasoning summary (the
    /// `agent.thinking` block). Controlled by
    /// `/set show-thinking <true|false>`.
    pub show_thinking: bool,
    /// Whether to render per-turn token usage stats below agent
    /// responses. Controlled by `/set show-turn-stats <true|false>`.
    pub show_turn_stats: bool,
    /// Whether to render the full-redraw debug counter in the model
    /// status bar. Controlled by `/set redraw-counter <true|false>`.
    pub redraw_counter: bool,
    /// Maximum number of rendered history lines replayed to the terminal on a
    /// full redraw. Controlled by `/set redraw-history-size <lines>`.
    pub redraw_history_size: usize,
    /// Whether to render UI↔harness socket throughput in the model
    /// status bar. Controlled by `/set show-ui-io <true|false>`.
    pub show_ui_io: bool,
    /// How tool calls are rendered in the transcript. Controlled by
    /// `/set show-tools <off|summarize-turn|summarize-prompt|compact|full>`.
    pub show_tools: ShowTools,
    /// How messages between the user and agents, or between agents, are
    /// rendered in the transcript. Controlled by `/set show-messages <mode>`.
    pub show_messages: ShowMessages,
    /// Notice visibility threshold. Controlled by
    /// `/set notice-level <critical|warning|info|debug|trace>`.
    pub notice_level: tau_proto::NoticeLevel,
    /// Deprecated compatibility setting for old routine status visibility.
    pub show_status: ShowStatus,
    /// Whether to show a compact indicator when the prompt input has hidden
    /// rows. Controlled by `/set show-prompt-scroll-indicator <true|false>`.
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

/// Which inter-agent/user-agent messages are shown in the CLI transcript.
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
    /// `/set` changes still persist as runtime state.
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

    /// Persist current state. Best-effort: a slash command never fails
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
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }
}

// ---------------------------------------------------------------------------
// Harness settings
// ---------------------------------------------------------------------------

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

    /// User-configured prompt templates exposed in the CLI as `/prompt <id>`.
    /// Map keys are non-empty ids with no whitespace so they can be addressed
    /// unambiguously from the slash command.
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
    session_retention_days: u64,
    extensions: HashMap<String, ExtensionEntry>,
    #[serde(default, alias = "customPrompts")]
    custom_prompts: BTreeMap<String, String>,
    #[serde(default)]
    tool_policy: ToolPolicy,
    agents: AgentsSettings,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentsSettings {
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
    #[serde(default, alias = "roleGroups")]
    role_groups: RawRoleGroups,
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
        let mut settings = Self {
            session_retention_days: wire.session_retention_days,
            extensions: wire.extensions,
            default_role: wire.agents.default_role,
            roles: HashMap::new(),
            role_groups: Vec::new(),
            prompt_fragments: wire.agents.prompt_fragments,
            required_skills: wire.agents.required_skills,
            custom_prompts: custom_prompt_map_to_vec(wire.custom_prompts),
            tool_policy: wire.tool_policy,
            agent_id_template: wire.agents.id_template,
            agent_display_name_template: wire.agents.display_name_template,
        };
        settings
            .apply_role_group_overrides(wire.agents.role_groups)
            .map_err(D::Error::custom)?;
        validate_custom_prompts(&settings.custom_prompts).map_err(D::Error::custom)?;
        settings.remove_disabled_roles();
        Ok(settings)
    }
}

/// Declarative harness-owned tool policy loaded from `tool_policy` config.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ToolPolicy {
    /// Rules keyed by stable names so higher-precedence config can override or
    /// disable built-in behavior.
    pub rules: IndexMap<String, ToolPolicyRule>,
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

#[derive(Default, Deserialize)]
#[serde(default)]
struct HarnessAgentRoleOverrides {
    #[serde(alias = "roleGroups")]
    role_groups: RawRoleGroups,
    #[serde(alias = "promptFragments")]
    prompt_fragments: Vec<RolePromptFragment>,
    #[serde(alias = "requiredSkills")]
    required_skills: Vec<tau_proto::SkillName>,
}

/// One saved prompt template exposed through the CLI `/prompt <id>` command.
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
    order: Option<Option<i64>>,
    #[serde(deserialize_with = "present_option")]
    description: Option<Option<String>>,
    #[serde(deserialize_with = "present_option")]
    model: Option<Option<ModelId>>,
    #[serde(deserialize_with = "present_option")]
    effort: Option<Option<tau_proto::Effort>>,
    #[serde(deserialize_with = "present_option")]
    verbosity: Option<Option<tau_proto::Verbosity>>,
    #[serde(alias = "thinkingSummary", deserialize_with = "present_option")]
    thinking_summary: Option<Option<tau_proto::ThinkingSummary>>,
    #[serde(alias = "serviceTier", deserialize_with = "present_option")]
    service_tier: Option<Option<tau_proto::ServiceTier>>,
    #[serde(deserialize_with = "present_option")]
    compaction: Option<Option<RoleCompaction>>,
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
fn present_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AgentRolePatch {
    #[serde(alias = "enabled", deserialize_with = "present_option")]
    enable: Option<Option<bool>>,
    #[serde(deserialize_with = "present_option")]
    order: Option<Option<i64>>,
    #[serde(deserialize_with = "present_option")]
    description: Option<Option<String>>,
    #[serde(deserialize_with = "present_option")]
    model: Option<Option<ModelId>>,
    #[serde(deserialize_with = "present_option")]
    effort: Option<Option<tau_proto::Effort>>,
    #[serde(deserialize_with = "present_option")]
    verbosity: Option<Option<tau_proto::Verbosity>>,
    #[serde(alias = "thinkingSummary", deserialize_with = "present_option")]
    thinking_summary: Option<Option<tau_proto::ThinkingSummary>>,
    #[serde(alias = "serviceTier", deserialize_with = "present_option")]
    service_tier: Option<Option<tau_proto::ServiceTier>>,
    #[serde(deserialize_with = "present_option")]
    compaction: Option<Option<RoleCompaction>>,
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
}

impl RawRoleGroup {
    fn defaults(&self) -> AgentRolePatch {
        AgentRolePatch {
            enable: self.enable,
            order: self.order,
            description: self.description.clone(),
            model: self.model.clone(),
            effort: self.effort,
            verbosity: self.verbosity,
            thinking_summary: self.thinking_summary,
            service_tier: self.service_tier,
            compaction: self.compaction,
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

    fn apply_role_group_overrides(&mut self, groups: RawRoleGroups) -> Result<(), SettingsError> {
        for (group_name, group) in groups {
            let group_defaults = group.defaults();
            let existing_role_names = self
                .role_groups
                .iter()
                .find(|existing_group| existing_group.name == group_name)
                .map(|existing_group| existing_group.roles.clone());
            if let Some(role_names) = &existing_role_names {
                for role_name in role_names {
                    if let Some(role) = self.roles.get_mut(role_name) {
                        role.apply_patch(&group_defaults);
                    }
                }
            }
            if group.roles.is_empty() {
                if existing_role_names.is_none() {
                    self.role_groups.push(RoleGroup {
                        name: group_name,
                        roles: Vec::new(),
                    });
                }
                continue;
            }
            for (role_name, role_overrides) in group.roles {
                let mut override_role = AgentRole::default();
                override_role.apply_patch(&group_defaults);
                override_role.apply_patch(&role_overrides);
                self.ensure_role_group_member(&group_name, &role_name)?;
                self.roles
                    .entry(role_name)
                    .and_modify(|role| {
                        role.apply_patch(&group_defaults);
                        role.apply_patch(&role_overrides);
                    })
                    .or_insert(override_role);
            }
        }
        Ok(())
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

    /// argv of the extension itself. `command[0]` is the executable;
    /// the rest are arguments. For built-in extensions this defaults
    /// to `[<current-exe>]`; for new entries this must be set
    /// explicitly. Tau-piggybacking entries can omit `command` and
    /// use `suffix` to pick the subcommand on the running tau binary.
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
    /// Optional role ordering key within a role group. Lower values come first;
    /// roles with the same order, or without an order, are sorted by role name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<RoleCompaction>,
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
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleCompaction {
    /// Ask the provider to use its model-specific default threshold.
    #[serde(alias = "providerDefault")]
    ProviderDefault,
    /// Do not request provider-side automatic compaction.
    Disabled,
    /// Ask the provider to compact at an explicit token threshold.
    Threshold(u64),
}

impl AgentRole {
    fn apply_patch(&mut self, patch: &AgentRolePatch) {
        if let Some(enable) = patch.enable {
            self.enable = enable;
        }
        if let Some(order) = patch.order {
            self.order = order;
        }
        if let Some(description) = &patch.description {
            self.description = description.clone();
        }
        if let Some(model) = &patch.model {
            self.model = model.clone();
        }
        if let Some(effort) = patch.effort {
            self.effort = effort;
        }
        if let Some(verbosity) = patch.verbosity {
            self.verbosity = verbosity;
        }
        if let Some(thinking_summary) = patch.thinking_summary {
            self.thinking_summary = thinking_summary;
        }
        if let Some(service_tier) = patch.service_tier {
            self.service_tier = service_tier;
        }
        if let Some(compaction) = patch.compaction {
            self.compaction = compaction;
        }
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
    /// A command-line role override named a role absent from effective config.
    UnknownRoleCliOverride(String),
    /// A `--harness-config KEY=VALUE` override had invalid syntax, YAML, or
    /// conflicting legacy/canonical key spellings.
    InvalidHarnessConfigCliOverride(String),
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
            Self::InvalidHarnessConfigCliOverride(message) => {
                write!(f, "invalid harness config CLI override: {message}")
            }
        }
    }
}

impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(source) => Some(source),
            Self::DuplicateGroupedRole { .. }
            | Self::UnknownRoleCliOverride(_)
            | Self::InvalidHarnessConfigCliOverride(_) => None,
        }
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
/// for tau-wide scalar state (`policy.cbor`, `cli.json`, …).
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

/// Validates that an extension name is safe to use as a single path component
/// in harness-owned per-extension paths and as an unambiguous segment in
/// dotted harness config override paths. Valid names contain only ASCII
/// letters, digits, `_`, and `-`.
pub fn validate_extension_name(extension_name: &str) -> Result<(), InvalidExtensionName> {
    if extension_name.is_empty() {
        return Err(InvalidExtensionName {
            name: extension_name.to_owned(),
            reason: "extension name must not be empty",
        });
    }
    if extension_name == "." || extension_name == ".." {
        return Err(InvalidExtensionName {
            name: extension_name.to_owned(),
            reason: "extension name must be a normal path component",
        });
    }
    if !extension_name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(InvalidExtensionName {
            name: extension_name.to_owned(),
            reason: "extension name may contain only ASCII letters, digits, '_' and '-'",
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
    /// Provider profile names that `tau dev tmux start` may copy into its
    /// scratch Tau state directory.
    ///
    /// Names are exact built-in provider namespaces and map to
    /// `auth.d/<provider>.json` in Tau provider auth storage.
    #[serde(default)]
    pub testing_providers: Vec<ProviderName>,
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
        if err.kind() == std::io::ErrorKind::NotFound {
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

/// Loads harness settings from `harness.yaml` with `harness.d/*.yaml`
/// overrides.
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
    let mut settings: HarnessSettings = load_yaml_layered_with_builtin_and_harness_overrides(
        BUILT_IN_HARNESS_YAML,
        dirs.config_dir.as_deref(),
        "harness",
        harness_config_overrides,
    )?;

    // Generic YAML layering replaces arrays, but `agents.prompt_fragments`,
    // `agents.required_skills`, and role-group metadata are additive. Recompute
    // them through the domain merge path; all other harness fields keep normal
    // config-layer semantics.
    let mut role_settings = HarnessSettings::built_in();
    for overrides in
        load_yaml_layer_files::<HarnessRoleOverrides>(dirs.config_dir.as_deref(), "harness")?
    {
        role_settings.apply_prompt_fragment_overrides(overrides.agents.prompt_fragments);
        role_settings.apply_required_skill_overrides(overrides.agents.required_skills);
        role_settings.apply_role_group_overrides(overrides.agents.role_groups)?;
    }
    for overrides in harness_role_cli_override_layers(harness_config_overrides)? {
        role_settings.apply_prompt_fragment_overrides(overrides.agents.prompt_fragments);
        role_settings.apply_required_skill_overrides(overrides.agents.required_skills);
        role_settings.apply_role_group_overrides(overrides.agents.role_groups)?;
    }
    role_settings.apply_role_cli_overrides(role_overrides)?;
    role_settings.remove_disabled_roles();
    role_settings.apply_agent_globals_to_roles();
    settings.prompt_fragments = role_settings.prompt_fragments;
    settings.required_skills = role_settings.required_skills;
    settings.roles = role_settings.roles;
    settings.role_groups = role_settings.role_groups;
    Ok(settings)
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
    normalize_alias_key(map, "thinkingSummary", "thinking_summary", source, path)?;
    normalize_alias_key(map, "serviceTier", "service_tier", source, path)?;
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
    Ok(())
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
    if let Some(tool_policy) = map.get_mut("tool_policy") {
        normalize_tool_policy_config_keys(tool_policy, source, "tool_policy")?;
    }
    if let Some(serde_json::Value::Object(agents)) = map.get_mut("agents") {
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
    let normalized_overrides = normalized_harness_config_overrides(overrides)?;
    for override_ in &normalized_overrides {
        builder = builder.add_source(harness_config_override_source(override_)?);
    }
    let config = builder.build()?;
    let value: serde_json::Value = config.try_deserialize()?;
    serde_json::from_value(value)
        .map_err(|error| SettingsError::Config(config::ConfigError::Message(error.to_string())))
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
    if parts[0] == "agents" && parts.len() > 1 {
        parts[1] = canonical_agents_key(parts[1]);
        if parts[1] == "role_groups" && parts.len() > 3 {
            if parts[3] == "roles" {
                if parts.len() > 5 {
                    parts[5] = canonical_role_key(parts[5]);
                }
            } else {
                parts[3] = canonical_role_key(parts[3]);
            }
        }
    }
    if parts[0] == "tool_policy" && parts.len() > 3 && parts[1] == "rules" {
        parts[3] = canonical_tool_policy_rule_key(parts[3]);
    }
    parts.join(".")
}

fn canonical_top_level_key(key: &str) -> &str {
    match key {
        "customPrompts" => "custom_prompts",
        "toolPolicy" => "tool_policy",
        _ => key,
    }
}

fn canonical_agents_key(key: &str) -> &str {
    match key {
        "defaultRole" => "default_role",
        "idTemplate" => "id_template",
        "displayNameTemplate" => "display_name_template",
        "roleGroups" => "role_groups",
        "promptFragments" => "prompt_fragments",
        "requiredSkills" => "required_skills",
        _ => key,
    }
}

fn canonical_role_key(key: &str) -> &str {
    match key {
        "enabled" => "enable",
        "thinkingSummary" => "thinking_summary",
        "serviceTier" => "service_tier",
        "promptFragments" => "prompt_fragments",
        "promptOverride" => "prompt_override",
        "enableToolGroups" => "enable_tool_groups",
        "disableToolGroups" => "disable_tool_groups",
        "enableToolTags" => "enable_tool_tags",
        "disableToolTags" => "disable_tool_tags",
        "enableTools" => "enable_tools",
        "disableTools" => "disable_tools",
        "requiredSkills" => "required_skills",
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
            config::Config::builder()
                .add_source(
                    config::File::from(path)
                        .format(config::FileFormat::Yaml)
                        .required(true),
                )
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
        if err.kind() == std::io::ErrorKind::NotFound {
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
