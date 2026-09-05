use tau_config::settings::WaitTimeoutMinutes;

use super::*;
use crate::background_completion_preview::BackgroundPreviewBudget;
use crate::harness::subagents_tool::wait_tool_spec;

fn conv(id: &str) -> AgentId {
    crate::parse_agent_id(id)
}

fn observation() -> Option<tau_proto::ObservationId> {
    Some(tau_proto::ObservationId::random())
}

fn wait_args_empty() -> CborValue {
    CborValue::Map(Vec::new())
}

fn wait_args_exact(call_id: &str) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("tool_call_id".to_owned()),
        CborValue::Text(call_id.to_owned()),
    )])
}

fn wait_args_all(call_ids: &[&str]) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("tool_call_ids".to_owned()),
        CborValue::Array(
            call_ids
                .iter()
                .map(|call_id| CborValue::Text((*call_id).to_owned()))
                .collect(),
        ),
    )])
}

fn wait_args_input(minutes: i64) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("timeout_minutes".to_owned()),
        CborValue::Integer(minutes.into()),
    )])
}

fn wait_tool_name() -> ToolName {
    ToolName::new(WAIT_TOOL_NAME)
}

fn slow_tool_name() -> ToolName {
    ToolName::new("slow")
}

fn background_placeholder(call_id: &str) -> ToolResult {
    ToolResult {
        presentation: Default::default(),
        call_id: call_id.into(),
        tool_name: slow_tool_name(),
        tool_type: ToolType::Function,
        result: CborValue::Text("still running".to_owned()),
        provider_content: Vec::new(),
        kind: ToolResultKind::BackgroundPlaceholder,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn foreground_result(call_id: &str, payload: &str) -> ToolResult {
    ToolResult {
        presentation: Default::default(),
        call_id: call_id.into(),
        tool_name: slow_tool_name(),
        tool_type: ToolType::Function,
        result: CborValue::Text(payload.to_owned()),
        provider_content: vec![tau_proto::ToolResultContentPart::Image(
            tau_proto::ImageContent {
                media_type: tau_proto::ImageMediaType::Png,
                data: vec![7; 64 * 1024].into(),
                width: 1,
                height: 1,
                detail: tau_proto::ImageDetail::High,
            },
        )],
        kind: ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn foreground_error(call_id: &str, payload: &str) -> ToolError {
    ToolError {
        call_id: call_id.into(),
        tool_name: slow_tool_name(),
        tool_type: ToolType::Function,
        message: payload.to_owned(),
        details: Some(CborValue::Text(payload.to_owned())),
        presentation: Default::default(),
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn background_result(call_id: &str, text: &str) -> ToolBackgroundResult {
    ToolBackgroundResult {
        call_id: call_id.into(),
        tool_name: slow_tool_name(),
        tool_type: ToolType::Function,
        result: CborValue::Text(text.to_owned()),
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn background_error(
    call_id: &str,
    message: &str,
    details: Option<CborValue>,
) -> ToolBackgroundError {
    ToolBackgroundError {
        call_id: call_id.into(),
        tool_name: slow_tool_name(),
        tool_type: ToolType::Function,
        message: message.to_owned(),
        details,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn start_wait_any(tracker: &mut WaitTracker, owner: &AgentId, call_id: &str) -> WaitStart {
    tracker.handle_wait_invoke(
        owner,
        call_id.into(),
        wait_tool_name(),
        &wait_args_empty(),
        observation(),
    )
}

fn start_wait_exact(
    tracker: &mut WaitTracker,
    owner: &AgentId,
    wait_call_id: &str,
    target_call_id: &str,
) -> WaitStart {
    tracker.handle_wait_invoke(
        owner,
        wait_call_id.into(),
        wait_tool_name(),
        &wait_args_exact(target_call_id),
        observation(),
    )
}

fn start_wait_all(
    tracker: &mut WaitTracker,
    owner: &AgentId,
    wait_call_id: &str,
    target_call_ids: &[&str],
) -> WaitStart {
    tracker.handle_wait_invoke(
        owner,
        wait_call_id.into(),
        wait_tool_name(),
        &wait_args_all(target_call_ids),
        observation(),
    )
}

fn track_call(tracker: &mut WaitTracker, owner: &AgentId, call_id: &str, reference: ToolCallRef) {
    tracker.reset_call_ref(call_id.into(), reference);
    tracker.record_tool_invoke(call_id.into(), slow_tool_name(), owner.clone());
}

fn start_wait_input(tracker: &mut WaitTracker, owner: &AgentId, call_id: &str) -> WaitStart {
    tracker.handle_wait_invoke(
        owner,
        call_id.into(),
        wait_tool_name(),
        &wait_args_input(1),
        observation(),
    )
}

fn call_ref(byte: u8, item_index: u32) -> ToolCallRef {
    ToolCallRef {
        declaration: tau_proto::ObservationId::from_bytes([byte; 16]),
        item_index,
    }
}

/// Unclaimed foreground terminals remain caller-owned while the tracker records
/// only its small normal-returned tombstone.
#[test]
fn unclaimed_foreground_terminals_remain_borrowed_and_normal_returned() {
    let owner = conv("main");
    let payload = "large terminal payload".repeat(4 * 1024);
    let result = foreground_result("result", &payload);
    let error = foreground_error("error", &payload);
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke(
        result.call_id.clone(),
        result.tool_name.clone(),
        owner.clone(),
    );
    tracker.record_tool_invoke(
        error.call_id.clone(),
        error.tool_name.clone(),
        owner.clone(),
    );

    assert!(
        tracker
            .record_tool_result(&result, owner.clone(), observation())
            .is_empty()
    );
    assert!(
        tracker
            .record_tool_error(&error, owner, observation())
            .is_empty()
    );

    assert_eq!(result.result, CborValue::Text(payload.clone()));
    assert_eq!(result.provider_content.len(), 1);
    assert_eq!(error.message, payload);
    assert_eq!(
        tracker.calls.get(&result.call_id),
        Some(&WaitCallState::NormalReturned)
    );
    assert_eq!(
        tracker.calls.get(&error.call_id),
        Some(&WaitCallState::NormalReturned)
    );
}

/// A background completion that arrives while compaction holds an exact wait
/// must remain the rollback winner even if activating input arrives afterward.
#[test]
fn compaction_claim_rollback_preserves_first_exact_completion_winner() {
    let owner = conv("main");
    let target: ToolCallId = "claimed-target".into();
    let wait_call: ToolCallId = "claimed-exact-wait".into();
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke(target.clone(), slow_tool_name(), owner.clone());
    tracker.record_tool_result(
        &background_placeholder(target.as_str()),
        owner.clone(),
        observation(),
    );
    assert!(
        start_wait_exact(&mut tracker, &owner, wait_call.as_str(), target.as_str())
            .reply
            .is_none()
    );
    assert!(tracker.claim_wait_for_manual_compaction(&owner, &wait_call));

    assert!(
        tracker
            .record_background_result(
                background_result(target.as_str(), "completed first"),
                owner.clone(),
                observation(),
            )
            .is_empty()
    );
    assert!(
        tracker
            .activate_waits_for(&owner, tau_proto::ObservationId::random())
            .is_empty()
    );
    let replies = tracker.rollback_manual_compaction_claim(&owner, &wait_call);

    assert_eq!(replies.len(), 1);
    assert!(matches!(
        &replies[0].kind,
        WaitReplyKind::Result {
            result: CborValue::Text(text),
            display: _,
        } if text == "completed first"
    ));
}

/// Committing compact preemption after the exact source completes must restore
/// that source's passive notification instead of leaving it suppressed.
#[test]
fn compaction_claim_cancellation_unsuppresses_completed_exact_source() {
    let owner = conv("main");
    let target: ToolCallId = "claimed-completed-target".into();
    let wait_call: ToolCallId = "claimed-cancelled-wait".into();
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke(target.clone(), slow_tool_name(), owner.clone());
    tracker.record_tool_result(
        &background_placeholder(target.as_str()),
        owner.clone(),
        observation(),
    );
    assert!(
        start_wait_exact(&mut tracker, &owner, wait_call.as_str(), target.as_str())
            .reply
            .is_none()
    );
    assert!(tracker.claim_wait_for_manual_compaction(&owner, &wait_call));
    assert!(
        tracker
            .record_background_result(
                background_result(target.as_str(), "source completed"),
                owner,
                observation(),
            )
            .is_empty()
    );

    let cancelled = tracker.record_tool_cancelled(&HashSet::from([wait_call]), None);

    assert_eq!(cancelled.unsuppress_call_ids, vec![target]);
    assert!(cancelled.replies.is_empty());
}

/// Owner teardown retires a claimed wait as installed state and returns its
/// call identity for the harness's outer tracking cleanup.
#[test]
fn owner_teardown_retires_claimed_wait() {
    let owner = conv("main");
    let target: ToolCallId = "claimed-teardown-target".into();
    let wait_call: ToolCallId = "claimed-teardown-wait".into();
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke(target.clone(), slow_tool_name(), owner.clone());
    assert!(
        start_wait_exact(&mut tracker, &owner, wait_call.as_str(), target.as_str())
            .reply
            .is_none()
    );
    assert!(tracker.claim_wait_for_manual_compaction(&owner, &wait_call));
    assert!(tracker.installed_wait_owners().contains(&owner));

    let retired = tracker.discard_owner(&owner);

    assert!(retired.contains(&wait_call));
    assert!(!tracker.installed_wait_owners().contains(&owner));
    assert!(!tracker.wait_claimed_for_manual_compaction(&owner, &wait_call));
}

/// A background error racing a claimed exact wait has the same notification
/// restoration contract as a successful source completion.
#[test]
fn compaction_claim_cancellation_unsuppresses_errored_exact_source() {
    let owner = conv("main");
    let target: ToolCallId = "claimed-errored-target".into();
    let wait_call: ToolCallId = "claimed-error-wait".into();
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke(target.clone(), slow_tool_name(), owner.clone());
    tracker.record_tool_result(
        &background_placeholder(target.as_str()),
        owner.clone(),
        observation(),
    );
    assert!(
        start_wait_exact(&mut tracker, &owner, wait_call.as_str(), target.as_str())
            .reply
            .is_none()
    );
    assert!(tracker.claim_wait_for_manual_compaction(&owner, &wait_call));
    assert!(
        tracker
            .record_background_error(
                background_error(target.as_str(), "source failed", None),
                owner,
                observation(),
                BackgroundErrorOutcome::Error,
            )
            .is_empty()
    );

    let cancelled = tracker.record_tool_cancelled(&HashSet::from([wait_call]), None);

    assert_eq!(cancelled.unsuppress_call_ids, vec![target]);
    assert!(cancelled.replies.is_empty());
}

/// Bare waits use the same exclusive claim and canonical cancellation path
/// without consuming or cancelling their running background source.
#[test]
fn compaction_claim_cancels_bare_wait_without_source_consumption() {
    let owner = conv("main");
    let target: ToolCallId = "claimed-bare-target".into();
    let wait_call: ToolCallId = "claimed-bare-wait".into();
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke(target.clone(), slow_tool_name(), owner.clone());
    tracker.record_tool_result(
        &background_placeholder(target.as_str()),
        owner.clone(),
        observation(),
    );
    assert!(
        start_wait_any(&mut tracker, &owner, wait_call.as_str())
            .reply
            .is_none()
    );
    assert!(tracker.claim_wait_for_manual_compaction(&owner, &wait_call));

    let cancelled = tracker.record_tool_cancelled(&HashSet::from([wait_call.clone()]), None);

    assert_eq!(cancelled.cancelled_waits.len(), 1);
    assert_eq!(cancelled.cancelled_waits[0].call_id, wait_call);
    assert!(tracker.is_backgrounded(&target));
}

/// Activating input observed during an exclusive claim settles the restored
/// input waiter exactly once if cancellation append rolls back.
#[test]
fn compaction_claim_rollback_replays_activation_winner() {
    let owner = conv("main");
    let wait_call: ToolCallId = "claimed-input-activation".into();
    let mut tracker = WaitTracker::default();
    assert!(
        start_wait_input(&mut tracker, &owner, wait_call.as_str())
            .reply
            .is_none()
    );
    assert!(tracker.claim_wait_for_manual_compaction(&owner, &wait_call));
    let activation = tau_proto::ObservationId::random();
    assert!(tracker.activate_waits_for(&owner, activation).is_empty());

    let replies = tracker.rollback_manual_compaction_claim(&owner, &wait_call);

    assert_eq!(replies.len(), 1);
    assert!(matches!(
        &replies[0].kind,
        WaitReplyKind::Result {
            result: CborValue::Map(entries),
            display: _,
        } if entries == &vec![(
            CborValue::Text("input_available".to_owned()),
            CborValue::Bool(true),
        )]
    ));
}

/// A claimed input deadline stays scheduled but cannot publish until append
/// rollback restores the waiter and its first timeout winner.
#[test]
fn compaction_claim_rollback_replays_timeout_winner() {
    let owner = conv("main");
    let wait_call: ToolCallId = "claimed-input-timeout".into();
    let mut tracker = WaitTracker::default();
    assert!(
        start_wait_input(&mut tracker, &owner, wait_call.as_str())
            .reply
            .is_none()
    );
    assert!(tracker.claim_wait_for_manual_compaction(&owner, &wait_call));
    let deadline = tracker
        .next_input_wait_deadline()
        .expect("claimed deadline remains scheduled");
    assert!(tracker.expire_input_waits(deadline).is_empty());

    let replies = tracker.rollback_manual_compaction_claim(&owner, &wait_call);

    assert_eq!(replies.len(), 1);
    assert!(matches!(
        &replies[0].kind,
        WaitReplyKind::Result {
            result: CborValue::Map(entries),
            display: Some(display),
        } if entries == &vec![(
            CborValue::Text("timed_out".to_owned()),
            CborValue::Bool(true),
        )] && display.status == ToolUseStatus::Warning
    ));
}

/// Terminal wait-correlation tombstones retain recent duplicate diagnostics but
/// cannot grow every correlation map without bound.
#[test]
fn terminal_wait_correlation_is_bounded() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    let active: ToolCallId = "active".into();
    tracker.record_tool_invoke(active.clone(), slow_tool_name(), owner.clone());

    for index in 0..(MAX_WAIT_TERMINAL_TOMBSTONES + 17) {
        let call_id: ToolCallId = format!("finished-{index}").into();
        tracker
            .call_refs
            .insert(call_id.clone(), call_ref((index % 251) as u8, 0));
        tracker
            .terminal_observations
            .insert(call_id.clone(), tau_proto::ObservationId::random());
        tracker.call_owners.insert(call_id.clone(), owner.clone());
        tracker
            .call_tool_names
            .insert(call_id.clone(), slow_tool_name());
        tracker.record_terminal_state(call_id, WaitCallState::NormalReturned);
    }

    assert_eq!(tracker.terminal_order.len(), MAX_WAIT_TERMINAL_TOMBSTONES);
    assert_eq!(tracker.calls.len(), MAX_WAIT_TERMINAL_TOMBSTONES + 1);
    assert_eq!(tracker.call_refs.len(), MAX_WAIT_TERMINAL_TOMBSTONES);
    assert_eq!(
        tracker.terminal_observations.len(),
        MAX_WAIT_TERMINAL_TOMBSTONES
    );
    assert_eq!(tracker.call_owners.len(), MAX_WAIT_TERMINAL_TOMBSTONES + 1);
    assert_eq!(
        tracker.call_tool_names.len(),
        MAX_WAIT_TERMINAL_TOMBSTONES + 1
    );
    assert!(matches!(
        tracker.calls.get(&active),
        Some(WaitCallState::Pending)
    ));
}

/// Teardown-discarded background calls retire every correlation map instead of
/// bypassing terminal tombstone eviction forever.
#[test]
fn discarded_background_correlation_is_fully_retired() {
    let owner = conv("side");
    let mut tracker = WaitTracker::default();
    for index in 0..2048 {
        let call_id: ToolCallId = format!("discarded-{index}").into();
        tracker.record_tool_invoke(call_id.clone(), slow_tool_name(), owner.clone());
        tracker
            .call_refs
            .insert(call_id.clone(), call_ref((index % 251) as u8, 0));
        tracker
            .terminal_observations
            .insert(call_id.clone(), tau_proto::ObservationId::random());
        tracker
            .calls
            .insert(call_id.clone(), WaitCallState::Backgrounded);
        tracker.push_completed(call_id.clone());
        tracker.discard_owner(&owner);
    }

    assert!(tracker.calls.is_empty());
    assert!(tracker.call_refs.is_empty());
    assert!(tracker.terminal_observations.is_empty());
    assert!(tracker.call_owners.is_empty());
    assert!(tracker.call_tool_names.is_empty());
    assert!(tracker.completion_order_by_owner.is_empty());
    assert!(tracker.completed_membership.is_empty());
    assert!(tracker.terminal_order.is_empty());
}

/// Unloading one agent retires all of its source calls and every installed wait
/// mode while preserving another agent's calls and waits.
#[test]
fn discarded_owner_retires_source_exact_bare_and_input_wait_state() {
    let owner = conv("side");
    let other = conv("other");
    let mut tracker = WaitTracker::default();
    for (call_id, call_owner) in [
        ("source", owner.clone()),
        ("wait-exact", owner.clone()),
        ("wait-bare", owner.clone()),
        ("wait-input", owner.clone()),
        ("other-source", other.clone()),
    ] {
        tracker.record_tool_invoke(call_id.into(), slow_tool_name(), call_owner);
    }
    tracker.calls.insert(
        "source".into(),
        WaitCallState::BackgroundResult(background_result("source", "done")),
    );
    tracker.push_completed("source".into());
    let wait = |call_id: &str, wait_owner: AgentId| WaitRequest {
        call_id: call_id.into(),
        tool_name: wait_tool_name(),
        owner: wait_owner,
        display_args: String::new(),
        call_ref: Some(call_ref(1, 0)),
        wait_observation: observation(),
        registration: observation(),
    };
    tracker
        .waiters
        .insert("source".into(), wait("wait-exact", owner.clone()));
    tracker
        .any_waiters
        .insert(owner.clone(), wait("wait-bare", owner.clone()));
    tracker.input_waiters.insert(
        owner.clone(),
        InputWaitRequest {
            request: wait("wait-input", owner.clone()),
            deadline: Instant::now() + Duration::from_secs(60),
        },
    );
    tracker
        .any_waiters
        .insert(other.clone(), wait("other-wait", other.clone()));

    let retired = tracker.discard_owner(&owner);

    for call_id in ["source", "wait-exact", "wait-bare", "wait-input"] {
        assert!(retired.contains(&ToolCallId::from(call_id)));
        assert!(!tracker.calls.contains_key(call_id));
        assert!(!tracker.call_owners.contains_key(call_id));
        assert!(!tracker.call_refs.contains_key(call_id));
        assert!(!tracker.terminal_observations.contains_key(call_id));
    }
    assert!(tracker.waiters.is_empty());
    assert!(!tracker.any_waiters.contains_key(&owner));
    assert!(!tracker.input_waiters.contains_key(&owner));
    assert!(tracker.calls.contains_key("other-source"));
    assert!(tracker.any_waiters.contains_key(&other));
}

/// Provider-declared exact, next-background, and activating-input waits retain
/// exact references and allocate registration identity only when installed.
#[test]
fn installed_wait_modes_emit_exact_registration_metadata() {
    let owner = conv("main");

    let mut exact = WaitTracker::default();
    exact.record_tool_invoke("source".into(), slow_tool_name(), owner.clone());
    exact.call_refs.insert("source".into(), call_ref(1, 0));
    exact.call_refs.insert("wait-exact".into(), call_ref(2, 1));
    let wait_observation = tau_proto::ObservationId::from_bytes([41; 16]);
    let start = exact.handle_wait_invoke(
        &owner,
        "wait-exact".into(),
        wait_tool_name(),
        &wait_args_exact("source"),
        Some(wait_observation),
    );
    let (registration_id, registration) = start.registration.expect("exact registration");
    assert_eq!(registration.wait_observation, wait_observation);
    assert_eq!(registration.wait_call, call_ref(2, 1));
    assert_eq!(
        registration.mode,
        tau_proto::ToolWaitMode::Exact {
            target: call_ref(1, 0)
        }
    );
    assert_eq!(exact.waiters["source"].registration, Some(registration_id));

    let mut any = WaitTracker::default();
    any.record_tool_invoke("source".into(), slow_tool_name(), owner.clone());
    any.call_refs.insert("source".into(), call_ref(3, 0));
    any.calls
        .insert("source".into(), WaitCallState::Backgrounded);
    any.call_refs.insert("wait-any".into(), call_ref(4, 1));
    let start = start_wait_any(&mut any, &owner, "wait-any");
    assert_eq!(
        start.registration.expect("any registration").1.mode,
        tau_proto::ToolWaitMode::NextBackground
    );

    let mut input = WaitTracker::default();
    input.call_refs.insert("wait-input".into(), call_ref(5, 2));
    let start = start_wait_input(&mut input, &owner, "wait-input");
    assert_eq!(
        start.registration.expect("input registration").1.mode,
        tau_proto::ToolWaitMode::ActivatingInput {
            effective_timeout_minutes: 1
        }
    );

    exact
        .call_refs
        .insert("wait-rejected".into(), call_ref(6, 3));
    assert!(
        start_wait_exact(&mut exact, &owner, "wait-rejected", "missing")
            .registration
            .is_none()
    );
}

/// An already-completed source produces an immediate settlement with fixed
/// declaration and canonical terminal occurrences.
#[test]
fn immediate_completion_settlement_keeps_fixed_durable_endpoints() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("source".into(), slow_tool_name(), owner.clone());
    tracker.call_refs.insert("source".into(), call_ref(1, 0));
    tracker.record_tool_result(&background_placeholder("source"), owner.clone(), None);
    let terminal = tau_proto::ObservationId::from_bytes([42; 16]);
    tracker.record_background_result(
        background_result("source", "done"),
        owner.clone(),
        Some(terminal),
    );
    tracker.call_refs.insert("wait".into(), call_ref(2, 0));
    let wait_observation = tau_proto::ObservationId::from_bytes([43; 16]);
    let reply = tracker
        .handle_wait_invoke(
            &owner,
            "wait".into(),
            wait_tool_name(),
            &wait_args_exact("source"),
            Some(wait_observation),
        )
        .reply
        .expect("immediate reply");
    assert_eq!(
        reply.settlement,
        Some(PendingWaitSettlement {
            wait_observation,
            wait_call: call_ref(2, 0),
            registration: None,
            wait_terminal: None,
            outcome: tau_proto::ToolWaitOutcome::CompletionDelivered {
                source_call: call_ref(1, 0),
                source_terminal: terminal,
                source_phase: tau_proto::ToolSourcePhase::Background,
                envelope: tau_proto::ToolOutputEnvelope::Identity,
            },
        })
    );
}

/// Registered delivery retains registration and source endpoints, while a
/// completion lacking durable source correlation deliberately emits none.
#[test]
fn registered_and_unavailable_completion_settlements_are_explicit() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("source".into(), slow_tool_name(), owner.clone());
    tracker.call_refs.insert("source".into(), call_ref(1, 0));
    tracker.record_tool_result(&background_placeholder("source"), owner.clone(), None);
    tracker.call_refs.insert("wait".into(), call_ref(2, 0));
    let wait_observation = tau_proto::ObservationId::from_bytes([44; 16]);
    let start = tracker.handle_wait_invoke(
        &owner,
        "wait".into(),
        wait_tool_name(),
        &wait_args_exact("source"),
        Some(wait_observation),
    );
    let registration = start.registration.expect("registration").0;
    let terminal = tau_proto::ObservationId::from_bytes([45; 16]);
    let reply = tracker
        .record_background_result(
            background_result("source", "done"),
            owner.clone(),
            Some(terminal),
        )
        .pop()
        .expect("registered completion");
    assert!(matches!(
        reply.settlement,
        Some(PendingWaitSettlement {
            wait_observation: actual_observation,
            wait_call: actual_wait,
            registration: Some(actual_registration),
            wait_terminal: _,
            outcome: tau_proto::ToolWaitOutcome::CompletionDelivered {
                source_call,
                source_terminal,
                ..
            },
        }) if actual_observation == wait_observation
            && actual_wait == call_ref(2, 0)
            && actual_registration == registration
            && source_call == call_ref(1, 0)
            && source_terminal == terminal
    ));

    let mut unavailable = WaitTracker::default();
    unavailable.record_tool_invoke("source".into(), slow_tool_name(), owner.clone());
    unavailable.record_tool_result(&background_placeholder("source"), owner.clone(), None);
    unavailable.call_refs.insert("wait".into(), call_ref(3, 0));
    unavailable.handle_wait_invoke(
        &owner,
        "wait".into(),
        wait_tool_name(),
        &wait_args_exact("source"),
        observation(),
    );
    let reply = unavailable
        .record_background_result(background_result("source", "done"), owner, None)
        .pop()
        .expect("uncorrelated completion reply");
    assert!(reply.settlement.is_none());
}

fn start_reply(start: WaitStart) -> WaitReply {
    start.reply.expect("wait reply")
}

fn reply_result(reply: WaitReply) -> CborValue {
    match reply.kind {
        WaitReplyKind::Result { result, .. } => result,
        other => panic!("expected result reply, got {other:?}"),
    }
}

fn reply_result_with_display(reply: WaitReply) -> (CborValue, Option<ToolUseState>) {
    match reply.kind {
        WaitReplyKind::Result { result, display } => (result, display),
        other => panic!("expected result reply, got {other:?}"),
    }
}

fn reply_error(reply: WaitReply) -> (String, Option<CborValue>) {
    match reply.kind {
        WaitReplyKind::Error {
            message, details, ..
        } => (message, details),
        other => panic!("expected error reply, got {other:?}"),
    }
}

fn reply_error_with_display(reply: WaitReply) -> (String, Option<CborValue>, Option<ToolUseState>) {
    match reply.kind {
        WaitReplyKind::Error {
            message,
            details,
            display,
        } => (message, details, display),
        other => panic!("expected error reply, got {other:?}"),
    }
}

fn cbor_map_text<'a>(value: &'a CborValue, key: &str) -> Option<&'a str> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries.iter().find_map(|(entry_key, entry_value)| {
        matches!(entry_key, CborValue::Text(text) if text == key)
            .then_some(entry_value)
            .and_then(|value| match value {
                CborValue::Text(text) => Some(text.as_str()),
                _ => None,
            })
    })
}

/// The no-arg form is intentional, so the agent-visible schema must not force
/// `tool_call_id` even though the exact-id form remains available.
#[test]
fn wait_tool_schema_does_not_require_tool_call_id() {
    let spec = wait_tool_spec();
    let parameters = spec.parameters.expect("parameters");
    let required = parameters
        .get("required")
        .and_then(serde_json::Value::as_array);

    assert!(required.is_none_or(|items| {
        items
            .iter()
            .all(|item| item.as_str() != Some("tool_call_id"))
    }));
}

/// `wait` returns the original background result to the model, but its UI
/// descriptor must describe the `wait` call itself. The source tool name is
/// useful context; source args/payload/stats are not, because rendering those
/// under the `wait` name made the transcript claim e.g. `wait cargo test`.
#[test]
fn wait_result_display_uses_wait_descriptor_with_source_tool_name() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    let source_display = ToolUseState {
        args: "cargo test".to_owned(),
        stats: tau_proto::ToolUseStats {
            matches: None,
            lines: Some(10),
            bytes: Some(200),
        },
        status: ToolUseStatus::Error,
        status_text: "1".to_owned(),
        payload: Some(tau_proto::ToolUsePayload::Text {
            text: "cargo test".to_owned(),
        }),
        ..Default::default()
    };
    assert!(
        tracker
            .record_background_result(
                ToolBackgroundResult {
                    call_id: "bg-shell".into(),
                    tool_name: ToolName::new("shell"),
                    tool_type: ToolType::Function,
                    result: CborValue::Text("done".to_owned()),
                    display: Some(source_display),
                    originator: tau_proto::PromptOriginator::User,
                },
                owner.clone(),
                observation()
            )
            .is_empty()
    );

    let (result, display) = reply_result_with_display(start_reply(start_wait_any(
        &mut tracker,
        &owner,
        "wait-shell",
    )));
    assert_eq!(
        cbor_map_text(&result, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-shell")
    );
    assert_eq!(cbor_map_text(&result, "output"), Some("done"));

    let display = display.expect("wait display");
    assert_eq!(display.args, "shell");
    assert_eq!(display.status, ToolUseStatus::Error);
    assert_eq!(display.status_text, "1");
    assert!(display.stats.is_empty());
    assert!(display.payload.is_none());
}

/// `wait({})` is now the shorthand for waiting on any background
/// completion scoped to the current conversation.
#[test]
fn wait_args_omitted_tool_call_id_parse_as_any_background() {
    assert_eq!(
        parse_wait_args(&wait_args_empty()),
        Ok(WaitTarget::AnyBackground)
    );
    let unrelated = CborValue::Map(vec![(
        CborValue::Text("unused".to_owned()),
        CborValue::Text("ignored".to_owned()),
    )]);
    assert_eq!(parse_wait_args(&unrelated), Ok(WaitTarget::AnyBackground));
}

/// A bare wait with no candidates points callers at the explicit input mode
/// rather than leaving an indefinite wait as an accidental interpretation.
#[test]
fn no_arg_wait_without_candidates_has_actionable_input_guidance() {
    let mut tracker = WaitTracker::default();
    let reply = start_reply(start_wait_any(&mut tracker, &conv("main"), "wait-empty"));
    assert_eq!(reply_error(reply).0, NO_BACKGROUND_WAIT_CANDIDATES);
}

/// Invalid explicit ids still fail early so a typo does not silently turn
/// into a broad no-arg wait.
#[test]
fn wait_args_reject_non_string_and_empty_tool_call_id() {
    let non_string = CborValue::Map(vec![(
        CborValue::Text("tool_call_id".to_owned()),
        CborValue::Bool(true),
    )]);
    assert_eq!(
        parse_wait_args(&non_string),
        Err("`tool_call_id` must be a string".to_owned())
    );
    assert_eq!(
        parse_wait_args(&wait_args_exact("   ")),
        Err("`tool_call_id` must not be empty".to_owned())
    );
}

/// Input waits accept positive whole minutes, clamp to the default one through
/// 1,440-minute range before conversion, reject ambiguous or malformed forms,
/// and explicitly diagnose the removed boolean.
#[test]
fn wait_args_parse_explicit_input_mode_and_reject_ambiguous_forms() {
    for (minutes, effective) in [
        (1, 1),
        (1_439, 1_439),
        (1_440, 1_440),
        (1_441, 1_440),
        (i64::MAX, 1_440),
    ] {
        assert_eq!(
            parse_wait_args(&wait_args_input(minutes)),
            Ok(WaitTarget::AnyInput(Duration::from_secs(effective * 60)))
        );
    }
    assert_eq!(
        parse_wait_args(&CborValue::Map(vec![(
            CborValue::Text("timeout_minutes".to_owned()),
            CborValue::Integer(u64::MAX.into()),
        )])),
        Ok(WaitTarget::AnyInput(Duration::from_secs(1_440 * 60)))
    );
    for minutes in [0, -1] {
        assert_eq!(
            parse_wait_args(&wait_args_input(minutes)),
            Err("`timeout_minutes` must be at least 1".to_owned())
        );
    }
    for value in [
        CborValue::Float(1.0),
        CborValue::Float(1.5),
        CborValue::Text("1".to_owned()),
        CborValue::Bool(true),
        CborValue::Null,
    ] {
        assert_eq!(
            parse_wait_args(&CborValue::Map(vec![(
                CborValue::Text("timeout_minutes".to_owned()),
                value,
            )])),
            Err("`timeout_minutes` must be an integer".to_owned())
        );
    }
    for entries in [
        vec![
            (
                CborValue::Text("timeout_minutes".to_owned()),
                CborValue::Integer(1.into()),
            ),
            (
                CborValue::Text("tool_call_id".to_owned()),
                CborValue::Text("call".to_owned()),
            ),
        ],
        vec![
            (
                CborValue::Text("tool_call_id".to_owned()),
                CborValue::Text("call".to_owned()),
            ),
            (
                CborValue::Text("timeout_minutes".to_owned()),
                CborValue::Integer(1.into()),
            ),
        ],
    ] {
        assert_eq!(
            parse_wait_args(&CborValue::Map(entries)),
            Err("`tool_call_id` and `timeout_minutes` are mutually exclusive".to_owned())
        );
    }
    assert_eq!(
        parse_wait_args(&CborValue::Map(vec![
            (
                CborValue::Text("timeout_minutes".to_owned()),
                CborValue::Integer(1.into()),
            ),
            (
                CborValue::Text("timeout_minutes".to_owned()),
                CborValue::Integer(2.into()),
            ),
        ])),
        Err("`timeout_minutes` must not be repeated".to_owned())
    );
    assert_eq!(
        parse_wait_args(&CborValue::Map(vec![(
            CborValue::Text("any_input".to_owned()),
            CborValue::Bool(true),
        )])),
        Err(
            "`any_input` is no longer supported; use `timeout_minutes` with a positive integer"
                .to_owned()
        )
    );
}

/// Ensures the shipped one-minute floor honors a requested one-minute deadline
/// while activating input still settles it immediately.
#[test]
fn default_input_wait_floor_preserves_early_activation() {
    let now = Instant::now();
    let owner = conv("owner");
    let mut tracker = WaitTracker::default();
    let start = tracker.handle_wait_invoke_at(
        &owner,
        "wait-floor".into(),
        wait_tool_name(),
        &wait_args_input(1),
        now,
        observation(),
    );

    assert!(start.reply.is_none());
    assert_eq!(
        tracker.next_input_wait_deadline(),
        Some(now + Duration::from_secs(60))
    );
    let replies = tracker.activate_waits_for(&owner, tau_proto::ObservationId::random());
    assert_eq!(replies.len(), 1);
    let (result, display) =
        reply_result_with_display(replies.into_iter().next().expect("activation reply"));
    assert_eq!(
        result,
        CborValue::Map(vec![(
            CborValue::Text("input_available".to_owned()),
            CborValue::Bool(true),
        )])
    );
    assert_eq!(display.expect("input display").args, "1m");
    assert_eq!(tracker.next_input_wait_deadline(), None);
}

/// Ensures a configured range clamps requests at both ends while leaving
/// argument-free background-result waits on their existing path.
#[test]
fn configured_input_wait_bounds_clamp_both_ends_without_affecting_background_waits() {
    let now = Instant::now();
    let owner = conv("owner");
    let mut tracker = WaitTracker::with_input_wait_timeout_bounds(
        WaitTimeoutBounds::new(
            WaitTimeoutMinutes::new(7).expect("positive minimum"),
            WaitTimeoutMinutes::new(9).expect("positive maximum"),
        )
        .expect("ordered bounds"),
    );

    assert!(
        tracker
            .handle_wait_invoke_at(
                &owner,
                "wait-low".into(),
                wait_tool_name(),
                &wait_args_input(1),
                now,
                observation(),
            )
            .reply
            .is_none()
    );
    assert_eq!(
        tracker.next_input_wait_deadline(),
        Some(now + Duration::from_secs(7 * 60))
    );
    assert_eq!(
        tracker
            .activate_waits_for(&owner, tau_proto::ObservationId::random())
            .len(),
        1
    );

    assert!(
        tracker
            .handle_wait_invoke_at(
                &owner,
                "wait-high".into(),
                wait_tool_name(),
                &wait_args_input(10),
                now,
                observation(),
            )
            .reply
            .is_none()
    );
    assert_eq!(
        tracker.next_input_wait_deadline(),
        Some(now + Duration::from_secs(9 * 60))
    );

    let background = start_reply(tracker.handle_wait_invoke_at(
        &owner,
        "wait-background".into(),
        wait_tool_name(),
        &wait_args_empty(),
        now,
        observation(),
    ));
    assert_eq!(reply_error(background).0, NO_BACKGROUND_WAIT_CANDIDATES);
}

/// Activating input completes only the addressed agent's input waiter, leaves
/// another agent's waiter untouched, and never copies input content into the
/// bounded input-wait result.
///
/// See `SPEC-tau-harness-activating-input-wait`.
#[test]
fn activating_input_wakes_only_target_owned_waits() {
    let owner = conv("owner");
    let other = conv("other");
    let mut tracker = WaitTracker::default();
    assert!(
        start_wait_input(&mut tracker, &owner, "input-owner")
            .reply
            .is_none()
    );
    assert!(
        start_wait_input(&mut tracker, &other, "input-other")
            .reply
            .is_none()
    );

    let replies = tracker.activate_waits_for(&owner, tau_proto::ObservationId::random());
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].wait_call_id.as_str(), "input-owner");
    let (result, display) =
        reply_result_with_display(replies.into_iter().next().expect("input reply"));
    assert_eq!(
        result,
        CborValue::Map(vec![(
            CborValue::Text("input_available".to_owned()),
            CborValue::Bool(true),
        )])
    );
    let display = display.expect("input-available display");
    assert_eq!(display.args, "1m");
    assert_eq!(display.status, ToolUseStatus::Success);

    let other_replies = tracker.activate_waits_for(&other, tau_proto::ObservationId::random());
    assert_eq!(other_replies.len(), 1);
    assert_eq!(other_replies[0].wait_call_id.as_str(), "input-other");
}

/// Exact and bare background waits expose the closed provider-visible
/// interruption contract without echoing a potentially untrusted target ID.
#[test]
fn activating_input_interruption_results_use_typed_headers() {
    let owner = conv("owner");
    let mut exact = WaitTracker::default();
    exact.record_tool_invoke("source".into(), slow_tool_name(), owner.clone());
    let exact_reply = start_wait_exact(&mut exact, &owner, "wait-exact", "source");
    assert!(exact_reply.reply.is_none());
    let exact_reply = exact
        .activate_waits_for(&owner, tau_proto::ObservationId::random())
        .pop()
        .expect("exact wait interruption");
    assert_eq!(
        reply_result_with_display(exact_reply).0,
        CborValue::Text(
            "tau_internal: true\nwait_outcome: interrupted\nwait_reason: activating_input\nwait_mode: exact\n\nNew input is queued; retry the wait to consume its target result."
                .to_owned()
        )
    );

    let mut any = WaitTracker::default();
    any.record_tool_invoke("source".into(), slow_tool_name(), owner.clone());
    any.calls
        .insert("source".into(), WaitCallState::Backgrounded);
    assert!(start_wait_any(&mut any, &owner, "wait-any").reply.is_none());
    let any_reply = any
        .activate_waits_for(&owner, tau_proto::ObservationId::random())
        .pop()
        .expect("bare wait interruption");
    assert_eq!(
        reply_result_with_display(any_reply).0,
        CborValue::Text(
            "tau_internal: true\nwait_outcome: interrupted\nwait_reason: activating_input\nwait_mode: any_background\n\nNew input is queued; retry the wait to consume its target result."
                .to_owned()
        )
    );
}

/// There can be only one pending input wait per runtime agent, preventing a
/// second tool call from replacing or aliasing the first waiter.
#[test]
fn duplicate_input_wait_is_rejected_without_replacing_first() {
    let owner = conv("owner");
    let mut tracker = WaitTracker::default();
    assert!(
        start_wait_input(&mut tracker, &owner, "input-first")
            .reply
            .is_none()
    );
    let duplicate = start_reply(start_wait_input(&mut tracker, &owner, "input-second"));
    let (message, _, display) = reply_error_with_display(duplicate);
    assert_eq!(
        message,
        "existing input wait for this agent already in progress"
    );
    let display = display.expect("duplicate wait display");
    assert_eq!(display.args, "1m");
    assert_eq!(display.status, ToolUseStatus::Error);
    let replies = tracker.activate_waits_for(&owner, tau_proto::ObservationId::random());
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].wait_call_id.as_str(), "input-first");
}

