//! Tests for cli parsing behavior.

use super::*;

/// Cache inspection accepts only absolute RFC3339 time bounds and closed
/// comma-separated geometry grouping dimensions.
#[test]
fn cache_selection_controls_parse_without_provider_or_daemon_state() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "agent",
        "cache",
        "agent-root",
        "--since",
        "2026-09-06T01:02:03Z",
        "--until",
        "2026-09-06T02:03:04+00:00",
        "--model",
        "provider/model",
        "--operation",
        "standalone-compaction",
        "--attempt",
        "2",
        "--require-exact-chain",
        "--group-by",
        "backend,controls",
    ]);
    assert!(matches!(
        cli.command,
        Some(super::super::cli::Command::Agent {
            command: super::super::cli::AgentCommand::Cache(args),
        }) if args.options.since.is_some()
            && args.options.until.is_some()
            && args.options.model.as_deref() == Some("provider/model")
            && matches!(
                args.options.operation,
                Some(super::super::cli::CacheOperation::StandaloneCompaction)
            )
            && args.options.attempt == Some(2)
            && args.options.require_exact_chain
            && args.options.group_by.len() == 2
    ));
    assert!(
        path_super_cli::Cli::try_parse_from([
            "tau",
            "agent",
            "cache",
            "agent-root",
            "--since",
            "yesterday",
        ])
        .is_err()
    );
    assert_eq!(
        super::super::cli::parse_cache_since("1970-01-01T00:00:00.000000001Z")
            .expect("lower bound"),
        1
    );
    assert_eq!(
        super::super::cli::parse_cache_until("1970-01-01T00:00:00.000000999Z")
            .expect("upper bound"),
        0
    );
}

#[test]
fn role_cli_overrides_preserve_argument_order() {
    let overrides = super::super::parse_role_cli_overrides([
        "tau",
        "--disable-role",
        "manager",
        "--disable-roles-all",
        "--enable-role=manager",
        "--enable-role",
        "engineer",
    ]);

    assert_eq!(
        overrides,
        vec![
            tau_config::settings::RoleCliOverride::Disable("manager".to_owned()),
            tau_config::settings::RoleCliOverride::DisableAll,
            tau_config::settings::RoleCliOverride::Enable("manager".to_owned()),
            tau_config::settings::RoleCliOverride::Enable("engineer".to_owned()),
        ]
    );
}
/// Prevents `--ephemeral` from becoming a misleading modifier for an already
/// running or persisted session, where the new process cannot guarantee a clean
/// session-persistence boundary.
#[test]
fn run_rejects_ephemeral_with_attach_or_resume() {
    assert!(
        super::super::reject_ephemeral_incompatible(true, &super::super::StartupMode::Attach(None))
            .is_err()
    );
    assert!(
        super::super::reject_ephemeral_incompatible(true, &super::super::StartupMode::Resume(None))
            .is_err()
    );
    assert!(
        super::super::reject_ephemeral_incompatible(true, &super::super::StartupMode::New).is_ok()
    );
}

