use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use tau_proto::{
    AgentPromptSteered, Configure, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage,
    HarnessOutputWriter, PromptMessageClass,
};

use super::*;

/// Thread-safe byte sink used by the manual extension writer thread.
#[derive(Clone, Default)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl SharedWriter {
    /// Consume the only writer reference and return all protocol bytes.
    fn into_bytes(self) -> Vec<u8> {
        Arc::try_unwrap(self.0)
            .expect("single writer reference")
            .into_inner()
            .expect("writer mutex")
    }
}

impl Write for SharedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("writer mutex")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn run_frames(
    papercut_enabled: bool,
    deliveries: impl IntoIterator<Item = HarnessOutputMessage>,
) -> Vec<HarnessInputMessage> {
    let config = papercut_enabled.then(|| {
        CborValue::Map(vec![(
            CborValue::Text("papercut".to_owned()),
            CborValue::Map(vec![(
                CborValue::Text("enable".to_owned()),
                CborValue::Bool(true),
            )]),
        )])
    });
    let configure = HarnessOutputMessage::Configure(Configure {
        tool_prefix: Some(tau_proto::ToolNamePrefix::parse("work").expect("prefix")),
        instance_name: tau_proto::ExtensionName::parse("std-utils").expect("extension name"),
        config: config.unwrap_or_else(|| CborValue::Map(Vec::new())),
        state_dir: None,
        secrets: BTreeMap::new(),
        settings_files: Default::default(),
    });
    let mut input = Vec::new();
    let mut input_writer = HarnessOutputWriter::new(&mut input);
    input_writer
        .write_message(&configure)
        .expect("configure input");
    for delivery in deliveries {
        input_writer
            .write_message(&delivery)
            .expect("delivery input");
    }
    input_writer.flush().expect("flush configure input");

    let output = SharedWriter::default();
    run(Cursor::new(input), output.clone()).expect("run std-utils");
    let mut reader = HarnessInputReader::new(Cursor::new(output.into_bytes()));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("read output frame") {
        frames.push(frame);
    }
    frames
}

fn startup_frames(papercut_enabled: bool) -> Vec<HarnessInputMessage> {
    run_frames(papercut_enabled, [])
}

