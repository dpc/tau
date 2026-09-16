use super::*;

/// Activity counts accepted notifications, not sequence numbers, and saturates.
#[test]
fn compacting_counts_validated_updates_only() {
    let mut shape = CompactStreamShape::default();
    let added = serde_json::json!({"type":"response.output_item.added","output_index":0,
        "item":{"type":"compaction","id":"cmp_1"}});
    let progress = serde_json::json!({"type":"response.compaction.compacting",
        "output_index":0,"item_id":"cmp_1","sequence_number":3});
    assert!(shape.validate(&progress).is_err());
    assert_eq!(shape.progress_updates, 0);
    shape.validate(&added).expect("valid added item");
    for count in 1..=2 {
        shape.validate(&progress).expect("valid progress");
        assert_eq!(shape.progress_updates, count);
        assert_eq!(shape.item, CompactItemPhase::Added);
    }
    for invalid in [
        serde_json::json!({"output_index":1,"item_id":"cmp_1"}),
        serde_json::json!({"output_index":0,"item_id":"other"}),
        serde_json::json!({"output_index":0}),
        serde_json::json!({"item_id":"cmp_1"}),
    ] {
        let mut event = invalid;
        event["type"] = "response.compaction.compacting".into();
        assert!(shape.validate(&event).is_err());
        assert_eq!(shape.progress_updates, 2);
    }
    shape.progress_updates = u64::MAX;
    shape.validate(&progress).expect("saturating progress");
    assert_eq!(shape.progress_updates, u64::MAX);
    shape
        .validate(
            &serde_json::json!({"type":"response.output_item.done","output_index":0,
        "item":{"type":"compaction","id":"cmp_1"}}),
        )
        .expect("valid completed item");
    assert!(shape.validate(&progress).is_err());
}

/// The native compact parser accepts only one completed slot-zero
/// compaction item followed by the documented completion terminal.
#[test]
fn accepts_exact_compact_shape() {
    validate(&[
        r#"{"type":"response.created"}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"compaction","id":"cmp_1"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"compaction","id":"cmp_1","encrypted_content":"opaque"}}"#,
        r#"{"type":"response.completed","response":{"status":"completed"}}"#,
    ])
    .expect("exact compact shape");
}

/// A completed compaction item may be the first slot event because Codex
/// recordings and live streams need not include the optional added event.
#[test]
fn accepts_direct_completed_compaction_item() {
    validate(&[
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"compaction"}}"#,
        r#"{"type":"response.completed"}"#,
    ])
    .expect("direct completed compaction");
}

/// ChatGPT's transport-timing telemetry may follow a completed compact item;
/// it carries no compact output and must not invalidate or advance the shape.
#[test]
fn accepts_websocket_timing_telemetry_without_changing_compact_state() {
    let mut shape = CompactStreamShape::default();
    let timing = serde_json::json!({
        "type": "responsesapi.websocket_timing",
        "timing_metrics": {
            "response_id": "provider-private",
            "total_turn_time_s": 1.25,
            "critical_path": {"kind": "provider-private"}
        }
    });
    shape
        .validate_observed(&timing, "ap-test")
        .expect("pre-output timing telemetry");
    assert_eq!(shape.item, CompactItemPhase::Missing);
    shape
        .validate(
            &serde_json::json!({"type":"response.output_item.done","output_index":0,
                "item":{"type":"compaction","id":"cmp_1"}}),
        )
        .expect("completed compaction");
    shape
        .validate_observed(&timing, "ap-test")
        .expect("post-output timing telemetry");
    assert_eq!(shape.item, CompactItemPhase::Done);
    assert_eq!(shape.progress_updates, 0);
    assert!(
        shape
            .validate(&serde_json::json!({
                "type": "responsesapi.websocket_timing.extra",
                "timing_metrics": {}
            }))
            .is_err(),
        "near-match telemetry kinds must remain fail-closed"
    );
    assert_eq!(shape.item, CompactItemPhase::Done);
    assert_eq!(shape.progress_updates, 0);
}

/// Same-index type replacement must fail before the ordinary accumulator
/// can overwrite the earlier slot and hide the original event history.
#[test]
fn rejects_same_index_type_replacement() {
    assert_rejected(&[
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"compaction"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}"#,
        r#"{"type":"response.completed"}"#,
    ]);
}

/// A done event with a different compaction identity is a same-type slot
/// replacement and must not overwrite the added provider item.
#[test]
fn rejects_same_index_compaction_identity_replacement() {
    assert_rejected(&[
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"compaction","id":"cmp_A"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"compaction","id":"cmp_B"}}"#,
        r#"{"type":"response.completed"}"#,
    ]);
}

/// A second compaction item must not overwrite the first accepted item.
#[test]
fn rejects_duplicate_compaction() {
    assert_rejected(&[
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"compaction"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"compaction"}}"#,
        r#"{"type":"response.completed"}"#,
    ]);
}

