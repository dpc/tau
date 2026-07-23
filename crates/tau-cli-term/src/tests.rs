use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;

fn new_test_term_with_data_and_bindings(
    commands: Vec<CommandCompletion>,
    bindings: impl IntoIterator<Item = (String, String)>,
) -> (
    HighTerm,
    TermHandle,
    CompletionData,
    std::sync::mpsc::Sender<TestRawEvent>,
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
    std::sync::mpsc::Sender<TestRawEvent>,
) {
    new_test_term_with_data_and_bindings(commands, std::iter::empty::<(String, String)>())
}

fn new_test_term(
    commands: Vec<CommandCompletion>,
) -> (HighTerm, TermHandle, std::sync::mpsc::Sender<TestRawEvent>) {
    let (term, handle, _completion_data, input_tx) = new_test_term_with_data(commands);
    (term, handle, input_tx)
}

fn send_key(input_tx: &std::sync::mpsc::Sender<TestRawEvent>, code: KeyCode) {
    send_key_with_modifiers(input_tx, code, KeyModifiers::NONE);
}

fn send_key_with_modifiers(
    input_tx: &std::sync::mpsc::Sender<TestRawEvent>,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    input_tx
        .send(TestRawEvent::Key(KeyEvent::new(code, modifiers)))
        .expect("send key");
}

fn send_submit(input_tx: &std::sync::mpsc::Sender<TestRawEvent>) {
    send_key_with_modifiers(input_tx, KeyCode::Enter, KeyModifiers::CONTROL);
}

fn submit(
    term: &mut HighTerm,
    handle: &TermHandle,
    input_tx: &std::sync::mpsc::Sender<TestRawEvent>,
    line: &str,
) {
    handle.set_buffer(line.to_owned(), line.len());
    send_submit(input_tx);
    assert!(matches!(
        term.get_next_event().expect("submit line"),
        Event::Line(submitted) if submitted == line
    ));
}

fn type_text(term: &mut HighTerm, input_tx: &std::sync::mpsc::Sender<TestRawEvent>, text: &str) {
    for ch in text.chars() {
        send_key(input_tx, KeyCode::Char(ch));
        assert!(matches!(
            term.get_next_event().expect("type char"),
            Event::BufferChanged
        ));
    }
}

fn submit_typed(term: &mut HighTerm, input_tx: &std::sync::mpsc::Sender<TestRawEvent>, line: &str) {
    type_text(term, input_tx, line);
    send_submit(input_tx);
    assert!(matches!(
        term.get_next_event().expect("submit typed line"),
        Event::Line(submitted) if submitted == line
    ));
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

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event().expect("continue history navigation"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "Hi");
}

