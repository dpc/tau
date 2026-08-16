use std::io as path_std_io;

use super::*;

/// Stable extension provenance never becomes a run-local route, including when
/// its spelling collides with a live connection identifier.
#[test]
fn persisted_extension_source_never_projects_to_connection_route() {
    let colliding = "same-spelling";
    let connection = PersistedEventSource::Connection(
        tau_proto::ConnectionId::parse(colliding).expect("connection id"),
    );

    for extension_name in [colliding, "different-spelling"] {
        let extension = PersistedEventSource::Extension(
            tau_proto::ExtensionName::parse(extension_name).expect("extension name"),
        );
        assert_eq!(extension.connection_id(), None);
    }
    assert_eq!(
        connection
            .connection_id()
            .map(tau_proto::ConnectionId::as_str),
        Some(colliding)
    );
}

/// Persisted provenance keeps its externally tagged JSON/CBOR shape for both
/// source domains.
#[test]
fn persisted_event_source_wire_shapes_round_trip() {
    let cases = [
        (
            PersistedEventSource::Connection(
                tau_proto::ConnectionId::parse("route-1").expect("connection id"),
            ),
            serde_json::json!({"connection": "route-1"}),
            ("connection", "route-1"),
        ),
        (
            PersistedEventSource::Extension(
                tau_proto::ExtensionName::parse("publisher-1").expect("extension name"),
            ),
            serde_json::json!({"extension": "publisher-1"}),
            ("extension", "publisher-1"),
        ),
    ];

    for (source, expected_json, (tag, value)) in cases {
        assert_eq!(
            serde_json::to_value(&source).expect("serialize source"),
            expected_json
        );
        assert_eq!(
            serde_json::from_value::<PersistedEventSource>(expected_json)
                .expect("deserialize source"),
            source
        );
        let mut cbor = Vec::new();
        ciborium::into_writer(&source, &mut cbor).expect("serialize source as CBOR");
        assert_eq!(
            ciborium::from_reader::<ciborium::Value, _>(cbor.as_slice())
                .expect("decode source CBOR shape"),
            ciborium::Value::Map(vec![(
                ciborium::Value::Text(tag.to_owned()),
                ciborium::Value::Text(value.to_owned()),
            )])
        );
        assert_eq!(
            ciborium::from_reader::<PersistedEventSource, _>(cbor.as_slice())
                .expect("deserialize source from CBOR"),
            source
        );
    }
}

/// Historical CBOR records without the private marker decode as Legacy.
#[test]
fn persisted_agent_event_missing_fold_semantics_defaults_to_legacy() {
    let agent_id = agent_id();
    let record = PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([7; 16]),
        seq: PersistedAgentEventSeq::new(0),
        source: None,
        event: Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id,
            head: AgentHead::Root,
        }),
        parent: AgentEventParent::InheritHead,
        fold_semantics: AgentJournalFoldSemantics::InferenceDeferredInputV1,
        recorded_at: tau_proto::UnixMicros::new(1),
    };
    let mut encoded = Vec::new();
    ciborium::into_writer(&record, &mut encoded).expect("encode record");
    let mut value =
        ciborium::from_reader::<ciborium::Value, _>(encoded.as_slice()).expect("decode value");
    let ciborium::Value::Map(fields) = &mut value else {
        panic!("record must encode as map");
    };
    fields.retain(|(key, _)| key != &ciborium::Value::Text("fold_semantics".to_owned()));
    encoded.clear();
    ciborium::into_writer(&value, &mut encoded).expect("encode historical shape");
    let decoded = ciborium::from_reader::<PersistedAgentEvent, _>(encoded.as_slice())
        .expect("decode historical record");
    assert_eq!(decoded.fold_semantics, AgentJournalFoldSemantics::Legacy);
}

/// Applies one contiguous durable record through the sole canonical fold path.
fn apply_persisted_test_record(
    tree: &mut AgentTree,
    parent: AgentEventParent,
    event: Event,
) -> (PersistedAgentEventSeq, Option<NodeId>) {
    let seq = tree.next_event_seq;
    let node = tree
        .apply_persisted_record(&PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
            seq,
            source: None,
            event,
            parent,
            fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
            recorded_at: tau_proto::UnixMicros::default(),
        })
        .expect("test record is contiguous and valid");
    (seq, node)
}

/// Observation-only events must survive validation without mutating replayed
/// transcript or runtime state.
#[test]
fn content_free_tool_observations_are_valid_replay_no_ops() {
    use tau_proto::{
        ActivationKind, AgentActivationQueued, AgentToolBackgroundedObserved,
        AgentToolCancellationRequested, AgentToolDispatchObserved, AgentToolTerminalClassified,
        AgentToolWaitObserved, AgentToolWaitRegistered, AgentToolWaitSettled, ObservationId,
        ToolCallRef, ToolTerminalCause, ToolWaitMode, ToolWaitOutcome, WaitRejectionReason,
    };

    let agent_id = tau_proto::AgentId::parse("agent_0").expect("agent id");
    let mut tree = AgentTree::from_events(agent_id, &[]);
    let id = |byte| ObservationId::from_bytes([byte; 16]);
    let call = ToolCallRef {
        declaration: id(1),
        item_index: 2,
    };
    let events = [
        Event::AgentToolDispatchObserved(AgentToolDispatchObserved { call }),
        Event::AgentToolBackgroundedObserved(AgentToolBackgroundedObserved { call }),
        Event::AgentToolWaitObserved(AgentToolWaitObserved {
            wait_call: call,
            mode: ToolWaitMode::NextBackground,
        }),
        Event::AgentToolWaitRegistered(AgentToolWaitRegistered {
            wait_observation: id(3),
            wait_call: call,
            mode: ToolWaitMode::NextBackground,
        }),
        Event::AgentActivationQueued(AgentActivationQueued {
            kind: ActivationKind::AgentMessage,
            source_observation: Some(id(3)),
            source_call: None,
        }),
        Event::AgentToolWaitSettled(AgentToolWaitSettled {
            wait_observation: id(3),
            wait_call: call,
            registration: Some(id(4)),
            wait_terminal: id(5),
            outcome: ToolWaitOutcome::Rejected {
                reason: WaitRejectionReason::NoBackgroundCandidate,
            },
        }),
        Event::AgentToolCancellationRequested(AgentToolCancellationRequested {
            cancel_call: call,
            target_call: ToolCallRef {
                declaration: id(6),
                item_index: 7,
            },
        }),
        Event::AgentToolTerminalClassified(AgentToolTerminalClassified {
            call,
            terminal: id(8),
            cause: ToolTerminalCause::Completed,
        }),
    ];

    for event in events {
        tree.validate_event(&event).expect("observation is valid");
        let prior_head = tree.head();
        assert_eq!(
            apply_persisted_test_record(&mut tree, AgentEventParent::InheritHead, event).1,
            None
        );
        assert_eq!(tree.head(), prior_head);
    }
}

/// A crash-cut unmatched start remains durable, while a different harness
/// runtime may start and finish a fresh turn without permitting same-runtime
/// overlap.
#[test]
fn outer_turn_fold_distinguishes_crash_recovery_from_runtime_overlap() {
    let agent_id = tau_proto::AgentId::parse("agent_0").expect("agent id");
    let mut tree = AgentTree::from_events(agent_id.clone(), &[]);
    apply_persisted_test_record(
        &mut tree,
        AgentEventParent::InheritHead,
        Event::AgentStarted(tau_proto::AgentStarted {
            agent_id: agent_id.clone(),
            creator: Some(tau_proto::AgentCreator::User),
            parent_agent: None,
            role: "engineer".to_owned(),
            display_name: None,
            metadata: Vec::new(),
            ephemeral: false,
        }),
    );
    let start = |id: &str, runtime: &str| {
        let prompt_id: tau_proto::AgentPromptId = id
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid");
        Event::AgentOuterTurnStarted(tau_proto::AgentOuterTurnStarted {
            agent_id: agent_id.clone(),
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            outer_turn_id: tau_proto::AgentOuterTurnId::for_prompt(&prompt_id),
            agent_prompt_id: prompt_id,
            runtime_id: tau_proto::AccountingRuntimeId::parse(runtime)
                .expect("test identifier must be valid"),
            activation: tau_proto::AgentOuterTurnActivation::External {
                correlation_id: tau_proto::AgentActivationCorrelationId::parse(id)
                    .expect("test identifier must be valid"),
            },
        })
    };
    let checkpoint = |id: &str| {
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_id.clone(),
            transaction_id: None,
            agent_prompt_id: id
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            through: tau_proto::AgentHead::Root,
            model: Some("test/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(tau_proto::AgentHead::Root),
        })
    };
    apply_persisted_test_record(
        &mut tree,
        AgentEventParent::InheritHead,
        checkpoint("ap-first"),
    );
    apply_persisted_test_record(
        &mut tree,
        AgentEventParent::InheritHead,
        start("ap-first", "runtime-1"),
    );
    assert!(
        tree.validate_event_at(
            AgentEventParent::InheritHead,
            &start("ap-overlap", "runtime-1")
        )
        .is_err()
    );
    apply_persisted_test_record(
        &mut tree,
        AgentEventParent::InheritHead,
        checkpoint("ap-overlap"),
    );
    assert!(
        tree.validate_event_at(
            AgentEventParent::InheritHead,
            &start("ap-overlap", "runtime-1")
        )
        .is_err()
    );
    apply_persisted_test_record(
        &mut tree,
        AgentEventParent::InheritHead,
        checkpoint("ap-second"),
    );
    apply_persisted_test_record(
        &mut tree,
        AgentEventParent::InheritHead,
        start("ap-second", "runtime-2"),
    );
    apply_persisted_test_record(
        &mut tree,
        AgentEventParent::InheritHead,
        Event::AgentOuterTurnFinished(tau_proto::AgentOuterTurnFinished {
            agent_id,
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            outer_turn_id: tau_proto::AgentOuterTurnId::for_prompt(
                &"ap-second"
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
            ),
            disposition: tau_proto::AgentOuterTurnDisposition::Settled,
        }),
    );
    assert!(
        !tree.outer_turn_is_open(&tau_proto::AgentOuterTurnId::for_prompt(
            &"ap-second"
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid")
        ))
    );
}

/// Ensures extension-supplied typed image metadata cannot bypass the durable
/// provider-content byte/type validation boundary.
#[test]
fn provider_image_content_rejects_mismatched_media_bytes() {
    let result = tau_proto::ToolResult {
        presentation: Default::default(),
        call_id: "call-image".into(),
        tool_name: tau_proto::ToolName::new("read_image"),
        tool_type: tau_proto::ToolType::Function,
        result: tau_proto::CborValue::Text("image metadata".to_owned()),
        provider_content: vec![tau_proto::ToolResultContentPart::Image(
            tau_proto::ImageContent {
                media_type: tau_proto::ImageMediaType::Png,
                data: b"not a PNG".to_vec().into(),
                width: 1,
                height: 1,
                detail: tau_proto::ImageDetail::High,
            },
        )],
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    };

    assert!(validate_tool_result_provider_content(&result).is_err());
}

/// Ensures durable validation independently decodes typed bytes and rejects
/// extension-supplied dimensions that do not describe the canonical image.
#[test]
fn provider_image_content_rejects_false_decoded_dimensions() {
    let source = image::DynamicImage::new_rgba8(1, 1);
    let mut encoded = path_std_io::Cursor::new(Vec::new());
    source
        .write_to(&mut encoded, image::ImageFormat::Png)
        .expect("encode fixture");
    let content = vec![tau_proto::ToolResultContentPart::Image(
        tau_proto::ImageContent {
            media_type: tau_proto::ImageMediaType::Png,
            data: encoded.into_inner().into(),
            width: 2,
            height: 1,
            detail: tau_proto::ImageDetail::High,
        },
    )];

    let error = validate_provider_content_parts(tau_proto::ToolType::Function, &content)
        .expect_err("false dimensions must fail");
    assert!(error.to_string().contains("do not match"));
}

