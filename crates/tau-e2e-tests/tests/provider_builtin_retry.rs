//! Exact production provider-builtin subprocess acceptance.
#![cfg(target_os = "linux")]

#[path = "provider_builtin_retry/daemon_guard.rs"]
mod daemon_guard;
#[path = "provider_builtin_retry/lifecycle.rs"]
mod lifecycle;

use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use daemon_guard::DaemonGuard;
use lifecycle::Lifecycle;
use nix::poll::{PollFd, PollFlags, poll};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
use tau_e2e_tests::{
    CapturedChatRequest, DurableSnapshot, PROVIDER_BUILTIN_SESSION, ProviderBuiltinFixture,
};
use tau_proto::{
    ClientKind, Event, EventName, EventSelector, HarnessInputMessage, HarnessOutputMessage, Hello,
    RetryPromptRequestId, RetryPromptStatus, Subscribe, UiCreateAgent, UiPromptSubmitted,
    UiRetryPrompt,
};
use tau_socket::{SocketPeer, SocketReceive};

const P1: &str = "first retry prompt";
const P2: &str = "later prompt";
/// Exact deterministic tool executable built for this test target.
const DUMMY_TOOL: &str = env!("CARGO_BIN_EXE_tau-e2e-test-dummy");
/// Outer deadlock guard for daemon startup and each causally signaled live
/// event.
const EVENT_WATCHDOG: Duration = Duration::from_secs(30);

/// Exercises a real 429 scheduler park and manual retry release through the
/// exact provider-builtin executable, preventing a retry from duplicating a
/// durable turn or leaving its shared cooldown stuck after a successful probe.
#[test]
fn provider_builtin_manual_retry_releases_parked_cooldown() -> Result<(), Box<dyn std::error::Error>>
{
    let Some(provider_bin) = provider_builtin_binary()? else {
        eprintln!(
            "skipping provider-builtin retry E2E: \
             set TAU_E2E_PROVIDER_BUILTIN_BIN to the exact candidate binary"
        );
        return Ok(());
    };
    let fixture = ProviderBuiltinFixture::new(
        "provider_builtin_manual_retry_releases_parked_cooldown",
        provider_bin,
    )?;
    let socket = fixture.socket_path();
    fixture.mark_daemon_started();
    let daemon = DaemonGuard::spawn(&fixture, &socket)?;
    let mut peer = connect_ui(&socket)?;
    let mut lifecycle = Lifecycle::default();

    create_p1(&mut peer)?;
    eprintln!("provider-builtin retry E2E: submitted P1; waiting for creation");
    let p1 = wait_for_created(&mut peer, &mut lifecycle, "p1")?;
    eprintln!("provider-builtin retry E2E: P1 created; waiting for throttle");
    let retry = wait_for_retry_update(&mut peer, &mut lifecycle, &p1.agent_prompt_id)?;
    assert!(
        retry.deltas.is_empty(),
        "retry update emitted semantic output"
    );
    let status = retry.status.as_ref().ok_or("retry update omitted status")?;
    assert!(
        status.clear_response,
        "retry update did not clear prior response"
    );
    let retry = status
        .retry
        .as_ref()
        .ok_or("retry update omitted retry facts")?;
    assert_eq!(
        retry.category,
        tau_proto::ProviderRetryCategory::Throttle,
        "unexpected retry category with status text {:?}",
        status.text
    );
    assert_eq!(retry.attempt, 1);
    assert!(
        86_400 <= retry.next_retry_delay_secs,
        "Retry-After hint was not retained: {}",
        retry.next_retry_delay_secs
    );
    eprintln!("provider-builtin retry E2E: throttle received; waiting for request 1");
    let request1 = match fixture.recv_request() {
        Ok(request) => request,
        Err(error) => {
            return Err(format!("{error}; daemon diagnostic: {}", daemon.diagnostic()?).into());
        }
    };
    assert_chat_request(&request1)?;
    assert_one_user_turn(&request1, P1)?;
    fixture.require_no_ready_request()?;
    lifecycle.require_parked(&p1.agent_prompt_id)?;
    require_no_terminal_ready(&mut peer, &mut lifecycle)?;
    eprintln!("provider-builtin retry E2E: P1 parked; requesting manual retry");

    peer.send(&HarnessInputMessage::emit(Event::UiRetryPrompt(
        UiRetryPrompt {
            request_id: RetryPromptRequestId::parse("provider-builtin-retry")?,
            session_id: session_id(),
            target_agent_id: Some(p1.agent_id.clone()),
            agent_prompt_id: None,
        },
    )))?;
    wait_for_retry_accepted(&mut peer, &mut lifecycle)?;
    fixture.release_accepted_retry()?;
    eprintln!("provider-builtin retry E2E: retry accepted; waiting for request 2");
    let request2 = fixture.recv_request()?;
    eprintln!("provider-builtin retry E2E: request 2 received; waiting for P1 terminal");
    assert_chat_request(&request2)?;
    assert_eq!(request1.body["model"], request2.body["model"]);
    assert_eq!(request1.body["messages"], request2.body["messages"]);
    assert_one_user_turn(&request2, P1)?;
    fixture.require_no_ready_request()?;

    wait_for_finished(
        &mut peer,
        &mut lifecycle,
        &p1.agent_prompt_id,
        "P1 complete",
    )?;
    lifecycle.require_finished(&p1.agent_prompt_id)?;
    eprintln!("provider-builtin retry E2E: P1 finished; submitting P2");

    submit_p2(&mut peer, &p1.agent_id)?;
    let p2 = wait_for_created(&mut peer, &mut lifecycle, "p2")?;
    eprintln!("provider-builtin retry E2E: P2 created; waiting for request 3");
    let request3 = fixture.recv_request()?;
    eprintln!("provider-builtin retry E2E: request 3 received; waiting for P2 terminal");
    assert_chat_request(&request3)?;
    assert_completed_p1_context(&request3)?;
    wait_for_finished(
        &mut peer,
        &mut lifecycle,
        &p2.agent_prompt_id,
        "P2 complete",
    )?;
    lifecycle.require_finished(&p2.agent_prompt_id)?;
    lifecycle.require_exact_totals(&p1.agent_prompt_id, &p2.agent_prompt_id)?;

    disconnect_ui(&mut peer)?;
    daemon.finish()?;
    let durable = DurableSnapshot::load(fixture.harness_state_dir(), &session_id())?;
    assert_durable_turns(&durable, &p1.agent_prompt_id, &p2.agent_prompt_id)?;
    fixture.finish()?;
    eprintln!("provider-builtin retry E2E: completed three-request script");
    Ok(())
}

