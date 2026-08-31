use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::io::{Cursor, Write};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use tau_proto::{
    AgentPromptSteered, Configure, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage,
    HarnessOutputWriter, PromptMessageClass, UnixMicros,
};

use super::*;

const EXPECTED_PAPERCUT_MODEL_GUIDANCE: &str = "Use this tool only if you encounter an incidental Tau harness, tooling, environment, confusing, or suspicious problem. Record one concise, best-effort report, then continue the primary task. Do not call it merely to state that no problem occurred, and do not retry.";

/// Ensures JSONL decoding constructs only current-schema papercut records and
/// keeps an unsupported schema distinct from malformed typed attribution.
#[test]
fn papercut_record_parser_enforces_the_current_schema() {
    let record = PapercutRecord::parse_json_line(
        r#"{"schema":1,"agent_id":"agent-a","session_id":"session-a","timestamp_us":1,"report":"report"}"#,
    )
    .expect("current record");
    assert_eq!(record.agent_id().as_str(), "agent-a");
    assert_eq!(record.session_id().as_str(), "session-a");
    assert_eq!(record.timestamp_us(), UnixMicros::new(1));
    assert_eq!(record.report(), "report");
    assert_eq!(
        PapercutRecord::parse_json_line(
            r#"{"schema":2,"agent_id":"agent-a","session_id":"session-a","timestamp_us":1,"report":"report"}"#
        ),
        Err(PapercutRecordParseError::UnsupportedSchema)
    );
    assert_eq!(
        PapercutRecordParseError::UnsupportedSchema.to_string(),
        "papercut record uses an unsupported schema"
    );
    assert_eq!(
        PapercutRecord::parse_json_line(
            r#"{"schema":1,"agent_id":"agent/a","session_id":"session-a","timestamp_us":1,"report":"report"}"#
        ),
        Err(PapercutRecordParseError::Invalid)
    );
}

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
        invocation_policy: tau_proto::ToolInvocationPolicy::default(),
        call_id: tau_proto::ToolCallId::new(call_id),
        tool_name: tau_proto::ToolName::new(TIMER_TOOL_NAME),
        arguments: args,
        agent_id: AgentId::parse(agent).expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    }
}

/// Deterministic UTC host-timezone source for timer runtime tests.
struct FixedHostTimezoneProvider;

impl HostTimezoneProvider for FixedHostTimezoneProvider {
    fn current_timezone(&self) -> Result<TimeZone, String> {
        Ok(TimeZone::UTC)
    }
}

/// Scripted host-timezone source for refresh and failure-recovery tests.
struct ScriptedHostTimezoneProvider {
    /// Ordered results returned by refresh attempts.
    results: Rc<RefCell<VecDeque<Result<TimeZone, String>>>>,
}

fn central_europe_timezone() -> TimeZone {
    TimeZone::posix("CET-1CEST,M3.5.0,M10.5.0/3").expect("Central European timezone")
}

fn eastern_timezone() -> TimeZone {
    TimeZone::posix("EST5EDT,M3.2.0,M11.1.0").expect("Eastern timezone")
}

fn japan_timezone() -> TimeZone {
    TimeZone::posix("JST-9").expect("Japan timezone")
}

impl HostTimezoneProvider for ScriptedHostTimezoneProvider {
    fn current_timezone(&self) -> Result<TimeZone, String> {
        self.results
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| Err("scripted timezone results exhausted".to_owned()))
    }
}

fn runtime() -> TimerRuntime {
    runtime_with_timezone_provider(Box::new(FixedHostTimezoneProvider))
}

fn runtime_with_timezone_provider(
    timezone_provider: Box<dyn HostTimezoneProvider>,
) -> TimerRuntime {
    TimerRuntime {
        handle: None,
        timers: HashMap::new(),
        timezone_provider,
        local_timezone: None,
        timezone_checked_at: None,
        pending_invocations: HashMap::new(),
        replay_complete_agents: HashSet::new(),
        reported_timer_agents: HashSet::new(),
        timer_tool_name: None,
        papercut_tool_name: None,
        session_id: None,
        papercut_storage: None,
    }
}

/// Invoke the normal live schedule mutation path in focused state tests.
fn schedule_timer(
    runtime: &mut TimerRuntime,
    agent_id: &AgentId,
    args: ScheduleArgs,
    now: UnixMicros,
) -> Result<CborValue, String> {
    runtime.schedule_timer(agent_id, args, now, ScheduleMutationSource::Live)
}

