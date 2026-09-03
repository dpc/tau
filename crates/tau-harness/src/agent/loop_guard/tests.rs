use super::*;

/// Ensures a progress reset keeps unresolved tool-call argument signatures
/// so a sibling success in the same provider turn cannot make later
/// failures collapse to an argument-unavailable loop signature.
#[test]
fn progress_reset_preserves_in_flight_tool_signatures() {
    let mut guard = LoopGuardState::default();
    guard.push_recent(
        LoopTurnSignature::AssistantText("repeated text".to_owned()),
        8,
    );
    guard.push_tool_call_signature("call-a".into(), "read:{path:a}".to_owned(), 8);

    guard.reset_for_progress();

    assert_eq!(
        guard.take_tool_call_signature(&ToolCallId::from("call-a")),
        Some("read:{path:a}".to_owned())
    );
    assert!(guard.recent.is_empty());
    assert!(guard.cycles.is_empty());
}

/// Repeated successful self-compaction uses the existing three-cycle breaker
/// threshold and blocks only after the dispatched breaker fails to change the
/// cycle.
#[test]
fn repeated_self_compaction_uses_existing_breaker_lifecycle() {
    let mut guard = LoopGuardState::default();
    let signature = LoopTurnSignature::SelfCompaction;

    for _ in 0..2 {
        guard.push_recent(signature.clone(), 8);
        assert!(!guard.recent_repeats(&signature, 3));
    }
    guard.push_recent(signature.clone(), 8);
    assert!(guard.recent_repeats(&signature, 3));

    guard.remember_cycle_pending("self-compaction".to_owned(), 8);
    guard.mark_pending_breakers_dispatched();
    guard.mark_cycle_blocked("self-compaction");
    assert_eq!(
        guard.cycle_state("self-compaction"),
        Some(LoopCycleState::Blocked)
    );
    assert!(guard.stop_automatic_continuation());
}

/// Ensures a branch/head invalidation drops unresolved signatures because
/// they were captured against the previous branch cursor.
#[test]
fn branch_invalidation_clears_in_flight_tool_signatures() {
    let mut guard = LoopGuardState::default();
    guard.push_tool_call_signature("call-a".into(), "read:{path:a}".to_owned(), 8);

    guard.invalidate_branch();

    assert_eq!(
        guard.take_tool_call_signature(&ToolCallId::from("call-a")),
        None
    );
}

/// Ensures cycle breaker bookkeeping stays bounded and keeps the newest
/// cycle records, preventing long-running loops from growing runtime state
/// without bound.
#[test]
fn cycle_bookkeeping_is_bounded() {
    let mut guard = LoopGuardState::default();

    for idx in 0..6 {
        guard.remember_cycle_pending(format!("cycle-{idx}"), 3);
    }

    assert_eq!(guard.cycles.len(), 3);
    assert_eq!(guard.cycles[0].key, "cycle-3");
    assert_eq!(guard.cycles[2].key, "cycle-5");
}

/// Ensures pending breakers advance to dispatched only when the harness
/// explicitly marks the queued pivot as folded into the transcript.
#[test]
fn pending_breakers_advance_to_dispatched() {
    let mut guard = LoopGuardState::default();
    guard.remember_cycle_pending("cycle".to_owned(), 4);

    assert_eq!(
        guard.cycle_state("cycle"),
        Some(LoopCycleState::BreakerPending)
    );

    guard.mark_pending_breakers_dispatched();

    assert_eq!(
        guard.cycle_state("cycle"),
        Some(LoopCycleState::BreakerDispatched)
    );
}
