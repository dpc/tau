//! Synchronized interrupted-worker restore acceptance.

use serde::Deserialize;
use tau_e2e_tests::{AgentWatchResultExpectationV2, WatchNotificationV2};
use tau_proto::{
    AgentId, AgentMessageKind, AgentPromptId, AgentWatchProviderCategory, AgentWatchProviderState,
    AgentWatchUpdateCause, Event, SessionAgentListScope, SessionId,
};

use super::super::daemon_support::{disconnect_ui, spawn_daemon};
use super::{
    BootIdentities, DeterministicFixture, DurableSessionSnapshot, FAKE_PROVIDER, Observed,
    ProviderTurnCounts, SESSION, ScenarioActionV2, ScenarioLaneV2, ScenarioV2,
    SessionRestoreObserver, WORKER_PROMPT, WORKER_PROVIDER_INITIAL, assert_provider_turn_counts,
    assert_restored_roster, assert_resume_boundaries, count_prompt, count_response,
    initial_live_watch_subscription_id, interruption_support as interruption, matched_action_count,
};

/// Exact compact JSON size of the reviewed S5 scenario grammar.
const SCENARIO_BYTES: usize = 1_470;
/// Existing bounded hold deadline used only to keep the provider prompt in
/// flight until the synchronized process-group kill.
const HOLD_TIMEOUT_MS: u64 = 10_000;
/// Maximum accepted size of the fake provider's durable cursor checkpoint.
const MAX_CHECKPOINT_BYTES: usize = 64 * 1024;

/// Decoded fake-provider cursor checkpoint observed at the crash cut.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorCheckpoint {
    /// Complete scenario identity associated with these cursors.
    scenario: ScenarioV2,
    /// Next action index for each configured lane.
    cursors: Vec<usize>,
    /// Immutable harness-agent lane bindings.
    agent_lanes: Vec<AgentLaneCheckpoint>,
    /// Harness-minted child identities retained for tool continuations.
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

