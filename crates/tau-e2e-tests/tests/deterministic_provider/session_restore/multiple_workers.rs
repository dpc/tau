//! Multi-worker cold-resume ordering independence acceptance.

use super::*;

const ALPHA_ROLE: &str = "deterministic-worker-alpha";
const BETA_ROLE: &str = "deterministic-worker-beta";
const ALPHA_NAME: &str = "alpha worker";
const BETA_NAME: &str = "beta worker";
const ALPHA_INSTRUCTION: &str = "Complete alpha instruction.";
const BETA_INSTRUCTION: &str = "Complete beta instruction.";
const ALPHA_INITIAL: &str = concat!(
    "[tau-internal]: You were started by an agent `main`. Your responses will be delivered to it. ",
    "You can use the `message` tool to communicate with agents.\n\n",
    "Complete alpha instruction."
);
const BETA_INITIAL: &str = concat!(
    "[tau-internal]: You were started by an agent `main`. Your responses will be delivered to it. ",
    "You can use the `message` tool to communicate with agents.\n\n",
    "Complete beta instruction."
);

/// Proves a three-member session restores by identity rather than worker
/// completion, roster-row, or resumed-activation order.
#[test]
fn cold_resume_multiple_workers_is_order_independent() -> Result<(), Box<dyn std::error::Error>> {
    let session_id = SessionId::parse(SESSION).expect("known-safe SessionId must be valid");
    let scenario = s4_scenario();
    let action_counts = scenario
        .lanes
        .iter()
        .map(|lane| lane.actions.len())
        .collect::<Vec<_>>();
    if action_counts != [6, 2, 2] || serde_json::to_vec(&scenario)?.len() != 1_911 {
        return Err(format!(
            "S4 scenario no longer matches its three-lane [6, 2, 2]-action, \
             1,911-byte budget: {action_counts:?}"
        )
        .into());
    }
    let fixture = DeterministicFixture::new_session_restore_multiple_workers(
        "cold_resume_multiple_workers_is_order_independent",
        &scenario,
        FAKE_PROVIDER,
    )?;
    fixture.assert_session_restore_multiple_worker_roles()?;

    let socket_a = fixture.socket_path("s4-boot-a");
    let daemon_a = spawn_daemon(&fixture, &socket_a, tau_harness::SessionLaunchStatus::New);
    let mut observer_a = SessionRestoreObserver::connect(&socket_a)?;
    observer_a.create_main("s4-main", "start worker alpha")?;
    observer_a.wait_for_marker("alpha completion observed")?;
    observer_a.wait_for_idle_agent_count(2)?;
    let main = sole_created_agent_for_role(&observer_a.events, "deterministic-main")?;
    observer_a.submit(&main, "s4-start-beta-ui", "start worker beta")?;
    observer_a.wait_for_marker("beta completion observed")?;
    observer_a.wait_for_idle_agent_count(3)?;
    observer_a.wait_for_session_boundary(&session_id)?;

    let identities = S4Identities::from_events(&observer_a.events)?;
    let creation_order = observed_worker_creation_order(&observer_a.events, &identities)?;
    assert_observed_worker_completions(&observer_a.events, &identities)?;
    assert_s4_boot_a_lifecycle(&observer_a.events, &identities, &session_id)?;
    assert_provider_turn_counts_by_agent(
        &observer_a.events,
        &BTreeMap::from([
            (identities.main.clone(), 8),
            (identities.alpha.clone(), 1),
            (identities.beta.clone(), 1),
        ]),
    )?;
    disconnect_ui(&mut observer_a.peer)?;
    daemon_a.finish()?;

    let snapshot_a = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    assert_s4_durable_membership(&snapshot_a, &identities)?;
    let boot_a_action_matches = matched_action_count(&fixture)?;

    let socket_b = fixture.socket_path("s4-boot-b");
    let daemon_b = spawn_daemon(
        &fixture,
        &socket_b,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut observer_b = SessionRestoreObserver::connect(&socket_b)?;
    observer_b.wait_for_session_boundary(&session_id)?;
    assert_resume_boundaries(&observer_b.events, &identities.all(), &session_id)?;
    assert_s4_replay_is_observational(&observer_b.events, &identities)?;
    if matched_action_count(&fixture)? != boot_a_action_matches {
        return Err("S4 replay consumed a fake-provider action".into());
    }

    let current = roster_by_id(observer_b.roster(&session_id, SessionAgentListScope::Current)?)?;
    let history = roster_by_id(observer_b.roster(&session_id, SessionAgentListScope::History)?)?;
    assert_s4_roster(&current, &identities)?;
    assert_s4_roster(&history, &identities)?;
    if current != history {
        return Err("S4 current/history roster sets differ".into());
    }

    let activation_order = creation_order.iter().rev().cloned().collect::<Vec<_>>();
    if activation_order == creation_order {
        return Err("S4 resumed activation order did not differ from creation".into());
    }
    let fresh_start = observer_b.events.len();
    for agent_id in &activation_order {
        let fresh = identities.fresh_case(agent_id)?;
        let turn_start = observer_b.events.len();
        observer_b.submit(agent_id, fresh.ctx_id, fresh.prompt)?;
        observer_b.wait_for_agent_marker(agent_id, fresh.response, turn_start)?;
        observer_b.wait_for_agent_idle_after(agent_id, turn_start)?;
        if !observer_b.events[turn_start..].iter().any(|observed| {
            !observed.replay
                && matches!(
                    &observed.event,
                    Event::AgentStatsUpdated(stats)
                        if &stats.agent_id == agent_id
                            && stats.navigation_mode == AgentNavigationMode::Active
                )
        }) {
            return Err(format!(
                "S4 accepted worker prompt did not publish live Active stats for {agent_id}"
            )
            .into());
        }
    }
    let active_roster =
        roster_by_id(observer_b.roster(&session_id, SessionAgentListScope::Current)?)?;
    for worker in identities.workers() {
        if !matches!(
            active_roster.get(worker).map(|row| row.lifecycle),
            Some(SessionAgentLifecycle::Live {
                navigation_mode: AgentNavigationMode::Active,
                ..
            })
        ) {
            return Err(format!("S4 worker {worker} did not remain Active after its turn").into());
        }
    }
    assert_no_s4_watch_refanout(&observer_b.events[fresh_start..], &identities)?;
    assert_provider_turn_counts_by_agent(
        &observer_b.events,
        &BTreeMap::from([
            (identities.main.clone(), 0),
            (identities.alpha.clone(), 1),
            (identities.beta.clone(), 1),
        ]),
    )?;
    disconnect_ui(&mut observer_b.peer)?;
    daemon_b.finish()?;

    let snapshot_b = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    snapshot_b.require_prefix(&snapshot_a)?;
    assert_s4_durable_membership(&snapshot_b, &identities)?;
    if snapshot_b.session_events != snapshot_a.session_events {
        return Err("S4 resume appended a durable membership fact".into());
    }
    assert_s4_owned_suffixes(&snapshot_a, &snapshot_b, &identities)?;
    assert_s4_lane_continuations(&fixture.trace()?)?;

    let socket_c = fixture.socket_path("s4-boot-c");
    let daemon_c = spawn_daemon(
        &fixture,
        &socket_c,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut observer_c = SessionRestoreObserver::connect(&socket_c)?;
    observer_c.wait_for_session_boundary(&session_id)?;
    let cold_roster =
        roster_by_id(observer_c.roster(&session_id, SessionAgentListScope::Current)?)?;
    assert_s4_roster(&cold_roster, &identities)?;
    if matched_action_count(&fixture)? != boot_a_action_matches + 2 {
        return Err("S4 second cold replay consumed a fake-provider action".into());
    }
    disconnect_ui(&mut observer_c.peer)?;
    daemon_c.finish()?;

    let snapshot_c = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    super::assert_initialization_only_refresh(&snapshot_b, &snapshot_c)?;
    fixture.assert_consumed()?;
    Ok(())
}

fn s4_scenario() -> ScenarioV2 {
    let watch = |content: &str, response: &str| ScenarioActionV2::WatchNotifications {
        notifications: vec![
            WatchNotificationV2::TurnState {
                state: AgentRuntimeState::Running,
            },
            WatchNotificationV2::Response {
                content: content.to_owned(),
            },
            WatchNotificationV2::TurnState {
                state: AgentRuntimeState::Idle,
            },
        ],
        response: response.to_owned(),
    };
    ScenarioV2::new(
        "s4-multiple-workers-ordering-independent",
        vec![
            ScenarioLaneV2 {
                ctx_id: "s4-main".to_owned(),
                actions: vec![
                    ScenarioActionV2::AgentStartCall {
                        user_text: "start worker alpha".to_owned(),
                        call_id: "s4-start-alpha".into(),
                        prompt: ALPHA_INSTRUCTION.to_owned(),
                        role: Some(ALPHA_ROLE.to_owned()),
                        task_name: ALPHA_NAME.to_owned(),
                    },
                    ScenarioActionV2::AgentStartResult {
                        user_text: "start worker alpha".to_owned(),
                        call_id: "s4-start-alpha".into(),
                        response: "alpha worker start accepted".to_owned(),
                    },
                    watch("alpha boot-a complete", "alpha completion observed"),
                    ScenarioActionV2::AgentStartCall {
                        user_text: "start worker beta".to_owned(),
                        call_id: "s4-start-beta".into(),
                        prompt: BETA_INSTRUCTION.to_owned(),
                        role: Some(BETA_ROLE.to_owned()),
                        task_name: BETA_NAME.to_owned(),
                    },
                    ScenarioActionV2::AgentStartResult {
                        user_text: "start worker beta".to_owned(),
                        call_id: "s4-start-beta".into(),
                        response: "beta worker start accepted".to_owned(),
                    },
                    watch("beta boot-a complete", "beta completion observed"),
                ],
            },
            ScenarioLaneV2 {
                ctx_id: "s4-worker-alpha".to_owned(),
                actions: vec![
                    ScenarioActionV2::Text {
                        user_text: ALPHA_INITIAL.to_owned(),
                        response: "alpha boot-a complete".to_owned(),
                    },
                    ScenarioActionV2::Text {
                        user_text: "fresh alpha work".to_owned(),
                        response: "fresh alpha complete".to_owned(),
                    },
                ],
            },
            ScenarioLaneV2 {
                ctx_id: "s4-worker-beta".to_owned(),
                actions: vec![
                    ScenarioActionV2::Text {
                        user_text: BETA_INITIAL.to_owned(),
                        response: "beta boot-a complete".to_owned(),
                    },
                    ScenarioActionV2::Text {
                        user_text: "fresh beta work".to_owned(),
                        response: "fresh beta complete".to_owned(),
                    },
                ],
            },
        ],
    )
}

/// Stable S4 identities discovered from immutable creation facts.
struct S4Identities {
    /// Durable parent agent that production-starts both workers.
    main: AgentId,
    /// Durable worker using the alpha role, instruction, and fake lane.
    alpha: AgentId,
    /// Durable worker using the beta role, instruction, and fake lane.
    beta: AgentId,
}

/// Worker-specific resumed turn used to prove retained lane ownership.
struct FreshCase {
    /// Fresh UI correlation ID, deliberately distinct from the lane's initial
    /// ID.
    ctx_id: &'static str,
    /// Exact direct prompt accepted by the restored worker.
    prompt: &'static str,
    /// Exact fake-provider response owned by that worker.
    response: &'static str,
}

impl S4Identities {
    fn from_events(events: &[Observed]) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            main: sole_created_agent_for_role(events, "deterministic-main")?,
            alpha: sole_created_agent_for_role(events, ALPHA_ROLE)?,
            beta: sole_created_agent_for_role(events, BETA_ROLE)?,
        })
    }

    fn all(&self) -> [&AgentId; 3] {
        [&self.main, &self.alpha, &self.beta]
    }

    fn workers(&self) -> [&AgentId; 2] {
        [&self.alpha, &self.beta]
    }

    fn fresh_case(&self, agent_id: &AgentId) -> Result<FreshCase, Box<dyn std::error::Error>> {
        if agent_id == &self.alpha {
            Ok(FreshCase {
                ctx_id: "s4-fresh-alpha",
                prompt: "fresh alpha work",
                response: "fresh alpha complete",
            })
        } else if agent_id == &self.beta {
            Ok(FreshCase {
                ctx_id: "s4-fresh-beta",
                prompt: "fresh beta work",
                response: "fresh beta complete",
            })
        } else {
            Err(format!("S4 activation selected non-worker {agent_id}").into())
        }
    }
}

