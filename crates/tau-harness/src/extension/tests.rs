use std::collections::BTreeMap;
use std::error::Error as _;
use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

use tau_config::settings::TauStateAccess;

use super::*;

fn test_extension_config(cwd: Option<PathBuf>) -> ExtensionConfig {
    ExtensionConfig {
        tool_prefix: None,
        name: "test-extension".to_owned(),
        command: "tau-test-extension".to_owned(),
        args: vec!["--stdio".to_owned()],
        role: None,
        component: None,
        require: true,
        startup_timeout: Duration::from_secs(2),
        cwd,
        config: serde_json::json!({}),
        secrets: BTreeMap::new(),
        tau_state_access: TauStateAccess::Legacy,
    }
}

#[test]
fn extension_stderr_log_path_rejects_unsafe_extension_names() {
    // Extension names originate in user-authored harness config. The
    // stderr log path is constructed before the Configure handshake, so it
    // must reject traversal and absolute-path names on its own.
    let sessions_dir = Path::new("/tmp/tau-sessions");
    assert_eq!(
        extension_stderr_log_path(sessions_dir, "session-1", "std-email")
            .expect("safe extension name"),
        sessions_dir
            .join("session-1")
            .join("logs")
            .join("std-email.log")
    );

    for name in ["", "../x", "a/b", "/tmp/x", ".", ".."] {
        assert!(
            extension_stderr_log_path(sessions_dir, "session-1", name).is_err(),
            "{name:?} must be rejected before building the log path"
        );
    }
}

#[test]
fn supervised_command_uses_configured_cwd() {
    // The cwd field is resolved from harness.yaml and must affect the OS
    // child process, not the LifecycleConfigure payload sent after spawn.
    let cwd = PathBuf::from("/tmp/tau-extension-cwd");
    let config = test_extension_config(Some(cwd.clone()));

    let (command, _) = supervised_command(
        &config,
        &ClientKind::Tool,
        None,
        Path::new("/tmp/tau-state"),
        false,
    )
    .expect("build supervised command");

    assert_eq!(command.get_current_dir(), Some(cwd.as_path()));
}

/// Proves persistent launch preparation creates a mount root only for
/// providers, preventing ordinary tool extensions from populating the
/// provider-settings tree.
#[test]
fn persistent_launch_prepares_settings_mount_only_for_provider() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = temp.path().join("state");
    std::fs::create_dir(&state).expect("state root");

    assert_eq!(
        prepare_provider_settings_mount(&state, "std-shell", &ClientKind::Tool, false)
            .expect("prepare tool launch"),
        None
    );
    assert!(!state.join("provider-settings").exists());

    let provider_root =
        prepare_provider_settings_mount(&state, "provider-work", &ClientKind::Provider, false)
            .expect("prepare provider launch")
            .expect("provider settings root");
    assert_eq!(provider_root, state.join("provider-settings/provider-work"));
    assert!(provider_root.is_dir());
    assert_eq!(
        std::fs::symlink_metadata(&provider_root)
            .expect("provider settings metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    let memory_state = temp.path().join("memory-state");
    std::fs::create_dir(&memory_state).expect("memory-only state root");
    assert_eq!(
        prepare_provider_settings_mount(
            &memory_state,
            "provider-memory",
            &ClientKind::Provider,
            true,
        )
        .expect("prepare memory-only provider launch"),
        None
    );
    assert!(!memory_state.join("provider-settings").exists());
}

/// Ensures a built-in instance's failed executable is actionable without
/// exposing arguments, extension config, or an absent cwd.
#[test]
fn builtin_spawn_failure_is_contextual_and_secret_safe() {
    let mut config = test_extension_config(None);
    config.name = "provider-builtin".to_owned();
    config.command = format!("/tau-test-missing-extension-{}", std::process::id());
    config.args = vec!["--token=argument-secret".to_owned()];
    config.config = serde_json::json!({"token": "config-secret"});

    let (tx, _rx) = mpsc::channel();
    let error = match spawn_supervised(
        &config,
        ClientKind::Provider,
        None,
        &tx,
        Path::new("/tmp/tau-state"),
        false,
    ) {
        Ok(_) => panic!("missing built-in executable must fail"),
        Err(error) => error,
    };
    let diagnostic = error.to_string();

    assert!(diagnostic.contains("extension instance \"provider-builtin\""));
    assert!(diagnostic.contains("`command` executable"));
    assert!(diagnostic.contains(&config.command));
    assert!(!diagnostic.contains("cwd"));
    assert!(!diagnostic.contains("argument-secret"));
    assert!(!diagnostic.contains("config-secret"));
    assert!(
        error.source().is_some(),
        "harness error must retain spawn source"
    );
    let os_error = error
        .source()
        .and_then(|source| source.source())
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .expect("extension context must retain the underlying OS error");
    assert_eq!(os_error.kind(), std::io::ErrorKind::NotFound);
    assert!(diagnostic.ends_with(&os_error.to_string()));
}

/// Ensures a custom instance's invalid configured cwd is included while every
/// unrelated or potentially secret field stays out and long fields are bounded.
#[test]
fn custom_spawn_failure_includes_only_relevant_bounded_context() {
    let current_exe = std::env::current_exe().expect("current test executable");
    let long_name = format!("custom-{}-instance-secret", "x".repeat(300));
    let cwd = PathBuf::from(format!(
        "/tau-test-missing-cwd-{}-{}",
        std::process::id(),
        "y".repeat(300)
    ));
    let mut config = test_extension_config(Some(cwd));
    config.name = long_name;
    config.command = format!("{}{}", current_exe.to_string_lossy(), "z".repeat(300));
    config.args = vec!["argument-secret".to_owned()];
    config.config = serde_json::json!({"secret": "config-secret"});

    let (tx, _rx) = mpsc::channel();
    let error = match spawn_supervised(
        &config,
        ClientKind::Tool,
        None,
        &tx,
        Path::new("/tmp/tau-state"),
        false,
    ) {
        Ok(_) => panic!("missing custom cwd must fail"),
        Err(error) => error,
    };
    let diagnostic = error.to_string();

    assert!(diagnostic.contains("configured cwd"));
    assert!(diagnostic.contains("custom-"));
    assert!(diagnostic.contains("tau-test-missing-cwd"));
    assert!(diagnostic.contains('…'), "long context must be truncated");
    assert!(
        diagnostic.len() < 1_500,
        "spawn diagnostic must remain bounded: {} bytes",
        diagnostic.len()
    );
    assert!(!diagnostic.contains("instance-secret"));
    assert!(!diagnostic.contains("argument-secret"));
    assert!(!diagnostic.contains("config-secret"));
    let os_error = error
        .source()
        .and_then(|source| source.source())
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .expect("custom spawn failure must retain its OS source");
    assert!(diagnostic.ends_with(&os_error.to_string()));
}
