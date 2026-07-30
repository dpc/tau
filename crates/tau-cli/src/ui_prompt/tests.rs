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
