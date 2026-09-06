use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use tau_e2e_tests::{
    DeterministicFixture, DurableSnapshot, InitialStatusOutcome, ScenarioActionV2, ScenarioLaneV2,
    ScenarioV1, ScenarioV2, StatusTerminalPhase, StatusToolOrder,
};
use tau_proto::{
    AgentId, AgentRuntimeState, CborValue, ClientKind, ContextItem, Event, EventName,
    EventSelector, HarnessInputMessage, Hello, ImageDetail, ImageMediaType, ProtocolVersion,
    ProviderFailureKind, Subscribe, ToolResultContentPart, ToolResultKind,
};
use tau_socket::SocketPeer;

#[path = "deterministic_provider/daemon_support.rs"]
mod daemon_support;
#[path = "deterministic_provider/persistence_barrier.rs"]
mod persistence_barrier;
#[path = "deterministic_provider/session_restore.rs"]
mod session_restore;
#[path = "deterministic_provider/standalone_compaction.rs"]
mod standalone_compaction;

use daemon_support::*;

const FAKE_PROVIDER: &str = env!("CARGO_BIN_EXE_tau-e2e-fake-provider");
const DUMMY_TOOL: &str = env!("CARGO_BIN_EXE_tau-e2e-test-dummy");
const HARNESS_DAEMON: &str = env!("CARGO_BIN_EXE_tau-e2e-harness-daemon");
const E2E_CLI: &str = env!("CARGO_BIN_EXE_tau-e2e-cli");
const SHELL_PROBE: &str = env!("CARGO_BIN_EXE_tau-e2e-shell-probe");
const RESTORE_NOTICE: &str = concat!(
    "Previous session was interrupted and restored. Less than 1 minute has passed ",
    "since the last recorded session event, and the state of the world might have changed. ",
    "Session-scoped tool and extension state may also have changed; inspect current tool state ",
    "and recreate timers or other session-scoped setup if still needed."
);

/// A real daemon socket admits a deliberately newer minor peer, exposes exactly
/// one replayable warning, and preserves ordinary operation and cleanup.
#[test]
fn deterministic_minor_protocol_skew_warns_and_continues() -> Result<(), Box<dyn std::error::Error>>
{
    const PROMPT: &str = "prove minor protocol skew continues";
    const RESPONSE: &str = "minor skew peer remained operational";
    let scenario = ScenarioV2::new(
        "protocol-minor-skew",
        vec![ScenarioLaneV2 {
            ctx_id: "protocol-minor-skew".to_owned(),
            actions: vec![ScenarioActionV2::Text {
                user_text: PROMPT.to_owned(),
                response: RESPONSE.to_owned(),
            }],
        }],
    );
    let fixture = DeterministicFixture::new_v2(
        "deterministic_minor_protocol_skew_warns_and_continues",
        &scenario,
        FAKE_PROVIDER,
    )?;
    let socket = fixture.socket_path("protocol-minor-skew");
    let daemon = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::New);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut peer = loop {
        match SocketPeer::connect(&socket) {
            Ok(peer) => break peer,
            Err(_) if Instant::now() < deadline => std::thread::yield_now(),
            Err(error) => return Err(error.into()),
        }
    };
    let peer_version = ProtocolVersion::new(
        tau_proto::PROTOCOL_VERSION.major,
        tau_proto::PROTOCOL_VERSION.minor + 1,
    );
    peer.send(&HarnessInputMessage::Hello(Hello {
        protocol_version: peer_version,
        client_name: "tau-e2e-minor-skew".parse()?,
        client_kind: ClientKind::Ui,
        expected_session_id: None,
        capabilities: Vec::new(),
    }))?;
    peer.send(&HarnessInputMessage::Subscribe(Subscribe {
        historical_selectors: vec![
            EventSelector::Exact(EventName::HARNESS_NOTICE),
            EventSelector::Exact(EventName::PROVIDER_RESPONSE_FINISHED),
        ],
        live_selectors: vec![
            EventSelector::Exact(EventName::HARNESS_NOTICE),
            EventSelector::Exact(EventName::PROVIDER_RESPONSE_FINISHED),
        ],
    }))?;

    let observed = loop {
        let observed = recv_observed(&mut peer)?;
        if matches!(
            &observed.event,
            Event::HarnessNotice(notice)
                if notice.message.contains("protocol version skew for peer")
        ) {
            break observed;
        }
    };
    let Event::HarnessNotice(notice) = &observed.event else {
        unreachable!("loop exits only for a harness notice");
    };
    assert!(notice.message.contains("tau-e2e-minor-skew"));
    assert!(notice.message.contains(&peer_version.to_string()));
    assert!(
        notice
            .message
            .contains(&tau_proto::PROTOCOL_VERSION.to_string())
    );
    assert!(notice.message.contains("continuing best-effort"));
    assert!(
        observed.replay,
        "late subscription must replay the live alert"
    );
    create_agent(&mut peer, "protocol-minor-skew", PROMPT)?;
    loop {
        if matches!(
            recv_event(&mut peer)?,
            Event::ProviderResponseFinished(finished)
                if finished.output_items.iter().any(|item| {
                    matches!(
                        item,
                        ContextItem::Message(message)
                            if message.content.iter().any(|part| {
                                matches!(
                                    part,
                                    tau_proto::ContentPart::Text { text } if text == RESPONSE
                                )
                            })
                    )
                })
        ) {
            break;
        }
    }
    disconnect_ui(&mut peer)?;
    drop(peer);
    daemon.finish()?;
    fixture.assert_consumed()?;
    Ok(())
}

