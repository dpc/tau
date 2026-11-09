//! Canonical discovery delivery without independent model activation.

use super::workdir_refresh::{
    commit_cwd, initialize_source, refresh_reply, refresh_reply_with_agents,
};
use super::*;
use crate::harness::tests::dispatch::{
    final_tool_result, provider_tool_response, shared_test_tool_spec,
};

/// Initial discovery remains passive until an ordinary turn, appears only in
/// user context, and subsequent requests retain that one original occurrence.
#[test]
fn discovery_initial_delivery_is_lazy_and_not_system_content() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    h.apply_session_discovery_snapshot(
        &crate::test_connection_id("discovery"),
        snapshot(
            vec![skill(
                &tmp.path().join("skill.md"),
                "lazy-catalog-sentinel",
                None,
            )],
            vec![tau_proto::DiscoveryAgentsFile {
                file_path: tmp.path().join("AGENTS.md"),
                display_path: None,
                content: "lazy-agents-sentinel </agents_md>".to_owned(),
            }],
        ),
    );
    let cid = ensure_test_user_agent(&mut h);
    assert_eq!(prompt_created_count(&h), 0);
    assert!(!event_log_events(&h).iter().any(|event| matches!(
        event, Event::AgentUserMessageInjected(injected) if injected.discovery_context.is_some()
    )));
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("first".to_owned()))
        .expect("first turn");
    assert_eq!(prompt_created_count(&h), 1);
    let first = read_nth_prompt_created(&h, 0);
    assert!(!first.system_prompt.contains("lazy-catalog-sentinel"));
    assert!(!first.system_prompt.contains("lazy-agents-sentinel"));
    let deliveries = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentUserMessageInjected(injected) if injected.discovery_context.is_some() => {
                Some(injected)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(deliveries.len(), 1);
    assert!(!deliveries[0].inference_activation);
    let rendered = deliveries[0]
        .discovery_context
        .as_ref()
        .expect("delivery")
        .render();
    assert!(deliveries[0].text.is_empty());
    let envelopes = deliveries[0]
        .discovery_context
        .as_ref()
        .expect("initial discovery")
        .render_envelopes();
    assert_eq!(envelopes.len(), 2);
    assert!(tau_proto::SKILLS_PAYLOAD_ENVELOPE.matches_whole(&envelopes[0]));
    assert!(tau_proto::AGENTS_MD_PAYLOAD_ENVELOPE.matches_whole(&envelopes[1]));
    assert!(rendered.contains("lazy-agents-sentinel &lt;/agents_md&gt;"));
    assert!(
        first.context.flatten().iter().any(|item| matches!(item,
            tau_proto::ContextItem::Message(message)
                if message.content == envelopes.iter().map(|text| tau_proto::ContentPart::Text {
                    text: text.clone(),
                }).collect::<Vec<_>>()
        )),
        "each discovery envelope has its own generic text carrier"
    );
    assert!(
        serde_json::to_string(&first.context)
            .expect("context")
            .contains("lazy-catalog-sentinel")
    );
    assert!(h.discovery_context_delta(&cid).is_none());
    h.handle_provider_response_finished(provider_text_response(
        &first.agent_prompt_id,
        first.agent_id.clone(),
        "done",
    ))
    .expect("finish");
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("second".to_owned()))
        .expect("second turn");
    assert_eq!(prompt_created_count(&h), 2);
    let second = read_nth_prompt_created(&h, 1);
    let text = serde_json::to_string(&second.context).expect("context");
    assert_eq!(text.matches("<skills>").count(), 1);
    assert_eq!(text.matches("<agents_md ").count(), 1);
    h.shutdown().expect("shutdown");
}

