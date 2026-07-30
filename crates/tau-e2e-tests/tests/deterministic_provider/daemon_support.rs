//! Bounded child-daemon protocol support for deterministic acceptance.
#![allow(dead_code)]

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::{fs as path_std_fs, thread};

use nix::sys::signal as path_nix_sys_signal;
use nix::{sys as path_nix_sys, unistd as path_nix_unistd};
use tau_e2e_tests::DeterministicFixture;
use tau_proto::{
    ClientKind, Event, EventName, EventSelector, HarnessInputMessage, HarnessOutputMessage, Hello,
    Subscribe,
};
use tau_socket::{SocketPeer, SocketReceive};

use super::*;

/// Owns one daemon child and enforces bounded cleanup when a test returns
/// early through `?`.
pub(super) struct DaemonGuard {
    /// Killable test-only daemon process.
    child: Option<Child>,
    /// Whether a consuming shutdown method already reaped the daemon.
    completed: bool,
    /// Captured synthetic daemon diagnostic.
    stderr_path: std::path::PathBuf,
    /// Dedicated process group containing daemon and supervised extensions.
    pgid: nix::unistd::Pid,
    /// Unix socket that must disappear with this daemon generation.
    socket_path: std::path::PathBuf,
}

/// Proof handle for one forcibly terminated daemon generation.
pub(super) struct TerminatedDaemon {
    /// Former private process group.
    pgid: nix::unistd::Pid,
    /// Former generation-specific socket.
    socket_path: std::path::PathBuf,
}

impl TerminatedDaemon {
    /// Requires the old process group, socket, and durable session lock to be
    /// absent before another daemon generation starts.
    pub(super) fn require_gone(
        &self,
        harness_state_dir: &Path,
        session_id: &str,
    ) -> Result<(), String> {
        if process_group_exists(self.pgid) {
            return Err("crashed daemon process group still exists".to_owned());
        }
        if self.socket_path.exists() {
            return Err("crashed daemon socket still exists".to_owned());
        }
        let sessions = harness_state_dir.join("sessions");
        if tau_harness::session_is_locked(&sessions, session_id)
            .map_err(|error| format!("probe crashed daemon session lock: {error}"))?
        {
            return Err("crashed daemon session lock is still held".to_owned());
        }
        Ok(())
    }
}

impl DaemonGuard {
    /// Sends uncatchable termination to the complete private daemon process
    /// group and requires every member to disappear under a bounded deadline.
    ///
    /// Unlike [`Self::finish`], this deliberately does not require graceful
    /// socket cleanup or a successful parent exit.
    pub(super) fn kill_ungracefully(mut self) -> Result<TerminatedDaemon, String> {
        use std::os::unix::process::ExitStatusExt;

        path_nix_sys::signal::killpg(self.pgid, path_nix_sys_signal::Signal::SIGKILL)
            .map_err(|error| format!("kill deterministic daemon process group: {error}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let child = self.child.as_mut().expect("daemon guard owns child");
        let status = loop {
            match child.try_wait().map_err(|error| error.to_string())? {
                Some(status) => break status,
                None if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                None => return Err("crashed daemon parent exceeded reap deadline".to_owned()),
            }
        };
        if status.signal() != Some(nix::libc::SIGKILL) {
            return Err(format!(
                "crashed daemon parent did not exit from SIGKILL: {status}"
            ));
        }
        if !wait_for_process_group_exit(self.pgid, Duration::from_secs(2)) {
            return Err("crashed daemon process group survived SIGKILL deadline".to_owned());
        }
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)
                .map_err(|error| format!("remove dead daemon socket: {error}"))?;
        }
        let terminated = TerminatedDaemon {
            pgid: self.pgid,
            socket_path: self.socket_path.clone(),
        };
        self.child.take();
        self.completed = true;
        Ok(terminated)
    }