/// The exact incident command unloads an idle saved agent through the
/// deterministic daemon.
#[test]
fn deterministic_exact_saved_agent_unload_command() -> Result<(), Box<dyn std::error::Error>> {
    let scenario = ScenarioV2::new(
        "cli-unload",
        vec![ScenarioLaneV2 {
            ctx_id: "seed".to_owned(),
            actions: vec![ScenarioActionV2::Text {
                user_text: "seed idle agent".to_owned(),
                response: "idle".to_owned(),
            }],
        }],
    );
    let fixture = DeterministicFixture::new_v2(
        "deterministic_exact_saved_agent_unload_command",
        &scenario,
        FAKE_PROVIDER,
    )?;
    let config_path = fixture.config_dir().join("harness.yaml");
    let mut config: serde_json::Value = serde_json::from_slice(&std::fs::read(&config_path)?)?;
    config["agents"]["id_template"] = serde_json::json!("zulip-bot-ngMK");
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;
    let socket = fixture
        .runtime_dir()
        .join("tau/harnesses/sockets")
        .join(blake3::hash(b"tau-zulip-bot").to_hex().to_string())
        .with_extension("sock");
    let daemon = spawn_daemon_for_cli_unload(&fixture, &socket);
    let mut ui = connect_ui(&socket)?;
    create_agent_in_session(&mut ui, "tau-zulip-bot", "seed", "seed idle agent")?;
    loop {
        if matches!(
            recv_event(&mut ui)?,
            Event::ProviderResponseFinished(finished)
                if finished.agent_id.as_str() == "zulip-bot-ngMK"
        ) {
            break;
        }
    }
    disconnect_ui(&mut ui)?;
    drop(ui);
    let output = Command::new(E2E_CLI)
        .env_clear()
        .env("HOME", fixture.root().join("home"))
        .env("XDG_CONFIG_HOME", fixture.root().join("xdg-config"))
        .env("XDG_STATE_HOME", fixture.root().join("xdg-state"))
        .env("XDG_CACHE_HOME", fixture.root().join("xdg-cache"))
        .env("XDG_RUNTIME_DIR", fixture.runtime_dir())
        .env("LANG", "C.UTF-8")
        .args(["agent", "unload", "tau-zulip-bot", "zulip-bot-ngMK"])
        .output()?;
    assert!(
        output.status.success(),
        "exact unload failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    daemon.finish()?;
    fixture.assert_consumed()?;
    Ok(())
}

/// Proves the supervised provider sees exactly one replay-safe output-limit
/// successor whose request retains the source reasoning and internal
/// instruction.
#[test]
fn deterministic_output_length_continues_once_with_exact_replay()
-> Result<(), Box<dyn std::error::Error>> {
    const USER: &str = "finish the bounded answer";
    const REASONING: &str = "retained deterministic plan";
    const ANSWER: &str = "completed after one continuation";
    let scenario = ScenarioV2::new(
        "output-length",
        vec![ScenarioLaneV2 {
            ctx_id: "output-length".to_owned(),
            actions: vec![
                ScenarioActionV2::OutputLengthReasoning {
                    user_text: USER.to_owned(),
                    reasoning: REASONING.to_owned(),
                    report_usage: true,
                },
                ScenarioActionV2::OutputLengthContinuation {
                    user_text: USER.to_owned(),
                    reasoning: REASONING.to_owned(),
                    response: ANSWER.to_owned(),
                    report_usage: true,
                },
            ],
        }],
    );
    let fixture = DeterministicFixture::new_v2(
        "deterministic_output_length_continues_once_with_exact_replay",
        &scenario,
        FAKE_PROVIDER,
    )?;
    let outcome = fixture.run_turn(USER)?;
    assert_eq!(outcome.response, ANSWER);
    assert!(outcome.tool_calls.is_empty());
    assert!(outcome.tool_results.is_empty());

    let events = fixture.published_trace_events()?;
    assert_output_length_provider_sequence(&events, REASONING, ANSWER);
    let snapshot = DurableSnapshot::load(
        fixture.harness_state_dir(),
        &"deterministic-e2e-session".parse()?,
    )?;
    assert_output_length_durable_sequence(&snapshot, ANSWER);
    assert_eq!(fixture.trace()?.matches(" matched ").count(), 2);
    fixture.assert_consumed()?;
    Ok(())
}

/// Proves one provider terminal can install a plural wait before successful and
/// failing core-shell siblings, then receive one successful mixed aggregate.
#[test]
fn deterministic_wait_all_returns_mixed_result_and_error_members()
-> Result<(), Box<dyn std::error::Error>> {
    const USER: &str = "join one successful and one failing harness tool";
    const RESPONSE: &str = "mixed plural wait completed";
    let wait_call_id = tau_proto::ToolCallId::from("mixed-wait-all");
    let success_call_id = tau_proto::ToolCallId::from("mixed-shell-success");
    let error_call_id = tau_proto::ToolCallId::from("mixed-workdir-error");
    let probe_executable = PathBuf::from(SHELL_PROBE).canonicalize()?;
    let scenario = ScenarioV2::new(
        "wait-all-mixed",
        vec![ScenarioLaneV2 {
            ctx_id: "wait-all-mixed".to_owned(),
            actions: vec![
                ScenarioActionV2::WaitAllMixedCalls {
                    user_text: USER.to_owned(),
                    wait_call_id: wait_call_id.clone(),
                    success_call_id: success_call_id.clone(),
                    error_call_id: error_call_id.clone(),
                    probe_executable,
                },
                ScenarioActionV2::WaitAllMixedResult {
                    user_text: USER.to_owned(),
                    wait_call_id: wait_call_id.clone(),
                    success_call_id: success_call_id.clone(),
                    error_call_id: error_call_id.clone(),
                    response: RESPONSE.to_owned(),
                },
            ],
        }],
    );
    let fixture = DeterministicFixture::new_wait_all_mixed(
        "deterministic_wait_all_returns_mixed_result_and_error_members",
        &scenario,
        FAKE_PROVIDER,
    )?;
    let outcome = fixture.run_turn(USER)?;
    assert_eq!(outcome.response, RESPONSE);

    let events = fixture.published_trace_events()?;
    let aggregate = events
        .iter()
        .find_map(|event| match event {
            Event::ProviderToolResult(result) if result.call_id == wait_call_id => {
                Some(&result.result)
            }
            _ => None,
        })
        .expect("plural wait result");
    let CborValue::Map(root) = aggregate else {
        panic!("plural wait result must be a map");
    };
    let CborValue::Array(members) = &root[0].1 else {
        panic!("plural wait results must be an array");
    };
    assert_eq!(members.len(), 2);
    assert!(matches!(
        &members[0],
        CborValue::Map(entries)
            if entries.contains(&(
                CborValue::Text("original_tool_call_id".to_owned()),
                CborValue::Text(success_call_id.to_string()),
            ))
                && entries.contains(&(
                    CborValue::Text("outcome".to_owned()),
                    CborValue::Text("result".to_owned()),
                ))
    ));
    assert!(matches!(
        &members[1],
        CborValue::Map(entries)
            if entries.contains(&(
                CborValue::Text("original_tool_call_id".to_owned()),
                CborValue::Text(error_call_id.to_string()),
            ))
                && entries.contains(&(
                    CborValue::Text("outcome".to_owned()),
                    CborValue::Text("error".to_owned()),
                ))
    ));
    fixture.assert_consumed()?;
    Ok(())
}

/// Proves the real supervised provider route preserves two append deltas and a
/// complete durable final assistant response without any live provider.
#[test]
fn deterministic_text_stream_and_final_response() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = "return the deterministic greeting";
    let fixture = DeterministicFixture::new(
        "deterministic_text_stream_and_final_response",
        &ScenarioV1::text_v1(prompt, "hello deterministic tau"),
        FAKE_PROVIDER,
        None,
    )?;
    let outcome = fixture.run_turn(prompt)?;
    assert_eq!(outcome.response, "hello deterministic tau");
    assert!(outcome.tool_calls.is_empty());
    assert!(outcome.tool_results.is_empty());
    let events = fixture.published_trace_events()?;
    assert_exact_extensions(&events, &["e2e-fake-provider"]);
    assert_text_provider_sequence(&events);
    Ok(())
}

/// Proves one provider-authored function call crosses the harness's real tool
/// validation/dispatch path into deterministic `tau-ext-test-dummy`, and that
/// the exact call/result identity returns in the provider continuation.
#[test]
fn deterministic_dummy_tool_round() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = "run the deterministic dummy tool";
    let fixture = DeterministicFixture::new(
        "deterministic_dummy_tool_round",
        &ScenarioV1::dummy_tool_round_v1(prompt),
        FAKE_PROVIDER,
        Some(DUMMY_TOOL.into()),
    )?;
    let outcome = fixture.run_turn(prompt)?;
    assert_eq!(outcome.response, "tool completed");
    assert_eq!(outcome.tool_calls.len(), 1);
    assert_eq!(outcome.tool_calls[0].call_id.as_str(), "fake-call-1");
    assert_eq!(outcome.tool_calls[0].name.as_str(), "restart_test_dummy");
    assert_eq!(outcome.tool_calls[0].arguments, CborValue::Map(Vec::new()));
    assert_eq!(outcome.tool_results.len(), 1);
    assert_eq!(outcome.tool_results[0].call_id.as_str(), "fake-call-1");
    assert_eq!(outcome.tool_results[0].kind, ToolResultKind::Final);
    assert_eq!(
        outcome.tool_results[0].result,
        CborValue::Text("restart succeeded".to_owned())
    );
    assert!(outcome.tool_results[0].provider_content.is_empty());
    let events = fixture.published_trace_events()?;
    assert_exact_extensions(&events, &["e2e-fake-provider", "e2e-test-dummy"]);
    assert_tool_provider_sequence(&events);
    let extension_log = fixture.extension_log("e2e-test-dummy")?;
    assert_eq!(extension_log.matches("test dummy configured").count(), 1);
    assert!(!extension_log.contains("fake-call-1"));
    assert!(!extension_log.contains(prompt));
    Ok(())
}

/// Proves a real supervised dummy that observed a provider-authored call can
/// disconnect, let the harness own and continue its error, restart once, and
/// serve one later explicit provider-authored call only after replacement
/// Ready.
#[test]
fn deterministic_dummy_disconnect_respawns_before_later_turn()
-> Result<(), Box<dyn std::error::Error>> {
    const FIRST_PROMPT: &str = "disconnect the supervised dummy";
    const SECOND_PROMPT: &str = "use the replacement dummy";
    const FIRST_CALL: &str = "disconnect-call";
    const SECOND_CALL: &str = "replacement-call";
    const DISCONNECT_DIAGNOSTIC: &str = "tau_internal: true\n\nTool call `disconnect-call` was interrupted because extension disconnected. Side effects may have occurred.";

    let fixture = DeterministicFixture::new_exit_once_then_success_v2(
        "deterministic_dummy_disconnect_respawns_before_later_turn",
        &ScenarioV2::new(
            "exit-once-then-success",
            vec![ScenarioLaneV2 {
                ctx_id: "exit-once-lane".to_owned(),
                actions: vec![
                    ScenarioActionV2::DummyToolCall {
                        user_text: FIRST_PROMPT.to_owned(),
                        call_id: FIRST_CALL.into(),
                    },
                    ScenarioActionV2::DummyToolRepair {
                        user_text: FIRST_PROMPT.to_owned(),
                        call_id: FIRST_CALL.into(),
                        diagnostic: DISCONNECT_DIAGNOSTIC.to_owned(),
                        response: "disconnect observed".to_owned(),
                    },
                    ScenarioActionV2::DummyToolCall {
                        user_text: SECOND_PROMPT.to_owned(),
                        call_id: SECOND_CALL.into(),
                    },
                    ScenarioActionV2::DummyToolResult {
                        user_text: SECOND_PROMPT.to_owned(),
                        call_id: SECOND_CALL.into(),
                        response: "replacement succeeded".to_owned(),
                    },
                ],
            }],
        ),
        FAKE_PROVIDER,
        DUMMY_TOOL,
    )?;
    let socket = fixture.socket_path("exit-once-respawn");
    let server = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::New);
    let mut peer = connect_ui(&socket)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut observed = Vec::new();

    create_agent(&mut peer, "exit-once-lane", FIRST_PROMPT)?;
    let agent_id = loop {
        let event = recv_observed_before(&mut peer, deadline)?;
        let created = match &event.event {
            Event::AgentPromptCreated(created)
                if created.ctx_id.as_deref() == Some("exit-once-lane") =>
            {
                Some(created.agent_id.clone())
            }
            _ => None,
        };
        observed.push(event);
        if let Some(created) = created {
            break created;
        }
    };

    let mut saw_progress = false;
    loop {
        let event = recv_observed_before(&mut peer, deadline)?;
        match &event.event {
            Event::ToolProgress(progress)
                if progress.call_id.as_str() == FIRST_CALL
                    && progress.message.as_deref() == Some("exit_once_then_success ready") =>
            {
                saw_progress = true;
            }
            Event::ProviderResponseFinished(finished) if saw_progress => {
                assert_assistant(&finished.output_items, "disconnect observed");
                observed.push(event);
                break;
            }
            _ => {}
        }
        observed.push(event);
    }

    wait_for_dummy_restart_ready(&mut peer, deadline, &mut observed)?;
    submit_prompt(&mut peer, &agent_id, "replacement-turn", SECOND_PROMPT)?;
    let mut second_finished = false;
    loop {
        let event = recv_observed_before(&mut peer, deadline)?;
        match &event.event {
            Event::ProviderResponseFinished(finished)
                if matches!(
                    finished.output_items.as_slice(),
                    [ContextItem::Message(message)]
                        if message.content
                            == [tau_proto::ContentPart::Text {
                                text: "replacement succeeded".to_owned()
                            }]
                ) =>
            {
                assert_assistant(&finished.output_items, "replacement succeeded");
                second_finished = true;
            }
            Event::AgentStatsUpdated(stats)
                if second_finished
                    && stats.agent_id == agent_id
                    && stats.runtime_state == AgentRuntimeState::Idle
                    && stats.tools.in_flight == 0 =>
            {
                observed.push(event);
                break;
            }
            _ => {}
        }
        observed.push(event);
    }

    disconnect_ui(&mut peer)?;
    server.finish()?;
    let snapshot = DurableSnapshot::load(
        fixture.harness_state_dir(),
        &"deterministic-e2e-session".parse()?,
    )?;
    assert_exit_once_respawn_events(
        &observed,
        &snapshot,
        FIRST_CALL,
        SECOND_CALL,
        DISCONNECT_DIAGNOSTIC,
    )?;
    assert_exit_once_public_projections(
        &fixture.published_trace_events()?,
        FIRST_CALL,
        SECOND_CALL,
        DISCONNECT_DIAGNOSTIC,
    )?;
    fixture.assert_consumed()?;
    Ok(())
}

