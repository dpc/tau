use super::*;
use crate::harness::PendingTool;

fn prompt_created_count(h: &Harness) -> u64 {
    let mut cursor = crate::event_log::EventLogSeq::new(0);
    let mut count = 0;
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq.next();
        if matches!(entry.event, Event::AgentPromptCreated(_)) {
            count += 1;
        }
    }
    count
}

/// Sink that rejects intercepted frames to exercise failed-delivery recovery.
struct FailingInterceptSink;

impl ConnectionSink for FailingInterceptSink {
    fn send(&mut self, _event: RoutedFrame) -> Result<(), ConnectionSendError> {
        Err(ConnectionSendError::new("test sink closed"))
    }
}

fn connect_named_test_tool(
    h: &mut Harness,
    connection_id: &str,
    component_name: &str,
) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    h.bus.connect(Connection::new(
        ConnectionMetadata {
            id: connection_id.into(),
            name: component_name.to_owned(),
            kind: tau_proto::ClientKind::Tool,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(TestSink {
            events: Arc::clone(&events),
        }),
    ));
    events
}

fn connect_failing_test_tool(h: &mut Harness, name: &str) {
    h.bus.connect(Connection::new(
        ConnectionMetadata {
            id: name.into(),
            name: name.to_owned(),
            kind: tau_proto::ClientKind::Tool,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(FailingInterceptSink),
    ));
}

#[test]
fn interception_exact_selector_intercepts_before_log() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "interceptor");
    let start_seq = h.event_log.next_seq();

    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("held"));

    let (event, transient) = intercepted_payload(&interceptor);
    assert_eq!(event, draft_event("held"));
    assert!(
        transient,
        "UiPromptDraft default transient flag is preserved"
    );
    assert_eq!(h.event_log.next_seq(), after_registration_seq);
    assert!(after_registration_seq.get() < start_seq.get() + 2);
}

#[test]
fn interception_drop_prevents_final_delivery() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    // UiPromptDraft is not on the must-pass list, so an explicit Drop
    // really does drop it.
    h.publish_event(None, draft_event("dropped"));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    assert_eq!(h.event_log.next_seq(), after_registration_seq);
}

#[test]
fn interception_pass_through_reaches_log_after_last_interceptor() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("released"));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("released event in log");
    assert_eq!(entry.event, draft_event("released"));
}

#[test]
fn interception_reply_can_modify_event() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("original"));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(draft_event("modified")))),
        })),
    )
    .expect("modifying reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("modified event in log");
    assert_eq!(entry.event, draft_event("modified"));
}

#[test]
fn interception_cannot_modify_mandatory_harness_notice() {
    // Mandatory harness diagnostics include extension config parse failures.
    // Interceptors may observe them, but must not be able to blank or downgrade
    // the message and recreate the same silent-fallback failure for live UIs.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.emit_info_important("extension core-shell rejected its config");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::HarnessNotice(
                tau_proto::HarnessNotice {
                    kind: "test.info".to_owned(),
                    message: String::new(),
                    level: tau_proto::NoticeLevel::Info,
                    always_show: false,
                },
            )))),
        })),
    )
    .expect("mutating reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("important info in log");
    assert!(matches!(
        entry.event,
        Event::HarnessNotice(info)
            if info.level == tau_proto::NoticeLevel::Warning
                && info.message == "extension core-shell rejected its config"
    ));
}

#[test]
fn interception_cannot_modify_critical_harness_notice() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.emit_notice(
        "test.critical",
        tau_proto::NoticeLevel::Critical,
        true,
        "critical failure",
    );
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::HarnessNotice(
                tau_proto::HarnessNotice {
                    kind: "test.info".to_owned(),
                    message: "downgraded".to_owned(),
                    level: tau_proto::NoticeLevel::Info,
                    always_show: false,
                },
            )))),
        })),
    )
    .expect("mutating reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("critical notice in log");
    assert!(matches!(
        entry.event,
        Event::HarnessNotice(info)
            if info.level == tau_proto::NoticeLevel::Critical
                && info.kind == "test.critical"
                && info.always_show
                && info.message == "critical failure"
    ));
}

