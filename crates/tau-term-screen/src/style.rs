//! Styled text types for terminal rendering.
//!
//! Content is represented as sequences of [`Span`]s, each pairing a
//! plain-text string with a [`Style`]. Display width is always
//! computable from the text alone — no ANSI escape codes are stored
//! in the data model.

use std::sync::Arc;

pub use crossterm::style::Color;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Maximum OSC 8 target length accepted by Tau's terminal renderer.
pub const MAX_HYPERLINK_TARGET_BYTES: usize = 4096;

/// Display width of a string in terminal columns, measured by grapheme cluster.
///
/// The measurement uses the same control-character policy as cell conversion:
/// line breaks have no inline width, tabs render as one space, and other
/// control graphemes render as a visible replacement cell.
pub fn display_width(text: &str) -> usize {
    UnicodeSegmentation::graphemes(text, true)
        .map(screen_grapheme_width)
        .sum()
}

/// Returns a string that fits within `max_width` terminal columns, appending an
/// ellipsis when truncation is needed.
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if display_width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 1 {
        return "…".to_owned();
    }

    let mut out = String::new();
    let mut width = 0;
    let prefix_width = max_width - 1;
    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        let grapheme_width = screen_grapheme_width(grapheme);
        if prefix_width < width + grapheme_width {
            break;
        }
        width += grapheme_width;
        out.push_str(grapheme);
    }
    out.push('…');
    out
}

/// Returns the previous grapheme-cluster boundary before `pos`.
pub fn previous_grapheme_boundary(text: &str, pos: usize) -> usize {
    let pos = pos.min(text.len());
    UnicodeSegmentation::grapheme_indices(text, true)
        .map(|(idx, _)| idx)
        .take_while(|idx| *idx < pos)
        .last()
        .unwrap_or(0)
}

/// Returns the next grapheme-cluster boundary after `pos`.
pub fn next_grapheme_boundary(text: &str, pos: usize) -> usize {
    if text.len() <= pos {
        return text.len();
    }
    for (idx, grapheme) in UnicodeSegmentation::grapheme_indices(text, true) {
        let end = idx + grapheme.len();
        if pos < end {
            return end;
        }
    }
    text.len()
}

pub(crate) fn is_line_break_grapheme(grapheme: &str) -> bool {
    matches!(grapheme, "\n" | "\r\n" | "\r")
}

pub(crate) fn screen_grapheme_width(grapheme: &str) -> usize {
    if is_line_break_grapheme(grapheme) {
        0
    } else if grapheme == "\t" || grapheme.chars().any(char::is_control) {
        1
    } else {
        UnicodeWidthStr::width(grapheme)
    }
}

pub(crate) fn push_grapheme_cells(
    cells: &mut Vec<Cell>,
    grapheme: &str,
    style: Style,
    hyperlink: Option<&Arc<str>>,
) {
    if grapheme == "\t" {
        cells.push(Cell::new(' ', style).with_hyperlink(hyperlink.cloned()));
        return;
    }
    if grapheme.chars().any(char::is_control) {
        cells.push(Cell::new('�', style).with_hyperlink(hyperlink.cloned()));
        return;
    }
    let grapheme_width = screen_grapheme_width(grapheme);
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: map-collect-loop-with-let
    for (idx, ch) in grapheme.chars().enumerate() {
        let width = if idx == 0 { grapheme_width } else { 0 };
        cells.push(
            Cell::new(ch, style)
                .with_width(width)
                .with_hyperlink(hyperlink.cloned()),
        );
    }
}

