use super::*;

/// Helper: builds Cell lines from plain strings.
fn plain_lines(texts: &[&str]) -> Vec<Vec<Cell>> {
    texts
        .iter()
        .map(|s| s.chars().map(Cell::plain).collect())
        .collect()
}

fn line_text(line: &[Cell]) -> String {
    line.iter().map(|cell| cell.ch).collect()
}

/// Helper: runs full_render into a vt100 parser and returns it.
///
/// `history_lines` is the number of lines at the top of
/// `all_lines` that belong to history (before the live area).
fn run_full_render(
    rows: u16,
    cols: u16,
    all_lines: Vec<Vec<Cell>>,
    history_lines: usize,
    cursor_row: usize,
    cursor_col: usize,
) -> (vt100::Parser, Screen) {
    let mut term = vt100::Parser::new(rows, cols, 200);
    let mut screen = Screen::new(cols as usize);
    let mut buf: Vec<u8> = Vec::new();

    let line_sources = (0..all_lines.len())
        .map(|wrapped_row| LineSource::Input { wrapped_row })
        .collect();
    let layout = LayoutAll {
        all_lines,
        line_sources,
        log_end: history_lines,
        history_generation: 0,
        history_width: cols as usize,
        cursor_row,
        cursor_col,
    };
    let plan = TerminalModel::default().plan_view(&layout, rows as usize);

    full_render(
        &mut buf,
        &mut screen,
        &layout,
        &plan,
        cols as usize,
        rows as usize,
        usize::MAX,
    )
    .expect("full_render should succeed");

    term.process(&buf);
    (term, screen)
}

/// Helper: visible rows as trimmed strings.
fn visible_rows(term: &vt100::Parser) -> Vec<String> {
    let (_, cols) = term.screen().size();
    term.screen().rows(0, cols).collect()
}

/// External-program pause must release terminal feature modes that Tau enabled
/// so child editors do not receive focus-reporting escape sequences.
#[test]
fn external_pause_disables_and_resume_enables_focus_reporting() {
    let mut pause = Vec::new();
    write_external_pause_features(&mut pause).expect("pause features");
    let pause = String::from_utf8(pause).expect("utf8 pause escapes");
    assert!(pause.contains("\u{1b}[?1004l"), "pause: {pause:?}");

    let mut resume = Vec::new();
    write_external_resume_features(&mut resume, CursorShape::Bar).expect("resume features");
    let resume = String::from_utf8(resume).expect("utf8 resume escapes");
    assert!(resume.contains("\u{1b}[?1004h"), "resume: {resume:?}");
}

/// Real terminal poll errors must propagate through `get_next_event` instead
/// of being treated as EOF.
#[test]
fn real_raw_event_propagates_poll_errors() {
    let result = read_real_raw_event(
        || false,
        |_| Err(io::Error::other("synthetic poll error")),
        || unreachable!("read should not be called"),
        || Ok((80, 24)),
    );
    let err = match result {
        Ok(_) => panic!("poll error should propagate"),
        Err(err) => err,
    };

    assert_eq!(err.to_string(), "synthetic poll error");
}

/// Real terminal read errors must propagate through `get_next_event` instead
/// of being treated as EOF.
#[test]
fn real_raw_event_propagates_read_errors() {
    let result = read_real_raw_event(
        || false,
        |_| Ok(true),
        || Err(io::Error::other("synthetic read error")),
        || Ok((80, 24)),
    );
    let err = match result {
        Ok(_) => panic!("read error should propagate"),
        Err(err) => err,
    };

    assert_eq!(err.to_string(), "synthetic read error");
}

/// Ctrl-W word deletion should stop at any Unicode whitespace separator, not
/// just ASCII spaces.
#[test]
fn word_left_boundary_uses_unicode_whitespace() {
    assert_eq!(word_left_boundary("foo\nbar", "foo\nbar".len()), 4);
    assert_eq!(word_left_boundary("foo\tbar", "foo\tbar".len()), 4);
    assert_eq!(
        word_left_boundary("foo\u{2003}bar", "foo\u{2003}bar".len()),
        "foo\u{2003}".len()
    );
    assert_eq!(word_left_boundary("foo  bar   ", "foo  bar   ".len()), 5);
}

// --- full_render: content overflows terminal height ---

/// Full redraw is allowed to write past the viewport; this locks in which rows
/// stay visible and which enter terminal scrollback.
#[test]
fn full_render_overflow_visible_and_scrollback() {
    // 3 history lines + 4 live lines = 7 total, 5-row terminal.
    let lines = plain_lines(&[
        "history 0",
        "history 1",
        "history 2",
        "above A",
        "above B",
        "> hello",
        "below",
    ]);
    let (mut term, _screen) = run_full_render(5, 30, lines, 3, 5, 7);

    // Visible: last 5 lines (indices 2..7).
    let vis = visible_rows(&term);
    assert_eq!(vis[0], "history 2");
    assert_eq!(vis[1], "above A");
    assert_eq!(vis[2], "above B");
    assert_eq!(vis[3], "> hello");
    assert_eq!(vis[4], "below");

    // Scrollback: indices 0..2.
    term.screen_mut().set_scrollback(2);
    let sb = visible_rows(&term);
    assert_eq!(sb[0], "history 0");
    assert_eq!(sb[1], "history 1");
}

/// After an overflowing full redraw, cursor coordinates and the retained Screen
/// cache must both be relative to the physical viewport.
#[test]
fn full_render_overflow_cursor_and_screen_state() {
    // 3 history + 4 live = 7, 5-row terminal.
    let lines = plain_lines(&[
        "history 0",
        "history 1",
        "history 2",
        "above A",
        "above B",
        "> hello",
        "below",
    ]);
    let (term, screen) = run_full_render(5, 30, lines, 3, 5, 7);

    // Terminal cursor: row 5 is "> hello", viewport_top=2,
    // live_start=3, cursor_in_live=2 → screen row = 3.
    let (r, c) = term.screen().cursor_position();
    assert_eq!(r, 3, "cursor row in viewport");
    assert_eq!(c, 7, "cursor col");

    // Screen tracks the visible viewport (5 lines).
    assert_eq!(
        screen.actual_line_count(),
        5,
        "screen tracks visible viewport"
    );
}

/// Full redraw history limiting should clear scrollback and replay only the
/// most recent rendered history rows plus the fixed prompt area, so slow remote
/// terminals do not receive the entire old transcript again.
#[test]
fn full_render_limits_replayed_history_rows() {
    let all_lines = plain_lines(&[
        "hist 0", "hist 1", "hist 2", "hist 3", "hist 4", "hist 5", "> prompt",
    ]);
    let layout = LayoutAll {
        line_sources: (0..all_lines.len())
            .map(|wrapped_row| LineSource::Input { wrapped_row })
            .collect(),
        all_lines,
        log_end: 6,
        history_generation: 0,
        history_width: 30,
        cursor_row: 6,
        cursor_col: 8,
    };
    let plan = TerminalModel::default().plan_view(&layout, 10);
    let mut buf = Vec::new();
    let mut screen = Screen::new(30);

    full_render(&mut buf, &mut screen, &layout, &plan, 30, 10, 2).expect("render");

    let text = String::from_utf8_lossy(&buf);
    assert!(!text.contains("hist 0"));
    assert!(!text.contains("hist 3"));
    assert!(text.contains("hist 4"));
    assert!(text.contains("hist 5"));
    assert!(text.contains("> prompt"));
    assert_eq!(screen.actual_line_count(), 3);
    assert_eq!(screen.cursor_row(), 2);
}

// --- full_render: content shorter than terminal ---

/// Cursor shape settings should use steady crossterm styles so Tau does not
/// accidentally request blinking cursors.
#[test]
fn cursor_shape_maps_to_steady_styles() {
    assert_eq!(
        CursorShape::Bar.crossterm_style().to_string(),
        crossterm::cursor::SetCursorStyle::SteadyBar.to_string()
    );
    assert_eq!(
        CursorShape::Block.crossterm_style().to_string(),
        crossterm::cursor::SetCursorStyle::SteadyBlock.to_string()
    );
}

/// Short full-render content should start at the top without synthetic padding
/// rows.
#[test]
fn full_render_short_content_at_top() {
    // 0 history + 3 live = 3, 10-row terminal.
    // Content starts at the top (no blank padding).
    let lines = plain_lines(&["above", "> hi", "below"]);
    let (term, _screen) = run_full_render(10, 30, lines, 0, 1, 4);

    let vis = visible_rows(&term);
    assert_eq!(vis[0], "above");
    assert_eq!(vis[1], "> hi");
    assert_eq!(vis[2], "below");
    // Rest is empty.
    for (i, row) in vis.iter().enumerate().take(10).skip(3) {
        assert_eq!(row, "", "row {i} should be blank");
    }
}

/// For non-overflowing full redraws, cursor placement and retained cache rows
/// should match the original content indices.
#[test]
fn full_render_short_content_cursor() {
    // 0 history + 3 live = 3, 10-row terminal.
    let lines = plain_lines(&["above", "> hi", "below"]);
    let (term, screen) = run_full_render(10, 30, lines, 0, 1, 4);

    // Content starts at the top. cursor_row=1 → screen row 1.
    let (r, c) = term.screen().cursor_position();
    assert_eq!(r, 1, "cursor row");
    assert_eq!(c, 4, "cursor col");

    // Screen tracks the visible viewport (3 lines).
    assert_eq!(
        screen.actual_line_count(),
        3,
        "screen tracks visible viewport"
    );
}

// --- full_render: exact fit ---

/// Exact-fit full redraws are the boundary between short and overflowing
/// content, so cursor and cache math must not branch incorrectly.
#[test]
fn full_render_exact_fit() {
    // 2 history + 3 live = 5, 5-row terminal.
    let lines = plain_lines(&["hist 0", "hist 1", "> cmd", "status A", "status B"]);
    let (term, screen) = run_full_render(5, 30, lines, 2, 2, 5);

    let vis = visible_rows(&term);
    assert_eq!(vis[0], "hist 0");
    assert_eq!(vis[4], "status B");

    // cursor_row=2, live_start=2, cursor_in_live=0.
    // Screen row = 0 (padding) + 2 (live_start) + 0 = 2.
    // Wait — viewport_top = 0 for exact fit, live_screen_start = 0 + 2 = 2.
    let (r, c) = term.screen().cursor_position();
    assert_eq!(r, 2, "cursor row");
    assert_eq!(c, 5, "cursor col");

    // Screen tracks the visible viewport (5 lines).
    assert_eq!(screen.actual_line_count(), 5);
}

/// When fixed prompt/status content is taller than the terminal, retained state
/// must cap to the physical viewport instead of log boundaries.
#[test]
fn full_render_caps_visible_state_when_fixed_area_exceeds_height() {
    // Two history rows plus six fixed rows (status/suggestions/below),
    // rendered into a three-row terminal. The physical viewport starts
    // inside the fixed area, not at log_end.
    let all_lines = plain_lines(&[
        "hist 0",
        "hist 1",
        "status",
        "> prompt",
        "suggestion",
        "below 0",
        "below 1",
        "below 2",
    ]);
    let mut term = vt100::Parser::new(3, 30, 200);
    let mut screen = Screen::new(30);
    let mut buf = Vec::new();
    let layout = LayoutAll {
        line_sources: (0..all_lines.len())
            .map(|wrapped_row| LineSource::Input { wrapped_row })
            .collect(),
        all_lines,
        log_end: 2,
        history_generation: 0,
        history_width: 30,
        cursor_row: 3,
        cursor_col: 8,
    };
    let plan = TerminalModel::bottom_aligned_plan(&layout, 3);

    full_render(&mut buf, &mut screen, &layout, &plan, 30, 3, usize::MAX).expect("render");
    term.process(&buf);

    assert_eq!(visible_rows(&term), vec!["below 0", "below 1", "below 2"]);
    assert_eq!(screen.actual_line_count(), 3);
}

/// Cursor positioning after full redraw must subtract the physical viewport
/// start, not the history/live split.
#[test]
fn full_render_cursor_uses_physical_viewport_start() {
    let all_lines = plain_lines(&["hist 0", "hist 1", "live 0", "> prompt", "below"]);
    let mut term = vt100::Parser::new(3, 30, 200);
    let mut screen = Screen::new(30);
    let mut buf = Vec::new();
    let layout = LayoutAll {
        line_sources: (0..all_lines.len())
            .map(|wrapped_row| LineSource::Input { wrapped_row })
            .collect(),
        all_lines,
        log_end: 2,
        history_generation: 0,
        history_width: 30,
        cursor_row: 3,
        cursor_col: 8,
    };
    let plan = TerminalModel::bottom_aligned_plan(&layout, 3);

    full_render(&mut buf, &mut screen, &layout, &plan, 30, 3, usize::MAX).expect("render");
    term.process(&buf);

    assert_eq!(visible_rows(&term), vec!["live 0", "> prompt", "below"]);
    assert_eq!(term.screen().cursor_position(), (1, 8));
    assert_eq!(screen.actual_line_count(), 3);
}

/// A resize full redraw should bottom-align real content directly and discard
/// any previous rubber-gap assumptions.
#[test]
fn full_render_resize_to_larger_bottom_aligns_without_rubber() {
    let all_lines = plain_lines(&[
        "hist 0", "hist 1", "hist 2", "hist 3", "hist 4", "hist 5", "hist 6", "hist 7", "hist 8",
        "hist 9", "> prompt",
    ]);
    let layout = LayoutAll {
        line_sources: (0..all_lines.len())
            .map(|wrapped_row| LineSource::Input { wrapped_row })
            .collect(),
        all_lines,
        log_end: 10,
        history_generation: 0,
        history_width: 30,
        cursor_row: 10,
        cursor_col: 8,
    };
    let mut model = TerminalModel {
        viewport_start: 6,
        rubber_height: 0,
        history_generation: 0,
        history_width: 30,
        known_lines: Vec::new(),
        known_sources: Vec::new(),
    };

    let plan = TerminalModel::bottom_aligned_plan(&layout, 10);
    model.reset_to_plan(layout, plan.viewport_start, plan.rubber_height);

    assert_eq!(plan.rubber_height, 0);
    assert_eq!(plan.viewport_start, 1);
    assert_eq!(model.viewport_start, 1);
}

