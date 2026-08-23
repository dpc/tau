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
    let sender_session = SessionId::parse(format!("peer-sender-{}", std::process::id()))
        .expect("known-safe SessionId must be valid");
    let sender_id = AgentId::parse("peer-sender")?;
    let message = "peer navigation activation".to_owned();
    let model_input = format!(
        "<tau_internal>Authenticated peer message\n\n\
         <tau_peer_message sender_session=\"{sender_session}\" sender_agent=\"{sender_id}\">\n\
         {message}\n\
         </tau_peer_message></tau_internal>"
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
        message_id: tau_proto::AgentMessageId::parse("peer-navigation-message")
            .expect("test identifier must satisfy its grammar"),
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
        client_name: tau_proto::ExtensionName::parse(CALLBACK_CLIENT_NAME)
            .expect("callback client name must satisfy the identifier grammar"),
        client_kind: tau_proto::ClientKind::External,
        expected_session_id: None,
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
    if let Some(failure) = result.failure {
        return Err(format!("external message failed: {failure:?}").into());
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
    wait_for_idle_active(&mut observer, &agent_id, &prompt, deadline)?;
    assert_exact_canceled_hold_facts(&observer.events, &prompt)?;

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

/// Waits for prompt creation, submission, active/running stats, and the
/// later prompt-correlated hold-ready notice in that order.
pub(super) fn wait_for_live_hold(
    observer: &mut SideObserver,
    agent_id: &AgentId,
    deadline: Instant,
) -> Result<AgentPromptCreated, Box<dyn std::error::Error>> {
    wait_for_live_hold_with_navigation(observer, agent_id, deadline, AgentNavigationMode::Active)
}

/// Waits for one correlated running provider hold while accepting the ordinary
/// active navigation mode used by a directly selected terminal agent.
pub(super) fn wait_for_selected_live_hold(
    observer: &mut SideObserver,
    agent_id: &AgentId,
    deadline: Instant,
) -> Result<AgentPromptCreated, Box<dyn std::error::Error>> {
    wait_for_live_hold_with_navigation(observer, agent_id, deadline, AgentNavigationMode::Active)
}

fn wait_for_live_hold_with_navigation(
    observer: &mut SideObserver,
    agent_id: &AgentId,
    deadline: Instant,
    expected_navigation: AgentNavigationMode,
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
                        && stats.navigation_mode == expected_navigation
                        && stats.runtime_state == AgentRuntimeState::Running
            )
        })
        .ok_or("hold-ready notice arrived before the required running snapshot")?;
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
        return Err(format!(
            "hold-ready notice was not ordered after {expected_navigation:?}/running"
        )
        .into());
    }
    if observer.events.iter().any(hold_ended) {
        return Err("provider hold ended before terminal selection".into());
    }
    Ok(prompt)
}

/// Builds the exact visible fake-provider readiness notice for one prompt.
pub(super) fn hold_ready_notice(prompt: &AgentPromptCreated) -> String {
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
                && notice.purpose == tau_proto::NoticePurpose::Diagnostic
    )
}

/// Requires one ready trace and no timeout or cancellation trace for a prompt.
pub(super) fn assert_hold_live(
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

/// Requires one ready and one canceled trace with no timeout for a prompt.
pub(super) fn assert_hold_reaped(
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

/// Waits for the prompt-correlated canceled terminal and provider
/// acknowledgement; exact lifecycle counts are checked after idle.
pub(super) fn wait_for_canceled_hold(
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
    Ok(())
}

/// Requires the complete side-observer lifecycle to contain exactly one fact
/// at each stage for the correlated canceled prompt.
pub(super) fn assert_exact_canceled_hold_facts(
    events: &[super::observer::ObservedEvent],
    prompt: &AgentPromptCreated,
) -> Result<(), Box<dyn std::error::Error>> {
    let created = events
        .iter()
        .filter(|observed| matches!(&observed.event, Event::AgentPromptCreated(value) if value.agent_prompt_id == prompt.agent_prompt_id))
        .count();
    let submitted = events
        .iter()
        .filter(|observed| matches!(&observed.event, Event::ProviderPromptSubmitted(value) if value.agent_prompt_id == prompt.agent_prompt_id))
        .count();
    let terminated = events
        .iter()
        .filter(|observed| matches!(&observed.event, Event::AgentPromptTerminated(value) if value.agent_prompt_id == prompt.agent_prompt_id && value.reason == AgentPromptTerminationReason::Canceled))
        .count();
    let cancellation_notices = events
        .iter()
        .filter_map(cancel_completed_payload)
        .collect::<Result<Vec<_>, _>>()?;
    let timeouts = events
        .iter()
        .filter(|observed| matches!(&observed.event, Event::HarnessNotice(notice) if notice.message.starts_with("e2e_fake_provider.hold_timeout ")))
        .count();
    if created != 1
        || submitted != 1
        || terminated != 1
        || cancellation_notices.len() != 1
        || timeouts != 0
    {
        return Err(format!(
            "unexpected hold facts: created={created}, submitted={submitted}, terminated={terminated}, cancel_completed={}, hold_timeout={timeouts}",
            cancellation_notices.len()
        )
        .into());
    }
    let cancellation = &cancellation_notices[0];
    if cancellation.selected != prompt.agent_prompt_id
        || cancellation.canceled_by != prompt.agent_prompt_id
        || cancellation.active_before.as_slice() != [prompt.agent_prompt_id.clone()]
    {
        return Err(format!(
            "provider cancellation stats did not identify one exact active prompt: {cancellation:?}"
        )
        .into());
    }
    Ok(())
}

/// Waits for an idle active snapshot ordered after prompt termination.
pub(super) fn wait_for_idle_active(
    observer: &mut SideObserver,
    agent_id: &AgentId,
    prompt: &AgentPromptCreated,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    wait_for_idle_with_navigation(
        observer,
        agent_id,
        prompt,
        deadline,
        AgentNavigationMode::Active,
    )
}

/// Waits for the selected terminal agent's correlated post-cancel idle state.
pub(super) fn wait_for_selected_idle(
    observer: &mut SideObserver,
    agent_id: &AgentId,
    prompt: &AgentPromptCreated,
    deadline: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    wait_for_idle_with_navigation(
        observer,
        agent_id,
        prompt,
        deadline,
        AgentNavigationMode::Active,
    )
}

fn wait_for_idle_with_navigation(
    observer: &mut SideObserver,
    agent_id: &AgentId,
    prompt: &AgentPromptCreated,
    deadline: Instant,
    expected_navigation: AgentNavigationMode,
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
                    && stats.navigation_mode == expected_navigation
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
                    && stats.navigation_mode == expected_navigation
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

fn cancel_completed_payload(
    observed: &super::observer::ObservedEvent,
) -> Option<Result<CancelAcknowledgement, serde_json::Error>> {
    let Event::HarnessNotice(notice) = &observed.event else {
        return None;
    };
    notice
        .message
        .strip_prefix("e2e_fake_provider.cancel_completed ")
        .map(serde_json::from_str)
}

/// Typed fake-provider snapshot emitted after one cancellation request.
#[derive(Debug, serde::Deserialize)]
struct CancelAcknowledgement {
    /// Prompt selected by the cancellation request.
    selected: tau_proto::AgentPromptId,
    /// Prompt whose hold worker acknowledged cancellation.
    canceled_by: tau_proto::AgentPromptId,
    /// Active prompt set immediately before provider removal.
    active_before: Vec<tau_proto::AgentPromptId>,
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
