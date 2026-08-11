use std::sync::Mutex;

use tempfile::TempDir;

use super::*;

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[cfg(unix)]
const RAW_ENV_SUBPROCESS_TEST: &str =
    "secret_sources::tests::raw_environment_entries_are_handled_without_panicking";

/// Proves raw matching names and values return redacted typed errors, obey
/// disposition, and do not let unrelated non-Unicode entries affect discovery.
#[cfg(unix)]
#[test]
fn raw_environment_entries_are_handled_without_panicking() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;
    use std::process::Command;

    if let Some(action) = std::env::var_os("TAU_RAW_ENV_TEST_ACTION") {
        let action = action.to_str().expect("Unicode test action");
        let result = load_secret_sources(match action {
            "suffix-retain" => EnvironmentDisposition::Retain,
            _ => EnvironmentDisposition::RemoveAfterSnapshot,
        });
        match action {
            "suffix-remove" => {
                assert!(matches!(
                    result,
                    Err(SecretSourceError::EnvironmentNameNotUnicode)
                ));
                assert!(
                    !std::env::vars_os().any(|(key, _)| is_secret_environment_key(&key)),
                    "one-shot capture must remove malformed matching names"
                );
            }
            "value-remove" => {
                let error = result.expect_err("raw matching value must fail");
                assert!(matches!(
                    error,
                    SecretSourceError::EnvironmentValueNotUnicode
                ));
                let diagnostic = format!("{error:?}: {error}");
                assert!(!diagnostic.contains("raw-value-secret"));
                assert!(
                    !std::env::vars_os().any(|(key, _)| is_secret_environment_key(&key)),
                    "one-shot capture must remove malformed matching values"
                );
            }
            "suffix-retain" => {
                assert!(matches!(
                    result,
                    Err(SecretSourceError::EnvironmentNameNotUnicode)
                ));
                assert!(
                    std::env::vars_os().any(|(key, _)| is_secret_environment_key(&key)),
                    "setup discovery must retain malformed matching names"
                );
            }
            "unrelated" => {
                result.expect("unrelated raw entry must be ignored");
                assert!(
                    std::env::vars_os().any(|(key, value)| {
                        key.as_encoded_bytes().starts_with(b"UNRELATED_")
                            && value.as_encoded_bytes().starts_with(b"raw-value")
                    }),
                    "unrelated raw entry must remain available"
                );
            }
            other => panic!("unknown raw environment test action: {other}"),
        }
        return;
    }

    let cases = [
        (
            "suffix-remove",
            OsString::from_vec(b"TAU_SECRET_BAD\xff".to_vec()),
            OsString::from("value"),
        ),
        (
            "value-remove",
            OsString::from("TAU_SECRET_BAD_VALUE"),
            OsString::from_vec(b"raw-value-secret\xff".to_vec()),
        ),
        (
            "suffix-retain",
            OsString::from_vec(b"TAU_SECRET_RETAIN\xff".to_vec()),
            OsString::from("value"),
        ),
        (
            "unrelated",
            OsString::from_vec(b"UNRELATED_\xff".to_vec()),
            OsString::from_vec(b"raw-value\xff".to_vec()),
        ),
    ];
    for (action, key, value) in cases {
        let output = Command::new(std::env::current_exe().expect("current test executable"))
            .args(["--exact", RAW_ENV_SUBPROCESS_TEST, "--nocapture"])
            .env_clear()
            .env("TAU_RAW_ENV_TEST_ACTION", action)
            .env(key, value)
            .output()
            .expect("launch raw environment test subprocess");
        assert!(
            output.status.success(),
            "{action} subprocess failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

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

/// Proves a Unicode environment suffix outside the safe-name grammar remains
/// a typed validation failure and is consumed by one-shot capture.
#[test]
#[allow(unsafe_code)]
fn invalid_unicode_environment_name_fails_after_cleanup() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    // SAFETY: the process environment is serialized by ENV_LOCK.
    unsafe { std::env::set_var("TAU_SECRET_BAD/NAME", "value") };

    assert!(matches!(
        load_secret_sources(EnvironmentDisposition::RemoveAfterSnapshot),
        Err(SecretSourceError::InvalidName(name)) if name == "bad/name"
    ));
    assert!(std::env::var_os("TAU_SECRET_BAD/NAME").is_none());
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
