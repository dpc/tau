use std::io::Cursor;
use std::sync::Once;

use tau_proto::{
    AgentPromptSubmitted, ContentPart, ContextItem, ContextRole, Event, HarnessInputMessage,
    HarnessInputReader, HarnessOutputMessage, HarnessOutputWriter, MessageItem,
    ProviderResponseFinished, ProviderStopReason, ToolBackgroundResult, ToolCallItem, ToolResult,
};
use tracing_subscriber::EnvFilter;

use super::*;

fn message_variant(msg: &HarnessInputMessage) -> &'static str {
    match msg {
        HarnessInputMessage::Hello(_) => "Hello",
        HarnessInputMessage::Subscribe(_) => "Subscribe",
        HarnessInputMessage::Intercept(_) => "Intercept",
        HarnessInputMessage::Ready(_) => "Ready",
        HarnessInputMessage::Disconnect(_) => "Disconnect",
        HarnessInputMessage::ConfigError(_) => "ConfigError",
        HarnessInputMessage::Emit(_) => "Emit",
        HarnessInputMessage::InterceptReply(_) => "InterceptReply",
        HarnessInputMessage::GetAgentPromptCreated(_) => "GetAgentPromptCreated",
        HarnessInputMessage::GetRenderedSystemPrompt(_) => "GetRenderedSystemPrompt",
        HarnessInputMessage::GetRenderedPrompt(_) => "GetRenderedPrompt",
        HarnessInputMessage::GetRenderedToolDefinitions(_) => "GetRenderedToolDefinitions",
        HarnessInputMessage::ExtensionDataRequest(_) => "ExtensionDataRequest",
        HarnessInputMessage::ExternalAgentMessage(_) => "ExternalAgentMessage",
    }
}

/// Install a `tracing` subscriber for tests. Pick up `TAU_LOG` (same
/// env var the extension uses in production); default to off so a
/// plain `cargo test` is silent. Run a hanging test like
/// `TAU_LOG=trace cargo test -p tau-ext-std-notifications $name -- --nocapture`
/// to see every frame the extension received and every event the
/// test side read or skipped.
fn init_test_tracing() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let filter = EnvFilter::try_from_env("TAU_LOG").unwrap_or_else(|_| EnvFilter::new("off"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .with_target(true)
            .try_init();
    });
}

/// Test-side wrapper around [`HarnessInputReader`] that exposes an
/// `Event`-flavoured API (drops other messages).
struct EventReader<R> {
    inner: HarnessInputReader<R>,
}

impl<R: std::io::Read> EventReader<R> {
    fn new(inner: R) -> Self {
        init_test_tracing();
        Self {
            inner: HarnessInputReader::new(inner),
        }
    }

    fn read_event(&mut self) -> Result<Option<Event>, tau_proto::DecodeError> {
        loop {
            match self.inner.read_message()? {
                None => {
                    tracing::trace!(target: "tau::test", "EventReader: end of stream");
                    return Ok(None);
                }
                Some(HarnessInputMessage::Emit(emit)) => {
                    let event = *emit.event;
                    tracing::trace!(target: "tau::test", name = %event.name(), "EventReader: event");
                    return Ok(Some(event));
                }
                Some(msg) => {
                    tracing::trace!(target: "tau::test", kind = message_variant(&msg), "EventReader: skipping message");
                    continue;
                }
            }
        }
    }

    fn read_frame(&mut self) -> Result<Option<HarnessInputMessage>, tau_proto::DecodeError> {
        self.inner.read_message()
    }
}

/// Test-side wrapper around [`HarnessOutputWriter`] that accepts `Event`
/// directly.
struct EventWriter<W> {
    inner: HarnessOutputWriter<W>,
}

impl<W: std::io::Write> EventWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner: HarnessOutputWriter::new(inner),
        }
    }

    fn write_event(&mut self, event: &Event) -> Result<(), tau_proto::EncodeError> {
        self.inner
            .write_message(&HarnessOutputMessage::deliver(event.clone()))
    }

    fn write_frame(&mut self, frame: &HarnessOutputMessage) -> Result<(), tau_proto::EncodeError> {
        self.inner.write_message(frame)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Build a disconnect output message for tests that previously sent
/// `Event::LifecycleDisconnect`.
fn disconnect_frame(reason: Option<String>) -> HarnessOutputMessage {
    HarnessOutputMessage::Disconnect(tau_proto::Disconnect { reason })
}

/// Build a configure output message for tests that previously sent
/// `Event::LifecycleConfigure`.
fn configure_frame(config: tau_proto::CborValue) -> HarnessOutputMessage {
    HarnessOutputMessage::Configure(tau_proto::Configure {
        instance_name: None,
        config,
        state_dir: None,
        secrets: std::collections::BTreeMap::new(),
    })
}

fn default_idle_payload_template() -> String {
    r#"{"title":"Agent idle: {{host}}:{{cwd_basename}}","body":"Waiting for user input"}"#
        .to_owned()
}

fn idle_osc_config(delay_seconds: u64, agent_summary: bool) -> serde_json::Value {
    let value = if agent_summary {
        r#"{"summary":"{{turn.agent_summary}}"}"#.to_owned()
    } else {
        default_idle_payload_template()
    };
    serde_json::json!({
        "delay_seconds": delay_seconds,
        "agent_summary": agent_summary,
        "osc1337": {
            "key": TEXT_VAR_NAME,
            "value": value,
        },
    })
}

fn default_notifications_config_frame() -> HarnessOutputMessage {
    configure_frame(tau_proto::json_to_cbor(&serde_json::json!({
        "agent_start": [{ "osc1337": { "key": SOUND_VAR_NAME, "value": VALUE_AGENT_START } }],
        "agent_end": [{ "osc1337": { "key": SOUND_VAR_NAME, "value": VALUE_AGENT_END } }],
        "agent_idle": [{
            "osc1337": {
                "key": TEXT_VAR_NAME,
                "value": default_idle_payload_template(),
            },
        }],
    })))
}

fn immediate_idle_agent_summary_config_frame() -> HarnessOutputMessage {
    configure_frame(tau_proto::json_to_cbor(&serde_json::json!({
        "agent_end": [{ "osc1337": { "key": SOUND_VAR_NAME, "value": VALUE_AGENT_END } }],
        "agent_idle": [idle_osc_config(0, true)],
    })))
}

fn bell_mode_config_frame() -> HarnessOutputMessage {
    configure_frame(tau_proto::json_to_cbor(&serde_json::json!({
        "agent_start": [],
        "agent_end": [{ "bell": true }],
        "agent_idle": [],
    })))
}

fn assistant_finished_response(
    agent_prompt_id: &str,
    text: &str,
    originator: tau_proto::PromptOriginator,
) -> ProviderResponseFinished {
    assistant_finished_response_for_agent("main", agent_prompt_id, text, originator)
}

fn tool_background_placeholder(
    call_id: &str,
    originator: tau_proto::PromptOriginator,
) -> ToolResult {
    ToolResult {
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new("shell"),
        tool_type: tau_proto::ToolType::Function,
        result: tau_proto::CborValue::Text("running in background".into()),
        kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
        display: None,
        originator,
    }
}

fn tool_background_result(
    call_id: &str,
    originator: tau_proto::PromptOriginator,
) -> ToolBackgroundResult {
    ToolBackgroundResult {
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new("shell"),
        tool_type: tau_proto::ToolType::Function,
        result: tau_proto::CborValue::Text("done".into()),
        display: None,
        originator,
    }
}

fn user_prompt_submitted_for_agent(
    agent_id: &str,
    text: impl Into<String>,
    originator: tau_proto::PromptOriginator,
) -> Event {
    Event::AgentPromptSubmitted(AgentPromptSubmitted {
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        text: text.into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator,
        display_name: None,
        ctx_id: None,
    })
}

fn user_prompt_submitted(
    text: impl Into<String>,
    originator: tau_proto::PromptOriginator,
) -> Event {
    user_prompt_submitted_for_agent("main", text, originator)
}

fn session_agent_loaded(session_id: &str, agent_id: &str) -> Event {
    Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
        session_id: session_id.into(),
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        ephemeral: false,
    })
}
fn session_agent_unloaded(session_id: &str, agent_id: &str) -> Event {
    Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
        session_id: session_id.into(),
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
    })
}
fn agent_state(agent_id: &str, state: tau_proto::AgentRuntimeState) -> Event {
    Event::AgentState(tau_proto::AgentStateChanged {
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        state,
    })
}
fn assistant_finished_response_for_agent(
    agent_id: &str,
    agent_prompt_id: &str,
    text: &str,
    originator: tau_proto::PromptOriginator,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        agent_prompt_id: agent_prompt_id.into(),
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
            phase: None,
        })],
        stop_reason: ProviderStopReason::EndTurn,
        error: None,
        originator,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn tool_call_finished_response(
    agent_prompt_id: &str,
    tool_call: ToolCallItem,
    originator: tau_proto::PromptOriginator,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        agent_prompt_id: agent_prompt_id.into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(tool_call)],
        stop_reason: ProviderStopReason::ToolCalls,
        error: None,
        originator,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

