use std::io::{Cursor, Write};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use tau_proto::{
    ActionOutput, ActionSchema, AgentPromptSubmitted, CborValue, Configure, Event, EventSelector,
    HarnessInputMessage, HarnessInputReader, HarnessNotice, HarnessOutputMessage,
    HarnessOutputWriter, InterceptAction, InterceptRequest, InterceptionPriority, NoticeLevel,
    PromptFragment, PromptMessageClass, PromptOriginator, PromptPriority, ToolName, ToolSpec,
    ToolStarted, ToolType, UnixMicros,
};

use super::*;

/// Thread-safe test writer that captures encoded harness-input frames.
#[derive(Clone, Default)]
struct SharedWriter {
    /// Shared byte buffer written by the runner's writer thread.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedWriter {
    /// Returns a snapshot of bytes written so far.
    fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().expect("lock shared writer").clone()
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().expect("lock shared writer").extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Counts {
    /// Number of replay-aware handler invocations.
    replay_aware: usize,
    /// Number of live-only handler invocations.
    live_only: usize,
    /// Last replay marker observed by the replay-aware handler.
    last_replay: Option<bool>,
    /// Last timestamp observed by the replay-aware handler.
    last_recorded_at: Option<UnixMicros>,
    /// Number of matching tool handler invocations.
    tool_matches: usize,
    /// Number of matching action handler invocations.
    action_matches: usize,
    /// Number of intercept handler invocations.
    intercepts: usize,
}

struct StartupExtension;

impl TauExtension for StartupExtension {
    type State = ();

    fn name(&self) -> &'static str {
        "startup"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .subscribe([tau_proto::EventName::SESSION_STARTED])
            .intercept(
                EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
                InterceptionPriority::new(7),
                |_| Ok(InterceptDecision::Pass),
            )
            .tool(tool_spec("demo_tool"), |_| Ok(()))
            .startup_event(Event::HarnessNotice(HarnessNotice::new(
                "startup",
                "startup event",
                NoticeLevel::Info,
            )))
            .ready_message("ready");
    }
}

#[derive(serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DemoConfig {
    /// Example typed value used by configuration tests.
    value: u32,
}

impl Default for DemoConfig {
    fn default() -> Self {
        Self { value: 1 }
    }
}

struct ConfigExtension;

impl TauExtension for ConfigExtension {
    type State = u32;

    fn name(&self) -> &'static str {
        "config"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.configure::<DemoConfig>(|cx| {
            *cx.state = cx.config().value;
            Ok(())
        });
    }
}

struct ConfigApplyErrorExtension;

impl TauExtension for ConfigApplyErrorExtension {
    type State = Counts;

    fn name(&self) -> &'static str {
        "config-apply-error"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .configure::<DemoConfig>(|_cx| Err(ClientError::handler("apply failed")))
            .on::<HarnessNotice>(|cx| {
                cx.state.replay_aware += 1;
                Ok(())
            });
    }
}

struct ConfigErrorHookExtension;

impl TauExtension for ConfigErrorHookExtension {
    type State = usize;

    fn name(&self) -> &'static str {
        "config-error-hook"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.configure_with_error::<DemoConfig>(
            |cx| {
                *cx.state = cx.config().value as usize;
                Ok(())
            },
            |cx| {
                *cx.state += 1;
                cx.handle
                    .emit_transient(notice("parse-error-hook"))
                    .expect("emit parse error hook notice");
            },
        );
    }
}

struct ConfigApplyErrorHookExtension;

impl TauExtension for ConfigApplyErrorHookExtension {
    type State = usize;

    fn name(&self) -> &'static str {
        "config-apply-error-hook"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.configure_with_error::<DemoConfig>(
            |_cx| Err(ClientError::handler("apply failed")),
            |cx| {
                *cx.state += 1;
                cx.handle
                    .emit_transient(notice("apply-error-hook"))
                    .expect("emit apply error hook notice");
            },
        );
    }
}

#[derive(Default)]
struct RawConfigState {
    /// True once the extension refuses further config without parsing it.
    locked: bool,
    /// Last typed config value applied by the raw handler.
    applied_value: Option<u32>,
    /// Number of later event deliveries observed after a config error.
    live_events: usize,
}

struct RawConfigureExtension;

impl TauExtension for RawConfigureExtension {
    type State = RawConfigState;

    fn name(&self) -> &'static str {
        "raw-config"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .configure_raw(|cx| {
                if cx.state.locked {
                    return Err(ClientError::handler("configuration is locked"));
                }
                let config: DemoConfig = cx.parse_config()?;
                cx.state.applied_value = Some(config.value);
                cx.state.locked = true;
                Ok(())
            })
            .on::<HarnessNotice>(|cx| {
                cx.state.live_events += 1;
                Ok(())
            });
    }
}

struct ReplayExtension;

impl TauExtension for ReplayExtension {
    type State = Counts;

    fn name(&self) -> &'static str {
        "replay"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .on_restore::<HarnessNotice>(|cx| {
                cx.state.replay_aware += 1;
                cx.state.last_replay = Some(cx.is_replay());
                cx.state.last_recorded_at = cx.recorded_at;
                Ok(())
            })
            .on_live::<HarnessNotice>(|cx| {
                cx.state.replay_aware += 1;
                cx.state.last_replay = Some(cx.is_replay());
                cx.state.last_recorded_at = cx.recorded_at;
                Ok(())
            })
            .on_live::<HarnessNotice>(|cx| {
                cx.state.live_only += 1;
                Ok(())
            });
    }
}

struct ToolExtension;

impl TauExtension for ToolExtension {
    type State = Counts;

    fn name(&self) -> &'static str {
        "tool"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.tool(tool_spec("owned_tool"), |cx| {
            cx.state.tool_matches += 1;
            Ok(())
        });
    }
}

struct StopToolExtension;

impl TauExtension for StopToolExtension {
    type State = Counts;

    fn name(&self) -> &'static str {
        "stop-tool"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.tool(tool_spec("stop_tool"), |mut cx| {
            cx.state.tool_matches += 1;
            cx.request_stop();
            Ok(())
        });
    }
}

struct MultiToolExtension;

impl TauExtension for MultiToolExtension {
    type State = Counts;

    fn name(&self) -> &'static str {
        "multi-tool"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .tool(tool_spec("first_tool"), |_| Ok(()))
            .tool(tool_spec("second_tool"), |_| Ok(()))
            .tool(tool_spec("third_tool"), |_| Ok(()));
    }
}

struct ActionExtension;

impl TauExtension for ActionExtension {
    type State = Counts;

    fn name(&self) -> &'static str {
        "action-owner"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .publish_actions(ActionSchema::default())
            .action("demo.run", |cx| {
                cx.state.action_matches += 1;
                cx.emit(Event::ActionResult(tau_proto::ActionResult {
                    invocation_id: cx.invoke.invocation_id.clone(),
                    action_id: cx.invoke.action_id.clone(),
                    output: ActionOutput::Text {
                        text: "action complete".to_owned(),
                    },
                }))
            });
    }
}

struct ContextStartupExtension;

impl TauExtension for ContextStartupExtension {
    type State = ();

    fn name(&self) -> &'static str {
        "context-startup"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .register_context_provider()
            .register_session_context_provider()
            .publish_prompt_fragment(PromptFragment {
                name: "shell.cwd".to_owned(),
                priority: PromptPriority::new(900),
                template: "cwd: {{agent_context.cwd}}".to_owned().into(),
            })
            .ready_message("context ready");
    }
}

#[derive(Default)]
struct RuntimeEventState {
    /// Ordered runtime handler labels observed by typed event dispatch tests.
    seen: Vec<&'static str>,
    /// Replay flags observed by the replay-aware metadata handler.
    metadata_replay_flags: Vec<bool>,
}

struct ShellRuntimeEventsExtension;

impl TauExtension for ShellRuntimeEventsExtension {
    type State = RuntimeEventState;

    fn name(&self) -> &'static str {
        "shell-runtime-events"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .on::<tau_proto::AgentStarted>(|cx| {
                cx.state.seen.push("agent_started");
                Ok(())
            })
            .on_restore::<tau_proto::AgentMetadataSet>(|cx| {
                cx.state.seen.push("metadata_set");
                cx.state.metadata_replay_flags.push(cx.is_replay());
                Ok(())
            })
            .on_live::<tau_proto::AgentMetadataSet>(|cx| {
                cx.state.seen.push("metadata_set");
                cx.state.metadata_replay_flags.push(cx.is_replay());
                Ok(())
            })
            .on_live::<tau_proto::AgentMetadataUnset>(|cx| {
                cx.state.seen.push("metadata_unset");
                Ok(())
            })
            .on_restore::<tau_proto::SessionAgentLoaded>(|cx| {
                cx.state.seen.push("agent_loaded");
                Ok(())
            })
            .on_live::<tau_proto::SessionAgentLoaded>(|cx| {
                cx.state.seen.push("agent_loaded");
                Ok(())
            })
            .on_restore::<tau_proto::SessionAgentUnloaded>(|cx| {
                cx.state.seen.push("agent_unloaded");
                Ok(())
            })
            .on_live::<tau_proto::SessionAgentUnloaded>(|cx| {
                cx.state.seen.push("agent_unloaded");
                Ok(())
            })
            .on_live::<tau_proto::ToolCancelRequest>(|cx| {
                cx.state.seen.push("tool_cancel_request");
                Ok(())
            })
            .on_live::<tau_proto::StartAgentAccepted>(|cx| {
                cx.state.seen.push("start_agent_accepted");
                Ok(())
            })
            .on_live::<tau_proto::StartAgentResult>(|cx| {
                cx.state.seen.push("start_agent_result");
                Ok(())
            })
            .on_live::<tau_proto::UiShellCommand>(|cx| {
                cx.state.seen.push("ui_shell_command");
                Ok(())
            });
    }
}

struct ContextReadyEmitExtension;

impl TauExtension for ContextReadyEmitExtension {
    type State = ();

    fn name(&self) -> &'static str {
        "context-ready-emit"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.tool(tool_spec("context_ready_tool"), |cx| {
            cx.handle().emit_context_ready(
                tau_proto::SessionId::new("session-1"),
                tau_proto::AgentId::parse("agent-1").expect("agent id"),
            )?;
            cx.handle()
                .emit_session_context_ready(tau_proto::SessionId::new("session-1"))?;
            Ok(())
        });
    }
}

struct DetachedEmitExtension;

impl TauExtension for DetachedEmitExtension {
    type State = Counts;