#[test]
fn extension_cli_overrides_preserve_argument_order() {
    let overrides = super::super::parse_extension_cli_overrides([
        "tau",
        "--disable-extension",
        "core-shell",
        "--enable-extensions-all",
        "--disable-extensions-all",
        "--enable-extension=std-websearch",
    ]);

    assert_eq!(
        overrides,
        vec![
            tau_config::settings::ExtensionCliOverride::Disable("core-shell".to_owned()),
            tau_config::settings::ExtensionCliOverride::EnableAll,
            tau_config::settings::ExtensionCliOverride::DisableAll,
            tau_config::settings::ExtensionCliOverride::Enable("std-websearch".to_owned()),
        ]
    );
}
/// Session inspection operations share the same noun-first nested command shape
/// as agent inspection.
#[test]
fn session_commands_parse_nested_operations() {
    let list = path_super_cli::Cli::parse_from(["tau", "session", "list"]);
    assert!(matches!(
        list.command,
        Some(super::super::cli::Command::Session {
            command: super::super::cli::SessionCommand::List(args),
        })
            if args.dir.is_none() && !args.json
    ));

    let show = path_super_cli::Cli::parse_from(["tau", "session", "show", "--session-id", "s1"]);
    assert!(matches!(
        show.command,
        Some(super::super::cli::Command::Session {
            command: super::super::cli::SessionCommand::Show { session_id, .. },
        }) if session_id.as_str() == "s1"
    ));

    let kill = path_super_cli::Cli::parse_from(["tau", "session", "kill", "s1"]);
    assert!(matches!(
        kill.command,
        Some(super::super::cli::Command::Session {
            command: super::super::cli::SessionCommand::Kill { session_id },
        }) if session_id.as_str() == "s1"
    ));
}
/// Missing paths and non-directory paths fail in clap's exit-2 value-validation
/// path instead of becoming successful empty list filters.
#[test]
fn session_list_rejects_invalid_directories_during_parsing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let file = temp.path().join("file");
    std::fs::write(&file, b"not a directory").expect("test file");
    for invalid in [temp.path().join("missing"), file] {
        let error = match path_super_cli::Cli::try_parse_from([
            path_std_ffi::OsStr::new("tau"),
            path_std_ffi::OsStr::new("session"),
            path_std_ffi::OsStr::new("list"),
            path_std_ffi::OsStr::new("--dir"),
            invalid.as_os_str(),
        ]) {
            Ok(_) => panic!("invalid directory should be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        assert_eq!(error.exit_code(), 2);
    }
}

/// An inaccessible directory is an invalid filter rather than an apparent
/// successful absence result.
#[test]
fn session_list_rejects_inaccessible_directory_during_parsing() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().expect("tempdir");
    let private = temp.path().join("private");
    let directory = private.join("project");
    std::fs::create_dir_all(&directory).expect("private project");
    std::fs::set_permissions(&private, path_std_fs::Permissions::from_mode(0o000))
        .expect("remove directory access");

    let result = path_super_cli::Cli::try_parse_from([
        path_std_ffi::OsStr::new("tau"),
        path_std_ffi::OsStr::new("session"),
        path_std_ffi::OsStr::new("list"),
        path_std_ffi::OsStr::new("--dir"),
        directory.as_os_str(),
    ]);

    std::fs::set_permissions(&private, path_std_fs::Permissions::from_mode(0o700))
        .expect("restore directory access");
    let error = match result {
        Ok(_) => panic!("inaccessible directory should be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    assert_eq!(error.exit_code(), 2);
}

/// The legacy `--config` flag must not be silently ignored because that makes
/// harness startup appear to use a config file that was never loaded.
#[test]
fn legacy_config_path_is_rejected() {
    let cli = path_super_cli::Cli::parse_from(["tau", "--config", "legacy.json"]);
    let err = super::super::reject_legacy_config_path(cli.run.config.as_deref())
        .expect_err("legacy config path should fail");

    assert!(err.to_string().contains("--config is no longer supported"));

    let non_run_cli =
        path_super_cli::Cli::parse_from(["tau", "--config", "legacy.json", "session", "list"]);
    let non_run_err = super::super::reject_legacy_config_path(non_run_cli.run.config.as_deref())
        .expect_err("legacy config path should fail before non-run dispatch");
    assert!(
        non_run_err
            .to_string()
            .contains("--config is no longer supported")
    );

    let explicit_run_cli =
        path_super_cli::Cli::parse_from(["tau", "run", "--config", "legacy.json"]);
    let Some(path_super_cli::Command::Run(explicit_run)) = explicit_run_cli.command else {
        panic!("expected explicit run command");
    };
    let explicit_run_err = super::super::reject_legacy_config_path(explicit_run.config.as_deref())
        .expect_err("legacy config path should fail before explicit run dispatch");
    assert!(
        explicit_run_err
            .to_string()
            .contains("--config is no longer supported")
    );
}

/// Dispatch validation rejects full detail for every noncompact encoding with
/// the tailored diagnostic while accepting both compact encodings.
#[test]
fn agent_trace_mode_validation_matches_format_semantics() {
    for format in [
        path_super_cli::AgentTraceFormat::TauJsonl,
        path_super_cli::AgentTraceFormat::OtlpJson,
        path_super_cli::AgentTraceFormat::AgentPerformanceJsonl,
    ] {
        let error =
            super::super::validate_agent_trace_mode(format, path_super_cli::AgentTraceMode::Full)
                .expect_err("full noncompact trace");
        assert_eq!(
            error.to_string(),
            "participant error: `agent trace --mode full` requires `--format \
             agent-tools-jsonl` or `--format agent-tools-toon`"
        );
    }
    for format in [
        path_super_cli::AgentTraceFormat::AgentToolsJsonl,
        path_super_cli::AgentTraceFormat::AgentToolsToon,
    ] {
        super::super::validate_agent_trace_mode(format, path_super_cli::AgentTraceMode::Full)
            .expect("full compact trace");
    }
}

/// Ensures `--profile` keeps its root-owned ordered selection syntax through
/// command parsing before startup forwards it to the harness.
#[test]
fn profile_flag_parses_as_root_harness_selection() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "--profile",
        "focused,review",
        "--role",
        "engineer",
        "dev",
        "print-prompt",
    ]);
    assert_eq!(cli.harness.profile.as_deref(), Some("focused,review"));
    assert_eq!(cli.harness.role.as_deref(), Some("engineer"));
}

