use std::sync as path_std_sync;

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

/// Empty content owns one addressable row, while wrapping and newlines split
/// rows without adding phantom rows at exact-width boundaries.
#[test]
fn layout_lines_obeys_empty_wrap_and_newline_boundaries() {
    for (content, width, expected) in [
        ("", 80, &[""][..]),
        ("abc", 80, &["abc"][..]),
        ("abcde", 3, &["abc", "de"][..]),
        ("abc", 3, &["abc"][..]),
        ("abc\ndef", 80, &["abc", "def"][..]),
        ("abc\ndef", 3, &["abc", "def"][..]),
    ] {
        let lines = layout_lines()
            .content(&StyledText::from(content))
            .width(width)
            .call();
        assert_eq!(line_chars(&lines), expected);
    }
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
        hyperlink: Some(path_std_sync::Arc::from("https://bad.test/\u{1b}]8")),
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
        ch: '\t',
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

/// Blocks pad every row to the terminal width while margins and alignment keep
/// their exact column geometry, including when oversized margins are clamped.
#[test]
fn layout_block_applies_padding_margins_and_alignment() {
    let cases = [
        (StyledBlock::new("hello"), 20, &["hello               "][..]),
        (
            StyledBlock::new("hi").margin_left(2).margin_right(3),
            20,
            &["  hi                "][..],
        ),
        (
            StyledBlock::new("hi").margin_left(10).margin_right(10),
            5,
            &["    h", "    i"][..],
        ),
        (
            StyledBlock::new("hi").align(Align::Center),
            10,
            &["    hi    "][..],
        ),
    ];

    for (block, width, expected) in cases {
        let lines = layout_block(&block, width);
        assert_eq!(line_chars(&lines), expected);
        assert!(lines.iter().all(|line| cols(line) == width));
    }
}

/// Width-adaptive excerpts preserve the first-line beginning and last-line end
/// while staying at exactly two physical rows after terminal shrink.
#[test]
fn two_line_elision_adapts_to_current_width() {
    let block = StyledBlock::new("").two_line_elision(crate::TwoLineElision {
        prefix: "◯ ".into(),
        first: "First line with discarded tail".into(),
        last: "forgotten start end of last line.".into(),
        first_omissions: vec!["   ┄".into(), "┄".into()],
        last_omissions: vec!["┄ ".into(), "┄".into()],
        labels: vec![" (queued)".into(), " (q)".into(), "q".into()],
        unabridged: None,
    });

    let rows = layout_block(&block, 32);
    assert_eq!(rows.len(), 2);
    assert_eq!(cols(&rows[0]), 32);
    assert_eq!(cols(&rows[1]), 32);
    let text = line_chars(&rows);
    assert!(text[0].starts_with("◯ First line with discarded"));
    assert!(text[0].contains('┄'));
    assert!(text[1].starts_with("┄ "));
    assert!(text[1].contains("last line. (queued)"));
}

/// Wide graphemes remain intact and every narrow layout stays within two rows
/// and its exact terminal-column budget.
#[test]
fn two_line_elision_handles_unicode_and_narrow_widths() {
    let block = StyledBlock::new("").two_line_elision(crate::TwoLineElision {
        prefix: "◯ ".into(),
        first: "👩‍🔬研究研究研究".into(),
        last: "研究研究👩‍🔬".into(),
        first_omissions: vec!["   ┄".into(), "┄".into()],
        last_omissions: vec!["┄ ".into(), "┄".into()],
        labels: vec![" (queued)".into(), " (q)".into(), "q".into()],
        unabridged: None,
    });

    for width in 1..=20 {
        let rows = layout_block(&block, width);
        assert_eq!(rows.len(), 2, "width {width}");
        assert!(rows.iter().all(|row| cols(row) == width), "width {width}");
    }
}

/// Boundary truncation retains complete grapheme clusters and preserves the
/// selected excerpt, omission, and label metadata.
#[test]
fn two_line_elision_preserves_boundary_graphemes_and_metadata() {
    let excerpt_style = Style::default().fg(Color::Green);
    let omission_style = Style::default().fg(Color::Red);
    let label_style = Style::default().fg(Color::Blue);
    let block = StyledBlock::new("").two_line_elision(crate::TwoLineElision {
        prefix: "".into(),
        first: Span::new("A👩‍🔬B", excerpt_style)
            .hyperlink("https://example.test/excerpt")
            .into(),
        last: Span::new("X👩‍🔬", excerpt_style)
            .hyperlink("https://example.test/excerpt")
            .into(),
        first_omissions: vec![
            Span::new("too-long", omission_style).into(),
            Span::new("┄", omission_style)
                .hyperlink("https://example.test/omission")
                .into(),
        ],
        last_omissions: vec![Span::new("┄", omission_style).into()],
        labels: vec![
            Span::new("label", label_style).into(),
            Span::new("q", label_style)
                .hyperlink("https://example.test/label")
                .into(),
        ],
        unabridged: None,
    });

    let rows = layout_block(&block, 4);
    let text = line_chars(&rows);
    assert_eq!(text, vec!["A👩‍🔬┄", "┄👩‍🔬q"]);
    let first_omission = rows[0]
        .iter()
        .position(|cell| cell.ch == '┄')
        .expect("first omission");
    for cell in &rows[0][..first_omission] {
        assert_eq!(cell.style, excerpt_style);
        assert_eq!(
            cell.hyperlink.as_deref(),
            Some("https://example.test/excerpt")
        );
    }
    let omission = &rows[0][first_omission];
    assert_eq!(omission.style, omission_style);
    assert_eq!(
        omission.hyperlink.as_deref(),
        Some("https://example.test/omission")
    );
    let last_omission = rows[1]
        .iter()
        .position(|cell| cell.ch == '┄')
        .expect("last omission");
    let label_index = rows[1]
        .iter()
        .position(|cell| cell.ch == 'q')
        .expect("label");
    for cell in &rows[1][last_omission + 1..label_index] {
        assert_eq!(cell.style, excerpt_style);
        assert_eq!(
            cell.hyperlink.as_deref(),
            Some("https://example.test/excerpt")
        );
    }
    let label = &rows[1][label_index];
    assert_eq!(label.style, label_style);
    assert_eq!(
        label.hyperlink.as_deref(),
        Some("https://example.test/label")
    );
}

/// A bounded complete presentation remains natural when it needs no more than
/// two rows, avoiding unnecessary duplicated head/tail excerpts.
#[test]
fn two_line_elision_keeps_short_content_unabridged() {
    let block = StyledBlock::new("").two_line_elision(crate::TwoLineElision {
        prefix: "◯ ".into(),
        first: "short".into(),
        last: "short".into(),
        first_omissions: vec!["   ┄".into(), "┄".into()],
        last_omissions: vec!["┄ ".into(), "┄".into()],
        labels: vec![" (queued)".into(), " (q)".into(), "q".into()],
        unabridged: Some("short (queued)".into()),
    });

    assert_eq!(
        line_chars(&layout_block(&block, 20)),
        vec!["◯ short (queued)    "]
    );
}

/// Exact two-row complete content stays unabridged, while the same retained
/// block switches to its excerpt when a narrower width would require three
/// rows.
#[test]
fn two_line_elision_switches_at_exact_row_boundary() {
    let block = StyledBlock::new("ordinary").two_line_elision(crate::TwoLineElision {
        prefix: "".into(),
        first: "1234".into(),
        last: "5678".into(),
        first_omissions: vec!["┄".into()],
        last_omissions: vec!["┄".into()],
        labels: vec![],
        unabridged: Some("12345678".into()),
    });

    assert_eq!(line_chars(&layout_block(&block, 4)), vec!["1234", "5678"]);
    let narrow = line_chars(&layout_block(&block, 3));
    assert_eq!(narrow.len(), 2);
    assert!(narrow.iter().all(|row| row.contains('┄')));
}

/// Selecting one alternative block layout replaces the previous mode instead of
/// leaving hidden priority state behind.
#[test]
fn styled_block_alternative_layouts_are_exclusive() {
    let mut line = crate::PriorityLine::new();
    line.push(
        crate::PriorityLinePriority::new(0),
        crate::PriorityLineAlignment::Left,
        Span::plain("priority"),
    );
    let block = StyledBlock::new("ordinary")
        .priority_line(line)
        .priority_line_body("hidden body")
        .two_line_elision(crate::TwoLineElision {
            prefix: "".into(),
            first: "first".into(),
            last: "last".into(),
            first_omissions: vec!["┄".into()],
            last_omissions: vec!["┄".into()],
            labels: vec![],
            unabridged: None,
        })
        .priority_line_body("also hidden");

    let rows = line_chars(&layout_block(&block, 8));
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| !row.contains("priority")));
    assert!(rows.iter().all(|row| !row.contains("hidden")));
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

