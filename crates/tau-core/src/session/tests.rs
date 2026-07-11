use super::*;

fn agent_id() -> AgentId {
    AgentId::parse("agent-metadata-test").expect("valid test agent id")
}

fn other_agent_id() -> AgentId {
    AgentId::parse("other-agent").expect("valid test agent id")
}

fn prompt_event(agent_id: AgentId) -> Event {
    Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
        agent_id,
        text: "hello".to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    })
}

fn validation_error(tree: &AgentTree, event: Event) -> String {
    tree.validate_event(&event)
        .expect_err("event should be rejected")
        .to_string()
}

/// Watch-turn messages must carry their structured payload, while ordinary
/// messages must not smuggle one into the durable agent transcript.
#[test]
fn validate_event_enforces_watch_turn_state_payload_discriminator() {
    let id = agent_id();
    let tree = AgentTree::from_events(id.clone(), &[]);
    let payload = tau_proto::AgentWatchTurnStateNotification {
        session_id: "session-1".into(),
        subscription_id: "watch-1".to_owned(),
        state: tau_proto::AgentRuntimeState::Running,
        initial: false,
        turn_generation: 1,
    };
    for (kind, watch_turn_state) in [
        (AgentMessageKind::WatchTurnState, None),
        (AgentMessageKind::Message, Some(payload)),
    ] {
        let event = Event::AgentMessageReceived(AgentMessageReceived {
            message_id: "msg-invalid-watch-state".into(),
            sender_id: other_agent_id(),
            sender_session_id: None,
            recipient_id: id.clone(),
            kind,
            watch_turn_state,
            message: String::new(),
        });
        assert!(
            validation_error(&tree, event).contains("payload must be present exactly"),
            "mismatched discriminator and payload must fail closed"
        );
    }
}

fn incoming_message(agent_id: AgentId) -> Event {
    Event::AgentMessageIncoming(tau_proto::AgentMessageIncoming {
        recipient_id: agent_id.clone(),
        envelope: tau_proto::MessageEnvelope {
            message_id: tau_proto::MessageId::new("msg-during-tools"),
            transport: tau_proto::MessageTransportRef {
                name: "slack".to_owned(),
                instance: Some(tau_proto::ExtensionName::from("std-slack")),
            },
            source: tau_proto::MessageEndpoint::External {
                stable_id: Some("U1".to_owned()),
                display_name: None,
                actor_kind: tau_proto::ExternalActorKind::Human,
            },
            destination: tau_proto::MessageEndpoint::Agent {
                session_id: None,
                agent_id,
                display_name: None,
            },
            conversation: None,
            operation: tau_proto::MessageOperation::Create {
                payload: tau_proto::MessagePayload::Text {
                    text: "during tools".to_owned(),
                    format: tau_proto::TextFormat::Plain,
                },
            },
            trust: tau_proto::MessageTrust {
                content: tau_proto::MessageContentTrust::UntrustedExternal,
                identity: tau_proto::SenderIdentityAssurance::VerifiedAccount,
                policy: tau_proto::SenderPolicyStatus::Allowlisted,
            },
            external_identity: None,
            ordering: None,
            occurred_at: None,
            reply_path: None,
        },
    })
}