fn declared_tool_names(frames: &[HarnessInputMessage]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                Event::ToolRegistrationDeclared(registration) => {
                    Some(registration.tool.name.to_string())
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn cbor_map(entries: Vec<(&str, CborValue)>) -> CborValue {
    CborValue::Map(
        entries
            .into_iter()
            .map(|(key, value)| (CborValue::Text(key.to_owned()), value))
            .collect(),
    )
}

fn started(call_id: &str, agent: &str, args: CborValue) -> ToolStarted {
    ToolStarted {
        call_id: tau_proto::ToolCallId::new(call_id),
        tool_name: tau_proto::ToolName::new(TIMER_TOOL_NAME),
        arguments: args,
        agent_id: AgentId::parse(agent).expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    }
}

fn runtime() -> TimerRuntime {
    TimerRuntime {
        handle: None,
        timers: HashMap::new(),
        pending_invocations: HashMap::new(),
        replay_complete_agents: HashSet::new(),
        timer_tool_name: None,
        papercut_tool_name: None,
        session_id: None,
        papercut_storage: None,
    }
}

/// Deterministic in-memory session append target for papercut unit tests.
#[derive(Clone, Default)]
struct FakePapercutStorage {
    /// Complete lines accepted by the fake harness.
    lines: Rc<RefCell<Vec<Vec<u8>>>>,
    /// Whether the next append should model a harness storage rejection.
    fail: bool,
}

impl PapercutStorage for FakePapercutStorage {
    fn append_papercut(&self, contents: Vec<u8>) -> Result<(), String> {
        if self.fail {
            return Err("permission: session scope unavailable".to_owned());
        }
        self.lines.borrow_mut().push(contents);
        Ok(())
    }
}

/// Due timer wakeups must explicitly request `persist=false` publication so
/// first-party behavior does not depend only on the event default.
#[test]
fn timer_wakeup_message_uses_persist_false() {
    let message = FireRecord {
        agent_id: AgentId::parse("agent-one").expect("agent id"),
        timer_id: "wake".to_owned(),
        prompt: "wake now".to_owned(),
        ctx_id: "timer:wake:1".to_owned(),
    }
    .into_internal_prompt_message();
    assert!(matches!(
        message,
        HarnessInputMessage::Emit(emit)
            if !emit.persist
                && matches!(
                    emit.event.as_ref(),
                    Event::ExtInternalPromptSubmitRequest(request)
                         if request.text == "wake now"
                             && request.ctx_id.as_deref() == Some("timer:wake:1")
                             && request.activation_kind
                                 == Some(tau_proto::InternalPromptActivationKind::Timer)
                 )
    ));
}

/// Timer schemas describe the runtime byte limit without a large grammar
/// repetition.
#[test]
fn timer_tool_schema_omits_message_max_length() {
    let spec = timer_tool_spec();
    let parameters = spec.parameters.expect("timer parameters");
    let message = parameters
        .pointer("/properties/message")
        .expect("timer message schema");
    let expected_description = format!("Reminder text; maximum {MAX_MESSAGE_BYTES} bytes.");

    assert!(message.get("maxLength").is_none());
    assert_eq!(
        message
            .get("description")
            .and_then(serde_json::Value::as_str),
        Some(expected_description.as_str())
    );
}

/// Restore folding waits for the agent replay boundary before firing an
/// overdue timer.
#[test]
fn overdue_timer_waits_for_agent_replay_complete() {
    let mut rt = runtime();
    let agent = AgentId::parse("agent-one").expect("agent id");
    let base = UnixMicros::new(1_000_000);
    let args = cbor_map(vec![
        ("action", CborValue::Text("schedule".to_owned())),
        ("timer_id", CborValue::Text("wake".to_owned())),
        ("delay_seconds", CborValue::Integer(10.into())),
        ("message", CborValue::Text("hello".to_owned())),
    ]);
    let start = started("call-1", agent.as_ref(), args);
    rt.handle_started_replay(&start);
    rt.handle_result_replay(
        &ToolResult {
            call_id: start.call_id.clone(),
            tool_name: start.tool_name.clone(),
            tool_type: ToolType::Function,
            result: CborValue::Text("ok".to_owned()),
            provider_content: Vec::new(),
            kind: ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        },
        base,
    );

    assert!(rt.collect_due(add_seconds(base, 15)).is_empty());
    rt.complete_agent_replay(&AgentReplayComplete {
        agent_id: agent.clone(),
        session_id: None,
        error: None,
    })
    .expect("complete");
    let fires = rt.collect_due(add_seconds(base, 15));
    assert_eq!(fires.len(), 1);
    assert_eq!(fires[0].ctx_id, "timer:wake:1");
}

/// Periodic timers coalesce missed downtime into one prompt and advance
/// past now.
#[test]
fn periodic_timer_coalesces_missed_intervals() {
    let mut rt = runtime();
    let agent = AgentId::parse("agent-one").expect("agent id");
    rt.replay_complete_agents.insert(agent.clone());
    rt.schedule_timer(
        &agent,
        ScheduleArgs {
            timer_id: "tick".to_owned(),
            delay_seconds: 1,
            interval_seconds: Some(10),
            message: "tick".to_owned(),
        },
        UnixMicros::new(0),
    )
    .expect("schedule");
    let fires = rt.collect_due(UnixMicros::new(35_000_000));
    assert_eq!(fires.len(), 1);
    assert!(fires[0].prompt.contains("Coalesced 4 missed"));
    let timer = rt
        .timers
        .get(&TimerKey {
            agent_id: agent,
            timer_id: "tick".to_owned(),
        })
        .expect("timer");
    assert!(timer.next_fire_at > UnixMicros::new(35_000_000));
}

/// Replayed timer-fired prompts remove one-shot timers so they are not
/// fired again.
#[test]
fn replayed_timer_prompt_removes_one_shot() {
    let mut rt = runtime();
    let agent = AgentId::parse("agent-one").expect("agent id");
    rt.replay_complete_agents.insert(agent.clone());
    rt.schedule_timer(
        &agent,
        ScheduleArgs {
            timer_id: "once".to_owned(),
            delay_seconds: 1,
            interval_seconds: None,
            message: "wake".to_owned(),
        },
        UnixMicros::new(0),
    )
    .expect("schedule");
    rt.handle_prompt_replay(
        &AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent.clone(),
            text: "Timer `once` fired: wake".to_owned(),
            message_class: PromptMessageClass::Internal,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: Some("timer:once:1".to_owned()),
        },
        Some(UnixMicros::new(1_000_000)),
    );
    assert!(!rt.timers.contains_key(&TimerKey {
        agent_id: agent,
        timer_id: "once".to_owned()
    }));
}

/// Live timer calls mark their owning agent fireable after catch-up
/// released them.
#[test]
fn live_timer_schedule_can_fire_without_prior_boundary() {
    let mut rt = runtime();
    let agent = AgentId::parse("agent-one").expect("agent id");
    let args = cbor_map(vec![
        ("action", CborValue::Text("schedule".to_owned())),
        ("timer_id", CborValue::Text("live".to_owned())),
        ("delay_seconds", CborValue::Integer(10.into())),
        ("message", CborValue::Text("wake".to_owned())),
    ]);
    let invoke = started("call-live", agent.as_ref(), args);

    rt.handle_live_tool(&invoke, UnixMicros::new(0))
        .expect("live schedule");

    let fires = rt.collect_due(UnixMicros::new(12_000_000));
    assert_eq!(fires.len(), 1);
    assert_eq!(fires[0].ctx_id, "timer:live:1");
}

/// Session lifecycle clears session-scoped timer state before a new session
/// can fire it.
#[test]
fn session_lifecycle_clears_session_scoped_timers() {
    let mut rt = runtime();
    let agent = AgentId::parse("agent-one").expect("agent id");
    rt.replay_complete_agents.insert(agent.clone());
    rt.schedule_timer(
        &agent,
        ScheduleArgs {
            timer_id: "old".to_owned(),
            delay_seconds: 10,
            interval_seconds: None,
            message: "wake".to_owned(),
        },
        UnixMicros::new(0),
    )
    .expect("schedule");

    rt.clear_session_state();

    assert!(rt.collect_due(UnixMicros::new(20_000_000)).is_empty());
    assert!(rt.timers.is_empty());
    assert!(rt.replay_complete_agents.is_empty());
}

/// Unloaded agents are not fireable again until a later successful replay
/// boundary.
#[test]
fn unloaded_agent_timers_are_dormant_until_replay_complete() {
    let mut rt = runtime();
    let agent = AgentId::parse("agent-one").expect("agent id");
    rt.replay_complete_agents.insert(agent.clone());
    rt.schedule_timer(
        &agent,
        ScheduleArgs {
            timer_id: "dormant".to_owned(),
            delay_seconds: 10,
            interval_seconds: None,
            message: "wake".to_owned(),
        },
        UnixMicros::new(0),
    )
    .expect("schedule");

    rt.unload_agent(&agent);

    assert!(rt.collect_due(UnixMicros::new(20_000_000)).is_empty());
    rt.replay_complete_agents.insert(agent.clone());
    assert_eq!(rt.collect_due(UnixMicros::new(20_000_000)).len(), 1);
}

/// Replayed steered timer prompts count as fired evidence for busy-agent
/// wakeups.
#[test]
fn replayed_steered_timer_prompt_removes_one_shot() {
    let mut rt = runtime();
    let agent = AgentId::parse("agent-one").expect("agent id");
    rt.replay_complete_agents.insert(agent.clone());
    rt.schedule_timer(
        &agent,
        ScheduleArgs {
            timer_id: "busy".to_owned(),
            delay_seconds: 10,
            interval_seconds: None,
            message: "wake".to_owned(),
        },
        UnixMicros::new(0),
    )
    .expect("schedule");
    let steered = AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        agent_id: agent.clone(),
        text: "Timer `busy` fired: wake".to_owned(),
        message_class: PromptMessageClass::Internal,
        internal_kind: None,
        ctx_id: Some("timer:busy:1".to_owned()),
    };
    let submitted = AgentPromptSubmitted {
        inference_activation: false,
        agent_id: steered.agent_id.clone(),
        text: steered.text,
        message_class: steered.message_class,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: steered.ctx_id,
    };

    rt.handle_prompt_replay(&submitted, Some(UnixMicros::new(10_000_000)));

    assert!(!rt.timers.contains_key(&TimerKey {
        agent_id: agent,
        timer_id: "busy".to_owned()
    }));
}