#[test]
fn interception_cannot_drop_critical_harness_notice() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.emit_notice(
        "test.critical",
        tau_proto::NoticeLevel::Critical,
        true,
        "critical failure",
    );
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("critical notice in log");
    assert!(matches!(
        entry.event,
        Event::HarnessNotice(info)
            if info.level == tau_proto::NoticeLevel::Critical
                && info.kind == "test.critical"
                && info.always_show
                && info.message == "critical failure"
    ));
}

#[test]
fn interception_cannot_escalate_non_mandatory_harness_notice() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.emit_info("ordinary notice");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::HarnessNotice(
                tau_proto::HarnessNotice {
                    kind: tau_proto::notice_kind::EXTENSION_CONFIG_ERROR.to_owned(),
                    message: "edited message".to_owned(),
                    level: tau_proto::NoticeLevel::Critical,
                    always_show: true,
                },
            )))),
        })),
    )
    .expect("mutating reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("notice in log");
    assert!(matches!(
        entry.event,
        Event::HarnessNotice(info)
            if info.level == tau_proto::NoticeLevel::Info
                && info.kind == tau_proto::notice_kind::HARNESS_NOTICE
                && !info.always_show
                && info.message == "edited message"
    ));
}

#[test]
fn interception_priority_orders_lower_values_first() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let low = connect_test_tool(&mut h, "low");
    let high = connect_test_tool(&mut h, "high");
    for (name, priority) in [("low", 10), ("high", 0)] {
        h.handle_extension_event(
            name,
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
                priority: InterceptionPriority::new(priority),
            })),
        )
        .expect("intercept registration");
    }

    h.publish_event(None, draft_event("ordered"));

    assert!(
        high.lock()
            .expect("high events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
    assert!(
        !low.lock()
            .expect("low events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
}

#[test]
fn interception_same_priority_orders_by_component_name_and_redelivery_continues() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let alpha = connect_test_tool(&mut h, "alpha");
    let beta = connect_test_tool(&mut h, "beta");
    for name in ["beta", "alpha"] {
        h.handle_extension_event(
            name,
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept registration");
    }

    h.publish_event(None, draft_event("chain"));
    assert!(
        alpha
            .lock()
            .expect("alpha events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
    assert!(
        !beta
            .lock()
            .expect("beta events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );

    h.handle_extension_event(
        "alpha",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("alpha pass");
    assert!(
        beta.lock()
            .expect("beta events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
}

#[test]
fn interception_exact_beats_prefix_even_with_lower_prefix_priority() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let exact = connect_test_tool(&mut h, "exact");
    let prefix = connect_test_tool(&mut h, "prefix");
    h.handle_extension_event(
        "prefix",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Prefix("ui".to_owned())],
            priority: InterceptionPriority::new(-100),
        })),
    )
    .expect("prefix registration");
    h.handle_extension_event(
        "exact",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(100),
        })),
    )
    .expect("exact registration");

    h.publish_event(None, draft_event("exact"));

    assert!(
        exact
            .lock()
            .expect("exact events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
    assert!(
        !prefix
            .lock()
            .expect("prefix events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
}

#[test]
fn interception_pass_advances_past_responding_interceptor() {
    // With the new InterceptReply protocol the cursor lives on the
    // harness side and always advances strictly past the interceptor
    // that just replied. The old "Emit with interception: None
    // restarts" pattern is gone — a Pass(None) reply does *not* loop
    // the event back through the same interceptor.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    h.publish_event(None, draft_event("once"));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass reply");

    let count = interceptor
        .lock()
        .expect("events")
        .iter()
        .filter(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
        .count();
    assert_eq!(
        count, 1,
        "pass-through must not re-trigger the same interceptor"
    );
}

/// Ensures same-priority cursor advancement uses full registration order rather
/// than connection-id order alone.
#[test]
fn interception_cursor_uses_full_registration_order() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let first = connect_named_test_tool(&mut h, "z-conn", "alpha-component");
    let second = connect_named_test_tool(&mut h, "a-conn", "beta-component");

    for connection_id in ["z-conn", "a-conn"] {
        h.handle_extension_event(
            connection_id,
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept registration");
    }

    h.publish_event(None, draft_event("ordered"));
    h.handle_extension_event(
        "z-conn",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("first pass reply");

    assert_eq!(
        first
            .lock()
            .expect("first events")
            .iter()
            .filter(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
            .count(),
        1
    );
    assert_eq!(
        second
            .lock()
            .expect("second events")
            .iter()
            .filter(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
            .count(),
        1,
        "same-priority cursor must follow component-name ordering, not connection-id ordering"
    );
}

#[test]
fn interception_defers_subsequent_publishes_until_reply() {
    // Regression for the "Ready" loop: while one publish is parked
    // waiting on an InterceptReply, the harness must defer any
    // subsequent publishes rather than commit them out of order.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    // Publish two: the first parks in interception (matches the
    // selector); the second does NOT match and so would, in the
    // buggy world, race ahead of it.
    h.publish_event(None, draft_event("held"));
    h.publish_event(
        None,
        Event::HarnessNotice(tau_proto::HarnessNotice {
            kind: "test.info".to_owned(),
            message: "second".to_owned(),
            level: tau_proto::NoticeLevel::Info,
            always_show: false,
        }),
    );
    // Neither has committed yet — interception is in flight on the
    // first, the second is sitting in `deferred_publishes`.
    assert_eq!(h.event_log.next_seq(), baseline_seq);

    // Reply: pass-through. Both events should now commit, in order.
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass reply");

    let first = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("first event committed");
    assert_eq!(first.event, draft_event("held"));
    let second = h
        .event_log
        .get_next_from(first.seq.next())
        .expect("second event committed");
    assert!(matches!(
        &second.event,
        Event::HarnessNotice(info) if info.message == "second"
    ));
}

#[test]
fn deferred_tool_result_persists_after_call_tracking_is_cleared() {
    // Regression for a real rostra session failure. A tool result can
    // arrive while an unrelated event is parked in interception. The
    // result publish is deferred, but the intake path still completes
    // the call immediately and clears `tool_agents`. The
    // eventual deferred commit must persist to the conversation's
    // session from the publish snapshot, not from now-missing call
    // tracking; otherwise the next LLM prompt contains a tool_use
    // without its matching tool_result and the provider rejects it.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id.clone());
    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "call-read".into();
    let tool_name = ToolName::new("read");

    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("default conversation has an agent id");
    h.tool_agents.insert(call_id.clone(), cid.clone());
    h.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: tool_name.clone(),
            internal_name: tool_name.clone(),
            tool_type: tau_proto::ToolType::Function,
        },
    );
    h.publish_for_agent(
        &cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            agent_prompt_id: "sp-main".into(),
            agent_id: crate::parse_agent_id(&agent_id),
            output_items: vec![ContextItem::ToolCall(ToolCallItem {
                call_id: call_id.clone(),
                name: tool_name.clone(),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            })],
            stop_reason: tau_proto::ProviderStopReason::ToolCalls,
            error: None,
            usage: None,
            originator: tau_proto::PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );

    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    h.publish_event(None, draft_event("held"));
    assert!(
        h.pending_intercept.is_some(),
        "draft publish should be parked in interception"
    );

    h.handle_extension_event(
        "tool-provider",
        TestProtocolItem::Event(Event::ToolResult(ToolResult {
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("ok".to_owned()),
            kind: tau_proto::ToolResultKind::Final,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        })),
    )
    .expect("defer tool result");
    assert!(
        !h.tool_agents.contains_key(&call_id),
        "tool call tracking is cleared before the deferred publish commits"
    );

    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("intercept reply");

    let has_result = default_agent_branch(&h).iter().any(|entry| {
        matches!(
            entry,
            AgentEntry::ToolResults { items }
                if items.iter().any(|item|
                    item.call_id == call_id && item.status == ToolResultStatus::Success
                )
        )
    });
    assert!(
        has_result,
        "deferred tool.result must persist despite cleared call tracking"
    );
}

#[test]
fn interception_drop_of_must_pass_event_is_overridden() {
    // AgentPromptSubmitted is on the MUST_PASS list — even if an
    // interceptor returns Drop, the harness must publish the
    // original event (with a warn).
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let prompt = Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        text: "hello".to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        display_name: None,
        ctx_id: None,
    });
    h.publish_event(None, prompt.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("must-pass event still committed despite Drop");
    assert_eq!(entry.event, prompt);
}

fn agent_started_event(role: &str) -> Event {
    Event::AgentStarted(tau_proto::AgentStarted {
        parent_agent: None,
        agent_id: tau_proto::AgentId::parse("agent-started-test").expect("agent id"),
        role: role.to_owned(),
        display_name: Some("Started Test".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    })
}

fn persisted_agent_started_events(h: &Harness) -> Vec<Event> {
    h.agent_store
        .agent_events("agent-started-test")
        .expect("agent.started durable log")
        .into_iter()
        .map(|entry| entry.event)
        .collect()
}

/// Ensures interceptors cannot drop agent creation facts now that
/// AgentStarted flows through the central publish/interception pipeline.
#[test]
fn interception_drop_of_agent_started_is_overridden() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let started = agent_started_event("engineer");
    h.publish_event(None, started.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("agent.started still committed despite Drop");
    assert_eq!(entry.event, started);
    assert_eq!(persisted_agent_started_events(&h), vec![started]);
}

/// Ensures interceptors cannot rewrite immutable agent creation facts such as
/// the role attached to an AgentStarted event.
#[test]
fn interception_replacement_of_agent_started_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let started = agent_started_event("engineer");
    h.publish_event(None, started.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(agent_started_event("reviewer")))),
        })),
    )
    .expect("replacement reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("agent.started committed");
    assert_eq!(entry.event, started);
    assert_eq!(persisted_agent_started_events(&h), vec![started]);
}

fn session_agent_loaded_event(agent_id: &str) -> Event {
    Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
        session_id: "session-intercept".into(),
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        ephemeral: false,
    })
}

fn session_agent_unloaded_event(agent_id: &str) -> Event {
    Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
        session_id: "session-intercept".into(),
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
    })
}

