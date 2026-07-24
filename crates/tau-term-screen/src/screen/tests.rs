use super::*;
use crate::style::{Color, Span, display_width};

// Security and cache regressions here cover the safeguards summarized by
// `ARCH-tau-term-screen`.

/// Test harness: pairs our `Screen` with a `vt100::Parser` acting as
/// a headless terminal emulator. We feed our escape-sequence output
/// into vt100 and assert on the resulting screen state.
struct TestTerm {
    screen: Screen,
    term: vt100::Parser,
}

impl TestTerm {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            screen: Screen::new(cols as usize),
            term: vt100::Parser::new(rows, cols, 0),
        }
    }

    /// Builds desired layout from plain content, feeds the diff output
    /// into the terminal emulator.
    fn render(&mut self, content: &str, cursor_char_offset: usize) {
        let width = self.screen.width();
        let styled: StyledText = content.into();
        let desired = layout_lines().content(&styled).width(width).call();
        let cursor = (cursor_char_offset / width, cursor_char_offset % width);
        let mut buf = Vec::new();
        self.screen
            .update(&mut buf, &desired, cursor)
            .expect("update should succeed");
        self.term.process(&buf);
    }

    /// Invalidates the screen (as async output would) and re-renders.
    fn invalidate_and_render(&mut self, content: &str, cursor_char_offset: usize) {
        let mut buf = Vec::new();
        self.screen
            .erase_all(&mut buf)
            .expect("erase should succeed");
        self.screen.invalidate();
        self.term.process(&buf);
        self.render(content, cursor_char_offset);
    }

    /// Returns the text on a given terminal row (trimmed of trailing
    /// whitespace).
    fn row_text(&self, row: usize) -> String {
        self.term
            .screen()
            .rows(0, self.term.screen().size().1)
            .nth(row)
            .unwrap_or_default()
    }

    /// Returns the cursor position as (row, col).
    fn cursor(&self) -> (u16, u16) {
        self.term.screen().cursor_position()
    }
}

/// Extracts character-only strings from cell lines (for assertions).
fn line_chars(lines: &[Vec<Cell>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| line.iter().map(|c| c.ch).collect())
        .collect()
}

fn plain_cell_lines(lines: &[&str]) -> Vec<Vec<Cell>> {
    lines
        .iter()
        .map(|line| line.chars().map(Cell::plain).collect())
        .collect()
}

// --- layout tests ---

/// Defines the layout base case: even empty content owns one addressable row so
/// cursor code never has to handle a zero-line prompt.
#[test]
fn layout_empty_produces_one_empty_line() {
    let lines = layout_lines().content(&StyledText::new()).width(80).call();
    assert_eq!(line_chars(&lines), vec![""]);
}

/// Documents the non-wrapping baseline for short plain text before exercising
/// the edge cases below.
#[test]
fn layout_short_produces_one_line() {
    let lines = layout_lines()
        .content(&StyledText::from("abc"))
        .width(80)
        .call();
    assert_eq!(line_chars(&lines), vec!["abc"]);
}

/// Protects the cell-width wrapping contract used by prompt and block renderers
/// to map logical content onto terminal rows.
#[test]
fn layout_wraps_at_width() {
    let lines = layout_lines()
        .content(&StyledText::from("abcde"))
        .width(3)
        .call();
    assert_eq!(line_chars(&lines), vec!["abc", "de"]);
}

/// Exact-width generic content must stop at the current row; continuation rows
/// are only added by prompt cursor policy when needed.
#[test]
fn layout_exact_width_is_one_line() {
    let lines = layout_lines()
        .content(&StyledText::from("abc"))
        .width(3)
        .call();
    assert_eq!(line_chars(&lines), vec!["abc"]);
}

/// Emoji presentation sequences such as `⚠️` occupy two terminal columns even
/// though the base warning-sign scalar has width one. The renderer must account
/// for that when padding block rows, otherwise exact-width rows enter pending
/// wrap and later differential updates repaint at the wrong physical row.
#[test]
fn layout_counts_emoji_variation_selector_as_wide() {
    let lines = layout_lines()
        .content(&StyledText::from("- ⚠️  …"))
        .width(7)
        .call();
    assert_eq!(line_chars(&lines), vec!["- ⚠️  …"]);
    assert_eq!(cols(&lines[0]), 7);
}

/// Complex emoji sequences must be measured as grapheme clusters. Counting each
/// Unicode scalar separately would wrap these examples into extra rows.
#[test]
fn layout_counts_emoji_grapheme_sequences_as_clusters() {
    for emoji in ["👩‍💻", "👍🏽"] {
        let lines = layout_lines()
            .content(&StyledText::from(emoji))
            .width(2)
            .call();
        assert_eq!(line_chars(&lines), vec![emoji]);
        assert_eq!(cols(&lines[0]), 2);
    }
}

/// Too-wide graphemes cannot be emitted into a narrower terminal row without
/// forcing physical autowrap outside the renderer model, so they are replaced
/// with a safe visible placeholder.
#[test]
fn layout_replaces_graphemes_wider_than_row() {
    let lines = layout_lines()
        .content(&StyledText::from("🙂a"))
        .width(1)
        .call();

    assert_eq!(line_chars(&lines), vec!["�", "a"]);
    for line in &lines {
        assert!(cols(line) <= 1);
    }

    let lines = layout_lines()
        .content(&StyledText::from("a🙂"))
        .width(1)
        .call();
    assert_eq!(line_chars(&lines), vec!["a", "�"]);
    for line in &lines {
        assert!(cols(line) <= 1);
    }
}

/// CRLF and bare carriage returns must be treated as line separators before
/// cell emission; otherwise raw control characters would be printed inside a
/// modeled row.
#[test]
fn layout_treats_crlf_as_newline() {
    let crlf_lines = layout_lines()
        .content(&StyledText::from("a\r\nb"))
        .width(80)
        .call();
    assert_eq!(line_chars(&crlf_lines), vec!["a", "b"]);

    let cr_lines = layout_lines()
        .content(&StyledText::from("a\rb"))
        .width(80)
        .call();
    assert_eq!(line_chars(&cr_lines), vec!["a", "b"]);
}

/// Grapheme segmentation has to cross span boundaries. Styled renderers often
/// split text by syntax/style, and an emoji variation sequence may straddle
/// that split.
#[test]
fn layout_counts_split_span_emoji_as_one_grapheme() {
    let styled = StyledText::from(vec![Span::plain("⚠"), Span::plain("️")]);
    let lines = layout_lines().content(&styled).width(2).call();
    assert_eq!(line_chars(&lines), vec!["⚠️"]);
    assert_eq!(cols(&lines[0]), 2);
    assert_eq!(styled.char_count(), 2);
}

/// Guards the style-to-cell conversion path so styled spans keep their
/// attributes after layout.
#[test]
fn layout_preserves_styles() {
    let style = Style::default().fg(Color::Red);
    let styled = StyledText::from(vec![Span::plain("ab"), Span::new("cd", style)]);
    let lines = layout_lines().content(&styled).width(80).call();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].len(), 4);
    assert_eq!(lines[0][0], Cell::plain('a'));
    assert_eq!(lines[0][1], Cell::plain('b'));
    assert_eq!(lines[0][2], Cell::new('c', style));
    assert_eq!(lines[0][3], Cell::new('d', style));
}

/// Explicit newlines must split visual rows even when no wrapping is needed.
#[test]
fn layout_handles_newlines() {
    let lines = layout_lines()
        .content(&StyledText::from("abc\ndef"))
        .width(80)
        .call();
    assert_eq!(line_chars(&lines), vec!["abc", "def"]);
}

/// Covers the boundary where an explicit newline occurs at the wrap width,
/// avoiding either merged rows or phantom blank rows.
#[test]
fn layout_newline_and_wrap() {
    let lines = layout_lines()
        .content(&StyledText::from("abc\ndef"))
        .width(3)
        .call();
    assert_eq!(line_chars(&lines), vec!["abc", "def"]);
}

/// Non-newline control characters must be sanitized before cell emission so
/// untrusted styled text cannot inject terminal controls such as CSI clears.
#[test]
fn layout_sanitizes_non_newline_control_characters() {
    let lines = layout_lines()
        .content(&StyledText::from("a\t\x1b[2J\x08\u{85}b"))
        .width(80)
        .call();

    assert_eq!(line_chars(&lines), vec!["a �[2J��b"]);
    assert_eq!(display_width("\t\x1b\x08\u{85}"), 4);
    assert_eq!(StyledText::from("\t\x1b\x08\u{85}").char_count(), 4);
    let mut buf = Vec::new();
    emit_styled_cells(&mut buf, &lines[0]).expect("cell emission should succeed");
    assert!(
        !buf.contains(&0x09),
        "sanitized content must not emit raw tab: {buf:?}"
    );
    assert!(
        !buf.contains(&0x1b),
        "sanitized content must not emit raw ESC: {buf:?}"
    );
}

