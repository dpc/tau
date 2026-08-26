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
//! Live rendering uses [`MarkdownStreamCache`]. Complete lines become stable as
//! soon as later input can no longer reinterpret them as a table; stable runs
//! are parsed once and cached. A possible table header or growing table remains
//! live because later rows can change its widths. The currently incomplete line
//! remains plain until it receives a newline or final/static rendering parses
//! the complete string.
//!
//! Inline recognition indexes the suffix once before rendering it. Unmatched
//! code backticks, link labels and targets, autolink brackets, and emphasis,
//! strong, or strike delimiters therefore reuse the same next-candidate facts
//! instead of rescanning the suffix for every failed opener.

use std::sync as path_std_sync;

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

/// Validated display projection inputs for one complete Markdown-lite table.
///
/// The projection retains parsed cells instead of reconstructed row strings so
/// inline parsing cannot consume a structural pipe at a cell boundary.
#[derive(Debug)]
struct TableProjection<'line> {
    /// Parsed header, delimiter, and body rows in source order.
    rows: Vec<TableRow<'line>>,
    /// Final terminal display width selected for each cell column.
    widths: Vec<usize>,
    /// Placement and delimiter marker selected by the delimiter row.
    alignments: Vec<TableAlignment>,
    /// Visible inline width of every non-delimiter cell.
    visible_widths: Vec<Vec<usize>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TableRowKind {
    Body,
    Separator,
}

/// Horizontal placement selected by one delimiter-row cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TableAlignment {
    Left,
    LeftMarked,
    Right,
    Center,
}

const TABLE_MAX_COLUMNS: usize = 12;
const TABLE_MAX_EXTRA_PADDING_BYTES: usize = 4096;
const TABLE_MAX_LOGICAL_ROW_DISPLAY_WIDTH: usize = 240;

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

    /// Returns this row's final terminal display width after table projection.
    fn logical_display_width(&self, widths: &[usize]) -> Option<usize> {
        let cells_width = widths
            .iter()
            .try_fold(0usize, |total, width| total.checked_add(*width))?;
        let separators_and_cell_margins = widths.len().checked_mul(3)?.checked_add(1)?;
        tau_term_screen::display_width(self.indent)
            .checked_add(cells_width)?
            .checked_add(separators_and_cell_margins)
    }
}

impl TableAlignment {
    /// Parses the deliberately small Markdown-lite delimiter-cell grammar.
    fn parse(cell: &str) -> Option<Self> {
        let cell = cell.trim();
        let left = cell.starts_with(':');
        let cell = cell.strip_prefix(':').unwrap_or(cell);
        let right = cell.ends_with(':');
        let dashes = cell.strip_suffix(':').unwrap_or(cell);
        if dashes.len() < 3 || !dashes.chars().all(|ch| ch == '-') {
            return None;
        }
        Some(match (left, right) {
            (true, true) => Self::Center,
            (true, false) => Self::LeftMarked,
            (false, true) => Self::Right,
            (false, false) => Self::Left,
        })
    }

    /// Returns the minimum width which preserves this marker and three dashes.
    fn minimum_width(self) -> usize {
        3 + match self {
            Self::Left => 0,
            Self::LeftMarked | Self::Right => 1,
            Self::Center => 2,
        }
    }

    /// Splits unused cell columns according to this alignment.
    fn padding(self, spare: usize) -> (usize, usize) {
        match self {
            Self::Left | Self::LeftMarked => (0, spare),
            Self::Right => (spare, 0),
            // Keep odd padding deterministic: the left gets floor(spare / 2).
            Self::Center => (spare / 2, spare - (spare / 2)),
        }
    }