/// Test marker for "we're past the lifecycle handshake". The hello /
/// subscribe / ready messages are directional protocol messages
/// (filtered out by `EventReader`), so reading from `EventReader`
/// after this returns will block until the extension emits an
/// actual `Event`.
/// test ever blocks here suspiciously, set `TAU_LOG=trace` and run
/// with `--nocapture` to see what `EventReader` is skipping vs.
/// surfacing.
fn drain_lifecycle<R: std::io::Read>(_reader: &mut EventReader<R>) {}

/// An explicit empty hook configuration should keep the extension completely
/// silent. This protects users who disable notifications from receiving stale
/// default side effects when prompts or responses arrive.
#[test]
fn empty_config_emits_no_notifications() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_start": [],
                "agent_end": [],
                "agent_idle": [],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&user_prompt_submitted(
            "hello",
            tau_proto::PromptOriginator::User,
        ))
        .expect("write");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_millis(1)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    assert!(reader.read_event().expect("read").is_none());
}

/// A normal user prompt followed by a final provider response should emit the
/// configured start and end OSC user variables in order. This is the core
/// sound-notification contract for interactive turns.
#[test]
fn emits_start_and_end_user_var_in_order() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&default_notifications_config_frame())
        .expect("write config");
    writer
        .write_event(&user_prompt_submitted(
            "hello",
            tau_proto::PromptOriginator::User,
        ))
        .expect("write");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write");
    // Explicit disconnect so the loop exits without waiting on
    // the (otherwise long) idle deadline triggered by the
    // `ProviderResponseFinished`.
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    let start = reader.read_event().expect("read").expect("start event");
    match start {
        Event::Osc1337SetUserVar(osc) => {
            assert_eq!(osc.name, SOUND_VAR_NAME);
            assert_eq!(osc.value, VALUE_AGENT_START);
        }
        other => panic!("expected Osc1337SetUserVar, got {other:?}"),
    }

    let end = reader.read_event().expect("read").expect("end event");
    match end {
        Event::Osc1337SetUserVar(osc) => {
            assert_eq!(osc.name, SOUND_VAR_NAME);
            assert_eq!(osc.value, VALUE_AGENT_END);
        }
        other => panic!("expected Osc1337SetUserVar, got {other:?}"),
    }
}

/// Subscribe-time catch-up re-delivers durable history as replay-marked
/// frames. Notifications are user-facing side effects, so replayed prompts
/// and responses must stay silent — only live frames may ring or chime.
#[test]
fn replay_marked_frames_emit_no_notifications() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&default_notifications_config_frame())
        .expect("write config");
    writer
        .write_frame(&HarnessOutputMessage::deliver_replay(
            tau_proto::UnixMicros::new(1),
            user_prompt_submitted("hello", tau_proto::PromptOriginator::User),
        ))
        .expect("write replayed prompt");
    writer
        .write_frame(&HarnessOutputMessage::deliver_replay(
            tau_proto::UnixMicros::new(2),
            Event::ProviderResponseFinished(assistant_finished_response(
                "sp-0",
                "done",
                tau_proto::PromptOriginator::User,
            )),
        ))
        .expect("write replayed response");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_millis(1)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    assert!(
        reader.read_event().expect("read").is_none(),
        "replayed history must not trigger notifications",
    );
}

/// Bell mode is an intentionally narrow transport: it only asks the
/// terminal to ring when the agent turn is complete. It must not emit
/// prompt-start bells, OSC user-var sound events, arm the idle text
/// notification, request an agent summary, or run an idle command.
#[test]
fn bell_mode_emits_only_completion_bell() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&bell_mode_config_frame())
        .expect("write config");
    writer
        .write_event(&user_prompt_submitted(
            "hello",
            tau_proto::PromptOriginator::User,
        ))
        .expect("write");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_millis(1)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    let end = reader.read_event().expect("read").expect("end event");
    assert!(matches!(end, Event::TermBell(_)));

    let extra = reader.read_event().expect("read");
    assert!(
        extra.is_none(),
        "bell mode emitted unexpected event: {extra:?}"
    );
}

/// Configured hook arrays must allow multiple actions and render
/// templates with the current agent id/name. This locks in the new
/// structured hook schema instead of the old single global mode.
#[test]
fn agent_start_hook_renders_multiple_configured_actions() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(&serde_json::json!({
            "agent_start": [
                { "bell": true },
                { "osc1337": { "key": "agent-{{agent.id}}", "value": "{{hook}}:{{agent.name}}" } },
            ],
            "agent_end": [],
            "agent_idle": [],
        }))))
        .expect("write config");
    writer
        .write_event(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            text: "hello".to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            display_name: Some("Friendly main".to_owned()),
            ctx_id: None,
        }))
        .expect("write prompt");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    let bell = reader.read_event().expect("read").expect("bell");
    assert!(matches!(bell, Event::TermBell(_)));
    let osc = reader.read_event().expect("read").expect("osc");
    match osc {
        Event::Osc1337SetUserVar(osc) => {
            assert_eq!(osc.name, "agent-main");
            assert_eq!(osc.value, "agent_start:Friendly main");
        }
        other => panic!("expected Osc1337SetUserVar, got {other:?}"),
    }
}

/// Display-name updates should survive a later prompt whose embedded display
/// name is blank, and templates should fall back to the stored name rather than
/// the raw agent id. This prevents transient empty prompt metadata from
/// degrading user-visible notifications.
#[test]
fn agent_start_hook_uses_display_name_set_with_id_fallback_for_blank_prompt_name() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_start": [
                    { "osc1337": { "key": "agent", "value": "{{agent.name}}" } },
                ],
                "agent_end": [],
                "agent_idle": [],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&Event::AgentDisplayNameSet(
            tau_proto::AgentDisplayNameSet {
                agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
                display_name: "Renamed main".to_owned(),
            },
        ))
        .expect("write name");
    writer
        .write_event(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            text: "hello".to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            display_name: Some("   ".to_owned()),
            ctx_id: None,
        }))
        .expect("write prompt");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let osc = reader.read_event().expect("read").expect("osc");
    match osc {
        Event::Osc1337SetUserVar(osc) => assert_eq!(osc.value, "Renamed main"),
        other => panic!("expected Osc1337SetUserVar, got {other:?}"),
    }
}