/// Errored replay boundaries discard restored timers so later live calls do
/// not unlock them.
#[test]
fn errored_replay_drops_restored_timers_before_later_live_call() {
    let mut rt = runtime();
    let agent = AgentId::parse("agent-one").expect("agent id");
    rt.schedule_timer(
        &agent,
        ScheduleArgs {
            timer_id: "bad-restore".to_owned(),
            delay_seconds: 10,
            interval_seconds: None,
            message: "wake".to_owned(),
        },
        UnixMicros::new(0),
    )
    .expect("restore schedule");
    rt.complete_agent_replay(&AgentReplayComplete {
        agent_id: agent.clone(),
        session_id: None,
        error: Some("corrupt agent log".to_owned()),
    })
    .expect("errored boundary");
    let list_args = cbor_map(vec![("action", CborValue::Text("list".to_owned()))]);
    let live = started("call-live-list", agent.as_ref(), list_args);

    rt.handle_live_tool(&live, UnixMicros::new(20_000_000))
        .expect("live list");

    assert!(rt.collect_due(UnixMicros::new(20_000_000)).is_empty());
}

/// Scheduling an already-active id is rejected instead of acting as update.
#[test]
fn duplicate_schedule_is_rejected() {
    let mut rt = runtime();
    let agent = AgentId::parse("agent-one").expect("agent id");
    let args = ScheduleArgs {
        timer_id: "same".to_owned(),
        delay_seconds: 10,
        interval_seconds: None,
        message: "wake".to_owned(),
    };
    rt.schedule_timer(&agent, args.clone(), UnixMicros::new(0))
        .expect("first schedule");

    assert!(rt.schedule_timer(&agent, args, UnixMicros::new(0)).is_err());
}

