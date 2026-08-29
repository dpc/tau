use std::{cell as path_std_cell, io as path_std_io, sync as path_std_sync, time as path_std_time};

use super::*;
use crate::terminal_history_generation::TerminalHistoryGeneration;

/// Builds a delivery identity for raw-terminal seam tests.
fn renderer_delivery_id(value: u64) -> RendererDeliveryId {
    RendererDeliveryId::new(value)
}

/// Builds one typed opaque fact for raw-layer correlation tests.
fn opaque_fact(label: &'static str, key: u8, invalidates: &[u8]) -> OpaquePresentationFact {
    let key =
        PresentationObservationKey::new(key).expect("test key must fit the invalidation mask");
    let mut mask = PresentationInvalidation::none();
    for invalidated in invalidates {
        mask = mask.with(
            PresentationObservationKey::new(*invalidated)
                .expect("test invalidation key must fit the mask"),
        );
    }
    OpaquePresentationFact::new(label, key, mask)
}

/// Helper: builds Cell lines from plain strings.
fn plain_lines(texts: &[&str]) -> Vec<CellRow> {
    texts
        .iter()
        .map(|s| CellRow::new(s.chars().map(Cell::plain).collect()))
        .collect()
}

fn line_text(line: &[Cell]) -> String {
    line.iter().map(|cell| cell.ch).collect()
}

fn skip_virtual_redraw_on_drop(term: &Term) {
    term.handle.lock().terminal.external_paused = true;
}

const TEST_HISTORY_MAX_BYTES: usize = 64 * 1024;

/// Helper: runs full_render into a vt100 parser and returns it.
///
/// `history_lines` is the number of lines at the top of
/// `all_lines` that belong to history (before the live area).
fn run_full_render(
    rows: u16,
    cols: u16,
    all_lines: Vec<CellRow>,
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
        history_generation: TerminalHistoryGeneration::default(),
        history_width: cols as usize,
        history_height: history_lines,
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

/// A full redraw must move only shared row pointers through layout, planning,
/// replay, terminal-model retention, and the differential screen cache.
#[test]
fn full_redraw_shares_row_buffers_without_cell_copies() {
    let mut state = SharedState::new(20, 3, "> ".into());
    append_history_for_cache_test(&mut state, "history α".to_owned());
    append_history_for_cache_test(&mut state, "emoji 👩‍💻".to_owned());
    let mut cache = HistoryLayoutCache::default();
    cache.refresh(&mut state);
    let tail = layout_tail(&state, cache.lines.len());

    CellRow::reset_metrics();
    let layout = layout_all_from_cached_history(&cache, tail);
    let plan = TerminalModel::full_redraw_plan(&layout, 3);
    assert!(layout.all_lines[0].shares_buffer_with(&cache.lines[0]));
    assert!(plan.render_lines[0].shares_buffer_with(&cache.lines[0]));

    let mut output = Vec::new();
    let mut screen = Screen::new(20);
    full_render(&mut output, &mut screen, &layout, &plan, 20, 3, usize::MAX)
        .expect("shared full redraw should render");
    let mut model = TerminalModel::default();
    model.reset_to_layout(&layout, plan.viewport_start, plan.rubber_height);

    if let Some(metrics) = CellRow::metrics() {
        assert_eq!(
            metrics.allocations, 0,
            "redraw must not allocate row buffers"
        );
        assert_eq!(metrics.cell_copies, 0, "redraw must not copy cell buffers");
        assert!(
            3 <= metrics.pointer_clones,
            "layout, plan, model, and screen should share pointers"
        );
    }
    assert!(screen.shares_row_buffer(0, &layout.all_lines[0]));
    assert!(model.known_lines[0].shares_buffer_with(&layout.all_lines[0]));
}

/// This manual benchmark reports immutable row-buffer work as transcript rows
/// and terminal columns scale through the complete cache-to-screen path; it
/// deliberately has no timing threshold.
#[test]
#[ignore = "manual full-redraw row ownership scaling benchmark"]
fn benchmark_full_redraw_row_buffer_scaling() {
    for rows in [64, 512, 4096] {
        for columns in [40, 120, 240] {
            let text = "x".repeat(columns);
            let mut state = SharedState::new(columns, 24, "> ".into());
            for _ in 0..rows {
                append_history_for_cache_test(&mut state, text.clone());
            }
            let mut cache = HistoryLayoutCache::default();
            cache.refresh(&mut state);
            let tail = layout_tail(&state, cache.lines.len());

            CellRow::reset_metrics();
            let started = path_std_time::Instant::now();
            let layout = layout_all_from_cached_history(&cache, tail);
            let plan = TerminalModel::full_redraw_plan(&layout, 24);
            let mut output = Vec::new();
            let mut screen = Screen::new(columns);
            full_render(&mut output, &mut screen, &layout, &plan, columns, 24, 200)
                .expect("scaling redraw should render");
            let mut model = TerminalModel::default();
            model.reset_to_layout(&layout, plan.viewport_start, plan.rubber_height);
            let elapsed = started.elapsed();
            let rows_per_second = rows as f64 / elapsed.as_secs_f64();
            let metrics = CellRow::metrics();
            eprintln!(
                "full redraw row-buffer benchmark: rows={rows} columns={columns} \
                 metrics={metrics:?} output_bytes={} \
                 elapsed={elapsed:?} rows_per_second={rows_per_second:.0}; \
                 no timing threshold",
                output.len(),
            );
        }
    }
}

/// Rubber rows are canonical allocation-free empty values, so viewport
/// stabilization does not allocate one cell buffer per blank row.
#[test]
fn rubber_rows_do_not_allocate_row_buffers() {
    let all_lines = plain_lines(&["history", "prompt"]);
    let layout = LayoutAll {
        line_sources: vec![
            LineSource::Input { wrapped_row: 0 },
            LineSource::Input { wrapped_row: 1 },
        ],
        all_lines,
        log_end: 1,
        history_generation: TerminalHistoryGeneration::default(),
        history_width: 20,
        history_height: 1,
        cursor_row: 1,
        cursor_col: 6,
    };

    CellRow::reset_metrics();
    let plan = TerminalModel::build_plan(&layout, 0, 10_000);
    assert_eq!(plan.render_lines.len(), 10_002);
    if let Some(metrics) = CellRow::metrics() {
        assert_eq!(metrics.allocations, 0);
    }
}

/// Helper: visible rows as trimmed strings.
fn visible_rows(term: &vt100::Parser) -> Vec<String> {
    let (_, cols) = term.screen().size();
    term.screen().rows(0, cols).collect()
}

const MOUSE_CAPTURE_ENABLE: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h";
const MOUSE_CAPTURE_DISABLE: &[u8] = b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

/// Returns the ordered exact mouse-capture command blocks emitted in `bytes`.
fn mouse_capture_transitions(bytes: &[u8]) -> Vec<&'static str> {
    let mut transitions = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset..].starts_with(MOUSE_CAPTURE_ENABLE) {
            transitions.push("enable");
            offset += MOUSE_CAPTURE_ENABLE.len();
        } else if bytes[offset..].starts_with(MOUSE_CAPTURE_DISABLE) {
            transitions.push("disable");
            offset += MOUSE_CAPTURE_DISABLE.len();
        } else {
            offset += 1;
        }
    }
    transitions
}

/// Stores terminal bytes while making the first flush fail after those bytes
/// have been accepted, simulating an uncertain terminal feature write.
struct FlushFailsOnce {
    /// Bytes accepted before or after the injected flush failure.
    bytes: Vec<u8>,
    /// Whether the next flush reports the injected I/O failure.
    fail_next_flush: bool,
}

impl Write for FlushFailsOnce {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.fail_next_flush {
            self.fail_next_flush = false;
            Err(io::Error::other("simulated flush failure"))
        } else {
            Ok(())
        }
    }
}

/// External-program pause must release terminal feature modes that Tau enabled
/// so child editors do not receive focus-reporting escape sequences.
#[test]
fn external_pause_disables_and_resume_enables_focus_reporting() {
    let mut pause = Vec::new();
    write_external_pause_features(&mut pause, TerminalOptions::default()).expect("pause features");
    let pause = String::from_utf8(pause).expect("utf8 pause escapes");
    assert!(pause.contains("\u{1b}[?1004l"), "pause: {pause:?}");

    let mut resume = Vec::new();
    write_external_resume_features(&mut resume, CursorShape::Bar, TerminalOptions::default())
        .expect("resume features");
    let resume = String::from_utf8(resume).expect("utf8 resume escapes");
    assert!(resume.contains("\u{1b}[?1004h"), "resume: {resume:?}");
}

/// A disabled mouse setting must explicitly disable terminal mouse reporting
/// during every acquisition, handoff, and normal cleanup. This keeps wheel,
/// selection, and link handling terminal-native after Tau reacquires a pane.
#[test]
fn mouse_disabled_disables_reporting_across_terminal_lifecycle() {
    let options = TerminalOptions {
        mouse: false,
        ..TerminalOptions::default()
    };
    let mut bytes = Vec::new();

    write_external_resume_features(&mut bytes, CursorShape::Bar, options)
        .expect("initial terminal acquisition");
    write_external_pause_features(&mut bytes, options).expect("external handoff");
    write_external_resume_features(&mut bytes, CursorShape::Bar, options).expect("external resume");
    write_drop_terminal_cleanup(&mut bytes, options).expect("terminal cleanup");

    assert_eq!(
        mouse_capture_transitions(&bytes),
        ["disable", "disable", "disable", "disable"]
    );
}

/// The default enabled mouse setting preserves existing terminal behavior by
/// emitting no mouse-capture control sequences in any ownership phase.
#[test]
fn mouse_enabled_emits_no_capture_sequences() {
    let options = TerminalOptions::default();
    let mut bytes = Vec::new();

    write_external_resume_features(&mut bytes, CursorShape::Bar, options)
        .expect("initial terminal acquisition");
    write_external_pause_features(&mut bytes, options).expect("external handoff");
    write_external_resume_features(&mut bytes, CursorShape::Bar, options).expect("external resume");
    write_drop_terminal_cleanup(&mut bytes, options).expect("terminal cleanup");

    assert!(
        mouse_capture_transitions(&bytes).is_empty(),
        "bytes: {bytes:?}"
    );
}

/// Initial feature setup must disable mouse reporting again after a write fails
/// because the terminal may have accepted the preceding feature sequence.
#[test]
fn mouse_disabled_setup_failure_releases_partial_capture() {
    let options = TerminalOptions {
        mouse: false,
        ..TerminalOptions::default()
    };
    let mut writer = FlushFailsOnce {
        bytes: Vec::new(),
        fail_next_flush: true,
    };

    initialize_terminal_features(&mut writer, CursorShape::Bar, options)
        .expect_err("injected initial feature flush failure");

    assert_eq!(
        mouse_capture_transitions(&writer.bytes),
        ["disable", "disable"]
    );
}

/// Dropping the prompt while an external program owns the terminal must not
/// write Tau's final shutdown frame into the editor/picker screen.
#[test]
fn shutdown_does_not_render_while_external_paused() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 40, 0);
    let (term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);

    handle.print_output("before-pause", "visible before pause");
    flush_redraws(&handle, &buf, &mut parser);
    assert!(buf.is_empty(), "test should start with drained output");

    term.pause_for_external_with_release(|| Ok(()))
        .expect("virtual pause");
    assert!(buf.is_empty(), "pause sync should not render while paused");

    drop(term);
    assert!(buf.is_empty(), "shutdown must not write while paused");
}

/// Real terminal cleanup is also skipped while paused; the pause path has
/// already disabled raw-mode terminal features before handing ownership to the
/// external program.
#[test]
fn drop_cleanup_is_skipped_while_external_paused() {
    let buf = SharedBuffer::new();
    let (mut term, _handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf), CursorShape::Bar);
    term.owns_raw_mode = true;
    term.terminal_options = TerminalOptions {
        mouse: false,
        ..TerminalOptions::default()
    };

    assert!(term.should_write_drop_terminal_cleanup());
    term.pause_for_external_with_release(|| Ok(()))
        .expect("virtual pause");
    assert!(!term.should_write_drop_terminal_cleanup());

    // Keep this virtual test from attempting real terminal cleanup on drop.
    term.owns_raw_mode = false;
}

/// Virtual input shutdown should wake a blocked `get_next_event` without
/// waiting for injected input. This prevents regressions to timeout polling for
/// test terminals and exercises the shared shutdown wake channel.
#[test]
fn virtual_input_shutdown_wakes_blocked_reader() {
    let buf = SharedBuffer::new();
    let (term, handle, _input_tx) = Term::new_virtual(40, 5, "> ", Box::new(buf), CursorShape::Bar);
    let (event_tx, event_rx) = path_std_sync::mpsc::channel();
    thread::spawn(move || {
        let _ = event_tx.send(term.get_next_event());
    });

    handle.request_input_shutdown();

    let event = event_rx
        .recv_timeout(path_std_time::Duration::from_secs(2))
        .expect("shutdown should wake the reader promptly")
        .expect("shutdown should surface EOF, not an input error");
    assert!(matches!(event, Event::Eof));
}

/// Once virtual input is closed, EOF must be sticky across repeated reads. The
/// internal shutdown channel stays connected through `TermHandle`, so this test
/// prevents a regression where only the first EOF wakeup is delivered and the
/// next read blocks forever.
#[test]
fn virtual_input_disconnect_eof_is_sticky() {
    let buf = SharedBuffer::new();
    let (term, _handle, input_tx) = Term::new_virtual(40, 5, "> ", Box::new(buf), CursorShape::Bar);
    let (event_tx, event_rx) = path_std_sync::mpsc::channel();
    thread::spawn(move || {
        let first = term.get_next_event();
        let second = term.get_next_event();
        let _ = event_tx.send((first, second));
    });
    drop(input_tx);

    let (first, second) = event_rx
        .recv_timeout(path_std_time::Duration::from_secs(2))
        .expect("closed virtual input should not leave repeated reads blocked");
    let first = first.expect("closed virtual input should return EOF");
    let second = second.expect("closed virtual input should keep returning EOF");

    assert!(matches!(first, Event::Eof));
    assert!(matches!(second, Event::Eof));
}