#[test]
fn history_items_matching_completion_do_not_steal_following_history_navigation() {
    let (mut term, handle, input_tx) = new_test_term(vec![
        CommandCompletion::new(":model", "Switch model"),
        CommandCompletion::new(":quit", "Exit"),
    ]);

    submit(&mut term, &handle, &input_tx, "Hi");
    submit(&mut term, &handle, &input_tx, ":model openai/gpt-5");

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
fn arrows_cycle_active_completion_even_when_history_exists() {
    let (mut term, handle, input_tx) = new_test_term(vec![
        CommandCompletion::new(":model", "Switch model"),
        CommandCompletion::new(":quit", "Exit"),
    ]);

    submit(&mut term, &handle, &input_tx, "Hi");

    send_key(&input_tx, KeyCode::Char(':'));
    assert!(matches!(
        term.get_next_event().expect("trigger completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event()
            .expect("cycle completion with history present"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":model");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event()
            .expect("cycle completion again with history present"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":quit");
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
fn arrows_cycle_repeatedly_through_completion_suggestions() {
    // Down four times should cycle: :model, :quit, :model, :quit.
    // Wrapping is the normal `(i + 1) mod len` — the None state is
    // only reachable via Up at idx 0.
    let (mut term, handle, input_tx) = new_test_term(vec![
        CommandCompletion::new(":model", "Switch model"),
        CommandCompletion::new(":quit", "Exit"),
    ]);

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
            "after {} Down keypresses the buffer should be {want:?}, got {:?}",
            i + 1,
            handle.get_buffer()
        );
    }
}

#[test]
fn arrows_still_cycle_active_completion_suggestions() {
    let (mut term, handle, input_tx) = new_test_term(vec![
        CommandCompletion::new(":model", "Switch model"),
        CommandCompletion::new(":quit", "Exit"),
    ]);

    send_key(&input_tx, KeyCode::Char(':'));
    assert!(matches!(
        term.get_next_event().expect("trigger completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("cycle completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":model");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("cycle completion again"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":quit");
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
            b"agent-1\tlive\tidle\tactive\tdurable\tavailable\trole\t-\t1\tname\tdisplay\n"
                .to_vec()
        )
        .expect("valid output"),
        Some("agent-1\tlive\tidle\tactive\tdurable\tavailable\trole\t-\t1\tname".to_owned())
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
        "agent-a\tlive\tidle\tactive\tdurable\tavailable\tdev\t-\t1\t短名\n",
        "agent-longer\tlive\trunning\tactive_auto\tephemeral\tavailable\t研究員\tparent\t2\t-\n",
        "é\tlive\tidle\tactive\tdurable\tavailable\tline\\nrole\t-\t3\twide界\n",
    );
    let formatted = format_agent_picker_rows(rows, 100).expect("valid picker rows");
    let formatted_rows = formatted.lines().collect::<Vec<_>>();
    assert_eq!(formatted_rows.len(), 3);

    let displays = formatted_rows
        .iter()
        .map(|row| row.rsplit_once('\t').expect("display field").1)
        .collect::<Vec<_>>();
    for display in &displays {
        assert!(display_width(display) <= 96);
    }
    let starts = displays
        .iter()
        .map(|display| {
            let role = display
                .find(if display.contains("dev") {
                    "dev"
                } else if display.contains("研究員") {
                    "研究員"
                } else {
                    "line\\nrole"
                })
                .expect("role");
            display_width(&display[..role])
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

/// Narrow terminals truncate safely to their display budget and progressively
/// omit trailing columns rather than allowing fzf horizontal scrolling.
#[test]
fn agent_picker_rows_fit_narrow_and_long_values() {
    let rows = format!(
        "{}\tlive\trunning\tactive_auto\tdurable\tavailable\t{}\t-\t1\t{}\n",
        "agent-id-".repeat(20),
        "役割".repeat(30),
        "display-name-".repeat(20),
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
static AGENT_FZF_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
test "$3" = "--with-nth=11"
test "$4" = "--no-multi"
test "$5" = "--no-hscroll"
test "$6" = "--prompt=agent> "
cat >/dev/null
printf 'agent-1\tlive\tidle\tactive\tdurable\tavailable\trole\t-\t1\tname\tdisplay\n'"#,
    );

    let selected = run_agent_fzf_command_with_ownership(
        program.as_os_str(),
        "agent-1\tlive\tidle\tactive\tdurable\tavailable\trole\t-\t1\tname\tdisplay\n",
        ProcessOwnership::ProcessGroup,
    )
    .expect("fake fzf succeeds");

    assert_eq!(
        selected.as_deref(),
        Some("agent-1\tlive\tidle\tactive\tdurable\tavailable\trole\t-\t1\tname")
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
            ProcessOwnership::ProcessGroup,
        )
        .expect("cancel is not an error");
        assert_eq!(selected, None);
    }
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
            ProcessOwnership::ProcessGroup,
        )
        .is_err()
    );

    let failed = fake_fzf("cat >/dev/null\nexit 2");
    let error = run_agent_fzf_command_with_ownership(
        failed.as_os_str(),
        "agent-1\tlive\n",
        ProcessOwnership::ProcessGroup,
    )
    .expect_err("status 2 is an error");
    assert!(error.contains("status 2"));
}