    /// Waits within the cleanup deadline, reaps the daemon, and returns its
    /// terminal result.
    pub(super) fn finish(mut self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let child = self.child.as_mut().expect("daemon guard owns child");
        let status = loop {
            match child.try_wait().map_err(|error| error.to_string())? {
                Some(status) => break status,
                None if Instant::now() < deadline => thread::yield_now(),
                None => {
                    child.kill().map_err(|error| error.to_string())?;
                    let _ = child.wait();
                    return Err("deterministic daemon exceeded shutdown deadline".to_owned());
                }
            }
        };
        let clean_group_exit = wait_for_process_group_exit(self.pgid, Duration::from_secs(2));
        if !clean_group_exit {
            force_process_group_cleanup(self.pgid)?;
        }
        if self.socket_path.exists() {
            return Err("daemon socket survived process-group cleanup".to_owned());
        }
        self.child.take();
        self.completed = true;
        if !clean_group_exit {
            return Err(
                "daemon process group required forced cleanup after parent exit".to_owned(),
            );
        }
        if status.success() {
            Ok(())
        } else {
            let diagnostic =
                std::fs::read_to_string(&self.stderr_path).map_err(|error| error.to_string())?;
            Err(if diagnostic.is_empty() {
                format!("deterministic daemon exited with {status}")
            } else {
                diagnostic
            })
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        if let Some(mut child) = self.child.take() {
            let _ = path_nix_sys::signal::killpg(self.pgid, path_nix_sys_signal::Signal::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && process_group_exists(self.pgid) {
                thread::sleep(Duration::from_millis(5));
            }
            if process_group_exists(self.pgid) {
                let _ =
                    path_nix_sys::signal::killpg(self.pgid, path_nix_sys_signal::Signal::SIGKILL);
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline && process_group_exists(self.pgid) {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            let _ = child.wait();
        }
    }
}

pub(super) fn spawn_daemon(
    fixture: &DeterministicFixture,
    socket: &Path,
    status: tau_harness::SessionLaunchStatus,
) -> DaemonGuard {
    // Mark orchestration incomplete until exact scenario consumption succeeds.
    fixture.mark_daemon_started();
    let stderr_path = fixture
        .root()
        .join(format!("daemon-{}.stderr", status_label(status)));
    let stderr = path_std_fs::File::create(&stderr_path).expect("create daemon stderr");
    let mut command = Command::new(HARNESS_DAEMON);
    command
        .env_clear()
        .env("HOME", fixture.root().join("home"))
        .env("XDG_CONFIG_HOME", fixture.root().join("xdg-config"))
        .env("XDG_STATE_HOME", fixture.root().join("xdg-state"))
        .env("XDG_CACHE_HOME", fixture.root().join("xdg-cache"))
        .env("XDG_RUNTIME_DIR", fixture.root().join("xdg-runtime"))
        .env("LANG", "C.UTF-8")
        .process_group(0)
        .arg(socket)
        .arg(fixture.harness_state_dir())
        .arg(fixture.root().join("config"))
        .arg(fixture.root().join("state"))
        .arg(status_label(status));
    if fixture.core_shell_enabled() {
        command.arg(fixture.shell_base());
    }
    if fixture.dummy_enabled() {
        command.arg("--test-dummy");
    }
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn deterministic daemon");
    let pgid = path_nix_unistd::Pid::from_raw(child.id().try_into().expect("daemon pid fits i32"));
    DaemonGuard {
        child: Some(child),
        completed: false,
        stderr_path,
        pgid,
        socket_path: socket.to_path_buf(),
    }
}

fn process_group_exists(pgid: nix::unistd::Pid) -> bool {
    path_nix_sys::signal::killpg(pgid, None).is_ok()
}

fn wait_for_process_group_exit(pgid: nix::unistd::Pid, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline && process_group_exists(pgid) {
        thread::sleep(Duration::from_millis(5));
    }
    !process_group_exists(pgid)
}

fn force_process_group_cleanup(pgid: nix::unistd::Pid) -> Result<(), String> {
    let _ = path_nix_sys::signal::killpg(pgid, path_nix_sys_signal::Signal::SIGTERM);
    if wait_for_process_group_exit(pgid, Duration::from_secs(2)) {
        return Ok(());
    }
    let _ = path_nix_sys::signal::killpg(pgid, path_nix_sys_signal::Signal::SIGKILL);
    if wait_for_process_group_exit(pgid, Duration::from_secs(2)) {
        return Ok(());
    }
    Err("daemon process group survived SIGKILL cleanup".to_owned())
}

/// Proves successful `finish` never repairs a leaked process-group member and
/// reports success; forced termination remains failure containment only.
#[test]
fn daemon_finish_rejects_a_lingering_process_group_member() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let stderr_path = tempdir.path().join("daemon.stderr");
    std::fs::write(&stderr_path, b"").expect("stderr file");
    let child = Command::new("sh")
        .arg("-c")
        .arg("sleep 30 & exit 0")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn lingering process group");
    let pgid = path_nix_unistd::Pid::from_raw(child.id().try_into().expect("child pid fits i32"));
    let guard = DaemonGuard {
        child: Some(child),
        completed: false,
        stderr_path,
        pgid,
        socket_path: tempdir.path().join("absent.sock"),
    };
    let error = guard.finish().expect_err("lingering member must fail");
    assert!(error.contains("required forced cleanup"), "{error}");
    assert!(!process_group_exists(pgid));
}

pub(super) fn status_label(status: tau_harness::SessionLaunchStatus) -> &'static str {
    match status {
        tau_harness::SessionLaunchStatus::New => "new",
        tau_harness::SessionLaunchStatus::Resumed => "resumed",
    }
}

pub(super) fn connect_ui(socket: &Path) -> Result<SocketPeer, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut peer = loop {
        match SocketPeer::connect(socket) {
            Ok(peer) => break peer,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error.into()),
        }
    };
    peer.send(&HarnessInputMessage::Hello(Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: tau_proto::ExtensionName::parse("tau-e2e-daemon")
            .expect("test extension name must satisfy the identifier grammar"),
        client_kind: ClientKind::Ui,
        expected_session_id: None,
        capabilities: Default::default(),
    }))?;
    let selectors = [
        EventName::AGENT_PROMPT_CREATED,
        EventName::SESSION_STARTED,
        EventName::SESSION_AGENT_LOADED,
        EventName::AGENT_STARTED,
        EventName::PROVIDER_PROMPT_SUBMITTED,
        EventName::PROVIDER_RESPONSE_FINISHED,
        EventName::AGENT_PROMPT_TERMINATED,
        EventName::AGENT_STATS_UPDATED,
        EventName::EXTENSION_EXITED,
        EventName::EXTENSION_RESTARTING,
        EventName::HARNESS_NOTICE,
        EventName::AGENT_METADATA_SET,
        EventName::EXTENSION_READY,
        EventName::EXTENSION_CONTEXT_READY,
        EventName::EXTENSION_SESSION_CONTEXT_READY,
        EventName::AGENT_REPLAY_COMPLETE,
        EventName::SESSION_REPLAY_COMPLETE,
        EventName::TOOL_RESULT,
    ]
    .into_iter()
    .map(EventSelector::Exact)
    .collect::<Vec<_>>();
    peer.send(&HarnessInputMessage::Subscribe(Subscribe {
        historical_selectors: selectors.clone(),
        live_selectors: selectors,
    }))?;
    Ok(peer)
}

