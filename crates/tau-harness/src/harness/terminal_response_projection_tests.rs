//! Equivalence and clearing oracles for terminal response projection.

use super::*;

/// The one-pass projection must remain byte-for-byte equivalent to the
/// independent legacy assistant-text and tool-call projections.
#[test]
fn one_pass_projection_matches_independent_reference_projections() {
    let output_items = vec![
        ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![
                ContentPart::Text {
                    text: "alpha".to_owned(),
                },
                ContentPart::HarnessInternalText {
                    text: "-beta".to_owned(),
                },
            ],
            phase: None,
            responses_raw_json: None,
        }),
        ContextItem::ToolCall(ToolCallItem {
            call_id: "call-one".into(),
            name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: Some("{}".to_owned()),
            responses_envelope: None,
        }),
    ];

    let projection =
        terminal_response_projection::TerminalResponseProjection::from_output_items(&output_items);
    assert_eq!(
        projection.assistant_text,
        assistant_text_from_output_items(&output_items)
    );
    let reference_calls = tool_calls_from_output_items(&output_items);
    assert_eq!(projection.tool_calls.len(), reference_calls.len());
    assert_eq!(projection.tool_calls[0].id, reference_calls[0].id);
    assert_eq!(projection.tool_calls[0].name, reference_calls[0].name);
    assert!(!projection.contains_compaction);
    assert!(!projection.contains_private_compaction_output);
}

/// The unified pass must preserve compaction privacy classification while
/// concatenating only assistant-role text.
#[test]
fn one_pass_projection_preserves_compaction_and_role_boundaries() {
    let output_items = vec![
        ContextItem::Message(MessageItem {
            role: ContextRole::User,
            content: vec![ContentPart::SyntheticCompactionSummary {
                text: "private-user".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        }),
        ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "visible".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        }),
        ContextItem::Compaction(
            tau_proto::OpaqueProviderItem::from_raw_json(
                r#"{"type":"compaction","encrypted_content":"opaque"}"#,
            )
            .expect("valid compaction sidecar"),
        ),
    ];

    let projection =
        terminal_response_projection::TerminalResponseProjection::from_output_items(&output_items);
    assert_eq!(projection.assistant_text.as_deref(), Some("visible"));
    assert!(projection.contains_compaction);
    assert!(projection.contains_private_compaction_output);
}

/// Malformed repetition normalization clears every derived output fact so
/// removed text, tools, and compaction cannot drive later continuations.
#[test]
fn cleared_repetition_projection_suppresses_all_output_facts() {
    let output_items = vec![
        ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "must disappear".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        }),
        ContextItem::ToolCall(ToolCallItem {
            call_id: "must-not-dispatch".into(),
            name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        }),
        ContextItem::Compaction(
            tau_proto::OpaqueProviderItem::from_raw_json(
                r#"{"type":"compaction","encrypted_content":"opaque"}"#,
            )
            .expect("valid compaction sidecar"),
        ),
    ];

    let mut projection =
        terminal_response_projection::TerminalResponseProjection::from_output_items(&output_items);
    projection.clear_output();
    assert!(projection.tool_calls.is_empty());
    assert!(projection.assistant_text.is_none());
    assert!(!projection.contains_compaction);
    assert!(!projection.contains_private_compaction_output);
}
