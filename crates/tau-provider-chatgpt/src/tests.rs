use super::*;

#[test]
fn publishes_chatgpt_model_metadata() {
    // ChatGPT account profiles do not store models; this crate is the
    // source of truth for their published model IDs and capabilities.
    let models = models_for_provider(&ProviderName::new("work-chatgpt"));
    let ids = models
        .iter()
        .map(|model| model.id.to_string())
        .collect::<Vec<_>>();

    assert_eq!(
        ids,
        vec![
            "work-chatgpt/gpt-5.5",
            "work-chatgpt/gpt-5.4",
            "work-chatgpt/gpt-5.4-mini",
            "work-chatgpt/gpt-5.3-codex",
        ],
    );
    assert!(models.iter().all(|model| model.supports_compaction));
    assert!(
        models
            .iter()
            .all(|model| model.tags.iter().any(|tag| tag.as_str() == "shell:chatgpt"))
    );
    assert!(models.iter().all(|model| {
        model
            .tags
            .iter()
            .any(|tag| tag.as_str() == "tools:custom-text")
    }));
}

#[test]
fn config_for_model_enables_codex_responses_capabilities() {
    // The builtin registry only supplies account credentials; ChatGPT owns
    // the Responses feature matrix for its model IDs.
    let config = config_for_model(
        &ModelName::new("gpt-5.3-codex"),
        "token".to_owned(),
        Some("account".to_owned()),
    );

    assert_eq!(config.surface, responses::ResponsesSurface::ChatGpt);
    assert_eq!(config.base_url, DEFAULT_BASE_URL);
    assert_eq!(config.api_key, "token");
    assert_eq!(config.account_id.as_deref(), Some("account"));
    assert!(config.supports_websocket);
    assert!(config.supports_compaction);
    assert!(config.supports_phase);
    assert!(config.supports_encrypted_reasoning);
}