/// Ensures the root run command accepts `--ephemeral` as an explicit
/// session-persistence mode without affecting the separate attach/resume flags.
#[test]
fn run_parses_ephemeral_flag() {
    let cli = path_super_cli::Cli::parse_from(["tau", "--ephemeral"]);
    assert!(cli.run.ephemeral);
    assert!(cli.command.is_none());
}

/// Session-list filtering canonicalizes symlink aliases during parsing and
/// composes independently with structured output.
#[test]
fn session_list_parses_canonical_directory_and_json() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    let alias = temp.path().join("alias");
    std::fs::create_dir(&project).expect("project directory");
    path_std_os_unix::fs::symlink(&project, &alias).expect("project symlink");

    let cli = path_super_cli::Cli::try_parse_from([
        path_std_ffi::OsStr::new("tau"),
        path_std_ffi::OsStr::new("session"),
        path_std_ffi::OsStr::new("list"),
        path_std_ffi::OsStr::new("--dir"),
        alias.as_os_str(),
        path_std_ffi::OsStr::new("--json"),
    ])
    .expect("session list arguments");

    assert!(matches!(
        cli.command,
        Some(super::super::cli::Command::Session {
            command: super::super::cli::SessionCommand::List(args),
        }) if args.dir.as_deref() == Some(project.as_path()) && args.json
    ));
}

/// Ensures the supported environment list is documented in long help together
/// with its grammar and precedence relative to CLI overrides.
#[test]
fn long_help_documents_extension_environment() {
    use clap::CommandFactory;
    let mut output = Vec::new();
    path_super_cli::Cli::command()
        .write_long_help(&mut output)
        .expect("render long help");
    let help = String::from_utf8(output).expect("help is UTF-8");
    assert!(help.contains("TAU_ENABLE_EXTENSIONS=NAME[,NAME...]"));
    assert!(help.contains("CLI enable/disable flags win"));
}

