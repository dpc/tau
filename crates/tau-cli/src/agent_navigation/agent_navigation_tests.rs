use super::*;

/// Covers the complete authoritative mode/runtime truth table and ensures
/// loaded membership alone never makes an agent eligible before its stats
/// snapshot.
#[test]
fn effective_activity_truth_table_requires_stats() {
    for mode in [
        AgentNavigationState::Active,
        AgentNavigationState::ActiveAuto,
        AgentNavigationState::Suspended,
    ] {
        let mut navigation = AgentNavigation::default();
        navigation.mark_live("worker");
        assert!(!navigation.is_active("worker"));
        navigation.apply_stats("worker", mode, tau_proto::AgentRuntimeState::Idle);
        assert_eq!(
            navigation.is_active("worker"),
            mode == AgentNavigationState::Active
        );
        navigation.apply_stats("worker", mode, tau_proto::AgentRuntimeState::Running);
        assert_eq!(
            navigation.is_active("worker"),
            mode != AgentNavigationState::Suspended
        );
    }
}

/// Ensures unload removes the complete cached snapshot and a same-id reload
/// remains ineligible until the harness sends its fresh classification.
#[test]
fn unload_removes_all_navigation_facts() {
    let mut navigation = AgentNavigation::default();
    navigation.mark_live("worker");
    navigation.apply_stats(
        "worker",
        AgentNavigationState::Suspended,
        tau_proto::AgentRuntimeState::Running,
    );
    navigation.unload("worker");
    assert!(!navigation.is_live("worker"));
    assert!(!navigation.is_active("worker"));

    navigation.mark_live("worker");
    assert!(!navigation.is_active("worker"));
    navigation.apply_stats(
        "worker",
        AgentNavigationState::ActiveAuto,
        tau_proto::AgentRuntimeState::Idle,
    );
    assert_eq!(navigation.mode("worker"), AgentNavigationState::ActiveAuto);
    assert!(!navigation.is_active("worker"));
}
