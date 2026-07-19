//! CLI entrypoint for tau: starts a harness daemon and connects as a
//! socket client for interactive chat.
//!
//! Component and local-execution boundaries are summarized in `ARCH-tau-cli`.
//! Transcript presentation follows `SPEC-tau-cli-transcript-styling`.

pub mod cli;

mod action_commands;
mod agent_activity;
mod agent_navigation;
mod chat;
mod daemon;
mod dev_tmux;
mod event_renderer;
mod list_agents;
mod markdown_render;
mod message_fact_render;
mod print_prompt;
mod print_tools;
mod prompt_history;
mod prompt_stdin;
mod provider_quota;
mod render_request;
mod send;
mod settings_registry;
mod skill_commands;
mod theme;
mod tool_render;
mod ui_client;
mod ui_commands;
mod ui_events;
mod ui_logging;
mod ui_prompt;
mod watch_activity;

use std::sync::{Mutex, MutexGuard};
use std::{fmt, io};

use tau_harness::{SessionLaunchStatus, runtime_dir};

use crate::chat::run_chat;
use crate::daemon::resolve_run_session_id;

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
    Io(io::Error),
    Encode(tau_proto::EncodeError),
    Harness(tau_harness::HarnessError),
    Inspect(tau_session_inspect::InspectError),
    DaemonExited(String),
    NoRunningDaemon,
    Participant(String),
    SessionNotFound(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(f, "I/O error: {source}"),
            Self::Encode(source) => write!(f, "encode error: {source}"),
            Self::Harness(source) => write!(f, "harness error: {source}"),
            Self::Inspect(source) => write!(f, "inspect error: {source}"),
            Self::DaemonExited(msg) => write!(f, "harness daemon exited: {msg}"),
            Self::NoRunningDaemon => f.write_str(
                "no harness daemon running for this project — \
                 drop `--attach` to spawn one",
            ),
            Self::Participant(msg) => write!(f, "participant error: {msg}"),
            Self::SessionNotFound(id) => write!(f, "session not found: `{id}`"),
        }
    }
}

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
            overrides.push(tau_config::settings::RoleCliOverride::DisableAll);
        } else if let Some(role) = arg.strip_prefix("--enable-role=") {
            overrides.push(tau_config::settings::RoleCliOverride::Enable(
                role.to_owned(),
            ));
        } else if arg == "--enable-role" {
            if let Some(role) = args.next() {
                overrides.push(tau_config::settings::RoleCliOverride::Enable(
                    role.to_string_lossy().into_owned(),
                ));
            }
        } else if let Some(role) = arg.strip_prefix("--disable-role=") {
            overrides.push(tau_config::settings::RoleCliOverride::Disable(
                role.to_owned(),
            ));
        } else if arg == "--disable-role"
            && let Some(role) = args.next()
        {
            overrides.push(tau_config::settings::RoleCliOverride::Disable(
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

fn reject_dev_tmux_startup_overrides(
    startup_role: Option<&str>,
    role_cli_overrides: &[tau_config::settings::RoleCliOverride],
    extension_cli_overrides: &[tau_config::settings::ExtensionCliOverride],
    harness_config_overrides: &[tau_config::settings::HarnessConfigCliOverride],
) -> Result<(), CliError> {
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
    startup_role: Option<&str>,
    role_cli_overrides: &[tau_config::settings::RoleCliOverride],
    extension_cli_overrides: &[tau_config::settings::ExtensionCliOverride],
) -> Result<(), CliError> {
    if startup_role.is_some() && !prompt_stdin {
        return Err(CliError::Participant(
            "--attach cannot apply --role to an already-running interactive daemon; use --prompt-stdin if you want --role for the submitted prompt".to_owned(),
        ));
    }
    if !role_cli_overrides.is_empty() {
        return Err(CliError::Participant(
            "--attach cannot apply role enable/disable overrides to an already-running daemon"
                .to_owned(),
        ));
    }
    if !extension_cli_overrides.is_empty() {
        return Err(CliError::Participant(
            "--attach cannot apply extension enable/disable overrides to an already-running daemon"
                .to_owned(),
        ));
    }
    Ok(())
}

fn reject_attach_extension_environment(environment_names: &[String]) -> Result<(), CliError> {
    if environment_names.is_empty() {
        return Ok(());
    }
    Err(CliError::Participant(format!(
        "--attach cannot apply {} to an already-running daemon",
        tau_config::settings::TAU_ENABLE_EXTENSIONS_ENV
    )))
}

fn reject_ephemeral_incompatible(
    ephemeral: bool,
    attach: bool,
    resume: Option<&str>,
) -> Result<(), CliError> {
    if ephemeral && attach {
        return Err(CliError::Participant(
            "--ephemeral cannot be combined with --attach".to_owned(),
        ));
    }
    if ephemeral && resume.is_some() {
        return Err(CliError::Participant(
            "--ephemeral cannot be combined with --resume".to_owned(),
        ));
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
            overrides.push(tau_config::settings::ExtensionCliOverride::EnableAll);
        } else if arg == "--disable-extensions-all" {
            overrides.push(tau_config::settings::ExtensionCliOverride::DisableAll);
        } else if let Some(extension) = arg.strip_prefix("--enable-extension=") {
            overrides.push(tau_config::settings::ExtensionCliOverride::Enable(
                extension.to_owned(),
            ));
        } else if arg == "--enable-extension" {
            if let Some(extension) = args.next() {
                overrides.push(tau_config::settings::ExtensionCliOverride::Enable(
                    extension.to_string_lossy().into_owned(),
                ));
            }
        } else if let Some(extension) = arg.strip_prefix("--disable-extension=") {
            overrides.push(tau_config::settings::ExtensionCliOverride::Disable(
                extension.to_owned(),
            ));
        } else if arg == "--disable-extension"
            && let Some(extension) = args.next()
        {
            overrides.push(tau_config::settings::ExtensionCliOverride::Disable(
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
    use std::process::ExitCode;

    use clap::Parser;

    let run = || -> Result<(), CliError> {
        let role_cli_overrides = parse_role_cli_overrides(std::env::args_os());
        let extension_cli_overrides = parse_extension_cli_overrides(std::env::args_os());
        let harness_config_overrides = parse_harness_config_cli_overrides(std::env::args_os())
            .map_err(CliError::Participant)?;
        let cli::Cli {
            version,
            harness,
            run,
            command,
        } = cli::Cli::parse();
        if version {
            println!("{}", version_label());
            return Ok(());
        }

        reject_legacy_config_path(run.config.as_deref())?;
        let command = command.unwrap_or(cli::Command::Run(run));
        let reads_extension_environment = match &command {
            cli::Command::Run(_) => true,
            cli::Command::Dev {
                command:
                    cli::DevCommand::PrintPrompt { .. }
                    | cli::DevCommand::PrintSystemPrompt
                    | cli::DevCommand::PrintTools
                    | cli::DevCommand::Tmux { .. },
            } => true,
            cli::Command::Component { name, .. } => name == "harness",
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
        match &command {
            cli::Command::Run(run) if run.attach => {
                reject_harness_config_overrides(&harness_config_overrides, "--attach")?;
                reject_attach_extension_environment(&environment_extension_names)?;
            }
            cli::Command::Run(_)
            | cli::Command::Dev {
                command:
                    cli::DevCommand::PrintPrompt { .. }
                    | cli::DevCommand::PrintSystemPrompt
                    | cli::DevCommand::PrintTools,
            } => {}
            cli::Command::Session { command } => {
                let command_name = match command {
                    cli::SessionCommand::List { .. } => "session list",
                    cli::SessionCommand::Show { .. } => "session show",
                };
                reject_harness_config_overrides(&harness_config_overrides, command_name)?;
            }
            cli::Command::Agent { .. } => {
                reject_harness_config_overrides(&harness_config_overrides, "agent list")?;
            }
            cli::Command::PolicyShow { .. } => {
                reject_harness_config_overrides(&harness_config_overrides, "policy-show")?;
            }
            cli::Command::Init { .. } => {
                reject_harness_config_overrides(&harness_config_overrides, "init")?;
            }
            cli::Command::Provider { .. } => {
                reject_harness_config_overrides(&harness_config_overrides, "provider")?;
            }
            cli::Command::Dev {
                command: cli::DevCommand::Send { .. },
            } => {
                reject_harness_config_overrides(&harness_config_overrides, "dev send")?;
            }
            cli::Command::Dev {
                command: cli::DevCommand::Tmux { .. },
            } => {
                reject_dev_tmux_startup_overrides(
                    harness.role.as_deref(),
                    &role_cli_overrides,
                    &extension_cli_overrides,
                    &harness_config_overrides,
                )?;
            }
            cli::Command::Dev {
                command: cli::DevCommand::DumpInitialPrompt { .. },
            } => {
                reject_harness_config_overrides(
                    &harness_config_overrides,
                    "dev dump-initial-prompt",
                )?;
            }
            cli::Command::Component { .. } => {
                reject_harness_config_overrides(&harness_config_overrides, "component")?;
            }
        }

        if let cli::Command::Dev {
            command: cli::DevCommand::Tmux { command },
        } = command
        {
            return dev_tmux::run(command);
        }

        tau_harness::validate_cli_overrides(
            &role_cli_overrides,
            &extension_cli_overrides,
            &harness_config_overrides,
        )
        .map_err(|error| CliError::Participant(error.to_string()))?;
        if reads_extension_environment {
            tau_harness::validate_extension_environment_and_cli_overrides(
                &environment_extension_names,
                &extension_cli_overrides,
                &role_cli_overrides,
                &harness_config_overrides,
            )
            .map_err(|error| CliError::Participant(error.to_string()))?;
        }
        match command {
            cli::Command::Run(cli::RunArgs {
                resume,
                config,
                prompt_stdin,
                attach,
                ephemeral,
            }) => {
                reject_legacy_config_path(config.as_deref())?;
                reject_ephemeral_incompatible(ephemeral, attach, resume.as_deref())?;
                if attach {
                    reject_attach_startup_overrides(
                        prompt_stdin,
                        harness.role.as_deref(),
                        &role_cli_overrides,
                        &extension_cli_overrides,
                    )?;
                }
                let (session_id, session_status) = if attach {
                    reject_harness_config_overrides(&harness_config_overrides, "--attach")?;
                    let cwd = std::env::current_dir()?;
                    let harness_path =
                        runtime_dir::find_harness_for_dir(&cwd).ok_or(CliError::NoRunningDaemon)?;
                    let daemon_session_id = runtime_dir::read_session_id(&harness_path)
                        .ok_or_else(|| {
                            CliError::Participant(
                                "running daemon did not publish its session id".to_owned(),
                            )
                        })?;
                    if let Some(requested) = resume.as_deref().filter(|s| !s.is_empty())
                        && requested != daemon_session_id
                    {
                        return Err(CliError::Participant(format!(
                            "--attach: daemon is bound to session `{daemon_session_id}`, \
                             cannot resume `{requested}` (start a fresh daemon for that)"
                        )));
                    }
                    (daemon_session_id, SessionLaunchStatus::Resumed)
                } else {
                    resolve_run_session_id(resume.as_deref())?
                };
                if prompt_stdin {
                    prompt_stdin::run_prompt_stdin(
                        &session_id,
                        attach,
                        session_status,
                        harness.role.as_deref(),
                        crate::daemon::DaemonCliOverrides {
                            role: &role_cli_overrides,
                            extension: &extension_cli_overrides,
                            extension_environment: None,
                            harness_config: &harness_config_overrides,
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
                            role: &role_cli_overrides,
                            extension: &extension_cli_overrides,
                            extension_environment: None,
                            harness_config: &harness_config_overrides,
                        },
                        ephemeral,
                    )
                }
            }

            cli::Command::Session {
                command: cli::SessionCommand::List { sessions_dir },
            } => {
                reject_harness_config_overrides(&harness_config_overrides, "session list")?;
                for line in tau_session_inspect::session_list_lines(sessions_dir)? {
                    println!("{line}");
                }
                Ok(())
            }

            cli::Command::Session {
                command:
                    cli::SessionCommand::Show {
                        session_id,
                        sessions_dir,
                    },
            } => {
                reject_harness_config_overrides(&harness_config_overrides, "session show")?;
                for line in tau_session_inspect::session_lines(sessions_dir, &session_id)? {
                    println!("{line}");
                }
                Ok(())
            }

            cli::Command::Agent {
                command: cli::AgentCommand::List(args),
            } => {
                reject_harness_config_overrides(&harness_config_overrides, "agent list")?;
                list_agents::run(&args)
            }

            cli::Command::PolicyShow { state_dir } => {
                reject_harness_config_overrides(&harness_config_overrides, "policy-show")?;
                for line in tau_session_inspect::policy_lines(state_dir.join("policy.cbor"))? {
                    println!("{line}");
                }
                Ok(())
            }

            cli::Command::Init { force } => {
                reject_harness_config_overrides(&harness_config_overrides, "init")?;
                run_init(force)
            }

            cli::Command::Provider { args } => {
                reject_harness_config_overrides(&harness_config_overrides, "provider")?;
                tau_ext_provider_builtin::run_provider_cli(&args)
                    .map_err(|e| CliError::Participant(e.to_string()))
            }

            cli::Command::Dev { command } => match command {
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
                    let role = required_harness_role(harness.role.as_deref(), "print-prompt")?;
                    print_prompt::run_print_prompt(
                        role,
                        enable_agents_md,
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
                        &role_cli_overrides,
                        &extension_cli_overrides,
                        &environment_extension_names,
                        &harness_config_overrides,
                    )
                }
                cli::DevCommand::PrintTools => {
                    let role = required_harness_role(harness.role.as_deref(), "print-tools")?;
                    print_tools::run_print_tools(
                        role,
                        &role_cli_overrides,
                        &extension_cli_overrides,
                        &environment_extension_names,
                        &harness_config_overrides,
                    )
                }
                cli::DevCommand::Tmux { command } => {
                    let _ = command;
                    unreachable!("dev tmux dispatch returns before harness config validation")
                }
            },

            cli::Command::Component {
                name,
                initial_ui_stdio,
            } => {
                reject_harness_config_overrides(&harness_config_overrides, "component")?;
                if initial_ui_stdio && name != "harness" {
                    return Err(CliError::Participant(
                        "--initial-ui-stdio is only valid for `tau component harness`".to_owned(),
                    ));
                }
                if name == "harness" && initial_ui_stdio {
                    ui_logging::init_stderr_from_env(
                        "tau_harness=info,tau_cli=info,provider-builtin=info",
                    );
                    return run_harness_component(true)
                        .map_err(|e| CliError::Participant(e.to_string()));
                }
                if name == "harness" {
                    ui_logging::init_stderr_from_env(
                        "tau_harness=info,tau_cli=info,provider-builtin=info",
                    );
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
                    ComponentLogging::CliStderr => ui_logging::init_stderr_from_env(
                        "tau_harness=info,tau_cli=info,provider-builtin=info",
                    ),
                    ComponentLogging::RunnerManaged => {}
                }
                (component.runner)().map_err(|e| CliError::Participant(e.to_string()))
            }
        }
    };

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