/// Timer result display args summarize schedule timing and recurrence
/// compactly.
#[test]
fn timer_display_args_summarize_schedule() {
    let args = cbor_map(vec![
        ("action", CborValue::Text("schedule".to_owned())),
        ("timer_id", CborValue::Text("standup".to_owned())),
        ("delay_seconds", CborValue::Integer(600.into())),
        ("interval_seconds", CborValue::Integer(3600.into())),
        ("message", CborValue::Text("wake".to_owned())),
    ]);

    assert_eq!(
        timer_display_args(&args, "call-1"),
        "schedule standup in 10m every 1h"
    );
}

/// Timer result display args identify cancel and list actions.
#[test]
fn timer_display_args_summarize_cancel_and_list() {
    let cancel = cbor_map(vec![
        ("action", CborValue::Text("cancel".to_owned())),
        ("timer_id", CborValue::Text("standup".to_owned())),
    ]);
    let list = cbor_map(vec![("action", CborValue::Text("list".to_owned()))]);

    assert_eq!(timer_display_args(&cancel, "call-1"), "cancel standup");
    assert_eq!(timer_display_args(&list, "call-1"), "list");
}

/// Terminal timer displays must make each successful action useful in compact
/// history without changing its model-visible result. This covers scheduled,
/// cancelled, absent, and listed timer lifecycle outcomes deterministically.
#[test]
fn timer_completion_display_summarizes_successful_action_outcomes() {
    let mut rt = runtime();
    let agent = "agent-one";
    let schedule = started(
        "call-schedule",
        agent,
        cbor_map(vec![
            ("action", CborValue::Text("schedule".to_owned())),
            ("timer_id", CborValue::Text("standup".to_owned())),
            ("delay_seconds", CborValue::Integer(600.into())),
            ("message", CborValue::Text("join the call".to_owned())),
        ]),
    );
    let scheduled = rt
        .handle_live_tool(&schedule, UnixMicros::new(0))
        .expect("schedule");
    assert_eq!(
        scheduled.result,
        CborValue::Text("scheduled timer `standup`".to_owned())
    );
    assert_eq!(scheduled.display.args, "schedule standup in 10m");
    assert!(scheduled.display.info_chips.is_empty());

    let list = started(
        "call-list",
        agent,
        cbor_map(vec![("action", CborValue::Text("list".to_owned()))]),
    );
    let listed = rt
        .handle_live_tool(&list, UnixMicros::new(0))
        .expect("list");
    assert_eq!(
        listed.result,
        CborValue::Text("standup: due in 600s, one-shot".to_owned())
    );
    assert_eq!(listed.display.args, "list");
    assert_eq!(listed.display.stats.matches, Some(1));
    assert!(listed.display.info_chips.is_empty());

    let cancel = started(
        "call-cancel",
        agent,
        cbor_map(vec![
            ("action", CborValue::Text("cancel".to_owned())),
            ("timer_id", CborValue::Text("standup".to_owned())),
        ]),
    );
    let cancelled = rt
        .handle_live_tool(&cancel, UnixMicros::new(0))
        .expect("cancel");
    assert_eq!(
        cancelled.result,
        CborValue::Text("cancelled timer `standup`".to_owned())
    );
    assert!(cancelled.display.info_chips.is_empty());

    let absent = rt
        .handle_live_tool(&cancel, UnixMicros::new(0))
        .expect("cancel absent timer");
    assert_eq!(
        absent.result,
        CborValue::Text("timer `standup` was not active".to_owned())
    );
    assert_eq!(absent.display.info_chips, ["not active"]);

    let empty = rt
        .handle_live_tool(&list, UnixMicros::new(0))
        .expect("empty list");
    assert_eq!(empty.result, CborValue::Text("no active timers".to_owned()));
    assert_eq!(empty.display.stats.matches, Some(0));
}

