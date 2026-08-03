use tau_proto::PromptMessageClass;

use super::*;

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
