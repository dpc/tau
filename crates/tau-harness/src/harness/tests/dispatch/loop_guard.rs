//! Tests for loop guard behavior.

use proptest::prelude::*;

use super::*;
use crate::harness::tool_runtime::{MAX_UTF8_BYTES_PER_SCALAR, tool_call_loop_signature};
use crate::harness::{LOOP_GUARD_TOOL_ARGUMENT_CHARS, bounded_loop_text};

fn legacy_tool_call_loop_signature(tool_name: &ToolName, arguments: &CborValue) -> String {
    format!(
        "{tool_name}:{}",
        bounded_loop_text(&format!("{arguments:?}"), LOOP_GUARD_TOOL_ARGUMENT_CHARS)
    )
}

fn assert_streamed_signature_matches_legacy(arguments: &CborValue) {
    let tool_name = ToolName::new("equivalence_tool");
    let expected = legacy_tool_call_loop_signature(&tool_name, arguments);
    let (actual, work) = tool_call_loop_signature(&tool_name, arguments);
    assert_eq!(actual, expected);
    assert!(
        work.formatted_scalars <= LOOP_GUARD_TOOL_ARGUMENT_CHARS + 1,
        "formatter inspected too many scalars: {work:?}"
    );
    assert!(
        work.formatted_bytes
            <= (LOOP_GUARD_TOOL_ARGUMENT_CHARS + 1).saturating_mul(MAX_UTF8_BYTES_PER_SCALAR),
        "formatter inspected too many bytes: {work:?}"
    );
    assert_eq!(work.output_allocations, 1);
}

fn arbitrary_cbor_value() -> impl Strategy<Value = CborValue> {
    let leaf = prop_oneof![
        Just(CborValue::Null),
        any::<bool>().prop_map(CborValue::Bool),
        any::<i64>().prop_map(|value| CborValue::Integer(value.into())),
        any::<f64>().prop_map(CborValue::Float),
        proptest::collection::vec(any::<u8>(), 0..32).prop_map(CborValue::Bytes),
        any::<String>().prop_map(CborValue::Text),
    ];
    leaf.prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..8).prop_map(CborValue::Array),
            proptest::collection::vec((inner.clone(), inner.clone()), 0..8)
                .prop_map(CborValue::Map),
            (any::<u64>(), inner).prop_map(|(tag, value)| CborValue::Tag(tag, Box::new(value))),
        ]
    })
}

/// The streaming sink must reproduce the legacy Debug-prefix signature for
/// every CBOR shape, including ordered maps, escapes, tags, and float details.
#[test]
fn streamed_tool_loop_signature_matches_legacy_cbor_corpus() {
    let values = [
        CborValue::Null,
        CborValue::Bool(true),
        CborValue::Integer((-42).into()),
        CborValue::Float(f64::NAN),
        CborValue::Bytes(vec![0, 1, 127, 255]),
        CborValue::Text("quote=\" slash=\\ control=\n unicode=🦀e\u{301}".to_owned()),
        CborValue::Array(vec![
            CborValue::Integer(1.into()),
            CborValue::Text("two".to_owned()),
        ]),
        CborValue::Map(vec![
            (
                CborValue::Text("z".to_owned()),
                CborValue::Text("first".to_owned()),
            ),
            (
                CborValue::Text("a".to_owned()),
                CborValue::Text("second".to_owned()),
            ),
        ]),
        CborValue::Tag(24, Box::new(CborValue::Array(vec![CborValue::Float(-0.0)]))),
    ];

    for value in values {
        assert_streamed_signature_matches_legacy(&value);
    }
}

/// Debug output immediately below, at, and above the 200-scalar limit must
/// preserve the old rule that only an omitted scalar adds the ellipsis.
#[test]
fn streamed_tool_loop_signature_preserves_exact_truncation_boundary() {
    let tool_name = ToolName::new("boundary_tool");

    for formatted_scalars in [
        LOOP_GUARD_TOOL_ARGUMENT_CHARS - 1,
        LOOP_GUARD_TOOL_ARGUMENT_CHARS,
        LOOP_GUARD_TOOL_ARGUMENT_CHARS + 1,
    ] {
        // `CborValue::Text` Debug adds the eight scalars in `Text("")`.
        let arguments = CborValue::Text("x".repeat(formatted_scalars - 8));
        assert_eq!(format!("{arguments:?}").chars().count(), formatted_scalars);
        let expected = legacy_tool_call_loop_signature(&tool_name, &arguments);
        let (actual, _) = tool_call_loop_signature(&tool_name, &arguments);
        assert_eq!(actual, expected);
        assert_eq!(
            actual.ends_with('…'),
            formatted_scalars > LOOP_GUARD_TOOL_ARGUMENT_CHARS
        );
    }
}

