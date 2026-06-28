//! Reusable end-to-end test utilities for `tau` crates.

use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tau_config::settings::TauDirs;
use tau_core::{AgentStore, AgentStoreError, PolicyStore, SessionStore};
use tau_harness::{
    HarnessError, ServeOptions, run_daemon_with_echo, run_embedded_message_with_echo,
    send_daemon_message,
};
use tau_session_inspect::{InspectError, open_policy_store, open_session_store};
use tempfile::TempDir;

/// Temporary runtime paths for end-to-end tests.
#[derive(Debug)]
pub struct TestRuntime {
    _tempdir: TempDir,
    /// Filesystem path where the spawned test daemon binds its Unix socket.
    pub socket_path: PathBuf,
    /// Per-state directory containing session subdirs and `policy.cbor`.
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

    /// Starts a foreground daemon in a background thread, eager-initing
    /// the given session id (typically what test code will then send a
    /// message to).
    ///
    /// `max_clients` is passed through to [`ServeOptions::max_clients`]. Use
    /// `Some(n)` for tests that later call [`DaemonHandle::join`], so the
    /// daemon exits after a bounded number of clients; `None` leaves it
    /// unbounded.
    pub fn spawn_daemon(&self, eager_session_id: &str, max_clients: Option<usize>) -> DaemonHandle {
        let socket_path = self.socket_path.clone();
        let state_dir = self.state_dir.clone();
        let dirs = self.dirs.clone();
        let eager_session_id = eager_session_id.to_owned();
        let join_handle = thread::spawn(move || {
            let options = ServeOptions::builder()
                .dirs(dirs)
                .maybe_max_clients(max_clients)
                .build();
            run_daemon_with_echo(socket_path, state_dir, &eager_session_id, options)
        });
        DaemonHandle { join_handle }
    }

    /// Waits until the daemon socket exists.
    pub fn wait_until_ready(&self, timeout: Duration) -> Result<(), WaitError> {
        wait_for_path(&self.socket_path, timeout)
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

    /// Opens the policy store for assertions.
    pub fn open_policy_store(&self) -> Result<PolicyStore, InspectError> {
        open_policy_store(self.state_dir.join("policy.cbor"))
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

/// Waits until one filesystem path exists.
pub fn wait_for_path(path: &Path, timeout: Duration) -> Result<(), WaitError> {
    let started_at = Instant::now();
    while !path.exists() {
        if timeout <= started_at.elapsed() {
            return Err(WaitError::Timeout {
                path: path.to_path_buf(),
                timeout,
            });
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Error returned when waiting for a test condition times out.
#[derive(Debug)]
pub enum WaitError {
    /// The requested filesystem path did not appear before the configured
    /// timeout elapsed.
    Timeout {
        /// Filesystem path that was being waited on.
        path: PathBuf,
        /// Maximum duration allowed for the path to appear.
        timeout: Duration,
    },
}

impl std::fmt::Display for WaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout { path, timeout } => write!(
                f,
                "timed out waiting for path {} after {timeout:?}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for WaitError {}

#[cfg(test)]
mod tests;