/// OSC 8 emission opens immediately before linked cells, closes immediately
/// after them, and rejects control-bearing targets.
#[test]
fn hyperlink_emission_has_exact_safe_boundaries() {
    let text = StyledText::from(vec![
        Span::plain("before "),
        Span::plain("link").hyperlink("https://example.test/路"),
        Span::plain(" after"),
    ]);
    let cells = text.to_cells();
    let mut buf = Vec::new();
    emit_styled_cells(&mut buf, &cells).expect("hyperlink cells should emit");
    assert_eq!(
        String::from_utf8(buf).expect("terminal output is UTF-8"),
        "before \u{1b}]8;;https://example.test/路\u{1b}\\link\u{1b}]8;;\u{1b}\\ after"
    );

    let unsafe_text = StyledText::from(Span::plain("safe").hyperlink("https://bad.test/\u{1b}]8"));
    assert!(unsafe_text.spans()[0].hyperlink.is_none());

    let unsafe_cells = [Cell {
        ch: 'x',
        style: Style::default(),
        width: 1,
        hyperlink: Some(std::sync::Arc::from("https://bad.test/\u{1b}]8")),
    }];
    let mut unsafe_buf = Vec::new();
    emit_styled_cells(&mut unsafe_buf, &unsafe_cells).expect("unsafe target falls back safely");
    assert_eq!(unsafe_buf, b"x");
}

/// The shared one-shot OSC 8 writer sanitizes provider-facing link labels
/// independently of structured styled-text layout.
#[test]
fn osc8_writer_sanitizes_control_bearing_labels() {
    let mut output = Vec::new();
    write_osc8_hyperlink(
        &mut output,
        "open\u{1b}]8;;evil\u{7}label",
        "https://example.test",
    )
    .expect("safe link target should emit");

    assert_eq!(
        String::from_utf8(output).expect("writer output is UTF-8"),
        "\u{1b}]8;;https://example.test\u{1b}\\open�]8;;evil�label\u{1b}]8;;\u{1b}\\"
    );
}

/// Bounding targets prevents narrow wrapping from repeating an unbounded target
/// once per physical row.
#[test]
fn hyperlink_targets_have_a_bounded_emission_size() {
    let maximum = "x".repeat(crate::style::MAX_HYPERLINK_TARGET_BYTES);
    let oversized = "x".repeat(crate::style::MAX_HYPERLINK_TARGET_BYTES + 1);

    assert!(Span::plain("ok").hyperlink(&maximum).hyperlink.is_some());
    assert!(
        Span::plain("plain")
            .hyperlink(&oversized)
            .hyperlink
            .is_none()
    );
}
/// Public cell construction and emission both apply the screen sanitization
/// policy so callers cannot accidentally bypass styled-text control filtering.
#[test]
fn cell_api_sanitizes_controls() {
    assert_eq!(Cell::plain('\t').ch, ' ');
    assert_eq!(Cell::plain('\x1b').ch, '�');

    let cells = [Cell {
        ch: '\x1b',
        style: Style::default(),
        width: 0,
        hyperlink: None,
    }];
    let mut buf = Vec::new();
    emit_styled_cells(&mut buf, &cells).expect("cell emission should succeed");
    assert!(!buf.contains(&0x1b), "raw public cells must be sanitized");
}
/// Invalid hand-built public cells are normalized before diffing and caching,
/// keeping the model's column widths aligned with emitted terminal output.
#[test]
fn screen_update_normalizes_public_cells_before_caching() {
    let mut term = TestTerm::new(4, 5);
    let invalid = vec![vec![Cell {
        ch: '\x1b',
        style: Style::default(),
        width: 0,
        hyperlink: None,
    }]];
    let mut buf = Vec::new();
    term.screen
        .update(&mut buf, &invalid, (0, 1))
        .expect("update should succeed");
    term.term.process(&buf);

    assert_eq!(term.screen.lines[0][0].ch, '�');
    assert_eq!(term.screen.lines[0][0].col_width(), 1);
    let rendered = String::from_utf8_lossy(&buf);
    assert!(rendered.contains('�'));

    let replacement = vec![vec![Cell::plain('A')]];
    buf.clear();
    term.screen
        .update(&mut buf, &replacement, (0, 1))
        .expect("update should succeed");
    term.term.process(&buf);

    assert_eq!(term.screen.lines[0][0].ch, 'A');
}

// --- layout_block tests ---

/// Documents the block layout invariant: rows are padded to terminal width so
/// repainting can clear stale content.
#[test]
fn layout_block_plain() {
    let block = StyledBlock::new("hello");
    let lines = layout_block(&block, 20);
    assert_eq!(lines.len(), 1);
    // Row should be exactly 20 cells wide (content + padding).
    assert_eq!(lines[0].len(), 20);
    let text: String = lines[0].iter().map(|c| c.ch).collect();
    assert!(text.starts_with("hello"), "text: {text:?}");
    // Remaining chars should be spaces.
    assert!(text[5..].chars().all(|c| c == ' '));
}

/// Margins are part of the full-width block row but outside the content area;
/// this protects left/right spacing math.
#[test]
fn layout_block_with_margins() {
    let block = StyledBlock::new("hi").margin_left(2).margin_right(3);
    let lines = layout_block(&block, 20);
    assert_eq!(lines[0].len(), 20);
    let text: String = lines[0].iter().map(|c| c.ch).collect();
    // 2 margin + "hi" + padding + 3 margin = 20
    assert_eq!(&text[..2], "  ", "left margin");
    assert_eq!(&text[2..4], "hi", "content");
    assert_eq!(&text[17..20], "   ", "right margin");
}

/// Oversized margins are clamped so every block row still fits the terminal
/// width and leaves at least one column for content.
#[test]
fn layout_block_clamps_margins_to_width() {
    let block = StyledBlock::new("hi").margin_left(10).margin_right(10);
    let lines = layout_block(&block, 5);

    for line in &lines {
        assert_eq!(cols(line), 5);
        assert_eq!(line.len(), 5);
    }
}

/// Center alignment splits spare cells evenly so status/header blocks stay
/// visually stable.
#[test]
fn layout_block_center_alignment() {
    let block = StyledBlock::new("hi").align(Align::Center);
    let lines = layout_block(&block, 10);
    let text: String = lines[0].iter().map(|c| c.ch).collect();
    // "hi" is 2 chars, padding = 8, left = 4, right = 4.
    assert_eq!(text, "    hi    ");
}

/// Block padding must be based on terminal columns, including emoji variation
/// sequences, so rows padded to full width do not overrun the terminal.
#[test]
fn layout_block_pads_emoji_variation_selector_to_width() {
    let lines = layout_block(&StyledBlock::new("- ⚠️  …"), 10);
    assert_eq!(line_chars(&lines), vec!["- ⚠️  …   "]);
    assert_eq!(cols(&lines[0]), 10);
}

/// Right-side block content is an inline adornment when both left and right
/// parts fit on one row.
#[test]
fn layout_block_right_content_shown_when_space_available() {
    let block = StyledBlock::new("left").right_content("right");
    let lines = layout_block(&block, 12);
    let text: String = lines[0].iter().map(|c| c.ch).collect();
    assert_eq!(text, "left   right");
}

/// When primary block content wraps, the right adornment is hidden rather than
/// colliding with wrapped text.
#[test]
fn layout_block_right_content_hidden_when_left_wraps() {
    let block = StyledBlock::new("abcdef").right_content("right");
    let lines = layout_block(&block, 5);
    let text: Vec<String> = lines
        .iter()
        .map(|line| line.iter().map(|c| c.ch).collect())
        .collect();
    assert_eq!(text, vec!["abcde", "f    "]);
}

/// Priority-line blocks must replace ordinary content while retaining their
/// single-line edge placement.
#[test]
fn layout_block_uses_priority_line_content() {
    let mut priority_line = crate::PriorityLine::new();
    priority_line.push(
        crate::PriorityLinePriority::new(0),
        crate::PriorityLineAlignment::Left,
        Span::new("left", Style::default().bold()),
    );
    priority_line.push(
        crate::PriorityLinePriority::new(10),
        crate::PriorityLineAlignment::Right,
        "right",
    );
    let lines = layout_block(
        &StyledBlock::new("ordinary")
            .right_content("adornment")
            .priority_line(priority_line)
            .bg(Color::DarkBlue),
        12,
    );

    assert_eq!(line_chars(&lines), vec!["left   right"]);
    assert!(lines[0][0].style.bold);
    assert_eq!(lines[0][5].style.bg, Some(Color::DarkBlue));
}