/// Timer display args do not echo unknown actions or unsafe timer ids.
#[test]
fn timer_display_args_do_not_echo_untrusted_fields() {
    let unknown = cbor_map(vec![(
        "action",
        CborValue::Text("bad action with lots of text".to_owned()),
    )]);
    let unsafe_schedule = cbor_map(vec![
        ("action", CborValue::Text("schedule".to_owned())),
        ("timer_id", CborValue::Text("../unsafe".to_owned())),
        ("delay_seconds", CborValue::Integer(600.into())),
        ("interval_seconds", CborValue::Integer(3600.into())),
    ]);

    assert_eq!(timer_display_args(&unknown, "call-1"), "");
    assert_eq!(
        timer_display_args(&unsafe_schedule, "call-1"),
        "schedule in 10m every 1h"
    );
}

/// Timer validation enforces bounded path-safe ids and message length.
#[test]
fn schedule_validation_rejects_unsafe_timer_id() {
    let args = cbor_map(vec![
        ("action", CborValue::Text("schedule".to_owned())),
        ("timer_id", CborValue::Text("../bad".to_owned())),
        ("delay_seconds", CborValue::Integer(10.into())),
        ("message", CborValue::Text("hello".to_owned())),
    ]);
    assert!(parse_action(&args, "call-1").is_err());
}