/// Proves a synchronized worker crash restores as dispatch-uncertain without
/// automatically resubmitting provider work across two cold resumes.
#[test]
fn cold_resume_fails_closed_for_dispatch_uncertain_worker() -> Result<(), Box<dyn std::error::Error>>
{
    let session_id = SessionId::parse(SESSION).expect("known-safe SessionId must be valid");
    let scenario = dispatch_uncertain_scenario();
    assert_scenario_budget(&scenario)?;
    let fixture = DeterministicFixture::new_session_restore_watch(
        "cold_resume_fails_closed_for_dispatch_uncertain_worker",
        &scenario,
        FAKE_PROVIDER,
    )?;
    fixture.assert_session_restore_watch_roles()?;

    let socket_a = fixture.socket_path("s5-boot-a");
    let daemon_a = spawn_daemon(&fixture, &socket_a, tau_harness::SessionLaunchStatus::New);
    let mut observer_a = SessionRestoreObserver::connect(&socket_a)?;
    observer_a.create_main("s5-main", "start the held deterministic worker")?;
    observer_a.wait_for_agent_role("deterministic-worker")?;
    let main_terminal_prompt = observer_a.wait_for_marker("held worker start accepted")?;
    let identities = BootIdentities::from_events(&observer_a.events)?;
    let boot_a_subscription_id =
        initial_live_watch_subscription_id(&observer_a.events, &identities, &session_id)?;
    let dispatch = interruption::wait_for_worker_dispatch(&mut observer_a, &identities.worker)?;
    interruption::wait_for_durable_dispatch(&fixture, &session_id, &identities.worker, &dispatch)?;
    interruption::wait_for_hold_readiness(&fixture, &dispatch.agent_prompt_id)?;
    let durable_a = interruption::wait_for_durable_response(
        &fixture,
        &session_id,
        &identities.main,
        &main_terminal_prompt,
    )?;
    assert_fake_checkpoint(&fixture, &scenario, &identities, [2, 1])?;
    interruption::assert_unfinished_worker_dispatch(&durable_a, &identities.worker, &dispatch)?;
    assert_provider_turn_counts(
        &observer_a.events,
        &identities,
        ProviderTurnCounts { main: 2, worker: 0 },
    )?;
    if matched_action_count(&fixture)? != 3 {
        return Err("S5 Boot A did not stop at the exact three-action crash cut".into());
    }
    interruption::assert_hold_ready_and_live(&fixture, &dispatch.agent_prompt_id)?;
    daemon_a.kill_ungracefully()?;
    drop(observer_a);

    let snapshot_a = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    interruption::assert_unfinished_worker_dispatch(&snapshot_a, &identities.worker, &dispatch)?;

    let socket_b = fixture.socket_path("s5-boot-b");
    let daemon_b = spawn_daemon(
        &fixture,
        &socket_b,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut observer_b = SessionRestoreObserver::connect(&socket_b)?;
    observer_b.wait_for_session_boundary(&session_id)?;
    assert_resume_boundaries(&observer_b.events, &identities.all(), &session_id)?;
    interruption::assert_dispatch_uncertain_notice(&observer_b.events, &identities.worker)?;
    assert_no_restored_watch(&observer_b.events, &identities)?;
    assert_provider_turn_counts(
        &observer_b.events,
        &identities,
        ProviderTurnCounts { main: 0, worker: 0 },
    )?;
    if matched_action_count(&fixture)? != 3 {
        return Err("S5 Boot B replay automatically consumed provider work".into());
    }
    let roster_b = observer_b.roster(&session_id, SessionAgentListScope::Current)?;
    assert_restored_roster(&roster_b, &identities)?;

    let watch_start = observer_b.events.len();
    observer_b.submit(
        &identities.main,
        "s5-watch",
        "recreate uncertain worker watch",
    )?;
    observer_b.wait_for_agent_marker(
        &identities.main,
        "uncertain worker watch recreated",
        watch_start,
    )?;
    observer_b.wait_for_agent_idle_after(&identities.main, watch_start)?;
    assert_dispatch_uncertain_watch(
        &observer_b.events[watch_start..],
        &identities,
        &session_id,
        &boot_a_subscription_id,
        &dispatch.agent_prompt_id,
    )?;
    assert_provider_turn_counts(
        &observer_b.events,
        &identities,
        ProviderTurnCounts { main: 2, worker: 0 },
    )?;
    if matched_action_count(&fixture)? != 5 {
        return Err("S5 Boot B explicit watch exceeded its two-action main budget".into());
    }
    assert_fake_checkpoint(&fixture, &scenario, &identities, [4, 1])?;
    let worker_start = observer_b.events.len();
    observer_b.submit(
        &identities.worker,
        "s5-fresh-worker",
        "fresh worker after uncertainty",
    )?;
    observer_b.wait_for_agent_marker(
        &identities.worker,
        "fresh worker after uncertainty complete",
        worker_start,
    )?;
    observer_b.wait_for_agent_idle_after(&identities.worker, worker_start)?;
    observer_b.wait_for_agent_marker(
        &identities.main,
        "fresh worker response observed",
        worker_start,
    )?;
    observer_b.wait_for_agent_idle_after(&identities.main, worker_start)?;
    assert_provider_turn_counts(
        &observer_b.events,
        &identities,
        ProviderTurnCounts { main: 4, worker: 1 },
    )?;
    if matched_action_count(&fixture)? != 8 {
        return Err(
            "S5 Boot B HumanUI supersession did not consume fresh worker/watch actions".into(),
        );
    }
    assert_fake_checkpoint(&fixture, &scenario, &identities, [6, 2])?;
    disconnect_ui(&mut observer_b.peer)?;
    daemon_b.finish()?;

    let snapshot_b = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    snapshot_b.require_prefix(&snapshot_a)?;
    interruption::assert_unfinished_worker_dispatch(&snapshot_b, &identities.worker, &dispatch)?;
    let main_suffix =
        super::suffix_after_initialization(&snapshot_a, &snapshot_b, &identities.main)?;
    if count_prompt(
        main_suffix,
        &identities.main,
        "recreate uncertain worker watch",
    ) != 1
        || count_response(
            main_suffix,
            &identities.main,
            "uncertain worker watch recreated",
        ) != 1
    {
        return Err("S5 main work did not follow its exact initialization refresh".into());
    }
    let worker_suffix =
        super::suffix_after_initialization(&snapshot_a, &snapshot_b, &identities.worker)?;
    let successor_ids = worker_suffix
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentInferenceDispatchStarted(started) => Some(started.agent_prompt_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if successor_ids.len() != 1 || successor_ids[0] == dispatch.agent_prompt_id {
        return Err(format!(
            "S5 successor checkpoint did not use one fresh prompt id: {successor_ids:?}"
        )
        .into());
    }
    let successor_id = &successor_ids[0];
    let old_outer_turn = tau_proto::AgentOuterTurnId::for_prompt(&dispatch.agent_prompt_id);
    let successor_outer_turn = tau_proto::AgentOuterTurnId::for_prompt(successor_id);
    let old_redispatches = worker_suffix
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentInferenceDispatchStarted(started)
                    if started.agent_prompt_id == dispatch.agent_prompt_id
            ) || matches!(
                &record.event,
                Event::AgentPromptStarted(started)
                    if started.agent_prompt_id == dispatch.agent_prompt_id
            ) || matches!(
                &record.event,
                Event::AgentPromptCreated(created)
                    if created.agent_prompt_id == dispatch.agent_prompt_id
            )
        })
        .count();
    let successor_starts = worker_suffix
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentPromptStarted(started)
                    if &started.agent_prompt_id == successor_id
            )
        })
        .count();
    let successor_prompt_start_bindings = worker_suffix
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentPromptStarted(started)
                    if &started.agent_prompt_id == successor_id
                        && started.agent_id == identities.worker
                        && started.session_id == session_id
                        && started.outer_turn_id.as_ref() == Some(&successor_outer_turn)
            )
        })
        .count();
    let successor_creates = observer_b.events[worker_start..]
        .iter()
        .filter(|observed| {
            matches!(
                &observed.event,
                Event::AgentPromptCreated(created)
                    if &created.agent_prompt_id == successor_id
            )
        })
        .count();
    let successor_responses = worker_suffix
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::ProviderResponseFinished(response)
                    if &response.agent_prompt_id == successor_id
            )
        })
        .count();
    let old_outer_starts = worker_suffix
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentOuterTurnStarted(started)
                    if started.outer_turn_id == old_outer_turn
            )
        })
        .count();
    let old_outer_starts_total = snapshot_b.agent_events[&identities.worker]
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentOuterTurnStarted(started)
                    if started.outer_turn_id == old_outer_turn
                        && started.agent_prompt_id == dispatch.agent_prompt_id
                        && started.agent_id == identities.worker
                        && started.session_id == session_id
            )
        })
        .count();
    let successor_prompt_outer_starts_total = worker_suffix
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentOuterTurnStarted(started)
                    if &started.agent_prompt_id == successor_id
            )
        })
        .count();
    let successor_outer_starts = worker_suffix
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentOuterTurnStarted(started)
                    if started.outer_turn_id == successor_outer_turn
                        && &started.agent_prompt_id == successor_id
                        && started.agent_id == identities.worker
                        && started.session_id == session_id
            )
        })
        .count();
    let old_finishes = snapshot_b.agent_events[&identities.worker]
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentOuterTurnFinished(finished)
                    if finished.outer_turn_id == old_outer_turn
            )
        })
        .count();
    let successor_finishes = worker_suffix
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentOuterTurnFinished(finished)
                    if finished.outer_turn_id == successor_outer_turn
            )
        })
        .count();
    if count_prompt(
        worker_suffix,
        &identities.worker,
        "fresh worker after uncertainty",
    ) != 1
        || count_response(
            worker_suffix,
            &identities.worker,
            "fresh worker after uncertainty complete",
        ) != 1
        || worker_suffix
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::AgentPromptTerminated(terminated)
                        if terminated.agent_prompt_id == dispatch.agent_prompt_id
                            && terminated.reason
                                == tau_proto::AgentPromptTerminationReason::Stale
                )
            })
            .count()
            != 1
        || old_redispatches != 0
        || successor_starts != 1
        || successor_prompt_start_bindings != 1
        || successor_creates != 1
        || successor_responses != 1
        || old_outer_starts != 0
        || old_outer_starts_total != 1
        || successor_prompt_outer_starts_total != 1
        || successor_outer_starts != 1
        || old_finishes != 0
        || successor_finishes != 1
    {
        return Err(format!(
            "S5 Boot B worker authority mismatch: old_redispatches={old_redispatches}, \
             successor_starts={successor_starts}, \
             successor_prompt_start_bindings={successor_prompt_start_bindings}, \
             successor_creates={successor_creates}, successor_responses={successor_responses}, \
             old_outer_starts={old_outer_starts}, old_outer_starts_total={old_outer_starts_total}, \
             successor_prompt_outer_starts_total={successor_prompt_outer_starts_total}, \
             successor_outer_starts={successor_outer_starts}, \
             old_finishes={old_finishes}, successor_finishes={successor_finishes}"
        )
        .into());
    }

    let socket_c = fixture.socket_path("s5-boot-c");
    let daemon_c = spawn_daemon(
        &fixture,
        &socket_c,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut observer_c = SessionRestoreObserver::connect(&socket_c)?;
    observer_c.wait_for_session_boundary(&session_id)?;
    assert_resume_boundaries(&observer_c.events, &identities.all(), &session_id)?;
    assert_no_restored_watch(&observer_c.events, &identities)?;
    assert_provider_turn_counts(
        &observer_c.events,
        &identities,
        ProviderTurnCounts { main: 0, worker: 0 },
    )?;
    if matched_action_count(&fixture)? != 8 {
        return Err("S5 Boot C automatically consumed provider work".into());
    }
    assert_fake_checkpoint(&fixture, &scenario, &identities, [6, 2])?;
    let roster_c = observer_c.roster(&session_id, SessionAgentListScope::Current)?;
    assert_restored_roster(&roster_c, &identities)?;
    disconnect_ui(&mut observer_c.peer)?;
    daemon_c.finish()?;

    let snapshot_c = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    super::assert_initialization_only_refresh(&snapshot_b, &snapshot_c)?;
    fixture.assert_consumed()?;
    Ok(())
}

