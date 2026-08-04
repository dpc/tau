use super::*;

fn provider() -> ProviderName {
    ProviderName::new("deepseek")
}

fn parse(value: serde_json::Value) -> Result<ProviderCredentialReference, String> {
    parse_provider_credential_reference(&provider(), value.as_object().expect("settings object"))
        .map_err(|error| error.to_string())
}

/// Proves the shared parser accepts only canonical direct and named API-key
/// references and preserves the selected source exactly.
#[test]
fn parses_closed_api_key_references() {
    let direct = serde_json::json!({
        "credential": {
            "kind": "api_key",
            "secret_path": "providers/deepseek/api-key.json"
        }
    });
    assert_eq!(parse(direct).expect("direct").named_source(), None);

    let named = serde_json::json!({
        "credential": {
            "kind": "api_key",
            "secret_path": "providers/deepseek/api-key.json",
            "source": {"kind": "named_secret", "name": "deepseek_api_key"}
        }
    });
    assert_eq!(
        parse(named).expect("named").named_source(),
        Some("deepseek_api_key")
    );
}

/// Proves malformed, path-confused, OAuth-bound, whitespace, and
/// unknown-field references fail closed for every consumer.
#[test]
fn rejects_noncanonical_references() {
    for invalid in [
        serde_json::json!({}),
        serde_json::json!({"credential": {"kind": "api_key"}}),
        serde_json::json!({"credential": {
            "kind": "api_key",
            "secret_path": "providers/other/api-key.json"
        }}),
        serde_json::json!({"credential": {
            "kind": "oauth",
            "secret_path": "providers/deepseek/oauth.json",
            "source": {"kind": "named_secret", "name": "oauth_key"}
        }}),
        serde_json::json!({"credential": {
            "kind": "api_key",
            "secret_path": "providers/deepseek/api-key.json",
            "source": {"kind": "named_secret", "name": " "}
        }}),
        serde_json::json!({"credential": {
            "kind": "api_key",
            "secret_path": "providers/deepseek/api-key.json",
            "source": {"kind": "named_secret", "name": "key", "extra": true}
        }}),
        serde_json::json!({"credential": {
            "kind": "api_key",
            "secret_path": "providers/deepseek/api-key.json",
            "extra": true
        }}),
        serde_json::json!({
            "api_key_secret": "legacy",
            "credential": {
                "kind": "api_key",
                "secret_path": "providers/deepseek/api-key.json"
            }
        }),
    ] {
        assert!(parse(invalid).is_err());
    }
}

/// Proves setup's serializer emits the exact schema accepted by the shared
/// parser, preventing writer/reader drift.
#[test]
fn serialized_reference_round_trips_through_shared_parser() {
    for source in [None, Some("deepseek_api_key")] {
        let credential =
            ProviderCredentialReference::new(&provider(), ProviderCredentialSlot::ApiKey, source)
                .expect("valid reference")
                .to_value();
        let parsed = parse(serde_json::json!({"credential": credential})).expect("round trip");
        assert_eq!(parsed.named_source(), source);
        assert_eq!(
            parsed.path(),
            &ProviderCredentialSlot::ApiKey.path(&provider())
        );
    }
}

/// Proves the lifecycle lock never follows a symlinked provider-settings root
/// or configured-instance directory.
#[cfg(unix)]
#[test]
fn instance_lock_rejects_symlink_components() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).expect("outside");
    symlink(&outside, temp.path().join("provider-settings")).expect("root symlink");
    assert!(ProviderSettingsInstanceLock::acquire_existing(temp.path(), "provider-work").is_err());

    std::fs::remove_file(temp.path().join("provider-settings")).expect("remove symlink");
    std::fs::create_dir(temp.path().join("provider-settings")).expect("settings root");
    symlink(
        &outside,
        temp.path().join("provider-settings/provider-work"),
    )
    .expect("instance symlink");
    assert!(ProviderSettingsInstanceLock::acquire_existing(temp.path(), "provider-work").is_err());
}
