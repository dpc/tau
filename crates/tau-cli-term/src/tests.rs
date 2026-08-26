use std::{cell as path_std_cell, rc as path_std_rc, sync as path_std_sync, time as path_std_time};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;

const TEST_PROMPT_HISTORY_MAX_BYTES: usize = 64 * 1024;

/// Unconfirmed foreground ownership disarms the outer resume guard so the
/// interactive attachment cannot re-enable raw input or redraw.
#[test]
fn ownership_failure_does_not_resume_external_terminal() {
    let resume_calls = path_std_rc::Rc::new(path_std_cell::Cell::new(0));
    let resume_state = resume_calls.clone();
    let guard = ExternalResumeGuard::new(move || {
        resume_state.set(resume_state.get() + 1);
        Ok(())
    });
    let error = BoundedCommandError::ForegroundOwnershipUnconfirmed {
        primary: "command failure (injected waiter failure)".to_owned(),
        restoration: "injected persistent restore failure".to_owned(),
    };

    let returned = preserve_pause_on_unconfirmed_foreground(guard, Err::<(), _>(error))
        .expect_err("ownership failure remains fatal");

    assert!(returned.is_foreground_ownership_unconfirmed());
    assert!(returned.to_string().contains("injected waiter failure"));
    assert!(returned.to_string().contains("persistent restore failure"));
    assert_eq!(resume_calls.get(), 0);
}

/// Picker setup failure after foreground transfer still checks restoration and
/// disarms the picker's distinct explicit-resume path on persistent failure.
#[cfg(unix)]
#[test]
fn picker_post_spawn_restoration_failure_does_not_resume() {
    use std::sync::atomic::Ordering;

    let _foreground_guard = bounded_command::FOREGROUND_CLAIM_TEST_LOCK
        .lock()
        .expect("foreground restore test lock");
    let _fzf_guard = AGENT_FZF_TEST_LOCK.lock().expect("agent fzf test lock");
    let program = fake_fzf("sleep 30");
    let (term, _handle, _input_tx) = new_test_term(vec![]);
    let resume_calls = path_std_rc::Rc::new(path_std_cell::Cell::new(0));
    let resume_state = resume_calls.clone();
    bounded_command::FAIL_FOREGROUND_RESTORE.store(true, Ordering::SeqCst);

    let error = term
        .pick_agent_row_with_command_and_terminal(
            program.as_os_str(),
            "",
            path_std_time::Duration::from_secs(2),
            ProcessOwnership::ForegroundProcessGroup,
            AgentPickerHooks {
                pause: || Ok(()),
                resume: move || {
                    resume_state.set(resume_state.get() + 1);
                    Ok(())
                },
                after_spawn: || Err("injected post-spawn setup failure".to_owned()),
            },
        )
        .expect_err("persistent restoration failure must fail stop");
    bounded_command::FAIL_FOREGROUND_RESTORE.store(false, Ordering::SeqCst);

    assert!(error.is_foreground_ownership_unconfirmed());
    let message = error.to_string();
    assert!(message.contains("injected post-spawn setup failure"));
    assert!(message.contains("restore Tau terminal foreground"));
    assert_eq!(resume_calls.get(), 0);
}

fn new_test_term_with_data_and_bindings(
    commands: Vec<CommandCompletion>,
    bindings: impl IntoIterator<Item = (String, String)>,
) -> (
    HighTerm,
    TermHandle,
    CompletionData,
    path_std_sync::mpsc::Sender<TestRawEvent>,
) {
    let (raw_term, handle, input_tx) = tau_cli_term_raw::Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        CursorShape::Bar,
    );
    let (term, completion_data) =
        HighTerm::new_for_test(raw_term, handle.clone(), commands, Theme::new(), bindings);
    (term, handle, completion_data, input_tx)
}

fn new_test_term_with_data(
    commands: Vec<CommandCompletion>,
) -> (
    HighTerm,
    TermHandle,
    CompletionData,
    path_std_sync::mpsc::Sender<TestRawEvent>,
) {
    new_test_term_with_data_and_bindings(commands, std::iter::empty::<(String, String)>())
}

fn new_test_term(
    commands: Vec<CommandCompletion>,
) -> (
    HighTerm,
    TermHandle,
    path_std_sync::mpsc::Sender<TestRawEvent>,
) {
    let (term, handle, _completion_data, input_tx) = new_test_term_with_data(commands);
    (term, handle, input_tx)
}

fn send_key(input_tx: &path_std_sync::mpsc::Sender<TestRawEvent>, code: KeyCode) {
    send_key_with_modifiers(input_tx, code, KeyModifiers::NONE);
}

fn send_key_with_modifiers(
    input_tx: &path_std_sync::mpsc::Sender<TestRawEvent>,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    input_tx
        .send(TestRawEvent::Key(KeyEvent::new(code, modifiers)))
        .expect("send key");
}

/// Replacing a submitted prompt must rewrite both history owners and discard
/// the submitted draft's old undo state so navigation, search, previews, and
/// undo cannot recover the original text.
#[test]
fn submitted_prompt_replacement_rewrites_every_terminal_history_view() {
    const CODE: &str = "CODE_SENTINEL_46";
    const STATE: &str = "STATE_SENTINEL_46";
    const REDACTED: &str = ":email auth google finish <redacted>";
    let sensitive_suffix =
        format!(":email auth google finish work http://127.0.0.1:54321/?code={CODE}&state={STATE}");
    let (mut term, handle, input_tx) = new_test_term(vec![]);
    submit(&mut term, &handle, &input_tx, "old");
    term.term.trigger_history_step(-1);
    input_tx
        .send(TestRawEvent::Paste(sensitive_suffix))
        .expect("paste sensitive line");
    assert!(matches!(
        term.get_next_event().expect("paste sensitive line"),
        Event::BufferChanged
    ));
    send_submit(&input_tx);
    let submitted = match term.get_next_event().expect("submitted line") {
        Event::Line(submitted) => submitted,
        _ => panic!("expected submitted line"),
    };
    assert!(submitted.contains(CODE));
    assert!(submitted.contains(STATE));

    term.replace_last_submitted_prompt(REDACTED.to_owned());

    assert_eq!(term.prompt_history, ["old", REDACTED]);
    let rows = prompt_history_search_rows(&term.prompt_history);
    let previews = prompt_history_preview_dir(&term.prompt_history).expect("preview directory");
    let serialized = format!(
        "{:?}\n{rows}\n{}",
        term.prompt_history,
        std::fs::read_to_string(previews.path().join("0")).expect("preview")
    );
    assert!(!serialized.contains(CODE));
    assert!(!serialized.contains(STATE));
    assert!(serialized.contains(REDACTED));

    term.term.trigger_history_step(-1);
    assert_eq!(handle.get_buffer(), REDACTED);
    assert!(!term.term.trigger_undo());
    assert!(!term.term.trigger_redo());
    term.term.trigger_history_step(-1);
    assert_eq!(handle.get_buffer(), REDACTED);
    assert!(!term.term.trigger_undo());
    assert!(!term.term.trigger_redo());
    term.term.trigger_history_step(1);
    assert_eq!(handle.get_buffer(), REDACTED);
    term.term.trigger_history_step(1);
    assert_eq!(handle.get_buffer(), "");
}

