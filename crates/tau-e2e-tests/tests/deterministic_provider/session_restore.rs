//! Cold-resume acceptance for durable worker restoration, watch recreation,
//! and loaded/unloaded/ephemeral membership composition.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use tau_e2e_tests::{
    AgentWatchResultExpectationV2, DeterministicFixture, DurableSessionSnapshot, ScenarioActionV2,
    ScenarioLaneV2, ScenarioV2, WatchNotificationV2,
};
use tau_proto::{
    AgentId, AgentMessageKind, AgentNavigationMode, AgentRuntimeState, AgentWatchUpdateCause,
    Event, SessionAgentFacts, SessionAgentLifecycle, SessionAgentListEntry, SessionAgentListScope,
    SessionAgentPersistence, SessionId,
};

use super::daemon_support::{disconnect_ui, spawn_daemon};
use super::{DUMMY_TOOL, FAKE_PROVIDER};

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
const WORKER_PROMPT: &str = "Complete the deterministic worker instruction.";
const WORKER_INITIAL: &str = concat!(
    "[tau-internal]: You were started by an agent `main`. Your responses will be delivered to it. ",
    "You can use the `message` tool to communicate with agents.\n\n",
    "Complete the deterministic worker instruction."
);

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
                        user_text: WORKER_INITIAL.to_owned(),
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
    let boot_a_action_matches = fixture
        .trace()?
        .lines()
        .filter(|line| line.contains(" matched "))
        .count();
    disconnect_ui(&mut observer_a.peer)?;
    daemon_a.finish()?;

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
                            user_text: WORKER_INITIAL.to_owned(),
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
    let boot_a_action_matches = matched_action_count(&fixture)?;
    disconnect_ui(&mut observer_a.peer)?;
    daemon_a.finish()?;

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
