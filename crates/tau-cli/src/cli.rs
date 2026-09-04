use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use tau_proto::SessionId;
use tau_session_inspect::{
    default_agents_dir, default_session_id, default_sessions_dir, default_state_dir,
};

#[cfg(test)]
mod tests;

#[derive(Parser)]
#[command(
    name = "tau",
    about = "Unix-native LLM agent harness",
    disable_version_flag = true
)]
pub struct Cli {
    /// Print version, build revision, and build date.
    #[arg(short = 'V', long = "version", global = true)]
    pub version: bool,

    #[command(flatten)]
    pub harness: HarnessArgs,

    #[command(flatten)]
    pub run: RunArgs,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Args)]
pub struct HarnessArgs {
    #[command(flatten)]
    pub role_overrides: RoleOverrideArgs,

    #[command(flatten)]
    pub extension_overrides: ExtensionOverrideArgs,

    /// Select the startup/rendered role.
    #[arg(short = 'r', long = "role")]
    pub role: Option<String>,

    /// Select comma-separated configuration profiles before CLI overrides.
    #[arg(long = "profile", value_name = "PROFILE")]
    pub profile: Option<String>,

    /// Override one harness config key after all config files are loaded.
    #[arg(
        long = "harness-config",
        value_name = "KEY=VALUE",
        require_equals = true
    )]
    pub harness_config: Vec<tau_config::settings::HarnessConfigCliOverride>,

    /// Override one provider alias for this harness startup.
    ///
    /// `TAU_PROVIDER_ALIASES` supplies a JSON object of lower-precedence
    /// environment overrides.
    #[arg(long = "provider-alias", value_name = "FROM=TO")]
    pub provider_alias: Vec<tau_config::settings::ProviderAliasCliOverride>,

    /// Override one exact model-name alias for this harness startup.
    ///
    /// `TAU_MODEL_ALIASES` supplies a JSON object of lower-precedence
    /// environment overrides.
    #[arg(long = "model-alias", value_name = "FROM=TO")]
    pub model_alias: Vec<tau_config::settings::ModelAliasCliOverride>,
}

#[derive(Args)]
pub struct RoleOverrideArgs {
    /// Enable a configured role after all config files are loaded.
    #[arg(long = "enable-role")]
    pub enable_role: Vec<String>,

    /// Disable a configured role after all config files are loaded.
    #[arg(long = "disable-role")]
    pub disable_role: Vec<String>,

    /// Disable every configured role before later CLI role overrides.
    #[arg(long = "disable-roles-all", action = clap::ArgAction::Count)]
    pub disable_roles_all: u8,
}

#[derive(Args)]
pub struct ExtensionOverrideArgs {
    /// Enable non-test configured extensions before later CLI extension
    /// overrides. The built-in test-dummy fixture still requires explicit
    /// `--enable-extension test-dummy`.
    #[arg(long = "enable-extensions-all", action = clap::ArgAction::Count)]
    pub enable_extensions_all: u8,

    /// Disable every configured extension before later CLI extension overrides.
    #[arg(long = "disable-extensions-all", action = clap::ArgAction::Count)]
    pub disable_extensions_all: u8,

    /// Enable a configured extension after all config files are loaded.
    ///
    /// `TAU_ENABLE_EXTENSIONS=NAME[,NAME...]` additively enables exact
    /// configured names before ordered CLI overrides. Space/tab around
    /// names is allowed; malformed or unknown names fail startup. CLI
    /// enable/disable flags win.
    #[arg(long = "enable-extension")]
    pub enable_extension: Vec<String>,

    /// Disable a configured extension after all config files are loaded.
    #[arg(long = "disable-extension")]
    pub disable_extension: Vec<String>,
}

#[derive(Args)]
/// Options shared by new, attach, and resume session startup.
pub struct RunArgs {
    /// Deprecated legacy extension config path; use `--harness-config`
    /// overrides instead.
    #[arg(long, hide = true)]
    pub config: Option<PathBuf>,

    /// Read one prompt from stdin, submit it, print final output, and exit.
    ///
    /// Answers go to stdout; reasoning, headers, and errors go to stderr.
    /// Each destination's terminal state is checked independently, and dynamic
    /// bodies are sanitized only when that destination is a terminal. Pipes and
    /// files retain semantic UTF-8 bytes and existing framing.
    #[arg(long = "prompt-stdin")]
    pub prompt_stdin: bool,

