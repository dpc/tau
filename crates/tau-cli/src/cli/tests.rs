use std::process::ExitCode;

use clap::{CommandFactory, Parser};

use super::{Cli, Command, DevCommand};

/// Offline cache scopes parse typed identities, implemented views, and an
/// explicitly requested disposable index path.
#[test]
fn offline_cache_scopes_parse_with_current_views_and_private_index() {
    assert!(
        Cli::try_parse_from([
            "tau",
            "agent",
            "cache",
            "agent",
            "--include-descendants",
            "--format",
            "jsonl",
            "--prompt",
            "prompt"
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from([
            "tau",
            "session",
            "cache",
            "session",
            "--state-dir",
            "/tmp/state"
        ])
        .is_ok()
    );
    assert!(Cli::try_parse_from(["tau", "agent", "cache", "bad.id"]).is_err());
    assert!(
        Cli::try_parse_from(["tau", "agent", "cache", "agent", "--index", "/tmp/index"]).is_ok()
    );
    assert!(Cli::try_parse_from(["tau", "agent", "cache", "agent", "--view", "geometry"]).is_ok());
}

/// Useful partial evidence and incompatible sources retain distinct process
/// exit codes.
#[test]
fn cache_evidence_exit_codes_are_not_generic_failures() {
    assert_eq!(crate::CliError::CachePartial.exit_code(), ExitCode::from(3));
    assert_eq!(crate::CliError::CacheInvalid.exit_code(), ExitCode::from(2));
}

/// Agent-list and headless-send parse their session argument into the shared
/// durable identity before command dispatch, rejecting the same invalid
/// grammar.
#[test]
fn session_commands_parse_session_ids_at_the_clap_boundary() {
    let agent_list = Cli::parse_from(["tau", "agent", "list", "session_A-1"]);
    assert!(matches!(
        agent_list.command,
        Some(Command::Agent {
            command: super::AgentCommand::List(super::AgentListArgs { ref session_id, .. }),
        }) if session_id.as_str() == "session_A-1"
    ));

    let dev_send = Cli::parse_from(["tau", "dev", "send", "session_A-1", "hello"]);
    assert!(matches!(
        dev_send.command,
        Some(Command::Dev {
            command: DevCommand::Send { ref session_id, ref line },
        }) if session_id.as_str() == "session_A-1" && line.as_slice() == ["hello"]
    ));

    for command in [
        ["tau", "agent", "list", "bad.id"].as_slice(),
        ["tau", "dev", "send", "bad.id", "hello"].as_slice(),
    ] {
        let error = match Cli::try_parse_from(command) {
            Ok(_) => panic!("invalid session id must fail"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains(
                "invalid value 'bad.id' for '<SESSION_ID>': \
                 session id contains invalid byte 0x2e at byte offset 3"
            ),
            "{command:?} must preserve the SessionId diagnostic: {error}"
        );
    }
}

/// Ensures the hidden developer command exposes the papercut list, Markdown,
/// clear, and explicit-state-root interface without loading user configuration.
#[test]
fn papercut_developer_commands_parse_with_their_documented_options() {
    assert!(Cli::try_parse_from(["tau", "dev", "papercut", "list"]).is_ok());
    assert!(Cli::try_parse_from(["tau", "dev", "papercut", "list", "--markdown"]).is_ok());
    assert!(
        Cli::try_parse_from([
            "tau",
            "dev",
            "papercut",
            "clear",
            "--state-dir",
            "/tmp/tau-state",
        ])
        .is_ok()
    );

    let mut command = Cli::command();
    let dev = command
        .find_subcommand_mut("dev")
        .expect("developer command");
    let papercut = dev
        .find_subcommand_mut("papercut")
        .expect("papercut command");
    assert!(papercut.find_subcommand_mut("list").is_some());
    assert!(papercut.find_subcommand_mut("clear").is_some());
}

/// The public startup contract uses target-oriented subcommands and rejects the
/// removed flags so scripts cannot silently select the previous path.
#[test]
fn attach_and_resume_are_subcommands_only() {
    assert!(Cli::try_parse_from(["tau", "--attach"]).is_err());
    assert!(Cli::try_parse_from(["tau", "--resume", "s1"]).is_err());

    let attach = Cli::parse_from(["tau", "attach", "s1"]);
    assert!(matches!(
        attach.command,
        Some(super::Command::Attach { session: Some(ref id) }) if id == "s1"
    ));
    let resume = Cli::parse_from(["tau", "resume"]);
    assert!(matches!(
        resume.command,
        Some(super::Command::Resume { session: None })
    ));
}

/// The foreground server requires a fixed id and exactly one explicit lifecycle
/// mode so omission or contradictory provisioning intent cannot reach startup.
#[test]
fn serve_requires_exactly_one_explicit_session_mode() {
    assert!(Cli::try_parse_from(["tau", "serve", "--session", "s1"]).is_err());
    assert!(Cli::try_parse_from(["tau", "serve", "--existing"]).is_err());
    assert!(
        Cli::try_parse_from(["tau", "serve", "--session", "s1", "--create", "--existing",])
            .is_err()
    );
    assert!(
        Cli::try_parse_from([
            "tau",
            "serve",
            "--session",
            "s1",
            "--create",
            "--create-or-existing",
        ])
        .is_err()
    );
    assert!(
        Cli::try_parse_from([
            "tau",
            "serve",
            "--session",
            "s1",
            "--existing",
            "--create-or-existing",
        ])
        .is_err()
    );
    let parsed = Cli::parse_from(["tau", "serve", "--session", "s1", "--existing"]);
    assert!(matches!(
        parsed.command,
        Some(super::Command::Serve {
            session,
            create: false,
            existing: true,
            create_or_existing: false,
            bootstrap_prompt_file: None,
            bootstrap_id: None,
            mirror_extension_stderr: false,
        }) if session.as_str() == "s1"
    ));
    let parsed = Cli::parse_from(["tau", "serve", "--session", "s2", "--create"]);
    assert!(matches!(
        parsed.command,
        Some(super::Command::Serve {
            session,
            create: true,
            existing: false,
            create_or_existing: false,
            bootstrap_prompt_file: None,
            bootstrap_id: None,
            mirror_extension_stderr: false,
        }) if session.as_str() == "s2"
    ));
    let parsed = Cli::parse_from(["tau", "serve", "--session", "s3", "--create-or-existing"]);
    assert!(matches!(
        parsed.command,
        Some(super::Command::Serve {
            session,
            create: false,
            existing: false,
            create_or_existing: true,
            bootstrap_prompt_file: None,
            bootstrap_id: None,
            mirror_extension_stderr: false,
        }) if session.as_str() == "s3"
    ));
}

/// Extension stderr mirroring is default-off and accepted only by fixed-session
/// serve rather than root, interactive, attach, resume, or component modes.
#[test]
fn extension_stderr_mirror_flag_is_serve_only_and_opt_in() {
    let parsed = Cli::parse_from([
        "tau",
        "serve",
        "--session",
        "s1",
        "--existing",
        "--mirror-extension-stderr",
    ]);
    assert!(matches!(
        parsed.command,
        Some(super::Command::Serve {
            mirror_extension_stderr: true,
            ..
        })
    ));
    for command in [
        vec!["tau", "--mirror-extension-stderr"],
        vec!["tau", "resume", "--mirror-extension-stderr"],
        vec!["tau", "attach", "s1", "--mirror-extension-stderr"],
        vec!["tau", "component", "harness", "--mirror-extension-stderr"],
        vec!["tau", "--prompt-stdin", "--mirror-extension-stderr"],
        vec!["tau", "--ephemeral", "--mirror-extension-stderr"],
    ] {
        assert!(
            Cli::try_parse_from(command).is_err(),
            "flag unexpectedly accepted outside serve"
        );
    }
}

/// Serve bootstrap options are paired and accept only the durable id grammar.
#[test]
fn serve_bootstrap_requires_paired_source_and_valid_id() {
    let base = ["tau", "serve", "--session", "s1", "--existing"];
    assert!(
        Cli::try_parse_from(
            base.into_iter()
                .chain(["--bootstrap-prompt-file", "prompt.md"])
        )
        .is_err()
    );
    assert!(
        Cli::try_parse_from(base.into_iter().chain(["--bootstrap-id", "telegram-v1"])).is_err()
    );
    assert!(
        Cli::try_parse_from(base.into_iter().chain([
            "--bootstrap-prompt-file",
            "-",
            "--bootstrap-id",
            "telegram_v1",
        ]))
        .is_ok()
    );
    assert!(
        Cli::try_parse_from(base.into_iter().chain([
            "--bootstrap-prompt-file",
            "-",
            "--bootstrap-id",
            "contains.dot",
        ]))
        .is_err()
    );
}

/// Startup options remain root-owned: callers place them before the target
/// subcommand, and omitted resume targets still preserve resume mode.
#[test]
fn targeted_startup_keeps_root_option_placement() {
    let parsed = Cli::parse_from(["tau", "--ephemeral", "resume"]);
    assert!(parsed.run.ephemeral);
    assert!(matches!(
        parsed.command,
        Some(super::Command::Resume { session: None })
    ));
    assert!(Cli::try_parse_from(["tau", "resume", "--ephemeral"]).is_err());
    for option in [
        "--role=engineer",
        "-r=engineer",
        "--profile=focused",
        "--harness-config=agents.default_role=engineer",
        "--provider-alias=current=codex-work",
        "--model-alias=current=gpt-5.5",
        "--enable-role=engineer",
        "--disable-role=engineer",
        "--disable-roles-all",
        "--enable-extensions-all",
        "--disable-extensions-all",
        "--enable-extension=core-shell",
        "--disable-extension=core-shell",
    ] {
        assert!(
            Cli::try_parse_from(["tau", "resume", option]).is_err(),
            "{option} must remain root-only"
        );
    }
}

/// Dedicated alias flags parse as typed, repeatable root startup options and
/// preserve provider dots plus model-name slashes.
#[test]
fn provider_and_model_alias_flags_are_typed_and_repeatable() {
    let parsed = Cli::parse_from([
        "tau",
        "--provider-alias",
        "current=codex.work",
        "--provider-alias",
        "current=codex-personal",
        "--model-alias",
        "fast=org/qwen-fast",
    ]);

    assert_eq!(parsed.harness.provider_alias.len(), 2);
    assert_eq!(
        parsed.harness.provider_alias[0].to.to_string(),
        "codex.work"
    );
    assert_eq!(
        parsed.harness.model_alias[0].to.to_string(),
        "org/qwen-fast"
    );
    for invalid in [
        ["tau", "--provider-alias", "missing"],
        ["tau", "--provider-alias", "=target"],
        ["tau", "--provider-alias", "name="],
        ["tau", "--provider-alias", "bad/name=target"],
        ["tau", "--model-alias", "name="],
    ] {
        assert!(
            Cli::try_parse_from(invalid).is_err(),
            "{invalid:?} must fail"
        );
    }
}

/// Ensures the reclaimed `-r` spelling selects the same root-owned role as
/// `--role`, without changing the `resume` subcommand or allowing root options
/// after it.
#[test]
fn short_role_option_matches_long_form_without_reclaiming_resume() {
    let long = Cli::parse_from(["tau", "--role", "engineer", "resume"]);
    let short = Cli::parse_from(["tau", "-r", "engineer", "resume"]);

    assert_eq!(short.harness.role, long.harness.role);
    assert!(matches!(
        short.command,
        Some(super::Command::Resume { session: None })
    ));
    assert!(Cli::try_parse_from(["tau", "resume", "-r", "engineer"]).is_err());
    assert!(Cli::try_parse_from(["tau", "--role", "engineer", "-r", "reviewer"]).is_err());
}

/// Ensures root help advertises the short role spelling after `-r` stopped
/// denoting the removed legacy resume flag.
#[test]
fn root_help_documents_short_role_option() {
    let help = Cli::command().render_long_help().to_string();

    assert!(help.contains("-r, --role <ROLE>"));
}

/// Agent-unload help states its durable-only, non-destructive, indeterminate
/// boundary.
#[test]
fn agent_unload_help_documents_operator_safety_contract() {
    let mut command = Cli::command();
    let unload = command
        .find_subcommand_mut("agent")
        .expect("agent command")
        .find_subcommand_mut("unload")
        .expect("unload command");
    let help = unload.render_long_help().to_string();
    assert!(help.contains("Only durable agents are supported"));
    assert!(help.contains("without deleting its transcript or session history"));
    assert!(help.contains("indeterminate"));
    assert!(help.contains("retrying the same session and agent is safe"));
}