/// Detail rows owned by an adaptive header must disappear when an essential
/// header band cannot fit, preventing anonymous output at tiny widths.
#[test]
fn priority_line_body_hides_when_required_header_fails_closed() {
    let mut priority_line = crate::PriorityLine::new();
    priority_line.require_through(crate::PriorityLinePriority::new(10));
    priority_line.push_truncated(
        crate::PriorityLinePriority::new(0),
        crate::PriorityLineAlignment::Left,
        "identity",
        crate::PriorityLineTruncation::new(4, 8),
    );
    priority_line.push(
        crate::PriorityLinePriority::new(10),
        crate::PriorityLineAlignment::Left,
        "ok",
    );
    let block = StyledBlock::new("ignored")
        .priority_line(priority_line)
        .priority_line_body("owned detail");

    assert_eq!(
        line_chars(&layout_block(&block, 7)),
        vec!["id┄y ok", "owned d", "etail  "]
    );
    assert_eq!(line_chars(&layout_block(&block, 6)), vec!["      "]);
}

/// Block backgrounds should paint content and padding, but not margins, so
/// margins remain transparent separators.
#[test]
fn layout_block_bg_applied_to_content_area() {
    let bg = Color::DarkBlue;
    let block = StyledBlock::new("ab").bg(bg).margin_left(1).margin_right(1);
    let lines = layout_block(&block, 10);
    // Margin cells should NOT have block bg.
    assert_eq!(lines[0][0].style.bg, None, "left margin has no bg");
    assert_eq!(lines[0][9].style.bg, None, "right margin has no bg");
    // Content area cells should have block bg.
    assert_eq!(lines[0][1].style.bg, Some(bg), "content has bg");
    assert_eq!(lines[0][2].style.bg, Some(bg), "content has bg");
    // Padding within content area should also have bg.
    assert_eq!(lines[0][3].style.bg, Some(bg), "padding has bg");
}

/// Combining a block background with styled text must not overwrite the
/// foreground color from the span.
#[test]
fn layout_block_content_fg_preserved_with_bg() {
    let fg = Color::Red;
    let bg = Color::DarkGreen;
    let block = StyledBlock::new(StyledText::from(Span::new("x", Style::default().fg(fg)))).bg(bg);
    let lines = layout_block(&block, 5);
    // The 'x' cell should have both fg from the span and bg from the block.
    assert_eq!(lines[0][0].ch, 'x');
    assert_eq!(lines[0][0].style.fg, Some(fg));
    assert_eq!(lines[0][0].style.bg, Some(bg));
}

/// Smoke-tests the complete block rendering path through ANSI output and a
/// terminal emulator, not just the in-memory cells.
#[test]
fn layout_block_renders_through_vt100() {
    let bg = Color::Blue;
    let block = StyledBlock::new(StyledText::from(Span::new(
        "test",
        Style::default().fg(Color::White),
    )))
    .bg(bg)
    .margin_left(1);
    let lines = layout_block(&block, 20);

    // Render through Screen + vt100.
    let mut term = vt100::Parser::new(5, 20, 0);
    let mut screen = Screen::new(20);
    let cursor = (0, 0);
    let mut buf = Vec::new();
    screen.update(&mut buf, &lines, cursor).expect("render ok");
    term.process(&buf);

    let row = term.screen().rows(0, 20).next().unwrap_or_default();
    assert!(row.starts_with(" test"), "row: {row:?}");

    // vt100 does not reliably preserve the exact foreground color
    // mapping for crossterm colors here, so assert that styling was
    // emitted at all rather than hard-coding vt100 color indices.
    let rendered = String::from_utf8(buf).expect("screen output should be utf8-ish ANSI");
    assert!(
        rendered.contains("\u{1b}["),
        "expected ANSI styling escapes in output, got: {rendered:?}"
    );
}

// --- screen rendering tests (using vt100 as a headless terminal) ---

/// Baseline render test: the first diff frame must draw prompt text and place
/// the cursor at the requested cell.
#[test]
fn first_render_shows_prompt() {
    let mut t = TestTerm::new(24, 80);
    t.render("> hello", 7);
    assert_eq!(t.row_text(0), "> hello");
    assert_eq!(t.cursor(), (0, 7));
}

/// Appending within an existing row should repaint the changed tail without
/// disturbing the cursor or surrounding content.
#[test]
fn appending_one_char_updates_correctly() {
    let mut t = TestTerm::new(24, 80);
    t.render("> hell", 6);
    assert_eq!(t.row_text(0), "> hell");

    t.render("> hello", 7);
    assert_eq!(t.row_text(0), "> hello");
    assert_eq!(t.cursor(), (0, 7));
}

/// Moving the cursor inside unchanged content must update terminal cursor
/// position without repainting stale text.
#[test]
fn cursor_moves_without_changing_content() {
    let mut t = TestTerm::new(24, 80);
    t.render("> hello", 7);

    // Move cursor to position 2 (after "> ").
    t.render("> hello", 2);
    assert_eq!(t.row_text(0), "> hello");
    assert_eq!(t.cursor(), (0, 2));
}

/// Updating a grapheme with zero-width continuations must repaint from the
/// cluster start, not from the continuation column. Otherwise changing `á` to
/// `a` can leave the old combining mark attached on real terminals.
#[test]
fn changing_combining_mark_repaints_cluster_start() {
    let mut screen = Screen::new(10);
    let first = layout_lines()
        .content(&StyledText::from("a\u{0301}"))
        .width(10)
        .call();
    let second = layout_lines()
        .content(&StyledText::from("a"))
        .width(10)
        .call();

    let mut buf = Vec::new();
    screen.update(&mut buf, &first, (0, 1)).expect("render ok");
    buf.clear();
    screen.update(&mut buf, &second, (0, 1)).expect("render ok");
    let rendered = String::from_utf8(buf).expect("screen output should be utf8-ish ANSI");
    assert!(
        rendered.contains('a'),
        "expected update to repaint base cell, got {rendered:?}"
    );
}

/// Shrinking a line must erase the old suffix so previous prompt characters do
/// not remain visible.
#[test]
fn shrinking_clears_old_text() {
    let mut t = TestTerm::new(24, 80);
    t.render("> hello world", 13);
    assert_eq!(t.row_text(0), "> hello world");

    t.render("> hi", 4);
    assert_eq!(t.row_text(0), "> hi");
    assert_eq!(t.cursor(), (0, 4));
}

/// Protects basic prompt wrapping and cursor placement when input exceeds the
/// terminal width.
#[test]
fn wrapping_to_second_line() {
    let mut t = TestTerm::new(24, 10);
    // 12 chars total, wraps at column 10.
    t.render("> abcdefghij", 12);
    assert_eq!(t.row_text(0), "> abcdefgh");
    assert_eq!(t.row_text(1), "ij");
    assert_eq!(t.cursor(), (1, 2));
}

/// When wrapped input shrinks back to one row, the now-unused wrapped row must
/// be cleared.
#[test]
fn removing_wrapped_line_clears_it() {
    let mut t = TestTerm::new(24, 10);
    t.render("> abcdefghij", 12);
    assert_eq!(t.row_text(1), "ij");

    t.render("> ab", 4);
    assert_eq!(t.row_text(0), "> ab");
    assert_eq!(t.row_text(1), "");
    assert_eq!(t.cursor(), (0, 4));
}

/// Async output invalidates Tau-owned rows; the next prompt render must redraw
/// from scratch at the same cursor position.
#[test]
fn invalidate_and_rerender_after_async_output() {
    let mut t = TestTerm::new(24, 80);
    t.render("> hello", 7);
    assert_eq!(t.row_text(0), "> hello");

    // Simulate async output clearing the prompt area.
    t.invalidate_and_render("> hello", 7);
    assert_eq!(t.row_text(0), "> hello");
    assert_eq!(t.cursor(), (0, 7));
}

/// Documents the exact-width cursor transition: filling the last column moves
/// the cursor to column zero of the next row.
#[test]
fn growing_from_one_to_two_lines() {
    let mut t = TestTerm::new(24, 10);
    t.render("> abcdefg", 9);
    assert_eq!(t.row_text(0), "> abcdefg");
    assert_eq!(t.row_text(1), "");

    // Add one more char, fills the line exactly.
    t.render("> abcdefgh", 10);
    assert_eq!(t.row_text(0), "> abcdefgh");
    // Cursor offset 10 / width 10 = row 1, col 0 (start of next line).
    assert_eq!(t.cursor(), (1, 0));

    // One more.
    t.render("> abcdefghi", 11);
    assert_eq!(t.row_text(0), "> abcdefgh");
    assert_eq!(t.row_text(1), "i");
    assert_eq!(t.cursor(), (1, 1));
}