/// Exercises Qwen's literal effort, reasoning stream, single and parallel tool
/// calls, raw argument replay, continuation, and usage-only terminal chunk
/// through the exact production provider executable.
#[test]
fn provider_builtin_qwen_text_tool_continuation_is_exact() -> Result<(), Box<dyn std::error::Error>>
{
    let Some(provider_bin) = provider_builtin_binary()? else {
        eprintln!(
            "skipping provider-builtin Qwen E2E: \
             set TAU_E2E_PROVIDER_BUILTIN_BIN to the exact candidate binary"
        );
        return Ok(());
    };
    let fixture = ProviderBuiltinFixture::new_qwen(
        "provider_builtin_qwen_text_tool_continuation_is_exact",
        provider_bin,
        DUMMY_TOOL,
    )?;
    let socket = fixture.socket_path();
    fixture.mark_daemon_started();
    let daemon = DaemonGuard::spawn(&fixture, &socket)?;
    let mut peer = connect_ui(&socket)?;
    let mut lifecycle = Lifecycle::default();

    create_qwen_prompt(&mut peer)?;
    let _prompt = wait_for_created(&mut peer, &mut lifecycle, "qwen")?;
    let request1 = fixture.recv_request()?;
    assert_qwen_common_request(&request1)?;
    assert_one_user_turn(&request1, "exercise qwen tools")?;

    let request2 = fixture.recv_request()?;
    assert_qwen_common_request(&request2)?;
    assert_qwen_round_two(&request1, &request2)?;

    let request3 = fixture.recv_request()?;
    assert_qwen_common_request(&request3)?;
    assert_qwen_round_three(&request1, &request3)?;
    wait_for_qwen_finished(&mut peer, &mut lifecycle)?;

    disconnect_ui(&mut peer)?;
    daemon.finish()?;
    fixture.finish()?;
    Ok(())
}

