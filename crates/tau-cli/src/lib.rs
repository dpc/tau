//! CLI entrypoint for tau: starts a harness daemon and connects as a
//! socket client for interactive chat.
//!
//! Component and local-execution boundaries are summarized in `ARCH-tau-cli`.
//! Transcript presentation follows `SPEC-tau-cli-transcript-styling`.

use tau_config::settings as path_tau_config_settings;

pub mod cli;

mod action_commands;
mod agent_activity;
mod agent_navigation;
mod chat;
mod daemon;
mod dev_tmux;
mod estimated_cost;
mod event_renderer;
mod line_output;
mod list_agents;
mod list_sessions;
mod markdown_render;
mod message_fact_render;
mod papercut;
mod print_prompt;
mod print_tools;
mod prompt_history;
mod prompt_stdin;
mod provider_quota;
mod render_request;
mod renderer_handle;
mod send;
mod settings_registry;
mod skill_commands;
mod terminal_text;
mod theme;
mod tool_render;
mod transcript_markers;
mod turn_stats_projection;
mod ui_client;
mod ui_commands;
mod ui_events;
mod ui_logging;
mod ui_prompt;
mod unload_agent;
mod watch_activity;

use std::process::ExitCode;
use std::sync::{Mutex, MutexGuard};
use std::{fmt, io};

use tau_harness::SessionLaunchStatus;

use crate::chat::run_chat;
use crate::daemon::resolve_resume_session_id;

/// Single shared message for mutex-poison panics: every mutex in this
/// crate is held only for short, infallible critical sections, so poison
/// means another thread panicked mid-update and continuing is unsafe.
pub(crate) const MUTEX_POISONED: &str = "mutex poisoned";

/// Locks `mutex` and panics on poison. Centralizes the panic message so
/// individual call sites read as `let mut g = locked(&m);` instead of
/// repeating `.expect("... mutex poisoned")` everywhere.
pub(crate) fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect(MUTEX_POISONED)
}

mod built_info;

const STARTUP_PUNS: &[&str] = &[
    "Tau is like Pi, but twice as much.",
    "A whole new angle on coding agents.",
    "Tau day is every day if you care about circles enough.",
    "Come for the agent, stay for the circumference discourse.",
    "Tau is the irrational choice for rational Unix hackers.",
    "Small tools, loosely joined — that’s the Tau of Unix.",
    "In Tau, what goes around comes around over stdio.",
    "We’ve come full τurn.",
    "Tau keeps the loop tight and the pipes honest.",
    "Every extension gets its turn in Tau.",
    "Tau speaks fluent stdio with a circular accent.",
    "Agents, tools, sockets, loops: a well-rounded lineup.",
    "Ready, set, Tau!",
    "Tau day to code.",
    "Tau-tau control.",
    "Tau-tally operational.",
    "Tau much power in one terminal.",
    "Tau infinity and beyond.",
    "Tau the line between human and agent.",
    "Tau’s what I’m talking about.",
    "One shell to Tau them all.",
    "Tau-powered, Unix-native.",
    "Complete revolution.",
    "Wrapping around nicely.",
    "Continuous on S¹, probably.",
    "Cohomology remains left as exercise.",
];

pub(crate) fn random_startup_pun() -> &'static str {
    use rand::Rng;
    let idx = rand::thread_rng().gen_range(0..STARTUP_PUNS.len());
    STARTUP_PUNS[idx]
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors returned by the CLI.
#[derive(Debug)]
pub enum CliError {
    /// Offline cache analysis produced useful output with missing evidence.
    CachePartial,
    /// Offline cache invocation or required schema could not be inspected.
    CacheInvalid,
    Io(io::Error),
    Encode(tau_proto::EncodeError),
    Harness(tau_harness::HarnessError),
    Inspect(tau_session_inspect::InspectError),
    DaemonExited(String),
    NoRunningDaemon,
    Participant(String),
    PromptStdin(PromptStdinError),
    SessionNotFound(String),
    /// The attachment stopped because terminal foreground ownership is
    /// unconfirmed.
    ForegroundOwnershipUnconfirmed {
        /// Complete failure message retained for non-terminal callers.
        message: String,
        /// Bounded private restoration diagnostic written by persistent UIs.
        diagnostic: tau_cli_term::ForegroundRestorationDiagnostic,
    },
    /// The attachment stopped after its first reported terminal output failure.
    TerminalOutputFailed(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CachePartial => f.write_str("cache analysis is partial; see report coverage"),
            Self::CacheInvalid => f.write_str("cache source unavailable, unsupported, or invalid; use a matching-build inspector and check the selected scope and limits"),
            Self::Io(source) => write!(f, "I/O error: {source}"),
            Self::Encode(source) => write!(f, "encode error: {source}"),
            Self::Harness(source) => write!(f, "harness error: {source}"),
            Self::Inspect(source) => write!(f, "inspect error: {source}"),
            Self::DaemonExited(msg) => write!(f, "harness daemon exited: {msg}"),
            Self::NoRunningDaemon => {
                f.write_str("no running session is available; start one with `tau`")
            }
            Self::Participant(msg) => write!(f, "participant error: {msg}"),
            Self::PromptStdin(error) => error.fmt(f),
            Self::SessionNotFound(id) => write!(f, "session not found: `{id}`"),
            Self::ForegroundOwnershipUnconfirmed { message, .. } => f.write_str(message),
            Self::TerminalOutputFailed(message) => f.write_str(message),
        }
    }
}

impl CliError {
    /// Distinguishes partial offline evidence from invalid invocation and
    /// ordinary failures.
    fn exit_code(&self) -> ExitCode {
        match self {
            Self::CachePartial => ExitCode::from(3),
            Self::CacheInvalid => ExitCode::from(2),
            _ => ExitCode::FAILURE,
        }
    }

    /// Returns whether the top-level CLI may write this failure to stderr.
    fn should_report_to_terminal(&self) -> bool {
        !matches!(
            self,
            Self::ForegroundOwnershipUnconfirmed { .. } | Self::TerminalOutputFailed(_)
        )
    }
}

