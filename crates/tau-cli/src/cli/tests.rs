use clap::{CommandFactory, Parser};

use super::Cli;

/// The removed inspection command must neither parse nor appear in top-level
/// help, preventing stale scripts from mistaking legacy state for active
/// policy.
#[test]
fn policy_show_is_not_exposed() {
    assert!(Cli::try_parse_from(["tau", "policy-show"]).is_err());

    let help = Cli::command().render_long_help().to_string();
    assert!(!help.contains("policy-show"));
}
