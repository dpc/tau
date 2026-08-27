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
            agent_id: agent_id.clone(),
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
            output_length_continuation: None,
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
            automatic_compaction_decision: None,
            agent_id: agent_id.clone(),
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
        automatic_compaction_decision: None,
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
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: None,
        originator: PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
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

fn install_provider_evidence(
    tree: &mut AgentTree,
    prompt: &str,
    response_node: NodeId,
    input_tokens: Option<tau_proto::TokenCount>,
) -> tau_proto::AgentPromptId {
    let prompt_id = tau_proto::AgentPromptId::parse(prompt).expect("valid prompt id");
    tree.inference_dispatch_order.push(prompt_id.clone());
    tree.inference_dispatches.insert(
        prompt_id.clone(),
        InferenceDispatchFold {
            checkpoint: tau_proto::AgentInferenceDispatchStarted {
                agent_id: agent_id(),
                transaction_id: None,
                agent_prompt_id: prompt_id.clone(),
                through: AgentHead::Node(response_node),
                model: Some("provider/model".into()),
                operation: Some(tau_proto::PromptOperation::Inference),
                activation_cut: None,
                output_length_continuation: None,
            },
            fold_semantics: AgentJournalFoldSemantics::InferenceDeferredInputV1,
            head_move_generation: tree.head_move_generation,
            finished: true,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            provider_attempt: Some(tau_proto::ProviderAttempt::ONE),
            provider_stop_reason: Some(tau_proto::ProviderStopReason::EndTurn),
            provider_input_tokens: input_tokens,
            rearms_output_length: false,
            output_length_plan_node: None,
            output_length_steer_node: None,
            response_node: Some(response_node),
        },
    );
    prompt_id
}

#[test]
fn direct_proactive_evidence_requires_newest_ancestral_observation_and_valid_source() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let evidence_head = append_user_input(&mut tree, "evidence");
    let AgentHead::Node(evidence_node) = evidence_head else {
        unreachable!()
    };
    let evidence_prompt = install_provider_evidence(
        &mut tree,
        "ap-exact-evidence",
        evidence_node,
        Some(tau_proto::TokenCount::new(200)),
    );
    let mut started = compaction_start("ct-exact-evidence");
    started.cut = evidence_head;
    started.resume_through = Some(evidence_head);
    started.trigger = tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence {
        evidence: tau_proto::ProactiveCompactionEvidence {
            provider_prompt_id: evidence_prompt,
            provider_input_tokens: tau_proto::TokenCount::new(200),
            threshold: tau_proto::TokenCount::new(100),
            threshold_source: tau_proto::CompactionThresholdSource::ProviderDefault,
        },
    };
    tree.validate_event(&Event::AgentStandaloneCompactionStarted(started.clone()))
        .expect("newest ancestral exact observation");

    let newer_head = append_user_input(&mut tree, "newer missing usage");
    let AgentHead::Node(newer_node) = newer_head else {
        unreachable!()
    };
    install_provider_evidence(&mut tree, "ap-newer-missing", newer_node, None);
    assert!(
        validation_error(
            &tree,
            Event::AgentStandaloneCompactionStarted(started.clone())
        )
        .contains("exact provider evidence")
    );

    let tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence { evidence } =
        &mut started.trigger
    else {
        unreachable!()
    };
    evidence.threshold_source =
        tau_proto::CompactionThresholdSource::NamedPolicies { names: Vec::new() };
    assert!(
        validation_error(&tree, Event::AgentStandaloneCompactionStarted(started))
            .contains("exact provider evidence")
    );
}

/// Same-head ancestry must avoid materializing an arbitrarily long selected
/// branch, while sibling and unknown heads retain their existing answers.
#[test]
fn same_current_head_ancestry_skips_branch_materialization() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let branch_point = append_user_input(&mut tree, "branch point");
    let original_branch = append_user_input(&mut tree, "original branch");
    tree.apply_event(&Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
        agent_id: agent_id(),
        head: branch_point,
    }));
    let sibling = append_user_input(&mut tree, "sibling");
    for index in 0..1_024 {
        append_user_input(&mut tree, &format!("long branch {index}"));
    }
    let current = AgentHead::Node(tree.head().expect("current head"));

    BRANCH_PATH_MATERIALIZATIONS.set(0);
    assert!(tree.is_ancestor_head(current, current));
    assert_eq!(
        BRANCH_PATH_MATERIALIZATIONS.get(),
        0,
        "same-head ancestry must not materialize the selected branch"
    );

    assert!(!tree.is_ancestor_head(original_branch, current));
    assert!(tree.is_ancestor_head(sibling, current));
    let unknown = AgentHead::Node(NodeId::new(usize::MAX as u64));
    assert!(!tree.is_ancestor_head(unknown, unknown));
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
            context_retreat: None,
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
            automatic_compaction_decision: None,
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
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            usage: None,
            originator: PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
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
            original_input_tokens: None,
            compaction_output_tokens: None,
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
            automatic_compaction_decision: None,
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
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            usage: None,
            originator: PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
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
    equal.supersedes = Some(failed.transaction_id.clone());
    equal_tree
        .validate_event(&Event::AgentStandaloneCompactionStarted(equal))
        .expect("equal retry remains valid");
    let mut wrong_model = compaction_start("ct-wrong-model");
    wrong_model.model = "other/model".into();
    wrong_model.cut = equal_cut;
    wrong_model.resume_through = Some(equal_cut);
    wrong_model.supersedes = Some(failed.transaction_id.clone());
    assert!(
        validation_error(
            &equal_tree,
            Event::AgentStandaloneCompactionStarted(wrong_model)
        )
        .contains("latest matching unresolved failure")
    );
    let mut later_failed = compaction_start("ct-later-failed");
    later_failed.cut = equal_cut;
    later_failed.resume_through = Some(equal_cut);
    fail_compaction(&mut equal_tree, &later_failed);
    let mut obsolete = compaction_start("ct-obsolete-predecessor");
    obsolete.cut = equal_cut;
    obsolete.resume_through = Some(equal_cut);
    obsolete.supersedes = Some(failed.transaction_id);
    assert!(
        validation_error(
            &equal_tree,
            Event::AgentStandaloneCompactionStarted(obsolete)
        )
        .contains("latest matching unresolved failure")
    );
    let mut automatic_supersession = compaction_start("ct-automatic-supersession");
    automatic_supersession.cut = equal_cut;
    automatic_supersession.resume_through = Some(equal_cut);
    automatic_supersession.supersedes = Some(later_failed.transaction_id);
    automatic_supersession.trigger = tau_proto::StandaloneCompactionTrigger::AutomaticThreshold;
    assert!(
        validation_error(
            &equal_tree,
            Event::AgentStandaloneCompactionStarted(automatic_supersession)
        )
        .contains("only an explicit manual compaction")
    );

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
    assert!({
        let error = validation_error(
            &sibling_tree,
            Event::AgentStandaloneCompactionStarted(sibling_start),
        );
        error.contains("preserve or retreat")
            || error.contains("latest matching unresolved failure")
    });

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
        original_input_tokens: None,
        compaction_output_tokens: None,
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
    let error = validation_error(
        &successful_tree,
        Event::AgentStandaloneCompactionStarted(supersedes_success),
    );
    assert!(
        error.contains("only a failed transaction")
            || error.contains("explicitly linked continuation"),
        "{error}"
    );
}

