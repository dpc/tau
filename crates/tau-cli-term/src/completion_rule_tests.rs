use super::{CommandCompletionMatch, CompletionRule, CompletionRuleKind, CompletionRules};

/// Parsed command configuration must produce the private nonempty argv form
/// while retaining the exact program, argument order, and surrounding token.
#[test]
fn parsed_command_rule_retains_nonempty_argv_and_token_surroundings() {
    let rules = CompletionRules::new(vec![
        CompletionRule::parse("#/", "complete_with_command fzf --filter snow")
            .expect("rule parses"),
    ]);
    let (argv, before, after) = rules
        .command_for_exact_token("ask #/ later", "ask #/".len())
        .expect("public exact command token");
    assert_eq!(argv, ["fzf", "--filter", "snow"]);
    assert_eq!(before, "ask ");
    assert_eq!(after, " later");

    let runtime_rules = rules.command_rules();
    let (command, before, after) = runtime_rules
        .command_for_exact_token("ask #/ later", "ask #/".len())
        .expect("private exact command token");
    let CommandCompletionMatch::Command(command) = command else {
        panic!("parsed command must be nonempty");
    };
    assert_eq!(command.program(), "fzf");
    assert_eq!(command.args(), ["--filter", "snow"]);
    assert_eq!(before, "ask ");
    assert_eq!(after, " later");
}

/// Public callers can still construct the legacy empty command vector, which
/// must reach the established fallback rather than becoming executable argv.
#[test]
fn public_empty_command_rule_retains_empty_fallback() {
    let rules = CompletionRules::new(vec![CompletionRule {
        prefix: "#/".to_owned(),
        kind: CompletionRuleKind::Command(Vec::new()),
    }]);
    let (argv, before, after) = rules
        .command_for_exact_token("ask #/ later", "ask #/".len())
        .expect("public empty command token");
    assert!(argv.is_empty());
    assert_eq!(before, "ask ");
    assert_eq!(after, " later");

    let runtime_rules = rules.command_rules();
    let (command, before, after) = runtime_rules
        .command_for_exact_token("ask #/ later", "ask #/".len())
        .expect("private empty command token still matches");
    assert!(matches!(command, CommandCompletionMatch::EmptyCommand));
    assert_eq!(before, "ask ");
    assert_eq!(after, " later");
}
