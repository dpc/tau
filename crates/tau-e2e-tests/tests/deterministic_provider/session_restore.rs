//! Cold-resume acceptance for a production-started durable worker.

use std::collections::{BTreeMap, BTreeSet};

use tau_e2e_tests::{
    DeterministicFixture, DurableSessionSnapshot, ScenarioActionV2, ScenarioLaneV2, ScenarioV2,
    WatchNotificationV2,
};
use tau_proto::{
    AgentId, AgentMessageKind, AgentNavigationMode, AgentRuntimeState, Event, SessionAgentFacts,
    SessionAgentLifecycle, SessionAgentListEntry, SessionAgentListScope, SessionAgentPersistence,
    SessionId,
};

use super::FAKE_PROVIDER;
use super::daemon_support::{disconnect_ui, spawn_daemon};

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

/// Proves cold resume restores a production-started completed worker as a
/// durable, independently addressable conversation without restoring its watch.
#[test]
fn cold_resume_restores_completed_production_worker() -> Result<(), Box<dyn std::error::Error>> {
    let session_id = SessionId::from(SESSION);
    let fixture = DeterministicFixture::new_session_restore(
        "cold_resume_restores_completed_production_worker",
        &ScenarioV2::new(
            "s1-quiescent-main-completed-worker",
            vec![
                ScenarioLaneV2 {
                    ctx_id: "s1-main".to_owned(),
                    actions: vec![
                        ScenarioActionV2::AgentStartCall {
                            user_text: "start the deterministic worker".to_owned(),
                            call_id: "s1-agent-start".into(),
                            prompt: WORKER_PROMPT.to_owned(),
                            role: Some("deterministic-worker".to_owned()),
                            task_name: "deterministic worker".to_owned(),
                        },
                        ScenarioActionV2::AgentStartResult {
                            user_text: "start the deterministic worker".to_owned(),
                            call_id: "s1-agent-start".into(),
                            response: "worker start accepted".to_owned(),
                        },
                        ScenarioActionV2::WatchNotifications {
                            notifications: vec![
                                WatchNotificationV2::TurnState {
                                    state: AgentRuntimeState::Running,
                                },
                                WatchNotificationV2::Response {
                                    content: "worker boot-a complete".to_owned(),
                                },
                                WatchNotificationV2::TurnState {
                                    state: AgentRuntimeState::Idle,
                                },
                            ],
                            response: "worker completion observed".to_owned(),
                        },
                        ScenarioActionV2::Text {
                            user_text: "fresh main work".to_owned(),
                            response: "fresh main complete".to_owned(),
                        },
                    ],
                },
                ScenarioLaneV2 {
                    ctx_id: "s1-worker".to_owned(),
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
        ),
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
        ProviderTurnCounts { main: 5, worker: 1 },
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
    assert_resume_boundaries(&observer_b.events, &identities, &session_id)?;
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

/// Exact main and worker identities discovered from immutable creation facts.
struct BootIdentities {
    /// Stable main agent id.
    main: AgentId,
    /// Stable production-started worker id.
    worker: AgentId,
}

/// Exact accepted provider prompts owned by each S1 role in one boot.
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
        || worker.display_name.as_deref() != Some("deterministic worker")
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
    identities: &BootIdentities,
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
    for agent_id in [&identities.main, &identities.worker] {
        let boundaries = events
            .iter()
            .enumerate()
            .filter(|(_, observed)| {
                !observed.replay
                    && observed.recorded_at.is_none()
                    && matches!(
                        &observed.event,
                        Event::AgentReplayComplete(done)
                            if &done.agent_id == agent_id
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
        if boundary >= session_boundary {
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
            Some("deterministic worker"),
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
                            AgentMessageKind::WatchPrompt
                                | AgentMessageKind::WatchResponse
                                | AgentMessageKind::WatchTurnState
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
    let main = counts.get(&identities.main).copied().unwrap_or_default();
    let worker = counts.get(&identities.worker).copied().unwrap_or_default();
    if main != expected.main || worker != expected.worker || counts.len() != 2 {
        return Err(format!(
            "provider-turn budget changed: main={main}/{}, worker={}/{}, all={counts:?}",
            expected.main, worker, expected.worker
        )
        .into());
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
    for (agent_id, expected_mode, role, parent, name) in [
        (
            &identities.main,
            AgentNavigationMode::Active,
            "deterministic-main",
            None,
            None,
        ),
        (
            &identities.worker,
            AgentNavigationMode::ActiveAuto,
            "deterministic-worker",
            Some(&identities.main),
            Some("deterministic worker"),
        ),
    ] {
        let row = roster
            .iter()
            .find(|row| &row.agent_id == agent_id)
            .ok_or_else(|| format!("roster omitted {agent_id}"))?;
        if row.persistence != SessionAgentPersistence::Durable
            || row.lifecycle
                != (SessionAgentLifecycle::Live {
                    runtime_state: AgentRuntimeState::Idle,
                    navigation_mode: expected_mode,
                })
        {
            return Err(format!("restored lifecycle changed for {agent_id}: {row:?}").into());
        }
        match &row.facts {
            SessionAgentFacts::Available {
                parent_agent,
                role: actual_role,
                display_name,
                ..
            } if parent_agent.as_ref() == parent
                && actual_role == role
                && display_name.as_deref() == name => {}
            facts => {
                return Err(
                    format!("restored creation facts changed for {agent_id}: {facts:?}").into(),
                );
            }
        }
    }
    Ok(())
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
