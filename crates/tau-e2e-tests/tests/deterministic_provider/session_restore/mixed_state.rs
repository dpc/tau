//! Mixed-state and repeated-resume isolation acceptance.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use tau_proto::{
    AgentId, AgentNavigationMode, AgentRuntimeState, Event, HarnessInputMessage,
    SessionAgentListEntry, SessionAgentListScope, SessionAgentPersistence, SessionId,
};

use super::super::daemon_support::{disconnect_ui, spawn_daemon};
use super::{
    DUMMY_TOOL, DeterministicFixture, DurableSessionSnapshot, FAKE_PROVIDER, Observed, SESSION,
    ScenarioActionV2, ScenarioLaneV2, ScenarioV2, SessionRestoreObserver, WatchNotificationV2,
    assert_idle_live_roster_row, assert_provider_turn_counts_by_agent, assert_resume_boundaries,
    count_prompt, count_response, interruption_support as interruption, matched_action_count,
};

const QUIESCENT_ROLE: &str = "deterministic-worker-quiescent";
const UNCERTAIN_ROLE: &str = "deterministic-worker-uncertain";
const REPAIR_ROLE: &str = "deterministic-worker-repair";
const QUIESCENT_PROMPT: &str = "Complete the quiescent deterministic instruction.";
const UNCERTAIN_PROMPT: &str = "Complete the dispatch-uncertain deterministic instruction.";
const REPAIR_PROMPT: &str = "Begin the interrupted-tool deterministic instruction.";
const QUIESCENT_INITIAL: &str = concat!(
    "[tau-internal]: You were started by an agent `main`. Your responses will be delivered to it. ",
    "You can use the `message` tool to communicate with agents.\n\n",
    "Complete the quiescent deterministic instruction."
);
const UNCERTAIN_INITIAL: &str = concat!(
    "[tau-internal]: You were started by an agent `main`. Your responses will be delivered to it. ",
    "You can use the `message` tool to communicate with agents.\n\n",
    "Complete the dispatch-uncertain deterministic instruction."
);
const REPAIR_CONTINUATION: &str = "Continue the mixed-state repaired tool round.";
const REPAIR_MARKER: &str = "mixed-state interrupted tool repaired";
const HOLD_TIMEOUT_MS: u64 = 10_000;
const TOOL_CALL_ID: &str = "s7-interrupted-tool";
const BOOT_A_CURSORS: [usize; 4] = [5, 1, 1, 1];
const COMPLETE_CURSORS: [usize; 4] = [5, 1, 1, 2];
const SCENARIO_BYTES: usize = 2_223;
const MAX_CHECKPOINT_BYTES: usize = 64 * 1024;

/// Decoded fake-provider checkpoint used to prove all four immutable lane
/// bindings and both production-started child associations.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorCheckpoint {
    /// Complete scenario identity associated with the cursor vector.
    scenario: ScenarioV2,
    /// Next action index for each configured lane.
    cursors: Vec<usize>,
    /// Immutable harness-agent lane bindings.
    agent_lanes: Vec<AgentLaneCheckpoint>,
    /// Harness-minted production child identities.
    child_agents: Vec<ChildAgentCheckpoint>,
}

/// One decoded agent-to-lane checkpoint row.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentLaneCheckpoint {
    /// Durable harness agent identity.
    agent_id: AgentId,
    /// Index into the configured scenario lanes.
    lane_index: usize,
}

/// One decoded parent-to-child checkpoint row.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildAgentCheckpoint {
    /// Parent that issued the production `agent_start`.
    parent_agent_id: AgentId,
    /// Zero-based successful start-result ordinal.
    start_ordinal: usize,
    /// Harness-minted durable worker identity.
    child_agent_id: AgentId,
}

/// Stable identities for the quiescent, uncertain, and repaired state classes.
struct S7Identities {
    /// Quiescent top-level main.
    main: AgentId,
    /// Completed production-started worker.
    quiescent: AgentId,
    /// Production-started worker with an unfinished provider dispatch.
    uncertain: AgentId,
    /// Direct durable child with an interrupted foreground dummy call.
    repair: AgentId,
}

/// Exact live provider turns owned by each S7 role in one daemon generation.
struct S7ProviderTurnCounts {
    /// Quiescent top-level main turns.
    main: usize,
    /// Completed production worker turns.
    quiescent: usize,
    /// Held dispatch-uncertain worker turns.
    uncertain: usize,
    /// Interrupted-tool repair worker turns.
    repair: usize,
}