/// Receives the two tool terminals and final Qwen terminal for one logical
/// harness prompt, checking reasoning and visible output at each phase.
fn wait_for_qwen_finished(
    peer: &mut SocketPeer,
    lifecycle: &mut Lifecycle,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut terminals = 0;
    loop {
        let event = recv_live(peer)?;
        reject_terminated(&event)?;
        lifecycle.record(&event);
        let Event::ProviderResponseFinished(finished) = event else {
            continue;
        };
        terminals += 1;
        match terminals {
            1 => {
                assert_eq!(
                    finished.stop_reason,
                    tau_proto::ProviderStopReason::ToolCalls
                );
                assert!(contains_exact_assistant_text(
                    &finished.output_items,
                    "calling one\n"
                ));
                assert!(has_exact_reasoning_text(&finished.output_items, "plan one"));
            }
            2 => {
                assert_eq!(
                    finished.stop_reason,
                    tau_proto::ProviderStopReason::ToolCalls
                );
                assert!(has_exact_reasoning_text(
                    &finished.output_items,
                    "plan parallel"
                ));
            }
            3 => {
                assert_eq!(finished.stop_reason, tau_proto::ProviderStopReason::EndTurn);
                assert!(finished.error.is_none());
                assert!(contains_exact_assistant_text(
                    &finished.output_items,
                    "Qwen complete ✓"
                ));
                assert!(has_exact_reasoning_text(
                    &finished.output_items,
                    "final plan"
                ));
                let usage = finished
                    .usage
                    .as_ref()
                    .ok_or("Qwen terminal omitted usage")?;
                assert_eq!(usage.prompt_sent_tokens, 101);
                assert_eq!(usage.response_received_tokens, 17);
                return Ok(());
            }
            _ => return Err("Qwen fixture emitted more than three provider terminals".into()),
        }
    }
}

/// Finds one exact accumulated reasoning item.
fn has_exact_reasoning_text(items: &[tau_proto::ContextItem], expected: &str) -> bool {
    items.iter().any(
        |item| matches!(item, tau_proto::ContextItem::ReasoningText(text) if text.text == expected),
    )
}

/// Finds one exact assistant text item among reasoning and tool output.
fn contains_exact_assistant_text(items: &[tau_proto::ContextItem], expected: &str) -> bool {
    items.iter().any(|item| {
        matches!(
            item,
            tau_proto::ContextItem::Message(message)
                if message.role == tau_proto::ContextRole::Assistant
                    && matches!(
                        message.content.as_slice(),
                        [tau_proto::ContentPart::Text { text }] if text == expected
                    )
        )
    })
}

/// Sends the one Qwen compatibility prompt.
fn create_qwen_prompt(peer: &mut SocketPeer) -> Result<(), Box<dyn std::error::Error>> {
    peer.send(&HarnessInputMessage::emit(Event::UiCreateAgent(
        UiCreateAgent {
            request_id: "provider-builtin-qwen-create".to_owned(),
            literal: false,
            session_id: session_id(),
            role: "provider-builtin-qwen".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: Some("exercise qwen tools".to_owned()),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("qwen".to_owned()),
            parent_agent: None,
            ephemeral: false,
        },
    )))?;
    Ok(())
}

/// Checks every fixed Qwen request field owned by profile lowering.
fn assert_qwen_common_request(
    request: &CapturedChatRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(request.body["stream"], true);
    assert_eq!(request.body["model"], "Qwen/Qwen3.8-27B");
    assert_eq!(request.body["reasoning_effort"], "xhigh");
    assert_eq!(request.body["stream_options"]["include_usage"], true);
    assert_eq!(
        request.body["chat_template_kwargs"],
        serde_json::json!({
            "enable_thinking": true,
            "preserve_thinking": true
        })
    );
    assert_eq!(request.body["temperature"], 1.0);
    assert_eq!(request.body["top_p"], 0.95);
    assert_eq!(request.body["top_k"], 20);
    assert_eq!(request.body["min_p"], 0.0);
    assert_eq!(request.body["presence_penalty"], 0.0);
    assert_eq!(request.body["repetition_penalty"], 1.0);
    assert_eq!(
        request.body["tools"][0]["function"]["name"],
        "restart_test_dummy"
    );
    Ok(())
}

