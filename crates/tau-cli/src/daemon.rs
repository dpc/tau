//! Harness daemon lifecycle: discovery, spawning, and initial UI wiring.

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tau_cli_picker::{PickerError, PickerItem, pick};
use tau_harness::{SessionLaunchStatus, runtime_dir};

use crate::{CliError, mint_short_id};

const RESUME_PICKER_LIMIT: usize = 10;
const SESSION_ID_SUFFIX_BYTES: usize = 7;
const SESSION_ID_MAX_BYTES: usize = tau_proto::SESSION_SCOPED_ID_MAX_LEN;
const OWNED_DAEMON_GRACEFUL_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const OWNED_DAEMON_EXIT_CHECK_INTERVAL: Duration = Duration::from_millis(10);

/// How this CLI invocation is related to its harness daemon.
///
/// - `Owned`: we spawned the daemon; Drop kills it unless the UI detached
///   (calls [`DaemonHandle::leak`]), in which case we forget the `Child` so the
///   daemon outlives us.
/// - `Attached`: we joined a daemon someone else owns. Drop never touches it.
pub(crate) struct InitialUiStdio {
    pub(crate) stdin: std::process::ChildStdin,
    pub(crate) stdout: std::process::ChildStdout,
}

pub(crate) enum DaemonHandle {
    /// `child` is `Some` until [`leak`] pulls it out.
    Owned {
        child: Option<std::process::Child>,
        harness_path: PathBuf,
        initial_ui: Option<InitialUiStdio>,
    },
    Attached {
        harness_path: PathBuf,
    },
}

impl DaemonHandle {
    pub(crate) fn socket_path(&self) -> PathBuf {
        runtime_dir::socket_path(self.harness_path())
    }

    /// Transfers the initial transport to a connected client.
    ///
    /// The client must close the returned input/write half before dropping this
    /// handle so the daemon observes EOF before the handle waits for normal
    /// exit.
    pub(crate) fn take_initial_ui_stdio(&mut self) -> Option<InitialUiStdio> {
        match self {
            Self::Owned { initial_ui, .. } => initial_ui.take(),
            Self::Attached { .. } => None,
        }
    }

    fn harness_path(&self) -> &Path {
        match self {
            Self::Owned { harness_path, .. } | Self::Attached { harness_path } => harness_path,
        }
    }

    /// Consume the handle without killing the child.
    ///
    /// Used by `:detach`: we want the daemon to outlive this CLI,
    /// whether we spawned it or attached to it. For `Owned` this
    /// `mem::forget`s the `Child` — on Linux its parent becomes init
    /// on our exit, which is exactly what we want for a long-lived
    /// daemon.
    pub(crate) fn leak(mut self) {
        if let Self::Owned { child, .. } = &mut self
            && let Some(child) = child.take()
        {
            std::mem::forget(child);
        }
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        if let Self::Owned {
            child, initial_ui, ..
        } = self
            && let Some(mut child) = child.take()
        {
            stop_owned_daemon(
                &mut child,
                initial_ui.take(),
                OWNED_DAEMON_GRACEFUL_EXIT_TIMEOUT,
            );
        }
        // Attached, or Owned-after-leak: do nothing. The daemon keeps
        // running so other UIs can still use it, or this same UI can
        // `tau -a` back in later.
    }
}

/// Stops a CLI-owned daemon, preferring its normal disconnect cleanup while
/// retaining a bounded forced-termination fallback.
fn stop_owned_daemon(
    child: &mut std::process::Child,
    initial_ui: Option<InitialUiStdio>,
    graceful_timeout: Duration,
) {
    // Closing the initial UI transport is the daemon's normal shutdown signal.
    // Give exit-on-disconnect time to stop extensions and remove runtime files.
    drop(initial_ui);
    let deadline = Instant::now() + graceful_timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                // dpc approved this bounded polling exception. std::process has
                // no portable timed child-exit notification; revisit when Rust
                // adds one or Tau adopts a race-safe timed process-handle API.
                std::thread::sleep(OWNED_DAEMON_EXIT_CHECK_INTERVAL);
            }
            Ok(None) | Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Resolves the session id for one `tau` invocation.
