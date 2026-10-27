//! Captured report authority is ordinary-inference-only across provider loss.

use super::*;

/// Exercise real report ingress parked at a Tool interceptor, with both model
/// declarations committed before dispatch so global interception cannot hide a
/// newly advertised fallback route.
fn assert_parked_standalone_terminal(disconnect: bool, fallback: bool) {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let mut model = h.provider_runtime.model_info[&"test/model".into()].clone();
    model.supports_compaction = false;
    model.supports_standalone_compaction = true;
    let provider = h.provider_runtime.model_routes[&model.id].clone();
    let fallback_provider = crate::test_connection_id("aaa-fallback-provider");
    if fallback {
        assert!(fallback_provider < provider, "original route must win");
        connect_ready_configured_extension(
            &mut h,
            fallback_provider.as_str(),
            fallback_provider.as_str(),
            tau_proto::ClientKind::Provider,
        );
        h.handle_extension_event_inner(
            &fallback_provider,
            Event::ProviderModelsDeclared(tau_proto::ProviderModelsDeclared {
                models: vec![model.clone()],
            }),
        )
        .expect("declare already active fallback");
    }
    h.handle_extension_event_inner(
        &provider,
        Event::ProviderModelsDeclared(tau_proto::ProviderModelsDeclared {
            models: vec![model.clone()],
        }),
    )
    .expect("declare standalone capability");
    assert_eq!(h.provider_runtime.model_routes[&model.id], provider);
    let cid = ensure_test_user_agent(&mut h);
    seed_reactive_compaction_prefix(&mut h, &cid);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&agent_id),
    );
    let compact = read_nth_prompt_created(&h, 0);
    assert_eq!(
        h.provider_runtime.pending_prompts[&compact.agent_prompt_id],
        provider
    );
    let initial_tree = agent_tree_for_conversation(&h, &cid).clone();
    let replacement = ContextItem::Compaction(
        tau_proto::OpaqueProviderItem::from_raw_json(
            r#"{"type":"compaction","id":"cmp_parked","encrypted_content":"opaque"}"#,
        )
        .expect("opaque item"),
    );
    let mut response = provider_text_response(&compact.agent_prompt_id, agent_id.clone(), "unused");
    response.output_items = vec![replacement.clone()];
    connect_test_tool(&mut h, "standalone-report-interceptor");
    h.handle_extension_event(
        "standalone-report-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_RESPONSE_FINISHED_REPORTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register report interceptor");
    h.handle_extension_event_inner(
        &provider,
        Event::ProviderResponseFinishedReported(response.into()),
    )
    .expect("park exact terminal");
    assert!(matches!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|p| &p.event),
        Some(Event::ProviderResponseFinishedReported(_))
    ));
    if disconnect {
        h.handle_disconnect(&provider);
        assert!(
            !h.provider_runtime
                .pending_prompts
                .contains_key(&compact.agent_prompt_id)
        );
        assert_eq!(
            h.provider_runtime.model_routes.get(&model.id),
            fallback.then_some(&fallback_provider)
        );
    }
    h.handle_extension_event(
        "standalone-report-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release report");
    for _ in 0..2 {
        h.handle_publication_capacity_ready();
    }
    let events = event_log_events(&h);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event,
                Event::ProviderResponseFinishedReported(report)
                    if report.agent_prompt_id == compact.agent_prompt_id
            ))
            .count(),
        1,
        "the stale report remains an observation"
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::AgentStandaloneCompactionFailed(_) | Event::ProviderResponseFinished(_)
    )));
    let replacements = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentCompacted(compacted) => Some(compacted),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(replacements.len(), usize::from(!disconnect));
    if disconnect {
        assert_eq!(
            agent_tree_for_conversation(&h, &cid).current_branch(),
            initial_tree.current_branch()
        );
    } else {
        assert_eq!(replacements[0].replacement_window, vec![replacement]);
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        1,
        "neither stale report nor manual success dispatches a successor"
    );
    let accounting = events
        .iter()
        .filter_map(|event| match event {
            Event::ProviderStandaloneExecutionAccounted(accounted)
                if accounted.agent_prompt_id == compact.agent_prompt_id =>
            {
                Some(accounted)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(accounting.len(), 1, "independent accounting closes once");
    if disconnect {
        assert_eq!(
            accounting[0].usage,
            tau_proto::StandaloneExecutionUsage::Unknown
        );
        assert_eq!(
            accounting[0].output,
            tau_proto::StandaloneExecutionOutput::Rejected
        );
    }
    h.shutdown().expect("shutdown");
}

/// A parked terminal cannot derive StaleBranch after its only provider
/// disappears.
#[test]
fn parked_standalone_terminal_loses_authority_without_route() {
    assert_parked_standalone_terminal(true, false);
}

/// A preexisting duplicate-model route cannot lend authority to a stale
/// terminal.
#[test]
fn parked_standalone_terminal_loses_authority_with_surviving_route() {
    assert_parked_standalone_terminal(true, true);
}

/// A still-current source installs the same valid parked replacement exactly
/// once.
#[test]
fn parked_standalone_terminal_live_source_installs_once() {
    assert_parked_standalone_terminal(false, true);
}
