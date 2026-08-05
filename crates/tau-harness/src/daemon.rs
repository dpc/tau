//! Public entry points: blocking `run_*` daemons, the embedded
//! single-message helpers, and the small types passed to/from them.
//!
//! Listener cancellation follows
//! `SPEC-tau-harness-extension-lifecycle`.

use std::collections::BTreeSet;
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{fmt, io as path_std_io, io, thread};

use rustix::event::{PollFd, PollFlags, poll};
use rustix::io::Errno;
use tau_proto::{
    ClientKind, ConnectionId, Disconnect, Event, EventName, EventSelector, HarnessInputMessage,
    HarnessOutputMessage, HarnessOutputWriter, Hello, PROTOCOL_VERSION, Subscribe, UiCreateAgent,
};
use tau_socket::{SocketListener, SocketPeer, SocketReceive};

use crate::error::HarnessError;
use crate::event::HarnessEvent;
use crate::format::{format_extension_event, format_tool_progress};
use crate::harness::{
    Harness, HarnessSessionLaunch, HarnessStartupInputs, InitialClient,
    InitialClientStartupErrorOutput, assistant_text_from_output_items,
    tool_calls_from_output_items,
};
use crate::settings::{
    Config, resolve_config, resolve_config_in, resolve_config_with_extension_cli_overrides,
};
use crate::{daemon as path_crate_daemon, runtime_dir};

/// Cap on how long [`send_daemon_message_with_trace`] (a synchronous test
/// helper) waits for a daemon response. This is not a daemon-wide knob —
/// the long-running daemon paths block indefinitely on their event loop.
const SEND_DAEMON_MESSAGE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLaunchStatus {
    New,
    Resumed,
}

impl SessionLaunchStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Resumed => "resumed",
        }
    }
}

/// Environment flag set by the CLI when the harness should avoid durable
/// session membership, metadata, debug, and per-session stderr logs.
pub const EPHEMERAL_ENV: &str = "TAU_EPHEMERAL";
/// Selects harness-wide process-local storage for owned preview daemons.
pub const MEMORY_ONLY_ENV: &str = "TAU_MEMORY_ONLY";

/// Immutable harness-wide storage capability policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HarnessStorageMode {
    /// Preserve all ordinary harness-managed storage.
    #[default]
    Durable,
    /// Keep session-owned artifacts in memory while retaining durable agents.
    SessionEphemeral,
    /// Keep semantic stores process-local and make delegated storage
    /// unavailable.
    MemoryOnly,
}

impl HarnessStorageMode {
    /// Returns the narrow session-store persistence policy.
    #[must_use]
    pub(crate) const fn session_persistence(self) -> tau_core::SessionPersistenceMode {
        match self {
            Self::Durable => tau_core::SessionPersistenceMode::Durable,
            Self::SessionEphemeral | Self::MemoryOnly => {
                tau_core::SessionPersistenceMode::Ephemeral
            }
        }
    }

    /// Returns true when session-owned artifacts are durable.
    #[must_use]
    pub const fn is_durable(self) -> bool {
        matches!(self, Self::Durable)
    }

    /// Returns true when session-owned artifacts stay process-local.
    #[must_use]
    pub const fn is_ephemeral(self) -> bool {
        !self.is_durable()
    }

    /// Returns true when all harness-managed storage is process-local.
    #[must_use]
    pub const fn is_memory_only(self) -> bool {
        matches!(self, Self::MemoryOnly)
    }
}

impl From<SessionLaunchStatus> for tau_proto::SessionDirStatus {
    fn from(status: SessionLaunchStatus) -> Self {
        match status {
            SessionLaunchStatus::New => Self::New,
            SessionLaunchStatus::Resumed => Self::Resumed,
        }
    }
}

fn session_start_reason(status: SessionLaunchStatus) -> tau_proto::SessionStartReason {
    match status {
        SessionLaunchStatus::New => tau_proto::SessionStartReason::Initial,
        SessionLaunchStatus::Resumed => tau_proto::SessionStartReason::Resume,
    }
}

/// Serve-loop options for daemon mode.
#[derive(Clone, Debug, Eq, PartialEq, bon::Builder)]
pub struct ServeOptions {
    /// Hard cap on total served clients before the serve loop exits.
    /// Used mainly in tests to bound a run. `None` = unbounded.
    pub max_clients: Option<usize>,
    /// When set, the daemon exits as soon as the last attached UI
    /// socket disconnects. When clear, the daemon keeps running with
    /// no attached UIs — a later `tau attach SESSION` can pick up the
    /// session. The `ui_detach_request` message flips this at runtime.
    ///
    /// Default `false`: daemon is long-lived unless explicitly told
    /// otherwise.
    #[builder(default)]
    pub exit_on_disconnect: bool,
    /// Session lifecycle status announced to UI clients for the eager
    /// session.
    #[builder(default = SessionLaunchStatus::New)]
    pub session_status: SessionLaunchStatus,
    /// Directory layout (config + state) the harness reads. Defaults to
    /// [`tau_config::settings::TauDirs::default()`] on the call site.
    pub dirs: Option<tau_config::settings::TauDirs>,
    /// Ignore process-environment startup override transports.
    ///
    /// This is a narrow hermetic-test control. Normal daemon launches preserve
    /// the default `false` behavior. Only config-resolving [`run_daemon`]
    /// accepts it; pre-resolved and injected-provider entrypoints reject it.
    #[builder(default)]
    pub ignore_startup_environment: bool,
    /// Exact allowed resolved extension instance names, when constrained.
    ///
    /// Validation happens before any configured extension process is spawned.
    /// Pre-resolved entrypoints validate the set but reject the environment
    /// bypass because their caller already owns configuration resolution.
    pub allowed_extensions: Option<BTreeSet<tau_proto::ExtensionName>>,
    /// Immutable storage policy for this harness process.
    ///
    /// Session-ephemeral mode suppresses only session-owned artifacts.
    /// Memory-only mode additionally suppresses agent, diagnostic, retention,
    /// and delegated extension storage while retaining lifecycle runtime files.
    #[builder(default)]
    pub storage_mode: HarnessStorageMode,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            max_clients: None,
            exit_on_disconnect: false,
            session_status: SessionLaunchStatus::New,
            dirs: None,
            ignore_startup_environment: false,
            allowed_extensions: None,
            storage_mode: HarnessStorageMode::Durable,
        }
    }
}

fn validate_serve_options(options: &ServeOptions) -> Result<(), HarnessError> {
    if options.storage_mode.is_ephemeral()
        && matches!(options.session_status, SessionLaunchStatus::Resumed)
    {
        return Err(HarnessError::Participant(
            "ephemeral sessions cannot resume persisted session state".to_owned(),
        ));
    }
    if options.ignore_startup_environment && options.allowed_extensions.is_none() {
        return Err(HarnessError::Participant(
            "ignore_startup_environment requires an exact allowed_extensions set".to_owned(),
        ));
    }
    Ok(())
}

fn validate_pre_resolved_serve_options(
    options: &ServeOptions,
    config: &crate::settings::Config,
) -> Result<(), HarnessError> {
    validate_serve_options(options)?;
    if options.ignore_startup_environment {
        return Err(HarnessError::Participant(
            "ignore_startup_environment requires the config-resolving run_daemon entrypoint"
                .to_owned(),
        ));
    }
    validate_allowed_extensions(config, options.allowed_extensions.as_ref())
}

