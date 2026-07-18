use super::*;

/// Ensures every navigation command emits the requested absolute action and
/// carries the captured session and target without touching a UI cache.
#[test]
fn navigation_requests_preserve_all_absolute_actions() {
    for action in [
        tau_proto::UiAgentNavigationModeAction::SetActive,
        tau_proto::UiAgentNavigationModeAction::SetActiveAuto,
        tau_proto::UiAgentNavigationModeAction::SetSuspended,
    ] {
        let event = set_agent_navigation_mode(
            "session-1",
            tau_proto::AgentId::parse("agent-1").expect("agent id"),
            action,
        );
        let Event::UiSetAgentNavigationMode(request) = event else {
            panic!("expected navigation request");
        };
        assert_eq!(request.session_id.as_str(), "session-1");
        assert_eq!(request.agent_id.as_str(), "agent-1");
        assert_eq!(request.action, action);
    }
}