pub(super) fn create_agent(
    peer: &mut SocketPeer,
    ctx_id: &str,
    prompt: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    peer.send(&HarnessInputMessage::emit(Event::UiCreateAgent(
        tau_proto::UiCreateAgent {
            request_id: "deterministic-daemon-create".to_owned(),
            literal: false,
            session_id: "deterministic-e2e-session"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            role: "deterministic-e2e".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: Some(prompt.to_owned()),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some(ctx_id.to_owned()),
            parent_agent: None,
            ephemeral: false,
        },
    )))?;
    Ok(())
}

pub(super) fn submit_prompt(
    peer: &mut SocketPeer,
    agent_id: &tau_proto::AgentId,
    ctx_id: &str,
    prompt: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    peer.send(&HarnessInputMessage::emit(Event::UiPromptSubmitted(
        tau_proto::UiPromptSubmitted {
            literal: false,
            session_id: "deterministic-e2e-session"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            text: prompt.to_owned(),
            agent_id: agent_id.clone(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some(ctx_id.to_owned()),
        },
    )))?;
    Ok(())
}

pub(super) fn cancel_prompt(
    peer: &mut SocketPeer,
    prompt: &tau_proto::AgentPromptCreated,
) -> Result<(), Box<dyn std::error::Error>> {
    peer.send(&HarnessInputMessage::emit(Event::UiCancelPrompt(
        tau_proto::UiCancelPrompt {
            session_id: "deterministic-e2e-session"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            target_agent_id: Some(prompt.agent_id.clone()),
            agent_prompt_id: Some(prompt.agent_prompt_id.clone()),
        },
    )))?;
    Ok(())
}

pub(super) fn disconnect_ui(peer: &mut SocketPeer) -> Result<(), Box<dyn std::error::Error>> {
    peer.send(&HarnessInputMessage::Disconnect(tau_proto::Disconnect {
        reason: Some("test complete".to_owned()),
    }))?;
    Ok(())
}

pub(super) fn recv_event(peer: &mut SocketPeer) -> Result<Event, Box<dyn std::error::Error>> {
    Ok(recv_observed(peer)?.event)
}

/// Drains events already delivered before the daemon closes its socket.
pub(super) fn recv_remaining_events(
    peer: &mut SocketPeer,
) -> Result<Vec<Event>, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match peer.recv_timeout(remaining)? {
            SocketReceive::Message {
                message: HarnessOutputMessage::Deliver(delivery),
            } => events.push(delivery.into_parts().0),
            SocketReceive::Message {
                message: HarnessOutputMessage::Disconnect(_),
            }
            | SocketReceive::Closed => return Ok(events),
            SocketReceive::Message { .. } => {}
            SocketReceive::Timeout => {
                return Err("timed out draining terminal daemon events".into());
            }
        }
    }
}