/// A cursor inside wrapped content should be addressed on its visual row, not
/// forced to the end of the rendered prompt.
#[test]
fn cursor_in_middle_of_wrapped_content() {
    let mut t = TestTerm::new(24, 10);
    // 15 chars, cursor at position 5.
    t.render("> abcdefghijklm", 5);
    assert_eq!(t.row_text(0), "> abcdefgh");
    assert_eq!(t.row_text(1), "ijklm");
    assert_eq!(t.cursor(), (0, 5));
}

// --- styled rendering tests ---

/// Ensures styled spans generate terminal styling escapes while preserving the
/// visible text.
#[test]
fn styled_content_renders_with_color() {
    let mut t = TestTerm::new(24, 80);
    let style = Style::default().fg(Color::Blue);
    let styled = StyledText::from(vec![Span::plain("hi "), Span::new("world", style)]);
    let desired = layout_lines().content(&styled).width(80).call();
    let mut buf = Vec::new();
    t.screen.update(&mut buf, &desired, (0, 8)).expect("ok");
    t.term.process(&buf);

    assert_eq!(t.row_text(0), "hi world");

    // "hi " should be default style.
    let cell_h = t.term.screen().cell(0, 0).expect("cell exists");
    assert!(!cell_h.bold());
    assert_eq!(cell_h.fgcolor(), vt100::Color::Default);

    // vt100's color decoding is not stable enough here to assert an
    // exact palette index, but the renderer should have emitted style
    // escapes for the colored span.
    let rendered = String::from_utf8(buf).expect("screen output should be utf8-ish ANSI");
    assert!(
        rendered.contains("\u{1b}["),
        "expected ANSI styling escapes in output, got: {rendered:?}"
    );
}

/// Ensures the terminal style model emits real crossed-out SGR for
/// strikethrough text instead of relying on color-only fallbacks.
#[test]
fn styled_content_emits_strikethrough_sgr() {
    let styled = StyledText::from(Span::new("gone", Style::default().strikethrough()));
    let desired = layout_lines().content(&styled).width(80).call();
    let mut buf = Vec::new();

    emit_styled_cells(&mut buf, &desired[0]).expect("cell emission should succeed");

    let rendered = String::from_utf8(buf).expect("screen output should be utf8-ish ANSI");
    assert!(
        rendered.contains("\u{1b}[9m"),
        "expected crossed-out SGR in output, got: {rendered:?}"
    );
}

/// Changing only style attributes must still be treated as a diff so style-only
/// updates reach the terminal.
#[test]
fn styled_diff_only_rerenders_changed_styles() {
    let mut t = TestTerm::new(24, 80);
    let bold = Style::default().bold();

    // First render: plain text.
    t.render("hello", 5);
    assert_eq!(t.row_text(0), "hello");

    // Second render: same text but bold.
    let styled = StyledText::from(Span::new("hello", bold));
    let desired = layout_lines().content(&styled).width(80).call();
    let mut buf = Vec::new();
    t.screen.update(&mut buf, &desired, (0, 5)).expect("ok");
    t.term.process(&buf);

    assert_eq!(t.row_text(0), "hello");
    let cell = t.term.screen().cell(0, 0).expect("cell exists");
    assert!(cell.bold());
}

// --- multi-zone prompt tests ---

/// Helper to build a multi-zone layout: above-prompt lines, then
/// input line(s) with optional right-prompt on the first input line.
fn build_prompt_layout(
    above: &str,
    left: &str,
    input: &str,
    right: &str,
    width: usize,
) -> (Vec<Vec<Cell>>, (usize, usize)) {
    let mut desired: Vec<Vec<Cell>> = Vec::new();
    let above_row_count = if above.is_empty() {
        0
    } else {
        let above_styled: StyledText = above.into();
        desired.extend(layout_lines().content(&above_styled).width(width).call());
        desired.len()
    };

    let content = format!("{left}{input}");
    let content_styled: StyledText = content.into();
    let mut input_lines = layout_lines().content(&content_styled).width(width).call();

    // Right prompt on first input line if it fits and input is single-line.
    if !right.is_empty() && !input_lines.is_empty() {
        let first = &input_lines[0];
        let right_styled: StyledText = right.into();
        let right_cells = right_styled.to_cells();
        let first_cols = cols(first);
        let right_cols = cols(&right_cells);
        let needed = first_cols + 1 + right_cols;
        if needed <= width && input_lines.len() == 1 {
            let padding = width - first_cols - right_cols;
            let mut padded = first.clone();
            padded.extend(std::iter::repeat_n(Cell::plain(' '), padding));
            padded.extend(right_cells);
            input_lines[0] = padded;
        }
    }

    desired.extend(input_lines);

    let cursor_cols = crate::style::display_width(&format!("{left}{input}"));
    let cursor_row = above_row_count + cursor_cols / width;
    let cursor_col = cursor_cols % width;

    (desired, (cursor_row, cursor_col))
}

/// Multi-zone layouts must keep status/above-prompt rows before the editable
/// input row and offset the cursor accordingly.
#[test]
fn above_prompt_renders_before_input() {
    let mut t = TestTerm::new(24, 40);
    let (lines, cursor) = build_prompt_layout("status line", "> ", "hello", "", 40);
    let mut buf = Vec::new();
    t.screen.update(&mut buf, &lines, cursor).expect("ok");
    t.term.process(&buf);

    assert_eq!(t.row_text(0), "status line");
    assert_eq!(t.row_text(1), "> hello");
    assert_eq!(t.cursor(), (1, 7));
}

/// Multiple above-prompt rows should stack above input without confusing cursor
/// row accounting.
#[test]
fn multi_line_above_prompt() {
    let mut t = TestTerm::new(24, 40);
    let (lines, cursor) = build_prompt_layout("line one\nline two", "> ", "hi", "", 40);
    let mut buf = Vec::new();
    t.screen.update(&mut buf, &lines, cursor).expect("ok");
    t.term.process(&buf);

    assert_eq!(t.row_text(0), "line one");
    assert_eq!(t.row_text(1), "line two");
    assert_eq!(t.row_text(2), "> hi");
    assert_eq!(t.cursor(), (2, 4));
}

/// Right prompt content should be right-aligned on a single input row when it
/// fits.
#[test]
fn right_prompt_shown_when_space_available() {
    let mut t = TestTerm::new(24, 40);
    let (lines, cursor) = build_prompt_layout("", "> ", "hi", "[ok]", 40);
    let mut buf = Vec::new();
    t.screen.update(&mut buf, &lines, cursor).expect("ok");
    t.term.process(&buf);

    let row = t.row_text(0);
    assert!(row.starts_with("> hi"), "row: {row:?}");
    assert!(row.ends_with("[ok]"), "row: {row:?}");
    assert_eq!(row.len(), 40);
}

/// Right prompt content must disappear when the input plus gap would exceed
/// terminal width.
#[test]
fn right_prompt_hidden_when_input_too_long() {
    let mut t = TestTerm::new(24, 20);
    // "> " (2) + 15 chars + 1 gap + "[ok]" (4) = 22 > 20.
    let (lines, cursor) = build_prompt_layout("", "> ", "abcdefghijklmno", "[ok]", 20);
    let mut buf = Vec::new();
    t.screen.update(&mut buf, &lines, cursor).expect("ok");
    t.term.process(&buf);

    let row = t.row_text(0);
    assert!(
        !row.contains("[ok]"),
        "right prompt should be hidden, row: {row:?}"
    );
    assert!(row.starts_with("> abcdefghijklmno"), "row: {row:?}");
}

/// Once input wraps, the right prompt is hidden so it cannot be rendered on the
/// wrong visual row.
#[test]
fn right_prompt_hidden_when_input_wraps() {
    let mut t = TestTerm::new(24, 10);
    // Input wraps to second line — right prompt should not appear.
    let (lines, cursor) = build_prompt_layout("", "> ", "abcdefghij", "[x]", 10);
    let mut buf = Vec::new();
    t.screen.update(&mut buf, &lines, cursor).expect("ok");
    t.term.process(&buf);

    let row0 = t.row_text(0);
    let row1 = t.row_text(1);
    assert!(!row0.contains("[x]"), "row0: {row0:?}");
    assert_eq!(row1, "ij");
}

