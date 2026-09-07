//! Durable multi-agent acceptance for live delivery plus cold worker
//! restoration, watch recreation, and membership composition.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;
use tau_e2e_tests::{
    AgentWatchResultExpectationV2, DeterministicFixture, DurableSessionSnapshot, ScenarioActionV2,
    ScenarioLaneV2, ScenarioV2, WatchNotificationV2,
};
use tau_proto::{
    AgentId, AgentMessageKind, AgentNavigationMode, AgentRuntimeState, AgentWatchUpdateCause,
    Event, SessionAgentFacts, SessionAgentLifecycle, SessionAgentListEntry, SessionAgentListScope,
    SessionAgentPersistence, SessionId,
};

use super::daemon_support::{
    OutputLengthCrashCut, disconnect_ui, spawn_daemon, spawn_daemon_at_output_length_cut,
};
use super::persistence_barrier::PersistenceBarrier;
use super::{DUMMY_TOOL, FAKE_PROVIDER};

/// Maximum accepted size of the fake provider's durable cursor checkpoint.
const MAX_FAKE_CURSOR_CHECKPOINT_BYTES: u64 = 64 * 1024;

/// Decoded fake-provider cursor state used to prove replay consumes no action.
#[derive(Debug, Deserialize, PartialEq, Eq)]
struct FakeCursorCheckpoint {
    /// Next action index for each configured scenario lane.
    cursors: Vec<usize>,
}

#[path = "session_restore/dispatch_uncertain.rs"]
mod dispatch_uncertain;
#[path = "session_restore/interrupted_tool.rs"]
mod interrupted_tool;
#[path = "session_restore/interruption_support.rs"]
mod interruption_support;
#[path = "session_restore/membership.rs"]
mod membership;
#[path = "session_restore/mixed_state.rs"]
mod mixed_state;
#[path = "session_restore/multiple_workers.rs"]
mod multiple_workers;
#[path = "session_restore/observer.rs"]
mod observer;
use observer::{Observed, SessionRestoreObserver};

const SESSION: &str = "deterministic-e2e-session";

/// Planned-response and steer crash cuts recover only the missing durable step,
/// then issue exactly one reserved successor after restart. The absent-usage
/// variant proves no fabricated counters or cost after cold reload, while the
/// reported-usage variant proves the same nonzero totals recompute after
/// restart.
#[test]
fn deterministic_output_length_recovers_planned_and_steer_cuts()
-> Result<(), Box<dyn std::error::Error>> {
    for cut in [
        OutputLengthCrashCut::PlannedResponse,
        OutputLengthCrashCut::ContinuationSteer,
    ] {
        run_output_length_restart_cut(cut, false)?;
    }
    // One reported-usage restart subcase proves cold reload recomputes the
    // same nonzero accounting totals as the live run.
    run_output_length_restart_cut(OutputLengthCrashCut::PlannedResponse, true)?;
    Ok(())
}

