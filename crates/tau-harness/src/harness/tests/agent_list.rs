use super::*;

/// Current and historical roster scopes preserve harness lifecycle authority.
#[test]
fn roster_scope_distinguishes_live_and_unloaded_agents() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    harness.publish_event(
        None,
        Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            session_id: "s1".into(),
            agent_id: agent_id.clone(),
            ephemeral: false,
        }),
    );

    let current = harness
        .build_session_agent_list(&"s1".into(), tau_proto::SessionAgentListScope::Current)
        .expect("current roster");
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].agent_id, agent_id);
    assert!(matches!(
        current[0].lifecycle,
        tau_proto::SessionAgentLifecycle::Live {
            navigation_mode: tau_proto::AgentNavigationMode::Active,
            ..
        }
    ));
    assert!(matches!(
        current[0].facts,
        tau_proto::SessionAgentFacts::Available { .. }
    ));

    harness
        .agents
        .get_mut(&cid)
        .expect("live agent runtime")
        .terminating = true;
    let stopping = harness
        .build_session_agent_list(&"s1".into(), tau_proto::SessionAgentListScope::Current)
        .expect("stopping roster");
    assert_eq!(
        stopping[0].lifecycle,
        tau_proto::SessionAgentLifecycle::Unavailable
    );

    harness.agents.remove(&cid);
    harness.publish_event(
        None,
        Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
            session_id: "s1".into(),
            agent_id: agent_id.clone(),
        }),
    );

    assert!(
        harness
            .build_session_agent_list(&"s1".into(), tau_proto::SessionAgentListScope::Current)
            .expect("current roster")
            .is_empty()
    );
    let history = harness
        .build_session_agent_list(&"s1".into(), tau_proto::SessionAgentListScope::History)
        .expect("history roster");
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].lifecycle,
        tau_proto::SessionAgentLifecycle::Unloaded
    );
}

/// Committed membership remains visible as unavailable even when no runtime or
/// agent journal can be restored.
#[test]
fn roster_keeps_committed_member_without_runtime_or_facts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let agent_id = tau_proto::AgentId::parse("missing-agent").expect("agent id");
    harness
        .session_roster_loaded_agents
        .insert(agent_id.clone());
    harness
        .session_roster_ever_loaded_agents
        .insert(agent_id.clone());

    let rows = harness
        .build_session_agent_list(&"s1".into(), tau_proto::SessionAgentListScope::Current)
        .expect("roster");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].agent_id, agent_id);
    assert_eq!(
        rows[0].lifecycle,
        tau_proto::SessionAgentLifecycle::Unavailable
    );
    assert_eq!(rows[0].facts, tau_proto::SessionAgentFacts::Missing);
}

/// Roster requests fail closed when the caller names a stale session.
#[test]
fn roster_rejects_stale_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let harness = quiet_provider_harness(temp.path()).expect("harness");

    let error = harness
        .build_session_agent_list(&"other".into(), tau_proto::SessionAgentListScope::Current)
        .expect_err("stale session must fail");

    assert_eq!(
        error.kind,
        tau_proto::SessionAgentListErrorKind::StaleSession
    );
}

/// Roster results are directed to the requesting UI and rejected for
/// non-UI-classified client connections.
#[test]
fn roster_result_is_ui_only_and_requester_directed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let _cid = ensure_test_user_agent(&mut harness);
    let requester = connect_test_client(&mut harness, "requester", tau_proto::ClientKind::Ui);
    let other_ui = connect_test_client(&mut harness, "other-ui", tau_proto::ClientKind::Ui);
    let tool = connect_test_client(&mut harness, "tool", tau_proto::ClientKind::Tool);
    let request = tau_proto::GetSessionAgentList {
        request_id: "request-1".to_owned(),
        session_id: "s1".into(),
        scope: tau_proto::SessionAgentListScope::Current,
    };

    harness
        .handle_client_message(
            "requester",
            HarnessInputMessage::GetSessionAgentList(request.clone()),
        )
        .expect("UI request");
    harness
        .handle_client_message("tool", HarnessInputMessage::GetSessionAgentList(request))
        .expect("non-UI request is ignored");

    assert!(
        requester
            .lock()
            .expect("requester frames")
            .iter()
            .any(|routed| matches!(
                &routed.frame,
                HarnessOutputMessage::SessionAgentListResult(result)
                    if result.request_id == "request-1"
            ))
    );
    assert!(other_ui.lock().expect("other UI frames").is_empty());
    assert!(tool.lock().expect("tool frames").is_empty());
}