/// Mid-turn `ProviderResponseFinished` events (those carrying
/// pending tool calls) must NOT trigger the end-of-turn sound.
/// The agent emits one of those per LLM call when it's looping
/// through tool use; the *turn* only ends with a final
/// `ProviderResponseFinished` that has empty `tool_calls`.
#[test]
fn mid_turn_finish_with_tool_calls_does_not_emit_end_sound() {
    use tau_proto::CborValue;
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&default_notifications_config_frame())
        .expect("write config");
    writer
        .write_event(&user_prompt_submitted(
            "hello",
            tau_proto::PromptOriginator::User,
        ))
        .expect("write");
    // Mid-turn finish: text=None, tool_calls non-empty. No
    // notification should fire.
    writer
        .write_event(&Event::ProviderResponseFinished(
            tool_call_finished_response(
                "sp-0",
                ToolCallItem {
                    call_id: "call-1".into(),
                    name: tau_proto::ToolName::new("shell"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: CborValue::Null,
                },
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("write");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    // We expect the user-submit sound but NO end sound, because
    // the tool-bearing ProviderResponseFinished is mid-turn.
    let start = reader.read_event().expect("read").expect("start");
    match start {
        Event::Osc1337SetUserVar(osc) => {
            assert_eq!(osc.value, VALUE_AGENT_START);
        }
        other => panic!("expected start OSC, got {other:?}"),
    }
    let next = reader.read_event().expect("read");
    assert!(
        next.is_none(),
        "no further OSC events expected after mid-turn finish, got {next:?}",
    );
}

/// A final response that arrives while a user-originated background tool is
/// still active should defer the completion sound until that background result
/// lands. This avoids announcing completion while side work is still running.
#[test]
fn final_response_waits_for_background_tools_before_end_sound() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&default_notifications_config_frame())
        .expect("write config");
    writer
        .write_event(&user_prompt_submitted(
            "run slow thing",
            tau_proto::PromptOriginator::User,
        ))
        .expect("write");
    writer
        .write_event(&Event::ToolResult(tool_background_placeholder(
            "call-bg",
            tau_proto::PromptOriginator::User,
        )))
        .expect("write");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write");
    writer
        .write_event(&Event::ToolBackgroundResult(tool_background_result(
            "call-bg",
            tau_proto::PromptOriginator::User,
        )))
        .expect("write");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    let start = reader.read_event().expect("read").expect("start");
    let Event::Osc1337SetUserVar(osc) = start else {
        panic!("expected start OSC, got {start:?}");
    };
    assert_eq!(osc.value, VALUE_AGENT_START);

    let end = reader.read_event().expect("read").expect("end");
    let Event::Osc1337SetUserVar(osc) = end else {
        panic!("expected deferred end OSC, got {end:?}");
    };
    assert_eq!(osc.value, VALUE_AGENT_END);
    assert!(reader.read_event().expect("read eof").is_none());
}

/// Starting a second prompt while an earlier final response is blocked on
/// background work must not forget that background work. This is a regression
/// test for premature completion sounds across adjacent turns.
#[test]
fn new_prompt_does_not_forget_previous_background_tool() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&default_notifications_config_frame())
        .expect("write config");
    for (text, spid) in [("run slow thing", "sp-0"), ("next prompt", "sp-1")] {
        writer
            .write_event(&user_prompt_submitted(
                text,
                tau_proto::PromptOriginator::User,
            ))
            .expect("write");
        if spid == "sp-0" {
            writer
                .write_event(&Event::ToolResult(tool_background_placeholder(
                    "call-bg",
                    tau_proto::PromptOriginator::User,
                )))
                .expect("write");
        }
        writer
            .write_event(&Event::ProviderResponseFinished(
                assistant_finished_response(spid, "done", tau_proto::PromptOriginator::User),
            ))
            .expect("write");
    }
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    let mut values = Vec::new();
    while let Some(event) = reader.read_event().expect("read") {
        if let Event::Osc1337SetUserVar(osc) = event {
            values.push(osc.value);
        }
    }
    assert_eq!(
        values,
        vec![VALUE_AGENT_START, VALUE_AGENT_START],
        "end sound must wait until the old background tool completes",
    );
}

/// If a final response is waiting on a background tool that never reports
/// completion before disconnect, the completion sound must remain suppressed.
/// This prevents false-positive "done" notifications for abandoned work.
#[test]
fn final_response_without_background_completion_does_not_emit_end_sound() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&default_notifications_config_frame())
        .expect("write config");
    writer
        .write_event(&user_prompt_submitted(
            "run slow thing",
            tau_proto::PromptOriginator::User,
        ))
        .expect("write");
    writer
        .write_event(&Event::ToolResult(tool_background_placeholder(
            "call-bg",
            tau_proto::PromptOriginator::User,
        )))
        .expect("write");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    let start = reader.read_event().expect("read").expect("start");
    let Event::Osc1337SetUserVar(osc) = start else {
        panic!("expected start OSC, got {start:?}");
    };
    assert_eq!(osc.value, VALUE_AGENT_START);
    assert!(reader.read_event().expect("read eof").is_none());
}

/// After ProviderResponseFinished we should see the end-sound OSC
/// and then, after the configured idle window expires with no
/// further input, the text-notification OSC carrying a JSON
/// payload that mirrors `user-text-notification.sh`. By default
/// the extension does not ask the agent for a summary; it emits the
/// configured idle payload immediately when the idle window elapses.
#[test]
fn idle_timeout_defaults_to_static_notification() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&default_notifications_config_frame())
        .expect("write config");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle_and_summary_timeout(
        Cursor::new(input),
        &mut output,
        Duration::from_millis(50),
        Duration::from_millis(50),
    )
    .expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    // First the end-of-turn sound.
    let end = reader.read_event().expect("read").expect("end event");
    let Event::Osc1337SetUserVar(osc) = end else {
        panic!("expected end sound OSC");
    };
    assert_eq!(osc.name, SOUND_VAR_NAME);
    assert_eq!(osc.value, VALUE_AGENT_END);

    // Then, after the (short) idle window, the static fallback text
    // notification. There must be no intervening StartAgentRequest.
    let fallback = reader.read_event().expect("read").expect("fallback event");
    let Event::Osc1337SetUserVar(osc) = fallback else {
        panic!("expected fallback OSC, got {fallback:?}");
    };
    assert_eq!(osc.name, TEXT_VAR_NAME);
    let payload: serde_json::Value =
        serde_json::from_str(&osc.value).expect("fallback payload is JSON");
    assert!(
        payload["title"]
            .as_str()
            .expect("title is a string")
            .starts_with("Agent idle: "),
        "title should start with `Agent idle: `, got {:?}",
        payload["title"],
    );
    assert_eq!(payload["body"], "Waiting for user input");
}

/// The snake_case `agent_idle` key must continue to arm the per-agent idle
/// notification. This prevents regressions to the old kebab-case spelling or
/// accidentally wiring per-agent idleness only through `agent_idle_all`.
#[test]
fn agent_idle_snake_case_fires_for_individual_agent_idle() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_idle": [idle_osc_config(0, false)],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write response");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_millis(1)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let idle = reader.read_event().expect("read").expect("idle event");
    let Event::Osc1337SetUserVar(osc) = idle else {
        panic!("expected idle OSC, got {idle:?}");
    };
    assert_eq!(osc.name, TEXT_VAR_NAME);
}