/// Real terminal read errors must propagate through `get_next_event` instead
/// of being treated as EOF.
#[test]
fn real_raw_event_propagates_read_errors() {
    let result = read_real_raw_event(
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

/// An overflowing full redraw must retain its clipped viewport, scrollback,
/// cursor, and cache coordinates as one physical-terminal state.
#[test]
fn full_render_overflow_preserves_visible_scrollback_cursor_and_cache() {
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
    let (mut term, screen) = run_full_render(5, 30, lines, 3, 5, 7);

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
        history_generation: TerminalHistoryGeneration::default(),
        history_width: 30,
        history_height: 6,
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

/// A short full redraw must remain top-aligned with matching cursor and cache
/// coordinates rather than adding synthetic bottom rubber.
#[test]
fn full_render_short_content_starts_at_top_with_matching_cursor_and_cache() {
    // 0 history + 3 live = 3, 10-row terminal.
    // Content starts at the top (no blank padding).
    let lines = plain_lines(&["above", "> hi", "below"]);
    let (term, screen) = run_full_render(10, 30, lines, 0, 1, 4);

    let vis = visible_rows(&term);
    assert_eq!(vis[0], "above");
    assert_eq!(vis[1], "> hi");
    assert_eq!(vis[2], "below");
    // Rest is empty.
    for (i, row) in vis.iter().enumerate().take(10).skip(3) {
        assert_eq!(row, "", "row {i} should be blank");
    }

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
        history_generation: TerminalHistoryGeneration::default(),
        history_width: 30,
        history_height: 2,
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
        history_generation: TerminalHistoryGeneration::default(),
        history_width: 30,
        history_height: 2,
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
        history_generation: TerminalHistoryGeneration::default(),
        history_width: 30,
        history_height: 10,
        cursor_row: 10,
        cursor_col: 8,
    };
    let mut model = TerminalModel {
        viewport_start: 6,
        rubber_height: 0,
        history_generation: TerminalHistoryGeneration::default(),
        history_width: 30,
        history_height: 10,
        active_height: 0,
        known_lines: Vec::new(),
        known_sources: Vec::new(),
    };

    let plan = TerminalModel::bottom_aligned_plan(&layout, 10);
    model.reset_to_layout(&layout, plan.viewport_start, plan.rubber_height);

    assert_eq!(plan.rubber_height, 0);
    assert_eq!(plan.viewport_start, 1);
    assert_eq!(model.viewport_start, 1);
}

/// Empty prompt rendering may show hint text, but the editable buffer and
/// cursor must stay empty so typing replaces the hint instead of appending to
/// it.
#[test]
fn empty_input_renders_placeholder_without_moving_cursor() {
    let mut st = SharedState::new(80, 24, "> ".into());
    st.editor.input_placeholder = Span::new(
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
    assert_eq!(st.editor.buffer, "");
    assert_eq!(st.editor.cursor, 0);
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

/// Keeps raw navigation bounded to its newest entries, so long-lived
/// attachments cannot retain an unbounded recalled-input prefix.
#[test]
fn input_history_retention_evicts_oldest_entries_before_navigation() {
    let buf = SharedBuffer::new();
    let (mut term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    term.seed_input_history((0..=INPUT_HISTORY_MAX_ENTRIES).map(|index| index.to_string()));

    {
        let state = handle.lock();
        assert_eq!(state.editor.input_history.len(), INPUT_HISTORY_MAX_ENTRIES);
        assert_eq!(
            state
                .editor
                .input_history
                .first()
                .expect("entry cap retains newest history")
                .buffer,
            "1"
        );
        assert_eq!(
            state
                .editor
                .input_history
                .last()
                .expect("entry cap retains newest history")
                .buffer,
            INPUT_HISTORY_MAX_ENTRIES.to_string()
        );
    }

    term.trigger_history_step(-1);
    assert_eq!(handle.get_buffer(), INPUT_HISTORY_MAX_ENTRIES.to_string());
}

/// Retains an exact primary-text byte limit, omits one larger submitted draft
/// without evicting the prior suffix, and still routes that draft unchanged.
#[test]
fn input_history_retention_keeps_exact_bytes_and_routes_oversize_prompt() {
    let buf = SharedBuffer::new();
    let (mut term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    term.seed_input_history([String::from("é").repeat(INPUT_HISTORY_MAX_BYTES / 2)]);
    assert_eq!(
        handle
            .lock()
            .editor
            .input_history
            .first()
            .expect("exact byte limit retains the entry")
            .buffer
            .len(),
        INPUT_HISTORY_MAX_BYTES
    );
    skip_virtual_redraw_on_drop(&term);

    let oversize = "x".repeat(INPUT_HISTORY_MAX_BYTES + 1);
    {
        let mut state = handle.lock();
        state.editor.buffer = oversize.clone();
        state.editor.cursor = oversize.len();
    }
    assert!(matches!(
        term.submit_or_accept_completion(),
        Event::Line(line) if line == oversize
    ));
    assert_eq!(
        handle
            .lock()
            .editor
            .input_history
            .first()
            .expect("oversize omission preserves prior history")
            .buffer
            .len(),
        INPUT_HISTORY_MAX_BYTES
    );
}

/// Evicts the oldest raw draft when individually valid entries overflow the
/// aggregate primary-text byte budget.
#[test]
fn input_history_retention_evicts_oldest_entries_for_aggregate_bytes() {
    let buf = SharedBuffer::new();
    let (mut term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    let half_budget = "x".repeat(INPUT_HISTORY_MAX_BYTES / 2);
    term.seed_input_history([
        "old".to_owned() + &half_budget,
        "new".to_owned() + &half_budget,
        "latest".to_owned(),
    ]);

    let state = handle.lock();
    assert_eq!(state.editor.input_history.len(), 2);
    assert_eq!(
        state
            .editor
            .input_history
            .first()
            .expect("aggregate cap retains newest raw draft")
            .buffer,
        format!("new{half_budget}")
    );
    assert_eq!(
        state
            .editor
            .input_history
            .last()
            .expect("aggregate cap retains newest raw draft")
            .buffer,
        "latest"
    );
}

/// Remaps a recalled source across omitted entries, so final redaction cannot
/// leave an earlier raw source navigable after history cleanup.
#[test]
fn input_history_retention_remaps_recalled_source_before_redaction() {
    const SENSITIVE: &str = ":email auth google finish account secret";
    const REDACTED: &str = ":email auth google finish <redacted>";
    let buf = SharedBuffer::new();
    let (mut term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    term.seed_input_history(["discarded".to_owned(), SENSITIVE.to_owned()]);
    {
        let mut state = handle.lock();
        state.editor.input_history[0].buffer.clear();
        state.editor.last_submitted_recalled_source = Some(1);
        state.limit_input_history();
    }

    term.replace_last_submitted_input(REDACTED.to_owned());
    let state = handle.lock();
    assert!(
        state
            .editor
            .input_history
            .iter()
            .all(|draft| draft.buffer == REDACTED)
    );
}

/// Omits an oversize raw-only draft when Down stashes it, without truncating
/// the active buffer that the user can still edit or submit.
#[test]
fn input_history_retention_omits_oversize_stashed_draft() {
    let buf = SharedBuffer::new();
    let (term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    let oversize = "x".repeat(INPUT_HISTORY_MAX_BYTES + 1);
    {
        let mut state = handle.lock();
        state.editor.buffer = oversize;
        state.editor.cursor = state.editor.buffer.len();
        assert!(state.push_current_as_history_entry(true));
        assert!(state.editor.input_history.is_empty());
    }
    assert_eq!(handle.get_buffer(), "");
    skip_virtual_redraw_on_drop(&term);
}

/// Defers aggregate eviction while a recalled draft is edited, so final
/// redaction rewrites both the recalled source and its submitted copy.
#[test]
fn recalled_aggregate_overflow_is_redacted_before_retention() {
    const REDACTED: &str = ":email auth google finish <redacted>";
    let buf = SharedBuffer::new();
    let (mut term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    skip_virtual_redraw_on_drop(&term);
    handle.lock().input_history_limit_override = Some(InputHistoryLimits {
        max_entries: INPUT_HISTORY_MAX_ENTRIES,
        max_bytes: TEST_HISTORY_MAX_BYTES,
    });
    term.defer_submitted_input_history_limit();
    term.seed_input_history([
        "o".repeat(TEST_HISTORY_MAX_BYTES / 2),
        "t".repeat(TEST_HISTORY_MAX_BYTES / 2),
    ]);
    {
        let mut state = handle.lock();
        assert!(state.enter_history_nav(0));
        state.editor.buffer.push_str(" secret");
        state.editor.cursor = state.editor.buffer.len();
        state.sync_buffer_to_history_nav();
    }
    assert!(matches!(term.submit_or_accept_completion(), Event::Line(_)));

    term.replace_last_submitted_input(REDACTED.to_owned());
    term.finalize_submitted_input_history();
    let state = term.handle.lock();
    assert_eq!(state.editor.input_history.len(), 3);
    assert!(
        state
            .editor
            .input_history
            .iter()
            .all(|draft| draft.buffer == REDACTED || draft.buffer.starts_with('o'))
    );
    drop(state);
    skip_virtual_redraw_on_drop(&term);
}

/// Enforces aggregate retention when leaving a recalled edit through empty WIP
/// navigation, so an abandoned edit cannot strand an over-budget history.
#[test]
fn recalled_aggregate_overflow_is_bounded_when_navigation_exits() {
    let buf = SharedBuffer::new();
    let (mut term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    skip_virtual_redraw_on_drop(&term);
    handle.lock().input_history_limit_override = Some(InputHistoryLimits {
        max_entries: INPUT_HISTORY_MAX_ENTRIES,
        max_bytes: TEST_HISTORY_MAX_BYTES,
    });
    term.seed_input_history([
        "o".repeat(TEST_HISTORY_MAX_BYTES / 2),
        "t".repeat(TEST_HISTORY_MAX_BYTES / 2),
    ]);
    {
        let mut state = handle.lock();
        assert!(state.enter_history_nav(0));
        state.editor.buffer.push_str(" overflow");
        state.editor.cursor = state.editor.buffer.len();
        state.sync_buffer_to_history_nav();
        assert!(state.advance_history_nav(1, 0));
        assert!(!state.advance_history_nav(1, 0));
    }

    let state = handle.lock();
    assert_eq!(state.editor.input_history.len(), 1);
    assert!(
        state.editor.input_history[0].buffer.starts_with('t'),
        "newest edited draft remains after old-prefix eviction"
    );
    drop(state);
    skip_virtual_redraw_on_drop(&term);
}

fn overbudget_recalled_navigation() -> (Term, TermHandle, path_std_sync::mpsc::Sender<RawEvent>) {
    let buf = SharedBuffer::new();
    let (mut term, handle, input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    skip_virtual_redraw_on_drop(&term);
    handle.lock().input_history_limit_override = Some(InputHistoryLimits {
        max_entries: INPUT_HISTORY_MAX_ENTRIES,
        max_bytes: TEST_HISTORY_MAX_BYTES,
    });
    term.seed_input_history([
        "o".repeat(TEST_HISTORY_MAX_BYTES / 2),
        "t".repeat(TEST_HISTORY_MAX_BYTES / 2),
    ]);
    {
        let mut state = handle.lock();
        assert!(state.enter_history_nav(0));
        state.editor.buffer.push_str(" overflow");
        state.editor.cursor = state.editor.buffer.len();
        state.sync_buffer_to_history_nav();
    }
    (term, handle, input_tx)
}

fn assert_recalled_overflow_was_bounded(handle: &TermHandle) {
    let state = handle.lock();
    assert_eq!(state.editor.input_history.len(), 1);
    assert!(state.editor.input_history[0].buffer.starts_with('t'));
}

/// Enforces retention at every externally distinct path that abandons an
/// active recalled edit, preventing stale over-budget draft copies.
#[test]
fn recalled_aggregate_overflow_is_bounded_on_every_navigation_exit_path() {
    type ExitPath = fn(&Term, &TermHandle, &path_std_sync::mpsc::Sender<RawEvent>);
    fn set_buffer(
        _term: &Term,
        handle: &TermHandle,
        _input_tx: &path_std_sync::mpsc::Sender<RawEvent>,
    ) {
        handle.set_buffer("replacement".to_owned(), "replacement".len());
    }
    fn set_buffer_preserving_undo(
        _term: &Term,
        handle: &TermHandle,
        _input_tx: &path_std_sync::mpsc::Sender<RawEvent>,
    ) {
        handle.set_buffer_preserving_undo("replacement".to_owned(), "replacement".len());
    }
    fn clear_prompt(
        term: &Term,
        _handle: &TermHandle,
        _input_tx: &path_std_sync::mpsc::Sender<RawEvent>,
    ) {
        assert!(matches!(
            term.trigger_named_action("clear-prompt"),
            Some(Event::BufferChanged)
        ));
    }
    fn clear_or_cancel_prompt(
        term: &Term,
        _handle: &TermHandle,
        _input_tx: &path_std_sync::mpsc::Sender<RawEvent>,
    ) {
        assert!(matches!(
            term.trigger_named_action("clear-or-cancel-prompt"),
            Some(Event::BufferChanged)
        ));
    }
    fn ctrl_c(term: &Term, _handle: &TermHandle, input_tx: &path_std_sync::mpsc::Sender<RawEvent>) {
        input_tx
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )))
            .expect("clear recalled edit with Ctrl-C");
        assert!(matches!(
            term.get_next_event().expect("handle Ctrl-C"),
            Event::BufferChanged
        ));
    }

    for exit in [
        set_buffer as ExitPath,
        set_buffer_preserving_undo,
        clear_prompt,
        clear_or_cancel_prompt,
        ctrl_c,
    ] {
        let (term, handle, input_tx) = overbudget_recalled_navigation();
        exit(&term, &handle, &input_tx);
        assert_recalled_overflow_was_bounded(&handle);
        skip_virtual_redraw_on_drop(&term);
    }
}

/// Keeps a recalled history source linked through queued-prompt recall, so
/// final redaction updates both navigable copies after returning to the draft.
#[test]
fn queued_recall_preserves_source_identity_for_final_redaction() {
    const SOURCE: &str = ":email auth google finish account";
    const REDACTED: &str = ":email auth google finish <redacted>";
    let buf = SharedBuffer::new();
    let (mut term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    term.seed_input_history([SOURCE.to_owned()]);
    {
        let mut state = handle.lock();
        assert!(state.enter_history_nav(0));
    }
    handle.recall_prompt_before_current("queued prompt".to_owned());
    assert_eq!(handle.get_buffer(), "queued prompt");
    {
        let mut state = handle.lock();
        assert!(state.advance_history_nav(1, 0));
    }
    assert_eq!(handle.get_buffer(), SOURCE);
    {
        let mut state = handle.lock();
        state.editor.buffer.push_str(" secret");
        state.editor.cursor = state.editor.buffer.len();
        state.sync_buffer_to_history_nav();
    }
    assert!(matches!(term.submit_or_accept_completion(), Event::Line(_)));

    term.replace_last_submitted_input(REDACTED.to_owned());
    let state = handle.lock();
    assert_eq!(state.editor.input_history.len(), 2);
    assert!(
        state
            .editor
            .input_history
            .iter()
            .all(|draft| draft.buffer == REDACTED)
    );
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

/// A large submitted undo/redo stack must transfer its existing allocations
/// into raw history rather than deep-cloning snapshots that canonical history
/// replacement immediately discards.
#[test]
fn submission_moves_large_undo_and_redo_stacks_without_materializing_copies() {
    const SNAPSHOT_COUNT: usize = 64;
    const SNAPSHOT_BYTES: usize = 64 * 1024;

    let buf = SharedBuffer::new();
    let (mut term, handle, input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    let (undo_allocation, redo_allocation, undo_buffers, redo_buffers) = {
        let mut st = handle.lock();
        st.editor.buffer = "submitted".to_owned();
        st.editor.cursor = st.editor.buffer.len();
        st.editor.current_undo = (0..SNAPSHOT_COUNT)
            .map(|index| PromptSnapshot {
                buffer: char::from(b'a' + (index % 26) as u8)
                    .to_string()
                    .repeat(SNAPSHOT_BYTES),
                cursor: index,
            })
            .collect();
        st.editor.current_redo = (0..SNAPSHOT_COUNT)
            .map(|index| PromptSnapshot {
                buffer: char::from(b'A' + (index % 26) as u8)
                    .to_string()
                    .repeat(SNAPSHOT_BYTES),
                cursor: index,
            })
            .collect();
        (
            st.editor.current_undo.as_ptr() as usize,
            st.editor.current_redo.as_ptr() as usize,
            st.editor
                .current_undo
                .iter()
                .map(|snapshot| snapshot.buffer.as_ptr() as usize)
                .collect::<Vec<_>>(),
            st.editor
                .current_redo
                .iter()
                .map(|snapshot| snapshot.buffer.as_ptr() as usize)
                .collect::<Vec<_>>(),
        )
    };

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("submit prompt");
    assert!(matches!(
        term.get_next_event().expect("submitted line"),
        Event::Line(line) if line == "submitted"
    ));

    {
        let st = handle.lock();
        let submitted = st
            .editor
            .input_history
            .last()
            .expect("submitted history entry");
        assert_eq!(submitted.undo.as_ptr() as usize, undo_allocation);
        assert_eq!(submitted.redo.as_ptr() as usize, redo_allocation);
        assert_eq!(
            submitted
                .undo
                .iter()
                .map(|snapshot| snapshot.buffer.as_ptr() as usize)
                .collect::<Vec<_>>(),
            undo_buffers
        );
        assert_eq!(
            submitted
                .redo
                .iter()
                .map(|snapshot| snapshot.buffer.as_ptr() as usize)
                .collect::<Vec<_>>(),
            redo_buffers
        );
        assert!(st.editor.current_undo.is_empty());
        assert!(st.editor.current_redo.is_empty());
    }

    term.replace_last_submitted_input("canonical".to_owned());
    let st = handle.lock();
    let submitted = st
        .editor
        .input_history
        .last()
        .expect("canonical history entry");
    assert_eq!(submitted.buffer, "canonical");
    assert!(submitted.undo.is_empty());
    assert!(submitted.redo.is_empty());
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
            assert_eq!(st.terminal.redraw_suppression, 1);
            assert!(st.terminal.redraw_dirty_while_suppressed);
            st.terminal.redraw_dirty_while_suppressed = false;
        }

        handle.print_output("suppressed-output", plain_block("hidden update"));
        {
            let mut st = handle.lock();
            assert!(st.terminal.redraw_dirty_while_suppressed);
            st.terminal.redraw_dirty_while_suppressed = false;
        }

        handle.print_osc1337_set_user_var("CurrentDir", "/tmp", false);
        {
            let mut st = handle.lock();
            assert!(st.terminal.redraw_dirty_while_suppressed);
            st.terminal.redraw_dirty_while_suppressed = false;
        }

        let snapshot = handle.output_snapshot();
        handle.replace_output_snapshot(snapshot);
        let st = handle.lock();
        assert!(st.terminal.redraw_dirty_while_suppressed);
    });

    {
        let st = handle.lock();
        assert_eq!(st.terminal.redraw_suppression, 0);
        assert!(!st.terminal.redraw_dirty_while_suppressed);
    }

    drop(term);
}

/// A redraw notification consumed by the worker during suppression must remain
/// dirty so the outer guard republishes it even when the transaction is a
/// no-op.
#[test]
fn consumed_redraw_remains_pending_during_suppression() {
    let buf = SharedBuffer::new();
    let (term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    handle.redraw_sync();

    handle.with_redraw_suppressed(|| {
        // Force the real redraw worker to consume a sync notification while
        // this scope prevents rendering.
        handle.redraw_sync();
        let st = handle.lock();
        assert_eq!(st.terminal.redraw_suppression, 1);
        assert!(st.terminal.redraw_dirty_while_suppressed);
    });

    assert_eq!(handle.lock().terminal.redraw_suppression, 0);
    drop(term);
}

/// Dropping a dirty outer suppression guard must notify its redraw channel
/// independently of redraw-worker scheduling.
#[test]
fn dirty_suppression_guard_notifies_redraw_channel() {
    let buf = SharedBuffer::new();
    let (term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    handle.redraw_sync();
    let (redraw, redraw_rx) = tau_blocking_notify_channel::channel();
    let isolated_handle = TermHandle {
        redraw,
        ..handle.clone()
    };

    isolated_handle.with_redraw_suppressed(|| {
        isolated_handle
            .lock()
            .terminal
            .redraw_dirty_while_suppressed = true;
    });

    assert_eq!(
        redraw_rx.try_recv(),
        Ok(tau_blocking_notify_channel::TryRecvStatus::Notified)
    );
    drop(term);
}

/// Output transactions must make a caller's multi-step visible snapshot
/// replacement atomic with respect to cloned handles that print concurrently.
/// This generic guarantee remains useful even though hidden-agent folding owns
/// detached models and no longer installs temporary terminal snapshots.
#[test]
fn output_transaction_blocks_concurrent_local_output_until_visible_snapshot_restored() {
    let buf = SharedBuffer::new();
    let (term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);

    handle.print_output("visible-base", plain_block("visible base"));
    let visible_snapshot = handle.output_snapshot();
    handle.clear_output();
    handle.print_output("hidden-base", plain_block("hidden base"));
    let hidden_snapshot = handle.output_snapshot();

    let (attempt_tx, attempt_rx) = path_std_sync::mpsc::channel();
    let (printed_tx, printed_rx) = path_std_sync::mpsc::channel();
    let local_handle = handle.clone();

    let worker = handle.with_output_transaction(|| {
        handle.replace_output_snapshot_quiet(hidden_snapshot);
        let worker = std::thread::spawn(move || {
            attempt_tx.send(()).expect("attempt signal should send");
            local_handle.print_output("local-output", plain_block("local visible output"));
            printed_tx.send(()).expect("printed signal should send");
        });
        attempt_rx
            .recv_timeout(path_std_time::Duration::from_secs(1))
            .expect("local output thread should attempt to print");
        assert!(
            printed_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "local output printed while hidden snapshot was installed"
        );

        handle.replace_output_snapshot_quiet(visible_snapshot);
        worker
    });

    printed_rx
        .recv_timeout(path_std_time::Duration::from_secs(1))
        .expect("local output should print after transaction exits");
    worker.join().expect("local output worker should finish");
    let snapshot = handle.output_snapshot();
    let rendered_text = snapshot
        .history
        .iter()
        .filter_map(|id| snapshot.blocks.get(id))
        .flat_map(|block| block.content.spans().iter())
        .map(|span| span.text.as_str())
        .collect::<String>();
    assert!(rendered_text.contains("visible base"));
    assert!(rendered_text.contains("local visible output"));
    assert!(!rendered_text.contains("hidden base"));

    drop(term);
}

/// Taking a visible snapshot must move its block map and zones intact, so
/// transcript selection does not duplicate styled blocks before restoring them.
#[test]
fn take_output_snapshot_transfers_visible_allocations_without_cloning() {
    let (_term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(std::io::sink()), CursorShape::Bar);
    let block_id = handle.new_block("visible", plain_block("visible transcript"));
    handle.push_history(block_id);
    let (block_pointer, history_pointer) = {
        let state = handle.lock();
        (
            std::ptr::from_ref(
                state
                    .layout
                    .blocks
                    .get(&block_id)
                    .expect("visible block must be installed"),
            ),
            state.layout.history.as_ptr(),
        )
    };
    let cloned_before = handle.output_snapshot_count();
    let taken_before = handle.output_snapshot_take_count();

    let snapshot = handle.take_output_snapshot();

    assert_eq!(handle.output_snapshot_count(), cloned_before);
    assert_eq!(handle.output_snapshot_take_count(), taken_before + 1);
    assert_eq!(
        std::ptr::from_ref(
            snapshot
                .blocks
                .get(&block_id)
                .expect("taken snapshot must retain the visible block"),
        ),
        block_pointer
    );
    assert_eq!(snapshot.history.as_ptr(), history_pointer);
    assert!(handle.lock().layout.blocks.is_empty());

    handle.replace_output_snapshot(snapshot);
    let state = handle.lock();
    assert_eq!(
        std::ptr::from_ref(
            state
                .layout
                .blocks
                .get(&block_id)
                .expect("restored block must be installed"),
        ),
        block_pointer
    );
    assert_eq!(state.layout.history.as_ptr(), history_pointer);
}

/// Detached snapshot mutation must preserve the same block identities, content,
/// and zone semantics as applying the equivalent operations through a terminal
/// handle. The CLI relies on this parity before materializing on selection.
#[test]
fn output_snapshot_mutation_matches_terminal_handle() {
    let (_term, handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(std::io::sink()), CursorShape::Bar);
    let mut model = OutputSnapshot::default();

    let terminal_first = handle.new_block("first", plain_block("first"));
    let model_first = model.new_block("first", plain_block("first"));
    assert_eq!(terminal_first, model_first);
    handle.push_history(terminal_first);
    model.push_history(model_first);

    let terminal_live = handle.new_block("live", plain_block("live"));
    let model_live = model.new_block("live", plain_block("live"));
    handle.push_above_active(terminal_live);
    model.push_above_active(model_live);
    handle.push_above_sticky(terminal_live);
    model.push_above_sticky(model_live);
    handle.push_below(terminal_live);
    model.push_below(model_live);
    handle.set_block(terminal_live, plain_block("updated"));
    model.set_block(model_live, plain_block("updated"));
    handle.remove_above_sticky(terminal_live);
    model.remove_above_sticky(model_live);

    let terminal_before = handle.new_block("before", plain_block("before"));
    let model_before = model.new_block("before", plain_block("before"));
    handle.push_above_active_before_any(terminal_before, [terminal_live]);
    model.push_above_active_before_any(model_before, [model_live]);

    let terminal_printed = handle.print_output("printed", plain_block("printed"));
    let model_printed = model.print_output("printed", plain_block("printed"));
    assert_eq!(terminal_printed, model_printed);

    let terminal_removed = handle.new_block("removed", plain_block("removed"));
    let model_removed = model.new_block("removed", plain_block("removed"));
    handle.push_above_active(terminal_removed);
    model.push_above_active(model_removed);
    handle.remove_block(terminal_removed);
    model.remove_block(model_removed);

    let terminal = handle.output_snapshot();
    assert_eq!(
        terminal
            .blocks
            .keys()
            .collect::<std::collections::HashSet<_>>(),
        model
            .blocks
            .keys()
            .collect::<std::collections::HashSet<_>>()
    );
    for (id, terminal_block) in &terminal.blocks {
        assert_eq!(
            layout_block(terminal_block, 80),
            layout_block(model.blocks.get(id).expect("matching model block"), 80),
            "block {id:?} has different fixed-width layout"
        );
    }
    assert_eq!(terminal.block_debug_ids, model.block_debug_ids);
    assert_eq!(terminal.history, model.history);
    assert_eq!(terminal.above_active, model.above_active);
    assert_eq!(terminal.above_sticky, model.above_sticky);
    assert_eq!(terminal.suggestions, model.suggestions);
    assert_eq!(terminal.below, model.below);
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
        handle.lock().editor.sticky_col = Some(6);
        assert!(
            term.handle_key(key).expect("home/control-a key").is_none(),
            "boundary-start key should not emit an event"
        );
        assert_eq!(handle.get_cursor(), 0);
        assert_eq!(handle.lock().editor.sticky_col, None);
    }

    for key in [
        KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
    ] {
        handle.set_buffer("abcdef".to_owned(), 6);
        handle.lock().editor.sticky_col = Some(6);
        assert!(
            term.handle_key(key).expect("end/control-e key").is_none(),
            "boundary-end key should not emit an event"
        );
        assert_eq!(handle.get_cursor(), 6);
        assert_eq!(handle.lock().editor.sticky_col, None);
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

/// A terminal sink blocked inside `write` must not hold prompt-input state or
/// prevent the caller from resolving a cancellation event.
#[test]
fn blocked_terminal_sink_keeps_cancel_input_responsive() {
    /// Writer that announces its first write and blocks until released.
    struct BlockingWriter {
        /// Signals that a terminal write reached the sink.
        entered: path_std_sync::mpsc::Sender<()>,
        /// Shared release flag and condition variable for the blocked write.
        release: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl std::io::Write for BlockingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let _ = self.entered.send(());
            let (lock, wake) = &*self.release;
            let released = lock.lock().expect("release mutex");
            let _released = wake
                .wait_while(released, |released| !*released)
                .expect("release mutex");
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let (entered_tx, entered_rx) = path_std_sync::mpsc::channel();
    let release = path_std_sync::Arc::new((
        path_std_sync::Mutex::new(false),
        path_std_sync::Condvar::new(),
    ));
    let (term, _handle, input_tx) = Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(BlockingWriter {
            entered: entered_tx,
            release: release.clone(),
        }),
        CursorShape::Bar,
    );
    entered_rx
        .recv_timeout(path_std_time::Duration::from_secs(1))
        .expect("redraw entered blocked sink");

    for _ in 0..2 {
        input_tx
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )))
            .expect("ctrl-c");
    }
    assert!(matches!(
        term.get_next_event().expect("first ctrl-c"),
        Event::Notice(_)
    ));
    assert!(matches!(
        term.get_next_event().expect("second ctrl-c"),
        Event::CancelPrompt
    ));

    let (lock, wake) = &*release;
    *lock.lock().expect("release mutex") = true;
    wake.notify_all();
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

    fn is_empty(&self) -> bool {
        self.0.lock().expect("shared buffer poisoned").is_empty()
    }

    fn drain_bytes(&self) -> Vec<u8> {
        let mut buf = self.0.lock().expect("shared buffer poisoned");
        std::mem::take(&mut *buf)
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

/// Terminal side effects are intentionally narrow: OSC user-var values are
/// encoded by the raw layer and invalid names are skipped instead of allowing
/// arbitrary escape injection.
#[test]
fn terminal_side_effect_api_encodes_and_validates_osc_user_vars() {
    let buf = SharedBuffer::new();
    let (term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    handle.redraw_sync();
    let _ = buf.drain_bytes();

    handle.print_osc1337_set_user_var("user-notification", "hello", false);
    handle.redraw_sync();
    let bytes = String::from_utf8(buf.drain_bytes()).expect("utf8 output");
    assert!(bytes.contains("\x1b]1337;SetUserVar=user-notification=aGVsbG8=\x07"));

    handle.print_osc1337_set_user_var("user-notification", "hello", true);
    handle.redraw_sync();
    let bytes = String::from_utf8(buf.drain_bytes()).expect("utf8 output");
    assert!(
        bytes.contains("\x1bPtmux;\x1b\x1b]1337;SetUserVar=user-notification=aGVsbG8=\x07\x1b\\")
    );

    handle.print_osc1337_set_user_var("bad=key", "ignored", false);
    handle.redraw_sync();
    let bytes = String::from_utf8(buf.drain_bytes()).expect("utf8 output");
    assert!(
        !bytes.contains("SetUserVar=") && !bytes.contains("\x1b]1337;"),
        "invalid OSC names must not emit an OSC side effect: {bytes:?}"
    );

    drop(term);
}

/// The redraw pipeline must emit pending raw side effects before a bracketed
/// full frame while leaving ordinary differential and scrolling paths unmarked.
#[test]
fn redraw_pipeline_brackets_only_full_frames_after_pending_side_effects() {
    let buf = SharedBuffer::new();
    let (term, handle, _input_tx) =
        Term::new_virtual(40, 4, "> ", Box::new(buf.clone()), CursorShape::Bar);
    handle.invalidate_screen();
    handle.redraw_sync();
    let initial = buf.drain_bytes();
    assert_eq!(
        initial
            .windows(8)
            .filter(|window| *window == b"\x1b[?2026h")
            .count(),
        1
    );
    assert_eq!(
        initial
            .windows(8)
            .filter(|window| *window == b"\x1b[?2026l")
            .count(),
        1
    );

    handle.set_buffer("diff".to_owned(), 4);
    handle.redraw_sync();
    let diff = buf.drain_bytes();
    assert!(!diff.windows(8).any(|window| window == b"\x1b[?2026h"));
    assert!(!diff.windows(8).any(|window| window == b"\x1b[?2026l"));

    for line in 0..8 {
        handle.print_output("scroll", format!("scroll {line}"));
    }
    handle.redraw_sync();
    let scroll = buf.drain_bytes();
    assert!(!scroll.windows(8).any(|window| window == b"\x1b[?2026h"));
    assert!(!scroll.windows(8).any(|window| window == b"\x1b[?2026l"));

    handle.print_osc1337_set_user_var("order", "before", false);
    handle.invalidate_screen();
    handle.redraw_sync();
    let full = buf.drain_bytes();
    let osc = full
        .windows(b"\x1b]1337;SetUserVar=".len())
        .position(|window| window == b"\x1b]1337;SetUserVar=")
        .expect("pending OSC");
    let begin = full
        .windows(8)
        .position(|window| window == b"\x1b[?2026h")
        .expect("BSU");
    let end = full
        .windows(8)
        .position(|window| window == b"\x1b[?2026l")
        .expect("ESU");
    assert!(osc < begin && begin < end);
    assert_eq!(
        full.windows(8)
            .filter(|window| *window == b"\x1b[?2026h")
            .count(),
        1
    );
    assert_eq!(
        full.windows(8)
            .filter(|window| *window == b"\x1b[?2026l")
            .count(),
        1
    );

    drop(term);
    let shutdown = buf.drain_bytes();
    assert!(!shutdown.windows(8).any(|window| window == b"\x1b[?2026h"));
    assert!(!shutdown.windows(8).any(|window| window == b"\x1b[?2026l"));
}

/// Resuming from an external command must invalidate the virtual terminal and
/// select the synchronized full-render path on the next completed repaint.
#[test]
fn external_resume_selects_bracketed_full_render() {
    let buf = SharedBuffer::new();
    let (term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    handle.redraw_sync();
    let _ = buf.drain_bytes();

    term.resume_after_external().expect("virtual resume");
    handle.redraw_sync();
    let resumed = buf.drain_bytes();

    assert!(resumed.windows(8).any(|window| window == b"\x1b[?2026h"));
    assert!(resumed.windows(8).any(|window| window == b"\x1b[?2026l"));
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

/// Prompt overflow must hide the complete right-side context rather than
/// leaving either its directory or session-id component behind.
#[test]
fn prompt_overflow_hides_complete_right_context() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(5, 25, 20);
    let (term, handle, input_tx) =
        Term::new_virtual(25, 5, "> ", Box::new(buf.clone()), CursorShape::Bar);
    handle.set_right_prompt(StyledText::from("/project &session-1"));
    flush_redraws(&handle, &buf, &mut parser);
    assert!(screen_contains(&parser, 25, "/project"));
    assert!(screen_contains(&parser, 25, "&session-1"));
    input_tx
        .send(RawEvent::Paste("123456789".to_owned()))
        .expect("send input");
    assert!(matches!(
        term.get_next_event().expect("input event"),
        Event::BufferChanged
    ));
    flush_redraws(&handle, &buf, &mut parser);

    assert!(!screen_contains(&parser, 25, "/project"));
    assert!(!screen_contains(&parser, 25, "&session-1"));
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

/// Prompt cursor placement must use the final column before the line becomes
/// full; prompt wrapping differs from generic block wrapping.
#[test]
fn prompt_input_cursor_uses_last_column_before_line_is_full() {
    let mut st = SharedState::new(10, 30, StyledText::from("> "));
    st.editor.buffer = "abcdefg".to_owned();
    st.editor.cursor = st.editor.buffer.len();

    let layout = layout_all(&st);

    assert_eq!(layout.all_lines.len(), 1, "prompt height");
    assert_eq!(line_text(&layout.all_lines[0]), "> abcdefg");
    assert_eq!((layout.cursor_row, layout.cursor_col), (0, 9));
}

/// A prompt that fills its last column must move the cursor immediately to the
/// next visual row at column zero.
#[test]
fn prompt_input_cursor_wraps_to_new_line_when_last_column_is_filled() {
    let mut st = SharedState::new(10, 30, StyledText::from("> "));
    st.editor.buffer = "abcdefgh".to_owned();
    st.editor.cursor = st.editor.buffer.len();

    let layout = layout_all(&st);

    assert_eq!(layout.all_lines.len(), 2, "prompt height");
    assert_eq!(line_text(&layout.all_lines[0]), "> abcdefgh");
    assert_eq!(line_text(&layout.all_lines[1]), "");
    assert_eq!((layout.cursor_row, layout.cursor_col), (1, 0));
}

/// An explicit newline after an exact-width prompt line must consume its
/// pending wrap instead of adding a phantom blank row.
#[test]
fn prompt_input_newline_after_filled_line_does_not_add_phantom_row() {
    let mut st = SharedState::new(10, 30, StyledText::from("> "));
    st.editor.buffer = "abcdefgh\n".to_owned();
    st.editor.cursor = st.editor.buffer.len();

    let layout = layout_all(&st);

    assert_eq!(layout.all_lines.len(), 2, "prompt height");
    assert_eq!(line_text(&layout.all_lines[0]), "> abcdefgh");
    assert_eq!(line_text(&layout.all_lines[1]), "");
    assert_eq!((layout.cursor_row, layout.cursor_col), (1, 0));
}

/// Text after an exact-width line and newline must share the immediate next row
/// with its cursor rather than leaving the cursor one row too low.
#[test]
fn prompt_input_text_after_newline_after_filled_line_keeps_cursor_on_text_row() {
    let mut st = SharedState::new(10, 30, StyledText::from("> "));
    st.editor.buffer = "abcdefgh\nZ".to_owned();
    st.editor.cursor = st.editor.buffer.len();

    let layout = layout_all(&st);

    assert_eq!(layout.all_lines.len(), 2, "prompt height");
    assert_eq!(line_text(&layout.all_lines[0]), "> abcdefgh");
    assert_eq!(line_text(&layout.all_lines[1]), "Z");
    assert_eq!((layout.cursor_row, layout.cursor_col), (1, 1));
}

/// Repeated exact-width lines ending in newlines must not accumulate phantom
/// rows, keeping cursor accounting and rendered prompt height in lockstep.
#[test]
fn prompt_input_repeated_full_lines_ending_in_newline_do_not_stack_phantom_rows() {
    let mut st = SharedState::new(10, 30, StyledText::from("> "));
    st.editor.buffer = "abcdefgh\nabcdefghij\n".to_owned();
    st.editor.cursor = st.editor.buffer.len();

    let layout = layout_all(&st);

    assert_eq!(layout.all_lines.len(), 3, "prompt height");
    assert_eq!(line_text(&layout.all_lines[0]), "> abcdefgh");
    assert_eq!(line_text(&layout.all_lines[1]), "abcdefghij");
    assert_eq!(line_text(&layout.all_lines[2]), "");
    assert_eq!((layout.cursor_row, layout.cursor_col), (2, 0));
}

/// Prompt height caps must reserve at most 33 percent of terminal rows, rounded
/// down, while keeping one editable row available on tiny terminals.
#[test]
fn prompt_input_height_cap_uses_floor_third_with_minimum_one() {
    assert_eq!(prompt_input_max_rows(0), 1);
    assert_eq!(prompt_input_max_rows(1), 1);
    assert_eq!(prompt_input_max_rows(3), 1);
    assert_eq!(prompt_input_max_rows(9), 2);
    assert_eq!(prompt_input_max_rows(12), 3);
}

/// A capped prompt viewport must show its hidden-row indicator while retaining
/// the newest editable rows.
#[test]
fn prompt_input_layout_caps_height_and_shows_hidden_row_indicator() {
    let mut st = SharedState::new(12, 12, StyledText::from("> "));
    st.editor.buffer = "a\nb\nc\nd\ne".to_owned();
    st.write_cursor(st.editor.buffer.len());

    let layout = layout_all(&st);

    assert_eq!(layout.all_lines.len(), 3, "cap floor(12*33%) = 3 rows");
    assert!(line_text(&layout.all_lines[0]).starts_with('↕'));
    assert_eq!(line_text(&layout.all_lines[1]), "d");
    assert_eq!(line_text(&layout.all_lines[2]), "e");
    assert_eq!(layout.line_sources[0], LineSource::InputScrollIndicator);
}

/// A one-row prompt cap must preserve the editable row and omit an indicator
/// that would consume the only available row.
#[test]
fn prompt_input_cap_one_keeps_editable_row_and_suppresses_indicator() {
    let mut st = SharedState::new(12, 1, StyledText::from("> "));
    st.editor.buffer = "a\nb\nc".to_owned();
    st.write_cursor(st.editor.buffer.len());

    let layout = layout_all(&st);

    assert_eq!(layout.all_lines.len(), 1);
    assert_eq!(line_text(&layout.all_lines[0]), "c");
    assert_eq!(layout.line_sources[0], LineSource::Input { wrapped_row: 2 });
}

/// Disabling the prompt scroll indicator must still expose the newest capped
/// input rows.
#[test]
fn prompt_input_scroll_indicator_can_be_disabled() {
    let mut st = SharedState::new(12, 9, StyledText::from("> "));
    st.editor.show_prompt_scroll_indicator = false;
    st.editor.buffer = "a\nb\nc\nd".to_owned();
    st.write_cursor(st.editor.buffer.len());

    let layout = layout_all(&st);

    assert_eq!(layout.all_lines.len(), 2, "cap floor(9*33%) = 2 rows");
    assert_eq!(line_text(&layout.all_lines[0]), "c");
    assert_eq!(line_text(&layout.all_lines[1]), "d");
}

/// Resizing taller must clamp a previously scrolled prompt viewport so newly
/// available rows reappear.
#[test]
fn prompt_input_resize_clamps_viewport_when_more_rows_fit() {
    let mut st = SharedState::new(12, 12, StyledText::from("> "));
    st.editor.buffer = "a\nb\nc\nd\ne".to_owned();
    st.write_cursor(st.editor.buffer.len());
    assert_eq!(st.editor.input_viewport_start, 3);

    st.terminal.height = 30;
    st.ensure_input_cursor_visible();

    assert_eq!(st.editor.input_viewport_start, 0);
    let layout = layout_all(&st);
    assert_eq!(layout.all_lines.len(), 5);
}

/// Plain Up must scroll locally before it starts navigating submitted history.
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
        st.editor.input_viewport_start = 2;
    }
    assert_eq!(handle.lock().editor.input_viewport_start, 2);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("up");
    let _ = term.get_next_event().expect("up event");

    assert_eq!(handle.get_buffer(), "a\nb\nc\nd\ne");
    assert_eq!(handle.lock().editor.input_viewport_start, 1);

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

/// The explicit history shortcut must bypass local prompt scrolling and recall
/// history immediately.
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

/// Completion-menu navigation must take precedence over local prompt scrolling.
#[test]
fn prompt_input_completion_menu_keeps_priority_over_local_scroll() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(12, 12, "> ", Box::new(buf), CursorShape::Bar);
    handle.set_buffer("a\nb\nc\nd\ne".to_owned(), 9);
    {
        let mut st = handle.lock();
        st.editor.completion = Some(CompletionMenu {
            candidates: vec![Candidate {
                label: "x".to_owned(),
                description: "candidate".to_owned(),
                replacement: "replacement".to_owned(),
                cursor: "replacement".len(),
            }],
            selected: None,
            original_buffer: st.editor.buffer.clone(),
            original_cursor: st.editor.cursor,
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

/// The prompt scroll indicator must fit a one-column terminal without wrapping.
#[test]
fn prompt_input_indicator_fits_tiny_terminal_width() {
    let mut st = SharedState::new(1, 7, StyledText::from(""));
    st.editor.buffer = "a\nb\nc".to_owned();
    st.write_cursor(st.editor.buffer.len());

    let layout = layout_all(&st);
    let indicator = line_text(&layout.all_lines[0]);

    assert_eq!(layout.line_sources[0], LineSource::InputScrollIndicator);
    assert!(
        display_width(&indicator) <= 1,
        "indicator must not wrap: {indicator:?}"
    );
}

/// Plain Down must scroll a locally clipped prompt before creating or
/// navigating a history entry.
#[test]
fn prompt_input_plain_down_scrolls_before_history_navigation() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(12, 12, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("a\nb\nc\nd\ne".to_owned(), 5);
    {
        let mut st = handle.lock();
        st.editor.input_viewport_start = 1;
    }
    assert_eq!(handle.lock().editor.input_viewport_start, 1);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("down");
    let _ = term.get_next_event().expect("down event");

    assert_eq!(handle.get_buffer(), "a\nb\nc\nd\ne");
    assert_eq!(handle.lock().editor.input_viewport_start, 2);
}

/// The explicit next-history shortcut must bypass local prompt scrolling.
#[test]
fn prompt_input_explicit_next_history_shortcut_bypasses_local_scroll() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(12, 12, "> ", Box::new(buf), CursorShape::Bar);

    handle.set_buffer("a\nb\nc\nd\ne".to_owned(), 5);
    {
        let mut st = handle.lock();
        st.editor.input_viewport_start = 1;
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

    let barrier = Arc::new(path_std_sync::Barrier::new(4));
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

/// Controls one flush that blocks until released and then reports an error.
#[derive(Clone)]
struct FailingFlushWriter {
    /// Shared writer observations and failure gate.
    inner: Arc<(Mutex<FailingFlushWriterState>, path_std_sync::Condvar)>,
}

/// Mutable observations for [`FailingFlushWriter`].
struct FailingFlushWriterState {
    /// Whether the first flush may return its injected error.
    release_failure: bool,
    /// Whether the renderer is currently inside the failing flush.
    flush_blocked: bool,
    /// Number of writes attempted against the underlying writer.
    writes: usize,
    /// Number of flushes attempted against the underlying writer.
    flushes: usize,
}

impl FailingFlushWriter {
    /// Creates a writer whose first flush waits for explicit release.
    fn new() -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(FailingFlushWriterState {
                    release_failure: false,
                    flush_blocked: false,
                    writes: 0,
                    flushes: 0,
                }),
                path_std_sync::Condvar::new(),
            )),
        }
    }

    /// Waits until the redraw owner reaches the injected flush.
    fn wait_until_blocked(&self) {
        let (state, condvar) = &*self.inner;
        let guard = state.lock().expect("failing writer poisoned");
        drop(
            condvar
                .wait_while(guard, |state| !state.flush_blocked)
                .expect("failing writer poisoned"),
        );
    }

    /// Lets the first flush return its injected error.
    fn release_failure(&self) {
        let (state, condvar) = &*self.inner;
        state
            .lock()
            .expect("failing writer poisoned")
            .release_failure = true;
        condvar.notify_all();
    }

    /// Returns the underlying write and flush attempt counts.
    fn counts(&self) -> (usize, usize) {
        let (state, _) = &*self.inner;
        let state = state.lock().expect("failing writer poisoned");
        (state.writes, state.flushes)
    }
}

impl io::Write for FailingFlushWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let (state, _) = &*self.inner;
        state.lock().expect("failing writer poisoned").writes += 1;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let (state, condvar) = &*self.inner;
        let mut state = state.lock().expect("failing writer poisoned");
        state.flushes += 1;
        state.flush_blocked = true;
        condvar.notify_all();
        drop(
            condvar
                .wait_while(state, |state| !state.release_failure)
                .expect("failing writer poisoned"),
        );
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "first flush failed",
        ))
    }
}

/// The first reported output failure must wake input and every redraw waiter,
/// retain its identity, stop all later writes, and let `Drop` join the failed
/// renderer without retrying terminal cleanup through the virtual writer.
#[test]
fn output_failure_fail_stops_attachment_and_releases_all_waiters() {
    let writer = FailingFlushWriter::new();
    let (term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(writer.clone()), CursorShape::Bar);
    writer.wait_until_blocked();

    let sync_threads: Vec<_> = (0..4)
        .map(|_| {
            let handle = handle.clone();
            thread::spawn(move || handle.redraw_sync())
        })
        .collect();
    let (term_tx, term_rx) = path_std_sync::mpsc::channel();
    thread::spawn(move || {
        let result = term.get_next_event();
        let _ = term_tx.send((result, term));
    });

    writer.release_failure();
    for thread in sync_threads {
        thread.join().expect("redraw waiter should be released");
    }
    let (result, term) = term_rx
        .recv_timeout(path_std_time::Duration::from_secs(2))
        .expect("output failure should wake input");
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("input owner should receive output failure"),
    };
    assert!(is_output_failure(&error));
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert!(error.to_string().contains("first flush failed"));

    let counts_after_failure = writer.counts();
    handle.redraw();
    handle.redraw_sync();
    assert_eq!(writer.counts(), counts_after_failure);

    fail_terminal_output(
        &handle.state,
        &handle.input_tx,
        &handle.sync_condvar,
        io::Error::new(io::ErrorKind::PermissionDenied, "later cleanup failed"),
    );
    let retained = handle
        .lock()
        .terminal
        .output_failure
        .as_ref()
        .expect("first output failure retained")
        .clone();
    assert_eq!(retained.kind, io::ErrorKind::BrokenPipe);
    assert_eq!(retained.message, "first flush failed");

    drop(term);
    assert_eq!(writer.counts(), counts_after_failure);
}

/// A writer that rejects every byte at the selected render helper boundary.
struct RejectWrites;

impl io::Write for RejectWrites {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("injected helper write failure"))
    }

    fn flush(&mut self) -> io::Result<()> {
        panic!("helper-level write failure must return before pass flush")
    }
}

/// Builds one ordinary redraw pass without spawning the production redraw
/// owner, allowing tests to inject directly at helper writes.
fn prepared_test_redraw(
    force_full: bool,
) -> (
    Arc<Mutex<SharedState>>,
    HistoryLayoutCache,
    TerminalModel,
    RedrawPass,
) {
    let state = Arc::new(Mutex::new(SharedState::new(40, 5, "> ".into())));
    state
        .lock()
        .expect("term state mutex poisoned")
        .terminal
        .invalidate_screen = force_full;
    let mut history_cache = HistoryLayoutCache::default();
    let terminal_model = TerminalModel::default();
    let pass = prepare_redraw_pass(
        &state,
        &mut history_cache,
        &terminal_model,
        40,
        5,
        &path_std_sync::Condvar::new(),
    )
    .expect("test redraw should prepare");
    (state, history_cache, terminal_model, pass)
}

/// Returns a zero-capacity buffer so selected helper writes reach the injected
/// writer immediately rather than moving the error to the pass-ending flush.
fn rejecting_buffer() -> BufWriter<Box<dyn Write + Send>> {
    BufWriter::with_capacity(0, Box::new(RejectWrites))
}

/// Pending raw side effects must return their write error from
/// `render_redraw_pass` before the normal frame renderer runs.
#[test]
fn pending_raw_write_error_propagates_from_redraw_pass() {
    let (state, history_cache, mut terminal_model, mut pass) = prepared_test_redraw(false);
    pass.pending_raw.push("\x07".to_owned());
    let mut screen = Screen::new(40);
    let error = render_redraw_pass(
        &state,
        &mut rejecting_buffer(),
        &mut screen,
        &history_cache,
        &mut terminal_model,
        &pass,
    )
    .expect_err("pending raw write should fail");
    assert_eq!(error.to_string(), "injected helper write failure");
}

/// Differential screen updates must return their helper-level write error
/// instead of advancing a stale terminal model.
#[test]
fn differential_write_error_propagates_from_render_helper() {
    let all_lines = plain_lines(&["line", "> "]);
    let layout = LayoutAll {
        line_sources: (0..all_lines.len())
            .map(|wrapped_row| LineSource::Input { wrapped_row })
            .collect(),
        all_lines,
        log_end: 1,
        history_generation: TerminalHistoryGeneration::default(),
        history_width: 40,
        history_height: 1,
        cursor_row: 1,
        cursor_col: 2,
    };
    let mut terminal_model = TerminalModel::default();
    let plan = terminal_model.plan_view(&layout, 5);
    let (_, _, _, pass) = prepared_test_redraw(false);
    let mut screen = Screen::new(40);
    render_diff_frame(
        &mut rejecting_buffer(),
        &mut screen,
        &mut terminal_model,
        &pass,
        &layout,
        plan,
    )
    .expect_err("differential helper write should fail");
}

/// Full renders must return synchronized-output helper write errors from the
/// redraw pass rather than swallowing them and reporting a successful frame.
#[test]
fn full_render_write_error_propagates_from_redraw_pass() {
    let (state, history_cache, mut terminal_model, pass) = prepared_test_redraw(true);
    assert!(matches!(pass.frame, RenderFrame::Full { .. }));
    let mut screen = Screen::new(40);
    render_redraw_pass(
        &state,
        &mut rejecting_buffer(),
        &mut screen,
        &history_cache,
        &mut terminal_model,
        &pass,
    )
    .expect_err("full-render helper write should fail");
}

/// Native scrolling must return its helper-level write error before resetting
/// the terminal model to an undelivered layout.
#[test]
fn scrolling_write_error_propagates_from_render_helper() {
    let all_lines = plain_lines(&["0", "1", "2", "3", "4", "5", "> "]);
    let layout = LayoutAll {
        line_sources: (0..all_lines.len())
            .map(|wrapped_row| LineSource::Input { wrapped_row })
            .collect(),
        all_lines,
        log_end: 6,
        history_generation: TerminalHistoryGeneration::default(),
        history_width: 40,
        history_height: 6,
        cursor_row: 6,
        cursor_col: 2,
    };
    let mut terminal_model = TerminalModel::default();
    let plan = terminal_model.plan_view(&layout, 5);
    assert!(terminal_model.viewport_start < plan.viewport_start);
    let (_, _, _, pass) = prepared_test_redraw(false);
    let mut screen = Screen::new(40);
    render_scrolling_frame(
        &mut rejecting_buffer(),
        &mut screen,
        &mut terminal_model,
        &pass,
        &layout,
        plan,
    )
    .expect_err("scroll helper write should fail");
}

/// Records underlying writes while rejecting only the first attempt.
#[derive(Clone)]
struct FailFirstBufferedWrite {
    /// Attempt count and bytes accepted after the injected first failure.
    inner: Arc<Mutex<(usize, Vec<u8>)>>,
}

impl io::Write for FailFirstBufferedWrite {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut inner = self.inner.lock().expect("failure probe poisoned");
        inner.0 += 1;
        if inner.0 == 1 {
            return Err(io::Error::other("first buffered write failed"));
        }
        inner.1.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A failed `BufWriter::flush_buf` retains the normal frame bytes, so fail-stop
/// must consume the buffer without letting `BufWriter::drop` retry them.
#[test]
fn failed_buffered_frame_is_discarded_without_drop_retry() {
    let probe = FailFirstBufferedWrite {
        inner: Arc::new(Mutex::new((0, Vec::new()))),
    };
    let mut writer: BufWriter<Box<dyn Write + Send>> =
        BufWriter::with_capacity(1024, Box::new(probe.clone()));
    writer.write_all(b"normal frame").expect("buffer frame");

    writer
        .flush()
        .expect_err("first buffered write should fail");
    discard_failed_output(writer);

    let inner = probe.inner.lock().expect("failure probe poisoned");
    assert_eq!(inner.0, 1, "failed frame bytes must not be retried");
    assert!(inner.1.is_empty(), "failed frame bytes must be discarded");
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
            condvar: Arc::new(path_std_sync::Condvar::new()),
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
        history_generation: TerminalHistoryGeneration::default(),
        history_width: 40,
        history_height: 2,
        cursor_row: 2,
        cursor_col: 8,
    };
    let plan = TerminalModel::full_redraw_plan(&layout, 10);

    let mut buffered = path_std_io::BufWriter::new(writer.clone());
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

    path_std_io::Write::flush(&mut buffered).expect("flush frame");
    assert_eq!(
        writer.write_count(),
        1,
        "small frame should write through once"
    );
    assert_eq!(writer.flush_count(), 1, "caller should flush once");
}

/// Records transaction bytes and injects failures at individual marker stages.
#[derive(Default)]
struct RecordingFailureWriter {
    /// Bytes accepted by the writer.
    bytes: Vec<u8>,
    /// Number of explicit flush calls.
    flushes: usize,
    /// Whether writing the test body fails.
    fail_body: bool,
    /// Whether writing BSU fails.
    fail_begin: bool,
    /// Whether writing ESU fails.
    fail_end: bool,
}

impl Write for RecordingFailureWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.fail_body && bytes == b"body" {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "body failed"));
        }
        if self.fail_begin && bytes == b"\x1b[?2026h" {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "BSU failed",
            ));
        }
        if self.fail_end && bytes == b"\x1b[?2026l" {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "ESU failed"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}

/// Synchronized updates must encode one exact BSU/body/ESU sequence and leave
/// flushing to the redraw-pass owner.
#[test]
fn synchronized_update_queues_exact_balanced_markers_without_flushing() {
    let mut writer = RecordingFailureWriter::default();

    with_synchronized_update(&mut writer, |writer| writer.write_all(b"body"))
        .expect("synchronized update");

    assert_eq!(writer.bytes, b"\x1b[?2026hbody\x1b[?2026l");
    assert_eq!(writer.flushes, 0);
}

/// Failure to queue BSU must return immediately without invoking the body or
/// attempting an unmatched ESU.
#[test]
fn synchronized_update_stops_after_begin_error() {
    let mut writer = RecordingFailureWriter {
        fail_begin: true,
        ..RecordingFailureWriter::default()
    };
    let body_called = path_std_cell::Cell::new(false);

    let error = with_synchronized_update(&mut writer, |writer| {
        body_called.set(true);
        writer.write_all(b"body")
    })
    .expect_err("BSU should fail");

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(!body_called.get());
    assert!(writer.bytes.is_empty());
}

/// One successful flush may establish several exact selected-presentation
/// facts, and local redraw requests must not fabricate additional remote facts.
#[test]
fn successful_flush_reports_exact_coalesced_presentation_facts() {
    let buffer = SharedBuffer::new();
    let (term, handle, _input_tx) =
        Term::new_virtual(40, 5, "> ", Box::new(buffer.clone()), CursorShape::Bar);
    handle.redraw_sync();
    let _ = buffer.drain_bytes();
    handle.with_redraw_suppressed(|| {
        handle.observe_presentation_mutation_for_test(
            renderer_delivery_id(11),
            opaque_fact("agent.prompt_queued", 0, &[]),
        );
        handle.print_output("queued", "queued");
        handle.observe_presentation_mutation_for_test(
            renderer_delivery_id(12),
            opaque_fact("agent.prompt_steered", 2, &[]),
        );
        handle.print_output("steered", "steered");
    });
    handle.redraw_sync();

    let successful = handle
        .lock()
        .presentation_observations
        .successful_test_passes
        .clone();
    assert!(
        successful
            .iter()
            .any(|pass| pass.iter().map(|fact| fact.0.get()).collect::<Vec<_>>() == [11, 12])
    );
    let reported_before_local_redraw = successful.iter().map(Vec::len).sum::<usize>();
    handle.redraw_sync();
    let reported_after_local_redraw = handle
        .lock()
        .presentation_observations
        .successful_test_passes
        .iter()
        .map(Vec::len)
        .sum::<usize>();
    assert_eq!(reported_after_local_redraw, reported_before_local_redraw);
    handle.with_redraw_suppressed(|| handle.redraw());
    handle.redraw_sync();
    {
        let mut state = handle.state.lock().expect("term state");
        state.terminal.width += 1;
    }
    handle.redraw_sync();
    let reported_after_suppression_and_resize = handle
        .lock()
        .presentation_observations
        .successful_test_passes
        .iter()
        .map(Vec::len)
        .sum::<usize>();
    assert_eq!(
        reported_after_suppression_and_resize,
        reported_before_local_redraw
    );
    drop(term);
}

/// Opaque keys outside the fixed invalidation mask must fail validation rather
/// than silently dropping or corrupting a registered fact.
#[test]
fn presentation_observation_keys_validate_mask_bounds() {
    assert!(PresentationObservationKey::new(63).is_some());
    assert!(PresentationObservationKey::new(64).is_none());
}

/// Mutation latency starts before contending for shared terminal state, so a
/// layout lock wait cannot disappear from mutation-to-flush timing.
#[test]
fn presentation_observation_timestamp_precedes_registration_lock_wait() {
    let (_term, handle, _input) =
        Term::new_virtual(80, 24, "> ", Box::new(io::sink()), CursorShape::Bar);
    let barrier = Arc::new(path_std_sync::Barrier::new(2));
    let mut guard = handle.state.lock().expect("term state");
    guard.terminal.external_paused = true;
    let observing_handle = handle.clone();
    let observing_barrier = barrier.clone();
    let observer = std::thread::spawn(move || {
        observing_barrier.wait();
        observing_handle.observe_presentation_mutation_for_test(
            renderer_delivery_id(1),
            opaque_fact("provider.response_updated", 3, &[]),
        );
    });
    barrier.wait();
    std::thread::sleep(Duration::from_millis(20));
    drop(guard);
    observer.join().expect("observer");

    let fact = handle
        .state
        .lock()
        .expect("term state")
        .presentation_observations
        .capture()
        .facts
        .pop()
        .expect("registered fact");
    assert!(fact.observed_at.elapsed() >= Duration::from_millis(10));
}

/// A mutation registered after an earlier frame was prepared belongs to the
/// next pass rather than the in-flight successful receipt.
#[test]
fn presentation_mutation_during_flush_moves_to_next_pass() {
    let state = Arc::new(Mutex::new(SharedState::new(40, 5, "> ".into())));
    state
        .lock()
        .expect("term state mutex poisoned")
        .presentation_observations
        .register(
            renderer_delivery_id(21),
            opaque_fact("provider.response_updated", 3, &[]),
            path_std_time::Instant::now(),
        );
    let mut history_cache = HistoryLayoutCache::default();
    let terminal_model = TerminalModel::default();
    let first = prepare_redraw_pass(
        &state,
        &mut history_cache,
        &terminal_model,
        40,
        5,
        &path_std_sync::Condvar::new(),
    )
    .expect("first pass");

    state
        .lock()
        .expect("term state mutex poisoned")
        .presentation_observations
        .register(
            renderer_delivery_id(22),
            opaque_fact("agent.prompt_steered", 2, &[]),
            path_std_time::Instant::now(),
        );
    trace_flushed_presentation_observations(&state, &first);
    let second = prepare_redraw_pass(
        &state,
        &mut history_cache,
        &terminal_model,
        40,
        5,
        &path_std_sync::Condvar::new(),
    )
    .expect("second pass");
    trace_flushed_presentation_observations(&state, &second);

    let exact_passes = state
        .lock()
        .expect("term state mutex poisoned")
        .presentation_observations
        .successful_test_passes
        .iter()
        .filter(|pass| !pass.is_empty())
        .map(|pass| pass.iter().map(|fact| fact.0.get()).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert_eq!(exact_passes, vec![vec![21], vec![22]]);
}

/// An invalidating fold keeps redraw capture suppressed through registration,
/// so a frame containing the successor cannot claim the removed predecessor.
#[test]
fn invalidating_observation_cannot_race_redraw_capture() {
    let state = Arc::new(Mutex::new(SharedState::new(40, 5, "> ".into())));
    {
        let mut guard = state.lock().expect("term state");
        guard.presentation_observations.register(
            renderer_delivery_id(1),
            opaque_fact("agent.prompt_queued", 0, &[]),
            path_std_time::Instant::now(),
        );
        guard.terminal.redraw_suppression = 1;
    }
    let mut history_cache = HistoryLayoutCache::default();
    assert!(
        prepare_redraw_pass(
            &state,
            &mut history_cache,
            &TerminalModel::default(),
            40,
            5,
            &path_std_sync::Condvar::new(),
        )
        .is_none()
    );
    {
        let mut guard = state.lock().expect("term state");
        guard.presentation_observations.register(
            renderer_delivery_id(2),
            opaque_fact("agent.prompt_submitted", 1, &[0]),
            path_std_time::Instant::now(),
        );
        guard.terminal.redraw_suppression = 0;
    }
    let pass = prepare_redraw_pass(
        &state,
        &mut history_cache,
        &TerminalModel::default(),
        40,
        5,
        &path_std_sync::Condvar::new(),
    )
    .expect("successor pass");
    assert_eq!(
        pass.presentation_observations
            .as_ref()
            .expect("captured observations")
            .facts
            .iter()
            .map(|fact| fact.delivery_id.get())
            .collect::<Vec<_>>(),
        [2]
    );
}

/// Pending correlation has a deterministic fixed bound and reports only an
/// omitted count; terminal classes also discard superseded ephemeral facts.
#[test]
fn presentation_observation_bound_and_supersession_are_conservative() {
    let mut observations = PresentationObservationState::new();
    for delivery_id in
        0..presentation_observation_state::MAX_PENDING_PRESENTATION_OBSERVATIONS as u64 + 3
    {
        observations.register(
            renderer_delivery_id(delivery_id),
            opaque_fact("agent.prompt_steered", 2, &[]),
            path_std_time::Instant::now(),
        );
    }
    let captured = observations.capture();
    assert_eq!(
        captured.facts.len(),
        presentation_observation_state::MAX_PENDING_PRESENTATION_OBSERVATIONS
    );
    assert_eq!(captured.omitted, 3);

    for delivery_id in
        100..100 + presentation_observation_state::MAX_PENDING_PRESENTATION_OBSERVATIONS as u64 + 3
    {
        observations.register(
            renderer_delivery_id(delivery_id),
            opaque_fact("provider.response_updated", 3, &[]),
            path_std_time::Instant::now(),
        );
    }
    observations.register(
        renderer_delivery_id(200),
        opaque_fact("provider.response_updated", 3, &[]),
        path_std_time::Instant::now(),
    );
    observations.register(
        renderer_delivery_id(201),
        opaque_fact("provider.response_finished", 4, &[3]),
        path_std_time::Instant::now(),
    );
    let captured = observations.capture();
    assert_eq!(captured.facts.len(), 1);
    assert_eq!(captured.facts[0].delivery_id.get(), 201);
    assert_eq!(captured.omitted, 0);

    for (delivery_id, fact) in [
        (300, opaque_fact("queued", 0, &[])),
        (301, opaque_fact("submitted", 1, &[0])),
        (302, opaque_fact("updated", 3, &[])),
        (303, opaque_fact("steered", 2, &[])),
        (304, opaque_fact("terminated", 5, &[0, 1, 3])),
    ] {
        observations.register(
            renderer_delivery_id(delivery_id),
            fact,
            path_std_time::Instant::now(),
        );
    }
    let captured = observations.capture();
    assert_eq!(
        captured
            .facts
            .iter()
            .map(|fact| fact.delivery_id.get())
            .collect::<Vec<_>>(),
        [303, 304]
    );
}

/// Process-local correlation changes no terminal bytes for an otherwise
/// byte-identical redraw.
#[test]
fn presentation_observation_does_not_change_vt_output() {
    fn render(observe: bool) -> Vec<u8> {
        let buffer = SharedBuffer::new();
        let (term, handle, _input_tx) =
            Term::new_virtual(40, 5, "> ", Box::new(buffer.clone()), CursorShape::Bar);
        handle.redraw_sync();
        let _ = buffer.drain_bytes();
        if observe {
            handle.observe_presentation_mutation_for_test(
                renderer_delivery_id(31),
                opaque_fact("agent.prompt_submitted", 1, &[0]),
            );
        }
        handle.print_output("same", "same");
        handle.redraw_sync();
        let bytes = buffer.drain_bytes();
        drop(term);
        bytes
    }

    assert_eq!(render(false), render(true));
}

/// Write and flush failures retain only failed-pass context and never create a
/// successful selected-presentation receipt.
#[test]
fn presentation_observations_never_succeed_on_write_or_flush_error() {
    /// Writer that accepts bytes but rejects the pass-ending flush.
    struct RejectFlush;

    impl Write for RejectFlush {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("injected flush failure"))
        }
    }

    fn prepared_observation() -> (
        Arc<Mutex<SharedState>>,
        HistoryLayoutCache,
        TerminalModel,
        RedrawPass,
    ) {
        let state = Arc::new(Mutex::new(SharedState::new(40, 5, "> ".into())));
        state
            .lock()
            .expect("term state mutex poisoned")
            .presentation_observations
            .register(
                renderer_delivery_id(41),
                opaque_fact("agent.prompt_queued", 0, &[]),
                path_std_time::Instant::now(),
            );
        let mut history_cache = HistoryLayoutCache::default();
        let terminal_model = TerminalModel::default();
        let pass = prepare_redraw_pass(
            &state,
            &mut history_cache,
            &terminal_model,
            40,
            5,
            &path_std_sync::Condvar::new(),
        )
        .expect("prepared observation");
        (state, history_cache, terminal_model, pass)
    }

    let (state, history_cache, mut terminal_model, pass) = prepared_observation();
    let error = render_redraw_pass(
        &state,
        &mut rejecting_buffer(),
        &mut Screen::new(40),
        &history_cache,
        &mut terminal_model,
        &pass,
    )
    .expect_err("write should fail");
    trace_failed_presentation_observations(&state, &pass, "write", Duration::ZERO, &error);
    assert!(
        state
            .lock()
            .expect("term state mutex poisoned")
            .presentation_observations
            .successful_test_passes
            .is_empty()
    );

    let (state, history_cache, mut terminal_model, pass) = prepared_observation();
    let mut writer: BufWriter<Box<dyn Write + Send>> = BufWriter::new(Box::new(RejectFlush));
    render_redraw_pass(
        &state,
        &mut writer,
        &mut Screen::new(40),
        &history_cache,
        &mut terminal_model,
        &pass,
    )
    .expect("frame should buffer");
    let error = writer.flush().expect_err("flush should fail");
    trace_failed_presentation_observations(&state, &pass, "flush", Duration::ZERO, &error);
    assert!(
        state
            .lock()
            .expect("term state mutex poisoned")
            .presentation_observations
            .successful_test_passes
            .is_empty()
    );
    discard_failed_output(writer);
}

/// The real redraw loop must report write and flush failures with distinct
/// stage timing while preserving attachment fail-stop.
#[test]
fn redraw_loop_records_stage_specific_presentation_failures() {
    /// Writer that rejects exactly one selected output stage.
    struct RejectStage {
        /// Output stage rejected by this writer.
        stage: &'static str,
        /// Whether a frame has reached the test's unique visible canary.
        saw_trigger: bool,
    }

    impl Write for RejectStage {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let has_trigger = bytes.contains(&b'x');
            if self.stage == "write" && has_trigger {
                Err(io::Error::other("injected write failure"))
            } else {
                self.saw_trigger |= has_trigger;
                Ok(bytes.len())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.stage == "flush" && self.saw_trigger {
                Err(io::Error::other("injected flush failure"))
            } else {
                Ok(())
            }
        }
    }

    for stage in ["write", "flush"] {
        let (term, handle, _input) = Term::new_virtual(
            40,
            5,
            "> ",
            Box::new(RejectStage {
                stage,
                saw_trigger: false,
            }),
            CursorShape::Bar,
        );
        // Drain the constructor's initial observation-free redraw before this
        // test arms a failure for the mutation it owns.
        handle.redraw_sync();
        handle.with_redraw_suppressed(|| {
            handle.observe_presentation_mutation_for_test(
                renderer_delivery_id(1),
                opaque_fact("agent.prompt_queued", 0, &[]),
            );
            handle.print_output(
                "failure-trigger",
                if stage == "write" {
                    "x".repeat(16 * 1024)
                } else {
                    "x".to_owned()
                },
            );
        });
        let error = match term.get_next_event() {
            Ok(_) => panic!("redraw output must fail"),
            Err(error) => error,
        };
        assert!(is_output_failure(&error));
        let records = handle
            .state
            .lock()
            .expect("term state")
            .presentation_failure_test_records
            .clone();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, stage);
        assert_eq!(records[0].2, 1);
        assert_eq!(records[0].3, 0);
        assert!(
            handle
                .state
                .lock()
                .expect("term state")
                .presentation_observations
                .successful_test_passes
                .iter()
                .all(Vec::is_empty)
        );
    }
}

