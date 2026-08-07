use std::collections as path_std_collections;
use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex, Once};

use tau_proto::{
    AgentPromptSubmitted, ContentPart, ContextItem, ContextRole, Event, HarnessInputMessage,
    HarnessInputReader, HarnessOutputMessage, HarnessOutputWriter, MessageItem,
    ProviderResponseFinished, ProviderStopReason, ToolBackgroundResult, ToolCallItem, ToolResult,
};
use tracing_subscriber::EnvFilter;

use super::*;

/// Shared byte sink used by tests that run tau-client's writer thread.
#[derive(Clone, Default)]
struct SharedWriter {
    /// Shared byte buffer written by the tau-client writer thread.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedWriter {
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

fn run_with_idle_output(input: Vec<u8>, idle_duration: Duration) -> Vec<u8> {
    let writer = SharedWriter::default();
    let output = writer.clone();
    run_with_idle(
        Cursor::new(with_initial_configure(input)),
        writer,
        idle_duration,
    )
    .expect("run");
    output.bytes()
}

fn run_with_idle_and_summary_output(
    input: Vec<u8>,
    idle_duration: Duration,
    summary_timeout: Duration,
) -> Vec<u8> {
    let writer = SharedWriter::default();
    let output = writer.clone();
    run_with_idle_and_summary_timeout(
        Cursor::new(with_initial_configure(input)),
        writer,
        idle_duration,
        summary_timeout,
    )
    .expect("run");
    output.bytes()
}

fn with_initial_configure(input: Vec<u8>) -> Vec<u8> {
    let mut framed = Vec::new();
    let mut writer = EventWriter::new(&mut framed);
    writer
        .write_frame(&configure_frame(tau_proto::CborValue::Map(Vec::new())))
        .expect("write initial configure");
    writer.flush().expect("flush initial configure");
    framed.extend(input);
    framed
}

/// Corrupted protocol input should surface as a fatal decode/read error under
/// tau-client instead of being silently treated as EOF and draining timers.
#[test]
fn malformed_protocol_input_returns_error() {
    let writer = SharedWriter::default();
    let error = run_with_idle(
        Cursor::new(with_initial_configure(vec![0x9f])),
        writer,
        Duration::from_secs(3600),
    )
    .expect_err("malformed input should fail");

    assert!(!error.to_string().is_empty());
}

/// Startup must keep the legacy exact subscription set without broadening to a
/// prefix selector, because notifications are visible side effects and replay
/// catch-up volume is a security/resource boundary.
#[test]
fn startup_uses_exact_notification_subscriptions() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_secs(3600));
    let mut reader = HarnessInputReader::new(Cursor::new(output));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("read frame") {
        frames.push(frame);
    }

    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    let subscribe = frames
        .iter()
        .find_map(|frame| match frame {
            HarnessInputMessage::Subscribe(subscribe) => Some(subscribe),
            _ => None,
        })
        .expect("subscribe frame");
    // Intentionally duplicate the production subscription contract here so an
    // accidental event-set change cannot update the test oracle too.
    let expected = vec![
        tau_proto::EventSelector::Exact(tau_proto::EventName::PROVIDER_PROMPT_SUBMITTED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::PROVIDER_RESPONSE_FINISHED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_STARTED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_TERMINATED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_STARTED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_DISPLAY_NAME_SET),
        tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_STATE),
        tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_LOADED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_UNLOADED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::SESSION_SHUTDOWN),
        tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_START_ACCEPTED),
        tau_proto::EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT),
        tau_proto::EventSelector::Exact(tau_proto::EventName::TOOL_RESULT),
        tau_proto::EventSelector::Exact(tau_proto::EventName::TOOL_ERROR),
        tau_proto::EventSelector::Exact(tau_proto::EventName::PROVIDER_TOOL_RESULT),
        tau_proto::EventSelector::Exact(tau_proto::EventName::PROVIDER_TOOL_ERROR),
        tau_proto::EventSelector::Exact(tau_proto::EventName::TOOL_BACKGROUND_RESULT),
        tau_proto::EventSelector::Exact(tau_proto::EventName::TOOL_BACKGROUND_ERROR),
        tau_proto::EventSelector::Exact(tau_proto::EventName::AGENT_START_RESULT),
    ];
    assert_eq!(subscribe.live_selectors, expected);
    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
    );
}

