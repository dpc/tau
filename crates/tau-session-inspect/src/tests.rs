use std::io::Write as _;

use base64::Engine as _;
use tau_proto::{
    AgentId, ContextRole, Event, MessageItem, SessionAgentLoaded, SessionId, ToolType,
};

use super::*;

fn export_trace(
    agents_dir: &std::path::Path,
    agent_id: &AgentId,
    descendants: DescendantSelection,
    format: AgentTraceFormat,
) -> Result<String, InspectError> {
    let mut prepared = prepare_agent_trace(agents_dir, agent_id, descendants, format)?;
    let mut bytes = Vec::new();
    prepared.copy_to(&mut bytes)?;
    String::from_utf8(bytes).map_err(|error| {
        InspectError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })
}

fn create_trace_agent(
    agents_dir: &std::path::Path,
    agent_id: &str,
    creator: tau_proto::AgentCreator,
    parent_agent: Option<&str>,
    timestamp: u64,
) {
    let agent_id = AgentId::parse(agent_id).expect("agent id");
    let mut store = tau_core::AgentStore::open_lazy(agents_dir).expect("agent store");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentStarted(tau_proto::AgentStarted {
                agent_id: agent_id.clone(),
                creator: Some(creator),
                parent_agent: parent_agent.map(|id| AgentId::parse(id).expect("parent id")),
                role: "trace-test".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
            tau_proto::UnixMicros::new(timestamp),
        )
        .expect("agent creation");
}

fn append_trace_prompt(agents_dir: &std::path::Path, agent_id: &str, text: &str, timestamp: u64) {
    let agent_id = AgentId::parse(agent_id).expect("agent id");
    let mut store = tau_core::AgentStore::open_lazy(agents_dir).expect("agent store");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                agent_id: agent_id.clone(),
                inference_activation: false,
                text: text.to_owned(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                originator: tau_proto::PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
            tau_proto::UnixMicros::new(timestamp + 1),
        )
        .expect("prompt append");
}

fn append_trace_prompt_lifecycle(
    agents_dir: &std::path::Path,
    agent_id: &str,
    prompt_id: &str,
    timestamp: u64,
) {
    let agent_id = AgentId::parse(agent_id).expect("agent id");
    let mut store = tau_core::AgentStore::open_lazy(agents_dir).expect("agent store");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
                agent_id: agent_id.clone(),
                transaction_id: None,
                agent_prompt_id: prompt_id.into(),
                through: tau_proto::AgentHead::Root,
                model: Some("provider/model".into()),
                operation: Some(tau_proto::PromptOperation::Inference),
                activation_cut: Some(tau_proto::AgentHead::Root),
            }),
            tau_proto::UnixMicros::new(timestamp),
        )
        .expect("inference dispatch");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
                agent_prompt_id: prompt_id.into(),
                agent_id: agent_id.clone(),
                session_id: "trace-session".into(),
                model: "provider/model".into(),
                model_params: Some(Default::default()),
                outer_turn_id: None,
                operation: tau_proto::PromptOperation::Inference,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            }),
            tau_proto::UnixMicros::new(timestamp),
        )
        .expect("prompt start");
}

fn append_background_tool_lifecycle(agents_dir: &std::path::Path, agent_id: &str, call_id: &str) {
    append_background_tool_calls(
        agents_dir,
        agent_id,
        &[(call_id, "background_test", "argument")],
    );
}

