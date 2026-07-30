use tau_config::settings as path_tau_config_settings;

use super::*;

/// Once a connected client closes its taken input pipe, dropping the owned
/// handle waits for normal lifecycle cleanup before forced termination.
#[cfg(unix)]
#[test]
fn owned_daemon_drop_allows_disconnect_cleanup() {
    let temp = tempfile::TempDir::new().expect("temporary directory");
    let marker = temp.path().join("clean-exit");
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("cat >/dev/null; : > \"$1\"")
        .arg("tau-daemon-drop-test")
        .arg(&marker)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn pipe-waiting child");
    let initial_ui = InitialUiStdio {
        stdin: child.stdin.take().expect("child stdin"),
        stdout: child.stdout.take().expect("child stdout"),
    };
    let mut handle = DaemonHandle::Owned {
        child: Some(child),
        harness_path: temp.path().join("unused"),
        initial_ui: Some(initial_ui),
    };
    let connected_transport = handle
        .take_initial_ui_stdio()
        .expect("connected client transport");

    drop(connected_transport);
    drop(handle);

    assert!(
        marker.exists(),
        "child must observe EOF and finish cleanup before forced termination"
    );
}

/// A child that does not exit after transport closure is still reaped once the
/// bounded graceful-cleanup allowance expires.
#[cfg(unix)]
#[test]
fn owned_daemon_cleanup_has_forced_termination_fallback() {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("trap '' HUP TERM; while :; do read _ || :; done")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn non-exiting child");
    let initial_ui = InitialUiStdio {
        stdin: child.stdin.take().expect("child stdin"),
        stdout: child.stdout.take().expect("child stdout"),
    };

    stop_owned_daemon(&mut child, Some(initial_ui), Duration::ZERO);

    assert!(
        child.try_wait().expect("query reaped child").is_some(),
        "forced fallback must reap a child that cannot clean up"
    );
}

#[test]
fn daemon_command_sets_and_clears_harness_config_override_env() {
    let override_ = tau_config::settings::HarnessConfigCliOverride {
        key: "session_retention_days".to_owned(),
        raw_value: "3".to_owned(),
    };
    let with_override = build_daemon_command(DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        session_id: "session-1",
        session_status: SessionLaunchStatus::New,
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: None,
        cli_overrides: DaemonCliOverrides {
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: std::slice::from_ref(&override_),
        },
        ephemeral: false,
    });
    assert!(with_override.get_envs().any(|(key, value)| {
        key == tau_harness::HARNESS_CONFIG_CLI_OVERRIDES_ENV && value.is_some()
    }));

    let without_override = build_daemon_command(DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        session_id: "session-1",
        session_status: SessionLaunchStatus::New,
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: None,
        cli_overrides: DaemonCliOverrides {
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: &[],
        },
        ephemeral: false,
    });
    assert!(without_override.get_envs().any(|(key, value)| {
        key == tau_harness::HARNESS_CONFIG_CLI_OVERRIDES_ENV && value.is_none()
    }));
}

/// Ensures the private parent-to-child extension transport cannot be inherited
/// when the current invocation has no ordered CLI extension operations.
#[test]
fn daemon_command_clears_empty_extension_transport() {
    let command = build_daemon_command(DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        session_id: "session-1",
        session_status: SessionLaunchStatus::New,
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: None,
        cli_overrides: DaemonCliOverrides {
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: &[],
        },
        ephemeral: false,
    });
    assert!(command.get_envs().any(|(key, value)| {
        key == tau_harness::EXTENSION_CLI_OVERRIDES_ENV && value.is_none()
    }));
    assert!(
        !command
            .get_envs()
            .any(|(key, _)| key == tau_config::settings::TAU_ENABLE_EXTENSIONS_ENV),
        "the supported public environment must remain inherited"
    );
}

/// Ensures preview daemons receive the already parsed public environment
/// through its supported variable while argv operations remain in the private
/// ordered CLI transport.
#[test]
fn daemon_command_forwards_public_extension_environment_separately() {
    let names = vec!["test-dummy".to_owned(), "std-rhai".to_owned()];
    let cli = [path_tau_config_settings::ExtensionCliOverride::Disable(
        "test-dummy".to_owned(),
    )];
    let command = build_daemon_command(DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        session_id: "print-tools-1",
        session_status: SessionLaunchStatus::New,
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: Some("engineer"),
        cli_overrides: DaemonCliOverrides {
            role: &[],
            extension: &cli,
            extension_environment: Some(&names),
            harness_config: &[],
        },
        ephemeral: false,
    });

    assert!(command.get_envs().any(|(key, value)| {
        key == tau_config::settings::TAU_ENABLE_EXTENSIONS_ENV
            && value == Some(std::ffi::OsStr::new("test-dummy,std-rhai"))
    }));
    assert!(command.get_envs().any(|(key, value)| {
        key == tau_harness::EXTENSION_CLI_OVERRIDES_ENV
            && value
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|value| {
                    serde_json::from_str::<Vec<tau_config::settings::ExtensionCliOverride>>(value)
                        .is_ok_and(|decoded| decoded == cli)
                })
    }));
}

