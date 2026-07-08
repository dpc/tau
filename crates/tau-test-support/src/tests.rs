use std::cell::RefCell;
use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::thread;
use std::time::Duration;

use tau_core::{
    Connection, ConnectionMetadata, ConnectionOrigin, ConnectionSendError, ConnectionSink,
    EventBus, RoutedFrame, SessionStore, ToolRegistry, memory_connection,
};
use tau_proto::{
    AgentPromptCreated, ClientKind, ConnectionId, ContentPart, ContextItem, ContextRole, Event,
    EventName, EventSelector, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage,
    HarnessOutputWriter, MessageItem,
};
use tempfile::TempDir;

use super::*;

/// Ensures the public `TestRuntime` fixture supports both embedded and daemon
/// harness paths, preventing regressions where one end-to-end mode loses access
/// to isolated state or the echo provider.
#[test]
fn runtime_supports_embedded_and_daemon_scenarios() {
    let runtime = TestRuntime::new().expect("runtime should be created");

    let embedded = runtime
        .run_embedded("session-1", "hello")
        .expect("embedded run should succeed");
    assert!(!embedded.is_empty(), "response should not be empty");

    let daemon = runtime.spawn_daemon("session-2", Some(1));
    runtime
        .wait_until_ready(Duration::from_secs(2))
        .expect("daemon socket should appear");
    let attached = runtime
        .send_daemon_message("session-2", "hello")
        .expect("daemon message should succeed");
    assert!(!attached.is_empty(), "response should not be empty");
    daemon.join().expect("daemon should exit cleanly");
}

/// Ensures panic payload formatting keeps useful string payloads instead of
/// replacing them with an opaque daemon-thread join error message.
#[test]
fn panic_payload_label_reports_string_payloads() {
    let static_str_payload: Box<dyn std::any::Any + Send> = Box::new("static panic");
    assert_eq!(panic_payload_label(&*static_str_payload), "static panic");

    let string_payload: Box<dyn std::any::Any + Send> = Box::new(String::from("owned panic"));
    assert_eq!(panic_payload_label(&*string_payload), "owned panic");
}

struct StreamSink {
    writer: Rc<RefCell<HarnessOutputWriter<BufWriter<UnixStream>>>>,
}

impl ConnectionSink for StreamSink {
    fn send(&mut self, routed: RoutedFrame) -> Result<(), ConnectionSendError> {
        let mut writer = self.writer.borrow_mut();
        writer
            .write_message(&routed.frame)
            .map_err(|error| ConnectionSendError::new(error.to_string()))?;
        writer
            .flush()
            .map_err(|error| ConnectionSendError::new(error.to_string()))
    }
}