#[cfg(any(test, feature = "echo-agent"))]
fn validate_echo_serve_options(options: &ServeOptions) -> Result<(), HarnessError> {
    validate_serve_options(options)?;
    if options.ignore_startup_environment || options.allowed_extensions.is_some() {
        return Err(HarnessError::Participant(
            "hermetic extension controls are not supported by the injected echo daemon".to_owned(),
        ));
    }
    Ok(())
}

/// One completed user interaction with optional progress updates.
#[derive(Clone, Debug, PartialEq)]
pub struct InteractionOutcome {
    /// Human-readable extension lifecycle messages observed during the turn.
    pub lifecycle_messages: Vec<String>,
    /// Human-readable tool progress messages observed during the turn.
    pub progress_messages: Vec<String>,
    /// Provider-requested tool calls observed by an embedded interaction.
    ///
    /// Daemon trace helpers leave this empty because their narrow subscription
    /// intentionally does not expose uncorrelated runtime tool traffic.
    pub tool_calls: Vec<tau_proto::ToolCallItem>,
    /// Terminal tool results observed by an embedded interaction, with typed
    /// provider content removed. Daemon trace helpers leave this empty.
    pub tool_results: Vec<tau_proto::ToolResult>,
    /// Final assistant response text.
    pub response: String,
}

/// Options for a one-shot embedded run.
#[derive(Clone, Debug, Default, Eq, PartialEq, bon::Builder)]
pub struct EmbeddedOptions {
    /// Directory layout (config + state) the harness reads. Defaults to
    /// [`tau_config::settings::TauDirs::default()`] on the call site.
    pub dirs: Option<tau_config::settings::TauDirs>,
    /// Ignore all process-environment startup override transports.
    #[builder(default)]
    pub ignore_startup_environment: bool,
    /// Exact allowed resolved extension instance names, when constrained.
    pub allowed_extensions: Option<BTreeSet<tau_proto::ExtensionName>>,
}

/// Binds a daemon-owned listener using tau-socket safe stale-path handling.
///
/// The returned listener owns identity-checked drop-time cleanup for the socket
/// path and must outlive any cloned raw listener used by the accept forwarder.
///
/// # Errors
///
/// Returns an error when parent directory creation, stale socket handling,
/// active socket detection, binding, or metadata inspection fails.
pub(crate) fn bind_listener(path: &Path) -> Result<SocketListener, HarnessError> {
    SocketListener::bind(path).map_err(HarnessError::from)
}

/// Listener ownership variants used by the daemon accept forwarder.
enum ListenerHandle {
    /// Externally supplied listener whose path the daemon must not unlink.
    // Externally supplied by socket activation; the daemon must not unlink its path.
    SocketActivated(UnixListener),
    /// Daemon-bound listener with identity-checked path cleanup.
    // Bound by this daemon; `SocketListener` owns identity-checked path cleanup.
    Bound(SocketListener),
}

impl ListenerHandle {
    // Spawns the only accept forwarder for this listener. The forwarder owns a
    // socketpair wake endpoint, so shutdown never depends on the filesystem socket
    // path still naming this listener.
    fn spawn_forwarder(
        &self,
        tx: mpsc::Sender<HarnessEvent>,
    ) -> Result<ListenerForwarder, HarnessError> {
        let listener = match self {
            Self::SocketActivated(listener) => listener.try_clone().map_err(HarnessError::Io)?,
            Self::Bound(listener) => listener.try_clone_raw_listener()?,
        };
        ListenerForwarder::spawn(listener, tx)
    }
}

/// Owned wake channel and join handle for the daemon accept thread.
struct ListenerForwarder {
    /// Wake endpoint used to interrupt the accept poll during cleanup.
    // Owned wake endpoint used to interrupt the accept-loop poll during drop.
    wake_tx: UnixStream,
    /// Accept thread that must be joined before listener teardown.
    // Accept-loop thread joined during `ListenerForwarder` drop.
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for ListenerForwarder {
    fn drop(&mut self) {
        // The accept loop polls this owned socketpair endpoint together with the
        // listener fd. Shutting down the write side wakes the thread without using
        // the filesystem socket path, and the wake fd is never forwarded as a
        // `NewClient` stream.
        let _ = self.wake_tx.shutdown(Shutdown::Write);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// One readiness outcome from the accept forwarder's poll.
enum ListenerForwarderReady {
    /// The owned shutdown endpoint became ready.
    Wake,
    /// The daemon listener has clients ready to accept.
    Accept,
}

/// Whether the accept-forwarder loop should continue.
enum ListenerForwarderAction {
    /// Continue polling or accepting.
    Continue,
    /// Stop because shutdown or channel closure was observed.
    Stop,
}

impl ListenerForwarder {
    const MAX_ACCEPT_BATCH: usize = 16;

    fn spawn(
        listener: UnixListener,
        tx: mpsc::Sender<HarnessEvent>,
    ) -> Result<ListenerForwarder, HarnessError> {
        Self::spawn_inner(listener, tx, None)
    }

    #[cfg(test)]
    fn spawn_for_test(
        listener: UnixListener,
        tx: mpsc::Sender<HarnessEvent>,
        before_wait_tx: mpsc::Sender<()>,
    ) -> Result<ListenerForwarder, HarnessError> {
        Self::spawn_inner(listener, tx, Some(before_wait_tx))
    }

    fn spawn_inner(
        listener: UnixListener,
        tx: mpsc::Sender<HarnessEvent>,
        before_wait_tx: Option<mpsc::Sender<()>>,
    ) -> Result<ListenerForwarder, HarnessError> {
        listener.set_nonblocking(true).map_err(HarnessError::Io)?;
        let (wake_rx, wake_tx) = UnixStream::pair().map_err(HarnessError::Io)?;
        let join = thread::spawn(move || {
            loop {
                if let Some(before_wait_tx) = before_wait_tx.as_ref() {
                    let _ = before_wait_tx.send(());
                }
                match Self::poll_ready(&listener, &wake_rx) {
                    Ok(ListenerForwarderReady::Wake) => return,
                    Ok(ListenerForwarderReady::Accept) => {
                        if matches!(
                            Self::accept_ready_clients(&listener, &wake_rx, &tx),
                            ListenerForwarderAction::Stop
                        ) {
                            return;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: "tau_harness::daemon",
                            %error,
                            "listener forwarder poll failed; stopping client accepts",
                        );
                        return;
                    }
                }
            }
        });
        Ok(ListenerForwarder {
            wake_tx,
            join: Some(join),
        })
    }

    fn poll_ready(
        listener: &UnixListener,
        wake_rx: &UnixStream,
    ) -> Result<ListenerForwarderReady, Errno> {
        loop {
            let mut fds = [
                PollFd::new(wake_rx, PollFlags::IN),
                PollFd::new(listener, PollFlags::IN),
            ];
            if let Err(error) = poll(&mut fds, -1) {
                if error == Errno::INTR {
                    continue;
                }
                return Err(error);
            }

            if Self::wake_revents_requested(fds[0].revents()) {
                return Ok(ListenerForwarderReady::Wake);
            }
            if fds[1].revents().contains(PollFlags::IN) {
                return Ok(ListenerForwarderReady::Accept);
            }
            if fds[1]
                .revents()
                .intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL)
            {
                return Err(Errno::INVAL);
            }
        }
    }

    fn accept_ready_clients(
        listener: &UnixListener,
        wake_rx: &UnixStream,
        tx: &mpsc::Sender<HarnessEvent>,
    ) -> ListenerForwarderAction {
        let mut accepted = 0;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    if tx.send(HarnessEvent::NewClient(stream)).is_err() {
                        return ListenerForwarderAction::Stop;
                    }
                    accepted += 1;
                    if Self::wake_requested(wake_rx) {
                        return ListenerForwarderAction::Stop;
                    }
                    if accepted >= Self::MAX_ACCEPT_BATCH {
                        return ListenerForwarderAction::Continue;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    return ListenerForwarderAction::Continue;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    tracing::warn!(
                        target: "tau_harness::daemon",
                        %error,
                        "listener forwarder accept failed; stopping client accepts",
                    );
                    return ListenerForwarderAction::Stop;
                }
            }
        }
    }