    fn name(&self) -> &'static str {
        "detached-emit"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.tool(tool_spec("detached_tool"), |cx| {
            cx.handle().emit_detached(notice("detached"))?;
            Ok(())
        });
    }
}

struct BlockingDetachedWriter {
    /// Shared flag that makes post-startup writes block until the test
    /// releases.
    blocked: Arc<(Mutex<bool>, Condvar)>,
}

impl Write for BlockingDetachedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let (lock, condvar) = &*self.blocked;
        let mut blocked = lock.lock().expect("lock block flag");
        while *blocked {
            blocked = condvar.wait(blocked).expect("wait block flag");
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct BlockingSignalWriter {
    /// Shared flag that makes writes block until the test releases them.
    blocked: Arc<(Mutex<bool>, Condvar)>,
    /// Signal sent when the writer enters its blocking write path.
    entered: mpsc::Sender<()>,
}

impl Write for BlockingSignalWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.entered.send(());
        let (lock, condvar) = &*self.blocked;
        let mut blocked = lock.lock().expect("lock block flag");
        while *blocked {
            blocked = condvar.wait(blocked).expect("wait block flag");
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct BlockingDetachedState {
    /// Shared flag used by the tool handler to block detached writer output.
    blocked: Arc<(Mutex<bool>, Condvar)>,
}

struct BlockingDetachedExtension;

impl TauExtension for BlockingDetachedExtension {
    type State = BlockingDetachedState;

    fn name(&self) -> &'static str {
        "blocking-detached"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.tool(tool_spec("blocking_detached_tool"), |cx| {
            let (lock, _condvar) = &*cx.state.blocked;
            *lock.lock().expect("lock block flag") = true;
            cx.handle().emit_detached(notice("queued detached"))?;
            Ok(())
        });
    }
}

struct DropSignalWriter {
    /// Signal sent when the writer thread exits and drops its writer.
    dropped: mpsc::Sender<()>,
}

impl Write for DropSignalWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for DropSignalWriter {
    fn drop(&mut self) {
        let _ = self.dropped.send(());
    }
}

struct HandlerErrorState {
    /// Sends a cloned handle out to the test so the writer channel remains
    /// open.
    leak_tx: mpsc::Sender<ClientHandle>,
}

struct HandlerErrorExtension;

impl TauExtension for HandlerErrorExtension {
    type State = HandlerErrorState;

    fn name(&self) -> &'static str {
        "handler-error"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.tool(tool_spec("handler_error_tool"), |cx| {
            cx.state
                .leak_tx
                .send(cx.handle())
                .expect("send leaked handle");
            Err(ClientError::handler("boom"))
        });
    }
}

struct FactoryHandleState {
    /// Handle installed by the detached-writer state factory.
    handle: ClientHandle,
}

struct FactoryHandleExtension;

impl TauExtension for FactoryHandleExtension {
    type State = FactoryHandleState;

    fn name(&self) -> &'static str {
        "factory-handle"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.tool(tool_spec("factory_handle_tool"), |cx| {
            cx.state.handle.emit_detached(notice("factory handle"))?;
            Ok(())
        });
    }
}

struct RawEventExtension;

impl TauExtension for RawEventExtension {
    type State = Counts;

    fn name(&self) -> &'static str {
        "raw-event"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.on_raw_live(
            EventSelector::Prefix("harness.".to_owned()),
            |cx: RawEventContext<'_, Counts>| {
                if matches!(cx.event(), Event::HarnessNotice(_)) {
                    cx.state.live_only += 1;
                }
                Ok(())
            },
        );
    }
}

struct RoutedRawEventExtension;

impl TauExtension for RoutedRawEventExtension {
    type State = Counts;

    fn name(&self) -> &'static str {
        "routed-raw-event"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .on_raw_routed(
                EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE),
                |cx: RawEventContext<'_, Counts>| {
                    cx.state.replay_aware += 1;
                    cx.state.last_replay = Some(cx.is_replay());
                    Ok(())
                },
            )
            .on_raw_routed_live(
                EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE),
                |cx: RawEventContext<'_, Counts>| {
                    cx.state.live_only += 1;
                    Ok(())
                },
            );
    }
}

struct InterceptExtension;

impl TauExtension for InterceptExtension {
    type State = Counts;

    fn name(&self) -> &'static str {
        "intercept"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.intercept(
            EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
            InterceptionPriority::new(0),
            |cx| {
                cx.state.intercepts += 1;
                Ok(InterceptDecision::replace(Event::AgentPromptSubmitted(
                    test_prompt("fixed"),
                )))
            },
        );
    }
}

struct InterceptErrorExtension;

impl TauExtension for InterceptErrorExtension {
    type State = Counts;

    fn name(&self) -> &'static str {
        "intercept-error"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.intercept(
            EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
            InterceptionPriority::new(0),
            |_cx| Err(ClientError::handler("intercept failed")),
        );
    }
}

struct NonSendStateExtension;

impl TauExtension for NonSendStateExtension {
    type State = Rc<()>;

    fn name(&self) -> &'static str {
        "non-send-state"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.on_live::<HarnessNotice>(|_cx| Ok(()));
    }
}

struct DeferredStartupExtension;

impl TauExtension for DeferredStartupExtension {
    type State = Counts;

    fn name(&self) -> &'static str {
        "deferred-startup"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.on_raw_routed(
            EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE),
            |cx: RawEventContext<'_, Counts>| {
                if matches!(cx.event(), Event::HarnessNotice(_)) {
                    cx.state.replay_aware += 1;
                }
                Ok(())
            },
        );
    }
}

struct StaticStartupDeclarationExtension;

impl TauExtension for StaticStartupDeclarationExtension {
    type State = ();

    fn name(&self) -> &'static str {
        "static-startup-declaration"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.subscribe([tau_proto::EventName::HARNESS_NOTICE]);
    }
}

fn run_messages<E>(
    extension: E,
    state: E::State,
    input: &[HarnessOutputMessage],
) -> (E::State, Vec<HarnessInputMessage>)
where
    E: TauExtension,
{
    let mut input_bytes = Vec::new();
    let mut input_writer = HarnessOutputWriter::new(&mut input_bytes);
    for message in input {
        input_writer.write_message(message).expect("write input");
    }
    input_writer.flush().expect("flush input");

    let writer = SharedWriter::default();
    let written = writer.clone();
    let state = TauExtensionRunner::new(extension)
        .run(Cursor::new(input_bytes), writer, state)
        .expect("runner succeeds");

    let mut reader = HarnessInputReader::new(Cursor::new(written.bytes()));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("read output") {
        frames.push(frame);
    }
    (state, frames)
}

fn encode_output_messages(input: &[HarnessOutputMessage]) -> Vec<u8> {
    let mut input_bytes = Vec::new();
    let mut input_writer = HarnessOutputWriter::new(&mut input_bytes);
    for message in input {
        input_writer.write_message(message).expect("write input");
    }
    input_writer.flush().expect("flush input");
    input_bytes
}

fn frames_from_writer(writer: &SharedWriter) -> Vec<HarnessInputMessage> {
    let mut reader = HarnessInputReader::new(Cursor::new(writer.bytes()));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("read output") {
        frames.push(frame);
    }
    frames
}

fn run_error<E>(extension: E, state: E::State) -> ClientError
where
    E: TauExtension,
{
    let writer = SharedWriter::default();
    match TauExtensionRunner::new(extension).run(Cursor::new(Vec::new()), writer, state) {
        Ok(_) => panic!("runner unexpectedly succeeded"),
        Err(error) => error,
    }
}

fn tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: ToolName::new(name),
        model_visible_name: None,
        description: Some("demo".to_owned()),
        tool_type: ToolType::Function,
        parameters: None,
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
        examples: Vec::new(),
    }
}

fn config_with_unknown_field() -> HarnessOutputMessage {
    HarnessOutputMessage::Configure(Configure {
        config: tau_proto::json_to_cbor(&serde_json::json!({ "unknown": 4 })),
        instance_name: None,
        state_dir: None,
        secrets: std::collections::BTreeMap::new(),
    })
}

fn notice(text: &str) -> Event {
    Event::HarnessNotice(HarnessNotice::new("test", text, NoticeLevel::Info))
}

fn notice_frame_index(frames: &[HarnessInputMessage], message: &str) -> usize {
    frames
        .iter()
        .position(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                Event::HarnessNotice(notice) => notice.message == message,
                _ => false,
            },
            _ => false,
        })
        .expect("notice frame")
}

fn config_error_frame_index(frames: &[HarnessInputMessage]) -> usize {
    frames
        .iter()
        .position(|frame| matches!(frame, HarnessInputMessage::ConfigError(_)))
        .expect("ConfigError frame")
}

fn ready_frame_index(frames: &[HarnessInputMessage]) -> usize {
    frames
        .iter()
        .position(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
        .expect("Ready frame")
}

fn configure_message() -> HarnessOutputMessage {
    HarnessOutputMessage::Configure(Configure {
        config: tau_proto::json_to_cbor(&serde_json::json!({ "value": 3 })),
        instance_name: None,
        state_dir: None,
        secrets: std::collections::BTreeMap::new(),
    })
}

fn disconnect(reason: &str) -> HarnessOutputMessage {
    HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
        reason: Some(reason.to_owned()),
    })
}

fn tool_started(name: &str) -> HarnessOutputMessage {
    HarnessOutputMessage::deliver(Event::ToolStarted(ToolStarted {
        call_id: format!("call-{name}").into(),
        tool_name: ToolName::new(name),
        arguments: CborValue::Map(Vec::new()),
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        originator: PromptOriginator::User,
    }))
}

fn extension_data_result(
    request_id: String,
    result: tau_proto::ExtensionDataResultPayload,
) -> HarnessOutputMessage {
    HarnessOutputMessage::ExtensionDataResult(Box::new(tau_proto::ExtensionDataResult {
        request_id,
        result,
    }))
}

fn extension_data_request_from_bytes(bytes: Vec<u8>) -> Option<tau_proto::ExtensionDataRequest> {
    let mut reader = HarnessInputReader::new(Cursor::new(bytes));
    loop {
        let frame = match reader.read_message() {
            Ok(Some(frame)) => frame,
            Ok(None) => return None,
            Err(_) => return None,
        };
        if let HarnessInputMessage::ExtensionDataRequest(request) = frame {
            return Some(request);
        }
    }
}

