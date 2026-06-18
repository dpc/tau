use std::path::PathBuf;

use tau_proto::{
    AgentInitialMetadata, CborValue, Event, PromptMessageClass, PromptOriginator, UiCreateAgent,
};

/// Default role used when the UI submits a prompt without an explicit selected
/// role from session state.
pub(crate) const DEFAULT_AGENT_ROLE: &str = "senior-engineer";

/// Build the standard user-originated create-agent event used by interactive
/// chat and one-shot/headless prompt submission paths.
pub(crate) fn create_user_agent_prompt(
    session_id: &str,
    role: impl Into<String>,
    prompt: impl Into<String>,
    model_override: Option<tau_proto::ModelId>,
) -> Event {
    Event::UiCreateAgent(UiCreateAgent {
        parent_agent: None,
        session_id: session_id.into(),
        role: role.into(),
        model_override,
        metadata: shell_cwd_metadata(),
        initial_prompt: Some(prompt.into()),
        message_class: PromptMessageClass::User,
        originator: PromptOriginator::User,
        ctx_id: None,
    })
}

pub(crate) fn shell_cwd_metadata() -> Vec<AgentInitialMetadata> {
    vec![AgentInitialMetadata {
        key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
        value: CborValue::Text(
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .display()
                .to_string(),
        ),
        inheritable: true,
    }]
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
        let Event::UiCreateAgent(req) =
            create_user_agent_prompt("s1", "engineer", "hello", Some(model.clone()))
        else {
            panic!("expected create agent event");
        };

        assert_eq!(req.model_override, Some(model));
    }
}