pub(crate) fn visit_styled_graphemes(
    spans: &[Span],
    mut f: impl FnMut(&str, Style, Option<&Arc<str>>),
) {
    let mut text = String::new();
    let mut char_styles = Vec::new();
    for span in spans {
        for ch in span.text.chars() {
            char_styles.push((text.len(), span.style, span.hyperlink.as_ref()));
            text.push(ch);
        }
    }

    let mut style_idx = 0;
    for (byte, grapheme) in UnicodeSegmentation::grapheme_indices(text.as_str(), true) {
        while style_idx + 1 < char_styles.len() && char_styles[style_idx + 1].0 <= byte {
            style_idx += 1;
        }
        let style = char_styles
            .get(style_idx)
            .map(|(_, style, _)| *style)
            .unwrap_or_default();
        let hyperlink = char_styles.get(style_idx).and_then(|(_, _, link)| *link);
        f(grapheme, style, hyperlink);
    }
}

/// Visual attributes for a single character cell.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Style {
    /// Optional foreground color applied to rendered cells.
    pub fg: Option<Color>,
    /// Optional background color applied to rendered cells.
    pub bg: Option<Color>,
    /// Whether cells should be emitted with bold text.
    pub bold: bool,
    /// Whether cells should be emitted with underline.
    pub underline: bool,
    /// Whether cells should be emitted with italic text.
    pub italic: bool,
    /// Whether cells should be emitted with strikethrough text.
    pub strikethrough: bool,
}

impl Style {
    /// Returns this style with a foreground color set.
    pub fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    /// Returns this style with a background color set.
    pub fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    /// Returns this style with bold text enabled.
    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    /// Returns this style with underline enabled.
    pub fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    /// Returns this style with italic text enabled.
    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    /// Returns this style with strikethrough text enabled.
    pub fn strikethrough(mut self) -> Self {
        self.strikethrough = true;
        self
    }
}

/// A terminal cell: one character, its visual style, and display width.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Cell {
    /// Character emitted for this cell.
    pub ch: char,
    /// Visual style applied while emitting this cell.
    pub style: Style,
    /// Display width in terminal columns.
    pub width: usize,
    /// Sanitized OSC 8 target active for this cell.
    pub hyperlink: Option<Arc<str>>,
}

impl Cell {
    pub(crate) fn sanitized_char(ch: char) -> char {
        if ch == '\t' {
            ' '
        } else if ch.is_control() {
            '�'
        } else {
            ch
        }
    }

    /// Creates a styled terminal cell, sanitizing control characters.
    pub fn new(ch: char, style: Style) -> Self {
        let ch = Self::sanitized_char(ch);
        Self {
            ch,
            style,
            width: ch.width().unwrap_or(0),
            hyperlink: None,
        }
    }

    /// Creates an unstyled terminal cell, sanitizing control characters.
    pub fn plain(ch: char) -> Self {
        let ch = Self::sanitized_char(ch);
        Self {
            ch,
            style: Style::default(),
            width: ch.width().unwrap_or(0),
            hyperlink: None,
        }
    }

    pub(crate) fn normalized(&self) -> Self {
        let ch = Self::sanitized_char(self.ch);
        if ch == self.ch {
            self.clone()
        } else {
            Self {
                ch,
                style: self.style,
                width: ch.width().unwrap_or(0),
                hyperlink: self.hyperlink.clone(),
            }
        }
    }

    /// Returns a copy of this cell with an explicit display width.
    pub fn with_width(mut self, width: usize) -> Self {
        self.width = width;
        self
    }

    /// Associates this cell with an OSC 8 hyperlink target.
    pub fn with_hyperlink(mut self, hyperlink: Option<Arc<str>>) -> Self {
        self.hyperlink = hyperlink.filter(|target| sanitize_hyperlink_target(target).is_some());
        self
    }

    /// Display width in terminal columns (1 for ASCII, 2 for wide
    /// chars like emoji/CJK, 0 for zero-width combiners).
    pub fn col_width(&self) -> usize {
        self.width
    }
}