#[test]
fn daemon_command_clears_socket_activation_env() {
    let command = build_daemon_command(DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        session_id: "session-1",
        session_status: SessionLaunchStatus::New,
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: None,
        cli_overrides: DaemonCliOverrides {
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: &[],
        },
        ephemeral: false,
    });

    for expected_key in [
        "LISTEN_FDS",
        "LISTEN_PID",
        "LISTEN_FDS_FIRST_FD",
        "LISTEN_FDNAMES",
    ] {
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == expected_key && value.is_none()),
            "expected {expected_key} to be removed from harness child environment"
        );
    }
}

/// Ensures a CLI-managed harness spawn receives the exact random instance id
/// that the parent uses to predict its later-attach runtime socket path.
#[test]
fn daemon_command_and_parent_path_share_runtime_instance_id() {
    let mut command = build_daemon_command(DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        session_id: "session-1",
        session_status: SessionLaunchStatus::New,
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: None,
        cli_overrides: DaemonCliOverrides {
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: &[],
        },
        ephemeral: false,
    });

    let instance_id = configure_runtime_instance(&mut command);
    assert!(command.get_envs().any(|(key, value)| {
        key == tau_harness::runtime_dir::HARNESS_INSTANCE_ID_ENV
            && value.and_then(std::ffi::OsStr::to_str) == Some(instance_id.as_str())
    }));
    assert!(
        tau_harness::runtime_dir::harness_path_for_process(205, &instance_id)
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|name| name == format!("205-{}", instance_id.as_str()))
    );
}

/// Guards both sides of the ephemeral child-process environment contract: an
/// ephemeral launch opts the harness child in, while a normal launch explicitly
/// clears any inherited marker.
#[test]
fn daemon_command_sets_ephemeral_env_only_when_requested() {
    let command = build_daemon_command(DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        session_id: "session-1",
        session_status: SessionLaunchStatus::New,
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: None,
        cli_overrides: DaemonCliOverrides {
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: &[],
        },
        ephemeral: true,
    });

    assert!(command.get_envs().any(|(key, value)| {
        key == tau_harness::EPHEMERAL_ENV && value.and_then(|v| v.to_str()) == Some("1")
    }));

    let command = build_daemon_command(DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        session_id: "session-1",
        session_status: SessionLaunchStatus::New,
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: None,
        cli_overrides: DaemonCliOverrides {
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: &[],
        },
        ephemeral: false,
    });

    assert!(
        command
            .get_envs()
            .any(|(key, value)| { key == tau_harness::EPHEMERAL_ENV && value.is_none() })
    );
}

#[test]
fn daemon_command_uses_initial_ui_stdio() {
    let command = build_daemon_command(DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        session_id: "session-1",
        session_status: SessionLaunchStatus::New,
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: None,
        cli_overrides: DaemonCliOverrides {
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: &[],
        },
        ephemeral: false,
    });

    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(args, ["component", "harness", "--initial-ui-stdio"]);
}

/// Session minting must normalize arbitrary filesystem basenames into the
/// strict protocol grammar without exceeding its byte limit.
#[test]
fn mint_session_id_produces_store_safe_ids_from_hostile_basenames() {
    // Session ids are used as tau-core store directory names. Cwd basenames can
    // contain characters that are valid on Unix but forbidden by the store
    // grammar, and can be too long once the random suffix is appended.
    let id = mint_session_id(Path::new("project\\name"));
    assert!(id.starts_with("project_name-"));
    assert!(!id.contains('\\'));

    let long = format!("my project.{}", "é".repeat(100));
    let id = mint_session_id(Path::new(&long));
    assert!(id.len() <= tau_proto::SESSION_SCOPED_ID_MAX_LEN);
    assert!(id.ends_with(|ch: char| ch.is_ascii_alphanumeric()));
}
