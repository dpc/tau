use std::process::Command;

use super::*;

fn provider() -> ProviderName {
    ProviderName::new("deepseek")
}

fn identity() -> ProviderCredentialIdentity {
    ProviderCredentialIdentity::parse("0123456789abcdef0123456789abcdef").expect("identity")
}

fn parse(value: serde_json::Value) -> Result<ProviderCredentialReference, String> {
    parse_provider_credential_reference(&provider(), value.as_object().expect("settings object"))
        .map_err(|error| error.to_string())
}

/// Proves keyless authentication requires the exact explicit marker while a
/// missing marker and keyless objects carrying Secret authority remain invalid.
#[test]
fn parses_only_explicit_closed_keyless_credentials() {
    let keyless = serde_json::json!({"credential": {"kind": "none"}});
    assert_eq!(
        parse_provider_credential(
            &provider(),
            keyless.as_object().expect("keyless settings object")
        )
        .expect("explicit keyless credential"),
        ProviderCredential::Keyless
    );
    for invalid in [
        serde_json::json!({}),
        serde_json::json!({"credential": {"kind": "none", "identity": "0123456789abcdef0123456789abcdef"}}),
        serde_json::json!({"credential": {"kind": "none", "source": {"kind": "named_secret", "name": "key"}}}),
    ] {
        assert!(
            parse_provider_credential(
                &provider(),
                invalid.as_object().expect("invalid settings object")
            )
            .is_err()
        );
    }
}

/// Proves the shared parser accepts only canonical direct and named API-key
/// references and preserves the selected source exactly.
#[test]
fn parses_closed_api_key_references() {
    let direct = serde_json::json!({
        "credential": {
            "kind": "api_key",
            "identity": "0123456789abcdef0123456789abcdef"
        }
    });
    assert_eq!(parse(direct).expect("direct").named_source(), None);

    let named = serde_json::json!({
        "credential": {
            "kind": "api_key",
            "identity": "0123456789abcdef0123456789abcdef",
            "source": {"kind": "named_secret", "name": "deepseek_api_key"}
        }
    });
    assert_eq!(
        parse(named).expect("named").named_source(),
        Some("deepseek_api_key")
    );
}

/// Proves malformed, identity-confused, OAuth-bound, whitespace, and
/// unknown-field references fail closed for every consumer.
#[test]
fn rejects_noncanonical_references() {
    for invalid in [
        serde_json::json!({}),
        serde_json::json!({"credential": {"kind": "api_key"}}),
        serde_json::json!({"credential": {
            "kind": "api_key",
            "identity": "not-an-identity"
        }}),
        serde_json::json!({"credential": {
            "kind": "oauth",
            "identity": "0123456789abcdef0123456789abcdef",
            "source": {"kind": "named_secret", "name": "oauth_key"}
        }}),
        serde_json::json!({"credential": {
            "kind": "api_key",
            "identity": "0123456789abcdef0123456789abcdef",
            "source": {"kind": "named_secret", "name": " "}
        }}),
        serde_json::json!({"credential": {
            "kind": "api_key",
            "identity": "0123456789abcdef0123456789abcdef",
            "source": {"kind": "named_secret", "name": "key", "extra": true}
        }}),
        serde_json::json!({"credential": {
            "kind": "api_key",
            "identity": "0123456789abcdef0123456789abcdef",
            "extra": true
        }}),
        serde_json::json!({
            "api_key_secret": "legacy",
            "credential": {
                "kind": "api_key",
                "identity": "0123456789abcdef0123456789abcdef"
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
            ProviderCredentialReference::new(identity(), ProviderCredentialSlot::ApiKey, source)
                .expect("valid reference")
                .to_value();
        let parsed = parse(serde_json::json!({"credential": credential})).expect("round trip");
        assert_eq!(parsed.named_source(), source);
        assert_eq!(
            parsed.path(),
            &ProviderCredentialSlot::ApiKey.path(&identity())
        );
    }
}

/// Proves the lifecycle lock never follows a symlinked providers root
/// or configured-instance directory.
#[cfg(unix)]
#[test]
fn instance_lock_rejects_symlink_components() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).expect("outside");
    symlink(&outside, temp.path().join("providers")).expect("root symlink");
    assert!(ProviderSettingsInstanceLock::acquire_existing(temp.path(), "provider-work").is_err());

    std::fs::remove_file(temp.path().join("providers")).expect("remove symlink");
    std::fs::create_dir(temp.path().join("providers")).expect("settings root");
    symlink(&outside, temp.path().join("providers/provider-work")).expect("instance symlink");
    assert!(ProviderSettingsInstanceLock::acquire_existing(temp.path(), "provider-work").is_err());
}

/// Proves persistent config-only startup can create one private lifecycle
/// authority without writing a profile or touching config.
#[test]
fn persistent_instance_lock_creates_private_state_authority() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().expect("tempdir");
    let lock = ProviderSettingsInstanceLock::acquire_or_create(temp.path(), "provider-work")
        .expect("private lock");

    assert_eq!(lock.root(), temp.path().join("providers/provider-work"));
    assert_eq!(
        std::fs::metadata(lock.root())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(
        std::fs::read_dir(lock.root())
            .expect("lock root")
            .next()
            .is_none()
    );
}

/// Proves config profile reads follow a regular leaf symlink while mutable
/// state reads reject the same indirection.
#[cfg(unix)]
#[test]
fn bounded_profile_reader_distinguishes_config_and_state_symlinks() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("target.json");
    let link = temp.path().join("profile.json");
    std::fs::write(&target, b"portable").expect("target");
    symlink(target, &link).expect("symlink");

    assert_eq!(
        read_provider_profile(&link, ProviderProfileLeafSymlinkPolicy::Follow)
            .expect("config profile"),
        b"portable"
    );
    assert!(read_provider_profile(&link, ProviderProfileLeafSymlinkPolicy::Reject).is_err());
}

/// Proves a raced config target that becomes a FIFO cannot block profile
/// discovery: Unix opens it nonblocking and rejects the opened descriptor.
#[cfg(unix)]
#[test]
fn bounded_profile_reader_rejects_fifo_without_blocking() {
    let temp = tempfile::tempdir().expect("tempdir");
    let fifo = temp.path().join("profile.json");
    let output = Command::new("mkfifo")
        .arg(&fifo)
        .output()
        .expect("run mkfifo");
    assert!(
        output.status.success(),
        "mkfifo failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let error = read_provider_profile(&fifo, ProviderProfileLeafSymlinkPolicy::Follow)
        .expect_err("FIFO rejected");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("regular file"));
}