/// Automatic context retreat must claim the exact successor pre-minted by the
/// failed transaction; neither an equal cut nor a rewritten successor is valid.
#[test]
fn automatic_context_retreat_claims_exact_strict_predecessor_plan() {
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let skipped = append_user_input(&mut tree, "skipped");
    let predecessor = append_user_input(&mut tree, "predecessor");
    let rejected_cut = append_user_input(&mut tree, "rejected");
    let mut rejected = compaction_start("ct-context-rejected");
    rejected.cut = rejected_cut;
    rejected.resume_through = Some(rejected_cut);
    rejected.trigger = tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence {
        evidence: tau_proto::ProactiveCompactionEvidence {
            provider_prompt_id: "ap-provider-evidence".parse().expect("valid prompt id"),
            provider_input_tokens: tau_proto::TokenCount::new(2),
            threshold: tau_proto::TokenCount::new(1),
            threshold_source: tau_proto::CompactionThresholdSource::ProviderDefault,
        },
    };
    tree.apply_event(&Event::AgentStandaloneCompactionStarted(rejected.clone()));
    tree.apply_event(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        agent_prompt_id: rejected.compact_prompt_id.clone(),
        agent_id: agent_id(),
        session_id: tau_proto::SessionId::parse("session").expect("session"),
        model: rejected.model.clone(),
        model_params: Some(tau_proto::ModelParams::default()),
        outer_turn_id: None,
        operation: tau_proto::PromptOperation::StandaloneCompaction,
        originator: rejected.originator.clone(),
        ctx_id: None,
    }));
    tree.apply_event(&Event::AgentInferenceDispatchStarted(
        tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_id(),
            transaction_id: Some(rejected.transaction_id.clone()),
            agent_prompt_id: rejected.compact_prompt_id.clone(),
            through: rejected.cut,
            model: Some(rejected.model.clone()),
            operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
            activation_cut: None,
            output_length_continuation: None,
        },
    ));
    let mut provider_rejection =
        tool_calling_response(&agent_id(), rejected.compact_prompt_id.as_str(), Vec::new());
    provider_rejection.stop_reason = tau_proto::ProviderStopReason::Error;
    provider_rejection.failure_kind = Some(tau_proto::ProviderFailureKind::ContextWindowExceeded);
    tree.apply_event(&Event::ProviderResponseFinished(provider_rejection));
    assert_eq!(
        tree.head().map(AgentHead::Node),
        Some(rejected_cut),
        "standalone rejection evidence must not enter the provider transcript"
    );
    assert!(matches!(
        tree.standalone_compaction_recovery(),
        Some(StandaloneCompactionRecovery::RejectedAwaitingFailure {
            started: ref projected,
            ..
        }) if projected.transaction_id == rejected.transaction_id
    ));
    let plan = tau_proto::ContextRetreatPlan {
        transaction_id: tau_proto::CompactionTransactionId::parse("ct-context-retreat")
            .expect("valid transaction id"),
        compact_prompt_id: "ap-context-retreat".parse().expect("valid prompt id"),
        cut: predecessor,
        roll_through: rejected_cut,
        model: rejected.model.clone(),
        originator: rejected.originator.clone(),
        resume_through: rejected.resume_through,
    };
    let failure = tau_proto::AgentStandaloneCompactionFailed {
        agent_id: agent_id(),
        transaction_id: rejected.transaction_id.clone(),
        cut: rejected.cut,
        reason: tau_proto::StandaloneCompactionFailureReason::ContextWindowExceeded,
        resume_through: rejected.resume_through,
        context_retreat: Some(plan.clone()),
    };
    let mut skipped_failure = failure.clone();
    skipped_failure.context_retreat.as_mut().expect("plan").cut = skipped;
    let skipped_error = validation_error(
        &tree,
        Event::AgentStandaloneCompactionFailed(skipped_failure),
    );
    assert!(
        skipped_error.contains("strict predecessor"),
        "{skipped_error}"
    );
    let mut rewritten_target = failure.clone();
    rewritten_target
        .context_retreat
        .as_mut()
        .expect("plan")
        .roll_through = predecessor;
    assert!(
        validation_error(
            &tree,
            Event::AgentStandaloneCompactionFailed(rewritten_target)
        )
        .contains("strict predecessor")
    );
    tree.validate_event(&Event::AgentStandaloneCompactionFailed(failure.clone()))
        .expect("canonical provider rejection authorizes the planned failure");
    tree.apply_event(&Event::AgentStandaloneCompactionFailed(failure));
    assert!(matches!(
        tree.standalone_compaction_recovery(),
        Some(StandaloneCompactionRecovery::AwaitingContextRetreat {
            failed: ref projected_failure,
            plan: ref projected_plan,
        }) if projected_failure.transaction_id == rejected.transaction_id
            && projected_plan == &plan
    ));
    let successor = tau_proto::AgentStandaloneCompactionStarted {
        compact_prompt_id: plan.compact_prompt_id,
        operation: tau_proto::PromptOperation::StandaloneCompaction,
        agent_id: agent_id(),
        transaction_id: plan.transaction_id,
        cut: plan.cut,
        resume_through: plan.resume_through,
        model: plan.model,
        originator: plan.originator,
        supersedes: Some(rejected.transaction_id.clone()),
        trigger: tau_proto::StandaloneCompactionTrigger::AutomaticContextRetreat {
            failed_transaction_id: rejected.transaction_id,
            roll_through: plan.roll_through,
        },
    };
    tree.validate_event(&Event::AgentStandaloneCompactionStarted(successor.clone()))
        .expect("exact strict-predecessor successor is valid");
    tree.apply_event(&Event::AgentStandaloneCompactionStarted(successor.clone()));
    let successor_transaction_id = successor.transaction_id.clone();
    let compacted = tau_proto::AgentCompacted {
        original_input_tokens: None,
        compaction_output_tokens: None,
        agent_id: agent_id(),
        replacement_window: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "retreated summary".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        transaction_id: Some(successor_transaction_id.clone()),
        cut: Some(successor.cut),
        suffix_end: Some(rejected_cut),
        compact_prompt_id: Some(successor.compact_prompt_id),
        model: Some(successor.model),
        operation: Some(successor.operation),
    };
    tree.validate_event(&Event::AgentCompacted(compacted.clone()))
        .expect("retreated successor success is valid");
    tree.apply_event(&Event::AgentCompacted(compacted));
    assert_eq!(
        tree.reactive_compaction_progress(&successor_transaction_id),
        Some(ReactiveCompactionProgress::NeedsContinuation {
            target_cut: rejected_cut
        }),
        "successful retreat must roll forward to the immutable recovery target"
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
        original_input_tokens: None,
        compaction_output_tokens: None,
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
    let successful_head = AgentHead::Node(tree.head().expect("successful boundary"));
    assert!(
        tree.unresolved_standalone_compaction_failure(&successor.model, successful_head)
            .is_none(),
        "successful exact successor clears its failed authority chain"
    );
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
        output_length_continuation: None,
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
            automatic_compaction_decision: None,
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
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            originator: PromptOriginator::User,
            usage: None,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
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
        output_length_continuation: None,
    };
    tree.validate_event(&Event::AgentInferenceDispatchStarted(checkpoint.clone()))
        .expect("ordinary checkpoint is valid");
    tree.apply_event(&Event::AgentInferenceDispatchStarted(checkpoint.clone()));
    let response = tau_proto::ProviderResponseFinished {
        automatic_compaction_decision: None,
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
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
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
        output_length_continuation: None,
    };
    let planned_response = tau_proto::ProviderResponseFinished {
        automatic_compaction_decision: None,
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
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
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
                output_length_disposition: tau_proto::OutputLengthDisposition::None,
                provider_attempt: None,
                provider_stop_reason: Some(tau_proto::ProviderStopReason::Error),
                provider_input_tokens: None,
                rearms_output_length: false,
                output_length_plan_node: None,
                output_length_steer_node: None,
                response_node: None,
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
        context_retreat: None,
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
        output_length_continuation: None,
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
        original_input_tokens: None,
        compaction_output_tokens: None,
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
        output_length_continuation: None,
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
        original_input_tokens: None,
        compaction_output_tokens: None,
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
        original_input_tokens: None,
        compaction_output_tokens: None,
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
        original_input_tokens: None,
        compaction_output_tokens: None,
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
            original_input_tokens: None,
            compaction_output_tokens: None,
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
            output_length_continuation: None,
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
                original_input_tokens: None,
                compaction_output_tokens: None,
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

/// A successful automatic pass, its claimed rolling successor, second boundary,
/// and final inference checkpoint must fold identically live and cold.
#[test]
fn automatic_compaction_continuation_chain_matches_live_and_cold_replay() {
    let record = |seq, event| PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
        seq: PersistedAgentEventSeq::new(seq),
        source: None,
        event,
        parent: AgentEventParent::InheritHead,
        fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
        recorded_at: tau_proto::UnixMicros::default(),
    };
    let user = |text: &str| {
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
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
        })
    };
    let mut live = AgentTree::from_events(agent_id(), &[]);
    let mut records = Vec::new();
    for (seq, event) in [user("prefix"), user("suffix")].into_iter().enumerate() {
        let record = record(seq as u64, event);
        live.apply_persisted_record(&record).expect("append input");
        records.push(record);
    }
    let prefix = AgentHead::Node(live.branch_node_ids_from(live.head())[0]);
    let suffix = AgentHead::Node(live.head().expect("suffix head"));
    let mut first = compaction_start("ct-auto-first");
    first.cut = prefix;
    first.resume_through = Some(suffix);
    first.trigger = tau_proto::StandaloneCompactionTrigger::AutomaticThreshold;
    let first_start = record(2, Event::AgentStandaloneCompactionStarted(first.clone()));
    live.apply_persisted_record(&first_start)
        .expect("append first start");
    records.push(first_start);
    let first_boundary_event = Event::AgentCompacted(tau_proto::AgentCompacted {
        original_input_tokens: None,
        compaction_output_tokens: None,
        agent_id: agent_id(),
        replacement_window: vec![ContextItem::Message(tau_proto::MessageItem {
            role: tau_proto::ContextRole::Assistant,
            content: vec![tau_proto::ContentPart::Text {
                text: "summary one".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        transaction_id: Some(first.transaction_id.clone()),
        cut: Some(first.cut),
        suffix_end: Some(suffix),
        compact_prompt_id: Some(first.compact_prompt_id.clone()),
        model: Some(first.model.clone()),
        operation: Some(first.operation),
    });
    let first_boundary_record = record(3, first_boundary_event);
    live.apply_persisted_record(&first_boundary_record)
        .expect("append first boundary");
    records.push(first_boundary_record);
    let first_boundary = AgentHead::Node(live.head().expect("first boundary"));

    let mut unlinked = live.clone();
    let mut invalid_manual = compaction_start("ct-unlinked-manual");
    invalid_manual.compact_prompt_id = "ap-unlinked-manual".parse().expect("prompt id");
    invalid_manual.cut = first_boundary;
    invalid_manual.resume_through = Some(first_boundary);
    assert!(
        unlinked
            .apply_persisted_record(&record(
                4,
                Event::AgentStandaloneCompactionStarted(invalid_manual),
            ))
            .is_err(),
        "an unlinked manual start cannot steal the successful checkpoint"
    );

    let mut sibling = live.clone();
    sibling
        .apply_persisted_record(&record(
            4,
            Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                agent_id: agent_id(),
                head: suffix,
            }),
        ))
        .expect("select sibling base");
    sibling
        .apply_persisted_record(&record(5, user("sibling")))
        .expect("append sibling");
    let sibling_head = AgentHead::Node(sibling.head().expect("sibling head"));
    let mut invalid_sibling = compaction_start("ct-auto-sibling");
    invalid_sibling.compact_prompt_id = "ap-auto-sibling".parse().expect("prompt id");
    invalid_sibling.cut = sibling_head;
    invalid_sibling.resume_through = Some(sibling_head);
    invalid_sibling.trigger = tau_proto::StandaloneCompactionTrigger::AutomaticContinuation {
        previous_transaction_id: first.transaction_id.clone(),
    };
    assert!(
        sibling
            .apply_persisted_record(&record(
                6,
                Event::AgentStandaloneCompactionStarted(invalid_sibling),
            ))
            .is_err(),
        "a sibling without the preceding replacement cannot claim its success"
    );

    let mut second = compaction_start("ct-auto-second");
    second.compact_prompt_id = "ap-auto-second".parse().expect("prompt id");
    second.cut = suffix;
    second.resume_through = Some(first_boundary);
    second.trigger = tau_proto::StandaloneCompactionTrigger::AutomaticContinuation {
        previous_transaction_id: first.transaction_id.clone(),
    };
    let second_start = record(4, Event::AgentStandaloneCompactionStarted(second.clone()));
    live.apply_persisted_record(&second_start)
        .expect("append continuation start");
    records.push(second_start);
    let second_boundary_record = record(
        5,
        Event::AgentCompacted(tau_proto::AgentCompacted {
            original_input_tokens: None,
            compaction_output_tokens: None,
            agent_id: agent_id(),
            replacement_window: vec![ContextItem::Message(tau_proto::MessageItem {
                role: tau_proto::ContextRole::Assistant,
                content: vec![tau_proto::ContentPart::Text {
                    text: "summary two".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
            transaction_id: Some(second.transaction_id.clone()),
            cut: Some(second.cut),
            suffix_end: Some(first_boundary),
            compact_prompt_id: Some(second.compact_prompt_id.clone()),
            model: Some(second.model.clone()),
            operation: Some(second.operation),
        }),
    );
    live.apply_persisted_record(&second_boundary_record)
        .expect("append second boundary");
    records.push(second_boundary_record);
    let second_boundary = AgentHead::Node(live.head().expect("second boundary"));
    let checkpoint = tau_proto::AgentInferenceDispatchStarted {
        agent_id: agent_id(),
        transaction_id: Some(second.transaction_id.clone()),
        agent_prompt_id: "ap-auto-final".parse().expect("prompt id"),
        through: second_boundary,
        model: Some(second.model.clone()),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: Some(second.cut),
        output_length_continuation: None,
    };
    let checkpoint_record = record(6, Event::AgentInferenceDispatchStarted(checkpoint.clone()));
    live.apply_persisted_record(&checkpoint_record)
        .expect("append final checkpoint");
    records.push(checkpoint_record);

    let replay = AgentTree::from_events(agent_id(), &records);
    assert_eq!(live, replay);
    assert_eq!(
        replay.standalone_compaction_recovery(),
        Some(StandaloneCompactionRecovery::DispatchUncertain(checkpoint))
    );
}

/// Reactive progress uses the origin activation's logical provider window, so
/// a final preserved suffix reaches a prior replacement boundary without
/// compacting the rejected activating input. Live append and cold replay must
/// agree, and a continuation beyond that target must fail validation.
#[test]
fn reactive_progress_reaches_prior_suffix_preserving_boundary_live_and_cold() {
    let record = |seq, event| PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
        seq: PersistedAgentEventSeq::new(seq),
        source: None,
        event,
        parent: AgentEventParent::InheritHead,
        fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
        recorded_at: tau_proto::UnixMicros::default(),
    };
    let user = |text: &str, inference_activation| {
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation,
            agent_id: agent_id(),
            text: text.to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        })
    };
    let mut live = AgentTree::from_events(agent_id(), &[]);
    let mut records = Vec::new();
    for event in [user("old prefix", false), user("preserved suffix", false)] {
        let persisted = record(records.len() as u64, event);
        live.apply_persisted_record(&persisted)
            .expect("append history");
        records.push(persisted);
    }
    let ids = live.branch_node_ids_from(live.head());
    let old_prefix = AgentHead::Node(ids[0]);
    let preserved_suffix = AgentHead::Node(ids[1]);
    let mut prior = compaction_start("ct-prior-logical-window");
    prior.cut = old_prefix;
    prior.resume_through = None;
    let persisted = record(
        records.len() as u64,
        Event::AgentStandaloneCompactionStarted(prior.clone()),
    );
    live.apply_persisted_record(&persisted)
        .expect("append prior start");
    records.push(persisted);
    let persisted = record(
        records.len() as u64,
        Event::AgentCompacted(tau_proto::AgentCompacted {
            original_input_tokens: None,
            compaction_output_tokens: None,
            agent_id: agent_id(),
            replacement_window: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "prior summary".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
            transaction_id: Some(prior.transaction_id),
            cut: Some(old_prefix),
            suffix_end: Some(preserved_suffix),
            compact_prompt_id: Some(prior.compact_prompt_id),
            model: Some(prior.model),
            operation: Some(prior.operation),
        }),
    );
    live.apply_persisted_record(&persisted)
        .expect("append prior boundary");
    records.push(persisted);
    let activation_cut = AgentHead::Node(live.head().expect("prior boundary"));
    let persisted = record(records.len() as u64, user("retained activation", true));
    live.apply_persisted_record(&persisted)
        .expect("append activation");
    records.push(persisted);
    let through = AgentHead::Node(live.head().expect("activation"));
    let failed_prompt_id = "ap-reactive-prior-boundary"
        .parse::<tau_proto::AgentPromptId>()
        .expect("prompt id");
    let checkpoint = tau_proto::AgentInferenceDispatchStarted {
        agent_id: agent_id(),
        transaction_id: None,
        agent_prompt_id: failed_prompt_id.clone(),
        through,
        model: Some("provider/model".into()),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: Some(activation_cut),
        output_length_continuation: None,
    };
    let persisted = record(
        records.len() as u64,
        Event::AgentInferenceDispatchStarted(checkpoint),
    );
    live.apply_persisted_record(&persisted)
        .expect("append rejected checkpoint");
    records.push(persisted);
    let persisted = record(
        records.len() as u64,
        Event::ProviderResponseFinished(tau_proto::ProviderResponseFinished {
            automatic_compaction_decision: None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,
            agent_prompt_id: failed_prompt_id.clone(),
            agent_id: agent_id(),
            output_items: Vec::new(),
            stop_reason: tau_proto::ProviderStopReason::Error,
            error: Some("bounded".to_owned()),
            failure_kind: Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            originator: PromptOriginator::User,
            usage: None,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );
    live.apply_persisted_record(&persisted)
        .expect("append rejection");
    records.push(persisted);
    let mut reactive = compaction_start("ct-reactive-prior-boundary");
    reactive.cut = preserved_suffix;
    reactive.resume_through = Some(through);
    reactive.trigger = tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
        failed_agent_prompt_id: failed_prompt_id,
    };
    let persisted = record(
        records.len() as u64,
        Event::AgentStandaloneCompactionStarted(reactive.clone()),
    );
    live.apply_persisted_record(&persisted)
        .expect("append reactive start");
    records.push(persisted);
    let suffix_end = AgentHead::Node(live.head().expect("reactive suffix end"));
    let persisted = record(
        records.len() as u64,
        Event::AgentCompacted(tau_proto::AgentCompacted {
            original_input_tokens: None,
            compaction_output_tokens: None,
            agent_id: agent_id(),
            replacement_window: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "reactive summary".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
            transaction_id: Some(reactive.transaction_id.clone()),
            cut: Some(reactive.cut),
            suffix_end: Some(suffix_end),
            compact_prompt_id: Some(reactive.compact_prompt_id.clone()),
            model: Some(reactive.model.clone()),
            operation: Some(reactive.operation),
        }),
    );
    live.apply_persisted_record(&persisted)
        .expect("append reactive boundary");
    records.push(persisted);

    let replay = AgentTree::from_events(agent_id(), &records);
    for tree in [&live, &replay] {
        assert_eq!(
            tree.reactive_compaction_progress(&reactive.transaction_id),
            Some(ReactiveCompactionProgress::ReachedTargetCut)
        );
    }
    let current = AgentHead::Node(live.head().expect("reactive boundary"));
    let mut beyond = compaction_start("ct-reactive-beyond-target");
    beyond.cut = through;
    beyond.resume_through = Some(current);
    beyond.trigger = tau_proto::StandaloneCompactionTrigger::AutomaticContinuation {
        previous_transaction_id: reactive.transaction_id,
    };
    assert!(
        live.validate_event(&Event::AgentStandaloneCompactionStarted(beyond))
            .is_err(),
        "a durable continuation cannot compact the rejected activation"
    );
}

/// A successful idle compaction with no resume watermark owes no checkpoint
/// and cannot prevent a later independent compaction transaction.
#[test]
fn no_resume_compaction_success_allows_later_independent_start() {
    let record = |seq, event| PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
        seq: PersistedAgentEventSeq::new(seq),
        source: None,
        event,
        parent: AgentEventParent::InheritHead,
        fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
        recorded_at: tau_proto::UnixMicros::default(),
    };
    let mut tree = AgentTree::from_events(agent_id(), &[]);
    let mut first = compaction_start("ct-idle-first");
    first.resume_through = None;
    tree.apply_persisted_record(&record(
        0,
        Event::AgentStandaloneCompactionStarted(first.clone()),
    ))
    .expect("append idle start");
    tree.apply_persisted_record(&record(
        1,
        Event::AgentCompacted(tau_proto::AgentCompacted {
            original_input_tokens: None,
            compaction_output_tokens: None,
            agent_id: agent_id(),
            replacement_window: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "idle summary".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
            transaction_id: Some(first.transaction_id),
            cut: Some(AgentHead::Root),
            suffix_end: Some(AgentHead::Root),
            compact_prompt_id: Some(first.compact_prompt_id),
            model: Some(first.model),
            operation: Some(first.operation),
        }),
    ))
    .expect("append idle success");
    let boundary = AgentHead::Node(tree.head().expect("idle boundary"));
    let mut later = compaction_start("ct-idle-later");
    later.compact_prompt_id = "ap-idle-later".parse().expect("prompt id");
    later.cut = boundary;
    later.resume_through = None;
    tree.apply_persisted_record(&record(2, Event::AgentStandaloneCompactionStarted(later)))
        .expect("independent start after no-resume success");
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
            output_length_continuation: None,
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
                automatic_compaction_decision: None,
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
                output_length_disposition: tau_proto::OutputLengthDisposition::None,
                usage: None,
                originator: PromptOriginator::User,
                compaction_original_input_tokens: None,
                compaction_output_tokens: None,
                backend: None,
                provider_attempt: Default::default(),
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
                display: None,
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
            output_length_continuation: None,
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
                output_length_continuation: None,
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
                original_input_tokens: None,
                compaction_output_tokens: None,
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
            output_length_continuation: None,
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
            output_length_continuation: None,
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
                output_length_continuation: None,
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
                    automatic_compaction_decision: None,
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
            output_length_continuation: None,
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
            output_length_continuation: None,
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
            automatic_compaction_decision: None,
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
        automatic_compaction_decision: None,
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
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: None,
        originator: PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
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
            Event::HarnessNotice(tau_proto::HarnessNotice::diagnostic(
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
        output_length_continuation: None,
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
        output_length_continuation: None,
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

/// Complete, valid source → plan → steer → owner → prompt-start → terminal
/// journal records shared by focused output-length recovery regressions.
struct OutputLengthRecoveryFixture {
    /// Durable agent whose journal owns every fixture record.
    agent_id: tau_proto::AgentId,
    /// Reserved successor dispatch used by ownership assertions.
    owner: tau_proto::AgentInferenceDispatchStarted,
    /// Complete valid journal, ordered through the successor terminal.
    records: Vec<PersistedAgentEvent>,
    /// Transcript head created by the reserved continuation steer.
    steer_node: NodeId,
    /// Transcript head created by the completed successor response.
    terminal_node: NodeId,
}

#[derive(Clone, Copy)]
/// Inclusive durable journal cut exposed by the shared recovery fixture.
enum OutputLengthFixturePhase {
    /// Source response durably reserves the successor.
    Plan,
    /// Exact internal steer has committed.
    Steer,
    /// Reserved successor owner has committed.
    Owner,
    /// Reserved successor prompt-start has committed.
    PromptStart,
    /// Reserved successor terminal has committed.
    Terminal,
}

impl OutputLengthRecoveryFixture {
    fn through(&self, phase: OutputLengthFixturePhase) -> &[PersistedAgentEvent] {
        let phase_index = self
            .records
            .iter()
            .position(|record| match (&record.event, phase) {
                (Event::ProviderResponseFinished(response), OutputLengthFixturePhase::Plan) => {
                    matches!(
                        response.output_length_disposition,
                        tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
                    )
                }
                (Event::AgentPromptSteered(_), OutputLengthFixturePhase::Steer) => true,
                (Event::AgentInferenceDispatchStarted(owner), OutputLengthFixturePhase::Owner) => {
                    owner.output_length_continuation.is_some()
                }
                (Event::AgentPromptStarted(started), OutputLengthFixturePhase::PromptStart) => {
                    started.agent_prompt_id == self.owner.agent_prompt_id
                }
                (Event::ProviderResponseFinished(response), OutputLengthFixturePhase::Terminal) => {
                    matches!(
                        response.output_length_disposition,
                        tau_proto::OutputLengthDisposition::ContinuationTerminal { .. }
                    )
                }
                _ => false,
            })
            .expect("fixture contains requested output-length phase");
        &self.records[..=phase_index]
    }

    fn steer(&self) -> tau_proto::AgentPromptSteered {
        self.records
            .iter()
            .find_map(|record| match &record.event {
                Event::AgentPromptSteered(steer) => Some(steer.clone()),
                _ => None,
            })
            .expect("fixture steer phase")
    }

    fn source_response(&self) -> tau_proto::ProviderResponseFinished {
        self.records
            .iter()
            .find_map(|record| match &record.event {
                Event::ProviderResponseFinished(response)
                    if matches!(
                        response.output_length_disposition,
                        tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
                    ) =>
                {
                    Some(response.clone())
                }
                _ => None,
            })
            .expect("fixture plan phase")
    }

    fn source_dispatch_record(&self) -> PersistedAgentEvent {
        self.records
            .iter()
            .find(|record| {
                matches!(
                    &record.event,
                    Event::AgentInferenceDispatchStarted(owner)
                        if owner.output_length_continuation.is_none()
                )
            })
            .cloned()
            .expect("fixture source dispatch")
    }

    fn outer_turn_started(&self) -> tau_proto::AgentOuterTurnStarted {
        self.records
            .iter()
            .find_map(|record| match &record.event {
                Event::AgentOuterTurnStarted(started) => Some(started.clone()),
                _ => None,
            })
            .expect("fixture outer-turn start")
    }

    fn source_prompt_started(&self) -> tau_proto::AgentPromptStarted {
        let source_prompt_id = self
            .owner
            .output_length_continuation
            .as_ref()
            .expect("fixture continuation owner")
            .source_agent_prompt_id
            .clone();
        self.records
            .iter()
            .find_map(|record| match &record.event {
                Event::AgentPromptStarted(started)
                    if started.agent_prompt_id == source_prompt_id =>
                {
                    Some(started.clone())
                }
                _ => None,
            })
            .expect("fixture source prompt-start")
    }

    fn owner(&self) -> &tau_proto::AgentInferenceDispatchStarted {
        &self.owner
    }

    fn agent_id(&self) -> &tau_proto::AgentId {
        &self.agent_id
    }

    const fn steer_head(&self) -> AgentHead {
        AgentHead::Node(self.steer_node)
    }

    const fn terminal_node(&self) -> NodeId {
        self.terminal_node
    }

    fn source_dispatch_in<'a>(
        &self,
        records: &'a mut [PersistedAgentEvent],
    ) -> &'a mut PersistedAgentEvent {
        let source_prompt_id = &self
            .owner
            .output_length_continuation
            .as_ref()
            .expect("fixture continuation owner")
            .source_agent_prompt_id;
        records
            .iter_mut()
            .find(|record| {
                matches!(
                    &record.event,
                    Event::AgentInferenceDispatchStarted(owner)
                        if owner.agent_prompt_id == *source_prompt_id
                            && owner.output_length_continuation.is_none()
                )
            })
            .expect("source dispatch record")
    }

    fn source_prompt_start_in<'a>(
        &self,
        records: &'a mut [PersistedAgentEvent],
    ) -> &'a mut tau_proto::AgentPromptStarted {
        let source_prompt_id = &self
            .owner
            .output_length_continuation
            .as_ref()
            .expect("fixture continuation owner")
            .source_agent_prompt_id;
        records
            .iter_mut()
            .find_map(|record| match &mut record.event {
                Event::AgentPromptStarted(started)
                    if started.agent_prompt_id == *source_prompt_id =>
                {
                    Some(started)
                }
                _ => None,
            })
            .expect("source prompt-start record")
    }

    fn steer_in<'a>(
        &self,
        records: &'a mut [PersistedAgentEvent],
    ) -> &'a mut tau_proto::AgentPromptSteered {
        records
            .iter_mut()
            .find_map(|record| match &mut record.event {
                Event::AgentPromptSteered(steer)
                    if steer.agent_id == self.agent_id
                        && steer.internal_kind
                            == Some(tau_proto::InternalPromptKind::OutputLengthContinuation) =>
                {
                    Some(steer)
                }
                _ => None,
            })
            .expect("continuation steer record")
    }

    fn owner_in<'a>(
        &self,
        records: &'a mut [PersistedAgentEvent],
    ) -> &'a mut tau_proto::AgentInferenceDispatchStarted {
        records
            .iter_mut()
            .find_map(|record| match &mut record.event {
                Event::AgentInferenceDispatchStarted(owner)
                    if owner.agent_prompt_id == self.owner.agent_prompt_id
                        && owner.output_length_continuation.is_some() =>
                {
                    Some(owner)
                }
                _ => None,
            })
            .expect("continuation owner record")
    }

    fn terminal_in<'a>(
        &self,
        records: &'a mut [PersistedAgentEvent],
    ) -> &'a mut tau_proto::ProviderResponseFinished {
        records
            .iter_mut()
            .find_map(|record| match &mut record.event {
                Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == self.owner.agent_prompt_id
                        && matches!(
                            response.output_length_disposition,
                            tau_proto::OutputLengthDisposition::ContinuationTerminal { .. }
                        ) =>
                {
                    Some(response)
                }
                _ => None,
            })
            .expect("continuation terminal record")
    }
}