fn append_background_tool_calls(
    agents_dir: &std::path::Path,
    agent_id: &str,
    calls: &[(&str, &str, &str)],
) {
    let agent_id = AgentId::parse(agent_id).expect("agent id");
    let mut store = tau_core::AgentStore::open_lazy(agents_dir).expect("agent store");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            Event::ProviderResponseFinished(tau_proto::ProviderResponseFinished {
                agent_prompt_id: "prompt-background".into(),
                agent_id: agent_id.clone(),
                output_items: calls
                    .iter()
                    .map(|(call_id, name, argument)| {
                        ContextItem::ToolCall(tau_proto::ToolCallItem {
                            call_id: (*call_id).into(),
                            name: tau_proto::ToolName::new(*name),
                            tool_type: ToolType::Function,
                            arguments: tau_proto::CborValue::Text((*argument).to_owned()),
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
                originator: tau_proto::PromptOriginator::User,
                usage: None,
                estimated_api_cost_increment: None,
                estimated_api_cost_rates: None,
                compaction_original_input_tokens: None,
                compaction_compacted_input_tokens: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            }),
        )
        .expect("provider tool call");
    for (call_id, name, _) in calls {
        let tool_name = tau_proto::ToolName::new(*name);
        store
            .append_agent_event(
                agent_id.as_str(),
                None,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    call_id: (*call_id).into(),
                    tool_name: tool_name.clone(),
                    tool_type: ToolType::Function,
                    result: tau_proto::CborValue::Null,
                    provider_content: Vec::new(),
                    kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            )
            .expect("background placeholder");
        store
            .append_agent_event(
                agent_id.as_str(),
                None,
                Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
                    call_id: (*call_id).into(),
                    tool_name,
                    tool_type: ToolType::Function,
                    result: tau_proto::CborValue::Text(format!("result-{call_id}")),
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            )
            .expect("real background result");
    }
}

/// Builds one durable provider response containing one model-visible tool call.
fn provider_tool_call_event(agent_id: &AgentId, prompt_id: &str, call_id: &str) -> Event {
    Event::ProviderResponseFinished(tau_proto::ProviderResponseFinished {
        agent_prompt_id: prompt_id.into(),
        agent_id: agent_id.clone(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: call_id.into(),
            name: tau_proto::ToolName::new("background_test"),
            tool_type: ToolType::Function,
            arguments: CborValue::Null,
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        estimated_api_cost_increment: None,
        estimated_api_cost_rates: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
}

/// Appends one timestamp-controlled shell call for compact trace regression
/// coverage.
fn append_compact_shell_trace(agents_dir: &std::path::Path, agent_id: &str) {
    let agent_id = AgentId::parse(agent_id).expect("agent id");
    let arguments = CborValue::Map(vec![
        (
            CborValue::Text("command".into()),
            CborValue::Text("printf 'one\\ntwo\\n'".into()),
        ),
        (
            CborValue::Text("workdir".into()),
            CborValue::Text("/work".into()),
        ),
    ]);
    let mut store = tau_core::AgentStore::open_lazy(agents_dir).expect("agent store");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::ProviderResponseFinished(tau_proto::ProviderResponseFinished {
                agent_prompt_id: "prompt-tools".into(),
                agent_id: agent_id.clone(),
                output_items: vec![
                    assistant_message("ignored prose"),
                    ContextItem::ToolCall(ToolCallItem {
                        call_id: "call-shell".into(),
                        name: tau_proto::ToolName::new("shell_command"),
                        tool_type: ToolType::Function,
                        arguments: arguments.clone(),
                        raw_arguments_json: None,
                        responses_envelope: None,
                    }),
                ],
                stop_reason: tau_proto::ProviderStopReason::ToolCalls,
                error: None,
                failure_kind: None,
                context_limit_telemetry: None,
                recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
                originator: tau_proto::PromptOriginator::User,
                usage: None,
                estimated_api_cost_increment: None,
                estimated_api_cost_rates: None,
                compaction_original_input_tokens: None,
                compaction_compacted_input_tokens: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            }),
            tau_proto::UnixMicros::new(11),
        )
        .expect("provider tool call");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::ProviderToolResult(tau_proto::ToolResult {
                call_id: "call-shell".into(),
                tool_name: tau_proto::ToolName::new("shell_command"),
                tool_type: ToolType::Function,
                result: CborValue::Text("one\ntwo\n".into()),
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::Final,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
            tau_proto::UnixMicros::new(21),
        )
        .expect("tool result");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::ProviderResponseFinished(tau_proto::ProviderResponseFinished {
                agent_prompt_id: "prompt-tools-reused".into(),
                agent_id: agent_id.clone(),
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: "call-shell".into(),
                    name: tau_proto::ToolName::new("shell_command"),
                    tool_type: ToolType::Function,
                    arguments,
                    raw_arguments_json: None,
                    responses_envelope: None,
                })],
                stop_reason: tau_proto::ProviderStopReason::ToolCalls,
                error: None,
                failure_kind: None,
                context_limit_telemetry: None,
                recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
                originator: tau_proto::PromptOriginator::User,
                usage: None,
                estimated_api_cost_increment: None,
                estimated_api_cost_rates: None,
                compaction_original_input_tokens: None,
                compaction_compacted_input_tokens: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            }),
            tau_proto::UnixMicros::new(31),
        )
        .expect("reused provider tool call");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::ProviderToolError(tau_proto::ToolError {
                call_id: "call-shell".into(),
                tool_name: tau_proto::ToolName::new("shell_command"),
                tool_type: ToolType::Function,
                message: "second failed".into(),
                details: Some(CborValue::Text("detail".into())),
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
            tau_proto::UnixMicros::new(41),
        )
        .expect("reused tool error");
}

fn assistant_message(text: impl Into<String>) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: None,
        responses_raw_json: None,
    })
}

/// Ensures assistant previews preserve the provider output order when a
/// response mixes plain text and tool calls. This prevents `:tree` and
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

/// Native trace output keeps complete prompt content, emits independently
/// parseable lines, and preserves authoritative per-agent sequence order.
#[test]
fn native_agent_trace_preserves_complete_records_and_order() {
    let temp = tempfile::tempdir().expect("tempdir");
    create_trace_agent(
        temp.path(),
        "agent-root",
        tau_proto::AgentCreator::User,
        None,
        10,
    );
    let content = "full prompt\nreasoning marker; tool args/results; image:data:image/png;base64,AA==; \
                   compaction marker; inter-agent message marker";
    append_trace_prompt(temp.path(), "agent-root", content, 11);

    let output = export_trace(
        temp.path(),
        &AgentId::parse("agent-root").expect("agent id"),
        DescendantSelection::RootOnly,
        AgentTraceFormat::TauJsonl,
    )
    .expect("native export");
    let lines = output
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("independent JSON line"))
        .collect::<Vec<_>>();

    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["schema_version"], 0);
    assert_eq!(lines[1]["seq"], 0);
    assert_eq!(lines[2]["seq"], 1);
    fn contains_exact_text(value: &serde_json::Value, expected: &str) -> bool {
        value.as_str() == Some(expected)
            || value.as_array().is_some_and(|values| {
                values
                    .iter()
                    .any(|value| contains_exact_text(value, expected))
            })
            || value.as_object().is_some_and(|values| {
                values
                    .values()
                    .any(|value| contains_exact_text(value, expected))
            })
    }
    assert!(contains_exact_text(&lines[2]["event"], content));
}

