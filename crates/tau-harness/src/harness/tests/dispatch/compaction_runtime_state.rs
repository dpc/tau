use super::*;
use crate::harness::{CompactionRuntimeState, PendingManualCompactionTool};

/// One agent-scoped transaction has exactly one delivery owner, so replay or
/// duplicate reactions cannot retain contradictory UI and model-tool owners.
#[test]
fn manual_compaction_start_owner_is_exclusive_per_agent_transaction() {
    let agent_id = tau_proto::AgentId::parse("agent-owner").expect("agent id");
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-owner").expect("transaction id");
    let request_id = tau_proto::CompactionRequestId::parse("cr-owner-ui").expect("request id");
    let mut state = CompactionRuntimeState::default();

    state.record_ui_start(agent_id.clone(), transaction_id.clone(), request_id.clone());
    state.record_model_tool_start(
        agent_id.clone(),
        transaction_id.clone(),
        PendingManualCompactionTool {
            request_id: tau_proto::CompactionRequestId::parse("cr-owner-tool").expect("request id"),
            caller_agent_id: agent_id.clone(),
            call_id: ToolCallId::from("call-owner"),
            tool_name: ToolName::new("compact"),
            target_agent_id: agent_id.clone(),
        },
    );

    assert_eq!(
        state.ui_start_request(agent_id.clone(), transaction_id.clone()),
        Some(&request_id)
    );
    assert!(!state.has_model_tool_start(agent_id, transaction_id));
    assert_eq!(state.active_manual_start_count(), 1);
}

/// Owner-specific transition helpers cannot consume the other delivery route;
/// the canonical terminal reaction can therefore try UI then model ownership.
#[test]
fn manual_compaction_start_transition_consumes_only_matching_owner() {
    let agent_id = tau_proto::AgentId::parse("agent-transition").expect("agent id");
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-transition").expect("transaction id");
    let call_id = ToolCallId::from("call-transition");
    let mut state = CompactionRuntimeState::default();
    state.record_model_tool_start(
        agent_id.clone(),
        transaction_id.clone(),
        PendingManualCompactionTool {
            request_id: tau_proto::CompactionRequestId::parse("cr-transition").expect("request id"),
            caller_agent_id: agent_id.clone(),
            call_id: call_id.clone(),
            tool_name: ToolName::new("compact"),
            target_agent_id: agent_id.clone(),
        },
    );

    state.remove_ui_start(agent_id.clone(), transaction_id.clone());
    assert!(state.has_model_tool_start(agent_id.clone(), transaction_id.clone()));
    let pending = state
        .take_model_tool_start(agent_id, transaction_id)
        .expect("model-tool owner");
    assert_eq!(pending.call_id, call_id);
    assert!(state.active_manual_starts_is_empty());
}

/// Session rollover clears model-tool delivery while preserving the independent
/// UI coalescing owner and its durable request correlation.
#[test]
fn manual_compaction_rollover_clears_only_model_tool_owners() {
    let agent_id = tau_proto::AgentId::parse("agent-rollover").expect("agent id");
    let ui_transaction =
        tau_proto::CompactionTransactionId::parse("ct-rollover-ui").expect("transaction id");
    let model_transaction =
        tau_proto::CompactionTransactionId::parse("ct-rollover-model").expect("transaction id");
    let ui_request = tau_proto::CompactionRequestId::parse("cr-rollover-ui").expect("request id");
    let mut state = CompactionRuntimeState::default();
    state.record_ui_start(agent_id.clone(), ui_transaction.clone(), ui_request.clone());
    state.record_model_tool_start(
        agent_id.clone(),
        model_transaction.clone(),
        PendingManualCompactionTool {
            request_id: tau_proto::CompactionRequestId::parse("cr-rollover-model")
                .expect("request id"),
            caller_agent_id: agent_id.clone(),
            call_id: ToolCallId::from("call-rollover"),
            tool_name: ToolName::new("compact"),
            target_agent_id: agent_id.clone(),
        },
    );

    state.clear_model_tool_starts();

    assert_eq!(
        state.ui_start_request(agent_id.clone(), ui_transaction),
        Some(&ui_request)
    );
    assert!(!state.has_model_tool_start(agent_id, model_transaction));
    assert_eq!(state.active_manual_start_count(), 1);
}