/// Builds the closed five-main/one-worker S5 action grammar.
fn dispatch_uncertain_scenario() -> ScenarioV2 {
    ScenarioV2::new(
        "s5-worker-dispatch-uncertain",
        vec![
            ScenarioLaneV2 {
                ctx_id: "s5-main".to_owned(),
                actions: vec![
                    ScenarioActionV2::AgentStartCall {
                        user_text: "start the held deterministic worker".to_owned(),
                        call_id: "s5-agent-start".into(),
                        prompt: WORKER_PROMPT.to_owned(),
                        role: "deterministic-worker".to_owned(),
                    },
                    ScenarioActionV2::AgentStartResult {
                        user_text: "start the held deterministic worker".to_owned(),
                        call_id: "s5-agent-start".into(),
                        response: "held worker start accepted".to_owned(),
                    },
                    ScenarioActionV2::AgentWatchCall {
                        user_text: "recreate uncertain worker watch".to_owned(),
                        call_id: "s5-agent-watch".into(),
                    },
                    ScenarioActionV2::AgentWatchResult {
                        user_text: "recreate uncertain worker watch".to_owned(),
                        call_id: "s5-agent-watch".into(),
                        expectation: AgentWatchResultExpectationV2::DispatchUncertainUnknown,
                        response: "uncertain worker watch recreated".to_owned(),
                    },
                    ScenarioActionV2::WatchNotifications {
                        notifications: vec![WatchNotificationV2::Prompt {
                            content: "fresh worker after uncertainty".to_owned(),
                        }],
                        response: "fresh worker prompt observed".to_owned(),
                    },
                    ScenarioActionV2::WatchNotifications {
                        notifications: vec![WatchNotificationV2::Response {
                            content: "fresh worker after uncertainty complete".to_owned(),
                        }],
                        response: "fresh worker response observed".to_owned(),
                    },
                ],
            },
            ScenarioLaneV2 {
                ctx_id: "s5-worker".to_owned(),
                actions: vec![
                    ScenarioActionV2::HoldUntilCancel {
                        user_text: WORKER_PROVIDER_INITIAL.to_owned(),
                        timeout_ms: HOLD_TIMEOUT_MS,
                    },
                    ScenarioActionV2::Text {
                        user_text: "fresh worker after uncertainty".to_owned(),
                        response: "fresh worker after uncertainty complete".to_owned(),
                    },
                ],
            },
        ],
    )
}

