use clap::{CommandFactory, Parser};

use super::Cli;

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

/// The removed inspection command must neither parse nor appear in top-level
/// help, preventing stale scripts from mistaking legacy state for active
/// policy.
#[test]
fn policy_show_is_not_exposed() {
    assert!(Cli::try_parse_from(["tau", "policy-show"]).is_err());

    let help = Cli::command().render_long_help().to_string();
    assert!(!help.contains("policy-show"));
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
