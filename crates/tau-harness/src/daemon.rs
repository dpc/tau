//! Public entry points: blocking `run_*` daemons, the embedded
//! single-message helpers, and the small types passed to/from them.

use std::os::unix::net::UnixListener;
#[cfg(any(test, feature = "echo-agent"))]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{fmt, io, thread};

use tau_proto::{
    ClientKind, ConnectionId, Disconnect, Event, EventSelector, HarnessInputMessage,
    HarnessOutputMessage, HarnessOutputWriter, Hello, PROTOCOL_VERSION, Subscribe, UiCreateAgent,
};
use tau_socket::{SocketListener, SocketPeer, SocketReceive};

use crate::error::HarnessError;
use crate::event::HarnessEvent;
use crate::format::{format_extension_event, format_tool_progress};
use crate::harness::{
    Harness, InitialClient, InitialClientStartupErrorOutput, assistant_text_from_output_items,
    tool_calls_from_output_items,
};
use crate::runtime_dir;
use crate::settings::{Config, resolve_config, resolve_config_in};

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
    /// no attached UIs — a later `tau --attach` can pick up the
    /// session. The `ui.detach_request` event flips this at runtime.
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
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            max_clients: None,
            exit_on_disconnect: false,
            session_status: SessionLaunchStatus::New,
            dirs: None,
        }
    }
}

/// One completed user interaction with optional progress updates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractionOutcome {
    pub lifecycle_messages: Vec<String>,
    pub progress_messages: Vec<String>,
    pub response: String,
}