fn output_length_record(
    seq: u64,
    parent: AgentEventParent,
    event: Event,
    fold_semantics: AgentJournalFoldSemantics,
) -> PersistedAgentEvent {
    PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([seq as u8; 16]),
        seq: PersistedAgentEventSeq::new(seq),
        source: None,
        event,
        parent,
        fold_semantics,
        recorded_at: tau_proto::UnixMicros::new(seq),
    }
}

fn output_length_steer(agent_id: tau_proto::AgentId) -> tau_proto::AgentPromptSteered {
    tau_proto::AgentPromptSteered {
        agent_id,
        inference_activation: true,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        text: tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION.to_owned(),
        trusted_internal_spans: vec![tau_proto::TrustedInternalSpan {
            start: 0,
            end: u32::try_from(tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION.len())
                .expect("bounded instruction"),
        }],
        message_class: tau_proto::PromptMessageClass::Internal,
        self_compaction_terminal: None,
        internal_kind: Some(tau_proto::InternalPromptKind::OutputLengthContinuation),
        ctx_id: None,
    }
}

fn output_length_recovery_fixture() -> OutputLengthRecoveryFixture {
    let agent_id = agent_id();
    let source_prompt_id = tau_proto::AgentPromptId::parse("ap-source").expect("source prompt id");
    let successor_prompt_id =
        tau_proto::AgentPromptId::parse("ap-successor").expect("successor prompt id");
    let outer_turn_id = tau_proto::AgentOuterTurnId::for_prompt(&source_prompt_id);
    let model: tau_proto::ModelId = "provider/model".into();
    let source = tau_proto::AgentInferenceDispatchStarted {
        agent_id: agent_id.clone(),
        transaction_id: None,
        agent_prompt_id: source_prompt_id.clone(),
        through: AgentHead::Root,
        model: Some(model.clone()),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: Some(AgentHead::Root),
        output_length_continuation: None,
    };
    let turn = tau_proto::AgentOuterTurnStarted {
        agent_id: agent_id.clone(),
        session_id: tau_proto::SessionId::parse("session").expect("session id"),
        outer_turn_id: outer_turn_id.clone(),
        agent_prompt_id: source_prompt_id.clone(),
        runtime_id: tau_proto::AccountingRuntimeId::parse("runtime").expect("runtime id"),
        activation: tau_proto::AgentOuterTurnActivation::External {
            correlation_id: tau_proto::AgentActivationCorrelationId::parse("activation")
                .expect("activation id"),
        },
    };
    let source_started = tau_proto::AgentPromptStarted {
        agent_prompt_id: source_prompt_id.clone(),
        agent_id: agent_id.clone(),
        session_id: tau_proto::SessionId::parse("session").expect("session id"),
        model: model.clone(),
        model_params: Some(tau_proto::ModelParams::default()),
        outer_turn_id: Some(outer_turn_id.clone()),
        operation: tau_proto::PromptOperation::Inference,
        originator: PromptOriginator::User,
        ctx_id: None,
    };
    let response = tau_proto::ProviderResponseFinished {
        automatic_compaction_decision: None,
        agent_prompt_id: source_prompt_id.clone(),
        agent_id: agent_id.clone(),
        output_items: vec![ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Full,
            text: "reasoning".to_owned(),
        })],
        stop_reason: tau_proto::ProviderStopReason::Length,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::ContinuationPlanned {
            outer_turn_id: outer_turn_id.clone(),
            successor_agent_prompt_id: successor_prompt_id.clone(),
            ordinal: 1,
            limit: 1,
        },
        originator: PromptOriginator::User,
        usage: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: Some(tau_proto::ProviderBackend {
            kind: tau_proto::ProviderBackendKind::ChatCompletions,
            base_url: "https://example.invalid/v1".to_owned(),
            transport: tau_proto::ProviderBackendTransport::HttpSse,
            stale_chain_fallback: false,
        }),
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    };
    let steer = output_length_steer(agent_id.clone());
    let owner = tau_proto::AgentInferenceDispatchStarted {
        agent_id: agent_id.clone(),
        transaction_id: None,
        agent_prompt_id: successor_prompt_id,
        through: AgentHead::Node(NodeId::new(1)),
        model: Some(model.clone()),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: Some(AgentHead::Root),
        output_length_continuation: Some(tau_proto::OutputLengthContinuationOwner {
            source_agent_prompt_id: source_prompt_id,
            outer_turn_id: outer_turn_id.clone(),
            ordinal: 1,
        }),
    };
    let successor_started = tau_proto::AgentPromptStarted {
        agent_prompt_id: owner.agent_prompt_id.clone(),
        agent_id: agent_id.clone(),
        session_id: tau_proto::SessionId::parse("session").expect("session id"),
        model,
        model_params: Some(tau_proto::ModelParams::default()),
        outer_turn_id: Some(outer_turn_id.clone()),
        operation: tau_proto::PromptOperation::Inference,
        originator: PromptOriginator::User,
        ctx_id: None,
    };
    let terminal = tau_proto::ProviderResponseFinished {
        automatic_compaction_decision: None,
        agent_prompt_id: owner.agent_prompt_id.clone(),
        agent_id: agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "finished".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::ContinuationTerminal {
            outer_turn_id,
            source_agent_prompt_id: owner
                .output_length_continuation
                .as_ref()
                .expect("continuation owner")
                .source_agent_prompt_id
                .clone(),
            ordinal: 1,
            outcome: tau_proto::OutputLengthContinuationOutcome::Completed,
            outer_turn_finish_owed: true,
        },
        originator: PromptOriginator::User,
        usage: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: Some(tau_proto::ProviderBackend {
            kind: tau_proto::ProviderBackendKind::ChatCompletions,
            base_url: "https://example.invalid/v1".to_owned(),
            transport: tau_proto::ProviderBackendTransport::HttpSse,
            stale_chain_fallback: false,
        }),
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    };
    let records = vec![
        output_length_record(
            0,
            AgentEventParent::Root,
            Event::AgentInferenceDispatchStarted(source),
            AgentJournalFoldSemantics::InferenceDeferredInputV1,
        ),
        output_length_record(
            1,
            AgentEventParent::Root,
            Event::AgentOuterTurnStarted(turn),
            AgentJournalFoldSemantics::Legacy,
        ),
        output_length_record(
            2,
            AgentEventParent::Root,
            Event::AgentPromptStarted(source_started),
            AgentJournalFoldSemantics::Legacy,
        ),
        output_length_record(
            3,
            AgentEventParent::Root,
            Event::ProviderResponseFinished(response),
            AgentJournalFoldSemantics::Legacy,
        ),
        output_length_record(
            4,
            AgentEventParent::Under(NodeId::new(0)),
            Event::AgentPromptSteered(steer),
            AgentJournalFoldSemantics::Legacy,
        ),
        output_length_record(
            5,
            AgentEventParent::Under(NodeId::new(1)),
            Event::AgentInferenceDispatchStarted(owner.clone()),
            AgentJournalFoldSemantics::InferenceDeferredInputV1,
        ),
        output_length_record(
            6,
            AgentEventParent::Under(NodeId::new(1)),
            Event::AgentPromptStarted(successor_started),
            AgentJournalFoldSemantics::Legacy,
        ),
        output_length_record(
            7,
            AgentEventParent::Under(NodeId::new(1)),
            Event::ProviderResponseFinished(terminal),
            AgentJournalFoldSemantics::Legacy,
        ),
    ];
    OutputLengthRecoveryFixture {
        agent_id,
        owner,
        records,
        steer_node: NodeId::new(1),
        terminal_node: NodeId::new(2),
    }
}

