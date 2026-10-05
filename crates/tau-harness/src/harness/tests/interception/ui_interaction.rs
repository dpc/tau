//! Acceptance remains owned by the exact ordinary interaction publication.

use tau_proto::UiCancelPrompt;

use super::*;
use crate::harness::tests::dispatch::enable_remote_compaction_for_test_model;

/// Park interaction facts without parking unrelated agent creation or prompts.
fn intercept_interactions(h: &mut Harness) {
    connect_test_tool(h, "ui-interaction-gate");
    h.handle_extension_event(
        "ui-interaction-gate",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_USER_INTERACTION_RECORDED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interaction gate");
}

/// Resolve the one outstanding interaction without changing its identity.
fn release_interaction(h: &mut Harness) {
    h.handle_extension_event(
        "ui-interaction-gate",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release interaction");
}

/// Submit a visible input through authenticated UI intake.
fn submit(h: &mut Harness, agent_id: &AgentId, text: &str) {
    h.handle_authenticated_ui_prompt_submitted(
        &crate::test_connection_id("interaction-ui"),
        tau_proto::UiPromptSubmitted {
            literal: true,
            session_id: h.session_runtime.current_session_id.clone(),
            text: text.to_owned(),
            agent_id: agent_id.clone(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        },
    )
    .expect("UI intake");
}

/// Count semantic acceptance facts independently of runtime ordering.
fn interactions(h: &Harness, agent_id: &AgentId) -> usize {
    h.session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent_id.as_str())
        .expect("agent records")
        .iter()
        .filter(|record| {
            matches!(&record.event,
            Event::AgentUserInteractionRecorded(interaction) if &interaction.agent_id == agent_id)
        })
        .count()
}

/// Assert parked/rejected input has neither accepted order nor prompt effects.
fn assert_unaccepted(h: &Harness, cid: &AgentId, agent_id: &AgentId) {
    assert_eq!(interactions(h, agent_id), 0);
    assert!(
        !h.session_runtime
            .user_interaction_order
            .contains_key(agent_id.as_str())
    );
    let agent = &h.agent_runtime.agent_registry.agents[cid];
    assert!(agent.dispatch.pending_prompts.is_empty());
    assert!(agent.dispatch.in_flight_prompt.is_none());
    assert!(!event_log_events(h).iter().any(|event| matches!(event,
        Event::AgentPromptSubmitted(prompt) if &prompt.agent_id == agent_id)));
}

/// Count requester-directed failures rather than generic diagnostic broadcasts.
fn ui_errors(frames: &Arc<Mutex<Vec<RoutedFrame>>>) -> usize {
    frames
        .lock()
        .expect("UI frames")
        .iter()
        .filter(|frame| {
            matches!(
                peel_inner_event(&frame.frame), Some(Event::HarnessNotice(notice))
                    if notice.kind == tau_proto::notice_kind::UI_COMMAND_ERROR
            )
        })
        .count()
}

/// Interaction interception must hold every dependent effect until ordinary
/// commit.
#[test]
fn parked_interaction_waits_for_commit_before_navigation_and_prompt() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let id = durable_agent_id_for_conversation(&h, &cid);
    h.agent_runtime
        .agent_registry
        .navigation_modes
        .insert(id.clone(), tau_proto::AgentNavigationMode::Suspended);
    intercept_interactions(&mut h);
    let ui = connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
    submit(&mut h, &id, "wait for acceptance");
    assert_unaccepted(&h, &cid, &id);
    assert_eq!(
        h.agent_runtime.agent_registry.navigation_modes[&id],
        tau_proto::AgentNavigationMode::Suspended
    );
    release_interaction(&mut h);
    assert_eq!(interactions(&h, &id), 1);
    assert_eq!(h.session_runtime.user_interaction_order[id.as_str()], 1);
    assert_eq!(
        h.agent_runtime.agent_registry.navigation_modes[&id],
        tau_proto::AgentNavigationMode::Active
    );
    assert_eq!(ui_errors(&ui), 0);
    let ordered: Vec<_> = event_log_events(&h)
        .iter()
        .filter_map(|event| match event {
            Event::AgentUserInteractionRecorded(_) => Some("interaction"),
            Event::AgentPromptSubmitted(_) => Some("prompt"),
            _ => None,
        })
        .collect();
    assert_eq!(ordered, ["interaction", "prompt"]);
    h.shutdown().expect("shutdown");
}

/// Storage rejection must fail the original UI input, not admit its prompt or
/// retry it.
#[test]
fn rejected_interaction_leaves_dependencies_unapplied() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let id = durable_agent_id_for_conversation(&h, &cid);
    h.agent_runtime
        .agent_registry
        .navigation_modes
        .insert(id.clone(), tau_proto::AgentNavigationMode::Suspended);
    intercept_interactions(&mut h);
    let ui = connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
    submit(&mut h, &id, "rejected input");
    reject_next_semantic_admission(&h);
    release_interaction(&mut h);
    assert_unaccepted(&h, &cid, &id);
    assert_eq!(ui_errors(&ui), 1);
    assert_eq!(
        h.agent_runtime.agent_registry.navigation_modes[&id],
        tau_proto::AgentNavigationMode::Suspended
    );
    assert!(h.runtime_io.publication.pending_intercept.is_none());
    h.shutdown().expect("shutdown");
}

