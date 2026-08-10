use std::fs::Permissions;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Barrier, mpsc};
use std::time::Duration;

use super::*;
use crate::credential_record::ApiKeyCredential;

/// Proves config-targeted setup publishes credentials in state while keeping
/// the profile source explicit and collision-free.
#[test]
fn config_target_uses_portable_profile_and_host_local_secret() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());

    let path = store
        .apply_to(&plan(), ProfileTarget::Config)
        .expect("config setup")
        .expect("config path");
    assert_eq!(
        path,
        temp.path()
            .join("config/providers/provider-work/chatgpt.json")
    );
    assert!(
        temp.path()
            .join("secrets/ext/provider-work/providers/chatgpt/oauth.json")
            .exists()
    );
    let snapshot = store.snapshot(&extension()).expect("snapshot");
    assert_eq!(snapshot.profiles[0].source, ProfileSource::Config);
}

/// Proves an explicitly keyless portable setup writes only its profile and
/// never creates a dummy Secret tree or record.
#[test]
fn keyless_config_target_does_not_publish_secret_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    let mut keyless = plan();
    keyless.settings =
        br#"{"kind":"chat_completions","models":[{"id":"local"}],"credential":{"kind":"none"}}"#
            .to_vec();
    keyless.credential = CredentialSetup::Keyless;

    store
        .apply_to(&keyless, ProfileTarget::Config)
        .expect("keyless config setup");

    assert!(!temp.path().join("secrets/ext/provider-work").exists());
}

/// Proves inspection and credential-only login follow a Home Manager-style
/// config leaf symlink without replacing it or modifying its read-only target.
#[cfg(unix)]
#[test]
fn snapshot_and_login_preserve_external_config_profile_symlink() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    let deployment = temp.path().join("nix-store-chatgpt.json");
    std::fs::write(&deployment, b"{\"deployed\":true}").expect("deployment");
    std::fs::set_permissions(&deployment, Permissions::from_mode(0o444))
        .expect("read-only deployment");
    let profile = temp
        .path()
        .join("config/providers/provider-work/chatgpt.json");
    std::fs::create_dir_all(profile.parent().expect("profile parent")).expect("config instance");
    symlink(&deployment, &profile).expect("profile symlink");

    let snapshot = store.snapshot(&extension()).expect("snapshot");

    assert_eq!(snapshot.profiles.len(), 1);
    assert_eq!(snapshot.profiles[0].source, ProfileSource::Config);
    assert_eq!(snapshot.profiles[0].path, profile);
    assert_eq!(snapshot.profiles[0].contents, b"{\"deployed\":true}");

    let replacement = SecretWrite {
        path: ProviderCredentialSlot::OAuth.path(&provider()),
        contents: SecretBytes::new(b"host-local-secret".to_vec()),
    };
    store
        .publish_credential(
            &extension(),
            &provider(),
            ProfileSource::Config,
            b"{\"deployed\":true}",
            &replacement,
            None,
        )
        .expect("credential publication");
    assert!(
        std::fs::symlink_metadata(&profile)
            .expect("profile metadata")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read(&deployment).expect("deployed profile"),
        b"{\"deployed\":true}"
    );
}

/// Proves output mode refuses an existing state owner before replacing its
/// credential record.
#[test]
fn stdout_target_rejects_state_collision_before_secret_write() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    store.apply(&plan()).expect("state setup");
    let secret = temp
        .path()
        .join("secrets/ext/provider-work/providers/chatgpt/oauth.json");
    let before = std::fs::read(&secret).expect("credential before collision");

    let error = store
        .apply_to(
            &direct_plan_with("new", "replacement"),
            ProfileTarget::Stdout,
        )
        .expect_err("collision");

    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read(secret).expect("credential after collision"),
        before
    );
}

/// Proves removal safely infers the sole config owner and deletes credentials
/// only after profile deactivation.
#[test]
fn remove_infers_unique_config_source() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    store
        .apply_to(&plan(), ProfileTarget::Config)
        .expect("config setup");

    assert!(
        store
            .remove_from(&extension(), &provider(), None)
            .expect("remove")
    );
    assert!(
        !temp
            .path()
            .join("secrets/ext/provider-work/providers/chatgpt/oauth.json")
            .exists()
    );
}

