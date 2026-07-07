use super::*;

fn markdown_test_theme() -> tau_themes::Theme {
    tau_themes::Theme::parse(
        r##"{
            styles: {
                "shell.output": { },
                "user.prompt": { },
                "prompt.marker.submitted": { fg: "red" },
                "markdown.strong": { bold: true },
                "markdown.emphasis": { italic: true },
                "markdown.strikethrough": { strikethrough: true },
                "markdown.heading": { underline: true },
                "markdown.list.marker": { fg: "green" },
                "markdown.code": { bg: "#111111" },
                "markdown.escape": { bg: "#222222" },
                "progress.indicator": { fg: "cyan" },
            }
        }"##,
    )
    .expect("valid markdown test theme")
}

fn rendered_text(block: &tau_cli_term::StyledBlock) -> String {
    block
        .content
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect()
}

/// Ensures non-table Markdown-lite syntax is style-only and preserves source
/// text exactly. Leading-pipe tables are covered separately because they may
/// get display-only padding.
#[test]
fn final_render_preserves_non_table_source_text() {
    let theme = markdown_test_theme();
    let block = markdown_block(
        &theme,
        names::USER_PROMPT,
        "# Title\n- *bold* and _italics_ and ~~deleted~~\n",
    );

    assert_eq!(
        rendered_text(&block),
        "# Title\n- *bold* and _italics_ and ~~deleted~~\n"
    );
}

/// Ensures headings, list markers, strong, emphasis, and strikethrough map to
/// semantic theme attributes.
#[test]
fn final_render_applies_markdown_styles() {
    let theme = markdown_test_theme();
    let block = markdown_block(
        &theme,
        names::SHELL_OUTPUT,
        "# Title\n- *bold* and _italics_ and ~~deleted~~",
    );
    let spans = block.content.spans();

    let heading = spans
        .iter()
        .find(|span| span.text == "# Title")
        .expect("expected styled markdown span");
    assert!(heading.style.underline);

    let marker = spans
        .iter()
        .find(|span| span.text == "-")
        .expect("expected styled markdown span");
    assert_eq!(marker.style.fg, Some(tau_cli_term::Color::Green));

    let strong = spans
        .iter()
        .find(|span| span.text == "*bold*")
        .expect("expected styled markdown span");
    assert!(strong.style.bold);

    let emphasis = spans
        .iter()
        .find(|span| span.text == "_italics_")
        .expect("expected styled markdown span");
    assert!(!emphasis.style.bold);
    assert!(emphasis.style.italic);

    let strikethrough = spans
        .iter()
        .find(|span| span.text == "~~deleted~~")
        .expect("expected styled markdown span");
    assert!(strikethrough.style.strikethrough);
}

/// Ensures reported emphasis forms map to italic and combined bold+italic
/// styling without stripping Markdown delimiters.
#[test]
fn final_render_styles_italic_and_bold_italic_emphasis() {
    let theme = markdown_test_theme();
    let source = "You can write text in **bold**, _italic_, or ***bold italic***.";
    let block = markdown_block(&theme, names::SHELL_OUTPUT, source);
    let spans = block.content.spans();

    assert_eq!(rendered_text(&block), source);

    let bold = spans
        .iter()
        .find(|span| span.text == "**bold**")
        .expect("expected styled markdown span");
    assert!(bold.style.bold);
    assert!(!bold.style.italic);

    let italic = spans
        .iter()
        .find(|span| span.text == "_italic_")
        .expect("expected styled markdown span");
    assert!(!italic.style.bold);
    assert!(italic.style.italic);

    let bold_italic = spans
        .iter()
        .find(|span| span.text == "***bold italic***")
        .expect("expected styled markdown span");
    assert!(bold_italic.style.bold);
    assert!(bold_italic.style.italic);
}

/// Ensures nested ordered list markers are list markers instead of indented
/// code.
#[test]
fn nested_ordered_list_items_are_not_indented_code() {
    let theme = markdown_test_theme();
    let block = markdown_block(
        &theme,
        names::SHELL_OUTPUT,
        "1. Parent item\n   - Child bullet\n     1. Nested numbered item\n     2. Another nested numbered item\n2. Second parent item",
    );
    let spans = block.content.spans();

    for marker in ["1.", "-", "2."] {
        let span = spans
            .iter()
            .find(|span| span.text == marker)
            .unwrap_or_else(|| panic!("missing marker span {marker}"));
        assert_eq!(span.style.fg, Some(tau_cli_term::Color::Green));
        assert_eq!(span.style.bg, None);
    }

    let nested_body = spans
        .iter()
        .find(|span| span.text.contains("Nested numbered item"))
        .expect("nested ordered item body");
    assert_eq!(nested_body.style.bg, None);
}

