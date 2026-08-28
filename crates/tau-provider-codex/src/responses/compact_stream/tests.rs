use super::*;

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