/// Proves an explicit source mismatch cannot deactivate another source or
/// remove its credential.
#[test]
fn remove_rejects_source_mismatch_without_touching_credentials() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    store.apply(&plan()).expect("state setup");
    let secret = temp
        .path()
        .join("secrets/ext/provider-work/providers/chatgpt/oauth.json");

    let error = store
        .remove_from(&extension(), &provider(), Some(ProfileSource::Config))
        .expect_err("source mismatch");

    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(secret.exists());
}

/// Proves an invalid duplicate owner remains ambiguous and preserves both the
/// profiles and credential.
#[test]
fn remove_rejects_cross_source_ambiguity() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    store.apply(&plan()).expect("state setup");
    let config = temp
        .path()
        .join("config/providers/provider-work/chatgpt.json");
    std::fs::create_dir_all(config.parent().expect("parent")).expect("config root");
    std::fs::write(&config, b"{}").expect("duplicate config");

    let error = store
        .remove_from(&extension(), &provider(), None)
        .expect_err("ambiguous source");

    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(config.exists());
    assert!(
        temp.path()
            .join("providers/provider-work/chatgpt.json")
            .exists()
    );
    assert!(
        temp.path()
            .join("secrets/ext/provider-work/providers/chatgpt/oauth.json")
            .exists()
    );
}

/// Proves a read-only config deletion failure leaves host-local credentials
/// intact because credentials are removed only after deactivation.
#[cfg(unix)]
#[test]
fn remove_config_failure_preserves_credentials() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    store
        .apply_to(&plan(), ProfileTarget::Config)
        .expect("config setup");
    let config_root = temp.path().join("config/providers/provider-work");
    std::fs::set_permissions(&config_root, Permissions::from_mode(0o500))
        .expect("read-only config root");

    let result = store.remove_from(&extension(), &provider(), Some(ProfileSource::Config));

    std::fs::set_permissions(&config_root, Permissions::from_mode(0o700))
        .expect("restore permissions");
    assert!(result.is_err());
    assert!(
        temp.path()
            .join("secrets/ext/provider-work/providers/chatgpt/oauth.json")
            .exists()
    );
}

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
        credential: CredentialSetup::Stored {
            secret: SecretWrite {
                path: tau_proto::ExtensionDataPath::new(
                    "providers/chatgpt/oauth.json".to_owned(),
                ),
                contents: SecretBytes::new(b"typed-secret".to_vec()),
            },
            named_source: None,
        },
    }
}

fn named_plan() -> ProviderSetupPlan {
    ProviderSetupPlan {
        settings: br#"{"kind":"chat_completions","credential":{"kind":"api_key","secret_path":"providers/chatgpt/api-key.json","source":{"kind":"named_secret","name":"setup_key"}}}"#.to_vec(),
        credential: CredentialSetup::Stored {
            secret: SecretWrite {
                path: tau_proto::ExtensionDataPath::new(
                    "providers/chatgpt/api-key.json".to_owned(),
                ),
                contents: SecretBytes::new(b"placeholder".to_vec()),
            },
            named_source: Some(NamedSecretSource {
                name: "setup_key".to_owned(),
                declaration: tau_config::settings::ExtensionSecretEntry { optional: false },
            }),
        },
        ..plan()
    }
}