/// Ensures interceptors cannot drop durable session membership load facts,
/// because resume state depends on the committed membership log matching live
/// delivery.
#[test]
fn interception_drop_of_session_agent_loaded_is_overridden() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SESSION_AGENT_LOADED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let loaded = session_agent_loaded_event("agent-loaded-original");
    h.publish_event(None, loaded.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("session.agent_loaded still committed despite Drop");
    assert_eq!(entry.event, loaded);
    let membership = h
        .store
        .session("session-intercept")
        .expect("session membership");
    assert!(
        membership
            .contains_agent(&tau_proto::AgentId::parse("agent-loaded-original").expect("agent id"))
    );
}

/// Ensures interceptors cannot rewrite durable session membership unload facts,
/// preventing one agent's unload from being persisted as another agent's
/// unload.
#[test]
fn interception_replacement_of_session_agent_unloaded_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SESSION_AGENT_UNLOADED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let unloaded = session_agent_unloaded_event("agent-unloaded-original");
    h.publish_event(None, unloaded.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(session_agent_unloaded_event(
                "agent-unloaded-replacement",
            )))),
        })),
    )
    .expect("replacement reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("session.agent_unloaded committed");
    assert_eq!(entry.event, unloaded);
    let events = h
        .store
        .session_events("session-intercept")
        .expect("session events")
        .into_iter()
        .map(|entry| entry.event)
        .collect::<Vec<_>>();
    assert_eq!(events, vec![unloaded]);
}

