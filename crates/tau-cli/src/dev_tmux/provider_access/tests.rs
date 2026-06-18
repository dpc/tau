use std::path::PathBuf;
#[cfg(unix)]
use std::process::Command;

use super::*;

/// Ensures a missing `testing.yaml` produces the safe local-only provider
/// access plan that neither copies provider files nor enables provider-builtin.
#[test]
fn missing_testing_config_keeps_provider_access_disabled() {
    let access = provider_access_from_settings(None, PathBuf::from("/tmp/scratch/state"), None);

    assert!(access.is_missing_config());
    assert!(!access.provider_extension_enabled());
    assert!(missing_testing_config_warning().contains("tau-self-knowledge-e2e-testing"));
}

/// Ensures an explicit allowlist is exact: only named auth.d JSON provider
/// profiles are copied into scratch state, while other profiles and lock files
/// remain unavailable to the tmux child.
#[test]
fn provider_allowlist_copies_only_exact_auth_json_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source_state = temp.path().join("real-state");
    let scratch_state = temp.path().join("scratch-state");
    let source_auth = source_state.join(PROVIDER_AUTH_DIR);
    std::fs::create_dir_all(&source_auth).expect("mkdir source auth");
    std::fs::write(source_auth.join("chatgpt.json"), r#"{"kind":"chatgpt"}"#).expect("chatgpt");
    std::fs::write(
        source_auth.join("openrouter.json"),
        r#"{"kind":"openrouter"}"#,
    )
    .expect("openrouter");
    std::fs::write(source_auth.join("chatgpt.lock"), "lock").expect("lock");
    let access = provider_access_from_settings(
        Some(source_state),
        scratch_state.clone(),
        Some(tau_config::settings::TestingSettings {
            testing_providers: vec![tau_proto::ProviderName::new("chatgpt")],
        }),
    );

    access
        .copy_allowed_profiles()
        .expect("copy provider profile");

    assert_eq!(
        std::fs::read_to_string(scratch_state.join(PROVIDER_AUTH_DIR).join("chatgpt.json"))
            .expect("copied chatgpt"),
        r#"{"kind":"chatgpt"}"#
    );
    assert!(
        !scratch_state
            .join(PROVIDER_AUTH_DIR)
            .join("openrouter.json")
            .exists()
    );
    assert!(
        !scratch_state
            .join(PROVIDER_AUTH_DIR)
            .join("chatgpt.lock")
            .exists()
    );
}

/// Ensures a reused scratch root is reconciled to the current missing-config
/// local-only policy by deleting stale provider JSON files from scratch state.
#[test]
fn missing_testing_config_removes_stale_scratch_provider_profiles() {
    let temp = tempfile::tempdir().expect("tempdir");
    let scratch_state = temp.path().join("scratch-state");
    let scratch_auth = scratch_state.join(PROVIDER_AUTH_DIR);
    std::fs::create_dir_all(&scratch_auth).expect("mkdir scratch auth");
    std::fs::write(scratch_auth.join("chatgpt.json"), "secret").expect("stale profile");
    let access = provider_access_from_settings(None, scratch_state.clone(), None);

    access
        .copy_allowed_profiles()
        .expect("stale provider profiles reconciled");

    assert!(!scratch_state.join(PROVIDER_AUTH_DIR).exists());
}

/// Ensures an explicitly empty `testing_providers` list behaves like a
/// deliberate local-only configuration: provider-builtin stays disabled, the
/// empty-config warning is used, and stale scratch profiles are removed.
#[test]
fn empty_testing_provider_allowlist_disables_access_and_removes_stale_profiles() {
    let temp = tempfile::tempdir().expect("tempdir");
    let scratch_state = temp.path().join("scratch-state");
    let scratch_auth = scratch_state.join(PROVIDER_AUTH_DIR);
    std::fs::create_dir_all(&scratch_auth).expect("mkdir scratch auth");
    std::fs::write(scratch_auth.join("chatgpt.json"), "secret").expect("stale profile");
    let access = provider_access_from_settings(
        None,
        scratch_state.clone(),
        Some(tau_config::settings::TestingSettings {
            testing_providers: Vec::new(),
        }),
    );

    assert!(!access.is_missing_config());
    assert!(!access.provider_extension_enabled());
    assert!(empty_testing_provider_warning().contains("no testing_providers"));
    access
        .copy_allowed_profiles()
        .expect("empty allowlist reconciles stale provider profiles");

    assert!(!scratch_state.join(PROVIDER_AUTH_DIR).exists());
}

