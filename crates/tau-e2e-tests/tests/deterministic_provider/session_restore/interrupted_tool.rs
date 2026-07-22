//! Interrupted worker foreground-tool restore acceptance.

use std::time::{Duration, Instant};

use tau_proto::{AgentId, Event, SessionAgentListScope, SessionId, ToolCallId};

use super::super::daemon_support::{disconnect_ui, spawn_daemon};
use super::{
    BootIdentities, DUMMY_TOOL, DeterministicFixture, DurableSessionSnapshot, FAKE_PROVIDER,
    ProviderTurnCounts, SESSION, ScenarioActionV2, ScenarioLaneV2, ScenarioV2,
    SessionRestoreObserver, WORKER_INITIAL, WORKER_PROMPT, assert_provider_turn_counts,
    assert_resume_boundaries, matched_action_count,
};

/// Exact provider-authored foreground tool identity.
const TOOL_CALL_ID: &str = "s6-interrupted-tool";
/// Exact compact JSON size of the reviewed S6 scenario grammar.
const SCENARIO_BYTES: usize = 1_259;

/// Proves a worker's acknowledged but unterminated foreground tool is repaired
/// once on resume and remains balanced across a second cold resume.
#[test]
fn cold_resume_repairs_interrupted_worker_tool_once() -> Result<(), Box<dyn std::error::Error>> {
    let session_id = SessionId::from(SESSION);
    let diagnostic = interrupted_tool_diagnostic(&TOOL_CALL_ID.into());
    let scenario = interrupted_tool_scenario(&diagnostic);
    assert_scenario_budget(&scenario)?;
    let fixture = DeterministicFixture::new_session_restore_interrupted_tool(
        "cold_resume_repairs_interrupted_worker_tool_once",
        &scenario,
        FAKE_PROVIDER,
        DUMMY_TOOL,
    )?;
    fixture.assert_session_restore_interrupted_tool_roles()?;

    let socket_a = fixture.socket_path("s6-boot-a");
    let daemon_a = spawn_daemon(&fixture, &socket_a, tau_harness::SessionLaunchStatus::New);
    let mut observer_a = SessionRestoreObserver::connect(&socket_a)?;
    observer_a.create_main("s6-main", "start the tool-holding deterministic worker")?;
    observer_a.wait_for_marker("tool-holding worker running observed")?;
    let identities = BootIdentities::from_events(&observer_a.events)?;
    wait_for_tool_readiness(&mut observer_a)?;
    let snapshot_a = wait_for_interrupted_tool_snapshot(&fixture, &session_id, &identities.worker)?;
    assert_interrupted_restore_stream(&snapshot_a, &identities.worker)?;
    assert_no_terminal_tool_event(&snapshot_a, &identities.worker)?;
    assert_provider_turn_counts(
        &observer_a.events,
        &identities,
        ProviderTurnCounts { main: 3, worker: 1 },
    )?;
    if matched_action_count(&fixture)? != 4 {
        return Err("S6 Boot A did not stop at the exact four-action crash cut".into());
    }
    let terminated = daemon_a.kill_ungracefully()?;
    drop(observer_a);
    terminated.require_gone(fixture.harness_state_dir(), session_id.as_str())?;

    let snapshot_a = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    assert_interrupted_restore_stream(&snapshot_a, &identities.worker)?;
    assert_no_terminal_tool_event(&snapshot_a, &identities.worker)?;

    let socket_b = fixture.socket_path("s6-boot-b");
    let daemon_b = spawn_daemon(
        &fixture,
        &socket_b,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut observer_b = SessionRestoreObserver::connect(&socket_b)?;
    observer_b.wait_for_session_boundary(&session_id)?;
    assert_resume_boundaries(&observer_b.events, &identities.all(), &session_id)?;
    assert_boot_b_repair_pair(
        &fixture,
        &observer_b.events,
        &identities.worker,
        &diagnostic,
    )?;
    assert_no_live_tool_restart(&observer_b.events, &identities.worker)?;
    assert_provider_turn_counts(
        &observer_b.events,
        &identities,
        ProviderTurnCounts { main: 0, worker: 0 },
    )?;

    let repair_start = observer_b.events.len();
    observer_b.submit(
        &identities.worker,
        "s6-worker-repair",
        "continue the repaired tool round",
    )?;
    observer_b.wait_for_agent_marker(
        &identities.worker,
        "interrupted worker tool repaired",
        repair_start,
    )?;
    observer_b.wait_for_agent_idle_after(&identities.worker, repair_start)?;
    assert_provider_turn_counts(
        &observer_b.events,
        &identities,
        ProviderTurnCounts { main: 0, worker: 1 },
    )?;
    if matched_action_count(&fixture)? != 5 {
        return Err("S6 Boot B repair did not consume exactly one fake action".into());
    }
    let current_b = observer_b.roster(&session_id, SessionAgentListScope::Current)?;
    let history_b = observer_b.roster(&session_id, SessionAgentListScope::History)?;
    disconnect_ui(&mut observer_b.peer)?;
    daemon_b.finish()?;

    let snapshot_b = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    let worker_events_b = load_agent_events(&fixture, &identities.worker)?;
    assert_boot_b_durable_state(
        &snapshot_a,
        &snapshot_b,
        &worker_events_b,
        &identities,
        &diagnostic,
    )?;

    let socket_c = fixture.socket_path("s6-boot-c");
    let daemon_c = spawn_daemon(
        &fixture,
        &socket_c,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut observer_c = SessionRestoreObserver::connect(&socket_c)?;
    observer_c.wait_for_session_boundary(&session_id)?;
    let boot_c_agents = snapshot_b.agent_events.keys().collect::<Vec<_>>();
    assert_resume_boundaries(&observer_c.events, &boot_c_agents, &session_id)?;
    assert_no_duplicate_repair(&observer_c.events, &identities.worker)?;
    assert_single_repair_trace(&fixture)?;
    assert_provider_turn_counts(
        &observer_c.events,
        &identities,
        ProviderTurnCounts { main: 0, worker: 0 },
    )?;
    if matched_action_count(&fixture)? != 5 {
        return Err("S6 Boot C consumed provider work without new input".into());
    }
    let current_c = observer_c.roster(&session_id, SessionAgentListScope::Current)?;
    let history_c = observer_c.roster(&session_id, SessionAgentListScope::History)?;
    if current_c != current_b || history_c != history_b {
        return Err("S6 Boot C changed current/history durable membership".into());
    }
    disconnect_ui(&mut observer_c.peer)?;
    daemon_c.finish()?;

    let snapshot_c = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    if snapshot_c != snapshot_b {
        return Err("S6 Boot C changed membership, execution restore, or agent streams".into());
    }
    if load_agent_events(&fixture, &identities.worker)? != worker_events_b {
        return Err("S6 Boot C changed the worker journal".into());
    }
    fixture.assert_consumed()?;
    Ok(())
}

/// Builds the closed three-main-action/two-worker-action S6 grammar.
fn interrupted_tool_scenario(diagnostic: &str) -> ScenarioV2 {
    ScenarioV2::new(
        "s6-interrupted-worker-foreground-tool",
        vec![
            ScenarioLaneV2 {
                ctx_id: "s6-main".to_owned(),
                actions: vec![
                    ScenarioActionV2::AgentStartCall {
                        user_text: "start the tool-holding deterministic worker".to_owned(),
                        call_id: "s6-agent-start".into(),
                        prompt: WORKER_PROMPT.to_owned(),
                        role: Some("deterministic-worker".to_owned()),
                        task_name: "tool-holding deterministic worker".to_owned(),
                    },
                    ScenarioActionV2::AgentStartResult {
                        user_text: "start the tool-holding deterministic worker".to_owned(),
                        call_id: "s6-agent-start".into(),
                        response: "tool-holding worker start accepted".to_owned(),
                    },
                    ScenarioActionV2::WatchNotifications {
                        notifications: vec![super::WatchNotificationV2::TurnState {
                            state: tau_proto::AgentRuntimeState::Running,
                        }],
                        response: "tool-holding worker running observed".to_owned(),
                    },
                ],
            },
            ScenarioLaneV2 {
                ctx_id: "s6-worker".to_owned(),
                actions: vec![
                    ScenarioActionV2::DummyToolCall {
                        user_text: WORKER_INITIAL.to_owned(),
                        call_id: TOOL_CALL_ID.into(),
                    },
                    ScenarioActionV2::DummyToolRepair {
                        user_text: "continue the repaired tool round".to_owned(),
                        call_id: TOOL_CALL_ID.into(),
                        diagnostic: diagnostic.to_owned(),
                        response: "interrupted worker tool repaired".to_owned(),
                    },
                ],
            },
        ],
    )
}

/// Enforces the reviewed S6 lane/action/encoded-size budget before startup.
fn assert_scenario_budget(scenario: &ScenarioV2) -> Result<(), Box<dyn std::error::Error>> {
    let actions = scenario
        .lanes
        .iter()
        .map(|lane| lane.actions.len())
        .collect::<Vec<_>>();
    let encoded = serde_json::to_vec(scenario)?;
    if actions != [3, 2] || encoded.len() != SCENARIO_BYTES {
        return Err(format!(
            "S6 scenario budget changed: lanes={}, actions={actions:?}, bytes={}",
            scenario.lanes.len(),
            encoded.len()
        )
        .into());
    }
    Ok(())
}

/// Waits for one exact canonical live readiness progress event.
fn wait_for_tool_readiness(
    observer: &mut SessionRestoreObserver,
) -> Result<(), Box<dyn std::error::Error>> {
    observer
        .recv_until(|observed| {
            matches!(
                &observed.event,
                Event::ToolProgress(progress)
                    if progress.call_id.as_str() == TOOL_CALL_ID
                        && progress.tool_name.as_str() == "restart_test_dummy"
                        && progress.message.as_deref() == Some("hold_no_side_effect ready")
            )
        })
        .and_then(|observed| {
            if observed.replay {
                Err("S6 hold readiness was not a live non-semantic fact".into())
            } else {
                Ok(())
            }
        })
}

/// Waits until the authoritative restore stream contains the exact interrupted
/// worker request/start pair.
fn wait_for_interrupted_tool_snapshot(
    fixture: &DeterministicFixture,
    session_id: &SessionId,
    worker: &AgentId,
) -> Result<DurableSessionSnapshot, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(snapshot) = DurableSessionSnapshot::load(fixture.harness_state_dir(), session_id)
            && interrupted_restore_records(&snapshot, worker).len() == 2
        {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err("S6 interrupted tool restore pair did not become durable".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Returns restore records correlated to the S6 worker/call.
fn interrupted_restore_records<'a>(
    snapshot: &'a DurableSessionSnapshot,
    worker: &AgentId,
) -> Vec<&'a tau_core::PersistedSessionEvent> {
    snapshot
        .restore_events
        .iter()
        .filter(|record| match &record.event {
            Event::ToolRequest(request) => {
                &request.agent_id == worker && request.call_id.as_str() == TOOL_CALL_ID
            }
            Event::ToolStarted(started) => {
                &started.agent_id == worker && started.call_id.as_str() == TOOL_CALL_ID
            }
            _ => false,
        })
        .collect()
}

/// Requires exactly one durable request followed by one canonical start.
fn assert_interrupted_restore_stream(
    snapshot: &DurableSessionSnapshot,
    worker: &AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    let records = interrupted_restore_records(snapshot, worker);
    if !matches!(
        records.as_slice(),
        [
            tau_core::PersistedSessionEvent {
                event: Event::ToolRequest(request),
                ..
            },
            tau_core::PersistedSessionEvent {
                event: Event::ToolStarted(started),
                ..
            }
        ] if request.call_id == started.call_id
            && request.tool_name == started.tool_name
            && request.arguments == started.arguments
            && request.tool_name.as_str() == "restart_test_dummy"
    ) {
        return Err("S6 restore stream lacks the exact request/start pair".into());
    }
    Ok(())
}

/// Requires that Boot A contains no terminal result for the interrupted call.
fn assert_no_terminal_tool_event(
    snapshot: &DurableSessionSnapshot,
    worker: &AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    if snapshot.agent_events[worker].iter().any(|record| {
        matches!(
            &record.event,
            Event::ToolResult(result) | Event::ProviderToolResult(result)
                if result.call_id.as_str() == TOOL_CALL_ID
        ) || matches!(
            &record.event,
            Event::ToolError(error) | Event::ProviderToolError(error)
                if error.call_id.as_str() == TOOL_CALL_ID
        )
    }) {
        return Err("S6 Boot A interrupted call already had a terminal event".into());
    }
    Ok(())
}

/// Requires one live non-semantic/durable repair pair owned by the worker.
fn assert_boot_b_repair_pair(
    fixture: &DeterministicFixture,
    events: &[super::Observed],
    worker: &AgentId,
    diagnostic: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let repair = events
        .iter()
        .enumerate()
        .filter_map(|(index, observed)| match &observed.event {
            Event::ToolError(error) | Event::ProviderToolError(error)
                if error.tool_name.as_str() == "restart_test_dummy" =>
            {
                Some((index, observed, error))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if repair.len() != 1
        || !matches!(repair[0].1.event, Event::ProviderToolError(_))
        || repair[0].2.message != diagnostic
        || !repair[0].1.replay
        || repair[0].1.recorded_at.is_none()
    {
        return Err(
            format!("S6 Boot B repair pair changed for worker {worker}: {repair:?}").into(),
        );
    }
    assert_single_repair_trace(fixture)?;
    Ok(())
}

/// Requires the complete strict fake trace to contain one ordered live repair
/// pair across every daemon generation.
fn assert_single_repair_trace(
    fixture: &DeterministicFixture,
) -> Result<(), Box<dyn std::error::Error>> {
    let trace = fixture.trace()?;
    let tool = format!("call_id={TOOL_CALL_ID} repair_tool_error");
    let provider = format!("call_id={TOOL_CALL_ID} repair_provider_tool_error");
    let lines = trace.lines().collect::<Vec<_>>();
    let tool_positions = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (*line == tool).then_some(index))
        .collect::<Vec<_>>();
    let provider_positions = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (*line == provider).then_some(index))
        .collect::<Vec<_>>();
    if tool_positions.len() != 1
        || provider_positions.len() != 1
        || tool_positions[0] >= provider_positions[0]
    {
        return Err(format!("S6 live repair trace changed: {trace}").into());
    }
    Ok(())
}

/// Requires resume repair not to redispatch the extension-owned tool.
fn assert_no_live_tool_restart(
    events: &[super::Observed],
    worker: &AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    if events.iter().any(|observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::ToolStarted(started)
                    if &started.agent_id == worker
                        && started.tool_name.as_str() == "restart_test_dummy"
            )
    }) {
        return Err("S6 Boot B emitted a second live tool.started".into());
    }
    Ok(())
}

/// Requires Boot B to append exactly one worker-owned durable repair while
/// preserving the restore execution stream and main journal.
fn assert_boot_b_durable_state(
    before: &DurableSessionSnapshot,
    after: &DurableSessionSnapshot,
    worker_events: &[tau_core::PersistedAgentEvent],
    identities: &BootIdentities,
    diagnostic: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !after.session_events.starts_with(&before.session_events)
        || after.restore_events != before.restore_events
        || after.agent_events.get(&identities.main) != before.agent_events.get(&identities.main)
    {
        return Err("S6 Boot B changed membership, execution restore, or the main journal".into());
    }
    if !worker_events.starts_with(&before.agent_events[&identities.worker]) {
        return Err("S6 Boot B changed the worker journal prefix".into());
    }
    let suffix = &worker_events[before.agent_events[&identities.worker].len()..];
    let errors = worker_events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ProviderToolError(error) => Some(error),
            _ => None,
        })
        .collect::<Vec<_>>();
    let other_terminals = suffix.iter().filter(|record| {
        matches!(
            record.event,
            Event::ToolError(_) | Event::ToolResult(_) | Event::ProviderToolResult(_)
        )
    });
    if errors.len() != 1
        || errors[0].call_id.as_str() != TOOL_CALL_ID
        || errors[0].message != diagnostic
        || other_terminals.count() != 0
    {
        return Err("S6 durable repair was not one worker-owned provider.tool_error".into());
    }
    Ok(())
}

/// Loads one agent journal independently of current membership composition.
fn load_agent_events(
    fixture: &DeterministicFixture,
    agent_id: &AgentId,
) -> Result<Vec<tau_core::PersistedAgentEvent>, Box<dyn std::error::Error>> {
    let store = tau_core::AgentStore::open(fixture.harness_state_dir().join("agents"))?;
    Ok(store.agent_events(agent_id.as_str())?)
}

/// Requires Boot C to publish no new live repair for the already balanced call.
fn assert_no_duplicate_repair(
    events: &[super::Observed],
    worker: &AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    if events.iter().any(|observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::ToolError(error) | Event::ProviderToolError(error)
                    if error.call_id.as_str() == TOOL_CALL_ID
            )
    }) {
        return Err(format!("S6 Boot C duplicated repair for worker {worker}").into());
    }
    Ok(())
}

/// Reproduces the harness's exact synthetic interrupted-tool diagnostic.
fn interrupted_tool_diagnostic(call_id: &ToolCallId) -> String {
    format!(
        "{}: true\n\nTool call `{call_id}` was interrupted due to session restart. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}