/// Enforces the reviewed S5 lane/action/encoded-size budget before startup.
fn assert_scenario_budget(scenario: &ScenarioV2) -> Result<(), Box<dyn std::error::Error>> {
    let actions = scenario
        .lanes
        .iter()
        .map(|lane| lane.actions.len())
        .collect::<Vec<_>>();
    let encoded = serde_json::to_vec(scenario)?;
    if actions != [6, 2] || encoded.len() != SCENARIO_BYTES {
        return Err(format!(
            "S5 scenario budget changed: lanes={}, actions={actions:?}, bytes={}",
            scenario.lanes.len(),
            encoded.len()
        )
        .into());
    }
    Ok(())
}

/// Decodes and validates the fake checkpoint's exact scenario, lane bindings,
/// child identity, and next-action cursors.
fn assert_fake_checkpoint(
    fixture: &DeterministicFixture,
    scenario: &ScenarioV2,
    identities: &BootIdentities,
    expected_cursors: [usize; 2],
) -> Result<(), Box<dyn std::error::Error>> {
    let path = fixture
        .harness_state_dir()
        .join("ext/e2e-fake-provider/scenario-cursor.json");
    let bytes = std::fs::read(&path)?;
    if bytes.len() > MAX_CHECKPOINT_BYTES {
        return Err(format!("fake checkpoint exceeded {MAX_CHECKPOINT_BYTES} bytes").into());
    }
    let checkpoint: CursorCheckpoint = serde_json::from_slice(&bytes)?;
    if checkpoint.scenario != *scenario || checkpoint.cursors != expected_cursors {
        return Err(format!(
            "fake checkpoint scenario/cursors changed: {:?}",
            checkpoint.cursors
        )
        .into());
    }
    if checkpoint.agent_lanes.len() != 2
        || !checkpoint
            .agent_lanes
            .iter()
            .any(|row| row.agent_id == identities.main && row.lane_index == 0)
        || !checkpoint
            .agent_lanes
            .iter()
            .any(|row| row.agent_id == identities.worker && row.lane_index == 1)
    {
        return Err("fake checkpoint agent-to-lane binding changed".into());
    }
    let [child] = checkpoint.child_agents.as_slice() else {
        return Err("fake checkpoint did not retain exactly one child association".into());
    };
    if child.parent_agent_id != identities.main
        || child.start_ordinal != 0
        || child.child_agent_id != identities.worker
    {
        return Err("fake checkpoint parent/child association changed".into());
    }
    Ok(())
}