    /// Run without writing session membership, session metadata, session debug
    /// events, per-session logs, session-scoped extension data, or the terminal
    /// UI log to disk.
    ///
    /// Agent transcripts, provider state, credentials, user/cache extension
    /// data, runtime sockets, and configuration state keep their normal
    /// persistence behavior.
    #[arg(long)]
    pub ephemeral: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run an interactive agent session.
    ///
    /// `tau` spawns a new harness daemon and attaches for this process.
    #[command(hide = true)]
    Run(RunArgs),

    /// Attach to a running session without taking daemon ownership.
    Attach {
        /// Running session id; omit to choose interactively.
        session: Option<SessionId>,
    },

    /// Resume a persisted session in a new harness daemon.
    Resume {
        /// Persisted session id; omission auto-selects the sole unlocked
        /// target, otherwise shows the eligible-session picker.
        session: Option<SessionId>,
    },

    /// Serve one fixed session in the foreground without an initial UI.
    Serve {
        /// Exact session id to create or resume.
        #[arg(long)]
        session: SessionId,

        /// Require the session directory to be completely absent, then create
        /// it.
        #[arg(
            long,
            required_unless_present_any = ["existing", "create_or_existing"],
            conflicts_with_all = ["existing", "create_or_existing"]
        )]
        create: bool,

        /// Require and strictly resume valid existing session state.
        #[arg(
            long,
            required_unless_present_any = ["create", "create_or_existing"],
            conflicts_with_all = ["create", "create_or_existing"]
        )]
        existing: bool,

        /// Resume valid existing state or atomically create an absent session.
        #[arg(
            long,
            required_unless_present_any = ["create", "existing"],
            conflicts_with_all = ["create", "existing"]
        )]
        create_or_existing: bool,

        /// Read one literal bootstrap prompt from this UTF-8 file after
        /// startup.
        ///
        /// `-` reads stdin through EOF once. The paired bootstrap id makes this
        /// submission durable and at-most-once across restarts.
        #[arg(
            long = "bootstrap-prompt-file",
            value_name = "PATH",
            requires = "bootstrap_id"
        )]
        bootstrap_prompt_file: Option<PathBuf>,

        /// Durable bootstrap generation id.
        #[arg(
            long = "bootstrap-id",
            value_name = "ID",
            requires = "bootstrap_prompt_file"
        )]
        bootstrap_id: Option<tau_harness::BootstrapId>,

        /// Mirror framed, escaped extension stderr to this process's stderr.
        ///
        /// Private per-session extension log files remain authoritative. Custom
        /// extension stderr is unredacted and may reach a wider journal
        /// audience.
        #[arg(long)]
        mirror_extension_stderr: bool,
    },

    /// Inspect sessions.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },

    /// Inspect agents.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },

    /// Copy sample config files to ~/.config/tau/
    Init {
        /// Overwrite existing config files
        #[arg(long)]
        force: bool,
    },

    /// Manage LLM providers (add, remove, list)
    Provider {
        /// Subcommand and arguments (e.g. `add`, `remove <name>`, `list`)
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },

    /// Developer-only commands.
    #[command(hide = true, hide_possible_values = true)]
    Dev {
        #[command(subcommand)]
        command: DevCommand,
    },

    /// Run a bundled Tau component as a standalone process.
    ///
    /// Bundled extensions are components too, but not every component is an
    /// extension; for example, the harness is a component.
    Component {
        /// Component name (harness or a bundled extension such as ext-shell,
        /// ext-provider-builtin, ext-websearch, ext-pim, ext-rhai,
        /// ext-telegram, ext-xmpp, ext-std-notifications, or ext-test-dummy)
        name: String,

        /// Use stdin/stdout as the initial UI connection before starting
        /// harness extensions. Only valid with `tau component harness`.
        #[arg(long, hide = true)]
        initial_ui_stdio: bool,
    },
}

#[derive(Subcommand)]
pub enum SessionCommand {
    /// List currently running sessions.
    List(SessionListArgs),

    /// Show a single session's history.
    Show {
        /// Session identifier
        #[arg(
            long,
            default_value_t = tau_proto::SessionId::parse(default_session_id())
                .expect("configured default session id must be valid")
        )]
        session_id: tau_proto::SessionId,

        /// Path to per-session storage root (`<state-dir>/sessions/`)
        #[arg(long, default_value_os_t = default_sessions_dir())]
        sessions_dir: PathBuf,
    },

    /// Print exact durable activity accounting for one session as TOON.
    Stats {
        /// Session identifier to account.
        #[arg(long)]
        session: tau_proto::SessionId,

        /// Path to per-session storage root (`<state-dir>/sessions/`).
        #[arg(long, default_value_os_t = default_sessions_dir())]
        sessions_dir: PathBuf,
    },
}

