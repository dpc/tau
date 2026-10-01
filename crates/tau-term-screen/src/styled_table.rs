//! Width-independent styled table cells, projected only at line-layout time.

use crate::style::{screen_grapheme_width, visit_styled_graphemes};
use crate::{Span, Style, StyledText, display_width};

/// Delimiter markers and horizontal placement of one table column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableColumnAlignment {
    /// Left alignment without a leading colon.
    Left,
    /// Left alignment with a leading colon.
    LeftMarked,
    /// Right alignment with a trailing colon.
    Right,
    /// Center alignment with both colons.
    Center,
}

impl TableColumnAlignment {
    fn markers(self) -> (&'static str, &'static str) {
        match self {
            Self::Left => ("", ""),
            Self::LeftMarked => (":", ""),
            Self::Right => ("", ":"),
            Self::Center => (":", ":"),
        }
    }

    fn minimum(self) -> usize {
        let (left, right) = self.markers();
        3 + left.len() + right.len()
    }

    fn padding(self, spare: usize) -> (usize, usize) {
        match self {
            Self::Left | Self::LeftMarked => (0, spare),
            Self::Right => (spare, 0),
            Self::Center => (spare / 2, spare - spare / 2),
        }
    }
}

/// A display-only grid whose styled cells survive pane changes and snapshots.
///
/// Callers bound source size and supply an output budget. Projection preserves
/// visible cell text, replacing wrapping whitespace with a line break where
/// possible and otherwise breaking at grapheme
/// boundaries; it never truncates. Impossible widths or excessive generated
/// output select the caller's ordinary styled fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StyledTable {
    /// Header followed by body rows; the delimiter is generated after the
    /// header.
    pub rows: Vec<Vec<StyledText>>,
    /// Source indentation for the header, delimiter, and each body row.
    pub indents: Vec<String>,
    /// Placement and delimiter markers in column order.
    pub alignments: Vec<TableColumnAlignment>,
    /// Intrinsic delimiter widths, without cell margins.
    pub delimiter_widths: Vec<usize>,
    /// Unpadded source projection used when a grid cannot be rendered.
    pub fallback: StyledText,
    /// Base style for pipes, margins, and generated delimiter markers.
    pub style: Style,
    /// Whether the last source row ended in a newline.
    pub trailing_newline: bool,
    /// Caller-owned ceiling on generated UTF-8 bytes, including
    /// borders/padding.
    pub max_output_bytes: usize,
}

impl StyledTable {
    /// Projects this grid to actual content columns, reserving a preceding
    /// marker.
    ///
    /// Continuation rows receive marker-width spaces so their pipes align with
    /// the first row. Returns `None` instead of clipping or allocating
    /// excessive display padding. The caller then uses `fallback`.
    pub fn project(&self, width: usize, preceding_width: usize) -> Option<StyledText> {
        let columns = self.alignments.len();
        if columns < 2
            || self.rows.is_empty()
            || self.indents.len() != self.rows.len() + 1
            || self.delimiter_widths.len() != columns
            || self.rows.iter().any(|row| row.len() != columns)
        {
            return None;
        }
        let indent = self.indents.iter().map(|s| display_width(s)).max()?;
        let overhead = columns.checked_mul(3)?.checked_add(1)?;
        let reserved = preceding_width.checked_add(indent)?.checked_add(overhead)?;
        let budget = width.checked_sub(reserved)?;
        let minima = self
            .alignments
            .iter()
            .map(|a| a.minimum())
            .collect::<Vec<_>>();
        if budget
            < minima
                .iter()
                .try_fold(0usize, |total, width| total.checked_add(*width))?
        {
            return None;
        }
        let mut widths = self.delimiter_widths.clone();
        for (index, minimum) in minima.iter().enumerate() {
            widths[index] = widths[index].max(*minimum);
        }
        for row in &self.rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(cell.char_count());
            }
        }
        shrink_widths(&mut widths, &minima, budget)?;
        let physical_width = widths
            .iter()
            .try_fold(reserved, |total, width| total.checked_add(*width))?;
        if self.max_output_bytes < physical_width {
            return None;
        }

        let mut output = StyledText::new();
        let mut bytes = 0usize;
        let mut first = true;
        for logical in 0..=self.rows.len() {
            let separator = logical == 1;
            let row_index = if logical == 0 { 0 } else { logical - 1 };
            let cells = if separator {
                widths
                    .iter()
                    .zip(&self.alignments)
                    .map(|(width, alignment)| {
                        let (left, right) = alignment.markers();
                        vec![StyledText::from(Span::new(
                            format!(
                                "{left}{}{right}",
                                "-".repeat(width - left.len() - right.len())
                            ),
                            self.style,
                        ))]
                    })
                    .collect::<Vec<_>>()
            } else {
                self.rows[row_index]
                    .iter()
                    .zip(&widths)
                    .map(|(cell, width)| wrap_cell(cell, *width))
                    .collect::<Option<Vec<_>>>()?
            };
            let height = cells.iter().map(Vec::len).max()?;
            for physical in 0..height {
                if !first {
                    append_text(
                        &mut output,
                        "\n",
                        self.style,
                        &mut bytes,
                        self.max_output_bytes,
                    )?;
                    append_text(
                        &mut output,
                        &" ".repeat(preceding_width),
                        self.style,
                        &mut bytes,
                        self.max_output_bytes,
                    )?;
                }
                first = false;
                append_text(
                    &mut output,
                    &self.indents[logical],
                    self.style,
                    &mut bytes,
                    self.max_output_bytes,
                )?;
                append_text(
                    &mut output,
                    "|",
                    self.style,
                    &mut bytes,
                    self.max_output_bytes,
                )?;
                for index in 0..columns {
                    let fragment = cells[index].get(physical);
                    let visible = fragment.map_or(0, StyledText::char_count);
                    let (left, right) = self.alignments[index].padding(widths[index] - visible);
                    append_text(
                        &mut output,
                        &" ".repeat(left + 1),
                        self.style,
                        &mut bytes,
                        self.max_output_bytes,
                    )?;
                    if let Some(fragment) = fragment {
                        for span in fragment.spans() {
                            bytes = bytes.checked_add(span.text.len())?;
                            if self.max_output_bytes < bytes {
                                return None;
                            }
                            output.push(span.clone());
                        }
                    }
                    append_text(
                        &mut output,
                        &format!("{}|", " ".repeat(right + 1)),
                        self.style,
                        &mut bytes,
                        self.max_output_bytes,
                    )?;
                }
            }
        }
        if self.trailing_newline {
            append_text(
                &mut output,
                "\n",
                self.style,
                &mut bytes,
                self.max_output_bytes,
            )?;
        }
        Some(output)
    }
}

