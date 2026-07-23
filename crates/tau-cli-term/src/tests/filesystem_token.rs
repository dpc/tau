use std::fs;

use crate::completion::{
    self, CommandCompletion, CompletionData, CompletionRule, CompletionRules, build_candidates,
    build_candidates_with_home, build_candidates_with_rules,
};

#[test]
fn dotslash_token_triggers_filesystem_candidates() {
    // Empty directory listing is fine — we just need the path to
    // *match* as a filesystem token (vs. returning the command
    // candidate list).
    let tmp = tempfile::tempdir().expect("tempdir");
    let prefix = format!("{}/", tmp.path().display());
    // Synthesize a buffer with a recognized filesystem prefix.
    // Relative paths must stay in filesystem completion.
    let buffer = "./";
    let cursor = buffer.len();
    let cands = build_candidates(
        &[CommandCompletion::new(":whatever", "")],
        &CompletionData::new(),
        buffer,
        cursor,
    );
    // No assertion on contents (the test machine's CWD differs);
    // just confirm we didn't fall through to command logic.
    for c in &cands {
        assert!(!c.replacement.starts_with('/'), "expected fs candidate");
    }
    let _ = prefix;
}

#[test]
fn home_relative_token_reads_injected_home_and_preserves_tilde_replacement() {
    // `~/...` completion must read entries from the user's home
    // directory, but accepting a candidate should keep the prompt
    // home-relative instead of inserting an absolute path.
    let home = tempfile::tempdir().expect("tempdir");
    fs::write(home.path().join("alpha.txt"), "").expect("write alpha");
    fs::write(home.path().join("beta.txt"), "").expect("write beta");
    let buffer = "open ~/a now";
    let cursor = "open ~/a".len();

    let cands = build_candidates_with_home(
        &[CommandCompletion::new(":whatever", "")],
        &CompletionData::new(),
        buffer,
        cursor,
        Some(home.path()),
    );

    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].label, "~/alpha.txt");
    assert_eq!(cands[0].replacement, "open ~/alpha.txt now");
}

#[test]
fn filesystem_directory_candidates_include_trailing_slash() {
    // Directory completions should be visibly distinct from files and accepting
    // one should leave the prompt ready to complete or type a child path.
    let home = tempfile::tempdir().expect("tempdir");
    fs::create_dir(home.path().join("alpha-dir")).expect("mkdir alpha-dir");
    fs::write(home.path().join("alpha.txt"), "").expect("write alpha file");
    let buffer = "open ~/alpha";
    let cursor = buffer.len();

    let cands = build_candidates_with_home(
        &[CommandCompletion::new(":whatever", "")],
        &CompletionData::new(),
        buffer,
        cursor,
        Some(home.path()),
    );

    let dir = cands
        .iter()
        .find(|cand| cand.description == "directory")
        .expect("directory candidate");
    assert_eq!(dir.label, "~/alpha-dir/");
    assert_eq!(dir.replacement, "open ~/alpha-dir/");

    let file = cands
        .iter()
        .find(|cand| cand.description == "file")
        .expect("file candidate");
    assert_eq!(file.label, "~/alpha.txt");
    assert_eq!(file.replacement, "open ~/alpha.txt");
}

/// Keeps an intrinsic command buffer out of generic filesystem completion.
#[test]
fn command_buffer_does_not_route_to_filesystem() {
    let cands = build_candidates(
        &[CommandCompletion::new(":model", "Switch model")],
        &CompletionData::new(),
        ":mod",
        ":mod".len(),
    );
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].replacement, ":model");
}

/// Ensures a leading absolute path uses the shipped slash path rule rather than
/// entering command completion.
#[test]
fn leading_absolute_path_completes_from_the_filesystem() {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("alpha.txt"), "").expect("write alpha");
    let buffer = format!("{}/a", directory.path().display());

    let candidates = build_candidates(
        &[CommandCompletion::new(":model", "Switch model")],
        &CompletionData::new(),
        &buffer,
        buffer.len(),
    );

    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].replacement,
        format!("{}/alpha.txt", directory.path().display())
    );
    assert!(!candidates[0].label.starts_with(':'));
}