/// Descendant discovery follows immutable creator provenance recursively and
/// excludes inheritance-only relations and unrelated durable agents.
#[test]
fn agent_trace_descendants_use_creator_not_parent_agent() {
    let temp = tempfile::tempdir().expect("tempdir");
    create_trace_agent(
        temp.path(),
        "agent-root",
        tau_proto::AgentCreator::User,
        None,
        10,
    );
    create_trace_agent(
        temp.path(),
        "agent-child",
        tau_proto::AgentCreator::Agent {
            session_id: "session-child".into(),
            agent_id: AgentId::parse("agent-root").expect("agent id"),
        },
        None,
        10,
    );
    create_trace_agent(
        temp.path(),
        "agent-grandchild",
        tau_proto::AgentCreator::Agent {
            session_id: "session-grandchild".into(),
            agent_id: AgentId::parse("agent-child").expect("agent id"),
        },
        None,
        9,
    );
    create_trace_agent(
        temp.path(),
        "agent-parent-only",
        tau_proto::AgentCreator::User,
        Some("agent-root"),
        8,
    );
    create_trace_agent(
        temp.path(),
        "agent-unrelated",
        tau_proto::AgentCreator::User,
        None,
        7,
    );

    let output = export_trace(
        temp.path(),
        &AgentId::parse("agent-root").expect("agent id"),
        DescendantSelection::Include,
        AgentTraceFormat::TauJsonl,
    )
    .expect("workflow export");
    let header: serde_json::Value =
        serde_json::from_str(output.lines().next().expect("header")).expect("header JSON");

    assert_eq!(
        header["included_agent_ids"],
        serde_json::json!(["agent-child", "agent-grandchild", "agent-root"])
    );
}

/// An unrelated legacy journal without `seq` cannot abort authenticated
/// descendant traversal rooted at a healthy agent.
#[test]
fn agent_trace_descendants_ignore_unrelated_legacy_creation_record() {
    let temp = tempfile::tempdir().expect("tempdir");
    create_trace_agent(
        temp.path(),
        "agent-root",
        tau_proto::AgentCreator::User,
        None,
        10,
    );
    create_trace_agent(
        temp.path(),
        "agent-child",
        tau_proto::AgentCreator::Agent {
            session_id: "session-child".into(),
            agent_id: AgentId::parse("agent-root").expect("agent id"),
        },
        None,
        11,
    );
    let legacy_dir = temp.path().join("agent-unrelated-legacy");
    std::fs::create_dir(&legacy_dir).expect("legacy agent directory");
    let mut legacy = std::fs::File::create(legacy_dir.join("events.cbor")).expect("legacy journal");
    legacy
        .write_all(&1_u64.to_le_bytes())
        .and_then(|()| legacy.write_all(&[0xa0]))
        .expect("legacy record missing seq");

    let output = export_trace(
        temp.path(),
        &AgentId::parse("agent-root").expect("agent id"),
        DescendantSelection::Include,
        AgentTraceFormat::TauJsonl,
    )
    .expect("unrelated legacy journal is outside the rooted workflow");
    let header: serde_json::Value =
        serde_json::from_str(output.lines().next().expect("header")).expect("header JSON");

    assert_eq!(
        header["included_agent_ids"],
        serde_json::json!(["agent-child", "agent-root"])
    );
}

/// A descendant with an authenticated creation edge remains in scope and must
/// fail locally when strict full-journal validation finds corruption.
#[test]
fn agent_trace_descendants_reject_reachable_corrupt_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    create_trace_agent(
        temp.path(),
        "agent-root",
        tau_proto::AgentCreator::User,
        None,
        10,
    );
    create_trace_agent(
        temp.path(),
        "agent-child",
        tau_proto::AgentCreator::Agent {
            session_id: "session-child".into(),
            agent_id: AgentId::parse("agent-root").expect("agent id"),
        },
        None,
        11,
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(temp.path().join("agent-child/events.cbor"))
        .expect("reachable journal")
        .write_all(&[1, 2, 3])
        .expect("corrupt reachable suffix");

    let result = prepare_agent_trace(
        temp.path(),
        &AgentId::parse("agent-root").expect("agent id"),
        DescendantSelection::Include,
        AgentTraceFormat::TauJsonl,
    );

    assert!(matches!(
        result,
        Err(InspectError::AgentStore(
            tau_core::AgentStoreError::Read { .. }
        ))
    ));
}