/// Ensures magic bytes alone cannot make a truncated image durable.
#[test]
fn provider_image_content_rejects_truncated_encoded_bytes() {
    let content = vec![tau_proto::ToolResultContentPart::Image(
        tau_proto::ImageContent {
            media_type: tau_proto::ImageMediaType::Png,
            data: b"\x89PNG\r\n\x1a\ntruncated".to_vec().into(),
            width: 1,
            height: 1,
            detail: tau_proto::ImageDetail::High,
        },
    )];

    assert!(validate_provider_content_parts(tau_proto::ToolType::Function, &content).is_err());
}

/// Ensures canonical media retained across an agent's complete append-only
/// history cannot grow past the aggregate logical-byte quota.
#[test]
fn provider_image_content_rejects_per_agent_aggregate_overflow() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    tree.retained_provider_image_bytes = MAX_RETAINED_PROVIDER_IMAGE_BYTES_PER_AGENT;
    let result = tau_proto::ToolResult {
        presentation: Default::default(),
        call_id: "call-image".into(),
        tool_name: tau_proto::ToolName::new("read_image"),
        tool_type: tau_proto::ToolType::Function,
        result: tau_proto::CborValue::Text("image metadata".to_owned()),
        provider_content: vec![tau_proto::ToolResultContentPart::Image(
            tau_proto::ImageContent {
                media_type: tau_proto::ImageMediaType::Png,
                data: b"\x89PNG\r\n\x1a\nfixture".to_vec().into(),
                width: 1,
                height: 1,
                detail: tau_proto::ImageDetail::High,
            },
        )],
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    };

    let error = tree
        .validate_event(&Event::ProviderToolResult(result))
        .expect_err("aggregate image quota must reject the event");
    assert!(error.to_string().contains("per-agent limit"));
}

/// Ensures provider output cannot inject input-side tool results and thereby
/// bypass the dedicated result validation, accounting, and call-pairing path.
#[test]
fn provider_response_rejects_input_side_tool_result_items() {
    let tree = AgentTree::from_events(agent_id(), &[]);
    let response = tau_proto::ProviderResponseFinished {
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: "ap-invalid-result"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: agent_id(),
        output_items: vec![ContextItem::ToolResult(tau_proto::ToolResultItem {
            presentation: Default::default(),
            call_id: "call-image".into(),
            tool_type: tau_proto::ToolType::Function,
            status: tau_proto::ToolResultStatus::Success,
            output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
                "invalid provider result".to_owned(),
            )),
            provider_content: Vec::new(),
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };

    let error = tree
        .validate_event(&Event::ProviderResponseFinished(response))
        .expect_err("input-side tool result must fail");
    assert!(error.to_string().contains("input-side tool result"));
}

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
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    })
}

/// Durable span provenance splits provider content without treating its source
/// label or delimiter-shaped text as authority, and rejects malformed byte
/// ranges before folding.
#[test]
fn trusted_internal_prompt_spans_are_typed_utf8_ranges() {
    let text = "bootstrap\n\n雪 user task";
    let mut event = prompt_event(agent_id());
    let Event::AgentPromptSubmitted(prompt) = &mut event else {
        unreachable!()
    };
    prompt.text = text.to_owned();
    prompt.trusted_internal_spans = vec![tau_proto::TrustedInternalSpan { start: 0, end: 11 }];

    let mut tree = AgentTree::from_events(agent_id(), &[]);
    tree.validate_event(&event).expect("valid span");
    tree.apply_event(&event);
    assert!(matches!(
        &tree.nodes()[0].entry,
        AgentEntry::UserInput { items, .. }
            if matches!(
                items.as_slice(),
                [ContextItem::Message(MessageItem { content, .. })]
                    if matches!(
                        content.as_slice(),
                        [
                            ContentPart::HarnessInternalText { text: prefix },
                            ContentPart::Text { text: task },
                        ] if prefix == "bootstrap\n\n" && task == "雪 user task"
                    )
            )
    ));

    let mut invalid = event;
    let Event::AgentPromptSubmitted(prompt) = &mut invalid else {
        unreachable!()
    };
    prompt.trusted_internal_spans = vec![tau_proto::TrustedInternalSpan { start: 12, end: 13 }];
    assert!(
        validation_error(&tree, invalid).contains("trusted internal prompt spans"),
        "ranges must begin and end on UTF-8 boundaries"
    );
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
        initiating_agent_prompt_id: "ap-tool-round"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
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
        compact_prompt_id: "ap-agent-metadata-test-0"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
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

fn append_user_input(tree: &mut AgentTree, text: &str) -> AgentHead {
    tree.apply_event(&Event::AgentPromptSubmitted(
        tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id(),
            text: text.to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        },
    ));
    AgentHead::Node(tree.head().expect("input node"))
}

fn fail_compaction(tree: &mut AgentTree, started: &tau_proto::AgentStandaloneCompactionStarted) {
    tree.apply_event(&Event::AgentStandaloneCompactionStarted(started.clone()));
    tree.apply_event(&Event::AgentStandaloneCompactionFailed(
        tau_proto::AgentStandaloneCompactionFailed {
            agent_id: agent_id(),
            transaction_id: started.transaction_id.clone(),
            cut: started.cut,
            reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
            resume_through: started.resume_through,
        },
    ));
}

/// Provider-prefix normalization must keep function, custom, and mixed parallel
/// tool rounds indivisible while leaving every already-closed boundary intact.
#[test]
fn closed_provider_prefix_retreats_only_from_tool_calling_assistant() {
    for tool_types in [
        vec![ToolType::Function],
        vec![ToolType::Custom],
        vec![ToolType::Function, ToolType::Custom],
    ] {
        let mut tree = AgentTree::from_events(agent_id(), &[]);
        let parent = append_user_input(&mut tree, "parent");
        let response = tau_proto::ProviderResponseFinished {
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: "ap-tool-prefix"
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            agent_id: agent_id(),
            output_items: tool_types
                .iter()
                .enumerate()
                .map(|(index, tool_type)| {
                    ContextItem::ToolCall(ToolCallItem {
                        call_id: format!("call-{index}").into(),
                        name: ToolName::new(format!("tool_{index}")),
                        tool_type: *tool_type,
                        arguments: tau_proto::CborValue::Null,
                        raw_arguments_json: None,
                        responses_envelope: None,
                    })
                })
                .collect(),
            stop_reason: tau_proto::ProviderStopReason::ToolCalls,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            usage: None,
            originator: PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        };
        tree.apply_event(&Event::ProviderResponseFinished(response));
        let assistant = AgentHead::Node(tree.head().expect("assistant node"));
        assert_eq!(tree.closed_provider_prefix_at_or_before(assistant), parent);
        assert_eq!(
            tree.closed_provider_prefix_at_or_before(parent),
            parent,
            "ordinary input is already a closed prefix"
        );
        assert_eq!(
            tree.closed_provider_prefix_at_or_before(AgentHead::Root),
            AgentHead::Root
        );
        for (index, tool_type) in tool_types.iter().enumerate() {
            tree.apply_event(&Event::ProviderToolResult(tau_proto::ToolResult {
                presentation: Default::default(),
                call_id: format!("call-{index}").into(),
                tool_name: ToolName::new(format!("tool_{index}")),
                tool_type: *tool_type,
                result: tau_proto::CborValue::Text(format!("result {index}")),
                provider_content: Vec::new(),
                kind: ToolResultKind::Final,
                display: None,
                originator: PromptOriginator::User,
            }));
        }
        let results = AgentHead::Node(tree.head().expect("whole results node"));
        assert_eq!(
            tree.closed_provider_prefix_at_or_before(results),
            results,
            "the whole results node is already closed"
        );
        tree.apply_event(&Event::AgentCompacted(tau_proto::AgentCompacted {
            agent_id: agent_id(),
            transaction_id: None,
            cut: None,
            suffix_end: None,
            compact_prompt_id: None,
            model: None,
            operation: None,
            replacement_window: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "compacted".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
        }));
        let compaction = AgentHead::Node(tree.head().expect("compaction node"));
        assert_eq!(
            tree.closed_provider_prefix_at_or_before(compaction),
            compaction,
            "a compaction boundary is already closed"
        );
    }

    let mut tree = AgentTree::from_events(agent_id(), &[]);
    tree.apply_event(&Event::ProviderResponseFinished(
        tau_proto::ProviderResponseFinished {
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: "ap-text-prefix"
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            agent_id: agent_id(),
            output_items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "closed".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
            stop_reason: tau_proto::ProviderStopReason::EndTurn,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            usage: None,
            originator: PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        },
    ));
    let text_response = AgentHead::Node(tree.head().expect("text response"));
    assert_eq!(
        tree.closed_provider_prefix_at_or_before(text_response),
        text_response
    );
}

/// Failed transaction successors may compact less history, but must never move
/// their cut forward, cross branches, or discard a retained resume obligation.
#[test]
fn superseding_compaction_allows_only_ancestor_cut_retreat() {
    let build_failed =
        |original_cut: AgentHead, resume: Option<AgentHead>, tree: &mut AgentTree| {
            let mut started = compaction_start("ct-failed-boundary");
            started.cut = original_cut;
            started.resume_through = resume;
            fail_compaction(tree, &started);
            started
        };

    let mut retreat_tree = AgentTree::from_events(agent_id(), &[]);
    let ancestor = append_user_input(&mut retreat_tree, "ancestor");
    let descendant = append_user_input(&mut retreat_tree, "descendant");
    let failed = build_failed(descendant, Some(descendant), &mut retreat_tree);
    let mut retreat = compaction_start("ct-retreat");
    retreat.cut = ancestor;
    retreat.resume_through = Some(descendant);
    retreat.supersedes = Some(failed.transaction_id);
    retreat_tree
        .validate_event(&Event::AgentStandaloneCompactionStarted(retreat))
        .expect("ancestor retreat preserves more exact suffix");

    let mut equal_tree = AgentTree::from_events(agent_id(), &[]);
    let equal_cut = append_user_input(&mut equal_tree, "equal");
    let failed = build_failed(equal_cut, Some(equal_cut), &mut equal_tree);
    let mut equal = compaction_start("ct-equal");
    equal.cut = equal_cut;
    equal.resume_through = Some(equal_cut);
    equal.supersedes = Some(failed.transaction_id);
    equal_tree
        .validate_event(&Event::AgentStandaloneCompactionStarted(equal))
        .expect("equal retry remains valid");

    let mut advance_tree = AgentTree::from_events(agent_id(), &[]);
    let original = append_user_input(&mut advance_tree, "original");
    let advanced = append_user_input(&mut advance_tree, "advanced");
    let failed = build_failed(original, Some(advanced), &mut advance_tree);
    let mut advanced_start = compaction_start("ct-advanced");
    advanced_start.cut = advanced;
    advanced_start.resume_through = Some(advanced);
    advanced_start.supersedes = Some(failed.transaction_id.clone());
    assert!(
        validation_error(
            &advance_tree,
            Event::AgentStandaloneCompactionStarted(advanced_start)
        )
        .contains("preserve or retreat")
    );

    let mut dropped = compaction_start("ct-dropped-resume");
    dropped.cut = original;
    dropped.resume_through = None;
    dropped.supersedes = Some(failed.transaction_id);
    assert!(
        validation_error(
            &advance_tree,
            Event::AgentStandaloneCompactionStarted(dropped)
        )
        .contains("preserve or retreat")
    );

    let mut sibling_tree = AgentTree::from_events(agent_id(), &[]);
    let branch_point = append_user_input(&mut sibling_tree, "branch point");
    let original_branch = append_user_input(&mut sibling_tree, "original branch");
    sibling_tree.apply_event(&Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
        agent_id: agent_id(),
        head: branch_point,
    }));
    let sibling = append_user_input(&mut sibling_tree, "sibling branch");
    let failed = build_failed(branch_point, Some(original_branch), &mut sibling_tree);
    let mut sibling_start = compaction_start("ct-sibling");
    sibling_start.cut = branch_point;
    sibling_start.resume_through = Some(sibling);
    sibling_start.supersedes = Some(failed.transaction_id);
    assert!(
        validation_error(
            &sibling_tree,
            Event::AgentStandaloneCompactionStarted(sibling_start)
        )
        .contains("preserve or retreat")
    );

    let unknown_tree = AgentTree::from_events(agent_id(), &[]);
    let mut unknown = compaction_start("ct-unknown-successor");
    unknown.supersedes = Some(tau_proto::CompactionTransactionId::parse("ct-unknown").expect("id"));
    assert!(
        validation_error(
            &unknown_tree,
            Event::AgentStandaloneCompactionStarted(unknown)
        )
        .contains("unknown transaction")
    );

    let mut successful_tree = AgentTree::from_events(agent_id(), &[]);
    let successful = compaction_start("ct-successful-predecessor");
    successful_tree.apply_event(&Event::AgentStandaloneCompactionStarted(successful.clone()));
    successful_tree.apply_event(&Event::AgentCompacted(tau_proto::AgentCompacted {
        agent_id: agent_id(),
        transaction_id: Some(successful.transaction_id.clone()),
        cut: Some(successful.cut),
        suffix_end: Some(AgentHead::Root),
        compact_prompt_id: Some(successful.compact_prompt_id),
        model: Some(successful.model),
        operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
        replacement_window: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "summary".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
    }));
    let mut supersedes_success = compaction_start("ct-after-success");
    supersedes_success.supersedes = Some(successful.transaction_id);
    assert!(
        validation_error(
            &successful_tree,
            Event::AgentStandaloneCompactionStarted(supersedes_success)
        )
        .contains("only a failed transaction")
    );
}