fn stream_connection(
    name: &str,
    kind: ClientKind,
    stream: UnixStream,
) -> (Connection, HarnessInputReader<BufReader<UnixStream>>) {
    let writer_stream = stream
        .try_clone()
        .expect("stream clone for writer should succeed");
    let connection = Connection::new(
        ConnectionMetadata {
            id: ConnectionId::default(),
            name: name.to_owned(),
            kind,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(StreamSink {
            writer: Rc::new(RefCell::new(HarnessOutputWriter::new(BufWriter::new(
                writer_stream,
            )))),
        }),
    );
    let reader = HarnessInputReader::new(BufReader::new(stream));
    (connection, reader)
}

/// Exercises a provider + tool vertical slice with downstream extension crates
/// wired through a `tau-core` bus, preventing layering regressions without
/// adding downstream dev-dependencies to `tau-core` itself.
///
/// The provider intentionally has no backend configured for the requested
/// model, so the test verifies the protocol handshake and deterministic
/// no-network error path rather than any external service behavior.
#[test]
fn deterministic_provider_and_tool_complete_one_vertical_slice() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");
    let _store = SessionStore::open(&store_path).expect("store should open");
    let mut bus = EventBus::new();
    let mut registry = ToolRegistry::new();

    let (provider_runtime_stream, provider_harness_stream) =
        UnixStream::pair().expect("provider stream pair should open");
    let (tool_runtime_stream, tool_harness_stream) =
        UnixStream::pair().expect("tool stream pair should open");

    let provider_thread = thread::spawn(move || {
        let provider_reader = provider_runtime_stream
            .try_clone()
            .expect("provider reader clone should succeed");
        tau_ext_provider_builtin::run(provider_reader, provider_runtime_stream)
            .expect("provider should run successfully");
    });
    let tool_thread = thread::spawn(move || {
        let tool_reader = tool_runtime_stream
            .try_clone()
            .expect("tool reader clone should succeed");
        tau_ext_shell::run(tool_reader, tool_runtime_stream)
            .expect("tool extension should run successfully");
    });

    let (provider_connection, mut provider_reader) = stream_connection(
        "provider-builtin",
        ClientKind::Provider,
        provider_harness_stream,
    );
    let (tool_connection, mut tool_reader) =
        stream_connection("tool", ClientKind::Tool, tool_harness_stream);
    let provider_id = bus.connect(provider_connection);
    let tool_id = bus.connect(tool_connection);

    let (ui_connection, _ui_inbox) = memory_connection("ui", ClientKind::Ui);
    let ui_id = bus.connect(ui_connection);
    bus.set_subscriptions(
        &ui_id,
        vec![EventSelector::Exact(EventName::PROVIDER_RESPONSE_FINISHED)],
        vec![EventSelector::Exact(EventName::PROVIDER_RESPONSE_FINISHED)],
    )
    .expect("ui subscription should be stored");

    // Read and process the provider's startup messages (hello, subscribe,
    // optional model publication, ready). Subscribe is a protocol message, not
    // an event, so we install subscriptions directly via `set_subscriptions`.
    let provider_hello = provider_reader
        .read_message()
        .expect("read")
        .expect("provider hello should arrive");
    assert!(matches!(provider_hello, HarnessInputMessage::Hello(_)));
    loop {
        let message = provider_reader
            .read_message()
            .expect("read")
            .expect("provider startup message should arrive");
        match message {
            HarnessInputMessage::Subscribe(sub) => {
                bus.set_subscriptions(&provider_id, sub.historical_selectors, sub.live_selectors)
                    .expect("provider subscriptions should be stored");
            }
            HarnessInputMessage::Emit(emit) => match *emit.event {
                Event::ProviderModelsUpdated(_) => {}
                other => panic!("unexpected provider startup event: {other:?}"),
            },
            HarnessInputMessage::Ready(_) => break,
            other => panic!("unexpected provider startup message: {other:?}"),
        }
    }

    let tool_hello = tool_reader
        .read_message()
        .expect("read")
        .expect("tool hello should arrive");
    assert!(matches!(tool_hello, HarnessInputMessage::Hello(_)));
    let tool_subscribe = tool_reader
        .read_message()
        .expect("read")
        .expect("tool subscribe should arrive");
    if let HarnessInputMessage::Subscribe(sub) = tool_subscribe {
        bus.set_subscriptions(&tool_id, sub.historical_selectors, sub.live_selectors)
            .expect("tool subscriptions should be stored");
    } else {
        panic!("expected tool subscribe message");
    }
    let mut registered_tool_names = Vec::new();
    loop {
        let startup_message = tool_reader
            .read_message()
            .expect("read")
            .expect("tool startup event should arrive");
        match startup_message {
            HarnessInputMessage::Emit(emit) => match *emit.event {
                Event::ToolRegister(tool_register) => {
                    let register_report = registry.register(&tool_id, tool_register.tool.clone());
                    assert!(register_report.warnings.is_empty());
                    registered_tool_names.push(tool_register.tool.name);
                }
                Event::ActionSchemaPublished(_)
                | Event::ExtensionStarting(_)
                | Event::ExtensionReady(_)
                | Event::ProviderModelsUpdated(_)
                | Event::ExtensionContextProviderRegister(_)
                | Event::ExtensionSessionContextProviderRegister(_)
                | Event::ExtPromptFragmentPublish(_) => {}
                other => panic!("unexpected tool startup event: {other:?}"),
            },
            HarnessInputMessage::Ready(_) => break,
            other => panic!("unexpected tool startup message: {other:?}"),
        }
    }
    assert!(registered_tool_names.iter().any(|name| name == "echo"));
    assert!(registered_tool_names.iter().any(|name| name == "read"));

    // Send an AgentPromptCreated directly to the provider.
    use tau_proto::ToolDefinition;

    let prompt = AgentPromptCreated {
        agent_prompt_id: "sp-1".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        session_id: "session-1".into(),
        system_prompt: "You are helpful.".to_owned(),
        context: tau_proto::PromptContext {
            blocks: vec![tau_proto::ContextBlock::UserInput(
                tau_proto::UserInputBlock {
                    items: vec![ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content: vec![ContentPart::Text {
                            text: "hello".to_owned(),
                        }],
                        phase: None,
                        responses_raw_json: None,
                    })],
                },
            )],
        },
        tools: vec![ToolDefinition {
            name: tau_proto::ToolName::new("echo"),
            model_visible_name: None,
            description: None,
            tool_type: tau_proto::ToolType::Function,
            parameters: None,
            format: None,
        }],
        tools_ref: None,
        model: "test/model".parse().expect("model id"),
        model_params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        compaction: None,
        share_user_cache_key: false,
    };
    let _ = bus.send_to(
        &provider_id,
        None,
        HarnessOutputMessage::deliver(Event::AgentPromptCreated(prompt)),
    );

    // Without a configured backend for the requested model, the provider should
    // close the turn without network I/O.
    let response = loop {
        let message = provider_reader
            .read_message()
            .expect("read")
            .expect("provider event should arrive");
        if let HarnessInputMessage::Emit(emit) = message
            && let Event::ProviderResponseFinished(r) = *emit.event
        {
            break r;
        }
    };
    assert_eq!(response.stop_reason, tau_proto::ProviderStopReason::Error);
    assert_eq!(
        response.error.as_deref(),
        Some("cannot resolve provider backend for: test/model")
    );
    assert!(response.output_items.is_empty());
    assert!(
        response
            .output_items
            .iter()
            .all(|item| !matches!(item, ContextItem::ToolCall(_)))
    );

    bus.send_to(
        &provider_id,
        Some(&ui_id),
        HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
            reason: Some("test complete".to_owned()),
        }),
    )
    .expect("provider disconnect should route");
    bus.send_to(
        &tool_id,
        Some(&ui_id),
        HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
            reason: Some("test complete".to_owned()),
        }),
    )
    .expect("tool disconnect should route");

    provider_thread
        .join()
        .expect("provider thread should finish");
    tool_thread.join().expect("tool thread should finish");
}
