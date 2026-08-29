//! Semantic persistence classification for harness events.

use tau_proto::{Event, SessionId};

/// Return whether an event should enter durable semantic stores.
///
/// Events that do not request persistence normally exist only for live
/// observers. `AgentPromptStarted` and canonical terminal tool completions are
/// exceptions because `AgentTree` folds them into prompt-generation and
/// terminal-tool state. A shell completion explicitly excluded from context
/// remains transient even if a caller requests persistence.
pub(crate) fn should_persist_event(event: &Event, persist: bool) -> bool {
    if matches!(
        event,
        Event::ShellCommandFinished(finished) if !finished.include_in_context
    ) {
        return false;
    }
    if matches!(event, Event::AgentPromptCreated(_)) {
        return false;
    }
    if event.is_message_report()
        || matches!(
            event,
            Event::ProviderModelsDeclared(_)
                | Event::AgentRuntimeIndicatorsDeclared(_)
                | Event::ProviderModelsUpdated(_)
                | Event::ProviderModelDeclarationDiagnostic(_)
                | Event::ToolRegistrationDeclared(_)
                | Event::ToolUnregistrationDeclared(_)
                | Event::ToolRegister(_)
                | Event::ToolUnregister(_)
                | Event::ToolProgressReported(_)
                | Event::ToolProgress(_)
                | Event::ToolResultReported(_)
                | Event::ToolErrorReported(_)
                | Event::ToolCancelledReported(_)
                | Event::ShellCommandProgressReported(_)
                | Event::ShellCommandFinishedReported(_)
                | Event::ProviderQuotaReplaceReported(_)
                | Event::ProviderQuotaPatchReported(_)
                | Event::ProviderQuotaClearReported(_)
                | Event::HarnessProviderQuotaChanged(_)
                | Event::ProviderPromptSubmittedReported(_)
                | Event::ProviderResponseUpdatedReported(_)
                | Event::ProviderResponseFinishedReported(_)
                | Event::ProviderRetryPromptResultReported(_)
                | Event::ProviderCacheMissDiagnosticReported(_)
                | Event::ProviderCacheRefreshFinishedReported(_)
                | Event::ProviderCacheRefreshFinished(_)
                | Event::AgentCacheRefreshRequested(_)
                | Event::AgentCacheRefreshCancelRequested(_)
                | Event::ExtPromptFragmentPublish(_)
                | Event::ExtensionSessionDiscoverySnapshotDeclared(_)
                | Event::ExtensionAgentDiscoverySnapshotDeclared(_)
                | Event::ExtensionSessionContextProviderRegister(_)
                | Event::ExtensionSessionContextReady(_)
                | Event::ExtensionContextProviderRegister(_)
                | Event::ExtAgentContextPublish(_)
                | Event::ExtensionContextReady(_)
                | Event::ExtInternalPromptSubmitRequest(_)
                | Event::StartAgentRequest(_)
                | Event::AgentMetadataSetRequest(_)
                | Event::AgentMetadataUnsetRequest(_)
                | Event::Osc1337SetUserVar(_)
                | Event::TermBell(_)
                | Event::ExtensionEvent(_)
                | Event::UiPromptDraft(_)
                | Event::UiFocusChanged(_)
        )
        || is_raw_tool_terminal_event(event)
    {
        return false;
    }
    persist || is_persistence_exception(event)
}

/// Return the session log target for session membership events.
pub(crate) fn session_membership_id_for_event(event: &Event) -> Option<SessionId> {
    match event {
        Event::SessionAgentLoaded(loaded) => Some(loaded.session_id.clone()),
        Event::SessionAgentUnloaded(unloaded) => Some(unloaded.session_id.clone()),
        _ => None,
    }
}

fn is_persistence_exception(event: &Event) -> bool {
    matches!(
        event,
        Event::AgentPromptStarted(_)
            | Event::ProviderStandaloneExecutionAccounted(_)
            | Event::ProviderStandaloneExecutionAccountingCorrected(_)
            | Event::ProviderToolResult(_)
            | Event::ProviderToolError(_)
            | Event::ToolCancelled(_)
            | Event::ToolBackgroundResult(_)
            | Event::ToolBackgroundError(_)
    )
}

fn is_raw_tool_terminal_event(event: &Event) -> bool {
    matches!(
        event,
        Event::ToolResult(_)
            | Event::ToolResultDisplay(_)
            | Event::ToolError(_)
            | Event::ToolBackgroundResultDisplay(_)
    )
}
