use std::fs::{File, Permissions};
use std::path::PathBuf;

use super::*;

fn target(extension: &str, provider: &str) -> TestingProvider {
    TestingProvider {
        extension: tau_proto::ExtensionName::parse(extension).expect("extension"),
        provider: tau_proto::ProviderName::new(provider),
    }
}

/// Proves an absent testing file grants no provider access.
#[test]
fn missing_testing_config_keeps_provider_access_disabled() {
    let access = provider_access_from_settings(None, PathBuf::from("/tmp/scratch/state"), None);
    assert!(access.is_missing_config());
    assert!(!access.provider_extension_enabled());
}

/// Proves one allowlisted pair copies only that instance and provider.
#[test]
fn provider_allowlist_copies_exact_instance_registration() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("real");
    let scratch = temp.path().join("scratch");
    let allowed = target("provider-builtin", "chatgpt");
    let denied = target("provider-builtin", "openrouter");
    for entry in [&allowed, &denied] {
        let settings = extension_provider_settings_dir_of(&source, entry.extension.as_str())
            .expect("settings root")
            .join(format!("{}.json", entry.provider));
        std::fs::create_dir_all(settings.parent().expect("parent")).expect("settings dir");
        std::fs::write(&settings, format!("settings:{}", entry.provider)).expect("settings");
        let secrets = extension_secret_dir_of(&source, entry.extension.as_str())
            .expect("secret root")
            .join("providers")
            .join(entry.provider.as_str());
        std::fs::create_dir_all(&secrets).expect("secret dir");
        std::fs::write(
            secrets.join("oauth.json"),
            format!("secret:{}", entry.provider),
        )
        .expect("secret");
    }
    let access = provider_access_from_settings(
        Some(source),
        scratch.clone(),
        Some(TestingSettings {
            testing_providers: vec![allowed.clone()],
        }),
    );

    access.copy_allowed_profiles().expect("copy registration");

    let copied_settings = extension_provider_settings_dir_of(&scratch, allowed.extension.as_str())
        .expect("settings")
        .join("chatgpt.json");
    let copied_secret = extension_secret_dir_of(&scratch, allowed.extension.as_str())
        .expect("secrets")
        .join("providers/chatgpt/oauth.json");
    assert_eq!(
        std::fs::read_to_string(copied_settings).expect("read"),
        "settings:chatgpt"
    );
    assert_eq!(
        std::fs::read_to_string(copied_secret).expect("read"),
        "secret:chatgpt"
    );
    assert!(
        !extension_provider_settings_dir_of(&scratch, denied.extension.as_str())
            .expect("settings")
            .join("openrouter.json")
            .exists()
    );
}

/// Proves a Home Manager-style config leaf symlink may target a read-only
/// regular deployment file outside the canonical config instance root.
#[cfg(unix)]
#[test]
fn provider_allowlist_copies_external_config_profile_symlink() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let temp = tempfile::tempdir().expect("tempdir");
    let config = temp.path().join("config");
    let state = temp.path().join("state");
    let scratch = temp.path().join("scratch");
    let allowed = target("provider-builtin", "chatgpt");
    let deployment = temp.path().join("nix-store-chatgpt.json");
    std::fs::write(&deployment, "deployed-settings").expect("deployment");
    std::fs::set_permissions(&deployment, Permissions::from_mode(0o444))
        .expect("read-only deployment");
    let profile = extension_provider_config_dir_of(&config, allowed.extension.as_str())
        .expect("config root")
        .join("chatgpt.json");
    std::fs::create_dir_all(profile.parent().expect("profile parent")).expect("config instance");
    symlink(&deployment, profile).expect("profile symlink");
    let secrets = extension_secret_dir_of(&state, allowed.extension.as_str())
        .expect("secret root")
        .join("providers/chatgpt");
    std::fs::create_dir_all(&secrets).expect("secret directory");
    std::fs::write(secrets.join("oauth.json"), "host-secret").expect("secret");
    let access = provider_access_from_dirs_and_settings(
        Some(config),
        Some(state),
        scratch.clone(),
        Some(TestingSettings {
            testing_providers: vec![allowed],
        }),
    );

    access.copy_allowed_profiles().expect("copy registration");

    assert_eq!(
        std::fs::read_to_string(scratch.join("providers/provider-builtin/chatgpt.json"))
            .expect("copied profile"),
        "deployed-settings"
    );
    assert_eq!(
        std::fs::read_to_string(
            scratch.join("secrets/ext/provider-builtin/providers/chatgpt/oauth.json")
        )
        .expect("copied secret"),
        "host-secret"
    );
}