/// Terminal failure while admitting or executing a one-shot stdin prompt.
#[derive(Debug)]
pub enum PromptStdinError {
    /// The harness did not acknowledge create admission before its deadline.
    AdmissionTimeout {
        /// Admission deadline that elapsed.
        timeout: std::time::Duration,
    },
    /// The harness rejected the correlated create request.
    Rejected {
        /// Stable protocol rejection category.
        reason: tau_proto::UiCreateAgentRejection,
        /// Bounded harness-authored diagnostic.
        message: String,
    },
    /// The accepted initial prompt failed after create admission completed.
    PromptFailed {
        /// Lifecycle stage that failed.
        stage: tau_proto::AgentPromptFailureStage,
        /// Bounded harness-authored diagnostic.
        message: String,
    },
    /// The correlated provider execution reached an unsuccessful terminal.
    ExecutionFailed {
        /// Provider-authored or harness-classified user-facing diagnostic.
        message: String,
    },
}

impl fmt::Display for PromptStdinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AdmissionTimeout { timeout } => write!(
                f,
                "timed out after {}s waiting for create-agent admission",
                timeout.as_secs()
            ),
            Self::Rejected { reason, message } => {
                write!(f, "create-agent request failed ({}): {message}", reason)
            }
            Self::PromptFailed { stage, message } => {
                write!(f, "initial prompt failed ({}): {message}", stage)
            }
            Self::ExecutionFailed { message } => {
                write!(f, "initial prompt failed (execution): {message}")
            }
        }
    }
}

impl std::error::Error for PromptStdinError {}
impl std::error::Error for CliError {}

impl From<io::Error> for CliError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<tau_harness::HarnessError> for CliError {
    fn from(source: tau_harness::HarnessError) -> Self {
        Self::Harness(source)
    }
}

impl From<tau_session_inspect::InspectError> for CliError {
    fn from(source: tau_session_inspect::InspectError) -> Self {
        Self::Inspect(source)
    }
}

// ---------------------------------------------------------------------------
// Build labels and version helpers (shared by chat banner, EventRenderer
// banner, and `tau --version`).
// ---------------------------------------------------------------------------

fn run_harness_component_default() -> Result<(), Box<dyn std::error::Error>> {
    run_harness_component(false)
}

fn run_harness_component(initial_ui_stdio: bool) -> Result<(), Box<dyn std::error::Error>> {
    if initial_ui_stdio {
        tau_harness::run_component_with_internal_tools_and_initial_ui_stdio(
            tau_harness_tools::builtin_handlers(),
        )
    } else {
        tau_harness::run_component_with_internal_tools(tau_harness_tools::builtin_handlers())
    }
}

fn build_revision() -> String {
    tau_harness::version::build_revision()
}

fn build_last_modified() -> Option<String> {
    // Fall back to the locally-formatted `built` timestamp when the
    // harness can't produce a date (e.g. a `cargo build` outside of
    // Nix where the date placeholder is unpatched and the harness'
    // `built` snapshot only has the RFC2822 string).
    tau_harness::version::build_last_modified()
        .or_else(|| short_built_time(built_info::BUILT_TIME_UTC))
        .filter(|date| date != "1980-01-01 00:00")
}

fn short_built_time(time: &str) -> Option<String> {
    let input_format = time::macros::format_description!(
        "[weekday repr:short], [day padding:none] [month repr:short] [year] [hour]:[minute]:[second] [offset_hour sign:mandatory][offset_minute]"
    );
    let output_format = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]");
    time::OffsetDateTime::parse(time, input_format)
        .ok()?
        .format(output_format)
        .ok()
}

pub(crate) fn build_label_parts() -> (String, String) {
    let version = format!("tau {}", env!("CARGO_PKG_VERSION"));
    let build = match build_last_modified() {
        Some(date) => format!("({}, {})", build_revision(), date),
        None => format!("({})", build_revision()),
    };
    (version, build)
}

fn version_label() -> String {
    let (version, build) = build_label_parts();
    format!("{version} {build}")
}

/// Build the two-line startup banner: logo + name/version/build on the
/// first line, logo continuation + random pun on the second.
pub(crate) fn build_banner(theme: &tau_themes::Theme) -> tau_cli_term::StyledText {
    use tau_themes::names;
    let logo = tau_cli_term::resolve::resolve(theme, names::BANNER_LOGO);
    let name = tau_cli_term::resolve::resolve(theme, names::BANNER_NAME);
    let version_style = tau_cli_term::resolve::resolve(theme, names::BANNER_VERSION);
    let build_style = tau_cli_term::resolve::resolve(theme, names::BANNER_BUILD);
    let pun_style = tau_cli_term::resolve::resolve(theme, names::BANNER_PUN);
    let pun = random_startup_pun();
    let (version, build) = build_label_parts();
    tau_cli_term::StyledText::from(vec![
        tau_cli_term::Span::new("▝▜▛▀ ", logo),
        tau_cli_term::Span::new("tau", name),
        tau_cli_term::Span::new(version.trim_start_matches("tau"), version_style),
        tau_cli_term::Span::new(" ", Default::default()),
        tau_cli_term::Span::new(build, build_style),
        tau_cli_term::Span::new("\n", Default::default()),
        tau_cli_term::Span::new(" ▐▙▖ ", logo),
        tau_cli_term::Span::new(pun, pun_style),
    ])
}

// ---------------------------------------------------------------------------
// Short-id minting (used for both session ids and per-UI log dir ids)
// ---------------------------------------------------------------------------

/// Build an id of the form `<prefix>-<6 base36 chars>`. Used for both
/// session and UI ids so the visual shape is consistent.
pub(crate) fn mint_short_id(prefix: &str) -> String {
    use rand::distributions::Distribution;

    struct Base36;
    impl Distribution<char> for Base36 {
        fn sample<R: rand::Rng + ?Sized>(&self, rng: &mut R) -> char {
            let n: u8 = rng.gen_range(0..36);
            if n < 10 {
                (b'0' + n) as char
            } else {
                (b'a' + (n - 10)) as char
            }
        }
    }

    let suffix: String = Base36.sample_iter(rand::thread_rng()).take(6).collect();
    format!("{prefix}-{suffix}")
}

// ---------------------------------------------------------------------------
// `tau init`
// ---------------------------------------------------------------------------

const SAMPLE_CLI: &str = include_str!("../../../config/cli.yaml");
const SAMPLE_HARNESS: &str = include_str!("../../../config/harness.yaml");