fn run_output_length_restart_cut(
    cut: OutputLengthCrashCut,
    report_usage: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    const USER: &str = "finish the restart-bounded answer";
    const REASONING: &str = "retained restart plan";
    const ANSWER: &str = "completed after restart continuation";
    let scenario = ScenarioV2::new(
        format!("output-length-{cut:?}-usage-{report_usage}"),
        vec![ScenarioLaneV2 {
            ctx_id: "output-length-restart".to_owned(),
            actions: vec![
                ScenarioActionV2::OutputLengthReasoning {
                    user_text: USER.to_owned(),
                    reasoning: REASONING.to_owned(),
                    report_usage,
                },
                ScenarioActionV2::OutputLengthContinuation {
                    user_text: USER.to_owned(),
                    reasoning: REASONING.to_owned(),
                    response: ANSWER.to_owned(),
                    report_usage,
                },
            ],
        }],
    );
    let fixture = DeterministicFixture::new_session_restore(
        &format!("deterministic_output_length_{cut:?}_usage_{report_usage}"),
        &scenario,
        FAKE_PROVIDER,
    )?;
    let session_id: SessionId = SESSION.parse()?;
    let reached = fixture.socket_path(&format!("output-length-{cut:?}-{report_usage}-barrier"));
    let barrier = PersistenceBarrier::bind(&reached, cut)?;
    let socket_a = fixture.socket_path(&format!("output-length-{cut:?}-{report_usage}-a"));
    let mut daemon_a = spawn_daemon_at_output_length_cut(
        &fixture,
        &socket_a,
        tau_harness::SessionLaunchStatus::New,
        cut,
        &reached,
    );
    let mut observer_a = SessionRestoreObserver::connect(&socket_a)?;
    let main = observer_a.create_idle_main()?;
    observer_a.submit(&main, "output-length-restart", USER)?;
    barrier.wait(&mut daemon_a)?;
    let snapshot_a = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    let before = &snapshot_a.agent_events[&main];
    assert_eq!(matched_action_count(&fixture)?, 1);
    assert_eq!(
        before
            .iter()
            .filter(|record| matches!(
                record.event,
                Event::ProviderResponseFinished(tau_proto::ProviderResponseFinished {
                    automatic_compaction_decision: None,
                    output_length_disposition:
                        tau_proto::OutputLengthDisposition::ContinuationPlanned { .. },
                    ..
                })
            ))
            .count(),
        1
    );
    let steer_count = before
        .iter()
        .filter(|record| {
            matches!(
                record.event,
                Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
                    internal_kind: Some(tau_proto::InternalPromptKind::OutputLengthContinuation),
                    ..
                })
            )
        })
        .count();
    assert_eq!(
        steer_count,
        match cut {
            OutputLengthCrashCut::PlannedResponse => 0,
            OutputLengthCrashCut::ContinuationSteer => 1,
            OutputLengthCrashCut::TypedReceiptSenderTerminal => {
                unreachable!("typed-receipt cut is not an output-length case")
            }
            OutputLengthCrashCut::NextProviderResponse => {
                unreachable!("two-response cut is not an output-length case")
            }
        }
    );
    assert!(before.iter().all(|record| !matches!(
        record.event,
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            output_length_continuation: Some(_),
            ..
        })
    )));

    let terminated = daemon_a.kill_ungracefully()?;
    drop(observer_a);
    terminated.require_gone(fixture.harness_state_dir(), session_id.as_str())?;

    let socket_b = fixture.socket_path(&format!("output-length-{cut:?}-{report_usage}-b"));
    let daemon_b = spawn_daemon(
        &fixture,
        &socket_b,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut observer_b = SessionRestoreObserver::connect(&socket_b)?;
    observer_b.wait_for_session_boundary(&session_id)?;
    observer_b.wait_for_agent_marker(&main, ANSWER, 0)?;
    disconnect_ui(&mut observer_b.peer)?;
    daemon_b.finish()?;
    let snapshot_b = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    let after = &snapshot_b.agent_events[&main];
    assert_eq!(
        after
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
    assert_eq!(
        after
            .iter()
            .filter(|record| matches!(
                record.event,
                Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
                    output_length_continuation: Some(_),
                    ..
                })
            ))
            .count(),
        1
    );
    assert_eq!(
        after
            .iter()
            .filter(|record| matches!(record.event, Event::ProviderResponseFinished(_)))
            .count(),
        2
    );
    if !report_usage {
        assert!(after.iter().all(|record| match &record.event {
            Event::ProviderResponseFinished(response) => {
                response.usage.is_none()
                    && response.estimated_api_cost_rates.is_none()
                    && response.estimated_api_cost_increment.is_none()
            }
            _ => true,
        }));
    }
    let final_stats = observer_b
        .events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::AgentStatsUpdated(stats) if stats.agent_id == main => Some(stats),
            _ => None,
        })
        .next_back()
        .expect("final restored agent stats");
    if report_usage {
        // The reported-usage restart must recompute the same nonzero totals as
        // the live run: distinct per-response usage survives, each response
        // keeps its own provider response id, and the aggregate equals the
        // arithmetic sum exactly once.
        let usages = after
            .iter()
            .filter_map(|record| match &record.event {
                Event::ProviderResponseFinished(response) => response.usage.clone(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            usages
                .iter()
                .map(|usage| (
                    usage.prompt_sent_tokens,
                    usage.prompt_cached_tokens,
                    usage.response_received_tokens
                ))
                .collect::<Vec<_>>(),
            vec![(10, 2, 3), (20, 5, 7)]
        );
        assert_eq!(
            after
                .iter()
                .filter_map(|record| match &record.event {
                    Event::ProviderResponseFinished(response) => {
                        response.provider_response_id.clone()
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![
                "resp-output-length-source".to_owned(),
                "resp-output-length-successor".to_owned(),
            ]
        );
        // Runtime-lifetime agent stats restart from zero, so the nonzero
        // cold-reload aggregate comes from the durable per-response cost
        // increments: each response's own increment survives exactly once and
        // sums to the same total as the live run.
        let cost_increments = after
            .iter()
            .filter_map(|record| match &record.event {
                Event::ProviderResponseFinished(response) => response.estimated_api_cost_increment,
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            cost_increments
                .iter()
                .map(|increment| increment.as_picodollars())
                .collect::<Vec<_>>(),
            vec![30_000_000, 63_000_000]
        );
        assert_eq!(
            cost_increments
                .iter()
                .map(|increment| increment.as_picodollars())
                .sum::<u64>(),
            93_000_000,
            "cold reload recomputes the same nonzero total cost"
        );
        assert_eq!(
            usages
                .iter()
                .map(|usage| usage.response_received_tokens)
                .sum::<u64>(),
            10,
            "cold reload recomputes the same nonzero token total"
        );
    } else {
        assert_eq!(final_stats.estimated_api_cost.as_picodollars(), 0);
    }
    // After restart the journal proves no source resend and exactly one
    // reserved successor. A separate accepted post-terminal initialization
    // suffix may dispatch independently of this closed lineage.
    let dispatches = after
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentInferenceDispatchStarted(dispatch) => Some((
                record.seq,
                dispatch.agent_prompt_id.clone(),
                dispatch.output_length_continuation.clone(),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        dispatches
            .iter()
            .filter(|(_, prompt_id, _)| prompt_id.as_str() == "ap-main-0")
            .count(),
        1,
        "restart never resends the source"
    );
    assert_eq!(
        dispatches
            .iter()
            .filter(|(_, _, continuation)| continuation.is_some())
            .count(),
        1,
        "restart dispatches exactly one reserved output-length successor"
    );
    assert_eq!(matched_action_count(&fixture)?, 2);
    fixture.assert_consumed()?;
    Ok(())
}

const WORKER_PROMPT: &str = "Complete the deterministic worker instruction.";
const WORKER_PROVIDER_INITIAL: &str = concat!(
    "<tau_internal>You were started by an agent `main`. Your responses will be delivered to it. ",
    "You can use the `message` tool to communicate with agents.\n\n</tau_internal>",
    "Complete the deterministic worker instruction."
);

/// A held non-tool response durably precedes typed and raw inputs that arrive
/// while its marked owner remains unresolved.
#[test]
fn deterministic_two_turn_provider_context_places_input_after_response()
-> Result<(), Box<dyn std::error::Error>> {
    run_gated_live_provider_context_placement(false)
}

/// A held parallel tool response and its complete aggregate durably precede
/// typed and raw inputs, with one coalesced successor.
#[test]
fn deterministic_parallel_tool_context_places_input_after_aggregate()
-> Result<(), Box<dyn std::error::Error>> {
    run_gated_live_provider_context_placement(true)
}

fn run_gated_live_provider_context_placement(
    parallel_tools: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    const H: &str = "held provider-context history";
    const TYPED: &str = "typed input while provider response is held";
    const RAW: &str = "raw input while provider response is held";
    const RELEASE: &str = "release held provider response";
    const RESPONSE: &str = "held provider response";
    const SUCCESSOR: &str = "deferred inputs observed once";
    let call_id = tau_proto::ToolCallId::new("provider-context-message");
    let tool_call_ids = vec![
        tau_proto::ToolCallId::new("provider-context-tool-a"),
        tau_proto::ToolCallId::new("provider-context-tool-b"),
    ];
    let raw_call_id = tau_proto::ToolCallId::new("provider-context-raw");
    let target_first = if parallel_tools {
        ScenarioActionV2::BarrierParallelDummyTools {
            user_text: H.to_owned(),
            barrier: "provider-context-release".to_owned(),
            participants: 2,
            tool_call_ids: tool_call_ids.clone(),
        }
    } else {
        ScenarioActionV2::BarrierText {
            user_text: H.to_owned(),
            barrier: "provider-context-release".to_owned(),
            participants: 2,
            response: RESPONSE.to_owned(),
        }
    };
    let target_successor = if parallel_tools {
        ScenarioActionV2::MessageAndRawInboundAfterParallelTools {
            call_id: call_id.clone(),
            message: TYPED.to_owned(),
            raw_text: RAW.to_owned(),
            held_user_text: H.to_owned(),
            tool_call_ids: tool_call_ids.clone(),
            response: SUCCESSOR.to_owned(),
        }
    } else {
        ScenarioActionV2::MessageAndRawInboundAfterHeld {
            call_id: call_id.clone(),
            message: TYPED.to_owned(),
            raw_text: RAW.to_owned(),
            held_user_text: H.to_owned(),
            response: SUCCESSOR.to_owned(),
        }
    };
    let name = if parallel_tools {
        "deterministic_parallel_tool_context_places_input_after_aggregate"
    } else {
        "deterministic_two_turn_provider_context_places_input_after_response"
    };
    let fixture = DeterministicFixture::new_provider_context_placement(
        name,
        &ScenarioV2::new(
            "gated-live-provider-context",
            vec![
                ScenarioLaneV2 {
                    ctx_id: "provider-context-sender".to_owned(),
                    actions: vec![
                        ScenarioActionV2::MessageCall {
                            user_text: "send while target response is held".to_owned(),
                            call_id: call_id.clone(),
                            message: TYPED.to_owned(),
                        },
                        ScenarioActionV2::MessageSenderResult {
                            call_id,
                            message: TYPED.to_owned(),
                            response: "sender committed deferred inputs".to_owned(),
                        },
                        ScenarioActionV2::ProviderContextRawMessageCall {
                            user_text: "publish raw input while target remains held".to_owned(),
                            call_id: raw_call_id.clone(),
                            raw_text: RAW.to_owned(),
                        },
                        ScenarioActionV2::ProviderContextRawMessageResultOrBarrier {
                            call_id: raw_call_id,
                            raw_text: RAW.to_owned(),
                            prior_user_text: "publish raw input while target remains held"
                                .to_owned(),
                            response: "raw input committed".to_owned(),
                            release_user_text: RELEASE.to_owned(),
                            barrier: "provider-context-release".to_owned(),
                            participants: 2,
                            barrier_response: "sender released target".to_owned(),
                        },
                        ScenarioActionV2::BarrierText {
                            user_text: RELEASE.to_owned(),
                            barrier: "provider-context-release".to_owned(),
                            participants: 2,
                            response: "sender released target".to_owned(),
                        },
                    ],
                },
                ScenarioLaneV2 {
                    ctx_id: "provider-context-target".to_owned(),
                    actions: vec![target_first, target_successor],
                },
            ],
        ),
        FAKE_PROVIDER,
        DUMMY_TOOL,
    )?;
    let socket = fixture.socket_path(name);
    let daemon = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::New);
    let mut observer = SessionRestoreObserver::connect(&socket)?;
    let sender = observer.create_idle_main()?;
    let target = observer.create_idle_worker(&sender)?;
    let start = observer.events.len();

    observer.submit(&target, "provider-context-target", H)?;
    observer.recv_until(|observed| {
        matches!(
            &observed.event,
            Event::AgentPromptCreated(prompt) if prompt.agent_id == target
        )
    })?;
    observer.submit(
        &sender,
        "provider-context-sender",
        "send while target response is held",
    )?;
    observer.recv_until(|observed| {
        matches!(
            &observed.event,
            Event::AgentMessageReceived(received)
                if received.recipient_id == target && received.message == TYPED
        )
    })?;
    assert!(
        observer.events[start..].iter().all(|observed| {
            !matches!(
                &observed.event,
                Event::ProviderResponseFinished(finished) if finished.agent_id == target
            )
        }),
        "target response remains held until both deferred inputs commit"
    );

    observer.submit(
        &sender,
        "provider-context-raw",
        "publish raw input while target remains held",
    )?;
    observer.recv_until(|observed| {
        matches!(
            &observed.event,
            Event::MessageDelivered(delivered)
                if delivered.agent_id.as_str() == target.as_ref() && delivered.text == RAW
        )
    })?;
    assert!(
        observer.events[start..].iter().all(|observed| {
            !matches!(
                &observed.event,
                Event::ProviderResponseFinished(finished) if finished.agent_id == target
            )
        }),
        "raw input commits before target response release"
    );
    observer.submit(&sender, "provider-context-release", RELEASE)?;
    observer.wait_for_agent_marker(&target, SUCCESSOR, start)?;
    observer.wait_for_agent_idle_after(&target, start)?;

    let prompts = observer.events[start..]
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::AgentPromptCreated(prompt) if prompt.agent_id == target => Some(prompt),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), 2, "one held prompt and one successor");
    let raw_index = observer.events[start..]
        .iter()
        .position(|observed| {
            matches!(
                &observed.event,
                Event::MessageDelivered(delivered) if delivered.text == RAW
            )
        })
        .expect("raw fact committed");
    let response_index = observer.events[start..]
        .iter()
        .position(|observed| {
            matches!(
                &observed.event,
                Event::ProviderResponseFinished(finished) if finished.agent_id == target
            )
        })
        .expect("held response committed");
    assert!(raw_index < response_index);
    let context = prompts[1].context.flatten();
    let rendered = context
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?;
    let position = |needle: &str| {
        rendered
            .iter()
            .position(|item| item.contains(needle))
            .unwrap_or_else(|| panic!("missing provider-context marker {needle}"))
    };
    let typed = position(TYPED);
    let raw = position(RAW);
    assert!(typed < raw);
    assert_eq!(
        rendered.iter().filter(|item| item.contains(TYPED)).count(),
        1
    );
    assert_eq!(rendered.iter().filter(|item| item.contains(RAW)).count(), 1);
    if parallel_tools {
        let last_call = context
            .iter()
            .rposition(|item| matches!(item, tau_proto::ContextItem::ToolCall(_)))
            .expect("parallel calls remain");
        let first_result = context
            .iter()
            .position(|item| matches!(item, tau_proto::ContextItem::ToolResult(_)))
            .expect("parallel aggregate remains");
        let last_result = context
            .iter()
            .rposition(|item| matches!(item, tau_proto::ContextItem::ToolResult(_)))
            .expect("parallel aggregate remains");
        assert_eq!(first_result, last_call + 1);
        assert_eq!(last_result + 1, first_result + tool_call_ids.len());
        assert!(last_result < typed);
    } else {
        assert!(position(RESPONSE) < typed);
    }

    disconnect_ui(&mut observer.peer)?;
    daemon.finish()?;
    let session_id = SessionId::parse(SESSION).expect("session id");
    let snapshot = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    let target_records = &snapshot.agent_events[&target];
    assert_eq!(
        target_records
            .iter()
            .filter(|record| matches!(record.event, Event::AgentInferenceDispatchStarted(_)))
            .count(),
        2,
        "held owner and one successor are the only durable checkpoints"
    );
    assert_eq!(
        target_records
            .iter()
            .filter(|record| {
                matches!(&record.event, Event::AgentMessageReceived(received)
                    if received.message == TYPED)
            })
            .count(),
        1
    );
    assert_eq!(
        target_records
            .iter()
            .filter(|record| {
                matches!(&record.event, Event::MessageDelivered(delivered)
                    if delivered.text == RAW)
            })
            .count(),
        1
    );
    let tree = tau_core::AgentTree::from_events(target.clone(), target_records);
    let node_kinds = tree
        .nodes()
        .iter()
        .filter_map(|node| match &node.entry {
            tau_core::AgentEntry::AssistantResponse { output_items, .. }
                if output_items
                    .iter()
                    .any(|item| matches!(item, tau_proto::ContextItem::ToolCall(_))) =>
            {
                Some("response")
            }
            tau_core::AgentEntry::ToolResults { items } => {
                assert_eq!(
                    items.iter().map(|item| &item.call_id).collect::<Vec<_>>(),
                    tool_call_ids.iter().collect::<Vec<_>>()
                );
                Some("aggregate")
            }
            tau_core::AgentEntry::AgentMessage { message, .. } if message == TYPED => Some("typed"),
            tau_core::AgentEntry::MessageFact { item, .. }
                if serde_json::to_string(item).is_ok_and(|text| text.contains(RAW)) =>
            {
                Some("raw")
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if parallel_tools {
        assert_eq!(
            node_kinds,
            ["response", "aggregate", "typed", "raw"],
            "one complete aggregate sits between response and deferred inputs"
        );
    } else {
        assert_eq!(node_kinds, ["typed", "raw"]);
    }
    fixture.assert_consumed()?;
    Ok(())
}

/// Proves the production message tool persists both owned facts, wakes its
/// idle recipient without creating a HumanUi prompt, and projects one inbound
/// wrapper into that recipient's sole provider turn.
#[test]
fn production_message_tool_delivers_one_canonical_inbound_wrapper()
-> Result<(), Box<dyn std::error::Error>> {
    let session_id = SessionId::parse(SESSION).expect("known-safe session id");
    let body = "complete the isolated message instruction";
    let fixture = DeterministicFixture::new_session_message(
        "production_message_tool_delivers_one_canonical_inbound_wrapper",
        &ScenarioV2::new(
            "production-message-tool",
            vec![
                ScenarioLaneV2 {
                    ctx_id: "message-main".to_owned(),
                    actions: vec![
                        ScenarioActionV2::MessageCall {
                            user_text: "send the worker its isolated instruction".to_owned(),
                            call_id: "message-call".into(),
                            message: body.to_owned(),
                        },
                        ScenarioActionV2::MessageSenderResult {
                            call_id: "message-call".into(),
                            message: body.to_owned(),
                            response: "message sent".to_owned(),
                        },
                        ScenarioActionV2::Text {
                            user_text: "fresh main work".to_owned(),
                            response: "fresh main accepted".to_owned(),
                        },
                    ],
                },
                ScenarioLaneV2 {
                    ctx_id: "message-worker".to_owned(),
                    actions: vec![
                        ScenarioActionV2::MessageInbound {
                            call_id: "message-call".into(),
                            message: body.to_owned(),
                            response: "worker received exactly one message".to_owned(),
                        },
                        ScenarioActionV2::Text {
                            user_text: "fresh worker work".to_owned(),
                            response: "fresh worker accepted".to_owned(),
                        },
                    ],
                },
            ],
        ),
        FAKE_PROVIDER,
    )?;
    let socket = fixture.socket_path("message-tool");
    let daemon = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::New);
    let mut observer = SessionRestoreObserver::connect(&socket)?;
    let main = observer.create_idle_main()?;
    let worker = observer.create_idle_worker(&main)?;
    let start = observer.events.len();
    observer.submit(
        &main,
        "message-main",
        "send the worker its isolated instruction",
    )?;
    observer.wait_for_agent_marker(&main, "message sent", start)?;
    observer.wait_for_agent_marker(&worker, "worker received exactly one message", start)?;
    observer.wait_for_agent_idle_after(&main, start)?;
    observer.wait_for_agent_idle_after(&worker, start)?;

    let dispatches = observer.events[start..]
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::AgentInferenceDispatchStarted(dispatch) => Some(&dispatch.agent_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        dispatches.iter().filter(|id| ***id == main).count(),
        2,
        "main has one call and one correlated continuation"
    );
    assert_eq!(
        dispatches.iter().filter(|id| ***id == worker).count(),
        1,
        "recipient activation has exactly one provider turn"
    );
    let fresh = observer.events.len();
    observer.submit(&main, "fresh-main", "fresh main work")?;
    observer.wait_for_agent_marker(&main, "fresh main accepted", fresh)?;
    observer.submit(&worker, "fresh-worker", "fresh worker work")?;
    observer.wait_for_agent_marker(&worker, "fresh worker accepted", fresh)?;
    disconnect_ui(&mut observer.peer)?;
    daemon.finish()?;
    let snapshot = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    let sent = snapshot.agent_events[&main]
        .iter()
        .filter(|record| matches!(record.event, Event::AgentMessageSent(_)))
        .collect::<Vec<_>>();
    assert!(
        snapshot.agent_events[&main]
            .iter()
            .all(|record| !matches!(record.event, Event::AgentMessageReceived(_))),
        "sender journal must not own an inbound occurrence"
    );
    let received = snapshot.agent_events[&worker]
        .iter()
        .filter(|record| matches!(record.event, Event::AgentMessageReceived(_)))
        .collect::<Vec<_>>();
    assert!(
        snapshot.agent_events[&worker]
            .iter()
            .all(|record| !matches!(record.event, Event::AgentMessageSent(_))),
        "recipient journal must not own an outbound occurrence"
    );
    assert_eq!(sent.len(), 1, "sender owns exactly one sent occurrence");
    assert_eq!(
        received.len(),
        1,
        "recipient owns exactly one received occurrence"
    );
    let sent_index = usize::try_from(sent[0].seq.get()).expect("stored sender sequence fits usize");
    let received_index =
        usize::try_from(received[0].seq.get()).expect("stored recipient sequence fits usize");
    assert_eq!(
        snapshot.agent_events[&main][sent_index].event, sent[0].event,
        "sent fact must retain its exact owning journal sequence"
    );
    assert_eq!(
        snapshot.agent_events[&worker][received_index].event, received[0].event,
        "received fact must retain its exact owning journal sequence"
    );
    let worker_suffix = snapshot.agent_events[&worker]
        .get(received_index..)
        .ok_or("received occurrence is outside the worker journal")?;
    let worker_tree =
        tau_core::AgentTree::from_events(worker.clone(), &snapshot.agent_events[&worker]);
    let received_head = worker_tree
        .nodes()
        .iter()
        .find(|node| {
            matches!(
                node.entry,
                tau_core::AgentEntry::AgentMessage {
                    durable_event_seq,
                    direction: tau_core::AgentMessageDirection::Inbound,
                    ..
                } if durable_event_seq == received[0].seq
            )
        })
        .map(|node| tau_proto::AgentHead::Node(node.id))
        .ok_or("worker tree omitted received-message node")?;
    let pre_receive_tree = tau_core::AgentTree::from_events(
        worker.clone(),
        &snapshot.agent_events[&worker][..received_index],
    );
    let pre_receive_head = pre_receive_tree
        .head()
        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
    let dispatch = worker_suffix
        .iter()
        .skip(1)
        .find_map(|record| match &record.event {
            Event::AgentInferenceDispatchStarted(dispatch) => Some(dispatch),
            _ => None,
        })
        .ok_or("received occurrence lacks a following dispatch checkpoint")?;
    if dispatch.agent_id != worker
        || dispatch.operation != Some(tau_proto::PromptOperation::Inference)
        || dispatch.activation_cut != Some(pre_receive_head)
        || dispatch.through != received_head
    {
        return Err("worker checkpoint does not own the received-message activation".into());
    }
    let (Event::AgentMessageSent(sent), Event::AgentMessageReceived(received)) =
        (&sent[0].event, &received[0].event)
    else {
        unreachable!("filtered typed message facts");
    };
    assert_eq!(sent.message_id, received.message_id);
    assert_eq!(sent.sender_id, main);
    assert_eq!(
        sent.recipient,
        tau_proto::AgentMessageRecipient::Agent {
            agent_id: worker.clone()
        }
    );
    assert_eq!(received.recipient_id, worker);
    assert_eq!(received.sender_id, main);
    assert_eq!(received.sender_session_id, None);
    assert_eq!(sent.kind, tau_proto::AgentMessageKind::Message);
    assert_eq!(received.kind, tau_proto::AgentMessageKind::Message);
    assert_eq!(received.watch_provider_status, None);
    assert_eq!(received.watch_work_status, None);
    assert_eq!(received.watch_long_wait, None);
    assert_eq!(sent.message, body);
    assert_eq!(received.message, body);
    assert!(
        snapshot.agent_events[&worker]
            .iter()
            .all(|record| !matches!(
                &record.event,
                Event::AgentPromptSubmitted(submitted) if submitted.text != "fresh worker work"
            )),
        "delivery must not manufacture a HumanUi prompt"
    );

    fixture.assert_consumed()?;
    Ok(())
}

/// A crash after durable typed receipt but before the held provider response
/// restores as one Stale owner closure and one message-driven successor.
#[test]
fn crash_with_deferred_typed_receipt_stales_owner_and_dispatches_once()
-> Result<(), Box<dyn std::error::Error>> {
    const H: &str = "hold crash target response";
    const BODY: &str = "typed receipt before crash";
    let session_id = SessionId::parse(SESSION).expect("session id");
    let fixture = DeterministicFixture::new_session_message(
        "crash_with_deferred_typed_receipt_stales_owner_and_dispatches_once",
        &ScenarioV2::new(
            "crash-deferred-typed-receipt",
            vec![
                ScenarioLaneV2 {
                    ctx_id: "crash-sender".to_owned(),
                    actions: vec![
                        ScenarioActionV2::MessageCall {
                            user_text: "send before target crash".to_owned(),
                            call_id: "crash-message-call".into(),
                            message: BODY.to_owned(),
                        },
                        ScenarioActionV2::MessageSenderResult {
                            call_id: "crash-message-call".into(),
                            message: BODY.to_owned(),
                            response: "sender committed receipt".to_owned(),
                        },
                    ],
                },
                ScenarioLaneV2 {
                    ctx_id: "crash-target".to_owned(),
                    actions: vec![
                        ScenarioActionV2::HoldUntilCancel {
                            user_text: H.to_owned(),
                            timeout_ms: 10_000,
                        },
                        ScenarioActionV2::MessageInboundAfterHeld {
                            call_id: "crash-message-call".into(),
                            message: BODY.to_owned(),
                            held_user_text: H.to_owned(),
                            response: "target resumed successor".to_owned(),
                        },
                    ],
                },
            ],
        ),
        FAKE_PROVIDER,
    )?;
    let socket_a = fixture.socket_path("typed-crash-a");
    let reached = fixture.socket_path("typed-receipt-sender-terminal-barrier");
    let barrier =
        PersistenceBarrier::bind(&reached, OutputLengthCrashCut::TypedReceiptSenderTerminal)?;
    let mut daemon_a = spawn_daemon_at_output_length_cut(
        &fixture,
        &socket_a,
        tau_harness::SessionLaunchStatus::New,
        OutputLengthCrashCut::TypedReceiptSenderTerminal,
        &reached,
    );
    let mut observer_a = SessionRestoreObserver::connect(&socket_a)?;
    let sender = observer_a.create_idle_main()?;
    let target = observer_a.create_idle_worker(&sender)?;
    observer_a.submit(&target, "crash-target", H)?;
    let owner_prompt_id = loop {
        let observed = observer_a.recv_one()?;
        if let Event::AgentPromptCreated(prompt) = observed.event
            && prompt.agent_id == target
        {
            break prompt.agent_prompt_id;
        }
    };
    observer_a.submit(&sender, "crash-sender", "send before target crash")?;
    let receipt_index = loop {
        let observed = observer_a.recv_one()?;
        if matches!(
            &observed.event,
            Event::AgentMessageReceived(message)
                if message.recipient_id == target && message.message == BODY
        ) {
            break observer_a.events.len() - 1;
        }
    };
    observer_a.wait_for_agent_marker(&sender, "sender committed receipt", receipt_index)?;
    barrier.wait(&mut daemon_a)?;
    let terminated = daemon_a.kill_ungracefully()?;
    drop(observer_a);
    terminated.require_gone(fixture.harness_state_dir(), session_id.as_str())?;
    let snapshot_a = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    let target_before_crash = &snapshot_a.agent_events[&target];
    assert_eq!(matched_action_count(&fixture)?, 3);
    assert_eq!(
        target_before_crash
            .iter()
            .filter(
                |record| matches!(&record.event, Event::AgentMessageReceived(message)
                if message.message == BODY)
            )
            .count(),
        1
    );
    assert!(
        target_before_crash
            .iter()
            .all(|record| !matches!(record.event, Event::AgentPromptTerminated(_)))
    );
    let receipt_index_before = target_before_crash
        .iter()
        .position(|record| matches!(record.event, Event::AgentMessageReceived(_)))
        .expect("receipt before crash");
    assert!(
        target_before_crash[receipt_index_before + 1..]
            .iter()
            .all(|record| !matches!(record.event, Event::AgentInferenceDispatchStarted(_)))
    );
    let socket_b = fixture.socket_path("typed-crash-b");
    let reached_b = fixture.socket_path("typed-receipt-resume-barrier");
    let barrier_b =
        PersistenceBarrier::bind(&reached_b, OutputLengthCrashCut::NextProviderResponse)?;
    let mut daemon_b = spawn_daemon_at_output_length_cut(
        &fixture,
        &socket_b,
        tau_harness::SessionLaunchStatus::Resumed,
        OutputLengthCrashCut::NextProviderResponse,
        &reached_b,
    );
    barrier_b.wait(&mut daemon_b)?;
    let terminated = daemon_b.kill_ungracefully()?;
    terminated.require_gone(fixture.harness_state_dir(), session_id.as_str())?;
    let snapshot_b = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    let target_records = &snapshot_b.agent_events[&target];
    let receipts = target_records
        .iter()
        .enumerate()
        .filter(|(_, record)| {
            matches!(&record.event, Event::AgentMessageReceived(message)
                if message.message == BODY)
        })
        .collect::<Vec<_>>();
    assert_eq!(receipts.len(), 1);
    let stales = target_records
        .iter()
        .enumerate()
        .filter(|(_, record)| {
            matches!(&record.event, Event::AgentPromptTerminated(value)
            if value.agent_prompt_id == owner_prompt_id
                && value.reason == tau_proto::AgentPromptTerminationReason::Stale)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        stales.len(),
        1,
        "expected stale owner in {:?}",
        target_records
            .iter()
            .map(|record| (record.seq, record.event.name()))
            .collect::<Vec<_>>()
    );
    let successor_checkpoints = target_records
        .iter()
        .enumerate()
        .filter(|(index, record)| {
            stales[0].0 < *index && matches!(record.event, Event::AgentInferenceDispatchStarted(_))
        })
        .collect::<Vec<_>>();
    assert_eq!(successor_checkpoints.len(), 1);
    assert!(receipts[0].0 < stales[0].0);
    let tree = tau_core::AgentTree::try_from_events(target.clone(), target_records)
        .expect("restored target fold");
    let receipt_node = tree
        .node_for_durable_event_seq(receipts[0].1.seq)
        .expect("receipt node");
    let Event::AgentInferenceDispatchStarted(successor) = &successor_checkpoints[0].1.event else {
        unreachable!("filtered successor checkpoint")
    };
    assert_eq!(successor.through, tau_proto::AgentHead::Node(receipt_node));
    assert_eq!(
        successor.activation_cut,
        Some(
            tree.node(receipt_node)
                .and_then(|node| node.parent_id)
                .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node)
        )
    );
    assert!(
        target_records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentInferenceDispatchStarted(dispatch)
                    if dispatch.agent_prompt_id == owner_prompt_id
            ))
            .count()
            == 1,
        "the uncertain owner is never resent in the durable suffix"
    );
    Ok(())
}

/// Return one agent's post-initialization resume suffix after validating the
/// shared durable snapshot prefix.
fn suffix_after_initialization<'a>(
    before: &DurableSessionSnapshot,
    after: &'a DurableSessionSnapshot,
    agent_id: &AgentId,
) -> Result<&'a [tau_core::PersistedAgentEvent], Box<dyn std::error::Error>> {
    let before_events = &before.agent_events[agent_id];
    let after_events = &after.agent_events[agent_id];
    suffix_after_initialization_events(before_events, after_events, agent_id, &after.session_id)
}

/// Require one fresh, content-equivalent initialization fact after an exact
/// event prefix and return only later agent-owned work.
fn suffix_after_initialization_events<'a>(
    before_events: &[tau_core::PersistedAgentEvent],
    after_events: &'a [tau_core::PersistedAgentEvent],
    agent_id: &AgentId,
    session_id: &SessionId,
) -> Result<&'a [tau_core::PersistedAgentEvent], Box<dyn std::error::Error>> {
    if !after_events.starts_with(before_events) {
        return Err(format!("{agent_id} journal prefix changed across resume").into());
    }
    let Some((initialization, suffix)) = after_events[before_events.len()..].split_first() else {
        return Err(format!("{agent_id} omitted its exact resume initialization fact").into());
    };
    let Event::AgentInitializationContextSet(current) = &initialization.event else {
        return Err(format!("{agent_id} resume suffix did not start with initialization").into());
    };
    let previous = before_events
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            Event::AgentInitializationContextSet(context) => Some(context),
            _ => None,
        })
        .ok_or_else(|| format!("{agent_id} lacks its prior initialization fact"))?;
    let tau_proto::AgentInitializationContextSet {
        session_id: current_session_id,
        agent_id: current_agent_id,
        agent_initialization_id: current_initialization_id,
        agents_message: current_agents_message,
        effective_skills: current_effective_skills,
        agents_files: current_agents_files,
    } = current;
    let tau_proto::AgentInitializationContextSet {
        session_id: previous_session_id,
        agent_id: previous_agent_id,
        agent_initialization_id: previous_initialization_id,
        agents_message: previous_agents_message,
        effective_skills: previous_effective_skills,
        agents_files: previous_agents_files,
    } = previous;
    if current_agent_id != agent_id || current_session_id != session_id {
        return Err(format!("{agent_id} resume initialization has the wrong owner").into());
    }
    if current_initialization_id == previous_initialization_id
        || current_session_id != previous_session_id
        || current_agent_id != previous_agent_id
        || current_agents_message != previous_agents_message
        || current_effective_skills != previous_effective_skills
        || current_agents_files != previous_agents_files
    {
        return Err(format!("{agent_id} did not append a fresh equivalent initialization").into());
    }
    Ok(suffix)
}