/// A run of text with a uniform style.
///
/// Spans are concatenated before grapheme segmentation so clusters may cross a
/// span boundary. Keep style boundaries on grapheme-cluster boundaries when
/// predictable per-cluster styling matters.
#[derive(Clone, Debug)]
pub struct Span {
    /// Plain text belonging to this span.
    pub text: String,
    /// Style associated with text in this span.
    ///
    /// If a rendered grapheme cluster crosses span boundaries, [`StyledText`]
    /// uses the style at the cluster's first scalar value.
    pub style: Style,
    /// Sanitized OSC 8 target for this span, when present.
    pub hyperlink: Option<Arc<str>>,
}

impl Span {
    /// Creates a span with explicit text and style.
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            hyperlink: None,
        }
    }

    /// Creates an unstyled span.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
            hyperlink: None,
        }
    }

    /// Returns this span with an OSC 8 target when the target is safe.
    pub fn hyperlink(mut self, target: impl AsRef<str>) -> Self {
        self.hyperlink = sanitize_hyperlink_target(target.as_ref()).map(Arc::from);
        self
    }
}

/// A sequence of styled spans representing rich text.
///
/// Can be constructed from plain `&str` / `String` (unstyled),
/// a single [`Span`], or a `Vec<Span>`.
///
/// Layout concatenates all spans before grapheme segmentation. Splitting a
/// grapheme cluster across spans is supported for width and wrapping, but the
/// cluster uses the style active at its first scalar value.
#[derive(Clone, Debug, Default)]
pub struct StyledText {
    spans: Vec<Span>,
}

impl StyledText {
    /// Creates an empty styled text sequence.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a styled span to this text sequence.
    pub fn push(&mut self, span: Span) {
        self.spans.push(span);
    }

    /// Returns the spans that make up this text sequence.
    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// Returns mutable access to the spans in this text sequence.
    pub fn spans_mut(&mut self) -> &mut [Span] {
        &mut self.spans
    }

    /// Total display width in terminal columns.
    ///
    /// Wide characters and emoji grapheme clusters count as terminal columns,
    /// not Unicode scalar values.
    pub fn char_count(&self) -> usize {
        let mut text = String::new();
        for span in &self.spans {
            text.push_str(&span.text);
        }
        display_width(&text)
    }

    /// Returns `true` if there is no text content.
    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|s| s.text.is_empty())
    }

    /// Converts to a flat sequence of [`Cell`]s (newlines excluded).
    pub fn to_cells(&self) -> Vec<Cell> {
        let mut cells = Vec::new();
        visit_styled_graphemes(&self.spans, |grapheme, style, hyperlink| {
            if !is_line_break_grapheme(grapheme) {
                push_grapheme_cells(&mut cells, grapheme, style, hyperlink);
            }
        });
        cells
    }
}

/// Rejects targets that could terminate OSC 8 or inject terminal controls.
pub fn sanitize_hyperlink_target(target: &str) -> Option<&str> {
    (!target.is_empty()
        && target.len() <= MAX_HYPERLINK_TARGET_BYTES
        && !target.chars().any(char::is_control))
    .then_some(target)
}

impl From<&str> for StyledText {
    fn from(s: &str) -> Self {
        Self {
            spans: vec![Span::plain(s)],
        }
    }
}

impl From<String> for StyledText {
    fn from(s: String) -> Self {
        Self {
            spans: vec![Span::plain(s)],
        }
    }
}

impl From<Span> for StyledText {
    fn from(span: Span) -> Self {
        Self { spans: vec![span] }
    }
}

impl From<Vec<Span>> for StyledText {
    fn from(spans: Vec<Span>) -> Self {
        Self { spans }
    }
}

/// Opaque numeric identifier for a [`StyledBlock`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockId(
    /// Stable numeric identity assigned by the higher-level block owner.
    pub u64,
);

/// Horizontal alignment within a block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    /// Place content at the left edge of the content area.
    #[default]
    Left,
    /// Center content within the content area.
    Center,
}

