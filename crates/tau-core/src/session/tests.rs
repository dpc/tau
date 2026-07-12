use super::*;

fn agent_id() -> AgentId {
    AgentId::parse("agent-metadata-test").expect("valid test agent id")
}

fn other_agent_id() -> AgentId {
    AgentId::parse("other-agent").expect("valid test agent id")
}

fn prompt_event(agent_id: AgentId) -> Event {
    Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
        inference_activation: false,
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

fn manual_request(id: &str) -> tau_proto::AgentManualCompactionRequested {
    tau_proto::AgentManualCompactionRequested {
        request_id: tau_proto::CompactionRequestId::parse(id).expect("valid request id"),
        caller_agent_id: other_agent_id(),
        target_agent_id: agent_id(),
        initiating_agent_prompt_id: "ap-tool-round".into(),
        initiating_tool_call_id: "call-compact".into(),
        initiating_tool_name: tau_proto::ManualCompactionTool::AgentCompact,
        visible_tool_name: ToolName::new("agent_compact"),
        requested_target_head: AgentHead::Root,
        target_generation: 0,
        model: "provider/model".into(),
        resume_inference: false,
    }
}

fn compaction_start(id: &str) -> tau_proto::AgentStandaloneCompactionStarted {
    tau_proto::AgentStandaloneCompactionStarted {
        compact_prompt_id: "ap-agent-metadata-test-0".into(),
        operation: tau_proto::PromptOperation::StandaloneCompaction,
        agent_id: agent_id(),
        transaction_id: tau_proto::CompactionTransactionId::parse(id).expect("valid id"),
        cut: AgentHead::Root,
        resume_through: Some(AgentHead::Root),
        model: tau_proto::ModelId::from("provider/model"),
        originator: PromptOriginator::User,
        supersedes: None,
        trigger: tau_proto::StandaloneCompactionTrigger::Manual,
    }
}

/// A canonical planned overflow must project one unclaimed recovery and accept
/// exactly one model/cut-correlated standalone transaction claim.
#[test]
fn reactive_overflow_recovery_is_claimed_exactly_once() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let checkpoint = tau_proto::AgentInferenceDispatchStarted {
        agent_id: agent_id(),
        transaction_id: None,
        agent_prompt_id: "ap-overflow".into(),
        through: AgentHead::Root,
        model: Some("provider/model".into()),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: Some(AgentHead::Root),
    };
    tree.validate_event(&Event::AgentInferenceDispatchStarted(checkpoint.clone()))
        .expect("ordinary checkpoint is valid");
    tree.apply_event(&Event::AgentInferenceDispatchStarted(checkpoint.clone()));
    let response = tau_proto::ProviderResponseFinished {
        agent_prompt_id: checkpoint.agent_prompt_id.clone(),
        agent_id: agent_id(),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::Error,
        error: Some("bounded display error".to_owned()),
        failure_kind: Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
        recovery_disposition: tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned,
        originator: PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };
    tree.validate_event(&Event::ProviderResponseFinished(response.clone()))
        .expect("canonical planned rejection is valid");
    tree.apply_event(&Event::ProviderResponseFinished(response));
    assert_eq!(
        tree.inference_dispatch_recovery(),
        Some(InferenceDispatchRecovery::ContextRecoveryRequired(
            checkpoint.clone()
        ))
    );

    let mut started = compaction_start("ct-reactive");
    started.trigger = tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
        failed_agent_prompt_id: checkpoint.agent_prompt_id,
    };
    tree.validate_event(&Event::AgentStandaloneCompactionStarted(started.clone()))
        .expect("matching claim is valid");
    tree.apply_event(&Event::AgentStandaloneCompactionStarted(started.clone()));
    assert_eq!(
        tree.inference_dispatch_recovery(),
        Some(InferenceDispatchRecovery::CompletedThrough(AgentHead::Root))
    );
    let mut second_claim = started.clone();
    second_claim.transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-reactive-second").expect("valid id");
    second_claim.compact_prompt_id = "ap-agent-metadata-test-1".into();
    assert!(
        validation_error(&tree, Event::AgentStandaloneCompactionStarted(second_claim))
            .contains("uniquely match"),
        "a distinct transaction and prompt must not claim the same source rejection twice"
    );
}

