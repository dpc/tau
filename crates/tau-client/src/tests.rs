use std::io::{Cursor, Write};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use tau_proto::{
    AgentPromptSubmitted, CborValue, Configure, Event, EventSelector, HarnessInputMessage,
    HarnessInputReader, HarnessNotice, HarnessOutputMessage, HarnessOutputWriter, InterceptAction,
    InterceptRequest, InterceptionPriority, NoticeLevel, PromptMessageClass, PromptOriginator,
    ToolName, ToolSpec, ToolStarted, ToolType, UnixMicros,
};

use super::*;

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

struct ReplayExtension;

impl TauExtension for ReplayExtension {
    type State = Counts;

    fn name(&self) -> &'static str {
        "replay"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .on::<HarnessNotice>(|cx| {
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

fn tool_started(name: &str) -> HarnessOutputMessage {
    HarnessOutputMessage::deliver(Event::ToolStarted(ToolStarted {
        call_id: format!("call-{name}").into(),
        tool_name: ToolName::new(name),
        arguments: CborValue::Map(Vec::new()),
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        originator: PromptOriginator::User,
    }))
}

fn test_prompt(text: &str) -> AgentPromptSubmitted {
    AgentPromptSubmitted {
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        text: text.to_owned(),
        message_class: PromptMessageClass::User,
        originator: PromptOriginator::User,
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

/// Ensures replay-aware handlers see metadata while live-only handlers skip
/// replay.
#[test]
fn replay_and_live_handlers_preserve_replay_metadata() {
    let replay_at = UnixMicros::new(10);
    let live_at = UnixMicros::new(11);
    let (state, _) = run_messages(
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
        subscribe.selectors,
        [EventSelector::Exact(tau_proto::EventName::TOOL_STARTED)]
    );
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
            if sub.selectors == [EventSelector::Prefix("harness.".to_owned())]
    ));
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
    assert!(matches!(&frames[1], HarnessInputMessage::Subscribe(sub) if sub.selectors.is_empty()));
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