/// Require a resume to preserve session-owned streams and append only one
/// validated initialization replacement to each loaded agent.
fn assert_initialization_only_refresh(
    before: &DurableSessionSnapshot,
    after: &DurableSessionSnapshot,
) -> Result<(), Box<dyn std::error::Error>> {
    if after.session_events != before.session_events
        || after.restore_events != before.restore_events
        || after.agent_events.keys().collect::<BTreeSet<_>>()
            != before.agent_events.keys().collect::<BTreeSet<_>>()
    {
        return Err("resume changed session, restore, or agent membership state".into());
    }
    for agent_id in before.agent_events.keys() {
        if !suffix_after_initialization(before, after, agent_id)?.is_empty() {
            return Err(format!("{agent_id} appended state beyond initialization refresh").into());
        }
    }
    Ok(())
}

/// Builds the shared S1/S3 production-worker grammar with scenario-local
/// correlation identifiers.
fn production_worker_scenario(name: &str, prefix: &str) -> ScenarioV2 {
    ScenarioV2::new(
        name,
        vec![
            ScenarioLaneV2 {
                ctx_id: format!("{prefix}-main"),
                actions: vec![
                    ScenarioActionV2::AgentStartCall {
                        user_text: "start the deterministic worker".to_owned(),
                        call_id: format!("{prefix}-agent-start").into(),
                        prompt: WORKER_PROMPT.to_owned(),
                        role: "deterministic-worker".to_owned(),
                    },
                    ScenarioActionV2::AgentStartResult {
                        user_text: "start the deterministic worker".to_owned(),
                        call_id: format!("{prefix}-agent-start").into(),
                        response: "worker start accepted".to_owned(),
                    },
                    ScenarioActionV2::WatchNotifications {
                        notifications: vec![WatchNotificationV2::Response {
                            content: "worker boot-a complete".to_owned(),
                        }],
                        response: "worker completion observed".to_owned(),
                    },
                    ScenarioActionV2::Text {
                        user_text: "fresh main work".to_owned(),
                        response: "fresh main complete".to_owned(),
                    },
                ],
            },
            ScenarioLaneV2 {
                ctx_id: format!("{prefix}-worker"),
                actions: vec![
                    ScenarioActionV2::Text {
                        user_text: WORKER_PROVIDER_INITIAL.to_owned(),
                        response: "worker boot-a complete".to_owned(),
                    },
                    ScenarioActionV2::Text {
                        user_text: "fresh worker work".to_owned(),
                        response: "fresh worker complete".to_owned(),
                    },
                ],
            },
        ],
    )
}

