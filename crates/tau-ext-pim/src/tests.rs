use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tau_proto::{
    EventName, EventSelector, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage,
    HarnessOutputWriter,
};

use super::*;

/// Thread-safe test writer that captures startup frames emitted by tau-client.
#[derive(Clone, Default)]
struct SharedWriter {
    /// Shared buffer containing encoded harness-input frames.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedWriter {
    fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().expect("writer lock").clone()
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().expect("writer lock").extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A failed PIM reconfigure may be an attempted policy revocation. Ensure the
/// wrapper does not keep serving calls from a previously accepted email or
/// calendar module state after reporting the new configuration as rejected.
#[test]
fn rejected_reconfigure_clears_previous_module_state() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let storage = Rc::new(storage::FsStorage::new(temp.path().join("storage")));
    let mut runtime = RuntimeState::default();
    runtime
        .configure(
            configure(CborValue::Map(vec![]), temp.path()),
            storage.clone(),
        )
        .expect("initial default config is accepted");

    let rejected = CborValue::Map(vec![
        (
            CborValue::Text("email".to_owned()),
            CborValue::Map(Vec::new()),
        ),
        (
            CborValue::Text("calendar".to_owned()),
            CborValue::Map(vec![(
                CborValue::Text("unknown".to_owned()),
                CborValue::Bool(true),
            )]),
        ),
    ]);
    assert!(
        runtime
            .configure(configure(rejected, temp.path()), storage)
            .is_err()
    );