/// Covers the combined status, input, and right-prompt path that real prompts
/// use.
#[test]
fn all_three_zones_together() {
    let mut t = TestTerm::new(24, 40);
    let (lines, cursor) = build_prompt_layout("tau v0.1", "$ ", "ls", "[main]", 40);
    let mut buf = Vec::new();
    t.screen.update(&mut buf, &lines, cursor).expect("ok");
    t.term.process(&buf);

    assert_eq!(t.row_text(0), "tau v0.1");
    let prompt_row = t.row_text(1);
    assert!(prompt_row.starts_with("$ ls"), "row: {prompt_row:?}");
    assert!(prompt_row.ends_with("[main]"), "row: {prompt_row:?}");
    assert_eq!(t.cursor(), (1, 4));
}

// --- full_render / scrollback tests ---

/// Verifies the basic technique: output more lines than the
/// terminal height using \r\n, then check that overflow went
/// into scrollback and the visible screen shows the last rows.
#[test]
fn overflow_lines_go_to_scrollback() {
    // 5 rows tall, 20 cols, 100 lines of scrollback buffer.
    let mut term = vt100::Parser::new(5, 20, 100);

    // Output 10 lines (more than 5 rows).
    let mut buf = Vec::new();
    for i in 0..10 {
        if i > 0 {
            buf.extend_from_slice(b"\r\n");
        }
        buf.extend_from_slice(format!("line {i}").as_bytes());
    }
    term.process(&buf);

    // Visible screen should show the last 5 lines (5-9).
    let visible: Vec<String> = term.screen().rows(0, 20).collect();
    assert_eq!(visible[0], "line 5");
    assert_eq!(visible[1], "line 6");
    assert_eq!(visible[2], "line 7");
    assert_eq!(visible[3], "line 8");
    assert_eq!(visible[4], "line 9");

    // Scrollback should contain lines 0-4.
    // Set scrollback offset to see them.
    term.screen_mut().set_scrollback(5);
    let scrolled: Vec<String> = term.screen().rows(0, 20).collect();
    assert_eq!(scrolled[0], "line 0");
    assert_eq!(scrolled[1], "line 1");
    assert_eq!(scrolled[2], "line 2");
    assert_eq!(scrolled[3], "line 3");
    assert_eq!(scrolled[4], "line 4");
}

/// Verifies clear screen + scrollback (\x1b[2J\x1b[H\x1b[3J),
/// then re-output at a different width — the full_render technique
/// used on resize.
#[test]
fn clear_and_rerender_scrollback() {
    let mut term = vt100::Parser::new(5, 20, 100);

    // First render: 8 lines.
    let mut buf = Vec::new();
    for i in 0..8 {
        if i > 0 {
            buf.extend_from_slice(b"\r\n");
        }
        buf.extend_from_slice(format!("old line {i}").as_bytes());
    }
    term.process(&buf);

    // Verify initial state: visible = lines 3-7, scrollback = 0-2.
    let visible: Vec<String> = term.screen().rows(0, 20).collect();
    assert_eq!(visible[0], "old line 3");

    // Now simulate resize: clear + re-render with new content.
    let mut buf2 = Vec::new();
    buf2.extend_from_slice(b"\x1b[2J\x1b[H\x1b[3J"); // clear screen + scrollback
    for i in 0..8 {
        if i > 0 {
            buf2.extend_from_slice(b"\r\n");
        }
        buf2.extend_from_slice(format!("new line {i}").as_bytes());
    }
    term.process(&buf2);

    // Visible screen should show the last 5 new lines.
    let visible: Vec<String> = term.screen().rows(0, 20).collect();
    assert_eq!(visible[0], "new line 3");
    assert_eq!(visible[1], "new line 4");
    assert_eq!(visible[2], "new line 5");
    assert_eq!(visible[3], "new line 6");
    assert_eq!(visible[4], "new line 7");

    // Scrollback should have new lines 0-2.
    term.screen_mut().set_scrollback(3);
    let scrolled: Vec<String> = term.screen().rows(0, 20).collect();
    assert_eq!(scrolled[0], "new line 0");
    assert_eq!(scrolled[1], "new line 1");
    assert_eq!(scrolled[2], "new line 2");
}

/// Verifies cursor positioning after outputting more lines than
/// the terminal height (the MoveUp technique used in full_render).
#[test]
fn cursor_positioning_after_overflow() {
    let mut term = vt100::Parser::new(5, 20, 100);

    // Output 8 lines, then move cursor up to where "line 5"
    // is (which should be row 0 of the visible screen).
    let mut buf = Vec::new();
    for i in 0..8 {
        if i > 0 {
            buf.extend_from_slice(b"\r\n");
        }
        buf.extend_from_slice(format!("line {i}").as_bytes());
    }
    // Cursor is now at row 4 (bottom of visible screen), after "line 7".
    // Move up 4 rows to get to row 0 (where "line 3" is).
    buf.extend_from_slice(b"\x1b[4A"); // MoveUp(4)
    buf.extend_from_slice(b"\x1b[10G"); // MoveToColumn(10), 1-indexed
    term.process(&buf);

    let (row, col) = term.screen().cursor_position();
    assert_eq!(row, 0);
    assert_eq!(col, 9); // MoveToColumn is 1-indexed, vt100 returns 0-indexed
}

/// End-to-end test of the full_render function using vt100.
/// Simulates: output history + live area, then position cursor
/// in the input area using the same logic as full_render.
#[test]
fn render_scrolling_after_exact_width_line_does_not_duplicate_rows() {
    let width = 5;
    let height = 3;
    let mut term = vt100::Parser::new(height as u16, width as u16, 20);
    let mut screen = Screen::new(width);

    let initial = ["aaaaa", "bbbbb", "ccccc"];
    let initial_lines = plain_cell_lines(&initial);
    let mut buf = Vec::new();
    screen
        .update(&mut buf, &initial_lines, (2, width))
        .expect("initial render should succeed");
    term.process(&buf);

    let all = ["aaaaa", "BBBBB", "ccccc", "ddddd"];
    let all_lines = plain_cell_lines(&all);
    let mut buf = Vec::new();
    screen
        .render_scrolling(&mut buf, &all_lines, 0, height, (3, width))
        .expect("scroll render should succeed");
    let has_unanchored_lf = buf
        .iter()
        .enumerate()
        .any(|(idx, byte)| *byte == b'\n' && (idx == 0 || buf[idx - 1] != b'G'));
    assert!(
        !has_unanchored_lf,
        "scrolling movement must move to column 0 before moving down: {buf:?}"
    );
    term.process(&buf);

    let visible: Vec<String> = term.screen().rows(0, width as u16).collect();
    assert_eq!(visible, vec!["BBBBB", "ccccc", "ddddd"]);
}

/// End-to-end model of a full redraw: overflow builds scrollback, visible rows
/// are the viewport, and the cursor returns to input.
#[test]
fn full_render_via_vt100() {
    use crossterm::QueueableCommand;
    use crossterm::cursor::{MoveToColumn, MoveUp};

    let height: usize = 5;
    let width: usize = 30;
    let mut term = vt100::Parser::new(height as u16, width as u16, 100);

    // Build "all_lines": 3 history + 2 above + 1 input + 1 below = 7 lines.
    // Viewport (last 5): above0, above1, "> hello", below0, but wait
    // that's only 4 visible from the live area. Let me just use strings.
    let lines_text = [
        "history 0",
        "history 1",
        "history 2",
        "above block A",
        "above block B",
        "> hello", // input line, cursor should be here
        "below status",
    ];

    // The cursor is at the input line (index 5), column 7 ("> hello" = 7 chars).
    let cursor_row: usize = 5;
    let cursor_col: usize = 7;

    // Simulate full_render: clear + output all lines.
    let mut buf: Vec<u8> = Vec::new();
    // Clear screen + scrollback.
    buf.extend_from_slice(b"\x1b[2J\x1b[H\x1b[3J");
    // Output all lines.
    for (i, line) in lines_text.iter().enumerate() {
        if i > 0 {
            buf.extend_from_slice(b"\r\n");
        }
        buf.extend_from_slice(line.as_bytes());
    }
    term.process(&buf);

    // After outputting, cursor is at the last line.
    let total = lines_text.len(); // 7
    let viewport_top = total.saturating_sub(height); // 7 - 5 = 2
    let current_vp_row = total.saturating_sub(1).saturating_sub(viewport_top); // 6 - 2 = 4
    let cursor_vp_row = cursor_row.saturating_sub(viewport_top); // 5 - 2 = 3

    // Move cursor from current position to cursor position.
    let up = current_vp_row.saturating_sub(cursor_vp_row); // 4 - 3 = 1
    let mut buf2: Vec<u8> = Vec::new();
    if up > 0 {
        (&mut buf2 as &mut dyn std::io::Write)
            .queue(MoveUp(up as u16))
            .expect("ok");
    }
    (&mut buf2 as &mut dyn std::io::Write)
        .queue(MoveToColumn(cursor_col as u16))
        .expect("ok");
    term.process(&buf2);

    // Verify visible screen (rows 2-6 of all_lines).
    let visible: Vec<String> = term.screen().rows(0, width as u16).collect();
    assert_eq!(visible[0], "history 2");
    assert_eq!(visible[1], "above block A");
    assert_eq!(visible[2], "above block B");
    assert_eq!(visible[3], "> hello");
    assert_eq!(visible[4], "below status");

    // Verify cursor position.
    let (r, c) = term.screen().cursor_position();
    assert_eq!(r, cursor_vp_row as u16, "cursor row");
    assert_eq!(c, cursor_col as u16, "cursor col");

    // Verify scrollback contains history 0 and 1.
    term.screen_mut().set_scrollback(2);
    let scrolled: Vec<String> = term.screen().rows(0, width as u16).collect();
    assert_eq!(scrolled[0], "history 0");
    assert_eq!(scrolled[1], "history 1");
}

