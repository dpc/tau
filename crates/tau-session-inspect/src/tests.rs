use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::{collections as path_std_collections, fs as path_std_fs, io as path_std_io};

use opentelemetry_proto::tonic::collector::trace as path_opentelemetry_proto_tonic_collector_trace;
use tau_proto::{
    AgentId, ContextRole, Event, MessageItem, SessionAgentLoaded, SessionId, ToolType,
};

use super::*;

/// Restores fixture permissions if a recursively read-only trace test exits
/// early.
#[cfg(unix)]
struct ReadOnlyTreeRestore {
    /// Original Unix modes for every fixture path.
    permissions: Vec<(PathBuf, u32)>,
}

#[cfg(unix)]
impl ReadOnlyTreeRestore {
    /// Removes write access from every existing fixture path.
    fn make(root: &std::path::Path) -> path_std_io::Result<Self> {
        let mut paths = Vec::new();
        collect_paths(root, &mut paths)?;
        let mut permissions = Vec::with_capacity(paths.len());
        for path in &paths {
            let mode = path_std_fs::metadata(path)?.permissions().mode();
            permissions.push((path.clone(), mode));
        }
        let restore = Self { permissions };
        for (path, mode) in &restore.permissions {
            path_std_fs::set_permissions(path, path_std_fs::Permissions::from_mode(mode & !0o222))?;
        }
        Ok(restore)
    }

    /// Restores the fixture's original modes before its temporary directory
    /// drops.
    fn restore(&mut self) -> path_std_io::Result<()> {
        while let Some((path, mode)) = self.permissions.last() {
            path_std_fs::set_permissions(path, path_std_fs::Permissions::from_mode(*mode))?;
            self.permissions.pop();
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for ReadOnlyTreeRestore {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Collects `root` and every descendant before a test removes directory write
/// access.
#[cfg(unix)]
fn collect_paths(root: &std::path::Path, paths: &mut Vec<PathBuf>) -> path_std_io::Result<()> {
    paths.push(root.to_path_buf());
    if root.is_dir() {
        for entry in path_std_fs::read_dir(root)? {
            collect_paths(&entry?.path(), paths)?;
        }
    }
    Ok(())
}

/// Recursively captures fixture paths and file bodies so read-only inspection
/// tests reject both visible output failures and accidental source mutations.
fn source_tree_snapshot(
    root: &std::path::Path,
) -> path_std_io::Result<path_std_collections::BTreeMap<PathBuf, Option<Vec<u8>>>> {
    let mut snapshot = path_std_collections::BTreeMap::new();
    source_tree_snapshot_into(root, PathBuf::new(), &mut snapshot)?;
    Ok(snapshot)
}

/// Adds one source-tree entry and its descendants to a deterministic snapshot.
fn source_tree_snapshot_into(
    path: &std::path::Path,
    relative_path: PathBuf,
    snapshot: &mut path_std_collections::BTreeMap<PathBuf, Option<Vec<u8>>>,
) -> path_std_io::Result<()> {
    if path.is_dir() {
        snapshot.insert(relative_path.clone(), None);
        for entry in path_std_fs::read_dir(path)? {
            let entry = entry?;
            source_tree_snapshot_into(
                &entry.path(),
                relative_path.join(entry.file_name()),
                snapshot,
            )?;
        }
    } else {
        snapshot.insert(relative_path, Some(path_std_fs::read(path)?));
    }
    Ok(())
}

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
        InspectError::Io(path_std_io::Error::new(
            path_std_io::ErrorKind::InvalidData,
            error,
        ))
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
                trusted_internal_spans: Vec::new(),
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
                agent_prompt_id: prompt_id
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
                through: tau_proto::AgentHead::Root,
                model: Some("provider/model".into()),
                operation: Some(tau_proto::PromptOperation::Inference),
                activation_cut: Some(tau_proto::AgentHead::Root),
                output_length_continuation: None,
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
                agent_prompt_id: prompt_id
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
                agent_id: agent_id.clone(),
                session_id: "trace-session"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
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

/// Appends a valid standalone-compaction materialization prefix so the
/// performance projection can prove that non-inference operations are excluded.
fn append_trace_compaction_prompt(
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
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                agent_id: agent_id.clone(),
                transaction_id: tau_proto::CompactionTransactionId::parse("compact-transaction")
                    .expect("transaction id"),
                compact_prompt_id: prompt_id
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
                cut: tau_proto::AgentHead::Root,
                resume_through: None,
                model: "provider/model".into(),
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator: tau_proto::PromptOriginator::User,
                supersedes: None,
                trigger: tau_proto::StandaloneCompactionTrigger::Manual,
            }),
            tau_proto::UnixMicros::new(timestamp),
        )
        .expect("compaction start");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
                agent_prompt_id: prompt_id
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
                agent_id: agent_id.clone(),
                session_id: "trace-session"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                model: "provider/model".into(),
                model_params: Some(Default::default()),
                outer_turn_id: None,
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            }),
            tau_proto::UnixMicros::new(timestamp + 1),
        )
        .expect("compaction prompt start");
}