    let event = runtime
        .dispatch_tool(tau_proto::ToolStarted {
            call_id: tau_proto::ToolCallId::new("call-email"),
            tool_name: tau_proto::ToolName::new("email_list_folders"),
            arguments: CborValue::Map(vec![]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        })
        .expect("email tool is handled by PIM");

    let Event::ToolError(error) = event else {
        panic!("rejected email module should return a tool error")
    };
    assert!(
        error
            .display
            .expect("display")
            .status_text
            .contains("configuration was rejected")
    );

    let event = runtime
        .dispatch_tool(tau_proto::ToolStarted {
            call_id: tau_proto::ToolCallId::new("call-calendar"),
            tool_name: tau_proto::ToolName::new("calendar_list_calendars"),
            arguments: CborValue::Map(vec![]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        })
        .expect("calendar tool is handled by PIM");

    let Event::ToolError(error) = event else {
        panic!("rejected calendar module should return a tool error")
    };
    assert!(
        error
            .display
            .expect("display")
            .status_text
            .contains("configuration was rejected")
    );
}

/// Legacy email-shaped configs still pass through the PIM wrapper. If that
/// fallback email configuration fails after a prior successful configure, the
/// wrapper must reject both modules instead of leaving stale calendar access.
#[test]
fn rejected_legacy_fallback_reconfigure_clears_calendar_state() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let storage = Rc::new(storage::FsStorage::new(temp.path().join("storage")));
    let mut runtime = RuntimeState::default();
    runtime
        .configure(
            configure(CborValue::Map(vec![]), temp.path()),
            storage.clone(),
        )
        .expect("initial default config is accepted");

    let rejected = CborValue::Map(vec![(
        CborValue::Text("accounts".to_owned()),
        CborValue::Text("not an email account list".to_owned()),
    )]);
    assert!(
        runtime
            .configure(configure(rejected, temp.path()), storage)
            .is_err()
    );

    let event = runtime
        .dispatch_tool(tau_proto::ToolStarted {
            call_id: tau_proto::ToolCallId::new("call-calendar"),
            tool_name: tau_proto::ToolName::new("calendar_list_calendars"),
            arguments: CborValue::Map(vec![]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        })
        .expect("calendar tool is handled by PIM");

    let Event::ToolError(error) = event else {
        panic!("rejected calendar module should return a tool error")
    };
    assert!(
        error
            .display
            .expect("display")
            .status_text
            .contains("configuration was rejected")
    );
}

fn configure(config: CborValue, state_root: &std::path::Path) -> tau_proto::Configure {
    tau_proto::Configure {
        config,
        instance_name: None,
        state_dir: Some(state_root.join("state")),
        secrets: BTreeMap::new(),
    }
}

#[test]
fn self_knowledge_pim_example_matches_extension_config_shape() {
    #[derive(Deserialize)]
    struct HarnessExample {
        extensions: BTreeMap<String, ExtensionExample>,
    }

    #[derive(Deserialize)]
    struct ExtensionExample {
        config: PimExtensionConfig,
    }

    let mut harness: HarnessExample =
        serde_yaml_ng::from_str(include_str!("../config/self-knowledge.harness.yaml"))
            .expect("self-knowledge PIM example parses as YAML");
    let pim = harness
        .extensions
        .remove("std-pim")
        .expect("std-pim example exists")
        .config;

    pim.email
        .expect("email example")
        .validate()
        .expect("email config validates");
    pim.calendar
        .expect("calendar example")
        .validate()
        .expect("calendar config validates");
}

#[test]
fn action_schema_contains_email_and_calendar_roots() {
    let roots = action_schema()
        .roots
        .into_iter()
        .map(|root| root.name)
        .collect::<Vec<_>>();

    assert_eq!(roots, vec!["/email", "/calendar"]);
}

/// Calendar Google auth intentionally remains device-flow based and its finish
/// action does not accept Gmail's pasted redirect URL argument.
#[test]
fn calendar_google_auth_schema_remains_device_flow_shape() {
    let schema = action_schema();
    let start = schema
        .parse_line("/calendar auth google start google")
        .expect("calendar auth start parses");
    assert_eq!(start.action_id, "calendar.auth.google.start");
    assert_eq!(start.argv, vec!["google".to_owned()]);

    let finish = schema
        .parse_line("/calendar auth google finish google")
        .expect("calendar auth finish parses");
    assert_eq!(finish.action_id, "calendar.auth.google.finish");
    assert_eq!(finish.argv, vec!["google".to_owned()]);

    assert!(
        schema
            .parse_line(
                "/calendar auth google finish google http://127.0.0.1:54321/?state=s&code=c",
            )
            .is_err(),
        "calendar finish must not accept Gmail redirect URL arguments"
    );
}

/// PIM subscribes to `tool.started` to receive its own email/calendar
/// calls, but the harness event stream can also contain starts for
/// tools owned by other extensions. Those foreign calls must be ignored
/// instead of producing terminal tool errors that race with the real
/// provider result.
#[test]
fn ignores_tool_started_for_tools_owned_by_other_extensions() {
    let mut runtime = RuntimeState::default();
    for tool_name in ["read", email::TOOL_NAME, calendar::TOOL_NAME] {
        let invoke = tau_proto::ToolStarted {
            call_id: tau_proto::ToolCallId::new(format!("call-{tool_name}")),
            tool_name: tau_proto::ToolName::new(tool_name),
            arguments: CborValue::Map(vec![]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        };

        assert!(runtime.dispatch_tool(invoke).is_none());
    }
}

/// Ensures tau-client startup preserves PIM's split email/calendar tool
/// registrations, prompt fragments, action schema publication, exact
/// subscriptions, and pre-`Ready` ordering.
#[test]
fn startup_registers_email_and_calendar_tools() {
    let writer = SharedWriter::default();
    let written = writer.clone();
    tau_client::TauExtensionRunner::new(PimExtension)
        .run(
            std::io::Cursor::new(Vec::new()),
            writer,
            RuntimeState::default(),
        )
        .expect("startup writes");

    let bytes = written.bytes();
    let mut reader = HarnessInputReader::new(bytes.as_slice());
    let mut tools = Vec::new();
    let mut prompt_tools = Vec::new();
    let mut per_tool_prompt_tools = Vec::new();
    let mut saw_subscription = false;
    let mut saw_action_schema = false;
    let mut saw_ready = false;
    while let Some(frame) = reader.read_message().expect("frame decodes") {
        match frame {
            HarnessInputMessage::Subscribe(subscribe) => {
                assert!(!saw_ready, "Subscribe should be emitted before Ready");
                saw_subscription = subscribe.selectors
                    == vec![
                        EventSelector::Exact(EventName::TOOL_STARTED),
                        EventSelector::Exact(EventName::ACTION_INVOKE),
                    ];
            }
            HarnessInputMessage::Emit(emit)
                if matches!(emit.event.as_ref(), Event::ToolRegister(_)) =>
            {
                assert!(!saw_ready, "tools should be registered before Ready");
                let Event::ToolRegister(register) = *emit.event else {
                    unreachable!();
                };
                if register.prompt_fragment.is_some() {
                    per_tool_prompt_tools.push(register.tool.name.clone());
                }
                if register
                    .tool_group
                    .as_ref()
                    .and_then(|group| group.prompt_fragment.as_ref())
                    .is_some()
                {
                    prompt_tools.push(
                        register
                            .tool_group
                            .as_ref()
                            .expect("group with prompt")
                            .name
                            .clone(),
                    );
                }
                tools.push(register.tool.name);
            }
            HarnessInputMessage::Emit(emit)
                if matches!(emit.event.as_ref(), Event::ActionSchemaPublished(_)) =>
            {
                assert!(!saw_ready, "actions should be published before Ready");
                saw_action_schema = true;
            }
            HarnessInputMessage::Ready(_) => {
                saw_ready = true;
            }
            _ => {}
        }
    }

    assert!(saw_subscription);
    assert!(saw_action_schema);
    assert!(saw_ready);
    assert!(
        tools
            .iter()
            .any(|tool| tool.as_str() == "email_list_folders")
    );
    assert!(tools.iter().any(|tool| tool.as_str() == "email_send"));
    assert!(
        tools
            .iter()
            .any(|tool| tool.as_str() == "calendar_list_calendars")
    );
    assert!(tools.iter().any(|tool| tool.as_str() == "calendar_respond"));
    assert!(prompt_tools.iter().any(|group| group.as_str() == "email"));
    assert!(
        prompt_tools
            .iter()
            .any(|group| group.as_str() == "calendar")
    );
    assert!(
        per_tool_prompt_tools
            .iter()
            .any(|tool| tool.as_str() == "email_read")
    );
    assert!(
        per_tool_prompt_tools
            .iter()
            .any(|tool| tool.as_str() == "calendar_get")
    );
    assert!(!tools.iter().any(|tool| tool.as_str() == email::TOOL_NAME));
    assert!(
        !tools
            .iter()
            .any(|tool| tool.as_str() == calendar::TOOL_NAME)
    );
    assert_eq!(tools.len(), 18);
}

/// Ensures the public `run` path installs extension-data storage before
/// configuration and keeps tau-client's live-only action dispatch behavior.
#[test]
fn public_run_installs_storage_and_skips_replayed_actions() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let (reader_stream, mut input_stream) = UnixStream::pair().expect("unix stream pair");
    let output = SharedWriter::default();
    let written = output.clone();
    let responder = spawn_storage_responder(
        written.clone(),
        input_stream.try_clone().expect("clone input stream"),
    );
    let run_thread =
        std::thread::spawn(move || run(reader_stream, output).map_err(|error| error.to_string()));
    {
        let mut writer = HarnessOutputWriter::new(&mut input_stream);
        writer
            .write_message(&HarnessOutputMessage::Configure(configure(
                CborValue::Map(vec![]),
                temp.path(),
            )))
            .expect("write configure");
        writer
            .write_message(&HarnessOutputMessage::deliver_replay(
                tau_proto::UnixMicros::new(1),
                unknown_action("replayed-action"),
            ))
            .expect("write replayed action");
        writer
            .write_message(&HarnessOutputMessage::deliver_live(
                tau_proto::UnixMicros::new(2),
                unknown_action("live-action"),
            ))
            .expect("write live action");
        writer.flush().expect("flush input");
    }
    drop(input_stream);

    responder.join().expect("storage responder");
    run_thread.join().expect("run thread").expect("public run");

    let output_bytes = written.bytes();
    let mut reader = HarnessInputReader::new(output_bytes.as_slice());
    let mut config_errors = Vec::new();
    let mut action_errors = Vec::new();
    while let Some(frame) = reader.read_message().expect("output frame decodes") {
        match frame {
            HarnessInputMessage::ConfigError(error) => config_errors.push(error.message),
            HarnessInputMessage::Emit(emit) => {
                if let Event::ActionError(error) = *emit.event {
                    action_errors.push(error.invocation_id);
                }
            }
            _ => {}
        }
    }

    assert_eq!(config_errors, Vec::<String>::new());
    assert_eq!(
        action_errors,
        vec![tau_proto::ActionInvocationId::new("live-action")]
    );
}

fn unknown_action(invocation_id: &str) -> Event {
    Event::ActionInvoke(tau_proto::ActionInvoke {
        invocation_id: tau_proto::ActionInvocationId::new(invocation_id),
        session_id: tau_proto::SessionId::new("session-1"),
        extension_name: tau_proto::ExtensionName::new("tau-ext-pim"),
        instance_id: tau_proto::ExtensionInstanceId::from(1),
        action_id: "pim.unknown".to_owned(),
        raw_line: "/pim unknown".to_owned(),
        argv: Vec::new(),
        arguments: CborValue::Map(Vec::new()),
    })
}

fn spawn_storage_responder(
    writer: SharedWriter,
    input_stream: UnixStream,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut input_writer = HarnessOutputWriter::new(input_stream);
        let mut responded = BTreeSet::new();
        let mut last_response = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let mut made_progress = false;
            for request in extension_data_requests_from_bytes(writer.bytes()) {
                if !responded.insert(request.request_id.clone()) {
                    continue;
                }
                input_writer
                    .write_message(&extension_data_result_for_request(request))
                    .expect("write storage response");
                input_writer.flush().expect("flush storage response");
                last_response = Instant::now();
                made_progress = true;
            }
            if responded.is_empty() || made_progress {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for PIM storage requests to settle"
                );
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            if last_response.elapsed() >= Duration::from_millis(50) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    })
}