/// Ensures pipe tables remain Markdown tables while cells are padded for
/// display.
#[test]
fn markdown_tables_are_padded_without_changing_cell_text() {
    let theme = markdown_test_theme();
    let block = markdown_block(
        &theme,
        names::SHELL_OUTPUT,
        "| A | Longer |\n| --- | --- |\n| one | two |\n| three | four |\n",
    );

    assert_eq!(
        rendered_text(&block),
        "| A     | Longer |\n| ----- | ------ |\n| one   | two    |\n| three | four   |\n"
    );
}

/// Ensures table padding is not applied inside fenced code blocks.
#[test]
fn markdown_tables_inside_code_fences_are_not_padded() {
    let theme = markdown_test_theme();
    let source = "```\n| A | Longer |\n| --- | --- |\n```\n";
    let block = markdown_block(&theme, names::SHELL_OUTPUT, source);

    assert_eq!(rendered_text(&block), source);
}

/// Ensures alignment marker colons survive table separator padding.
#[test]
fn markdown_table_separator_alignment_is_preserved() {
    let theme = markdown_test_theme();
    let block = markdown_block(
        &theme,
        names::SHELL_OUTPUT,
        "| Left | Right | Center |\n| :--- | ---: | :---: |\n| a | b | c |\n",
    );

    assert_eq!(
        rendered_text(&block),
        "| Left | Right | Center |\n| :--- | ----: | :----: |\n| a    | b     | c      |\n"
    );
}

/// Ensures escaped pipes remain cell content instead of becoming separators.
#[test]
fn markdown_table_escaped_pipes_remain_cell_content() {
    let theme = markdown_test_theme();
    let block = markdown_block(
        &theme,
        names::SHELL_OUTPUT,
        "| Cell | Other |\n| --- | --- |\n| x\\|y | z |\n",
    );

    assert_eq!(
        rendered_text(&block),
        "| Cell | Other |\n| ---- | ----- |\n| x\\|y | z     |\n"
    );
}

/// Ensures pipes inside inline code spans remain cell content.
#[test]
fn markdown_table_code_span_pipes_remain_cell_content() {
    let theme = markdown_test_theme();
    let block = markdown_block(
        &theme,
        names::SHELL_OUTPUT,
        "| Cell | Other |\n| --- | --- |\n| `x|y` | z |\n",
    );

    assert_eq!(
        rendered_text(&block),
        "| Cell  | Other |\n| ----- | ----- |\n| `x|y` | z     |\n"
    );
}

/// Ensures indented pipe-shaped text remains code and is not table-padded.
#[test]
fn indented_pipe_tables_remain_code() {
    let theme = markdown_test_theme();
    let source = "    | A | Longer |\n    | --- | --- |\n";
    let block = markdown_block(&theme, names::SHELL_OUTPUT, source);

    assert_eq!(rendered_text(&block), source);
    assert!(block.content.spans().iter().any(|span| {
        span.style.bg
            == Some(tau_cli_term::Color::Rgb {
                r: 0x11,
                g: 0x11,
                b: 0x11,
            })
    }));
}

/// Ensures ambiguous no-leading-pipe tables are left unchanged.
#[test]
fn no_leading_pipe_tables_are_left_unchanged() {
    let theme = markdown_test_theme();
    let source = "   A | Longer\n   --- | ---\n";
    let block = markdown_block(&theme, names::SHELL_OUTPUT, source);

    assert_eq!(rendered_text(&block), source);
}

/// Ensures pathological wide tables fall back to source text instead of
/// expanding output.
#[test]
fn very_wide_tables_are_not_padded() {
    let theme = markdown_test_theme();
    let wide = "x".repeat(TABLE_MAX_CELL_WIDTH + 1);
    let source = format!("| A | B |\n| --- | --- |\n| {wide} | y |\n| z | q |\n");
    let block = markdown_block(&theme, names::SHELL_OUTPUT, &source);

    assert_eq!(rendered_text(&block), source);
}