/// Requires the public journal projection to publish one disconnected error and
/// one later normal success independently of their canonical durable owners.
fn assert_exit_once_public_projections(
    events: &[Event],
    first_call: &str,
    second_call: &str,
    _diagnostic: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let errors = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::ToolError(error) if error.call_id.as_str() == first_call
            )
        })
        .count();
    let results = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::ToolResult(result)
                    if result.call_id.as_str() == second_call
                        && result.result == CborValue::Text("restart succeeded".to_owned())
            )
        })
        .count();
    if errors != 1 || results != 1 {
        return Err(format!(
            "public terminal projections changed: disconnected errors={errors}, replacement results={results}"
        )
        .into());
    }
    Ok(())
}

/// Waits for the sole expected restart and replacement readiness without
/// dropping unrelated lifecycle and public projection observations.
fn wait_for_dummy_restart_ready(
    peer: &mut tau_socket::SocketPeer,
    deadline: Instant,
    observed: &mut Vec<DaemonObserved>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut restarted = false;
    loop {
        let event = recv_observed_before(peer, deadline).map_err(|error| {
            format!(
                "wait for dummy restart/Ready after {} observations: {error}",
                observed.len()
            )
        })?;
        match &event.event {
            Event::ExtensionRestarting(restarting)
                if restarting.extension_name.as_str() == "e2e-test-dummy" =>
            {
                if restarted
                    || restarting.attempt != 1
                    || restarting.reason.as_deref() != Some("unexpected disconnect")
                {
                    return Err(
                        format!("unexpected dummy restart observation: {restarting:?}").into(),
                    );
                }
                restarted = true;
            }
            Event::ExtensionReady(ready)
                if restarted && ready.extension_name.as_str() == "e2e-test-dummy" =>
            {
                observed.push(event);
                return Ok(());
            }
            _ => {}
        }
        observed.push(event);
    }
}

/// Separates canonical durable ownership from public tool projections and
/// verifies the one causal disconnect/restart/replacement ordering.
fn assert_exit_once_respawn_events(
    observed: &[DaemonObserved],
    snapshot: &DurableSnapshot,
    first_call: &str,
    second_call: &str,
    diagnostic: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let first_ready = observed
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                Event::ExtensionReady(ready) if ready.extension_name.as_str() == "e2e-test-dummy"
            )
        })
        .ok_or("missing initial dummy readiness")?;
    let first_started = observed
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                Event::ToolStarted(started) if started.call_id.as_str() == first_call
            )
        })
        .ok_or("missing first public tool start")?;
    let first_public_error = observed
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                Event::ToolError(error)
                    if error.call_id.as_str() == first_call && error.message == diagnostic
            )
        })
        .ok_or("missing first public disconnected tool error")?;
    let first_progress = observed
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                Event::ToolProgress(progress)
                    if progress.call_id.as_str() == first_call
                        && progress.message.as_deref() == Some("exit_once_then_success ready")
            )
        })
        .ok_or("missing correlated first-call dummy observation progress")?;
    let restart = observed
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                Event::ExtensionRestarting(restarting)
                    if restarting.extension_name.as_str() == "e2e-test-dummy"
            )
        })
        .ok_or("missing dummy restart")?;
    let replacement_ready = observed
        .iter()
        .enumerate()
        .skip(restart + 1)
        .find_map(|(index, event)| {
            matches!(
                &event.event,
                Event::ExtensionReady(ready) if ready.extension_name.as_str() == "e2e-test-dummy"
            )
            .then_some(index)
        })
        .ok_or("missing replacement dummy readiness")?;
    let second_started = observed
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                Event::ToolStarted(started) if started.call_id.as_str() == second_call
            )
        })
        .ok_or("missing second public tool start")?;
    let dummy_exits = observed
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(
                &event.event,
                Event::ExtensionExited(exited)
                    if exited.extension_name.as_str() == "e2e-test-dummy"
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    if dummy_exits.len() != 1
        || !(first_started < first_progress
            && first_progress < first_public_error
            && first_public_error < dummy_exits[0]
            && dummy_exits[0] < restart
            && dummy_exits[0] < replacement_ready
            && replacement_ready < second_started)
        || first_started <= first_ready
    {
        return Err("dummy lifecycle/public projection order changed".into());
    }
    let dummy_ready_count = observed
        .iter()
        .filter(|event| {
            matches!(
                &event.event,
                Event::ExtensionReady(ready) if ready.extension_name.as_str() == "e2e-test-dummy"
            )
        })
        .count();
    if dummy_ready_count != 2 {
        return Err(format!(
            "expected initial and replacement dummy Ready, got {dummy_ready_count}"
        )
        .into());
    }
    let initial_instance = match &observed[first_ready].event {
        Event::ExtensionReady(ready) => &ready.instance_id,
        _ => unreachable!("initial readiness position identifies ExtensionReady"),
    };
    let replacement_instance = match &observed[replacement_ready].event {
        Event::ExtensionReady(ready) => &ready.instance_id,
        _ => unreachable!("replacement readiness position identifies ExtensionReady"),
    };
    if initial_instance != replacement_instance {
        return Err("dummy respawn changed its logical instance identity".into());
    }
    if observed
        .iter()
        .filter(|event| {
            matches!(
                &event.event,
                Event::ExtensionRestarting(restarting)
                    if restarting.extension_name.as_str() == "e2e-test-dummy"
            )
        })
        .count()
        != 1
    {
        return Err("dummy restarted more than once".into());
    }
    if observed.iter().any(|event| {
        matches!(event.event, Event::ExtensionRestarting(_))
            && !matches!(
                &event.event,
                Event::ExtensionRestarting(restarting)
                    if restarting.extension_name.as_str() == "e2e-test-dummy"
            )
    }) {
        return Err("provider or unrelated extension restarted".into());
    }

    let durable = snapshot
        .agent_events
        .iter()
        .map(|record| (&record.event, record.observation_id))
        .collect::<Vec<_>>();
    let first_errors = durable
        .iter()
        .filter_map(|(event, observation_id)| match event {
            Event::ProviderToolError(error)
                if error.call_id.as_str() == first_call
                    && error.message == diagnostic
                    && error.tool_name.as_str() == "restart_test_dummy" =>
            {
                Some(*observation_id)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if first_errors.len() != 1
        || durable.iter().any(|(event, _)| {
            matches!(event, Event::ProviderToolResult(result) if result.call_id.as_str() == first_call)
        })
    {
        return Err("first call did not have exactly one canonical disconnected error".into());
    }
    if !durable.iter().any(|(event, _)| {
        matches!(
            event,
            Event::AgentToolTerminalClassified(classification)
                if classification.terminal == first_errors[0]
                    && classification.cause == tau_proto::ToolTerminalCause::ProviderDisconnected
        )
    }) {
        return Err("first canonical error lacked ProviderDisconnected classification".into());
    }
    let second_results = durable
        .iter()
        .filter(|(event, _)| {
            matches!(
                event,
                Event::ProviderToolResult(result)
                    if result.call_id.as_str() == second_call
                        && result.result == CborValue::Text("restart succeeded".to_owned())
            )
        })
        .count();
    if second_results != 1
        || durable.iter().any(|(event, _)| {
            matches!(event, Event::ProviderToolError(error) if error.call_id.as_str() == second_call)
        })
    {
        return Err("replacement call did not have exactly one canonical success".into());
    }
    let folded =
        tau_core::AgentTree::try_from_events(snapshot.agent_id.clone(), &snapshot.agent_events)
            .map_err(|error| {
                format!("final durable agent fold rejected its event stream: {error}")
            })?;
    if folded.has_open_foreground_tool_round()
        || !folded.unresolved_foreground_tool_calls().is_empty()
    {
        return Err("final durable agent fold retained an unresolved foreground tool call".into());
    }
    Ok(())
}

/// The real provider, status handler, and dummy tool preserve current-status
/// policy in either parallel order: Working suppresses the tool-start reminder,
/// blocks a final, and Done or Blocked releases that final.
#[test]
fn deterministic_current_status_policy_round() -> Result<(), Box<dyn std::error::Error>> {
    for order in [StatusToolOrder::StatusFirst, StatusToolOrder::WorkFirst] {
        for initial_status in [
            InitialStatusOutcome::AcceptedWorking,
            InitialStatusOutcome::Rejected,
        ] {
            for terminal_phase in [StatusTerminalPhase::Done, StatusTerminalPhase::Blocked] {
                let prompt =
                    format!("run current status policy in {order:?} order with {initial_status:?}");
                let fixture = DeterministicFixture::new(
                    "deterministic_current_status_policy_round",
                    &ScenarioV1::status_policy_round_v1(
                        &prompt,
                        order,
                        initial_status,
                        terminal_phase,
                    ),
                    FAKE_PROVIDER,
                    Some(DUMMY_TOOL.into()),
                )?;
                let outcome = fixture.run_turn(&prompt)?;
                assert_eq!(outcome.response, "status policy completed");
                assert_eq!(outcome.tool_calls.len(), 4);
                assert_eq!(
                    outcome.tool_results.len(),
                    if initial_status == InitialStatusOutcome::AcceptedWorking {
                        4
                    } else {
                        3
                    }
                );
                assert_eq!(
                    outcome.tool_results.iter().any(|result| {
                        result.call_id.as_str() == "status-policy-working"
                            && result.result
                                == CborValue::Text(
                                    "Status accepted: working — Exercise current status policy"
                                        .to_owned(),
                                )
                    }),
                    initial_status == InitialStatusOutcome::AcceptedWorking
                );
            }
        }
    }
    Ok(())
}

/// Proves strict startup configuration rejects an unsupported scenario version
/// before Ready and causes required-extension harness startup to fail.
#[test]
fn deterministic_bad_config_fails_startup() -> Result<(), Box<dyn std::error::Error>> {
    let mut scenario = ScenarioV1::text_v1("unused", "unused");
    scenario.version = 2;
    let fixture = DeterministicFixture::new(
        "deterministic_bad_config_fails_startup",
        &scenario,
        FAKE_PROVIDER,
        None,
    )?;
    let error = fixture
        .run_turn("unused")
        .expect_err("required provider must reject bad config before Ready");
    let diagnostic = error.to_string();
    assert!(
        diagnostic.contains("ScenarioV1 version must be 0"),
        "unexpected startup diagnostic: {diagnostic}"
    );
    assert!(fixture.root().join("artifacts/scenario.json").is_file());
    assert!(
        fixture
            .root()
            .join("artifacts/harness-config.json")
            .is_file()
    );
    // The exact provider diagnostic proves startup failed before Ready. Do not
    // use the best-effort asynchronous debug trace as a fatal-startup oracle:
    // the daemon may exit before its detached writer creates events.jsonl.
    fixture.acknowledge_expected_failure();
    Ok(())
}

/// Proves a first-turn prompt mismatch fails the real interaction, retains a
/// bounded semantic diagnostic, and cannot be mistaken for scenario
/// consumption.
#[test]
fn deterministic_prompt_mismatch_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = DeterministicFixture::new(
        "deterministic_prompt_mismatch_fails_closed",
        &ScenarioV1::text_v1("expected prompt", "unreachable"),
        FAKE_PROVIDER,
        None,
    )?;
    let error = fixture
        .run_turn("wrong prompt")
        .expect_err("mismatched prompt must disconnect without a successful response");
    assert!(
        error.to_string().contains("provider disconnected")
            || error
                .to_string()
                .contains("scenario first mismatch at turn 0"),
        "unexpected mismatch error: {error}"
    );
    let trace = fixture.trace()?;
    assert!(
        trace.contains("scenario first mismatch at turn 0: last HumanUi user envelope mismatch")
    );
    assert!(trace.lines().all(|line| line.len() <= 1024));
    assert!(fixture.assert_consumed().is_err());
    fixture.acknowledge_expected_failure();
    Ok(())
}

/// Proves a typed terminal rejection does not prevent a later explicit user
/// turn from succeeding; this is not provider retry coverage.
#[test]
fn deterministic_typed_error_then_later_success() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = DeterministicFixture::new_v2(
        "deterministic_typed_error_then_later_success",
        &ScenarioV2::new(
            "error-then-success",
            vec![ScenarioLaneV2 {
                ctx_id: "error-lane".to_owned(),
                actions: vec![
                    ScenarioActionV2::Error {
                        user_text: "reject this request".to_owned(),
                        failure_kind: ProviderFailureKind::RequestRejected,
                        error: "synthetic rejection".to_owned(),
                    },
                    ScenarioActionV2::Text {
                        user_text: "try a later request".to_owned(),
                        response: "later request succeeded".to_owned(),
                    },
                ],
            }],
        ),
        FAKE_PROVIDER,
    )?;
    let socket = fixture.socket_path("error-success");
    let server = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::New);
    let mut peer = connect_ui(&socket)?;
    create_agent(&mut peer, "error-lane", "reject this request")?;
    let first = recv_until_finished(&mut peer)?;
    assert_eq!(first.stop_reason, tau_proto::ProviderStopReason::Error);
    assert_eq!(
        first.failure_kind,
        Some(ProviderFailureKind::RequestRejected)
    );
    assert_eq!(first.error.as_deref(), Some("synthetic rejection"));
    assert!(first.output_items.is_empty());
    submit_prompt(
        &mut peer,
        &first.agent_id,
        "later-success",
        "try a later request",
    )?;
    let second = recv_until_finished(&mut peer)?;
    assert_eq!(second.stop_reason, tau_proto::ProviderStopReason::EndTurn);
    assert_assistant(&second.output_items, "later request succeeded");
    disconnect_ui(&mut peer)?;
    server.finish()?;
    fixture.assert_consumed()?;
    Ok(())
}

