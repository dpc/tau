use std::sync::Mutex;

use tempfile::TempDir;

use super::*;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Proves setup retention and harness one-shot removal share normalization,
/// trimming, environment precedence, and redacted Debug behavior.
#[test]
#[allow(unsafe_code)]
fn environment_disposition_and_resolution_are_canonical() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let temp = TempDir::new().expect("tempdir");
    std::fs::create_dir(temp.path().join("secrets")).expect("secrets");
    std::fs::write(temp.path().join("secrets/api_key.yaml"), " file \n").expect("file");
    // SAFETY: the process environment is serialized by ENV_LOCK.
    unsafe { std::env::set_var("TAU_SECRET_API_KEY", " env \n") };

    let retained = load_secret_sources(EnvironmentDisposition::Retain).expect("retained sources");
    assert_eq!(
        resolve_named_secret(temp.path(), &retained, "API_KEY")
            .expect("resolve")
            .as_deref(),
        Some("env")
    );
    assert_eq!(
        std::env::var("TAU_SECRET_API_KEY").as_deref(),
        Ok(" env \n")
    );
    let debug = format!("{retained:?}");
    assert!(debug.contains("api_key"));
    assert!(!debug.contains(" env "));

    let removed =
        load_secret_sources(EnvironmentDisposition::RemoveAfterSnapshot).expect("one-shot sources");
    assert_eq!(
        resolve_named_secret(temp.path(), &removed, "api_key")
            .expect("resolve")
            .as_deref(),
        Some("env")
    );
    assert!(std::env::var("TAU_SECRET_API_KEY").is_err());
}

/// Proves normalized collisions fail and one-shot capture removes every
/// colliding variable even on error.
#[test]
#[allow(unsafe_code)]
fn collisions_fail_after_environment_cleanup() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    // SAFETY: the process environment is serialized by ENV_LOCK.
    unsafe {
        std::env::set_var("TAU_SECRET_COLLIDE", "one");
        std::env::set_var("TAU_SECRET_collide", "two");
    }
    assert!(load_secret_sources(EnvironmentDisposition::RemoveAfterSnapshot).is_err());
    assert!(std::env::var("TAU_SECRET_COLLIDE").is_err());
    assert!(std::env::var("TAU_SECRET_collide").is_err());
}

/// Proves declaration optionality and invalid UTF-8 behavior are shared rather
/// than reimplemented by setup or harness startup.
#[test]
fn declared_secret_handles_optional_missing_and_invalid_utf8() {
    let temp = TempDir::new().expect("tempdir");
    let optional = ExtensionSecretEntry { optional: true };
    assert!(
        resolve_declared_secret(
            temp.path(),
            &SecretSources::default(),
            "provider-builtin",
            "missing",
            &optional,
        )
        .expect("optional")
        .is_none()
    );

    std::fs::create_dir(temp.path().join("secrets")).expect("secrets");
    std::fs::write(temp.path().join("secrets/bad.yaml"), [0xff]).expect("invalid UTF-8");
    assert!(
        resolve_declared_secret(
            temp.path(),
            &SecretSources::default(),
            "provider-builtin",
            "bad",
            &optional,
        )
        .is_err()
    );
}
