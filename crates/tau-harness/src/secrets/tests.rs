use std::sync::Mutex;
use std::time::Duration;

use tau_config::settings::{ExtensionSecretEntry, TauRuntimeSocketAccess, TauStateAccess};
use tempfile::TempDir;

use super::*;
use crate::settings::{Config, CoreConfig, CoreMode, ExtensionConfig};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn config_with_secret(optional: bool) -> Config {
    let mut secrets = BTreeMap::new();
    secrets.insert(
        "mail_password".to_owned(),
        ExtensionSecretEntry { optional },
    );
    Config {
        core: CoreConfig {
            mode: CoreMode::Embedded,
        },
        extensions: BTreeMap::from([(
            "std-email".to_owned(),
            ExtensionConfig {
                tool_prefix: None,
                name: "std-email".to_owned(),
                command: "tau".to_owned(),
                args: Vec::new(),
                role: None,
                component: None,
                require: true,
                startup_timeout: Duration::from_secs(2),
                cwd: None,
                config: serde_json::json!({}),
                secrets,
                tau_state_access: TauStateAccess::Legacy,
                tau_runtime_socket_access: TauRuntimeSocketAccess::Hidden,
            },
        )]),
        extension_startup_diagnostics: Vec::new(),
    }
}

#[test]
fn file_secret_values_are_trimmed() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path().join("secrets");
    std::fs::create_dir_all(&dir).expect("mkdir secrets");
    std::fs::write(dir.join("mail_password.yaml"), "\n value \t\n").expect("write");
    let sources = SecretSources::default();

    let resolved = resolve_extension_secrets(&config_with_secret(false), td.path(), &sources)
        .expect("resolve secrets");

    assert_eq!(
        resolved.secrets["std-email"]["mail_password"].expose_secret(),
        "value"
    );
}

#[test]
#[allow(unsafe_code)]
fn env_overrides_file_and_is_removed() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let td = TempDir::new().expect("tempdir");
    let dir = td.path().join("secrets");
    std::fs::create_dir_all(&dir).expect("mkdir secrets");
    std::fs::write(dir.join("mail_password.yaml"), "file").expect("write");
    // SAFETY: serialized test-only environment mutation.
    unsafe { std::env::set_var("TAU_SECRET_MAIL_PASSWORD", "env") };

    let sources = load_secret_sources().expect("load sources");
    assert!(std::env::var("TAU_SECRET_MAIL_PASSWORD").is_err());
    let resolved = resolve_extension_secrets(&config_with_secret(false), td.path(), &sources)
        .expect("resolve secrets");

    assert_eq!(
        resolved.secrets["std-email"]["mail_password"].expose_secret(),
        "env"
    );
}

#[test]
#[allow(unsafe_code)]
fn uppercase_configured_secret_name_matches_normalized_env() {
    // Secret names in config and TAU_SECRET_* suffixes are treated
    // case-insensitively, while the key handed to the extension stays as
    // configured so extension config can reference the same spelling.
    let _guard = ENV_LOCK.lock().expect("env lock");
    let mut config = config_with_secret(false);
    let extension = config.extensions.get_mut("std-email").expect("extension");
    extension.secrets.clear();
    extension.secrets.insert(
        "GOOGLE_CALENDAR_CLIENT_ID".to_owned(),
        ExtensionSecretEntry { optional: false },
    );
    // SAFETY: serialized test-only environment mutation.
    unsafe { std::env::set_var("TAU_SECRET_GOOGLE_CALENDAR_CLIENT_ID", "client") };

    let sources = load_secret_sources().expect("load sources");
    let tempdir = TempDir::new().expect("tempdir");
    let resolved =
        resolve_extension_secrets(&config, tempdir.path(), &sources).expect("resolve secrets");

    assert_eq!(
        resolved.secrets["std-email"]["GOOGLE_CALENDAR_CLIENT_ID"].expose_secret(),
        "client"
    );
}

#[test]
fn missing_required_secret_names_extension_and_secret() {
    let td = TempDir::new().expect("tempdir");
    let err = resolve_extension_secrets(
        &config_with_secret(false),
        td.path(),
        &SecretSources::default(),
    )
    .expect_err("missing required secret should fail");
    let msg = err.to_string();
    assert!(msg.contains("std-email"));
    assert!(msg.contains("mail_password"));
}

