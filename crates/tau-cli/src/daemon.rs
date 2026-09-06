//! Harness daemon lifecycle: discovery, spawning, and initial UI wiring.

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tau_cli_picker::{PickerError, PickerItem, pick};
use tau_harness::{HarnessStorageMode, SessionLaunchStatus, runtime_dir};

use crate::{CliError, mint_short_id};

const RESUME_PICKER_LIMIT: usize = 10;
const SESSION_ID_SUFFIX_BYTES: usize = 7;
const SESSION_ID_MAX_BYTES: usize = tau_proto::SESSION_SCOPED_ID_MAX_LEN;
const OWNED_DAEMON_EXIT_CHECK_INTERVAL: Duration = Duration::from_millis(10);
pub(crate) const REQUESTED_DAEMON_EXIT_WAIT: Duration = Duration::from_secs(10);

/// How this CLI invocation is related to its harness daemon.
///
/// - `Owned`: we spawned the daemon. Dropping the UI handle never kills it;
///   daemon lifetime belongs to the bound session.
/// - `Attached`: we joined a daemon someone else owns. Drop never touches it.
pub(crate) struct InitialUiStdio {
    /// Parent-side writer connected to the harness's standard input.
    pub(crate) stdin: Box<dyn Write + Send>,
    /// Parent-side reader connected to the harness's standard output.
    pub(crate) stdout: Box<dyn Read + Send>,
    /// Read-side endpoint retained so the UI can interrupt a blocking read.
    pub(crate) shutdown_stream: Option<UnixStream>,
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
    /// Used when the harness acknowledges survival or termination remains
    /// unconfirmed. The harness's own lifetime policy controls ordinary EOF;
    /// dropping this process handle never kills it. For `Owned` this
    /// `mem::forget`s the `Child` — on Linux its parent becomes init
    /// on our exit, which is exactly what we want for a long-lived
    /// daemon when policy keeps it running.
    pub(crate) fn leak(mut self) {
        if let Self::Owned { child, .. } = &mut self
            && let Some(child) = child.take()
        {
            std::mem::forget(child);
        }
    }

    /// Waits boundedly for an explicit canonical shutdown without ever forcing
    /// the child to exit.
    ///
    /// A child still running after `timeout` is detached so its harness-owned
    /// cleanup may continue independently. Returns a confirmed child exit
    /// status, or `None` for an attached daemon, timeout, or wait failure.
    pub(crate) fn wait_requested_exit_or_leak(
        mut self,
        timeout: Duration,
    ) -> Option<std::process::ExitStatus> {
        let Self::Owned {
            child, initial_ui, ..
        } = &mut self
        else {
            return None;
        };
        drop(initial_ui.take());
        let mut child = child.take()?;
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return Some(status);
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(OWNED_DAEMON_EXIT_CHECK_INTERVAL);
                }
                Ok(None) | Err(_) => {
                    std::mem::forget(child);
                    return None;
                }
            }
        }
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        if let Self::Owned { child, .. } = self
            && let Some(child) = child.take()
        {
            std::mem::forget(child);
        }
    }
}

/// Resolves an explicit or interactively selected persisted session.
pub(crate) fn resolve_resume_session_id(
    requested: Option<&tau_proto::SessionId>,
) -> Result<tau_proto::SessionId, CliError> {
    if let Some(id) = requested {
        return if session_exists(id)? {
            Ok(id.clone())
        } else {
            Err(CliError::SessionNotFound(id.to_string()))
        };
    }
    pick_resume_session()?.ok_or_else(|| {
        CliError::Participant("no resumable session selected; pass `tau resume SESSION`".to_owned())
    })
}

fn session_exists(id: &tau_proto::SessionId) -> Result<bool, CliError> {
    let sessions_dir = tau_session_inspect::default_sessions_dir();
    let metas = tau_harness::list_session_metas(&sessions_dir)?;
    Ok(metas.into_iter().any(|(session_id, _)| session_id == *id))
}

