use tau_proto::{ContextRole, MessageItem, ToolType};

use super::*;

fn assistant_message(text: impl Into<String>) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: None,
    })
}

/// Ensures assistant previews preserve the provider output order when a
/// response mixes plain text and tool calls. This prevents `/tree` and
/// session-inspection output from hiding tool calls that explain following
/// messages.
#[test]
fn assistant_preview_represents_multiple_messages_and_tool_calls_in_order() {
    let output_items = vec![
        assistant_message("first"),
        ContextItem::ToolCall(ToolCallItem {
            call_id: "call-1".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/main.rs".into()),
            )]),
        }),
        assistant_message("second"),
    ];

    assert_eq!(
        assistant_output_preview(&output_items).as_deref(),
        Some("first tool.call read src/main.rs second")
    );
    assert_eq!(
        format_session_entry(&AgentEntry::AssistantResponse {
            provider_response_id: None,
            backend: None,
            output_items,
            usage: None,
        }),
        "agent: first tool.call read src/main.rs second"
    );
}

/// Ensures all terminal tool results in one model round are represented in
/// inspection output. This protects multi-call rounds from being collapsed to
/// only the first result.
#[test]
fn tool_results_preview_includes_every_result_in_round() {
    let entry = AgentEntry::ToolResults {
        items: vec![
            tau_proto::ToolResultItem {
                call_id: "call-1".into(),
                tool_type: ToolType::Function,
                status: ToolResultStatus::Success,
                output: tau_proto::ToolResponse::from_cbor(&CborValue::Text("ok".into())),
            },
            tau_proto::ToolResultItem {
                call_id: "call-2".into(),
                tool_type: ToolType::Function,
                status: ToolResultStatus::Error {
                    message: "failed".into(),
                },
                output: tau_proto::ToolResponse::from_cbor(&CborValue::Null),
            },
        ],
    };

    assert_eq!(
        format_session_entry(&entry),
        "tool.result call-1 -> ok; tool.error call-2 -> failed"
    );
}

/// Ensures read-only inspection commands do not create state directories merely
/// to report that no sessions or policy approvals exist.
#[test]
fn missing_inspection_roots_are_reported_without_creating_them() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let state_dir = temp_dir.path().join("missing-state");
    let sessions_dir = state_dir.join("sessions");
    let policy_path = state_dir.join("policy.cbor");

    assert_eq!(
        session_list_lines(&sessions_dir).expect("session list"),
        vec!["no sessions"]
    );
    assert_eq!(
        session_lines(&sessions_dir, "default").expect("session lines"),
        vec!["session default not found"]
    );
    assert_eq!(
        policy_lines(&policy_path).expect("policy lines"),
        vec!["no policy approvals"]
    );
    assert!(
        !state_dir.exists(),
        "read-only inspection must not create the state directory"
    );
}

/// Ensures path lookup failures are surfaced as inspection errors instead of
/// being flattened into empty/missing inspection output.
#[test]
fn invalid_inspection_roots_return_errors() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file_parent = temp_dir.path().join("not-a-directory");
    std::fs::write(&file_parent, b"file").expect("write marker file");

    let sessions_dir = file_parent.join("sessions");
    let policy_path = file_parent.join("policy.cbor");

    assert!(session_list_lines(&sessions_dir).is_err());
    assert!(session_lines(&sessions_dir, "default").is_err());
    assert!(policy_lines(&policy_path).is_err());
}