/// Current-session probes use harness-assigned connection metadata rather than
/// the caller's Hello claim and return the in-memory id only to the requester.
#[test]
fn current_session_result_is_authoritative_ui_only_and_directed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let requester = connect_test_client(&mut harness, "requester", tau_proto::ClientKind::Ui);
    let other_ui = connect_test_client(&mut harness, "other-ui", tau_proto::ClientKind::Ui);
    let tool = connect_test_client(&mut harness, "tool", tau_proto::ClientKind::Tool);
    let request = tau_proto::GetCurrentSession {
        request_id: "current-1".to_owned(),
    };

    harness
        .handle_client_message(
            "requester",
            HarnessInputMessage::Hello(tau_proto::Hello {
                protocol_version: tau_proto::PROTOCOL_VERSION,
                client_name: "claim-external".into(),
                client_kind: tau_proto::ClientKind::External,
                capabilities: Default::default(),
            }),
        )
        .expect("Hello claim");
    harness
        .handle_client_message(
            "requester",
            HarnessInputMessage::GetCurrentSession(request.clone()),
        )
        .expect("UI request");
    harness
        .handle_client_message("tool", HarnessInputMessage::GetCurrentSession(request))
        .expect("non-UI request is ignored");

    assert!(
        requester
            .lock()
            .expect("requester frames")
            .iter()
            .any(|routed| matches!(
                &routed.frame,
                HarnessOutputMessage::CurrentSessionResult(result)
                    if result.request_id == "current-1"
                        && result.session_id.as_str() == "s1"
                        && result.project_root
                            == std::env::current_dir()
                                .expect("current directory")
                                .canonicalize()
                                .expect("canonical current directory")
            ))
    );
    assert!(other_ui.lock().expect("other UI frames").is_empty());
    assert!(tool.lock().expect("tool frames").is_empty());
}

/// Membership cache limits and invariants fail before retaining a row vector.
#[test]
fn roster_bounds_cached_membership_before_projection() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    for index in 0..=super::super::MAX_SESSION_AGENT_LIST_ENTRIES {
        harness
            .session_roster_ever_loaded_agents
            .insert(tau_proto::AgentId::parse(format!("agent-{index}")).expect("agent id"));
    }

    let error = harness
        .build_session_agent_list(&"s1".into(), tau_proto::SessionAgentListScope::History)
        .expect_err("oversized membership cache");
    assert_eq!(
        error.kind,
        tau_proto::SessionAgentListErrorKind::TooManyAgents
    );

    harness.session_roster_ever_loaded_agents.clear();
    harness
        .session_roster_loaded_agents
        .insert(tau_proto::AgentId::parse("orphan").expect("agent id"));
    let error = harness
        .build_session_agent_list(&"s1".into(), tau_proto::SessionAgentListScope::Current)
        .expect_err("inconsistent membership cache");
    assert_eq!(
        error.kind,
        tau_proto::SessionAgentListErrorKind::SessionRead
    );
    harness.session_roster_loaded_agents.clear();
    harness.session_roster_valid = false;
    let error = harness
        .build_session_agent_list(&"s1".into(), tau_proto::SessionAgentListScope::Current)
        .expect_err("invalid restored projection");
    assert_eq!(
        error.kind,
        tau_proto::SessionAgentListErrorKind::SessionRead
    );
}

/// Response sizing aborts through a bounded writer instead of first retaining
/// an oversized encoded buffer.
#[test]
fn roster_response_size_check_rejects_oversized_content() {
    let message =
        HarnessOutputMessage::SessionAgentListResult(Box::new(tau_proto::SessionAgentListResult {
            request_id: "request".to_owned(),
            session_id: "s1".into(),
            result: tau_proto::SessionAgentListResultPayload::Error {
                error: tau_proto::SessionAgentListError {
                    kind: tau_proto::SessionAgentListErrorKind::ResponseTooLarge,
                    message: "x".repeat(tau_proto::MAX_PROTOCOL_MESSAGE_BYTES as usize),
                },
            },
        }));

    assert!(!super::super::session_agent_list_message_fits(&message));
}