/// Ensures only commands that launch a harness or render from harness settings
/// validate a fresh snapshot. Attach and local/IPC inspection must remain
/// usable after an already-running daemon's source configuration becomes
/// invalid.
#[test]
fn harness_settings_validation_is_limited_to_config_consumers() {
    let run_args = || path_super_cli::RunArgs {
        config: None,
        prompt_stdin: false,
        ephemeral: false,
    };
    assert!(!super::super::consumes_harness_settings(
        &super::super::DispatchCommand::Startup {
            args: run_args(),
            mode: super::super::StartupMode::Attach(None),
        }
    ));
    assert!(super::super::consumes_harness_settings(
        &super::super::DispatchCommand::Startup {
            args: run_args(),
            mode: super::super::StartupMode::New,
        }
    ));
    assert!(super::super::consumes_harness_settings(
        &super::super::DispatchCommand::Startup {
            args: run_args(),
            mode: super::super::StartupMode::Resume(None),
        }
    ));
    assert!(!super::super::consumes_harness_settings(
        &super::super::DispatchCommand::Other(path_super_cli::Command::Session {
            command: path_super_cli::SessionCommand::List(Default::default()),
        })
    ));
    assert!(!super::super::consumes_harness_settings(
        &super::super::DispatchCommand::Other(path_super_cli::Command::Dev {
            command: path_super_cli::DevCommand::Send {
                session_id: "s1".parse().expect("valid session id"),
                line: vec!["hello".to_owned()],
            },
        })
    ));
    assert!(super::super::consumes_harness_settings(
        &super::super::DispatchCommand::Other(path_super_cli::Command::Dev {
            command: path_super_cli::DevCommand::PrintSystemPrompt,
        })
    ));
}

/// Ensures the hidden tmux helper parses isolated scratch/workdir startup
/// options, because future manual E2E sessions must not accidentally inherit
/// the user's real Tau state.
#[test]
fn dev_tmux_start_parses_isolated_startup_options() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "dev",
        "tmux",
        "start",
        "--scratch-root",
        "/tmp/tau-e2e-test",
        "--session",
        "manual",
        "--tau-bin",
        "target/debug/tau",
        "--workdir",
        "/tmp/tau-e2e-test/work",
        "--width",
        "100",
        "--height",
        "30",
    ]);

    assert!(matches!(
        cli.command,
        Some(super::super::cli::Command::Dev {
            command: super::super::cli::DevCommand::Tmux {
                command: super::super::cli::DevTmuxCommand::Start(args),
            },
        }) if args.common.scratch_root == Some(std::path::PathBuf::from("/tmp/tau-e2e-test"))
            && args.common.session == "manual"
            && args.tau_bin == Some(std::path::PathBuf::from("target/debug/tau"))
            && args.workdir == Some(std::path::PathBuf::from("/tmp/tau-e2e-test/work"))
            && args.width == 100
            && args.height == 30
    ));
}

/// Ensures `start` can omit its scratch root so the helper can generate a fresh
/// temporary root, and accepts the shorter `--root` spelling when a caller
/// needs to select the root explicitly.
#[test]
fn dev_tmux_start_accepts_generated_root_and_root_alias() {
    let generated = path_super_cli::Cli::parse_from(["tau", "dev", "tmux", "start"]);
    assert!(matches!(
        generated.command,
        Some(super::super::cli::Command::Dev {
            command: super::super::cli::DevCommand::Tmux {
                command: super::super::cli::DevTmuxCommand::Start(args),
            },
        }) if args.common.scratch_root.is_none()
    ));

    let explicit = path_super_cli::Cli::parse_from([
        "tau",
        "dev",
        "tmux",
        "start",
        "--root",
        "/tmp/tau-e2e-test",
    ]);
    assert!(matches!(
        explicit.command,
        Some(super::super::cli::Command::Dev {
            command: super::super::cli::DevCommand::Tmux {
                command: super::super::cli::DevTmuxCommand::Start(args),
            },
        }) if args.common.scratch_root == Some(std::path::PathBuf::from("/tmp/tau-e2e-test"))
    ));
}

