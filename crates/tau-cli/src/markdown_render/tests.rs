use std::env::VarError;

use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, TestCaseError, TestRunner};

use super::*;

fn markdown_test_theme() -> tau_themes::Theme {
    tau_themes::Theme::parse(
        r##"{
            styles: {
                "shell.output": { },
                "user.prompt": { fg: "magenta", bg: "#101010" },
                "agent.response": { fg: "cyan", bg: "#101010" },
                "prompt.marker.submitted": { fg: "red" },
                "markdown.strong": { bold: true, underline: true },
                "markdown.emphasis": { italic: true },
                "markdown.strikethrough": { fg: "gray", strikethrough: true },
                "markdown.heading": { bold: true, underline: true },
                "markdown.list.marker": { bold: true },
                "markdown.code": { bg: "#111111" },
                "markdown.escape": { bg: "#222222" },
                "markdown.link": { fg: "red", bold: true },
                "progress.indicator": { fg: "cyan" },
            }
        }"##,
    )
    .expect("valid markdown test theme")
}

fn arbitrary_markdown_source(max_chars: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 0..=max_chars)
        .prop_map(|characters| characters.into_iter().collect())
}

fn delimiter_heavy_markdown_source(max_tokens: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            Just(" "),
            Just("\t"),
            Just("\n"),
            Just("*"),
            Just("**"),
            Just("***"),
            Just("****"),
            Just("_"),
            Just("__"),
            Just("~~"),
            Just("`"),
            Just("```"),
            Just("~~~"),
            Just("\\"),
            Just("["),
            Just("]"),
            Just("("),
            Just(")"),
            Just("<"),
            Just(">"),
            Just("|"),
            Just("#"),
            Just("- "),
            Just("---"),
            Just(":---:"),
            Just("| heading | value |\n| --- | :---: |\n| cell | row |\n"),
            Just("https://example.test/path"),
            Just("é"),
        ],
        0..=max_tokens,
    )
    .prop_map(|tokens| tokens.concat())
}

fn parse_heavy_fuzz_cases(value: Option<&str>) -> Result<u32, String> {
    let Some(value) = value else {
        return Ok(1_000);
    };
    let cases = value
        .parse()
        .map_err(|_| "TAU_MARKDOWN_FUZZ_CASES must be a positive u32".to_owned())?;
    if cases == 0 {
        return Err("TAU_MARKDOWN_FUZZ_CASES must be a positive u32".to_owned());
    }
    Ok(cases)
}

fn heavy_fuzz_cases() -> Result<u32, String> {
    match std::env::var("TAU_MARKDOWN_FUZZ_CASES") {
        Ok(value) => parse_heavy_fuzz_cases(Some(&value)),
        Err(VarError::NotPresent) => parse_heavy_fuzz_cases(None),
        Err(VarError::NotUnicode(_)) => {
            Err("TAU_MARKDOWN_FUZZ_CASES must be valid Unicode".to_owned())
        }
    }
}

fn normalized_spans(
    spans: &[tau_cli_term::Span],
) -> Vec<(String, tau_cli_term::Style, Option<std::sync::Arc<str>>)> {
    let mut normalized: Vec<(String, tau_cli_term::Style, Option<std::sync::Arc<str>>)> =
        Vec::new();
    for span in spans {
        if let Some((text, style, hyperlink)) = normalized.last_mut()
            && *style == span.style
            && *hyperlink == span.hyperlink
        {
            text.push_str(&span.text);
        } else {
            normalized.push((span.text.clone(), span.style, span.hyperlink.clone()));
        }
    }
    normalized
}

/// Returns the terminal display columns occupied by unescaped structural pipes.
fn table_pipe_display_columns(line: &str) -> Vec<usize> {
    line.char_indices()
        .filter_map(|(index, character)| (character == '|').then_some(index))
        .map(|index| tau_term_screen::display_width(&line[..index]))
        .collect()
}

/// Ensures each projected row keeps every structural pipe in the same columns.
fn assert_table_pipe_columns_align(text: &str) {
    let mut rows = text.lines();
    let expected = table_pipe_display_columns(rows.next().expect("table header"));
    for row in rows {
        assert_eq!(
            table_pipe_display_columns(row),
            expected,
            "unaligned table row: {row:?}"
        );
    }
}

fn assert_markdown_rendering_property(
    theme: &tau_themes::Theme,
    source: &str,
) -> Result<(), TestCaseError> {
    let _ = markdown_block(theme, names::AGENT_RESPONSE, source);

    let mut cache = MarkdownStreamCache::default();
    for end in source
        .char_indices()
        .map(|(index, character)| index + character.len_utf8())
    {
        let _ = markdown_streaming_block(theme, names::AGENT_RESPONSE, &source[..end], &mut cache);
    }

    let completed = format!("{source}\n\n");
    let static_block = markdown_block(theme, names::AGENT_RESPONSE, &completed);
    let streaming_block =
        markdown_streaming_block(theme, names::AGENT_RESPONSE, &completed, &mut cache);
    let static_spans = static_block.content.spans();
    let streaming_spans = streaming_block.content.spans();
    let Some((progress, streaming_body)) = streaming_spans.split_last() else {
        return Err(TestCaseError::fail(
            "streaming rendering must include progress",
        ));
    };
    prop_assert_eq!(
        normalized_spans(streaming_body),
        normalized_spans(static_spans)
    );
    prop_assert_eq!(&progress.text, tau_proto::PROGRESS_INDICATOR_TEXT);
    prop_assert_eq!(
        progress.style,
        tau_cli_term::Style {
            fg: Some(tau_cli_term::Color::Cyan),
            ..tau_cli_term::Style::default()
        }
    );
    prop_assert_eq!(&progress.hyperlink, &None);
    Ok(())
}