/// Timer validation retains the byte limit omitted from the model-visible
/// schema.
#[test]
fn schedule_validation_enforces_message_byte_limit() {
    let schedule_args = |message| {
        cbor_map(vec![
            ("action", CborValue::Text("schedule".to_owned())),
            ("delay_seconds", CborValue::Integer(10.into())),
            ("message", CborValue::Text(message)),
        ])
    };
    let bytes_per_character = "é".len();
    let character_limit = MAX_MESSAGE_BYTES / bytes_per_character;
    let at_limit = "é".repeat(character_limit);
    let oversized = "é".repeat(character_limit + 1);

    assert_eq!(at_limit.len(), MAX_MESSAGE_BYTES);
    assert!(parse_action(&schedule_args(at_limit), "call-1").is_ok());
    assert!(oversized.chars().count() < MAX_MESSAGE_BYTES);
    assert_eq!(
        parse_action(&schedule_args(oversized), "call-1").expect_err("oversized message"),
        format!("message must be 1..={MAX_MESSAGE_BYTES} bytes")
    );
}

fn papercut_started(call_id: &str, agent: &str, report: &str) -> ToolStarted {
    ToolStarted {
        call_id: tau_proto::ToolCallId::new(call_id),
        tool_name: tau_proto::ToolName::new(PAPERCUT_TOOL_NAME),
        arguments: cbor_map(vec![("report", CborValue::Text(report.to_owned()))]),
        agent_id: AgentId::parse(agent).expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    }
}

/// The extension must never declare papercut or its prompt until the
/// per-instance opt-in setting enables it; normal enabled-by-default policy
/// then leaves final role visibility to the ordinary harness policy pipeline.
#[test]
fn papercut_config_gates_visibility_and_prompt() {
    let disabled: UtilsConfig =
        serde_json::from_value(serde_json::json!({})).expect("default config");
    let enabled: UtilsConfig = serde_json::from_value(serde_json::json!({
        "papercut": {"enable": true}
    }))
    .expect("enabled config");

    assert!(!disabled.papercut.enable);
    assert!(enabled.papercut.enable);
    assert_eq!(tool_registrations(false).len(), 1);
    let registrations = tool_registrations(true);
    let papercut = registrations
        .iter()
        .find(|registration| registration.tool.name.as_str() == PAPERCUT_TOOL_NAME)
        .expect("enabled papercut registration");
    assert!(papercut.tool.enabled_by_default);
    assert!(
        papercut
            .prompt_fragment
            .as_ref()
            .expect("papercut prompt")
            .template
            .contains("do not retry")
    );
}

