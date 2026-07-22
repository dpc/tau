//! Fresh-session peer auto-start and real-terminal navigation coverage.

use std::path::Path;
use std::time::Instant;

use tau_e2e_tests::{ScenarioActionV2, ScenarioLaneV2, ScenarioV2};
use tau_proto::{
    AgentId, AgentNavigationMode, AgentPromptCreated, AgentPromptTerminationReason,
    AgentRuntimeState, Event, HarnessInputMessage, HarnessOutputMessage, SessionId,
};

#[path = "peer_navigation/fake_external_sender.rs"]
mod fake_external_sender;

use fake_external_sender::FakeExternalSender;

use super::gate_fixture::GateFixture;
use super::observer::{SideObserver, discover_daemon};
use super::pty_process::{PtyArtifacts, PtyProcess};
use super::{DEADLINE, FAKE_PROVIDER};

const REQUEST_ID: &str = "peer-navigation-request";
const AUTH_REQUEST_ID: &str = "auth-peer-navigation-request";
const CALLBACK_CLIENT_NAME: &str = "tau-external-agent-message";

/// Proves an authenticated external message can auto-start the first agent in
/// an otherwise empty session and Ctrl-J can select it while its turn is live.
#[test]
fn external_message_first_agent_is_immediately_navigable() -> Result<(), Box<dyn std::error::Error>>
{
    let sender_session = SessionId::from(format!("peer-sender-{}", std::process::id()));
    let sender_id = AgentId::parse("peer-sender")?;
    let message = "peer navigation activation".to_owned();
    let model_input = format!(
        "[tau-internal]: You have received a message from {sender_session}/{sender_id}\n\n\
         <message>\n{message}\n</message>"
    );
    let scenario = ScenarioV2::new(
        "external-message-first-agent-navigation",
        vec![ScenarioLaneV2 {
            ctx_id: "peer-entrypoint".to_owned(),
            actions: vec![ScenarioActionV2::HoldUntilCancel {
                user_text: model_input,
                timeout_ms: 10_000,
            }],
        }],
    );
    let fixture = GateFixture::new_peer_entrypoint(&scenario, Path::new(FAKE_PROVIDER))?;
    let mut target = PtyProcess::spawn(
        fixture.command(None),
        false,
        Some(PtyArtifacts::new(
            fixture.artifact_path("peer-target-pty.raw.bounded"),
            fixture.artifact_path("peer-target-pty.normalized.txt"),
        )),
    )?;
    let deadline = Instant::now() + DEADLINE;
    let (target_socket, target_session) = discover_daemon(fixture.runtime_home(), None, deadline)?;
    let mut observer = SideObserver::connect(
        &target_socket,
        &target_session,
        fixture.artifact_path("peer-target-observer.json"),
        deadline,
    )?;
    observer.wait_for_extension("e2e-fake-provider", deadline)?;
    assert!(
        !observer
            .events
            .iter()
            .any(|observed| matches!(observed.event, Event::AgentStarted(_))),
        "target session must be agentless before the peer message"
    );
    target.wait_for(
        "Write a message to start a new deterministic-peer agent...",
        deadline,
    )?;

    let request = tau_proto::ExternalAgentMessageRequest {
        request_id: REQUEST_ID.to_owned(),
        message_id: "peer-navigation-message".into(),
        capability: "peer-navigation-capability".to_owned(),
        sender_session_id: sender_session.clone(),
        sender_id: sender_id.clone(),
        recipient_session_id: target_session.clone(),
        recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
        kind: tau_proto::AgentMessageKind::Message,
        message,
    };
    let mut sender =
        FakeExternalSender::start(fixture.runtime_home(), &sender_session, request.clone())?;
    let mut peer = tau_socket::SocketPeer::connect(&target_socket)?;
    peer.send(&HarnessInputMessage::Hello(tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: CALLBACK_CLIENT_NAME.into(),
        client_kind: tau_proto::ClientKind::External,
        capabilities: Default::default(),
    }))?;
    peer.send(&HarnessInputMessage::ExternalAgentMessage(request))?;
    sender.authorize(deadline)?;
    let result = recv_external_message_result(&mut peer, deadline)?;
    if result.request_id != REQUEST_ID {
        return Err(format!(
            "external message result request id was `{}`, expected `{REQUEST_ID}`",
            result.request_id
        )
        .into());
    }
    if let Some(error) = result.error {
        return Err(format!("external message failed: {error}").into());
    }
    if !result.started {
        return Err("external message did not auto-start its bare recipient".into());
    }
    let agent_id = result.recipient_id.ok_or("missing started recipient")?;

    let prompt = wait_for_live_hold(&mut observer, &agent_id, deadline)?;
    // The provider emits this marker only after the hold is ready. The harness
    // broadcasts it after the already-queued Running update, so rendering it
    // proves the target UI consumed that cache update on its own stream.
    target.wait_for(&hold_ready_notice(&prompt), deadline)?;
    assert_hold_live(&fixture, &prompt)?;
    target.send_next_agent_key()?;
    target.wait_ready_for(agent_id.as_str(), deadline)?;

    observer.cancel_prompt(&target_session, &prompt)?;
    wait_for_canceled_hold(&mut observer, &prompt, deadline)?;
    assert_hold_reaped(&fixture, &prompt)?;
    wait_for_idle_auto(&mut observer, &agent_id, &prompt, deadline)?;

    fixture.write_artifact(
        "peer-target-observer.json",
        &serde_json::to_vec_pretty(&observer.events)?,
    )?;
    drop(peer);
    drop(observer);
    sender.finish()?;
    target.finish()?;
    fixture.require_boot_gone(target_session.as_str())?;
    fixture.complete();
    Ok(())
}

