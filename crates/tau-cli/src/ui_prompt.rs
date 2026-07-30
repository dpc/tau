use tau_proto::{PromptMessageClass, PromptOriginator, UiCreateAgent};

static NEXT_CREATE_REQUEST_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Default role used when the UI submits a prompt without an explicit selected
/// role from session state.
pub(crate) const DEFAULT_AGENT_ROLE: &str = "engineer";

/// Whether downstream prompt-command processors may interpret canonical text.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum PromptCommandHandling {
    /// Apply the ordinary harness-owned prompt-command grammar.
    #[default]
    Interpret,
    /// Preserve canonical text produced by the doubled-colon literal escape.
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
    /// Controls harness-owned prompt-command interpretation.
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
    let next_id = || NEXT_CREATE_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_role_matches_built_in_harness_default() {
        let built_in = tau_config::settings::HarnessSettings::built_in();
        assert_eq!(built_in.default_role.as_deref(), Some(DEFAULT_AGENT_ROLE));
    }

    /// The interactive `:new` + `:model` flow must be able to carry a one-shot
    /// model override through the create-agent event for the first prompt.
    #[test]
    fn create_user_agent_prompt_preserves_model_override() {
        let model: tau_proto::ModelId = "test/override".parse().expect("model id");
        let req = create_user_agent_prompt(
            &tau_proto::SessionId::parse("s1").expect("test session id"),
            "engineer",
            "hello",
            CreateUserAgentPromptOptions {
                model_override: Some(model.clone()),
                ephemeral: false,
                command_handling: PromptCommandHandling::Interpret,
            },
        );

        assert_eq!(req.model_override, Some(model));
    }

    /// The interactive `:new` + `:ephemeral` flow must be able to carry a
    /// one-shot memory-only request through the create-agent event.
    #[test]
    fn create_user_agent_prompt_preserves_ephemeral_flag() {
        let req = create_user_agent_prompt(
            &tau_proto::SessionId::parse("s1").expect("test session id"),
            "engineer",
            "hello",
            CreateUserAgentPromptOptions {
                model_override: None,
                ephemeral: true,
                command_handling: PromptCommandHandling::Interpret,
            },
        );

        assert!(req.ephemeral);
        assert!(
            req.metadata.is_empty(),
            "the UI must not seed any shell instance from its own filesystem namespace"
        );
    }
}