/// Deferred startup must consume the actual encoded configuration before
/// declaring tools, preserving configured prefixes and placing every accepted
/// declaration before `Ready`.
#[test]
fn papercut_runtime_startup_gates_prefixed_declarations() {
    let disabled = startup_frames(false);
    let enabled = startup_frames(true);

    assert_eq!(declared_tool_names(&disabled), ["work_timer"]);
    assert_eq!(
        declared_tool_names(&enabled),
        ["work_timer", "work_papercut"]
    );
    for frames in [&disabled, &enabled] {
        let ready = frames
            .iter()
            .position(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
            .expect("ready");
        assert!(
            frames[..ready]
                .iter()
                .any(|frame| matches!(frame, HarnessInputMessage::Subscribe(_)))
        );
        let last_declaration = frames
            .iter()
            .rposition(|frame| {
                matches!(
                    frame,
                    HarnessInputMessage::Emit(emit)
                        if matches!(emit.event.as_ref(), Event::ToolRegistrationDeclared(_))
                )
            })
            .expect("tool declaration");
        assert!(last_declaration < ready);
    }
}

/// A live, prefixed papercut call after `session.started` reaches the
/// session-storage path, while an unprefixed lookalike remains unhandled by
/// the dynamically scoped runtime dispatch.
#[test]
fn papercut_runtime_dispatches_only_prefixed_live_calls() {
    let session = Event::SessionStarted(tau_proto::SessionStarted {
        session_id: tau_proto::SessionId::parse("session-42").expect("session id"),
        reason: tau_proto::SessionStartReason::Initial,
    });
    let mut prefixed = papercut_started("prefixed", "agent-one", "prefix dispatch");
    prefixed.tool_name = tau_proto::ToolName::new("work_papercut");
    let frames = run_frames(
        true,
        [
            HarnessOutputMessage::Deliver(tau_proto::EventDelivery::live(
                UnixMicros::new(1),
                session,
            )),
            HarnessOutputMessage::Deliver(tau_proto::EventDelivery::live(
                UnixMicros::new(2),
                Event::ToolStarted(papercut_started(
                    "unprefixed",
                    "agent-one",
                    "must not dispatch",
                )),
            )),
            HarnessOutputMessage::Deliver(tau_proto::EventDelivery::live(
                UnixMicros::new(3),
                Event::ToolStarted(prefixed),
            )),
        ],
    );

    let appends = frames
        .iter()
        .filter(|frame| {
            matches!(
                frame,
                HarnessInputMessage::ExtensionDataRequest(request)
                    if matches!(request.op, ExtensionDataRequestOp::AppendFile { .. })
            )
        })
        .count();
    let results: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                Event::ToolResultReported(result) => Some(result.call_id.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect();

    assert_eq!(appends, 1);
    assert_eq!(results, ["prefixed"]);
}

/// Live session shutdown drops the current attribution before later calls, and
/// replayed papercut starts remain observation-only rather than issuing a
/// second session append.
#[test]
fn papercut_runtime_lifecycle_and_replay_do_not_append() {
    let session_id = tau_proto::SessionId::parse("session-42").expect("session id");
    let mut after_shutdown = papercut_started("after-shutdown", "agent-one", "late call");
    after_shutdown.tool_name = tau_proto::ToolName::new("work_papercut");
    let lifecycle_frames = run_frames(
        true,
        [
            HarnessOutputMessage::Deliver(tau_proto::EventDelivery::live(
                UnixMicros::new(1),
                Event::SessionStarted(tau_proto::SessionStarted {
                    session_id: session_id.clone(),
                    reason: tau_proto::SessionStartReason::Initial,
                }),
            )),
            HarnessOutputMessage::Deliver(tau_proto::EventDelivery::live(
                UnixMicros::new(2),
                Event::SessionShutdown(tau_proto::SessionShutdown { session_id }),
            )),
            HarnessOutputMessage::Deliver(tau_proto::EventDelivery::live(
                UnixMicros::new(3),
                Event::ToolStarted(after_shutdown),
            )),
        ],
    );
    let mut replayed = papercut_started("replayed", "agent-one", "already handled");
    replayed.tool_name = tau_proto::ToolName::new("work_papercut");
    let replay_frames = run_frames(
        true,
        [HarnessOutputMessage::Deliver(tau_proto::EventDelivery {
            event: Box::new(Event::ToolStarted(replayed)),
            replay: true,
            recorded_at: Some(UnixMicros::new(4)),
        })],
    );

    for frames in [&lifecycle_frames, &replay_frames] {
        assert!(frames.iter().all(|frame| !matches!(
            frame,
            HarnessInputMessage::ExtensionDataRequest(request)
                if matches!(request.op, ExtensionDataRequestOp::AppendFile { .. })
        )));
    }
}

/// Invalid configuration must fail closed instead of silently exposing the
/// opt-in diagnostic tool with a misspelled or unsupported setting.
#[test]
fn papercut_config_rejects_unknown_fields() {
    assert!(
        serde_json::from_value::<UtilsConfig>(serde_json::json!({
            "papercut": {"enabled": true}
        }))
        .is_err()
    );
}

/// The model-visible schema and runtime validation jointly bound Unicode scalar
/// count and encoded bytes, rejecting empty reports before any storage request.
#[test]
fn papercut_schema_and_validation_bound_report() {
    let schema = papercut_tool_spec()
        .parameters
        .expect("papercut parameter schema");
    assert_eq!(
        schema
            .pointer("/required/0")
            .and_then(serde_json::Value::as_str),
        Some("report")
    );
    assert_eq!(
        schema
            .pointer("/properties/report/maxLength")
            .and_then(serde_json::Value::as_u64),
        Some(MAX_PAPERCUT_REPORT_CHARS as u64)
    );

    assert!(
        parse_papercut_report(&cbor_map(vec![(
            "report",
            CborValue::Text(" \n".to_owned())
        )]))
        .is_err()
    );
    assert!(
        parse_papercut_report(&cbor_map(vec![(
            "report",
            CborValue::Text("x".repeat(MAX_PAPERCUT_REPORT_CHARS + 1))
        )]))
        .is_err()
    );
    assert!(
        parse_papercut_report(&cbor_map(vec![(
            "report",
            CborValue::Text("é".repeat(MAX_PAPERCUT_REPORT_BYTES / "é".len() + 1))
        )]))
        .is_err()
    );
}

/// One accepted papercut writes exactly one newline-terminated compact JSON
/// record whose attribution comes only from harness-owned tool and session
/// facts, not from model arguments.
#[test]
fn papercut_append_uses_harness_attribution_and_jsonl_newline() {
    let mut rt = runtime();
    rt.session_id = Some("session-42".to_owned());
    let storage = FakePapercutStorage::default();
    let lines = Rc::clone(&storage.lines);
    rt.papercut_storage = Some(Box::new(storage));
    let invoke = papercut_started("call-1", "agent-one", "tool output was confusing");

    assert_eq!(
        rt.record_papercut(&invoke, UnixMicros::new(1_234)),
        "recorded; continue the primary task and do not retry"
    );
    let lines = lines.borrow();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].ends_with(b"\n"));
    let record: serde_json::Value =
        serde_json::from_slice(&lines[0]).expect("valid newline-delimited JSON");
    assert_eq!(record["schema"], PAPERCUT_SCHEMA_VERSION);
    assert_eq!(record["agent_id"], "agent-one");
    assert_eq!(record["session_id"], "session-42");
    assert_eq!(record["timestamp_us"], 1_234);
    assert_eq!(record["report"], "tool output was confusing");
    assert_eq!(record.as_object().expect("record object").len(), 5);
}