/// A later explicit absolute navigation write wins even if it repeats the old
/// mode.
#[test]
fn parked_interaction_preserves_newer_explicit_navigation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let id = durable_agent_id_for_conversation(&h, &cid);
    h.agent_runtime
        .agent_registry
        .navigation_modes
        .insert(id.clone(), tau_proto::AgentNavigationMode::Suspended);
    intercept_interactions(&mut h);
    connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
    submit(&mut h, &id, "old input");
    h.handle_set_agent_navigation_mode(
        &crate::test_connection_id("interaction-ui"),
        tau_proto::UiSetAgentNavigationMode {
            request_id: "explicit-navigation".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            agent_id: id.clone(),
            action: tau_proto::UiAgentNavigationModeAction::SetSuspended,
        },
    );
    release_interaction(&mut h);
    assert_eq!(interactions(&h, &id), 1);
    assert_eq!(
        h.agent_runtime.agent_registry.navigation_modes[&id],
        tau_proto::AgentNavigationMode::Suspended
    );
    assert!(event_log_events(&h).iter().any(
        |event| matches!(event, Event::AgentPromptSubmitted(prompt) if prompt.agent_id == id)
    ));
    h.shutdown().expect("shutdown");
}

/// Same-agent occurrences retain distinct ownership and advance order exactly
/// once each.
#[test]
fn successive_parked_interactions_have_distinct_commit_outcomes() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let id = durable_agent_id_for_conversation(&h, &cid);
    intercept_interactions(&mut h);
    let ui = connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
    submit(&mut h, &id, "first rejected");
    submit(&mut h, &id, "second accepted");
    assert_unaccepted(&h, &cid, &id);
    reject_next_semantic_admission(&h);
    release_interaction(&mut h);
    assert_unaccepted(&h, &cid, &id);
    assert_eq!(ui_errors(&ui), 1);
    release_interaction(&mut h);
    assert_eq!(interactions(&h, &id), 1);
    assert_eq!(h.session_runtime.user_interaction_order[id.as_str()], 1);
    assert!(event_log_events(&h).iter().any(|event| matches!(event,
        Event::AgentPromptSubmitted(prompt) if prompt.text == "second accepted")));
    h.shutdown().expect("shutdown");
}