/// Proves tmux rejects an oversized external config profile before copying it
/// into scratch state.
#[cfg(unix)]
#[test]
fn provider_allowlist_rejects_oversized_external_config_profile() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let config = temp.path().join("config");
    let state = temp.path().join("state");
    let scratch = temp.path().join("scratch");
    let allowed = target("provider-builtin", "chatgpt");
    let deployment = temp.path().join("oversized.json");
    let file = File::create(&deployment).expect("deployment");
    file.set_len(tau_config::provider_settings::MAX_PROVIDER_PROFILE_FILE_BYTES + 1)
        .expect("oversized deployment");
    let profile = extension_provider_config_dir_of(&config, allowed.extension.as_str())
        .expect("config root")
        .join("chatgpt.json");
    std::fs::create_dir_all(profile.parent().expect("profile parent")).expect("config instance");
    symlink(deployment, profile).expect("profile symlink");
    let access = provider_access_from_dirs_and_settings(
        Some(config),
        Some(state),
        scratch.clone(),
        Some(TestingSettings {
            testing_providers: vec![allowed],
        }),
    );

    let error = access
        .copy_allowed_profiles()
        .expect_err("oversized profile");

    assert!(error.to_string().contains("exceeds"));
    assert!(!scratch.join(PROVIDER_SETTINGS_DIR).exists());
}

/// Proves tmux bounds the explicit profile allowlist before touching host
/// provider state.
#[test]
fn provider_allowlist_rejects_too_many_profiles() {
    let temp = tempfile::tempdir().expect("tempdir");
    let profiles = (0..=tau_config::provider_settings::MAX_PROVIDER_PROFILE_FILES)
        .map(|index| target("provider-builtin", &format!("p-{index}")))
        .collect();
    let access = provider_access_from_dirs_and_settings(
        None,
        None,
        temp.path().join("scratch"),
        Some(TestingSettings {
            testing_providers: profiles,
        }),
    );

    let error = access
        .copy_allowed_profiles()
        .expect_err("too many profiles");

    assert!(error.to_string().contains("allowlist for instance"));
    assert!(error.to_string().contains("exceeds"));
}

/// Proves tmux applies the aggregate profile byte budget independently to each
/// allowlisted provider extension instance.
#[test]
fn provider_allowlist_rejects_per_instance_aggregate_profile_bytes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config = temp.path().join("config");
    let state = temp.path().join("state");
    let scratch = temp.path().join("scratch");
    let mut profiles = Vec::new();
    for index in 0..16 {
        let entry = target("provider-builtin", &format!("p-{index}"));
        let profile = extension_provider_config_dir_of(&config, entry.extension.as_str())
            .expect("config root")
            .join(format!("{}.json", entry.provider));
        std::fs::create_dir_all(profile.parent().expect("profile parent"))
            .expect("config instance");
        let file = File::create(profile).expect("profile");
        file.set_len(tau_config::provider_settings::MAX_PROVIDER_PROFILE_FILE_BYTES)
            .expect("bounded profile");
        let secrets = extension_secret_dir_of(&state, entry.extension.as_str())
            .expect("secret root")
            .join("providers")
            .join(entry.provider.as_str());
        std::fs::create_dir_all(secrets).expect("secret directory");
        profiles.push(entry);
    }
    let access = provider_access_from_dirs_and_settings(
        Some(config),
        Some(state),
        scratch.clone(),
        Some(TestingSettings {
            testing_providers: profiles,
        }),
    );

    let error = access.copy_allowed_profiles().expect_err("aggregate bound");

    assert!(error.to_string().contains("snapshot for instance"));
    assert!(error.to_string().contains("exceeds"));
    assert!(!scratch.join(PROVIDER_SETTINGS_DIR).exists());
}