/// Replay of a historical failed open cut followed by a corrected successful
/// successor must transfer continuation ownership to the successor checkpoint
/// and clear blocked recovery after its terminal inference response.
#[test]
fn corrected_compaction_successor_owns_replay_checkpoint() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let prefix = append_user_input(&mut tree, "prefix");
    let historical_cut = append_user_input(&mut tree, "historical open cut");
    let resume = append_user_input(&mut tree, "owed activation");
    let mut failed = compaction_start("ct-historical-failed");
    failed.cut = historical_cut;
    failed.resume_through = Some(resume);
    fail_compaction(&mut tree, &failed);
    assert!(matches!(
        tree.standalone_compaction_recovery(),
        Some(StandaloneCompactionRecovery::Blocked { .. })
    ));

    let mut successor = compaction_start("ct-corrected-successor");
    successor.compact_prompt_id = "ap-agent-metadata-test-successor"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    successor.cut = prefix;
    successor.resume_through = Some(resume);
    successor.supersedes = Some(failed.transaction_id);
    tree.validate_event(&Event::AgentStandaloneCompactionStarted(successor.clone()))
        .expect("corrected ancestor successor");
    tree.apply_event(&Event::AgentStandaloneCompactionStarted(successor.clone()));
    let compacted = tau_proto::AgentCompacted {
        agent_id: agent_id(),
        transaction_id: Some(successor.transaction_id.clone()),
        cut: Some(successor.cut),
        suffix_end: Some(resume),
        compact_prompt_id: Some(successor.compact_prompt_id.clone()),
        model: Some(successor.model.clone()),
        operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
        replacement_window: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "corrected summary".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
    };
    tree.validate_event(&Event::AgentCompacted(compacted.clone()))
        .expect("corrected successor success");
    tree.apply_event(&Event::AgentCompacted(compacted));
    let through = AgentHead::Node(tree.head().expect("compaction boundary"));
    let checkpoint = tau_proto::AgentInferenceDispatchStarted {
        agent_id: agent_id(),
        transaction_id: Some(successor.transaction_id.clone()),
        agent_prompt_id: "ap-successor-continuation"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        through,
        model: Some(successor.model),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: Some(successor.cut),
    };
    tree.validate_event(&Event::AgentInferenceDispatchStarted(checkpoint.clone()))
        .expect("successor-owned checkpoint");
    tree.apply_event(&Event::AgentInferenceDispatchStarted(checkpoint.clone()));
    assert_eq!(
        tree.standalone_compaction_recovery(),
        Some(StandaloneCompactionRecovery::DispatchUncertain(
            checkpoint.clone()
        ))
    );
    tree.apply_event(&Event::ProviderResponseFinished(
        tau_proto::ProviderResponseFinished {
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: checkpoint.agent_prompt_id,
            agent_id: agent_id(),
            output_items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "continued".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
            stop_reason: tau_proto::ProviderStopReason::EndTurn,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            originator: PromptOriginator::User,
            usage: None,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        },
    ));
    assert_eq!(tree.standalone_compaction_recovery(), None);
}