fn run_init(force: bool) -> Result<(), CliError> {
    let Some(dir) = tau_config::settings::config_dir() else {
        return Err(CliError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine config directory",
        )));
    };
    std::fs::create_dir_all(&dir)?;

    let files = [("cli.yaml", SAMPLE_CLI), ("harness.yaml", SAMPLE_HARNESS)];

    for (name, content) in &files {
        let path = dir.join(name);
        if path.exists() && !force {
            eprintln!(
                "skip: {} (exists, use --force to overwrite)",
                path.display()
            );
        } else {
            std::fs::write(&path, content)?;
            eprintln!("wrote: {}", path.display());
        }
    }

    eprintln!("next: use `tau provider add` to log in to a hosted LLM provider");

    Ok(())
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

pub type ComponentRunner = fn() -> Result<(), Box<dyn std::error::Error>>;

fn parse_role_cli_overrides<I, S>(args: I) -> Vec<tau_config::settings::RoleCliOverride>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let mut overrides = Vec::new();
    let mut args = args.into_iter().map(Into::into);
    let _program = args.next();
    while let Some(arg) = args.next() {
        let arg = arg.to_string_lossy();
        if arg == "--" {
            break;
        }
        if arg == "--disable-roles-all" {
            overrides.push(path_tau_config_settings::RoleCliOverride::DisableAll);
        } else if let Some(role) = arg.strip_prefix("--enable-role=") {
            overrides.push(path_tau_config_settings::RoleCliOverride::Enable(
                role.to_owned(),
            ));
        } else if arg == "--enable-role" {
            if let Some(role) = args.next() {
                overrides.push(path_tau_config_settings::RoleCliOverride::Enable(
                    role.to_string_lossy().into_owned(),
                ));
            }
        } else if let Some(role) = arg.strip_prefix("--disable-role=") {
            overrides.push(path_tau_config_settings::RoleCliOverride::Disable(
                role.to_owned(),
            ));
        } else if arg == "--disable-role"
            && let Some(role) = args.next()
        {
            overrides.push(path_tau_config_settings::RoleCliOverride::Disable(
                role.to_string_lossy().into_owned(),
            ));
        }
    }
    overrides
}

fn reject_harness_config_overrides(
    overrides: &[tau_config::settings::HarnessConfigCliOverride],
    command: &str,
) -> Result<(), CliError> {
    if overrides.is_empty() {
        return Ok(());
    }
    Err(CliError::Participant(format!(
        "--harness-config can only be used when starting a new harness instance; `{command}` cannot apply it to an existing or absent harness"
    )))
}

/// Tracks which public startup-only alias inputs the invocation supplied.
#[derive(Clone, Copy, Default)]
struct ModelReferenceAliasInputPresence {
    /// Whether `--provider-alias` was supplied.
    provider_flag: bool,
    /// Whether `--model-alias` was supplied.
    model_flag: bool,
    /// Whether `TAU_PROVIDER_ALIASES` was present.
    provider_environment: bool,
    /// Whether `TAU_MODEL_ALIASES` was present.
    model_environment: bool,
}

/// Rejects startup-only alias sources without misreporting them as generic
/// `--harness-config` input.
fn reject_model_reference_alias_inputs(
    presence: ModelReferenceAliasInputPresence,
    command: &str,
) -> Result<(), CliError> {
    let source = if presence.provider_flag {
        "--provider-alias"
    } else if presence.model_flag {
        "--model-alias"
    } else if presence.provider_environment {
        tau_config::settings::TAU_PROVIDER_ALIASES_ENV
    } else if presence.model_environment {
        tau_config::settings::TAU_MODEL_ALIASES_ENV
    } else {
        return Ok(());
    };
    Err(CliError::Participant(format!(
        "{source} can only be used when starting a new harness instance; `{command}` cannot apply it to an existing or absent harness"
    )))
}

fn validate_agent_trace_mode(
    format: cli::AgentTraceFormat,
    mode: cli::AgentTraceMode,
) -> Result<(), CliError> {
    if mode == cli::AgentTraceMode::Full
        && !matches!(
            format,
            cli::AgentTraceFormat::AgentToolsJsonl | cli::AgentTraceFormat::AgentToolsToon
        )
    {
        return Err(CliError::Participant(
            "`agent trace --mode full` requires `--format agent-tools-jsonl` or \
             `--format agent-tools-toon`"
                .to_owned(),
        ));
    }
    Ok(())
}

fn reject_dev_tmux_startup_overrides(
    profile: Option<&str>,
    startup_role: Option<&str>,
    role_cli_overrides: &[tau_config::settings::RoleCliOverride],
    extension_cli_overrides: &[tau_config::settings::ExtensionCliOverride],
    harness_config_overrides: &[tau_config::settings::HarnessConfigCliOverride],
) -> Result<(), CliError> {
    if profile.is_some() {
        return Err(CliError::Participant(
            "`tau dev tmux` cannot use a configuration profile because the outer helper must not load normal user harness config before spawning the scratch Tau".to_owned(),
        ));
    }
    if startup_role.is_some() {
        return Err(CliError::Participant(
            "`tau dev tmux` cannot use --role because the outer helper must not load normal user harness config before spawning the scratch Tau".to_owned(),
        ));
    }
    if !role_cli_overrides.is_empty() {
        return Err(CliError::Participant(
            "`tau dev tmux` cannot use role enable/disable overrides because the outer helper must not load normal user harness config before spawning the scratch Tau".to_owned(),
        ));
    }
    if !extension_cli_overrides.is_empty() {
        return Err(CliError::Participant(
            "`tau dev tmux` cannot use extension enable/disable overrides because the outer helper must not load normal user harness config before spawning the scratch Tau".to_owned(),
        ));
    }
    reject_harness_config_overrides(harness_config_overrides, "dev tmux")
}

fn reject_legacy_config_path(config: Option<&std::path::Path>) -> Result<(), CliError> {
    if let Some(path) = config {
        return Err(CliError::Participant(format!(
            "--config is no longer supported (got `{}`); use harness.yaml or --harness-config KEY=VALUE overrides",
            path.display()
        )));
    }
    Ok(())
}