/// Distinct process generations must not reuse a live idle-summary correlation,
/// even when both start at sequence zero and their requests overlap.
#[test]
fn idle_summary_query_ids_are_namespaced_by_process_generation() {
    let mut first = SummaryQueryIds {
        run_nonce: 1,
        next: 0,
    };
    let mut respawned = SummaryQueryIds {
        run_nonce: 2,
        next: 0,
    };

    assert_ne!(first.next_id(), respawned.next_id());
    assert_ne!(first.next_id(), respawned.next_id());
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
                    tracing::trace!(
                        target: "tau::test",
                        kind = %tau_client::harness_input_message_name(&msg),
                        "EventReader: skipping message"
                    );
                    continue;
                }
            }
        }
    }

    fn read_frame(&mut self) -> Result<Option<HarnessInputMessage>, tau_proto::DecodeError> {
        self.inner.read_message()
    }

    fn read_emit(&mut self) -> Result<Option<tau_proto::Emit>, tau_proto::DecodeError> {
        loop {
            match self.inner.read_message()? {
                Some(HarnessInputMessage::Emit(emit)) => return Ok(Some(emit)),
                Some(_) => {}
                None => return Ok(None),
            }
        }
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
        tool_prefix: None,
        instance_name: tau_proto::ExtensionName::parse("test-extension")
            .expect("test extension name must satisfy the identifier grammar"),
        config,
        state_dir: None,
        secrets: path_std_collections::BTreeMap::new(),
        settings_files: Default::default(),
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
        presentation: Default::default(),
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new("shell"),
        tool_type: tau_proto::ToolType::Function,
        result: tau_proto::CborValue::Text("running in background".into()),
        provider_content: Vec::new(),
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
        inference_activation: false,
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        text: text.into(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator,
        submission_source: Default::default(),
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
        agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
            .expect("test identifier must be valid"),

        session_id: session_id
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        ephemeral: false,
    })
}
fn session_agent_unloaded(session_id: &str, agent_id: &str) -> Event {
    Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
        session_id: session_id
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
    })
}
fn session_shutdown(session_id: &str) -> Event {
    Event::SessionShutdown(tau_proto::SessionShutdown {
        session_id: session_id
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
    })
}
fn agent_state(agent_id: &str, state: tau_proto::AgentRuntimeState) -> Event {
    Event::AgentState(tau_proto::AgentStateChanged {
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        state,
    })
}

fn agent_prompt_terminated(agent_id: &str, agent_prompt_id: &str) -> Event {
    Event::AgentPromptTerminated(tau_proto::AgentPromptTerminated {
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        agent_prompt_id: agent_prompt_id
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        reason: tau_proto::AgentPromptTerminationReason::Canceled,
        originator: tau_proto::PromptOriginator::User,
    })
}

fn agent_prompt_started_for_agent(agent_id: &str, agent_prompt_id: &str) -> Event {
    Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        model_params: Some(tau_proto::ModelParams::default()),
        outer_turn_id: None,

        agent_prompt_id: agent_prompt_id
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        model: "test/model".parse().expect("model id"),
        operation: tau_proto::PromptOperation::Inference,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    })
}

fn assistant_finished_response_for_agent(
    agent_id: &str,
    agent_prompt_id: &str,
    text: &str,
    originator: tau_proto::PromptOriginator,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: agent_prompt_id
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
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
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: agent_prompt_id
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(tool_call)],
        stop_reason: ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
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

    let output = run_with_idle_output(input, Duration::from_millis(1));

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

    let output = run_with_idle_output(input, Duration::from_secs(3600));

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

    let output = run_with_idle_output(input, Duration::from_millis(1));

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

    let output = run_with_idle_output(input, Duration::from_millis(1));

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