fn spawn_extension_data_responder(
    writer: SharedWriter,
    writer_stream: UnixStream,
    result: tau_proto::ExtensionDataResultPayload,
) -> std::thread::JoinHandle<tau_proto::ExtensionDataRequest> {
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        let request = loop {
            if let Some(request) = extension_data_request_from_bytes(writer.bytes()) {
                break request;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for extension data request"
            );
            std::thread::sleep(Duration::from_millis(1));
        };
        let mut input_writer = HarnessOutputWriter::new(writer_stream);
        input_writer
            .write_message(&extension_data_result(request.request_id.clone(), result))
            .expect("write extension data result");
        input_writer.flush().expect("flush extension data result");
        request
    })
}

fn latest_extension_data_request(writer: &SharedWriter) -> tau_proto::ExtensionDataRequest {
    frames_from_writer(writer)
        .into_iter()
        .rev()
        .find_map(|frame| match frame {
            HarnessInputMessage::ExtensionDataRequest(request) => Some(request),
            _ => None,
        })
        .expect("extension data request")
}

fn action_invoke(extension_name: &str, action_id: &str) -> Event {
    Event::ActionInvoke(tau_proto::ActionInvoke {
        invocation_id: format!("invoke-{extension_name}-{action_id}").into(),
        session_id: "session-1".into(),
        extension_name: extension_name.into(),
        instance_id: 0.into(),
        action_id: action_id.to_owned(),
        raw_line: format!("/{action_id}"),
        argv: Vec::new(),
        arguments: CborValue::Map(Vec::new()),
    })
}

fn agent_id() -> tau_proto::AgentId {
    tau_proto::AgentId::parse("agent-1").expect("agent id")
}

fn agent_started() -> Event {
    Event::AgentStarted(tau_proto::AgentStarted {
        agent_id: agent_id(),
        parent_agent: None,
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    })
}

fn metadata_set() -> Event {
    Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
        agent_id: agent_id(),
        key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
        value: CborValue::Text("/tmp".to_owned()),
        inheritable: true,
    })
}

fn metadata_unset() -> Event {
    Event::AgentMetadataUnset(tau_proto::AgentMetadataUnset {
        agent_id: agent_id(),
        key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
    })
}

fn session_agent_loaded() -> Event {
    Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
        session_id: "session-1".into(),
        agent_id: agent_id(),
        ephemeral: false,
    })
}

fn session_agent_unloaded() -> Event {
    Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
        session_id: "session-1".into(),
        agent_id: agent_id(),
    })
}

fn tool_cancel_request() -> Event {
    Event::ToolCancelRequest(tau_proto::ToolCancelRequest {
        target_call_id: tau_proto::ToolCallId::new("call-1"),
    })
}

fn start_agent_accepted() -> Event {
    Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "query-1".to_owned(),
        agent_id: agent_id(),
    })
}

fn start_agent_result() -> Event {
    Event::StartAgentResult(tau_proto::StartAgentResult {
        query_id: "query-1".to_owned(),
        text: "done".to_owned(),
        error: None,
    })
}

fn ui_shell_command() -> Event {
    Event::UiShellCommand(tau_proto::UiShellCommand {
        session_id: "session-1".into(),
        command_id: "cmd-1".into(),
        command: "pwd".to_owned(),
        include_in_context: true,
        target_agent_id: Some(agent_id()),
    })
}

fn test_prompt(text: &str) -> AgentPromptSubmitted {
    AgentPromptSubmitted {
        inference_activation: false,
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        text: text.to_owned(),
        message_class: PromptMessageClass::User,
        originator: PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    }
}

/// Ensures startup frames preserve the harness-required order before `Ready`.
#[test]
fn startup_frame_order_is_stable() {
    let (_, frames) = run_messages(StartupExtension, (), &[]);

    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert!(matches!(frames[1], HarnessInputMessage::Subscribe(_)));
    assert!(matches!(frames[2], HarnessInputMessage::Intercept(_)));
    assert!(matches!(frames[3], HarnessInputMessage::Emit(_)));
    assert!(matches!(frames[4], HarnessInputMessage::Emit(_)));
    assert!(matches!(frames[5], HarnessInputMessage::Ready(_)));
    assert_eq!(frames.len(), 6);
}

/// Ensures typed configuration parse failures are reported as `ConfigError`.
#[test]
fn configure_parse_failure_sends_config_error() {
    let (_, frames) = run_messages(ConfigExtension, 0, &[config_with_unknown_field()]);

    let error = frames
        .iter()
        .find_map(|frame| match frame {
            HarnessInputMessage::ConfigError(error) => Some(error),
            _ => None,
        })
        .expect("ConfigError frame");
    assert!(error.message.contains("unknown field"));
    assert!(error.message.contains("unknown"));
}

/// Ensures configuration application failures are reported as `ConfigError` and
/// do not stop the runner.
#[test]
fn configure_application_failure_sends_config_error() {
    let (state, frames) = run_messages(
        ConfigApplyErrorExtension,
        Counts::default(),
        &[
            HarnessOutputMessage::Configure(Configure {
                config: tau_proto::json_to_cbor(&serde_json::json!({ "value": 9 })),
                instance_name: None,
                state_dir: None,
                secrets: std::collections::BTreeMap::new(),
            }),
            HarnessOutputMessage::deliver_live(UnixMicros::new(13), notice("after-error")),
        ],
    );

    let error = frames
        .iter()
        .find_map(|frame| match frame {
            HarnessInputMessage::ConfigError(error) => Some(error),
            _ => None,
        })
        .expect("ConfigError frame");
    assert_eq!(error.message, "apply failed");
    assert_eq!(state.replay_aware, 1);
}

/// Ensures typed configuration decode failures can run extension cleanup before
/// the runner emits `ConfigError`.
#[test]
fn configure_parse_failure_runs_error_hook() {
    let (state, frames) = run_messages(ConfigErrorHookExtension, 0, &[config_with_unknown_field()]);

    assert_eq!(state, 1);
    assert!(notice_frame_index(&frames, "parse-error-hook") < config_error_frame_index(&frames));
}

/// Ensures configuration application failures run the error hook before the
/// runner emits `ConfigError`.
#[test]
fn configure_application_failure_runs_error_hook() {
    let (state, frames) = run_messages(
        ConfigApplyErrorHookExtension,
        0,
        &[HarnessOutputMessage::Configure(Configure {
            config: tau_proto::json_to_cbor(&serde_json::json!({ "value": 9 })),
            instance_name: None,
            state_dir: None,
            secrets: std::collections::BTreeMap::new(),
        })],
    );

    assert_eq!(state, 1);
    assert!(notice_frame_index(&frames, "apply-error-hook") < config_error_frame_index(&frames));
    let error = frames
        .iter()
        .find_map(|frame| match frame {
            HarnessInputMessage::ConfigError(error) => Some(error),
            _ => None,
        })
        .expect("ConfigError frame");
    assert_eq!(error.message, "apply failed");
}

/// Ensures raw configuration handlers can run state-dependent policy before
/// parsing and that returned errors emit one `ConfigError` without stopping the
/// message loop.
#[test]
fn raw_configure_error_emits_config_error_and_continues() {
    let (state, frames) = run_messages(
        RawConfigureExtension,
        RawConfigState::default(),
        &[
            HarnessOutputMessage::Configure(Configure {
                config: tau_proto::json_to_cbor(&serde_json::json!({ "value": 9 })),
                instance_name: None,
                state_dir: None,
                secrets: std::collections::BTreeMap::new(),
            }),
            config_with_unknown_field(),
            HarnessOutputMessage::deliver_live(UnixMicros::new(21), notice("after-error")),
        ],
    );

    assert_eq!(state.applied_value, Some(9));
    assert_eq!(state.live_events, 1);
    let errors = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::ConfigError(error) => Some(error.message.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(errors, vec!["configuration is locked"]);
}

/// Ensures replay-aware handlers see metadata while live-only handlers skip
/// replay.
#[test]
fn replay_and_live_handlers_preserve_replay_metadata() {
    let replay_at = UnixMicros::new(10);
    let live_at = UnixMicros::new(11);
    let (state, frames) = run_messages(
        ReplayExtension,
        Counts::default(),
        &[
            HarnessOutputMessage::deliver_replay(replay_at, notice("old")),
            HarnessOutputMessage::deliver_live(live_at, notice("new")),
        ],
    );

    assert_eq!(state.replay_aware, 2);
    assert_eq!(state.live_only, 1);
    assert_eq!(state.last_replay, Some(false));
    assert_eq!(state.last_recorded_at, Some(live_at));
    assert!(matches!(
        &frames[1],
        HarnessInputMessage::Subscribe(sub)
            if sub.historical_selectors == [EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)]
                && sub.live_selectors == [EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)]
    ));
}

/// Ensures live tool handlers are dispatched only for their registered tool
/// name.
#[test]
fn tool_handler_matches_only_registered_tool_name() {
    let (state, _) = run_messages(
        ToolExtension,
        Counts::default(),
        &[tool_started("other_tool"), tool_started("owned_tool")],
    );

    assert_eq!(state.tool_matches, 1);
}

/// Ensures repeated helper-added subscriptions are coalesced without changing
/// the order of first-seen startup selectors.
#[test]
fn multi_tool_registration_subscribes_to_tool_started_once() {
    let (_, frames) = run_messages(MultiToolExtension, Counts::default(), &[]);

    let subscribe = frames
        .iter()
        .find_map(|frame| match frame {
            HarnessInputMessage::Subscribe(subscribe) => Some(subscribe),
            _ => None,
        })
        .expect("subscribe frame");
    assert_eq!(
        subscribe.live_selectors,
        [EventSelector::Exact(tau_proto::EventName::TOOL_STARTED)]
    );
}

