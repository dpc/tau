//! Regression tests for lightweight terminal tool-call projection.

use tau_proto::{
    CborValue, ContentPart, ContextRecoveryDisposition, ContextRole, MessageItem,
    OutputLengthDisposition, PromptOriginator, ProviderResponseFinished, ProviderStopReason,
    ResponsesToolCallEnvelope, ToolCallItem, ToolName, ToolType,
};

use super::terminal_tool_calls::*;
use super::*;

/// Builds one call whose payload allocations must remain owned exclusively
/// by the canonical terminal event.
fn call(
    call_id: &str,
    name: &str,
    tool_type: ToolType,
    arguments: CborValue,
    raw_arguments_json: Option<String>,
    responses_envelope: Option<ResponsesToolCallEnvelope>,
) -> ContextItem {
    ContextItem::ToolCall(ToolCallItem {
        call_id: call_id.into(),
        name: ToolName::new(name),
        tool_type,
        arguments,
        raw_arguments_json,
        responses_envelope,
    })
}

/// Materializes the removed implementation so retained projection behavior
/// stays differential-tested against its exact call selection and order.
fn legacy_cloned_calls(output_items: &[ContextItem]) -> Vec<ToolCallItem> {
    output_items
        .iter()
        .filter_map(|item| match item {
            ContextItem::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

/// Wraps output items in the self-contained terminal shape used by dispatch.
fn finished(
    output_items: Vec<ContextItem>,
    stop_reason: ProviderStopReason,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: "terminal-projection".parse().expect("valid prompt id"),
        agent_id: "terminal-agent".parse().expect("valid agent id"),
        output_items,
        stop_reason,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: ContextRecoveryDisposition::None,
        output_length_disposition: OutputLengthDisposition::None,
        originator: PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

/// Zero, one, and many calls retain legacy selection and provider order while
/// reporting one traversal and every lightweight metadata operation.
#[test]
fn projection_matches_legacy_selection_for_zero_one_and_many_calls() {
    let mut many = vec![ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text {
            text: "before".to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    })];
    for index in 0..32 {
        many.push(call(
            &format!("many-{index}"),
            if index % 2 == 0 { "function" } else { "custom" },
            if index % 2 == 0 {
                ToolType::Function
            } else {
                ToolType::Custom
            },
            CborValue::Null,
            None,
            None,
        ));
    }
    for output_items in [
        vec![],
        vec![call(
            "one",
            "function",
            ToolType::Function,
            CborValue::Null,
            None,
            None,
        )],
        {
            let mut mixed = vec![
                ContextItem::Message(MessageItem {
                    role: ContextRole::Assistant,
                    content: vec![ContentPart::Text {
                        text: "before".to_owned(),
                    }],
                    phase: None,
                    responses_raw_json: None,
                }),
                call(
                    "first",
                    "function",
                    ToolType::Function,
                    CborValue::Null,
                    None,
                    None,
                ),
                ContextItem::Message(MessageItem {
                    role: ContextRole::Assistant,
                    content: vec![ContentPart::HarnessInternalText {
                        text: "between".to_owned(),
                    }],
                    phase: None,
                    responses_raw_json: None,
                }),
                call(
                    "second",
                    "custom",
                    ToolType::Custom,
                    CborValue::Null,
                    None,
                    None,
                ),
            ];
            mixed.extend(many);
            mixed
        },
    ] {
        let first_tool_index = output_items
            .iter()
            .position(|item| matches!(item, ContextItem::ToolCall(_)));
        let expected_slots = first_tool_index
            .map(|index| output_items.len() - index)
            .unwrap_or(0);
        let legacy = legacy_cloned_calls(&output_items);
        let finished = finished(output_items, ProviderStopReason::ToolCalls);
        let projection = TerminalToolCalls::from_finished(&finished);
        assert_eq!(projection.len(), legacy.len());
        for (projected, cloned) in projection.iter().zip(&legacy) {
            assert_eq!(projected.call_id, cloned.call_id);
            assert_eq!(projected.name, cloned.name);
            assert!(projected.admitted);
        }
        assert_eq!(
            projection.work(),
            TerminalToolCallWork {
                output_items_visited: finished.output_items.len(),
                metadata_buffers_allocated: usize::from(first_tool_index.is_some()),
                metadata_slots_reserved: expected_slots,
                metadata_fields_cloned: legacy.len() * 2,
            }
        );
    }
}

/// An 8 MiB Unicode argument and raw JSON string stay outside the lightweight
/// metadata retained by terminal classification.
#[test]
fn projection_excludes_large_unicode_arguments_and_raw_sidecars() {
    let large_unicode = "🦀".repeat(2 * 1024 * 1024);
    let large_raw = format!("{{\"payload\":\"{large_unicode}\"}}");
    let output_items = vec![call(
        "large",
        "custom",
        ToolType::Custom,
        CborValue::Text(large_unicode),
        Some(large_raw),
        Some(ResponsesToolCallEnvelope {
            item_id: Some("provider-item".to_owned()),
            status: Some("completed".to_owned()),
            extra_fields: Some(CborValue::Text("raw-sidecar-🦀".repeat(512))),
        }),
    )];
    let finished = finished(output_items, ProviderStopReason::ToolCalls);
    let projection = TerminalToolCalls::from_finished(&finished);
    let projected = projection.iter().next().expect("projected call");
    let canonical = match &finished.output_items[0] {
        ContextItem::ToolCall(call) => call,
        _ => unreachable!("fixture is a call"),
    };

    assert_eq!(projected.call_id, canonical.call_id);
    assert_eq!(projected.name, canonical.name);
}
