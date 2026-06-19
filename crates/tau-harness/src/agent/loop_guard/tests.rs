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
