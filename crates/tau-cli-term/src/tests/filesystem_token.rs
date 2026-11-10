use std::sync as path_std_sync;

use crate::completion::{
    CommandCompletion, CompletionData, CompletionRule, CompletionRules, build_candidates,
    build_candidates_with_rules,
};

/// Prose contractions must not swallow later path triggers, regardless of line
/// wrapping or the byte width of text before the cursor.
#[test]
fn prose_apostrophes_do_not_hide_later_path_rules() {
    let literal = "./README.md looks like shit. It supposed to be a human-facing informative, high level intro to\nproject, and instead it's full of commits and super low level dev details. You can look how ./";
    let data = CompletionData::new();
    let calls = path_std_sync::Arc::new(path_std_sync::Mutex::new(Vec::new()));
    let observed = calls.clone();
    data.set_remote_completer(path_std_sync::Arc::new(move |buffer, cursor, mode| {
        observed
            .lock()
            .expect("calls")
            .push((buffer.to_owned(), cursor, mode));
        Vec::new()
    }));
    for (buffer, cursor) in [
        (literal, "./README.md".len()),
        (literal, literal.len()),
        ("doesn't trigger ./", "doesn't trigger ./".len()),
        ("l'été\nopen './hé llo' tail", "l'été\nopen './hé".len()),
        ("it's ./héllo tail", "it's ./hé".len()),
        ("it's ./README.md", 0),
    ] {
        let before = calls.lock().expect("calls").len();
        build_candidates(&[], &data, buffer, cursor);
        if cursor == 0 {
            assert_eq!(calls.lock().expect("calls").len(), before);
        } else {
            assert_eq!(
                calls.lock().expect("calls").last(),
                Some(&(buffer.to_owned(), cursor, Some(false))),
                "{buffer:?}"
            );
            assert_eq!(calls.lock().expect("calls").len(), before + 1);
        }
    }
    // A cursor inside a UTF-8 scalar must not route a malformed path request.
    let buffer = "it's ./hé";
    let before = calls.lock().expect("calls").len();
    build_candidates(&[], &data, buffer, buffer.len() - 1);
    assert_eq!(calls.lock().expect("calls").len(), before);
}

/// Path rules never inspect the terminal host when no remote cache is
/// installed.
#[test]
fn path_rules_have_no_local_fallback() {
    for buffer in ["./", "../", "~/", "/tmp/", "!shell cat ./"] {
        assert!(build_candidates(&[], &CompletionData::new(), buffer, buffer.len()).is_empty());
    }
}

/// Rule selection calls the application cache with the original local cursor.
#[test]
fn path_rules_delegate_to_remote_cache() {
    let data = CompletionData::new();
    let calls = path_std_sync::Arc::new(path_std_sync::Mutex::new(Vec::new()));
    let observed = calls.clone();
    data.set_remote_completer(path_std_sync::Arc::new(move |buffer, cursor, mode| {
        observed
            .lock()
            .expect("calls")
            .push((buffer.to_owned(), cursor, mode));
        Vec::new()
    }));
    for buffer in ["./x", "../x", "~/x", "/tmp/x", "!sh", "!!sh"] {
        build_candidates(&[], &data, buffer, buffer.len());
    }
    let calls = calls.lock().expect("calls");
    assert_eq!(calls.len(), 6);
    assert_eq!(calls[0].2, Some(false));
    assert_eq!(calls[1].2, Some(false));
    assert_eq!(calls[4].2, None);
    assert_eq!(calls[5].2, None);
}