/// Ensures `send` keeps prompt text as a trailing argument vector and exposes a
/// no-enter mode, protecting the manual workflow's ability to paste slash
/// commands or partial prompts before submitting them.
#[test]
fn dev_tmux_send_parses_literal_text_and_enter_toggle() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "dev",
        "tmux",
        "send",
        "--scratch-root",
        "/tmp/tau-e2e-test",
        "--no-enter",
        "--",
        ":version",
        "with spaces",
    ]);

    assert!(matches!(
        cli.command,
        Some(super::super::cli::Command::Dev {
            command: super::super::cli::DevCommand::Tmux {
                command: super::super::cli::DevTmuxCommand::Send(args),
            },
        }) if args.target.common.scratch_root == Some(std::path::PathBuf::from("/tmp/tau-e2e-test"))
            && args.no_enter
            && args.text == vec![":version".to_owned(), "with spaces".to_owned()]
    ));
}

/// Session inspection rejects controlled identifiers during argument parsing,
/// before either command can inspect or create a missing sessions root.
#[test]
fn session_show_and_stats_reject_invalid_ids_with_missing_roots() {
    for command in ["show", "stats"] {
        let id_flag = if command == "show" {
            "--session-id"
        } else {
            "--session"
        };
        let error = match path_super_cli::Cli::try_parse_from([
            "tau",
            "session",
            command,
            id_flag,
            "bad.id",
            "--sessions-dir",
            "/definitely/missing/tau-sessions",
        ]) {
            Ok(_) => panic!("invalid session id must be rejected"),
            Err(error) => error,
        };
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("bad.id"));
        assert!(diagnostic.contains("session id contains invalid byte"));
    }
}

#[test]
fn harness_config_flags_parse_repeated_and_global() {
    let overrides = super::super::parse_harness_config_cli_overrides([
        "tau",
        "--harness-config=extensions.core-shell.config.working_directory=/foo",
        "dev",
        "print-prompt",
        "--harness-config=session_retention_days=3",
    ])
    .expect("parse overrides");

    assert_eq!(
        overrides,
        vec![
            tau_config::settings::HarnessConfigCliOverride {
                key: "extensions.core-shell.config.working_directory".to_owned(),
                raw_value: "/foo".to_owned(),
            },
            tau_config::settings::HarnessConfigCliOverride {
                key: "session_retention_days".to_owned(),
                raw_value: "3".to_owned(),
            },
        ]
    );
}
#[test]
fn harness_config_overrides_reject_attach_only_paths() {
    let overrides = [tau_config::settings::HarnessConfigCliOverride {
        key: "session_retention_days".to_owned(),
        raw_value: "3".to_owned(),
    }];

    let err = super::super::reject_harness_config_overrides(&overrides, "attach")
        .expect_err("attach cannot apply overrides");
    assert!(err.to_string().contains("starting a new harness instance"));
}

/// Alias rejection preserves the exact dedicated flag or environment source
/// instead of misdiagnosing it as a generic harness-config override.
#[test]
fn alias_inputs_reject_non_startup_commands_with_source_identity() {
    let cases = [
        ((true, false, false, false), "--provider-alias"),
        ((false, true, false, false), "--model-alias"),
        (
            (false, false, true, false),
            tau_config::settings::TAU_PROVIDER_ALIASES_ENV,
        ),
        (
            (false, false, false, true),
            tau_config::settings::TAU_MODEL_ALIASES_ENV,
        ),
    ];
    for ((provider_flag, model_flag, provider_env, model_env), expected) in cases {
        let error = super::super::reject_model_reference_alias_inputs(
            super::super::ModelReferenceAliasInputPresence {
                provider_flag,
                model_flag,
                provider_environment: provider_env,
                model_environment: model_env,
            },
            "attach",
        )
        .expect_err("non-startup command must reject alias input");
        assert!(error.to_string().contains(expected), "{error}");
        assert!(!error.to_string().contains("--harness-config"), "{error}");
    }
}