///
/// - `None` → mint `<basename(cwd)>-<rand6>`.
/// - `Some("")` (bare `-r`) → interactively pick among recent sessions whose
///   metadata matches cwd; if none, mint fresh.
/// - `Some(id)` → resume that explicit id; error if it does not exist.
pub(crate) fn resolve_run_session_id(
    resume: Option<&str>,
) -> Result<(tau_proto::SessionId, SessionLaunchStatus), CliError> {
    let cwd = std::env::current_dir()?;
    match resume {
        None => Ok((mint_session_id(&cwd), SessionLaunchStatus::New)),
        Some("") => match pick_resume_session(&cwd)? {
            Some(id) => Ok((id, SessionLaunchStatus::Resumed)),
            None => Ok((mint_session_id(&cwd), SessionLaunchStatus::New)),
        },
        Some(id) => {
            let id = tau_proto::SessionId::parse(id).map_err(|error| {
                CliError::Participant(format!("invalid session id `{id}`: {error}"))
            })?;
            if session_exists(&id)? {
                Ok((id, SessionLaunchStatus::Resumed))
            } else {
                Err(CliError::SessionNotFound(id.to_string()))
            }
        }
    }
}

fn session_exists(id: &tau_proto::SessionId) -> Result<bool, CliError> {
    let sessions_dir = tau_session_inspect::default_sessions_dir();
    let metas = tau_harness::list_session_metas(&sessions_dir)?;
    Ok(metas
        .into_iter()
        .any(|(session_id, _)| session_id.as_str() == id.as_str()))
}

pub(crate) fn mint_session_id(cwd: &Path) -> tau_proto::SessionId {
    let basename = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session");
    tau_proto::SessionId::parse(mint_short_id(&sanitize_session_id_prefix(basename)))
        .expect("sanitized session id minting must satisfy the protocol grammar")
}

fn sanitize_session_id_prefix(prefix: &str) -> String {
    let mut prefix = prefix
        .bytes()
        .map(|byte| match byte {
            byte if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') => char::from(byte),
            _ => '_',
        })
        .collect::<String>();
    let max_prefix_bytes = SESSION_ID_MAX_BYTES - SESSION_ID_SUFFIX_BYTES;
    while prefix.len() > max_prefix_bytes {
        prefix.pop();
    }
    if prefix == "." || prefix == ".." || prefix.is_empty() {
        "session".to_owned()
    } else {
        prefix
    }
}

fn pick_resume_session(_cwd: &Path) -> Result<Option<tau_proto::SessionId>, CliError> {
    let sessions_dir = tau_session_inspect::default_sessions_dir();
    let mut metas = tau_harness::list_session_metas(&sessions_dir)?;
    metas.sort_by_key(|(_, meta)| std::cmp::Reverse(meta.last_touched));
    metas.truncate(RESUME_PICKER_LIMIT);
    if metas.is_empty() {
        return Ok(None);
    }
    if metas.len() == 1 || !io::IsTerminal::is_terminal(&io::stdin()) {
        return Ok(metas.first().map(|(sid, _)| sid.clone()));
    }

    let rows = metas
        .into_iter()
        .map(|(sid, _meta)| {
            let locked = tau_harness::session_is_locked(&sessions_dir, sid.as_str())
                .unwrap_or_else(|error| {
                    tracing::warn!(
                        target: "tau_cli::startup",
                        session_id = sid.as_str(),
                        %error,
                        "could not determine session lock state — assuming unlocked"
                    );
                    false
                });
            let item = sid.as_str().to_owned();
            let id = sid;
            (id, item, locked)
        })
        .collect::<Vec<_>>();
    if rows.iter().all(|(_, _, locked)| *locked) {
        return Ok(None);
    }
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: unwrap-or-default
    let default = rows
        .iter()
        .position(|(_, _, locked)| !*locked)
        .unwrap_or_default();
    if rows.iter().filter(|(_, _, locked)| !*locked).count() == 1 {
        return Ok(Some(rows[default].0.clone()));
    }
    let items = rows
        .iter()
        .map(|(_, item, locked)| {
            if *locked {
                PickerItem::disabled(item)
            } else {
                PickerItem::enabled(item)
            }
        })
        .collect::<Vec<_>>();
    let selection = match pick("Resume session", &items) {
        Ok(selection) => selection,
        Err(PickerError::Cancelled) => return Ok(None),
        Err(e) => return Err(CliError::Participant(e.to_string())),
    };
    Ok(Some(rows[selection].0.clone()))
}

