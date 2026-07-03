use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};

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