/// Reactive claims must fail closed for unknown, unfinished, unplanned,
/// transaction-bound, wrong-operation, and mismatched immutable correlations.
#[test]
fn reactive_overflow_claim_rejects_invalid_source_correlations() {
    let base_checkpoint = tau_proto::AgentInferenceDispatchStarted {
        agent_id: agent_id(),
        transaction_id: None,
        agent_prompt_id: "ap-overflow-negative".into(),
        through: AgentHead::Root,
        model: Some("provider/model".into()),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: Some(AgentHead::Root),
    };
    let planned_response = tau_proto::ProviderResponseFinished {
        agent_prompt_id: base_checkpoint.agent_prompt_id.clone(),
        agent_id: agent_id(),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::Error,
        error: Some("bounded".to_owned()),
        failure_kind: Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
        recovery_disposition: tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned,
        originator: PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };
    let claim = |source: &str| {
        let mut started = compaction_start("ct-negative");
        started.trigger = tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
            failed_agent_prompt_id: source.into(),
        };
        started
    };

    let empty = AgentTree::from_events(agent_id(), &[]);
    assert!(
        validation_error(
            &empty,
            Event::AgentStandaloneCompactionStarted(claim("ap-unknown"))
        )
        .contains("unknown")
    );

    let mut unfinished = AgentTree::from_events(agent_id(), &[]);
    unfinished
        .validate_event(&Event::AgentInferenceDispatchStarted(
            base_checkpoint.clone(),
        ))
        .expect("checkpoint");
    unfinished.apply_event(&Event::AgentInferenceDispatchStarted(
        base_checkpoint.clone(),
    ));
    assert!(
        validation_error(
            &unfinished,
            Event::AgentStandaloneCompactionStarted(claim(
                base_checkpoint.agent_prompt_id.as_str()
            ))
        )
        .contains("uniquely match")
    );

    let mut unplanned = unfinished.clone();
    let mut ordinary_response = planned_response.clone();
    ordinary_response.recovery_disposition = tau_proto::ContextRecoveryDisposition::None;
    unplanned
        .validate_event(&Event::ProviderResponseFinished(ordinary_response.clone()))
        .expect("ordinary terminal response");
    unplanned.apply_event(&Event::ProviderResponseFinished(ordinary_response));
    assert!(
        validation_error(
            &unplanned,
            Event::AgentStandaloneCompactionStarted(claim(
                base_checkpoint.agent_prompt_id.as_str()
            ))
        )
        .contains("uniquely match")
    );

    let mismatches = [
        ("model", {
            let mut value = claim(base_checkpoint.agent_prompt_id.as_str());
            value.model = "provider/other".into();
            value
        }),
        ("cut", {
            let mut value = claim(base_checkpoint.agent_prompt_id.as_str());
            value.cut = AgentHead::Node(NodeId::new(42));
            value
        }),
        ("resume", {
            let mut value = claim(base_checkpoint.agent_prompt_id.as_str());
            value.resume_through = None;
            value
        }),
    ];
    for (name, mismatched) in mismatches {
        let mut tree = unfinished.clone();
        tree.validate_event(&Event::ProviderResponseFinished(planned_response.clone()))
            .expect("planned response");
        tree.apply_event(&Event::ProviderResponseFinished(planned_response.clone()));
        assert!(
            validation_error(&tree, Event::AgentStandaloneCompactionStarted(mismatched))
                .contains("uniquely match"),
            "{name} mismatch"
        );
    }

    for (name, mutate) in [("transaction-bound", 0_u8), ("wrong-operation", 1_u8)] {
        let mut checkpoint = base_checkpoint.clone();
        checkpoint.agent_prompt_id = format!("ap-{name}").into();
        if mutate == 0 {
            checkpoint.transaction_id =
                Some(tau_proto::CompactionTransactionId::parse("ct-source").expect("id"));
        } else {
            checkpoint.operation = Some(tau_proto::PromptOperation::StandaloneCompaction);
        }
        let mut tree = AgentTree::from_events(agent_id(), &[]);
        tree.inference_dispatches.insert(
            checkpoint.agent_prompt_id.clone(),
            InferenceDispatchFold {
                checkpoint: checkpoint.clone(),
                finished: true,
                recovery_disposition:
                    tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned,
            },
        );
        assert!(
            validation_error(
                &tree,
                Event::AgentStandaloneCompactionStarted(claim(checkpoint.agent_prompt_id.as_str()))
            )
            .contains("uniquely match"),
            "{name}"
        );
    }
}