/// Verifies full_render with fewer lines than terminal height
/// (no scrollback needed).
#[test]
fn full_render_no_overflow() {
    let height: usize = 10;
    let width: usize = 30;
    let mut term = vt100::Parser::new(height as u16, width as u16, 100);

    let lines_text = [
        "above", "> hi", // cursor here, col 4
        "below",
    ];
    let cursor_row: usize = 1;
    let cursor_col: usize = 4;

    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"\x1b[2J\x1b[H\x1b[3J");
    for (i, line) in lines_text.iter().enumerate() {
        if i > 0 {
            buf.extend_from_slice(b"\r\n");
        }
        buf.extend_from_slice(line.as_bytes());
    }

    let total = lines_text.len(); // 3
    let viewport_top = total.saturating_sub(height); // 0
    let current_vp_row = total.saturating_sub(1).saturating_sub(viewport_top); // 2
    let cursor_vp_row = cursor_row.saturating_sub(viewport_top); // 1

    let up = current_vp_row.saturating_sub(cursor_vp_row); // 1
    if up > 0 {
        use std::io::Write;
        (&mut buf as &mut dyn Write)
            .queue(crossterm::cursor::MoveUp(up as u16))
            .expect("ok");
    }
    {
        use std::io::Write;
        (&mut buf as &mut dyn Write)
            .queue(crossterm::cursor::MoveToColumn(cursor_col as u16))
            .expect("ok");
    }
    term.process(&buf);

    let visible: Vec<String> = term.screen().rows(0, width as u16).collect();
    assert_eq!(visible[0], "above");
    assert_eq!(visible[1], "> hi");
    assert_eq!(visible[2], "below");

    let (r, c) = term.screen().cursor_position();
    assert_eq!(r, 1, "cursor row");
    assert_eq!(c, 4, "cursor col");
}

/// Simulates a resize: initial render at width 20, then resize to
/// width 10 (causing lines to re-wrap and produce more rows),
/// clear + re-render. Verifies scrollback is rebuilt correctly
/// with the new wrapping.
#[test]
fn scrollback_rebuilt_after_resize() {
    let height: usize = 5;

    // --- Phase 1: render at width 20 ---
    let width1: u16 = 20;
    let mut term = vt100::Parser::new(height as u16, width1, 100);

    // 7 lines at width 20 (2 scroll into scrollback on a 5-row terminal).
    let phase1 = [
        "aaaaaaaa", // 8 chars, fits in 20
        "bbbbbbbb",
        "cccccccc",
        "dddddddd",
        "eeeeeeee",
        "> input", // cursor line
        "status bar below",
    ];

    let mut buf: Vec<u8> = Vec::new();
    for (i, line) in phase1.iter().enumerate() {
        if i > 0 {
            buf.extend_from_slice(b"\r\n");
        }
        buf.extend_from_slice(line.as_bytes());
    }
    term.process(&buf);

    // Verify: visible = lines 2-6, scrollback = lines 0-1.
    let visible: Vec<String> = term.screen().rows(0, width1).collect();
    assert_eq!(visible[0], "cccccccc");
    assert_eq!(visible[4], "status bar below");

    // --- Phase 2: resize to width 10, re-render ---
    // At width 10, "status bar below" (16 chars) wraps to 2 lines.
    // Total lines increase.
    // Create a fresh parser at the new size (simulates the real
    // terminal being resized — our clear+rerender rebuilds
    // everything from scratch anyway).
    let width2: u16 = 10;
    let mut term = vt100::Parser::new(height as u16, width2, 100);

    let phase2 = [
        "aaaaaaaa", // fits in 10
        "bbbbbbbb",
        "cccccccc",
        "dddddddd",
        "eeeeeeee",
        "> input",
        "status bar", // "status bar below" wraps at 10
        " below",
    ];

    let mut buf2: Vec<u8> = Vec::new();
    buf2.extend_from_slice(b"\x1b[2J\x1b[H\x1b[3J"); // clear + clear scrollback
    for (i, line) in phase2.iter().enumerate() {
        if i > 0 {
            buf2.extend_from_slice(b"\r\n");
        }
        buf2.extend_from_slice(line.as_bytes());
    }
    term.process(&buf2);

    // Total = 8 lines, height = 5, so viewport_top = 3.
    // Visible: lines 3-7 = dddddddd, eeeeeeee, > input, status bar, " below"
    let visible2: Vec<String> = term.screen().rows(0, width2).collect();
    assert_eq!(visible2[0], "dddddddd", "visible row 0 after resize");
    assert_eq!(visible2[1], "eeeeeeee", "visible row 1 after resize");
    assert_eq!(visible2[2], "> input", "visible row 2 after resize");
    assert_eq!(visible2[3], "status bar", "visible row 3 after resize");
    assert_eq!(visible2[4], " below", "visible row 4 after resize");

    // Scrollback should have lines 0-2 (aaaaaaaa, bbbbbbbb, cccccccc).
    term.screen_mut().set_scrollback(3);
    let scrolled: Vec<String> = term.screen().rows(0, width2).collect();
    assert_eq!(scrolled[0], "aaaaaaaa", "scrollback row 0 after resize");
    assert_eq!(scrolled[1], "bbbbbbbb", "scrollback row 1 after resize");
    assert_eq!(scrolled[2], "cccccccc", "scrollback row 2 after resize");
}

struct PendingWrapTerm {
    width: usize,
    height: usize,
    rows: Vec<String>,
    scrollback: Vec<String>,
    row: usize,
    col: usize,
    pending_wrap: bool,
}

impl PendingWrapTerm {
    fn new(width: usize, rows: &[&str], cursor: (usize, usize)) -> Self {
        Self {
            width,
            height: rows.len(),
            rows: rows.iter().map(|row| (*row).to_owned()).collect(),
            scrollback: Vec::new(),
            row: cursor.0,
            col: cursor.1.min(width.saturating_sub(1)),
            pending_wrap: cursor.1 == width,
        }
    }

    fn process(&mut self, bytes: &[u8]) {
        let mut idx = 0;
        while idx < bytes.len() {
            match bytes[idx] {
                b'\x1b' => {
                    idx = self.process_escape(bytes, idx + 1);
                }
                b'\r' => {
                    self.col = 0;
                    idx += 1;
                }
                b'\n' => {
                    if self.pending_wrap {
                        self.advance_line();
                        self.pending_wrap = false;
                    }
                    self.advance_line();
                    idx += 1;
                }
                byte => {
                    self.print(byte as char);
                    idx += 1;
                }
            }
        }
    }

    fn process_escape(&mut self, bytes: &[u8], mut idx: usize) -> usize {
        if bytes.get(idx) != Some(&b'[') {
            return idx;
        }
        idx += 1;
        let start = idx;
        while bytes.get(idx).is_some_and(u8::is_ascii_digit) {
            idx += 1;
        }
        let n = std::str::from_utf8(&bytes[start..idx])
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(1);
        match bytes.get(idx) {
            Some(b'A') => {
                self.row = self.row.saturating_sub(n);
                self.pending_wrap = false;
                idx + 1
            }
            Some(b'G') => {
                self.col = n.saturating_sub(1).min(self.width.saturating_sub(1));
                self.pending_wrap = false;
                idx + 1
            }
            Some(b'K') => {
                self.rows[self.row].truncate(self.col);
                self.pending_wrap = false;
                idx + 1
            }
            _ => idx,
        }
    }

