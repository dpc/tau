//! Loaded, unloaded, and ephemeral cold-resume membership acceptance.

use super::*;

/// Proves cold resume composes current durable membership and historical
/// unloaded membership while discarding all process-local ephemeral state.
#[test]
fn cold_resume_composes_loaded_unloaded_and_ephemeral_membership()
-> Result<(), Box<dyn std::error::Error>> {
    let session_id = SessionId::parse(SESSION).expect("known-safe SessionId must be valid");
    let fixture = DeterministicFixture::new_session_restore(
        "cold_resume_composes_loaded_unloaded_and_ephemeral_membership",
        &production_worker_scenario("s3-loaded-unloaded-ephemeral-membership", "s3"),
        FAKE_PROVIDER,
    )?;
    fixture.assert_session_restore_roles()?;

    let socket_a = fixture.socket_path("s3-boot-a");
    let daemon_a = spawn_daemon(&fixture, &socket_a, tau_harness::SessionLaunchStatus::New);
    let mut observer_a = SessionRestoreObserver::connect(&socket_a)?;
    observer_a.create_main("s3-main", "start the deterministic worker")?;
    observer_a.wait_for_marker("worker completion observed")?;
    observer_a.wait_for_two_idle_agents()?;
    observer_a.wait_for_session_boundary(&session_id)?;
    let identities = BootIdentities::from_events(&observer_a.events)?;
    assert_boot_a_lifecycle(&observer_a.events, &identities, &session_id)?;
    assert_provider_turn_counts(
        &observer_a.events,
        &identities,
        ProviderTurnCounts { main: 3, worker: 1 },
    )?;

    let ephemeral = observer_a.create_ephemeral_worker(&identities.main)?;
    if ephemeral == identities.main || ephemeral == identities.worker {
        return Err("ephemeral worker reused a durable identity".into());
    }
    let current_a = observer_a.roster(&session_id, SessionAgentListScope::Current)?;
    let history_a = observer_a.roster(&session_id, SessionAgentListScope::History)?;
    assert_s3_live_roster(&current_a, &identities, &ephemeral)?;
    assert_s3_live_roster(&history_a, &identities, &ephemeral)?;
    disconnect_ui(&mut observer_a.peer)?;
    daemon_a.finish()?;

    let snapshot_a = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    assert_durable_boot_a(&snapshot_a, &identities)?;
    assert_ephemeral_not_durable(fixture.harness_state_dir(), &snapshot_a, &ephemeral)?;

    let unloaded = seed_unloaded_worker(
        fixture.harness_state_dir(),
        &session_id,
        &identities,
        &ephemeral,
    )?;
    let absent = S3AbsentAgents {
        ephemeral,
        unloaded,
    };
    assert_seeded_unloaded_worker(
        fixture.harness_state_dir(),
        &session_id,
        &identities,
        &absent,
    )?;
    let snapshot_seeded = DurableSessionSnapshot::load(fixture.harness_state_dir(), &session_id)?;
    assert_durable_boot_a(&snapshot_seeded, &identities)?;
    assert_ephemeral_not_durable(
        fixture.harness_state_dir(),
        &snapshot_seeded,
        &absent.ephemeral,
    )?;

    let boot_a_action_matches = matched_action_count(&fixture)?;
    let socket_b = fixture.socket_path("s3-boot-b");
    let daemon_b = spawn_daemon(
        &fixture,
        &socket_b,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut observer_b = SessionRestoreObserver::connect(&socket_b)?;
    observer_b.wait_for_session_boundary(&session_id)?;
    assert_resume_boundaries(&observer_b.events, &identities.all(), &session_id)?;
    assert_replay_is_observational(&observer_b.events, &identities)?;
    assert_s3_absent_runtime_state(&observer_b.events, &absent)?;
    if matched_action_count(&fixture)? != boot_a_action_matches {
        return Err("S3 cold replay consumed a fake-provider lane action".into());
    }

    let current_b = observer_b.roster(&session_id, SessionAgentListScope::Current)?;
    let history_b = observer_b.roster(&session_id, SessionAgentListScope::History)?;
    assert_restored_roster(&current_b, &identities)?;
    assert_s3_history_roster(&history_b, &identities, &absent)?;

    observer_b.assert_absent_route(&absent.unloaded, "s3-unloaded-route")?;
    observer_b.assert_absent_route(&absent.ephemeral, "s3-ephemeral-route")?;
    if matched_action_count(&fixture)? != boot_a_action_matches {
        return Err("an absent S3 route activated the fake provider".into());
    }

    let fresh_start = observer_b.events.len();
    observer_b.submit(&identities.worker, "s3-fresh-worker", "fresh worker work")?;
    observer_b.wait_for_agent_marker(&identities.worker, "fresh worker complete", fresh_start)?;
    observer_b.wait_for_agent_idle_after(&identities.worker, fresh_start)?;
    observer_b.submit(&identities.main, "s3-fresh-main", "fresh main work")?;
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
    snapshot_b.require_prefix(&snapshot_seeded)?;
    if snapshot_b.session_events != snapshot_seeded.session_events {
        return Err("S3 resume or route probes appended durable membership".into());
    }
    assert_owned_suffixes(&snapshot_seeded, &snapshot_b, &identities)?;
    assert_seeded_unloaded_worker(
        fixture.harness_state_dir(),
        &session_id,
        &identities,
        &absent,
    )?;
    assert_ephemeral_not_durable(fixture.harness_state_dir(), &snapshot_b, &absent.ephemeral)?;
    fixture.assert_consumed()?;
    Ok(())
}

const S3_UNLOADED_ID: &str = "s3-seeded-unloaded-worker";
const S3_UNLOADED_NAME: &str = "seeded unloaded worker";

/// Agent identities that must have no live route after Boot B.
struct S3AbsentAgents {
    /// Process-local worker discarded with Boot A.
    ephemeral: AgentId,
    /// Durable worker whose latest membership fact is unload.
    unloaded: AgentId,
}

/// Seeds one valid unloaded durable worker only after Boot A has terminated and
/// released both stores.
///
/// The seed consists of one sequence-zero immutable creation fact followed by
/// adjacent durable session load and unload facts.
fn seed_unloaded_worker(
    state_root: &Path,
    session_id: &SessionId,
    identities: &BootIdentities,
    ephemeral: &AgentId,
) -> Result<AgentId, Box<dyn std::error::Error>> {
    let agent_id = AgentId::parse(S3_UNLOADED_ID)?;
    if [&identities.main, &identities.worker, ephemeral].contains(&&agent_id) {
        return Err("seeded unloaded worker identity collided with a live agent".into());
    }

    let mut agents = tau_core::AgentStore::open(state_root.join("agents"))?;
    if agents.agent_exists(agent_id.as_str()) {
        return Err("seeded unloaded worker already had a durable journal".into());
    }
    let outcome = agents.append_agent_event(
        agent_id.as_str(),
        None,
        Event::AgentStarted(tau_proto::AgentStarted {
            creator: Some(tau_proto::AgentCreator::default()),

            agent_id: agent_id.clone(),
            parent_agent: Some(identities.main.clone()),
            role: "deterministic-worker".to_owned(),
            display_name: Some(S3_UNLOADED_NAME.to_owned()),
            metadata: Vec::new(),
            ephemeral: false,
        }),
    )?;
    if outcome.seq.get() != 0 {
        return Err("seeded worker creation was not sequence zero".into());
    }
    drop(agents);

    let mut sessions = tau_core::SessionStore::open(state_root.join("sessions"))?;
    sessions.append_session_event(
        session_id.as_str(),
        None,
        Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: session_id.clone(),
            agent_id: agent_id.clone(),
            ephemeral: false,
        }),
    )?;
    sessions.append_session_event(
        session_id.as_str(),
        None,
        Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
            session_id: session_id.clone(),
            agent_id: agent_id.clone(),
        }),
    )?;
    Ok(agent_id)
}