/// OTLP trace output is one request object, retains every durable occurrence as
/// a span event, and never invents cross-agent timestamp ordering.
#[test]
fn otlp_agent_trace_is_one_lossy_request_with_raw_events() {
    let temp = tempfile::tempdir().expect("tempdir");
    create_trace_agent(
        temp.path(),
        "agent-root",
        tau_proto::AgentCreator::User,
        None,
        20,
    );
    append_trace_prompt(
        temp.path(),
        "agent-root",
        "later sequence, earlier clock",
        10,
    );

    let output = export_trace(
        temp.path(),
        &AgentId::parse("agent-root").expect("agent id"),
        DescendantSelection::RootOnly,
        AgentTraceFormat::OtlpJson,
    )
    .expect("OTLP export");
    let _: opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest =
        serde_json::from_str(&output).expect("standard OTLP protobuf JSON");
    let request: serde_json::Value = serde_json::from_str(&output).expect("one OTLP request");
    let spans = request["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .expect("spans");
    let root = &spans[0];

    assert_eq!(root["startTimeUnixNano"], root["endTimeUnixNano"]);
    assert_eq!(root["events"].as_array().expect("raw events").len(), 2);
    assert!(
        root["attributes"]
            .as_array()
            .expect("attributes")
            .iter()
            .any(|attribute| attribute["key"] == "tau.incomplete"
                && attribute["value"]["boolValue"] == true)
    );
}

/// A store-backed valid prompt lifecycle exposes only durable lifecycle
/// metadata as OTLP input; non-persisted materialized prompts are never
/// claimed.
#[test]
fn otlp_prompt_input_is_durable_lifecycle_metadata() {
    let temp = tempfile::tempdir().expect("tempdir");
    create_trace_agent(
        temp.path(),
        "agent-root",
        tau_proto::AgentCreator::User,
        None,
        1,
    );
    append_trace_prompt_lifecycle(temp.path(), "agent-root", "prompt-1", 2);

    let output = export_trace(
        temp.path(),
        &AgentId::parse("agent-root").expect("agent id"),
        DescendantSelection::RootOnly,
        AgentTraceFormat::OtlpJson,
    )
    .expect("OTLP export");
    let request: serde_json::Value = serde_json::from_str(&output).expect("OTLP request");
    let llm = request["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .expect("spans")
        .iter()
        .find(|span| {
            span["attributes"].as_array().is_some_and(|attributes| {
                attributes.iter().any(|attribute| {
                    attribute["key"] == "openinference.span.kind"
                        && attribute["value"]["stringValue"] == "LLM"
                })
            })
        })
        .expect("LLM span");
    let input_scope = llm["attributes"]
        .as_array()
        .expect("attributes")
        .iter()
        .find(|attribute| attribute["key"] == "tau.input.scope")
        .expect("input scope");

    assert_eq!(
        input_scope["value"]["stringValue"],
        "lifecycle_metadata_only"
    );
}

/// A valid store-backed background lifecycle serializes exactly one TOOL span
/// whose input is the original start and whose output is the real completion.
#[test]
fn otlp_background_tool_is_one_span_with_real_terminal() {
    let temp = tempfile::tempdir().expect("tempdir");
    create_trace_agent(
        temp.path(),
        "agent-root",
        tau_proto::AgentCreator::User,
        None,
        1,
    );
    append_background_tool_lifecycle(temp.path(), "agent-root", "call-background");

    let output = export_trace(
        temp.path(),
        &AgentId::parse("agent-root").expect("agent id"),
        DescendantSelection::RootOnly,
        AgentTraceFormat::OtlpJson,
    )
    .expect("OTLP export");
    let request: serde_json::Value = serde_json::from_str(&output).expect("OTLP request");
    let tool_spans = request["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .expect("spans")
        .iter()
        .filter(|span| {
            span["attributes"].as_array().is_some_and(|attributes| {
                attributes.iter().any(|attribute| {
                    attribute["key"] == "openinference.span.kind"
                        && attribute["value"]["stringValue"] == "TOOL"
                })
            })
        })
        .collect::<Vec<_>>();

    assert_eq!(tool_spans.len(), 1);
    let attributes = tool_spans[0]["attributes"].as_array().expect("attributes");
    let attribute = |key| {
        attributes
            .iter()
            .find(|attribute| attribute["key"] == key)
            .and_then(|attribute| attribute["value"]["stringValue"].as_str())
            .expect("string attribute")
    };
    assert!(
        attribute("input.value").contains("\"record_type\":\"provider_tool_call\""),
        "{}",
        attribute("input.value")
    );
    assert!(attribute("output.value").contains("tool.background_result"));
    assert!(attribute("output.value").contains("result-call-background"));
}

/// Multiple provider-declared calls retain only their own compact input and
/// OpenInference tool attributes, avoiding response-wide quadratic expansion.
#[test]
fn otlp_multi_tool_response_projects_each_call_once() {
    let temp = tempfile::tempdir().expect("tempdir");
    create_trace_agent(
        temp.path(),
        "agent-root",
        tau_proto::AgentCreator::User,
        None,
        1,
    );
    let calls = [
        ("call-a", "tool_a", "argument-a"),
        ("call-b", "tool_b", "argument-b"),
        ("call-c", "tool_c", "argument-c"),
    ];
    append_background_tool_calls(temp.path(), "agent-root", &calls);
    let output = export_trace(
        temp.path(),
        &AgentId::parse("agent-root").expect("agent id"),
        DescendantSelection::RootOnly,
        AgentTraceFormat::OtlpJson,
    )
    .expect("OTLP export");
    let request: serde_json::Value = serde_json::from_str(&output).expect("OTLP request");
    let tool_spans = request["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .expect("spans")
        .iter()
        .filter(|span| {
            span["attributes"].as_array().is_some_and(|attributes| {
                attributes.iter().any(|attribute| {
                    attribute["key"] == "openinference.span.kind"
                        && attribute["value"]["stringValue"] == "TOOL"
                })
            })
        })
        .collect::<Vec<_>>();

    assert_eq!(tool_spans.len(), calls.len());
    for (call_id, name, argument) in calls {
        let span = tool_spans
            .iter()
            .find(|span| {
                span["attributes"].as_array().is_some_and(|attributes| {
                    attributes.iter().any(|attribute| {
                        attribute["key"] == "tau.tool.call_id"
                            && attribute["value"]["stringValue"] == call_id
                    })
                })
            })
            .expect("call span");
        let attributes = span["attributes"].as_array().expect("attributes");
        let attribute = |key| {
            attributes
                .iter()
                .find(|attribute| attribute["key"] == key)
                .and_then(|attribute| attribute["value"]["stringValue"].as_str())
                .expect("string attribute")
        };
        assert_eq!(attribute("tool.name"), name);
        assert!(attribute("tool.parameters").contains(argument));
        assert!(attribute("input.value").contains(call_id));
        for (other_id, _, _) in calls {
            if other_id != call_id {
                assert!(!attribute("input.value").contains(other_id));
            }
        }
        assert!(attribute("output.value").contains(&format!("result-{call_id}")));
    }
}

/// The lite agent-tools format keeps only model-visible calls, exposes shell
/// commands directly, uses trace-relative timing, and replaces output with
/// exact byte and logical-line counts.
#[test]
fn agent_tools_lite_is_compact_relative_and_output_free() {
    let temp = tempfile::tempdir().expect("tempdir");
    create_trace_agent(
        temp.path(),
        "agent-root",
        tau_proto::AgentCreator::User,
        None,
        1,
    );
    append_compact_shell_trace(temp.path(), "agent-root");

    let output = export_trace(
        temp.path(),
        &AgentId::parse("agent-root").expect("agent id"),
        DescendantSelection::RootOnly,
        AgentTraceFormat::AgentToolsJsonl(AgentTraceMode::Lite),
    )
    .expect("compact trace");
    let lines = output
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON line"))
        .collect::<Vec<_>>();

    assert_eq!(lines.len(), 3, "header plus two reused model-visible calls");
    assert_eq!(lines[0]["schema"], "tau.agent_tools");
    assert_eq!(lines[0]["output"], "counts");
    assert_eq!(lines[1]["at_us"], 10);
    assert_eq!(lines[1]["duration_us"], 10);
    assert_eq!(lines[1]["tool"], "shell_command");
    assert_eq!(lines[1]["command"], "printf 'one\\ntwo\\n'");
    assert_eq!(
        lines[1]["arguments"],
        serde_json::json!({
            "command": "printf 'one\\ntwo\\n'",
            "workdir": "/work",
        })
    );
    assert_eq!(lines[1]["status"], "ok");
    assert_eq!(lines[1]["output_bytes"], 8);
    assert_eq!(lines[1]["output_lines"], 2);
    assert!(lines[1].get("output").is_none());
    assert!(!output.contains("ignored prose"));
    assert_eq!(lines[2]["at_us"], 30);
    assert_eq!(lines[2]["status"], "error");
    assert_eq!(lines[2]["output_bytes"], 28);
    assert_eq!(lines[2]["output_lines"], 3);
}

/// The full agent-tools format retains the same flat call shape while replacing
/// lite counters with the complete provider-facing tool output.
#[test]
fn agent_tools_full_includes_rendered_output() {
    let temp = tempfile::tempdir().expect("tempdir");
    create_trace_agent(
        temp.path(),
        "agent-root",
        tau_proto::AgentCreator::User,
        None,
        1,
    );
    append_compact_shell_trace(temp.path(), "agent-root");

    let output = export_trace(
        temp.path(),
        &AgentId::parse("agent-root").expect("agent id"),
        DescendantSelection::RootOnly,
        AgentTraceFormat::AgentToolsJsonl(AgentTraceMode::Full),
    )
    .expect("compact full trace");
    let lines = output
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON line"))
        .collect::<Vec<_>>();

    assert_eq!(lines[0]["output"], "full");
    assert_eq!(lines[1]["output"], "one\ntwo\n");
    assert!(lines[1].get("output_bytes").is_none());
    assert!(lines[1].get("output_lines").is_none());
    assert_eq!(lines[2]["output"], "error: second failed\n\ndetail");

    let toon = export_trace(
        temp.path(),
        &AgentId::parse("agent-root").expect("agent id"),
        DescendantSelection::RootOnly,
        AgentTraceFormat::AgentToolsToon(AgentTraceMode::Full),
    )
    .expect("compact full TOON");
    assert!(toon.contains(r#"output: "one\ntwo\n""#));
    let decoded: serde_json::Value = serde_toon::from_str(&toon).expect("strict TOON");
    assert_eq!(decoded["calls"][0]["output"], "one\ntwo\n");
}

/// TOON uses one counted calls array, escapes multiline full output inside one
/// scalar, and round-trips complete tagged-CBOR arguments that ordinary JSON
/// cannot represent faithfully.
#[test]
fn agent_tools_toon_frames_multiline_and_lossless_arguments() {
    let temp = tempfile::tempdir().expect("tempdir");
    create_trace_agent(
        temp.path(),
        "agent-root",
        tau_proto::AgentCreator::User,
        None,
        1,
    );
    let agent_id = AgentId::parse("agent-root").expect("agent id");
    let mut call = provider_tool_call_event(&agent_id, "prompt-toon", "call-toon");
    let Event::ProviderResponseFinished(finished) = &mut call else {
        unreachable!("helper returns provider response")
    };
    let ContextItem::ToolCall(call_item) = &mut finished.output_items[0] else {
        unreachable!("helper returns tool call")
    };
    call_item.arguments = CborValue::Map(vec![(
        CborValue::Bytes(vec![1, 2]),
        CborValue::Tag(42, Box::new(CborValue::Float(f64::NAN))),
    )]);
    let mut store = tau_core::AgentStore::open_lazy(temp.path()).expect("agent store");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            call,
            tau_proto::UnixMicros::new(2),
        )
        .expect("tool call");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::ProviderToolResult(tau_proto::ToolResult {
                call_id: "call-toon".into(),
                tool_name: tau_proto::ToolName::new("background_test"),
                tool_type: ToolType::Function,
                result: CborValue::Text("first\nsecond\n".into()),
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::Final,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
            tau_proto::UnixMicros::new(3),
        )
        .expect("tool result");
    drop(store);

    let full = export_trace(
        temp.path(),
        &agent_id,
        DescendantSelection::RootOnly,
        AgentTraceFormat::AgentToolsToon(AgentTraceMode::Full),
    )
    .expect("full TOON");
    assert!(full.contains("calls[1]:"));
    assert!(full.contains("arguments_json_base64:"));
    let decoded: serde_json::Value = serde_toon::from_str(&full).expect("strict TOON round trip");
    let call = &decoded["calls"][0];
    let arguments: serde_json::Value = serde_json::from_slice(
        &base64::engine::general_purpose::STANDARD
            .decode(
                call["arguments_json_base64"]
                    .as_str()
                    .expect("lossless arguments JSON base64 scalar"),
            )
            .expect("base64 arguments JSON"),
    )
    .expect("lossless arguments JSON");
    let jsonl = export_trace(
        temp.path(),
        &agent_id,
        DescendantSelection::RootOnly,
        AgentTraceFormat::AgentToolsJsonl(AgentTraceMode::Full),
    )
    .expect("full JSONL");
    let jsonl_call: serde_json::Value =
        serde_json::from_str(jsonl.lines().nth(1).expect("call line")).expect("JSONL call");
    assert_eq!(call["call_id"], jsonl_call["call_id"]);
    assert_eq!(call["status"], jsonl_call["status"]);
    assert_eq!(call["output"], "first\nsecond\n");
    assert_eq!(arguments["type"], "map");
    assert_eq!(arguments["value"][0]["key"]["type"], "bytes");
    assert_eq!(arguments["value"][0]["value"]["type"], "tag");
    assert_eq!(
        arguments,
        serde_json::json!({
            "type": "map",
            "value": [{
                "key": {"type": "bytes", "encoding": "base64", "value": "AQI="},
                "value": {
                    "type": "tag",
                    "tag": "42",
                    "value": {"type": "float64_bits", "value": "7ff8000000000000"},
                },
            }],
        })
    );

    let lite = export_trace(
        temp.path(),
        &agent_id,
        DescendantSelection::RootOnly,
        AgentTraceFormat::AgentToolsToon(AgentTraceMode::Lite),
    )
    .expect("lite TOON");
    let decoded: serde_json::Value = serde_toon::from_str(&lite).expect("strict TOON round trip");
    assert_eq!(decoded["output"], "counts");
    let call = &decoded["calls"][0];
    let arguments: serde_json::Value = serde_json::from_slice(
        &base64::engine::general_purpose::STANDARD
            .decode(
                call["arguments_json_base64"]
                    .as_str()
                    .expect("lossless arguments JSON base64 scalar"),
            )
            .expect("base64 arguments JSON"),
    )
    .expect("lossless arguments JSON");
    assert_eq!(arguments, jsonl_call["arguments"]);
    assert_eq!(call["output_bytes"], 13);
    assert_eq!(call["output_lines"], 2);
    assert!(call.get("output").is_none());
}

/// A background placeholder keeps the model-visible call open until the real
/// background result, so the compact trace exposes one call with the real
/// output.
#[test]
fn agent_tools_background_placeholder_waits_for_real_result() {
    let temp = tempfile::tempdir().expect("tempdir");
    create_trace_agent(
        temp.path(),
        "agent-root",
        tau_proto::AgentCreator::User,
        None,
        1,
    );
    append_background_tool_lifecycle(temp.path(), "agent-root", "call-background");

    let output = export_trace(
        temp.path(),
        &AgentId::parse("agent-root").expect("agent id"),
        DescendantSelection::RootOnly,
        AgentTraceFormat::AgentToolsJsonl(AgentTraceMode::Full),
    )
    .expect("compact full trace");
    let calls = output
        .lines()
        .skip(1)
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON line"))
        .collect::<Vec<_>>();

    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["status"], "ok");
    assert_eq!(calls[0]["output"], "result-call-background");
}

/// Two unresolved background generations cannot be correlated by call ID alone,
/// so compact projection fails rather than assigning a terminal to the wrong
/// model-visible occurrence.
#[test]
fn agent_tools_rejects_ambiguous_concurrent_background_id_reuse() {
    let temp = tempfile::tempdir().expect("tempdir");
    create_trace_agent(
        temp.path(),
        "agent-root",
        tau_proto::AgentCreator::User,
        None,
        1,
    );
    let agent_id = AgentId::parse("agent-root").expect("agent id");
    let mut store = tau_core::AgentStore::open_lazy(temp.path()).expect("agent store");
    for (prompt_id, timestamp) in [("prompt-first", 2), ("prompt-second", 4)] {
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                tau_core::AgentEventParent::InheritHead,
                provider_tool_call_event(&agent_id, prompt_id, "call-reused"),
                tau_proto::UnixMicros::new(timestamp),
            )
            .expect("provider tool call");
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                tau_core::AgentEventParent::InheritHead,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    call_id: "call-reused".into(),
                    tool_name: tau_proto::ToolName::new("background_test"),
                    tool_type: ToolType::Function,
                    result: CborValue::Null,
                    provider_content: Vec::new(),
                    kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
                tau_proto::UnixMicros::new(timestamp + 1),
            )
            .expect("background placeholder");
    }
    drop(store);

    let error = export_trace(
        temp.path(),
        &agent_id,
        DescendantSelection::RootOnly,
        AgentTraceFormat::AgentToolsJsonl(AgentTraceMode::Lite),
    )
    .expect_err("ambiguous generations must fail");

    assert!(
        error
            .to_string()
            .contains("ambiguous concurrent background tool call ID `call-reused`")
    );
}

/// A reused foreground ID remains distinct from its unresolved background
/// predecessor regardless of which typed terminal arrives first.
#[test]
fn agent_tools_routes_reused_foreground_and_background_terminals() {
    for foreground_first in [false, true] {
        let temp = tempfile::tempdir().expect("tempdir");
        create_trace_agent(
            temp.path(),
            "agent-root",
            tau_proto::AgentCreator::User,
            None,
            1,
        );
        let agent_id = AgentId::parse("agent-root").expect("agent id");
        let mut store = tau_core::AgentStore::open_lazy(temp.path()).expect("agent store");
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                tau_core::AgentEventParent::InheritHead,
                provider_tool_call_event(&agent_id, "prompt-background", "call-reused"),
                tau_proto::UnixMicros::new(2),
            )
            .expect("first call");
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                tau_core::AgentEventParent::InheritHead,
                Event::ProviderToolResult(tau_proto::ToolResult {
                    call_id: "call-reused".into(),
                    tool_name: tau_proto::ToolName::new("background_test"),
                    tool_type: ToolType::Function,
                    result: CborValue::Null,
                    provider_content: Vec::new(),
                    kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
                tau_proto::UnixMicros::new(3),
            )
            .expect("placeholder");
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                tau_core::AgentEventParent::InheritHead,
                provider_tool_call_event(&agent_id, "prompt-foreground", "call-reused"),
                tau_proto::UnixMicros::new(4),
            )
            .expect("reused foreground");

        let foreground = Event::ProviderToolError(tau_proto::ToolError {
            call_id: "call-reused".into(),
            tool_name: tau_proto::ToolName::new("background_test"),
            tool_type: ToolType::Function,
            message: "foreground failed".into(),
            details: None,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        });
        let background = Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
            call_id: "call-reused".into(),
            tool_name: tau_proto::ToolName::new("background_test"),
            tool_type: ToolType::Function,
            result: CborValue::Text("background done".into()),
            display: None,
            originator: tau_proto::PromptOriginator::User,
        });
        let terminals = if foreground_first {
            [foreground, background]
        } else {
            [background, foreground]
        };
        for (offset, terminal) in terminals.into_iter().enumerate() {
            store
                .append_agent_event_at(
                    agent_id.as_str(),
                    None,
                    tau_core::AgentEventParent::InheritHead,
                    terminal,
                    tau_proto::UnixMicros::new(5 + offset as u64),
                )
                .expect("typed terminal");
        }
        drop(store);

        let output = export_trace(
            temp.path(),
            &agent_id,
            DescendantSelection::RootOnly,
            AgentTraceFormat::AgentToolsJsonl(AgentTraceMode::Full),
        )
        .expect("compact trace");
        let calls = output
            .lines()
            .skip(1)
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON line"))
            .collect::<Vec<_>>();

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["status"], "ok");
        assert_eq!(calls[0]["output"], "background done");
        assert_eq!(
            calls[0]["duration_us"],
            if foreground_first { 4 } else { 3 }
        );
        assert_eq!(calls[1]["status"], "error");
        assert_eq!(calls[1]["output"], "error: foreground failed\n\n");
        assert_eq!(
            calls[1]["duration_us"],
            if foreground_first { 1 } else { 2 }
        );
    }
}

