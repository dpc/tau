//! Standalone compaction terminalization regression tests.

use super::*;

/// A late tool-surface collision must terminalize an already-started
/// compaction without sending provider work or retaining its runtime owner.
#[test]
fn late_prompt_surface_failure_terminalizes_running_compaction() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = crate::parse_agent_id(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .as_deref()
            .expect("durable agent"),
    );
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-late-surface").expect("transaction id");
    let compact_prompt_id = test_agent_prompt_id("ap-late-surface");
    h.session_runtime
        .agent_store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::Root,
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                agent_id: agent_id.clone(),
                transaction_id: transaction_id.clone(),
                compact_prompt_id: compact_prompt_id.clone(),
                cut: tau_proto::AgentHead::Root,
                resume_through: None,
                model: "test/model".into(),
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator: tau_proto::PromptOriginator::User,
                supersedes: None,
                trigger: tau_proto::StandaloneCompactionTrigger::Manual,
            }),
            tau_proto::UnixMicros::now(),
        )
        .expect("seed unresolved durable compaction start");
    let agent = h
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("user agent");
    agent.dispatch.activation_dispatch = path_crate_agent::ActivationDispatchState::Running {
        id: transaction_id.clone(),
        cut: tau_proto::AgentHead::Root,
        resume_through: None,
        model: "test/model".into(),
        branch_generation: 0,
        compact_prompt_id: compact_prompt_id.clone(),
    };
    agent.dispatch.in_flight_prompt = Some(compact_prompt_id.clone());

    for internal_name in ["first_internal", "second_internal"] {
        h.tool_routing.registry.register(
            &crate::test_connection_id("late-surface-test"),
            ToolSpec {
                provider_scope: None,
                name: ToolName::new(internal_name),
                model_visible_name: Some(ToolName::new("duplicate_visible")),
                description: None,
                tool_type: tau_proto::ToolType::Function,
                parameters: None,
                format: None,
                tags: Vec::new(),
                enabled_by_default: true,
                background_support: None,
                examples: Vec::new(),
            },
        );
    }

    assert!(h.prepare_agent_prompt_for_dispatch(&cid).is_none());
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.message.contains("duplicate model-visible name `duplicate_visible`")
    )));
    let records = h
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("agent records");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentStandaloneCompactionStarted(started)
                    if started.transaction_id == transaction_id
                        && started.cut == tau_proto::AgentHead::Root
                        && started.resume_through.is_none()
                        && started.compact_prompt_id == compact_prompt_id
            ))
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentStandaloneCompactionFailed(failed)
                    if failed.transaction_id == transaction_id
                        && failed.cut == tau_proto::AgentHead::Root
                        && failed.resume_through.is_none()
                        && failed.reason
                            == tau_proto::StandaloneCompactionFailureReason::RouteFailed
            ))
            .count(),
        1
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(created) if created.agent_prompt_id == compact_prompt_id
    )));
    assert!(!records.iter().any(|record| matches!(
        &record.event,
        Event::AgentPromptStarted(started) if started.agent_prompt_id == compact_prompt_id
    )));
    let agent = &h.agent_runtime.agent_registry.agents[&cid];
    assert!(matches!(
        agent.dispatch.activation_dispatch,
        path_crate_agent::ActivationDispatchState::None
    ));
    assert!(agent.dispatch.in_flight_prompt.is_none());
    h.shutdown().expect("shutdown");
}