/// Output and exact-directory filters for `tau session list`.
#[derive(Args, Clone, Debug, Default)]
pub struct SessionListArgs {
    /// List only harnesses whose canonical startup root is this directory.
    #[arg(long, value_name = "DIR", value_parser = parse_canonical_directory)]
    pub dir: Option<PathBuf>,

    /// Emit one JSON array with session id and canonical project root fields.
    #[arg(long)]
    pub json: bool,
}

/// Canonicalizes one existing directory during CLI parsing.
fn parse_canonical_directory(value: &str) -> Result<PathBuf, String> {
    let canonical = Path::new(value)
        .canonicalize()
        .map_err(|error| format!("cannot access directory `{value}`: {error}"))?;
    if !canonical.is_dir() {
        return Err(format!("path is not a directory: `{value}`"));
    }
    Ok(canonical)
}

#[derive(Subcommand)]
pub enum AgentCommand {
    /// List agents known to a running session.
    List(AgentListArgs),
    /// Project a validated durable agent snapshot (defaults to compact TOON
    /// lite).
    Trace(AgentTraceArgs),
}

/// Options for `tau agent trace`.
#[derive(Args, Clone)]
pub struct AgentTraceArgs {
    /// Durable agent journal to export.
    pub agent_id: tau_proto::AgentId,

    /// Recursively include agents created by the requested workflow.
    #[arg(long)]
    pub include_descendants: bool,

    /// Machine-readable export format.
    #[arg(long, value_enum, default_value_t)]
    pub format: AgentTraceFormat,

    /// Compact semantic text and tool-output detail.
    #[arg(long, value_enum, default_value_t)]
    pub mode: AgentTraceMode,

    /// Durable agent journal root.
    #[arg(long, default_value_os_t = default_agents_dir())]
    pub agents_dir: PathBuf,
}

/// Machine-readable agent trace export format.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum AgentTraceFormat {
    /// Complete canonical Tau JSON Lines.
    TauJsonl,
    /// Lossy OTLP/OpenInference JSON visualization adapter.
    OtlpJson,
    /// Compact assistant, message, reasoning, and tool-call timeline as JSON
    /// Lines.
    AgentToolsJsonl,
    /// Compact assistant, message, reasoning, and tool-call timeline as TOON.
    #[default]
    AgentToolsToon,
    /// Content-free provider, tool, wait, outer-turn, and compaction accounting
    /// as JSON Lines.
    AgentPerformanceJsonl,
}

/// Content detail for compact semantic trace formats.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum AgentTraceMode {
    /// Report complete metrics and at most 4 KiB of each text/output item.
    #[default]
    Lite,
    /// Report complete metrics and complete semantic text/normalized output.
    Full,
}

/// Filters for `tau agent list`.
#[derive(Args, Clone)]
pub struct AgentListArgs {
    /// Running session to query.
    pub session_id: SessionId,

    /// Include suspended live agents.
    #[arg(long)]
    pub include_suspended: bool,

    /// Include current unavailable agents and rows with missing, invalid, or
    /// unreadable creation facts.
    #[arg(long)]
    pub include_unavailable: bool,

    /// Include previously loaded and now-unloaded agents.
    #[arg(long)]
    pub include_unloaded: bool,

    /// Include every supported agent category.
    #[arg(long)]
    pub all: bool,
}

#[derive(Subcommand)]
pub enum DevCommand {
    /// Send one line to a running session.
    Send {
        /// Running session identifier.
        session_id: SessionId,

        /// Line to submit. Commands are interpreted like the TUI.
        #[arg(required = true, trailing_var_arg = true)]
        line: Vec<String>,
    },

    /// Dump the initial provider prompt built from local config.
    DumpInitialPrompt {
        /// Output path.
        #[arg(long, default_value = "tmp/initial_prompt.txt")]
        out: PathBuf,

        /// Synthetic first user message.
        #[arg(long, default_value = "hello")]
        message: String,
    },

    /// Print the effective provider-visible prompt context.
    ///
    /// Configures ordinary extensions, initializes one fresh ephemeral agent,
    /// and waits boundedly for its context without calling a provider.
    /// Extensions retain ordinary persistent state access and side effects.
    /// Omitting `--role` uses the configured startup role.
    PrintPrompt {
        /// Include harness-injected AGENTS.md context.
        #[arg(long = "enable-agents-md", default_value_t = true, action = clap::ArgAction::Set)]
        enable_agents_md: bool,
    },