fn wait_for_live_hold(
    observer: &mut SideObserver,
    agent_id: &AgentId,
    deadline: Instant,
) -> Result<AgentPromptCreated, Box<dyn std::error::Error>> {
    let created = observer.recv_until(deadline, |observed| {
        matches!(
            &observed.event,
            Event::AgentPromptCreated(created) if &created.agent_id == agent_id
        )
    })?;
    let Event::AgentPromptCreated(prompt) = created.event else {
        unreachable!("predicate admitted another event");
    };
    observer.recv_until(deadline, |observed| {
        matches!(
            &observed.event,
            Event::ProviderPromptSubmitted(submitted)
                if submitted.agent_prompt_id == prompt.agent_prompt_id
        )
    })?;
    let hold_ready = hold_ready_notice(&prompt);
    observer.recv_until(deadline, |observed| {
        is_exact_visible_hold_ready(observed, &hold_ready)
    })?;
    let running_index = observer
        .events
        .iter()
        .position(|observed| {
            matches!(
                &observed.event,
                Event::AgentStatsUpdated(stats)
                    if &stats.agent_id == agent_id
                        && stats.navigation_mode == AgentNavigationMode::ActiveAuto
                        && stats.runtime_state == AgentRuntimeState::Running
            )
        })
        .ok_or("hold-ready notice arrived before an active-auto/running snapshot")?;
    let hold_ready_indices = observer
        .events
        .iter()
        .enumerate()
        .filter_map(|(index, observed)| {
            is_exact_visible_hold_ready(observed, &hold_ready).then_some(index)
        })
        .collect::<Vec<_>>();
    if hold_ready_indices.len() != 1 {
        return Err("provider did not emit exactly one correlated hold-ready fact".into());
    }
    if running_index >= hold_ready_indices[0] {
        return Err("hold-ready notice was not ordered after active-auto/running".into());
    }
    if observer.events.iter().any(hold_ended) {
        return Err("provider hold ended before terminal selection".into());
    }
    Ok(prompt)
}

fn hold_ready_notice(prompt: &AgentPromptCreated) -> String {
    format!(
        "e2e_fake_provider.hold_ready {{\"prompt_id\":\"{}\"}}",
        prompt.agent_prompt_id
    )
}

fn is_exact_visible_hold_ready(
    observed: &super::observer::ObservedEvent,
    expected_message: &str,
) -> bool {
    matches!(
        &observed.event,
        Event::HarnessNotice(notice)
            if notice.kind == tau_proto::notice_kind::EXTENSION_NOTICE
                && notice.message == expected_message
                && notice.level == tau_proto::NoticeLevel::Info
                && !notice.always_show
    )
}

fn assert_hold_live(
    fixture: &GateFixture,
    prompt: &AgentPromptCreated,
) -> Result<(), Box<dyn std::error::Error>> {
    assert_hold_trace_live(&fixture.trace()?, prompt)
}

fn assert_hold_trace_live(
    trace: &str,
    prompt: &AgentPromptCreated,
) -> Result<(), Box<dyn std::error::Error>> {
    let ready = format!("prompt_id={} hold_ready", prompt.agent_prompt_id);
    let timeout = format!("prompt_id={} hold_timeout", prompt.agent_prompt_id);
    let canceled = format!("prompt_id={} hold_canceled", prompt.agent_prompt_id);
    if trace.lines().filter(|line| *line == ready).count() != 1
        || trace.contains(&timeout)
        || trace.contains(&canceled)
    {
        return Err(format!("provider hold was not live at terminal selection: {trace}").into());
    }
    Ok(())
}

