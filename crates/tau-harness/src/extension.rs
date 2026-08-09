//! Extension lifecycle tracking and the spawn helpers used to start both
//! supervised child-process and in-process extensions.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::{fs as path_std_fs, io as path_std_io};

use tau_client::ProtocolIoMeter;
use tau_config::settings::InvalidExtensionName;
#[cfg(not(test))]
use tau_config::settings::{TauRuntimeSocketAccess, TauStateAccess};
use tau_core::ConnectionOrigin;
use tau_proto::ClientKind;

use crate::error::{ExtensionSpawnError, HarnessError};
use crate::event::{
    HarnessEvent, SupervisedWriterHandle, WriterCommand, spawn_reader_thread_after_initialized,
    spawn_supervised_writer_thread_with_isolation_tempdir, spawn_writer_thread,
};
use crate::prompt::chrono_free_date;
use crate::settings::ExtensionConfig;

/// Lifecycle phase of a configured extension. Drives the
/// `extensions_all_ready()` gate that keeps user prompts queued until
/// every desired extension has finished its handshake.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtensionState {
    /// Process spawned (or in-process thread started); no
    /// `LifecycleHello` seen yet.
    Spawning,
    /// `LifecycleHello` received; waiting for the extension to finish
    /// announcing tools/skills and emit `LifecycleReady`.
    Handshaking,
    /// `LifecycleReady` received; the extension is fully online.
    Ready,
    /// The connection dropped after at least reaching `Spawning`.
    /// Fresh prompts continue with the remaining live providers.
    Disconnected,
}

/// One admitted extension instance and its live routing state.
pub(crate) struct ExtensionEntry {
    /// Stable configured extension identity.
    pub(crate) name: tau_proto::ExtensionName,
    /// Run-local extension instance identity.
    pub(crate) instance_id: tau_proto::ExtensionInstanceId,
    /// Allocated live routing identity.
    pub(crate) connection_id: tau_proto::ConnectionId,
    /// Authenticated protocol client kind.
    pub(crate) kind: ClientKind,
    /// Optional protocol authorities declared in the authenticated handshake.
    pub(crate) peer_capabilities: std::collections::BTreeSet<tau_proto::PeerCapability>,
    /// Immutable structural tool prefix assigned to this instance.
    pub(crate) tool_prefix: Option<tau_proto::ToolNamePrefix>,
    /// Whether startup requires this extension to initialize successfully.
    pub(crate) require: bool,
    /// Whether an unexpected supervised disconnect may be respawned.
    pub(crate) respawn_allowed: bool,
    /// PID of supervised child process, or current process for in-process.
    pub(crate) pid: Option<u32>,
    /// In-process extension thread handle (for join on shutdown).
    pub(crate) in_process_thread: Option<JoinHandle<Result<(), String>>>,
    /// Original config for supervised extensions. Present only for
    /// out-of-process children that the harness can respawn.
    pub(crate) supervised_config: Option<ExtensionConfig>,
    /// Resolved secret values authorized for this extension. Values must not be
    /// logged.
    pub(crate) secrets: std::collections::BTreeMap<String, tau_proto::SecretValue>,
    /// Number of restart attempts performed by the harness.
    pub(crate) restart_attempt: u32,
    /// Current lifecycle state. See `extensions_all_ready` for how this
    /// gates dispatch.
    pub(crate) state: ExtensionState,
    /// Protocol frame byte/count counters for this extension connection.
    pub(crate) protocol_io: ProtocolIoMeter,
}

/// Private one-shot ack that lets an extension reader start forwarding frames
/// after the harness has installed its bus and lifecycle state.
pub(crate) type ExtensionInitializedAck = Sender<()>;

/// Internal request for the harness loop to install an extension connection.
pub(crate) struct ExtensionConnectCommand {
    /// Lifecycle entry to insert once the bus connection exists.
    pub(crate) entry: ExtensionEntry,
    /// Bus metadata origin to report for the connection.
    pub(crate) origin: ConnectionOrigin,
    /// Writer channel owned by the bus connection sink.
    pub(crate) writer_tx: Sender<WriterCommand>,
    /// Ack that releases the reader after state installation completes.
    pub(crate) initialized_ack: ExtensionInitializedAck,
    /// Retained writer ownership for supervised child cleanup.
    pub(crate) supervised_writer: Option<SupervisedWriterHandle>,
    /// Previous connection id to replace when this is a supervised respawn.
    pub(crate) replaces: Option<tau_proto::ConnectionId>,
}

