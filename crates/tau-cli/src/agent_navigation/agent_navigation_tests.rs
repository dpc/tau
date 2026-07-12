use super::*;

/// Covers the complete mode/runtime/live truth table so every consumer can rely
/// on the single effective-activity calculation.
#[test]
fn effective_activity_truth_table() {
    for mode in [
        AgentNavigationState::Active,
        AgentNavigationState::ActiveAuto,
        AgentNavigationState::Suspended,
    ] {
        let mut navigation = AgentNavigation::default();
        assert!(!navigation.is_active("worker"));
        navigation.mark_live("worker");
        navigation.set_mode("worker", mode);
        assert_eq!(
            navigation.is_active("worker"),
            mode == AgentNavigationState::Active
        );
        navigation.update_runtime("worker", tau_proto::AgentRuntimeState::Idle);
        assert_eq!(
            navigation.is_active("worker"),
            mode == AgentNavigationState::Active
        );
        navigation.update_runtime("worker", tau_proto::AgentRuntimeState::Running);
        assert_eq!(
            navigation.is_active("worker"),
            mode != AgentNavigationState::Suspended
        );
    }
}

/// Ensures unload removes local overrides and runtime facts before a same-id
/// reload receives its newly reconstructed classification.
#[test]
fn unload_removes_all_navigation_facts() {
    let mut navigation = AgentNavigation::default();
    navigation.mark_active_auto_if_absent("worker");
    navigation.update_runtime("worker", tau_proto::AgentRuntimeState::Running);
    navigation.set_mode("worker", AgentNavigationState::Suspended);

    navigation.unload("worker");
    assert!(!navigation.is_live("worker"));
    assert!(!navigation.is_active("worker"));
    assert_eq!(navigation.mode("worker"), AgentNavigationState::Active);

    navigation.set_mode("worker", AgentNavigationState::Active);
    assert!(!navigation.is_live("worker"));
    assert!(!navigation.is_active("worker"));
    navigation.set_mode("worker", AgentNavigationState::Suspended);
    assert_eq!(navigation.mode("worker"), AgentNavigationState::Active);

    navigation.mark_active_auto_if_absent("worker");
    assert_eq!(navigation.mode("worker"), AgentNavigationState::ActiveAuto);
    assert!(!navigation.is_active("worker"));
}