/// Ensures action schemas are published before `Ready`, `action.invoke` is
/// subscribed exactly once, replayed invocations are skipped, and live action
/// handlers match declared action ids while leaving extension/instance routing
/// to the harness.
#[test]
fn action_schema_and_live_dispatch_match_action_after_harness_routing() {
    let (state, frames) = run_messages(
        ActionExtension,
        Counts::default(),
        &[
            HarnessOutputMessage::deliver_replay(
                UnixMicros::new(1),
                action_invoke("action-owner", "demo.run"),
            ),
            HarnessOutputMessage::deliver_live(
                UnixMicros::new(2),
                action_invoke("action-owner", "other.run"),
            ),
            HarnessOutputMessage::deliver_live(
                UnixMicros::new(3),
                action_invoke("configured-action-instance", "demo.run"),
            ),
        ],
    );

    assert_eq!(state.action_matches, 1);
    assert!(matches!(
        &frames[1],
        HarnessInputMessage::Subscribe(sub)
            if sub.live_selectors == [EventSelector::Exact(tau_proto::EventName::ACTION_INVOKE)]
    ));
    let (schema_index, published_schema) = frames
        .iter()
        .enumerate()
        .find_map(|(index, frame)| {
            let HarnessInputMessage::Emit(emit) = frame else {
                return None;
            };
            let Event::ActionSchemaPublished(published) = emit.event.as_ref() else {
                return None;
            };
            Some((index, published))
        })
        .expect("action schema published");
    assert_eq!(
        published_schema.extension_name,
        tau_proto::ExtensionName::default()
    );
    assert_eq!(published_schema.instance_id, 0.into());
    assert!(schema_index < ready_frame_index(&frames));
    let action_results = frames
        .iter()
        .filter(|frame| {
            matches!(
                frame,
                HarnessInputMessage::Emit(emit)
                    if matches!(emit.event.as_ref(), Event::ActionResult(result)
                        if result.action_id == "demo.run")
            )
        })
        .count();
    assert_eq!(action_results, 1);
}

/// Ensures context-provider startup helpers emit the existing protocol DTOs in
/// startup-event order before `Ready` without taking ownership of runtime
/// context-ready or session-ready behavior.
#[test]
fn context_provider_helpers_publish_startup_events_before_ready() {
    let (_, frames) = run_messages(ContextStartupExtension, (), &[]);

    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert!(matches!(
        frames[1],
        HarnessInputMessage::Emit(ref emit)
            if matches!(emit.event.as_ref(), Event::ExtensionContextProviderRegister(_))
    ));
    assert!(matches!(
        frames[2],
        HarnessInputMessage::Emit(ref emit)
            if matches!(
                emit.event.as_ref(),
                Event::ExtensionSessionContextProviderRegister(_)
            )
    ));
    let HarnessInputMessage::Emit(prompt_fragment_emit) = &frames[3] else {
        panic!("expected prompt fragment publish before Ready: {frames:?}");
    };
    let Event::ExtPromptFragmentPublish(publish) = prompt_fragment_emit.event.as_ref() else {
        panic!("expected prompt fragment publish before Ready: {frames:?}");
    };
    assert_eq!(publish.fragment.name, "shell.cwd");
    assert_eq!(publish.fragment.priority, PromptPriority::new(900));
    assert!(
        publish.fragment.template.contains("agent_context.cwd"),
        "prompt fragment should carry shell cwd context template",
    );
    assert!(matches!(frames[4], HarnessInputMessage::Ready(_)));
    assert_eq!(frames.len(), 5);
}

/// Ensures shell-oriented first-party typed event payloads map to their exact
/// event names and preserve replay-aware versus live-only dispatch boundaries.
#[test]
fn shell_runtime_event_payloads_subscribe_and_dispatch_with_replay_policy() {
    let (state, frames) = run_messages(
        ShellRuntimeEventsExtension,
        RuntimeEventState::default(),
        &[
            HarnessOutputMessage::deliver_replay(UnixMicros::new(1), agent_started()),
            HarnessOutputMessage::deliver_replay(UnixMicros::new(2), metadata_set()),
            HarnessOutputMessage::deliver_live(UnixMicros::new(3), metadata_set()),
            HarnessOutputMessage::deliver_replay(UnixMicros::new(4), metadata_unset()),
            HarnessOutputMessage::deliver_live(UnixMicros::new(5), metadata_unset()),
            HarnessOutputMessage::deliver_replay(UnixMicros::new(6), session_agent_loaded()),
            HarnessOutputMessage::deliver_live(UnixMicros::new(7), session_agent_loaded()),
            HarnessOutputMessage::deliver_replay(UnixMicros::new(8), session_agent_unloaded()),
            HarnessOutputMessage::deliver_live(UnixMicros::new(9), session_agent_unloaded()),
            HarnessOutputMessage::deliver_live(UnixMicros::new(10), tool_cancel_request()),
            HarnessOutputMessage::deliver_live(UnixMicros::new(11), start_agent_accepted()),
            HarnessOutputMessage::deliver_live(UnixMicros::new(12), start_agent_result()),
            HarnessOutputMessage::deliver_live(UnixMicros::new(13), ui_shell_command()),
        ],
    );

    let subscribe = frames
        .iter()
        .find_map(|frame| match frame {
            HarnessInputMessage::Subscribe(subscribe) => Some(subscribe),
            _ => None,
        })
        .expect("subscribe frame");
    assert_eq!(
        subscribe.live_selectors,
        [
            EventSelector::Exact(tau_proto::EventName::AGENT_STARTED),
            EventSelector::Exact(tau_proto::EventName::AGENT_METADATA_SET),
            EventSelector::Exact(tau_proto::EventName::AGENT_METADATA_UNSET),
            EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_LOADED),
            EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_UNLOADED),
            EventSelector::Exact(tau_proto::EventName::TOOL_CANCEL_REQUEST),
            EventSelector::Exact(tau_proto::EventName::AGENT_START_ACCEPTED),
            EventSelector::Exact(tau_proto::EventName::AGENT_START_RESULT),
            EventSelector::Exact(tau_proto::EventName::UI_SHELL_COMMAND),
        ]
    );
    assert_eq!(
        subscribe.historical_selectors,
        [
            EventSelector::Exact(tau_proto::EventName::AGENT_METADATA_SET),
            EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_LOADED),
            EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_UNLOADED),
        ]
    );
    assert_eq!(state.metadata_replay_flags, [true, false]);
    assert_eq!(
        state.seen,
        [
            "metadata_set",
            "metadata_set",
            "metadata_unset",
            "agent_loaded",
            "agent_loaded",
            "agent_unloaded",
            "agent_unloaded",
            "tool_cancel_request",
            "start_agent_accepted",
            "start_agent_result",
            "ui_shell_command",
        ]
    );
}

/// Ensures context-ready emit helpers produce the existing readiness DTOs
/// without taking ownership of readiness policy.
#[test]
fn client_handle_context_ready_helpers_emit_existing_events() {
    let (_, frames) = run_messages(
        ContextReadyEmitExtension,
        (),
        &[tool_started("context_ready_tool")],
    );

    let ready_events = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => Some(emit.event.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(ready_events.iter().any(|event| matches!(
        event,
        Event::ExtensionContextReady(ready)
            if ready.session_id == "session-1" && ready.agent_id.as_str() == "agent-1"
    )));
    assert!(ready_events.iter().any(|event| matches!(
        event,
        Event::ExtensionSessionContextReady(ready) if ready.session_id == "session-1"
    )));
}

/// Ensures detached sends still flow through the writer before runner
/// shutdown, covering background-worker style output that must not wait for
/// flush before the handler returns.
#[test]
fn detached_emit_is_written_before_shutdown() {
    let (_, frames) = run_messages(
        DetachedEmitExtension,
        Counts::default(),
        &[tool_started("detached_tool")],
    );

    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), Event::HarnessNotice(notice) if notice.message == "detached")
    )));
}

/// Ensures writer shutdown closes all handle clones before queuing `Shutdown`,
/// so synchronous sends cannot enqueue behind shutdown and wait forever for an
/// acknowledgement the writer will never send.
#[test]
fn client_handle_send_after_queued_shutdown_fails_promptly() {
    let blocked = Arc::new((Mutex::new(true), Condvar::new()));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (sender, receiver) = mpsc::channel();
    let handle = ClientHandle::new(sender);
    let cloned = handle.clone();
    let writer_thread = std::thread::spawn({
        let blocked = Arc::clone(&blocked);
        move || {
            crate::writer_thread::run_writer(
                BlockingSignalWriter {
                    blocked,
                    entered: entered_tx,
                },
                receiver,
            )
        }
    });

    handle
        .emit_detached(notice("blocked before shutdown"))
        .expect("queue blocked write");
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("writer entered blocked write");

    let shutdown_thread = std::thread::spawn(move || handle.shutdown());
    let start = std::time::Instant::now();
    loop {
        match cloned.emit_detached(notice("probe after shutdown")) {
            Ok(()) if start.elapsed() < Duration::from_secs(1) => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(()) => panic!("shutdown did not close cloned handles promptly"),
            Err(ClientError::WriterClosed) => break,
            Err(error) => panic!("unexpected detached send error: {error}"),
        }
    }
    assert!(matches!(
        cloned.emit(notice("sync after shutdown")),
        Err(ClientError::WriterClosed)
    ));

    let (lock, condvar) = &*blocked;
    *lock.lock().expect("lock block flag") = false;
    condvar.notify_all();
    shutdown_thread
        .join()
        .expect("shutdown thread")
        .expect("shutdown result");
    writer_thread
        .join()
        .expect("writer thread")
        .expect("writer result");
}

/// Ensures the detached-writer run mode returns on harness `Disconnect` even
/// when a background-style detached write is blocked behind output
/// backpressure.
#[test]
fn detached_writer_disconnect_does_not_wait_for_blocked_detached_output() {
    let blocked = Arc::new((Mutex::new(false), Condvar::new()));
    let writer = BlockingDetachedWriter {
        blocked: Arc::clone(&blocked),
    };
    let state = BlockingDetachedState {
        blocked: Arc::clone(&blocked),
    };
    let input = encode_output_messages(&[
        tool_started("blocking_detached_tool"),
        HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
            reason: Some("test".to_owned()),
        }),
    ]);
    let (done_tx, done_rx) = mpsc::channel();

    std::thread::spawn(move || {
        let result = TauExtensionRunner::new(BlockingDetachedExtension).run_detached_writer(
            Cursor::new(input),
            writer,
            state,
        );
        done_tx.send(result.is_ok()).expect("send done");
    });

    let result = done_rx.recv_timeout(Duration::from_secs(1));
    let (lock, condvar) = &*blocked;
    *lock.lock().expect("lock block flag") = false;
    condvar.notify_all();
    assert!(
        result.expect("disconnect should not wait for blocked detached output"),
        "extension should exit cleanly"
    );
}

/// Ensures non-disconnect errors in detached-writer mode still shut down and
/// join the writer instead of returning early while cloned handles keep it
/// live.
#[test]
fn detached_writer_handler_error_still_shuts_down_writer() {
    let (leak_tx, leak_rx) = mpsc::channel();
    let (dropped_tx, dropped_rx) = mpsc::channel();
    let input = encode_output_messages(&[tool_started("handler_error_tool")]);

    let error = match TauExtensionRunner::new(HandlerErrorExtension).run_detached_writer(
        Cursor::new(input),
        DropSignalWriter {
            dropped: dropped_tx,
        },
        HandlerErrorState { leak_tx },
    ) {
        Ok(_) => panic!("handler error should stop runner"),
        Err(error) => error,
    };

    let _leaked_handle = leak_rx.recv().expect("leaked handle");
    assert!(error.to_string().contains("boom"));
    dropped_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("writer should be shut down and dropped despite leaked handle");
}