/// A recoverable body-write failure must still attempt ESU so a terminal does
/// not remain synchronized until its implementation-specific timeout.
#[test]
fn synchronized_update_attempts_end_after_body_error() {
    let mut writer = RecordingFailureWriter {
        fail_body: true,
        ..RecordingFailureWriter::default()
    };

    let error = with_synchronized_update(&mut writer, |writer| writer.write_all(b"body"))
        .expect_err("body should fail");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(writer.bytes, b"\x1b[?2026h\x1b[?2026l");
}

/// The body error must win when both the body and closing marker fail; when
/// only ESU fails, its error must be returned to the caller.
#[test]
fn synchronized_update_preserves_error_precedence() {
    let mut both_fail = RecordingFailureWriter {
        fail_body: true,
        fail_end: true,
        ..RecordingFailureWriter::default()
    };
    let body_error = with_synchronized_update(&mut both_fail, |writer| writer.write_all(b"body"))
        .expect_err("body and ESU should fail");
    assert_eq!(body_error.kind(), io::ErrorKind::InvalidData);

    let mut end_fails = RecordingFailureWriter {
        fail_end: true,
        ..RecordingFailureWriter::default()
    };
    let end_error = with_synchronized_update(&mut end_fails, |writer| writer.write_all(b"body"))
        .expect_err("ESU should fail");
    assert_eq!(end_error.kind(), io::ErrorKind::BrokenPipe);
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

/// Resize sampling must prefer the reported dimension, then a fresh actual
/// dimension, and retain zero only when both are zero.
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

/// Moving live blocks before active anchors must insert, reposition, and append
/// them without rebuilding the active zone.
#[test]
fn push_above_active_before_any_inserts_moves_and_appends() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(8, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 8, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    let top = handle.new_block("test", plain_block("top live"));
    let middle = handle.new_block("test", plain_block("middle live"));
    let bottom = handle.new_block("test", plain_block("bottom live"));
    let appended = handle.new_block("test", plain_block("appended live"));
    handle.push_above_active(top);
    handle.push_above_active(bottom);
    handle.push_above_active_before_any(middle, [bottom]);
    flush_redraws(&handle, &buf, &mut parser);
    assert_terminal_rows_match(
        &mut parser,
        40,
        8,
        &["top live", "middle live", "bottom live", "> "],
    );

    handle.push_above_active_before_any(middle, [top]);
    flush_redraws(&handle, &buf, &mut parser);
    assert_terminal_rows_match(
        &mut parser,
        40,
        8,
        &["middle live", "top live", "bottom live", "> "],
    );

    handle.push_above_active_before_any(appended, std::iter::empty::<BlockId>());
    flush_redraws(&handle, &buf, &mut parser);
    assert_terminal_rows_match(
        &mut parser,
        40,
        8,
        &[
            "middle live",
            "top live",
            "bottom live",
            "appended live",
            "> ",
        ],
    );
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

/// Lowercase and shifted uppercase control-letter bindings remain distinct so
/// applications can assign C-b and C-B different actions.
#[test]
fn control_letter_bindings_distinguish_shift() {
    let buf = SharedBuffer::new();
    let (mut term, _handle, input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    term.set_bindings(vec![
        ("C-b".to_owned(), "lower".to_owned()),
        ("C-B".to_owned(), "upper".to_owned()),
    ]);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
        )))
        .expect("send C-b");
    assert!(matches!(
        term.get_next_event().expect("C-b event"),
        Event::Binding(action) if action == "lower"
    ));

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('B'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )))
        .expect("send C-B");
    assert!(matches!(
        term.get_next_event().expect("C-B event"),
        Event::Binding(action) if action == "upper"
    ));

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )))
        .expect("send lowercase shifted C-B");
    assert!(matches!(
        term.get_next_event().expect("lowercase shifted C-B event"),
        Event::Binding(action) if action == "upper"
    ));

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('B'),
            KeyModifiers::CONTROL,
        )))
        .expect("send uppercase C-B without explicit shift");
    assert!(matches!(
        term.get_next_event().expect("uppercase C-B event"),
        Event::Binding(action) if action == "upper"
    ));

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('\u{2}'),
            KeyModifiers::NONE,
        )))
        .expect("send legacy Ctrl-B byte");
    assert!(matches!(
        term.get_next_event().expect("legacy C-b event"),
        Event::Binding(action) if action == "lower"
    ));
}