/// Keeps search history's independent newest suffix bounded, while raw
/// navigation retains its own unrelated entries and the picker remains newest
/// first.
#[test]
fn prompt_history_retention_evicts_oldest_entries_independently() {
    let (mut term, handle, _input_tx) = new_test_term(vec![]);
    term.term.seed_input_history(["raw-only".to_owned()]);
    term.prompt_history = bounded_seeded_prompt_history(
        (0..=PROMPT_HISTORY_MAX_ENTRIES)
            .map(|index| index.to_string())
            .collect(),
    );

    assert_eq!(term.prompt_history.len(), PROMPT_HISTORY_MAX_ENTRIES);
    assert_eq!(
        term.prompt_history
            .first()
            .expect("entry cap retains newest search history"),
        "1"
    );
    assert_eq!(
        term.prompt_history
            .last()
            .expect("entry cap retains newest search history"),
        &PROMPT_HISTORY_MAX_ENTRIES.to_string()
    );
    assert_eq!(
        prompt_history_search_rows(&term.prompt_history)
            .lines()
            .next()
            .expect("nonempty history produces a picker row"),
        format!(
            "{}\t{PROMPT_HISTORY_MAX_ENTRIES}",
            PROMPT_HISTORY_MAX_ENTRIES - 1
        )
    );
    term.term.trigger_history_step(-1);
    assert_eq!(handle.get_buffer(), "raw-only");
}

/// Retains an exact search-text byte limit but omits one larger finalized
/// prompt without changing the raw line returned to routing.
#[test]
fn prompt_history_retention_keeps_exact_bytes_and_omits_oversize_prompt() {
    let (mut term, handle, input_tx) = new_test_term(vec![]);
    term.prompt_history =
        bounded_seeded_prompt_history(vec![String::from("é").repeat(PROMPT_HISTORY_MAX_BYTES / 2)]);
    assert_eq!(
        term.prompt_history
            .first()
            .expect("exact byte limit retains the search entry")
            .len(),
        PROMPT_HISTORY_MAX_BYTES
    );

    term.prompt_history_limit_override = Some(PromptHistoryLimits {
        max_entries: PROMPT_HISTORY_MAX_ENTRIES,
        max_bytes: TEST_PROMPT_HISTORY_MAX_BYTES,
    });
    term.term
        .set_input_history_max_bytes_for_test(TEST_PROMPT_HISTORY_MAX_BYTES);
    term.prompt_history = vec![String::from("é").repeat(TEST_PROMPT_HISTORY_MAX_BYTES / 2)];
    let oversize = "x".repeat(TEST_PROMPT_HISTORY_MAX_BYTES + 1);
    handle.set_buffer(oversize.clone(), oversize.len());
    send_submit(&input_tx);
    assert!(matches!(
        term.get_next_event().expect("route oversize prompt"),
        Event::Line(line) if line == oversize
    ));
    term.finalize_last_submitted_prompt_history();
    assert_eq!(
        term.prompt_history
            .first()
            .expect("oversize omission preserves prior history")
            .len(),
        TEST_PROMPT_HISTORY_MAX_BYTES
    );
}

/// Accounts after literal-colon canonicalization, so an escaped line that is
/// one byte too large before normalization remains navigable and searchable.
#[test]
fn prompt_history_retention_uses_canonical_literal_bytes() {
    let (mut term, _handle, _input_tx) = new_test_term(vec![]);
    term.prompt_history_limit_override = Some(PromptHistoryLimits {
        max_entries: PROMPT_HISTORY_MAX_ENTRIES,
        max_bytes: TEST_PROMPT_HISTORY_MAX_BYTES,
    });
    let canonical = format!(":{}", "x".repeat(TEST_PROMPT_HISTORY_MAX_BYTES - 1));
    let escaped = format!(":{canonical}");
    term.record_submitted_prompt(&escaped);

    term.finalize_last_submitted_prompt_history();
    assert_eq!(term.prompt_history, [canonical]);
    assert!(prompt_history_search_rows(&term.prompt_history).starts_with("0\t:"));
}

/// Applies retention after redaction, so an oversized sensitive routed line
/// leaves only its small safe presentation in both history owners.
#[test]
fn prompt_history_retention_uses_redacted_final_form() {
    const REDACTED: &str = ":email auth google finish <redacted>";
    let (mut term, handle, input_tx) = new_test_term(vec![]);
    term.prompt_history_limit_override = Some(PromptHistoryLimits {
        max_entries: PROMPT_HISTORY_MAX_ENTRIES,
        max_bytes: TEST_PROMPT_HISTORY_MAX_BYTES,
    });
    submit(&mut term, &handle, &input_tx, "prior");
    term.finalize_last_submitted_prompt_history();
    let sensitive = format!(
        ":email auth google finish {}",
        "x".repeat(TEST_PROMPT_HISTORY_MAX_BYTES)
    );
    handle.set_buffer(sensitive.clone(), sensitive.len());
    send_submit(&input_tx);
    assert!(matches!(
        term.get_next_event().expect("route sensitive prompt"),
        Event::Line(line) if line == sensitive
    ));
    term.replace_last_submitted_prompt(REDACTED.to_owned());
    term.finalize_last_submitted_prompt_history();
    assert_eq!(term.prompt_history, ["prior", REDACTED]);
    term.term.trigger_history_step(-1);
    assert_eq!(term.handle().get_buffer(), REDACTED);
}

/// Evicts the oldest search entry when several individually valid entries
/// exceed the aggregate primary-text byte budget.
#[test]
fn prompt_history_retention_evicts_oldest_entries_for_aggregate_bytes() {
    let half_budget = "x".repeat(PROMPT_HISTORY_MAX_BYTES / 2);
    let history = bounded_seeded_prompt_history(vec![
        "old".to_owned() + &half_budget,
        "new".to_owned() + &half_budget,
        "latest".to_owned(),
    ]);

    assert_eq!(history, [format!("new{half_budget}"), "latest".to_owned()]);
}