/// Terminal-output hooks preserve their existing caller-selected durable wire
/// bit; the harness classifies the committed events as no-store live side
/// effects independently of this metadata.
#[test]
fn terminal_output_hooks_preserve_non_transient_emit_metadata() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_start": [
                    { "bell": true },
                    { "osc1337": { "key": "status", "value": "started" } },
                ],
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
        .expect("write prompt");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_secs(3600));
    let mut reader = EventReader::new(Cursor::new(output));
    for expected_name in [
        tau_proto::EventName::TERM_BELL,
        tau_proto::EventName::TERM_OSC1337_SET_USER_VAR,
    ] {
        let emit = reader.read_emit().expect("read").expect("terminal emit");
        assert!(
            emit.persist,
            "{expected_name} must preserve the existing persist=true wire bit"
        );
        assert_eq!(emit.event.name(), expected_name);
    }
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
            inference_activation: false,
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            text: "hello".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: Some("Friendly main".to_owned()),
            ctx_id: None,
        }))
        .expect("write prompt");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_secs(3600));

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
            inference_activation: false,
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            text: "hello".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: Some("   ".to_owned()),
            ctx_id: None,
        }))
        .expect("write prompt");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_secs(3600));

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
                    raw_arguments_json: None,
                    responses_envelope: None,
                },
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("write");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_secs(3600));

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

    let output = run_with_idle_output(input, Duration::from_secs(3600));

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

    let output = run_with_idle_output(input, Duration::from_secs(3600));

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

/// A background tool in one agent must not block completion notifications for a
/// different loaded agent.
#[test]
fn background_tool_deferral_is_scoped_to_owning_agent() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_start": [],
                "agent_end": [{
                    "osc1337": { "key": SOUND_VAR_NAME, "value": "end:{{agent.id}}" },
                }],
                "agent_idle": [],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&user_prompt_submitted_for_agent(
            "main",
            "slow",
            tau_proto::PromptOriginator::User,
        ))
        .expect("main prompt");
    writer
        .write_event(&Event::ProviderResponseFinished(
            tool_call_finished_response(
                "sp-main-tools",
                ToolCallItem {
                    call_id: "call-main-bg".into(),
                    name: tau_proto::ToolName::new("shell"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: tau_proto::CborValue::Null,
                    raw_arguments_json: None,
                    responses_envelope: None,
                },
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("main tool call");
    writer
        .write_event(&user_prompt_submitted_for_agent(
            "other",
            "quick",
            tau_proto::PromptOriginator::User,
        ))
        .expect("other prompt");
    writer
        .write_event(&Event::ProviderToolResult(tool_background_placeholder(
            "call-main-bg",
            tau_proto::PromptOriginator::User,
        )))
        .expect("main bg placeholder");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response_for_agent(
                "main",
                "sp-main",
                "main done",
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("main final");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response_for_agent(
                "other",
                "sp-other",
                "other done",
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("other final");
    writer
        .write_event(&Event::ToolBackgroundResult(tool_background_result(
            "call-main-bg",
            tau_proto::PromptOriginator::User,
        )))
        .expect("main bg result");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_secs(3600));

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let other_end = reader.read_event().expect("read").expect("other end");
    let Event::Osc1337SetUserVar(osc) = other_end else {
        panic!("expected other end OSC, got {other_end:?}");
    };
    assert_eq!(osc.value, "end:other");
    let main_end = reader.read_event().expect("read").expect("main end");
    let Event::Osc1337SetUserVar(osc) = main_end else {
        panic!("expected main end OSC, got {main_end:?}");
    };
    assert_eq!(osc.value, "end:main");
    assert!(reader.read_event().expect("read eof").is_none());
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

    let output = run_with_idle_output(input, Duration::from_secs(3600));

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
    let output = run_with_idle_and_summary_output(
        input,
        Duration::from_millis(50),
        Duration::from_millis(50),
    );

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

    let output = run_with_idle_output(input, Duration::from_millis(1));

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

    let output = run_with_idle_output(input, Duration::from_millis(1));

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

    let output = run_with_idle_output(input, Duration::from_millis(1));

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

    let output = run_with_idle_output(input, Duration::from_millis(1));

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
                agent_prompt_id: "sp-other"
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
                originator: tau_proto::PromptOriginator::User,
            },
        ))
        .expect("provider prompt");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_millis(1));

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let all_idle = reader.read_event().expect("read").expect("all idle event");
    let Event::Osc1337SetUserVar(osc) = all_idle else {
        panic!("expected all-idle OSC, got {all_idle:?}");
    };
    assert_eq!(osc.value, "agent_idle_all:main");
}

