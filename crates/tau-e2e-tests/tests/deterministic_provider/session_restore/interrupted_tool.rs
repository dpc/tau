//! Interrupted worker foreground-tool restore acceptance.

use tau_proto::{AgentId, Event, SessionAgentListScope, SessionId};

use super::super::daemon_support::{disconnect_ui, spawn_daemon};
use super::{
    BootIdentities, DUMMY_TOOL, DeterministicFixture, DurableSessionSnapshot, FAKE_PROVIDER,
    ProviderTurnCounts, SESSION, ScenarioActionV2, ScenarioLaneV2, ScenarioV2,
    SessionRestoreObserver, WORKER_INITIAL, WORKER_PROMPT, assert_provider_turn_counts,
    assert_resume_boundaries, interruption_support as interruption, matched_action_count,
};

/// Exact provider-authored foreground tool identity.
const TOOL_CALL_ID: &str = "s6-interrupted-tool";
/// Exact compact JSON size of the reviewed S6 scenario grammar.
const SCENARIO_BYTES: usize = 1_120;

/// Proves a worker's acknowledged but unterminated foreground tool is repaired
/// once on resume and remains balanced across a second cold resume.
#[test]
fn cold_resume_repairs_interrupted_worker_tool_once() -> Result<(), Box<dyn std::error::Error>> {
    let session_id = SessionId::parse(SESSION).expect("known-safe SessionId must be valid");
    let diagnostic = interruption::interrupted_tool_diagnostic(&TOOL_CALL_ID.into());
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
    observer_a.wait_for_agent_role("deterministic-worker")?;
    observer_a.wait_for_marker("tool-holding worker start accepted")?;
    let identities = BootIdentities::from_events(&observer_a.events)?;
    interruption::wait_for_tool_readiness(&mut observer_a, TOOL_CALL_ID)?;
    let snapshot_a = interruption::wait_for_interrupted_tool_snapshot(
        &fixture,
        &session_id,
        &identities.worker,
        TOOL_CALL_ID,
    )?;
    interruption::assert_interrupted_restore_stream(&snapshot_a, &identities.worker, TOOL_CALL_ID)?;
    interruption::assert_no_terminal_tool_event(&snapshot_a, &identities.worker, TOOL_CALL_ID)?;
    assert_provider_turn_counts(
        &observer_a.events,
        &identities,
        ProviderTurnCounts { main: 2, worker: 1 },
    )?;
    if matched_action_count(&fixture)? != 3 {
        return Err("S6 Boot A did not stop at the exact three-action crash cut".into());
    }
    let terminated = daemon_a.kill_ungracefully()?;
    drop(observer_a);
    terminated.require_gone(fixture.harness_state_dir(), session_id.as_str())?;

    let snapshot_a = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    interruption::assert_interrupted_restore_stream(&snapshot_a, &identities.worker, TOOL_CALL_ID)?;
    interruption::assert_no_terminal_tool_event(&snapshot_a, &identities.worker, TOOL_CALL_ID)?;

    let socket_b = fixture.socket_path("s6-boot-b");
    let daemon_b = spawn_daemon(
        &fixture,
        &socket_b,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut observer_b = SessionRestoreObserver::connect(&socket_b)?;
    observer_b.wait_for_session_boundary(&session_id)?;
    assert_resume_boundaries(&observer_b.events, &identities.all(), &session_id)?;
    interruption::assert_boot_b_repair_pair(
        &fixture,
        &observer_b.events,
        &identities.worker,
        TOOL_CALL_ID,
        &diagnostic,
    )?;
    interruption::assert_no_live_tool_restart(&observer_b.events, &identities.worker)?;
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
    if matched_action_count(&fixture)? != 4 {
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
    interruption::assert_no_duplicate_repair(&observer_c.events, &identities.worker, TOOL_CALL_ID)?;
    interruption::assert_single_repair_trace(&fixture, TOOL_CALL_ID)?;
    assert_provider_turn_counts(
        &observer_c.events,
        &identities,
        ProviderTurnCounts { main: 0, worker: 0 },
    )?;
    if matched_action_count(&fixture)? != 4 {
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
    super::assert_initialization_only_refresh(&snapshot_b, &snapshot_c)?;
    if load_agent_events(&fixture, &identities.worker)? != worker_events_b {
        return Err("S6 Boot C changed the unloaded worker journal".into());
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
    if actions != [2, 2] || encoded.len() != SCENARIO_BYTES {
        return Err(format!(
            "S6 scenario budget changed: lanes={}, actions={actions:?}, bytes={}",
            scenario.lanes.len(),
            encoded.len()
        )
        .into());
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
        || !super::suffix_after_initialization(before, after, &identities.main)?.is_empty()
    {
        return Err("S6 Boot B changed membership, execution restore, or main state".into());
    }
    if !worker_events.starts_with(&before.agent_events[&identities.worker]) {
        return Err("S6 Boot B changed the worker journal prefix".into());
    }
    let suffix = super::suffix_after_initialization_events(
        &before.agent_events[&identities.worker],
        worker_events,
        &identities.worker,
        &after.session_id,
    )?;
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
