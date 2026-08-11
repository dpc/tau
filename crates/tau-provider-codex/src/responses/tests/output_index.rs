use super::*;

/// Pins the cold-state boundary so one event may create 1,024 slots but cannot
/// request the 1,025th sparse slot.
#[test]
fn output_index_bounds_empty_state_slot_growth() {
    let mut accepted = path_crate_common::StreamState::new();
    let accepted_event = serde_json::json!({
        "type": "response.output_text.delta",
        "output_index": 1023,
        "delta": "accepted",
    });
    apply_event(&mut accepted, &accepted_event, &mut |_| {}).expect("growth of 1,024 slots");
    assert_eq!(accepted.output_items.len(), 1024);
    assert_eq!(accepted.text, "accepted");

    for rejected_index in [1024_u64, u64::MAX] {
        let mut rejected = path_crate_common::StreamState::new();
        let event = serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": rejected_index,
            "delta": "must not apply",
        });
        let error = apply_event(&mut rejected, &event, &mut |_| {})
            .expect_err("sparse output index must be rejected");
        assert_invalid_output_index(error);
        assert!(rejected.output_items.is_empty());
        assert!(rejected.text.is_empty());
    }
}

/// Proves that the bound measures new slots relative to current state rather
/// than imposing an absolute output-item ceiling.
#[test]
fn output_index_bounds_relative_slot_growth() {
    let mut state = path_crate_common::StreamState::new();
    for (index, text) in [(1, "seed"), (1025, "exact growth")] {
        let event = serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": index,
            "delta": text,
        });
        apply_event(&mut state, &event, &mut |_| {}).expect("allowed relative growth");
    }
    assert_eq!(state.output_items.len(), 1026);

    let rejected = serde_json::json!({
        "type": "response.output_text.delta",
        "output_index": 2050,
        "delta": "one slot too far",
    });
    let error = apply_event(&mut state, &rejected, &mut |_| {})
        .expect_err("growth of 1,025 slots must fail");
    assert_invalid_output_index(error);
    assert_eq!(state.output_items.len(), 1026);
    assert!(!state.text.contains("one slot too far"));
}

/// Ensures dense streams can cross slot 1,024 because each event requests only
/// one new slot.
#[test]
fn output_index_allows_dense_output_beyond_1024_items() {
    let mut state = path_crate_common::StreamState::new();
    for index in 0..=1024 {
        let event = serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": index,
            "delta": "x",
        });
        apply_event(&mut state, &event, &mut |_| {}).expect("dense output must remain valid");
    }
    assert_eq!(state.output_items.len(), 1025);
    assert_eq!(state.text.len(), 1025);
}

/// Preserves lower and duplicate provider indexes while keeping durable items
/// in provider-index order.
#[test]
fn output_index_allows_backward_and_duplicate_events() {
    let mut state = path_crate_common::StreamState::new();
    for (index, text) in [(2, "later"), (0, "first"), (2, "later")] {
        let event = serde_json::json!({
            "type": "response.output_item.done",
            "output_index": index,
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": text }],
            },
        });
        apply_event(&mut state, &event, &mut |_| {}).expect("backward or duplicate output index");
    }
    let items = state.into_output_items();
    assert_eq!(items.len(), 2);
    let tau_proto::ContextItem::Message(first) = &items[0] else {
        panic!("expected first message");
    };
    let tau_proto::ContextItem::Message(later) = &items[1] else {
        panic!("expected later message");
    };
    assert!(matches!(&first.content[0], tau_proto::ContentPart::Text { text } if text == "first"));
    assert!(matches!(&later.content[0], tau_proto::ContentPart::Text { text } if text == "later"));
}

/// Rejects every malformed present index while retaining the historical
/// absent-index alias to slot zero.
#[test]
fn output_index_rejects_malformed_present_values_and_defaults_absent_to_zero() {
    for output_index in [
        serde_json::json!(-1),
        serde_json::json!(1.5),
        serde_json::json!("1"),
        serde_json::Value::Null,
    ] {
        let mut state = path_crate_common::StreamState::new();
        let event = serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": output_index,
            "delta": "must not apply",
        });
        let error = apply_event(&mut state, &event, &mut |_| {})
            .expect_err("malformed present output index");
        assert_invalid_output_index(error);
        assert!(state.output_items.is_empty());
    }

    let mut state = path_crate_common::StreamState::new();
    let absent = serde_json::json!({
        "type": "response.output_text.delta",
        "delta": "slot zero",
    });
    apply_event(&mut state, &absent, &mut |_| {}).expect("absent index remains slot zero");
    assert_eq!(state.output_items.len(), 1);
    assert_eq!(state.text, "slot zero");
}

/// Rejects an index outside the public progress representation independently
/// of sparse-growth and target-width checks.
#[test]
fn output_index_rejects_value_outside_public_u32_range() {
    let event = serde_json::json!({ "output_index": u64::from(u32::MAX) + 1 });
    let error = parse_output_index(&event).expect_err("index must fit the public progress type");
    assert_invalid_output_index(error);
}

/// Exercises each indexed handler family and proves rejection occurs before
/// output, repetition, reasoning, or callback mutation.
#[test]
fn output_index_rejection_precedes_indexed_handler_mutation() {
    let events = [
        serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": 1024,
            "delta": ".".repeat(1024),
        }),
        serde_json::json!({
            "type": "response.output_text.done",
            "output_index": 1024,
            "text": "must not apply",
        }),
        serde_json::json!({
            "type": "response.reasoning_summary_text.delta",
            "output_index": 1024,
            "delta": ".".repeat(1024),
        }),
        serde_json::json!({
            "type": "response.reasoning_summary_part.added",
            "output_index": 1024,
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 1024,
            "delta": "_clone".repeat(180),
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.done",
            "output_index": 1024,
            "arguments": "{}",
        }),
        serde_json::json!({
            "type": "response.custom_tool_call_input.delta",
            "output_index": 1024,
            "delta": "_clone".repeat(180),
        }),
        serde_json::json!({
            "type": "response.custom_tool_call_input.done",
            "output_index": 1024,
            "input": "input",
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 1024,
            "item": {
                "type": "message",
                "role": "assistant",
            },
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 1024,
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "must not apply" }],
            },
        }),
    ];
    for event in events {
        let mut state = path_crate_common::StreamState::new();
        let mut update_count = 0;
        let error = apply_event(&mut state, &event, &mut |_| update_count += 1)
            .expect_err("sparse output index must precede handler checks");
        assert_invalid_output_index(error);
        assert!(state.output_items.is_empty());
        assert!(state.text.is_empty());
        assert!(state.thinking.is_none());
        assert_eq!(state.non_visible_output_bytes(), 0);
        assert_eq!(update_count, 0);

        let accepted_reasoning = serde_json::json!({
            "type": "response.reasoning_summary_text.delta",
            "output_index": 0,
            "delta": "accepted",
        });
        apply_event(&mut state, &accepted_reasoning, &mut |_| {})
            .expect("rejected index must not capture reasoning ownership");
        let mut emitter = path_crate_common::StreamDeltaEmitter::default();
        assert!(matches!(
            emitter.deltas(&state).as_slice(),
            [tau_proto::ProviderResponseTextDelta::ReasoningText {
                output_index: 0,
                ..
            }]
        ));
    }
}

/// Checks the closed, content-free error returned for every invalid output
/// index condition.
fn assert_invalid_output_index(error: LlmError) {
    match error {
        LlmError::InvalidResponse(message) => assert_eq!(
            message,
            "provider output index advances beyond the sparse-slot limit"
        ),
        other => panic!("expected invalid output index, got {other:?}"),
    }
}