    fn print(&mut self, ch: char) {
        if self.pending_wrap {
            self.advance_line();
            self.pending_wrap = false;
        }
        while self.rows[self.row].len() < self.col {
            self.rows[self.row].push(' ');
        }
        if self.col < self.rows[self.row].len() {
            self.rows[self.row].replace_range(self.col..self.col + 1, &ch.to_string());
        } else {
            self.rows[self.row].push(ch);
        }
        if self.col + 1 == self.width {
            self.pending_wrap = true;
        } else {
            self.col += 1;
        }
    }

    fn advance_line(&mut self) {
        if self.row + 1 == self.height {
            self.scrollback.push(self.rows.remove(0));
            self.rows.push(String::new());
        } else {
            self.row += 1;
        }
    }
}

/// Regression guard from `fix(term-screen): avoid pending-wrap double scroll`:
/// moving down from a terminal pending-wrap state must produce one scroll, not
/// two.
#[test]
fn scrolling_from_pending_wrap_scrolls_once() {
    let width = 5;
    let height = 3;
    let mut screen = Screen::new(width);
    screen.reset_to(plain_cell_lines(&["bbbbb", "ccccc", "ddddd"]), 2, width);

    let all = plain_cell_lines(&["aaaaa", "bbbbb", "ccccc", "ddddd", "eeeee"]);
    let mut buf = Vec::new();
    screen
        .render_scrolling(&mut buf, &all, 1, height, (4, width))
        .expect("scroll render should succeed");

    let mut term = PendingWrapTerm::new(width, &["bbbbb", "ccccc", "ddddd"], (2, width));
    term.process(&buf);

    assert_eq!(term.scrollback, vec!["bbbbb"]);
    assert_eq!(term.rows, vec!["ccccc", "ddddd", "eeeee"]);
}
/// Clearing lines below an exact-width final row must start from the following
/// row, not from terminal pending-wrap state at the end of the full row.
#[test]
fn update_clears_below_after_exact_width_line() {
    let mut term = TestTerm::new(4, 5);
    term.render("abcde\nold", 5);
    term.render("abcde", 5);

    assert_eq!(term.row_text(0), "abcde");
    assert_eq!(term.row_text(1), "");
}

/// Regression guard from `fix(term-screen): avoid rewriting rows before
/// scroll`: rows about to leave the viewport must be scrolled naturally, not
/// repainted into scrollback.
#[test]
fn scrolling_after_already_scrolled_does_not_rewrite_rows_that_will_drop() {
    let width = 5;
    let height = 3;
    let mut screen = Screen::new(width);
    let mut prev_visible_start = 0;

    for frame in [
        &["aaaaa", "bbbbb", "ccccc"][..],
        &["aaaaa", "bbbbb", "ccccc", "ddddd"][..],
    ] {
        let lines = plain_cell_lines(frame);
        let visible_start = lines.len().saturating_sub(height);
        let mut buf = Vec::new();
        if visible_start > prev_visible_start {
            screen
                .render_scrolling(&mut buf, &lines, prev_visible_start, height, (2, width))
                .expect("scroll render should succeed");
        } else {
            screen
                .update(&mut buf, &lines[visible_start..], (2, width))
                .expect("update should succeed");
        }
        prev_visible_start = visible_start;
    }

    let lines = plain_cell_lines(&["aaaaa", "bbbbb", "ccccc", "ddddd", "eeeee"]);
    let mut buf = Vec::new();
    screen
        .render_scrolling(&mut buf, &lines, prev_visible_start, height, (2, width))
        .expect("scroll render should succeed");

    let output = String::from_utf8_lossy(&buf);
    assert!(output.contains("eeeee"));
    assert!(
        !output.contains("bbbbb"),
        "must not rewrite the row that should only drop to scrollback: {buf:?}"
    );
    assert!(
        !output.contains("ccccc"),
        "must not rewrite unchanged middle row: {buf:?}"
    );
    assert!(
        !output.contains("ddddd"),
        "must not rewrite unchanged bottom row: {buf:?}"
    );
}

/// Regression budget for differential rendering. A moderately complex TUI
/// layout is first rendered far enough to populate scrollback; changing one
/// visible character through the normal no-growth `update()` path must not
/// accidentally fall back to full-screen output.
#[test]
fn scrolling_single_cell_change_has_bounded_output() {
    const WIDTH: usize = 80;
    const HEIGHT: usize = 20;
    const MAX_DIFF_BYTES: usize = 1_200;

    fn complex_lines(changed_char: char) -> Vec<Vec<Cell>> {
        let mut all = Vec::new();
        for i in 0..36 {
            let mut text = StyledText::new();
            text.push(Span::new(
                format!("agent-{i:02} "),
                Style::default().fg(Color::Cyan).bold(),
            ));
            text.push(Span::new(
                "processed tool output with cached context and progress ",
                Style::default(),
            ));
            let marker = if i == 28 { changed_char } else { 'a' };
            text.push(Span::new(
                format!("marker={marker} status=ready"),
                Style::default().fg(Color::Yellow),
            ));
            let block = StyledBlock::new(text)
                .right_content(Span::new(
                    format!("#{i:03}"),
                    Style::default().fg(Color::DarkGrey),
                ))
                .margins(1, 1)
                .bg(if i % 2 == 0 {
                    Color::DarkBlue
                } else {
                    Color::DarkGreen
                });
            all.extend(layout_block(&block, WIDTH));
        }
        all
    }

    let before = complex_lines('x');
    assert!(HEIGHT < before.len(), "fixture must create scrollback");
    let mut screen = Screen::new(WIDTH);
    let mut term = vt100::Parser::new(HEIGHT as u16, WIDTH as u16, 200);
    let mut initial = Vec::new();
    screen
        .render_scrolling(&mut initial, &before, 0, HEIGHT, (before.len() - 1, 0))
        .expect("initial render should succeed");
    term.process(&initial);

    let after = complex_lines('y');
    let changed_rows: Vec<_> = before
        .iter()
        .zip(after.iter())
        .enumerate()
        .filter_map(|(row, (old, new))| (old != new).then_some(row))
        .collect();
    assert_eq!(
        changed_rows.len(),
        1,
        "fixture should change one physical row"
    );
    let prev_viewport_top = before.len().saturating_sub(HEIGHT);
    assert!(
        prev_viewport_top <= changed_rows[0] && changed_rows[0] < before.len(),
        "changed row must be visible: changed={} viewport={}..{}",
        changed_rows[0],
        prev_viewport_top,
        before.len()
    );

    let visible_after = &after[prev_viewport_top..];
    let mut full_visible = Vec::new();
    Screen::new(WIDTH)
        .update(&mut full_visible, visible_after, (HEIGHT - 1, 0))
        .expect("full visible render should succeed");

    let mut diff = Vec::new();
    screen
        .update(&mut diff, visible_after, (HEIGHT - 1, 0))
        .expect("differential render should succeed");
    term.process(&diff);
    let visible_text = term
        .screen()
        .rows(0, WIDTH as u16)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        visible_text.contains("marker=y"),
        "updated viewport should contain new marker: {visible_text:?}"
    );
    assert!(
        !visible_text.contains("marker=x"),
        "updated viewport should not contain stale marker: {visible_text:?}"
    );

    assert!(!diff.is_empty(), "changed cell should emit output");
    assert!(
        diff.len() < MAX_DIFF_BYTES,
        "single-cell update emitted {} bytes, expected < {MAX_DIFF_BYTES}; output={:?}",
        diff.len(),
        String::from_utf8_lossy(&diff)
    );
    assert!(
        diff.len() * 4 < full_visible.len(),
        "single-cell update should be much smaller than full visible render: diff={} full={}",
        diff.len(),
        full_visible.len()
    );
}

/// Regression guard from `fix(term-screen): scroll when appending empty rows`:
/// even an empty appended row must advance the viewport and scrollback.
#[test]
fn scrolling_growth_by_empty_line_still_scrolls_viewport() {
    let width = 5;
    let height = 3;
    let mut term = vt100::Parser::new(height as u16, width as u16, 20);
    let mut screen = Screen::new(width);

    let initial = plain_cell_lines(&["aaaaa", "bbbbb", "ccccc"]);
    let mut buf = Vec::new();
    screen
        .update(&mut buf, &initial, (2, width))
        .expect("initial render should succeed");
    term.process(&buf);

    let all = plain_cell_lines(&["aaaaa", "bbbbb", "ccccc", ""]);
    let mut buf = Vec::new();
    screen
        .render_scrolling(&mut buf, &all, 0, height, (3, 0))
        .expect("scroll render should succeed");
    term.process(&buf);

    let visible: Vec<String> = term.screen().rows(0, width as u16).collect();
    assert_eq!(visible, vec!["bbbbb", "ccccc", ""]);

    term.screen_mut().set_scrollback(1);
    let scrolled: Vec<String> = term.screen().rows(0, width as u16).collect();
    assert_eq!(scrolled, vec!["aaaaa", "bbbbb", "ccccc"]);
}