/// Sender labels must preserve stable verified identity and visibly qualify
/// weaker assurance classes instead of presenting display names as authority.
#[test]
fn message_envelope_sender_labels_reflect_identity_assurance() {
    let Event::AgentMessageIncoming(incoming) = incoming_message(agent_id()) else {
        unreachable!("helper returns incoming message");
    };
    let mut envelope = incoming.envelope;
    let cases = [
        (
            tau_proto::SenderIdentityAssurance::VerifiedAccount,
            Some("U1"),
            Some("Alice"),
            "Alice (U1)",
        ),
        (
            tau_proto::SenderIdentityAssurance::VerifiedAccount,
            Some("U1"),
            None,
            "U1",
        ),
        (
            tau_proto::SenderIdentityAssurance::VerifiedAccount,
            None,
            Some("Alice"),
            "unverified Alice",
        ),
        (
            tau_proto::SenderIdentityAssurance::RoomMembership,
            None,
            Some("Alice"),
            "room occupant Alice",
        ),
        (
            tau_proto::SenderIdentityAssurance::DisplayOnly,
            None,
            Some("Alice"),
            "unverified Alice",
        ),
        (
            tau_proto::SenderIdentityAssurance::Unknown,
            None,
            None,
            "unverified external sender",
        ),
        (
            tau_proto::SenderIdentityAssurance::AuthenticatedTauAgent,
            Some("agent-a"),
            None,
            "agent-a",
        ),
    ];
    for (identity, stable_id, display_name, expected) in cases {
        envelope.trust.identity = identity;
        envelope.source = tau_proto::MessageEndpoint::External {
            stable_id: stable_id.map(str::to_owned),
            display_name: display_name.map(str::to_owned),
            actor_kind: tau_proto::ExternalActorKind::Human,
        };
        let item = super::message_envelope_item(tau_proto::MessageDirection::Incoming, &envelope);
        assert_eq!(item.model_presentation.source_label, expected);
    }
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
                        raw_arguments_json: None,
                        responses_envelope: None,
                    }),
                    ContextItem::ToolCall(ToolCallItem {
                        call_id: second_call_id.clone(),
                        name: ToolName::new("second_tool"),
                        tool_type: ToolType::Function,
                        arguments: tau_proto::CborValue::Null,
                        raw_arguments_json: None,
                        responses_envelope: None,
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
            &incoming_message(tree.agent_id.clone()),
        )
        .is_none(),
        "message must wait until the tool result preserves provider adjacency"
    );
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

    let final_node_id = tree
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
    let final_node = tree.node(final_node_id).expect("message node should exist");
    assert!(matches!(
        final_node.entry,
        AgentEntry::MessageEnvelope { .. }
    ));
    let tool_results_node = tree
        .node(final_node.parent_id.expect("message follows tool results"))
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
        tree.unresolved_foreground_tool_calls_from(Some(final_node_id))
            .is_empty()
    );
}

/// Ensures the validation refactor preserves the distinct diagnostic for
/// agent-scoped transcript events that target a different agent.
#[test]
fn validate_event_rejects_mismatched_transcript_agent_id() {
    let tree = AgentTree::from_events(agent_id(), &[]);

    assert_eq!(
        validation_error(&tree, prompt_event(other_agent_id())),
        "agent event agent_id did not match target agent"
    );
}

/// Ensures non-agent-transcript events keep the generic durable-store
/// diagnostic rather than being accepted by validation dispatch fallbacks.
#[test]
fn validate_event_rejects_non_agent_transcript_event() {
    let tree = AgentTree::from_events(agent_id(), &[]);

    assert_eq!(
        validation_error(
            &tree,
            Event::HarnessNotice(tau_proto::HarnessNotice::new(
                "test",
                "not an agent transcript event",
                tau_proto::NoticeLevel::Info,
            )),
        ),
        "agent store only persists agent transcript events"
    );
}

/// Ensures mismatched metadata preserves its historical generic diagnostic,
/// preventing later cleanup from accidentally treating it like transcript
/// agent-id mismatches.
#[test]
fn validate_event_rejects_mismatched_metadata_with_generic_diagnostic() {
    let tree = AgentTree::from_events(agent_id(), &[]);

    assert_eq!(
        validation_error(
            &tree,
            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                agent_id: other_agent_id(),
                key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
                value: tau_proto::CborValue::Text("/tmp".to_owned()),
                inheritable: true,
            }),
        ),
        "agent store only persists agent transcript events"
    );
}

/// Ensures both creation-time and update-time blank display names keep the
/// same rejection, because UIs rely on non-empty labels or id fallbacks.
#[test]
fn validate_event_rejects_blank_display_names() {
    let agent_id = agent_id();
    let tree = AgentTree::from_events(agent_id.clone(), &[]);

    assert_eq!(
        validation_error(
            &tree,
            Event::AgentStarted(tau_proto::AgentStarted {
                agent_id: agent_id.clone(),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: Some("   ".to_owned()),
                metadata: Vec::new(),
                ephemeral: false,
            }),
        ),
        "agent display name must not be empty"
    );
    assert_eq!(
        validation_error(
            &tree,
            Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
                agent_id,
                display_name: "\t".to_owned(),
            }),
        ),
        "agent display name must not be empty"
    );
}
