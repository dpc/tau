use super::*;

fn agent_id() -> AgentId {
    AgentId::parse("agent-metadata-test").expect("valid test agent id")
}

/// Ensures metadata set/unset facts fold into side state without creating
/// transcript nodes, preventing extension state from polluting prompts.
#[test]
fn metadata_set_unset_fold_without_transcript_nodes() {
    let agent_id = agent_id();
    let mut tree = AgentTree::from_events(agent_id.clone(), &[]);
    let key = tau_proto::AgentMetadataKey::new("ext_core-shell_cwd");
    tree.apply_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
        agent_id: agent_id.clone(),
        key: key.clone(),
        value: tau_proto::CborValue::Text("/tmp".to_owned()),
        inheritable: true,
    }));
    assert!(tree.nodes().is_empty());
    assert_eq!(tree.head(), None);
    assert_eq!(
        tree.metadata().get(&key).map(|entry| entry.inheritable),
        Some(true)
    );
    tree.apply_event(&Event::AgentMetadataUnset(tau_proto::AgentMetadataUnset {
        agent_id,
        key: key.clone(),
    }));
    assert!(!tree.metadata().contains_key(&key));
    assert!(tree.nodes().is_empty());
}

/// Ensures child-agent inheritance snapshots only entries explicitly marked
/// inheritable, preventing private extension scratch keys from leaking.
#[test]
fn inheritable_metadata_filters_non_inheritable_entries() {
    let agent_id = agent_id();
    let mut tree = AgentTree::from_events(agent_id.clone(), &[]);
    let inherit_key = tau_proto::AgentMetadataKey::new("inherit");
    let local_key = tau_proto::AgentMetadataKey::new("local");
    for (key, inheritable) in [(inherit_key.clone(), true), (local_key.clone(), false)] {
        tree.apply_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key,
            value: tau_proto::CborValue::Bool(true),
            inheritable,
        }));
    }
    let inherited = tree.inheritable_metadata();
    assert!(inherited.contains_key(&inherit_key));
    assert!(!inherited.contains_key(&local_key));
}

/// Ensures provider tool-call rounds fold only after every terminal result
/// arrives, preserving the original model call order rather than result arrival
/// order.
#[test]
fn provider_tool_round_waits_for_all_terminal_results() {
    let agent_id = agent_id();
    let mut tree = AgentTree::from_events(agent_id.clone(), &[]);
    let first_call_id = ToolCallId::from("call-first");
    let second_call_id = ToolCallId::from("call-second");
    let assistant_node_id = tree
        .apply_event_at(
            AgentEventParent::InheritHead,
            &Event::ProviderResponseFinished(tau_proto::ProviderResponseFinished {
                agent_prompt_id: "sp-tool-round".into(),
                agent_id,
                output_items: vec![
                    ContextItem::ToolCall(ToolCallItem {
                        call_id: first_call_id.clone(),
                        name: ToolName::new("first_tool"),
                        tool_type: ToolType::Function,
                        arguments: tau_proto::CborValue::Null,
                    }),
                    ContextItem::ToolCall(ToolCallItem {
                        call_id: second_call_id.clone(),
                        name: ToolName::new("second_tool"),
                        tool_type: ToolType::Function,
                        arguments: tau_proto::CborValue::Null,
                    }),
                ],
                stop_reason: tau_proto::ProviderStopReason::ToolCalls,
                error: None,
                usage: None,
                originator: PromptOriginator::User,
                compaction_original_input_tokens: None,
                compaction_compacted_input_tokens: None,
                backend: None,
                provider_response_id: Some("response-id".to_owned()),
                ws_pool_delta: None,
            }),
        )
        .expect("assistant response should fold");

    assert_eq!(tree.head(), Some(assistant_node_id));
    assert!(
        tree.apply_event_at(
            AgentEventParent::InheritHead,
            &Event::ProviderToolResult(tau_proto::ToolResult {
                call_id: second_call_id.clone(),
                tool_name: ToolName::new("second_tool"),
                tool_type: ToolType::Function,
                result: tau_proto::CborValue::Text("second done".to_owned()),
                kind: ToolResultKind::Final,
                display: None,
                originator: PromptOriginator::User,
            }),
        )
        .is_none()
    );
    assert_eq!(tree.head(), Some(assistant_node_id));

    let tool_results_node_id = tree
        .apply_event_at(
            AgentEventParent::InheritHead,
            &Event::ProviderToolError(tau_proto::ToolError {
                call_id: first_call_id.clone(),
                tool_name: ToolName::new("first_tool"),
                tool_type: ToolType::Function,
                message: "first failed".to_owned(),
                details: None,
                display: None,
                originator: PromptOriginator::User,
            }),
        )
        .expect("final terminal result should close the round");
    let tool_results_node = tree
        .node(tool_results_node_id)
        .expect("tool results node should exist");
    assert_eq!(tool_results_node.parent_id, Some(assistant_node_id));

    let AgentEntry::ToolResults { items } = &tool_results_node.entry else {
        panic!("expected tool results entry");
    };
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].call_id, first_call_id);
    assert!(matches!(
        items[0].status,
        ToolResultStatus::Error { ref message } if message == "first failed"
    ));
    assert_eq!(items[1].call_id, second_call_id);
    assert!(matches!(items[1].status, ToolResultStatus::Success));
    assert!(
        tree.unresolved_foreground_tool_calls_from(Some(tool_results_node_id))
            .is_empty()
    );
}