/// Ensures detached-writer state factories can retain a handle for later
/// background-style output without bypassing startup ordering.
#[test]
fn detached_writer_state_factory_can_store_client_handle() {
    let input = encode_output_messages(&[tool_started("factory_handle_tool")]);
    let writer = SharedWriter::default();
    let written = writer.clone();

    TauExtensionRunner::new(FactoryHandleExtension)
        .run_detached_writer_with_state(Cursor::new(input), writer, |handle| {
            handle
                .emit_detached(notice("factory initialized"))
                .expect("factory emit");
            FactoryHandleState { handle }
        })
        .expect("run");

    let mut reader = HarnessInputReader::new(Cursor::new(written.bytes()));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("read output") {
        frames.push(frame);
    }
    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert!(
        frames
            .iter()
            .position(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
            .is_some_and(|ready_index| frames[..ready_index]
                .iter()
                .any(|frame| matches!(frame, HarnessInputMessage::Subscribe(_))))
    );
    let ready_index = frames
        .iter()
        .position(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
        .expect("ready frame");
    let factory_emit_index = frames
        .iter()
        .position(|frame| matches!(
            frame,
            HarnessInputMessage::Emit(emit)
                if matches!(emit.event.as_ref(), Event::HarnessNotice(notice) if notice.message == "factory initialized")
        ))
        .expect("factory emit");
    assert!(ready_index < factory_emit_index);
    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), Event::HarnessNotice(notice) if notice.message == "factory handle")
    )));
}

/// Ensures manual-loop startup reaches `Ready` before the state factory can use
/// its handle, preserving startup staging for custom loops with background
/// workers.
#[test]
fn manual_loop_startup_ready_precedes_state_factory_handle_output() {
    let writer = SharedWriter::default();
    let written = writer.clone();
    let runtime = TauExtensionRunner::new(FactoryHandleExtension)
        .start_manual_loop_with_state(Cursor::new(Vec::new()), writer, |handle| {
            handle
                .emit_detached(notice("manual factory initialized"))
                .expect("factory emit");
            FactoryHandleState { handle }
        })
        .expect("start manual loop");

    runtime.finish().expect("finish");

    let frames = frames_from_writer(&written);
    let ready_index = frames
        .iter()
        .position(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
        .expect("ready frame");
    let factory_emit_index = notice_frame_index(&frames, "manual factory initialized");
    assert!(ready_index < factory_emit_index);
}

/// Ensures deferred manual startup writes only `Hello`, lets the caller receive
/// initial configuration, and then preserves explicit dynamic startup order
/// before `Ready`.
#[test]
fn manual_loop_deferred_startup_writes_hello_then_dynamic_startup() {
    let input = encode_output_messages(&[configure_message()]);
    let writer = SharedWriter::default();
    let written = writer.clone();
    let mut runtime = TauExtensionRunner::new(DeferredStartupExtension)
        .start_manual_loop_deferred_startup(Cursor::new(input), writer, Counts::default())
        .expect("start deferred manual loop");

    let initial_frames = frames_from_writer(&written);
    assert_eq!(initial_frames.len(), 1);
    assert!(matches!(initial_frames[0], HarnessInputMessage::Hello(_)));
    assert!(matches!(
        runtime.recv().expect("receive initial configure"),
        ManualRuntimeInput::Message(HarnessOutputMessage::Configure(_))
    ));

    runtime
        .startup_subscribe([EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)])
        .expect("dynamic subscribe");
    runtime
        .startup_intercept(
            [EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            InterceptionPriority::new(0),
        )
        .expect("dynamic intercept");
    runtime
        .startup_event(notice("dynamic startup event"))
        .expect("dynamic startup event");
    runtime
        .startup_ready(Some("dynamic ready".to_owned()))
        .expect("dynamic ready");
    runtime.finish().expect("finish");

    let frames = frames_from_writer(&written);
    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert!(matches!(frames[1], HarnessInputMessage::Subscribe(_)));
    assert!(matches!(frames[2], HarnessInputMessage::Intercept(_)));
    assert!(matches!(
        &frames[3],
        HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), Event::HarnessNotice(notice) if notice.message == "dynamic startup event")
    ));
    assert!(matches!(
        &frames[4],
        HarnessInputMessage::Ready(ready) if ready.message.as_deref() == Some("dynamic ready")
    ));
    assert_eq!(frames.len(), 5);
}

/// Ensures config-gated extensions can report a startup configuration failure
/// and then send one inert `Ready` frame without leaking other declarations.
#[test]
fn manual_loop_deferred_startup_config_error_then_inert_ready() {
    let writer = SharedWriter::default();
    let written = writer.clone();
    let mut runtime = TauExtensionRunner::new(DeferredStartupExtension)
        .start_manual_loop_deferred_startup(Cursor::new(Vec::new()), writer, Counts::default())
        .expect("start deferred manual loop");

    runtime
        .handle()
        .config_error("dynamic config failed")
        .expect("config error");
    runtime
        .startup_ready(Some("disabled".to_owned()))
        .expect("inert ready");
    runtime.finish().expect("finish");

    let frames = frames_from_writer(&written);
    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert!(matches!(
        &frames[1],
        HarnessInputMessage::ConfigError(error) if error.message == "dynamic config failed"
    ));
    assert!(matches!(
        &frames[2],
        HarnessInputMessage::Ready(ready) if ready.message.as_deref() == Some("disabled")
    ));
    assert_eq!(frames.len(), 3);
}

/// Ensures deferred startup helpers enforce the one-way startup lifecycle and
/// reject duplicate `Ready` or late startup declarations.
#[test]
fn manual_loop_deferred_startup_rejects_duplicate_ready() {
    let writer = SharedWriter::default();
    let written = writer.clone();
    let mut runtime = TauExtensionRunner::new(DeferredStartupExtension)
        .start_manual_loop_deferred_startup(Cursor::new(Vec::new()), writer, Counts::default())
        .expect("start deferred manual loop");

    runtime.startup_ready(None).expect("first ready");
    let duplicate_ready = runtime
        .startup_ready(None)
        .expect_err("duplicate ready rejected");
    let late_event = runtime
        .startup_event(notice("too late"))
        .expect_err("late startup event rejected");
    runtime.finish().expect("finish");

    assert!(duplicate_ready.to_string().contains("already completed"));
    assert!(late_event.to_string().contains("already completed"));
    let frames = frames_from_writer(&written);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
            .count(),
        1
    );
}

/// Ensures deferred startup mode cannot accidentally drop static declarations
/// from ordinary tau-client builder helpers.
#[test]
fn manual_loop_deferred_startup_rejects_static_declarations() {
    let writer = SharedWriter::default();
    let error = match TauExtensionRunner::new(StaticStartupDeclarationExtension)
        .start_manual_loop_deferred_startup(Cursor::new(Vec::new()), writer, ())
    {
        Ok(_) => panic!("static startup declarations should be rejected"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("static startup declarations"));
}

/// Ensures the intercept-reply convenience helper emits the existing protocol
/// frame shape for custom-loop extensions that own dynamic interception policy.
#[test]
fn client_handle_intercept_reply_helper_emits_reply() {
    let writer = SharedWriter::default();
    let written = writer.clone();
    let mut runtime = TauExtensionRunner::new(DeferredStartupExtension)
        .start_manual_loop_deferred_startup(Cursor::new(Vec::new()), writer, Counts::default())
        .expect("start deferred manual loop");

    runtime
        .handle()
        .intercept_reply(InterceptAction::Drop)
        .expect("intercept reply");
    runtime.startup_ready(None).expect("ready");
    runtime.finish().expect("finish");

    let replies = frames_from_writer(&written)
        .into_iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::InterceptReply(reply) => Some(reply),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 1);
    assert!(matches!(replies[0].action, InterceptAction::Drop));
}

/// Ensures manual-loop receive distinguishes no-input timeouts, decoded
/// messages, and clean input EOF so callers can interleave timers with harness
/// dispatch.
#[test]
fn manual_loop_recv_timeout_distinguishes_timeout_message_and_input_closed() {
    let (reader, writer_stream) = UnixStream::pair().expect("unix stream pair");
    let writer = SharedWriter::default();
    let mut runtime = TauExtensionRunner::new(ReplayExtension)
        .start_manual_loop(reader, writer, Counts::default())
        .expect("start manual loop");

    assert!(matches!(
        runtime
            .recv_timeout(Duration::from_millis(10))
            .expect("timeout receive"),
        ManualRuntimeInput::Timeout
    ));

    let mut input_writer = HarnessOutputWriter::new(writer_stream);
    input_writer
        .write_message(&HarnessOutputMessage::deliver_live(
            UnixMicros::new(30),
            notice("manual"),
        ))
        .expect("write input");
    input_writer.flush().expect("flush input");
    drop(input_writer);

    let message = match runtime
        .recv_timeout(Duration::from_secs(1))
        .expect("message")
    {
        ManualRuntimeInput::Message(message) => message,
        ManualRuntimeInput::Timeout => panic!("expected message before timeout"),
        ManualRuntimeInput::InputClosed => panic!("expected message before input closed"),
    };
    assert_eq!(
        runtime.dispatch_one(message).expect("dispatch"),
        DispatchOutcome::Continue
    );
    assert!(matches!(
        runtime.recv().expect("input closed"),
        ManualRuntimeInput::InputClosed
    ));
    assert_eq!(runtime.finish().expect("finish").replay_aware, 1);
}