/// Explicit fuzzy rules select the remote fuzzy mode, including quoted paths.
#[test]
fn path_fuzzy_rules_delegate_quoted_unicode_token() {
    let data = CompletionData::new();
    let calls = path_std_sync::Arc::new(path_std_sync::Mutex::new(Vec::new()));
    let observed = calls.clone();
    data.set_remote_completer(path_std_sync::Arc::new(move |buffer, cursor, mode| {
        observed
            .lock()
            .expect("calls")
            .push((buffer.to_owned(), cursor, mode));
        Vec::new()
    }));
    let rules = CompletionRules::new(vec![
        CompletionRule::parse("./", "complete_path_fuzzy").expect("fuzzy rule"),
    ]);
    let buffer = "!remote cat './hé llo'";
    let cursor = "!remote cat './hé".len();
    build_candidates_with_rules(&[], &data, &rules, buffer, cursor);
    assert_eq!(
        *calls.lock().expect("calls"),
        [(buffer.to_owned(), cursor, Some(true))]
    );
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
fn leading_absolute_path_completes_from_remote_cache() {
    let buffer = "/elsewhere/a";
    let data = CompletionData::new();
    data.set_remote_completer(path_std_sync::Arc::new(|_, _, _| {
        vec![crate::Candidate {
            label: "/elsewhere/alpha.txt".to_owned(),
            description: "remote · file".to_owned(),
            replacement: "/elsewhere/alpha.txt".to_owned(),
            cursor: "/elsewhere/alpha.txt".len(),
            acceptance: None,
        }]
    }));
    let candidates = build_candidates(
        &[CommandCompletion::new(":model", "Switch model")],
        &data,
        buffer,
        buffer.len(),
    );

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].replacement, "/elsewhere/alpha.txt");
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
    data.set_agent_mention_completer(path_std_sync::Arc::new(|args| {
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

/// Session completion replaces only a word-leading ampersand token and keeps
/// ordinary surrounding prompt text unchanged.
#[test]
fn ampersand_token_completes_running_session_ids() {
    let data = CompletionData::new();
    data.set_session_completer(path_std_sync::Arc::new(|args| {
        assert_eq!(args, ["roj"]);
        vec![crate::completion::CompletionItem::new(
            "project-main",
            "/work/project",
        )]
    }));
    let buffer = "ask &roj about this";
    let cursor = "ask &roj".len();

    let candidates = build_candidates(&[], &data, buffer, cursor);

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].label, "project-main");
    assert_eq!(candidates[0].description, "/work/project");
    assert_eq!(candidates[0].replacement, "ask &project-main about this");
    assert_eq!(candidates[0].cursor, "ask &project-main".len());
}

/// The named session rule parses and participates in the built-in rule set.
#[test]
fn complete_sessions_rule_parses_and_is_built_in() {
    assert!(matches!(
        CompletionRule::parse("&", "complete_sessions").map(|rule| rule.kind),
        Some(crate::completion::CompletionRuleKind::Sessions)
    ));
    let data = CompletionData::new();
    data.set_session_completer(path_std_sync::Arc::new(|_| {
        vec![crate::completion::CompletionItem::plain("session-one")]
    }));
    assert_eq!(build_candidates(&[], &data, "&", 1).len(), 1);
}

/// Session completion never treats an embedded ampersand as a token trigger and
/// never erases a remote-agent suffix from a session address.
#[test]
fn session_completion_rejects_embedded_tokens_and_remote_agent_suffixes() {
    let data = CompletionData::new();
    data.set_session_completer(path_std_sync::Arc::new(|_| {
        vec![crate::completion::CompletionItem::plain("project-main")]
    }));

    assert!(build_candidates(&[], &data, "x&proj", "x&proj".len()).is_empty());
    assert!(build_candidates(&[], &data, "&proj/@agent", "&proj".len()).is_empty());
    assert!(build_candidates(&[], &data, "&proj/@agent", "&proj/@agent".len()).is_empty());
}

/// Intrinsic command mode retains precedence over session completion in command
/// arguments.
#[test]
fn command_mode_does_not_offer_session_completion() {
    let data = CompletionData::new();
    data.set_session_completer(path_std_sync::Arc::new(|_| {
        vec![crate::completion::CompletionItem::plain("project-main")]
    }));
    assert!(build_candidates(&[], &data, ":anything &proj", 15).is_empty());
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