/// Appends one timestamp-controlled content-free terminal accounting fact.
fn append_trace_provider_terminal(
    agents_dir: &std::path::Path,
    agent_id: &str,
    prompt_id: &str,
    timestamp: u64,
    usage: Option<tau_proto::ProviderTokenUsage>,
    cost_picodollars: Option<u64>,
) {
    try_append_trace_provider_terminal(
        agents_dir,
        agent_id,
        prompt_id,
        timestamp,
        usage,
        cost_picodollars,
    )
    .expect("provider terminal");
}

fn try_append_trace_provider_terminal(
    agents_dir: &std::path::Path,
    agent_id: &str,
    prompt_id: &str,
    timestamp: u64,
    usage: Option<tau_proto::ProviderTokenUsage>,
    cost_picodollars: Option<u64>,
) -> Result<tau_core::AgentAppendOutcome, tau_core::AgentStoreError> {
    let agent_id = AgentId::parse(agent_id).expect("agent id");
    let mut store = tau_core::AgentStore::open_lazy(agents_dir).expect("agent store");
    store.append_agent_event_at(
        agent_id.as_str(),
        None,
        tau_core::AgentEventParent::InheritHead,
        Event::ProviderResponseFinished(tau_proto::ProviderResponseFinished {
            automatic_compaction_decision: None,
            agent_prompt_id: prompt_id
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            agent_id: agent_id.clone(),
            output_items: vec![assistant_message("private response")],
            stop_reason: tau_proto::ProviderStopReason::EndTurn,
            error: Some("private provider error".to_owned()),
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            originator: tau_proto::PromptOriginator::User,
            usage,
            estimated_api_cost_increment: cost_picodollars
                .map(tau_proto::EstimatedApiCost::from_picodollars),
            estimated_api_cost_rates: None,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        }),
        tau_proto::UnixMicros::new(timestamp),
    )
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
                automatic_compaction_decision: None,
                agent_prompt_id: "prompt-background"
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
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
                output_length_disposition: tau_proto::OutputLengthDisposition::None,
                originator: tau_proto::PromptOriginator::User,
                usage: None,
                estimated_api_cost_increment: None,
                estimated_api_cost_rates: None,
                compaction_original_input_tokens: None,
                compaction_output_tokens: None,
                backend: None,
                provider_attempt: Default::default(),
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
                    presentation: Default::default(),
                    call_id: (*call_id).into(),
                    tool_name: tool_name.clone(),
                    tool_type: ToolType::Function,
                    result: tau_proto::CborValue::Text(format!(
                        "private tool result sentinel for {call_id}"
                    )),
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
                presentation: Default::default(),
                call_id: "call-1".into(),
                tool_type: ToolType::Function,
                status: ToolResultStatus::Success,
                output: tau_proto::ToolResponse::from_cbor(&CborValue::Text("ok".into())),
                provider_content: Vec::new(),
            },
            tau_proto::ToolResultItem {
                presentation: Default::default(),
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

/// Ensures read-only session inspection does not create missing state
/// directories.
#[test]
fn missing_inspection_roots_are_reported_without_creating_them() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let state_dir = temp_dir.path().join("missing-state");
    let sessions_dir = state_dir.join("sessions");

    assert_eq!(
        session_list_lines(&sessions_dir).expect("session list"),
        vec!["no sessions"]
    );
    assert_eq!(
        session_lines(
            &sessions_dir,
            &tau_proto::SessionId::parse("default").expect("session id"),
        )
        .expect("session lines"),
        vec!["session default not found"]
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

    let before = source_tree_snapshot(temp_dir.path()).expect("snapshot fixture");
    assert!(matches!(
        session_list_lines(&sessions_dir),
        Err(InspectError::Io(_))
    ));
    assert_eq!(
        source_tree_snapshot(temp_dir.path()).expect("snapshot after session list"),
        before,
        "session listing must not mutate an invalid source root"
    );
    assert!(matches!(
        session_lines(
            &sessions_dir,
            &tau_proto::SessionId::parse("default").expect("session id")
        ),
        Err(InspectError::Io(_))
    ));
    assert_eq!(
        source_tree_snapshot(temp_dir.path()).expect("snapshot after session show"),
        before,
        "session inspection must not mutate an invalid source root"
    );
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
            session_id: "session-child"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: AgentId::parse("agent-root").expect("agent id"),
        },
        None,
        10,
    );
    create_trace_agent(
        temp.path(),
        "agent-grandchild",
        tau_proto::AgentCreator::Agent {
            session_id: "session-grandchild"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
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
            session_id: "session-child"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: AgentId::parse("agent-root").expect("agent id"),
        },
        None,
        11,
    );
    let legacy_dir = temp.path().join("agent-unrelated-legacy");
    std::fs::create_dir(&legacy_dir).expect("legacy agent directory");
    let mut legacy =
        path_std_fs::File::create(legacy_dir.join("events.cbor")).expect("legacy journal");
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
            session_id: "session-child"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: AgentId::parse("agent-root").expect("agent id"),
        },
        None,
        11,
    );
    path_std_fs::OpenOptions::new()
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