/// A diff render after full redraw must compare against the retained visible
/// viewport so subsequent live updates remain incremental.
#[test]
fn full_render_then_diff_render() {
    // After full_render, Screen tracks the live area.
    // A subsequent Screen::update (as render_live does) should
    // diff only against the live area.
    //
    // 0 history + 3 live = 3, 10-row terminal.
    let lines = plain_lines(&["above", "> hello", "below"]);
    let (_term, mut screen) = run_full_render(10, 30, lines, 0, 1, 7);

    // Screen should track 3 lines (visible viewport).
    assert_eq!(screen.actual_line_count(), 3);

    // Diff update: change "> hello" to "> world".
    let live_lines2 = plain_lines(&["above", "> world", "below"]);
    let mut buf2: Vec<u8> = Vec::new();
    screen
        .update(&mut buf2, &live_lines2, (1, 7))
        .expect("update should succeed");

    assert!(!buf2.is_empty(), "diff should produce output");
}

/// Empty prompt rendering may show hint text, but the editable buffer and
/// cursor must stay empty so typing replaces the hint instead of appending to
/// it.
#[test]
fn empty_input_renders_placeholder_without_moving_cursor() {
    let mut st = SharedState::new(80, 24, "> ".into());
    st.input_placeholder = Span::new(
        "Write a message to engineer...",
        Style::default().fg(Color::DarkGrey).italic(),
    )
    .into();

    let layout = layout_all(&st);

    assert_eq!(
        line_text(&layout.all_lines[0]),
        "> Write a message to engineer..."
    );
    assert_eq!(layout.cursor_row, 0);
    assert_eq!(layout.cursor_col, 2);
    assert_eq!(st.buffer, "");
    assert_eq!(st.cursor, 0);
    assert!(layout.all_lines[0][2].style.italic);
}

/// Documents the prompt-history contract: submitted entries are navigable while
/// the unsent draft is restored at the end.
#[test]
fn input_history_navigates_submitted_and_draft_entries() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(24, 80, 0);

    let (term, handle, input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf.clone()), CursorShape::Bar);

    handle.set_buffer("first draft".to_owned(), "first draft".len());
    flush_redraws(&handle, &buf, &mut parser);

    handle.set_buffer("one".to_owned(), 3);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("send enter");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::Line(line) if line == "one"
    ));

    handle.set_buffer("two".to_owned(), 3);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("send enter");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::Line(line) if line == "two"
    ));

    handle.set_buffer("draft".to_owned(), 5);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "two");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "one");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("send down");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "two");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("send down");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "draft");
}
/// Public buffer setters accept byte offsets from higher-level UI code; invalid
/// offsets inside a Unicode grapheme must be normalized before edit operations
/// use them with `String::insert`/`drain`.
#[test]
fn set_buffer_clamps_cursor_to_grapheme_boundary() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("éx".to_owned(), 1);
    assert_eq!(handle.get_cursor(), 0);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::NONE,
        )))
        .expect("send inserted char");
    assert!(matches!(
        term.get_next_event().expect("insert event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "zéx");

    handle.set_buffer_preserving_undo("éx".to_owned(), 1);
    assert_eq!(handle.get_cursor(), 0);

    drop(term);
}

/// Redraw suppression must be scoped: callers use it around arbitrary snapshot
/// updates, and the counter must be restored after the closure returns so
/// future redraws are not permanently suppressed.
#[test]
fn redraw_suppression_is_scoped() {
    let buf = SharedBuffer::new();
    let (term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.with_redraw_suppressed(|| {
        handle.redraw();
        {
            let mut st = handle.lock();
            assert_eq!(st.redraw_suppression, 1);
            assert!(st.redraw_dirty_while_suppressed);
            st.redraw_dirty_while_suppressed = false;
        }

        handle.print_output("suppressed-output", plain_block("hidden update"));
        {
            let mut st = handle.lock();
            assert!(st.redraw_dirty_while_suppressed);
            st.redraw_dirty_while_suppressed = false;
        }

        handle.print_terminal_escape("\x1b]1337;CurrentDir=/tmp\x07");
        {
            let mut st = handle.lock();
            assert!(st.redraw_dirty_while_suppressed);
            st.redraw_dirty_while_suppressed = false;
        }

        let snapshot = handle.output_snapshot();
        handle.replace_output_snapshot(snapshot);
        let st = handle.lock();
        assert!(st.redraw_dirty_while_suppressed);
    });

    {
        let st = handle.lock();
        assert_eq!(st.redraw_suppression, 0);
        assert!(!st.redraw_dirty_while_suppressed);
    }

    drop(term);
}

/// Input shutdown requests must wake a virtual input loop without requiring an
/// injected key event; the real terminal path checks the same flag between
/// short crossterm polls.
#[test]
fn input_shutdown_request_returns_eof() {
    let buf = SharedBuffer::new();
    let (term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.request_input_shutdown();

    assert!(matches!(term.get_next_event(), Ok(Event::Eof)));
}

/// Recalling a queued prompt should insert it immediately before the current
/// draft in history navigation order so one Down restores the interrupted
/// draft.
#[test]
fn recalled_prompt_sits_before_current_draft() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("draft".to_owned(), 5);
    handle.recall_prompt_before_current("queued".to_owned());
    assert_eq!(handle.get_buffer(), "queued");
    assert_eq!(handle.get_cursor(), 6);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("send down");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "draft");
}

/// Escape outside the completion menu is surfaced so callers can request a
/// harness-side queued-prompt recall instead of racing local UI state.
#[test]
fn escape_outside_completion_surfaces_event() {
    let buf = SharedBuffer::new();
    let (term, _handle, input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("send esc");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::Escape
    ));
}

/// Seeded history from previous sessions should appear before the current draft
/// when navigating upward.
#[test]
fn seeded_input_history_is_recalled_before_current_draft() {
    let buf = SharedBuffer::new();
    let (mut term, handle, input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    term.seed_input_history(["old one".to_owned(), "old two".to_owned()]);

    handle.set_buffer("draft".to_owned(), 5);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "old two");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "old one");
}

/// Pressing Down from a non-empty draft creates a fresh editable prompt while
/// keeping the draft reachable via history.
#[test]
fn down_from_non_empty_draft_creates_fresh_prompt_and_history_entry() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("draft".to_owned(), 5);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("send down");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "");
    assert_eq!(handle.get_cursor(), 0);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "draft");
    // Column-preserving: empty buffer cursor sat at the prompt
    // edge (visual col 2), so Up lands at byte 0 of "draft" — also
    // the prompt edge of the previous entry's last (and only) row.
    assert_eq!(handle.get_cursor(), 0);
}

/// History Up from a multi-line draft should preserve visual column on the
/// previous entry's last row.
#[test]
fn up_lands_at_last_row_same_col_in_previous_entry() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("alphabet".to_owned(), 8);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("enter");
    let _ = term.get_next_event().expect("event");

    // Cursor on row 0 visual col 4 (byte 2 = "be|ta\nworld").
    handle.set_buffer("beta\nworld".to_owned(), 2);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "alphabet");
    // Visual col 4 with left prompt "> " (2 cols) → byte 2 of "alphabet".
    assert_eq!(handle.get_cursor(), 2);
}

/// History Down should mirror Up by landing on the next entry's first row at
/// the preserved visual column.
#[test]
fn down_lands_at_first_row_same_col_in_next_entry() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("first\nlonger".to_owned(), "first\nlonger".len());
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("enter");
    let _ = term.get_next_event().expect("event");

    handle.set_buffer("second".to_owned(), 6);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("enter");
    let _ = term.get_next_event().expect("event");

    // Draft "abc" with cursor at byte 1 (visual col 3 = "a|bc").
    handle.set_buffer("abc".to_owned(), 1);

    // Up → "second", visual col 3 → byte 1 ("s|econd").
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_buffer(), "second");
    assert_eq!(handle.get_cursor(), 1);

    // Up → "first\nlonger", last row at visual col 3 → byte 9
    // ("first\nlon|ger").
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_buffer(), "first\nlonger");
    assert_eq!(handle.get_cursor(), 9);

    // Down → "second", first row at visual col 3 → byte 1.
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("down");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_buffer(), "second");
    assert_eq!(handle.get_cursor(), 1);
}

/// Vertical motion inside a buffer must remember the intended column even when
/// an intermediate row is too short.
#[test]
fn down_preserves_sticky_column_across_short_line_in_buffer() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    // Three rows: long / short / long. Cursor on row 0 visual col 6
    // (byte 4 = "abcd|ef").
    handle.set_buffer("abcdef\nx\nabcdef".to_owned(), 4);

    // Down truncates onto "x" at byte 8 (just after 'x', visual col 1).
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("down");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 8);

    // Down again restores visual col 6 (sticky preserved through the
    // short row): byte 15 = end of buffer ("abcdef" on row 2).
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("down");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 15);
}

/// Typing after vertical motion establishes a new column so future Up/Down does
/// not use stale sticky-column state.
#[test]
fn typing_clears_sticky_column() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("abcdef\nx\nabcdef".to_owned(), 4);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("down");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 8);

    // Typing a char clears sticky and re-bases the column at the new
    // cursor (visual col 2 after "xy").
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('y'),
            KeyModifiers::NONE,
        )))
        .expect("y");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 9);

    // Down lands at visual col 2 of row 2, NOT the original col 6:
    // byte 12 ("ab|cdef") instead of byte 16 (end).
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("down");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 12);
}

/// History navigation should preserve the desired column across short entries
/// instead of permanently clamping it.
#[test]
fn step_history_preserves_sticky_column_across_short_entry() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    // Submit three entries with a short one in the middle — the
    // short entry will clamp the sticky column locally but must not
    // permanently truncate it for the next step.
    for line in ["abcdef", "x", "xyzabc"] {
        handle.set_buffer(line.to_owned(), line.len());
        input_tx
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::CONTROL,
            )))
            .expect("ctrl+enter");
        let _ = term.get_next_event().expect("event");
    }

    // Draft "draft" cursor=4 → visual col 6 ("draf|t").
    handle.set_buffer("draft".to_owned(), 4);

    // Up → "xyzabc" at visual col 6 → byte 4.
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_buffer(), "xyzabc");
    assert_eq!(handle.get_cursor(), 4);

    // Up → "x" (short middle entry). Cursor clamps to end-of-line.
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_buffer(), "x");
    assert_eq!(handle.get_cursor(), 1);

    // Up → "abcdef": sticky col 6 survived the short entry, so cursor
    // lands at byte 4 ("abcd|ef") rather than the start of the line.
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_buffer(), "abcdef");
    assert_eq!(handle.get_cursor(), 4);
}

/// Upward motion through a short in-buffer row should keep the original column
/// for the next row.
#[test]
fn up_preserves_sticky_column_across_short_line_in_buffer() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    // Cursor at end of row 2 (visual col 6).
    handle.set_buffer("abcdef\nx\nabcdef".to_owned(), 15);

    // Up onto "x": truncated to byte 8 (after 'x').
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 8);

    // Up again restores visual col 6 on row 0: byte 4 ("abcd|ef").
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 4);
}

/// The sticky column chosen while moving through a multi-line draft must carry
/// into history navigation.
#[test]
fn sticky_column_carries_from_buffer_into_history() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("pqrstuv".to_owned(), 7);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("enter");
    let _ = term.get_next_event().expect("event");

    // 3-row buffer with empty middle row. Cursor at end (row 2 col 3).
    handle.set_buffer("abcdef\n\nxyz".to_owned(), 11);

    // Up → end of empty middle row at byte 7 (col 0).
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 7);

    // Up → row 0 of current buffer. Sticky col 3 (set on first Up)
    // is preserved through the empty row, so we land at byte 1
    // ("a|bcdef") instead of the start.
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 1);

    // Up → step_history into "pqrstuv". Sticky col 3 still in
    // effect, so cursor lands at byte 1 of "pqrstuv" ("p|qrstuv").
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_buffer(), "pqrstuv");
    assert_eq!(handle.get_cursor(), 1);
}

/// Editing with Backspace should reset sticky-column state so later vertical
/// moves follow the edited cursor position.
#[test]
fn backspace_clears_sticky_column() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("abcdef\nx\nabcdef".to_owned(), 4);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("down");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 8);

    // Backspace deletes 'x', clears sticky. Buffer becomes
    // "abcdef\n\nabcdef", cursor=7 (start of empty middle row).
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )))
        .expect("backspace");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_buffer(), "abcdef\n\nabcdef");
    assert_eq!(handle.get_cursor(), 7);

    // Down uses current col (0) — not sticky 6. Lands at byte 8
    // (start of row 2) instead of byte 14 (sticky-preserved col 6).
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("down");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 8);
}

/// Horizontal cursor movement intentionally abandons sticky-column state before
/// the next vertical move.
#[test]
fn left_clears_sticky_column() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("abcdef\nx\nabcdef".to_owned(), 4);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("down");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 8);

    // Left clears sticky (no event), then Down uses recomputed col.
    // Left moves cursor to byte 7 (start of "x", visual col 0). Down
    // lands at byte 9 (start of row 2) instead of 15 (sticky col 6).
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::NONE,
        )))
        .expect("left");
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("down");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 9);
}

/// Home jumps to the prompt edge and should reset vertical sticky state, even
/// when later rows are short.
#[test]
fn home_clears_sticky_column() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("abcdef\nx\nabcdef".to_owned(), 4);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("down");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 8);

    // Home → cursor=0 (visual col 2, prompt edge). Down then lands
    // at byte 8 (after 'x' on row 1) — col 2 → truncated to last
    // available position on the short row. Without clearing sticky,
    // col 6 would have given the same byte 8 here, so step further
    // and assert: another Down lands at byte 11 (visual col 2 on
    // row 2 = "ab|cdef"), not byte 15 (col 6).
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Home,
            KeyModifiers::NONE,
        )))
        .expect("home");
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("down");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 8);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("down");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 11);
}