#[test]
fn optional_extension_missing_required_secret_is_skipped_with_diagnostic() {
    let td = TempDir::new().expect("tempdir");
    let mut config = config_with_secret(false);
    config
        .extensions
        .get_mut("std-email")
        .expect("extension")
        .require = false;

    let resolved = resolve_extension_secrets(&config, td.path(), &SecretSources::default())
        .expect("optional missing secret should not fail startup");

    assert!(!resolved.secrets.contains_key("std-email"));
    assert!(resolved.skipped_extensions.contains("std-email"));
    assert_eq!(resolved.diagnostics.len(), 1);
    assert_eq!(resolved.diagnostics[0].extension, "std-email");
    assert_eq!(
        resolved.diagnostics[0].message,
        "optional extension std-email did not initialize"
    );
    assert!(!resolved.diagnostics[0].message.contains("mail_password"));
    assert!(!resolved.diagnostics[0].message.contains("super-secret"));
}

#[test]
fn optional_extension_invalid_secret_name_is_skipped_with_diagnostic() {
    let td = TempDir::new().expect("tempdir");
    let mut config = config_with_secret(false);
    let extension = config.extensions.get_mut("std-email").expect("extension");
    extension.require = false;
    extension.secrets = BTreeMap::from([("../bad".to_owned(), ExtensionSecretEntry::default())]);

    let resolved = resolve_extension_secrets(&config, td.path(), &SecretSources::default())
        .expect("optional invalid secret name should skip extension");

    assert!(!resolved.secrets.contains_key("std-email"));
    assert!(resolved.skipped_extensions.contains("std-email"));
    assert_eq!(
        resolved.diagnostics[0].message,
        "optional extension std-email did not initialize"
    );
    assert!(!resolved.diagnostics[0].message.contains("../bad"));
}

#[test]
fn missing_optional_secret_is_omitted() {
    let td = TempDir::new().expect("tempdir");
    let resolved = resolve_extension_secrets(
        &config_with_secret(true),
        td.path(),
        &SecretSources::default(),
    )
    .expect("optional missing secret resolves");
    assert!(resolved.secrets["std-email"].is_empty());
}

#[test]
fn provider_bound_secret_is_excluded_from_configure() {
    let td = TempDir::new().expect("tempdir");
    let dir = td.path().join("secrets");
    std::fs::create_dir_all(&dir).expect("mkdir secrets");
    std::fs::write(dir.join("mail_password.yaml"), "provider-only").expect("write");
    let bound = BTreeMap::from([(
        "std-email".to_owned(),
        BTreeSet::from(["mail_password".to_owned()]),
    )]);

    let resolved = resolve_extension_secrets_excluding(
        &config_with_secret(false),
        td.path(),
        &SecretSources::default(),
        &bound,
    )
    .expect("resolve secrets");

    assert!(resolved.secrets["std-email"].is_empty());
}

#[test]
fn invalid_secret_names_are_rejected_before_path_join() {
    let mut config = config_with_secret(false);
    let mut secrets = BTreeMap::new();
    secrets.insert("../bad".to_owned(), ExtensionSecretEntry::default());
    config
        .extensions
        .get_mut("std-email")
        .expect("test config should include std-email extension")
        .secrets = secrets;
    let td = TempDir::new().expect("tempdir");

    let err = resolve_extension_secrets(&config, td.path(), &SecretSources::default())
        .expect_err("invalid secret name should fail");
    assert!(err.to_string().contains("../bad"));
}

#[test]
#[allow(unsafe_code)]
fn secret_sources_debug_redacts_values() {
    // Secret source values may come from TAU_SECRET_* environment variables;
    // Debug output must stay safe if the type is logged accidentally.
    let _guard = ENV_LOCK.lock().expect("env lock");
    // SAFETY: serialized test-only environment mutation.
    unsafe { std::env::set_var("TAU_SECRET_MAIL_PASSWORD", "super-secret") };
    let sources =
        load_config_secret_sources(EnvironmentDisposition::RemoveAfterSnapshot).expect("sources");

    let rendered = format!("{sources:?}");

    assert!(rendered.contains("mail_password"));
    assert!(rendered.contains("names"));
    assert!(!rendered.contains("super-secret"));
}

#[test]
#[allow(unsafe_code)]
fn normalized_env_name_collisions_are_rejected() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    // SAFETY: serialized test-only environment mutation.
    unsafe {
        std::env::set_var("TAU_SECRET_COLLIDE", "one");
        std::env::set_var("TAU_SECRET_collide", "two");
    }
    let err = load_secret_sources().expect_err("collision should fail");
    assert!(err.to_string().contains("collide"));
    assert!(std::env::var("TAU_SECRET_COLLIDE").is_err());
    assert!(std::env::var("TAU_SECRET_collide").is_err());
}
