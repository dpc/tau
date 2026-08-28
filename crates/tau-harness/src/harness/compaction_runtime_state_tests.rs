//! Verifies typed runtime ownership for compaction starts and pending
//! acceptance.

use super::compaction_runtime_state::{
    CompactionRuntimeState, ManualCompactionRequestKey, SuppressedStart,
};
use super::{
    AcceptedManualCompactionTool, PendingManualCompactionAcceptance, StagedManualCompactionTool,
};

/// Construct a model-tool request with an explicitly chosen agent-local id.
fn model_request(
    target_agent_id: &str,
    request_id: &tau_proto::CompactionRequestId,
    call_id: &str,
) -> tau_proto::AgentManualCompactionRequested {
    tau_proto::AgentManualCompactionRequested {
        request_id: request_id.clone(),
        target_agent_id: tau_proto::AgentId::parse(target_agent_id).expect("valid target id"),
        source: tau_proto::ManualCompactionSource::Tool(tau_proto::ManualToolCompactionSource {
            caller_agent_id: tau_proto::AgentId::parse("requesting-agent")
                .expect("valid caller id"),
            initiating_agent_prompt_id: tau_proto::AgentPromptId::parse("ap-requesting-1")
                .expect("valid prompt id"),
            initiating_tool_call_id: call_id.into(),
            initiating_tool_name: tau_proto::ManualCompactionTool::AgentCompact,
            visible_tool_name: tau_proto::ToolName::new("agent_compact"),
            resume_inference: false,
        }),
        requested_target_head: tau_proto::AgentHead::Root,
        target_generation: tau_proto::MaterializedPromptGeneration::initial(),
        model: "test/model".into(),
    }
}

/// Construct a UI request with an explicitly chosen agent-local id.
fn ui_request(
    target_agent_id: &str,
    request_id: &tau_proto::CompactionRequestId,
) -> tau_proto::AgentManualCompactionRequested {
    tau_proto::AgentManualCompactionRequested {
        request_id: request_id.clone(),
        target_agent_id: tau_proto::AgentId::parse(target_agent_id).expect("valid target id"),
        source: tau_proto::ManualCompactionSource::UiCompact {
            ui_compact: tau_proto::UiManualCompactionSource {
                eligible_automatic_transaction_id: None,
                target_role: "test-role".into(),
            },
        },
        requested_target_head: tau_proto::AgentHead::Root,
        target_generation: tau_proto::MaterializedPromptGeneration::initial(),
        model: "test/model".into(),
    }
}

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

/// Request ids are local to the target journal, so equal ids on two targets
/// must retain independent same-origin and cross-origin pre-commit owners.
#[test]
fn pending_manual_acceptances_scope_equal_request_ids_to_target_agent() {
    let request_id =
        tau_proto::CompactionRequestId::parse("cr-equal-local").expect("valid request id");
    let model_a = model_request("target-a", &request_id, "call-target-a");
    let model_b = model_request("target-b", &request_id, "call-target-b");
    let mut state = CompactionRuntimeState::default();
    state.pending_manual_acceptances.insert(
        ManualCompactionRequestKey::for_request(&model_a),
        PendingManualCompactionAcceptance::ModelTool(StagedManualCompactionTool {
            request: model_a.clone(),
            visible_tool_name: tau_proto::ToolName::new("agent_compact"),
        }),
    );
    state.pending_manual_acceptances.insert(
        ManualCompactionRequestKey::for_request(&model_b),
        PendingManualCompactionAcceptance::ModelTool(StagedManualCompactionTool {
            request: model_b.clone(),
            visible_tool_name: tau_proto::ToolName::new("agent_compact"),
        }),
    );

    assert_eq!(state.pending_manual_acceptances.len(), 2);
    let key_a = ManualCompactionRequestKey::for_request(&model_a);
    let key_b = ManualCompactionRequestKey::for_request(&model_b);
    state.pending_ui_acknowledgements.insert(
        key_a.clone(),
        vec![tau_proto::ConnectionId::parse("ui-target-a").expect("valid connection id")],
    );
    state.pending_ui_acknowledgements.insert(
        key_b.clone(),
        vec![tau_proto::ConnectionId::parse("ui-target-b").expect("valid connection id")],
    );
    assert_eq!(
        state
            .pending_ui_acknowledgements
            .remove(&key_a)
            .expect("target A ACK owner")[0]
            .as_str(),
        "ui-target-a"
    );
    assert_eq!(
        state
            .pending_ui_acknowledgements
            .remove(&key_b)
            .expect("target B ACK owner")[0]
            .as_str(),
        "ui-target-b"
    );
    let removed_a = state
        .remove_pending_model_acceptance(&ManualCompactionRequestKey::for_request(&model_a))
        .expect("target A model owner");
    assert_eq!(
        removed_a
            .request
            .required_tool_source()
            .initiating_tool_call_id
            .as_str(),
        "call-target-a"
    );
    assert_eq!(
        state
            .remove_pending_model_acceptance(&ManualCompactionRequestKey::for_request(&model_b))
            .expect("target B model owner")
            .request
            .required_tool_source()
            .initiating_tool_call_id
            .as_str(),
        "call-target-b"
    );

    let ui_b = ui_request("target-b", &request_id);
    state.pending_manual_acceptances.insert(
        ManualCompactionRequestKey::for_request(&model_a),
        PendingManualCompactionAcceptance::ModelTool(StagedManualCompactionTool {
            request: model_a.clone(),
            visible_tool_name: tau_proto::ToolName::new("agent_compact"),
        }),
    );
    state.pending_manual_acceptances.insert(
        ManualCompactionRequestKey::for_request(&ui_b),
        PendingManualCompactionAcceptance::Ui(AcceptedManualCompactionTool {
            request: ui_b.clone(),
            visible_tool_name: tau_proto::ToolName::new("compact"),
        }),
    );

    assert_eq!(state.pending_manual_acceptances.len(), 2);
    assert!(
        state
            .remove_pending_ui_acceptance(&ManualCompactionRequestKey::for_request(&model_a))
            .is_none(),
        "a competing origin on the same target must not consume the first owner"
    );
    assert_eq!(
        state
            .remove_pending_ui_acceptance(&ManualCompactionRequestKey::for_request(&ui_b))
            .expect("target B UI owner")
            .request
            .target_agent_id,
        ui_b.target_agent_id
    );
    assert_eq!(
        state
            .remove_pending_model_acceptance(&ManualCompactionRequestKey::for_request(&model_a))
            .expect("target A model owner remains")
            .request
            .required_tool_source()
            .initiating_tool_call_id
            .as_str(),
        "call-target-a"
    );
}