fn direct_plan_with(settings_marker: &str, secret: &str) -> ProviderSetupPlan {
    ProviderSetupPlan {
        extension_instance: extension(),
        provider: provider(),
        settings: format!(
            "{{\"kind\":\"chatgpt\",\"marker\":\"{settings_marker}\",\"credential\":{{\"kind\":\"oauth\",\"secret_path\":\"providers/chatgpt/oauth.json\"}}}}"
        )
        .into_bytes(),
        credential: CredentialSetup::Stored {
            secret: SecretWrite {
                path: tau_proto::ExtensionDataPath::new(
                    "providers/chatgpt/oauth.json".to_owned(),
                ),
                contents: SecretBytes::new(secret.as_bytes().to_vec()),
            },
            named_source: None,
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

    let settings_path = temp.path().join("providers/provider-work/chatgpt.json");
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

/// Proves list/status captures only credential slots belonging to the settings
/// generation and ignores orphan records.
#[test]
fn snapshot_pairs_active_settings_with_matching_credentials_only() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    store.apply(&plan()).expect("apply");
    let orphan = temp
        .path()
        .join("secrets/ext/provider-work/providers/orphan/api-key.json");
    std::fs::create_dir_all(orphan.parent().expect("orphan parent")).expect("orphan root");
    std::fs::write(&orphan, b"orphan").expect("orphan");

    let snapshot = store.snapshot(&extension()).expect("snapshot");

    assert_eq!(snapshot.profiles.len(), 1);
    assert_eq!(
        snapshot
            .credentials
            .get(&(provider(), ProviderCredentialSlot::OAuth)),
        Some(&b"typed-secret".to_vec())
    );
    assert!(
        snapshot
            .credentials
            .keys()
            .all(|(provider, _)| provider.as_str() != "orphan")
    );

    std::fs::remove_file(temp.path().join("providers/provider-work/chatgpt.json"))
        .expect("remove settings");
    let empty = store.snapshot(&extension()).expect("empty snapshot");
    assert!(empty.profiles.is_empty());
    assert!(empty.credentials.is_empty());
}

/// Proves login publishes only the host-local Secret and leaves a portable
/// profile byte-for-byte unchanged without creating a shadow state profile.
#[test]
fn publish_credential_preserves_config_profile_without_state_shadow() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    let plan = plan();
    let config_path = store
        .apply_to(&plan, ProfileTarget::Config)
        .expect("config setup")
        .expect("config path");
    let settings_before = std::fs::read(&config_path).expect("settings");
    let replacement = SecretWrite {
        path: ProviderCredentialSlot::ApiKey.path(&plan.provider),
        contents: SecretBytes::new(b"refreshed-secret".to_vec()),
    };

    store
        .publish_credential(
            &plan.extension_instance,
            &plan.provider,
            ProfileSource::Config,
            &settings_before,
            &replacement,
            None,
        )
        .expect("credential publication");

    assert_eq!(
        std::fs::read(&config_path).expect("settings after"),
        settings_before
    );
    assert!(
        !temp
            .path()
            .join("providers/provider-work/chatgpt.json")
            .exists()
    );
    assert_eq!(
        store
            .credential(
                &plan.extension_instance,
                &plan.provider,
                ProviderCredentialSlot::ApiKey
            )
            .expect("credential"),
        b"refreshed-secret"
    );
}

/// Proves credential publication refuses changed bytes, moved ownership, and a
/// cross-source collision while preserving the previously active Secret.
#[test]
fn publish_credential_rejects_every_stale_profile_identity() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    let plan = plan();
    let config_path = store
        .apply_to(&plan, ProfileTarget::Config)
        .expect("config setup")
        .expect("config path");
    let settings_before = std::fs::read(&config_path).expect("settings");
    let replacement = SecretWrite {
        path: ProviderCredentialSlot::OAuth.path(&plan.provider),
        contents: SecretBytes::new(b"new-secret".to_vec()),
    };
    let secret_before = store
        .credential(
            &plan.extension_instance,
            &plan.provider,
            ProviderCredentialSlot::OAuth,
        )
        .expect("credential");

    std::fs::write(&config_path, b"changed settings").expect("changed settings");
    for (label, expected_kind) in [
        ("changed bytes", ErrorKind::InvalidData),
        ("moved source", ErrorKind::InvalidData),
        ("cross-source collision", ErrorKind::AlreadyExists),
    ] {
        if label == "moved source" {
            std::fs::remove_file(&config_path).expect("remove config profile");
            std::fs::write(
                temp.path().join("providers/provider-work/chatgpt.json"),
                &settings_before,
            )
            .expect("state profile");
        } else if label == "cross-source collision" {
            std::fs::write(&config_path, &settings_before).expect("config duplicate");
        }
        let error = store
            .publish_credential(
                &plan.extension_instance,
                &plan.provider,
                ProfileSource::Config,
                &settings_before,
                &replacement,
                None,
            )
            .expect_err(label);
        assert_eq!(error.kind(), expected_kind, "{label}");
        assert_eq!(
            store
                .credential(
                    &plan.extension_instance,
                    &plan.provider,
                    ProviderCredentialSlot::OAuth
                )
                .expect("credential after refusal"),
            secret_before,
            "{label}"
        );
    }
}