fn assert_hold_reaped(
    fixture: &GateFixture,
    prompt: &AgentPromptCreated,
) -> Result<(), Box<dyn std::error::Error>> {
    let trace = fixture.trace()?;
    let ready = format!("prompt_id={} hold_ready", prompt.agent_prompt_id);
    let canceled = format!("prompt_id={} hold_canceled", prompt.agent_prompt_id);
    let timeout = format!("prompt_id={} hold_timeout", prompt.agent_prompt_id);
    if trace.lines().filter(|line| *line == ready).count() != 1
        || trace
            .lines()
            .filter(|line| line.starts_with(&canceled))
            .count()
            != 1
        || trace.contains(&timeout)
    {
        return Err(format!("provider hold did not cancel and reap exactly once: {trace}").into());
    }
    Ok(())
}

fn wait_for_canceled_hold(
    observer: &mut SideObserver,
    prompt: &AgentPromptCreated,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    observer.recv_until(deadline, |observed| {
        matches!(
            &observed.event,
            Event::AgentPromptTerminated(terminated)
                if terminated.agent_prompt_id == prompt.agent_prompt_id
                    && terminated.reason == AgentPromptTerminationReason::Canceled
        )
    })?;
    if !observer.events.iter().any(cancel_completed) {
        observer.recv_until(deadline, cancel_completed)?;
    }
    let cancellations = observer
        .events
        .iter()
        .filter(|observed| cancel_completed(observed))
        .count();
    let timeouts = observer
        .events
        .iter()
        .filter(|observed| {
            matches!(
                &observed.event,
                Event::HarnessNotice(notice)
                    if notice.message.starts_with("e2e_fake_provider.hold_timeout ")
            )
        })
        .count();
    if cancellations != 1 || timeouts != 0 {
        return Err(format!(
            "unexpected hold terminal facts: cancel_completed={cancellations}, hold_timeout={timeouts}"
        )
        .into());
    }
    Ok(())
}

fn wait_for_idle_auto(
    observer: &mut SideObserver,
    agent_id: &AgentId,
    prompt: &AgentPromptCreated,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let terminated = observer
        .events
        .iter()
        .rposition(|observed| {
            matches!(
                &observed.event,
                Event::AgentPromptTerminated(value)
                    if value.agent_prompt_id == prompt.agent_prompt_id
            )
        })
        .ok_or("canceled prompt termination was not retained")?;
    if observer.events[terminated + 1..].iter().any(|observed| {
        matches!(
            &observed.event,
            Event::AgentStatsUpdated(stats)
                if &stats.agent_id == agent_id
                    && stats.navigation_mode == AgentNavigationMode::ActiveAuto
                    && stats.runtime_state == AgentRuntimeState::Idle
        )
    }) {
        return Ok(());
    }
    observer.recv_until(deadline, |observed| {
        matches!(
            &observed.event,
            Event::AgentStatsUpdated(stats)
                if &stats.agent_id == agent_id
                    && stats.navigation_mode == AgentNavigationMode::ActiveAuto
                    && stats.runtime_state == AgentRuntimeState::Idle
        )
    })?;
    Ok(())
}

fn hold_ended(observed: &super::observer::ObservedEvent) -> bool {
    matches!(
        &observed.event,
        Event::HarnessNotice(notice)
            if notice.message.starts_with("e2e_fake_provider.cancel_completed ")
                || notice.message.starts_with("e2e_fake_provider.hold_timeout ")
    )
}

fn cancel_completed(observed: &super::observer::ObservedEvent) -> bool {
    matches!(
        &observed.event,
        Event::HarnessNotice(notice)
            if notice.message.starts_with("e2e_fake_provider.cancel_completed ")
    )
}

/// Receives the exact external-message RPC result before the shared deadline.
fn recv_external_message_result(
    peer: &mut tau_socket::SocketPeer,
    deadline: Instant,
) -> Result<tau_proto::ExternalAgentMessageResult, Box<dyn std::error::Error>> {
    loop {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err("external message result timed out".into());
        }
        match peer.recv_timeout(timeout)? {
            tau_socket::SocketReceive::Message {
                message: HarnessOutputMessage::ExternalAgentMessageResult(result),
            } => return Ok(result),
            tau_socket::SocketReceive::Message { .. } => {}
            tau_socket::SocketReceive::Timeout => {
                return Err("external message result timed out".into());
            }
            tau_socket::SocketReceive::Closed => {
                return Err("target closed before external message result".into());
            }
        }
    }
}