/// Result of spawning an in-process extension transport.
#[cfg_attr(not(any(test, feature = "echo-agent")), allow(dead_code))]
pub(crate) struct InProcessSpawn {
    /// Connection id assigned before the reader thread starts.
    pub(crate) connection_id: tau_proto::ConnectionId,
    /// Writer channel to install in the bus from the harness loop.
    pub(crate) writer_tx: Sender<WriterCommand>,
    /// In-process extension thread handle to join during shutdown.
    pub(crate) thread: JoinHandle<Result<(), String>>,
    /// Ack that releases the reader after state installation completes.
    pub(crate) initialized_ack: ExtensionInitializedAck,
    /// Protocol frame byte/count counters shared by this extension connection.
    pub(crate) protocol_io: ProtocolIoMeter,
}

/// Result of spawning a supervised extension transport.
pub(crate) struct SupervisedSpawn {
    /// Connection id assigned before the reader thread starts.
    pub(crate) connection_id: tau_proto::ConnectionId,
    /// Writer channel to install in the bus from the harness loop.
    pub(crate) writer_tx: Sender<WriterCommand>,
    /// OS process id of the supervised child.
    pub(crate) child_pid: u32,
    /// Ack that releases the reader after state installation completes.
    pub(crate) initialized_ack: ExtensionInitializedAck,
    /// Retained writer ownership for child cleanup and joining.
    pub(crate) writer: SupervisedWriterHandle,
    /// Protocol frame byte/count counters shared by this extension connection.
    pub(crate) protocol_io: ProtocolIoMeter,
}

static NEXT_EXTENSION_CONNECTION_ID: AtomicU64 = AtomicU64::new(0);

fn next_extension_connection_id() -> tau_proto::ConnectionId {
    let next = NEXT_EXTENSION_CONNECTION_ID.fetch_add(1, Ordering::Relaxed) + 1;
    tau_proto::ConnectionId::parse(format!("ext-conn-{next}"))
        .expect("generated extension connection id must satisfy the connection identifier grammar")
}

#[cfg_attr(not(any(test, feature = "echo-agent")), allow(dead_code))]
pub(crate) fn spawn_in_process<F>(
    _name: &str,
    _kind: ClientKind,
    run: F,
    tx: &Sender<HarnessEvent>,
) -> Result<InProcessSpawn, HarnessError>
where
    F: FnOnce(UnixStream, UnixStream) -> Result<(), String> + Send + 'static,
{
    // Two unidirectional pairs so dropping one end cleanly EOFs the
    // other — no shared clones keeping the socket alive.
    let (ext_read, harness_write) = UnixStream::pair()?; // harness → extension
    let (harness_read, ext_write) = UnixStream::pair()?; // extension → harness

    let connection_id = next_extension_connection_id();
    let protocol_io = ProtocolIoMeter::default();
    let writer_tx = spawn_writer_thread(harness_write, Some(protocol_io.clone()));

    let (initialized_tx, initialized_rx) = mpsc::channel();
    spawn_reader_thread_after_initialized(
        connection_id.clone(),
        harness_read,
        tx.clone(),
        initialized_rx,
    );

    let thread = thread::spawn(move || run(ext_read, ext_write));
    Ok(InProcessSpawn {
        connection_id,
        writer_tx,
        thread,
        initialized_ack: initialized_tx,
        protocol_io,
    })
}

/// Per-session log directory: `<sessions_dir>/<session_id>/logs/`.
/// Holds the harness daemon's own tracing output (`tau-harness.log`)
/// plus one file per spawned extension. Lives next to `events.jsonl`
/// so a session dir is self-contained for post-mortems.
pub fn session_logs_dir(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(session_id).join("logs")
}