/// Ensures a reused scratch root is narrowed to the current exact allowlist
/// before fresh provider profiles are copied into it.
#[test]
fn provider_allowlist_removes_unallowed_stale_scratch_profiles() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source_state = temp.path().join("real-state");
    let scratch_state = temp.path().join("scratch-state");
    let source_auth = source_state.join(PROVIDER_AUTH_DIR);
    let scratch_auth = scratch_state.join(PROVIDER_AUTH_DIR);
    std::fs::create_dir_all(&source_auth).expect("mkdir source auth");
    std::fs::create_dir_all(&scratch_auth).expect("mkdir scratch auth");
    std::fs::write(source_auth.join("chatgpt.json"), "fresh").expect("source profile");
    std::fs::write(scratch_auth.join("chatgpt.json"), "stale").expect("stale allowed");
    std::fs::write(scratch_auth.join("openrouter.json"), "stale").expect("stale unallowed");
    let access = provider_access_from_settings(
        Some(source_state),
        scratch_state.clone(),
        Some(tau_config::settings::TestingSettings {
            testing_providers: vec![tau_proto::ProviderName::new("chatgpt")],
        }),
    );

    access
        .copy_allowed_profiles()
        .expect("provider profiles reconciled");

    assert_eq!(
        std::fs::read_to_string(scratch_auth.join("chatgpt.json")).expect("allowed refreshed"),
        "fresh"
    );
    assert!(!scratch_auth.join("openrouter.json").exists());
}

/// Ensures a failed multi-provider copy cleans up credentials already copied
/// for the current allowlist instead of leaving partial scratch secrets behind.
#[test]
fn provider_allowlist_copy_failure_cleans_up_partial_profiles() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source_state = temp.path().join("real-state");
    let scratch_state = temp.path().join("scratch-state");
    let source_auth = source_state.join(PROVIDER_AUTH_DIR);
    let scratch_auth = scratch_state.join(PROVIDER_AUTH_DIR);
    std::fs::create_dir_all(&source_auth).expect("mkdir source auth");
    std::fs::write(source_auth.join("chatgpt.json"), "secret").expect("source profile");
    let access = provider_access_from_settings(
        Some(source_state),
        scratch_state,
        Some(tau_config::settings::TestingSettings {
            testing_providers: vec![
                tau_proto::ProviderName::new("chatgpt"),
                tau_proto::ProviderName::new("openrouter"),
            ],
        }),
    );

    let error = access
        .copy_allowed_profiles()
        .expect_err("missing second provider aborts copy");

    assert!(error.to_string().contains("openrouter"));
    assert!(!scratch_auth.join("chatgpt.json").exists());
}

/// Ensures a pre-existing regular destination that is hard-linked outside the
/// scratch tree is unlinked before copying instead of being truncated through
/// the outside link.
#[cfg(unix)]
#[test]
fn provider_allowlist_does_not_truncate_hardlinked_destination() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source_state = temp.path().join("real-state");
    let scratch_state = temp.path().join("scratch-state");
    let source_auth = source_state.join(PROVIDER_AUTH_DIR);
    let scratch_auth = scratch_state.join(PROVIDER_AUTH_DIR);
    std::fs::create_dir_all(&source_auth).expect("mkdir source auth");
    std::fs::create_dir_all(&scratch_auth).expect("mkdir scratch auth");
    std::fs::write(source_auth.join("chatgpt.json"), "fresh").expect("source profile");
    let outside = temp.path().join("outside.json");
    std::fs::write(&outside, "outside-secret").expect("outside");
    std::fs::hard_link(&outside, scratch_auth.join("chatgpt.json")).expect("hard link");
    let access = provider_access_from_settings(
        Some(source_state),
        scratch_state,
        Some(tau_config::settings::TestingSettings {
            testing_providers: vec![tau_proto::ProviderName::new("chatgpt")],
        }),
    );

    access
        .copy_allowed_profiles()
        .expect("hardlinked destination unlinked and replaced");

    assert_eq!(
        std::fs::read_to_string(outside).expect("outside unchanged"),
        "outside-secret"
    );
    assert_eq!(
        std::fs::read_to_string(scratch_auth.join("chatgpt.json")).expect("scratch profile"),
        "fresh"
    );
}

