use std::os::unix::fs::PermissionsExt as _;

use super::*;

fn extension() -> tau_proto::ExtensionName {
    tau_proto::ExtensionName::parse("provider-work").expect("extension")
}

fn provider() -> tau_proto::ProviderName {
    tau_proto::ProviderName::new("chatgpt")
}

fn plan() -> ProviderSetupPlan {
    ProviderSetupPlan {
        extension_instance: extension(),
        provider: provider(),
        settings: br#"{"kind":"chatgpt","credential":{"kind":"oauth","secret_path":"providers/chatgpt/oauth.json"}}"#.to_vec(),
        secret: SecretWrite {
            path: tau_proto::ExtensionDataPath::new(
                "providers/chatgpt/oauth.json".to_owned(),
            ),
            contents: SecretBytes::new(b"typed-secret".to_vec()),
        },
    }
}

/// Proves registration uses the exact scoped layout and private modes, then
/// removes both activation settings and credentials.
#[test]
fn apply_and_remove_use_scoped_private_layout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    let plan = plan();
    let settings = plan.settings.clone();
    store.apply(&plan).expect("apply");

    let settings_path = temp
        .path()
        .join("provider-settings/provider-work/chatgpt.json");
    let secret_path = temp
        .path()
        .join("secrets/ext/provider-work/providers/chatgpt/oauth.json");
    assert_eq!(std::fs::read(&settings_path).expect("settings"), settings);
    assert_eq!(
        std::fs::read(&secret_path).expect("secret"),
        b"typed-secret"
    );
    assert_eq!(
        std::fs::metadata(&settings_path)
            .expect("settings metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(&secret_path)
            .expect("secret metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    for directory in [
        temp.path().join("secrets"),
        temp.path().join("secrets/ext"),
        temp.path().join("secrets/ext/provider-work"),
        temp.path().join("secrets/ext/provider-work/providers"),
        temp.path()
            .join("secrets/ext/provider-work/providers/chatgpt"),
    ] {
        assert_eq!(
            std::fs::metadata(directory)
                .expect("private directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    assert!(store.remove(&extension(), &provider()).expect("remove"));
    assert!(!settings_path.exists());
    assert!(!secret_path.exists());
}

/// Proves secret bytes never appear through ordinary diagnostic formatting.
#[test]
fn secret_bytes_debug_redacts_payload() {
    let debug = format!("{:?}", SecretBytes::new(b"never-print-this".to_vec()));
    assert!(!debug.contains("never-print-this"));
    assert!(debug.contains("len"));
}

/// Proves a settings publication failure occurs after the secret write, leaving
/// an inactive orphan rather than an active registration without credentials.
#[cfg(unix)]
#[test]
fn apply_failure_preserves_secret_first_boundary() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).expect("outside");
    symlink(&outside, temp.path().join("provider-settings")).expect("settings symlink");
    let store = SetupStore::open_in(temp.path());

    store
        .apply(&plan())
        .expect_err("settings publication must fail");

    assert_eq!(
        std::fs::read(
            temp.path()
                .join("secrets/ext/provider-work/providers/chatgpt/oauth.json"),
        )
        .expect("orphaned secret"),
        b"typed-secret"
    );
    assert!(!outside.join("provider-work/chatgpt.json").exists());
}

/// Proves a credential-removal failure occurs after settings disappear, so a
/// stale secret cannot remain activated by its settings record.
#[cfg(unix)]
#[test]
fn remove_failure_preserves_settings_first_boundary() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    store.apply(&plan()).expect("apply");
    let provider_dir = temp
        .path()
        .join("secrets/ext/provider-work/providers/chatgpt");
    std::fs::remove_dir_all(&provider_dir).expect("remove provider directory");
    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).expect("outside");
    std::fs::write(outside.join("oauth.json"), "outside-secret").expect("outside secret");
    symlink(&outside, &provider_dir).expect("provider symlink");

    store
        .remove(&extension(), &provider())
        .expect_err("credential removal must fail");

    assert!(
        !temp
            .path()
            .join("provider-settings/provider-work/chatgpt.json")
            .exists()
    );
    assert_eq!(
        std::fs::read(outside.join("oauth.json")).expect("outside secret"),
        b"outside-secret"
    );
}