/// Proves an explicit empty allowlist narrows reusable scratch state to none.
#[test]
fn empty_allowlist_removes_stale_settings_and_secrets() {
    let temp = tempfile::tempdir().expect("tempdir");
    let scratch = temp.path().join("scratch");
    let settings = scratch.join(PROVIDER_SETTINGS_DIR).join("stale/file.json");
    let secret = scratch
        .join(EXTENSION_SECRETS_DIR)
        .join("stale/providers/p/oauth.json");
    std::fs::create_dir_all(settings.parent().expect("parent")).expect("settings dir");
    std::fs::create_dir_all(secret.parent().expect("parent")).expect("secret dir");
    std::fs::write(settings, "stale").expect("settings");
    std::fs::write(secret, "stale").expect("secret");
    let access = provider_access_from_settings(
        None,
        scratch.clone(),
        Some(TestingSettings {
            testing_providers: Vec::new(),
        }),
    );

    access.copy_allowed_profiles().expect("reconcile");

    assert!(!scratch.join(PROVIDER_SETTINGS_DIR).exists());
    assert!(!scratch.join(EXTENSION_SECRETS_DIR).exists());
}

/// Proves one bad allowlisted registration removes material copied for earlier
/// pairs, preventing reusable scratch state from retaining a partial grant.
#[test]
fn partial_copy_failure_reconciles_both_scratch_trees() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("real");
    let scratch = temp.path().join("scratch");
    let valid = target("provider-builtin", "chatgpt");
    let missing = target("provider-work", "missing");
    let settings = extension_provider_settings_dir_of(&source, valid.extension.as_str())
        .expect("settings root")
        .join("chatgpt.json");
    std::fs::create_dir_all(settings.parent().expect("parent")).expect("settings dir");
    std::fs::write(settings, "settings").expect("settings");
    let secrets = extension_secret_dir_of(&source, valid.extension.as_str())
        .expect("secret root")
        .join("providers/chatgpt");
    std::fs::create_dir_all(&secrets).expect("secret dir");
    std::fs::write(secrets.join("oauth.json"), "secret").expect("secret");
    let access = provider_access_from_settings(
        Some(source),
        scratch.clone(),
        Some(TestingSettings {
            testing_providers: vec![valid, missing],
        }),
    );

    access
        .copy_allowed_profiles()
        .expect_err("missing registration must fail");

    assert!(!scratch.join(PROVIDER_SETTINGS_DIR).exists());
    assert!(!scratch.join(EXTENSION_SECRETS_DIR).exists());
}

/// Proves a credential leaf symlink cannot redirect copying outside state.
#[cfg(unix)]
#[test]
fn source_secret_symlink_fails_closed() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("real");
    let scratch = temp.path().join("scratch");
    let entry = target("provider-builtin", "chatgpt");
    let settings = extension_provider_settings_dir_of(&source, entry.extension.as_str())
        .expect("settings")
        .join("chatgpt.json");
    std::fs::create_dir_all(settings.parent().expect("parent")).expect("dir");
    std::fs::write(settings, "settings").expect("settings");
    let credential_dir = extension_secret_dir_of(&source, entry.extension.as_str())
        .expect("secrets")
        .join("providers/chatgpt");
    std::fs::create_dir_all(&credential_dir).expect("dir");
    let outside = temp.path().join("outside");
    std::fs::write(&outside, "secret").expect("outside");
    symlink(outside, credential_dir.join("oauth.json")).expect("symlink");
    let access = provider_access_from_settings(
        Some(source),
        scratch,
        Some(TestingSettings {
            testing_providers: vec![entry],
        }),
    );

    assert!(access.copy_allowed_profiles().is_err());
}

/// Proves copying rejects a symlinked source ancestor rather than relying only
/// on `O_NOFOLLOW` at the final credential file.
#[cfg(unix)]
#[test]
fn source_settings_ancestor_symlink_fails_closed() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("real");
    let scratch = temp.path().join("scratch");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(outside.join("provider-builtin")).expect("outside");
    std::fs::write(
        outside.join("provider-builtin/chatgpt.json"),
        "outside-settings",
    )
    .expect("outside settings");
    std::fs::create_dir_all(&source).expect("source");
    symlink(&outside, source.join(PROVIDER_SETTINGS_DIR)).expect("settings ancestor symlink");
    let access = provider_access_from_settings(
        Some(source),
        scratch.clone(),
        Some(TestingSettings {
            testing_providers: vec![target("provider-builtin", "chatgpt")],
        }),
    );

    access
        .copy_allowed_profiles()
        .expect_err("ancestor symlink must fail");

    assert!(!scratch.join(PROVIDER_SETTINGS_DIR).exists());
    assert!(!scratch.join(EXTENSION_SECRETS_DIR).exists());
}