    fn wake_requested(wake_rx: &UnixStream) -> bool {
        loop {
            let mut fds = [PollFd::new(wake_rx, PollFlags::IN)];
            match poll(&mut fds, 0) {
                Ok(_) if Self::wake_revents_requested(fds[0].revents()) => return true,
                Ok(_) => return false,
                Err(error) if error == Errno::INTR => continue,
                Err(error) => {
                    tracing::warn!(
                        target: "tau_harness::daemon",
                        %error,
                        "listener forwarder wake poll failed; stopping client accepts",
                    );
                    return true;
                }
            }
        }
    }

    fn wake_revents_requested(revents: PollFlags) -> bool {
        revents.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL)
    }
}

fn open_listener(path: &Path) -> Result<ListenerHandle, HarnessError> {
    let mut listenfd = listenfd::ListenFd::from_env();
    if let Some(listener) = listenfd.take_unix_listener(0).map_err(HarnessError::Io)? {
        tracing::info!(
            target: "tau_harness::startup",
            socket_path = %path.display(),
            "using socket-activated harness listener",
        );
        let actual_path = listener
            .local_addr()
            .map_err(HarnessError::Io)?
            .as_pathname()
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                HarnessError::Participant(
                    "socket-activated harness listener must have a pathname".to_owned(),
                )
            })?;
        if actual_path != path {
            return Err(HarnessError::Participant(format!(
                "socket-activated harness listener path `{}` does not match expected `{}`",
                actual_path.display(),
                path.display()
            )));
        }
        return Ok(ListenerHandle::SocketActivated(listener));
    }

    Ok(ListenerHandle::Bound(bind_listener(path)?))
}

/// Runs one embedded interaction and returns progress plus the final
/// agent response.
pub fn run_embedded_message_with_trace(
    state_dir: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<InteractionOutcome, HarnessError> {
    run_embedded_message_with_options(state_dir, session_id, message, EmbeddedOptions::default())
}

/// Runs one embedded interaction and returns the final agent response.
pub fn run_embedded_message(
    state_dir: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<String, HarnessError> {
    Ok(run_embedded_message_with_trace(state_dir, session_id, message)?.response)
}

/// Like [`run_embedded_message_with_trace`] but lets the caller override
/// directory layout and other options.
pub fn run_embedded_message_with_options(
    state_dir: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
    options: EmbeddedOptions,
) -> Result<InteractionOutcome, HarnessError> {
    let state_dir = state_dir.into();
    let explicit_dirs = options.dirs.is_some();
    let dirs = options
        .dirs
        .unwrap_or_else(|| tau_config::settings::TauDirs {
            config_dir: Some(state_dir.join("config")),
            state_dir: Some(state_dir.join("runtime")),
        });
    let config = if options.ignore_startup_environment {
        crate::settings::resolve_config_in_without_environment(&dirs)
    } else if explicit_dirs {
        resolve_config_in(&dirs)
    } else {
        resolve_config(None)
    }
    .map_err(|error| HarnessError::Participant(error.to_string()))?;
    validate_allowed_extensions(&config, options.allowed_extensions.as_ref())?;
    let mut harness = if options.ignore_startup_environment {
        Harness::from_config_without_startup_environment(
            &config,
            &state_dir,
            dirs,
            session_id,
            tau_proto::SessionStartReason::Initial,
            HarnessStorageMode::Durable,
        )
    } else {
        Harness::from_config(
            &config,
            &state_dir,
            dirs,
            session_id,
            tau_proto::SessionStartReason::Initial,
            HarnessStorageMode::Durable,
        )
    }?;
    let mut outcome = match harness.send_user_message(session_id, message, None) {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = harness.shutdown();
            return Err(error);
        }
    };
    harness.shutdown()?;
    outcome.lifecycle_messages = harness.lifecycle_messages.clone();
    Ok(outcome)
}

fn validate_allowed_extensions(
    config: &crate::settings::Config,
    allowed: Option<&BTreeSet<tau_proto::ExtensionName>>,
) -> Result<(), HarnessError> {
    let Some(allowed) = allowed else {
        return Ok(());
    };
    let actual = config
        .extensions
        .keys()
        .cloned()
        .map(tau_proto::ExtensionName::parse)
        .collect::<Result<BTreeSet<_>, _>>()
        .expect("validated config extension names must remain canonical")
        .into_iter()
        .collect::<BTreeSet<_>>();
    if &actual != allowed {
        return Err(HarnessError::Participant(format!(
            "resolved extensions differ from deterministic allowlist: expected {allowed:?}, got {actual:?}"
        )));
    }
    Ok(())
}

/// Like [`run_embedded_message_with_trace`] but uses the echo provider and
/// the in-process shell tool for testing.
#[cfg(any(test, feature = "echo-agent"))]
pub fn run_embedded_message_with_echo(
    state_dir: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<InteractionOutcome, HarnessError> {
    fn echo_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        crate::harness::run_echo_provider(r, w).map_err(|e| e.to_string())
    }
    let state_dir = state_dir.into();
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(state_dir.join("config")),
        state_dir: Some(state_dir.join("runtime")),
    };
    let mut harness = Harness::new_with_provider(
        state_dir,
        dirs,
        echo_runner,
        echo_tools(),
        session_id,
        tau_proto::SessionStartReason::Initial,
        HarnessStorageMode::Durable,
    )?;
    disable_echo_tool_context_gate_for_tests(&mut harness);
    harness.enable_echo_tool_for_tests();
    let mut outcome = match harness.send_user_message(session_id, message, None) {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = harness.shutdown();
            return Err(error);
        }
    };
    harness.shutdown()?;
    outcome.lifecycle_messages = harness.lifecycle_messages.clone();
    Ok(outcome)
}

