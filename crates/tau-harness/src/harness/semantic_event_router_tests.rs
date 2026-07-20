use tau_proto::{
    AgentStarted, CborValue, Event, ExtensionName, PromptOriginator, ProviderModelsDeclared,
    ProviderModelsUpdated, SessionAgentLoaded, SessionAgentUnloaded, ToolError, ToolName,
    ToolProgress, ToolResult, ToolResultKind, ToolType,
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

/// Tool declarations and canonical registry state are runtime lifecycle only,
/// even if a peer sends an incorrect durable Emit override.
#[test]
fn tool_lifecycle_state_never_enters_semantic_history() {
    let declaration = tau_proto::ToolRegistrationDeclared {
        tool: tau_proto::ToolSpec {
            name: ToolName::new("runtime_tool"),
            model_visible_name: None,
            description: None,
            parameters: None,
            format: None,
            tool_type: ToolType::Function,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
        tool_group: None,
        prompt_fragment: None,
    };
    for event in [
        Event::ToolRegistrationDeclared(declaration.clone()),
        Event::ToolRegister(tau_proto::ToolRegister {
            publisher_extension_id: ExtensionName::from("tool"),
            publisher_instance_id: 1.into(),
            tool: declaration.tool,
            tool_group: None,
            prompt_fragment: None,
        }),
        Event::ToolUnregistrationDeclared(tau_proto::ToolUnregistrationDeclared {
            tool_name: ToolName::new("runtime_tool"),
        }),
        Event::ToolUnregister(tau_proto::ToolUnregister {
            publisher_extension_id: ExtensionName::from("tool"),
            publisher_instance_id: 1.into(),
            tool_name: ToolName::new("runtime_tool"),
        }),
    ] {
        assert!(!should_persist_event(&event, false));
        assert!(!should_persist_event(&event, true));
    }
}

/// Peer progress observations and canonical harness progress stay live-only
/// even when a sender incorrectly requests durable publication.
#[test]
fn tool_progress_never_enters_semantic_history() {
    let progress = ToolProgress {
        call_id: "progress-call".into(),
        tool_name: ToolName::new("runtime_tool"),
        message: Some("running".to_owned()),
        progress: None,
        display: None,
    };
    for event in [
        Event::ToolProgressReported(progress.clone()),
        Event::ToolProgress(progress),
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