/// Path of the per-session, per-extension stderr log:
/// `<sessions_dir>/<session_id>/logs/<name>.log`.
///
/// Extension names come from user-authored config, so validate the name before
/// treating it as a harness-owned path component.
pub(crate) fn extension_stderr_log_path(
    sessions_dir: &Path,
    session_id: &str,
    name: &str,
) -> Result<PathBuf, InvalidExtensionName> {
    tau_config::settings::validate_extension_name(name)?;
    Ok(session_logs_dir(sessions_dir, session_id).join(format!("{name}.log")))
}

/// Path of the per-session harness daemon log:
/// `<sessions_dir>/<session_id>/logs/tau-harness.log`. The CLI points
/// the daemon's stderr at this file when spawning it, so the daemon's
/// tracing output (which writes to stderr via `init_stderr_from_env`)
/// lands alongside the per-extension logs.
pub fn harness_log_path(sessions_dir: &Path, session_id: &str) -> PathBuf {
    session_logs_dir(sessions_dir, session_id).join("tau-harness.log")
}

fn supervised_command(
    config: &ExtensionConfig,
    kind: &ClientKind,
    stderr_log_path: Option<&Path>,
    state_dir: &Path,
    memory_only: bool,
    provider_settings: &std::collections::BTreeMap<String, Vec<u8>>,
) -> Result<(Command, Option<tempfile::TempDir>), HarnessError> {
    let (mut command, empty_mask) =
        isolated_supervised_command(config, kind, state_dir, memory_only, provider_settings)?;
    command.stdin(Stdio::piped()).stdout(Stdio::piped());
    if stderr_log_path.is_some() {
        command.stderr(Stdio::piped());
    } else {
        command.stderr(Stdio::inherit());
    }
    Ok((command, empty_mask))
}

