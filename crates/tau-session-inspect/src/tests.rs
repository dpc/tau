use tau_proto::{
    AgentId, ContextRole, Event, MessageItem, SessionAgentLoaded, SessionId, ToolType,
};

use super::*;

fn assistant_message(text: impl Into<String>) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: None,
        responses_raw_json: None,
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
            raw_arguments_json: None,
            responses_envelope: None,
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
                provider_content: Vec::new(),
            },
            tau_proto::ToolResultItem {
                call_id: "call-2".into(),
                tool_type: ToolType::Function,
                status: ToolResultStatus::Error {
                    message: "failed".into(),
                },
                output: tau_proto::ToolResponse::from_cbor(&CborValue::Null),
                provider_content: Vec::new(),
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

/// Ensures one corrupt journal cannot prevent `session-list` from reporting
/// healthy sessions, while preserving a visible typed diagnostic for the
/// corrupt session instead of folding or silently skipping it.
#[test]
fn session_list_isolates_invalid_session_journals() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let sessions_dir = temp_dir.path().join("sessions");
    let mut store = SessionStore::open(&sessions_dir).expect("session store");
    for (session_id, agent_id) in [("healthy", "agent-good"), ("invalid", "agent-bad")] {
        store
            .append_session_event(
                session_id,
                None,
                Event::SessionAgentLoaded(SessionAgentLoaded {
                    session_id: SessionId::from(session_id),
                    agent_id: AgentId::parse(agent_id).expect("agent id"),
                    ephemeral: false,
                }),
            )
            .expect("membership append");
    }
    drop(store);

    let invalid_path = sessions_dir.join("invalid").join("events.cbor");
    let mut bytes = std::fs::read(&invalid_path).expect("read invalid journal");
    let seq_value = bytes
        .windows(5)
        .position(|window| window == b"\x63seq\x00")
        .map(|offset| offset + 4)
        .expect("encoded sequence field");
    bytes[seq_value] = 5;
    std::fs::write(&invalid_path, bytes).expect("write invalid journal");

    let lines = session_list_lines(&sessions_dir).expect("session list");
    assert_eq!(lines[0], "healthy (1 loaded agent(s))");
    assert!(
        lines[1].starts_with("invalid (invalid session state: invalid session event sequence in "),
        "corrupt session must retain its typed diagnostic: {lines:?}"
    );
    assert!(
        lines[1].ends_with("events.cbor: expected 0, got 5)"),
        "diagnostic must identify the nonzero initial sequence: {lines:?}"
    );
}