/// Live source dispatch, turn, prompt-start, and response folding must project
/// the exact reserved successor rather than deriving authority from cold data.
#[test]
fn output_length_recovery_projects_exact_live_plan() {
    let fixture = output_length_recovery_fixture();
    let mut tree = AgentTree::from_events(fixture.agent_id().clone(), &[]);
    tree.apply_persisted_record(&fixture.source_dispatch_record())
        .expect("marked source dispatch");
    let turn = fixture.outer_turn_started();
    tree.validate_event(&Event::AgentOuterTurnStarted(turn.clone()))
        .expect("outer-turn start");
    tree.apply_event(&Event::AgentOuterTurnStarted(turn));
    let started = fixture.source_prompt_started();
    tree.validate_event(&Event::AgentPromptStarted(started.clone()))
        .expect("source prompt-start");
    tree.apply_event(&Event::AgentPromptStarted(started));
    let response = fixture.source_response();
    tree.validate_event(&Event::ProviderResponseFinished(response.clone()))
        .expect("planned response");
    tree.apply_event(&Event::ProviderResponseFinished(response));
    assert!(matches!(
        tree.output_length_continuation_recovery(),
        Some(super::OutputLengthContinuationRecovery::SteerNeeded {
            successor_agent_prompt_id,
            ..
        }) if successor_agent_prompt_id == fixture.owner().agent_prompt_id
    ));
}

/// Only the approved exact continuation instruction may claim a durable plan;
/// otherwise replay must reject modified bytes.
#[test]
fn output_length_recovery_accepts_new_exact_steer_only() {
    let fixture = output_length_recovery_fixture();
    let tree = AgentTree::try_from_events(
        fixture.agent_id().clone(),
        fixture.through(OutputLengthFixturePhase::Plan),
    )
    .expect("planned cut");
    let mut steer = output_length_steer(fixture.agent_id().clone());
    tree.validate_event(&Event::AgentPromptSteered(steer.clone()))
        .expect("new exact steer");

    steer.text.push('!');
    steer.trusted_internal_spans[0].end =
        u32::try_from(steer.text.len()).expect("bounded malformed instruction");
    assert!(
        tree.validate_event(&Event::AgentPromptSteered(steer))
            .is_err()
    );
}

/// Only a successful ordinary canonical tool-call response may rearm the
/// output-length budget; nearby terminal shapes remain negative boundaries.
#[test]
fn output_length_rearm_boundary_rejects_non_action_responses() {
    let fixture = output_length_recovery_fixture();
    let checkpoint = fixture.source_dispatch_record();
    let Event::AgentInferenceDispatchStarted(checkpoint) = checkpoint.event else {
        unreachable!("fixture source dispatch");
    };
    let mut response = fixture.source_response();
    response.stop_reason = tau_proto::ProviderStopReason::ToolCalls;
    response.output_items = vec![ContextItem::ToolCall(tau_proto::ToolCallItem {
        call_id: "rearm-call".into(),
        name: tau_proto::ToolName::new("step"),
        tool_type: tau_proto::ToolType::Function,
        arguments: ciborium::Value::Map(Vec::new()),
        raw_arguments_json: None,
        responses_envelope: None,
    })];
    assert!(super::output_length_response_rearms_budget(
        &checkpoint,
        &response
    ));
    let mut end_turn_with_call = response.clone();
    end_turn_with_call.stop_reason = tau_proto::ProviderStopReason::EndTurn;
    assert!(super::output_length_response_rearms_budget(
        &checkpoint,
        &end_turn_with_call
    ));

    let mut empty_stop = response.clone();
    empty_stop.output_items.clear();
    assert!(!super::output_length_response_rearms_budget(
        &checkpoint,
        &empty_stop
    ));
    let mut truncated_call = response.clone();
    truncated_call.stop_reason = tau_proto::ProviderStopReason::Length;
    assert!(!super::output_length_response_rearms_budget(
        &checkpoint,
        &truncated_call
    ));
    let mut failed = response.clone();
    failed.failure_kind = Some(tau_proto::ProviderFailureKind::Unknown);
    assert!(!super::output_length_response_rearms_budget(
        &checkpoint,
        &failed
    ));
    let mut compact_checkpoint = checkpoint.clone();
    compact_checkpoint.transaction_id =
        Some(tau_proto::CompactionTransactionId::parse("ct-rearm").expect("transaction id"));
    assert!(!super::output_length_response_rearms_budget(
        &compact_checkpoint,
        &response
    ));

    let mut tree = AgentTree::try_from_events(
        fixture.agent_id().clone(),
        fixture.through(OutputLengthFixturePhase::Terminal),
    )
    .expect("spent selected lineage");
    let mut off_branch_checkpoint = checkpoint;
    off_branch_checkpoint.agent_prompt_id =
        tau_proto::AgentPromptId::parse("ap-off-branch-action").expect("prompt id");
    tree.inference_dispatch_order
        .push(off_branch_checkpoint.agent_prompt_id.clone());
    tree.inference_dispatches.insert(
        off_branch_checkpoint.agent_prompt_id.clone(),
        InferenceDispatchFold {
            checkpoint: off_branch_checkpoint,
            fold_semantics: AgentJournalFoldSemantics::InferenceDeferredInputV1,
            head_move_generation: tree.head_move_generation,
            finished: true,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            provider_attempt: Some(tau_proto::ProviderAttempt::ONE),
            provider_stop_reason: Some(tau_proto::ProviderStopReason::ToolCalls),
            provider_input_tokens: None,
            rearms_output_length: true,
            output_length_plan_node: None,
            output_length_steer_node: None,
            response_node: Some(NodeId::new(999)),
        },
    );
    assert_eq!(
        tree.output_length_budget_spent_outer_turn(),
        Some(fixture.outer_turn_started().outer_turn_id)
    );
}

/// Cold plan and steer cuts project only their exact missing fact, while owner
/// and prompt-start cuts conservatively suppress resend.
#[test]
fn output_length_recovery_projects_exact_cold_cuts() {
    let fixture = output_length_recovery_fixture();
    let plan_cut = AgentTree::try_from_events(
        fixture.agent_id().clone(),
        fixture.through(OutputLengthFixturePhase::Plan),
    )
    .expect("cold plan cut");
    assert!(matches!(
        plan_cut.output_length_continuation_recovery(),
        Some(super::OutputLengthContinuationRecovery::SteerNeeded {
            successor_agent_prompt_id,
            ..
        }) if successor_agent_prompt_id == fixture.owner().agent_prompt_id
    ));

    let mut plan_cut = plan_cut;
    let steer = fixture.steer();
    plan_cut
        .validate_event(&Event::AgentPromptSteered(steer.clone()))
        .expect("exact steer");
    plan_cut.apply_event(&Event::AgentPromptSteered(steer.clone()));
    assert!(
        plan_cut
            .validate_event(&Event::AgentPromptSteered(steer))
            .is_err()
    );
    let through = match plan_cut.output_length_continuation_recovery() {
        Some(super::OutputLengthContinuationRecovery::OwnerNeeded { through, .. }) => through,
        recovery => panic!("expected owner repair, got {recovery:?}"),
    };
    assert_eq!(through, fixture.steer_head());

    let steer_cut = AgentTree::try_from_events(
        fixture.agent_id().clone(),
        fixture.through(OutputLengthFixturePhase::Steer),
    )
    .expect("cold steer cut");
    assert!(matches!(
        steer_cut.output_length_continuation_recovery(),
        Some(super::OutputLengthContinuationRecovery::OwnerNeeded {
            through,
            ..
        }) if through == fixture.steer_head()
    ));
    for (name, records) in [
        ("owner", fixture.through(OutputLengthFixturePhase::Owner)),
        (
            "prompt-start",
            fixture.through(OutputLengthFixturePhase::PromptStart),
        ),
    ] {
        let cut = AgentTree::try_from_events(fixture.agent_id().clone(), records)
            .unwrap_or_else(|error| panic!("cold {name} cut: {error}"));
        assert_eq!(cut.output_length_continuation_recovery(), None);
        assert!(matches!(
            cut.inference_dispatch_recovery(),
            Some(super::InferenceDispatchRecovery::DispatchUncertain(_))
        ));
    }
}