/// One combined stable-schema fixture protects provider-item tie ordering,
/// cancellation, incomplete calls, and decreasing terminal clocks in both
/// modes.
#[test]
fn agent_tools_stable_records_cover_non_success_branches() {
    let temp = tempfile::tempdir().expect("tempdir");
    create_trace_agent(
        temp.path(),
        "agent-root",
        tau_proto::AgentCreator::User,
        None,
        1,
    );
    let agent_id = AgentId::parse("agent-root").expect("agent id");
    let mut tied = provider_tool_call_event(&agent_id, "prompt-tied", "call-cancelled");
    let Event::ProviderResponseFinished(finished) = &mut tied else {
        unreachable!("helper returns provider response")
    };
    finished
        .output_items
        .push(ContextItem::ToolCall(ToolCallItem {
            call_id: "call-incomplete".into(),
            name: tau_proto::ToolName::new("incomplete_tool"),
            tool_type: ToolType::Function,
            arguments: CborValue::Text("pending".into()),
            raw_arguments_json: None,
            responses_envelope: None,
        }));
    let mut store = tau_core::AgentStore::open_lazy(temp.path()).expect("agent store");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            tied,
            tau_proto::UnixMicros::new(10),
        )
        .expect("tied calls");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::ToolCancelled(tau_proto::ToolCancelled {
                call_id: "call-cancelled".into(),
                tool_name: tau_proto::ToolName::new("background_test"),
                tool_type: ToolType::Function,
            }),
            tau_proto::UnixMicros::new(11),
        )
        .expect("cancelled");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::ProviderToolResult(tau_proto::ToolResult {
                call_id: "call-incomplete".into(),
                tool_name: tau_proto::ToolName::new("incomplete_tool"),
                tool_type: ToolType::Function,
                result: CborValue::Null,
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
            tau_proto::UnixMicros::new(12),
        )
        .expect("incomplete background placeholder");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            provider_tool_call_event(&agent_id, "prompt-decreasing", "call-decreasing"),
            tau_proto::UnixMicros::new(20),
        )
        .expect("decreasing call");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::ProviderToolResult(tau_proto::ToolResult {
                call_id: "call-decreasing".into(),
                tool_name: tau_proto::ToolName::new("background_test"),
                tool_type: ToolType::Function,
                result: CborValue::Text("done".into()),
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::Final,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
            tau_proto::UnixMicros::new(19),
        )
        .expect("decreasing terminal");
    drop(store);

    let export = |mode| {
        export_trace(
            temp.path(),
            &agent_id,
            DescendantSelection::RootOnly,
            AgentTraceFormat::AgentToolsJsonl(mode),
        )
        .expect("compact trace")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON line"))
        .collect::<Vec<_>>()
    };
    let lite = export(AgentTraceMode::Lite);
    let full = export(AgentTraceMode::Full);

    let header = |output| {
        serde_json::json!({
            "schema": "tau.agent_tools",
            "schema_version": 0,
            "record_type": "header",
            "root_agent_id": "agent-root",
            "included_agent_ids": ["agent-root"],
            "output": output,
            "time_unit": "microseconds",
        })
    };
    assert_eq!(lite[0], header("counts"));
    assert_eq!(full[0], header("full"));
    assert_eq!(
        lite[1],
        serde_json::json!({
            "record_type": "call", "at_us": 9, "agent_id": "agent-root",
            "call_id": "call-cancelled", "tool": "background_test",
            "arguments": null, "status": "cancelled", "duration_us": 1,
            "output_bytes": 22, "output_lines": 2,
        })
    );
    assert_eq!(
        full[1],
        serde_json::json!({
            "record_type": "call", "at_us": 9, "agent_id": "agent-root",
            "call_id": "call-cancelled", "tool": "background_test",
            "arguments": null, "status": "cancelled", "duration_us": 1,
            "output": "cancelled: cancelled\n\n",
        })
    );
    assert_eq!(lite[2]["call_id"], "call-incomplete");
    assert_eq!(lite[2]["status"], "incomplete");
    assert_eq!(lite[2]["output_bytes"], 0);
    assert_eq!(lite[2]["output_lines"], 0);
    assert_eq!(full[2]["call_id"], "call-incomplete");
    assert_eq!(full[2]["status"], "incomplete");
    assert!(full[2].get("duration_us").is_none());
    assert!(full[2].get("output").is_none());
    assert_eq!(lite[3]["call_id"], "call-decreasing");
    assert!(lite[3].get("duration_us").is_none());
    assert_eq!(full[3]["output"], "done");
    assert!(full[3].get("duration_us").is_none());
}