/// Instruction refreshes use the same cumulative receipts as advertisements:
/// removals and unchanged returns are silent, edits and reversions resend full
/// instructions, and repeated visits never manufacture scope-only messages.
#[test]
fn discovery_instruction_refreshes_deliver_only_new_or_latest_changed_files() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(&mut h, "one", "one", tau_proto::ClientKind::Tool);
    register_discovery_test_provider(&mut h, "one");
    let role = h.config.selected_role.clone();
    let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    let user = skill(&tmp.path().join("user.md"), "user", None);
    let project = skill(&tmp.path().join("project.md"), "project", None);
    initialize_source(&mut h, &agent, "one", user.clone(), project.clone());
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("initial".to_owned()))
        .expect("initial");
    let mut prompt = read_nth_prompt_created(&h, 0);
    let original = tau_proto::DiscoveryAgentsFile {
        display_path: None,
        file_path: tmp.path().join("AGENTS.project.md"),
        content: "old project instructions one".to_owned(),
    };
    let new = tau_proto::DiscoveryAgentsFile {
        display_path: None,
        file_path: tmp.path().join("other/AGENTS.md"),
        content: "new instructions".to_owned(),
    };
    let mut edited = original.clone();
    edited.content = "replacement instructions </agents_md>".to_owned();
    let mut count = 1;
    for (index, files) in [
        vec![],
        vec![new],
        vec![original.clone()],
        vec![edited],
        vec![original.clone()],
        vec![original],
    ]
    .into_iter()
    .enumerate()
    {
        h.handle_provider_response_finished(provider_text_response(
            &prompt.agent_prompt_id,
            prompt.agent_id.clone(),
            "done",
        ))
        .expect("finish");
        let revision = commit_cwd(&mut h, &agent, "one", &format!("instructions-{index}"));
        refresh_reply_with_agents(
            &mut h,
            &agent,
            "one",
            revision,
            vec![user.clone(), project.clone()],
            files,
        );
        let needs_delivery = matches!(index, 1 | 3 | 4);
        assert_eq!(h.discovery_context_delta(&cid).is_some(), needs_delivery);
        assert_eq!(
            prompt_created_count(&h),
            (index + 1) as u64,
            "refresh stays passive"
        );
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("next".to_owned()))
            .expect("next");
        prompt = read_nth_prompt_created(&h, index + 1);
        let deliveries = event_log_events(&h)
            .into_iter()
            .filter_map(|event| match event {
                Event::AgentUserMessageInjected(injected) => injected.discovery_context,
                _ => None,
            })
            .collect::<Vec<_>>();
        count += usize::from(needs_delivery);
        assert_eq!(deliveries.len(), count);
        if needs_delivery {
            let delivery = deliveries.last().expect("delivery");
            assert_eq!(delivery.items.len(), 1);
            assert_eq!(delivery.updated.is_empty(), index == 1);
            let text = delivery.render();
            assert!(!text.contains("<skills>"));
            assert_eq!(text.matches("<agents_md ").count(), 1);
            if index == 3 {
                assert!(text.contains("updated=\"true\""));
                assert!(text.contains("replacement instructions &lt;/agents_md&gt;"));
            } else if index == 4 {
                assert!(text.contains("updated=\"true\""));
                assert!(text.contains("old project instructions one"));
            }
        }
        assert!(h.discovery_context_delta(&cid).is_none());
    }
    h.shutdown().expect("shutdown");
}

