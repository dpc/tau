use tau_config::settings as path_tau_config_settings;
use tau_config::settings::ProfileName;

use super::*;

/// One available row bypasses a picker even when newer rows are locked.
#[test]
fn one_unlocked_resume_row_is_selected_deterministically() {
    assert_eq!(
        sole_unlocked_resume_index([true, true, false, true]).expect("selection succeeds"),
        Some(2)
    );
}

/// All-locked rows produce an actionable error instead of looking like a
/// cancelled picker or an absent persisted session.
#[test]
fn all_locked_resume_rows_report_attach_action() {
    let error = sole_unlocked_resume_index([true, true]).expect_err("all rows are locked");
    assert!(error.to_string().contains("tau attach SESSION"));
}

/// Several available rows preserve interactive selection.
#[test]
fn several_unlocked_resume_rows_require_picker() {
    assert_eq!(
        sole_unlocked_resume_index([false, true, false]).expect("selection succeeds"),
        None
    );
}

/// The display cap must not hide every selectable target when the newest rows
/// are all owned by running harnesses.
#[test]
fn resume_picker_preserves_unlocked_row_beyond_display_cap() {
    let mut locked = vec![true; RESUME_PICKER_LIMIT + 2];
    locked[RESUME_PICKER_LIMIT + 1] = false;

    let visible = visible_resume_indices(locked);

    assert_eq!(visible.len(), RESUME_PICKER_LIMIT);
    assert_eq!(visible[RESUME_PICKER_LIMIT - 1], RESUME_PICKER_LIMIT + 1);
}

/// Resume stderr setup must remain path-free until the harness reports
/// lock-held readiness, so a deleted selection is not recreated by logging.
#[test]
fn resumed_daemon_output_defers_session_log_creation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let session_dir = temp.path().join("session-1");
    let output = daemon_output_for_session_in(
        temp.path(),
        "session-1",
        HarnessStorageMode::Durable,
        SessionLaunchStatus::Resumed,
    )
    .expect("resolve resumed output");

    assert!(output.deferred_harness_log.is_some());
    assert!(!session_dir.exists());
}

/// The resumed relay appends diagnostics to the lock-held file created by the
/// child rather than replacing or truncating it.
#[cfg(unix)]
#[test]
fn resumed_stderr_relay_appends_to_child_created_log() {
    let temp = tempfile::tempdir().expect("tempdir");
    let harness_log = temp.path().join("session/logs/tau-harness.log");
    std::fs::create_dir_all(harness_log.parent().expect("log parent")).expect("create log parent");
    std::fs::write(&harness_log, "child-ready\n").expect("create child log");
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("printf 'resumed diagnostic\\n' >&2")
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn diagnostic child");
    let stderr = child.stderr.take().expect("child stderr");

    relay_stderr_after_lock_held_log(stderr, harness_log.clone());
    child.wait().expect("wait for child");
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        let contents = std::fs::read_to_string(&harness_log).expect("read log");
        if contents.contains("resumed diagnostic") {
            assert!(contents.starts_with("child-ready\n"));
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("relay did not append resumed diagnostics");
}

/// A child exit and cleanup before the relay opens the lock-held file must not
/// recreate either the log or its deleted session directory.
#[cfg(unix)]
#[test]
fn resumed_stderr_relay_does_not_recreate_deleted_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let session_dir = temp.path().join("session");
    let harness_log = session_dir.join("logs/tau-harness.log");
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("printf 'failed resume\\n' >&2")
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn diagnostic child");
    let stderr = child.stderr.take().expect("child stderr");

    relay_stderr_after_lock_held_log(stderr, harness_log);
    child.wait().expect("wait for child");
    std::thread::sleep(Duration::from_millis(100));

    assert!(!session_dir.exists());
}

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
        cleanup_runtime_pair_after_reap: false,
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

/// A prompt-style client whose owned child withholds admission returns on its
/// deadline, closes the initial UI input, and lets normal daemon drop reap it.
#[cfg(unix)]
#[test]
fn owned_daemon_withheld_admission_times_out_and_cleans_up() {
    let temp = tempfile::TempDir::new().expect("temporary directory");
    let marker = temp.path().join("clean-exit");
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("cat >/dev/null; : > \"$1\"")
        .arg("tau-admission-timeout-test")
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
        cleanup_runtime_pair_after_reap: false,
    };
    let expected = tau_proto::SessionId::parse("session-1").expect("valid session");

    let error = match crate::ui_client::connect_daemon_ui_client_with_timeout(
        &mut handle,
        "timeout-test",
        Some(&expected),
        Duration::from_millis(10),
    ) {
        Ok(_) => panic!("withheld admission must time out"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    drop(handle);
    assert!(marker.exists());
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
            profile: None,
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: std::slice::from_ref(&override_),
        },
        storage_mode: HarnessStorageMode::Durable,
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
            profile: None,
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: &[],
        },
        storage_mode: HarnessStorageMode::Durable,
    });
    assert!(without_override.get_envs().any(|(key, value)| {
        key == tau_harness::HARNESS_CONFIG_CLI_OVERRIDES_ENV && value.is_none()
    }));
}

