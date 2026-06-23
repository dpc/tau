use std::sync::{Arc, Mutex};

use crate::{
    EditorContext, PROMPT_HISTORY_PREVIEW_MAX_BYTES, PROMPT_HISTORY_PREVIEW_TOTAL_BYTES,
    PROMPT_HISTORY_SEARCH_MAX_ROWS, PROMPT_HISTORY_SUMMARY_MAX_CHARS, PromptShellAction,
    PromptShellCommand, PromptShellResult, prompt_history_preview_dir, prompt_history_search_rows,
    run_prompt_shell_action,
};

#[test]
fn search_rows_are_newest_first_and_keep_multiline_prompts_one_row() {
    // Ctrl-R feeds fzf an indexed table, not raw prompt text. This
    // regression test protects multiline prompts from being split
    // into multiple fzf candidates and verifies the newest prompt
    // is shown first.
    let history = vec![
        "old prompt".to_owned(),
        "newer\nmultiline prompt".to_owned(),
    ];

    let rows = prompt_history_search_rows(&history);

    assert_eq!(rows, "1\tnewer multiline prompt\n0\told prompt\n");
}

/// Keeps prompt-history picker setup bounded before launching the external
/// picker, even if the stored prompt history contains many large entries.
#[test]
fn search_rows_and_previews_are_bounded_before_picker_launch() {
    let huge_prompt = "word ".repeat(PROMPT_HISTORY_SUMMARY_MAX_CHARS + 100)
        + &"x".repeat(PROMPT_HISTORY_PREVIEW_MAX_BYTES * 2);
    let history: Vec<String> = (0..(PROMPT_HISTORY_SEARCH_MAX_ROWS + 10))
        .map(|index| format!("prompt {index} {huge_prompt}"))
        .collect();

    let rows = prompt_history_search_rows(&history);
    assert_eq!(rows.lines().count(), PROMPT_HISTORY_SEARCH_MAX_ROWS);
    assert!(
        rows.lines()
            .all(|line| line.chars().count() < PROMPT_HISTORY_SUMMARY_MAX_CHARS + 32)
    );
    assert!(rows.starts_with(&(PROMPT_HISTORY_SEARCH_MAX_ROWS + 9).to_string()));

    let preview_dir = prompt_history_preview_dir(&history).expect("preview dir");
    let previews: Vec<_> = std::fs::read_dir(preview_dir.path())
        .expect("read preview dir")
        .collect::<Result<_, _>>()
        .expect("preview entries");
    assert_eq!(previews.len(), PROMPT_HISTORY_SEARCH_MAX_ROWS);
    let mut total_preview_bytes = 0usize;
    for entry in previews {
        let len = entry.metadata().expect("preview metadata").len() as usize;
        total_preview_bytes += len;
        assert!(
            len <= PROMPT_HISTORY_PREVIEW_MAX_BYTES,
            "preview {:?} had {len} bytes",
            entry.path()
        );
    }
    assert!(
        total_preview_bytes <= PROMPT_HISTORY_PREVIEW_TOTAL_BYTES,
        "preview directory used {total_preview_bytes} bytes"
    );
}

/// Protects prompt-history setup against huge single-token entries: summary
/// construction must stop at the display cap instead of measuring the whole
/// word.
#[test]
fn search_rows_truncate_single_token_summary_at_cap() {
    let huge_prompt = "x".repeat(PROMPT_HISTORY_SUMMARY_MAX_CHARS * 100);
    let rows = prompt_history_search_rows(&[huge_prompt]);
    let summary = rows
        .strip_prefix("0\t")
        .and_then(|row| row.strip_suffix('\n'))
        .expect("single history row");

    assert_eq!(summary.chars().count(), PROMPT_HISTORY_SUMMARY_MAX_CHARS);
    assert!(summary.ends_with('…'));
}

/// Preserves the empty-history behavior expected by Ctrl-R: when there are no
/// selectable history entries, Tau must not launch the user-configured picker
/// command because that command may have side effects or wait for input.
#[test]
fn empty_history_search_returns_none_without_spawning_picker() {
    let (term, handle, _input_tx) = tau_cli_term_raw::Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        crate::CursorShape::Bar,
    );
    let temp_dir = tempfile::tempdir().expect("marker tempdir");
    let marker = temp_dir.path().join("spawned");
    let action = PromptShellAction::HistorySearch(PromptShellCommand {
        command: format!("printf spawned > '{}'", marker.display()),
        trim: true,
    });

    let result = run_prompt_shell_action(
        &term,
        &handle,
        Arc::new(Mutex::new(EditorContext::default())),
        None,
        &[],
        action,
    )
    .expect("empty history search action");

    assert!(result.is_none());
    assert!(!marker.exists(), "history picker command was spawned");
}

#[test]
fn selected_history_prompt_replaces_buffer_and_can_be_undone() {
    // Ctrl-R must record the draft before launching the picker, expose
    // original history prompts through TAU_PROMPT_HISTORY_DIR for fzf
    // previews, then replace the buffer with the original history entry
    // (including embedded newlines). Undo should restore the draft the
    // user had before opening the picker.
    let (term, handle, _input_tx) = tau_cli_term_raw::Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        crate::CursorShape::Bar,
    );
    handle.set_buffer("current draft".to_owned(), "current draft".len());
    let history = vec!["old".to_owned(), "chosen\noriginal".to_owned()];
    let action = PromptShellAction::HistorySearch(PromptShellCommand {
            command: r#"index=$(head -n 1 | cut -f1); expected=$(printf 'chosen\noriginal'); test "$(cat "$TAU_PROMPT_HISTORY_DIR/$index")" = "$expected"; printf '%s\n' "$index""#.to_owned(),
            trim: true,
        });

    let result = run_prompt_shell_action(
        &term,
        &handle,
        Arc::new(Mutex::new(EditorContext::default())),
        None,
        &history,
        action,
    )
    .expect("history search action")
    .expect("selected prompt");

    match result {
        PromptShellResult::ReplacePreservingUndo(text) => {
            assert_eq!(text, "chosen\noriginal");
            handle.set_buffer_preserving_undo(text, "chosen\noriginal".len());
        }
        _ => panic!("expected undo-preserving replacement"),
    }
    assert_eq!(handle.get_buffer(), "chosen\noriginal");
    assert!(term.trigger_undo());
    assert_eq!(handle.get_buffer(), "current draft");
}