/// Session shutdown must discard a pending `agent_idle_all` timer for the
/// closing session. Without this regression guard, EOF idle-draining can emit a
/// notification for a session the harness has already left.
#[test]
fn agent_idle_all_timer_is_cleared_on_session_shutdown() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_idle_all": [{
                    "delay_seconds": 1,
                    "osc1337": { "key": TEXT_VAR_NAME, "value": "stale all idle" },
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
        .write_event(&session_shutdown("s1"))
        .expect("shutdown session");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_millis(1));

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    assert!(reader.read_event().expect("read eof").is_none());
}

/// Session shutdown must also clear the all-idle session/agent membership
/// tracker. Otherwise a later state transition for the same agent id can arm a
/// duplicate notification for the already-closed session.
#[test]
fn agent_idle_all_tracker_forgets_shutdown_session_membership() {
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
        .expect("load main in old session");
    writer
        .write_event(&agent_state("main", tau_proto::AgentRuntimeState::Running))
        .expect("main running in old session");
    writer
        .write_event(&session_shutdown("s1"))
        .expect("shutdown old session");
    writer
        .write_event(&session_agent_loaded("s2", "main"))
        .expect("load main in new session");
    writer
        .write_event(&agent_state("main", tau_proto::AgentRuntimeState::Running))
        .expect("main running in new session");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response_for_agent(
                "main",
                "sp-main",
                "done",
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("finish main in new session");
    writer
        .write_event(&agent_state("main", tau_proto::AgentRuntimeState::Idle))
        .expect("main idle in new session");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_millis(1));

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let all_idle = reader.read_event().expect("read").expect("all idle event");
    let Event::Osc1337SetUserVar(osc) = all_idle else {
        panic!("expected all-idle OSC, got {all_idle:?}");
    };
    assert_eq!(osc.value, "all idle");
    assert!(reader.read_event().expect("read eof").is_none());
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

    let emit = reader.read_emit().expect("read").expect("summary query");
    assert!(
        !emit.persist,
        "idle-summary start requests must explicitly use transient delivery"
    );
    let Event::StartAgentRequest(query) = *emit.event else {
        panic!("expected StartAgentRequest, got {:?}", emit.event);
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
                name: test_extension_name("std-notifications"),
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

/// A shutdown for an unrelated session must not remove the ignore entry for an
/// accepted all-idle summary side agent. This prevents the later side-agent
/// load and running events from being tracked as real session work and
/// canceling the pending summary-backed notification.
#[test]
fn unrelated_session_shutdown_preserves_pending_all_idle_summary_ignore() {
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
                    "osc1337": {
                        "key": TEXT_VAR_NAME,
                        "value": "{{hook}}:{{turn.agent_summary}}",
                    },
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
        .write_event(&session_shutdown("s2"))
        .expect("shutdown unrelated session");
    writer
        .write_event(&session_agent_loaded("s1", "summary"))
        .expect("load summary side agent");
    writer
        .write_event(&agent_state(
            "summary",
            tau_proto::AgentRuntimeState::Running,
        ))
        .expect("summary running");
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

    let output = run_with_idle_output(input, Duration::from_millis(1));

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
    let output =
        run_with_idle_and_summary_output(input, Duration::from_millis(1), Duration::from_millis(1));

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

    let output = run_with_idle_output(input, Duration::from_millis(1));

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
    let output = run_with_idle_and_summary_output(
        input,
        Duration::from_millis(50),
        Duration::from_millis(50),
    );

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

/// Immediate-then-periodic coalesced typing pings (`UiPromptDraft`) arriving
/// during the `WaitingIdle` window must extend the deadline so the
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
                session_id: "s1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                target_agent_id: None,
                text: Some(format!("partial draft {i}")),
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
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            target_agent_id: None,
            text: Some("typing while summary is in flight".into()),
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

/// Oversized rendered command arguments should skip the command rather than
/// spawning a local process with unbounded untrusted template data.
#[test]
fn oversized_rendered_command_arg_skips_command() {
    use tempfile::TempDir;

    let td = TempDir::new().expect("tempdir");
    let out_path = td.path().join("out.txt");
    let cmd = format!("touch {dest}", dest = out_path.display());

    let oversized = "x".repeat(MAX_COMMAND_ARG_LEN + 1);
    let cfg = tau_proto::json_to_cbor(&serde_json::json!({
        "agent_end": [{
            "command": ["bash", "-c", cmd, "_marker", oversized],
        }],
    }));
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer.write_frame(&configure_frame(cfg)).expect("write");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write response");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let _output = run_with_idle_output(input, Duration::from_secs(3600));

    thread::sleep(Duration::from_millis(50));
    assert!(
        !out_path.exists(),
        "oversized command argument should skip spawning the command",
    );
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

    let output = run_with_idle_output(input, Duration::from_secs(3600));

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

/// A malformed reconfiguration must emit one `ConfigError` and keep the
/// previous accepted config plus already-armed idle hook state intact. This
/// protects the migration contract that tau-client reports config errors while
/// std-notifications continues with the last valid notification policy.
#[test]
fn invalid_config_preserves_previous_config_and_pending_idle() {
    let valid_config = tau_proto::json_to_cbor(&serde_json::json!({
        "agent_idle": [{
            "osc1337": {
                "key": TEXT_VAR_NAME,
                "value": "still-active",
            },
        }],
    }));
    let bad_config = tau_proto::json_to_cbor(&serde_json::json!({
        "totally_unknown_field": 7,
    }));

    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(valid_config))
        .expect("write valid config");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-0", "done", tau_proto::PromptOriginator::User),
        ))
        .expect("write response");
    writer
        .write_frame(&configure_frame(bad_config))
        .expect("write bad config");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_millis(20));
    let mut reader = HarnessInputReader::new(Cursor::new(output));
    let mut config_error_count = 0;
    let mut saw_preserved_idle_hook = false;

    while let Some(frame) = reader.read_message().expect("read") {
        match frame {
            HarnessInputMessage::ConfigError(error) => {
                config_error_count += 1;
                assert!(
                    error.message.contains("totally_unknown_field"),
                    "config error should describe the malformed reconfigure: {error:?}",
                );
            }
            HarnessInputMessage::Emit(emit) => {
                if let Event::Osc1337SetUserVar(osc) = *emit.event {
                    saw_preserved_idle_hook |=
                        osc.name == TEXT_VAR_NAME && osc.value == "still-active";
                }
            }
            _ => {}
        }
    }

    assert_eq!(
        config_error_count, 1,
        "malformed reconfigure should emit exactly one ConfigError",
    );
    assert!(
        saw_preserved_idle_hook,
        "previous config and pending idle hook should remain active after invalid reconfigure",
    );
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

    let output = run_with_idle_output(input, Duration::from_secs(3600));

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

    let output = run_with_idle_output(input, Duration::from_secs(3600));

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
            inference_activation: false,
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            text: "hello".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: Some("bad=key".to_owned()),
            ctx_id: None,
        }))
        .expect("write prompt");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_secs(3600));

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    assert!(
        reader.read_event().expect("read").is_none(),
        "runtime-invalid OSC key should skip emission",
    );
}