pub(crate) struct DaemonOutput {
    pub(crate) stderr: Stdio,
}

pub(crate) fn daemon_output_for_session(
    session_id: &str,
    ephemeral: bool,
) -> Result<DaemonOutput, CliError> {
    if ephemeral {
        return Ok(DaemonOutput {
            stderr: Stdio::null(),
        });
    }
    // Route the daemon's stderr (where its tracing subscriber writes) into the
    // per-session harness log so it sits next to per-extension logs under
    // `<session>/logs/`. The CLI's own tracing still goes to `ui.log`; the two
    // streams are intentionally separated so a session post-mortem doesn't need
    // to pull from two places.
    let sessions_dir = tau_session_inspect::default_sessions_dir();
    let harness_log = tau_harness::harness_log_path(&sessions_dir, session_id);
    if let Some(parent) = harness_log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&harness_log)
        .map(Stdio::from)?;
    Ok(DaemonOutput { stderr })
}

pub(crate) struct DaemonCliOverrides<'a> {
    pub(crate) role: &'a [tau_config::settings::RoleCliOverride],
    pub(crate) extension: &'a [tau_config::settings::ExtensionCliOverride],
    /// Parsed public extension environment to forward deterministically.
    pub(crate) extension_environment: Option<&'a [String]>,
    pub(crate) harness_config: &'a [tau_config::settings::HarnessConfigCliOverride],
}

pub(crate) fn resolve_daemon(
    attach: bool,
    session_id: &str,
    session_status: SessionLaunchStatus,
    daemon_output: Option<DaemonOutput>,
    startup_role: Option<&str>,
    cli_overrides: DaemonCliOverrides<'_>,
    ephemeral: bool,
) -> Result<DaemonHandle, CliError> {
    tracing::debug!(target: "tau_cli::startup", attach, session_id, "resolving harness daemon");
    let project_root = std::env::current_dir()?;
    if attach {
        tracing::debug!(target: "tau_cli::startup", project_root = %project_root.display(), "looking for existing harness daemon");
        let harness_path =
            runtime_dir::find_harness_for_dir(&project_root).ok_or(CliError::NoRunningDaemon)?;
        tracing::debug!(target: "tau_cli::startup", harness_path = %harness_path.display(), "attached harness daemon resolved");
        return Ok(DaemonHandle::Attached { harness_path });
    }
    start_daemon(
        session_id,
        session_status,
        daemon_output.expect("daemon output for spawned harness"),
        startup_role,
        cli_overrides,
        ephemeral,
    )
}

/// Spawns a new harness daemon.
///
/// Child stdin/stdout are reserved for the initial UI protocol and returned
/// immediately; the harness delays extension startup internally until that UI
/// sends its subscribe message.
fn start_daemon(
    session_id: &str,
    session_status: SessionLaunchStatus,
    output: DaemonOutput,
    startup_role: Option<&str>,
    cli_overrides: DaemonCliOverrides<'_>,
    ephemeral: bool,
) -> Result<DaemonHandle, CliError> {
    let tau_binary = std::env::current_exe()?;
    tracing::debug!(target: "tau_cli::startup", tau_binary = %tau_binary.display(), session_id, "spawning harness daemon");

    let mut command = build_daemon_command(DaemonCommandSpec {
        tau_binary: &tau_binary,
        session_id,
        session_status,
        stdout: Stdio::piped(),
        stderr: output.stderr,
        stdin: Stdio::piped(),
        startup_role,
        cli_overrides,
        ephemeral,
    });
    let runtime_instance_id = configure_runtime_instance(&mut command);
    let spawn_result = command.spawn();

    let mut child = spawn_result?;

    tracing::debug!(target: "tau_cli::startup", pid = child.id(), "harness daemon spawned");
    let harness_path = runtime_dir::harness_path_for_process(child.id(), &runtime_instance_id);
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| CliError::Participant("missing harness stdin pipe".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CliError::Participant("missing harness stdout pipe".to_owned()))?;
    Ok(DaemonHandle::Owned {
        child: Some(child),
        harness_path,
        initial_ui: Some(InitialUiStdio { stdin, stdout }),
    })
}