/// Construct a correlated visible initial prompt without admitting it early.
fn create_with_initial_prompt(h: &mut Harness) -> AgentId {
    h.handle_ui_create_agent_from(
        &crate::test_connection_id("interaction-ui"),
        tau_proto::UiCreateAgent {
            request_id: "interaction-create".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            role: "engineer".to_owned(),
            model_override: None,
            effort_override: None,
            metadata: Vec::new(),
            initial_prompt: Some("initial input".to_owned()),
            literal: true,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("interaction-create-prompt".to_owned()),
            parent_agent: None,
            ephemeral: false,
        },
    )
    .expect("create UI agent");
    h.agent_runtime
        .agent_registry
        .agents
        .values()
        .find_map(|agent| agent.identity.agent_id.as_deref())
        .map(crate::parse_agent_id)
        .expect("created agent id")
}

/// The correlated create result waits for interaction acceptance, then prompt
/// admission.
#[test]
fn initial_ui_interaction_gates_create_admission() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    intercept_interactions(&mut h);
    let ui = connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
    let id = create_with_initial_prompt(&mut h);
    let cid = h.agent_runtime.agent_registry.agent_routes[id.as_str()].clone();
    assert_unaccepted(&h, &cid, &id);
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .next_ctx_id
            .is_none()
    );
    assert!(!ui.lock().expect("UI frames").iter().any(|frame| matches!(
        peel_inner_event(&frame.frame),
        Some(Event::UiCreateAgentResult(_))
    )));
    release_interaction(&mut h);
    assert_eq!(interactions(&h, &id), 1);
    assert_eq!(ui.lock().expect("UI frames").iter().filter(|frame| matches!(peel_inner_event(&frame.frame),
        Some(Event::UiCreateAgentResult(result)) if matches!(&result.outcome,
            tau_proto::UiCreateAgentOutcome::Created { agent_id, initial_prompt: tau_proto::UiCreateAgentInitialPrompt::Queued } if agent_id == &id
        ))).count(), 1);
    h.shutdown().expect("shutdown");
}

/// Rejected create interaction reports its durable identity without admission,
/// and cannot lend its initial correlation to a later uncorrelated prompt.
#[test]
fn initial_ui_interaction_rejection_reports_existing_create_failure() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    intercept_interactions(&mut h);
    let ui = connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
    let id = create_with_initial_prompt(&mut h);
    let cid = h.agent_runtime.agent_registry.agent_routes[id.as_str()].clone();
    reject_next_semantic_admission(&h);
    release_interaction(&mut h);
    assert_unaccepted(&h, &cid, &id);
    let results: Vec<_> = ui
        .lock()
        .expect("UI frames")
        .iter()
        .filter_map(|frame| match peel_inner_event(&frame.frame) {
            Some(Event::UiCreateAgentResult(result)) => Some(result.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert!(
        matches!(&results[0].outcome, tau_proto::UiCreateAgentOutcome::Rejected {
        reason: tau_proto::UiCreateAgentRejection::InitialPromptFailed, agent_id: Some(agent_id), ..
    } if agent_id == &id)
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .next_ctx_id
            .is_none()
    );
    submit(&mut h, &id, "follow-up without initial correlation");
    release_interaction(&mut h);
    let followups: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentPromptCreated(prompt) if prompt.agent_id == id => Some(prompt),
            _ => None,
        })
        .collect();
    assert_eq!(followups.len(), 1);
    assert!(
        followups[0].ctx_id.is_none(),
        "rejected initial correlation cannot leak into later input"
    );
    h.shutdown().expect("shutdown");
}

/// Idle cancellation remains rejected and cannot become a new pending-input
/// cancel API.
#[test]
fn idle_cancel_does_not_cancel_parked_interaction() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let id = durable_agent_id_for_conversation(&h, &cid);
    intercept_interactions(&mut h);
    let ui = connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
    submit(&mut h, &id, "idle pending input");
    h.handle_cancel_prompt(
        &crate::test_connection_id("interaction-ui"),
        &UiCancelPrompt {
            session_id: h.session_runtime.current_session_id.clone(),
            target_agent_id: Some(id.clone()),
            agent_prompt_id: None,
        },
    );
    assert_eq!(ui_errors(&ui), 1);
    assert_unaccepted(&h, &cid, &id);
    release_interaction(&mut h);
    assert_eq!(interactions(&h, &id), 1);
    h.shutdown().expect("shutdown");
}