fn session_started_event(session_id: &str) -> Event {
    Event::SessionStarted(tau_proto::SessionStarted {
        session_id: session_id.into(),
        reason: tau_proto::SessionStartReason::New,
    })
}

fn session_shutdown_event(session_id: &str) -> Event {
    Event::SessionShutdown(tau_proto::SessionShutdown {
        session_id: session_id.into(),
    })
}

fn agent_message_sent_event(message: &str) -> Event {
    Event::AgentMessageSent(tau_proto::AgentMessageSent {
        message_id: tau_proto::AgentMessageId::from("msg-intercept"),
        sender_id: tau_proto::AgentId::parse("agent-message-sender").expect("agent id"),
        recipient: tau_proto::AgentMessageRecipient::Agent {
            agent_id: tau_proto::AgentId::parse("agent-message-recipient").expect("agent id"),
        },
        kind: tau_proto::AgentMessageKind::Message,
        message: message.to_owned(),
    })
}

fn agent_message_received_event(recipient_id: &str) -> Event {
    Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::from("msg-intercept"),
        sender_id: tau_proto::AgentId::parse("agent-message-sender").expect("agent id"),
        sender_session_id: None,
        recipient_id: tau_proto::AgentId::parse(recipient_id).expect("agent id"),
        kind: tau_proto::AgentMessageKind::Message,
        message: "hello".to_owned(),
    })
}