/// Cancellation removes runtime waiter state, so input accepted later cannot
/// produce a second terminal result for the canceled wait call.
#[test]
fn cancelled_input_wait_cannot_be_woken_later() {
    let owner = conv("owner");
    let mut tracker = WaitTracker::default();
    assert!(
        start_wait_input(&mut tracker, &owner, "input-cancel")
            .reply
            .is_none()
    );
    let cancelled =
        tracker.record_tool_cancelled(&HashSet::from([ToolCallId::from("input-cancel")]), None);
    assert!(cancelled.replies.is_empty());
    assert!(
        tracker
            .activate_waits_for(&owner, tau_proto::ObservationId::random())
            .is_empty()
    );
    assert!(
        tracker
            .expire_input_waits(Instant::now() + Duration::from_secs(120))
            .is_empty()
    );
}

/// Unloading an endpoint drops its runtime-only waiter so a later endpoint
/// reusing the same runtime id cannot inherit or complete stale tool state.
#[test]
fn discarded_input_wait_is_not_inherited_by_reused_agent_id() {
    let owner = conv("owner");
    let mut tracker = WaitTracker::default();
    assert!(
        start_wait_input(&mut tracker, &owner, "input-old")
            .reply
            .is_none()
    );
    tracker.discard_input_wait_for(&owner);
    assert!(
        tracker
            .activate_waits_for(&owner, tau_proto::ObservationId::random())
            .is_empty()
    );
    assert!(
        tracker
            .expire_input_waits(Instant::now() + Duration::from_secs(120))
            .is_empty()
    );
    assert!(
        start_wait_input(&mut tracker, &owner, "input-new")
            .reply
            .is_none()
    );
    let replies = tracker.activate_waits_for(&owner, tau_proto::ObservationId::random());
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].wait_call_id.as_str(), "input-new");
}

