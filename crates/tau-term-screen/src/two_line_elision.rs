//! Width-adaptive two-row excerpt layout.

use crate::{Cell, StyledText, layout_lines};

/// Width-adaptive two-row content that preserves leading and trailing excerpts.
///
/// Callers must bound the prefix, excerpts, candidate lists, and optional
/// unabridged content. Layout work scales with the supplied values and does not
/// impose its own source-size limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TwoLineElision {
    /// Styled prefix shown before the first excerpt, such as a prompt marker.
    pub prefix: StyledText,
    /// Styled beginning of the first source line.
    pub first: StyledText,
    /// Styled end of the last source line.
    pub last: StyledText,
    /// Styled first-row omission markers, ordered from preferred to most
    /// compact.
    pub first_omissions: Vec<StyledText>,
    /// Styled second-row omission markers, ordered from preferred to most
    /// compact.
    pub last_omissions: Vec<StyledText>,
    /// Styled state labels, ordered from preferred to most compact.
    pub labels: Vec<StyledText>,
    /// Bounded complete presentation retained when it naturally fits two rows.
    pub unabridged: Option<StyledText>,
}

impl TwoLineElision {
    /// Returns whether every possible presentation is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.prefix.is_empty()
            && self.first.is_empty()
            && self.last.is_empty()
            && self.first_omissions.iter().all(StyledText::is_empty)
            && self.last_omissions.iter().all(StyledText::is_empty)
            && self.labels.iter().all(StyledText::is_empty)
            && self.unabridged.as_ref().is_none_or(StyledText::is_empty)
    }

    /// Lays out this excerpt within the current display width.
    pub(crate) fn layout(&self, width: usize) -> Vec<Vec<Cell>> {
        let configured_prefix = self.prefix.to_cells();
        let prefix = if cells_width(&configured_prefix) < width {
            configured_prefix
        } else {
            Vec::new()
        };
        if let Some(unabridged) = &self.unabridged {
            let mut complete = self.prefix.clone();
            for span in unabridged.spans() {
                complete.push(span.clone());
            }
            let lines = layout_lines().content(&complete).width(width).call();
            if lines.len() <= 2 {
                return lines;
            }
        }

        let first_marker = select(
            &self.first_omissions,
            width.saturating_sub(cells_width(&prefix)),
        );
        let label = select(&self.labels, width);
        let last_marker = select(
            &self.last_omissions,
            width.saturating_sub(cells_width(&label)),
        );

        let mut first_row = prefix;
        let first_budget = width
            .saturating_sub(cells_width(&first_row))
            .saturating_sub(cells_width(&first_marker));
        first_row.extend(cell_prefix(&self.first.to_cells(), first_budget));
        first_row.extend(first_marker);

        let last_budget = width
            .saturating_sub(cells_width(&last_marker))
            .saturating_sub(cells_width(&label));
        let mut last_row = last_marker;
        last_row.extend(cell_suffix(&self.last.to_cells(), last_budget));
        last_row.extend(label);
        vec![first_row, last_row]
    }
}

fn select(options: &[StyledText], width: usize) -> Vec<Cell> {
    for option in options {
        let cells = option.to_cells();
        if cells_width(&cells) <= width {
            return cells;
        }
    }
    Vec::new()
}

fn cell_prefix(cells: &[Cell], width: usize) -> Vec<Cell> {
    let mut used = 0;
    cells
        .iter()
        .take_while(|cell| {
            let next = used + cell.col_width();
            let fits = next <= width;
            if fits {
                used = next;
            }
            fits
        })
        .cloned()
        .collect()
}

fn cell_suffix(cells: &[Cell], width: usize) -> Vec<Cell> {
    let mut used = 0;
    let mut start = cells.len();
    for (index, cell) in cells.iter().enumerate().rev() {
        let next = used + cell.col_width();
        if width < next {
            start = index + 1;
            while start < cells.len() && cells[start].col_width() == 0 {
                start += 1;
            }
            break;
        }
        used = next;
        start = index;
    }
    cells[start..].to_vec()
}

fn cells_width(cells: &[Cell]) -> usize {
    cells.iter().map(Cell::col_width).sum()
}