    /// Preserves delimiter colons while expanding the dash run to `width`.
    fn render_separator_cell(self, width: usize) -> String {
        let (left, right) = match self {
            Self::Left => ("", ""),
            Self::LeftMarked => (":", ""),
            Self::Right => ("", ":"),
            Self::Center => (":", ":"),
        };
        let dash_count = width - left.len() - right.len();
        format!("{left}{}{right}", "-".repeat(dash_count))
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

/// Borrowed semantic projections used to construct one styled block.
struct RenderRuns<'run> {
    /// Stable cached runs whose grammar can no longer change.
    stable: &'run [MarkdownRun],
    /// Runs for the grammar-unstable complete-line suffix.
    live: &'run [MarkdownRun],
    /// Current incomplete line, which remains base/plain.
    incomplete: &'run str,
}

/// Append-aware cache for Markdown-lite live response/thinking rendering.
///
/// `source_len` is the byte length classified by the accumulator on the
/// previous update. `stable_until` is a UTF-8 boundary before which grammar can
/// no longer change: those bytes exist only in `stable_runs`, not in another
/// source copy. `stable_fence` is the parser state at that boundary.
///
/// `live_runs` covers `stable_until..complete_until`. That suffix contains only
/// grammar that can still change: a possible table header or a growing table
/// whose later rows can revise all column widths. The incomplete line after
/// `complete_until` is borrowed directly from the caller while constructing
/// themed spans and is never copied into this cache.
/// `osc8_links` records the visible-link projection used to derive cached table
/// widths, so changing that setting invalidates the cache rather than retaining
/// stale alignment spaces.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct MarkdownStreamCache {
    source_len: usize,
    stable_until: usize,
    stable_runs: Vec<MarkdownRun>,
    stable_fence: Option<FenceKind>,
    complete_until: usize,
    live_runs: Vec<MarkdownRun>,
    osc8_links: Option<bool>,
    #[cfg(test)]
    work_bytes: usize,
    #[cfg(test)]
    test_source: String,
}

impl MarkdownStreamCache {
    fn reset_for_replacement(&mut self) {
        self.source_len = 0;
        self.stable_until = 0;
        self.stable_runs.clear();
        self.stable_fence = None;
        self.complete_until = 0;
        self.live_runs.clear();
        self.osc8_links = None;
        #[cfg(test)]
        self.test_source.clear();
    }

    fn parse_counted(
        &mut self,
        text: &str,
        in_fence: &mut Option<FenceKind>,
        osc8_links: bool,
    ) -> Vec<MarkdownRun> {
        #[cfg(test)]
        {
            self.work_bytes += text.len();
        }
        parse_markdown_with_state(text, in_fence, osc8_links)
    }

    fn advance_append(&mut self, text: &str, osc8_links: bool) {
        let appended = &text[self.source_len..];
        #[cfg(test)]
        {
            self.work_bytes += appended.len();
            self.test_source.clear();
            self.test_source.push_str(text);
        }
        let Some(last_newline) = appended.rfind('\n') else {
            self.source_len = text.len();
            return;
        };
        self.complete_until = self.source_len + last_newline + 1;
        self.source_len = text.len();

        let pending = &text[self.stable_until..self.complete_until];
        #[cfg(test)]
        {
            self.work_bytes += pending.len();
        }
        let retain_at = unstable_suffix_start(pending, self.stable_fence, osc8_links);
        if 0 < retain_at {
            let stable = &pending[..retain_at];
            let mut fence = self.stable_fence;
            let runs = self.parse_counted(stable, &mut fence, osc8_links);
            self.stable_runs.extend(runs);
            self.stable_until += retain_at;
            self.stable_fence = fence;
        }
        let live = &text[self.stable_until..self.complete_until];
        let mut live_fence = self.stable_fence;
        self.live_runs = self.parse_counted(live, &mut live_fence, osc8_links);
    }
}

/// Exact source mutation classification supplied by provider accumulation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MarkdownStreamUpdate {
    /// The new snapshot consists of the prior bytes followed by a suffix.
    Append,
    /// Existing bytes were cleared, replaced, or inserted before the end.
    Replace,
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
    let update = if text.starts_with(&cache.test_source) {
        MarkdownStreamUpdate::Append
    } else {
        MarkdownStreamUpdate::Replace
    };
    markdown_streaming_block_with_osc8(theme, base_style_name, text, cache, update, true)
}

#[cfg(test)]
fn markdown_prefixed_streaming_block(
    theme: &tau_themes::Theme,
    base_style_name: &str,
    prefix_text: &str,
    text: &str,
    cache: &mut MarkdownStreamCache,
) -> tau_cli_term::StyledBlock {
    let update = if text.starts_with(&cache.test_source) {
        MarkdownStreamUpdate::Append
    } else {
        MarkdownStreamUpdate::Replace
    };
    markdown_prefixed_streaming_block_with_osc8(
        theme,
        base_style_name,
        prefix_text,
        text,
        cache,
        update,
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
        RenderRuns {
            stable: &parse_markdown_with_state(text, &mut in_fence, osc8_links),
            live: &[],
            incomplete: "",
        },
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
        RenderRuns {
            stable: &parse_markdown_with_state(text, &mut in_fence, osc8_links),
            live: &[],
            incomplete: "",
        },
        false,
        osc8_links,
    )
}