/// Clean restart preserves the completed `H, R` prefix and dispatches one later
/// `Q` successor without resending the closed prompt.
#[test]
fn deterministic_restart_preserves_response_before_successor_input()
-> Result<(), Box<dyn std::error::Error>> {
    const H: &str = "restart placement H";
    const R: &str = "restart placement R";
    const Q: &str = "restart placement Q";
    let fixture = DeterministicFixture::new_v2(
        "deterministic_restart_preserves_response_before_successor_input",
        &ScenarioV2::new(
            "restart-provider-context-placement",
            vec![ScenarioLaneV2 {
                ctx_id: "restart-placement".to_owned(),
                actions: vec![
                    ScenarioActionV2::Text {
                        user_text: H.to_owned(),
                        response: R.to_owned(),
                    },
                    ScenarioActionV2::Text {
                        user_text: Q.to_owned(),
                        response: "restart successor".to_owned(),
                    },
                ],
            }],
        ),
        FAKE_PROVIDER,
    )?;
    let socket_a = fixture.socket_path("placement-boot-a");
    let server_a = spawn_daemon(&fixture, &socket_a, tau_harness::SessionLaunchStatus::New);
    let mut peer_a = connect_ui(&socket_a)?;
    create_agent(&mut peer_a, "restart-placement", H)?;
    let first = recv_until_finished(&mut peer_a)?;
    assert_assistant(&first.output_items, R);
    let agent_id = first.agent_id;
    disconnect_ui(&mut peer_a)?;
    server_a.finish()?;
    let snapshot_a = DurableSnapshot::load(
        fixture.harness_state_dir(),
        &"deterministic-e2e-session".parse()?,
    )?;
    assert_restart_boot_a_prefix(&snapshot_a, H, R);
    let journal_prefix = std::fs::read(
        fixture
            .harness_state_dir()
            .join("agents")
            .join(agent_id.as_str())
            .join("events.cbor"),
    )?;

    let socket_b = fixture.socket_path("placement-boot-b");
    let server_b = spawn_daemon(
        &fixture,
        &socket_b,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut peer_b = connect_ui(&socket_b)?;
    loop {
        if matches!(
            recv_observed(&mut peer_b)?.event,
            Event::SessionReplayComplete(_)
        ) {
            break;
        }
    }
    submit_prompt(&mut peer_b, &agent_id, "restart-successor", Q)?;
    let second_prompt = recv_until_created(&mut peer_b, Some("restart-successor"))?;
    assert_restart_context(&second_prompt.context, H, R, Q);
    let second = recv_until_finished_for(&mut peer_b, &second_prompt.agent_prompt_id)?;
    assert_assistant(&second.output_items, "restart successor");
    disconnect_ui(&mut peer_b)?;
    server_b.finish()?;
    let snapshot_b = DurableSnapshot::load(
        fixture.harness_state_dir(),
        &"deterministic-e2e-session".parse()?,
    )?;
    snapshot_b.require_prefix(&snapshot_a)?;
    let journal_after = std::fs::read(
        fixture
            .harness_state_dir()
            .join("agents")
            .join(agent_id.as_str())
            .join("events.cbor"),
    )?;
    assert!(
        journal_after.starts_with(&journal_prefix),
        "Boot B rewrote the closed Boot A agent journal prefix"
    );
    assert_restart_successor_suffix(&snapshot_a, &snapshot_b, &second_prompt.agent_prompt_id, Q);
    assert_eq!(
        fixture.trace()?.matches(" matched ").count(),
        2,
        "cold replay must not consume a source action or dispatch a third turn"
    );
    fixture.assert_consumed()?;
    Ok(())
}

/// Proves the daemon guard refuses a nominally successful parent exit while a
/// process-group child remains, then reaps that child as failure containment.
#[test]
fn daemon_finish_rejects_a_lingering_process_group_member() {
    assert_daemon_finish_rejects_a_lingering_process_group_member();
}

/// Proves a provider-side hold has a hard deadline, its worker is reaped, and
/// an independent later agent remains live.
#[test]
fn deterministic_hold_timeout_is_bounded_and_reaped_before_later_work()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = DeterministicFixture::new_v2(
        "deterministic_hold_timeout_is_bounded_and_reaped_before_later_work",
        &ScenarioV2::new(
            "timeout-then-success",
            vec![
                ScenarioLaneV2 {
                    ctx_id: "timeout-lane".to_owned(),
                    actions: vec![ScenarioActionV2::HoldUntilCancel {
                        user_text: "time out".to_owned(),
                        timeout_ms: 100,
                    }],
                },
                ScenarioLaneV2 {
                    ctx_id: "after-timeout".to_owned(),
                    actions: vec![ScenarioActionV2::Text {
                        user_text: "continue after timeout".to_owned(),
                        response: "timeout cleaned".to_owned(),
                    }],
                },
            ],
        ),
        FAKE_PROVIDER,
    )?;
    let socket = fixture.socket_path("timeout");
    let server = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::New);
    let mut peer = connect_ui(&socket)?;
    create_agent(&mut peer, "timeout-lane", "time out")?;
    let timeout = recv_until_finished(&mut peer)?;
    assert_eq!(timeout.stop_reason, tau_proto::ProviderStopReason::Error);
    assert_eq!(timeout.failure_kind, Some(ProviderFailureKind::Unknown));
    assert_eq!(
        timeout.error.as_deref(),
        Some("deterministic hold timed out")
    );
    create_agent(&mut peer, "after-timeout", "continue after timeout")?;
    let later = recv_until_finished(&mut peer)?;
    assert_assistant(&later.output_items, "timeout cleaned");
    disconnect_ui(&mut peer)?;
    server.finish()?;
    let trace = fixture.trace()?;
    assert_eq!(trace.matches("hold_timeout").count(), 1);
    assert_eq!(trace.matches("hold_reaped").count(), 1);
    fixture.assert_consumed()?;
    Ok(())
}