/// Monotonic input deadlines expose the earliest waiter, expire all due
/// waiters exactly once, isolate later targets, and use warning UI metadata.
#[test]
fn input_wait_deadlines_are_ordered_and_expire_exactly_once() {
    let now = Instant::now();
    let first = conv("first");
    let first_peer = conv("first-peer");
    let second = conv("second");
    let mut tracker = WaitTracker::with_input_wait_timeout_bounds(
        WaitTimeoutBounds::new(
            WaitTimeoutMinutes::new(1).expect("positive minimum"),
            WaitTimeoutMinutes::new(1_440).expect("positive maximum"),
        )
        .expect("ordered bounds"),
    );
    assert!(
        tracker
            .handle_wait_invoke_at(
                &first,
                "wait-first".into(),
                wait_tool_name(),
                &wait_args_input(1),
                now,
                observation(),
            )
            .reply
            .is_none()
    );
    assert!(
        tracker
            .handle_wait_invoke_at(
                &first_peer,
                "wait-first-peer".into(),
                wait_tool_name(),
                &wait_args_input(1),
                now,
                observation(),
            )
            .reply
            .is_none()
    );
    assert!(
        tracker
            .handle_wait_invoke_at(
                &second,
                "wait-second".into(),
                wait_tool_name(),
                &wait_args_input(2),
                now,
                observation(),
            )
            .reply
            .is_none()
    );
    assert_eq!(
        tracker.next_input_wait_deadline(),
        Some(now + Duration::from_secs(60))
    );
    assert!(
        tracker
            .expire_input_waits(now + Duration::from_secs(59))
            .is_empty()
    );

    let replies = tracker.expire_input_waits(now + Duration::from_secs(60));
    assert_eq!(replies.len(), 2);
    let reply = replies
        .into_iter()
        .find(|reply| reply.wait_call_id.as_str() == "wait-first")
        .expect("first due timeout");
    let (result, display) = reply_result_with_display(reply);
    assert_eq!(
        result,
        CborValue::Map(vec![(
            CborValue::Text("timed_out".to_owned()),
            CborValue::Bool(true),
        )])
    );
    let display = display.expect("timeout display");
    assert_eq!(display.args, "1m");
    assert_eq!(display.status, ToolUseStatus::Warning);
    assert_eq!(display.status_text, "timeout");
    assert!(
        tracker
            .expire_input_waits(now + Duration::from_secs(60))
            .is_empty()
    );
    assert_eq!(
        tracker.next_input_wait_deadline(),
        Some(now + Duration::from_secs(120))
    );
    assert_eq!(
        tracker
            .activate_waits_for(&second, tau_proto::ObservationId::random())
            .len(),
        1
    );
    assert_eq!(tracker.next_input_wait_deadline(), None);
}

