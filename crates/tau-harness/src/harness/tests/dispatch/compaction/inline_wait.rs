//! Inline-only wait preemption at the real cancellation publication boundary.

use super::*;

/// Check the trigger's admission boundary independently of wait cancellation.
/// Successful inline work remains ordinary inference, but must carry its
/// trigger.
fn check_inline_trigger_admission(after_wait: bool, intercepted: bool, reject: bool) {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role")
        .inference_compaction = Some(path_tau_config_settings::RoleCompaction::Threshold(1200));
    let wait = wait_input_call("inline-trigger-wait");
    if after_wait {
        assert!(intercepted, "park the trigger after cancellation commits");
        seed_assistant_tool_round(&mut h, &cid, &[(wait.id.as_str(), "wait")]);
        seed_tools_running(&mut h, &cid, vec![wait.id.clone()]);
        h.handle_wait_tool_call(&cid, &wait, ToolName::new("wait"))
            .expect("install waiter");
    }
    if intercepted {
        connect_test_tool(&mut h, "inline-trigger-interceptor");
        h.handle_extension_event(
            "inline-trigger-interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_COMPACTION_TRIGGERED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept trigger");
    } else if reject {
        reject_next_semantic_admission(&h);
    }
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    if intercepted {
        let events = event_log_events(&h);
        assert!(!events.iter().any(|event| matches!(
            event,
            Event::AgentCompactionTriggered(_) | Event::AgentPromptCreated(_)
        )));
        if after_wait {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(
                        event,
                        Event::ToolCancelled(cancelled) if cancelled.call_id == wait.id
                    ))
                    .count(),
                1,
                "reject the trigger only after the wait cancellation commits"
            );
            assert!(matches!(
                h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
                AgentTurnState::Idle
            ));
        }
        if reject {
            reject_next_semantic_admission(&h);
        }
        h.handle_extension_event(
            "inline-trigger-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("resolve trigger admission");
    }
    let events = event_log_events(&h);
    let triggers = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::AgentCompactionTriggered(trigger) if trigger.agent_id == agent_id
            )
        })
        .count();
    let prompts: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentPromptCreated(prompt) if prompt.agent_id == agent_id => Some(prompt),
            _ => None,
        })
        .collect();
    assert_eq!(triggers, usize::from(!reject));
    assert_eq!(prompts.len(), usize::from(!reject));
    for prompt in prompts {
        assert_eq!(prompt.operation, tau_proto::PromptOperation::Inference);
        assert!(
            prompt
                .context
                .flatten()
                .contains(&ContextItem::CompactionTrigger)
        );
    }
    if reject {
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            Event::HarnessNotice(notice)
                if notice.kind.as_str() == tau_proto::notice_kind::HARNESS_FAILURE
        )));
        assert!(
            !h.prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .contains_key(&cid)
        );
    }
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::AgentManualCompactionRequested(_) | Event::AgentStandaloneCompactionStarted(_)
    )));
    h.shutdown().expect("shutdown");
}

/// A transient admission rejection must not release a trigger-free idle prompt.
#[test]
fn inline_idle_trigger_admission_gates_dispatch() {
    for reject in [false, true] {
        check_inline_trigger_admission(false, false, reject);
        check_inline_trigger_admission(false, true, reject);
    }
}

/// Settled wait cancellation does not authorize the compact command's dispatch
/// when its later trigger rejects, including after asynchronous interception.
#[test]
fn inline_post_wait_trigger_admission_gates_dispatch() {
    for reject in [false, true] {
        check_inline_trigger_admission(true, true, reject);
    }
}

/// Ordinary arbitration that must survive a rejected cancellation claim.
#[derive(Clone, Copy, Debug)]
enum Wake {
    /// The original activating-input deadline expires.
    Timeout,
    /// Accepted activating input becomes ready.
    Input,
    /// The specifically awaited background call completes.
    Exact,
    /// The oldest background call completes for a bare wait.
    Bare,
}

/// Publish a competing wake through the normal wait settlement entry points.
fn wake(h: &mut Harness, cid: &AgentId, kind: Wake, deadline: Option<Instant>) {
    match kind {
        Wake::Timeout => h.process_input_wait_deadlines(deadline.expect("input deadline")),
        Wake::Input => h.activate_waits_for(cid, tau_proto::ObservationId::random()),
        Wake::Exact | Wake::Bare => h.record_wait_background_result(
            tau_proto::ToolBackgroundResult {
                call_id: "inline-background".into(),
                tool_name: ToolName::new("slow"),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Text("completed".to_owned()),
                display: None,
                originator: tau_proto::PromptOriginator::User,
            },
            Some(tau_proto::ObservationId::random()),
        ),
    }
}