/// Proves cold resume restores a production-started completed worker as a
/// durable, independently addressable conversation without restoring its watch.
#[test]
fn cold_resume_restores_completed_production_worker() -> Result<(), Box<dyn std::error::Error>> {
    let session_id = SessionId::parse(SESSION).expect("known-safe SessionId must be valid");
    let fixture = DeterministicFixture::new_session_restore(
        "cold_resume_restores_completed_production_worker",
        &production_worker_scenario("s1-quiescent-main-completed-worker", "s1"),
        FAKE_PROVIDER,
    )?;
    fixture.assert_session_restore_roles()?;

    let socket_a = fixture.socket_path("s1-boot-a");
    let daemon_a = spawn_daemon(&fixture, &socket_a, tau_harness::SessionLaunchStatus::New);
    let mut observer_a = SessionRestoreObserver::connect(&socket_a)?;
    observer_a.create_main("s1-main", "start the deterministic worker")?;
    observer_a.wait_for_marker("worker completion observed")?;
    observer_a.wait_for_two_idle_agents()?;
    let identities = BootIdentities::from_events(&observer_a.events)?;
    assert_boot_a_lifecycle(&observer_a.events, &identities, &session_id)?;
    assert_provider_turn_counts(
        &observer_a.events,
        &identities,
        ProviderTurnCounts { main: 3, worker: 1 },
    )?;
    disconnect_ui(&mut observer_a.peer)?;
    daemon_a.finish()?;
    let boot_a_action_matches = matched_action_count(&fixture)?;
    assert_eq!(
        boot_a_action_matches, 4,
        "S1 Boot A must consume exactly three main and one worker action"
    );
    let boot_a_cursor = fake_provider_cursor(&fixture)?;
    assert_eq!(
        boot_a_cursor.cursors,
        [3, 1],
        "S1 Boot A must persist the exact next-action cursors"
    );

    let snapshot_a = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    assert_durable_boot_a(&snapshot_a, &identities)?;

    let socket_b = fixture.socket_path("s1-boot-b");
    let daemon_b = spawn_daemon(
        &fixture,
        &socket_b,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut observer_b = SessionRestoreObserver::connect(&socket_b)?;
    observer_b.wait_for_session_boundary(&session_id)?;
    assert_resume_boundaries(&observer_b.events, &identities.all(), &session_id)?;
    assert_replay_is_observational(&observer_b.events, &identities)?;
    assert_eq!(
        fixture
            .trace()?
            .lines()
            .filter(|line| line.contains(" matched "))
            .count(),
        boot_a_action_matches,
        "cold replay must not consume a fake-provider lane action"
    );
    assert_eq!(
        fake_provider_cursor(&fixture)?,
        boot_a_cursor,
        "S1 cold replay must preserve the exact fake-provider cursor checkpoint"
    );

    let current = observer_b.roster(&session_id, SessionAgentListScope::Current)?;
    let history = observer_b.roster(&session_id, SessionAgentListScope::History)?;
    assert_restored_roster(&current, &identities)?;
    assert_eq!(history, current);

    let fresh_start = observer_b.events.len();
    observer_b.submit(&identities.worker, "fresh-worker", "fresh worker work")?;
    observer_b.wait_for_agent_marker(&identities.worker, "fresh worker complete", fresh_start)?;
    observer_b.wait_for_agent_idle_after(&identities.worker, fresh_start)?;
    assert_no_live_watch_refanout(&observer_b.events[fresh_start..], &identities)?;
    observer_b.submit(&identities.main, "fresh-main", "fresh main work")?;
    observer_b.wait_for_agent_marker(&identities.main, "fresh main complete", fresh_start)?;
    observer_b.wait_for_agent_idle_after(&identities.main, fresh_start)?;
    assert_no_live_watch_refanout(&observer_b.events[fresh_start..], &identities)?;
    assert_fresh_work_after_boundaries(&observer_b.events, &identities, &session_id)?;
    assert_provider_turn_counts(
        &observer_b.events,
        &identities,
        ProviderTurnCounts { main: 1, worker: 1 },
    )?;
    disconnect_ui(&mut observer_b.peer)?;
    daemon_b.finish()?;

    let snapshot_b = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    snapshot_b.require_prefix(&snapshot_a)?;
    assert_durable_boot_a(&snapshot_b, &identities)?;
    if snapshot_b.session_events != snapshot_a.session_events {
        return Err("cold resume appended a durable membership fact".into());
    }
    assert_owned_suffixes(&snapshot_a, &snapshot_b, &identities)?;
    fixture.assert_consumed()?;
    Ok(())
}

/// Proves a cold resume restores no automatic watch edge, while an explicit
/// production `agent_watch` call establishes one fresh correlated subscription
/// without turning its initial snapshot or replay into provider work.
#[test]
fn cold_resume_recreates_explicit_worker_watch() -> Result<(), Box<dyn std::error::Error>> {
    let session_id = SessionId::parse(SESSION).expect("known-safe SessionId must be valid");
    let fixture = DeterministicFixture::new_session_restore_watch(
        "cold_resume_recreates_explicit_worker_watch",
        &ScenarioV2::new(
            "s2-explicit-watch-recreation",
            vec![
                ScenarioLaneV2 {
                    ctx_id: "s2-main".to_owned(),
                    actions: vec![
                        ScenarioActionV2::AgentStartCall {
                            user_text: "start the deterministic worker".to_owned(),
                            call_id: "s2-agent-start".into(),
                            prompt: WORKER_PROMPT.to_owned(),
                            role: "deterministic-worker".to_owned(),
                        },
                        ScenarioActionV2::AgentStartResult {
                            user_text: "start the deterministic worker".to_owned(),
                            call_id: "s2-agent-start".into(),
                            response: "worker start accepted".to_owned(),
                        },
                        ScenarioActionV2::WatchNotifications {
                            notifications: vec![WatchNotificationV2::Response {
                                content: "worker boot-a complete".to_owned(),
                            }],
                            response: "worker completion observed".to_owned(),
                        },
                        ScenarioActionV2::AgentWatchCall {
                            user_text: "recreate worker watch".to_owned(),
                            call_id: "s2-agent-watch".into(),
                        },
                        ScenarioActionV2::AgentWatchResult {
                            user_text: "recreate worker watch".to_owned(),
                            call_id: "s2-agent-watch".into(),
                            expectation: AgentWatchResultExpectationV2::Enabled,
                            response: "worker watch recreated".to_owned(),
                        },
                        ScenarioActionV2::WatchNotificationChains {
                            prompt: "fresh watched worker work".to_owned(),
                            response: "fresh watched worker complete".to_owned(),
                            completion: "fresh watched worker observed".to_owned(),
                        },
                    ],
                },
                ScenarioLaneV2 {
                    ctx_id: "s2-worker".to_owned(),
                    actions: vec![
                        ScenarioActionV2::Text {
                            user_text: WORKER_PROVIDER_INITIAL.to_owned(),
                            response: "worker boot-a complete".to_owned(),
                        },
                        ScenarioActionV2::Text {
                            user_text: "fresh watched worker work".to_owned(),
                            response: "fresh watched worker complete".to_owned(),
                        },
                    ],
                },
            ],
        ),
        FAKE_PROVIDER,
    )?;
    fixture.assert_session_restore_watch_roles()?;

    let socket_a = fixture.socket_path("s2-boot-a");
    let daemon_a = spawn_daemon(&fixture, &socket_a, tau_harness::SessionLaunchStatus::New);
    let mut observer_a = SessionRestoreObserver::connect(&socket_a)?;
    observer_a.create_main("s2-main", "start the deterministic worker")?;
    observer_a.wait_for_marker("worker completion observed")?;
    observer_a.wait_for_two_idle_agents()?;
    let identities = BootIdentities::from_events(&observer_a.events)?;
    assert_boot_a_lifecycle(&observer_a.events, &identities, &session_id)?;
    let boot_a_subscription_id =
        initial_live_watch_subscription_id(&observer_a.events, &identities, &session_id)?;
    assert_provider_turn_counts(
        &observer_a.events,
        &identities,
        ProviderTurnCounts { main: 3, worker: 1 },
    )?;
    disconnect_ui(&mut observer_a.peer)?;
    daemon_a.finish()?;
    let boot_a_action_matches = matched_action_count(&fixture)?;
    assert_eq!(
        boot_a_action_matches, 4,
        "S2 Boot A must consume exactly three main and one worker action"
    );
    let boot_a_cursor = fake_provider_cursor(&fixture)?;
    assert_eq!(
        boot_a_cursor.cursors,
        [3, 1],
        "S2 Boot A must persist the exact next-action cursors"
    );

    let snapshot_a = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    assert_durable_boot_a(&snapshot_a, &identities)?;

    let socket_b = fixture.socket_path("s2-boot-b");
    let daemon_b = spawn_daemon(
        &fixture,
        &socket_b,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut observer_b = SessionRestoreObserver::connect(&socket_b)?;
    observer_b.wait_for_session_boundary(&session_id)?;
    assert_resume_boundaries(&observer_b.events, &identities.all(), &session_id)?;
    assert_replay_is_observational(&observer_b.events, &identities)?;
    if matched_action_count(&fixture)? != boot_a_action_matches {
        return Err("S2 cold replay consumed a fake-provider action".into());
    }
    if fake_provider_cursor(&fixture)? != boot_a_cursor {
        return Err("S2 cold replay changed the fake-provider cursor checkpoint".into());
    }

    let watch_start = observer_b.events.len();
    observer_b.submit(&identities.main, "s2-watch", "recreate worker watch")?;
    observer_b.wait_for_agent_marker(&identities.main, "worker watch recreated", watch_start)?;
    observer_b.wait_for_agent_idle_after(&identities.main, watch_start)?;
    let new_subscription_id = assert_explicit_watch_initial(
        &observer_b.events[watch_start..],
        &identities,
        &session_id,
        &boot_a_subscription_id,
    )?;
    if matched_action_count(&fixture)? != boot_a_action_matches + 2 {
        return Err("initial watch snapshot became provider input".into());
    }
    assert_provider_turn_counts(
        &observer_b.events,
        &identities,
        ProviderTurnCounts { main: 2, worker: 0 },
    )?;

    let worker_start = observer_b.events.len();
    observer_b.submit(
        &identities.worker,
        "s2-worker-fresh",
        "fresh watched worker work",
    )?;
    observer_b.wait_for_agent_marker(
        &identities.worker,
        "fresh watched worker complete",
        worker_start,
    )?;
    observer_b.wait_for_agent_idle_after(&identities.worker, worker_start)?;
    observer_b.wait_for_agent_marker(
        &identities.main,
        "fresh watched worker observed",
        worker_start,
    )?;
    observer_b.wait_for_agent_idle_after(&identities.main, worker_start)?;
    assert_explicit_watch_notifications(
        &observer_b.events[watch_start..],
        &identities,
        &session_id,
        &new_subscription_id,
    )?;
    assert_provider_turn_counts(
        &observer_b.events,
        &identities,
        ProviderTurnCounts { main: 4, worker: 1 },
    )?;
    disconnect_ui(&mut observer_b.peer)?;
    daemon_b.finish()?;

    let snapshot_b = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    snapshot_b.require_prefix(&snapshot_a)?;
    assert_durable_boot_a(&snapshot_b, &identities)?;
    if snapshot_b.session_events != snapshot_a.session_events {
        return Err("S2 resume appended a durable membership fact".into());
    }
    fixture.assert_consumed()?;
    Ok(())
}

/// Exact main and worker identities discovered from immutable creation facts.
struct BootIdentities {
    /// Stable main agent id.
    main: AgentId,
    /// Stable production-started worker id.
    worker: AgentId,
}

/// Exact accepted provider prompts owned by each session-restore role in one
/// observed boot.
#[derive(Clone, Copy)]
struct ProviderTurnCounts {
    /// Main-agent provider turns.
    main: usize,
    /// Worker-agent provider turns.
    worker: usize,
}

impl BootIdentities {
    /// Extracts the one main and one worker creation fact.
    fn from_events(events: &[Observed]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut main = None;
        let mut worker = None;
        for observed in events {
            if let Event::AgentStarted(started) = &observed.event {
                match started.role.as_str() {
                    "deterministic-main" => main = Some(started.agent_id.clone()),
                    "deterministic-worker" => worker = Some(started.agent_id.clone()),
                    role => return Err(format!("unexpected created role `{role}`").into()),
                }
            }
        }
        Ok(Self {
            main: main.ok_or("main creation fact missing")?,
            worker: worker.ok_or("worker creation fact missing")?,
        })
    }

    /// Returns every current durable identity for set-oriented shared oracles.
    fn all(&self) -> [&AgentId; 2] {
        [&self.main, &self.worker]
    }
}

/// Exact idle live-roster facts expected for one restored agent.
struct IdleLiveRosterExpectation<'a> {
    /// Transcript and membership persistence.
    persistence: SessionAgentPersistence,
    /// Harness-owned live navigation default.
    navigation_mode: AgentNavigationMode,
    /// Immutable creation role.
    role: &'a str,
    /// Immutable creation parent.
    parent: Option<&'a AgentId>,
    /// Current display name.
    display_name: Option<&'a str>,
}

fn assert_idle_live_roster_row(
    roster: &[SessionAgentListEntry],
    agent_id: &AgentId,
    expected: IdleLiveRosterExpectation<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    let row = roster
        .iter()
        .find(|row| &row.agent_id == agent_id)
        .ok_or_else(|| format!("live roster omitted {agent_id}"))?;
    if row.persistence != expected.persistence
        || row.lifecycle
            != (SessionAgentLifecycle::Live {
                runtime_state: AgentRuntimeState::Idle,
                navigation_mode: expected.navigation_mode,
            })
    {
        return Err(format!("live roster lifecycle changed for {agent_id}: {row:?}").into());
    }
    match &row.facts {
        SessionAgentFacts::Available {
            parent_agent,
            role: actual_role,
            display_name: actual_name,
            ..
        } if parent_agent.as_ref() == expected.parent
            && actual_role == expected.role
            && actual_name.as_deref() == expected.display_name =>
        {
            Ok(())
        }
        facts => {
            Err(format!("live roster creation facts changed for {agent_id}: {facts:?}").into())
        }
    }
}

fn matched_action_count(
    fixture: &DeterministicFixture,
) -> Result<usize, Box<dyn std::error::Error>> {
    Ok(fixture
        .trace()?
        .lines()
        .filter(|line| line.contains(" matched "))
        .count())
}

/// Reads the bounded provider-local checkpoint used as independent action
/// consumption authority across a no-input cold-resume boundary.
fn fake_provider_cursor(
    fixture: &DeterministicFixture,
) -> Result<FakeCursorCheckpoint, Box<dyn std::error::Error>> {
    let path = fixture
        .harness_state_dir()
        .join("ext/e2e-fake-provider/scenario-cursor.json");
    let metadata = std::fs::metadata(&path)?;
    if metadata.len() > MAX_FAKE_CURSOR_CHECKPOINT_BYTES {
        return Err("fake-provider cursor checkpoint exceeds its bounded schema".into());
    }
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

fn initial_live_watch_subscription_id(
    events: &[Observed],
    identities: &BootIdentities,
    session_id: &SessionId,
) -> Result<String, Box<dyn std::error::Error>> {
    let subscription_ids = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::AgentMessageReceived(message)
                if !observed.replay
                    && message.sender_id == identities.worker
                    && message.recipient_id == identities.main
                    && message.kind == AgentMessageKind::WatchWorkStatus
                    && message
                        .watch_work_status
                        .as_ref()
                        .is_some_and(|state| state.initial && &state.session_id == session_id) =>
            {
                message
                    .watch_work_status
                    .as_ref()
                    .map(|state| state.subscription_id.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let [subscription_id] = subscription_ids.as_slice() else {
        return Err(
            format!("expected one initial watch subscription, got {subscription_ids:?}").into(),
        );
    };
    if subscription_id.is_empty() {
        return Err("initial watch subscription id was empty".into());
    }
    Ok(subscription_id.clone())
}

fn assert_explicit_watch_initial(
    events: &[Observed],
    identities: &BootIdentities,
    session_id: &SessionId,
    boot_a_subscription_id: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let updates = events
        .iter()
        .filter(|observed| {
            !observed.replay
                && matches!(
                    &observed.event,
                    Event::AgentWatchesUpdated(update)
                        if &update.session_id == session_id
                            && update.watcher_id == identities.main
                            && update.watched_agent_ids == [identities.worker.clone()]
                            && update.changed_agent_id.as_ref() == Some(&identities.worker)
                            && update.cause == AgentWatchUpdateCause::AgentWatchEnable
                )
        })
        .count();
    if updates != 1 {
        return Err(format!("explicit watch published {updates} exact enable snapshots").into());
    }
    let subscription_id = initial_live_watch_subscription_id(events, identities, session_id)?;
    if subscription_id == boot_a_subscription_id {
        return Err("explicit watch reused Boot A subscription identity".into());
    }
    let initial = events.iter().filter(|observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::AgentMessageReceived(message)
                    if message.sender_id == identities.worker
                        && message.recipient_id == identities.main
                        && message.kind == AgentMessageKind::WatchWorkStatus
                        && message.watch_provider_status.is_none()
                        && message.watch_work_status.as_ref().is_some_and(|state| {
                            state.initial
                                && &state.session_id == session_id
                                && state.subscription_id == subscription_id
                                && state.phase == tau_proto::AgentWorkStatusPhase::Unreported
                        })
            )
    });
    if initial.count() != 1 {
        return Err("explicit watch lacked one exact idle initial snapshot".into());
    }
    if events.iter().any(|observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::AgentMessageReceived(message)
                    if message.sender_id == identities.worker
                        && message.recipient_id == identities.main
                        && matches!(
                            message.kind,
                            AgentMessageKind::WatchPrompt
                                | AgentMessageKind::WatchResponse
                                | AgentMessageKind::WatchProviderStatus
                        )
            )
    }) {
        return Err("explicit watch emitted a non-initial notification before worker input".into());
    }
    Ok(subscription_id)
}