/// Runs one embedded interaction with a feature-gated deterministic provider
/// runner and the no-side-effect echo tool.
///
/// This narrow seam is available only to cross-crate acceptance tests; normal
/// harness builds cannot inject provider runners.
#[cfg(feature = "provider-test-support")]
pub fn run_embedded_message_with_test_provider(
    state_dir: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
    provider_runner: fn(UnixStream, UnixStream) -> Result<(), String>,
) -> Result<InteractionOutcome, HarnessError> {
    let state_dir = state_dir.into();
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(state_dir.join("config")),
        state_dir: Some(state_dir.join("runtime")),
    };
    let mut harness = Harness::new_with_provider(
        state_dir,
        dirs,
        provider_runner,
        echo_tools(),
        session_id,
        tau_proto::SessionStartReason::Initial,
        HarnessStorageMode::Durable,
    )?;
    disable_echo_tool_context_gate_for_tests(&mut harness);
    harness.enable_echo_tool_for_tests();
    let mut outcome = match harness.send_user_message(session_id, message, None) {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = harness.shutdown();
            return Err(error);
        }
    };
    harness.shutdown()?;
    outcome.lifecycle_messages = harness.lifecycle_messages.clone();
    Ok(outcome)
}

/// In-process tool list used by the echo-provider test helpers. Lives
/// here so the only call site that depends on `tau-ext-shell` is
/// gated behind the `echo-agent` feature.
#[cfg(any(test, feature = "echo-agent"))]
fn echo_tools() -> Vec<crate::harness::InProcessTool> {
    fn shell_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        tau_ext_shell::run(r, w).map_err(|e| e.to_string())
    }
    vec![crate::harness::InProcessTool {
        name: "shell",
        runner: shell_runner,
    }]
}

#[cfg(any(test, feature = "echo-agent"))]
fn disable_echo_tool_context_gate_for_tests(harness: &mut Harness) {
    // Echo-mode harnesses use the shell extension only to satisfy deterministic
    // tool calls. Keep those helpers focused on provider/tool behavior instead
    // of deferring prompts for shell's cwd context acknowledgement.
    harness.agent_context_providers.clear();
    harness.pending_agent_discovery.clear();
    harness.frozen_agent_discovery.clear();
    harness.agent_context_initialized.clear();
}

/// Runs a foreground daemon that accepts socket clients.
///
/// `eager_session_id` is the session the harness pre-warms (AGENTS.md +
/// skill discovery) and where `events.jsonl` lands. Subsequent prompts for
/// other session ids lazy-init.
pub fn run_daemon(
    socket_path: impl Into<PathBuf>,
    state_dir: impl Into<PathBuf>,
    eager_session_id: &str,
    options: ServeOptions,
) -> Result<(), HarnessError> {
    run_daemon_with_internal_tools(
        socket_path,
        state_dir,
        eager_session_id,
        options,
        Vec::new(),
    )
}

/// Runs a foreground socket daemon with harness-owned tool handlers installed
/// before agent rehydration and configured-extension readiness.
pub fn run_daemon_with_internal_tools(
    socket_path: impl Into<PathBuf>,
    state_dir: impl Into<PathBuf>,
    eager_session_id: &str,
    options: ServeOptions,
    internal_tool_handlers: crate::InternalToolHandlers,
) -> Result<(), HarnessError> {
    validate_serve_options(&options)?;
    let socket_path = socket_path.into();
    let state_dir = state_dir.into();
    let listener_handle = open_listener(&socket_path)?;
    let (config, dirs) = match options.dirs.clone() {
        Some(dirs) => {
            let config = if options.ignore_startup_environment {
                crate::settings::resolve_config_in_without_environment(&dirs)
            } else {
                resolve_config_in(&dirs)
            }
            .map_err(|error| HarnessError::Participant(error.to_string()))?;
            (config, dirs)
        }
        None => {
            let dirs = tau_config::settings::TauDirs {
                config_dir: Some(state_dir.join("config")),
                state_dir: Some(state_dir.join("runtime")),
            };
            let config = if options.ignore_startup_environment {
                crate::settings::resolve_config_in_without_environment(&dirs)
            } else {
                resolve_config(None)
            }
            .map_err(|error| HarnessError::Participant(error.to_string()))?;
            (config, dirs)
        }
    };
    validate_allowed_extensions(&config, options.allowed_extensions.as_ref())?;
    let mut initial_client_error_stream = None;
    let project_root = std::env::current_dir()?.canonicalize()?;
    let (mut harness, initial_client_id) = Harness::from_config_with_initial_client(
        &config,
        state_dir,
        dirs,
        eager_session_id,
        HarnessSessionLaunch {
            reason: session_start_reason(options.session_status),
            storage_mode: options.storage_mode,
        },
        HarnessStartupInputs {
            initial_client: None,
            internal_tool_handlers,
            ignore_startup_environment: options.ignore_startup_environment,
            project_root,
        },
        &mut initial_client_error_stream,
    )?;
    debug_assert!(initial_client_id.is_none());

    let tx = harness.tx.clone();
    let forwarder = listener_handle.spawn_forwarder(tx)?;

    let result = harness.run_event_loop(options.max_clients, options.exit_on_disconnect);
    let _ = harness.shutdown();
    drop(forwarder);
    drop(listener_handle);
    result
}

/// Like [`run_daemon`] but uses the echo provider for testing. Also enables
/// the shell extension's `echo` tool so echo-provider-driven tool calls
/// resolve.
#[cfg(any(test, feature = "echo-agent"))]
pub fn run_daemon_with_echo(
    socket_path: impl Into<PathBuf>,
    state_dir: impl Into<PathBuf>,
    eager_session_id: &str,
    options: ServeOptions,
) -> Result<(), HarnessError> {
    validate_echo_serve_options(&options)?;
    fn echo_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        crate::harness::run_echo_provider(r, w).map_err(|e| e.to_string())
    }
    let socket_path = socket_path.into();
    let state_dir = state_dir.into();
    let listener_handle = open_listener(&socket_path)?;
    let dirs = options
        .dirs
        .clone()
        .unwrap_or_else(|| tau_config::settings::TauDirs {
            config_dir: Some(state_dir.join("config")),
            state_dir: Some(state_dir.join("runtime")),
        });
    let mut harness = Harness::new_with_provider(
        state_dir,
        dirs,
        echo_runner,
        echo_tools(),
        eager_session_id,
        session_start_reason(options.session_status),
        options.storage_mode,
    )?;
    disable_echo_tool_context_gate_for_tests(&mut harness);
    harness.enable_echo_tool_for_tests();

    let tx = harness.tx.clone();
    let forwarder = listener_handle.spawn_forwarder(tx)?;

    let result = harness.run_event_loop(options.max_clients, options.exit_on_disconnect);
    let _ = harness.shutdown();
    drop(forwarder);
    drop(listener_handle);
    result
}

/// Runs a foreground daemon using extensions from configuration.
pub fn run_daemon_with_config(
    config: &Config,
    socket_path: impl Into<PathBuf>,
    state_dir: impl Into<PathBuf>,
    eager_session_id: &str,
    options: ServeOptions,
) -> Result<(), HarnessError> {
    validate_pre_resolved_serve_options(&options, config)?;
    let socket_path = socket_path.into();
    let state_dir = state_dir.into();
    let listener_handle = open_listener(&socket_path)?;
    let dirs = options.dirs.clone().unwrap_or_default();
    let mut harness = Harness::from_config(
        config,
        state_dir,
        dirs,
        eager_session_id,
        session_start_reason(options.session_status),
        options.storage_mode,
    )?;

    let tx = harness.tx.clone();
    let forwarder = listener_handle.spawn_forwarder(tx)?;

    let result = harness.run_event_loop(options.max_clients, options.exit_on_disconnect);
    let _ = harness.shutdown();
    drop(forwarder);
    drop(listener_handle);
    result
}

