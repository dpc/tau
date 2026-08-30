use tau_proto::{BackgroundSupport, CborValue};

use super::*;

fn cid(value: &str) -> AgentId {
    crate::parse_agent_id(value)
}

fn call(id: &str) -> AgentToolCall {
    AgentToolCall {
        call_ref: None,
        id: id.into(),
        name: ToolName::new("tool"),
        tool_type: ToolType::Function,
        arguments: CborValue::Null,
    }
}

/// Builds one invocation whose dominant allocation makes payload ownership
/// observable without allocator or timing inference.
fn large_call(id: &str, bytes: usize) -> AgentToolCall {
    AgentToolCall {
        arguments: CborValue::Text("x".repeat(bytes)),
        ..call(id)
    }
}

fn push(machine: &mut ToolTurnMachine, cid: &AgentId, id: &str) {
    machine.push(cid.clone(), call(id), BackgroundSupport::Never);
}

fn pop_id(machine: &mut ToolTurnMachine) -> Option<String> {
    machine
        .pop_dispatchable(Instant::now())
        .map(|(pending, _)| pending.invocation.id.as_str().to_owned())
}

/// Active-category aggregation must retain real background calls, apply the
/// conservative uncategorized fallback, and combine concurrent categories.
#[test]
fn active_categories_cover_all_real_in_flight_calls() {
    let mut machine = ToolTurnMachine::default();
    let conv = cid("conv");
    let fetch_id = ToolCallId::from("fetch");
    machine.record_unqueued_in_flight(
        conv.clone(),
        fetch_id.clone(),
        ToolTurnCategories::from_tags(&[tau_proto::ToolTag::new(
            tau_proto::TURN_DATA_FETCH_TOOL_TAG,
        )]),
    );
    assert!(machine.begin_backgrounding(&fetch_id));
    assert!(machine.mark_backgrounded(&fetch_id));
    machine.record_unqueued_in_flight(
        conv.clone(),
        ToolCallId::from("wait"),
        ToolTurnCategories::from_tags(&[tau_proto::ToolTag::new(tau_proto::TURN_WAIT_TOOL_TAG)]),
    );

    let categories = machine.active_categories_for(&conv);
    assert!(!categories.manipulator());
    assert!(categories.data_fetch());
    assert!(categories.wait());

    machine.record_unqueued_in_flight(
        conv.clone(),
        ToolCallId::from("uncategorized"),
        ToolTurnCategories::default(),
    );
    assert!(machine.active_categories_for(&conv).manipulator());

    assert!(machine.mark_complete(&ToolCallId::from("uncategorized")));
    assert!(!machine.active_categories_for(&conv).manipulator());
    assert!(machine.mark_complete(&fetch_id));
    assert!(!machine.active_categories_for(&conv).data_fetch());
}

#[test]
fn queued_calls_dispatch_in_provider_order_without_global_locking() {
    let mut machine = ToolTurnMachine::default();
    let conv = cid("conv");
    push(&mut machine, &conv, "first");
    push(&mut machine, &conv, "second");
    push(&mut machine, &conv, "third");

    assert_eq!(pop_id(&mut machine).as_deref(), Some("first"));
    assert_eq!(pop_id(&mut machine).as_deref(), Some("second"));
    assert_eq!(pop_id(&mut machine).as_deref(), Some("third"));
    assert_eq!(machine.pending_len(), 0);
    assert_eq!(machine.in_flight_len(), 3);
}