/// The new `agent_idle_all` hook must fire only after every loaded agent in the
/// session has returned to idle. This catches implementations that merely copy
/// the per-agent idle behavior and fire as soon as one agent finishes.
#[test]
fn agent_idle_all_fires_when_every_loaded_session_agent_is_idle() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_idle_all": [{
                    "delay_seconds": 0,
                    "osc1337": { "key": TEXT_VAR_NAME, "value": "{{hook}}:{{agent.id}}" },
                }],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&session_agent_loaded("s1", "main"))
        .expect("load main");
    writer
        .write_event(&session_agent_loaded("s1", "other"))
        .expect("load other");
    writer
        .write_event(&user_prompt_submitted_for_agent(
            "main",
            "work 1",
            tau_proto::PromptOriginator::User,
        ))
        .expect("prompt main");
    writer
        .write_event(&agent_state("main", tau_proto::AgentRuntimeState::Running))
        .expect("main running");
    writer
        .write_event(&user_prompt_submitted_for_agent(
            "other",
            "work 2",
            tau_proto::PromptOriginator::User,
        ))
        .expect("prompt other");
    writer
        .write_event(&agent_state("other", tau_proto::AgentRuntimeState::Running))
        .expect("other running");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response_for_agent(
                "main",
                "sp-main",
                "main done",
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("finish main");
    writer
        .write_event(&agent_state("main", tau_proto::AgentRuntimeState::Idle))
        .expect("main idle");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response_for_agent(
                "other",
                "sp-other",
                "other done",
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("finish other");
    writer
        .write_event(&agent_state("other", tau_proto::AgentRuntimeState::Idle))
        .expect("other idle");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_millis(1)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let all_idle = reader.read_event().expect("read").expect("all idle event");
    let Event::Osc1337SetUserVar(osc) = all_idle else {
        panic!("expected all-idle OSC, got {all_idle:?}");
    };
    assert_eq!(osc.value, "agent_idle_all:other");
    assert!(reader.read_event().expect("read eof").is_none());
}

/// If any other agent in the same session is still busy, `agent_idle_all` must
/// remain silent even though the finishing agent itself is idle.
#[test]
fn agent_idle_all_does_not_fire_while_another_session_agent_is_busy() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_idle_all": [{
                    "delay_seconds": 0,
                    "osc1337": { "key": TEXT_VAR_NAME, "value": "all idle" },
                }],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&session_agent_loaded("s1", "main"))
        .expect("load main");
    writer
        .write_event(&session_agent_loaded("s1", "other"))
        .expect("load other");
    writer
        .write_event(&user_prompt_submitted_for_agent(
            "main",
            "work 1",
            tau_proto::PromptOriginator::User,
        ))
        .expect("prompt main");
    writer
        .write_event(&agent_state("main", tau_proto::AgentRuntimeState::Running))
        .expect("main running");
    writer
        .write_event(&user_prompt_submitted_for_agent(
            "other",
            "work 2",
            tau_proto::PromptOriginator::User,
        ))
        .expect("prompt other");
    writer
        .write_event(&agent_state("other", tau_proto::AgentRuntimeState::Running))
        .expect("other running");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response_for_agent(
                "main",
                "sp-main",
                "main done",
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("finish main");
    writer
        .write_event(&agent_state("main", tau_proto::AgentRuntimeState::Idle))
        .expect("main idle");
    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_millis(1)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    assert!(reader.read_event().expect("read").is_none());
}

/// Running work in one session must not clear an already armed all-idle timer
/// for another session.
#[test]
fn agent_idle_all_timer_survives_running_agent_in_other_session() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_idle_all": [{
                    "delay_seconds": 0,
                    "osc1337": { "key": TEXT_VAR_NAME, "value": "{{hook}}:{{agent.id}}" },
                }],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&session_agent_loaded("s1", "main"))
        .expect("load main");
    writer
        .write_event(&agent_state("main", tau_proto::AgentRuntimeState::Running))
        .expect("main running");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response_for_agent(
                "main",
                "sp-main",
                "done",
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("finish main");
    writer
        .write_event(&agent_state("main", tau_proto::AgentRuntimeState::Idle))
        .expect("main idle");
    writer
        .write_event(&session_agent_loaded("s2", "other"))
        .expect("load other");
    writer
        .write_event(&agent_state("other", tau_proto::AgentRuntimeState::Running))
        .expect("other running");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_millis(1)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let all_idle = reader.read_event().expect("read").expect("all idle event");
    let Event::Osc1337SetUserVar(osc) = all_idle else {
        panic!("expected all-idle OSC, got {all_idle:?}");
    };
    assert_eq!(osc.value, "agent_idle_all:main");
}

/// A provider prompt has no session id, so it must not clear an all-idle timer
/// already armed for another session.
#[test]
fn agent_idle_all_timer_survives_provider_prompt_in_other_session() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_idle_all": [{
                    "delay_seconds": 0,
                    "osc1337": { "key": TEXT_VAR_NAME, "value": "{{hook}}:{{agent.id}}" },
                }],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&session_agent_loaded("s1", "main"))
        .expect("load main");
    writer
        .write_event(&agent_state("main", tau_proto::AgentRuntimeState::Running))
        .expect("main running");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response_for_agent(
                "main",
                "sp-main",
                "done",
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("finish main");
    writer
        .write_event(&agent_state("main", tau_proto::AgentRuntimeState::Idle))
        .expect("main idle");
    writer
        .write_event(&Event::ProviderPromptSubmitted(
            tau_proto::ProviderPromptSubmitted {
                agent_prompt_id: "sp-other".into(),
                originator: tau_proto::PromptOriginator::User,
            },
        ))
        .expect("provider prompt");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_millis(1)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let all_idle = reader.read_event().expect("read").expect("all idle event");
    let Event::Osc1337SetUserVar(osc) = all_idle else {
        panic!("expected all-idle OSC, got {all_idle:?}");
    };
    assert_eq!(osc.value, "agent_idle_all:main");
}

/// An `agent_idle_all` hook with `agent_summary` spawns a side conversation.
/// That side prompt must not clear the pending all-idle hook before the
/// matching `StartAgentResult` arrives.
#[test]
fn agent_idle_all_summary_side_prompt_does_not_cancel_pending_notification() {
    use std::os::unix::net::UnixStream;

    let (test_side, ext_side) = UnixStream::pair().expect("pair");
    let ext_reader = ext_side.try_clone().expect("clone");
    let ext_writer = ext_side;
    let handle = thread::spawn(move || {
        run_with_idle_and_summary_timeout(
            ext_reader,
            ext_writer,
            Duration::from_millis(1),
            Duration::from_secs(5),
        )
        .expect("run");
    });

    let test_writer_stream = test_side.try_clone().expect("clone");
    let mut writer = EventWriter::new(test_writer_stream);
    let mut reader = EventReader::new(test_side);
    drain_lifecycle(&mut reader);

    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_idle_all": [{
                    "delay_seconds": 0,
                    "agent_summary": true,
                    "osc1337": { "key": TEXT_VAR_NAME, "value": "{{hook}}:{{turn.agent_summary}}" },
                }],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&session_agent_loaded("s1", "main"))
        .expect("load main");
    writer
        .write_event(&agent_state("main", tau_proto::AgentRuntimeState::Running))
        .expect("main running");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response_for_agent(
                "main",
                "sp-main",
                "done",
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("finish main");
    writer
        .write_event(&agent_state("main", tau_proto::AgentRuntimeState::Idle))
        .expect("main idle");
    writer.flush().expect("flush");

    let query = reader.read_event().expect("read").expect("summary query");
    let Event::StartAgentRequest(query) = query else {
        panic!("expected StartAgentRequest, got {query:?}");
    };

    writer
        .write_event(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
            query_id: query.query_id.clone(),
            agent_id: tau_proto::AgentId::parse("summary").expect("agent id"),
        }))
        .expect("accepted");
    writer
        .write_event(&session_agent_loaded("s1", "summary"))
        .expect("load summary");
    writer
        .write_event(&agent_state(
            "summary",
            tau_proto::AgentRuntimeState::Running,
        ))
        .expect("summary running");
    writer
        .write_event(&user_prompt_submitted_for_agent(
            "main",
            "summary side prompt",
            tau_proto::PromptOriginator::Extension {
                name: "std-notifications".into(),
                query_id: query.query_id.clone(),
            },
        ))
        .expect("side prompt");
    writer
        .write_event(&Event::StartAgentResult(tau_proto::StartAgentResult {
            query_id: query.query_id,
            text: "all done".into(),
            error: None,
        }))
        .expect("summary result");
    writer.flush().expect("flush");

    let text = reader.read_event().expect("read").expect("notification");
    let Event::Osc1337SetUserVar(osc) = text else {
        panic!("expected all-idle notification, got {text:?}");
    };
    assert_eq!(osc.value, "agent_idle_all:all done");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
    drop(writer);
    drop(reader);
    handle.join().expect("ext thread");
}