/// Resolves an explicit or interactively selected running session.
pub(crate) fn resolve_attach_session_id(
    requested: Option<&tau_proto::SessionId>,
) -> Result<tau_proto::SessionId, CliError> {
    if let Some(id) = requested {
        return match runtime_dir::find_harness_for_session(id.as_str()).map_err(|error| {
            CliError::Participant(format!("cannot select attach target: {error}"))
        })? {
            Some(_) => Ok(id.clone()),
            None => Err(CliError::Participant(format!(
                "session `{id}` is not running; use `tau resume {id}` to start it"
            ))),
        };
    }

    let sessions = runtime_dir::list_running_sessions()
        .map_err(|error| CliError::Participant(format!("cannot list attach targets: {error}")))?;
    if sessions.is_empty() {
        return Err(CliError::Participant(
            "no running sessions are available to attach".to_owned(),
        ));
    }
    if !io::IsTerminal::is_terminal(&io::stdin()) {
        return Err(CliError::Participant(
            "cannot choose an attach target without an interactive terminal; pass `tau attach SESSION`"
                .to_owned(),
        ));
    }
    let items = sessions
        .iter()
        .map(|session| {
            PickerItem::enabled(format!(
                "{}  {}",
                session.session_id,
                session.project_root.display()
            ))
        })
        .collect::<Vec<_>>();
    let selection = match pick("Attach session", &items) {
        Ok(selection) => selection,
        Err(PickerError::Cancelled) => {
            return Err(CliError::Participant(
                "no running session selected; pass `tau attach SESSION`".to_owned(),
            ));
        }
        Err(error) => return Err(CliError::Participant(error.to_string())),
    };
    Ok(sessions[selection].session_id.clone())
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

fn pick_resume_session() -> Result<Option<tau_proto::SessionId>, CliError> {
    let sessions_dir = tau_session_inspect::default_sessions_dir();
    let mut metas = tau_harness::list_session_metas(&sessions_dir)?;
    metas.sort_by_key(|(_, meta)| std::cmp::Reverse(meta.last_touched));
    if metas.is_empty() {
        return Err(CliError::Participant(
            "no persisted sessions are available to resume".to_owned(),
        ));
    }
    if !io::IsTerminal::is_terminal(&io::stdin()) {
        return Err(CliError::Participant(
            "cannot choose a resume target without an interactive terminal; pass `tau resume SESSION`".to_owned(),
        ));
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
    if let Some(only) = sole_unlocked_resume_index(rows.iter().map(|(_, _, locked)| *locked))? {
        return Ok(Some(rows[only].0.clone()));
    }
    let visible = visible_resume_indices(rows.iter().map(|(_, _, locked)| *locked));
    let items = visible
        .iter()
        .map(|index| {
            let (_, item, locked) = &rows[*index];
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
    Ok(Some(rows[visible[selection]].0.clone()))
}

/// Returns the only unlocked row, requests a picker for several unlocked rows,
/// or reports that every persisted target is already owned.
fn sole_unlocked_resume_index(
    locked: impl IntoIterator<Item = bool>,
) -> Result<Option<usize>, CliError> {
    let mut unlocked = locked
        .into_iter()
        .enumerate()
        .filter_map(|(index, locked)| (!locked).then_some(index));
    let Some(first) = unlocked.next() else {
        return Err(CliError::Participant(
            "all persisted sessions are currently locked by running harnesses; use `tau attach \
             SESSION` for a running target"
                .to_owned(),
        ));
    };
    Ok(unlocked.next().is_none().then_some(first))
}

/// Caps the picker while preserving at least one selectable row when an
/// unlocked target falls beyond the newest rows.
fn visible_resume_indices(locked: impl IntoIterator<Item = bool>) -> Vec<usize> {
    let locked = locked.into_iter().collect::<Vec<_>>();
    let mut visible = (0..locked.len().min(RESUME_PICKER_LIMIT)).collect::<Vec<_>>();
    if visible.iter().all(|index| locked[*index])
        && let Some(first_unlocked) = locked.iter().position(|locked| !*locked)
        && let Some(last) = visible.last_mut()
    {
        *last = first_unlocked;
    }
    visible
}

pub(crate) struct DaemonOutput {
    pub(crate) stderr: Stdio,
    /// Durable log target deferred until resumed startup proves ownership.
    deferred_harness_log: Option<PathBuf>,
    /// Whether this spawn belongs to the conversational terminal UI.
    introduction_notice_eligible: bool,
}

impl DaemonOutput {
    /// Marks this output policy as belonging to a conversational chat launch.
    pub(crate) fn with_introduction_notice(mut self) -> Self {
        self.introduction_notice_eligible = true;
        self
    }
}

/// Maps the public session-only ephemeral flag without changing its semantics.
pub(crate) const fn storage_mode_from_ephemeral(ephemeral: bool) -> HarnessStorageMode {
    if ephemeral {
        HarnessStorageMode::SessionEphemeral
    } else {
        HarnessStorageMode::Durable
    }
}

pub(crate) fn daemon_output_for_session(
    session_id: &str,
    storage_mode: HarnessStorageMode,
    session_status: SessionLaunchStatus,
) -> Result<DaemonOutput, CliError> {
    daemon_output_for_session_in(
        &tau_session_inspect::default_sessions_dir(),
        session_id,
        storage_mode,
        session_status,
    )
}

/// Resolves daemon output for a conversational chat spawn and opts its initial
/// stdio client into the one-time welcome.
pub(crate) fn daemon_output_for_chat_session(
    session_id: &str,
    storage_mode: HarnessStorageMode,
    session_status: SessionLaunchStatus,
) -> Result<DaemonOutput, CliError> {
    daemon_output_for_chat_session_in(
        &tau_session_inspect::default_sessions_dir(),
        session_id,
        storage_mode,
        session_status,
    )
}

fn daemon_output_for_chat_session_in(
    sessions_dir: &Path,
    session_id: &str,
    storage_mode: HarnessStorageMode,
    session_status: SessionLaunchStatus,
) -> Result<DaemonOutput, CliError> {
    daemon_output_for_session_in(sessions_dir, session_id, storage_mode, session_status)
        .map(DaemonOutput::with_introduction_notice)
}

/// Resolves child stderr policy under an explicit sessions root.
fn daemon_output_for_session_in(
    sessions_dir: &Path,
    session_id: &str,
    storage_mode: HarnessStorageMode,
    session_status: SessionLaunchStatus,
) -> Result<DaemonOutput, CliError> {
    if !matches!(storage_mode, HarnessStorageMode::Durable) {
        return Ok(DaemonOutput {
            stderr: Stdio::null(),
            deferred_harness_log: None,
            introduction_notice_eligible: false,
        });
    }
    // Route the daemon's stderr (where its tracing subscriber writes) into the
    // per-session harness log so it sits next to per-extension logs under
    // `<session>/logs/`. The CLI's own tracing still goes to `ui.log`; the two
    // streams are intentionally separated so a session post-mortem doesn't need
    // to pull from two places.
    let harness_log = tau_harness::harness_log_path(sessions_dir, session_id);
    if matches!(session_status, SessionLaunchStatus::Resumed) {
        return Ok(DaemonOutput {
            stderr: Stdio::piped(),
            deferred_harness_log: Some(harness_log),
            introduction_notice_eligible: false,
        });
    }
    if let Some(parent) = harness_log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&harness_log)
        .map(Stdio::from)?;
    Ok(DaemonOutput {
        stderr,
        deferred_harness_log: None,
        introduction_notice_eligible: false,
    })
}

pub(crate) struct DaemonCliOverrides<'a> {
    /// Explicit ordered profile selection resolved before the daemon is
    /// spawned.
    pub(crate) profile: Option<&'a tau_config::settings::ProfileSelection>,
    pub(crate) role: &'a [tau_config::settings::RoleCliOverride],
    pub(crate) extension: &'a [tau_config::settings::ExtensionCliOverride],
    /// Parsed public extension environment to forward deterministically.
    pub(crate) extension_environment: Option<&'a [String]>,
    pub(crate) harness_config: &'a [tau_config::settings::HarnessConfigCliOverride],
    /// Whether the owned child must keep agent storage process-local.
    pub(crate) memory_only_agent_store: bool,
}

pub(crate) fn resolve_daemon(
    attach: bool,
    session_id: &str,
    session_status: SessionLaunchStatus,
    daemon_output: Option<DaemonOutput>,
    startup_role: Option<&str>,
    cli_overrides: DaemonCliOverrides<'_>,
    storage_mode: HarnessStorageMode,
) -> Result<DaemonHandle, CliError> {
    tracing::debug!(target: "tau_cli::startup", attach, session_id, "resolving harness daemon");
    if attach {
        tracing::debug!(target: "tau_cli::startup", session_id, "looking for existing harness daemon");
        let harness_path = runtime_dir::find_harness_for_session(session_id)
            .map_err(|error| CliError::Participant(format!("cannot select attach target: {error}")))?
            .ok_or_else(|| CliError::Participant(format!(
                "session `{session_id}` is not running; use `tau resume {session_id}` to start it"
            )))?;
        tracing::debug!(target: "tau_cli::startup", harness_path = %harness_path.display(), "attached harness daemon resolved");
        return Ok(DaemonHandle::Attached { harness_path });
    }
    start_daemon(
        session_id,
        session_status,
        daemon_output.expect("daemon output for spawned harness"),
        startup_role,
        cli_overrides,
        storage_mode,
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
    storage_mode: HarnessStorageMode,
) -> Result<DaemonHandle, CliError> {
    let tau_binary = std::env::current_exe()?;
    let DaemonOutput {
        stderr,
        deferred_harness_log,
        introduction_notice_eligible,
    } = output;
    tracing::debug!(target: "tau_cli::startup", tau_binary = %tau_binary.display(), session_id, "spawning harness daemon");

    let memory_only_agent_store = cli_overrides.memory_only_agent_store;
    let (stdin_parent, stdin_child) = UnixStream::pair()?;
    let (stdout_parent, stdout_child) = UnixStream::pair()?;
    let stdout_shutdown_stream = stdout_parent.try_clone()?;
    let mut command = build_daemon_command(DaemonCommandSpec {
        tau_binary: &tau_binary,
        session_id,
        session_status,
        stdout: Stdio::from(OwnedFd::from(stdout_child)),
        stderr,
        stdin: Stdio::from(OwnedFd::from(stdin_child)),
        startup_role,
        cli_overrides,
        storage_mode,
    });
    configure_introduction_notice(&mut command, introduction_notice_eligible);
    configure_agent_store_mode(&mut command, memory_only_agent_store);
    let spawn_result = command.spawn();

    let mut child = spawn_result?;

    tracing::debug!(target: "tau_cli::startup", pid = child.id(), "harness daemon spawned");
    let session_id = tau_proto::SessionId::parse(session_id)
        .map_err(|error| CliError::Participant(error.to_string()))?;
    let harness_path = runtime_dir::harness_path_for_session(&session_id);
    if let Some(harness_log) = deferred_harness_log {
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| CliError::Participant("missing harness stderr pipe".to_owned()))?;
        relay_stderr_after_lock_held_log(stderr, harness_log);
    }
    Ok(DaemonHandle::Owned {
        child: Some(child),
        harness_path,
        initial_ui: Some(InitialUiStdio {
            stdin: Box::new(stdin_parent),
            stdout: Box::new(stdout_parent),
            shutdown_stream: Some(stdout_shutdown_stream),
        }),
    })
}

/// Sets or explicitly clears the private preview-agent storage marker.
///
/// Clearing inherited input keeps ordinary durable and global
/// session-ephemeral launches on the persistent agent store.
fn configure_agent_store_mode(command: &mut Command, memory_only_agent_store: bool) {
    if memory_only_agent_store {
        command.env(tau_harness::MEMORY_ONLY_AGENT_STORE_ENV, "1");
    } else {
        command.env_remove(tau_harness::MEMORY_ONLY_AGENT_STORE_ENV);
    }
}

/// Sets or clears the private marker that authorizes the conversational
/// welcome.
fn configure_introduction_notice(command: &mut Command, eligible: bool) {
    if eligible {
        command.env(tau_harness::INITIAL_UI_INTRODUCTION_NOTICE_ENV, "1");
    } else {
        command.env_remove(tau_harness::INITIAL_UI_INTRODUCTION_NOTICE_ENV);
    }
}

/// Drains resumed stderr immediately, then appends only after the child creates
/// its log while retaining the session lock. Opening without `create` ensures a
/// child exit plus cleanup race cannot recreate a deleted session.
fn relay_stderr_after_lock_held_log(mut stderr: std::process::ChildStderr, harness_log: PathBuf) {
    std::thread::spawn(move || {
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(16);
        std::thread::spawn(move || {
            let mut buffer = vec![0_u8; 8 * 1024];
            loop {
                match stderr.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if sender.send(buffer[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let temporary_path = std::env::temp_dir().join(format!(
            "tau-resume-stderr-{}-{}",
            std::process::id(),
            mint_short_id("relay")
        ));
        let Ok(mut temporary) = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&temporary_path)
        else {
            return;
        };
        let mut target: Option<std::fs::File> = None;
        loop {
            if target.is_none() {
                let opened = OpenOptions::new().append(true).open(&harness_log).ok();
                if let Some(mut opened) = opened {
                    let _ = temporary.seek(SeekFrom::Start(0));
                    let _ = io::copy(&mut temporary, &mut opened);
                    target = Some(opened);
                }
            }
            match receiver.recv_timeout(Duration::from_millis(20)) {
                Ok(bytes) => {
                    if let Some(target) = target.as_mut() {
                        let _ = target.write_all(&bytes);
                    } else {
                        let _ = temporary.write_all(&bytes);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = std::fs::remove_file(temporary_path);
    });
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
    storage_mode: HarnessStorageMode,
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
        .stdin(spec.stdin)
        .stdout(spec.stdout)
        .stderr(spec.stderr);

    for key in [
        "LISTEN_FDS",
        "LISTEN_PID",
        "LISTEN_FDS_FIRST_FD",
        "LISTEN_FDNAMES",
        tau_config::settings::TAU_PROVIDER_ALIASES_ENV,
        tau_config::settings::TAU_MODEL_ALIASES_ENV,
    ] {
        cmd.env_remove(key);
    }

    if let Some(role) = spec.startup_role.filter(|role| !role.is_empty()) {
        cmd.env(tau_harness::STARTUP_ROLE_ENV, role);
    }
    if let Some(profile) = spec.cli_overrides.profile {
        cmd.env(tau_config::settings::TAU_PROFILE_ENV, profile.to_string());
    } else {
        cmd.env_remove(tau_config::settings::TAU_PROFILE_ENV);
    }
    match spec.storage_mode {
        HarnessStorageMode::Durable => {
            cmd.env_remove(tau_harness::EPHEMERAL_ENV);
            cmd.env_remove(tau_harness::MEMORY_ONLY_ENV);
        }
        HarnessStorageMode::SessionEphemeral => {
            cmd.env(tau_harness::EPHEMERAL_ENV, "1");
            cmd.env_remove(tau_harness::MEMORY_ONLY_ENV);
        }
        HarnessStorageMode::MemoryOnly => {
            cmd.env_remove(tau_harness::EPHEMERAL_ENV);
            cmd.env(tau_harness::MEMORY_ONLY_ENV, "1");
        }
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
