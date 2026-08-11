use std::time::{Duration, Instant};

use tau_e2e_tests::{
    DeterministicFixture, InitialStatusOutcome, ScenarioActionV2, ScenarioLaneV2, ScenarioV1,
    ScenarioV2, StatusTerminalPhase, StatusToolOrder,
};
use tau_proto::{CborValue, Event, ProviderFailureKind, ToolResultKind};

#[path = "deterministic_provider/daemon_support.rs"]
mod daemon_support;
#[path = "deterministic_provider/session_restore.rs"]
mod session_restore;
#[path = "deterministic_provider/standalone_compaction.rs"]
mod standalone_compaction;

use daemon_support::*;

const FAKE_PROVIDER: &str = env!("CARGO_BIN_EXE_tau-e2e-fake-provider");
const DUMMY_TOOL: &str = env!("CARGO_BIN_EXE_tau-e2e-test-dummy");
const HARNESS_DAEMON: &str = env!("CARGO_BIN_EXE_tau-e2e-harness-daemon");

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
                | tau_proto::ContentPart::HarnessInternalText { text } => text.as_str(),
            })
            .collect(),
        other => panic!("unexpected assistant output: {other:?}"),
    }
}