/// A canonical planned overflow must project one unclaimed recovery and accept
/// exactly one model/cut-correlated standalone transaction claim.
#[test]
fn reactive_overflow_recovery_is_claimed_exactly_once() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let checkpoint = tau_proto::AgentInferenceDispatchStarted {
        agent_id: agent_id(),
        transaction_id: None,
        agent_prompt_id: "ap-overflow"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        through: AgentHead::Root,
        model: Some("provider/model".into()),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: Some(AgentHead::Root),
    };
    tree.validate_event(&Event::AgentInferenceDispatchStarted(checkpoint.clone()))
        .expect("ordinary checkpoint is valid");
    tree.apply_event(&Event::AgentInferenceDispatchStarted(checkpoint.clone()));
    let response = tau_proto::ProviderResponseFinished {
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: checkpoint.agent_prompt_id.clone(),
        agent_id: agent_id(),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::Error,
        error: Some("bounded display error".to_owned()),
        failure_kind: Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
        context_limit_telemetry: None,
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
    second_claim.compact_prompt_id = "ap-agent-metadata-test-1"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
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
        agent_prompt_id: "ap-overflow-negative"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        through: AgentHead::Root,
        model: Some("provider/model".into()),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: Some(AgentHead::Root),
    };
    let planned_response = tau_proto::ProviderResponseFinished {
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: base_checkpoint.agent_prompt_id.clone(),
        agent_id: agent_id(),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::Error,
        error: Some("bounded".to_owned()),
        failure_kind: Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
        context_limit_telemetry: None,
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
            failed_agent_prompt_id: source
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
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
        checkpoint.agent_prompt_id = format!("ap-{name}")
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid");
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
                head_move_generation: 0,
                fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
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
        agent_prompt_id: "ap-agent-metadata-test-1"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
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

/// A successful transaction accepts only the exact model, inference operation,
/// and activation cut owned by its durable start.
#[test]
fn compaction_checkpoint_rejects_ownership_mismatches() {
    let started = compaction_start("ct-owned-checkpoint");
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    tree.validate_event(&Event::AgentStandaloneCompactionStarted(started.clone()))
        .expect("start");
    tree.apply_event(&Event::AgentStandaloneCompactionStarted(started.clone()));
    let compacted = tau_proto::AgentCompacted {
        agent_id: agent_id(),
        replacement_window: vec![tau_proto::ContextItem::Message(tau_proto::MessageItem {
            role: tau_proto::ContextRole::User,
            content: vec![tau_proto::ContentPart::Text {
                text: "summary".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        transaction_id: Some(started.transaction_id.clone()),
        cut: Some(started.cut),
        suffix_end: Some(started.cut),
        compact_prompt_id: Some(started.compact_prompt_id.clone()),
        model: Some(started.model.clone()),
        operation: Some(started.operation),
    };
    tree.validate_event(&Event::AgentCompacted(compacted.clone()))
        .expect("compaction outcome");
    tree.apply_event(&Event::AgentCompacted(compacted));
    let checkpoint = tau_proto::AgentInferenceDispatchStarted {
        agent_id: agent_id(),
        transaction_id: Some(started.transaction_id),
        agent_prompt_id: "ap-owned-inference"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        through: AgentHead::Root,
        model: Some(started.model),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: Some(started.cut),
    };
    tree.validate_event(&Event::AgentInferenceDispatchStarted(checkpoint.clone()))
        .expect("exact checkpoint");
    for mut mismatched in [
        {
            let mut value = checkpoint.clone();
            value.model = Some("provider/other".into());
            value
        },
        {
            let mut value = checkpoint.clone();
            value.operation = Some(tau_proto::PromptOperation::StandaloneCompaction);
            value
        },
        {
            let mut value = checkpoint.clone();
            value.activation_cut = None;
            value
        },
    ] {
        mismatched.agent_prompt_id = format!("{}-mismatch", mismatched.agent_prompt_id)
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid");
        assert!(
            validation_error(&tree, Event::AgentInferenceDispatchStarted(mismatched))
                .contains("mismatches its transaction")
        );
    }
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
            7 => {
                compacted.compact_prompt_id = Some(
                    "ap-wrong"
                        .parse::<tau_proto::AgentPromptId>()
                        .expect("known-safe AgentPromptId must be valid"),
                )
            }
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

/// Provider-authored opaque compaction items must survive identical live and
/// persisted boundary validation without a parsed or serialized replacement.
#[test]
fn provider_compaction_replacement_has_identical_live_and_replay_state() {
    let replacement = ContextItem::Compaction(tau_proto::OpaqueProviderItem::with_raw_json(
        tau_proto::CborValue::Map(vec![]),
        r#"{"type":"compaction","id":"cmp_1","encrypted_content":"opaque"}"#.to_owned(),
    ));
    let boundary = Event::AgentCompacted(tau_proto::AgentCompacted {
        agent_id: agent_id(),
        transaction_id: None,
        cut: None,
        suffix_end: None,
        compact_prompt_id: None,
        model: None,
        operation: None,
        replacement_window: vec![replacement.clone()],
    });
    let mut live = AgentTree::from_events(agent_id(), &[]);
    live.validate_event(&boundary)
        .expect("live boundary must validate");
    live.apply_event(&boundary);
    let mut replay = AgentTree::from_events(agent_id(), &[]);
    apply_persisted_test_record(&mut replay, AgentEventParent::Root, boundary);

    assert!(matches!(
        live.current_branch().last(),
        Some(AgentEntry::Compaction {
            replacement_window,
            ..
        }) if replacement_window == &vec![replacement]
    ));
    assert_eq!(live.current_branch(), replay.current_branch());
}

/// Ensures bounded canonical opaque boundaries have the same complete core
/// projection when appended live or reconstructed cold, while rejected
/// boundaries cannot partially mutate that projection or consume a sequence.
#[test]
fn standalone_compaction_opaque_windows_match_live_append_and_cold_replay() {
    fn record(seq: u64, event: Event) -> PersistedAgentEvent {
        PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
            seq: PersistedAgentEventSeq::new(seq),
            source: None,
            event,
            parent: AgentEventParent::InheritHead,
            fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
            recorded_at: tau_proto::UnixMicros::default(),
        }
    }

    for case in 0_u8..3 {
        let raw_json = format!(
            r#"{{"type":"compaction","id":"cmp-{case}","encrypted_content":"opaque-{case}"}}"#
        );
        let replacement = ContextItem::Compaction(tau_proto::OpaqueProviderItem::with_raw_json(
            tau_proto::CborValue::Map(vec![]),
            raw_json.clone(),
        ));
        let started = compaction_start(&format!("ct-opaque-{case}"));
        let compacted = tau_proto::AgentCompacted {
            agent_id: agent_id(),
            replacement_window: vec![replacement.clone()],
            transaction_id: Some(started.transaction_id.clone()),
            cut: Some(started.cut),
            suffix_end: Some(started.cut),
            compact_prompt_id: Some(started.compact_prompt_id.clone()),
            model: Some(started.model.clone()),
            operation: Some(started.operation),
        };
        let checkpoint = tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_id(),
            transaction_id: Some(started.transaction_id.clone()),
            agent_prompt_id: format!("ap-opaque-inference-{case}")
                .parse()
                .expect("bounded test prompt id"),
            through: AgentHead::Root,
            model: Some(started.model.clone()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(started.cut),
        };
        let records = vec![
            record(0, Event::AgentStandaloneCompactionStarted(started.clone())),
            record(1, Event::AgentCompacted(compacted)),
            record(2, Event::AgentInferenceDispatchStarted(checkpoint.clone())),
        ];

        let mut live = AgentTree::from_events(agent_id(), &[]);
        for event in &records {
            live.apply_persisted_record(event)
                .expect("generated canonical boundary must append");
        }
        let replay = AgentTree::from_events(agent_id(), &records);

        assert_eq!(live, replay, "case {case}: live and replay projections");
        assert_eq!(live.head(), replay.head(), "case {case}: boundary head");
        assert_eq!(
            live.standalone_compaction_recovery(),
            Some(StandaloneCompactionRecovery::DispatchUncertain(checkpoint)),
            "case {case}: transaction and checkpoint recovery"
        );
        assert!(matches!(
            live.current_branch().last(),
            Some(AgentEntry::Compaction {
                replacement_window,
                ..
            }) if matches!(
                replacement_window.as_slice(),
                [ContextItem::Compaction(item)] if item.raw_json.as_deref() == Some(raw_json.as_str())
            )
        ));
    }

    for (label, replacement_window, transaction_id) in [
        (
            "empty replacement",
            Vec::new(),
            Some(
                tau_proto::CompactionTransactionId::parse("ct-invalid-empty")
                    .expect("bounded test transaction id"),
            ),
        ),
        (
            "harness trigger",
            vec![ContextItem::CompactionTrigger],
            Some(
                tau_proto::CompactionTransactionId::parse("ct-invalid-trigger")
                    .expect("bounded test transaction id"),
            ),
        ),
        (
            "unknown transaction",
            vec![ContextItem::Compaction(
                tau_proto::OpaqueProviderItem::with_raw_json(
                    tau_proto::CborValue::Map(vec![]),
                    r#"{"type":"compaction","id":"cmp-invalid","encrypted_content":"opaque"}"#,
                ),
            )],
            Some(
                tau_proto::CompactionTransactionId::parse("ct-other")
                    .expect("bounded test transaction id"),
            ),
        ),
    ] {
        let started = compaction_start("ct-invalid");
        let mut live = AgentTree::from_events(agent_id(), &[]);
        apply_persisted_test_record(
            &mut live,
            AgentEventParent::InheritHead,
            Event::AgentStandaloneCompactionStarted(started.clone()),
        );
        let before = live.clone();
        let sequence = live.next_event_seq();
        let invalid = record(
            sequence.get(),
            Event::AgentCompacted(tau_proto::AgentCompacted {
                agent_id: agent_id(),
                replacement_window,
                transaction_id,
                cut: Some(started.cut),
                suffix_end: Some(started.cut),
                compact_prompt_id: Some(started.compact_prompt_id),
                model: Some(started.model),
                operation: Some(started.operation),
            }),
        );

        assert!(
            live.apply_persisted_record(&invalid).is_err(),
            "{label} must reject"
        );
        assert_eq!(live, before, "{label} must leave the projection unchanged");
        assert_eq!(
            live.next_event_seq(),
            sequence,
            "{label} must not consume its record sequence"
        );
    }
}

/// Watch provider-status messages must carry their structured payload, while
/// ordinary messages must not smuggle one into the durable agent transcript.
#[test]
fn validate_event_enforces_watch_payload_discriminator() {
    let id = agent_id();
    let tree = AgentTree::from_events(id.clone(), &[]);
    let provider_payload = tau_proto::AgentWatchProviderStatusNotification {
        session_id: "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        subscription_id: "watch-1".to_owned(),
        turn_generation: 1,
        agent_prompt_id: "sp-watch"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
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
            message_id: tau_proto::AgentMessageId::parse("msg-invalid-watch-provider-status")
                .expect("test message id must satisfy its grammar"),
            sender_id: other_agent_id(),
            sender_session_id: None,
            recipient_id: id.clone(),
            kind,
            watch_provider_status,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: String::new(),
        });
        assert!(
            validation_error(&tree, event).contains("payload must be present exactly"),
            "provider-status payloads must match their discriminator"
        );
    }
}

/// Ensures canonical work-status phase/title violations fail with a diagnostic
/// distinct from the message-kind payload discriminator.
#[test]
fn validate_event_rejects_noncanonical_work_status_title_shape() {
    let id = agent_id();
    let tree = AgentTree::from_events(id.clone(), &[]);
    let event_for = |phase, title: Option<String>| {
        Event::AgentMessageReceived(AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("msg-invalid-work-status")
                .expect("test message id must satisfy its grammar"),
            sender_id: other_agent_id(),
            sender_session_id: None,
            recipient_id: id.clone(),
            kind: AgentMessageKind::WatchWorkStatus,
            watch_provider_status: None,
            watch_work_status: Some(tau_proto::AgentWatchWorkStatusNotification {
                session_id: "session-1".parse().expect("valid session id"),
                subscription_id: "watch-1".to_owned(),
                status_epoch: 1,
                phase,
                title,
                initial: true,
            }),
            watch_long_wait: None,
            watch_lifecycle: None,
            message: String::new(),
        })
    };
    let shape_diagnostic =
        "work-status title must be absent for unreported and present for every reported phase";
    for (phase, title) in [
        (
            tau_proto::AgentWorkStatusPhase::Unreported,
            Some("title".to_owned()),
        ),
        (tau_proto::AgentWorkStatusPhase::Working, None),
    ] {
        assert_eq!(
            validation_error(&tree, event_for(phase, title)),
            shape_diagnostic
        );
    }
    let canonical_diagnostic = "work-status title must be nonempty, trimmed, one line, control-free, and at most 160 UTF-8 bytes";
    for title in [
        String::new(),
        " ".to_owned(),
        " title".to_owned(),
        "title ".to_owned(),
        "two\nlines".to_owned(),
        "control\u{7}".to_owned(),
        "line\u{2028}separator".to_owned(),
        "paragraph\u{2029}separator".to_owned(),
        "x".repeat(161),
    ] {
        assert_eq!(
            validation_error(
                &tree,
                event_for(tau_proto::AgentWorkStatusPhase::Working, Some(title))
            ),
            canonical_diagnostic
        );
    }
    tree.validate_event(&event_for(
        tau_proto::AgentWorkStatusPhase::Working,
        Some("x".repeat(160)),
    ))
    .expect("the exact 160-byte boundary must remain valid");
}

/// Ensures semantic watch kinds require exactly their matching typed payload
/// and lifecycle facts remain content-free.
#[test]
fn validate_event_enforces_semantic_watch_payload_discriminators() {
    let id = agent_id();
    let tree = AgentTree::from_events(id.clone(), &[]);
    let work = tau_proto::AgentWatchWorkStatusNotification {
        session_id: "session-1".parse().expect("valid session id"),
        subscription_id: "watch-1".to_owned(),
        status_epoch: 1,
        phase: tau_proto::AgentWorkStatusPhase::Working,
        title: Some("work".to_owned()),
        initial: false,
    };
    let wait = tau_proto::AgentWatchLongWaitNotification {
        session_id: "session-1".parse().expect("valid session id"),
        subscription_id: "watch-1".to_owned(),
        status_epoch: 1,
        threshold_minutes: 15,
    };
    for (kind, watch_work_status, watch_long_wait) in [
        (AgentMessageKind::WatchWorkStatus, None, None),
        (AgentMessageKind::Message, Some(work), None),
        (AgentMessageKind::WatchLongWait, None, None),
        (AgentMessageKind::Message, None, Some(wait)),
    ] {
        let event = Event::AgentMessageReceived(AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("msg-invalid-semantic-watch")
                .expect("valid message id"),
            sender_id: other_agent_id(),
            sender_session_id: None,
            recipient_id: id.clone(),
            kind,
            watch_provider_status: None,
            watch_work_status,
            watch_long_wait,
            watch_lifecycle: None,
            message: String::new(),
        });
        assert_eq!(
            validation_error(&tree, event),
            "watch payload must be present exactly for its matching watch message kind"
        );
    }
    let lifecycle = tau_proto::AgentWatchLifecycleNotification {
        state: tau_proto::AgentWatchLifecycleState::Stopped,
        reason: tau_proto::AgentWatchLifecycleReason::UnexpectedUnload,
    };
    let lifecycle_event = |kind, watch_lifecycle, message| {
        Event::AgentMessageReceived(AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("msg-invalid-watch-lifecycle")
                .expect("valid message id"),
            sender_id: other_agent_id(),
            sender_session_id: None,
            recipient_id: id.clone(),
            kind,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle,
            message,
        })
    };
    assert_eq!(
        validation_error(
            &tree,
            lifecycle_event(
                AgentMessageKind::WatchLifecycle,
                Some(lifecycle.clone()),
                "content must not survive".to_owned(),
            )
        ),
        "watch lifecycle messages must be content-free"
    );
    assert_eq!(
        validation_error(
            &tree,
            lifecycle_event(AgentMessageKind::Message, Some(lifecycle), String::new())
        ),
        "watch payload must be present exactly for its matching watch message kind"
    );
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
        mutation_id: None,
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
            mutation_id: None,
            inheritable,
        }));
    }
    let inherited = tree.inheritable_metadata();
    assert!(inherited.contains_key(&inherit_key));
    assert!(!inherited.contains_key(&local_key));
}

