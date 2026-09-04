use tau_config::settings as path_tau_config_settings;
use tau_config::settings::ProfileSelection;

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
            memory_only_agent_store: false,
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
            memory_only_agent_store: false,
        },
        storage_mode: HarnessStorageMode::Durable,
    });
    assert!(without_override.get_envs().any(|(key, value)| {
        key == tau_harness::HARNESS_CONFIG_CLI_OVERRIDES_ENV && value.is_none()
    }));
    for variable in [
        tau_config::settings::TAU_PROVIDER_ALIASES_ENV,
        tau_config::settings::TAU_MODEL_ALIASES_ENV,
    ] {
        assert!(
            with_override
                .get_envs()
                .any(|(key, value)| key == variable && value.is_none()),
            "spawned daemon must not re-read inherited {variable}"
        );
    }
}

/// Ensures the launcher does not manufacture a parent-wide logging allowlist;
/// the harness and each extension must own their absent-environment fallback.
#[test]
fn daemon_command_leaves_tau_log_inherited() {
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
            memory_only_agent_store: false,
        },
        storage_mode: HarnessStorageMode::Durable,
    });

    assert!(
        command
            .get_envs()
            .all(|(name, _)| name != std::ffi::OsStr::new("TAU_LOG"))
    );
}

/// Ensures an ordered CLI profile selection reaches the daemon unchanged and an
/// absent selection clears inherited profile state rather than changing child
/// configuration.
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
            memory_only_agent_store: false,
        },
        storage_mode: HarnessStorageMode::Durable,
    };
    let profile = ProfileSelection::parse("focused,review").expect("profile selection");
    let selected = build_daemon_command(spec(Some(&profile)));
    assert!(selected.get_envs().any(|(key, value)| {
        key == tau_config::settings::TAU_PROFILE_ENV && value == Some("focused,review".as_ref())
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
            memory_only_agent_store: false,
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
            memory_only_agent_store: false,
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
            memory_only_agent_store: false,
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
            memory_only_agent_store: false,
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
            memory_only_agent_store: false,
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
            memory_only_agent_store: false,
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

/// The preview-only agent-store marker must be set for diagnostics and cleared
/// for both ordinary durable and global session-ephemeral launches.
#[test]
fn daemon_command_scopes_memory_only_agent_store_to_previews() {
    for storage_mode in [
        HarnessStorageMode::Durable,
        HarnessStorageMode::SessionEphemeral,
    ] {
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
                memory_only_agent_store: false,
            },
            storage_mode,
        });
        command.env(tau_harness::MEMORY_ONLY_AGENT_STORE_ENV, "ambient");
        configure_agent_store_mode(&mut command, false);
        assert!(command.get_envs().any(|(key, value)| {
            key == tau_harness::MEMORY_ONLY_AGENT_STORE_ENV && value.is_none()
        }));
    }

    let mut preview = Command::new("tau");
    configure_agent_store_mode(&mut preview, true);
    assert!(preview.get_envs().any(|(key, value)| {
        key == tau_harness::MEMORY_ONLY_AGENT_STORE_ENV
            && value.and_then(std::ffi::OsStr::to_str) == Some("1")
    }));
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
            memory_only_agent_store: false,
        },
        storage_mode: HarnessStorageMode::Durable,
    });

    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(args, ["component", "harness", "--initial-ui-stdio"]);
}

/// Only conversational chat launches mark their owned stdio client as eligible
/// for the welcome; preview and one-shot callers retain the cleared default.
#[test]
fn introduction_notice_marker_is_explicit_and_reversible() {
    let mut command = Command::new("tau");
    configure_introduction_notice(&mut command, true);
    assert!(command.get_envs().any(|(key, value)| {
        key == tau_harness::INITIAL_UI_INTRODUCTION_NOTICE_ENV
            && value.and_then(std::ffi::OsStr::to_str) == Some("1")
    }));

    configure_introduction_notice(&mut command, false);
    assert!(command.get_envs().any(|(key, value)| {
        key == tau_harness::INITIAL_UI_INTRODUCTION_NOTICE_ENV && value.is_none()
    }));
}

/// The production output constructors distinguish conversational chat from
/// prompt-stdin and render-preview launches before command construction.
#[test]
fn chat_output_opts_into_introduction_while_headless_output_does_not() {
    let temp = tempfile::tempdir().expect("tempdir");
    let chat = daemon_output_for_chat_session_in(
        temp.path(),
        "chat",
        HarnessStorageMode::MemoryOnly,
        SessionLaunchStatus::New,
    )
    .expect("chat output");
    let headless = daemon_output_for_session_in(
        temp.path(),
        "preview",
        HarnessStorageMode::MemoryOnly,
        SessionLaunchStatus::New,
    )
    .expect("headless output");
    assert!(chat.introduction_notice_eligible);
    assert!(!headless.introduction_notice_eligible);
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