proptest! {
    /// Generated nested CBOR values must keep the old signature byte-for-byte;
    /// this guards formatter chunking and future ciborium Debug changes.
    #[test]
    fn streamed_tool_loop_signature_matches_generated_legacy_values(
        arguments in arbitrary_cbor_value()
    ) {
        assert_streamed_signature_matches_legacy(&arguments);
    }
}

/// Inputs from 1 KiB through 8 MiB must have input-size-independent formatter
/// work and one bounded output allocation while retaining exact old signatures.
#[test]
fn streamed_tool_loop_signature_bounds_large_input_work_and_allocations() {
    let cases = [
        CborValue::Text("x".repeat(1024)),
        CborValue::Bytes(vec![0x5a; 1024]),
        CborValue::Text("🦀".repeat((8 * 1024 * 1024) / '🦀'.len_utf8())),
        CborValue::Bytes(vec![0xa5; 8 * 1024 * 1024]),
        CborValue::Map(vec![(
            CborValue::Text("payload".to_owned()),
            CborValue::Text("m".repeat(8 * 1024 * 1024)),
        )]),
        CborValue::Array(vec![CborValue::Tag(
            42,
            Box::new(CborValue::Map(vec![(
                CborValue::Text("nested".to_owned()),
                CborValue::Text("n".repeat(8 * 1024 * 1024)),
            )])),
        )]),
    ];

    for arguments in cases {
        assert_streamed_signature_matches_legacy(&arguments);
    }
}

/// Arguments that collided after the old 200-scalar truncation must continue
/// to collide so repetition thresholds and loop-breaking order stay unchanged.
#[test]
fn streamed_tool_loop_signature_preserves_truncated_collisions() {
    let tool_name = ToolName::new("collision_tool");
    let common = "p".repeat(LOOP_GUARD_TOOL_ARGUMENT_CHARS + 32);
    let left = CborValue::Text(format!("{common}left"));
    let right = CborValue::Text(format!("{common}right"));

    let (left, _) = tool_call_loop_signature(&tool_name, &left);
    let (right, _) = tool_call_loop_signature(&tool_name, &right);

    assert_eq!(left, right);
    assert!(left.ends_with('…'));
}

#[test]
fn loop_guard_repeated_assistant_text_flows_through_provider_responses() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";

    for idx in 0..3 {
        let spid: AgentPromptId = test_agent_prompt_id(format!("sp-loop-text-{idx}"));
        seed_agent_thinking(&mut h, &cid, spid.as_str());
        h.prompt_coordination
            .prompt_runtime
            .agents
            .insert(spid.clone(), cid.clone());
        h.handle_provider_response_finished(provider_text_response(
            &spid,
            tau_proto::AgentId::parse("main").expect("agent id"),
            text,
        ))
        .expect("response handled");
    }

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered)
            if steered.message_class == tau_proto::PromptMessageClass::Internal
                && steered.text.contains("Loop guard:")
    )));
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("agent")
            .dispatch
            .pending_prompts
            .is_empty()
    );

    let spid = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .and_then(|conv| conv.dispatch.in_flight_prompt.clone())
        .expect("loop breaker prompt dispatched");
    h.handle_provider_response_finished(provider_text_response(
        &spid,
        tau_proto::AgentId::parse("main").expect("agent id"),
        text,
    ))
    .expect("response handled");

    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("agent")
            .execution
            .loop_guard
            .stop_automatic_continuation()
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.message.contains("Loop guard stopped automatic continuation")
    )));
}

/// A loop-guard stop cannot discard the typed wake that owns already-committed
/// agent-message activation.
#[test]
fn loop_guard_block_preserves_canonical_agent_message_wake() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .execution
        .loop_guard
        .mark_cycle_blocked("cycle");
    seed_tools_running(&mut h, &cid, vec!["done-call".into()]);
    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("loop-guard-agent-message")
                .expect("test identifier must satisfy its grammar"),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: crate::parse_agent_id(&recipient_id),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "external message".to_owned(),
        }),
    );

    h.maybe_complete_agent_turn_for(&cid, "done-call");

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered) if steered.text.contains("external message")
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(prompt)
            if prompt.agent_id.as_str() == recipient_id.as_str()
                && prompt.context.flatten().iter().any(|item| {
                    text_part(item).is_some_and(|text| text.contains("external message"))
                })
    )));
}
#[test]
fn loop_guard_detects_repeated_assistant_text_once_then_blocks() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";

    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.dispatch.pending_prompts.len(), 1);
    assert!(
        conv.dispatch.pending_prompts[0]
            .text
            .contains("Loop guard:")
    );
    assert!(!conv.execution.loop_guard.stop_automatic_continuation());

    h.mark_loop_guard_breakers_dispatched(&cid);
    h.record_assistant_loop_signature(&cid, Some(text));

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.dispatch.pending_prompts.len(), 1);
    assert!(conv.execution.loop_guard.stop_automatic_continuation());
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.message.contains("Loop guard stopped automatic continuation")
    )));
}