/// Exercise cancellation admission, not a direct rollback-helper invocation.
fn check_arbitration(kind: Wake, reject: bool, wake_before_release: bool) {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let mut wait = wait_input_call("inline-wait-race");
    if matches!(kind, Wake::Exact | Wake::Bare) {
        let background = ToolCallId::from("inline-background");
        h.tool_routing
            .tool_runtime
            .tool_agents
            .insert(background.clone(), cid.clone());
        h.tool_routing.tool_runtime.pending_tools.insert(
            background.clone(),
            PendingTool {
                name: ToolName::new("slow"),
                internal_name: ToolName::new("slow"),
                tool_type: tau_proto::ToolType::Function,
                allows_provider_image: false,
                image_storage: Default::default(),
            },
        );
        h.record_wait_tool_request(&background);
        h.record_wait_tool_result(
            &ToolResult {
                presentation: Default::default(),
                call_id: background.clone(),
                tool_name: ToolName::new("slow"),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Null,
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            },
            Some(tau_proto::ObservationId::random()),
        );
        wait.arguments = if matches!(kind, Wake::Exact) {
            CborValue::Map(vec![(
                CborValue::Text("tool_call_id".to_owned()),
                CborValue::Text(background.to_string()),
            )])
        } else {
            CborValue::Map(Vec::new())
        };
    }
    seed_assistant_tool_round(&mut h, &cid, &[(wait.id.as_str(), "wait")]);
    seed_tools_running(&mut h, &cid, vec![wait.id.clone()]);
    h.handle_wait_tool_call(&cid, &wait, ToolName::new("wait"))
        .expect("install waiter");
    let deadline = h.next_input_wait_deadline();
    connect_test_tool(&mut h, "inline-wait-interceptor");
    h.handle_extension_event(
        "inline-wait-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_CANCELLED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept cancellation");
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    assert!(
        h.wait_claimed_for_manual_compaction(&cid, &wait.id),
        "{kind:?} must install and claim its waiter"
    );
    assert!(!event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentCompactionTriggered(_) | Event::AgentStandaloneCompactionStarted(_)
    )));
    assert!(
        !h.runtime_io
            .publication
            .deferred
            .iter()
            .any(|pending| matches!(pending.event(), Event::AgentCompactionTriggered(_))),
        "interception must not merely hide an independent compaction intent"
    );
    if wake_before_release {
        wake(&mut h, &cid, kind, deadline);
        assert_eq!(tool_result_count(&h, wait.id.as_str()), 0);
    }
    if reject {
        reject_next_semantic_admission(&h);
    }
    h.handle_extension_event(
        "inline-wait-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("resolve cancellation admission");
    if !wake_before_release {
        wake(&mut h, &cid, kind, deadline);
    }
    h.retry_retained_tool_terminals(&cid);
    let events = event_log_events(&h);
    assert_eq!(tool_result_count(&h, wait.id.as_str()), usize::from(reject));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event,
            Event::ToolCancelled(cancelled) if cancelled.call_id == wait.id))
            .count(),
        usize::from(!reject)
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::AgentCompactionTriggered(_)))
            .count(),
        usize::from(!reject)
    );
    assert!(!h.wait_claimed_for_manual_compaction(&cid, &wait.id));
    assert!(
        !h.tool_routing
            .tool_runtime
            .retained_terminals
            .contains_key(&wait.id)
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::AgentManualCompactionRequested(_) | Event::AgentStandaloneCompactionStarted(_)
    )));
}

/// Rejection restores both wakes deferred during interception and later
/// ordinary arbitration, without retaining the rejected cancellation or inline
/// intent.
#[test]
fn inline_rejected_cancellation_restores_wait_arbitration() {
    for kind in [Wake::Timeout, Wake::Input, Wake::Exact, Wake::Bare] {
        for wake_before_release in [true, false] {
            check_arbitration(kind, true, wake_before_release);
        }
    }
}

/// A winning cancellation suppresses competing wakes and starts inline
/// compaction exactly once only after canonical cancellation admission.
#[test]
fn inline_committed_cancellation_owns_competing_wakes() {
    for kind in [Wake::Timeout, Wake::Input, Wake::Exact, Wake::Bare] {
        check_arbitration(kind, false, true);
    }
}