/// Ensures a pre-existing FIFO destination at an allowed provider JSON path is
/// removed during scratch reconciliation, so copying never opens it for writing
/// and cannot block or stream credentials into a special file.
#[cfg(unix)]
#[test]
fn provider_allowlist_replaces_fifo_destination_without_blocking() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source_state = temp.path().join("real-state");
    let scratch_state = temp.path().join("scratch-state");
    let source_auth = source_state.join(PROVIDER_AUTH_DIR);
    let scratch_auth = scratch_state.join(PROVIDER_AUTH_DIR);
    std::fs::create_dir_all(&source_auth).expect("mkdir source auth");
    std::fs::create_dir_all(&scratch_auth).expect("mkdir scratch auth");
    std::fs::write(source_auth.join("chatgpt.json"), "fresh").expect("source profile");
    let fifo = scratch_auth.join("chatgpt.json");
    let output = Command::new("mkfifo")
        .arg(&fifo)
        .output()
        .expect("run mkfifo");
    assert!(
        output.status.success(),
        "mkfifo failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let access = provider_access_from_settings(
        Some(source_state),
        scratch_state,
        Some(tau_config::settings::TestingSettings {
            testing_providers: vec![tau_proto::ProviderName::new("chatgpt")],
        }),
    );

    access
        .copy_allowed_profiles()
        .expect("fifo destination replaced");

    assert_eq!(
        std::fs::read_to_string(scratch_auth.join("chatgpt.json")).expect("scratch profile"),
        "fresh"
    );
}

/// Ensures provider copying refuses symlinked source profiles instead of
/// following them to arbitrary user files outside the provider auth directory.
#[cfg(unix)]
#[test]
fn provider_profile_copy_rejects_source_symlink() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source_auth = temp.path().join("real-state").join(PROVIDER_AUTH_DIR);
    let scratch_auth = temp.path().join("scratch-state").join(PROVIDER_AUTH_DIR);
    std::fs::create_dir_all(&source_auth).expect("mkdir source auth");
    std::fs::create_dir_all(&scratch_auth).expect("mkdir scratch auth");
    let outside = temp.path().join("outside.json");
    std::fs::write(&outside, "secret").expect("outside");
    std::os::unix::fs::symlink(&outside, source_auth.join("chatgpt.json")).expect("symlink");

    let error = copy_provider_profile(
        &source_auth,
        &scratch_auth,
        &tau_proto::ProviderName::new("chatgpt"),
    )
    .expect_err("symlink source refused");

    assert!(error.to_string().contains("refusing symlink path"));
}

/// Ensures provider copying refuses a FIFO source profile without blocking,
/// preventing special files in real provider storage from being treated as
/// credential JSON.
#[cfg(unix)]
#[test]
fn provider_profile_copy_rejects_source_fifo_without_blocking() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source_auth = temp.path().join("real-state").join(PROVIDER_AUTH_DIR);
    let scratch_auth = temp.path().join("scratch-state").join(PROVIDER_AUTH_DIR);
    std::fs::create_dir_all(&source_auth).expect("mkdir source auth");
    std::fs::create_dir_all(&scratch_auth).expect("mkdir scratch auth");
    let fifo = source_auth.join("chatgpt.json");
    let output = Command::new("mkfifo")
        .arg(&fifo)
        .output()
        .expect("run mkfifo");
    assert!(
        output.status.success(),
        "mkfifo failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let error = copy_provider_profile(
        &source_auth,
        &scratch_auth,
        &tau_proto::ProviderName::new("chatgpt"),
    )
    .expect_err("fifo source refused");

    assert!(error.to_string().contains("non-regular provider profile"));
    assert!(!scratch_auth.join("chatgpt.json").exists());
}