/// Requires the complete ordered first-tool continuation transcript.
fn assert_qwen_round_two(
    initial: &CapturedChatRequest,
    continuation: &CapturedChatRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let initial_messages = initial.body["messages"]
        .as_array()
        .ok_or("initial Qwen messages are not an array")?;
    assert_eq!(initial_messages.len(), 2);
    let expected = serde_json::json!([
        initial_messages[0].clone(),
        {"role": "user", "content": "<user>exercise qwen tools</user>"},
        {
            "role": "assistant",
            "content": "calling one\n",
            "reasoning_content": "plan one",
            "reasoning": "plan one",
            "tool_calls": [{
                "id": "qwen-call-1",
                "type": "function",
                "function": {
                    "name": "restart_test_dummy",
                    "arguments": " { } "
                }
            }]
        },
        {
            "role": "tool",
            "tool_call_id": "qwen-call-1",
            "content": "restart succeeded"
        }
    ]);
    assert_eq!(continuation.body["messages"], expected);
    Ok(())
}

/// Requires the complete ordered parallel-tool continuation transcript.
fn assert_qwen_round_three(
    initial: &CapturedChatRequest,
    continuation: &CapturedChatRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let initial_messages = initial.body["messages"]
        .as_array()
        .ok_or("initial Qwen messages are not an array")?;
    assert_eq!(initial_messages.len(), 2);
    let expected = serde_json::json!([
        initial_messages[0].clone(),
        {"role": "user", "content": "<user>exercise qwen tools</user>"},
        {
            "role": "assistant",
            "content": "calling one\n",
            "reasoning_content": "plan one",
            "reasoning": "plan one",
            "tool_calls": [{
                "id": "qwen-call-1",
                "type": "function",
                "function": {
                    "name": "restart_test_dummy",
                    "arguments": " { } "
                }
            }]
        },
        {
            "role": "tool",
            "tool_call_id": "qwen-call-1",
            "content": "restart succeeded"
        },
        {
            "role": "assistant",
            "content": null,
            "reasoning_content": "plan parallel",
            "reasoning": "plan parallel",
            "tool_calls": [
                {
                    "id": "qwen-call-2",
                    "type": "function",
                    "function": {
                        "name": "restart_test_dummy",
                        "arguments": "{}"
                    }
                },
                {
                    "id": "qwen-call-3",
                    "type": "function",
                    "function": {
                        "name": "restart_test_dummy",
                        "arguments": " {  } "
                    }
                }
            ]
        },
        {
            "role": "tool",
            "tool_call_id": "qwen-call-2",
            "content": "restart succeeded"
        },
        {
            "role": "tool",
            "tool_call_id": "qwen-call-3",
            "content": "restart succeeded"
        }
    ]);
    assert_eq!(continuation.body["messages"], expected);
    Ok(())
}

/// Returns the only supported exact provider-builtin test executable path.
fn provider_builtin_binary() -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    let Some(path) = std::env::var_os("TAU_E2E_PROVIDER_BUILTIN_BIN") else {
        return Ok(None);
    };
    let path = PathBuf::from(path);
    if !path.is_absolute() || !path.is_file() {
        return Err("TAU_E2E_PROVIDER_BUILTIN_BIN must name an absolute executable file".into());
    }
    Ok(Some(path))
}