/// Sends one user message to a running daemon and returns progress
/// plus the final response.
///
/// Stamps the outgoing `UiCreateAgent` with a unique `ctx_id` and
/// uses the matching `AgentPromptCreated` to capture the
/// `agent_prompt_id` the harness allocated for this submission.
/// Without this, opening a fresh socket against a daemon that has
/// served a previous prompt would replay that prompt's terminal
/// `ProviderResponseFinished` to the new subscriber and the helper
/// would return the historical response instead of waiting for the
/// live one.
pub fn send_daemon_message_with_trace(
    socket_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<InteractionOutcome, HarnessError> {
    let ctx_id = next_ctx_id();
    let mut peer = connect_daemon_message_peer(socket_path)?;
    send_daemon_message_prompt(&mut peer, session_id, message, &ctx_id)?;
    wait_for_daemon_trace_outcome(peer, ctx_id)
}

fn connect_daemon_message_peer(
    socket_path: impl Into<PathBuf>,
) -> Result<SocketPeer, HarnessError> {
    let mut peer = connect_daemon_helper(socket_path, "tau-cli")?;
    let selectors = daemon_message_event_selectors();
    peer.send(&HarnessInputMessage::Subscribe(Subscribe {
        historical_selectors: selectors.clone(),
        live_selectors: selectors,
    }))?;
    Ok(peer)
}

fn daemon_message_event_selectors() -> Vec<EventSelector> {
    use EventName as E;

    vec![
        EventSelector::Exact(E::AGENT_PROMPT_CREATED),
        EventSelector::Exact(E::UI_CREATE_AGENT_RESULT),
        EventSelector::Exact(E::AGENT_PROMPT_FAILED),
        EventSelector::Exact(E::PROVIDER_RESPONSE_FINISHED),
        EventSelector::Exact(E::TOOL_PROGRESS),
        EventSelector::Exact(E::SHELL_COMMAND_PROGRESS),
        EventSelector::Exact(E::HARNESS_NOTICE),
        EventSelector::Exact(E::EXTENSION_STARTING),
        EventSelector::Exact(E::EXTENSION_READY),
        EventSelector::Exact(E::EXTENSION_EXITED),
        EventSelector::Exact(E::EXTENSION_RESTARTING),
    ]
}

fn send_daemon_message_prompt(
    peer: &mut SocketPeer,
    session_id: &str,
    message: &str,
    ctx_id: &str,
) -> Result<(), HarnessError> {
    let session_id = tau_proto::SessionId::parse(session_id).map_err(|error| {
        HarnessError::Participant(format!("invalid daemon-message session id: {error}"))
    })?;
    peer.send(&HarnessInputMessage::emit(Event::UiCreateAgent(
        daemon_message_create_agent(session_id, message, ctx_id),
    )))?;
    Ok(())
}

fn daemon_message_create_agent(
    session_id: tau_proto::SessionId,
    message: &str,
    ctx_id: &str,
) -> UiCreateAgent {
    UiCreateAgent {
        request_id: format!("daemon-create-{ctx_id}"),
        literal: false,
        parent_agent: None,
        session_id,
        role: "engineer".to_owned(),
        model_override: None,
        metadata: Vec::new(),
        initial_prompt: Some(message.to_owned()),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: Some(ctx_id.to_owned()),
        ephemeral: false,
    }
}

fn wait_for_daemon_trace_outcome(
    mut peer: SocketPeer,
    ctx_id: String,
) -> Result<InteractionOutcome, HarnessError> {
    let started_at = Instant::now();
    let mut lifecycle_messages = Vec::new();
    let mut progress_messages = Vec::new();
    // Counter parsed out of the `AgentPromptCreated` whose `ctx_id`
    // matches our submit. The terminal `ProviderResponseFinished` has a
    // spid counter where `our_spid_counter <= terminal_counter` (equal when no tool
    // calls, higher when tool-result follow-ups bump the counter).
    let mut our_spid_counter: Option<u64> = None;
    let mut created_agent_id = None;

    loop {
        if SEND_DAEMON_MESSAGE_TIMEOUT <= started_at.elapsed() {
            return Err(HarnessError::ResponseTimeout);
        }
        if let Some(message) = recv_daemon_message(
            &mut peer,
            SEND_DAEMON_MESSAGE_TIMEOUT.saturating_sub(started_at.elapsed()),
        )? {
            let state = DaemonTraceState {
                ctx_id: &ctx_id,
                lifecycle_messages: &mut lifecycle_messages,
                progress_messages: &mut progress_messages,
                our_spid_counter: &mut our_spid_counter,
                created_agent_id: &mut created_agent_id,
            };
            if let Some(outcome) = handle_daemon_trace_message(&mut peer, message, state)? {
                return Ok(outcome);
            }
        }
    }
}

/// Mutable correlation and output projection for one daemon prompt trace.
struct DaemonTraceState<'a> {
    /// Correlation id attached to the submitted prompt.
    ctx_id: &'a str,
    /// Lifecycle/status messages collected for the caller.
    lifecycle_messages: &'a mut Vec<String>,
    /// Tool and shell progress messages collected for the caller.
    progress_messages: &'a mut Vec<String>,
    /// Parsed prompt counter for the submitted prompt.
    our_spid_counter: &'a mut Option<u64>,
    /// Created agent returned by admission.
    created_agent_id: &'a mut Option<tau_proto::AgentId>,
}

impl DaemonTraceState<'_> {
    /// Bind exactly once to the created agent's matching initial prompt.
    fn bind_prompt(&mut self, prompt: &tau_proto::AgentPromptCreated) {
        if self.our_spid_counter.is_none()
            && self.created_agent_id.as_ref() == Some(&prompt.agent_id)
            && prompt.ctx_id.as_deref() == Some(self.ctx_id)
        {
            *self.our_spid_counter = parse_agent_prompt_index(prompt.agent_prompt_id.as_ref());
        }
    }

    /// Return whether a provider terminal belongs to the bound prompt chain.
    fn owns_finished(&self, finished: &tau_proto::ProviderResponseFinished) -> bool {
        tool_calls_from_output_items(&finished.output_items).is_empty()
            && self.created_agent_id.as_ref() == Some(&finished.agent_id)
            && self.our_spid_counter.is_some_and(|ours| {
                parse_agent_prompt_index(finished.agent_prompt_id.as_ref())
                    .is_some_and(|counter| ours <= counter)
            })
    }
}

fn handle_daemon_trace_message(
    peer: &mut SocketPeer,
    message: HarnessOutputMessage,
    state: DaemonTraceState<'_>,
) -> Result<Option<InteractionOutcome>, HarnessError> {
    match message {
        HarnessOutputMessage::Deliver(delivery) => {
            handle_daemon_trace_event(peer, delivery.into_event(), state)
        }
        HarnessOutputMessage::Disconnect(d) => Err(HarnessError::Participant(
            d.reason.unwrap_or_else(|| "daemon disconnected".to_owned()),
        )),
        _ => Ok(None),
    }
}