/// Rejects invalid local fuzz-depth overrides before creating the heavy runner,
/// so a typo cannot silently select a different stress workload.
#[test]
fn markdown_heavy_fuzz_cases_reject_invalid_values() {
    assert_eq!(parse_heavy_fuzz_cases(None), Ok(1_000));
    assert_eq!(parse_heavy_fuzz_cases(Some("42")), Ok(42));
    assert!(parse_heavy_fuzz_cases(Some("0")).is_err());
    assert!(parse_heavy_fuzz_cases(Some("many")).is_err());
    assert!(parse_heavy_fuzz_cases(Some("4294967296")).is_err());
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        max_shrink_iters: 0,
        .. ProptestConfig::default()
    })]

    /// Exercises arbitrary bounded Unicode through static and append-only
    /// Markdown rendering, ensuring malformed text cannot panic and sealed
    /// streaming output retains its static rendering before the progress marker.
    #[test]
    fn arbitrary_markdown_rendering_is_panic_free(source in arbitrary_markdown_source(96)) {
        let theme = markdown_test_theme();
        assert_markdown_rendering_property(&theme, &source)?;
    }

    /// Targets the delimiter combinations that drive the Markdown-lite parser,
    /// guarding against malformed runs, escapes, links, fences, and tables
    /// panicking in either static or streaming rendering.
    #[test]
    fn delimiter_heavy_markdown_rendering_is_panic_free(
        source in delimiter_heavy_markdown_source(128)
    ) {
        let theme = markdown_test_theme();
        assert_markdown_rendering_property(&theme, &source)?;
    }
}