/// Trace preparation rejects missing, active, and corrupt durable journals
/// before it yields any artifact that the CLI could copy to stdout.
#[test]
fn agent_trace_failures_produce_no_prepared_output() {
    let temp = tempfile::tempdir().expect("tempdir");
    let missing = prepare_agent_trace(
        temp.path(),
        &AgentId::parse("agent-missing").expect("agent id"),
        DescendantSelection::RootOnly,
        AgentTraceFormat::TauJsonl,
    );
    assert!(missing.is_err());

    let active_id = AgentId::parse("agent-active").expect("agent id");
    let mut active_store = tau_core::AgentStore::open_lazy(temp.path()).expect("agent store");
    active_store
        .append_agent_event(
            active_id.as_str(),
            None,
            Event::AgentStarted(tau_proto::AgentStarted {
                agent_id: active_id.clone(),
                creator: Some(tau_proto::AgentCreator::User),
                parent_agent: None,
                role: "test".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("active creation");
    assert!(
        prepare_agent_trace(
            temp.path(),
            &active_id,
            DescendantSelection::RootOnly,
            AgentTraceFormat::TauJsonl,
        )
        .is_err()
    );
    drop(active_store);

    let path = temp.path().join(active_id.as_str()).join("events.cbor");
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("journal")
        .write_all(&[1, 2, 3])
        .expect("torn frame");
    assert!(
        prepare_agent_trace(
            temp.path(),
            &active_id,
            DescendantSelection::RootOnly,
            AgentTraceFormat::TauJsonl,
        )
        .is_err()
    );
}

/// Ensures one corrupt journal cannot prevent `session list` from reporting
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
                    agent_initialization_id: tau_proto::AgentInitializationId::new("test-init"),

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
