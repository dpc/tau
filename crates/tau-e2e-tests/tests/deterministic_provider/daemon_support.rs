//! Bounded child-daemon protocol support for deterministic acceptance.

use super::*;

/// Owns one daemon child and enforces bounded cleanup when a test returns
/// early through `?`.
pub(super) struct DaemonGuard {
    /// Killable test-only daemon process.
    child: Option<Child>,
    /// Whether [`Self::finish`] already reaped the daemon.
    completed: bool,
    /// Captured synthetic daemon diagnostic.
    stderr_path: std::path::PathBuf,
}

impl DaemonGuard {
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
        self.child.take();
        self.completed = true;
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
            let _ = child.kill();
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
    let stderr = std::fs::File::create(&stderr_path).expect("create daemon stderr");
    let child = Command::new(HARNESS_DAEMON)
        .arg(socket)
        .arg(fixture.harness_state_dir())
        .arg(fixture.root().join("config"))
        .arg(fixture.root().join("state"))
        .arg(status_label(status))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn deterministic daemon");
    DaemonGuard {
        child: Some(child),
        completed: false,
        stderr_path,
    }
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
                thread::yield_now();
            }
            Err(error) => return Err(error.into()),
        }
    };
    peer.send(&HarnessInputMessage::Hello(Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: "tau-e2e-daemon".into(),
        client_kind: ClientKind::Ui,
    }))?;
    let selectors = [
        EventName::AGENT_PROMPT_CREATED,
        EventName::PROVIDER_PROMPT_SUBMITTED,
        EventName::PROVIDER_RESPONSE_FINISHED,
        EventName::AGENT_PROMPT_TERMINATED,
        EventName::EXTENSION_EXITED,
        EventName::EXTENSION_RESTARTING,
        EventName::HARNESS_NOTICE,
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
            session_id: "deterministic-e2e-session".into(),
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
            session_id: "deterministic-e2e-session".into(),
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
            session_id: "deterministic-e2e-session".into(),
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
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match peer.recv_timeout(remaining)? {
            SocketReceive::Message {
                message: HarnessOutputMessage::Deliver(delivery),
            } => return Ok(delivery.into_event()),
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

pub(super) fn recv_until_cancel_ack_and_terminated(
    peer: &mut SocketPeer,
    selected: &tau_proto::AgentPromptCreated,
    active_before: &[&tau_proto::AgentPromptCreated],
) -> Result<tau_proto::AgentPromptTerminated, Box<dyn std::error::Error>> {
    let mut terminated = None;
    let mut acknowledged = false;
    loop {
        match recv_event(peer)? {
            Event::AgentPromptTerminated(value)
                if active_before.iter().any(|prompt| {
                    value.agent_prompt_id == prompt.agent_prompt_id
                        && value.agent_prompt_id != selected.agent_prompt_id
                }) =>
            {
                return Err("untargeted hold terminated before exact cancellation".into());
            }
            Event::AgentPromptTerminated(value)
                if value.agent_prompt_id == selected.agent_prompt_id =>
            {
                terminated = Some(value);
            }
            Event::HarnessNotice(notice) if notice.level == tau_proto::NoticeLevel::Trace => {
                let mut active = active_before
                    .iter()
                    .map(|prompt| prompt.agent_prompt_id.to_string())
                    .collect::<Vec<_>>();
                active.sort();
                let Some(payload) = notice
                    .message
                    .strip_prefix("e2e_fake_provider.cancel_completed ")
                else {
                    continue;
                };
                let payload = serde_json::from_str::<serde_json::Value>(payload)?;
                assert_eq!(
                    payload,
                    serde_json::json!({
                        "selected": selected.agent_prompt_id.to_string(),
                        "canceled_by": selected.agent_prompt_id.to_string(),
                        "active_before": active,
                    })
                );
                acknowledged = true;
            }
            Event::ProviderResponseFinished(value)
                if active_before.iter().any(|prompt| {
                    value.agent_prompt_id == prompt.agent_prompt_id
                        && value.agent_prompt_id != selected.agent_prompt_id
                }) =>
            {
                return Err("untargeted hold finished before exact cancellation".into());
            }
            _ => {}
        }
        if acknowledged && let Some(terminated) = terminated {
            return Ok(terminated);
        }
    }
}