/// Reactive recovery from the reserved successor keeps only recovery authority
/// on that response, then binds the exact transaction-owned descendant back to
/// the original output-length lineage.
#[test]
fn output_length_reactive_descendant_resolves_exact_owner() {
    let fixture = output_length_recovery_fixture();
    let mut tree = AgentTree::try_from_events(
        fixture.agent_id().clone(),
        fixture.through(OutputLengthFixturePhase::PromptStart),
    )
    .expect("reserved successor cut");
    let owner = fixture.owner().clone();
    let continuation = owner
        .output_length_continuation
        .as_ref()
        .expect("continuation owner")
        .clone();
    let rejection = tau_proto::ProviderResponseFinished {
        automatic_compaction_decision: None,
        agent_prompt_id: owner.agent_prompt_id.clone(),
        agent_id: fixture.agent_id().clone(),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::Error,
        error: Some("context overflow".to_owned()),
        failure_kind: Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        provider_attempt: tau_proto::ProviderAttempt::new(3).expect("nonzero attempt"),
        originator: PromptOriginator::User,
        usage: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };
    tree.validate_event(&Event::ProviderResponseFinished(rejection.clone()))
        .expect("reserved context rejection");
    tree.apply_event(&Event::ProviderResponseFinished(rejection));

    let mut started = compaction_start("ct-output-length-reactive");
    started.agent_id = fixture.agent_id().clone();
    started.compact_prompt_id =
        tau_proto::AgentPromptId::parse("ap-output-length-compact").expect("compact prompt id");
    started.resume_through = Some(owner.through);
    started.trigger = tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
        failed_agent_prompt_id: owner.agent_prompt_id.clone(),
    };
    tree.validate_event(&Event::AgentStandaloneCompactionStarted(started.clone()))
        .expect("reactive compaction start");
    tree.apply_event(&Event::AgentStandaloneCompactionStarted(started.clone()));
    let compact_parent = AgentHead::Node(tree.head().expect("rejected response node"));
    let compacted = tau_proto::AgentCompacted {
        original_input_tokens: None,
        compaction_output_tokens: None,
        agent_id: fixture.agent_id().clone(),
        transaction_id: Some(started.transaction_id.clone()),
        cut: Some(started.cut),
        suffix_end: Some(compact_parent),
        compact_prompt_id: Some(started.compact_prompt_id.clone()),
        model: Some(started.model.clone()),
        operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
        replacement_window: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "summary".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
    };
    tree.validate_event(&Event::AgentCompacted(compacted.clone()))
        .expect("reactive compaction success");
    tree.apply_event(&Event::AgentCompacted(compacted));
    let descendant_prompt_id =
        tau_proto::AgentPromptId::parse("ap-output-length-descendant").expect("descendant id");
    let descendant = tau_proto::AgentInferenceDispatchStarted {
        agent_id: fixture.agent_id().clone(),
        transaction_id: Some(started.transaction_id),
        agent_prompt_id: descendant_prompt_id.clone(),
        through: AgentHead::Node(tree.head().expect("compaction node")),
        model: Some(started.model),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: Some(started.cut),
        output_length_continuation: None,
    };
    let mut unrelated = descendant.clone();
    unrelated.agent_prompt_id =
        tau_proto::AgentPromptId::parse("ap-unrelated-descendant").expect("unrelated id");
    unrelated.transaction_id = None;
    tree.apply_event(&Event::AgentInferenceDispatchStarted(unrelated.clone()));
    assert_eq!(
        tree.output_length_lineage_owner_for_prompt(&unrelated.agent_prompt_id),
        None
    );
    tree.validate_event(&Event::AgentInferenceDispatchStarted(descendant.clone()))
        .expect("exact post-compaction descendant");
    tree.apply_event(&Event::AgentInferenceDispatchStarted(descendant.clone()));
    assert_eq!(
        tree.output_length_lineage_owner_for_prompt(&descendant_prompt_id),
        Some(continuation.clone())
    );
    let descendant_started = tau_proto::AgentPromptStarted {
        agent_prompt_id: descendant_prompt_id.clone(),
        agent_id: fixture.agent_id().clone(),
        session_id: tau_proto::SessionId::parse("session").expect("session id"),
        model: descendant.model.clone().expect("descendant model"),
        model_params: Some(tau_proto::ModelParams::default()),
        outer_turn_id: Some(continuation.outer_turn_id.clone()),
        operation: tau_proto::PromptOperation::Inference,
        originator: PromptOriginator::User,
        ctx_id: None,
    };
    tree.validate_event(&Event::AgentPromptStarted(descendant_started.clone()))
        .expect("descendant prompt-start");
    tree.apply_event(&Event::AgentPromptStarted(descendant_started));
    let terminal = tau_proto::ProviderResponseFinished {
        automatic_compaction_decision: None,
        agent_prompt_id: descendant_prompt_id,
        agent_id: fixture.agent_id().clone(),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::Length,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::ContinuationTerminal {
            outer_turn_id: continuation.outer_turn_id.clone(),
            source_agent_prompt_id: continuation.source_agent_prompt_id,
            ordinal: continuation.ordinal,
            outcome: tau_proto::OutputLengthContinuationOutcome::Incomplete,
            outer_turn_finish_owed: true,
        },
        provider_attempt: Default::default(),
        originator: PromptOriginator::User,
        usage: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };
    tree.validate_event(&Event::ProviderResponseFinished(terminal.clone()))
        .expect("descendant terminal");
    tree.apply_event(&Event::ProviderResponseFinished(terminal));
    assert_eq!(
        tree.output_length_budget_spent_outer_turn(),
        Some(continuation.outer_turn_id)
    );
}

/// Output-length plans require a marked user-owned source dispatch and prompt
/// start; legacy or side-conversation sources cannot mint continuation work.
#[test]
fn output_length_recovery_rejects_unmarked_and_side_owned_sources() {
    let fixture = output_length_recovery_fixture();
    let mut legacy_source = fixture.through(OutputLengthFixturePhase::Plan).to_vec();
    fixture
        .source_dispatch_in(&mut legacy_source)
        .fold_semantics = AgentJournalFoldSemantics::Legacy;
    assert!(AgentTree::try_from_events(fixture.agent_id().clone(), &legacy_source).is_err());

    let mut side_started = fixture.through(OutputLengthFixturePhase::Plan).to_vec();
    let started = fixture.source_prompt_start_in(&mut side_started);
    started.originator = PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("side-owner").expect("extension name"),
        query_id: "side-query".to_owned(),
    };
    assert!(AgentTree::try_from_events(fixture.agent_id().clone(), &side_started).is_err());
}

/// Cold finish repair is authorized only by the stamped terminal bit and names
/// the exact still-open outer turn.
#[test]
fn output_length_finish_repair_requires_stamped_authority() {
    let fixture = output_length_recovery_fixture();
    let terminal_cut = AgentTree::try_from_events(
        fixture.agent_id().clone(),
        fixture.through(OutputLengthFixturePhase::Terminal),
    )
    .expect("cold terminal cut");
    assert_eq!(
        terminal_cut.output_length_outer_turn_finish_repair(),
        fixture
            .owner()
            .output_length_continuation
            .as_ref()
            .map(|continuation| continuation.outer_turn_id.clone())
    );

    let mut no_finish_records = fixture.through(OutputLengthFixturePhase::Terminal).to_vec();
    let response = fixture.terminal_in(&mut no_finish_records);
    let tau_proto::OutputLengthDisposition::ContinuationTerminal {
        outer_turn_finish_owed,
        ..
    } = &mut response.output_length_disposition
    else {
        unreachable!("constructed continuation terminal")
    };
    *outer_turn_finish_owed = false;
    let no_finish_cut = AgentTree::try_from_events(fixture.agent_id().clone(), &no_finish_records)
        .expect("cold terminal without finish repair");
    assert_eq!(no_finish_cut.output_length_outer_turn_finish_repair(), None);
}

/// Malformed or duplicate continuation facts fail cold fold instead of
/// authorizing repair from a nearby but different plan.
#[test]
fn output_length_recovery_rejects_mismatched_and_duplicate_facts() {
    let fixture = output_length_recovery_fixture();

    let mut wrong_steer = fixture.through(OutputLengthFixturePhase::Steer).to_vec();
    let steer = fixture.steer_in(&mut wrong_steer);
    steer.text.push_str(" changed");
    assert!(AgentTree::try_from_events(fixture.agent_id().clone(), &wrong_steer).is_err());

    let mut wrong_owner = fixture.through(OutputLengthFixturePhase::Owner).to_vec();
    let owner = fixture.owner_in(&mut wrong_owner);
    owner.through = AgentHead::Root;
    assert!(AgentTree::try_from_events(fixture.agent_id().clone(), &wrong_owner).is_err());

    let mut wrong_terminal = fixture.through(OutputLengthFixturePhase::Terminal).to_vec();
    let terminal = fixture.terminal_in(&mut wrong_terminal);
    let tau_proto::OutputLengthDisposition::ContinuationTerminal { ordinal, .. } =
        &mut terminal.output_length_disposition
    else {
        unreachable!("constructed continuation terminal")
    };
    *ordinal = 2;
    assert!(AgentTree::try_from_events(fixture.agent_id().clone(), &wrong_terminal).is_err());

    let mut duplicate_plan = fixture.through(OutputLengthFixturePhase::Plan).to_vec();
    duplicate_plan.push(output_length_record(
        4,
        AgentEventParent::Root,
        Event::ProviderResponseFinished(fixture.source_response()),
        AgentJournalFoldSemantics::Legacy,
    ));
    assert!(AgentTree::try_from_events(fixture.agent_id().clone(), &duplicate_plan).is_err());

    let continuation = fixture
        .owner()
        .output_length_continuation
        .as_ref()
        .expect("continuation owner");
    let finish = tau_proto::AgentOuterTurnFinished {
        automatic_compaction_decision: None,
        agent_id: fixture.agent_id().clone(),
        session_id: tau_proto::SessionId::parse("session").expect("session id"),
        outer_turn_id: continuation.outer_turn_id.clone(),
        disposition: tau_proto::AgentOuterTurnDisposition::Settled,
    };
    let mut finished = fixture.through(OutputLengthFixturePhase::Terminal).to_vec();
    finished.push(output_length_record(
        8,
        AgentEventParent::Under(fixture.terminal_node()),
        Event::AgentOuterTurnFinished(finish.clone()),
        AgentJournalFoldSemantics::Legacy,
    ));
    let finished_tree = AgentTree::try_from_events(fixture.agent_id().clone(), &finished)
        .expect("matching finish closes repair");
    assert_eq!(finished_tree.output_length_outer_turn_finish_repair(), None);
    finished.push(output_length_record(
        9,
        AgentEventParent::Under(fixture.terminal_node()),
        Event::AgentOuterTurnFinished(finish),
        AgentJournalFoldSemantics::Legacy,
    ));
    assert!(AgentTree::try_from_events(fixture.agent_id().clone(), &finished).is_err());
}

