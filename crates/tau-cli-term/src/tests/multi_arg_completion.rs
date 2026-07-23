use std::sync::Arc;

use crate::completion::{
    ArgCompleter, CommandCompletion, CommandName, CompletionData, CompletionItem, build_candidates,
};

/// Build a completer that returns its first/second-arg menus
/// verbatim, ignoring filtering. Lets the test focus on the
/// argument-parsing + replacement-prefix logic in
/// `build_arg_candidates`, not the ranking inside completers.
fn make_completer() -> ArgCompleter {
    Arc::new(|args: &[&str]| match args.len() {
        1 => vec![
            CompletionItem::new("show-diff", "[false] diffs"),
            CompletionItem::new("show-thinking", "[true] reasoning"),
        ],
        2 => match args[0] {
            "show-diff" => vec![
                CompletionItem::new("true", "enabled"),
                CompletionItem::new("false", "disabled"),
            ],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    })
}

#[test]
fn first_arg_completion_lists_names_with_descriptions() {
    let data = CompletionData::new();
    data.set_arg_completer(CommandName::new(":set"), make_completer());
    let buf = ":set ";
    let cands = build_candidates(
        &[CommandCompletion::new(":set", "set a UI setting")],
        &data,
        buf,
        buf.len(),
    );
    assert_eq!(cands.len(), 2);
    assert_eq!(cands[0].label, "show-diff");
    assert_eq!(cands[0].description, "[false] diffs");
    assert_eq!(cands[0].replacement, ":set show-diff");
}

#[test]
fn second_arg_completion_keeps_first_arg_in_replacement() {
    let data = CompletionData::new();
    data.set_arg_completer(CommandName::new(":set"), make_completer());
    let buf = ":set show-diff ";
    let cands = build_candidates(
        &[CommandCompletion::new(":set", "set a UI setting")],
        &data,
        buf,
        buf.len(),
    );
    assert_eq!(cands.len(), 2);
    assert_eq!(cands[0].label, "true");
    // The first arg must be preserved in the replacement so
    // accepting a value completes the full `:set <name> <value>`
    // form rather than dropping the name.
    assert_eq!(cands[0].replacement, ":set show-diff true");
    assert_eq!(cands[1].replacement, ":set show-diff false");
}

/// Protects cursor-aware command argument completion: completing the
/// first arg in the middle of a command must preserve the untouched suffix.
#[test]
fn first_arg_completion_preserves_text_after_cursor() {
    let data = CompletionData::new();
    data.set_arg_completer(CommandName::new(":set"), make_completer());
    let buf = ":set sh trailing";
    let cursor = ":set sh".len();
    let cands = build_candidates(
        &[CommandCompletion::new(":set", "set a UI setting")],
        &data,
        buf,
        cursor,
    );

    assert_eq!(cands.len(), 2);
    assert_eq!(cands[0].replacement, ":set show-diff trailing");
    assert_eq!(cands[1].replacement, ":set show-thinking trailing");
}

/// Protects later-argument completion at a non-final cursor position so
/// accepting a value does not drop text after the active token.
#[test]
fn second_arg_completion_preserves_text_after_cursor() {
    let data = CompletionData::new();
    data.set_arg_completer(CommandName::new(":set"), make_completer());
    let buf = ":set show-diff t trailing";
    let cursor = ":set show-diff t".len();
    let cands = build_candidates(
        &[CommandCompletion::new(":set", "set a UI setting")],
        &data,
        buf,
        cursor,
    );

    assert_eq!(cands.len(), 2);
    assert_eq!(cands[0].replacement, ":set show-diff true trailing");
    assert_eq!(cands[1].replacement, ":set show-diff false trailing");
}

/// Protects command name completion when the cursor is still in the
/// command token but later arguments are already present.
#[test]
fn command_name_completion_preserves_trailing_arguments() {
    let data = CompletionData::new();
    let buf = ":mod trailing";
    let cursor = ":mod".len();
    let cands = build_candidates(
        &[
            CommandCompletion::new(":model", "switch model"),
            CommandCompletion::new(":quit", "exit"),
        ],
        &data,
        buf,
        cursor,
    );

    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].label, ":model");
    assert_eq!(cands[0].replacement, ":model trailing");
}

/// Protects command completion from panicking on valid multi-byte whitespace
/// separators such as an em space.
#[test]
fn command_arg_completion_accepts_multibyte_whitespace() {
    let data = CompletionData::new();
    data.set_arg_completer(CommandName::new(":set"), make_completer());
    let buf = ":set\u{2003}show-diff ";
    let cands = build_candidates(
        &[CommandCompletion::new(":set", "set a UI setting")],
        &data,
        buf,
        buf.len(),
    );

    assert_eq!(cands.len(), 2);
    assert_eq!(cands[0].replacement, ":set show-diff true");
    assert_eq!(cands[1].replacement, ":set show-diff false");
}

/// Protects the public completion entry point from non-char-boundary cursor
/// offsets supplied by callers.
#[test]
fn command_arg_completion_ignores_non_boundary_cursor() {
    let data = CompletionData::new();
    data.set_arg_completer(CommandName::new(":set"), make_completer());
    let buf = ":set é";
    let cursor_inside_e = ":set ".len() + 1;

    let result = std::panic::catch_unwind(|| {
        build_candidates(
            &[CommandCompletion::new(":set", "set a UI setting")],
            &data,
            buf,
            cursor_inside_e,
        )
    });

    assert!(result.is_ok());
}

/// Protects cursor-aware command completion from rewriting a command when
/// the cursor is still in leading whitespace before the command token.
#[test]
fn command_completion_ignores_cursor_before_command_token() {
    let data = CompletionData::new();
    let buf = "  :mod trailing";
    for cursor in [1, 2] {
        let cands = build_candidates(
            &[CommandCompletion::new(":model", "switch model")],
            &data,
            buf,
            cursor,
        );

        assert!(cands.is_empty(), "cursor {cursor} produced {cands:?}");
    }
}

/// Protects command-name completion from preserving same-token text after the
/// cursor; accepting a command should replace the whole command token.
#[test]
fn command_name_completion_replaces_same_token_suffix() {
    let data = CompletionData::new();
    let buf = ":modXYZ";
    let cursor = ":mod".len();
    let cands = build_candidates(
        &[CommandCompletion::new(":model", "switch model")],
        &data,
        buf,
        cursor,
    );

    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].label, ":model");
    assert_eq!(cands[0].replacement, ":model");
}

#[test]
fn third_arg_returns_no_candidates() {
    let data = CompletionData::new();
    data.set_arg_completer(CommandName::new(":set"), make_completer());
    let buf = ":set show-diff true ";
    let cands = build_candidates(
        &[CommandCompletion::new(":set", "set a UI setting")],
        &data,
        buf,
        buf.len(),
    );
    assert!(cands.is_empty());
}