/// Render live text with stable complete lines cached, table-dependent complete
/// lines formatted provisionally, and only the current incomplete line plain.
pub(crate) fn markdown_streaming_block_with_osc8(
    theme: &tau_themes::Theme,
    base_style_name: &str,
    text: &str,
    cache: &mut MarkdownStreamCache,
    update: MarkdownStreamUpdate,
    osc8_links: bool,
) -> tau_cli_term::StyledBlock {
    markdown_prefixed_streaming_block_with_osc8(
        theme,
        base_style_name,
        "",
        text,
        cache,
        update,
        osc8_links,
    )
}

/// Render live append-only text with a stable base-styled state marker before
/// the incrementally formatted Markdown-lite body.
pub(crate) fn markdown_prefixed_streaming_block_with_osc8(
    theme: &tau_themes::Theme,
    base_style_name: &str,
    prefix_text: &str,
    text: &str,
    cache: &mut MarkdownStreamCache,
    update: MarkdownStreamUpdate,
    osc8_links: bool,
) -> tau_cli_term::StyledBlock {
    if cache.osc8_links.is_some_and(|cached| cached != osc8_links) {
        cache.reset_for_replacement();
    }
    cache.osc8_links = Some(osc8_links);
    if update == MarkdownStreamUpdate::Replace || text.len() < cache.source_len {
        cache.reset_for_replacement();
        cache.osc8_links = Some(osc8_links);
    }
    cache.advance_append(text, osc8_links);

    let prefix = [MarkdownRun {
        text: prefix_text.to_owned(),
        style: MarkdownStyle::Base,
        hyperlink: None,
    }];
    styled_block_from_runs(
        theme,
        base_style_name,
        &prefix,
        RenderRuns {
            stable: &cache.stable_runs,
            live: &cache.live_runs,
            incomplete: &text[cache.complete_until..],
        },
        true,
        osc8_links,
    )
}