/// Ensures provider tool-call rounds fold only after every terminal result,
/// preserving model call order and then flushing typed agent messages and
/// message facts together in durable acceptance order.
#[test]
fn provider_tool_round_waits_for_all_terminal_results() {
    let agent_id = agent_id();
    let mut tree = AgentTree::from_events(agent_id.clone(), &[]);
    let first_call_id = ToolCallId::from("call-first");
    let second_call_id = ToolCallId::from("call-second");
    let third_call_id = ToolCallId::from("call-third");
    apply_persisted_test_record(
        &mut tree,
        AgentEventParent::Root,
        Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            agent_id: agent_id.clone(),
            text: "H".to_owned(),
            inference_activation: true,
            message_class: Default::default(),
        }),
    );
    tree.apply_persisted_record(&PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([1; 16]),
        seq: PersistedAgentEventSeq::new(1),
        source: None,
        event: Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_id.clone(),
            transaction_id: None,
            agent_prompt_id: tau_proto::AgentPromptId::parse("sp-tool-round").expect("prompt id"),
            through: AgentHead::Node(NodeId::new(0)),
            model: Some("provider/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(AgentHead::Root),
        }),
        parent: AgentEventParent::Under(NodeId::new(0)),
        fold_semantics: AgentJournalFoldSemantics::InferenceDeferredInputV1,
        recorded_at: tau_proto::UnixMicros::new(1),
    })
    .expect("V1 owner");
    let (pre_response_seq, pre_response_node) = apply_persisted_test_record(
        &mut tree,
        AgentEventParent::Under(NodeId::new(0)),
        Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            agent_id: agent_id.clone(),
            text: "accepted before tool-bearing response".to_owned(),
            inference_activation: false,
            message_class: Default::default(),
        }),
    );
    assert!(pre_response_node.is_none());
    let assistant_node_id = tree
        .apply_event_at(
            AgentEventParent::Under(NodeId::new(0)),
            &Event::ProviderResponseFinished(tau_proto::ProviderResponseFinished {
                estimated_api_cost_rates: None,
                estimated_api_cost_increment: None,

                agent_prompt_id: "sp-tool-round"
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
                agent_id: agent_id.clone(),
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
                        call_id: third_call_id.clone(),
                        name: ToolName::new("third_tool"),
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
                context_limit_telemetry: None,
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
    let (outbound_seq, outbound_node) = apply_persisted_test_record(
        &mut tree,
        AgentEventParent::InheritHead,
        Event::AgentMessageSent(tau_proto::AgentMessageSent {
            message_id: tau_proto::AgentMessageId::parse("agent-sent-during-tool")
                .expect("test identifier must satisfy its grammar"),
            sender_id: agent_id.clone(),
            recipient: tau_proto::AgentMessageRecipient::Agent {
                agent_id: other_agent_id(),
            },
            kind: AgentMessageKind::Message,
            message: "outbound after result".to_owned(),
        }),
    );
    assert!(
        outbound_node.is_none(),
        "outbound projection must remain pending behind the tool result"
    );
    let (message_fact_seq, message_fact_node) = apply_persisted_test_record(
        &mut tree,
        AgentEventParent::InheritHead,
        Event::MessageDelivered(tau_proto::MessageDelivered::new(
            tau_proto::MessagePublisherId::parse("test-publisher")
                .expect("canonical publisher id must satisfy the identifier grammar"),
            tau_proto::MessageAgentTarget::new(agent_id.as_str()),
            tau_proto::MessageFactId::new("during-tool-fact"),
            tau_proto::MessageParty {
                stable_id: "external-sender".to_owned(),
                display_name: None,
                sender_auth: None,
            },
            None,
            "later",
        )),
    );
    assert!(
        message_fact_node.is_none(),
        "generic message fact must share the tool-adjacent pending input queue"
    );
    let (inbound_seq, inbound_node) = apply_persisted_test_record(
        &mut tree,
        AgentEventParent::InheritHead,
        Event::AgentMessageReceived(AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("agent-received-during-tool")
                .expect("test identifier must satisfy its grammar"),
            sender_id: other_agent_id(),
            sender_session_id: None,
            recipient_id: agent_id.clone(),
            kind: AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "inbound after fact".to_owned(),
        }),
    );
    assert!(
        inbound_node.is_none(),
        "inbound projection must remain pending behind the tool result"
    );
    assert!(
        tree.apply_event_at(
            AgentEventParent::InheritHead,
            &Event::ProviderToolResult(tau_proto::ToolResult {
                presentation: Default::default(),
                call_id: second_call_id.clone(),
                tool_name: ToolName::new("second_tool"),
                tool_type: ToolType::Function,
                result: tau_proto::CborValue::Text("second done".to_owned()),
                provider_content: Vec::new(),
                kind: ToolResultKind::Final,
                display: None,
                originator: PromptOriginator::User,
            }),
        )
        .is_none()
    );
    assert_eq!(tree.head(), Some(assistant_node_id));
    assert!(
        tree.apply_event_at(
            AgentEventParent::InheritHead,
            &Event::ToolCancelled(tau_proto::ToolCancelled {
                presentation: Default::default(),
                call_id: third_call_id.clone(),
                tool_name: ToolName::new("third_tool"),
                tool_type: ToolType::Function,
            }),
        )
        .is_none()
    );
    assert_eq!(tree.head(), Some(assistant_node_id));

    let final_node_id = tree
        .apply_event_at(
            AgentEventParent::InheritHead,
            &Event::ProviderToolError(tau_proto::ToolError {
                presentation: Default::default(),
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
    let final_node = tree
        .node(final_node_id)
        .expect("inbound agent-message node should exist");
    assert!(matches!(
        final_node.entry,
        AgentEntry::AgentMessage {
            durable_event_seq,
            direction: AgentMessageDirection::Inbound,
            ..
        } if durable_event_seq == inbound_seq
    ));
    let message_fact_node = tree
        .node(final_node.parent_id.expect("inbound follows message fact"))
        .expect("message-fact node should exist");
    assert!(matches!(
        message_fact_node.entry,
        AgentEntry::MessageFact {
            durable_event_seq,
            ..
        } if durable_event_seq == message_fact_seq
    ));
    let outbound_node = tree
        .node(message_fact_node.parent_id.expect("fact follows outbound"))
        .expect("outbound agent-message node should exist");
    assert!(matches!(
        outbound_node.entry,
        AgentEntry::AgentMessage {
            durable_event_seq,
            direction: AgentMessageDirection::Outbound,
            ..
        } if durable_event_seq == outbound_seq
    ));
    let pre_response_node = tree
        .node(
            outbound_node
                .parent_id
                .expect("outbound follows pre-response input"),
        )
        .expect("pre-response input should exist");
    assert!(matches!(
        pre_response_node.entry,
        AgentEntry::UserInput { .. }
    ));
    assert_eq!(
        tree.node_for_durable_event_seq(pre_response_seq),
        Some(pre_response_node.id)
    );
    let tool_results_node = tree
        .node(
            pre_response_node
                .parent_id
                .expect("pre-response input follows tool results"),
        )
        .expect("tool results node should exist");
    assert_eq!(tool_results_node.parent_id, Some(assistant_node_id));

    let AgentEntry::ToolResults { items } = &tool_results_node.entry else {
        panic!("expected tool results entry");
    };
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].call_id, first_call_id);
    assert!(matches!(
        items[0].status,
        ToolResultStatus::Error { ref message } if message == "first failed"
    ));
    assert_eq!(items[1].call_id, third_call_id);
    assert!(matches!(
        items[1].status,
        ToolResultStatus::Cancelled { .. }
    ));
    assert_eq!(items[2].call_id, second_call_id);
    assert!(matches!(items[2].status, ToolResultStatus::Success));
    assert!(
        tree.unresolved_foreground_tool_calls_from(Some(final_node_id))
            .is_empty()
    );
}

/// Ensures one foreground provider round owns the whole tree while only
/// context accepted on the tool-calling assistant's branch defers behind it.
#[test]
fn provider_tool_round_is_tree_global_and_branch_applicable() {
    let agent_id = agent_id();
    let mut tree = AgentTree::from_events(agent_id.clone(), &[]);
    let root_node = tree
        .apply_event_at(
            AgentEventParent::Root,
            &Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: true,
                agent_id: agent_id.clone(),
                text: "root prompt".to_owned(),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                originator: PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
        )
        .expect("root prompt folds");
    let call_id = ToolCallId::from("call-global");
    let assistant_node = tree
        .apply_event_at(
            AgentEventParent::Under(root_node),
            &Event::ProviderResponseFinished(tool_calling_response(
                &agent_id,
                "ap-global",
                vec![call_id.clone()],
            )),
        )
        .expect("first tool-bearing response folds");
    assert!(tree.has_open_foreground_tool_round());

    let second_round = Event::ProviderResponseFinished(tool_calling_response(
        &agent_id,
        "ap-second",
        vec![ToolCallId::from("call-second-round")],
    ));
    let error = tree
        .validate_event_at(AgentEventParent::Under(root_node), &second_round)
        .expect_err("a sibling response cannot open another foreground round");
    assert!(error.to_string().contains("already has an open"));

    let sibling_message = Event::AgentMessageReceived(AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse("sibling-message")
            .expect("test identifier must satisfy its grammar"),
        sender_id: other_agent_id(),
        sender_session_id: None,
        recipient_id: agent_id.clone(),
        kind: AgentMessageKind::Message,
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: None,
        watch_lifecycle: None,
        message: "sibling materializes now".to_owned(),
    });
    let (_, sibling_node) = apply_persisted_test_record(
        &mut tree,
        AgentEventParent::Under(root_node),
        sibling_message,
    );
    let sibling_node = sibling_node.expect("sibling input materializes immediately");
    assert_eq!(
        tree.node(sibling_node)
            .expect("materialized sibling node remains addressable")
            .parent_id,
        Some(root_node)
    );

    let descendant_message = Event::AgentMessageReceived(AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse("descendant-message")
            .expect("test identifier must satisfy its grammar"),
        sender_id: other_agent_id(),
        sender_session_id: None,
        recipient_id: agent_id,
        kind: AgentMessageKind::Message,
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: None,
        watch_lifecycle: None,
        message: "descendant waits".to_owned(),
    });
    let (_, descendant_node) = apply_persisted_test_record(
        &mut tree,
        AgentEventParent::Under(assistant_node),
        descendant_message,
    );
    assert!(descendant_node.is_none());

    let drained = tree.apply_event_at(
        AgentEventParent::InheritHead,
        &Event::ProviderToolResult(tau_proto::ToolResult {
            presentation: Default::default(),
            call_id,
            tool_name: ToolName::new("tool"),
            tool_type: ToolType::Function,
            result: tau_proto::CborValue::Text("done".to_owned()),
            provider_content: Vec::new(),
            kind: ToolResultKind::Final,
            display: None,
            originator: PromptOriginator::User,
        }),
    );
    let drained = drained.expect("tool result and deferred input fold");
    let drained_node = tree.node(drained).expect("drained message node");
    assert!(matches!(
        drained_node.entry,
        AgentEntry::AgentMessage { .. }
    ));
    let results_node_id = drained_node
        .parent_id
        .expect("drained message has a tool-results parent");
    let results_node = tree
        .node(results_node_id)
        .expect("tool-results parent remains addressable");
    assert_eq!(results_node.parent_id, Some(assistant_node));
    assert!(!tree.has_open_foreground_tool_round());
}