/// Connects one observer UI and subscribes to the fixture's typed lifecycle
/// facts.
fn connect_ui(socket: &Path) -> Result<SocketPeer, Box<dyn std::error::Error>> {
    wait_for_socket(socket)?;
    let mut peer = SocketPeer::connect(socket)?;
    peer.send(&HarnessInputMessage::Hello(Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: tau_proto::ExtensionName::parse("provider-builtin-retry-e2e")?,
        client_kind: ClientKind::Ui,
        expected_session_id: Some(session_id()),
        capabilities: Default::default(),
    }))?;
    let selectors: Vec<_> = [
        EventName::AGENT_PROMPT_CREATED,
        EventName::PROVIDER_PROMPT_SUBMITTED,
        EventName::PROVIDER_RESPONSE_UPDATED,
        EventName::PROVIDER_RESPONSE_FINISHED,
        EventName::AGENT_PROMPT_TERMINATED,
        EventName::UI_RETRY_PROMPT_RESULT,
    ]
    .into_iter()
    .map(EventSelector::Exact)
    .collect();
    peer.send(&HarnessInputMessage::Subscribe(Subscribe {
        historical_selectors: selectors.clone(),
        live_selectors: selectors,
    }))?;
    Ok(peer)
}

/// Waits for the daemon's socket creation notification without polling.
fn wait_for_socket(socket: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let parent = socket.parent().ok_or("daemon socket has no parent")?;
    let filename = socket.file_name().ok_or("daemon socket has no filename")?;
    let inotify = Inotify::init(InitFlags::IN_CLOEXEC | InitFlags::IN_NONBLOCK)?;
    inotify.add_watch(
        parent,
        AddWatchFlags::IN_CREATE | AddWatchFlags::IN_MOVED_TO,
    )?;
    if socket.exists() {
        return Ok(());
    }
    let deadline = Instant::now() + EVENT_WATCHDOG;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_ms = u16::try_from(remaining.as_millis().min(u128::from(u16::MAX)))?;
        let mut descriptors = [PollFd::new(inotify.as_fd(), PollFlags::POLLIN)];
        if poll(&mut descriptors, timeout_ms)? == 0 {
            return Err("timed out waiting for daemon socket creation".into());
        }
        if inotify
            .read_events()?
            .iter()
            .any(|event| event.name.as_deref() == Some(filename))
        {
            return Ok(());
        }
    }
}

/// Sends the one initial durable user prompt.
fn create_p1(peer: &mut SocketPeer) -> Result<(), Box<dyn std::error::Error>> {
    peer.send(&HarnessInputMessage::emit(Event::UiCreateAgent(
        UiCreateAgent {
            request_id: "provider-builtin-retry-create".to_owned(),
            literal: false,
            session_id: session_id(),
            role: "provider-builtin-retry".to_owned(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: Some(P1.to_owned()),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("p1".to_owned()),
            parent_agent: None,
            ephemeral: false,
        },
    )))?;
    Ok(())
}

/// Sends P2 immediately after P1's accepted terminal.
fn submit_p2(
    peer: &mut SocketPeer,
    agent_id: &tau_proto::AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    peer.send(&HarnessInputMessage::emit(Event::UiPromptSubmitted(
        UiPromptSubmitted {
            literal: false,
            session_id: session_id(),
            text: P2.to_owned(),
            agent_id: agent_id.clone(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("p2".to_owned()),
        },
    )))?;
    Ok(())
}

/// Receives the exact created prompt for one fixture correlation id.
fn wait_for_created(
    peer: &mut SocketPeer,
    lifecycle: &mut Lifecycle,
    ctx_id: &str,
) -> Result<tau_proto::AgentPromptCreated, Box<dyn std::error::Error>> {
    loop {
        let event = recv_live(peer)?;
        eprintln!(
            "provider-builtin retry E2E: observed live {} while waiting for {ctx_id} creation",
            event.name()
        );
        reject_terminated(&event)?;
        lifecycle.record(&event);
        if let Event::AgentPromptCreated(created) = event
            && created.ctx_id.as_deref() == Some(ctx_id)
        {
            return Ok(created);
        }
    }
}

/// Receives the first typed retry update for P1.
fn wait_for_retry_update(
    peer: &mut SocketPeer,
    lifecycle: &mut Lifecycle,
    prompt_id: &tau_proto::AgentPromptId,
) -> Result<tau_proto::ProviderResponseUpdated, Box<dyn std::error::Error>> {
    loop {
        let event = recv_live(peer)?;
        reject_terminated(&event)?;
        lifecycle.record(&event);
        if let Event::ProviderResponseUpdated(update) = event
            && &update.agent_prompt_id == prompt_id
            && update
                .status
                .as_ref()
                .is_some_and(|status| status.retry.is_some())
        {
            return Ok(update);
        }
    }
}

/// Receives the harness-routed accepted result for the ordinary retry command.
fn wait_for_retry_accepted(
    peer: &mut SocketPeer,
    lifecycle: &mut Lifecycle,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let event = recv_live(peer)?;
        reject_terminated(&event)?;
        lifecycle.record(&event);
        if let Event::UiRetryPromptResult(result) = event
            && result.request_id.as_str() == "provider-builtin-retry"
        {
            if result.status != Some(RetryPromptStatus::Accepted) {
                return Err(format!("manual retry was not accepted: {result:?}").into());
            }
            return Ok(());
        }
    }
}

/// Receives one accepted canonical provider terminal with its expected text.
fn wait_for_finished(
    peer: &mut SocketPeer,
    lifecycle: &mut Lifecycle,
    prompt_id: &tau_proto::AgentPromptId,
    expected_text: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let event = recv_live(peer)?;
        reject_terminated(&event)?;
        lifecycle.record(&event);
        if let Event::ProviderResponseFinished(finished) = event
            && &finished.agent_prompt_id == prompt_id
        {
            if finished.stop_reason != tau_proto::ProviderStopReason::EndTurn
                || finished.error.is_some()
                || !has_exact_assistant_text(&finished.output_items, expected_text)
            {
                return Err(format!("unexpected provider terminal: {finished:?}").into());
            }
            return Ok(());
        }
    }
}