fn extension_data_requests_from_bytes(bytes: Vec<u8>) -> Vec<tau_proto::ExtensionDataRequest> {
    let mut reader = HarnessInputReader::new(bytes.as_slice());
    let mut requests = Vec::new();
    loop {
        match reader.read_message() {
            Ok(Some(HarnessInputMessage::ExtensionDataRequest(request))) => requests.push(request),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    requests
}

fn extension_data_result_for_request(
    request: tau_proto::ExtensionDataRequest,
) -> HarnessOutputMessage {
    let result = match request.op {
        tau_proto::ExtensionDataRequestOp::ReadFile { .. } => {
            tau_proto::ExtensionDataResultPayload::Error {
                kind: tau_proto::ExtensionDataErrorKind::NotFound,
                message: "missing".to_owned(),
            }
        }
        tau_proto::ExtensionDataRequestOp::WriteFile { .. } => {
            tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::WriteFile,
            }
        }
        tau_proto::ExtensionDataRequestOp::CreateFile { .. } => {
            tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::CreateFile,
            }
        }
        tau_proto::ExtensionDataRequestOp::AppendFile { .. } => {
            tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::AppendFile,
            }
        }
        tau_proto::ExtensionDataRequestOp::DeleteFile { .. } => {
            tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::DeleteFile,
            }
        }
        tau_proto::ExtensionDataRequestOp::RenameFile { .. } => {
            tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::RenameFile,
            }
        }
        tau_proto::ExtensionDataRequestOp::ListFiles { .. } => {
            tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::ListFiles {
                    entries: Vec::new(),
                },
            }
        }
    };
    HarnessOutputMessage::ExtensionDataResult(Box::new(tau_proto::ExtensionDataResult {
        request_id: request.request_id,
        result,
    }))
}