/// Home, End, Ctrl-A, and Ctrl-E reset vertical sticky-column state even when
/// the cursor is already at the requested boundary; this preserves the raw key
/// path's historical unconditional `write_cursor` side effect.
#[test]
fn boundary_cursor_keys_clear_sticky_column_even_when_cursor_does_not_move() {
    let buf = SharedBuffer::new();
    let (term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    for key in [
        KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
    ] {
        handle.set_buffer("abcdef".to_owned(), 0);
        handle.lock().sticky_col = Some(6);
        assert!(
            term.handle_key(key).expect("home/control-a key").is_none(),
            "boundary-start key should not emit an event"
        );
        assert_eq!(handle.get_cursor(), 0);
        assert_eq!(handle.lock().sticky_col, None);
    }

    for key in [
        KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
    ] {
        handle.set_buffer("abcdef".to_owned(), 6);
        handle.lock().sticky_col = Some(6);
        assert!(
            term.handle_key(key).expect("end/control-e key").is_none(),
            "boundary-end key should not emit an event"
        );
        assert_eq!(handle.get_cursor(), 6);
        assert_eq!(handle.lock().sticky_col, None);
    }
}

/// Ctrl-U should keep emitting `BufferChanged` and refreshing prompt state even
/// at cursor zero; higher-level redraw/completion code historically relied on
/// that raw-key event rather than treating the boundary press as a no-op.
#[test]
fn ctrl_u_at_cursor_zero_still_emits_buffer_changed() {
    let buf = SharedBuffer::new();
    let (term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("abcdef".to_owned(), 0);
    let event = term
        .handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
        .expect("ctrl-u key");

    assert!(matches!(event, Some(Event::BufferChanged)));
    assert_eq!(handle.get_buffer(), "abcdef");
    assert_eq!(handle.get_cursor(), 0);
}

/// Ctrl-Up bypasses in-buffer vertical motion and jumps to history while
/// preserving the current visual column.
#[test]
fn ctrl_up_jumps_to_history_with_column_preserved() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("xyzw".to_owned(), 4);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("enter");
    let _ = term.get_next_event().expect("event");

    // Cursor on row 1 of multi-line draft at visual col 4 (byte 10
    // = "abcde\nfghi|j").
    handle.set_buffer("abcde\nfghij".to_owned(), 10);

    // Plain Up would move within the buffer. Ctrl-Up bypasses that
    // and goes straight to history, preserving visual col 4 → byte 2
    // of "xyzw" ("xy|zw").
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::CONTROL,
        )))
        .expect("ctrl-up");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_buffer(), "xyzw");
    assert_eq!(handle.get_cursor(), 2);
}

/// Ctrl-K/Ctrl-J history shortcuts should share the same column-preserving
/// behavior as arrow-key history navigation.
#[test]
fn ctrl_k_steps_history_back_with_column_preserved() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("xyzw".to_owned(), 4);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("enter");
    let _ = term.get_next_event().expect("event");

    handle.set_buffer("abc".to_owned(), 1);

    // Ctrl-K → step_history(-1), preserving visual col 3.
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
        )))
        .expect("ctrl-k");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_buffer(), "xyzw");
    assert_eq!(handle.get_cursor(), 1);

    // Ctrl-J → step_history(+1), preserving column. Lands back on
    // the WIP draft "abc" at visual col 3 → byte 1 ("a|bc").
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL,
        )))
        .expect("ctrl-j");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_buffer(), "abc");
    assert_eq!(handle.get_cursor(), 1);
}

/// Ctrl-C on an empty prompt should avoid accidental exits: the first press
/// warns, and the second consecutive press asks the caller to cancel the LLM
/// response rather than exiting the prompt.
#[test]
fn ctrl_c_empty_prompt_requires_second_press_to_cancel() {
    let buf = SharedBuffer::new();
    let (term, _handle, input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))
        .expect("first ctrl-c");

    match term.get_next_event().expect("event") {
        Event::Notice(message) => assert_eq!(
            message,
            "Press Ctrl-C again to cancel the current response; use Ctrl-D to exit"
        ),
        _ => panic!("expected notice"),
    }

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))
        .expect("second ctrl-c");

    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::CancelPrompt
    ));
}

/// Any intervening key should disarm the empty-prompt Ctrl-C cancel guard so
/// the user must press Ctrl-C twice consecutively to cancel.
#[test]
fn ctrl_c_cancel_guard_resets_after_other_key() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))
        .expect("first ctrl-c");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::Notice(_)
    ));

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )))
        .expect("type x");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    handle.set_buffer(String::new(), 0);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))
        .expect("ctrl-c after reset");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::Notice(_)
    ));
}

/// Clearing a non-empty prompt with Ctrl-C should participate in undo/redo like
/// other buffer edits.
#[test]
fn ctrl_c_clear_can_be_undone_and_redone() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    handle.set_buffer("draft".to_owned(), 5);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))
        .expect("ctrl-c");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "");

    assert!(term.trigger_undo());
    assert_eq!(handle.get_buffer(), "draft");
    assert_eq!(handle.get_cursor(), 5);

    assert!(term.trigger_redo());
    assert_eq!(handle.get_buffer(), "");
    assert_eq!(handle.get_cursor(), 0);
}

/// Undo state belongs to the edited history entry and must survive leaving and
/// returning to that entry.
#[test]
fn undo_state_follows_history_entry() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    for ch in "first".chars() {
        input_tx
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )))
            .expect("char");
        let _ = term.get_next_event().expect("event");
    }
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("enter");
    let _ = term.get_next_event().expect("event");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_buffer(), "first");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::End,
            KeyModifiers::NONE,
        )))
        .expect("end");
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('!'),
            KeyModifiers::NONE,
        )))
        .expect("bang");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_buffer(), "first!");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("down");
    let _ = term.get_next_event().expect("event");
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up again");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_buffer(), "first!");

    assert!(term.trigger_undo());
    assert_eq!(handle.get_buffer(), "first");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("down after undo");
    let _ = term.get_next_event().expect("event");
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up after undo");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_buffer(), "first");
}

/// Wrapped single-line input should use visual columns, not byte offsets, for
/// Up/Down cursor movement.
#[test]
fn vertical_motion_uses_visual_column_in_wrapped_line() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(10, 5, "> ", Box::new(buf), CursorShape::Bar);

    // "abcdefghijkl" with width=10 and a 2-col left prompt wraps to:
    //   row 0: "> abcdefgh"  (cols 2..10, h at col 9)
    //   row 1: "ijkl"        (i at col 0, l at col 3)
    // Cursor at end → visual position (1, 4).
    handle.set_buffer("abcdefghijkl".to_owned(), 12);

    // Up: target visual col 4 → row 0 col 4 → byte 2 ("ab|cdefghijkl").
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up");
    let _ = term.get_next_event().expect("event");
    assert_eq!(handle.get_cursor(), 2);
}

/// Regression guard for the WIP history slot: after returning to a draft, Down
/// must push the new draft again and reset the prompt.
#[test]
fn down_at_wip_slot_in_nav_mode_pushes_and_resets() {
    // Repro: after a Down has pushed once, navigating Up then
    // editing the WIP slot and pressing Down should push again.
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("first".to_owned(), 5);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("send down");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "first");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("send down");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "");

    for ch in "second".chars() {
        input_tx
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )))
            .expect("send char");
        let _ = term.get_next_event().expect("event");
    }
    assert_eq!(handle.get_buffer(), "second");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("send down");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "");
    assert_eq!(handle.get_cursor(), 0);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "second");
}

/// Empty prompts should not create hidden history entries when navigated,
/// preventing later recall of blank submissions.
#[test]
fn down_from_empty_prompt_does_not_create_history_entry() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    // Down/Up on an empty prompt with no history is a no-op and
    // surfaces no event. Send a follow-up Enter that submits the
    // (still empty) buffer; if Down had wrongly pushed an empty
    // entry into `input_history`, a subsequent Up would recall it.
    for code in [KeyCode::Down, KeyCode::Up] {
        input_tx
            .send(RawEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
            .expect("send key");
    }
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("send enter");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::Line(line) if line.is_empty()
    ));
    assert_eq!(handle.get_buffer(), "");
    assert_eq!(handle.get_cursor(), 0);

    // No history entries exist, so Up is again a no-op. Verify by
    // sending a typed character afterwards and confirming the
    // BufferChanged it produces shows just that character — no
    // recalled history line.
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )))
        .expect("send char");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "x");
}

/// Up inside a multi-line draft should move within the draft before stepping
/// into history.
#[test]
fn vertical_motion_stays_within_multiline_buffer_before_history() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(10, 5, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("abc\ndef".to_owned(), "abc\ndef".len());

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("send up");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "abc\ndef");
    assert_eq!(handle.get_cursor(), 1);
}

/// A diff after a full redraw with history must update visible rows
/// incrementally while keeping history rows in the cache.
#[test]
fn full_render_then_diff_with_history() {
    // 3 history + 2 live = 5, 5-row terminal.
    // Screen tracks all 5 visible lines. A diff update that
    // changes only the live portion should produce minimal output.
    let lines = plain_lines(&["h0", "h1", "h2", "> cmd", "status"]);
    let (_term, mut screen) = run_full_render(5, 30, lines, 3, 3, 5);

    assert_eq!(screen.actual_line_count(), 5, "visible viewport tracked");

    // Update: change "> cmd" to "> new" (history unchanged).
    let visible2 = plain_lines(&["h0", "h1", "h2", "> new", "status"]);
    let mut buf2: Vec<u8> = Vec::new();
    screen.update(&mut buf2, &visible2, (3, 5)).expect("ok");

    assert!(!buf2.is_empty());
}

// --- Virtual terminal E2E tests ---

/// Shared buffer that implements Write for the redraw thread
/// and can be drained into a vt100 parser by the test.
#[derive(Clone)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl SharedBuffer {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    /// Drain accumulated bytes into a vt100 parser.
    fn drain_into(&self, parser: &mut vt100::Parser) {
        let mut buf = self.0.lock().expect("shared buffer poisoned");
        if !buf.is_empty() {
            parser.process(&buf);
            buf.clear();
        }
    }
}

impl io::Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("shared buffer poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Helper: get visible rows from a vt100 parser as trimmed strings.
fn vt100_rows(parser: &vt100::Parser, cols: u16) -> Vec<String> {
    parser.screen().rows(0, cols).collect()
}

/// Helper: check if any visible row contains the given text.
fn screen_contains(parser: &vt100::Parser, cols: u16, text: &str) -> bool {
    vt100_rows(parser, cols).iter().any(|r| r.contains(text))
}

/// Helper: trigger a sync redraw and drain output into the parser.
fn flush_redraws(handle: &TermHandle, buf: &SharedBuffer, parser: &mut vt100::Parser) {
    handle.redraw_sync();
    buf.drain_into(parser);
}

fn plain_block(text: impl Into<String>) -> StyledBlock {
    StyledBlock::new(StyledText::from(Span::plain(text.into())))
}

fn assert_no_full_redraw_after(
    handle: &TermHandle,
    buf: &SharedBuffer,
    parser: &mut vt100::Parser,
    action: impl FnOnce(),
) {
    let before = handle.full_render_count();
    action();
    flush_redraws(handle, buf, parser);
    assert_eq!(
        handle.full_render_count(),
        before,
        "operation should not require full redraw"
    );
}

fn assert_full_redraw_after(
    handle: &TermHandle,
    buf: &SharedBuffer,
    parser: &mut vt100::Parser,
    action: impl FnOnce(),
) {
    let before = handle.full_render_count();
    action();
    flush_redraws(handle, buf, parser);
    assert_eq!(
        handle.full_render_count(),
        before + 1,
        "operation should require exactly one full redraw"
    );
}

/// After a capped full redraw, the renderer's internal viewport must remain
/// clipped so later ordinary differential redraws do not reintroduce history
/// rows that were intentionally omitted from terminal scrollback.
#[test]
fn clipped_full_redraw_keeps_later_diff_redraws_clipped() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 20);

    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    handle.set_redraw_history_size(1);
    for idx in 0..4 {
        handle.print_output(format!("hist-{idx}"), plain_block(format!("hist {idx}")));
    }
    flush_redraws(&handle, &buf, &mut parser);

    assert_full_redraw_after(&handle, &buf, &mut parser, || {
        handle.invalidate_screen();
    });
    assert!(!screen_contains(&parser, 40, "hist 0"));
    assert!(!screen_contains(&parser, 40, "hist 2"));
    assert!(screen_contains(&parser, 40, "hist 3"));

    let full_renders = handle.full_render_count();
    handle.set_right_prompt(StyledText::from("status"));
    handle.redraw();
    flush_redraws(&handle, &buf, &mut parser);

    assert_eq!(handle.full_render_count(), full_renders);
    assert!(!screen_contains(&parser, 40, "hist 0"));
    assert!(!screen_contains(&parser, 40, "hist 2"));
    assert!(screen_contains(&parser, 40, "hist 3"));
    assert!(screen_contains(&parser, 40, "status"));
}

/// Pasting multiline text should normalize layout and cursor state so the
/// rendered terminal matches the buffer.
#[test]
fn multiline_buffer_layout_tracks_cursor_after_paste() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(30, 10, 20);

    let (term, handle, input_tx) =
        Term::new_virtual(10, 30, "> ", Box::new(buf.clone()), CursorShape::Bar);

    input_tx
        .send(RawEvent::Paste("abc\ndefghijkl".to_owned()))
        .expect("send paste");
    assert!(matches!(
        term.get_next_event().expect("paste event"),
        Event::BufferChanged
    ));
    flush_redraws(&handle, &buf, &mut parser);

    assert_eq!(handle.get_buffer(), "abc\ndefghijkl");
    assert_eq!(handle.get_cursor(), "abc\ndefghijkl".len());
    assert_eq!(&vt100_rows(&parser, 10)[..2], &["> abc", "defghijkl"]);
    assert_eq!(parser.screen().cursor_position(), (1, 9));
}