/// A request above the input-wait ceiling installs a 1,440-minute monotonic
/// deadline and remains wakeable before that deadline without a real long
/// sleep.
#[test]
fn capped_input_wait_uses_24_hour_deadline_and_wakes_immediately() {
    let now = Instant::now();
    let owner = conv("owner");
    let mut tracker = WaitTracker::default();
    let start = tracker.handle_wait_invoke_at(
        &owner,
        "wait-capped".into(),
        wait_tool_name(),
        &wait_args_input(1_441),
        now,
        observation(),
    );

    assert!(start.reply.is_none());
    assert_eq!(tracker.input_waiters[&owner].request.display_args, "1440m");
    let effective_timeout = Duration::from_secs(1_440 * 60);
    assert_eq!(
        tracker.next_input_wait_deadline(),
        Some(now + effective_timeout)
    );
    assert!(
        tracker
            .expire_input_waits(now + effective_timeout - Duration::from_secs(1))
            .is_empty()
    );

    let replies = tracker.activate_waits_for(&owner, tau_proto::ObservationId::random());
    assert_eq!(replies.len(), 1);
    let (result, display) =
        reply_result_with_display(replies.into_iter().next().expect("input reply"));
    assert_eq!(
        result,
        CborValue::Map(vec![(
            CborValue::Text("input_available".to_owned()),
            CborValue::Bool(true),
        )])
    );
    assert_eq!(display.expect("input display").args, "1440m");
    assert_eq!(tracker.next_input_wait_deadline(), None);
}

/// Event-loop serialization makes timeout-before-input terminal exactly once.
#[test]
fn expired_input_wait_cannot_be_activated_later() {
    let now = Instant::now();
    let owner = conv("owner");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .handle_wait_invoke_at(
                &owner,
                "wait".into(),
                wait_tool_name(),
                &wait_args_input(1),
                now,
                observation(),
            )
            .reply
            .is_none()
    );
    assert_eq!(
        tracker
            .expire_input_waits(now + Duration::from_secs(60))
            .len(),
        1
    );
    assert!(
        tracker
            .activate_waits_for(&owner, tau_proto::ObservationId::random())
            .is_empty()
    );
}

/// When timeout wins before a background completion, that later completion
/// remains available to the ordinary bare background collector.
#[test]
fn input_timeout_does_not_consume_later_background_completion() {
    let now = Instant::now();
    let owner = conv("owner");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .handle_wait_invoke_at(
                &owner,
                "input-wait".into(),
                wait_tool_name(),
                &wait_args_input(1),
                now,
                observation(),
            )
            .reply
            .is_none()
    );
    assert_eq!(
        tracker
            .expire_input_waits(now + Duration::from_secs(60))
            .len(),
        1
    );
    assert!(
        tracker
            .record_background_result(
                background_result("later-bg", "done"),
                owner.clone(),
                observation()
            )
            .is_empty()
    );
    let collected = start_wait_any(&mut tracker, &owner, "collect-later");
    assert_eq!(
        cbor_map_text(
            &reply_result(start_reply(collected)),
            ORIGINAL_TOOL_CALL_ID_HEADER,
        ),
        Some("later-bg")
    );
}

/// Completed any-waits must use deterministic finish order, not HashMap
/// iteration order. The call that finishes first is consumed first even if
/// its id sorts after a later completion.
#[test]
fn no_arg_wait_consumes_oldest_completed_background_result_for_owner() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .record_background_result(
                background_result("bg-b", "first finished"),
                owner.clone(),
                observation()
            )
            .is_empty()
    );
    assert!(
        tracker
            .record_background_result(
                background_result("bg-a", "second finished"),
                owner.clone(),
                observation()
            )
            .is_empty()
    );

    let first = start_wait_any(&mut tracker, &owner, "wait-first");
    assert_eq!(
        first.suppress_call_id.as_ref().map(|id| id.as_str()),
        Some("bg-b")
    );
    let first_result = reply_result(start_reply(first));
    assert_eq!(
        cbor_map_text(&first_result, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-b")
    );
    assert_eq!(
        cbor_map_text(&first_result, "output"),
        Some("first finished")
    );

    let second = start_wait_any(&mut tracker, &owner, "wait-second");
    assert_eq!(
        second.suppress_call_id.as_ref().map(|id| id.as_str()),
        Some("bg-a")
    );
    let second_result = reply_result(start_reply(second));
    assert_eq!(
        cbor_map_text(&second_result, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-a")
    );
    assert_eq!(
        cbor_map_text(&second_result, "output"),
        Some("second finished")
    );
}

/// Owner-local completion FIFOs must match a simple reference model across a
/// 4,096-call workload while lazy tombstones retire each queue node at most
/// once.
#[test]
fn large_owner_local_completion_queues_match_reference_with_linear_retirement() {
    const OWNERS: usize = 4;
    const CALLS: usize = 4_096;
    let owners: Vec<_> = (0..OWNERS)
        .map(|index| conv(&format!("owner-{index}")))
        .collect();
    let mut expected = vec![VecDeque::<ToolCallId>::new(); OWNERS];
    let mut tracker = WaitTracker::default();

    for index in 0..CALLS {
        let owner_index = index % OWNERS;
        let owner = owners[owner_index].clone();
        let call_id: ToolCallId = format!("queued-{index:04}").into();
        tracker.record_tool_invoke(call_id.clone(), slow_tool_name(), owner.clone());
        assert!(
            tracker
                .record_tool_result(
                    &background_placeholder(call_id.as_str()),
                    owner.clone(),
                    observation(),
                )
                .is_empty()
        );
        assert!(
            tracker
                .record_background_result(
                    background_result(call_id.as_str(), call_id.as_str()),
                    owner,
                    observation(),
                )
                .is_empty()
        );
        expected[owner_index].push_back(call_id);
    }

    for index in (0..CALLS).step_by(7) {
        let call_id: ToolCallId = format!("queued-{index:04}").into();
        tracker.consume_completed_call(&call_id);
        expected[index % OWNERS].retain(|candidate| candidate != &call_id);
    }
    for index in (3..CALLS).step_by(11) {
        let call_id: ToolCallId = format!("queued-{index:04}").into();
        if expected[index % OWNERS]
            .iter()
            .any(|candidate| candidate == &call_id)
        {
            let reply = start_reply(start_wait_exact(
                &mut tracker,
                &owners[index % OWNERS],
                &format!("exact-{index:04}"),
                call_id.as_str(),
            ));
            assert_eq!(reply_result(reply), CborValue::Text(call_id.to_string()));
            expected[index % OWNERS].retain(|candidate| candidate != &call_id);
        }
    }

    let torn_down = OWNERS - 1;
    tracker.discard_owner(&owners[torn_down]);
    expected[torn_down].clear();
    for owner_index in 0..OWNERS {
        while let Some(expected_call_id) = expected[owner_index].pop_front() {
            let reply = start_reply(start_wait_any(
                &mut tracker,
                &owners[owner_index],
                &format!("bare-{owner_index}-{}", expected_call_id.as_str()),
            ));
            let result = reply_result(reply);
            assert_eq!(
                cbor_map_text(&result, ORIGINAL_TOOL_CALL_ID_HEADER),
                Some(expected_call_id.as_str())
            );
        }
        assert!(
            tracker
                .oldest_completed_for_owner(&owners[owner_index])
                .is_none()
        );
    }

    assert!(tracker.completed_membership.is_empty());
    assert!(tracker.completion_order_by_owner.is_empty());
    assert_eq!(tracker.completion_nodes_retired(), CALLS);
}

/// Reusing a completed call ID must not revive its stale FIFO position ahead of
/// completions that occurred between the old and new generations.
#[test]
fn reused_call_id_keeps_new_completion_at_its_new_fifo_position() {
    let owner = conv("reuse-owner");
    let mut tracker = WaitTracker::default();

    for call_id in ["x", "y"] {
        tracker.record_tool_invoke(call_id.into(), slow_tool_name(), owner.clone());
        assert!(
            tracker
                .record_tool_result(
                    &background_placeholder(call_id),
                    owner.clone(),
                    observation()
                )
                .is_empty()
        );
        assert!(
            tracker
                .record_background_result(
                    background_result(call_id, &format!("old-{call_id}")),
                    owner.clone(),
                    observation(),
                )
                .is_empty()
        );
        if call_id == "x" {
            tracker.consume_completed_call(&ToolCallId::from("x"));
        }
    }

    tracker.record_tool_invoke("x".into(), slow_tool_name(), owner.clone());
    tracker.record_tool_result(&background_placeholder("x"), owner.clone(), observation());
    tracker.record_background_result(
        background_result("x", "new-x"),
        owner.clone(),
        observation(),
    );

    for (wait_id, expected) in [("wait-y", "y"), ("wait-x", "x")] {
        let result = reply_result(start_reply(start_wait_any(&mut tracker, &owner, wait_id)));
        assert_eq!(
            cbor_map_text(&result, ORIGINAL_TOOL_CALL_ID_HEADER),
            Some(expected)
        );
    }
    assert!(tracker.oldest_completed_for_owner(&owner).is_none());
    assert_eq!(tracker.completion_nodes_retired(), 3);
}

/// If a same-conversation background call is still running, `wait({})`
/// must block and resolve when the first matching completion arrives.
#[test]
fn no_arg_wait_blocks_on_running_background_call_and_resolves() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("bg-run".into(), slow_tool_name(), owner.clone());
    assert!(
        tracker
            .record_tool_result(
                &background_placeholder("bg-run"),
                owner.clone(),
                observation()
            )
            .is_empty()
    );

    let start = start_wait_any(&mut tracker, &owner, "wait-any");
    assert!(start.reply.is_none());
    assert!(start.suppress_call_id.is_none());

    let replies =
        tracker.record_background_result(background_result("bg-run", "done"), owner, observation());
    assert_eq!(replies.len(), 1);
    assert_eq!(
        replies[0].suppress_call_id.as_ref().map(|id| id.as_str()),
        Some("bg-run")
    );
    let result = reply_result(replies.into_iter().next().expect("reply"));
    assert_eq!(
        cbor_map_text(&result, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-run")
    );
    assert_eq!(cbor_map_text(&result, "output"), Some("done"));
}

/// If a no-arg wait is active while its only background call is canceled, the
/// cancellation is the completion it was waiting for. It must reply immediately
/// instead of parking the waiter behind the newly stored cancellation result.
#[test]
fn no_arg_wait_resolves_when_running_background_call_is_cancelled() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("bg-cancel-any".into(), slow_tool_name(), owner.clone());
    assert!(
        tracker
            .record_tool_result(
                &background_placeholder("bg-cancel-any"),
                owner.clone(),
                observation()
            )
            .is_empty()
    );
    assert!(
        start_wait_any(&mut tracker, &owner, "wait-cancel-any")
            .reply
            .is_none()
    );

    let cancelled =
        tracker.record_tool_cancelled(&HashSet::from([ToolCallId::from("bg-cancel-any")]), None);
    assert_eq!(
        cancelled
            .suppress_call_ids
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>(),
        vec!["bg-cancel-any"]
    );
    assert_eq!(cancelled.replies.len(), 1);
    let (message, details, display) =
        reply_error_with_display(cancelled.replies.into_iter().next().expect("wait reply"));
    assert_eq!(message, "Tool call canceled");
    let details = details.expect("original call id details");
    assert_eq!(
        cbor_map_text(&details, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-cancel-any")
    );
    let display = display.expect("wait cancellation display");
    assert_eq!(display.args, "slow");
    assert_eq!(display.status, ToolUseStatus::Error);
}

/// Background errors are completions too. A no-arg wait must return the
/// error and include the original background id in provider-visible
/// details.
#[test]
fn no_arg_wait_returns_background_error_with_original_id_details() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    let details = CborValue::Map(vec![(
        CborValue::Text("hint".to_owned()),
        CborValue::Text("bad input".to_owned()),
    )]);
    assert!(
        tracker
            .record_background_error(
                background_error("bg-fail", "boom", Some(details)),
                owner.clone(),
                observation(),
                BackgroundErrorOutcome::Error,
            )
            .is_empty()
    );

    let reply = start_reply(start_wait_any(&mut tracker, &owner, "wait-error"));
    let (message, details) = reply_error(reply);
    assert_eq!(message, "boom");
    let details = details.expect("details");
    assert_eq!(
        cbor_map_text(&details, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-fail")
    );
    assert_eq!(cbor_map_text(&details, "hint"), Some("bad input"));
}

/// When an exact waiter and an any-waiter can both see the same completion,
/// the exact waiter gets the result and the any-waiter does not consume a
/// duplicate copy.
#[test]
fn explicit_waiter_wins_over_any_waiter_for_same_completion() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("bg-run".into(), slow_tool_name(), owner.clone());
    assert!(
        tracker
            .record_tool_result(
                &background_placeholder("bg-run"),
                owner.clone(),
                observation()
            )
            .is_empty()
    );
    assert!(
        start_wait_any(&mut tracker, &owner, "wait-any")
            .reply
            .is_none()
    );
    assert!(
        start_wait_exact(&mut tracker, &owner, "wait-exact", "bg-run")
            .reply
            .is_none()
    );

    let replies =
        tracker.record_background_result(background_result("bg-run", "done"), owner, observation());
    assert!(replies.iter().any(|reply| {
        reply.wait_call_id.as_str() == "wait-exact"
            && matches!(reply.kind, WaitReplyKind::Result { .. })
    }));
    assert!(replies.iter().any(|reply| {
        reply.wait_call_id.as_str() == "wait-any"
            && matches!(
                &reply.kind,
                WaitReplyKind::Error { message, .. }
                    if message == NO_BACKGROUND_WAIT_CANDIDATES
            )
    }));
}

