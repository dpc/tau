//! Reusable end-to-end test utilities for `tau` crates.

use std::os::unix as path_std_os_unix;
use std::path::PathBuf;
use std::thread::{self, JoinHandle};

use tau_config::settings::TauDirs;
use tau_core::{AgentStore, AgentStoreError, SessionStore};
use tau_harness::{
    HarnessError, InteractionOutcome, ServeOptions, run_daemon_with_echo_on_listener,
    run_embedded_message_with_echo, run_embedded_message_with_test_provider, send_daemon_message,
};
use tau_session_inspect::{InspectError, open_session_store};
use tau_socket::SocketListener;
use tempfile::TempDir;

/// Completed causal quota fixture plus the exact harness-committed event trace.
#[derive(Clone, Debug, PartialEq)]
pub struct CausalQuotaOutcome {
    /// Embedded interaction result, including the exact tool call and result.
    pub interaction: InteractionOutcome,
    /// Ordered events committed by the harness during the interaction.
    pub events: Vec<tau_proto::Event>,
}

/// Failure while running or decoding the causal quota fixture.
#[derive(Debug)]
pub enum CausalQuotaError {
    /// Embedded harness/provider execution failed.
    Harness(HarnessError),
    /// Local trace storage could not be read.
    Io(std::io::Error),
    /// One trace line was not valid JSON.
    TraceJson {
        /// One-based line number in `events.jsonl`.
        line: usize,
        /// Original JSON decoder failure.
        source: serde_json::Error,
    },
    /// A published event payload was incompatible with the protocol schema.
    TraceEvent {
        /// One-based line number in `events.jsonl`.
        line: usize,
        /// Original typed-event decoder failure.
        source: serde_json::Error,
    },
    /// A valid JSON line did not satisfy the trace record schema.
    TraceShape {
        /// One-based line number in `events.jsonl`.
        line: usize,
        /// Description of the invalid record shape.
        message: String,
    },
}

impl std::fmt::Display for CausalQuotaError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Harness(error) => write!(formatter, "causal quota harness: {error}"),
            Self::Io(error) => write!(formatter, "causal quota trace I/O: {error}"),
            Self::TraceJson { line, source } => {
                write!(
                    formatter,
                    "invalid causal trace JSON on line {line}: {source}"
                )
            }
            Self::TraceEvent { line, source } => {
                write!(
                    formatter,
                    "invalid published event on causal trace line {line}: {source}"
                )
            }
            Self::TraceShape { line, message } => {
                write!(
                    formatter,
                    "invalid causal trace record on line {line}: {message}"
                )
            }
        }
    }
}

impl std::error::Error for CausalQuotaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Harness(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::TraceJson { source, .. } | Self::TraceEvent { source, .. } => Some(source),
            Self::TraceShape { .. } => None,
        }
    }
}

impl From<HarnessError> for CausalQuotaError {
    fn from(error: HarnessError) -> Self {
        Self::Harness(error)
    }
}

impl From<std::io::Error> for CausalQuotaError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Runs the feature-gated quota provider through the embedded harness.
///
/// The returned trace causally spans local Responses classification, manual
/// provider retry, tool execution, tool-result continuation, and final harness
/// commit. It performs no external network or credential access.
pub fn run_causal_quota_fixture(
    state_dir: impl Into<PathBuf>,
) -> Result<CausalQuotaOutcome, CausalQuotaError> {
    fn provider_runner(
        reader: path_std_os_unix::net::UnixStream,
        writer: path_std_os_unix::net::UnixStream,
    ) -> Result<(), String> {
        tau_ext_provider_builtin::run_quota_recovery_fixture(reader, writer)
    }

    let state_dir = state_dir.into();
    let interaction = run_embedded_message_with_test_provider(
        &state_dir,
        "s1",
        "causal quota fixture",
        provider_runner,
    )?;
    let trace_path = tau_config::settings::sessions_dir_of(&state_dir)
        .join("s1")
        .join("events.jsonl");
    let raw = std::fs::read_to_string(trace_path)?;
    let events = parse_published_trace_events(&raw)?;
    Ok(CausalQuotaOutcome {
        interaction,
        events,
    })
}

/// Parses round-trippable published events from the best-effort debug trace.
///
/// Fixed-shape diagnostic summaries such as `agent.prompt_created` are skipped
/// because they intentionally are not protocol events.
pub fn parse_published_trace_events(raw: &str) -> Result<Vec<tau_proto::Event>, CausalQuotaError> {
    let mut events = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let line_number = index + 1;
        let entry: serde_json::Value =
            serde_json::from_str(line).map_err(|source| CausalQuotaError::TraceJson {
                line: line_number,
                source,
            })?;
        let object = entry
            .as_object()
            .ok_or_else(|| CausalQuotaError::TraceShape {
                line: line_number,
                message: "record must be an object".to_owned(),
            })?;
        let record_type = object
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| CausalQuotaError::TraceShape {
                line: line_number,
                message: "record must have a string type".to_owned(),
            })?;
        if record_type != "published" {
            continue;
        }
        // Full provider prompts are deliberately represented by a bounded,
        // content-free summary in events.jsonl. They remain observable live,
        // but the debug row is not a round-trippable protocol event.
        if object.get("event_name").and_then(serde_json::Value::as_str)
            == Some("agent.prompt_created")
        {
            continue;
        }
        let payload = object
            .get("event")
            .cloned()
            .ok_or_else(|| CausalQuotaError::TraceShape {
                line: line_number,
                message: "published record has no event".to_owned(),
            })?;
        let event =
            serde_json::from_value(payload).map_err(|source| CausalQuotaError::TraceEvent {
                line: line_number,
                source,
            })?;
        events.push(event);
    }
    Ok(events)
}