/// Long prompts must scroll the viewport to keep the cursor visible, then let
/// edits update that viewport correctly.
#[test]
fn long_multiline_prompt_scrolls_viewport_to_cursor() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 10, 20);

    let (term, handle, input_tx) =
        Term::new_virtual(10, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    let text = (0..8)
        .map(|idx| format!("line{idx:02}"))
        .collect::<Vec<_>>()
        .join("\n");

    input_tx
        .send(RawEvent::Paste(text.clone()))
        .expect("send paste");
    assert!(matches!(
        term.get_next_event().expect("paste event"),
        Event::BufferChanged
    ));
    flush_redraws(&handle, &buf, &mut parser);
    assert_eq!(vt100_rows(&parser, 10)[0], "line07");

    let full_renders = handle.full_render_count();
    for _ in 0..5 {
        input_tx
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::NONE,
            )))
            .expect("send up");
        assert!(matches!(
            term.get_next_event().expect("up event"),
            Event::BufferChanged
        ));
    }
    flush_redraws(&handle, &buf, &mut parser);

    assert_eq!(vt100_rows(&parser, 10)[0], "line02");
    assert_eq!(parser.screen().cursor_position(), (0, 6));
    assert_eq!(handle.full_render_count(), full_renders);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('X'),
            KeyModifiers::NONE,
        )))
        .expect("send char");
    assert!(matches!(
        term.get_next_event().expect("type event"),
        Event::BufferChanged
    ));
    flush_redraws(&handle, &buf, &mut parser);

    assert!(handle.get_buffer().contains("line02X\nline03"));
    assert_eq!(vt100_rows(&parser, 10)[0], "line02X");
    assert_eq!(parser.screen().cursor_position(), (0, 7));
}

/// Regression guard from `fix(cli-term-raw): normalize pasted newlines`: CRLF
/// paste input should render and position the cursor like LF input.
#[test]
fn paste_normalizes_crlf_so_cursor_matches_rendered_multiline_buffer() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(30, 10, 20);

    let (term, handle, input_tx) =
        Term::new_virtual(10, 30, "> ", Box::new(buf.clone()), CursorShape::Bar);

    input_tx
        .send(RawEvent::Paste("abc\r\ndefghijkl".to_owned()))
        .expect("send paste");
    assert!(matches!(
        term.get_next_event().expect("paste event"),
        Event::BufferChanged
    ));
    flush_redraws(&handle, &buf, &mut parser);

    assert_eq!(handle.get_buffer(), "abc\ndefghijkl");
    assert_eq!(handle.get_cursor(), "abc\ndefghijkl".len());
    assert_eq!(&vt100_rows(&parser, 10)[..2], &["> abc", "defghijkl"]);
    assert_eq!(parser.screen().cursor_position(), (1, 9));
}

/// Unit-level guard for the byte/visual-position helpers used by multiline
/// prompt navigation.
#[test]
fn multiline_buffer_vertical_cursor_motion_uses_visual_lines() {
    let width = 10;
    let left_cols = 2;
    let text = "abc\ndefghijkl";

    let (row, col) = buffer_position_for_byte(text, text.len(), width, left_cols);
    assert_eq!((row, col), (1, 9));

    let up = byte_offset_for_buffer_position(text, 0, 5, width, left_cols);
    assert_eq!(up, 3);

    let down = byte_offset_for_buffer_position(text, 1, 9, width, left_cols);
    assert_eq!(down, text.len());
}

/// Prompt cursor math must use the same grapheme-aware widths as rendering, or
/// input containing emoji will visibly drift from the cursor position.
#[test]
fn prompt_cursor_math_counts_emoji_graphemes() {
    let width = 4;
    let left_cols = 2;
    let text = "⚠️";

    let (row, col) = buffer_position_for_byte(text, text.len(), width, left_cols);
    assert_eq!((row, col), (1, 0));
    assert_eq!(
        byte_offset_for_buffer_position(text, 1, 0, width, left_cols),
        text.len()
    );
}

/// CRLF is one Unicode grapheme cluster, but prompt layout should treat it as a
/// single line break, matching pasted-text normalization and block rendering.
#[test]
fn prompt_cursor_math_treats_crlf_as_newline() {
    let text = "a\r\nb";
    assert_eq!(buffer_position_for_byte(text, text.len(), 10, 2), (1, 1));
    assert_eq!(
        byte_offset_for_buffer_position(text, 1, 1, 10, 2),
        text.len()
    );
}

/// Inverse cursor mapping must mirror prompt pending-wrap handling. A newline
/// immediately after an exact-width row consumes the pending wrap; it must not
/// capture targets that visually belong to the following text row.
#[test]
fn byte_offset_after_exact_width_newline_uses_following_text() {
    let text = "abcdefgh\nZ";
    assert_eq!(buffer_position_for_byte(text, text.len(), 10, 2), (1, 1));
    assert_eq!(
        byte_offset_for_buffer_position(text, 1, 1, 10, 2),
        text.len()
    );
}

/// Horizontal editing movement should treat complex emoji as one grapheme
/// cluster so cursor movement and deletion cannot split their byte sequences.
#[test]
fn prompt_boundaries_move_by_grapheme_cluster() {
    for grapheme in ["⚠️", "👩‍💻", "👍🏽", "a\u{0301}"] {
        assert_eq!(
            next_char_boundary(grapheme, 0),
            grapheme.len(),
            "{grapheme:?}"
        );
        assert_eq!(
            prev_char_boundary(grapheme, grapheme.len()),
            0,
            "{grapheme:?}"
        );
    }
}

// Regression guard for prompt-input cursor wrapping. The final-column case is
// prompt-only cursor behavior, not generic block wrapping behavior.
#[test]
fn prompt_input_cursor_uses_last_column_before_line_is_full() {
    let mut st = SharedState::new(10, 30, StyledText::from("> "));
    st.buffer = "abcdefg".to_owned();
    st.cursor = st.buffer.len();

    let layout = layout_all(&st);

    assert_eq!(layout.all_lines.len(), 1, "prompt height");
    assert_eq!(line_text(&layout.all_lines[0]), "> abcdefg");
    assert_eq!((layout.cursor_row, layout.cursor_col), (0, 9));
}

// Commonly broken property: once prompt input fills the last column, the cursor
// must immediately live on the next visual row at column 0.
#[test]
fn prompt_input_cursor_wraps_to_new_line_when_last_column_is_filled() {
    let mut st = SharedState::new(10, 30, StyledText::from("> "));
    st.buffer = "abcdefgh".to_owned();
    st.cursor = st.buffer.len();

    let layout = layout_all(&st);

    assert_eq!(layout.all_lines.len(), 2, "prompt height");
    assert_eq!(line_text(&layout.all_lines[0]), "> abcdefgh");
    assert_eq!(line_text(&layout.all_lines[1]), "");
    assert_eq!((layout.cursor_row, layout.cursor_col), (1, 0));
}

// Regression guard for Shift+Enter after an exact-width prompt line.
// The previous printable character already moved the cursor to the next
// visual row; the explicit newline must consume that pending wrap, not create
// an additional phantom blank row below it.
#[test]
fn prompt_input_newline_after_filled_line_does_not_add_phantom_row() {
    let mut st = SharedState::new(10, 30, StyledText::from("> "));
    st.buffer = "abcdefgh\n".to_owned();
    st.cursor = st.buffer.len();

    let layout = layout_all(&st);

    assert_eq!(layout.all_lines.len(), 2, "prompt height");
    assert_eq!(line_text(&layout.all_lines[0]), "> abcdefgh");
    assert_eq!(line_text(&layout.all_lines[1]), "");
    assert_eq!((layout.cursor_row, layout.cursor_col), (1, 0));
}

// After an exact-width line followed by a newline, newly typed text must land
// on the immediate next row. This catches the bug where the cursor was drawn
// one row too low while inserted characters appeared on the row above.
#[test]
fn prompt_input_text_after_newline_after_filled_line_keeps_cursor_on_text_row() {
    let mut st = SharedState::new(10, 30, StyledText::from("> "));
    st.buffer = "abcdefgh\nZ".to_owned();
    st.cursor = st.buffer.len();

    let layout = layout_all(&st);

    assert_eq!(layout.all_lines.len(), 2, "prompt height");
    assert_eq!(line_text(&layout.all_lines[0]), "> abcdefgh");
    assert_eq!(line_text(&layout.all_lines[1]), "Z");
    assert_eq!((layout.cursor_row, layout.cursor_col), (1, 1));
}

// Multi-line version of the same regression: every exact-width line ending in
// an explicit newline used to add one more phantom row, so the cursor drifted
// farther down with each full line. Cursor accounting and rendered prompt
// height must stay in lockstep for all lines.
#[test]
fn prompt_input_repeated_full_lines_ending_in_newline_do_not_stack_phantom_rows() {
    let mut st = SharedState::new(10, 30, StyledText::from("> "));
    st.buffer = "abcdefgh\nabcdefghij\n".to_owned();
    st.cursor = st.buffer.len();

    let layout = layout_all(&st);

    assert_eq!(layout.all_lines.len(), 3, "prompt height");
    assert_eq!(line_text(&layout.all_lines[0]), "> abcdefgh");
    assert_eq!(line_text(&layout.all_lines[1]), "abcdefghij");
    assert_eq!(line_text(&layout.all_lines[2]), "");
    assert_eq!((layout.cursor_row, layout.cursor_col), (2, 0));
}

#[test]
fn prompt_input_height_cap_uses_floor_third_with_minimum_one() {
    assert_eq!(prompt_input_max_rows(0), 1);
    assert_eq!(prompt_input_max_rows(1), 1);
    assert_eq!(prompt_input_max_rows(3), 1);
    assert_eq!(prompt_input_max_rows(9), 2);
    assert_eq!(prompt_input_max_rows(12), 3);
}

#[test]
fn prompt_input_layout_caps_height_and_shows_hidden_row_indicator() {
    let mut st = SharedState::new(12, 12, StyledText::from("> "));
    st.buffer = "a\nb\nc\nd\ne".to_owned();
    st.write_cursor(st.buffer.len());

    let layout = layout_all(&st);

    assert_eq!(layout.all_lines.len(), 3, "cap floor(12*33%) = 3 rows");
    assert!(line_text(&layout.all_lines[0]).starts_with('↕'));
    assert_eq!(line_text(&layout.all_lines[1]), "d");
    assert_eq!(line_text(&layout.all_lines[2]), "e");
    assert_eq!(layout.line_sources[0], LineSource::InputScrollIndicator);
}

#[test]
fn prompt_input_cap_one_keeps_editable_row_and_suppresses_indicator() {
    let mut st = SharedState::new(12, 1, StyledText::from("> "));
    st.buffer = "a\nb\nc".to_owned();
    st.write_cursor(st.buffer.len());

    let layout = layout_all(&st);

    assert_eq!(layout.all_lines.len(), 1);
    assert_eq!(line_text(&layout.all_lines[0]), "c");
    assert_eq!(layout.line_sources[0], LineSource::Input { wrapped_row: 2 });
}

#[test]
fn prompt_input_scroll_indicator_can_be_disabled() {
    let mut st = SharedState::new(12, 9, StyledText::from("> "));
    st.show_prompt_scroll_indicator = false;
    st.buffer = "a\nb\nc\nd".to_owned();
    st.write_cursor(st.buffer.len());

    let layout = layout_all(&st);

    assert_eq!(layout.all_lines.len(), 2, "cap floor(9*33%) = 2 rows");
    assert_eq!(line_text(&layout.all_lines[0]), "c");
    assert_eq!(line_text(&layout.all_lines[1]), "d");
}

#[test]
fn prompt_input_resize_clamps_viewport_when_more_rows_fit() {
    let mut st = SharedState::new(12, 12, StyledText::from("> "));
    st.buffer = "a\nb\nc\nd\ne".to_owned();
    st.write_cursor(st.buffer.len());
    assert_eq!(st.input_viewport_start, 3);

    st.height = 30;
    st.ensure_input_cursor_visible();

    assert_eq!(st.input_viewport_start, 0);
    let layout = layout_all(&st);
    assert_eq!(layout.all_lines.len(), 5);
}

#[test]
fn prompt_input_plain_up_scrolls_before_history_navigation() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(12, 12, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("hist".to_owned(), 4);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("submit history");
    let _ = term.get_next_event().expect("submit event");

    handle.set_buffer("a\nb\nc\nd\ne".to_owned(), 5);
    {
        let mut st = handle.lock();
        st.input_viewport_start = 2;
    }
    assert_eq!(handle.lock().input_viewport_start, 2);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up");
    let _ = term.get_next_event().expect("up event");

    assert_eq!(handle.get_buffer(), "a\nb\nc\nd\ne");
    assert_eq!(handle.lock().input_viewport_start, 1);

    for _ in 0..2 {
        input_tx
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::NONE,
            )))
            .expect("up");
        let _ = term.get_next_event().expect("up event");
    }
    assert_eq!(handle.get_buffer(), "hist");
}

#[test]
fn prompt_input_explicit_history_shortcut_bypasses_local_scroll() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(12, 12, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("hist".to_owned(), 4);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("submit history");
    let _ = term.get_next_event().expect("submit event");

    handle.set_buffer("a\nb\nc\nd\ne".to_owned(), 9);
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::CONTROL,
        )))
        .expect("ctrl-up");
    let _ = term.get_next_event().expect("ctrl-up event");

    assert_eq!(handle.get_buffer(), "hist");
}

#[test]
fn prompt_input_completion_menu_keeps_priority_over_local_scroll() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(12, 12, "> ", Box::new(buf), CursorShape::Bar);
    handle.set_buffer("a\nb\nc\nd\ne".to_owned(), 9);
    {
        let mut st = handle.lock();
        st.completion = Some(CompletionMenu {
            candidates: vec![Candidate {
                label: "x".to_owned(),
                description: "candidate".to_owned(),
                replacement: "replacement".to_owned(),
            }],
            selected: None,
            original_buffer: st.buffer.clone(),
            original_cursor: st.cursor,
        });
    }

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up");
    let _ = term.get_next_event().expect("up event");

    assert_eq!(handle.get_buffer(), "replacement");
}

#[test]
fn prompt_input_indicator_fits_tiny_terminal_width() {
    let mut st = SharedState::new(1, 7, StyledText::from(""));
    st.buffer = "a\nb\nc".to_owned();
    st.write_cursor(st.buffer.len());

    let layout = layout_all(&st);
    let indicator = line_text(&layout.all_lines[0]);

    assert_eq!(layout.line_sources[0], LineSource::InputScrollIndicator);
    assert!(
        display_width(&indicator) <= 1,
        "indicator must not wrap: {indicator:?}"
    );
}

#[test]
fn prompt_input_plain_down_scrolls_before_history_navigation() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(12, 12, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("a\nb\nc\nd\ne".to_owned(), 5);
    {
        let mut st = handle.lock();
        st.input_viewport_start = 1;
    }
    assert_eq!(handle.lock().input_viewport_start, 1);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("down");
    let _ = term.get_next_event().expect("down event");

    assert_eq!(handle.get_buffer(), "a\nb\nc\nd\ne");
    assert_eq!(handle.lock().input_viewport_start, 2);
}