fn handle_daemon_trace_event(
    peer: &mut SocketPeer,
    event: Event,
    mut state: DaemonTraceState<'_>,
) -> Result<Option<InteractionOutcome>, HarnessError> {
    match event {
        Event::UiCreateAgentResult(result)
            if result.request_id == format!("daemon-create-{}", state.ctx_id) =>
        {
            match result.outcome {
                tau_proto::UiCreateAgentOutcome::Created { agent_id, .. } => {
                    *state.created_agent_id = Some(agent_id);
                }
                tau_proto::UiCreateAgentOutcome::Rejected {
                    reason, message, ..
                } => {
                    return Err(HarnessError::Participant(format!(
                        "create-agent request failed ({reason}): {message}"
                    )));
                }
            }
        }
        Event::AgentPromptFailed(failed)
            if failed.request_id == format!("daemon-create-{}", state.ctx_id)
                && state.created_agent_id.as_ref() == Some(&failed.agent_id)
                && failed.ctx_id == state.ctx_id =>
        {
            return Err(HarnessError::Participant(format!(
                "initial prompt failed ({}): {}",
                failed.stage, failed.message
            )));
        }
        Event::ToolProgress(p) => state.progress_messages.push(format_tool_progress(&p)),
        Event::ShellCommandProgress(_) => state
            .progress_messages
            .push("shell: running shell command".to_owned()),
        Event::HarnessNotice(info) => state.lifecycle_messages.push(info.message),
        event @ (Event::ExtensionStarting(_)
        | Event::ExtensionReady(_)
        | Event::ExtensionExited(_)
        | Event::ExtensionRestarting(_)) => {
            state
                .lifecycle_messages
                .push(format_extension_event(&event));
        }
        Event::AgentPromptCreated(prompt) => state.bind_prompt(&prompt),
        Event::ProviderResponseFinished(finished) if state.owns_finished(&finished) => {
            if matches!(
                finished.stop_reason,
                tau_proto::ProviderStopReason::Error
                    | tau_proto::ProviderStopReason::RepetitionDetected
            ) || finished.failure_kind.is_some()
                || finished.error.is_some()
            {
                return Err(HarnessError::Participant(finished.error.unwrap_or_else(
                    || {
                        finished.failure_kind.map_or_else(
                            || format!("provider stopped with {:?}", finished.stop_reason),
                            |kind| format!("provider failure: {}", kind.as_str()),
                        )
                    },
                )));
            }
            peer.send(&HarnessInputMessage::Disconnect(Disconnect {
                reason: Some("done".to_owned()),
            }))?;
            return Ok(Some(InteractionOutcome {
                lifecycle_messages: std::mem::take(state.lifecycle_messages),
                progress_messages: std::mem::take(state.progress_messages),
                tool_calls: Vec::new(),
                tool_results: Vec::new(),
                response: assistant_text_from_output_items(&finished.output_items)
                    .unwrap_or_default(),
            }));
        }
        _ => {}
    }

    Ok(None)
}

fn parse_agent_prompt_index(agent_prompt_id: &str) -> Option<u64> {
    agent_prompt_id
        .strip_prefix("ap-")?
        .rsplit_once('-')?
        .1
        .parse()
        .ok()
}

/// Generates a unique correlation id for one daemon-helper submission.
/// The pid + atomic counter combination is unique within the test
/// process; the bytes never need to be sortable or persisted.
fn next_ctx_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "tau-daemon-helper-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Sends one user message to a running daemon and returns the final
/// response.
pub fn send_daemon_message(
    socket_path: impl Into<PathBuf>,
    session_id: &str,
    message: &str,
) -> Result<String, HarnessError> {
    Ok(send_daemon_message_with_trace(socket_path, session_id, message)?.response)
}

/// Requests the rendered system prompt for `role` from a running harness
/// daemon.
pub fn get_daemon_rendered_system_prompt(
    socket_path: impl Into<PathBuf>,
    role: &str,
) -> Result<String, HarnessError> {
    let request_id = next_render_request_id("tau-rendered-system-prompt");
    let mut peer = connect_daemon_helper(socket_path, "tau-print-prompt")?;
    peer.send(&HarnessInputMessage::GetRenderedSystemPrompt(
        tau_proto::GetRenderedSystemPrompt {
            request_id: request_id.clone(),
            role: role.to_owned(),
        },
    ))?;

    let started_at = Instant::now();
    loop {
        if SEND_DAEMON_MESSAGE_TIMEOUT <= started_at.elapsed() {
            let _ = peer.send(&HarnessInputMessage::Disconnect(Disconnect {
                reason: Some("done".to_owned()),
            }));
            return Err(HarnessError::ResponseTimeout);
        }
        if let Some(message) = recv_daemon_message(
            &mut peer,
            SEND_DAEMON_MESSAGE_TIMEOUT.saturating_sub(started_at.elapsed()),
        )? {
            match message {
                HarnessOutputMessage::RenderedSystemPromptResult(result)
                    if result.request_id == request_id =>
                {
                    let _ = peer.send(&HarnessInputMessage::Disconnect(Disconnect {
                        reason: Some("done".to_owned()),
                    }));
                    if let Some(error) = result.error {
                        return Err(HarnessError::Participant(error));
                    }
                    return result.prompt.ok_or_else(|| {
                        HarnessError::Participant(
                            "daemon returned no rendered system prompt".to_owned(),
                        )
                    });
                }
                HarnessOutputMessage::Disconnect(d) => {
                    return Err(HarnessError::Participant(
                        d.reason.unwrap_or_else(|| "daemon disconnected".to_owned()),
                    ));
                }
                _ => {}
            }
        }
    }
}

/// Requests the effective provider-facing tool definitions for `role` from a
/// running harness daemon.
pub fn get_daemon_rendered_tool_definitions(
    socket_path: impl Into<PathBuf>,
    role: &str,
) -> Result<Vec<tau_proto::ToolDefinition>, HarnessError> {
    let request_id = next_render_request_id("tau-rendered-tools");
    let mut peer = connect_daemon_helper(socket_path, "tau-print-tools")?;
    peer.send(&HarnessInputMessage::GetRenderedToolDefinitions(
        tau_proto::GetRenderedToolDefinitions {
            request_id: request_id.clone(),
            role: Some(role.to_owned()),
        },
    ))?;

    let started_at = Instant::now();
    loop {
        if SEND_DAEMON_MESSAGE_TIMEOUT <= started_at.elapsed() {
            let _ = peer.send(&HarnessInputMessage::Disconnect(Disconnect {
                reason: Some("done".to_owned()),
            }));
            return Err(HarnessError::ResponseTimeout);
        }
        if let Some(message) = recv_daemon_message(
            &mut peer,
            SEND_DAEMON_MESSAGE_TIMEOUT.saturating_sub(started_at.elapsed()),
        )? {
            match message {
                HarnessOutputMessage::RenderedToolDefinitionsResult(result)
                    if result.request_id == request_id =>
                {
                    let _ = peer.send(&HarnessInputMessage::Disconnect(Disconnect {
                        reason: Some("done".to_owned()),
                    }));
                    if let Some(error) = result.error {
                        return Err(HarnessError::Participant(error));
                    }
                    return result.tools.ok_or_else(|| {
                        HarnessError::Participant(
                            "daemon returned no rendered tool definitions".to_owned(),
                        )
                    });
                }
                HarnessOutputMessage::Disconnect(d) => {
                    return Err(HarnessError::Participant(
                        d.reason.unwrap_or_else(|| "daemon disconnected".to_owned()),
                    ));
                }
                _ => {}
            }
        }
    }
}

