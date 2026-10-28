//! Production message-release tool, including closed schema and main authority.

use std::collections::HashSet;

use super::*;

#[test]
fn deterministic_peer_review_release_enforces_schema_authority_and_receipt()
-> Result<(), Box<dyn std::error::Error>> {
    let main: AgentId = "review-main".parse()?;
    let worker: AgentId = "review-worker".parse()?;
    let source = tau_proto::PeerMessageReviewReference {
        main_agent_id: main.clone(),
        durable_event_seq: 1,
    };
    let actions = |name: &str, override_body, rejected| {
        vec![
            ScenarioActionV2::PeerReviewRelease {
                user_text: name.to_owned(),
                call_id: name.into(),
                reference: source.clone(),
                override_body,
            },
            ScenarioActionV2::PeerReviewReleaseResult {
                call_id: name.into(),
                rejected,
            },
        ]
    };
    let mut main_actions = actions("schema-rejection", true, true);
    main_actions.extend(actions("approve-original", false, false));
    main_actions.extend(actions("retry-original", false, false));
    let mut worker_actions = actions("worker-forgery", false, true);
    worker_actions.push(ScenarioActionV2::PeerReviewDelivered);
    let scenario = ScenarioV2::new(
        "peer-review-release",
        vec![
            ScenarioLaneV2 {
                ctx_id: "main".to_owned(),
                actions: main_actions,
            },
            ScenarioLaneV2 {
                ctx_id: "worker".to_owned(),
                actions: worker_actions,
            },
        ],
    );
    let fixture = DeterministicFixture::new_session_message(
        "deterministic_peer_review_release_enforces_schema_authority_and_receipt",
        &scenario,
        FAKE_PROVIDER,
    )?;
    let config_path = fixture.config_dir().join("harness.yaml");
    let mut config: serde_json::Value = serde_json::from_slice(&std::fs::read(&config_path)?)?;
    for role in ["deterministic-main", "deterministic-worker"] {
        config["agents"]["role_groups"]["e2e"]["roles"][role]["tools"] =
            serde_json::json!(["message_release"]);
    }
    config["agents"]["role_groups"]["e2e"]["roles"]["deterministic-main"]["main_priority"] =
        serde_json::json!(10);
    std::fs::write(config_path, serde_json::to_vec_pretty(&config)?)?;
    seed_review(&fixture, &main, &worker)?;
    let socket = fixture.socket_path("peer-review");
    let daemon = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::Resumed);
    let mut ui = connect_ui(&socket)?;
    submit_prompt(&mut ui, &worker, "worker", "worker-forgery")?;
    wait_for_idle(&mut ui, &[&worker])?;
    submit_prompt(&mut ui, &main, "main", "schema-rejection")?;
    wait_for_idle(&mut ui, &[&main])?;
    submit_prompt(&mut ui, &main, "main", "approve-original")?;
    wait_for_idle(&mut ui, &[&main, &worker])?;
    submit_prompt(&mut ui, &main, "main", "retry-original")?;
    wait_for_idle(&mut ui, &[&main])?;
    disconnect_ui(&mut ui)?;
    drop(ui);
    daemon.finish()?;
    fixture.assert_consumed()?;
    let snapshot = tau_e2e_tests::DurableSessionSnapshot::load(
        fixture.harness_state_dir(),
        &"deterministic-e2e-session".parse()?,
    )?;
    let receipts = snapshot.agent_events[&worker]
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentMessageReceived(received) => Some(received),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].message, "immutable original peer body");
    assert_eq!(receipts[0].sender_id.as_str(), "external-sender");
    assert_eq!(
        receipts[0].sender_session_id.as_ref().map(|id| id.as_str()),
        Some("external-session")
    );
    assert_eq!(
        receipts[0].peer_review,
        Some(tau_proto::PeerMessageReview::Released { source })
    );
    Ok(())
}

/// Seed only committed durable facts while no daemon owns either store. The
/// production provider/tool path performs every release and authority decision.
fn seed_review(
    fixture: &DeterministicFixture,
    main: &AgentId,
    worker: &AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    let session: tau_proto::SessionId = "deterministic-e2e-session".parse()?;
    let mut sessions =
        tau_core::SessionStore::open_fixture(fixture.harness_state_dir().join("sessions"))?;
    sessions.record_session_meta(session.as_str())?;
    let mut agents =
        tau_core::AgentStore::open_fixture(fixture.harness_state_dir().join("agents"))?;
    for (agent, role) in [
        (main, "deterministic-main"),
        (worker, "deterministic-worker"),
    ] {
        agents.append_agent_event(
            agent.as_str(),
            None,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),
                agent_id: agent.clone(),
                parent_agent: None,
                role: role.to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )?;
        sessions.append_session_event(
            session.as_str(),
            None,
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                main: agent == main,
                session_id: session.clone(),
                agent_id: agent.clone(),
                agent_initialization_id: format!("init-{agent}").parse()?,
                ephemeral: false,
            }),
        )?;
    }
    let outcome = agents.append_agent_event(
        main.as_str(),
        None,
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: "original-peer-message".parse()?,
            sender_id: "external-sender".parse()?,
            sender_session_id: Some("external-session".parse()?),
            cli_origin: None,
            recipient_id: main.clone(),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            sender_notice: None,
            recipient_notice: None,
            peer_review: Some(tau_proto::PeerMessageReview::Pending {
                intended_recipient: worker.clone(),
            }),
            message: "immutable original peer body".to_owned(),
        }),
    )?;
    assert_eq!(outcome.seq.get(), 1);
    Ok(())
}

fn wait_for_idle(
    peer: &mut SocketPeer,
    agents: &[&AgentId],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut finished = HashSet::new();
    let mut idle = HashSet::new();
    while idle.len() < agents.len() {
        match recv_event(peer)? {
            Event::ProviderResponseFinished(event)
                if agents.contains(&&event.agent_id)
                    && event.stop_reason == tau_proto::ProviderStopReason::EndTurn =>
            {
                finished.insert(event.agent_id);
            }
            Event::AgentStatsUpdated(stats)
                if finished.contains(&stats.agent_id)
                    && stats.runtime_state == AgentRuntimeState::Idle
                    && stats.tools.in_flight == 0 =>
            {
                idle.insert(stats.agent_id);
            }
            _ => {}
        }
    }
    Ok(())
}