#[test]
fn loop_guard_progress_reset_preserves_in_flight_argument_signatures() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for (idx, path) in ["a.txt", "b.txt", "c.txt"].into_iter().enumerate() {
        h.remember_tool_call_loop_signature(
            &cid,
            &crate::harness::AgentToolCall {
                call_ref: None,
                id: format!("call-{idx}").into(),
                name: ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(path.to_owned()),
                )]),
            },
        );
    }

    h.reset_loop_guard_for_progress(&cid);

    for idx in 0..3 {
        h.record_tool_failure_loop_signature(
            &cid,
            &loop_guard_tool_error(&format!("call-{idx}"), "read", "generic failure"),
        );
    }

    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("agent")
            .dispatch
            .pending_prompts
            .is_empty()
    );
}

#[test]
fn loop_guard_same_batch_failures_do_not_block_before_breaker_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for idx in 0..4 {
        h.record_tool_failure_loop_signature(
            &cid,
            &loop_guard_tool_error(&format!("call-{idx}"), "tool", &format!("failure {idx}")),
        );
    }

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.dispatch.pending_prompts.len(), 1);
    assert!(!conv.execution.loop_guard.stop_automatic_continuation());
}

#[test]
fn loop_guard_detects_abab_suffix() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    h.record_assistant_loop_signature(
        &cid,
        Some("First repeated long assistant state with no concrete action."),
    );
    h.record_tool_failure_loop_signature(
        &cid,
        &loop_guard_tool_error("call-a", "read", "first distinct failure"),
    );
    h.record_assistant_loop_signature(
        &cid,
        Some("First repeated long assistant state with no concrete action."),
    );
    h.record_tool_failure_loop_signature(
        &cid,
        &loop_guard_tool_error("call-b", "read", "first distinct failure"),
    );

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.dispatch.pending_prompts.len(), 1);
    assert!(conv.dispatch.pending_prompts[0].text.contains("A/B/A/B"));
}

#[test]
fn loop_guard_resets_on_user_progress() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";

    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.reset_loop_guard_for_progress(&cid);
    h.record_assistant_loop_signature(&cid, Some(text));

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert!(conv.dispatch.pending_prompts.is_empty());
}

#[test]
fn loop_guard_resets_on_agent_head_move() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("agent id")
        .to_owned();
    h.publish_pending_prompt_for_agent(&cid, PendingPrompt::user("seed".to_owned()))
        .expect("publish seed");
    let node_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent")
        .identity
        .head
        .expect("agent head");
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));

    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: crate::parse_agent_id(&agent_id),
            head: tau_proto::AgentHead::Node(node_id),
        }),
    );

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert!(
        !conv
            .dispatch
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_loop_guard)
    );
}
#[test]
fn loop_guard_detects_repeated_same_failing_tool_call() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for idx in 0..3 {
        h.record_tool_failure_loop_signature(
            &cid,
            &loop_guard_tool_error(&format!("call-{idx}"), "read", "missing file"),
        );
    }

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.dispatch.pending_prompts.len(), 1);
    assert!(
        conv.dispatch.pending_prompts[0]
            .text
            .contains("repeated identical failing tool call")
    );
}

#[test]
fn loop_guard_repeated_tool_failure_signature_includes_arguments() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for (idx, path) in ["a.txt", "b.txt"].into_iter().enumerate() {
        let call = crate::harness::AgentToolCall {
            call_ref: None,
            id: format!("call-{idx}").into(),
            name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text(path.to_owned()),
            )]),
        };
        h.remember_tool_call_loop_signature(&cid, &call);
        h.record_tool_failure_loop_signature(
            &cid,
            &loop_guard_tool_error(call.id.as_str(), "read", "generic failure"),
        );
    }

    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("agent")
            .dispatch
            .pending_prompts
            .is_empty()
    );
}