#[test]
fn prompt_input_explicit_next_history_shortcut_bypasses_local_scroll() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(12, 12, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("a\nb\nc\nd\ne".to_owned(), 5);
    {
        let mut st = handle.lock();
        st.input_viewport_start = 1;
    }

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::CONTROL,
        )))
        .expect("ctrl-down");
    let _ = term.get_next_event().expect("ctrl-down event");

    assert_eq!(handle.get_buffer(), "");
}

/// Virtual terminal smoke test: constructing Term should render the prompt on
/// the redraw thread.
#[test]
fn virtual_term_shows_prompt() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(24, 80, 0);

    let (term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf.clone()), CursorShape::Bar);

    flush_redraws(&handle, &buf, &mut parser);

    assert!(screen_contains(&parser, 80, "> "));

    drop(term);
}

/// Direct buffer updates should flow through layout and redraw so typed input
/// appears next to the prompt.
#[test]
fn virtual_term_renders_typed_input() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(24, 80, 0);

    let (_term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf.clone()), CursorShape::Bar);

    // Simulate typing by setting the buffer directly (avoids
    // needing to drive the input event loop).
    handle.set_buffer("hello".to_owned(), 5);
    flush_redraws(&handle, &buf, &mut parser);

    assert!(
        screen_contains(&parser, 80, "> hello"),
        "expected '> hello' on screen, got: {:?}",
        vt100_rows(&parser, 80)
    );
}

/// Editing the prompt after persistent history has scrolled away must keep the
/// viewport stable and update only the prompt tail. This is the user-visible
/// case protected by the cached-history redraw fast path.
#[test]
fn prompt_edit_after_scrolled_history_updates_prompt_tail() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 80);

    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    for i in 0..20 {
        handle.print_output("history", plain_block(format!("history line {i}")));
    }
    flush_redraws(&handle, &buf, &mut parser);

    let full_render_count = handle.full_render_count();
    handle.set_buffer("x".to_owned(), 1);
    flush_redraws(&handle, &buf, &mut parser);

    assert_eq!(
        handle.full_render_count(),
        full_render_count,
        "prompt-only edits should stay on the incremental path"
    );
    assert!(
        screen_contains(&parser, 40, "> x"),
        "edited prompt should be visible, got: {:?}",
        vt100_rows(&parser, 40)
    );
}

/// Printed output blocks should be included in the virtual terminal frame, not
/// only stored in the model.
#[test]
fn virtual_term_renders_print_output() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(24, 80, 0);

    let (_term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf.clone()), CursorShape::Bar);

    handle.print_output(
        "test",
        StyledBlock::new(StyledText::from(Span::plain("Hello from output"))),
    );

    flush_redraws(&handle, &buf, &mut parser);

    assert!(
        screen_contains(&parser, 80, "Hello from output"),
        "expected output on screen, got: {:?}",
        vt100_rows(&parser, 80)
    );
}

/// Updating a live block should replace visible content in place and clear the
/// previous text.
#[test]
fn virtual_term_updates_block_in_place() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(24, 80, 0);

    let (_term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf.clone()), CursorShape::Bar);

    // Create a block in above_active (live area).
    let block_id = handle.new_block(
        "test",
        StyledBlock::new(StyledText::from(Span::plain("loading..."))),
    );
    handle.push_above_active(block_id);
    handle.redraw();

    flush_redraws(&handle, &buf, &mut parser);
    assert!(screen_contains(&parser, 80, "loading..."));

    // Update it in place.
    handle.set_block(
        block_id,
        StyledBlock::new(StyledText::from(Span::plain("done!"))),
    );
    handle.redraw();

    flush_redraws(&handle, &buf, &mut parser);
    assert!(
        screen_contains(&parser, 80, "done!"),
        "expected 'done!' on screen, got: {:?}",
        vt100_rows(&parser, 80)
    );
    assert!(
        !screen_contains(&parser, 80, "loading..."),
        "old content should be gone"
    );
}

/// Streaming-finalization path: active partial output is removed and final
/// output is printed to history without leaving stale partial text.
#[test]
fn virtual_term_block_removed_from_active_then_printed_to_history() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(24, 80, 0);

    let (_term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf.clone()), CursorShape::Bar);

    // Simulate streaming: create live block, update, finalize.
    let block_id = handle.new_block(
        "test",
        StyledBlock::new(StyledText::from(Span::plain("streaming..."))),
    );
    handle.push_above_active(block_id);
    handle.redraw();
    flush_redraws(&handle, &buf, &mut parser);

    // Update with partial text.
    handle.set_block(
        block_id,
        StyledBlock::new(StyledText::from(Span::plain("partial response"))),
    );
    handle.redraw();
    flush_redraws(&handle, &buf, &mut parser);
    assert!(screen_contains(&parser, 80, "partial response"));

    // Finalize: remove live block, print to history.
    handle.remove_block(block_id);
    handle.print_output(
        "test",
        StyledBlock::new(StyledText::from(Span::plain("final response"))),
    );
    flush_redraws(&handle, &buf, &mut parser);

    assert!(
        screen_contains(&parser, 80, "final response"),
        "final should be visible, got: {:?}",
        vt100_rows(&parser, 80)
    );
    // The old "partial response" should be gone — only "final response" remains.
    assert!(
        !screen_contains(&parser, 80, "partial response"),
        "partial should be gone, got: {:?}",
        vt100_rows(&parser, 80)
    );
}

/// Calling redraw_sync immediately after creating a virtual
/// terminal must not deadlock.  Before the fix, if the redraw
/// thread hadn't consumed the initial notification yet, the
/// sync check saw `sync_completed < sync_requested` and did
/// `continue`, looping forever without rendering.
#[test]
fn redraw_sync_does_not_deadlock_on_fresh_term() {
    for _ in 0..50 {
        let buf = SharedBuffer::new();
        let mut parser = vt100::Parser::new(10, 40, 0);
        let (term, handle, _input_tx) =
            Term::new_virtual(40, 10, "> ", Box::new(buf.clone()), CursorShape::Bar);

        // This would hang before the fix.
        handle.redraw_sync();
        buf.drain_into(&mut parser);
        assert!(screen_contains(&parser, 40, "> "));

        drop(term);
    }
}

/// Multiple concurrent redraw_sync calls must all complete.
#[test]
fn concurrent_redraw_syncs_all_complete() {
    let buf = SharedBuffer::new();
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 10, "> ", Box::new(buf.clone()), CursorShape::Bar);

    // Warm up — make sure redraw thread has done its first cycle.
    handle.redraw_sync();

    let barrier = Arc::new(std::sync::Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let h = handle.clone();
            let b = barrier.clone();
            thread::spawn(move || {
                b.wait();
                h.redraw_sync();
            })
        })
        .collect();

    for t in threads {
        t.join().expect("redraw_sync thread panicked");
    }
}

/// A writer that can block on flush() and counts completed
/// flushes. Each flush corresponds to one render cycle.
#[derive(Clone)]
struct GatedWriter {
    inner: Arc<Mutex<GatedWriterInner>>,
    condvar: Arc<std::sync::Condvar>,
}

struct GatedWriterInner {
    /// When true, flush() blocks until gate is opened.
    gate_closed: bool,
    /// The writer is currently blocked inside flush().
    blocked: bool,
    /// Total number of write() calls that have reached the inner writer.
    write_count: u64,
    /// Total number of flush() calls that have completed.
    flush_count: u64,
}

impl GatedWriter {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(GatedWriterInner {
                gate_closed: false,
                blocked: false,
                write_count: 0,
                flush_count: 0,
            })),
            condvar: Arc::new(std::sync::Condvar::new()),
        }
    }

    /// Close the gate — the next flush() will block.
    fn close_gate(&self) {
        self.inner
            .lock()
            .expect("gated writer poisoned")
            .gate_closed = true;
    }

    /// Block until the writer is actually stuck inside flush().
    fn wait_until_blocked(&self) {
        let guard = self.inner.lock().expect("gated writer poisoned");
        let _g = self
            .condvar
            .wait_while(guard, |s| !s.blocked)
            .expect("gated writer poisoned");
    }

    /// Open the gate — unblocks a stuck flush() and keeps it open.
    fn open_gate(&self) {
        let mut s = self.inner.lock().expect("gated writer poisoned");
        s.gate_closed = false;
        self.condvar.notify_all();
    }

    /// How many write() calls reached the inner writer so far.
    fn write_count(&self) -> u64 {
        self.inner
            .lock()
            .expect("gated writer poisoned")
            .write_count
    }

    /// How many flush() calls have completed so far.
    fn flush_count(&self) -> u64 {
        self.inner
            .lock()
            .expect("gated writer poisoned")
            .flush_count
    }
}

impl io::Write for GatedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner
            .lock()
            .expect("gated writer poisoned")
            .write_count += 1;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut s = self.inner.lock().expect("gated writer poisoned");
        if s.gate_closed {
            s.blocked = true;
            self.condvar.notify_all();
            s = self
                .condvar
                .wait_while(s, |s| s.gate_closed)
                .expect("gated writer poisoned");
            s.blocked = false;
        }
        s.flush_count += 1;
        self.condvar.notify_all();
        Ok(())
    }
}

/// Full redraw should only queue terminal output. The redraw loop owns
/// the single frame-ending flush so cursor placement is batched with
/// the rest of the repaint.
#[test]
fn full_redraw_queues_without_flushing_mid_frame() {
    let writer = GatedWriter::new();
    let mut screen = Screen::new(40);
    let all_lines = plain_lines(&["line 0", "line 1", "> prompt"]);
    let line_sources = (0..all_lines.len())
        .map(|wrapped_row| LineSource::Input { wrapped_row })
        .collect();
    let layout = LayoutAll {
        all_lines,
        line_sources,
        log_end: 2,
        history_generation: 0,
        history_width: 40,
        cursor_row: 2,
        cursor_col: 8,
    };
    let plan = TerminalModel::full_redraw_plan(&layout, 10);

    let mut buffered = std::io::BufWriter::new(writer.clone());
    full_render(
        &mut buffered,
        &mut screen,
        &layout,
        &plan,
        40,
        10,
        usize::MAX,
    )
    .expect("full render");
    assert_eq!(
        writer.write_count(),
        0,
        "buffered full render should not write through"
    );
    assert_eq!(writer.flush_count(), 0, "full render should not flush");

    std::io::Write::flush(&mut buffered).expect("flush frame");
    assert_eq!(
        writer.write_count(),
        1,
        "small frame should write through once"
    );
    assert_eq!(writer.flush_count(), 1, "caller should flush once");
}

/// Verify that notifications coalesce: while the redraw thread
/// is blocked mid-render, many notifications pile up and produce
/// at most one additional render after unblocking.
///
/// Uses the gated writer to create deterministic windows where
/// notifications must accumulate:
///
/// 1. Close gate → trigger render → redraw thread blocks in flush
/// 2. Fire N notifications (all coalesce into one pending flag)
/// 3. Open gate → blocked render completes → one coalesced render
/// 4. redraw_sync settles any remaining async renders
///
/// Per round we expect at most 3 flushes (blocked + coalesced +
/// sync). Without coalescing we'd see N+2 per round.
#[test]
fn notifications_coalesce_while_rendering() {
    let writer = GatedWriter::new();
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 10, "> ", Box::new(writer.clone()), CursorShape::Bar);

    // Let the initial render finish so the redraw thread is idle
    // at recv(). The gate is open, so the render completes.
    handle.redraw_sync();

    const ROUNDS: usize = 5;
    const NOTIFICATIONS_PER_ROUND: usize = 10;

    for round in 0..ROUNDS {
        let before = writer.flush_count();

        // Close the gate so the next render blocks in flush().
        writer.close_gate();

        // Trigger a render — the redraw thread wakes from recv(),
        // renders, enters flush(), and blocks.
        handle.set_buffer(format!("r{round}"), 0);
        handle.redraw();
        writer.wait_until_blocked();

        // Redraw thread is stuck in flush. Fire many notifications.
        // They all coalesce into a single pending flag in the
        // notify channel.
        for j in 0..NOTIFICATIONS_PER_ROUND {
            handle.set_buffer(format!("r{round}-{j}"), 0);
            handle.redraw();
        }

        // Open the gate. The blocked flush completes, the loop
        // picks up the coalesced notification and renders once
        // more.
        writer.open_gate();

        // Settle: redraw_sync guarantees at least one render
        // after this point completes, draining any stragglers.
        handle.redraw_sync();

        let after = writer.flush_count();
        let renders = after - before;

        // Without coalescing we'd see NOTIFICATIONS_PER_ROUND + 2
        // (= 12) renders. With coalescing: the blocked render (1)
        // + the coalesced render (1) + possibly the sync render
        // (1). Under coverage instrumentation, the redraw thread may
        // also observe one notification just before the burst fully
        // coalesces, so allow one extra render while still proving the
        // burst did not render once per notification.
        assert!(
            renders <= 4,
            "round {round}: expected ≤4 renders, got {renders}. \
                 Without coalescing this would be {}.",
            NOTIFICATIONS_PER_ROUND + 2,
        );
    }
}

/// Coalescing still works after sync: many async redraws followed
/// by a sync should reflect the final state, not spin.
#[test]
fn coalescing_preserved_after_sync() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(10, 40, 0);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 10, "> ", Box::new(buf.clone()), CursorShape::Bar);

    // Fire a bunch of async redraws, then one sync.
    for i in 0..20 {
        handle.set_buffer(format!("v{i}"), 2);
        handle.redraw();
    }
    handle.set_buffer("final".into(), 5);
    flush_redraws(&handle, &buf, &mut parser);
    assert!(
        screen_contains(&parser, 40, "> final"),
        "expected '> final', got: {:?}",
        vt100_rows(&parser, 40)
    );
}

