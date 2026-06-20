use tau_proto::{
    AgentStarted, Event, PromptOriginator, SessionAgentLoaded, SessionAgentUnloaded, ToolError,
    ToolName, ToolType,
};

use super::semantic_event_router::{session_membership_id_for_event, should_persist_event};
use crate::parse_agent_id;

#[test]
fn transient_non_tool_event_is_not_persisted() {
    let event = Event::AgentStarted(AgentStarted {
        parent_agent: None,
        agent_id: parse_agent_id("agent-1"),
        role: "default".into(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    });

    assert!(!should_persist_event(&event, true));
    assert!(should_persist_event(&event, false));
}

#[test]
fn transient_terminal_tool_event_is_persisted() {
    let event = Event::ToolError(ToolError {
        call_id: "call-1".into(),
        tool_name: ToolName::new("tool"),
        tool_type: ToolType::Function,
        message: "failed".to_owned(),
        details: None,
        display: None,
        originator: PromptOriginator::User,
    });

    assert!(should_persist_event(&event, true));
}

#[test]
fn session_membership_events_route_to_session_log() {
    let loaded = Event::SessionAgentLoaded(SessionAgentLoaded {
        session_id: "session-1".into(),
        agent_id: parse_agent_id("agent-1"),
        ephemeral: false,
    });
    let unloaded = Event::SessionAgentUnloaded(SessionAgentUnloaded {
        session_id: "session-2".into(),
        agent_id: parse_agent_id("agent-1"),
    });

    assert_eq!(
        session_membership_id_for_event(&loaded).as_deref(),
        Some("session-1")
    );
    assert_eq!(
        session_membership_id_for_event(&unloaded).as_deref(),
        Some("session-2")
    );
}