/// Transaction ids and terminal outcomes are durable uniqueness boundaries;
/// replay must reject duplicates rather than silently replacing folded state.
#[test]
fn compaction_fold_rejects_duplicate_start_and_outcome() {
    let started = compaction_start("ct-one");
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    tree.validate_event(&Event::AgentStandaloneCompactionStarted(started.clone()))
        .expect("first start is valid");
    tree.apply_event(&Event::AgentStandaloneCompactionStarted(started.clone()));
    assert!(
        validation_error(
            &tree,
            Event::AgentStandaloneCompactionStarted(started.clone())
        )
        .contains("duplicate")
    );

    let failed = tau_proto::AgentStandaloneCompactionFailed {
        agent_id: agent_id(),
        transaction_id: started.transaction_id,
        cut: AgentHead::Root,
        reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
        resume_through: Some(AgentHead::Root),
    };
    tree.validate_event(&Event::AgentStandaloneCompactionFailed(failed.clone()))
        .expect("first outcome is valid");
    tree.apply_event(&Event::AgentStandaloneCompactionFailed(failed.clone()));
    assert!(
        validation_error(&tree, Event::AgentStandaloneCompactionFailed(failed))
            .contains("duplicate outcome")
    );
}

/// Checkpoints may acknowledge only one validated successful transaction and
/// must not be accepted before its compact outcome exists.
#[test]
fn compaction_fold_rejects_premature_and_unknown_checkpoints() {
    let started = compaction_start("ct-checkpoint");
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let mut wrong_operation = started.clone();
    wrong_operation.operation = tau_proto::PromptOperation::Inference;
    assert!(
        validation_error(
            &tree,
            Event::AgentStandaloneCompactionStarted(wrong_operation)
        )
        .contains("non-standalone")
    );
    tree.validate_event(&Event::AgentStandaloneCompactionStarted(started.clone()))
        .expect("start is valid");
    tree.apply_event(&Event::AgentStandaloneCompactionStarted(started.clone()));
    let checkpoint = tau_proto::AgentInferenceDispatchStarted {
        agent_id: agent_id(),
        transaction_id: Some(started.transaction_id),
        agent_prompt_id: "ap-agent-metadata-test-1".into(),
        through: AgentHead::Root,
        model: None,
        operation: None,
        activation_cut: None,
    };
    assert!(
        validation_error(
            &tree,
            Event::AgentInferenceDispatchStarted(checkpoint.clone())
        )
        .contains("requires one successful")
    );

    let unknown = tau_proto::AgentInferenceDispatchStarted {
        transaction_id: Some(tau_proto::CompactionTransactionId::parse("ct-unknown").expect("id")),
        ..checkpoint
    };
    assert!(
        validation_error(&tree, Event::AgentInferenceDispatchStarted(unknown))
            .contains("unknown compaction transaction")
    );
}