fn connect_daemon_helper(
    socket_path: impl Into<PathBuf>,
    client_name: &str,
) -> Result<SocketPeer, HarnessError> {
    let mut peer = SocketPeer::connect(socket_path)?;
    peer.send(&HarnessInputMessage::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: tau_proto::ExtensionName::parse(client_name)
            .expect("validated daemon client name must remain canonical"),
        client_kind: ClientKind::Ui,
        expected_session_id: None,
        capabilities: Default::default(),
    }))?;
    Ok(peer)
}

fn recv_daemon_message(
    peer: &mut SocketPeer,
    timeout: Duration,
) -> Result<Option<HarnessOutputMessage>, HarnessError> {
    match peer.recv_timeout(timeout)? {
        SocketReceive::Message { message } => Ok(Some(message)),
        SocketReceive::Timeout => Ok(None),
        SocketReceive::Closed => Err(HarnessError::Participant(
            "daemon socket closed before response".to_owned(),
        )),
    }
}

fn next_render_request_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        prefix,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Runs the harness daemon with runtime directory management.
pub fn run_harness_daemon(
    project_root: &Path,
    config: &Config,
    eager_session_id: &str,
    options: ServeOptions,
) -> Result<(), HarnessError> {
    run_harness_daemon_with_internal_tools(
        project_root,
        config,
        eager_session_id,
        options,
        Vec::new(),
    )
}

/// Runs the harness daemon with injected internal tool handlers.
pub fn run_harness_daemon_with_internal_tools(
    project_root: &Path,
    config: &Config,
    eager_session_id: &str,
    options: ServeOptions,
    internal_tool_handlers: crate::InternalToolHandlers,
) -> Result<(), HarnessError> {
    let runtime_instance_id = runtime_dir::HarnessInstanceId::mint();
    run_harness_daemon_with_internal_tools_and_initial_client(
        project_root,
        config,
        eager_session_id,
        options,
        internal_tool_handlers,
        RuntimeHarnessLaunch {
            runtime_instance_id,
            initial_client: None,
            initial_client_error_stream: None,
        },
    )
}

/// Runtime-path identity and optional initial-client transport for one daemon.
struct RuntimeHarnessLaunch {
    /// Random discriminator shared with a spawning CLI when present.
    runtime_instance_id: runtime_dir::HarnessInstanceId,
    /// Initial UI accepted directly over inherited stdio when present.
    initial_client: Option<InitialClient>,
    /// Best-effort startup error transport for the initial UI.
    initial_client_error_stream: Option<InitialClientStartupErrorOutput>,
}

fn run_harness_daemon_with_internal_tools_and_initial_client(
    project_root: &Path,
    config: &Config,
    eager_session_id: &str,
    options: ServeOptions,
    internal_tool_handlers: crate::InternalToolHandlers,
    launch: RuntimeHarnessLaunch,
) -> Result<(), HarnessError> {
    let RuntimeHarnessLaunch {
        runtime_instance_id,
        initial_client,
        mut initial_client_error_stream,
    } = launch;
    let project_root = canonical_project_root(project_root)?;
    validate_pre_resolved_serve_options(&options, config)?;
    let startup_started_at = Instant::now();
    tracing::debug!(target: "tau_harness::startup", project_root = %project_root.display(), eager_session_id, "starting harness daemon");
    let mut harness_paths = notify_startup_error(
        runtime_dir::prepare_harness_paths_for_instance(
            &project_root,
            eager_session_id,
            &runtime_instance_id,
        ),
        &mut initial_client_error_stream,
    )?;
    tracing::debug!(target: "tau_harness::startup", harness_path = %harness_paths.path().display(), elapsed_ms = startup_started_at.elapsed().as_millis(), "prepared harness paths");
    let socket_path = harness_paths.socket_path();
    let listener = notify_startup_error(
        bind_listener(&socket_path),
        &mut initial_client_error_stream,
    )?;

    let state_dir = tau_session_inspect::default_state_dir();
    let dirs = options.dirs.clone().unwrap_or_default();
    tracing::debug!(target: "tau_harness::startup", state_dir = %state_dir.display(), elapsed_ms = startup_started_at.elapsed().as_millis(), "constructing harness");
    let (mut harness, initial_client_id) = notify_startup_error(
        Harness::from_config_with_initial_client(
            config,
            &state_dir,
            dirs,
            eager_session_id,
            HarnessSessionLaunch {
                reason: session_start_reason(options.session_status),
                storage_mode: options.storage_mode,
            },
            HarnessStartupInputs {
                initial_client,
                internal_tool_handlers,
                ignore_startup_environment: false,
                project_root,
            },
            &mut initial_client_error_stream,
        ),
        &mut initial_client_error_stream,
    )?;
    harness.set_runtime_harness_path(harness_paths.path().to_path_buf());
    harness_paths.set_peer_entrypoint(harness.has_peer_entrypoint());
    tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "harness constructed");

    tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "writing daemon ready markers");
    notify_startup_error_after_accept(
        harness_paths.write_metadata(),
        &mut initial_client_error_stream,
        &mut harness,
        initial_client_id.as_ref(),
    )?;
    tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "daemon ready markers written");

    let tx = harness.tx.clone();
    let listener_handle = ListenerHandle::Bound(listener);
    let forwarder = notify_startup_error_after_accept(
        listener_handle.spawn_forwarder(tx),
        &mut initial_client_error_stream,
        &mut harness,
        initial_client_id.as_ref(),
    )?;
    let result = harness.run_event_loop(options.max_clients, options.exit_on_disconnect);
    let _ = harness.shutdown();
    drop(forwarder);
    drop(listener_handle);
    harness_paths.cleanup();
    result
}

