//! Discovery authority follows original canonical occurrences, not summary
//! text.

use super::*;

/// Construct a small typed receipt without coupling the test to harness
/// scanning.
fn discovery_delivery(identity: &str, content: &str) -> tau_proto::DiscoveryContextDelivery {
    let item = tau_proto::DiscoveryContextItem {
        identity: identity.to_owned(),
        version: blake3::hash(content.as_bytes()).to_hex().to_string(),
        content: content.to_owned(),
    };
    tau_proto::DiscoveryContextDelivery {
        scope: vec![item.reference()],
        items: vec![item],
    }
}

/// Live and cold folds agree on initial and post-tool receipts. Compaction
/// discards consumed receipt authority while retaining original suffix
/// receipts.
#[test]
fn discovery_receipts_follow_live_replay_and_compaction_suffix() {
    let agent = agent_id();
    let mut live = AgentTree::from_events(agent.clone(), &[]);
    let mut records = Vec::new();
    let mut append = |tree: &mut AgentTree, event| {
        let record = PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([records.len() as u8; 16]),
            seq: tree.next_event_seq(),
            source: None,
            event,
            parent: AgentEventParent::InheritHead,
            fold_semantics: AgentJournalFoldSemantics::CommitOrder,
            recorded_at: UnixMicros::default(),
        };
        tree.apply_persisted_record(&record).expect("live append");
        records.push(record);
        let cold = AgentTree::try_from_events(agent.clone(), &records).expect("cold prefix");
        assert_eq!(
            tree.discovery_context_receipts(tree.head()),
            cold.discovery_context_receipts(cold.head()),
        );
    };
    let first = discovery_delivery("agents:/first/AGENTS.md", "first");
    append(
        &mut live,
        Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            agent_id: agent.clone(),
            inference_activation: false,
            text: String::new(),
            discovery_context: Some(first.clone()),
            message_class: tau_proto::PromptMessageClass::Internal,
        }),
    );
    let first_cut = AgentHead::Node(live.head().expect("initial"));
    assert_eq!(live.discovery_context_receipts(live.head()), vec![&first]);
    let second = discovery_delivery("agents:/second/AGENTS.md", "second");
    append(
        &mut live,
        Event::ProviderResponseFinished(tool_calling_response(
            &agent,
            "ap-discovery-tool",
            vec!["discovery-tool".into()],
        )),
    );
    append(
        &mut live,
        Event::ProviderToolResult(tau_proto::ToolResult {
            presentation: Default::default(),
            call_id: "discovery-tool".into(),
            tool_name: ToolName::new("workdir"),
            tool_type: ToolType::Function,
            result: tau_proto::CborValue::Text("cwd committed".to_owned()),
            provider_content: Vec::new(),
            kind: ToolResultKind::Final,
            display: None,
            originator: PromptOriginator::User,
        }),
    );
    append(
        &mut live,
        Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            agent_id: agent.clone(),
            inference_activation: false,
            text: String::new(),
            discovery_context: Some(second.clone()),
            message_class: tau_proto::PromptMessageClass::Internal,
        }),
    );
    assert_eq!(
        live.discovery_context_receipts(live.head()),
        vec![&first, &second]
    );
    let second_head = AgentHead::Node(live.head().expect("second receipt"));
    append(
        &mut live,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent.clone(),
            head: first_cut,
        }),
    );
    assert_eq!(live.discovery_context_receipts(live.head()), vec![&first]);
    append(
        &mut live,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent.clone(),
            head: second_head,
        }),
    );
    assert_eq!(
        live.discovery_context_receipts(live.head()),
        vec![&first, &second]
    );
    for (name, cut) in [
        ("ct-discovery-prefix", Some(first_cut)),
        ("ct-discovery-all", None),
    ] {
        let suffix_end = AgentHead::Node(live.head().expect("head"));
        let cut = cut.unwrap_or_else(|| {
            AgentHead::Node(
                live.active_provider_window(live.head())
                    .transcript
                    .last()
                    .expect("suffix")
                    .0,
            )
        });
        let mut started = compaction_start(name);
        started.compact_prompt_id = format!("ap-{name}").parse().expect("prompt id");
        started.cut = cut;
        started.resume_through = None;
        append(
            &mut live,
            Event::AgentStandaloneCompactionStarted(started.clone()),
        );
        append(
            &mut live,
            Event::AgentCompacted(tau_proto::AgentCompacted {
                original_input_tokens: None,
                compaction_output_tokens: None,
                compact_prompt_id: started.compact_prompt_id,
                model: started.model,
                operation: started.operation,
                agent_id: agent.clone(),
                transaction_id: started.transaction_id,
                cut,
                suffix_end,
                replacement_window: vec![ContextItem::Compaction(
                    tau_proto::OpaqueProviderItem::from_raw_json(
                        r#"{"type":"compaction","encrypted_content":"discovery-summary"}"#,
                    )
                    .expect("opaque"),
                )],
            }),
        );
        let receipts = live.discovery_context_receipts(live.head());
        if name == "ct-discovery-prefix" {
            assert_eq!(receipts, vec![&second]);
        } else {
            assert!(
                receipts.is_empty(),
                "opaque summaries cannot assert delivery receipts"
            );
        }
    }
}