/// Ensures provider copying refuses symlinked scratch destinations so a reused
/// scratch tree cannot redirect copied credentials into arbitrary files.
#[cfg(unix)]
#[test]
fn provider_profile_copy_rejects_destination_symlink() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source_auth = temp.path().join("real-state").join(PROVIDER_AUTH_DIR);
    let scratch_auth = temp.path().join("scratch-state").join(PROVIDER_AUTH_DIR);
    std::fs::create_dir_all(&source_auth).expect("mkdir source auth");
    std::fs::create_dir_all(&scratch_auth).expect("mkdir scratch auth");
    std::fs::write(source_auth.join("chatgpt.json"), "secret").expect("source");
    let outside = temp.path().join("outside.json");
    std::fs::write(&outside, "unchanged").expect("outside");
    std::os::unix::fs::symlink(&outside, scratch_auth.join("chatgpt.json")).expect("symlink");

    let error = copy_provider_profile(
        &source_auth,
        &scratch_auth,
        &tau_proto::ProviderName::new("chatgpt"),
    )
    .expect_err("symlink destination refused");

    assert!(error.to_string().contains("refusing symlink path"));
    assert_eq!(
        std::fs::read_to_string(outside).expect("outside unchanged"),
        "unchanged"
    );
}

/// Ensures the allowlist copier rejects a symlinked real `auth.d` directory
/// before looking up opted-in profile names, keeping provider access tied to
/// the expected Tau provider storage tree.
#[cfg(unix)]
#[test]
fn provider_allowlist_rejects_source_auth_directory_symlink() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source_state = temp.path().join("real-state");
    let outside_auth = temp.path().join("outside-auth");
    let scratch_state = temp.path().join("scratch-state");
    std::fs::create_dir_all(&source_state).expect("mkdir source state");
    std::fs::create_dir_all(&outside_auth).expect("mkdir outside auth");
    std::os::unix::fs::symlink(&outside_auth, source_state.join(PROVIDER_AUTH_DIR))
        .expect("auth dir symlink");
    let access = provider_access_from_settings(
        Some(source_state),
        scratch_state,
        Some(tau_config::settings::TestingSettings {
            testing_providers: vec![tau_proto::ProviderName::new("chatgpt")],
        }),
    );

    let error = access
        .copy_allowed_profiles()
        .expect_err("symlink auth dir refused");

    assert!(error.to_string().contains("source auth directory"));
    assert!(error.to_string().contains("refusing symlink path"));
}

/// Ensures stale scratch cleanup fails closed on symlinked entries instead of
/// following or deleting a link that could point outside the helper scratch
/// tree.
#[cfg(unix)]
#[test]
fn provider_reconcile_rejects_scratch_auth_entry_symlink() {
    let temp = tempfile::tempdir().expect("tempdir");
    let scratch_state = temp.path().join("scratch-state");
    let scratch_auth = scratch_state.join(PROVIDER_AUTH_DIR);
    std::fs::create_dir_all(&scratch_auth).expect("mkdir scratch auth");
    let outside = temp.path().join("outside.json");
    std::fs::write(&outside, "unchanged").expect("outside");
    std::os::unix::fs::symlink(&outside, scratch_auth.join("chatgpt.json")).expect("symlink");
    let access = provider_access_from_settings(None, scratch_state, None);

    let error = access
        .copy_allowed_profiles()
        .expect_err("scratch auth symlink refused");

    assert!(error.to_string().contains("refusing symlink path"));
    assert_eq!(
        std::fs::read_to_string(outside).expect("outside unchanged"),
        "unchanged"
    );
}