/// Proves login materializes the current named-source value rather than its
/// placeholder and preserves the old credential when later resolution fails.
#[test]
fn publish_credential_materializes_named_source_before_replacement() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    let plan = plan();
    let config_path = store
        .apply_to(&plan, ProfileTarget::Config)
        .expect("config setup")
        .expect("config path");
    let settings = std::fs::read(config_path).expect("settings");
    std::fs::create_dir_all(temp.path().join("secrets")).expect("source root");
    let source_path = temp.path().join("secrets/setup_key.yaml");
    std::fs::write(&source_path, "resolved-value\n").expect("named source");
    let replacement = SecretWrite {
        path: ProviderCredentialSlot::ApiKey.path(&plan.provider),
        contents: SecretBytes::new(b"placeholder".to_vec()),
    };
    let source = NamedSecretSource {
        name: "setup_key".to_owned(),
        declaration: tau_config::settings::ExtensionSecretEntry { optional: false },
    };

    store
        .publish_credential(
            &plan.extension_instance,
            &plan.provider,
            ProfileSource::Config,
            &settings,
            &replacement,
            Some(&source),
        )
        .expect("named publication");
    let before = store
        .credential(
            &plan.extension_instance,
            &plan.provider,
            ProviderCredentialSlot::ApiKey,
        )
        .expect("materialized credential");
    let parsed: crate::credential_record::ApiKeyCredential =
        serde_json::from_slice(&before).expect("typed credential");
    assert_eq!(parsed.into_value(), "resolved-value");

    std::fs::remove_file(source_path).expect("remove source");
    store
        .publish_credential(
            &plan.extension_instance,
            &plan.provider,
            ProfileSource::Config,
            &settings,
            &replacement,
            Some(&source),
        )
        .expect_err("missing named source");
    assert_eq!(
        store
            .credential(
                &plan.extension_instance,
                &plan.provider,
                ProviderCredentialSlot::ApiKey,
            )
            .expect("preserved credential"),
        before
    );
}

/// Proves direct credential-only publication accepts the exact Secret file
/// limit and rejects one extra byte without replacing the accepted record.
#[test]
fn publish_credential_enforces_direct_secret_file_limit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    let plan = plan();
    let config_path = store
        .apply_to(&plan, ProfileTarget::Config)
        .expect("config setup")
        .expect("config path");
    let settings = std::fs::read(config_path).expect("settings");
    let secret_limit =
        usize::try_from(MAX_SECRET_DATA_FILE_BYTES).expect("Secret limit fits usize");
    let exact = SecretWrite {
        path: ProviderCredentialSlot::OAuth.path(&plan.provider),
        contents: SecretBytes::new(vec![b'x'; secret_limit]),
    };
    store
        .publish_credential(
            &plan.extension_instance,
            &plan.provider,
            ProfileSource::Config,
            &settings,
            &exact,
            None,
        )
        .expect("exact-limit credential");
    let too_large = SecretWrite {
        path: ProviderCredentialSlot::OAuth.path(&plan.provider),
        contents: SecretBytes::new(vec![b'y'; secret_limit + 1]),
    };

    let error = store
        .publish_credential(
            &plan.extension_instance,
            &plan.provider,
            ProfileSource::Config,
            &settings,
            &too_large,
            None,
        )
        .expect_err("oversized credential");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        store
            .credential(
                &plan.extension_instance,
                &plan.provider,
                ProviderCredentialSlot::OAuth,
            )
            .expect("preserved exact-limit credential"),
        exact.contents.expose()
    );
}

