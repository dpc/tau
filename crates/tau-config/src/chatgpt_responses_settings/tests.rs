use super::*;

/// Shared profile projection rejects other adapters and malformed mode values
/// while retaining the exact default and audited-model effective-mode rules.
#[test]
fn private_chatgpt_wire_settings_are_closed_and_model_specific() {
    for bytes in [
        br#"{"kind":"responses"}"#.as_slice(),
        br#"{"kind":"chat_completions","tags":["shell:chatgpt"]}"#,
        br#"{"responses_lite_compatibility":true}"#,
        br#"{"kind":"chatgpt","responses_lite_compatibility":"true"}"#,
        br#"{"kind":"chatgpt","responses_lite_compatibility":null}"#,
    ] {
        assert!(ChatgptResponsesSettings::from_private_profile(bytes).is_none());
    }
    let standard = ChatgptResponsesSettings::from_private_profile(
        br#"{"kind":"chatgpt","credential":"irrelevant-to-wire-selection"}"#,
    )
    .expect("private profile");
    assert!(!standard.uses_lite("gpt-5.6-luna"));
    let lite = ChatgptResponsesSettings::from_private_profile(
        br#"{"kind":"chatgpt","responses_lite_compatibility":true}"#,
    )
    .expect("private Lite profile");
    for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
        assert!(lite.uses_lite(model));
    }
    for model in ["gpt-6-astra", "gpt-5.6-luna-other", "unknown"] {
        assert!(!lite.uses_lite(model));
    }
}