/// Ensures tables with too many columns fall back to source text.
#[test]
fn too_many_table_columns_are_not_padded() {
    let theme = markdown_test_theme();
    let header = format!(
        "| {} |\n",
        (0..=TABLE_MAX_COLUMNS)
            .map(|_| "H")
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let separator = format!(
        "| {} |\n",
        (0..=TABLE_MAX_COLUMNS)
            .map(|_| "---")
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let row = format!(
        "| {} |\n",
        (0..=TABLE_MAX_COLUMNS)
            .map(|_| "x")
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let source = format!("{header}{separator}{row}");
    let block = markdown_block(&theme, names::SHELL_OUTPUT, &source);

    assert_eq!(rendered_text(&block), source);
}

/// Ensures rendered-line byte limits fall back to source text independently of
/// the per-cell width limit.
#[test]
fn too_long_rendered_table_lines_are_not_padded() {
    let theme = markdown_test_theme();
    let medium = "x".repeat((TABLE_MAX_RENDERED_LINE_BYTES / TABLE_MAX_COLUMNS) + 1);
    let header = format!(
        "| {} |\n",
        (0..TABLE_MAX_COLUMNS)
            .map(|_| medium.as_str())
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let separator = format!(
        "| {} |\n",
        (0..TABLE_MAX_COLUMNS)
            .map(|_| "---")
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let row = format!(
        "| {} |\n",
        (0..TABLE_MAX_COLUMNS)
            .map(|_| "x")
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let source = format!("{header}{separator}{row}");
    let block = markdown_block(&theme, names::SHELL_OUTPUT, &source);

    assert_eq!(rendered_text(&block), source);
}

/// Ensures aggregate padding limits fall back even when each rendered line is
/// individually within bounds.
#[test]
fn too_much_total_table_padding_is_not_padded() {
    let theme = markdown_test_theme();
    let wide = "x".repeat(TABLE_MAX_CELL_WIDTH);
    let mut source = format!("| {wide} | {wide} |\n| --- | --- |\n");
    let short_rows = (TABLE_MAX_EXTRA_PADDING_BYTES / TABLE_MAX_CELL_WIDTH) + 1;
    for _ in 0..short_rows {
        source.push_str("| a | b |\n");
    }
    let block = markdown_block(&theme, names::SHELL_OUTPUT, &source);

    assert_eq!(rendered_text(&block), source);
}

/// Ensures live sealed table chunks are padded once they become stable.
#[test]
fn live_stream_pads_sealed_markdown_tables() {
    let theme = markdown_test_theme();
    let mut cache = MarkdownStreamCache::default();
    let source = "| A | Longer |\n| --- | --- |\n| one | two |\n\n";
    let block = markdown_streaming_block(&theme, names::SHELL_OUTPUT, source, &mut cache);

    assert_eq!(
        rendered_text(&block),
        "| A   | Longer |\n| --- | ------ |\n| one | two    |\n\n…"
    );
}

/// Ensures unmatched, escaped, identifier, and code-like delimiters do not
/// style accidentally.
#[test]
fn inline_parser_avoids_common_false_positives() {
    let theme = markdown_test_theme();
    let block = markdown_block(
        &theme,
        names::SHELL_OUTPUT,
        "foo_bar_baz \\*literal\\* \\~~deleted\\~~ `~~code~~`\n```\n~~code~~\n```\n~~open",
    );

    let spans = block.content.spans();
    for span in spans {
        assert!(!span.style.bold, "unexpected bold span: {span:?}");
        assert!(!span.style.italic, "unexpected italic span: {span:?}");
        assert!(
            !span.style.strikethrough,
            "unexpected strikethrough span: {span:?}"
        );
    }
    let escape = spans
        .iter()
        .find(|span| span.text == "\\*")
        .expect("escaped marker span");
    assert!(escape.style.bg.is_some());

    let tilde_escape = spans
        .iter()
        .find(|span| span.text == "\\~")
        .expect("escaped tilde marker span");
    assert!(tilde_escape.style.bg.is_some());

    let inline_code = spans
        .iter()
        .find(|span| span.text == "`~~code~~`")
        .expect("inline code span");
    assert!(inline_code.style.bg.is_some());
}

/// Ensures live rendering formats completed lines before a blank-line seal,
/// while still leaving the current incomplete streamed line plain.
#[test]
fn live_stream_formats_complete_lines_and_leaves_current_line_plain() {
    let theme = markdown_test_theme();
    let mut cache = MarkdownStreamCache::default();

    let block = markdown_streaming_block(&theme, names::SHELL_OUTPUT, "*bold*", &mut cache);
    let bold = block
        .content
        .spans()
        .iter()
        .find(|span| span.text == "*bold*")
        .expect("expected styled markdown span");
    assert!(!bold.style.bold);

    let block = markdown_streaming_block(&theme, names::SHELL_OUTPUT, "*bold*\nnext", &mut cache);
    let bold = block
        .content
        .spans()
        .iter()
        .find(|span| span.text == "*bold*")
        .expect("expected styled markdown span");
    assert!(bold.style.bold);
    let next = block
        .content
        .spans()
        .iter()
        .find(|span| span.text == "next")
        .expect("expected styled markdown span");
    assert!(!next.style.bold);

    let block = markdown_streaming_block(&theme, names::SHELL_OUTPUT, "*bold*\n\nnext", &mut cache);
    let bold = block
        .content
        .spans()
        .iter()
        .find(|span| span.text == "*bold*")
        .expect("expected styled markdown span");
    assert!(bold.style.bold);
    let next = block
        .content
        .spans()
        .iter()
        .find(|span| span.text == "next")
        .expect("expected styled markdown span");
    assert!(!next.style.bold);
}

/// Ensures the live parser applies line-level and inline Markdown-lite styling
/// to newline-terminated text even before a blank line finalizes the block.
#[test]
fn live_stream_formats_complete_lines_before_blank_line() {
    let theme = markdown_test_theme();
    let mut cache = MarkdownStreamCache::default();
    let block = markdown_streaming_block(
        &theme,
        names::SHELL_OUTPUT,
        "# Heading\n- *bold*\nplain",
        &mut cache,
    );
    let spans = block.content.spans();

    let heading = spans
        .iter()
        .find(|span| span.text == "# Heading")
        .expect("heading span");
    assert!(heading.style.underline);
    let marker = spans
        .iter()
        .find(|span| span.text == "-")
        .expect("list marker span");
    assert_eq!(marker.style.fg, Some(tau_cli_term::Color::Green));
    let bold = spans
        .iter()
        .find(|span| span.text == "*bold*")
        .expect("bold span");
    assert!(bold.style.bold);
    let plain = spans
        .iter()
        .find(|span| span.text == "plain")
        .expect("incomplete line span");
    assert!(!plain.style.bold);
    assert!(!plain.style.underline);
}

/// Ensures a syntactically complete-looking inline marker on the final
/// no-newline line does not receive provisional streaming styles.
#[test]
fn live_stream_keeps_incomplete_current_line_plain() {
    let theme = markdown_test_theme();
    let mut cache = MarkdownStreamCache::default();
    let block = markdown_streaming_block(&theme, names::SHELL_OUTPUT, "_maybe_", &mut cache);
    let span = block
        .content
        .spans()
        .iter()
        .find(|span| span.text == "_maybe_")
        .expect("incomplete line span");

    assert!(!span.style.italic);
}

/// Ensures an opening code fence starts provisional code highlighting as soon
/// as the fence line and following code line are complete, even before a
/// closing fence or blank-line seal arrives.
#[test]
fn live_stream_highlights_unclosed_fenced_code_after_newline() {
    let theme = markdown_test_theme();
    let mut cache = MarkdownStreamCache::default();
    let block = markdown_streaming_block(
        &theme,
        names::SHELL_OUTPUT,
        "```rust\nlet x = 1;\n",
        &mut cache,
    );
    let spans = block.content.spans();
    for text in ["```rust", "let x = 1;"] {
        let span = spans
            .iter()
            .find(|span| span.text == text)
            .unwrap_or_else(|| panic!("missing code span {text}"));
        assert_eq!(
            span.style.bg,
            Some(tau_cli_term::Color::Rgb {
                r: 0x11,
                g: 0x11,
                b: 0x11,
            })
        );
    }
}

/// Documents that live tables use the same provisional padding as final
/// tables, so later wider rows can still alter earlier table display widths.
#[test]
fn live_stream_pads_complete_table_lines_before_blank_line() {
    let theme = markdown_test_theme();
    let mut cache = MarkdownStreamCache::default();
    let source = "| A | Longer |\n| --- | --- |\n| one | two |\nnext";
    let block = markdown_streaming_block(&theme, names::SHELL_OUTPUT, source, &mut cache);

    assert_eq!(
        rendered_text(&block),
        "| A   | Longer |\n| --- | ------ |\n| one | two    |\nnext …"
    );
}

/// Ensures non-append provider replacements reset the streaming cache
/// safely, including any provisional live-block parse.
#[test]
fn live_stream_cache_resets_on_replacement() {
    let theme = markdown_test_theme();
    let mut cache = MarkdownStreamCache::default();
    let _ = markdown_streaming_block(&theme, names::SHELL_OUTPUT, "*old*\n_live_\n", &mut cache);
    let block = markdown_streaming_block(&theme, names::SHELL_OUTPUT, "_new_\n\n", &mut cache);

    assert_eq!(rendered_text(&block), "_new_\n\n…");
    let emphasis = block
        .content
        .spans()
        .iter()
        .find(|span| span.text == "_new_")
        .expect("expected styled markdown span");
    assert!(!emphasis.style.bold);
    assert!(emphasis.style.italic);
}

/// Ensures a same-shape non-append replacement cannot reuse the prior
/// provisional live parse when the completed-line boundary is unchanged.
#[test]
fn live_stream_replacement_reparses_same_boundary_live_lines() {
    let theme = markdown_test_theme();
    let mut cache = MarkdownStreamCache::default();
    let _ = markdown_streaming_block(&theme, names::SHELL_OUTPUT, "*old*\n", &mut cache);
    let block = markdown_streaming_block(&theme, names::SHELL_OUTPUT, "_new_\n", &mut cache);
    let spans = block.content.spans();

    assert_eq!(rendered_text(&block), "_new_\n…");
    assert!(!spans.iter().any(|span| span.text == "*old*"));
    let emphasis = spans
        .iter()
        .find(|span| span.text == "_new_")
        .expect("replacement emphasis span");
    assert!(emphasis.style.italic);
}

/// Ensures submitted prompt prefixes keep prompt-marker semantics instead
/// of inheriting the Markdown list-marker style.
#[test]
fn prompt_marker_uses_submitted_marker_style() {
    let theme = markdown_test_theme();
    let block = markdown_prompt_block(&theme, names::USER_PROMPT, "> ".to_owned(), "- item");
    let spans = block.content.spans();

    let prompt_marker = spans
        .iter()
        .find(|span| span.text == "> ")
        .expect("expected styled markdown span");
    assert_eq!(prompt_marker.style.fg, Some(tau_cli_term::Color::Red));

    let list_marker = spans
        .iter()
        .find(|span| span.text == "-")
        .expect("expected styled markdown span");
    assert_eq!(list_marker.style.fg, Some(tau_cli_term::Color::Green));
}

/// Ensures the live cache carries fenced-code parser state across sealed
/// chunks split by blank lines inside the fence.
#[test]
fn live_stream_preserves_fence_state_across_blank_lines() {
    let theme = markdown_test_theme();
    let mut cache = MarkdownStreamCache::default();
    let _ = markdown_streaming_block(&theme, names::SHELL_OUTPUT, "```\n\n", &mut cache);
    let block = markdown_streaming_block(
        &theme,
        names::SHELL_OUTPUT,
        "```\n\n*not bold*\n\n",
        &mut cache,
    );
    let code = block
        .content
        .spans()
        .iter()
        .find(|span| span.text.contains("*not bold*"))
        .expect("code text span after second update");
    assert!(!code.style.bold);

    let block = markdown_streaming_block(
        &theme,
        names::SHELL_OUTPUT,
        "```\n\n*not bold*\n\n```\n\n*bold*\n\n",
        &mut cache,
    );

    let code = block
        .content
        .spans()
        .iter()
        .find(|span| span.text.contains("*not bold*"))
        .expect("code text span");
    assert!(!code.style.bold);

    let bold = block
        .content
        .spans()
        .iter()
        .find(|span| span.text == "*bold*")
        .expect("post-fence bold span");
    assert!(bold.style.bold);
}