fn assert_seeded_unloaded_worker(
    state_root: &Path,
    session_id: &SessionId,
    identities: &BootIdentities,
    absent: &S3AbsentAgents,
) -> Result<(), Box<dyn std::error::Error>> {
    let agent_id = &absent.unloaded;
    let agents = tau_core::AgentStore::open(state_root.join("agents"))?;
    let records = agents.agent_events(agent_id.as_str())?;
    let [record] = records.as_slice() else {
        return Err(format!("seeded worker journal has {} records", records.len()).into());
    };
    if record.seq.get() != 0
        || !matches!(
            &record.event,
            Event::AgentStarted(started)
                if &started.agent_id == agent_id
                    && started.parent_agent.as_ref() == Some(&identities.main)
                    && started.role == "deterministic-worker"
                    && started.display_name.as_deref() == Some(S3_UNLOADED_NAME)
                    && started.metadata.is_empty()
                    && !started.ephemeral
        )
    {
        return Err("seeded worker immutable creation fact changed".into());
    }

    let mut sessions = tau_core::SessionStore::open(state_root.join("sessions"))?;
    let events = sessions.session_events(session_id.as_str())?;
    let membership = events
        .iter()
        .filter(|record| match &record.event {
            Event::SessionAgentLoaded(event) => &event.agent_id == agent_id,
            Event::SessionAgentUnloaded(event) => &event.agent_id == agent_id,
            _ => false,
        })
        .collect::<Vec<_>>();
    let [loaded, unloaded] = membership.as_slice() else {
        return Err(format!("seeded worker has {} membership records", membership.len()).into());
    };
    if !matches!(
        &loaded.event,
        Event::SessionAgentLoaded(event)
            if &event.session_id == session_id
                && &event.agent_id == agent_id
                && !event.ephemeral
    ) || !matches!(
        &unloaded.event,
        Event::SessionAgentUnloaded(event)
            if &event.session_id == session_id && &event.agent_id == agent_id
    ) || unloaded.seq.get() != loaded.seq.get() + 1
    {
        return Err("seeded worker load/unload history changed".into());
    }
    if sessions
        .load_session(session_id.as_str())?
        .is_none_or(|membership| membership.contains_agent(agent_id))
    {
        return Err("seeded worker remained in composed current membership".into());
    }
    Ok(())
}