/// Ensures manual-loop callers can block reactively until either harness input
/// or caller-owned side-channel work wakes them, instead of using timeout
/// polling to interleave those sources.
#[test]
fn manual_loop_waker_reacts_to_harness_input_and_side_channel_work() {
    let (reader, writer_stream) = UnixStream::pair().expect("unix stream pair");
    let writer = SharedWriter::default();
    let mut runtime = TauExtensionRunner::new(ReplayExtension)
        .start_manual_loop(reader, writer, Counts::default())
        .expect("start manual loop");

    assert!(matches!(
        runtime.try_recv().expect("empty initial poll"),
        ManualRuntimePoll::Empty
    ));

    let input_writer_stream = writer_stream
        .try_clone()
        .expect("clone input writer stream");
    let mut input_writer = HarnessOutputWriter::new(input_writer_stream);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        input_writer
            .write_message(&HarnessOutputMessage::deliver_live(
                UnixMicros::new(31),
                notice("reactive"),
            ))
            .expect("write input");
        input_writer.flush().expect("flush input");
    });

    runtime.wait_for_wake();
    assert!(matches!(
        runtime.try_recv().expect("message after reader wake"),
        ManualRuntimePoll::Message(_)
    ));

    assert!(matches!(
        runtime.try_recv().expect("empty after message"),
        ManualRuntimePoll::Empty
    ));
    let side_waker = runtime.waker();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        side_waker.wake();
    });
    runtime.wait_for_wake();
    let poll = runtime
        .try_recv()
        .expect("no protocol input after side wake");
    assert!(matches!(poll, ManualRuntimePoll::Empty), "{poll:?}");

    drop(writer_stream);
    runtime.finish().expect("finish");
}

/// Ensures graceful manual-loop finish is explicitly writer-focused when input
/// is still open: it must not wait forever for an arbitrary blocking reader to
/// reach EOF.
#[test]
fn manual_loop_finish_before_input_close_detaches_reader() {
    let (reader, writer_stream) = UnixStream::pair().expect("unix stream pair");
    let writer = SharedWriter::default();
    let runtime = TauExtensionRunner::new(ReplayExtension)
        .start_manual_loop(reader, writer, Counts::default())
        .expect("start manual loop");
    let start = std::time::Instant::now();

    let state = runtime.finish().expect("finish before EOF");

    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(state.replay_aware, 0);
    drop(writer_stream);
}

/// Ensures manual-loop callers can keep emitting after input EOF and flush that
/// post-EOF work during graceful finish.
#[test]
fn manual_loop_allows_post_eof_output_before_finish() {
    let writer = SharedWriter::default();
    let written = writer.clone();
    let mut runtime = TauExtensionRunner::new(ReplayExtension)
        .start_manual_loop(Cursor::new(Vec::new()), writer, Counts::default())
        .expect("start manual loop");

    assert!(matches!(
        runtime.recv().expect("input closed"),
        ManualRuntimeInput::InputClosed
    ));
    runtime.state_mut().live_only = 5;
    assert_eq!(runtime.state().live_only, 5);
    runtime
        .handle()
        .emit(notice("post-eof"))
        .expect("post EOF emit");
    runtime.finish().expect("finish");

    let frames = frames_from_writer(&written);
    assert!(notice_frame_index(&frames, "post-eof") > ready_frame_index(&frames));
}

/// Ensures extension-data RPC uses tau-client's reader pump for demux:
/// unrelated frames are not consumed by the request helper and remain available
/// to the manual loop in original order.
#[test]
fn manual_loop_extension_data_request_preserves_unrelated_frames() {
    let (reader, writer_stream) = UnixStream::pair().expect("unix stream pair");
    let writer = SharedWriter::default();
    let written = writer.clone();
    let mut runtime = TauExtensionRunner::new(ReplayExtension)
        .start_manual_loop(reader, writer, Counts::default())
        .expect("start manual loop");

    let preload_stream = writer_stream
        .try_clone()
        .expect("clone writer stream for preload");
    let mut input_writer = HarnessOutputWriter::new(preload_stream);
    input_writer
        .write_message(&HarnessOutputMessage::deliver_live(
            UnixMicros::new(30),
            notice("before-result"),
        ))
        .expect("write unrelated");
    input_writer
        .write_message(&extension_data_result(
            "unrelated-request".to_owned(),
            tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::WriteFile,
            },
        ))
        .expect("write unrelated result");
    input_writer.flush().expect("flush unrelated input");
    drop(input_writer);

    let responder = spawn_extension_data_responder(
        written.clone(),
        writer_stream,
        tau_proto::ExtensionDataResultPayload::Ok {
            value: tau_proto::ExtensionDataValue::ReadFile {
                contents: b"stored".to_vec(),
            },
        },
    );
    let request_result = runtime
        .extension_data_request(
            tau_proto::ExtensionDataScope::User,
            tau_proto::ExtensionDataRequestOp::ReadFile {
                path: tau_proto::ExtensionDataPath::new("state.cbor"),
            },
        )
        .expect("extension data request");
    let request = responder.join().expect("responder thread");

    assert_eq!(
        request_result,
        tau_proto::ExtensionDataValue::ReadFile {
            contents: b"stored".to_vec()
        }
    );
    assert_eq!(request.scope, tau_proto::ExtensionDataScope::User);
    assert!(matches!(
        request.op,
        tau_proto::ExtensionDataRequestOp::ReadFile { ref path }
            if path.as_str() == "state.cbor"
    ));

    match runtime.recv().expect("preserved delivery") {
        ManualRuntimeInput::Message(HarnessOutputMessage::Deliver(delivery)) => {
            assert!(
                matches!(delivery.event.as_ref(), Event::HarnessNotice(notice) if notice.message == "before-result")
            );
        }
        other => panic!("expected preserved delivery, got {other:?}"),
    }
    match runtime.recv().expect("preserved unrelated result") {
        ManualRuntimeInput::Message(HarnessOutputMessage::ExtensionDataResult(result)) => {
            assert_eq!(result.request_id, "unrelated-request");
        }
        other => panic!("expected preserved unrelated result, got {other:?}"),
    }
    assert!(matches!(
        runtime.recv().expect("input closed"),
        ManualRuntimeInput::InputClosed
    ));
    runtime.finish().expect("finish");
}

/// Ensures harness extension-data errors are surfaced as RPC errors without
/// stopping the manual runtime.
#[test]
fn manual_loop_extension_data_request_reports_harness_error() {
    let (reader, writer_stream) = UnixStream::pair().expect("unix stream pair");
    let writer = SharedWriter::default();
    let written = writer.clone();
    let mut runtime = TauExtensionRunner::new(ReplayExtension)
        .start_manual_loop(reader, writer, Counts::default())
        .expect("start manual loop");

    let responder = spawn_extension_data_responder(
        written,
        writer_stream,
        tau_proto::ExtensionDataResultPayload::Error {
            kind: tau_proto::ExtensionDataErrorKind::NotFound,
            message: "missing".to_owned(),
        },
    );
    let error = runtime
        .extension_data_request(
            tau_proto::ExtensionDataScope::User,
            tau_proto::ExtensionDataRequestOp::DeleteFile {
                path: tau_proto::ExtensionDataPath::new("missing"),
            },
        )
        .expect_err("harness error should surface");
    responder.join().expect("responder thread");

    assert!(matches!(
        error,
        ExtensionDataRpcError::Harness {
            kind: tau_proto::ExtensionDataErrorKind::NotFound,
            ref message,
        } if message == "missing"
    ));
    runtime.finish().expect("finish");
}

/// Ensures extension-data RPC preserves an early Disconnect for the caller's
/// normal manual-loop shutdown path.
#[test]
fn manual_loop_extension_data_request_preserves_disconnect() {
    let (reader, writer_stream) = UnixStream::pair().expect("unix stream pair");
    let writer = SharedWriter::default();
    let mut runtime = TauExtensionRunner::new(ReplayExtension)
        .start_manual_loop(reader, writer, Counts::default())
        .expect("start manual loop");
    let mut input_writer = HarnessOutputWriter::new(writer_stream);
    input_writer
        .write_message(&HarnessOutputMessage::deliver_live(
            UnixMicros::new(32),
            notice("should not run before disconnect"),
        ))
        .expect("write unrelated delivery");
    input_writer
        .write_message(&disconnect("done"))
        .expect("write disconnect");
    input_writer.flush().expect("flush disconnect");

    let error = runtime
        .extension_data_request(
            tau_proto::ExtensionDataScope::User,
            tau_proto::ExtensionDataRequestOp::ListFiles {
                path: tau_proto::ExtensionDataPath::new(""),
            },
        )
        .expect_err("disconnect should surface");
    assert!(matches!(error, ExtensionDataRpcError::Disconnect(_)));

    assert!(matches!(
        runtime.recv().expect("preserved disconnect"),
        ManualRuntimeInput::Message(HarnessOutputMessage::Disconnect(_))
    ));
    let _ = runtime.finish_detached();
}

/// Ensures clean input EOF before a matching extension-data response is
/// reported as `InputClosed` and remains visible to the outer manual receive
/// loop.
#[test]
fn manual_loop_extension_data_request_reports_input_closed() {
    let (reader, writer_stream) = UnixStream::pair().expect("unix stream pair");
    let writer = SharedWriter::default();
    let mut runtime = TauExtensionRunner::new(ReplayExtension)
        .start_manual_loop(reader, writer, Counts::default())
        .expect("start manual loop");
    drop(writer_stream);

    let error = runtime
        .extension_data_request(
            tau_proto::ExtensionDataScope::User,
            tau_proto::ExtensionDataRequestOp::ListFiles {
                path: tau_proto::ExtensionDataPath::new(""),
            },
        )
        .expect_err("clean EOF should surface");
    assert!(matches!(error, ExtensionDataRpcError::InputClosed));
    assert!(matches!(
        runtime.recv().expect("input remains closed"),
        ManualRuntimeInput::InputClosed
    ));
    runtime.finish().expect("finish");
}

/// Ensures unrelated frames already read by the extension-data helper are
/// restored if a later malformed protocol frame turns the request into a client
/// error.
#[test]
fn manual_loop_extension_data_request_restores_frames_on_reader_error() {
    let (reader, mut writer_stream) = UnixStream::pair().expect("unix stream pair");
    let writer = SharedWriter::default();
    let mut runtime = TauExtensionRunner::new(ReplayExtension)
        .start_manual_loop(reader, writer, Counts::default())
        .expect("start manual loop");
    {
        let mut input_writer = HarnessOutputWriter::new(&mut writer_stream);
        input_writer
            .write_message(&HarnessOutputMessage::deliver_live(
                UnixMicros::new(33),
                notice("before-error"),
            ))
            .expect("write unrelated delivery");
        input_writer.flush().expect("flush unrelated input");
    }
    writer_stream
        .write_all(b"\xff")
        .expect("write malformed frame");
    writer_stream.flush().expect("flush malformed frame");
    drop(writer_stream);

    let error = runtime
        .extension_data_request(
            tau_proto::ExtensionDataScope::User,
            tau_proto::ExtensionDataRequestOp::ListFiles {
                path: tau_proto::ExtensionDataPath::new(""),
            },
        )
        .expect_err("malformed frame should surface as client error");
    assert!(matches!(error, ExtensionDataRpcError::Client(_)));
    match runtime.recv().expect("preserved delivery") {
        ManualRuntimeInput::Message(HarnessOutputMessage::Deliver(delivery)) => {
            assert!(
                matches!(delivery.event.as_ref(), Event::HarnessNotice(notice) if notice.message == "before-error")
            );
        }
        other => panic!("expected preserved delivery, got {other:?}"),
    }
    assert!(matches!(
        runtime
            .recv()
            .expect("input marked closed after reader error"),
        ManualRuntimeInput::InputClosed
    ));
    runtime.finish().expect("finish");
}