/// Ensures interceptors cannot drop session lifecycle facts required by
/// extensions and context providers for per-session setup.
#[test]
fn interception_drop_of_session_started_is_overridden() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::SESSION_STARTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let started = session_started_event("session-lifecycle-original");
    h.publish_event(None, started.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("session.started still committed despite Drop");
    assert_eq!(entry.event, started);
}

/// Ensures interceptors cannot rewrite session shutdown facts used to flush or
/// drop extension-owned per-session state.
#[test]
fn interception_replacement_of_session_shutdown_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::SESSION_SHUTDOWN)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let shutdown = session_shutdown_event("session-lifecycle-original");
    h.publish_event(None, shutdown.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(session_shutdown_event(
                "session-lifecycle-replacement",
            )))),
        })),
    )
    .expect("replacement reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("session.shutdown committed");
    assert_eq!(entry.event, shutdown);
}

/// Ensures interceptors cannot drop harness-validated sender-side message
/// projections after recipient validation has already succeeded.
#[test]
fn interception_drop_of_agent_message_sent_is_overridden() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_SENT,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let sent = agent_message_sent_event("hello");
    h.publish_event(None, sent.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("agent.message_sent still committed despite Drop");
    assert_eq!(entry.event, sent);
}

/// Ensures interceptors cannot rewrite harness-validated recipient-side message
/// projections, including attempts to route the projection to another agent.
#[test]
fn interception_replacement_of_agent_message_received_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let received = agent_message_received_event("agent-message-recipient");
    h.publish_event(None, received.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(agent_message_received_event(
                "agent-message-other-recipient",
            )))),
        })),
    )
    .expect("replacement reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("agent.message_received committed");
    assert_eq!(entry.event, received);
}

fn tool_result_event(call_id: &str, text: &str) -> Event {
    Event::ToolResult(ToolResult {
        call_id: call_id.into(),
        tool_name: ToolName::new("test_tool"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text(text.to_owned()),
        kind: tau_proto::ToolResultKind::Final,
        originator: tau_proto::PromptOriginator::User,
        display: None,
    })
}

fn tool_cancelled_event(call_id: &str) -> Event {
    Event::ToolCancelled(tau_proto::ToolCancelled {
        call_id: call_id.into(),
        tool_name: ToolName::new("test_tool"),
        tool_type: tau_proto::ToolType::Function,
    })
}

/// Ensures interceptors cannot rewrite terminal tool transcript facts, because
/// changing the call id would detach the completion from the requested tool
/// use.
#[test]
fn interception_replacement_of_tool_result_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_RESULT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let result = tool_result_event("call-original", "ok");
    h.publish_event(None, result.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(tool_result_event(
                "call-rewritten",
                "rewritten",
            )))),
        })),
    )
    .expect("replacement reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("tool.result committed");
    assert_eq!(entry.event, result);
}

/// Ensures interceptors cannot drop cancellation facts for terminal tool calls.
#[test]
fn interception_drop_of_tool_cancelled_is_overridden() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_CANCELLED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let cancelled = tool_cancelled_event("call-cancelled");
    h.publish_event(None, cancelled.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("tool.cancelled still committed despite Drop");
    assert_eq!(entry.event, cancelled);
}

/// Ensures a failed intercept-request delivery does not park the publish
/// pipeline forever and subsequent publishes still commit.
#[test]
fn failed_intercept_request_delivery_skips_interceptor_and_drains_publishes() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    connect_failing_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let first = draft_event("first");
    let second = draft_event("second");
    h.publish_event(None, first.clone());
    h.publish_event(None, second.clone());

    assert!(h.pending_intercept.is_none());
    let first_entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("first draft committed");
    assert_eq!(first_entry.event, first);
    let second_entry = h
        .event_log
        .get_next_from(first_entry.seq.next())
        .expect("second draft committed");
    assert_eq!(second_entry.event, second);
}