/// Meta character bindings match exact Alt-only events, including the
/// Crossterm event produced by the legacy `ESC a` terminal encoding, without
/// stealing plain text or modifier combinations.
#[test]
fn meta_character_binding_matches_alt_only() {
    let buf = SharedBuffer::new();
    let (mut term, handle, input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    term.set_bindings(vec![
        ("M-a".to_owned(), "all".to_owned()),
        ("M-z".to_owned(), "second-meta".to_owned()),
        ("m-q".to_owned(), "invalid-lowercase-prefix".to_owned()),
        ("C-a".to_owned(), "control".to_owned()),
    ]);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::ALT,
        )))
        .expect("send M-a");
    assert!(matches!(
        term.get_next_event().expect("M-a event"),
        Event::Binding(action) if action == "all"
    ));

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::ALT,
        )))
        .expect("send M-z");
    assert!(matches!(
        term.get_next_event().expect("M-z event"),
        Event::Binding(action) if action == "second-meta"
    ));

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        )))
        .expect("send plain a");
    assert!(matches!(
        term.get_next_event().expect("plain a event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "a");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        )))
        .expect("send M-S-a");
    assert!(matches!(
        term.get_next_event().expect("M-S-a event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "aa");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::ALT,
        )))
        .expect("send Alt-q for invalid m-q spelling");
    assert!(matches!(
        term.get_next_event().expect("unbound Alt-q event"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "aaq");

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::ALT | KeyModifiers::CONTROL,
        )))
        .expect("send M-C-a");
    assert!(matches!(
        term.get_next_event().expect("M-C-a event"),
        Event::Binding(action) if action == "control"
    ));
}

/// Shifted punctuation remains a plain control binding because uppercase
/// binding syntax distinguishes shifted letters only.
#[test]
fn control_punctuation_binding_ignores_shift_modifier() {
    let buf = SharedBuffer::new();
    let (mut term, _handle, input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    term.set_bindings(vec![("C-!".to_owned(), "punctuation".to_owned())]);

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Char('!'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )))
        .expect("send Ctrl+Shift+!");
    assert!(matches!(
        term.get_next_event().expect("punctuation event"),
        Event::Binding(action) if action == "punctuation"
    ));
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
        if buffer == ":" {
            vec![
                Candidate {
                    label: ":model".to_owned(),
                    description: "switch model".to_owned(),
                    replacement: ":model".to_owned(),
                    cursor: ":model".len(),
                },
                Candidate {
                    label: ":quit".to_owned(),
                    description: "exit".to_owned(),
                    replacement: ":quit".to_owned(),
                    cursor: ":quit".len(),
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
            KeyCode::Char(':'),
            KeyModifiers::NONE,
        )))
        .expect("send command prefix");
    assert!(matches!(
        term.get_next_event().expect("open completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), ":");

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
    assert_eq!(handle.get_buffer(), ":quit");

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
    assert_eq!(handle.get_buffer(), ":quit");
}

/// Enter acceptance keeps a whole-buffer candidate's explicit UTF-8 byte
/// cursor.
#[test]
fn completion_accept_preserves_suffix_and_explicit_cursor() {
    let buf = SharedBuffer::new();
    let (term, handle, input_tx) = Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
    handle.set_buffer("日 tail".to_owned(), "日".len());
    {
        let mut st = handle.lock();
        st.editor.completion = Some(CompletionMenu {
            candidates: vec![Candidate {
                label: "日本".to_owned(),
                description: "candidate".to_owned(),
                replacement: "日本 tail".to_owned(),
                cursor: "日本".len(),
            }],
            selected: None,
            original_buffer: st.editor.buffer.clone(),
            original_cursor: st.editor.cursor,
        });
    }

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("cycle completion");
    assert!(matches!(
        term.get_next_event().expect("preview completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "日本 tail");
    assert_eq!(handle.get_cursor(), "日本".len());

    input_tx
        .send(RawEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("accept completion");
    assert!(matches!(
        term.get_next_event().expect("accept completion"),
        Event::CompletionAccept
    ));
    assert_eq!(handle.get_buffer(), "日本 tail");
    assert_eq!(handle.get_cursor(), "日本".len());
}

/// Rejects malformed completion cursor metadata before preview or later edits.
#[test]
fn completion_rejects_invalid_cursor_metadata() {
    for (replacement, invalid_cursor) in [("e\u{301}", 1), ("é", 99)] {
        let buf = SharedBuffer::new();
        let (mut term, handle, input_tx) =
            Term::new_virtual(80, 24, "> ", Box::new(buf), CursorShape::Bar);
        term.set_completion_source(Some(Box::new(move |_: &str, _: usize| {
            vec![Candidate {
                label: "invalid".to_owned(),
                description: "candidate".to_owned(),
                replacement: replacement.to_owned(),
                cursor: invalid_cursor,
            }]
        })));

        input_tx
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )))
            .expect("type initial text");
        assert!(matches!(
            term.get_next_event().expect("initial edit"),
            Event::BufferChanged
        ));
        assert!(handle.lock().editor.completion.is_none());

        input_tx
            .send(RawEvent::Key(KeyEvent::new(
                KeyCode::Char('y'),
                KeyModifiers::NONE,
            )))
            .expect("edit after malformed candidate");
        assert!(matches!(
            term.get_next_event().expect("safe subsequent edit"),
            Event::BufferChanged
        ));
        assert_eq!(handle.get_buffer(), "xy");
    }
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
                cursor: "./crates/".len(),
            }],
            "./crates/" => vec![Candidate {
                label: "./crates/tau-cli-term-raw/".to_owned(),
                description: "directory".to_owned(),
                replacement: "./crates/tau-cli-term-raw/".to_owned(),
                cursor: "./crates/tau-cli-term-raw/".len(),
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

/// Appends one ordinary persistent output block through the same state mutation
/// used by `TermHandle::print_output`.
fn append_history_for_cache_test(st: &mut SharedState, text: String) {
    let id = st.alloc_id();
    st.layout.blocks.insert(id, plain_block(text));
    st.layout
        .block_debug_ids
        .insert(id, "cache-test".to_owned());
    st.append_history(id);
}

/// Adds a specified history entry so removal tests can place duplicate
/// references without changing the block-store identity.
fn append_history_id_for_cache_test(st: &mut SharedState, id: BlockId, text: String) {
    st.layout.blocks.insert(id, plain_block(text));
    st.layout
        .block_debug_ids
        .insert(id, "cache-removal-test".to_owned());
    st.append_history(id);
}

/// Recomputes the history membership index independently of the implementation.
fn history_reference_counts(history: &[BlockId]) -> HashMap<BlockId, usize> {
    let mut counts = HashMap::new();
    for &id in history {
        *counts.entry(id).or_insert(0) += 1;
    }
    counts
}

/// A new prompt on a long transcript must lay out only its appended history
/// entry, preventing redraw CPU from growing with session length.
#[test]
fn history_layout_cache_refreshes_only_appended_suffix() {
    let mut st = SharedState::new(80, 24, "> ".into());
    for index in 0..10_000 {
        append_history_for_cache_test(&mut st, format!("history {index}"));
    }
    let mut cache = HistoryLayoutCache::default();
    assert_eq!(cache.refresh(&mut st), 10_000);
    assert_eq!(cache.lines.len(), 10_000);

    append_history_for_cache_test(&mut st, "submitted prompt".to_owned());

    assert_eq!(
        cache.refresh(&mut st),
        1,
        "append refresh must not revisit the old transcript"
    );
    assert_eq!(cache.lines.len(), 10_001);
    assert_eq!(
        line_text(cache.lines.last().expect("appended line")).trim_end(),
        "submitted prompt"
    );
}

/// Queued active-block removal must skip a long persistent-history scan when
/// its exact membership index proves the active block is absent from history.
#[test]
fn queued_active_removal_skips_long_history_when_membership_is_absent() {
    let mut st = SharedState::new(80, 24, "> ".into());
    for index in 0..10_000 {
        append_history_for_cache_test(&mut st, format!("history {index}"));
    }
    let mut cache = HistoryLayoutCache::default();
    assert_eq!(cache.refresh(&mut st), 10_000);

    let active = st.alloc_id();
    st.layout.blocks.insert(active, plain_block("queued"));
    st.layout
        .block_debug_ids
        .insert(active, "queued-active".to_owned());
    st.layout.above_active.push(active);
    st.layout.history_removal_scan_entries = 0;

    assert!(st.remove_block(active, true));
    assert_eq!(
        st.layout.history_removal_scan_entries, 0,
        "history membership must prevent scanning an unrelated long transcript"
    );
    assert_eq!(
        cache.refresh(&mut st),
        0,
        "active removal must not dirty history"
    );
    assert_eq!(st.layout.history.len(), 10_000);
    assert_eq!(st.layout.history_refs.len(), 10_000);
}

/// History removal must scan exactly once to remove duplicate references, then
/// re-layout only from the first removed entry for every long-history
/// placement.
#[test]
fn history_removal_uses_first_changed_suffix_for_all_placements_and_duplicates() {
    const HISTORY_LEN: usize = 10_000;
    let cases = [
        ("early", vec![3]),
        ("middle", vec![HISTORY_LEN / 2]),
        ("late", vec![HISTORY_LEN - 4]),
        ("duplicates", vec![3, HISTORY_LEN / 2, HISTORY_LEN - 4]),
    ];

    for (name, positions) in cases {
        let mut st = SharedState::new(80, 24, "> ".into());
        let target = BlockId(u64::MAX);
        for index in 0..HISTORY_LEN {
            let id = if positions.contains(&index) {
                target
            } else {
                BlockId(index as u64)
            };
            append_history_id_for_cache_test(&mut st, id, format!("history {index}"));
        }
        let mut cache = HistoryLayoutCache::default();
        assert_eq!(cache.refresh(&mut st), HISTORY_LEN, "{name}");
        st.layout.history_removal_scan_entries = 0;

        assert!(st.remove_block(target, true), "{name}");
        let first_removed = positions[0];
        let expected_history: Vec<_> = (0..HISTORY_LEN)
            .filter(|index| !positions.contains(index))
            .map(|index| BlockId(index as u64))
            .collect();

        assert_eq!(
            st.layout.history_removal_scan_entries, HISTORY_LEN,
            "{name}: one complete scan removes every duplicate reference"
        );
        assert_eq!(st.layout.history, expected_history, "{name}");
        assert_eq!(
            st.layout.history_refs,
            history_reference_counts(&st.layout.history),
            "{name}: membership index remains exact"
        );
        assert_eq!(st.layout.history_dirty_from, Some(first_removed), "{name}");
        assert_eq!(
            cache.refresh(&mut st),
            st.layout.history.len() - first_removed,
            "{name}: cache must re-layout only the changed suffix"
        );
        assert_eq!(
            cache.entry_line_offsets.len(),
            st.layout.history.len() + 1,
            "{name}: cache offsets must match the compacted history"
        );
    }
}

/// The optimized removal must preserve the reference model's blocks, ordered
/// zones, duplicate-history membership, and presentation delta across a
/// deterministic mixed mutation sequence.
#[test]
fn randomized_block_removal_matches_reference_model() {
    let mut st = SharedState::new(80, 24, "> ".into());
    let mut reference = OutputSnapshot::default();
    let mut seed = 0x8f3b_7a51_u64;

    for step in 0..2_000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let operation = seed % 7;
        let mut existing_ids: Vec<_> = reference.blocks.keys().copied().collect();
        existing_ids.sort_unstable_by_key(|id| id.0);

        match operation {
            0 => {
                let text = format!("block {step}");
                let id = st.alloc_id();
                st.layout.blocks.insert(id, plain_block(text.clone()));
                st.layout
                    .block_debug_ids
                    .insert(id, format!("block-{step}"));
                let reference_id = reference.new_block(format!("block-{step}"), plain_block(text));
                assert_eq!(id, reference_id);
            }
            1 if !existing_ids.is_empty() => {
                let id = existing_ids[(seed as usize) % existing_ids.len()];
                st.append_history(id);
                reference.push_history(id);
            }
            2 if !existing_ids.is_empty() => {
                let id = existing_ids[(seed as usize) % existing_ids.len()];
                if !st.layout.above_active.contains(&id) {
                    st.layout.above_active.push(id);
                }
                reference.push_above_active(id);
            }
            3 if !existing_ids.is_empty() => {
                let id = existing_ids[(seed as usize) % existing_ids.len()];
                if !st.layout.above_sticky.contains(&id) {
                    st.layout.above_sticky.push(id);
                }
                reference.push_above_sticky(id);
            }
            4 if !existing_ids.is_empty() => {
                let id = existing_ids[(seed as usize) % existing_ids.len()];
                if !st.layout.suggestions.contains(&id) {
                    st.layout.suggestions.push(id);
                }
                if !reference.suggestions.contains(&id) {
                    reference.suggestions.push(id);
                }
            }
            5 if !existing_ids.is_empty() => {
                let id = existing_ids[(seed as usize) % existing_ids.len()];
                if !st.layout.below.contains(&id) {
                    st.layout.below.push(id);
                }
                reference.push_below(id);
            }
            _ => {
                let id = if existing_ids.is_empty() || seed & 1 == 0 {
                    BlockId(st.layout.next_id.saturating_add(1))
                } else {
                    existing_ids[(seed as usize) % existing_ids.len()]
                };
                let expected_delta = reference.history.contains(&id)
                    || reference.above_active.contains(&id)
                    || reference.above_sticky.contains(&id)
                    || reference.suggestions.contains(&id)
                    || reference.below.contains(&id);
                reference.remove_block(id);
                assert_eq!(st.remove_block(id, true), expected_delta, "step {step}");
            }
        }

        assert_eq!(st.layout.blocks, reference.blocks, "step {step}: blocks");
        assert_eq!(
            st.layout.block_debug_ids, reference.block_debug_ids,
            "step {step}: block diagnostics"
        );
        assert_eq!(
            st.layout.next_id, reference.next_id,
            "step {step}: identities"
        );
        assert_eq!(st.layout.history, reference.history, "step {step}: history");
        assert_eq!(
            st.layout.history_refs,
            history_reference_counts(&reference.history),
            "step {step}: history membership"
        );
        assert_eq!(
            st.layout.above_active, reference.above_active,
            "step {step}: active zone"
        );
        assert_eq!(
            st.layout.above_sticky, reference.above_sticky,
            "step {step}: sticky zone"
        );
        assert_eq!(
            st.layout.suggestions, reference.suggestions,
            "step {step}: suggestions"
        );
        assert_eq!(st.layout.below, reference.below, "step {step}: below zone");
    }
}

/// This manual benchmark reports scan and suffix-layout work at increasing
/// history sizes without treating elapsed time as a correctness threshold.
#[test]
#[ignore = "manual queued active-block removal scaling benchmark"]
fn benchmark_queued_active_removal_history_membership_scaling() {
    for history_len in [1_000, 10_000, 100_000] {
        let mut st = SharedState::new(80, 24, "> ".into());
        for index in 0..history_len {
            append_history_for_cache_test(&mut st, format!("history {index}"));
        }
        let mut cache = HistoryLayoutCache::default();
        cache.refresh(&mut st);
        let active = st.alloc_id();
        st.layout.blocks.insert(active, plain_block("queued"));
        st.layout.above_active.push(active);
        st.layout.history_removal_scan_entries = 0;
        let started = path_std_time::Instant::now();

        assert!(st.remove_block(active, true));
        let cache_entries_relaid = cache.refresh(&mut st);
        eprintln!(
            "queued active removal benchmark: history_entries={history_len} history_scan_entries={} cache_entries_relaid={cache_entries_relaid} previous_history_scan_entries={history_len} elapsed={:?}; no timing threshold",
            st.layout.history_removal_scan_entries,
            started.elapsed()
        );
    }
}

/// Removing a queued active block above a scrolled transcript must leave hidden
/// history untouched, preserve the visible tail, and stay on the scrolling-safe
/// incremental rendering path.
#[test]
fn queued_active_removal_preserves_hidden_history_visible_tail_and_scrollback() {
    let buf = SharedBuffer::new();
    let mut parser = vt100::Parser::new(4, 40, 50);
    let (_term, handle, _input_tx) =
        Term::new_virtual(40, 4, "> ", Box::new(buf.clone()), CursorShape::Bar);
    flush_redraws(&handle, &buf, &mut parser);

    for index in 0..8 {
        handle.print_output("history", plain_block(format!("history {index}")));
    }
    let active = handle.new_block("queued", plain_block("queued prompt"));
    handle.push_above_active(active);
    flush_redraws(&handle, &buf, &mut parser);
    handle.lock().layout.history_removal_scan_entries = 0;

    assert_no_full_redraw_after(&handle, &buf, &mut parser, || handle.remove_block(active));
    let visible = visible_rows(&parser);
    assert_eq!(
        visible.iter().map(|row| row.trim_end()).collect::<Vec<_>>(),
        ["history 6", "history 7", "", ">"],
        "rubber keeps the visible tail stable while the active row disappears"
    );
    parser.screen_mut().set_scrollback(6);
    let scrollback = visible_rows(&parser);
    assert_eq!(
        scrollback[..4]
            .iter()
            .map(|row| row.trim_end())
            .collect::<Vec<_>>(),
        ["history 0", "history 1", "history 2", "history 3"],
        "the incremental frame must preserve already-scrolled history"
    );
    parser.screen_mut().set_scrollback(0);
    assert_eq!(
        handle.lock().layout.history_removal_scan_entries,
        0,
        "queued active removal must not revisit hidden history"
    );
}

/// Appending a prompt after the viewport has overflowed must use the
/// suffix-only scrolling frame rather than materializing a full-transcript
/// render plan.
#[test]
fn long_history_append_selects_fast_scrolling_frame() {
    let mut st = SharedState::new(80, 24, "> ".into());
    for index in 0..10_000 {
        append_history_for_cache_test(&mut st, format!("history {index}"));
    }
    let mut cache = HistoryLayoutCache::default();
    cache.refresh(&mut st);
    let tail = layout_tail(&st, cache.lines.len());
    let layout = layout_all_from_cached_history(&cache, tail);
    let mut model = TerminalModel::default();
    let plan = model.plan_view(&layout, st.terminal.height);
    model.reset_to_layout(&layout, plan.viewport_start, plan.rubber_height);
    let width = st.terminal.width;
    let height = st.terminal.height;

    append_history_for_cache_test(&mut st, "submitted prompt".to_owned());
    let state = Arc::new(Mutex::new(st));
    let pass = prepare_redraw_pass(
        &state,
        &mut cache,
        &model,
        width,
        height,
        &path_std_sync::Condvar::new(),
    )
    .expect("append should produce a redraw pass");

    let RenderFrame::Fast { tail, metrics } = &pass.frame else {
        panic!("append should avoid the full-transcript frame");
    };
    let suffix = scrolling_suffix(&cache.lines, tail, metrics, &model);
    assert_eq!(
        suffix.len(),
        height + 1,
        "one-row append should build only the old viewport plus its new row"
    );
}

/// Finalizing a mutable active block into history can rewrite rows at the
/// history/active boundary, so it must retain hidden-prefix validation.
#[test]
fn history_append_replacing_active_rows_selects_full_frame() {
    let mut st = SharedState::new(80, 24, "> ".into());
    for index in 0..100 {
        append_history_for_cache_test(&mut st, format!("history {index}"));
    }
    let active_id = st.alloc_id();
    st.layout
        .blocks
        .insert(active_id, plain_block("partial 0\npartial 1"));
    st.layout
        .block_debug_ids
        .insert(active_id, "active-cache-test".to_owned());
    st.layout.above_active.push(active_id);

    let mut cache = HistoryLayoutCache::default();
    cache.refresh(&mut st);
    let tail = layout_tail(&st, cache.lines.len());
    let layout = layout_all_from_cached_history(&cache, tail);
    let mut model = TerminalModel::default();
    let plan = model.plan_view(&layout, st.terminal.height);
    model.reset_to_layout(&layout, plan.viewport_start, plan.rubber_height);
    let width = st.terminal.width;
    let height = st.terminal.height;

    st.layout.above_active.clear();
    st.layout.blocks.remove(&active_id);
    st.layout.block_debug_ids.remove(&active_id);
    append_history_for_cache_test(&mut st, "final 0\nfinal 1".to_owned());
    let state = Arc::new(Mutex::new(st));
    let pass = prepare_redraw_pass(
        &state,
        &mut cache,
        &model,
        width,
        height,
        &path_std_sync::Condvar::new(),
    )
    .expect("finalization should produce a redraw pass");

    assert!(
        matches!(pass.frame, RenderFrame::Full { .. }),
        "active replacement must validate the complete hidden prefix"
    );
}

/// Protects the external-program pause invariant: if releasing the terminal
/// fails after redraws are paused, production rollback via
/// `resume_after_external` must unmute redraws and invalidate the next frame.
#[test]
fn external_pause_failure_rolls_back_through_resume() {
    let (term, _handle, _input_tx) =
        Term::new_virtual(80, 24, "> ", Box::new(std::io::sink()), CursorShape::Bar);
    let _redraw_guard = RedrawSuppressionGuard::new(&term.handle);
    let release_called = path_std_cell::Cell::new(false);

    let error = term
        .pause_for_external_with_release(|| {
            release_called.set(true);
            Err(io::Error::other("simulated release failure"))
        })
        .expect_err("release failure should be returned");

    assert!(release_called.get());
    assert_eq!(error.to_string(), "simulated release failure");
    let st = term.handle.lock();
    assert!(!st.terminal.external_paused);
    assert!(st.terminal.invalidate_screen);
}

/// The global warning policy should admit the first warning, reject one just
/// before the interval, and admit one exactly at the interval boundary.
#[test]
fn stall_warning_limiter_rate_limits_deterministically() {
    let start = path_std_time::Instant::now();
    let mut limiter = StallWarningLimiter { last: None };

    assert!(limiter.admit(start));
    assert!(!limiter.admit(start + Duration::from_millis(4_999)));
    assert!(limiter.admit(start + Duration::from_secs(5)));
}