/// Ensures timed extension-data requests return promptly and leave later frames
/// available to the manual loop.
#[test]
fn manual_loop_extension_data_request_timeout_keeps_runtime_usable() {
    let (reader, writer_stream) = UnixStream::pair().expect("unix stream pair");
    let writer = SharedWriter::default();
    let written = writer.clone();
    let mut runtime = TauExtensionRunner::new(ReplayExtension)
        .start_manual_loop(reader, writer, Counts::default())
        .expect("start manual loop");

    let error = runtime
        .extension_data_request_timeout(
            tau_proto::ExtensionDataScope::User,
            tau_proto::ExtensionDataRequestOp::ListFiles {
                path: tau_proto::ExtensionDataPath::new(""),
            },
            Duration::from_millis(10),
        )
        .expect_err("request should time out");
    assert!(matches!(error, ExtensionDataRpcError::Timeout));
    let request = latest_extension_data_request(&written);
    assert_eq!(request.scope, tau_proto::ExtensionDataScope::User);

    let mut input_writer = HarnessOutputWriter::new(writer_stream);
    input_writer
        .write_message(&extension_data_result(
            request.request_id.clone(),
            tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::ListFiles { entries: vec![] },
            },
        ))
        .expect("write late extension data result");
    input_writer
        .write_message(&HarnessOutputMessage::deliver_live(
            UnixMicros::new(34),
            notice("after-timeout"),
        ))
        .expect("write later message");
    input_writer.flush().expect("flush later message");
    match runtime.recv().expect("late result remains available") {
        ManualRuntimeInput::Message(HarnessOutputMessage::ExtensionDataResult(result)) => {
            assert_eq!(result.request_id, request.request_id);
        }
        other => panic!("expected late extension data result, got {other:?}"),
    }
    match runtime.recv().expect("runtime remains usable") {
        ManualRuntimeInput::Message(HarnessOutputMessage::Deliver(delivery)) => {
            assert!(
                matches!(delivery.event.as_ref(), Event::HarnessNotice(notice) if notice.message == "after-timeout")
            );
        }
        other => panic!("expected later delivery, got {other:?}"),
    }
    runtime.finish().expect("finish");
}

/// Ensures handler-owned code can use `ExtensionDataClient` without direct
/// transport access, which is the storage shape needed by PIM-style migrations.
#[test]
fn manual_loop_extension_data_client_works_inside_handler() {
    /// State used to prove handler-owned storage can retain an extension-data
    /// client without making the manual runtime own PIM policy.
    #[derive(Default)]
    struct StorageState {
        /// Client installed by the test after startup and used inside the tool
        /// handler.
        client: Option<ExtensionDataClient>,
        /// Contents returned by the correlated extension-data read.
        contents: Vec<u8>,
    }

    /// Minimal extension whose tool handler performs one extension-data read.
    struct StorageExtension;

    impl TauExtension for StorageExtension {
        type State = StorageState;

        fn name(&self) -> &'static str {
            "storage"
        }

        fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
            builder.tool(tool_spec("load"), |cx| {
                let client = cx
                    .state
                    .client
                    .as_ref()
                    .expect("extension data client installed");
                match client.request(
                    tau_proto::ExtensionDataScope::User,
                    tau_proto::ExtensionDataRequestOp::ReadFile {
                        path: tau_proto::ExtensionDataPath::new("handler-state.cbor"),
                    },
                ) {
                    Ok(tau_proto::ExtensionDataValue::ReadFile { contents }) => {
                        cx.state.contents = contents;
                        Ok(())
                    }
                    Ok(other) => Err(ClientError::handler(format!(
                        "unexpected extension data value: {other:?}"
                    ))),
                    Err(error) => Err(ClientError::handler(error.to_string())),
                }
            });
        }
    }

    let (reader, writer_stream) = UnixStream::pair().expect("unix stream pair");
    let writer = SharedWriter::default();
    let written = writer.clone();
    let mut runtime = TauExtensionRunner::new(StorageExtension)
        .start_manual_loop(reader, writer, StorageState::default())
        .expect("start manual loop");
    runtime.state_mut().client = Some(runtime.extension_data_client());

    let responder = spawn_extension_data_responder(
        written,
        writer_stream,
        tau_proto::ExtensionDataResultPayload::Ok {
            value: tau_proto::ExtensionDataValue::ReadFile {
                contents: b"handler contents".to_vec(),
            },
        },
    );
    assert_eq!(
        runtime
            .dispatch_one(tool_started("load"))
            .expect("dispatch storage handler"),
        DispatchOutcome::Continue
    );
    let request = responder.join().expect("responder thread");
    assert!(matches!(
        request.op,
        tau_proto::ExtensionDataRequestOp::ReadFile { ref path }
            if path.as_str() == "handler-state.cbor"
    ));
    assert_eq!(runtime.state().contents, b"handler contents");
    runtime.finish().expect("finish");
}

/// Ensures manual-loop dispatch preserves config-error emission and continues
/// to process later messages after the caller feeds them one at a time.
#[test]
fn manual_loop_dispatch_config_error_continues() {
    let writer = SharedWriter::default();
    let written = writer.clone();
    let mut runtime = TauExtensionRunner::new(RawConfigureExtension)
        .start_manual_loop(Cursor::new(Vec::new()), writer, RawConfigState::default())
        .expect("start manual loop");

    assert_eq!(
        runtime
            .dispatch_one(HarnessOutputMessage::Configure(Configure {
                config: tau_proto::json_to_cbor(&serde_json::json!({ "value": 7 })),
                instance_name: None,
                state_dir: None,
                secrets: std::collections::BTreeMap::new(),
            }))
            .expect("valid config"),
        DispatchOutcome::Continue
    );
    assert_eq!(
        runtime
            .dispatch_one(config_with_unknown_field())
            .expect("config error"),
        DispatchOutcome::Continue
    );
    assert_eq!(
        runtime
            .dispatch_one(HarnessOutputMessage::deliver_live(
                UnixMicros::new(31),
                notice("after manual config error"),
            ))
            .expect("event after error"),
        DispatchOutcome::Continue
    );

    let state = runtime.finish().expect("finish");
    assert_eq!(state.applied_value, Some(7));
    assert_eq!(state.live_events, 1);
    let errors = frames_from_writer(&written)
        .into_iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::ConfigError(error) => Some(error.message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(errors, vec!["configuration is locked"]);
}

/// Ensures manual-loop dispatch uses the same replay-aware and live-only event
/// filtering as the owned runner.
#[test]
fn manual_loop_dispatch_preserves_replay_filtering() {
    let writer = SharedWriter::default();
    let mut runtime = TauExtensionRunner::new(ReplayExtension)
        .start_manual_loop(Cursor::new(Vec::new()), writer, Counts::default())
        .expect("start manual loop");

    runtime
        .dispatch_one(HarnessOutputMessage::deliver_replay(
            UnixMicros::new(32),
            notice("old manual"),
        ))
        .expect("dispatch replay");
    runtime
        .dispatch_one(HarnessOutputMessage::deliver_live(
            UnixMicros::new(33),
            notice("new manual"),
        ))
        .expect("dispatch live");

    let state = runtime.finish().expect("finish");
    assert_eq!(state.replay_aware, 2);
    assert_eq!(state.live_only, 1);
}

/// Ensures manual-loop dispatch reports tool-requested stops to the caller
/// without turning them into fatal handler errors.
#[test]
fn manual_loop_dispatch_reports_stop_requested() {
    let writer = SharedWriter::default();
    let mut runtime = TauExtensionRunner::new(StopToolExtension)
        .start_manual_loop(Cursor::new(Vec::new()), writer, Counts::default())
        .expect("start manual loop");

    assert_eq!(
        runtime
            .dispatch_one(tool_started("stop_tool"))
            .expect("dispatch stop tool"),
        DispatchOutcome::StopRequested
    );

    let state = runtime.finish().expect("finish");
    assert_eq!(state.tool_matches, 1);
}

/// Ensures manual-loop intercept dispatch still sends exactly one pass-through
/// reply before surfacing a handler error.
#[test]
fn manual_loop_intercept_error_sends_one_reply() {
    let writer = SharedWriter::default();
    let written = writer.clone();
    let mut runtime = TauExtensionRunner::new(InterceptErrorExtension)
        .start_manual_loop(Cursor::new(Vec::new()), writer, Counts::default())
        .expect("start manual loop");

    let error = runtime
        .dispatch_one(HarnessOutputMessage::InterceptRequest(InterceptRequest {
            event: Box::new(Event::AgentPromptSubmitted(test_prompt("original"))),
            transient: false,
        }))
        .expect_err("intercept error");
    assert_eq!(error.to_string(), "intercept failed");
    runtime.finish().expect("finish");

    let replies = frames_from_writer(&written)
        .into_iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::InterceptReply(reply) => Some(reply),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 1);
    assert!(matches!(&replies[0].action, InterceptAction::Pass(None)));
}

/// Ensures manual-loop detached finish does not wait on blocked background
/// output after the caller has observed a protocol disconnect.
#[test]
fn manual_loop_detached_finish_does_not_wait_for_blocked_output() {
    let blocked = Arc::new((Mutex::new(false), Condvar::new()));
    let writer = BlockingDetachedWriter {
        blocked: Arc::clone(&blocked),
    };
    let state = BlockingDetachedState {
        blocked: Arc::clone(&blocked),
    };
    let mut runtime = TauExtensionRunner::new(BlockingDetachedExtension)
        .start_manual_loop(Cursor::new(Vec::new()), writer, state)
        .expect("start manual loop");

    runtime
        .dispatch_one(tool_started("blocking_detached_tool"))
        .expect("dispatch tool");
    assert!(matches!(
        runtime.dispatch_one(disconnect("done")).expect("disconnect"),
        DispatchOutcome::Disconnect(disconnect) if disconnect.reason.as_deref() == Some("done")
    ));
    let _state = runtime.finish_detached();

    let (lock, condvar) = &*blocked;
    *lock.lock().expect("lock block flag") = false;
    condvar.notify_all();
}