/// Preserves recalled-source identity through high-level input handling until
/// redaction finalizes aggregate retention.
#[test]
fn recalled_aggregate_overflow_is_redacted_before_highterm_retention() {
    const REDACTED: &str = ":email auth google finish <redacted>";
    let (mut term, _handle, input_tx) = new_test_term(vec![]);
    term.prompt_history_limit_override = Some(PromptHistoryLimits {
        max_entries: PROMPT_HISTORY_MAX_ENTRIES,
        max_bytes: TEST_PROMPT_HISTORY_MAX_BYTES,
    });
    term.term
        .set_input_history_max_bytes_for_test(TEST_PROMPT_HISTORY_MAX_BYTES);
    term.term.seed_input_history([
        "o".repeat(TEST_PROMPT_HISTORY_MAX_BYTES / 2),
        "t".repeat(TEST_PROMPT_HISTORY_MAX_BYTES / 2),
    ]);
    term.prompt_history = vec![
        "o".repeat(TEST_PROMPT_HISTORY_MAX_BYTES / 2),
        "t".repeat(TEST_PROMPT_HISTORY_MAX_BYTES / 2),
    ];
    term.term.trigger_history_step(-1);
    input_tx
        .send(TestRawEvent::Paste(" secret".to_owned()))
        .expect("extend recalled draft");
    assert!(matches!(
        term.get_next_event().expect("edit recalled draft"),
        Event::BufferChanged
    ));
    send_submit(&input_tx);
    assert!(matches!(
        term.get_next_event().expect("route recalled draft"),
        Event::Line(_)
    ));

    term.replace_last_submitted_prompt(REDACTED.to_owned());
    term.finalize_last_submitted_prompt_history();
    term.term.trigger_history_step(-1);
    assert_eq!(term.handle().get_buffer(), REDACTED);
    term.term.trigger_history_step(-1);
    assert_eq!(term.handle().get_buffer(), REDACTED);
    term.term.trigger_history_step(-1);
    assert!(term.handle().get_buffer().starts_with('o'));
    assert_eq!(
        term.prompt_history,
        [
            "t".repeat(TEST_PROMPT_HISTORY_MAX_BYTES / 2),
            REDACTED.to_owned()
        ]
    );
}

fn send_submit(input_tx: &path_std_sync::mpsc::Sender<TestRawEvent>) {
    send_key_with_modifiers(input_tx, KeyCode::Enter, KeyModifiers::CONTROL);
}

fn submit(
    term: &mut HighTerm,
    handle: &TermHandle,
    input_tx: &path_std_sync::mpsc::Sender<TestRawEvent>,
    line: &str,
) {
    handle.set_buffer(line.to_owned(), line.len());
    send_submit(input_tx);
    assert!(matches!(
        term.get_next_event().expect("submit line"),
        Event::Line(submitted) if submitted == line
    ));
}

fn type_text(
    term: &mut HighTerm,
    input_tx: &path_std_sync::mpsc::Sender<TestRawEvent>,
    text: &str,
) {
    for ch in text.chars() {
        send_key(input_tx, KeyCode::Char(ch));
        assert!(matches!(
            term.get_next_event().expect("type char"),
            Event::BufferChanged
        ));
    }
}

fn submit_typed(
    term: &mut HighTerm,
    input_tx: &path_std_sync::mpsc::Sender<TestRawEvent>,
    line: &str,
) {
    type_text(term, input_tx, line);
    send_submit(input_tx);
    assert!(matches!(
        term.get_next_event().expect("submit typed line"),
        Event::Line(submitted) if submitted == line
    ));
}

/// A plain submission needs only raw terminal's asynchronous clear redraw:
/// high-level history synchronization has no visible menu to remove.
#[test]
fn plain_submission_does_not_request_a_second_redraw() {
    let (mut term, handle, input_tx) = new_test_term(Vec::new());
    handle.set_buffer("plain prompt".to_owned(), "plain prompt".len());
    let before = handle.redraw_request_count();

    send_submit(&input_tx);

    assert!(matches!(
        term.get_next_event().expect("submit plain prompt"),
        Event::Line(line) if line == "plain prompt"
    ));
    assert_eq!(handle.redraw_request_count(), before + 1);
}

/// Submission must request a redraw after removing an already-visible
/// completion menu; raw terminal's input-clear redraw alone cannot erase that
/// menu.
#[test]
fn submission_with_visible_menu_requests_menu_removal_redraw() {
    let (mut term, handle, input_tx) = new_test_term(vec![
        CommandCompletion::new(":model", "Switch model"),
        CommandCompletion::new(":quit", "Exit"),
    ]);
    type_text(&mut term, &input_tx, ":");
    assert_eq!(
        handle.output_snapshot().suggestion_ids(),
        [COMPLETION_MENU_BLOCK_ID]
    );
    let before = handle.redraw_request_count();

    send_submit(&input_tx);

    assert!(matches!(
        term.get_next_event().expect("submit completion prefix"),
        Event::Line(line) if line == ":"
    ));
    assert!(handle.output_snapshot().suggestion_ids().is_empty());
    assert_eq!(handle.redraw_request_count(), before + 2);
}

/// Accepting a preview updates its menu, then submission clears the accepted
/// buffer and removes that menu, with no further redundant redraw request.
#[test]
fn completion_accept_then_submit_keeps_exact_redraw_requests() {
    let (mut term, handle, input_tx) = new_test_term(vec![
        CommandCompletion::new(":model", "Switch model"),
        CommandCompletion::new(":quit", "Exit"),
    ]);
    type_text(&mut term, &input_tx, ":");
    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("preview first completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":model");
    let before = handle.redraw_request_count();

    send_submit(&input_tx);
    send_submit(&input_tx);

    assert!(matches!(
        term.get_next_event().expect("accept then submit completion"),
        Event::Line(line) if line == ":model"
    ));
    assert_eq!(handle.get_buffer(), "");
    assert!(handle.output_snapshot().suggestion_ids().is_empty());
    assert_eq!(handle.redraw_request_count(), before + 4);
}

/// Ordinary submission must return the typed line, retain it for navigation,
/// and expose the canonical submitted draft with no undo/redo state.
#[test]
fn ordinary_submission_preserves_line_and_clears_recalled_undo_state() {
    let (mut term, handle, input_tx) = new_test_term(Vec::new());

    submit_typed(&mut term, &input_tx, "ordinary prompt");
    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event().expect("recall ordinary prompt"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "ordinary prompt");
    assert!(!term.term.trigger_undo());
    assert!(!term.term.trigger_redo());
}

/// Submitting an edited recalled entry must still replace both its source and
/// appended navigation entries while clearing the submitted edit stacks.
#[test]
fn recalled_submission_preserves_source_replacement_and_clears_undo_state() {
    let (mut term, handle, input_tx) = new_test_term(Vec::new());

    submit_typed(&mut term, &input_tx, "recalled");
    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event().expect("recall submitted prompt"),
        Event::BufferChanged
    ));
    send_key(&input_tx, KeyCode::End);
    type_text(&mut term, &input_tx, " edit");
    send_submit(&input_tx);
    let submitted = match term
        .get_next_event()
        .expect("submit edited recalled prompt")
    {
        Event::Line(line) => line,
        _ => panic!("expected submitted line"),
    };
    assert_eq!(submitted, "recalled edit");

    for expected_position in ["latest submission", "recalled source"] {
        term.term.trigger_history_step(-1);
        assert_eq!(handle.get_buffer(), "recalled edit", "{expected_position}");
        assert!(!term.term.trigger_undo());
        assert!(!term.term.trigger_redo());
    }
}