/// full_render pushes overflow lines into terminal scrollback.
#[test]
fn full_render_populates_scrollback() {
    // Exact same params as the passing overflow test — only
    // line contents differ.
    let lines = plain_lines(&[
        "line 0", "line 1", "line 2", "line 3", "line 4", "line 5", "> prompt",
    ]);
    let (mut term, _screen) = run_full_render(5, 30, lines, 3, 5, 7);

    // Scroll back 2 lines (the overflow amount).
    term.screen_mut().set_scrollback(2);
    let sb = visible_rows(&term);
    assert_eq!(
        sb[0], "line 0",
        "line 0 should be in scrollback, got: {sb:?}"
    );
    assert_eq!(
        sb[1], "line 1",
        "line 1 should be in scrollback, got: {sb:?}"
    );
}

/// Diff-path scrolling: history that overflows the viewport
/// during normal operation enters the terminal scrollback.
#[test]
fn diff_update_scrolls_overflow_into_scrollback() {
    let buf = SharedBuffer::new();
    // 5-row terminal with scrollback capacity.
    let mut parser = vt100::Parser::new(5, 40, 50);

    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    // Add 6 history lines — total is 7 (6 + prompt), viewport
    // is 5, so 2 lines overflow.
    for i in 0..6 {
        handle.print_output(
            "test",
            StyledBlock::new(StyledText::from(Span::plain(format!("line {i}")))),
        );
    }
    flush_redraws(&handle, &buf, &mut parser);

    // The prompt + last few history lines are visible.
    assert!(
        screen_contains(&parser, 40, "> "),
        "prompt should be visible, got: {:?}",
        vt100_rows(&parser, 40)
    );

    // The earliest lines should be in terminal scrollback.
    parser.screen_mut().set_scrollback(2);
    let sb_rows = vt100_rows(&parser, 40);
    assert!(
        sb_rows[0].contains("line 0"),
        "line 0 should be in scrollback, got: {sb_rows:?}"
    );
    assert!(
        sb_rows[1].contains("line 1"),
        "line 1 should be in scrollback, got: {sb_rows:?}"
    );
}

/// Pi-style overflow must also work when the content growth comes
/// from updating an existing live block in place, not only from
/// appending new history entries.
#[test]
fn live_block_growth_scrolls_updated_lines_into_scrollback() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);

    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    let block_id = handle.new_block(
        "test",
        StyledBlock::new(StyledText::from(Span::plain("starting"))),
    );
    handle.push_above_active(block_id);
    flush_redraws(&handle, &buf, &mut parser);

    let full_render_count = handle.full_render_count();
    handle.set_block(
        block_id,
        StyledBlock::new(StyledText::from(Span::plain(
            "stream 0\nstream 1\nstream 2\nstream 3\nstream 4\nstream 5",
        ))),
    );
    flush_redraws(&handle, &buf, &mut parser);
    assert_eq!(
        handle.full_render_count(),
        full_render_count,
        "visible lines that scroll during the same render should not force a full redraw"
    );

    assert!(
        screen_contains(&parser, 40, "stream 5"),
        "latest line should remain visible, got: {:?}",
        vt100_rows(&parser, 40)
    );
    assert!(
        screen_contains(&parser, 40, "> "),
        "prompt should remain visible, got: {:?}",
        vt100_rows(&parser, 40)
    );

    parser.screen_mut().set_scrollback(2);
    let sb_rows = vt100_rows(&parser, 40);
    assert!(
        sb_rows[0].contains("stream 0"),
        "updated line 0 should be in scrollback, got: {sb_rows:?}"
    );
    assert!(
        sb_rows[1].contains("stream 1"),
        "updated line 1 should be in scrollback, got: {sb_rows:?}"
    );
}

/// Visible history updates can be patched incrementally; this protects the
/// no-full-redraw fast path.
#[test]
fn visible_history_block_update_does_not_full_redraw() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    let mut ids = Vec::new();
    for i in 0..3 {
        ids.push(handle.print_output("test", plain_block(format!("line {i}"))));
    }
    flush_redraws(&handle, &buf, &mut parser);

    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
        handle.set_block(ids[2], plain_block("line 2 updated"));
    });
}

/// Hidden scrollback changes require a full redraw because the terminal
/// scrollback cannot be patched in place.
#[test]
fn hidden_history_block_update_full_redraws() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    let mut ids = Vec::new();
    for i in 0..8 {
        ids.push(handle.print_output("test", plain_block(format!("line {i}"))));
    }
    flush_redraws(&handle, &buf, &mut parser);

    assert_full_redraw_after(&handle, &buf, &mut parser, || {
        handle.set_block(ids[0], plain_block("line 0 updated while hidden"));
    });
}

/// Visible active tool/status updates should stay on the incremental path for
/// smooth streaming output.
#[test]
fn visible_active_block_update_does_not_full_redraw() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    for i in 0..3 {
        handle.print_output("test", plain_block(format!("line {i}")));
    }
    let active = handle.new_block("test", plain_block("active"));
    handle.push_above_active(active);
    flush_redraws(&handle, &buf, &mut parser);

    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
        handle.set_block(active, plain_block("active updated"));
    });
}

/// Finalizing a visible active block into history should preserve the viewport
/// and avoid an unnecessary full redraw.
#[test]
fn active_block_finalized_to_history_does_not_full_redraw_when_visible() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    for i in 0..3 {
        handle.print_output("test", plain_block(format!("line {i}")));
    }
    let active = handle.new_block("test", plain_block("tool done"));
    handle.push_above_active(active);
    flush_redraws(&handle, &buf, &mut parser);

    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
        handle.remove_block(active);
        handle.print_output("test", plain_block("tool done"));
    });
}

/// Removing a visible active block can remain incremental when new tail content
/// keeps the viewport moving downward.
#[test]
fn visible_active_block_removal_does_not_full_redraw_when_viewport_still_moves_down() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    for i in 0..6 {
        handle.print_output("test", plain_block(format!("line {i}")));
    }
    let active = handle.new_block("test", plain_block("temporary active"));
    handle.push_above_active(active);
    flush_redraws(&handle, &buf, &mut parser);

    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
        handle.remove_block(active);
        handle.print_output("test", plain_block("new output keeps viewport moving"));
    });
}

/// Regression guard from `fix(term): absorb visible shrinkage with viewport
/// rubber`: shrinkage is absorbed with a blank rubber row instead of full
/// redraw.
#[test]
fn removing_visible_block_that_moves_viewport_up_uses_rubber_without_full_redraw() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    for i in 0..6 {
        handle.print_output("test", plain_block(format!("line {i}")));
    }
    let active = handle.new_block("test", plain_block("temporary active"));
    handle.push_above_active(active);
    flush_redraws(&handle, &buf, &mut parser);

    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
        handle.remove_block(active);
    });
}

/// After rubber has kept an incremental frame stable, a later full redraw must
/// discard the rubber and repaint the true viewport.
#[test]
fn full_redraw_after_rubber_discards_rubber_and_repaints_viewport() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    for i in 0..6 {
        handle.print_output("test", plain_block(format!("line {i}")));
    }
    let active = handle.new_block("test", plain_block("temporary active"));
    handle.push_above_active(active);
    flush_redraws(&handle, &buf, &mut parser);

    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
        handle.remove_block(active);
    });
    assert_terminal_rows_match(
        &mut parser,
        40,
        5,
        &["line 3", "line 4", "line 5", "", "> "],
    );

    assert_full_redraw_after(&handle, &buf, &mut parser, || {
        handle.invalidate_screen();
    });
    assert_terminal_rows_match(
        &mut parser,
        40,
        5,
        &["line 2", "line 3", "line 4", "line 5", "> "],
    );
}

/// Regression guard from `fix(term): redraw resize scrollback without rubber
/// gaps`: resizing after rubber must rebuild without leaving the synthetic gap.
#[test]
fn resize_full_redraw_discards_rubber_gap() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);
    let (term, handle, input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    for i in 0..6 {
        handle.print_output("test", plain_block(format!("line {i}")));
    }
    let active = handle.new_block("test", plain_block("temporary active"));
    handle.push_above_active(active);
    flush_redraws(&handle, &buf, &mut parser);

    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
        handle.remove_block(active);
    });
    assert_terminal_rows_match(
        &mut parser,
        40,
        5,
        &["line 3", "line 4", "line 5", "", "> "],
    );

    parser.screen_mut().set_size(8, 40);
    input_tx
        .send(RawEvent::Resize(40, 8))
        .expect("send resize event");
    assert!(matches!(
        term.get_next_event().expect("resize event"),
        Event::Resize {
            width: 40,
            height: 8
        }
    ));
    flush_redraws(&handle, &buf, &mut parser);

    assert_terminal_rows_match(
        &mut parser,
        40,
        8,
        &[
            "line 0", "line 1", "line 2", "line 3", "line 4", "line 5", "> ",
        ],
    );
}

#[test]
fn resize_resampling_uses_actual_size_without_hiding_zero() {
    assert_eq!(resample_resize_dimension(0, 80), 80);
    assert_eq!(resample_resize_dimension(120, 80), 120);
    assert_eq!(resample_resize_dimension(0, 0), 0);
}

/// Transient zero-sized resize reports from terminals/tmux are ignored so the
/// prompt does not get stuck wrapping every character as its own row.
#[test]
fn zero_resize_keeps_previous_tracked_size() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);
    let (term, handle, input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    input_tx
        .send(RawEvent::Resize(0, 0))
        .expect("send zero resize event");
    assert!(matches!(
        term.get_next_event().expect("resize event"),
        Event::Resize {
            width: 40,
            height: 5
        }
    ));
    assert_eq!(handle.size(), (40, 5));

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        )))
        .expect("send first key");
    assert!(matches!(
        term.get_next_event().expect("first key event"),
        Event::BufferChanged
    ));
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::NONE,
        )))
        .expect("send second key");
    assert!(matches!(
        term.get_next_event().expect("second key event"),
        Event::BufferChanged
    ));
    flush_redraws(&handle, &buf, &mut parser);

    assert_terminal_rows_match(&mut parser, 40, 5, &["> ab"]);
}

/// Resize full redraw must rebuild terminal scrollback correctly even when rows
/// exactly fill the old or new width.
#[test]
fn resize_full_redraw_rebuilds_scrollback_for_exact_width_lines() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(10, 12, 50);
    let (term, handle, input_tx) =
        Term::new_virtual(12, 10, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    for i in 0..7 {
        handle.print_output(
            "test",
            plain_block(format!("{i}{i}{i}{i}{i}{i}{i}{i}{i}{i}")),
        );
    }
    flush_redraws(&handle, &buf, &mut parser);

    parser.screen_mut().set_size(6, 10);
    input_tx
        .send(RawEvent::Resize(10, 6))
        .expect("send resize event");
    assert!(matches!(
        term.get_next_event().expect("resize event"),
        Event::Resize {
            width: 10,
            height: 6
        }
    ));
    flush_redraws(&handle, &buf, &mut parser);

    assert_terminal_rows_match(
        &mut parser,
        10,
        6,
        &[
            "0000000000",
            "1111111111",
            "2222222222",
            "3333333333",
            "4444444444",
            "5555555555",
            "6666666666",
            "> ",
        ],
    );
}

/// Shrinking the terminal should rebuild the scrollback model without blank
/// gaps between history and prompt.
#[test]
fn resize_full_redraw_rebuilds_scrollback_without_gap() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(10, 40, 50);
    let (term, handle, input_tx) =
        Term::new_virtual(40, 10, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    for i in 0..6 {
        handle.print_output("test", plain_block(format!("line {i}")));
    }
    flush_redraws(&handle, &buf, &mut parser);
    assert_terminal_rows_match(
        &mut parser,
        40,
        10,
        &[
            "line 0", "line 1", "line 2", "line 3", "line 4", "line 5", "> ",
        ],
    );

    parser.screen_mut().set_size(6, 40);
    input_tx
        .send(RawEvent::Resize(40, 6))
        .expect("send resize event");
    assert!(matches!(
        term.get_next_event().expect("resize event"),
        Event::Resize {
            width: 40,
            height: 6
        }
    ));
    flush_redraws(&handle, &buf, &mut parser);

    assert_terminal_rows_match(
        &mut parser,
        40,
        6,
        &[
            "line 0", "line 1", "line 2", "line 3", "line 4", "line 5", "> ",
        ],
    );
}

/// Below-prompt status changes are visible fixed-area updates and should not
/// force a scrollback-resetting full redraw.
#[test]
fn below_status_update_with_scrollback_does_not_full_redraw() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    for i in 0..8 {
        handle.print_output("test", plain_block(format!("line {i}")));
    }
    let status = handle.new_block("test", plain_block("status 0"));
    handle.push_below(status);
    flush_redraws(&handle, &buf, &mut parser);

    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
        handle.set_block(status, plain_block("status 1"));
    });
}

/// Tool-summary churn reorders visible blocks during normal operation; this
/// protects the incremental path for that UI pattern.
#[test]
fn tool_summary_like_reorder_in_visible_area_does_not_full_redraw() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    for i in 0..4 {
        handle.print_output("test", plain_block(format!("line {i}")));
    }
    let summary = handle.new_block("test", plain_block("tools 0/2"));
    let tool1 = handle.new_block("test", plain_block("tool one running"));
    let tool2 = handle.new_block("test", plain_block("tool two running"));
    handle.push_above_active(summary);
    handle.push_above_active(tool1);
    handle.push_above_active(tool2);
    flush_redraws(&handle, &buf, &mut parser);

    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
        handle.remove_block(tool1);
        handle.set_block(summary, plain_block("tools 1/2"));
        handle.print_output("test", plain_block("tool one ok"));
    });

    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
        handle.remove_block(tool2);
        handle.remove_block(summary);
        handle.print_output("test", plain_block("tools 2/2"));
        handle.print_output("test", plain_block("tool two ok"));
    });
}

