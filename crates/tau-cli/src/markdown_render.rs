//! UI-only Markdown-lite styling for transcript text.
//!
//! Tau stores and sends prompts, responses, and reasoning as plain text. This
//! renderer is deliberately a terminal presentation layer: it converts a small
//! Markdown-like subset into semantic theme spans and never changes protocol
//! data, durable event logs, model context, or transcript copies.
//!
//! Supported syntax is intentionally small: ATX headings (`# Heading`),
//! unordered (`-`, `*`, `+`) and ordered (`1.`/`1)`) list markers,
//! `*strong*`/`**strong**`, `_emphasis_`, combined `***strong emphasis***`,
//! `~~strikethrough~~`, links, HTTP(S) autolinks and bare URLs, backslash
//! escapes, and leading-pipe tables.
//! Triple-asterisk runs compose strong and emphasis styles, while
//! strikethrough uses its own semantic style; this remains
//! delimiter-preserving Markdown-lite, not a general CommonMark parser. Most
//! constructs preserve exact source characters; tables may receive bounded
//! display-only padding spaces so cells align while the result remains valid
//! Markdown table syntax.
//!
//! Inline backtick spans, fenced code blocks, and indented code-like lines use
//! code styling and suppress nested Markdown-lite styling. Escaped marker
//! sequences use a separate escape style so opt-outs remain visible. Table
//! padding is disabled in code contexts.
//!
//! Live rendering uses [`MarkdownStreamCache`]. Blank lines seal earlier text;
//! sealed chunks are parsed once and cached. The current unsealed block is also
//! parsed through its last completed newline, while the currently incomplete
//! streamed line remains plain until it receives a newline or final/static
//! rendering parses the complete string.

use tau_themes::{SpanTree, StyleIdx, StyleName, ThemedText, names};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FenceKind {
    Backticks,
    Tildes,
}

impl FenceKind {
    fn marker(self) -> &'static str {
        match self {
            Self::Backticks => "```",
            Self::Tildes => "~~~",
        }
    }
}

/// Parsed leading-pipe table row.
///
/// Invariant: `cells` excludes the opening and closing pipe delimiters. For
/// rendered table blocks, [`pad_table_lines`] has verified every row's `cells`
/// vector has the same number of entries. Cell slices are trimmed views into
/// the source line; rendering may add bounded display padding around them but
/// must not change their contents.
#[derive(Debug)]
struct TableRow<'line> {
    indent: &'line str,
    cells: Vec<&'line str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TableRowKind {
    Body,
    Separator,
}

const TABLE_MAX_COLUMNS: usize = 12;
const TABLE_MAX_CELL_WIDTH: usize = 80;
const TABLE_MAX_EXTRA_PADDING_BYTES: usize = 4096;
const TABLE_MAX_RENDERED_LINE_BYTES: usize = 240;

impl<'line> TableRow<'line> {
    fn parse(line: &'line str) -> Option<Self> {
        if !line.contains('|') || is_indented_code(line) {
            return None;
        }
        let indent_len = line.len() - line.trim_start_matches([' ', '\t']).len();
        let indent = &line[..indent_len];
        let body = &line[indent_len..];
        if !body.starts_with('|') || !ends_with_unescaped_pipe(body) {
            return None;
        }
        let mut cells = split_unescaped_pipes(body);
        cells.remove(0);
        cells.pop();
        if cells.len() < 2 || TABLE_MAX_COLUMNS < cells.len() {
            return None;
        }
        Some(Self {
            indent,
            cells: cells.into_iter().map(str::trim).collect(),
        })
    }