/// Proves both agents cross the barrier before either terminal response and
/// each dynamic agent identity receives its lane-local response.
#[test]
fn deterministic_two_lane_barrier_isolates_concurrent_agents()
-> Result<(), Box<dyn std::error::Error>> {
    let lanes = ["lane-a", "lane-b"]
        .into_iter()
        .map(|lane| ScenarioLaneV2 {
            ctx_id: lane.to_owned(),
            actions: vec![ScenarioActionV2::BarrierText {
                user_text: format!("prompt {lane}"),
                barrier: "both".to_owned(),
                participants: 2,
                response: format!("response {lane}"),
            }],
        })
        .collect();
    let fixture = DeterministicFixture::new_v2(
        "deterministic_two_lane_barrier_isolates_concurrent_agents",
        &ScenarioV2::new("two-lane-barrier", lanes),
        FAKE_PROVIDER,
    )?;
    let socket = fixture.socket_path("barrier");
    let server = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::New);
    let mut peer = connect_ui(&socket)?;
    create_agent(&mut peer, "lane-a", "prompt lane-a")?;
    let created_a = recv_until_created(&mut peer, Some("lane-a"))?;
    let submitted_a = recv_until_submitted(&mut peer)?;
    assert_eq!(submitted_a.agent_prompt_id, created_a.agent_prompt_id);
    create_agent(&mut peer, "lane-b", "prompt lane-b")?;
    let created_b = recv_until_created(&mut peer, Some("lane-b"))?;
    let submitted_b = recv_until_submitted(&mut peer)?;
    assert_eq!(submitted_b.agent_prompt_id, created_b.agent_prompt_id);
    let first = recv_until_finished(&mut peer)?;
    let second = recv_until_finished(&mut peer)?;
    let responses = [
        (first.agent_id, assistant_text(&first.output_items)),
        (second.agent_id, assistant_text(&second.output_items)),
    ]
    .into_iter()
    .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(
        responses.get(&created_a.agent_id).map(String::as_str),
        Some("response lane-a")
    );
    assert_eq!(
        responses.get(&created_b.agent_id).map(String::as_str),
        Some("response lane-b")
    );
    disconnect_ui(&mut peer)?;
    server.finish()?;
    fixture.assert_consumed()?;
    Ok(())
}

/// Proves provider disconnect is daemon-fatal and produces one exit fact
/// without provider respawn.
#[test]
fn deterministic_provider_disconnect_is_fatal_and_not_restarted()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = DeterministicFixture::new_v2(
        "deterministic_provider_disconnect_is_fatal_and_not_restarted",
        &ScenarioV2::new(
            "disconnect",
            vec![ScenarioLaneV2 {
                ctx_id: "disconnect-lane".to_owned(),
                actions: vec![ScenarioActionV2::Disconnect {
                    user_text: "disconnect now".to_owned(),
                    reason: "synthetic provider disconnect".to_owned(),
                }],
            }],
        ),
        FAKE_PROVIDER,
    )?;
    let socket = fixture.socket_path("disconnect");
    let server = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::New);
    let mut peer = connect_ui(&socket)?;
    create_agent(&mut peer, "disconnect-lane", "disconnect now")?;
    let error = server
        .finish()
        .expect_err("provider disconnect must terminate daemon");
    assert!(error.to_string().contains("provider disconnected"));
    let events = recv_remaining_events(&mut peer)?;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::ExtensionExited(_)))
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, Event::ExtensionRestarting(_)))
    );
    fixture.assert_consumed()?;
    Ok(())
}

/// Proves a quiescent clean resume restores the same durable agent's immutable
/// lane binding and cursor without consuming replay deliveries.
#[test]
fn deterministic_clean_resume_restores_fake_cursor_without_replay_consumption()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = DeterministicFixture::new_v2(
        "deterministic_clean_resume_restores_fake_cursor_without_replay_consumption",
        &ScenarioV2::new(
            "clean-resume",
            vec![ScenarioLaneV2 {
                ctx_id: "resume-lane".to_owned(),
                actions: vec![
                    ScenarioActionV2::Text {
                        user_text: "before clean stop".to_owned(),
                        response: "before".to_owned(),
                    },
                    ScenarioActionV2::Text {
                        user_text: "after clean resume".to_owned(),
                        response: "after".to_owned(),
                    },
                ],
            }],
        ),
        FAKE_PROVIDER,
    )?;
    let first_agent = {
        let socket = fixture.socket_path("resume-one");
        let server = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::New);
        let mut peer = connect_ui(&socket)?;
        create_agent(&mut peer, "resume-lane", "before clean stop")?;
        let created = recv_until_created(&mut peer, Some("resume-lane"))?;
        let finished = recv_until_finished_for(&mut peer, &created.agent_prompt_id)?;
        assert_assistant(&finished.output_items, "before");
        disconnect_ui(&mut peer)?;
        server.finish()?;
        created.agent_id
    };
    let socket = fixture.socket_path("resume-two");
    let server = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::Resumed);
    let mut peer = connect_ui(&socket)?;
    submit_prompt(
        &mut peer,
        &first_agent,
        "fresh-ctx-must-not-rebind",
        "after clean resume",
    )?;
    let created = recv_until_created(&mut peer, Some("fresh-ctx-must-not-rebind"))?;
    assert_eq!(created.agent_id, first_agent);
    let finished = recv_until_finished_for(&mut peer, &created.agent_prompt_id)?;
    assert_assistant(&finished.output_items, "after");
    disconnect_ui(&mut peer)?;
    server.finish()?;
    let trace = fixture.trace()?;
    assert_eq!(trace.matches(" configured").count(), 2);
    assert_eq!(trace.matches(" matched ").count(), 2);
    fixture.assert_consumed()?;
    Ok(())
}