/// Protects the sequence where an empty row scroll is followed by a text row,
/// preserving terminal order across frames.
#[test]
fn scrolling_empty_line_then_text_line_keeps_order() {
    let width = 5;
    let height = 3;
    let mut term = vt100::Parser::new(height as u16, width as u16, 20);
    let mut screen = Screen::new(width);
    let mut prev_visible_start = 0;

    for frame in [
        &["aaaaa", "bbbbb", "ccccc"][..],
        &["aaaaa", "bbbbb", "ccccc", ""][..],
        &["aaaaa", "bbbbb", "ccccc", "", "ddddd"][..],
    ] {
        let lines = plain_cell_lines(frame);
        let visible_start = lines.len().saturating_sub(height);
        let mut buf = Vec::new();
        if visible_start > prev_visible_start {
            screen
                .render_scrolling(
                    &mut buf,
                    &lines,
                    prev_visible_start,
                    height,
                    (visible_start, 0),
                )
                .expect("scroll render should succeed");
        } else {
            screen
                .update(&mut buf, &lines[visible_start..], (0, 0))
                .expect("update should succeed");
        }
        term.process(&buf);
        prev_visible_start = visible_start;
    }

    let visible: Vec<String> = term.screen().rows(0, width as u16).collect();
    assert_eq!(visible, vec!["ccccc", "", "ddddd"]);

    term.screen_mut().set_scrollback(2);
    let scrolled: Vec<String> = term.screen().rows(0, width as u16).collect();
    assert_eq!(scrolled, vec!["aaaaa", "bbbbb", "ccccc"]);
}

/// When a changed top row becomes scrollback during the same frame, scrollback
/// must contain the new text, not the old cached row.
#[test]
fn scrolling_changed_view_top_into_scrollback_preserves_new_text() {
    let width = 5;
    let height = 3;
    let mut term = vt100::Parser::new(height as u16, width as u16, 20);
    let mut screen = Screen::new(width);

    let mut prev_visible_start = 0;
    for frame in [
        &["aaaaa", "bbbbb", "ccccc"][..],
        &["aaaaa", "bbbbb", "ccccc", "ddddd"][..],
    ] {
        let lines = plain_cell_lines(frame);
        let visible_start = lines.len().saturating_sub(height);
        let mut buf = Vec::new();
        if visible_start > prev_visible_start {
            screen
                .render_scrolling(&mut buf, &lines, prev_visible_start, height, (2, width))
                .expect("scroll render should succeed");
        } else {
            screen
                .update(&mut buf, &lines[visible_start..], (2, width))
                .expect("update should succeed");
        }
        term.process(&buf);
        prev_visible_start = visible_start;
    }

    let lines = plain_cell_lines(&["aaaaa", "BBBBB", "ccccc", "ddddd", "eeeee"]);
    let mut buf = Vec::new();
    screen
        .render_scrolling(&mut buf, &lines, prev_visible_start, height, (2, width))
        .expect("scroll render should succeed");
    term.process(&buf);

    let visible: Vec<String> = term.screen().rows(0, width as u16).collect();
    assert_eq!(visible, vec!["ccccc", "ddddd", "eeeee"]);

    term.screen_mut().set_scrollback(2);
    let scrolled: Vec<String> = term.screen().rows(0, width as u16).collect();
    assert_eq!(scrolled, vec!["aaaaa", "BBBBB", "ccccc"]);
}

/// Compares incremental scrolling with full-render behavior for exact-width
/// cursor positions across possible cursor rows.
#[test]
fn scrolling_matches_full_render_for_cursor_rows_and_exact_width_lines() {
    let width = 5;
    let height = 3;

    for cursor_row in 0..height {
        let mut term = vt100::Parser::new(height as u16, width as u16, 20);
        let mut screen = Screen::new(width);
        let mut prev_visible_start = 0;

        let frames: &[&[&str]] = &[
            &["aaaaa", "bbbbb", "ccccc"],
            &["aaaaa", "bbbbb", "ccccc", "ddddd"],
            &["aaaaa", "bbbbb", "ccccc", "ddddd", "eeeee"],
        ];

        for frame in frames {
            let lines = plain_cell_lines(frame);
            let visible_start = lines.len().saturating_sub(height);
            let desired_cursor = (cursor_row.min(lines.len() - 1), width);
            let mut buf = Vec::new();
            if visible_start > prev_visible_start {
                screen
                    .render_scrolling(&mut buf, &lines, prev_visible_start, height, desired_cursor)
                    .expect("scroll render should succeed");
            } else {
                screen
                    .update(&mut buf, &lines[visible_start..], desired_cursor)
                    .expect("update should succeed");
            }
            term.process(&buf);
            prev_visible_start = visible_start;
        }

        let visible: Vec<String> = term.screen().rows(0, width as u16).collect();
        assert_eq!(
            visible,
            vec!["ccccc", "ddddd", "eeeee"],
            "cursor row {cursor_row}"
        );

        term.screen_mut().set_scrollback(2);
        let scrolled: Vec<String> = term.screen().rows(0, width as u16).collect();
        assert_eq!(
            scrolled,
            vec!["aaaaa", "bbbbb", "ccccc"],
            "cursor row {cursor_row}"
        );
    }
}

/// Exact-width cursors can leave the terminal in pending-wrap state; this
/// protects scrollback order when the hidden top row also changes.
#[test]
fn scrolling_from_exact_width_cursor_with_top_change_keeps_scrollback_order() {
    let width = 5;
    let height = 3;
    let mut term = vt100::Parser::new(height, width, 20);
    let mut screen = Screen::new(width as usize);

    let initial = plain_cell_lines(&["aaaaa", "bbbbb", "ccccc"]);
    let mut buf = Vec::new();
    screen
        .update(&mut buf, &initial, (2, width as usize))
        .expect("initial render should succeed");
    term.process(&buf);

    let all = plain_cell_lines(&["AAAAA", "bbbbb", "ccccc", "ddddd"]);
    let mut buf = Vec::new();
    screen
        .render_scrolling(&mut buf, &all, 0, height as usize, (3, width as usize))
        .expect("scroll render should succeed");
    term.process(&buf);

    let visible: Vec<String> = term.screen().rows(0, width).collect();
    assert_eq!(visible, vec!["bbbbb", "ccccc", "ddddd"]);

    term.screen_mut().set_scrollback(1);
    let scrolled: Vec<String> = term.screen().rows(0, width).collect();
    assert_eq!(scrolled, vec!["AAAAA", "bbbbb", "ccccc"]);
}

/// Repeated one-row growth should move each overflow row into scrollback
/// exactly once, without duplicate visible or history rows.
#[test]
fn repeated_scrolling_growth_does_not_duplicate_overflow_rows() {
    let width = 5;
    let height = 3;
    let mut term = vt100::Parser::new(height, width, 20);
    let mut screen = Screen::new(width as usize);

    let frames: Vec<Vec<&str>> = vec![
        vec!["aaaaa", "bbbbb", "ccccc"],
        vec!["aaaaa", "bbbbb", "ccccc", "ddddd"],
        vec!["aaaaa", "bbbbb", "ccccc", "ddddd", "eeeee"],
    ];

    let mut prev_visible_start = 0;
    for frame in &frames {
        let lines = plain_cell_lines(frame);
        let mut buf = Vec::new();
        let cursor_row = frame.len().saturating_sub(1);
        screen
            .render_scrolling(
                &mut buf,
                &lines,
                prev_visible_start,
                height as usize,
                (cursor_row, width as usize),
            )
            .expect("scroll render should succeed");
        term.process(&buf);
        prev_visible_start = frame.len().saturating_sub(height as usize);
    }

    let visible: Vec<String> = term.screen().rows(0, width).collect();
    assert_eq!(visible, vec!["ccccc", "ddddd", "eeeee"]);

    term.screen_mut().set_scrollback(2);
    let scrolled: Vec<String> = term.screen().rows(0, width).collect();
    assert_eq!(scrolled, vec!["aaaaa", "bbbbb", "ccccc"]);
}