/// Proves named-source materialization applies the Secret file limit to the
/// fully serialized typed record and preserves the exact-limit winner.
#[test]
fn publish_credential_enforces_materialized_named_secret_file_limit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    let plan = plan();
    let config_path = store
        .apply_to(&plan, ProfileTarget::Config)
        .expect("config setup")
        .expect("config path");
    let settings = std::fs::read(config_path).expect("settings");
    std::fs::create_dir_all(temp.path().join("secrets")).expect("source root");
    let source_path = temp.path().join("secrets/setup_key.yaml");
    let serialized_overhead = serde_json::to_vec(&ApiKeyCredential::new(String::new()))
        .expect("empty typed credential")
        .len();
    let secret_limit =
        usize::try_from(MAX_SECRET_DATA_FILE_BYTES).expect("Secret limit fits usize");
    let exact_value = "x".repeat(secret_limit - serialized_overhead);
    std::fs::write(&source_path, &exact_value).expect("exact named source");
    let replacement = SecretWrite {
        path: ProviderCredentialSlot::ApiKey.path(&plan.provider),
        contents: SecretBytes::new(b"placeholder".to_vec()),
    };
    let source = NamedSecretSource {
        name: "setup_key".to_owned(),
        declaration: tau_config::settings::ExtensionSecretEntry { optional: false },
    };
    store
        .publish_credential(
            &plan.extension_instance,
            &plan.provider,
            ProfileSource::Config,
            &settings,
            &replacement,
            Some(&source),
        )
        .expect("exact-limit named credential");
    let before = store
        .credential(
            &plan.extension_instance,
            &plan.provider,
            ProviderCredentialSlot::ApiKey,
        )
        .expect("exact-limit credential");
    assert_eq!(before.len(), secret_limit);

    std::fs::write(&source_path, format!("{exact_value}x")).expect("oversized named source");
    let error = store
        .publish_credential(
            &plan.extension_instance,
            &plan.provider,
            ProfileSource::Config,
            &settings,
            &replacement,
            Some(&source),
        )
        .expect_err("oversized named credential");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        store
            .credential(
                &plan.extension_instance,
                &plan.provider,
                ProviderCredentialSlot::ApiKey,
            )
            .expect("preserved credential"),
        before
    );
}

/// Proves list/status holds the instance lock across settings and credential
/// reads, so a concurrent replacement yields a complete old snapshot followed
/// by a complete new registration.
#[test]
fn snapshot_cannot_mix_concurrent_replacement_generations() {
    let temp = tempfile::tempdir().expect("tempdir");
    SetupStore::open_in(temp.path())
        .apply(&direct_plan_with("old", "old-secret"))
        .expect("old apply");
    let snapshot_entered = Arc::new(Barrier::new(2));
    let release_snapshot = Arc::new(Barrier::new(2));
    let state = temp.path().to_path_buf();
    let worker_entered = Arc::clone(&snapshot_entered);
    let worker_release = Arc::clone(&release_snapshot);
    let snapshot = std::thread::spawn(move || {
        SetupStore::open_in(state)
            .with_acquired_pause(worker_entered, worker_release)
            .snapshot(&extension())
    });
    snapshot_entered.wait();

    let replacement_contended = Arc::new(Barrier::new(2));
    let worker_contended = Arc::clone(&replacement_contended);
    let state = temp.path().to_path_buf();
    let replacement = std::thread::spawn(move || {
        SetupStore::open_in(state)
            .with_contention(worker_contended)
            .apply(&direct_plan_with("new", "new-secret"))
    });
    replacement_contended.wait();
    release_snapshot.wait();

    let snapshot = snapshot.join().expect("snapshot thread").expect("snapshot");
    replacement
        .join()
        .expect("replacement thread")
        .expect("replacement");
    assert!(
        String::from_utf8(snapshot.profiles[0].contents.clone())
            .expect("settings")
            .contains("\"marker\":\"old\"")
    );
    assert_eq!(
        snapshot
            .credentials
            .get(&(provider(), ProviderCredentialSlot::OAuth)),
        Some(&b"old-secret".to_vec())
    );
    assert!(
        std::fs::read_to_string(temp.path().join("providers/provider-work/chatgpt.json"),)
            .expect("new settings")
            .contains("\"marker\":\"new\"")
    );
    assert_eq!(
        std::fs::read(
            temp.path()
                .join("secrets/ext/provider-work/providers/chatgpt/oauth.json"),
        )
        .expect("new secret"),
        b"new-secret"
    );
}

/// Proves secret bytes never appear through ordinary diagnostic formatting.
#[test]
fn secret_bytes_debug_redacts_payload() {
    let debug = format!("{:?}", SecretBytes::new(b"never-print-this".to_vec()));
    assert!(!debug.contains("never-print-this"));
    assert!(debug.contains("len"));
}