/// Unloading the last busy agent in a session should remove stale busy state
/// and allow `agent_idle_all` to fire for the now-idle remaining session.
#[test]
fn agent_idle_all_fires_when_busy_agent_unloads() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_idle_all": [{
                    "delay_seconds": 0,
                    "osc1337": { "key": TEXT_VAR_NAME, "value": "{{hook}}:{{agent.id}}" },
                }],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&session_agent_loaded("s1", "main"))
        .expect("load main");
    writer
        .write_event(&session_agent_loaded("s1", "other"))
        .expect("load other");
    writer
        .write_event(&agent_state("main", tau_proto::AgentRuntimeState::Idle))
        .expect("main idle");
    writer
        .write_event(&agent_state("other", tau_proto::AgentRuntimeState::Running))
        .expect("other running");
    writer
        .write_event(&session_agent_unloaded("s1", "other"))
        .expect("unload other");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_millis(1)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let all_idle = reader.read_event().expect("read").expect("all idle event");
    let Event::Osc1337SetUserVar(osc) = all_idle else {
        panic!("expected all-idle OSC, got {all_idle:?}");
    };
    assert_eq!(osc.value, "agent_idle_all:other");
}

/// An all-idle summary can be armed by unloading the busy agent that made the
/// session non-idle. The resulting side query must not name the unloaded agent
/// as an explicit parent, because the harness may reject parents that are no
/// longer loaded.
#[test]
fn agent_idle_all_summary_after_unload_uses_no_parent_agent() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_idle_all": [{
                    "delay_seconds": 0,
                    "agent_summary": true,
                    "osc1337": { "key": TEXT_VAR_NAME, "value": "{{turn.agent_summary}}" },
                }],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&session_agent_loaded("s1", "main"))
        .expect("load main");
    writer
        .write_event(&session_agent_loaded("s1", "other"))
        .expect("load other");
    writer
        .write_event(&agent_state("main", tau_proto::AgentRuntimeState::Idle))
        .expect("main idle");
    writer
        .write_event(&agent_state("other", tau_proto::AgentRuntimeState::Running))
        .expect("other running");
    writer
        .write_event(&session_agent_unloaded("s1", "other"))
        .expect("unload other");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle_and_summary_timeout(
        Cursor::new(input),
        &mut output,
        Duration::from_millis(1),
        Duration::from_millis(1),
    )
    .expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let query = reader.read_event().expect("read").expect("summary query");
    let Event::StartAgentRequest(query) = query else {
        panic!("expected StartAgentRequest, got {query:?}");
    };
    assert_eq!(query.parent_agent, None);
}

/// Kebab-case hook keys are intentionally rejected; notification configuration
/// keys are snake_case. This catches accidental reintroduction of the old
/// `agent-idle` spelling.
#[test]
fn kebab_case_idle_config_key_is_rejected() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent-idle": [idle_osc_config(0, false)],
            }),
        )))
        .expect("write config");
    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_millis(1)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    let err_frame = loop {
        let frame = reader
            .read_frame()
            .expect("read")
            .expect("config error frame");
        if matches!(frame, HarnessInputMessage::ConfigError(_)) {
            break frame;
        }
    };
    let HarnessInputMessage::ConfigError(e) = err_frame else {
        unreachable!()
    };
    assert!(e.message.contains("agent-idle"));
}

/// When `agent_summary` is enabled, idle window elapsing must
/// trigger an `StartAgentRequest` to the agent for a one-sentence summary.
/// When no result arrives within the summary timeout, the extension
/// then fires the configured idle payload so the user still gets
/// nudged.
#[test]
fn idle_timeout_requests_summary_when_enabled_then_falls_back() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&immediate_idle_agent_summary_config_frame())
        .expect("write");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle_and_summary_timeout(
        Cursor::new(input),
        &mut output,
        Duration::from_millis(50),
        Duration::from_millis(50),
    )
    .expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    let end = reader.read_event().expect("read").expect("end event");
    let Event::Osc1337SetUserVar(osc) = end else {
        panic!("expected end sound OSC");
    };
    assert_eq!(osc.name, SOUND_VAR_NAME);
    assert_eq!(osc.value, VALUE_AGENT_END);

    let query = reader
        .read_event()
        .expect("read")
        .expect("start-agent-request event");
    let Event::StartAgentRequest(query) = query else {
        panic!("expected StartAgentRequest, got {query:?}");
    };
    assert!(
        !query.query_id.is_empty(),
        "extension must mint a non-empty query_id",
    );
    assert!(query.instruction.contains("summarize") || query.instruction.contains("Summarize"));

    let fallback = reader.read_event().expect("read").expect("fallback event");
    let Event::Osc1337SetUserVar(osc) = fallback else {
        panic!("expected fallback OSC, got {fallback:?}");
    };
    assert_eq!(osc.name, TEXT_VAR_NAME);
    let payload: serde_json::Value =
        serde_json::from_str(&osc.value).expect("fallback payload is JSON");
    assert_eq!(payload["summary"], "");
}

