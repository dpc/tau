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