/// Attach mode connects to an existing daemon, so explicit startup
/// role/extension/profile overrides must fail instead of pretending to
/// reconfigure that daemon.
#[test]
fn attach_rejects_startup_overrides_that_existing_daemon_cannot_apply() {
    let role_overrides = [path_tau_config_settings::RoleCliOverride::Enable(
        "manager".to_owned(),
    )];
    let extension_overrides = [path_tau_config_settings::ExtensionCliOverride::Disable(
        "core-shell".to_owned(),
    )];

    let role_err =
        super::super::reject_attach_startup_overrides(false, false, Some("manager"), &[], &[])
            .expect_err("interactive attach role should fail");
    assert!(role_err.to_string().contains("cannot apply --role"));

    let role_override_err =
        super::super::reject_attach_startup_overrides(false, false, None, &role_overrides, &[])
            .expect_err("attach role overrides should fail");
    assert!(
        role_override_err
            .to_string()
            .contains("role enable/disable")
    );

    let extension_override_err = super::super::reject_attach_startup_overrides(
        false,
        false,
        None,
        &[],
        &extension_overrides,
    )
    .expect_err("attach extension overrides should fail");
    assert!(
        extension_override_err
            .to_string()
            .contains("extension enable/disable")
    );

    super::super::reject_attach_startup_overrides(true, false, Some("manager"), &[], &[])
        .expect("prompt-stdin uses --role for the submitted prompt");

    let profile_error = super::super::reject_attach_startup_overrides(false, true, None, &[], &[])
        .expect_err("attach cannot apply explicitly selected profile");
    assert!(profile_error.to_string().contains("cannot apply --profile"));
}

#[test]
fn harness_config_flag_requires_key_value() {
    let err = match path_super_cli::Cli::try_parse_from(["tau", "--harness-config=missing-equals"])
    {
        Ok(_) => panic!("missing KEY=VALUE must fail"),
        Err(err) => err,
    };

    assert!(err.to_string().contains("expected KEY=VALUE"));
}

#[test]
fn global_harness_flags_parse_before_dev_print_prompt() {
    // Hidden diagnostic commands use the same global harness args as normal
    // startup, including flags placed before the `dev` subcommand.
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "--disable-roles-all",
        "--role",
        "manager",
        "dev",
        "print-prompt",
    ]);

    assert_eq!(cli.harness.role_overrides.disable_roles_all, 1);
    assert_eq!(cli.harness.role.as_deref(), Some("manager"));
    assert!(matches!(
        cli.command,
        Some(super::super::cli::Command::Dev {
            command: super::super::cli::DevCommand::PrintPrompt {
                enable_agents_md: true
            },
        })
    ));
}

#[test]
fn role_cli_flags_accept_repeated_and_mixed_options() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "--disable-roles-all",
        "--enable-role",
        "manager",
        "--disable-role",
        "engineer",
        "--disable-roles-all",
    ]);

    assert_eq!(cli.harness.role_overrides.disable_roles_all, 2);
    assert_eq!(cli.harness.role_overrides.enable_role, vec!["manager"]);
    assert_eq!(cli.harness.role_overrides.disable_role, vec!["engineer"]);
}

#[test]
fn extension_cli_flags_accept_repeated_and_mixed_options() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "--enable-extensions-all",
        "--disable-extension",
        "core-shell",
        "--enable-extension",
        "std-websearch",
        "--disable-extensions-all",
    ]);

    assert_eq!(cli.harness.extension_overrides.enable_extensions_all, 1);
    assert_eq!(cli.harness.extension_overrides.disable_extensions_all, 1);
    assert_eq!(
        cli.harness.extension_overrides.enable_extension,
        vec!["std-websearch"]
    );
    assert_eq!(
        cli.harness.extension_overrides.disable_extension,
        vec!["core-shell"]
    );
}

