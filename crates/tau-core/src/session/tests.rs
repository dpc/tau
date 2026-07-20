use super::*;

/// Ensures extension-supplied typed image metadata cannot bypass the durable
/// provider-content byte/type validation boundary.
#[test]
fn provider_image_content_rejects_mismatched_media_bytes() {
    let result = tau_proto::ToolResult {
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
    let mut encoded = std::io::Cursor::new(Vec::new());
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
        agent_prompt_id: "ap-invalid-result".into(),
        agent_id: agent_id(),
        output_items: vec![ContextItem::ToolResult(tau_proto::ToolResultItem {
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
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
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

fn append_user_input(tree: &mut AgentTree, text: &str) -> AgentHead {
    tree.apply_event(&Event::AgentPromptSubmitted(
        tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id(),
            text: text.to_owned(),
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
            agent_prompt_id: "ap-tool-prefix".into(),
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
            agent_prompt_id: "ap-text-prefix".into(),
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
    successor.compact_prompt_id = "ap-agent-metadata-test-successor".into();
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
        agent_prompt_id: "ap-successor-continuation".into(),
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
        agent_prompt_id: "ap-owned-inference".into(),
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
        mismatched.agent_prompt_id = format!("{}-mismatch", mismatched.agent_prompt_id).into();
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
/// preserving model call order and then flushing message facts in the pending
/// input FIFO.
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
    assert!(
        tree.record_committed_message_fact(
            Box::new(tau_proto::MessageItem {
                role: tau_proto::ContextRole::User,
                content: vec![tau_proto::ContentPart::Text {
                    text: "<tau_message event=\"delivered\">later</tau_message>".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            }),
            PersistedAgentEventSeq::new(7),
        )
        .is_none(),
        "generic message fact must share the tool-adjacent pending input queue"
    );
    assert!(
        tree.apply_event_at(
            AgentEventParent::InheritHead,
            &Event::ProviderToolResult(tau_proto::ToolResult {
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
    let final_node = tree
        .node(final_node_id)
        .expect("message-fact node should exist");
    assert!(matches!(
        final_node.entry,
        AgentEntry::MessageFact {
            durable_event_seq,
            ..
        } if durable_event_seq == PersistedAgentEventSeq::new(7)
    ));
    let tool_results_node = tree
        .node(final_node.parent_id.expect("fact follows tool results"))
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
            internal_kind: None,
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