/// Rejects any canonical prompt termination because this acceptance has no
/// cancellation or failure-terminal branch.
fn reject_terminated(event: &Event) -> Result<(), Box<dyn std::error::Error>> {
    if let Event::AgentPromptTerminated(terminated) = event {
        return Err(format!("unexpected prompt termination: {terminated:?}").into());
    }
    Ok(())
}

/// Rejects a terminal already queued before the ordinary retry command can
/// causally release the parked prompt.
fn require_no_terminal_ready(
    peer: &mut SocketPeer,
    lifecycle: &mut Lifecycle,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        match peer.recv_timeout(Duration::ZERO)? {
            SocketReceive::Message {
                message: HarnessOutputMessage::Deliver(delivery),
            } => {
                let (event, replay, _) = delivery.into_parts();
                if replay {
                    return Err(
                        format!("pre-retry lifecycle event arrived as replay: {event:?}").into(),
                    );
                }
                reject_terminated(&event)?;
                if matches!(event, Event::ProviderResponseFinished(_)) {
                    return Err(
                        format!("unexpected terminal before manual retry: {event:?}").into(),
                    );
                }
                lifecycle.record(&event);
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
            SocketReceive::Timeout => return Ok(()),
            SocketReceive::Closed => return Err("daemon socket closed".into()),
        }
    }
}

/// Receives one live event through the test's only socket watchdog.
fn recv_live(peer: &mut SocketPeer) -> Result<Event, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + EVENT_WATCHDOG;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match peer.recv_timeout(remaining)? {
            SocketReceive::Message {
                message: HarnessOutputMessage::Deliver(delivery),
            } => {
                let (event, replay, _) = delivery.into_parts();
                if replay {
                    return Err(
                        format!("retry lifecycle event arrived as replay: {event:?}").into(),
                    );
                }
                return Ok(event);
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
            SocketReceive::Timeout => return Err("timed out waiting for live retry event".into()),
            SocketReceive::Closed => return Err("daemon socket closed".into()),
        }
    }
}

/// Sends the expected clean UI disconnect.
fn disconnect_ui(peer: &mut SocketPeer) -> Result<(), Box<dyn std::error::Error>> {
    peer.send(&HarnessInputMessage::Disconnect(tau_proto::Disconnect {
        reason: Some("provider-builtin retry test complete".to_owned()),
    }))?;
    Ok(())
}

/// Requires the exact Chat Completions route, model, and one user input.
fn assert_chat_request(request: &CapturedChatRequest) -> Result<(), Box<dyn std::error::Error>> {
    if request.method != "POST"
        || request.path != "/v1/chat/completions"
        || request.body["model"] != "retry-model"
        || request.body["stream"] != true
    {
        return Err(format!("unexpected Chat Completions request: {request:?}").into());
    }
    Ok(())
}