/// Runs a deeper reusable fuzz workload over both arbitrary Unicode and
/// delimiter-heavy Markdown. Ordinary nextest and selfci runs compile this
/// ignored test but do not execute it; run
/// `TAU_MARKDOWN_FUZZ_CASES=20000 cargo test -p dpc-tau-cli
/// markdown_heavy_fuzz_harness -- --ignored` locally when deliberately
/// stress-testing the renderer. The default is 1,000 cases.
#[test]
#[ignore = "heavy Markdown fuzz workload is compile-only in ordinary CI"]
fn markdown_heavy_fuzz_harness() {
    let strategy = prop_oneof![
        arbitrary_markdown_source(256),
        delimiter_heavy_markdown_source(384),
    ];
    let mut runner = TestRunner::new(ProptestConfig {
        cases: heavy_fuzz_cases().expect("TAU_MARKDOWN_FUZZ_CASES must be valid and positive"),
        max_shrink_iters: 0,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    let theme = markdown_test_theme();

    runner
        .run(&strategy, |source| {
            assert_markdown_rendering_property(&theme, &source)
        })
        .expect("heavy Markdown fuzz workload must preserve static and streaming rendering");
}

/// Markdown links keep only their labels while inline, autolink, bare,
/// multiple, and Unicode forms carry their exact destinations as OSC 8
/// metadata.
#[test]
fn markdown_links_render_labels_and_targets() {
    let theme = markdown_test_theme();
    let block = markdown_block(
        &theme,
        names::AGENT_RESPONSE,
        "See [docs](https://example.test/a) and <https://例.test/路> or https://two.test/x.",
    );

    assert_eq!(
        rendered_text(&block),
        "See docs and https://例.test/路 or https://two.test/x."
    );
    let links: Vec<_> = block
        .content
        .spans()
        .iter()
        .filter_map(|span| {
            span.hyperlink
                .as_deref()
                .map(|target| (span.text.as_str(), target))
        })
        .collect();
    assert_eq!(
        links,
        [
            ("docs", "https://example.test/a"),
            ("https://例.test/路", "https://例.test/路"),
            ("https://two.test/x", "https://two.test/x"),
        ]
    );
    let docs = block
        .content
        .spans()
        .iter()
        .find(|span| span.text == "docs")
        .expect("link label span");
    assert!(docs.style.bold);
    assert_eq!(docs.style.fg, Some(tau_cli_term::Color::Red));
    assert_eq!(
        docs.style.bg,
        Some(tau_cli_term::Color::Rgb {
            r: 0x10,
            g: 0x10,
            b: 0x10,
        })
    );
}

/// Links nested in supported emphasis delimiters retain both the surrounding
/// semantic style and their exact sanitized OSC 8 targets.
#[test]
fn markdown_links_compose_with_surrounding_inline_styles() {
    let theme = markdown_test_theme();
    let block = markdown_block(
        &theme,
        names::AGENT_RESPONSE,
        "**https://strong.test/x** _[label](https://emphasis.test/y)_ \
         ~~<https://strike.test/z>~~ ***https://both.test/q***",
    );
    let linked: Vec<_> = block
        .content
        .spans()
        .iter()
        .filter_map(|span| {
            span.hyperlink.as_deref().map(|target| {
                (
                    span.text.as_str(),
                    target,
                    span.style.bold,
                    span.style.italic,
                    span.style.strikethrough,
                )
            })
        })
        .collect();

    assert_eq!(
        rendered_text(&block),
        "**https://strong.test/x** _label_ ~~https://strike.test/z~~ \
         ***https://both.test/q***"
    );
    assert_eq!(
        linked,
        [
            (
                "https://strong.test/x",
                "https://strong.test/x",
                true,
                false,
                false,
            ),
            ("label", "https://emphasis.test/y", true, true, false,),
            (
                "https://strike.test/z",
                "https://strike.test/z",
                true,
                false,
                true,
            ),
            (
                "https://both.test/q",
                "https://both.test/q",
                true,
                true,
                false,
            ),
        ]
    );
    let strong_link = block
        .content
        .spans()
        .iter()
        .find(|span| span.hyperlink.as_deref() == Some("https://strong.test/x"))
        .expect("strong nested link");
    assert!(strong_link.style.underline);
    let strike_link = block
        .content
        .spans()
        .iter()
        .find(|span| span.hyperlink.as_deref() == Some("https://strike.test/z"))
        .expect("strikethrough nested link");
    assert_eq!(strike_link.style.fg, Some(tau_cli_term::Color::Red));
}

/// Inline code nested in emphasis remains non-clickable even when it contains a
/// URL, preventing the nested-link scan from weakening code suppression.
#[test]
fn markdown_nested_code_still_suppresses_links() {
    let theme = markdown_test_theme();
    let source = "**`https://not-a-link.test`**";
    let block = markdown_block(&theme, names::AGENT_RESPONSE, source);

    assert_eq!(rendered_text(&block), source);
    assert!(
        block
            .content
            .spans()
            .iter()
            .all(|span| span.hyperlink.is_none())
    );
}

/// Style delimiters inside a code span stay opaque to the outer delimiter
/// matcher, so they cannot expose a later URL as a link.
#[test]
fn markdown_outer_style_skips_code_delimiters() {
    let theme = markdown_test_theme();
    let source = "**`code https://not-a-link.test ** tail`**";
    let block = markdown_block(&theme, names::AGENT_RESPONSE, source);

    assert_eq!(rendered_text(&block), source);
    assert!(
        block
            .content
            .spans()
            .iter()
            .all(|span| span.hyperlink.is_none())
    );
}

/// Style delimiters inside an explicit link target stay opaque, preserving one
/// complete label hyperlink instead of exposing a partial bare URL.
#[test]
fn markdown_outer_style_skips_explicit_link_target_delimiters() {
    let theme = markdown_test_theme();
    let block = markdown_block(
        &theme,
        names::AGENT_RESPONSE,
        "**[label](https://example.test/a**b)**",
    );
    let links: Vec<_> = block
        .content
        .spans()
        .iter()
        .filter_map(|span| {
            span.hyperlink
                .as_deref()
                .map(|target| (span.text.as_str(), target))
        })
        .collect();

    assert_eq!(links, [("label", "https://example.test/a**b")]);
}

/// Disabling OSC 8 on a nested explicit link retains its enclosing style while
/// exposing the target exactly once for terminal URL detection.
#[test]
fn markdown_nested_explicit_link_exposes_target_without_osc8() {
    let theme = markdown_test_theme();
    let block = markdown_block_with_osc8(
        &theme,
        names::AGENT_RESPONSE,
        "**[label](https://example.test/x)**",
        false,
    );

    assert_eq!(rendered_text(&block), "**label (https://example.test/x)**");
    let label = block
        .content
        .spans()
        .iter()
        .find(|span| span.text.contains("label"))
        .expect("visible nested link");
    assert!(label.style.underline);
    assert!(label.hyperlink.is_none());
}

/// Disabling OSC 8 exposes the destination for terminal URL detection, removes
/// hyperlink metadata, and leaves malformed or escaped syntax literal.
#[test]
fn markdown_links_can_disable_osc8_and_leave_invalid_syntax_literal() {
    let theme = markdown_test_theme();
    let block = markdown_block_with_osc8(
        &theme,
        names::AGENT_RESPONSE,
        "[label](https://example.test) [broken](target \\[escaped](url)",
        false,
    );

    assert_eq!(
        rendered_text(&block),
        "label (https://example.test) [broken](target \\[escaped](url)"
    );
    assert!(
        block
            .content
            .spans()
            .iter()
            .all(|span| span.hyperlink.is_none())
    );
}

/// Angle-delimited link destinations preserve reserved URL parentheses while
/// producing the exact canonical target in OSC 8 metadata.
#[test]
fn markdown_angle_link_destination_preserves_exact_parentheses() {
    let theme = markdown_test_theme();
    let target = "https://example.test/a)b(c";
    let source = format!("[label](<{target}>)");
    let block = markdown_block_with_osc8(&theme, names::AGENT_RESPONSE, &source, true);
    assert_eq!(rendered_text(&block), "label");
    assert_eq!(
        block
            .content
            .spans()
            .iter()
            .find_map(|span| span.hyperlink.as_deref()),
        Some(target)
    );
}

/// Bare URLs require token boundaries and useful bodies, while autolinks reject
/// whitespace instead of turning malformed angle-bracket text into a link.
#[test]
fn markdown_url_recognition_rejects_malformed_and_embedded_forms() {
    let theme = markdown_test_theme();
    let source = "abchttps://host http:// <https://host path> (https://valid.test/x)";
    let block = markdown_block(&theme, names::AGENT_RESPONSE, source);

    assert_eq!(rendered_text(&block), source);
    let links: Vec<_> = block
        .content
        .spans()
        .iter()
        .filter_map(|span| span.hyperlink.as_deref())
        .collect();
    assert_eq!(links, ["https://valid.test/x"]);
}

/// A bare URL inside an outer style uses that style's local boundaries and
/// cannot scan through the closing delimiter into following transcript text.
#[test]
fn markdown_nested_bare_url_stays_inside_outer_style_range() {
    let theme = markdown_test_theme();
    let source = "**x https://inside.test/a** outside";
    let block = markdown_block(&theme, names::AGENT_RESPONSE, source);
    let links: Vec<_> = block
        .content
        .spans()
        .iter()
        .filter_map(|span| {
            span.hyperlink
                .as_deref()
                .map(|target| (span.text.as_str(), target))
        })
        .collect();

    assert_eq!(rendered_text(&block), source);
    assert_eq!(links, [("https://inside.test/a", "https://inside.test/a")]);
}

/// A failed outer URL may advance the shared terminator cursor past a later
/// styled range; the nested parse must clamp that cursor to its local boundary.
#[test]
fn markdown_nested_bare_url_clamps_retained_terminator_cursor() {
    let theme = markdown_test_theme();
    let source = "http://a**https://nested.test/x**\u{0001}";
    let block = markdown_block(&theme, names::AGENT_RESPONSE, source);
    let links: Vec<_> = block
        .content
        .spans()
        .iter()
        .filter_map(|span| span.hyperlink.as_deref())
        .collect();

    assert_eq!(rendered_text(&block), source);
    assert_eq!(links, ["https://nested.test/x"]);
}

/// Link metadata survives narrow wrapping and never extends to surrounding
/// text.
#[test]
fn markdown_link_boundaries_survive_wrapping() {
    let theme = markdown_test_theme();
    let block = markdown_block(
        &theme,
        names::AGENT_RESPONSE,
        "a [long label](https://example.test) z",
    );
    let lines = tau_term_screen::layout_lines()
        .content(&block.content)
        .width(4)
        .call();
    let linked: String = lines
        .iter()
        .flatten()
        .filter(|cell| cell.hyperlink.is_some())
        .map(|cell| cell.ch)
        .collect();

    assert_eq!(linked, "long label");
    let unlinked: String = lines
        .iter()
        .flatten()
        .filter(|cell| cell.hyperlink.is_none())
        .map(|cell| cell.ch)
        .collect();
    assert_eq!(unlinked, "a  z");
}

/// A linked wide grapheme replaced at terminal width one retains its target.
#[test]
fn markdown_unicode_link_survives_one_column_layout() {
    let theme = markdown_test_theme();
    let block = markdown_block(
        &theme,
        names::AGENT_RESPONSE,
        "[界](https://example.test/wide)",
    );
    let lines = tau_term_screen::layout_lines()
        .content(&block.content)
        .width(1)
        .call();

    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0][0].ch, '�');
    assert_eq!(
        lines[0][0].hyperlink.as_deref(),
        Some("https://example.test/wide")
    );
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
    assert!(marker.style.bold);

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

/// Response state markers must stay outside Markdown parsing so a first-column
/// heading keeps its structural styling in both final and streaming blocks.
#[test]
fn response_prefix_preserves_first_column_markdown_structure() {
    let theme = markdown_test_theme();
    let mut cache = MarkdownStreamCache::default();
    let blocks = [
        markdown_prefixed_block(&theme, names::AGENT_RESPONSE, "◆ ", "# heading"),
        markdown_prefixed_streaming_block(
            &theme,
            names::AGENT_RESPONSE,
            "◇ ",
            "# heading\n",
            &mut cache,
        ),
    ];

    for (block, marker) in blocks.iter().zip(["◆ ", "◇ "]) {
        let spans = block.content.spans();
        let marker = spans
            .iter()
            .find(|span| span.text == marker)
            .expect("response state marker");
        assert!(!marker.style.bold);
        assert!(!marker.style.underline);

        let heading = spans
            .iter()
            .find(|span| span.text == "# heading")
            .expect("first-column Markdown heading");
        assert!(heading.style.bold);
        assert!(heading.style.underline);
    }
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
        assert_eq!(span.style.fg, None);
        assert_eq!(span.style.bg, None);
    }

    let nested_body = spans
        .iter()
        .find(|span| span.text.contains("Nested numbered item"))
        .expect("nested ordered item body");
    assert_eq!(nested_body.style.bg, None);
}