fn reject_attach_startup_overrides(
    prompt_stdin: bool,
    profile_selected: bool,
    startup_role: Option<&str>,
    role_cli_overrides: &[tau_config::settings::RoleCliOverride],
    extension_cli_overrides: &[tau_config::settings::ExtensionCliOverride],
) -> Result<(), CliError> {
    if profile_selected {
        return Err(CliError::Participant(
            "`tau attach` cannot apply --profile to an already-running daemon".to_owned(),
        ));
    }
    if startup_role.is_some() && !prompt_stdin {
        return Err(CliError::Participant(
            "`tau attach` cannot apply --role to an already-running interactive daemon; use --prompt-stdin if you want --role for the submitted prompt".to_owned(),
        ));
    }
    if !role_cli_overrides.is_empty() {
        return Err(CliError::Participant(
            "`tau attach` cannot apply role enable/disable overrides to an already-running daemon"
                .to_owned(),
        ));
    }
    if !extension_cli_overrides.is_empty() {
        return Err(CliError::Participant(
            "`tau attach` cannot apply extension enable/disable overrides to an already-running daemon"
                .to_owned(),
        ));
    }
    Ok(())
}

fn reject_ephemeral_incompatible(
    ephemeral: bool,
    startup_mode: &StartupMode,
) -> Result<(), CliError> {
    if ephemeral {
        let command = match startup_mode {
            StartupMode::New => return Ok(()),
            StartupMode::Attach(_) => "attach",
            StartupMode::Resume(_) => "resume",
        };
        return Err(CliError::Participant(format!(
            "--ephemeral cannot be combined with `tau {command}`"
        )));
    }
    Ok(())
}

fn required_harness_role<'a>(role: Option<&'a str>, command: &str) -> Result<&'a str, CliError> {
    role.ok_or_else(|| CliError::Participant(format!("tau dev {command} requires --role <role>")))
}

fn parse_extension_cli_overrides<I, S>(args: I) -> Vec<tau_config::settings::ExtensionCliOverride>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let mut overrides = Vec::new();
    let mut args = args.into_iter().map(Into::into);
    let _program = args.next();
    while let Some(arg) = args.next() {
        let arg = arg.to_string_lossy();
        if arg == "--" {
            break;
        }
        if arg == "--enable-extensions-all" {
            overrides.push(path_tau_config_settings::ExtensionCliOverride::EnableAll);
        } else if arg == "--disable-extensions-all" {
            overrides.push(path_tau_config_settings::ExtensionCliOverride::DisableAll);
        } else if let Some(extension) = arg.strip_prefix("--enable-extension=") {
            overrides.push(path_tau_config_settings::ExtensionCliOverride::Enable(
                extension.to_owned(),
            ));
        } else if arg == "--enable-extension" {
            if let Some(extension) = args.next() {
                overrides.push(path_tau_config_settings::ExtensionCliOverride::Enable(
                    extension.to_string_lossy().into_owned(),
                ));
            }
        } else if let Some(extension) = arg.strip_prefix("--disable-extension=") {
            overrides.push(path_tau_config_settings::ExtensionCliOverride::Disable(
                extension.to_owned(),
            ));
        } else if arg == "--disable-extension"
            && let Some(extension) = args.next()
        {
            overrides.push(path_tau_config_settings::ExtensionCliOverride::Disable(
                extension.to_string_lossy().into_owned(),
            ));
        }
    }
    overrides
}

fn parse_harness_config_cli_overrides<I, S>(
    args: I,
) -> Result<Vec<tau_config::settings::HarnessConfigCliOverride>, String>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let mut overrides = Vec::new();
    let mut args = args.into_iter().map(Into::into);
    let _program = args.next();
    for arg in args {
        let arg = arg.to_string_lossy();
        if arg == "--" {
            break;
        }
        if let Some(value) = arg.strip_prefix("--harness-config=") {
            overrides.push(value.parse()?);
        }
    }
    Ok(overrides)
}

/// Describes how a bundled component gets its global tracing subscriber.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComponentLogging {
    /// `tau-cli` installs a stderr subscriber before invoking the component.
    CliStderr,
    /// The component installs its own subscriber, or does not emit tracing
    /// logs.
    RunnerManaged,
}

/// Target semantics selected by the public session-startup command.
#[derive(Clone, Debug, Eq, PartialEq)]
enum StartupMode {
    /// Mint and own a fresh session.
    New,
    /// Attach to an existing live session, selecting interactively when absent.
    Attach(Option<tau_proto::SessionId>),
    /// Resume persisted state, selecting interactively when absent.
    Resume(Option<tau_proto::SessionId>),
}

/// A startup request or one of the non-startup CLI commands.
enum DispatchCommand {
    Startup {
        args: cli::RunArgs,
        mode: StartupMode,
    },
    Other(cli::Command),
}

/// Whether dispatch must accept a fresh effective harness settings snapshot.
///
/// Commands that only inspect local state or connect to an existing daemon must
/// remain usable when configuration changes after that daemon's accepted
/// startup.
fn consumes_harness_settings(command: &DispatchCommand) -> bool {
    match command {
        DispatchCommand::Startup {
            mode: StartupMode::Attach(_),
            ..
        } => false,
        DispatchCommand::Startup { .. } => true,
        DispatchCommand::Other(cli::Command::Dev {
            command:
                cli::DevCommand::DumpInitialPrompt { .. }
                | cli::DevCommand::PrintPrompt { .. }
                | cli::DevCommand::PrintSystemPrompt
                | cli::DevCommand::PrintTools,
        }) => true,
        DispatchCommand::Other(cli::Command::Component { name, .. }) => name == "harness",
        DispatchCommand::Other(cli::Command::Serve { .. }) => true,
        _ => false,
    }
}

/// Returns whether a dispatch path must reject all public alias inputs even if
/// another internal validation path happens to inspect harness settings.
fn rejects_model_reference_alias_inputs(command: &DispatchCommand) -> bool {
    !consumes_harness_settings(command)
        || matches!(
            command,
            DispatchCommand::Other(cli::Command::Dev {
                command: cli::DevCommand::DumpInitialPrompt { .. }
            })
        )
}

pub struct Component {
    /// Name accepted by the `tau component <name>` dispatcher.
    pub name: &'static str,
    /// Function that runs the component over stdin/stdout.
    pub runner: ComponentRunner,
    /// Owner of the component's tracing initialization.
    pub logging: ComponentLogging,
}

/// Parses CLI arguments via clap and dispatches to the appropriate
/// command.
pub fn main_with_args() -> std::process::ExitCode {
    main_with_args_and_components(&[])
}