/// An added item without its matching done event is not a completed compact
/// slot and must fail at the terminal.
#[test]
fn rejects_incomplete_slot() {
    assert_rejected(&[
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"compaction"}}"#,
        r#"{"type":"response.completed"}"#,
    ]);
}

/// Completion without any compact output must not install an empty
/// boundary.
#[test]
fn rejects_missing_slot() {
    assert_rejected(&[r#"{"type":"response.completed"}"#]);
}

/// A second output index is extra compact output even if projection would
/// later discard or collapse it.
#[test]
fn rejects_extra_slot() {
    assert_rejected(&[
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"compaction"}}"#,
        r#"{"type":"response.completed"}"#,
    ]);
}

/// Non-item semantic output is outside the compact-only response language.
#[test]
fn rejects_extra_output() {
    assert_rejected(&[
        r#"{"type":"response.output_text.delta","output_index":0,"delta":"summary"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"compaction"}}"#,
        r#"{"type":"response.completed"}"#,
    ]);
}

/// Compact slot events require their original explicit index rather than
/// the ordinary parser's missing-index compatibility default.
#[test]
fn rejects_missing_output_index() {
    assert_rejected(&[
        r#"{"type":"response.output_item.done","item":{"type":"compaction"}}"#,
        r#"{"type":"response.completed"}"#,
    ]);
}

/// Native standalone compaction requires `response.completed`; the ordinary
/// inference parser's legacy `response.done` compatibility must not leak
/// in.
#[test]
fn rejects_wrong_terminal() {
    assert_rejected(&[
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"compaction"}}"#,
        r#"{"type":"response.done"}"#,
    ]);
}

fn validate(events: &[&str]) -> Result<(), LlmError> {
    let mut shape = CompactStreamShape::default();
    for raw in events {
        let event = serde_json::from_str(raw).expect("valid test event");
        shape.validate(&event)?;
    }
    Ok(())
}

fn assert_rejected(events: &[&str]) {
    assert!(matches!(
        validate(events),
        Err(LlmError::InvalidResponse(_))
    ));
}

/// Only identity/ordering-only notices in informational namespaces may be
/// ignored; they never count as compacting or alter the output lifecycle.
#[test]
fn unknown_informational_notifications_preserve_compact_state() {
    let mut shape = CompactStreamShape::default();
    let global =
        serde_json::json!({"type":"response.notification.future_notice","sequence_number":1});
    assert!(shape.is_ignored_notification(&global));
    shape.validate(&global).expect("global notification");
    assert_eq!(shape.item, CompactItemPhase::Missing);
    assert_eq!(shape.progress_updates, 0);
    shape
        .validate(
            &serde_json::json!({"type":"response.output_item.added","output_index":0,
        "item":{"type":"compaction","id":"cmp_1"}}),
        )
        .expect("valid added item");
    let local = serde_json::json!({"type":"response.compaction.future_notice",
        "output_index":0,"item_id":"cmp_1","sequence_number":99});
    assert!(shape.is_ignored_notification(&local));
    shape.validate(&local).expect("local notification");
    assert_eq!(shape.item, CompactItemPhase::Added);
    assert_eq!(shape.progress_updates, 0);
    assert!(
        shape
            .validate(&serde_json::json!({"type":"response.completed"}))
            .is_err()
    );
    for event in [
        serde_json::json!({"type":"response.notification.future_notice","payload":"private"}),
        serde_json::json!({"type":"response.compaction.future_notice","output_index":1,"item_id":"cmp_1"}),
        serde_json::json!({"type":"response.compaction.future_notice","output_index":0,"item_id":"other"}),
        serde_json::json!({"type":"response.compaction.future_notice","output_index":0}),
        serde_json::json!({"type":"response.compaction.future_notice","sequence_number":-1}),
        serde_json::json!({"type":"response.compaction.delta"}),
        serde_json::json!({"type":"response.output_text.delta"}),
        serde_json::json!({"type":"response.future_tool.added"}),
        serde_json::json!({"type":"response.output_item.done","output_index":0,"item":{"type":"unknown"}}),
    ] {
        assert!(!shape.is_ignored_notification(&event));
        assert!(shape.validate(&event).is_err());
        assert_eq!(shape.item, CompactItemPhase::Added);
        assert_eq!(shape.progress_updates, 0);
    }
    for kind in ["error", "response.failed", "response.incomplete"] {
        // The real parser, not notification tolerance, still owns these errors.
        let event = serde_json::json!({"type":kind});
        assert!(!shape.is_ignored_notification(&event));
        shape
            .validate(&event)
            .expect("error passes to semantic parser");
    }
    for suffix in [
        "added",
        "done",
        "delta",
        "created",
        "in_progress",
        "started",
        "completed",
        "failed",
        "incomplete",
        "error",
        "canceled",
        "cancelled",
    ] {
        let event = serde_json::json!({"type":format!("response.compaction.{suffix}")});
        assert!(!shape.is_ignored_notification(&event));
        assert!(shape.validate(&event).is_err());
    }
}