/// Pseudo-random visible block churn stress-tests that visible-only mutations
/// keep using incremental rendering.
#[test]
fn randomized_visible_block_churn_does_not_full_redraw() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(6, 40, 100);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 6, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    let mut history_ids = Vec::new();
    for i in 0..5 {
        history_ids.push(handle.print_output("test", plain_block(format!("seed {i}"))));
    }
    let mut active_ids = Vec::new();
    flush_redraws(&handle, &buf, &mut parser);

    let mut rng = 0x1234_5678_u64;
    for step in 0..120 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        match rng % 5 {
            0 => {
                assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
                    history_ids
                        .push(handle.print_output("test", plain_block(format!("append {step}"))));
                });
            }
            1 => {
                let id = active_ids
                    .last()
                    .copied()
                    .or_else(|| history_ids.last().copied())
                    .expect("visible id");
                assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
                    handle.set_block(id, plain_block(format!("visible update {step}")));
                });
            }
            2 => {
                if active_ids.len() < 2 {
                    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
                        let id = handle.new_block("test", plain_block(format!("active {step}")));
                        handle.push_above_active(id);
                        active_ids.push(id);
                    });
                }
            }
            3 => {
                if let Some(&id) = active_ids.last() {
                    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
                        handle.set_block(id, plain_block(format!("active update {step}")));
                    });
                }
            }
            _ => {
                if let Some(id) = active_ids.pop() {
                    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
                        handle.remove_block(id);
                        history_ids.push(
                            handle.print_output(
                                "test",
                                plain_block(format!("active finalized {step}")),
                            ),
                        );
                    });
                }
            }
        }
    }
}

/// Every hidden scrollback mutation needs its own full redraw so the retained
/// model never diverges from terminal history.
#[test]
fn repeated_hidden_block_updates_each_full_redraw() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    let mut ids = Vec::new();
    for i in 0..8 {
        ids.push(handle.print_output("test", plain_block(format!("line {i}"))));
    }
    flush_redraws(&handle, &buf, &mut parser);

    for i in 0..5 {
        assert_full_redraw_after(&handle, &buf, &mut parser, || {
            handle.set_block(ids[0], plain_block(format!("hidden update {i}")));
        });
    }
}

fn assert_terminal_rows_match(
    parser: &mut vt100::Parser,
    cols: u16,
    height: usize,
    known: &[&str],
) {
    let viewport_start = known.len().saturating_sub(height);
    for scrollback in 0..=viewport_start {
        parser.screen_mut().set_scrollback(scrollback);
        let start = viewport_start - scrollback;
        let mut expected = known[start..known.len().min(start + height)]
            .iter()
            .map(|line| line.trim_end().to_owned())
            .collect::<Vec<_>>();
        expected.resize(height, String::new());
        let actual = vt100_rows(parser, cols)
            .into_iter()
            .map(|line| line.trim_end().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            actual, expected,
            "terminal rows should match retained model at scrollback offset {scrollback}"
        );
    }
    parser.screen_mut().set_scrollback(0);
}

fn assert_no_full_redraw_and_rows(
    handle: &TermHandle,
    buf: &SharedBuffer,
    parser: &mut vt100::Parser,
    cols: u16,
    height: usize,
    expected: &[&str],
    action: impl FnOnce(),
) {
    assert_no_full_redraw_after(handle, buf, parser, action);
    assert_terminal_rows_match(parser, cols, height, expected);
}

/// Basic append operations should keep Tau's retained model and vt100's
/// scrollback in lockstep without full redraws.
#[test]
fn terminal_scrollback_model_matches_vt100_for_basic_append_paths() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(4, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 4, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);
    assert_terminal_rows_match(&mut parser, 40, 4, &["> "]);

    assert_no_full_redraw_and_rows(&handle, &buf, &mut parser, 40, 4, &["one", "> "], || {
        handle.print_output("test", plain_block("one"));
    });
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        4,
        &["one", "two", "> "],
        || {
            handle.print_output("test", plain_block("two"));
        },
    );
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        4,
        &["one", "two", "three", "> "],
        || {
            handle.print_output("test", plain_block("three"));
        },
    );
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        4,
        &["one", "two", "three", "four", "> "],
        || {
            handle.print_output("test", plain_block("four"));
        },
    );
}

/// Empty visible blocks are layout no-ops and must not perturb the retained
/// terminal model.
#[test]
fn empty_blocks_in_visible_zones_do_not_change_model_or_full_redraw() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    handle.print_output("test", plain_block("history"));
    flush_redraws(&handle, &buf, &mut parser);
    assert_terminal_rows_match(&mut parser, 40, 5, &["history", "> "]);

    let active = handle.new_block("test", plain_block(""));
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        5,
        &["history", "> "],
        || {
            handle.push_above_active(active);
        },
    );
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        5,
        &["history", "> "],
        || {
            handle.set_block(active, plain_block(""));
        },
    );
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        5,
        &["history", "> "],
        || {
            handle.remove_block(active);
        },
    );

    let sticky = handle.new_block("test", plain_block(""));
    let suggestions = handle.new_block("test", plain_block(""));
    let below = handle.new_block("test", plain_block(""));
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        5,
        &["history", "> "],
        || {
            handle.push_above_sticky(sticky);
            handle.push_suggestions(suggestions);
            handle.push_below(below);
        },
    );
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        5,
        &["history", "> "],
        || {
            handle.set_block(sticky, plain_block(""));
            handle.set_block(suggestions, plain_block(""));
            handle.set_block(below, plain_block(""));
        },
    );
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        5,
        &["history", "> "],
        || {
            handle.remove_block(sticky);
            handle.remove_block(suggestions);
            handle.remove_block(below);
        },
    );
}

/// Empty history blocks hidden in scrollback should also be no-ops, avoiding
/// expensive redraws for zero-height content.
#[test]
fn empty_history_blocks_in_scrollback_do_not_change_model_or_full_redraw() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(4, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 4, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    let mut expected = Vec::new();
    for i in 0..3 {
        let line = format!("before {i}");
        expected.push(line.clone());
        handle.print_output("test", plain_block(line));
    }
    let empty = handle.print_output("test", plain_block(""));
    for i in 0..6 {
        let line = format!("after {i}");
        expected.push(line.clone());
        handle.print_output("test", plain_block(line));
    }
    expected.push("> ".to_owned());
    flush_redraws(&handle, &buf, &mut parser);
    let expected_refs = expected.iter().map(String::as_str).collect::<Vec<_>>();
    assert_terminal_rows_match(&mut parser, 40, 4, &expected_refs);

    assert_no_full_redraw_and_rows(&handle, &buf, &mut parser, 40, 4, &expected_refs, || {
        handle.set_block(empty, plain_block(""));
    });
    assert_no_full_redraw_and_rows(&handle, &buf, &mut parser, 40, 4, &expected_refs, || {
        handle.remove_block(empty);
    });
    assert_no_full_redraw_and_rows(&handle, &buf, &mut parser, 40, 4, &expected_refs, || {
        handle.print_output("test", plain_block(""));
    });
}

/// Repeated history appends should naturally spill rows into terminal
/// scrollback while staying incremental.
#[test]
fn repeated_tail_appends_spill_viewport_to_scrollback_without_full_redraw() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(4, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 4, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    let mut expected = vec!["> ".to_owned()];
    for i in 0..12 {
        let line = format!("line {i}");
        expected.insert(expected.len() - 1, line);
        let expected_refs = expected.iter().map(String::as_str).collect::<Vec<_>>();
        assert_no_full_redraw_and_rows(&handle, &buf, &mut parser, 40, 4, &expected_refs, || {
            handle.print_output("test", plain_block(format!("line {i}")));
        });
    }
}

/// Growing live output in place should scroll overflow rows into terminal
/// scrollback without resorting to full redraw.
#[test]
fn repeated_live_growth_spills_viewport_to_scrollback_without_full_redraw() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(4, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 4, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    let active = handle.new_block("test", plain_block("live 0"));
    handle.push_above_active(active);
    flush_redraws(&handle, &buf, &mut parser);
    assert_terminal_rows_match(&mut parser, 40, 4, &["live 0", "> "]);

    let mut expected_lines = vec!["live 0".to_owned()];
    for i in 1..10 {
        expected_lines.push(format!("live {i}"));
        let mut expected = expected_lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        expected.push("> ");
        let content = expected_lines.join("\n");
        assert_no_full_redraw_and_rows(&handle, &buf, &mut parser, 40, 4, &expected, || {
            handle.set_block(active, plain_block(content));
        });
    }
}

/// Middle active-block growth, shrinkage, and removal are visible-only edits
/// and should keep the model synchronized incrementally.
#[test]
fn middle_visible_active_block_lifecycle_without_scrollback_does_not_full_redraw() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(8, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 8, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    handle.print_output("test", plain_block("history"));
    let top = handle.new_block("test", plain_block("top live"));
    let middle = handle.new_block("test", plain_block("middle live"));
    let bottom = handle.new_block("test", plain_block("bottom live"));
    handle.push_above_active(top);
    handle.push_above_active(middle);
    handle.push_above_active(bottom);
    flush_redraws(&handle, &buf, &mut parser);
    assert_terminal_rows_match(
        &mut parser,
        40,
        8,
        &["history", "top live", "middle live", "bottom live", "> "],
    );

    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        8,
        &[
            "history",
            "top live",
            "middle a",
            "middle b",
            "bottom live",
            "> ",
        ],
        || {
            handle.set_block(middle, plain_block("middle a\nmiddle b"));
        },
    );
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        8,
        &[
            "history",
            "top live",
            "middle a",
            "middle b",
            "middle c",
            "bottom live",
            "> ",
        ],
        || {
            handle.set_block(middle, plain_block("middle a\nmiddle b\nmiddle c"));
        },
    );
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        8,
        &["history", "top live", "middle small", "bottom live", "> "],
        || {
            handle.set_block(middle, plain_block("middle small"));
        },
    );
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        8,
        &["history", "top live", "bottom live", "> "],
        || {
            handle.remove_block(middle);
        },
    );
}

/// Below-zone middle block edits exercise the same incremental splice logic
/// below the prompt.
#[test]
fn middle_visible_below_block_lifecycle_without_scrollback_does_not_full_redraw() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(8, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 8, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    let first = handle.new_block("test", plain_block("below one"));
    let middle = handle.new_block("test", plain_block("below middle"));
    let last = handle.new_block("test", plain_block("below last"));
    handle.push_below(first);
    handle.push_below(middle);
    handle.push_below(last);
    flush_redraws(&handle, &buf, &mut parser);
    assert_terminal_rows_match(
        &mut parser,
        40,
        8,
        &["> ", "below one", "below middle", "below last"],
    );

    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        8,
        &["> ", "below one", "below a", "below b", "below last"],
        || {
            handle.set_block(middle, plain_block("below a\nbelow b"));
        },
    );
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        8,
        &["> ", "below one", "below middle", "below last"],
        || {
            handle.set_block(middle, plain_block("below middle"));
        },
    );
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        8,
        &["> ", "below one", "below last"],
        || {
            handle.remove_block(middle);
        },
    );
}

/// Sticky above-prompt blocks should support visible middle edits without
/// invalidating the full screen.
#[test]
fn middle_visible_sticky_block_lifecycle_without_scrollback_does_not_full_redraw() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(8, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 8, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    let first = handle.new_block("test", plain_block("sticky one"));
    let middle = handle.new_block("test", plain_block("sticky middle"));
    let last = handle.new_block("test", plain_block("sticky last"));
    handle.push_above_sticky(first);
    handle.push_above_sticky(middle);
    handle.push_above_sticky(last);
    flush_redraws(&handle, &buf, &mut parser);
    assert_terminal_rows_match(
        &mut parser,
        40,
        8,
        &["sticky one", "sticky middle", "sticky last", "> "],
    );

    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        8,
        &["sticky one", "sticky a", "sticky b", "sticky last", "> "],
        || {
            handle.set_block(middle, plain_block("sticky a\nsticky b"));
        },
    );
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        8,
        &["sticky one", "sticky small", "sticky last", "> "],
        || {
            handle.set_block(middle, plain_block("sticky small"));
        },
    );
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        8,
        &["sticky one", "sticky last", "> "],
        || {
            handle.remove_block(middle);
        },
    );
}

/// Suggestion block churn should be patched in place so completions can update
/// without full redraw flicker.
#[test]
fn middle_visible_suggestions_block_lifecycle_without_scrollback_does_not_full_redraw() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(8, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 8, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    let first = handle.new_block("test", plain_block("suggest one"));
    let middle = handle.new_block("test", plain_block("suggest middle"));
    let last = handle.new_block("test", plain_block("suggest last"));
    handle.push_suggestions(first);
    handle.push_suggestions(middle);
    handle.push_suggestions(last);
    flush_redraws(&handle, &buf, &mut parser);
    assert_terminal_rows_match(
        &mut parser,
        40,
        8,
        &["> ", "suggest one", "suggest middle", "suggest last"],
    );

    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        8,
        &[
            "> ",
            "suggest one",
            "suggest a",
            "suggest b",
            "suggest last",
        ],
        || {
            handle.set_block(middle, plain_block("suggest a\nsuggest b"));
        },
    );
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        8,
        &["> ", "suggest one", "suggest small", "suggest last"],
        || {
            handle.set_block(middle, plain_block("suggest small"));
        },
    );
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        8,
        &["> ", "suggest one", "suggest last"],
        || {
            handle.remove_block(middle);
        },
    );
}

/// Changing prompt height shifts below blocks; this protects incremental
/// movement and retained-model consistency.
#[test]
fn prompt_height_changes_shift_below_blocks_without_full_redraw_or_model_drift() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(8, 10, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(10, 8, "> ", Box::new(buf.clone()), CursorShape::Bar);
    let below = handle.new_block("test", plain_block("below"));
    handle.push_below(below);
    flush_redraws(&handle, &buf, &mut parser);
    assert_terminal_rows_match(&mut parser, 10, 8, &["> ", "below"]);

    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        10,
        8,
        &["> abc", "def", "below"],
        || {
            handle.set_buffer("abc\ndef".to_owned(), "abc\ndef".len());
        },
    );
    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        10,
        8,
        &["> short", "below"],
        || {
            handle.set_buffer("short".to_owned(), "short".len());
        },
    );
}

/// A visible middle block can grow enough to push rows into scrollback; the
/// retained model must still match vt100 without full redraw.
#[test]
fn visible_middle_block_growth_into_scrollback_keeps_model_in_sync_without_full_redraw() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    for i in 0..3 {
        handle.print_output("test", plain_block(format!("history {i}")));
    }
    let top = handle.new_block("test", plain_block("top live"));
    let middle = handle.new_block("test", plain_block("middle live"));
    let bottom = handle.new_block("test", plain_block("bottom live"));
    handle.push_above_active(top);
    handle.push_above_active(middle);
    handle.push_above_active(bottom);
    flush_redraws(&handle, &buf, &mut parser);
    assert_terminal_rows_match(
        &mut parser,
        40,
        5,
        &[
            "history 0",
            "history 1",
            "history 2",
            "top live",
            "middle live",
            "bottom live",
            "> ",
        ],
    );

    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        5,
        &[
            "history 0",
            "history 1",
            "history 2",
            "top live",
            "middle a",
            "middle b",
            "middle c",
            "bottom live",
            "> ",
        ],
        || {
            handle.set_block(middle, plain_block("middle a\nmiddle b\nmiddle c"));
        },
    );
}