/// The former clone-first probe is executable beside the borrowed probe. Both
/// select the same call, but only the former duplicates its multi-megabyte
/// argument allocation before the queue transfers the original allocation.
#[test]
fn borrowed_candidate_probe_eliminates_pending_payload_clone() {
    for mebibytes in [1, 2, 4, 8] {
        let bytes = mebibytes * 1024 * 1024;
        let legacy_id = format!("legacy-{mebibytes}");
        start_pending_tool_ownership_probe(&legacy_id);
        let mut legacy = ToolTurnMachine::default();
        legacy.push(
            cid("conv"),
            large_call(&legacy_id, bytes),
            BackgroundSupport::Never,
        );
        let probe = legacy
            .next_dispatchable()
            .cloned()
            .expect("legacy candidate");
        assert_eq!(probe.invocation.id.as_str(), legacy_id);
        let (legacy_pending, _) = legacy
            .pop_dispatchable(Instant::now())
            .expect("legacy dispatch");
        assert_eq!(legacy_pending.invocation.id.as_str(), legacy_id);
        let legacy_work = finish_pending_tool_ownership_probe(&legacy_id);

        let borrowed_id = format!("borrowed-{mebibytes}");
        start_pending_tool_ownership_probe(&borrowed_id);
        let mut borrowed = ToolTurnMachine::default();
        borrowed.push(
            cid("conv"),
            large_call(&borrowed_id, bytes),
            BackgroundSupport::Never,
        );
        assert_eq!(
            borrowed
                .next_dispatchable()
                .map(|pending| pending.invocation.id.as_str()),
            Some(borrowed_id.as_str())
        );
        let (borrowed_pending, _) = borrowed
            .pop_dispatchable(Instant::now())
            .expect("borrowed dispatch");
        assert_eq!(borrowed_pending.invocation.id.as_str(), borrowed_id);
        let borrowed_work = finish_pending_tool_ownership_probe(&borrowed_id);

        assert_eq!(legacy_work.pending_clones, 1);
        assert_eq!(borrowed_work.pending_clones, 0);
        assert_eq!(legacy_work.candidate_visits, 1);
        assert_eq!(borrowed_work.candidate_visits, 1);
        assert_eq!(legacy_work.queue_pops, 1);
        assert_eq!(borrowed_work.queue_pops, 1);
        assert_eq!(legacy_work.admission_text_ptr, legacy_work.popped_text_ptr);
        assert_eq!(
            borrowed_work.admission_text_ptr,
            borrowed_work.popped_text_ptr
        );
    }
}

/// A failed readiness gate can inspect the same front entry repeatedly without
/// changing its queue position, ownership, or deadline start. Cancellation
/// still removes that untouched entry without ever marking it in flight.
#[test]
fn borrowed_candidate_remains_owned_by_queue_until_pop_or_cancel() {
    let blocked_id = "blocked-large";
    start_pending_tool_ownership_probe(blocked_id);
    let mut machine = ToolTurnMachine::default();
    machine.push(
        cid("conv"),
        large_call(blocked_id, 1024 * 1024),
        BackgroundSupport::MinForegroundSeconds(5),
    );

    for _ in 0..3 {
        assert_eq!(
            machine
                .next_dispatchable()
                .map(|pending| pending.invocation.id.as_str()),
            Some(blocked_id)
        );
    }
    assert_eq!(machine.pending_len(), 1);
    assert_eq!(machine.in_flight_len(), 0);
    assert!(machine.next_background_deadline().is_none());

    let cancelled =
        machine.cancel_queued_for(&cid("conv"), &HashSet::from([ToolCallId::from(blocked_id)]));
    assert_eq!(cancelled.len(), 1);
    let work = finish_pending_tool_ownership_probe(blocked_id);
    assert_eq!(work.pending_clones, 0);
    assert_eq!(work.candidate_visits, 3);
    assert_eq!(work.queue_pops, 0);
    assert_eq!(work.popped_text_ptr, 0);
    assert_eq!(work.execution_text_ptr, 0);
}

#[test]
fn conversation_predicates_report_pending_and_foreground_in_flight_work() {
    let mut machine = ToolTurnMachine::default();
    let conv = cid("conv");
    let other = cid("other");
    push(&mut machine, &conv, "shared");

    assert!(machine.any_pending_for(&conv));
    assert!(!machine.any_pending_for(&other));
    assert!(!machine.any_in_flight_for(&conv));

    assert_eq!(pop_id(&mut machine).as_deref(), Some("shared"));
    assert!(!machine.any_pending_for(&conv));
    assert!(machine.any_in_flight_for(&conv));
    assert!(!machine.any_in_flight_for(&other));
}