/// Assigns one random runtime identity to both sides of a CLI-managed spawn.
fn configure_runtime_instance(command: &mut Command) -> runtime_dir::HarnessInstanceId {
    let instance_id = runtime_dir::HarnessInstanceId::mint();
    command.env(runtime_dir::HARNESS_INSTANCE_ID_ENV, instance_id.as_str());
    instance_id
}

struct DaemonCommandSpec<'a> {
    tau_binary: &'a Path,
    session_id: &'a str,
    session_status: SessionLaunchStatus,
    stdout: Stdio,
    stderr: Stdio,
    stdin: Stdio,
    startup_role: Option<&'a str>,
    cli_overrides: DaemonCliOverrides<'a>,
    ephemeral: bool,
}

/// Build the `tau component harness` command, reserving stdio for the initial
/// UI protocol.
fn build_daemon_command(spec: DaemonCommandSpec<'_>) -> Command {
    let mut cmd = Command::new(spec.tau_binary);
    cmd.arg("component")
        .arg("harness")
        .env("TAU_SESSION_ID", spec.session_id)
        .env("TAU_SESSION_STATUS", spec.session_status.as_str())
        // TAU_VERSION/TAU_BUILD/TAU_LAST_MODIFIED used to be forwarded
        // here; the harness child now reads its own `built` snapshot
        // (see `tau_harness::version::export_to_env`) and publishes
        // them to its own environment instead.
        // Default-enable info logging in the child process so `tau`
        // captures harness logs without requiring an env var. Users
        // can still override/filter with `TAU_LOG`.
        .env(
            "TAU_LOG",
            // Preserve behavior at this site.
            // ast-grep-ignore: silent-unwrap-or-else
            std::env::var("TAU_LOG").unwrap_or_else(|_| {
                "tau_harness=info,tau_cli=info,provider-builtin=info".to_owned()
            }),
        )
        .stdin(spec.stdin)
        .stdout(spec.stdout)
        .stderr(spec.stderr);

    for key in [
        "LISTEN_FDS",
        "LISTEN_PID",
        "LISTEN_FDS_FIRST_FD",
        "LISTEN_FDNAMES",
    ] {
        cmd.env_remove(key);
    }

    if let Some(role) = spec.startup_role.filter(|role| !role.is_empty()) {
        cmd.env(tau_harness::STARTUP_ROLE_ENV, role);
    }
    if spec.ephemeral {
        cmd.env(tau_harness::EPHEMERAL_ENV, "1");
    } else {
        cmd.env_remove(tau_harness::EPHEMERAL_ENV);
    }
    if !spec.cli_overrides.role.is_empty() {
        cmd.env(
            tau_harness::ROLE_CLI_OVERRIDES_ENV,
            serde_json::to_string(spec.cli_overrides.role).expect("role overrides serialize"),
        );
    }
    if !spec.cli_overrides.extension.is_empty() {
        cmd.env(
            tau_harness::EXTENSION_CLI_OVERRIDES_ENV,
            serde_json::to_string(spec.cli_overrides.extension)
                .expect("extension overrides serialize"),
        );
    } else {
        cmd.env_remove(tau_harness::EXTENSION_CLI_OVERRIDES_ENV);
    }
    if let Some(names) = spec.cli_overrides.extension_environment {
        cmd.env(
            tau_config::settings::TAU_ENABLE_EXTENSIONS_ENV,
            names.join(","),
        );
    }
    if !spec.cli_overrides.harness_config.is_empty() {
        cmd.env(
            tau_harness::HARNESS_CONFIG_CLI_OVERRIDES_ENV,
            serde_json::to_string(spec.cli_overrides.harness_config)
                .expect("harness config overrides serialize"),
        );
    } else {
        cmd.env_remove(tau_harness::HARNESS_CONFIG_CLI_OVERRIDES_ENV);
    }

    cmd.arg("--initial-ui-stdio");

    cmd
}

#[cfg(test)]
mod tests;
