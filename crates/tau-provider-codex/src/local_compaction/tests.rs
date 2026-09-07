use super::*;
use crate::common::{MessageAccumulator, ToolCallAccumulator};

/// Create synthetic parser state for terminal-shape validation, without
/// pretending that these test-only slots passed stream accounting.
fn state(items: Vec<OutputItemAccumulator>) -> StreamState {
    let mut state = StreamState::new();
    state.output_items = items;
    state
}

/// One bounded assistant narrative is accepted byte-for-byte. Empty,
/// multiple, oversized, and tool-bearing output must never reach history,
/// including tool artifacts ordinary inference would otherwise drop.
#[test]
fn local_summary_validates_complete_private_output() {
    let message = |text: String| {
        OutputItemAccumulator::Message(MessageAccumulator {
            text,
            ..Default::default()
        })
    };
    let text = "  Preserve these exact bytes.  ";
    assert!(matches!(
        validate_narrative(state(vec![message(text.to_owned())])),
        Ok(tau_proto::ContextItem::LocalCompactionNarrative(item)) if item.narrative == text
    ));
    for items in [
        vec![],
        vec![message(" ".to_owned())],
        vec![message("a".to_owned()), message("b".to_owned())],
        vec![message(
            "x".repeat(tau_proto::LOCAL_COMPACTION_NARRATIVE_MAX_BYTES + 1),
        )],
        vec![
            message("valid text".to_owned()),
            OutputItemAccumulator::ToolCall(ToolCallAccumulator::new(
                tau_proto::ToolType::Function,
            )),
        ],
        vec![OutputItemAccumulator::Compaction(None)],
    ] {
        assert!(validate_narrative(state(items)).is_err());
    }
    let mut reasoning = state(vec![message("summary".to_owned())]);
    reasoning.thinking = Some("r".repeat(tau_proto::LOCAL_COMPACTION_NARRATIVE_MAX_BYTES + 1));
    assert!(validate_narrative(reasoning).is_err());
}

/// Original-event validation rejects attempted tools before projection, so
/// overwriting the same output index later cannot hide a forbidden call.
/// An error with embedded item-shaped fields still belongs to the error parser.
#[test]
fn local_summary_rejects_forbidden_original_events_before_projection() {
    for item in [
        serde_json::json!({"type": "function_call", "name": ""}),
        serde_json::json!({"type": "custom_tool_call", "name": "run"}),
        serde_json::json!({"type": "web_search_call"}),
        serde_json::json!({"type": "message", "role": "user"}),
        serde_json::json!({"type": "message", "role": "assistant",
            "content": [{"type": "refusal", "refusal": "no"}]}),
    ] {
        assert!(
            validate_event(&serde_json::json!({
                "type": "response.output_item.added", "output_index": 0, "item": item
            }))
            .is_err()
        );
        assert!(
            validate_event(&serde_json::json!({
                "type": "response.completed", "response": {"output": [item]}
            }))
            .is_err()
        );
        assert!(
            validate_event(&serde_json::json!({
                "type": "error", "code": "compaction_not_supported", "item": item
            }))
            .is_ok()
        );
    }
    for kind in [
        "response.function_call_arguments.delta",
        "response.web_search_call.in_progress",
    ] {
        assert!(validate_event(&serde_json::json!({"type": kind})).is_err());
    }
}

/// Discarding reasoning must not bypass its acceptance byte bound: both
/// opaque item payloads and aggregate terminal-only reasoning are bounded, as
/// is the combination of retained opaque items and streamed visible reasoning.
#[test]
fn local_summary_bounds_opaque_and_terminal_reasoning() {
    let reasoning = |size| {
        serde_json::json!({
            "type": "reasoning", "encrypted_content": "r".repeat(size)
        })
    };
    let limit = tau_proto::LOCAL_COMPACTION_NARRATIVE_MAX_BYTES;
    let huge = reasoning(limit + 1);
    for kind in ["response.output_item.added", "response.output_item.done"] {
        assert!(
            validate_event(&serde_json::json!({
                "type": kind, "output_index": 0, "item": huge
            }))
            .is_err()
        );
    }
    let medium = reasoning(limit / 2);
    assert!(
        validate_event(&serde_json::json!({
            "type": "response.completed", "response": {"output": [medium, medium]}
        }))
        .is_err()
    );
    let opaque = tau_proto::OpaqueProviderItem::from_raw_json(medium.to_string()).expect("opaque");
    let mut state = state(vec![
        OutputItemAccumulator::Reasoning(opaque),
        OutputItemAccumulator::Message(MessageAccumulator {
            text: "summary".to_owned(),
            ..Default::default()
        }),
    ]);
    state.thinking = Some("r".repeat(limit / 2));
    assert!(validate_narrative(state).is_err());
}