#[test]
fn interception_disconnect_mid_reply_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("inflight"));
    // Disconnect before the interceptor replies. The harness should
    // treat this as Pass(None) and still commit the event.
    h.handle_disconnect("interceptor");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("event committed after disconnect");
    assert_eq!(entry.event, draft_event("inflight"));
}

#[test]
fn interception_user_prompt_dispatch_waits_for_commit() {
    // Regression for the "Ready" loop. When `AgentPromptSubmitted` is
    // held in interception, the harness must not dispatch the agent
    // prompt against the pre-prompt conversation tail — the
    // assembled message list must include the just-committed user
    // message. We assert this by inspecting the conversation
    // head/tree before vs. after the intercept reply lands.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id.clone());

    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    let cid = ensure_test_user_agent(&mut h);
    let head_before_dispatch = h.agents.get(&cid).and_then(|c| c.head);
    let prompts_before = prompt_created_count(&h);

    // Drive the user-prompt path. The publish parks in interception.
    h.dispatch_prompt_for_agent(&cid, "real question".to_owned())
        .expect("dispatch");

    // While the intercept is in flight: no agent prompt was minted,
    // c.head hasn't moved, and the deferred-dispatch queue contains
    // our cid.
    assert_eq!(
        prompt_created_count(&h),
        prompts_before,
        "agent dispatch must wait until the prompt commits"
    );
    assert_eq!(
        h.agents.get(&cid).and_then(|c| c.head),
        head_before_dispatch,
        "c.head must not advance while the prompt is parked"
    );
    assert_eq!(h.pending_user_prompt_dispatches.len(), 1);

    // Reply pass-through. Commit + react fires the deferred
    // dispatch, and the AgentPromptCreated is built from the
    // updated tree.
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("intercept reply");

    assert_eq!(h.pending_user_prompt_dispatches.len(), 0);
    assert_eq!(
        prompt_created_count(&h),
        prompts_before + 1,
        "agent dispatch fires once the prompt commits"
    );
    let head_after = h
        .agents
        .get(&cid)
        .and_then(|c| c.head)
        .expect("c.head advanced");
    let entry = default_agent_node(&h, head_after);
    assert!(
        matches!(
            &entry.entry,
            AgentEntry::UserInput { items }
                if matches!(
                    items.as_slice(),
                    [ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content,
                        ..
                    })] if matches!(content.as_slice(), [ContentPart::Text { text }] if text == "real question")
                )
        ),
        "c.head points at the just-committed user prompt"
    );
}

#[test]
fn interception_mutating_prompt_reaches_agent() {
    // End-to-end check that mirrors the test-dummy's "Tao → Tau"
    // correction flow: an interceptor replies with
    // `Pass(Some(modified))` and the agent receives the modified
    // text in its message list. Verifies the full chain (intercept
    // request → reply with mutation → fold of mutated event →
    // c.head sync → agent dispatch with up-to-date branch) end-to-
    // end.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id.clone());

    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, "I love Tao".to_owned())
        .expect("dispatch");

    // Interceptor replies with the mutated event.
    let agent_id = h
        .agents
        .get(&cid)
        .and_then(|conv| conv.agent_id.as_ref())
        .expect("prompt publish assigned an agent id")
        .clone();
    let mutated = Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
        agent_id: crate::parse_agent_id(&agent_id),
        text: "I love Tau".to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        display_name: None,
        ctx_id: None,
    });
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(mutated))),
        })),
    )
    .expect("intercept reply");

    // The committed user message reflects the *mutated* text — and
    // c.head points at it (see `interception_user_prompt_dispatch_
    // waits_for_commit` for the dispatch-side assertion).
    let head = h
        .agents
        .get(&cid)
        .and_then(|c| c.head)
        .expect("c.head advanced");
    let entry = default_agent_node(&h, head);
    assert!(
        matches!(
            &entry.entry,
            AgentEntry::UserInput { items }
                if matches!(
                    items.as_slice(),
                    [ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content,
                        ..
                    })] if matches!(content.as_slice(), [ContentPart::Text { text }] if text == "I love Tau")
                )
        ),
        "the agent will see the *interceptor-mutated* text, not the user's typo"
    );
}