/// Ensures every rendered completion menu uses the same suggestion block id, so
/// reopening or refreshing a menu replaces any stale menu rows left behind by a
/// missed high-level cleanup path instead of appending another visible block.
#[test]
fn completion_menu_rendering_reuses_fixed_suggestion_block_id() {
    let (mut term, handle, input_tx) = new_test_term(vec![
        CommandCompletion::new(":model", "Switch model"),
        CommandCompletion::new(":quit", "Exit"),
    ]);

    send_key(&input_tx, KeyCode::Char(':'));
    assert!(matches!(
        term.get_next_event().expect("open completion"),
        Event::BufferChanged
    ));

    let snapshot = handle.output_snapshot();
    assert_eq!(snapshot.suggestion_ids(), &[COMPLETION_MENU_BLOCK_ID]);

    term.menu_block_id = None;
    send_key(&input_tx, KeyCode::Char('m'));
    assert!(matches!(
        term.get_next_event().expect("refresh completion"),
        Event::BufferChanged
    ));

    let snapshot = handle.output_snapshot();
    assert_eq!(snapshot.suggestion_ids(), &[COMPLETION_MENU_BLOCK_ID]);
}

/// Merges extension-provided command roots into the intrinsic root menu.
#[test]
fn dynamic_commands_are_in_root_completion_menu() {
    let data = CompletionData::new();
    data.set_dynamic_commands(vec![CommandCompletion::new(":email", "Email approvals")]);

    let candidates =
        completion::build_candidates(&[CommandCompletion::new(":quit", "Exit")], &data, ":", 1);

    let labels: Vec<_> = candidates
        .iter()
        .map(|candidate| candidate.label.as_str())
        .collect();
    assert_eq!(labels, vec![":quit", ":email"]);
}

/// Accepts the complete command-token alphabet.
#[test]
fn command_name_accepts_one_colon_and_one_token() {
    assert_eq!(CommandName::new(":a_b-1").as_str(), ":a_b-1");
}

/// Rejects missing, doubled, empty, separated, non-ASCII, or punctuated tokens.
#[test]
fn command_name_rejects_text_outside_command_token_grammar() {
    for invalid in ["model", ":", "::literal", ":model arg", ":é", ":model!"] {
        assert!(
            !completion::is_valid_command_name(invalid),
            "{invalid:?} should be rejected"
        );
    }
}

/// Enforces the validated grammar at construction time.
#[test]
#[should_panic(expected = "CommandName must be")]
fn command_name_constructor_panics_for_invalid_tokens() {
    CommandName::new("model");
}

#[test]
fn dynamic_arg_completers_are_replaced_with_dynamic_commands() {
    let data = CompletionData::new();
    data.set_dynamic_commands_and_arg_completers(
        vec![CommandCompletion::new(":email", "Email approvals")],
        vec![(
            CommandName::new(":email"),
            std::sync::Arc::new(|_| vec![CompletionItem::plain("in")]),
        )],
    );
    assert_eq!(
        completion::build_candidates(&[], &data, ":email ", 7)[0].label,
        "in"
    );

    data.set_dynamic_commands(Vec::new());
    assert!(completion::build_candidates(&[], &data, ":email ", 7).is_empty());
}

/// Keeps the suffix and reports the byte cursor immediately after a
/// replacement.
#[test]
fn argument_completion_reports_mid_buffer_utf8_cursor() {
    let data = CompletionData::new();
    data.set_arg_completions(
        CommandName::new(":model"),
        vec![CompletionItem::plain("日本")],
    );

    let candidate =
        &completion::build_candidates(&[], &data, "  :model 日 suffix", "  :model 日".len())[0];

    assert_eq!(candidate.replacement, "  :model 日本 suffix");
    assert_eq!(candidate.cursor, "  :model 日本".len());
}

/// Reports the insertion point before a suffix when completing an empty token.
#[test]
fn argument_completion_reports_mid_buffer_insertion_cursor() {
    let data = CompletionData::new();
    data.set_arg_completions(
        CommandName::new(":model"),
        vec![CompletionItem::plain("inserted")],
    );

    let candidate = &completion::build_candidates(&[], &data, ":model  suffix", ":model ".len())[0];

    assert_eq!(candidate.replacement, ":model inserted suffix");
    assert_eq!(candidate.cursor, ":model inserted".len());
}

/// Includes command indentation while leaving the cursor before a preserved
/// suffix.
#[test]
fn command_completion_reports_indented_mid_buffer_cursor() {
    let data = CompletionData::new();
    let candidate = &completion::build_candidates(
        &[CommandCompletion::new(":quit", "Exit")],
        &data,
        "  :q suffix",
        "  :q".len(),
    )[0];

    assert_eq!(candidate.replacement, "  :quit suffix");
    assert_eq!(candidate.cursor, "  :quit".len());
}

/// Keeps the established end-of-buffer behavior when no suffix follows
/// completion.
#[test]
fn command_completion_reports_end_of_buffer_cursor() {
    let data = CompletionData::new();
    let candidate = &completion::build_candidates(
        &[CommandCompletion::new(":quit", "Exit")],
        &data,
        ":q",
        ":q".len(),
    )[0];

    assert_eq!(candidate.replacement, ":quit");
    assert_eq!(candidate.cursor, candidate.replacement.len());
}

#[test]
fn typed_history_item_matching_completion_needs_one_up_per_item() {
    let (mut term, handle, input_tx) = new_test_term(vec![
        CommandCompletion::new(":model", "Switch model"),
        CommandCompletion::new(":quit", "Exit"),
    ]);

    submit_typed(&mut term, &input_tx, "Hi");
    submit_typed(&mut term, &input_tx, ":model openai/gpt-5");

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event()
            .expect("navigate to command history item"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":model openai/gpt-5");

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event().expect("continue history navigation"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "Hi");
}

/// Ensures literal-colon escaping stays visible while editing and submitting,
/// but the terminal stores only canonical prompt text for later recall.
#[test]
fn literal_colon_escape_is_canonicalized_in_prompt_history() {
    let (mut term, handle, input_tx) = new_test_term(Vec::new());

    submit_typed(&mut term, &input_tx, "  ::literal");
    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event().expect("recall canonical prompt"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "  :literal");
    assert!(!term.term.trigger_undo());
    assert!(!term.term.trigger_redo());
}