fn assert_ephemeral_not_durable(
    state_root: &Path,
    snapshot: &DurableSessionSnapshot,
    ephemeral: &AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    if snapshot.agent_events.contains_key(ephemeral)
        || snapshot
            .session_events
            .iter()
            .any(|record| match &record.event {
                Event::SessionAgentLoaded(event) => &event.agent_id == ephemeral,
                Event::SessionAgentUnloaded(event) => &event.agent_id == ephemeral,
                _ => false,
            })
    {
        return Err("ephemeral worker entered durable session membership".into());
    }
    let agents = tau_core::AgentStore::open(state_root.join("agents"))?;
    if agents.agent_exists(ephemeral.as_str())
        || state_root.join("agents").join(ephemeral.as_str()).exists()
    {
        return Err("ephemeral worker created a durable journal or directory".into());
    }
    Ok(())
}

fn assert_s3_live_roster(
    roster: &[SessionAgentListEntry],
    identities: &BootIdentities,
    ephemeral: &AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    if roster.len() != 3 {
        return Err(format!("Boot A S3 roster has {} rows", roster.len()).into());
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
            display_name: Some("deterministic worker"),
        },
    )?;
    assert_idle_live_roster_row(
        roster,
        ephemeral,
        IdleLiveRosterExpectation {
            persistence: SessionAgentPersistence::Ephemeral,
            navigation_mode: AgentNavigationMode::Active,
            role: "deterministic-worker",
            parent: Some(&identities.main),
            display_name: None,
        },
    )
}

fn assert_s3_history_roster(
    roster: &[SessionAgentListEntry],
    identities: &BootIdentities,
    absent: &S3AbsentAgents,
) -> Result<(), Box<dyn std::error::Error>> {
    if roster.len() != 3 || roster.iter().any(|row| row.agent_id == absent.ephemeral) {
        return Err(format!("Boot B S3 history membership changed: {roster:?}").into());
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
            display_name: Some("deterministic worker"),
        },
    )?;
    let row = roster
        .iter()
        .find(|row| row.agent_id == absent.unloaded)
        .ok_or("history roster omitted the seeded unloaded worker")?;
    if row.persistence != SessionAgentPersistence::Durable
        || row.lifecycle != SessionAgentLifecycle::Unloaded
    {
        return Err(format!("seeded worker history lifecycle changed: {row:?}").into());
    }
    match &row.facts {
        SessionAgentFacts::Available {
            parent_agent,
            role,
            display_name,
            ..
        } if parent_agent.as_ref() == Some(&identities.main)
            && role == "deterministic-worker"
            && display_name.as_deref() == Some(S3_UNLOADED_NAME) =>
        {
            Ok(())
        }
        facts => Err(format!("seeded worker history facts changed: {facts:?}").into()),
    }
}

fn assert_s3_absent_runtime_state(
    events: &[Observed],
    absent: &S3AbsentAgents,
) -> Result<(), Box<dyn std::error::Error>> {
    let ephemeral_membership = events.iter().filter(|observed| match &observed.event {
        Event::SessionAgentLoaded(event) => event.agent_id == absent.ephemeral,
        Event::SessionAgentUnloaded(event) => event.agent_id == absent.ephemeral,
        _ => false,
    });
    if ephemeral_membership.count() != 0 {
        return Err("ephemeral membership returned during cold replay".into());
    }
    if events.iter().any(|observed| {
        runtime_event_mentions_agent(&observed.event, &absent.ephemeral)
            || runtime_event_mentions_agent(&observed.event, &absent.unloaded)
    }) {
        return Err(
            "an ephemeral or unloaded agent regained transcript replay or a live route".into(),
        );
    }
    Ok(())
}

fn runtime_event_mentions_agent(event: &Event, agent_id: &AgentId) -> bool {
    match event {
        Event::AgentStarted(value) => &value.agent_id == agent_id,
        Event::AgentReplayComplete(value) => &value.agent_id == agent_id,
        Event::AgentStatsUpdated(value) => &value.agent_id == agent_id,
        Event::AgentPromptSubmitted(value) => &value.agent_id == agent_id,
        Event::AgentPromptCreated(value) => &value.agent_id == agent_id,
        Event::AgentInferenceDispatchStarted(value) => &value.agent_id == agent_id,
        Event::ProviderResponseFinished(value) => &value.agent_id == agent_id,
        Event::AgentWatchesUpdated(value) => {
            &value.watcher_id == agent_id
                || value
                    .watched_agent_ids
                    .iter()
                    .any(|watched| watched == agent_id)
        }
        Event::AgentMessageReceived(value) => {
            &value.sender_id == agent_id || &value.recipient_id == agent_id
        }
        _ => false,
    }
}
