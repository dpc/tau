#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd as _;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt as _;

use tau_core::{AgentEventParent, AgentStore};
use tau_proto::{
    AgentCreator, AgentId, AgentStarted, CborValue, ContextItem, Event, ObservationId, ToolCallRef,
    UnixMicros,
};

use super::*;

/// A trace with no virtual calls still emits one strict TOON document with an
/// explicit empty counted array.
#[test]
fn compact_toon_frames_zero_calls() {
    let (root, _native) = prepare_fixture();
    let mut toon = prepare_agent_trace(
        root.path(),
        &AgentId::parse("agent-stage").expect("agent id"),
        DescendantSelection::RootOnly,
        AgentTraceFormat::AgentToolsToon(AgentTraceMode::Lite),
    )
    .expect("TOON trace");
    let mut bytes = Vec::new();
    toon.copy_to(&mut bytes).expect("copy TOON");
    let text = String::from_utf8(bytes).expect("UTF-8 TOON");
    let decoded: serde_json::Value = serde_toon::from_str(&text).expect("strict TOON");

    assert!(text.contains("records[0]:"));
    assert_eq!(decoded["records"], serde_json::json!([]));
}

fn prepare_fixture() -> (tempfile::TempDir, PreparedAgentTrace) {
    let root = tempfile::tempdir().expect("state root");
    let agent_id = AgentId::parse("agent-stage").expect("agent id");
    let mut store = AgentStore::open_lazy(root.path()).expect("store");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentStarted(AgentStarted {
                agent_id: agent_id.clone(),
                creator: Some(AgentCreator::User),
                parent_agent: None,
                role: "test".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
            UnixMicros::new(1),
        )
        .expect("creation");
    drop(store);
    let prepared = prepare_agent_trace(
        root.path(),
        &agent_id,
        DescendantSelection::RootOnly,
        AgentTraceFormat::TauJsonl,
    )
    .expect("prepared trace");
    (root, prepared)
}

/// Native persisted-event export must expose the exact observation identity
/// decoded from the durable envelope.
#[test]
fn native_occurrence_exposes_canonical_observation_id() {
    let (_root, mut prepared) = prepare_fixture();
    let mut bytes = Vec::new();
    prepared.copy_to(&mut bytes).expect("copy trace");
    let lines = std::str::from_utf8(&bytes)
        .expect("UTF-8")
        .lines()
        .collect::<Vec<_>>();
    let occurrence: serde_json::Value =
        serde_json::from_str(lines.get(1).expect("event line")).expect("JSON event");
    let id = occurrence["observation_id"]
        .as_str()
        .expect("observation id string");
    assert_eq!(id.len(), 32);
    assert!(
        id.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
}

/// Public store-backed JSONL and TOON exports must decode persisted
/// observation identities, preserve qualified timing, and represent terminal
/// output in lite mode, with whole-document JSONL/TOON semantic parity across
/// resolved/incomplete calls, activation sources, and relationships.
#[test]
fn public_compact_exports_project_persisted_explicit_observations() {
    let (root, _native) = prepare_fixture();
    let agent_id = AgentId::parse("agent-stage").expect("agent id");
    let declaration = ObservationId::from_bytes([1; 16]);
    let dispatch = ObservationId::from_bytes([2; 16]);
    let classification = ObservationId::from_bytes([3; 16]);
    let terminal = ObservationId::from_bytes([4; 16]);
    let cancellation = ObservationId::from_bytes([5; 16]);
    let activation = ObservationId::from_bytes([6; 16]);
    let call = ToolCallRef {
        declaration,
        item_index: 0,
    };
    let cancel_call = ToolCallRef {
        declaration,
        item_index: 1,
    };
    let mut store = AgentStore::open_lazy(root.path()).expect("store");
    let append = |store: &mut AgentStore, id, event, at| -> Result<(), tau_core::AgentStoreError> {
        store
            .append_agent_event_at_with_observation_id(
                agent_id.as_str(),
                None,
                AgentEventParent::InheritHead,
                event,
                UnixMicros::new(at),
                id,
            )
            .map(|_| ())
    };
    append(
        &mut store,
        declaration,
        Event::ProviderResponseFinished(tau_proto::ProviderResponseFinished {
            agent_prompt_id: "prompt-tools".into(),
            agent_id: agent_id.clone(),
            output_items: vec![
                ContextItem::ToolCall(tau_proto::ToolCallItem {
                    call_id: "call-shell".into(),
                    name: tau_proto::ToolName::new("shell_command"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: CborValue::Map(vec![(
                        CborValue::Text("command".into()),
                        CborValue::Text("printf done".into()),
                    )]),
                    raw_arguments_json: None,
                    responses_envelope: None,
                }),
                ContextItem::ToolCall(tau_proto::ToolCallItem {
                    call_id: "call-cancel".into(),
                    name: tau_proto::ToolName::new("cancel"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: CborValue::Map(Vec::new()),
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
        10,
    )
    .expect("declaration");
    append(
        &mut store,
        dispatch,
        Event::AgentToolDispatchObserved(tau_proto::AgentToolDispatchObserved { call }),
        12,
    )
    .expect("dispatch");
    append(
        &mut store,
        classification,
        Event::AgentToolTerminalClassified(tau_proto::AgentToolTerminalClassified {
            call,
            terminal,
            cause: tau_proto::ToolTerminalCause::Completed,
        }),
        19,
    )
    .expect("classification");
    append(
        &mut store,
        terminal,
        Event::ProviderToolResult(tau_proto::ToolResult {
            call_id: "call-shell".into(),
            tool_name: tau_proto::ToolName::new("shell_command"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("done".into()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
        20,
    )
    .expect("terminal");
    append(
        &mut store,
        cancellation,
        Event::AgentToolCancellationRequested(tau_proto::AgentToolCancellationRequested {
            cancel_call,
            target_call: call,
        }),
        21,
    )
    .expect("cancellation relationship");
    append(
        &mut store,
        activation,
        Event::AgentActivationQueued(tau_proto::AgentActivationQueued {
            kind: tau_proto::ActivationKind::BackgroundCompletion,
            source_observation: Some(terminal),
            source_call: Some(call),
        }),
        22,
    )
    .expect("activation");
    drop(store);

    let mut jsonl = prepare_agent_trace(
        root.path(),
        &agent_id,
        DescendantSelection::RootOnly,
        AgentTraceFormat::AgentToolsJsonl(AgentTraceMode::Lite),
    )
    .expect("JSONL");
    let mut json_bytes = Vec::new();
    jsonl.copy_to(&mut json_bytes).expect("copy JSONL");
    let json_values = std::str::from_utf8(&json_bytes)
        .expect("UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON record"))
        .collect::<Vec<serde_json::Value>>();
    let call_record = json_values
        .iter()
        .skip(1)
        .find(|record| record["record_type"] == "call")
        .expect("call record");
    assert_eq!(call_record["declaration_to_dispatch_us"], 2);
    assert_eq!(call_record["dispatch_to_terminal_us"], 8);
    assert_eq!(call_record["terminal"], terminal.to_string());
    assert_eq!(call_record["output"], "done");

    let mut toon = prepare_agent_trace(
        root.path(),
        &agent_id,
        DescendantSelection::RootOnly,
        AgentTraceFormat::AgentToolsToon(AgentTraceMode::Lite),
    )
    .expect("TOON");
    let mut toon_bytes = Vec::new();
    toon.copy_to(&mut toon_bytes).expect("copy TOON");
    let decoded: serde_json::Value =
        serde_toon::from_str(std::str::from_utf8(&toon_bytes).expect("UTF-8"))
            .expect("strict TOON");
    let mut expected = json_values[0].clone();
    expected["records"] = serde_json::Value::Array(json_values[1..].to_vec());
    assert_eq!(decoded, expected);
}

/// Sensitive staging uses a mode-0600 anonymous file whose procfs descriptor
/// has no live pathname that can survive process termination.
#[test]
#[cfg(target_os = "linux")]
fn prepared_trace_staging_is_private_and_anonymous() {
    let (_root, prepared) = prepare_fixture();
    assert_eq!(
        prepared.file.metadata().expect("metadata").mode() & 0o777,
        0o600
    );
    let descriptor = format!("/proc/self/fd/{}", prepared.file.as_raw_fd());
    let target = std::fs::read_link(descriptor).expect("descriptor target");
    let target = target.to_string_lossy();

    assert!(
        target.contains("(deleted)") || target.contains("/#"),
        "anonymous staging must have no live pathname: {target}"
    );
}

/// A destination write failure is returned without persisting or renaming the
/// anonymous staged artifact.
#[test]
fn prepared_trace_copy_propagates_destination_failure() {
    /// Destination that deterministically rejects every write.
    struct FailingWriter;
    impl std::io::Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "consumer exited",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let (_root, mut prepared) = prepare_fixture();
    let error = prepared
        .copy_to(&mut FailingWriter)
        .expect_err("copy must return destination failure");

    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
}
