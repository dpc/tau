//! Focused generation-negative explicit-compaction admission oracles.

use super::*;

/// An explicit request remains admissible through generation-scoped negative
/// evidence so the provider can refresh identity; this does not restore the
/// automatic-compaction capability bit.
#[test]
fn manual_cross_compaction_admits_generation_negative_model() {
    let (_td, mut h, caller, _target, call, target_id) = setup_manual_cross_compaction_test();
    let model = h
        .provider_runtime
        .model_info
        .get_mut(&"echo/model".into())
        .expect("echo model");
    model.supports_standalone_compaction = false;
    model.standalone_compaction_generation_negative = true;

    h.request_agent_tool_compaction(
        &caller,
        &call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );

    assert!(event_log_events(&h).into_iter().any(|event| matches!(
        event,
        Event::AgentStandaloneCompactionStarted(started)
            if started.agent_id == target_id
    )));
    assert!(!h.provider_runtime.model_info[&"echo/model".into()].supports_standalone_compaction);
}

/// Without a numeric provider default, the default policy has no boundary.
/// Explicit UI requests still reach an extension with generation-negative
/// evidence; the harness does not fabricate an extension-specific lowerer.
#[test]
fn missing_provider_default_keeps_automatic_compaction_boundaryless_but_admits_manual_ui() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = false;
    info.standalone_compaction_generation_negative = true;
    info.standalone_compaction_threshold = None;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id.clone(),
            text: "retained context".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    {
        let agent = h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        agent.execution.context_input_tokens = Some(tau_proto::TokenCount::new(1_000));
        agent.execution.context_usage_model = Some("test/model".into());
        agent.execution.context_usage_prompt_id =
            Some(test_agent_prompt_id("ap-test-provider-usage"));
        agent.execution.context_usage_head = agent.identity.head;
    }
    assert!(
        !h.schedule_standalone_auto_compaction(&cid),
        "provider_default without a numeric default has no scheduling authority"
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentStandaloneCompactionStarted(_)))
            .count(),
        0,
        "automatic scheduling must not create a transaction"
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentPromptCreated(prompt)
                    if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction
            ))
            .count(),
        0,
        "automatic scheduling must not create a provider compact prompt"
    );

    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );

    let compact = read_nth_prompt_created(&h, 0);
    assert_eq!(
        compact.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    let starts = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 1);
    assert!(matches!(
        starts[0].trigger,
        tau_proto::StandaloneCompactionTrigger::ManualUi { .. }
    ));
    h.shutdown().expect("shutdown");
}