/// Accepted cancellation rejects all same-agent pending inputs, leaves another
/// agent intact, and consumes the old interceptor reply without applying it to
/// subsequent work.
#[test]
fn active_cancel_rejects_all_same_agent_interactions_only() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let id = durable_agent_id_for_conversation(&h, &cid);
    let other_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let other_id = durable_agent_id_for_conversation(&h, &other_cid);
    finish_test_agent_context_wait(&mut h, &other_id);
    seed_agent_thinking(&mut h, &cid, "active-cancel-interactions");
    intercept_interactions(&mut h);
    let ui = connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
    submit(&mut h, &id, "first canceled");
    submit(&mut h, &id, "second canceled");
    submit(&mut h, &other_id, "other agent accepted");
    h.handle_cancel_prompt(
        &crate::test_connection_id("interaction-ui"),
        &UiCancelPrompt {
            session_id: h.session_runtime.current_session_id.clone(),
            target_agent_id: Some(id.clone()),
            agent_prompt_id: None,
        },
    );
    assert_eq!(interactions(&h, &id), 0);
    assert_eq!(ui_errors(&ui), 2);
    assert_eq!(interactions(&h, &other_id), 1);
    release_interaction(&mut h);
    assert_eq!(interactions(&h, &id), 0);
    assert_eq!(interactions(&h, &other_id), 1);
    h.shutdown().expect("shutdown");
}

/// Teardown rejects parked and deferred owners rather than letting late replies
/// execute input.
#[test]
fn agent_teardown_rejects_pending_ui_interactions() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let id = durable_agent_id_for_conversation(&h, &cid);
    intercept_interactions(&mut h);
    let ui = connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
    submit(&mut h, &id, "first pending teardown");
    submit(&mut h, &id, "second pending teardown");
    h.remove_agent(&cid);
    assert_eq!(ui_errors(&ui), 2);
    release_interaction(&mut h);
    assert_eq!(interactions(&h, &id), 0);
    assert!(
        !h.session_runtime
            .user_interaction_order
            .contains_key(id.as_str())
    );
    h.shutdown().expect("shutdown");
}

/// Final shutdown rejects both parked and deferred UI input before forced
/// publication draining.
#[test]
fn shutdown_rejects_pending_ui_interactions() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let id = durable_agent_id_for_conversation(&h, &cid);
    intercept_interactions(&mut h);
    let ui = connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
    submit(&mut h, &id, "first pending shutdown");
    submit(&mut h, &id, "second pending shutdown");
    h.shutdown().expect("shutdown");
    assert_eq!(ui_errors(&ui), 2);
    assert_eq!(interactions(&h, &id), 0);
    assert!(
        !h.session_runtime
            .user_interaction_order
            .contains_key(id.as_str())
    );
}

/// An input whose target becomes terminating while parked must fail before
/// semantic admission.
#[test]
fn stale_ui_interaction_owner_cannot_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let id = durable_agent_id_for_conversation(&h, &cid);
    intercept_interactions(&mut h);
    let ui = connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
    submit(&mut h, &id, "stale input");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .terminating = true;
    release_interaction(&mut h);
    assert_unaccepted(&h, &cid, &id);
    assert_eq!(ui_errors(&ui), 1);
    h.shutdown().expect("shutdown");
}

/// UI connection loss alone does not change the established lifecycle of
/// submitted input.
#[test]
fn ui_disconnect_does_not_cancel_pending_interaction() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let id = durable_agent_id_for_conversation(&h, &cid);
    intercept_interactions(&mut h);
    connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
    submit(&mut h, &id, "input outlives connection");
    h.runtime_io
        .bus
        .disconnect(&crate::test_connection_id("interaction-ui"));
    release_interaction(&mut h);
    assert_eq!(interactions(&h, &id), 1);
    assert!(event_log_events(&h).iter().any(|event| matches!(event,
        Event::AgentPromptSubmitted(prompt) if prompt.text == "input outlives connection")));
    h.shutdown().expect("shutdown");
}