/// Proves a dummy's typed image crosses a complete live tool round, survives a
/// clean restart as exact durable bytes, and reaches the later provider prompt
/// once without leaking into generic fixture metadata.
#[test]
fn deterministic_typed_image_tool_result_replays_after_clean_restart()
-> Result<(), Box<dyn std::error::Error>> {
    const CALL_ID: &str = "typed-image-call";
    const IMAGE_DIGEST: &str = "1c22ad7f40a18bbcb1c50dc8a78ac6a1a36b9a0a3c7f9833c965b2ef8100a734";
    let fixture = DeterministicFixture::new_typed_image_v2(
        "deterministic_typed_image_tool_result_replays_after_clean_restart",
        &ScenarioV2::new(
            "typed-image-tool-replay",
            vec![ScenarioLaneV2 {
                ctx_id: "typed-image-lane".to_owned(),
                actions: vec![
                    ScenarioActionV2::TypedImageToolCall {
                        user_text: "inspect the deterministic image".to_owned(),
                        call_id: CALL_ID.into(),
                    },
                    ScenarioActionV2::TypedImageToolResult {
                        call_id: CALL_ID.into(),
                        response: "live typed image accepted".to_owned(),
                    },
                    ScenarioActionV2::TypedImageReplay {
                        user_text: "continue after typed image restart".to_owned(),
                        call_id: CALL_ID.into(),
                        response: "replayed typed image accepted".to_owned(),
                    },
                ],
            }],
        ),
        FAKE_PROVIDER,
        DUMMY_TOOL,
    )?;
    let call_id = CALL_ID.into();
    let agent_id = {
        let socket = fixture.socket_path("typed-image-boot-a");
        let server = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::New);
        let mut peer = connect_ui(&socket)?;
        create_agent(
            &mut peer,
            "typed-image-lane",
            "inspect the deterministic image",
        )?;
        let created = recv_until_created(&mut peer, Some("typed-image-lane"))?;
        let display = recv_until_tool_continuation_idle(
            &mut peer,
            &created.agent_id,
            &created.agent_prompt_id,
            &call_id,
        )
        .map_err(|error| format!("wait for live typed-image continuation: {error}"))?;
        assert_typed_image_display(&display, CALL_ID);
        disconnect_ui(&mut peer)?;
        server.finish()?;
        created.agent_id
    };
    let snapshot_a = DurableSnapshot::load(
        fixture.harness_state_dir(),
        &"deterministic-e2e-session".parse()?,
    )?;
    assert_typed_image_round(&snapshot_a, &call_id, IMAGE_DIGEST)?;
    assert_durable_assistant(&snapshot_a, "live typed image accepted");

    let socket = fixture.socket_path("typed-image-boot-b");
    let server = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::Resumed);
    let mut peer = connect_ui(&socket)?;
    let replay_display = recv_until_typed_image_display(&mut peer, &agent_id, &call_id)
        .map_err(|error| format!("wait for byte-free typed-image replay display: {error}"))?;
    assert_typed_image_display(&replay_display, CALL_ID);
    submit_prompt(
        &mut peer,
        &agent_id,
        "typed-image-replay",
        "continue after typed image restart",
    )?;
    let created = recv_until_created(&mut peer, Some("typed-image-replay"))?;
    assert_eq!(created.agent_id, agent_id);
    let finished = recv_until_finished_for(&mut peer, &created.agent_prompt_id)
        .map_err(|error| format!("wait for replayed typed-image continuation: {error}"))?;
    assert_assistant(&finished.output_items, "replayed typed image accepted");
    disconnect_ui(&mut peer)?;
    server.finish()?;

    let snapshot_b = DurableSnapshot::load(
        fixture.harness_state_dir(),
        &"deterministic-e2e-session".parse()?,
    )?;
    snapshot_b.require_prefix(&snapshot_a)?;
    assert_typed_image_round(&snapshot_b, &call_id, IMAGE_DIGEST)?;
    assert_durable_assistant(&snapshot_b, "replayed typed image accepted");
    assert_eq!(fixture.trace()?.matches(" configured").count(), 2);
    assert_eq!(fixture.trace()?.matches(" matched ").count(), 3);
    fixture.assert_consumed()?;
    Ok(())
}

/// Requires one complete durable call/result round with the fixed canonical
/// image, retaining its exact bytes and structural image properties.
fn assert_typed_image_round(
    snapshot: &DurableSnapshot,
    call_id: &tau_proto::ToolCallId,
    expected_digest: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        snapshot
            .image_tool_result_digest(call_id)?
            .to_hex()
            .as_str(),
        expected_digest
    );
    let events = snapshot
        .agent_events
        .iter()
        .map(|record| &record.event)
        .collect::<Vec<_>>();
    let call_positions = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            Event::ProviderResponseFinished(finished)
                if matches!(
                    finished.output_items.as_slice(),
                    [ContextItem::ToolCall(call)]
                        if &call.call_id == call_id
                            && call.name.as_str()
                                == tau_ext_test_dummy::TYPED_IMAGE_TEST_DUMMY_TOOL_NAME
                ) =>
            {
                Some(index)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let generic_results = events
        .iter()
        .filter(|event| matches!(event, Event::ToolResult(result) if &result.call_id == call_id))
        .count();
    assert_eq!(
        generic_results, 0,
        "typed image must remain on the canonical provider tool-result route"
    );
    let result_positions = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            Event::ProviderToolResult(result) if &result.call_id == call_id => {
                Some((index, result))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(call_positions.len(), 1, "typed image call must be unique");
    assert_eq!(
        result_positions.len(),
        1,
        "typed image result must be unique"
    );
    let (result_position, result) = result_positions[0];
    assert!(
        call_positions[0] < result_position,
        "typed image result must close its provider-authored call"
    );
    assert!(
        events[call_positions[0] + 1..result_position]
            .iter()
            .all(|event| !matches!(event, Event::ProviderResponseFinished(_))),
        "no later provider response may interrupt the typed image round"
    );
    let [ToolResultContentPart::Image(image)] = result.provider_content.as_slice() else {
        panic!("typed image result must retain exactly one typed image");
    };
    assert_eq!(
        result.tool_name.as_str(),
        tau_ext_test_dummy::TYPED_IMAGE_TEST_DUMMY_TOOL_NAME
    );
    assert_eq!(result.tool_type, tau_proto::ToolType::Function);
    assert_eq!(result.kind, ToolResultKind::Final);
    assert_eq!(
        result.result,
        CborValue::Text("typed image succeeded".to_owned())
    );
    assert_eq!(image.media_type, ImageMediaType::Png);
    assert_eq!((image.width, image.height), (1, 1));
    assert_eq!(image.detail, ImageDetail::High);
    assert!(
        image.data.len() == tau_ext_test_dummy::TYPED_IMAGE_PNG.len()
            && image.data.as_ref() == tau_ext_test_dummy::TYPED_IMAGE_PNG,
        "typed image bytes differ from the fixed canonical fixture"
    );
    Ok(())
}

/// Waits for the provider-authored typed-image call and its agent's later idle
/// state, avoiding timing-based shutdown.
fn recv_until_tool_continuation_idle(
    peer: &mut tau_socket::SocketPeer,
    agent_id: &AgentId,
    prompt_id: &tau_proto::AgentPromptId,
    call_id: &tau_proto::ToolCallId,
) -> Result<tau_proto::ToolResultDisplay, Box<dyn std::error::Error>> {
    let mut saw_tool_call = false;
    let mut saw_continuation = false;
    let mut display = None;
    loop {
        match recv_event(peer)? {
            Event::ProviderResponseFinished(finished)
                if &finished.agent_prompt_id == prompt_id
                    && matches!(
                        finished.output_items.as_slice(),
                        [ContextItem::ToolCall(call)]
                            if call.name.as_str()
                                == tau_ext_test_dummy::TYPED_IMAGE_TEST_DUMMY_TOOL_NAME
                    ) =>
            {
                saw_tool_call = true;
            }
            Event::ToolResultDisplay(value) if saw_tool_call && &value.call_id == call_id => {
                if display.replace(value).is_some() {
                    return Err("duplicate typed-image display before live continuation".into());
                }
            }
            Event::ToolResult(result) if &result.call_id == call_id => {
                return Err("generic UI event carried the typed-image result".into());
            }
            Event::ProviderPromptSubmitted(submitted)
                if saw_tool_call && submitted.agent_prompt_id != *prompt_id =>
            {
                saw_continuation = true;
            }
            Event::AgentStatsUpdated(stats)
                if saw_continuation
                    && display.is_some()
                    && &stats.agent_id == agent_id
                    && stats.runtime_state == AgentRuntimeState::Idle
                    && stats.tools.in_flight == 0 =>
            {
                return Ok(display.expect("guard retained typed-image display"));
            }
            _ => {}
        }
    }
}

/// Waits for the historical byte-free UI projection of the typed image result.
fn recv_until_typed_image_display(
    peer: &mut tau_socket::SocketPeer,
    agent_id: &AgentId,
    call_id: &tau_proto::ToolCallId,
) -> Result<tau_proto::ToolResultDisplay, Box<dyn std::error::Error>> {
    let mut display = None;
    loop {
        match recv_event(peer)? {
            Event::ToolResultDisplay(value) if &value.call_id == call_id => {
                if display.replace(value).is_some() {
                    return Err("duplicate typed-image display before replay completion".into());
                }
            }
            Event::ToolResult(result) if &result.call_id == call_id => {
                return Err("historical UI event carried the typed-image result".into());
            }
            Event::AgentReplayComplete(replay)
                if &replay.agent_id == agent_id && replay.error.is_none() =>
            {
                return display.ok_or_else(|| {
                    "typed-image display was absent before agent replay completed".into()
                });
            }
            _ => {}
        }
    }
}

/// Requires the generic UI projection to preserve only tool correlation and
/// presentation metadata, never a typed image buffer.
fn assert_typed_image_display(display: &tau_proto::ToolResultDisplay, call_id: &str) {
    assert_eq!(display.call_id.as_str(), call_id);
    assert_eq!(
        display.tool_name.as_str(),
        tau_ext_test_dummy::TYPED_IMAGE_TEST_DUMMY_TOOL_NAME
    );
    assert_eq!(display.tool_type, tau_proto::ToolType::Function);
    assert_eq!(display.kind, ToolResultKind::Final);
    assert_eq!(display.display, None);
    assert_eq!(display.originator, tau_proto::PromptOriginator::User);
}

/// Requires Boot A to persist one closed prompt/response turn before shutdown.
fn assert_restart_boot_a_prefix(snapshot: &DurableSnapshot, prompt: &str, response: &str) {
    assert_eq!(
        snapshot
            .agent_events
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::AgentPromptSubmitted(submitted)
                        if submitted.inference_activation && submitted.text == prompt
                )
            })
            .count(),
        1,
        "Boot A must durably accept its source prompt exactly once"
    );
    assert_eq!(
        snapshot
            .agent_events
            .iter()
            .filter(|record| matches!(&record.event, Event::AgentInferenceDispatchStarted(_)))
            .count(),
        1,
        "Boot A must durably own one source dispatch"
    );
    assert_eq!(
        snapshot
            .agent_events
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::ProviderResponseFinished(finished)
                        if assistant_output_is(&finished.output_items, response)
                )
            })
            .count(),
        1,
        "Boot A must durably close the source response before shutdown"
    );
    assert_eq!(
        snapshot
            .agent_events
            .iter()
            .filter(|record| matches!(&record.event, Event::AgentOuterTurnFinished(_)))
            .count(),
        1,
        "Boot A must durably finish its source outer turn before shutdown"
    );
}