/// The `json` helper should make untrusted prompt/response text safe to embed
/// into JSON notification payloads instead of letting quotes or newlines break
/// downstream consumers' expected structure.
#[test]
fn json_helper_quotes_untrusted_template_values() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_end": [{
                    "osc1337": {
                        "key": TEXT_VAR_NAME,
                        "value": "{\"body\":{{json turn.agent_response}}}",
                    },
                }],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response(
                "sp-0",
                "quote: \"yes\"\nnext",
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("write response");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_secs(3600));

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let event = reader.read_event().expect("read").expect("osc");
    let Event::Osc1337SetUserVar(osc) = event else {
        panic!("expected text OSC, got {event:?}");
    };
    let payload: serde_json::Value = serde_json::from_str(&osc.value).expect("valid JSON");
    assert_eq!(payload["body"], "quote: \"yes\"\nnext");
}

/// Oversized rendered OSC values are skipped so untrusted model text cannot
/// amplify terminal-facing side effects without bound.
#[test]
fn oversized_rendered_osc_value_is_skipped() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_end": [{
                    "osc1337": { "key": TEXT_VAR_NAME, "value": "{{turn.agent_response}}" },
                }],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response(
                "sp-0",
                &"x".repeat(MAX_OSC1337_VALUE_LEN + 1),
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("write response");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_secs(3600));

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    assert!(reader.read_event().expect("read eof").is_none());
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
    let output =
        run_with_idle_and_summary_output(input, Duration::from_millis(1), Duration::from_millis(1));

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
            session_id: "session"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            target_agent_id: None,
            text: Some("still typing".to_owned()),
        }))
        .expect("write draft");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_secs(3600));

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

    // Long idle window — if the cancel works, we never wait.
    let output = run_with_idle_output(input, Duration::from_secs(3600));

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
                    raw_arguments_json: None,
                    responses_envelope: None,
                },
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("write");

    // Sub-agent activity — must not clear idle, fire chimes, or
    // touch `waiting_for_final_response`.
    writer
        .write_event(&Event::ProviderPromptSubmitted(ProviderPromptSubmitted {
            agent_prompt_id: "sp-side"
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            originator: tau_proto::PromptOriginator::Extension {
                name: test_extension_name("core-subagents"),
                query_id: "q1".into(),
            },
        }))
        .expect("write");
    writer
        .write_event(&user_prompt_submitted(
            "side instruction",
            tau_proto::PromptOriginator::Extension {
                name: test_extension_name("core-subagents"),
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
                    name: test_extension_name("core-subagents"),
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

    let output = run_with_idle_output(input, Duration::from_secs(3600));

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

    let output = run_with_idle_output(input, Duration::from_secs(3600));

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

/// Interleaved user turns for different agents should each get their own start
/// and end notification. This prevents the duplicate-suppression state for one
/// active agent from muting another loaded agent's visible turn.
#[test]
fn interleaved_agents_each_emit_start_and_end_with_own_prompt_context() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_start": [{
                    "osc1337": { "key": SOUND_VAR_NAME, "value": "start:{{agent.id}}:{{turn.user_prompt}}" },
                }],
                "agent_end": [{
                    "osc1337": { "key": SOUND_VAR_NAME, "value": "end:{{agent.id}}:{{turn.user_prompt}}:{{turn.agent_response}}" },
                }],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&user_prompt_submitted_for_agent(
            "main",
            "main prompt",
            tau_proto::PromptOriginator::User,
        ))
        .expect("main prompt");
    writer
        .write_event(&user_prompt_submitted_for_agent(
            "other",
            "other prompt",
            tau_proto::PromptOriginator::User,
        ))
        .expect("other prompt");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response_for_agent(
                "main",
                "sp-main",
                "main done",
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("main done");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response_for_agent(
                "other",
                "sp-other",
                "other done",
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("other done");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_secs(3600));

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let mut values = Vec::new();
    while let Some(Event::Osc1337SetUserVar(osc)) = reader.read_event().expect("read") {
        values.push(osc.value);
    }
    assert_eq!(
        values,
        [
            "start:main:main prompt",
            "start:other:other prompt",
            "end:main:main prompt:main done",
            "end:other:other prompt:other done",
        ],
    );
}