/// Created-agent shutdown must return the pre-admission create rejection, not a
/// later prompt terminal.
#[test]
fn shutdown_rejects_pending_initial_ui_interaction() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    intercept_interactions(&mut h);
    let ui = connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
    let id = create_with_initial_prompt(&mut h);
    h.shutdown().expect("shutdown");
    assert_eq!(interactions(&h, &id), 0);
    let results: Vec<_> = ui
        .lock()
        .expect("UI frames")
        .iter()
        .filter_map(|frame| match peel_inner_event(&frame.frame) {
            Some(Event::UiCreateAgentResult(result)) => Some(result.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert!(
        matches!(&results[0].outcome, tau_proto::UiCreateAgentOutcome::Rejected {
        reason: tau_proto::UiCreateAgentRejection::InitialPromptFailed, agent_id: Some(agent_id), ..
    } if agent_id == &id)
    );
    assert!(!event_log_events(&h).iter().any(|event| matches!(event,
        Event::AgentPromptFailed(failed) if failed.agent_id == id)));
}

/// Ordinary committed interaction facts replay once, without reconstructing or
/// executing admission owners.
#[test]
fn committed_ui_interaction_replays_without_admission_effects() {
    let tmp = TempDir::new().expect("tempdir");
    let state_dir = tmp.path().join("state");
    let id = {
        let mut h = echo_harness(&state_dir).expect("harness");
        let cid = ensure_test_user_agent(&mut h);
        let id = durable_agent_id_for_conversation(&h, &cid);
        connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
        submit(&mut h, &id, "one accepted input");
        assert_eq!(interactions(&h, &id), 1);
        let prompt_id = h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .in_flight_prompt
            .clone()
            .expect("prompt");
        h.handle_provider_response_finished(provider_text_response(&prompt_id, id.clone(), "done"))
            .expect("complete prompt before restart");
        let late = connect_test_client(&mut h, "interaction-late", tau_proto::ClientKind::Ui);
        h.handle_client_event(
            "interaction-late",
            TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
                historical_selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_USER_INTERACTION_RECORDED,
                )],
                live_selectors: Vec::new(),
            })),
        )
        .expect("late subscribe");
        drive_harness_until_history_complete(&mut h);
        assert_eq!(late.lock().expect("late frames").iter().filter(|frame| matches!(
            &frame.frame, HarnessOutputMessage::Deliver(delivery)
                if delivery.replay && matches!(delivery.event.as_ref(), Event::AgentUserInteractionRecorded(_))
        )).count(), 1);
        assert_eq!(h.session_runtime.user_interaction_order[id.as_str()], 1);
        h.shutdown().expect("shutdown");
        id
    };
    let mut restored =
        echo_harness_with_start_reason("s1", &state_dir, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    assert_eq!(interactions(&restored, &id), 1);
    assert!(restored.session_runtime.user_interaction_order.is_empty());
    let cid = restored.agent_runtime.agent_registry.agent_routes[id.as_str()].clone();
    assert!(
        restored.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .is_empty()
    );
    assert!(
        restored.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .in_flight_prompt
            .is_none()
    );
    restored.shutdown().expect("shutdown restored");
}

