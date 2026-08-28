//! Verifies exclusive runtime reactions for committed compaction starts.

use super::compaction_runtime_state::{CompactionRuntimeState, SuppressedStart};

/// Construct stable agent-local transaction correlation for reaction tests.
fn transaction() -> (tau_proto::AgentId, tau_proto::CompactionTransactionId) {
    (
        tau_proto::AgentId::parse("agent-runtime-state").expect("valid agent id"),
        tau_proto::CompactionTransactionId::parse("ct-runtime-state")
            .expect("valid transaction id"),
    )
}

/// Ensures each legal suppression reason is consumed as exactly one typed
/// reaction rather than parallel flags.
#[test]
fn suppressed_start_records_each_legal_reaction() {
    let (agent_id, transaction_id) = transaction();
    let mut state = CompactionRuntimeState::default();

    state.suppress_start_for_queued_terminal(agent_id.clone(), transaction_id.clone());
    assert_eq!(
        state.take_suppressed_start(agent_id.clone(), transaction_id.clone()),
        Some(SuppressedStart::TerminalAlreadyQueued)
    );

    state.suppress_start_for_cancellation(agent_id.clone(), transaction_id.clone());
    assert_eq!(
        state.take_suppressed_start(agent_id.clone(), transaction_id.clone()),
        Some(SuppressedStart::Cancelled)
    );

    state.suppress_start_for_preflight(
        agent_id.clone(),
        transaction_id.clone(),
        tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge,
    );
    assert_eq!(
        state.take_suppressed_start(agent_id, transaction_id),
        Some(SuppressedStart::PreflightFailure(
            tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge
        ))
    );
    assert!(state.suppressed_starts_is_empty());
}

/// Ensures competing reactions preserve the previous implementation's
/// preflight-over-cancellation-over-queued-terminal precedence without
/// retaining contradictory combinations.
#[test]
fn suppressed_start_competition_keeps_one_highest_priority_reaction() {
    let (agent_id, transaction_id) = transaction();
    let mut state = CompactionRuntimeState::default();

    state.suppress_start_for_queued_terminal(agent_id.clone(), transaction_id.clone());
    state.suppress_start_for_cancellation(agent_id.clone(), transaction_id.clone());
    state.suppress_start_for_queued_terminal(agent_id.clone(), transaction_id.clone());
    state.suppress_start_for_preflight(
        agent_id.clone(),
        transaction_id.clone(),
        tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge,
    );
    state.suppress_start_for_cancellation(agent_id.clone(), transaction_id.clone());
    state.suppress_start_for_queued_terminal(agent_id.clone(), transaction_id.clone());

    assert_eq!(
        state.take_suppressed_start(agent_id, transaction_id),
        Some(SuppressedStart::PreflightFailure(
            tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge
        ))
    );
    assert!(state.suppressed_starts_is_empty());
}