/// Options for a one-shot embedded run.
#[derive(Clone, Debug, Default, Eq, PartialEq, bon::Builder)]
pub struct EmbeddedOptions {
    /// Directory layout (config + state) the harness reads. Defaults to
    /// [`tau_config::settings::TauDirs::default()`] on the call site.
    pub dirs: Option<tau_config::settings::TauDirs>,
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

enum ListenerHandle {
    // Externally supplied by socket activation; the daemon must not unlink its path.
    SocketActivated(UnixListener),
    // Bound by this daemon; `SocketListener` owns identity-checked path cleanup.
    Bound(SocketListener),
}

impl ListenerHandle {
    fn spawn_forwarder(
        &self,
        tx: mpsc::Sender<HarnessEvent>,
    ) -> Result<ListenerForwarder, HarnessError> {
        let listener = match self {
            Self::SocketActivated(listener) => listener.try_clone().map_err(HarnessError::Io)?,
            Self::Bound(listener) => listener.try_clone_raw_listener()?,
        };
        spawn_listener_forwarder(listener, tx)
    }
}

struct ListenerForwarder {
    // Stop signal sent before joining the accept-loop thread.
    stop_tx: mpsc::Sender<()>,
    // Accept-loop thread joined during `ListenerForwarder` drop.
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for ListenerForwarder {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn spawn_listener_forwarder(
    listener: UnixListener,
    tx: mpsc::Sender<HarnessEvent>,
) -> Result<ListenerForwarder, HarnessError> {
    listener.set_nonblocking(true).map_err(HarnessError::Io)?;
    let (stop_tx, stop_rx) = mpsc::channel();
    let join = thread::spawn(move || {
        loop {
            if stop_rx.try_recv().is_ok() {
                return;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    if tx.send(HarnessEvent::NewClient(stream)).is_err() {
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if stop_rx.recv_timeout(Duration::from_millis(20)).is_ok() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
    Ok(ListenerForwarder {
        stop_tx,
        join: Some(join),
    })
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
    let (config, dirs) = match options.dirs {
        Some(dirs) => {
            let config = resolve_config_in(&dirs)
                .map_err(|error| HarnessError::Participant(error.to_string()))?;
            (config, dirs)
        }
        None => {
            let dirs = tau_config::settings::TauDirs {
                config_dir: Some(state_dir.join("config")),
                state_dir: Some(state_dir.join("runtime")),
            };
            let config = resolve_config(None)
                .map_err(|error| HarnessError::Participant(error.to_string()))?;
            (config, dirs)
        }
    };
    let mut harness = Harness::from_config(
        &config,
        &state_dir,
        dirs,
        session_id,
        tau_proto::SessionStartReason::Initial,
    )?;
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
    harness.pending_agent_context_ready.clear();
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
    let socket_path = socket_path.into();
    let state_dir = state_dir.into();
    let listener_handle = open_listener(&socket_path)?;
    let (config, dirs) = match options.dirs.clone() {
        Some(dirs) => {
            let config = resolve_config_in(&dirs)
                .map_err(|error| HarnessError::Participant(error.to_string()))?;
            (config, dirs)
        }
        None => {
            let dirs = tau_config::settings::TauDirs {
                config_dir: Some(state_dir.join("config")),
                state_dir: Some(state_dir.join("runtime")),
            };
            let config = resolve_config(None)
                .map_err(|error| HarnessError::Participant(error.to_string()))?;
            (config, dirs)
        }
    };
    let mut harness = Harness::from_config(
        &config,
        state_dir,
        dirs,
        eager_session_id,
        session_start_reason(options.session_status),
    )?;

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
    fn parse_agent_prompt_index(agent_prompt_id: &str) -> Option<u64> {
        agent_prompt_id
            .strip_prefix("ap-")?
            .rsplit_once('-')?
            .1
            .parse()
            .ok()
    }

    let mut peer = SocketPeer::connect(socket_path)?;
    peer.send(&HarnessInputMessage::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-cli".into(),
        client_kind: ClientKind::Ui,
    }))?;
    peer.send(&HarnessInputMessage::Subscribe(Subscribe {
        selectors: vec![
            EventSelector::Prefix("agent.".to_owned()),
            EventSelector::Prefix("provider.".to_owned()),
            EventSelector::Prefix("session.".to_owned()),
            EventSelector::Prefix("tool.".to_owned()),
            EventSelector::Prefix("shell.".to_owned()),
            EventSelector::Prefix("extension.".to_owned()),
            EventSelector::Prefix("harness.".to_owned()),
        ],
    }))?;
    let ctx_id = next_ctx_id();
    peer.send(&HarnessInputMessage::emit(Event::UiCreateAgent(
        UiCreateAgent {
            parent_agent: None,
            session_id: session_id.into(),
            role: "senior-engineer".to_owned(),
            model_override: None,
            metadata: vec![tau_proto::AgentInitialMetadata {
                key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
                value: tau_proto::CborValue::Text(
                    std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("."))
                        .display()
                        .to_string(),
                ),
                inheritable: true,
            }],
            initial_prompt: Some(message.to_owned()),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some(ctx_id.clone()),
        },
    )))?;

    let started_at = Instant::now();
    let mut lifecycle_messages = Vec::new();
    let mut progress_messages = Vec::new();
    // Counter parsed out of the `AgentPromptCreated` whose `ctx_id`
    // matches our submit. The terminal `ProviderResponseFinished` has a
    // spid counter where `our_spid_counter <= terminal_counter` (equal when no tool
    // calls, higher when tool-result follow-ups bump the counter).
    let mut our_spid_counter: Option<u64> = None;
    loop {
        if SEND_DAEMON_MESSAGE_TIMEOUT <= started_at.elapsed() {
            return Err(HarnessError::ResponseTimeout);
        }
        if let Some(message) = recv_daemon_message(
            &mut peer,
            SEND_DAEMON_MESSAGE_TIMEOUT.saturating_sub(started_at.elapsed()),
        )? {
            match message {
                HarnessOutputMessage::Deliver(delivery) => match delivery.into_event() {
                    Event::ToolProgress(p) => progress_messages.push(format_tool_progress(&p)),
                    Event::ShellCommandProgress(_) => {
                        progress_messages.push("shell: running shell command".to_owned())
                    }
                    Event::HarnessNotice(info) => lifecycle_messages.push(info.message),
                    event @ (Event::ExtensionStarting(_)
                    | Event::ExtensionReady(_)
                    | Event::ExtensionExited(_)
                    | Event::ExtensionRestarting(_)) => {
                        lifecycle_messages.push(format_extension_event(&event));
                    }
                    Event::AgentPromptCreated(prompt)
                        if prompt.ctx_id.as_deref() == Some(ctx_id.as_str()) =>
                    {
                        our_spid_counter =
                            parse_agent_prompt_index(prompt.agent_prompt_id.as_ref());
                    }
                    Event::ProviderResponseFinished(finished)
                        if tool_calls_from_output_items(&finished.output_items).is_empty()
                            && our_spid_counter.is_some_and(|ours| {
                                parse_agent_prompt_index(finished.agent_prompt_id.as_ref())
                                    .is_some_and(|c| ours <= c)
                            }) =>
                    {
                        peer.send(&HarnessInputMessage::Disconnect(Disconnect {
                            reason: Some("done".to_owned()),
                        }))?;
                        return Ok(InteractionOutcome {
                            lifecycle_messages,
                            progress_messages,
                            response: assistant_text_from_output_items(&finished.output_items)
                                .unwrap_or_default(),
                        });
                    }
                    _ => {}
                },
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
        client_name: client_name.into(),
        client_kind: ClientKind::Ui,
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
    run_harness_daemon_with_internal_tools_and_initial_client(
        project_root,
        config,
        eager_session_id,
        options,
        internal_tool_handlers,
        None,
        None,
    )
}

fn run_harness_daemon_with_internal_tools_and_initial_client(
    project_root: &Path,
    config: &Config,
    eager_session_id: &str,
    options: ServeOptions,
    internal_tool_handlers: crate::InternalToolHandlers,
    initial_client: Option<InitialClient>,
    mut initial_client_error_stream: Option<InitialClientStartupErrorOutput>,
) -> Result<(), HarnessError> {
    let startup_started_at = Instant::now();
    tracing::debug!(target: "tau_harness::startup", project_root = %project_root.display(), eager_session_id, "starting harness daemon");
    let harness_paths = notify_startup_error(
        runtime_dir::prepare_harness_paths(project_root, eager_session_id),
        &mut initial_client_error_stream,
    )?;
    tracing::debug!(target: "tau_harness::startup", harness_path = %harness_paths.path().display(), elapsed_ms = startup_started_at.elapsed().as_millis(), "prepared harness paths");
    let listener = notify_startup_error(
        bind_listener(&harness_paths.socket_path()),
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
            session_start_reason(options.session_status),
            initial_client,
            &mut initial_client_error_stream,
        ),
        &mut initial_client_error_stream,
    )?;
    harness.install_internal_tool_handlers(internal_tool_handlers);
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

/// Entrypoint for `tau component harness`.
pub fn run_component() -> Result<(), Box<dyn std::error::Error>> {
    run_component_with_internal_tools(Vec::new())
}

/// Entrypoint for `tau component harness` with injected internal tool handlers.
pub fn run_component_with_internal_tools(
    internal_tool_handlers: crate::InternalToolHandlers,
) -> Result<(), Box<dyn std::error::Error>> {
    run_component_with_internal_tools_and_initial_client(internal_tool_handlers, None)
}

/// Entrypoint for `tau component harness` with injected internal tool handlers
/// and an initial UI connection carried over stdio.
pub fn run_component_with_internal_tools_and_initial_ui_stdio(
    internal_tool_handlers: crate::InternalToolHandlers,
) -> Result<(), Box<dyn std::error::Error>> {
    run_component_with_internal_tools_and_initial_client(
        internal_tool_handlers,
        Some(InitialClient::Stdio),
    )
}

fn run_component_with_internal_tools_and_initial_client(
    internal_tool_handlers: crate::InternalToolHandlers,
    initial_client: Option<InitialClient>,
) -> Result<(), Box<dyn std::error::Error>> {
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
        let config = resolve_config(None)?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "resolved config");
        // The CLI passes the minted/resumed session id via the harness's
        // SESSION_ID env var when spawning a daemon. Fallback to
        // `default_session_id()` covers a bare `tau component harness`
        // launched without a CLI in front of it.
        let eager_session_id = std::env::var("TAU_SESSION_ID")
            .unwrap_or_else(|_| tau_session_inspect::default_session_id().to_owned());
        let session_status = match std::env::var("TAU_SESSION_STATUS").as_deref() {
            Ok("resumed") => crate::daemon::SessionLaunchStatus::Resumed,
            _ => crate::daemon::SessionLaunchStatus::New,
        };
        run_harness_daemon_with_internal_tools_and_initial_client(
            &project_root,
            &config,
            &eager_session_id,
            // Exit once the spawning UI leaves. A UI that wants the
            // daemon to outlive it sends `ui.detach_request`, which
            // flips this to `false` at runtime.
            ServeOptions {
                exit_on_disconnect: true,
                session_status,
                ..Default::default()
            },
            internal_tool_handlers,
            initial_client,
            initial_client_error_output.take(),
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