/// Rejects restoration of the daemon-lifetime automatic watch relation or any
/// fresh worker-to-main watch notification.
fn assert_no_restored_watch(
    events: &[Observed],
    identities: &BootIdentities,
) -> Result<(), Box<dyn std::error::Error>> {
    let restored_topology = events.iter().any(|observed| {
        matches!(
            &observed.event,
            Event::AgentWatchesUpdated(update)
                if update.watcher_id == identities.main
                    && update.watched_agent_ids.contains(&identities.worker)
        )
    });
    let live_refanout = events.iter().any(|observed| {
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
                                | AgentMessageKind::WatchWorkStatus
                                | AgentMessageKind::WatchProviderStatus
                        )
            )
    });
    if restored_topology || live_refanout {
        return Err("cold resume restored or re-fanned the old automatic watch".into());
    }
    Ok(())
}

/// Requires one fresh explicit watch and its exact non-model-visible initial
/// dispatch-uncertain provider snapshot.
fn assert_dispatch_uncertain_watch(
    events: &[Observed],
    identities: &BootIdentities,
    session_id: &SessionId,
    old_subscription_id: &str,
    prompt_id: &AgentPromptId,
) -> Result<(), Box<dyn std::error::Error>> {
    let subscription_id = initial_live_watch_subscription_id(events, identities, session_id)?;
    if subscription_id == old_subscription_id {
        return Err("explicit S5 watch reused the Boot A subscription identity".into());
    }
    let updates = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::AgentWatchesUpdated(update)
                if !observed.replay && update.watcher_id == identities.main =>
            {
                Some(update)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let watch_messages = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::AgentMessageReceived(message)
                if !observed.replay
                    && message.sender_id == identities.worker
                    && message.recipient_id == identities.main
                    && matches!(
                        message.kind,
                        AgentMessageKind::WatchPrompt
                            | AgentMessageKind::WatchResponse
                            | AgentMessageKind::WatchWorkStatus
                            | AgentMessageKind::WatchProviderStatus
                    ) =>
            {
                Some(message)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let [update] = updates.as_slice() else {
        return Err(format!(
            "explicit S5 watch emitted {} main-owned topology updates",
            updates.len()
        )
        .into());
    };
    if &update.session_id != session_id
        || update.watched_agent_ids != [identities.worker.clone()]
        || update.changed_agent_id.as_ref() != Some(&identities.worker)
        || update.cause != AgentWatchUpdateCause::AgentWatchEnable
    {
        return Err(format!("explicit S5 watch topology changed: {update:?}").into());
    }
    let [work_message, status_message] = watch_messages.as_slice() else {
        return Err(format!(
            "explicit S5 watch emitted {} worker-to-main snapshots",
            watch_messages.len()
        )
        .into());
    };
    let Some(work) = work_message.watch_work_status.as_ref() else {
        return Err("explicit S5 watch did not publish its work-status snapshot first".into());
    };
    let Some(status) = status_message.watch_provider_status.as_ref() else {
        return Err("explicit S5 watch did not publish its provider snapshot second".into());
    };
    if work_message.kind != AgentMessageKind::WatchWorkStatus
        || work_message.watch_provider_status.is_some()
        || !work.initial
        || &work.session_id != session_id
        || work.subscription_id != subscription_id
        || work.phase != tau_proto::AgentWorkStatusPhase::Unreported
        || status_message.kind != AgentMessageKind::WatchProviderStatus
        || !status.initial
        || &status.session_id != session_id
        || status.subscription_id != subscription_id
        || &status.agent_prompt_id != prompt_id
        || status.state
            != (AgentWatchProviderState::DispatchUncertain {
                category: AgentWatchProviderCategory::Unknown,
            })
    {
        return Err("explicit S5 watch initial snapshot payload or ordering changed".into());
    }
    Ok(())
}