/// Ensures a resolved CLI profile reaches the daemon and an absent selection
/// clears inherited profile state rather than changing child configuration.
#[test]
fn daemon_command_sets_and_clears_profile_env() {
    let spec = |profile| DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        session_id: "session-1",
        session_status: SessionLaunchStatus::New,
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: None,
        cli_overrides: DaemonCliOverrides {
            profile,
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: &[],
        },
        storage_mode: HarnessStorageMode::Durable,
    };
    let profile = ProfileName::parse("focused").expect("profile");
    let selected = build_daemon_command(spec(Some(&profile)));
    assert!(selected.get_envs().any(|(key, value)| {
        key == tau_config::settings::TAU_PROFILE_ENV && value == Some("focused".as_ref())
    }));

    let absent = build_daemon_command(spec(None));
    assert!(
        absent.get_envs().any(|(key, value)| {
            key == tau_config::settings::TAU_PROFILE_ENV && value.is_none()
        })
    );
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
            profile: None,
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: &[],
        },
        storage_mode: HarnessStorageMode::Durable,
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
            profile: None,
            role: &[],
            extension: &cli,
            extension_environment: Some(&names),
            harness_config: &[],
        },
        storage_mode: HarnessStorageMode::Durable,
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
            profile: None,
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: &[],
        },
        storage_mode: HarnessStorageMode::Durable,
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
            profile: None,
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: &[],
        },
        storage_mode: HarnessStorageMode::Durable,
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

/// Every storage mode sets only its matching child-process environment marker
/// and explicitly clears markers inherited from the parent.
#[test]
fn daemon_command_maps_storage_mode_to_exclusive_environment_marker() {
    let command = build_daemon_command(DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        session_id: "session-1",
        session_status: SessionLaunchStatus::New,
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: None,
        cli_overrides: DaemonCliOverrides {
            profile: None,
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: &[],
        },
        storage_mode: HarnessStorageMode::SessionEphemeral,
    });

    assert!(command.get_envs().any(|(key, value)| {
        key == tau_harness::EPHEMERAL_ENV && value.and_then(|v| v.to_str()) == Some("1")
    }));
    assert!(
        command
            .get_envs()
            .any(|(key, value)| key == tau_harness::MEMORY_ONLY_ENV && value.is_none())
    );

    let command = build_daemon_command(DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        session_id: "session-1",
        session_status: SessionLaunchStatus::New,
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: None,
        cli_overrides: DaemonCliOverrides {
            profile: None,
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: &[],
        },
        storage_mode: HarnessStorageMode::Durable,
    });

    assert!(
        command
            .get_envs()
            .any(|(key, value)| { key == tau_harness::EPHEMERAL_ENV && value.is_none() })
    );
    assert!(
        command
            .get_envs()
            .any(|(key, value)| key == tau_harness::MEMORY_ONLY_ENV && value.is_none())
    );

    let command = build_daemon_command(DaemonCommandSpec {
        tau_binary: Path::new("tau"),
        session_id: "session-1",
        session_status: SessionLaunchStatus::New,
        stdout: Stdio::null(),
        stderr: Stdio::null(),
        stdin: Stdio::null(),
        startup_role: None,
        cli_overrides: DaemonCliOverrides {
            profile: None,
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: &[],
        },
        storage_mode: HarnessStorageMode::MemoryOnly,
    });
    assert!(command.get_envs().any(|(key, value)| {
        key == tau_harness::MEMORY_ONLY_ENV && value.and_then(|v| v.to_str()) == Some("1")
    }));
    assert!(
        command
            .get_envs()
            .any(|(key, value)| key == tau_harness::EPHEMERAL_ENV && value.is_none())
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
            profile: None,
            role: &[],
            extension: &[],
            extension_environment: None,
            harness_config: &[],
        },
        storage_mode: HarnessStorageMode::Durable,
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