/// Requires one and only one upstream user message with exact content.
fn assert_one_user_turn(
    request: &CapturedChatRequest,
    expected_prompt: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let messages = request.body["messages"]
        .as_array()
        .ok_or("Chat Completions request omitted messages")?;
    let users = messages
        .iter()
        .filter(|message| message["role"] == "user")
        .collect::<Vec<_>>();
    let expected_content = format!("<user>{expected_prompt}</user>");
    if users.len() != 1 || users[0]["content"] != expected_content {
        return Err(format!("unexpected upstream user messages: {users:?}").into());
    }
    Ok(())
}

/// Requires P2 to carry the exact completed P1 context once and in order.
fn assert_completed_p1_context(
    request: &CapturedChatRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let messages = request.body["messages"]
        .as_array()
        .ok_or("Chat Completions request omitted messages")?;
    let conversational = messages
        .iter()
        .filter(|message| message["role"] == "user" || message["role"] == "assistant")
        .map(|message| Some((message["role"].as_str()?, message["content"].as_str()?)))
        .collect::<Option<Vec<_>>>()
        .ok_or("Chat Completions conversational message was not text")?;
    let expected = [
        ("user", "<user>first retry prompt</user>"),
        ("assistant", "P1 complete"),
        ("user", "<user>later prompt</user>"),
    ];
    if conversational != expected {
        return Err(
            format!("unexpected upstream conversational context: {conversational:?}").into(),
        );
    }
    let users = conversational
        .iter()
        .filter(|(role, _)| *role == "user")
        .map(|(_, content)| *content)
        .collect::<Vec<_>>();
    if users
        != [
            "<user>first retry prompt</user>",
            "<user>later prompt</user>",
        ]
    {
        return Err(format!("duplicate upstream user context: {users:?}").into());
    }
    Ok(())
}

/// Validates durable canonical turn ownership after graceful teardown.
fn assert_durable_turns(
    durable: &DurableSnapshot,
    p1: &tau_proto::AgentPromptId,
    p2: &tau_proto::AgentPromptId,
) -> Result<(), Box<dyn std::error::Error>> {
    let finished = durable
        .agent_events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ProviderResponseFinished(value) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    let durable_prompts = durable
        .agent_events
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentPromptSubmitted(value) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    let retry_updates = durable
        .agent_events
        .iter()
        .filter(|record| matches!(record.event, Event::ProviderResponseUpdated(_)))
        .count();
    let terminated = durable
        .agent_events
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentPromptTerminated(value) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    if durable_prompts.len() != 2
        || durable_prompts[0].text != P1
        || durable_prompts[1].text != P2
        || finished.len() != 2
        || finished[0].agent_prompt_id != *p1
        || finished[1].agent_prompt_id != *p2
        || !has_exact_assistant_text(&finished[0].output_items, "P1 complete")
        || !has_exact_assistant_text(&finished[1].output_items, "P2 complete")
        || retry_updates != 0
        || !terminated.is_empty()
    {
        return Err(format!(
            "unexpected durable retry journal: prompts={durable_prompts:?}, finished={finished:?}, \
             retry_updates={retry_updates}, terminated={terminated:?}"
        )
        .into());
    }
    Ok(())
}

/// Requires the complete terminal output to be one exact assistant text
/// message.
fn has_exact_assistant_text(items: &[tau_proto::ContextItem], expected_text: &str) -> bool {
    matches!(
        items,
        [tau_proto::ContextItem::Message(message)]
            if matches!(
                message.content.as_slice(),
                [tau_proto::ContentPart::Text { text }] if text == expected_text
            )
                && message.role == tau_proto::ContextRole::Assistant
                && message.phase.is_none()
                && message.responses_raw_json.is_none()
    )
}

/// Returns the fixture's fixed durable session identity.
fn session_id() -> tau_proto::SessionId {
    PROVIDER_BUILTIN_SESSION
        .parse()
        .expect("known fixture session id is valid")
}
