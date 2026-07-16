use tau_proto::{Event, PromptMessageClass, PromptOriginator, UiCreateAgent};

/// Default role used when the UI submits a prompt without an explicit selected
/// role from session state.
pub(crate) const DEFAULT_AGENT_ROLE: &str = "engineer";

/// One-shot options applied while building a user-owned agent creation request.
#[derive(Clone, Debug, Default)]
pub(crate) struct CreateUserAgentPromptOptions {
    /// Model override installed before the first prompt is dispatched.
    pub(crate) model_override: Option<tau_proto::ModelId>,
    /// Whether the new agent should be memory-only for the daemon lifetime.
    pub(crate) ephemeral: bool,
}

/// Build the standard user-originated create-agent event used by interactive
/// chat and one-shot/headless prompt submission paths.
pub(crate) fn create_user_agent_prompt(
    session_id: &str,
    role: impl Into<String>,
    prompt: impl Into<String>,
    options: CreateUserAgentPromptOptions,
) -> Event {
    Event::UiCreateAgent(UiCreateAgent {
        parent_agent: None,
        session_id: session_id.into(),
        role: role.into(),
        model_override: options.model_override,
        metadata: Vec::new(),
        initial_prompt: Some(prompt.into()),
        message_class: PromptMessageClass::User,
        originator: PromptOriginator::User,
        ctx_id: None,
        ephemeral: options.ephemeral,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_role_matches_built_in_harness_default() {
        let built_in = tau_config::settings::HarnessSettings::built_in();
        assert_eq!(built_in.default_role.as_deref(), Some(DEFAULT_AGENT_ROLE));
    }

    /// The interactive `/new` + `/model` flow must be able to carry a one-shot
    /// model override through the create-agent event for the first prompt.
    #[test]
    fn create_user_agent_prompt_preserves_model_override() {
        let model: tau_proto::ModelId = "test/override".parse().expect("model id");
        let Event::UiCreateAgent(req) = create_user_agent_prompt(
            "s1",
            "engineer",
            "hello",
            CreateUserAgentPromptOptions {
                model_override: Some(model.clone()),
                ephemeral: false,
            },
        ) else {
            panic!("expected create agent event");
        };

        assert_eq!(req.model_override, Some(model));
    }

    /// The interactive `/new` + `/ephemeral` flow must be able to carry a
    /// one-shot memory-only request through the create-agent event.
    #[test]
    fn create_user_agent_prompt_preserves_ephemeral_flag() {
        let Event::UiCreateAgent(req) = create_user_agent_prompt(
            "s1",
            "engineer",
            "hello",
            CreateUserAgentPromptOptions {
                model_override: None,
                ephemeral: true,
            },
        ) else {
            panic!("expected create agent event");
        };

        assert!(req.ephemeral);
        assert!(
            req.metadata.is_empty(),
            "the UI must not seed any shell instance from its own filesystem namespace"
        );
    }
}