#[test]
fn publish_for_agent_does_not_emit_navigate_tree() {
    // Phase 4: cross-conversation publishes used to bounce
    // `tree.head()` via a `UiNavigateTree` event before folding the
    // real event. With explicit-parent folds in
    // `AgentTree::apply_event_at`, the bounce is gone — the harness
    // stamps the conversation's `head` directly.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id.clone());

    let baseline_seq = h.event_log.next_seq();
    let cid = ensure_test_user_agent(&mut h);

    // Two prompts in a row on the same conversation. Either would
    // historically have caused `publish_for_agent_from` to
    // bounce `tree.head()` via `UiNavigateTree`.
    h.dispatch_prompt_for_agent(&cid, "first".to_owned())
        .expect("first dispatch");
    h.dispatch_prompt_for_agent(&cid, "second".to_owned())
        .expect("second dispatch");

    let mut navigates = 0;
    let mut user_msgs = 0;
    let mut id = baseline_seq;
    while let Some(entry) = h.event_log.get_next_from(id) {
        match &entry.event {
            Event::UiNavigateTree(_) => navigates += 1,
            Event::AgentPromptSubmitted(_) => user_msgs += 1,
            _ => {}
        }
        id = entry.seq.next();
    }
    assert_eq!(
        navigates, 0,
        "cross-conversation publishes must not emit UiNavigateTree anymore"
    );
    assert_eq!(user_msgs, 2);
}

#[test]
fn interception_disconnect_clears_registration() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    h.handle_disconnect("interceptor");
    let after_disconnect_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("not intercepted"));

    let entry = h
        .event_log
        .get_next_from(after_disconnect_seq)
        .expect("event reaches log");
    assert_eq!(entry.event, draft_event("not intercepted"));
}

#[test]
fn agent_metadata_set_and_unset_events_are_interceptable() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "metadata-interceptor");
    h.handle_extension_event(
        "metadata-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![
                EventSelector::Exact(tau_proto::EventName::AGENT_METADATA_SET),
                EventSelector::Exact(tau_proto::EventName::AGENT_METADATA_UNSET),
            ],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    let agent_id = tau_proto::AgentId::parse("metadata-agent").expect("agent id");
    let key = tau_proto::AgentMetadataKey::new("ext_core-shell_cwd");
    let set = Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
        agent_id: agent_id.clone(),
        key: key.clone(),
        value: CborValue::Text("/tmp".to_owned()),
        inheritable: true,
    });
    h.publish_event(None, set.clone());
    let (event, transient) = intercepted_payload(&interceptor);
    assert_eq!(event, set);
    assert!(!transient, "metadata set must be durable by default");
    h.handle_extension_event(
        "metadata-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass set");

    interceptor.lock().expect("events").clear();
    let unset = Event::AgentMetadataUnset(tau_proto::AgentMetadataUnset { agent_id, key });
    h.publish_event(None, unset.clone());
    let (event, transient) = intercepted_payload(&interceptor);
    assert_eq!(event, unset);
    assert!(!transient, "metadata unset must be durable by default");

    h.shutdown().expect("shutdown");
}

#[test]
fn invalid_metadata_interceptor_replacements_fall_back_to_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "metadata-rewriter");
    h.handle_extension_event(
        "metadata-rewriter",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_METADATA_SET,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    let agent_id = tau_proto::AgentId::parse("metadata-agent").expect("agent id");
    h.session_loaded_agents.insert(agent_id.clone());
    let original = Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
        agent_id: agent_id.clone(),
        key: tau_proto::AgentMetadataKey::new("valid"),
        value: CborValue::Text("ok".to_owned()),
        inheritable: true,
    });
    h.publish_event(None, original.clone());
    let replacement = Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
        agent_id,
        key: tau_proto::AgentMetadataKey::new("too-large"),
        value: CborValue::Bytes(vec![0; tau_proto::MAX_AGENT_METADATA_VALUE_BYTES + 1]),
        inheritable: true,
    });
    h.handle_extension_event(
        "metadata-rewriter",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(replacement))),
        })),
    )
    .expect("replace with invalid metadata");

    let events = event_log_events(&h);
    assert!(events.iter().any(|event| event == &original));
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::AgentMetadataSet(set) if set.key.as_str() == "too-large"
    )));

    h.shutdown().expect("shutdown");
}