    /// Print only the rendered system prompt for a role.
    ///
    /// Uses a stable fake agent id as the explicit `agent_id` input for custom
    /// templates; built-in templates intentionally omit agent identity.
    PrintSystemPrompt,

    /// Print the effective tool definitions.
    ///
    /// Uses the same fresh ephemeral-agent lifecycle and effective model/tool
    /// snapshot as `print-prompt`, without calling a provider. Extensions
    /// retain ordinary persistent state access and side effects. Omitting
    /// `--role` uses the configured startup role.
    PrintTools,

    /// Inspect or clear reports recorded by the standard papercut reporter.
    Papercut {
        /// Papercut operation to run.
        #[command(subcommand)]
        command: PapercutCommand,
    },

    /// Manage a manual Tau end-to-end session in a private tmux server.
    Tmux {
        /// Tmux helper action to run.
        #[command(subcommand)]
        command: DevTmuxCommand,
    },
}

/// Commands that inspect or clear the standard papercut reporter's records.
#[derive(Subcommand)]
pub enum PapercutCommand {
    /// List recorded papercut reports.
    List {
        /// Render the reports as copyable Markdown.
        #[arg(long)]
        markdown: bool,

        /// Tau state directory containing the standard reporter's records.
        #[arg(long, default_value_os_t = default_state_dir())]
        state_dir: PathBuf,
    },

    /// Remove every papercut report present at this command's serialized clear
    /// boundary.
    Clear {
        /// Tau state directory containing the standard reporter's records.
        #[arg(long, default_value_os_t = default_state_dir())]
        state_dir: PathBuf,
    },
}

/// Hidden tmux helper subcommands for manual Tau end-to-end sessions.
#[derive(Subcommand)]
pub enum DevTmuxCommand {
    /// Start Tau in an isolated scratch environment inside tmux.
    Start(DevTmuxStartArgs),

    /// Capture the current tmux pane contents.
    Capture(DevTmuxTargetArgs),

    /// Send text to the tmux pane, followed by Enter by default.
    Send(DevTmuxSendArgs),

    /// Stop the private tmux server.
    Stop(DevTmuxStopArgs),
}

/// Shared tmux target arguments used by the manual E2E helper.
#[derive(Args)]
pub struct DevTmuxCommonArgs {
    /// Scratch root containing the tmux socket and isolated Tau environment.
    /// When omitted, `start` generates a fresh temporary root; target commands
    /// use the historical static fallback root.
    #[arg(long = "scratch-root", visible_alias = "root")]
    pub scratch_root: Option<PathBuf>,

    /// Private tmux session name.
    #[arg(long, default_value = "tau-e2e")]
    pub session: String,
}

/// Arguments for starting a new isolated Tau tmux session.
#[derive(Args)]
pub struct DevTmuxStartArgs {
    /// Shared tmux socket/session selection.
    #[command(flatten)]
    pub common: DevTmuxCommonArgs,

    /// Tau binary to run inside tmux.
    #[arg(long)]
    pub tau_bin: Option<PathBuf>,

    /// Working directory for Tau and core-shell.
    #[arg(long)]
    pub workdir: Option<PathBuf>,

    /// Initial tmux pane width.
    #[arg(long, default_value_t = 120)]
    pub width: u16,

    /// Initial tmux pane height.
    #[arg(long, default_value_t = 40)]
    pub height: u16,
}

/// Arguments that identify an existing Tau tmux session.
#[derive(Args)]
pub struct DevTmuxTargetArgs {
    /// Shared tmux socket/session selection.
    #[command(flatten)]
    pub common: DevTmuxCommonArgs,
}

/// Arguments for sending literal input to an existing Tau tmux session.
#[derive(Args)]
pub struct DevTmuxSendArgs {
    /// Existing tmux session to receive input.
    #[command(flatten)]
    pub target: DevTmuxTargetArgs,

    /// Do not send Enter after the text.
    #[arg(long)]
    pub no_enter: bool,

    /// Text to send literally to the Tau prompt.
    #[arg(required = true, trailing_var_arg = true)]
    pub text: Vec<String>,
}

/// Arguments for stopping an existing Tau tmux session.
#[derive(Args)]
pub struct DevTmuxStopArgs {
    /// Existing tmux session to stop.
    #[command(flatten)]
    pub target: DevTmuxTargetArgs,

    /// Remove the scratch root after stopping tmux.
    #[arg(long)]
    pub remove_scratch: bool,
}