/// Ensures structural Markdown emphasis inherits custom user and assistant
/// foregrounds/backgrounds while adjacent text proves modifiers do not leak.
#[test]
fn structural_emphasis_preserves_transcript_base_styles() {
    let theme = markdown_test_theme();
    for (base, expected_fg) in [
        (names::USER_PROMPT, tau_cli_term::Color::Magenta),
        (names::AGENT_RESPONSE, tau_cli_term::Color::Cyan),
    ] {
        let block = markdown_block(&theme, base, "# Heading\n12. item\nplain");
        let spans = block.content.spans();
        for text in ["# Heading", "12."] {
            let span = spans
                .iter()
                .find(|span| span.text == text)
                .unwrap_or_else(|| panic!("missing structural span {text}"));
            assert_eq!(span.style.fg, Some(expected_fg));
            assert_eq!(
                span.style.bg,
                Some(tau_cli_term::Color::Rgb {
                    r: 0x10,
                    g: 0x10,
                    b: 0x10,
                })
            );
            assert!(span.style.bold);
        }
        let plain = spans
            .iter()
            .find(|span| span.text.contains(" item") && span.text.contains("plain"))
            .expect("list body and following plain text");
        assert_eq!(plain.style.fg, Some(expected_fg));
        assert!(!plain.style.bold);
        assert!(!plain.style.underline);
    }
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

/// Ensures a pipe-shaped row with more cells ends a valid table instead of
/// invalidating its earlier rows after streaming has already rendered them.
#[test]
fn markdown_table_cell_count_mismatch_seals_prior_table() {
    let theme = markdown_test_theme();
    let source = concat!(
        "| heading | value |\n",
        "| --- | :---: |\n",
        "| cell | row |\n",
        "| mismatched | cells | here |\n",
    );
    let static_block = markdown_block(&theme, names::SHELL_OUTPUT, source);

    assert_eq!(
        rendered_text(&static_block),
        concat!(
            "| heading | value |\n",
            "| ------- | :---: |\n",
            "| cell    |  row  |\n",
            "| mismatched | cells | here |\n",
        )
    );
    assert_markdown_rendering_property(&theme, source)
        .expect("sealed streaming table must retain its static projection");
}

/// Reproduces the reported ordinary prose table, which previously fell back at
/// 80 scalar characters, and keeps its numeric-looking effort column right
/// aligned in one bounded 139-column logical table.
#[test]
fn reported_markdown_table_aligns_long_scope_and_right_effort() {
    let theme = markdown_test_theme();
    let source = concat!(
        "| Scope | NativeReasoningEffort |\n",
        "| --- | ---: |\n",
        "| Formed 7-guardian federation, connected gateway, configured/advertising FLIP, log paths, working `fman-cli` | **4–7 engineer-days** |\n",
        "| Complete FI-requested/funded liquidity and register the gateway in federation consensus | **8–15 days total** |\n",
        "| Real `cloud-fman-telemetry` collection | **+3–6 days** |\n",
        "| Throwaway demo script | **2–3 days**, but brittle |\n",
    );
    let block = markdown_block(&theme, names::SHELL_OUTPUT, source);

    assert_eq!(
        rendered_text(&block),
        concat!(
            "| Scope                                                                                                       |     NativeReasoningEffort |\n",
            "| ----------------------------------------------------------------------------------------------------------- | ------------------------: |\n",
            "| Formed 7-guardian federation, connected gateway, configured/advertising FLIP, log paths, working `fman-cli` |     **4–7 engineer-days** |\n",
            "| Complete FI-requested/funded liquidity and register the gateway in federation consensus                     |       **8–15 days total** |\n",
            "| Real `cloud-fman-telemetry` collection                                                                      |             **+3–6 days** |\n",
            "| Throwaway demo script                                                                                       | **2–3 days**, but brittle |\n",
        )
    );
    assert_table_pipe_columns_align(&rendered_text(&block));
    assert_eq!(
        tau_term_screen::display_width(
            rendered_text(&block)
                .lines()
                .next()
                .expect("reported table header")
        ),
        139
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

/// Ensures delimiter markers select left, right, and deterministic odd-center
/// alignment while preserving their colons.
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
        "| Left | Right | Center |\n| :--- | ----: | :----: |\n| a    |     b |   c    |\n"
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

/// Ensures table columns use the terminal's grapheme-aware display width rather
/// than Unicode scalar counts for CJK, emoji, and combining sequences.
#[test]
fn markdown_table_unicode_cells_align_by_display_width() {
    let theme = markdown_test_theme();
    let block = markdown_block(
        &theme,
        names::SHELL_OUTPUT,
        "| Text | Amount |\n| --- | ---: |\n| 中 | 1 |\n| 👨‍👩‍👧‍👦 | 22 |\n| e\u{301} | 333 |\n",
    );

    let rendered = rendered_text(&block);
    assert_table_pipe_columns_align(&rendered);
    assert!(
        rendered.contains("| 中   |      1 |"),
        "CJK cell receives display-column padding: {rendered:?}"
    );
}

/// Ensures table width follows the same explicit-link projection as span
/// emission for both OSC 8 modes without changing hyperlink metadata rules.
#[test]
fn markdown_table_links_measure_visible_osc8_projection() {
    let theme = markdown_test_theme();
    let source = "| Link | Value |\n| --- | ---: |\n| [label](https://example.test/target) | 1 |\n";
    let enabled = markdown_block_with_osc8(&theme, names::SHELL_OUTPUT, source, true);
    let disabled = markdown_block_with_osc8(&theme, names::SHELL_OUTPUT, source, false);

    let enabled_text = rendered_text(&enabled);
    let disabled_text = rendered_text(&disabled);
    assert_table_pipe_columns_align(&enabled_text);
    assert_table_pipe_columns_align(&disabled_text);
    assert!(enabled_text.contains("| label |"));
    assert!(disabled_text.contains("| label (https://example.test/target) |"));
    assert!(
        enabled
            .content
            .spans()
            .iter()
            .any(|span| span.hyperlink.as_deref() == Some("https://example.test/target"))
    );
    assert!(
        disabled
            .content
            .spans()
            .iter()
            .all(|span| span.hyperlink.is_none())
    );
}

/// Ensures an explicit-link-looking sequence split by a structural pipe cannot
/// hide that pipe after table width measurement has treated it as a boundary.
#[test]
fn markdown_table_does_not_parse_links_across_structural_pipes() {
    let theme = markdown_test_theme();
    let source = "| A | B |\n| --- | --- |\n| [x | ](https://example.test) |\n";
    let block = markdown_block(&theme, names::SHELL_OUTPUT, source);
    let rendered = rendered_text(&block);

    assert!(rendered.contains("| [x  | ](https://example.test) |"));
    assert_table_pipe_columns_align(&rendered);
    assert!(
        block
            .content
            .spans()
            .iter()
            .all(|span| !(span.hyperlink.is_some() && span.text.contains("x  |"))),
        "a structural table pipe must not become part of an OSC 8 label"
    );
}

/// Ensures the retained cell projections emit exactly the same runs as the
/// complete Markdown path across pipes, code, Unicode, links, and both OSC 8
/// projections.
#[test]
fn markdown_table_retained_cell_runs_match_complete_rendering() {
    let source = "| A | Link | Code |\n| :--- | ---: | :---: |\n| x\\|y | [中](https://example.test) | `a|b` |\n";

    for osc8_links in [false, true] {
        let (projected, reparsed, work) =
            projected_table_runs_and_work(source, osc8_links).expect("valid table projection");
        let mut fence = None;
        let complete = parse_markdown_with_state(source, &mut fence, osc8_links);

        assert_eq!(projected, reparsed);
        assert_eq!(projected, complete);
        assert_eq!(work.parsed_rows, 3);
        assert_eq!(work.parsed_cells, 9);
        assert_eq!(work.emitted_cells, 6);
    }
}

/// Proves a large accepted table performs exactly one structural and inline
/// parse per projected row and cell without relying on elapsed-time thresholds.
#[test]
fn markdown_large_table_projects_each_cell_once() {
    // Each one-column-wide body cell contributes two canonical margins and two
    // alignment spaces. Stay just below the aggregate padding limit.
    const BODY_ROWS: usize = TABLE_MAX_EXTRA_PADDING_BYTES / (TABLE_MAX_COLUMNS * 4) - 5;
    let header = format!(
        "| {} |\n",
        (0..TABLE_MAX_COLUMNS)
            .map(|index| format!("H{index}"))
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
    let body = format!(
        "| {} |\n",
        (0..TABLE_MAX_COLUMNS)
            .map(|_| "x")
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let source = format!("{header}{separator}{}", body.repeat(BODY_ROWS));

    let (projected, reparsed, work) =
        projected_table_runs_and_work(&source, true).expect("bounded large table projection");
    let mut fence = None;
    assert_eq!(projected, reparsed);
    assert_eq!(
        projected,
        parse_markdown_with_state(&source, &mut fence, true)
    );
    assert_eq!(work.parsed_rows, BODY_ROWS + 2);
    assert_eq!(work.parsed_cells, (BODY_ROWS + 2) * TABLE_MAX_COLUMNS);
    assert_eq!(work.emitted_cells, (BODY_ROWS + 1) * TABLE_MAX_COLUMNS);
}

/// Ensures the canonical-margin lower bound stops cell projection at a fixed
/// count for a far-over-bound table while preserving exact raw fallback text.
#[test]
fn markdown_over_bound_table_stops_projection_work_early() {
    const COLUMNS: usize = 2;
    const RETAINED_ROWS: usize = TABLE_MAX_EXTRA_PADDING_BYTES / (COLUMNS * 2);
    const BODY_ROWS: usize = RETAINED_ROWS * 2;
    let mut source = "| A | B |\n| --- | --- |\n".to_owned();
    source.push_str(&"| x | y |\n".repeat(BODY_ROWS));

    let work = table_projection_work(&source, true);
    assert_eq!(work.parsed_rows, RETAINED_ROWS + 1);
    assert_eq!(work.parsed_cells, RETAINED_ROWS * COLUMNS);
    assert_eq!(work.emitted_cells, 0);

    let theme = markdown_test_theme();
    let block = markdown_block(&theme, names::SHELL_OUTPUT, &source);
    assert_eq!(rendered_text(&block), source);
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

/// Ensures a cell above the former scalar cutoff aligns when its final row
/// remains inside the display-column bound.
#[test]
fn table_cells_above_former_cutoff_align_inside_row_bound() {
    let theme = markdown_test_theme();
    let wide = "x".repeat(81);
    let source = format!("| A | B |\n| --- | --- |\n| {wide} | y |\n| z | q |\n");
    let block = markdown_block(&theme, names::SHELL_OUTPUT, &source);

    assert_ne!(rendered_text(&block), source);
    assert_table_pipe_columns_align(&rendered_text(&block));
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

/// Ensures a final logical row above the terminal display-column bound remains
/// raw Markdown rather than allocating alignment padding.
#[test]
fn table_rows_above_display_width_bound_are_not_padded() {
    let theme = markdown_test_theme();
    let wide = "x".repeat(TABLE_MAX_LOGICAL_ROW_DISPLAY_WIDTH);
    let source = format!("| A | B |\n| --- | --- |\n| {wide} | y |\n");
    let block = markdown_block(&theme, names::SHELL_OUTPUT, &source);

    assert_eq!(rendered_text(&block), source);
}

/// Ensures a growing syntactic table that exceeds either padding bound stays
/// live until its blank-line seal, so streaming and final rendering both use
/// the raw-Markdown fallback.
#[test]
fn live_stream_unpadded_tables_match_static_after_seal() {
    let theme = markdown_test_theme();
    let row_too_wide = "x".repeat(TABLE_MAX_LOGICAL_ROW_DISPLAY_WIDTH);
    let width_bound_source = format!("| A | B |\n| --- | --- |\n| {row_too_wide} | y |\n\n");
    let wide_cell = "x".repeat(110);
    let mut padding_bound_source = format!("| {wide_cell} | {wide_cell} |\n| --- | --- |\n");
    let padding_per_short_row = 2 * (wide_cell.len() - 1);
    let short_rows = (TABLE_MAX_EXTRA_PADDING_BYTES / padding_per_short_row) + 1;
    for _ in 0..short_rows {
        padding_bound_source.push_str("| a | b |\n");
    }
    padding_bound_source.push('\n');

    for source in [width_bound_source, padding_bound_source] {
        let static_block = markdown_block(&theme, names::SHELL_OUTPUT, &source);
        assert_eq!(rendered_text(&static_block), source);
        assert_markdown_rendering_property(&theme, &source)
            .expect("sealed unpadded table must retain its static projection");
    }
}

/// Ensures aggregate padding limits fall back even when each rendered line is
/// individually within bounds.
#[test]
fn too_much_total_table_padding_is_not_padded() {
    let theme = markdown_test_theme();
    let wide = "x".repeat(110);
    let mut source = format!("| {wide} | {wide} |\n| --- | --- |\n");
    let padding_per_short_row = 2 * (wide.len() - 1);
    let short_rows = (TABLE_MAX_EXTRA_PADDING_BYTES / padding_per_short_row) + 1;
    for _ in 0..short_rows {
        source.push_str("| a | b |\n");
    }
    let block = markdown_block(&theme, names::SHELL_OUTPUT, &source);

    assert_eq!(rendered_text(&block), source);
}

/// Ensures canonical cell margins count toward the padding budget even when
/// rows need no additional alignment spaces.
#[test]
fn too_many_canonical_table_margins_are_not_padded() {
    let theme = markdown_test_theme();
    let mut source = "|abc|def|\n|---|---|\n".to_owned();
    for _ in 0..=(TABLE_MAX_EXTRA_PADDING_BYTES / 4) {
        source.push_str("|abc|def|\n");
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

/// Ensures streaming waits for a complete header and delimiter, then revises
/// completed rows deterministically until a blank line seals final widths.
#[test]
fn live_stream_tables_update_only_complete_lines_and_match_final_parse() {
    let theme = markdown_test_theme();
    let mut cache = MarkdownStreamCache::default();
    let header = "| A | Longer |\n";
    let delimiter = "| --- | ---: |\n";
    let body = "| one | two |\n";
    let source = format!("{header}{delimiter}{body}");

    assert_eq!(
        rendered_text(&markdown_streaming_block(
            &theme,
            names::SHELL_OUTPUT,
            header,
            &mut cache,
        )),
        format!("{header}…")
    );
    assert_eq!(
        rendered_text(&markdown_streaming_block(
            &theme,
            names::SHELL_OUTPUT,
            &format!("{header}{delimiter}"),
            &mut cache,
        )),
        "| A   | Longer |\n| --- | -----: |\n…"
    );
    assert_eq!(
        rendered_text(&markdown_streaming_block(
            &theme,
            names::SHELL_OUTPUT,
            &format!("{header}{delimiter}| one |"),
            &mut cache,
        )),
        "| A   | Longer |\n| --- | -----: |\n| one | …"
    );

    let sealed = format!("{source}\n");
    let live = markdown_streaming_block(&theme, names::SHELL_OUTPUT, &sealed, &mut cache);
    let final_block = markdown_block(&theme, names::SHELL_OUTPUT, &sealed);
    assert_eq!(
        rendered_text(&live),
        format!("{}…", rendered_text(&final_block))
    );
}

/// Proves every formerly suffix-searching recognizer has a deterministic linear
/// work bound for unmatched and mixed adversarial candidates at audit sizes.
#[test]
fn inline_recognition_work_is_linear_for_failed_candidates() {
    const SIZES: [usize; 3] = [1024, 8 * 1024, 64 * 1024];
    const WORK_PER_BYTE: usize = 32;
    let theme = markdown_test_theme();

    for size in SIZES {
        let candidates = [
            ("unmatched-link", "[".repeat(size)),
            ("unmatched-autolink", "<".repeat(size)),
            (
                "delimiter-runs",
                "***~~_*".repeat(size.div_ceil(7))[..size].to_owned(),
            ),
            (
                "mixed",
                "[<*_~`\\]x".repeat(size.div_ceil(9))[..size].to_owned(),
            ),
            (
                "malformed-bare-url",
                "(http:///".repeat(size.div_ceil(9))[..size].to_owned(),
            ),
        ];
        for (kind, source) in candidates {
            let work = inline_recognition_work(&source);
            assert!(
                work <= WORK_PER_BYTE * size + 64,
                "{kind} at {size} bytes inspected {work} positions"
            );
            assert_eq!(
                rendered_text(&markdown_block(&theme, names::SHELL_OUTPUT, &source)),
                source,
                "{kind} must retain its raw-visible fallback"
            );
        }
    }
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

/// Prevents malformed adjacent asterisk runs from reaching the styled-span
/// slicer with reversed bounds when static or completed streamed assistant text
/// is parsed.
#[test]
fn markdown_adjacent_asterisk_runs_preserve_text_without_panicking() {
    let theme = markdown_test_theme();
    for source in ["***", "****"] {
        let block = markdown_block(&theme, names::AGENT_RESPONSE, source);
        assert_eq!(rendered_text(&block), source);

        let mut cache = MarkdownStreamCache::default();
        let complete = format!("{source}\n");
        let block = markdown_streaming_block(&theme, names::AGENT_RESPONSE, &complete, &mut cache);
        assert_eq!(rendered_text(&block), format!("{complete}…"));
    }
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
    assert_eq!(marker.style.fg, None);
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

/// A nested URL becomes clickable when its streamed line completes, while the
/// incomplete line remains governed by the existing plain-text streaming rule.
#[test]
fn live_stream_recognizes_nested_links_only_on_complete_lines() {
    let theme = markdown_test_theme();
    let mut cache = MarkdownStreamCache::default();
    let incomplete = markdown_streaming_block(
        &theme,
        names::AGENT_RESPONSE,
        "**https://example.test/incomplete**",
        &mut cache,
    );
    assert!(
        incomplete
            .content
            .spans()
            .iter()
            .all(|span| span.hyperlink.is_none())
    );

    let complete = markdown_streaming_block(
        &theme,
        names::AGENT_RESPONSE,
        "**https://example.test/incomplete**\n",
        &mut cache,
    );
    let link = complete
        .content
        .spans()
        .iter()
        .find(|span| span.hyperlink.is_some())
        .expect("completed nested link");
    assert_eq!(
        link.hyperlink.as_deref(),
        Some("https://example.test/incomplete")
    );
    assert!(link.style.bold);
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
    assert_eq!(list_marker.style.fg, Some(tau_cli_term::Color::Magenta));
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

/// Proves completed ordinary lines advance with work proportional to the newly
/// appended bytes instead of reparsing the full accumulated prefix. The fixed
/// 1 KiB chunks and doubling update counts make a quadratic regression
/// deterministic without relying on wall-clock timing.
#[test]
fn live_stream_append_work_is_linear_for_stable_lines() {
    let theme = markdown_test_theme();
    let chunk = format!("*stable* {}\n", "x".repeat(1_024 - "*stable* \n".len()));
    assert_eq!(chunk.len(), 1_024);

    let mut observations = Vec::new();
    for updates in [32, 64, 128] {
        let mut cache = MarkdownStreamCache::default();
        let mut source = String::new();
        for _ in 0..updates {
            source.push_str(&chunk);
            let streaming = markdown_streaming_block_with_osc8(
                &theme,
                names::SHELL_OUTPUT,
                &source,
                &mut cache,
                MarkdownStreamUpdate::Append,
                true,
            );
            let final_block = markdown_block(&theme, names::SHELL_OUTPUT, &source);
            let (progress, body) = streaming
                .content
                .spans()
                .split_last()
                .expect("streaming progress span");
            assert_eq!(progress.text, tau_proto::PROGRESS_INDICATOR_TEXT);
            assert_eq!(
                normalized_spans(body),
                normalized_spans(final_block.content.spans())
            );
        }
        assert!(
            cache.work_bytes <= source.len() * 3,
            "{} bytes of source caused {} bytes of parser/scanner work",
            source.len(),
            cache.work_bytes
        );
        observations.push(cache.work_bytes);
    }
    assert!(observations[1] <= observations[0] * 2 + 1_024);
    assert!(observations[2] <= observations[1] * 2 + 1_024);
}

/// Ensures an irreversibly rejected table does not keep every later
/// leading-pipe row grammar-live. A column mismatch cannot be repaired by
/// appending rows, so only the final possible header may remain in the suffix.
#[test]
fn live_stream_rejected_table_work_remains_linear() {
    let theme = markdown_test_theme();
    let row_overhead = "|  | y | z |\n".len();
    let chunk = format!("| {} | y | z |\n", "x".repeat(1_024 - row_overhead));
    assert_eq!(chunk.len(), 1_024);

    for updates in [32, 64, 128] {
        let mut cache = MarkdownStreamCache::default();
        let mut source = "| A | B |\n| --- | --- |\n".to_owned();
        let _ = markdown_streaming_block_with_osc8(
            &theme,
            names::SHELL_OUTPUT,
            &source,
            &mut cache,
            MarkdownStreamUpdate::Append,
            true,
        );
        for _ in 0..updates {
            source.push_str(&chunk);
            let _ = markdown_streaming_block_with_osc8(
                &theme,
                names::SHELL_OUTPUT,
                &source,
                &mut cache,
                MarkdownStreamUpdate::Append,
                true,
            );
        }
        assert!(
            cache.work_bytes <= source.len() * 6,
            "{} bytes of rejected table source caused {} bytes of work",
            source.len(),
            cache.work_bytes
        );
    }
}

/// Exercises every grammar dependency retained by the suffix cache and checks
/// each legal completed-line boundary against the full parser. Replacement,
/// middle insertion, and OSC 8 changes must discard prior semantic state.
#[test]
fn live_stream_suffix_transitions_match_full_parser() {
    let theme = markdown_test_theme();
    let mut cache = MarkdownStreamCache::default();
    let updates = [
        ("plain\n", MarkdownStreamUpdate::Append, true),
        ("plain\n*incomplete*", MarkdownStreamUpdate::Append, true),
        ("plain\n*incomplete*\n", MarkdownStreamUpdate::Append, true),
        (
            "plain\n*incomplete*\n\n",
            MarkdownStreamUpdate::Append,
            true,
        ),
        (
            "plain\n*incomplete*\n\n```\n[code](https://example.test)\n",
            MarkdownStreamUpdate::Append,
            true,
        ),
        (
            "plain\n*incomplete*\n\n```\n[code](https://example.test)\n```\n",
            MarkdownStreamUpdate::Append,
            true,
        ),
        (
            "| A | B |\n| :--- | ---: |\n| [x](https://x.test) | y |\n",
            MarkdownStreamUpdate::Replace,
            true,
        ),
        (
            "| A | B |\n| :--- | ---: |\n| [x](https://x.test) | y |\n| wider | z |\n",
            MarkdownStreamUpdate::Append,
            true,
        ),
        (
            "| inserted | B |\n| :--- | ---: |\n| [x](https://x.test) | y |\n",
            MarkdownStreamUpdate::Replace,
            true,
        ),
        (
            "| inserted | B |\n| :--- | ---: |\n| [x](https://x.test) | y |\n",
            MarkdownStreamUpdate::Append,
            false,
        ),
        ("", MarkdownStreamUpdate::Replace, false),
    ];

    for (source, update, osc8_links) in updates {
        let streaming = markdown_streaming_block_with_osc8(
            &theme,
            names::SHELL_OUTPUT,
            source,
            &mut cache,
            update,
            osc8_links,
        );
        let final_block = markdown_block_with_osc8(&theme, names::SHELL_OUTPUT, source, osc8_links);
        let (progress, body) = streaming
            .content
            .spans()
            .split_last()
            .expect("streaming progress span");
        assert_eq!(progress.text, tau_proto::PROGRESS_INDICATOR_TEXT);
        if !source.is_empty() && !source.ends_with('\n') {
            assert!(
                body.iter().any(|span| span.text.contains("*incomplete*")),
                "incomplete line must remain visible and plain"
            );
            continue;
        }
        assert_eq!(
            normalized_spans(body),
            normalized_spans(final_block.content.spans()),
            "source: {source:?}"
        );
    }
}