/// Documents exact escape canonicalization, including leading whitespace and
/// the rule that only one colon is removed.
#[test]
fn literal_colon_prompt_canonicalization_removes_exactly_one_colon() {
    assert_eq!(
        canonical_literal_colon_prompt("::text"),
        Some(":text".to_owned())
    );
    assert_eq!(
        canonical_literal_colon_prompt("  :::text"),
        Some("  ::text".to_owned())
    );
    assert_eq!(canonical_literal_colon_prompt(":text"), None);
    assert_eq!(canonical_literal_colon_prompt("text ::later"), None);
}

#[test]
fn history_after_accepting_argument_completion_needs_one_up_per_item() {
    let (mut term, handle, completion_data, input_tx) = new_test_term_with_data(vec![
        CommandCompletion::new(":model", "Switch model"),
        CommandCompletion::new(":quit", "Exit"),
    ]);
    completion_data.set_arg_completions(
        CommandName::new(":model"),
        vec![CompletionItem::plain("openai/gpt-5")],
    );

    submit_typed(&mut term, &input_tx, "Hi");
    type_text(&mut term, &input_tx, ":model op");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("cycle argument completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":model openai/gpt-5");

    send_submit(&input_tx);
    send_submit(&input_tx);
    assert!(matches!(
        term.get_next_event().expect("accept and submit completion"),
        Event::Line(line) if line == ":model openai/gpt-5"
    ));

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event()
            .expect("navigate to completed history item"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":model openai/gpt-5");
    assert!(!term.term.trigger_undo());
    assert!(!term.term.trigger_redo());

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event().expect("continue history navigation"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "Hi");
}

#[test]
fn up_arrow_cycles_completion_after_down_cycles_with_history_present() {
    let (mut term, handle, completion_data, input_tx) = new_test_term_with_data(vec![
        CommandCompletion::new(":model", "Switch model"),
        CommandCompletion::new(":quit", "Exit"),
    ]);
    completion_data.set_arg_completions(
        CommandName::new(":model"),
        vec![
            CompletionItem::plain("anthropic/claude-sonnet-4-5"),
            CompletionItem::plain("openai/gpt-5"),
            CompletionItem::plain("openai/gpt-5-mini"),
        ],
    );

    submit_typed(&mut term, &input_tx, "Hi");
    type_text(&mut term, &input_tx, ":model ");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("cycle to first model"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":model anthropic/claude-sonnet-4-5");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("cycle to second model"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":model openai/gpt-5");

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event().expect("cycle back to first model"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":model anthropic/claude-sonnet-4-5");
}

#[test]
fn up_at_first_match_returns_to_original_buffer_then_wraps() {
    // From idx 0, Up returns to the un-selected state (no preview),
    // restoring the original buffer the user typed. A *second* Up
    // wraps around to the last candidate. This is the symmetric,
    // four-state cycle: None → 0 → 1 → ... → len-1 → 0 → 1 → ...,
    // with one None reachable on the Up-from-0 boundary.
    let (mut term, handle, input_tx) = new_test_term(vec![
        CommandCompletion::new(":model", "Switch model"),
        CommandCompletion::new(":quit", "Exit"),
    ]);

    send_key(&input_tx, KeyCode::Char(':'));
    assert!(matches!(
        term.get_next_event().expect("trigger completion"),
        Event::BufferChanged
    ));

    let sequence: &[(KeyCode, &str)] = &[
        (KeyCode::Down, ":model"),
        (KeyCode::Down, ":quit"),
        (KeyCode::Up, ":model"),
        // Up from idx 0 → no selection → buffer is restored to what
        // the user actually typed.
        (KeyCode::Up, ":"),
        // Continuing Up from None wraps to the last match.
        (KeyCode::Up, ":quit"),
    ];
    for (i, (key, want)) in sequence.iter().enumerate() {
        send_key(&input_tx, *key);
        assert!(matches!(
            term.get_next_event().expect("cycle"),
            Event::BufferChanged
        ));
        assert_eq!(
            handle.get_buffer(),
            *want,
            "step {} ({key:?}): expected {want:?}, got {:?}",
            i + 1,
            handle.get_buffer()
        );
    }
}

#[test]
fn arrows_cycle_repeatedly_through_completion_with_history_present() {
    // With prior submitted lines, Down at the prompt would normally
    // route to history navigation. The mode-driven dispatch in raw
    // gives the open completion menu first claim on Up/Down, so the
    // arrows cycle the menu and the history is never touched.
    let (mut term, handle, input_tx) = new_test_term(vec![
        CommandCompletion::new(":model", "Switch model"),
        CommandCompletion::new(":quit", "Exit"),
    ]);

    submit(&mut term, &handle, &input_tx, "earlier-1");
    submit(&mut term, &handle, &input_tx, "earlier-2");

    send_key(&input_tx, KeyCode::Char(':'));
    assert!(matches!(
        term.get_next_event().expect("trigger completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":");

    let expected = [":model", ":quit", ":model", ":quit"];
    for (i, want) in expected.iter().enumerate() {
        send_key(&input_tx, KeyCode::Down);
        assert!(matches!(
            term.get_next_event().expect("cycle completion"),
            Event::BufferChanged
        ));
        assert_eq!(
            handle.get_buffer(),
            *want,
            "after {} Down keypresses (with history present) the buffer \
             should be {want:?}, got {:?}",
            i + 1,
            handle.get_buffer()
        );
    }
}

#[test]
fn editing_after_preview_commits_it_as_the_new_original_buffer() {
    // Once the user has cycled to a candidate and started editing
    // the previewed text, Esc should drop them back at *the edited
    // preview*, not at the prefix they originally typed before
    // opening the menu. This pins the "every edit commits the prior
    // preview" rule the raw layer documents in `refresh_completion`.
    let (mut term, handle, input_tx) = new_test_term(vec![
        CommandCompletion::new(":model", "Switch model"),
        CommandCompletion::new(":quit", "Exit"),
    ]);

    type_text(&mut term, &input_tx, ":m");
    assert_eq!(handle.get_buffer(), ":m");

    // Cycle to ":model" — buffer now previews the candidate.
    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("preview :model"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":model");

    // Backspace edits the preview. The new buffer (":mode") still
    // matches ":model" by prefix, so the menu re-opens — but with
    // ":mode" as the new original.
    send_key(&input_tx, KeyCode::Backspace);
    assert!(matches!(
        term.get_next_event().expect("backspace edits preview"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":mode");

    // Esc dismisses to the edited preview, not back to ":m".
    send_key(&input_tx, KeyCode::Esc);
    assert!(matches!(
        term.get_next_event()
            .expect("esc returns to edited preview"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":mode");
}

#[test]
fn submit_prompt_binding_submits_line() {
    // The built-in C-Enter binding routes through the configurable
    // action path, but it must still behave like raw Ctrl-Enter.
    let (mut term, handle, _completion_data, input_tx) = new_test_term_with_data_and_bindings(
        Vec::new(),
        vec![("C-Enter".to_owned(), "submit-prompt".to_owned())],
    );

    handle.set_buffer("hello".to_owned(), "hello".len());
    send_submit(&input_tx);

    assert!(matches!(
        term.get_next_event().expect("submit prompt action"),
        Event::Line(line) if line == "hello"
    ));
    assert_eq!(handle.get_buffer(), "");
}

/// Ctrl-V reaches the application toggle without consuming bracketed paste as
/// key input or changing the current prompt draft.
#[test]
fn verbose_mode_binding_preserves_bracketed_paste() {
    let (mut term, handle, _completion_data, input_tx) = new_test_term_with_data_and_bindings(
        Vec::new(),
        vec![("C-v".to_owned(), "verbose-mode-toggle".to_owned())],
    );

    send_key_with_modifiers(&input_tx, KeyCode::Char('v'), KeyModifiers::CONTROL);
    assert!(matches!(
        term.get_next_event().expect("verbose toggle action"),
        Event::Action(action) if action == "verbose-mode-toggle"
    ));
    assert_eq!(handle.get_buffer(), "");

    input_tx
        .send(TestRawEvent::Paste("pasted\npayload".to_owned()))
        .expect("bracketed paste");
    assert!(matches!(
        term.get_next_event().expect("paste event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "pasted\npayload");
}

#[test]
fn submit_prompt_binding_accepts_completion_before_submit() {
    // With a completion preview active, submit-prompt accepts the
    // preview and keeps the user in the prompt. A second press then
    // submits the accepted text, matching raw Ctrl-Enter.
    let (mut term, handle, _completion_data, input_tx) = new_test_term_with_data_and_bindings(
        vec![
            CommandCompletion::new(":model", "Switch model"),
            CommandCompletion::new(":quit", "Exit"),
        ],
        vec![("C-Enter".to_owned(), "submit-prompt".to_owned())],
    );

    type_text(&mut term, &input_tx, ":");
    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("preview first completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":model");

    send_submit(&input_tx);
    send_submit(&input_tx);
    assert!(matches!(
        term.get_next_event().expect("accept then submit"),
        Event::Line(line) if line == ":model"
    ));
}

#[test]
fn named_editing_actions_can_be_rebound() {
    let (mut term, handle, _completion_data, input_tx) = new_test_term_with_data_and_bindings(
        Vec::new(),
        vec![
            ("C-x".to_owned(), "clear-prompt".to_owned()),
            ("C-c".to_owned(), "insert-newline".to_owned()),
            ("Home".to_owned(), "cursor-end".to_owned()),
        ],
    );

    handle.set_buffer("draft".to_owned(), 0);
    send_key(&input_tx, KeyCode::Home);
    assert!(matches!(
        term.get_next_event().expect("cursor-end action"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_cursor(), "draft".len());

    send_key_with_modifiers(&input_tx, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(matches!(
        term.get_next_event().expect("ctrl-c rebound to newline"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "draft\n");

    send_key_with_modifiers(&input_tx, KeyCode::Char('x'), KeyModifiers::CONTROL);
    assert!(matches!(
        term.get_next_event().expect("clear-prompt action"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "");
}

#[test]
fn insert_newline_binding_inserts_newline() {
    // Users can bind any supported key spelling to insert-newline;
    // here plain Enter is bound explicitly instead of relying on the
    // raw fallback.
    let (mut term, handle, _completion_data, input_tx) = new_test_term_with_data_and_bindings(
        Vec::new(),
        vec![("Enter".to_owned(), "insert-newline".to_owned())],
    );

    handle.set_buffer("line one".to_owned(), "line one".len());
    send_key(&input_tx, KeyCode::Enter);

    assert!(matches!(
        term.get_next_event().expect("insert newline action"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "line one\n");
}

mod trailer;

mod filesystem_token;

mod multi_arg_completion;

mod prompt_history_search;

mod prompt_action_parse;

#[test]
fn dismiss_completion_menu_closes_rendered_completion_menu() {
    // Application-level UI transitions such as switching the selected agent do
    // not edit the prompt buffer. They still need a central way to close stale
    // completion UI so the rendered suggestion block cannot stick around.
    let (mut term, handle, input_tx) =
        new_test_term(vec![CommandCompletion::new(":agent", "Manage agents")]);

    type_text(&mut term, &input_tx, ":");
    assert!(handle.completion_state().is_some());
    assert!(term.menu_block_id.is_some());

    assert!(term.dismiss_completion_menu());

    assert!(handle.completion_state().is_none());
    assert!(term.menu_block_id.is_none());
    assert!(!term.dismiss_completion_menu());
}

/// Agent-picker output accepts one UTF-8 row and strips its line terminator.
#[test]
fn agent_fzf_output_parses_one_row() {
    assert_eq!(
        parse_agent_fzf_output(
            "agent-1\tlive\tidle\tidle\tactive\tdurable\tavailable\trole\t-\t1\tname\t$.00/$.00\t🚀\ttitle\t💤\tdisplay\n"
                .as_bytes()
                .to_vec()
        )
        .expect("valid output"),
        Some(
            "agent-1\tlive\tidle\tidle\tactive\tdurable\tavailable\trole\t-\t1\tname\t$.00/$.00\t🚀\ttitle\t💤"
                .to_owned()
        )
    );
    assert_eq!(
        parse_agent_fzf_output(Vec::new()).expect("empty output"),
        None
    );
}

/// Agent-picker output rejects malformed multi-row and non-UTF-8 selections.
#[test]
fn agent_fzf_output_rejects_malformed_selection() {
    assert!(parse_agent_fzf_output(b"agent-1\nagent-2\n".to_vec()).is_err());
    assert!(parse_agent_fzf_output(vec![0xff]).is_err());
    assert!(parse_agent_fzf_output(b"agent-1\tlive\tdisplay\n".to_vec()).is_err());
}

/// Width-aware picker projection aligns columns by terminal display width while
/// preserving Unicode, escaped controls, missing values, and the source TSV.
#[test]
fn agent_picker_rows_align_unicode_and_round_trip_source_rows() {
    let rows = concat!(
        "agent-a\tlive\tidle\tidle\tactive\tdurable\tavailable\tdev\t-\t1\t短名\t$.00/$.00\t🚀\ttrace parser\t💤\n",
        "agent-longer\tlive\trunning\tresponding\tactive_auto\tephemeral\tavailable\t研究員\tparent\t2\t-\t$2.1/$4.3\t⛔️\tawait review\t✨\n",
        "é\tlive\tidle\tidle\tactive\tdurable\tavailable\tline\\nrole\t-\t3\twide界\t-/-\t❓\t-\t💤\n",
    );
    let formatted = format_agent_picker_rows(rows, 100).expect("valid picker rows");
    let formatted_rows = formatted.lines().collect::<Vec<_>>();
    assert_eq!(formatted_rows.len(), 3);

    let displays = formatted_rows
        .iter()
        .map(|row| row.rsplit_once('\t').expect("display field").1)
        .collect::<Vec<_>>();
    assert_eq!(
        displays[0].split_whitespace().take(2).collect::<Vec<_>>(),
        ["🚀💤", "@agent-a"]
    );
    assert_eq!(
        displays[1].split_whitespace().take(2).collect::<Vec<_>>(),
        ["⛔️✨", "@agent-longer"]
    );
    assert_eq!(
        displays[2].split_whitespace().take(2).collect::<Vec<_>>(),
        ["❓💤", "@é"]
    );
    assert!(
        displays
            .iter()
            .all(|display| !display.contains("live") && !display.contains("研究員")),
        "picker presentation must omit lifecycle and role columns: {displays:?}"
    );
    for display in &displays {
        assert!(display_width(display) <= 96);
    }
    let starts = displays
        .iter()
        .zip(["trace parser", "await review", "-"])
        .map(|(display, title)| {
            let title = display.rfind(title).expect("title");
            display_width(&display[..title])
        })
        .collect::<Vec<_>>();
    assert!(starts.windows(2).all(|pair| pair[0] == pair[1]));

    for (source, picker_row) in rows.lines().zip(formatted_rows) {
        assert_eq!(
            parse_agent_fzf_output(format!("{picker_row}\n").into_bytes())
                .expect("picker selection"),
            Some(source.to_owned())
        );
    }
}

/// Once width permits more than identity, status has second priority and lower
/// descriptive columns remain omitted without changing the source row.
#[test]
fn agent_picker_status_has_second_narrow_column_priority() {
    let row = "agent-a\tlive\trunning\tresponding\tactive\tdurable\tavailable\tengineer\t-\t1\tname\t$2.1\t🚀\timplement picker\t✨\n";
    let display = format_agent_picker_rows(row, 30)
        .expect("valid picker row")
        .trim_end()
        .rsplit_once('\t')
        .expect("display field")
        .1
        .to_owned();
    assert_eq!(
        display.split_whitespace().collect::<Vec<_>>(),
        ["🚀✨", "@agent-a", "name"]
    );
}

/// Narrow terminals truncate safely to their display budget and progressively
/// omit trailing columns rather than allowing fzf horizontal scrolling.
#[test]
fn agent_picker_rows_fit_narrow_and_long_values() {
    let rows = format!(
        "{}\tlive\trunning\tresponding\tactive_auto\tdurable\tavailable\t{}\t-\t1\t{}\t$2.1\t🚀\t{}\t✨\n",
        "agent-id-".repeat(20),
        "役割".repeat(30),
        "display-name-".repeat(20),
        "status-title-".repeat(20),
    );
    for terminal_width in [1, 8, 20, 40, 80] {
        let formatted = format_agent_picker_rows(&rows, terminal_width).expect("valid picker row");
        let display = formatted
            .trim_end_matches('\n')
            .rsplit_once('\t')
            .expect("display field")
            .1;
        assert!(
            display_width(display)
                <= terminal_width.saturating_sub(AGENT_PICKER_FZF_DECORATION_WIDTH)
        );
    }
}

/// Practical-width pickers omit low-priority trailing columns before collapsing
/// similar agent identities into indistinguishable prefixes.
#[test]
fn agent_picker_practical_width_preserves_similar_agent_ids() {
    let rows = concat!(
        "reviewer-security-a1\tlive\trunning\tresponding\tactive\tdurable\tavailable\treviewer\t-\t1\tSecurity\t$1.2\t🚀\treviewing trust boundary\t✨\n",
        "reviewer-reliability-b2\tlive\trunning\tresponding\tactive\tdurable\tavailable\treviewer\t-\t2\tReliability\t$1.3\t🚀\treviewing retries\t✨\n",
    );

    for terminal_width in [50, 64] {
        let formatted = format_agent_picker_rows(rows, terminal_width).expect("valid picker rows");
        let displays = formatted
            .lines()
            .map(|row| row.rsplit_once('\t').expect("display field").1)
            .collect::<Vec<_>>();

        assert!(displays.iter().all(|display| display.starts_with("🚀✨ @")));
        assert_ne!(displays[0], displays[1]);
        assert!(
            displays
                .iter()
                .all(|display| display_width(display) <= terminal_width - 4)
        );
    }
}

#[cfg(unix)]
fn fake_fzf(script: &str) -> tempfile::TempPath {
    use std::os::unix::fs::PermissionsExt as _;

    let file = tempfile::NamedTempFile::new().expect("fake fzf file");
    std::fs::write(file.path(), format!("#!/bin/sh\n{script}\n")).expect("write fake fzf");
    let mut permissions = std::fs::metadata(file.path())
        .expect("fake fzf metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(file.path(), permissions).expect("make fake fzf executable");
    file.into_temp_path()
}

#[cfg(unix)]
static AGENT_FZF_TEST_LOCK: std::sync::Mutex<()> = path_std_sync::Mutex::new(());

/// The direct picker passes rows through stdin and returns the exact selected
/// row without requiring a real `fzf` binary in CI.
#[cfg(unix)]
#[test]
fn agent_fzf_command_uses_bounded_direct_process() {
    let _guard = AGENT_FZF_TEST_LOCK.lock().expect("agent fzf test lock");
    let program = fake_fzf(
        r#"set -eu
test "$#" -eq 6
test "$1" = "--height=100%"
test "$2" = "$(printf '%s\t' '--delimiter=')"
test "$3" = "--with-nth=16"
test "$4" = "--no-multi"
test "$5" = "--no-hscroll"
test "$6" = "--prompt=agent> "
cat >/dev/null
printf 'agent-1\tlive\tidle\tidle\tactive\tdurable\tavailable\trole\t-\t1\tname\t$.00/$.00\t🚀\ttitle\t💤\tdisplay\n'"#,
    );

    let selected = run_agent_fzf_command_with_ownership(
        program.as_os_str(),
        "agent-1\tlive\tidle\tidle\tactive\tdurable\tavailable\trole\t-\t1\tname\t$.00/$.00\t🚀\ttitle\t💤\tdisplay\n",
        AGENT_PICKER_TIMEOUT,
        ProcessOwnership::ProcessGroup,
        || Ok(()),
    )
    .expect("fake fzf succeeds");

    assert_eq!(
        selected.as_deref(),
        Some(
            "agent-1\tlive\tidle\tidle\tactive\tdurable\tavailable\trole\t-\t1\tname\t$.00/$.00\t🚀\ttitle\t💤"
        )
    );
}

/// Conventional fzf cancel statuses leave selection unchanged.
#[cfg(unix)]
#[test]
fn agent_fzf_command_treats_cancel_statuses_as_cancel() {
    let _guard = AGENT_FZF_TEST_LOCK.lock().expect("agent fzf test lock");
    for status in [1, 130] {
        let program = fake_fzf(&format!("cat >/dev/null\nexit {status}"));
        let selected = run_agent_fzf_command_with_ownership(
            program.as_os_str(),
            "agent-1\tlive\n",
            AGENT_PICKER_TIMEOUT,
            ProcessOwnership::ProcessGroup,
            || Ok(()),
        )
        .expect("cancel is not an error");
        assert_eq!(selected, None);
    }
}

/// An empty agent roster still releases the terminal and starts fzf, so the
/// picker action has the same observable path for every candidate count.
#[cfg(unix)]
#[test]
fn agent_picker_empty_roster_still_invokes_fzf() {
    let _guard = AGENT_FZF_TEST_LOCK.lock().expect("agent fzf test lock");
    let program = fake_fzf("cat >/dev/null\nexit 1");
    let (term, _handle, _input_tx) = new_test_term(vec![]);
    let pause_count = path_std_rc::Rc::new(path_std_cell::Cell::new(0));
    let resume_count = path_std_rc::Rc::new(path_std_cell::Cell::new(0));
    let spawned = path_std_rc::Rc::new(path_std_cell::Cell::new(false));
    let pause_state = pause_count.clone();
    let resume_state = resume_count.clone();
    let spawned_state = spawned.clone();

    let selected = term
        .pick_agent_row_with_command_and_terminal(
            program.as_os_str(),
            "",
            AGENT_PICKER_TIMEOUT,
            ProcessOwnership::ProcessGroup,
            AgentPickerHooks {
                pause: move || {
                    pause_state.set(pause_state.get() + 1);
                    Ok(())
                },
                resume: move || {
                    resume_state.set(resume_state.get() + 1);
                    Ok(())
                },
                after_spawn: move || {
                    spawned_state.set(true);
                    Ok(())
                },
            },
        )
        .expect("empty picker cancels");

    assert_eq!(selected, None);
    assert!(spawned.get());
    assert_eq!(pause_count.get(), 1);
    assert_eq!(resume_count.get(), 1);
}

/// Spawn failures and non-cancel statuses remain visible picker errors.
#[cfg(unix)]
#[test]
fn agent_fzf_command_reports_missing_program_and_failure_status() {
    let _guard = AGENT_FZF_TEST_LOCK.lock().expect("agent fzf test lock");
    let missing = tempfile::tempdir()
        .expect("missing-program parent")
        .path()
        .join("fzf");
    assert!(
        run_agent_fzf_command_with_ownership(
            missing.as_os_str(),
            "agent-1\tlive\n",
            AGENT_PICKER_TIMEOUT,
            ProcessOwnership::ProcessGroup,
            || Ok(()),
        )
        .is_err()
    );

    let failed = fake_fzf("cat >/dev/null\nexit 2");
    let error = run_agent_fzf_command_with_ownership(
        failed.as_os_str(),
        "agent-1\tlive\n",
        AGENT_PICKER_TIMEOUT,
        ProcessOwnership::ProcessGroup,
        || Ok(()),
    )
    .expect_err("status 2 is an error");
    assert!(error.to_string().contains("status 2"));
}

/// The production picker wiring must retain the documented interactive bound
/// rather than regressing to the prompt command's one-hour allowance.
#[test]
fn agent_picker_production_timeout_is_five_minutes() {
    assert_eq!(AGENT_PICKER_TIMEOUT, std::time::Duration::from_secs(300));
}

/// A nonterminating picker must return promptly, kill descendants in its owned
/// process group, and invoke terminal resume with an actionable picker error.
#[cfg(target_os = "linux")]
#[test]
fn agent_picker_timeout_resumes_terminal_and_kills_descendants() {
    let _guard = AGENT_FZF_TEST_LOCK.lock().expect("agent fzf test lock");
    let pid_dir = tempfile::tempdir().expect("descendant pid directory");
    let pending_pid_file = pid_dir.path().join("descendant.pid.pending");
    let pid_file = pid_dir.path().join("descendant.pid");
    let program = fake_fzf(&format!(
        "sleep 30 &\necho $! > '{}'\nmv '{}' '{}'\nwait",
        pending_pid_file.display(),
        pending_pid_file.display(),
        pid_file.display()
    ));
    let (term, _handle, _input_tx) = new_test_term(vec![]);
    let terminal_paused = path_std_rc::Rc::new(path_std_cell::Cell::new(false));
    let pause_state = terminal_paused.clone();
    let resume_state = terminal_paused.clone();
    let started = path_std_time::Instant::now();

    let error = term
        .pick_agent_row_with_command_and_terminal(
            program.as_os_str(),
            "agent-1\tlive\tidle\tidle\tactive\tdurable\tavailable\trole\t-\t1\tname\t$.00/$.00\t🚀\ttitle\t💤\n",
            path_std_time::Duration::from_millis(100),
            ProcessOwnership::ProcessGroup,
            AgentPickerHooks {
                pause: move || {
                    assert!(!pause_state.replace(true), "terminal paused twice");
                    Ok(())
                },
                resume: move || {
                    assert!(resume_state.replace(false), "terminal was not paused");
                    Ok(())
                },
                after_spawn: || {
                    let readiness_deadline =
                        path_std_time::Instant::now() + path_std_time::Duration::from_secs(2);
                    while !pid_file.exists() {
                        if readiness_deadline <= path_std_time::Instant::now() {
                            return Err("fake fzf did not publish descendant pid".to_owned());
                        }
                        std::thread::yield_now();
                    }
                    Ok(())
                },
            },
        )
        .expect_err("nonterminating picker times out");

    assert!(started.elapsed() < std::time::Duration::from_secs(3));
    assert!(error.to_string().contains("fzf failed: command exceeded"));
    assert!(!terminal_paused.get(), "picker left terminal paused");
    let descendant_pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("read descendant pid")
        .trim()
        .parse()
        .expect("parse descendant pid");
    match std::fs::read_to_string(format!("/proc/{descendant_pid}/stat")) {
        Ok(stat) => assert_eq!(
            stat.split_whitespace().nth(2),
            Some("Z"),
            "picker descendant remained runnable after process-group cleanup"
        ),
        Err(error) => assert_eq!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "could not inspect picker descendant cleanup"
        ),
    }
}