impl S7Identities {
    /// Extracts exactly one live Boot-A creation for every closed S7 role.
    fn from_events(events: &[Observed]) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            main: sole_live_agent_for_role(events, "deterministic-main")?,
            quiescent: sole_live_agent_for_role(events, QUIESCENT_ROLE)?,
            uncertain: sole_live_agent_for_role(events, UNCERTAIN_ROLE)?,
            repair: sole_live_agent_for_role(events, REPAIR_ROLE)?,
        })
    }

    /// Returns every current durable identity for shared replay oracles.
    fn all(&self) -> [&AgentId; 4] {
        [&self.main, &self.quiescent, &self.uncertain, &self.repair]
    }

    /// Returns the exact identity-to-lane checkpoint projection.
    fn lane_bindings(&self) -> BTreeMap<AgentId, usize> {
        BTreeMap::from([
            (self.main.clone(), 0),
            (self.quiescent.clone(), 1),
            (self.uncertain.clone(), 2),
            (self.repair.clone(), 3),
        ])
    }
}

/// Proves mixed quiescent, dispatch-uncertain, and interrupted-tool state
/// remains agent-owned across two no-input cold resumes.
#[test]
fn cold_resume_mixed_state_is_agent_owned_and_idempotent() -> Result<(), Box<dyn std::error::Error>>
{
    let session_id = SessionId::parse(SESSION).expect("known-safe SessionId must be valid");
    let diagnostic = interruption::interrupted_tool_diagnostic(&TOOL_CALL_ID.into());
    let scenario = mixed_state_scenario(&diagnostic);
    assert_scenario_budget(&scenario)?;
    let fixture = DeterministicFixture::new_session_restore_mixed(
        "cold_resume_mixed_state_is_agent_owned_and_idempotent",
        &scenario,
        FAKE_PROVIDER,
        DUMMY_TOOL,
    )?;
    fixture.assert_session_restore_mixed_roles()?;

    let socket_a = fixture.socket_path("s7-boot-a");
    let daemon_a = spawn_daemon(&fixture, &socket_a, tau_harness::SessionLaunchStatus::New);
    let mut observer_a = SessionRestoreObserver::connect(&socket_a)?;
    observer_a.create_main("s7-main", "start the quiescent deterministic worker")?;
    observer_a.wait_for_marker("quiescent worker completion observed")?;
    observer_a.wait_for_idle_agent_count(2)?;
    let main = sole_live_agent_for_role(&observer_a.events, "deterministic-main")?;
    observer_a.submit(
        &main,
        "s7-start-uncertain",
        "start the dispatch-uncertain deterministic worker",
    )?;
    observer_a.wait_for_agent_role(UNCERTAIN_ROLE)?;
    observer_a.wait_for_marker("dispatch-uncertain worker start accepted")?;
    let uncertain = sole_live_agent_for_role(&observer_a.events, UNCERTAIN_ROLE)?;
    let dispatch = interruption::wait_for_worker_dispatch(&mut observer_a, &uncertain)?;
    let durable_dispatch =
        interruption::wait_for_durable_dispatch(&fixture, &session_id, &uncertain, &dispatch)?;
    interruption::wait_for_hold_readiness(&fixture, &dispatch.agent_prompt_id)?;

    create_direct_repair_worker(&mut observer_a, &main)?;
    let identities = S7Identities::from_events(&observer_a.events)?;
    if identities.uncertain != uncertain {
        return Err("S7 uncertain worker identity changed during repair-worker creation".into());
    }
    interruption::wait_for_tool_readiness(&mut observer_a, TOOL_CALL_ID)?;
    wait_for_boot_a_runtime_states(&mut observer_a, &identities)?;
    let snapshot_a = interruption::wait_for_interrupted_tool_snapshot(
        &fixture,
        &session_id,
        &identities.repair,
        TOOL_CALL_ID,
    )?;
    interruption::assert_unfinished_worker_dispatch(
        &durable_dispatch,
        &identities.uncertain,
        &dispatch,
    )?;
    interruption::assert_interrupted_restore_stream(&snapshot_a, &identities.repair, TOOL_CALL_ID)?;
    interruption::assert_no_terminal_tool_event(&snapshot_a, &identities.repair, TOOL_CALL_ID)?;
    assert_boot_a_durable_state(&snapshot_a, &identities)?;
    assert_provider_turn_counts_by_agent(
        &observer_a.events,
        &provider_budget(
            &identities,
            S7ProviderTurnCounts {
                main: 5,
                quiescent: 1,
                uncertain: 1,
                repair: 1,
            },
        ),
    )?;
    if matched_action_count(&fixture)? != 8 {
        return Err("S7 Boot A did not stop at the exact eight-action crash cut".into());
    }
    assert_fake_checkpoint(&fixture, &scenario, &identities, BOOT_A_CURSORS)?;
    interruption::assert_hold_ready_and_live(&fixture, &dispatch.agent_prompt_id)?;
    let terminated = daemon_a.kill_ungracefully()?;
    drop(observer_a);
    terminated.require_gone(fixture.harness_state_dir(), session_id.as_str())?;

    let snapshot_a = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    interruption::assert_unfinished_worker_dispatch(&snapshot_a, &identities.uncertain, &dispatch)?;
    interruption::assert_interrupted_restore_stream(&snapshot_a, &identities.repair, TOOL_CALL_ID)?;
    interruption::assert_no_terminal_tool_event(&snapshot_a, &identities.repair, TOOL_CALL_ID)?;

    let socket_b = fixture.socket_path("s7-boot-b");
    let daemon_b = spawn_daemon(
        &fixture,
        &socket_b,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut observer_b = SessionRestoreObserver::connect(&socket_b)?;
    observer_b.wait_for_session_boundary(&session_id)?;
    assert_resume_boundaries(&observer_b.events, &identities.all(), &session_id)?;
    let warning_b = assert_owned_uncertain_warning(&observer_b.events, &identities)?;
    interruption::assert_boot_b_repair_pair(
        &fixture,
        &observer_b.events,
        &identities.repair,
        TOOL_CALL_ID,
        &diagnostic,
    )?;
    interruption::assert_no_live_tool_restart(&observer_b.events, &identities.repair)?;
    assert_no_live_restore_dispatch(&observer_b.events, &identities)?;
    assert_no_restored_worker_watch(&observer_b.events, &identities)?;
    assert_provider_turn_counts_by_agent(
        &observer_b.events,
        &provider_budget(&identities, S7ProviderTurnCounts::ZERO),
    )?;
    if matched_action_count(&fixture)? != 8 {
        return Err("S7 Boot B consumed fake work while restoring mixed state".into());
    }
    assert_fake_checkpoint(&fixture, &scenario, &identities, BOOT_A_CURSORS)?;
    let current_b = roster_by_id(observer_b.roster(&session_id, SessionAgentListScope::Current)?)?;
    let history_b = roster_by_id(observer_b.roster(&session_id, SessionAgentListScope::History)?)?;
    assert_mixed_roster(&current_b, &identities)?;
    if history_b != current_b {
        return Err("S7 Boot B current/history rosters differ".into());
    }
    disconnect_ui(&mut observer_b.peer)?;
    daemon_b.finish()?;
    assert_no_uncertain_termination(&fixture, &identities, &dispatch)?;

    let snapshot_b = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    assert_boot_b_durable_repair(&snapshot_a, &snapshot_b, &identities, &diagnostic)?;
    interruption::assert_unfinished_worker_dispatch(&snapshot_b, &identities.uncertain, &dispatch)?;

    let socket_c = fixture.socket_path("s7-boot-c");
    let daemon_c = spawn_daemon(
        &fixture,
        &socket_c,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut observer_c = SessionRestoreObserver::connect(&socket_c)?;
    observer_c.wait_for_session_boundary(&session_id)?;
    assert_resume_boundaries(&observer_c.events, &identities.all(), &session_id)?;
    let warning_c = assert_owned_uncertain_warning(&observer_c.events, &identities)?;
    if warning_c <= warning_b {
        return Err("S7 Boot C did not publish a fresh uncertain-worker warning".into());
    }
    interruption::assert_no_duplicate_repair(&observer_c.events, &identities.repair, TOOL_CALL_ID)?;
    interruption::assert_single_repair_trace(&fixture, TOOL_CALL_ID)?;
    interruption::assert_no_live_tool_restart(&observer_c.events, &identities.repair)?;
    assert_no_live_restore_dispatch(&observer_c.events, &identities)?;
    assert_no_restored_worker_watch(&observer_c.events, &identities)?;
    assert_provider_turn_counts_by_agent(
        &observer_c.events,
        &provider_budget(&identities, S7ProviderTurnCounts::ZERO),
    )?;
    if matched_action_count(&fixture)? != 8 {
        return Err("S7 Boot C consumed fake work without user input".into());
    }
    assert_fake_checkpoint(&fixture, &scenario, &identities, BOOT_A_CURSORS)?;
    let current_c = roster_by_id(observer_c.roster(&session_id, SessionAgentListScope::Current)?)?;
    let history_c = roster_by_id(observer_c.roster(&session_id, SessionAgentListScope::History)?)?;
    if current_c != current_b || history_c != history_b {
        return Err("S7 Boot C changed current/history roster state".into());
    }

    let snapshot_c = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    super::assert_initialization_only_refresh(&snapshot_b, &snapshot_c)?;
    interruption::assert_unfinished_worker_dispatch(&snapshot_c, &identities.uncertain, &dispatch)?;

    let continuation_start = observer_c.events.len();
    observer_c.submit(
        &identities.repair,
        "s7-repair-continuation",
        REPAIR_CONTINUATION,
    )?;
    observer_c.wait_for_agent_marker(&identities.repair, REPAIR_MARKER, continuation_start)?;
    observer_c.wait_for_agent_idle_after(&identities.repair, continuation_start)?;
    assert_provider_turn_counts_by_agent(
        &observer_c.events,
        &provider_budget(
            &identities,
            S7ProviderTurnCounts {
                repair: 1,
                ..S7ProviderTurnCounts::ZERO
            },
        ),
    )?;
    if matched_action_count(&fixture)? != 9 {
        return Err("S7 repair continuation did not consume exactly its own final action".into());
    }
    assert_fake_checkpoint(&fixture, &scenario, &identities, COMPLETE_CURSORS)?;
    assert_lane_matches(&fixture, COMPLETE_CURSORS)?;
    let current_after =
        roster_by_id(observer_c.roster(&session_id, SessionAgentListScope::Current)?)?;
    let history_after =
        roster_by_id(observer_c.roster(&session_id, SessionAgentListScope::History)?)?;
    if current_after != current_b || history_after != history_b {
        return Err("S7 explicit repair continuation changed durable membership".into());
    }
    disconnect_ui(&mut observer_c.peer)?;
    daemon_c.finish()?;
    assert_no_uncertain_termination(&fixture, &identities, &dispatch)?;

    let snapshot_after = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    assert_owned_continuation(&snapshot_c, &snapshot_after, &identities)?;
    interruption::assert_unfinished_worker_dispatch(
        &snapshot_after,
        &identities.uncertain,
        &dispatch,
    )?;
    fixture.assert_consumed()?;
    Ok(())
}

/// Builds the closed four-lane S7 grammar within the existing eight-action
/// per-lane limit.
fn mixed_state_scenario(diagnostic: &str) -> ScenarioV2 {
    ScenarioV2::new(
        "s7-mixed-state-repeated-resume-isolation",
        vec![
            ScenarioLaneV2 {
                ctx_id: "s7-main".to_owned(),
                actions: vec![
                    ScenarioActionV2::AgentStartCall {
                        user_text: "start the quiescent deterministic worker".to_owned(),
                        call_id: "s7-start-quiescent".into(),
                        prompt: QUIESCENT_PROMPT.to_owned(),
                        role: Some(QUIESCENT_ROLE.to_owned()),
                        task_name: "quiescent worker".to_owned(),
                    },
                    ScenarioActionV2::AgentStartResult {
                        user_text: "start the quiescent deterministic worker".to_owned(),
                        call_id: "s7-start-quiescent".into(),
                        response: "quiescent worker start accepted".to_owned(),
                    },
                    ScenarioActionV2::WatchNotifications {
                        notifications: vec![WatchNotificationV2::Response {
                            content: "quiescent worker complete".to_owned(),
                        }],
                        response: "quiescent worker completion observed".to_owned(),
                    },
                    ScenarioActionV2::AgentStartCall {
                        user_text: "start the dispatch-uncertain deterministic worker".to_owned(),
                        call_id: "s7-start-uncertain".into(),
                        prompt: UNCERTAIN_PROMPT.to_owned(),
                        role: Some(UNCERTAIN_ROLE.to_owned()),
                        task_name: "dispatch-uncertain worker".to_owned(),
                    },
                    ScenarioActionV2::AgentStartResult {
                        user_text: "start the dispatch-uncertain deterministic worker".to_owned(),
                        call_id: "s7-start-uncertain".into(),
                        response: "dispatch-uncertain worker start accepted".to_owned(),
                    },
                ],
            },
            ScenarioLaneV2 {
                ctx_id: "s7-quiescent".to_owned(),
                actions: vec![ScenarioActionV2::Text {
                    user_text: QUIESCENT_INITIAL.to_owned(),
                    response: "quiescent worker complete".to_owned(),
                }],
            },
            ScenarioLaneV2 {
                ctx_id: "s7-uncertain".to_owned(),
                actions: vec![ScenarioActionV2::HoldUntilCancel {
                    user_text: UNCERTAIN_INITIAL.to_owned(),
                    timeout_ms: HOLD_TIMEOUT_MS,
                }],
            },
            ScenarioLaneV2 {
                ctx_id: "s7-repair".to_owned(),
                actions: vec![
                    ScenarioActionV2::DummyToolCall {
                        user_text: REPAIR_PROMPT.to_owned(),
                        call_id: TOOL_CALL_ID.into(),
                    },
                    ScenarioActionV2::DummyToolRepair {
                        user_text: REPAIR_CONTINUATION.to_owned(),
                        call_id: TOOL_CALL_ID.into(),
                        diagnostic: diagnostic.to_owned(),
                        response: REPAIR_MARKER.to_owned(),
                    },
                ],
            },
        ],
    )
}

/// Enforces the reviewed lane/action/encoded-size S7 budget before startup.
fn assert_scenario_budget(scenario: &ScenarioV2) -> Result<(), Box<dyn std::error::Error>> {
    let actions = scenario
        .lanes
        .iter()
        .map(|lane| lane.actions.len())
        .collect::<Vec<_>>();
    let encoded = serde_json::to_vec(scenario)?;
    if actions != COMPLETE_CURSORS || encoded.len() != SCENARIO_BYTES {
        return Err(format!(
            "S7 scenario budget changed: lanes={}, actions={actions:?}, bytes={}",
            scenario.lanes.len(),
            encoded.len()
        )
        .into());
    }
    Ok(())
}

/// Creates the repair worker through the existing bounded UI creation protocol,
/// avoiding a third production start pair that would exceed the reviewed fake
/// grammar's per-lane limit.
fn create_direct_repair_worker(
    observer: &mut SessionRestoreObserver,
    parent: &AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    let start = observer.events.len();
    observer
        .peer
        .send(&HarnessInputMessage::emit(Event::UiCreateAgent(
            tau_proto::UiCreateAgent {
                literal: false,
                session_id: SESSION
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                role: REPAIR_ROLE.to_owned(),
                model_override: None,
                metadata: Vec::new(),
                initial_prompt: Some(REPAIR_PROMPT.to_owned()),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: Some("s7-repair".to_owned()),
                parent_agent: Some(parent.clone()),
                ephemeral: false,
            },
        )))?;
    let mut next = start;
    let mut repair = None;
    let mut loaded = BTreeSet::new();
    loop {
        while let Some(observed) = observer.events.get(next) {
            next += 1;
            match &observed.event {
                Event::AgentStarted(started)
                    if !observed.replay
                        && started.role == REPAIR_ROLE
                        && started.parent_agent.as_ref() == Some(parent)
                        && !started.ephemeral =>
                {
                    if repair.replace(started.agent_id.clone()).is_some() {
                        return Err("S7 created more than one direct repair worker".into());
                    }
                }
                Event::SessionAgentLoaded(event) if !observed.replay && !event.ephemeral => {
                    loaded.insert(event.agent_id.clone());
                }
                _ => {}
            }
        }
        if repair
            .as_ref()
            .is_some_and(|agent_id| loaded.contains(agent_id))
        {
            return Ok(());
        }
        observer.recv_one()?;
    }
}

/// Returns the sole live creation identity for one exact role.
fn sole_live_agent_for_role(
    events: &[Observed],
    role: &str,
) -> Result<AgentId, Box<dyn std::error::Error>> {
    let ids = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::AgentStarted(started) if !observed.replay && started.role == role => {
                Some(started.agent_id.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let [agent_id] = ids.as_slice() else {
        return Err(format!("S7 expected one live `{role}` creation, got {ids:?}").into());
    };
    Ok(agent_id.clone())
}

/// Constructs one exact per-agent live provider-turn budget.
impl S7ProviderTurnCounts {
    /// Zero live provider turns for every S7 role.
    const ZERO: Self = Self {
        main: 0,
        quiescent: 0,
        uncertain: 0,
        repair: 0,
    };
}

/// Maps named S7 role counts to their harness-minted identities.
fn provider_budget(
    identities: &S7Identities,
    counts: S7ProviderTurnCounts,
) -> BTreeMap<AgentId, usize> {
    BTreeMap::from([
        (identities.main.clone(), counts.main),
        (identities.quiescent.clone(), counts.quiescent),
        (identities.uncertain.clone(), counts.uncertain),
        (identities.repair.clone(), counts.repair),
    ])
}

/// Waits for the exact mixed Boot-A runtime composition: quiescent main/worker,
/// held inference on the uncertain worker, and one in-flight repair-worker
/// tool.
fn wait_for_boot_a_runtime_states(
    observer: &mut SessionRestoreObserver,
    identities: &S7Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut latest = BTreeMap::new();
    let mut next = 0;
    loop {
        while let Some(observed) = observer.events.get(next) {
            next += 1;
            if let Event::AgentStatsUpdated(stats) = &observed.event {
                latest.insert(
                    stats.agent_id.clone(),
                    (stats.runtime_state, stats.tools.in_flight),
                );
            }
        }
        if latest.len() == 4
            && latest.get(&identities.main) == Some(&(AgentRuntimeState::Idle, 0))
            && latest.get(&identities.quiescent) == Some(&(AgentRuntimeState::Idle, 0))
            && latest.get(&identities.uncertain) == Some(&(AgentRuntimeState::Running, 0))
            && latest.get(&identities.repair) == Some(&(AgentRuntimeState::Running, 1))
        {
            return Ok(());
        }
        observer.recv_one()?;
    }
}

/// Requires exact durable membership and immutable role ownership at the crash
/// cut.
fn assert_boot_a_durable_state(
    snapshot: &DurableSessionSnapshot,
    identities: &S7Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    let expected = identities
        .all()
        .into_iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if snapshot
        .agent_events
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>()
        != expected
    {
        return Err("S7 Boot A durable agent set changed".into());
    }
    for (agent_id, role, parent, name) in [
        (&identities.main, "deterministic-main", None, None),
        (
            &identities.quiescent,
            QUIESCENT_ROLE,
            Some(&identities.main),
            Some("quiescent worker"),
        ),
        (
            &identities.uncertain,
            UNCERTAIN_ROLE,
            Some(&identities.main),
            Some("dispatch-uncertain worker"),
        ),
        (
            &identities.repair,
            REPAIR_ROLE,
            Some(&identities.main),
            None,
        ),
    ] {
        let records = &snapshot.agent_events[agent_id];
        let starts = records
            .iter()
            .filter_map(|record| match &record.event {
                Event::AgentStarted(started) if &started.agent_id == agent_id => Some(started),
                _ => None,
            })
            .collect::<Vec<_>>();
        let [started] = starts.as_slice() else {
            return Err(format!("S7 {agent_id} lacks one durable creation fact").into());
        };
        if started.role != role
            || started.parent_agent.as_ref() != parent
            || started.display_name.as_deref() != name
            || started.ephemeral
        {
            return Err(
                format!("S7 immutable creation changed for {agent_id}: {started:?}").into(),
            );
        }
    }
    Ok(())
}

/// Requires Boot B to preserve every non-repair journal and append one exact
/// provider error only to the repair worker.
fn assert_boot_b_durable_repair(
    before: &DurableSessionSnapshot,
    after: &DurableSessionSnapshot,
    identities: &S7Identities,
    diagnostic: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    after.require_prefix(before)?;
    if after.session_events != before.session_events
        || after.restore_events != before.restore_events
    {
        return Err("S7 Boot B changed membership or execution-restore streams".into());
    }
    for agent_id in [
        &identities.main,
        &identities.quiescent,
        &identities.uncertain,
    ] {
        if !super::suffix_after_initialization(before, after, agent_id)?.is_empty() {
            return Err(format!("S7 Boot B appended repair state to {agent_id}").into());
        }
    }
    let suffix = super::suffix_after_initialization(before, after, &identities.repair)?;
    let [classification, record] = suffix else {
        return Err(format!("S7 repair suffix contained {} records", suffix.len()).into());
    };
    if !matches!(
        &record.event,
        Event::ProviderToolError(error)
            if error.call_id.as_str() == TOOL_CALL_ID
                && error.tool_name.as_str() == "restart_test_dummy"
                && error.message == diagnostic
    ) {
        return Err("S7 repair suffix was not the exact worker-owned provider error".into());
    }
    if !matches!(
        &classification.event,
        Event::AgentToolTerminalClassified(value)
            if value.terminal == record.observation_id
                && value.cause == tau_proto::ToolTerminalCause::RestartRepair
    ) {
        return Err("S7 repair suffix lacked the exact restart-repair classification".into());
    }
    Ok(())
}

/// Requires one exact uncertain warning and rejects restore warnings naming any
/// other S7 agent.
fn assert_owned_uncertain_warning(
    events: &[Observed],
    identities: &S7Identities,
) -> Result<tau_proto::UnixMicros, Box<dyn std::error::Error>> {
    let recorded = interruption::assert_dispatch_uncertain_notice(events, &identities.uncertain)?;
    let restore_warnings = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::HarnessNotice(notice)
                if notice
                    .message
                    .starts_with("inference dispatch for restored agent `") =>
            {
                Some(notice.message.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let expected = format!(
        "inference dispatch for restored agent `{}` is uncertain; retry explicitly",
        identities.uncertain
    );
    if restore_warnings != [expected.as_str()] {
        return Err(format!(
            "S7 restore warning was not solely uncertain-worker-owned: {restore_warnings:?}"
        )
        .into());
    }
    Ok(recorded)
}

/// Rejects restored automatic-watch topology or fresh model-visible worker
/// notifications in a no-input generation.
fn assert_no_restored_worker_watch(
    events: &[Observed],
    identities: &S7Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    let workers = BTreeSet::from([
        identities.quiescent.clone(),
        identities.uncertain.clone(),
        identities.repair.clone(),
    ]);
    if events.iter().any(|observed| {
        (!observed.replay
            && matches!(
                &observed.event,
                Event::AgentWatchesUpdated(update)
                    if update.watcher_id == identities.main
                        && update.watched_agent_ids.iter().any(|id| workers.contains(id))
            ))
            || (!observed.replay
                && matches!(
                    &observed.event,
                    Event::AgentMessageReceived(message)
                        if message.recipient_id == identities.main
                            && workers.contains(&message.sender_id)
                ))
    }) {
        return Err("S7 restored or re-fanned a daemon-lifetime worker watch".into());
    }
    Ok(())
}

/// Requires a no-input restore generation to create no fresh inference dispatch
/// for any S7 agent.
fn assert_no_live_restore_dispatch(
    events: &[Observed],
    identities: &S7Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    let agents = identities
        .all()
        .into_iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if events.iter().any(|observed| {
        !observed.replay
            && matches!(
                &observed.event,
                Event::AgentInferenceDispatchStarted(dispatch)
                    if agents.contains(&dispatch.agent_id)
            )
    }) {
        return Err("S7 no-input resume created a fresh agent inference dispatch".into());
    }
    Ok(())
}

/// Requires the append-only published-event projection to contain no transient
/// terminal for the original uncertain prompt.
fn assert_no_uncertain_termination(
    fixture: &DeterministicFixture,
    identities: &S7Identities,
    dispatch: &tau_proto::AgentInferenceDispatchStarted,
) -> Result<(), Box<dyn std::error::Error>> {
    let terminations = fixture
        .published_trace_events()?
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                Event::AgentPromptTerminated(terminated)
                    if terminated.agent_id == identities.uncertain
                        && terminated.agent_prompt_id == dispatch.agent_prompt_id
            )
        })
        .collect::<Vec<_>>();
    if !terminations.is_empty() {
        return Err(
            format!("S7 uncertain prompt unexpectedly terminalized: {terminations:?}").into(),
        );
    }
    Ok(())
}

/// Converts one directed roster response into an identity-keyed set.
fn roster_by_id(
    rows: Vec<SessionAgentListEntry>,
) -> Result<BTreeMap<AgentId, SessionAgentListEntry>, Box<dyn std::error::Error>> {
    let mut by_id = BTreeMap::new();
    for row in rows {
        if by_id.insert(row.agent_id.clone(), row).is_some() {
            return Err("S7 roster returned a duplicate identity".into());
        }
    }
    Ok(by_id)
}

/// Requires all four restored identities, immutable creation facts, and idle
/// live navigation defaults.
fn assert_mixed_roster(
    roster: &BTreeMap<AgentId, SessionAgentListEntry>,
    identities: &S7Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    let expected = identities
        .all()
        .into_iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if roster.keys().cloned().collect::<BTreeSet<_>>() != expected {
        return Err(format!("S7 restored roster identity set changed: {roster:?}").into());
    }
    let rows = roster.values().cloned().collect::<Vec<_>>();
    for (agent_id, mode, role, parent, name) in [
        (
            &identities.main,
            AgentNavigationMode::Active,
            "deterministic-main",
            None,
            None,
        ),
        (
            &identities.quiescent,
            AgentNavigationMode::ActiveAuto,
            QUIESCENT_ROLE,
            Some(&identities.main),
            Some("quiescent worker"),
        ),
        (
            &identities.uncertain,
            AgentNavigationMode::ActiveAuto,
            UNCERTAIN_ROLE,
            Some(&identities.main),
            Some("dispatch-uncertain worker"),
        ),
        (
            &identities.repair,
            AgentNavigationMode::Active,
            REPAIR_ROLE,
            Some(&identities.main),
            None,
        ),
    ] {
        assert_idle_live_roster_row(
            &rows,
            agent_id,
            super::IdleLiveRosterExpectation {
                persistence: SessionAgentPersistence::Durable,
                navigation_mode: mode,
                role,
                parent,
                display_name: name,
            },
        )?;
    }
    Ok(())
}

/// Decodes the exact checkpoint and proves that no restore generation rebound
/// an agent lane or production child ordinal.
fn assert_fake_checkpoint(
    fixture: &DeterministicFixture,
    scenario: &ScenarioV2,
    identities: &S7Identities,
    expected_cursors: [usize; 4],
) -> Result<(), Box<dyn std::error::Error>> {
    let path = fixture
        .harness_state_dir()
        .join("ext/e2e-fake-provider/scenario-cursor.json");
    let bytes = std::fs::read(path)?;
    if bytes.len() > MAX_CHECKPOINT_BYTES {
        return Err(format!("S7 fake checkpoint exceeded {MAX_CHECKPOINT_BYTES} bytes").into());
    }
    let checkpoint: CursorCheckpoint = serde_json::from_slice(&bytes)?;
    if checkpoint.scenario != *scenario || checkpoint.cursors != expected_cursors {
        return Err(format!(
            "S7 fake checkpoint scenario/cursors changed: {:?}",
            checkpoint.cursors
        )
        .into());
    }
    if checkpoint.agent_lanes.len() != 4 || checkpoint.child_agents.len() != 2 {
        return Err(format!(
            "S7 fake checkpoint row counts changed: bindings={}, children={}",
            checkpoint.agent_lanes.len(),
            checkpoint.child_agents.len()
        )
        .into());
    }
    let mut actual_bindings = BTreeMap::new();
    for row in checkpoint.agent_lanes {
        if actual_bindings
            .insert(row.agent_id, row.lane_index)
            .is_some()
        {
            return Err("S7 fake checkpoint duplicated an agent lane binding".into());
        }
    }
    if actual_bindings != identities.lane_bindings() {
        return Err(format!("S7 fake lane bindings changed: {actual_bindings:?}").into());
    }
    let mut actual_children = BTreeMap::new();
    for row in checkpoint.child_agents {
        if actual_children
            .insert(row.start_ordinal, (row.parent_agent_id, row.child_agent_id))
            .is_some()
        {
            return Err("S7 fake checkpoint duplicated a child start ordinal".into());
        }
    }
    let expected_children = BTreeMap::from([
        (0, (identities.main.clone(), identities.quiescent.clone())),
        (1, (identities.main.clone(), identities.uncertain.clone())),
    ]);
    if actual_children != expected_children {
        return Err(format!("S7 fake child associations changed: {actual_children:?}").into());
    }
    Ok(())
}

/// Requires every configured lane action to match exactly once and no other
/// lane/action marker to appear.
fn assert_lane_matches(
    fixture: &DeterministicFixture,
    expected: [usize; 4],
) -> Result<(), Box<dyn std::error::Error>> {
    let trace = fixture.trace()?;
    let lanes = ["s7-main", "s7-quiescent", "s7-uncertain", "s7-repair"];
    for (lane, actions) in lanes.into_iter().zip(expected) {
        for action in 0..actions {
            let marker = format!("lane={lane} action={action} ");
            if trace
                .lines()
                .filter(|line| line.contains(&marker) && line.contains(" matched "))
                .count()
                != 1
            {
                return Err(format!("S7 lane `{lane}` action {action} match count changed").into());
            }
        }
    }
    Ok(())
}

/// Requires the post-Boot-C explicit continuation to append only to the repair
/// worker and to contain its own exact prompt/response pair.
fn assert_owned_continuation(
    before: &DurableSessionSnapshot,
    after: &DurableSessionSnapshot,
    identities: &S7Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    after.require_prefix(before)?;
    if after.session_events != before.session_events
        || after.restore_events != before.restore_events
    {
        return Err("S7 repair continuation changed session-owned durable streams".into());
    }
    for agent_id in [
        &identities.main,
        &identities.quiescent,
        &identities.uncertain,
    ] {
        if after.agent_events[agent_id] != before.agent_events[agent_id] {
            return Err(format!("S7 repair continuation leaked into {agent_id}").into());
        }
    }
    let suffix =
        &after.agent_events[&identities.repair][before.agent_events[&identities.repair].len()..];
    if count_prompt(suffix, &identities.repair, REPAIR_CONTINUATION) != 1
        || count_response(suffix, &identities.repair, REPAIR_MARKER) != 1
        || suffix.iter().any(|record| {
            matches!(
                record.event,
                Event::ProviderToolError(_) | Event::ToolError(_) | Event::ToolStarted(_)
            )
        })
    {
        return Err("S7 repair continuation suffix was not solely repair-worker-owned".into());
    }
    Ok(())
}