/// A unit of layout: styled content with background, alignment, and margins.
///
/// When rendered, the block's content is wrapped at the available
/// terminal-column width (after subtracting margins), aligned within that
/// space, and the block's background color fills remaining content-area cells.
#[derive(Clone, Debug)]
pub struct StyledBlock {
    /// Primary content rendered in the block's content area.
    pub content: StyledText,
    /// Optional right-aligned adornment for single-row left-aligned blocks.
    ///
    /// `layout_block` renders this only when [`Self::align`] is
    /// [`Align::Left`], primary content lays out to one row, and both sides
    /// fit with separator padding.
    pub right_content: StyledText,
    /// Optional priority-based content rendered as exactly one adaptive line.
    ///
    /// When present, `layout_block` uses this instead of [`Self::content`] and
    /// [`Self::right_content`].
    pub priority_line: Option<crate::PriorityLine>,
    /// Optional ordinary body rendered after [`Self::priority_line`].
    ///
    /// This stays empty unless a one-line adaptive header owns related
    /// multi-line detail content. Layout also hides this body when the
    /// priority line's configured essential band cannot fit.
    pub priority_line_body: StyledText,
    /// Optional background color for the content area and its padding.
    pub bg: Option<Color>,
    /// Horizontal alignment applied to primary content.
    pub align: Align,
    /// Requested transparent left margin width in terminal columns.
    ///
    /// `layout_block` may clamp this so each row has at least one content
    /// column.
    pub margin_left: u16,
    /// Requested transparent right margin width in terminal columns.
    ///
    /// `layout_block` may clamp this so each row has at least one content
    /// column.
    pub margin_right: u16,
}

impl StyledBlock {
    /// Creates a left-aligned block with no margins or background.
    pub fn new(content: impl Into<StyledText>) -> Self {
        Self {
            content: content.into(),
            right_content: StyledText::new(),
            priority_line: None,
            priority_line_body: StyledText::new(),
            bg: None,
            align: Align::Left,
            margin_left: 0,
            margin_right: 0,
        }
    }

    /// Returns `true` when the selected layout's effective primary content is
    /// empty.
    ///
    /// A priority line supersedes ordinary primary content. Right content
    /// remains an adornment and is excluded from this primary-content
    /// predicate.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.priority_line
            .as_ref()
            .map_or_else(|| self.content.is_empty(), crate::PriorityLine::is_empty)
    }

    /// Returns this block with a content-area background color.
    pub fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    /// Returns this block with the requested content alignment.
    pub fn align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }

    /// Returns this block with right-side adornment content.
    ///
    /// The adornment is rendered only for left-aligned, single-row primary
    /// content when both sides fit with separator padding.
    pub fn right_content(mut self, content: impl Into<StyledText>) -> Self {
        self.right_content = content.into();
        self
    }

    /// Returns this block with priority-based single-line content.
    pub fn priority_line(mut self, line: crate::PriorityLine) -> Self {
        self.priority_line = Some(line);
        self
    }

    /// Returns this block with ordinary body content below its priority line.
    ///
    /// The body has no effect unless [`Self::priority_line`] is present, and
    /// it remains hidden whenever that line's essential band fails closed.
    pub fn priority_line_body(mut self, body: impl Into<StyledText>) -> Self {
        self.priority_line_body = body.into();
        self
    }

    /// Returns this block with a requested transparent left margin.
    ///
    /// The margin may be clamped during layout.
    pub fn margin_left(mut self, n: u16) -> Self {
        self.margin_left = n;
        self
    }

    /// Returns this block with a requested transparent right margin.
    ///
    /// The margin may be clamped during layout.
    pub fn margin_right(mut self, n: u16) -> Self {
        self.margin_right = n;
        self
    }

    /// Returns this block with requested transparent left and right margins.
    ///
    /// The margins may be clamped during layout.
    pub fn margins(mut self, left: u16, right: u16) -> Self {
        self.margin_left = left;
        self.margin_right = right;
        self
    }
}

impl From<&str> for StyledBlock {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for StyledBlock {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<StyledText> for StyledBlock {
    fn from(text: StyledText) -> Self {
        Self::new(text)
    }
}