fn assert_explicit_watch_notifications(
    events: &[Observed],
    identities: &BootIdentities,
    session_id: &SessionId,
    subscription_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let relevant = events
        .iter()
        .enumerate()
        .filter_map(|(index, observed)| match &observed.event {
            Event::AgentMessageReceived(message)
                if !observed.replay
                    && message.sender_id == identities.worker
                    && message.recipient_id == identities.main =>
            {
                Some((index, message))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if relevant.len() != 3 {
        return Err(format!(
            "explicit watch emitted {} worker-to-main notifications instead of three",
            relevant.len()
        )
        .into());
    }
    if relevant
        .iter()
        .any(|(_, message)| message.watch_provider_status.is_some())
    {
        return Err("S2 watch facts carried an unexpected provider-status payload".into());
    }
    assert_watch_prompt_response(&relevant)?;
    let (_, initial) = sole_watch_message(&relevant, AgentMessageKind::WatchWorkStatus)?;
    if !initial.watch_work_status.as_ref().is_some_and(|status| {
        status.initial
            && &status.session_id == session_id
            && status.subscription_id == subscription_id
            && status.phase == tau_proto::AgentWorkStatusPhase::Unreported
    }) {
        return Err("explicit watch initial work-status snapshot changed".into());
    }
    Ok(())
}

fn assert_watch_prompt_response(
    messages: &[(usize, &tau_proto::AgentMessageReceived)],
) -> Result<(), Box<dyn std::error::Error>> {
    let (prompt_index, prompt_message) =
        sole_watch_message(messages, AgentMessageKind::WatchPrompt)?;
    let (response_index, response_message) =
        sole_watch_message(messages, AgentMessageKind::WatchResponse)?;
    if prompt_message.message != "fresh watched worker work"
        || response_message.message != "fresh watched worker complete"
        || response_index <= prompt_index
    {
        return Err("watched prompt/response content or causal order changed".into());
    }
    Ok(())
}

fn sole_watch_message<'a>(
    messages: &[(usize, &'a tau_proto::AgentMessageReceived)],
    kind: AgentMessageKind,
) -> Result<(usize, &'a tau_proto::AgentMessageReceived), Box<dyn std::error::Error>> {
    let mut matching = messages
        .iter()
        .filter(|(_, message)| message.kind == kind)
        .map(|(index, message)| (*index, *message));
    let Some(message) = matching.next() else {
        return Err(format!("expected one {kind:?} notification, got none").into());
    };
    if matching.next().is_some() {
        return Err(format!("expected one {kind:?} notification, got multiple").into());
    }
    Ok(message)
}

fn assert_boot_a_lifecycle(
    events: &[Observed],
    identities: &BootIdentities,
    session_id: &SessionId,
) -> Result<(), Box<dyn std::error::Error>> {
    let started = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::AgentStarted(started) => Some(started),
            _ => None,
        })
        .collect::<Vec<_>>();
    let started_ids = started
        .iter()
        .map(|started| started.agent_id.clone())
        .collect::<BTreeSet<_>>();
    if started_ids != BTreeSet::from([identities.main.clone(), identities.worker.clone()]) {
        return Err(format!("unexpected observed creation identities: {started_ids:?}").into());
    }
    let worker = started
        .iter()
        .find(|started| started.agent_id == identities.worker)
        .ok_or("worker creation fact missing")?;
    if worker.parent_agent.as_ref() != Some(&identities.main)
        || worker.role != "deterministic-worker"
        || worker.display_name.is_some()
    {
        return Err(format!("worker immutable creation fact changed: {worker:?}").into());
    }
    for agent_id in [&identities.main, &identities.worker] {
        let loads = events
            .iter()
            .filter(|observed| {
                matches!(
                    &observed.event,
                    Event::SessionAgentLoaded(loaded)
                        if &loaded.session_id == session_id
                            && &loaded.agent_id == agent_id
                            && !loaded.ephemeral
                )
            })
            .count();
        if loads != 1 {
            return Err(format!("Boot A observed {loads} loads for {agent_id}").into());
        }
    }
    let watch = events.iter().find_map(|observed| match &observed.event {
        Event::AgentWatchesUpdated(watch)
            if watch.watcher_id == identities.main
                && watch.watched_agent_ids == [identities.worker.clone()] =>
        {
            Some(watch)
        }
        _ => None,
    });
    if watch.is_none() {
        return Err("production agent_start did not establish its automatic watch".into());
    }
    Ok(())
}

fn assert_durable_boot_a(
    snapshot: &DurableSessionSnapshot,
    identities: &BootIdentities,
) -> Result<(), Box<dyn std::error::Error>> {
    let expected = BTreeSet::from([identities.main.clone(), identities.worker.clone()]);
    if snapshot
        .agent_events
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>()
        != expected
    {
        return Err("durable current membership is not the exact main/worker pair".into());
    }
    for agent_id in [&identities.main, &identities.worker] {
        let records = &snapshot.agent_events[agent_id];
        let starts = records
            .iter()
            .enumerate()
            .filter(|(_, record)| {
                matches!(&record.event, Event::AgentStarted(started) if &started.agent_id == agent_id)
            })
            .collect::<Vec<_>>();
        if starts.len() != 1 || starts[0].0 != 0 || starts[0].1.seq.get() != 0 {
            return Err(format!("{agent_id} lacks one sequence-zero creation fact").into());
        }
        let loads = snapshot
            .session_events
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::SessionAgentLoaded(loaded)
                        if &loaded.agent_id == agent_id && !loaded.ephemeral
                )
            })
            .count();
        let unloads = snapshot
            .session_events
            .iter()
            .filter(|record| {
                matches!(&record.event, Event::SessionAgentUnloaded(unloaded) if &unloaded.agent_id == agent_id)
            })
            .count();
        if loads != 1 || unloads != 0 {
            return Err(format!(
                "unexpected durable membership for {agent_id}: loads={loads}, unloads={unloads}"
            )
            .into());
        }
    }
    Ok(())
}