#[test]
fn loop_guard_production_tool_failure_signature_includes_arguments() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for idx in 0..3 {
        h.execute_agent_tool_call(
            &cid,
            &crate::harness::AgentToolCall {
                call_ref: None,
                id: format!("distinct-{idx}").into(),
                name: ToolName::new("missing_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(format!("file-{idx}.txt")),
                )]),
            },
        )
        .expect("tool call handled");
    }
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("agent")
            .dispatch
            .pending_prompts
            .is_empty()
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.text.contains("Loop guard:")
    )));

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    for idx in 0..3 {
        h.execute_agent_tool_call(
            &cid,
            &crate::harness::AgentToolCall {
                call_ref: None,
                id: format!("same-{idx}").into(),
                name: ToolName::new("missing_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text("same.txt".to_owned()),
                )]),
            },
        )
        .expect("tool call handled");
    }
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.text.contains("repeated identical failing tool call")
    )));
}

#[test]
fn loop_guard_detects_consecutive_different_tool_failures() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for idx in 0..4 {
        h.record_tool_failure_loop_signature(
            &cid,
            &loop_guard_tool_error(&format!("call-{idx}"), "tool", &format!("failure {idx}")),
        );
    }

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.dispatch.pending_prompts.len(), 1);
    assert!(
        conv.dispatch.pending_prompts[0]
            .text
            .contains("several consecutive tool failures")
    );
}

#[test]
fn loop_guard_resets_on_successful_terminal_tool_result() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.record_tool_failure_loop_signature(
        &cid,
        &loop_guard_tool_error("failed-call", "read", "missing file"),
    );

    h.publish_terminal_tool_result(
        Some(&cid),
        None,
        tau_proto::ToolResult {
            presentation: Default::default(),
            call_id: "ok-call".into(),
            tool_name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("ok".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        },
    );

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.execution.loop_guard.consecutive_tool_failures(), 0);
    assert!(conv.dispatch.pending_prompts.is_empty());
}

#[test]
fn loop_guard_provider_repetition_response_queues_pivot_then_blocks() {
    // Provider-side stream repetition is trusted only as a loop-guard trigger:
    // the provider's error is display text, while the harness uses a fixed pivot
    // reason and blocks only after the breaker was tried.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);

    let spid: AgentPromptId = test_agent_prompt_id("sp-provider-repetition-1");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    h.handle_provider_response_finished(provider_repetition_response(
        &spid,
        tau_proto::AgentId::parse("main").expect("agent id"),
    ))
    .expect("response handled");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered)
            if steered.message_class == tau_proto::PromptMessageClass::Internal
                && steered.text.contains("provider detected a tight exact stream repetition")
    )));
    let spid = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .and_then(|conv| conv.dispatch.in_flight_prompt.clone())
        .expect("loop breaker prompt dispatched");
    h.handle_provider_response_finished(provider_repetition_response(
        &spid,
        tau_proto::AgentId::parse("main").expect("agent id"),
    ))
    .expect("response handled");

    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("agent")
            .execution
            .loop_guard
            .stop_automatic_continuation()
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.level == tau_proto::NoticeLevel::Warning
                && notice.purpose == tau_proto::NoticePurpose::Alert
                && notice.message.contains("Loop guard stopped automatic continuation")
    )));
}

#[test]
fn loop_guard_resets_when_user_prompt_is_published() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));

    h.reset_loop_guard_progress_reset_count_for_test();
    h.publish_pending_prompt_for_agent(&cid, PendingPrompt::user("new direction".to_owned()))
        .expect("publish user prompt");

    assert_eq!(h.loop_guard_progress_reset_count_for_test(), 1);
    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert!(
        !conv
            .dispatch
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_loop_guard)
    );
}

/// Ordinary dispatch owns one progress reset before it reaches publication, so
/// publication must not scan and retain the pending queue a second time.
#[test]
fn loop_guard_ordinary_dispatch_resets_once_before_publication() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .pending_prompts
        .push_back(PendingPrompt::loop_guard("stale pivot".to_owned()));

    h.reset_loop_guard_progress_reset_count_for_test();
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("new direction".to_owned()))
        .expect("dispatch user prompt");

    assert_eq!(h.loop_guard_progress_reset_count_for_test(), 1);
    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert!(
        !conv
            .dispatch
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_loop_guard),
        "the single reset must retain the old pivot-removal outcome"
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted) if submitted.text == "new direction"
    )));
}