/// Parses CLI arguments via clap and dispatches to the appropriate
/// command, using caller-provided component registrations for
/// `component` dispatch.
pub fn main_with_args_and_components(components: &[Component]) -> std::process::ExitCode {
    tau_ext_provider_builtin::initialize_cache_diagnostic_build(build_revision());
    use std::process::ExitCode;

    use clap::{CommandFactory, FromArgMatches};

    let run = || -> Result<(), CliError> {
        let role_cli_overrides = parse_role_cli_overrides(std::env::args_os());
        let extension_cli_overrides = parse_extension_cli_overrides(std::env::args_os());
        let mut harness_config_overrides = parse_harness_config_cli_overrides(std::env::args_os())
            .map_err(CliError::Participant)?;
        let clap_command = cli::Cli::command();
        let clap_matches = clap_command.get_matches();
        let profile_was_explicitly_selected = matches!(
            clap_matches.value_source("profile"),
            Some(clap::parser::ValueSource::CommandLine)
        );
        let cli::Cli {
            version,
            harness,
            run,
            command,
        } = cli::Cli::from_arg_matches(&clap_matches)
            .expect("arguments accepted by Cli::command must parse as Cli");
        let provider_alias_environment =
            std::env::var_os(tau_config::settings::TAU_PROVIDER_ALIASES_ENV);
        let model_alias_environment = std::env::var_os(tau_config::settings::TAU_MODEL_ALIASES_ENV);
        let alias_input_presence = ModelReferenceAliasInputPresence {
            provider_flag: !harness.provider_alias.is_empty(),
            model_flag: !harness.model_alias.is_empty(),
            provider_environment: provider_alias_environment.is_some(),
            model_environment: model_alias_environment.is_some(),
        };
        if version {
            reject_model_reference_alias_inputs(alias_input_presence, "version")?;
            println!("{}", version_label());
            return Ok(());
        }
        reject_legacy_config_path(run.config.as_deref())?;
        if matches!(&command, Some(cli::Command::Serve { .. }))
            && (run.ephemeral || run.prompt_stdin)
        {
            return Err(CliError::Participant(
                "`tau serve` does not accept --ephemeral or --prompt-stdin".to_owned(),
            ));
        }
        let command = match command {
            Some(cli::Command::Run(args)) => DispatchCommand::Startup {
                args,
                mode: StartupMode::New,
            },
            Some(cli::Command::Attach { session }) => DispatchCommand::Startup {
                args: run,
                mode: StartupMode::Attach(session),
            },
            Some(cli::Command::Resume { session }) => DispatchCommand::Startup {
                args: run,
                mode: StartupMode::Resume(session),
            },
            Some(command) => DispatchCommand::Other(command),
            None => DispatchCommand::Startup {
                args: run,
                mode: StartupMode::New,
            },
        };
        let direct_harness_component = matches!(
            &command,
            DispatchCommand::Other(cli::Command::Component { name, .. }) if name == "harness"
        );
        let attaching = matches!(
            &command,
            DispatchCommand::Startup {
                mode: StartupMode::Attach(_),
                ..
            }
        );
        let dump_initial_prompt = matches!(
            &command,
            DispatchCommand::Other(cli::Command::Dev {
                command: cli::DevCommand::DumpInitialPrompt { .. }
            })
        );
        if rejects_model_reference_alias_inputs(&command) || direct_harness_component {
            reject_model_reference_alias_inputs(
                ModelReferenceAliasInputPresence {
                    provider_environment: !direct_harness_component
                        && !attaching
                        && alias_input_presence.provider_environment,
                    model_environment: !direct_harness_component
                        && !attaching
                        && alias_input_presence.model_environment,
                    ..alias_input_presence
                },
                if direct_harness_component {
                    "component harness"
                } else if dump_initial_prompt {
                    "dev dump-initial-prompt"
                } else {
                    "this command"
                },
            )?;
        }
        let alias_config_overrides = tau_config::settings::model_reference_alias_config_overrides(
            tau_config::settings::ModelReferenceAliasSources {
                provider_environment: (!direct_harness_component && !attaching)
                    .then_some(provider_alias_environment)
                    .flatten(),
                model_environment: (!direct_harness_component && !attaching)
                    .then_some(model_alias_environment)
                    .flatten(),
                provider_cli: &harness.provider_alias,
                model_cli: &harness.model_alias,
            },
        )
        .map_err(|error| CliError::Participant(error.to_string()))?;
        harness_config_overrides.extend(alias_config_overrides);
        if let DispatchCommand::Startup {
            args,
            mode: StartupMode::Attach(_),
        } = &command
        {
            reject_harness_config_overrides(&harness_config_overrides, "attach")?;
            reject_attach_startup_overrides(
                args.prompt_stdin,
                profile_was_explicitly_selected,
                harness.role.as_deref(),
                &role_cli_overrides,
                &extension_cli_overrides,
            )?;
        }
        let profile_was_selected = profile_was_explicitly_selected
            || std::env::var_os(tau_config::settings::TAU_PROFILE_ENV).is_some();
        let reads_extension_environment = match &command {
            DispatchCommand::Startup {
                mode: StartupMode::New | StartupMode::Resume(_),
                ..
            } => true,
            DispatchCommand::Other(cli::Command::Dev {
                command:
                    cli::DevCommand::PrintPrompt { .. }
                    | cli::DevCommand::PrintSystemPrompt
                    | cli::DevCommand::PrintTools
                    | cli::DevCommand::Tmux { .. },
            }) => true,
            DispatchCommand::Other(cli::Command::Component { name, .. }) => name == "harness",
            DispatchCommand::Other(cli::Command::Serve { .. }) => true,
            _ => false,
        };
        let environment_extension_names = if reads_extension_environment {
            tau_config::settings::parse_enable_extensions_env(std::env::var_os(
                tau_config::settings::TAU_ENABLE_EXTENSIONS_ENV,
            ))
            .map_err(|error| CliError::Participant(error.to_string()))?
        } else {
            Vec::new()
        };
        if reads_extension_environment {
            tau_config::settings::parse_tau_state_access_env(std::env::var_os(
                tau_config::settings::TAU_EXTENSION_TAU_STATE_ACCESS_ENV,
            ))
            .map_err(|error| CliError::Participant(error.to_string()))?;
        }
        match &command {
            DispatchCommand::Startup { .. }
            | DispatchCommand::Other(cli::Command::Dev {
                command:
                    cli::DevCommand::PrintPrompt { .. }
                    | cli::DevCommand::PrintSystemPrompt
                    | cli::DevCommand::PrintTools,
            }) => {}
            DispatchCommand::Other(cli::Command::Session { command }) => {
                let command_name = match command {
                    cli::SessionCommand::List(_) => "session list",
                    cli::SessionCommand::Show { .. } => "session show",
                    cli::SessionCommand::Stats { .. } => "session stats",
                    cli::SessionCommand::Cache(_) => "session cache",
                };
                reject_harness_config_overrides(&harness_config_overrides, command_name)?;
            }
            DispatchCommand::Other(cli::Command::Agent { command }) => {
                let command_name = match command {
                    cli::AgentCommand::List(_) => "agent list",
                    cli::AgentCommand::Unload(_) => "agent unload",
                    cli::AgentCommand::Trace(_) => "agent trace",
                    cli::AgentCommand::Cache(_) => "agent cache",
                };
                reject_harness_config_overrides(&harness_config_overrides, command_name)?;
            }
            DispatchCommand::Other(cli::Command::Init { .. }) => {
                reject_harness_config_overrides(&harness_config_overrides, "init")?;
            }
            DispatchCommand::Other(cli::Command::Provider { .. }) => {
                reject_harness_config_overrides(&harness_config_overrides, "provider")?;
            }
            DispatchCommand::Other(cli::Command::Dev {
                command: cli::DevCommand::Send { .. },
            }) => {
                reject_harness_config_overrides(&harness_config_overrides, "dev send")?;
            }
            DispatchCommand::Other(cli::Command::Dev {
                command: cli::DevCommand::Papercut { .. },
            }) => {
                reject_harness_config_overrides(&harness_config_overrides, "dev papercut")?;
            }
            DispatchCommand::Other(cli::Command::Dev {
                command: cli::DevCommand::Tmux { .. },
            }) => {
                reject_dev_tmux_startup_overrides(
                    profile_was_selected.then_some("selected"),
                    harness.role.as_deref(),
                    &role_cli_overrides,
                    &extension_cli_overrides,
                    &harness_config_overrides,
                )?;
            }
            DispatchCommand::Other(cli::Command::Dev {
                command: cli::DevCommand::DumpInitialPrompt { .. },
            }) => {
                reject_harness_config_overrides(
                    &harness_config_overrides,
                    "dev dump-initial-prompt",
                )?;
            }
            DispatchCommand::Other(cli::Command::Component { .. }) => {
                reject_harness_config_overrides(&harness_config_overrides, "component")?;
            }
            DispatchCommand::Other(cli::Command::Serve { .. }) => {}
            DispatchCommand::Other(cli::Command::Run(_))
            | DispatchCommand::Other(cli::Command::Attach { .. })
            | DispatchCommand::Other(cli::Command::Resume { .. }) => {
                unreachable!("startup variants normalize to DispatchCommand::Startup")
            }
        }

        if let DispatchCommand::Other(cli::Command::Dev {
            command: cli::DevCommand::Tmux { command },
        }) = command
        {
            return dev_tmux::run(command);
        }

        let consumes_harness_settings = consumes_harness_settings(&command);
        let selected_profile = if consumes_harness_settings {
            tau_config::settings::selected_profile(harness.profile.as_deref())
                .map_err(|error| CliError::Participant(error.to_string()))?
        } else {
            None
        };
        if consumes_harness_settings {
            tau_harness::validate_cli_overrides_with_profile(
                selected_profile.as_ref(),
                &role_cli_overrides,
                &extension_cli_overrides,
                &harness_config_overrides,
            )
            .map_err(|error| CliError::Participant(error.to_string()))?;
        }
        if reads_extension_environment && consumes_harness_settings {
            tau_harness::validate_extension_environment_and_cli_overrides_with_profile(
                selected_profile.as_ref(),
                &environment_extension_names,
                &extension_cli_overrides,
                &role_cli_overrides,
                &harness_config_overrides,
            )
            .map_err(|error| CliError::Participant(error.to_string()))?;
        }
        match command {
            DispatchCommand::Startup {
                args:
                    cli::RunArgs {
                        config,
                        prompt_stdin,
                        ephemeral,
                    },
                mode: startup_mode,
            } => {
                reject_legacy_config_path(config.as_deref())?;
                reject_ephemeral_incompatible(ephemeral, &startup_mode)?;
                let (session_id, session_status) = match &startup_mode {
                    StartupMode::Attach(session) => (
                        crate::daemon::resolve_attach_session_id(session.as_ref())?,
                        SessionLaunchStatus::Resumed,
                    ),
                    StartupMode::Resume(session) => (
                        resolve_resume_session_id(session.as_ref())?,
                        SessionLaunchStatus::Resumed,
                    ),
                    StartupMode::New => (
                        crate::daemon::mint_session_id(&std::env::current_dir()?),
                        SessionLaunchStatus::New,
                    ),
                };
                let attach = matches!(startup_mode, StartupMode::Attach(_));
                if prompt_stdin {
                    prompt_stdin::run_prompt_stdin(
                        &session_id,
                        attach,
                        session_status,
                        harness.role.as_deref(),
                        crate::daemon::DaemonCliOverrides {
                            profile: selected_profile.as_ref(),
                            role: &role_cli_overrides,
                            extension: &extension_cli_overrides,
                            extension_environment: None,
                            harness_config: &harness_config_overrides,
                            memory_only_agent_store: false,
                        },
                        ephemeral,
                    )
                } else {
                    run_chat(
                        &session_id,
                        attach,
                        session_status,
                        harness.role.as_deref(),
                        crate::daemon::DaemonCliOverrides {
                            profile: selected_profile.as_ref(),
                            role: &role_cli_overrides,
                            extension: &extension_cli_overrides,
                            extension_environment: None,
                            harness_config: &harness_config_overrides,
                            memory_only_agent_store: false,
                        },
                        ephemeral,
                    )
                }
            }

            DispatchCommand::Other(cli::Command::Session {
                command: cli::SessionCommand::List(args),
            }) => {
                reject_harness_config_overrides(&harness_config_overrides, "session list")?;
                list_sessions::run(&args)
            }

            DispatchCommand::Other(cli::Command::Session {
                command:
                    cli::SessionCommand::Show {
                        session_id,
                        sessions_dir,
                    },
            }) => {
                reject_harness_config_overrides(&harness_config_overrides, "session show")?;
                for line in tau_session_inspect::session_lines(sessions_dir, &session_id)? {
                    println!("{line}");
                }
                Ok(())
            }

            DispatchCommand::Other(cli::Command::Session {
                command:
                    cli::SessionCommand::Stats {
                        session,
                        sessions_dir,
                    },
            }) => {
                reject_harness_config_overrides(&harness_config_overrides, "session stats")?;
                let stats = tau_session_inspect::read_session_stats(&sessions_dir, &session)?
                    .ok_or_else(|| {
                        CliError::Participant(format!("session `{session}` not found"))
                    })?;
                let output = serde_toon::to_string(&stats).map_err(|error| {
                    CliError::Participant(format!("failed to serialize session stats: {error}"))
                })?;
                println!("{output}");
                Ok(())
            }

            DispatchCommand::Other(cli::Command::Agent {
                command: cli::AgentCommand::List(args),
            }) => {
                reject_harness_config_overrides(&harness_config_overrides, "agent list")?;
                list_agents::run(&args)
            }
            DispatchCommand::Other(cli::Command::Agent {
                command: cli::AgentCommand::Unload(args),
            }) => {
                reject_harness_config_overrides(&harness_config_overrides, "agent unload")?;
                unload_agent::run(&args)
            }
            DispatchCommand::Other(cli::Command::Agent {
                command: cli::AgentCommand::Cache(args),
            }) => run_cache_report(
                args.options,
                tau_session_inspect::CacheScope::Agent {
                    agent_id: args.agent_id,
                    include_descendants: args.include_descendants,
                },
            ),
            DispatchCommand::Other(cli::Command::Session {
                command: cli::SessionCommand::Cache(args),
            }) => run_cache_report(
                args.options,
                tau_session_inspect::CacheScope::Session(args.session_id),
            ),
            DispatchCommand::Other(cli::Command::Agent {
                command: cli::AgentCommand::Trace(args),
            }) => {
                reject_harness_config_overrides(&harness_config_overrides, "agent trace")?;
                let mode = match args.mode {
                    cli::AgentTraceMode::Lite => tau_session_inspect::AgentTraceMode::Lite,
                    cli::AgentTraceMode::Full => tau_session_inspect::AgentTraceMode::Full,
                };
                let format = match args.format {
                    cli::AgentTraceFormat::TauJsonl => {
                        tau_session_inspect::AgentTraceFormat::TauJsonl
                    }
                    cli::AgentTraceFormat::OtlpJson => {
                        tau_session_inspect::AgentTraceFormat::OtlpJson
                    }
                    cli::AgentTraceFormat::AgentToolsJsonl => {
                        tau_session_inspect::AgentTraceFormat::AgentToolsJsonl(mode)
                    }
                    cli::AgentTraceFormat::AgentToolsToon => {
                        tau_session_inspect::AgentTraceFormat::AgentToolsToon(mode)
                    }
                    cli::AgentTraceFormat::AgentPerformanceJsonl => {
                        tau_session_inspect::AgentTraceFormat::AgentPerformanceJsonl
                    }
                };
                validate_agent_trace_mode(args.format, args.mode)?;
                let mut output = tau_session_inspect::prepare_agent_trace(
                    &args.agents_dir,
                    &args.agent_id,
                    if args.include_descendants {
                        tau_session_inspect::DescendantSelection::Include
                    } else {
                        tau_session_inspect::DescendantSelection::RootOnly
                    },
                    format,
                )?;
                line_output::stream_stdout(|writer| output.copy_to(writer).map(|_| ()))
            }

            DispatchCommand::Other(cli::Command::Init { force }) => {
                reject_harness_config_overrides(&harness_config_overrides, "init")?;
                run_init(force)
            }

            DispatchCommand::Other(cli::Command::Provider { args }) => {
                reject_harness_config_overrides(&harness_config_overrides, "provider")?;
                tau_ext_provider_builtin::run_provider_cli(&args)
                    .map_err(|e| CliError::Participant(e.to_string()))
            }

            DispatchCommand::Other(cli::Command::Dev { command }) => match command {
                cli::DevCommand::Send { session_id, line } => {
                    reject_harness_config_overrides(&harness_config_overrides, "dev send")?;
                    send::run_send(&session_id, &line.join(" "))
                }
                cli::DevCommand::DumpInitialPrompt { out, message } => {
                    reject_harness_config_overrides(
                        &harness_config_overrides,
                        "dev dump-initial-prompt",
                    )?;
                    tau_harness::dump_initial_prompt(&out, &message)?;
                    println!("wrote {}", out.display());
                    Ok(())
                }
                cli::DevCommand::PrintPrompt { enable_agents_md } => {
                    print_prompt::run_print_prompt(
                        harness.role.as_deref(),
                        enable_agents_md,
                        selected_profile.as_ref(),
                        &role_cli_overrides,
                        &extension_cli_overrides,
                        &environment_extension_names,
                        &harness_config_overrides,
                    )
                }
                cli::DevCommand::PrintSystemPrompt => {
                    let role =
                        required_harness_role(harness.role.as_deref(), "print-system-prompt")?;
                    print_prompt::run_print_system_prompt(
                        role,
                        selected_profile.as_ref(),
                        &role_cli_overrides,
                        &extension_cli_overrides,
                        &environment_extension_names,
                        &harness_config_overrides,
                    )
                }
                cli::DevCommand::PrintTools => print_tools::run_print_tools(
                    harness.role.as_deref(),
                    selected_profile.as_ref(),
                    &role_cli_overrides,
                    &extension_cli_overrides,
                    &environment_extension_names,
                    &harness_config_overrides,
                ),
                cli::DevCommand::Papercut { command } => papercut::run(command),
                cli::DevCommand::Tmux { command } => {
                    let _ = command;
                    unreachable!("dev tmux dispatch returns before harness config validation")
                }
            },

            DispatchCommand::Other(cli::Command::Component {
                name,
                initial_ui_stdio,
            }) => {
                reject_harness_config_overrides(&harness_config_overrides, "component")?;
                if harness.profile.is_some() {
                    return Err(CliError::Participant(
                        "`tau component` cannot apply --profile in-process; set TAU_PROFILE before starting the component"
                            .to_owned(),
                    ));
                }
                if initial_ui_stdio && name != "harness" {
                    return Err(CliError::Participant(
                        "--initial-ui-stdio is only valid for `tau component harness`".to_owned(),
                    ));
                }
                if name == "harness" && initial_ui_stdio {
                    ui_logging::init_stderr_from_env("tau_harness=info,tau_cli=info,warn");
                    return run_harness_component(true)
                        .map_err(|e| CliError::Participant(e.to_string()));
                }
                if name == "harness" {
                    ui_logging::init_stderr_from_env("tau_harness=info,tau_cli=info,warn");
                    return tau_harness::run_component_with_internal_tools_and_extension_cli_overrides(
                        tau_harness_tools::builtin_handlers(),
                        extension_cli_overrides.clone(),
                    )
                    .map_err(|error| CliError::Participant(error.to_string()));
                }
                let built_in_components = [Component {
                    name: "harness",
                    runner: run_harness_component_default,
                    logging: ComponentLogging::CliStderr,
                }];
                let component = built_in_components
                    .iter()
                    .chain(components)
                    .find(|component| component.name == name)
                    .ok_or_else(|| {
                        let available = built_in_components
                            .iter()
                            .chain(components)
                            .map(|component| component.name)
                            .collect::<Vec<_>>()
                            .join(", ");
                        CliError::Participant(format!(
                            "unknown component: {name}\navailable: {available}"
                        ))
                    })?;
                match component.logging {
                    ComponentLogging::CliStderr => {
                        ui_logging::init_stderr_from_env("tau_harness=info,tau_cli=info,warn")
                    }
                    ComponentLogging::RunnerManaged => {}
                }
                (component.runner)().map_err(|e| CliError::Participant(e.to_string()))
            }
            DispatchCommand::Other(cli::Command::Serve {
                session,
                create,
                existing,
                create_or_existing: _,
                bootstrap_prompt_file,
                bootstrap_id,
                mirror_extension_stderr,
            }) => {
                ui_logging::init_stderr_from_env("tau_harness=info,tau_cli=info,warn");
                let options = tau_harness::FixedSessionServeOptions {
                    session_id: &session,
                    profile_selection: selected_profile.as_ref(),
                    startup_role: harness.role.as_deref(),
                    environment_extension_names: &environment_extension_names,
                    extension_cli_overrides: &extension_cli_overrides,
                    role_cli_overrides: &role_cli_overrides,
                    harness_config_overrides: &harness_config_overrides,
                    internal_tool_handlers: tau_harness_tools::builtin_handlers(),
                    bootstrap: bootstrap_prompt_file
                        .as_deref()
                        .zip(bootstrap_id.as_ref())
                        .map(|(prompt_file, id)| tau_harness::BootstrapPromptOptions {
                            prompt_file,
                            id,
                        }),
                    mirror_extension_stderr,
                };
                if create {
                    tau_harness::run_create_session_component_with_internal_tools(options)
                } else if existing {
                    tau_harness::run_existing_session_component_with_internal_tools(options)
                } else {
                    tau_harness::run_create_or_existing_session_component_with_internal_tools(
                        options,
                    )
                }
                .map_err(|error| CliError::Participant(error.to_string()))
            }
            DispatchCommand::Other(
                cli::Command::Run(_) | cli::Command::Attach { .. } | cli::Command::Resume { .. },
            ) => {
                unreachable!("startup variants normalize to DispatchCommand::Startup")
            }
        }
    };

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if error.should_report_to_terminal() {
                eprintln!("error: {error}");
            }
            error.exit_code()
        }
    }
}

