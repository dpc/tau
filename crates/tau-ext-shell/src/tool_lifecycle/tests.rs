use std::sync::mpsc;

use tau_proto::{AgentId, ToolCallId, ToolName};

use super::*;

/// Ensures cancellation after effect start remains visible while the active
/// cancellation sender is being registered, then leaves no terminal tombstone.
#[test]
fn effect_started_cancellation_survives_sender_handoff() {
    let (tx, _rx) = mpsc::channel();
    let registry = ToolLifecycleRegistry::default();
    let call_id = ToolCallId::new("effect-started");
    let lifecycle = registry.admit(
        call_id.clone(),
        ToolName::new("shell"),
        AgentId::parse("agent-a").expect("agent id"),
        Output::channel(tx),
    );

    assert!(lifecycle.start_effect());
    assert_eq!(
        registry.cancel(&call_id),
        Some(CancelOutcome::EffectStarted)
    );
    assert!(lifecycle.effect_cancel_requested());
    lifecycle.finish();

    assert_eq!(registry.cancel(&call_id), None);
}

/// Ensures terminal preparation failure keeps effect-started ownership live
/// even after the manual loop consumes its error, so disconnect cleanup rather
/// than successful worker completion remains responsible for settlement.
#[test]
fn terminal_preparation_failure_preserves_lifecycle_ownership() {
    let (tx, rx) = mpsc::channel();
    let output = Output::channel(tx);
    let registry = ToolLifecycleRegistry::default();
    let call_id = ToolCallId::new("preparation-failure");
    let lifecycle = registry.admit(
        call_id.clone(),
        ToolName::new("read"),
        AgentId::parse("agent-a").expect("agent id"),
        output.clone(),
    );
    assert!(lifecycle.start_effect());
    let oversized = "x".repeat(
        usize::try_from(tau_client::MAX_OUTBOUND_FRAME_BYTES).expect("frame limit fits usize"),
    );
    let result = output.report_tool_terminal(Event::ToolError(tau_proto::ToolError {
        presentation: Default::default(),
        call_id: call_id.clone(),
        tool_name: ToolName::new("read"),
        tool_type: ToolType::Function,
        message: oversized.clone(),
        details: None,
        display: Some(tau_proto::ToolUseState {
            status: tau_proto::ToolUseStatus::Error,
            status_text: oversized,
            ..Default::default()
        }),
        originator: Default::default(),
    }));
    assert!(matches!(result, Err(tau_client::ClientError::Overloaded)));
    assert!(
        rx.try_recv().is_err(),
        "failed preparation must submit no frame"
    );

    output
        .take_mandatory_failure()
        .expect_err("manual loop observes preparation failure");
    if !output.mandatory_output_failed() {
        lifecycle.finish();
    }

    assert_eq!(
        registry.cancel(&call_id),
        Some(CancelOutcome::EffectStarted)
    );
}