/// Ensures convenience incremental folds allocate deterministic, monotonic
/// synthetic sequences for repeated agent-message occurrences.
#[test]
fn synthetic_agent_message_folds_advance_occurrence_sequence() {
    let agent_id = agent_id();
    let mut tree = AgentTree::from_events(agent_id.clone(), &[]);
    for (message_id, message) in [("sent-one", "one"), ("sent-two", "two")] {
        tree.apply_event(&Event::AgentMessageSent(tau_proto::AgentMessageSent {
            message_id: tau_proto::AgentMessageId::parse(message_id)
                .expect("test identifier must satisfy its grammar"),
            sender_id: agent_id.clone(),
            recipient: AgentMessageRecipient::Agent {
                agent_id: other_agent_id(),
            },
            kind: AgentMessageKind::Message,
            message: message.to_owned(),
        }));
    }
    let sequences = tree
        .nodes()
        .iter()
        .filter_map(|node| match node.entry {
            AgentEntry::AgentMessage {
                durable_event_seq, ..
            } => Some(durable_event_seq.get()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(sequences, vec![0, 1]);
    assert_eq!(tree.next_event_seq().get(), 2);
}

/// A V1 ordinary checkpoint owns same-branch asynchronous input until its
/// response, and cold replay must allocate the identical single input node.
#[test]
fn inference_deferred_input_v1_matches_live_append_and_cold_replay() {
    let agent_id = agent_id();
    let prompt_id = tau_proto::AgentPromptId::parse("ap-v1-owner").expect("test prompt id");
    let record = |seq, parent, event, fold_semantics| PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([seq as u8; 16]),
        seq: PersistedAgentEventSeq::new(seq),
        source: None,
        event,
        parent,
        fold_semantics,
        recorded_at: tau_proto::UnixMicros::new(seq),
    };
    let initial = Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
        agent_id: agent_id.clone(),
        text: "H".to_owned(),
        inference_activation: true,
        message_class: Default::default(),
    });
    let checkpoint =
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_id.clone(),
            transaction_id: None,
            agent_prompt_id: prompt_id.clone(),
            through: AgentHead::Node(NodeId::new(0)),
            model: Some("provider/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(AgentHead::Root),
        });
    let input = Event::AgentMessageReceived(AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse("v1-input").expect("test message id"),
        sender_id: other_agent_id(),
        sender_session_id: None,
        recipient_id: agent_id.clone(),
        kind: AgentMessageKind::Message,
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: None,
        watch_lifecycle: None,
        message: "Q".to_owned(),
    });
    let second_input = Event::AgentMessageReceived(AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse("v1-input-two").expect("test message id"),
        sender_id: other_agent_id(),
        sender_session_id: None,
        recipient_id: agent_id.clone(),
        kind: AgentMessageKind::Message,
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: None,
        watch_lifecycle: None,
        message: "Q2".to_owned(),
    });
    let raw_input = Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::parse("v1-bridge").expect("publisher"),
        tau_proto::MessageAgentTarget::new(agent_id.as_str()),
        tau_proto::MessageFactId::new("v1-raw"),
        tau_proto::MessageParty {
            stable_id: "external".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "Q3",
    ));
    let mut response = tool_calling_response(&agent_id, prompt_id.as_str(), Vec::new());
    response.stop_reason = tau_proto::ProviderStopReason::EndTurn;
    response.output_items = vec![ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text {
            text: "R".to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    })];
    let records = vec![
        record(
            0,
            AgentEventParent::Root,
            initial,
            AgentJournalFoldSemantics::Legacy,
        ),
        record(
            1,
            AgentEventParent::Under(NodeId::new(0)),
            checkpoint,
            AgentJournalFoldSemantics::InferenceDeferredInputV1,
        ),
        record(
            2,
            AgentEventParent::Under(NodeId::new(0)),
            input,
            AgentJournalFoldSemantics::Legacy,
        ),
        record(
            3,
            AgentEventParent::Under(NodeId::new(0)),
            second_input,
            AgentJournalFoldSemantics::Legacy,
        ),
        record(
            4,
            AgentEventParent::InheritHead,
            raw_input,
            AgentJournalFoldSemantics::Legacy,
        ),
        record(
            5,
            AgentEventParent::Under(NodeId::new(0)),
            Event::ProviderResponseFinished(response),
            AgentJournalFoldSemantics::Legacy,
        ),
    ];
    let mut live = AgentTree::from_events(agent_id.clone(), &[]);
    for record in &records {
        live.apply_persisted_record(record).expect("live V1 fold");
    }
    let replay = AgentTree::try_from_events(agent_id, &records).expect("cold V1 fold");
    assert_eq!(live, replay);
    assert!(matches!(
        live.nodes()[1].entry,
        AgentEntry::AssistantResponse { .. }
    ));
    assert!(matches!(
        live.nodes()[2].entry,
        AgentEntry::AgentMessage { durable_event_seq, .. }
            if durable_event_seq == PersistedAgentEventSeq::new(2)
    ));
    assert!(matches!(
        live.nodes()[3].entry,
        AgentEntry::AgentMessage { durable_event_seq, .. }
            if durable_event_seq == PersistedAgentEventSeq::new(3)
    ));
    assert!(matches!(
        live.nodes()[4].entry,
        AgentEntry::MessageFact { durable_event_seq, .. }
            if durable_event_seq == PersistedAgentEventSeq::new(4)
    ));
}

/// Standalone compaction preserves the exact V1 post-response suffix and never
/// places deferred input inside an open provider tool round.
#[test]
fn v1_compaction_retains_exact_suffix_without_splitting_tool_round() {
    for (with_tool, reactive_recovery) in [(false, false), (true, false), (false, true)] {
        let agent_id = agent_id();
        let prompt_id = tau_proto::AgentPromptId::parse(match (with_tool, reactive_recovery) {
            (true, false) => "ap-v1-compact-tool",
            (false, true) => "ap-v1-compact-reactive",
            (false, false) => "ap-v1-compact-text",
            (true, true) => unreachable!("reactive rejection cannot open a tool round"),
        })
        .expect("prompt id");
        let mut tree = AgentTree::from_events(agent_id.clone(), &[]);
        let mut records = Vec::new();
        let mut append = |tree: &mut AgentTree,
                          parent: AgentEventParent,
                          event: Event,
                          fold_semantics: AgentJournalFoldSemantics| {
            let seq = tree.next_event_seq();
            let record = PersistedAgentEvent {
                observation_id: tau_proto::ObservationId::from_bytes([seq.get() as u8; 16]),
                seq,
                source: None,
                event,
                parent,
                fold_semantics,
                recorded_at: tau_proto::UnixMicros::new(seq.get()),
            };
            tree.apply_persisted_record(&record)
                .expect("V1 compaction record");
            records.push(record);
        };
        append(
            &mut tree,
            AgentEventParent::Root,
            Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                agent_id: agent_id.clone(),
                text: "H".to_owned(),
                inference_activation: false,
                message_class: Default::default(),
            }),
            AgentJournalFoldSemantics::Legacy,
        );
        if reactive_recovery {
            let prefix_call = ToolCallId::from("call-v1-reactive-prefix");
            append(
                &mut tree,
                AgentEventParent::Under(NodeId::new(0)),
                Event::ProviderResponseFinished(tool_calling_response(
                    &agent_id,
                    "ap-v1-reactive-prefix",
                    vec![prefix_call.clone()],
                )),
                AgentJournalFoldSemantics::Legacy,
            );
            append(
                &mut tree,
                AgentEventParent::InheritHead,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    presentation: Default::default(),
                    call_id: prefix_call,
                    tool_name: ToolName::new("prefix"),
                    tool_type: ToolType::Function,
                    result: tau_proto::CborValue::Text("prefix done".to_owned()),
                    provider_content: Vec::new(),
                    kind: ToolResultKind::Final,
                    display: None,
                    originator: PromptOriginator::User,
                }),
                AgentJournalFoldSemantics::Legacy,
            );
        }
        let owner_through = tree.head().expect("owner through");
        append(
            &mut tree,
            AgentEventParent::Under(owner_through),
            Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
                agent_id: agent_id.clone(),
                transaction_id: None,
                agent_prompt_id: prompt_id.clone(),
                through: AgentHead::Node(owner_through),
                model: Some("provider/model".into()),
                operation: Some(tau_proto::PromptOperation::Inference),
                activation_cut: Some(AgentHead::Root),
            }),
            AgentJournalFoldSemantics::InferenceDeferredInputV1,
        );
        append(
            &mut tree,
            AgentEventParent::Under(owner_through),
            Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                agent_id: agent_id.clone(),
                text: "Q".to_owned(),
                inference_activation: true,
                message_class: Default::default(),
            }),
            AgentJournalFoldSemantics::Legacy,
        );
        let call_id = ToolCallId::from("call-v1-compact");
        let response = if reactive_recovery {
            let mut response = tool_calling_response(&agent_id, prompt_id.as_str(), Vec::new());
            response.stop_reason = tau_proto::ProviderStopReason::Error;
            response.failure_kind = Some(tau_proto::ProviderFailureKind::ContextWindowExceeded);
            response.error = Some("context overflow".to_owned());
            response.recovery_disposition =
                tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned;
            response
        } else if with_tool {
            tool_calling_response(&agent_id, prompt_id.as_str(), vec![call_id.clone()])
        } else {
            let mut response = tool_calling_response(&agent_id, prompt_id.as_str(), Vec::new());
            response.stop_reason = tau_proto::ProviderStopReason::EndTurn;
            response.output_items = vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "R".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })];
            response
        };
        append(
            &mut tree,
            AgentEventParent::Under(owner_through),
            Event::ProviderResponseFinished(response),
            AgentJournalFoldSemantics::Legacy,
        );
        if with_tool {
            append(
                &mut tree,
                AgentEventParent::InheritHead,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    presentation: Default::default(),
                    call_id,
                    tool_name: ToolName::new("test"),
                    tool_type: ToolType::Function,
                    result: tau_proto::CborValue::Text("done".to_owned()),
                    provider_content: Vec::new(),
                    kind: ToolResultKind::Final,
                    display: None,
                    originator: PromptOriginator::User,
                }),
                AgentJournalFoldSemantics::Legacy,
            );
        }
        let suffix_end = AgentHead::Node(tree.head().expect("Q suffix head"));
        let mut started = compaction_start(match (with_tool, reactive_recovery) {
            (true, false) => "ct-v1-compact-tool",
            (false, true) => "ct-v1-compact-reactive",
            (false, false) => "ct-v1-compact-text",
            (true, true) => unreachable!("reactive rejection cannot open a tool round"),
        });
        started.cut = if reactive_recovery {
            AgentHead::Root
        } else {
            AgentHead::Node(NodeId::new(0))
        };
        started.resume_through = Some(if reactive_recovery {
            AgentHead::Node(owner_through)
        } else {
            suffix_end
        });
        if reactive_recovery {
            started.trigger = tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
                failed_agent_prompt_id: prompt_id,
            };
        }
        append(
            &mut tree,
            AgentEventParent::InheritHead,
            Event::AgentStandaloneCompactionStarted(started.clone()),
            AgentJournalFoldSemantics::Legacy,
        );
        append(
            &mut tree,
            AgentEventParent::InheritHead,
            Event::AgentCompacted(tau_proto::AgentCompacted {
                agent_id: agent_id.clone(),
                transaction_id: Some(started.transaction_id),
                cut: Some(started.cut),
                suffix_end: Some(suffix_end),
                compact_prompt_id: Some(started.compact_prompt_id),
                model: Some(started.model),
                operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
                replacement_window: vec![ContextItem::Message(MessageItem {
                    role: ContextRole::Assistant,
                    content: vec![ContentPart::Text {
                        text: "summary".to_owned(),
                    }],
                    phase: None,
                    responses_raw_json: None,
                })],
            }),
            AgentJournalFoldSemantics::Legacy,
        );

        let replay =
            AgentTree::try_from_events(agent_id, &records).expect("cold V1 compaction replay");
        assert_eq!(
            tree, replay,
            "with_tool={with_tool}, reactive_recovery={reactive_recovery}"
        );
        let branch = tree.current_branch();
        let q_index = branch
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    AgentEntry::UserInput { items, .. }
                        if serde_json::to_string(items).is_ok_and(|text| text.contains('Q'))
                )
            })
            .expect("Q suffix retained");
        if !reactive_recovery {
            let response_index = branch
                .iter()
                .position(|entry| matches!(entry, AgentEntry::AssistantResponse { .. }))
                .expect("response suffix retained");
            assert!(
                response_index < q_index,
                "with_tool={with_tool}, reactive_recovery={reactive_recovery}"
            );
        }
        if with_tool || reactive_recovery {
            let response_index = branch
                .iter()
                .position(|entry| matches!(entry, AgentEntry::AssistantResponse { .. }))
                .expect("tool response suffix retained");
            let results_index = branch
                .iter()
                .position(|entry| matches!(entry, AgentEntry::ToolResults { .. }))
                .expect("tool aggregate retained");
            assert!(
                response_index < results_index,
                "with_tool={with_tool}, reactive_recovery={reactive_recovery}"
            );
            assert!(
                results_index < q_index,
                "with_tool={with_tool}, reactive_recovery={reactive_recovery}"
            );
        }
    }
}