/// Cancellation follows the same arbitration as a normal completion: an exact
/// waiter for the canceled call consumes the cancellation result, and a no-arg
/// waiter must not receive a duplicate copy with original-call details.
#[test]
fn explicit_waiter_wins_over_any_waiter_for_same_cancelled_completion() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("bg-cancel-race".into(), slow_tool_name(), owner.clone());
    tracker
        .call_refs
        .insert("bg-cancel-race".into(), call_ref(1, 0));
    tracker
        .call_refs
        .insert("wait-any-cancel".into(), call_ref(2, 0));
    tracker
        .call_refs
        .insert("wait-exact-cancel".into(), call_ref(3, 0));
    assert!(
        tracker
            .record_tool_result(
                &background_placeholder("bg-cancel-race"),
                owner.clone(),
                observation()
            )
            .is_empty()
    );
    assert!(
        start_wait_any(&mut tracker, &owner, "wait-any-cancel")
            .reply
            .is_none()
    );
    assert!(
        start_wait_exact(&mut tracker, &owner, "wait-exact-cancel", "bg-cancel-race")
            .reply
            .is_none()
    );

    let terminal = tau_proto::ObservationId::from_bytes([46; 16]);
    let cancelled = tracker.record_tool_cancelled(
        &HashSet::from([ToolCallId::from("bg-cancel-race")]),
        Some((&ToolCallId::from("bg-cancel-race"), terminal)),
    );
    assert_eq!(cancelled.replies.len(), 2);
    assert!(cancelled.replies.iter().any(|reply| {
        reply.wait_call_id.as_str() == "wait-exact-cancel"
            && matches!(
                &reply.kind,
                WaitReplyKind::Error { message, details, .. }
                    if message == "Tool call `bg-cancel-race` was cancelled"
                        && details.is_none()
            )
    }));
    assert!(cancelled.replies.iter().any(|reply| matches!(
        &reply.settlement,
        Some(PendingWaitSettlement {
            wait_call,
            outcome: tau_proto::ToolWaitOutcome::CompletionDelivered {
                source_call,
                source_terminal,
                source_phase: tau_proto::ToolSourcePhase::Background,
                envelope: tau_proto::ToolOutputEnvelope::Identity,
            },
            ..
        }) if *wait_call == call_ref(3, 0)
            && *source_call == call_ref(1, 0)
            && *source_terminal == terminal
    )));
    assert!(cancelled.replies.iter().any(|reply| {
        reply.wait_call_id.as_str() == "wait-any-cancel"
            && matches!(
                &reply.kind,
                WaitReplyKind::Error { message, details, .. }
                    if message == "background tool call in this conversation was cancelled"
                        && details.is_none()
            )
    }));
    assert!(cancelled.replies.iter().any(|reply| matches!(
        &reply.settlement,
        Some(PendingWaitSettlement {
            wait_call,
            registration: Some(_),
            outcome: tau_proto::ToolWaitOutcome::Rejected {
                reason: tau_proto::WaitRejectionReason::NoBackgroundCandidate,
            },
            ..
        }) if *wait_call == call_ref(2, 0)
    )));
    assert!(cancelled.suppress_call_ids.is_empty());
}

/// A bare waiter that wins a background cancellation links the canonical
/// terminal through the original-call-id envelope.
#[test]
fn any_wait_cancellation_settlement_keeps_source_terminal() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("source".into(), slow_tool_name(), owner.clone());
    tracker.call_refs.insert("source".into(), call_ref(1, 0));
    tracker.record_tool_result(&background_placeholder("source"), owner.clone(), None);
    tracker.call_refs.insert("wait".into(), call_ref(2, 0));
    assert!(start_wait_any(&mut tracker, &owner, "wait").reply.is_none());
    let terminal = tau_proto::ObservationId::from_bytes([48; 16]);
    let cancelled = tracker.record_tool_cancelled(
        &HashSet::from([ToolCallId::from("source")]),
        Some((&ToolCallId::from("source"), terminal)),
    );
    assert!(matches!(
        &cancelled.replies[0].settlement,
        Some(PendingWaitSettlement {
            wait_call,
            outcome: tau_proto::ToolWaitOutcome::CompletionDelivered {
                source_call,
                source_terminal,
                source_phase: tau_proto::ToolSourcePhase::Background,
                envelope: tau_proto::ToolOutputEnvelope::OriginalToolCallIdHeader,
            },
            ..
        }) if *wait_call == call_ref(2, 0)
            && *source_call == call_ref(1, 0)
            && *source_terminal == terminal
    ));
}

/// Cancelling a foreground source resolves an installed exact waiter with the
/// canonical cancellation terminal and foreground phase.
#[test]
fn foreground_cancellation_settlement_keeps_source_terminal() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("source".into(), slow_tool_name(), owner.clone());
    tracker.call_refs.insert("source".into(), call_ref(1, 0));
    tracker.call_refs.insert("wait".into(), call_ref(2, 0));
    assert!(
        start_wait_exact(&mut tracker, &owner, "wait", "source")
            .reply
            .is_none()
    );
    let terminal = tau_proto::ObservationId::from_bytes([47; 16]);
    let cancelled = tracker.record_tool_cancelled(
        &HashSet::from([ToolCallId::from("source")]),
        Some((&ToolCallId::from("source"), terminal)),
    );
    assert!(matches!(
        &cancelled.replies[0].settlement,
        Some(PendingWaitSettlement {
            wait_call,
            outcome: tau_proto::ToolWaitOutcome::CompletionDelivered {
                source_call,
                source_terminal,
                source_phase: tau_proto::ToolSourcePhase::Foreground,
                envelope: tau_proto::ToolOutputEnvelope::Identity,
            },
            ..
        }) if *wait_call == call_ref(2, 0)
            && *source_call == call_ref(1, 0)
            && *source_terminal == terminal
    ));
}

/// Parallel duplicate no-arg waits in one conversation would be ambiguous:
/// only one waiter may consume the next completion.
#[test]
fn duplicate_no_arg_waits_in_same_conversation_error() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("bg-run".into(), slow_tool_name(), owner.clone());
    assert!(
        tracker
            .record_tool_result(
                &background_placeholder("bg-run"),
                owner.clone(),
                observation()
            )
            .is_empty()
    );
    assert!(
        start_wait_any(&mut tracker, &owner, "wait-one")
            .reply
            .is_none()
    );

    let (message, details) = reply_error(start_reply(start_wait_any(
        &mut tracker,
        &owner,
        "wait-two",
    )));
    assert!(message.contains("existing wait for a background tool call"));
    assert!(details.is_none());
}

/// The no-arg form is scoped to its caller's conversation. A completion in
/// a different conversation must not be stolen by this wait.
#[test]
fn no_arg_wait_ignores_background_completions_from_other_conversations() {
    let main = conv("main");
    let side = conv("side");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .record_background_result(
                background_result("side-bg", "side done"),
                side.clone(),
                observation()
            )
            .is_empty()
    );

    let (message, _) = reply_error(start_reply(start_wait_any(
        &mut tracker,
        &main,
        "wait-main",
    )));
    assert!(message.contains("no background tool calls"));

    let side_result = reply_result(start_reply(start_wait_any(
        &mut tracker,
        &side,
        "wait-side",
    )));
    assert_eq!(
        cbor_map_text(&side_result, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("side-bg")
    );
}

/// Exact-id waits are scoped to the calling conversation just like no-arg
/// waits. A caller that guesses or observes another conversation's completed
/// background id must not consume it or learn that it exists.
#[test]
fn exact_wait_does_not_consume_completed_background_result_from_other_conversation() {
    let main = conv("main");
    let side = conv("side");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .record_background_result(
                background_result("side-bg", "side done"),
                side.clone(),
                observation()
            )
            .is_empty()
    );

    let (message, details) = reply_error(start_reply(start_wait_exact(
        &mut tracker,
        &main,
        "wait-main",
        "side-bg",
    )));
    assert_eq!(message, "unknown tool call: `side-bg`");
    assert!(details.is_none());

    assert_eq!(
        reply_result(start_reply(start_wait_exact(
            &mut tracker,
            &side,
            "wait-side",
            "side-bg",
        ))),
        CborValue::Text("side done".to_owned())
    );
}

/// Exact-id waits must also reject another conversation's still-running
/// background call. Without this guard, a cross-agent exact wait could park
/// first and consume the later completion.
#[test]
fn exact_wait_does_not_attach_to_running_background_call_from_other_conversation() {
    let main = conv("main");
    let side = conv("side");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("side-bg".into(), slow_tool_name(), side.clone());
    assert!(
        tracker
            .record_tool_result(
                &background_placeholder("side-bg"),
                side.clone(),
                observation()
            )
            .is_empty()
    );

    let (message, details) = reply_error(start_reply(start_wait_exact(
        &mut tracker,
        &main,
        "wait-main",
        "side-bg",
    )));
    assert_eq!(message, "unknown tool call: `side-bg`");
    assert!(details.is_none());

    let start = start_wait_exact(&mut tracker, &side, "wait-side", "side-bg");
    assert!(start.reply.is_none());
    let replies = tracker.record_background_result(
        background_result("side-bg", "side done"),
        side,
        observation(),
    );
    assert_eq!(replies.len(), 1);
    assert_eq!(
        reply_result(replies.into_iter().next().expect("side wait reply")),
        CborValue::Text("side done".to_owned())
    );
}

/// Once a no-arg wait consumes a completion, a later exact wait for that
/// original id must report that the result was already handled.
#[test]
fn exact_wait_after_no_arg_consumes_reports_already_consumed() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .record_background_result(
                background_result("bg-once", "done"),
                owner.clone(),
                observation()
            )
            .is_empty()
    );
    let _ = start_reply(start_wait_any(&mut tracker, &owner, "wait-any"));

    let (message, _) = reply_error(start_reply(start_wait_exact(
        &mut tracker,
        &owner,
        "wait-exact",
        "bg-once",
    )));
    assert!(message.contains("already consumed"));
}

/// Agent teardown must retire completed background payloads instead of making
/// them available to another agent's bare wait.
#[test]
fn retired_background_owner_cannot_be_consumed_by_parent_no_arg_wait() {
    let parent = conv("parent");
    let side = conv("side");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .record_background_result(
                background_result("bg-side", "done"),
                side.clone(),
                observation()
            )
            .is_empty()
    );
    tracker.discard_owner(&side);

    let (message, _) = reply_error(start_reply(start_wait_any(
        &mut tracker,
        &parent,
        "wait-parent",
    )));
    assert!(message.contains("no background tool calls"));
}

/// A canceled background call remains waitable once. It should not be marked
/// consumed before the caller has a chance to retrieve the cancellation result.
#[test]
fn exact_wait_after_background_cancel_returns_cancel_error_once() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("bg-cancel".into(), slow_tool_name(), owner.clone());
    assert!(
        tracker
            .record_tool_result(
                &background_placeholder("bg-cancel"),
                owner.clone(),
                observation()
            )
            .is_empty()
    );

    let cancelled = HashSet::from([ToolCallId::from("bg-cancel")]);
    let wait_cancel = tracker.record_tool_cancelled(&cancelled, None);
    assert!(wait_cancel.replies.is_empty());

    let reply = start_reply(start_wait_exact(
        &mut tracker,
        &owner,
        "wait-cancel",
        "bg-cancel",
    ));
    let (message, details) = reply_error(reply);
    assert_eq!(message, "Tool call canceled");
    assert!(details.is_none());

    let reply = start_reply(start_wait_exact(
        &mut tracker,
        &owner,
        "wait-cancel-again",
        "bg-cancel",
    ));
    let (message, _) = reply_error(reply);
    assert_eq!(message, "result for tool call `bg-cancel` already consumed");
}

/// Repeated-wait advice augments only timeout results and preserves the typed
/// timeout flag that callers already consume.
#[test]
fn timeout_advice_is_additive_and_model_visible() {
    let mut reply = wait_timed_out_reply(
        ToolCallId::from("wait-timeout"),
        wait_tool_name(),
        "5m".to_owned(),
    );
    reply.add_timeout_advice("prefer an event-driven wake");
    let WaitReplyKind::Result {
        result: CborValue::Map(entries),
        ..
    } = reply.kind
    else {
        panic!("timeout reply must be a map result");
    };
    assert!(entries.contains(&(
        CborValue::Text("timed_out".to_owned()),
        CborValue::Bool(true)
    )));
    assert!(entries.contains(&(
        CborValue::Text("advice".to_owned()),
        CborValue::Text("prefer an event-driven wake".to_owned())
    )));
}

/// Plural parsing enforces the approved bounded distinct-array interface
/// without changing singular or timeout selection.
#[test]
fn wait_all_parser_enforces_bounds_duplicates_and_mutual_exclusion() {
    assert_eq!(
        parse_wait_args(&wait_args_all(&["a"])),
        Ok(WaitTarget::ExactAll(vec![ToolCallId::from("a")]))
    );
    let maximum = (0..tau_proto::MAX_WAIT_ALL_MEMBERS)
        .map(|index| format!("call-{index}"))
        .collect::<Vec<_>>();
    let maximum_refs = maximum.iter().map(String::as_str).collect::<Vec<_>>();
    assert!(matches!(
        parse_wait_args(&wait_args_all(&maximum_refs)),
        Ok(WaitTarget::ExactAll(targets))
            if targets.len() == tau_proto::MAX_WAIT_ALL_MEMBERS
    ));
    assert_eq!(
        parse_wait_args(&wait_args_all(&[])),
        Err("`tool_call_ids` must contain at least one entry".to_owned())
    );
    let over_maximum = (0..=tau_proto::MAX_WAIT_ALL_MEMBERS)
        .map(|index| format!("call-{index}"))
        .collect::<Vec<_>>();
    let over_maximum_refs = over_maximum.iter().map(String::as_str).collect::<Vec<_>>();
    assert_eq!(
        parse_wait_args(&wait_args_all(&over_maximum_refs)),
        Err(format!(
            "`tool_call_ids` must contain at most {} entries",
            tau_proto::MAX_WAIT_ALL_MEMBERS
        ))
    );
    assert_eq!(
        parse_wait_args(&wait_args_all(&["a", "a"])),
        Err("`tool_call_ids` must not contain duplicates".to_owned())
    );
    assert_eq!(
        parse_wait_args(&wait_args_all(&[" "])),
        Err("`tool_call_ids` entries must not be empty".to_owned())
    );
    assert_eq!(
        parse_wait_args(&CborValue::Map(vec![(
            CborValue::Text("tool_call_ids".to_owned()),
            CborValue::Array(vec![CborValue::Integer(1.into())]),
        )])),
        Err("every `tool_call_ids` entry must be a string".to_owned())
    );
    assert_eq!(
        parse_wait_args(&CborValue::Map(vec![
            (
                CborValue::Text("tool_call_id".to_owned()),
                CborValue::Text("a".to_owned()),
            ),
            (
                CborValue::Text("tool_call_ids".to_owned()),
                CborValue::Array(vec![CborValue::Text("b".to_owned())]),
            ),
        ])),
        Err(
            "`tool_call_id`, `tool_call_ids`, and `timeout_minutes` are mutually exclusive"
                .to_owned()
        )
    );
    assert!(
        parse_wait_args(&CborValue::Map(vec![
            (
                CborValue::Text("tool_call_ids".to_owned()),
                CborValue::Array(vec![CborValue::Text("a".to_owned())]),
            ),
            (
                CborValue::Text("timeout_minutes".to_owned()),
                CborValue::Integer(7.into()),
            ),
        ]))
        .is_err()
    );
    assert_eq!(
        normalized_wait_timeout_minutes_inner(
            &CborValue::Map(vec![(
                CborValue::Text("timeout_minutes".to_owned()),
                CborValue::Integer(1_000_000.into()),
            )]),
            WaitTimeoutBounds::built_in(),
        ),
        Ok(Some(1440))
    );
}