/// Proves the outer `tau dev tmux` dispatcher refuses startup overrides that
/// would require normal harness configuration validation before the helper has
/// switched into its scratch HOME/XDG environment.
#[test]
fn dev_tmux_rejects_startup_overrides_before_harness_validation() {
    let role_error =
        super::super::reject_dev_tmux_startup_overrides(None, Some("manager"), &[], &[], &[])
            .expect_err("--role refused");
    assert!(role_error.to_string().contains("cannot use --role"));

    let extension_error = super::super::reject_dev_tmux_startup_overrides(
        None,
        None,
        &[],
        &[path_tau_config_settings::ExtensionCliOverride::DisableAll],
        &[],
    )
    .expect_err("extension override refused");
    assert!(
        extension_error
            .to_string()
            .contains("cannot use extension enable/disable overrides")
    );
}

/// The shared version label must use the exact build metadata string in both
/// command modes, rather than accepting arbitrary text between its parentheses.
#[test]
fn runtime_version_label_matches_cli_version_shape() {
    let label = super::super::version_label();
    let expected = match super::super::build_last_modified() {
        Some(date) => format!(
            "tau {} ({}, {date})",
            env!("CARGO_PKG_VERSION"),
            super::super::build_revision()
        ),
        None => format!(
            "tau {} ({})",
            env!("CARGO_PKG_VERSION"),
            super::super::build_revision()
        ),
    };
    assert_eq!(label, expected);

    let (_term, handle, vt) = setup(100, 24);
    handle.print_output(
        "system-info",
        tau_cli_term::resolve::themed_block(
            &cli_test_theme(),
            tau_themes::names::SYSTEM_INFO,
            format!("{}{}", transcript_markers::NOTICE, label),
        ),
    );
    sync(&handle);
    assert_eq!(
        vt.screen_text(100)
            .join("\n")
            .matches(label.as_str())
            .count(),
        1,
        ":version command feedback must render the shared label exactly once"
    );
}

/// Ensures the resolved profile stack follows the session-directory status
/// while absent selection does not add a synthetic startup line.
#[test]
fn session_directory_status_reports_only_selected_startup_profile_stacks() {
    let session_dir = HarnessSessionDir {
        session_id: test_session_id("tau-agent-test"),
        path: "/tmp/tau-agent-test".into(),
        status: SessionDirStatus::New,
    };

    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.set_startup_profile_selection(Some(
        path_tau_config_settings::ProfileSelection::parse("focused,review")
            .expect("profile selection"),
    ));
    renderer.handle(&Event::HarnessSessionDir(session_dir.clone()));
    sync(&handle);
    let named_lines = visible_lines(&vt, 100);
    let session_line = named_lines
        .iter()
        .position(|line| line.contains("▤ session dir:"))
        .expect("session directory status");
    assert_eq!(
        named_lines
            .get(session_line + 1)
            .map(|line| line.trim_end()),
        Some("▤ config profile stack: focused,review")
    );

    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::HarnessSessionDir(session_dir));
    sync(&handle);
    assert!(!vt.screen_contains(100, "config profile stack:"));
}

/// Public agent-list flags parse additively around one required session id.
#[test]
fn list_agents_command_parses_filters() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "agent",
        "list",
        "session-1",
        "--include-suspended",
        "--include-unloaded",
    ]);

    assert!(matches!(
        cli.command,
        Some(super::super::cli::Command::Agent {
            command: super::super::cli::AgentCommand::List(args),
        })
            if args.session_id == "session-1"
                && args.include_suspended
                && args.include_unloaded
                && !args.include_unavailable
                && !args.all
    ));
}

/// The one-shot unload command requires exact session and agent identifiers.
#[test]
fn unload_agent_command_parses_target() {
    // ast-grep-ignore: limit-rust-symbol-path-depth
    let cli = crate::cli::Cli::try_parse_from(["tau", "agent", "unload", "session-1", "agent-1"])
        .expect("valid unload command");
    assert!(matches!(
        cli.command,
        Some(crate::cli::Command::Agent {
            command: crate::cli::AgentCommand::Unload(crate::cli::AgentUnloadArgs {
                ref session_id,
                ref agent_id,
            }),
        }) if session_id.as_str() == "session-1" && agent_id.as_str() == "agent-1"
    ));
}

