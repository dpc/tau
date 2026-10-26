//! Native parser success remains distinct from canonical replacement admission.

use super::*;

/// A parser-accepted native item is not installed until canonical admission;
/// release installs it once, unchanged, including after cold replay. Paired
/// with Codex `successful_compact_response_retains_exact_terminal` and CLI
/// `standalone_compaction_stream_is_hidden_from_cli_output`: these component
/// oracles share the opaque item, not a whole-stack capture-to-UI execution.
#[test]
fn standalone_compaction_accepts_canonical_opaque_provider_item() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = quiet_provider_harness(&state).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    seed_reactive_compaction_prefix(&mut h, &cid);
    let initial_tree = agent_tree_for_conversation(&h, &cid).clone();
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent");
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&agent_id),
    );
    let compact = read_nth_prompt_created(&h, 0);
    let replacement = ContextItem::Compaction(
        tau_proto::OpaqueProviderItem::from_raw_json(
            r#"{"type":"compaction","id":"cmp_1","encrypted_content":"opaque"}"#,
        )
        .expect("valid compaction item"),
    );

    // Accept accounting, but reject the replacement's semantic admission.
    h.session_runtime
        .persistence_owner
        .as_ref()
        .expect("owner")
        .reject_admissions_after_for_test(1, 1);
    h.handle_provider_response_finished(ProviderResponseFinished {
        // Display evidence alone cannot turn a successful terminal into failure.
        error_category: Some(tau_proto::ProviderErrorCategory::Transport),
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: compact.agent_prompt_id,
        agent_id: crate::parse_agent_id(&agent_id),
        output_items: vec![replacement.clone()],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        final_status_disposition: tau_proto::FinalStatusDisposition::Accepted,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("retain parser-accepted compaction");

    assert_eq!(
        agent_tree_for_conversation(&h, &cid).current_branch(),
        initial_tree.current_branch()
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .contains_key(&cid)
    );
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentCompacted(_)))
    );
    assert!(
        !h.session_runtime
            .agent_store
            .snapshot_agent_events_for_test(&agent_id)
            .expect("accepted records")
            .iter()
            .any(|record| matches!(&record.event, Event::AgentCompacted(_)))
    );

    // Retry the retained owner, never resend the provider result. This checks
    // admission, not runtime wake acquisition (covered by the capacity
    // oracles).
    for _ in 0..2 {
        h.handle_publication_capacity_ready();
    }
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .is_empty()
    );
    let compacted = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentCompacted(compacted) => Some(compacted),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(compacted.len(), 1);
    assert_eq!(compacted[0].replacement_window, vec![replacement.clone()]);
    let installed_tree = agent_tree_for_conversation(&h, &cid).clone();
    assert!(matches!(
        installed_tree.current_branch().last(),
        Some(tau_core::AgentEntry::Compaction { replacement_window, .. })
            if replacement_window == &vec![replacement.clone()]
    ));

    // Canonical acceptance is not an fsync barrier. Orderly shutdown drains
    // persistence before the cold replay assertions below.
    h.shutdown().expect("shutdown");
    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let resumed_cid = ensure_test_user_agent(&mut resumed);
    assert_eq!(
        agent_tree_for_conversation(&resumed, &resumed_cid).current_branch(),
        installed_tree.current_branch()
    );
    let records = resumed
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(&agent_id)
        .expect("cold durable records");
    let replacements = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentCompacted(compacted) => Some(compacted),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(replacements.len(), 1);
    assert_eq!(replacements[0].replacement_window, vec![replacement]);
    resumed.shutdown().expect("shutdown resumed");
}