/// An all-complete plural wait returns arbitrary member payloads in request
/// order and delays every consumption until its wait terminal commits.
#[test]
fn wait_all_complete_results_are_ordered_typed_and_commit_atomically() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    track_call(&mut tracker, &owner, "a", call_ref(1, 0));
    track_call(&mut tracker, &owner, "b", call_ref(2, 0));
    tracker.retain_call_ref("wait-all".into(), call_ref(3, 0));
    let terminal_a = tau_proto::ObservationId::from_bytes([11; 16]);
    let terminal_b = tau_proto::ObservationId::from_bytes([12; 16]);
    let typed_output = CborValue::Map(vec![(
        CborValue::Text("nested".to_owned()),
        CborValue::Array(vec![CborValue::Integer(7.into()), CborValue::Bool(true)]),
    )]);
    tracker.record_background_result(
        ToolBackgroundResult {
            result: typed_output.clone(),
            ..background_result("a", "unused")
        },
        owner.clone(),
        Some(terminal_a),
    );
    tracker.record_background_error(
        background_error(
            "b",
            "failed",
            Some(CborValue::Array(vec![CborValue::Text("detail".to_owned())])),
        ),
        owner.clone(),
        Some(terminal_b),
        BackgroundErrorOutcome::Error,
    );

    let start = start_wait_all(&mut tracker, &owner, "wait-all", &["b", "a"]);
    assert!(
        start.registration.is_none(),
        "all-complete waits are immediate"
    );
    assert_eq!(
        start.suppress_call_ids,
        vec![ToolCallId::from("b"), ToolCallId::from("a")]
    );
    let reply = start_reply(start);
    let settlement = reply.settlement.clone().expect("plural settlement");
    assert!(matches!(
        settlement.outcome,
        tau_proto::ToolWaitOutcome::CompletionsDelivered { ref sources }
            if sources.iter().map(|source| source.source_call).collect::<Vec<_>>()
                == vec![call_ref(2, 0), call_ref(1, 0)]
    ));
    let result = reply_result(reply);
    let expected = CborValue::Map(vec![(
        CborValue::Text("results".to_owned()),
        CborValue::Array(vec![
            CborValue::Map(vec![
                original_tool_call_id_entry(&ToolCallId::from("b")),
                (
                    CborValue::Text("outcome".to_owned()),
                    CborValue::Text("error".to_owned()),
                ),
                (
                    CborValue::Text("message".to_owned()),
                    CborValue::Text("failed".to_owned()),
                ),
                (
                    CborValue::Text("details".to_owned()),
                    CborValue::Array(vec![CborValue::Text("detail".to_owned())]),
                ),
            ]),
            CborValue::Map(vec![
                original_tool_call_id_entry(&ToolCallId::from("a")),
                (
                    CborValue::Text("outcome".to_owned()),
                    CborValue::Text("result".to_owned()),
                ),
                (CborValue::Text("output".to_owned()), typed_output.clone()),
            ]),
        ]),
    )]);
    assert_eq!(result, expected);
    let CborValue::Map(root) = &result else {
        panic!("plural wait result must be a map");
    };
    let CborValue::Array(members) = &root[0].1 else {
        panic!("plural wait results must be an array");
    };
    assert_eq!(
        cbor_map_text(&members[0], "original_tool_call_id"),
        Some("b")
    );
    assert_eq!(cbor_map_text(&members[0], "outcome"), Some("error"));
    assert_eq!(
        cbor_map_text(&members[1], "original_tool_call_id"),
        Some("a")
    );
    assert_eq!(cbor_map_text(&members[1], "outcome"), Some("result"));
    let CborValue::Map(a_member) = &members[1] else {
        panic!("result member must be a map");
    };
    assert_eq!(a_member[2].1, typed_output);
    assert!(tracker.is_completed(&ToolCallId::from("a")));
    assert!(tracker.is_completed(&ToolCallId::from("b")));

    let (consumed, replies) = tracker.commit_exact_all_wait(&ToolCallId::from("wait-all"));
    assert_eq!(consumed, vec![ToolCallId::from("b"), ToolCallId::from("a")]);
    assert!(replies.is_empty());
    assert!(tracker.is_consumed(&ToolCallId::from("a")));
    assert!(tracker.is_consumed(&ToolCallId::from("b")));
}

/// A mixed completed/running set reserves the completed member without
/// consuming it, registers once, and settles only when the running member ends.
#[test]
fn wait_all_mixed_completed_and_running_members_register_once() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    track_call(&mut tracker, &owner, "ready", call_ref(1, 0));
    track_call(&mut tracker, &owner, "running", call_ref(2, 0));
    tracker.record_tool_result(
        &background_placeholder("ready"),
        owner.clone(),
        observation(),
    );
    tracker.record_tool_result(
        &background_placeholder("running"),
        owner.clone(),
        observation(),
    );
    tracker.record_background_result(
        background_result("ready", "ready payload"),
        owner.clone(),
        observation(),
    );
    tracker.retain_call_ref("wait-all".into(), call_ref(3, 0));

    let start = start_wait_all(&mut tracker, &owner, "wait-all", &["running", "ready"]);
    assert!(start.reply.is_none());
    assert!(start.registration.is_some());
    assert!(tracker.is_completed(&ToolCallId::from("ready")));
    let replies = tracker.record_background_result(
        background_result("running", "running payload"),
        owner,
        observation(),
    );
    assert_eq!(replies.len(), 1);
    let CborValue::Map(root) = reply_result(replies.into_iter().next().expect("plural reply"))
    else {
        panic!("plural result must be a map");
    };
    let CborValue::Array(members) = &root[0].1 else {
        panic!("plural members must be an array");
    };
    assert_eq!(
        members
            .iter()
            .map(|member| cbor_map_text(member, ORIGINAL_TOOL_CALL_ID_HEADER))
            .collect::<Vec<_>>(),
        vec![Some("running"), Some("ready")]
    );
}

/// A running plural wait settles only at the final member, regardless of
/// completion order, and never lets a bare waiter steal a reserved member.
#[test]
fn wait_all_running_members_settle_on_final_completion_and_outrank_bare_wait() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    for (index, call_id) in ["a", "b", "c"].into_iter().enumerate() {
        track_call(
            &mut tracker,
            &owner,
            call_id,
            call_ref(u8::try_from(index + 1).expect("small index"), 0),
        );
        tracker.record_tool_result(
            &background_placeholder(call_id),
            owner.clone(),
            observation(),
        );
    }
    tracker.retain_call_ref("wait-all".into(), call_ref(9, 0));
    tracker.retain_call_ref("wait-any".into(), call_ref(10, 0));
    let start = start_wait_all(&mut tracker, &owner, "wait-all", &["a", "b"]);
    assert!(start.reply.is_none());
    assert!(start.registration.is_some());
    assert_eq!(
        start.suppress_call_ids,
        vec![ToolCallId::from("a"), ToolCallId::from("b")]
    );
    assert!(
        start_wait_any(&mut tracker, &owner, "wait-any")
            .reply
            .is_none()
    );

    assert!(
        tracker
            .record_background_result(
                background_result("b", "second request member first"),
                owner.clone(),
                observation(),
            )
            .is_empty()
    );
    let bare = tracker.record_background_result(
        background_result("c", "unreserved"),
        owner.clone(),
        observation(),
    );
    assert_eq!(bare.len(), 1);
    assert_eq!(
        cbor_map_text(
            &reply_result(bare.into_iter().next().expect("bare reply")),
            ORIGINAL_TOOL_CALL_ID_HEADER,
        ),
        Some("c")
    );
    let replies = tracker.record_background_result(
        background_result("a", "first request member last"),
        owner,
        observation(),
    );
    assert_eq!(replies.len(), 1);
    let result = reply_result(replies.into_iter().next().expect("plural reply"));
    let CborValue::Map(root) = result else {
        panic!("plural result must be a map");
    };
    let CborValue::Array(members) = &root[0].1 else {
        panic!("plural results must be an array");
    };
    assert_eq!(
        cbor_map_text(&members[0], "original_tool_call_id"),
        Some("a")
    );
    assert_eq!(
        cbor_map_text(&members[1], "original_tool_call_id"),
        Some("b")
    );
}

/// Interruption restores only notices that reservation actually prevented from
/// reaching the provider; already-delivered and never-produced notices are only
/// unsuppressed.
#[test]
fn wait_all_interruption_restores_only_undelivered_notices() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    for (index, call_id) in ["a", "b", "c"].into_iter().enumerate() {
        track_call(
            &mut tracker,
            &owner,
            call_id,
            call_ref(u8::try_from(index + 1).expect("small index"), 0),
        );
        tracker.record_tool_result(
            &background_placeholder(call_id),
            owner.clone(),
            observation(),
        );
    }
    tracker.record_background_result(
        background_result("a", "already delivered"),
        owner.clone(),
        observation(),
    );
    tracker.retain_call_ref("wait-all".into(), call_ref(9, 0));
    let start = start_wait_all(&mut tracker, &owner, "wait-all", &["a", "b", "c"]);
    assert!(start.reply.is_none());
    tracker.record_exact_all_notice_suppressed(
        &ToolCallId::from("wait-all"),
        &ToolCallId::from("a"),
        false,
    );
    tracker.record_background_result(
        background_result("b", "suppressed while reserved"),
        owner.clone(),
        observation(),
    );
    tracker.record_exact_all_notice_blocked(&ToolCallId::from("b"));

    let reply = tracker
        .activate_waits_for(&owner, observation().expect("observation"))
        .pop()
        .expect("plural interruption");
    assert_eq!(
        reply
            .unsuppress_notices
            .iter()
            .map(|notice| notice.call_id.clone())
            .collect::<Vec<_>>(),
        vec![ToolCallId::from("b")]
    );
    assert_eq!(
        reply.release_suppression_call_ids,
        vec![ToolCallId::from("a"), ToolCallId::from("c")]
    );
}

/// A background completion whose caller selected DoNotQueue does not acquire a
/// synthetic notice-restoration obligation merely because it was reserved.
#[test]
fn wait_all_do_not_queue_completion_restores_no_notice() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    for (index, call_id) in ["a", "b"].into_iter().enumerate() {
        track_call(
            &mut tracker,
            &owner,
            call_id,
            call_ref(u8::try_from(index + 1).expect("small index"), 0),
        );
        tracker.record_tool_result(
            &background_placeholder(call_id),
            owner.clone(),
            observation(),
        );
    }
    tracker.retain_call_ref("wait-all".into(), call_ref(3, 0));
    assert!(
        start_wait_all(&mut tracker, &owner, "wait-all", &["a", "b"])
            .reply
            .is_none()
    );
    tracker.record_background_result(
        background_result("a", "do not queue"),
        owner.clone(),
        observation(),
    );
    let reply = tracker
        .activate_waits_for(&owner, observation().expect("observation"))
        .pop()
        .expect("plural interruption");
    assert!(reply.unsuppress_call_ids.is_empty());
    assert_eq!(
        reply.release_suppression_call_ids,
        vec![ToolCallId::from("a"), ToolCallId::from("b")]
    );
}

/// When a plural wait consumes the last background candidates, a competing
/// bare waiter receives the ordinary no-candidate terminal instead of remaining
/// installed forever.
#[test]
fn wait_all_commit_finishes_stranded_bare_waiter() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    track_call(&mut tracker, &owner, "a", call_ref(1, 0));
    tracker.record_tool_result(&background_placeholder("a"), owner.clone(), observation());
    tracker.retain_call_ref("wait-all".into(), call_ref(2, 0));
    tracker.retain_call_ref("wait-any".into(), call_ref(3, 0));
    assert!(
        start_wait_all(&mut tracker, &owner, "wait-all", &["a"])
            .reply
            .is_none()
    );
    assert!(
        start_wait_any(&mut tracker, &owner, "wait-any")
            .reply
            .is_none()
    );
    assert_eq!(
        tracker
            .record_background_result(background_result("a", "plural wins"), owner, observation(),)
            .len(),
        1
    );

    let (consumed, replies) = tracker.commit_exact_all_wait(&ToolCallId::from("wait-all"));
    assert_eq!(consumed, vec![ToolCallId::from("a")]);
    assert_eq!(replies.len(), 1);
    assert!(matches!(replies[0].kind, WaitReplyKind::Error { .. }));
}

/// Failed plural preflight leaves every valid member and completion FIFO
/// untouched, then activation interruption releases all successful
/// reservations.
#[test]
fn wait_all_preflight_and_interruption_are_atomic() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    track_call(&mut tracker, &owner, "a", call_ref(1, 0));
    track_call(&mut tracker, &owner, "b", call_ref(2, 0));
    tracker.retain_call_ref("invalid-wait".into(), call_ref(3, 0));
    tracker.retain_call_ref("active-wait".into(), call_ref(4, 0));
    tracker.record_background_result(
        background_result("a", "ready"),
        owner.clone(),
        observation(),
    );
    tracker.record_terminal_state("b".into(), WaitCallState::Consumed);

    assert!(
        !tracker
            .exact_all_preflight_succeeds(&[ToolCallId::from("a"), ToolCallId::from("b")], &owner,),
        "queued activating input must not mask an invalid plural preflight",
    );
    let invalid = start_wait_all(&mut tracker, &owner, "invalid-wait", &["a", "b"]);
    assert!(invalid.registration.is_none());
    assert!(invalid.suppress_call_id.is_none());
    assert!(invalid.suppress_call_ids.is_empty());
    assert!(matches!(
        start_reply(invalid).kind,
        WaitReplyKind::Error { .. }
    ));
    assert!(tracker.exact_all_reservations.is_empty());
    assert_eq!(
        tracker.oldest_completed_for_owner(&owner),
        Some(ToolCallId::from("a"))
    );

    track_call(&mut tracker, &owner, "b", call_ref(5, 0));
    tracker.record_tool_result(&background_placeholder("b"), owner.clone(), observation());
    assert!(
        tracker
            .exact_all_preflight_succeeds(&[ToolCallId::from("a"), ToolCallId::from("b")], &owner,)
    );
    let active = start_wait_all(&mut tracker, &owner, "active-wait", &["a", "b"]);
    assert!(active.reply.is_none());
    tracker.record_exact_all_notice_suppressed(
        &ToolCallId::from("active-wait"),
        &ToolCallId::from("a"),
        true,
    );
    let activation = tau_proto::ObservationId::from_bytes([33; 16]);
    let replies = tracker.activate_waits_for(&owner, activation);
    assert_eq!(replies.len(), 1);
    assert_eq!(
        reply_result(replies[0].clone()),
        CborValue::Text(
            "tau_internal: true\nwait_outcome: interrupted\nwait_reason: activating_input\nwait_mode: exact_all\n\nNew input is queued; retry the wait to consume its target result."
                .to_owned()
        )
    );
    assert_eq!(
        replies[0]
            .unsuppress_notices
            .iter()
            .map(|notice| notice.call_id.clone())
            .collect::<Vec<_>>(),
        vec![ToolCallId::from("a")]
    );
    assert_eq!(
        replies[0].release_suppression_call_ids,
        vec![ToolCallId::from("b")]
    );
    assert!(tracker.exact_all_reservations.is_empty());
    assert_eq!(
        tracker.oldest_completed_for_owner(&owner),
        Some(ToolCallId::from("a"))
    );
}