/// Runs the dependency-light cache inspector without connecting to a daemon.
fn run_cache_report(
    args: cli::CacheArgs,
    scope: tau_session_inspect::CacheScope,
) -> Result<(), CliError> {
    let options = tau_session_inspect::CacheOptions {
        state_dir: args.state_dir,
        scope,
        prompt: args.prompt,
        selection: tau_session_inspect::CacheSelection {
            since_unix_micros: args.since,
            until_unix_micros: args.until,
            model: args.model,
            operation: args.operation.map(|operation| match operation {
                cli::CacheOperation::Inference => tau_session_inspect::CacheOperation::Inference,
                cli::CacheOperation::StandaloneCompaction => {
                    tau_session_inspect::CacheOperation::StandaloneCompaction
                }
                cli::CacheOperation::CacheRefresh => {
                    tau_session_inspect::CacheOperation::CacheRefresh
                }
            }),
            attempt: args.attempt,
            require_exact_chain: args.require_exact_chain,
            group_by: args
                .group_by
                .into_iter()
                .map(|group| match group {
                    cli::CacheGroup::Model => tau_session_inspect::CacheGroup::Model,
                    cli::CacheGroup::Backend => tau_session_inspect::CacheGroup::Backend,
                    cli::CacheGroup::Controls => tau_session_inspect::CacheGroup::Controls,
                })
                .collect(),
        },
        view: match args.view {
            cli::CacheView::Summary => tau_session_inspect::CacheView::Summary,
            cli::CacheView::Attribution => tau_session_inspect::CacheView::Attribution,
            cli::CacheView::Continuity => tau_session_inspect::CacheView::Continuity,
            cli::CacheView::Geometry => tau_session_inspect::CacheView::Geometry,
            cli::CacheView::Gaps => tau_session_inspect::CacheView::Gaps,
        },
        limits: tau_session_inspect::CacheScanLimits {
            compressed_file_bytes: args.max_compressed_bytes,
            decompressed_file_bytes: args.max_decompressed_bytes,
            total_decompressed_bytes: args.max_total_bytes,
            working_memory_bytes: args.max_memory_bytes,
        },
        producer_build: build_revision(),
        index: args.index,
    };
    let report =
        tau_session_inspect::read_cache_report(&options).map_err(|_| CliError::CacheInvalid)?;
    line_output::stream_stdout(|writer| match (args.format, args.view) {
        (cli::CacheFormat::Summary, cli::CacheView::Summary) => report.write_summary(writer),
        (cli::CacheFormat::Summary, _) | (cli::CacheFormat::Jsonl, _) => report.write_jsonl(writer),
    })?;
    if report.index_written() {
        eprintln!(
            "Wrote a private disposable cache index; it contains linkable evidence and must be deleted manually when no longer needed."
        );
    }
    if report.is_partial() {
        Err(CliError::CachePartial)
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