/// Explicit-parent validation must compare suffix_end with the selected branch
/// parent, not the tree's unrelated global write cursor.
#[test]
fn compaction_boundary_validates_explicit_parent() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    tree.apply_event(&prompt_event(agent_id()));
    let first = tree.head().expect("prompt node");
    tree.apply_event(&prompt_event(agent_id()));
    let started = tau_proto::AgentStandaloneCompactionStarted {
        cut: AgentHead::Node(first),
        resume_through: Some(AgentHead::Node(first)),
        ..compaction_start("ct-parent")
    };
    tree.validate_event_at(
        AgentEventParent::Under(first),
        &Event::AgentStandaloneCompactionStarted(started.clone()),
    )
    .expect("start on selected branch");
    tree.apply_event_at(
        AgentEventParent::Under(first),
        &Event::AgentStandaloneCompactionStarted(started.clone()),
    );
    let boundary = Event::AgentCompacted(tau_proto::AgentCompacted {
        compact_prompt_id: Some(started.compact_prompt_id.clone()),
        model: Some(started.model.clone()),
        operation: Some(started.operation),
        agent_id: agent_id(),
        transaction_id: Some(started.transaction_id),
        cut: Some(AgentHead::Node(first)),
        suffix_end: Some(AgentHead::Node(first)),
        replacement_window: vec![tau_proto::ContextItem::Message(tau_proto::MessageItem {
            role: tau_proto::ContextRole::User,
            content: vec![tau_proto::ContentPart::Text {
                text: "summary".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
    });
    tree.validate_event_at(AgentEventParent::Under(first), &boundary)
        .expect("explicit boundary parent, not global head, is authoritative");

    for case in 0..10 {
        let mut invalid = boundary.clone();
        let Event::AgentCompacted(compacted) = &mut invalid else {
            unreachable!()
        };
        match case {
            0 => compacted.transaction_id = None,
            1 => compacted.cut = None,
            2 => compacted.suffix_end = None,
            3 => compacted.compact_prompt_id = None,
            4 => compacted.model = None,
            5 => compacted.operation = None,
            6 => compacted.cut = Some(AgentHead::Root),
            7 => compacted.compact_prompt_id = Some("ap-wrong".into()),
            8 => compacted.model = Some("other/model".into()),
            9 => compacted.operation = Some(tau_proto::PromptOperation::Inference),
            _ => unreachable!(),
        }
        assert!(
            tree.validate_event_at(AgentEventParent::Under(first), &invalid)
                .is_err()
        );
    }

    let mut unknown = boundary.clone();
    let Event::AgentCompacted(compacted) = &mut unknown else {
        unreachable!()
    };
    compacted.transaction_id =
        Some(tau_proto::CompactionTransactionId::parse("ct-unknown").expect("transaction id"));
    assert!(
        tree.validate_event_at(AgentEventParent::Under(first), &unknown)
            .expect_err("unknown transaction must fail")
            .to_string()
            .contains("unknown")
    );

    tree.apply_event_at(AgentEventParent::Under(first), &boundary);
    assert!(
        tree.validate_event_at(AgentEventParent::Under(first), &boundary)
            .expect_err("duplicate successful boundary must fail")
            .to_string()
            .contains("duplicate outcome")
    );
}

/// Legacy all-absent compaction boundaries remain valid hard boundaries even
/// though they cannot participate in new transaction recovery.
#[test]
fn legacy_compaction_boundary_without_transaction_metadata_replays() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let boundary = Event::AgentCompacted(tau_proto::AgentCompacted {
        agent_id: agent_id(),
        transaction_id: None,
        cut: None,
        suffix_end: None,
        compact_prompt_id: None,
        model: None,
        operation: None,
        replacement_window: vec![tau_proto::ContextItem::Message(tau_proto::MessageItem {
            role: tau_proto::ContextRole::Assistant,
            content: vec![tau_proto::ContentPart::Text {
                text: "legacy summary".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
    });

    tree.validate_event(&boundary)
        .expect("legacy all-absent boundary");
    tree.apply_event(&boundary);
    assert!(matches!(
        tree.current_branch().last(),
        Some(AgentEntry::Compaction { .. })
    ));
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
            watch_provider_status: None,
            message: String::new(),
        });
        assert!(
            validation_error(&tree, event).contains("payload must be present exactly"),
            "mismatched discriminator and payload must fail closed"
        );
    }

    let provider_payload = tau_proto::AgentWatchProviderStatusNotification {
        session_id: "session-1".into(),
        subscription_id: "watch-1".to_owned(),
        turn_generation: 1,
        agent_prompt_id: "sp-watch".into(),
        state: tau_proto::AgentWatchProviderState::Retrying {
            category: tau_proto::AgentWatchProviderCategory::Transport,
            attempt: 1,
            next_retry_delay_secs: 2,
        },
        initial: false,
    };
    for (kind, watch_provider_status) in [
        (AgentMessageKind::WatchProviderStatus, None),
        (AgentMessageKind::Message, Some(provider_payload)),
    ] {
        let event = Event::AgentMessageReceived(AgentMessageReceived {
            message_id: "msg-invalid-watch-provider-status".into(),
            sender_id: other_agent_id(),
            sender_session_id: None,
            recipient_id: id.clone(),
            kind,
            watch_turn_state: None,
            watch_provider_status,
            message: String::new(),
        });
        assert!(
            validation_error(&tree, event).contains("payload must be present exactly"),
            "provider-status payloads must match their discriminator"
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
                failure_kind: None,
                recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
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

/// Ensures an accepted manual request is durable before start and exactly one
/// matching transaction can claim it after replay.
#[test]
fn manual_compaction_request_is_durable_and_uniquely_claimed() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let request = manual_request("cr-1");
    tree.validate_event(&Event::AgentManualCompactionRequested(request.clone()))
        .expect("request is valid");
    tree.apply_event(&Event::AgentManualCompactionRequested(request.clone()));
    assert_eq!(
        tree.manual_compaction_recoveries(),
        vec![ManualCompactionRecovery::Waiting(request.clone())]
    );

    let mut started = compaction_start("ct-manual-tool");
    started.resume_through = None;
    started.trigger = tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
        request_id: request.request_id.clone(),
        caller_agent_id: request.caller_agent_id.clone(),
        initiating_tool_call_id: request.initiating_tool_call_id.clone(),
    };
    tree.validate_event(&Event::AgentStandaloneCompactionStarted(started.clone()))
        .expect("matching transaction claim is valid");
    tree.apply_event(&Event::AgentStandaloneCompactionStarted(started.clone()));
    assert_eq!(
        tree.manual_compaction_recoveries(),
        vec![ManualCompactionRecovery::Started {
            requested: request,
            started: Box::new(started.clone()),
            outcome: None,
        }]
    );

    let mut duplicate = started;
    duplicate.transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-manual-tool-2").expect("valid id");
    assert!(
        validation_error(&tree, Event::AgentStandaloneCompactionStarted(duplicate))
            .contains("uniquely match")
    );
}

/// Ensures a pre-start failure is terminal and cannot race a later start or a
/// second terminal fact for the same accepted request.
#[test]
fn manual_compaction_pre_start_failure_is_exactly_once() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let request = manual_request("cr-failed");
    tree.validate_event(&Event::AgentManualCompactionRequested(request.clone()))
        .expect("request is valid");
    tree.apply_event(&Event::AgentManualCompactionRequested(request.clone()));
    let failed = tau_proto::AgentManualCompactionRequestFailed {
        request_id: request.request_id.clone(),
        target_agent_id: request.target_agent_id.clone(),
        reason: tau_proto::ManualCompactionRequestFailureReason::Cancelled,
    };
    tree.validate_event(&Event::AgentManualCompactionRequestFailed(failed.clone()))
        .expect("first failure is valid");
    tree.apply_event(&Event::AgentManualCompactionRequestFailed(failed.clone()));
    assert_eq!(
        tree.manual_compaction_recoveries(),
        vec![ManualCompactionRecovery::Failed {
            requested: request.clone(),
            failed: failed.clone(),
        }]
    );
    assert!(
        validation_error(&tree, Event::AgentManualCompactionRequestFailed(failed))
            .contains("terminal")
    );

    let mut started = compaction_start("ct-too-late");
    started.resume_through = None;
    started.trigger = tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
        request_id: request.request_id,
        caller_agent_id: request.caller_agent_id,
        initiating_tool_call_id: request.initiating_tool_call_id,
    };
    assert!(
        validation_error(&tree, Event::AgentStandaloneCompactionStarted(started))
            .contains("uniquely match")
    );
}

/// Standalone compaction provider prompts must not advance the target-owned
/// ordinary-inference generation used by the manual compaction rate guard.
#[test]
fn manual_compaction_generation_excludes_standalone_prompts() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let prompt = |id: &str, operation| {
        Event::AgentPromptCreated(tau_proto::AgentPromptCreated {
            agent_prompt_id: id.into(),
            agent_id: agent_id(),
            session_id: "session".into(),
            system_prompt: String::new(),
            context: tau_proto::PromptContext::default(),
            tools: Vec::new(),
            tools_ref: None,
            model: "provider/model".into(),
            model_params: Default::default(),
            tool_choice: Default::default(),
            originator: PromptOriginator::User,
            share_user_cache_key: false,
            ctx_id: None,
            compaction: None,
            operation,
        })
    };
    tree.apply_event(&prompt(
        "ap-compact",
        tau_proto::PromptOperation::StandaloneCompaction,
    ));
    assert_eq!(tree.ordinary_inference_generation(), 0);
    tree.apply_event(&prompt(
        "ap-inference",
        tau_proto::PromptOperation::Inference,
    ));
    assert_eq!(tree.ordinary_inference_generation(), 1);
}

/// Manual transaction claims fail closed when any immutable request
/// correlation is unknown or changed.
#[test]
fn manual_compaction_claim_rejects_correlation_mismatches() {
    let request = manual_request("cr-correlated");
    let mut base = AgentTree::from_events(agent_id(), &[]);
    base.validate_event(&Event::AgentManualCompactionRequested(request.clone()))
        .expect("request");
    base.apply_event(&Event::AgentManualCompactionRequested(request.clone()));
    let matching = || {
        let mut started = compaction_start("ct-correlated");
        started.resume_through = None;
        started.trigger = tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
            request_id: request.request_id.clone(),
            caller_agent_id: request.caller_agent_id.clone(),
            initiating_tool_call_id: request.initiating_tool_call_id.clone(),
        };
        started
    };

    let mut unknown = matching();
    if let tau_proto::StandaloneCompactionTrigger::ManualAgentTool { request_id, .. } =
        &mut unknown.trigger
    {
        *request_id = tau_proto::CompactionRequestId::parse("cr-unknown").expect("request id");
    }
    assert!(
        validation_error(&base, Event::AgentStandaloneCompactionStarted(unknown))
            .contains("unknown request")
    );

    for mutation in ["caller", "call", "model"] {
        let mut started = matching();
        match mutation {
            "caller" => {
                if let tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
                    caller_agent_id,
                    ..
                } = &mut started.trigger
                {
                    *caller_agent_id = agent_id();
                }
            }
            "call" => {
                if let tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
                    initiating_tool_call_id,
                    ..
                } = &mut started.trigger
                {
                    *initiating_tool_call_id = "call-other".into();
                }
            }
            "model" => started.model = "provider/other".into(),
            _ => unreachable!(),
        }
        assert!(
            validation_error(&base, Event::AgentStandaloneCompactionStarted(started))
                .contains("uniquely match"),
            "{mutation}"
        );
    }
}

/// Branch-specific notification lookup must not accept matching text from the
/// tree's global cursor when the caller conversation points at another branch.
#[test]
fn manual_completion_notification_lookup_is_branch_specific() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let input = |text: &str| {
        Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
            agent_id: agent_id(),
            text: text.to_owned(),
            message_class: tau_proto::PromptMessageClass::Internal,
            inference_activation: false,
            ctx_id: None,
        })
    };
    tree.apply_event(&input("caller notification"));
    let caller_head = tree.head();
    tree.apply_event(&Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
        agent_id: agent_id(),
        head: AgentHead::Root,
    }));
    tree.apply_event(&input("other branch notification"));

    assert!(tree.has_user_input_text_on_branch(caller_head, "caller notification"));
    assert!(!tree.has_user_input_text_on_branch(caller_head, "other branch notification"));
    assert!(tree.has_user_input_text_on_branch(tree.head(), "other branch notification"));
}