/// Ensures the manual-loop API keeps extension state caller-thread-local
/// instead of requiring `State: Send` merely because reader and writer run on
/// threads.
#[test]
fn manual_loop_state_does_not_need_to_be_send() {
    let writer = SharedWriter::default();
    let runtime = TauExtensionRunner::new(NonSendStateExtension)
        .start_manual_loop(Cursor::new(Vec::new()), writer, Rc::new(()))
        .expect("start manual loop");

    let _state = runtime.finish().expect("finish");
}

/// Ensures raw event handlers can select a family of event names while still
/// honoring live-only replay filtering.
#[test]
fn raw_event_handler_matches_prefix_and_skips_replay() {
    let (state, frames) = run_messages(
        RawEventExtension,
        Counts::default(),
        &[
            HarnessOutputMessage::deliver_replay(UnixMicros::new(8), notice("old")),
            HarnessOutputMessage::deliver_live(UnixMicros::new(9), notice("new")),
        ],
    );

    assert_eq!(state.live_only, 1);
    assert!(matches!(
        &frames[1],
        HarnessInputMessage::Subscribe(sub)
            if sub.live_selectors == [EventSelector::Prefix("harness.".to_owned())]
    ));
}

/// Ensures routed raw handlers do not add startup subscriptions while still
/// preserving replay-aware and live-only dispatch policy for direct deliveries.
#[test]
fn routed_raw_event_handler_does_not_subscribe() {
    let (state, frames) = run_messages(
        RoutedRawEventExtension,
        Counts::default(),
        &[
            HarnessOutputMessage::deliver_replay(UnixMicros::new(8), notice("old")),
            HarnessOutputMessage::deliver_live(UnixMicros::new(9), notice("new")),
        ],
    );

    assert_eq!(state.replay_aware, 2);
    assert_eq!(state.live_only, 1);
    assert_eq!(state.last_replay, Some(false));
    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(frame, HarnessInputMessage::Subscribe(_))),
        "routed handlers must not broaden startup subscriptions: {frames:?}",
    );
    assert!(matches!(frames[1], HarnessInputMessage::Ready(_)));
}

/// Ensures the intercept abstraction sends exactly one reply per request.
#[test]
fn intercept_request_gets_exactly_one_reply() {
    let (_, frames) = run_messages(
        InterceptExtension,
        Counts::default(),
        &[HarnessOutputMessage::InterceptRequest(InterceptRequest {
            event: Box::new(Event::AgentPromptSubmitted(test_prompt("original"))),
            transient: false,
        })],
    );

    let replies = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::InterceptReply(reply) => Some(reply),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 1);
    assert!(matches!(
        &replies[0].action,
        InterceptAction::Pass(Some(event))
            if matches!(event.as_ref(), Event::AgentPromptSubmitted(prompt) if prompt.text == "fixed")
    ));
}

/// Ensures a harness disconnect stops the runner before later queued frames
/// run.
#[test]
fn disconnect_stops_runner() {
    let (state, _) = run_messages(
        ToolExtension,
        Counts::default(),
        &[
            HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
                reason: Some("done".to_owned()),
            }),
            tool_started("owned_tool"),
        ],
    );

    assert_eq!(state.tool_matches, 0);
}

/// Ensures the startup helper can still send an intentionally empty
/// subscription.
#[test]
fn empty_subscribe_frame_remains_available() {
    struct EmptySubscribe;

    impl TauExtension for EmptySubscribe {
        type State = ();

        fn name(&self) -> &'static str {
            "empty-subscribe"
        }

        fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
            builder.subscribe_empty();
        }
    }

    let (_, frames) = run_messages(EmptySubscribe, (), &[]);
    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert!(
        matches!(&frames[1], HarnessInputMessage::Subscribe(sub) if sub.live_selectors.is_empty())
    );
    assert!(matches!(frames[2], HarnessInputMessage::Ready(_)));
    assert_eq!(frames.len(), 3);
}

struct NoticePlugin;

impl ExtensionPlugin<Counts> for NoticePlugin {
    fn register(self, builder: &mut ExtensionBuilder<Counts>) {
        builder.on_live::<HarnessNotice>(|cx| {
            cx.state.live_only += 1;
            Ok(())
        });
    }
}

struct PluginExtension;

impl TauExtension for PluginExtension {
    type State = Counts;

    fn name(&self) -> &'static str {
        "plugin"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.install(NoticePlugin);
    }
}

/// Ensures reusable plugins can compose handlers into an extension builder.
#[test]
fn plugin_install_registers_handlers() {
    let (state, _) = run_messages(
        PluginExtension,
        Counts::default(),
        &[HarnessOutputMessage::deliver_live(
            UnixMicros::new(12),
            notice("plugin"),
        )],
    );

    assert_eq!(state.live_only, 1);
}

/// Ensures the builder rejects an extension that mixes intercept priorities.
#[test]
fn builder_rejects_mixed_intercept_priorities() {
    struct MixedPriorities;

    impl TauExtension for MixedPriorities {
        type State = ();

        fn name(&self) -> &'static str {
            "mixed-priorities"
        }

        fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
            builder
                .intercept(
                    EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
                    InterceptionPriority::new(1),
                    |_| Ok(InterceptDecision::Pass),
                )
                .intercept(
                    EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE),
                    InterceptionPriority::new(2),
                    |_| Ok(InterceptDecision::Pass),
                );
        }
    }

    let error = run_error(MixedPriorities, ());
    assert!(matches!(error, ClientError::Builder(_)));
    assert!(error.to_string().contains("mixed interception priorities"));
}

/// Ensures the builder rejects registering more than one intercept handler.
#[test]
fn builder_rejects_duplicate_intercept_handlers() {
    struct DuplicateHandlers;

    impl TauExtension for DuplicateHandlers {
        type State = ();

        fn name(&self) -> &'static str {
            "duplicate-handlers"
        }

        fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
            builder
                .intercept(
                    EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
                    InterceptionPriority::new(1),
                    |_| Ok(InterceptDecision::Pass),
                )
                .intercept(
                    EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE),
                    InterceptionPriority::new(1),
                    |_| Ok(InterceptDecision::Pass),
                );
        }
    }

    let error = run_error(DuplicateHandlers, ());
    assert!(matches!(error, ClientError::Builder(_)));
    assert!(error.to_string().contains("only one intercept handler"));
}

/// Protocol I/O keys should identify delivered events by their inner event name
/// so generic stats point at the event family that generated traffic, not the
/// transport envelope.
#[test]
fn protocol_io_output_message_key_uses_delivered_event_name() {
    let message = HarnessOutputMessage::deliver(Event::TermBell(tau_proto::TermBell {}));

    assert_eq!(output_message_key(&message), "term.bell");
}

/// Protocol I/O keys should identify emitted events by their inner event name
/// so peer-originated event traffic is grouped consistently with delivered
/// traffic.
#[test]
fn protocol_io_input_message_key_uses_emitted_event_name() {
    let message = HarnessInputMessage::emit(Event::TermBell(tau_proto::TermBell {}));

    assert_eq!(input_message_key(&message), "term.bell");
}

/// Cumulative protocol I/O counters must survive sample draining because debug
/// dumps are lifetime counters while rolling samples drive transient status.
#[test]
fn protocol_io_meter_keeps_cumulative_stats_after_sampling() {
    let meter = ProtocolIoMeter::default();
    meter.record_bytes(
        ProtocolIoDirection::Downlink,
        "small.event".to_owned(),
        Some(10),
    );
    meter.record_bytes(
        ProtocolIoDirection::Downlink,
        "small.event".to_owned(),
        Some(15),
    );

    let sample = meter.take_sample();

    assert_eq!(sample.downlink_bytes, 25);
    assert_eq!(
        meter
            .cumulative_stats()
            .downlink
            .get("small.event")
            .copied(),
        Some(ProtocolIoFrameStats {
            count: 2,
            bytes: 25
        })
    );
}

/// Protocol I/O meters must bound distinct keys so a peer cannot grow harness
/// memory forever by emitting unique custom event names.
#[test]
fn protocol_io_meter_buckets_overflow_after_key_cap() {
    let meter = ProtocolIoMeter::default();
    for index in 0..(PROTOCOL_IO_MAX_KEYS_PER_DIRECTION + 4) {
        meter.record_bytes(
            ProtocolIoDirection::Uplink,
            format!("custom.event_{index}"),
            Some(1),
        );
    }

    let sample = meter.take_sample();
    let cumulative = meter.cumulative_stats();

    assert_eq!(
        sample.uplink_breakdown.len(),
        PROTOCOL_IO_MAX_KEYS_PER_DIRECTION
    );
    assert_eq!(cumulative.uplink.len(), PROTOCOL_IO_MAX_KEYS_PER_DIRECTION);
    assert_eq!(
        sample.uplink_breakdown.get(PROTOCOL_IO_OVERFLOW_KEY),
        Some(&5)
    );
    assert_eq!(
        cumulative.uplink.get(PROTOCOL_IO_OVERFLOW_KEY).copied(),
        Some(ProtocolIoFrameStats { count: 5, bytes: 5 })
    );
}

/// Human-readable protocol I/O stats should use stable labels supplied by the
/// caller so UI and extension debug dumps can share accounting without sharing
/// perspective-specific wording.
#[test]
fn protocol_io_cumulative_stats_format_uses_labels_and_sorting() {
    let mut stats = ProtocolIoCumulativeStats::default();
    stats.uplink.insert(
        "message.hello".to_owned(),
        ProtocolIoFrameStats {
            count: 1,
            bytes: 50,
        },
    );
    stats.downlink.insert(
        "small.event".to_owned(),
        ProtocolIoFrameStats {
            count: 3,
            bytes: 512,
        },
    );
    stats.downlink.insert(
        "large.event".to_owned(),
        ProtocolIoFrameStats {
            count: 2,
            bytes: 12 * 1024,
        },
    );

    let formatted = format_protocol_io_cumulative_stats(
        "Example stats",
        "peer -> harness",
        "harness -> peer",
        "empty",
        &stats,
    );

    assert!(formatted.contains("peer -> harness: 50B in 1 frame(s)"));
    assert!(formatted.contains("  message.hello: 50B count=1"));
    assert!(formatted.contains("harness -> peer: 12K in 5 frame(s)"));
    assert!(!formatted.contains("bytes="));
    assert!(
        formatted.find("large.event").expect("large event line")
            < formatted.find("small.event").expect("small event line")
    );
}