pub(super) struct DaemonObserved {
    /// Delivered typed event.
    pub event: Event,
    /// Whether the delivery belongs to historical replay.
    pub replay: bool,
    /// Durable append timestamp when the event came from a journal.
    pub recorded_at: Option<tau_proto::UnixMicros>,
}

pub(super) fn recv_observed(
    peer: &mut SocketPeer,
) -> Result<DaemonObserved, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match peer.recv_timeout(remaining)? {
            SocketReceive::Message {
                message: HarnessOutputMessage::Deliver(delivery),
            } => {
                let (event, replay, recorded_at) = delivery.into_parts();
                return Ok(DaemonObserved {
                    event,
                    replay,
                    recorded_at,
                });
            }
            SocketReceive::Message {
                message: HarnessOutputMessage::Disconnect(disconnect),
            } => {
                return Err(disconnect
                    .reason
                    .unwrap_or_else(|| "daemon disconnected".to_owned())
                    .into());
            }
            SocketReceive::Message { .. } => {}
            SocketReceive::Timeout => return Err("timed out waiting for daemon event".into()),
            SocketReceive::Closed => return Err("daemon socket closed".into()),
        }
    }
}

pub(super) fn recv_until_finished(
    peer: &mut SocketPeer,
) -> Result<tau_proto::ProviderResponseFinished, Box<dyn std::error::Error>> {
    loop {
        if let Event::ProviderResponseFinished(value) = recv_event(peer)? {
            return Ok(value);
        }
    }
}

pub(super) fn recv_until_finished_for(
    peer: &mut SocketPeer,
    prompt_id: &tau_proto::AgentPromptId,
) -> Result<tau_proto::ProviderResponseFinished, Box<dyn std::error::Error>> {
    loop {
        if let Event::ProviderResponseFinished(value) = recv_event(peer)?
            && &value.agent_prompt_id == prompt_id
        {
            return Ok(value);
        }
    }
}

pub(super) fn recv_until_created(
    peer: &mut SocketPeer,
    ctx_id: Option<&str>,
) -> Result<tau_proto::AgentPromptCreated, Box<dyn std::error::Error>> {
    loop {
        if let Event::AgentPromptCreated(value) = recv_event(peer)?
            && value.ctx_id.as_deref() == ctx_id
        {
            return Ok(value);
        }
    }
}

pub(super) fn recv_until_submitted(
    peer: &mut SocketPeer,
) -> Result<tau_proto::ProviderPromptSubmitted, Box<dyn std::error::Error>> {
    loop {
        if let Event::ProviderPromptSubmitted(value) = recv_event(peer)? {
            return Ok(value);
        }
    }
}