/// Block cells preserve span foregrounds over the background and restore the
/// default foreground before emitting background-only padding.
#[test]
fn layout_block_content_fg_preserved_with_bg() {
    crossterm::style::force_color_output(true);
    let fg = Color::Red;
    let bg = Color::DarkGreen;
    let block = StyledBlock::new(StyledText::from(Span::new("x", Style::default().fg(fg)))).bg(bg);
    let lines = layout_block(&block, 5);
    // The 'x' cell should have both fg from the span and bg from the block.
    assert_eq!(lines[0][0].ch, 'x');
    assert_eq!(lines[0][0].style.fg, Some(fg));
    assert_eq!(lines[0][0].style.bg, Some(bg));

    let mut emitted = Vec::new();
    emit_styled_cells(&mut emitted, &lines[0]).expect("block cell emission should succeed");

    let mut term = vt100::Parser::new(1, 5, 0);
    term.process(&emitted);
    assert_eq!(term.screen().rows(0, 5).next(), Some("x    ".to_owned()));
    let content = term.screen().cell(0, 0).expect("content cell");
    assert_eq!(content.fgcolor(), vt100::Color::Idx(9));
    assert_eq!(content.bgcolor(), vt100::Color::Idx(2));
    let padding = term.screen().cell(0, 1).expect("padding cell");
    assert_eq!(padding.fgcolor(), vt100::Color::Default);
    assert_eq!(padding.bgcolor(), vt100::Color::Idx(2));
    assert_eq!(term.screen().fgcolor(), vt100::Color::Default);
    assert_eq!(term.screen().bgcolor(), vt100::Color::Default);
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

/// Default and colored spans apply the requested foreground and reset it before
/// a following default-style sentinel.
#[test]
fn styled_content_renders_with_color() {
    crossterm::style::force_color_output(true);
    let mut t = TestTerm::new(24, 80);
    let style = Style::default().fg(Color::Blue);
    let styled = StyledText::from(vec![
        Span::plain("hi "),
        Span::new("world", style),
        Span::plain("!"),
    ]);
    let desired = layout_lines().content(&styled).width(80).call();
    let mut buf = Vec::new();
    emit_styled_cells(&mut buf, &desired[0]).expect("cell emission should succeed");
    t.term.process(&buf);

    assert_eq!(t.row_text(0), "hi world!");
    assert_eq!(
        t.term.screen().cell(0, 3).expect("colored cell").fgcolor(),
        vt100::Color::Idx(12)
    );
    assert_eq!(
        t.term
            .screen()
            .cell(0, 8)
            .expect("default-style sentinel")
            .fgcolor(),
        vt100::Color::Default
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

// --- scrolling tests ---

/// A scrolling render after an exact-width row must anchor its newline at
/// column zero so it does not duplicate physical rows.
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
        if prev_visible_start < visible_start {
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

/// A scrolling frame normalizes a late hand-built cell, repaints that suffix
/// without redrawing unchanged rows, and caches the normalized viewport.
#[test]
fn scrolling_single_cell_change_has_bounded_output() {
    crossterm::style::force_color_output(true);
    const WIDTH: usize = 5;
    const HEIGHT: usize = 3;

    let before = plain_cell_lines(&["aaaaa", "bbbbb", "ccccc", "ddddd"]);
    let mut screen = Screen::new(WIDTH);
    let mut term = vt100::Parser::new(HEIGHT as u16, WIDTH as u16, 20);
    let mut initial = Vec::new();
    screen
        .render_scrolling(&mut initial, &before, 0, HEIGHT, (3, WIDTH))
        .expect("initial scrolling render should succeed");
    term.process(&initial);

    let mut raw_after = before.clone();
    raw_after[3][3] = Cell {
        ch: '\t',
        style: Style::default().fg(Color::Cyan),
        width: 1,
        hyperlink: None,
    };
    let visible_after = &raw_after[1..];
    let mut normalized_after = before[1..].to_vec();
    normalized_after[2][3] = Cell::new(' ', Style::default().fg(Color::Cyan));
    let mut diff = Vec::new();
    screen
        .update(&mut diff, visible_after, (HEIGHT - 1, WIDTH))
        .expect("differential render should succeed");
    assert_eq!(&diff[..4], b"\x1b[4G");
    let payload = &diff[4..];
    assert!(
        payload.ends_with(b"d"),
        "changed suffix must retain the trailing cell"
    );
    assert_eq!(
        payload.iter().filter(|byte| **byte == b'd').count(),
        1,
        "changed suffix must not repaint the unchanged ddd prefix"
    );
    for unchanged_row in [b"bbbbb".as_slice(), b"ccccc".as_slice(), b"ddd".as_slice()] {
        assert!(
            !payload
                .windows(unchanged_row.len())
                .any(|bytes| bytes == unchanged_row),
            "changed suffix must not repaint {unchanged_row:?}: {diff:?}"
        );
    }
    term.process(&diff);

    let visible: Vec<String> = term.screen().rows(0, WIDTH as u16).collect();
    assert_eq!(
        visible,
        vec!["bbbbb", "ccccc", "ddd d"],
        "diff={:?}",
        String::from_utf8_lossy(&diff)
    );
    assert_eq!(
        term.screen()
            .cell(2, 3)
            .expect("normalized replacement cell")
            .fgcolor(),
        vt100::Color::Idx(14)
    );
    assert_eq!(
        term.screen()
            .cell(2, 4)
            .expect("unchanged trailing cell")
            .fgcolor(),
        vt100::Color::Default
    );
    assert_eq!(screen.lines, normalized_after);

    let mut unchanged = Vec::new();
    screen
        .update(&mut unchanged, visible_after, (HEIGHT - 1, WIDTH))
        .expect("identical raw viewport update should succeed");
    assert!(
        unchanged.is_empty(),
        "identical raw viewport must not repaint"
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
        if prev_visible_start < visible_start {
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
        if prev_visible_start < visible_start {
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
            if prev_visible_start < visible_start {
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