fn assert_resume_boundaries(
    events: &[Observed],
    agent_ids: &[&AgentId],
    session_id: &SessionId,
) -> Result<(), Box<dyn std::error::Error>> {
    let session_boundaries = events
        .iter()
        .enumerate()
        .filter(|(_, observed)| {
            !observed.replay
                && observed.recorded_at.is_none()
                && matches!(
                    &observed.event,
                    Event::SessionReplayComplete(done)
                        if &done.session_id == session_id && done.error.is_none()
                )
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let [session_boundary] = session_boundaries.as_slice() else {
        return Err(format!(
            "expected one live session replay boundary, got {session_boundaries:?}"
        )
        .into());
    };
    for agent_id in agent_ids {
        let boundaries = events
            .iter()
            .enumerate()
            .filter(|(_, observed)| {
                !observed.replay
                    && observed.recorded_at.is_none()
                    && matches!(
                        &observed.event,
                        Event::AgentReplayComplete(done)
                            if &done.agent_id == *agent_id
                                && done.session_id.as_ref() == Some(session_id)
                                && done.error.is_none()
                    )
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let [boundary] = boundaries.as_slice() else {
            return Err(format!(
                "expected one live replay boundary for {agent_id}, got {boundaries:?}"
            )
            .into());
        };
        if session_boundary <= boundary {
            return Err(
                format!("{agent_id} replay boundary did not precede session boundary").into(),
            );
        }
    }
    Ok(())
}

fn assert_replay_is_observational(
    events: &[Observed],
    identities: &BootIdentities,
) -> Result<(), Box<dyn std::error::Error>> {
    let watch_responses = events
        .iter()
        .filter(|observed| {
            matches!(
                &observed.event,
                Event::AgentMessageReceived(message)
                    if message.kind == AgentMessageKind::WatchResponse
                        && message.sender_id == identities.worker
                        && message.recipient_id == identities.main
                        && message.message == "worker boot-a complete"
            )
        })
        .collect::<Vec<_>>();
    if watch_responses.len() != 1
        || !watch_responses[0].replay
        || watch_responses[0].recorded_at.is_none()
    {
        return Err("old worker completion was not exactly one replayed transcript fact".into());
    }
    for (agent_id, marker) in [
        (&identities.main, "worker completion observed"),
        (&identities.worker, "worker boot-a complete"),
    ] {
        let terminals = events
            .iter()
            .filter(|observed| {
                observed.replay
                    && observed.recorded_at.is_some()
                    && matches!(
                        &observed.event,
                        Event::ProviderResponseFinished(finished)
                            if &finished.agent_id == agent_id
                                && provider_response_contains(finished, marker)
                    )
            })
            .count();
        if terminals != 1 {
            return Err(
                format!("{agent_id} replayed terminal `{marker}` count was {terminals}").into(),
            );
        }
    }
    let replayed_starts = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::AgentStarted(started) => Some((observed, started)),
            _ => None,
        })
        .collect::<Vec<_>>();
    if replayed_starts.len() != 2
        || replayed_starts
            .iter()
            .any(|(observed, _)| !observed.replay || observed.recorded_at.is_none())
    {
        return Err("Boot B creation facts were not exactly two replay deliveries".into());
    }
    for (agent_id, expected_role, expected_parent, expected_name) in [
        (&identities.main, "deterministic-main", None, None),
        (
            &identities.worker,
            "deterministic-worker",
            Some(&identities.main),
            None,
        ),
    ] {
        let exact = replayed_starts
            .iter()
            .filter(|(_, started)| {
                &started.agent_id == agent_id
                    && started.role == expected_role
                    && started.parent_agent.as_ref() == expected_parent
                    && started.display_name.as_deref() == expected_name
            })
            .count();
        if exact != 1 {
            return Err(format!(
                "Boot B replayed creation fact for {agent_id} was missing or changed"
            )
            .into());
        }
    }
    if events.iter().any(|observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::AgentWatchesUpdated(watch) if !watch.watched_agent_ids.is_empty()
            )
    }) {
        return Err("cold resume restored the daemon-lifetime automatic watch edge".into());
    }
    if events.iter().any(|observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::ProviderPromptSubmitted(_) | Event::AgentInferenceDispatchStarted(_)
            )
    }) {
        return Err("cold replay dispatched fresh provider work".into());
    }
    Ok(())
}

