//! Provider switching is tested at dispatch without live upstream requests.

use super::*;

/// Selects a synthetic second provider on the existing quiet test route.
fn select_foreign_provider(h: &mut Harness, cid: &AgentId) {
    let source: tau_proto::ModelId = "test/model".into();
    let destination: tau_proto::ModelId = "foreign/model".into();
    let mut info = h.provider_runtime.model_info[&source].clone();
    info.id = destination.clone();
    h.provider_runtime
        .model_info
        .insert(destination.clone(), info);
    h.provider_runtime.model_routes.insert(
        destination.clone(),
        h.provider_runtime.model_routes[&source].clone(),
    );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(cid)
        .expect("agent")
        .identity
        .model_override = Some(destination);
}

/// Counts actual materializations, not selection acknowledgements.
fn request_count(h: &Harness) -> usize {
    event_log_events(h)
        .iter()
        .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
        .count()
}

/// The dispatch boundary must refuse an incompatible native replacement before
/// materializing or delivering another paid request, leaving the prefix intact.
#[test]
fn provider_switch_dispatch_refuses_opaque_compaction_before_provider_request() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&agent_id),
    );
    let prompt = read_nth_prompt_created(&h, 0);
    let replacement = ContextItem::Compaction(
        tau_proto::OpaqueProviderItem::from_raw_json(
            r#"{ "type": "compaction", "encrypted_content": "synthetic" }"#,
        )
        .expect("opaque replacement"),
    );
    let mut response = provider_text_response(&prompt.agent_prompt_id, prompt.agent_id, "unused");
    response.output_items = vec![replacement.clone()];
    h.handle_provider_response_finished(response)
        .expect("compact");
    let requests = request_count(&h);
    select_foreign_provider(&mut h, &cid);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("continue elsewhere".to_owned()))
        .expect("activation handled");
    assert_eq!(request_count(&h), requests);
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event, Event::HarnessNotice(notice) if notice.message.contains("Cannot switch provider")
    )));
    let tree = h
        .session_runtime
        .agent_store
        .agent(&agent_id)
        .expect("tree");
    assert!(
        crate::prompt::assemble_prompt_context_from(tree, tree.head())
            .context
            .flatten()
            .contains(&replacement)
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .in_flight_prompt
            .is_none()
    );
    h.shutdown().expect("shutdown");
}

/// Ordinary dispatch must actually send the portable projection and publish a
/// UI-only diagnostic once, rather than merely testing a conversion helper.
#[test]
fn provider_switch_dispatch_warns_once_and_sends_portable_history() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("first".to_owned()))
        .expect("dispatch");
    let first = read_nth_prompt_created(&h, 0);
    let mut response =
        provider_text_response(&first.agent_prompt_id, first.agent_id, "portable answer");
    response.output_items.insert(
        0,
        ContextItem::Reasoning(
            tau_proto::OpaqueProviderItem::from_raw_json(
                r#"{ "type": "reasoning", "encrypted_content": "synthetic" }"#,
            )
            .expect("reasoning"),
        ),
    );
    h.handle_provider_response_finished(response)
        .expect("finish");
    select_foreign_provider(&mut h, &cid);
    for _ in 0..2 {
        let index = request_count(&h);
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("continue".to_owned()))
            .expect("dispatch");
        let prompt = read_nth_prompt_created(&h, index);
        assert!(
            !prompt
                .context
                .flatten_iter()
                .any(|item| matches!(item, ContextItem::Reasoning(_)))
        );
        assert!(
            serde_json::to_string(&prompt.context)
                .expect("context")
                .contains("portable answer")
        );
        h.handle_provider_response_finished(provider_text_response(
            &prompt.agent_prompt_id,
            prompt.agent_id,
            "continued",
        ))
        .expect("finish");
    }
    let warnings = event_log_events(&h)
        .into_iter()
        .filter(|event| {
            matches!(
                event, Event::HarnessNotice(notice)
                    if notice.message.contains("continuation is best effort")
                        && notice.purpose == tau_proto::NoticePurpose::Diagnostic
            )
        })
        .count();
    assert_eq!(warnings, 1);
    h.shutdown().expect("shutdown");
}