/// Park the independently required terminal/accounting fact after UI ownership
/// is rejected.
fn intercept_teardown_obligation(h: &mut Harness, event: tau_proto::EventName) {
    connect_test_tool(h, "teardown-obligation");
    h.handle_extension_event(
        "teardown-obligation",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(event)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register obligation gate");
}

/// Release the teardown fact without treating it as an interaction reply.
fn release_teardown_obligation(h: &mut Harness) {
    h.handle_extension_event(
        "teardown-obligation",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release obligation");
}

/// Marked-inference teardown rejects pending input before waiting for its own
/// terminal commit.
#[test]
fn active_agent_teardown_rejects_ui_interaction_before_terminal_wait() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let id = durable_agent_id_for_conversation(&h, &cid);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("active work".to_owned()))
        .expect("dispatch");
    intercept_interactions(&mut h);
    intercept_teardown_obligation(&mut h, tau_proto::EventName::AGENT_PROMPT_TERMINATED);
    let ui = connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
    submit(&mut h, &id, "pending before active teardown");
    h.remove_agent(&cid);
    assert_eq!(ui_errors(&ui), 1);
    assert_eq!(interactions(&h, &id), 0);
    assert!(matches!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::AgentPromptTerminated(_))
    ));
    release_interaction(&mut h);
    assert_eq!(interactions(&h, &id), 0);
    release_teardown_obligation(&mut h);
    assert!(!h.agent_runtime.agent_registry.agents.contains_key(&cid));
    assert!(!event_log_events(&h).iter().any(|event| matches!(event,
        Event::AgentPromptSubmitted(prompt) if prompt.text == "pending before active teardown")));
    h.shutdown().expect("shutdown");
}

/// Start genuine standalone work so teardown must preserve its final accounting
/// obligation.
fn start_standalone(h: &mut Harness, id: &AgentId) {
    enable_remote_compaction_for_test_model(h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        h.session_runtime.current_session_id.clone(),
        Some(id.as_str()),
    );
    assert!(
        !h.prompt_coordination
            .standalone_accounting
            .owners
            .is_empty()
    );
}

/// Standalone accounting may delay retirement, but cannot delay rejection or
/// admit new UI input.
#[test]
fn standalone_agent_teardown_rejects_ui_interaction_before_accounting_wait() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let id = durable_agent_id_for_conversation(&h, &cid);
    start_standalone(&mut h, &id);
    intercept_interactions(&mut h);
    intercept_teardown_obligation(
        &mut h,
        tau_proto::EventName::PROVIDER_STANDALONE_EXECUTION_ACCOUNTED,
    );
    let ui = connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
    submit(&mut h, &id, "pending before standalone teardown");
    h.remove_agent(&cid);
    assert_eq!(ui_errors(&ui), 1);
    assert_eq!(interactions(&h, &id), 0);
    assert!(h.has_unsettled_standalone_accounting_for(&cid));
    release_interaction(&mut h);
    assert_eq!(interactions(&h, &id), 0);
    release_teardown_obligation(&mut h);
    assert!(!h.agent_runtime.agent_registry.agents.contains_key(&cid));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event,
        Event::ProviderStandaloneExecutionAccounted(accounted) if accounted.agent_id == id))
            .count(),
        1
    );
    h.shutdown().expect("shutdown");
}

/// Shutdown rejects pending input before force-settling standalone accounting.
#[test]
fn standalone_shutdown_rejects_ui_interaction_before_accounting_settlement() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let id = durable_agent_id_for_conversation(&h, &cid);
    start_standalone(&mut h, &id);
    intercept_interactions(&mut h);
    intercept_teardown_obligation(
        &mut h,
        tau_proto::EventName::PROVIDER_STANDALONE_EXECUTION_ACCOUNTED,
    );
    let ui = connect_test_client(&mut h, "interaction-ui", tau_proto::ClientKind::Ui);
    submit(&mut h, &id, "pending before accounting shutdown");
    h.shutdown().expect("force-settle parked accounting");
    assert_eq!(ui_errors(&ui), 1);
    assert_eq!(interactions(&h, &id), 0);
    assert!(!h.has_unsettled_standalone_accounting_publication());
    assert_eq!(interactions(&h, &id), 0);
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event,
        Event::ProviderStandaloneExecutionAccounted(accounted) if accounted.agent_id == id))
            .count(),
        1
    );
}
