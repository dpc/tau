use std::sync as path_std_sync;
use std::sync::atomic as path_std_sync_atomic;

#[cfg(test)]
mod tests;
use tau_proto::{PromptMessageClass, PromptOriginator, UiCreateAgent};

static NEXT_CREATE_REQUEST_ID: path_std_sync::atomic::AtomicU64 =
    path_std_sync_atomic::AtomicU64::new(1);

/// Default role used when the UI submits a prompt without an explicit selected
/// role from session state.
pub(crate) const DEFAULT_AGENT_ROLE: &str = "engineer";

/// Whether downstream prompt-command processors may interpret canonical text.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum PromptCommandHandling {
    /// Apply the ordinary harness-owned prompt-command grammar.
    #[default]
    Interpret,
    /// Preserve text from a source that bypasses prompt command processing.
    LiteralEscape,
}

impl PromptCommandHandling {
    fn is_literal_escape(self) -> bool {
        matches!(self, Self::LiteralEscape)
    }
}

/// One-shot options applied while building a user-owned agent creation request.
#[derive(Clone, Debug, Default)]
pub(crate) struct CreateUserAgentPromptOptions {
    /// Model override installed before the first prompt is dispatched.
    pub(crate) model_override: Option<tau_proto::ModelId>,
    /// Whether the new agent should be memory-only for the daemon lifetime.
    pub(crate) ephemeral: bool,
    /// Controls whether harness-owned prompt commands may interpret the text.
    pub(crate) command_handling: PromptCommandHandling,
}

/// Build the standard user-originated create-agent event used by interactive
/// chat and one-shot/headless prompt submission paths.
pub(crate) fn create_user_agent_prompt(
    session_id: &tau_proto::SessionId,
    role: impl Into<String>,
    prompt: impl Into<String>,
    options: CreateUserAgentPromptOptions,
) -> UiCreateAgent {
    let next_id = || NEXT_CREATE_REQUEST_ID.fetch_add(1, path_std_sync_atomic::Ordering::Relaxed);
    let process_id = std::process::id();
    UiCreateAgent {
        request_id: format!("ui-create-{process_id}-{}", next_id()),
        parent_agent: None,
        session_id: session_id.clone(),
        role: role.into(),
        model_override: options.model_override,
        metadata: Vec::new(),
        initial_prompt: Some(prompt.into()),
        literal: options.command_handling.is_literal_escape(),
        message_class: PromptMessageClass::User,
        originator: PromptOriginator::User,
        ctx_id: Some(format!("ui-prompt-{process_id}-{}", next_id())),
        ephemeral: options.ephemeral,
    }
}