/// When a matching `StartAgentResult` arrives before the
/// summary timeout, the text notification's body must be the
/// agent's summary text rather than the static fallback.
///
/// Coordinates with the running extension via a UnixStream pair:
/// the test thread reads each emitted event and only writes the
/// `StartAgentResult` *after* observing the `StartAgentRequest`,
/// so the result lands while the extension is in the
/// `WaitingSummary` state (not the earlier `WaitingIdle`).
#[test]
fn summary_result_populates_notification_template() {
    use std::os::unix::net::UnixStream;

    let (test_side, ext_side) = UnixStream::pair().expect("pair");
    let ext_reader = ext_side.try_clone().expect("clone");
    let ext_writer = ext_side;
    let handle = thread::spawn(move || {
        run_with_idle_and_summary_timeout(
            ext_reader,
            ext_writer,
            Duration::from_millis(50),
            Duration::from_secs(5),
        )
        .expect("run");
    });

    let test_writer_stream = test_side.try_clone().expect("clone");
    let mut writer = EventWriter::new(test_writer_stream);
    let mut reader = EventReader::new(test_side);

    drain_lifecycle(&mut reader);

    writer
        .write_frame(&immediate_idle_agent_summary_config_frame())
        .expect("write");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write");
    writer.flush().expect("flush");

    // end-of-turn sound, then the side-query.
    let _end = reader.read_event().expect("read").expect("end");
    let query = reader.read_event().expect("read").expect("query");
    let Event::StartAgentRequest(query) = query else {
        panic!("expected StartAgentRequest, got {query:?}");
    };

    writer
        .write_event(&Event::StartAgentResult(tau_proto::StartAgentResult {
            query_id: query.query_id.clone(),
            text: "  refactoring the harness state, awaiting next prompt  ".into(),
            error: None,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let text = reader.read_event().expect("read").expect("text");
    let Event::Osc1337SetUserVar(osc) = text else {
        panic!("expected populated text OSC, got {text:?}");
    };
    let payload: serde_json::Value = serde_json::from_str(&osc.value).expect("payload is JSON");
    assert_eq!(
        payload["summary"], "refactoring the harness state, awaiting next prompt",
        "summary template variable should be trimmed",
    );
    // Cleanly disconnect so the extension exits.
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");
    drop(writer);
    drop(reader);
    handle.join().expect("ext thread");
}

/// Idle summary result text is model-controlled, so it must be clamped before
/// templates can copy it into terminal payloads or command args. This keeps a
/// runaway side-agent response from producing unbounded notification output.
#[test]
fn long_summary_result_is_truncated_before_template_rendering() {
    use std::os::unix::net::UnixStream;

    let (test_side, ext_side) = UnixStream::pair().expect("pair");
    let ext_reader = ext_side.try_clone().expect("clone");
    let ext_writer = ext_side;
    let handle = thread::spawn(move || {
        run_with_idle_and_summary_timeout(
            ext_reader,
            ext_writer,
            Duration::from_millis(1),
            Duration::from_secs(5),
        )
        .expect("run");
    });

    let test_writer_stream = test_side.try_clone().expect("clone");
    let mut writer = EventWriter::new(test_writer_stream);
    let mut reader = EventReader::new(test_side);
    drain_lifecycle(&mut reader);

    writer
        .write_frame(&immediate_idle_agent_summary_config_frame())
        .expect("write");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write");
    writer.flush().expect("flush");

    let _end = reader.read_event().expect("read").expect("end");
    let query = reader.read_event().expect("read").expect("query");
    let Event::StartAgentRequest(query) = query else {
        panic!("expected StartAgentRequest, got {query:?}");
    };
    let long_summary = format!(
        "{}TAIL",
        "é".repeat((SUMMARY_TEXT_LIMIT_BYTES / "é".len()) + 10)
    );
    writer
        .write_event(&Event::StartAgentResult(tau_proto::StartAgentResult {
            query_id: query.query_id,
            text: long_summary.clone(),
            error: None,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let text = reader.read_event().expect("read").expect("text");
    let Event::Osc1337SetUserVar(osc) = text else {
        panic!("expected text OSC, got {text:?}");
    };
    let payload: serde_json::Value = serde_json::from_str(&osc.value).expect("payload is JSON");
    let summary = payload["summary"].as_str().expect("summary string");
    assert!(summary.ends_with("… [truncated]"));
    assert!(!summary.contains("TAIL"));
    assert!(summary.is_char_boundary(summary.len()));

    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");
    drop(writer);
    drop(reader);
    handle.join().expect("ext thread");
}

/// Trailing-edge typing pings (`UiPromptDraft`) arriving during
/// the `WaitingIdle` window must extend the deadline so the
/// idle notification doesn't fire while the user is still
/// composing. Without this, a slow typer would get the
/// "what were you working on?" notification mid-sentence.
#[test]
fn prompt_draft_extends_idle_deadline() {
    use std::os::unix::net::UnixStream;

    use tau_proto::UiPromptDraft;

    let (test_side, ext_side) = UnixStream::pair().expect("pair");
    let ext_reader = ext_side.try_clone().expect("clone");
    let ext_writer = ext_side;
    let handle = thread::spawn(move || {
        run_with_idle_and_summary_timeout(
            ext_reader,
            ext_writer,
            Duration::from_millis(200),
            Duration::from_millis(50),
        )
        .expect("run");
    });

    let test_writer_stream = test_side.try_clone().expect("clone");
    let mut writer = EventWriter::new(test_writer_stream);
    let mut reader = EventReader::new(test_side);

    drain_lifecycle(&mut reader);

    // Arm the idle deadline.
    writer
        .write_frame(&default_notifications_config_frame())
        .expect("write config");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write");
    writer.flush().expect("flush");

    // end-of-turn sound.
    let _end = reader.read_event().expect("read").expect("end");

    // Send several drafts ~100ms apart. Each one resets the
    // 200ms idle deadline; if the extension honors them
    // correctly no text notification should fire during this
    // window.
    for i in 0..5 {
        writer
            .write_event(&Event::UiPromptDraft(UiPromptDraft {
                session_id: "s1".into(),
                text: format!("partial draft {i}"),
            }))
            .expect("write");
        writer.flush().expect("flush");
        thread::sleep(Duration::from_millis(100));
    }

    // Stop typing. The next event the extension emits must be
    // the static text notification — and crucially, the elapsed
    // time before it fires must be at least the original 200ms
    // (because we kept resetting the deadline) plus the final
    // ~200ms wait.
    let started = Instant::now();
    let text = reader.read_event().expect("read").expect("text");
    let elapsed = started.elapsed();
    let Event::Osc1337SetUserVar(osc) = text else {
        panic!("expected text notification OSC, got {text:?}");
    };
    assert_eq!(osc.name, TEXT_VAR_NAME);
    // Without the deadline reset, the notification would have fired
    // at idle_duration (200ms) into the typing window — i.e.
    // ~300ms before we started timing — so the read here would
    // return ~immediately. With the reset, the most recent
    // draft (sent ~100ms ago) bumped the deadline ~200ms into
    // the future, so the read should block for roughly 100ms.
    // 30ms is a deliberately loose lower bound so CI jitter
    // doesn't flake the test.
    assert!(
        Duration::from_millis(30) <= elapsed,
        "notification fired too soon ({elapsed:?}); idle deadline wasn't reset",
    );

    // Disconnect to let the extension exit.
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");
    drop(writer);
    drop(reader);
    handle.join().expect("ext thread");
}

/// `UiPromptDraft` arriving while a side-query summary is
/// already in flight must NOT cancel it (we don't yet have
/// prompt cancellation). The summary completes normally and
/// surfaces as the notification body.
#[test]
fn prompt_draft_during_waiting_summary_does_not_cancel() {
    use std::os::unix::net::UnixStream;

    use tau_proto::UiPromptDraft;

    let (test_side, ext_side) = UnixStream::pair().expect("pair");
    let ext_reader = ext_side.try_clone().expect("clone");
    let ext_writer = ext_side;
    let handle = thread::spawn(move || {
        run_with_idle_and_summary_timeout(
            ext_reader,
            ext_writer,
            Duration::from_millis(50),
            Duration::from_secs(5),
        )
        .expect("run");
    });

    let test_writer_stream = test_side.try_clone().expect("clone");
    let mut writer = EventWriter::new(test_writer_stream);
    let mut reader = EventReader::new(test_side);

    drain_lifecycle(&mut reader);

    writer
        .write_frame(&immediate_idle_agent_summary_config_frame())
        .expect("write");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write");
    writer.flush().expect("flush");

    let _end = reader.read_event().expect("read").expect("end");
    let query = reader.read_event().expect("read").expect("query");
    let Event::StartAgentRequest(query) = query else {
        panic!("expected StartAgentRequest, got {query:?}");
    };

    // User starts typing AFTER we've dispatched the side query.
    // The summary must still be allowed to land.
    writer
        .write_event(&Event::UiPromptDraft(UiPromptDraft {
            session_id: "s1".into(),
            text: "typing while summary is in flight".into(),
        }))
        .expect("write");
    writer.flush().expect("flush");

    // Now deliver the summary result.
    writer
        .write_event(&Event::StartAgentResult(tau_proto::StartAgentResult {
            query_id: query.query_id,
            text: "the model's summary".into(),
            error: None,
        }))
        .expect("write");
    writer.flush().expect("flush");

    // Notification must expose the summary template variable, not be cancelled.
    let text = reader.read_event().expect("read").expect("text");
    let Event::Osc1337SetUserVar(osc) = text else {
        panic!("expected populated text OSC, got {text:?}");
    };
    let payload: serde_json::Value = serde_json::from_str(&osc.value).expect("payload is JSON");
    assert_eq!(payload["summary"], "the model's summary");

    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");
    drop(writer);
    drop(reader);
    handle.join().expect("ext thread");
}

/// When an idle hook command is configured, it must run alongside the
/// OSC notification after rendering configured template args into argv.
/// Uses a tiny shell command that writes the rendered args into a temp file
#[test]
fn idle_command_runs_with_rendered_template_args() {
    use std::os::unix::net::UnixStream;

    use tempfile::TempDir;

    let td = TempDir::new().expect("tempdir");
    let out_path = td.path().join("out.txt");

    // bash one-liner: writes rendered agent/turn args into the output file,
    // separated by `|||` so the test can assert each piece without stdin.
    let cmd = format!(
        "printf '%s|||%s' \"$1\" \"$2\" >> {dest}",
        dest = out_path.display(),
    );

    let (test_side, ext_side) = UnixStream::pair().expect("pair");
    let ext_reader = ext_side.try_clone().expect("clone");
    let ext_writer = ext_side;
    let handle = thread::spawn(move || {
        run_with_idle_and_summary_timeout(
            ext_reader,
            ext_writer,
            Duration::from_millis(50),
            Duration::from_millis(50),
        )
        .expect("run");
    });

    let test_writer_stream = test_side.try_clone().expect("clone");
    let mut writer = EventWriter::new(test_writer_stream);
    let mut reader = EventReader::new(test_side);

    drain_lifecycle(&mut reader);

    // Configure the extension with the test command.
    let mut idle_hook = idle_osc_config(0, false);
    idle_hook["command"] = serde_json::json!([
        "bash",
        "-c",
        cmd,
        "_marker",
        "{{agent.id}}",
        "{{turn.agent_response}}"
    ]);
    let cfg = tau_proto::json_to_cbor(&serde_json::json!({
        "agent_end": [{ "osc1337": { "key": SOUND_VAR_NAME, "value": VALUE_AGENT_END } }],
        "agent_idle": [idle_hook],
    }));
    writer.write_frame(&configure_frame(cfg)).expect("write");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write");
    writer.flush().expect("flush");

    // Drain: end-sound, static fallback OSC. We don't care about
    // the exact contents — what we want is the command to run as a
    // side effect.
    let _ = reader.read_event().expect("read").expect("end");
    let _ = reader.read_event().expect("read").expect("fallback");

    // The command runs in a detached thread; poll the output
    // file briefly until it appears (max 2s).
    let started = Instant::now();
    loop {
        if out_path.exists()
            && let Ok(contents) = std::fs::read_to_string(&out_path)
            && contents.contains("|||")
        {
            let mut parts = contents.splitn(2, "|||");
            let agent_id = parts.next().expect("agent id field");
            let response = parts.next().expect("response field");
            assert_eq!(agent_id, "main");
            assert_eq!(response, "done");
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "idle hook command never produced output",
        );
        thread::sleep(Duration::from_millis(20));
    }

    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");
    drop(writer);
    drop(reader);
    handle.join().expect("ext thread");
}

/// A bogus `config` value (one that doesn't match `ExtConfig`)
/// must trigger a `LifecycleConfigError` carrying a human-readable
/// message, so the harness can surface it to the user.
#[test]
fn invalid_config_emits_lifecycle_config_error() {
    // Build a config CBOR value that doesn't match ExtConfig:
    // an unknown field, which `deny_unknown_fields` rejects.
    let bad_config = tau_proto::json_to_cbor(&serde_json::json!({
        "totally_unknown_field": 7,
    }));

    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(bad_config))
        .expect("write");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    // Skip startup messages (hello, subscribe, ready) until we reach
    // the ConfigError reply.
    let err_frame = loop {
        let frame = reader
            .read_frame()
            .expect("read")
            .expect("config error frame");
        if matches!(frame, HarnessInputMessage::ConfigError(_)) {
            break frame;
        }
    };
    let HarnessInputMessage::ConfigError(e) = err_frame else {
        unreachable!()
    };
    assert!(!e.message.is_empty(), "config error must carry a message");
}

/// Bad hook templates must be rejected during Configure instead of
/// crashing the extension later when the hook fires.
#[test]
fn invalid_hook_template_emits_config_error() {
    let bad_config = tau_proto::json_to_cbor(&serde_json::json!({
        "agent_start": [{ "osc1337": { "key": "ok", "value": "{{missing}}" } }],
    }));

    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(bad_config))
        .expect("write");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    let err_frame = loop {
        let frame = reader
            .read_frame()
            .expect("read")
            .expect("config error frame");
        if matches!(frame, HarnessInputMessage::ConfigError(_)) {
            break frame;
        }
    };
    let HarnessInputMessage::ConfigError(e) = err_frame else {
        unreachable!()
    };
    assert!(e.message.contains("missing"));
}

/// OSC 1337 user-var key validation is the last defense before names are
/// embedded into terminal escape sequences. Cover every documented rejection
/// class directly so integration tests do not have to exercise each one.
#[test]
fn osc1337_name_validator_covers_documented_constraints() {
    let valid_128 = "a".repeat(128);
    assert!(validate_osc1337_name(&valid_128).is_ok());

    for (name, expected) in [
        ("".to_owned(), "must not be empty"),
        ("a".repeat(129), "at most 128"),
        ("bad=key".to_owned(), "must not contain"),
        ("bad\u{1b}key".to_owned(), "control"),
        ("bad\u{7}key".to_owned(), "control"),
        ("snowman-☃".to_owned(), "printable ASCII"),
    ] {
        let err = validate_osc1337_name(&name).expect_err("invalid key");
        assert!(
            err.contains(expected),
            "expected error containing {expected:?}, got {err:?}",
        );
        assert!(
            !err.contains('\u{1b}') && !err.contains('\u{7}'),
            "validator errors must not echo raw controls: {err:?}",
        );
    }
}

/// A statically invalid OSC user-var key must fail configuration validation
/// before any notification fires. The UI also rejects malformed OSC names, and
/// this extension should fail closed before invalid keys reach that boundary.
#[test]
fn invalid_static_osc1337_key_emits_config_error() {
    let bad_config = tau_proto::json_to_cbor(&serde_json::json!({
        "agent_start": [{ "osc1337": { "key": "bad=key", "value": "value" } }],
    }));

    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(bad_config))
        .expect("write");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    let err_frame = loop {
        let frame = reader
            .read_frame()
            .expect("read")
            .expect("config error frame");
        if matches!(frame, HarnessInputMessage::ConfigError(_)) {
            break frame;
        }
    };
    let HarnessInputMessage::ConfigError(e) = err_frame else {
        unreachable!()
    };
    assert!(e.message.contains("osc1337.key"));
    assert!(e.message.contains("invalid"));
}

/// A template can render to a valid key during configuration but an invalid key
/// at runtime because it uses untrusted event data. The extension must skip the
/// OSC emission rather than sending a malformed terminal escape.
#[test]
fn runtime_invalid_osc1337_key_is_skipped() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_start": [
                    { "osc1337": { "key": "{{agent.name}}", "value": "value" } },
                ],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            text: "hello".to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            display_name: Some("bad=key".to_owned()),
            ctx_id: None,
        }))
        .expect("write prompt");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    assert!(
        reader.read_event().expect("read").is_none(),
        "runtime-invalid OSC key should skip emission",
    );
}