    fn render(&self, widths: &[usize], row_kind: TableRowKind) -> String {
        let mut rendered = self.indent.to_owned();
        rendered.push('|');
        for (index, width) in widths.iter().copied().enumerate() {
            if index > 0 {
                rendered.push('|');
            }
            rendered.push(' ');
            let cell = self.cells.get(index).copied().unwrap_or_default();
            match row_kind {
                TableRowKind::Separator => rendered.push_str(&render_separator_cell(cell, width)),
                TableRowKind::Body => {
                    rendered.push_str(cell);
                    rendered.push_str(&" ".repeat(width.saturating_sub(cell.chars().count())));
                }
            }
            rendered.push(' ');
        }
        rendered.push('|');
        rendered
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MarkdownStyle {
    Base,
    Strong,
    StrongEmphasis,
    Emphasis,
    Strikethrough,
    Heading,
    ListMarker,
    PromptMarker,
    Code,
    Escape,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MarkdownRun {
    text: String,
    style: MarkdownStyle,
    hyperlink: Option<String>,
}

/// Append-aware cache for Markdown-lite live response/thinking rendering.
///
/// `source` is the latest full provider snapshot. `finalized_until` is a UTF-8
/// byte boundary into `source`; everything before it has been sealed by a blank
/// line, parsed exactly once, and stored in `finalized_runs`. `in_fence` is the
/// parser context after those sealed runs, so a fenced code block remains plain
/// even when blank lines inside it cause multiple sealed chunks.
///
/// `live_start..live_complete_until` identifies the provisionally parsed
/// completed-line range after `finalized_until`. `live_source` stores the exact
/// text for that range, and `live_runs` may be reused only when both the byte
/// boundaries and source text still match. The provisional parse uses a local
/// copy of `in_fence`; only blank-line finalization commits parser state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct MarkdownStreamCache {
    source: String,
    finalized_until: usize,
    finalized_runs: Vec<MarkdownRun>,
    in_fence: Option<FenceKind>,
    live_start: usize,
    live_complete_until: usize,
    live_source: String,
    live_runs: Vec<MarkdownRun>,
}

impl MarkdownStreamCache {
    fn reset_for_replacement(&mut self) {
        self.source.clear();
        self.finalized_until = 0;
        self.finalized_runs.clear();
        self.in_fence = None;
        self.live_start = 0;
        self.live_complete_until = 0;
        self.live_source.clear();
        self.live_runs.clear();
    }

    fn advance_finalized(&mut self, text: &str, sealed_until: usize) {
        self.finalized_runs.extend(parse_markdown_with_state(
            &text[self.finalized_until..sealed_until],
            &mut self.in_fence,
        ));
        self.finalized_until = sealed_until;
        if self.live_start != self.finalized_until {
            self.live_start = self.finalized_until;
            self.live_complete_until = self.finalized_until;
            self.live_source.clear();
            self.live_runs.clear();
        }
    }

    fn refresh_live_runs(&mut self, text: &str, complete_until: usize) {
        let live_source = &text[self.finalized_until..complete_until];
        if self.live_start == self.finalized_until
            && self.live_complete_until == complete_until
            && self.live_source == live_source
        {
            return;
        }

        let mut live_fence = self.in_fence;
        self.live_runs = parse_markdown_with_state(live_source, &mut live_fence);
        self.live_start = self.finalized_until;
        self.live_complete_until = complete_until;
        self.live_source = live_source.to_owned();
    }
}

#[cfg(test)]
pub(crate) fn markdown_block(
    theme: &tau_themes::Theme,
    base_style_name: &str,
    text: &str,
) -> tau_cli_term::StyledBlock {
    markdown_block_with_osc8(theme, base_style_name, text, true)
}

#[cfg(test)]
fn markdown_prefixed_block(
    theme: &tau_themes::Theme,
    base_style_name: &str,
    prefix_text: &str,
    text: &str,
) -> tau_cli_term::StyledBlock {
    markdown_prefixed_block_with_osc8(theme, base_style_name, prefix_text, text, true)
}

#[cfg(test)]
fn markdown_prompt_block(
    theme: &tau_themes::Theme,
    base_style_name: &str,
    marker_text: String,
    text: &str,
) -> tau_cli_term::StyledBlock {
    markdown_prompt_block_with_osc8(theme, base_style_name, marker_text, text, true)
}

#[cfg(test)]
fn markdown_streaming_block(
    theme: &tau_themes::Theme,
    base_style_name: &str,
    text: &str,
    cache: &mut MarkdownStreamCache,
) -> tau_cli_term::StyledBlock {
    markdown_streaming_block_with_osc8(theme, base_style_name, text, cache, true)
}

#[cfg(test)]
fn markdown_prefixed_streaming_block(
    theme: &tau_themes::Theme,
    base_style_name: &str,
    prefix_text: &str,
    text: &str,
    cache: &mut MarkdownStreamCache,
) -> tau_cli_term::StyledBlock {
    markdown_prefixed_streaming_block_with_osc8(
        theme,
        base_style_name,
        prefix_text,
        text,
        cache,
        true,
    )
}

/// Render final/static transcript text with Markdown-lite semantic styles.
pub(crate) fn markdown_block_with_osc8(
    theme: &tau_themes::Theme,
    base_style_name: &str,
    text: &str,
    osc8_links: bool,
) -> tau_cli_term::StyledBlock {
    markdown_prefixed_block_with_osc8(theme, base_style_name, "", text, osc8_links)
}

/// Render final/static transcript text with a base-styled state marker before
/// the Markdown-lite body.
pub(crate) fn markdown_prefixed_block_with_osc8(
    theme: &tau_themes::Theme,
    base_style_name: &str,
    prefix_text: &str,
    text: &str,
    osc8_links: bool,
) -> tau_cli_term::StyledBlock {
    let prefix = [MarkdownRun {
        text: prefix_text.to_owned(),
        style: MarkdownStyle::Base,
        hyperlink: None,
    }];
    let mut in_fence = None;
    styled_block_from_runs(
        theme,
        base_style_name,
        &prefix,
        &parse_markdown_with_state(text, &mut in_fence),
        false,
        osc8_links,
    )
}

/// Render a configured prompt-state marker followed by Markdown-lite prompt
/// text.
pub(crate) fn markdown_prompt_block_with_osc8(
    theme: &tau_themes::Theme,
    base_style_name: &str,
    marker_text: String,
    text: &str,
    osc8_links: bool,
) -> tau_cli_term::StyledBlock {
    let prefix = [MarkdownRun {
        text: marker_text,
        style: MarkdownStyle::PromptMarker,
        hyperlink: None,
    }];
    let mut in_fence = None;
    styled_block_from_runs(
        theme,
        base_style_name,
        &prefix,
        &parse_markdown_with_state(text, &mut in_fence),
        false,
        osc8_links,
    )
}

/// Render live append-only text with sealed paragraphs cached, completed lines
/// in the current block formatted provisionally, and only the current
/// incomplete line left plain.
pub(crate) fn markdown_streaming_block_with_osc8(
    theme: &tau_themes::Theme,
    base_style_name: &str,
    text: &str,
    cache: &mut MarkdownStreamCache,
    osc8_links: bool,
) -> tau_cli_term::StyledBlock {
    markdown_prefixed_streaming_block_with_osc8(theme, base_style_name, "", text, cache, osc8_links)
}

/// Render live append-only text with a stable base-styled state marker before
/// the incrementally formatted Markdown-lite body.
pub(crate) fn markdown_prefixed_streaming_block_with_osc8(
    theme: &tau_themes::Theme,
    base_style_name: &str,
    prefix_text: &str,
    text: &str,
    cache: &mut MarkdownStreamCache,
    osc8_links: bool,
) -> tau_cli_term::StyledBlock {
    if !text.starts_with(&cache.source) {
        cache.reset_for_replacement();
    }

    let sealed_until = latest_sealed_boundary(text).unwrap_or(0);
    if cache.finalized_until < sealed_until {
        cache.advance_finalized(text, sealed_until);
    }
    let complete_until = latest_complete_line_boundary(text)
        .unwrap_or(cache.finalized_until)
        .max(cache.finalized_until);
    cache.refresh_live_runs(text, complete_until);
    cache.source = text.to_owned();

    let mut runs = cache.finalized_runs.clone();
    runs.extend(cache.live_runs.clone());
    if complete_until < text.len() {
        runs.push(MarkdownRun {
            text: text[complete_until..].to_owned(),
            style: MarkdownStyle::Base,
            hyperlink: None,
        });
    }
    let prefix = [MarkdownRun {
        text: prefix_text.to_owned(),
        style: MarkdownStyle::Base,
        hyperlink: None,
    }];
    styled_block_from_runs(theme, base_style_name, &prefix, &runs, true, osc8_links)
}

fn styled_block_from_runs(
    theme: &tau_themes::Theme,
    base_style_name: &str,
    prefix: &[MarkdownRun],
    runs: &[MarkdownRun],
    progress: bool,
    osc8_links: bool,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::{convert_color, themed_text};

    let mut themed = ThemedText::new();
    let base = themed.add_style(base_style_name);
    let strong = themed.add_style(names::MARKDOWN_STRONG);
    let emphasis = themed.add_style(names::MARKDOWN_EMPHASIS);
    let strikethrough = themed.add_style(names::MARKDOWN_STRIKETHROUGH);
    let heading = themed.add_style(names::MARKDOWN_HEADING);
    let list_marker = themed.add_style(names::MARKDOWN_LIST_MARKER);
    let prompt_marker = themed.add_style(names::PROMPT_MARKER_SUBMITTED);
    let code = themed.add_style(names::MARKDOWN_CODE);
    let escape = themed.add_style(names::MARKDOWN_ESCAPE);
    let link = themed.add_style(names::MARKDOWN_LINK);
    let progress_style = themed.add_style(names::PROGRESS_INDICATOR);

    let mut body_children = Vec::new();
    let styles = MarkdownStyleIndexes {
        strong,
        emphasis,
        strikethrough,
        heading,
        list_marker,
        prompt_marker,
        code,
        escape,
        link,
    };
    push_runs(&mut body_children, prefix, styles, false);
    push_runs(&mut body_children, runs, styles, !osc8_links);

    let needs_space = progress && body_children_text_ends_non_whitespace(prefix, runs);

    let mut root_children = Vec::new();
    if !body_children.is_empty() {
        root_children.push(SpanTree::span(base, body_children));
    }
    if progress {
        if needs_space {
            root_children.push(SpanTree::span(base, vec![SpanTree::text(" ")]));
        }
        root_children.push(SpanTree::span(
            progress_style,
            vec![SpanTree::text(tau_proto::PROGRESS_INDICATOR_TEXT)],
        ));
    }
    themed.push_tree(SpanTree::span(StyleIdx::DEFAULT, root_children));

    let body_ts = theme.resolve_style(&StyleName::new(base_style_name));
    let mut rendered = themed_text(theme, &themed);
    let targets = prefix
        .iter()
        .chain(runs)
        .filter(|run| !run.text.is_empty())
        .map(|run| run.hyperlink.as_deref());
    for (span, target) in rendered.spans_mut().iter_mut().zip(targets) {
        if osc8_links {
            span.hyperlink = target
                .and_then(tau_cli_term::sanitize_hyperlink_target)
                .map(std::sync::Arc::from);
        }
    }
    let mut block = tau_cli_term::StyledBlock::new(rendered);
    if let Some(bg) = body_ts.bg {
        block = block.bg(convert_color(bg));
    }
    block
}

fn body_children_text_ends_non_whitespace(prefix: &[MarkdownRun], runs: &[MarkdownRun]) -> bool {
    runs_end_non_whitespace(runs) || runs_end_non_whitespace(prefix)
}

fn runs_end_non_whitespace(runs: &[MarkdownRun]) -> bool {
    runs.iter()
        .rev()
        .find(|run| !run.text.is_empty())
        .and_then(|run| run.text.chars().next_back())
        .is_some_and(|c| !c.is_whitespace())
}

#[derive(Clone, Copy)]
struct MarkdownStyleIndexes {
    strong: StyleIdx,
    emphasis: StyleIdx,
    strikethrough: StyleIdx,
    heading: StyleIdx,
    list_marker: StyleIdx,
    prompt_marker: StyleIdx,
    code: StyleIdx,
    escape: StyleIdx,
    link: StyleIdx,
}

fn push_runs(
    children: &mut Vec<SpanTree<StyleIdx>>,
    runs: &[MarkdownRun],
    styles: MarkdownStyleIndexes,
    show_link_target: bool,
) {
    for run in runs {
        if !(run.text.is_empty()) {
            let target = run.hyperlink.as_deref();
            let needs_visible_target = target.is_some_and(|target| {
                show_link_target || tau_cli_term::sanitize_hyperlink_target(target).is_none()
            });
            let text = if let Some(target) = target.filter(|_| needs_visible_target)
                && target != run.text
            {
                format!("{} ({target})", run.text)
            } else {
                run.text.clone()
            };
            let leaf = if target.is_some() {
                SpanTree::span(styles.link, vec![SpanTree::text(text)])
            } else {
                SpanTree::text(text)
            };
            let node = match run.style {
                MarkdownStyle::Base => leaf,
                MarkdownStyle::Strong => SpanTree::span(styles.strong, vec![leaf]),
                MarkdownStyle::StrongEmphasis => SpanTree::span(
                    styles.strong,
                    vec![SpanTree::span(styles.emphasis, vec![leaf])],
                ),
                MarkdownStyle::Emphasis => SpanTree::span(styles.emphasis, vec![leaf]),
                MarkdownStyle::Strikethrough => SpanTree::span(styles.strikethrough, vec![leaf]),
                MarkdownStyle::Heading => SpanTree::span(styles.heading, vec![leaf]),
                MarkdownStyle::ListMarker => SpanTree::span(styles.list_marker, vec![leaf]),
                MarkdownStyle::PromptMarker => SpanTree::span(styles.prompt_marker, vec![leaf]),
                MarkdownStyle::Code => SpanTree::span(styles.code, vec![leaf]),
                MarkdownStyle::Escape => SpanTree::span(styles.escape, vec![leaf]),
            };
            children.push(node);
        }
    }
}

fn latest_sealed_boundary(text: &str) -> Option<usize> {
    let mut offset = 0;
    let mut latest = None;
    for line in text.split_inclusive('\n') {
        offset += line.len();
        if line.ends_with('\n') && line.trim().is_empty() {
            latest = Some(offset);
        }
    }
    latest
}

fn latest_complete_line_boundary(text: &str) -> Option<usize> {
    text.rfind('\n').map(|index| index + 1)
}

fn parse_markdown_with_state(text: &str, in_fence: &mut Option<FenceKind>) -> Vec<MarkdownRun> {
    let mut runs = Vec::new();
    let lines = text
        .split_inclusive('\n')
        .map(|line| {
            line.strip_suffix('\n')
                .map_or((line, ""), |body| (body, "\n"))
        })
        .collect::<Vec<_>>();
    let mut index = 0;
    while index < lines.len() {
        let (body, newline) = lines[index];
        let trimmed = body.trim_start();
        if let Some(fence) = *in_fence {
            push_run(&mut runs, body, MarkdownStyle::Code);
            push_run(&mut runs, newline, MarkdownStyle::Base);
            if trimmed.starts_with(fence.marker()) {
                *in_fence = None;
            }
            index += 1;
            continue;
        }
        if let Some(fence) = fence_marker(trimmed) {
            push_run(&mut runs, body, MarkdownStyle::Code);
            push_run(&mut runs, newline, MarkdownStyle::Base);
            *in_fence = Some(fence);
            index += 1;
            continue;
        }
        if let Some(table_end) = table_block_end(&lines, index)
            && let Some(padded_lines) = pad_table_lines(&lines[index..table_end])
        {
            for (padded, (_, newline)) in padded_lines.into_iter().zip(&lines[index..table_end]) {
                parse_inline(&padded, &mut runs);
                push_run(&mut runs, newline, MarkdownStyle::Base);
            }
            index = table_end;
            continue;
        }
        if is_heading(body) {
            push_run(&mut runs, body, MarkdownStyle::Heading);
            push_run(&mut runs, newline, MarkdownStyle::Base);
            index += 1;
            continue;
        }
        if let Some((indent_end, marker_end)) = list_marker_range(body) {
            push_run(&mut runs, &body[..indent_end], MarkdownStyle::Base);
            push_run(
                &mut runs,
                &body[indent_end..marker_end],
                MarkdownStyle::ListMarker,
            );
            parse_inline(&body[marker_end..], &mut runs);
            push_run(&mut runs, newline, MarkdownStyle::Base);
            index += 1;
            continue;
        }
        if is_indented_code(body) {
            push_run(&mut runs, body, MarkdownStyle::Code);
            push_run(&mut runs, newline, MarkdownStyle::Base);
            index += 1;
            continue;
        }
        parse_inline(body, &mut runs);
        push_run(&mut runs, newline, MarkdownStyle::Base);
        index += 1;
    }
    runs
}

fn table_block_end(lines: &[(&str, &str)], start: usize) -> Option<usize> {
    if start + 1 >= lines.len()
        || is_indented_code(lines[start].0)
        || TableRow::parse(lines[start].0).is_none()
    {
        return None;
    }
    let separator = TableRow::parse(lines[start + 1].0)?;
    if !separator.cells.iter().all(|cell| is_separator_cell(cell)) {
        return None;
    }

    let mut end = start + 2;
    while end < lines.len() {
        if is_indented_code(lines[end].0) || TableRow::parse(lines[end].0).is_none() {
            break;
        }
        end += 1;
    }
    Some(end)
}

fn split_unescaped_pipes(text: &str) -> Vec<&str> {
    let mut cells = Vec::new();
    let mut start = 0;
    let mut escaped = false;
    let mut in_code_span = false;
    for (index, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '`' {
            in_code_span = !in_code_span;
            continue;
        }
        if ch == '|' && !in_code_span {
            cells.push(&text[start..index]);
            start = index + ch.len_utf8();
        }
    }
    cells.push(&text[start..]);
    cells
}

fn ends_with_unescaped_pipe(text: &str) -> bool {
    let mut escaped = false;
    let mut in_code_span = false;
    let mut last_unescaped_pipe = false;
    for ch in text.chars() {
        if escaped {
            escaped = false;
            last_unescaped_pipe = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            last_unescaped_pipe = false;
            continue;
        }
        if ch == '`' {
            in_code_span = !in_code_span;
            last_unescaped_pipe = false;
            continue;
        }
        last_unescaped_pipe = ch == '|' && !in_code_span;
    }
    last_unescaped_pipe
}

fn is_separator_cell(cell: &str) -> bool {
    let cell = cell.trim();
    let cell = cell.strip_prefix(':').unwrap_or(cell);
    let cell = cell.strip_suffix(':').unwrap_or(cell);
    3 <= cell.len() && cell.chars().all(|ch| ch == '-')
}

fn pad_table_lines(lines: &[(&str, &str)]) -> Option<Vec<String>> {
    let rows = lines
        .iter()
        .map(|(line, _)| TableRow::parse(line))
        .collect::<Option<Vec<_>>>()?;
    let columns = rows.first()?.cells.len();
    if rows.iter().any(|row| row.cells.len() != columns) {
        return None;
    }
    let mut widths = vec![3; columns];
    for (row_index, row) in rows.iter().enumerate() {
        if !(row_index == 1) {
            for (index, cell) in row.cells.iter().enumerate() {
                widths[index] = widths[index].max(cell.chars().count());
            }
        }
    }
    if widths.iter().any(|width| TABLE_MAX_CELL_WIDTH < *width) {
        return None;
    }

    let mut extra_padding = 0usize;
    let mut padded = Vec::with_capacity(rows.len());
    for (row_index, row) in rows.iter().enumerate() {
        let row_kind = if row_index == 1 {
            TableRowKind::Separator
        } else {
            TableRowKind::Body
        };
        let rendered = row.render(&widths, row_kind);
        if TABLE_MAX_RENDERED_LINE_BYTES < rendered.len() {
            return None;
        }
        extra_padding =
            extra_padding.saturating_add(rendered.len().saturating_sub(lines[row_index].0.len()));
        if TABLE_MAX_EXTRA_PADDING_BYTES < extra_padding {
            return None;
        }
        padded.push(rendered);
    }
    Some(padded)
}

fn render_separator_cell(cell: &str, width: usize) -> String {
    let left = cell.trim().starts_with(':');
    let right = cell.trim().ends_with(':');
    let dash_count = width.saturating_sub(usize::from(left) + usize::from(right));
    let dash_count = dash_count.max(3);
    format!(
        "{}{}{}",
        if left { ":" } else { "" },
        "-".repeat(dash_count),
        if right { ":" } else { "" }
    )
}

fn is_heading(line: &str) -> bool {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    (1..=6).contains(&hashes) && line.as_bytes().get(hashes) == Some(&b' ')
}

fn list_marker_range(line: &str) -> Option<(usize, usize)> {
    let indent_end = line
        .char_indices()
        .find(|(_, c)| !matches!(c, ' ' | '\t'))
        .map_or(line.len(), |(idx, _)| idx);
    let rest = &line[indent_end..];
    let bytes = rest.as_bytes();
    let marker = bytes.first().copied()?;
    if matches!(marker, b'-' | b'*' | b'+') && bytes.get(1) == Some(&b' ') {
        return Some((indent_end, indent_end + 1));
    }

    let digits = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    if matches!(bytes.get(digits), Some(b'.' | b')')) && bytes.get(digits + 1) == Some(&b' ') {
        return Some((indent_end, indent_end + digits + 1));
    }
    None
}

fn is_indented_code(line: &str) -> bool {
    line.starts_with('\t') || line.starts_with("    ")
}

fn fence_marker(trimmed: &str) -> Option<FenceKind> {
    if trimmed.starts_with("```") {
        Some(FenceKind::Backticks)
    } else if trimmed.starts_with("~~~") {
        Some(FenceKind::Tildes)
    } else {
        None
    }
}

fn parse_inline(text: &str, runs: &mut Vec<MarkdownRun>) {
    parse_inline_with_style(text, runs, MarkdownStyle::Base, true);
}

fn parse_inline_with_style(
    text: &str,
    runs: &mut Vec<MarkdownRun>,
    inherited_style: MarkdownStyle,
    recognize_delimiters: bool,
) {
    let mut index = 0;
    while index < text.len() {
        let rest = &text[index..];
        let ch = rest.chars().next().expect("non-empty remainder");
        if ch == '\\'
            && let Some(len) = escaped_len(rest)
        {
            let style = if inherited_style == MarkdownStyle::Base {
                MarkdownStyle::Escape
            } else {
                inherited_style
            };
            push_run(runs, &rest[..len], style);
            index += len;
            continue;
        }
        if ch == '`'
            && let Some(end) = find_unescaped(&rest[1..], '`')
        {
            let len = 1 + end + 1;
            let style = if inherited_style == MarkdownStyle::Base {
                MarkdownStyle::Code
            } else {
                inherited_style
            };
            push_run(runs, &rest[..len], style);
            index += len;
            continue;
        }
        if ch == '['
            && let Some((label, target, len)) = parse_inline_link(rest)
        {
            push_link_run(runs, label, target, inherited_style);
            index += len;
            continue;
        }
        if ch == '<'
            && let Some((target, len)) = parse_autolink(rest)
        {
            push_link_run(runs, target, target, inherited_style);
            index += len;
            continue;
        }
        if is_bare_url_start(text, index, rest) {
            let len = bare_url_len(rest);
            let target = &rest[..len];
            if valid_http_target(target) {
                push_link_run(runs, target, target, inherited_style);
                index += len;
                continue;
            }
        }
        if recognize_delimiters
            && rest.starts_with("***")
            && let Some(end) = find_closing_sequence(text, index, "***")
        {
            push_styled_inline(runs, &text[index..end], MarkdownStyle::StrongEmphasis);
            index = end;
            continue;
        }
        if recognize_delimiters
            && rest.starts_with("**")
            && let Some(end) = find_closing_sequence(text, index, "**")
        {
            push_styled_inline(runs, &text[index..end], MarkdownStyle::Strong);
            index = end;
            continue;
        }
        if recognize_delimiters
            && rest.starts_with("~~")
            && let Some(end) = find_closing_sequence(text, index, "~~")
        {
            push_styled_inline(runs, &text[index..end], MarkdownStyle::Strikethrough);
            index = end;
            continue;
        }
        if recognize_delimiters
            && matches!(ch, '*' | '_')
            && delimiter_allowed(text, index, ch)
            && let Some(end) = find_closing_delimiter(text, index, ch)
        {
            let style = if ch == '*' {
                MarkdownStyle::Strong
            } else {
                MarkdownStyle::Emphasis
            };
            push_styled_inline(runs, &text[index..end], style);
            index = end;
            continue;
        }
        let next = index + ch.len_utf8();
        push_run(runs, &text[index..next], inherited_style);
        index = next;
    }
}

/// Parses links inside a uniformly styled delimiter-preserving span.
fn push_styled_inline(runs: &mut Vec<MarkdownRun>, text: &str, style: MarkdownStyle) {
    let delimiter_len = match style {
        MarkdownStyle::StrongEmphasis => 3,
        MarkdownStyle::Strikethrough => 2,
        MarkdownStyle::Strong if text.starts_with("**") => 2,
        MarkdownStyle::Strong | MarkdownStyle::Emphasis => 1,
        _ => 0,
    };
    let content_end = text.len().saturating_sub(delimiter_len);
    push_run(runs, &text[..delimiter_len], style);
    parse_inline_with_style(&text[delimiter_len..content_end], runs, style, false);
    push_run(runs, &text[content_end..], style);
}

fn escaped_len(rest: &str) -> Option<usize> {
    let mut chars = rest.chars();
    (chars.next() == Some('\\'))
        .then_some(chars.next()?)
        .filter(|c| {
            matches!(
                c,
                '*' | '_' | '~' | '#' | '-' | '\\' | '`' | '[' | ']' | '(' | ')' | '<' | '>'
            )
        })
        .map(|c| 1 + c.len_utf8())
}

fn find_unescaped(text: &str, needle: char) -> Option<usize> {
    let mut escaped = false;
    for (idx, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == needle {
            return Some(idx);
        }
    }
    None
}

fn find_closing_sequence(text: &str, start: usize, delimiter: &str) -> Option<usize> {
    let after_open = start + delimiter.len();
    let rest = &text[after_open..];
    let mut relative = 0;
    while relative < rest.len() {
        let candidate = &rest[relative..];
        if let Some(len) = opaque_inline_len(candidate) {
            relative += len;
            continue;
        }
        let close = after_open + relative;
        if candidate.starts_with(delimiter) && after_open < close {
            return Some(close + delimiter.len());
        }
        relative += candidate
            .chars()
            .next()
            .expect("non-empty candidate")
            .len_utf8();
    }
    None
}

fn find_closing_delimiter(text: &str, start: usize, delimiter: char) -> Option<usize> {
    let after_open = start + delimiter.len_utf8();
    let rest = &text[after_open..];
    let mut relative = 0;
    while relative < rest.len() {
        let candidate = &rest[relative..];
        if let Some(len) = opaque_inline_len(candidate) {
            relative += len;
            continue;
        }
        let ch = candidate.chars().next().expect("non-empty candidate");
        if ch == delimiter {
            let close = after_open + relative;
            if delimiter_allowed(text, close, delimiter) && after_open < close {
                return Some(close + delimiter.len_utf8());
            }
        }
        relative += ch.len_utf8();
    }
    None
}

fn opaque_inline_len(text: &str) -> Option<usize> {
    escaped_len(text)
        .or_else(|| {
            text.strip_prefix('`')
                .and_then(|rest| find_unescaped(rest, '`').map(|end| end + 2))
        })
        .or_else(|| {
            text.starts_with('[')
                .then(|| parse_inline_link(text))
                .flatten()
                .map(|(_, _, len)| len)
        })
        .or_else(|| {
            text.starts_with('<')
                .then(|| parse_autolink(text))
                .flatten()
                .map(|(_, len)| len)
        })
}

fn delimiter_allowed(text: &str, index: usize, delimiter: char) -> bool {
    if delimiter != '_' {
        return true;
    }
    let previous = text[..index].chars().next_back();
    let next = text[index + delimiter.len_utf8()..].chars().next();
    !(previous.is_some_and(|c| c.is_alphanumeric()) && next.is_some_and(|c| c.is_alphanumeric()))
}

fn push_run(runs: &mut Vec<MarkdownRun>, text: &str, style: MarkdownStyle) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = runs.last_mut()
        && last.style == style
        && last.hyperlink.is_none()
    {
        last.text.push_str(text);
        return;
    }
    runs.push(MarkdownRun {
        text: text.to_owned(),
        style,
        hyperlink: None,
    });
}

fn push_link_run(runs: &mut Vec<MarkdownRun>, text: &str, target: &str, style: MarkdownStyle) {
    runs.push(MarkdownRun {
        text: text.to_owned(),
        style,
        hyperlink: Some(target.to_owned()),
    });
}

fn parse_inline_link(text: &str) -> Option<(&str, &str, usize)> {
    let close_label = find_unescaped(&text[1..], ']')? + 1;
    let open_target = close_label + 1;
    if text.as_bytes().get(open_target) != Some(&b'(') {
        return None;
    }
    let close_target = find_unescaped(&text[open_target + 1..], ')')? + open_target + 1;
    let label = &text[1..close_label];
    let target = &text[open_target + 1..close_target];
    (!label.is_empty() && !target.is_empty() && !target.chars().any(char::is_whitespace))
        .then_some((label, target, close_target + 1))
}

fn parse_autolink(text: &str) -> Option<(&str, usize)> {
    let close = text.find('>')?;
    let target = &text[1..close];
    valid_http_target(target).then_some((target, close + 1))
}

fn is_bare_url_start(text: &str, index: usize, rest: &str) -> bool {
    (rest.starts_with("https://") || rest.starts_with("http://"))
        && text[..index]
            .chars()
            .next_back()
            .is_none_or(|ch| ch.is_whitespace() || matches!(ch, '(' | '[' | '{'))
}

fn valid_http_target(target: &str) -> bool {
    let body = target
        .strip_prefix("https://")
        .or_else(|| target.strip_prefix("http://"));
    body.is_some_and(|body| {
        !body.is_empty()
            && !body.starts_with(['/', '?', '#'])
            && !body.chars().any(|ch| ch.is_whitespace() || ch.is_control())
    })
}

fn bare_url_len(text: &str) -> usize {
    let mut end = text.len();
    for (idx, ch) in text.char_indices() {
        if ch.is_whitespace() || ch == '<' || ch == '>' {
            end = idx;
            break;
        }
    }
    while text[..end].ends_with(['.', ',', ';', ':', '!', '?', ')', ']']) {
        end -= text[..end].chars().next_back().map_or(0, char::len_utf8);
    }
    end
}

#[cfg(test)]
mod tests;