/// Failed session append, including ephemeral or quota denials reported by the
/// harness, returns a non-distraction outcome and never creates a retry loop.
#[test]
fn papercut_append_failure_does_not_distract_the_primary_task() {
    let mut rt = runtime();
    rt.session_id = Some("session-42".to_owned());
    let storage = FakePapercutStorage {
        fail: true,
        ..Default::default()
    };
    let lines = Rc::clone(&storage.lines);
    rt.papercut_storage = Some(Box::new(storage));

    let outcome = rt.record_papercut(
        &papercut_started("call-1", "agent-one", "ephemeral session denied storage"),
        UnixMicros::new(1),
    );

    assert!(outcome.starts_with("not recorded:"));
    assert!(outcome.contains("continue the primary task"));
    assert!(outcome.contains("do not retry"));
    assert!(lines.borrow().is_empty());
}

/// Session rollover clears papercut attribution so a late call cannot append a
/// record that falsely claims the previous session.
#[test]
fn papercut_session_lifecycle_drops_old_session_attribution() {
    let mut rt = runtime();
    rt.session_id = Some("old-session".to_owned());
    rt.papercut_storage = Some(Box::new(FakePapercutStorage::default()));

    rt.clear_session_state();

    let outcome = rt.record_papercut(
        &papercut_started("call-1", "agent-one", "late rollover call"),
        UnixMicros::new(1),
    );
    assert!(outcome.contains("no active session"));
}

/// Replay folds timer history only. Replayed papercut calls must not append a
/// second external record after the original live best-effort append.
#[test]
fn replayed_papercut_does_not_duplicate_append() {
    let mut rt = runtime();
    rt.session_id = Some("session-42".to_owned());
    let storage = FakePapercutStorage::default();
    let lines = Rc::clone(&storage.lines);
    rt.papercut_storage = Some(Box::new(storage));
    let invoke = papercut_started("call-1", "agent-one", "one-time report");

    assert!(
        rt.record_papercut(&invoke, UnixMicros::new(1))
            .starts_with("recorded")
    );
    rt.handle_started_replay(&invoke);

    assert_eq!(lines.borrow().len(), 1);
    assert!(rt.pending_invocations.is_empty());
}