/// Idle summary requests run in a side conversation, so the emitted instruction
/// must carry bounded recent-turn context explicitly. This ensures the summary
/// can describe the visible user prompt and assistant response it is notifying
/// about.
#[test]
fn idle_summary_request_contains_recent_turn_context() {
    let long_prompt = format!(
        "please refactor the notification extension {} PROMPT_TAIL",
        "é".repeat((SUMMARY_CONTEXT_LIMIT_BYTES / "é".len()) + 10),
    );
    let long_response = format!(
        "updated docs and validation {} RESPONSE_TAIL",
        "é".repeat((SUMMARY_CONTEXT_LIMIT_BYTES / "é".len()) + 10),
    );
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_idle": [idle_osc_config(0, true)],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&user_prompt_submitted(
            long_prompt,
            tau_proto::PromptOriginator::User,
        ))
        .expect("write prompt");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", &long_response, tau_proto::PromptOriginator::User),
        ))
        .expect("write response");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle_and_summary_timeout(
        Cursor::new(input),
        &mut output,
        Duration::from_millis(1),
        Duration::from_millis(1),
    )
    .expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let query = reader.read_event().expect("read").expect("summary query");
    let Event::StartAgentRequest(query) = query else {
        panic!("expected StartAgentRequest, got {query:?}");
    };
    assert_eq!(query.parent_agent, None);
    assert!(query.instruction.contains("User prompt:"));
    assert!(
        query
            .instruction
            .contains("please refactor the notification extension")
    );
    assert!(!query.instruction.contains("PROMPT_TAIL"));
    assert!(query.instruction.contains("Assistant response:"));
    assert!(query.instruction.contains("updated docs and validation"));
    assert!(!query.instruction.contains("RESPONSE_TAIL"));
    assert!(query.instruction.contains("… [truncated]"));
    assert!(query.instruction.is_char_boundary(query.instruction.len()));
}

