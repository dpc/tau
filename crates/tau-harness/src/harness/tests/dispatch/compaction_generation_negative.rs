//! Focused generation-negative explicit-compaction admission oracles.

use super::*;

/// An explicit request remains admissible through generation-scoped negative
/// evidence so the provider can refresh identity; this does not restore the
/// automatic-compaction capability bit.
#[test]
fn manual_cross_compaction_admits_generation_negative_model() {
    let (_td, mut h, caller, _target, call, target_id) = setup_manual_cross_compaction_test();
    let model = h
        .provider_runtime
        .model_info
        .get_mut(&"echo/model".into())
        .expect("echo model");
    model.supports_standalone_compaction = false;
    model.standalone_compaction_generation_negative = true;

    h.request_agent_tool_compaction(
        &caller,
        &call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );

    assert!(event_log_events(&h).into_iter().any(|event| matches!(
        event,
        Event::AgentStandaloneCompactionStarted(started)
            if started.agent_id == target_id
    )));
    assert!(!h.provider_runtime.model_info[&"echo/model".into()].supports_standalone_compaction);
}