/// A reserved successor terminal must never also plan reactive recovery on the
/// same response, and a finish-owed bit must stay consistent with the durable
/// output shape: a ToolCalls terminal without call items may owe its finish,
/// while one with call items contradicts the bit and must fail closed.
#[test]
fn output_length_terminal_rejects_reactive_recovery_and_pins_finish_bit() {
    let fixture = output_length_recovery_fixture();
    let tree = AgentTree::try_from_events(
        fixture.agent_id().clone(),
        fixture.through(OutputLengthFixturePhase::PromptStart),
    )
    .expect("owner and prompt-start fold");
    let terminal = fixture
        .records
        .iter()
        .find_map(|record| match &record.event {
            Event::ProviderResponseFinished(response)
                if matches!(
                    response.output_length_disposition,
                    tau_proto::OutputLengthDisposition::ContinuationTerminal { .. }
                ) =>
            {
                Some(response.clone())
            }
            _ => None,
        })
        .expect("fixture successor terminal");

    let mut combined = terminal.clone();
    combined.recovery_disposition =
        tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned;
    assert!(
        validation_error(&tree, Event::ProviderResponseFinished(combined))
            .contains("cannot combine context recovery and output-length dispositions"),
        "a successor terminal and reactive recovery are mutually exclusive on one response"
    );

    let tau_proto::OutputLengthDisposition::ContinuationTerminal {
        outer_turn_id,
        source_agent_prompt_id,
        ..
    } = &terminal.output_length_disposition
    else {
        unreachable!("constructed continuation terminal")
    };
    let mut tool_calls_terminal = terminal.clone();
    tool_calls_terminal.stop_reason = tau_proto::ProviderStopReason::ToolCalls;
    tool_calls_terminal.output_items = Vec::new();
    tool_calls_terminal.output_length_disposition =
        tau_proto::OutputLengthDisposition::ContinuationTerminal {
            outer_turn_id: outer_turn_id.clone(),
            source_agent_prompt_id: source_agent_prompt_id.clone(),
            ordinal: 1,
            outcome: tau_proto::OutputLengthContinuationOutcome::Completed,
            outer_turn_finish_owed: true,
        };
    tree.validate_event(&Event::ProviderResponseFinished(
        tool_calls_terminal.clone(),
    ))
    .expect("ToolCalls terminal without call items may owe its finish");

    tool_calls_terminal.output_items = vec![ContextItem::ToolCall(ToolCallItem {
        call_id: "finish-bit-call".into(),
        name: ToolName::new("finish_bit_tool"),
        tool_type: tau_proto::ToolType::Function,
        arguments: tau_proto::CborValue::Null,
        raw_arguments_json: None,
        responses_envelope: None,
    })];
    assert!(
        validation_error(&tree, Event::ProviderResponseFinished(tool_calls_terminal))
            .contains("output-length terminal mismatches"),
        "a finish-owed ToolCalls terminal must not also carry call items"
    );

    // An EndTurn terminal with call items always dispatches those calls, so a
    // finish-owed bit on that shape is equally contradictory and must fail
    // closed instead of trusting the stamped bit.
    let mut end_turn_with_calls = terminal.clone();
    end_turn_with_calls.output_items = vec![ContextItem::ToolCall(ToolCallItem {
        call_id: "finish-bit-end-turn-call".into(),
        name: ToolName::new("finish_bit_tool"),
        tool_type: tau_proto::ToolType::Function,
        arguments: tau_proto::CborValue::Null,
        raw_arguments_json: None,
        responses_envelope: None,
    })];
    assert!(
        validation_error(&tree, Event::ProviderResponseFinished(end_turn_with_calls))
            .contains("output-length terminal mismatches"),
        "a finish-owed EndTurn terminal must not also carry call items"
    );
}

/// A completed historical continuation cannot hide or consume a later
/// unresolved plan at either the plan or steer crash cut.
#[test]
fn output_length_recovery_selects_later_unresolved_plan() {
    let fixture = output_length_recovery_fixture();
    let owner_continuation = fixture
        .owner()
        .output_length_continuation
        .as_ref()
        .expect("continuation owner");
    let mut records = fixture.through(OutputLengthFixturePhase::Terminal).to_vec();
    records.push(output_length_record(
        8,
        AgentEventParent::Under(fixture.terminal_node()),
        Event::AgentOuterTurnFinished(tau_proto::AgentOuterTurnFinished {
            automatic_compaction_decision: None,
            agent_id: fixture.agent_id().clone(),
            session_id: tau_proto::SessionId::parse("session").expect("session id"),
            outer_turn_id: owner_continuation.outer_turn_id.clone(),
            disposition: tau_proto::AgentOuterTurnDisposition::Settled,
        }),
        AgentJournalFoldSemantics::Legacy,
    ));
    let later_source_prompt_id =
        tau_proto::AgentPromptId::parse("ap-later-source").expect("later source prompt id");
    let later_successor_prompt_id =
        tau_proto::AgentPromptId::parse("ap-later-successor").expect("later successor prompt id");
    let later_outer_turn_id = tau_proto::AgentOuterTurnId::for_prompt(&later_source_prompt_id);
    records.push(output_length_record(
        9,
        AgentEventParent::Under(fixture.terminal_node()),
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            agent_id: fixture.agent_id().clone(),
            transaction_id: None,
            agent_prompt_id: later_source_prompt_id.clone(),
            through: AgentHead::Node(fixture.terminal_node()),
            model: fixture.owner().model.clone(),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(AgentHead::Node(fixture.terminal_node())),
            output_length_continuation: None,
        }),
        AgentJournalFoldSemantics::InferenceDeferredInputV1,
    ));
    records.push(output_length_record(
        10,
        AgentEventParent::Under(fixture.terminal_node()),
        Event::AgentOuterTurnStarted(tau_proto::AgentOuterTurnStarted {
            agent_id: fixture.agent_id().clone(),
            session_id: tau_proto::SessionId::parse("session").expect("session id"),
            outer_turn_id: later_outer_turn_id.clone(),
            agent_prompt_id: later_source_prompt_id.clone(),
            runtime_id: tau_proto::AccountingRuntimeId::parse("later-runtime").expect("runtime id"),
            activation: tau_proto::AgentOuterTurnActivation::External {
                correlation_id: tau_proto::AgentActivationCorrelationId::parse("later-activation")
                    .expect("activation id"),
            },
        }),
        AgentJournalFoldSemantics::Legacy,
    ));
    records.push(output_length_record(
        11,
        AgentEventParent::Under(fixture.terminal_node()),
        Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
            agent_prompt_id: later_source_prompt_id.clone(),
            agent_id: fixture.agent_id().clone(),
            session_id: tau_proto::SessionId::parse("session").expect("session id"),
            model: fixture.owner().model.clone().expect("owner model"),
            model_params: Some(tau_proto::ModelParams::default()),
            outer_turn_id: Some(later_outer_turn_id.clone()),
            operation: tau_proto::PromptOperation::Inference,
            originator: PromptOriginator::User,
            ctx_id: None,
        }),
        AgentJournalFoldSemantics::Legacy,
    ));
    let mut later_response = fixture.source_response();
    later_response.agent_prompt_id = later_source_prompt_id;
    later_response.output_length_disposition =
        tau_proto::OutputLengthDisposition::ContinuationPlanned {
            outer_turn_id: later_outer_turn_id,
            successor_agent_prompt_id: later_successor_prompt_id.clone(),
            ordinal: 1,
            limit: 1,
        };
    records.push(output_length_record(
        12,
        AgentEventParent::Under(fixture.terminal_node()),
        Event::ProviderResponseFinished(later_response),
        AgentJournalFoldSemantics::Legacy,
    ));
    let plan_cut = AgentTree::try_from_events(fixture.agent_id().clone(), &records)
        .expect("completed history and later plan cut");
    assert!(matches!(
        plan_cut.output_length_continuation_recovery(),
        Some(super::OutputLengthContinuationRecovery::SteerNeeded {
            successor_agent_prompt_id,
            ..
        }) if successor_agent_prompt_id == later_successor_prompt_id
    ));
    let later_plan_node = plan_cut.head().expect("later plan transcript node");

    records.push(output_length_record(
        13,
        AgentEventParent::Under(later_plan_node),
        Event::AgentPromptSteered(output_length_steer(fixture.agent_id().clone())),
        AgentJournalFoldSemantics::Legacy,
    ));
    let steer_cut = AgentTree::try_from_events(fixture.agent_id().clone(), &records)
        .expect("completed history and later steer cut");
    let later_steer_head =
        AgentHead::Node(steer_cut.head().expect("later continuation steer node"));
    assert!(matches!(
        steer_cut.output_length_continuation_recovery(),
        Some(super::OutputLengthContinuationRecovery::OwnerNeeded {
            successor_agent_prompt_id,
            through,
            ..
        }) if successor_agent_prompt_id == later_successor_prompt_id && through == later_steer_head
    ));
}

/// Cold reconstruction after each dormant repair cut exposes only the next
/// exact fact while preserving the selected sibling.
#[test]
fn output_length_recovery_repairs_dormant_branch_without_selecting_it() {
    let fixture = output_length_recovery_fixture();
    let mut records = fixture.through(OutputLengthFixturePhase::Plan).to_vec();
    records.push(output_length_record(
        4,
        AgentEventParent::Root,
        Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            agent_id: fixture.agent_id().clone(),
            text: "branch elsewhere".to_owned(),
            inference_activation: false,
            message_class: Default::default(),
        }),
        AgentJournalFoldSemantics::Legacy,
    ));
    let mut tree =
        AgentTree::try_from_events(fixture.agent_id().clone(), &records).expect("off-lineage cut");
    assert!(matches!(
        tree.output_length_continuation_recovery(),
        Some(super::OutputLengthContinuationRecovery::BranchInvalid { .. })
    ));
    let sibling = AgentHead::Node(tree.head().expect("selected sibling"));
    let repair = tree
        .output_length_dormant_repair()
        .expect("dormant steer repair");
    let super::OutputLengthDormantRepair::Steer { parent, .. } = repair else {
        panic!("expected dormant steer repair");
    };
    let steer = Event::AgentPromptSteered(output_length_steer(fixture.agent_id().clone()));
    let steer_parent = AgentEventParent::from_head(parent);
    tree.validate_event_at(steer_parent, &steer)
        .expect("dormant steer validates");
    records.push(output_length_record(
        records.len() as u64,
        steer_parent,
        steer,
        AgentJournalFoldSemantics::Legacy,
    ));
    tree =
        AgentTree::try_from_events(fixture.agent_id().clone(), &records).expect("cold steer cut");
    assert_eq!(tree.head(), sibling.as_option());

    let super::OutputLengthDormantRepair::Owner {
        source,
        successor_agent_prompt_id,
        outer_turn_id,
        through,
        plan_parent,
    } = tree
        .output_length_dormant_repair()
        .expect("dormant owner repair")
    else {
        panic!("expected dormant owner repair");
    };
    assert_eq!(plan_parent, parent);
    let owner = tau_proto::AgentInferenceDispatchStarted {
        agent_id: fixture.agent_id().clone(),
        transaction_id: None,
        agent_prompt_id: successor_agent_prompt_id,
        through,
        model: source.model,
        operation: source.operation,
        activation_cut: source.activation_cut,
        output_length_continuation: Some(tau_proto::OutputLengthContinuationOwner {
            source_agent_prompt_id: source.agent_prompt_id,
            outer_turn_id,
            ordinal: 1,
        }),
    };
    let owner_event = Event::AgentInferenceDispatchStarted(owner.clone());
    let owner_parent = AgentEventParent::from_head(through);
    tree.validate_event_at(owner_parent, &owner_event)
        .expect("dormant owner validates");
    records.push(output_length_record(
        records.len() as u64,
        owner_parent,
        owner_event,
        AgentJournalFoldSemantics::InferenceDeferredInputV1,
    ));
    tree =
        AgentTree::try_from_events(fixture.agent_id().clone(), &records).expect("cold owner cut");
    assert_eq!(tree.head(), sibling.as_option());

    let super::OutputLengthDormantRepair::Terminal { parent, .. } = tree
        .output_length_dormant_repair()
        .expect("dormant terminal repair")
    else {
        panic!("expected dormant terminal repair");
    };
    let continuation = owner
        .output_length_continuation
        .as_ref()
        .expect("continuation owner");
    let terminal = Event::ProviderResponseFinished(tau_proto::ProviderResponseFinished {
        automatic_compaction_decision: None,
        agent_prompt_id: owner.agent_prompt_id.clone(),
        agent_id: fixture.agent_id().clone(),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::Error,
        error: Some("output-length continuation branch was deselected".to_owned()),
        failure_kind: Some(tau_proto::ProviderFailureKind::Unknown),
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::ContinuationTerminal {
            outer_turn_id: continuation.outer_turn_id.clone(),
            source_agent_prompt_id: continuation.source_agent_prompt_id.clone(),
            ordinal: 1,
            outcome: tau_proto::OutputLengthContinuationOutcome::Failed,
            outer_turn_finish_owed: true,
        },
        provider_attempt: Default::default(),
        originator: PromptOriginator::User,
        usage: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    });
    let terminal_parent = AgentEventParent::from_head(parent);
    tree.validate_event_at(terminal_parent, &terminal)
        .expect("pre-start dormant failure validates");
    records.push(output_length_record(
        records.len() as u64,
        terminal_parent,
        terminal,
        AgentJournalFoldSemantics::Legacy,
    ));
    tree = AgentTree::try_from_events(fixture.agent_id().clone(), &records)
        .expect("cold terminal cut");
    assert_eq!(tree.head(), sibling.as_option());

    let Some(super::OutputLengthDormantRepair::Finish {
        outer_turn_id: finish_turn,
        parent: finish_parent_head,
    }) = tree.output_length_dormant_repair()
    else {
        panic!("expected dormant finish repair");
    };
    assert_eq!(finish_turn, continuation.outer_turn_id);
    let finish = Event::AgentOuterTurnFinished(tau_proto::AgentOuterTurnFinished {
        automatic_compaction_decision: None,
        agent_id: fixture.agent_id().clone(),
        session_id: tau_proto::SessionId::parse("session").expect("session id"),
        outer_turn_id: continuation.outer_turn_id.clone(),
        disposition: tau_proto::AgentOuterTurnDisposition::Settled,
    });
    let finish_parent = AgentEventParent::from_head(finish_parent_head);
    tree.validate_event_at(finish_parent, &finish)
        .expect("dormant owed finish validates");
    records.push(output_length_record(
        records.len() as u64,
        finish_parent,
        finish,
        AgentJournalFoldSemantics::Legacy,
    ));
    tree =
        AgentTree::try_from_events(fixture.agent_id().clone(), &records).expect("cold finish cut");
    assert_eq!(tree.output_length_dormant_repair(), None);
    assert_eq!(tree.head(), sibling.as_option());
}