/// Applying a new config while an idle deadline is pending must clear
/// old pending hook indexes so later drafts or timeouts cannot index
/// into the replacement config and panic.
#[test]
fn config_reload_clears_pending_idle_hooks() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&default_notifications_config_frame())
        .expect("write config");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write response");
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_idle": [],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&Event::UiPromptDraft(tau_proto::UiPromptDraft {
            session_id: "session".into(),
            text: "still typing".to_owned(),
        }))
        .expect("write draft");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let end = reader.read_event().expect("read").expect("end event");
    match end {
        Event::Osc1337SetUserVar(osc) => assert_eq!(osc.value, VALUE_AGENT_END),
        other => panic!("expected Osc1337SetUserVar, got {other:?}"),
    }
    assert!(reader.read_event().expect("read").is_none());
}
/// A user prompt arriving inside the idle window must cancel the
/// pending text notification — only the end-sound OSC should be
/// emitted before stdin closes.
#[test]
fn user_prompt_during_idle_window_cancels_text_notification() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&default_notifications_config_frame())
        .expect("write config");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write");
    writer
        .write_event(&user_prompt_submitted(
            "another question",
            tau_proto::PromptOriginator::User,
        ))
        .expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    // Long idle window — if the cancel works, we never wait.
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    let end = reader.read_event().expect("read").expect("end event");
    let Event::Osc1337SetUserVar(osc) = end else {
        panic!("expected end sound OSC");
    };
    assert_eq!(osc.value, VALUE_AGENT_END);

    // The follow-up user prompt should emit the user-submit
    // sound and cancel the idle deadline.
    let next = reader
        .read_event()
        .expect("read")
        .expect("user-submit event");
    let Event::Osc1337SetUserVar(osc) = next else {
        panic!("expected user-submit sound OSC");
    };
    assert_eq!(osc.value, VALUE_AGENT_START);

    assert!(reader.read_event().expect("read eof").is_none());
}

/// Sub-agent (`PromptOriginator::Extension`) prompt + response
/// activity must not perturb the notifications extension. A
/// `agent_start` flow runs an entire side conversation between the
/// user's prompt and the main agent's final response — none of those
/// side events should clear the idle timer or fire the end-of-turn
/// chime, since the user isn't seeing them.
#[test]
fn sub_agent_prompts_and_responses_are_ignored() {
    use tau_proto::{CborValue, ProviderPromptSubmitted, ToolName};
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);

    writer
        .write_frame(&default_notifications_config_frame())
        .expect("write config");
    // User starts a turn → expect agent_start sound.
    writer
        .write_event(&user_prompt_submitted(
            "delegate something",
            tau_proto::PromptOriginator::User,
        ))
        .expect("write");

    // Main agent emits an agent_start tool_call (mid-turn).
    writer
        .write_event(&Event::ProviderResponseFinished(
            tool_call_finished_response(
                "sp-main",
                ToolCallItem {
                    call_id: "delegate-call".into(),
                    name: ToolName::new("agent_start"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: CborValue::Null,
                },
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("write");

    // Sub-agent activity — must not clear idle, fire chimes, or
    // touch `waiting_for_final_response`.
    writer
        .write_event(&Event::ProviderPromptSubmitted(ProviderPromptSubmitted {
            agent_prompt_id: "sp-side".into(),
            originator: tau_proto::PromptOriginator::Extension {
                name: "core-subagents".into(),
                query_id: "q1".into(),
            },
        }))
        .expect("write");
    writer
        .write_event(&user_prompt_submitted(
            "side instruction",
            tau_proto::PromptOriginator::Extension {
                name: "core-subagents".into(),
                query_id: "q1".into(),
            },
        ))
        .expect("write");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response(
                "sp-side",
                "delegated answer",
                tau_proto::PromptOriginator::Extension {
                    name: "core-subagents".into(),
                    query_id: "q1".into(),
                },
            ),
        ))
        .expect("write");

    // Main agent finally finishes the user's turn → end sound.
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-main", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    // Expect exactly two OSC events: agent_start (user prompt) and
    // agent_end (main agent's final response). Sub-agent activity
    // between them must NOT produce any sounds.
    let start = reader.read_event().expect("read").expect("start");
    let Event::Osc1337SetUserVar(osc) = start else {
        panic!("expected agent_start OSC, got {start:?}");
    };
    assert_eq!(osc.value, VALUE_AGENT_START);

    let end = reader.read_event().expect("read").expect("end");
    let Event::Osc1337SetUserVar(osc) = end else {
        panic!("expected agent_end OSC, got {end:?}");
    };
    assert_eq!(osc.value, VALUE_AGENT_END);

    assert!(
        reader.read_event().expect("read eof").is_none(),
        "no further OSC events expected — sub-agent activity must be silent",
    );
}

/// Duplicate `AgentPromptSubmitted` events during one visible turn should emit
/// only one start sound. This catches repeated prompt-delivery events from
/// producing noisy duplicate notifications.
#[test]
fn duplicate_agent_prompt_submitted_during_same_turn_emits_one_start_sound() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&default_notifications_config_frame())
        .expect("write config");
    writer
        .write_event(&user_prompt_submitted(
            "hello",
            tau_proto::PromptOriginator::User,
        ))
        .expect("write");
    writer
        .write_event(&user_prompt_submitted(
            "internal replay",
            tau_proto::PromptOriginator::User,
        ))
        .expect("write");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    run_with_idle(Cursor::new(input), &mut output, Duration::from_secs(3600)).expect("run");

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);

    let first = reader.read_event().expect("read").expect("first OSC");
    let Event::Osc1337SetUserVar(osc) = first else {
        panic!("expected first sound OSC");
    };
    assert_eq!(osc.value, VALUE_AGENT_START);

    let second = reader.read_event().expect("read").expect("second OSC");
    let Event::Osc1337SetUserVar(osc) = second else {
        panic!("expected second sound OSC");
    };
    assert_eq!(osc.value, VALUE_AGENT_END);

    assert!(reader.read_event().expect("read eof").is_none());
}
