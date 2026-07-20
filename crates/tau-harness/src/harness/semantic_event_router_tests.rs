use tau_proto::{
    AgentStarted, CborValue, Event, ExtensionName, PromptOriginator, ProviderModelsDeclared,
    ProviderModelsUpdated, SessionAgentLoaded, SessionAgentUnloaded, ToolError, ToolName,
    ToolResult, ToolResultKind, ToolType,
};

use super::semantic_event_router::{session_membership_id_for_event, should_persist_event};
use crate::parse_agent_id;

/// Provider declaration and canonical model snapshots are reconstructed current
/// state, never durable semantic history.
#[test]
fn provider_model_state_never_enters_semantic_history() {
    for event in [
        Event::ProviderModelsDeclared(ProviderModelsDeclared { models: Vec::new() }),
        Event::ProviderModelsUpdated(ProviderModelsUpdated {
            publisher_extension_id: ExtensionName::from("provider"),
            models: Vec::new(),
        }),
    ] {
        assert!(!should_persist_event(&event, false));
        assert!(!should_persist_event(&event, true));
    }
}

/// Ensures ordinary transient facts remain live-only so progress/status updates
/// cannot accidentally enter durable session or agent replay logs.
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

/// Ensures raw extension-owned tool completions stay live-only semantic events
/// even when published through a non-transient helper path; their
/// provider-owned counterparts are the durable transcript facts.
#[test]
fn raw_tool_terminal_events_are_not_persisted() {
    let result = Event::ToolResult(ToolResult {
        call_id: "call-1".into(),
        tool_name: ToolName::new("tool"),
        tool_type: ToolType::Function,
        result: CborValue::Null,
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: None,
        originator: PromptOriginator::User,
    });
    let event = Event::ToolError(ToolError {
        call_id: "call-1".into(),
        tool_name: ToolName::new("tool"),
        tool_type: ToolType::Function,
        message: "failed".to_owned(),
        details: None,
        display: None,
        originator: PromptOriginator::User,
    });

    assert!(!should_persist_event(&result, false));
    assert!(!should_persist_event(&result, true));
    assert!(!should_persist_event(&event, false));
    assert!(!should_persist_event(&event, true));
}

/// Ensures transient durable tool completions still reach the agent store for
/// resume/replay; only raw `tool.result` / `tool.error` observer facts are
/// filtered out before semantic persistence.
#[test]
fn transient_provider_terminal_tool_event_is_persisted() {
    let event = Event::ProviderToolError(ToolError {
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

/// Ensures session membership facts continue routing by their embedded session
/// id instead of falling through to agent transcript persistence.
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