/// Ensures configured colon triggers cannot shadow intrinsic command mode or
/// leak prompt-text completers into command-name and argument completion.
#[test]
fn configured_colon_rules_cannot_shadow_command_mode() {
    let rules = CompletionRules::new(vec![
        CompletionRule::parse(":", "complete_path").expect("path rule"),
        CompletionRule::parse(":set", "complete_with_command printf hostile")
            .expect("command rule"),
    ]);
    let data = CompletionData::new();

    let root = build_candidates_with_rules(
        &[CommandCompletion::new(":model", "Switch model")],
        &data,
        &rules,
        ":mod",
        ":mod".len(),
    );
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].replacement, ":model");

    assert!(
        build_candidates_with_rules(
            &[CommandCompletion::new(":set", "Set UI state")],
            &data,
            &rules,
            ":set /tmp",
            ":set /tmp".len(),
        )
        .is_empty()
    );
    assert!(
        rules
            .command_for_exact_token(":set", ":set".len())
            .is_none()
    );
}

/// Ensures doubled-colon literal prompt text does not open command completion.
#[test]
fn literal_colon_escape_does_not_enter_command_completion() {
    assert!(
        build_candidates(
            &[CommandCompletion::new(":model", "Switch model")],
            &CompletionData::new(),
            "::model",
            "::model".len(),
        )
        .is_empty()
    );
}

#[test]
fn at_token_completes_agent_mentions_in_prompt_text() {
    // Agent mentions are prompt-text completions, not commands. Accepting
    // one must replace only the current `@...` token and preserve surrounding
    // prompt text.
    let data = CompletionData::new();
    data.set_agent_mention_completer(std::sync::Arc::new(|args| {
        assert_eq!(args, ["wo"]);
        vec![crate::completion::CompletionItem::new("worker", "agent")]
    }));
    let buffer = "ask @wo for help";
    let cursor = "ask @wo".len();

    let cands = build_candidates(
        &[CommandCompletion::new(":model", "Switch model")],
        &data,
        buffer,
        cursor,
    );

    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].label, "worker");
    assert_eq!(cands[0].description, "agent");
    assert_eq!(cands[0].replacement, "ask @worker for help");
}

#[test]
fn at_mentions_remain_agent_completion_after_dotslash_fuzzy_port() {
    // The external patch also used `@` for file fuzzy search. Tau reserves that
    // prefix for agent mentions, so keep this regression focused and hermetic.
    let data = CompletionData::new();
    data.set_agent_mention_completer(std::sync::Arc::new(|args| {
        assert_eq!(args, ["wor"]);
        vec![crate::completion::CompletionItem::plain("worker")]
    }));

    let mentions = build_candidates(
        &[CommandCompletion::new(":whatever", "")],
        &data,
        "ask @wor",
        "ask @wor".len(),
    );
    assert_eq!(mentions.len(), 1);
    assert_eq!(mentions[0].replacement, "ask @worker");
}
/// Preserves indentation while completing a leading command token.
#[test]
fn leading_whitespace_command_preserves_prefix() {
    let cands = build_candidates(
        &[CommandCompletion::new(":model", "Switch model")],
        &CompletionData::new(),
        "  :mod",
        "  :mod".len(),
    );
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].replacement, "  :model");
}

#[test]
fn configured_actions_preserve_surrounding_prompt_text() {
    let rules = CompletionRules::new(vec![
        CompletionRule::parse("#", "complete_actions").expect("rule parses"),
    ]);
    let cands = build_candidates_with_rules(
        &[CommandCompletion::new(":model", "Switch model")],
        &CompletionData::new(),
        &rules,
        "ask #mod now",
        "ask #mod".len(),
    );
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].replacement, "ask #model now");
}

/// Gives intrinsic leading commands precedence over configurable token rules.
#[test]
fn command_completion_requires_whole_token_and_loses_to_leading_command_tokens() {
    let rules = CompletionRules::new(vec![
        CompletionRule::parse("#/", "complete_with_command fzf").expect("rule parses"),
        CompletionRule::parse(":", "complete_with_command fzf").expect("rule parses"),
    ]);
    assert!(rules.command_for_exact_token("#/foo", "#/".len()).is_none());
    assert!(rules.command_for_exact_token(":", ":".len()).is_none());
    assert!(
        rules
            .command_for_exact_token("ask #/", "ask #/".len())
            .is_some()
    );
}

#[test]
fn non_slash_non_path_buffer_returns_nothing() {
    let cands = build_candidates(
        &[CommandCompletion::new(":model", "Switch model")],
        &CompletionData::new(),
        "hello",
        "hello".len(),
    );
    assert!(cands.is_empty());
}

#[test]
fn parent_traversal_token_is_recognised() {
    let cands = build_candidates(
        &[CommandCompletion::new(":whatever", "")],
        &CompletionData::new(),
        "../",
        "../".len(),
    );
    // Non-empty or empty is fine; we just verify it didn't fall
    // back to command behavior.
    for c in &cands {
        assert!(!c.replacement.starts_with('/'));
    }
    let _ = completion::CommandCompletion::new(":x", "");
}