/// Temporary runtime paths for end-to-end tests.
#[derive(Debug)]
pub struct TestRuntime {
    _tempdir: TempDir,
    /// Filesystem path where the spawned test daemon binds its Unix socket.
    pub socket_path: PathBuf,
    /// Per-state directory containing session and agent data.
    pub state_dir: PathBuf,
    /// Isolated `$XDG_CONFIG_HOME`/`$XDG_STATE_HOME` layout so tests don't
    /// leak into (or read from) the developer's real `~/.config/tau` and
    /// `~/.local/state/tau`.
    pub dirs: TauDirs,
}

impl TestRuntime {
    /// Creates isolated temporary paths for one test runtime.
    ///
    /// The echo harness bypasses provider-owned model publication and answers
    /// through in-process test fixtures, which is enough for tests asserting
    /// "response is non-empty" while keeping real provider credentials out of
    /// end-to-end tests.
    pub fn new() -> Result<Self, std::io::Error> {
        let tempdir = TempDir::new()?;
        let config_dir = tempdir.path().join("config");
        let state_dir = tempdir.path().join("state");
        std::fs::create_dir_all(&config_dir)?;
        std::fs::create_dir_all(&state_dir)?;
        Ok(Self {
            socket_path: tempdir.path().join("daemon.sock"),
            state_dir: state_dir.clone(),
            dirs: TauDirs {
                config_dir: Some(config_dir),
                state_dir: Some(state_dir),
            },
            _tempdir: tempdir,
        })
    }

    /// Runs one embedded interaction and returns the agent response.
    pub fn run_embedded(&self, session_id: &str, message: &str) -> Result<String, HarnessError> {
        Ok(run_embedded_message_with_echo(&self.state_dir, session_id, message)?.response)
    }

    /// Binds then starts a foreground daemon in a background thread,
    /// eager-initing the given session id (typically what test code will
    /// then send a message to).
    ///
    /// This returns only after [`SocketListener::bind`] has completed, so its
    /// socket has entered the kernel listen state. `max_clients` is passed
    /// through to [`ServeOptions::max_clients`]. Use `Some(n)` for tests
    /// that later call [`DaemonHandle::join`], so the daemon exits after a
    /// bounded number of clients; `None` leaves it unbounded.
    pub fn spawn_daemon(
        &self,
        eager_session_id: &str,
        max_clients: Option<usize>,
    ) -> Result<DaemonHandle, HarnessError> {
        let listener = SocketListener::bind(&self.socket_path)?;
        let state_dir = self.state_dir.clone();
        let dirs = self.dirs.clone();
        let eager_session_id = eager_session_id.to_owned();
        let join_handle = thread::spawn(move || {
            let options = ServeOptions::builder()
                .dirs(dirs)
                .maybe_max_clients(max_clients)
                .build();
            run_daemon_with_echo_on_listener(listener, state_dir, &eager_session_id, options)
        });
        Ok(DaemonHandle { join_handle })
    }

    /// Sends one message to a running daemon.
    pub fn send_daemon_message(
        &self,
        session_id: &str,
        message: &str,
    ) -> Result<String, HarnessError> {
        send_daemon_message(&self.socket_path, session_id, message)
    }

    /// Opens the session store for assertions.
    pub fn open_session_store(&self) -> Result<SessionStore, InspectError> {
        open_session_store(tau_config::settings::sessions_dir_of(&self.state_dir))
    }

    /// Opens the agent store for transcript assertions.
    pub fn open_agent_store(&self) -> Result<AgentStore, AgentStoreError> {
        AgentStore::open(self.state_dir.join("agents"))
    }
}

/// A running daemon thread handle.
#[derive(Debug)]
pub struct DaemonHandle {
    join_handle: JoinHandle<Result<(), HarnessError>>,
}

impl DaemonHandle {
    /// Waits for the daemon thread to finish.
    ///
    /// # Errors
    ///
    /// Returns any [`HarnessError`] produced by the daemon serve loop. If the
    /// daemon thread panicked, returns [`HarnessError::ThreadJoin`] with the
    /// panic payload string when it is available.
    pub fn join(self) -> Result<(), HarnessError> {
        match self.join_handle.join() {
            Ok(result) => result,
            Err(payload) => Err(HarnessError::ThreadJoin(format!(
                "daemon ({})",
                panic_payload_label(&payload)
            ))),
        }
    }
}

fn panic_payload_label<'a>(payload: &'a (dyn std::any::Any + Send + 'static)) -> &'a str {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.as_str()
    } else {
        "non-string panic payload"
    }
}

#[cfg(test)]
mod tests;