/// Every state-level plural preflight rejection leaves a valid completed member
/// in its original FIFO and installs no partial reservation.
#[test]
fn wait_all_preflight_rejects_invalid_state_matrix_without_mutation() {
    fn base() -> (WaitTracker, AgentId) {
        let owner = conv("main");
        let mut tracker = WaitTracker::default();
        track_call(&mut tracker, &owner, "a", call_ref(1, 0));
        tracker.record_tool_result(&background_placeholder("a"), owner.clone(), observation());
        tracker.record_background_result(
            background_result("a", "ready"),
            owner.clone(),
            observation(),
        );
        tracker.retain_call_ref("wait-all".into(), call_ref(9, 0));
        (tracker, owner)
    }

    fn assert_atomic_rejection(mut tracker: WaitTracker, owner: &AgentId, target: &str) {
        let start = start_wait_all(&mut tracker, owner, "wait-all", &["a", target]);
        assert!(start.registration.is_none());
        assert!(start.suppress_call_id.is_none());
        assert!(start.suppress_call_ids.is_empty());
        assert!(matches!(
            start_reply(start).kind,
            WaitReplyKind::Error { .. }
        ));
        assert!(tracker.exact_all_reservations.is_empty());
        assert_eq!(
            tracker.oldest_completed_for_owner(owner),
            Some(ToolCallId::from("a"))
        );
    }

    let (tracker, owner) = base();
    assert_atomic_rejection(tracker, &owner, "unknown");

    let (mut tracker, owner) = base();
    track_call(&mut tracker, &conv("other"), "b", call_ref(2, 0));
    assert_atomic_rejection(tracker, &owner, "b");

    let (mut tracker, owner) = base();
    track_call(&mut tracker, &owner, "b", call_ref(2, 0));
    tracker.record_terminal_state("b".into(), WaitCallState::Consumed);
    assert_atomic_rejection(tracker, &owner, "b");

    let (mut tracker, owner) = base();
    track_call(&mut tracker, &owner, "b", call_ref(2, 0));
    tracker.record_terminal_state("b".into(), WaitCallState::NormalReturned);
    assert_atomic_rejection(tracker, &owner, "b");

    let (mut tracker, owner) = base();
    track_call(&mut tracker, &owner, "b", call_ref(2, 0));
    tracker.retain_call_ref("wait-b".into(), call_ref(3, 0));
    assert!(
        start_wait_exact(&mut tracker, &owner, "wait-b", "b")
            .reply
            .is_none()
    );
    assert_atomic_rejection(tracker, &owner, "b");

    let (mut tracker, owner) = base();
    track_call(&mut tracker, &owner, "b", call_ref(2, 0));
    track_call(&mut tracker, &owner, "c", call_ref(3, 0));
    tracker.retain_call_ref("first-set".into(), call_ref(4, 0));
    assert!(
        start_wait_all(&mut tracker, &owner, "first-set", &["b", "c"])
            .reply
            .is_none()
    );
    let rejected = start_wait_all(&mut tracker, &owner, "wait-all", &["a", "b"]);
    assert!(rejected.registration.is_none());
    assert!(rejected.suppress_call_id.is_none());
    assert!(rejected.suppress_call_ids.is_empty());
    assert!(matches!(
        start_reply(rejected).kind,
        WaitReplyKind::Error { .. }
    ));
    assert_eq!(tracker.exact_all_reservations.len(), 2);
    assert!(!tracker.exact_all_reservations.contains_key(&call_ref(1, 0)));
    assert_eq!(
        tracker.oldest_completed_for_owner(&owner),
        Some(ToolCallId::from("a"))
    );
}

/// Reusing a display ID after its reserved generation completes cannot replace
/// that member's payload or cause plural commit to consume the new generation.
#[test]
fn wait_all_binds_members_to_stable_call_generations() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    track_call(&mut tracker, &owner, "reused", call_ref(1, 0));
    track_call(&mut tracker, &owner, "other", call_ref(2, 0));
    tracker.retain_call_ref("wait-all".into(), call_ref(3, 0));
    tracker.record_tool_result(
        &background_placeholder("reused"),
        owner.clone(),
        observation(),
    );
    tracker.record_tool_result(
        &background_placeholder("other"),
        owner.clone(),
        observation(),
    );
    assert!(
        start_wait_all(&mut tracker, &owner, "wait-all", &["reused", "other"])
            .reply
            .is_none()
    );
    assert!(
        tracker
            .record_background_result(
                background_result("reused", "old generation"),
                owner.clone(),
                observation(),
            )
            .is_empty()
    );

    tracker.reset_call_ref("reused".into(), call_ref(9, 0));
    tracker.record_tool_invoke("reused".into(), slow_tool_name(), owner.clone());
    let replies = tracker.record_background_result(
        background_result("other", "other generation"),
        owner,
        observation(),
    );
    let result = reply_result(replies.into_iter().next().expect("plural reply"));
    let CborValue::Map(root) = result else {
        panic!("plural result must be a map");
    };
    let CborValue::Array(members) = &root[0].1 else {
        panic!("plural results must be an array");
    };
    let CborValue::Map(reused_member) = &members[0] else {
        panic!("plural member must be a map");
    };
    assert_eq!(
        reused_member[2].1,
        CborValue::Text("old generation".to_owned())
    );

    let (consumed, replies) = tracker.commit_exact_all_wait(&ToolCallId::from("wait-all"));
    assert_eq!(
        consumed,
        vec![ToolCallId::from("other")],
        "old generation commit may consume matching siblings but not the reused display ID"
    );
    assert!(replies.is_empty());
    assert_eq!(
        tracker.call_ref(&ToolCallId::from("reused")),
        Some(call_ref(9, 0))
    );
    assert_eq!(
        tracker.calls.get(&ToolCallId::from("reused")),
        Some(&WaitCallState::Pending)
    );
}

/// Reusing a display ID after an old completed generation was reserved cannot
/// make interruption lose that generation. A later bare wait consumes the old
/// payload while the new pending declaration remains intact.
#[test]
fn wait_all_reuse_then_interruption_restores_old_generation() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    track_call(&mut tracker, &owner, "reused", call_ref(1, 0));
    track_call(&mut tracker, &owner, "other", call_ref(2, 0));
    tracker.record_tool_result(
        &background_placeholder("reused"),
        owner.clone(),
        observation(),
    );
    tracker.record_tool_result(
        &background_placeholder("other"),
        owner.clone(),
        observation(),
    );
    let old_terminal = tau_proto::ObservationId::from_bytes([71; 16]);
    tracker.record_background_result(
        background_result("reused", "old generation"),
        owner.clone(),
        Some(old_terminal),
    );
    tracker.retain_call_ref("wait-all".into(), call_ref(3, 0));
    tracker.retain_call_ref("wait-any".into(), call_ref(4, 0));
    assert!(
        start_wait_all(&mut tracker, &owner, "wait-all", &["reused", "other"])
            .reply
            .is_none()
    );
    tracker.record_exact_all_notice_suppressed(
        &ToolCallId::from("wait-all"),
        &ToolCallId::from("reused"),
        true,
    );

    tracker.reset_call_ref("reused".into(), call_ref(9, 0));
    tracker.record_tool_invoke("reused".into(), slow_tool_name(), owner.clone());
    assert_eq!(
        tracker
            .activate_waits_for(&owner, observation().expect("observation"))
            .len(),
        1
    );
    assert_eq!(
        tracker.oldest_completed_for_owner(&owner),
        Some(ToolCallId::from("reused"))
    );

    let reply = start_reply(start_wait_any(&mut tracker, &owner, "wait-any"));
    assert!(reply.suppress_call_id.is_none());
    assert!(reply.suppress_call_ids.is_empty());
    assert_eq!(
        reply.remove_pending_notices,
        vec![CompletionNotice {
            call_id: ToolCallId::from("reused"),
            source_call: call_ref(1, 0),
            source_terminal: old_terminal,
        }]
    );
    assert_eq!(
        cbor_map_text(&reply_result(reply.clone()), ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("reused")
    );
    let CborValue::Map(result) = reply_result(reply) else {
        panic!("released generation result must be wrapped");
    };
    assert_eq!(
        result
            .iter()
            .find(|(key, _)| key == &CborValue::Text("output".to_owned()))
            .map(|(_, value)| value),
        Some(&CborValue::Text("old generation".to_owned()))
    );
    assert_eq!(
        tracker.call_ref(&ToolCallId::from("reused")),
        Some(call_ref(9, 0))
    );
    assert_eq!(
        tracker.calls.get(&ToolCallId::from("reused")),
        Some(&WaitCallState::Pending)
    );
}

/// Reused display IDs cannot rewrite a released old generation's typed
/// cancellation outcome during plural-wait rollback.
#[test]
fn wait_all_reuse_then_interruption_preserves_old_cancellation_preview() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    track_call(&mut tracker, &owner, "reused", call_ref(1, 0));
    track_call(&mut tracker, &owner, "other", call_ref(2, 0));
    tracker.record_tool_result(
        &background_placeholder("reused"),
        owner.clone(),
        observation(),
    );
    tracker.record_tool_result(
        &background_placeholder("other"),
        owner.clone(),
        observation(),
    );
    let old_terminal = tau_proto::ObservationId::from_bytes([72; 16]);
    tracker.record_background_error(
        background_error("reused", "producer wording is not authority", None),
        owner.clone(),
        Some(old_terminal),
        BackgroundErrorOutcome::Cancelled,
    );
    tracker.retain_call_ref("wait-all".into(), call_ref(3, 0));
    assert!(
        start_wait_all(&mut tracker, &owner, "wait-all", &["reused", "other"])
            .reply
            .is_none()
    );
    tracker.record_exact_all_notice_suppressed(
        &ToolCallId::from("wait-all"),
        &ToolCallId::from("reused"),
        true,
    );

    tracker.reset_call_ref("reused".into(), call_ref(9, 0));
    tracker.record_tool_invoke("reused".into(), slow_tool_name(), owner.clone());
    let interruption =
        tracker.activate_waits_for(&owner, observation().expect("activation observation"));
    assert_eq!(interruption.len(), 1);

    let mut budget = BackgroundPreviewBudget::default();
    let preview = tracker
        .render_background_completion_preview(
            &ToolCallId::from("reused"),
            old_terminal,
            &mut budget,
        )
        .expect("released generation preview");
    assert!(preview.contains("tool_outcome=\"cancelled\""), "{preview}");
    assert!(!preview.contains("tool_outcome=\"error\""), "{preview}");
    assert_eq!(
        tracker.call_ref(&ToolCallId::from("reused")),
        Some(call_ref(9, 0))
    );
    assert_eq!(
        tracker.calls.get(&ToolCallId::from("reused")),
        Some(&WaitCallState::Pending)
    );
}

/// An old plural reservation never captures a reused generation's foreground
/// terminal or blocks a new exact waiter for that generation.
#[test]
fn wait_all_old_reservation_does_not_capture_reused_foreground_generation() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    track_call(&mut tracker, &owner, "reused", call_ref(1, 0));
    track_call(&mut tracker, &owner, "other", call_ref(2, 0));
    tracker.record_tool_result(
        &background_placeholder("reused"),
        owner.clone(),
        observation(),
    );
    tracker.record_tool_result(
        &background_placeholder("other"),
        owner.clone(),
        observation(),
    );
    tracker.record_background_result(
        background_result("reused", "old generation"),
        owner.clone(),
        observation(),
    );
    tracker.retain_call_ref("wait-all".into(), call_ref(3, 0));
    assert!(
        start_wait_all(&mut tracker, &owner, "wait-all", &["reused", "other"])
            .reply
            .is_none()
    );

    tracker.reset_call_ref("reused".into(), call_ref(9, 0));
    tracker.record_tool_invoke("reused".into(), slow_tool_name(), owner.clone());
    tracker.retain_call_ref("wait-new".into(), call_ref(10, 0));
    assert!(
        start_wait_exact(&mut tracker, &owner, "wait-new", "reused")
            .reply
            .is_none(),
        "old generation reservation must not block a new exact waiter"
    );
    let replies = tracker.record_tool_result(
        &foreground_result("reused", "new foreground"),
        owner.clone(),
        observation(),
    );
    assert_eq!(replies.len(), 1);
    assert_eq!(
        reply_result(replies.into_iter().next().expect("new exact reply")),
        CborValue::Text("new foreground".to_owned())
    );
    assert!(
        tracker
            .exact_all_waits
            .get(&ToolCallId::from("wait-all"))
            .is_some_and(|wait| {
                matches!(
                    &wait.members[0].completion,
                    Some(ExactAllCompletion::Result { output, .. })
                        if output == &CborValue::Text("old generation".to_owned())
                )
            })
    );
}

/// A reused generation's background terminal remains an independent FIFO
/// completion while the old generation stays reserved; rollback exposes old
/// then new in their original completion order.
#[test]
fn wait_all_old_reservation_does_not_capture_reused_background_generation() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    track_call(&mut tracker, &owner, "reused", call_ref(1, 0));
    track_call(&mut tracker, &owner, "other", call_ref(2, 0));
    tracker.record_tool_result(
        &background_placeholder("reused"),
        owner.clone(),
        observation(),
    );
    tracker.record_tool_result(
        &background_placeholder("other"),
        owner.clone(),
        observation(),
    );
    tracker.record_background_result(
        background_result("reused", "old generation"),
        owner.clone(),
        observation(),
    );
    tracker.retain_call_ref("wait-all".into(), call_ref(3, 0));
    assert!(
        start_wait_all(&mut tracker, &owner, "wait-all", &["reused", "other"])
            .reply
            .is_none()
    );

    tracker.reset_call_ref("reused".into(), call_ref(9, 0));
    tracker.record_tool_invoke("reused".into(), slow_tool_name(), owner.clone());
    tracker.record_tool_result(
        &background_placeholder("reused"),
        owner.clone(),
        observation(),
    );
    assert!(
        tracker
            .record_background_result(
                background_result("reused", "new generation"),
                owner.clone(),
                observation(),
            )
            .is_empty()
    );
    assert_eq!(
        tracker
            .activate_waits_for(&owner, observation().expect("observation"))
            .len(),
        1
    );

    tracker.retain_call_ref("wait-old".into(), call_ref(10, 0));
    let old = start_reply(start_wait_any(&mut tracker, &owner, "wait-old"));
    assert!(
        old.suppress_call_id.is_none(),
        "old-generation delivery must not suppress the reused generation's notice"
    );
    assert_eq!(
        old.remove_pending_notices.len(),
        0,
        "a generation without a notice obligation removes no notice"
    );
    let CborValue::Map(old_result) = reply_result(old) else {
        panic!("old result wrapper");
    };
    assert_eq!(
        old_result
            .iter()
            .find(|(key, _)| key == &CborValue::Text("output".to_owned()))
            .map(|(_, value)| value),
        Some(&CborValue::Text("old generation".to_owned()))
    );
    tracker.retain_call_ref("wait-new".into(), call_ref(11, 0));
    let new = start_reply(start_wait_any(&mut tracker, &owner, "wait-new"));
    let CborValue::Map(new_result) = reply_result(new) else {
        panic!("new result wrapper");
    };
    assert_eq!(
        new_result
            .iter()
            .find(|(key, _)| key == &CborValue::Text("output".to_owned()))
            .map(|(_, value)| value),
        Some(&CborValue::Text("new generation".to_owned()))
    );
}