/// Durable navigation after dispatch resets V1 eligibility for the reselected
/// real branch; off-branch completion cannot steal selection.
#[test]
fn inference_deferred_input_v1_head_move_resets_branch_eligibility() {
    let agent_id = agent_id();
    let prompt_id = tau_proto::AgentPromptId::parse("ap-v1-moved").expect("prompt id");
    let record = |seq, parent, event, fold_semantics| PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([seq as u8; 16]),
        seq: PersistedAgentEventSeq::new(seq),
        source: None,
        event,
        parent,
        fold_semantics,
        recorded_at: tau_proto::UnixMicros::new(seq),
    };
    let input = |text: &str| {
        Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            agent_id: agent_id.clone(),
            text: text.to_owned(),
            inference_activation: true,
            message_class: Default::default(),
        })
    };
    let moved = |head| {
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent_id.clone(),
            head,
        })
    };
    let checkpoint =
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_id.clone(),
            transaction_id: None,
            agent_prompt_id: prompt_id.clone(),
            through: AgentHead::Node(NodeId::new(0)),
            model: Some("provider/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(AgentHead::Root),
        });
    let mut response = tool_calling_response(&agent_id, prompt_id.as_str(), Vec::new());
    response.stop_reason = tau_proto::ProviderStopReason::EndTurn;
    let records = vec![
        record(0, AgentEventParent::Root, input("H"), Default::default()),
        record(
            1,
            AgentEventParent::Under(NodeId::new(0)),
            input("D"),
            Default::default(),
        ),
        record(
            2,
            AgentEventParent::InheritHead,
            moved(AgentHead::Node(NodeId::new(0))),
            Default::default(),
        ),
        record(
            3,
            AgentEventParent::Under(NodeId::new(0)),
            checkpoint,
            AgentJournalFoldSemantics::InferenceDeferredInputV1,
        ),
        record(
            4,
            AgentEventParent::InheritHead,
            moved(AgentHead::Node(NodeId::new(1))),
            Default::default(),
        ),
        record(
            5,
            AgentEventParent::Under(NodeId::new(1)),
            input("Q"),
            Default::default(),
        ),
        record(
            6,
            AgentEventParent::Under(NodeId::new(0)),
            Event::ProviderResponseFinished(response),
            Default::default(),
        ),
    ];
    let tree = AgentTree::try_from_events(agent_id, &records).expect("fold moved branch");
    assert_eq!(tree.head(), Some(NodeId::new(2)));
    assert_eq!(
        tree.node(NodeId::new(2)).expect("Q").parent_id,
        Some(NodeId::new(1))
    );
    assert!(matches!(
        tree.node(NodeId::new(3)).expect("response").entry,
        AgentEntry::AssistantResponse { .. }
    ));
    assert_eq!(
        tree.node(NodeId::new(3)).expect("response").parent_id,
        Some(NodeId::new(0))
    );
}

/// Root, ancestor, and sibling context never moves behind a V1 owner, while an
/// exact owner-branch occurrence remains node-less until the response.
#[test]
fn inference_deferred_input_v1_defers_only_exact_owner_branch() {
    let agent_id = agent_id();
    let prompt_id = tau_proto::AgentPromptId::parse("ap-v1-branches").expect("prompt id");
    let record = |seq, parent, event, fold_semantics| PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([seq as u8; 16]),
        seq: PersistedAgentEventSeq::new(seq),
        source: None,
        event,
        parent,
        fold_semantics,
        recorded_at: tau_proto::UnixMicros::new(seq),
    };
    let input = |text: &str| {
        Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            agent_id: agent_id.clone(),
            text: text.to_owned(),
            inference_activation: false,
            message_class: Default::default(),
        })
    };
    let checkpoint =
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_id.clone(),
            transaction_id: None,
            agent_prompt_id: prompt_id.clone(),
            through: AgentHead::Node(NodeId::new(1)),
            model: Some("provider/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(AgentHead::Node(NodeId::new(0))),
        });
    let mut response = tool_calling_response(&agent_id, prompt_id.as_str(), Vec::new());
    response.stop_reason = tau_proto::ProviderStopReason::EndTurn;
    let records = vec![
        record(0, AgentEventParent::Root, input("H"), Default::default()),
        record(
            1,
            AgentEventParent::Under(NodeId::new(0)),
            input("D"),
            Default::default(),
        ),
        record(
            2,
            AgentEventParent::Under(NodeId::new(0)),
            input("S"),
            Default::default(),
        ),
        record(
            3,
            AgentEventParent::InheritHead,
            Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                agent_id: agent_id.clone(),
                head: AgentHead::Node(NodeId::new(1)),
            }),
            Default::default(),
        ),
        record(
            4,
            AgentEventParent::Under(NodeId::new(1)),
            checkpoint,
            AgentJournalFoldSemantics::InferenceDeferredInputV1,
        ),
        record(5, AgentEventParent::Root, input("root"), Default::default()),
        record(
            6,
            AgentEventParent::Under(NodeId::new(0)),
            input("ancestor"),
            Default::default(),
        ),
        record(
            7,
            AgentEventParent::Under(NodeId::new(2)),
            input("sibling"),
            Default::default(),
        ),
        record(
            8,
            AgentEventParent::Under(NodeId::new(1)),
            input("owned"),
            Default::default(),
        ),
        record(
            9,
            AgentEventParent::Under(NodeId::new(1)),
            Event::ProviderResponseFinished(response),
            Default::default(),
        ),
    ];
    let tree = AgentTree::try_from_events(agent_id, &records).expect("branch fold");
    assert_eq!(tree.nodes().len(), 8);
    for (node, parent) in [
        (NodeId::new(3), None),
        (NodeId::new(4), Some(NodeId::new(0))),
        (NodeId::new(5), Some(NodeId::new(2))),
    ] {
        assert_eq!(
            tree.node(node).expect("immediate branch node").parent_id,
            parent
        );
    }
    assert!(matches!(
        tree.node(NodeId::new(6)).expect("response").entry,
        AgentEntry::AssistantResponse { .. }
    ));
    assert_eq!(
        tree.node(NodeId::new(7)).expect("owned input").parent_id,
        Some(NodeId::new(6))
    );
}

/// Both no-response terminal reasons restore owned input at its accepted
/// parent, and a later provider response cannot canonicalize.
#[test]
fn inference_deferred_input_v1_terminal_fallback_rejects_late_response() {
    for reason in [
        tau_proto::AgentPromptTerminationReason::Canceled,
        tau_proto::AgentPromptTerminationReason::Stale,
    ] {
        let agent_id = agent_id();
        let prompt_id = tau_proto::AgentPromptId::parse(format!("ap-v1-{reason:?}").to_lowercase())
            .expect("prompt id");
        let record = |seq, parent, event, fold_semantics| PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([seq as u8; 16]),
            seq: PersistedAgentEventSeq::new(seq),
            source: None,
            event,
            parent,
            fold_semantics,
            recorded_at: tau_proto::UnixMicros::new(seq),
        };
        let checkpoint =
            Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
                agent_id: agent_id.clone(),
                transaction_id: None,
                agent_prompt_id: prompt_id.clone(),
                through: AgentHead::Node(NodeId::new(0)),
                model: Some("provider/model".into()),
                operation: Some(tau_proto::PromptOperation::Inference),
                activation_cut: Some(AgentHead::Root),
            });
        let records = vec![
            record(
                0,
                AgentEventParent::Root,
                Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                    agent_id: agent_id.clone(),
                    text: "H".to_owned(),
                    inference_activation: true,
                    message_class: Default::default(),
                }),
                Default::default(),
            ),
            record(
                1,
                AgentEventParent::Under(NodeId::new(0)),
                checkpoint,
                AgentJournalFoldSemantics::InferenceDeferredInputV1,
            ),
            record(
                2,
                AgentEventParent::Under(NodeId::new(0)),
                Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                    agent_id: agent_id.clone(),
                    text: "Q".to_owned(),
                    inference_activation: true,
                    message_class: Default::default(),
                }),
                Default::default(),
            ),
            record(
                3,
                AgentEventParent::Under(NodeId::new(0)),
                Event::AgentPromptTerminated(tau_proto::AgentPromptTerminated {
                    agent_id: agent_id.clone(),
                    agent_prompt_id: prompt_id.clone(),
                    reason,
                    originator: PromptOriginator::User,
                }),
                Default::default(),
            ),
        ];
        let mut tree =
            AgentTree::try_from_events(agent_id.clone(), &records).expect("terminal fold");
        assert_eq!(tree.nodes().len(), 2);
        assert_eq!(
            tree.node(NodeId::new(1)).expect("Q").parent_id,
            Some(NodeId::new(0))
        );
        let response = tool_calling_response(&agent_id, prompt_id.as_str(), Vec::new());
        let late = record(
            4,
            AgentEventParent::Under(NodeId::new(0)),
            Event::ProviderResponseFinished(response),
            Default::default(),
        );
        assert!(tree.apply_persisted_record(&late).is_err());
        assert_eq!(tree.nodes().len(), 2);
    }
}

/// Mixed journals preserve Legacy allocation and old explicit NodeId targets
/// while later V1 records use response-before-input placement.
#[test]
fn mixed_legacy_v1_replay_preserves_old_node_targets() {
    let agent_id = agent_id();
    let legacy_prompt = tau_proto::AgentPromptId::parse("ap-legacy").expect("prompt id");
    let v1_prompt = tau_proto::AgentPromptId::parse("ap-v1-mixed").expect("prompt id");
    let record = |seq, parent, event, fold_semantics| PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([seq as u8; 16]),
        seq: PersistedAgentEventSeq::new(seq),
        source: None,
        event,
        parent,
        fold_semantics,
        recorded_at: tau_proto::UnixMicros::new(seq),
    };
    let input = |text: &str| {
        Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            agent_id: agent_id.clone(),
            text: text.to_owned(),
            inference_activation: true,
            message_class: Default::default(),
        })
    };
    let checkpoint = |prompt_id: tau_proto::AgentPromptId, through| {
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_id.clone(),
            transaction_id: None,
            agent_prompt_id: prompt_id,
            through,
            model: Some("provider/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(AgentHead::Root),
        })
    };
    let records = vec![
        record(0, AgentEventParent::Root, input("H"), Default::default()),
        record(
            1,
            AgentEventParent::Under(NodeId::new(0)),
            checkpoint(legacy_prompt.clone(), AgentHead::Node(NodeId::new(0))),
            Default::default(),
        ),
        record(
            2,
            AgentEventParent::Under(NodeId::new(0)),
            input("legacy Q"),
            Default::default(),
        ),
        record(
            3,
            AgentEventParent::Under(NodeId::new(1)),
            Event::ProviderResponseFinished(tool_calling_response(
                &agent_id,
                legacy_prompt.as_str(),
                Vec::new(),
            )),
            Default::default(),
        ),
        record(
            4,
            AgentEventParent::Under(NodeId::new(2)),
            checkpoint(v1_prompt.clone(), AgentHead::Node(NodeId::new(2))),
            AgentJournalFoldSemantics::InferenceDeferredInputV1,
        ),
        record(
            5,
            AgentEventParent::Under(NodeId::new(2)),
            input("V1 Q"),
            Default::default(),
        ),
        record(
            6,
            AgentEventParent::Under(NodeId::new(2)),
            Event::ProviderResponseFinished(tool_calling_response(
                &agent_id,
                v1_prompt.as_str(),
                Vec::new(),
            )),
            Default::default(),
        ),
        record(
            7,
            AgentEventParent::InheritHead,
            Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                agent_id: agent_id.clone(),
                head: AgentHead::Node(NodeId::new(1)),
            }),
            Default::default(),
        ),
    ];
    let tree = AgentTree::try_from_events(agent_id, &records).expect("mixed replay");
    assert!(matches!(
        tree.node(NodeId::new(1)).expect("legacy input").entry,
        AgentEntry::UserInput { .. }
    ));
    assert!(matches!(
        tree.node(NodeId::new(2)).expect("legacy response").entry,
        AgentEntry::AssistantResponse { .. }
    ));
    assert!(matches!(
        tree.node(NodeId::new(3)).expect("V1 response").entry,
        AgentEntry::AssistantResponse { .. }
    ));
    assert_eq!(
        tree.node(NodeId::new(4)).expect("V1 input").parent_id,
        Some(NodeId::new(3))
    );
    assert_eq!(tree.head(), Some(NodeId::new(1)));
}