/// Requires the resumed provider context to contain the exact completed
/// user/assistant prefix, recovery notice, and one submitted successor input.
fn assert_restart_context(context: &tau_proto::PromptContext, h: &str, r: &str, q: &str) {
    let items = context.flatten_iter().collect::<Vec<_>>();
    let [
        ContextItem::Message(h_item),
        ContextItem::Message(r_item),
        ContextItem::Message(restore_item),
        ContextItem::Message(q_item),
    ] = items.as_slice()
    else {
        panic!("expected typed H, R, restore, Q context items, got {items:?}");
    };
    assert_exact_text_message(
        h_item,
        tau_proto::ContextRole::User,
        &format!("<user>{h}</user>"),
    );
    assert_exact_text_message(r_item, tau_proto::ContextRole::Assistant, r);
    assert_exact_text_message(
        restore_item,
        tau_proto::ContextRole::User,
        &format!("<tau_internal>{RESTORE_NOTICE}</tau_internal>"),
    );
    assert_exact_text_message(
        q_item,
        tau_proto::ContextRole::User,
        &format!("<user>{q}</user>"),
    );
}

/// Requires one message to have exactly one typed text part with the given role
/// and content.
fn assert_exact_text_message(
    message: &tau_proto::MessageItem,
    role: tau_proto::ContextRole,
    text: &str,
) {
    assert_eq!(message.role, role);
    let [tau_proto::ContentPart::Text { text: actual }] = message.content.as_slice() else {
        panic!("expected one typed text part, got {:?}", message.content);
    };
    assert_eq!(actual, text);
}