/// Consuming the current generation through an exact wait leaves the older
/// released generation independently waitable without synthesizing a shared
/// notice action.
#[test]
fn exact_current_generation_restores_released_generation_notice() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    track_call(&mut tracker, &owner, "reused", call_ref(1, 0));
    track_call(&mut tracker, &owner, "other", call_ref(2, 0));
    tracker.record_tool_result(
        &background_placeholder("reused"),
        owner.clone(),
        observation(),
    );
    tracker.record_tool_result(
        &background_placeholder("other"),
        owner.clone(),
        observation(),
    );
    let old_terminal = tau_proto::ObservationId::from_bytes([73; 16]);
    tracker.record_background_result(
        background_result("reused", "old generation"),
        owner.clone(),
        Some(old_terminal),
    );
    tracker.retain_call_ref("wait-all".into(), call_ref(3, 0));
    assert!(
        start_wait_all(&mut tracker, &owner, "wait-all", &["reused", "other"])
            .reply
            .is_none()
    );
    tracker.record_exact_all_notice_suppressed(
        &ToolCallId::from("wait-all"),
        &ToolCallId::from("reused"),
        true,
    );
    tracker.reset_call_ref("reused".into(), call_ref(9, 0));
    tracker.record_tool_invoke("reused".into(), slow_tool_name(), owner.clone());
    assert_eq!(
        tracker
            .activate_waits_for(&owner, observation().expect("observation"))
            .len(),
        1
    );
    tracker.record_tool_result(
        &background_placeholder("reused"),
        owner.clone(),
        observation(),
    );
    tracker.record_background_result(
        background_result("reused", "current generation"),
        owner.clone(),
        observation(),
    );
    tracker.retain_call_ref("wait-exact".into(), call_ref(10, 0));

    let start = start_wait_exact(&mut tracker, &owner, "wait-exact", "reused");
    assert_eq!(start.suppress_call_id, Some(ToolCallId::from("reused")));
    let reply = start_reply(start);
    assert!(reply.unsuppress_call_id.is_none());
    assert!(reply.unsuppress_notices.is_empty());
    assert_eq!(
        tracker.oldest_completed_for_owner(&owner),
        Some(ToolCallId::from("reused"))
    );
}

/// Plural commit consumes only current generations and leaves an older released
/// generation independently waitable.
#[test]
fn wait_all_current_generation_restores_released_generation_notice() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    track_call(&mut tracker, &owner, "reused", call_ref(1, 0));
    track_call(&mut tracker, &owner, "other", call_ref(2, 0));
    tracker.record_tool_result(
        &background_placeholder("reused"),
        owner.clone(),
        observation(),
    );
    tracker.record_tool_result(
        &background_placeholder("other"),
        owner.clone(),
        observation(),
    );
    let old_terminal = tau_proto::ObservationId::from_bytes([72; 16]);
    tracker.record_background_result(
        background_result("reused", "old generation"),
        owner.clone(),
        Some(old_terminal),
    );
    tracker.retain_call_ref("old-wait".into(), call_ref(3, 0));
    assert!(
        start_wait_all(&mut tracker, &owner, "old-wait", &["reused", "other"])
            .reply
            .is_none()
    );
    tracker.record_exact_all_notice_suppressed(
        &ToolCallId::from("old-wait"),
        &ToolCallId::from("reused"),
        true,
    );
    tracker.reset_call_ref("reused".into(), call_ref(9, 0));
    tracker.record_tool_invoke("reused".into(), slow_tool_name(), owner.clone());
    assert_eq!(
        tracker
            .activate_waits_for(&owner, observation().expect("observation"))
            .len(),
        1
    );
    tracker.record_tool_result(
        &background_placeholder("reused"),
        owner.clone(),
        observation(),
    );
    tracker.record_background_result(
        background_result("reused", "current generation"),
        owner.clone(),
        observation(),
    );
    tracker.record_background_result(
        background_result("other", "other result"),
        owner.clone(),
        observation(),
    );
    tracker.retain_call_ref("current-wait".into(), call_ref(10, 0));
    let _reply = start_reply(start_wait_all(
        &mut tracker,
        &owner,
        "current-wait",
        &["reused", "other"],
    ));

    let (consumed, replies) = tracker.commit_exact_all_wait(&ToolCallId::from("current-wait"));
    assert_eq!(
        consumed,
        vec![ToolCallId::from("reused"), ToolCallId::from("other")]
    );
    assert!(replies.is_empty());
    assert_eq!(
        tracker.oldest_completed_for_owner(&owner),
        Some(ToolCallId::from("reused"))
    );
}

/// A released generation completed under DoNotQueue policy carries no notice
/// obligation into later singular or plural current-generation consumption.
#[test]
fn current_generation_waits_do_not_restore_notice_for_do_not_queue_release() {
    for plural in [false, true] {
        let owner = conv("main");
        let mut tracker = WaitTracker::default();
        track_call(&mut tracker, &owner, "reused", call_ref(1, 0));
        track_call(&mut tracker, &owner, "other", call_ref(2, 0));
        tracker.record_tool_result(
            &background_placeholder("reused"),
            owner.clone(),
            observation(),
        );
        tracker.record_tool_result(
            &background_placeholder("other"),
            owner.clone(),
            observation(),
        );
        tracker.record_background_result(
            background_result("reused", "old generation"),
            owner.clone(),
            Some(tau_proto::ObservationId::from_bytes([74; 16])),
        );
        tracker.retain_call_ref("old-wait".into(), call_ref(3, 0));
        assert!(
            start_wait_all(&mut tracker, &owner, "old-wait", &["reused", "other"])
                .reply
                .is_none()
        );
        tracker.reset_call_ref("reused".into(), call_ref(9, 0));
        tracker.record_tool_invoke("reused".into(), slow_tool_name(), owner.clone());
        assert_eq!(
            tracker
                .activate_waits_for(&owner, observation().expect("observation"))
                .len(),
            1
        );
        tracker.record_tool_result(
            &background_placeholder("reused"),
            owner.clone(),
            observation(),
        );
        tracker.record_background_result(
            background_result("reused", "current generation"),
            owner.clone(),
            observation(),
        );

        if plural {
            tracker.record_background_result(
                background_result("other", "other result"),
                owner.clone(),
                observation(),
            );
            tracker.retain_call_ref("current-wait".into(), call_ref(10, 0));
            let _reply = start_reply(start_wait_all(
                &mut tracker,
                &owner,
                "current-wait",
                &["reused", "other"],
            ));
            let (_consumed, _replies) =
                tracker.commit_exact_all_wait(&ToolCallId::from("current-wait"));
        } else {
            tracker.retain_call_ref("current-wait".into(), call_ref(10, 0));
            let reply = start_reply(start_wait_exact(
                &mut tracker,
                &owner,
                "current-wait",
                "reused",
            ));
            assert!(reply.unsuppress_notices.is_empty());
            assert!(reply.unsuppress_call_id.is_none());
        }
    }
}

/// Multiple released generations retain distinct FIFO notice identities and
/// each bare delivery removes only its own exact pending notice.
#[test]
fn multiple_released_notice_generations_keep_exact_fifo_correlation() {
    let owner = conv("main");
    let call_id = ToolCallId::from("reused");
    let first_generation = CompletionGeneration(1);
    let second_generation = CompletionGeneration(2);
    let mut tracker = WaitTracker::default();
    tracker.completion_order_by_owner.insert(
        owner.clone(),
        VecDeque::from([
            CompletionNode {
                call_id: call_id.clone(),
                generation: first_generation,
            },
            CompletionNode {
                call_id: call_id.clone(),
                generation: second_generation,
            },
        ]),
    );
    for (generation, call_ref, terminal) in [
        (
            first_generation,
            call_ref(1, 0),
            tau_proto::ObservationId::from_bytes([75; 16]),
        ),
        (
            second_generation,
            call_ref(2, 0),
            tau_proto::ObservationId::from_bytes([76; 16]),
        ),
    ] {
        tracker.released_completions.insert(
            generation,
            ReleasedCompletion {
                call_ref,
                tool_name: slow_tool_name(),
                completion: ExactAllCompletion::Result {
                    output: CborValue::Null,
                    display: None,
                    phase: tau_proto::ToolSourcePhase::Background,
                    terminal,
                },
                restore_notice: true,
            },
        );
    }

    for (index, (expected_call, expected_terminal)) in [
        (
            call_ref(1, 0),
            tau_proto::ObservationId::from_bytes([75; 16]),
        ),
        (
            call_ref(2, 0),
            tau_proto::ObservationId::from_bytes([76; 16]),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let wait_id = format!("wait-{index}");
        tracker.retain_call_ref(wait_id.clone().into(), call_ref(10 + index as u8, 0));
        let reply = start_reply(start_wait_any(&mut tracker, &owner, &wait_id));
        assert_eq!(
            reply.remove_pending_notices,
            vec![CompletionNotice {
                call_id: call_id.clone(),
                source_call: expected_call,
                source_terminal: expected_terminal,
            }]
        );
    }
}

/// Member cancellation becomes an ordered error member, while cancellation of
/// the plural wait itself releases every reservation without consuming sources.
#[test]
fn wait_all_cancellation_preserves_member_and_group_atomicity() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    for (index, call_id) in ["a", "b"].into_iter().enumerate() {
        track_call(
            &mut tracker,
            &owner,
            call_id,
            call_ref(u8::try_from(index + 1).expect("small index"), 0),
        );
    }
    tracker.retain_call_ref("wait-all".into(), call_ref(3, 0));
    assert!(
        start_wait_all(&mut tracker, &owner, "wait-all", &["a", "b"])
            .reply
            .is_none()
    );
    let cancelled_a = ToolCallId::from("a");
    let terminal_a = tau_proto::ObservationId::from_bytes([44; 16]);
    let first = tracker.record_tool_cancelled(
        &HashSet::from([cancelled_a.clone()]),
        Some((&cancelled_a, terminal_a)),
    );
    assert!(
        first.replies.is_empty(),
        "member cancellation does not fail fast"
    );
    let replies =
        tracker.record_tool_result(&foreground_result("b", "ok"), owner.clone(), observation());
    let result = reply_result(replies.into_iter().next().expect("plural reply"));
    let CborValue::Map(root) = result else {
        panic!("plural result must be a map");
    };
    let CborValue::Array(members) = &root[0].1 else {
        panic!("plural results must be an array");
    };
    assert_eq!(cbor_map_text(&members[0], "outcome"), Some("error"));
    assert_eq!(
        cbor_map_text(&members[0], "message"),
        Some("Tool call `a` was cancelled")
    );
    let CborValue::Map(cancelled_member) = &members[0] else {
        panic!("cancelled member must be a map");
    };
    assert_eq!(
        cancelled_member
            .iter()
            .find(|(key, _)| key == &CborValue::Text("details".to_owned()))
            .map(|(_, value)| value),
        Some(&CborValue::Null)
    );
    assert_eq!(cbor_map_text(&members[1], "outcome"), Some("result"));
    let CborValue::Map(ok_member) = &members[1] else {
        panic!("successful member must be a map");
    };
    assert_eq!(
        ok_member
            .iter()
            .find(|(key, _)| key == &CborValue::Text("output".to_owned()))
            .map(|(_, value)| value),
        Some(&CborValue::Text("ok".to_owned()))
    );

    let mut tracker = WaitTracker::default();
    track_call(&mut tracker, &owner, "source", call_ref(5, 0));
    tracker.retain_call_ref("cancelled-wait".into(), call_ref(6, 0));
    assert!(
        start_wait_all(&mut tracker, &owner, "cancelled-wait", &["source"],)
            .reply
            .is_none()
    );
    let cancelled_wait = ToolCallId::from("cancelled-wait");
    let cancellation = tracker.record_tool_cancelled(
        &HashSet::from([cancelled_wait.clone()]),
        Some((
            &cancelled_wait,
            tau_proto::ObservationId::from_bytes([45; 16]),
        )),
    );
    assert_eq!(cancellation.cancelled_waits.len(), 1);
    assert!(cancellation.unsuppress_call_ids.is_empty());
    assert_eq!(
        cancellation.release_suppression_call_ids,
        vec![ToolCallId::from("source")]
    );
    assert!(tracker.exact_all_reservations.is_empty());
    assert_eq!(
        tracker.calls.get(&ToolCallId::from("source")),
        Some(&WaitCallState::Pending)
    );
}

/// Manual-compaction claim defers a plural terminal until rollback and owner
/// teardown drops both active and terminal-publication-pending reservations.
#[test]
fn wait_all_compaction_claim_and_owner_teardown_preserve_runtime_boundaries() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    track_call(&mut tracker, &owner, "a", call_ref(1, 0));
    track_call(&mut tracker, &owner, "b", call_ref(2, 0));
    tracker.retain_call_ref("wait-all".into(), call_ref(3, 0));
    assert!(
        start_wait_all(&mut tracker, &owner, "wait-all", &["a", "b"])
            .reply
            .is_none()
    );
    assert!(tracker.claim_wait_for_manual_compaction(&owner, &ToolCallId::from("wait-all")));
    assert!(
        tracker
            .record_tool_result(&foreground_result("a", "a"), owner.clone(), observation())
            .is_empty()
    );
    assert!(
        tracker
            .record_tool_result(&foreground_result("b", "b"), owner.clone(), observation())
            .is_empty()
    );
    let replies = tracker.rollback_manual_compaction_claim(&owner, &ToolCallId::from("wait-all"));
    assert_eq!(replies.len(), 1);
    assert!(tracker.pending_exact_all_commits.contains_key("wait-all"));

    let retired = tracker.discard_owner(&owner);
    assert!(retired.contains(&ToolCallId::from("wait-all")));
    assert!(tracker.exact_all_reservations.is_empty());
    assert!(tracker.pending_exact_all_commits.is_empty());
}