/// Once the reserved successor prompt-start commits, branch movement cannot
/// authorize a competing synthetic pre-start failure.
#[test]
fn output_length_post_start_branch_move_has_no_synthetic_repair() {
    let fixture = output_length_recovery_fixture();
    let mut records = fixture
        .through(OutputLengthFixturePhase::PromptStart)
        .to_vec();
    records.push(output_length_record(
        u64::try_from(records.len()).expect("bounded fixture"),
        AgentEventParent::Root,
        Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            agent_id: fixture.agent_id().clone(),
            text: "post-start sibling".to_owned(),
            inference_activation: false,
            message_class: Default::default(),
        }),
        AgentJournalFoldSemantics::Legacy,
    ));
    let tree = AgentTree::try_from_events(fixture.agent_id().clone(), &records)
        .expect("post-start sibling");
    assert_eq!(tree.output_length_dormant_repair(), None);
    assert_eq!(tree.output_length_continuation_recovery(), None);
}

/// Cold sticky output-length status stops at newer selected work, whether that
/// work is unfinished or later succeeds.
#[test]
fn output_length_terminal_incomplete_does_not_cross_newer_selected_dispatch() {
    let fixture = output_length_recovery_fixture();
    let mut records = fixture.through(OutputLengthFixturePhase::Terminal).to_vec();
    let terminal = fixture.terminal_in(&mut records);
    terminal.stop_reason = tau_proto::ProviderStopReason::Length;
    terminal.output_items.clear();
    terminal.provider_attempt =
        tau_proto::ProviderAttempt::new(4).expect("nonzero provider attempt");
    let tau_proto::OutputLengthDisposition::ContinuationTerminal { outcome, .. } =
        &mut terminal.output_length_disposition
    else {
        panic!("fixture successor terminal");
    };
    *outcome = tau_proto::OutputLengthContinuationOutcome::Incomplete;
    let mut tree =
        AgentTree::try_from_events(fixture.agent_id().clone(), &records).expect("incomplete cut");
    assert_eq!(
        tree.output_length_terminal_incomplete()
            .expect("latest incomplete")
            .provider_attempt
            .get(),
        4
    );

    let later_prompt_id =
        tau_proto::AgentPromptId::parse("ap-later").expect("known-safe prompt id");
    let selected = tree.head().map_or(AgentHead::Root, AgentHead::Node);
    tree.apply_event(&Event::AgentInferenceDispatchStarted(
        tau_proto::AgentInferenceDispatchStarted {
            agent_id: fixture.agent_id().clone(),
            transaction_id: None,
            agent_prompt_id: later_prompt_id.clone(),
            through: selected,
            model: Some("provider/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(selected),
            output_length_continuation: None,
        },
    ));
    assert_eq!(tree.output_length_terminal_incomplete(), None);

    tree.apply_event(&Event::ProviderResponseFinished(
        tau_proto::ProviderResponseFinished {
            automatic_compaction_decision: None,
            agent_prompt_id: later_prompt_id,
            agent_id: fixture.agent_id().clone(),
            output_items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "later success".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
            stop_reason: tau_proto::ProviderStopReason::EndTurn,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            provider_attempt: Default::default(),
            originator: PromptOriginator::User,
            usage: None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        },
    ));
    assert_eq!(tree.output_length_terminal_incomplete(), None);
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

/// A continuation prompt's durable outer-turn ownership admits its eager
/// decision, which survives the finish cut and is claimed exactly once.
#[test]
fn eager_automatic_decision_replays_terminal_finish_and_start_cuts() {
    let agent_id = agent_id();
    let mut tree = AgentTree::from_events(agent_id.clone(), &[]);
    let session_id = tau_proto::SessionId::parse("session").expect("session");
    let initial_prompt_id =
        tau_proto::AgentPromptId::parse("prompt-eager-initial").expect("prompt");
    let continuation_prompt_id =
        tau_proto::AgentPromptId::parse("prompt-eager-continuation").expect("prompt");
    let outer_turn_id = tau_proto::AgentOuterTurnId::for_prompt(&initial_prompt_id);
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-eager").expect("transaction");
    let model = tau_proto::ModelId::new(
        tau_proto::ProviderName::new("test"),
        tau_proto::ModelName::new("model"),
    );
    tree.apply_event(&Event::AgentInferenceDispatchStarted(
        tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_id.clone(),
            transaction_id: None,
            agent_prompt_id: continuation_prompt_id.clone(),
            through: AgentHead::Root,
            model: Some(model.clone()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: None,
            output_length_continuation: None,
        },
    ));
    tree.apply_event(&Event::AgentOuterTurnStarted(
        tau_proto::AgentOuterTurnStarted {
            agent_id: agent_id.clone(),
            session_id: session_id.clone(),
            outer_turn_id: outer_turn_id.clone(),
            agent_prompt_id: initial_prompt_id,
            runtime_id: tau_proto::AccountingRuntimeId::parse("runtime").expect("runtime"),
            activation: tau_proto::AgentOuterTurnActivation::External {
                correlation_id: tau_proto::AgentActivationCorrelationId::parse("activation")
                    .expect("activation"),
            },
        },
    ));
    let tree_without_prompt_start = tree.clone();
    tree.apply_event(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        agent_prompt_id: continuation_prompt_id.clone(),
        agent_id: agent_id.clone(),
        session_id: session_id.clone(),
        model: model.clone(),
        model_params: Some(tau_proto::ModelParams::default()),
        outer_turn_id: Some(outer_turn_id.clone()),
        operation: tau_proto::PromptOperation::Inference,
        originator: PromptOriginator::User,
        ctx_id: None,
    }));
    append_user_input(&mut tree, "terminal owner");
    let mut terminal =
        tool_calling_response(&agent_id, continuation_prompt_id.as_str(), Vec::new());
    terminal.stop_reason = tau_proto::ProviderStopReason::EndTurn;
    terminal.usage = Some(tau_proto::ProviderTokenUsage {
        model: Some(model.clone()),
        prompt_sent_tokens: 100,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 0,
        stats: Default::default(),
    });
    terminal.automatic_compaction_decision = Some(tau_proto::AutomaticCompactionDecision {
        transaction_id: transaction_id.clone(),
        outer_turn_id: outer_turn_id.clone(),
        model: model.clone(),
        threshold: tau_proto::TokenCount::new(100),
        evidence: Some(tau_proto::ProactiveCompactionEvidence {
            provider_prompt_id: continuation_prompt_id.clone(),
            provider_input_tokens: tau_proto::TokenCount::new(100),
            threshold: tau_proto::TokenCount::new(100),
            threshold_source: tau_proto::CompactionThresholdSource::ProviderDefault,
        }),
    });
    let terminal = Event::ProviderResponseFinished(terminal);
    assert!(
        tree_without_prompt_start.validate_event(&terminal).is_err(),
        "the initial outer-turn prompt id is not sufficient ownership"
    );
    tree.validate_event(&terminal)
        .expect("terminal decision validates");
    tree.apply_event(&terminal);
    let cut = tree.head().map_or(AgentHead::Root, AgentHead::Node);
    assert!(matches!(
        tree.standalone_compaction_recovery(),
        Some(
            super::StandaloneCompactionRecovery::AwaitingAutomaticStart {
                finish_committed: false,
                ..
            }
        )
    ));
    let finish = Event::AgentOuterTurnFinished(tau_proto::AgentOuterTurnFinished {
        agent_id: agent_id.clone(),
        session_id,
        outer_turn_id,
        disposition: tau_proto::AgentOuterTurnDisposition::Settled,
        automatic_compaction_decision: Some(transaction_id.clone()),
    });
    tree.validate_event(&finish)
        .expect("finish reference validates");
    tree.apply_event(&finish);
    assert!(matches!(
        tree.standalone_compaction_recovery(),
        Some(
            super::StandaloneCompactionRecovery::AwaitingAutomaticStart {
                finish_committed: true,
                ..
            }
        )
    ));
    let mut stale_tree = tree.clone();
    stale_tree.apply_event(&Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
        agent_id: agent_id.clone(),
        head: AgentHead::Root,
    }));
    append_user_input(&mut stale_tree, "selected sibling");
    let stale =
        Event::AgentStandaloneCompactionFailed(tau_proto::AgentStandaloneCompactionFailed {
            agent_id: agent_id.clone(),
            transaction_id: transaction_id.clone(),
            cut,
            reason: tau_proto::StandaloneCompactionFailureReason::StaleBranch,
            resume_through: None,
            context_retreat: None,
        });
    stale_tree
        .validate_event(&stale)
        .expect("pre-start stale closure validates");
    stale_tree.apply_event(&stale);
    assert_eq!(stale_tree.standalone_compaction_recovery(), None);
    let start =
        Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
            agent_id: agent_id.clone(),
            transaction_id: transaction_id.clone(),
            compact_prompt_id: tau_proto::AgentPromptId::parse("compact-eager").expect("prompt"),
            cut,
            resume_through: Some(cut),
            model,
            operation: tau_proto::PromptOperation::StandaloneCompaction,
            originator: PromptOriginator::User,
            supersedes: None,
            trigger: tau_proto::StandaloneCompactionTrigger::AutomaticPolicy {
                decision_id: transaction_id.clone(),
            },
        });
    let Event::AgentStandaloneCompactionStarted(mut wrong_start) = start.clone() else {
        unreachable!("constructed standalone compaction start")
    };
    wrong_start.trigger = tau_proto::StandaloneCompactionTrigger::Manual;
    assert!(
        tree.validate_event(&Event::AgentStandaloneCompactionStarted(wrong_start))
            .is_err(),
        "generic compaction starts must not claim a reserved decision identity"
    );
    tree.validate_event(&start)
        .expect("decision claim validates");
    tree.apply_event(&start);
    assert!(matches!(
        tree.standalone_compaction_recovery(),
        Some(super::StandaloneCompactionRecovery::Interrupted(_))
    ));
    tree.apply_event(&Event::AgentStandaloneCompactionFailed(
        tau_proto::AgentStandaloneCompactionFailed {
            agent_id: agent_id.clone(),
            transaction_id: transaction_id.clone(),
            cut,
            reason: tau_proto::StandaloneCompactionFailureReason::Interrupted,
            resume_through: None,
            context_retreat: None,
        },
    ));
    let prompt_two = tau_proto::AgentPromptId::parse("prompt-eager-two").expect("prompt");
    let turn_two = tau_proto::AgentOuterTurnId::for_prompt(&prompt_two);
    tree.apply_event(&Event::AgentInferenceDispatchStarted(
        tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_id.clone(),
            transaction_id: None,
            agent_prompt_id: prompt_two.clone(),
            through: tree.head().map_or(AgentHead::Root, AgentHead::Node),
            model: Some(tau_proto::ModelId::new(
                tau_proto::ProviderName::new("test"),
                tau_proto::ModelName::new("model"),
            )),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: None,
            output_length_continuation: None,
        },
    ));
    tree.apply_event(&Event::AgentOuterTurnStarted(
        tau_proto::AgentOuterTurnStarted {
            agent_id: agent_id.clone(),
            session_id: tau_proto::SessionId::parse("session").expect("session"),
            outer_turn_id: turn_two.clone(),
            agent_prompt_id: prompt_two.clone(),
            runtime_id: tau_proto::AccountingRuntimeId::parse("runtime-two").expect("runtime"),
            activation: tau_proto::AgentOuterTurnActivation::External {
                correlation_id: tau_proto::AgentActivationCorrelationId::parse("activation-two")
                    .expect("activation"),
            },
        },
    ));
    let mut collision = tool_calling_response(&agent_id, prompt_two.as_str(), Vec::new());
    collision.stop_reason = tau_proto::ProviderStopReason::EndTurn;
    collision.automatic_compaction_decision = Some(tau_proto::AutomaticCompactionDecision {
        transaction_id,
        outer_turn_id: turn_two,
        model: tau_proto::ModelId::new(
            tau_proto::ProviderName::new("test"),
            tau_proto::ModelName::new("model"),
        ),
        threshold: tau_proto::TokenCount::new(100),
        evidence: None,
    });
    assert!(
        tree.validate_event(&Event::ProviderResponseFinished(collision))
            .is_err(),
        "decision identity must not collide with a prior transaction"
    );
}