/// Deterministic in-memory append target for papercut unit tests.
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
            return Err("permission: memory-only storage unavailable".to_owned());
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
/// repetition and expose the closed daily wall-clock input.
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
    assert_eq!(
        parameters
            .pointer("/properties/daily_time/pattern")
            .and_then(serde_json::Value::as_str),
        Some("^([01][0-9]|2[0-3]):[0-5][0-9]$")
    );
    assert_eq!(
        parameters
            .pointer("/properties/utc/type")
            .and_then(serde_json::Value::as_str),
        Some("boolean")
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
    rt.handle_started_replay(&start, Some(base));
    rt.handle_result_replay(&ToolResult {
        presentation: Default::default(),
        call_id: start.call_id.clone(),
        tool_name: start.tool_name.clone(),
        tool_type: ToolType::Function,
        result: CborValue::Text("ok".to_owned()),
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    });

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

/// Replay terminal correlation keeps the typed call id from the start through
/// result and error handling, leaving unknown terminals inert and preserving
/// the established fallback timer id.
#[test]
fn replay_terminal_correlation_preserves_typed_call_ids_and_fallback_timer_ids() {
    let mut rt = runtime();
    let agent = AgentId::parse("agent-one").expect("agent id");
    let first = started(
        "call/one?",
        agent.as_ref(),
        cbor_map(vec![
            ("action", CborValue::Text("schedule".to_owned())),
            ("delay_seconds", CborValue::Integer(10.into())),
            ("message", CborValue::Text("wake".to_owned())),
        ]),
    );
    let second = started(
        "call-error",
        agent.as_ref(),
        cbor_map(vec![
            ("action", CborValue::Text("schedule".to_owned())),
            ("timer_id", CborValue::Text("discarded".to_owned())),
            ("delay_seconds", CborValue::Integer(10.into())),
            ("message", CborValue::Text("wake".to_owned())),
        ]),
    );

    rt.handle_started_replay(&first, Some(UnixMicros::new(0)));
    rt.handle_started_replay(&second, Some(UnixMicros::new(0)));
    assert_eq!(
        rt.pending_invocations
            .get(&first.call_id)
            .expect("first pending invocation")
            .call_id,
        first.call_id
    );

    let unknown = ToolCallId::new("unknown");
    rt.handle_error_replay(&unknown);
    assert_eq!(rt.pending_invocations.len(), 2);

    rt.handle_error_replay(&second.call_id);
    rt.handle_result_replay(&ToolResult {
        presentation: Default::default(),
        call_id: second.call_id.clone(),
        tool_name: second.tool_name.clone(),
        tool_type: ToolType::Function,
        result: CborValue::Text("ok".to_owned()),
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    });
    assert!(!rt.timers.contains_key(&TimerKey {
        agent_id: agent.clone(),
        timer_id: "discarded".to_owned(),
    }));

    rt.handle_result_replay(&ToolResult {
        presentation: Default::default(),
        call_id: first.call_id.clone(),
        tool_name: first.tool_name.clone(),
        tool_type: ToolType::Function,
        result: CborValue::Text("ok".to_owned()),
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    });
    assert!(rt.pending_invocations.is_empty());
    assert!(rt.timers.contains_key(&TimerKey {
        agent_id: agent,
        timer_id: "call-call-one-".to_owned(),
    }));
}

/// Periodic timers coalesce missed downtime into one prompt and advance
/// past now.
#[test]
fn periodic_timer_coalesces_missed_intervals() {
    let mut rt = runtime();
    let agent = AgentId::parse("agent-one").expect("agent id");
    rt.replay_complete_agents.insert(agent.clone());
    schedule_timer(
        &mut rt,
        &agent,
        ScheduleArgs {
            timer_id: "tick".to_owned(),
            timing: ScheduleTiming::Relative {
                delay_seconds: 1,
                interval_seconds: Some(10),
            },
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
    assert!(UnixMicros::new(35_000_000) < timer.next_fire_at().expect("resolved deadline"));
}

/// Daily UTC timers coalesce every overdue wall-clock occurrence into one
/// wakeup and preserve the next requested time of day.
#[test]
fn daily_timer_coalesces_overdue_occurrences() {
    let mut rt = runtime();
    let agent = AgentId::parse("agent-one").expect("agent id");
    rt.replay_complete_agents.insert(agent.clone());
    let scheduled_at = UnixMicros::new(1_767_254_400_000_000); // 2026-01-01 08:00Z
    schedule_timer(
        &mut rt,
        &agent,
        ScheduleArgs {
            timer_id: "agenda".to_owned(),
            timing: ScheduleTiming::Daily(
                DailySchedule::parse("08:00", WallClockZone::Utc).expect("daily schedule"),
            ),
            message: "prepare agenda".to_owned(),
        },
        scheduled_at,
    )
    .expect("schedule");

    let fires = rt.collect_due(UnixMicros::new(1_767_517_200_000_000)); // Jan 4 09:00Z

    assert_eq!(fires.len(), 1);
    assert!(fires[0].prompt.contains("Coalesced 3 missed"));
    let timer = rt
        .timers
        .get(&TimerKey {
            agent_id: agent,
            timer_id: "agenda".to_owned(),
        })
        .expect("daily timer retained");
    assert_eq!(
        timer.next_fire_at(),
        Some(UnixMicros::new(1_767_600_000_000_000)) // Jan 5 08:00Z
    );
}

/// Daily schedule parsing requires one timing mode and keeps UTC scoped to
/// wall-clock schedules without changing relative arguments.
#[test]
fn daily_schedule_arguments_are_closed_and_mutually_exclusive() {
    let daily = cbor_map(vec![
        ("action", CborValue::Text("schedule".to_owned())),
        ("timer_id", CborValue::Text("agenda".to_owned())),
        ("daily_time", CborValue::Text("08:00".to_owned())),
        ("utc", CborValue::Bool(true)),
        ("message", CborValue::Text("prepare agenda".to_owned())),
    ]);
    let parsed = parse_action(&daily, "call-daily").expect("daily action");
    assert!(matches!(
        parsed,
        TimerAction::Schedule(ScheduleArgs {
            timing: ScheduleTiming::Daily(_),
            ..
        })
    ));

    let both = cbor_map(vec![
        ("action", CborValue::Text("schedule".to_owned())),
        ("daily_time", CborValue::Text("08:00".to_owned())),
        ("delay_seconds", CborValue::Integer(60.into())),
        ("message", CborValue::Text("invalid".to_owned())),
    ]);
    assert!(parse_action(&both, "call-both").is_err());

    let relative_utc = cbor_map(vec![
        ("action", CborValue::Text("schedule".to_owned())),
        ("delay_seconds", CborValue::Integer(60.into())),
        ("utc", CborValue::Bool(false)),
        ("message", CborValue::Text("invalid".to_owned())),
    ]);
    assert!(parse_action(&relative_utc, "call-relative").is_err());
}

/// One cadence-bounded timezone snapshot preserves an already-due occurrence
/// across a zone change and a backward clock move cannot repeat it.
#[test]
fn local_timezone_refresh_preserves_due_and_backward_clock_progress() {
    let results = Rc::new(RefCell::new(VecDeque::from([
        Ok(central_europe_timezone()),
        Ok(eastern_timezone()),
        Ok(japan_timezone()),
    ])));
    let mut rt = runtime_with_timezone_provider(Box::new(ScriptedHostTimezoneProvider {
        results: Rc::clone(&results),
    }));
    let agent = AgentId::parse("agent-one").expect("agent id");
    rt.replay_complete_agents.insert(agent.clone());
    let scheduled_at = UnixMicros::new(1_767_225_600_000_000); // 2026-01-01 00:00Z
    schedule_timer(
        &mut rt,
        &agent,
        ScheduleArgs {
            timer_id: "local".to_owned(),
            timing: ScheduleTiming::Daily(
                DailySchedule::parse("08:00", WallClockZone::Local).expect("daily schedule"),
            ),
            message: "wake".to_owned(),
        },
        scheduled_at,
    )
    .expect("schedule");

    let due = UnixMicros::new(1_767_250_860_000_000); // 2026-01-01 07:01Z
    rt.timezone_checked_at = Instant::now().checked_sub(Duration::from_secs(60));
    assert_eq!(rt.collect_due(due).len(), 1);
    let backward = UnixMicros::new(1_767_247_200_000_000); // 2026-01-01 06:00Z
    rt.timezone_checked_at = Instant::now().checked_sub(Duration::from_secs(60));
    assert!(rt.collect_due(backward).is_empty());
    let timer = rt
        .timers
        .get(&TimerKey {
            agent_id: agent,
            timer_id: "local".to_owned(),
        })
        .expect("daily timer");
    assert!(due < timer.next_fire_at().expect("resolved deadline"));
    assert!(results.borrow().is_empty());
}

/// Host timezone lookup occurs once at scheduling and not again before the
/// 60-second refresh boundary, then applies the next shared snapshot.
#[test]
fn local_timezone_refresh_obeys_sixty_second_cadence() {
    let results = Rc::new(RefCell::new(VecDeque::from([
        Ok(central_europe_timezone()),
        Ok(eastern_timezone()),
    ])));
    let mut rt = runtime_with_timezone_provider(Box::new(ScriptedHostTimezoneProvider {
        results: Rc::clone(&results),
    }));
    let agent = AgentId::parse("agent-one").expect("agent id");
    rt.replay_complete_agents.insert(agent.clone());
    let start = UnixMicros::new(1_767_225_600_000_000);
    schedule_timer(
        &mut rt,
        &agent,
        ScheduleArgs {
            timer_id: "cadence".to_owned(),
            timing: ScheduleTiming::Daily(
                DailySchedule::parse("08:00", WallClockZone::Local).expect("daily schedule"),
            ),
            message: "wake".to_owned(),
        },
        start,
    )
    .expect("schedule");

    assert!(rt.collect_due(add_seconds(start, 59)).is_empty());
    assert_eq!(results.borrow().len(), 1);
    rt.timezone_checked_at = Instant::now().checked_sub(Duration::from_secs(60));
    assert!(rt.collect_due(add_seconds(start, 60)).is_empty());
    assert!(results.borrow().is_empty());
    assert_eq!(
        rt.timers
            .get(&TimerKey {
                agent_id: agent,
                timer_id: "cadence".to_owned(),
            })
            .expect("timer")
            .next_fire_at(),
        Some(UnixMicros::new(1_767_272_400_000_000)) // Jan 1 13:00Z
    );
}

/// An accepted local schedule remains reconstructable when timezone lookup is
/// transiently unavailable and becomes due after a later shared refresh.
#[test]
fn restored_local_timer_recovers_after_timezone_lookup_failure() {
    let results = Rc::new(RefCell::new(VecDeque::from([
        Err("timezone unavailable".to_owned()),
        Ok(central_europe_timezone()),
    ])));
    let mut rt = runtime_with_timezone_provider(Box::new(ScriptedHostTimezoneProvider {
        results: Rc::clone(&results),
    }));
    let agent = AgentId::parse("agent-one").expect("agent id");
    let started_at = UnixMicros::new(1_767_225_600_000_000); // 2026-01-01 00:00Z
    let start = started(
        "call-local",
        agent.as_ref(),
        cbor_map(vec![
            ("action", CborValue::Text("schedule".to_owned())),
            ("timer_id", CborValue::Text("local".to_owned())),
            ("daily_time", CborValue::Text("08:00".to_owned())),
            ("message", CborValue::Text("wake".to_owned())),
        ]),
    );
    rt.handle_started_replay(&start, Some(started_at));
    rt.handle_result_replay(&ToolResult {
        presentation: Default::default(),
        call_id: start.call_id.clone(),
        tool_name: start.tool_name.clone(),
        tool_type: ToolType::Function,
        result: CborValue::Text("ok".to_owned()),
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    });
    rt.replay_complete_agents.insert(agent.clone());
    rt.timezone_checked_at = None;

    let fires = rt.collect_due(UnixMicros::new(1_767_344_400_000_000)); // Jan 2 09:00Z

    assert_eq!(fires.len(), 1);
    assert!(fires[0].prompt.contains("Coalesced 2 missed"));
    assert!(results.borrow().is_empty());
}

/// Replay uses the recorded tool-start timestamp, so a result committed just
/// after the requested minute cannot move the first firing to tomorrow.
#[test]
fn replay_schedule_anchor_matches_live_across_exact_minute_boundary() {
    let mut rt = runtime();
    let agent = AgentId::parse("agent-one").expect("agent id");
    let started_at = UnixMicros::new(1_767_254_399_999_000); // 07:59:59.999Z
    let start = started(
        "call-boundary",
        agent.as_ref(),
        cbor_map(vec![
            ("action", CborValue::Text("schedule".to_owned())),
            ("timer_id", CborValue::Text("boundary".to_owned())),
            ("daily_time", CborValue::Text("08:00".to_owned())),
            ("utc", CborValue::Bool(true)),
            ("message", CborValue::Text("wake".to_owned())),
        ]),
    );
    rt.handle_started_replay(&start, Some(started_at));
    rt.handle_result_replay(&ToolResult {
        presentation: Default::default(),
        call_id: start.call_id,
        tool_name: start.tool_name,
        tool_type: ToolType::Function,
        result: CborValue::Text("ok".to_owned()),
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    });

    assert_eq!(
        rt.timers
            .get(&TimerKey {
                agent_id: agent,
                timer_id: "boundary".to_owned(),
            })
            .expect("timer")
            .next_fire_at(),
        Some(UnixMicros::new(1_767_254_400_000_000))
    );
}

/// Replayed daily schedule and canonical prompt facts advance the same timer to
/// the next wall-clock occurrence without waiting for live firing.
#[test]
fn replayed_daily_prompt_advances_reconstructed_schedule() {
    let mut rt = runtime();
    let agent = AgentId::parse("agent-one").expect("agent id");
    let started_at = UnixMicros::new(1_767_225_600_000_000); // Jan 1 00:00Z
    let start = started(
        "call-replay-daily",
        agent.as_ref(),
        cbor_map(vec![
            ("action", CborValue::Text("schedule".to_owned())),
            ("timer_id", CborValue::Text("daily".to_owned())),
            ("daily_time", CborValue::Text("08:00".to_owned())),
            ("utc", CborValue::Bool(true)),
            ("message", CborValue::Text("wake".to_owned())),
        ]),
    );
    rt.handle_started_replay(&start, Some(started_at));
    rt.handle_result_replay(&ToolResult {
        presentation: Default::default(),
        call_id: start.call_id,
        tool_name: start.tool_name,
        tool_type: ToolType::Function,
        result: CborValue::Text("ok".to_owned()),
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    });
    rt.handle_prompt_replay(
        &AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent.clone(),
            text: "Timer `daily` fired: wake".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: PromptMessageClass::Internal,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: Some("timer:daily:1".to_owned()),
        },
        Some(UnixMicros::new(1_767_254_400_000_000)),
    );

    assert_eq!(
        rt.timers
            .get(&TimerKey {
                agent_id: agent,
                timer_id: "daily".to_owned(),
            })
            .expect("timer")
            .next_fire_at(),
        Some(UnixMicros::new(1_767_340_800_000_000))
    );
}

/// Replayed timer-fired prompts remove one-shot timers so they are not
/// fired again.
#[test]
fn replayed_timer_prompt_removes_one_shot() {
    let mut rt = runtime();
    let agent = AgentId::parse("agent-one").expect("agent id");
    rt.replay_complete_agents.insert(agent.clone());
    schedule_timer(
        &mut rt,
        &agent,
        ScheduleArgs {
            timer_id: "once".to_owned(),
            timing: ScheduleTiming::Relative {
                delay_seconds: 1,
                interval_seconds: None,
            },
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
            trusted_internal_spans: Vec::new(),
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
    schedule_timer(
        &mut rt,
        &agent,
        ScheduleArgs {
            timer_id: "old".to_owned(),
            timing: ScheduleTiming::Relative {
                delay_seconds: 10,
                interval_seconds: None,
            },
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
    schedule_timer(
        &mut rt,
        &agent,
        ScheduleArgs {
            timer_id: "dormant".to_owned(),
            timing: ScheduleTiming::Relative {
                delay_seconds: 10,
                interval_seconds: None,
            },
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
    schedule_timer(
        &mut rt,
        &agent,
        ScheduleArgs {
            timer_id: "busy".to_owned(),
            timing: ScheduleTiming::Relative {
                delay_seconds: 10,
                interval_seconds: None,
            },
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
        trusted_internal_spans: Vec::new(),
        message_class: PromptMessageClass::Internal,
        internal_kind: None,
        ctx_id: Some("timer:busy:1".to_owned()),
    };
    let submitted = AgentPromptSubmitted {
        inference_activation: false,
        agent_id: steered.agent_id.clone(),
        text: steered.text,
        trusted_internal_spans: Vec::new(),
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
    schedule_timer(
        &mut rt,
        &agent,
        ScheduleArgs {
            timer_id: "bad-restore".to_owned(),
            timing: ScheduleTiming::Relative {
                delay_seconds: 10,
                interval_seconds: None,
            },
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

/// Scheduling keeps duplicate ids distinct from exact per-agent and session
/// capacity boundaries, so no accepted timer is implicitly replaced or lost.
#[test]
fn schedule_rejects_duplicate_ids_and_enforces_capacity_boundaries() {
    let mut rt = runtime();
    let agents = ["agent-one", "agent-two", "agent-three", "agent-four"]
        .map(|id| AgentId::parse(id).expect("agent id"));
    let args = |timer_id| ScheduleArgs {
        timer_id,
        timing: ScheduleTiming::Relative {
            delay_seconds: 10,
            interval_seconds: None,
        },
        message: "wake".to_owned(),
    };

    schedule_timer(
        &mut rt,
        &agents[0],
        args("same".to_owned()),
        UnixMicros::new(0),
    )
    .expect("first schedule");
    assert_eq!(
        schedule_timer(
            &mut rt,
            &agents[0],
            args("same".to_owned()),
            UnixMicros::new(0),
        )
        .expect_err("duplicate id"),
        "timer `same` is already active; cancel it before scheduling a replacement"
    );

    for timer_index in 1..MAX_TIMERS_PER_AGENT {
        schedule_timer(
            &mut rt,
            &agents[0],
            args(format!("timer-0-{timer_index}")),
            UnixMicros::new(0),
        )
        .expect("timer within per-agent capacity");
    }
    assert_eq!(
        schedule_timer(
            &mut rt,
            &agents[0],
            args("per-agent-over".to_owned()),
            UnixMicros::new(0),
        )
        .expect_err("per-agent capacity"),
        format!("timer limit exceeded: at most {MAX_TIMERS_PER_AGENT} active timers per agent")
    );
    let mut remaining = MAX_TIMERS_TOTAL - rt.timers.len();
    for (agent_index, agent) in agents.iter().enumerate().skip(1) {
        let timer_count = remaining.min(MAX_TIMERS_PER_AGENT);
        for timer_index in 0..timer_count {
            schedule_timer(
                &mut rt,
                agent,
                args(format!("timer-{agent_index}-{timer_index}")),
                UnixMicros::new(0),
            )
            .expect("timer within session capacity");
        }
        remaining -= timer_count;
        if remaining == 0 {
            break;
        }
    }
    assert_eq!(remaining, 0, "four agents must reach the session capacity");
    assert_eq!(rt.timers.len(), MAX_TIMERS_TOTAL);
    assert_eq!(
        schedule_timer(
            &mut rt,
            &AgentId::parse("agent-five").expect("agent id"),
            args("session-over".to_owned()),
            UnixMicros::new(0),
        )
        .expect_err("session capacity"),
        format!("timer limit exceeded: at most {MAX_TIMERS_TOTAL} active timers per session")
    );
}

/// Daily timer display and list output state the wall clock and whether it uses
/// UTC, instead of presenting it as a drifting fixed interval.
#[test]
fn timer_display_and_list_summarize_daily_utc_schedule() {
    let args = cbor_map(vec![
        ("action", CborValue::Text("schedule".to_owned())),
        ("timer_id", CborValue::Text("agenda".to_owned())),
        ("daily_time", CborValue::Text("08:00".to_owned())),
        ("utc", CborValue::Bool(true)),
        ("message", CborValue::Text("prepare agenda".to_owned())),
    ]);
    assert_eq!(
        timer_display_args(&args, "call-daily"),
        "schedule agenda daily at 08:00 UTC"
    );

    let mut rt = runtime();
    let agent = AgentId::parse("agent-one").expect("agent id");
    schedule_timer(
        &mut rt,
        &agent,
        ScheduleArgs {
            timer_id: "agenda".to_owned(),
            timing: ScheduleTiming::Daily(
                DailySchedule::parse("08:00", WallClockZone::Utc).expect("daily schedule"),
            ),
            message: "prepare agenda".to_owned(),
        },
        UnixMicros::new(1_767_254_400_000_000),
    )
    .expect("schedule");
    assert_eq!(
        rt.list_timers(&agent, UnixMicros::new(1_767_254_400_000_000))
            .result,
        CborValue::Text("agenda: due in 86400s, daily at 08:00 UTC".to_owned())
    );
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
            ("interval_seconds", CborValue::Integer(3600.into())),
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
    assert_eq!(scheduled.display.args, "schedule standup in 10m every 1h");
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
        CborValue::Text("standup: due in 600s, repeats every 3600s".to_owned())
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
    assert_eq!(cancelled.display.args, "cancel standup");
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

/// Timer validation accepts exactly the path-safe identifier boundary and
/// rejects each invalid grammar or size class with its stable error.
#[test]
fn timer_id_validation_enforces_boundaries() {
    let action_args = |action: &str, timer_id: String| {
        let mut entries = vec![
            ("action", CborValue::Text(action.to_owned())),
            ("timer_id", CborValue::Text(timer_id)),
        ];
        if action == "schedule" {
            entries.extend([
                ("delay_seconds", CborValue::Integer(10.into())),
                ("message", CborValue::Text("hello".to_owned())),
            ]);
        }
        cbor_map(entries)
    };

    for (action, timer_id, expected) in [
        (
            "schedule",
            String::new(),
            format!("timer_id must be 1..={MAX_TIMER_ID_BYTES} bytes"),
        ),
        (
            "cancel",
            String::new(),
            format!("timer_id must be 1..={MAX_TIMER_ID_BYTES} bytes"),
        ),
        (
            "schedule",
            "../bad".to_owned(),
            "timer_id must contain only ASCII letters, digits, '_' or '-'".to_owned(),
        ),
        (
            "schedule",
            "bad/id".to_owned(),
            "timer_id must contain only ASCII letters, digits, '_' or '-'".to_owned(),
        ),
        (
            "schedule",
            "bad.id".to_owned(),
            "timer_id must contain only ASCII letters, digits, '_' or '-'".to_owned(),
        ),
        (
            "schedule",
            "é".to_owned(),
            "timer_id must contain only ASCII letters, digits, '_' or '-'".to_owned(),
        ),
        (
            "schedule",
            "x".repeat(MAX_TIMER_ID_BYTES + 1),
            format!("timer_id must be 1..={MAX_TIMER_ID_BYTES} bytes"),
        ),
        (
            "cancel",
            "x".repeat(MAX_TIMER_ID_BYTES + 1),
            format!("timer_id must be 1..={MAX_TIMER_ID_BYTES} bytes"),
        ),
    ] {
        assert_eq!(
            parse_action(&action_args(action, timer_id), "call-1").expect_err("invalid timer id"),
            expected
        );
    }

    let max_id = format!("Az09_-{}", "x".repeat(MAX_TIMER_ID_BYTES - 6));
    assert!(parse_action(&action_args("schedule", max_id.clone()), "call-1").is_ok());
    assert!(parse_action(&action_args("cancel", max_id), "call-1").is_ok());
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
        invocation_policy: tau_proto::ToolInvocationPolicy::default(),
        call_id: tau_proto::ToolCallId::new(call_id),
        tool_name: tau_proto::ToolName::new(PAPERCUT_TOOL_NAME),
        arguments: cbor_map(vec![("report", CborValue::Text(report.to_owned()))]),
        agent_id: AgentId::parse(agent).expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    }
}

/// Ensures the enabled reporter presents one exact conditional instruction in
/// both model-visible surfaces, preventing an unconditional or no-problem call.
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
    assert_eq!(
        papercut
            .prompt_fragment
            .as_ref()
            .expect("papercut prompt")
            .template
            .as_str(),
        EXPECTED_PAPERCUT_MODEL_GUIDANCE
    );
    assert_eq!(
        papercut.tool.description.as_deref(),
        Some(EXPECTED_PAPERCUT_MODEL_GUIDANCE)
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

/// A live, prefixed papercut call after `session.started` reaches the shared
/// per-instance User-storage path, while an unprefixed lookalike remains
/// unhandled by the dynamically scoped runtime dispatch.
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

    let appends: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::ExtensionDataRequest(request)
                if matches!(request.op, ExtensionDataRequestOp::AppendFile { .. }) =>
            {
                Some(request)
            }
            _ => None,
        })
        .collect();
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

    assert_eq!(appends.len(), 1);
    assert_eq!(appends[0].scope, ExtensionDataScope::User);
    assert_eq!(results, ["prefixed"]);
}

/// Live schedule and cancel mutations must publish complete transient timer
/// indicator replacements instead of keeping the timer tool call active.
#[test]
fn timer_runtime_declares_and_retracts_scheduled_indicator() {
    let mut schedule = started(
        "schedule",
        "agent-one",
        cbor_map(vec![
            ("action", CborValue::Text("schedule".to_owned())),
            ("timer_id", CborValue::Text("wake".to_owned())),
            ("delay_seconds", CborValue::Integer(60.into())),
            ("message", CborValue::Text("wake up".to_owned())),
        ]),
    );
    schedule.tool_name = tau_proto::ToolName::new("work_timer");
    let mut cancel = started(
        "cancel",
        "agent-one",
        cbor_map(vec![
            ("action", CborValue::Text("cancel".to_owned())),
            ("timer_id", CborValue::Text("wake".to_owned())),
        ]),
    );
    cancel.tool_name = tau_proto::ToolName::new("work_timer");
    let frames = run_frames(
        false,
        [
            HarnessOutputMessage::Deliver(tau_proto::EventDelivery::live(
                UnixMicros::new(1),
                Event::SessionStarted(tau_proto::SessionStarted {
                    session_id: tau_proto::SessionId::parse("session-42").expect("session id"),
                    reason: tau_proto::SessionStartReason::Initial,
                }),
            )),
            HarnessOutputMessage::Deliver(tau_proto::EventDelivery::live(
                UnixMicros::new(2),
                Event::ToolStarted(schedule),
            )),
            HarnessOutputMessage::Deliver(tau_proto::EventDelivery::live(
                UnixMicros::new(3),
                Event::ToolStarted(cancel),
            )),
        ],
    );

    let declarations = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                Event::AgentRuntimeIndicatorsDeclared(declaration) => {
                    assert!(!emit.persist);
                    Some(declaration.indicators.clone())
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        declarations,
        [
            vec![tau_proto::AgentRuntimeIndicator::TimerScheduled],
            vec![]
        ]
    );
}

/// Successful replay reconstruction must publish timer presence after the
/// agent boundary rather than while historical facts are still arriving.
#[test]
fn timer_runtime_replay_declares_reconstructed_presence() {
    let agent = AgentId::parse("agent-one").expect("agent id");
    let mut start = started(
        "replay-schedule",
        agent.as_ref(),
        cbor_map(vec![
            ("action", CborValue::Text("schedule".to_owned())),
            ("timer_id", CborValue::Text("wake".to_owned())),
            ("delay_seconds", CborValue::Integer(60.into())),
            ("message", CborValue::Text("wake up".to_owned())),
        ]),
    );
    start.tool_name = tau_proto::ToolName::new("work_timer");
    let result = ToolResult {
        presentation: Default::default(),
        call_id: start.call_id.clone(),
        tool_name: start.tool_name.clone(),
        tool_type: ToolType::Function,
        result: CborValue::Text("ok".to_owned()),
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    };
    let frames = run_frames(
        false,
        [
            HarnessOutputMessage::Deliver(tau_proto::EventDelivery::replay(
                UnixMicros::new(1),
                Event::ToolStarted(start),
            )),
            HarnessOutputMessage::Deliver(tau_proto::EventDelivery::replay(
                UnixMicros::new(2),
                Event::ToolResult(result),
            )),
            HarnessOutputMessage::Deliver(tau_proto::EventDelivery::live(
                UnixMicros::new(3),
                Event::AgentReplayComplete(AgentReplayComplete {
                    agent_id: agent,
                    session_id: None,
                    error: None,
                }),
            )),
        ],
    );

    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::Emit(emit)
            if !emit.persist
                && matches!(
                    emit.event.as_ref(),
                    Event::AgentRuntimeIndicatorsDeclared(declaration)
                        if declaration.indicators
                            == [tau_proto::AgentRuntimeIndicator::TimerScheduled]
                )
    )));
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

/// Ensures the model-visible schema retains its bounded report contract while
/// parsing accepts both exact limits and rejects blank or one-over reports.
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
    assert_eq!(
        papercut_tool_spec().description.as_deref(),
        Some(EXPECTED_PAPERCUT_MODEL_GUIDANCE)
    );

    let parse = |report: String| {
        parse_papercut_report(&cbor_map(vec![("report", CborValue::Text(report))]))
    };
    let report_at_scalar_limit = "x".repeat(MAX_PAPERCUT_REPORT_CHARS);
    let report_at_byte_limit = "\u{10ffff}".repeat(MAX_PAPERCUT_REPORT_CHARS);
    assert_eq!(report_at_byte_limit.len(), MAX_PAPERCUT_REPORT_BYTES);
    assert!(parse(report_at_scalar_limit).is_ok());
    assert!(parse(report_at_byte_limit.clone()).is_ok());

    assert_eq!(
        parse(" \n".to_owned()).expect_err("blank report"),
        "report must not be empty"
    );
    assert_eq!(
        parse("x".repeat(MAX_PAPERCUT_REPORT_CHARS + 1)).expect_err("one scalar over"),
        format!(
            "report must contain at most {MAX_PAPERCUT_REPORT_CHARS} Unicode scalars and {MAX_PAPERCUT_REPORT_BYTES} bytes"
        )
    );
    assert_eq!(
        parse(format!("{report_at_byte_limit}x")).expect_err("one byte over"),
        format!(
            "report must contain at most {MAX_PAPERCUT_REPORT_CHARS} Unicode scalars and {MAX_PAPERCUT_REPORT_BYTES} bytes"
        )
    );
}

/// One accepted papercut writes exactly one newline-terminated compact JSON
/// record whose attribution comes only from harness-owned tool and session
/// facts, not from model arguments.
#[test]
fn papercut_append_uses_harness_attribution_and_jsonl_newline() {
    let mut rt = runtime();
    rt.session_id = Some(tau_proto::SessionId::parse("session-42").expect("session id"));
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
    assert_eq!(
        lines[0],
        b"{\"schema\":1,\"agent_id\":\"agent-one\",\"session_id\":\"session-42\",\"timestamp_us\":1234,\"report\":\"tool output was confusing\"}\n"
    );
}

/// A call without the current harness session remains a best-effort no-op and
/// must not reach storage with model-supplied or stale attribution.
#[test]
fn papercut_without_active_session_does_not_append() {
    let mut rt = runtime();
    let storage = FakePapercutStorage::default();
    let lines = Rc::clone(&storage.lines);
    rt.papercut_storage = Some(Box::new(storage));

    assert_eq!(
        rt.record_papercut(
            &papercut_started("call-1", "agent-one", "missing lifecycle state"),
            UnixMicros::new(1),
        ),
        "not recorded: no active session is available; continue the primary task and do not retry"
    );
    assert!(lines.borrow().is_empty());
}

/// A failed append, including memory-only, quota, or RPC rejection, returns a
/// non-distraction outcome and never creates a retry loop.
#[test]
fn papercut_append_failure_does_not_distract_the_primary_task() {
    let mut rt = runtime();
    rt.session_id = Some(tau_proto::SessionId::parse("session-42").expect("session id"));
    let storage = FakePapercutStorage {
        fail: true,
        ..Default::default()
    };
    let lines = Rc::clone(&storage.lines);
    rt.papercut_storage = Some(Box::new(storage));

    let outcome = rt.record_papercut(
        &papercut_started("call-1", "agent-one", "memory-only storage denied"),
        UnixMicros::new(1),
    );

    assert!(outcome.starts_with("not recorded:"));
    assert!(outcome.contains("continue the primary task"));
    assert!(outcome.contains("do not retry"));
    assert!(lines.borrow().is_empty());
}

/// Replay folds timer history only. Replayed papercut calls must not append a
/// second external record after the original live best-effort append.
#[test]
fn replayed_papercut_does_not_duplicate_append() {
    let mut rt = runtime();
    rt.session_id = Some(tau_proto::SessionId::parse("session-42").expect("session id"));
    let storage = FakePapercutStorage::default();
    let lines = Rc::clone(&storage.lines);
    rt.papercut_storage = Some(Box::new(storage));
    let invoke = papercut_started("call-1", "agent-one", "one-time report");

    assert!(
        rt.record_papercut(&invoke, UnixMicros::new(1))
            .starts_with("recorded")
    );
    rt.handle_started_replay(&invoke, None);

    assert_eq!(lines.borrow().len(), 1);
    assert!(rt.pending_invocations.is_empty());
}