/// Proves named-source resolution and publication happen while setup owns the
/// instance lifecycle lock, and settings remain the final activation write.
#[test]
fn named_apply_blocks_on_instance_lock_and_publishes_coherent_pair() {
    let temp = tempfile::tempdir().expect("tempdir");
    let settings_root = temp.path().join("providers/provider-work");
    std::fs::create_dir_all(&settings_root).expect("settings root");
    std::fs::create_dir_all(temp.path().join("secrets")).expect("source root");
    std::fs::write(
        temp.path().join("secrets/setup_key.yaml"),
        " named-value \n",
    )
    .expect("source");
    let lock = ProviderSettingsInstanceLock::acquire_existing(temp.path(), "provider-work")
        .expect("lock")
        .expect("existing root");
    let (sender, receiver) = mpsc::channel();
    let state = temp.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let worker = std::thread::spawn(move || {
        sender.send(
            SetupStore::open_in(state)
                .with_contention(worker_barrier)
                .apply(&named_plan()),
        )
    });

    barrier.wait();
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    drop(lock);
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("setup completion")
        .expect("named apply");
    worker.join().expect("setup thread").expect("send result");

    let credential = std::fs::read(
        temp.path()
            .join("secrets/ext/provider-work/providers/chatgpt/api-key.json"),
    )
    .expect("credential");
    let credential: crate::credential_record::ApiKeyCredential =
        serde_json::from_slice(&credential).expect("typed credential");
    assert_eq!(credential.into_value(), "named-value");
    assert!(
        temp.path()
            .join("providers/provider-work/chatgpt.json")
            .is_file()
    );
}

/// Proves setup fails before either activation boundary when its selected
/// required declaration has no value.
#[test]
fn missing_named_source_does_not_activate_settings_or_write_placeholder() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());

    let error = store
        .apply(&named_plan())
        .expect_err("missing required named source");

    assert!(error.to_string().contains("setup_key"));
    assert!(
        !temp
            .path()
            .join("providers/provider-work/chatgpt.json")
            .exists()
    );
    assert!(
        !temp
            .path()
            .join("secrets/ext/provider-work/providers/chatgpt/api-key.json")
            .exists()
    );
}

/// Proves removal of an orphan credential and first setup serialize even when
/// no provider settings file established an earlier generation.
#[test]
fn orphan_removal_racing_first_setup_leaves_one_complete_generation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let settings_root = temp.path().join("providers/provider-work");
    let secret = temp
        .path()
        .join("secrets/ext/provider-work/providers/chatgpt/oauth.json");
    std::fs::create_dir_all(secret.parent().expect("secret parent")).expect("secret root");
    std::fs::write(&secret, "orphan").expect("orphan credential");
    assert!(!settings_root.exists());
    let removal_entered = Arc::new(Barrier::new(2));
    let release_removal = Arc::new(Barrier::new(2));
    let state = temp.path().to_path_buf();
    let worker_removal_entered = Arc::clone(&removal_entered);
    let worker_release_removal = Arc::clone(&release_removal);
    let remove = std::thread::spawn(move || {
        SetupStore::open_in(state)
            .with_acquired_pause(worker_removal_entered, worker_release_removal)
            .remove(&extension(), &provider())
    });
    removal_entered.wait();
    assert!(settings_root.is_dir());

    let setup_contended = Arc::new(Barrier::new(2));
    let state = temp.path().to_path_buf();
    let worker_setup_contended = Arc::clone(&setup_contended);
    let setup = std::thread::spawn(move || {
        SetupStore::open_in(state)
            .with_contention(worker_setup_contended)
            .apply(&direct_plan_with("new", "new-secret"))
    });
    setup_contended.wait();
    release_removal.wait();
    remove.join().expect("remove thread").expect("remove");
    setup.join().expect("setup thread").expect("setup");

    let settings = settings_root.join("chatgpt.json");
    assert!(
        std::fs::read_to_string(settings)
            .expect("settings")
            .contains("\"marker\":\"new\"")
    );
    assert_eq!(std::fs::read(secret).expect("secret"), b"new-secret");
}

/// Proves removal waits for the same instance lock before deactivating
/// settings, then removes credentials in the universal settings-before-Secret
/// order.
#[test]
fn remove_blocks_on_instance_lock_and_removes_complete_registration() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    store.apply(&plan()).expect("apply");
    let lock = ProviderSettingsInstanceLock::acquire_existing(temp.path(), "provider-work")
        .expect("lock")
        .expect("existing root");
    let (sender, receiver) = mpsc::channel();
    let state = temp.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let worker = std::thread::spawn(move || {
        sender.send(
            SetupStore::open_in(state)
                .with_contention(worker_barrier)
                .remove(&extension(), &provider()),
        )
    });

    barrier.wait();
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    drop(lock);
    assert!(
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("remove completion")
            .expect("remove")
    );
    worker.join().expect("remove thread").expect("send result");
    assert!(
        !temp
            .path()
            .join("providers/provider-work/chatgpt.json")
            .exists()
    );
    assert!(
        !temp
            .path()
            .join("secrets/ext/provider-work/providers/chatgpt/oauth.json")
            .exists()
    );
}