/// Instant background support asks the harness to close the foreground at
/// dispatch time while keeping the actual tool call tracked until its real
/// result arrives.
#[test]
fn instant_background_completes_foreground_but_remains_running() {
    let mut machine = ToolTurnMachine::default();
    let conv = cid("conv");
    machine.push(conv.clone(), call("bg"), BackgroundSupport::Instant);

    let (pending, action) = machine.pop_dispatchable(Instant::now()).expect("dispatch");
    assert_eq!(pending.invocation.id.as_str(), "bg");
    assert_eq!(
        action,
        ForegroundAction::Background {
            call_id: "bg".into()
        }
    );
    assert!(!machine.is_backgrounded(&"bg".into()));
    assert!(machine.any_in_flight_for(&conv));
    assert!(machine.begin_backgrounding(&"bg".into()));
    assert!(!machine.begin_backgrounding(&"bg".into()));
    assert!(machine.any_in_flight_for(&conv));
    assert!(machine.mark_backgrounded(&"bg".into()));
    assert!(machine.is_backgrounded(&"bg".into()));
    assert!(!machine.any_in_flight_for(&conv));
    assert_eq!(machine.in_flight_len(), 1);
}

/// MinForegroundSeconds uses the dispatch instant as the start time. The
/// harness event loop can sleep until `next_background_deadline` instead of
/// polling.
#[test]
fn min_foreground_deadline_backgrounds_once_when_due() {
    let mut machine = ToolTurnMachine::default();
    let conv = cid("conv");
    let start = Instant::now();
    machine.push(
        conv,
        call("slow"),
        BackgroundSupport::MinForegroundSeconds(5),
    );
    let (_, action) = machine.pop_dispatchable(start).expect("dispatch");
    assert_eq!(action, ForegroundAction::None);
    assert_eq!(
        machine.background_due(start + std::time::Duration::from_secs(4)),
        Vec::<ToolCallId>::new()
    );

    assert_eq!(
        machine.background_due(start + std::time::Duration::from_secs(5)),
        vec![ToolCallId::from("slow")]
    );
    assert!(machine.begin_backgrounding(&"slow".into()));
    assert_eq!(
        machine.background_due(start + std::time::Duration::from_secs(6)),
        Vec::<ToolCallId>::new()
    );
    assert!(machine.mark_backgrounded(&"slow".into()));
    assert!(machine.is_backgrounded(&"slow".into()));
}

/// Never preserves old foreground behavior: no deadline is armed, but it no
/// longer participates in harness-side tool locking.
#[test]
fn never_background_has_no_deadline_and_does_not_block_dispatch() {
    let mut machine = ToolTurnMachine::default();
    let conv = cid("conv");
    machine.push(conv.clone(), call("never"), BackgroundSupport::Never);
    machine.push(conv, call("behind"), BackgroundSupport::Never);
    let (_, action) = machine.pop_dispatchable(Instant::now()).expect("dispatch");
    assert_eq!(action, ForegroundAction::None);
    assert!(machine.next_background_deadline().is_none());
    assert_eq!(pop_id(&mut machine).as_deref(), Some("behind"));
}

/// A late real result removes actual-running state exactly once after the
/// foreground was already closed by the synthetic background placeholder.
#[test]
fn late_background_completion_clears_actual_running_once() {
    let mut machine = ToolTurnMachine::default();
    let conv = cid("conv");
    machine.push(conv, call("late"), BackgroundSupport::Instant);
    machine.pop_dispatchable(Instant::now()).expect("dispatch");
    assert!(machine.begin_backgrounding(&"late".into()));
    assert!(machine.mark_backgrounded(&"late".into()));
    assert!(machine.is_backgrounded(&"late".into()));

    assert!(machine.mark_complete(&"late".into()));
    assert!(!machine.mark_complete(&"late".into()));
    assert!(!machine.is_backgrounded(&"late".into()));
}