#[test]
fn loop_guard_reset_removes_pending_breaker_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("agent")
            .dispatch
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_loop_guard)
    );

    h.publish_pending_prompt_for_agent(&cid, PendingPrompt::user("new input".to_owned()))
        .expect("publish user prompt");

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert!(
        !conv
            .dispatch
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_loop_guard)
    );
}

#[test]
fn loop_guard_resets_when_user_prompt_is_queued() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("agent id")
        .to_owned();
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("sp-loop-queued-reset"),
    };
    h.record_assistant_loop_signature(
        &cid,
        Some("I will keep trying the same plan without taking any tool action or making progress."),
    );

    h.reset_loop_guard_progress_reset_count_for_test();
    let submission = h
        .submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            &agent_id,
            PendingPrompt::user("queued user input".to_owned()),
        )
        .expect("submit prompt");

    assert_eq!(submission, PromptSubmission::Queued);
    assert_eq!(h.loop_guard_progress_reset_count_for_test(), 1);
    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.dispatch.pending_prompts.len(), 1);
}

#[test]
fn loop_guard_queued_user_prompt_removes_pending_breaker() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("agent id")
        .to_owned();
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("sp-loop-queued-breaker-reset"),
    };

    let submission = h
        .submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            &agent_id,
            PendingPrompt::user("queued user input".to_owned()),
        )
        .expect("submit prompt");

    assert_eq!(submission, PromptSubmission::Queued);
    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert!(
        !conv
            .dispatch
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_loop_guard)
    );
}

/// Passive completion and restore notices form an extra publication batch, but
/// the outer ordinary prompt still performs exactly one reset before either
/// notice can publish.
#[test]
fn loop_guard_passive_restore_extra_batch_resets_once() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .pending_prompts
        .extend([
            PendingPrompt::loop_guard("stale pivot".to_owned()),
            PendingPrompt::passive_background_completion("passive completion".to_owned()),
        ]);
    h.prompt_coordination
        .pending_notices
        .restore_sessions
        .insert(h.session_runtime.current_session_id.clone(), None);

    h.reset_loop_guard_progress_reset_count_for_test();
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("new direction".to_owned()))
        .expect("dispatch user prompt with notices");

    assert_eq!(h.loop_guard_progress_reset_count_for_test(), 1);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.text == "passive completion"
                && submitted.internal_kind
                    == Some(tau_proto::InternalPromptKind::BackgroundToolCompletion)
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.message_class == tau_proto::PromptMessageClass::Internal
                && submitted.text.contains("state of the world")
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted) if submitted.text == "new direction"
    )));
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .all(|prompt| !prompt.is_loop_guard()),
        "the batch must retain the reset's stale-pivot removal"
    );
}

/// Rejected dispatches must not reset a terminating agent before any
/// publication work begins.
#[test]
fn loop_guard_rejected_dispatch_does_not_reset_before_publication() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .terminating = true;

    h.reset_loop_guard_progress_reset_count_for_test();
    assert!(
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("new direction".to_owned()))
            .is_err()
    );

    assert_eq!(h.loop_guard_progress_reset_count_for_test(), 0);
}

/// Folding an ordinary queued user prompt removes a stale loop-guard pivot from
/// the same batch so only the user's real progress reaches the next prompt.
#[test]
fn loop_guard_folding_user_prompt_drops_stale_pivot_from_batch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    {
        let conv = h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        conv.dispatch
            .pending_prompts
            .push_back(PendingPrompt::user("queued user input".to_owned()));
    }
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));

    h.fold_pending_prompts_as_steered(&cid);

    assert!(!event_log_contains_any_source(&h, |event| {
        matches!(
            event,
            Event::AgentPromptSteered(steered) if steered.text.contains("Loop guard:")
        )
    }));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered) if steered.text == "queued user input"
    )));
}
#[test]
fn loop_guard_resets_on_successful_background_tool_result() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    publish_test_tool_declaration(&mut h, &cid, "bg-call");
    h.record_tool_failure_loop_signature(
        &cid,
        &loop_guard_tool_error("failed-call", "read", "missing file"),
    );
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert("bg-call".into(), cid.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        "bg-call".into(),
        PendingTool {
            name: ToolName::new("read"),
            internal_name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );

    h.handle_background_tool_result(
        &crate::test_connection_id("conn-bg"),
        tau_proto::ToolResult {
            presentation: Default::default(),
            call_id: "bg-call".into(),
            tool_name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("ok".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        },
    );

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.execution.loop_guard.consecutive_tool_failures(), 0);
    assert!(
        conv.dispatch
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_activating_background_completion),
        "committed background result should queue its completion prompt"
    );
}