/// Proves a startup generation holding the instance lock observes the complete
/// old pair while replacement setup blocks, and the next generation observes
/// the complete new pair.
#[test]
fn setup_replacement_cannot_split_startup_generation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    store
        .apply(&direct_plan_with("old", "old-secret"))
        .expect("old apply");
    let lock = ProviderSettingsInstanceLock::acquire_existing(temp.path(), "provider-work")
        .expect("lock")
        .expect("existing root");
    let old_settings = std::fs::read(lock.root().join("chatgpt.json")).expect("old settings");
    let old_secret = std::fs::read(
        temp.path()
            .join("secrets/ext/provider-work/providers/chatgpt/oauth.json"),
    )
    .expect("old secret");
    let (sender, receiver) = mpsc::channel();
    let state = temp.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let worker = std::thread::spawn(move || {
        sender.send(
            SetupStore::open_in(state)
                .with_contention(worker_barrier)
                .apply(&direct_plan_with("new", "new-secret")),
        )
    });

    barrier.wait();
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    assert!(
        String::from_utf8(old_settings)
            .expect("UTF-8")
            .contains("\"marker\":\"old\"")
    );
    assert_eq!(old_secret, b"old-secret");
    drop(lock);
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("replacement completion")
        .expect("replacement");
    worker.join().expect("setup thread").expect("send result");

    assert!(
        std::fs::read_to_string(temp.path().join("providers/provider-work/chatgpt.json"),)
            .expect("new settings")
            .contains("\"marker\":\"new\"")
    );
    assert_eq!(
        std::fs::read(
            temp.path()
                .join("secrets/ext/provider-work/providers/chatgpt/oauth.json"),
        )
        .expect("new secret"),
        b"new-secret"
    );
}

/// Proves a startup generation holding the instance lock sees the complete old
/// registration while removal blocks; a subsequent generation sees no active
/// settings even if credential cleanup were to fail.
#[test]
fn removal_cannot_split_startup_generation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(temp.path());
    store.apply(&plan()).expect("apply");
    let lock = ProviderSettingsInstanceLock::acquire_existing(temp.path(), "provider-work")
        .expect("lock")
        .expect("existing root");
    assert!(lock.root().join("chatgpt.json").is_file());
    assert!(
        temp.path()
            .join("secrets/ext/provider-work/providers/chatgpt/oauth.json")
            .is_file()
    );
    let (sender, receiver) = mpsc::channel();
    let state = temp.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let worker = std::thread::spawn(move || {
        sender.send(
            SetupStore::open_in(state)
                .with_contention(worker_barrier)
                .remove(&extension(), &provider()),
        )
    });

    barrier.wait();
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    drop(lock);
    assert!(
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("remove completion")
            .expect("remove")
    );
    worker.join().expect("remove thread").expect("send result");
    assert!(
        !temp
            .path()
            .join("providers/provider-work/chatgpt.json")
            .exists()
    );
}

/// Proves a leaf settings publication failure occurs after the secret write,
/// leaving an inactive orphan rather than an active registration without
/// credentials.
#[cfg(unix)]
#[test]
fn apply_failure_preserves_secret_first_boundary() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let settings_root = temp.path().join("providers/provider-work");
    std::fs::create_dir_all(&settings_root).expect("settings root");
    let outside = temp.path().join("outside.json");
    std::fs::write(&outside, "outside").expect("outside");
    symlink(&outside, settings_root.join("chatgpt.json")).expect("settings symlink");
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
    assert_eq!(
        std::fs::read_to_string(&outside).expect("outside"),
        "outside"
    );
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
            .join("providers/provider-work/chatgpt.json")
            .exists()
    );
    assert_eq!(
        std::fs::read(outside.join("oauth.json")).expect("outside secret"),
        b"outside-secret"
    );
}