fn isolated_supervised_command(
    config: &ExtensionConfig,
    kind: &ClientKind,
    state_dir: &Path,
    memory_only: bool,
    provider_settings: &std::collections::BTreeMap<String, Vec<u8>>,
) -> Result<(Command, Option<tempfile::TempDir>), HarnessError> {
    #[cfg(test)]
    {
        let _ = (kind, state_dir, memory_only, provider_settings);
        let mut command = Command::new(&config.command);
        command.args(&config.args);
        if let Some(cwd) = config.cwd.as_ref() {
            command.current_dir(cwd);
        }
        Ok((command, None))
    }

    #[cfg(not(test))]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let tau_state_access = if memory_only {
            TauStateAccess::Hidden
        } else {
            config.tau_state_access
        };
        let runtime_socket_root = crate::runtime_dir::prepare_harnesses_dir()?;
        let settings_root =
            prepare_provider_settings_mount(state_dir, &config.name, kind, memory_only)?;
        let cwd = config
            .cwd
            .clone()
            .unwrap_or(std::env::current_dir()?)
            .canonicalize()?;
        let state_root = if memory_only {
            state_dir
                .exists()
                .then(|| state_dir.canonicalize())
                .transpose()?
        } else {
            std::fs::create_dir_all(state_dir)?;
            create_private_state_tree(
                state_dir,
                &PathBuf::from("secrets/ext").join(config.name.as_str()),
            )?;
            create_private_state_tree(state_dir, &PathBuf::from("ext").join(config.name.as_str()))?;
            Some(state_dir.canonicalize()?)
        };
        if state_root.as_ref().is_some_and(|target| {
            tau_state_access != TauStateAccess::Legacy && cwd.starts_with(target)
                || cwd.starts_with(target.join("secrets"))
        }) {
            return Err(HarnessError::Participant(format!(
                "extension `{}` cwd must not be at or below masked Tau state",
                config.name
            )));
        }
        if config.tau_runtime_socket_access == TauRuntimeSocketAccess::Hidden
            && cwd.starts_with(&runtime_socket_root)
        {
            return Err(HarnessError::Participant(format!(
                "extension `{}` cwd must not be at or below masked Tau runtime sockets",
                config.name
            )));
        }
        let empty_mask = tempfile::Builder::new()
            .prefix("tau-extension-state-mask-")
            .tempdir()?;
        let outer_mask = empty_mask.path().join("outer");
        let runtime_socket_mask = empty_mask.path().join("runtime-sockets");
        let staging_root = empty_mask.path().join("staging");
        std::fs::create_dir(&outer_mask)?;
        std::fs::create_dir(&runtime_socket_mask)?;
        std::fs::create_dir(&staging_root)?;
        let state_root_ref = state_root.as_deref();
        let own_target = (!memory_only).then(|| {
            state_root
                .as_ref()
                .expect("persistent state root")
                .join("ext")
                .join(&config.name)
        });
        let settings_target = settings_root.as_ref().map(|path| {
            state_root.as_ref().expect("persistent state root").join(
                path.strip_prefix(state_dir)
                    .expect("provider settings below state"),
            )
        });
        for target in [&own_target, &settings_target].into_iter().flatten() {
            let relative = target
                .strip_prefix(state_root.as_ref().expect("persistent state root"))
                .expect("isolation target below state");
            std::fs::create_dir_all(outer_mask.join(relative))?;
        }
        std::fs::set_permissions(
            empty_mask.path(),
            path_std_fs::Permissions::from_mode(0o700),
        )?;
        let mut command = Command::new(&config.command);
        command.args(&config.args).current_dir("/");
        let stage_source = |target: Option<&Path>| {
            target.map(|target| {
                staging_root.join(
                    target
                        .strip_prefix(
                            state_root
                                .as_ref()
                                .expect("mount target requires state root"),
                        )
                        .expect("mount target below state"),
                )
            })
        };
        let own_source = stage_source(own_target.as_deref());
        let settings_source = settings_target
            .as_ref()
            .map(|_| empty_mask.path().join("provider-profile-snapshot"));
        if let Some(settings_source) = &settings_source {
            materialize_provider_settings_snapshot(settings_source, provider_settings)?;
        }
        let secret_mask_target = state_root.as_ref().map(|root| root.join("secrets"));
        crate::extension_launcher::configure_command(
            &mut command,
            crate::extension_launcher::IsolationPlan {
                isolation_root: empty_mask.path(),
                state_root: state_root_ref,
                tau_state_access,
                outer_mask: &outer_mask,
                runtime_socket_mask: &runtime_socket_mask,
                staging_root: &staging_root,
                secret_mask_target: secret_mask_target.as_deref(),
                own_state: own_source.as_deref().zip(own_target.as_deref()).map(
                    |(source, target)| crate::extension_launcher::MountPlan { source, target },
                ),
                provider_settings: settings_source
                    .as_deref()
                    .zip(settings_target.as_deref())
                    .map(|(source, target)| crate::extension_launcher::MountPlan {
                        source,
                        target,
                    }),
                runtime_socket_root: &runtime_socket_root,
                tau_runtime_socket_access: config.tau_runtime_socket_access,
                cwd: &cwd,
            },
        )
        .map_err(HarnessError::Participant)?;
        Ok((command, Some(empty_mask)))
    }
}

fn materialize_provider_settings_snapshot(
    root: &Path,
    settings: &std::collections::BTreeMap<String, Vec<u8>>,
) -> path_std_io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::create_dir(root)?;
    for (name, contents) in settings {
        let path = root.join(name);
        std::fs::write(&path, contents)?;
        std::fs::set_permissions(&path, path_std_fs::Permissions::from_mode(0o400))?;
    }
    std::fs::set_permissions(root, path_std_fs::Permissions::from_mode(0o500))?;
    Ok(())
}

fn prepare_provider_settings_mount(
    state_dir: &Path,
    extension_name: &str,
    kind: &ClientKind,
    memory_only: bool,
) -> Result<Option<PathBuf>, HarnessError> {
    if memory_only || kind != &ClientKind::Provider {
        return Ok(None);
    }
    let settings_root =
        tau_config::settings::extension_provider_settings_dir_of(state_dir, extension_name)
            .map_err(|error| HarnessError::Participant(error.to_string()))?;
    create_private_state_tree(state_dir, &PathBuf::from("providers").join(extension_name))?;
    Ok(Some(settings_root))
}

