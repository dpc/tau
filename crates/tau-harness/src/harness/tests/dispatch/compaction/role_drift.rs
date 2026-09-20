//! Tests queued manual-compaction eligibility across role changes.

use super::*;

/// A queued UI request follows its captured model rather than the role name
/// used to resolve that model, so a role-following agent remains eligible after
/// switching to another role backed by the same model.
#[test]
fn queued_ui_compaction_allows_same_model_role_drift() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .identity
        .role = None;
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    seed_agent_thinking(&mut h, &cid, "ap-role-drift-ui-compact");

    let accepted_role = h.config.selected_role.clone();
    let replacement_role = "same-model-replacement".to_owned();
    let role = h.config.available_roles[&accepted_role].clone();
    h.config
        .available_roles
        .insert(replacement_role.clone(), role);
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentManualCompactionRequested(request)
            if matches!(
                &request.source,
                tau_proto::ManualCompactionSource::UiCompact { ui_compact }
                    if ui_compact.target_role == accepted_role
            )
    )));

    h.handle_ui_role_select(
        crate::harness::harness_connection_id(),
        tau_proto::UiRoleSelect {
            role: replacement_role,
        },
    )
    .expect("select replacement role");
    h.set_agent_turn_state(&cid, AgentTurnState::Idle);

    assert!(h.try_start_queued_ui_compaction(&cid));
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentStandaloneCompactionStarted(started)
            if matches!(
                started.trigger,
                tau_proto::StandaloneCompactionTrigger::ManualUi { .. }
            )
    )));
    assert!(!event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentManualCompactionRequestFailed(failed)
            if failed.reason == tau_proto::ManualCompactionRequestFailureReason::ModelChanged
    )));
}
