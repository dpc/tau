//! Fresh provider usage and byte-cap admission after a standalone summary.

use super::*;

fn start_replacement_only_continuation(
    h: &mut Harness,
) -> (AgentId, tau_proto::AgentPromptCreated) {
    enable_remote_compaction_for_test_model(h);
    let cid = ensure_test_user_agent(h);
    {
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("model");
        info.supports_compaction = false;
        info.supports_standalone_compaction = true;
        info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));
    }
    establish_exact_provider_usage(h, &cid, 100);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model")
        .standalone_compaction_threshold = Some(tau_proto::TokenCount::new(100));
    assert!(h.schedule_standalone_auto_compaction_for_activation(&cid, true));
    let compact = read_nth_prompt_created(h, 1);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model")
        .standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));
    h.handle_provider_response_finished(provider_text_response(
        &compact.agent_prompt_id,
        compact.agent_id,
        "replacement only",
    ))
    .expect("summary");
    (cid, read_nth_prompt_created(h, 2))
}

/// A standalone-owned ordinary response from a deselected branch cannot grant
/// token authority to the selected branch.
#[test]
fn deselected_standalone_continuation_does_not_install_fresh_usage() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let (cid, inference) = start_replacement_only_continuation(&mut h);
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: inference.agent_id.clone(),
            head: tau_proto::AgentHead::Root,
        }),
    );
    let mut response = provider_text_response(&inference.agent_prompt_id, inference.agent_id, "");
    response.output_items.clear();
    response.usage = Some(tau_proto::ProviderTokenUsage {
        model: Some(inference.model),
        prompt_sent_tokens: 100,
        ..Default::default()
    });
    let before = event_log_events(&h).len();
    h.handle_provider_response_finished(response)
        .expect("off-branch response");
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        None
    );
    assert!(
        !event_log_events(&h)[before..].iter().any(|event| matches!(
            event, Event::HarnessAgentContextUsageChanged(usage)
                if usage.agent_id == cid && usage.input_tokens == Some(100)
        )),
        "off-branch response must not publish selected usage"
    );
    assert_eq!(
        h.automatic_compaction_reported_input_tokens(&cid, &"test/model".into()),
        None
    );
    h.shutdown().expect("shutdown");
}

/// Provider-visible replacement-only history reaches automatic byte-cap failure
/// with fresh ordinary usage. The terminal adds an empty structural response,
/// not provider-visible suffix content; boundary-only fitting is pinned too.
#[test]
fn automatic_replacement_only_over_budget_commits_one_preflight_failure() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let (cid, inference) = start_replacement_only_continuation(&mut h);
    let before_tree = h
        .session_runtime
        .agent_store
        .agent(inference.agent_id.as_str())
        .expect("tree");
    assert!(
        before_tree
            .unresolved_inference_through(&inference.agent_prompt_id)
            .is_some(),
        "recovery={:?}",
        before_tree.inference_dispatch_recovery()
    );
    let mut response = provider_text_response(&inference.agent_prompt_id, inference.agent_id, "");
    response.output_items.clear();
    response.usage = Some(tau_proto::ProviderTokenUsage {
        model: Some(inference.model),
        prompt_sent_tokens: 100,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 0,
        stats: Default::default(),
    });
    h.handle_provider_response_finished(response)
        .expect("fresh usage");
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let tree = h
        .session_runtime
        .agent_store
        .agent(&agent_id)
        .expect("tree");
    let window = tree.active_provider_window(tree.head());
    let records = h
        .session_runtime
        .agent_store
        .agent_events(&agent_id)
        .expect("records");
    let cold = tau_core::AgentTree::from_events(agent_id.clone(), &records);
    assert_eq!(
        cold.provider_prompt_for_response_node(cold.head().expect("response")),
        Some(inference.agent_prompt_id.clone())
    );
    assert_eq!(
        crate::prompt::assemble_prompt_context_from(&cold, cold.head()).context,
        crate::prompt::assemble_prompt_context_from(tree, tree.head()).context
    );
    let (_, tokens, _, _, usage_prompt) = h
        .agent_context_usage_at(&agent_id, tree.head())
        .expect("durable fresh usage");
    assert_eq!(tokens, tau_proto::TokenCount::new(100));
    assert_eq!(usage_prompt, inference.agent_prompt_id);
    assert_eq!(
        h.automatic_compaction_reported_input_tokens(&cid, &"test/model".into()),
        Some(tokens)
    );
    assert!(window.replacement.is_some());
    assert!(
        window.transcript.iter().all(|(_, entry)| matches!(
            entry, tau_core::AgentEntry::AssistantResponse { output_items, .. }
                if output_items.is_empty()
        )),
        "the fresh usage terminal adds no provider-visible suffix"
    );
    assert_eq!(
        h.fitting_standalone_compaction_cut(
            &agent_id,
            tau_proto::AgentHead::Node(window.replacement_boundary.expect("replacement")),
            tau_proto::ByteCount::new(1),
        ),
        None
    );
    {
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("model");
        info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(100));
        info.standalone_compaction_prefix_budget = Some(tau_proto::ByteCount::new(1));
    }
    assert!(
        h.schedule_standalone_auto_compaction_for_activation(&cid, true),
        "usage={:?}, dispatch={:?}, input={:?}, model={:?}, prompt={:?}",
        h.automatic_compaction_reported_input_tokens(&cid, &"test/model".into()),
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_usage_model,
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_usage_prompt_id
    );
    assert!(!h.schedule_standalone_auto_compaction_for_activation(&cid, true));
    let events = event_log_events(&h);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event, Event::AgentStandaloneCompactionFailed(failed)
                    if failed.reason == tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge
            ))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event, Event::AgentPromptCreated(prompt)
                    if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction
            ))
            .count(),
        1,
        "only the first successful summary dispatched"
    );
    h.shutdown().expect("shutdown");
}