/// Terminal fallback reports the global last allocation separately from the
/// selected virtual-queue tail.
#[test]
fn v1_terminal_fallback_restores_selected_queue_not_global_last_node() {
    let agent_id = agent_id();
    let prompt_id = tau_proto::AgentPromptId::parse("ap-v1-multiqueue").expect("prompt id");
    let mut tree = AgentTree::from_events(agent_id.clone(), &[]);
    for (parent, text) in [
        (AgentEventParent::Root, "H"),
        (AgentEventParent::Under(NodeId::new(0)), "D"),
    ] {
        apply_persisted_test_record(
            &mut tree,
            parent,
            Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                agent_id: agent_id.clone(),
                text: text.to_owned(),
                inference_activation: false,
                message_class: Default::default(),
            }),
        );
    }
    apply_persisted_test_record(
        &mut tree,
        AgentEventParent::InheritHead,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent_id.clone(),
            head: AgentHead::Node(NodeId::new(0)),
        }),
    );
    let seq = tree.next_event_seq();
    tree.apply_persisted_record(&PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([3; 16]),
        seq,
        source: None,
        event: Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_id.clone(),
            transaction_id: None,
            agent_prompt_id: prompt_id.clone(),
            through: AgentHead::Node(NodeId::new(0)),
            model: Some("provider/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(AgentHead::Root),
        }),
        parent: AgentEventParent::Under(NodeId::new(0)),
        fold_semantics: AgentJournalFoldSemantics::InferenceDeferredInputV1,
        recorded_at: tau_proto::UnixMicros::new(seq.get()),
    })
    .expect("owner");
    for (parent, text) in [(NodeId::new(0), "selected Q"), (NodeId::new(1), "other Q")] {
        let (_, node) = apply_persisted_test_record(
            &mut tree,
            AgentEventParent::Under(parent),
            Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                agent_id: agent_id.clone(),
                text: text.to_owned(),
                inference_activation: false,
                message_class: Default::default(),
            }),
        );
        assert!(node.is_none());
    }
    let (_, global_last) = apply_persisted_test_record(
        &mut tree,
        AgentEventParent::Under(NodeId::new(0)),
        Event::AgentPromptTerminated(tau_proto::AgentPromptTerminated {
            agent_id,
            agent_prompt_id: prompt_id,
            reason: tau_proto::AgentPromptTerminationReason::Canceled,
            originator: PromptOriginator::User,
        }),
    );
    assert_eq!(global_last, Some(NodeId::new(3)));
    assert_eq!(tree.head(), Some(NodeId::new(2)));
}

fn tool_calling_response(
    agent_id: &AgentId,
    prompt_id: &str,
    call_ids: Vec<ToolCallId>,
) -> tau_proto::ProviderResponseFinished {
    tau_proto::ProviderResponseFinished {
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: prompt_id
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: agent_id.clone(),
        output_items: call_ids
            .into_iter()
            .map(|call_id| {
                ContextItem::ToolCall(ToolCallItem {
                    call_id,
                    name: ToolName::new("tool"),
                    tool_type: ToolType::Function,
                    arguments: tau_proto::CborValue::Null,
                    raw_arguments_json: None,
                    responses_envelope: None,
                })
            })
            .collect(),
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }
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
                mutation_id: None,
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
                creator: Some(tau_proto::AgentCreator::default()),

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

/// Typed self-compaction delivery must match one durable terminal and cannot
/// commit twice after replay.
#[test]
fn self_compaction_terminal_delivery_is_correlated_and_exactly_once() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let mut request = manual_request("cr-delivery");
    request.caller_agent_id = request.target_agent_id.clone();
    request.initiating_tool_name = tau_proto::ManualCompactionTool::Compact;
    request.visible_tool_name = ToolName::new("compact");
    request.resume_inference = true;
    tree.validate_event(&Event::AgentManualCompactionRequested(request.clone()))
        .expect("request");
    tree.apply_event(&Event::AgentManualCompactionRequested(request.clone()));
    let failed = tau_proto::AgentManualCompactionRequestFailed {
        request_id: request.request_id.clone(),
        target_agent_id: request.target_agent_id.clone(),
        reason: tau_proto::ManualCompactionRequestFailureReason::Cancelled,
    };
    tree.validate_event(&Event::AgentManualCompactionRequestFailed(failed.clone()))
        .expect("failure");
    tree.apply_event(&Event::AgentManualCompactionRequestFailed(failed));
    let terminal = tau_proto::SelfCompactionTerminal {
        request_id: request.request_id.clone(),
        tool_call_id: request.initiating_tool_call_id,
        transaction_id: None,
        outcome: tau_proto::SelfCompactionTerminalOutcome::RequestFailed {
            reason: tau_proto::ManualCompactionRequestFailureReason::Cancelled,
        },
    };
    let delivery = Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
        self_compaction_terminal: Some(terminal.clone()),
        agent_id: agent_id(),
        inference_activation: true,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        text: "bounded terminal".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        ctx_id: None,
    });
    tree.validate_event(&delivery).expect("matching delivery");
    tree.apply_event(&delivery);
    assert_eq!(
        tree.self_compaction_delivery(&request.request_id),
        Some(&terminal)
    );
    assert!(
        validation_error(&tree, delivery).contains("duplicate self-compaction terminal delivery")
    );
}

/// Standalone compaction provider prompts must not advance the target-owned
/// ordinary-inference generation used by the manual compaction rate guard.
#[test]
fn manual_compaction_generation_excludes_standalone_prompts() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let prompt = |id: &str, operation| {
        Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
            model_params: Some(tau_proto::ModelParams::default()),
            outer_turn_id: None,

            agent_prompt_id: id
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            agent_id: agent_id(),
            session_id: "session"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            model: "provider/model".into(),
            operation,
            originator: PromptOriginator::User,
            ctx_id: None,
        })
    };
    let mut compact_owner = compaction_start("ct-generation");
    compact_owner.compact_prompt_id = "ap-compact"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    compact_owner.model = "provider/model".into();
    tree.validate_event(&Event::AgentStandaloneCompactionStarted(
        compact_owner.clone(),
    ))
    .expect("standalone owner");
    tree.apply_event(&Event::AgentStandaloneCompactionStarted(compact_owner));
    let compact_prompt = prompt(
        "ap-compact",
        tau_proto::PromptOperation::StandaloneCompaction,
    );
    tree.validate_event(&compact_prompt)
        .expect("standalone materialization");
    tree.apply_event(&compact_prompt);
    assert_eq!(tree.ordinary_inference_generation(), 0);
    let checkpoint = tau_proto::AgentInferenceDispatchStarted {
        agent_id: agent_id(),
        transaction_id: None,
        agent_prompt_id: "ap-inference"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        through: AgentHead::Root,
        model: Some("provider/model".into()),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: Some(AgentHead::Root),
    };
    tree.validate_event(&Event::AgentInferenceDispatchStarted(checkpoint.clone()))
        .expect("inference owner");
    tree.apply_event(&Event::AgentInferenceDispatchStarted(checkpoint));
    let inference_prompt = prompt("ap-inference", tau_proto::PromptOperation::Inference);
    tree.validate_event(&inference_prompt)
        .expect("inference materialization");
    tree.apply_event(&inference_prompt);
    assert_eq!(tree.ordinary_inference_generation(), 1);
}

/// Compact prompt facts require one exact unresolved owner, reject identity
/// drift and duplicates, and advance ordinary generation exactly once.
#[test]
fn prompt_started_requires_unique_matching_owner() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let checkpoint = tau_proto::AgentInferenceDispatchStarted {
        agent_id: agent_id(),
        transaction_id: None,
        agent_prompt_id: "ap-owned"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        through: AgentHead::Root,
        model: Some("provider/model".into()),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: Some(AgentHead::Root),
    };
    let started = tau_proto::AgentPromptStarted {
        model_params: Some(tau_proto::ModelParams::default()),
        outer_turn_id: None,

        agent_prompt_id: checkpoint.agent_prompt_id.clone(),
        agent_id: agent_id(),
        session_id: "session"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        model: checkpoint.model.clone().expect("model"),
        operation: tau_proto::PromptOperation::Inference,
        originator: PromptOriginator::User,
        ctx_id: None,
    };

    assert!(
        validation_error(&tree, Event::AgentPromptStarted(started.clone()))
            .contains("uniquely match")
    );
    tree.validate_event(&Event::AgentInferenceDispatchStarted(checkpoint.clone()))
        .expect("checkpoint owner");
    tree.apply_event(&Event::AgentInferenceDispatchStarted(checkpoint));

    let mut wrong_model = started.clone();
    wrong_model.model = "other/model".into();
    assert!(validation_error(&tree, Event::AgentPromptStarted(wrong_model)).contains("mismatches"));
    assert!(tree.prompt_started_can_materialize(&started));
    let source_error = tree
        .apply_persisted_record(&PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
            seq: tree.next_event_seq(),
            source: Some(PersistedEventSource::Connection(test_connection_id(
                "external-author",
            ))),
            event: Event::AgentPromptStarted(started.clone()),
            parent: AgentEventParent::InheritHead,
            fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
            recorded_at: tau_proto::UnixMicros::new(1),
        })
        .expect_err("compact facts must be harness-authored");
    assert!(source_error.to_string().contains("source-free"));
    tree.validate_event(&Event::AgentPromptStarted(started.clone()))
        .expect("matching compact fact");
    tree.apply_event(&Event::AgentPromptStarted(started.clone()));
    assert_eq!(
        tree.prompt_started(&started.agent_prompt_id),
        Some(&started)
    );
    assert!(tree.prompt_started_is_dispatchable(&started));
    assert!(!tree.prompt_started_can_materialize(&started));
    assert_eq!(tree.ordinary_inference_generation(), 1);
    assert!(validation_error(&tree, Event::AgentPromptStarted(started)).contains("duplicate"));
    assert_eq!(tree.ordinary_inference_generation(), 1);
}

/// Old full prompt records fail strict fold instead of receiving compatibility,
/// migration, deduplication, or precedence treatment.
#[test]
fn persisted_full_prompt_record_is_explicitly_unsupported() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let prompt = tau_proto::AgentPromptCreated {
        agent_prompt_id: "ap-legacy-full"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: agent_id(),
        session_id: "session"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        system_prompt: "legacy full body".to_owned(),
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
        operation: tau_proto::PromptOperation::Inference,
    };

    let error = tree
        .apply_persisted_record(&PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
            seq: PersistedAgentEventSeq::new(0),
            source: None,
            event: Event::AgentPromptCreated(prompt),
            parent: AgentEventParent::InheritHead,
            fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
            recorded_at: tau_proto::UnixMicros::new(1),
        })
        .expect_err("cold fold must reject old full prompt records");
    assert!(
        error
            .to_string()
            .contains("discard or reset this agent journal")
    );
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
            self_compaction_terminal: None,
            agent_id: agent_id(),
            text: text.to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::Internal,
            internal_kind: None,
            inference_activation: false,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
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

/// Builds a validated connection identifier used by this test module.
fn test_connection_id(value: impl AsRef<str>) -> tau_proto::ConnectionId {
    tau_proto::ConnectionId::parse(value.as_ref())
        .expect("test connection id must satisfy the identifier grammar")
}