/// Resolves and validates the immutable directory identity advertised by a
/// runtime harness.
fn canonical_project_root(project_root: &Path) -> Result<PathBuf, HarnessError> {
    let canonical = project_root.canonicalize()?;
    if !canonical.is_dir() {
        return Err(HarnessError::Participant(format!(
            "harness project root is not a directory: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

/// Entrypoint for `tau component harness`.
pub fn run_component() -> Result<(), Box<dyn std::error::Error>> {
    run_component_with_internal_tools(Vec::new())
}

/// Entrypoint for `tau component harness` with injected internal tool handlers.
pub fn run_component_with_internal_tools(
    internal_tool_handlers: crate::InternalToolHandlers,
) -> Result<(), Box<dyn std::error::Error>> {
    run_component_with_internal_tools_and_initial_client(
        internal_tool_handlers,
        ComponentLaunch::Direct(Vec::new()),
    )
}

/// Runs a direct harness component with typed ordered extension CLI overrides.
pub fn run_component_with_internal_tools_and_extension_cli_overrides(
    internal_tool_handlers: crate::InternalToolHandlers,
    extension_cli_overrides: Vec<tau_config::settings::ExtensionCliOverride>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_component_with_internal_tools_and_initial_client(
        internal_tool_handlers,
        ComponentLaunch::Direct(extension_cli_overrides),
    )
}

/// Entrypoint for `tau component harness` with injected internal tool handlers
/// and an initial UI connection carried over stdio.
pub fn run_component_with_internal_tools_and_initial_ui_stdio(
    internal_tool_handlers: crate::InternalToolHandlers,
) -> Result<(), Box<dyn std::error::Error>> {
    run_component_with_internal_tools_and_initial_client(
        internal_tool_handlers,
        ComponentLaunch::SpawnedInitialUiStdio,
    )
}

enum ComponentLaunch {
    Direct(Vec<tau_config::settings::ExtensionCliOverride>),
    SpawnedInitialUiStdio,
}

impl ComponentLaunch {
    fn uses_spawned_transport(&self) -> bool {
        matches!(self, Self::SpawnedInitialUiStdio)
    }

    fn extension_overrides(
        &self,
        transport: Option<std::ffi::OsString>,
    ) -> Result<Vec<tau_config::settings::ExtensionCliOverride>, Box<dyn std::error::Error>> {
        match self {
            Self::Direct(overrides) => Ok(overrides.clone()),
            Self::SpawnedInitialUiStdio => {
                crate::settings::parse_extension_cli_overrides_transport(transport)
            }
        }
    }

    /// Resolves the runtime identity used by this component launch.
    fn runtime_instance_id(
        &self,
        transport: Option<std::ffi::OsString>,
    ) -> Result<runtime_dir::HarnessInstanceId, std::io::Error> {
        match self {
            Self::Direct(_) => Ok(runtime_dir::HarnessInstanceId::mint()),
            Self::SpawnedInitialUiStdio => match transport {
                Some(value) => {
                    runtime_dir::HarnessInstanceId::parse(value.into_string().map_err(|_| {
                        path_std_io::Error::new(
                            path_std_io::ErrorKind::InvalidInput,
                            "harness runtime instance id is not UTF-8",
                        )
                    })?)
                }
                None => Ok(runtime_dir::HarnessInstanceId::mint()),
            },
        }
    }
}

fn run_component_with_internal_tools_and_initial_client(
    internal_tool_handlers: crate::InternalToolHandlers,
    launch: ComponentLaunch,
) -> Result<(), Box<dyn std::error::Error>> {
    let initial_client = launch
        .uses_spawned_transport()
        .then_some(InitialClient::Stdio);
    let mut initial_client_error_output = initial_client
        .as_ref()
        .map(|InitialClient::Stdio| InitialClientStartupErrorOutput::Stdout);
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let startup_started_at = Instant::now();
        let current_exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_owned());
        tracing::info!(
            target: "tau_harness::startup",
            pid = std::process::id(),
            current_exe = %current_exe,
            version = env!("CARGO_PKG_VERSION"),
            build = %crate::version::build_revision(),
            "harness component starting",
        );
        // Make TAU_VERSION/TAU_BUILD/TAU_LAST_MODIFIED visible to anything
        // we spawn (shell extension, sub-agents) by reading our own
        // `built` snapshot — saves the parent CLI from having to forward
        // these via env vars on every daemon launch.
        crate::version::export_to_env();
        let project_root = std::env::current_dir()?;
        tracing::debug!(target: "tau_harness::startup", project_root = %project_root.display(), elapsed_ms = startup_started_at.elapsed().as_millis(), "resolved project root");
        let extension_overrides = launch.extension_overrides(std::env::var_os(
            crate::settings::EXTENSION_CLI_OVERRIDES_ENV,
        ))?;
        let config = resolve_config_with_extension_cli_overrides(&extension_overrides)?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "resolved config");
        // The CLI passes the minted/resumed session id via the harness's
        // SESSION_ID env var when spawning a daemon. Fallback to
        // `default_session_id()` covers a bare `tau component harness`
        // launched without a CLI in front of it.
        let eager_session_id = std::env::var("TAU_SESSION_ID")
            .unwrap_or_else(|_| tau_session_inspect::default_session_id().to_owned());
        let session_status = match std::env::var("TAU_SESSION_STATUS").as_deref() {
            Ok("resumed") => path_crate_daemon::SessionLaunchStatus::Resumed,
            _ => path_crate_daemon::SessionLaunchStatus::New,
        };
        let storage_mode = if std::env::var_os(MEMORY_ONLY_ENV).is_some() {
            HarnessStorageMode::MemoryOnly
        } else if std::env::var_os(EPHEMERAL_ENV).is_some() {
            HarnessStorageMode::SessionEphemeral
        } else {
            HarnessStorageMode::Durable
        };
        let runtime_instance_id =
            launch.runtime_instance_id(std::env::var_os(runtime_dir::HARNESS_INSTANCE_ID_ENV))?;
        run_harness_daemon_with_internal_tools_and_initial_client(
            &project_root,
            &config,
            &eager_session_id,
            // Exit once the spawning UI leaves. A UI that wants the
            // daemon to outlive it sends `ui_detach_request`, which
            // flips this to `false` at runtime.
            ServeOptions {
                exit_on_disconnect: true,
                session_status,
                storage_mode,
                ..Default::default()
            },
            internal_tool_handlers,
            RuntimeHarnessLaunch {
                runtime_instance_id,
                initial_client,
                initial_client_error_stream: initial_client_error_output.take(),
            },
        )
        .map_err(Into::into)
    })();
    if let Err(error) = result.as_ref() {
        send_initial_client_startup_error(initial_client_error_output.take(), error.as_ref());
    }
    result
}

fn notify_startup_error<T, E>(
    result: Result<T, E>,
    stream: &mut Option<InitialClientStartupErrorOutput>,
) -> Result<T, HarnessError>
where
    E: fmt::Display + Into<HarnessError>,
{
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            send_initial_client_startup_error(stream.take(), &error);
            Err(error.into())
        }
    }
}

fn notify_startup_error_after_accept<T, E>(
    result: Result<T, E>,
    stream: &mut Option<InitialClientStartupErrorOutput>,
    harness: &mut Harness,
    initial_client_id: Option<&ConnectionId>,
) -> Result<T, HarnessError>
where
    E: fmt::Display + Into<HarnessError>,
{
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            if stream.is_some() {
                send_initial_client_startup_error(stream.take(), &error);
            } else {
                harness.send_startup_disconnect_to_initial_client(initial_client_id, &error);
            }
            Err(error.into())
        }
    }
}

fn send_initial_client_startup_error(
    output: Option<InitialClientStartupErrorOutput>,
    error: &dyn fmt::Display,
) {
    let Some(output) = output else {
        return;
    };
    match output {
        #[cfg(test)]
        InitialClientStartupErrorOutput::Stream(stream) => {
            let mut writer = HarnessOutputWriter::new(stream);
            let _ = writer.write_message(&HarnessOutputMessage::Disconnect(Disconnect {
                reason: Some(format!("harness startup failed: {error}")),
            }));
            let _ = writer.flush();
        }
        InitialClientStartupErrorOutput::Stdout => {
            let mut writer = HarnessOutputWriter::new(io::stdout().lock());
            let _ = writer.write_message(&HarnessOutputMessage::Disconnect(Disconnect {
                reason: Some(format!("harness startup failed: {error}")),
            }));
            let _ = writer.flush();
        }
    }
}

#[cfg(test)]
mod tests;
