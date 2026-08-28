//! Typed eager reducer state for side-conversation provider terminals.

/// Exact eager reducer input constructed after side-conversation
/// classification.
pub(crate) struct EagerSideConversationTerminal<'a> {
    /// Classified side-conversation authority and route.
    pub(super) plan: super::provider_terminal_plan::SideConversationTerminalPlan,
    /// Fully normalized canonical provider terminal.
    pub(super) response: &'a tau_proto::ProviderResponseFinished,
    /// Whether this is an extension query without tool authority.
    pub(super) is_non_tool_ext_query: bool,
    /// Display-only assistant text returned to the requesting extension.
    pub(super) assistant_text: Option<&'a str>,
    /// Exhaustive prompt-tool effect selected from reconciled call presence.
    pub(super) tool_effect: SideConversationToolEffect<'a>,
    /// Provider connection retained for synthetic tool-error attribution.
    pub(super) source: Option<&'a tau_proto::ConnectionId>,
}

/// Exact prompt-tool effect for one side-conversation terminal.
pub(crate) enum SideConversationToolEffect<'a> {
    /// Release the prompt-local tool snapshot for a no-tool terminal.
    ClearPromptSnapshot,
    /// Reject this exact normalized call aggregate without dispatching it.
    Reject(&'a mut super::NormalizedFinishedToolCalls),
}