/// Compensating shrink/growth across zones should preserve row order and model
/// sync while staying incremental.
#[test]
fn visible_middle_block_shrink_with_compensating_below_growth_keeps_model_in_sync_without_full_redraw()
 {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    for i in 0..3 {
        handle.print_output("test", plain_block(format!("history {i}")));
    }
    let top = handle.new_block("test", plain_block("top live"));
    let middle = handle.new_block("test", plain_block("middle a\nmiddle b\nmiddle c"));
    let bottom = handle.new_block("test", plain_block("bottom live"));
    handle.push_above_active(top);
    handle.push_above_active(middle);
    handle.push_above_active(bottom);
    flush_redraws(&handle, &buf, &mut parser);

    assert_no_full_redraw_and_rows(
        &handle,
        &buf,
        &mut parser,
        40,
        5,
        &[
            "history 0",
            "history 1",
            "history 2",
            "top live",
            "middle small",
            "bottom live",
            "> ",
            "tail a",
            "tail b",
        ],
        || {
            handle.set_block(middle, plain_block("middle small"));
            let tail_a = handle.new_block("test", plain_block("tail a"));
            let tail_b = handle.new_block("test", plain_block("tail b"));
            handle.push_below(tail_a);
            handle.push_below(tail_b);
        },
    );
}

/// Long-form integration guard: visible churn across history, active, and below
/// zones must match the known-lines model at every scrollback offset.
#[test]
fn terminal_scrollback_matches_known_lines_model_across_visible_churn() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);
    assert_terminal_rows_match(&mut parser, 40, 5, &["> "]);

    for i in 0..6 {
        handle.print_output("test", plain_block(format!("line {i}")));
    }
    flush_redraws(&handle, &buf, &mut parser);
    assert_terminal_rows_match(
        &mut parser,
        40,
        5,
        &[
            "line 0", "line 1", "line 2", "line 3", "line 4", "line 5", "> ",
        ],
    );

    let active = handle.new_block("test", plain_block("active"));
    handle.push_above_active(active);
    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {});
    assert_terminal_rows_match(
        &mut parser,
        40,
        5,
        &[
            "line 0", "line 1", "line 2", "line 3", "line 4", "line 5", "active", "> ",
        ],
    );

    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
        handle.set_block(active, plain_block("active updated"));
    });
    assert_terminal_rows_match(
        &mut parser,
        40,
        5,
        &[
            "line 0",
            "line 1",
            "line 2",
            "line 3",
            "line 4",
            "line 5",
            "active updated",
            "> ",
        ],
    );

    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
        handle.remove_block(active);
        handle.print_output("test", plain_block("active updated"));
    });
    assert_terminal_rows_match(
        &mut parser,
        40,
        5,
        &[
            "line 0",
            "line 1",
            "line 2",
            "line 3",
            "line 4",
            "line 5",
            "active updated",
            "> ",
        ],
    );

    let status = handle.new_block("test", plain_block("status 0"));
    handle.push_below(status);
    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {});
    assert_terminal_rows_match(
        &mut parser,
        40,
        5,
        &[
            "line 0",
            "line 1",
            "line 2",
            "line 3",
            "line 4",
            "line 5",
            "active updated",
            "> ",
            "status 0",
        ],
    );

    assert_no_full_redraw_after(&handle, &buf, &mut parser, || {
        handle.set_block(status, plain_block("status 1"));
    });
    assert_terminal_rows_match(
        &mut parser,
        40,
        5,
        &[
            "line 0",
            "line 1",
            "line 2",
            "line 3",
            "line 4",
            "line 5",
            "active updated",
            "> ",
            "status 1",
        ],
    );
}

/// Enter, Shift+Enter, and Alt+Enter all insert a `\n` at the cursor
/// without submitting the line. Mirrors the affordance users expect
/// from chat UIs. Shift+Enter covers terminals that speak the kitty
/// keyboard protocol; Alt+Enter (the `\e\r` byte sequence) is the
/// universal fallback for terminals that don't.
/// A buffer ending in `\n` (as produced by Enter / Shift+Enter / Alt+Enter)
/// must render with an extra blank row so the cursor visibly lands
/// on a new line — otherwise the prompt height doesn't grow until
/// the next character is typed.
#[test]
fn trailing_newline_buffer_grows_prompt_height() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(30, 10, 20);

    let (_term, handle, _input_tx) =
        Term::new_virtual(10, 30, "> ", Box::new(buf.clone()), CursorShape::Bar);

    handle.set_buffer("abc\n".to_owned(), "abc\n".len());
    flush_redraws(&handle, &buf, &mut parser);

    assert_eq!(&vt100_rows(&parser, 10)[..2], &["> abc", ""]);
    assert_eq!(parser.screen().cursor_position(), (1, 0));
}

/// Regression guard from `fix(term): wrap prompt cursor after exact-width
/// input`: an exact-width prompt end needs a cursor row before below blocks.
#[test]
fn exact_width_prompt_end_grows_prompt_height_for_cursor() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(30, 10, 20);

    let (_term, handle, _input_tx) =
        Term::new_virtual(10, 30, "> ", Box::new(buf.clone()), CursorShape::Bar);
    let below = handle.new_block("below", plain_block("below"));
    handle.push_below(below);

    handle.set_buffer("abc\nabcdefghij".to_owned(), "abc\nabcdefghij".len());
    flush_redraws(&handle, &buf, &mut parser);

    assert_eq!(
        &vt100_rows(&parser, 10)[..4],
        &["> abc", "abcdefghij", "", "below     "]
    );
    assert_eq!(parser.screen().cursor_position(), (2, 0));
}

/// Enter, Shift-Enter, and Alt-Enter should insert newlines for multiline
/// prompts, while Ctrl-Enter submits.
#[test]
fn enter_variants_insert_newline_and_ctrl_enter_submits() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("line one".to_owned(), "line one".len());

    // Plain Enter: stay on the line, surface BufferChanged.
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("send enter");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "line one\n");

    // Shift+Enter: same behavior as plain Enter.
    handle.set_buffer("line one".to_owned(), "line one".len());
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
        )))
        .expect("send shift+enter");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "line one\n");

    // Alt+Enter: same behavior as shift, exercises the universal
    // fallback path.
    handle.set_buffer("line one\n".to_owned(), "line one\n".len());
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT,
        )))
        .expect("send alt+enter");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "line one\n\n");

    // Type more, then Ctrl+Enter to submit the whole multi-line
    // buffer as one Line event.
    handle.set_buffer("line one\nline two".to_owned(), "line one\nline two".len());
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("send ctrl+enter");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::Line(line) if line == "line one\nline two"
    ));
}

/// Enter and Ctrl-Enter can be bound explicitly; those bindings take precedence
/// over the default newline and submit behavior, without stealing the explicit
/// Shift-Enter / Alt-Enter multiline affordance.
#[test]
fn enter_bindings_override_default_newline_and_submit() {
    let buf = SharedBuffer::new();
    let (mut term, handle, input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    term.set_bindings(vec![
        ("Enter".to_owned(), "plain-enter".to_owned()),
        ("C-Enter".to_owned(), "ctrl-enter".to_owned()),
        ("BackTab".to_owned(), "backtab".to_owned()),
    ]);

    handle.set_buffer("draft".to_owned(), "draft".len());
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("send enter");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::Binding(action) if action == "plain-enter"
    ));
    assert_eq!(handle.get_buffer(), "draft");

    handle.set_buffer("draft".to_owned(), "draft".len());
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
        )))
        .expect("send shift+enter");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "draft\n");

    handle.set_buffer("draft".to_owned(), "draft".len());
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT,
        )))
        .expect("send alt+enter");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "draft\n");

    handle.set_buffer("draft".to_owned(), "draft".len());
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("send ctrl+enter");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::Binding(action) if action == "ctrl-enter"
    ));
    assert_eq!(handle.get_buffer(), "draft");
    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::SHIFT,
        )))
        .expect("send backtab");
    assert!(matches!(
        term.get_next_event().expect("event"),
        Event::Binding(action) if action == "backtab"
    ));
    assert_eq!(handle.get_buffer(), "draft");
}

/// When the completion menu is open, completion navigation keys must be
/// consumed before matching configurable bindings. This keeps global bindings
/// such as Shift-Tab role cycling from stealing completion-menu navigation.
#[test]
fn completion_keys_take_precedence_over_bindings() {
    let buf = SharedBuffer::new();
    let (mut term, handle, input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    term.set_completion_source(Some(Box::new(|buffer: &str, _cursor: usize| {
        if buffer == "/" {
            vec![
                Candidate {
                    label: "/model".to_owned(),
                    description: "switch model".to_owned(),
                    replacement: "/model".to_owned(),
                },
                Candidate {
                    label: "/quit".to_owned(),
                    description: "exit".to_owned(),
                    replacement: "/quit".to_owned(),
                },
            ]
        } else {
            Vec::new()
        }
    })));
    term.set_bindings(vec![
        ("BackTab".to_owned(), "backtab".to_owned()),
        ("Enter".to_owned(), "plain-enter".to_owned()),
    ]);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        )))
        .expect("send slash");
    assert!(matches!(
        term.get_next_event().expect("open completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::SHIFT,
        )))
        .expect("send backtab");
    assert!(matches!(
        term.get_next_event().expect("cycle completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/quit");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("send enter");
    assert!(matches!(
        term.get_next_event().expect("accept completion"),
        Event::CompletionAccept
    ));
    assert_eq!(handle.get_buffer(), "/quit");
}

/// Accepting a filesystem-like directory completion must immediately refresh
/// the menu for the accepted path. This keeps drilling into `./crates/` usable
/// without requiring an extra keypress to discover entries inside it.
#[test]
fn accepting_completion_refreshes_next_candidates_immediately() {
    let buf = SharedBuffer::new();
    let (mut term, handle, input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    term.set_completion_source(Some(Box::new(
        |buffer: &str, _cursor: usize| match buffer {
            "./" => vec![Candidate {
                label: "./crates/".to_owned(),
                description: "directory".to_owned(),
                replacement: "./crates/".to_owned(),
            }],
            "./crates/" => vec![Candidate {
                label: "./crates/tau-cli-term-raw/".to_owned(),
                description: "directory".to_owned(),
                replacement: "./crates/tau-cli-term-raw/".to_owned(),
            }],
            _ => Vec::new(),
        },
    )));

    for ch in ['.', '/'] {
        input_tx
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )))
            .expect("send path char");
        assert!(matches!(
            term.get_next_event().expect("type path char"),
            Event::BufferChanged
        ));
    }

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::NONE,
        )))
        .expect("send tab");
    assert!(matches!(
        term.get_next_event().expect("select directory"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "./crates/");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("send enter");
    assert!(matches!(
        term.get_next_event().expect("accept directory"),
        Event::CompletionAccept
    ));

    let completion = handle
        .completion_state()
        .expect("accepted directory should open child completions");
    assert_eq!(completion.selected, None);
    assert_eq!(completion.candidates.len(), 1);
    assert_eq!(completion.candidates[0].label, "./crates/tau-cli-term-raw/");
}

/// If the row leaving the viewport changed, the scrolling planner should know
/// it can still render that prefix before it drops.
#[test]
fn scrolling_when_dropping_changed_top_row_can_incremental_render() {
    let prev = plain_lines(&["aaaaa", "bbbbb", "ccccc"]);
    let next = plain_lines(&["AAAAA", "bbbbb", "ccccc", "ddddd"]);

    assert_eq!(changed_line_in_range(&prev, &next, 0..1), Some(0));
}

/// If the leaving row is unchanged, prefix-change detection must not invent
/// work that would force an unnecessary redraw.
#[test]
fn scrolling_when_dropping_unchanged_top_row_has_no_prefix_change() {
    let prev = plain_lines(&["aaaaa", "bbbbb", "ccccc"]);
    let next = plain_lines(&["aaaaa", "bbbbb", "ccccc", "ddddd"]);

    assert_eq!(changed_line_in_range(&prev, &next, 0..1), None);
}

/// Hidden-line detection should ignore visible-only edits so incremental
/// repaint remains available.
#[test]
fn hidden_lines_changed_ignores_visible_changes() {
    let prev = plain_lines(&["old hidden", "visible"]);
    let next = plain_lines(&["old hidden", "VISIBLE"]);

    assert!(!hidden_lines_changed(&prev, &next, 1));
}

/// Hidden-line detection must catch changed scrollback rows because those
/// require a full redraw.
#[test]
fn hidden_lines_changed_detects_scrollback_changes() {
    let prev = plain_lines(&["old hidden", "visible"]);
    let next = plain_lines(&["new hidden", "visible"]);

    assert!(hidden_lines_changed(&prev, &next, 1));
}

/// Removing a hidden scrollback row also invalidates terminal history and must
/// force the full-redraw path.
#[test]
fn hidden_lines_changed_detects_removed_scrollback_line() {
    let prev = plain_lines(&["hidden", "visible"]);
    let next = plain_lines(&["visible"]);

    assert!(hidden_lines_changed(&prev, &next, 1));
}

/// Protects the external-program pause invariant: if releasing the terminal
/// fails after redraws are paused, production rollback via
/// `resume_after_external` must unmute redraws and invalidate the next frame.
#[test]
fn external_pause_failure_rolls_back_through_resume() {
    let (term, _handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(std::io::sink()), CursorShape::Bar);
    let _redraw_guard = RedrawSuppressionGuard::new(&term.handle);
    let release_called = std::cell::Cell::new(false);

    let error = term
        .pause_for_external_with_release(|| {
            release_called.set(true);
            Err(io::Error::other("simulated release failure"))
        })
        .expect_err("release failure should be returned");

    assert!(release_called.get());
    assert_eq!(error.to_string(), "simulated release failure");
    let st = term.handle.lock();
    assert!(!st.external_paused);
    assert!(st.invalidate_screen);
}