/// A source refresh follows the closed tool round as user input in the same
/// continuation; unchanged items are omitted cumulatively, while edits and
/// reversions replace the latest delivered content with full updated text.
#[test]
fn discovery_workdir_terminal_preserves_turn_and_reuses_surviving_versions() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(&mut h, "one", "one", tau_proto::ClientKind::Tool);
    register_discovery_test_provider(&mut h, "one");
    h.tool_routing.registry.register(
        &crate::test_connection_id("one"),
        shared_test_tool_spec("workdir"),
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("one"),
        shared_test_tool_spec("other"),
    );
    let role = h.config.selected_role.clone();
    let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    let user = skill(&tmp.path().join("user.md"), "user", None);
    let old = skill(&tmp.path().join("old.md"), "old", None);
    let new = skill(&tmp.path().join("new.md"), "new", None);
    let mut edited = old.clone();
    edited.description = "edited advertisement at the same identity".to_owned();
    initialize_source(&mut h, &agent, "one", user.clone(), old.clone());
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("move directories".to_owned()))
        .expect("initial");
    let mut prompt = read_nth_prompt_created(&h, 0);
    for (index, project) in [new, old.clone(), old.clone(), edited, old.clone(), old]
        .into_iter()
        .enumerate()
    {
        let call_id = format!("workdir-{index}");
        let sibling_id = format!("other-{index}");
        let mut response =
            provider_tool_response(&prompt, &call_id, "workdir", CborValue::Map(Vec::new()));
        response.output_items.extend(
            provider_tool_response(&prompt, &sibling_id, "other", CborValue::Map(Vec::new()))
                .output_items,
        );
        h.handle_provider_response_finished(response)
            .expect("parallel tool round");
        let revision = commit_cwd(&mut h, &agent, "one", &format!("move-{index}"));
        refresh_reply(&mut h, &agent, "one", revision, vec![user.clone(), project]);
        assert_eq!(
            prompt_created_count(&h),
            u64::try_from(index + 1).expect("count"),
            "refresh must not call a provider"
        );
        let result = final_tool_result(&call_id, "workdir", "cwd committed");
        h.handle_extension_event(
            "one",
            TestProtocolItem::Event(Event::ToolResultReported(result)),
        )
        .expect("workdir terminal");
        assert_eq!(
            prompt_created_count(&h),
            u64::try_from(index + 1).expect("count"),
            "discovery must wait for the complete tool round"
        );
        h.handle_extension_event(
            "one",
            TestProtocolItem::Event(Event::ToolResultReported(final_tool_result(
                &sibling_id,
                "other",
                "other completed",
            ))),
        )
        .expect("sibling terminal");
        assert_eq!(
            prompt_created_count(&h),
            u64::try_from(index + 2).expect("count")
        );
        prompt = read_nth_prompt_created(&h, index + 1);
        let terminal = event_log_events(&h)
            .into_iter()
            .find_map(|event| match event {
                Event::ProviderToolResult(result) if result.call_id.as_str() == call_id => {
                    Some(result)
                }
                _ => None,
            })
            .expect("canonical result");
        assert_eq!(
            terminal.presentation,
            tau_proto::ToolResultPresentation::ToolPayload
        );
        assert_eq!(terminal.result, CborValue::Text("cwd committed".to_owned()));
        let deliveries = event_log_events(&h)
            .into_iter()
            .filter_map(|event| match event {
                Event::AgentUserMessageInjected(injected) => injected.discovery_context,
                _ => None,
            })
            .collect::<Vec<_>>();
        let delivery = deliveries.last().expect("discovery delivery");
        match index {
            0 => {
                assert_eq!(deliveries.len(), 2);
                assert_eq!(delivery.items.len(), 1);
                assert_eq!(delivery.items[0].identity, "skill:new");
            }
            1 | 2 => assert_eq!(deliveries.len(), 2),
            3 => {
                assert_eq!(deliveries.len(), 3);
                assert_eq!(delivery.items.len(), 1);
                assert_eq!(delivery.items[0].identity, "skill:old");
                assert_eq!(
                    delivery.items[0].content,
                    "edited advertisement at the same identity"
                );
                assert_eq!(delivery.updated, vec!["skill:old"]);
                assert!(
                    delivery
                        .render()
                        .contains("* `old` (updated) - edited advertisement")
                );
            }
            4 => {
                assert_eq!(deliveries.len(), 4);
                assert_eq!(
                    delivery.items.len(),
                    1,
                    "reversion replaces the last delivered edit"
                );
                assert_eq!(delivery.updated, vec!["skill:old"]);
                assert!(!delivery.render().contains("edited advertisement"));
            }
            5 => assert_eq!(deliveries.len(), 4),
            other => panic!("unexpected discovery delivery: {other:?}"),
        }
        if matches!(index, 0 | 3 | 4) {
            let blocks = &prompt.context.blocks;
            assert!(matches!(
                blocks.last(),
                Some(tau_proto::ContextBlock::UserInput(_))
            ));
            assert!(matches!(
                blocks.get(blocks.len() - 2),
                Some(tau_proto::ContextBlock::ToolResults(_))
            ));
        }
        assert!(
            delivery
                .scope
                .iter()
                .any(|item| item.identity == "skill:user")
        );
        assert!(
            delivery
                .items
                .iter()
                .all(|item| item.identity != "skill:user")
        );
        assert!(h.discovery_context_delta(&cid).is_none());
    }
    let events = event_log_events(&h);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::AgentOuterTurnStarted(_)))
            .count(),
        1
    );
    assert_eq!(events.iter().filter(|event| matches!(
        event, Event::AgentUserMessageInjected(injected) if injected.discovery_context.is_some()
    )).count(), 4);
    h.shutdown().expect("shutdown");
}