fn styled_block_from_runs(
    theme: &tau_themes::Theme,
    base_style_name: &str,
    prefix: &[MarkdownRun],
    runs: RenderRuns<'_>,
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
    push_runs(&mut body_children, runs.stable, styles, !osc8_links);
    push_runs(&mut body_children, runs.live, styles, !osc8_links);
    if !runs.incomplete.is_empty() {
        body_children.push(SpanTree::text(runs.incomplete));
    }

    let needs_space = progress
        && runs.incomplete.chars().next_back().map_or_else(
            || {
                body_children_text_ends_non_whitespace(prefix, runs.live)
                    || runs_end_non_whitespace(runs.stable)
            },
            |character| !character.is_whitespace(),
        );

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
        .chain(runs.stable)
        .chain(runs.live)
        .filter(|run| !run.text.is_empty())
        .map(|run| run.hyperlink.as_deref())
        .chain((!runs.incomplete.is_empty()).then_some(None));
    for (span, target) in rendered.spans_mut().iter_mut().zip(targets) {
        if osc8_links {
            span.hyperlink = target
                .and_then(tau_cli_term::sanitize_hyperlink_target)
                .map(path_std_sync::Arc::from);
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
        if run.text.is_empty() {
            continue;
        }
        let target = run.hyperlink.as_deref();
        let text = visible_run_text(run, show_link_target);
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

/// Returns exactly the text a Markdown run exposes before terminal layout.
///
/// Table width projection and styled span emission share this decision so
/// explicit link targets occupy columns only when OSC 8 is disabled (or the
/// target cannot safely become an OSC 8 hyperlink).
fn visible_run_text(run: &MarkdownRun, show_link_target: bool) -> String {
    let target = run.hyperlink.as_deref();
    let needs_visible_target = target.is_some_and(|target| {
        show_link_target || tau_cli_term::sanitize_hyperlink_target(target).is_none()
    });
    if let Some(target) = target.filter(|_| needs_visible_target)
        && target != run.text
    {
        format!("{} ({target})", run.text)
    } else {
        run.text.clone()
    }
}

/// Returns the first byte whose parsed projection can still change after an
/// append. Every input line is complete.
///
/// A standalone leading-pipe row remains a possible table header. Once followed
/// by a delimiter row, the entire trailing table remains live because each new
/// row can revise every column width. All other complete lines, including fence
/// lines and blank-line seals, are stable.
fn unstable_suffix_start(text: &str, initial_fence: Option<FenceKind>, osc8_links: bool) -> usize {
    let lines = text
        .split_inclusive('\n')
        .map(|line| {
            let body = line.strip_suffix('\n').unwrap_or(line);
            (body, line.len())
        })
        .collect::<Vec<_>>();
    let parser_lines = lines
        .iter()
        .map(|(body, _)| (*body, "\n"))
        .collect::<Vec<_>>();
    let mut fence = initial_fence;
    let mut offset = 0;
    let mut index = 0;
    while index < lines.len() {
        let (body, line_len) = lines[index];
        let trimmed = body.trim_start();
        if let Some(kind) = fence {
            if trimmed.starts_with(kind.marker()) {
                fence = None;
            }
            offset += line_len;
            index += 1;
            continue;
        }
        if let Some(kind) = fence_marker(trimmed) {
            fence = Some(kind);
            offset += line_len;
            index += 1;
            continue;
        }
        if let Some(table_end) = table_block_end(&parser_lines, index)
            && pad_table_lines(&parser_lines[index..table_end], osc8_links).is_some()
        {
            if table_end == lines.len() {
                return offset;
            }
            offset += lines[index..table_end]
                .iter()
                .map(|(_, len)| len)
                .sum::<usize>();
            index = table_end;
            continue;
        }
        if index + 1 == lines.len()
            && !body.trim().is_empty()
            && !is_indented_code(body)
            && TableRow::parse(body).is_some()
        {
            return offset;
        }
        offset += line_len;
        index += 1;
    }
    offset
}

fn parse_markdown_with_state(
    text: &str,
    in_fence: &mut Option<FenceKind>,
    osc8_links: bool,
) -> Vec<MarkdownRun> {
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
            && let Some(table) = pad_table_lines(&lines[index..table_end], osc8_links)
        {
            for (row_index, row) in table.rows.iter().enumerate() {
                parse_table_row(
                    row,
                    &table.widths,
                    &table.alignments,
                    &table.visible_widths[row_index],
                    table_row_kind(row_index),
                    &mut runs,
                );
                let (_, newline) = lines[index + row_index];
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
    if !separator
        .cells
        .iter()
        .all(|cell| TableAlignment::parse(cell).is_some())
    {
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

fn pad_table_lines<'line>(
    lines: &[(&'line str, &str)],
    osc8_links: bool,
) -> Option<TableProjection<'line>> {
    let rows = lines
        .iter()
        .map(|(line, _)| TableRow::parse(line))
        .collect::<Option<Vec<_>>>()?;
    let columns = rows.first()?.cells.len();
    if rows.iter().any(|row| row.cells.len() != columns) {
        return None;
    }
    let alignments = rows[1]
        .cells
        .iter()
        .map(|cell| TableAlignment::parse(cell))
        .collect::<Option<Vec<_>>>()?;
    let mut widths = alignments
        .iter()
        .zip(&rows[1].cells)
        .map(|(alignment, cell)| {
            alignment
                .minimum_width()
                .max(tau_term_screen::display_width(cell.trim()))
        })
        .collect::<Vec<_>>();
    let mut visible_widths = Vec::with_capacity(rows.len());
    for (row_index, row) in rows.iter().enumerate() {
        if row_index == 1 {
            visible_widths.push(vec![0; columns]);
            continue;
        }
        let row_widths = row
            .cells
            .iter()
            .map(|cell| inline_display_width(cell, osc8_links))
            .collect::<Vec<_>>();
        for (index, _) in row.cells.iter().enumerate() {
            widths[index] = widths[index].max(row_widths[index]);
        }
        visible_widths.push(row_widths);
    }

    let mut extra_padding = 0usize;
    for (row_index, row) in rows.iter().enumerate() {
        if TABLE_MAX_LOGICAL_ROW_DISPLAY_WIDTH < row.logical_display_width(&widths)? {
            return None;
        }
        let row_kind = if row_index == 1 {
            TableRowKind::Separator
        } else {
            TableRowKind::Body
        };
        let row_extra =
            table_row_extra_padding(row, &widths, &visible_widths[row_index], row_kind)?;
        extra_padding = extra_padding.checked_add(row_extra)?;
        if TABLE_MAX_EXTRA_PADDING_BYTES < extra_padding {
            return None;
        }
    }
    Some(TableProjection {
        rows,
        widths,
        alignments,
        visible_widths,
    })
}

/// Appends one validated table row while parsing each cell independently.
fn parse_table_row(
    row: &TableRow<'_>,
    widths: &[usize],
    alignments: &[TableAlignment],
    visible_widths: &[usize],
    row_kind: TableRowKind,
    runs: &mut Vec<MarkdownRun>,
) {
    push_run(runs, row.indent, MarkdownStyle::Base);
    push_run(runs, "|", MarkdownStyle::Base);
    for (index, cell) in row.cells.iter().enumerate() {
        if index != 0 {
            push_run(runs, "|", MarkdownStyle::Base);
        }
        push_run(runs, " ", MarkdownStyle::Base);
        match row_kind {
            TableRowKind::Separator => {
                let separator = alignments[index].render_separator_cell(widths[index]);
                push_run(runs, &separator, MarkdownStyle::Base);
            }
            TableRowKind::Body => {
                let spare = widths[index].saturating_sub(visible_widths[index]);
                let (left, right) = alignments[index].padding(spare);
                push_table_spaces(runs, left);
                parse_inline(cell, runs);
                push_table_spaces(runs, right);
            }
        }
        push_run(runs, " ", MarkdownStyle::Base);
    }
    push_run(runs, "|", MarkdownStyle::Base);
}

/// Appends a bounded run of table-alignment spaces.
fn push_table_spaces(runs: &mut Vec<MarkdownRun>, count: usize) {
    if count != 0 {
        push_run(runs, &" ".repeat(count), MarkdownStyle::Base);
    }
}

/// Identifies the delimiter row in a table projection.
fn table_row_kind(row_index: usize) -> TableRowKind {
    if row_index == 1 {
        TableRowKind::Separator
    } else {
        TableRowKind::Body
    }
}

/// Measures a table cell after the inline renderer's visible-link projection.
fn inline_display_width(cell: &str, osc8_links: bool) -> usize {
    let mut runs = Vec::new();
    parse_inline(cell, &mut runs);
    let visible = runs
        .iter()
        .map(|run| visible_run_text(run, !osc8_links))
        .collect::<String>();
    tau_term_screen::display_width(&visible)
}

/// Counts canonical cell margins, alignment spaces, and delimiter dashes
/// without comparing rendered bytes to source bytes, because visible OSC 8 link
/// text may be shorter than its raw Markdown source.
fn table_row_extra_padding(
    row: &TableRow<'_>,
    widths: &[usize],
    visible_widths: &[usize],
    row_kind: TableRowKind,
) -> Option<usize> {
    row.cells
        .iter()
        .enumerate()
        .try_fold(0usize, |total, (index, cell)| {
            let alignment_padding = match row_kind {
                TableRowKind::Body => widths[index].checked_sub(visible_widths[index])?,
                TableRowKind::Separator => {
                    widths[index].checked_sub(tau_term_screen::display_width(cell.trim()))?
                }
            };
            // The projection canonicalizes one ASCII space on both sides of
            // every cell, so account for them even when the source already had
            // matching whitespace. This keeps row count bounded conservatively.
            total.checked_add(2)?.checked_add(alignment_padding)
        })
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
    let mut work = InlineWork::default();
    parse_inline_with_style(text, runs, MarkdownStyle::Base, true, &mut work);
}

#[cfg(test)]
fn inline_recognition_work(text: &str) -> usize {
    let mut runs = Vec::new();
    let mut work = InlineWork::default();
    parse_inline_with_style(text, &mut runs, MarkdownStyle::Base, true, &mut work);
    work.inspected
}

/// Deterministic count of input positions inspected by inline recognition.
///
/// This is deliberately not elapsed time: tests use it to keep failed
/// recognition linear across machines and build profiles.
#[derive(Default)]
struct InlineWork {
    inspected: usize,
}

impl InlineWork {
    fn inspect(&mut self) {
        self.inspected = self.inspected.saturating_add(1);
    }
}

/// Suffix facts and monotonic search cursors shared by one inline parse.
///
/// The previous parser searched each candidate's remaining suffix
/// independently. This scanner instead inspects each character a fixed number
/// of times. Its fused `u32` index is valid because each parsed line is bounded
/// by the protocol frame limit. Opaque spans use the same precedence as
/// rendering (escape, code, link, autolink), and delimiter cursors jump over
/// them before considering a close. Consequently a failed candidate cannot hide
/// or expose a delimiter differently from the old left-to-right search.
struct InlineScanner<'text> {
    text: &'text str,
    opaque_len: Vec<u32>,
    closing_cursors: [usize; 5],
    bare_url_cursor: usize,
}

impl<'text> InlineScanner<'text> {
    fn new(text: &'text str, work: &mut InlineWork) -> Self {
        let mut opaque_len = vec![0; text.len() + 1];
        let mut next_backtick = None;
        let mut after_next_backtick = None;
        let mut next_bracket = None;
        let mut after_next_bracket = None;
        let mut next_paren = None;
        let mut after_next_paren = None;
        let mut next_greater = None;
        let mut next_whitespace = None;
        let mut next_http_invalid = None;
        for (index, ch) in text.char_indices().rev() {
            work.inspect();
            opaque_len[index] = scanner_opaque_len(
                text,
                index,
                next_backtick,
                next_bracket,
                next_greater,
                next_http_invalid,
                &opaque_len,
            );
            if ch == ']'
                && text.as_bytes().get(index + 1) == Some(&b'(')
                && let Some(close_target) = next_paren
                && index + 2 < close_target
                && next_whitespace.is_none_or(|found| close_target <= found)
            {
                opaque_len[index] =
                    u32::try_from(close_target + 1).expect("Markdown line fits protocol frame");
            }
            update_unescaped_index(index, ch, '`', &mut next_backtick, &mut after_next_backtick);
            update_unescaped_index(index, ch, ']', &mut next_bracket, &mut after_next_bracket);
            update_unescaped_index(index, ch, ')', &mut next_paren, &mut after_next_paren);
            next_greater = (ch == '>').then_some(index).or(next_greater);
            next_whitespace = ch.is_whitespace().then_some(index).or(next_whitespace);
            next_http_invalid = (ch.is_whitespace() || ch.is_control())
                .then_some(index)
                .or(next_http_invalid);
        }
        for (index, ch) in text.char_indices() {
            work.inspect();
            if ch == ']' {
                opaque_len[index] = 0;
            }
        }
        Self {
            text,
            opaque_len,
            closing_cursors: [0; 5],
            bare_url_cursor: 0,
        }
    }

    fn code_len(&self, start: usize, range_end: usize) -> Option<usize> {
        let len = self.opaque_len(start);
        (len != 0 && start + len <= range_end).then_some(len)
    }

    fn link(
        &self,
        start: usize,
        range_end: usize,
        work: &mut InlineWork,
    ) -> Option<(&'text str, &'text str, usize)> {
        let len = self.opaque_len(start);
        if len == 0 || range_end < start + len {
            return None;
        }
        let text = &self.text[start..start + len];
        let close_label = find_unescaped_counted(&text[1..], ']', work)? + start + 1;
        let open_target = close_label + 1;
        let close_target = start + len - 1;
        let label = &self.text[start + 1..close_label];
        let target = &self.text[open_target + 1..close_target];
        Some((label, target, len))
    }

    fn autolink(&self, start: usize, range_end: usize) -> Option<(&'text str, usize)> {
        let len = self.opaque_len(start);
        (len != 0 && start + len <= range_end)
            .then(|| (&self.text[start + 1..start + len - 1], len))
    }

    fn bare_url_len(
        &mut self,
        start: usize,
        range_end: usize,
        work: &mut InlineWork,
    ) -> Option<usize> {
        let rest = &self.text[start..range_end];
        let body = rest
            .strip_prefix("https://")
            .or_else(|| rest.strip_prefix("http://"))?;
        if body.is_empty() || body.starts_with(['/', '?', '#']) {
            return None;
        }

        let mut end = self.bare_url_cursor.max(start).min(range_end);
        while end < range_end {
            work.inspect();
            let ch = self.text[end..]
                .chars()
                .next()
                .expect("non-empty bare URL suffix");
            if ch.is_whitespace() || ch.is_control() || matches!(ch, '<' | '>') {
                self.bare_url_cursor = end;
                if ch.is_control() && !ch.is_whitespace() {
                    return None;
                }
                break;
            }
            end += ch.len_utf8();
        }
        if end == self.text.len() {
            self.bare_url_cursor = end;
        }
        while self.text[start..end].ends_with(['.', ',', ';', ':', '!', '?', ')', ']']) {
            end -= self.text[start..end]
                .chars()
                .next_back()
                .map_or(0, char::len_utf8);
        }
        let len = end - start;
        valid_http_target(&self.text[start..end]).then_some(len)
    }

    fn closing_sequence(
        &mut self,
        start: usize,
        delimiter: &str,
        work: &mut InlineWork,
    ) -> Option<usize> {
        let after_open = start + delimiter.len();
        let cursor = match delimiter {
            "***" => 0,
            "**" => 1,
            "~~" => 2,
            "*" => 3,
            "_" => 4,
            _ => return None,
        };
        let mut candidate = self.closing_cursors[cursor].max(after_open);
        while candidate < self.text.len() {
            work.inspect();
            let opaque_len = self.opaque_len(candidate);
            if opaque_len != 0 {
                candidate += opaque_len;
                continue;
            }
            if after_open < candidate
                && self.text[candidate..].starts_with(delimiter)
                && (delimiter != "_" || delimiter_allowed(self.text, candidate, '_'))
            {
                self.closing_cursors[cursor] = candidate;
                return Some(candidate + delimiter.len());
            }
            candidate += self.text[candidate..]
                .chars()
                .next()
                .expect("non-empty closing suffix")
                .len_utf8();
        }
        self.closing_cursors[cursor] = self.text.len();
        None
    }

    fn opaque_len(&self, start: usize) -> usize {
        self.opaque_len[start] as usize
    }
}

fn parse_inline_with_style(
    text: &str,
    runs: &mut Vec<MarkdownRun>,
    inherited_style: MarkdownStyle,
    recognize_delimiters: bool,
    work: &mut InlineWork,
) {
    let mut scanner = InlineScanner::new(text, work);
    parse_inline_range(
        &mut scanner,
        runs,
        0,
        text.len(),
        inherited_style,
        recognize_delimiters,
        work,
    );
}

fn parse_inline_range(
    scanner: &mut InlineScanner<'_>,
    runs: &mut Vec<MarkdownRun>,
    start: usize,
    end: usize,
    inherited_style: MarkdownStyle,
    recognize_delimiters: bool,
    work: &mut InlineWork,
) {
    let text = scanner.text;
    let mut index = start;
    while index < end {
        work.inspect();
        let rest = &text[index..end];
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
            && let Some(len) = scanner.code_len(index, end)
        {
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
            && let Some((label, target, len)) = scanner.link(index, end, work)
        {
            push_link_run(runs, label, target, inherited_style);
            index += len;
            continue;
        }
        if ch == '<'
            && let Some((target, len)) = scanner.autolink(index, end)
        {
            push_link_run(runs, target, target, inherited_style);
            index += len;
            continue;
        }
        if is_bare_url_start(text, start, index, rest)
            && let Some(len) = scanner.bare_url_len(index, end, work)
        {
            let target = &rest[..len];
            push_link_run(runs, target, target, inherited_style);
            index += len;
            continue;
        }
        if recognize_delimiters
            && rest.starts_with("***")
            && let Some(end) = scanner.closing_sequence(index, "***", work)
        {
            push_styled_inline(
                scanner,
                runs,
                index,
                end,
                MarkdownStyle::StrongEmphasis,
                3,
                work,
            );
            index = end;
            continue;
        }
        if recognize_delimiters
            && rest.starts_with("**")
            && let Some(end) = scanner.closing_sequence(index, "**", work)
        {
            push_styled_inline(scanner, runs, index, end, MarkdownStyle::Strong, 2, work);
            index = end;
            continue;
        }
        if recognize_delimiters
            && rest.starts_with("~~")
            && let Some(end) = scanner.closing_sequence(index, "~~", work)
        {
            push_styled_inline(
                scanner,
                runs,
                index,
                end,
                MarkdownStyle::Strikethrough,
                2,
                work,
            );
            index = end;
            continue;
        }
        if recognize_delimiters
            && matches!(ch, '*' | '_')
            && delimiter_allowed(text, index, ch)
            && let Some(end) =
                scanner.closing_sequence(index, if ch == '*' { "*" } else { "_" }, work)
        {
            let style = if ch == '*' {
                MarkdownStyle::Strong
            } else {
                MarkdownStyle::Emphasis
            };
            push_styled_inline(scanner, runs, index, end, style, ch.len_utf8(), work);
            index = end;
            continue;
        }
        let next = index + ch.len_utf8();
        push_run(runs, &text[index..next], inherited_style);
        index = next;
    }
}

/// Parses links inside a uniformly styled delimiter-preserving span.
fn push_styled_inline(
    scanner: &mut InlineScanner<'_>,
    runs: &mut Vec<MarkdownRun>,
    start: usize,
    end: usize,
    style: MarkdownStyle,
    delimiter_len: usize,
    work: &mut InlineWork,
) {
    let content_end = end.saturating_sub(delimiter_len);
    if content_end < start + delimiter_len {
        push_run(runs, &scanner.text[start..end], style);
        return;
    }
    push_run(runs, &scanner.text[start..start + delimiter_len], style);
    parse_inline_range(
        scanner,
        runs,
        start + delimiter_len,
        content_end,
        style,
        false,
        work,
    );
    push_run(runs, &scanner.text[content_end..end], style);
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

fn update_unescaped_index(
    index: usize,
    ch: char,
    needle: char,
    next: &mut Option<usize>,
    after_next: &mut Option<usize>,
) {
    let current = if ch == '\\' {
        *after_next
    } else if ch == needle {
        Some(index)
    } else {
        *next
    };
    *after_next = *next;
    *next = current;
}

fn find_unescaped_counted(text: &str, needle: char, work: &mut InlineWork) -> Option<usize> {
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        work.inspect();
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == needle {
            return Some(index);
        }
    }
    None
}

fn scanner_opaque_len(
    text: &str,
    index: usize,
    next_backtick: Option<usize>,
    next_bracket: Option<usize>,
    next_greater: Option<usize>,
    next_http_invalid: Option<usize>,
    link_end_by_label: &[u32],
) -> u32 {
    let rest = &text[index..];
    if let Some(len) = escaped_len(rest) {
        return u32::try_from(len).expect("escape length fits u32");
    }
    if rest.starts_with('`')
        && let Some(close) = next_backtick
    {
        return u32::try_from(close - index + 1).expect("Markdown line fits protocol frame");
    }
    if rest.starts_with('[')
        && let Some(close_label) = next_bracket
    {
        let link_end = link_end_by_label[close_label] as usize;
        if index + 1 < close_label && link_end != 0 {
            return u32::try_from(link_end - index).expect("Markdown line fits protocol frame");
        }
    }
    if rest.starts_with('<')
        && let Some(close) = next_greater
        && scanner_valid_http_target(text, index + 1, close, next_http_invalid)
    {
        return u32::try_from(close - index + 1).expect("Markdown line fits protocol frame");
    }
    0
}

fn scanner_valid_http_target(
    text: &str,
    start: usize,
    end: usize,
    next_http_invalid: Option<usize>,
) -> bool {
    let target = &text[start..end];
    let Some(body) = target
        .strip_prefix("https://")
        .or_else(|| target.strip_prefix("http://"))
    else {
        return false;
    };
    !body.is_empty()
        && !body.starts_with(['/', '?', '#'])
        && next_http_invalid.is_none_or(|found| end <= found)
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

fn is_bare_url_start(text: &str, range_start: usize, index: usize, rest: &str) -> bool {
    (rest.starts_with("https://") || rest.starts_with("http://"))
        && (index == range_start
            || text[..index]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_whitespace() || matches!(ch, '(' | '[' | '{')))
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

#[cfg(test)]
mod tests;
