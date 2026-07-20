use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use tau_session_inspect::{default_session_id, default_sessions_dir, default_state_dir};

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
    #[arg(long = "role", global = true)]
    pub role: Option<String>,

    /// Override one harness config key after all config files are loaded.
    #[arg(
        long = "harness-config",
        global = true,
        value_name = "KEY=VALUE",
        require_equals = true
    )]
    pub harness_config: Vec<tau_config::settings::HarnessConfigCliOverride>,
}

#[derive(Args)]
pub struct RoleOverrideArgs {
    /// Enable a configured role after all config files are loaded.
    #[arg(long = "enable-role", global = true)]
    pub enable_role: Vec<String>,

    /// Disable a configured role after all config files are loaded.
    #[arg(long = "disable-role", global = true)]
    pub disable_role: Vec<String>,

    /// Disable every configured role before later CLI role overrides.
    #[arg(long = "disable-roles-all", global = true, action = clap::ArgAction::Count)]
    pub disable_roles_all: u8,
}

#[derive(Args)]
pub struct ExtensionOverrideArgs {
    /// Enable non-test configured extensions before later CLI extension
    /// overrides. The built-in test-dummy fixture still requires explicit
    /// `--enable-extension test-dummy`.
    #[arg(long = "enable-extensions-all", global = true, action = clap::ArgAction::Count)]
    pub enable_extensions_all: u8,

    /// Disable every configured extension before later CLI extension overrides.
    #[arg(long = "disable-extensions-all", global = true, action = clap::ArgAction::Count)]
    pub disable_extensions_all: u8,

    /// Enable a configured extension after all config files are loaded.
    ///
    /// `TAU_ENABLE_EXTENSIONS=NAME[,NAME...]` additively enables exact
    /// configured names before ordered CLI overrides. Space/tab around
    /// names is allowed; malformed or unknown names fail startup. CLI
    /// enable/disable flags win.
    #[arg(long = "enable-extension", global = true)]
    pub enable_extension: Vec<String>,

    /// Disable a configured extension after all config files are loaded.
    #[arg(long = "disable-extension", global = true)]
    pub disable_extension: Vec<String>,
}

#[derive(Args)]
pub struct RunArgs {
    /// Resume an existing session.
    ///
    /// Bare `-r` resumes the most recent session whose metadata matches
    /// the current working directory. `-r <id>` resumes that
    /// specific session id. Without `-r`, a fresh session id is minted
    /// (`<basename(cwd)>-<rand6>`).
    #[arg(short = 'r', long = "resume", num_args = 0..=1, default_missing_value = "")]
    pub resume: Option<String>,

    /// Deprecated legacy extension config path; use `--harness-config`
    /// overrides instead.
    #[arg(long, hide = true)]
    pub config: Option<PathBuf>,

    /// Read one prompt from stdin, submit it, print final output, and exit.
    #[arg(long = "prompt-stdin")]
    pub prompt_stdin: bool,

    /// Attach to an existing harness daemon for this project instead of
    /// spawning a new one. Errors if no daemon is running.
    #[arg(short = 'a', long)]
    pub attach: bool,

    /// Run without writing session membership, session metadata, session debug
    /// events, per-session logs, session-scoped extension data, or the terminal
    /// UI log to disk.
    ///
    /// Agent transcripts, provider state, credentials, user/cache extension
    /// data, runtime sockets, and policy/config state keep their normal
    /// persistence behavior.
    /// Cannot be combined with --resume or --attach.
    #[arg(long)]
    pub ephemeral: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run an interactive agent session.
    ///
    /// By default, `tau` spawns a new harness daemon and attaches to it
    /// for the duration of the session. Pass `--attach` (or `-a`) to
    /// connect to an already-running daemon for the current project
    /// instead — useful for a second UI, or for reconnecting after
    /// `/detach`.
    #[command(hide = true)]
    Run(RunArgs),

    /// Inspect sessions.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },

    /// Inspect agents in a running session.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },

    /// Show persisted policy approvals
    PolicyShow {
        /// Path to tau state directory (policy.cbor lives inside)
        #[arg(long, default_value_os_t = default_state_dir())]
        state_dir: PathBuf,
    },

    /// Copy sample config files to ~/.config/tau/
    Init {
        /// Overwrite existing config files
        #[arg(long)]
        force: bool,
    },

    /// Manage LLM provider profiles (add, remove, list)
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
    List,

    /// Show a single session's history.
    Show {
        /// Session identifier
        #[arg(long, default_value_t = default_session_id().to_owned())]
        session_id: String,

        /// Path to per-session storage root (`<state-dir>/sessions/`)
        #[arg(long, default_value_os_t = default_sessions_dir())]
        sessions_dir: PathBuf,
    },
}

#[derive(Subcommand)]
pub enum AgentCommand {
    /// List agents known to a running session.
    List(AgentListArgs),
}

/// Filters for `tau agent list`.
#[derive(Args, Clone)]
pub struct AgentListArgs {
    /// Running session to query.
    pub session_id: String,

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
        session_id: String,

        /// Line to submit. Slash commands are interpreted like the TUI.
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

    /// Print the effective provider-visible prompt context for a role.
    ///
    /// Uses a stable fake agent id so role previews render agent-scoped
    /// template sections such as Agent identity.
    PrintPrompt {
        /// Include harness-injected AGENTS.md context.
        #[arg(long = "enable-agents-md", default_value_t = true, action = clap::ArgAction::Set)]
        enable_agents_md: bool,
    },

    /// Print only the rendered system prompt for a role.
    ///
    /// Uses a stable fake agent id so role previews render agent-scoped
    /// template sections such as Agent identity.
    PrintSystemPrompt,

    /// Print the effective tool definitions for a role.
    PrintTools,

    /// Manage a manual Tau end-to-end session in a private tmux server.
    Tmux {
        /// Tmux helper action to run.
        #[command(subcommand)]
        command: DevTmuxCommand,
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