fn assert_no_live_watch_refanout(
    events: &[Observed],
    identities: &BootIdentities,
) -> Result<(), Box<dyn std::error::Error>> {
    if events.iter().any(|observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::AgentMessageReceived(message)
                    if message.sender_id == identities.worker
                        && message.recipient_id == identities.main
                        && matches!(
                            message.kind,
                            AgentMessageKind::WatchPrompt | AgentMessageKind::WatchResponse
                        )
            )
    }) {
        return Err("restored automatic watch re-fanned fresh worker activity".into());
    }
    Ok(())
}

fn assert_provider_turn_counts(
    events: &[Observed],
    identities: &BootIdentities,
    expected: ProviderTurnCounts,
) -> Result<(), Box<dyn std::error::Error>> {
    assert_provider_turn_counts_by_agent(
        events,
        &BTreeMap::from([
            (identities.main.clone(), expected.main),
            (identities.worker.clone(), expected.worker),
        ]),
    )
}

fn assert_provider_turn_counts_by_agent(
    events: &[Observed],
    expected: &BTreeMap<AgentId, usize>,
) -> Result<(), Box<dyn std::error::Error>> {
    let created = events
        .iter()
        .filter(|observed| !observed.replay)
        .filter_map(|observed| match &observed.event {
            Event::AgentPromptCreated(prompt) => {
                Some((prompt.agent_prompt_id.clone(), prompt.agent_id.clone()))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let mut counts = BTreeMap::new();
    for submitted in events.iter().filter_map(|observed| {
        (!observed.replay)
            .then_some(&observed.event)
            .and_then(|event| match event {
                Event::ProviderPromptSubmitted(submitted) => Some(submitted),
                _ => None,
            })
    }) {
        let agent_id = created
            .get(&submitted.agent_prompt_id)
            .ok_or_else(|| {
                format!(
                    "provider accepted prompt {} without one observed creation",
                    submitted.agent_prompt_id
                )
            })?
            .clone();
        *counts.entry(agent_id).or_insert(0) += 1;
    }
    for agent_id in expected.keys() {
        counts.entry(agent_id.clone()).or_insert(0);
    }
    if &counts != expected {
        return Err(format!("provider-turn budget changed: {counts:?} != {expected:?}").into());
    }
    Ok(())
}

fn assert_restored_roster(
    roster: &[SessionAgentListEntry],
    identities: &BootIdentities,
) -> Result<(), Box<dyn std::error::Error>> {
    if roster.len() != 2 {
        return Err(format!("restored roster has {} rows", roster.len()).into());
    }
    assert_idle_live_roster_row(
        roster,
        &identities.main,
        IdleLiveRosterExpectation {
            persistence: SessionAgentPersistence::Durable,
            navigation_mode: AgentNavigationMode::Active,
            role: "deterministic-main",
            parent: None,
            display_name: None,
        },
    )?;
    assert_idle_live_roster_row(
        roster,
        &identities.worker,
        IdleLiveRosterExpectation {
            persistence: SessionAgentPersistence::Durable,
            navigation_mode: AgentNavigationMode::ActiveAuto,
            role: "deterministic-worker",
            parent: Some(&identities.main),
            display_name: None,
        },
    )
}

fn assert_fresh_work_after_boundaries(
    events: &[Observed],
    identities: &BootIdentities,
    session_id: &SessionId,
) -> Result<(), Box<dyn std::error::Error>> {
    let boundary = events
        .iter()
        .position(|observed| {
            !observed.replay
                && observed.recorded_at.is_none()
                && matches!(
                    &observed.event,
                    Event::SessionReplayComplete(done) if &done.session_id == session_id
                )
        })
        .ok_or("live session replay boundary missing")?;
    for (agent_id, prompt, marker) in [
        (&identities.main, "fresh main work", "fresh main complete"),
        (
            &identities.worker,
            "fresh worker work",
            "fresh worker complete",
        ),
    ] {
        let submitted = position(
            events,
            &format!("{agent_id} submitted prompt `{prompt}`"),
            |event| {
                matches!(
                    event,
                    Event::AgentPromptSubmitted(value)
                        if &value.agent_id == agent_id && value.text == prompt
                )
            },
        )?;
        let finished = position(
            events,
            &format!("{agent_id} finished marker `{marker}`"),
            |event| {
                matches!(
                    event,
                    Event::ProviderResponseFinished(value)
                        if &value.agent_id == agent_id && provider_response_contains(value, marker)
                )
            },
        )?;
        if submitted <= boundary || finished <= submitted {
            return Err(format!("fresh work ordering changed for {agent_id}").into());
        }
    }
    Ok(())
}

fn assert_owned_suffixes(
    before: &DurableSessionSnapshot,
    after: &DurableSessionSnapshot,
    identities: &BootIdentities,
) -> Result<(), Box<dyn std::error::Error>> {
    for (owner, other, prompt, marker) in [
        (
            &identities.main,
            &identities.worker,
            "fresh main work",
            "fresh main complete",
        ),
        (
            &identities.worker,
            &identities.main,
            "fresh worker work",
            "fresh worker complete",
        ),
    ] {
        let prefix_len = before.agent_events[owner].len();
        let suffix = &after.agent_events[owner][prefix_len..];
        if count_prompt(suffix, owner, prompt) != 1 || count_response(suffix, owner, marker) != 1 {
            return Err(format!("fresh suffix for {owner} is incomplete or duplicated").into());
        }
        let other_suffix = &after.agent_events[other][before.agent_events[other].len()..];
        if count_prompt(other_suffix, owner, prompt) != 0
            || count_response(other_suffix, owner, marker) != 0
        {
            return Err(format!("fresh work for {owner} leaked into {other} journal").into());
        }
        if suffix
            .iter()
            .any(|record| matches!(record.event, Event::AgentStarted(_)))
        {
            return Err(format!("cold resume appended another creation fact for {owner}").into());
        }
    }
    Ok(())
}

fn count_prompt(
    records: &[tau_core::PersistedAgentEvent],
    agent_id: &AgentId,
    text: &str,
) -> usize {
    records
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentPromptSubmitted(prompt)
                    if &prompt.agent_id == agent_id && prompt.text == text
            )
        })
        .count()
}

fn count_response(
    records: &[tau_core::PersistedAgentEvent],
    agent_id: &AgentId,
    marker: &str,
) -> usize {
    records
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::ProviderResponseFinished(response)
                    if &response.agent_id == agent_id
                        && provider_response_contains(response, marker)
            )
        })
        .count()
}

fn provider_response_contains(
    response: &tau_proto::ProviderResponseFinished,
    marker: &str,
) -> bool {
    response.output_items.iter().any(|item| {
        matches!(
            item,
            tau_proto::ContextItem::Message(message)
                if message.content.iter().any(|part| {
                    matches!(part, tau_proto::ContentPart::Text { text } if text == marker)
                })
        )
    })
}

fn position(
    events: &[Observed],
    expectation: &str,
    predicate: impl Fn(&Event) -> bool,
) -> Result<usize, Box<dyn std::error::Error>> {
    events
        .iter()
        .position(|observed| predicate(&observed.event))
        .ok_or_else(|| format!("required observed event missing: {expectation}").into())
}