/// Shrinks widest columns to a common ceiling; ties shrink earlier columns
/// first.
///
/// Binary search is equivalent to repeated widest-first shrinking, but keeps
/// allocator work independent of intrinsic prose/delimiter lengths.
fn shrink_widths(widths: &mut [usize], minima: &[usize], budget: usize) -> Option<()> {
    let total = widths
        .iter()
        .try_fold(0usize, |total, width| total.checked_add(*width))?;
    if total <= budget {
        return Some(());
    }
    let originals = widths.to_vec();
    let mut low = 0;
    let mut high = *widths.iter().max()?;
    while low < high {
        let distance = high - low;
        let middle = low + distance / 2 + distance % 2;
        let capped = widths
            .iter()
            .zip(minima)
            .try_fold(0usize, |total, (width, minimum)| {
                total.checked_add((*width).min(middle).max(*minimum))
            })?;
        if capped <= budget {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    for (width, minimum) in widths.iter_mut().zip(minima) {
        *width = (*width).min(low).max(*minimum);
    }
    let mut spare = budget.checked_sub(widths.iter().sum::<usize>())?;
    for (width, original) in widths.iter_mut().zip(originals).rev() {
        if 0 < spare && *width == low && *width < original {
            *width += 1;
            spare -= 1;
        }
    }
    debug_assert_eq!(spare, 0);
    Some(())
}

/// Breaks cells at words or graphemes, preserving styles and link targets.
///
/// Whitespace selected as a wrap boundary becomes the physical line break;
/// interior whitespace and all non-whitespace graphemes remain intact.
fn wrap_cell(cell: &StyledText, width: usize) -> Option<Vec<StyledText>> {
    let mut graphemes = Vec::new();
    let mut has_newline = false;
    visit_styled_graphemes(cell.spans(), |text, style, hyperlink| {
        has_newline |= crate::style::is_line_break_grapheme(text);
        let mut span = Span::new(text, style);
        span.hyperlink = hyperlink.cloned();
        graphemes.push((
            span,
            screen_grapheme_width(text),
            text.chars().all(char::is_whitespace),
        ));
    });
    if has_newline {
        return None;
    }
    let mut lines = Vec::new();
    let mut start = 0;
    while start < graphemes.len() {
        let mut end = start;
        let mut columns = 0;
        let mut word_break = None;
        while end < graphemes.len() && columns + graphemes[end].1 <= width {
            columns += graphemes[end].1;
            if graphemes[end].2 {
                word_break = Some(end + 1);
            }
            end += 1;
        }
        if end == start {
            return None;
        }
        let mut next = end;
        if end < graphemes.len() {
            end = word_break.unwrap_or(end);
            next = end;
            while start < end && graphemes[end - 1].2 {
                end -= 1;
            }
            while next < graphemes.len() && graphemes[next].2 {
                next += 1;
            }
        }
        if start < end {
            lines.push(StyledText::from(
                graphemes[start..end]
                    .iter()
                    .map(|(span, _, _)| span.clone())
                    .collect::<Vec<_>>(),
            ));
        }
        start = next;
    }
    if lines.is_empty() {
        lines.push(StyledText::new());
    }
    Some(lines)
}

/// Adds generated text only while the caller's output ceiling permits it.
fn append_text(
    output: &mut StyledText,
    text: &str,
    style: Style,
    bytes: &mut usize,
    limit: usize,
) -> Option<()> {
    *bytes = bytes.checked_add(text.len())?;
    if limit < *bytes {
        return None;
    }
    if !text.is_empty() {
        output.push(Span::new(text, style));
    }
    Some(())
}

/// Expands table spans before ordinary layout, preserving other span
/// boundaries.
pub(crate) fn project_tables(content: &StyledText, width: usize) -> StyledText {
    let mut output = StyledText::new();
    let mut current_line = String::new();
    for span in content.spans() {
        let projection = span.table.as_ref().map(|table| {
            table
                .project(width, display_width(&current_line))
                .unwrap_or_else(|| table.fallback.clone())
        });
        let spans = projection
            .as_ref()
            .map_or_else(|| std::slice::from_ref(span), StyledText::spans);
        for span in spans {
            if let Some((_, tail)) = span.text.rsplit_once('\n') {
                current_line.clear();
                current_line.push_str(tail);
            } else {
                current_line.push_str(&span.text);
            }
            output.push(span.clone());
        }
    }
    output
}

#[cfg(test)]
mod tests;
