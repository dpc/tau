//! Semantic persistence classification for harness events.

use tau_proto::{Event, SessionId};

/// Return whether an event should enter durable semantic stores.
///
/// Transient events normally exist only for live observers. Terminal tool
/// events are the exception: they must still be persisted so resumed agents can
/// see tool completions that happened after a transient dispatch path.
pub(crate) fn should_persist_event(event: &Event, transient: bool) -> bool {
    if event.is_message_report()
        || matches!(
            event,
            Event::ProviderModelsDeclared(_)
                | Event::ProviderModelsUpdated(_)
                | Event::ToolRegistrationDeclared(_)
                | Event::ToolUnregistrationDeclared(_)
                | Event::ToolRegister(_)
                | Event::ToolUnregister(_)
                | Event::ToolProgressReported(_)
                | Event::ToolProgress(_)
                | Event::ToolResultReported(_)
                | Event::ToolErrorReported(_)
                | Event::ToolCancelledReported(_)
                | Event::ProviderQuotaReplaceReported(_)
                | Event::ProviderQuotaPatchReported(_)
                | Event::ProviderQuotaClearReported(_)
                | Event::HarnessProviderQuotaChanged(_)
                | Event::ProviderPromptSubmittedReported(_)
                | Event::ProviderResponseUpdatedReported(_)
                | Event::ProviderResponseFinishedReported(_)
                | Event::ProviderRetryPromptResultReported(_)
                | Event::ProviderCacheMissDiagnosticReported(_)
                | Event::ExtPromptFragmentPublish(_)
                | Event::ExtSkillAvailable(_)
                | Event::ExtAgentsMdAvailable(_)
                | Event::ExtensionSessionContextProviderRegister(_)
                | Event::ExtensionSessionContextReady(_)
                | Event::ExtensionContextProviderRegister(_)
                | Event::ExtAgentContextPublish(_)
                | Event::ExtensionContextReady(_)
                | Event::ExtInternalPromptSubmitRequest(_)
        )
        || is_raw_tool_terminal_event(event)
    {
        return false;
    }
    !transient || is_transient_tool_terminal_event(event)
}

/// Return the session log target for session membership events.
pub(crate) fn session_membership_id_for_event(event: &Event) -> Option<SessionId> {
    match event {
        Event::SessionAgentLoaded(loaded) => Some(loaded.session_id.clone()),
        Event::SessionAgentUnloaded(unloaded) => Some(unloaded.session_id.clone()),
        _ => None,
    }
}

fn is_transient_tool_terminal_event(event: &Event) -> bool {
    matches!(
        event,
        Event::ProviderToolResult(_)
            | Event::ProviderToolError(_)
            | Event::ToolCancelled(_)
            | Event::ToolBackgroundResult(_)
            | Event::ToolBackgroundError(_)
    )
}

fn is_raw_tool_terminal_event(event: &Event) -> bool {
    matches!(event, Event::ToolResult(_) | Event::ToolError(_))
}