/// Exhaustively checks the narrow automatic-policy lineage model across zero
/// through three real tool rounds. Every accepted command must fold identically
/// live and cold; rejected ownership and duplicate-repair commands must leave
/// the complete live tree unchanged.
#[test]
fn automatic_policy_lineage_model_matches_live_and_cold_folds() {
    for rounds in 0..=3 {
        let trace = automatic_policy_tool_trace(rounds);
        let mut live = AgentTree::from_events(trace.agent_id.clone(), &[]);
        let mut records = Vec::new();
        for (command, event) in &trace.events {
            append_model_event(
                &mut live,
                &mut records,
                event.clone(),
                &trace.label(command),
            );
            let cold = AgentTree::try_from_events(trace.agent_id.clone(), &records).unwrap_or_else(
                |error| panic!("{} cold replay failed: {error}", trace.label(command)),
            );
            assert_eq!(live, cold, "{}", trace.label(command));
        }

        let wrong_owner = {
            let mut response = trace.terminal.clone();
            response.agent_prompt_id =
                tau_proto::AgentPromptId::parse("model-wrong-owner").expect("prompt");
            Event::ProviderResponseFinished(response)
        };
        let terminal_cut = trace
            .cuts
            .terminal_decision
            .checked_sub(1)
            .expect("terminal has a predecessor");
        let prefix = records[..terminal_cut].to_vec();
        let mut before_terminal =
            AgentTree::try_from_events(trace.agent_id.clone(), &prefix).expect("terminal prefix");
        assert_rejected_model_event(
            &mut before_terminal,
            wrong_owner,
            &trace.label("reject_wrong_owner"),
        );

        for (command, duplicate) in [
            (
                "duplicate_terminal",
                Event::ProviderResponseFinished(trace.terminal.clone()),
            ),
            ("duplicate_finish", trace.finish.clone()),
            ("duplicate_start", trace.start.clone()),
        ] {
            assert_rejected_model_event(&mut live, duplicate, &trace.label(command));
        }

        assert_automatic_policy_crash_cuts(&trace, &records);
    }
}

/// One deterministic model trace and its named durable crash boundaries.
struct AutomaticPolicyToolTrace {
    /// Stable case label printed on every failed property.
    rounds: usize,
    /// Agent owning the trace.
    agent_id: tau_proto::AgentId,
    /// Ordered accepted model commands and their durable events.
    events: Vec<(&'static str, Event)>,
    /// Final decision-bearing response for negative mutations.
    terminal: tau_proto::ProviderResponseFinished,
    /// Matching outer-finish event.
    finish: Event,
    /// Matching protected standalone start.
    start: Event,
    /// Named durable record counts at each crash boundary.
    cuts: AutomaticPolicyCuts,
}

impl AutomaticPolicyToolTrace {
    /// Formats a reproducible fixed-seed trace label.
    fn label(&self, command: &str) -> String {
        format!("seed=0 rounds={} command={command}", self.rounds)
    }
}

/// Durable record counts immediately after each named crash boundary.
struct AutomaticPolicyCuts {
    /// Latest continuation prompt start (or the initial prompt for zero
    /// rounds).
    continuation_prompt_started: usize,
    /// Decision-bearing terminal.
    terminal_decision: usize,
    /// Matching outer-turn finish.
    outer_finished: usize,
    /// Protected automatic standalone start.
    standalone_started: usize,
}

/// Builds one real linear tool topology for the bounded lineage model.
fn automatic_policy_tool_trace(rounds: usize) -> AutomaticPolicyToolTrace {
    let agent_id = agent_id();
    let session_id = tau_proto::SessionId::parse("model-session").expect("session");
    let model: tau_proto::ModelId = "test/model".into();
    let initial_prompt =
        tau_proto::AgentPromptId::parse(format!("model-prompt-{rounds}-0")).expect("prompt");
    let outer_turn_id = tau_proto::AgentOuterTurnId::for_prompt(&initial_prompt);
    let transaction_id = tau_proto::CompactionTransactionId::parse(format!("model-ct-{rounds}"))
        .expect("transaction");
    let mut events = Vec::new();
    let mut prompt = initial_prompt.clone();
    let push_prompt = |events: &mut Vec<(&'static str, Event)>,
                       prompt_id: &tau_proto::AgentPromptId,
                       through: AgentHead| {
        events.push((
            "dispatch",
            Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
                agent_id: agent_id.clone(),
                transaction_id: None,
                agent_prompt_id: prompt_id.clone(),
                through,
                model: Some(model.clone()),
                operation: Some(tau_proto::PromptOperation::Inference),
                activation_cut: Some(through),
                output_length_continuation: None,
            }),
        ));
        events.push((
            "prompt_started",
            Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
                agent_prompt_id: prompt_id.clone(),
                agent_id: agent_id.clone(),
                session_id: session_id.clone(),
                model: model.clone(),
                model_params: Some(tau_proto::ModelParams::default()),
                outer_turn_id: Some(outer_turn_id.clone()),
                operation: tau_proto::PromptOperation::Inference,
                originator: PromptOriginator::User,
                ctx_id: None,
            }),
        ));
    };
    push_prompt(&mut events, &prompt, AgentHead::Root);
    let initial_prompt_started = events.pop().expect("initial prompt start");
    events.push((
        "outer_started",
        Event::AgentOuterTurnStarted(tau_proto::AgentOuterTurnStarted {
            agent_id: agent_id.clone(),
            session_id: session_id.clone(),
            outer_turn_id: outer_turn_id.clone(),
            agent_prompt_id: initial_prompt.clone(),
            runtime_id: tau_proto::AccountingRuntimeId::parse(format!("model-runtime-{rounds}"))
                .expect("runtime"),
            activation: tau_proto::AgentOuterTurnActivation::External {
                correlation_id: tau_proto::AgentActivationCorrelationId::parse(format!(
                    "model-activation-{rounds}"
                ))
                .expect("activation"),
            },
        }),
    ));
    events.push(initial_prompt_started);
    for round in 0..rounds {
        let call_id = tau_proto::ToolCallId::from(format!("model-call-{rounds}-{round}"));
        events.push((
            "tool_terminal",
            Event::ProviderResponseFinished(tool_calling_response(
                &agent_id,
                prompt.as_str(),
                vec![call_id.clone()],
            )),
        ));
        events.push((
            "tool_result",
            Event::ProviderToolResult(tau_proto::ToolResult {
                presentation: Default::default(),
                call_id,
                tool_name: tau_proto::ToolName::new("tool"),
                tool_type: tau_proto::ToolType::Function,
                result: tau_proto::CborValue::Text("done".to_owned()),
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::Final,
                display: None,
                originator: PromptOriginator::User,
            }),
        ));
        prompt = tau_proto::AgentPromptId::parse(format!("model-prompt-{rounds}-{}", round + 1))
            .expect("prompt");
        push_prompt(
            &mut events,
            &prompt,
            AgentHead::Node(NodeId::new((round * 2 + 1) as u64)),
        );
    }
    let continuation_prompt_started = events.len();
    let mut terminal = tool_calling_response(&agent_id, prompt.as_str(), Vec::new());
    terminal.stop_reason = tau_proto::ProviderStopReason::EndTurn;
    terminal.usage = Some(tau_proto::ProviderTokenUsage {
        model: Some(model.clone()),
        prompt_sent_tokens: 100,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 0,
        stats: Default::default(),
    });
    terminal.automatic_compaction_decision = Some(tau_proto::AutomaticCompactionDecision {
        transaction_id: transaction_id.clone(),
        outer_turn_id: outer_turn_id.clone(),
        model: model.clone(),
        threshold: tau_proto::TokenCount::new(100),
        evidence: Some(tau_proto::ProactiveCompactionEvidence {
            provider_prompt_id: prompt.clone(),
            provider_input_tokens: tau_proto::TokenCount::new(100),
            threshold: tau_proto::TokenCount::new(100),
            threshold_source: tau_proto::CompactionThresholdSource::ProviderDefault,
        }),
    });
    events.push((
        "terminal_decision",
        Event::ProviderResponseFinished(terminal.clone()),
    ));
    let terminal_decision = events.len();
    let finish = Event::AgentOuterTurnFinished(tau_proto::AgentOuterTurnFinished {
        agent_id: agent_id.clone(),
        session_id,
        outer_turn_id,
        disposition: tau_proto::AgentOuterTurnDisposition::Settled,
        automatic_compaction_decision: Some(transaction_id.clone()),
    });
    events.push(("outer_finished", finish.clone()));
    let outer_finished = events.len();
    let start =
        Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
            agent_id: agent_id.clone(),
            transaction_id: transaction_id.clone(),
            compact_prompt_id: tau_proto::AgentPromptId::parse(format!("model-compact-{rounds}"))
                .expect("prompt"),
            cut: AgentHead::Node(NodeId::new((rounds * 2) as u64)),
            resume_through: Some(AgentHead::Node(NodeId::new((rounds * 2) as u64))),
            model,
            operation: tau_proto::PromptOperation::StandaloneCompaction,
            originator: PromptOriginator::User,
            supersedes: None,
            trigger: tau_proto::StandaloneCompactionTrigger::AutomaticPolicy {
                decision_id: transaction_id,
            },
        });
    events.push(("standalone_started", start.clone()));
    let standalone_started = events.len();
    AutomaticPolicyToolTrace {
        rounds,
        agent_id,
        events,
        terminal,
        finish,
        start,
        cuts: AutomaticPolicyCuts {
            continuation_prompt_started,
            terminal_decision,
            outer_finished,
            standalone_started,
        },
    }
}

/// Appends one model-accepted command and records its exact durable form.
fn append_model_event(
    tree: &mut AgentTree,
    records: &mut Vec<PersistedAgentEvent>,
    event: Event,
    label: &str,
) {
    let parent = match &event {
        Event::ProviderResponseFinished(response) => tree
            .inference_dispatches
            .get(&response.agent_prompt_id)
            .map(|dispatch| match dispatch.checkpoint.through {
                AgentHead::Root => AgentEventParent::Root,
                AgentHead::Node(node) => AgentEventParent::Under(node),
            })
            .unwrap_or(AgentEventParent::InheritHead),
        _ => AgentEventParent::InheritHead,
    };
    tree.validate_event_at(parent, &event)
        .unwrap_or_else(|error| panic!("{label} unexpectedly rejected: {error}"));
    let fold_semantics = AgentJournalFoldSemantics::for_new_event(&event);
    let record = PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([records.len() as u8; 16]),
        seq: PersistedAgentEventSeq::new(records.len() as u64),
        source: None,
        event,
        parent,
        fold_semantics,
        recorded_at: tau_proto::UnixMicros::new(records.len() as u64),
    };
    tree.apply_persisted_record(&record)
        .unwrap_or_else(|error| panic!("{label} append failed: {error}"));
    records.push(record);
}

/// Drives a rejected record through the mutating durable fold seam and proves
/// that neither folded state nor the next sequence changes.
fn assert_rejected_model_event(tree: &mut AgentTree, event: Event, label: &str) {
    let unchanged = tree.clone();
    let record = PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([255; 16]),
        seq: tree.next_event_seq(),
        source: None,
        event: event.clone(),
        parent: AgentEventParent::InheritHead,
        fold_semantics: AgentJournalFoldSemantics::for_new_event(&event),
        recorded_at: tau_proto::UnixMicros::new(u64::MAX),
    };
    assert!(
        tree.apply_persisted_record(&record).is_err(),
        "{label} must be rejected"
    );
    assert_eq!(*tree, unchanged, "{label} partially mutated the fold");
}

/// Replays each named post-tool crash cut twice and checks the exact cold
/// recovery projection. Reopening is observational: it must not manufacture
/// another durable fact or change which suffix is owed.
fn assert_automatic_policy_crash_cuts(
    trace: &AutomaticPolicyToolTrace,
    records: &[PersistedAgentEvent],
) {
    for (name, cut, expected) in [
        (
            "continuation_prompt_started",
            trace.cuts.continuation_prompt_started,
            "none",
        ),
        (
            "terminal_decision",
            trace.cuts.terminal_decision,
            "awaiting_finish",
        ),
        (
            "outer_finished",
            trace.cuts.outer_finished,
            "awaiting_start",
        ),
        (
            "standalone_started",
            trace.cuts.standalone_started,
            "interrupted",
        ),
    ] {
        let first = AgentTree::try_from_events(trace.agent_id.clone(), &records[..cut])
            .unwrap_or_else(|error| panic!("{} cut={name}: {error}", trace.label("reopen")));
        let second = AgentTree::try_from_events(trace.agent_id.clone(), &records[..cut])
            .expect("second reopen");
        assert_eq!(first, second, "{} cut={name}", trace.label("second_reopen"));
        let actual = match first.standalone_compaction_recovery() {
            None => "none",
            Some(super::StandaloneCompactionRecovery::AwaitingAutomaticStart {
                finish_committed: false,
                ..
            }) => "awaiting_finish",
            Some(super::StandaloneCompactionRecovery::AwaitingAutomaticStart {
                finish_committed: true,
                ..
            }) => "awaiting_start",
            Some(super::StandaloneCompactionRecovery::Interrupted(_)) => "interrupted",
            Some(_) => "other",
        };
        assert_eq!(actual, expected, "{} cut={name}", trace.label("projection"));
    }
}
