use super::*;

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

#[test]
fn mint_session_id_produces_store_safe_ids_from_hostile_basenames() {
    // Session ids are used as tau-core store directory names. Cwd basenames can
    // contain characters that are valid on Unix but forbidden by the store
    // grammar, and can be too long once the random suffix is appended.
    let id = mint_session_id(Path::new("project\\name"));
    assert!(id.starts_with("project_name-"));
    assert!(!id.contains('\\'));

    let long = "é".repeat(100);
    let id = mint_session_id(Path::new(&long));
    assert!(id.len() <= SESSION_ID_MAX_BYTES);
    assert!(id.ends_with(|ch: char| ch.is_ascii_alphanumeric()));
}