/// Prompt termination means no provider response will arrive. The extension
/// should clear the in-flight state so a later prompt for the same agent is
/// treated as a fresh user turn and can ring its start notification.
#[test]
fn terminated_prompt_clears_in_flight_turn_state() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&default_notifications_config_frame())
        .expect("write config");
    writer
        .write_event(&user_prompt_submitted(
            "first",
            tau_proto::PromptOriginator::User,
        ))
        .expect("first prompt");
    writer
        .write_event(&agent_prompt_terminated("main", "sp-first"))
        .expect("terminated");
    writer
        .write_event(&user_prompt_submitted(
            "second",
            tau_proto::PromptOriginator::User,
        ))
        .expect("second prompt");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_secs(3600));

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let first = reader.read_event().expect("read").expect("first start");
    let Event::Osc1337SetUserVar(osc) = first else {
        panic!("expected first start OSC");
    };
    assert_eq!(osc.value, VALUE_AGENT_START);
    let second = reader.read_event().expect("read").expect("second start");
    let Event::Osc1337SetUserVar(osc) = second else {
        panic!("expected second start OSC");
    };
    assert_eq!(osc.value, VALUE_AGENT_START);
    assert!(reader.read_event().expect("read eof").is_none());
}

/// A termination for an older non-current prompt must still mark that prompt id
/// consumed so a stale provider completion cannot emit a belated end sound.
#[test]
fn stale_completion_after_non_current_termination_is_ignored() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&default_notifications_config_frame())
        .expect("write config");
    writer
        .write_event(&agent_prompt_started_for_agent("main", "sp-current"))
        .expect("current prompt started");
    writer
        .write_event(&agent_prompt_terminated("main", "sp-old"))
        .expect("old terminated");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response("sp-old", "stale", tau_proto::PromptOriginator::User),
        ))
        .expect("stale response");
    writer.write_frame(&disconnect_frame(None)).expect("write");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_secs(3600));

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    assert!(reader.read_event().expect("read eof").is_none());
}