fn sole_created_agent_for_role(
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
        return Err(format!("expected one `{role}` creation, got {ids:?}").into());
    };
    Ok(agent_id.clone())
}

fn observed_worker_creation_order(
    events: &[Observed],
    identities: &S4Identities,
) -> Result<Vec<AgentId>, Box<dyn std::error::Error>> {
    let order = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::AgentStarted(started)
                if !observed.replay && identities.workers().contains(&&started.agent_id) =>
            {
                Some(started.agent_id.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if order.len() != 2
        || order.iter().cloned().collect::<BTreeSet<_>>()
            != identities.workers().into_iter().cloned().collect()
    {
        return Err(format!("S4 worker creation order is incomplete: {order:?}").into());
    }
    Ok(order)
}

fn assert_observed_worker_completions(
    events: &[Observed],
    identities: &S4Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    let order = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::ProviderResponseFinished(response)
                if !observed.replay
                    && response.agent_id == identities.alpha
                    && provider_response_contains(response, "alpha boot-a complete") =>
            {
                Some(identities.alpha.clone())
            }
            Event::ProviderResponseFinished(response)
                if !observed.replay
                    && response.agent_id == identities.beta
                    && provider_response_contains(response, "beta boot-a complete") =>
            {
                Some(identities.beta.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if order.len() != 2
        || order.iter().cloned().collect::<BTreeSet<_>>()
            != identities.workers().into_iter().cloned().collect()
    {
        return Err(format!("S4 observed completion set is incomplete: {order:?}").into());
    }
    Ok(())
}

fn assert_s4_boot_a_lifecycle(
    events: &[Observed],
    identities: &S4Identities,
    session_id: &SessionId,
) -> Result<(), Box<dyn std::error::Error>> {
    let starts = events
        .iter()
        .filter_map(|observed| match &observed.event {
            Event::AgentStarted(started) if !observed.replay => Some(started),
            _ => None,
        })
        .collect::<Vec<_>>();
    if starts.len() != 3 {
        return Err(format!("S4 observed {} creation facts", starts.len()).into());
    }
    for (agent_id, parent, role, name) in [
        (&identities.main, None, "deterministic-main", None),
        (
            &identities.alpha,
            Some(&identities.main),
            ALPHA_ROLE,
            Some(ALPHA_NAME),
        ),
        (
            &identities.beta,
            Some(&identities.main),
            BETA_ROLE,
            Some(BETA_NAME),
        ),
    ] {
        let started = starts
            .iter()
            .find(|started| &started.agent_id == agent_id)
            .ok_or_else(|| format!("S4 creation missing for {agent_id}"))?;
        if started.parent_agent.as_ref() != parent
            || started.role != role
            || started.display_name.as_deref() != name
            || started.ephemeral
        {
            return Err(
                format!("S4 immutable creation changed for {agent_id}: {started:?}").into(),
            );
        }
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
        let unloads = events
            .iter()
            .filter(|observed| {
                matches!(
                    &observed.event,
                    Event::SessionAgentUnloaded(unloaded)
                        if &unloaded.session_id == session_id && &unloaded.agent_id == agent_id
                )
            })
            .count();
        if loads != 1 || unloads != 0 {
            return Err(format!(
                "S4 live membership changed for {agent_id}: loads={loads}, unloads={unloads}"
            )
            .into());
        }
    }
    Ok(())
}

fn assert_s4_durable_membership(
    snapshot: &DurableSessionSnapshot,
    identities: &S4Identities,
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
        return Err("S4 composed durable membership changed".into());
    }
    for agent_id in identities.all() {
        let records = &snapshot.agent_events[agent_id];
        let starts = records
            .iter()
            .filter(|record| {
                matches!(&record.event, Event::AgentStarted(started) if &started.agent_id == agent_id)
            })
            .collect::<Vec<_>>();
        if starts.len() != 1 || starts[0].seq.get() != 0 {
            return Err(format!("{agent_id} lacks one sequence-zero creation").into());
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
                matches!(
                    &record.event,
                    Event::SessionAgentUnloaded(unloaded) if &unloaded.agent_id == agent_id
                )
            })
            .count();
        if loads != 1 || unloads != 0 {
            return Err(format!(
                "S4 durable membership changed for {agent_id}: loads={loads}, unloads={unloads}"
            )
            .into());
        }
    }
    Ok(())
}

fn assert_s4_replay_is_observational(
    events: &[Observed],
    identities: &S4Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    for (agent_id, marker) in [
        (&identities.alpha, "alpha boot-a complete"),
        (&identities.beta, "beta boot-a complete"),
        (&identities.main, "alpha completion observed"),
        (&identities.main, "beta completion observed"),
    ] {
        let terminals = events
            .iter()
            .filter(|observed| {
                observed.replay
                    && observed.recorded_at.is_some()
                    && matches!(
                        &observed.event,
                        Event::ProviderResponseFinished(response)
                            if &response.agent_id == agent_id
                                && provider_response_contains(response, marker)
                    )
            })
            .count();
        if terminals != 1 {
            return Err(format!("S4 replayed `{marker}` for {agent_id} {terminals} times").into());
        }
    }
    Ok(())
}

fn roster_by_id(
    rows: Vec<SessionAgentListEntry>,
) -> Result<BTreeMap<AgentId, SessionAgentListEntry>, Box<dyn std::error::Error>> {
    let mut by_id = BTreeMap::new();
    for row in rows {
        if by_id.insert(row.agent_id.clone(), row).is_some() {
            return Err("S4 roster returned a duplicate agent row".into());
        }
    }
    Ok(by_id)
}

fn assert_s4_roster(
    roster: &BTreeMap<AgentId, SessionAgentListEntry>,
    identities: &S4Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    let expected_ids = identities
        .all()
        .into_iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if roster.keys().cloned().collect::<BTreeSet<_>>() != expected_ids {
        return Err(format!("S4 ID-keyed roster changed: {roster:?}").into());
    }
    for (agent_id, navigation_mode, role, parent, name) in [
        (
            &identities.main,
            AgentNavigationMode::Active,
            "deterministic-main",
            None,
            None,
        ),
        (
            &identities.alpha,
            AgentNavigationMode::ActiveAuto,
            ALPHA_ROLE,
            Some(&identities.main),
            Some(ALPHA_NAME),
        ),
        (
            &identities.beta,
            AgentNavigationMode::ActiveAuto,
            BETA_ROLE,
            Some(&identities.main),
            Some(BETA_NAME),
        ),
    ] {
        let row = roster
            .get(agent_id)
            .ok_or_else(|| format!("S4 roster omitted {agent_id}"))?;
        assert_idle_live_roster_row(
            std::slice::from_ref(row),
            agent_id,
            IdleLiveRosterExpectation {
                persistence: SessionAgentPersistence::Durable,
                navigation_mode,
                role,
                parent,
                display_name: name,
            },
        )?;
    }
    Ok(())
}

fn assert_no_s4_watch_refanout(
    events: &[Observed],
    identities: &S4Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    if events.iter().any(|observed| {
        matches!(
            &observed.event,
            Event::AgentMessageReceived(message)
                if message.recipient_id == identities.main
                    && identities.workers().contains(&&message.sender_id)
                    && matches!(
                        message.kind,
                        AgentMessageKind::WatchPrompt
                            | AgentMessageKind::WatchResponse
                            | AgentMessageKind::WatchTurnState
                    )
        )
    }) {
        return Err("S4 restored an automatic worker watch".into());
    }
    Ok(())
}

fn assert_s4_owned_suffixes(
    before: &DurableSessionSnapshot,
    after: &DurableSessionSnapshot,
    identities: &S4Identities,
) -> Result<(), Box<dyn std::error::Error>> {
    if !super::suffix_after_initialization(before, after, &identities.main)?.is_empty() {
        return Err("S4 worker activation appended beyond the main initialization".into());
    }
    for owner in identities.workers() {
        let fresh = identities.fresh_case(owner)?;
        let suffix = super::suffix_after_initialization(before, after, owner)?;
        if count_prompt(suffix, owner, fresh.prompt) != 1
            || count_response(suffix, owner, fresh.response) != 1
            || suffix
                .iter()
                .any(|record| matches!(record.event, Event::AgentStarted(_)))
        {
            return Err(format!("S4 fresh suffix for {owner} changed").into());
        }
        for other in identities.all() {
            if other == owner {
                continue;
            }
            let other_suffix = &after.agent_events[other][before.agent_events[other].len()..];
            if count_prompt(other_suffix, owner, fresh.prompt) != 0
                || count_response(other_suffix, owner, fresh.response) != 0
            {
                return Err(format!("S4 fresh work for {owner} leaked into {other}").into());
            }
        }
    }
    Ok(())
}

fn assert_s4_lane_continuations(trace: &str) -> Result<(), Box<dyn std::error::Error>> {
    for lane in ["s4-worker-alpha", "s4-worker-beta"] {
        for action in [0, 1] {
            let marker = format!("lane={lane} action={action} ");
            if trace
                .lines()
                .filter(|line| line.contains(&marker) && line.contains(" matched "))
                .count()
                != 1
            {
                return Err(format!("S4 lane binding changed for {lane} action {action}").into());
            }
        }
    }
    Ok(())
}