fn create_private_state_tree(root: &Path, relative: &Path) -> Result<(), HarnessError> {
    use std::os::unix::fs::PermissionsExt as _;

    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match std::fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == path_std_io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let metadata = std::fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(HarnessError::Participant(
                "extension private state path crosses a non-directory".to_owned(),
            ));
        }
        std::fs::set_permissions(&current, path_std_fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn spawn_supervised(
    config: &ExtensionConfig,
    kind: ClientKind,
    stderr_log_path: Option<PathBuf>,
    tx: &Sender<HarnessEvent>,
    state_dir: &Path,
    memory_only: bool,
    provider_settings: &std::collections::BTreeMap<String, Vec<u8>>,
) -> Result<SupervisedSpawn, HarnessError> {
    let (mut command, empty_mask) = supervised_command(
        config,
        &kind,
        stderr_log_path.as_deref(),
        state_dir,
        memory_only,
        provider_settings,
    )?;
    for key in std::env::vars()
        .map(|(key, _)| key)
        .filter(|key| key.starts_with("TAU_SECRET_"))
    {
        command.env_remove(key);
    }
    command.env_remove(tau_config::settings::TAU_EXTENSION_TAU_STATE_ACCESS_ENV);
    let mut child = command.spawn().map_err(|source| {
        HarnessError::ExtensionSpawn(ExtensionSpawnError::new(
            &config.name,
            &config.command,
            config.cwd.as_deref(),
            source,
        ))
    })?;
    let child_pid = child.id();
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| HarnessError::Participant("missing stdin".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::Participant("missing stdout".to_owned()))?;

    if let (Some(log_path), Some(stderr)) = (stderr_log_path, child.stderr.take()) {
        spawn_extension_stderr_logger(config.name.clone(), stderr, log_path);
    }

    let connection_id = next_extension_connection_id();
    let protocol_io = ProtocolIoMeter::default();
    let (writer_tx, writer) = spawn_supervised_writer_thread_with_isolation_tempdir(
        connection_id.clone(),
        stdin,
        child,
        Some(protocol_io.clone()),
        tx.clone(),
        empty_mask,
    );

    let (initialized_tx, initialized_rx) = mpsc::channel();
    spawn_reader_thread_after_initialized(
        connection_id.clone(),
        stdout,
        tx.clone(),
        initialized_rx,
    );

    Ok(SupervisedSpawn {
        connection_id,
        writer_tx,
        child_pid,
        initialized_ack: initialized_tx,
        writer,
        protocol_io,
    })
}

/// Read an extension's stderr line-by-line and append each line
/// verbatim to `log_path`. Extensions are expected to use
/// `tau_client::init_logging_for` (or any other `tracing`-based
/// formatter), which already emits its own timestamps and levels —
/// adding our own prefix would double up the metadata. The thread
/// exits naturally when stderr closes (i.e. the child exits), so
/// callers don't need to track the join handle.
fn spawn_extension_stderr_logger(
    name: String,
    stderr: std::process::ChildStderr,
    log_path: PathBuf,
) {
    use std::io::{BufReader, Write};
    thread::spawn(move || {
        if let Some(parent) = log_path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            eprintln!(
                "tau: failed to create extension log dir {}: {e}",
                parent.display()
            );
            return;
        }
        let mut file = match path_std_fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!(
                    "tau: failed to open extension log {}: {e}",
                    log_path.display()
                );
                return;
            }
        };

        let _ = writeln!(
            file,
            "--- {} (pid={}) attached at {} ---",
            name,
            std::process::id(),
            chrono_free_date()
        );
        let _ = file.flush();

        let mut reader = BufReader::new(stderr);
        let mut buf = [0u8; 4096];
        loop {
            match path_std_io::Read::read(&mut reader, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = file.write_all(&buf[..n]);
                    let _ = file.flush();
                }
                Err(_) => break,
            }
        }
        let _ = writeln!(
            file,
            "--- {} stderr closed at {} ---",
            name,
            chrono_free_date()
        );
        let _ = file.flush();
    });
}

#[cfg(test)]
mod tests;