/// Per-agent idle timers should survive another agent starting a prompt. This
/// prevents one loaded agent from cancelling another agent's pending idle text.
#[test]
fn agent_idle_timers_are_scoped_per_agent() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_idle": [{
                    "delay_seconds": 0,
                    "osc1337": { "key": TEXT_VAR_NAME, "value": "idle:{{agent.id}}" },
                }],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&user_prompt_submitted_for_agent(
            "main",
            "main",
            tau_proto::PromptOriginator::User,
        ))
        .expect("main prompt");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response_for_agent(
                "main",
                "sp-main",
                "done",
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("main response");
    writer
        .write_event(&user_prompt_submitted_for_agent(
            "other",
            "other",
            tau_proto::PromptOriginator::User,
        ))
        .expect("other prompt");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_millis(1));

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let idle = reader.read_event().expect("read").expect("idle");
    let Event::Osc1337SetUserVar(osc) = idle else {
        panic!("expected main idle OSC, got {idle:?}");
    };
    assert_eq!(osc.value, "idle:main");
}

/// Provider prompt submissions carry only an agent prompt id. If the extension
/// does not yet know that prompt's owning agent, it must not globally clear
/// unrelated per-agent idle timers, or a prompt for one active agent can
/// suppress another agent's waiting-user notification.
#[test]
fn unowned_provider_prompt_does_not_clear_other_agent_idle_timer() {
    let mut input = Vec::new();
    let mut writer = EventWriter::new(&mut input);
    writer
        .write_frame(&configure_frame(tau_proto::json_to_cbor(
            &serde_json::json!({
                "agent_idle": [{
                    "delay_seconds": 0,
                    "osc1337": { "key": TEXT_VAR_NAME, "value": "idle:{{agent.id}}" },
                }],
            }),
        )))
        .expect("write config");
    writer
        .write_event(&user_prompt_submitted_for_agent(
            "main",
            "main prompt",
            tau_proto::PromptOriginator::User,
        ))
        .expect("main prompt");
    writer
        .write_event(&Event::ProviderResponseFinished(
            assistant_finished_response_for_agent(
                "main",
                "sp-main",
                "done",
                tau_proto::PromptOriginator::User,
            ),
        ))
        .expect("main response");
    writer
        .write_event(&Event::ProviderPromptSubmitted(
            tau_proto::ProviderPromptSubmitted {
                agent_prompt_id: "sp-unowned-other"
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
                originator: tau_proto::PromptOriginator::User,
            },
        ))
        .expect("unowned provider prompt");
    writer.flush().expect("flush");

    let output = run_with_idle_output(input, Duration::from_millis(1));

    let mut reader = EventReader::new(Cursor::new(output));
    drain_lifecycle(&mut reader);
    let idle = reader.read_event().expect("read").expect("idle");
    let Event::Osc1337SetUserVar(osc) = idle else {
        panic!("expected main idle OSC, got {idle:?}");
    };
    assert_eq!(osc.value, "idle:main");
}

/// Builds a validated extension name used by this test module.
fn test_extension_name(value: impl AsRef<str>) -> tau_proto::ExtensionName {
    tau_proto::ExtensionName::parse(value.as_ref())
        .expect("test extension name must satisfy the identifier grammar")
}