/// Agent trace defaults select the compact TOON lite overview and the ordinary
/// state-directory journal root.
#[test]
fn agent_trace_command_parses_defaults() {
    let cli = path_super_cli::Cli::parse_from(["tau", "agent", "trace", "agent-root"]);

    assert!(matches!(
        cli.command,
        Some(super::super::cli::Command::Agent {
            command: super::super::cli::AgentCommand::Trace(args),
        })
            if args.agent_id.as_str() == "agent-root"
                && !args.include_descendants
                && args.format == super::super::cli::AgentTraceFormat::AgentToolsToon
                && args.mode == super::super::cli::AgentTraceMode::Lite
                && args.agents_dir == tau_session_inspect::default_agents_dir()
    ));
}

/// Agent trace accepts the lossy OTLP adapter, descendant workflow inclusion,
/// and an explicit offline journal root together.
#[test]
fn agent_trace_command_parses_all_options() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "agent",
        "trace",
        "agent-root",
        "--include-descendants",
        "--format",
        "otlp-json",
        "--mode",
        "lite",
        "--agents-dir",
        "/tmp/agents",
    ]);

    assert!(matches!(
        cli.command,
        Some(super::super::cli::Command::Agent {
            command: super::super::cli::AgentCommand::Trace(args),
        })
            if args.include_descendants
                && args.format == super::super::cli::AgentTraceFormat::OtlpJson
                && args.mode == super::super::cli::AgentTraceMode::Lite
                && args.agents_dir == std::path::Path::new("/tmp/agents")
    ));
}

/// Agent trace accepts both compact encodings, defaults their detail to lite,
/// and parses explicit full detail independently from encoding.
#[test]
fn agent_trace_command_parses_compact_formats_and_modes() {
    for (name, expected) in [
        (
            "agent-tools-jsonl",
            path_super_cli::AgentTraceFormat::AgentToolsJsonl,
        ),
        (
            "agent-tools-toon",
            path_super_cli::AgentTraceFormat::AgentToolsToon,
        ),
    ] {
        let cli = path_super_cli::Cli::parse_from([
            "tau",
            "agent",
            "trace",
            "agent-root",
            "--format",
            name,
        ]);

        assert!(matches!(
            cli.command,
            Some(super::super::cli::Command::Agent {
                command: super::super::cli::AgentCommand::Trace(args),
            }) if args.format == expected && args.mode == super::super::cli::AgentTraceMode::Lite
        ));
    }

    let full = path_super_cli::Cli::parse_from([
        "tau",
        "agent",
        "trace",
        "agent-root",
        "--format",
        "agent-tools-toon",
        "--mode",
        "full",
    ]);
    assert!(matches!(
        full.command,
        Some(super::super::cli::Command::Agent {
            command: super::super::cli::AgentCommand::Trace(args),
        }) if args.format == super::super::cli::AgentTraceFormat::AgentToolsToon
            && args.mode == super::super::cli::AgentTraceMode::Full
    ));
}

/// Agent trace exposes the content-free performance JSONL projection without a
/// payload-detail mode.
#[test]
fn agent_trace_command_parses_performance_format() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "agent",
        "trace",
        "agent-root",
        "--format",
        "agent-performance-jsonl",
    ]);

    assert!(matches!(
        cli.command,
        Some(super::super::cli::Command::Agent {
            command: super::super::cli::AgentCommand::Trace(args),
        }) if args.format == super::super::cli::AgentTraceFormat::AgentPerformanceJsonl
            && args.mode == super::super::cli::AgentTraceMode::Lite
    ));
}

#[test]
fn prompt_stdin_flag_is_parsed_for_default_run() {
    // `--prompt-stdin` keeps the normal harness/session args but replaces the
    // terminal UI with the one-shot stdin client.
    let cli = path_super_cli::Cli::parse_from(["tau", "--role", "manager", "--prompt-stdin"]);

    assert!(cli.run.prompt_stdin);
    assert_eq!(cli.harness.role.as_deref(), Some("manager"));
}