/// Creator-rooted membership changing after journal capture must return the
/// typed race error before any prepared artifact becomes visible.
#[test]
fn agent_trace_descendants_reject_membership_change_during_snapshot() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root_id = AgentId::parse("agent-root").expect("agent id");
    create_trace_agent(
        temp.path(),
        root_id.as_str(),
        tau_proto::AgentCreator::User,
        None,
        10,
    );

    let result = prepare_agent_trace_for_test(
        temp.path(),
        &root_id,
        DescendantSelection::Include,
        AgentTraceFormat::TauJsonl,
        || {
            create_trace_agent(
                temp.path(),
                "agent-late-child",
                tau_proto::AgentCreator::Agent {
                    session_id: "session-child"
                        .parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
                    agent_id: root_id.clone(),
                },
                None,
                11,
            );
        },
    );

    assert!(matches!(
        result,
        Err(InspectError::Trace(AgentTraceError::DescendantsChanged))
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
    let _: path_opentelemetry_proto_tonic_collector_trace::v1::ExportTraceServiceRequest =
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

/// Performance export keeps exact accounting, qualifies journal-wall intervals,
/// reports missing values, includes descendants, and excludes private bodies.
#[test]
fn agent_performance_is_content_free_exact_and_per_agent() {
    let temp = tempfile::tempdir().expect("tempdir");
    create_trace_agent(
        temp.path(),
        "agent-root",
        tau_proto::AgentCreator::User,
        None,
        1,
    );
    create_trace_agent(
        temp.path(),
        "agent-child",
        tau_proto::AgentCreator::Agent {
            agent_id: AgentId::parse("agent-root").expect("root id"),
            session_id: "trace-session"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
        },
        Some("agent-root"),
        2,
    );
    append_trace_prompt_lifecycle(temp.path(), "agent-root", "prompt-accounted", 10);
    append_trace_prompt(
        temp.path(),
        "agent-root",
        "private prompt body sentinel",
        11,
    );
    append_trace_provider_terminal(
        temp.path(),
        "agent-root",
        "prompt-accounted",
        30,
        Some(tau_proto::ProviderTokenUsage {
            prompt_sent_tokens: 1_000,
            prompt_cached_tokens: 1_100,
            prompt_cache_read_ceiling_tokens: None,
            cache: None,
            response_received_tokens: 25,
            ..Default::default()
        }),
        Some(987_654_321),
    );
    append_trace_prompt_lifecycle(temp.path(), "agent-root", "prompt-missing", 35);
    append_trace_provider_terminal(temp.path(), "agent-root", "prompt-missing", 40, None, None);
    append_trace_prompt_lifecycle(temp.path(), "agent-root", "prompt-zero", 42);
    append_trace_provider_terminal(
        temp.path(),
        "agent-root",
        "prompt-zero",
        43,
        Some(tau_proto::ProviderTokenUsage::default()),
        Some(0),
    );
    append_trace_provider_terminal(
        temp.path(),
        "agent-root",
        "prompt-terminal-only",
        44,
        Some(tau_proto::ProviderTokenUsage::default()),
        Some(0),
    );
    append_trace_compaction_prompt(temp.path(), "agent-root", "prompt-compaction", 50);
    append_trace_prompt_lifecycle(temp.path(), "agent-child", "prompt-incomplete", 20);
    append_background_tool_calls(
        temp.path(),
        "agent-child",
        &[(
            "private-tool-call-sentinel",
            "private_tool_name_sentinel",
            "private tool argument sentinel",
        )],
    );

    let output = export_trace(
        temp.path(),
        &AgentId::parse("agent-root").expect("agent id"),
        DescendantSelection::Include,
        AgentTraceFormat::AgentPerformanceJsonl,
    )
    .expect("performance trace");
    assert!(!output.contains("private response"));
    assert!(!output.contains("private provider error"));
    assert!(!output.contains("private prompt body sentinel"));
    assert!(!output.contains("private_tool_name_sentinel"));
    assert!(!output.contains("private tool argument sentinel"));
    assert!(!output.contains("private tool result sentinel"));
    assert!(!output.contains("prompt-compaction"));
    assert!(!output.contains("prompt-terminal-only"));
    let rows = output
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON line"))
        .collect::<Vec<_>>();

    assert_eq!(rows[0]["schema"], "tau.agent_performance");
    assert_eq!(
        rows[0]["timing_fidelity"],
        "recorded_at_wall_clock_append_invocation_interval"
    );
    assert_eq!(rows[0]["content_included"], false);
    assert_eq!(
        rows[0]
            .as_object()
            .expect("header object")
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([
            "schema",
            "schema_version",
            "record_type",
            "root_agent_id",
            "included_agent_ids",
            "time_unit",
            "timing_fidelity",
            "content_included",
        ])
    );

    let accounted = rows
        .iter()
        .find(|row| row["agent_prompt_id"] == "prompt-accounted")
        .expect("accounted provider prompt");
    assert_eq!(accounted["at_us"], 9);
    assert_eq!(accounted["terminal_at_us"], 29);
    assert_eq!(accounted["recorded_at_wall_elapsed_us"], 20);
    assert_eq!(accounted["prompt_sent_tokens"], 1_000);
    assert_eq!(accounted["prompt_cached_tokens"], 1_000);
    assert_eq!(accounted["response_received_tokens"], 25);
    assert_eq!(accounted["estimated_api_cost_picodollars"], 987_654_321);
    let allowed = path_std_collections::BTreeSet::from([
        "record_type",
        "agent_id",
        "agent_prompt_id",
        "model",
        "at_us",
        "terminal_at_us",
        "recorded_at_wall_elapsed_us",
        "terminal_present",
        "prompt_sent_tokens",
        "prompt_cached_tokens",
        "response_received_tokens",
        "estimated_api_cost_picodollars",
    ]);
    assert_eq!(
        accounted
            .as_object()
            .expect("provider-prompt object")
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        allowed
    );

    let missing = rows
        .iter()
        .find(|row| row["agent_prompt_id"] == "prompt-missing")
        .expect("missing-accounting provider prompt");
    assert!(missing.get("prompt_sent_tokens").is_none());
    assert!(missing.get("estimated_api_cost_picodollars").is_none());
    let zero = rows
        .iter()
        .find(|row| row["agent_prompt_id"] == "prompt-zero")
        .expect("present-zero provider prompt");
    assert_eq!(zero["prompt_sent_tokens"], 0);
    assert_eq!(zero["prompt_cached_tokens"], 0);
    assert_eq!(zero["response_received_tokens"], 0);
    assert_eq!(zero["estimated_api_cost_picodollars"], 0);

    let root_summary = rows
        .iter()
        .find(|row| {
            row["record_type"] == "agent_summary" && row["agent_id"].as_str() == Some("agent-root")
        })
        .expect("root summary");
    assert_eq!(root_summary["provider_prompt_occurrences"], 3);
    assert_eq!(root_summary["provider_prompt_complete"], 3);
    assert_eq!(root_summary["usage_reported_occurrences"], 2);
    assert_eq!(root_summary["usage_missing_occurrences"], 1);
    assert_eq!(root_summary["cost_reported_occurrences"], 2);
    assert_eq!(root_summary["cost_missing_occurrences"], 1);
    assert_eq!(root_summary["cache_hit_ratio_ppm"], 1_000_000);
    let summary_allowed = path_std_collections::BTreeSet::from([
        "record_type",
        "agent_id",
        "provider_prompt_occurrences",
        "provider_prompt_complete",
        "provider_prompt_incomplete",
        "provider_prompt_elapsed_reported",
        "provider_prompt_recorded_at_wall_elapsed_sum_us",
        "prompt_sent_tokens",
        "prompt_cached_tokens",
        "response_received_tokens",
        "cache_hit_ratio_ppm",
        "estimated_api_cost_picodollars",
        "usage_reported_occurrences",
        "usage_missing_occurrences",
        "cost_reported_occurrences",
        "cost_missing_occurrences",
    ]);
    assert_eq!(
        root_summary
            .as_object()
            .expect("summary object")
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        summary_allowed
    );

    let child_summary = rows
        .iter()
        .find(|row| {
            row["record_type"] == "agent_summary" && row["agent_id"].as_str() == Some("agent-child")
        })
        .expect("child summary");
    assert_eq!(child_summary["provider_prompt_incomplete"], 1);
    assert!(child_summary.get("prompt_sent_tokens").is_none());
}

fn performance_prompt_row(start: u64, terminal: u64) -> serde_json::Value {
    let temp = tempfile::tempdir().expect("tempdir");
    create_trace_agent(
        temp.path(),
        "agent-root",
        tau_proto::AgentCreator::User,
        None,
        1,
    );
    append_trace_prompt_lifecycle(temp.path(), "agent-root", "prompt", start);
    append_trace_provider_terminal(temp.path(), "agent-root", "prompt", terminal, None, None);
    let output = export_trace(
        temp.path(),
        &AgentId::parse("agent-root").expect("agent id"),
        DescendantSelection::RootOnly,
        AgentTraceFormat::AgentPerformanceJsonl,
    )
    .expect("performance trace");
    output
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON line"))
        .find(|row| row["record_type"] == "provider_prompt")
        .expect("provider prompt")
}

/// An unavailable start timestamp omits both its relative offset and elapsed
/// interval.
#[test]
fn agent_performance_omits_unavailable_timestamp_interval() {
    let row = performance_prompt_row(0, 10);
    assert!(row.get("at_us").is_none());
    assert!(row.get("recorded_at_wall_elapsed_us").is_none());
}

/// Equal available timestamps preserve a genuine zero elapsed interval.
#[test]
fn agent_performance_reports_valid_zero_elapsed() {
    let row = performance_prompt_row(10, 10);
    assert_eq!(row["recorded_at_wall_elapsed_us"], 0);
}

/// A decreasing wall clock never produces a synthetic elapsed interval.
#[test]
fn agent_performance_omits_decreasing_clock_interval() {
    let row = performance_prompt_row(30, 20);
    assert!(row.get("recorded_at_wall_elapsed_us").is_none());
}

/// Offline trace capture must retain writer synchronization without requesting
/// write access to any existing agent-store artifact.
#[cfg(unix)]
#[test]
fn agent_trace_exports_recursively_read_only_store() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-read-only").expect("agent id");
    create_trace_agent(
        temp.path(),
        agent_id.as_str(),
        tau_proto::AgentCreator::User,
        None,
        10,
    );
    append_trace_prompt(temp.path(), agent_id.as_str(), "immutable snapshot", 11);
    let mut restore = ReadOnlyTreeRestore::make(temp.path()).expect("remove fixture write access");

    let output = export_trace(
        temp.path(),
        &agent_id,
        DescendantSelection::RootOnly,
        AgentTraceFormat::TauJsonl,
    )
    .expect("offline trace from recursively read-only store");

    assert!(
        output.contains("immutable snapshot"),
        "the actual trace path must export the read-only fixture"
    );
    restore.restore().expect("restore fixture permissions");
}

/// Trace preparation rejects a missing root journal without changing the
/// source tree or creating a prepared artifact.
#[test]
fn agent_trace_rejects_missing_journal_without_mutating_source_tree() {
    let temp = tempfile::tempdir().expect("tempdir");
    let before = source_tree_snapshot(temp.path()).expect("snapshot fixture");
    let error = match prepare_agent_trace(
        temp.path(),
        &AgentId::parse("agent-missing").expect("agent id"),
        DescendantSelection::RootOnly,
        AgentTraceFormat::TauJsonl,
    ) {
        Err(error) => error,
        Ok(_) => panic!("missing root journal must be rejected"),
    };
    let InspectError::AgentStore(tau_core::AgentStoreError::JournalMissing { path }) = error else {
        panic!("missing root must report its missing lock journal: {error:?}");
    };
    assert_eq!(path, temp.path().join("agent-missing").join("lock"));
    assert_eq!(
        source_tree_snapshot(temp.path()).expect("snapshot after rejection"),
        before
    );
}

/// Trace preparation rejects an active writer with its nonblocking lock error
/// and leaves the durable source tree unchanged.
#[test]
fn agent_trace_rejects_active_writer_without_mutating_source_tree() {
    let temp = tempfile::tempdir().expect("tempdir");
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

    let before = source_tree_snapshot(temp.path()).expect("snapshot fixture");
    let error = match prepare_agent_trace(
        temp.path(),
        &active_id,
        DescendantSelection::RootOnly,
        AgentTraceFormat::TauJsonl,
    ) {
        Err(error) => error,
        Ok(_) => panic!("active writer must be rejected"),
    };
    let InspectError::AgentStore(tau_core::AgentStoreError::Open { path, source }) = error else {
        panic!("active writer must report its lock error: {error:?}");
    };
    assert_eq!(path, temp.path().join(active_id.as_str()).join("lock"));
    assert_eq!(source.kind(), path_std_io::ErrorKind::WouldBlock);
    assert_eq!(
        source_tree_snapshot(temp.path()).expect("snapshot after rejection"),
        before
    );
}

/// Trace preparation selects a complete inactive journal EOF rather than a
/// superseded malformed checkpoint and leaves its durable source tree
/// unchanged.
#[test]
fn agent_trace_uses_complete_eof_over_malformed_checkpoint_without_mutation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-complete-eof").expect("agent id");
    let mut store = tau_core::AgentStore::open_lazy(temp.path()).expect("agent store");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            Event::AgentStarted(tau_proto::AgentStarted {
                agent_id: agent_id.clone(),
                creator: Some(tau_proto::AgentCreator::User),
                parent_agent: None,
                role: "test".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("creation");
    let checkpoint_path = temp.path().join(agent_id.as_str()).join("meta.json");
    let mut checkpoint: serde_json::Value =
        serde_json::from_slice(&path_std_fs::read(&checkpoint_path).expect("checkpoint"))
            .expect("checkpoint JSON");
    checkpoint["journal"]["boundary_blake3_128"] = serde_json::Value::String("0".repeat(32));
    path_std_fs::write(
        &checkpoint_path,
        serde_json::to_vec(&checkpoint).expect("encode checkpoint"),
    )
    .expect("rewrite checkpoint");
    drop(store);

    let before = source_tree_snapshot(temp.path()).expect("snapshot fixture");
    let output = export_trace(
        temp.path(),
        &agent_id,
        DescendantSelection::RootOnly,
        AgentTraceFormat::TauJsonl,
    )
    .expect("complete journal EOF must supersede malformed checkpoint");
    assert!(
        output.contains(agent_id.as_str()),
        "the trace must include the complete journal"
    );
    assert_eq!(
        source_tree_snapshot(temp.path()).expect("snapshot after export"),
        before
    );
}

/// Trace preparation rejects a torn inactive journal suffix without mutating
/// the durable source tree it failed to inspect.
#[test]
fn agent_trace_rejects_torn_suffix_without_mutating_source_tree() {
    let temp = tempfile::tempdir().expect("tempdir");
    let active_id = AgentId::parse("agent-active").expect("agent id");
    create_trace_agent(
        temp.path(),
        active_id.as_str(),
        tau_proto::AgentCreator::User,
        None,
        10,
    );
    let path = temp.path().join(active_id.as_str()).join("events.cbor");
    path_std_fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("journal")
        .write_all(&[1, 2, 3])
        .expect("torn frame");

    let before = source_tree_snapshot(temp.path()).expect("snapshot fixture");
    let error = match prepare_agent_trace(
        temp.path(),
        &active_id,
        DescendantSelection::RootOnly,
        AgentTraceFormat::TauJsonl,
    ) {
        Err(error) => error,
        Ok(_) => panic!("torn journal suffix must be rejected"),
    };
    let InspectError::AgentStore(tau_core::AgentStoreError::Read {
        path: error_path,
        source,
    }) = error
    else {
        panic!("torn suffix must report its journal read error: {error:?}");
    };
    assert_eq!(error_path, path);
    assert_eq!(source.kind(), path_std_io::ErrorKind::UnexpectedEof);
    assert_eq!(
        source_tree_snapshot(temp.path()).expect("snapshot after rejection"),
        before
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
                    agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                        .expect("test identifier must be valid"),

                    session_id: SessionId::parse(session_id)
                        .expect("known-safe SessionId must be valid"),
                    agent_id: AgentId::parse(agent_id).expect("agent id"),
                    ephemeral: false,
                }),
            )
            .expect("membership append");
    }
    drop(store);

    let invalid_path = sessions_dir.join("invalid").join("events.cbor");
    write_framed_session_event(
        &invalid_path,
        &tau_core::PersistedSessionEvent {
            seq: tau_core::PersistedSessionEventSeq::new(5),
            source: None,
            event: Event::SessionAgentLoaded(SessionAgentLoaded {
                agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                    .expect("test identifier must be valid"),
                session_id: SessionId::parse("invalid")
                    .expect("known-safe SessionId must be valid"),
                agent_id: AgentId::parse("agent-bad").expect("agent id"),
                ephemeral: false,
            }),
            recorded_at: tau_proto::UnixMicros::new(0),
        },
    );

    let lines = session_list_lines(&sessions_dir).expect("session list");
    let temporary_prefix = temp_dir.path().display().to_string();
    let lines = lines
        .iter()
        .map(|line| line.replace(&temporary_prefix, "<temp>"))
        .collect::<Vec<_>>();
    assert_eq!(
        lines,
        vec![
            "healthy (1 loaded agent(s))".to_owned(),
            format!(
                "invalid (invalid session state: invalid session event sequence in {}: expected 0, got 5)",
                PathBuf::from("<temp>")
                    .join("sessions")
                    .join("invalid")
                    .join("events.cbor")
                    .display()
            ),
        ]
    );
}

/// Writes one deliberately framed session record, keeping corruption fixtures
/// independent from the serializer's incidental map-key ordering.
fn write_framed_session_event(path: &std::path::Path, event: &tau_core::PersistedSessionEvent) {
    let mut payload = Vec::new();
    ciborium::into_writer(event, &mut payload).expect("encode session fixture");
    let length = u64::try_from(payload.len()).expect("fixture payload length fits u64");
    let mut frame = length.to_le_bytes().to_vec();
    frame.extend(payload);
    path_std_fs::write(path, frame).expect("write framed session fixture");
}
