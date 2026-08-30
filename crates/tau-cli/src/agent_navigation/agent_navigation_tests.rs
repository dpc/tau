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
        let agent_id = tau_proto::AgentId::parse("worker").expect("valid agent id");
        navigation.mark_live(agent_id.clone());
        assert!(!navigation.is_active(&agent_id));
        navigation.apply_stats(&agent_id, mode, tau_proto::AgentRuntimeState::Idle);
        assert_eq!(
            navigation.is_active(&agent_id),
            mode == AgentNavigationState::Active
        );
        navigation.apply_stats(&agent_id, mode, tau_proto::AgentRuntimeState::Running);
        assert_eq!(
            navigation.is_active(&agent_id),
            mode != AgentNavigationState::Suspended
        );
    }
}

/// Ensures unload removes the complete cached snapshot and a same-id reload
/// remains ineligible until the harness sends its fresh classification.
#[test]
fn unload_removes_all_navigation_facts() {
    let mut navigation = AgentNavigation::default();
    let agent_id = tau_proto::AgentId::parse("worker").expect("valid agent id");
    navigation.mark_live(agent_id.clone());
    navigation.apply_stats(
        &agent_id,
        AgentNavigationState::Suspended,
        tau_proto::AgentRuntimeState::Running,
    );
    navigation.unload(&agent_id);
    assert!(!navigation.is_live(&agent_id));
    assert!(!navigation.is_active(&agent_id));

    navigation.mark_live(agent_id.clone());
    assert!(!navigation.is_active(&agent_id));
    navigation.apply_stats(
        &agent_id,
        AgentNavigationState::ActiveAuto,
        tau_proto::AgentRuntimeState::Idle,
    );
    assert_eq!(navigation.mode(&agent_id), AgentNavigationState::ActiveAuto);
    assert!(!navigation.is_active(&agent_id));
}
