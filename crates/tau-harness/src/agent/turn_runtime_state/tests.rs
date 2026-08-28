use super::AutomaticCompactionRuntimeState;

/// Ensures advancing an eager decision replaces its decision phase and an
/// unrelated terminal cannot clear the resulting protected start.
#[test]
fn automatic_compaction_runtime_state_owns_one_correlated_phase() {
    let decision_id = tau_proto::CompactionTransactionId::parse("ct-1").expect("valid id");
    let other_id = tau_proto::CompactionTransactionId::parse("ct-2").expect("valid id");
    let mut state = AutomaticCompactionRuntimeState::default();

    state.record_decision(decision_id.clone());
    assert_eq!(state.decision_id(), Some(&decision_id));

    state.record_start(decision_id.clone());
    assert_eq!(state.decision_id(), None);
    assert!(state.start_is_pending_for(&decision_id));

    state.clear_start(&other_id);
    assert!(state.start_is_pending_for(&decision_id));
    state.clear_start(&decision_id);
    assert!(state.transaction_id().is_none());
}