/// Requires the cold-resume journal suffix to add exactly one correlated
/// successor prompt, dispatch, and terminal without replaying the source.
fn assert_restart_successor_suffix(
    before: &DurableSnapshot,
    after: &DurableSnapshot,
    successor_prompt_id: &tau_proto::AgentPromptId,
    successor_text: &str,
) {
    let suffix = &after.agent_events[before.agent_events.len()..];
    let submitted = suffix
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentPromptSubmitted(submitted) => Some(submitted),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [restore, submitted] = submitted.as_slice() else {
        panic!("expected a durable restore notice and one successor input, got {submitted:?}");
    };
    assert!(!restore.inference_activation);
    assert_eq!(restore.text, RESTORE_NOTICE);
    assert_eq!(
        restore.message_class,
        tau_proto::PromptMessageClass::Internal
    );
    assert_eq!(
        restore.submission_source,
        tau_proto::PromptSubmissionSource::HarnessInternal
    );
    assert!(submitted.inference_activation);
    assert_eq!(submitted.text, successor_text);

    let dispatches = suffix
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentInferenceDispatchStarted(dispatch) => Some(dispatch),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [dispatch] = dispatches.as_slice() else {
        panic!("expected exactly one durable successor dispatch, got {dispatches:?}");
    };
    assert_eq!(&dispatch.agent_prompt_id, successor_prompt_id);

    let starts = suffix
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentPromptStarted(started) => Some(started),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [started] = starts.as_slice() else {
        panic!("expected exactly one durable successor prompt start, got {starts:?}");
    };
    assert_eq!(&started.agent_prompt_id, successor_prompt_id);

    let terminals = suffix
        .iter()
        .filter_map(|record| match &record.event {
            Event::ProviderResponseFinished(finished) => Some(finished),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [terminal] = terminals.as_slice() else {
        panic!("expected exactly one durable successor terminal, got {terminals:?}");
    };
    assert_eq!(&terminal.agent_prompt_id, successor_prompt_id);

    assert_eq!(
        after
            .agent_events
            .iter()
            .filter(|record| matches!(&record.event, Event::AgentInferenceDispatchStarted(_)))
            .count(),
        2,
        "cold replay must not resend the source or dispatch a third turn"
    );
}

/// Returns whether one terminal contains exactly the expected assistant text.
fn assistant_output_is(items: &[ContextItem], expected: &str) -> bool {
    matches!(
        items,
        [ContextItem::Message(message)]
            if message.content
                == [tau_proto::ContentPart::Text {
                    text: expected.to_owned()
                }]
    )
}

/// Requires one durable assistant message carrying the expected final text.
fn assert_durable_assistant(snapshot: &DurableSnapshot, expected: &str) {
    assert!(snapshot.agent_events.iter().any(|record| {
        matches!(
            &record.event,
            Event::ProviderResponseFinished(finished)
                if matches!(
                    finished.output_items.as_slice(),
                    [ContextItem::Message(message)]
                        if message.content
                            == [tau_proto::ContentPart::Text {
                                text: expected.to_owned()
                            }]
                )
        )
    }));
}

fn assert_exact_extensions(events: &[tau_proto::Event], expected: &[&str]) {
    let mut actual = events
        .iter()
        .filter_map(|event| match event {
            tau_proto::Event::ExtensionStarting(starting) => Some(starting.extension_name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    actual.sort_unstable();
    assert_eq!(actual, expected, "unexpected active extension set");
}

fn assert_one_fake_model(events: &[tau_proto::Event]) {
    let snapshots = events
        .iter()
        .filter_map(|event| match event {
            tau_proto::Event::ProviderModelsUpdated(snapshot) => Some(snapshot),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].models.len(), 1);
    assert_eq!(snapshots[0].models[0].id.provider.as_str(), "fake");
    assert_eq!(snapshots[0].models[0].id.model.as_str(), "test");
}

fn assert_text_provider_sequence(events: &[tau_proto::Event]) {
    assert_one_fake_model(events);
    let lifecycle = provider_lifecycle(events);
    let [
        ProviderLifecycle::Submitted(submitted),
        ProviderLifecycle::Updated(first),
        ProviderLifecycle::Updated(second),
        ProviderLifecycle::Finished(finished),
    ] = lifecycle.as_slice()
    else {
        panic!("unexpected text provider lifecycle: {lifecycle:?}");
    };
    let id = &submitted.agent_prompt_id;
    assert_eq!(&first.agent_prompt_id, id);
    assert_eq!(&second.agent_prompt_id, id);
    assert_eq!(&finished.agent_prompt_id, id);
    assert_update(first, "hello determ");
    assert_update(second, "inistic tau");
    assert_eq!(finished.stop_reason, tau_proto::ProviderStopReason::EndTurn);
    assert_assistant(&finished.output_items, "hello deterministic tau");
}

fn assert_tool_provider_sequence(events: &[tau_proto::Event]) {
    assert_one_fake_model(events);
    let lifecycle = provider_lifecycle(events);
    let [
        ProviderLifecycle::Submitted(first_submitted),
        ProviderLifecycle::Finished(first_finished),
        ProviderLifecycle::Submitted(second_submitted),
        ProviderLifecycle::Finished(second_finished),
    ] = lifecycle.as_slice()
    else {
        panic!("unexpected tool provider lifecycle: {lifecycle:?}");
    };
    assert_eq!(
        first_submitted.agent_prompt_id,
        first_finished.agent_prompt_id
    );
    assert_eq!(
        second_submitted.agent_prompt_id,
        second_finished.agent_prompt_id
    );
    assert_ne!(
        first_submitted.agent_prompt_id,
        second_submitted.agent_prompt_id
    );
    assert_eq!(
        first_finished.stop_reason,
        tau_proto::ProviderStopReason::ToolCalls
    );
    assert_eq!(
        first_finished.output_items,
        vec![tau_proto::ContextItem::ToolCall(
            outcome_tool_call_projection()
        )]
    );
    assert_eq!(
        second_finished.stop_reason,
        tau_proto::ProviderStopReason::EndTurn
    );
    assert_assistant(&second_finished.output_items, "tool completed");
}

#[derive(Debug)]
enum ProviderLifecycle<'a> {
    /// Provider accepted one exact prompt id.
    Submitted(&'a tau_proto::ProviderPromptSubmitted),
    /// Provider emitted one streamed update for that prompt.
    Updated(&'a tau_proto::ProviderResponseUpdated),
    /// Provider emitted one terminal response.
    Finished(&'a tau_proto::ProviderResponseFinished),
}

fn provider_lifecycle(events: &[tau_proto::Event]) -> Vec<ProviderLifecycle<'_>> {
    events
        .iter()
        .filter_map(|event| match event {
            tau_proto::Event::ProviderPromptSubmitted(value) => {
                Some(ProviderLifecycle::Submitted(value))
            }
            tau_proto::Event::ProviderResponseUpdated(value) => {
                Some(ProviderLifecycle::Updated(value))
            }
            tau_proto::Event::ProviderResponseFinished(value) => {
                Some(ProviderLifecycle::Finished(value))
            }
            _ => None,
        })
        .collect()
}

fn assert_output_length_provider_sequence(events: &[Event], reasoning: &str, answer: &str) {
    let lifecycle = provider_lifecycle(events);
    let [
        ProviderLifecycle::Submitted(first_request),
        ProviderLifecycle::Finished(source),
        ProviderLifecycle::Submitted(second_request),
        ProviderLifecycle::Finished(successor),
    ] = lifecycle.as_slice()
    else {
        panic!("unexpected output-length provider lifecycle: {lifecycle:?}");
    };
    assert_ne!(
        first_request.agent_prompt_id,
        second_request.agent_prompt_id
    );
    assert_eq!(source.agent_prompt_id, first_request.agent_prompt_id);
    assert_eq!(successor.agent_prompt_id, second_request.agent_prompt_id);
    assert_eq!(source.stop_reason, tau_proto::ProviderStopReason::Length);
    assert!(matches!(
        source.output_items.as_slice(),
        [ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Full,
            text,
        })] if text == reasoning
    ));
    assert!(matches!(
        source.output_length_disposition,
        tau_proto::OutputLengthDisposition::ContinuationPlanned {
            ordinal: 1,
            limit: 1,
            ..
        }
    ));
    assert_eq!(
        successor.stop_reason,
        tau_proto::ProviderStopReason::EndTurn
    );
    assert_assistant(&successor.output_items, answer);
    assert!(matches!(
        successor.output_length_disposition,
        tau_proto::OutputLengthDisposition::ContinuationTerminal {
            ordinal: 1,
            outcome: tau_proto::OutputLengthContinuationOutcome::Completed,
            ..
        }
    ));
    let source_usage = source.usage.as_ref().expect("source usage");
    assert_eq!(source_usage.prompt_sent_tokens, 10);
    assert_eq!(source_usage.prompt_cached_tokens, 2);
    assert_eq!(source_usage.response_received_tokens, 3);
    assert_eq!(source_usage.stats.total.requests, 1);
    assert_eq!(source_usage.stats.total.sent_tokens, 10);
    assert_eq!(source_usage.stats.total.cached_tokens, 2);
    assert_eq!(source_usage.stats.total.received_tokens, 3);
    let successor_usage = successor.usage.as_ref().expect("successor usage");
    assert_eq!(successor_usage.prompt_sent_tokens, 20);
    assert_eq!(successor_usage.prompt_cached_tokens, 5);
    assert_eq!(successor_usage.response_received_tokens, 7);
    assert_eq!(successor_usage.stats.total.requests, 2);
    assert_eq!(successor_usage.stats.total.sent_tokens, 30);
    assert_eq!(successor_usage.stats.total.cached_tokens, 7);
    assert_eq!(successor_usage.stats.total.received_tokens, 10);
    for response in [source, successor] {
        let rates = response
            .estimated_api_cost_rates
            .expect("captured nonzero rates");
        assert_eq!(rates.uncached_input.as_micro_usd(), 2_000_000);
        assert_eq!(rates.cached_input.as_micro_usd(), 1_000_000);
        assert_eq!(rates.output.as_micro_usd(), 4_000_000);
    }
    // Each accepted response keeps its own backend, response id, and cache
    // observation; the pair must never collapse into one shared record.
    assert_eq!(
        source.backend.as_ref().map(|backend| &backend.kind),
        Some(&tau_proto::ProviderBackendKind::ChatCompletions)
    );
    assert_eq!(
        successor.backend.as_ref().map(|backend| &backend.kind),
        Some(&tau_proto::ProviderBackendKind::ChatCompletions)
    );
    assert_eq!(
        source.provider_response_id.as_deref(),
        Some("resp-output-length-source")
    );
    assert_eq!(
        successor.provider_response_id.as_deref(),
        Some("resp-output-length-successor")
    );
    assert_ne!(source.provider_response_id, successor.provider_response_id);
    assert_ne!(
        source_usage.prompt_cached_tokens,
        successor_usage.prompt_cached_tokens
    );
    assert_eq!(
        source
            .estimated_api_cost_increment
            .expect("source cost")
            .as_picodollars(),
        30_000_000
    );
    assert_eq!(
        successor
            .estimated_api_cost_increment
            .expect("successor cost")
            .as_picodollars(),
        63_000_000
    );
    let successor_position = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == successor.agent_prompt_id
            )
        })
        .expect("successor response position");
    let final_stats = events[successor_position + 1..]
        .iter()
        .filter_map(|event| match event {
            Event::AgentStatsUpdated(stats) if stats.agent_id == successor.agent_id => Some(stats),
            _ => None,
        })
        .next_back()
        .expect("final post-successor stats");
    assert_eq!(
        final_stats.estimated_api_cost.as_picodollars(),
        93_000_000,
        "response increments contribute to the final aggregate exactly once"
    );
}

fn assert_output_length_durable_sequence(snapshot: &DurableSnapshot, answer: &str) {
    let position = |predicate: &dyn Fn(&Event) -> bool| {
        snapshot
            .agent_events
            .iter()
            .position(|record| predicate(&record.event))
            .expect("required durable output-length fact")
    };
    let source = position(&|event| {
        matches!(
            event,
            Event::ProviderResponseFinished(response)
                if matches!(
                    response.output_length_disposition,
                    tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
                )
        )
    });
    let steer = position(&|event| {
        matches!(
            event,
            Event::AgentPromptSteered(steer)
                if steer.internal_kind
                    == Some(tau_proto::InternalPromptKind::OutputLengthContinuation)
                    && steer.text == tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION
        )
    });
    let owner = position(&|event| {
        matches!(
            event,
            Event::AgentInferenceDispatchStarted(owner)
                if owner.output_length_continuation.is_some()
        )
    });
    let start = position(&|event| {
        matches!(
            event,
            Event::AgentPromptStarted(started)
                if snapshot.agent_events.iter().any(|record| matches!(
                    &record.event,
                    Event::AgentInferenceDispatchStarted(owner)
                        if owner.output_length_continuation.is_some()
                            && owner.agent_prompt_id == started.agent_prompt_id
                ))
        )
    });
    let terminal = position(&|event| {
        matches!(
            event,
            Event::ProviderResponseFinished(response)
                if matches!(
                    response.output_length_disposition,
                    tau_proto::OutputLengthDisposition::ContinuationTerminal { .. }
                )
                    && response.output_items.iter().any(|item| matches!(
                        item,
                        ContextItem::Message(message)
                            if message.content == [tau_proto::ContentPart::Text {
                                text: answer.to_owned()
                            }]
                    ))
        )
    });
    let finish = position(&|event| matches!(event, Event::AgentOuterTurnFinished(_)));
    assert!(
        source < steer && steer < owner && owner < start && start < terminal && terminal < finish
    );
    let durable_responses = snapshot
        .agent_events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ProviderResponseFinished(response) => Some(response),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(durable_responses.len(), 2);
    assert_eq!(
        durable_responses[0].provider_response_id.as_deref(),
        Some("resp-output-length-source"),
        "the cold snapshot preserves each response's own provider response id"
    );
    assert_eq!(
        durable_responses[1].provider_response_id.as_deref(),
        Some("resp-output-length-successor")
    );
    assert_ne!(
        durable_responses[0].provider_response_id,
        durable_responses[1].provider_response_id
    );
    assert!(durable_responses.iter().all(|response| {
        response
            .backend
            .as_ref()
            .is_some_and(|backend| backend.kind == tau_proto::ProviderBackendKind::ChatCompletions)
    }));
    let durable_cached = durable_responses
        .iter()
        .map(|response| {
            response
                .usage
                .as_ref()
                .expect("reported usage survives cold reload")
                .prompt_cached_tokens
        })
        .collect::<Vec<_>>();
    assert_eq!(durable_cached, vec![2, 5]);
    assert_eq!(
        snapshot
            .agent_events
            .iter()
            .filter(|record| matches!(
                record.event,
                Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
                    internal_kind: Some(tau_proto::InternalPromptKind::OutputLengthContinuation),
                    ..
                })
            ))
            .count(),
        1
    );
}

fn outcome_tool_call_projection() -> tau_proto::ToolCallItem {
    tau_proto::ToolCallItem {
        call_id: "fake-call-1".into(),
        name: tau_proto::ToolName::new("restart_test_dummy"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
        raw_arguments_json: Some("{}".to_owned()),
        responses_envelope: None,
    }
}

fn assert_update(update: &tau_proto::ProviderResponseUpdated, expected: &str) {
    match update.deltas.as_slice() {
        [
            tau_proto::ProviderResponseTextDelta::Message {
                output_index,
                text,
                phase,
            },
        ] => {
            assert_eq!(*output_index, 0);
            assert_eq!(text, expected);
            assert_eq!(*phase, None);
        }
        other => panic!("unexpected update deltas: {other:?}"),
    }
}

fn assert_assistant(items: &[tau_proto::ContextItem], expected: &str) {
    match items {
        [tau_proto::ContextItem::Message(message)] => {
            assert_eq!(message.role, tau_proto::ContextRole::Assistant);
            assert_eq!(message.phase, None);
            assert_eq!(message.responses_raw_json, None);
            assert_eq!(
                message.content,
                vec![tau_proto::ContentPart::Text {
                    text: expected.to_owned()
                }]
            );
        }
        other => panic!("unexpected assistant output: {other:?}"),
    }
}

fn assistant_text(items: &[tau_proto::ContextItem]) -> String {
    match items {
        [tau_proto::ContextItem::Message(message)] => message
            .content
            .iter()
            .map(|part| match part {
                tau_proto::ContentPart::Text { text }
                | tau_proto::ContentPart::SyntheticCompactionSummary { text }
                | tau_proto::ContentPart::HarnessInternalText { text } => text.as_str(),
                tau_proto::ContentPart::UrlCitation { .. }
                | tau_proto::ContentPart::CitationMetadataInvalid => "",
            })
            .collect(),
        other => panic!("unexpected assistant output: {other:?}"),
    }
}
